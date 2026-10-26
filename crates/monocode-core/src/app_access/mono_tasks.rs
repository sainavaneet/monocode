//! Mono's task actions: answer or escalate worker questions, and resolve the
//! review hand-offs the coordinator could not decide. The calling Mono comes
//! from the bridge's session binding; a task belongs to it only when the
//! task's `mono_id` names it, and another Mono's task reads as not found.
use super::{optional_text, text};
use crate::orchestration::WorkspaceMode;
use crate::store::Store;
use crate::tasks::completion::CriterionRun;
use crate::tasks::decisions::Escalation;
use crate::tasks::{
    AssignmentState, DecisionRequest, NewEvent, Requester, State, Subject, Task, TaskError,
    TaskStore, Verification,
};
use serde_json::{json, Value};

pub const MONO_TASK_ACTIONS: [&str; 6] = [
    "tasks.decisions.list",
    "tasks.decisions.answer",
    "tasks.decisions.escalate",
    "tasks.review.accept",
    "tasks.review.sendBack",
    "tasks.criteria.verify",
];
/// Escalation alternatives: at most this many, each up to the byte limit.
pub const MAX_ALTERNATIVES: usize = 6;
pub const MAX_ALTERNATIVE_BYTES: usize = 500;
const MAX_ANSWER: usize = 20_000;
const MAX_WHY: usize = 4_000;
const MAX_CHECK: usize = 2_000;
const OFF: &str =
    "Task coordination is off (native-task-coordinator), so Mono's task actions are unavailable.";

pub(super) fn failed(error: TaskError) -> String {
    error.to_string()
}

pub(super) fn revision(input: &Value) -> Result<u64, String> {
    input["revision"]
        .as_u64()
        .ok_or_else(|| "Provide revision (the one you read).".into())
}

/// The task if `mono` owns it.
pub(super) fn owned_task(tasks: &TaskStore<'_>, mono: &str, id: &str) -> Result<Task, String> {
    tasks
        .task(id)
        .map_err(failed)?
        .filter(|t| t.mono_id.as_deref() == Some(mono))
        .ok_or_else(|| failed(TaskError::NotFound(format!("task:{id}"))))
}

/// The decision if its task belongs to `mono`.
fn owned_decision(tasks: &TaskStore<'_>, mono: &str, id: &str) -> Result<DecisionRequest, String> {
    let missing = || failed(TaskError::NotFound(format!("decision:{id}")));
    let decision = tasks.decision(id).map_err(failed)?.ok_or_else(missing)?;
    owned_task(tasks, mono, &decision.task_id).map_err(|_| missing())?;
    Ok(decision)
}

/// The gate every Mono task action passes: its fields are valid and task
/// coordination is on.
pub(super) fn ready(store: &Store, action: &str, input: &Value) -> Result<(), String> {
    crate::mono::api::validate(action, input)?;
    if !crate::tasks::coordinator::enabled(store) {
        return Err(OFF.into());
    }
    Ok(())
}

/// Runs a Mono task action for `mono`, the Mono bound to the calling
/// session. Nothing happens unless the task coordinator is enabled.
pub fn mono_task_action(
    store: &Store,
    mono: &str,
    action: &str,
    input: &Value,
    now: u64,
) -> Result<Value, String> {
    if !MONO_TASK_ACTIONS.contains(&action) {
        return Err("Unknown task action. Run app help".into());
    }
    ready(store, action, input)?;
    let tasks = store.tasks().map_err(failed)?;
    match action {
        "tasks.decisions.list" => list(&tasks, mono),
        "tasks.decisions.answer" => {
            let decision = owned_decision(&tasks, mono, &text(input, "decisionId", 512)?)?;
            let answer = text(input, "answer", MAX_ANSWER)?;
            let revision = tasks
                .answer_as(
                    &decision.id,
                    revision(input)?,
                    &answer,
                    Requester::Mono,
                    now,
                )
                .map_err(failed)?;
            Ok(json!({"decisionId": decision.id, "revision": revision, "state": "answered"}))
        }
        "tasks.decisions.escalate" => {
            let decision = owned_decision(&tasks, mono, &text(input, "decisionId", 512)?)?;
            let escalation = Escalation {
                question: optional_text(input, "question", MAX_WHY)?,
                why: text(input, "why", MAX_WHY)?,
                recommendation: text(input, "recommendation", MAX_WHY)?,
                alternatives: input["alternatives"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|a| a.trim().to_owned())
                    .filter(|a| !a.is_empty())
                    .collect(),
            };
            let card = tasks
                .escalate(&decision.id, revision(input)?, &escalation, now)
                .map_err(failed)?;
            Ok(
                json!({"decisionId": decision.id, "revision": card["revision"], "state": "open", "card": card}),
            )
        }
        "tasks.review.accept" | "tasks.review.sendBack" => review(&tasks, mono, action, input, now),
        _ => {
            let run = prepare_mono_verify(store, mono, input)?;
            finish_mono_verify(store, &run, run.run(now))
        }
    }
}

fn list(tasks: &TaskStore<'_>, mono: &str) -> Result<Value, String> {
    let mut entries = Vec::new();
    for d in tasks.mono_decisions(mono).map_err(failed)? {
        let task = tasks.task(&d.task_id).map_err(failed)?;
        let assignment = match &d.assignment_id {
            Some(id) => tasks.assignment(&d.task_id, id).map_err(failed)?,
            None => None,
        };
        let prior: Vec<Value> = tasks
            .prior_decisions(&d.task_id)
            .map_err(failed)?
            .into_iter()
            .filter(|p| p.id != d.id)
            .map(|p| json!({"id": p.id, "question": p.question, "answer": p.answer}))
            .collect();
        entries.push(json!({
            "id": d.id, "revision": d.revision, "state": d.state, "scope": d.scope,
            "question": d.question, "context": d.context, "askedBy": d.asked_by,
            "escalated": tasks.escalation(&d.id).map_err(failed)?.is_some(),
            "providerQuestion": tasks.provider_question(&d.id).map_err(failed)?.is_some(),
            "answer": d.answer, "answeredBy": d.answered_by, "attemptId": d.attempt_id,
            "task": task.map(|t| json!({"id": t.id, "title": t.title, "request": t.request,
                "stage": t.stage, "lifecycle": t.lifecycle})),
            "assignment": assignment.map(|a| json!({"id": a.id, "title": a.title,
                "prompt": a.prompt, "files": a.files, "dependsOn": a.depends_on,
                "status": a.status, "revision": a.revision})),
            "prior": prior,
        }));
    }
    Ok(json!({"decisions": entries}))
}

/// Mono accepts reviewed work or sends it back through the transition table;
/// a send-back keeps Mono's reason for the next attempt's brief.
fn review(
    tasks: &TaskStore<'_>,
    mono: &str,
    action: &str,
    input: &Value,
    now: u64,
) -> Result<Value, String> {
    let task = owned_task(tasks, mono, &text(input, "taskId", 512)?)?;
    let id = text(input, "assignmentId", 512)?;
    let subject = Subject::Assignment {
        task: &task.id,
        id: &id,
    };
    let to = if action == "tasks.review.accept" {
        AssignmentState::Accepted
    } else {
        AssignmentState::Queued
    };
    let reason = if to == AssignmentState::Queued {
        Some(text(input, "reason", MAX_ANSWER)?)
    } else {
        None
    };
    let revision = tasks
        .transition(
            subject,
            State::Assignment(to),
            Requester::Mono,
            revision(input)?,
            now,
        )
        .map_err(failed)?;
    if let Some(reason) = reason {
        let attempt = tasks.attempts(&task.id, &id).map_err(failed)?.pop();
        if let Some(attempt) = attempt {
            let note = NewEvent {
                id: crate::tasks::brief::mono_note_event(&attempt.id),
                task_id: task.id.clone(),
                assignment_id: Some(id.clone()),
                attempt_id: Some(attempt.id),
                kind: "assignment.mono_note".into(),
                payload: json!({"reason": reason}),
            };
            tasks.append_event(&note, now).map_err(failed)?;
        }
    }
    Ok(json!({"assignment": {"id": id, "status": to, "revision": revision}}))
}

/// The first step of `tasks.criteria.verify`, under the store: checks the
/// action, the flag and ownership and picks the workspace. The app runs the
/// check ([`CriterionRun::run`]) without the store lock, then records it
/// with [`finish_mono_verify`].
pub fn prepare_mono_verify(
    store: &Store,
    mono: &str,
    input: &Value,
) -> Result<CriterionRun, String> {
    ready(store, "tasks.criteria.verify", input)?;
    let tasks = store.tasks().map_err(failed)?;
    let task = owned_task(&tasks, mono, &text(input, "taskId", 512)?)?;
    let criterion = text(input, "criterionId", 512)?;
    let check = text(input, "check", MAX_CHECK)?;
    let named = optional_text(input, "assignmentId", 512)?;
    let worktrees: Vec<_> = tasks
        .workspaces(&task.id)
        .map_err(failed)?
        .into_iter()
        .filter(|w| match &named {
            Some(id) => w.assignment_id.as_deref() == Some(id.as_str()),
            None => w.mode == WorkspaceMode::Worktree,
        })
        .collect();
    let [workspace] = worktrees.as_slice() else {
        return Err(
            "Name the assignment whose workspace shows the criterion (assignmentId): the task has no single result worktree."
                .into(),
        );
    };
    CriterionRun::prepare(store, &task.id, &criterion, workspace, &check).map_err(failed)
}

/// The last step of `tasks.criteria.verify`, under the store: records the
/// check's verification unless the task changed while it ran.
pub fn finish_mono_verify(
    store: &Store,
    run: &CriterionRun,
    outcome: Result<Verification, TaskError>,
) -> Result<Value, String> {
    let verification = outcome.map_err(failed)?;
    run.record(store, &verification).map_err(failed)?;
    Ok(json!({
        "verificationId": verification.id, "criterionId": run.criterion_id,
        "passed": verification.passed, "checkpoint": verification.checkpoint_commit,
        "workspace": run.workspace.path,
    }))
}
