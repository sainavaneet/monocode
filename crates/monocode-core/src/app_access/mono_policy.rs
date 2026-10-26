//! Mono's policy commands for its own tasks: read the autonomy policy a
//! task runs under, apply the Mono's current policy to it (the explicit step
//! that lets more authority reach work under way), and unblock it. They pass
//! the same gate as Mono's other task actions: valid fields, task
//! coordination on, and a task the calling Mono owns (another Mono's task
//! reads as not found).
use super::mono_tasks::{failed, owned_task, ready, revision};
use super::text;
use crate::store::Store;
use crate::tasks::autonomy;
use crate::tasks::{policy, Requester};
use serde_json::{json, Value};

pub const MONO_POLICY_ACTIONS: [&str; 3] =
    ["tasks.policy.show", "tasks.policy.apply", "tasks.unblock"];

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
    ready(store, action, input)?;
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
            let prepared = (
                tasks.unblock_check(&task.id).map_err(failed)?,
                revision(input)?,
            );
            let problems = prepared.0.problems();
            mono_unblock_finish(store, &prepared, &problems, now)
        }
    }
}

/// `tasks.unblock` in steps, so the app checks the task's worktrees with
/// Git without holding the store lock: this step (under the store) passes
/// the same gate as [`mono_policy_action`] and collects what to check.
pub fn mono_unblock_prepare(
    store: &Store,
    mono: &str,
    input: &Value,
) -> Result<MonoUnblock, String> {
    const ACTION: &str = "tasks.unblock";
    ready(store, ACTION, input)?;
    let tasks = store.tasks().map_err(failed)?;
    let task = owned_task(&tasks, mono, &text(input, "taskId", 512)?)?;
    Ok((
        tasks.unblock_check(&task.id).map_err(failed)?,
        revision(input)?,
    ))
}

/// The unblock check and the task revision Mono read.
pub type MonoUnblock = (crate::tasks::workspace_check::UnblockCheck, u64);

/// The last step of `tasks.unblock`, under the store again, with the Git
/// check's `problems`.
pub fn mono_unblock_finish(
    store: &Store,
    (check, expected): &MonoUnblock,
    problems: &[crate::tasks::workspace_check::Problem],
    now: u64,
) -> Result<Value, String> {
    let tasks = store.tasks().map_err(failed)?;
    let revision = tasks
        .unblock_checked(check, problems, *expected, Requester::Mono, now)
        .map_err(failed)?;
    Ok(json!({"taskId": check.task_id(), "lifecycle": "active", "revision": revision}))
}
