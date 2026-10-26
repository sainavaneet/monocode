//! Review's guards: checks that change the workspace, programs outside it,
//! snapshot ref names, file scopes, retries, and what Mono's send-backs
//! carry into the next brief.
use super::tests::{claimed, events, feature, git, open, repo, scoped, status, CHECK, NOW};
use super::*;
use crate::tasks::findings::{out_of_scope, ref_part, task_ref};
use crate::tasks::{checks, AssignmentState, State, Subject};
use std::time::Duration;

#[test]
fn a_check_that_changes_the_workspace_is_a_correction_without_evidence() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed(
        &store,
        &repo,
        scoped(&[CHECK, "touch src/generated.rs"]),
        feature,
    );
    let reviews = run.coordinator.review(&store, NOW + 10).unwrap();
    let review = &reviews[0];
    assert_eq!(review.outcome, ReviewOutcome::Correction, "{review:?}");
    assert!(review.findings.contains(&Finding::ModifiedByCheck {
        files: vec!["src/generated.rs".into()]
    }));
    // No verification claims to describe the snapshot the checks started from.
    assert!(review.verifications.is_empty());
    assert!(store
        .tasks()
        .unwrap()
        .verifications("t")
        .unwrap()
        .is_empty());
    assert_eq!(status(&store), AssignmentState::Queued);
}

#[test]
fn programs_are_found_on_path_or_inside_the_workspace_only() {
    let base = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(base.path()).unwrap();
    let workspace = root.join("workspace");
    let program = "#!/bin/sh\nexit 0\n";
    for dir in [workspace.join("tools"), root.join("outside")] {
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ok");
        std::fs::write(&path, program).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let run = |check: &str| checks::run(check, &workspace, Duration::from_secs(30));
    assert!(run("tools/ok").unwrap().passed());
    assert!(run("./tools/ok").unwrap().passed());
    assert!(run("true").unwrap().passed());
    let outside = root.join("outside/ok");
    for refused in [
        "../outside/ok",
        "tools/../../outside/ok",
        &outside.to_string_lossy(),
    ] {
        let error = run(refused).unwrap_err();
        assert!(
            error.contains("outside the workspace"),
            "{refused}: {error}"
        );
    }
    assert!(run("tools/missing").is_err());
}

#[test]
fn snapshot_refs_never_collide_and_are_valid() {
    assert_ne!(ref_part("a.1"), ref_part("a-1"));
    assert_ne!(ref_part("a.1"), ref_part("a_2E1"));
    assert_ne!(
        task_ref("a.1", &["attempts", "x", "y"]),
        task_ref("a-1", &["attempts", "x", "y"])
    );
    // Fixed sub-namespaces keep an assignment named `criteria` apart.
    assert_ne!(
        task_ref("t", &["attempts", "criteria", "x"]),
        task_ref("t", &["criteria", "x"])
    );
    let dir = tempfile::tempdir().unwrap();
    for id in [
        "", "a.lock", "x/../y", "~^:?*[\\", "naïve", ".hidden", "a b",
    ] {
        let reference = task_ref(id, &["attempts", id, "before"]);
        let ok = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["check-ref-format", &reference])
            .status()
            .unwrap()
            .success();
        assert!(ok, "{id:?} gave an invalid ref {reference}");
    }
}

#[test]
fn scopes_are_normalised_and_patterns_go_to_mono() {
    let changed = vec!["src/a.rs".to_owned(), "README.md".to_owned()];
    for scope in ["src", "./src", "src/", "./src/"] {
        let outside = out_of_scope(&changed, &[scope.to_owned()]).unwrap();
        assert_eq!(outside, vec!["README.md".to_owned()], "{scope}");
    }
    for whole in [".", "./", ""] {
        assert!(out_of_scope(&changed, &[whole.to_owned()])
            .unwrap()
            .is_empty());
    }
    assert!(out_of_scope(&changed, &["src/*.rs".to_owned()]).is_err());
    assert!(out_of_scope(&["srcx/a.rs".to_owned()], &["src".to_owned()])
        .unwrap()
        .contains(&"srcx/a.rs".to_owned()));
}

#[test]
fn a_mono_send_back_carries_the_review_findings_into_the_next_brief() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed(&store, &repo, scoped(&[]), feature);
    let reviews = run.coordinator.review(&store, NOW + 10).unwrap();
    assert_eq!(reviews[0].outcome, ReviewOutcome::NeedsMonoReview);
    // Mono judges it and sends it back.
    let tasks = store.tasks().unwrap();
    let a = Subject::Assignment { task: "t", id: "a" };
    let queued = State::Assignment(AssignmentState::Queued);
    let revision = tasks.revision(a).unwrap();
    tasks
        .transition(a, queued, Requester::Mono, revision, NOW + 11)
        .unwrap();
    run.coordinator
        .tick(&store, &run.launcher, NOW + 12)
        .unwrap();
    let session = run.launcher.started.borrow()[1].id;
    let brief = store.follow_ups(session).unwrap().draft.unwrap().text;
    for needle in ["Mono sent back the previous attempt", "no check to verify"] {
        assert!(brief.contains(needle), "brief lacks {needle}: {brief}");
    }
}

#[test]
fn repeated_review_failures_hand_the_claim_to_mono() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed(&store, &repo, scoped(&[CHECK]), feature);
    for n in 0..REVIEW_RETRY_LIMIT {
        let jobs = run.coordinator.review_jobs(&store, NOW + 10).unwrap();
        assert_eq!(jobs.len(), 1);
        let error = format!("database is locked ({n})");
        run.coordinator
            .review_failed(&store, &jobs[0], &error, NOW + 10)
            .unwrap();
    }
    let reviews = run.coordinator.review(&store, NOW + 20).unwrap();
    let review = &reviews[0];
    assert_eq!(review.outcome, ReviewOutcome::NeedsMonoReview);
    assert!(review.findings.iter().any(|f| matches!(
        f,
        Finding::Workspace { reason } if reason.contains("database is locked (2)")
    )));
    assert_eq!(events(&store, "assignment.reviewed").len(), 1);
}

#[test]
fn a_worktree_without_a_base_commit_is_not_scoped_against_head() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed(&store, &repo, scoped(&[CHECK]), |tree| {
        feature(tree);
        std::fs::write(tree.join("README.md"), "# Changed\n").unwrap();
        git(tree, &["add", "."]);
        git(tree, &["commit", "-q", "-m", "escape"]);
    });
    store
        .connection()
        .execute("UPDATE native_task_workspaces SET base_commit=NULL", [])
        .unwrap();
    let reviews = run.coordinator.review(&store, NOW + 10).unwrap();
    let review = &reviews[0];
    assert_eq!(review.outcome, ReviewOutcome::NeedsMonoReview);
    assert!(review
        .findings
        .iter()
        .any(|f| matches!(f, Finding::Workspace { reason } if reason.contains("base commit"))));
    assert!(store
        .tasks()
        .unwrap()
        .verifications("t")
        .unwrap()
        .is_empty());
}
