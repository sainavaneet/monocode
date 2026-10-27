//! A declared check that cannot run can be waived by Mono's judgement, with
//! a reason; a check that ran and failed cannot.
use crate::store::Store;
use crate::tasks::coordinator::FLAG;
use crate::tasks::review::tests::{claimed_with, events, feature, open, repo, scoped, CHECK, NOW};
use crate::tasks::store::tests::{assignment, force, memory, new_task, plan};
use crate::tasks::{
    AcceptanceCriterion, AssignmentState, RoundOutcome, State, Subject, TaskStage, Verification,
};
use serde_json::json;

/// A step no program can run: review and verification hand it to Mono.
const PROSE: &str = "Check the dashboard by hand";

fn waive(store: &Store, check: &str, reason: &str) -> Result<serde_json::Value, String> {
    crate::app_access::mono_task_action(
        store,
        "mono-1",
        "tasks.criteria.waive",
        &json!({"taskId": "t", "criterionId": "c1", "check": check, "reason": reason}),
        NOW + 30,
    )
}

#[test]
fn an_unrunnable_check_waived_with_a_reason_completes_the_task() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed_with(&store, &repo, scoped(&[CHECK]), &[CHECK, PROSE], feature);
    run.coordinator.review(&store, NOW + 10).unwrap();
    let rounds = run.coordinator.verify(&store, NOW + 20).unwrap();
    assert_eq!(
        rounds[0].outcome,
        RoundOutcome::NeedsMonoReview,
        "{rounds:?}"
    );
    // Mono is told it may waive the check.
    let waiting = events(&store, "task.verification_waiting");
    assert!(
        waiting[0]
            .payload
            .to_string()
            .contains("tasks.criteria.waive"),
        "{:?}",
        waiting[0].payload
    );
    waive(&store, PROSE, "Checked the dashboard myself").unwrap();
    let rounds = run.coordinator.verify(&store, NOW + 31).unwrap();
    assert_eq!(rounds[0].outcome, RoundOutcome::Completed, "{rounds:?}");
    let report = rounds[0].report.as_ref().unwrap();
    assert!(report.criteria[0].satisfied);
    let shown = report.criteria[0].waived.join("; ");
    assert!(
        shown.contains("waived: Checked the dashboard myself"),
        "{shown}"
    );
    assert_eq!(events(&store, "verification.waived").len(), 1);
    // The monitor's timeline shows it too.
    let all = store.tasks().unwrap().events_after(0, 10_000).unwrap();
    let names = crate::tasks::timeline::Names::default();
    let timeline = crate::tasks::timeline::entries(&all, &names);
    let shown: Vec<&str> = timeline.iter().map(|e| e.text.as_str()).collect();
    assert!(
        shown
            .iter()
            .any(|t| t.contains("waived: Checked the dashboard myself")),
        "{shown:?}"
    );
}

/// Task `t` in Verifying with criterion `c1` checked by [`PROSE`] alone.
fn verifying() -> Store {
    let store = memory();
    store.save_setting(FLAG, "enabled").unwrap();
    let tasks = store.tasks().unwrap();
    let mut task = new_task("t");
    task.criteria = vec![AcceptanceCriterion {
        id: "c1".into(),
        description: "Looks right".into(),
        required: true,
        checks: vec![PROSE.into()],
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

fn recorded(store: &Store, evidence: serde_json::Value, passed: bool) {
    store
        .tasks()
        .unwrap()
        .record_verification(&Verification {
            id: "v1".into(),
            task_id: "t".into(),
            assignment_id: Some("a".into()),
            criterion_id: Some("c1".into()),
            checkpoint_commit: "snap-1".into(),
            check: PROSE.into(),
            passed,
            evidence: evidence.to_string(),
            invalidated: false,
            created_at: 2,
        })
        .unwrap();
}

#[test]
fn waiving_needs_a_reason() {
    let store = verifying();
    recorded(
        &store,
        json!({"unrunnable": "It is not a command line."}),
        false,
    );
    let refused = waive(&store, PROSE, "   ").unwrap_err();
    assert!(refused.contains("reason"), "{refused}");
    assert_eq!(store.tasks().unwrap().verifications("t").unwrap().len(), 1);
}

#[test]
fn a_check_that_ran_and_failed_cannot_be_waived() {
    let store = verifying();
    recorded(&store, json!({"argv": ["false"], "exitCode": 1}), false);
    let refused = waive(&store, PROSE, "It is fine").unwrap_err();
    assert!(refused.contains("ran and failed"), "{refused}");
    // A check never tried cannot be waived either.
    let refused = waive(&store, "other-check", "It is fine").unwrap_err();
    assert!(refused.contains("not a check of"), "{refused}");
    assert_eq!(store.tasks().unwrap().verifications("t").unwrap().len(), 1);
}
