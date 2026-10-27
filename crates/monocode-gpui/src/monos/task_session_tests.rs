//! A task worker's session in the app: a question that cannot be routed to
//! Mono stays open and is routed on the next pass, and a worker view
//! recreated from the store keeps billing as its task's policy snapshot.
use super::task_tests::{assert_no_provider_started, mono_busy, pump, storage_question, worker};
use super::tests::{Setup, setup};
use super::*;
use gpui::{AppContext as _, TestAppContext};
use monocode_core::agent::Decision;
use monocode_core::tasks::autonomy::Autonomy;
use monocode_core::tasks::{fixtures, policy};

/// The worker session's view, installed as a draft so no provider turn
/// starts.
fn worker_view(s: &Setup, session: i64, cx: &mut TestAppContext) -> Entity<Conversation> {
    let summary = locked(&s.store)
        .unwrap()
        .sessions()
        .unwrap()
        .into_iter()
        .find(|summary| summary.id == session)
        .unwrap();
    let view = cx.update(|cx| {
        let owner = s.manager.read(cx).owner.upgrade().unwrap();
        owner.update(cx, |o, cx| {
            o.mono_install_app(summary, Settings::default(), true, cx);
            o.mono_ensure_session(session, cx).unwrap()
        })
    });
    cx.run_until_parked();
    view
}

#[gpui::test]
fn a_worker_question_that_cannot_be_routed_stays_open_until_the_next_pass_routes_it(
    cx: &mut TestAppContext,
) {
    let before = monocode_core::provider::test_guard::refused();
    let s = setup(cx);
    let session = worker(&s);
    mono_busy(&s, cx);
    let view = worker_view(&s, session, cx);
    fixtures::hide_decisions(&locked(&s.store).unwrap(), true).unwrap();
    let replies = cx.update(|cx| {
        view.update(cx, |v, _| {
            let replies = v.test_provider_waiting();
            // Fail closed: not handed to the user, where it would auto-skip.
            assert!(v.route_task_questions(vec![storage_question()]).is_empty());
            assert_eq!(v.test_status(), "Waiting for Mono's answer");
            replies
        })
    });
    let kept = replies.try_recv().unwrap();
    assert_eq!(kept.id, storage_question().id);
    assert!(matches!(kept.decision, Decision::KeepOpen));
    {
        let db = locked(&s.store).unwrap();
        let tasks = db.existing_tasks().unwrap().unwrap();
        let events = tasks.events_after(0, 10_000).unwrap();
        let unrouted: Vec<_> = events
            .iter()
            .filter(|e| e.kind == crate::conversation::task_questions::UNROUTED)
            .collect();
        assert_eq!(unrouted.len(), 1, "{events:?}");
        assert_eq!(unrouted[0].attempt_id.as_deref(), Some("x"));
    }
    // Storage recovers: the next task pass routes it to Mono.
    fixtures::hide_decisions(&locked(&s.store).unwrap(), false).unwrap();
    pump(&s, cx);
    let id = {
        let db = locked(&s.store).unwrap();
        let open = db.tasks().unwrap().mono_decisions(&s.id).unwrap();
        assert_eq!(open.len(), 1);
        open[0].id.clone()
    };
    view.read_with(cx, |v, _| {
        assert_eq!(
            v.task_question_id(&id),
            Some(storage_question().id.as_str())
        );
    });
    assert_no_provider_started(before);
}

#[gpui::test]
fn a_recreated_worker_view_bills_as_its_task_policy_snapshot(cx: &mut TestAppContext) {
    let before = monocode_core::provider::test_guard::refused();
    let s = setup(cx);
    let session = worker(&s);
    {
        let db = locked(&s.store).unwrap();
        let task = db.tasks().unwrap().task("t").unwrap().unwrap();
        let billing = Autonomy {
            api_key_billing: Some(true),
            ..Autonomy::default()
        };
        policy::capture_with(&db, &task, "codex", &billing, 1).unwrap();
    }
    let ordinary = locked(&s.store)
        .unwrap()
        .create("codex", &s.project)
        .unwrap();
    let summary = locked(&s.store)
        .unwrap()
        .sessions()
        .unwrap()
        .into_iter()
        .find(|summary| summary.id == session)
        .unwrap();
    // As the Shell recreates a view: from its stored summary alone.
    let store = s.store.clone();
    let (worker, other) = cx.update(|cx| {
        (
            cx.new(|cx| Conversation::new(summary, store.clone(), cx)),
            cx.new(|cx| Conversation::new(ordinary, store, cx)),
        )
    });
    for view in [&worker, &other] {
        view.update(cx, |v, cx| v.load(cx));
    }
    cx.run_until_parked();
    worker.read_with(cx, |v, _| assert_eq!(v.settings.api_key_pin, Some(true)));
    other.read_with(cx, |v, _| assert_eq!(v.settings.api_key_pin, None));
    assert_no_provider_started(before);
}
