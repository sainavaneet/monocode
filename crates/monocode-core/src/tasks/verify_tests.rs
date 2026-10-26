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
use crate::tasks::review::MONO_REVIEW;
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
fn several_worktrees_wait_for_integration() {
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
    assert_eq!(task(&store).stage, TaskStage::Integrating);
    let waiting = tasks.pending_deliveries(MONO_REVIEW).unwrap();
    assert_eq!(waiting.len(), 1);
    let needs = events(&store, "task.needs_integration");
    assert!(needs[0].payload.to_string().contains("integrated result"));
    // Repeated passes queue it once.
    coordinator.verify(&store, 11).unwrap();
    assert_eq!(tasks.pending_deliveries(MONO_REVIEW).unwrap().len(), 1);
}
