//! Task-level notices for the owning Mono's inbox (lifecycle, restart and
//! steering), and how the inbox describes them with what Mono can do next.
use super::events::{append, NewEvent};
use super::review::MONO_REVIEW;
use super::{Subject, TaskError, TaskStore};
use rusqlite::Connection;
use serde_json::json;

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

/// How Mono's inbox describes a lifecycle notice: what happened and how to
/// act on it. None for other events.
pub(super) fn describe(kind: &str, task: &str, assignment: Option<&str>) -> Option<String> {
    let a = assignment.unwrap_or("...");
    let resume = format!("tasks.resume {{\"taskId\":\"{task}\",\"revision\":<task revision>}}");
    let (what, how) = match kind {
        "recovery.stopped_by_user" => (
            "The user stopped a worker by hand; it is held, not failed",
            format!("Continue it with tasks.resume {{\"taskId\":\"{task}\",\"revision\":<task revision>,\"assignmentIds\":[\"{a}\"]}} (its session is stopped again first), change it with tasks.steer, or end the task with tasks.cancel."),
        ),
        "recovery.workspace_missing" => (
            "A recorded workspace is missing or moved, so the task is blocked",
            "Nothing restarts in a fresh workspace on its own. Ask the user to restore the worktree, or steer the affected assignments with tasks.steer.".into(),
        ),
        "task.restart_held" => (
            "The app restarted; this task waits because its policy does not continue after a restart",
            format!("Continue it with {resume}, or end it with tasks.cancel."),
        ),
        "task.takeover_held" => (
            "This app took over the task from another app or process that may still be running its workers, so they are held and nothing was relaunched",
            format!("Make sure that app has stopped its workers (or has quit), then continue with {resume}; it also continues on its own once that process is gone."),
        ),
        "workspace.owned_elsewhere" => (
            "An assignment keeps waiting for its worktree, which another live worker still works in",
            "It starts on its own once that worker ends. If that worker is stuck, stop it (tasks.pause, or tasks.cancel for its task), or steer this assignment with tasks.steer.".into(),
        ),
        "task.steer_assess" => (
            "The requirements changed and the affected assignments are not known",
            format!("Decide which assignments the new requirements affect and call tasks.steer {{\"taskId\":\"{task}\",\"revision\":<plan revision>,\"requirements\":\"...\",\"assignmentIds\":[...]}} (or pass the revised plan). Nothing was stopped."),
        ),
        _ => return None,
    };
    Some(format!("{what}.\n{how}"))
}
