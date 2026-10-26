//! Launch intents, dispatch through `sessions.start`'s durable steps, receipts
//! and the coordinator's reading of worker claims.
//!
//! Order: workspace recorded, attempt created, intent event plus attempt
//! Starting and assignment Running in one transaction, then the session is
//! reserved under the attempt's operation id (as an app request id, so a
//! retry resolves to the same session), started, and its receipt bound. A
//! crash anywhere leaves a state the next tick finishes without a second
//! attempt or a second session.
use super::coordinator::{in_flight, latest_attempt, Coordinator, Launcher, Policy, Tick, REVIEW};
use super::events::append;
use super::states::{apply, is_latest_attempt};
use super::worker::{claim_event, event, settle};
use super::workspaces::{self, Prepared};
use super::{
    Assignment, AssignmentState, Attempt, AttemptState, ClaimOutcome, NewAttempt, NewEvent,
    Requester, State, Subject, Task, TaskError, TaskLifecycle, TaskStore, Workspace,
};
use crate::agent::Settings;
use crate::app_access;
use crate::orchestration::WorkspaceMode;
use crate::provider::Kind;
use crate::store::{Session, Store};
use rusqlite::Connection;
use serde_json::json;

/// The app caller under which coordinator launches reserve their sessions;
/// the request id is the attempt's operation id.
pub const LAUNCH_CALLER: i64 = 0;
const TITLE_CHARS: usize = 160;

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
        let prepared = match workspaces::prepare(&tasks, task, assignment, now) {
            Ok(prepared) => prepared,
            Err(reason) => {
                block(&tasks, task, assignment, &reason, now)?;
                return Ok(None);
            }
        };
        let attempt = match latest_attempt(&tasks, &task.id, &assignment.id)? {
            Some(attempt) if in_flight(&attempt) => attempt,
            _ => {
                let id = crate::mono::new_id();
                tasks.create_attempt(
                    &NewAttempt {
                        id: format!("attempt-{id}"),
                        task_id: task.id.clone(),
                        assignment_id: assignment.id.clone(),
                        operation_id: format!("task-launch-{id}"),
                        provider: self.policy.worker_provider(task),
                        account_id: None,
                    },
                    now,
                )?
            }
        };
        let tx = tasks.begin()?;
        append(&tx, &intent(&attempt, &prepared), now)?;
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
            bind(store, attempt, Some(session), now)?;
            tick.bound.push(attempt.id.clone());
            return Ok(());
        }
        if task.lifecycle != TaskLifecycle::Active {
            return Ok(());
        }
        // Fence: a coordinator that lost the lease must not start a worker.
        self.lease(&tasks, &task.id, now)?;
        let reserved = reserve(store, task, attempt);
        let started = reserved.and_then(|(session, settings, fresh)| {
            if fresh {
                launcher.start(&session, &settings)?;
            }
            Ok(session)
        });
        match started {
            Ok(session) => {
                bind(store, attempt, Some(session.id), now)?;
                tick.launched.push(attempt.id.clone());
            }
            Err(reason) => {
                fail(&tasks, attempt, Some(&reason), now)?;
                tick.failed.push(attempt.id.clone());
            }
        }
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
                let resumable = matches!(
                    assignment.status,
                    AssignmentState::Ready | AssignmentState::Running
                );
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
                } else {
                    bind(store, &attempt, None, now)?;
                }
            }
            AttemptState::Running | AttemptState::Waiting => {
                let claim = tasks.claim(&attempt.id)?;
                if claim.is_some_and(|c| c.outcome == ClaimOutcome::Failed) {
                    fail(&tasks, &attempt, None, now)?;
                    tick.failed.push(attempt.id);
                }
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
fn intent(attempt: &Attempt, prepared: &Prepared) -> NewEvent {
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
    });
    let id = format!("launch:{}", attempt.id);
    event(attempt, id, "attempt.launch_intent", payload)
}

/// `sessions.start`'s durable steps, keyed by the operation id: reserve the
/// session, select the workspace, configure it and prepare the worker brief
/// as its first prompt. Returns whether this call reserved it.
fn reserve(
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
    let prompt = worker_brief(task, &assignment, &workspace);
    let title: String = format!("{}: {}", task.title, assignment.title)
        .chars()
        .take(TITLE_CHARS)
        .collect();
    let mut input = json!({"prompt": prompt, "title": title});
    if workspace.mode == WorkspaceMode::Worktree {
        input["worktreeCwd"] = json!(workspace.path);
    }
    let models = crate::catalog::cached_models_for(kind, &Default::default()).unwrap_or_default();
    let mut settings =
        app_access::launch_settings(&input, kind.id(), Settings::default(), kind, &models)?;
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
    if fresh {
        store.title(session.id, &title)?;
        session.title = title;
        store.configure_agent(session.id, kind.id(), &settings)?;
        store.prepare_app_prompt(session.id, &attempt.operation_id, &prompt)?;
        session.has_messages = true;
    }
    Ok((session, settings, fresh))
}

fn workspace_of(tasks: &TaskStore<'_>, task: &str, assignment: &str) -> Result<Workspace, String> {
    tasks
        .workspaces(task)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|w| w.assignment_id.as_deref() == Some(assignment))
        .ok_or_else(|| format!("No workspace is recorded for assignment {assignment}"))
}

/// Records the session the launch resolved to (None: already recorded) and
/// moves the attempt to Running, so the worker's calls are accepted.
fn bind(store: &Store, attempt: &Attempt, session: Option<i64>, now: u64) -> Result<(), TaskError> {
    let tasks = store.tasks()?;
    let bound = match (&attempt.session_id, session) {
        (Some(id), _) => id.clone(),
        (None, Some(handle)) => store.persisted_session_id(handle).map_err(storage)?,
        (None, None) => return Err(TaskError::Invalid("No session to bind.".into())),
    };
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
        format!("receipt:{}", attempt.id),
        "attempt.receipt",
        payload,
    );
    append(&tx, &receipt, now)?;
    if current.status == AttemptState::Starting {
        let running = State::Attempt(AttemptState::Running);
        settle(
            &tx,
            Subject::Attempt(&attempt.id),
            running,
            Requester::Coordinator,
            now,
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// The coordinator's own classification: the attempt Failed, and its
/// assignment Failed too when it is the current run. No retry here; the
/// assignment stays Failed until recovery or Mono queues it again.
fn fail(
    tasks: &TaskStore<'_>,
    attempt: &Attempt,
    reason: Option<&str>,
    now: u64,
) -> Result<(), TaskError> {
    let tx = tasks.begin()?;
    if let Some(reason) = reason {
        let id = format!("dispatch_failed:{}", attempt.id);
        let payload = json!({"reason": reason});
        append(
            &tx,
            &event(attempt, id, "attempt.dispatch_failed", payload),
            now,
        )?;
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

fn fail_assignment(conn: &Connection, attempt: &Attempt, now: u64) -> Result<(), TaskError> {
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

/// Everything a worker needs, since it does not inherit Mono's conversation.
pub fn worker_brief(task: &Task, assignment: &Assignment, workspace: &Workspace) -> String {
    let list = |items: &[String], none: &str| {
        if items.is_empty() {
            none.to_owned()
        } else {
            items.join(", ")
        }
    };
    let place = match workspace.mode {
        WorkspaceMode::Worktree => format!(
            "your own worktree {} on branch {} from commit {}. Commit your work there; never touch the main checkout.",
            workspace.path,
            workspace.branch.as_deref().unwrap_or("?"),
            workspace.base_commit.as_deref().unwrap_or("?"),
        ),
        WorkspaceMode::Checkout => format!("the project checkout {}.", workspace.path),
    };
    let files = if assignment.files.is_empty() {
        "None. This is an investigation: read and report. Do not edit files.".to_owned()
    } else {
        format!(
            "{}. If you need a change outside them, report it as blocked.",
            assignment.files.join(", ")
        )
    };
    format!(
        "You are a worker Mono launched for task \"{task_title}\", assignment {id}: {title}.\n\
         You do not have Mono's conversation; everything you need is here.\n\n\
         Objective:\n{prompt}\n\n\
         Workspace: {place}\n\
         Files you may change: {files}\n\
         Depends on (already accepted): {deps}\n\
         Expected verification: {checks}\n\n\
         Report through the app commands (`monocode-gpui app ACTION --json '{{...}}'`); the task and attempt come from this session:\n\
         - tasks.worker.status {{}} shows your assignment and answered decisions.\n\
         - tasks.worker.ask {{question, context?, scope?}} when you need a decision; then wait for the answer.\n\
         - tasks.worker.blocked {{reason}} if you cannot continue.\n\
         - tasks.worker.result {{outcome: \"succeeded\"|\"failed\", summary, workspace, commits, validation, evidence, remainingWork, limitations}} once, when done. It is a claim Mono reviews against the acceptance criteria; it does not complete the task.",
        task_title = task.title,
        id = assignment.id,
        title = assignment.title,
        prompt = assignment.prompt,
        deps = list(&assignment.depends_on, "none"),
        checks = list(&assignment.verification, "run focused checks and report them"),
    )
}
