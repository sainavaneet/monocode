//! Release acceptance (handoff "Release acceptance"): one task started from
//! a Mono chat's plan, driven through the entry points the app uses — the
//! plan card's start, the coordinator tick, the worker and Mono task
//! commands, the user's decision answer, recovery's turn end and the
//! restart startup — against a temporary Git repository and a file-backed
//! store. The fake launcher stands in for the app's session start, so no
//! provider runs. Each step asserts the state it persisted.
use super::coordinator::{lease_resource, Coordinator, ExplicitStart, LEASE_TTL_MS};
use super::decisions::tests::mono_received;
use super::decisions::ReplyMatch;
use super::recovery::tests::{accounts, allow, stop_all, Recorded};
use super::recovery::{self, turn_ended, Outcome, TurnEnd};
use super::review::tests::{git, open, repo, Repo, CHECK, NOW};
use super::store::tests::assignment;
use super::{
    confirm_started, handoff, inbox, policy, workspaces, AcceptanceCriterion, AssignmentState,
    AttemptState, DecisionState, Finding, NewTask, Requester, ReviewOutcome, RoundOutcome, Task,
    TaskError, TaskLifecycle, TaskStage,
};
use crate::app_access::{mono_task_action, worker_action};
use crate::orchestration::Plan;
use crate::store::Store;
use serde_json::{json, Value};
use std::path::PathBuf;

/// Unique among the crate's tests: live policies are a process-wide registry.
const MONO: &str = "mono-acceptance";
/// The criterion Mono wrote that the first implementation does not meet.
const CRITERION: &str = "grep -q fixed src/feature.rs";

/// The app as one process sees it: its store, coordinator and clock.
struct App {
    store: Store,
    coordinator: Coordinator,
    launcher: Recorded,
    now: u64,
}

impl App {
    fn tick(&mut self) -> super::coordinator::Tick {
        self.now += 1;
        let tick = self
            .coordinator
            .tick(&self.store, &self.launcher, self.now)
            .unwrap();
        assert!(tick.errors.is_empty(), "{tick:?}");
        tick
    }

    fn task(&self, id: &str) -> Task {
        self.store.tasks().unwrap().task(id).unwrap().unwrap()
    }

    fn status(&self, id: &str) -> AssignmentState {
        let tasks = self.store.tasks().unwrap();
        tasks.assignment(id, "a").unwrap().unwrap().status
    }

    fn attempts(&self, id: &str) -> Vec<super::Attempt> {
        self.store.tasks().unwrap().attempts(id, "a").unwrap()
    }

    fn events(&self, kind: &str) -> Vec<super::Event> {
        let all = self.store.tasks().unwrap().events_after(0, 10_000).unwrap();
        all.into_iter().filter(|e| e.kind == kind).collect()
    }

    fn brief(&self, session: i64) -> String {
        self.store.follow_ups(session).unwrap().draft.unwrap().text
    }

    /// The newest launch, confirmed started by the app.
    fn confirm_last(&mut self) -> i64 {
        let session = self.launcher.last().0.id;
        self.now += 1;
        assert!(confirm_started(&self.store, session, self.now).unwrap());
        session
    }

    fn worker(&mut self, session: i64, request: &str, action: &str, input: Value) -> Value {
        self.now += 1;
        worker_action(&self.store, session, request, action, &input, self.now).unwrap()
    }

    fn mono(&mut self, action: &str, input: Value) -> Result<Value, String> {
        self.now += 1;
        mono_task_action(&self.store, MONO, action, &input, self.now)
    }

    fn decision(&self, id: &str) -> super::DecisionRequest {
        self.store.tasks().unwrap().decision(id).unwrap().unwrap()
    }

    /// The app hands each answered decision to its worker once (the GUI's
    /// task pump does this through the worker's chat).
    fn hand_answers_to_workers(&mut self, expected: &str, session: i64) {
        let tasks = self.store.tasks().unwrap();
        let handoffs = tasks.pending_handoffs().unwrap();
        assert_eq!(handoffs.len(), 1, "{handoffs:?}");
        let handoff = &handoffs[0];
        assert_eq!(handoff.decision.id, expected);
        assert_eq!(
            handoff.session_id,
            self.store.persisted_session_id(session).unwrap()
        );
        let attempt = handoff.decision.attempt_id.clone().unwrap();
        self.now += 1;
        let revision = handoff.decision.revision;
        assert!(tasks
            .resolve_handoff(expected, revision, &attempt, "message", self.now)
            .unwrap());
        assert!(tasks.pending_handoffs().unwrap().is_empty());
    }
}

/// The app's first process. Its holder names a process that has exited, so
/// the next process can prove it gone after the "quit".
fn first_process(store: Store) -> App {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    App {
        store,
        coordinator: Coordinator::new(format!("monocode-native:{pid}:first"), ExplicitStart),
        launcher: Recorded::default(),
        now: NOW,
    }
}

/// Step 1: the user starts the plan Mono wrote in its chat. The plan card's
/// Start (`start_native_orchestration`) calls `policy::start_plan` with the
/// owning Mono; the task persists with its policy snapshot and criteria.
fn start_from_mono_chat(app: &mut App, repo: &Repo) -> (String, i64) {
    let project = repo.root.to_string_lossy().into_owned();
    let chat = app.store.create("codex", &project).unwrap().id;
    let plan = Plan {
        title: "Add the feature".into(),
        summary: "One worker in its own worktree".into(),
        tasks: vec![crate::tasks::Assignment {
            verification: vec![CHECK.into()],
            ..assignment("a", &[])
        }],
    };
    let mut criteria = policy::plan_criteria(&plan);
    criteria.push(AcceptanceCriterion {
        id: "fixed".into(),
        description: "The feature is marked fixed".into(),
        required: true,
        checks: vec![CRITERION.into()],
    });
    let id = format!("{}1", policy::plan_task_prefix(chat));
    let new = NewTask {
        id: id.clone(),
        mono_id: Some(MONO.into()),
        project,
        title: "Add the feature".into(),
        request: "Add the feature and mark it fixed".into(),
        criteria,
    };
    let task =
        policy::start_plan(&app.store, &new, &plan, "codex", Requester::User, app.now).unwrap();
    assert_eq!(
        (task.stage, task.lifecycle),
        (TaskStage::Implementing, TaskLifecycle::Active)
    );
    assert_eq!(task.mono_id.as_deref(), Some(MONO));
    assert_eq!(task.criteria.len(), 2);
    let started = policy::started_plans(&app.store, chat).unwrap();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].id, id);
    let tasks = app.store.tasks().unwrap();
    let snapshot = policy::of(&tasks, &id).unwrap().unwrap();
    assert_eq!(snapshot.start_account, "default");
    assert!(!snapshot.use_api_key);
    assert_eq!(app.status(&id), AssignmentState::Queued);
    (id, chat)
}

/// Step 2: the coordinator creates the dedicated worktree and launches the
/// worker into it. Returns the worker's session and worktree.
fn launch_into_worktree(app: &mut App, repo: &Repo, id: &str) -> (i64, PathBuf) {
    let head = git(&repo.root, &["rev-parse", "HEAD"]);
    let tick = app.tick();
    assert_eq!(tick.launched.len(), 1, "{tick:?}");
    let tasks = app.store.tasks().unwrap();
    let recorded = tasks.workspaces(id).unwrap();
    assert_eq!(recorded.len(), 1);
    let workspace = &recorded[0];
    assert_eq!(
        workspace.mode,
        crate::orchestration::WorkspaceMode::Worktree
    );
    assert_eq!(workspace.base_commit.as_deref(), Some(head.as_str()));
    assert_eq!(
        workspace.branch.as_deref(),
        Some(workspaces::branch_for(id, "a").as_str())
    );
    assert_eq!(workspace.owner, format!("task:{id}"));
    let (_, settings) = app.launcher.last();
    assert_eq!(
        settings.worktree_cwd.as_deref(),
        Some(workspace.path.as_str())
    );
    assert_eq!(settings.account.id, "default");
    assert!(!settings.use_api_key);
    // The user's checkout is untouched; the worktree is listed beside it.
    assert_eq!(git(&repo.root, &["branch", "--show-current"]), "main");
    assert_eq!(git(&repo.root, &["worktree", "list"]).lines().count(), 2);
    let session = app.confirm_last();
    assert_eq!(app.attempts(id)[0].status, AttemptState::Running);
    assert_eq!(app.status(id), AssignmentState::Running);
    (session, PathBuf::from(&workspace.path))
}

/// Matrix: another app sharing the database cannot take a live lease.
fn competing_app_is_refused(app: &App, repo: &Repo, id: &str) {
    let theirs = Store::open(&repo.db).unwrap();
    let launcher = Recorded::default();
    let desktop = Coordinator::new("monocode-desktop", ExplicitStart);
    let tick = desktop.tick(&theirs, &launcher, app.now + 1).unwrap();
    assert_eq!(tick.leased_elsewhere, vec![id.to_owned()]);
    assert_eq!(launcher.count(), 0);
    let taken = theirs.tasks().unwrap().acquire_lease(
        &lease_resource(id),
        "monocode-desktop",
        LEASE_TTL_MS,
        app.now + 1,
    );
    assert!(
        matches!(taken, Err(TaskError::LeaseHeld { .. })),
        "{taken:?}"
    );
}

/// Step 3: a routine question goes to Mono, Mono answers it through its
/// command, the answer reaches the worker once and the work resumes.
fn routine_question_answered_by_mono(app: &mut App, id: &str, worker: i64, chat: i64) {
    let asked = app.worker(
        worker,
        "q-parser",
        "tasks.worker.ask",
        json!({"question": "Which parser should I use?", "context": "nom or winnow",
               "scope": "assignment"}),
    );
    let question = asked["decision"]["id"].as_str().unwrap().to_owned();
    assert_eq!(app.status(id), AssignmentState::WaitingForDecision);
    assert_eq!(app.attempts(id)[0].status, AttemptState::Waiting);
    // Mono's inbox carries it to Mono's chat once.
    let batch = inbox::next_batch(&app.store, MONO, app.now)
        .unwrap()
        .unwrap();
    assert!(batch.message.contains(&question), "{}", batch.message);
    assert!(batch.message.contains("Which parser should I use?"));
    mono_received(&app.store, chat, &batch);
    let acked = inbox::acknowledge_delivered(&app.store, MONO, chat, app.now).unwrap();
    assert_eq!(acked, vec![batch.id]);
    let revision = app.decision(&question).revision;
    app.mono(
        "tasks.decisions.answer",
        json!({"decisionId": question, "revision": revision, "answer": "Use nom"}),
    )
    .unwrap();
    let answered = app.decision(&question);
    assert_eq!(answered.state, DecisionState::Answered);
    assert_eq!(answered.answered_by, Some(Requester::Mono));
    app.hand_answers_to_workers(&question, worker);
    assert_eq!(app.decision(&question).state, DecisionState::Resolved);
    assert_eq!(app.status(id), AssignmentState::Running);
    assert_eq!(app.attempts(id)[0].status, AttemptState::Running);
}

/// Step 4: Mono escalates a question to the user. It waits with no timeout
/// and no inferred answer, Mono cannot answer for the user, and the user's
/// reply resumes the worker.
fn escalated_question_waits_for_the_user(app: &mut App, id: &str, worker: i64) {
    let asked = app.worker(
        worker,
        "q-api",
        "tasks.worker.ask",
        json!({"question": "Change the public API?", "scope": "assignment"}),
    );
    let question = asked["decision"]["id"].as_str().unwrap().to_owned();
    let revision = app.decision(&question).revision;
    let escalated = app
        .mono(
            "tasks.decisions.escalate",
            json!({"decisionId": question, "revision": revision,
                   "why": "It changes the public API",
                   "recommendation": "Keep the old API",
                   "alternatives": ["Break the API"]}),
        )
        .unwrap();
    assert_eq!(escalated["card"]["recommendation"], "Keep the old API");
    let revision = app.decision(&question).revision;
    let refused = app.mono(
        "tasks.decisions.answer",
        json!({"decisionId": question, "revision": revision, "answer": "Break it"}),
    );
    assert!(refused.unwrap_err().contains("user"));
    // Thirty days of ticks: still open, still waiting, nothing handed over.
    for _ in 0..3 {
        app.now += 10 * 24 * 3_600_000;
        app.tick();
        assert_eq!(app.decision(&question).state, DecisionState::Open);
        assert_eq!(app.status(id), AssignmentState::WaitingForDecision);
    }
    let tasks = app.store.tasks().unwrap();
    assert!(tasks.pending_handoffs().unwrap().is_empty());
    assert_eq!(tasks.user_decisions(MONO).unwrap().len(), 1);
    // The user's reply in Mono's chat names the recommendation.
    let ReplyMatch::Resolves {
        decision,
        revision,
        answer,
    } = tasks.match_user_reply(MONO, "Keep the old API").unwrap()
    else {
        panic!("the reply should answer the open decision");
    };
    assert_eq!(decision, question);
    app.now += 1;
    tasks
        .answer_as(&decision, revision, &answer, Requester::User, app.now)
        .unwrap();
    assert_eq!(app.decision(&question).answered_by, Some(Requester::User));
    app.hand_answers_to_workers(&question, worker);
    assert_eq!(app.status(id), AssignmentState::Running);
    assert_eq!(app.attempts(id)[0].status, AttemptState::Running);
}

/// Step 5: the worker's account runs out of usage. Recovery cools it down
/// and replaces the worker on the next allowed account, in the same
/// worktree, with a durable hand-off and one retry spent. Meanwhile the
/// user flips the API key preference; the task keeps its snapshot.
fn account_failure_recovers_on_another_account(
    app: &mut App,
    id: &str,
    worker: i64,
    tree: &std::path::Path,
) -> i64 {
    app.store
        .save_ui_state(r#"{"preferences":{"api_key_enabled":{"codex":true}}}"#)
        .unwrap();
    assert!(policy::live_api_key(
        &app.store,
        crate::provider::Kind::Codex
    ));
    let end = TurnEnd {
        error: Some("Usage limit reached".into()),
        usage_limited: true,
        resets_at: Some(app.now + 3_600_000),
        ..TurnEnd::default()
    };
    app.now += 1;
    let outcome = turn_ended(&app.store, worker, &end, app.now).unwrap();
    assert!(
        matches!(&outcome, Outcome::Retry { account, .. } if account == "account-b"),
        "{outcome:?}"
    );
    let first = app.attempts(id)[0].clone();
    assert_eq!(first.status, AttemptState::Interrupted);
    assert_eq!(app.status(id), AssignmentState::Running, "progress kept");
    assert_eq!(stop_all(&app.store, app.now), 1);
    let tick = app.tick();
    assert_eq!(tick.launched.len(), 1, "{tick:?}");
    let (_, settings) = app.launcher.last();
    assert_eq!(settings.account.id, "account-b");
    assert!(
        !settings.use_api_key,
        "the snapshot, not the new preference"
    );
    assert_eq!(settings.worktree_cwd.as_deref(), tree.to_str());
    let attempts = app.attempts(id);
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[1].account_id.as_deref(), Some("account-b"));
    let tasks = app.store.tasks().unwrap();
    let row = recovery::recovery(&tasks, &first.id).unwrap().unwrap();
    assert_eq!(
        (row.root_attempt.as_str(), row.retries_used),
        (first.id.as_str(), 1)
    );
    assert_eq!(row.failed_account.as_deref(), Some("default"));
    let handed = handoff::of(&tasks, &attempts[1].id).unwrap().unwrap();
    assert_eq!(handed.from_attempt, first.id);
    assert_eq!(handed.workspace.path, tree.to_string_lossy());
    let answers: Vec<_> = handed.decisions.iter().map(|d| d.answer.clone()).collect();
    assert!(answers.contains(&Some("Use nom".into())), "{answers:?}");
    assert!(
        answers.contains(&Some("Keep the old API".into())),
        "{answers:?}"
    );
    assert!(handed.pending_questions.is_empty());
    assert_eq!(tasks.workspaces(id).unwrap().len(), 1);
    app.confirm_last()
}

/// Step 6: the worker commits work that passes its assignment check but not
/// Mono's criterion. Review accepts, verification sends it back with the
/// failed criterion, and the coordinator relaunches with it in the brief.
fn failed_check_produces_a_correction(
    app: &mut App,
    id: &str,
    worker: i64,
    tree: &std::path::Path,
) -> i64 {
    std::fs::write(tree.join("src/feature.rs"), "pub fn feature() {}\n").unwrap();
    git(tree, &["add", "."]);
    git(tree, &["commit", "-q", "-m", "Add the feature"]);
    let claim = json!({"outcome": "succeeded", "summary": "Added the feature",
                       "validation": [format!("{CHECK} passed")]});
    app.worker(worker, "done-1", "tasks.worker.result", claim);
    let tick = app.tick();
    assert_eq!(tick.reviews.len(), 1, "{tick:?}");
    // The worker that reported is stopped; review waits for its turn to end.
    assert!(app
        .coordinator
        .review(&app.store, app.now)
        .unwrap()
        .is_empty());
    assert_eq!(stop_all(&app.store, app.now), 1);
    app.now += 1;
    let reviews = app.coordinator.review(&app.store, app.now).unwrap();
    assert_eq!(reviews[0].outcome, ReviewOutcome::Accepted, "{reviews:?}");
    app.now += 1;
    let rounds = app.coordinator.verify(&app.store, app.now).unwrap();
    assert_eq!(rounds[0].outcome, RoundOutcome::Correction, "{rounds:?}");
    assert!(
        rounds[0].findings.iter().any(|f| matches!(
            f,
            Finding::CriterionNotMet { criterion, exit_code: Some(1), .. } if criterion == "fixed"
        )),
        "{rounds:?}"
    );
    assert_eq!(app.status(id), AssignmentState::Queued);
    assert_eq!(app.task(id).stage, TaskStage::Implementing);
    let tick = app.tick();
    assert_eq!(tick.launched.len(), 1, "{tick:?}");
    let session = app.launcher.last().0.id;
    let brief = app.brief(session);
    assert!(brief.contains("Acceptance criterion fixed"), "{brief}");
    app.confirm_last()
}

/// Step 7: the app quits while the correcting worker runs, and the next
/// process starts. Its startup proves the old holder gone, marks the
/// running attempt Interrupted, keeps every record and continues the work
/// with a replacement that still carries the correction.
fn restart_preserves_state(app: App, repo: &Repo, id: &str) -> App {
    let before = app.task(id);
    let running = app.attempts(id).last().unwrap().clone();
    assert_eq!(running.status, AttemptState::Running);
    let events = app.store.tasks().unwrap().events_after(0, 10_000).unwrap();
    let now = app.now + LEASE_TTL_MS + 1_000;
    drop(app);

    let store = Store::open(&repo.db).unwrap();
    let holder = format!("monocode-native:{}:second", std::process::id());
    let mut app = App {
        store,
        coordinator: Coordinator::new(holder, ExplicitStart),
        launcher: Recorded::default(),
        now,
    };
    let startup = app.coordinator.startup(&app.store, app.now).unwrap();
    assert_eq!(startup.reconciled, vec![id.to_owned()], "{startup:?}");
    assert_eq!(startup.interrupted, vec![running.id.clone()], "{startup:?}");
    assert!(
        startup.held.is_empty() && startup.missing.is_empty(),
        "{startup:?}"
    );
    let after = app.task(id);
    assert_eq!(
        (after.stage, after.lifecycle),
        (before.stage, before.lifecycle)
    );
    assert_eq!(after.criteria, before.criteria);
    let tasks = app.store.tasks().unwrap();
    let kept = tasks.events_after(0, 10_000).unwrap();
    assert!(kept.len() >= events.len());
    assert!(kept.iter().zip(&events).all(|(a, b)| a.id == b.id));
    assert_eq!(tasks.workspaces(id).unwrap().len(), 1);
    let row = recovery::recovery(&tasks, &running.id).unwrap().unwrap();
    assert_eq!(row.failure, super::restart::RESTART);
    let tick = app.tick();
    assert_eq!(tick.launched.len(), 1, "{tick:?}");
    let replacement = app.attempts(id).last().unwrap().clone();
    let tasks = app.store.tasks().unwrap();
    let handed = handoff::of(&tasks, &replacement.id).unwrap().unwrap();
    assert_eq!(handed.from_attempt, running.id);
    let session = app.launcher.last().0.id;
    let brief = app.brief(session);
    assert!(brief.contains("Acceptance criterion fixed"), "{brief}");
    app.confirm_last();
    // Only the replacement started: another pass launches nothing more.
    let again = app.tick();
    assert!(again.launched.is_empty(), "{again:?}");
    assert_eq!(app.launcher.count(), 1);
    app
}

/// Step 8: the correction is reviewed, the criteria pass and the task
/// completes with a report carrying the workspace and the test evidence.
fn correction_verified_and_reported(app: &mut App, id: &str, tree: &std::path::Path) {
    let worker = app.launcher.last().0.id;
    std::fs::write(
        tree.join("src/feature.rs"),
        "pub fn feature() {} // fixed\n",
    )
    .unwrap();
    git(tree, &["commit", "-q", "-am", "Mark the feature fixed"]);
    let claim = json!({"outcome": "succeeded", "summary": "Marked it fixed",
                       "validation": [format!("{CRITERION} passed")]});
    app.worker(worker, "done-2", "tasks.worker.result", claim);
    let tick = app.tick();
    assert_eq!(tick.reviews.len(), 1, "{tick:?}");
    // The worker that reported is stopped; review waits for its turn to end.
    assert!(app
        .coordinator
        .review(&app.store, app.now)
        .unwrap()
        .is_empty());
    assert_eq!(stop_all(&app.store, app.now), 1);
    app.now += 1;
    let reviews = app.coordinator.review(&app.store, app.now).unwrap();
    assert_eq!(reviews[0].outcome, ReviewOutcome::Accepted, "{reviews:?}");
    app.now += 1;
    let rounds = app.coordinator.verify(&app.store, app.now).unwrap();
    assert_eq!(rounds[0].outcome, RoundOutcome::Completed, "{rounds:?}");
    let task = app.task(id);
    assert_eq!(
        (task.stage, task.lifecycle),
        (TaskStage::Completed, TaskLifecycle::Completed)
    );
    let report = rounds[0].report.as_ref().unwrap();
    assert!(report.implemented && report.committed, "{report:?}");
    assert!(
        !report.merged && !report.pushed && !report.deployed,
        "{report:?}"
    );
    let work = &report.assignments[0];
    assert_eq!(work.path.as_deref(), tree.to_str());
    assert_eq!(
        work.branch.as_deref(),
        Some(workspaces::branch_for(id, "a").as_str())
    );
    assert_eq!(work.commits.len(), 2, "{work:?}");
    assert!(
        !work.uncommitted && !work.verifications.is_empty(),
        "{work:?}"
    );
    assert_eq!(report.criteria.len(), 2);
    for criterion in &report.criteria {
        assert!(
            criterion.satisfied && criterion.checkpoint.is_some(),
            "{criterion:?}"
        );
    }
    let tasks = app.store.tasks().unwrap();
    let evidence: Vec<_> = tasks
        .verifications(id)
        .unwrap()
        .into_iter()
        .filter(|v| v.criterion_id.as_deref() == Some("fixed") && !v.invalidated)
        .collect();
    assert!(evidence.iter().all(|v| v.passed), "{evidence:?}");
    assert!(!evidence.is_empty(), "the criterion has no verification");
    let recorded: Value = serde_json::from_str(&evidence[0].evidence).unwrap();
    assert_eq!(
        recorded["argv"],
        json!(["grep", "-q", "fixed", "src/feature.rs"])
    );
    assert_eq!(recorded["exitCode"], 0);
    assert_eq!(recorded["path"], tree.to_string_lossy().as_ref());
    assert_eq!(app.events("task.final_report").len(), 1);
}

#[test]
fn a_task_from_a_mono_chat_survives_questions_failures_corrections_and_a_restart() {
    let repo = repo();
    let store = open(&repo);
    accounts(&store, &["account-b"]);
    allow(&store, &repo, &[("default", None), ("account-b", None)]);
    let mut app = first_process(store);

    let (id, chat) = start_from_mono_chat(&mut app, &repo);
    let (worker, tree) = launch_into_worktree(&mut app, &repo, &id);
    competing_app_is_refused(&app, &repo, &id);
    routine_question_answered_by_mono(&mut app, &id, worker, chat);
    escalated_question_waits_for_the_user(&mut app, &id, worker);
    let worker = account_failure_recovers_on_another_account(&mut app, &id, worker, &tree);
    failed_check_produces_a_correction(&mut app, &id, worker, &tree);
    let mut app = restart_preserves_state(app, &repo, &id);
    correction_verified_and_reported(&mut app, &id, &tree);

    // The finished task and its report are still there after another start.
    drop(app);
    let store = Store::open(&repo.db).unwrap();
    let tasks = store.tasks().unwrap();
    let task = tasks.task(&id).unwrap().unwrap();
    assert_eq!(task.lifecycle, TaskLifecycle::Completed);
    let all = tasks.events_after(0, 10_000).unwrap();
    assert_eq!(
        all.iter().filter(|e| e.kind == "task.final_report").count(),
        1
    );
}
