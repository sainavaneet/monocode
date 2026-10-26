//! Restart reconciliation: what a coordinator does with a task whose lease it
//! took from another holder.
//!
//! Whether the previous holder's workers died with it decides everything.
//! A native holder names its process (`monocode-native:<pid>:…`); when that
//! process is gone (this app before it quit), every attempt that was
//! Starting with a session, Running or Waiting is Interrupted, because
//! quitting kills the worker process groups (`process::shutdown_all`), and
//! continues through task-7's recovery path on the same lineage when the
//! task's policy allows restart continuation (otherwise it is held and the
//! task waits with the reason). When the previous holder may still be alive
//! (a live process that stalled past its lease, or another app such as the
//! desktop app, which cannot be checked), its workers are Interrupted and
//! held, nothing is relaunched over them, the task waits with the reason and
//! Mono is told; they continue once that process is gone, or when the user
//! or Mono resumes the task.
//!
//! Then: recorded workspaces must still exist on their branch and base (a
//! missing or moved one is the explicit recovery condition, never a fresh
//! start), waiting decisions are restored to Mono's queue (idempotently), a
//! task-wide open question keeps the task Waiting, and paused tasks stay
//! paused. The Git checks can run without the store lock: [`Coordinator::
//! startup_prepare`], [`workspace_check::check`], [`Coordinator::
//! startup_finish`].
use super::coordinator::{enabled, lease_resource, Coordinator, Policy};
use super::decisions::queue_for_mono;
use super::guard::{may_be_running, task_or_missing};
use super::notices::notify;
use super::park::park;
use super::recovery::{HELD, SCHEDULED};
use super::waiting::{activate, sync_waiting, wait_for, waiting_cause, TAKEOVER};
use super::workspace_check::{self, Problem};
use super::{NewEvent, Task, TaskError, TaskLifecycle, TaskStore, Workspace};
use crate::store::Store;
use rusqlite::OptionalExtension;
use serde_json::json;

pub use super::waiting::RESTART;
const RESTART_REASON: &str =
    "The app quit while this worker ran, and quitting stops the worker's process.";
const TAKEOVER_REASON: &str = "Another app or process held this task and may still be running this worker; it is held until that process is gone or the task is resumed.";

/// Whether a lease holder's process is still running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    Dead,
    Alive,
    /// Not a holder this app can check (another app).
    Unknown,
}

/// The liveness of a native holder (`monocode-native:<pid>:<random>`) on
/// this machine; any other holder is Unknown.
pub fn native_liveness(holder: &str) -> Liveness {
    let Some(pid) = holder
        .strip_prefix("monocode-native:")
        .and_then(|rest| rest.split(':').next())
        .and_then(|pid| pid.parse::<u32>().ok())
    else {
        return Liveness::Unknown;
    };
    if pid == std::process::id() {
        return Liveness::Alive;
    }
    match running(pid) {
        Some(true) => Liveness::Alive,
        Some(false) => Liveness::Dead,
        None => Liveness::Unknown,
    }
}

/// Whether process `pid` exists; None when it cannot be told (a pid out of
/// range, or no cheap check on this platform).
#[cfg(unix)]
fn running(pid: u32) -> Option<bool> {
    let pid = libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 0)?;
    // Signal 0 only checks that the process exists.
    let found = unsafe { libc::kill(pid, 0) } == 0;
    Some(found || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

#[cfg(not(unix))]
fn running(_pid: u32) -> Option<bool> {
    None
}

/// What startup reconciliation did, by task, attempt, workspace or decision.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Startup {
    /// False when the flag is off and nothing was read or written.
    pub enabled: bool,
    pub reconciled: Vec<String>,
    /// Attempts moved to Interrupted.
    pub interrupted: Vec<String>,
    /// Interrupted attempts held (policy, or a previous holder that may be
    /// alive).
    pub held: Vec<String>,
    /// Workspaces that are missing or moved.
    pub missing: Vec<String>,
    /// Open decisions put back in their Mono's queue.
    pub restored: Vec<String>,
    /// Tasks another holder still leases; the coordinator's ticks retry them.
    pub leased_elsewhere: Vec<String>,
    pub errors: Vec<String>,
}

/// Workspaces startup still has to check, per task.
pub type Pending = Vec<(String, Vec<Workspace>)>;

fn takeover_event(task: &str, holder: &str) -> String {
    format!("takeover:{task}:{holder}")
}

fn restarted_event(task: &str, holder: &str) -> String {
    format!("restart:{task}:{holder}")
}

/// Every task that is neither cancelled nor completed, in creation order.
fn unfinished(tasks: &TaskStore<'_>) -> Result<Vec<String>, TaskError> {
    let mut statement = tasks.conn.prepare(
        "SELECT id FROM native_tasks WHERE lifecycle NOT IN ('cancelled','completed') ORDER BY rowid",
    )?;
    let ids = statement.query_map([], |row| row.get(0))?;
    Ok(ids.collect::<Result<_, _>>()?)
}

fn marker(id: String, task: &str, kind: &str, payload: serde_json::Value) -> NewEvent {
    NewEvent {
        id,
        task_id: task.into(),
        assignment_id: None,
        attempt_id: None,
        kind: kind.into(),
        payload,
    }
}

impl<P: Policy> Coordinator<P> {
    /// The whole startup sequence with the store held throughout (tests,
    /// and callers that do not share the store).
    pub fn startup(&self, store: &Store, now: u64) -> Result<Startup, TaskError> {
        let (mut out, pending) = self.startup_prepare(store, now)?;
        let problems = check_pending(&pending);
        self.startup_finish(store, &mut out, problems, now)?;
        Ok(out)
    }

    /// Startup's store part: take each unfinished task's lease (one held by
    /// another holder is left alone; later ticks retry), reconcile what this
    /// holder took over, and return the workspaces to check.
    pub fn startup_prepare(
        &self,
        store: &Store,
        now: u64,
    ) -> Result<(Startup, Pending), TaskError> {
        let mut out = Startup::default();
        let mut pending = Vec::new();
        if !enabled(store) {
            return Ok((out, pending));
        }
        out.enabled = true;
        let tasks = store.tasks()?;
        for id in unfinished(&tasks)? {
            match self.take(&tasks, &id, now) {
                Err(TaskError::LeaseHeld { .. }) => out.leased_elsewhere.push(id),
                Err(error) => out.errors.push(format!("{id}: {error}")),
                Ok(()) => match self.reconcile_store(store, &id, now, &mut out) {
                    Ok(Some(workspaces)) => pending.push((id, workspaces)),
                    Ok(None) => {}
                    Err(error) => out.errors.push(format!("{id}: {error}")),
                },
            }
        }
        Ok((out, pending))
    }

    /// Startup's last store part: records the recovery condition for the
    /// workspaces the checks found missing or moved, and marks each task
    /// reconciled (a task whose finish failed is retried by the next tick).
    pub fn startup_finish(
        &self,
        store: &Store,
        out: &mut Startup,
        problems: Vec<(String, Vec<Problem>)>,
        now: u64,
    ) -> Result<(), TaskError> {
        if !out.enabled {
            return Ok(());
        }
        let tasks = store.tasks()?;
        for (task, found) in problems {
            if let Err(error) = self.finish_one(&tasks, &task, &found, now, out) {
                out.errors.push(format!("{task}: {error}"));
            }
        }
        Ok(())
    }

    /// Takes or renews the task's lease. Taking it from another holder is
    /// recorded with that holder, so reconciliation runs for this holder
    /// exactly once, even if it fails midway and a later tick retries it.
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
            let payload = json!({"previous": previous, "holder": self.holder});
            let event = marker(
                takeover_event(id, &self.holder),
                id,
                "task.lease_taken_over",
                payload,
            );
            tasks.append_event(&event, now)?;
        }
        Ok(())
    }

    /// The holder this one took the task over from, if it did.
    fn previous_holder(
        &self,
        tasks: &TaskStore<'_>,
        id: &str,
    ) -> Result<Option<String>, TaskError> {
        Ok(
            super::worker::payload_of(tasks.conn, &takeover_event(id, &self.holder))?
                .and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok())
                .and_then(|p| p["previous"].as_str().map(str::to_owned)),
        )
    }

    /// Reconciles a task this holder took over (all of it, Git included,
    /// under the store: the ticker's path).
    pub(super) fn reconcile_restart(
        &self,
        store: &Store,
        id: &str,
        now: u64,
        out: &mut Startup,
    ) -> Result<(), TaskError> {
        if let Some(workspaces) = self.reconcile_store(store, id, now, out)? {
            let found = workspace_check::check(&workspaces);
            self.finish_one(&store.tasks()?, id, &found, now, out)?;
        }
        Ok(())
    }

    /// The store part of reconciling a task this holder took over and has
    /// not reconciled yet; returns the workspaces still to check.
    fn reconcile_store(
        &self,
        store: &Store,
        id: &str,
        now: u64,
        out: &mut Startup,
    ) -> Result<Option<Vec<Workspace>>, TaskError> {
        let tasks = store.tasks()?;
        let Some(previous) = self.previous_holder(&tasks, id)? else {
            return Ok(None);
        };
        if super::worker::payload_of(tasks.conn, &restarted_event(id, &self.holder))?.is_some() {
            return Ok(None);
        }
        let task = task_or_missing(&tasks, id)?;
        let finished = matches!(
            task.lifecycle,
            TaskLifecycle::Cancelled | TaskLifecycle::Completed
        );
        if finished {
            return Ok(Some(Vec::new()));
        }
        let dead = (self.liveness)(&previous) == Liveness::Dead;
        self.interrupt_running(&tasks, &task, dead, now, out)?;
        out.restored.extend(restore_decisions(&tasks, id, now)?);
        sync_waiting(&tasks, id, now)?;
        Ok(Some(workspace_check::to_check(&tasks, id)?))
    }

    /// Records the workspace problems found for a task this holder took
    /// over, then marks it reconciled. Only then: a failure before the mark
    /// is retried by the next tick (every step is idempotent).
    fn finish_one(
        &self,
        tasks: &TaskStore<'_>,
        id: &str,
        found: &[Problem],
        now: u64,
        out: &mut Startup,
    ) -> Result<(), TaskError> {
        record(tasks, id, found, now, out)?;
        let previous = self.previous_holder(tasks, id)?;
        let payload = json!({"holder": self.holder, "previous": previous});
        let done = marker(
            restarted_event(id, &self.holder),
            id,
            "task.restart_reconciled",
            payload,
        );
        tasks.append_event(&done, now)?;
        out.reconciled.push(id.into());
        Ok(())
    }

    /// Moves every attempt whose worker ran under the previous holder to
    /// Interrupted: continued or held per the policy when that holder is
    /// dead, held otherwise (its process may still run them).
    fn interrupt_running(
        &self,
        tasks: &TaskStore<'_>,
        task: &Task,
        dead: bool,
        now: u64,
        out: &mut Startup,
    ) -> Result<(), TaskError> {
        let continues = dead && self.policy.restart_continuation(task);
        let (category, reason) = if dead {
            (RESTART, RESTART_REASON)
        } else {
            (TAKEOVER, TAKEOVER_REASON)
        };
        let state = if continues { SCHEDULED } else { HELD };
        let tx = tasks.begin()?;
        let inside = TaskStore { conn: &tx };
        let mut held = Vec::new();
        for assignment in inside.assignments(&task.id)? {
            let Some(attempt) = inside.attempts(&task.id, &assignment.id)?.pop() else {
                continue;
            };
            if !may_be_running(&attempt) || inside.claim(&attempt.id)?.is_some() {
                continue;
            }
            park(&tx, &attempt, category, reason, state, false, now)?;
            out.interrupted.push(attempt.id.clone());
            if !continues {
                held.push(attempt.id);
            }
        }
        if !held.is_empty() {
            let (kind, why) = if dead {
                ("task.restart_held", "The app restarted while workers ran, and this task's policy does not continue work after a restart. Resume the task to continue it.")
            } else {
                ("task.takeover_held", "Another app or process that may still be running this task's workers held it; they are held, and nothing is relaunched until that process is gone or the task is resumed.")
            };
            wait_for(&tx, &task.id, category, why, now)?;
            notify(&tx, &task.id, None, kind, why, now)?;
        }
        tx.commit()?;
        out.held.extend(held);
        Ok(())
    }

    /// Workers held because the previous holder might still run them
    /// continue once that holder's process is gone (and the policy continues
    /// after a restart), whatever the task's lifecycle; a task waiting only
    /// for them is Active again.
    pub(super) fn release_takeover(
        &self,
        store: &Store,
        id: &str,
        now: u64,
    ) -> Result<(), TaskError> {
        let tasks = store.tasks()?;
        if !takeover_held(tasks.conn, id)? {
            return Ok(());
        }
        let Some(previous) = self.previous_holder(&tasks, id)? else {
            return Ok(());
        };
        let task = task_or_missing(&tasks, id)?;
        if (self.liveness)(&previous) != Liveness::Dead || !self.policy.restart_continuation(&task)
        {
            return Ok(());
        }
        let tx = tasks.begin()?;
        release_takeover_rows(&tx, id, now)?;
        if waiting_cause(&tx, id)?.as_deref() == Some(TAKEOVER) {
            activate(&tx, id, now)?;
        }
        tx.commit()?;
        Ok(())
    }
}

/// Whether workers of the task are held because a previous holder might
/// still run them (independent of the task's lifecycle).
pub(super) fn takeover_held(conn: &rusqlite::Connection, task: &str) -> Result<bool, TaskError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM native_task_recoveries WHERE task_id=?1 AND category=?2 AND state=?3)",
        rusqlite::params![task, TAKEOVER, HELD],
        |row| row.get(0),
    )?)
}

/// Schedules the task's takeover-held workers now.
pub(super) fn release_takeover_rows(
    conn: &rusqlite::Connection,
    task: &str,
    now: u64,
) -> Result<Vec<String>, TaskError> {
    let held: Vec<String> = {
        let mut statement = conn.prepare(
            "SELECT attempt_id FROM native_task_recoveries WHERE task_id=?1 AND category=?2 AND state=?3",
        )?;
        let rows =
            statement.query_map(rusqlite::params![task, TAKEOVER, HELD], |row| row.get(0))?;
        rows.collect::<Result<_, _>>()?
    };
    conn.execute(
        "UPDATE native_task_recoveries SET state=?2,due_at=?3,revision=revision+1,updated_at=?3
         WHERE task_id=?1 AND category=?4 AND state=?5",
        rusqlite::params![task, SCHEDULED, super::store::ms(now), TAKEOVER, HELD],
    )?;
    Ok(held)
}

/// The holder the task was last taken over from, by any holder.
pub(super) fn last_previous_holder(
    conn: &rusqlite::Connection,
    task: &str,
) -> Result<Option<String>, TaskError> {
    Ok(conn
        .query_row(
            "SELECT json_extract(payload,'$.previous') FROM native_task_events
             WHERE task_id=?1 AND kind='task.lease_taken_over' ORDER BY seq DESC LIMIT 1",
            [task],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// Runs the Git checks for what [`Coordinator::startup_prepare`] returned;
/// needs no store.
pub fn check_pending(pending: &Pending) -> Vec<(String, Vec<Problem>)> {
    pending
        .iter()
        .map(|(task, workspaces)| (task.clone(), workspace_check::check(workspaces)))
        .collect()
}

/// Records the recovery condition for `found`, unless the task finished
/// meanwhile.
fn record(
    tasks: &TaskStore<'_>,
    task: &str,
    found: &[Problem],
    now: u64,
    out: &mut Startup,
) -> Result<(), TaskError> {
    if found.is_empty() {
        return Ok(());
    }
    let current = task_or_missing(tasks, task)?;
    if matches!(
        current.lifecycle,
        TaskLifecycle::Cancelled | TaskLifecycle::Completed
    ) {
        return Ok(());
    }
    let tx = tasks.begin()?;
    workspace_check::record_condition(&tx, task, found, now)?;
    tx.commit()?;
    out.missing.extend(found.iter().map(|(w, _)| w.id.clone()));
    Ok(())
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
