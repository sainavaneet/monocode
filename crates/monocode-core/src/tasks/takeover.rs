//! Workers held because the holder that ran them might still be running
//! them. Each held worker records the holder it belonged to (its owner)
//! when it was held, and is judged against that holder however many
//! takeovers follow: a later hop from a dead holder proves nothing about
//! the first one.
use super::coordinator::{Coordinator, Policy};
use super::events::append;
use super::guard::task_or_missing;
use super::recovery::{HELD, SCHEDULED};
use super::restart::{native_liveness, Liveness};
use super::store::ms;
use super::waiting::{activate, waiting_cause, TAKEOVER};
use super::worker::{event, payload_of};
use super::{Attempt, TaskError};
use crate::store::Store;
use rusqlite::{params, Connection};
use serde_json::json;

fn owner_event(attempt: &str) -> String {
    format!("held-owner:{attempt}")
}

/// Records which holder ran the attempt it holds (write-once).
pub(super) fn record_owner(
    conn: &Connection,
    attempt: &Attempt,
    owner: &str,
    now: u64,
) -> Result<(), TaskError> {
    let payload = json!({"holder": owner});
    let held = event(
        attempt,
        owner_event(&attempt.id),
        "attempt.held_owner",
        payload,
    );
    append(conn, &held, now)?;
    Ok(())
}

/// A takeover-held worker: its attempt and owner.
pub(super) struct Held {
    pub attempt: String,
    /// The holder that ran it; empty when unknown (never provably dead).
    pub owner: String,
}

/// The task's takeover-held workers (of `assignments` only, when given).
pub(super) fn held(
    conn: &Connection,
    task: &str,
    assignments: &[String],
) -> Result<Vec<Held>, TaskError> {
    let mut statement = conn.prepare(
        "SELECT attempt_id,assignment_id FROM native_task_recoveries
         WHERE task_id=?1 AND category=?2 AND state=?3 ORDER BY rowid",
    )?;
    let rows = statement
        .query_map(params![task, TAKEOVER, HELD], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::new();
    for (attempt, assignment) in rows {
        if !assignments.is_empty() && !assignments.contains(&assignment) {
            continue;
        }
        let owner = payload_of(conn, &owner_event(&attempt))?
            .and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok())
            .and_then(|p| p["holder"].as_str().map(str::to_owned))
            .unwrap_or_default();
        out.push(Held { attempt, owner });
    }
    Ok(out)
}

/// Whether any worker of the task is takeover-held.
pub(super) fn any_held(conn: &Connection, task: &str) -> Result<bool, TaskError> {
    Ok(!held(conn, task, &[])?.is_empty())
}

/// Whether resuming these held workers needs the caller's confirmation:
/// one of their owners is not provably gone.
pub(super) fn needs_confirmation(
    conn: &Connection,
    task: &str,
    assignments: &[String],
) -> Result<bool, TaskError> {
    Ok(held(conn, task, assignments)?
        .iter()
        .any(|h| native_liveness(&h.owner) != Liveness::Dead))
}

/// Schedules these held workers now; returns their attempts.
pub(super) fn release(
    conn: &Connection,
    workers: &[Held],
    now: u64,
) -> Result<Vec<String>, TaskError> {
    for worker in workers {
        conn.execute(
            "UPDATE native_task_recoveries SET state=?2,due_at=?3,revision=revision+1,updated_at=?3
             WHERE attempt_id=?1 AND state=?4",
            params![worker.attempt, SCHEDULED, ms(now), HELD],
        )?;
    }
    Ok(workers.iter().map(|w| w.attempt.clone()).collect())
}

impl<P: Policy> Coordinator<P> {
    /// Held workers whose owner's process is gone continue (when the policy
    /// continues after a restart), whatever the task's lifecycle; a task
    /// that waited only for takeover-held workers is Active again once none
    /// is left.
    pub(super) fn release_takeover(
        &self,
        store: &Store,
        id: &str,
        now: u64,
    ) -> Result<(), TaskError> {
        let tasks = store.tasks()?;
        let workers = held(tasks.conn, id, &[])?;
        if workers.is_empty() {
            return Ok(());
        }
        let task = task_or_missing(&tasks, id)?;
        if !self.restart_continues(store, &task)? {
            return Ok(());
        }
        let gone: Vec<Held> = workers
            .into_iter()
            .filter(|w| (self.liveness)(&w.owner) == Liveness::Dead)
            .collect();
        if gone.is_empty() {
            return Ok(());
        }
        let tx = tasks.begin()?;
        release(&tx, &gone, now)?;
        let cleared = !any_held(&tx, id)?;
        if cleared && waiting_cause(&tx, id)?.as_deref() == Some(TAKEOVER) {
            activate(&tx, id, now)?;
        }
        tx.commit()?;
        Ok(())
    }
}
