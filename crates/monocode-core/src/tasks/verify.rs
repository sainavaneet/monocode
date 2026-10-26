//! From accepted work to a completed task.
//!
//! Once every assignment that is not cancelled is Accepted, the coordinator
//! moves the stage Implementing → Reviewing → Integrating → Verifying on its
//! own authority. The criteria are checked on the task's result: the one
//! worktree when exactly one assignment has a worktree. With several, the
//! result is their integration (task-11), so the task waits for Mono at
//! Integrating; with none, Mono verifies.
//!
//! A verification round runs every check of each required criterion that no
//! live passing verification satisfies yet, in that worktree, between two
//! snapshots. A failing check (or one that changes the workspace) sends the
//! responsible assignment back with a correction and the stage back to
//! Implementing. A criterion without checks, or a check that cannot run,
//! waits for Mono (`mono_review`), who may call `verify_criterion`; it is
//! never satisfied by default. When every required criterion is satisfied
//! the final report is persisted and the task completes. As with review,
//! only collecting and recording use the store.
use super::checks::{self, Batch};
use super::completion::{complete, evidence, refresh_verifications, FinalReport};
use super::coordinator::{Coordinator, Policy};
use super::events::{append, NewEvent};
use super::findings::{task_ref, Finding, Review, ReviewOutcome};
use super::review::{MAX_CORRECTIONS, MONO_REVIEW, REVIEWED};
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
const NO_RESULT: &str = "No assignment produced a worktree whose result can be checked, so Mono verifies the acceptance criteria.";
const SEVERAL: &str = "Several worktrees hold the task's work; the acceptance criteria are checked on their integrated result, which integration produces.";

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
    /// The assignment whose worktree is the task's result.
    assignment: Assignment,
    workspace: Workspace,
    /// Required criteria with checks and no live passing verification.
    pending: Vec<AcceptanceCriterion>,
    /// Required criteria with no checks and no live passing verification.
    unchecked: Vec<String>,
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
}

impl VerifyJob {
    /// Snapshots the result worktree and runs the pending criteria's checks.
    pub fn inspect(&self) -> RoundInspection {
        let mut found = RoundInspection::default();
        if self.pending.is_empty() {
            return found;
        }
        for criterion in &self.pending {
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
        found
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

/// A key that changes whenever the task, its verifications or its decisions
/// change, so a round that waited is tried again only then.
fn round_key(tasks: &TaskStore<'_>, task: &Task) -> Result<String, TaskError> {
    let (verifications, decisions): (i64, i64) = tasks.conn.query_row(
        "SELECT (SELECT COUNT(*) FROM native_task_verifications WHERE task_id=?1),
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
    ) -> Result<Option<(Task, Assignment, Workspace)>, TaskError> {
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
            if results.len() != 1 {
                let reason = if results.is_empty() {
                    NO_RESULT
                } else {
                    SEVERAL
                };
                let waiting = [Finding::NotChecked {
                    reason: reason.into(),
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
        if task.stage != TaskStage::Verifying || results.len() != 1 {
            return Ok(None);
        }
        let (assignment, workspace) = results.into_iter().next().expect("one result");
        Ok(Some((task, assignment, workspace)))
    }

    /// Advances tasks whose work is accepted and returns the verification
    /// rounds to run. A round that already waited is not repeated until the
    /// task, its verifications or its decisions change.
    pub fn verify_jobs(&self, store: &Store, now: u64) -> Result<Vec<VerifyJob>, TaskError> {
        let tasks = store.tasks()?;
        let mut jobs = Vec::new();
        for id in in_progress(&tasks)? {
            match self.lease(&tasks, &id, now) {
                Err(TaskError::LeaseHeld { .. }) => continue,
                other => other?,
            }
            let Some((task, assignment, workspace)) = self.advance(&tasks, &id, now)? else {
                continue;
            };
            refresh_verifications(store, &id, now)?;
            let round = round_key(&tasks, &task)?;
            if event_exists(&tasks, &wait_event(&id, &round))? {
                continue;
            }
            let live: Vec<Verification> = tasks
                .verifications(&id)?
                .into_iter()
                .filter(|v| !v.invalidated)
                .collect();
            let mut pending = Vec::new();
            let mut unchecked = Vec::new();
            for criterion in task.criteria.iter().filter(|c| c.required) {
                let latest = live
                    .iter()
                    .rev()
                    .find(|v| v.criterion_id.as_deref() == Some(criterion.id.as_str()));
                if latest.is_some_and(|v| v.passed) {
                    continue;
                }
                if criterion.checks.is_empty() {
                    unchecked.push(criterion.id.clone());
                } else {
                    pending.push(criterion.clone());
                }
            }
            let corrections = tasks
                .reviews(&id, &assignment.id)?
                .iter()
                .filter(|r| r.outcome == ReviewOutcome::Correction)
                .count();
            jobs.push(VerifyJob {
                task,
                assignment,
                workspace,
                pending,
                unchecked,
                round,
                corrections,
            });
        }
        Ok(jobs)
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
        if !unchanged {
            return Ok(None);
        }
        let mut findings = round_findings(job, &inspection);
        let corrective = findings.iter().any(Finding::is_corrective);
        if corrective && job.corrections < MAX_CORRECTIONS {
            return send_back(&tasks, job, &inspection, findings, now).map(Some);
        }
        if corrective {
            findings.push(Finding::CorrectionLimit {
                corrections: job.corrections,
            });
        }
        if findings.iter().any(Finding::needs_mono) {
            let tx = tasks.begin()?;
            record_criteria(&tasks, job, &inspection, now)?;
            let id = wait_event(&job.task.id, &job.round);
            hand_to_mono(&tasks, &job.task.id, &id, WAITING, &findings, now)?;
            tx.commit()?;
            return Ok(Some(round(job, RoundOutcome::NeedsMonoReview, findings)));
        }
        let tx = tasks.begin()?;
        record_criteria(&tasks, job, &inspection, now)?;
        tx.commit()?;
        match complete(store, &job.task.id, Requester::Coordinator, now) {
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
        for job in self.verify_jobs(store, now)? {
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

fn round_findings(job: &VerifyJob, inspection: &RoundInspection) -> Vec<Finding> {
    let mut findings = Vec::new();
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
                reason: reason.clone(),
            }),
        }
    }
    if !inspection.batch.modified.is_empty() {
        findings.push(Finding::ModifiedByCheck {
            files: inspection.batch.modified.clone(),
        });
    }
    for criterion in &job.unchecked {
        findings.push(Finding::NotChecked {
            reason: format!("Acceptance criterion {criterion} has no check; Mono verifies it."),
        });
    }
    findings
}

/// One verification per criterion check that ran on the round's snapshot;
/// none when a check changed the workspace.
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
    for (index, (criterion, result)) in inspection
        .criteria
        .iter()
        .zip(&inspection.batch.results)
        .enumerate()
    {
        let Ok(run) = result else {
            continue;
        };
        let id = format!(
            "criterion:{}:{criterion}:{}:{index}",
            job.task.id, job.round
        );
        tasks.record_verification(&Verification {
            id: id.clone(),
            task_id: job.task.id.clone(),
            assignment_id: Some(job.assignment.id.clone()),
            criterion_id: Some(criterion.clone()),
            checkpoint_commit: checkpoint.clone(),
            check: run.check.clone(),
            passed: run.passed(),
            evidence: evidence(&job.workspace, run),
            invalidated: false,
            created_at: now,
        })?;
        ids.push(id);
    }
    Ok(ids)
}

/// The criteria failed: the responsible assignment goes back to Queued with
/// a correction and the stage back to Implementing, in one transaction. The
/// round's results are recorded after the send-back so they stay visible
/// until the workspace changes.
fn send_back(
    tasks: &TaskStore<'_>,
    job: &VerifyJob,
    inspection: &RoundInspection,
    findings: Vec<Finding>,
    now: u64,
) -> Result<Round, TaskError> {
    let tx = tasks.begin()?;
    let assignment = Subject::Assignment {
        task: &job.task.id,
        id: &job.assignment.id,
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
    let attempt = tasks.attempts(&job.task.id, &job.assignment.id)?.pop();
    let review = Review {
        attempt_id: attempt.as_ref().map(|a| a.id.clone()).unwrap_or_default(),
        assignment_id: job.assignment.id.clone(),
        outcome: ReviewOutcome::Correction,
        checkpoint: inspection.checkpoint.clone(),
        changed_files: inspection.batch.modified.clone(),
        findings: findings.clone(),
        verifications,
    };
    let event = NewEvent {
        id: format!("verified:{}@{}", job.task.id, job.round),
        task_id: job.task.id.clone(),
        assignment_id: Some(job.assignment.id.clone()),
        attempt_id: attempt.map(|a| a.id),
        kind: REVIEWED.into(),
        payload: serde_json::to_value(&review)?,
    };
    append(&tx, &event, now)?;
    tx.commit()?;
    Ok(round(job, RoundOutcome::Correction, findings))
}
#[cfg(test)]
#[path = "verify_tests.rs"]
mod tests;
