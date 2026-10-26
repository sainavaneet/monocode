//! Steering through the coordinator against temporary Git repositories:
//! only the affected workers stop; the rest continue.
use super::*;
use crate::app_access::worker_action;
use crate::tasks::coordinator::{Coordinator, Policy};
use crate::tasks::decisions::tests::{independent, task_with};
use crate::tasks::lifecycle::{pause, resume, Resume};
use crate::tasks::recovery::tests::{kinds, stop_all};
use crate::tasks::recovery::{turn_ended, TurnEnd};
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

fn plan_revision(store: &Store) -> u64 {
    store
        .tasks()
        .unwrap()
        .task("t")
        .unwrap()
        .unwrap()
        .plan_revision
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
        plan_revision(&store),
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

    // Nothing starts in a's workspace while its old worker may still run.
    coordinator().tick(&store, &launcher, NOW + 13).unwrap();
    assert_eq!(launcher.started.borrow().len(), 2);
    // Once stopped, a starts again under the new requirements.
    stop_all(&store, NOW + 14);
    coordinator().tick(&store, &launcher, NOW + 20).unwrap();
    assert_eq!(launcher.started.borrow().len(), 3);
    let second_a = latest(&store, "a");
    assert_ne!(second_a.id, first_a.id);
    let session = launcher.started.borrow()[2].id;
    let brief = store.follow_ups(session).unwrap().draft.unwrap().text;
    assert!(brief.contains("Support Windows paths too"), "{brief}");
    // The production launch (dispatch -> reserve_with) carries the whole
    // steering note, not only the requirement text.
    let note = brief_note(&store.tasks().unwrap(), "t").unwrap().unwrap();
    assert!(brief.contains(&note), "{brief}");
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
        plan_revision(&store),
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
        plan_revision(&store),
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
            steered.plan_revision,
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
        plan_revision(&store),
        Requester::Mono,
        &request,
        NOW + 5
    )
    .is_err());
    let steered = steer(
        &store,
        "t",
        plan_revision(&store),
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

#[test]
fn a_pending_stop_still_occupies_its_file_scope() {
    let repo = repo();
    let store = open(&repo);
    let project = repo.root.to_string_lossy().into_owned();
    let launcher = FakeLauncher::default();
    let mut c = independent("c");
    c.files = vec!["src/a/c".into()];
    task_with(
        &store,
        &project,
        vec![independent("a"), independent("b"), c],
    );
    coordinator().tick(&store, &launcher, NOW).unwrap();
    assert_eq!(launcher.started.borrow().len(), 2, "c waits for a's scope");
    for session in launcher.started.borrow().iter() {
        confirm_started(&store, session.id, NOW + 1).unwrap();
    }
    let request = Steer {
        requirements: "Redo a".into(),
        assignments: vec!["a".into()],
        plan: None,
    };
    steer(
        &store,
        "t",
        plan_revision(&store),
        Requester::User,
        &request,
        NOW + 2,
    )
    .unwrap();
    // a's old worker may still run in src/a: c (inside it) does not start.
    for later in 3..6 {
        coordinator().tick(&store, &launcher, NOW + later).unwrap();
    }
    assert_eq!(launcher.started.borrow().len(), 2);
    assert_eq!(status(&store, "c"), AssignmentState::Ready);
    stop_all(&store, NOW + 6);
    coordinator().tick(&store, &launcher, NOW + 7).unwrap();
    assert_eq!(launcher.started.borrow().len(), 3);
}

#[test]
fn pause_steer_and_resume_launch_nothing_until_the_stops_are_confirmed() {
    let repo = repo();
    let store = open(&repo);
    let project = repo.root.to_string_lossy().into_owned();
    let launcher = FakeLauncher::default();
    two_running(&store, &project, &launcher);
    pause(&store, "t", revision(&store), Requester::User, NOW + 2).unwrap();
    let request = Steer {
        requirements: "Redo a".into(),
        assignments: vec!["a".into()],
        plan: None,
    };
    steer(
        &store,
        "t",
        plan_revision(&store),
        Requester::User,
        &request,
        NOW + 3,
    )
    .unwrap();
    resume(
        &store,
        "t",
        revision(&store),
        Requester::User,
        &Resume::default(),
        NOW + 4,
    )
    .unwrap();
    for later in 5..8 {
        coordinator().tick(&store, &launcher, NOW + later).unwrap();
    }
    assert_eq!(launcher.started.borrow().len(), 2);
    stop_all(&store, NOW + 8);
    coordinator().tick(&store, &launcher, NOW + 9).unwrap();
    assert_eq!(launcher.started.borrow().len(), 4);
}

#[test]
fn every_resume_of_held_workers_is_recorded() {
    let repo = repo();
    let store = open(&repo);
    let project = repo.root.to_string_lossy().into_owned();
    let launcher = FakeLauncher::default();
    let started = two_running(&store, &project, &launcher);
    let stopped = TurnEnd {
        stopped: true,
        ..TurnEnd::default()
    };
    for session in &started {
        turn_ended(&store, *session, &stopped, NOW + 2).unwrap();
    }
    stop_all(&store, NOW + 3);
    let at = revision(&store);
    resume(
        &store,
        "t",
        at,
        Requester::Mono,
        &Resume {
            assignments: vec!["a".into()],
            ..Resume::default()
        },
        NOW + 4,
    )
    .unwrap();
    assert_eq!(revision(&store), at);
    resume(
        &store,
        "t",
        at,
        Requester::Mono,
        &Resume {
            assignments: vec!["b".into()],
            ..Resume::default()
        },
        NOW + 5,
    )
    .unwrap();
    let resumes = kinds(&store, "task.resume_requested");
    assert_eq!(resumes.len(), 2);
    assert_eq!(resumes[1].payload["released"].as_array().unwrap().len(), 1);
}

#[test]
fn a_named_assignment_that_cannot_move_is_reported_as_skipped() {
    let repo = repo();
    let store = open(&repo);
    let project = repo.root.to_string_lossy().into_owned();
    let launcher = FakeLauncher::default();
    two_running(&store, &project, &launcher);
    let drop_b = Steer {
        requirements: "Drop b".into(),
        plan: Some(crate::orchestration::Plan {
            title: "Plan".into(),
            summary: String::new(),
            tasks: vec![independent("a")],
        }),
        ..Steer::default()
    };
    steer(
        &store,
        "t",
        plan_revision(&store),
        Requester::Mono,
        &drop_b,
        NOW + 2,
    )
    .unwrap();
    assert_eq!(status(&store, "b"), AssignmentState::Cancelled);
    let named = Steer {
        requirements: "Also b".into(),
        assignments: vec!["b".into()],
        plan: None,
    };
    let steered = steer(
        &store,
        "t",
        plan_revision(&store),
        Requester::Mono,
        &named,
        NOW + 3,
    )
    .unwrap();
    assert!(steered.affected.is_empty());
    assert_eq!(steered.skipped.len(), 1);
    assert_eq!(steered.skipped[0].id, "b");
    assert!(steered.skipped[0].reason.contains("cancelled"));
    assert_eq!(status(&store, "b"), AssignmentState::Cancelled);
}

#[test]
fn routine_task_changes_do_not_refuse_a_steer_but_a_newer_plan_does() {
    let repo = repo();
    let store = open(&repo);
    let project = repo.root.to_string_lossy().into_owned();
    let launcher = FakeLauncher::default();
    two_running(&store, &project, &launcher);
    let read = plan_revision(&store);
    // The task changes (paused, resumed) but its plan does not.
    pause(&store, "t", revision(&store), Requester::User, NOW + 2).unwrap();
    resume(
        &store,
        "t",
        revision(&store),
        Requester::User,
        &Resume::default(),
        NOW + 3,
    )
    .unwrap();
    let first = Steer {
        requirements: "Redo a".into(),
        assignments: vec!["a".into()],
        plan: None,
    };
    steer(&store, "t", read, Requester::User, &first, NOW + 4).unwrap();
    // A second steer written against the same (now older) plan is stale.
    let second = Steer {
        requirements: "Redo b".into(),
        assignments: vec!["b".into()],
        plan: None,
    };
    let stale = steer(&store, "t", read, Requester::User, &second, NOW + 5);
    assert!(matches!(stale, Err(TaskError::Stale { .. })), "{stale:?}");
}
