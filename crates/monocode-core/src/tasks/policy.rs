//! The policy snapshotted into a task when it is created: the billing
//! choice, the starting account and the Mono's autonomy policy
//! ([`super::autonomy`]). A later change to the live API key preference or
//! the Mono's policy does not change an in-flight task, except that reduced
//! authority applies at once and [`super::autonomy::apply`] re-snapshots it
//! on request. The account allowlist is not snapshotted: recovery reads the
//! live Mono or project allowlist each time it decides, so allowing another
//! account and unblocking the task takes effect.
//!
//! Stored as the event `policy:<task>` (write-once by its stable id), and
//! each applied revision as `policy:<task>@<revision>`, so it is as durable
//! and replayable as the rest of the task's history and revisions are
//! recorded, not rewritten. Account ids only, never keys or tokens.
use super::autonomy::{self, Autonomy};
use super::events::NewEvent;
use super::{NewTask, Task, TaskError, TaskStore};
use crate::accounts::eligibility::{self, AccountPlan, AllowedAccount, Allowlist, Pool};
use crate::provider::Kind;
use crate::store::Store;
use crate::usage::Failure;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub(super) const POLICY: &str = "task.policy";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskPolicy {
    /// The provider the task's workers start on.
    pub provider: String,
    /// The account selected for the project when the task was created.
    pub start_account: String,
    /// Bill the saved API key instead of the CLI login (the app preference
    /// for `provider` at creation).
    pub use_api_key: bool,
    pub captured_at: u64,
    /// What the Mono allowed when this was captured (older snapshots, taken
    /// before the policy existed, read as the default).
    #[serde(default)]
    pub autonomy: Autonomy,
    /// 0 when captured; each explicit apply adds one.
    #[serde(default)]
    pub revision: u64,
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

/// The task's snapshot, if it was taken: its latest revision.
pub fn of(tasks: &TaskStore<'_>, task_id: &str) -> Result<Option<TaskPolicy>, TaskError> {
    latest(tasks.conn, task_id)
}

/// The latest snapshot revision on `conn` (also inside a transaction).
pub(super) fn latest(conn: &Connection, task_id: &str) -> Result<Option<TaskPolicy>, TaskError> {
    conn.query_row(
        "SELECT payload FROM native_task_events WHERE task_id=?1 AND kind=?2
           ORDER BY seq DESC LIMIT 1",
        [task_id, POLICY],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(|payload| serde_json::from_str(&payload).map_err(TaskError::from))
    .transpose()
}

/// The task's snapshot, taken now from the live settings and the published
/// policy if it does not exist yet (a task created before snapshots, or by a
/// path that skipped [`create_task`]). Taking it twice keeps the first.
pub fn capture(
    store: &Store,
    task: &Task,
    provider: &str,
    now: u64,
) -> Result<TaskPolicy, TaskError> {
    let live = autonomy::live(task.mono_id.as_deref(), &task.project);
    capture_with(store, task, provider, &live, now)
}

/// As [`capture`], with `live` as the Mono's policy.
pub fn capture_with(
    store: &Store,
    task: &Task,
    provider: &str,
    live: &Autonomy,
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
    let policy = TaskPolicy {
        provider: kind.id().into(),
        start_account: start,
        use_api_key: live
            .api_key_billing
            .unwrap_or_else(|| live_api_key(store, kind)),
        captured_at: now,
        autonomy: live.clone().normalized(),
        revision: 0,
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

/// Creates the task and snapshots its policy for workers on `provider`,
/// with the Mono's policy as the app last published it.
pub fn create_task(
    store: &Store,
    new: &NewTask,
    provider: &str,
    now: u64,
) -> Result<Task, TaskError> {
    let live = autonomy::live(new.mono_id.as_deref(), &new.project);
    create_task_with(store, new, provider, &live, now)
}

/// As [`create_task`], with `live` as the Mono's policy (what the
/// coordinator's [`super::coordinator::Policy::autonomy`] says).
pub fn create_task_with(
    store: &Store,
    new: &NewTask,
    provider: &str,
    live: &Autonomy,
    now: u64,
) -> Result<Task, TaskError> {
    let task = store.tasks()?.create_task(new, now)?;
    capture_with(store, &task, provider, live, now)?;
    Ok(task)
}

/// The live allowlist for the task: the Mono's, else the project's, else
/// only the account the task started on.
pub fn allowlist(store: &Store, task: &Task, policy: &TaskPolicy) -> Result<Allowlist, TaskError> {
    Ok(
        eligibility::allowlist_for(store, task.mono_id.as_deref(), &task.project)
            .map_err(TaskError::Storage)?
            .unwrap_or_else(|| Allowlist::only(&policy.provider, &policy.start_account)),
    )
}

/// Everything recovery chooses accounts from, read once per decision.
pub(super) struct Candidates {
    /// The live allowlist, with provider fallback as the snapshot allows.
    pub allowlist: Allowlist,
    /// The snapshot's retries per lineage.
    pub retry_limit: u32,
    pub health: eligibility::HealthBook,
    exists: Box<dyn Fn(&AllowedAccount) -> bool>,
    start_account: String,
}

impl Candidates {
    pub fn read(store: &Store, task: &Task, policy: &TaskPolicy) -> Result<Self, TaskError> {
        let mut allowlist = allowlist(store, task, policy)?;
        allowlist.provider_fallback = policy.autonomy.provider_fallback;
        Ok(Self {
            allowlist,
            retry_limit: policy.autonomy.retry_limit,
            health: eligibility::health(store).map_err(TaskError::Storage)?,
            exists: Box::new(eligibility::known_accounts(store).map_err(TaskError::Storage)?),
            start_account: policy.start_account.clone(),
        })
    }

    pub fn pool(&self) -> Pool<'_> {
        Pool {
            allowlist: &self.allowlist,
            health: &self.health,
            exists: &*self.exists,
            exhausted_until: &|_| None,
        }
    }

    /// The entry for the account an attempt ran on.
    pub fn entry(&self, provider: &str, account: Option<&str>) -> AllowedAccount {
        self.allowlist
            .entry(provider, account.unwrap_or(&self.start_account))
    }

    pub fn eligible(&self, account: &AllowedAccount, now: u64) -> bool {
        (self.exists)(account)
            && self
                .health
                .get(&account.provider, &account.account_id)
                .available(now)
    }
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
    task: &Task,
    policy: &TaskPolicy,
    now: u64,
) -> Result<LaunchAccount, TaskError> {
    let candidates = Candidates::read(store, task, policy)?;
    let pool = candidates.pool();
    if let Some(account) = eligibility::first_eligible(&pool, &policy.provider, None, now) {
        return Ok(LaunchAccount::Use(account));
    }
    let start = candidates.entry(&policy.provider, None);
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
