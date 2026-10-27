//! Integration of a task whose accepted work spans several worktrees.
//!
//! Once every live assignment is Accepted and two or more of them worked in
//! their own worktrees, the coordinator integrates them, but only with merge
//! authority (`Coordinator::authorize(Merge)`, checked right before each
//! pass); without it the task raises a decision instead. Integration:
//!
//! 1. confirms the integration base: the base recorded for the task's
//!    integration worktree, else the first recorded assignment base. If the
//!    user's branch has moved since, the work is still integrated onto the
//!    recorded base (never silently rebased onto the new upstream) and both
//!    are recorded;
//! 2. records what it integrates: each assignment's branch tip, in
//!    dependency order (plan order among independent ones);
//! 3. merges each branch onto the task's integration branch
//!    (`mono/<task>/integration`) in its own worktree, skipping one already
//!    contained. A conflict is aborted at once, so the integration worktree
//!    stays clean, and its files are recorded; uncommitted work in an
//!    assignment's worktree is never merged or touched;
//! 4. resolves a conflict (or uncommitted work) within the task's
//!    authority: with commit authority and corrections left, the later
//!    assignment is sent back to rebase onto the integration branch (the
//!    stage returns to Implementing, and its next acceptance re-integrates);
//!    otherwise it raises a decision for Mono (who may ask the user);
//! 5. on a clean result, moves the stage to Verifying, where the acceptance
//!    criteria run on the integration worktree through the existing
//!    verification rounds and the task completes through `complete`.
//!
//! Every pass appends one `task.integration` record (the final one is the
//! task's integration state, also carried by the final report). Nothing
//! here pushes, deploys or deletes a worktree. As with review, preparing and
//! recording use the store; the Git work (`merging`) does not.
use super::autonomy::Authority;
use super::coordinator::{Coordinator, Policy};
use super::decisions::queue_for_mono;
use super::events::{append, NewEvent};
use super::findings::{Finding, Review, ReviewOutcome};
use super::merging::{dependency_order, Step};
pub use super::merging::{IntegrationJob, IntegrationRecord, Merge, Outcome};
use super::review::{acknowledge, MAX_CORRECTIONS, REVIEWED, REVIEW_RETRY_LIMIT};
use super::states::apply;
use super::store::insert_decision;
use super::{
    Assignment, AssignmentState, DecisionScope, DeliveryState, NewDecision, Requester, State,
    Subject, TaskError, TaskLifecycle, TaskStage, TaskStore, Workspace,
};
use crate::orchestration::WorkspaceMode;
use crate::store::Store;
use rusqlite::{params, Connection};

/// The kind of each integration record.
pub const INTEGRATION: &str = "task.integration";
/// The prefix of the decisions integration raises.
const DECISION: &str = "integration:";
const AFTER_ANSWER: &str = "Integration runs again once this decision is answered.";

/// The task's integration branch.
pub fn branch_for(task: &str) -> String {
    super::workspaces::branch_for(task, RESERVED_ID)
}

/// The name integration's branch and worktree take after the task; no
/// assignment may use it (ignoring case).
pub const RESERVED_ID: &str = "integration";

/// Refuses a plan with an assignment named [`RESERVED_ID`] (ignoring case).
pub(super) fn refuse_reserved(plan: &crate::orchestration::Plan) -> Result<(), TaskError> {
    match plan
        .tasks
        .iter()
        .find(|a| a.id.eq_ignore_ascii_case(RESERVED_ID))
    {
        Some(taken) => Err(TaskError::Invalid(format!(
            "Assignment ID {} is reserved for the task's integration branch; rename it.",
            taken.id
        ))),
        None => Ok(()),
    }
}

/// The record id of the task's integration worktree.
pub fn workspace_id(task: &str) -> String {
    format!("{task}/integration")
}

/// Tasks that may be ready to integrate.
fn integrating(tasks: &TaskStore<'_>) -> Result<Vec<String>, TaskError> {
    let mut statement = tasks.conn.prepare(
        "SELECT id FROM native_tasks WHERE lifecycle='active' AND stage='integrating' ORDER BY rowid",
    )?;
    let ids = statement.query_map([], |row| row.get(0))?;
    Ok(ids.collect::<Result<_, _>>()?)
}

fn prefix(task: &str) -> String {
    format!("{DECISION}{task}:")
}

/// Whether a decision integration raised for the task is not resolved yet.
fn unresolved(conn: &Connection, task: &str) -> Result<bool, TaskError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM native_task_decisions WHERE task_id=?1
           AND substr(id,1,length(?2))=?2 AND state<>'resolved')",
        params![task, prefix(task)],
        |row| row.get(0),
    )?)
}

/// The snapshot the assignment's acceptance rests on: the checkpoint of its
/// latest review that has one, whatever its outcome (the state Mono saw
/// when it accepted over a correction, or what review accepted).
fn accepted_checkpoint(
    tasks: &TaskStore<'_>,
    task: &str,
    assignment: &str,
) -> Result<Option<String>, TaskError> {
    Ok(tasks
        .reviews(task, assignment)?
        .into_iter()
        .rev()
        .find_map(|r| r.checkpoint))
}

/// Raises a task-wide decision for Mono, inside the caller's transaction.
fn raise(
    conn: &Connection,
    task: &str,
    assignment: Option<&str>,
    question: String,
    context: String,
    now: u64,
) -> Result<String, TaskError> {
    let earlier: i64 = conn.query_row(
        "SELECT COUNT(*) FROM native_task_decisions WHERE task_id=?1 AND substr(id,1,length(?2))=?2",
        params![task, prefix(task)],
        |row| row.get(0),
    )?;
    let id = format!("{}{}", prefix(task), earlier + 1);
    let decision = NewDecision {
        id: id.clone(),
        task_id: task.into(),
        assignment_id: assignment.map(str::to_owned),
        attempt_id: None,
        question,
        context: format!("{context} {AFTER_ANSWER}"),
        scope: DecisionScope::Task,
        asked_by: Requester::Coordinator,
    };
    insert_decision(conn, &decision, now)?;
    queue_for_mono(conn, &id, now)?;
    Ok(id)
}

/// Raises `question` in its own transaction.
fn raise_now(
    tasks: &TaskStore<'_>,
    task: &str,
    question: String,
    context: String,
    now: u64,
) -> Result<(), TaskError> {
    let tx = tasks.begin()?;
    raise(&tx, task, None, question, context, now)?;
    tx.commit()?;
    Ok(())
}

/// Failed passes recorded at the task's `revision`.
fn failures(conn: &Connection, task: &str, revision: u64) -> Result<u32, TaskError> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM native_task_events WHERE task_id=?1 AND kind=?2
           AND substr(id,1,length(?3))=?3 AND json_extract(payload,'$.outcome.state')='failed'",
        params![task, INTEGRATION, format!("integration:{task}@{revision}#")],
        |row| row.get(0),
    )?;
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

/// The worktree record for the task's integration branch.
fn integration_workspace(
    job: &IntegrationJob,
    record: &IntegrationRecord,
    path: &str,
    now: u64,
) -> Workspace {
    Workspace {
        id: workspace_id(&job.task.id),
        task_id: job.task.id.clone(),
        assignment_id: None,
        mode: WorkspaceMode::Worktree,
        repository: record
            .repository
            .clone()
            .unwrap_or_else(|| job.task.project.clone()),
        path: path.into(),
        branch: Some(job.branch.clone()),
        base_commit: Some(record.base.clone()),
        owner: super::workspaces::owner_of(&job.task.id),
        revision: 0,
        created_at: now,
    }
}

impl<P: Policy> Coordinator<P> {
    /// Takes the answers Mono gave to integration's decisions: each is
    /// resolved, so the next pass runs again (and launches are no longer
    /// held by it).
    fn take_answers(&self, tasks: &TaskStore<'_>, now: u64) -> Result<(), TaskError> {
        let mut statement = tasks.conn.prepare(
            "SELECT id,task_id,revision FROM native_task_decisions
             WHERE state='answered' AND asked_by='coordinator' AND substr(id,1,length(?1))=?1",
        )?;
        let answered = statement
            .query_map([DECISION], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    super::store::read_ms(row, 2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (id, task, revision) in answered {
            match self.lease(tasks, &task, now) {
                Err(TaskError::LeaseHeld { .. }) => continue,
                other => other?,
            }
            let delivery = format!("answer:{id}");
            if tasks
                .delivery(&delivery)?
                .is_some_and(|d| d.state != DeliveryState::Acknowledged)
            {
                acknowledge(tasks.conn, &delivery, now)?;
            }
            tasks.resolve_decision(&id, revision, now)?;
        }
        Ok(())
    }

    /// Tasks whose accepted work is ready to integrate, with one error per
    /// task that could not be prepared (the others still run). A task
    /// without merge authority raises a decision instead.
    pub fn integration_jobs(
        &self,
        store: &Store,
        now: u64,
    ) -> Result<(Vec<IntegrationJob>, Vec<String>), TaskError> {
        let tasks = store.tasks()?;
        self.take_answers(&tasks, now)?;
        let mut jobs = Vec::new();
        let mut errors = Vec::new();
        for id in integrating(&tasks)? {
            match self.integration_job(store, &tasks, &id, now) {
                Ok(Some(job)) => jobs.push(job),
                Ok(None) | Err(TaskError::LeaseHeld { .. }) => {}
                Err(error) => errors.push(format!("{id}: {error}")),
            }
        }
        Ok((jobs, errors))
    }

    fn integration_job(
        &self,
        store: &Store,
        tasks: &TaskStore<'_>,
        id: &str,
        now: u64,
    ) -> Result<Option<IntegrationJob>, TaskError> {
        self.lease(tasks, id, now)?;
        let task = super::guard::task_or_missing(tasks, id)?;
        if task.stage != TaskStage::Integrating
            || task.lifecycle != TaskLifecycle::Active
            || unresolved(tasks.conn, id)?
        {
            return Ok(None);
        }
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
                let tree = workspaces.iter().find(|w| {
                    w.mode == WorkspaceMode::Worktree
                        && w.assignment_id.as_deref() == Some(a.id.as_str())
                })?;
                Some((a, tree.clone()))
            })
            .collect();
        if results.len() < 2 {
            return Ok(None);
        }
        let branch = branch_for(id);
        let names: Vec<String> = results
            .iter()
            .map(|(_, w)| w.branch.clone().unwrap_or_else(|| w.path.clone()))
            .collect();
        if results
            .iter()
            .any(|(_, w)| w.id == workspace_id(id) || w.branch.as_deref() == Some(branch.as_str()))
        {
            let question = format!("Task {id} cannot be integrated: an assignment's worktree already uses the integration branch {branch}.");
            let context = "Rename the assignment in the plan, or integrate the branches yourself.";
            raise_now(tasks, id, question, context.into(), now)?;
            return Ok(None);
        }
        match self.authorize(store, id, Authority::Merge) {
            Ok(()) => {}
            Err(TaskError::Invalid(reason)) => {
                let question = format!(
                    "Task {id}'s accepted work is on {} branches ({}) that must be merged onto its integration branch {branch}, but the task may not merge.",
                    names.len(),
                    names.join(", ")
                );
                let context = format!("{reason} Or integrate the branches yourself.");
                raise_now(tasks, id, question, context, now)?;
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
        let recorded = workspaces
            .iter()
            .find(|w| w.id == workspace_id(id))
            .cloned();
        let first = workspaces.iter().find_map(|w| {
            results
                .iter()
                .any(|(_, r)| r.id == w.id)
                .then(|| w.base_commit.clone())
                .flatten()
        });
        let Some(base) = recorded
            .as_ref()
            .and_then(|w| w.base_commit.clone())
            .or(first)
        else {
            let question = format!(
                "Task {id} cannot be integrated: no base commit is recorded for its worktrees."
            );
            raise_now(
                tasks,
                id,
                question,
                "Integrate the branches yourself, or re-plan the task.".into(),
                now,
            )?;
            return Ok(None);
        };
        let steps = dependency_order(results)
            .into_iter()
            .map(|(a, w)| {
                Ok(Step {
                    checkpoint: accepted_checkpoint(tasks, id, &a.id)?,
                    assignment: a.id,
                    branch: w.branch.unwrap_or_default(),
                    base: w.base_commit,
                })
            })
            .collect::<Result<_, TaskError>>()?;
        Ok(Some(IntegrationJob {
            task,
            branch,
            recorded,
            base,
            steps,
        }))
    }

    /// Whether the task may send `assignment` back to fix its branch: the
    /// worker must commit, and it has corrections left. Otherwise why not.
    fn correctable(
        &self,
        store: &Store,
        task: &str,
        assignment: &str,
    ) -> Result<Result<(), String>, TaskError> {
        match self.authorize(store, task, Authority::Commit) {
            Ok(()) => {}
            Err(TaskError::Invalid(reason)) => return Ok(Err(reason)),
            Err(error) => return Err(error),
        }
        let corrections = store
            .tasks()?
            .reviews(task, assignment)?
            .iter()
            .filter(|r| r.outcome == ReviewOutcome::Correction)
            .count();
        if corrections >= MAX_CORRECTIONS {
            return Ok(Err(format!(
                "Assignment {assignment} was already sent back {corrections} times."
            )));
        }
        Ok(Ok(()))
    }

    /// Records a pass: the integration worktree, the record, and what
    /// follows from it, in one transaction. None when the task changed
    /// meanwhile or the lease was lost.
    pub fn record_integration(
        &self,
        store: &Store,
        job: &IntegrationJob,
        record: IntegrationRecord,
        now: u64,
    ) -> Result<Option<IntegrationRecord>, TaskError> {
        let tasks = store.tasks()?;
        let id = job.task.id.as_str();
        match self.lease(&tasks, id, now) {
            Err(TaskError::LeaseHeld { .. }) => return Ok(None),
            other => other?,
        }
        let unchanged = tasks.task(id)?.is_some_and(|t| {
            t.revision == job.task.revision
                && t.stage == TaskStage::Integrating
                && t.lifecycle == TaskLifecycle::Active
        });
        if !unchanged {
            return Ok(None);
        }
        let fix = match &record.outcome {
            Outcome::Conflict { assignment, .. }
            | Outcome::NoCommits { assignment, .. }
            | Outcome::Uncommitted { assignment, .. } => {
                Some(self.correctable(store, id, assignment)?)
            }
            _ => None,
        };
        // Merging ran without the store lock: authority revoked meanwhile
        // stops the result from moving on.
        let merge = match &record.outcome {
            Outcome::Clean => match self.authorize(store, id, Authority::Merge) {
                Ok(()) => Ok(()),
                Err(TaskError::Invalid(reason)) => Err(reason),
                Err(error) => return Err(error),
            },
            _ => Ok(()),
        };
        let tx = tasks.begin()?;
        let inside = TaskStore { conn: &tx };
        if let Some(path) = &record.path {
            let recorded = inside
                .workspaces(id)?
                .iter()
                .any(|w| w.id == workspace_id(id));
            if !recorded {
                inside.save_workspace(&integration_workspace(job, &record, path, now))?;
            }
        }
        let revision = job.task.revision;
        let n = inside.events_of(id, INTEGRATION)?;
        let event = NewEvent {
            id: format!("integration:{id}@{revision}#{n}"),
            task_id: id.into(),
            assignment_id: None,
            attempt_id: None,
            kind: INTEGRATION.into(),
            payload: serde_json::to_value(&record)?,
        };
        append(&tx, &event, now)?;
        match &record.outcome {
            Outcome::Clean if merge.is_err() => {
                let reason = merge.err().unwrap_or_default();
                let question = format!(
                    "Task {id}'s branches were merged onto {}, but merge authority was revoked meanwhile, so the result does not move on to verification.",
                    job.branch
                );
                raise(
                    &tx,
                    id,
                    None,
                    question,
                    format!("{reason} Or integrate the branches yourself."),
                    now,
                )?;
            }
            Outcome::Clean => {
                let verifying = State::Stage(TaskStage::Verifying);
                apply(
                    &tx,
                    Subject::Task(id),
                    verifying,
                    Requester::Coordinator,
                    revision,
                    now,
                )?;
            }
            Outcome::Conflict {
                assignment,
                branch,
                files,
            } => {
                let finding = Finding::IntegrationConflict {
                    branch: job.branch.clone(),
                    onto: record.head.clone().unwrap_or_else(|| job.base.clone()),
                    files: files.clone(),
                };
                let what = format!(
                    "Merging assignment {assignment}'s branch {branch} onto the integration branch {} conflicted in {}. The merge was aborted; the integration worktree is clean.",
                    job.branch,
                    files.join(", ")
                );
                settle(&tx, job, assignment, finding, what, fix, n, now)?;
            }
            Outcome::Uncommitted {
                assignment,
                branch,
                files,
            } => {
                let finding = Finding::UncommittedWork {
                    branch: branch.clone(),
                    files: files.clone(),
                };
                let what = format!(
                    "Assignment {assignment}'s accepted work is not all committed on {branch} ({} differ from its last commit), so it cannot be merged onto {} as accepted.",
                    files.join(", "),
                    job.branch
                );
                settle(&tx, job, assignment, finding, what, fix, n, now)?;
            }
            Outcome::NoCommits { assignment, branch } => {
                let finding = Finding::NoCommits {
                    branch: branch.clone(),
                };
                let what = format!(
                    "Assignment {assignment}'s branch {branch} has no commits since its base, so nothing of it can be merged onto {}.",
                    job.branch
                );
                settle(&tx, job, assignment, finding, what, fix, n, now)?;
            }
            Outcome::Failed { reason } => {
                let failed = failures(&tx, id, revision)?;
                if failed >= REVIEW_RETRY_LIMIT && failed % REVIEW_RETRY_LIMIT == 0 {
                    let question = format!("Integrating task {id} failed {failed} times: {reason}");
                    let context = "Fix the cause (or integrate the branches yourself).".into();
                    raise(&tx, id, None, question, context, now)?;
                }
            }
        }
        tx.commit()?;
        Ok(Some(record))
    }

    /// Prepares, runs and records every integration in turn, holding
    /// `store` throughout. The app runs the three steps separately.
    pub fn integrate(&self, store: &Store, now: u64) -> Result<Vec<IntegrationRecord>, TaskError> {
        let (jobs, errors) = self.integration_jobs(store, now)?;
        if let Some(error) = errors.into_iter().next() {
            return Err(TaskError::Storage(error));
        }
        let mut records = Vec::new();
        for job in jobs {
            let record = job.run();
            records.extend(self.record_integration(store, &job, record, now)?);
        }
        Ok(records)
    }
}

/// A conflict or uncommitted work: sent back with a correction when the
/// task may (`fix`), otherwise a decision for Mono.
#[allow(clippy::too_many_arguments)]
fn settle(
    conn: &Connection,
    job: &IntegrationJob,
    assignment: &str,
    finding: Finding,
    what: String,
    fix: Option<Result<(), String>>,
    n: u64,
    now: u64,
) -> Result<(), TaskError> {
    let task = job.task.id.as_str();
    if let Some(Err(why)) = fix {
        let context = format!(
            "Sending the assignment back is beyond the task's authority: {why} Resolve it (for example send the assignment back yourself, or fix its branch), then answer."
        );
        raise(conn, task, Some(assignment), what, context, now)?;
        return Ok(());
    }
    let inside = TaskStore { conn };
    let subject = Subject::Assignment {
        task,
        id: assignment,
    };
    let queued = State::Assignment(AssignmentState::Queued);
    let coordinator = Requester::Coordinator;
    apply(
        conn,
        subject,
        queued,
        coordinator,
        inside.revision(subject)?,
        now,
    )?;
    let stage = Subject::Task(task);
    let implementing = State::Stage(TaskStage::Implementing);
    apply(
        conn,
        stage,
        implementing,
        coordinator,
        inside.revision(stage)?,
        now,
    )?;
    let attempt = inside.attempts(task, assignment)?.pop();
    let review = Review {
        attempt_id: attempt.as_ref().map(|a| a.id.clone()).unwrap_or_default(),
        assignment_id: assignment.into(),
        outcome: ReviewOutcome::Correction,
        checkpoint: None,
        changed_files: Vec::new(),
        findings: vec![finding],
        verifications: Vec::new(),
    };
    let event = NewEvent {
        id: format!("integration-review:{task}@{}#{n}", job.task.revision),
        task_id: task.into(),
        assignment_id: Some(assignment.into()),
        attempt_id: attempt.map(|a| a.id),
        kind: REVIEWED.into(),
        payload: serde_json::to_value(&review)?,
    };
    append(conn, &event, now)?;
    Ok(())
}

impl TaskStore<'_> {
    /// How many events of `kind` the task has.
    fn events_of(&self, task: &str, kind: &str) -> Result<u64, TaskError> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM native_task_events WHERE task_id=?1 AND kind=?2",
            [task, kind],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(n).unwrap_or_default())
    }

    /// Whether the task's latest integration record is clean and newer
    /// (by event order) than the latest acceptance of each of
    /// `assignments`, so it holds the work as accepted now.
    pub fn integration_current(
        &self,
        task: &str,
        assignments: &[String],
    ) -> Result<bool, TaskError> {
        let latest = |kind: &str, assignment: Option<&str>| -> Result<Option<i64>, TaskError> {
            Ok(self.conn.query_row(
                "SELECT MAX(seq) FROM native_task_events WHERE task_id=?1 AND kind=?2
                   AND (?3 IS NULL OR assignment_id=?3)",
                params![task, kind, assignment],
                |row| row.get(0),
            )?)
        };
        let clean = self
            .integration(task)?
            .is_some_and(|r| r["outcome"]["state"] == "clean");
        let Some(integrated) = latest(INTEGRATION, None)?.filter(|_| clean) else {
            return Ok(false);
        };
        for assignment in assignments {
            if latest("assignment.accepted", Some(assignment))?.is_some_and(|s| s > integrated) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The task's latest integration record.
    pub fn integration(&self, task: &str) -> Result<Option<serde_json::Value>, TaskError> {
        let payload: Option<String> = rusqlite::OptionalExtension::optional(self.conn.query_row(
            "SELECT payload FROM native_task_events WHERE task_id=?1 AND kind=?2 ORDER BY seq DESC LIMIT 1",
            [task, INTEGRATION],
            |row| row.get(0),
        ))?;
        payload
            .map(|p| serde_json::from_str(&p).map_err(TaskError::from))
            .transpose()
    }
}

/// The base a rebased worktree now starts from: `onto` from the
/// assignment's latest review, when that sent it back to rebase onto the
/// integration branch.
pub(super) fn rebase_onto(review: Option<&Review>) -> Option<String> {
    review?.findings.iter().find_map(|f| match f {
        Finding::IntegrationConflict { onto, .. } => Some(onto.clone()),
        _ => None,
    })
}

/// Records that the assignment's worktree now starts from `onto` (it was
/// rebased onto the integration branch), inside the caller's transaction.
pub(super) fn rebased(
    conn: &Connection,
    workspace: &Workspace,
    onto: &str,
    now: u64,
) -> Result<(), TaskError> {
    if workspace.base_commit.as_deref() == Some(onto) {
        return Ok(());
    }
    conn.execute(
        "UPDATE native_task_workspaces SET base_commit=?2,revision=revision+1,updated_at=?3 WHERE id=?1",
        params![workspace.id, onto, super::store::ms(now)],
    )?;
    let event = NewEvent {
        id: format!("rebased:{}@{onto}", workspace.id),
        task_id: workspace.task_id.clone(),
        assignment_id: workspace.assignment_id.clone(),
        attempt_id: None,
        kind: "workspace.rebased".into(),
        payload: serde_json::json!({"workspaceId": workspace.id, "from": workspace.base_commit, "to": onto}),
    };
    append(conn, &event, now)?;
    Ok(())
}

#[cfg(test)]
#[path = "integration_tests.rs"]
pub(super) mod tests;

#[cfg(test)]
#[path = "integration_guard_tests.rs"]
mod guard_tests;
