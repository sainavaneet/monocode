//! From accepted work to a completed task through the coordinator alone:
//! stage driving, criterion rounds, corrections and Mono's seam.
use super::*;
use crate::store::Store;
use crate::tasks::completion::verify_criterion;
use crate::tasks::confirm_started;
use crate::tasks::coordinator::{Coordinator, ExplicitStart};
use crate::tasks::review::tests::{
    claimed_with, events, feature, git, open, repo, scoped, status, CHECK, NOW,
};
use crate::tasks::review::{MONO_REVIEW, REVIEW_RETRY_LIMIT};
use crate::tasks::store::tests::{assignment, force, memory, new_task, plan};
use serde_json::json;

fn task(store: &Store) -> Task {
    store.tasks().unwrap().task("t").unwrap().unwrap()
}

fn report(store: &Store, session: i64, request: &str, now: u64) {
    crate::app_access::worker_action(
        store,
        session,
        request,
        "tasks.worker.result",
        &json!({"outcome": "succeeded", "summary": "Done", "validation": [CHECK]}),
        now,
    )
    .unwrap();
}

#[test]
fn a_task_completes_through_review_and_verification_alone() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed_with(&store, &repo, scoped(&[CHECK]), &[CHECK], |tree| {
        feature(tree);
        git(tree, &["add", "."]);
        git(tree, &["commit", "-q", "-m", "feature"]);
    });
    run.coordinator.review(&store, NOW + 10).unwrap();
    assert_eq!(status(&store), AssignmentState::Accepted);
    let rounds = run.coordinator.verify(&store, NOW + 20).unwrap();
    assert_eq!(rounds.len(), 1);
    assert_eq!(rounds[0].outcome, RoundOutcome::Completed, "{rounds:?}");
    let report = rounds[0].report.as_ref().unwrap();
    assert!(report.implemented && report.committed && !report.merged && !report.deployed);
    assert!(report.criteria[0].satisfied);
    let t = task(&store);
    assert_eq!(
        (t.stage, t.lifecycle),
        (TaskStage::Completed, TaskLifecycle::Completed)
    );
    // The stages were driven by the coordinator through the table.
    for kind in [
        "task.reviewing",
        "task.integrating",
        "task.verifying",
        "task.verified",
    ] {
        let moved = events(&store, kind);
        assert_eq!(moved.len(), 1, "{kind}");
        assert_eq!(moved[0].payload["requester"], "coordinator");
    }
    let tasks = store.tasks().unwrap();
    let criterion = tasks
        .verifications("t")
        .unwrap()
        .into_iter()
        .find(|v| v.criterion_id.as_deref() == Some("c1"))
        .unwrap();
    assert!(criterion.passed && !criterion.checkpoint_commit.is_empty());
    assert_eq!(events(&store, "task.final_report").len(), 1);
    assert!(run.coordinator.verify(&store, NOW + 30).unwrap().is_empty());
}

#[test]
fn a_failing_criterion_sends_the_work_back_and_the_fix_completes() {
    let repo = repo();
    let store = open(&repo);
    let criterion = "grep -q fixed src/feature.rs";
    let run = claimed_with(&store, &repo, scoped(&[CHECK]), &[criterion], feature);
    run.coordinator.review(&store, NOW + 10).unwrap();
    assert_eq!(status(&store), AssignmentState::Accepted);

    let rounds = run.coordinator.verify(&store, NOW + 20).unwrap();
    assert_eq!(rounds[0].outcome, RoundOutcome::Correction, "{rounds:?}");
    assert!(matches!(
        &rounds[0].findings[0],
        Finding::CriterionNotMet { criterion: c, exit_code: Some(1), .. } if c == "c1"
    ));
    assert_eq!(status(&store), AssignmentState::Queued);
    assert_eq!(task(&store).stage, TaskStage::Implementing);
    let tasks = store.tasks().unwrap();
    let correction = tasks.latest_correction("t", "a").unwrap().unwrap();
    assert_eq!(correction.outcome, ReviewOutcome::Correction);

    // The coordinator relaunches with the finding in the brief.
    let tick = run
        .coordinator
        .tick(&store, &run.launcher, NOW + 30)
        .unwrap();
    assert_eq!(tick.launched.len(), 1, "{tick:?}");
    let session = run.launcher.started.borrow()[1].id;
    let brief = store.follow_ups(session).unwrap().draft.unwrap().text;
    assert!(brief.contains("Acceptance criterion c1"), "{brief}");
    assert!(confirm_started(&store, session, NOW + 31).unwrap());

    // The fixing attempt is reviewed, then verified, and the task completes.
    std::fs::write(
        run.tree.join("src/feature.rs"),
        "pub fn feature() {} // fixed\n",
    )
    .unwrap();
    report(&store, session, "r2", NOW + 32);
    let tick = run
        .coordinator
        .tick(&store, &run.launcher, NOW + 33)
        .unwrap();
    assert_eq!(tick.reviews.len(), 1, "{tick:?}");
    let reviews = run.coordinator.review(&store, NOW + 34).unwrap();
    assert_eq!(reviews[0].outcome, ReviewOutcome::Accepted, "{reviews:?}");
    let rounds = run.coordinator.verify(&store, NOW + 35).unwrap();
    assert_eq!(rounds[0].outcome, RoundOutcome::Completed, "{rounds:?}");
    assert_eq!(task(&store).stage, TaskStage::Completed);
    let report = rounds[0].report.as_ref().unwrap();
    assert!(report.criteria[0].satisfied);
}

#[test]
fn a_criterion_without_checks_waits_for_mono() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed_with(&store, &repo, scoped(&[CHECK]), &[], feature);
    run.coordinator.review(&store, NOW + 10).unwrap();
    let rounds = run.coordinator.verify(&store, NOW + 20).unwrap();
    assert_eq!(rounds[0].outcome, RoundOutcome::NeedsMonoReview);
    assert!(
        matches!(&rounds[0].findings[0], Finding::NotChecked { reason } if reason.contains("c1"))
    );
    let t = task(&store);
    assert_eq!(
        (t.stage, t.lifecycle),
        (TaskStage::Verifying, TaskLifecycle::Active)
    );
    let tasks = store.tasks().unwrap();
    assert_eq!(tasks.pending_deliveries(MONO_REVIEW).unwrap().len(), 1);
    // Nothing changed, so the round is not repeated.
    assert!(run.coordinator.verify(&store, NOW + 21).unwrap().is_empty());

    // Mono verifies the criterion; the next round completes the task.
    let workspace = tasks.workspaces("t").unwrap().remove(0);
    verify_criterion(&store, "t", "c1", &workspace, CHECK, NOW + 22).unwrap();
    let rounds = run.coordinator.verify(&store, NOW + 23).unwrap();
    assert_eq!(rounds[0].outcome, RoundOutcome::Completed, "{rounds:?}");
}

#[test]
fn a_round_waiting_for_mono_runs_again_when_its_evidence_goes_stale() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed_with(&store, &repo, scoped(&[CHECK]), &[], feature);
    run.coordinator.review(&store, NOW + 10).unwrap();
    let rounds = run.coordinator.verify(&store, NOW + 20).unwrap();
    assert_eq!(rounds[0].outcome, RoundOutcome::NeedsMonoReview);
    assert!(run.coordinator.verify(&store, NOW + 21).unwrap().is_empty());
    // A later edit invalidates the review's verification: the waiting round
    // is eligible again rather than resting on stale evidence.
    std::fs::write(run.tree.join("src/feature.rs"), "pub fn changed() {}\n").unwrap();
    let rounds = run.coordinator.verify(&store, NOW + 22).unwrap();
    assert_eq!(rounds.len(), 1, "{rounds:?}");
    assert_eq!(rounds[0].outcome, RoundOutcome::NeedsMonoReview);
    let tasks = store.tasks().unwrap();
    assert_eq!(tasks.pending_deliveries(MONO_REVIEW).unwrap().len(), 2);
}

#[test]
fn several_worktrees_wait_at_integrating_for_integration_not_mono() {
    let store = memory();
    let tasks = store.tasks().unwrap();
    tasks.create_task(&new_task("t"), 1).unwrap();
    let both = plan(vec![assignment("a", &[]), assignment("b", &[])]);
    tasks.save_plan("t", 1, &both, 1).unwrap();
    force(
        &store,
        Subject::Task("t"),
        State::Stage(TaskStage::Implementing),
    );
    for id in ["a", "b"] {
        let accepted = State::Assignment(AssignmentState::Accepted);
        force(&store, Subject::Assignment { task: "t", id }, accepted);
        tasks
            .save_workspace(&Workspace {
                id: format!("t/{id}"),
                task_id: "t".into(),
                assignment_id: Some(id.into()),
                mode: crate::orchestration::WorkspaceMode::Worktree,
                repository: "/repo".into(),
                path: format!("/repo-worktrees/{id}"),
                branch: Some(format!("mono/t/{id}")),
                base_commit: Some("abc".into()),
                owner: "task:t".into(),
                revision: 0,
                created_at: 1,
            })
            .unwrap();
    }
    let coordinator = Coordinator::new("gpui-1", ExplicitStart);
    assert!(coordinator.verify(&store, 10).unwrap().is_empty());
    // Integration (task-11) merges them; Mono is not asked to.
    assert_eq!(task(&store).stage, TaskStage::Integrating);
    coordinator.verify(&store, 11).unwrap();
    assert_eq!(task(&store).stage, TaskStage::Integrating);
    assert!(tasks.pending_deliveries(MONO_REVIEW).unwrap().is_empty());
    assert!(events(&store, "task.needs_integration").is_empty());
}

#[test]
fn verifying_several_worktrees_without_an_integration_is_refused_to_mono_once() {
    let store = memory();
    let tasks = store.tasks().unwrap();
    tasks.create_task(&new_task("t"), 1).unwrap();
    let both = plan(vec![assignment("a", &[]), assignment("b", &[])]);
    tasks.save_plan("t", 1, &both, 1).unwrap();
    force(
        &store,
        Subject::Task("t"),
        State::Stage(TaskStage::Verifying),
    );
    for id in ["a", "b"] {
        let accepted = State::Assignment(AssignmentState::Accepted);
        force(&store, Subject::Assignment { task: "t", id }, accepted);
        tasks
            .save_workspace(&Workspace {
                id: format!("t/{id}"),
                task_id: "t".into(),
                assignment_id: Some(id.into()),
                mode: crate::orchestration::WorkspaceMode::Worktree,
                repository: "/repo".into(),
                path: format!("/repo-worktrees/{id}"),
                branch: Some(format!("mono/t/{id}")),
                base_commit: Some("abc".into()),
                owner: "task:t".into(),
                revision: 0,
                created_at: 1,
            })
            .unwrap();
    }
    // Mono moved the task to Verifying itself: there is no integration.
    let coordinator = Coordinator::new("gpui-1", ExplicitStart);
    assert!(coordinator.verify(&store, 10).unwrap().is_empty());
    assert_eq!(task(&store).stage, TaskStage::Verifying);
    let refused = events(&store, "task.needs_integration");
    assert_eq!(refused.len(), 1);
    assert!(refused[0]
        .payload
        .to_string()
        .contains("without a clean integration"));
    assert_eq!(tasks.pending_deliveries(MONO_REVIEW).unwrap().len(), 1);
    coordinator.verify(&store, 11).unwrap();
    assert_eq!(tasks.pending_deliveries(MONO_REVIEW).unwrap().len(), 1);
}

fn verification_rows(store: &Store) -> usize {
    store.tasks().unwrap().verifications("t").unwrap().len()
}

fn mono_deliveries(store: &Store) -> usize {
    let tasks = store.tasks().unwrap();
    tasks.pending_deliveries(MONO_REVIEW).unwrap().len()
}

#[test]
fn correction_limit_during_verification_hands_to_mono_once_and_does_not_rerun() {
    let repo = repo();
    let store = open(&repo);
    let failing = "grep -q fixed src/feature.rs";
    let run = claimed_with(&store, &repo, scoped(&[CHECK]), &[failing], feature);
    run.coordinator.review(&store, NOW + 10).unwrap();
    // The assignment was already sent back the maximum number of times.
    let tasks = store.tasks().unwrap();
    for n in 0..MAX_CORRECTIONS {
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
    assert_eq!(
        rounds[0].outcome,
        RoundOutcome::NeedsMonoReview,
        "{rounds:?}"
    );
    assert!(rounds[0]
        .findings
        .iter()
        .any(|f| matches!(f, Finding::CorrectionLimit { .. })));
    let rows = verification_rows(&store);
    for later in 1..=3 {
        let again = run.coordinator.verify(&store, NOW + 20 + later).unwrap();
        assert!(again.is_empty(), "pass {later} re-ran: {again:?}");
    }
    assert_eq!(mono_deliveries(&store), 1);
    assert_eq!(verification_rows(&store), rows);
    assert_eq!(task(&store).stage, TaskStage::Verifying);
}

#[test]
fn a_passing_criterion_beside_one_without_checks_waits_for_mono_once() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed_with(&store, &repo, scoped(&[CHECK]), &[CHECK], feature);
    run.coordinator.review(&store, NOW + 10).unwrap();
    let tasks = store.tasks().unwrap();
    let mut criteria = task(&store).criteria;
    criteria.push(AcceptanceCriterion {
        id: "c2".into(),
        description: "Reads well".into(),
        required: true,
        checks: Vec::new(),
    });
    let revision = task(&store).revision;
    tasks
        .update_criteria("t", revision, &criteria, NOW + 11)
        .unwrap();
    let rounds = run.coordinator.verify(&store, NOW + 20).unwrap();
    assert_eq!(
        rounds[0].outcome,
        RoundOutcome::NeedsMonoReview,
        "{rounds:?}"
    );
    let rows = verification_rows(&store);
    for later in 1..=3 {
        let again = run.coordinator.verify(&store, NOW + 20 + later).unwrap();
        assert!(again.is_empty(), "pass {later} re-ran: {again:?}");
    }
    assert_eq!(mono_deliveries(&store), 1);
    assert_eq!(verification_rows(&store), rows);
}

#[test]
fn a_task_without_required_criteria_goes_to_mono() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed_with(&store, &repo, scoped(&[CHECK]), &[], feature);
    run.coordinator.review(&store, NOW + 10).unwrap();
    let tasks = store.tasks().unwrap();
    let revision = task(&store).revision;
    tasks.update_criteria("t", revision, &[], NOW + 11).unwrap();
    let rounds = run.coordinator.verify(&store, NOW + 20).unwrap();
    assert_eq!(
        rounds[0].outcome,
        RoundOutcome::NeedsMonoReview,
        "{rounds:?}"
    );
    assert!(matches!(
        &rounds[0].findings[0],
        Finding::NotChecked { reason } if reason.contains("no required acceptance criterion")
    ));
    assert!(run.coordinator.verify(&store, NOW + 21).unwrap().is_empty());
    assert_eq!(mono_deliveries(&store), 1);
}

#[test]
fn rounds_that_fail_to_record_go_to_mono_without_rerunning() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed_with(&store, &repo, scoped(&[CHECK]), &[CHECK], feature);
    run.coordinator.review(&store, NOW + 10).unwrap();
    for n in 0..REVIEW_RETRY_LIMIT {
        let (jobs, errors) = run.coordinator.verify_jobs(&store, NOW + 20).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(jobs.len(), 1);
        let error = format!("database is locked ({n})");
        run.coordinator
            .round_failed(&store, &jobs[0], &error, NOW + 20)
            .unwrap();
    }
    let rows = verification_rows(&store);
    let (jobs, _) = run.coordinator.verify_jobs(&store, NOW + 30).unwrap();
    assert!(jobs.is_empty());
    assert_eq!(mono_deliveries(&store), 1);
    let waiting = events(&store, "task.verification_waiting");
    assert!(waiting[0]
        .payload
        .to_string()
        .contains("database is locked (2)"));
    assert!(run.coordinator.verify(&store, NOW + 31).unwrap().is_empty());
    assert_eq!(verification_rows(&store), rows);
}

#[test]
fn one_broken_task_does_not_stop_the_others_from_verifying() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed_with(&store, &repo, scoped(&[CHECK]), &[CHECK], feature);
    run.coordinator.review(&store, NOW + 10).unwrap();
    let tasks = store.tasks().unwrap();
    tasks.create_task(&new_task("u"), NOW).unwrap();
    force(
        &store,
        Subject::Task("u"),
        State::Stage(TaskStage::Implementing),
    );
    store
        .connection()
        .execute(
            "UPDATE native_tasks SET criteria='not json' WHERE id='u'",
            [],
        )
        .unwrap();
    let (jobs, errors) = run.coordinator.verify_jobs(&store, NOW + 20).unwrap();
    assert_eq!(jobs.len(), 1, "t still verifies");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].starts_with("u: "), "{errors:?}");
}
