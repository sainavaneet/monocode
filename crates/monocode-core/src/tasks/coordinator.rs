//! The execution coordinator: one core service per process that turns
//! started tasks into worker sessions and observes what the workers report.
//!
//! Each tick, for every task that may have work: take or renew the task's
//! database lease (the other app sharing monocode.db is the competing
//! coordinator), reconcile each assignment's latest attempt (finish a launch
//! whose intent was persisted, bind a session a retried launch already
//! started, classify a failed claim, request one review of a succeeded one),
//! then launch Ready assignments the policy and file scopes allow. A launch
//! persists its intent before any external effect, so every step is
//! at-least-once and idempotent. It does nothing unless [`FLAG`] is enabled.
use super::{
    AssignmentState, Attempt, AttemptState, Requester, State, Subject, Task, TaskError,
    TaskLifecycle, TaskStage, TaskStore,
};
use crate::agent::Settings;
use crate::store::{Session, Store};

/// The `native_settings` key that turns the coordinator on. Off unless its
/// value is `enabled`, because the desktop file is shared with live agents.
pub const FLAG: &str = "native-task-coordinator";
/// How long a lease outlives its holder's last tick.
pub const LEASE_TTL_MS: u64 = 30_000;
/// How often a running app ticks; well inside the lease's lifetime.
pub const TICK_INTERVAL_MS: u64 = 5_000;
/// How long a reserved worker session may wait for the app to confirm it
/// started before the attempt fails with a `launch.unconfirmed` event.
pub const LAUNCH_CONFIRM_TIMEOUT_MS: u64 = 60_000;
/// The destination of review requests for succeeded claims.
pub const REVIEW: &str = "review";

/// Whether the coordinator may act in this database.
pub fn enabled(store: &Store) -> bool {
    store
        .setting(FLAG)
        .ok()
        .flatten()
        .is_some_and(|value| value.trim() == "enabled")
}

/// The lease that lets one coordinator act on a task.
pub fn lease_resource(task_id: &str) -> String {
    format!("task:{task_id}")
}

/// What a Mono allows the coordinator to do on its own. The defaults launch
/// only tasks Mono or the user explicitly started, one worker at a time.
pub trait Policy {
    /// The Mono's live autonomy policy for `task`. The coordinator combines
    /// it with the task's snapshot (`autonomy::effective`): reductions apply
    /// at once, more authority waits for an explicit apply. By default it
    /// follows the two methods below.
    fn autonomy(&self, task: &Task) -> super::autonomy::Autonomy {
        super::autonomy::Autonomy {
            automatic_execution: self.automatic_execution(task),
            worker_limit: u32::try_from(self.worker_limit(task)).unwrap_or(u32::MAX),
            ..super::autonomy::Autonomy::default()
        }
    }
    /// Whether a Ready task may move into Implementing without an explicit
    /// start.
    fn automatic_execution(&self, _task: &Task) -> bool {
        false
    }
    /// How many of a task's assignments may run at once.
    fn worker_limit(&self, _task: &Task) -> usize {
        1
    }
    /// The provider that runs the task's workers.
    fn worker_provider(&self, _task: &Task) -> String {
        "codex".into()
    }
    /// Whether work interrupted by an app restart continues on its own once
    /// the app is back (task-9's policy fills this in). A task reaches a
    /// worker only once Mono or the user started it explicitly, so the
    /// default continues; otherwise the task waits for a resume.
    fn restart_continuation(&self, _task: &Task) -> bool {
        true
    }
}

/// The app's policy: each Mono's policy as the app published it
/// (`autonomy::publish`, from its preferences). A Mono with none published
/// gets the default: nothing automatic, one worker at a time.
pub struct ExplicitStart;
impl Policy for ExplicitStart {
    fn autonomy(&self, task: &Task) -> super::autonomy::Autonomy {
        super::autonomy::live(task.mono_id.as_deref(), &task.project)
    }
}

/// Starts a worker session the coordinator already reserved and configured
/// through `sessions.start`'s durable steps. The GUI installs and submits it;
/// tests record it.
pub trait Launcher {
    fn start(&self, session: &Session, settings: &Settings) -> Result<(), String>;
}

/// What one tick did, by attempt or task id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tick {
    /// False when the flag is off and nothing was read or written.
    pub enabled: bool,
    pub launched: Vec<String>,
    /// Attempts whose already-started session was bound without a relaunch.
    pub bound: Vec<String>,
    pub failed: Vec<String>,
    /// Replacement attempts recovery created for interrupted ones.
    pub recovered: Vec<String>,
    /// Interrupted attempts whose stop stalled (reported once each).
    pub stalled: Vec<String>,
    /// Attempts whose succeeded claim was queued for review.
    pub reviews: Vec<String>,
    pub leased_elsewhere: Vec<String>,
    /// Per-task errors; the next tick retries.
    pub errors: Vec<String>,
}

pub struct Coordinator<P: Policy = ExplicitStart> {
    pub(super) holder: String,
    pub(super) policy: P,
    /// Whether a previous lease holder's process is gone (its workers died
    /// with it); see `restart`.
    pub(super) liveness: fn(&str) -> super::restart::Liveness,
}

impl<P: Policy> Coordinator<P> {
    /// `holder` names this process in leases; it must differ per process.
    pub fn new(holder: impl Into<String>, policy: P) -> Self {
        Self {
            holder: holder.into(),
            policy,
            liveness: super::restart::native_liveness,
        }
    }

    /// Replaces how a previous holder's liveness is judged (tests stand in
    /// for processes that quit or still run).
    pub fn with_liveness(mut self, liveness: fn(&str) -> super::restart::Liveness) -> Self {
        self.liveness = liveness;
        self
    }

    pub fn holder(&self) -> &str {
        &self.holder
    }

    /// One pass over every task that may have work. `now` is Unix ms.
    pub fn tick(
        &self,
        store: &Store,
        launcher: &dyn Launcher,
        now: u64,
    ) -> Result<Tick, TaskError> {
        let mut tick = Tick::default();
        if !enabled(store) {
            return Ok(tick);
        }
        tick.enabled = true;
        let tasks = store.tasks()?;
        for id in candidates(&tasks)? {
            match self.take(&tasks, &id, now) {
                Err(TaskError::LeaseHeld { .. }) => tick.leased_elsewhere.push(id),
                Err(error) => tick.errors.push(format!("{id}: {error}")),
                Ok(()) => {
                    // A task taken over from a holder that is gone is
                    // reconciled first (its workers died with it).
                    let mut restart = super::restart::Startup::default();
                    let reconciled = self
                        .reconcile_restart(store, &id, now, &mut restart)
                        .and_then(|()| self.release_takeover(store, &id, now));
                    if let Err(error) = reconciled {
                        tick.errors.push(format!("{id}: {error}"));
                        continue;
                    }
                    match self.step(store, &id, launcher, now, &mut tick) {
                        Ok(()) => {}
                        // Lost to another holder at the dispatch fence.
                        Err(TaskError::LeaseHeld { .. }) => tick.leased_elsewhere.push(id),
                        Err(error) => tick.errors.push(format!("{id}: {error}")),
                    }
                }
            }
        }
        Ok(tick)
    }

    /// Takes or renews the task's lease. Called again right before each
    /// dispatch, so a holder that lost it never starts a worker.
    pub(super) fn lease(&self, tasks: &TaskStore<'_>, id: &str, now: u64) -> Result<(), TaskError> {
        tasks
            .acquire_lease(&lease_resource(id), &self.holder, LEASE_TTL_MS, now)
            .map(|_| ())
    }

    fn step(
        &self,
        store: &Store,
        id: &str,
        launcher: &dyn Launcher,
        now: u64,
        tick: &mut Tick,
    ) -> Result<(), TaskError> {
        let tasks = store.tasks()?;
        // A task-wide open question makes the task wait, and its answer
        // makes it active again.
        super::waiting::sync_waiting(&tasks, id, now)?;
        let Some(mut task) = tasks.task(id)? else {
            return Ok(());
        };
        if task.stage == TaskStage::Ready {
            if task.lifecycle != TaskLifecycle::Active
                || !self.autonomy(store, &task)?.automatic_execution
            {
                return Ok(());
            }
            let implementing = State::Stage(TaskStage::Implementing);
            tasks.transition(
                Subject::Task(id),
                implementing,
                Requester::Coordinator,
                task.revision,
                now,
            )?;
            task = tasks
                .task(id)?
                .ok_or_else(|| TaskError::NotFound(format!("task:{id}")))?;
        }
        for assignment in tasks.assignments(id)? {
            self.reconcile(store, &task, &assignment, launcher, now, tick)?;
        }
        // Reconciling may have blocked the task.
        let Some(task) = tasks.task(id)? else {
            return Ok(());
        };
        if task.lifecycle == TaskLifecycle::Active {
            self.schedule(store, &task, launcher, now, tick)?;
        }
        Ok(())
    }

    /// Launches Ready assignments up to the policy's limit, never two with
    /// overlapping file scopes at once. Queued ones become Ready once their
    /// dependencies are accepted.
    fn schedule(
        &self,
        store: &Store,
        task: &Task,
        launcher: &dyn Launcher,
        now: u64,
        tick: &mut Tick,
    ) -> Result<(), TaskError> {
        let tasks = store.tasks()?;
        let ready = State::Assignment(AssignmentState::Ready);
        for assignment in tasks.assignments(&task.id)? {
            if assignment.status != AssignmentState::Queued {
                continue;
            }
            let subject = Subject::Assignment {
                task: &task.id,
                id: &assignment.id,
            };
            match tasks.transition(
                subject,
                ready,
                Requester::Coordinator,
                assignment.revision,
                now,
            ) {
                Ok(_) | Err(TaskError::Precondition(_)) => {}
                Err(error) => return Err(error),
            }
        }
        let assignments = tasks.assignments(&task.id)?;
        let mut active: Vec<&[String]> = assignments
            .iter()
            .filter(|a| {
                matches!(
                    a.status,
                    AssignmentState::Running | AssignmentState::WaitingForDecision
                )
            })
            .map(|a| a.files.as_slice())
            .collect();
        // A worker whose stop is unconfirmed still occupies its file scope.
        let mut occupied: Vec<&[String]> = active.clone();
        for assignment in &assignments {
            if super::guard::pending_stop(tasks.conn, &task.id, &assignment.id)?.is_some() {
                occupied.push(assignment.files.as_slice());
            }
        }
        // The effective policy (snapshot restricted by live): fewer workers
        // than running now launches nothing; none is stopped.
        let limit = self.autonomy(store, task)?.worker_limit as usize;
        for assignment in assignments
            .iter()
            .filter(|a| a.status == AssignmentState::Ready)
        {
            if active.len() >= limit {
                break;
            }
            // Two agents never work one workspace.
            if self.held_by_stop(&tasks, &task.id, &assignment.id, now, tick)? {
                continue;
            }
            // A question holds what its scope covers; the rest continues.
            if tasks
                .launch_blocked(&task.id, assignment, &assignments)?
                .is_some()
            {
                continue;
            }
            if occupied
                .iter()
                .any(|files| crate::orchestration::overlaps(files, &assignment.files))
            {
                continue;
            }
            let Some(attempt) = self.begin_launch(store, task, assignment, now)? else {
                return Ok(());
            };
            self.dispatch(store, task, &attempt, launcher, now, tick)?;
            active.push(&assignment.files);
            occupied.push(&assignment.files);
        }
        Ok(())
    }
}

/// Tasks that may have work to launch or results to observe.
fn candidates(tasks: &TaskStore<'_>) -> Result<Vec<String>, TaskError> {
    let mut statement = tasks.conn.prepare(
        "SELECT id FROM native_tasks WHERE stage IN ('ready','implementing')
           AND lifecycle NOT IN ('cancelled','completed') ORDER BY rowid",
    )?;
    let ids = statement.query_map([], |row| row.get(0))?;
    Ok(ids.collect::<Result<_, _>>()?)
}

/// The assignment's newest attempt by insertion order.
pub(super) fn latest_attempt(
    tasks: &TaskStore<'_>,
    task_id: &str,
    assignment_id: &str,
) -> Result<Option<Attempt>, TaskError> {
    Ok(tasks.attempts(task_id, assignment_id)?.pop())
}

/// Whether the attempt's launch has not reached a session yet.
pub(super) fn in_flight(attempt: &Attempt) -> bool {
    matches!(
        attempt.status,
        AttemptState::Created | AttemptState::Starting
    )
}

#[cfg(test)]
#[path = "coordinator_tests.rs"]
mod tests;
