//! Review, correction, verification and completion against temporary Git
//! repositories: a worker claims success in its worktree, the coordinator's
//! review inspects the real workspace and decides.
use super::*;
use crate::agent::Settings;
use crate::orchestration::WorkspaceMode;
use crate::store::{Session, Store};
use crate::tasks::completion::{self, complete, final_report, live_verifications};
use crate::tasks::coordinator::{Coordinator, ExplicitStart, Launcher, REVIEW};
use crate::tasks::store::tests::{assignment, force, plan};
use crate::tasks::{
    checks, confirm_started, AcceptanceCriterion, AssignmentState, DeliveryState, NewDecision,
    NewTask, Precondition, Requester, State, Subject, TaskError, TaskLifecycle, TaskStage,
};
use serde_json::json;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub(in crate::tasks) const NOW: u64 = 1_000_000;
pub(in crate::tasks) const CHECK: &str = "test -f src/feature.rs";

pub(in crate::tasks) struct Repo {
    _dir: tempfile::TempDir,
    pub(in crate::tasks) root: PathBuf,
    pub(in crate::tasks) db: PathBuf,
}

pub(in crate::tasks) fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

pub(in crate::tasks) fn repo() -> Repo {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let root = base.join("project");
    std::fs::create_dir_all(root.join("src")).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "test@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("src/lib.rs"), "pub fn one() {}\n").unwrap();
    std::fs::write(root.join("README.md"), "# Project\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "init"]);
    Repo {
        db: base.join("monocode.db"),
        _dir: dir,
        root,
    }
}

pub(in crate::tasks) fn open(repo: &Repo) -> Store {
    let store = Store::open(&repo.db).unwrap();
    store
        .save_setting(crate::tasks::coordinator::FLAG, "enabled")
        .unwrap();
    store
}

pub(in crate::tasks) fn scoped(checks: &[&str]) -> crate::tasks::Assignment {
    crate::tasks::Assignment {
        files: vec!["src".into()],
        workspace: Some(WorkspaceMode::Worktree),
        verification: checks.iter().map(|c| (*c).to_owned()).collect(),
        ..assignment("a", &[])
    }
}

#[derive(Default)]
pub(in crate::tasks) struct FakeLauncher {
    pub(in crate::tasks) started: RefCell<Vec<Session>>,
}

impl Launcher for FakeLauncher {
    fn start(&self, session: &Session, _settings: &Settings) -> Result<(), String> {
        self.started.borrow_mut().push(session.clone());
        Ok(())
    }
}

/// Task `t` with one assignment, launched into its worktree, where `work`
/// changes files before the worker claims success and review is requested.
pub(in crate::tasks) struct Claimed {
    pub(in crate::tasks) coordinator: Coordinator,
    pub(in crate::tasks) launcher: FakeLauncher,
    pub(in crate::tasks) tree: PathBuf,
}

pub(in crate::tasks) fn claimed(
    store: &Store,
    repo: &Repo,
    assignment: crate::tasks::Assignment,
    work: impl FnOnce(&Path),
) -> Claimed {
    claimed_with(store, repo, assignment, &[], work)
}

/// As [`claimed`], with criterion `c1` demonstrated by `criterion_checks`.
pub(in crate::tasks) fn claimed_with(
    store: &Store,
    repo: &Repo,
    assignment: crate::tasks::Assignment,
    criterion_checks: &[&str],
    work: impl FnOnce(&Path),
) -> Claimed {
    let tasks = store.tasks().unwrap();
    tasks
        .create_task(
            &NewTask {
                id: "t".into(),
                mono_id: Some("mono-1".into()),
                project: repo.root.to_string_lossy().into_owned(),
                title: "Ship it".into(),
                request: "Implement the feature".into(),
                criteria: vec![AcceptanceCriterion {
                    id: "c1".into(),
                    description: "The feature exists".into(),
                    required: true,
                    checks: criterion_checks.iter().map(|c| (*c).to_owned()).collect(),
                }],
            },
            NOW,
        )
        .unwrap();
    tasks
        .save_plan("t", 1, &plan(vec![assignment]), NOW)
        .unwrap();
    for stage in [TaskStage::Ready, TaskStage::Implementing] {
        stage_to(store, stage);
    }
    let coordinator = Coordinator::new("gpui-1", ExplicitStart);
    let launcher = FakeLauncher::default();
    let tick = coordinator.tick(store, &launcher, NOW).unwrap();
    assert!(tick.errors.is_empty(), "{tick:?}");
    let session = launcher.started.borrow()[0].id;
    assert!(confirm_started(store, session, NOW + 1).unwrap());
    let tree = PathBuf::from(&tasks.workspaces("t").unwrap()[0].path);
    work(&tree);
    crate::app_access::worker_action(
        store,
        session,
        "r1",
        "tasks.worker.result",
        &json!({"outcome": "succeeded", "summary": "Added the feature",
                "validation": ["test -f src/feature.rs passed"]}),
        NOW + 2,
    )
    .unwrap();
    let tick = coordinator.tick(store, &launcher, NOW + 3).unwrap();
    assert_eq!(tick.reviews.len(), 1, "{tick:?}");
    // The app stops the worker that reported; review waits for that.
    ack_stops(store);
    Claimed {
        coordinator,
        launcher,
        tree,
    }
}

/// Confirms every requested worker stop, as the app does once each
/// stopped session's turn has ended.
pub(in crate::tasks) fn ack_stops(store: &Store) {
    for stop in crate::tasks::recovery::pending_stops(store).unwrap() {
        crate::tasks::recovery::stopped(store, &stop.delivery, NOW).unwrap();
    }
}

pub(in crate::tasks) fn feature(tree: &Path) {
    std::fs::write(tree.join("src/feature.rs"), "pub fn feature() {}\n").unwrap();
}

pub(in crate::tasks) fn stage_to(store: &Store, stage: TaskStage) {
    let tasks = store.tasks().unwrap();
    let t = Subject::Task("t");
    let revision = tasks.revision(t).unwrap();
    tasks
        .transition(t, State::Stage(stage), Requester::Mono, revision, NOW)
        .unwrap();
}

pub(in crate::tasks) fn status(store: &Store) -> AssignmentState {
    store
        .tasks()
        .unwrap()
        .assignment("t", "a")
        .unwrap()
        .unwrap()
        .status
}

pub(in crate::tasks) fn events(store: &Store, kind: &str) -> Vec<crate::tasks::Event> {
    let all = store.tasks().unwrap().events_after(0, 10_000).unwrap();
    all.into_iter().filter(|e| e.kind == kind).collect()
}

pub(in crate::tasks) fn evidence(v: &crate::tasks::Verification) -> serde_json::Value {
    serde_json::from_str(&v.evidence).unwrap()
}

#[test]
fn a_failed_check_produces_a_correction_the_next_attempt_sees() {
    let repo = repo();
    let store = open(&repo);
    // The worker claims success but never wrote the file the check needs.
    let run = claimed(&store, &repo, scoped(&[CHECK]), |tree| {
        std::fs::write(tree.join("src/other.rs"), "// other\n").unwrap();
    });
    let reviews = run.coordinator.review(&store, NOW + 10).unwrap();
    assert_eq!(reviews.len(), 1);
    let review = &reviews[0];
    assert_eq!(review.outcome, ReviewOutcome::Correction);
    assert_eq!(review.changed_files, vec!["src/other.rs".to_owned()]);
    let failed = review
        .findings
        .iter()
        .find_map(|f| match f {
            Finding::FailedCheck {
                check,
                exit_code,
                timed_out,
                ..
            } => Some((check.clone(), *exit_code, *timed_out)),
            _ => None,
        })
        .expect("a failed check finding");
    assert_eq!(failed, (CHECK.to_owned(), Some(1), false));
    assert_eq!(status(&store), AssignmentState::Queued);

    // The check result is a verification of the snapshot it ran against.
    let tasks = store.tasks().unwrap();
    let verifications = tasks.verifications("t").unwrap();
    assert_eq!(verifications.len(), 1);
    let v = &verifications[0];
    assert!(!v.passed);
    assert_eq!(v.check, CHECK);
    assert_eq!(Some(&v.checkpoint_commit), review.checkpoint.as_ref());
    assert_eq!(evidence(v)["exitCode"], 1);
    assert_eq!(evidence(v)["path"], run.tree.to_string_lossy().as_ref());

    // The correction is durable, tied to the assignment, and the delivery
    // is acknowledged.
    let correction = tasks.latest_correction("t", "a").unwrap().unwrap();
    assert_eq!(&correction, review);
    let delivery = tasks
        .delivery(&format!("review:{}", review.attempt_id))
        .unwrap()
        .unwrap();
    assert_eq!(delivery.state, DeliveryState::Acknowledged);
    assert!(tasks.pending_deliveries(REVIEW).unwrap().is_empty());

    // The next tick relaunches into the same worktree with the findings.
    let tick = run
        .coordinator
        .tick(&store, &run.launcher, NOW + 20)
        .unwrap();
    assert_eq!(tick.launched.len(), 1, "{tick:?}");
    assert_eq!(tasks.attempts("t", "a").unwrap().len(), 2);
    let session = run.launcher.started.borrow()[1].id;
    let brief = store.follow_ups(session).unwrap().draft.unwrap().text;
    for needle in ["sent back", CHECK, "exit code 1"] {
        assert!(brief.contains(needle), "brief lacks {needle}: {brief}");
    }
}

#[test]
fn an_accepted_result_needs_a_passing_verification() {
    // Without any check to run, review cannot accept: Mono decides.
    let repo1 = repo();
    let store = open(&repo1);
    let run = claimed(&store, &repo1, scoped(&[]), feature);
    let reviews = run.coordinator.review(&store, NOW + 10).unwrap();
    assert_eq!(reviews[0].outcome, ReviewOutcome::NeedsMonoReview);
    assert!(reviews[0].findings.contains(&Finding::NoChecks));
    assert_eq!(status(&store), AssignmentState::NeedsReview);
    let tasks = store.tasks().unwrap();
    let handed = tasks.pending_deliveries(MONO_REVIEW).unwrap();
    assert_eq!(handed.len(), 1);
    assert_eq!(
        handed[0].event_id,
        format!("reviewed:{}", reviews[0].attempt_id)
    );
    assert!(tasks.verifications("t").unwrap().is_empty());

    // A passing check in the worktree, never the main checkout, accepts.
    let repo2 = repo();
    let store = open(&repo2);
    let run = claimed(&store, &repo2, scoped(&[CHECK]), feature);
    assert!(!repo2.root.join("src/feature.rs").exists());
    let reviews = run.coordinator.review(&store, NOW + 10).unwrap();
    let review = &reviews[0];
    assert_eq!(review.outcome, ReviewOutcome::Accepted, "{review:?}");
    assert_eq!(status(&store), AssignmentState::Accepted);
    let tasks = store.tasks().unwrap();
    let verifications = tasks.verifications("t").unwrap();
    assert_eq!(verifications.len(), 1);
    let v = &verifications[0];
    assert!(v.passed && !v.invalidated);
    assert_eq!(v.assignment_id.as_deref(), Some("a"));
    assert_eq!(evidence(v)["path"], run.tree.to_string_lossy().as_ref());
    assert_eq!(evidence(v)["exitCode"], 0);
    // The checkpoint holds the uncommitted file the check saw.
    let shown = git(
        &run.tree,
        &["show", &format!("{}:src/feature.rs", v.checkpoint_commit)],
    );
    assert_eq!(shown, "pub fn feature() {}");
    assert_eq!(review.verifications, vec![v.id.clone()]);
    assert!(tasks.pending_deliveries(MONO_REVIEW).unwrap().is_empty());
}

#[test]
fn files_outside_the_scope_produce_a_correction() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed(&store, &repo, scoped(&[CHECK]), |tree| {
        feature(tree);
        std::fs::write(tree.join("README.md"), "# Changed\n").unwrap();
    });
    let reviews = run.coordinator.review(&store, NOW + 10).unwrap();
    let review = &reviews[0];
    assert_eq!(review.outcome, ReviewOutcome::Correction);
    assert!(review.findings.contains(&Finding::OutOfScope {
        path: "README.md".into()
    }));
    assert_eq!(status(&store), AssignmentState::Queued);
}

#[test]
fn a_changed_workspace_invalidates_its_verifications() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed(&store, &repo, scoped(&[CHECK]), feature);
    run.coordinator.review(&store, NOW + 10).unwrap();
    let tasks = store.tasks().unwrap();
    let workspace = tasks.workspaces("t").unwrap().remove(0);
    let criterion =
        completion::verify_criterion(&store, "t", "c1", &workspace, CHECK, NOW + 11).unwrap();
    assert!(criterion.passed);
    assert_eq!(criterion.criterion_id.as_deref(), Some("c1"));

    // Unchanged: both stay live.
    assert_eq!(live_verifications(&store, "t", NOW + 12).unwrap().len(), 2);
    assert!(events(&store, "verification.invalidated").is_empty());

    // A later edit makes both stale: they are neither shown nor counted.
    std::fs::write(run.tree.join("src/feature.rs"), "pub fn changed() {}\n").unwrap();
    assert!(live_verifications(&store, "t", NOW + 13)
        .unwrap()
        .is_empty());
    assert!(tasks
        .verifications("t")
        .unwrap()
        .iter()
        .all(|v| v.invalidated));
    assert_eq!(events(&store, "verification.invalidated").len(), 2);

    // Verifying again records a live result for the new snapshot.
    let again =
        completion::verify_criterion(&store, "t", "c1", &workspace, CHECK, NOW + 14).unwrap();
    assert_ne!(again.checkpoint_commit, criterion.checkpoint_commit);
    let live = live_verifications(&store, "t", NOW + 15).unwrap();
    assert_eq!(live.iter().map(|v| &v.id).collect::<Vec<_>>(), [&again.id]);
}

#[test]
fn duplicate_review_deliveries_do_not_review_twice() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed(&store, &repo, scoped(&[CHECK]), feature);
    // Two passes pick up the same delivery before either records.
    let first = run
        .coordinator
        .review_jobs(&store, NOW + 10, &mut Default::default())
        .unwrap();
    let second = run
        .coordinator
        .review_jobs(&store, NOW + 10, &mut Default::default())
        .unwrap();
    assert_eq!((first.len(), second.len()), (1, 1));
    let recorded = run
        .coordinator
        .record_review(&store, &first[0], first[0].inspect(), NOW + 11)
        .unwrap();
    assert!(recorded.is_some());
    let tasks = store.tasks().unwrap();
    let revision = tasks
        .revision(Subject::Assignment { task: "t", id: "a" })
        .unwrap();
    let duplicate = run
        .coordinator
        .record_review(&store, &second[0], second[0].inspect(), NOW + 12)
        .unwrap();
    assert_eq!(duplicate, None);

    // A redelivered request after the review is acknowledged, not re-run.
    let attempt = &recorded.unwrap().attempt_id;
    store
        .connection()
        .execute(
            "UPDATE native_task_deliveries SET state='pending' WHERE id=?1",
            [format!("review:{attempt}")],
        )
        .unwrap();
    assert!(run.coordinator.review(&store, NOW + 13).unwrap().is_empty());
    assert_eq!(events(&store, "assignment.reviewed").len(), 1);
    assert_eq!(events(&store, "assignment.accepted").len(), 1);
    assert_eq!(tasks.verifications("t").unwrap().len(), 1);
    assert_eq!(
        tasks
            .revision(Subject::Assignment { task: "t", id: "a" })
            .unwrap(),
        revision
    );
    assert!(tasks.pending_deliveries(REVIEW).unwrap().is_empty());
}

#[test]
fn checks_that_need_a_shell_or_are_prose_go_to_mono_unrun() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed(
        &store,
        &repo,
        scoped(&["rm -rf src && true", "Run the tests and confirm they pass"]),
        feature,
    );
    let reviews = run.coordinator.review(&store, NOW + 10).unwrap();
    let review = &reviews[0];
    assert_eq!(review.outcome, ReviewOutcome::NeedsMonoReview);
    let unrunnable = review
        .findings
        .iter()
        .filter(|f| matches!(f, Finding::UnrunnableCheck { .. }))
        .count();
    assert_eq!(unrunnable, 2);
    assert!(run.tree.join("src/lib.rs").exists(), "nothing was executed");
    assert!(store
        .tasks()
        .unwrap()
        .verifications("t")
        .unwrap()
        .is_empty());
    assert_eq!(status(&store), AssignmentState::NeedsReview);

    // A plain command, runnable on its own, never runs in the user's
    // checkout: investigations go to Mono unchecked.
    let repo = super::tests::repo();
    let store = open(&repo);
    let investigation = crate::tasks::Assignment {
        workspace: Some(WorkspaceMode::Checkout),
        ..scoped(&["touch ran-sentinel"])
    };
    let run = claimed(&store, &repo, investigation, |_| {});
    assert_eq!(run.tree, repo.root);
    let reviews = run.coordinator.review(&store, NOW + 10).unwrap();
    assert_eq!(reviews[0].outcome, ReviewOutcome::NeedsMonoReview);
    let unchecked = reviews[0].findings.last();
    assert!(matches!(unchecked, Some(Finding::NotChecked { .. })));
    assert!(
        !repo.root.join("ran-sentinel").exists(),
        "ran in the checkout"
    );
    assert!(reviews[0].checkpoint.is_none());
    let refs = git(&repo.root, &["for-each-ref", "refs/monocode"]);
    assert!(
        refs.is_empty(),
        "no snapshot in the user's repository: {refs}"
    );
}

#[test]
fn a_requirement_changed_after_launch_sends_the_work_back() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed(&store, &repo, scoped(&[CHECK]), feature);
    let tasks = store.tasks().unwrap();
    let mut changed = tasks.assignment("t", "a").unwrap().unwrap();
    changed.prompt = "Do the work differently".into();
    tasks.update_assignment("t", &changed, NOW + 5).unwrap();
    let reviews = run.coordinator.review(&store, NOW + 10).unwrap();
    let review = &reviews[0];
    assert_eq!(review.outcome, ReviewOutcome::Correction);
    assert!(review
        .findings
        .iter()
        .any(|f| matches!(f, Finding::StaleRevision { .. })));
    assert_eq!(status(&store), AssignmentState::Queued);
}

#[test]
fn send_backs_invalidate_verifications_that_no_longer_hold() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed(&store, &repo, scoped(&[CHECK]), feature);
    run.coordinator.review(&store, NOW + 10).unwrap();
    let tasks = store.tasks().unwrap();
    let a = Subject::Assignment { task: "t", id: "a" };
    let queued = State::Assignment(AssignmentState::Queued);
    tasks
        .transition(
            a,
            queued,
            Requester::Mono,
            tasks.revision(a).unwrap(),
            NOW + 11,
        )
        .unwrap();
    assert!(tasks
        .verifications("t")
        .unwrap()
        .iter()
        .all(|v| v.invalidated));

    // A stage send-back invalidates task-level verifications too.
    force(
        &store,
        Subject::Task("t"),
        State::Stage(TaskStage::Verifying),
    );
    let workspace = tasks.workspaces("t").unwrap().remove(0);
    let criterion =
        completion::verify_criterion(&store, "t", "c1", &workspace, CHECK, NOW + 12).unwrap();
    assert!(!criterion.invalidated);
    stage_to(&store, TaskStage::Implementing);
    assert!(tasks
        .verifications("t")
        .unwrap()
        .iter()
        .all(|v| v.invalidated));
}

#[test]
fn completion_needs_criteria_no_open_decision_and_a_final_report() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed(&store, &repo, scoped(&[CHECK]), |tree| {
        feature(tree);
        git(tree, &["add", "."]);
        git(tree, &["commit", "-q", "-m", "feature"]);
    });
    let commit = git(&run.tree, &["rev-parse", "HEAD"]);
    run.coordinator.review(&store, NOW + 10).unwrap();
    assert_eq!(status(&store), AssignmentState::Accepted);
    for stage in [
        TaskStage::Reviewing,
        TaskStage::Integrating,
        TaskStage::Verifying,
    ] {
        stage_to(&store, stage);
    }
    let tasks = store.tasks().unwrap();
    let refused = |expected: Precondition| {
        assert_eq!(
            complete(&store, "t", Requester::Mono, NOW + 20).map(|_| ()),
            Err(TaskError::Precondition(expected))
        );
        assert!(events(&store, "task.final_report").is_empty());
    };

    // No criteria at all is not "every criterion satisfied".
    let t = Subject::Task("t");
    tasks
        .update_criteria("t", tasks.revision(t).unwrap(), &[], NOW + 11)
        .unwrap();
    refused(Precondition::CriteriaSatisfied);
    let required = AcceptanceCriterion {
        id: "c1".into(),
        description: "The feature exists".into(),
        required: true,
        checks: Vec::new(),
    };
    tasks
        .update_criteria("t", tasks.revision(t).unwrap(), &[required], NOW + 12)
        .unwrap();
    refused(Precondition::CriteriaSatisfied);
    let workspace = tasks.workspaces("t").unwrap().remove(0);
    completion::verify_criterion(&store, "t", "c1", &workspace, CHECK, NOW + 13).unwrap();

    // An unresolved decision blocks completion, answered or not.
    tasks
        .create_decision(
            &NewDecision {
                id: "d".into(),
                task_id: "t".into(),
                assignment_id: None,
                attempt_id: None,
                question: "Ship to production?".into(),
                context: String::new(),
                scope: crate::tasks::DecisionScope::Task,
                asked_by: Requester::Mono,
            },
            NOW + 14,
        )
        .unwrap();
    refused(Precondition::NoBlockingDecision);
    tasks
        .answer_decision("d", 1, "Not yet", Requester::User, NOW + 15)
        .unwrap();
    refused(Precondition::NoBlockingDecision);
    tasks.resolve_decision("d", 2, NOW + 16).unwrap();

    // The table itself refuses completion without a persisted report.
    assert_eq!(
        tasks.transition(
            t,
            State::Stage(TaskStage::Completed),
            Requester::Mono,
            tasks.revision(t).unwrap(),
            NOW + 17,
        ),
        Err(TaskError::Precondition(Precondition::FinalReportRecorded))
    );

    let report = complete(&store, "t", Requester::Mono, NOW + 20).unwrap();
    assert!(report.implemented && report.committed);
    assert!(!report.merged && !report.pushed && !report.deployed);
    let a = &report.assignments[0];
    assert_eq!(a.commits, vec![commit.clone()]);
    assert!(!a.uncommitted);
    assert!(report.criteria[0].satisfied);
    let task = tasks.task("t").unwrap().unwrap();
    assert_eq!(task.stage, TaskStage::Completed);
    assert_eq!(task.lifecycle, TaskLifecycle::Completed);
    let persisted = events(&store, "task.final_report");
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].payload, serde_json::to_value(&report).unwrap());

    // The owning Mono hears about the report in its inbox, with the
    // evidence flags and no task action owed, and the card keeps showing
    // the task with its report until the user hides it.
    let batch = crate::tasks::inbox::next_batch(&store, "mono-1", NOW + 21)
        .unwrap()
        .expect("the final report reaches Mono");
    assert!(batch.message.contains("Task complete"), "{}", batch.message);
    assert!(
        batch
            .message
            .contains("implemented, committed, not merged, not pushed, not deployed"),
        "{}",
        batch.message
    );
    assert!(batch.message.contains("1 commit(s)"), "{}", batch.message);
    assert!(
        batch.message.contains("Criteria satisfied: 1 of 1"),
        "{}",
        batch.message
    );
    let shown = crate::tasks::view::monitor(&store, "mono-1", NOW + 22).unwrap();
    assert_eq!(shown.len(), 1);
    let summary = shown[0]
        .report
        .clone()
        .expect("completed tasks carry their report");
    assert!(summary.implemented && summary.committed && !summary.merged);
    assert_eq!(
        summary.evidence_line(),
        "implemented, committed · not merged, not pushed, not deployed"
    );
    assert_eq!((summary.criteria_satisfied, summary.criteria_total), (1, 1));
    assert_eq!(summary.assignments[0].1, 1);
    assert_eq!(shown[0].badge(), None, "a completed task raises no badge");
    assert_eq!(shown[0].activity, crate::tasks::view::MonoActivity::Idle);
    // A day later it has aged out; hiding it removes it at once.
    let later = NOW + 22 + crate::tasks::lifecycle::RECENT_COMPLETION_MS;
    assert!(crate::tasks::view::monitor(&store, "mono-1", later)
        .unwrap()
        .is_empty());
    crate::tasks::lifecycle::dismiss_report(&store, "t").unwrap();
    assert!(crate::tasks::view::monitor(&store, "mono-1", NOW + 23)
        .unwrap()
        .is_empty());

    // Merged comes from the repository, never from what a worker wrote.
    let branch = workspace.branch.clone().unwrap();
    git(&repo.root, &["merge", "-q", "--ff-only", &branch]);
    let after = final_report(&store, "t", NOW + 30).unwrap();
    assert!(after.merged && !after.pushed && !after.deployed);
}

#[test]
fn check_runs_record_exit_codes_truncate_output_and_time_out() {
    let dir = tempfile::tempdir().unwrap();
    let passed = checks::run("true", dir.path(), Duration::from_secs(30)).unwrap();
    assert_eq!((passed.exit_code, passed.timed_out), (Some(0), false));
    assert!(passed.passed());
    let noisy = checks::run("seq 1 200000", dir.path(), Duration::from_secs(30)).unwrap();
    assert!(noisy.passed() && noisy.truncated);
    assert!(noisy.output.len() <= checks::CHECK_OUTPUT_BYTES);
    assert!(noisy.output.ends_with("200000"));
    let slow = checks::run("sleep 5", dir.path(), Duration::from_millis(200)).unwrap();
    assert!(slow.timed_out && !slow.passed());
    assert!(slow.duration_ms < 4_000);
    assert!(checks::run(
        "nonexistent-program-xyz",
        dir.path(),
        Duration::from_secs(1)
    )
    .is_err());
    assert!(checks::run("echo $HOME", dir.path(), Duration::from_secs(1)).is_err());
}

#[test]
fn repeated_corrections_stop_at_the_limit_and_go_to_mono() {
    let repo = repo();
    let store = open(&repo);
    let run = claimed(&store, &repo, scoped(&[CHECK]), |_| {});
    // Earlier attempts were already sent back MAX_CORRECTIONS times.
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
        let event = crate::tasks::NewEvent {
            id: format!("reviewed:earlier-{n}"),
            task_id: "t".into(),
            assignment_id: Some("a".into()),
            attempt_id: None,
            kind: "assignment.reviewed".into(),
            payload: serde_json::to_value(&earlier).unwrap(),
        };
        tasks.append_event(&event, NOW + 4).unwrap();
    }
    let reviews = run.coordinator.review(&store, NOW + 10).unwrap();
    let review = &reviews[0];
    assert_eq!(review.outcome, ReviewOutcome::NeedsMonoReview);
    assert!(review.findings.contains(&Finding::CorrectionLimit {
        corrections: MAX_CORRECTIONS
    }));
    assert_eq!(status(&store), AssignmentState::NeedsReview);
    assert_eq!(tasks.pending_deliveries(MONO_REVIEW).unwrap().len(), 1);
}
