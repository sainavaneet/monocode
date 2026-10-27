//! The final report and task completion.
//!
//! The report states implemented, committed, merged, pushed and deployed
//! from evidence only, never from a worker's words. Git facts about the
//! branches ([`ReportFacts`]) are gathered first, without the store; the
//! report is then built from the store inside the completing transaction,
//! so it shows exactly the verifications the precondition accepted.
use super::completion::{criterion_satisfied, refresh_verifications};
use super::events::{append, NewEvent};
use super::states::apply;
use super::{
    AssignmentState, Requester, State, Subject, TaskError, TaskLifecycle, TaskStage, TaskStore,
    Verification, Workspace,
};
use crate::git::git;
use crate::orchestration::WorkspaceMode;
use crate::store::Store;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// The record kind of a persisted final report; its event id is
/// `final_report:<task>`.
pub const FINAL_REPORT: &str = "task.final_report";

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
    /// The task's final integration record, when several worktrees were
    /// integrated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration: Option<serde_json::Value>,
}

/// What Git shows about one worktree's branch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BranchFacts {
    pub commits: Vec<String>,
    pub merged: bool,
    pub pushed: bool,
    pub uncommitted: bool,
}

/// Git facts per workspace id, gathered without the store.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReportFacts(HashMap<String, BranchFacts>);

impl ReportFacts {
    /// Reads each worktree's branch and status. Runs Git only.
    pub fn gather(workspaces: &[Workspace]) -> Self {
        let facts = workspaces
            .iter()
            .filter(|w| w.mode == WorkspaceMode::Worktree)
            .map(|w| (w.id.clone(), branch_facts(w)))
            .collect();
        Self(facts)
    }
}

fn branch_facts(workspace: &Workspace) -> BranchFacts {
    let uncommitted = git(Path::new(&workspace.path), &["status", "--porcelain"])
        .is_ok_and(|out| !out.trim().is_empty());
    let mut facts = BranchFacts {
        uncommitted,
        ..BranchFacts::default()
    };
    let (Some(branch), Some(base)) = (&workspace.branch, &workspace.base_commit) else {
        return facts;
    };
    let repository = Path::new(&workspace.repository);
    let reference = format!("refs/heads/{branch}");
    let Ok(tip) = git(repository, &["rev-parse", "--verify", &reference]) else {
        return facts;
    };
    let tip = tip.trim().to_owned();
    let range = format!("{base}..{tip}");
    facts.commits = git(repository, &["rev-list", "--reverse", &range])
        .map(|out| out.lines().map(str::to_owned).collect())
        .unwrap_or_default();
    let committed = !facts.commits.is_empty();
    facts.merged =
        committed && git(repository, &["merge-base", "--is-ancestor", &tip, "HEAD"]).is_ok();
    facts.pushed = committed
        && git(repository, &["branch", "-r", "--contains", &tip])
            .is_ok_and(|out| !out.trim().is_empty());
    facts
}

/// The task's final report as the evidence stands now, after refreshing its
/// verifications.
pub fn final_report(store: &Store, task_id: &str, now: u64) -> Result<FinalReport, TaskError> {
    refresh_verifications(store, task_id, now)?;
    let tasks = store.tasks()?;
    let facts = ReportFacts::gather(&tasks.workspaces(task_id)?);
    build_report(&tasks, task_id, &facts, now)
}

/// The report from the store's live verifications and `facts`; no Git.
pub(super) fn build_report(
    tasks: &TaskStore<'_>,
    task_id: &str,
    facts: &ReportFacts,
    now: u64,
) -> Result<FinalReport, TaskError> {
    let live: Vec<Verification> = tasks
        .verifications(task_id)?
        .into_iter()
        .filter(|v| !v.invalidated)
        .collect();
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
        let found = workspace
            .filter(|w| w.mode == WorkspaceMode::Worktree)
            .and_then(|w| facts.0.get(&w.id))
            .cloned()
            .unwrap_or_default();
        let committed = workspace.is_some_and(|w| w.branch.is_some()) && !found.commits.is_empty();
        assignments.push(AssignmentReport {
            implemented: a.status == AssignmentState::Accepted && !verifications.is_empty(),
            id: a.id,
            status: a.status,
            mode: workspace.map(|w| w.mode),
            path: workspace.map(|w| w.path.clone()),
            branch: workspace.and_then(|w| w.branch.clone()),
            base_commit: workspace.and_then(|w| w.base_commit.clone()),
            committed,
            merged: committed && found.merged,
            pushed: committed && found.pushed,
            uncommitted: found.uncommitted,
            commits: found.commits,
            deployed: false,
            verifications,
        });
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
                satisfied: criterion_satisfied(c, &live),
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
        integration: tasks.integration(task_id)?,
    })
}

/// Completes the task: refreshes its verifications and reads its branches,
/// then completes it with [`complete_with`].
pub fn complete(
    store: &Store,
    task_id: &str,
    requester: Requester,
    now: u64,
) -> Result<FinalReport, TaskError> {
    refresh_verifications(store, task_id, now)?;
    let tasks = store.tasks()?;
    let facts = ReportFacts::gather(&tasks.workspaces(task_id)?);
    complete_with(&tasks, task_id, requester, &facts, now)
}

/// In one transaction: builds the final report from the store and `facts`,
/// persists it, and moves the stage Verifying → Completed and the lifecycle
/// Active → Completed. Any unmet precondition rolls everything back, report
/// included. No Git runs here.
pub(super) fn complete_with(
    tasks: &TaskStore<'_>,
    task_id: &str,
    requester: Requester,
    facts: &ReportFacts,
    now: u64,
) -> Result<FinalReport, TaskError> {
    let tx = tasks.begin()?;
    let report = build_report(tasks, task_id, facts, now)?;
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
