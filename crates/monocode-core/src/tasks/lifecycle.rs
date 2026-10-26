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
//! Cancelling, and nothing new starts in its workspaces (`guard`).
use super::events::{append, NewEvent};
use super::guard::{may_be_running, request_stop, task_or_missing, task_stopping};
use super::park::{move_rows, park, WAITING_ROWS};
use super::recovery::{stop_delivery, HELD, SCHEDULED, SUPERSEDED};
use super::states::apply;
use super::waiting::{waiting_cause, RESTART, TAKEOVER};
use super::worker::settle;
use super::{
    AssignmentState, Attempt, AttemptState, Requester, State, Subject, Task, TaskError,
    TaskLifecycle, TaskStore,
};
use crate::store::Store;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// The recovery category of an attempt a pause interrupted.
pub const PAUSED: &str = "paused";
const PAUSE_REASON: &str = "The task was paused while this worker ran.";

/// What the task shows: its lifecycle, with Pausing and Cancelling while a
/// worker it stopped has not confirmed it stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Whether `tasks.resume` applies: paused, or held after a restart or a
    /// takeover (not a wait on a question, not a block).
    pub resumable: bool,
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

/// The record of who asked for a control and what it did; `key` makes it
/// unique per request.
fn control_event(task: &str, kind: &str, key: &str, payload: serde_json::Value) -> NewEvent {
    NewEvent {
        id: format!("{kind}:{task}@{key}"),
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
        if may_be_running(&attempt) && inside.claim(&attempt.id)?.is_none() {
            park(&tx, &attempt, PAUSED, PAUSE_REASON, SCHEDULED, true, now)?;
            stopping.push(attempt.id);
        }
    }
    let payload = json!({"requester": requester, "stopping": stopping});
    let event = control_event(id, "task.pause_requested", &revision.to_string(), payload);
    append(&tx, &event, now)?;
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
    let event = control_event(id, "task.cancel_requested", &revision.to_string(), payload);
    append(&tx, &event, now)?;
    tx.commit()?;
    controlled(&tasks, id, stopping, None)
}

/// Cancels a live attempt and asks the app to stop its session when it may
/// be running. Returns whether a stop was requested.
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
    let subject = Subject::Attempt(&attempt.id);
    settle(conn, subject, cancelled, Requester::Coordinator, now)?;
    if !may_be_running(attempt) {
        return Ok(false);
    }
    let id = format!("cancelled:{}", attempt.id);
    request_stop(conn, attempt, id, &stop_delivery(&attempt.id), now)
}

/// Whether resuming applies to the task as it is: paused, or waiting
/// because a restart or a takeover held its workers.
fn reopens(conn: &Connection, task: &Task) -> Result<bool, TaskError> {
    Ok(match task.lifecycle {
        TaskLifecycle::Paused => true,
        TaskLifecycle::Waiting => matches!(
            waiting_cause(conn, &task.id)?.as_deref(),
            Some(RESTART | TAKEOVER)
        ),
        _ => false,
    })
}

/// Resumes a paused task (or one held after a restart or a takeover): the
/// recorded workspaces are checked first, then in one transaction the task
/// is Active again, held workers are carried on, every interrupted worker's
/// session is stopped again (the user may have typed into it since; nothing
/// launches until that stop is confirmed), and a missing or moved
/// workspace blocks the task with the recovery condition instead. The
/// coordinator then resumes only eligible work. With `assignments`, an
/// Active task's workers the user stopped by hand are carried on instead.
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
    let current = task_or_missing(&tasks, id)?;
    if current.revision != revision {
        return Err(TaskError::Stale {
            subject: format!("task:{id}"),
            expected: revision,
            actual: current.revision,
        });
    }
    let reopen = reopens(tasks.conn, &current)?;
    if !reopen && current.lifecycle != TaskLifecycle::Active {
        return Err(TaskError::InvalidState {
            subject: format!("task:{id}"),
            state: current.lifecycle.as_str().into(),
        });
    }
    for assignment in assignments {
        if tasks.assignment(id, assignment)?.is_none() {
            return Err(TaskError::NotFound(format!("assignment:{assignment}")));
        }
    }
    // Git runs before the write lock is taken; the result is applied in
    // the same transaction as the resume.
    let problems = super::workspace_check::verify(&tasks, &current)?;
    let tx = tasks.begin()?;
    if reopen {
        let active = State::Lifecycle(TaskLifecycle::Active);
        apply(&tx, Subject::Task(id), active, requester, revision, now)?;
        tx.execute(
            "UPDATE native_tasks SET blocked_reason=NULL WHERE id=?1",
            [id],
        )?;
    }
    let mut released = Vec::new();
    if assignments.is_empty() {
        released.extend(move_rows(&tx, id, None, &[HELD], SCHEDULED, now)?);
    }
    for assignment in assignments {
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
    let key = crate::mono::new_id();
    restop_interrupted(&tx, id, assignments, &key, now)?;
    let blocked = if problems.is_empty() {
        None
    } else {
        Some(super::workspace_check::record_condition(
            &tx, id, &problems, now,
        )?)
    };
    let payload = json!({"requester": requester, "released": released, "blocked": blocked});
    let event = control_event(id, "task.resume_requested", &key, payload);
    append(&tx, &event, now)?;
    tx.commit()?;
    controlled(&tasks, id, released, blocked)
}

/// Asks the app to stop, again, the session of every interrupted latest
/// worker (of `assignments` only, when given), so a session the user kept
/// driving cannot share the workspace with what resumes.
fn restop_interrupted(
    conn: &Connection,
    task: &str,
    assignments: &[String],
    key: &str,
    now: u64,
) -> Result<(), TaskError> {
    let inside = TaskStore { conn };
    for assignment in inside.assignments(task)? {
        if !assignments.is_empty() && !assignments.contains(&assignment.id) {
            continue;
        }
        let Some(attempt) = inside.attempts(task, &assignment.id)?.pop() else {
            continue;
        };
        if attempt.status == AttemptState::Interrupted {
            let id = format!("resume-stop:{}:{key}", attempt.id);
            let delivery = format!("stop-resume:{}:{key}", attempt.id);
            request_stop(conn, &attempt, id, &delivery, now)?;
        }
    }
    Ok(())
}

fn controlled(
    tasks: &TaskStore<'_>,
    id: &str,
    attempts: Vec<String>,
    blocked: Option<String>,
) -> Result<Controlled, TaskError> {
    let task = task_or_missing(tasks, id)?;
    Ok(Controlled {
        task_id: task.id.clone(),
        revision: task.revision,
        display: display(tasks, &task)?,
        attempts,
        blocked,
    })
}

/// What the task shows now (see [`Display`]).
pub fn display(tasks: &TaskStore<'_>, task: &Task) -> Result<Display, TaskError> {
    Ok(match task.lifecycle {
        TaskLifecycle::Active => Display::Active,
        TaskLifecycle::Waiting => Display::Waiting,
        TaskLifecycle::Blocked => Display::Blocked,
        TaskLifecycle::Completed => Display::Completed,
        TaskLifecycle::Paused if task_stopping(tasks, &task.id)? => Display::Pausing,
        TaskLifecycle::Paused => Display::Paused,
        TaskLifecycle::Cancelled if task_stopping(tasks, &task.id)? => Display::Cancelling,
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
        let task = task_or_missing(&tasks, &id)?;
        let display = display(&tasks, &task)?;
        if display == Display::Cancelled {
            continue;
        }
        let reason = match display {
            Display::Blocked | Display::Waiting => task.blocked_reason.clone(),
            _ => None,
        };
        rows.push(ControlRow {
            resumable: reopens(tasks.conn, &task)?,
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
    let Ok(bound) = store.persisted_session_id(session) else {
        return Ok(None);
    };
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
    let title = task_or_missing(&tasks, &attempt.task_id)?.title;
    Ok(Some(format!(
        "Detached from task \"{title}\": this worker {how}, so nothing done here reports to the task or changes it."
    )))
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
