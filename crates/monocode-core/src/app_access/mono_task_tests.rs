//! Mono's task commands: authority comes from the Mono that owns the task,
//! which the bridge derives from the calling session, never from the input.
use super::*;
use crate::tasks::DecisionState;
use std::path::Path;

const NOW: u64 = 10;

/// The shared running-worker fixture, owned by `mono-1`, whose worker asked
/// one question. Returns the decision id.
fn fixture() -> (Store, String) {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .save_setting(crate::tasks::coordinator::FLAG, "enabled")
        .unwrap();
    let worker = crate::tasks::fixtures::running_worker(&store, "mono-1", "/project").unwrap();
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

#[test]
fn mono_pauses_resumes_steers_and_cancels_only_its_own_tasks() {
    let (store, _) = fixture();
    let revision = || store.tasks().unwrap().task("t").unwrap().unwrap().revision;
    let call = |mono: &str, action: &str, input: Value| {
        mono_task_action(&store, mono, action, &input, NOW + 1)
    };
    let pause = json!({"taskId": "t", "revision": revision()});
    let refused = call("mono-2", "tasks.pause", pause.clone()).unwrap_err();
    assert!(refused.contains("not found"), "{refused}");
    let paused = call("mono-1", "tasks.pause", pause.clone()).unwrap();
    assert_eq!(paused["display"], "pausing");
    // A stale revision changes nothing.
    assert!(call("mono-1", "tasks.resume", pause).is_err());
    let resumed = call(
        "mono-1",
        "tasks.resume",
        json!({"taskId": "t", "revision": revision()}),
    )
    .unwrap();
    assert_eq!(resumed["display"], "active");
    let steer = json!({"taskId": "t", "revision": revision(), "requirements": "Also log errors",
        "assignmentIds": ["a"]});
    assert!(call("mono-2", "tasks.steer", steer.clone()).is_err());
    let steered = call("mono-1", "tasks.steer", steer).unwrap();
    assert_eq!(steered["affected"], json!(["a"]));
    let cancel = json!({"taskId": "t", "revision": revision()});
    assert!(call("mono-2", "tasks.cancel", cancel.clone()).is_err());
    let cancelled = call("mono-1", "tasks.cancel", cancel).unwrap();
    assert!(matches!(
        cancelled["display"].as_str(),
        Some("cancelled" | "cancelling")
    ));
    // Unknown fields are refused before anything changes.
    assert!(call(
        "mono-1",
        "tasks.pause",
        json!({"taskId": "t", "revision": 1, "x": 1})
    )
    .is_err());
}
