//! A coordinator launch run end to end with the python app-server fixture:
//! the reserved session runs the worker brief in its own worktree, and the
//! worker's report reaches review durably.
use super::*;
use monocode_core::orchestration::Plan;
use monocode_core::tasks::coordinator::{Coordinator, ExplicitStart, FLAG, Launcher, REVIEW};
use monocode_core::tasks::{
    AcceptanceCriterion, Assignment, AssignmentState, NewTask, Requester, State, Subject, TaskStage,
};
use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

const PROVIDER: &str = r#"#!/usr/bin/env python3
import json, os, sys
def send(value):
    print(json.dumps(value), flush=True)
for line in sys.stdin:
    value = json.loads(line)
    method = value.get('method')
    if method == 'initialize':
        send({'id': value['id'], 'result': {}})
    elif method == 'thread/start':
        send({'id': value['id'], 'result': {'thread': {'id': 'thread'}}})
    elif method == 'turn/start':
        with open('WORKED.json', 'w') as out:
            json.dump({'cwd': os.getcwd(), 'params': value.get('params')}, out)
        send({'id': value['id'], 'result': {'turn': {'id': 'turn'}}})
        send({'method': 'item/agentMessage/delta', 'params': {'itemId': 'done', 'delta': 'Done'}})
        send({'method': 'turn/completed', 'params': {'turn': {'status': 'completed'}}})
        break
"#;

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
}

struct Fixture(std::path::PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Recorded(RefCell<Vec<(Session, Settings)>>);
impl Launcher for Recorded {
    fn start(&self, session: &Session, settings: &Settings) -> Result<(), String> {
        self.0
            .borrow_mut()
            .push((session.clone(), settings.clone()));
        Ok(())
    }
}

fn started_task(store: &Store, project: &Path) {
    let tasks = store.tasks().unwrap();
    tasks
        .create_task(
            &NewTask {
                id: "t".into(),
                mono_id: None,
                project: project.to_string_lossy().into_owned(),
                title: "Ship it".into(),
                request: "Implement the feature".into(),
                criteria: vec![AcceptanceCriterion {
                    id: "c1".into(),
                    description: "Tests pass".into(),
                    required: true,
                    checks: Vec::new(),
                }],
            },
            1,
        )
        .unwrap();
    let plan = Plan {
        title: "Plan".into(),
        summary: "Approach".into(),
        tasks: vec![Assignment {
            id: "a".into(),
            title: "Add the feature".into(),
            prompt: "Write src/feature.rs".into(),
            files: vec!["src".into()],
            verification: vec!["cargo test".into()],
            ..Assignment::default()
        }],
    };
    tasks.save_plan("t", 1, &plan, 1).unwrap();
    let t = Subject::Task("t");
    for stage in [TaskStage::Ready, TaskStage::Implementing] {
        let revision = tasks.revision(t).unwrap();
        tasks
            .transition(t, State::Stage(stage), Requester::Mono, revision, 1)
            .unwrap();
    }
}

#[test]
fn a_launched_worker_runs_its_brief_in_its_worktree_and_reports_durably() {
    let dir = Fixture(std::env::temp_dir().join(format!(
        "monocode-task-launch-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )));
    std::fs::create_dir_all(&dir.0).unwrap();
    let base = std::fs::canonicalize(&dir.0).unwrap();
    let project = base.join("project");
    std::fs::create_dir_all(project.join("src")).unwrap();
    git(&project, &["init", "-q", "-b", "main"]);
    git(&project, &["config", "user.email", "test@example.com"]);
    git(&project, &["config", "user.name", "Test"]);
    git(&project, &["config", "commit.gpgsign", "false"]);
    std::fs::write(project.join("src/lib.rs"), "pub fn one() {}\n").unwrap();
    git(&project, &["add", "."]);
    git(&project, &["commit", "-q", "-m", "init"]);
    let binary = base.join("mock-provider");
    std::fs::write(&binary, PROVIDER).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();

    let store = Store::open(&base.join("monocode.db")).unwrap();
    store.save_setting(FLAG, "enabled").unwrap();
    started_task(&store, &project);
    let store: Database = Arc::new(Mutex::new(store));
    let coordinator = Coordinator::new("gpui-test", ExplicitStart);
    let launcher = Recorded::default();
    let tick = coordinator
        .tick(&locked(&store).unwrap(), &launcher, 10)
        .unwrap();
    assert_eq!(tick.launched.len(), 1, "{tick:?}");
    let (session, settings) = launcher.0.borrow()[0].clone();
    // The app installed and submitted it.
    assert!(
        monocode_core::tasks::confirm_started(&locked(&store).unwrap(), session.id, 11).unwrap()
    );
    let worktree = settings.worktree_cwd.clone().unwrap();
    let brief = locked(&store)
        .unwrap()
        .follow_ups(session.id)
        .unwrap()
        .draft
        .unwrap()
        .text;

    // Run the launched session's first turn as the GUI would, with the fake
    // provider standing in for codex.
    let (updates, receive) = async_channel::bounded(64);
    let (_controls, control_rx) = std::sync::mpsc::channel();
    let worker_store = store.clone();
    let worker = std::thread::spawn(move || {
        run_turn_using(
            TurnInput {
                summary: session,
                prompt: brief,
                settings: Settings {
                    text_only: true,
                    ..settings
                },
            },
            control_rx,
            worker_store,
            Arc::new(AtomicBool::new(false)),
            updates,
            |_| Some(binary),
        );
    });
    let status = loop {
        let update = receive.recv_blocking().expect("runtime closed early");
        if update.finished {
            break update.status;
        }
    };
    worker.join().unwrap();
    assert_eq!(status, "Ready");
    let seen: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(Path::new(&worktree).join("WORKED.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(seen["cwd"], worktree.as_str());
    let params = seen["params"].to_string();
    for needle in ["Write src/feature.rs", "tasks.worker.result", "cargo test"] {
        assert!(
            params.contains(needle),
            "the provider did not get {needle}: {params}"
        );
    }
    assert!(
        !project.join("WORKED.json").exists(),
        "the main checkout is untouched"
    );

    // The worker reports through its bound session; review is requested once.
    let session_id = launcher.0.borrow()[0].0.id;
    let report = monocode_core::app_access::worker_action(
        &locked(&store).unwrap(),
        session_id,
        "r1",
        "tasks.worker.result",
        &serde_json::json!({"outcome": "succeeded", "summary": "Added the feature"}),
        20,
    )
    .unwrap();
    assert_eq!(report["assignment"]["status"], "needs_review");
    for now in [30, 40] {
        let tick = coordinator
            .tick(&locked(&store).unwrap(), &launcher, now)
            .unwrap();
        assert!(tick.launched.is_empty());
    }
    drop(store);
    let reopened = Store::open(&base.join("monocode.db")).unwrap();
    let tasks = reopened.tasks().unwrap();
    assert_eq!(
        tasks.assignment("t", "a").unwrap().unwrap().status,
        AssignmentState::NeedsReview
    );
    assert_eq!(tasks.pending_deliveries(REVIEW).unwrap().len(), 1);
    assert_eq!(launcher.0.borrow().len(), 1);
}

const FAILING_PROVIDER: &str = r#"#!/usr/bin/env python3
import json, sys
def send(value):
    print(json.dumps(value), flush=True)
for line in sys.stdin:
    value = json.loads(line)
    method = value.get('method')
    if method == 'initialize':
        send({'id': value['id'], 'result': {}})
    elif method == 'thread/start':
        send({'id': value['id'], 'result': {'thread': {'id': 'thread'}}})
    elif method == 'turn/start':
        send({'id': value['id'], 'result': {'turn': {'id': 'turn'}}})
        send({'method': 'turn/completed', 'params': {'turn': {'status': 'failed', 'error': {
            'message': 'Fixture subscription exhausted', 'codexErrorInfo': 'usageLimitExceeded'}}}})
        break
"#;

#[test]
fn a_worker_that_hits_its_usage_limit_continues_on_the_next_allowed_account() {
    use monocode_core::accounts::eligibility::{AllowedAccount, Allowlist, Scope, save_allowlist};
    use monocode_core::tasks::recovery::{self, Outcome};
    let dir = Fixture(std::env::temp_dir().join(format!(
        "monocode-task-recovery-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )));
    std::fs::create_dir_all(&dir.0).unwrap();
    let base = std::fs::canonicalize(&dir.0).unwrap();
    let project = base.join("project");
    std::fs::create_dir_all(project.join("src")).unwrap();
    git(&project, &["init", "-q", "-b", "main"]);
    git(&project, &["config", "user.email", "test@example.com"]);
    git(&project, &["config", "user.name", "Test"]);
    git(&project, &["config", "commit.gpgsign", "false"]);
    std::fs::write(project.join("src/lib.rs"), "pub fn one() {}\n").unwrap();
    git(&project, &["add", "."]);
    git(&project, &["commit", "-q", "-m", "init"]);
    let binary = base.join("failing-provider");
    std::fs::write(&binary, FAILING_PROVIDER).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();

    let store = Store::open(&base.join("monocode.db")).unwrap();
    store.save_setting(FLAG, "enabled").unwrap();
    // Hermetic: both accounts are profiles under the temp data dir, never
    // the real ~/.codex of the default account.
    let mut book = monocode_core::accounts::Book {
        accounts: ["account-a", "account-b"]
            .iter()
            .map(|id| monocode_core::accounts::Account {
                id: (*id).into(),
                provider: "codex".into(),
                label: (*id).into(),
            })
            .collect(),
        ..Default::default()
    };
    let project_key = project.to_string_lossy().into_owned();
    book.select(
        monocode_core::provider::Kind::Codex,
        &project_key,
        "account-a",
    )
    .unwrap();
    store.save_provider_accounts(&book).unwrap();
    let mut allowed = Allowlist::only("codex", "account-a");
    allowed.accounts.push(AllowedAccount {
        priority: 1,
        ..AllowedAccount::new("codex", "account-b")
    });
    save_allowlist(&store, Scope::Project(&project_key), &allowed).unwrap();
    started_task(&store, &project);
    let store: Database = Arc::new(Mutex::new(store));
    let coordinator = Coordinator::new("gpui-test", ExplicitStart);
    let launcher = Recorded::default();
    coordinator
        .tick(&locked(&store).unwrap(), &launcher, 10)
        .unwrap();
    let (session, settings) = launcher.0.borrow()[0].clone();
    assert_eq!(settings.account.id, "account-a");
    assert!(
        settings
            .account
            .home(monocode_core::provider::Kind::Codex)
            .unwrap()
            .starts_with(&base)
    );
    monocode_core::tasks::confirm_started(&locked(&store).unwrap(), session.id, 11).unwrap();
    let brief = locked(&store)
        .unwrap()
        .follow_ups(session.id)
        .unwrap()
        .draft
        .unwrap()
        .text;

    // The fake provider fails the worker's turn with a usage limit.
    let (updates, receive) = async_channel::bounded(64);
    let (_controls, control_rx) = std::sync::mpsc::channel();
    let worker_store = store.clone();
    let id = session.id;
    let worker = std::thread::spawn(move || {
        run_turn_using(
            TurnInput {
                summary: session,
                prompt: brief,
                settings: Settings {
                    text_only: true,
                    ..settings
                },
            },
            control_rx,
            worker_store,
            Arc::new(AtomicBool::new(false)),
            updates,
            |_| Some(binary),
        );
    });
    let finished = loop {
        let update = receive.recv_blocking().expect("runtime closed early");
        if update.finished {
            break update;
        }
    };
    worker.join().unwrap();
    assert!(
        finished.status.starts_with("Usage limit reached"),
        "{}",
        finished.status
    );

    // What the conversation reports when the turn finishes.
    let end = crate::conversation::task_recovery::turn_end(
        &finished.status,
        false,
        finished.usage_limit.as_ref(),
    );
    let outcome = recovery::turn_ended(&locked(&store).unwrap(), id, &end, 20).unwrap();
    assert!(
        matches!(&outcome, Outcome::Retry { account, .. } if account == "account-b"),
        "{outcome:?}"
    );
    let stops = recovery::pending_stops(&locked(&store).unwrap()).unwrap();
    assert_eq!(stops.len(), 1);
    for stop in stops {
        recovery::stopped(&locked(&store).unwrap(), &stop.delivery, 21).unwrap();
    }
    coordinator
        .tick(&locked(&store).unwrap(), &launcher, 22)
        .unwrap();
    assert_eq!(launcher.0.borrow().len(), 2);
    let (replacement, settings) = launcher.0.borrow()[1].clone();
    assert_eq!(settings.account.id, "account-b");
    let brief = locked(&store)
        .unwrap()
        .follow_ups(replacement.id)
        .unwrap()
        .draft
        .unwrap()
        .text;
    assert!(brief.contains("Hand-off from attempt"), "{brief}");
    assert!(brief.contains("Write src/feature.rs"));
}
