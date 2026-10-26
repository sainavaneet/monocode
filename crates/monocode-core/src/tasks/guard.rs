//! The one rule every launch path follows: two agents never work one
//! workspace. Nothing launches for an assignment while an earlier worker of
//! it, or of another assignment of the task sharing its workspace, has a
//! stop the app has not confirmed, nor while another live attempt (of any
//! task) works in the same recorded worktree (a stop is confirmed only once the
//! session's turn has ended in every window). A stop that stays unconfirmed
//! past [`STOP_STALL_MS`] is reported once to Mono.
//!
//! Also the predicates and lookups the lifecycle, restart and steering code
//! share.
use super::coordinator::{Coordinator, Policy, Tick};
use super::events::append;
use super::recovery::{notify_mono, STOP_STALL_MS, WORKER_STOP};
use super::store::read_ms;
use super::worker::event;
use super::{Attempt, AttemptState, Task, TaskError, TaskStore};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

/// The task, or NotFound.
pub(super) fn task_or_missing(tasks: &TaskStore<'_>, id: &str) -> Result<Task, TaskError> {
    tasks
        .task(id)?
        .ok_or_else(|| TaskError::NotFound(format!("task:{id}")))
}

/// Whether the attempt's worker may still be running: a live state, or a
/// start whose session was handed out. An attempt without a session never
/// ran.
pub(super) fn may_be_running(attempt: &Attempt) -> bool {
    match attempt.status {
        AttemptState::Running | AttemptState::Waiting => true,
        AttemptState::Starting => attempt.session_id.is_some(),
        _ => false,
    }
}

/// The earliest unconfirmed stop of a worker of `assignment`, or of another
/// assignment of the task recorded in the same workspace path: its attempt
/// and when the stop was requested.
pub(super) fn pending_stop(
    conn: &Connection,
    task: &str,
    assignment: &str,
) -> Result<Option<(String, u64)>, TaskError> {
    Ok(conn
        .query_row(
            "SELECT e.attempt_id,d.created_at FROM native_task_deliveries d
               JOIN native_task_events e ON e.id=d.event_id
             WHERE d.destination=?3 AND d.state<>'acknowledged' AND e.task_id=?1
               AND e.attempt_id IS NOT NULL
               AND (e.assignment_id=?2 OR e.assignment_id IN (
                 SELECT o.assignment_id FROM native_task_workspaces mine
                   JOIN native_task_workspaces o ON o.task_id=mine.task_id AND o.path=mine.path
                 WHERE mine.task_id=?1 AND mine.assignment_id=?2))
             ORDER BY d.rowid LIMIT 1",
            params![task, assignment, WORKER_STOP],
            |row| Ok((row.get::<_, String>(0)?, read_ms(row, 1)?)),
        )
        .optional()?)
}

/// Another live attempt (of any task) whose recorded worktree is the one
/// recorded for `assignment`: one mutable workspace is never owned by two
/// live attempts. Returns that attempt. Checkout records are read-only
/// investigations and are not owned.
pub(super) fn workspace_owner(
    conn: &Connection,
    task: &str,
    assignment: &str,
) -> Result<Option<String>, TaskError> {
    Ok(conn
        .query_row(
            "SELECT t.id FROM native_task_workspaces mine
               JOIN native_task_workspaces o ON o.path=mine.path AND o.mode='worktree'
                 AND NOT (o.task_id=mine.task_id AND o.assignment_id=mine.assignment_id)
               JOIN native_task_attempts t ON t.task_id=o.task_id AND t.assignment_id=o.assignment_id
             WHERE mine.task_id=?1 AND mine.assignment_id=?2 AND mine.mode='worktree'
               AND t.status IN ('created','starting','running','waiting')
             ORDER BY t.rowid LIMIT 1",
            params![task, assignment],
            |row| row.get(0),
        )
        .optional()?)
}

/// Whether a stop of the task is unconfirmed or a worker of it may still
/// run (the task shows Pausing or Cancelling meanwhile).
pub(super) fn task_stopping(tasks: &TaskStore<'_>, task: &str) -> Result<bool, TaskError> {
    let unconfirmed: bool = tasks.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM native_task_deliveries d JOIN native_task_events e ON e.id=d.event_id
           WHERE e.task_id=?1 AND d.destination=?2 AND d.state<>'acknowledged')",
        params![task, WORKER_STOP],
        |row| row.get(0),
    )?;
    if unconfirmed {
        return Ok(true);
    }
    for assignment in tasks.assignments(task)? {
        if tasks
            .attempts(task, &assignment.id)?
            .iter()
            .any(may_be_running)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Asks the app to stop the attempt's session (event `id`, delivery
/// `delivery`), inside the caller's transaction. Returns whether it asked.
pub(super) fn request_stop(
    conn: &Connection,
    attempt: &Attempt,
    id: String,
    delivery: &str,
    now: u64,
) -> Result<bool, TaskError> {
    let Some(session) = attempt.session_id.as_ref() else {
        return Ok(false);
    };
    let payload = json!({"sessionId": session, "status": attempt.status});
    let stored = append(
        conn,
        &event(attempt, id, "attempt.stop_requested", payload),
        now,
    )?;
    TaskStore { conn }.enqueue_delivery(
        delivery,
        &stored.id,
        WORKER_STOP,
        &format!("session:{session}"),
        now,
    )?;
    Ok(true)
}

impl<P: Policy> Coordinator<P> {
    /// Whether an unconfirmed stop holds the assignment's launch; a stop
    /// unconfirmed past [`STOP_STALL_MS`] is reported once (event and Mono
    /// notice) and listed in `tick.stalled`.
    pub(super) fn held_by_stop(
        &self,
        tasks: &TaskStore<'_>,
        task: &str,
        assignment: &str,
        now: u64,
        tick: &mut Tick,
    ) -> Result<bool, TaskError> {
        let Some((attempt, since)) = pending_stop(tasks.conn, task, assignment)? else {
            return Ok(false);
        };
        if now < since.saturating_add(STOP_STALL_MS) {
            return Ok(true);
        }
        let Some(stopped) = tasks.attempt(&attempt)? else {
            return Ok(true);
        };
        let id = format!("recovery.stop_stalled:{attempt}@1");
        if super::worker::payload_of(tasks.conn, &id)?.is_none() {
            let reason = "A stopped worker's session has not confirmed it stopped, so nothing new starts in its workspace. Check that its window is open and its turn ended.";
            let tx = tasks.begin()?;
            notify_mono(&tx, &stopped, "recovery.stop_stalled", reason, 1, now)?;
            tx.commit()?;
            tick.stalled.push(attempt);
        }
        Ok(true)
    }
}
