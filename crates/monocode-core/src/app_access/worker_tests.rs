//! Worker protocol commands: identity comes from the authenticated app
//! session's attempt binding, never from ids in the request.
use super::*;
use crate::orchestration::{Plan, Task as Assignment};
use crate::tasks::{
    AssignmentState, AttemptState, DecisionScope, DecisionState, NewAttempt, NewTask, Requester,
    State, Subject, TaskLifecycle, TaskStage,
};
use std::path::Path;

const NOW: u64 = 10;

/// A task Implementing, assignment `a` Running, attempt `x` Running and
/// bound to the returned caller's session.
fn fixture() -> (Store, i64) {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let caller = store.create("codex", "/project").unwrap().id;
    let tasks = store.tasks().unwrap();
    tasks
        .create_task(
            &NewTask {
                id: "t".into(),
                mono_id: Some("mono-1".into()),
                project: "/project".into(),
                title: "Ship it".into(),
                request: "Implement the feature".into(),
                criteria: vec![],
            },
            1,
        )
        .unwrap();
    let plan = Plan {
        title: "Plan".into(),
        summary: "Approach".into(),
        tasks: vec![Assignment {
            id: "a".into(),
            title: "Assignment a".into(),
            prompt: "Do the work".into(),
            files: vec!["src".into()],
            verification: vec!["cargo test".into()],
            ..Assignment::default()
        }],
    };
    tasks.save_plan("t", 1, &plan, 1).unwrap();
    let task = Subject::Task("t");
    for (to, by) in [
        (State::Stage(TaskStage::Ready), Requester::Mono),
        (
            State::Stage(TaskStage::Implementing),
            Requester::Coordinator,
        ),
    ] {
        let rev = tasks.revision(task).unwrap();
        tasks.transition(task, to, by, rev, 1).unwrap();
    }
    let a = Subject::Assignment { task: "t", id: "a" };
    for to in [AssignmentState::Ready, AssignmentState::Running] {
        let rev = tasks.revision(a).unwrap();
        tasks
            .transition(a, State::Assignment(to), Requester::Coordinator, rev, 1)
            .unwrap();
    }
    launch(&store, "x", caller);
    (store, caller)
}

/// Records attempt `id` for assignment `a`, starts it and binds `session`.
fn launch(store: &Store, id: &str, session: i64) {
    let tasks = store.tasks().unwrap();
    tasks
        .create_attempt(
            &NewAttempt {
                id: id.into(),
                task_id: "t".into(),
                assignment_id: "a".into(),
                operation_id: format!("launch-{id}"),
                provider: "codex".into(),
                account_id: None,
            },
            1,
        )
        .unwrap();
    let x = Subject::Attempt(id);
    for to in [AttemptState::Starting, AttemptState::Running] {
        let rev = tasks.revision(x).unwrap();
        tasks
            .transition(x, State::Attempt(to), Requester::Coordinator, rev, 1)
            .unwrap();
    }
    let rev = tasks.revision(x).unwrap();
    let bound = store.persisted_session_id(session).unwrap();
    tasks.bind_attempt_session(id, rev, &bound, 1).unwrap();
}

fn call(
    store: &Store,
    caller: i64,
    request: &str,
    action: &str,
    input: Value,
) -> Result<Value, String> {
    worker_action(store, caller, request, action, &input, NOW)
}

fn attempt_status(store: &Store, id: &str) -> AttemptState {
    store.tasks().unwrap().attempt(id).unwrap().unwrap().status
}

fn assignment_status(store: &Store) -> AssignmentState {
    let tasks = store.tasks().unwrap();
    tasks.assignment("t", "a").unwrap().unwrap().status
}

fn events(store: &Store, kind: &str) -> Vec<crate::tasks::Event> {
    let tasks = store.tasks().unwrap();
    let all = tasks.events_after(0, 10_000).unwrap();
    all.into_iter().filter(|e| e.kind == kind).collect()
}

fn full_result() -> Value {
    json!({
        "outcome": "succeeded",
        "summary": "Added the parser and its tests",
        "workspace": "/project/.worktrees/a (branch task-a)",
        "commits": ["abc1234"],
        "validation": ["cargo test -p core: 12 passed"],
        "evidence": ["target/test-output.txt"],
        "remainingWork": ["Document the flag"],
        "limitations": ["Windows paths untested"]
    })
}

#[test]
fn worker_commands_reject_sessions_without_an_attempt_and_ids_in_the_request() {
    let (store, caller) = fixture();
    let stranger = store.create("codex", "/project").unwrap().id;
    let requests = [
        ("tasks.worker.status", json!({})),
        (
            "tasks.worker.ask",
            json!({"question":"Which parser?","scope":"assignment"}),
        ),
        ("tasks.worker.result", full_result()),
        (
            "tasks.worker.blocked",
            json!({"reason":"Missing credentials"}),
        ),
    ];
    for (action, input) in &requests {
        let error = call(&store, stranger, "r1", action, input.clone()).unwrap_err();
        assert!(
            error.contains("not bound to a task attempt"),
            "{action}: {error}"
        );
    }
    // Identity never comes from the request: ids are not accepted at all.
    for key in ["taskId", "assignmentId", "attemptId", "sessionId"] {
        let error = call(
            &store,
            caller,
            "r2",
            "tasks.worker.status",
            json!({key: "x"}),
        )
        .unwrap_err();
        assert!(error.contains("does not take"), "{key}: {error}");
    }
    assert!(call(&store, caller, "r3", "tasks.worker.finish", json!({})).is_err());
    // Nothing the rejected calls asked for was persisted.
    assert_eq!(attempt_status(&store, "x"), AttemptState::Running);
    assert_eq!(assignment_status(&store), AssignmentState::Running);
    assert!(events(&store, "decision.requested").is_empty());
    assert!(store.tasks().unwrap().claim("x").unwrap().is_none());
}

#[test]
fn status_reports_the_bound_work_and_answered_decisions_without_writing() {
    let (store, caller) = fixture();
    let status = call(&store, caller, "s1", "tasks.worker.status", json!({})).unwrap();
    assert_eq!(status["task"]["id"], "t");
    assert_eq!(status["task"]["stage"], "implementing");
    assert_eq!(status["task"]["lifecycle"], "active");
    assert_eq!(status["assignment"]["id"], "a");
    assert_eq!(status["assignment"]["status"], "running");
    assert_eq!(status["assignment"]["files"], json!(["src"]));
    assert_eq!(status["attempt"]["id"], "x");
    assert_eq!(status["attempt"]["status"], "running");
    assert_eq!(status["answeredDecisions"], json!([]));
    assert!(status["claim"].is_null());

    let asked = call(
        &store,
        caller,
        "q1",
        "tasks.worker.ask",
        json!({"question":"Which parser?","scope":"assignment"}),
    )
    .unwrap();
    let id = asked["decision"]["id"].as_str().unwrap().to_owned();
    let tasks = store.tasks().unwrap();
    let rev = tasks.decision(&id).unwrap().unwrap().revision;
    tasks
        .answer_decision(&id, rev, "Use nom", Requester::Mono, NOW)
        .unwrap();
    let before = tasks.events_after(0, 10_000).unwrap().len();
    let status = call(&store, caller, "s2", "tasks.worker.status", json!({})).unwrap();
    assert_eq!(tasks.events_after(0, 10_000).unwrap().len(), before);
    assert_eq!(status["assignment"]["status"], "waiting_for_decision");
    assert_eq!(status["answeredDecisions"][0]["id"], id.as_str());
    assert_eq!(status["answeredDecisions"][0]["answer"], "Use nom");
    assert_eq!(status["openDecisions"], json!([]));
}

#[test]
fn ask_persists_a_scoped_decision_and_waits_for_it() {
    let (store, caller) = fixture();
    let input = json!({"question":"Change the public API?","context":"It breaks callers","scope":"dependents"});
    let asked = call(&store, caller, "q1", "tasks.worker.ask", input.clone()).unwrap();
    let id = asked["decision"]["id"].as_str().unwrap().to_owned();
    let tasks = store.tasks().unwrap();
    let decision = tasks.decision(&id).unwrap().unwrap();
    assert_eq!(decision.task_id, "t");
    assert_eq!(decision.assignment_id.as_deref(), Some("a"));
    assert_eq!(decision.attempt_id.as_deref(), Some("x"));
    assert_eq!(decision.scope, DecisionScope::Dependents);
    assert_eq!(decision.asked_by, Requester::Worker);
    assert_eq!(decision.state, DecisionState::Open);
    assert_eq!(decision.question, "Change the public API?");
    assert_eq!(decision.context, "It breaks callers");
    assert_eq!(
        assignment_status(&store),
        AssignmentState::WaitingForDecision
    );
    assert_eq!(attempt_status(&store, "x"), AttemptState::Waiting);
    assert_eq!(events(&store, "decision.requested").len(), 1);

    // A retried request is the same question, not a second one.
    let again = call(&store, caller, "q1", "tasks.worker.ask", input).unwrap();
    assert_eq!(again["decision"]["id"], id.as_str());
    assert_eq!(events(&store, "decision.requested").len(), 1);

    // Another question while waiting keeps the assignment waiting.
    let second = call(
        &store,
        caller,
        "q2",
        "tasks.worker.ask",
        json!({"question":"Also rename it?","scope":"task"}),
    )
    .unwrap();
    assert_ne!(second["decision"]["id"], id.as_str());
    assert_eq!(
        assignment_status(&store),
        AssignmentState::WaitingForDecision
    );
    assert_eq!(events(&store, "decision.requested").len(), 2);

    for bad in [
        json!({"question":"?","scope":"everything"}),
        json!({"scope":"task"}),
        json!({"question":"?","scope":"task","blocking":true}),
    ] {
        assert!(call(&store, caller, "q3", "tasks.worker.ask", bad).is_err());
    }
}

#[test]
fn a_result_is_one_claim_in_review_and_never_completes_the_task() {
    let (store, caller) = fixture();
    let first = call(&store, caller, "r1", "tasks.worker.result", full_result()).unwrap();
    assert_eq!(first["duplicate"], false);
    assert_eq!(first["claim"]["outcome"], "succeeded");
    assert_eq!(
        first["claim"]["remainingWork"],
        json!(["Document the flag"])
    );
    let tasks = store.tasks().unwrap();
    let claim = tasks.claim("x").unwrap().unwrap();
    assert_eq!(claim.summary, "Added the parser and its tests");
    assert_eq!(claim.commits, ["abc1234"]);
    assert_eq!(claim.validation, ["cargo test -p core: 12 passed"]);
    assert_eq!(claim.evidence, ["target/test-output.txt"]);
    assert_eq!(claim.limitations, ["Windows paths untested"]);
    assert_eq!(
        claim.workspace.as_deref(),
        Some("/project/.worktrees/a (branch task-a)")
    );
    assert_eq!(attempt_status(&store, "x"), AttemptState::Succeeded);
    assert_eq!(assignment_status(&store), AssignmentState::NeedsReview);
    let task = tasks.task("t").unwrap().unwrap();
    assert_eq!(task.stage, TaskStage::Implementing);
    assert_eq!(task.lifecycle, TaskLifecycle::Active);

    // The same result delivered again, even under a new request id, is the
    // existing claim: no second claim and no second review.
    let again = call(&store, caller, "r2", "tasks.worker.result", full_result()).unwrap();
    assert_eq!(again["duplicate"], true);
    assert_eq!(again["claim"], first["claim"]);
    assert_eq!(events(&store, "attempt.claim").len(), 1);
    assert_eq!(events(&store, "assignment.needs_review").len(), 1);

    // A different result for an already claimed attempt is refused.
    let mut changed = full_result();
    changed["summary"] = json!("Something else");
    let error = call(&store, caller, "r3", "tasks.worker.result", changed).unwrap_err();
    assert!(error.contains("already reported"), "{error}");
    assert_eq!(tasks.claim("x").unwrap().unwrap(), claim);
}

#[test]
fn a_failed_result_is_recorded_without_review() {
    let (store, caller) = fixture();
    let input =
        json!({"outcome":"failed","summary":"Could not reproduce","remainingWork":["Everything"]});
    call(&store, caller, "r1", "tasks.worker.result", input).unwrap();
    let claim = store.tasks().unwrap().claim("x").unwrap().unwrap();
    assert_eq!(claim.outcome, crate::tasks::ClaimOutcome::Failed);
    assert_eq!(attempt_status(&store, "x"), AttemptState::Failed);
    assert_eq!(assignment_status(&store), AssignmentState::Running);
    for bad in [
        json!({"outcome":"done","summary":"x"}),
        json!({"outcome":"succeeded"}),
        json!({"outcome":"succeeded","summary":"x","commits":"abc"}),
        json!({"outcome":"succeeded","summary":"x","evidence":[1]}),
    ] {
        assert!(call(&store, caller, "r2", "tasks.worker.result", bad).is_err());
    }
}

#[test]
fn blocked_records_the_reason_and_waits() {
    let (store, caller) = fixture();
    let input = json!({"reason":"The staging database is unreachable"});
    let blocked = call(&store, caller, "b1", "tasks.worker.blocked", input.clone()).unwrap();
    assert_eq!(blocked["attempt"]["status"], "waiting");
    assert_eq!(attempt_status(&store, "x"), AttemptState::Waiting);
    assert_eq!(assignment_status(&store), AssignmentState::Running);
    let recorded = events(&store, "attempt.blocked");
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].attempt_id.as_deref(), Some("x"));
    assert_eq!(recorded[0].assignment_id.as_deref(), Some("a"));
    assert_eq!(
        recorded[0].payload["reason"],
        "The staging database is unreachable"
    );
    call(&store, caller, "b1", "tasks.worker.blocked", input).unwrap();
    assert_eq!(events(&store, "attempt.blocked").len(), 1);
    assert!(call(&store, caller, "b2", "tasks.worker.blocked", json!({})).is_err());
    // A blocked worker can still claim its result afterwards.
    call(&store, caller, "r1", "tasks.worker.result", full_result()).unwrap();
    assert_eq!(assignment_status(&store), AssignmentState::NeedsReview);
}

#[test]
fn a_replaced_attempt_cannot_ask_or_block_for_its_assignment() {
    let (store, caller) = fixture();
    let replacement = store.create("codex", "/project").unwrap().id;
    launch(&store, "y", replacement);
    let ask = json!({"question":"Which parser?","scope":"assignment"});
    let error = call(&store, caller, "q1", "tasks.worker.ask", ask.clone()).unwrap_err();
    assert!(error.contains("replaced"), "{error}");
    assert!(call(
        &store,
        caller,
        "b1",
        "tasks.worker.blocked",
        json!({"reason":"x"})
    )
    .is_err());
    assert_eq!(assignment_status(&store), AssignmentState::Running);
    call(&store, replacement, "q1", "tasks.worker.ask", ask).unwrap();
    assert_eq!(
        assignment_status(&store),
        AssignmentState::WaitingForDecision
    );
}
