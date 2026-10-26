//! A worker that reports its result is stopped: nothing relaunches beside
//! its still-running turn, and review waits until the stop is confirmed so
//! a late write cannot slip in under the reviewed snapshot.
use crate::store::Store;
use crate::tasks::coordinator::{Coordinator, ExplicitStart};
use crate::tasks::recovery::{pending_stops, stopped};
use crate::tasks::review::tests::{
    ack_stops, feature, open, repo, scoped, stage_to, FakeLauncher, CHECK, NOW,
};
use crate::tasks::store::tests::plan;
use crate::tasks::{
    confirm_started, AcceptanceCriterion, AssignmentState, NewTask, Requester, State, Subject,
    TaskStage,
};
use serde_json::json;

struct Reported {
    coordinator: Coordinator,
    launcher: FakeLauncher,
}

/// Task `t` with one launched worker that reported `outcome`.
fn reported(store: &Store, root: &std::path::Path, outcome: &str) -> Reported {
    let tasks = store.tasks().unwrap();
    let task = NewTask {
        id: "t".into(),
        mono_id: Some("mono-1".into()),
        project: root.to_string_lossy().into_owned(),
        title: "Ship it".into(),
        request: "Implement the feature".into(),
        criteria: vec![AcceptanceCriterion {
            id: "c1".into(),
            description: "The feature exists".into(),
            required: true,
            checks: vec![CHECK.into()],
        }],
    };
    tasks.create_task(&task, NOW).unwrap();
    tasks
        .save_plan("t", 1, &plan(vec![scoped(&[CHECK])]), NOW)
        .unwrap();
    for stage in [TaskStage::Ready, TaskStage::Implementing] {
        stage_to(store, stage);
    }
    let coordinator = Coordinator::new("gpui-1", ExplicitStart);
    let launcher = FakeLauncher::default();
    coordinator.tick(store, &launcher, NOW).unwrap();
    let session = launcher.started.borrow()[0].id;
    assert!(confirm_started(store, session, NOW + 1).unwrap());
    feature(std::path::Path::new(
        &tasks.workspaces("t").unwrap()[0].path,
    ));
    crate::app_access::worker_action(
        store,
        session,
        "r1",
        "tasks.worker.result",
        &json!({"outcome": outcome, "summary": "Reported", "validation": [CHECK]}),
        NOW + 2,
    )
    .unwrap();
    Reported {
        coordinator,
        launcher,
    }
}

#[test]
fn a_reported_result_stops_the_worker() {
    for outcome in ["succeeded", "failed"] {
        let repo = repo();
        let store = open(&repo);
        reported(&store, &repo.root, outcome);
        let stops = pending_stops(&store).unwrap();
        assert_eq!(stops.len(), 1, "{outcome}: {stops:?}");
    }
}

#[test]
fn a_sent_back_assignment_does_not_relaunch_until_the_stop_is_confirmed() {
    let repo = repo();
    let store = open(&repo);
    let run = reported(&store, &repo.root, "succeeded");
    // Mono sends the claim back before any review.
    let tasks = store.tasks().unwrap();
    let subject = Subject::Assignment { task: "t", id: "a" };
    let queued = State::Assignment(AssignmentState::Queued);
    let revision = tasks.revision(subject).unwrap();
    tasks
        .transition(subject, queued, Requester::Mono, revision, NOW + 3)
        .unwrap();
    for later in 4..7 {
        let tick = run
            .coordinator
            .tick(&store, &run.launcher, NOW + later)
            .unwrap();
        assert!(tick.launched.is_empty(), "{tick:?}");
    }
    assert_eq!(run.launcher.started.borrow().len(), 1);
    ack_stops(&store);
    let tick = run
        .coordinator
        .tick(&store, &run.launcher, NOW + 8)
        .unwrap();
    assert_eq!(tick.launched.len(), 1, "{tick:?}");
}

#[test]
fn review_waits_for_the_reporting_worker_to_stop() {
    let repo = repo();
    let store = open(&repo);
    let run = reported(&store, &repo.root, "succeeded");
    let tick = run
        .coordinator
        .tick(&store, &run.launcher, NOW + 3)
        .unwrap();
    assert_eq!(tick.reviews.len(), 1, "{tick:?}");
    assert!(run.coordinator.review(&store, NOW + 4).unwrap().is_empty());
    let stop = pending_stops(&store).unwrap().remove(0);
    stopped(&store, &stop.delivery, NOW + 5).unwrap();
    let reviews = run.coordinator.review(&store, NOW + 6).unwrap();
    assert_eq!(reviews.len(), 1, "{reviews:?}");
}
