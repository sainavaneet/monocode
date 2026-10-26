//! Steering through the coordinator against temporary Git repositories:
//! only the affected workers stop; the rest continue.
use super::*;
use crate::app_access::worker_action;
use crate::tasks::coordinator::{Coordinator, Policy};
use crate::tasks::decisions::tests::{independent, task_with};
use crate::tasks::recovery::tests::{kinds, stop_all};
use crate::tasks::review::tests::{open, repo, FakeLauncher, NOW};
use crate::tasks::{confirm_started, Attempt, AttemptState};
use serde_json::json;

struct Two;
impl Policy for Two {
    fn worker_limit(&self, _task: &Task) -> usize {
        2
    }
}

fn coordinator() -> Coordinator<Two> {
    Coordinator::new("gpui-1", Two)
}

fn status(store: &Store, id: &str) -> AssignmentState {
    let tasks = store.tasks().unwrap();
    tasks.assignment("t", id).unwrap().unwrap().status
}

fn latest(store: &Store, id: &str) -> Attempt {
    store
        .tasks()
        .unwrap()
        .attempts("t", id)
        .unwrap()
        .pop()
        .unwrap()
}

fn revision(store: &Store) -> u64 {
    store.tasks().unwrap().task("t").unwrap().unwrap().revision
}

/// Task `t` with independent assignments `a` and `b`, both running.
fn two_running(store: &Store, project: &str, launcher: &FakeLauncher) -> Vec<i64> {
    task_with(store, project, vec![independent("a"), independent("b")]);
    coordinator().tick(store, launcher, NOW).unwrap();
    let started: Vec<i64> = launcher.started.borrow().iter().map(|s| s.id).collect();
    assert_eq!(started.len(), 2);
    for session in &started {
        assert!(confirm_started(store, *session, NOW + 1).unwrap());
    }
    started
}

#[test]
fn steering_stops_only_affected_workers() {
    let repo = repo();
    let store = open(&repo);
    let project = repo.root.to_string_lossy().into_owned();
    let launcher = FakeLauncher::default();
    let started = two_running(&store, &project, &launcher);
    let first_a = latest(&store, "a");
    let b = latest(&store, "b");

    let request = Steer {
        requirements: "Support Windows paths too".into(),
        assignments: vec!["a".into()],
        plan: None,
    };
    let steered = steer(
        &store,
        "t",
        revision(&store),
        Requester::User,
        &request,
        NOW + 10,
    )
    .unwrap();
    assert_eq!(steered.affected, vec!["a".to_owned()]);
    assert_eq!(steered.stopping, vec![first_a.id.clone()]);
    assert!(!steered.assessing);
    assert_eq!(latest(&store, "a").status, AttemptState::Cancelled);
    assert_eq!(status(&store, "a"), AssignmentState::Queued);
    // b keeps running and may still finish.
    assert_eq!(latest(&store, "b").status, AttemptState::Running);
    assert_eq!(status(&store, "b"), AssignmentState::Running);
    let stops = crate::tasks::recovery::pending_stops(&store).unwrap();
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].session_id, first_a.session_id.clone().unwrap());
    // The stopped worker's late result is history only.
    let late = worker_action(
        &store,
        started[0],
        "late",
        "tasks.worker.result",
        &json!({"outcome": "succeeded", "summary": "Old requirements"}),
        NOW + 11,
    );
    assert!(late.is_err());
    worker_action(
        &store,
        started[1],
        "done",
        "tasks.worker.result",
        &json!({"outcome": "succeeded", "summary": "Done"}),
        NOW + 12,
    )
    .unwrap();
    assert_eq!(status(&store, "b"), AssignmentState::NeedsReview);
    assert_eq!(b.status, AttemptState::Running);

    // Once stopped, a starts again under the new requirements.
    stop_all(&store, NOW + 13);
    coordinator().tick(&store, &launcher, NOW + 20).unwrap();
    assert_eq!(launcher.started.borrow().len(), 3);
    let second_a = latest(&store, "a");
    assert_ne!(second_a.id, first_a.id);
    let session = launcher.started.borrow()[2].id;
    let brief = store.follow_ups(session).unwrap().draft.unwrap().text;
    assert!(brief.contains("Support Windows paths too"), "{brief}");
    let events = kinds(&store, "task.steered");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload["planRevision"], 2);
}

#[test]
fn a_revised_plan_requeues_changed_work_drops_removed_work_and_keeps_every_revision() {
    let repo = repo();
    let store = open(&repo);
    let project = repo.root.to_string_lossy().into_owned();
    let launcher = FakeLauncher::default();
    two_running(&store, &project, &launcher);
    let mut a = independent("a");
    a.prompt = "Do the work for Windows too".into();
    let c = independent("c");
    let revised = crate::orchestration::Plan {
        title: "Plan".into(),
        summary: "Revised".into(),
        tasks: vec![a, c],
    };
    let request = Steer {
        requirements: "Windows, and drop b".into(),
        assignments: vec![],
        plan: Some(revised),
    };
    let steered = steer(
        &store,
        "t",
        revision(&store),
        Requester::Mono,
        &request,
        NOW + 10,
    )
    .unwrap();
    assert_eq!(steered.affected, vec!["a".to_owned()]);
    assert_eq!(steered.removed, vec!["b".to_owned()]);
    assert_eq!(steered.added, vec!["c".to_owned()]);
    assert_eq!(status(&store, "a"), AssignmentState::Queued);
    assert_eq!(status(&store, "b"), AssignmentState::Cancelled);
    assert_eq!(latest(&store, "b").status, AttemptState::Cancelled);
    assert_eq!(status(&store, "c"), AssignmentState::Queued);
    let tasks = store.tasks().unwrap();
    assert_eq!(
        tasks.assignment("t", "a").unwrap().unwrap().prompt,
        "Do the work for Windows too"
    );
    // A second steer is a new revision; the first stays recorded.
    let again = Steer {
        requirements: "Also macOS".into(),
        assignments: vec!["c".into()],
        plan: None,
    };
    let second = steer(
        &store,
        "t",
        revision(&store),
        Requester::User,
        &again,
        NOW + 20,
    )
    .unwrap();
    assert_eq!(second.plan_revision, steered.plan_revision + 1);
    let events = kinds(&store, "task.steered");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].payload["requirements"], "Windows, and drop b");
    // A stale revision is refused.
    assert!(matches!(
        steer(
            &store,
            "t",
            steered.revision,
            Requester::User,
            &again,
            NOW + 30
        ),
        Err(TaskError::Stale { .. })
    ));
}

#[test]
fn steering_without_known_affected_work_asks_mono_and_stops_nothing() {
    let repo = repo();
    let store = open(&repo);
    let project = repo.root.to_string_lossy().into_owned();
    let launcher = FakeLauncher::default();
    two_running(&store, &project, &launcher);
    let request = Steer {
        requirements: "Make it faster".into(),
        ..Steer::default()
    };
    // Mono must say what it affects.
    assert!(steer(
        &store,
        "t",
        revision(&store),
        Requester::Mono,
        &request,
        NOW + 5
    )
    .is_err());
    let steered = steer(
        &store,
        "t",
        revision(&store),
        Requester::User,
        &request,
        NOW + 10,
    )
    .unwrap();
    assert!(steered.assessing);
    assert!(steered.stopping.is_empty());
    assert_eq!(latest(&store, "a").status, AttemptState::Running);
    assert_eq!(latest(&store, "b").status, AttemptState::Running);
    assert!(crate::tasks::recovery::pending_stops(&store)
        .unwrap()
        .is_empty());
    let batch = crate::tasks::inbox::next_batch(&store, "mono-1", NOW + 11)
        .unwrap()
        .unwrap();
    assert!(
        batch.message.contains("The requirements changed"),
        "{}",
        batch.message
    );
    assert!(
        batch.message.contains("Make it faster"),
        "{}",
        batch.message
    );
}
