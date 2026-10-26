//! Interrupting workers on the lifecycle's behalf (pause, restart) rather
//! than because they failed, and moving the recovery rows that carry them on.
//!
//! A parked attempt goes through task-7's recovery path like any other
//! interrupted attempt: once its task is Active, the stop confirmed and its
//! row due, the coordinator replaces it on the same lineage with a durable
//! hand-off, resuming the provider thread when it can.
use super::events::append;
use super::recovery::{
    lineage, stop_delivery, BLOCKED, BLOCK_PENDING, COLUMNS, HELD, SCHEDULED, WORKER_STOP,
};
use super::store::ms;
use super::worker::{event, settle};
use super::{AttemptState, Requester, State, Subject, TaskError, TaskStore};
use rusqlite::{params, Connection};
use serde_json::json;

/// Interrupts a live attempt on the lifecycle's behalf (pause, restart),
/// not because it failed, inside the caller's transaction: the event
/// `parked:<attempt>`, the attempt Interrupted *before* anything stops it
/// (so its turn ending is not read as a failure), a stop request for its
/// session when `stop`, and a recovery row in `state` (Scheduled now, or
/// Held) on the same account and lineage, spending no retry.
pub(super) fn park(
    conn: &Connection,
    attempt: &super::Attempt,
    category: &str,
    reason: &str,
    state: &str,
    stop: bool,
    now: u64,
) -> Result<(), TaskError> {
    let tasks = TaskStore { conn };
    let payload = json!({"category": category, "reason": reason, "sessionId": attempt.session_id});
    let parked = event(
        attempt,
        format!("parked:{}", attempt.id),
        "attempt.parked",
        payload,
    );
    let parked = append(conn, &parked, now)?;
    let interrupted = State::Attempt(AttemptState::Interrupted);
    settle(
        conn,
        Subject::Attempt(&attempt.id),
        interrupted,
        Requester::Coordinator,
        now,
    )?;
    if let Some(session) = attempt.session_id.as_ref().filter(|_| stop) {
        let reference = format!("session:{session}");
        tasks.enqueue_delivery(
            &stop_delivery(&attempt.id),
            &parked.id,
            WORKER_STOP,
            &reference,
            now,
        )?;
    }
    let (root, used) = lineage(&tasks, &attempt.id)?;
    conn.execute(
        &format!(
            "INSERT INTO native_task_recoveries({COLUMNS},created_at,updated_at)
           VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?8,?9,?10,?11,NULL,1,?11,?11)
           ON CONFLICT(attempt_id) DO NOTHING"
        ),
        params![
            attempt.id,
            attempt.task_id,
            attempt.assignment_id,
            root,
            used,
            category,
            reason,
            attempt.provider,
            attempt.account_id,
            state,
            ms(now)
        ],
    )?;
    Ok(())
}

/// Sets every waiting recovery row of the task (of `assignment` only, when
/// given) in one of `from` to `to`, due now. Returns the attempts it moved.
pub(super) fn move_rows(
    conn: &Connection,
    task: &str,
    assignment: Option<&str>,
    from: &[&str],
    to: &str,
    now: u64,
) -> Result<Vec<String>, TaskError> {
    let mut statement = conn.prepare(
        "SELECT attempt_id,state FROM native_task_recoveries
         WHERE task_id=?1 AND (?2 IS NULL OR assignment_id=?2) ORDER BY rowid",
    )?;
    let rows = statement
        .query_map(params![task, assignment], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut moved = Vec::new();
    for (attempt, state) in rows {
        if !from.contains(&state.as_str()) {
            continue;
        }
        conn.execute(
            "UPDATE native_task_recoveries SET state=?2,due_at=?3,revision=revision+1,updated_at=?3
             WHERE attempt_id=?1",
            params![attempt, to, ms(now)],
        )?;
        moved.push(attempt);
    }
    Ok(moved)
}

/// The recovery states that still wait to act.
pub(super) const WAITING_ROWS: [&str; 4] = [SCHEDULED, BLOCKED, BLOCK_PENDING, HELD];
