//! The coordinator against temporary Git repositories, file-backed stores and
//! a fake launcher standing in for the GUI's session start.
use super::*;
use crate::agent::Settings;
use crate::orchestration::WorkspaceMode;
use crate::store::{Session, Store};
use crate::tasks::store::tests::{assignment, plan};
use crate::tasks::{
    confirm_started, start_failed, AssignmentState, AttemptState, NewTask, Subject,
};
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::process::Command;

const NOW: u64 = 1_000_000;

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn repo() -> Repo {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let root = base.join("project");
    std::fs::create_dir_all(root.join("src")).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "test@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("src/lib.rs"), "pub fn one() {}\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "init"]);
    Repo {
        db: base.join("monocode.db"),
        _dir: dir,
        root,
    }
}

/// A file-backed store, as two processes sharing monocode.db would open it.
fn open(repo: &Repo) -> Store {
    let store = Store::open(&repo.db).unwrap();
    store.save_setting(FLAG, "enabled").unwrap();
    store
}

/// Task `t`, planned and explicitly started by Mono.
fn started(store: &Store, repo: &Repo, assignments: Vec<crate::tasks::Assignment>) {
    let tasks = store.tasks().unwrap();
    tasks
        .create_task(
            &NewTask {
                id: "t".into(),
                mono_id: Some("mono-1".into()),
                project: repo.root.to_string_lossy().into_owned(),
                title: "Ship it".into(),
                request: "Implement the feature".into(),
                criteria: crate::tasks::store::tests::new_task("t").criteria,
            },
            NOW,
        )
        .unwrap();
    tasks.save_plan("t", 1, &plan(assignments), NOW).unwrap();
    let t = Subject::Task("t");
    for stage in [TaskStage::Ready, TaskStage::Implementing] {
        let revision = tasks.revision(t).unwrap();
        tasks
            .transition(t, State::Stage(stage), Requester::Mono, revision, NOW)
            .unwrap();
    }
}

/// Reads the project in place: plans mark investigation with `checkout`.
fn investigation(id: &str) -> crate::tasks::Assignment {
    crate::tasks::Assignment {
        files: vec![".".into()],
        workspace: Some(WorkspaceMode::Checkout),
        ..assignment(id, &[])
    }
}

fn scoped(id: &str, files: &[&str], deps: &[&str]) -> crate::tasks::Assignment {
    crate::tasks::Assignment {
        files: files.iter().map(|f| (*f).to_owned()).collect(),
        workspace: None,
        ..assignment(id, deps)
    }
}

/// Stands in for the GUI: records each session it is asked to start.
#[derive(Default)]
struct FakeLauncher {
    started: RefCell<Vec<(Session, Settings)>>,
    crash: Cell<bool>,
    refuse: Cell<bool>,
}

impl Launcher for FakeLauncher {
    fn start(&self, session: &Session, settings: &Settings) -> Result<(), String> {
        if self.crash.get() {
            panic!("the app quit before installing the session");
        }
        if self.refuse.get() {
            return Err("The app is closing".into());
        }
        self.started
            .borrow_mut()
            .push((session.clone(), settings.clone()));
        Ok(())
    }
}

fn coordinator(holder: &str) -> Coordinator {
    Coordinator::new(holder, ExplicitStart)
}

fn status(store: &Store, id: &str) -> AssignmentState {
    let tasks = store.tasks().unwrap();
    tasks.assignment("t", id).unwrap().unwrap().status
}

fn attempts(store: &Store, id: &str) -> Vec<crate::tasks::Attempt> {
    store.tasks().unwrap().attempts("t", id).unwrap()
}

fn events(store: &Store, kind: &str) -> Vec<crate::tasks::Event> {
    let tasks = store.tasks().unwrap();
    let all = tasks.events_after(0, 10_000).unwrap();
    all.into_iter().filter(|e| e.kind == kind).collect()
}

fn report(store: &Store, session: i64, request: &str, outcome: &str) -> serde_json::Value {
    crate::app_access::worker_action(
        store,
        session,
        request,
        "tasks.worker.result",
        &json!({"outcome": outcome, "summary": "Did the work", "commits": ["abc123"]}),
        NOW + 5,
    )
    .unwrap()
}

#[test]
fn the_coordinator_is_inert_until_its_flag_is_enabled() {
    let repo = repo();
    let store = Store::open(&repo.db).unwrap();
    started(&store, &repo, vec![scoped("a", &["src"], &[])]);
    let launcher = FakeLauncher::default();
    let tick = coordinator("gpui-1").tick(&store, &launcher, NOW).unwrap();
    assert!(!tick.enabled);
    let tasks = store.tasks().unwrap();
    assert_eq!(tasks.lease(&lease_resource("t")).unwrap(), None);
    assert!(attempts(&store, "a").is_empty());
    assert!(launcher.started.borrow().is_empty());
    assert!(tasks.workspaces("t").unwrap().is_empty());
    assert_eq!(git(&repo.root, &["worktree", "list"]).lines().count(), 1);
    store.save_setting(FLAG, "enabled").unwrap();
    assert!(enabled(&store));
}

#[test]
fn one_assignment_launches_into_its_worktree_and_reports_durably() {
    let repo = repo();
    let head = git(&repo.root, &["rev-parse", "HEAD"]);
    let store = open(&repo);
    started(&store, &repo, vec![scoped("a", &["src"], &[])]);
    let launcher = FakeLauncher::default();
    let tick = coordinator("gpui-1").tick(&store, &launcher, NOW).unwrap();
    assert!(tick.enabled, "{tick:?}");
    assert!(tick.errors.is_empty(), "{tick:?}");

    // The workspace was recorded with its repository, base, branch and owner.
    let tasks = store.tasks().unwrap();
    let workspaces = tasks.workspaces("t").unwrap();
    assert_eq!(workspaces.len(), 1);
    let workspace = &workspaces[0];
    assert_eq!(workspace.mode, WorkspaceMode::Worktree);
    assert_eq!(workspace.repository, repo.root.to_string_lossy());
    assert_eq!(workspace.base_commit.as_deref(), Some(head.as_str()));
    let branch = workspace.branch.clone().unwrap();
    assert_eq!(branch, crate::tasks::workspaces::branch_for("t", "a"));
    assert_eq!(workspace.owner, "task:t");
    assert!(Path::new(&workspace.path).join("src/lib.rs").is_file());
    assert_ne!(Path::new(&workspace.path), repo.root);
    assert!(git(&repo.root, &["worktree", "list"]).contains(&branch));

    // One attempt, bound to the one session handed to the app. Reserved is
    // not started: it stays Starting until the app confirms.
    let all = attempts(&store, "a");
    assert_eq!(all.len(), 1);
    let attempt = &all[0];
    assert_eq!(attempt.status, AttemptState::Starting);
    assert_eq!(status(&store, "a"), AssignmentState::Running);
    assert_eq!(tick.launched, vec![attempt.id.clone()]);
    let started = launcher.started.borrow();
    assert_eq!(started.len(), 1);
    let (session, settings) = &started[0];
    assert_eq!(
        attempt.session_id.as_deref(),
        Some(store.persisted_session_id(session.id).unwrap().as_str())
    );
    assert_eq!(
        settings.worktree_cwd.as_deref(),
        Some(workspace.path.as_str())
    );
    let saved = store.agent_settings(session.id).unwrap().unwrap();
    assert_eq!(saved.worktree_cwd.as_deref(), Some(workspace.path.as_str()));
    let brief = store.follow_ups(session.id).unwrap().draft.unwrap().text;
    for needle in [
        "Do the work",
        "src",
        "cargo test",
        "tasks.worker.result",
        &workspace.path,
    ] {
        assert!(brief.contains(needle), "brief lacks {needle}: {brief}");
    }

    // The intent was persisted with everything needed to reconcile it.
    let intent = events(&store, "attempt.launch_intent");
    assert_eq!(intent.len(), 1);
    let payload = &intent[0].payload;
    assert_eq!(payload["operationId"], attempt.operation_id);
    assert_eq!(payload["attemptId"], attempt.id);
    assert_eq!(payload["assignmentId"], "a");
    assert_eq!(payload["workspace"]["path"], workspace.path);
    assert_eq!(events(&store, "attempt.reserved").len(), 1);
    drop(started);
    assert!(confirm_started(&store, session_id(&launcher), NOW + 1).unwrap());
    assert_eq!(attempts(&store, "a")[0].status, AttemptState::Running);

    // The worker reports; a duplicated result is one claim and one review.
    let claim = report(&store, session_id(&launcher), "r1", "succeeded");
    assert_eq!(claim["duplicate"], false);
    assert_eq!(
        report(&store, session_id(&launcher), "r2", "succeeded")["duplicate"],
        true
    );
    for later in 1..=3 {
        let tick = coordinator("gpui-1")
            .tick(&store, &launcher, NOW + 10 * later)
            .unwrap();
        assert!(tick.launched.is_empty(), "{tick:?}");
    }
    let reviews = store.tasks().unwrap().pending_deliveries(REVIEW).unwrap();
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].event_id, format!("claim:{}", attempt.id));
    assert_eq!(launcher.started.borrow().len(), 1);
    drop(store);

    // Durable: a fresh connection sees the same outcome.
    let reopened = Store::open(&repo.db).unwrap();
    assert_eq!(status(&reopened, "a"), AssignmentState::NeedsReview);
    assert_eq!(attempts(&reopened, "a")[0].status, AttemptState::Succeeded);
    assert_eq!(
        reopened
            .tasks()
            .unwrap()
            .pending_deliveries(REVIEW)
            .unwrap()
            .len(),
        1
    );
}

fn session_id(launcher: &FakeLauncher) -> i64 {
    launcher.started.borrow()[0].0.id
}

#[test]
fn a_crash_between_intent_and_dispatch_does_not_duplicate_the_launch() {
    let repo = repo();
    let store = open(&repo);
    started(&store, &repo, vec![scoped("a", &["src"], &[])]);
    // The first process persists the intent, then dies before dispatching.
    let first = coordinator("gpui-1");
    let tasks = store.tasks().unwrap();
    tasks
        .acquire_lease(&lease_resource("t"), "gpui-1", LEASE_TTL_MS, NOW)
        .unwrap();
    let subject = Subject::Assignment { task: "t", id: "a" };
    let ready = State::Assignment(AssignmentState::Ready);
    tasks
        .transition(subject, ready, Requester::Coordinator, 1, NOW)
        .unwrap();
    let task = tasks.task("t").unwrap().unwrap();
    let a = tasks.assignment("t", "a").unwrap().unwrap();
    let attempt = first.begin_launch(&store, &task, &a, NOW).unwrap().unwrap();
    assert_eq!(attempt.status, AttemptState::Starting);
    assert_eq!(status(&store, "a"), AssignmentState::Running);
    assert_eq!(attempt.session_id, None);

    // A new process cannot act while the dead one's lease is live.
    let second = coordinator("gpui-2");
    let launcher = FakeLauncher::default();
    let tick = second.tick(&store, &launcher, NOW + 1).unwrap();
    assert_eq!(tick.leased_elsewhere, vec!["t".to_owned()]);
    assert!(launcher.started.borrow().is_empty());

    // After expiry it reconciles the intent: one dispatch, no new attempt.
    let later = NOW + LEASE_TTL_MS + 1;
    let tick = second.tick(&store, &launcher, later).unwrap();
    assert!(tick.errors.is_empty(), "{tick:?}");
    assert_eq!(launcher.started.borrow().len(), 1);
    let all = attempts(&store, "a");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, attempt.id);
    assert_eq!(all[0].status, AttemptState::Starting);
    assert!(all[0].session_id.is_some());
    assert_eq!(store.sessions().unwrap().len(), 1);
    assert_eq!(store.tasks().unwrap().workspaces("t").unwrap().len(), 1);
    let lease = store
        .tasks()
        .unwrap()
        .lease(&lease_resource("t"))
        .unwrap()
        .unwrap();
    assert_eq!((lease.holder.as_str(), lease.generation), ("gpui-2", 2));
    second.tick(&store, &launcher, later + 1).unwrap();
    assert_eq!(launcher.started.borrow().len(), 1);
}

#[test]
fn a_retried_launch_after_dispatch_resolves_to_the_existing_session() {
    let repo = repo();
    let store = open(&repo);
    started(&store, &repo, vec![scoped("a", &["src"], &[])]);
    // The first process reserves the session, then dies before recording
    // the reservation on the attempt.
    let first = coordinator("gpui-1");
    let tasks = store.tasks().unwrap();
    let subject = Subject::Assignment { task: "t", id: "a" };
    let ready = State::Assignment(AssignmentState::Ready);
    tasks
        .transition(subject, ready, Requester::Coordinator, 1, NOW)
        .unwrap();
    let task = tasks.task("t").unwrap().unwrap();
    let a = tasks.assignment("t", "a").unwrap().unwrap();
    let attempt = first.begin_launch(&store, &task, &a, NOW).unwrap().unwrap();
    let (session, _, fresh) = crate::tasks::launch::reserve(&store, &task, &attempt).unwrap();
    assert!(fresh);
    assert_eq!(attempts(&store, "a")[0].session_id, None);

    let launcher = FakeLauncher::default();
    let later = NOW + LEASE_TTL_MS + 1;
    let tick = coordinator("gpui-2")
        .tick(&store, &launcher, later)
        .unwrap();
    assert!(tick.errors.is_empty(), "{tick:?}");
    assert_eq!(tick.bound, vec![attempt.id.clone()]);
    assert!(launcher.started.borrow().is_empty(), "never started twice");
    // Bound to the reserved session, but not started: still Starting.
    let bound = &attempts(&store, "a")[0];
    assert_eq!(bound.status, AttemptState::Starting);
    assert_eq!(
        bound.session_id.as_deref(),
        Some(store.persisted_session_id(session.id).unwrap().as_str())
    );
    assert!(events(&store, "attempt.started").is_empty());
    assert_eq!(store.sessions().unwrap().len(), 1);
}

#[test]
fn a_confirmed_start_moves_the_attempt_to_running_exactly_once() {
    let repo = repo();
    let store = open(&repo);
    started(&store, &repo, vec![scoped("a", &["src"], &[])]);
    let launcher = FakeLauncher::default();
    coordinator("gpui-1").tick(&store, &launcher, NOW).unwrap();
    let session = session_id(&launcher);
    // A worker cannot report before its start is confirmed.
    assert!(crate::app_access::worker_action(
        &store,
        session,
        "early",
        "tasks.worker.result",
        &json!({"outcome": "succeeded", "summary": "s"}),
        NOW,
    )
    .is_err());
    assert!(confirm_started(&store, session, NOW + 1).unwrap());
    assert!(!confirm_started(&store, session, NOW + 2).unwrap());
    assert_eq!(attempts(&store, "a")[0].status, AttemptState::Running);
    assert_eq!(events(&store, "attempt.started").len(), 1);
    assert_eq!(events(&store, "attempt.running").len(), 1);
    // A later failure report from the app cannot undo a confirmed start.
    start_failed(&store, session, "late", NOW + 3).unwrap();
    assert_eq!(attempts(&store, "a")[0].status, AttemptState::Running);
    // Sessions that are not task workers are ignored.
    let other = store.create("codex", "/elsewhere").unwrap().id;
    assert!(!confirm_started(&store, other, NOW + 4).unwrap());
}

#[test]
fn a_launch_the_app_cannot_start_is_recovered_with_an_event() {
    let repo = repo();
    let store = open(&repo);
    started(&store, &repo, vec![scoped("a", &["src"], &[])]);
    let launcher = FakeLauncher::default();
    let ours = coordinator("gpui-1");
    ours.tick(&store, &launcher, NOW).unwrap();
    // No window to start it in.
    start_failed(&store, session_id(&launcher), "No window", NOW + 1).unwrap();
    // Recovery (task-7) interrupts it and keeps its slot for a retry.
    let attempt = &attempts(&store, "a")[0];
    assert_eq!(attempt.status, AttemptState::Interrupted);
    assert_eq!(status(&store, "a"), AssignmentState::Running);
    let failed = events(&store, "attempt.start_failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].payload["reason"], "No window");
    let interrupted = events(&store, "attempt.interrupted");
    assert_eq!(interrupted[0].payload["requester"], "coordinator");
    ours.tick(&store, &launcher, NOW + 10).unwrap();
    assert_eq!(attempts(&store, "a").len(), 1);

    // Handing the session over can fail too (the app is closing).
    let repo = self::repo();
    let store = open(&repo);
    started(&store, &repo, vec![scoped("a", &["src"], &[])]);
    let refusing = FakeLauncher::default();
    refusing.refuse.set(true);
    let tick = ours.tick(&store, &refusing, NOW).unwrap();
    assert_eq!(tick.failed.len(), 1, "{tick:?}");
    assert_eq!(attempts(&store, "a")[0].status, AttemptState::Failed);
    assert_eq!(events(&store, "attempt.dispatch_failed").len(), 1);
}

#[test]
fn an_unconfirmed_reservation_is_recovered_after_the_timeout() {
    let repo = repo();
    let store = open(&repo);
    started(&store, &repo, vec![scoped("a", &["src"], &[])]);
    // The session is reserved and handed over, then the app dies before
    // installing it.
    let crashing = FakeLauncher::default();
    crashing.crash.set(true);
    let first = coordinator("gpui-1");
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        first.tick(&store, &crashing, NOW)
    }));
    assert!(crashed.is_err());
    let reserved = &attempts(&store, "a")[0];
    assert_eq!(reserved.status, AttemptState::Starting);
    assert!(reserved.session_id.is_some());

    let launcher = FakeLauncher::default();
    let second = coordinator("gpui-2");
    let later = NOW + LEASE_TTL_MS + 1;
    let tick = second.tick(&store, &launcher, later).unwrap();
    assert!(
        tick.failed.is_empty() && tick.launched.is_empty(),
        "{tick:?}"
    );
    assert_eq!(attempts(&store, "a")[0].status, AttemptState::Starting);
    let expired = NOW + LAUNCH_CONFIRM_TIMEOUT_MS;
    let tick = second.tick(&store, &launcher, expired).unwrap();
    assert_eq!(tick.failed, vec![reserved.id.clone()]);
    assert_eq!(attempts(&store, "a")[0].status, AttemptState::Interrupted);
    assert_eq!(status(&store, "a"), AssignmentState::Running);
    assert_eq!(events(&store, "launch.unconfirmed").len(), 1);
    assert!(launcher.started.borrow().is_empty());
    second.tick(&store, &launcher, expired + 1).unwrap();
    assert_eq!(events(&store, "launch.unconfirmed").len(), 1);
    assert_eq!(attempts(&store, "a").len(), 1);
}

#[test]
fn two_coordinators_cannot_both_hold_a_task_lease() {
    let repo = repo();
    let ours = open(&repo);
    // The competing process has its own connection to the same file.
    let theirs = Store::open(&repo.db).unwrap();
    started(&ours, &repo, vec![scoped("a", &["src"], &[])]);
    let (our_launcher, their_launcher) = (FakeLauncher::default(), FakeLauncher::default());
    let (a, b) = (coordinator("gpui-1"), coordinator("tauri-1"));
    assert_eq!(a.tick(&ours, &our_launcher, NOW).unwrap().launched.len(), 1);
    let tick = b.tick(&theirs, &their_launcher, NOW + 1).unwrap();
    assert_eq!(tick.leased_elsewhere, vec!["t".to_owned()]);
    assert!(their_launcher.started.borrow().is_empty());
    let held = theirs.tasks().unwrap().acquire_lease(
        &lease_resource("t"),
        "tauri-1",
        LEASE_TTL_MS,
        NOW + 2,
    );
    assert!(matches!(held, Err(TaskError::LeaseHeld { .. })), "{held:?}");
    // Renewal keeps the lease past the first expiry while we keep ticking.
    a.tick(&ours, &our_launcher, NOW + LEASE_TTL_MS - 1)
        .unwrap();
    let tick = b
        .tick(&theirs, &their_launcher, NOW + LEASE_TTL_MS + 1)
        .unwrap();
    assert_eq!(tick.leased_elsewhere, vec!["t".to_owned()]);
    // Once we stop, they take over with a new generation and launch nothing new.
    let tick = b
        .tick(&theirs, &their_launcher, NOW + 3 * LEASE_TTL_MS)
        .unwrap();
    assert!(tick.leased_elsewhere.is_empty());
    let lease = theirs
        .tasks()
        .unwrap()
        .lease(&lease_resource("t"))
        .unwrap()
        .unwrap();
    assert_eq!((lease.holder.as_str(), lease.generation), ("tauri-1", 2));
    let tick = a
        .tick(&ours, &our_launcher, NOW + 3 * LEASE_TTL_MS + 1)
        .unwrap();
    assert_eq!(tick.leased_elsewhere, vec!["t".to_owned()]);
    assert!(their_launcher.started.borrow().is_empty());
    assert_eq!(attempts(&ours, "a").len(), 1);
}

#[test]
fn a_dirty_checkout_is_left_intact_and_the_worktree_starts_from_head() {
    let repo = repo();
    std::fs::write(repo.root.join("src/lib.rs"), "pub fn edited() {}\n").unwrap();
    std::fs::write(repo.root.join("notes.txt"), "untracked\n").unwrap();
    let before = git(&repo.root, &["status", "--porcelain"]);
    let store = open(&repo);
    started(&store, &repo, vec![scoped("a", &["src"], &[])]);
    let launcher = FakeLauncher::default();
    let tick = coordinator("gpui-1").tick(&store, &launcher, NOW).unwrap();
    assert_eq!(tick.launched.len(), 1, "{tick:?}");
    // The user's checkout keeps its changes exactly.
    assert_eq!(git(&repo.root, &["status", "--porcelain"]), before);
    assert_eq!(
        std::fs::read_to_string(repo.root.join("src/lib.rs")).unwrap(),
        "pub fn edited() {}\n"
    );
    assert!(repo.root.join("notes.txt").is_file());
    // The worktree has HEAD's content and none of the dirty changes.
    let workspace = &store.tasks().unwrap().workspaces("t").unwrap()[0];
    let tree = Path::new(&workspace.path);
    assert_eq!(
        std::fs::read_to_string(tree.join("src/lib.rs")).unwrap(),
        "pub fn one() {}\n"
    );
    assert!(!tree.join("notes.txt").exists());
    assert_eq!(git(tree, &["status", "--porcelain"]), "");
    // Not silent: the intent says the base checkout had uncommitted changes.
    let intent = &events(&store, "attempt.launch_intent")[0];
    assert_eq!(intent.payload["dirtyBase"], true);
}

#[test]
fn investigation_reads_the_existing_checkout() {
    let repo = repo();
    let store = open(&repo);
    started(&store, &repo, vec![investigation("look")]);
    let launcher = FakeLauncher::default();
    let tick = coordinator("gpui-1").tick(&store, &launcher, NOW).unwrap();
    assert_eq!(tick.launched.len(), 1, "{tick:?}");
    let workspace = &store.tasks().unwrap().workspaces("t").unwrap()[0];
    assert_eq!(workspace.mode, WorkspaceMode::Checkout);
    assert_eq!(workspace.path, repo.root.to_string_lossy());
    assert_eq!(workspace.branch.as_deref(), Some("main"));
    assert_eq!(git(&repo.root, &["worktree", "list"]).lines().count(), 1);
    assert_eq!(launcher.started.borrow()[0].1.worktree_cwd, None);
    let brief = store
        .follow_ups(launcher.started.borrow()[0].0.id)
        .unwrap()
        .draft
        .unwrap()
        .text;
    assert!(brief.contains("the user's project checkout"), "{brief}");
    assert!(brief.contains("make no edits"), "{brief}");
    assert!(!brief.contains("Files you may change"), "{brief}");
    assert!(!brief.contains("worktree"), "{brief}");
}

#[test]
fn a_failed_claim_is_classified_by_the_coordinator_without_a_retry() {
    let repo = repo();
    let store = open(&repo);
    started(&store, &repo, vec![scoped("a", &["src"], &[])]);
    let launcher = FakeLauncher::default();
    let ours = coordinator("gpui-1");
    ours.tick(&store, &launcher, NOW).unwrap();
    confirm_started(&store, session_id(&launcher), NOW + 1).unwrap();
    report(&store, session_id(&launcher), "r1", "failed");
    // The claim alone does not fail the attempt; the coordinator does.
    assert_eq!(attempts(&store, "a")[0].status, AttemptState::Running);
    let tick = ours.tick(&store, &launcher, NOW + 10).unwrap();
    let attempt = &attempts(&store, "a")[0];
    assert_eq!(tick.failed, vec![attempt.id.clone()]);
    assert_eq!(attempt.status, AttemptState::Failed);
    assert_eq!(status(&store, "a"), AssignmentState::Failed);
    let failed = events(&store, "attempt.failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].payload["requester"], "coordinator");
    for later in 2..5 {
        ours.tick(&store, &launcher, NOW + 10 * later).unwrap();
    }
    assert_eq!(attempts(&store, "a").len(), 1, "retries belong to recovery");
    assert_eq!(launcher.started.borrow().len(), 1);
    assert_eq!(events(&store, "attempt.failed").len(), 1);
}

/// Allows everything the default policy refuses.
struct Automatic(usize);
impl Policy for Automatic {
    fn automatic_execution(&self, _: &crate::tasks::Task) -> bool {
        true
    }
    fn worker_limit(&self, _: &crate::tasks::Task) -> usize {
        self.0
    }
}

#[test]
fn only_explicitly_started_tasks_run_under_the_default_policy() {
    let repo = repo();
    let store = open(&repo);
    started(&store, &repo, vec![investigation("look")]);
    let tasks = store.tasks().unwrap();
    // Planned and Ready, but nobody started it.
    let t = Subject::Task("t");
    crate::tasks::store::tests::force(&store, t, State::Stage(TaskStage::Ready));
    let launcher = FakeLauncher::default();
    coordinator("gpui-1").tick(&store, &launcher, NOW).unwrap();
    assert_eq!(tasks.task("t").unwrap().unwrap().stage, TaskStage::Ready);
    assert!(launcher.started.borrow().is_empty());
    assert_eq!(
        ExplicitStart.worker_limit(&tasks.task("t").unwrap().unwrap()),
        1
    );
    let automatic = Coordinator::new("gpui-1", Automatic(1));
    automatic.tick(&store, &launcher, NOW + 1).unwrap();
    assert_eq!(
        tasks.task("t").unwrap().unwrap().stage,
        TaskStage::Implementing
    );
    assert_eq!(launcher.started.borrow().len(), 1);
}

#[test]
fn limits_dependencies_and_overlapping_scopes_gate_launches() {
    let repo = repo();
    let store = open(&repo);
    started(
        &store,
        &repo,
        vec![
            scoped("a", &["src"], &[]),
            scoped("b", &["src/lib.rs"], &[]),
            scoped("c", &["docs"], &[]),
            scoped("d", &["tests"], &["a"]),
        ],
    );
    let launcher = FakeLauncher::default();
    // The default policy runs one worker at a time.
    let tick = coordinator("gpui-1").tick(&store, &launcher, NOW).unwrap();
    assert_eq!(tick.launched.len(), 1, "{tick:?}");
    assert_eq!(status(&store, "a"), AssignmentState::Running);
    // With room for more, b still waits for a's overlapping scope and d for
    // its dependency.
    let wider = Coordinator::new("gpui-1", Automatic(4));
    let tick = wider.tick(&store, &launcher, NOW + 1).unwrap();
    assert_eq!(tick.launched.len(), 1, "{tick:?}");
    assert_eq!(status(&store, "b"), AssignmentState::Ready);
    assert_eq!(status(&store, "c"), AssignmentState::Running);
    assert_eq!(status(&store, "d"), AssignmentState::Queued);
    assert_eq!(launcher.started.borrow().len(), 2);
}

#[test]
fn paused_tasks_launch_nothing_and_unpreparable_workspaces_block_the_task() {
    let repo = repo();
    let store = open(&repo);
    started(&store, &repo, vec![scoped("a", &["src"], &[])]);
    let t = Subject::Task("t");
    crate::tasks::store::tests::force(&store, t, State::Lifecycle(TaskLifecycle::Paused));
    let launcher = FakeLauncher::default();
    coordinator("gpui-1").tick(&store, &launcher, NOW).unwrap();
    assert!(attempts(&store, "a").is_empty());
    assert!(store.tasks().unwrap().workspaces("t").unwrap().is_empty());

    // A worktree cannot be made outside a Git repository.
    crate::tasks::store::tests::force(&store, t, State::Lifecycle(TaskLifecycle::Active));
    let outside = repo.root.parent().unwrap().join("plain");
    std::fs::create_dir_all(&outside).unwrap();
    store
        .connection()
        .execute(
            "UPDATE native_tasks SET project=?1 WHERE id='t'",
            [outside.to_string_lossy()],
        )
        .unwrap();
    let tick = coordinator("gpui-1")
        .tick(&store, &launcher, NOW + 1)
        .unwrap();
    assert!(tick.launched.is_empty());
    let tasks = store.tasks().unwrap();
    assert_eq!(
        tasks.task("t").unwrap().unwrap().lifecycle,
        TaskLifecycle::Blocked
    );
    let reason = events(&store, "task.workspace_failed");
    assert_eq!(reason.len(), 1);
    assert!(reason[0].payload["reason"].as_str().unwrap().len() > 3);
    assert!(attempts(&store, "a").is_empty());
    assert!(launcher.started.borrow().is_empty());
}

#[test]
fn a_block_while_reconciling_launches_nothing_else_that_tick() {
    let repo = repo();
    let store = open(&repo);
    started(
        &store,
        &repo,
        vec![scoped("a", &["src"], &[]), scoped("b", &["docs"], &[])],
    );
    // An earlier run left a's launch intent unfinished, and the project has
    // since stopped being a repository.
    let tasks = store.tasks().unwrap();
    let subject = Subject::Assignment { task: "t", id: "a" };
    let ready = State::Assignment(AssignmentState::Ready);
    tasks
        .transition(subject, ready, Requester::Coordinator, 1, NOW)
        .unwrap();
    tasks
        .create_attempt(&crate::tasks::store::tests::new_attempt("x", "t", "a"), NOW)
        .unwrap();
    let outside = repo.root.parent().unwrap().join("plain");
    std::fs::create_dir_all(&outside).unwrap();
    store
        .connection()
        .execute(
            "UPDATE native_tasks SET project=?1 WHERE id='t'",
            [outside.to_string_lossy()],
        )
        .unwrap();
    let launcher = FakeLauncher::default();
    let tick = Coordinator::new("gpui-1", Automatic(4))
        .tick(&store, &launcher, NOW)
        .unwrap();
    assert!(tick.errors.is_empty(), "{tick:?}");
    assert_eq!(
        tasks.task("t").unwrap().unwrap().lifecycle,
        TaskLifecycle::Blocked
    );
    assert_eq!(events(&store, "task.workspace_failed").len(), 1);
    assert!(attempts(&store, "b").is_empty());
    assert_eq!(status(&store, "b"), AssignmentState::Queued);
    assert!(tasks.workspaces("t").unwrap().is_empty());
    assert!(launcher.started.borrow().is_empty());
}

#[test]
fn a_leftover_branch_is_adopted_with_its_commits() {
    let repo = repo();
    let main = git(&repo.root, &["rev-parse", "HEAD"]);
    // An earlier run's branch survives without its worktree.
    let branch = crate::tasks::workspaces::branch_for("t", "a");
    git(&repo.root, &["checkout", "-q", "-b", &branch]);
    std::fs::write(repo.root.join("src/earlier.rs"), "// kept\n").unwrap();
    git(&repo.root, &["add", "."]);
    git(&repo.root, &["commit", "-q", "-m", "earlier work"]);
    git(&repo.root, &["checkout", "-q", "main"]);
    let store = open(&repo);
    started(&store, &repo, vec![scoped("a", &["src"], &[])]);
    let launcher = FakeLauncher::default();
    let tick = coordinator("gpui-1").tick(&store, &launcher, NOW).unwrap();
    assert_eq!(tick.launched.len(), 1, "{tick:?}");
    let tasks = store.tasks().unwrap();
    assert_eq!(
        tasks.task("t").unwrap().unwrap().lifecycle,
        TaskLifecycle::Active
    );
    let workspace = &tasks.workspaces("t").unwrap()[0];
    assert_eq!(workspace.branch.as_deref(), Some(branch.as_str()));
    assert!(Path::new(&workspace.path).join("src/earlier.rs").is_file());
    assert_eq!(workspace.base_commit.as_deref(), Some(main.as_str()));
}
