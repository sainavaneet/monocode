//! Pause, resume and cancel, as the user or Mono asks for them, and what the
//! task's lifecycle shows while they take effect.
//!
//! Each control persists its intent in one transaction before anything
//! external happens: the lifecycle moves through the transition table, and
//! every worker that is still running is moved first (Interrupted for a
//! pause, Cancelled for a cancel) and only then asked to stop through the
//! stop path. So a stopped worker's turn ending is never read as a failure,
//! and a late message from it changes nothing. Pausing keeps everything:
//! each interrupted worker gets a recovery row that resuming carries out on
//! the same lineage and account (the provider thread is resumed when
//! possible, otherwise a replacement starts from a durable hand-off).
//! Cancelling keeps every change and piece of evidence but ends retries and
//! recovery for good. While a stop is unconfirmed the task shows Pausing or
//! Cancelling.
use super::events::{append, NewEvent};
use super::park::{move_rows, park, WAITING_ROWS};
use super::recovery::{stop_delivery, HELD, SCHEDULED, SUPERSEDED, WORKER_STOP};
use super::review::MONO_REVIEW;
use super::states::apply;
use super::worker::{event, settle};
use super::{
    AssignmentState, Attempt, AttemptState, Requester, State, Subject, Task, TaskError,
    TaskLifecycle, TaskStore,
};
use crate::store::Store;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use serde_json::json;

/// The recovery category of an attempt a pause interrupted.
pub const PAUSED: &str = "paused";
const PAUSE_REASON: &str = "The task was paused while this worker ran.";
/// The waiting cause of a task-wide open question.
const DECISION: &str = "decision";

/// What the task shows: its lifecycle, with Pausing and Cancelling while a
/// worker it stopped has not confirmed it stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Display {
    Active,
    Waiting,
    Pausing,
    Paused,
    Blocked,
    Cancelling,
    Cancelled,
    Completed,
}

impl Display {
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::Waiting => "Waiting",
            Self::Pausing => "Pausing",
            Self::Paused => "Paused",
            Self::Blocked => "Blocked",
            Self::Cancelling => "Cancelling",
            Self::Cancelled => "Cancelled",
            Self::Completed => "Completed",
        }
    }
}

/// One task as its controls show it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlRow {
    pub task_id: String,
    pub title: String,
    pub revision: u64,
    pub display: Display,
    /// Why it waits or is blocked.
    pub reason: Option<String>,
}

/// What a pause, resume or cancel did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Controlled {
    pub task_id: String,
    pub revision: u64,
    pub display: Display,
    /// Attempts asked to stop (pause, cancel) or carried on (resume).
    pub attempts: Vec<String>,
    /// Why resuming could not continue, when it blocked instead.
    pub blocked: Option<String>,
}

fn only_user_or_mono(requester: Requester) -> Result<(), TaskError> {
    if matches!(requester, Requester::User | Requester::Mono) {
        return Ok(());
    }
    Err(TaskError::Invalid(
        "Only the user or Mono pauses, resumes, cancels or steers a task.".into(),
    ))
}

fn task(tasks: &TaskStore<'_>, id: &str) -> Result<Task, TaskError> {
    tasks
        .task(id)?
        .ok_or_else(|| TaskError::NotFound(format!("task:{id}")))
}

/// Whether the attempt's worker may still be running: a session that was
/// handed out, or a live state. An attempt without a session never ran.
fn running(attempt: &Attempt) -> bool {
    match attempt.status {
        AttemptState::Running | AttemptState::Waiting => true,
        AttemptState::Starting => attempt.session_id.is_some(),
        _ => false,
    }
}

/// The record of who asked for a control and what it stopped.
fn control_event(task: &str, kind: &str, revision: u64, payload: serde_json::Value) -> NewEvent {
    NewEvent {
        id: format!("{kind}:{task}@{revision}"),
        task_id: task.into(),
        assignment_id: None,
        attempt_id: None,
        kind: kind.into(),
        payload,
    }
}

/// Pauses the task: the intent is persisted at once, new launches stop
/// (the coordinator only launches for Active tasks), and every running
/// worker is Interrupted with a recovery row before its stop is requested.
pub fn pause(
    store: &Store,
    id: &str,
    revision: u64,
    requester: Requester,
    now: u64,
) -> Result<Controlled, TaskError> {
    only_user_or_mono(requester)?;
    let tasks = store.tasks()?;
    let tx = tasks.begin()?;
    let inside = TaskStore { conn: &tx };
    let paused = State::Lifecycle(TaskLifecycle::Paused);
    let revision = apply(&tx, Subject::Task(id), paused, requester, revision, now)?;
    let mut stopping = Vec::new();
    for assignment in inside.assignments(id)? {
        let Some(attempt) = inside.attempts(id, &assignment.id)?.pop() else {
            continue;
        };
        if running(&attempt) && inside.claim(&attempt.id)?.is_none() {
            park(&tx, &attempt, PAUSED, PAUSE_REASON, SCHEDULED, true, now)?;
            stopping.push(attempt.id);
        }
    }
    let payload = json!({"requester": requester, "stopping": stopping});
    append(
        &tx,
        &control_event(id, "task.pause_requested", revision, payload),
        now,
    )?;
    tx.commit()?;
    controlled(&tasks, id, stopping, None)
}

/// Cancels the task for good: the intent is persisted at once; launches,
/// retries and recovery end; every unfinished assignment and its live
/// attempt are Cancelled before the attempt's stop is requested. Worktrees,
/// branches, claims and verifications stay; a late result is recorded in
/// the history and changes nothing.
pub fn cancel(
    store: &Store,
    id: &str,
    revision: u64,
    requester: Requester,
    now: u64,
) -> Result<Controlled, TaskError> {
    only_user_or_mono(requester)?;
    let tasks = store.tasks()?;
    let tx = tasks.begin()?;
    let inside = TaskStore { conn: &tx };
    let cancelled = State::Lifecycle(TaskLifecycle::Cancelled);
    let revision = apply(&tx, Subject::Task(id), cancelled, requester, revision, now)?;
    move_rows(&tx, id, None, &WAITING_ROWS, SUPERSEDED, now)?;
    let mut stopping = Vec::new();
    for assignment in inside.assignments(id)? {
        if let Some(attempt) = inside.attempts(id, &assignment.id)?.pop() {
            if cancel_attempt(&tx, &attempt, now)? {
                stopping.push(attempt.id);
            }
        }
        let to = State::Assignment(AssignmentState::Cancelled);
        let subject = Subject::Assignment {
            task: id,
            id: &assignment.id,
        };
        if super::authorize(State::Assignment(assignment.status), to, requester).is_ok() {
            settle(&tx, subject, to, requester, now)?;
        }
    }
    let payload = json!({"requester": requester, "stopping": stopping});
    append(
        &tx,
        &control_event(id, "task.cancel_requested", revision, payload),
        now,
    )?;
    tx.commit()?;
    controlled(&tasks, id, stopping, None)
}

/// Cancels a live attempt and asks the app to stop its session. Returns
/// whether a stop was requested.
pub(super) fn cancel_attempt(
    conn: &Connection,
    attempt: &Attempt,
    now: u64,
) -> Result<bool, TaskError> {
    let live = matches!(
        attempt.status,
        AttemptState::Created
            | AttemptState::Starting
            | AttemptState::Running
            | AttemptState::Waiting
    );
    if !live {
        return Ok(false);
    }
    let cancelled = State::Attempt(AttemptState::Cancelled);
    settle(
        conn,
        Subject::Attempt(&attempt.id),
        cancelled,
        Requester::Coordinator,
        now,
    )?;
    let Some(session) = attempt.session_id.as_ref().filter(|_| running(attempt)) else {
        return Ok(false);
    };
    let payload = json!({"sessionId": session});
    let stored = append(
        conn,
        &event(
            attempt,
            format!("cancelled:{}", attempt.id),
            "attempt.stop_requested",
            payload,
        ),
        now,
    )?;
    TaskStore { conn }.enqueue_delivery(
        &stop_delivery(&attempt.id),
        &stored.id,
        WORKER_STOP,
        &format!("session:{session}"),
        now,
    )?;
    Ok(true)
}

/// Resumes a paused task (or one held after a restart): the task is Active
/// again, recorded workspaces are checked first (a missing or moved one
/// blocks it with the recovery condition instead), held workers are carried
/// on, and the coordinator then resumes only eligible work: replacements for
/// the workers the pause stopped, and launches the scheduler allows. With
/// `assignments`, an Active task's workers the user stopped by hand are
/// carried on instead.
pub fn resume(
    store: &Store,
    id: &str,
    revision: u64,
    requester: Requester,
    assignments: &[String],
    now: u64,
) -> Result<Controlled, TaskError> {
    only_user_or_mono(requester)?;
    let tasks = store.tasks()?;
    let current = task(&tasks, id)?;
    if current.revision != revision {
        return Err(TaskError::Stale {
            subject: format!("task:{id}"),
            expected: revision,
            actual: current.revision,
        });
    }
    let held_after_restart = current.lifecycle == TaskLifecycle::Waiting
        && waiting_cause(tasks.conn, id)?.as_deref() == Some(super::restart::RESTART);
    let reopen = current.lifecycle == TaskLifecycle::Paused || held_after_restart;
    if !reopen && current.lifecycle != TaskLifecycle::Active {
        return Err(TaskError::InvalidState {
            subject: format!("task:{id}"),
            state: current.lifecycle.as_str().into(),
        });
    }
    // Reconcile the workspaces before anything runs in them again.
    let problems = super::restart::verify_workspaces(&tasks, &current)?;
    let tx = tasks.begin()?;
    if reopen {
        let active = State::Lifecycle(TaskLifecycle::Active);
        apply(&tx, Subject::Task(id), active, requester, revision, now)?;
        tx.execute(
            "UPDATE native_tasks SET blocked_reason=NULL WHERE id=?1",
            [id],
        )?;
    }
    let inside = TaskStore { conn: &tx };
    let mut released = Vec::new();
    if assignments.is_empty() {
        released.extend(move_rows(&tx, id, None, &[HELD], SCHEDULED, now)?);
    }
    for assignment in assignments {
        if inside.assignment(id, assignment)?.is_none() {
            return Err(TaskError::NotFound(format!("assignment:{assignment}")));
        }
        released.extend(move_rows(
            &tx,
            id,
            Some(assignment),
            &[HELD],
            SCHEDULED,
            now,
        )?);
    }
    if !reopen && released.is_empty() {
        return Err(TaskError::Invalid(
            "The task is not paused and has no held worker to resume.".into(),
        ));
    }
    let revision = inside.revision(Subject::Task(id))?;
    let payload = json!({"requester": requester, "released": released});
    append(
        &tx,
        &control_event(id, "task.resume_requested", revision, payload),
        now,
    )?;
    tx.commit()?;
    let blocked = if problems.is_empty() {
        None
    } else {
        let now_task = task(&tasks, id)?;
        Some(super::restart::recovery_condition(
            &tasks, &now_task, &problems, now,
        )?)
    };
    controlled(&tasks, id, released, blocked)
}

fn controlled(
    tasks: &TaskStore<'_>,
    id: &str,
    attempts: Vec<String>,
    blocked: Option<String>,
) -> Result<Controlled, TaskError> {
    let task = task(tasks, id)?;
    Ok(Controlled {
        task_id: task.id.clone(),
        revision: task.revision,
        display: display(tasks, &task)?,
        attempts,
        blocked,
    })
}

/// Whether a stop the task asked for is still unconfirmed, or a worker of
/// it may still run.
fn stopping(tasks: &TaskStore<'_>, task: &str) -> Result<bool, TaskError> {
    Ok(tasks.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM native_task_deliveries d JOIN native_task_events e ON e.id=d.event_id
           WHERE e.task_id=?1 AND d.destination=?2 AND d.state<>'acknowledged')
         OR EXISTS(SELECT 1 FROM native_task_attempts WHERE task_id=?1
           AND (status IN ('running','waiting') OR (status='starting' AND session_id IS NOT NULL)))",
        rusqlite::params![task, WORKER_STOP],
        |row| row.get(0),
    )?)
}

/// What the task shows now (see [`Display`]).
pub fn display(tasks: &TaskStore<'_>, task: &Task) -> Result<Display, TaskError> {
    Ok(match task.lifecycle {
        TaskLifecycle::Active => Display::Active,
        TaskLifecycle::Waiting => Display::Waiting,
        TaskLifecycle::Blocked => Display::Blocked,
        TaskLifecycle::Completed => Display::Completed,
        TaskLifecycle::Paused if stopping(tasks, &task.id)? => Display::Pausing,
        TaskLifecycle::Paused => Display::Paused,
        TaskLifecycle::Cancelled if stopping(tasks, &task.id)? => Display::Cancelling,
        TaskLifecycle::Cancelled => Display::Cancelled,
    })
}

/// The controls of `mono`'s tasks that are not finished (a cancelled task
/// stays listed while it is still Cancelling).
pub fn controls(store: &Store, mono: &str) -> Result<Vec<ControlRow>, TaskError> {
    if !super::coordinator::enabled(store) {
        return Ok(Vec::new());
    }
    let Some(tasks) = store.existing_tasks()? else {
        return Ok(Vec::new());
    };
    let mut statement = tasks.conn.prepare(
        "SELECT id FROM native_tasks WHERE mono_id=?1 AND lifecycle<>'completed' ORDER BY rowid",
    )?;
    let ids = statement
        .query_map([mono], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut rows = Vec::new();
    for id in ids {
        let task = task(&tasks, &id)?;
        let display = display(&tasks, &task)?;
        if display == Display::Cancelled {
            continue;
        }
        let reason = match display {
            Display::Blocked | Display::Waiting => task.blocked_reason.clone(),
            _ => None,
        };
        rows.push(ControlRow {
            task_id: task.id,
            title: task.title,
            revision: task.revision,
            display,
            reason,
        });
    }
    Ok(rows)
}

/// What a worker session shows once it no longer belongs to its task: the
/// user or the task stopped it, it failed, or a newer attempt replaced it.
/// Anything typed there afterwards never reports to the task.
pub fn detached(store: &Store, session: i64) -> Result<Option<String>, TaskError> {
    if !super::coordinator::enabled(store) {
        return Ok(None);
    }
    let Some(tasks) = store.existing_tasks()? else {
        return Ok(None);
    };
    let bound = store
        .persisted_session_id(session)
        .map_err(TaskError::Storage)?;
    let Some(attempt) = tasks.attempt_for_session(&bound)? else {
        return Ok(None);
    };
    let latest = super::states::is_latest_attempt(tasks.conn, Subject::Attempt(&attempt.id))?;
    let how = match attempt.status {
        AttemptState::Interrupted => "was stopped",
        AttemptState::Cancelled => "was cancelled",
        AttemptState::Failed => "failed",
        _ if !latest => "was replaced by a newer worker",
        _ => return Ok(None),
    };
    let title = task(&tasks, &attempt.task_id)?.title;
    Ok(Some(format!(
        "Detached from task \"{title}\": this worker {how}, so nothing done here reports to the task or changes it."
    )))
}

/// Queues a notice for the task's Mono, once per kind and task revision.
pub(super) fn notify(
    conn: &Connection,
    task: &str,
    assignment: Option<&str>,
    kind: &str,
    reason: &str,
    now: u64,
) -> Result<(), TaskError> {
    let tasks = TaskStore { conn };
    let revision = tasks.revision(Subject::Task(task))?;
    let id = format!("{kind}:{task}@{revision}");
    let notice = NewEvent {
        id: id.clone(),
        task_id: task.into(),
        assignment_id: assignment.map(str::to_owned),
        attempt_id: None,
        kind: kind.into(),
        payload: json!({"reason": reason}),
    };
    append(conn, &notice, now)?;
    let reference = format!("task:{task}");
    tasks.enqueue_delivery(&format!("mono-{id}"), &id, MONO_REVIEW, &reference, now)?;
    Ok(())
}

/// Moves an Active task to Waiting for `cause`, with the reason shown.
/// Returns whether it moved.
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
    let payload = json!({"cause": cause, "reason": reason});
    append(
        conn,
        &control_event(task, "task.waiting_cause", revision, payload),
        now,
    )?;
    Ok(true)
}

/// Why the task waits, when this module made it wait and nothing moved its
/// lifecycle since: the latest lifecycle event is the cause it recorded.
pub(super) fn waiting_cause(conn: &Connection, task: &str) -> Result<Option<String>, TaskError> {
    let latest: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT kind,json_extract(payload,'$.cause') FROM native_task_events
             WHERE task_id=?1 AND kind IN ('task.waiting_cause','task.waiting','task.active',
               'task.resumed','task.paused','task.blocked','task.cancelled','task.completed')
             ORDER BY seq DESC LIMIT 1",
            [task],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(latest
        .filter(|(kind, _)| kind == "task.waiting_cause")
        .and_then(|(_, cause)| cause))
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
    let current = task(tasks, id)?;
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
            let active = State::Lifecycle(TaskLifecycle::Active);
            apply(
                &tx,
                Subject::Task(id),
                active,
                Requester::Coordinator,
                current.revision,
                now,
            )?;
            tx.execute(
                "UPDATE native_tasks SET blocked_reason=NULL WHERE id=?1",
                [id],
            )?;
            tx.commit()?;
        }
        _ => {}
    }
    Ok(())
}

/// How Mono's inbox describes a lifecycle notice: what happened and how to
/// act on it. None for other events.
pub(super) fn describe(kind: &str, task: &str, assignment: Option<&str>) -> Option<String> {
    let a = assignment.unwrap_or("...");
    let (what, how) = match kind {
        "recovery.stopped_by_user" => (
            "The user stopped a worker by hand; it is held, not failed",
            format!("Continue it with tasks.resume {{\"taskId\":\"{task}\",\"revision\":<task revision>,\"assignmentIds\":[\"{a}\"]}}, change it with tasks.steer, or end the task with tasks.cancel."),
        ),
        "recovery.workspace_missing" => (
            "A recorded workspace is missing or moved, so the task is blocked",
            "Nothing restarts in a fresh workspace on its own. Ask the user to restore the worktree, or steer the affected assignments with tasks.steer.".into(),
        ),
        "task.restart_held" => (
            "The app restarted; this task waits because its policy does not continue after a restart",
            format!("Continue it with tasks.resume {{\"taskId\":\"{task}\",\"revision\":<task revision>}}, or end it with tasks.cancel."),
        ),
        "task.steer_assess" => (
            "The requirements changed and the affected assignments are not known",
            format!("Decide which assignments the new requirements affect and call tasks.steer {{\"taskId\":\"{task}\",\"revision\":<task revision>,\"requirements\":\"...\",\"assignmentIds\":[...]}} (or pass the revised plan). Nothing was stopped."),
        ),
        _ => return None,
    };
    Some(format!("{what}.\n{how}"))
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
