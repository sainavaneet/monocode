//! Mono's policy commands: show and apply the autonomy policy of its own
//! tasks, and unblock them, through the transition table.
use super::*;
use crate::tasks::autonomy::{self, Autonomy, MonoAutonomy};
use crate::tasks::{policy, Subject, TaskLifecycle};
use std::collections::HashMap;
use std::path::Path;

const NOW: u64 = 10;
/// A Mono id no other test publishes a policy for.
const MONO: &str = "mono-policy-commands";

fn fixture() -> Store {
    let store = Store::open(Path::new(":memory:")).unwrap();
    store
        .save_setting(crate::tasks::coordinator::FLAG, "enabled")
        .unwrap();
    crate::tasks::fixtures::running_worker(&store, MONO, "/project").unwrap();
    let task = store.tasks().unwrap().task("t").unwrap().unwrap();
    policy::capture_with(&store, &task, "codex", &Autonomy::default(), NOW).unwrap();
    store
}

fn block(store: &Store) {
    store
        .connection()
        .execute(
            "UPDATE native_tasks SET lifecycle='blocked', blocked_reason='No eligible account' WHERE id='t'",
            [],
        )
        .unwrap();
}

#[test]
fn mono_policy_commands_validate_their_fields() {
    let validate = crate::mono::api::validate;
    assert!(validate("tasks.policy.show", &json!({"taskId": "t"})).is_ok());
    assert!(validate("tasks.policy.show", &json!({"taskId": "t", "revision": 1})).is_err());
    assert!(validate("tasks.policy.apply", &json!({"taskId": "t", "revision": 0})).is_ok());
    assert!(validate(
        "tasks.policy.apply",
        &json!({"taskId": "t", "revision": "0"})
    )
    .is_err());
    assert!(validate("tasks.unblock", &json!({"taskId": "t", "revision": 3})).is_ok());
    assert!(validate("tasks.unblock", &json!({"taskId": "t", "reason": "x"})).is_err());
}

#[test]
fn mono_policy_commands_show_apply_and_unblock_only_the_owning_monos_tasks() {
    let _published = autonomy::tests_lock();
    let store = fixture();
    let generous = Autonomy {
        worker_limit: 3,
        push: true,
        ..Autonomy::default()
    };
    let mut live = HashMap::new();
    live.insert(
        MONO.to_owned(),
        MonoAutonomy {
            policy: generous.clone(),
            ..MonoAutonomy::default()
        },
    );
    autonomy::publish(live);
    let call = |mono: &str, action: &str, input: Value| {
        mono_policy_action(&store, mono, action, &input, NOW + 1)
    };
    // Another Mono sees nothing and changes nothing.
    for (action, input) in [
        ("tasks.policy.show", json!({"taskId": "t"})),
        ("tasks.policy.apply", json!({"taskId": "t", "revision": 0})),
        ("tasks.unblock", json!({"taskId": "t", "revision": 1})),
    ] {
        let refused = call("mono-other", action, input).unwrap_err();
        assert!(refused.contains("not found"), "{action}: {refused}");
    }
    let shown = call(MONO, "tasks.policy.show", json!({"taskId": "t"})).unwrap();
    assert_eq!(shown["revision"], 0);
    assert_eq!(shown["snapshot"]["workerLimit"], 1);
    assert_eq!(shown["live"]["workerLimit"], 3);
    assert_eq!(shown["effective"]["push"], false);
    assert_eq!(shown["differs"], true);
    // Apply takes the live policy into the task with a revision bump.
    let stale = call(
        MONO,
        "tasks.policy.apply",
        json!({"taskId": "t", "revision": 4}),
    );
    assert!(stale.unwrap_err().contains("changed"));
    let applied = call(
        MONO,
        "tasks.policy.apply",
        json!({"taskId": "t", "revision": 0}),
    )
    .unwrap();
    assert_eq!(applied["revision"], 1);
    let shown = call(MONO, "tasks.policy.show", json!({"taskId": "t"})).unwrap();
    assert_eq!(shown["effective"]["push"], true);
    assert_eq!(shown["differs"], false);
    // Unblock is for blocked tasks, through the transition table.
    let active = call(MONO, "tasks.unblock", json!({"taskId": "t", "revision": 3}));
    assert!(active.unwrap_err().contains("not blocked"));
    block(&store);
    let tasks = store.tasks().unwrap();
    let revision = tasks.revision(Subject::Task("t")).unwrap();
    let stale = call(
        MONO,
        "tasks.unblock",
        json!({"taskId": "t", "revision": revision + 1}),
    );
    assert!(stale.unwrap_err().contains("changed"));
    let unblocked = call(
        MONO,
        "tasks.unblock",
        json!({"taskId": "t", "revision": revision}),
    )
    .unwrap();
    assert_eq!(unblocked["lifecycle"], "active");
    let task = tasks.task("t").unwrap().unwrap();
    assert_eq!(task.lifecycle, TaskLifecycle::Active);
    assert_eq!(task.blocked_reason, None);
    // The user unblocks through the store, and any path out of Blocked
    // (here a plain transition to Paused) clears the reason with it.
    block(&store);
    let revision = tasks.revision(Subject::Task("t")).unwrap();
    tasks
        .unblock("t", revision, crate::tasks::Requester::User, NOW + 2)
        .unwrap();
    let task = tasks.task("t").unwrap().unwrap();
    assert_eq!(task.lifecycle, TaskLifecycle::Active);
    assert_eq!(task.blocked_reason, None);
    block(&store);
    let revision = tasks.revision(Subject::Task("t")).unwrap();
    let paused = crate::tasks::State::Lifecycle(TaskLifecycle::Paused);
    tasks
        .transition(
            Subject::Task("t"),
            paused,
            crate::tasks::Requester::User,
            revision,
            NOW + 3,
        )
        .unwrap();
    assert_eq!(tasks.task("t").unwrap().unwrap().blocked_reason, None);
    autonomy::publish(HashMap::new());
    // With the coordinator off, nothing is available.
    store
        .save_setting(crate::tasks::coordinator::FLAG, "off")
        .unwrap();
    assert!(call(MONO, "tasks.policy.show", json!({"taskId": "t"})).is_err());
}
