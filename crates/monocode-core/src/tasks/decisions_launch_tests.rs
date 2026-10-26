//! Decisions through the coordinator against temporary Git repositories:
//! scopes hold launches, and a replacement worker takes over a crashed
//! worker's question with its answer.
use super::tests::{answer, ask, decision, events, independent, status, task_with, NOW};
use crate::app_access::worker_action;
use crate::orchestration::WorkspaceMode;
use crate::tasks::coordinator::{Coordinator, ExplicitStart, Policy};
use crate::tasks::review::tests::{open, repo, FakeLauncher};
use crate::tasks::store::tests::assignment;
use crate::tasks::{
    confirm_started, AssignmentState, AttemptState, DecisionState, Requester, State, Subject,
};
use serde_json::json;

struct Two;
impl Policy for Two {
    fn worker_limit(&self, _task: &crate::tasks::Task) -> usize {
        2
    }
}

#[test]
fn a_task_wide_question_stops_new_launches_while_independent_work_continues() {
    let repo = repo();
    let store = open(&repo);
    let project = repo.root.to_string_lossy().into_owned();
    let mut b = independent("b");
    b.workspace = Some(WorkspaceMode::Worktree);
    let mut c = independent("c");
    c.workspace = Some(WorkspaceMode::Worktree);
    let mut a = independent("a");
    a.workspace = Some(WorkspaceMode::Worktree);
    task_with(&store, &project, vec![a, b, c]);
    let coordinator = Coordinator::new("gpui-1", Two);
    let launcher = FakeLauncher::default();
    coordinator.tick(&store, &launcher, NOW).unwrap();
    let started: Vec<i64> = launcher.started.borrow().iter().map(|s| s.id).collect();
    assert_eq!(started.len(), 2);
    for session in &started {
        confirm_started(&store, *session, NOW + 1).unwrap();
    }
    assert_eq!(status(&store, "a"), AssignmentState::Running);
    assert_eq!(status(&store, "b"), AssignmentState::Running);
    // a stops the whole task; b keeps running; c may not start.
    let id = ask(&store, started[0], "q1", "Rename the crate?", "task");
    for later in 1..=3 {
        coordinator
            .tick(&store, &launcher, NOW + 10 * later)
            .unwrap();
    }
    assert_eq!(launcher.started.borrow().len(), 2);
    assert_eq!(status(&store, "b"), AssignmentState::Running);
    assert_eq!(status(&store, "c"), AssignmentState::Ready);
    // b finishes meanwhile; c still waits for the decision.
    worker_action(
        &store,
        started[1],
        "r1",
        "tasks.worker.result",
        &json!({"outcome": "succeeded", "summary": "Done"}),
        NOW + 50,
    )
    .unwrap();
    coordinator.tick(&store, &launcher, NOW + 60).unwrap();
    assert_eq!(launcher.started.borrow().len(), 2);
    answer(&store, &id, "Keep the name");
    let tasks = store.tasks().unwrap();
    let handoff = tasks.pending_handoffs().unwrap().remove(0);
    tasks
        .resolve_handoff(
            &id,
            handoff.decision.revision,
            &handoff.decision.attempt_id.unwrap(),
            "message",
            NOW + 70,
        )
        .unwrap();
    coordinator.tick(&store, &launcher, NOW + 80).unwrap();
    assert_eq!(launcher.started.borrow().len(), 3);
    assert_eq!(status(&store, "c"), AssignmentState::Running);
}

#[test]
fn a_replacement_worker_receives_the_pending_answer() {
    let repo = repo();
    let store = open(&repo);
    let project = repo.root.to_string_lossy().into_owned();
    let investigation = crate::tasks::Assignment {
        files: vec![".".into()],
        workspace: Some(WorkspaceMode::Checkout),
        ..assignment("a", &[])
    };
    task_with(&store, &project, vec![investigation]);
    let coordinator = Coordinator::new("gpui-1", ExplicitStart);
    let launcher = FakeLauncher::default();
    coordinator.tick(&store, &launcher, NOW).unwrap();
    let first = launcher.started.borrow()[0].id;
    confirm_started(&store, first, NOW + 1).unwrap();
    let tasks = store.tasks().unwrap();
    let x = tasks.attempts("t", "a").unwrap()[0].id.clone();
    let id = ask(
        &store,
        first,
        "q1",
        "Which module owns parsing?",
        "assignment",
    );

    // The waiting worker crashes; its question stays.
    let coordinator_moves = |subject: Subject<'_>, to: State| {
        let rev = tasks.revision(subject).unwrap();
        tasks
            .transition(subject, to, Requester::Coordinator, rev, NOW + 5)
            .unwrap();
    };
    coordinator_moves(
        Subject::Attempt(&x),
        State::Attempt(AttemptState::Interrupted),
    );
    let a = Subject::Assignment { task: "t", id: "a" };
    coordinator_moves(a, State::Assignment(AssignmentState::Failed));
    coordinator_moves(a, State::Assignment(AssignmentState::Queued));
    // No replacement runs while the question is open.
    coordinator.tick(&store, &launcher, NOW + 10).unwrap();
    assert_eq!(launcher.started.borrow().len(), 1);
    assert_eq!(decision(&store, &id).state, DecisionState::Open);

    answer(&store, &id, "The parser module");
    // The crashed worker is gone, so nothing is sent to it.
    assert!(tasks.pending_handoffs().unwrap().is_empty());
    coordinator.tick(&store, &launcher, NOW + 20).unwrap();
    assert_eq!(launcher.started.borrow().len(), 2);
    let replacement = tasks.attempts("t", "a").unwrap().pop().unwrap();
    assert_ne!(replacement.id, x);
    let inherited = decision(&store, &id);
    assert_eq!(
        inherited.attempt_id.as_deref(),
        Some(replacement.id.as_str())
    );
    assert_eq!(inherited.state, DecisionState::Answered);
    assert_eq!(events(&store, "decision.inherited").len(), 1);
    let second = launcher.started.borrow()[1].id;
    let brief = store.follow_ups(second).unwrap().draft.unwrap().text;
    for needle in [
        "Which module owns parsing?",
        "The parser module",
        id.as_str(),
    ] {
        assert!(brief.contains(needle), "{needle}: {brief}");
    }
    // The confirmed brief is the hand-off: resolved once, never re-sent.
    assert!(confirm_started(&store, second, NOW + 21).unwrap());
    assert_eq!(decision(&store, &id).state, DecisionState::Resolved);
    assert!(tasks.pending_handoffs().unwrap().is_empty());
    let status = worker_action(
        &store,
        second,
        "s1",
        "tasks.worker.status",
        &json!({}),
        NOW + 22,
    )
    .unwrap();
    assert_eq!(status["decisions"][0]["id"], id.as_str());
    assert_eq!(status["decisions"][0]["answer"], "The parser module");
}
