//! Worker questions routed to the owning Mono, Mono's answers and escalations,
//! user replies, and exactly-once hand-off of each answer to its worker.
use crate::agent::{Question, QuestionItem, QuestionOption};
use crate::app_access::{mono_task_action, worker_action};
use crate::store::Store;
use crate::tasks::coordinator::{Coordinator, ExplicitStart, FLAG};
use crate::tasks::review::tests::FakeLauncher;
use crate::tasks::store::tests::{assignment, plan};
use crate::tasks::{decisions, inbox};
use crate::tasks::{
    AssignmentState, AttemptState, DecisionState, DeliveryState, NewAttempt, NewTask, Requester,
    State, Subject, TaskError, TaskStage,
};
use serde_json::{json, Value};
use std::path::Path;

pub(super) const NOW: u64 = 1_000;

/// Task `t` (owned by `mono-1`) Implementing, with `assignments` all Queued.
pub(super) fn task_with(store: &Store, project: &str, assignments: Vec<crate::tasks::Assignment>) {
    store.save_setting(FLAG, "enabled").unwrap();
    let tasks = store.tasks().unwrap();
    tasks
        .create_task(
            &NewTask {
                id: "t".into(),
                mono_id: Some("mono-1".into()),
                project: project.into(),
                title: "Ship it".into(),
                request: "Implement the feature".into(),
                criteria: vec![],
            },
            NOW,
        )
        .unwrap();
    tasks.save_plan("t", 1, &plan(assignments), NOW).unwrap();
    let t = Subject::Task("t");
    for stage in [TaskStage::Ready, TaskStage::Implementing] {
        let rev = tasks.revision(t).unwrap();
        tasks
            .transition(t, State::Stage(stage), Requester::Mono, rev, NOW)
            .unwrap();
    }
}

pub(super) fn independent(id: &str) -> crate::tasks::Assignment {
    crate::tasks::Assignment {
        files: vec![format!("src/{id}")],
        ..assignment(id, &[])
    }
}

/// Runs assignment `a` with attempt `x` bound to a new worker session.
pub(super) fn running(store: &Store, assignment: &str, attempt: &str) -> i64 {
    let session = store.create("codex", "/project").unwrap().id;
    let tasks = store.tasks().unwrap();
    let a = Subject::Assignment {
        task: "t",
        id: assignment,
    };
    for to in [AssignmentState::Ready, AssignmentState::Running] {
        let rev = tasks.revision(a).unwrap();
        tasks
            .transition(a, State::Assignment(to), Requester::Coordinator, rev, NOW)
            .unwrap();
    }
    tasks
        .create_attempt(
            &NewAttempt {
                id: attempt.into(),
                task_id: "t".into(),
                assignment_id: assignment.into(),
                operation_id: format!("launch-{attempt}"),
                provider: "codex".into(),
                account_id: None,
            },
            NOW,
        )
        .unwrap();
    let x = Subject::Attempt(attempt);
    for to in [AttemptState::Starting, AttemptState::Running] {
        let rev = tasks.revision(x).unwrap();
        tasks
            .transition(x, State::Attempt(to), Requester::Coordinator, rev, NOW)
            .unwrap();
    }
    let rev = tasks.revision(x).unwrap();
    let bound = store.persisted_session_id(session).unwrap();
    tasks
        .bind_attempt_session(attempt, rev, &bound, NOW)
        .unwrap();
    session
}

pub(super) fn ask(
    store: &Store,
    session: i64,
    request: &str,
    question: &str,
    scope: &str,
) -> String {
    let asked = worker_action(
        store,
        session,
        request,
        "tasks.worker.ask",
        &json!({"question": question, "context": "Two parsers fit", "scope": scope}),
        NOW + 1,
    )
    .unwrap();
    asked["decision"]["id"].as_str().unwrap().to_owned()
}

pub(super) fn mono(store: &Store, action: &str, input: Value) -> Result<Value, String> {
    mono_task_action(store, "mono-1", action, &input, NOW + 2)
}

pub(super) fn decision(store: &Store, id: &str) -> crate::tasks::DecisionRequest {
    store.tasks().unwrap().decision(id).unwrap().unwrap()
}

pub(super) fn answer(store: &Store, id: &str, text: &str) {
    let revision = decision(store, id).revision;
    mono(
        store,
        "tasks.decisions.answer",
        json!({"decisionId": id, "revision": revision, "answer": text}),
    )
    .unwrap();
}

fn escalate(store: &Store, id: &str) -> Value {
    let revision = decision(store, id).revision;
    mono(
        store,
        "tasks.decisions.escalate",
        json!({"decisionId": id, "revision": revision,
               "why": "It changes the public API",
               "recommendation": "Keep the old API",
               "alternatives": ["Break the API", "Add a new API beside it"]}),
    )
    .unwrap()["card"]
        .clone()
}

pub(super) fn status(store: &Store, assignment: &str) -> AssignmentState {
    let tasks = store.tasks().unwrap();
    tasks.assignment("t", assignment).unwrap().unwrap().status
}

fn attempt_status(store: &Store, id: &str) -> AttemptState {
    store.tasks().unwrap().attempt(id).unwrap().unwrap().status
}

pub(super) fn events(store: &Store, kind: &str) -> Vec<crate::tasks::Event> {
    let all = store.tasks().unwrap().events_after(0, 10_000).unwrap();
    all.into_iter().filter(|e| e.kind == kind).collect()
}

/// Mono's chat turn carrying `batch` was persisted.
fn mono_received(store: &Store, mono_session: i64, batch: &inbox::Batch) {
    store
        .save_setting(&format!("mono-delivered:{mono_session}:{}", batch.id), "1")
        .unwrap();
}

#[test]
fn mono_answers_a_routine_question_and_the_worker_resumes() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    task_with(&store, "/project", vec![independent("a")]);
    let worker = running(&store, "a", "x");
    let chat = store.create("codex", "/home").unwrap().id;
    let id = ask(
        &store,
        worker,
        "q1",
        "Which parser should I use?",
        "assignment",
    );
    assert_eq!(status(&store, "a"), AssignmentState::WaitingForDecision);

    // One event for the owning Mono, carrying the context it needs.
    let batch = inbox::next_batch(&store, "mono-1", NOW + 2)
        .unwrap()
        .unwrap();
    for needle in [
        id.as_str(),
        "Which parser should I use?",
        "Two parsers fit",
        "Ship it",
        "Assignment a",
        "tasks.decisions.answer",
        "tasks.decisions.escalate",
    ] {
        assert!(
            batch.message.contains(needle),
            "{needle}: {}",
            batch.message
        );
    }
    assert!(inbox::next_batch(&store, "mono-2", NOW + 2)
        .unwrap()
        .is_none());
    // Until Mono's turn has it, the same batch comes back: never a second.
    let again = inbox::next_batch(&store, "mono-1", NOW + 3)
        .unwrap()
        .unwrap();
    assert_eq!(again.id, batch.id);
    assert!(
        inbox::acknowledge_delivered(&store, "mono-1", chat, NOW + 3)
            .unwrap()
            .is_empty()
    );
    mono_received(&store, chat, &batch);
    assert_eq!(
        inbox::acknowledge_delivered(&store, "mono-1", chat, NOW + 4).unwrap(),
        vec![batch.id.clone()]
    );
    let tasks = store.tasks().unwrap();
    let delivery = tasks.delivery(&batch.deliveries[0]).unwrap().unwrap();
    assert_eq!(delivery.state, DeliveryState::Acknowledged);
    assert!(inbox::next_batch(&store, "mono-1", NOW + 5)
        .unwrap()
        .is_none());

    // Mono answers; the answer is handed to the asking worker exactly once.
    answer(&store, &id, "Use nom");
    let answered = decision(&store, &id);
    assert_eq!(answered.state, DecisionState::Answered);
    assert_eq!(answered.answered_by, Some(Requester::Mono));
    let handoffs = tasks.pending_handoffs().unwrap();
    assert_eq!(handoffs.len(), 1);
    assert_eq!(handoffs[0].decision.id, id);
    assert_eq!(
        handoffs[0].session_id,
        store.persisted_session_id(worker).unwrap()
    );
    assert!(handoffs[0].question.is_none());
    let message = decisions::answer_message(&handoffs[0].decision);
    assert!(
        message.contains("Use nom") && message.contains(&id),
        "{message}"
    );
    let revision = handoffs[0].decision.revision;
    assert!(tasks
        .resolve_handoff(&id, revision, "x", "message", NOW + 6)
        .unwrap());
    assert!(!tasks
        .resolve_handoff(&id, revision, "x", "message", NOW + 7)
        .unwrap());
    assert_eq!(decision(&store, &id).state, DecisionState::Resolved);
    assert_eq!(events(&store, "decision.resolved").len(), 1);
    assert_eq!(attempt_status(&store, "x"), AttemptState::Running);
    assert_eq!(status(&store, "a"), AssignmentState::Running);
    assert!(tasks.pending_handoffs().unwrap().is_empty());
}

#[test]
fn a_user_question_stays_waiting_across_a_simulated_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("monocode.db");
    let id = {
        let store = Store::open(&db).unwrap();
        task_with(&store, "/project", vec![independent("a"), independent("b")]);
        let worker = running(&store, "a", "x");
        let id = ask(&store, worker, "q1", "Change the public API?", "assignment");
        let card = escalate(&store, &id);
        assert_eq!(card["type"], "decision");
        assert_eq!(card["decisionId"], id.as_str());
        assert_eq!(card["question"], "Change the public API?");
        assert_eq!(card["why"], "It changes the public API");
        assert_eq!(card["recommendation"], "Keep the old API");
        assert_eq!(card["alternatives"][1], "Add a new API beside it");
        let paused = card["paused"].as_str().unwrap();
        let continues = card["continues"].as_str().unwrap();
        assert!(paused.contains("Assignment a"), "{paused}");
        assert!(continues.contains("Assignment b"), "{continues}");
        // Mono cannot answer for the user once it asked them.
        let revision = decision(&store, &id).revision;
        let refused = mono(
            &store,
            "tasks.decisions.answer",
            json!({"decisionId": id, "revision": revision, "answer": "Break it"}),
        );
        assert!(refused.unwrap_err().contains("user"));
        id
    };
    // Restart: everything is still waiting, and no time produces an answer.
    let store = Store::open(&db).unwrap();
    let launcher = FakeLauncher::default();
    let coordinator = Coordinator::new("gpui-2", ExplicitStart);
    for later in [NOW + 60_000, NOW + 30 * 24 * 3_600_000] {
        coordinator.tick(&store, &launcher, later).unwrap();
        assert_eq!(decision(&store, &id).state, DecisionState::Open);
        assert_eq!(status(&store, "a"), AssignmentState::WaitingForDecision);
    }
    let tasks = store.tasks().unwrap();
    assert!(tasks.pending_handoffs().unwrap().is_empty());
    let open = tasks.user_decisions("mono-1").unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].1["recommendation"], "Keep the old API");
    // An unclear reply asks again; a listed alternative answers.
    assert_eq!(
        tasks
            .match_user_reply("mono-1", "hmm, what do you think?")
            .unwrap(),
        decisions::ReplyMatch::Unclear(vec![id.clone()])
    );
    let matched = tasks
        .match_user_reply("mono-1", "  keep the old api ")
        .unwrap();
    let decisions::ReplyMatch::Resolves {
        decision: matched_id,
        revision,
        answer,
    } = matched
    else {
        panic!("{matched:?}")
    };
    assert_eq!(
        (matched_id.as_str(), answer.as_str()),
        (id.as_str(), "Keep the old API")
    );
    tasks
        .answer_as(&id, revision, &answer, Requester::User, NOW + 10)
        .unwrap();
    assert_eq!(decision(&store, &id).answered_by, Some(Requester::User));
    assert_eq!(tasks.pending_handoffs().unwrap().len(), 1);
}

#[test]
fn several_questions_route_to_the_right_workers() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    task_with(&store, "/project", vec![independent("a"), independent("b")]);
    let first = running(&store, "a", "x");
    let second = running(&store, "b", "y");
    let qa = ask(&store, first, "q1", "Question from a", "assignment");
    let qb = ask(&store, second, "q1", "Question from b", "assignment");
    assert_ne!(qa, qb);
    let batch = inbox::next_batch(&store, "mono-1", NOW + 2)
        .unwrap()
        .unwrap();
    assert_eq!(batch.deliveries.len(), 2);
    assert!(batch.message.contains("Question from a") && batch.message.contains("Question from b"));
    answer(&store, &qb, "Answer for b");
    answer(&store, &qa, "Answer for a");
    let tasks = store.tasks().unwrap();
    let handoffs = tasks.pending_handoffs().unwrap();
    assert_eq!(handoffs.len(), 2);
    for handoff in &handoffs {
        let (session, attempt, text) = if handoff.decision.id == qa {
            (first, "x", "Answer for a")
        } else {
            (second, "y", "Answer for b")
        };
        assert_eq!(
            handoff.session_id,
            store.persisted_session_id(session).unwrap()
        );
        assert_eq!(handoff.decision.attempt_id.as_deref(), Some(attempt));
        assert_eq!(handoff.decision.answer.as_deref(), Some(text));
        // The wrong worker cannot take another's answer.
        let other = if attempt == "x" { "y" } else { "x" };
        let revision = handoff.decision.revision;
        assert!(!tasks
            .resolve_handoff(&handoff.decision.id, revision, other, "message", NOW + 3)
            .unwrap());
    }
    let revision = decision(&store, &qa).revision;
    assert!(tasks
        .resolve_handoff(&qa, revision, "x", "message", NOW + 4)
        .unwrap());
    assert_eq!(status(&store, "a"), AssignmentState::Running);
    assert_eq!(status(&store, "b"), AssignmentState::WaitingForDecision);
}

#[test]
fn a_stale_answer_cannot_resolve_a_superseded_request() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    task_with(&store, "/project", vec![independent("a"), independent("b")]);
    let worker = running(&store, "a", "x");
    let id = ask(&store, worker, "q1", "Change the API?", "assignment");
    let asked = decision(&store, &id).revision;
    escalate(&store, &id);
    let escalated = decision(&store, &id);
    assert!(escalated.revision > asked);
    let tasks = store.tasks().unwrap();
    // Read before the escalation: stale.
    let stale = tasks.answer_as(&id, asked, "Break it", Requester::User, NOW + 3);
    assert!(matches!(stale, Err(TaskError::Stale { .. })), "{stale:?}");
    // The revision history is recorded, not rewritten.
    assert_eq!(events(&store, "decision.requested").len(), 1);
    assert_eq!(events(&store, "decision.escalated").len(), 1);
    // The assignment is cancelled: the request is superseded.
    let a = Subject::Assignment { task: "t", id: "a" };
    let rev = tasks.revision(a).unwrap();
    let cancelled = State::Assignment(AssignmentState::Cancelled);
    tasks
        .transition(a, cancelled, Requester::Mono, rev, NOW + 4)
        .unwrap();
    let superseded = tasks.answer_as(
        &id,
        escalated.revision,
        "Break it",
        Requester::User,
        NOW + 5,
    );
    assert!(
        matches!(&superseded, Err(TaskError::Invalid(m)) if m.contains("superseded")),
        "{superseded:?}"
    );
    assert_eq!(decision(&store, &id).state, DecisionState::Open);

    // An answered request cannot be answered again, and a hand-off read
    // before a newer revision does nothing.
    let other = running(&store, "b", "y");
    let second = ask(&store, other, "q2", "Which name?", "assignment");
    answer(&store, &second, "Call it parse");
    let revision = decision(&store, &second).revision;
    let again = tasks.answer_as(&second, revision, "Other", Requester::Mono, NOW + 6);
    assert!(
        matches!(again, Err(TaskError::InvalidState { .. })),
        "{again:?}"
    );
    assert!(!tasks
        .resolve_handoff(&second, revision - 1, "y", "message", NOW + 7)
        .unwrap());
    assert_eq!(decision(&store, &second).state, DecisionState::Answered);
}

#[test]
fn a_succeeded_result_waits_for_its_decisions_and_a_claim_ends_questions() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    task_with(&store, "/project", vec![independent("a")]);
    let worker = running(&store, "a", "x");
    let id = ask(&store, worker, "q1", "Which parser?", "assignment");
    let result = |request: &str| {
        worker_action(
            &store,
            worker,
            request,
            "tasks.worker.result",
            &json!({"outcome": "succeeded", "summary": "Done"}),
            NOW + 5,
        )
    };
    assert!(result("r1").unwrap_err().contains(&id));
    answer(&store, &id, "Use nom");
    // Answered is not yet delivered: still refused.
    assert!(result("r2").unwrap_err().contains(&id));
    let tasks = store.tasks().unwrap();
    assert!(tasks.claim("x").unwrap().is_none());
    let revision = decision(&store, &id).revision;
    assert!(tasks
        .resolve_handoff(&id, revision, "x", "message", NOW + 6)
        .unwrap());
    result("r3").unwrap();
    assert_eq!(status(&store, "a"), AssignmentState::NeedsReview);
    // Once the attempt claimed a result it asks and blocks no more.
    let asked = worker_action(
        &store,
        worker,
        "q2",
        "tasks.worker.ask",
        &json!({"question": "One more?"}),
        NOW + 7,
    );
    assert!(asked.unwrap_err().contains("result"));
    let blocked = worker_action(
        &store,
        worker,
        "b1",
        "tasks.worker.blocked",
        &json!({"reason": "Stuck"}),
        NOW + 7,
    );
    assert!(blocked.unwrap_err().contains("result"));
}

#[test]
fn decision_scopes_pause_only_what_they_cover() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let c = crate::tasks::Assignment {
        files: vec!["src/c".into()],
        ..assignment("c", &["a"])
    };
    let d = crate::tasks::Assignment {
        files: vec!["src/d".into()],
        ..assignment("d", &["c"])
    };
    task_with(
        &store,
        "/project",
        vec![independent("a"), independent("b"), c, d],
    );
    let worker = running(&store, "a", "x");
    let tasks = store.tasks().unwrap();
    let all = tasks.assignments("t").unwrap();
    let blocked = |id: &str| {
        let assignment = all.iter().find(|a| a.id == id).unwrap();
        tasks
            .launch_blocked("t", assignment, &all)
            .unwrap()
            .is_some()
    };
    for (scope, expected) in [
        ("assignment", [false, false, false]),
        ("dependents", [false, true, true]),
        ("task", [true, true, true]),
    ] {
        let id = ask(&store, worker, scope, &format!("A {scope} question"), scope);
        assert_eq!(
            [blocked("b"), blocked("c"), blocked("d")],
            expected,
            "scope {scope}"
        );
        answer(&store, &id, "Go ahead");
        let revision = decision(&store, &id).revision;
        tasks
            .resolve_handoff(&id, revision, "x", "message", NOW + 3)
            .unwrap();
        assert!(!blocked("b") && !blocked("c") && !blocked("d"));
    }
}

fn provider_question() -> Question {
    Question {
        id: "\"wire-7\"".into(),
        title: "The agent has a question".into(),
        items: vec![QuestionItem {
            id: "q1".into(),
            prompt: "Which storage engine?".into(),
            multiple: false,
            allow_custom: true,
            options: ["SQLite", "Postgres"]
                .into_iter()
                .map(|label| QuestionOption {
                    id: label.to_lowercase(),
                    label: label.into(),
                    description: String::new(),
                })
                .collect(),
        }],
        auto_resolve_at: None,
    }
}

#[test]
fn provider_questions_on_attempt_sessions_go_to_mono_and_others_stay_with_the_user() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    task_with(&store, "/project", vec![independent("a")]);
    let worker = running(&store, "a", "x");
    let user_session = store.create("codex", "/project").unwrap().id;
    let question = provider_question();
    assert!(
        decisions::route_question(&store, user_session, &question, NOW)
            .unwrap()
            .is_none()
    );
    store.save_setting(FLAG, "off").unwrap();
    assert!(decisions::route_question(&store, worker, &question, NOW)
        .unwrap()
        .is_none());
    store.save_setting(FLAG, "enabled").unwrap();
    let routed = decisions::route_question(&store, worker, &question, NOW)
        .unwrap()
        .unwrap();
    // A repeated event is the same request.
    let again = decisions::route_question(&store, worker, &question, NOW + 1)
        .unwrap()
        .unwrap();
    assert_eq!(routed.id, again.id);
    assert!(routed.question.contains("Which storage engine?"));
    assert!(routed.question.contains("Postgres"));
    assert_eq!(status(&store, "a"), AssignmentState::WaitingForDecision);
    let batch = inbox::next_batch(&store, "mono-1", NOW + 2)
        .unwrap()
        .unwrap();
    assert!(batch.message.contains("Which storage engine?"));
    answer(&store, &routed.id, "postgres");
    let tasks = store.tasks().unwrap();
    let handoff = tasks.pending_handoffs().unwrap().remove(0);
    let original = handoff.question.expect("the provider question");
    assert_eq!(original.id, question.id);
    let answers =
        crate::provider::answers_from_text(&original, handoff.decision.answer.as_deref().unwrap());
    assert_eq!(answers, vec![vec!["Postgres".to_owned()]]);
}

#[test]
fn review_hand_offs_reach_mono_through_the_same_queue() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    task_with(&store, "/project", vec![independent("a")]);
    let worker = running(&store, "a", "x");
    worker_action(
        &store,
        worker,
        "r1",
        "tasks.worker.result",
        &json!({"outcome": "succeeded", "summary": "Parser added"}),
        NOW + 1,
    )
    .unwrap();
    // As review records a hand-off it cannot decide.
    let tasks = store.tasks().unwrap();
    let review = crate::tasks::NewEvent {
        id: "reviewed:x".into(),
        task_id: "t".into(),
        assignment_id: Some("a".into()),
        attempt_id: Some("x".into()),
        kind: "assignment.reviewed".into(),
        payload: json!({"attemptId": "x", "assignmentId": "a", "outcome": "needs_mono_review",
            "checkpoint": null, "changedFiles": [], "verifications": [],
            "findings": [{"kind": "noChecks"}]}),
    };
    tasks.append_event(&review, NOW + 2).unwrap();
    tasks
        .enqueue_delivery(
            "mono-review:x",
            "reviewed:x",
            crate::tasks::review::MONO_REVIEW,
            "attempt:x",
            NOW + 2,
        )
        .unwrap();
    let question = running_question(&store);
    let batch = inbox::next_batch(&store, "mono-1", NOW + 3)
        .unwrap()
        .unwrap();
    assert_eq!(batch.deliveries.len(), 2);
    for needle in [
        "Parser added",
        "tasks.review.accept",
        "tasks.review.sendBack",
        &question,
    ] {
        assert!(
            batch.message.contains(needle),
            "{needle}: {}",
            batch.message
        );
    }
    let revision = tasks
        .revision(Subject::Assignment { task: "t", id: "a" })
        .unwrap();
    mono(
        &store,
        "tasks.review.accept",
        json!({"taskId": "t", "assignmentId": "a", "revision": revision}),
    )
    .unwrap();
    assert_eq!(status(&store, "a"), AssignmentState::Accepted);
}

/// A second assignment's worker asks a question; returns its id.
fn running_question(store: &Store) -> String {
    let tasks = store.tasks().unwrap();
    let plan_tasks = tasks.assignments("t").unwrap();
    assert_eq!(plan_tasks.len(), 1);
    let id = "question-from-mono";
    tasks
        .create_decision(
            &crate::tasks::NewDecision {
                id: id.into(),
                task_id: "t".into(),
                assignment_id: Some("a".into()),
                attempt_id: Some("x".into()),
                question: "Keep the old name?".into(),
                context: String::new(),
                scope: crate::tasks::DecisionScope::Assignment,
                asked_by: Requester::Worker,
            },
            NOW + 2,
        )
        .unwrap();
    decisions::queue_for_mono(store.connection(), id, NOW + 2).unwrap();
    "Keep the old name?".into()
}

#[test]
fn free_text_replies_resolve_only_unambiguous_decisions() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    task_with(&store, "/project", vec![independent("a"), independent("b")]);
    let first = running(&store, "a", "x");
    let second = running(&store, "b", "y");
    let qa = ask(&store, first, "q1", "Rename the crate?", "assignment");
    let qb = ask(&store, second, "q1", "Drop Windows support?", "assignment");
    escalate(&store, &qa);
    let tasks = store.tasks().unwrap();
    assert_eq!(
        tasks
            .match_user_reply("mono-2", "Keep the old API")
            .unwrap(),
        decisions::ReplyMatch::None
    );
    escalate(&store, &qb);
    // Two are open: an option alone is ambiguous; naming the decision is not.
    assert_eq!(
        tasks
            .match_user_reply("mono-1", "Keep the old API")
            .unwrap(),
        decisions::ReplyMatch::Unclear(vec![qa.clone(), qb.clone()])
    );
    let named = tasks
        .match_user_reply("mono-1", &format!("{qb}: no, keep Windows"))
        .unwrap();
    let revision = decision(&store, &qb).revision;
    assert_eq!(
        named,
        decisions::ReplyMatch::Resolves {
            decision: qb.clone(),
            revision,
            answer: "no, keep Windows".into()
        }
    );
    // An empty reply answers nothing.
    assert_eq!(
        tasks.match_user_reply("mono-1", "").unwrap(),
        decisions::ReplyMatch::Unclear(vec![qa, qb])
    );
}
