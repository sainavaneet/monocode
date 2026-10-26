//! Integration's guards: authority checked again after the unlocked merge,
//! commit authority for corrections, repeated failures handed to Mono, the
//! user's checkout never adopted, and a leftover branch recorded as it is.
use super::tests::{commit, integration_tree, ok, part, started, Live, Run};
use super::*;
use crate::tasks::review::tests::{events, git, repo, Repo, NOW};
use crate::tasks::{AssignmentState, DecisionState, ReviewOutcome, TaskStage};

/// Two independent parts, both committed and accepted, at Integrating.
fn accepted(repo: &Repo) -> Run {
    let parts = vec![
        part("a", &["src/a.rs"], &[], "test -f src/a.rs"),
        part("b", &["src/b.rs"], &[], "test -f src/b.rs"),
    ];
    let run = started(repo, Live::merging(), parts, &["test -f src/a.rs"]);
    assert_eq!(run.tick(NOW).launched.len(), 2);
    for (id, at) in [("a", NOW + 10), ("b", NOW + 20)] {
        let file = format!("src/{id}.rs");
        let reviews = run.work(id, at, |tree| commit(tree, &file, "pub fn part() {}\n"));
        assert_eq!(reviews[0].outcome, ReviewOutcome::Accepted, "{reviews:?}");
    }
    assert!(run
        .coordinator
        .verify(&run.store, NOW + 30)
        .unwrap()
        .is_empty());
    assert_eq!(run.task().stage, TaskStage::Integrating);
    run
}

fn decision(run: &Run, n: u32) -> Option<crate::tasks::DecisionRequest> {
    let tasks = run.store.tasks().unwrap();
    tasks.decision(&format!("integration:t:{n}")).unwrap()
}

#[test]
fn a_conflict_without_commit_authority_raises_a_decision() {
    let repo = repo();
    let run = super::tests::conflicting(&repo, Live::merging());
    // The worker would have to commit its rebase; the policy no longer
    // allows commits.
    run.coordinator.policy().0.borrow_mut().commit = false;
    let records = run.integrate(NOW + 50);
    assert!(
        matches!(records[0].outcome, Outcome::Conflict { .. }),
        "{records:?}"
    );
    assert_eq!(run.status("b"), AssignmentState::Accepted);
    assert_eq!(run.task().stage, TaskStage::Integrating);
    let raised = decision(&run, 1).expect("a decision for Mono");
    assert_eq!(raised.state, DecisionState::Open);
    assert_eq!(raised.assignment_id.as_deref(), Some("b"));
    assert!(raised.context.contains("commit"), "{raised:?}");
}

#[test]
fn merge_authority_revoked_while_merging_stops_the_result_at_a_decision() {
    let repo = repo();
    let run = accepted(&repo);
    let (jobs, errors) = run
        .coordinator
        .integration_jobs(&run.store, NOW + 40)
        .unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    // The merge runs without the store lock; authority is revoked meanwhile.
    let record = jobs[0].run();
    assert_eq!(record.outcome, Outcome::Clean);
    run.coordinator.policy().0.borrow_mut().merge = false;
    let recorded = run
        .coordinator
        .record_integration(&run.store, &jobs[0], record, NOW + 41)
        .unwrap();
    assert!(recorded.is_some());
    assert_eq!(run.task().stage, TaskStage::Integrating);
    assert_eq!(events(&run.store, INTEGRATION).len(), 1);
    let raised = decision(&run, 1).expect("a decision for Mono");
    assert!(raised.question.contains("revoked"), "{raised:?}");
}

#[test]
fn repeated_failures_go_to_mono_and_the_users_checkout_is_never_adopted() {
    let repo = repo();
    let run = accepted(&repo);
    // The user has the integration branch checked out in their own
    // checkout: it is never taken over as the integration worktree.
    let base = git(&repo.root, &["rev-parse", "HEAD"]);
    git(&repo.root, &["checkout", "-q", "-b", &branch_for("t")]);
    for (pass, at) in [(1, NOW + 40), (2, NOW + 41)] {
        let records = run.integrate(at);
        assert!(
            matches!(records[0].outcome, Outcome::Failed { .. }),
            "{records:?}"
        );
        assert!(decision(&run, 1).is_none(), "pass {pass}");
    }
    run.integrate(NOW + 42);
    let raised = decision(&run, 1).expect("a decision after three failures");
    assert!(raised.question.contains("failed 3 times"), "{raised:?}");
    // The user's checkout was not touched: same commit, nothing merged.
    assert_eq!(git(&repo.root, &["rev-parse", "HEAD"]), base);
    assert_eq!(git(&repo.root, &["status", "--porcelain"]), "");
    let tasks = run.store.tasks().unwrap();
    assert!(tasks
        .workspaces("t")
        .unwrap()
        .iter()
        .all(|w| w.id != workspace_id("t")));
    // Waiting for Mono: no further passes until it answers.
    assert!(run.integrate(NOW + 43).is_empty());
}

#[test]
fn a_leftover_integration_branch_is_recorded_as_it_is() {
    let repo = repo();
    let run = accepted(&repo);
    let base = git(&repo.root, &["rev-parse", "HEAD"]);
    // A crash left the integration branch behind, already holding a's work.
    let a_tip = git(&repo.root, &["rev-parse", "mono/t/a"]);
    git(&repo.root, &["branch", &branch_for("t"), &a_tip]);
    let records = run.integrate(NOW + 40);
    let record = &records[0];
    assert_eq!(record.outcome, Outcome::Clean, "{record:?}");
    assert!(record.adopted);
    assert_eq!(record.base, base);
    assert_eq!(record.existing, std::slice::from_ref(&a_tip));
    let merged: Vec<(&str, bool)> = record
        .merges
        .iter()
        .map(|m| (m.assignment.as_str(), m.merged))
        .collect();
    assert_eq!(merged, [("a", false), ("b", true)]);
    let tree = integration_tree(&run);
    assert!(ok(&tree, &["merge-base", "--is-ancestor", &a_tip, "HEAD"]));
    let tasks = run.store.tasks().unwrap();
    let recorded = tasks
        .workspaces("t")
        .unwrap()
        .into_iter()
        .find(|w| w.id == workspace_id("t"))
        .unwrap();
    assert_eq!(recorded.base_commit.as_deref(), Some(record.base.as_str()));
}

#[test]
fn partly_committed_accepted_work_is_sent_back_instead_of_merged_without_its_rest() {
    let repo = repo();
    let parts = vec![
        part("a", &["src"], &[], "test -f src/a.rs"),
        part("b", &["docs"], &[], "test -f README.md"),
    ];
    let run = started(&repo, Live::merging(), parts, &["test -f src/a.rs"]);
    assert_eq!(run.tick(NOW).launched.len(), 2);
    // a commits one file and leaves another uncommitted; review accepted
    // the whole worktree.
    let reviews = run.work("a", NOW + 10, |tree| {
        commit(tree, "src/a.rs", "pub fn a() {}\n");
        std::fs::write(tree.join("src/a_more.rs"), "pub fn more() {}\n").unwrap();
    });
    assert_eq!(reviews[0].outcome, ReviewOutcome::Accepted, "{reviews:?}");
    run.work("b", NOW + 20, |tree| {
        std::fs::create_dir_all(tree.join("docs")).unwrap();
        commit(tree, "docs/b.md", "# b\n");
    });
    assert!(run
        .coordinator
        .verify(&run.store, NOW + 30)
        .unwrap()
        .is_empty());
    let records = run.integrate(NOW + 40);
    assert!(
        matches!(&records[0].outcome, Outcome::Uncommitted { assignment, files, .. }
            if assignment == "a" && files == &["src/a_more.rs".to_owned()]),
        "{records:?}"
    );
    assert_eq!(run.status("a"), AssignmentState::Queued);
    assert_ne!(run.task().stage, TaskStage::Verifying);
}

#[test]
fn an_assignment_that_changed_nothing_is_skipped_without_a_correction() {
    let repo = repo();
    let parts = vec![
        part("a", &["src/a.rs"], &[], "test -f src/a.rs"),
        // An investigation-like part with nothing to change.
        part("b", &["docs"], &[], "test -f README.md"),
    ];
    let run = started(&repo, Live::merging(), parts, &["test -f src/a.rs"]);
    assert_eq!(run.tick(NOW).launched.len(), 2);
    run.work("a", NOW + 10, |tree| {
        commit(tree, "src/a.rs", "pub fn a() {}\n")
    });
    let reviews = run.work("b", NOW + 20, |_| {});
    assert_eq!(reviews[0].outcome, ReviewOutcome::Accepted, "{reviews:?}");
    assert!(run
        .coordinator
        .verify(&run.store, NOW + 30)
        .unwrap()
        .is_empty());
    let records = run.integrate(NOW + 40);
    assert_eq!(records[0].outcome, Outcome::Clean, "{records:?}");
    let merged: Vec<(&str, bool)> = records[0]
        .merges
        .iter()
        .map(|m| (m.assignment.as_str(), m.merged))
        .collect();
    assert_eq!(merged, [("a", true), ("b", false)]);
    assert_eq!(run.status("b"), AssignmentState::Accepted);
    assert_eq!(run.task().stage, TaskStage::Verifying);
}

#[test]
fn a_clean_integration_older_than_an_acceptance_is_not_verified() {
    let repo = repo();
    let run = accepted(&repo);
    let records = run.integrate(NOW + 40);
    assert_eq!(records[0].outcome, Outcome::Clean);
    // b is accepted again after that integration (Mono re-judged it).
    let tasks = run.store.tasks().unwrap();
    tasks
        .append_event(
            &crate::tasks::NewEvent {
                id: "reaccepted-b".into(),
                task_id: "t".into(),
                assignment_id: Some("b".into()),
                attempt_id: None,
                kind: "assignment.accepted".into(),
                payload: serde_json::json!({}),
            },
            NOW + 41,
        )
        .unwrap();
    let rounds = run.coordinator.verify(&run.store, NOW + 50).unwrap();
    assert!(rounds.is_empty(), "{rounds:?}");
    let refused = events(&run.store, "task.needs_integration");
    assert_eq!(refused.len(), 1);
    assert!(refused[0]
        .payload
        .to_string()
        .contains("without a clean integration"));
}
