//! The account and billing policy snapshotted into a task when it is
//! created: later changes to the live preference or allowlist do not change
//! how an in-flight task runs or recovers.
//!
//! Stored once as the event `policy:<task>` (write-once by its stable id), so
//! it is as durable and replayable as the rest of the task's history. Account
//! ids only, never keys or tokens.
use super::events::NewEvent;
use super::worker::payload_of;
use super::{NewTask, Task, TaskError, TaskStore};
use crate::accounts::eligibility::{self, AccountPlan, AllowedAccount, Allowlist, Pool};
use crate::provider::Kind;
use crate::store::Store;
use crate::usage::Failure;
use serde::{Deserialize, Serialize};

const POLICY: &str = "task.policy";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskPolicy {
    /// The provider the task's workers start on.
    pub provider: String,
    /// The account selected for the project when the task was created.
    pub start_account: String,
    /// Where recovery may move work; by default only `start_account`.
    pub accounts: Allowlist,
    /// Bill the saved API key instead of the CLI login (the app preference
    /// for `provider` at creation).
    pub use_api_key: bool,
    pub captured_at: u64,
}

fn policy_event(task: &str) -> String {
    format!("policy:{task}")
}

/// The app's live API key preference for `kind`, as its saved workspace
/// state records it (`preferences.api_key_enabled`).
pub fn live_api_key(store: &Store, kind: Kind) -> bool {
    store
        .ui_state()
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|state| state["preferences"]["api_key_enabled"][kind.id()].as_bool())
        .unwrap_or(false)
}

/// The task's snapshot, if it was taken.
pub fn of(tasks: &TaskStore<'_>, task_id: &str) -> Result<Option<TaskPolicy>, TaskError> {
    payload_of(tasks.conn, &policy_event(task_id))?
        .map(|payload| serde_json::from_str(&payload).map_err(TaskError::from))
        .transpose()
}

/// The task's snapshot, taken now from the live settings if it does not
/// exist yet (a task created before snapshots, or by a path that skipped
/// [`create_task`]). Taking it twice keeps the first.
pub fn capture(
    store: &Store,
    task: &Task,
    provider: &str,
    now: u64,
) -> Result<TaskPolicy, TaskError> {
    let tasks = store.tasks()?;
    if let Some(policy) = of(&tasks, &task.id)? {
        return Ok(policy);
    }
    let kind = Kind::parse(provider)
        .ok_or_else(|| TaskError::Invalid(format!("Unknown worker provider {provider}")))?;
    let book = store.provider_accounts().map_err(TaskError::Storage)?;
    let start = book.selected(kind, &task.project);
    let accounts = eligibility::allowlist_for(store, task.mono_id.as_deref(), &task.project)
        .map_err(TaskError::Storage)?
        .unwrap_or_else(|| Allowlist::only(kind.id(), &start));
    let policy = TaskPolicy {
        provider: kind.id().into(),
        start_account: start,
        accounts,
        use_api_key: live_api_key(store, kind),
        captured_at: now,
    };
    let event = NewEvent {
        id: policy_event(&task.id),
        task_id: task.id.clone(),
        assignment_id: None,
        attempt_id: None,
        kind: POLICY.into(),
        payload: serde_json::to_value(&policy)?,
    };
    tasks.append_event(&event, now)?;
    // Whoever wrote first wins.
    of(&tasks, &task.id)?.ok_or_else(|| TaskError::NotFound(policy_event(&task.id)))
}

/// Creates the task and snapshots its policy for workers on `provider`.
pub fn create_task(
    store: &Store,
    new: &NewTask,
    provider: &str,
    now: u64,
) -> Result<Task, TaskError> {
    let task = store.tasks()?.create_task(new, now)?;
    capture(store, &task, provider, now)?;
    Ok(task)
}

impl TaskPolicy {
    /// The entry for the account an attempt ran on.
    pub fn entry(&self, provider: &str, account: Option<&str>) -> AllowedAccount {
        self.accounts
            .entry(provider, account.unwrap_or(&self.start_account))
    }
}

/// Whether the app still knows the account (it may have been removed).
pub(super) fn known(store: &Store) -> Result<impl Fn(&AllowedAccount) -> bool, TaskError> {
    let book = store.provider_accounts().map_err(TaskError::Storage)?;
    Ok(move |account: &AllowedAccount| {
        Kind::parse(&account.provider).is_some_and(|kind| book.exists(kind, &account.account_id))
    })
}

/// Where a fresh launch runs.
pub(super) enum LaunchAccount {
    Use(AllowedAccount),
    /// Every allowed account rests until a known reset; launch later.
    Wait,
    Block(String),
}

/// The account a fresh launch runs on: the highest-priority eligible one of
/// the policy's provider.
pub(super) fn launch_choice(
    store: &Store,
    policy: &TaskPolicy,
    now: u64,
) -> Result<LaunchAccount, TaskError> {
    let health = eligibility::health(store).map_err(TaskError::Storage)?;
    let exists = known(store)?;
    let pool = Pool {
        allowlist: &policy.accounts,
        health: &health,
        exists: &exists,
        exhausted_until: &|_| None,
    };
    if let Some(account) = eligibility::first_eligible(&pool, &policy.provider, None, now) {
        return Ok(LaunchAccount::Use(account));
    }
    let start = policy.entry(&policy.provider, None);
    Ok(
        match eligibility::plan(Failure::UsageExhausted, &start, &pool, None, now) {
            AccountPlan::Wait { .. } => LaunchAccount::Wait,
            AccountPlan::Block(reason) => LaunchAccount::Block(reason),
            AccountPlan::Same | AccountPlan::Switch(_) => {
                LaunchAccount::Block("No allowed account can run this task.".into())
            }
        },
    )
}
