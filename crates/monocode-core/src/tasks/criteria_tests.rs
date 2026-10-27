//! A criterion with several checks is satisfied only when every one of them
//! passed on one snapshot; the precondition, the next round's pending
//! criteria and the final report apply that one rule.
use crate::store::Store;
use crate::tasks::completion::{complete, final_report};
use crate::tasks::review::tests::{claimed_with, events, feature, open, repo, scoped, CHECK, NOW};
use crate::tasks::review::REVIEWED;
use crate::tasks::store::tests::{assignment, force, memory, new_task, plan};
use crate::tasks::{
    AcceptanceCriterion, AssignmentState, NewEvent, Precondition, Review, ReviewOutcome,
    RoundOutcome, State, Subject, TaskError, TaskStage, Verification,
};

const FIRST: &str = "first-check";
const SECOND: &str = "second-check";

/// Task `t` in Verifying, its one assignment accepted, with criterion `c1`
/// demonstrated by two checks.
fn verifying() -> Store {
    let store = memory();
    let tasks = store.tasks().unwrap();
    let mut task = new_task("t");
    task.criteria = vec![AcceptanceCriterion {
        id: "c1".into(),
        description: "Both checks pass".into(),
        required: true,
        checks: vec![FIRST.into(), SECOND.into()],
    }];
    tasks.create_task(&task, 1).unwrap();
    tasks
        .save_plan("t", 1, &plan(vec![assignment("a", &[])]), 1)
        .unwrap();
    let accepted = State::Assignment(AssignmentState::Accepted);
    force(&store, Subject::Assignment { task: "t", id: "a" }, accepted);
    force(
        &store,
        Subject::Task("t"),
        State::Stage(TaskStage::Verifying),
    );
    store
}

fn record(store: &Store, id: &str, check: &str, checkpoint: &str, passed: bool) {
    store
        .tasks()
        .unwrap()
        .record_verification(&Verification {
            id: id.into(),
            task_id: "t".into(),
            assignment_id: Some("a".into()),
            criterion_id: Some("c1".into()),
            checkpoint_commit: checkpoint.into(),
            check: check.into(),
            passed,
            evidence: "{}".into(),
            invalidated: false,
            created_at: 2,
        })
        .unwrap();
}

fn satisfied(store: &Store) -> bool {
    let report = final_report(store, "t", 3).unwrap();
    let reported = report.criteria[0].satisfied;
    let completed = match complete(store, "t", crate::tasks::Requester::Coordinator, 3) {
        Ok(_) => true,
        Err(TaskError::Precondition(Precondition::CriteriaSatisfied)) => false,
        Err(other) => panic!("{other:?}"),
    };
    assert_eq!(reported, completed, "report and precondition disagree");
    completed
}

#[test]
fn a_failing_first_check_is_not_hidden_by_a_passing_second() {
    let store = verifying();
    record(&store, "v1", FIRST, "snap-1", false);
    record(&store, "v2", SECOND, "snap-1", true);
    assert!(!satisfied(&store));
}

#[test]
fn a_check_that_never_ran_leaves_its_criterion_unsatisfied() {
    let store = verifying();
    // The first check could not run, so only the second was recorded.
    record(&store, "v2", SECOND, "snap-1", true);
    assert!(!satisfied(&store));
}

#[test]
fn checks_that_passed_on_different_snapshots_do_not_combine() {
    let store = verifying();
    record(&store, "v1", FIRST, "snap-1", true);
    record(&store, "v2", SECOND, "snap-2", true);
    assert!(!satisfied(&store));
}

#[test]
fn every_check_passing_on_one_snapshot_satisfies_the_criterion() {
    let store = verifying();
    record(&store, "v1", FIRST, "snap-1", true);
    record(&store, "v2", SECOND, "snap-1", true);
    assert!(satisfied(&store));
}

#[test]
fn a_failed_check_keeps_the_task_open_after_the_round_changes() {
    let repo = repo();
    let store = open(&repo);
    let failing = "grep -q fixed src/feature.rs";
    let run = claimed_with(&store, &repo, scoped(&[CHECK]), &[failing, CHECK], feature);
    run.coordinator.review(&store, NOW + 10).unwrap();
    // The assignment was already sent back the maximum number of times, so
    // the failing round waits for Mono instead.
    let tasks = store.tasks().unwrap();
    for n in 0..crate::tasks::review::MAX_CORRECTIONS {
        let earlier = Review {
            attempt_id: format!("earlier-{n}"),
            assignment_id: "a".into(),
            outcome: ReviewOutcome::Correction,
            checkpoint: None,
            changed_files: Vec::new(),
            findings: Vec::new(),
            verifications: Vec::new(),
        };
        let event = NewEvent {
            id: format!("reviewed:earlier-{n}"),
            task_id: "t".into(),
            assignment_id: Some("a".into()),
            attempt_id: None,
            kind: REVIEWED.into(),
            payload: serde_json::to_value(&earlier).unwrap(),
        };
        tasks.append_event(&event, NOW + 11).unwrap();
    }
    let rounds = run.coordinator.verify(&store, NOW + 20).unwrap();
    assert_eq!(rounds[0].outcome, RoundOutcome::NeedsMonoReview);
    // Something unrelated changes the round's key; the failing check still
    // stands, so the next round neither completes nor skips the criterion.
    tasks
        .record_verification(&Verification {
            id: "unrelated".into(),
            task_id: "t".into(),
            assignment_id: Some("a".into()),
            criterion_id: None,
            checkpoint_commit: "x".into(),
            check: "true".into(),
            passed: true,
            evidence: "{}".into(),
            invalidated: false,
            created_at: NOW + 21,
        })
        .unwrap();
    let rounds = run.coordinator.verify(&store, NOW + 22).unwrap();
    assert!(
        rounds.iter().all(|r| r.outcome != RoundOutcome::Completed),
        "{rounds:?}"
    );
    let task = tasks.task("t").unwrap().unwrap();
    assert_eq!(task.stage, TaskStage::Verifying);
    assert!(events(&store, "task.final_report").is_empty());
}

#[test]
fn monos_own_checks_are_confined_like_the_plans() {
    let repo = repo();
    let store = open(&repo);
    claimed_with(&store, &repo, scoped(&[CHECK]), &[CHECK], feature);
    let workspace = store.tasks().unwrap().workspaces("t").unwrap().remove(0);
    for refused in [
        "git push origin main",
        "cat /etc/hosts",
        "git -C .. status",
        "sh -c true",
        "python3 -c print(1)",
        "git config core.hooksPath hooks",
    ] {
        let run = crate::tasks::completion::verify_criterion(
            &store,
            "t",
            "c1",
            &workspace,
            refused,
            NOW + 5,
        );
        assert!(run.is_err(), "{refused}: {run:?}");
    }
    let tasks = store.tasks().unwrap();
    assert!(tasks
        .verifications("t")
        .unwrap()
        .iter()
        .all(|v| v.criterion_id.is_none()));
}
