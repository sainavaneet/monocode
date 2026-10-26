//! Parallel scheduling from the persisted model: workspace ownership, file
//! scopes, the concurrency cap, workspaces kept across account switches and
//! unblocking only onto worktrees that are still there.
use super::*;
use crate::orchestration::WorkspaceMode;
use crate::store::Store;
use crate::tasks::recovery::tests::{accounts, allow, attempts, launched, stop_all, Recorded};
use crate::tasks::recovery::{turn_ended, Outcome, TurnEnd};
use crate::tasks::review::tests::{events, git, open, repo, FakeLauncher, Repo, NOW};
use crate::tasks::store::tests::{assignment, plan};
use crate::tasks::{confirm_started, NewTask, Workspace};
use serde_json::json;

struct Limit(usize);
impl Policy for Limit {
    fn worker_limit(&self, _task: &Task) -> usize {
        self.0
    }
}

fn part(id: &str, files: &[&str]) -> crate::tasks::Assignment {
    crate::tasks::Assignment {
        files: files.iter().map(|f| (*f).to_owned()).collect(),
        ..assignment(id, &[])
    }
}

/// Task `id` in `repo` with `parts`, started by Mono.
fn start(store: &Store, repo: &Repo, id: &str, parts: Vec<crate::tasks::Assignment>) {
    let tasks = store.tasks().unwrap();
    tasks
        .create_task(
            &NewTask {
                id: id.into(),
                mono_id: Some("mono-1".into()),
                project: repo.root.to_string_lossy().into_owned(),
                title: "Ship it".into(),
                request: "Do the parts".into(),
                criteria: vec![],
            },
            NOW,
        )
        .unwrap();
    tasks.save_plan(id, 1, &plan(parts), NOW).unwrap();
    for stage in [TaskStage::Ready, TaskStage::Implementing] {
        let revision = tasks.revision(Subject::Task(id)).unwrap();
        tasks
            .transition(
                Subject::Task(id),
                State::Stage(stage),
                Requester::Mono,
                revision,
                NOW,
            )
            .unwrap();
    }
}

fn running(store: &Store, task: &str) -> Vec<String> {
    let tasks = store.tasks().unwrap();
    tasks
        .assignments(task)
        .unwrap()
        .into_iter()
        .filter(|a| a.status == AssignmentState::Running)
        .map(|a| a.id)
        .collect()
}

/// `assignment`'s worker starts and claims success.
fn finish(store: &Store, launcher: &FakeLauncher, task: &str, assignment: &str, now: u64) {
    let tasks = store.tasks().unwrap();
    let attempt = tasks.attempts(task, assignment).unwrap().pop().unwrap();
    let bound = attempt.session_id.unwrap();
    let session = launcher
        .started
        .borrow()
        .iter()
        .find(|s| store.persisted_session_id(s.id).unwrap() == bound)
        .unwrap()
        .id;
    // Already confirmed when a test kept the worker running a while.
    confirm_started(store, session, now).unwrap();
    crate::app_access::worker_action(
        store,
        session,
        &format!("done-{assignment}"),
        "tasks.worker.result",
        &json!({"outcome": "succeeded", "summary": "Done", "validation": []}),
        now + 1,
    )
    .unwrap();
    crate::tasks::review::tests::ack_stops(store);
}

#[test]
fn two_workers_never_own_one_mutable_workspace() {
    let repo = repo();
    let store = open(&repo);
    let coordinator = Coordinator::new("gpui-1", Limit(2));
    let launcher = FakeLauncher::default();
    start(&store, &repo, "t1", vec![part("a", &["src/a.rs"])]);
    coordinator.tick(&store, &launcher, NOW).unwrap();
    assert_eq!(running(&store, "t1"), ["a"]);
    let tasks = store.tasks().unwrap();
    let owned = tasks.workspaces("t1").unwrap().pop().unwrap();
    let a = tasks
        .attempts("t1", "a")
        .unwrap()
        .pop()
        .unwrap()
        .session_id
        .unwrap();
    let session = launcher.started.borrow()[0].id;
    assert_eq!(store.persisted_session_id(session).unwrap(), a);
    assert!(confirm_started(&store, session, NOW + 1).unwrap());
    // Another task's assignment is recorded in that same worktree (an
    // adopted or hand-edited record, spelled differently): it may not start
    // beside a's worker.
    start(&store, &repo, "t2", vec![part("b", &["src/b.rs"])]);
    let alias = format!("{}-alias", owned.path);
    std::os::unix::fs::symlink(&owned.path, &alias).unwrap();
    tasks
        .save_workspace(&Workspace {
            id: "t2/b".into(),
            task_id: "t2".into(),
            assignment_id: Some("b".into()),
            path: alias,
            ..owned.clone()
        })
        .unwrap();
    for n in 0..3 {
        let tick = coordinator.tick(&store, &launcher, NOW + 10 + n).unwrap();
        assert!(tick.errors.is_empty(), "{tick:?}");
    }
    assert!(running(&store, "t2").is_empty());
    assert!(tasks.attempts("t2", "b").unwrap().is_empty());
    assert!(events(&store, "workspace.owned_elsewhere").is_empty());
    // Waiting past the notice interval tells Mono once.
    let late = NOW + 10 + crate::tasks::guard::OWNED_NOTICE_MS;
    for n in 0..3 {
        coordinator.tick(&store, &launcher, late + n).unwrap();
    }
    assert_eq!(events(&store, "workspace.owned_elsewhere").len(), 1);
    assert!(running(&store, "t2").is_empty());
    // Once a's worker is done with the worktree, b may have it.
    finish(&store, &launcher, "t1", "a", late + 10);
    coordinator.tick(&store, &launcher, late + 20).unwrap();
    // b's launch now goes ahead (its hand-edited path spelling is then
    // refused by the launch's own worktree check, as any unlisted path is).
    assert_eq!(tasks.attempts("t2", "b").unwrap().len(), 1);
    assert_eq!(owned.mode, WorkspaceMode::Worktree);
}

#[test]
fn overlapping_assignments_serialize() {
    let repo = repo();
    let store = open(&repo);
    let coordinator = Coordinator::new("gpui-1", Limit(4));
    let launcher = FakeLauncher::default();
    start(
        &store,
        &repo,
        "t",
        vec![part("a", &["src"]), part("b", &["src/lib.rs"])],
    );
    for n in 0..3 {
        coordinator.tick(&store, &launcher, NOW + n).unwrap();
        assert_eq!(running(&store, "t"), ["a"], "b waits for a's scope");
    }
    finish(&store, &launcher, "t", "a", NOW + 10);
    coordinator.tick(&store, &launcher, NOW + 20).unwrap();
    assert_eq!(running(&store, "t"), ["b"]);
}

#[test]
fn independent_assignments_run_concurrently_up_to_the_cap() {
    let repo = repo();
    let store = open(&repo);
    let coordinator = Coordinator::new("gpui-1", Limit(2));
    let launcher = FakeLauncher::default();
    let parts = vec![
        part("a", &["src/a.rs"]),
        part("b", &["docs"]),
        part("c", &["tests"]),
    ];
    start(&store, &repo, "t", parts);
    let tick = coordinator.tick(&store, &launcher, NOW).unwrap();
    assert_eq!(tick.launched.len(), 2);
    assert_eq!(running(&store, "t"), ["a", "b"]);
    coordinator.tick(&store, &launcher, NOW + 1).unwrap();
    assert_eq!(running(&store, "t").len(), 2, "never above the cap");
    finish(&store, &launcher, "t", "a", NOW + 10);
    coordinator.tick(&store, &launcher, NOW + 20).unwrap();
    assert_eq!(running(&store, "t"), ["b", "c"]);
    // Each runs in its own worktree on its own branch.
    let workspaces = store.tasks().unwrap().workspaces("t").unwrap();
    let mut paths: Vec<&str> = workspaces.iter().map(|w| w.path.as_str()).collect();
    paths.dedup();
    assert_eq!(paths.len(), 3);
}

#[test]
fn a_replacement_after_an_account_switch_keeps_the_tasks_branch_and_worktree() {
    let repo = repo();
    let store = open(&repo);
    accounts(&store, &["account-b"]);
    allow(&store, &repo, &[("default", None), ("account-b", None)]);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    let (_, first) = launcher.last();
    let end = TurnEnd {
        error: Some("Usage limit reached".into()),
        usage_limited: true,
        resets_at: Some(NOW + 3_600_000),
        ..TurnEnd::default()
    };
    let outcome = turn_ended(&store, session, &end, NOW + 5).unwrap();
    assert!(matches!(outcome, Outcome::Retry { .. }), "{outcome:?}");
    stop_all(&store, NOW + 6);
    crate::tasks::recovery::tests::coordinator()
        .tick(&store, &launcher, NOW + 6)
        .unwrap();
    let (_, second) = launcher.last();
    assert_eq!(second.account.id, "account-b");
    assert_eq!(attempts(&store).len(), 2);
    // The replacement works in the task's recorded worktree and branch.
    assert_eq!(second.worktree_cwd, first.worktree_cwd);
    let workspaces = store.tasks().unwrap().workspaces("t").unwrap();
    assert_eq!(workspaces.len(), 1);
    let intents = events(&store, "attempt.launch_intent");
    assert_eq!(intents.len(), 2);
    assert_eq!(
        intents[0].payload["workspace"],
        intents[1].payload["workspace"]
    );
    assert_eq!(
        intents[1].payload["workspace"]["branch"],
        crate::tasks::workspaces::branch_for("t", "a")
    );
    let tree = std::path::Path::new(&workspaces[0].path);
    assert_eq!(
        git(tree, &["symbolic-ref", "--short", "HEAD"]),
        crate::tasks::workspaces::branch_for("t", "a")
    );
}

#[test]
fn unblocking_rechecks_recorded_worktrees() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    launched(&store, &repo, &launcher);
    let tasks = store.tasks().unwrap();
    let t = Subject::Task("t");
    let blocked = State::Lifecycle(TaskLifecycle::Blocked);
    let revision = tasks.revision(t).unwrap();
    tasks
        .transition(t, blocked, Requester::Mono, revision, NOW + 1)
        .unwrap();
    // The worktree disappears while the task is blocked.
    let tree = tasks.workspaces("t").unwrap().pop().unwrap().path;
    std::fs::rename(&tree, format!("{tree}-moved")).unwrap();
    let revision = tasks.revision(t).unwrap();
    let refused = tasks.unblock("t", revision, Requester::User, NOW + 2);
    let Err(TaskError::Invalid(reason)) = refused else {
        panic!("{refused:?}");
    };
    assert!(reason.contains(&tree), "{reason}");
    let task = tasks.task("t").unwrap().unwrap();
    assert_eq!(task.lifecycle, TaskLifecycle::Blocked);
    // Restored, it unblocks.
    std::fs::rename(format!("{tree}-moved"), &tree).unwrap();
    let revision = tasks.revision(t).unwrap();
    // The app checks Git between two short store locks: anything that
    // changed meanwhile refuses the unblock rather than trusting the check.
    let check = tasks.unblock_check("t").unwrap();
    let problems = check.problems();
    assert!(problems.is_empty());
    let mut moved = tasks.workspaces("t").unwrap().pop().unwrap();
    moved.id = "t/extra".into();
    moved.assignment_id = None;
    tasks.save_workspace(&moved).unwrap();
    let changed = tasks.unblock_checked(&check, &problems, revision, Requester::User, NOW + 3);
    assert!(
        matches!(&changed, Err(TaskError::Invalid(m)) if m.contains("changed")),
        "{changed:?}"
    );
    let check = tasks.unblock_check("t").unwrap();
    let problems = check.problems();
    let stale = tasks.unblock_checked(&check, &problems, revision - 1, Requester::User, NOW + 3);
    assert!(matches!(stale, Err(TaskError::Stale { .. })), "{stale:?}");
    tasks
        .unblock_checked(&check, &problems, revision, Requester::User, NOW + 3)
        .unwrap();
    assert_eq!(
        tasks.task("t").unwrap().unwrap().lifecycle,
        TaskLifecycle::Active
    );
}

#[test]
fn a_plan_the_user_starts_becomes_a_task_the_coordinator_schedules() {
    let repo = repo();
    let store = open(&repo);
    let plan = crate::orchestration::Plan {
        title: "Two parts".into(),
        summary: String::new(),
        tasks: vec![
            crate::tasks::Assignment {
                verification: vec!["test -f src/a.rs".into()],
                ..part("a", &["src/a.rs"])
            },
            crate::tasks::Assignment {
                verification: Vec::new(),
                ..assignment("b", &["a"])
            },
        ],
    };
    let new = NewTask {
        id: "plan-1".into(),
        mono_id: None,
        project: repo.root.to_string_lossy().into_owned(),
        title: "Two parts".into(),
        request: "Two parts".into(),
        criteria: crate::tasks::policy::plan_criteria(&plan),
    };
    let task = crate::tasks::policy::start_plan(&store, &new, &plan, "codex", Requester::User, NOW)
        .unwrap();
    assert_eq!(
        (task.stage, task.lifecycle),
        (TaskStage::Implementing, TaskLifecycle::Active)
    );
    // Each assignment's checks become a criterion run on the result.
    assert_eq!(task.criteria.len(), 1);
    assert_eq!(task.criteria[0].checks, ["test -f src/a.rs"]);
    let tasks = store.tasks().unwrap();
    assert!(crate::tasks::policy::of(&tasks, "plan-1")
        .unwrap()
        .is_some());
    // The coordinator schedules it: a now, b once a is accepted.
    let launcher = FakeLauncher::default();
    let tick = crate::tasks::recovery::tests::coordinator()
        .tick(&store, &launcher, NOW + 1)
        .unwrap();
    assert_eq!(tick.launched.len(), 1, "{tick:?}");
    assert_eq!(running(&store, "plan-1"), ["a"]);
    // An invalid plan creates nothing.
    let mut bad = plan.clone();
    bad.tasks[1].depends_on = vec!["missing".into()];
    let refused = crate::tasks::policy::start_plan(
        &store,
        &NewTask {
            id: "plan-2".into(),
            ..new
        },
        &bad,
        "codex",
        Requester::User,
        NOW,
    );
    assert!(matches!(refused, Err(TaskError::Invalid(_))), "{refused:?}");
    assert!(tasks.task("plan-2").unwrap().is_none());
    // A start that fails part-way (here at the policy snapshot) leaves
    // nothing behind: the task, its plan and its stages are one transaction.
    let failed = crate::tasks::policy::start_plan(
        &store,
        &NewTask {
            id: "plan-3".into(),
            mono_id: None,
            project: repo.root.to_string_lossy().into_owned(),
            title: "Broken".into(),
            request: "Broken".into(),
            criteria: Vec::new(),
        },
        &plan,
        "no-such-provider",
        Requester::User,
        NOW,
    );
    assert!(failed.is_err(), "{failed:?}");
    assert!(tasks.task("plan-3").unwrap().is_none());
    assert!(tasks.assignments("plan-3").unwrap().is_empty());
}
