//! The coordinator's locked verification steps run no Git: snapshotting,
//! refreshing evidence and reading branch facts all happen in `inspect`.
use super::*;
use crate::git::calls;
use crate::tasks::review::tests::{claimed_with, feature, git, open, repo, scoped, CHECK, NOW};

#[test]
fn preparing_and_recording_a_round_run_no_git() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed_with(&store, &repo, scoped(&[CHECK]), &[CHECK], |tree| {
        feature(tree);
        git(tree, &["add", "."]);
        git(tree, &["commit", "-q", "-m", "feature"]);
    });
    run.coordinator.review(&store, NOW + 10).unwrap();
    let before = calls();
    let (jobs, errors) = run.coordinator.verify_jobs(&store, NOW + 20).unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(calls(), before, "verify_jobs ran Git under the store");
    let inspection = jobs[0].inspect();
    let before = calls();
    let round = run
        .coordinator
        .record_round(&store, &jobs[0], inspection, NOW + 21)
        .unwrap()
        .unwrap();
    assert_eq!(calls(), before, "record_round ran Git under the store");
    assert_eq!(round.outcome, RoundOutcome::Completed, "{round:?}");
    let report = round.report.unwrap();
    assert!(
        report.committed && report.criteria[0].satisfied,
        "{report:?}"
    );
}

#[test]
fn a_waiting_round_still_notices_stale_evidence_without_git_under_the_store() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed_with(&store, &repo, scoped(&[CHECK]), &[], feature);
    run.coordinator.review(&store, NOW + 10).unwrap();
    let rounds = run.coordinator.verify(&store, NOW + 20).unwrap();
    assert_eq!(rounds[0].outcome, RoundOutcome::NeedsMonoReview);
    std::fs::write(run.tree.join("src/feature.rs"), "pub fn changed() {}\n").unwrap();
    let before = calls();
    let (jobs, _) = run.coordinator.verify_jobs(&store, NOW + 21).unwrap();
    assert_eq!(calls(), before);
    let inspection = jobs[0].inspect();
    let before = calls();
    let round = run
        .coordinator
        .record_round(&store, &jobs[0], inspection, NOW + 22)
        .unwrap();
    assert_eq!(calls(), before);
    assert_eq!(round.unwrap().outcome, RoundOutcome::NeedsMonoReview);
    let live = store.tasks().unwrap().verifications("t").unwrap();
    assert!(live.iter().all(|v| v.invalidated), "{live:?}");
}
