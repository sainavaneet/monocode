//! Restart reconciliation: what a coordinator does with a task it takes over
//! from a holder that is gone (this app before it quit, or another process).
//!
//! Sequence, per task, once this holder owns its lease: every attempt that
//! was Starting with a session, Running or Waiting is Interrupted, because
//! quitting kills the worker process groups (`process::shutdown_all`); it
//! continues through task-7's recovery path on the same lineage when the
//! task's policy allows restart continuation, otherwise it is held and the
//! task waits with the reason. Recorded workspaces must still exist on their
//! branch and base; a missing or moved one is an explicit recovery condition
//! (event, task Blocked with the reason, Mono told), never a fresh start.
//! Waiting decisions are restored to Mono's queue (idempotently), a
//! task-wide open question keeps the task Waiting, and paused tasks stay
//! paused: nothing here resumes them.
use super::coordinator::{enabled, lease_resource, Coordinator, Policy};
use super::decisions::queue_for_mono;
use super::events::{append, NewEvent};
use super::lifecycle::{notify, sync_waiting, wait_for};
use super::park::park;
use super::recovery::{self, HELD, SCHEDULED};
use super::{Attempt, AttemptState, Task, TaskError, TaskLifecycle, TaskStore, Workspace};
use crate::orchestration::WorkspaceMode;
use crate::store::Store;
use rusqlite::OptionalExtension;
use serde_json::json;
use std::path::Path;

/// The recovery category of an attempt interrupted by a restart.
pub const RESTART: &str = "restart";
const RESTART_REASON: &str =
    "The app quit while this worker ran, and quitting stops the worker's process.";

/// What startup reconciliation did, by task, attempt, workspace or decision.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Startup {
    /// False when the flag is off and nothing was read or written.
    pub enabled: bool,
    pub reconciled: Vec<String>,
    /// Attempts moved to Interrupted that recovery continues.
    pub interrupted: Vec<String>,
    /// Interrupted attempts held because the policy does not continue them.
    pub held: Vec<String>,
    /// Workspaces that are missing or moved.
    pub missing: Vec<String>,
    /// Open decisions put back in their Mono's queue.
    pub restored: Vec<String>,
    /// Tasks another holder (such as the desktop app) still leases; the
    /// coordinator's ticks retry them.
    pub leased_elsewhere: Vec<String>,
    pub errors: Vec<String>,
}

fn takeover_event(task: &str, holder: &str) -> String {
    format!("takeover:{task}:{holder}")
}

fn restarted_event(task: &str, holder: &str) -> String {
    format!("restart:{task}:{holder}")
}

fn exists(tasks: &TaskStore<'_>, event: &str) -> Result<bool, TaskError> {
    Ok(super::worker::payload_of(tasks.conn, event)?.is_some())
}

/// Every task that is neither cancelled nor completed, in creation order.
fn unfinished(tasks: &TaskStore<'_>) -> Result<Vec<String>, TaskError> {
    let mut statement = tasks.conn.prepare(
        "SELECT id FROM native_tasks WHERE lifecycle NOT IN ('cancelled','completed') ORDER BY rowid",
    )?;
    let ids = statement.query_map([], |row| row.get(0))?;
    Ok(ids.collect::<Result<_, _>>()?)
}

/// Whether the attempt's worker process was running when its holder quit.
fn was_running(tasks: &TaskStore<'_>, attempt: &Attempt) -> Result<bool, TaskError> {
    let running = match attempt.status {
        AttemptState::Running | AttemptState::Waiting => true,
        AttemptState::Starting => attempt.session_id.is_some(),
        _ => false,
    };
    Ok(running && tasks.claim(&attempt.id)?.is_none())
}

impl<P: Policy> Coordinator<P> {
    /// The startup sequence for every unfinished task: take its lease (one
    /// held by another live holder is left alone and retried by later
    /// ticks), then reconcile it if this holder took it over. Nothing happens
    /// unless the coordinator flag is enabled.
    pub fn startup(&self, store: &Store, now: u64) -> Result<Startup, TaskError> {
        let mut out = Startup::default();
        if !enabled(store) {
            return Ok(out);
        }
        out.enabled = true;
        let tasks = store.tasks()?;
        for id in unfinished(&tasks)? {
            match self.take(&tasks, &id, now) {
                Err(TaskError::LeaseHeld { .. }) => out.leased_elsewhere.push(id),
                Err(error) => out.errors.push(format!("{id}: {error}")),
                Ok(()) => {
                    if let Err(error) = self.reconcile_restart(store, &id, now, &mut out) {
                        out.errors.push(format!("{id}: {error}"));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Takes or renews the task's lease. Taking it from another holder (a
    /// process that quit or stopped coordinating) is recorded, so restart
    /// reconciliation runs for this holder exactly once, even if it fails
    /// midway and a later tick retries it.
    pub(super) fn take(&self, tasks: &TaskStore<'_>, id: &str, now: u64) -> Result<(), TaskError> {
        let previous: Option<String> = tasks
            .conn
            .query_row(
                "SELECT holder FROM native_task_leases WHERE resource=?1",
                [lease_resource(id)],
                |row| row.get(0),
            )
            .optional()?;
        self.lease(tasks, id, now)?;
        if let Some(previous) = previous.filter(|holder| *holder != self.holder) {
            let event = NewEvent {
                id: takeover_event(id, &self.holder),
                task_id: id.into(),
                assignment_id: None,
                attempt_id: None,
                kind: "task.lease_taken_over".into(),
                payload: json!({"previous": previous, "holder": self.holder}),
            };
            tasks.append_event(&event, now)?;
        }
        Ok(())
    }

    /// Reconciles a task this holder took over and has not reconciled yet;
    /// otherwise does nothing.
    pub(super) fn reconcile_restart(
        &self,
        store: &Store,
        id: &str,
        now: u64,
        out: &mut Startup,
    ) -> Result<(), TaskError> {
        let tasks = store.tasks()?;
        let pending = exists(&tasks, &takeover_event(id, &self.holder))?
            && !exists(&tasks, &restarted_event(id, &self.holder))?;
        let Some(task) = tasks.task(id)?.filter(|_| pending) else {
            return Ok(());
        };
        let finished = matches!(
            task.lifecycle,
            TaskLifecycle::Cancelled | TaskLifecycle::Completed
        );
        if !finished {
            self.interrupt_running(&tasks, &task, now, out)?;
            let problems = verify_workspaces(&tasks, &task)?;
            if !problems.is_empty() {
                out.missing
                    .extend(problems.iter().map(|(w, _)| w.id.clone()));
                recovery_condition(&tasks, &task, &problems, now)?;
            }
            out.restored.extend(restore_decisions(&tasks, id, now)?);
            sync_waiting(&tasks, id, now)?;
        }
        let done = NewEvent {
            id: restarted_event(id, &self.holder),
            task_id: id.into(),
            assignment_id: None,
            attempt_id: None,
            kind: "task.restart_reconciled".into(),
            payload: json!({"holder": self.holder}),
        };
        tasks.append_event(&done, now)?;
        out.reconciled.push(id.into());
        Ok(())
    }

    /// Moves every attempt whose worker died with the old holder to
    /// Interrupted, continuing or holding it per the task's policy.
    fn interrupt_running(
        &self,
        tasks: &TaskStore<'_>,
        task: &Task,
        now: u64,
        out: &mut Startup,
    ) -> Result<(), TaskError> {
        let continues = self.policy.restart_continuation(task);
        let state = if continues { SCHEDULED } else { HELD };
        let tx = tasks.begin()?;
        let inside = TaskStore { conn: &tx };
        let mut held = Vec::new();
        for assignment in inside.assignments(&task.id)? {
            let Some(attempt) = inside.attempts(&task.id, &assignment.id)?.pop() else {
                continue;
            };
            if !was_running(&inside, &attempt)? {
                continue;
            }
            park(&tx, &attempt, RESTART, RESTART_REASON, state, false, now)?;
            out.interrupted.push(attempt.id.clone());
            if !continues {
                held.push(attempt.id);
            }
        }
        if !held.is_empty() {
            let reason = "The app restarted while workers ran, and this task's policy does not continue work after a restart. Resume the task to continue it.";
            wait_for(&tx, &task.id, RESTART, reason, now)?;
            notify(&tx, &task.id, None, "task.restart_held", reason, now)?;
        }
        tx.commit()?;
        out.held.extend(held);
        Ok(())
    }
}

/// Why a recorded workspace can no longer be used as recorded, if it cannot:
/// the directory is gone, it is no longer a worktree of its repository, its
/// branch changed, or its HEAD no longer descends from the recorded base.
fn workspace_problem(workspace: &Workspace) -> Option<String> {
    let path = Path::new(&workspace.path);
    if !path.is_dir() {
        return Some(format!("{} no longer exists", workspace.path));
    }
    if workspace.mode == WorkspaceMode::Checkout {
        return None;
    }
    let same = |other: &str| {
        let canonical = |p: &str| std::fs::canonicalize(p).ok();
        canonical(other).is_some() && canonical(other) == canonical(&workspace.path)
    };
    let trees = match crate::git::worktrees(Path::new(&workspace.repository)) {
        Ok(trees) => trees,
        Err(error) => return Some(format!("its repository cannot be read ({error})")),
    };
    let Some(tree) = trees.iter().find(|t| same(&t.path)) else {
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

/// Workspaces of the task's unfinished assignments that are missing or
/// moved, with why.
pub(super) fn verify_workspaces(
    tasks: &TaskStore<'_>,
    task: &Task,
) -> Result<Vec<(Workspace, String)>, TaskError> {
    let live: Vec<String> = tasks
        .assignments(&task.id)?
        .into_iter()
        .filter(|a| a.status != super::AssignmentState::Cancelled)
        .map(|a| a.id)
        .collect();
    Ok(tasks
        .workspaces(&task.id)?
        .into_iter()
        .filter(|w| w.assignment_id.as_ref().is_none_or(|a| live.contains(a)))
        .filter_map(|w| workspace_problem(&w).map(|why| (w, why)))
        .collect())
}

/// The explicit recovery condition for missing or moved workspaces: one
/// event per workspace, the task Blocked with the reason (a paused task keeps
/// its pause; resuming checks again), and one Mono notice.
pub(super) fn recovery_condition(
    tasks: &TaskStore<'_>,
    task: &Task,
    problems: &[(Workspace, String)],
    now: u64,
) -> Result<String, TaskError> {
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
    let tx = tasks.begin()?;
    for (workspace, why) in problems {
        let event = NewEvent {
            id: format!("workspace_missing:{}@{}", workspace.id, task.revision),
            task_id: task.id.clone(),
            assignment_id: workspace.assignment_id.clone(),
            attempt_id: None,
            kind: "recovery.workspace_missing".into(),
            payload: json!({"workspaceId": workspace.id, "path": workspace.path,
                "branch": workspace.branch, "baseCommit": workspace.base_commit, "reason": why}),
        };
        append(&tx, &event, now)?;
    }
    recovery::block(&tx, &task.id, &reason, now)?;
    notify(
        &tx,
        &task.id,
        None,
        "recovery.workspace_missing",
        &reason,
        now,
    )?;
    tx.commit()?;
    Ok(reason)
}

/// Puts every open decision of the task that its Mono still has to answer
/// back in the Mono's queue; queueing is idempotent per decision.
fn restore_decisions(
    tasks: &TaskStore<'_>,
    task: &str,
    now: u64,
) -> Result<Vec<String>, TaskError> {
    let mut statement = tasks.conn.prepare(
        "SELECT id FROM native_task_decisions WHERE task_id=?1 AND state='open' ORDER BY rowid",
    )?;
    let ids = statement
        .query_map([task], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut restored = Vec::new();
    for id in ids {
        if tasks.escalation(&id)?.is_some() {
            continue;
        }
        queue_for_mono(tasks.conn, &id, now)?;
        restored.push(id);
    }
    Ok(restored)
}

#[cfg(test)]
#[path = "restart_tests.rs"]
mod tests;
