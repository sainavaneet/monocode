//! The Mono task pump feeds the monitoring card and the sidebar badge from
//! persisted task state; live worker sessions and the card's worker button
//! reach the Mono panel. No provider runs.
use super::task_tests::{ask, decision, inbox_messages, mono_busy, pump, worker};
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
    assert!(badge(&s, "Decision needed", visual));
    assert!(!badge(&s, "1 working", visual));

    // Paused as well: the decision still outranks the pause.
    {
        let db = locked(&s.store).unwrap();
        let revision = db.tasks().unwrap().task("t").unwrap().unwrap().revision;
        lifecycle::pause(&db, "t", revision, Requester::User, 50).unwrap();
    }
    pump(&s, visual);
    assert!(badge(&s, "Decision needed", visual));
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
        activity: Some("Edit src/lib.rs".into()),
        started_at: Some(1),
    };
    cx.update(|cx| {
        s.manager
            .update(cx, |m, cx| m.task_worker_changed(session, live.clone(), cx))
    });
    cx.update(|cx| {
        let chat = s.manager.read(cx).chats[&s.id].clone();
        assert_eq!(chat.read(cx).task_monitor.live.get(&session), Some(&live));
    });
}

#[gpui::test]
fn the_mono_overview_lists_its_tasks_from_persisted_state(cx: &mut TestAppContext) {
    let s = setup(cx);
    let session = worker(&s);
    pump(&s, cx);
    let visual = &mut VisualTestContext::from_window(s.window.into(), cx);
    visual.update(|_, cx| {
        s.manager.update(cx, |m, cx| {
            m.settings = None;
            m.select_overview(OverviewTab::Sessions, cx)
        })
    });
    visual.run_until_parked();
    visual.refresh().unwrap();
    assert!(visual.debug_bounds("mono-overview-task-t").is_some());
    assert!(visual.debug_bounds("task-progress-segments").is_some());
    assert!(visual.debug_bounds("overview-worker-activity").is_some());
    let line = visual.update(|_, cx| {
        super::task_overview::overview_line(&s.manager.read(cx).task_monitors[&s.id][0])
    });
    assert_eq!(line, "Ship it · Active · Implementing · Mono: Supervising");
    let worker = visual
        .debug_bounds("overview-worker-t-a")
        .expect("task worker is visible even without a direct launch");
    assert!(
        worker.size.height <= px(42.),
        "worker rows should fit a single compact line: {worker:?}"
    );
    visual.simulate_click(worker.center(), Default::default());
    visual.run_until_parked();
    s.window
        .update(visual, |shell, _, cx| {
            assert_eq!(s.manager.read(cx).route, Some(Route::Session(session)));
            assert_eq!(
                shell.workspace.project, s.project,
                "opening a worker must preserve the current project"
            );
            s.manager.update(cx, |m, cx| {
                m.route = None;
                m.task_monitors.get_mut(&s.id).unwrap()[0].workers[0].detached = vec![s.session];
                cx.notify();
            });
        })
        .unwrap();
    visual.run_until_parked();
    visual.refresh().unwrap();
    let previous = visual
        .debug_bounds(Box::leak(
            format!("previous-worker-session-{}", s.session).into_boxed_str(),
        ))
        .unwrap();
    visual.simulate_click(previous.center(), Default::default());
    visual.run_until_parked();
    s.window
        .update(visual, |shell, _, cx| {
            assert_eq!(
                s.manager.read(cx).route,
                Some(Route::Session(s.session)),
                "history click must not bubble to the current worker"
            );
            assert_eq!(shell.workspace.project, s.project);
        })
        .unwrap();
}

#[gpui::test]
fn a_monitor_that_cannot_be_read_never_holds_back_task_events(cx: &mut TestAppContext) {
    let s = setup(cx);
    let session = worker(&s);
    let id = ask(&s, session, "Which parser should I use?");
    monocode_core::tasks::fixtures::break_monitor(&locked(&s.store).unwrap()).unwrap();
    let now = monocode_core::time::now_ms();
    assert!(monocode_core::tasks::view::monitor(&locked(&s.store).unwrap(), &s.id, now).is_err());
    mono_busy(&s, cx);
    pump(&s, cx);
    let queued = inbox_messages(&s, cx);
    assert_eq!(queued.len(), 1, "{queued:?}");
    assert!(queued[0].text.contains(&id));
    cx.update(|cx| {
        let chat = s.manager.read(cx).chats[&s.id].clone();
        assert!(chat.read(cx).task_monitor.tasks.is_empty());
    });
}

#[gpui::test]
fn task_details_start_compact_and_expand_on_demand(cx: &mut TestAppContext) {
    let s = setup(cx);
    worker(&s);
    pump(&s, cx);
    let chat = cx.update(|cx| s.manager.read(cx).chats[&s.id].clone());
    chat.update(cx, |v, cx| {
        v.orchestration_plan = Some(monocode_core::orchestration::Plan {
            title: "Replacement proposal".into(),
            summary: "A very long proposal summary. ".repeat(100),
            tasks: (0..12)
                .map(|i| monocode_core::orchestration::Task {
                    id: format!("task-{i}"),
                    title: format!("Assignment {i}"),
                    ..Default::default()
                })
                .collect(),
        });
        cx.notify();
    });
    s.manager.update(cx, |m, cx| {
        m.settings = None;
        cx.notify();
    });
    let visual = &mut VisualTestContext::from_window(s.window.into(), cx);
    visual.run_until_parked();
    visual.refresh().unwrap();
    assert!(visual.debug_bounds("orchestration-summary").is_some());
    assert!(visual.debug_bounds("orchestration-card").is_none());
    let summary = visual.debug_bounds("orchestration-summary").unwrap();
    assert!(summary.size.height <= px(36.));
    visual.simulate_click(summary.center(), Default::default());
    visual.run_until_parked();
    visual.refresh().unwrap();
    let card = visual.debug_bounds("orchestration-card").unwrap();
    assert!(
        card.size.height <= px(320.),
        "long plans must not push controls outside the window"
    );
    let cancel = visual.debug_bounds("task-control-t-tasks.cancel").unwrap();
    assert!(
        cancel.top() >= card.top() && cancel.bottom() <= card.bottom(),
        "Cancel stays visible above long plan details"
    );
    visual.simulate_click(cancel.center(), Default::default());
    visual.run_until_parked();
    let state = locked(&s.store)
        .unwrap()
        .tasks()
        .unwrap()
        .task("t")
        .unwrap()
        .unwrap()
        .lifecycle;
    assert_eq!(state, monocode_core::tasks::TaskLifecycle::Cancelled);
    chat.read_with(visual, |v, _| {
        assert!(
            v.orchestration_plan.is_some(),
            "cancelling the old run preserves the new proposal"
        )
    });
    let summary = visual.debug_bounds("orchestration-summary").unwrap();
    visual.simulate_click(summary.center(), Default::default());
    visual.run_until_parked();
    visual.refresh().unwrap();
    assert!(visual.debug_bounds("orchestration-card").is_none());
}

#[gpui::test]
fn global_activity_opens_the_mono_in_its_assigned_project(cx: &mut TestAppContext) {
    let s = setup(cx);
    let other = format!("{}/other-project", s.project);
    std::fs::create_dir_all(&other).unwrap();
    s.window
        .update(cx, |shell, window, cx| {
            shell.workspace.projects.push(other.clone());
            shell.workspace.project = other;
            shell.open_activity_mono(&s.id, window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    s.window
        .update(cx, |shell, _, cx| {
            assert_eq!(shell.workspace.project, s.project);
            assert_eq!(s.manager.read(cx).active.as_deref(), Some(s.id.as_str()));
        })
        .unwrap();
}

#[gpui::test]
fn a_completed_task_shows_its_report_until_the_user_hides_it(cx: &mut TestAppContext) {
    use monocode_core::tasks::lifecycle::{Display, dismissed_key};
    use monocode_core::tasks::view::ReportSummary;
    let s = setup(cx);
    worker(&s);
    pump(&s, cx);
    let chat = cx.update(|cx| s.manager.read(cx).chats[&s.id].clone());
    // The card renders what the view carries: a completed task with its
    // final report instead of worker rows and controls.
    let mut done = cx.update(|cx| s.manager.read(cx).task_monitors[&s.id][0].clone());
    done.control.display = Display::Completed;
    done.report = Some(ReportSummary {
        implemented: true,
        committed: true,
        merged: false,
        pushed: false,
        deployed: false,
        criteria_satisfied: 1,
        criteria_total: 1,
        assignments: vec![("mono/t/a".into(), 2)],
        generated_at: monocode_core::time::now_ms(),
    });
    assert!(
        !super::task_overview::is_live_overview_task(&done),
        "a completed report belongs in the conversation, not the live Sessions panel"
    );
    let current = cx.update(|cx| s.manager.read(cx).task_monitors[&s.id][0].clone());
    assert!(
        super::tasks::task_display_priority(&current) < super::tasks::task_display_priority(&done),
        "current work must sort ahead of a retained completion report"
    );
    let shown = vec![done];
    s.manager.update(cx, |m, cx| {
        m.settings = None;
        m.task_monitors.insert(s.id.clone(), shown.clone());
        chat.update(cx, |chat, cx| {
            chat.set_task_controls(Vec::new(), cx);
            chat.set_task_monitor(shown, cx);
        });
        cx.notify();
    });
    let visual = &mut VisualTestContext::from_window(s.window.into(), cx);
    visual.run_until_parked();
    visual.refresh().unwrap();
    // The plan card starts compact; open it to see the task cards.
    if let Some(summary) = visual.debug_bounds("orchestration-summary") {
        visual.simulate_click(summary.center(), Default::default());
        visual.run_until_parked();
        visual.refresh().unwrap();
    }
    assert!(visual.debug_bounds("task-monitor-t-report").is_some());
    assert!(
        visual
            .debug_bounds("task-monitor-t-state-Completed")
            .is_some()
    );
    assert!(
        visual.debug_bounds("task-worker-t-a").is_none(),
        "a completed task lists no worker rows"
    );
    assert!(visual.debug_bounds("task-control-t-tasks.cancel").is_none());
    assert_eq!(
        visual.update(|_, cx| s.manager.read(cx).task_badge(&s.id)),
        None,
        "a completed task raises no badge"
    );
    let hide = visual.debug_bounds("task-monitor-t-hide").unwrap();
    visual.simulate_click(hide.center(), Default::default());
    visual.run_until_parked();
    visual.refresh().unwrap();
    assert!(visual.debug_bounds("task-monitor-t-report").is_none());
    visual.update(|_, cx| {
        assert!(s.manager.read(cx).task_monitors[&s.id].is_empty());
        assert!(chat.read(cx).task_monitor.tasks.is_empty());
    });
    assert_eq!(
        locked(&s.store)
            .unwrap()
            .setting(&dismissed_key("t"))
            .unwrap(),
        Some("1".into()),
        "the dismissal is remembered for the next pump"
    );
}
