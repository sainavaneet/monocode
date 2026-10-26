//! Mono as development lead in the app: worker questions reach the owning
//! Mono's chat as one queued message, answers reach the waiting worker once,
//! and decisions Mono escalates render as cards the user answers.
use super::tests::{Setup, call, setup};
use super::*;
use gpui::TestAppContext;
use monocode_core::agent::{Control, Decision, Question, QuestionItem, QuestionOption};
use monocode_core::tasks::{DecisionState, DeliveryState, Requester, coordinator::FLAG};
use serde_json::json;

/// The shared running-worker fixture (`monocode_core::tasks::fixtures`),
/// owned by the setup's Mono, with the coordinator flag on.
fn worker(s: &Setup) -> i64 {
    let db = locked(&s.store).unwrap();
    db.save_setting(FLAG, "enabled").unwrap();
    monocode_core::tasks::fixtures::running_worker(&db, &s.id, &s.project).unwrap()
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

/// Mono is mid-turn: queued task events wait, and no provider turn starts.
fn mono_busy(s: &Setup, cx: &mut TestAppContext) {
    cx.update(|cx| s.manager.update(cx, |m, cx| m.ensure_chat(&s.id, cx)));
    cx.run_until_parked();
    cx.update(|cx| {
        let chat = s.manager.read(cx).chats[&s.id].clone();
        chat.update(cx, |v, _| v.busy = true);
    });
}

/// No test may start a real provider: the core guard refused nothing here.
fn assert_no_provider_started(before: usize) {
    use monocode_core::provider::test_guard;
    assert!(test_guard::under_test());
    assert!(monocode_core::provider::resolve(monocode_core::provider::Kind::Codex).is_none());
    assert_eq!(
        test_guard::refused(),
        before,
        "a real provider launch was attempted"
    );
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
    let before = monocode_core::provider::test_guard::refused();
    let s = setup(cx);
    let session = worker(&s);
    let id = ask(&s, session, "Which parser should I use?");
    // Mono is mid-turn: the event waits in the outbox, never a second turn.
    mono_busy(&s, cx);
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
    drop(db);
    assert_no_provider_started(before);
}

#[gpui::test]
fn a_failing_task_message_is_retried_then_shown_in_monos_chat(cx: &mut TestAppContext) {
    let before = monocode_core::provider::test_guard::refused();
    let s = setup(cx);
    let session = worker(&s);
    ask(&s, session, "Which parser should I use?");
    mono_busy(&s, cx);
    let mut batches = Vec::new();
    for _ in 0..monocode_core::tasks::inbox::INBOX_RETRY_LIMIT {
        pump(&s, cx);
        let queued = inbox_messages(&s, cx);
        assert_eq!(queued.len(), 1, "{queued:?}");
        batches.push(queued[0].id.clone());
        // Mono's turn for it failed.
        cx.update(|cx| {
            s.manager.update(cx, |m, _| {
                let mono = m.roster.get_mut(&s.id).unwrap();
                mono.outbox[0].error = Some("Usage limit reached".into());
            })
        });
        pump(&s, cx);
        let left = inbox_messages(&s, cx);
        assert!(left.is_empty(), "after failure {}: {left:?}", batches.len());
    }
    batches.dedup();
    assert_eq!(batches.len(), 3, "each retry is a new batch");
    let shown = cx.update(|cx| {
        let m = s.manager.read(cx);
        m.roster.get(&s.id).unwrap().pending_reports.clone()
    });
    assert!(
        shown
            .iter()
            .any(|r| r.contains("could not reach") && r.contains("Usage limit")),
        "{shown:?}"
    );
    // Nothing more is queued for the abandoned event.
    pump(&s, cx);
    assert!(inbox_messages(&s, cx).is_empty());
    assert_no_provider_started(before);
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
    let before = monocode_core::provider::test_guard::refused();
    let s = setup(cx);
    let session = worker(&s);
    mono_busy(&s, cx);
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
    // Another session's question stays with the user, with today's
    // behaviour: the provider is not told to keep it open.
    let ordinary = cx.update(|cx| {
        other_view.update(cx, |v, _| {
            let replies = v.test_provider_waiting();
            assert_eq!(v.route_task_questions(vec![storage_question()]).len(), 1);
            replies
        })
    });
    assert!(ordinary.try_recv().is_err());
    let replies = cx.update(|cx| {
        worker_view.update(cx, |v, _| {
            let replies = v.test_provider_waiting();
            assert!(v.route_task_questions(vec![storage_question()]).is_empty());
            assert_eq!(v.test_status(), "Waiting for Mono's answer");
            replies
        })
    });
    // The routed question never expires into a denial: kept open at once.
    let kept = replies.try_recv().unwrap();
    assert_eq!(kept.id, storage_question().id);
    assert!(matches!(kept.decision, Decision::KeepOpen));
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
    // A routed question the provider withdraws stops showing as waiting.
    let mut withdrawn = storage_question();
    withdrawn.id = "\"wire-2\"".into();
    cx.update(|cx| {
        worker_view.update(cx, |v, _| {
            assert!(v.route_task_questions(vec![withdrawn.clone()]).is_empty());
            assert_eq!(v.test_status(), "Waiting for Mono's answer");
            v.forget_task_question(&withdrawn.id);
            assert_eq!(v.test_status(), "Working…");
        })
    });
    assert_no_provider_started(before);
}

#[gpui::test]
fn an_escalated_decision_is_a_card_in_monos_chat_that_the_user_answers(cx: &mut TestAppContext) {
    let before = monocode_core::provider::test_guard::refused();
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
    // An unclear reply answers nothing and asks, once per decision.
    let notes = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            let m = s.manager.read(cx);
            m.roster
                .get(&s.id)
                .unwrap()
                .pending_reports
                .iter()
                .filter(|r| r.contains("did not answer") && r.contains(&id))
                .count()
        })
    };
    for reply in ["what do you think?", "and the tests?"] {
        cx.update(|cx| s.manager.update(cx, |m, cx| m.user_reply(&s.id, reply, cx)));
        assert_eq!(decision(&s, &id).state, DecisionState::Open);
        assert_eq!(notes(cx), 1, "after {reply:?}");
    }
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
    // The card now shows the answer and offers no choices.
    let rows = locked(&s.store).unwrap().load(s.session).unwrap().rows;
    let card = rows
        .iter()
        .filter_map(|r| serde_json::from_str::<serde_json::Value>(&r.text).ok())
        .find(|c| c["type"] == "decision")
        .unwrap();
    assert_eq!(card["state"], "answered");
    assert_eq!(card["answer"], "Keep the old API");
    // Choosing again on the stale card is refused.
    cx.update(|cx| {
        s.manager
            .update(cx, |m, cx| m.card_action(&s.id, &choice, cx))
    });
    assert!(cx.update(|cx| s.manager.read(cx).error.is_some()));
    assert_no_provider_started(before);
}

/// Habit timeouts (`habits::TIMEOUT_MS`, `APPROVAL_WAIT_MS`) act on habit
/// runs and their launches only: a habit run that started long ago times
/// out, while a task worker running just as long is untouched.
#[gpui::test]
fn habit_timeouts_never_stop_a_task_worker(cx: &mut TestAppContext) {
    let s = setup(cx);
    let worker = worker(&s);
    let long_ago = 1;
    let summary = locked(&s.store)
        .unwrap()
        .sessions()
        .unwrap()
        .into_iter()
        .find(|x| x.id == worker)
        .unwrap();
    let store = s.store.clone();
    let worker_view = cx.new(|cx| Conversation::new(summary, store, cx));
    cx.update(|cx| worker_view.update(cx, |v, _| v.busy = true));
    cx.update(|cx| {
        s.manager.update(cx, |m, cx| {
            let habit_session = locked(&s.store)
                .unwrap()
                .create("codex", &s.project)
                .unwrap();
            let store = s.store.clone();
            let view = cx.new(|cx| Conversation::new(habit_session, store, cx));
            m.running.insert(
                99,
                RunningHabit {
                    mono: s.id.clone(),
                    habit: "nightly".into(),
                    view,
                    started: long_ago,
                    launched: long_ago,
                    failure: None,
                    waiting_since: None,
                    cards: Vec::new(),
                    mailbox: Vec::new(),
                },
            );
            m.check_habits(cx);
            // The habit run timed out (well past TIMEOUT_MS) and was
            // finished...
            assert!(m.running.get(&99).is_none_or(|r| r.failure.is_some()));
        })
    });
    cx.run_until_parked();
    // ...while the task worker keeps running and its attempt is unchanged.
    cx.update(|cx| {
        assert!(worker_view.read(cx).turn_running());
        assert!(!worker_view.read(cx).follow_ups_paused());
    });
    let db = locked(&s.store).unwrap();
    let attempt = db.tasks().unwrap().attempt("x").unwrap().unwrap();
    assert_eq!(attempt.status, monocode_core::tasks::AttemptState::Running);
    assert!(
        monocode_core::tasks::recovery::pending_stops(&db)
            .unwrap()
            .is_empty()
    );
}
