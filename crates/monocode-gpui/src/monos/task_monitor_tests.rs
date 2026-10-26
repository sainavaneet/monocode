//! The Mono task pump feeds the monitoring card and the sidebar badge from
//! persisted task state; live worker sessions and the card's worker button
//! reach the Mono panel. No provider runs.
use super::task_tests::{ask, decision, pump, worker};
use super::tests::{Setup, call, setup};
use super::*;
use crate::conversation::orchestration_card::OpenOrchestrationWorker;
use crate::conversation::task_monitor::LiveWorker;
use gpui::{TestAppContext, VisualTestContext};
use monocode_core::tasks::{Requester, lifecycle};
use serde_json::json;

/// Whether the Mono's sidebar row shows exactly this badge label.
fn badge(s: &Setup, label: &str, cx: &mut VisualTestContext) -> bool {
    cx.refresh().unwrap();
    let selector = format!("mono-task-badge-{}:{label}", s.id);
    cx.debug_bounds(Box::leak(selector.into_boxed_str()))
        .is_some()
}

/// The Mono panel is closed so its sidebar row is in view.
fn sidebar(s: &Setup, cx: &mut TestAppContext) -> VisualTestContext {
    s.window
        .update(cx, |_, _, cx| {
            s.manager.update(cx, |m, cx| {
                m.settings = None;
                m.close(cx);
            })
        })
        .unwrap();
    cx.run_until_parked();
    VisualTestContext::from_window(s.window.into(), cx)
}

#[gpui::test]
fn the_sidebar_badge_reflects_the_monos_tasks(cx: &mut TestAppContext) {
    let s = setup(cx);
    let session = worker(&s);
    pump(&s, cx);
    let visual = &mut sidebar(&s, cx);
    assert!(badge(&s, "1 working", visual));
    assert_eq!(
        visual.update(|_, cx| s.manager.read(cx).task_badge(&s.id)),
        Some(monocode_core::tasks::view::Badge::Working(1))
    );

    // Mono escalates a worker's question: it needs the user.
    let id = ask(&s, session, "Change the public API?");
    let revision = decision(&s, &id).revision;
    let escalated = call(
        &s,
        "escalate-1",
        "tasks.decisions.escalate",
        json!({"decisionId": id, "revision": revision, "why": "Callers break",
               "recommendation": "Keep the old API"}),
        visual,
    )
    .try_recv()
    .unwrap();
    assert!(escalated.is_ok(), "{escalated:?}");
    pump(&s, visual);
    assert!(badge(&s, "Needs your decision", visual));
    assert!(!badge(&s, "1 working", visual));

    // Paused as well: the decision still outranks the pause.
    {
        let db = locked(&s.store).unwrap();
        let revision = db.tasks().unwrap().task("t").unwrap().unwrap().revision;
        lifecycle::pause(&db, "t", revision, Requester::User, 50).unwrap();
    }
    pump(&s, visual);
    assert!(badge(&s, "Needs your decision", visual));
}

#[gpui::test]
fn the_pump_fills_the_card_and_its_worker_button_opens_the_session(cx: &mut TestAppContext) {
    let s = setup(cx);
    let session = worker(&s);
    pump(&s, cx);
    let chat = cx.update(|cx| s.manager.read(cx).chats[&s.id].clone());
    cx.update(|cx| {
        let chat = chat.read(cx);
        assert_eq!(chat.task_monitor.tasks.len(), 1);
        assert_eq!(chat.task_monitor.tasks[0].workers[0].session, Some(session));
        assert_eq!(chat.task_controls.len(), 1);
    });
    cx.update(|cx| chat.update(cx, |_, cx| cx.emit(OpenOrchestrationWorker(session))));
    cx.run_until_parked();
    cx.update(|cx| {
        assert_eq!(s.manager.read(cx).route, Some(Route::Session(session)));
    });
}

#[gpui::test]
fn a_worker_session_change_reaches_the_card_before_the_next_read(cx: &mut TestAppContext) {
    let s = setup(cx);
    let session = worker(&s);
    pump(&s, cx);
    let live = LiveWorker {
        busy: true,
        needs_input: true,
        updated_at: 9,
    };
    cx.update(|cx| {
        s.manager
            .update(cx, |m, cx| m.task_worker_changed(session, live, cx))
    });
    cx.update(|cx| {
        let chat = s.manager.read(cx).chats[&s.id].clone();
        assert_eq!(chat.read(cx).task_monitor.live.get(&session), Some(&live));
    });
}

#[gpui::test]
fn the_mono_overview_lists_its_tasks_from_persisted_state(cx: &mut TestAppContext) {
    let s = setup(cx);
    worker(&s);
    pump(&s, cx);
    let visual = &mut VisualTestContext::from_window(s.window.into(), cx);
    visual.update(|_, cx| {
        s.manager
            .update(cx, |m, cx| m.select_overview(OverviewTab::Sessions, cx))
    });
    visual.run_until_parked();
    visual.refresh().unwrap();
    assert!(visual.debug_bounds("mono-overview-task-t").is_some());
    let line = visual.update(|_, cx| {
        super::task_overview::overview_line(&s.manager.read(cx).task_monitors[&s.id][0])
    });
    assert_eq!(line, "Ship it · Active · Implementing · Mono: Supervising");
}
