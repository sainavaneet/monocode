//! Integration of several worktrees against temporary Git repositories:
//! workers commit on their own branches, review accepts them, and the
//! coordinator merges them in dependency order onto the task's integration
//! branch, checks the combined result and completes the task, or surfaces a
//! conflict and re-integrates once it is resolved.
use super::*;
use crate::store::Store;
use crate::tasks::autonomy::{self, Autonomy};
use crate::tasks::coordinator::{Coordinator, Policy};
use crate::tasks::review::tests::{events, git, open, repo, FakeLauncher, Repo, NOW};
use crate::tasks::review::MAX_CORRECTIONS;
use crate::tasks::store::tests::{assignment, plan};
use crate::tasks::{
    confirm_started, AcceptanceCriterion, Assignment, AssignmentState, DecisionScope,
    DecisionState, Finding, NewEvent, NewTask, Requester, Review, ReviewOutcome, RoundOutcome,
    State, Subject, Task, TaskLifecycle, TaskStage,
};
use serde_json::json;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The live policy, changeable mid-test: two workers, merging allowed
/// unless a test takes it away.
struct Live(RefCell<Autonomy>);
impl Live {
    fn merging() -> Self {
        Self(RefCell::new(Autonomy {
            worker_limit: 2,
            merge: true,
            ..Autonomy::default()
        }))
    }
    fn without_merge() -> Self {
        let live = Self::merging();
        live.0.borrow_mut().merge = false;
        live
    }
}
impl Policy for Live {
    fn autonomy(&self, _: &Task) -> Autonomy {
        self.0.borrow().clone()
    }
}

struct Run {
    store: Store,
    coordinator: Coordinator<Live>,
    launcher: FakeLauncher,
    requests: RefCell<u32>,
}

fn part(id: &str, files: &[&str], deps: &[&str], check: &str) -> Assignment {
    Assignment {
        files: files.iter().map(|f| (*f).to_owned()).collect(),
        verification: vec![check.into()],
        ..assignment(id, deps)
    }
}

/// Task `t` in `repo` with `parts`, criterion `c1` checked by `checks` on
/// the task's result, started by Mono.
fn started(repo: &Repo, live: Live, parts: Vec<Assignment>, checks: &[&str]) -> Run {
    let store = open(repo);
    let tasks = store.tasks().unwrap();
    tasks
        .create_task(
            &NewTask {
                id: "t".into(),
                mono_id: Some("mono-1".into()),
                project: repo.root.to_string_lossy().into_owned(),
                title: "Ship it".into(),
                request: "Implement both parts".into(),
                criteria: vec![AcceptanceCriterion {
                    id: "c1".into(),
                    description: "Both parts exist".into(),
                    required: true,
                    checks: checks.iter().map(|c| (*c).to_owned()).collect(),
                }],
            },
            NOW,
        )
        .unwrap();
    tasks.save_plan("t", 1, &plan(parts), NOW).unwrap();
    for stage in [TaskStage::Ready, TaskStage::Implementing] {
        let revision = tasks.revision(Subject::Task("t")).unwrap();
        tasks
            .transition(
                Subject::Task("t"),
                State::Stage(stage),
                Requester::Mono,
                revision,
                NOW,
            )
            .unwrap();
    }
    Run {
        store,
        coordinator: Coordinator::new("gpui-1", live),
        launcher: FakeLauncher::default(),
        requests: RefCell::new(0),
    }
}

impl Run {
    fn tick(&self, now: u64) -> crate::tasks::coordinator::Tick {
        let tick = self
            .coordinator
            .tick(&self.store, &self.launcher, now)
            .unwrap();
        assert!(tick.errors.is_empty(), "{tick:?}");
        tick
    }

    fn task(&self) -> Task {
        self.store.tasks().unwrap().task("t").unwrap().unwrap()
    }

    fn status(&self, id: &str) -> AssignmentState {
        let tasks = self.store.tasks().unwrap();
        tasks.assignment("t", id).unwrap().unwrap().status
    }

    fn tree(&self, id: &str) -> PathBuf {
        let tasks = self.store.tasks().unwrap();
        let workspace = tasks
            .workspaces("t")
            .unwrap()
            .into_iter()
            .find(|w| w.assignment_id.as_deref() == Some(id))
            .unwrap();
        PathBuf::from(workspace.path)
    }

    /// The app session of `id`'s latest attempt.
    fn session(&self, id: &str) -> i64 {
        let tasks = self.store.tasks().unwrap();
        let attempt = tasks.attempts("t", id).unwrap().pop().unwrap();
        let bound = attempt.session_id.unwrap();
        let started = self.launcher.started.borrow();
        started
            .iter()
            .rev()
            .find(|s| self.store.persisted_session_id(s.id).unwrap() == bound)
            .unwrap()
            .id
    }

    /// `id`'s worker starts, does `work` in its worktree and claims success;
    /// review then decides.
    fn work(&self, id: &str, now: u64, work: impl FnOnce(&Path)) -> Vec<Review> {
        let session = self.session(id);
        assert!(confirm_started(&self.store, session, now).unwrap());
        work(&self.tree(id));
        let request = {
            let mut n = self.requests.borrow_mut();
            *n += 1;
            format!("r{n}")
        };
        crate::app_access::worker_action(
            &self.store,
            session,
            &request,
            "tasks.worker.result",
            &json!({"outcome": "succeeded", "summary": "Done", "validation": []}),
            now + 1,
        )
        .unwrap();
        self.tick(now + 2);
        self.coordinator.review(&self.store, now + 3).unwrap()
    }

    fn integrate(&self, now: u64) -> Vec<IntegrationRecord> {
        self.coordinator.integrate(&self.store, now).unwrap()
    }
}

fn commit(tree: &Path, file: &str, content: &str) {
    std::fs::write(tree.join(file), content).unwrap();
    git(tree, &["add", "."]);
    git(tree, &["commit", "-q", "-m", file]);
}

fn ok(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
        .status
        .success()
}

fn integration_tree(run: &Run) -> PathBuf {
    let tasks = run.store.tasks().unwrap();
    let workspace = tasks
        .workspaces("t")
        .unwrap()
        .into_iter()
        .find(|w| w.id == workspace_id("t"))
        .expect("the integration worktree is recorded");
    assert_eq!(workspace.assignment_id, None);
    assert_eq!(workspace.branch.as_deref(), Some(branch_for("t").as_str()));
    assert_eq!(workspace.owner, "task:t");
    PathBuf::from(workspace.path)
}

#[test]
fn several_worktrees_integrate_in_dependency_order_onto_the_recorded_base_and_complete() {
    let repo = repo();
    let base = git(&repo.root, &["rev-parse", "HEAD"]);
    // b depends on a, so it starts once a is accepted and merges after it.
    let parts = vec![
        part("b", &["src/b.rs"], &["a"], "test -f src/b.rs"),
        part("a", &["src/a.rs"], &[], "test -f src/a.rs"),
    ];
    let checks = ["test -f src/a.rs", "test -f src/b.rs"];
    let run = started(&repo, Live::merging(), parts, &checks);
    assert_eq!(run.tick(NOW).launched.len(), 1);
    let reviews = run.work("a", NOW + 10, |tree| {
        commit(tree, "src/a.rs", "pub fn a() {}\n")
    });
    assert_eq!(reviews[0].outcome, ReviewOutcome::Accepted, "{reviews:?}");
    assert_eq!(run.tick(NOW + 20).launched.len(), 1);
    let reviews = run.work("b", NOW + 30, |tree| {
        commit(tree, "src/b.rs", "pub fn b() {}\n")
    });
    assert_eq!(reviews[0].outcome, ReviewOutcome::Accepted, "{reviews:?}");
    // The user's branch moves on meanwhile; integration keeps the base.
    commit(&repo.root, "README.md", "# Moved on\n");
    let upstream = git(&repo.root, &["rev-parse", "HEAD"]);

    // Verification reaches Integrating and leaves it to integration.
    assert!(run
        .coordinator
        .verify(&run.store, NOW + 40)
        .unwrap()
        .is_empty());
    assert_eq!(run.task().stage, TaskStage::Integrating);
    let mono = run.store.tasks().unwrap();
    assert!(mono
        .pending_deliveries(crate::tasks::review::MONO_REVIEW)
        .unwrap()
        .is_empty());
    assert!(events(&run.store, "task.needs_integration").is_empty());

    let records = run.integrate(NOW + 50);
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(record.outcome, Outcome::Clean, "{record:?}");
    assert_eq!(record.order, ["a", "b"]);
    assert_eq!(record.base, base);
    assert_eq!(record.upstream_head.as_deref(), Some(upstream.as_str()));
    assert!(record.base_moved);
    assert!(record.merges.iter().all(|m| m.merged));
    assert_eq!(run.task().stage, TaskStage::Verifying);
    assert_eq!(events(&run.store, INTEGRATION).len(), 1);

    let tree = integration_tree(&run);
    assert!(tree.join("src/a.rs").exists() && tree.join("src/b.rs").exists());
    assert!(ok(&tree, &["merge-base", "--is-ancestor", &base, "HEAD"]));
    assert!(
        !ok(&tree, &["merge-base", "--is-ancestor", &upstream, "HEAD"]),
        "integration never silently rebases onto the moved upstream"
    );
    // Nothing reached the user's branch, and nothing was pushed.
    assert_eq!(git(&repo.root, &["rev-parse", "HEAD"]), upstream);
    assert!(!repo.root.join("src/a.rs").exists());

    // The criteria are checked on the combined result and the task completes.
    let rounds = run.coordinator.verify(&run.store, NOW + 60).unwrap();
    assert_eq!(rounds.len(), 1);
    assert_eq!(rounds[0].outcome, RoundOutcome::Completed, "{rounds:?}");
    let report = rounds[0].report.as_ref().unwrap();
    let integration = report
        .integration
        .as_ref()
        .expect("the final integration state");
    assert_eq!(integration["outcome"]["state"], "clean");
    assert_eq!(integration["branch"], branch_for("t"));
    let verifications = run.store.tasks().unwrap().verifications("t").unwrap();
    let criterion = verifications
        .iter()
        .find(|v| v.criterion_id.as_deref() == Some("c1"))
        .unwrap();
    let evidence: serde_json::Value = serde_json::from_str(&criterion.evidence).unwrap();
    assert_eq!(evidence["path"], tree.to_string_lossy().as_ref());
    let task = run.task();
    assert_eq!(
        (task.stage, task.lifecycle),
        (TaskStage::Completed, TaskLifecycle::Completed)
    );
}

/// Two parts editing `src/lib.rs` differently, both accepted, a first.
fn conflicting(repo: &Repo, live: Live) -> Run {
    let parts = vec![
        part("a", &["src/lib.rs"], &[], "test -f src/lib.rs"),
        part("b", &["src"], &[], "test -f src/lib.rs"),
    ];
    let checks = ["grep -q a_part src/lib.rs", "grep -q b_part src/lib.rs"];
    let run = started(repo, live, parts, &checks);
    // Overlapping scopes: only a runs at first.
    assert_eq!(run.tick(NOW).launched.len(), 1);
    let reviews = run.work("a", NOW + 10, |tree| {
        commit(tree, "src/lib.rs", "pub fn one() {}\npub fn a_part() {}\n")
    });
    assert_eq!(reviews[0].outcome, ReviewOutcome::Accepted, "{reviews:?}");
    // b started from the same base once a's worker had finished.
    assert_eq!(run.status("b"), AssignmentState::Running);
    let reviews = run.work("b", NOW + 30, |tree| {
        commit(tree, "src/lib.rs", "pub fn one() {}\npub fn b_part() {}\n")
    });
    assert_eq!(reviews[0].outcome, ReviewOutcome::Accepted, "{reviews:?}");
    assert!(run
        .coordinator
        .verify(&run.store, NOW + 40)
        .unwrap()
        .is_empty());
    assert_eq!(run.task().stage, TaskStage::Integrating);
    run
}

#[test]
fn an_integration_conflict_is_surfaced_and_retested_after_resolution() {
    let repo = repo();
    let run = conflicting(&repo, Live::merging());
    let records = run.integrate(NOW + 50);
    let record = &records[0];
    let Outcome::Conflict {
        assignment, files, ..
    } = &record.outcome
    else {
        panic!("{record:?}");
    };
    assert_eq!(
        (assignment.as_str(), files.as_slice()),
        ("b", &["src/lib.rs".to_owned()][..])
    );
    // The merge was aborted: the integration worktree is clean, with a merged.
    let tree = integration_tree(&run);
    assert_eq!(git(&tree, &["status", "--porcelain"]), "");
    assert!(!ok(&tree, &["rev-parse", "-q", "--verify", "MERGE_HEAD"]));
    let onto = git(&tree, &["rev-parse", "HEAD"]);
    assert_eq!(record.head.as_deref(), Some(onto.as_str()));
    // Within the task's authority: b is sent back to rebase, a stays.
    assert_eq!(run.status("a"), AssignmentState::Accepted);
    assert_eq!(run.status("b"), AssignmentState::Queued);
    assert_eq!(run.task().stage, TaskStage::Implementing);
    let tasks = run.store.tasks().unwrap();
    let correction = tasks.latest_correction("t", "b").unwrap().unwrap();
    assert!(correction.findings.iter().any(|f| matches!(
        f,
        Finding::IntegrationConflict { branch, onto: o, files }
            if *branch == branch_for("t") && *o == onto && files == &["src/lib.rs".to_owned()]
    )));

    // The relaunched worker is told what to do, and resolves it.
    assert_eq!(run.tick(NOW + 60).launched.len(), 1);
    let session = run.session("b");
    let brief = run.store.follow_ups(session).unwrap().draft.unwrap().text;
    assert!(
        brief.contains(&branch_for("t")) && brief.contains("src/lib.rs"),
        "{brief}"
    );
    let reviews = run.work("b", NOW + 70, |tree| {
        assert!(
            !ok(tree, &["rebase", &branch_for("t")]),
            "the rebase conflicts"
        );
        std::fs::write(
            tree.join("src/lib.rs"),
            "pub fn one() {}\npub fn a_part() {}\npub fn b_part() {}\n",
        )
        .unwrap();
        git(tree, &["add", "src/lib.rs"]);
        git(tree, &["-c", "core.editor=true", "rebase", "--continue"]);
    });
    assert_eq!(reviews[0].outcome, ReviewOutcome::Accepted, "{reviews:?}");
    // Review scoped the rebased work against the integration branch it now
    // starts from, and recorded that as the worktree's base.
    assert_eq!(reviews[0].changed_files, ["src/lib.rs"]);
    let rebased = tasks
        .workspaces("t")
        .unwrap()
        .into_iter()
        .find(|w| w.assignment_id.as_deref() == Some("b"))
        .unwrap();
    assert_eq!(rebased.base_commit.as_deref(), Some(onto.as_str()));

    // Re-integration is clean and the combined result is retested.
    assert!(run
        .coordinator
        .verify(&run.store, NOW + 80)
        .unwrap()
        .is_empty());
    let records = run.integrate(NOW + 90);
    assert_eq!(records[0].outcome, Outcome::Clean, "{records:?}");
    let merged: Vec<(&str, bool)> = records[0]
        .merges
        .iter()
        .map(|m| (m.assignment.as_str(), m.merged))
        .collect();
    assert_eq!(merged, [("a", false), ("b", true)]);
    let rounds = run.coordinator.verify(&run.store, NOW + 100).unwrap();
    assert_eq!(rounds[0].outcome, RoundOutcome::Completed, "{rounds:?}");
    assert_eq!(events(&run.store, INTEGRATION).len(), 2);
    // a's own checks still hold for its untouched worktree, so the report
    // counts both parts as implemented.
    let report = rounds[0].report.as_ref().unwrap();
    assert!(report.implemented, "{report:?}");
}

#[test]
fn a_conflict_beyond_the_tasks_authority_raises_a_decision() {
    let repo = repo();
    let run = conflicting(&repo, Live::merging());
    // b was already sent back as often as review allows.
    let tasks = run.store.tasks().unwrap();
    for n in 0..MAX_CORRECTIONS {
        let earlier = Review {
            attempt_id: format!("earlier-{n}"),
            assignment_id: "b".into(),
            outcome: ReviewOutcome::Correction,
            checkpoint: None,
            changed_files: Vec::new(),
            findings: Vec::new(),
            verifications: Vec::new(),
        };
        let event = NewEvent {
            id: format!("earlier-{n}"),
            task_id: "t".into(),
            assignment_id: Some("b".into()),
            attempt_id: None,
            kind: "assignment.reviewed".into(),
            payload: serde_json::to_value(&earlier).unwrap(),
        };
        tasks.append_event(&event, NOW).unwrap();
    }
    let records = run.integrate(NOW + 50);
    assert!(matches!(records[0].outcome, Outcome::Conflict { .. }));
    assert_eq!(run.status("b"), AssignmentState::Accepted);
    assert_eq!(run.task().stage, TaskStage::Integrating);
    let decision = tasks.decision("integration:t:1").unwrap().unwrap();
    assert_eq!(decision.state, DecisionState::Open);
    assert_eq!(decision.scope, DecisionScope::Task);
    assert_eq!(decision.asked_by, Requester::Coordinator);
    assert!(decision.question.contains("src/lib.rs"), "{decision:?}");
    let queued = tasks
        .pending_deliveries(crate::tasks::decisions::MONO_DECISION)
        .unwrap();
    assert_eq!(queued.len(), 1);
    // Nothing more happens until Mono answers.
    assert!(run.integrate(NOW + 60).is_empty());
    assert!(tasks.decision("integration:t:2").unwrap().is_none());
    // Mono answers without fixing anything: the answer is taken, and the
    // conflict is surfaced again as a new decision.
    tasks
        .answer_as(
            "integration:t:1",
            decision.revision,
            "Keep both",
            Requester::Mono,
            NOW + 70,
        )
        .unwrap();
    run.integrate(NOW + 80);
    let first = tasks.decision("integration:t:1").unwrap().unwrap();
    assert_eq!(first.state, DecisionState::Resolved);
    let second = tasks.decision("integration:t:2").unwrap().unwrap();
    assert_eq!(second.state, DecisionState::Open);
}

#[test]
fn merging_without_merge_authority_raises_a_decision_instead() {
    let repo = repo();
    let parts = vec![
        part("a", &["src/a.rs"], &[], "test -f src/a.rs"),
        part("b", &["src/b.rs"], &[], "test -f src/b.rs"),
    ];
    let run = started(&repo, Live::without_merge(), parts, &["test -f src/a.rs"]);
    assert_eq!(run.tick(NOW).launched.len(), 2);
    run.work("a", NOW + 10, |tree| {
        commit(tree, "src/a.rs", "pub fn a() {}\n")
    });
    run.work("b", NOW + 20, |tree| {
        commit(tree, "src/b.rs", "pub fn b() {}\n")
    });
    assert!(run
        .coordinator
        .verify(&run.store, NOW + 30)
        .unwrap()
        .is_empty());
    assert!(run.integrate(NOW + 40).is_empty());
    // Nothing was merged or created: no integration branch, no record.
    assert!(!ok(
        &repo.root,
        &["rev-parse", "--verify", "-q", &branch_for("t")]
    ));
    assert!(events(&run.store, INTEGRATION).is_empty());
    let tasks = run.store.tasks().unwrap();
    let decision = tasks.decision("integration:t:1").unwrap().unwrap();
    assert!(decision.question.contains("merge"), "{decision:?}");
    assert_eq!(run.task().stage, TaskStage::Integrating);

    // The user allows merging and applies it to the task; Mono answers.
    run.coordinator.policy().0.borrow_mut().merge = true;
    let revision = crate::tasks::policy::of(&tasks, "t")
        .unwrap()
        .unwrap()
        .revision;
    let live = run.coordinator.policy().0.borrow().clone();
    autonomy::apply(&run.store, "t", revision, &live, NOW + 50).unwrap();
    tasks
        .answer_as(
            "integration:t:1",
            1,
            "Merge authority is granted now.",
            Requester::Mono,
            NOW + 60,
        )
        .unwrap();
    let records = run.integrate(NOW + 70);
    assert_eq!(records[0].outcome, Outcome::Clean, "{records:?}");
    assert_eq!(run.task().stage, TaskStage::Verifying);
}

#[test]
fn dirty_worktrees_are_never_deleted_and_uncommitted_work_is_sent_back() {
    let repo = repo();
    let parts = vec![
        part("a", &["src/a.rs"], &[], "test -f src/a.rs"),
        part("b", &["src/b.rs"], &[], "test -f src/b.rs"),
    ];
    let run = started(&repo, Live::merging(), parts, &["test -f src/a.rs"]);
    assert_eq!(run.tick(NOW).launched.len(), 2);
    run.work("a", NOW + 10, |tree| {
        commit(tree, "src/a.rs", "pub fn a() {}\n")
    });
    // b never commits: its work is only in its worktree.
    let reviews = run.work("b", NOW + 20, |tree| {
        std::fs::write(tree.join("src/b.rs"), "pub fn b() {}\n").unwrap()
    });
    assert_eq!(reviews[0].outcome, ReviewOutcome::Accepted, "{reviews:?}");
    assert!(run
        .coordinator
        .verify(&run.store, NOW + 30)
        .unwrap()
        .is_empty());
    let records = run.integrate(NOW + 40);
    assert!(
        matches!(&records[0].outcome, Outcome::Uncommitted { assignment, files }
            if assignment == "b" && files == &["src/b.rs".to_owned()]),
        "{records:?}"
    );
    // Sent back to commit it; the dirty worktree and its work stay.
    assert_eq!(run.status("b"), AssignmentState::Queued);
    let tree = run.tree("b");
    assert_eq!(
        std::fs::read_to_string(tree.join("src/b.rs")).unwrap(),
        "pub fn b() {}\n"
    );
    let listed = git(&repo.root, &["worktree", "list", "--porcelain"]);
    for path in [tree, run.tree("a"), integration_tree(&run)] {
        assert!(
            listed.contains(&path.to_string_lossy().into_owned()),
            "{listed}"
        );
    }
    let correction = run
        .store
        .tasks()
        .unwrap()
        .latest_correction("t", "b")
        .unwrap()
        .unwrap();
    assert!(correction
        .findings
        .iter()
        .any(|f| matches!(f, Finding::UncommittedWork { .. })));
}
