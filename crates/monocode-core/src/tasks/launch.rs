//! Launch intents, dispatch through `sessions.start`'s durable steps, receipts
//! and the coordinator's reading of worker claims.
//!
//! Order: workspace recorded, attempt created, intent event plus attempt
//! Starting and assignment Running in one transaction, then the session is
//! reserved under the attempt's operation id (as an app request id, so a
//! retry resolves to the same session), started, and its receipt bound. A
//! crash anywhere leaves a state the next tick finishes without a second
//! attempt or a second session.
use super::coordinator::{
    in_flight, latest_attempt, Coordinator, Launcher, Policy, Tick, LAUNCH_CONFIRM_TIMEOUT_MS,
    REVIEW,
};
use super::events::append;
use super::policy::{self, LaunchAccount};
use super::recovery;
use super::states::{apply, is_latest_attempt};
use super::store::read_ms;
use super::worker::{claim_event, event, payload_of, settle};
use super::workspaces::{self, PrepareError, Prepared};
use super::{
    Assignment, AssignmentState, Attempt, AttemptState, ClaimOutcome, DecisionState, NewAttempt,
    NewEvent, Requester, State, Subject, Task, TaskError, TaskLifecycle, TaskStore, Workspace,
};
use crate::accounts::Profile;
use crate::agent::Settings;
use crate::app_access;
use crate::orchestration::WorkspaceMode;
use crate::provider::Kind;
use crate::store::{Session, Store};
use crate::usage::Failure;
use rusqlite::{Connection, OptionalExtension};
use serde_json::json;

/// The app caller under which coordinator launches reserve their sessions;
/// the request id is the attempt's operation id.
pub const LAUNCH_CALLER: i64 = 0;
const TITLE_CHARS: usize = 160;
/// Failure event kinds.
const DISPATCH_FAILED: &str = "attempt.dispatch_failed";
const START_FAILED: &str = "attempt.start_failed";
/// A recovery condition: reserved, never confirmed started.
const UNCONFIRMED: &str = "launch.unconfirmed";
/// A session the app started after its attempt stopped waiting for it.
const ORPHANED: &str = "launch.orphaned";

fn storage(error: String) -> TaskError {
    TaskError::Storage(error)
}

impl<P: Policy> Coordinator<P> {
    /// Records the launch intent: the workspace, an attempt with a unique
    /// operation id (or the in-flight one a crash left), the intent event,
    /// and the attempt Starting before the assignment Running. Returns None
    /// when no workspace could be prepared; the task is then Blocked.
    pub(super) fn begin_launch(
        &self,
        store: &Store,
        task: &Task,
        assignment: &Assignment,
        now: u64,
    ) -> Result<Option<Attempt>, TaskError> {
        let tasks = store.tasks()?;
        // Nothing starts beside a worker whose stop is unconfirmed.
        if super::guard::pending_stop(tasks.conn, &task.id, &assignment.id)?.is_some() {
            return Ok(None);
        }
        let prepared = match workspaces::prepare(&tasks, task, assignment, now) {
            Ok(prepared) => prepared,
            Err(PrepareError::Workspace(reason)) => {
                block(&tasks, task, assignment, &reason, now)?;
                return Ok(None);
            }
            Err(PrepareError::Storage(error)) => return Err(error),
        };
        let attempt = match latest_attempt(&tasks, &task.id, &assignment.id)? {
            Some(attempt) if in_flight(&attempt) => attempt,
            _ => {
                // The task's snapshot decides the account (account ids only).
                let policy = policy::capture(store, task, &self.policy.worker_provider(task), now)?;
                let account = match policy::launch_choice(store, task, &policy, now)? {
                    LaunchAccount::Use(account) => account,
                    LaunchAccount::Wait => return Ok(None),
                    LaunchAccount::Block(reason) => {
                        unavailable(&tasks, task, assignment, &reason, now)?;
                        return Ok(None);
                    }
                };
                let id = crate::mono::new_id();
                tasks.create_attempt(
                    &NewAttempt {
                        id: format!("attempt-{id}"),
                        task_id: task.id.clone(),
                        assignment_id: assignment.id.clone(),
                        operation_id: format!("task-launch-{id}"),
                        provider: account.provider,
                        account_id: Some(account.account_id),
                    },
                    now,
                )?
            }
        };
        let tx = tasks.begin()?;
        // A replacement takes over the questions (and answers) of the attempt
        // it replaces.
        super::decisions::inherit(&tx, &attempt, now)?;
        append(&tx, &intent(&attempt, assignment, &prepared), now)?;
        let starting = State::Attempt(AttemptState::Starting);
        settle(
            &tx,
            Subject::Attempt(&attempt.id),
            starting,
            Requester::Coordinator,
            now,
        )?;
        let parent = Subject::Assignment {
            task: &task.id,
            id: &assignment.id,
        };
        let running = State::Assignment(AssignmentState::Running);
        settle(&tx, parent, running, Requester::Coordinator, now)?;
        tx.commit()?;
        tasks.attempt(&attempt.id)
    }

    /// Resolves a Starting attempt to its session: binds one a retried launch
    /// already reserved, otherwise reserves, starts and binds a new one.
    pub(super) fn dispatch(
        &self,
        store: &Store,
        task: &Task,
        attempt: &Attempt,
        launcher: &dyn Launcher,
        now: u64,
        tick: &mut Tick,
    ) -> Result<(), TaskError> {
        let tasks = store.tasks()?;
        let existing = store
            .app_session_for_request(LAUNCH_CALLER, &attempt.operation_id)
            .map_err(storage)?;
        if let Some(session) = existing {
            bind(store, attempt, session, now)?;
            tick.bound.push(attempt.id.clone());
            return Ok(());
        }
        if task.lifecycle != TaskLifecycle::Active {
            return Ok(());
        }
        // Fence: a coordinator that lost the lease must not start a worker.
        self.lease(&tasks, &task.id, now)?;
        let (session, settings, fresh) = match reserve(store, task, attempt) {
            Ok(reserved) => reserved,
            Err(reason) => {
                fail(&tasks, attempt, Some((DISPATCH_FAILED, &reason)), now)?;
                tick.failed.push(attempt.id.clone());
                return Ok(());
            }
        };
        // Durable before the GUI sees it, so its confirmation finds the
        // attempt; the attempt stays Starting until then.
        bind(store, attempt, session.id, now)?;
        if fresh {
            if let Err(reason) = launcher.start(&session, &settings) {
                fail(&tasks, attempt, Some((DISPATCH_FAILED, &reason)), now)?;
                tick.failed.push(attempt.id.clone());
                return Ok(());
            }
        }
        tick.launched.push(attempt.id.clone());
        Ok(())
    }

    /// Finishes or observes the assignment's latest attempt.
    pub(super) fn reconcile(
        &self,
        store: &Store,
        task: &Task,
        assignment: &Assignment,
        launcher: &dyn Launcher,
        now: u64,
        tick: &mut Tick,
    ) -> Result<(), TaskError> {
        let tasks = store.tasks()?;
        let Some(mut attempt) = latest_attempt(&tasks, &task.id, &assignment.id)? else {
            return Ok(());
        };
        match attempt.status {
            AttemptState::Created | AttemptState::Starting => {
                // A replacement created while its assignment still waited for
                // a decision resumes too.
                let resumable = matches!(
                    assignment.status,
                    AssignmentState::Ready | AssignmentState::Running
                ) || (attempt.status == AttemptState::Created
                    && assignment.status == AssignmentState::WaitingForDecision);
                if !resumable {
                    return Ok(());
                }
                let unfinished = attempt.status == AttemptState::Created
                    || assignment.status == AssignmentState::Ready;
                if unfinished {
                    if task.lifecycle != TaskLifecycle::Active {
                        return Ok(());
                    }
                    match self.begin_launch(store, task, assignment, now)? {
                        Some(resumed) => attempt = resumed,
                        None => return Ok(()),
                    }
                }
                if attempt.session_id.is_none() {
                    self.dispatch(store, task, &attempt, launcher, now, tick)?;
                } else if unconfirmed(&tasks, &attempt, now)? {
                    // A crash after the reservation: recovered like any
                    // other failure to start.
                    let reason =
                        "The worker session was reserved but the app never confirmed it started.";
                    launch_failed(store, &attempt, UNCONFIRMED, reason, now)?;
                    tick.failed.push(attempt.id);
                }
            }
            AttemptState::Running | AttemptState::Waiting => {
                let claim = tasks.claim(&attempt.id)?;
                if claim.is_some_and(|c| c.outcome == ClaimOutcome::Failed) {
                    fail(&tasks, &attempt, None, now)?;
                    tick.failed.push(attempt.id);
                }
            }
            AttemptState::Interrupted => {
                self.recover(store, task, assignment, &attempt, launcher, now, tick)?;
            }
            AttemptState::Succeeded => {
                let queued = assignment.status == AssignmentState::NeedsReview
                    && request_review(&tasks, &attempt, now)?;
                if queued {
                    tick.reviews.push(attempt.id);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// The persisted launch intent: everything needed to reconcile it later.
fn intent(attempt: &Attempt, assignment: &Assignment, prepared: &Prepared) -> NewEvent {
    let w = &prepared.workspace;
    let payload = json!({
        "operationId": attempt.operation_id,
        "taskId": attempt.task_id,
        "assignmentId": attempt.assignment_id,
        "attemptId": attempt.id,
        "provider": attempt.provider,
        "workspace": {
            "id": w.id, "mode": w.mode, "repository": w.repository, "path": w.path,
            "branch": w.branch, "baseCommit": w.base_commit, "owner": w.owner,
        },
        // Uncommitted changes in the checkout are not in the worktree.
        "dirtyBase": prepared.dirty_base,
        // What the worker was asked to do; review compares it to the current
        // requirements.
        "requirements": super::review::requirements(assignment),
    });
    let id = format!("launch:{}", attempt.id);
    event(attempt, id, "attempt.launch_intent", payload)
}

/// `sessions.start`'s durable steps, keyed by the operation id: reserve the
/// session, select the workspace, configure it and prepare the worker brief
/// as its first prompt. Returns whether this call reserved it.
pub(super) fn reserve(
    store: &Store,
    task: &Task,
    attempt: &Attempt,
) -> Result<(Session, Settings, bool), String> {
    let tasks = store.tasks().map_err(|e| e.to_string())?;
    let assignment = tasks
        .assignment(&task.id, &attempt.assignment_id)
        .map_err(|e| e.to_string())?
        .ok_or("The assignment no longer exists")?;
    let workspace = workspace_of(&tasks, &task.id, &assignment.id)?;
    let kind = Kind::parse(&attempt.provider).ok_or("Unknown worker provider")?;
    let correction = tasks
        .latest_correction(&task.id, &assignment.id)
        .map_err(|e| e.to_string())?;
    let decisions = super::decisions::assignment_decisions(&tasks, &task.id, &assignment.id)
        .map_err(|e| e.to_string())?;
    let prompt = super::brief::worker_brief(
        task,
        &assignment,
        &workspace,
        correction.as_ref(),
        &decisions,
    );
    // Mono's reason for sending the previous attempt back, if it did.
    let previous = tasks
        .attempts(&task.id, &assignment.id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .rev()
        .find(|a| a.id != attempt.id);
    let note = match previous {
        Some(previous) => payload_of(tasks.conn, &super::brief::mono_note_event(&previous.id))
            .map_err(|e| e.to_string())?
            .and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok())
            .and_then(|p| p["reason"].as_str().map(str::to_owned)),
        None => None,
    };
    let prompt = match note {
        Some(reason) => format!("{prompt}\n\nMono sent the previous attempt back: {reason}"),
        None => prompt,
    };
    // Requirements revised by steering since the plan was made.
    let steered = super::steer::brief_note(&tasks, &task.id).map_err(|e| e.to_string())?;
    let prompt = match steered {
        Some(note) => format!("{prompt}\n\n{note}"),
        None => prompt,
    };
    // A replacement continues from its durable hand-off.
    let handoff = super::handoff::of(&tasks, &attempt.id).map_err(|e| e.to_string())?;
    let prompt = match &handoff {
        Some(handoff) => format!("{prompt}\n\n{}", super::handoff::text(handoff)),
        None => prompt,
    };
    let title: String = format!("{}: {}", task.title, assignment.title)
        .chars()
        .take(TITLE_CHARS)
        .collect();
    let mut input = json!({"prompt": prompt, "title": title});
    if workspace.mode == WorkspaceMode::Worktree {
        input["worktreeCwd"] = json!(workspace.path);
    }
    // The account recovery or the allowlist chose, and the billing choice
    // snapshotted when the task was created; never the live preference.
    let snapshot = policy::of(&tasks, &task.id).map_err(|e| e.to_string())?;
    let account = attempt
        .account_id
        .clone()
        .or_else(|| snapshot.as_ref().map(|p| p.start_account.clone()))
        .unwrap_or_else(|| crate::accounts::DEFAULT.into());
    let profile = match store.data_dir() {
        Some(root) => Profile::new(&account, root),
        None => Profile {
            id: account,
            root: None,
        },
    };
    let billing = snapshot.as_ref().is_some_and(|p| p.use_api_key);
    let base = Settings {
        account: profile.clone(),
        use_api_key: billing,
        api_key_pin: Some(billing),
        ..Settings::default()
    };
    let models = crate::catalog::cached_models_for(kind, &profile).unwrap_or_default();
    let mut settings = app_access::launch_settings(&input, kind.id(), base, kind, &models)?;
    settings.revision = 0;
    // Validates and selects the recorded worktree before anything is
    // reserved; the session id only names worktrees this never creates.
    app_access::launch_checkout(&input, &task.project, 0, &mut settings)?;
    let fingerprint = app_access::fingerprint("tasks.launch", &json!(attempt.operation_id));
    let (mut session, fresh) = store.create_app_session(
        LAUNCH_CALLER,
        &attempt.operation_id,
        &fingerprint,
        kind.id(),
        &task.project,
    )?;
    let mut resumed = false;
    if fresh {
        store.title(session.id, &title)?;
        session.title = title;
        store.configure_agent(session.id, kind.id(), &settings)?;
        store.prepare_app_prompt(session.id, &attempt.operation_id, &prompt)?;
        session.has_messages = true;
        // Resume the failed worker's provider thread when it can move to
        // this account; otherwise the hand-off alone carries the work.
        if let Some(handoff) = &handoff {
            if let Some(thread) =
                resumable_thread(store, &tasks, &handoff.from_attempt, &profile, kind)
            {
                store.provider_id(session.id, &thread)?;
                session.provider_id = Some(thread);
                resumed = true;
            }
        }
    }
    // The answers this brief carries; confirming the start hands them over.
    let carried: Vec<_> = decisions
        .iter()
        .filter(|d| d.state == DecisionState::Answered)
        .map(|d| json!({"id": d.id, "revision": d.revision}))
        .collect();
    let briefed = event(
        attempt,
        briefed_event(&attempt.id),
        "attempt.briefed",
        json!({"decisions": carried, "resumedThread": resumed}),
    );
    tasks
        .append_event(&briefed, attempt.updated_at)
        .map_err(|e| e.to_string())?;
    Ok((session, settings, fresh))
}

/// The provider thread of the attempt `from`, carried to `to`'s account
/// home when it differs. None when it had no thread, the provider cannot
/// resume, or the transfer fails.
fn resumable_thread(
    store: &Store,
    tasks: &TaskStore<'_>,
    from: &str,
    to: &Profile,
    kind: Kind,
) -> Option<String> {
    if !matches!(kind, Kind::Codex | Kind::Claude) {
        return None;
    }
    let previous = tasks.attempt(from).ok()??;
    if previous.provider != kind.id() {
        return None;
    }
    let handle = store
        .resolve_session_id(previous.session_id.as_deref()?)
        .ok()?;
    let thread = store
        .sessions()
        .ok()?
        .into_iter()
        .find(|s| s.id == handle)?
        .provider_id?;
    let account = previous
        .account_id
        .unwrap_or_else(|| crate::accounts::DEFAULT.into());
    if account != to.id {
        let root = store.data_dir()?;
        Profile::new(&account, root)
            .transfer(to, kind, &thread)
            .ok()?;
    }
    Some(thread)
}

fn workspace_of(tasks: &TaskStore<'_>, task: &str, assignment: &str) -> Result<Workspace, String> {
    tasks
        .workspaces(task)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|w| w.assignment_id.as_deref() == Some(assignment))
        .ok_or_else(|| format!("No workspace is recorded for assignment {assignment}"))
}

/// Records the session a launch reserved. The attempt stays Starting: a
/// reserved session is not a running worker until the app confirms it
/// ([`confirm_started`]).
fn bind(store: &Store, attempt: &Attempt, session: i64, now: u64) -> Result<(), TaskError> {
    let tasks = store.tasks()?;
    let bound = store.persisted_session_id(session).map_err(storage)?;
    let tx = tasks.begin()?;
    let current = tasks
        .attempt(&attempt.id)?
        .ok_or_else(|| TaskError::NotFound(format!("attempt:{}", attempt.id)))?;
    if current.session_id.is_none() {
        tasks.bind_attempt_session(&attempt.id, current.revision, &bound, now)?;
    }
    let payload = json!({"sessionId": bound, "operationId": attempt.operation_id});
    let receipt = event(
        attempt,
        reserved_event(&attempt.id),
        "attempt.reserved",
        payload,
    );
    append(&tx, &receipt, now)?;
    tx.commit()?;
    Ok(())
}

fn briefed_event(attempt: &str) -> String {
    format!("briefed:{attempt}")
}

/// Resolves the answers the attempt's brief carried, if they are still
/// current for it, now that the brief reached the worker.
fn hand_over_briefed(conn: &Connection, attempt: &Attempt, now: u64) -> Result<(), TaskError> {
    let tasks = TaskStore { conn };
    let Some(briefed) = payload_of(conn, &briefed_event(&attempt.id))? else {
        return Ok(());
    };
    let briefed: serde_json::Value = serde_json::from_str(&briefed)?;
    for carried in briefed["decisions"].as_array().into_iter().flatten() {
        let (Some(id), Some(revision)) = (carried["id"].as_str(), carried["revision"].as_u64())
        else {
            continue;
        };
        let current = tasks.decision(id)?.filter(|d| {
            d.state == DecisionState::Answered
                && d.revision == revision
                && d.attempt_id.as_deref() == Some(attempt.id.as_str())
        });
        if let Some(decision) = current {
            super::decisions::resolve(conn, &decision, &attempt.id, "brief", now)?;
        }
    }
    Ok(())
}

fn reserved_event(attempt: &str) -> String {
    format!("reserved:{attempt}")
}

/// Whether the attempt's session was reserved longer than
/// [`LAUNCH_CONFIRM_TIMEOUT_MS`] ago without the app confirming it started.
fn unconfirmed(tasks: &TaskStore<'_>, attempt: &Attempt, now: u64) -> Result<bool, TaskError> {
    let reserved: Option<u64> = tasks
        .conn
        .query_row(
            "SELECT created_at FROM native_task_events WHERE id=?1",
            [reserved_event(&attempt.id)],
            |row| read_ms(row, 0),
        )
        .optional()?;
    let since = reserved.unwrap_or(attempt.updated_at);
    Ok(now >= since.saturating_add(LAUNCH_CONFIRM_TIMEOUT_MS))
}

/// The coordinator attempt bound to the app session `session`, if any.
fn launched_attempt(store: &Store, session: i64) -> Result<Option<Attempt>, TaskError> {
    let bound = store.persisted_session_id(session).map_err(storage)?;
    store.tasks()?.attempt_for_session(&bound)
}

/// The app installed and submitted the session it was handed: its Starting
/// attempt becomes Running, so the worker's calls are accepted. Returns
/// false when there was nothing to move (a duplicate confirmation, or not a
/// task worker).
pub fn confirm_started(store: &Store, session: i64, now: u64) -> Result<bool, TaskError> {
    let Some(attempt) = launched_attempt(store, session)? else {
        return Ok(false);
    };
    if attempt.status != AttemptState::Starting {
        orphaned(store, &attempt, now)?;
        return Ok(false);
    }
    let tasks = store.tasks()?;
    let tx = tasks.begin()?;
    let payload = json!({"sessionId": attempt.session_id});
    let id = format!("started:{}", attempt.id);
    append(&tx, &event(&attempt, id, "attempt.started", payload), now)?;
    let running = State::Attempt(AttemptState::Running);
    let subject = Subject::Attempt(&attempt.id);
    settle(&tx, subject, running, Requester::Coordinator, now)?;
    hand_over_briefed(&tx, &attempt, now)?;
    tx.commit()?;
    Ok(true)
}

/// The app could not start the session it was handed (no window, closing):
/// the attempt fails with the reason, under the coordinator's authority.
pub fn start_failed(store: &Store, session: i64, reason: &str, now: u64) -> Result<(), TaskError> {
    let Some(attempt) = launched_attempt(store, session)? else {
        return Ok(());
    };
    if attempt.status != AttemptState::Starting {
        return Ok(());
    }
    launch_failed(store, &attempt, START_FAILED, reason, now)
}

/// A launch that never started: the failure event, then recovery as a
/// temporary failure (backoff, the retry budget, a replacement).
fn launch_failed(
    store: &Store,
    attempt: &Attempt,
    kind: &str,
    reason: &str,
    now: u64,
) -> Result<(), TaskError> {
    let tasks = store.tasks()?;
    let id = format!("{kind}:{}", attempt.id);
    let payload = json!({"reason": reason});
    tasks.append_event(&event(attempt, id, kind, payload), now)?;
    recovery::interrupt(store, &attempt.id, Failure::Temporary, reason, None, now)?;
    Ok(())
}

/// The app started a session after its attempt stopped waiting for it (it
/// timed out, failed or was cancelled): a running worker nobody owns. It is
/// recorded once and stopped through the stop path, so two agents never
/// write one worktree.
fn orphaned(store: &Store, attempt: &Attempt, now: u64) -> Result<(), TaskError> {
    let finished = matches!(
        attempt.status,
        AttemptState::Interrupted | AttemptState::Failed | AttemptState::Cancelled
    );
    let Some(session) = attempt.session_id.as_deref().filter(|_| finished) else {
        return Ok(());
    };
    let tasks = store.tasks()?;
    let id = format!("orphaned:{}", attempt.id);
    let payload = json!({"sessionId": session, "status": attempt.status});
    let tx = tasks.begin()?;
    let inside = TaskStore { conn: &tx };
    let stored = append(&tx, &event(attempt, id, ORPHANED, payload), now)?;
    let reference = format!("session:{session}");
    inside.enqueue_delivery(
        &format!("stop-orphan:{}", attempt.id),
        &stored.id,
        recovery::WORKER_STOP,
        &reference,
        now,
    )?;
    tx.commit()?;
    Ok(())
}

/// No allowed account can run a fresh launch: the task is Blocked with why.
fn unavailable(
    tasks: &TaskStore<'_>,
    task: &Task,
    assignment: &Assignment,
    reason: &str,
    now: u64,
) -> Result<(), TaskError> {
    let tx = tasks.begin()?;
    recovery::block(&tx, &task.id, reason, now)?;
    let revision = TaskStore { conn: &tx }.revision(Subject::Task(&task.id))?;
    let payload = json!({"assignmentId": assignment.id, "reason": reason});
    let unavailable = NewEvent {
        id: format!("account_unavailable:{}@{revision}", task.id),
        task_id: task.id.clone(),
        assignment_id: Some(assignment.id.clone()),
        attempt_id: None,
        kind: "task.account_unavailable".into(),
        payload,
    };
    append(&tx, &unavailable, now)?;
    tx.commit()?;
    Ok(())
}

/// The coordinator's own classification: the attempt Failed, and its
/// assignment Failed too when it is the current run. No retry here; the
/// assignment stays Failed until recovery or Mono queues it again.
/// `why` is the failure event's kind and reason, when the attempt did not
/// fail by its own claim.
fn fail(
    tasks: &TaskStore<'_>,
    attempt: &Attempt,
    why: Option<(&str, &str)>,
    now: u64,
) -> Result<(), TaskError> {
    let tx = tasks.begin()?;
    if let Some((kind, reason)) = why {
        let id = format!("{kind}:{}", attempt.id);
        let payload = json!({"reason": reason});
        append(&tx, &event(attempt, id, kind, payload), now)?;
    }
    let subject = Subject::Attempt(&attempt.id);
    settle(
        &tx,
        subject,
        State::Attempt(AttemptState::Failed),
        Requester::Coordinator,
        now,
    )?;
    if is_latest_attempt(&tx, subject)? {
        fail_assignment(&tx, attempt, now)?;
    }
    tx.commit()?;
    Ok(())
}

pub(super) fn fail_assignment(
    conn: &Connection,
    attempt: &Attempt,
    now: u64,
) -> Result<(), TaskError> {
    let tasks = TaskStore { conn };
    let parent = Subject::Assignment {
        task: &attempt.task_id,
        id: &attempt.assignment_id,
    };
    let failed = State::Assignment(AssignmentState::Failed);
    let status = tasks.state(parent, failed)?;
    if matches!(
        status,
        State::Assignment(AssignmentState::Running | AssignmentState::WaitingForDecision)
    ) {
        let revision = tasks.revision(parent)?;
        apply(conn, parent, failed, Requester::Coordinator, revision, now)?;
    }
    Ok(())
}

/// Queues one review of the current attempt's succeeded claim; a duplicated
/// result or a repeated tick finds the same delivery. Returns whether it was
/// queued now.
fn request_review(tasks: &TaskStore<'_>, attempt: &Attempt, now: u64) -> Result<bool, TaskError> {
    let id = format!("review:{}", attempt.id);
    if tasks.delivery(&id)?.is_some()
        || tasks.claim(&attempt.id)?.is_none()
        || !is_latest_attempt(tasks.conn, Subject::Attempt(&attempt.id))?
    {
        return Ok(false);
    }
    let reference = format!("attempt:{}", attempt.id);
    tasks.enqueue_delivery(&id, &claim_event(&attempt.id), REVIEW, &reference, now)?;
    Ok(true)
}

/// No workspace could be prepared: the task is Blocked with the reason, and
/// nothing was launched.
fn block(
    tasks: &TaskStore<'_>,
    task: &Task,
    assignment: &Assignment,
    reason: &str,
    now: u64,
) -> Result<(), TaskError> {
    let tx = tasks.begin()?;
    let subject = Subject::Task(&task.id);
    let revision = tasks.revision(subject)?;
    let blocked = State::Lifecycle(TaskLifecycle::Blocked);
    let revision = apply(&tx, subject, blocked, Requester::Coordinator, revision, now)?;
    tx.execute(
        "UPDATE native_tasks SET blocked_reason=?2 WHERE id=?1",
        [&task.id, reason],
    )?;
    let payload = json!({"assignmentId": assignment.id, "reason": reason});
    let failed = NewEvent {
        id: format!("workspace_failed:{}@{revision}", task.id),
        task_id: task.id.clone(),
        assignment_id: Some(assignment.id.clone()),
        attempt_id: None,
        kind: "task.workspace_failed".into(),
        payload,
    };
    append(&tx, &failed, now)?;
    tx.commit()?;
    Ok(())
}
