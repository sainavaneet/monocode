//! Where an assignment works, decided and recorded before its worker starts.
//!
//! A substantial assignment (one with a file scope to change) gets its own
//! branch and worktree through the same path as `worktrees.create`. An
//! investigation (`workspace: checkout`) reads the existing checkout. Dirty changes in the checkout are
//! never copied: a worktree starts from the committed `HEAD`, the dirty files
//! stay where they are, and the caller is told so it can say so. Nothing here
//! deletes a worktree.
use super::{Assignment, Task, TaskError, TaskStore, Workspace};
use crate::git::{self, git};
use crate::orchestration::WorkspaceMode;
use std::path::Path;

/// A recorded workspace and whether the checkout it came from had
/// uncommitted changes the worker will not see.
pub struct Prepared {
    pub workspace: Workspace,
    pub dirty_base: bool,
}

/// The assignment's own choice (a plan marks investigation `checkout`),
/// otherwise a worktree whenever it has a file scope to change.
pub fn mode_for(assignment: &Assignment) -> WorkspaceMode {
    assignment
        .workspace
        .unwrap_or(if assignment.files.is_empty() {
            WorkspaceMode::Checkout
        } else {
            WorkspaceMode::Worktree
        })
}

/// One stable branch per assignment, so a retry finds its earlier work.
pub fn branch_for(task_id: &str, assignment_id: &str) -> String {
    let slug = |text: &str| -> String {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    };
    format!("mono/{}/{}", slug(task_id), slug(assignment_id))
}

/// The stable record id of an assignment's workspace.
fn workspace_id(task_id: &str, assignment_id: &str) -> String {
    format!("{task_id}/{assignment_id}")
}

/// The owner recorded on every workspace a task's coordinator creates.
pub fn owner_of(task_id: &str) -> String {
    format!("task:{task_id}")
}

fn head(project: &Path) -> Option<String> {
    git(project, &["rev-parse", "--verify", "HEAD^{commit}"])
        .ok()
        .map(|s| s.trim().to_owned())
}

/// Uncommitted or untracked changes in the checkout.
fn dirty(project: &Path) -> bool {
    git(project, &["status", "--porcelain"]).is_ok_and(|s| !s.trim().is_empty())
}

/// Why no workspace was prepared.
#[derive(Debug)]
pub enum PrepareError {
    /// Git or the project refused: the task cannot proceed as planned.
    Workspace(String),
    /// The task store failed; a later tick retries.
    Storage(TaskError),
}

impl From<TaskError> for PrepareError {
    fn from(error: TaskError) -> Self {
        Self::Storage(error)
    }
}

/// Returns the assignment's recorded workspace, or creates and records one.
/// `dirty_base` is only reported when the workspace is created. Nothing is
/// recorded when preparation fails.
pub fn prepare(
    tasks: &TaskStore<'_>,
    task: &Task,
    assignment: &Assignment,
    now: u64,
) -> Result<Prepared, PrepareError> {
    let id = workspace_id(&task.id, &assignment.id);
    if let Some(workspace) = tasks.workspaces(&task.id)?.into_iter().find(|w| w.id == id) {
        return Ok(Prepared {
            workspace,
            dirty_base: false,
        });
    }
    let project = Path::new(&task.project);
    let mode = mode_for(assignment);
    let dirty_base = mode == WorkspaceMode::Worktree && dirty(project);
    let repository = git(project, &["rev-parse", "--show-toplevel"])
        .map(|root| root.trim().to_owned())
        .ok();
    let (repository, path, branch, base_commit) = match mode {
        WorkspaceMode::Checkout => (
            repository.unwrap_or_else(|| task.project.clone()),
            task.project.clone(),
            git(project, &["symbolic-ref", "--short", "HEAD"])
                .ok()
                .map(|s| s.trim().to_owned()),
            head(project),
        ),
        WorkspaceMode::Worktree => {
            let repository = repository.ok_or_else(|| {
                PrepareError::Workspace(format!(
                    "{} is not a Git repository, so assignment {} cannot have its own worktree.",
                    task.project, assignment.id
                ))
            })?;
            let base = head(project).ok_or_else(|| {
                PrepareError::Workspace(
                    "The repository has no commit to base a worktree on.".into(),
                )
            })?;
            let branch = branch_for(&task.id, &assignment.id);
            let (path, base) =
                worktree(project, &branch, &base).map_err(PrepareError::Workspace)?;
            (repository, path, Some(branch), Some(base))
        }
    };
    let workspace = tasks.save_workspace(&Workspace {
        id,
        task_id: task.id.clone(),
        assignment_id: Some(assignment.id.clone()),
        mode,
        repository,
        path,
        branch,
        base_commit,
        owner: owner_of(&task.id),
        revision: 0,
        created_at: now,
    })?;
    Ok(Prepared {
        workspace,
        dirty_base,
    })
}

/// The assignment's worktree path and the commit its work starts from.
/// Creates it from `base`, or adopts what an earlier run left behind (the
/// branch name is ours alone): a worktree on the branch, or the branch alone,
/// which is then checked out in a new worktree with its commits kept.
fn worktree(project: &Path, branch: &str, base: &str) -> Result<(String, String), String> {
    let forked = |tip: &str| {
        git(project, &["merge-base", tip, base])
            .map(|s| s.trim().to_owned())
            .unwrap_or_else(|_| base.to_owned())
    };
    let existing = git::worktrees(project)?
        .into_iter()
        .find(|tree| tree.branch.as_deref() == Some(branch) && !tree.missing);
    if let Some(tree) = existing {
        return Ok((tree.path, forked(&tree.head)));
    }
    let reference = format!("refs/heads/{branch}");
    if git(project, &["rev-parse", "--verify", "--quiet", &reference]).is_ok() {
        let tree = git::create_project_worktree(project, branch, None, true)?;
        return Ok((tree.path, forked(&reference)));
    }
    let tree = git::create_project_worktree(project, branch, Some(base), false)?;
    Ok((tree.path, base.to_owned()))
}
