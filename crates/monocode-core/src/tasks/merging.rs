//! The Git half of integration (see [`super::integration`]): preparing the
//! task's integration worktree and merging each assignment's branch onto it
//! in order, aborting at the first conflict so the worktree stays clean.
//! Nothing here reads or writes the store, so the app runs it without the
//! store lock; nothing here pushes or deletes a worktree.
use super::{Assignment, Task, Workspace};
use crate::git::git;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// How an integration pass ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Outcome {
    /// Every branch is merged; the criteria are checked next.
    Clean,
    /// Merging `branch` conflicted in `files`; the merge was aborted.
    Conflict {
        assignment: String,
        branch: String,
        files: Vec<String>,
    },
    /// The assignment's worktree has changes no commit holds.
    Uncommitted {
        assignment: String,
        files: Vec<String>,
    },
    /// Git refused for another reason; retried, then Mono decides.
    Failed { reason: String },
}

/// One assignment's branch as integration found it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Merge {
    pub assignment: String,
    pub branch: String,
    /// The branch tip integrated.
    pub tip: String,
    /// False when the integration branch already contained the tip.
    pub merged: bool,
    /// The merge commit, when one was made.
    pub commit: Option<String>,
}

/// One integration pass, persisted as a `task.integration` event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationRecord {
    pub task_id: String,
    pub repository: Option<String>,
    pub branch: String,
    /// The integration worktree.
    pub path: Option<String>,
    /// The recorded base the work is integrated onto.
    pub base: String,
    /// The user's branch and its tip at integration time.
    pub base_branch: Option<String>,
    pub upstream_head: Option<String>,
    /// The user's branch moved past the recorded base.
    pub base_moved: bool,
    /// Assignments in the order they are merged.
    pub order: Vec<String>,
    pub merges: Vec<Merge>,
    /// The integration branch's tip after the pass.
    pub head: Option<String>,
    pub outcome: Outcome,
}

#[derive(Clone, Debug)]
pub(super) struct Step {
    pub(super) assignment: String,
    pub(super) branch: String,
    pub(super) worktree: String,
}

/// A task ready to integrate, read from the store.
pub struct IntegrationJob {
    pub(super) task: Task,
    pub(super) branch: String,
    /// The integration worktree, once recorded.
    pub(super) recorded: Option<Workspace>,
    pub(super) base: String,
    pub(super) steps: Vec<Step>,
}

fn trimmed(result: Result<String, String>) -> Option<String> {
    result
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn rev(cwd: &Path, reference: &str) -> Option<String> {
    trimmed(git(cwd, &["rev-parse", "--verify", "-q", reference]))
}

/// Changed and untracked paths in a worktree.
fn dirty_files(tree: &Path) -> Result<Vec<String>, String> {
    let status = git(tree, &["status", "--porcelain", "--untracked-files=all"])?;
    Ok(status
        .lines()
        .filter_map(|line| line.get(3..))
        .map(|path| path.rsplit(" -> ").next().unwrap_or(path).to_owned())
        .collect())
}

impl IntegrationJob {
    pub fn task_id(&self) -> &str {
        &self.task.id
    }

    /// Prepares the integration worktree and merges every branch in order.
    /// Touches no store, so it may run without the store lock.
    pub fn run(&self) -> IntegrationRecord {
        let project = Path::new(&self.task.project);
        let upstream_head = rev(project, "HEAD");
        let mut record = IntegrationRecord {
            task_id: self.task.id.clone(),
            repository: trimmed(git(project, &["rev-parse", "--show-toplevel"])),
            branch: self.branch.clone(),
            path: None,
            base: self.base.clone(),
            base_branch: trimmed(git(project, &["symbolic-ref", "--short", "-q", "HEAD"])),
            base_moved: upstream_head.as_deref() != Some(self.base.as_str()),
            upstream_head,
            order: self.steps.iter().map(|s| s.assignment.clone()).collect(),
            merges: Vec::new(),
            head: None,
            outcome: Outcome::Clean,
        };
        let path = match self.worktree() {
            Ok(path) => path,
            Err(reason) => {
                record.outcome = Outcome::Failed { reason };
                return record;
            }
        };
        let tree = Path::new(&path);
        record.outcome = self.merge_all(tree, &mut record.merges);
        record.head = rev(tree, "HEAD");
        record.path = Some(path);
        record
    }

    /// The recorded integration worktree as recorded, or a new (or adopted)
    /// one on the integration branch from the base.
    fn worktree(&self) -> Result<String, String> {
        if let Some(recorded) = &self.recorded {
            return match super::workspace_check::problem(recorded) {
                None => Ok(recorded.path.clone()),
                Some(why) => Err(format!(
                    "The integration worktree cannot be used as recorded: {why}."
                )),
            };
        }
        let project = Path::new(&self.task.project);
        super::workspaces::worktree(project, &self.branch, &self.base).map(|(path, _)| path)
    }

    fn merge_all(&self, tree: &Path, merges: &mut Vec<Merge>) -> Outcome {
        let failed = |reason: String| Outcome::Failed { reason };
        match dirty_files(tree) {
            Ok(files) if !files.is_empty() => {
                return failed(format!(
                    "The integration worktree {} has uncommitted changes ({}); integration leaves it untouched until they are dealt with.",
                    tree.display(),
                    files.join(", ")
                ))
            }
            Ok(_) => {}
            Err(error) => return failed(error),
        }
        for step in &self.steps {
            match dirty_files(Path::new(&step.worktree)) {
                Ok(files) if !files.is_empty() => {
                    return Outcome::Uncommitted {
                        assignment: step.assignment.clone(),
                        files,
                    }
                }
                Ok(_) => {}
                Err(error) => return failed(error),
            }
            let reference = format!("refs/heads/{}^{{commit}}", step.branch);
            let Some(tip) = rev(tree, &reference) else {
                return failed(format!("The branch {} cannot be read.", step.branch));
            };
            let mut merge = Merge {
                assignment: step.assignment.clone(),
                branch: step.branch.clone(),
                tip: tip.clone(),
                merged: false,
                commit: None,
            };
            if git(tree, &["merge-base", "--is-ancestor", &tip, "HEAD"]).is_ok() {
                merges.push(merge);
                continue;
            }
            let message = format!(
                "Integrate {} ({}) for task {}",
                step.assignment, step.branch, self.task.id
            );
            let merged = git(
                tree,
                &[
                    "merge",
                    "--no-ff",
                    "--no-edit",
                    "-m",
                    &message,
                    &step.branch,
                ],
            );
            if let Err(error) = merged {
                return abort(tree, step, &error);
            }
            merge.merged = true;
            merge.commit = rev(tree, "HEAD");
            merges.push(merge);
        }
        Outcome::Clean
    }
}

/// Aborts a failed merge so the integration worktree is clean again, and
/// says whether it conflicted (and where) or failed otherwise.
fn abort(tree: &Path, step: &Step, error: &str) -> Outcome {
    let files: Vec<String> = git(tree, &["diff", "--name-only", "--diff-filter=U"])
        .map(|out| out.lines().map(str::to_owned).collect())
        .unwrap_or_default();
    let aborted = git(tree, &["merge", "--abort"]);
    if files.is_empty() {
        return Outcome::Failed {
            reason: format!("Merging {} failed: {error}", step.branch),
        };
    }
    if let Err(abort) = aborted {
        return Outcome::Failed {
            reason: format!(
                "Merging {} conflicted in {} and the merge could not be aborted: {abort}",
                step.branch,
                files.join(", ")
            ),
        };
    }
    Outcome::Conflict {
        assignment: step.assignment.clone(),
        branch: step.branch.clone(),
        files,
    }
}

/// Assignments in dependency order; plan order among independent ones.
pub(super) fn dependency_order(
    mut pending: Vec<(Assignment, Workspace)>,
) -> Vec<(Assignment, Workspace)> {
    let mut placed = Vec::new();
    while !pending.is_empty() {
        let ids: Vec<String> = pending.iter().map(|(a, _)| a.id.clone()).collect();
        let next = pending
            .iter()
            .position(|(a, _)| a.depends_on.iter().all(|d| !ids.contains(d)))
            .unwrap_or(0);
        placed.push(pending.remove(next));
    }
    placed
}
