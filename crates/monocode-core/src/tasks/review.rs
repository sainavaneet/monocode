//! The coordinator's deterministic review of a worker's success claim: the
//! step between "the worker says it is done" and the assignment counting as
//! done.
//!
//! For each pending `review:<attempt>` delivery: check the claim is still the
//! current run of the current requirements, snapshot the actual workspace
//! (`checkpoint`), flag files changed outside the assignment's scope, compare
//! the reported validation with the plan's checks, run those checks in the
//! workspace, and then accept, send back with a durable correction, or hand
//! the claim to Mono when it cannot be decided mechanically. Collecting jobs
//! and recording outcomes need the store; inspecting (Git and the checks)
//! does not, so the app can run it without holding the store lock.
use super::checks::{self, CheckRun, CHECK_TIMEOUT_MS};
use super::completion::evidence;
use super::coordinator::{Coordinator, Policy, REVIEW};
use super::events::{append, PendingDelivery};
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
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;

/// Corrections an assignment may receive before review stops re-queueing it
/// and hands the next claim to Mono.
pub const MAX_CORRECTIONS: usize = 3;
/// Failed inspections (Git errors) before review hands the claim to Mono.
pub const REVIEW_RETRY_LIMIT: u32 = 3;
/// Where claims that need judgement wait for Mono's review.
pub const MONO_REVIEW: &str = "mono_review";
const REVIEWED: &str = "assignment.reviewed";
/// How much failing output a worker brief repeats.
const BRIEF_OUTPUT_CHARS: usize = 1_500;

/// Something review found in a claim, its workspace or its checks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Finding {
    /// The claim no longer belongs to the current run of the assignment.
    Superseded {
        reason: String,
    },
    /// The assignment's requirements changed after the attempt started.
    StaleRevision {
        reason: String,
    },
    OutOfScope {
        path: String,
    },
    FailedCheck {
        check: String,
        exit_code: Option<i32>,
        timed_out: bool,
        output: String,
    },
    /// The step needs a shell or a reader; it was not run.
    UnrunnableCheck {
        check: String,
        reason: String,
    },
    /// A plan check the worker's validation does not mention. Informational:
    /// review runs every check itself.
    UnreportedCheck {
        check: String,
    },
    /// The plan gives no check, so nothing could verify the work.
    NoChecks,
    /// The worker claimed success but listed work still to do.
    RemainingWork {
        item: String,
    },
    Workspace {
        reason: String,
    },
    CorrectionLimit {
        corrections: usize,
    },
}

impl Finding {
    /// Mechanically wrong: the work goes back to a worker.
    fn is_corrective(&self) -> bool {
        matches!(
            self,
            Self::StaleRevision { .. } | Self::OutOfScope { .. } | Self::FailedCheck { .. }
        )
    }

    /// Not decidable without judgement.
    fn needs_mono(&self) -> bool {
        matches!(
            self,
            Self::UnrunnableCheck { .. }
                | Self::NoChecks
                | Self::RemainingWork { .. }
                | Self::Workspace { .. }
                | Self::CorrectionLimit { .. }
        )
    }

    /// One line for the next worker's brief.
    pub fn describe(&self) -> String {
        match self {
            Self::Superseded { reason } => format!("The claim was superseded: {reason}"),
            Self::StaleRevision { reason } => reason.clone(),
            Self::OutOfScope { path } => {
                format!("{path} was changed outside the files you may change.")
            }
            Self::FailedCheck {
                check,
                exit_code,
                timed_out,
                output,
            } => {
                let how = match (timed_out, exit_code) {
                    (true, _) => "timed out".to_owned(),
                    (false, Some(code)) => format!("failed with exit code {code}"),
                    (false, None) => "was killed".to_owned(),
                };
                if output.is_empty() {
                    return format!("`{check}` {how} with no output.");
                }
                let start = output.chars().count().saturating_sub(BRIEF_OUTPUT_CHARS);
                let end: String = output.chars().skip(start).collect();
                format!("`{check}` {how}. End of its output:\n{end}")
            }
            Self::UnrunnableCheck { check, reason } => {
                format!("`{check}` could not be run: {reason}")
            }
            Self::UnreportedCheck { check } => {
                format!("`{check}` was not in the validation you reported.")
            }
            Self::NoChecks => "The plan gave no check to verify the work.".into(),
            Self::RemainingWork { item } => format!("You reported remaining work: {item}"),
            Self::Workspace { reason } => format!("The workspace could not be inspected: {reason}"),
            Self::CorrectionLimit { corrections } => {
                format!("The work was already sent back {corrections} times.")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewOutcome {
    /// Every check ran and passed on the inspected snapshot.
    Accepted,
    /// Sent back to Queued with the findings for the next attempt.
    Correction,
    /// Left in NeedsReview for Mono's judgement (a `mono_review` delivery).
    NeedsMonoReview,
    /// The claim is no longer current; nothing changed.
    Superseded,
}

/// One attempt's review, persisted as event `reviewed:<attempt>` (kind
/// `assignment.reviewed`). A correction is a review with that outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    pub attempt_id: String,
    pub assignment_id: String,
    pub outcome: ReviewOutcome,
    /// The workspace snapshot inspected and checked.
    pub checkpoint: Option<String>,
    pub changed_files: Vec<String>,
    pub findings: Vec<Finding>,
    /// The verification records of the checks that ran.
    pub verifications: Vec<String>,
}

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
    corrections: usize,
}

/// What inspecting the workspace found; no store access.
#[derive(Default)]
pub struct Inspection {
    checkpoint: Option<String>,
    changed_files: Vec<String>,
    findings: Vec<Finding>,
    runs: Vec<CheckRun>,
    /// Git could not inspect the workspace; the review is retried.
    error: Option<String>,
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

/// A Git ref component from a record id.
pub(super) fn ref_part(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn in_scope(path: &str, files: &[String]) -> bool {
    files
        .iter()
        .any(|scope| scope == "." || Path::new(path).starts_with(scope))
}

impl ReviewJob {
    /// Snapshots the workspace, checks the scope and runs the plan's checks.
    /// Touches no store, so it may run without the store lock.
    pub fn inspect(&self) -> Inspection {
        let mut found = Inspection::default();
        if self.superseded.is_some() {
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
        let reference = format!(
            "refs/monocode/tasks/{}/{}/{}",
            ref_part(&self.task.id),
            ref_part(&self.assignment.id),
            ref_part(&self.attempt.id)
        );
        let snapshot = checkpoint::snapshot_workspace(&workspace.path, &reference);
        let checkpoint = match snapshot {
            Ok(commit) => commit,
            Err(error) => {
                found.error = Some(error);
                return found;
            }
        };
        if workspace.mode == WorkspaceMode::Worktree {
            let base = workspace.base_commit.as_deref().unwrap_or("HEAD");
            match checkpoint::changed_paths(&workspace.path, base, &checkpoint) {
                Ok(paths) => found.changed_files = paths,
                Err(error) => {
                    found.error = Some(error);
                    return found;
                }
            }
            for path in &found.changed_files {
                if !in_scope(path, &self.assignment.files) {
                    found
                        .findings
                        .push(Finding::OutOfScope { path: path.clone() });
                }
            }
        }
        found.checkpoint = Some(checkpoint);
        if self.assignment.verification.is_empty() {
            found.findings.push(Finding::NoChecks);
        }
        let timeout = Duration::from_millis(CHECK_TIMEOUT_MS);
        for check in &self.assignment.verification {
            match checks::run(check, Path::new(&workspace.path), timeout) {
                Ok(run) => {
                    if !run.passed() {
                        found.findings.push(Finding::FailedCheck {
                            check: check.clone(),
                            exit_code: run.exit_code,
                            timed_out: run.timed_out,
                            output: run.output.clone(),
                        });
                    }
                    found.runs.push(run);
                }
                Err(reason) => found.findings.push(Finding::UnrunnableCheck {
                    check: check.clone(),
                    reason,
                }),
            }
        }
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
        if let Some(reason) = &inspection.error {
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
        let verified = inspection.runs.iter().any(CheckRun::passed);
        if findings.iter().any(Finding::needs_mono) || !verified {
            return (ReviewOutcome::NeedsMonoReview, findings);
        }
        (ReviewOutcome::Accepted, findings)
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

fn acknowledge(conn: &Connection, id: &str, now: u64) -> Result<(), TaskError> {
    let key = Key::new("native_task_deliveries", "id=?", vec![id], "delivery");
    let revision = key.revision(conn)?;
    key.bump(conn, revision, "state=?", vec![&"acknowledged"], now)?;
    Ok(())
}

impl<P: Policy> Coordinator<P> {
    /// The pending review requests this coordinator holds the task lease
    /// for. A request already reviewed is acknowledged and skipped; a paused
    /// or blocked task's requests wait.
    pub fn review_jobs(
        &self,
        store: &crate::store::Store,
        now: u64,
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
            jobs.push(ReviewJob {
                superseded: superseded(&tasks, &task, &assignment, &attempt, claim.as_ref())?,
                stale: stale(&tasks, &attempt, &assignment)?,
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
            let delivery = tasks.delivery(&job.delivery.id)?;
            if let Some(d) = delivery.filter(|d| d.retries + 1 < REVIEW_RETRY_LIMIT) {
                let failed = DeliveryOutcome::Failed(error.clone());
                tasks.record_delivery(&d.id, d.revision, failed, now)?;
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
        for job in self.review_jobs(store, now)? {
            let inspection = job.inspect();
            reviews.extend(self.record_review(store, &job, inspection, now)?);
        }
        Ok(reviews)
    }
}

/// One verification per check that ran, against the inspected snapshot.
fn record_checks(
    tasks: &TaskStore<'_>,
    job: &ReviewJob,
    inspection: &Inspection,
    now: u64,
) -> Result<Vec<String>, TaskError> {
    let (Some(checkpoint), Some(workspace)) = (&inspection.checkpoint, &job.workspace) else {
        return Ok(Vec::new());
    };
    let mut ids = Vec::new();
    for (index, run) in inspection.runs.iter().enumerate() {
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

    /// The assignment's latest review, when it sent the work back: what the
    /// next attempt must address.
    pub fn latest_correction(
        &self,
        task_id: &str,
        assignment_id: &str,
    ) -> Result<Option<Review>, TaskError> {
        Ok(self
            .reviews(task_id, assignment_id)?
            .pop()
            .filter(|review| review.outcome == ReviewOutcome::Correction))
    }
}

#[cfg(test)]
#[path = "review_tests.rs"]
mod tests;
