//! Where an assignment works, decided and recorded before its worker starts.
//!
//! A substantial assignment (one with a file scope to change) gets its own
//! branch and worktree through the same path as `worktrees.create`. An
//! investigation (`workspace: checkout`) reads the existing checkout. Dirty changes in the checkout are
//! never copied: a worktree starts from the committed `HEAD`, the dirty files
//! stay where they are, and the caller is told so it can say so. Nothing here
//! deletes a worktree.
use super::{Assignment, Task, TaskStore, Workspace};
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

/// Returns the assignment's recorded workspace, or creates and records one.
/// Errors explain why no workspace could be prepared; nothing is recorded then.
pub fn prepare(
    tasks: &TaskStore<'_>,
    task: &Task,
    assignment: &Assignment,
    now: u64,
) -> Result<Prepared, String> {
    let project = Path::new(&task.project);
    let mode = mode_for(assignment);
    let dirty_base = mode == WorkspaceMode::Worktree && dirty(project);
    let id = workspace_id(&task.id, &assignment.id);
    let recorded = tasks.workspaces(&task.id).map_err(|e| e.to_string())?;
    if let Some(workspace) = recorded.into_iter().find(|w| w.id == id) {
        return Ok(Prepared {
            workspace,
            dirty_base,
        });
    }
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
                format!(
                    "{} is not a Git repository, so assignment {} cannot have its own worktree.",
                    task.project, assignment.id
                )
            })?;
            let base = head(project)
                .ok_or("The repository has no commit to base a worktree on.".to_owned())?;
            let branch = branch_for(&task.id, &assignment.id);
            let tree = worktree(project, &branch, &base)?;
            (repository, tree.path, Some(branch), Some(tree.head))
        }
    };
    let workspace = tasks
        .save_workspace(&Workspace {
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
        })
        .map_err(|e| e.to_string())?;
    Ok(Prepared {
        workspace,
        dirty_base,
    })
}

/// Creates the assignment's worktree from `base`, or adopts the one a crash
/// left behind before it was recorded (the branch name is ours alone).
fn worktree(project: &Path, branch: &str, base: &str) -> Result<git::Worktree, String> {
    let existing = git::worktrees(project)?
        .into_iter()
        .find(|tree| tree.branch.as_deref() == Some(branch) && !tree.missing);
    if let Some(tree) = existing {
        return Ok(tree);
    }
    let mut tree = git::create_project_worktree(project, branch, Some(base), false)?;
    tree.head = base.to_owned();
    Ok(tree)
}
