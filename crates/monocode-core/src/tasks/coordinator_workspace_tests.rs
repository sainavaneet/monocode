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
