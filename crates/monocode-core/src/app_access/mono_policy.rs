//! Mono's policy commands for its own tasks: read the autonomy policy a
//! task runs under, apply the Mono's current policy to it (the explicit step
//! that lets more authority reach work under way), and unblock it. The
//! calling Mono comes from the bridge's session binding; another Mono's task
//! reads as not found.
use super::mono_tasks::owned_task;
use super::text;
use crate::store::Store;
use crate::tasks::autonomy;
use crate::tasks::{policy, Requester, State, Subject, TaskError, TaskLifecycle};
use serde_json::{json, Value};

pub const MONO_POLICY_ACTIONS: [&str; 3] =
    ["tasks.policy.show", "tasks.policy.apply", "tasks.unblock"];
const OFF: &str =
    "Task coordination is off (native-task-coordinator), so Mono's task actions are unavailable.";

fn failed(error: TaskError) -> String {
    error.to_string()
}

fn revision(input: &Value) -> Result<u64, String> {
    input["revision"]
        .as_u64()
        .ok_or_else(|| "Provide revision (the one you read).".into())
}

/// Runs a policy command for `mono`, the Mono bound to the calling session.
/// The live policy is the one the app published from its preferences.
pub fn mono_policy_action(
    store: &Store,
    mono: &str,
    action: &str,
    input: &Value,
    now: u64,
) -> Result<Value, String> {
    if !MONO_POLICY_ACTIONS.contains(&action) {
        return Err("Unknown task action. Run app help".into());
    }
    crate::mono::api::validate(action, input)?;
    if !crate::tasks::coordinator::enabled(store) {
        return Err(OFF.into());
    }
    let tasks = store.tasks().map_err(failed)?;
    let task = owned_task(&tasks, mono, &text(input, "taskId", 512)?)?;
    let live = autonomy::live(Some(mono), &task.project);
    match action {
        "tasks.policy.show" => {
            let snapshot = policy::of(&tasks, &task.id).map_err(failed)?;
            let effective = autonomy::effective(snapshot.as_ref(), &live);
            Ok(json!({
                "taskId": task.id,
                "lifecycle": task.lifecycle,
                "blockedReason": task.blocked_reason,
                "revision": snapshot.as_ref().map(|s| s.revision),
                "snapshot": snapshot.as_ref().map(|s| &s.autonomy),
                "live": live,
                "effective": effective,
                "differs": snapshot.as_ref().is_some_and(|s| s.autonomy != live),
            }))
        }
        "tasks.policy.apply" => {
            let applied =
                autonomy::apply(store, &task.id, revision(input)?, &live, now).map_err(failed)?;
            Ok(json!({"taskId": task.id, "revision": applied.revision, "policy": applied.autonomy}))
        }
        _ => {
            let revision =
                unblock(store, &task.id, revision(input)?, Requester::Mono, now).map_err(failed)?;
            Ok(json!({"taskId": task.id, "lifecycle": "active", "revision": revision}))
        }
    }
}

/// Moves a Blocked task back to Active for `requester` through the
/// transition table (which clears its blocked reason), so recovery and the
/// coordinator pick it up again. Returns the task's new revision.
pub fn unblock(
    store: &Store,
    task_id: &str,
    revision: u64,
    requester: Requester,
    now: u64,
) -> Result<u64, TaskError> {
    let tasks = store.tasks()?;
    let task = tasks
        .task(task_id)?
        .ok_or_else(|| TaskError::NotFound(format!("task:{task_id}")))?;
    if task.lifecycle != TaskLifecycle::Blocked {
        return Err(TaskError::Invalid(format!(
            "Task {task_id} is not blocked (it is {}).",
            task.lifecycle.as_str()
        )));
    }
    let revision = tasks.transition(
        Subject::Task(task_id),
        State::Lifecycle(TaskLifecycle::Active),
        requester,
        revision,
        now,
    )?;
    // The reason stays in the task's events; an active task shows none.
    store
        .connection()
        .execute(
            "UPDATE native_tasks SET blocked_reason=NULL WHERE id=?1 AND lifecycle='active'",
            [task_id],
        )
        .map_err(TaskError::from)?;
    Ok(revision)
}
