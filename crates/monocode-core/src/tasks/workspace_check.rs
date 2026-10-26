//! Whether recorded workspaces are still usable as recorded, and the
//! explicit recovery condition when one is not. The Git checks need no
//! store, so callers can run them without holding the store lock.
use super::events::{append, NewEvent};
use super::guard::task_or_missing;
use super::notices::notify;
use super::recovery;
use super::states::apply;
use super::{
    AssignmentState, Requester, State, Subject, Task, TaskError, TaskLifecycle, TaskStore,
    Workspace,
};
use crate::orchestration::WorkspaceMode;
use rusqlite::Connection;
use serde_json::json;
use std::path::Path;

/// A workspace that cannot be used as recorded, with why.
pub type Problem = (Workspace, String);

/// Why a recorded workspace can no longer be used as recorded, if it cannot:
/// the directory is gone, it is no longer a worktree of its repository, its
/// branch changed, or its HEAD no longer descends from the recorded base.
pub fn problem(workspace: &Workspace) -> Option<String> {
    let path = Path::new(&workspace.path);
    if !path.is_dir() {
        return Some(format!("{} no longer exists", workspace.path));
    }
    if workspace.mode == WorkspaceMode::Checkout {
        return None;
    }
    let canonical = |p: &str| std::fs::canonicalize(p).ok();
    let mine = canonical(&workspace.path);
    let trees = match crate::git::worktrees(Path::new(&workspace.repository)) {
        Ok(trees) => trees,
        Err(error) => return Some(format!("its repository cannot be read ({error})")),
    };
    let Some(tree) = trees
        .iter()
        .find(|t| mine.is_some() && canonical(&t.path) == mine)
    else {
        return Some(format!(
            "{} is no longer a worktree of {}",
            workspace.path, workspace.repository
        ));
    };
    if let Some(branch) = &workspace.branch {
        if tree.branch.as_deref() != Some(branch.as_str()) {
            return Some(format!(
                "{} is on {} instead of its branch {branch}",
                workspace.path,
                tree.branch.as_deref().unwrap_or("a detached HEAD")
            ));
        }
    }
    if let Some(base) = &workspace.base_commit {
        let descends =
            crate::git::git(path, &["merge-base", "--is-ancestor", base, "HEAD"]).is_ok();
        if !descends {
            return Some(format!(
                "{}'s HEAD no longer descends from its base {base}",
                workspace.path
            ));
        }
    }
    None
}

/// The recorded workspaces of the task's unfinished assignments (not
/// Accepted, whose worktree may be cleaned up, nor Cancelled).
pub(super) fn to_check(tasks: &TaskStore<'_>, task: &str) -> Result<Vec<Workspace>, TaskError> {
    let unfinished: Vec<String> = tasks
        .assignments(task)?
        .into_iter()
        .filter(|a| {
            !matches!(
                a.status,
                AssignmentState::Accepted | AssignmentState::Cancelled
            )
        })
        .map(|a| a.id)
        .collect();
    Ok(tasks
        .workspaces(task)?
        .into_iter()
        .filter(|w| {
            w.assignment_id
                .as_ref()
                .is_none_or(|a| unfinished.contains(a))
        })
        .collect())
}

/// Checks the workspaces with Git; no store needed.
pub fn check(workspaces: &[Workspace]) -> Vec<Problem> {
    workspaces
        .iter()
        .filter_map(|w| problem(w).map(|why| (w.clone(), why)))
        .collect()
}

/// Workspaces of the task's unfinished assignments that are missing or
/// moved, with why.
pub(super) fn verify(tasks: &TaskStore<'_>, task: &Task) -> Result<Vec<Problem>, TaskError> {
    Ok(check(&to_check(tasks, &task.id)?))
}

/// The explicit recovery condition, inside the caller's transaction: one
/// event per workspace, the task Blocked with the reason (a paused task
/// keeps its pause; resuming checks again), and one Mono notice. Returns
/// the reason.
pub(super) fn record_condition(
    conn: &Connection,
    task: &str,
    problems: &[Problem],
    now: u64,
) -> Result<String, TaskError> {
    let current = task_or_missing(&TaskStore { conn }, task)?;
    let lines: Vec<String> = problems
        .iter()
        .map(|(w, why)| {
            format!(
                "assignment {}: {why}",
                w.assignment_id.as_deref().unwrap_or("?")
            )
        })
        .collect();
    let reason = format!(
        "A recorded workspace is missing or moved ({}). Work is never restarted in a fresh workspace on its own: restore the worktree, or decide with Mono how to continue.",
        lines.join("; ")
    );
    for (workspace, why) in problems {
        let event = NewEvent {
            id: format!("workspace_missing:{}@{}", workspace.id, current.revision),
            task_id: task.into(),
            assignment_id: workspace.assignment_id.clone(),
            attempt_id: None,
            kind: "recovery.workspace_missing".into(),
            payload: json!({"workspaceId": workspace.id, "path": workspace.path,
                "branch": workspace.branch, "baseCommit": workspace.base_commit, "reason": why}),
        };
        append(conn, &event, now)?;
    }
    recovery::block(conn, task, &reason, now)?;
    notify(conn, task, None, "recovery.workspace_missing", &reason, now)?;
    Ok(reason)
}

/// What unblocking a task must check with Git, read under the store, so
/// the Git checks ([`UnblockCheck::problems`]) run without the store lock.
#[derive(Clone, Debug)]
pub struct UnblockCheck {
    task_id: String,
    workspaces: Vec<Workspace>,
}

impl UnblockCheck {
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    /// The recorded workspaces that are missing or moved. No store access.
    pub fn problems(&self) -> Vec<Problem> {
        check(&self.workspaces)
    }
}

impl TaskStore<'_> {
    /// Step one of unblocking: refuses a task that is not Blocked and
    /// collects the workspaces to check.
    pub fn unblock_check(&self, task_id: &str) -> Result<UnblockCheck, TaskError> {
        let task = task_or_missing(self, task_id)?;
        if task.lifecycle != TaskLifecycle::Blocked {
            return Err(TaskError::Invalid(format!(
                "Task {task_id} is not blocked (it is {}).",
                task.lifecycle.as_str()
            )));
        }
        Ok(UnblockCheck {
            task_id: task_id.into(),
            workspaces: to_check(self, task_id)?,
        })
    }

    /// Step three: returns the Blocked task to Active for `requester`, in
    /// one transaction through the table (which clears the blocked reason),
    /// unless the checked workspaces are missing or moved, or changed since
    /// they were collected, or the task is no longer at `expected_revision`.
    /// Returns the task's new revision.
    pub fn unblock_checked(
        &self,
        check: &UnblockCheck,
        problems: &[Problem],
        expected_revision: u64,
        requester: Requester,
        now: u64,
    ) -> Result<u64, TaskError> {
        let task_id = check.task_id.as_str();
        if !problems.is_empty() {
            let lines: Vec<String> = problems
                .iter()
                .map(|(w, why)| {
                    format!(
                        "assignment {}: {why}",
                        w.assignment_id.as_deref().unwrap_or("integration")
                    )
                })
                .collect();
            return Err(TaskError::Invalid(format!(
                "Task {task_id} stays blocked: a recorded workspace is missing or moved ({}). Restore the worktree, or decide with Mono how to continue.",
                lines.join("; ")
            )));
        }
        let tx = self.begin()?;
        let inside = TaskStore { conn: &tx };
        let lifecycle = task_or_missing(&inside, task_id)?.lifecycle;
        if lifecycle != TaskLifecycle::Blocked {
            return Err(TaskError::Invalid(format!(
                "Task {task_id} is not blocked (it is {}).",
                lifecycle.as_str()
            )));
        }
        if to_check(&inside, task_id)? != check.workspaces {
            return Err(TaskError::Invalid(format!(
                "Task {task_id}'s workspaces changed while they were checked; unblock it again."
            )));
        }
        let active = State::Lifecycle(TaskLifecycle::Active);
        let revision = apply(
            &tx,
            Subject::Task(task_id),
            active,
            requester,
            expected_revision,
            now,
        )?;
        tx.commit()?;
        Ok(revision)
    }

    /// All three steps of unblocking while holding this store throughout
    /// (Git included). The app runs [`Self::unblock_check`], the Git check
    /// and [`Self::unblock_checked`] with the store lock released between.
    pub fn unblock(
        &self,
        task_id: &str,
        expected_revision: u64,
        requester: Requester,
        now: u64,
    ) -> Result<u64, TaskError> {
        let check = self.unblock_check(task_id)?;
        let problems = check.problems();
        self.unblock_checked(&check, &problems, expected_revision, requester, now)
    }
}
