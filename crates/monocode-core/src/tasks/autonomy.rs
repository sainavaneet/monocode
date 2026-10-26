//! What a Mono lets its tasks do on their own: the autonomy policy.
//!
//! Each Mono has a policy with optional per-project overrides. The app keeps
//! it in its preferences and publishes it here ([`publish`]) whenever they
//! change, so the coordinator reads the live policy without the GUI.
//!
//! A task captures the effective policy when it is created (or, for a task
//! created without one, when it first launches) into its snapshot
//! ([`super::policy::TaskPolicy`]). Later changes reach a task in two ways:
//! - Reductions apply at once. Every decision point (the coordinator,
//!   review and recovery) combines the snapshot with the live policy and
//!   keeps the more restrictive of the two for automatic execution, worker
//!   concurrency, commit/merge/push/deploy authority, restart continuation,
//!   provider fallback and the retry limit ([`Autonomy::restricted_by`]).
//!   Running workers are never stopped by this; new disallowed actions are,
//!   and while commit is revoked, review sends back work whose assignment
//!   branch has commits since its base.
//! - Everything else (more authority, the workspace preference, worktree
//!   retention, API key billing) reaches an active task only through the
//!   explicit [`apply`] step, which re-snapshots the live policy into the
//!   task with a revision bump. Billing is never a reduction: which account
//!   bills an in-flight task does not change under it.
//!
//! The account fallback list is not part of the policy: recovery reads the
//! live allowlist (`accounts::eligibility`) each time it decides.
use super::coordinator::{Coordinator, Policy};
use super::events::append;
use super::policy::{self, TaskPolicy};
use super::recovery::RETRY_BUDGET;
use super::{NewEvent, Task, TaskError, TaskLifecycle, TaskStore};
use crate::store::Store;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;

/// The most workers one task may run at once.
pub const MAX_WORKERS: u32 = 8;
/// The most retries one attempt lineage may spend.
pub const MAX_RETRIES: u32 = 10;

/// Where an assignment works.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkspacePreference {
    /// A worktree for work that changes files; investigations read the
    /// checkout (task-4's rule).
    #[default]
    Automatic,
    /// A dedicated worktree for every assignment, investigations included.
    Worktree,
    /// The current checkout. Read-only work runs there; work that would
    /// change files is refused rather than written into the user's checkout.
    Checkout,
}

/// What happens to a task's worktrees once it is done. Nothing deletes a
/// dirty worktree, whatever this says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Retention {
    #[default]
    Keep,
    /// Clean worktrees may be removed after the task completes.
    RemoveClean,
}

/// Steps that change history outside the task's own worktree, or ship it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Authority {
    Commit,
    Merge,
    Push,
    Deploy,
}

impl Authority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Merge => "merge",
            Self::Push => "push",
            Self::Deploy => "deploy",
        }
    }
}

/// One Mono's policy (or a project's override of it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Autonomy {
    /// Planned tasks start executing without an explicit start.
    pub automatic_execution: bool,
    pub workspace: WorkspacePreference,
    /// Workers one task may run at once.
    pub worker_limit: u32,
    /// Recovery may move work to another provider's allowed account.
    pub provider_fallback: bool,
    /// Retries per attempt lineage, whatever the account changes.
    pub retry_limit: u32,
    /// Workers commit in the task's own worktree.
    pub commit: bool,
    pub merge: bool,
    pub push: bool,
    pub deploy: bool,
    /// Work interrupted by an app restart continues on its own.
    pub restart_continuation: bool,
    pub worktree_retention: Retention,
    /// Bill the saved API key (true) or the login (false); None follows the
    /// app's preference for the provider.
    pub api_key_billing: Option<bool>,
}

impl Default for Autonomy {
    fn default() -> Self {
        Self {
            automatic_execution: false,
            workspace: WorkspacePreference::Automatic,
            worker_limit: 1,
            provider_fallback: false,
            retry_limit: RETRY_BUDGET,
            commit: true,
            merge: false,
            push: false,
            deploy: false,
            restart_continuation: true,
            worktree_retention: Retention::Keep,
            api_key_billing: None,
        }
    }
}

impl Autonomy {
    /// Numbers clamped to what the coordinator accepts.
    pub fn normalized(mut self) -> Self {
        self.worker_limit = self.worker_limit.clamp(1, MAX_WORKERS);
        self.retry_limit = self.retry_limit.min(MAX_RETRIES);
        self
    }

    pub fn allows(&self, authority: Authority) -> bool {
        match authority {
            Authority::Commit => self.commit,
            Authority::Merge => self.merge,
            Authority::Push => self.push,
            Authority::Deploy => self.deploy,
        }
    }

    /// This snapshot as `live` still allows it: authority the live policy
    /// took away is gone at once; authority it added waits for [`apply`].
    pub fn restricted_by(&self, live: &Autonomy) -> Autonomy {
        Autonomy {
            automatic_execution: self.automatic_execution && live.automatic_execution,
            worker_limit: self.worker_limit.min(live.worker_limit.max(1)),
            commit: self.commit && live.commit,
            merge: self.merge && live.merge,
            push: self.push && live.push,
            deploy: self.deploy && live.deploy,
            restart_continuation: self.restart_continuation && live.restart_continuation,
            provider_fallback: self.provider_fallback && live.provider_fallback,
            retry_limit: self.retry_limit.min(live.retry_limit),
            ..self.clone()
        }
    }
}

/// A Mono's policy and its per-project overrides (keyed like account
/// selections, so trailing slashes do not matter).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MonoAutonomy {
    pub policy: Autonomy,
    pub projects: BTreeMap<String, Autonomy>,
}

impl MonoAutonomy {
    /// The policy for work in `project`.
    pub fn for_project(&self, project: &str) -> &Autonomy {
        self.projects
            .get(&crate::accounts::project_key(project))
            .unwrap_or(&self.policy)
    }

    pub fn has_override(&self, project: &str) -> bool {
        self.projects
            .contains_key(&crate::accounts::project_key(project))
    }

    /// Sets (Some) or removes (None) the override for `project`.
    pub fn set_project(&mut self, project: &str, policy: Option<Autonomy>) {
        let key = crate::accounts::project_key(project);
        match policy {
            Some(policy) => {
                self.projects.insert(key, policy);
            }
            None => {
                self.projects.remove(&key);
            }
        }
    }
}

/// The policies the app last published, by Mono id.
static LIVE: RwLock<Option<HashMap<String, MonoAutonomy>>> = RwLock::new(None);

/// Serializes tests that publish or read published policies (the registry
/// is process-wide). Publishing tests use Mono ids no other test owns, so a
/// reader of another Mono (such as the fixtures' `mono-1`) always sees the
/// default whatever is published.
/// It also empties the registry when dropped (still holding the lock),
/// even when the test fails.
#[cfg(test)]
pub(crate) fn tests_lock() -> Published {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    Published(LOCK.lock().unwrap_or_else(|e| e.into_inner()))
}

#[cfg(test)]
pub(crate) struct Published(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

#[cfg(test)]
impl Drop for Published {
    fn drop(&mut self) {
        publish(HashMap::new());
    }
}

/// Replaces the live policies (the app calls this whenever its preferences
/// change).
pub fn publish(policies: HashMap<String, MonoAutonomy>) {
    let mut live = LIVE.write().unwrap_or_else(|e| e.into_inner());
    *live = Some(policies);
}

/// The live policy for `mono`'s work in `project`; the default for work no
/// Mono owns or a Mono the app published nothing for.
pub fn live(mono: Option<&str>, project: &str) -> Autonomy {
    let Some(mono) = mono else {
        return Autonomy::default();
    };
    let live = LIVE.read().unwrap_or_else(|e| e.into_inner());
    live.as_ref()
        .and_then(|policies| policies.get(mono))
        .map(|policy| policy.for_project(project).clone().normalized())
        .unwrap_or_default()
}

/// The policy that governs `task` now: its snapshot as the live policy
/// still allows it, or the live policy before any snapshot exists.
pub fn effective(snapshot: Option<&TaskPolicy>, live: &Autonomy) -> Autonomy {
    match snapshot {
        Some(snapshot) => snapshot.autonomy.restricted_by(live),
        None => live.clone().normalized(),
    }
}

fn refusal(authority: Authority) -> String {
    format!(
        "Mono's autonomy policy does not allow this task to {}. Change the policy in the Mono's settings (Autonomy) and apply it to the task first.",
        authority.as_str()
    )
}

impl<P: Policy> Coordinator<P> {
    /// The policy the coordinator consults for the live side.
    pub fn policy(&self) -> &P {
        &self.policy
    }

    /// The effective policy for `task` (see [`effective`]).
    pub fn autonomy(&self, store: &Store, task: &Task) -> Result<Autonomy, TaskError> {
        let snapshot = policy::of(&store.tasks()?, &task.id)?;
        Ok(effective(snapshot.as_ref(), &self.policy.autonomy(task)))
    }

    /// Whether `task` may take a commit, merge, push or deploy step now. A
    /// refusal carries the reason; integration and delivery steps call this
    /// right before they act.
    pub fn authorize(
        &self,
        store: &Store,
        task_id: &str,
        authority: Authority,
    ) -> Result<(), TaskError> {
        let task = store
            .tasks()?
            .task(task_id)?
            .ok_or_else(|| TaskError::NotFound(format!("task:{task_id}")))?;
        if self.autonomy(store, &task)?.allows(authority) {
            Ok(())
        } else {
            Err(TaskError::Invalid(refusal(authority)))
        }
    }

    /// Whether `task`'s work interrupted by a restart continues on its own.
    pub fn restart_continues(&self, store: &Store, task: &Task) -> Result<bool, TaskError> {
        Ok(self.autonomy(store, task)?.restart_continuation)
    }
}

/// Re-snapshots `live` into an active task: the explicit step that lets a
/// changed policy (including more authority) reach work already under way.
/// `revision` is the policy revision the caller read; a stale one is refused.
pub fn apply(
    store: &Store,
    task_id: &str,
    revision: u64,
    live: &Autonomy,
    now: u64,
) -> Result<TaskPolicy, TaskError> {
    let tasks = store.tasks()?;
    let task = tasks
        .task(task_id)?
        .ok_or_else(|| TaskError::NotFound(format!("task:{task_id}")))?;
    if matches!(
        task.lifecycle,
        TaskLifecycle::Cancelled | TaskLifecycle::Completed
    ) {
        return Err(TaskError::Invalid(format!(
            "Task {task_id} is {}; its policy stays as it ran.",
            task.lifecycle.as_str()
        )));
    }
    let billing = live
        .api_key_billing
        .map(Ok)
        .unwrap_or_else(|| billing_preference(store, &tasks, task_id))?;
    let tx = tasks.begin()?;
    let current = policy::latest(&tx, task_id)?.ok_or_else(|| {
        TaskError::Invalid(format!(
            "Task {task_id} has not launched yet; it takes the Mono's current policy when it does."
        ))
    })?;
    if current.revision != revision {
        return Err(TaskError::Stale {
            subject: format!("policy:{task_id}"),
            expected: revision,
            actual: current.revision,
        });
    }
    let next = TaskPolicy {
        autonomy: live.clone().normalized(),
        use_api_key: billing,
        revision: current.revision + 1,
        captured_at: now,
        ..current
    };
    let event = NewEvent {
        id: format!("policy:{task_id}@{}", next.revision),
        task_id: task_id.into(),
        assignment_id: None,
        attempt_id: None,
        kind: policy::POLICY.into(),
        payload: serde_json::to_value(&next)?,
    };
    append(&tx, &event, now)?;
    tx.commit()?;
    Ok(next)
}

/// The app's API key preference for the task's provider.
fn billing_preference(
    store: &Store,
    tasks: &TaskStore<'_>,
    task_id: &str,
) -> Result<bool, TaskError> {
    let provider = policy::of(tasks, task_id)?
        .map(|p| p.provider)
        .unwrap_or_else(|| "codex".into());
    Ok(crate::provider::Kind::parse(&provider)
        .is_some_and(|kind| policy::live_api_key(store, kind)))
}

/// A task as the policy view lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Standing {
    pub task: Task,
    pub snapshot: Option<TaskPolicy>,
}

/// `mono`'s unfinished tasks with their snapshots, oldest first. Creates no
/// tables: a database without tasks lists none.
pub fn mono_tasks(store: &Store, mono: &str) -> Result<Vec<Standing>, TaskError> {
    let Some(tasks) = store.existing_tasks()? else {
        return Ok(Vec::new());
    };
    let mut statement = tasks.conn.prepare(
        "SELECT id FROM native_tasks WHERE mono_id=?1
           AND lifecycle NOT IN ('cancelled','completed') ORDER BY rowid",
    )?;
    let ids: Vec<String> = statement
        .query_map([mono], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let mut standings = Vec::new();
    for id in ids {
        let Some(task) = tasks.task(&id)? else {
            continue;
        };
        let snapshot = policy::of(&tasks, &id)?;
        standings.push(Standing { task, snapshot });
    }
    Ok(standings)
}

#[cfg(test)]
#[path = "autonomy_tests.rs"]
mod tests;
