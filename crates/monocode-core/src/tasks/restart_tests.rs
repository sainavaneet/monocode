//! Restart reconciliation against temporary Git repositories: a second
//! coordinator (a new process) takes over after the old lease expired.
use super::*;
use crate::tasks::coordinator::{ExplicitStart, LEASE_TTL_MS};
use crate::tasks::decisions::tests::ask;
use crate::tasks::lifecycle::{display, pause, resume, Display};
use crate::tasks::recovery::tests::{
    attempts, coordinator, fail, kinds, launched, launched_unconfirmed, stop_all, Recorded,
};
use crate::tasks::review::tests::{git, open, repo, scoped, NOW};
use crate::tasks::store::tests::plan;
use crate::tasks::{
    policy, recovery, AssignmentState, DecisionState, NewTask, Requester, State, Subject, TaskStage,
};

/// When the next process starts: the quit process's lease has expired.
const LATER: u64 = NOW + LEASE_TTL_MS + 1_000;

fn restarted() -> Coordinator {
    Coordinator::new("gpui-2", ExplicitStart)
}

fn current(store: &Store) -> Task {
    store.tasks().unwrap().task("t").unwrap().unwrap()
}

fn brief(store: &Store, session: i64) -> String {
    store.follow_ups(session).unwrap().draft.unwrap().text
}

struct NoRestart;
impl Policy for NoRestart {
    fn restart_continuation(&self, _task: &Task) -> bool {
        false
    }
}

#[test]
fn a_crash_after_the_launch_intent_does_not_duplicate_the_launch() {
    let repo = repo();
    let store = open(&repo);
    let new = NewTask {
        id: "t".into(),
        mono_id: Some("mono-1".into()),
        project: repo.root.to_string_lossy().into_owned(),
        title: "Ship it".into(),
        request: "Implement the feature".into(),
        criteria: vec![],
    };
    let task = policy::create_task(&store, &new, "codex", NOW).unwrap();
    let tasks = store.tasks().unwrap();
    tasks
        .save_plan("t", 1, &plan(vec![scoped(&[])]), NOW)
        .unwrap();
    let t = Subject::Task("t");
    for stage in [TaskStage::Ready, TaskStage::Implementing] {
        let revision = tasks.revision(t).unwrap();
        tasks
            .transition(t, State::Stage(stage), Requester::Mono, revision, NOW)
            .unwrap();
    }
    let a = Subject::Assignment { task: "t", id: "a" };
    let revision = tasks.revision(a).unwrap();
    let ready = State::Assignment(AssignmentState::Ready);
    tasks
        .transition(a, ready, Requester::Coordinator, revision, NOW)
        .unwrap();
    // The old process persisted the intent and quit before dispatching.
    let old = coordinator();
    old.lease(&tasks, "t", NOW).unwrap();
    let task = tasks.task(&task.id).unwrap().unwrap();
    let assignment = tasks.assignment("t", "a").unwrap().unwrap();
    old.begin_launch(&store, &task, &assignment, NOW)
        .unwrap()
        .unwrap();

    let launcher = Recorded::default();
    let startup = restarted().startup(&store, LATER).unwrap();
    assert_eq!(startup.reconciled, vec!["t".to_owned()]);
    assert!(startup.interrupted.is_empty(), "{startup:?}");
    for later in 0..3 {
        restarted().tick(&store, &launcher, LATER + later).unwrap();
    }
    assert_eq!(launcher.count(), 1);
    assert_eq!(attempts(&store).len(), 1);
}

#[test]
fn running_workers_are_interrupted_on_restart_and_continue_on_their_lineage() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    // One retry is already spent on this lineage.
    fail(&store, session, "503 Service Unavailable", NOW + 1);
    stop_all(&store, NOW + 2);
    let due = NOW + recovery::backoff_ms(1) + 2;
    coordinator().tick(&store, &launcher, due).unwrap();
    let second = launcher.last().0.id;
    assert!(crate::tasks::confirm_started(&store, second, due).unwrap());
    let root = attempts(&store)[0].id.clone();
    let running = attempts(&store)[1].id.clone();

    let startup = restarted().startup(&store, due + LEASE_TTL_MS + 1).unwrap();
    assert_eq!(startup.interrupted, vec![running.clone()]);
    let tasks = store.tasks().unwrap();
    let row = recovery::recovery(&tasks, &running).unwrap().unwrap();
    assert_eq!(row.failure, RESTART);
    assert_eq!(row.root_attempt, root);
    assert_eq!(row.retries_used, 1, "a restart spends no retry");
    // A second startup of the same process changes nothing.
    let again = restarted().startup(&store, due + LEASE_TTL_MS + 2).unwrap();
    assert!(again.reconciled.is_empty() && again.interrupted.is_empty());

    restarted()
        .tick(&store, &launcher, due + LEASE_TTL_MS + 3)
        .unwrap();
    assert_eq!(launcher.count(), 3);
    let all = attempts(&store);
    assert_eq!(all.len(), 3);
    let row = recovery::recovery(&tasks, &running).unwrap().unwrap();
    assert_eq!(row.state, "replaced");
    assert_eq!(row.retries_used, 1);
    assert!(crate::tasks::handoff::of(&tasks, &all[2].id)
        .unwrap()
        .is_some());
}

#[test]
fn a_crash_after_a_persisted_answer_keeps_it() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    let id = ask(&store, session, "q1", "Which parser?", "assignment");
    let tasks = store.tasks().unwrap();
    let revision = tasks.decision(&id).unwrap().unwrap().revision;
    tasks
        .answer_as(&id, revision, "Use nom", Requester::Mono, NOW + 5)
        .unwrap();
    // The app quits before the answer reaches the worker.

    let startup = restarted().startup(&store, LATER).unwrap();
    assert_eq!(startup.interrupted.len(), 1);
    let kept = tasks.decision(&id).unwrap().unwrap();
    assert_eq!(kept.state, DecisionState::Answered);
    assert_eq!(kept.answer.as_deref(), Some("Use nom"));
    restarted().tick(&store, &launcher, LATER + 1).unwrap();
    assert_eq!(launcher.count(), 2);
    let replacement = launcher.last().0.id;
    assert!(brief(&store, replacement).contains("Use nom"));
    assert!(crate::tasks::confirm_started(&store, replacement, LATER + 2).unwrap());
    let resolved = tasks.decision(&id).unwrap().unwrap();
    assert_eq!(resolved.state, DecisionState::Resolved);
    assert_eq!(resolved.answer.as_deref(), Some("Use nom"));
}

#[test]
fn paused_tasks_stay_paused_across_restart() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    launched(&store, &repo, &launcher);
    let revision = current(&store).revision;
    pause(&store, "t", revision, Requester::User, NOW + 5).unwrap();
    stop_all(&store, NOW + 6);

    let startup = restarted().startup(&store, LATER).unwrap();
    assert_eq!(startup.reconciled, vec!["t".to_owned()]);
    assert!(startup.interrupted.is_empty());
    for later in 1..=3 {
        restarted().tick(&store, &launcher, LATER + later).unwrap();
    }
    assert_eq!(launcher.count(), 1);
    let task = current(&store);
    assert_eq!(task.lifecycle, TaskLifecycle::Paused);
    assert_eq!(
        display(&store.tasks().unwrap(), &task).unwrap(),
        Display::Paused
    );
    // Resuming after the restart continues the paused work.
    resume(&store, "t", task.revision, Requester::User, &[], LATER + 10).unwrap();
    restarted().tick(&store, &launcher, LATER + 11).unwrap();
    assert_eq!(launcher.count(), 2);
}

#[test]
fn without_restart_continuation_the_task_waits_with_a_reason() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    launched(&store, &repo, &launcher);
    let next = Coordinator::new("gpui-2", NoRestart);
    let startup = next.startup(&store, LATER).unwrap();
    assert_eq!(startup.held.len(), 1);
    let task = current(&store);
    assert_eq!(task.lifecycle, TaskLifecycle::Waiting);
    assert!(task.blocked_reason.unwrap().contains("policy"));
    assert_eq!(kinds(&store, "task.restart_held").len(), 1);
    next.tick(&store, &launcher, LATER + 1).unwrap();
    assert_eq!(launcher.count(), 1);
    let revision = current(&store).revision;
    resume(&store, "t", revision, Requester::User, &[], LATER + 2).unwrap();
    assert_eq!(current(&store).lifecycle, TaskLifecycle::Active);
    next.tick(&store, &launcher, LATER + 3).unwrap();
    assert_eq!(launcher.count(), 2);
}

#[test]
fn a_missing_worktree_yields_the_recovery_condition() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    launched(&store, &repo, &launcher);
    let workspace = store.tasks().unwrap().workspaces("t").unwrap().remove(0);
    std::fs::remove_dir_all(&workspace.path).unwrap();

    let startup = restarted().startup(&store, LATER).unwrap();
    assert_eq!(startup.missing, vec![workspace.id.clone()]);
    let task = current(&store);
    assert_eq!(task.lifecycle, TaskLifecycle::Blocked);
    assert!(task.blocked_reason.unwrap().contains("missing or moved"));
    assert_eq!(kinds(&store, "recovery.workspace_missing").len(), 2);
    for later in 1..=3 {
        restarted().tick(&store, &launcher, LATER + later).unwrap();
    }
    assert_eq!(launcher.count(), 1, "never a fresh start");
    assert!(!std::path::Path::new(&workspace.path).exists());
}

#[test]
fn a_moved_worktree_branch_is_a_recovery_condition_too() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    launched(&store, &repo, &launcher);
    let workspace = store.tasks().unwrap().workspaces("t").unwrap().remove(0);
    git(
        std::path::Path::new(&workspace.path),
        &["checkout", "-q", "-b", "elsewhere"],
    );
    let startup = restarted().startup(&store, LATER).unwrap();
    assert_eq!(startup.missing, vec![workspace.id]);
    assert_eq!(current(&store).lifecycle, TaskLifecycle::Blocked);
}

#[test]
fn a_lease_another_app_holds_keeps_startup_passive_until_it_expires() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    launched_unconfirmed(&store, &repo, &launcher);
    let session = launcher.last().0.id;
    assert!(crate::tasks::confirm_started(&store, session, NOW).unwrap());
    let tasks = store.tasks().unwrap();
    tasks
        .acquire_lease(
            &lease_resource("t"),
            "monocode-desktop",
            LEASE_TTL_MS,
            LATER,
        )
        .unwrap();
    let startup = restarted().startup(&store, LATER + 1).unwrap();
    assert_eq!(startup.leased_elsewhere, vec!["t".to_owned()]);
    assert!(startup.reconciled.is_empty());
    assert_eq!(attempts(&store)[0].status, AttemptState::Running);
    // Once that lease lapses, the next tick takes the task over.
    let after = LATER + LEASE_TTL_MS + 2;
    restarted().tick(&store, &launcher, after).unwrap();
    assert_eq!(attempts(&store)[0].status, AttemptState::Interrupted);
    assert_eq!(kinds(&store, "task.restart_reconciled").len(), 1);
}
