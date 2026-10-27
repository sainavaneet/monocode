//! From accepted work to a completed task.
//!
//! Once every assignment that is not cancelled is Accepted, the coordinator
//! moves the stage Implementing → Reviewing → Integrating → Verifying on its
//! own authority. The criteria are checked on the task's result: the one
//! worktree when exactly one assignment has a worktree. With several, the
//! result is their integration (`integration`, which moves the stage on to
//! Verifying once they merge cleanly): the integration worktree, where a
//! failing criterion cannot be pinned on one assignment, so Mono decides
//! what goes back. With none, Mono verifies.
//!
//! A verification round runs every check of each required criterion that no
//! live passing verification satisfies yet, in that worktree, between two
//! snapshots. A failing check (or one that changes the workspace) sends the
//! responsible assignment back with a correction and the stage back to
//! Implementing. A criterion without checks, or a check that cannot run,
//! waits for Mono (`mono_review`): Mono may verify a check-less criterion
//! with `verify_criterion`, and waive a declared check that cannot run with
//! a reason (`waive`); neither is satisfied by default. A criterion with checks is satisfied only when
//! every one of its checks passed on one snapshot (`criterion_satisfied`);
//! a check that could not run is recorded as not passing. When every
//! required criterion is satisfied
//! the final report is persisted and the task completes. As with review,
//! only collecting and recording use the store, and neither runs Git:
//! refreshing evidence, snapshots, checks and the report's branch facts all
//! happen in `inspect`.
use super::checks::{self, Batch};
use super::completion::{criterion_satisfied, evidence, invalidate_in, stale_verifications};
use super::coordinator::{Coordinator, Policy};
use super::events::{append, NewEvent};
use super::findings::{task_ref, Finding, Review, ReviewOutcome};
use super::report::{complete_with, FinalReport, ReportFacts};
use super::review::{MAX_CORRECTIONS, MONO_REVIEW, REVIEWED, REVIEW_RETRY_LIMIT};
use super::states::apply;
use super::{
    AcceptanceCriterion, Assignment, AssignmentState, Requester, State, Subject, Task, TaskError,
    TaskLifecycle, TaskStage, TaskStore, Verification, Workspace,
};
use crate::orchestration::WorkspaceMode;
use crate::store::Store;
use rusqlite::OptionalExtension;
use serde_json::json;

const NEEDS_INTEGRATION: &str = "task.needs_integration";
const WAITING: &str = "task.verification_waiting";
const FAILED: &str = "task.verification_failed";
const NO_CRITERIA: &str = "The task has no required acceptance criterion, so nothing mechanical can verify it; Mono decides.";
const UNINTEGRATED: &str = "The task reached Verifying without a clean integration of its several worktrees, so there is no combined result to check. Send it back (stage Implementing) so the coordinator integrates the accepted work, or verify the criteria yourself.";
const NO_RESULT: &str = "No assignment produced a worktree whose result can be checked, so Mono verifies the acceptance criteria.";
const COMBINED: &str = "The acceptance criteria failed on the integrated result of several assignments, so no single assignment is sent back automatically; Mono decides which work goes back.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundOutcome {
    /// The final report is persisted and the task is Completed.
    Completed,
    /// A criterion failed: the assignment is Queued and the stage
    /// Implementing, with a correction for the next attempt.
    Correction,
    /// Waiting for Mono (a `mono_review` delivery).
    NeedsMonoReview,
    /// Something else must change first, such as an open decision.
    Waiting,
}

/// What one verification round decided.
#[derive(Clone, Debug, PartialEq)]
pub struct Round {
    pub task_id: String,
    pub outcome: RoundOutcome,
    pub findings: Vec<Finding>,
    pub report: Option<FinalReport>,
}

/// A task in Verifying with the criteria still to check.
pub struct VerifyJob {
    task: Task,
    /// The assignment whose worktree is the task's result; None for the
    /// integrated result of several.
    assignment: Option<Assignment>,
    workspace: Workspace,
    /// The task's workspaces, whose branches the final report describes.
    workspaces: Vec<Workspace>,
    /// The task's live verifications as recorded; `inspect` checks which
    /// still match their workspace.
    live: Vec<Verification>,
    /// This round already waited (for Mono or a precondition): it runs again
    /// only if `inspect` finds stale evidence.
    waited: bool,
    /// Names this round: the task's revision and what its verifications
    /// and decisions looked like when it started.
    round: String,
    corrections: usize,
}

/// What a round's checks found; no store access.
#[derive(Debug, Default)]
pub struct RoundInspection {
    checkpoint: Option<String>,
    /// The criterion of each check, in batch order.
    criteria: Vec<String>,
    checks: Vec<String>,
    batch: Batch,
    error: Option<String>,
    /// Live verifications whose workspace changed since they ran.
    stale: Vec<String>,
    /// Required criteria with no checks and no live passing verification.
    unchecked: Vec<String>,
    /// A round that already waited found nothing new: nothing to record.
    idle: bool,
    /// The branches' Git facts, for the final report.
    facts: ReportFacts,
}

impl VerifyJob {
    /// Everything that needs Git, without the store: finds stale evidence,
    /// snapshots the result worktree, runs the checks of every required
    /// criterion not yet satisfied, and reads the branches for the report.
    pub fn inspect(&self) -> RoundInspection {
        let stale = stale_verifications(&self.task.id, &self.live);
        if self.waited && stale.is_empty() {
            return RoundInspection {
                idle: true,
                ..RoundInspection::default()
            };
        }
        let live: Vec<Verification> = (self.live.iter())
            .filter(|v| !stale.contains(&v.id))
            .cloned()
            .collect();
        let mut found = RoundInspection {
            stale,
            ..RoundInspection::default()
        };
        let mut pending = Vec::new();
        for criterion in self.task.criteria.iter().filter(|c| c.required) {
            if criterion_satisfied(criterion, &live) {
                continue;
            }
            if criterion.checks.is_empty() {
                found.unchecked.push(criterion.id.clone());
            } else {
                pending.push(criterion);
            }
        }
        if !pending.is_empty() {
            self.run_checks(&pending, &mut found);
        }
        found.facts = ReportFacts::gather(&self.workspaces);
        found
    }

    fn run_checks(&self, pending: &[&AcceptanceCriterion], found: &mut RoundInspection) {
        for criterion in pending {
            for check in &criterion.checks {
                found.criteria.push(criterion.id.clone());
                found.checks.push(check.clone());
            }
        }
        let side = |side: &str| task_ref(&self.task.id, &["rounds", &self.round, side]);
        let path = &self.workspace.path;
        match crate::checkpoint::snapshot_workspace(path, &side("before")) {
            Ok(before) => {
                found.batch = checks::run_batch(path, &found.checks, &before, &side("after"));
                found.error = found.batch.error.clone();
                found.checkpoint = Some(before);
            }
            Err(error) => found.error = Some(error),
        }
    }

    /// Marks this round's stale evidence invalidated, inside the caller's
    /// transaction.
    fn invalidate_stale(
        &self,
        conn: &rusqlite::Connection,
        inspection: &RoundInspection,
        now: u64,
    ) -> Result<(), TaskError> {
        for v in self
            .live
            .iter()
            .filter(|v| inspection.stale.contains(&v.id))
        {
            invalidate_in(conn, v, now)?;
        }
        Ok(())
    }
}

fn wait_event(task: &str, round: &str) -> String {
    format!("verify_wait:{task}@{round}")
}

fn event_exists(tasks: &TaskStore<'_>, id: &str) -> Result<bool, TaskError> {
    Ok(tasks
        .conn
        .query_row("SELECT 1 FROM native_task_events WHERE id=?1", [id], |_| {
            Ok(())
        })
        .optional()?
        .is_some())
}

/// Records why the task waits for Mono and queues it once per `id`.
fn hand_to_mono(
    tasks: &TaskStore<'_>,
    task: &str,
    id: &str,
    kind: &str,
    findings: &[Finding],
    now: u64,
) -> Result<(), TaskError> {
    let event = NewEvent {
        id: id.into(),
        task_id: task.into(),
        assignment_id: None,
        attempt_id: None,
        kind: kind.into(),
        payload: json!({ "findings": findings }),
    };
    append(tasks.conn, &event, now)?;
    let reference = format!("task:{task}");
    tasks.enqueue_delivery(&format!("mono-{id}"), id, MONO_REVIEW, &reference, now)?;
    Ok(())
}

/// Tasks whose work may be ready to verify.
fn in_progress(tasks: &TaskStore<'_>) -> Result<Vec<String>, TaskError> {
    let mut statement = tasks.conn.prepare(
        "SELECT id FROM native_tasks WHERE lifecycle='active'
           AND stage IN ('implementing','reviewing','integrating','verifying') ORDER BY rowid",
    )?;
    let ids = statement.query_map([], |row| row.get(0))?;
    Ok(ids.collect::<Result<_, _>>()?)
}

/// A key that changes whenever the task, its verifications (including one
/// being invalidated by a later workspace change) or its decisions change,
/// so a round that waited is tried again only then.
fn round_key(tasks: &TaskStore<'_>, task: &Task) -> Result<String, TaskError> {
    let (verifications, decisions): (i64, i64) = tasks.conn.query_row(
        "SELECT (SELECT COUNT(*)+COALESCE(SUM(invalidated),0) FROM native_task_verifications WHERE task_id=?1),
                (SELECT COALESCE(SUM(revision),0) FROM native_task_decisions WHERE task_id=?1)",
        [&task.id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(format!("{}-{verifications}-{decisions}", task.revision))
}

impl<P: Policy> Coordinator<P> {
    /// Moves a task whose work is all accepted up to Verifying and returns
    /// it with its result worktree, or None when it is not (yet) there.
    fn advance(
        &self,
        tasks: &TaskStore<'_>,
        id: &str,
        now: u64,
    ) -> Result<Option<(Task, Option<Assignment>, Workspace)>, TaskError> {
        let reload = || {
            tasks
                .task(id)?
                .ok_or_else(|| TaskError::NotFound(format!("task:{id}")))
        };
        let mut task = reload()?;
        let live: Vec<Assignment> = tasks
            .assignments(id)?
            .into_iter()
            .filter(|a| a.status != AssignmentState::Cancelled)
            .collect();
        if live.is_empty() || live.iter().any(|a| a.status != AssignmentState::Accepted) {
            return Ok(None);
        }
        let workspaces = tasks.workspaces(id)?;
        let results: Vec<(Assignment, Workspace)> = live
            .into_iter()
            .filter_map(|a| {
                let worktree = workspaces.iter().find(|w| {
                    w.mode == WorkspaceMode::Worktree
                        && w.assignment_id.as_deref() == Some(a.id.as_str())
                })?;
                Some((a, worktree.clone()))
            })
            .collect();
        let steps = [
            (TaskStage::Implementing, TaskStage::Reviewing),
            (TaskStage::Reviewing, TaskStage::Integrating),
        ];
        for (from, to) in steps {
            if task.stage == from {
                let subject = Subject::Task(id);
                let coordinator = Requester::Coordinator;
                tasks.transition(subject, State::Stage(to), coordinator, task.revision, now)?;
                task = reload()?;
            }
        }
        if task.stage == TaskStage::Integrating {
            // Several worktrees: integration merges them and moves on.
            if results.len() > 1 {
                return Ok(None);
            }
            if results.is_empty() {
                let waiting = [Finding::NotChecked {
                    reason: NO_RESULT.into(),
                }];
                let key = format!("integrate:{id}@{}", task.revision);
                hand_to_mono(tasks, id, &key, NEEDS_INTEGRATION, &waiting, now)?;
                return Ok(None);
            }
            let verifying = State::Stage(TaskStage::Verifying);
            let coordinator = Requester::Coordinator;
            tasks.transition(
                Subject::Task(id),
                verifying,
                coordinator,
                task.revision,
                now,
            )?;
            task = reload()?;
        }
        if task.stage != TaskStage::Verifying || results.is_empty() {
            return Ok(None);
        }
        if results.len() > 1 {
            // Only a clean integration newer than every acceptance of the
            // work it merged is a result to verify; a task moved here
            // without one (by Mono) is refused explicitly, once.
            let integrated: Vec<String> = results.iter().map(|(a, _)| a.id.clone()).collect();
            let clean = tasks.integration_current(id, &integrated)?;
            let integrated = workspaces
                .into_iter()
                .find(|w| w.id == super::integration::workspace_id(id))
                .filter(|_| clean);
            if integrated.is_none() {
                let waiting = [Finding::NotChecked {
                    reason: UNINTEGRATED.into(),
                }];
                let key = format!("unintegrated:{id}@{}", task.revision);
                hand_to_mono(tasks, id, &key, NEEDS_INTEGRATION, &waiting, now)?;
            }
            return Ok(integrated.map(|w| (task, None, w)));
        }
        let (assignment, workspace) = results.into_iter().next().expect("one result");
        Ok(Some((task, Some(assignment), workspace)))
    }

    /// Advances tasks whose work is accepted and returns the verification
    /// rounds to run, with one error per task that could not be prepared
    /// (the others still run). A round that already waited is not repeated
    /// until the task, its verifications or its decisions change; one that
    /// failed to record [`REVIEW_RETRY_LIMIT`] times goes to Mono instead.
    pub fn verify_jobs(
        &self,
        store: &Store,
        now: u64,
    ) -> Result<(Vec<VerifyJob>, Vec<String>), TaskError> {
        let tasks = store.tasks()?;
        let mut jobs = Vec::new();
        let mut errors = Vec::new();
        for id in in_progress(&tasks)? {
            match self.verify_job(&tasks, &id, now) {
                Ok(Some(job)) => jobs.push(job),
                Ok(None) | Err(TaskError::LeaseHeld { .. }) => {}
                Err(error) => errors.push(format!("{id}: {error}")),
            }
        }
        Ok((jobs, errors))
    }

    fn verify_job(
        &self,
        tasks: &TaskStore<'_>,
        id: &str,
        now: u64,
    ) -> Result<Option<VerifyJob>, TaskError> {
        self.lease(tasks, id, now)?;
        let Some((task, assignment, workspace)) = self.advance(tasks, id, now)? else {
            return Ok(None);
        };
        let round = round_key(tasks, &task)?;
        let waited = wait_event(id, &round);
        let already_waited = event_exists(tasks, &waited)?;
        let failures = failures(tasks, id, &round)?;
        if failures.len() >= REVIEW_RETRY_LIMIT as usize {
            let reason = format!(
                "Verification failed to record {} times: {}",
                failures.len(),
                failures.last().map_or("unknown error", String::as_str)
            );
            let findings = [Finding::Workspace { reason }];
            hand_to_mono(tasks, id, &waited, WAITING, &findings, now)?;
            return Ok(None);
        }
        let live: Vec<Verification> = tasks
            .verifications(id)?
            .into_iter()
            .filter(|v| !v.invalidated)
            .collect();
        let corrections = match &assignment {
            Some(assignment) => tasks
                .reviews(id, &assignment.id)?
                .iter()
                .filter(|r| r.outcome == ReviewOutcome::Correction)
                .count(),
            None => 0,
        };
        Ok(Some(VerifyJob {
            workspaces: tasks.workspaces(id)?,
            live,
            waited: already_waited,
            task,
            assignment,
            workspace,
            round,
            corrections,
        }))
    }

    /// Counts a failure to record `job`'s round; after
    /// [`REVIEW_RETRY_LIMIT`] the round goes to Mono instead of re-running.
    pub fn round_failed(
        &self,
        store: &Store,
        job: &VerifyJob,
        error: &str,
        now: u64,
    ) -> Result<(), TaskError> {
        let tasks = store.tasks()?;
        let n = failures(&tasks, &job.task.id, &job.round)?.len();
        let event = NewEvent {
            id: format!("verify_failed:{}@{}#{n}", job.task.id, job.round),
            task_id: job.task.id.clone(),
            assignment_id: None,
            attempt_id: None,
            kind: FAILED.into(),
            payload: json!({ "round": job.round, "error": error }),
        };
        append(tasks.conn, &event, now)?;
        Ok(())
    }

    /// Records a round's outcome: a correction, a wait for Mono, or the
    /// final report and completion. None when the task changed meanwhile or
    /// the lease was lost.
    pub fn record_round(
        &self,
        store: &Store,
        job: &VerifyJob,
        inspection: RoundInspection,
        now: u64,
    ) -> Result<Option<Round>, TaskError> {
        let tasks = store.tasks()?;
        match self.lease(&tasks, &job.task.id, now) {
            Err(TaskError::LeaseHeld { .. }) => return Ok(None),
            other => other?,
        }
        let current = tasks.task(&job.task.id)?;
        let unchanged = current.is_some_and(|t| {
            t.revision == job.task.revision
                && t.stage == TaskStage::Verifying
                && t.lifecycle == TaskLifecycle::Active
        });
        if !unchanged || inspection.idle {
            return Ok(None);
        }
        let mut findings = round_findings(job, &inspection);
        let corrective = findings.iter().any(Finding::is_corrective);
        if let Some(assignment) = job.assignment.as_ref().filter(|_| corrective) {
            if job.corrections < MAX_CORRECTIONS {
                return send_back(&tasks, job, assignment, &inspection, findings, now).map(Some);
            }
        }
        if corrective && job.assignment.is_none() {
            findings.push(Finding::NotChecked {
                reason: COMBINED.into(),
            });
        } else if corrective {
            findings.push(Finding::CorrectionLimit {
                corrections: job.corrections,
            });
        }
        if findings.iter().any(Finding::needs_mono) {
            let tx = tasks.begin()?;
            job.invalidate_stale(&tx, &inspection, now)?;
            record_criteria(&tasks, job, &inspection, now)?;
            // Keyed after recording, so this round's own rows do not make
            // the next pass look like a new round.
            let id = wait_event(&job.task.id, &round_key(&tasks, &job.task)?);
            hand_to_mono(&tasks, &job.task.id, &id, WAITING, &findings, now)?;
            tx.commit()?;
            return Ok(Some(round(job, RoundOutcome::NeedsMonoReview, findings)));
        }
        let tx = tasks.begin()?;
        job.invalidate_stale(&tx, &inspection, now)?;
        record_criteria(&tasks, job, &inspection, now)?;
        tx.commit()?;
        let coordinator = Requester::Coordinator;
        match complete_with(&tasks, &job.task.id, coordinator, &inspection.facts, now) {
            Ok(report) => Ok(Some(Round {
                report: Some(report),
                ..round(job, RoundOutcome::Completed, findings)
            })),
            Err(TaskError::Precondition(precondition)) => {
                let wait = NewEvent {
                    id: wait_event(&job.task.id, &round_key(&tasks, &job.task)?),
                    task_id: job.task.id.clone(),
                    assignment_id: None,
                    attempt_id: None,
                    kind: WAITING.into(),
                    payload: json!({ "precondition": format!("{precondition:?}") }),
                };
                append(tasks.conn, &wait, now)?;
                Ok(Some(round(job, RoundOutcome::Waiting, findings)))
            }
            Err(error) => Err(error),
        }
    }

    /// Advances, inspects and records every verification round in turn,
    /// holding `store` throughout. The app uses the three steps separately.
    pub fn verify(&self, store: &Store, now: u64) -> Result<Vec<Round>, TaskError> {
        let mut rounds = Vec::new();
        let (jobs, errors) = self.verify_jobs(store, now)?;
        if let Some(error) = errors.into_iter().next() {
            return Err(TaskError::Storage(error));
        }
        for job in jobs {
            let inspection = job.inspect();
            rounds.extend(self.record_round(store, &job, inspection, now)?);
        }
        Ok(rounds)
    }
}

fn round(job: &VerifyJob, outcome: RoundOutcome, findings: Vec<Finding>) -> Round {
    Round {
        task_id: job.task.id.clone(),
        outcome,
        findings,
        report: None,
    }
}

/// The recorded failures of `round`, oldest first.
fn failures(tasks: &TaskStore<'_>, task: &str, round: &str) -> Result<Vec<String>, TaskError> {
    let mut statement = tasks.conn.prepare(
        "SELECT json_extract(payload,'$.error') FROM native_task_events
         WHERE task_id=?1 AND kind=?2 AND json_extract(payload,'$.round')=?3 ORDER BY seq",
    )?;
    let rows = statement.query_map([task, FAILED, round], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn round_findings(job: &VerifyJob, inspection: &RoundInspection) -> Vec<Finding> {
    let mut findings = Vec::new();
    if !job.task.criteria.iter().any(|c| c.required) {
        findings.push(Finding::NotChecked {
            reason: NO_CRITERIA.into(),
        });
    }
    if let Some(error) = &inspection.error {
        findings.push(Finding::Workspace {
            reason: error.clone(),
        });
    }
    let results = inspection.criteria.iter().zip(&inspection.checks);
    for ((criterion, check), result) in results.zip(&inspection.batch.results) {
        match result {
            Ok(run) if !run.passed() => findings.push(Finding::CriterionNotMet {
                criterion: criterion.clone(),
                check: check.clone(),
                exit_code: run.exit_code,
                timed_out: run.timed_out,
                output: run.output.clone(),
            }),
            Ok(_) => {}
            Err(reason) => findings.push(Finding::UnrunnableCheck {
                check: check.clone(),
                reason: format!("{reason} {}", waive_hint(&job.task.id, criterion, check)),
            }),
        }
    }
    if !inspection.batch.modified.is_empty() {
        findings.push(Finding::ModifiedByCheck {
            files: inspection.batch.modified.clone(),
        });
    }
    for criterion in &inspection.unchecked {
        findings.push(Finding::NotChecked {
            reason: format!("Acceptance criterion {criterion} has no check; Mono verifies it."),
        });
    }
    findings
}

/// How Mono can settle a declared check that cannot run.
fn waive_hint(task: &str, criterion: &str, check: &str) -> String {
    let input = json!({"taskId": task, "criterionId": criterion, "check": check, "reason": "..."});
    format!("If it cannot be made to run, judge criterion {criterion} yourself and waive this check with tasks.criteria.waive {input} (a reason is required; a check that ran and failed cannot be waived).")
}

/// One verification per criterion check of the round's snapshot, a check
/// that could not run recorded as not passing; none when a check changed the
/// workspace.
fn record_criteria(
    tasks: &TaskStore<'_>,
    job: &VerifyJob,
    inspection: &RoundInspection,
    now: u64,
) -> Result<Vec<String>, TaskError> {
    let Some(checkpoint) = &inspection.checkpoint else {
        return Ok(Vec::new());
    };
    if !inspection.batch.trustworthy() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    let checks = inspection.criteria.iter().zip(&inspection.checks);
    for (index, ((criterion, check), result)) in checks.zip(&inspection.batch.results).enumerate() {
        let id = format!(
            "criterion:{}:{criterion}:{}:{index}",
            job.task.id, job.round
        );
        let (passed, evidence) = match result {
            Ok(run) => (run.passed(), evidence(&job.workspace, run)),
            Err(reason) => (false, unrunnable(&job.workspace, reason)),
        };
        tasks.record_verification(&Verification {
            id: id.clone(),
            task_id: job.task.id.clone(),
            assignment_id: job.assignment.as_ref().map(|a| a.id.clone()),
            criterion_id: Some(criterion.clone()),
            checkpoint_commit: checkpoint.clone(),
            check: check.clone(),
            passed,
            evidence,
            invalidated: false,
            created_at: now,
        })?;
        ids.push(id);
    }
    Ok(ids)
}

/// The evidence of a check that could not run: where, and why.
fn unrunnable(workspace: &Workspace, reason: &str) -> String {
    json!({
        "workspace": workspace.id,
        "path": workspace.path,
        "unrunnable": reason,
    })
    .to_string()
}

/// The criteria failed: the responsible assignment goes back to Queued with
/// a correction and the stage back to Implementing, in one transaction. The
/// round's results are recorded after the send-back so they stay visible
/// until the workspace changes.
fn send_back(
    tasks: &TaskStore<'_>,
    job: &VerifyJob,
    responsible: &Assignment,
    inspection: &RoundInspection,
    findings: Vec<Finding>,
    now: u64,
) -> Result<Round, TaskError> {
    let tx = tasks.begin()?;
    job.invalidate_stale(&tx, inspection, now)?;
    let assignment = Subject::Assignment {
        task: &job.task.id,
        id: &responsible.id,
    };
    let queued = State::Assignment(AssignmentState::Queued);
    let coordinator = Requester::Coordinator;
    apply(
        &tx,
        assignment,
        queued,
        coordinator,
        tasks.revision(assignment)?,
        now,
    )?;
    let task = Subject::Task(&job.task.id);
    let implementing = State::Stage(TaskStage::Implementing);
    apply(
        &tx,
        task,
        implementing,
        coordinator,
        tasks.revision(task)?,
        now,
    )?;
    let verifications = record_criteria(tasks, job, inspection, now)?;
    let attempt = tasks.attempts(&job.task.id, &responsible.id)?.pop();
    let review = Review {
        attempt_id: attempt.as_ref().map(|a| a.id.clone()).unwrap_or_default(),
        assignment_id: responsible.id.clone(),
        outcome: ReviewOutcome::Correction,
        checkpoint: inspection.checkpoint.clone(),
        changed_files: inspection.batch.modified.clone(),
        findings: findings.clone(),
        verifications,
    };
    let event = NewEvent {
        id: format!("verified:{}@{}", job.task.id, job.round),
        task_id: job.task.id.clone(),
        assignment_id: Some(responsible.id.clone()),
        attempt_id: attempt.map(|a| a.id),
        kind: REVIEWED.into(),
        payload: serde_json::to_value(&review)?,
    };
    append(&tx, &event, now)?;
    tx.commit()?;
    Ok(round(job, RoundOutcome::Correction, findings))
}
#[cfg(test)]
#[path = "verify_lock_tests.rs"]
mod lock_tests;
#[cfg(test)]
#[path = "verify_tests.rs"]
mod tests;
