//! Replacement workers, their durable hand-off, late starts, sessions that
//! end without a result, and the billing snapshot.
use super::*;
use crate::store::Store;
use crate::tasks::coordinator::LAUNCH_CONFIRM_TIMEOUT_MS;
use crate::tasks::recovery::launch_allowed;
use crate::tasks::recovery::tests::{
    attempts, coordinator, fail, kinds, launched, stop_all, Recorded,
};
use crate::tasks::recovery::{pending_stops, turn_ended, Outcome, TurnEnd};
use crate::tasks::review::tests::{git, open, repo, NOW};
use crate::tasks::{
    confirm_started, policy, recovery, AssignmentState, AttemptState, NewDecision, Requester,
};
use serde_json::{json, Value};

fn brief(store: &Store, session: i64) -> String {
    store.follow_ups(session).unwrap().draft.unwrap().text
}

/// Keys of every object in `value`, recursively.
fn keys(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                out.push(key.clone());
                keys(inner, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| keys(v, out)),
        _ => {}
    }
}

#[test]
fn a_replacement_keeps_the_task_and_workspace_and_carries_a_durable_handoff() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    let (_, settings) = launcher.last();
    let tree = std::path::PathBuf::from(settings.worktree_cwd.clone().unwrap());
    // The worker commits some work and leaves an edit, and Mono answered a
    // question for it.
    std::fs::write(tree.join("src/feature.rs"), "pub fn feature() {}\n").unwrap();
    git(&tree, &["add", "."]);
    git(&tree, &["commit", "-q", "-m", "Add the feature skeleton"]);
    std::fs::write(tree.join("src/lib.rs"), "pub fn one() { /* wip */ }\n").unwrap();
    let tasks = store.tasks().unwrap();
    let first = attempts(&store)[0].clone();
    tasks
        .create_decision(
            &NewDecision {
                id: "d1".into(),
                task_id: "t".into(),
                assignment_id: Some("a".into()),
                attempt_id: Some(first.id.clone()),
                question: "Which error type?".into(),
                context: String::new(),
                scope: crate::tasks::DecisionScope::Assignment,
                asked_by: Requester::Worker,
            },
            NOW,
        )
        .unwrap();
    tasks
        .answer_decision("d1", 1, "Use thiserror", Requester::Mono, NOW)
        .unwrap();
    let outcome = fail(
        &store,
        session,
        "503 upstream error; key sk-live-abcdefghijklmnop leaked",
        NOW + 1,
    );
    let Outcome::Retry { at, .. } = outcome else {
        panic!("{outcome:?}");
    };
    stop_all(&store, at);
    coordinator().tick(&store, &launcher, at).unwrap();

    // Same task, same assignment, same workspace and worktree.
    let all = attempts(&store);
    assert_eq!(all.len(), 2);
    let second = &all[1];
    assert_eq!(
        (second.task_id.as_str(), second.assignment_id.as_str()),
        ("t", "a")
    );
    assert_eq!(tasks.workspaces("t").unwrap().len(), 1);
    let (session, settings) = launcher.last();
    assert_eq!(
        settings.worktree_cwd.as_deref(),
        Some(tree.to_str().unwrap())
    );

    // The durable hand-off record.
    let record = of(&tasks, &second.id).unwrap().unwrap();
    assert_eq!(record.from_attempt, first.id);
    assert_eq!(record.objective, "Do the work");
    assert_eq!(record.workspace.path, tree.to_string_lossy());
    assert!(record.completed_work.commits[0].contains("Add the feature skeleton"));
    assert!(record
        .completed_work
        .uncommitted
        .iter()
        .any(|f| f.contains("src/lib.rs")));
    assert_eq!(record.decisions[0].answer.as_deref(), Some("Use thiserror"));
    assert!(record.next_step.contains("tasks.worker.result"));
    assert_eq!(record.failure.category, "temporary");
    assert!(
        !record.failure.reason.contains("sk-live"),
        "{:?}",
        record.failure
    );
    let json = serde_json::to_value(&record).unwrap();
    let mut names = Vec::new();
    keys(&json, &mut names);
    for name in &names {
        let lower = name.to_lowercase();
        for word in [
            "token",
            "secret",
            "password",
            "credential",
            "apikey",
            "auth",
        ] {
            assert!(!lower.contains(word), "credential-like field {name}");
        }
    }
    for field in [
        "objective",
        "revision",
        "decisions",
        "workspace",
        "completedWork",
        "validation",
        "pendingQuestions",
        "nextStep",
    ] {
        assert!(json.get(field).is_some(), "missing {field}: {json}");
    }

    // The replacement's brief carries it.
    let text = brief(&store, session.id);
    for needle in [
        "Hand-off from attempt",
        "Add the feature skeleton",
        "src/lib.rs",
        "Use thiserror",
        "Next step",
    ] {
        assert!(text.contains(needle), "brief lacks {needle}: {text}");
    }
    assert!(!text.contains("sk-live"));
}

#[test]
fn a_late_start_after_the_timeout_is_stopped_as_an_orphan() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    // Launch without confirming: the app crashed before installing it.
    crate::tasks::recovery::tests::launched_unconfirmed(&store, &repo, &launcher);
    let orphan = launcher.last().0.id;
    let expired = NOW + LAUNCH_CONFIRM_TIMEOUT_MS;
    coordinator().tick(&store, &launcher, expired).unwrap();
    assert_eq!(attempts(&store)[0].status, AttemptState::Interrupted);
    assert_eq!(kinds(&store, "launch.unconfirmed").len(), 1);
    // Recovery took the same path as any other failure.
    let tasks = store.tasks().unwrap();
    let row = recovery::recovery(&tasks, &attempts(&store)[0].id)
        .unwrap()
        .unwrap();
    assert_eq!(row.failure, "temporary");
    // The never-started session is stopped before a replacement launches.
    assert_eq!(stop_all(&store, expired), 1);
    let due = row.due_at;
    coordinator().tick(&store, &launcher, due).unwrap();
    assert_eq!(launcher.count(), 2);
    // The app finally confirms the old session: it is an orphan, recorded
    // once and stopped through the stop path.
    assert!(!confirm_started(&store, orphan, due + 1).unwrap());
    assert!(!confirm_started(&store, orphan, due + 2).unwrap());
    assert_eq!(kinds(&store, "launch.orphaned").len(), 1);
    assert!(!launch_allowed(&store, orphan).unwrap());
    let stops = pending_stops(&store).unwrap();
    assert_eq!(stops.len(), 1);
    assert_eq!(
        stops[0].session_id,
        store.persisted_session_id(orphan).unwrap()
    );
    // The replacement is unaffected.
    let current = launcher.last().0.id;
    assert!(launch_allowed(&store, current).unwrap());
    assert!(confirm_started(&store, current, due + 3).unwrap());
}

#[test]
fn a_session_that_ends_without_a_result_is_reconciled_by_mono() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    let quiet = TurnEnd::default();
    let outcome = turn_ended(&store, session, &quiet, NOW + 1).unwrap();
    assert_eq!(outcome, Outcome::HandedToMono);
    let tasks = store.tasks().unwrap();
    assert_eq!(attempts(&store)[0].status, AttemptState::Interrupted);
    assert_eq!(
        tasks.assignment("t", "a").unwrap().unwrap().status,
        AssignmentState::Failed,
        "never assumed to have succeeded"
    );
    let batch = crate::tasks::inbox::next_batch(&store, "mono-1", NOW + 2)
        .unwrap()
        .unwrap();
    assert!(
        batch.message.contains("ended without reporting a result"),
        "{}",
        batch.message
    );
    assert!(batch.message.contains("tasks.review.sendBack"));
    assert_eq!(
        turn_ended(&store, session, &quiet, NOW + 3).unwrap(),
        Outcome::Ignored
    );

    // A worker waiting on a question, or one that claimed, ended normally.
    let repo = self::repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    crate::app_access::worker_action(
        &store,
        session,
        "q1",
        "tasks.worker.ask",
        &json!({"question": "Which?"}),
        NOW + 1,
    )
    .unwrap();
    assert_eq!(
        turn_ended(&store, session, &quiet, NOW + 2).unwrap(),
        Outcome::Ignored
    );
    let repo = self::repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    crate::app_access::worker_action(
        &store,
        session,
        "r1",
        "tasks.worker.result",
        &json!({"outcome": "succeeded", "summary": "Done"}),
        NOW + 1,
    )
    .unwrap();
    let failed = TurnEnd {
        error: Some("503".into()),
        ..TurnEnd::default()
    };
    assert_eq!(
        turn_ended(&store, session, &failed, NOW + 2).unwrap(),
        Outcome::Ignored
    );
    let tasks = store.tasks().unwrap();
    assert_eq!(
        tasks.assignment("t", "a").unwrap().unwrap().status,
        AssignmentState::NeedsReview,
        "the claim stands"
    );
    // Sessions that are not workers are ignored.
    let other = store.create("codex", "/elsewhere").unwrap().id;
    assert_eq!(
        turn_ended(&store, other, &failed, NOW + 3).unwrap(),
        Outcome::Ignored
    );
}

#[test]
fn the_api_key_choice_is_snapshotted_when_the_task_is_created() {
    let repo = repo();
    let store = open(&repo);
    store
        .save_ui_state(r#"{"preferences":{"api_key_enabled":{"codex":true}}}"#)
        .unwrap();
    assert!(policy::live_api_key(&store, crate::provider::Kind::Codex));
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    // The user turns API key billing off while the task is in flight.
    store
        .save_ui_state(r#"{"preferences":{"api_key_enabled":{"codex":false}}}"#)
        .unwrap();
    assert!(!policy::live_api_key(&store, crate::provider::Kind::Codex));
    let (_, settings) = launcher.last();
    assert!(settings.use_api_key);
    assert_eq!(settings.api_key_pin, Some(true));
    let Outcome::Retry { at, .. } = fail(&store, session, "503", NOW + 1) else {
        panic!("expected a retry");
    };
    stop_all(&store, at);
    coordinator().tick(&store, &launcher, at).unwrap();
    let (_, settings) = launcher.last();
    assert_eq!(
        settings.api_key_pin,
        Some(true),
        "recovery uses the snapshot"
    );
    let tasks = store.tasks().unwrap();
    let snapshot = policy::of(&tasks, "t").unwrap().unwrap();
    assert!(snapshot.use_api_key);
    assert_eq!(snapshot.start_account, "default");
    assert_eq!(
        snapshot.accounts,
        crate::accounts::eligibility::Allowlist::only("codex", "default")
    );
}

#[test]
fn workers_launch_on_the_account_selected_for_the_project() {
    let repo = repo();
    let store = open(&repo);
    crate::tasks::recovery::tests::accounts(&store, &["account-b"]);
    let mut book = store.provider_accounts().unwrap();
    let project = repo.root.to_string_lossy().into_owned();
    book.select(crate::provider::Kind::Codex, &project, "account-b")
        .unwrap();
    store.save_provider_accounts(&book).unwrap();
    let launcher = Recorded::default();
    launched(&store, &repo, &launcher);
    assert_eq!(launcher.last().1.account.id, "account-b");
    assert_eq!(attempts(&store)[0].account_id.as_deref(), Some("account-b"));
}
