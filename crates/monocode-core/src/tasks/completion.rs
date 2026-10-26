//! Verification freshness, criterion checks, the final report and task
//! completion.
//!
//! A verification names the checkpoint commit it ran against. It stops
//! counting when the work is sent back, or when the workspace's current
//! snapshot holds different files: it is then marked invalidated rather than
//! shown as a stale result. Completion needs every assignment accepted, at
//! least one required criterion and every required criterion satisfied by a
//! live verification, no unresolved decision, and a final report persisted in
//! the same transaction. The report states implemented, committed, merged,
//! pushed and deployed from evidence only, never from a worker's words.
use super::checks::{self, CheckRun};
use super::events::{append, NewEvent};
use super::findings::task_ref;
use super::states::apply;
use super::{
    AssignmentState, Requester, State, Subject, TaskError, TaskLifecycle, TaskStage, Verification,
    Workspace,
};
use crate::checkpoint;
use crate::git::git;
use crate::orchestration::WorkspaceMode;
use crate::store::Store;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;

/// The record kind of a persisted final report; its event id is
/// `final_report:<task>`.
pub const FINAL_REPORT: &str = "task.final_report";
const INVALIDATED: &str = "verification.invalidated";

/// The verification evidence: where and how the check ran, and the end of
/// its output.
pub(super) fn evidence(workspace: &Workspace, run: &CheckRun) -> String {
    json!({
        "workspace": workspace.id,
        "path": workspace.path,
        "argv": run.argv,
        "exitCode": run.exit_code,
        "timedOut": run.timed_out,
        "output": run.output,
        "truncated": run.truncated,
        "durationMs": run.duration_ms,
    })
    .to_string()
}

/// Sending work back makes the verifications of what is being redone stop
/// counting: an assignment's own when it returns to Queued, every one of the
/// task's when its stage returns to Implementing.
pub(super) fn invalidate_on_send_back(
    conn: &Connection,
    task: &str,
    assignment: Option<&str>,
    from: State,
    to: State,
) -> Result<(), TaskError> {
    use TaskStage::{Implementing, Integrating, Reviewing, Verifying};
    match (from, to) {
        (State::Assignment(_), State::Assignment(AssignmentState::Queued)) => {
            conn.execute(
                "UPDATE native_task_verifications SET invalidated=1
                 WHERE task_id=?1 AND assignment_id=?2 AND invalidated=0",
                [task, assignment.unwrap_or_default()],
            )?;
        }
        (State::Stage(Reviewing | Integrating | Verifying), State::Stage(Implementing)) => {
            conn.execute(
                "UPDATE native_task_verifications SET invalidated=1 WHERE task_id=?1 AND invalidated=0",
                [task],
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// One criterion check, split so the check itself runs without the store:
/// [`CriterionRun::prepare`] reads under the store, [`CriterionRun::run`]
/// snapshots and runs the check alone, [`CriterionRun::record`] re-checks the
/// task and records the verification.
#[derive(Clone, Debug)]
pub struct CriterionRun {
    pub task_id: String,
    pub criterion_id: String,
    pub workspace: Workspace,
    pub check: String,
    /// The task revision read when preparing; a change refuses the record.
    pub task_revision: u64,
    run_id: String,
}

impl CriterionRun {
    pub fn prepare(
        store: &Store,
        task_id: &str,
        criterion_id: &str,
        workspace: &Workspace,
        check: &str,
    ) -> Result<Self, TaskError> {
        let task = store
            .tasks()?
            .task(task_id)?
            .ok_or_else(|| TaskError::NotFound(format!("task:{task_id}")))?;
        if !task.criteria.iter().any(|c| c.id == criterion_id) {
            return Err(TaskError::Invalid(format!(
                "Task {task_id} has no criterion {criterion_id}."
            )));
        }
        Ok(Self {
            task_id: task_id.into(),
            criterion_id: criterion_id.into(),
            workspace: workspace.clone(),
            check: check.into(),
            task_revision: task.revision,
            run_id: crate::mono::new_id(),
        })
    }

    /// Snapshots the workspace and runs the check; touches no store.
    pub fn run(&self, now: u64) -> Result<Verification, TaskError> {
        let (task_id, criterion_id) = (&self.task_id, &self.criterion_id);
        let reference =
            |side: &str| task_ref(task_id, &["criteria", criterion_id, &self.run_id, side]);
        let checkpoint = checkpoint::snapshot_workspace(&self.workspace.path, &reference("before"))
            .map_err(|e| {
                TaskError::Invalid(format!("The workspace could not be snapshotted: {e}"))
            })?;
        let checks = [self.check.clone()];
        let mut batch = checks::run_batch(
            &self.workspace.path,
            &checks,
            &checkpoint,
            &reference("after"),
        );
        if let Some(error) = batch.error.take() {
            return Err(TaskError::Invalid(error));
        }
        if !batch.modified.is_empty() {
            return Err(TaskError::Invalid(format!(
                "`{}` modified the workspace ({}), so it verifies nothing.",
                self.check,
                batch.modified.join(", ")
            )));
        }
        let run = batch.results.remove(0).map_err(TaskError::Invalid)?;
        Ok(Verification {
            id: format!("criterion:{task_id}:{criterion_id}:{}", self.run_id),
            task_id: task_id.clone(),
            assignment_id: self.workspace.assignment_id.clone(),
            criterion_id: Some(criterion_id.clone()),
            checkpoint_commit: checkpoint,
            check: self.check.clone(),
            passed: run.passed(),
            evidence: evidence(&self.workspace, &run),
            invalidated: false,
            created_at: now,
        })
    }

    /// Records the verification if the task did not change meanwhile.
    pub fn record(&self, store: &Store, verification: &Verification) -> Result<(), TaskError> {
        let tasks = store.tasks()?;
        let actual = tasks.revision(super::Subject::Task(&self.task_id))?;
        if actual != self.task_revision {
            return Err(TaskError::Stale {
                subject: format!("task:{}", self.task_id),
                expected: self.task_revision,
                actual,
            });
        }
        tasks.record_verification(verification)
    }
}

/// Runs `check` in `workspace` against a fresh snapshot and records whether
/// it shows the task's criterion `criterion_id`, holding `store` throughout.
/// Which check demonstrates a criterion is the caller's judgement (Mono, or
/// integration's acceptance run); the result is evidence either way. The app
/// uses [`CriterionRun`]'s steps instead, so the check runs unlocked.
pub fn verify_criterion(
    store: &Store,
    task_id: &str,
    criterion_id: &str,
    workspace: &Workspace,
    check: &str,
    now: u64,
) -> Result<Verification, TaskError> {
    let run = CriterionRun::prepare(store, task_id, criterion_id, workspace, check)?;
    let verification = run.run(now)?;
    run.record(store, &verification)?;
    Ok(verification)
}

/// Snapshots each workspace the task's live verifications ran in and
/// invalidates those whose checkpoint no longer matches it (or whose
/// workspace is gone). Verifications without a recorded workspace path are
/// left as they are. Returns the invalidated ids.
pub fn refresh_verifications(
    store: &Store,
    task_id: &str,
    now: u64,
) -> Result<Vec<String>, TaskError> {
    let tasks = store.tasks()?;
    let mut current: HashMap<String, Option<String>> = HashMap::new();
    let mut stale = Vec::new();
    for v in tasks.verifications(task_id)? {
        if v.invalidated {
            continue;
        }
        let place: serde_json::Value = serde_json::from_str(&v.evidence).unwrap_or_default();
        let Some(path) = place["path"].as_str() else {
            continue;
        };
        let snapshot = current.entry(path.to_owned()).or_insert_with(|| {
            let workspace = place["workspace"].as_str().unwrap_or(path);
            let reference = task_ref(task_id, &["refresh", workspace]);
            checkpoint::snapshot_workspace(path, &reference).ok()
        });
        let same = snapshot.as_deref().is_some_and(|now| {
            checkpoint::same_snapshot(path, now, &v.checkpoint_commit).unwrap_or(false)
        });
        if !same {
            invalidate(&tasks, &v, now)?;
            stale.push(v.id);
        }
    }
    Ok(stale)
}

fn invalidate(tasks: &super::TaskStore<'_>, v: &Verification, now: u64) -> Result<(), TaskError> {
    let tx = tasks.begin()?;
    let changed = tx.execute(
        "UPDATE native_task_verifications SET invalidated=1 WHERE id=?1 AND invalidated=0",
        [&v.id],
    )?;
    if changed == 1 {
        let payload = json!({
            "verificationId": v.id,
            "checkpoint": v.checkpoint_commit,
            "reason": "The workspace changed after this check ran.",
        });
        let event = NewEvent {
            id: format!("invalidated:{}", v.id),
            task_id: v.task_id.clone(),
            assignment_id: v.assignment_id.clone(),
            attempt_id: None,
            kind: INVALIDATED.into(),
            payload,
        };
        append(&tx, &event, now)?;
    }
    tx.commit()?;
    Ok(())
}

/// The task's verifications that still reflect their workspace, after
/// [`refresh_verifications`].
pub fn live_verifications(
    store: &Store,
    task_id: &str,
    now: u64,
) -> Result<Vec<Verification>, TaskError> {
    refresh_verifications(store, task_id, now)?;
    Ok(store
        .tasks()?
        .verifications(task_id)?
        .into_iter()
        .filter(|v| !v.invalidated)
        .collect())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssignmentReport {
    pub id: String,
    pub status: AssignmentState,
    pub mode: Option<WorkspaceMode>,
    pub path: Option<String>,
    pub branch: Option<String>,
    pub base_commit: Option<String>,
    /// Commits on the branch since its base, oldest first.
    pub commits: Vec<String>,
    /// The worktree has changes no commit holds.
    pub uncommitted: bool,
    /// Accepted, with a live passing verification.
    pub implemented: bool,
    pub committed: bool,
    /// The branch tip is in the repository's checked-out HEAD.
    pub merged: bool,
    /// A remote-tracking branch contains the tip.
    pub pushed: bool,
    /// No task step records deployment evidence, so this is never inferred.
    pub deployed: bool,
    /// Live passing verifications of the assignment.
    pub verifications: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CriterionReport {
    pub id: String,
    pub required: bool,
    pub satisfied: bool,
    /// The latest live verification of the criterion and its checkpoint.
    pub verification: Option<String>,
    pub checkpoint: Option<String>,
}

/// What the task delivered, each claim derived from recorded evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FinalReport {
    pub task_id: String,
    /// Each flag holds for every assignment that is not cancelled (and
    /// committed, merged and pushed only when some assignment has a branch).
    pub implemented: bool,
    pub committed: bool,
    pub merged: bool,
    pub pushed: bool,
    pub deployed: bool,
    pub assignments: Vec<AssignmentReport>,
    pub criteria: Vec<CriterionReport>,
    pub generated_at: u64,
}

fn branch_facts(workspace: &Workspace, report: &mut AssignmentReport) {
    let (Some(branch), Some(base)) = (&workspace.branch, &workspace.base_commit) else {
        return;
    };
    let repository = Path::new(&workspace.repository);
    let reference = format!("refs/heads/{branch}");
    let Ok(tip) = git(repository, &["rev-parse", "--verify", &reference]) else {
        return;
    };
    let tip = tip.trim().to_owned();
    let range = format!("{base}..{tip}");
    report.commits = git(repository, &["rev-list", "--reverse", &range])
        .map(|out| out.lines().map(str::to_owned).collect())
        .unwrap_or_default();
    report.committed = !report.commits.is_empty();
    report.merged =
        report.committed && git(repository, &["merge-base", "--is-ancestor", &tip, "HEAD"]).is_ok();
    report.pushed = report.committed
        && git(repository, &["branch", "-r", "--contains", &tip])
            .is_ok_and(|out| !out.trim().is_empty());
}

/// The task's final report as the evidence stands now, after refreshing its
/// verifications.
pub fn final_report(store: &Store, task_id: &str, now: u64) -> Result<FinalReport, TaskError> {
    let live = live_verifications(store, task_id, now)?;
    let tasks = store.tasks()?;
    let task = tasks
        .task(task_id)?
        .ok_or_else(|| TaskError::NotFound(format!("task:{task_id}")))?;
    let workspaces = tasks.workspaces(task_id)?;
    let mut assignments = Vec::new();
    for a in tasks.assignments(task_id)? {
        if a.status == AssignmentState::Cancelled {
            continue;
        }
        let workspace = workspaces
            .iter()
            .find(|w| w.assignment_id.as_deref() == Some(a.id.as_str()));
        let verifications: Vec<String> = live
            .iter()
            .filter(|v| v.passed && v.assignment_id.as_deref() == Some(a.id.as_str()))
            .map(|v| v.id.clone())
            .collect();
        let mut report = AssignmentReport {
            implemented: a.status == AssignmentState::Accepted && !verifications.is_empty(),
            id: a.id,
            status: a.status,
            mode: workspace.map(|w| w.mode),
            path: workspace.map(|w| w.path.clone()),
            branch: workspace.and_then(|w| w.branch.clone()),
            base_commit: workspace.and_then(|w| w.base_commit.clone()),
            commits: Vec::new(),
            uncommitted: false,
            committed: false,
            merged: false,
            pushed: false,
            deployed: false,
            verifications,
        };
        if let Some(w) = workspace.filter(|w| w.mode == WorkspaceMode::Worktree) {
            branch_facts(w, &mut report);
            report.uncommitted = git(Path::new(&w.path), &["status", "--porcelain"])
                .is_ok_and(|out| !out.trim().is_empty());
        }
        assignments.push(report);
    }
    let criteria = task
        .criteria
        .iter()
        .map(|c| {
            let latest = live
                .iter()
                .rev()
                .find(|v| v.criterion_id.as_deref() == Some(c.id.as_str()));
            CriterionReport {
                id: c.id.clone(),
                required: c.required,
                satisfied: latest.is_some_and(|v| v.passed),
                verification: latest.map(|v| v.id.clone()),
                checkpoint: latest.map(|v| v.checkpoint_commit.clone()),
            }
        })
        .collect();
    let branched: Vec<&AssignmentReport> =
        assignments.iter().filter(|a| a.branch.is_some()).collect();
    let all = |flag: fn(&AssignmentReport) -> bool| {
        !branched.is_empty() && branched.iter().all(|a| flag(a))
    };
    Ok(FinalReport {
        task_id: task_id.into(),
        implemented: !assignments.is_empty() && assignments.iter().all(|a| a.implemented),
        committed: all(|a| a.committed),
        merged: all(|a| a.merged),
        pushed: all(|a| a.pushed),
        deployed: false,
        assignments,
        criteria,
        generated_at: now,
    })
}

/// Completes the task: refreshes its verifications, then in one
/// transaction persists the final report and moves the stage Verifying →
/// Completed and the lifecycle Active → Completed. Any unmet precondition
/// rolls everything back, report included.
pub fn complete(
    store: &Store,
    task_id: &str,
    requester: Requester,
    now: u64,
) -> Result<FinalReport, TaskError> {
    let report = final_report(store, task_id, now)?;
    let tasks = store.tasks()?;
    let tx = tasks.begin()?;
    let event = NewEvent {
        id: format!("final_report:{task_id}"),
        task_id: task_id.into(),
        assignment_id: None,
        attempt_id: None,
        kind: FINAL_REPORT.into(),
        payload: serde_json::to_value(&report)?,
    };
    append(&tx, &event, now)?;
    let task = Subject::Task(task_id);
    let completed = State::Stage(TaskStage::Completed);
    apply(&tx, task, completed, requester, tasks.revision(task)?, now)?;
    let done = State::Lifecycle(TaskLifecycle::Completed);
    apply(&tx, task, done, requester, tasks.revision(task)?, now)?;
    tx.commit()?;
    Ok(report)
}
