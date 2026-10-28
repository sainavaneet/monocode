//! Where an assignment works, decided and recorded before its worker starts.
//!
//! A substantial assignment (one with a file scope to change) gets its own
//! branch and worktree through the same path as `worktrees.create`. An
//! investigation (`workspace: checkout`) reads the existing checkout. Dirty changes in the checkout are
//! never copied: a worktree starts from the committed `HEAD`, the dirty files
//! stay where they are, and the caller is told so it can say so. Nothing here
//! deletes a worktree.
use super::autonomy::WorkspacePreference;
use super::{Assignment, Task, TaskError, TaskStore, Workspace};
use crate::git::{self, git};
use crate::orchestration::WorkspaceMode;
use std::path::{Path, PathBuf};

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

/// The mode under the task's workspace preference. A writing assignment
/// under the current-checkout preference is refused with the reason: the
/// coordinator never writes into the user's checkout.
pub fn mode_with(
    assignment: &Assignment,
    preference: WorkspacePreference,
) -> Result<WorkspaceMode, String> {
    let automatic = mode_for(assignment);
    match preference {
        WorkspacePreference::Automatic => Ok(automatic),
        WorkspacePreference::Worktree => Ok(WorkspaceMode::Worktree),
        WorkspacePreference::Checkout if automatic == WorkspaceMode::Checkout => Ok(automatic),
        WorkspacePreference::Checkout => Err(format!(
            "The Mono's policy keeps work in the current checkout, but assignment {} changes files ({}), and the coordinator never writes into your checkout. Choose automatic or dedicated worktrees in the Mono's Autonomy settings, apply the policy to the task, then unblock it.",
            assignment.id,
            assignment.files.join(", ")
        )),
    }
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

/// How long a checkout's dirty state, read by a warm-up, stands in for a
/// fresh `git status` (a full-tree scan on a large repository) in the
/// locked launch step. The state is informational: it marks the intent.
const DIRTY_CACHE_MS: u64 = 60_000;

fn dirty_cache() -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, (u64, bool)>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, (u64, bool)>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The checkout's dirty state as a recent warm-up read it, else read now.
fn dirty_at(project: &Path, now: u64) -> bool {
    if let Ok(cache) = dirty_cache().lock() {
        if let Some((at, dirty)) = cache.get(project) {
            if now.saturating_sub(*at) <= DIRTY_CACHE_MS {
                return *dirty;
            }
        }
    }
    let dirty = dirty(project);
    if let Ok(mut cache) = dirty_cache().lock() {
        cache.insert(project.to_path_buf(), (now, dirty));
    }
    dirty
}

/// A launch the coordinator is about to make, for the warm-up that runs
/// before the locked tick (`Coordinator::launch_candidates`).
#[derive(Clone, Debug, PartialEq)]
pub struct WarmUp {
    pub task: Task,
    pub assignment: Assignment,
    pub preference: WorkspacePreference,
    /// The assignment already has a recorded workspace: nothing to create.
    pub recorded: bool,
}

/// Creates the assignment's worktree ahead of the locked launch, so the
/// checkout (minutes on a large repository) never runs under the store.
/// Idempotent: `prepare` adopts the worktree it finds on the branch and
/// records it. Errors are left for `prepare` to report; this returns them
/// only for logging.
pub fn warm_up(job: &WarmUp, now: u64) -> Result<(), String> {
    if job.recorded {
        return Ok(());
    }
    let mode = mode_with(&job.assignment, job.preference)?;
    if mode != WorkspaceMode::Worktree {
        return Ok(());
    }
    let selected = repository_for(&job.task, &job.assignment)?;
    let project = selected.as_path();
    let base = match &job.assignment.base_ref {
        Some(base) => git(
            project,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{base}^{{commit}}"),
            ],
        )
        .ok()
        .map(|s| s.trim().to_owned()),
        None => head(project),
    }
    .ok_or_else(|| "The repository has no commit to base a worktree on.".to_owned())?;
    let dirty = dirty(project);
    if let Ok(mut cache) = dirty_cache().lock() {
        cache.insert(project.to_path_buf(), (now, dirty));
    }
    let branch = branch_for(&job.task.id, &job.assignment.id);
    worktree(project, &branch, &base).map(|_| ())
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

/// Resolve structured routing inside the approved task workspace. Prose is
/// never used as authority to select a different repository.
pub fn repository_for(task: &Task, assignment: &Assignment) -> Result<PathBuf, String> {
    let root = Path::new(&task.project);
    let selected = match &assignment.repository {
        Some(repository) => {
            let root = root
                .canonicalize()
                .map_err(|_| "Task workspace is unavailable")?;
            let path = Path::new(repository);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            let path = path
                .canonicalize()
                .map_err(|_| "Assignment repository is unavailable")?;
            if !path.starts_with(&root) {
                return Err("Assignment repository is outside the task workspace. Select an approved parent workspace before starting this plan.".into());
            }
            let top = git(&path, &["rev-parse", "--show-toplevel"])
                .map_err(|_| "Assignment repository is not a Git repository")?;
            if Path::new(top.trim()).canonicalize().ok().as_ref() != Some(&path) {
                return Err(
                    "Select the repository root, not a folder within another repository.".into(),
                );
            }
            path
        }
        // The same form as the explicit case, so two assignments of one
        // repository are seen as one repository (symlinked roots included).
        None => root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
    };
    // Older Mono plans used this unambiguous prefix without structured routing.
    // Block mismatches rather than launching in the lead's unrelated checkout.
    if let Some(named) = assignment
        .prompt
        .strip_prefix("Repo ")
        .and_then(|p| p.split_whitespace().next())
    {
        let named = Path::new(named.trim_end_matches('.'));
        if named.is_absolute() && named.canonicalize().ok() != selected.canonicalize().ok() {
            return Err(format!("Assignment {} names a different repository in its prompt. Set its repository field explicitly before resuming; no worker was started.", assignment.id));
        }
    }
    Ok(selected)
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
    let selected = repository_for(task, assignment).map_err(PrepareError::Workspace)?;
    let project = selected.as_path();
    // Accepted dependencies do not magically appear in another worktree.
    // Refuse stale inputs rather than asking a worker to recreate predecessor work.
    let recorded = tasks.workspaces(&task.id)?;
    let mine = recorded
        .iter()
        .find(|w| w.assignment_id.as_deref() == Some(&assignment.id));
    let input_tip = mine.and_then(|w| head(Path::new(&w.path))).or_else(|| {
        assignment
            .base_ref
            .as_ref()
            .and_then(|base| {
                git(
                    project,
                    &[
                        "rev-parse",
                        "--verify",
                        "--end-of-options",
                        &format!("{base}^{{commit}}"),
                    ],
                )
                .ok()
                .map(|s| s.trim().to_owned())
            })
            .or_else(|| head(project))
    });
    for dependency in assignment
        .depends_on
        .iter()
        .filter(|_| assignment.repository.is_some() || assignment.base_ref.is_some())
    {
        if let Some(source) = recorded
            .iter()
            .find(|w| w.assignment_id.as_deref() == Some(dependency))
        {
            if Path::new(&source.repository).canonicalize().ok() == project.canonicalize().ok() {
                if let (Some(tip), Some(input)) =
                    (head(Path::new(&source.path)), input_tip.as_deref())
                {
                    if git(project, &["merge-base", "--is-ancestor", &tip, input]).is_err() {
                        return Err(PrepareError::Workspace(format!("Assignment {} needs the accepted changes from {}. Integrate its branch into a reviewed base_ref before resuming; the worker was not launched with stale inputs.", assignment.id, dependency)));
                    }
                }
            }
        }
    }
    let id = workspace_id(&task.id, &assignment.id);
    if let Some(workspace) = tasks.workspaces(&task.id)?.into_iter().find(|w| w.id == id) {
        let actual = Path::new(&workspace.repository).canonicalize().ok();
        let expected = git(project, &["rev-parse", "--show-toplevel"])
            .ok()
            .and_then(|p| Path::new(p.trim()).canonicalize().ok())
            .or_else(|| project.canonicalize().ok());
        if actual != expected {
            return Err(PrepareError::Workspace("Saved workspace belongs to another repository. Preserve its work and repair the assignment before resuming.".into()));
        }
        if let Some(base) = &assignment.base_ref {
            let expected_base = git(
                project,
                &[
                    "rev-parse",
                    "--verify",
                    "--end-of-options",
                    &format!("{base}^{{commit}}"),
                ],
            )
            .map_err(PrepareError::Workspace)?;
            if workspace.base_commit.as_deref() != Some(expected_base.trim()) {
                return Err(PrepareError::Workspace(
                    "Saved workspace has a different base commit. Review it before resuming."
                        .into(),
                ));
            }
        }
        return Ok(Prepared {
            workspace,
            dirty_base: false,
        });
    }
    let preference = super::policy::of(tasks, &task.id)?
        .map(|policy| policy.autonomy.workspace)
        .unwrap_or_default();
    let mode = mode_with(assignment, preference).map_err(PrepareError::Workspace)?;
    if mode == WorkspaceMode::Checkout {
        if let Some(base) = &assignment.base_ref {
            let expected = git(
                project,
                &[
                    "rev-parse",
                    "--verify",
                    "--end-of-options",
                    &format!("{base}^{{commit}}"),
                ],
            )
            .map_err(PrepareError::Workspace)?;
            if head(project).as_deref() != Some(expected.trim()) {
                return Err(PrepareError::Workspace(
                    "The checkout is not at the requested base_ref. Select a dedicated worktree to inspect that revision without changing the user's checkout.".into(),
                ));
            }
        }
    }
    let dirty_base = mode == WorkspaceMode::Worktree && dirty_at(project, now);
    let repository = git(project, &["rev-parse", "--show-toplevel"])
        .map(|root| root.trim().to_owned())
        .ok();
    let (repository, path, branch, base_commit) = match mode {
        WorkspaceMode::Checkout => (
            repository.unwrap_or_else(|| selected.to_string_lossy().into_owned()),
            selected.to_string_lossy().into_owned(),
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
            let base = if let Some(base) = &assignment.base_ref {
                git(
                    project,
                    &[
                        "rev-parse",
                        "--verify",
                        "--end-of-options",
                        &format!("{base}^{{commit}}"),
                    ],
                )
                .ok()
                .map(|s| s.trim().to_owned())
            } else {
                head(project)
            }
            .ok_or_else(|| {
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
pub(super) fn worktree(
    project: &Path,
    branch: &str,
    base: &str,
) -> Result<(String, String), String> {
    let forked = |tip: &str| {
        git(project, &["merge-base", tip, base])
            .map(|s| s.trim().to_owned())
            .unwrap_or_else(|_| base.to_owned())
    };
    let existing = git::worktrees(project)?
        .into_iter()
        // Never the user's main checkout, even on our branch.
        .find(|tree| tree.branch.as_deref() == Some(branch) && !tree.missing && !tree.main);
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
