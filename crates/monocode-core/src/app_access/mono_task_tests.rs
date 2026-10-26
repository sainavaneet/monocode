//! Mono's task commands: authority comes from the Mono that owns the task,
//! which the bridge derives from the calling session, never from the input.
use super::*;
use crate::orchestration::{Plan, Task as Assignment};
use crate::tasks::{
    AssignmentState, AttemptState, DecisionState, NewAttempt, NewTask, Requester, State, Subject,
    TaskStage,
};
use std::path::Path;

const NOW: u64 = 10;

/// Task `t` owned by `mono-1`, assignment `a` Running, attempt `x` bound to a
/// worker session that asked one question. Returns the decision id.
fn fixture() -> (Store, String) {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .save_setting(crate::tasks::coordinator::FLAG, "enabled")
        .unwrap();
    let worker = store.create("codex", "/project").unwrap().id;
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
            ..Assignment::default()
        }],
    };
    tasks.save_plan("t", 1, &plan, 1).unwrap();
    let t = Subject::Task("t");
    for stage in [TaskStage::Ready, TaskStage::Implementing] {
        let rev = tasks.revision(t).unwrap();
        tasks
            .transition(t, State::Stage(stage), Requester::Mono, rev, 1)
            .unwrap();
    }
    let a = Subject::Assignment { task: "t", id: "a" };
    for to in [AssignmentState::Ready, AssignmentState::Running] {
        let rev = tasks.revision(a).unwrap();
        tasks
            .transition(a, State::Assignment(to), Requester::Coordinator, rev, 1)
            .unwrap();
    }
    tasks
        .create_attempt(
            &NewAttempt {
                id: "x".into(),
                task_id: "t".into(),
                assignment_id: "a".into(),
                operation_id: "launch-x".into(),
                provider: "codex".into(),
                account_id: None,
            },
            1,
        )
        .unwrap();
    let x = Subject::Attempt("x");
    for to in [AttemptState::Starting, AttemptState::Running] {
        let rev = tasks.revision(x).unwrap();
        tasks
            .transition(x, State::Attempt(to), Requester::Coordinator, rev, 1)
            .unwrap();
    }
    let rev = tasks.revision(x).unwrap();
    let bound = store.persisted_session_id(worker).unwrap();
    tasks.bind_attempt_session("x", rev, &bound, 1).unwrap();
    let asked = worker_action(
        &store,
        worker,
        "q1",
        "tasks.worker.ask",
        &json!({"question": "Which parser?"}),
        NOW,
    )
    .unwrap();
    let id = asked["decision"]["id"].as_str().unwrap().to_owned();
    (store, id)
}

#[test]
fn mono_task_actions_belong_to_the_owning_mono_only() {
    let (store, id) = fixture();
    let revision = store
        .tasks()
        .unwrap()
        .decision(&id)
        .unwrap()
        .unwrap()
        .revision;
    let call = |mono: &str, action: &str, input: Value| {
        mono_task_action(&store, mono, action, &input, NOW + 1)
    };
    // Another Mono sees nothing and can change nothing.
    let listed = call("mono-2", "tasks.decisions.list", json!({})).unwrap();
    assert_eq!(listed["decisions"], json!([]));
    let answer = json!({"decisionId": id, "revision": revision, "answer": "nom"});
    for (action, input) in [
        ("tasks.decisions.answer", answer.clone()),
        (
            "tasks.decisions.escalate",
            json!({"decisionId": id, "revision": revision, "why": "w", "recommendation": "r"}),
        ),
        (
            "tasks.review.accept",
            json!({"taskId": "t", "assignmentId": "a", "revision": 1}),
        ),
        (
            "tasks.review.sendBack",
            json!({"taskId": "t", "assignmentId": "a", "revision": 1, "reason": "r"}),
        ),
    ] {
        let refused = call("mono-2", action, input).unwrap_err();
        assert!(refused.contains("not found"), "{action}: {refused}");
    }
    assert_eq!(
        store.tasks().unwrap().decision(&id).unwrap().unwrap().state,
        DecisionState::Open
    );
    // Fields are validated before anything changes.
    assert!(call(
        "mono-1",
        "tasks.decisions.answer",
        json!({"decisionId": id})
    )
    .is_err());
    assert!(call(
        "mono-1",
        "tasks.decisions.answer",
        json!({"decisionId": id, "revision": "1", "answer": "nom"})
    )
    .is_err());
    assert!(call(
        "mono-1",
        "tasks.decisions.list",
        json!({"monoId": "mono-1"})
    )
    .is_err());

    // The owner lists the request with its context and answers it.
    let listed = call("mono-1", "tasks.decisions.list", json!({})).unwrap();
    let entry = &listed["decisions"][0];
    assert_eq!(entry["id"], id.as_str());
    assert_eq!(entry["revision"], revision);
    assert_eq!(entry["task"]["title"], "Ship it");
    assert_eq!(entry["assignment"]["title"], "Assignment a");
    assert_eq!(entry["escalated"], false);
    let answered = call("mono-1", "tasks.decisions.answer", answer).unwrap();
    assert_eq!(answered["state"], "answered");

    // With the coordinator flag off, Mono's task actions do nothing.
    store
        .save_setting(crate::tasks::coordinator::FLAG, "off")
        .unwrap();
    let off = call("mono-1", "tasks.decisions.list", json!({})).unwrap_err();
    assert!(off.contains("off"), "{off}");
}
