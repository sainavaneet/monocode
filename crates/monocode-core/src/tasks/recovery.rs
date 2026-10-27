//! Recovery of task workers from account and provider failures.
//!
//! Sequence on failure ([`interrupt`]): classify it (`usage::Failure`),
//! record what it says about the account (cooldown, unhealthy), then in one
//! transaction record the failure, move the attempt to Interrupted (so it
//! can no longer report or dispatch), ask the app to stop its session through
//! the existing stop path (a `worker_stop` delivery), and persist the
//! recovery decision for the attempt's lineage: retry later on the same or
//! another allowed account, wait for a reset, block the task with a clear
//! reason, or hand the assignment to Mono. The coordinator carries out a
//! scheduled retry once it is due, the task is Active and the old session is
//! confirmed stopped ([`super::replace`]): it starts a replacement with a
//! durable hand-off ([`super::handoff`]).
//!
//! A claim the worker already made stands (that completed operation is
//! reconciled by the coordinator, not retried); a session that ends without
//! one is never assumed to have succeeded. Cancelled tasks never recover;
//! paused ones record the decision and act on it once Active again. Every
//! retry counts against one budget per lineage, whatever account it uses.
use super::coordinator;
use super::events::{append, NewEvent};
use super::launch::fail_assignment;
use super::policy::Candidates;
use super::review::MONO_REVIEW;
use super::states::{apply, is_latest_attempt};
use super::store::{ms, read_ms};
use super::worker::{event, settle};
use super::{
    policy, AttemptState, DeliveryState, Requester, State, Subject, Task, TaskError, TaskLifecycle,
    TaskStore,
};
use crate::accounts::eligibility::{self, AccountPlan, AllowedAccount};
use crate::store::Store;
use crate::usage::Failure;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::json;

/// Retries one attempt lineage may use, across every account it moves to.
pub const RETRY_BUDGET: u32 = 3;
/// The first backoff after a temporary failure; it doubles per retry...
pub const BACKOFF_BASE_MS: u64 = 15_000;
/// ...up to this.
pub const BACKOFF_MAX_MS: u64 = 10 * 60 * 1000;
/// How long a stop request may stay unconfirmed before recovery reports it
/// (it still never launches a replacement over an unstopped worker).
pub const STOP_STALL_MS: u64 = 2 * 60 * 1000;
/// Where requests to stop a failed worker's session wait for the app.
pub const WORKER_STOP: &str = "worker_stop";
/// The category of a session that ended with no result and no error.
pub const ENDED_WITHOUT_RESULT: &str = "ended_without_result";
/// The category of a worker the user stopped by hand while its task ran.
pub const STOPPED_BY_USER: &str = "stopped_by_user";

pub(super) const SCHEDULED: &str = "scheduled";
pub(super) const BLOCKED: &str = "blocked";
/// Blocked while the task was paused; the task is blocked when it resumes.
pub(super) const BLOCK_PENDING: &str = "block_pending";
pub(super) const EXHAUSTED: &str = "exhausted";
pub(super) const REPLACED: &str = "replaced";
/// Kept until the user or Mono resumes it (a worker the user stopped, or a
/// restart the task's policy does not continue).
pub(super) const HELD: &str = "held";
/// Never carried out: the task was cancelled or its assignment re-queued.
pub(super) const SUPERSEDED: &str = "superseded";
const MONO: &str = "mono";
const ABANDONED: &str = "abandoned";

/// The wait before retry number `retry` (1-based) of a lineage.
pub fn backoff_ms(retry: u32) -> u64 {
    let doublings = retry.saturating_sub(1).min(20);
    BACKOFF_BASE_MS
        .saturating_mul(1u64 << doublings)
        .min(BACKOFF_MAX_MS)
}

/// How a worker's turn ended, as the app saw it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnEnd {
    /// The user or the app stopped it.
    pub stopped: bool,
    /// The provider's error, when the turn failed.
    pub error: Option<String>,
    /// The provider reported a usage limit for the turn.
    pub usage_limited: bool,
    /// When that limit resets, if known (Unix ms).
    pub resets_at: Option<u64>,
}

/// What recovery decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Not a failure recovery owns (not a worker, already handled, claimed).
    Ignored,
    Retry {
        at: u64,
        provider: String,
        account: String,
    },
    /// Nothing is eligible before `until`; the retry happens then.
    Waiting {
        until: u64,
    },
    Blocked(String),
    /// The lineage spent its retries; the assignment failed and Mono decides.
    Exhausted,
    /// The session ended without a result; Mono reconciles it.
    HandedToMono,
    /// The user stopped the worker; it waits, held, for Mono or the user.
    Held,
    /// The task was cancelled.
    Abandoned,
}

/// The recovery decision for one interrupted attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recovery {
    pub attempt_id: String,
    pub task_id: String,
    pub assignment_id: String,
    /// The first attempt of the lineage.
    pub root_attempt: String,
    /// Retries the lineage used, including the one this schedules.
    pub retries_used: u32,
    /// A `Failure` name, or [`ENDED_WITHOUT_RESULT`].
    pub failure: String,
    /// Redacted.
    pub reason: String,
    pub provider: String,
    pub failed_account: Option<String>,
    pub next_provider: Option<String>,
    pub next_account: Option<String>,
    pub state: String,
    pub due_at: u64,
    pub replacement: Option<String>,
    pub revision: u64,
}

pub(super) const COLUMNS: &str = "attempt_id,task_id,assignment_id,root_attempt,retries_used,category,reason,provider,failed_account,next_provider,next_account,state,due_at,replacement,revision";

fn row(row: &Row<'_>) -> rusqlite::Result<Recovery> {
    Ok(Recovery {
        attempt_id: row.get(0)?,
        task_id: row.get(1)?,
        assignment_id: row.get(2)?,
        root_attempt: row.get(3)?,
        retries_used: row.get(4)?,
        failure: row.get(5)?,
        reason: row.get(6)?,
        provider: row.get(7)?,
        failed_account: row.get(8)?,
        next_provider: row.get(9)?,
        next_account: row.get(10)?,
        state: row.get(11)?,
        due_at: read_ms(row, 12)?,
        replacement: row.get(13)?,
        revision: read_ms(row, 14)?,
    })
}

/// The recovery decision for `attempt`, if it was interrupted by one.
pub fn recovery(tasks: &TaskStore<'_>, attempt: &str) -> Result<Option<Recovery>, TaskError> {
    Ok(tasks
        .conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM native_task_recoveries WHERE attempt_id=?1"),
            [attempt],
            row,
        )
        .optional()?)
}

/// The lineage `attempt` belongs to and the retries it has used.
pub(super) fn lineage(tasks: &TaskStore<'_>, attempt: &str) -> Result<(String, u32), TaskError> {
    let parent = tasks
        .conn
        .query_row(
            "SELECT root_attempt,retries_used FROM native_task_recoveries WHERE replacement=?1",
            [attempt],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(parent.unwrap_or_else(|| (attempt.to_owned(), 0)))
}

fn unknowns(tasks: &TaskStore<'_>, root: &str) -> Result<u32, TaskError> {
    Ok(tasks.conn.query_row(
        "SELECT COUNT(*) FROM native_task_recoveries WHERE root_attempt=?1 AND category='unknown'",
        [root],
        |row| row.get(0),
    )?)
}

pub(super) fn stop_delivery(attempt: &str) -> String {
    format!("stop:{attempt}")
}

/// Whether the attempt still runs and has not claimed a result.
fn live(tasks: &TaskStore<'_>, attempt: &super::Attempt) -> Result<bool, TaskError> {
    Ok(matches!(
        attempt.status,
        AttemptState::Starting | AttemptState::Running | AttemptState::Waiting
    ) && tasks.claim(&attempt.id)?.is_none())
}

/// What happens next for the lineage.
enum Next {
    Schedule {
        due: u64,
        provider: String,
        account: String,
        retries: u32,
        waiting: bool,
    },
    Block(String),
    Exhausted,
    Mono,
    Hold,
    Abandon,
}

impl Next {
    fn outcome(&self) -> Outcome {
        match self {
            Next::Schedule {
                due, waiting: true, ..
            } => Outcome::Waiting { until: *due },
            Next::Schedule {
                due,
                provider,
                account,
                ..
            } => Outcome::Retry {
                at: *due,
                provider: provider.clone(),
                account: account.clone(),
            },
            Next::Block(reason) => Outcome::Blocked(reason.clone()),
            Next::Exhausted => Outcome::Exhausted,
            Next::Mono => Outcome::HandedToMono,
            Next::Hold => Outcome::Held,
            Next::Abandon => Outcome::Abandoned,
        }
    }
}

fn decide(
    tasks: &TaskStore<'_>,
    candidates: &Candidates,
    failure: Failure,
    failed: &AllowedAccount,
    (root, used): (&str, u32),
    (reason, resets_at): (&str, Option<u64>),
    now: u64,
) -> Result<Next, TaskError> {
    if failure == Failure::Unknown && unknowns(tasks, root)? >= 1 {
        return Ok(Next::Block(format!(
            "The worker failed again for an unknown reason ({}). Recovery retries an unknown failure once and never switches accounts for it.",
            super::handoff::redact(reason)
        )));
    }
    let pool = candidates.pool();
    let retry = |due: u64, provider: &str, account: &str, waiting: bool| {
        if used >= candidates.retry_limit {
            Next::Exhausted
        } else {
            Next::Schedule {
                due,
                provider: provider.into(),
                account: account.into(),
                retries: used + 1,
                waiting,
            }
        }
    };
    let (provider, account) = (failed.provider.as_str(), failed.account_id.as_str());
    Ok(
        match eligibility::plan(failure, failed, &pool, resets_at, now) {
            AccountPlan::Same => retry(
                now.saturating_add(backoff_ms(used + 1)),
                provider,
                account,
                false,
            ),
            AccountPlan::Switch(next) => retry(now, &next.provider, &next.account_id, false),
            AccountPlan::Wait { until } => retry(until, provider, account, true),
            AccountPlan::Block(why) => Next::Block(format!(
                "{why} The provider said: {}",
                super::handoff::redact(reason)
            )),
        },
    )
}

/// A worker's turn ended. Called by the app for every finished turn; only
/// the latest live attempt of a task worker without a claim is acted on.
pub fn turn_ended(
    store: &Store,
    session: i64,
    end: &TurnEnd,
    now: u64,
) -> Result<Outcome, TaskError> {
    if !coordinator::enabled(store) {
        return Ok(Outcome::Ignored);
    }
    let Some(tasks) = store.existing_tasks()? else {
        return Ok(Outcome::Ignored);
    };
    let bound = store
        .persisted_session_id(session)
        .map_err(TaskError::Storage)?;
    let Some(attempt) = tasks.attempt_for_session(&bound)? else {
        return Ok(Outcome::Ignored);
    };
    if !live(&tasks, &attempt)? || !is_latest_attempt(tasks.conn, Subject::Attempt(&attempt.id))? {
        return Ok(Outcome::Ignored);
    }
    let error = end.error.as_deref().filter(|e| !e.trim().is_empty());
    match error {
        Some(error) => {
            let mut failure = Failure::classify(error);
            if end.usage_limited && failure != Failure::SharedQuota {
                failure = Failure::UsageExhausted;
            }
            interrupt(store, &attempt.id, failure, error, end.resets_at, now)
        }
        None if end.usage_limited => interrupt(
            store,
            &attempt.id,
            Failure::UsageExhausted,
            "Usage limit reached",
            end.resets_at,
            now,
        ),
        // Waiting on a decision or blocked: the turn ends by design.
        None if attempt.status != AttemptState::Running => Ok(Outcome::Ignored),
        // The user stopped it by hand (pause and cancel move the attempt
        // first, so they never get here): held, never failed, and Mono told.
        None if end.stopped => settle_failure(
            store,
            &attempt.id,
            STOPPED_BY_USER,
            "The user stopped this worker before it reported a result.",
            now,
            |_| Ok(Next::Hold),
        ),
        None => {
            let why = if end.stopped {
                "The worker was stopped before it reported a result."
            } else {
                "The worker ended its turn without reporting a result."
            };
            settle_failure(store, &attempt.id, ENDED_WITHOUT_RESULT, why, now, |_| {
                Ok(Next::Mono)
            })
        }
    }
}

/// Records `failure` of the attempt and decides its recovery (see the module
/// docs). Idempotent: an attempt that is no longer live is ignored.
pub fn interrupt(
    store: &Store,
    attempt_id: &str,
    failure: Failure,
    reason: &str,
    resets_at: Option<u64>,
    now: u64,
) -> Result<Outcome, TaskError> {
    let tasks = store.tasks()?;
    let Some(attempt) = tasks.attempt(attempt_id)? else {
        return Ok(Outcome::Ignored);
    };
    if !live(&tasks, &attempt)? {
        return Ok(Outcome::Ignored);
    }
    let task = task(&tasks, &attempt.task_id)?;
    let policy = policy::capture(store, &task, &attempt.provider, now)?;
    let line = lineage(&tasks, &attempt.id)?;
    settle_failure(store, attempt_id, failure.as_str(), reason, now, |task| {
        // Read the live allowlist; record what the failure says about the
        // account (only now that the interrupt is certain), then decide.
        let before = Candidates::read(store, task, &policy)?;
        let failed = before.entry(&attempt.provider, attempt.account_id.as_deref());
        eligibility::record_failure(store, &before.allowlist, &failed, failure, resets_at, now)
            .map_err(TaskError::Storage)?;
        if task.lifecycle == TaskLifecycle::Cancelled {
            return Ok(Next::Abandon);
        }
        let candidates = Candidates::read(store, task, &policy)?;
        decide(
            &tasks,
            &candidates,
            failure,
            &failed,
            (&line.0, line.1),
            (reason, resets_at),
            now,
        )
    })
}

fn task(tasks: &TaskStore<'_>, id: &str) -> Result<Task, TaskError> {
    tasks
        .task(id)?
        .ok_or_else(|| TaskError::NotFound(format!("task:{id}")))
}

/// Interrupts the attempt and persists `next` in one transaction: the
/// failure event, attempt Interrupted, the stop request for its session, the
/// recovery row, and a blocked task or failed assignment with Mono's notice.
fn settle_failure(
    store: &Store,
    attempt_id: &str,
    category: &str,
    reason: &str,
    now: u64,
    next: impl FnOnce(&Task) -> Result<Next, TaskError>,
) -> Result<Outcome, TaskError> {
    let tasks = store.tasks()?;
    let Some(found) = tasks.attempt(attempt_id)? else {
        return Ok(Outcome::Ignored);
    };
    let tx = tasks.begin()?;
    let inside = TaskStore { conn: &tx };
    // Re-read under the write lock: a concurrent report may have won, or the
    // task may have been paused or cancelled. Only then is anything (the
    // account's standing included) recorded.
    let task = task(&inside, &found.task_id)?;
    let Some(attempt) = inside.attempt(attempt_id)? else {
        return Ok(Outcome::Ignored);
    };
    if !live(&inside, &attempt)? {
        return Ok(Outcome::Ignored);
    }
    let next = next(&task)?;
    let reason = super::handoff::redact(reason);
    let failed = event(
        &attempt,
        format!("failure:{}", attempt.id),
        "attempt.failure",
        json!({"category": category, "reason": reason, "sessionId": attempt.session_id,
               "accountId": attempt.account_id}),
    );
    let failed = append(&tx, &failed, now)?;
    let interrupted = State::Attempt(AttemptState::Interrupted);
    settle(
        &tx,
        Subject::Attempt(&attempt.id),
        interrupted,
        Requester::Coordinator,
        now,
    )?;
    if let Some(session) = &attempt.session_id {
        let reference = format!("session:{session}");
        inside.enqueue_delivery(
            &stop_delivery(&attempt.id),
            &failed.id,
            WORKER_STOP,
            &reference,
            now,
        )?;
    }
    // A paused task keeps its pause; it is blocked when it resumes.
    let blocked = match &next {
        Next::Block(why) => {
            append(&tx, &block_reason_event(&attempt, why), now)?;
            block(&tx, &task.id, why, now)?
        }
        _ => false,
    };
    let (state, due, provider, account, retries) = match &next {
        Next::Schedule {
            due,
            provider,
            account,
            retries,
            ..
        } => (
            SCHEDULED,
            *due,
            Some(provider.as_str()),
            Some(account.as_str()),
            *retries,
        ),
        Next::Block(_) if blocked => (BLOCKED, now, None, None, 0),
        Next::Block(_) => (BLOCK_PENDING, now, None, None, 0),
        Next::Exhausted => (EXHAUSTED, now, None, None, 0),
        Next::Mono => (MONO, now, None, None, 0),
        Next::Hold => (HELD, now, None, None, 0),
        Next::Abandon => (ABANDONED, now, None, None, 0),
    };
    let (root, used) = lineage(&inside, &attempt.id)?;
    let retries = if matches!(next, Next::Schedule { .. }) {
        retries
    } else {
        used
    };
    tx.execute(
        &format!(
            "INSERT INTO native_task_recoveries({COLUMNS},created_at,updated_at)
           VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,NULL,1,?14,?14)"
        ),
        params![
            attempt.id,
            attempt.task_id,
            attempt.assignment_id,
            root,
            retries,
            category,
            reason,
            attempt.provider,
            attempt.account_id,
            provider,
            account,
            state,
            ms(due),
            ms(now)
        ],
    )?;
    match &next {
        Next::Block(why) if blocked => {
            notify_mono(&tx, &attempt, "recovery.blocked", why, 1, now)?;
        }
        Next::Exhausted => {
            fail_assignment(&tx, &attempt, now)?;
            let why = format!("The worker failed ({category}: {reason}) after {used} retries, the task's retry limit.");
            notify_mono(&tx, &attempt, "recovery.exhausted", &why, 1, now)?;
        }
        Next::Mono => {
            fail_assignment(&tx, &attempt, now)?;
            notify_mono(
                &tx,
                &attempt,
                "recovery.ended_without_result",
                &reason,
                1,
                now,
            )?;
        }
        Next::Hold => {
            notify_mono(&tx, &attempt, "recovery.stopped_by_user", &reason, 1, now)?;
        }
        Next::Block(_) | Next::Schedule { .. } | Next::Abandon => {}
    }
    tx.commit()?;
    Ok(next.outcome())
}

/// Blocks an Active or Waiting task with `reason`; a paused task keeps its
/// pause and is blocked when it resumes and recovery runs again.
pub(super) fn block(
    conn: &Connection,
    task: &str,
    reason: &str,
    now: u64,
) -> Result<bool, TaskError> {
    let tasks = TaskStore { conn };
    let subject = Subject::Task(task);
    let blocked = State::Lifecycle(TaskLifecycle::Blocked);
    let current = tasks.state(subject, blocked)?;
    if !matches!(
        current,
        State::Lifecycle(TaskLifecycle::Active | TaskLifecycle::Waiting)
    ) {
        return Ok(false);
    }
    let revision = tasks.revision(subject)?;
    apply(
        conn,
        subject,
        blocked,
        Requester::Coordinator,
        revision,
        now,
    )?;
    conn.execute(
        "UPDATE native_tasks SET blocked_reason=?2 WHERE id=?1",
        [task, reason],
    )?;
    Ok(true)
}

/// Keeps why recovery blocks the attempt's lineage, for a task that was
/// paused at the time and is blocked when it resumes.
fn block_reason_event(attempt: &super::Attempt, why: &str) -> NewEvent {
    let id = format!("block-reason:{}", attempt.id);
    event(attempt, id, "recovery.block_reason", json!({"reason": why}))
}

/// The reason recorded by [`block_reason_event`].
pub(super) fn block_reason(conn: &Connection, attempt: &str) -> Result<Option<String>, TaskError> {
    Ok(
        super::worker::payload_of(conn, &format!("block-reason:{attempt}"))?
            .and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok())
            .and_then(|p| p["reason"].as_str().map(str::to_owned)),
    )
}

/// Queues one item for the owning Mono's inbox about the attempt.
pub(super) fn notify_mono(
    conn: &Connection,
    attempt: &super::Attempt,
    kind: &str,
    reason: &str,
    revision: u64,
    now: u64,
) -> Result<(), TaskError> {
    let tasks = TaskStore { conn };
    let id = format!("{kind}:{}@{revision}", attempt.id);
    let notice: NewEvent = event(attempt, id.clone(), kind, json!({"reason": reason}));
    append(conn, &notice, now)?;
    let delivery = format!("mono-recovery:{}@{revision}:{kind}", attempt.id);
    let reference = format!("attempt:{}", attempt.id);
    tasks.enqueue_delivery(&delivery, &id, MONO_REVIEW, &reference, now)?;
    Ok(())
}

/// A request for the app to stop a failed worker's session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StopRequest {
    pub delivery: String,
    /// The persisted session id (`Store::resolve_session_id` gives its handle).
    pub session_id: String,
    /// The session is no longer in the store (deleted): it cannot run, so
    /// the stop counts as done once the absent grace has passed.
    pub gone: bool,
}

/// Stop requests the app has not carried out.
pub fn pending_stops(store: &Store) -> Result<Vec<StopRequest>, TaskError> {
    if !coordinator::enabled(store) {
        return Ok(Vec::new());
    }
    let Some(tasks) = store.existing_tasks()? else {
        return Ok(Vec::new());
    };
    Ok(tasks
        .pending_deliveries(WORKER_STOP)?
        .into_iter()
        .filter_map(|d| {
            let session = d.payload_ref.strip_prefix("session:")?.to_owned();
            // A store error proves nothing: the stop then stays pending.
            let gone = store.session_persisted(&session) == Ok(false);
            Some(StopRequest {
                delivery: d.id,
                session_id: session,
                gone,
            })
        })
        .collect())
}

/// The app stopped the session (or it was not running here).
pub fn stopped(store: &Store, delivery: &str, now: u64) -> Result<(), TaskError> {
    let tasks = store.tasks()?;
    let Some(current) = tasks.delivery(delivery)? else {
        return Ok(());
    };
    if current.state == DeliveryState::Acknowledged {
        return Ok(());
    }
    tasks.record_delivery(
        delivery,
        current.revision,
        super::DeliveryOutcome::Acknowledged,
        now,
    )?;
    Ok(())
}

/// Whether the app may install and start `session` now: it must be a
/// worker whose attempt is still Starting. Sessions that are not task
/// workers are always allowed. A refused one is an orphan (see
/// [`super::launch::confirm_started`]).
pub fn launch_allowed(store: &Store, session: i64) -> Result<bool, TaskError> {
    let Some(tasks) = store.existing_tasks()? else {
        return Ok(true);
    };
    let bound = store
        .persisted_session_id(session)
        .map_err(TaskError::Storage)?;
    Ok(match tasks.attempt_for_session(&bound)? {
        Some(attempt) => attempt.status == AttemptState::Starting,
        None => true,
    })
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
pub(in crate::tasks) mod tests;
