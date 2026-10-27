//! The coordinator's deterministic review of a worker's success claim: the
//! step between "the worker says it is done" and the assignment counting as
//! done.
//!
//! For each pending `review:<attempt>` delivery: check the claim is still the
//! current run of the current requirements, snapshot the actual workspace
//! (`checkpoint`), flag files changed outside the assignment's scope, compare
//! the reported validation with the plan's checks, run those checks in the
//! worktree, snapshot again to catch checks that change it, and then accept,
//! send back with a durable correction, or hand the claim to Mono when it
//! cannot be decided mechanically. Investigations in the user's checkout are
//! never snapshotted or checked here; Mono judges them. Collecting jobs and
//! recording outcomes need the store; inspecting (Git and the checks) does
//! not, so the app can run it without holding the store lock.
use super::autonomy::Authority;
use super::checks::{self, Batch};
use super::completion::evidence;
use super::coordinator::{Coordinator, Policy, REVIEW};
use super::events::{append, PendingDelivery};
use super::findings::{out_of_scope, task_ref};
pub use super::findings::{Finding, Review, ReviewOutcome};
use super::states::{apply, is_latest_attempt};
use super::store::Key;
use super::worker::event;
use super::{
    Assignment, AssignmentState, Attempt, AttemptState, Claim, ClaimOutcome, DeliveryOutcome,
    Requester, State, Subject, Task, TaskError, TaskLifecycle, TaskStore, Verification, Workspace,
};
use crate::checkpoint;
use crate::orchestration::WorkspaceMode;
use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Corrections an assignment may receive before review stops re-queueing it
/// and hands the next claim to Mono.
pub const MAX_CORRECTIONS: usize = 3;
/// Failed inspections or recordings before review hands the claim to Mono.
pub const REVIEW_RETRY_LIMIT: u32 = 3;
/// Where claims and tasks that need judgement wait for Mono.
pub const MONO_REVIEW: &str = "mono_review";
pub(super) const REVIEWED: &str = "assignment.reviewed";
const CHECKOUT: &str = "Investigations work in the user's checkout, so review neither snapshots nor runs checks there; Mono judges the findings.";

/// Everything a review needs, read from the store.
pub struct ReviewJob {
    delivery: PendingDelivery,
    task: Task,
    assignment: Assignment,
    attempt: Attempt,
    claim: Option<Claim>,
    workspace: Option<Workspace>,
    superseded: Option<String>,
    stale: Option<String>,
    /// The last error once inspecting or recording failed too often.
    exhausted: Option<String>,
    corrections: usize,
    /// The task may not commit now (its effective policy says so), so any
    /// commit in the worktree is sent back.
    commits_refused: bool,
    /// The integration commit the previous review asked the worker to
    /// rebase onto; once the worktree descends from it, it is the base.
    rebase_onto: Option<String>,
}

/// What inspecting the workspace found; no store access.
#[derive(Debug, Default)]
pub struct Inspection {
    checkpoint: Option<String>,
    changed_files: Vec<String>,
    findings: Vec<Finding>,
    batch: Batch,
    /// Git could not inspect the workspace; the review is retried.
    error: Option<String>,
    /// The worktree was rebased onto this integration commit, now its base.
    rebased_onto: Option<String>,
}

/// A fingerprint of what the worker was asked to do, recorded with its
/// launch so review can tell when the requirements changed since.
pub(super) fn requirements(a: &Assignment) -> String {
    let fields = serde_json::json!([
        a.title,
        a.prompt,
        a.files,
        a.depends_on,
        a.workspace,
        a.verification
    ]);
    format!("{:x}", Sha256::digest(fields.to_string().as_bytes()))
}

/// Findings for checks that failed or could not run.
pub(super) fn check_findings(checks: &[String], batch: &Batch) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (check, result) in checks.iter().zip(&batch.results) {
        match result {
            Ok(run) if !run.passed() => findings.push(Finding::FailedCheck {
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
    if !batch.modified.is_empty() {
        findings.push(Finding::ModifiedByCheck {
            files: batch.modified.clone(),
        });
    }
    findings
}

impl ReviewJob {
    /// Snapshots the workspace, checks the scope, runs the plan's checks and
    /// snapshots again. Touches no store, so it may run without the store
    /// lock.
    pub fn inspect(&self) -> Inspection {
        let mut found = Inspection::default();
        if self.superseded.is_some() || self.exhausted.is_some() {
            return found;
        }
        if let Some(reason) = &self.stale {
            found.findings.push(Finding::StaleRevision {
                reason: reason.clone(),
            });
        }
        if let Some(claim) = &self.claim {
            for item in &claim.remaining_work {
                found
                    .findings
                    .push(Finding::RemainingWork { item: item.clone() });
            }
            for check in &self.assignment.verification {
                if !claim.validation.iter().any(|v| v.contains(check.trim())) {
                    found.findings.push(Finding::UnreportedCheck {
                        check: check.clone(),
                    });
                }
            }
        }
        let Some(workspace) = &self.workspace else {
            found.findings.push(Finding::Workspace {
                reason: "No workspace is recorded for the assignment.".into(),
            });
            return found;
        };
        if workspace.mode == WorkspaceMode::Checkout {
            found.findings.push(Finding::NotChecked {
                reason: CHECKOUT.into(),
            });
            return found;
        }
        let Some(base) = workspace.base_commit.as_deref() else {
            found.findings.push(Finding::Workspace {
                reason:
                    "No base commit is recorded for the worktree, so its changes cannot be scoped."
                        .into(),
            });
            return found;
        };
        // A worker sent back to rebase onto the integration branch is
        // scoped against what it rebased onto, not its original base.
        let rebased = self.rebase_onto.as_deref().filter(|onto| {
            let path = Path::new(&workspace.path);
            crate::git::git(path, &["merge-base", "--is-ancestor", onto, "HEAD"]).is_ok()
        });
        found.rebased_onto = rebased.map(str::to_owned);
        let base = rebased.unwrap_or(base);
        let refs = ["attempts", &self.assignment.id, &self.attempt.id];
        let before_ref = task_ref(&self.task.id, &[&refs[..], &["before"]].concat());
        let checkpoint = match checkpoint::snapshot_workspace(&workspace.path, &before_ref)
            .and_then(|commit| {
                let changed = checkpoint::changed_paths(&workspace.path, base, &commit)?;
                Ok((commit, changed))
            }) {
            Ok((commit, changed)) => {
                found.changed_files = changed;
                commit
            }
            Err(error) => {
                found.error = Some(error);
                return found;
            }
        };
        if self.commits_refused {
            match crate::git::git(
                Path::new(&workspace.path),
                &["rev-list", "--abbrev-commit", &format!("{base}..HEAD")],
            ) {
                Ok(list) => {
                    let commits: Vec<String> = list.lines().map(str::to_owned).collect();
                    if !commits.is_empty() {
                        found
                            .findings
                            .push(Finding::UnauthorizedCommits { commits });
                    }
                }
                Err(error) => {
                    found.error = Some(error);
                    return found;
                }
            }
        }
        match out_of_scope(&found.changed_files, &self.assignment.files) {
            Ok(paths) => found
                .findings
                .extend(paths.into_iter().map(|path| Finding::OutOfScope { path })),
            Err(reason) => found.findings.push(Finding::NotChecked { reason }),
        }
        if self.assignment.verification.is_empty() {
            found.findings.push(Finding::NoChecks);
        }
        let after_ref = task_ref(&self.task.id, &[&refs[..], &["after"]].concat());
        found.batch = checks::run_batch(
            &workspace.path,
            &self.assignment.verification,
            &checkpoint,
            &after_ref,
            !self.commits_refused,
        );
        found.error = found.batch.error.clone();
        found
            .findings
            .extend(check_findings(&self.assignment.verification, &found.batch));
        found.checkpoint = Some(checkpoint);
        found
    }

    /// The outcome and the findings it rests on.
    fn decide(&self, inspection: &Inspection) -> (ReviewOutcome, Vec<Finding>) {
        let mut findings = inspection.findings.clone();
        if let Some(reason) = &self.superseded {
            findings.push(Finding::Superseded {
                reason: reason.clone(),
            });
            return (ReviewOutcome::Superseded, findings);
        }
        if let Some(reason) = self.exhausted.as_ref().or(inspection.error.as_ref()) {
            findings.push(Finding::Workspace {
                reason: reason.clone(),
            });
        }
        if findings.iter().any(Finding::is_corrective) {
            if self.corrections < MAX_CORRECTIONS {
                return (ReviewOutcome::Correction, findings);
            }
            findings.push(Finding::CorrectionLimit {
                corrections: self.corrections,
            });
            return (ReviewOutcome::NeedsMonoReview, findings);
        }
        let verified = inspection.batch.trustworthy()
            && inspection
                .batch
                .results
                .iter()
                .any(|r| r.as_ref().is_ok_and(|run| run.passed()));
        if findings.iter().any(Finding::needs_mono) || !verified {
            return (ReviewOutcome::NeedsMonoReview, findings);
        }
        (ReviewOutcome::Accepted, findings)
    }

    /// The review request this job answers.
    pub fn delivery_id(&self) -> &str {
        &self.delivery.id
    }
}

fn reviewed_event(attempt: &str) -> String {
    format!("reviewed:{attempt}")
}

/// Why `attempt`'s claim is no longer the one to review, if it is not.
fn superseded(
    tasks: &TaskStore<'_>,
    task: &Task,
    assignment: &Assignment,
    attempt: &Attempt,
    claim: Option<&Claim>,
) -> Result<Option<String>, TaskError> {
    let reason = if matches!(
        task.lifecycle,
        TaskLifecycle::Cancelled | TaskLifecycle::Completed
    ) {
        format!("The task is {}.", task.lifecycle.as_str())
    } else if !is_latest_attempt(tasks.conn, Subject::Attempt(&attempt.id))? {
        "A newer attempt replaced this one.".into()
    } else if assignment.status != AssignmentState::NeedsReview
        || attempt.status != AttemptState::Succeeded
    {
        format!("The assignment is {}.", assignment.status.as_str())
    } else if claim.is_none_or(|c| c.outcome != ClaimOutcome::Succeeded) {
        "The attempt has no success claim.".into()
    } else {
        return Ok(None);
    };
    Ok(Some(reason))
}

/// Whether the requirements changed since the attempt's launch intent.
fn stale(
    tasks: &TaskStore<'_>,
    attempt: &Attempt,
    assignment: &Assignment,
) -> Result<Option<String>, TaskError> {
    let payload: Option<String> = tasks
        .conn
        .query_row(
            "SELECT payload FROM native_task_events WHERE id=?1",
            [format!("launch:{}", attempt.id)],
            |row| row.get(0),
        )
        .optional()?;
    let launched = payload
        .and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok())
        .and_then(|p| p["requirements"].as_str().map(str::to_owned));
    Ok(launched
        .filter(|launched| *launched != requirements(assignment))
        .map(|_| {
            "The assignment's requirements changed after this attempt started; redo the work against the current ones.".into()
        }))
}

pub(super) fn acknowledge(conn: &Connection, id: &str, now: u64) -> Result<(), TaskError> {
    let key = Key::new("native_task_deliveries", "id=?", vec![id], "delivery");
    let revision = key.revision(conn)?;
    key.bump(conn, revision, "state=?", vec![&"acknowledged"], now)?;
    Ok(())
}

impl<P: Policy> Coordinator<P> {
    /// The pending review requests this coordinator holds the task lease
    /// for. A request already reviewed is acknowledged and skipped; a paused
    /// or blocked task's requests wait. A reporting worker's stop that
    /// stalls is listed in `tick.stalled` (once, as in the tick).
    pub fn review_jobs(
        &self,
        store: &crate::store::Store,
        now: u64,
        tick: &mut super::coordinator::Tick,
    ) -> Result<Vec<ReviewJob>, TaskError> {
        let tasks = store.tasks()?;
        let mut jobs = Vec::new();
        for delivery in tasks.pending_deliveries(REVIEW)? {
            let attempt = match delivery.payload_ref.strip_prefix("attempt:") {
                Some(id) => tasks.attempt(id)?,
                None => None,
            };
            let Some(attempt) = attempt else {
                continue;
            };
            match self.lease(&tasks, &attempt.task_id, now) {
                Err(TaskError::LeaseHeld { .. }) => continue,
                other => other?,
            }
            if tasks.review(&attempt.id)?.is_some() {
                acknowledge(tasks.conn, &delivery.id, now)?;
                continue;
            }
            let task = tasks.task(&attempt.task_id)?;
            let assignment = tasks.assignment(&attempt.task_id, &attempt.assignment_id)?;
            let (Some(task), Some(assignment)) = (task, assignment) else {
                continue;
            };
            if matches!(
                task.lifecycle,
                TaskLifecycle::Paused | TaskLifecycle::Blocked
            ) {
                continue;
            }
            // The worker that reported is stopped first, so nothing it
            // still writes lands after the reviewed snapshot. A stop that
            // stalls is reported to Mono once.
            if self.held_by_stop(&tasks, &task.id, &assignment.id, now, tick)? {
                continue;
            }
            let claim = tasks.claim(&attempt.id)?;
            let workspace = tasks
                .workspaces(&task.id)?
                .into_iter()
                .find(|w| w.assignment_id.as_deref() == Some(assignment.id.as_str()));
            let corrections = tasks
                .reviews(&task.id, &assignment.id)?
                .iter()
                .filter(|r| r.outcome == ReviewOutcome::Correction)
                .count();
            let exhausted = (delivery.retries >= REVIEW_RETRY_LIMIT).then(|| {
                let last = delivery.last_error.as_deref().unwrap_or("unknown error");
                format!("Review failed {} times: {last}", delivery.retries)
            });
            // Fail closed: an error reading the policy fails the pass (the
            // next tick retries) rather than counting as authority.
            let commits_refused = match self.authorize(store, &task.id, Authority::Commit) {
                Ok(()) => false,
                Err(TaskError::Invalid(_)) => true,
                Err(error) => return Err(error),
            };
            let rebase_onto =
                super::integration::rebase_onto(tasks.reviews(&task.id, &assignment.id)?.last());
            jobs.push(ReviewJob {
                commits_refused,
                rebase_onto,
                superseded: superseded(&tasks, &task, &assignment, &attempt, claim.as_ref())?,
                stale: stale(&tasks, &attempt, &assignment)?,
                exhausted,
                delivery,
                task,
                assignment,
                attempt,
                claim,
                workspace,
                corrections,
            });
        }
        Ok(jobs)
    }

    /// Counts a failure to inspect or record `job` against its request; after
    /// [`REVIEW_RETRY_LIMIT`] the next review hands the claim to Mono.
    pub fn review_failed(
        &self,
        store: &crate::store::Store,
        job: &ReviewJob,
        error: &str,
        now: u64,
    ) -> Result<(), TaskError> {
        let tasks = store.tasks()?;
        if let Some(d) = tasks.delivery(&job.delivery.id)? {
            if d.state == super::DeliveryState::Pending {
                let failed = DeliveryOutcome::Failed(error.into());
                tasks.record_delivery(&d.id, d.revision, failed, now)?;
            }
        }
        Ok(())
    }

    /// Records the decision for an inspected job in one transaction: the
    /// assignment's transition, its verifications, the review record and the
    /// delivery's acknowledgment. Returns None when nothing was decided: the
    /// attempt was already reviewed, the lease was lost, the assignment
    /// changed during inspection, or inspection failed and will be retried.
    pub fn record_review(
        &self,
        store: &crate::store::Store,
        job: &ReviewJob,
        inspection: Inspection,
        now: u64,
    ) -> Result<Option<Review>, TaskError> {
        let tasks = store.tasks()?;
        match self.lease(&tasks, &job.task.id, now) {
            Err(TaskError::LeaseHeld { .. }) => return Ok(None),
            other => other?,
        }
        if let Some(error) = &inspection.error {
            if job.delivery.retries + 1 < REVIEW_RETRY_LIMIT {
                self.review_failed(store, job, error, now)?;
                return Ok(None);
            }
        }
        let tx = tasks.begin()?;
        if tasks.review(&job.attempt.id)?.is_some() {
            acknowledge(&tx, &job.delivery.id, now)?;
            tx.commit()?;
            return Ok(None);
        }
        let subject = Subject::Assignment {
            task: &job.task.id,
            id: &job.assignment.id,
        };
        if job.superseded.is_none() && tasks.revision(subject)? != job.assignment.revision {
            return Ok(None);
        }
        let (outcome, findings) = job.decide(&inspection);
        let to = match outcome {
            ReviewOutcome::Accepted => Some(AssignmentState::Accepted),
            ReviewOutcome::Correction => Some(AssignmentState::Queued),
            _ => None,
        };
        if let Some(to) = to {
            let (from, coordinator) = (job.assignment.revision, Requester::Coordinator);
            apply(&tx, subject, State::Assignment(to), coordinator, from, now)?;
        }
        if let (Some(onto), Some(workspace)) = (&inspection.rebased_onto, &job.workspace) {
            super::integration::rebased(&tx, workspace, onto, now)?;
        }
        let verifications = record_checks(&tasks, job, &inspection, now)?;
        let review = Review {
            attempt_id: job.attempt.id.clone(),
            assignment_id: job.assignment.id.clone(),
            outcome,
            checkpoint: inspection.checkpoint.clone(),
            changed_files: inspection.changed_files.clone(),
            findings,
            verifications,
        };
        let id = reviewed_event(&job.attempt.id);
        let payload = serde_json::to_value(&review)?;
        append(
            &tx,
            &event(&job.attempt, id.clone(), REVIEWED, payload),
            now,
        )?;
        if outcome == ReviewOutcome::NeedsMonoReview {
            let reference = format!("attempt:{}", job.attempt.id);
            let handoff = format!("mono-review:{}", job.attempt.id);
            tasks.enqueue_delivery(&handoff, &id, MONO_REVIEW, &reference, now)?;
        }
        acknowledge(&tx, &job.delivery.id, now)?;
        tx.commit()?;
        Ok(Some(review))
    }

    /// Collects, inspects and records every pending review in turn, holding
    /// `store` throughout. The app uses the three steps separately.
    pub fn review(&self, store: &crate::store::Store, now: u64) -> Result<Vec<Review>, TaskError> {
        let mut reviews = Vec::new();
        let mut tick = super::coordinator::Tick::default();
        for job in self.review_jobs(store, now, &mut tick)? {
            let inspection = job.inspect();
            reviews.extend(self.record_review(store, &job, inspection, now)?);
        }
        Ok(reviews)
    }
}

/// One verification per check that ran against the inspected snapshot;
/// none when a check changed the workspace.
fn record_checks(
    tasks: &TaskStore<'_>,
    job: &ReviewJob,
    inspection: &Inspection,
    now: u64,
) -> Result<Vec<String>, TaskError> {
    let (Some(checkpoint), Some(workspace)) = (&inspection.checkpoint, &job.workspace) else {
        return Ok(Vec::new());
    };
    if !inspection.batch.trustworthy() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    for (index, result) in inspection.batch.results.iter().enumerate() {
        let Ok(run) = result else {
            continue;
        };
        let id = format!("check:{}:{index}", job.attempt.id);
        tasks.record_verification(&Verification {
            id: id.clone(),
            task_id: job.task.id.clone(),
            assignment_id: Some(job.assignment.id.clone()),
            criterion_id: None,
            checkpoint_commit: checkpoint.clone(),
            check: run.check.clone(),
            passed: run.passed(),
            evidence: evidence(workspace, run),
            invalidated: false,
            created_at: now,
        })?;
        ids.push(id);
    }
    Ok(ids)
}

impl TaskStore<'_> {
    /// The review of `attempt_id`'s claim, once recorded.
    pub fn review(&self, attempt_id: &str) -> Result<Option<Review>, TaskError> {
        let payload: Option<String> = self
            .conn
            .query_row(
                "SELECT payload FROM native_task_events WHERE id=?1",
                [reviewed_event(attempt_id)],
                |row| row.get(0),
            )
            .optional()?;
        payload
            .map(|p| serde_json::from_str(&p).map_err(TaskError::from))
            .transpose()
    }

    /// An assignment's reviews, oldest first.
    pub fn reviews(&self, task_id: &str, assignment_id: &str) -> Result<Vec<Review>, TaskError> {
        let mut statement = self.conn.prepare(
            "SELECT payload FROM native_task_events
             WHERE task_id=?1 AND assignment_id=?2 AND kind=?3 ORDER BY seq",
        )?;
        let rows = statement.query_map([task_id, assignment_id, REVIEWED], |row| {
            row.get::<_, String>(0)
        })?;
        rows.map(|payload| Ok(serde_json::from_str(&payload?)?))
            .collect()
    }

    /// The assignment's latest review unless it accepted the work or was
    /// superseded: what the next attempt must address, whether review sent
    /// the work back itself or Mono did after judging it.
    pub fn latest_correction(
        &self,
        task_id: &str,
        assignment_id: &str,
    ) -> Result<Option<Review>, TaskError> {
        Ok(self
            .reviews(task_id, assignment_id)?
            .pop()
            .filter(|review| {
                !matches!(
                    review.outcome,
                    ReviewOutcome::Accepted | ReviewOutcome::Superseded
                )
            }))
    }
}

#[cfg(test)]
#[path = "review_tests.rs"]
pub(super) mod tests;

#[cfg(test)]
#[path = "review_guard_tests.rs"]
mod guard_tests;
