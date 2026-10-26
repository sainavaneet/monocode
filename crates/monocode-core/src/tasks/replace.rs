//! The coordinator's half of recovery: once a recovery decision is due, the
//! task is Active and the failed worker's session is confirmed stopped,
//! start its replacement on the chosen account with a durable hand-off.
use super::coordinator::{Coordinator, Launcher, Policy, Tick};
use super::handoff::{self, Inputs};
use super::launch::fail_assignment;
use super::policy::Candidates;
use super::recovery::{
    self, block, block_reason, notify_mono, stop_delivery, Recovery, BLOCKED, BLOCK_PENDING,
    EXHAUSTED, REPLACED, SCHEDULED, STOP_STALL_MS,
};
use super::store::{ms, Key};
use super::{
    policy, Assignment, Attempt, DeliveryState, NewAttempt, Task, TaskError, TaskLifecycle,
    TaskStore,
};
use crate::accounts::eligibility::{self, AccountPlan, AllowedAccount};
use crate::store::Store;
use crate::usage::Failure;
use rusqlite::params;

fn key(attempt: &str) -> Key<'_> {
    Key::new(
        "native_task_recoveries",
        "attempt_id=?",
        vec![attempt],
        "recovery",
    )
}

/// What to do with a due recovery.
enum Choice {
    Replace {
        provider: String,
        account: String,
        retries: u32,
    },
    /// Nothing eligible yet; try again at `until` without spending a retry.
    Later(u64),
    Block(String),
    Exhausted,
}

/// Picks the account a due retry runs on: the one chosen when it was
/// scheduled if still eligible, else the best eligible one of the live
/// allowlist now. A configuration or policy failure never changes account:
/// it retries (after an unblock) on the account that failed, or blocks.
fn choose(store: &Store, row: &Recovery, now: u64) -> Result<Choice, TaskError> {
    let tasks = store.tasks()?;
    let task = tasks
        .task(&row.task_id)?
        .ok_or_else(|| TaskError::NotFound(format!("task:{}", row.task_id)))?;
    let policy = policy::capture(store, &task, &row.provider, now)?;
    // A blocked lineage that was unblocked spends a retry now.
    let retries = if row.state == BLOCKED {
        if row.retries_used >= policy.autonomy.retry_limit {
            return Ok(Choice::Exhausted);
        }
        row.retries_used + 1
    } else {
        row.retries_used
    };
    let provider = row.next_provider.clone().unwrap_or(row.provider.clone());
    let candidates = Candidates::read(store, &task, &policy)?;
    let pool = candidates.pool();
    let failed: AllowedAccount = candidates.entry(&row.provider, row.failed_account.as_deref());
    let replace = |account: AllowedAccount| Choice::Replace {
        provider: account.provider,
        account: account.account_id,
        retries,
    };
    let keeps_account = matches!(
        Failure::parse(&row.failure),
        Some(Failure::Configuration | Failure::Policy)
    );
    if keeps_account {
        return Ok(if candidates.eligible(&failed, now) {
            replace(failed)
        } else {
            Choice::Block(format!(
                "{}/{} is no longer available, and a configuration or policy failure never moves to another account.",
                failed.provider, failed.account_id
            ))
        });
    }
    let preferred = row
        .next_account
        .as_deref()
        .map(|id| candidates.allowlist.entry(&provider, id))
        .filter(|a| candidates.eligible(a, now));
    if let Some(account) =
        preferred.or_else(|| eligibility::first_eligible(&pool, &provider, None, now))
    {
        return Ok(replace(account));
    }
    // Nothing eligible: wait for a known cooldown, else block with why.
    let failed = candidates.entry(&provider, row.failed_account.as_deref());
    Ok(
        match eligibility::plan(Failure::UsageExhausted, &failed, &pool, None, now) {
            AccountPlan::Wait { until } => Choice::Later(until),
            AccountPlan::Block(reason) => Choice::Block(reason),
            AccountPlan::Same | AccountPlan::Switch(_) => {
                Choice::Block("No eligible account was found when the retry was due.".into())
            }
        },
    )
}

impl<P: Policy> Coordinator<P> {
    /// Carries out the recovery of the assignment's latest attempt, which
    /// was Interrupted. Does nothing until the task is Active, the decision
    /// is due, the old session is stopped and no open question holds it.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn recover(
        &self,
        store: &Store,
        task: &Task,
        assignment: &Assignment,
        attempt: &Attempt,
        launcher: &dyn Launcher,
        now: u64,
        tick: &mut Tick,
    ) -> Result<(), TaskError> {
        if task.lifecycle != TaskLifecycle::Active {
            return Ok(());
        }
        let tasks = store.tasks()?;
        let Some(row) = recovery::recovery(&tasks, &attempt.id)? else {
            return Ok(());
        };
        if row.state == BLOCK_PENDING {
            // Blocked while paused: now that it resumed, block it rather
            // than relaunch.
            let reason =
                block_reason(tasks.conn, &attempt.id)?.unwrap_or_else(|| row.reason.clone());
            let tx = tasks.begin()?;
            let revision = key(&attempt.id).bump(
                &tx,
                row.revision,
                "state=?",
                params![BLOCKED].to_vec(),
                now,
            )?;
            block(&tx, &task.id, &reason, now)?;
            notify_mono(&tx, attempt, "recovery.blocked", &reason, revision, now)?;
            tx.commit()?;
            return Ok(());
        }
        // Two agents never write one workspace: nothing replaces a worker
        // whose stop the app has not confirmed. A stop that stays pending
        // past STOP_STALL_MS is reported once (event and Mono notice).
        if let Some(stop) = tasks.delivery(&stop_delivery(&attempt.id))? {
            if stop.state != DeliveryState::Acknowledged {
                if now >= stop.created_at.saturating_add(STOP_STALL_MS) {
                    let reason = "The failed worker's session has not confirmed it stopped, so no replacement is started in its workspace. Check that its window is open and its turn ended.";
                    let first = super::worker::payload_of(
                        tasks.conn,
                        &format!("recovery.stop_stalled:{}@1", attempt.id),
                    )?
                    .is_none();
                    let tx = tasks.begin()?;
                    notify_mono(&tx, attempt, "recovery.stop_stalled", reason, 1, now)?;
                    tx.commit()?;
                    if first {
                        tick.stalled.push(attempt.id.clone());
                    }
                }
                return Ok(());
            }
        }
        let due = row.state == BLOCKED || (row.state == SCHEDULED && now >= row.due_at);
        if !due {
            return Ok(());
        }
        let all = tasks.assignments(&task.id)?;
        if tasks.launch_blocked(&task.id, assignment, &all)?.is_some() {
            return Ok(());
        }
        match choose(store, &row, now)? {
            Choice::Later(until) => {
                key(&attempt.id).bump(
                    tasks.conn,
                    row.revision,
                    "state=?,due_at=?",
                    params![SCHEDULED, ms(until)].to_vec(),
                    now,
                )?;
            }
            Choice::Block(reason) => {
                let tx = tasks.begin()?;
                let revision = key(&attempt.id).bump(
                    &tx,
                    row.revision,
                    "state=?",
                    params![BLOCKED].to_vec(),
                    now,
                )?;
                block(&tx, &task.id, &reason, now)?;
                notify_mono(&tx, attempt, "recovery.blocked", &reason, revision, now)?;
                tx.commit()?;
            }
            Choice::Exhausted => {
                let tx = tasks.begin()?;
                let revision = key(&attempt.id).bump(
                    &tx,
                    row.revision,
                    "state=?",
                    params![EXHAUSTED].to_vec(),
                    now,
                )?;
                fail_assignment(&tx, attempt, now)?;
                let why = format!(
                    "The worker's retries are spent (the task's retry limit); its last failure was {}: {}.",
                    row.failure, row.reason
                );
                notify_mono(&tx, attempt, "recovery.exhausted", &why, revision, now)?;
                tx.commit()?;
            }
            Choice::Replace {
                provider,
                account,
                retries,
            } => {
                let replacement = self.replacement(
                    store, task, assignment, attempt, &row, &provider, &account, retries, now,
                )?;
                tick.recovered.push(replacement.id.clone());
                let Some(started) = self.begin_launch(store, task, assignment, now)? else {
                    return Ok(());
                };
                self.dispatch(store, task, &started, launcher, now, tick)?;
            }
        }
        Ok(())
    }

    /// Creates the replacement attempt (idempotent per interrupted attempt),
    /// records its hand-off, and marks the recovery done.
    #[allow(clippy::too_many_arguments)]
    fn replacement(
        &self,
        store: &Store,
        task: &Task,
        assignment: &Assignment,
        attempt: &Attempt,
        row: &Recovery,
        provider: &str,
        account: &str,
        retries: u32,
        now: u64,
    ) -> Result<Attempt, TaskError> {
        let tasks = store.tasks()?;
        let id = crate::mono::new_id();
        let next = tasks.create_attempt(
            &NewAttempt {
                id: format!("attempt-{id}"),
                task_id: task.id.clone(),
                assignment_id: assignment.id.clone(),
                operation_id: format!("task-recover-{}", attempt.id),
                provider: provider.into(),
                account_id: Some(account.into()),
            },
            now,
        )?;
        let workspace = tasks
            .workspaces(&task.id)?
            .into_iter()
            .find(|w| w.assignment_id.as_deref() == Some(assignment.id.as_str()));
        if let Some(workspace) = workspace {
            let record = handoff::build(
                &tasks,
                &Inputs {
                    task,
                    assignment,
                    workspace: &workspace,
                    from: attempt,
                    to: &next,
                    category: &row.failure,
                    reason: &row.reason,
                },
            )?;
            handoff::record(&tasks, &record, &next, now)?;
        }
        let tx = tasks.begin()?;
        let inside = TaskStore { conn: &tx };
        if let Some(current) = recovery::recovery(&inside, &attempt.id)? {
            if current.state != REPLACED {
                key(&attempt.id).bump(
                    &tx,
                    current.revision,
                    "state=?,replacement=?,retries_used=?,next_provider=?,next_account=?",
                    params![REPLACED, next.id, retries, provider, account].to_vec(),
                    now,
                )?;
            }
        }
        tx.commit()?;
        Ok(next)
    }
}
