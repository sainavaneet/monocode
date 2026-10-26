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
use crate::orchestration::WorkspaceMode;
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

/// How long an assignment may wait for a worktree another live attempt
/// owns before Mono is told, once.
pub const OWNED_NOTICE_MS: u64 = 10 * 60 * 1000;

fn canonical(path: &str) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.into())
}

/// Another live attempt (of any task) whose recorded worktree is the one
/// recorded for `assignment`, compared by canonical path: one mutable
/// workspace is never owned by two live attempts. Returns that attempt.
/// Checkout records are read-only investigations and are not owned.
pub(super) fn workspace_owner(
    conn: &Connection,
    task: &str,
    assignment: &str,
) -> Result<Option<String>, TaskError> {
    let mine = TaskStore { conn }.workspaces(task)?.into_iter().find(|w| {
        w.assignment_id.as_deref() == Some(assignment) && w.mode == WorkspaceMode::Worktree
    });
    let Some(mine) = mine else {
        return Ok(None);
    };
    let mut statement = conn.prepare(
        "SELECT t.id,o.path FROM native_task_workspaces o
           JOIN native_task_attempts t ON t.task_id=o.task_id AND t.assignment_id=o.assignment_id
         WHERE o.mode='worktree' AND NOT (o.task_id=?1 AND o.assignment_id=?2)
           AND t.status IN ('created','starting','running','waiting')
         ORDER BY t.rowid",
    )?;
    let live = statement
        .query_map(params![task, assignment], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let path = canonical(&mine.path);
    Ok(live
        .into_iter()
        .find(|(_, other)| canonical(other) == path)
        .map(|(attempt, _)| attempt))
}

impl<P: Policy> Coordinator<P> {
    /// Whether another live attempt owns the assignment's worktree. Once it
    /// has waited [`OWNED_NOTICE_MS`] for that owner, Mono is told once.
    pub(super) fn held_by_owner(
        &self,
        tasks: &TaskStore<'_>,
        task: &str,
        assignment: &str,
        now: u64,
    ) -> Result<bool, TaskError> {
        let Some(owner) = workspace_owner(tasks.conn, task, assignment)? else {
            return Ok(false);
        };
        let waiting = format!("workspace.owned:{task}/{assignment}@{owner}");
        // The wait marker, and the notice once due, are written together.
        let tx = tasks.begin()?;
        let since = match super::worker::payload_of(&tx, &waiting)? {
            Some(payload) => serde_json::from_str::<serde_json::Value>(&payload)?["since"]
                .as_u64()
                .unwrap_or(now),
            None => {
                let event = super::NewEvent {
                    id: waiting.clone(),
                    task_id: task.into(),
                    assignment_id: Some(assignment.into()),
                    attempt_id: None,
                    kind: "workspace.owned".into(),
                    payload: json!({"since": now, "owner": owner}),
                };
                append(&tx, &event, now)?;
                now
            }
        };
        let notice = format!("{waiting}#notice");
        if now >= since.saturating_add(OWNED_NOTICE_MS)
            && super::worker::payload_of(&tx, &notice)?.is_none()
        {
            let reason = format!(
                "Assignment {assignment} has waited since {since} for its worktree, which attempt {owner} still works in; it starts once that attempt ends."
            );
            let marker = super::NewEvent {
                id: notice,
                task_id: task.into(),
                assignment_id: Some(assignment.into()),
                attempt_id: None,
                kind: "workspace.owned_notice".into(),
                payload: json!({"owner": owner}),
            };
            append(&tx, &marker, now)?;
            super::notices::notify(&tx, task, Some(assignment), OWNED_ELSEWHERE, &reason, now)?;
        }
        tx.commit()?;
        Ok(true)
    }
}

/// The Mono notice for a worktree another attempt keeps owning.
pub(super) const OWNED_ELSEWHERE: &str = "workspace.owned_elsewhere";

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
