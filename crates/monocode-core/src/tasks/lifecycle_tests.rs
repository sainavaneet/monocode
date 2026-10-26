//! Pause, resume and cancel through the coordinator against temporary Git
//! repositories; the recorded launcher stands in for the app.
use super::*;
use crate::app_access::worker_action;
use crate::mono::habits::{APPROVAL_WAIT_MS, GRACE_MS, TIMEOUT_MS};
use crate::tasks::decisions::tests::ask;
use crate::tasks::recovery::tests::{attempts, coordinator, kinds, launched, stop_all, Recorded};
use crate::tasks::recovery::{self, turn_ended, Outcome, TurnEnd};
use crate::tasks::review::tests::{open, repo, NOW};
use serde_json::json;

fn current(store: &Store) -> Task {
    store.tasks().unwrap().task("t").unwrap().unwrap()
}

fn assignment_status(store: &Store) -> AssignmentState {
    let tasks = store.tasks().unwrap();
    tasks.assignment("t", "a").unwrap().unwrap().status
}

fn shown(store: &Store) -> Display {
    display(&store.tasks().unwrap(), &current(store)).unwrap()
}

fn stopped_turn() -> TurnEnd {
    TurnEnd {
        stopped: true,
        ..TurnEnd::default()
    }
}

#[test]
fn pausing_interrupts_workers_before_stopping_them_and_shows_pausing_until_confirmed() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    let revision = current(&store).revision;

    let paused = pause(&store, "t", revision, Requester::User, NOW + 10).unwrap();
    assert_eq!(paused.display, Display::Pausing);
    let first = attempts(&store).remove(0);
    assert_eq!(paused.attempts, vec![first.id.clone()]);
    // Moved before the stop: the worker is Interrupted, its work kept.
    assert_eq!(first.status, AttemptState::Interrupted);
    assert_eq!(assignment_status(&store), AssignmentState::Running);
    assert_eq!(recovery::pending_stops(&store).unwrap().len(), 1);
    let rows = controls(&store, "mono-1").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].display, Display::Pausing);

    // The stopped turn ending is not a failure.
    let ended = turn_ended(&store, session, &stopped_turn(), NOW + 11).unwrap();
    assert_eq!(ended, Outcome::Ignored);
    assert!(kinds(&store, "recovery.ended_without_result").is_empty());
    assert_eq!(assignment_status(&store), AssignmentState::Running);

    // Paused once the app confirms the stop; nothing launches meanwhile.
    assert_eq!(stop_all(&store, NOW + 12), 1);
    assert_eq!(shown(&store), Display::Paused);
    for later in 1..=3 {
        coordinator()
            .tick(&store, &launcher, NOW + 20 * later)
            .unwrap();
    }
    assert_eq!(launcher.count(), 1);

    // Resuming continues on the same lineage and account, spending no retry.
    let revision = current(&store).revision;
    let resumed = resume(
        &store,
        "t",
        revision,
        Requester::User,
        &Resume::default(),
        NOW + 100,
    )
    .unwrap();
    assert_eq!(resumed.display, Display::Active);
    assert!(resumed.blocked.is_none());
    // The paused worker's session is stopped again (the user may have typed
    // into it): nothing launches until that is confirmed.
    coordinator().tick(&store, &launcher, NOW + 101).unwrap();
    assert_eq!(launcher.count(), 1);
    assert_eq!(stop_all(&store, NOW + 102), 1);
    coordinator().tick(&store, &launcher, NOW + 103).unwrap();
    assert_eq!(launcher.count(), 2);
    let all = attempts(&store);
    assert_eq!(all.len(), 2);
    assert_eq!(all[1].account_id, first.account_id);
    let tasks = store.tasks().unwrap();
    let row = recovery::recovery(&tasks, &first.id).unwrap().unwrap();
    assert_eq!(row.failure, PAUSED);
    assert_eq!(row.state, "replaced");
    assert_eq!(row.retries_used, 0);
    assert!(crate::tasks::handoff::of(&tasks, &all[1].id)
        .unwrap()
        .is_some());
    // The pause and the resume are in the history.
    assert_eq!(kinds(&store, "task.pause_requested").len(), 1);
    assert_eq!(kinds(&store, "task.resume_requested").len(), 1);
}

#[test]
fn late_results_do_not_revive_cancelled_tasks() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    let revision = current(&store).revision;

    let cancelled = cancel(&store, "t", revision, Requester::Mono, NOW + 10).unwrap();
    assert_eq!(cancelled.display, Display::Cancelling);
    assert_eq!(attempts(&store)[0].status, AttemptState::Cancelled);
    assert_eq!(assignment_status(&store), AssignmentState::Cancelled);

    // The worker reports after the cancel: kept in history, changes nothing.
    let late = worker_action(
        &store,
        session,
        "late",
        "tasks.worker.result",
        &json!({"outcome": "succeeded", "summary": "Finished anyway"}),
        NOW + 11,
    );
    assert!(late.is_err(), "{late:?}");
    assert_eq!(kinds(&store, "attempt.late_claim").len(), 1);
    let ended = turn_ended(&store, session, &stopped_turn(), NOW + 12).unwrap();
    assert_eq!(ended, Outcome::Ignored);
    for later in 1..=3 {
        coordinator()
            .tick(&store, &launcher, NOW + 20 * later)
            .unwrap();
    }
    assert_eq!(launcher.count(), 1);
    assert_eq!(current(&store).lifecycle, TaskLifecycle::Cancelled);
    assert_eq!(assignment_status(&store), AssignmentState::Cancelled);
    // Neither a resume nor a review reopens it.
    let revision = current(&store).revision;
    assert!(resume(
        &store,
        "t",
        revision,
        Requester::User,
        &Resume::default(),
        NOW + 90
    )
    .is_err());
    let tasks = store.tasks().unwrap();
    let a = Subject::Assignment { task: "t", id: "a" };
    let rev = tasks.revision(a).unwrap();
    let accepted = State::Assignment(AssignmentState::Accepted);
    assert!(tasks
        .transition(a, accepted, Requester::Mono, rev, NOW + 91)
        .is_err());
    // Cancelling until the app confirms the stop; then gone from controls.
    assert_eq!(
        controls(&store, "mono-1").unwrap()[0].display,
        Display::Cancelling
    );
    stop_all(&store, NOW + 92);
    assert_eq!(shown(&store), Display::Cancelled);
    assert!(controls(&store, "mono-1").unwrap().is_empty());
    // The worktree and its branch are kept.
    let workspaces = tasks.workspaces("t").unwrap();
    assert!(std::path::Path::new(&workspaces[0].path).is_dir());
}

#[test]
fn a_user_stop_holds_the_worker_and_tells_mono_instead_of_failing_it() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);

    let ended = turn_ended(&store, session, &stopped_turn(), NOW + 10).unwrap();
    assert_eq!(ended, Outcome::Held);
    let first = attempts(&store).remove(0);
    assert_eq!(first.status, AttemptState::Interrupted);
    assert_eq!(assignment_status(&store), AssignmentState::Running);
    assert_eq!(kinds(&store, "recovery.stopped_by_user").len(), 1);
    assert!(kinds(&store, "assignment.failed").is_empty());
    stop_all(&store, NOW + 11);
    for later in 1..=3 {
        coordinator()
            .tick(&store, &launcher, NOW + 20 * later)
            .unwrap();
    }
    assert_eq!(
        launcher.count(),
        1,
        "a held worker waits for Mono or the user"
    );

    let revision = current(&store).revision;
    let resumed = resume(
        &store,
        "t",
        revision,
        Requester::Mono,
        &Resume {
            assignments: vec!["a".into()],
            ..Resume::default()
        },
        NOW + 90,
    )
    .unwrap();
    assert_eq!(resumed.attempts, vec![first.id]);
    // The user still drives the stopped session: resuming stops it first,
    // and nothing launches until its turn has ended.
    for later in 1..=3 {
        coordinator()
            .tick(&store, &launcher, NOW + 90 + later)
            .unwrap();
    }
    assert_eq!(launcher.count(), 1);
    assert_eq!(stop_all(&store, NOW + 95), 1);
    coordinator().tick(&store, &launcher, NOW + 96).unwrap();
    assert_eq!(launcher.count(), 2);
}

#[test]
fn a_task_wide_question_makes_the_task_wait_until_it_is_answered() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    let id = ask(&store, session, "q1", "Rename the crate?", "task");

    coordinator().tick(&store, &launcher, NOW + 10).unwrap();
    let waiting = current(&store);
    assert_eq!(waiting.lifecycle, TaskLifecycle::Waiting);
    assert!(waiting.blocked_reason.unwrap().contains(&id));
    assert_eq!(shown(&store), Display::Waiting);

    let tasks = store.tasks().unwrap();
    let revision = tasks.decision(&id).unwrap().unwrap().revision;
    tasks
        .answer_as(&id, revision, "Keep the name", Requester::Mono, NOW + 20)
        .unwrap();
    coordinator().tick(&store, &launcher, NOW + 30).unwrap();
    let active = current(&store);
    assert_eq!(active.lifecycle, TaskLifecycle::Active);
    assert_eq!(active.blocked_reason, None);
}

#[test]
fn the_coordinator_never_times_out_a_task_worker_past_habit_timeouts() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    let far = NOW + TIMEOUT_MS + APPROVAL_WAIT_MS + GRACE_MS + 1;
    for later in 0..3 {
        coordinator().tick(&store, &launcher, far + later).unwrap();
    }
    assert_eq!(attempts(&store)[0].status, AttemptState::Running);
    assert_eq!(assignment_status(&store), AssignmentState::Running);
    // A worker waiting for an answer waits longer than any habit would.
    let id = ask(&store, session, "q1", "Which parser?", "assignment");
    let farther = far + 2 * APPROVAL_WAIT_MS;
    coordinator().tick(&store, &launcher, farther).unwrap();
    let tasks = store.tasks().unwrap();
    assert_eq!(attempts(&store)[0].status, AttemptState::Waiting);
    assert_eq!(
        tasks.decision(&id).unwrap().unwrap().state,
        crate::tasks::DecisionState::Open
    );
    assert!(recovery::pending_stops(&store).unwrap().is_empty());
    assert!(kinds(&store, "attempt.failure").is_empty());
    assert_eq!(launcher.count(), 1);
}

#[test]
fn stopped_worker_sessions_say_they_are_detached() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    assert_eq!(detached(&store, session).unwrap(), None);
    let revision = current(&store).revision;
    pause(&store, "t", revision, Requester::User, NOW + 10).unwrap();
    let notice = detached(&store, session).unwrap().unwrap();
    assert!(
        notice.contains("Detached from task \"Ship it\""),
        "{notice}"
    );
    // Typing there afterwards reports nothing to the task.
    let typed = worker_action(
        &store,
        session,
        "after",
        "tasks.worker.blocked",
        &json!({"reason": "typed by the user"}),
        NOW + 11,
    );
    assert!(typed.is_err());
    let ordinary = store.create("codex", "/elsewhere").unwrap().id;
    assert_eq!(detached(&store, ordinary).unwrap(), None);
}
