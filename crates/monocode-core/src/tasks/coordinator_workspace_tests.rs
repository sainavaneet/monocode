//! Where the coordinator launches workers: a worktree from HEAD that leaves
//! a dirty checkout alone, the checkout itself for an investigation, and an
//! earlier run's branch adopted with its commits.
use super::tests::{
    coordinator, events, git, investigation, open, repo, scoped, started, FakeLauncher, NOW,
};
use crate::orchestration::WorkspaceMode;
use crate::tasks::TaskLifecycle;
use std::path::Path;

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
fn investigation_refuses_a_different_explicit_base_without_changing_checkout() {
    let repo = repo();
    let base = git(&repo.root, &["rev-parse", "HEAD"]);
    git(&repo.root, &["commit", "--allow-empty", "-m", "new tip"]);
    let tip = git(&repo.root, &["rev-parse", "HEAD"]);
    let store = open(&repo);
    let mut assignment = investigation("look");
    assignment.base_ref = Some(base);
    started(&store, &repo, vec![assignment]);
    let launcher = FakeLauncher::default();
    let tick = coordinator("gpui-1").tick(&store, &launcher, NOW).unwrap();
    assert!(tick.launched.is_empty());
    assert!(store.tasks().unwrap().workspaces("t").unwrap().is_empty());
    assert_eq!(git(&repo.root, &["rev-parse", "HEAD"]), tip);
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

#[test]
fn explicit_repositories_route_workers_and_preserve_base_after_store_reopen() {
    struct Parallel;
    impl super::Policy for Parallel {
        fn worker_limit(&self, _: &crate::tasks::Task) -> usize {
            2
        }
    }

    let mut repo = repo();
    let parent = repo.root.parent().unwrap().to_path_buf();
    git(
        &parent,
        &["clone", "-q", repo.root.to_str().unwrap(), "second"],
    );
    let base = git(&repo.root, &["rev-parse", "HEAD"]);
    repo.root = parent;
    let store = open(&repo);
    let mut a = scoped("a", &["src"], &[]);
    a.repository = Some("project".into());
    a.base_ref = Some(base.clone());
    let mut b = scoped("b", &["src"], &[]);
    b.repository = Some("second".into());
    b.base_ref = Some(base.clone());
    started(&store, &repo, vec![a, b]);
    drop(store);
    let store = open(&repo);
    let launcher = FakeLauncher::default();
    let tick = super::Coordinator::new("gpui-1", Parallel)
        .tick(&store, &launcher, NOW)
        .unwrap();
    assert_eq!(tick.launched.len(), 2, "{tick:?}");
    let workspaces = store.tasks().unwrap().workspaces("t").unwrap();
    for (id, name) in [("a", "project"), ("b", "second")] {
        let w = workspaces
            .iter()
            .find(|w| w.assignment_id.as_deref() == Some(id))
            .unwrap();
        assert_eq!(w.repository, repo.root.join(name).to_string_lossy());
        assert_eq!(w.base_commit.as_deref(), Some(base.as_str()));
        assert!(Path::new(&w.path).join("src/lib.rs").exists());
    }
    let launched = launcher.started.borrow();
    assert!(launched
        .iter()
        .any(|(session, _)| session.cwd == repo.root.join("project").to_string_lossy()));
    assert!(launched
        .iter()
        .any(|(session, _)| session.cwd == repo.root.join("second").to_string_lossy()));
}

#[test]
fn repository_escape_and_legacy_prompt_mismatch_create_no_workspaces() {
    for explicit in [true, false] {
        let repo = repo();
        let other = super::tests::repo();
        let store = open(&repo);
        let mut a = scoped("a", &["src"], &[]);
        if explicit {
            a.repository = Some(other.root.to_string_lossy().into_owned());
        } else {
            a.prompt = format!("Repo {}. Implement the feature.", other.root.display());
        }
        started(&store, &repo, vec![a]);
        let launcher = FakeLauncher::default();
        coordinator("gpui-1").tick(&store, &launcher, NOW).unwrap();
        assert!(launcher.started.borrow().is_empty());
        assert!(store.tasks().unwrap().workspaces("t").unwrap().is_empty());
        assert_eq!(
            store.tasks().unwrap().task("t").unwrap().unwrap().lifecycle,
            TaskLifecycle::Blocked
        );
    }
}

#[test]
fn unavailable_account_blocks_before_worktree_creation() {
    let repo = repo();
    let store = open(&repo);
    started(&store, &repo, vec![scoped("a", &["src"], &[])]);
    crate::accounts::eligibility::mark_unhealthy(&store, "codex", "default", NOW).unwrap();
    crate::accounts::eligibility::mark_unhealthy(&store, "claude", "default", NOW).unwrap();
    let launcher = FakeLauncher::default();
    coordinator("gpui-1").tick(&store, &launcher, NOW).unwrap();
    assert!(launcher.started.borrow().is_empty());
    assert!(store.tasks().unwrap().workspaces("t").unwrap().is_empty());
}

#[test]
fn successors_do_not_launch_with_missing_dependency_commits() {
    let repo = repo();
    let store = open(&repo);
    started(
        &store,
        &repo,
        vec![scoped("a", &["src"], &[]), scoped("b", &["tests"], &["a"])],
    );
    let tasks = store.tasks().unwrap();
    let task = tasks.task("t").unwrap().unwrap();
    let a = tasks.assignment("t", "a").unwrap().unwrap();
    let mut b = tasks.assignment("t", "b").unwrap().unwrap();
    b.repository = Some(".".into());
    let source = crate::tasks::workspaces::prepare(&tasks, &task, &a, NOW).unwrap();
    let path = Path::new(&source.workspace.path);
    std::fs::write(path.join("src/lib.rs"), "pub fn dependency() {}\n").unwrap();
    git(path, &["add", "src/lib.rs"]);
    git(path, &["commit", "-qm", "dependency result"]);
    assert!(
        matches!(crate::tasks::workspaces::prepare(&tasks,&task,&b,NOW+1),Err(crate::tasks::workspaces::PrepareError::Workspace(message)) if message.contains("stale inputs"))
    );
    assert_eq!(tasks.workspaces("t").unwrap().len(), 1);
    let mut reviewed = b;
    reviewed.base_ref = Some(git(path, &["rev-parse", "HEAD"]));
    let ready = crate::tasks::workspaces::prepare(&tasks, &task, &reviewed, NOW + 2).unwrap();
    assert_eq!(
        std::fs::read_to_string(Path::new(&ready.workspace.path).join("src/lib.rs")).unwrap(),
        "pub fn dependency() {}\n"
    );
}

/// The worktree checkout runs before the tick holds the store: the
/// candidates are read without Git, the warm-up creates the worktree, and
/// the locked launch adopts it instead of checking out again.
#[test]
fn a_warmed_up_worktree_is_adopted_by_the_locked_launch_within_the_worker_limit() {
    use crate::git::calls;
    use crate::tasks::workspaces::warm_up;
    let repo = repo();
    let store = open(&repo);
    started(
        &store,
        &repo,
        vec![scoped("a", &["src"], &[]), scoped("b", &["README.md"], &[])],
    );
    let coordinator = coordinator("gpui-1");
    let before = calls();
    let jobs = coordinator.launch_candidates(&store, NOW).unwrap();
    assert_eq!(calls(), before, "launch_candidates ran Git under the store");
    // One worker at a time (the default policy): one warm-up, not two.
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0].assignment.id, "a");
    assert!(!jobs[0].recorded);
    warm_up(&jobs[0], NOW).unwrap();
    let listed = git(&repo.root, &["worktree", "list"]);
    assert_eq!(listed.lines().count(), 2, "{listed}");
    let warmed = listed
        .lines()
        .find(|l| l.contains("mono/t/a"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    // A second warm-up is a no-op, and the tick adopts the worktree.
    warm_up(&jobs[0], NOW + 1).unwrap();
    let launcher = FakeLauncher::default();
    let tick = coordinator.tick(&store, &launcher, NOW + 2).unwrap();
    assert_eq!(tick.launched.len(), 1, "{tick:?}");
    let workspace = &store.tasks().unwrap().workspaces("t").unwrap()[0];
    assert_eq!(
        Path::new(&workspace.path).canonicalize().unwrap(),
        Path::new(&warmed).canonicalize().unwrap()
    );
    assert_eq!(git(&repo.root, &["worktree", "list"]).lines().count(), 2);
    // Now that a is running, nothing more fits the limit.
    assert!(coordinator
        .launch_candidates(&store, NOW + 3)
        .unwrap()
        .is_empty());
}
