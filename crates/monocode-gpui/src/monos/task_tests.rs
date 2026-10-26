//! Mono as development lead in the app: worker questions reach the owning
//! Mono's chat as one queued message, answers reach the waiting worker once,
//! and decisions Mono escalates render as cards the user answers.
use super::tests::{Setup, call, setup};
use super::*;
use gpui::TestAppContext;
use monocode_core::agent::{Control, Decision, Question, QuestionItem, QuestionOption};
use monocode_core::orchestration::{Plan, Task as Assignment};
use monocode_core::tasks::{
    AssignmentState, AttemptState, DecisionState, DeliveryState, NewAttempt, NewTask, Requester,
    State, Subject, TaskStage, coordinator::FLAG,
};
use serde_json::json;

/// Task `t` owned by the setup's Mono, assignment `a` Running and attempt `x`
/// bound to a new worker session in the project. Returns the worker session.
fn worker(s: &Setup) -> i64 {
    let db = locked(&s.store).unwrap();
    db.save_setting(FLAG, "enabled").unwrap();
    let session = db.create("codex", &s.project).unwrap().id;
    let tasks = db.tasks().unwrap();
    tasks
        .create_task(
            &NewTask {
                id: "t".into(),
                mono_id: Some(s.id.clone()),
                project: s.project.clone(),
                title: "Ship it".into(),
                request: "Implement the feature".into(),
                criteria: vec![],
            },
            1,
        )
        .unwrap();
    let plan = Plan {
        title: "Plan".into(),
        summary: "Approach".into(),
        tasks: vec![Assignment {
            id: "a".into(),
            title: "Assignment a".into(),
            prompt: "Do the work".into(),
            files: vec!["src".into()],
            ..Assignment::default()
        }],
    };
    tasks.save_plan("t", 1, &plan, 1).unwrap();
    let t = Subject::Task("t");
    for stage in [TaskStage::Ready, TaskStage::Implementing] {
        let rev = tasks.revision(t).unwrap();
        tasks
            .transition(t, State::Stage(stage), Requester::Mono, rev, 1)
            .unwrap();
    }
    let a = Subject::Assignment { task: "t", id: "a" };
    for to in [AssignmentState::Ready, AssignmentState::Running] {
        let rev = tasks.revision(a).unwrap();
        tasks
            .transition(a, State::Assignment(to), Requester::Coordinator, rev, 1)
            .unwrap();
    }
    tasks
        .create_attempt(
            &NewAttempt {
                id: "x".into(),
                task_id: "t".into(),
                assignment_id: "a".into(),
                operation_id: "launch-x".into(),
                provider: "codex".into(),
                account_id: None,
            },
            1,
        )
        .unwrap();
    let x = Subject::Attempt("x");
    for to in [AttemptState::Starting, AttemptState::Running] {
        let rev = tasks.revision(x).unwrap();
        tasks
            .transition(x, State::Attempt(to), Requester::Coordinator, rev, 1)
            .unwrap();
    }
    let rev = tasks.revision(x).unwrap();
    let bound = db.persisted_session_id(session).unwrap();
    tasks.bind_attempt_session("x", rev, &bound, 1).unwrap();
    session
}

fn ask(s: &Setup, session: i64, question: &str) -> String {
    let db = locked(&s.store).unwrap();
    let asked = monocode_core::app_access::worker_action(
        &db,
        session,
        "q1",
        "tasks.worker.ask",
        &json!({"question": question}),
        2,
    )
    .unwrap();
    asked["decision"]["id"].as_str().unwrap().to_owned()
}

fn decision(s: &Setup, id: &str) -> monocode_core::tasks::DecisionRequest {
    let db = locked(&s.store).unwrap();
    db.tasks().unwrap().decision(id).unwrap().unwrap()
}

fn pump(s: &Setup, cx: &mut TestAppContext) {
    cx.update(|cx| s.manager.update(cx, |m, cx| m.pump_tasks(cx)));
    cx.run_until_parked();
}

fn inbox_messages(s: &Setup, cx: &mut TestAppContext) -> Vec<Message> {
    cx.update(|cx| {
        let m = s.manager.read(cx);
        let mono = m.roster.get(&s.id).unwrap();
        mono.outbox
            .iter()
            .filter(|q| q.id.starts_with("task-inbox-"))
            .cloned()
            .collect()
    })
}

#[gpui::test]
fn task_events_wait_for_an_idle_mono_and_reach_its_chat_once(cx: &mut TestAppContext) {
    let s = setup(cx);
    let session = worker(&s);
    let id = ask(&s, session, "Which parser should I use?");
    // Mono is mid-turn: the event waits in the outbox, never a second turn.
    cx.update(|cx| {
        s.manager.update(cx, |m, cx| {
            m.ensure_chat(&s.id, cx);
        })
    });
    cx.run_until_parked();
    cx.update(|cx| {
        let chat = s.manager.read(cx).chats[&s.id].clone();
        chat.update(cx, |v, _| v.busy = true);
    });
    let rows_before = locked(&s.store)
        .unwrap()
        .load(s.session)
        .unwrap()
        .rows
        .len();
    pump(&s, cx);
    pump(&s, cx);
    let queued = inbox_messages(&s, cx);
    assert_eq!(queued.len(), 1, "{queued:?}");
    assert!(queued[0].internal);
    assert!(queued[0].text.contains("Which parser should I use?"));
    assert!(queued[0].text.contains(&id));
    assert_eq!(
        locked(&s.store)
            .unwrap()
            .load(s.session)
            .unwrap()
            .rows
            .len(),
        rows_before
    );
    // Mono's transcript received it: acknowledged, and nothing is re-sent.
    let batch = queued[0].id.clone();
    locked(&s.store)
        .unwrap()
        .save_setting(&format!("mono-delivered:{}:{batch}", s.session), "1")
        .unwrap();
    cx.update(|cx| {
        s.manager.update(cx, |m, _| {
            m.roster
                .get_mut(&s.id)
                .unwrap()
                .outbox
                .retain(|q| q.id != batch);
        })
    });
    pump(&s, cx);
    assert!(inbox_messages(&s, cx).is_empty());
    let db = locked(&s.store).unwrap();
    let delivery = db
        .tasks()
        .unwrap()
        .delivery(&format!("mono-decision:{id}"))
        .unwrap()
        .unwrap();
    assert_eq!(delivery.state, DeliveryState::Acknowledged);
}

fn storage_question() -> Question {
    Question {
        id: "\"wire-1\"".into(),
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

#[gpui::test]
fn a_routed_provider_question_gets_monos_answer_through_its_reply_once(cx: &mut TestAppContext) {
    let s = setup(cx);
    let session = worker(&s);
    let other = locked(&s.store)
        .unwrap()
        .create("codex", &s.project)
        .unwrap()
        .id;
    // As launched sessions are: installed in the Shell, here as drafts so
    // no provider turn starts.
    let view = |id: i64, cx: &mut TestAppContext| {
        let summary = locked(&s.store)
            .unwrap()
            .sessions()
            .unwrap()
            .into_iter()
            .find(|summary| summary.id == id)
            .unwrap();
        cx.update(|cx| {
            let owner = s.manager.read(cx).owner.upgrade().unwrap();
            owner.update(cx, |o, cx| {
                o.mono_install_app(summary, Settings::default(), true, cx);
                o.mono_ensure_session(id, cx).unwrap()
            })
        })
    };
    let (worker_view, other_view) = (view(session, cx), view(other, cx));
    cx.run_until_parked();
    // Another session's question stays with the user.
    let kept = cx.update(|cx| {
        other_view.update(cx, |v, _| v.route_task_questions(vec![storage_question()]))
    });
    assert_eq!(kept.len(), 1);
    let replies = cx.update(|cx| {
        worker_view.update(cx, |v, _| {
            let replies = v.test_provider_waiting();
            assert!(v.route_task_questions(vec![storage_question()]).is_empty());
            replies
        })
    });
    let id = {
        let db = locked(&s.store).unwrap();
        let tasks = db.tasks().unwrap();
        let open = tasks.mono_decisions(&s.id).unwrap();
        assert_eq!(open.len(), 1);
        open[0].id.clone()
    };
    let revision = decision(&s, &id).revision;
    let answered = call(
        &s,
        "answer-1",
        "tasks.decisions.answer",
        json!({"decisionId": id, "revision": revision, "answer": "postgres"}),
        cx,
    )
    .try_recv()
    .unwrap();
    assert_eq!(answered.unwrap()["state"], "answered");
    pump(&s, cx);
    let Control {
        id: question,
        decision: reply,
    } = replies.try_recv().unwrap();
    assert_eq!(question, storage_question().id);
    let Decision::Answer(answers) = reply else {
        panic!("expected an answer")
    };
    assert_eq!(answers, vec![vec!["Postgres".to_owned()]]);
    assert_eq!(decision(&s, &id).state, DecisionState::Resolved);
    pump(&s, cx);
    assert!(replies.try_recv().is_err());
}

#[gpui::test]
fn an_escalated_decision_is_a_card_in_monos_chat_that_the_user_answers(cx: &mut TestAppContext) {
    let s = setup(cx);
    let session = worker(&s);
    let id = ask(&s, session, "Change the public API?");
    let revision = decision(&s, &id).revision;
    let escalated = call(
        &s,
        "escalate-1",
        "tasks.decisions.escalate",
        json!({"decisionId": id, "revision": revision, "why": "Callers break",
               "recommendation": "Keep the old API", "alternatives": ["Break it"]}),
        cx,
    )
    .try_recv()
    .unwrap()
    .unwrap();
    assert_eq!(escalated["card"]["type"], "decision");
    cx.update(|cx| s.manager.update(cx, |m, cx| m.dispatch(&s.id, cx)));
    cx.run_until_parked();
    let rows = locked(&s.store).unwrap().load(s.session).unwrap().rows;
    let card = rows
        .iter()
        .filter(|r| r.role == "mono-card")
        .filter_map(|r| serde_json::from_str::<serde_json::Value>(&r.text).ok())
        .find(|c| c["type"] == "decision")
        .expect("a decision card in Mono's chat");
    assert_eq!(card["decisionId"], id.as_str());
    assert_eq!(card["recommendation"], "Keep the old API");
    assert!(card["paused"].as_str().unwrap().contains("Assignment a"));
    // Mono is busy from here, so the user's messages queue rather than start
    // a provider turn in the test.
    cx.update(|cx| {
        let chat = s.manager.read(cx).chats[&s.id].clone();
        chat.update(cx, |v, _| v.busy = true);
    });
    // An unclear reply answers nothing and asks; the card's choice answers.
    cx.update(|cx| {
        s.manager
            .update(cx, |m, cx| m.user_reply(&s.id, "what do you think?", cx))
    });
    assert_eq!(decision(&s, &id).state, DecisionState::Open);
    let asked = cx.update(|cx| {
        s.manager
            .read(cx)
            .roster
            .get(&s.id)
            .unwrap()
            .pending_reports
            .iter()
            .any(|r| r.contains(&id))
    });
    let posted = locked(&s.store)
        .unwrap()
        .load(s.session)
        .unwrap()
        .rows
        .iter()
        .any(|r| r.text.contains("did not answer"));
    assert!(asked || posted);
    let choice = json!({"type": "decision-answer", "decisionId": id,
        "revision": card["revision"], "text": "Keep the old API"});
    cx.update(|cx| {
        s.manager
            .update(cx, |m, cx| m.card_action(&s.id, &choice, cx))
    });
    cx.run_until_parked();
    let answered = decision(&s, &id);
    assert_eq!(answered.state, DecisionState::Answered);
    assert_eq!(answered.answered_by, Some(Requester::User));
    assert_eq!(answered.answer.as_deref(), Some("Keep the old API"));
    // Another Mono's session could not have answered it: the bridge binds
    // the Mono from the session, and a stale card choice is refused.
    cx.update(|cx| {
        s.manager
            .update(cx, |m, cx| m.card_action(&s.id, &choice, cx))
    });
    assert!(cx.update(|cx| s.manager.read(cx).error.is_some()));
}
