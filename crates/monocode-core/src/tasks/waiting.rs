//! Why a task waits, recorded explicitly with the transition that made it
//! wait: a task-wide question, a restart its policy does not continue, or a
//! lease taken over from a holder that may still run its workers.
use super::events::{append, NewEvent};
use super::guard::task_or_missing;
use super::states::apply;
use super::worker::payload_of;
use super::{Requester, State, Subject, TaskError, TaskLifecycle, TaskStore};
use rusqlite::{Connection, OptionalExtension};
use serde_json::json;

/// A task-wide open question holds the task.
pub const DECISION: &str = "decision";
/// A restart the task's policy does not continue.
pub const RESTART: &str = "restart";
/// A lease taken from a holder that may still be running its workers.
pub const TAKEOVER: &str = "takeover";

/// The cause recorded for the transition into Waiting at `revision`.
fn cause_event(task: &str, revision: u64) -> String {
    format!("task.waiting_cause:{task}@{revision}")
}

/// Moves an Active task to Waiting for `cause`, with the reason shown, and
/// records the cause against that transition. Returns whether it moved.
pub(super) fn wait_for(
    conn: &Connection,
    task: &str,
    cause: &str,
    reason: &str,
    now: u64,
) -> Result<bool, TaskError> {
    let tasks = TaskStore { conn };
    let subject = Subject::Task(task);
    let waiting = State::Lifecycle(TaskLifecycle::Waiting);
    if tasks.state(subject, waiting)? != State::Lifecycle(TaskLifecycle::Active) {
        return Ok(false);
    }
    let revision = tasks.revision(subject)?;
    let revision = apply(
        conn,
        subject,
        waiting,
        Requester::Coordinator,
        revision,
        now,
    )?;
    conn.execute(
        "UPDATE native_tasks SET blocked_reason=?2 WHERE id=?1",
        [task, reason],
    )?;
    let cause = NewEvent {
        id: cause_event(task, revision),
        task_id: task.into(),
        assignment_id: None,
        attempt_id: None,
        kind: "task.waiting_cause".into(),
        payload: json!({"cause": cause, "reason": reason, "revision": revision}),
    };
    append(conn, &cause, now)?;
    Ok(true)
}

/// Why the task waits: the cause recorded with the transition that made it
/// Waiting. None when it is not Waiting, or something else made it wait.
pub(super) fn waiting_cause(conn: &Connection, task: &str) -> Result<Option<String>, TaskError> {
    let tasks = TaskStore { conn };
    if task_or_missing(&tasks, task)?.lifecycle != TaskLifecycle::Waiting {
        return Ok(None);
    }
    let entered: Option<i64> = conn
        .query_row(
            "SELECT json_extract(payload,'$.revision') FROM native_task_events
             WHERE task_id=?1 AND kind='task.waiting' ORDER BY seq DESC LIMIT 1",
            [task],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let Some(entered) = entered.and_then(|r| u64::try_from(r).ok()) else {
        return Ok(None);
    };
    Ok(payload_of(conn, &cause_event(task, entered))?
        .and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok())
        .and_then(|p| p["cause"].as_str().map(str::to_owned)))
}

/// Moves a Waiting task back to Active as the coordinator, clearing the
/// reason.
pub(super) fn activate(conn: &Connection, task: &str, now: u64) -> Result<(), TaskError> {
    let tasks = TaskStore { conn };
    let revision = tasks.revision(Subject::Task(task))?;
    let active = State::Lifecycle(TaskLifecycle::Active);
    apply(
        conn,
        Subject::Task(task),
        active,
        Requester::Coordinator,
        revision,
        now,
    )?;
    conn.execute(
        "UPDATE native_tasks SET blocked_reason=NULL WHERE id=?1",
        [task],
    )?;
    Ok(())
}

/// The open task-wide question that holds the whole task, if one does.
fn task_wide_question(conn: &Connection, task: &str) -> Result<Option<String>, TaskError> {
    Ok(conn
        .query_row(
            "SELECT d.id FROM native_task_decisions d WHERE d.task_id=?1 AND d.scope='task'
               AND d.state='open'
               AND NOT EXISTS(SELECT 1 FROM native_task_assignments a
                 WHERE a.task_id=d.task_id AND a.id=d.assignment_id
                   AND a.status IN ('cancelled','failed'))
             ORDER BY d.rowid LIMIT 1",
            [task],
            |row| row.get(0),
        )
        .optional()?)
}

/// A task-wide open question makes an Active task Waiting; once no such
/// question is open (it was answered), the task is Active again. Running
/// workers are untouched either way, and launches the question holds stay
/// held until its answer reaches the worker (`launch_blocked`).
pub(super) fn sync_waiting(tasks: &TaskStore<'_>, id: &str, now: u64) -> Result<(), TaskError> {
    let current = task_or_missing(tasks, id)?;
    let open = task_wide_question(tasks.conn, id)?;
    match (current.lifecycle, open) {
        (TaskLifecycle::Active, Some(decision)) => {
            let reason = format!("Waiting for decision {decision}, which holds the whole task.");
            let tx = tasks.begin()?;
            wait_for(&tx, id, DECISION, &reason, now)?;
            tx.commit()?;
        }
        (TaskLifecycle::Waiting, None)
            if waiting_cause(tasks.conn, id)?.as_deref() == Some(DECISION) =>
        {
            let tx = tasks.begin()?;
            activate(&tx, id, now)?;
            tx.commit()?;
        }
        _ => {}
    }
    Ok(())
}
