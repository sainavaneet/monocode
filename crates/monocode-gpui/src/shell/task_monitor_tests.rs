//! The monitoring card on a lead's plan card, in a Shell window: each state
//! renders its label, the controls carry out task-8's lifecycle actions, the
//! worker buttons open the worker and its changes, live sessions show at
//! once, and no secret reaches the timeline. No provider runs: the worker is
//! the core fixture's bound session.
use super::*;
use crate::conversation::orchestration_card::OpenOrchestrationWorker;
use crate::conversation::task_monitor::{
    LiveWorker, OpenTaskChanges, counts_line, timeline_line, times_line, waiting_line,
};
use gpui::{VisualTestContext, WindowHandle};
use monocode_core::tasks::view::{self, TaskMonitor};
use monocode_core::tasks::{Requester, TaskLifecycle, coordinator::FLAG, lifecycle, recovery};
use std::cell::RefCell;
use std::rc::Rc;

const SECRET: &str = "sk-ant-api03-Zx81kQ2vB7nP0wLmT4yR";

/// The fixture's running worker (task `t`, owned by `mono-1`) with the
/// coordinator on and a recorded worktree; returns its session handle.
fn running_worker(shell: &Shell) -> i64 {
    let store = locked(&shell.store).unwrap();
    store.save_setting(FLAG, "enabled").unwrap();
    let session =
        monocode_core::tasks::fixtures::running_worker(&store, "mono-1", "/work/monocode").unwrap();
    store
        .tasks()
        .unwrap()
        .save_workspace(&monocode_core::tasks::Workspace {
            id: "w".into(),
            task_id: "t".into(),
            assignment_id: Some("a".into()),
            mode: monocode_core::orchestration::WorkspaceMode::Worktree,
            repository: "/work/monocode".into(),
            path: "/work/monocode-a".into(),
            branch: Some("task/t-a".into()),
            base_commit: Some("abc123".into()),
            owner: "test".into(),
            revision: 0,
            created_at: 1,
        })
        .unwrap();
    session
}

fn monitors(shell: &Shell) -> Vec<TaskMonitor> {
    view::monitor(&locked(&shell.store).unwrap(), "mono-1").unwrap()
}

/// Reads the store into the lead's card, as the Mono task pump does.
fn show(window: &WindowHandle<Shell>, lead: i64, cx: &mut VisualTestContext) {
    window
        .update(cx, |shell, _, cx| {
            let tasks = monitors(shell);
            let rows = tasks.iter().map(|t| t.control.clone()).collect();
            shell.sessions[&lead].update(cx, |view, cx| {
                view.set_task_controls(rows, cx);
                view.set_task_monitor(tasks, cx)
            });
        })
        .unwrap();
    cx.run_until_parked();
    cx.refresh().unwrap();
}

fn shown(cx: &mut VisualTestContext, selector: &str) -> bool {
    cx.debug_bounds(Box::leak(selector.to_owned().into_boxed_str()))
        .is_some()
}

fn click(cx: &mut VisualTestContext, selector: &str) {
    cx.refresh().unwrap();
    let bounds = cx
        .debug_bounds(Box::leak(selector.to_owned().into_boxed_str()))
        .unwrap_or_else(|| panic!("{selector} is not shown"));
    cx.simulate_click(bounds.center(), Default::default());
    cx.run_until_parked();
}

fn lifecycle_of(window: &WindowHandle<Shell>, cx: &mut VisualTestContext) -> TaskLifecycle {
    window
        .update(cx, |shell, _, _| {
            let store = locked(&shell.store).unwrap();
            store.tasks().unwrap().task("t").unwrap().unwrap().lifecycle
        })
        .unwrap()
}

fn setup(cx: &mut gpui::TestAppContext) -> (WindowHandle<Shell>, i64, i64, VisualTestContext) {
    let (window, _, lead) = super::tests::window(cx);
    let worker = window
        .update(cx, |shell, window, cx| {
            shell.activate_session(lead, window, cx);
            running_worker(shell)
        })
        .unwrap();
    let visual = VisualTestContext::from_window(window.into(), cx);
    (window, lead, worker, visual)
}

#[gpui::test]
fn each_task_state_renders_its_label_on_the_monitoring_card(cx: &mut gpui::TestAppContext) {
    let (window, lead, worker, mut visual) = setup(cx);
    let cx = &mut visual;
    show(&window, lead, cx);
    assert!(shown(cx, "task-monitor-t"));
    assert!(shown(cx, "task-monitor-t-state-Active"));
    assert!(shown(cx, "task-worker-t-a-state-Working"));
    assert!(shown(cx, "task-worker-t-a-workspace"));
    assert!(shown(cx, "task-monitor-t-stage"));
    assert!(shown(cx, "task-monitor-t-objective"));
    assert!(shown(cx, "task-monitor-t-times"));
    // The minimal control rows give way to the card.
    assert!(!shown(cx, "task-controls"));

    // The live session waits on an approval: shown before the next read.
    window
        .update(cx, |shell, _, cx| {
            let live = LiveWorker {
                busy: true,
                needs_input: true,
                updated_at: 5,
            };
            shell.sessions[&lead].update(cx, |v, cx| v.set_worker_live(worker, live, cx));
        })
        .unwrap();
    cx.run_until_parked();
    cx.refresh().unwrap();
    assert!(shown(cx, "task-worker-t-a-state-Needs input"));

    // Paused with the worker's stop unconfirmed: Pausing, then a detached
    // session the user can still open.
    window
        .update(cx, |shell, _, _| {
            let store = locked(&shell.store).unwrap();
            let revision = store.tasks().unwrap().task("t").unwrap().unwrap().revision;
            lifecycle::pause(&store, "t", revision, Requester::User, 10).unwrap();
        })
        .unwrap();
    show(&window, lead, cx);
    assert!(shown(cx, "task-monitor-t-state-Pausing"));
    assert!(shown(cx, &format!("task-worker-t-a-detached-{worker}")));
    assert!(!shown(cx, "task-worker-t-a-open"));
}

#[gpui::test]
fn open_decisions_and_quiet_reminders_show_on_the_card(cx: &mut gpui::TestAppContext) {
    let (window, lead, _, mut visual) = setup(cx);
    let cx = &mut visual;
    window
        .update(cx, |shell, _, cx| {
            let mut tasks = monitors(shell);
            tasks[0].decisions = vec![
                view::DecisionView {
                    id: "d1".into(),
                    assignment_id: Some("a".into()),
                    escalated: true,
                    quiet: false,
                },
                view::DecisionView {
                    id: "d2".into(),
                    assignment_id: Some("a".into()),
                    escalated: false,
                    quiet: true,
                },
            ];
            tasks[0].control.reason = Some("Waiting for decision d1".into());
            tasks[0].waiting_cause = Some("decision".into());
            assert_eq!(
                waiting_line(&tasks[0]).as_deref(),
                Some("Waiting on a decision: Waiting for decision d1")
            );
            shell.sessions[&lead].update(cx, |v, cx| v.set_task_monitor(tasks, cx));
        })
        .unwrap();
    cx.run_until_parked();
    cx.refresh().unwrap();
    assert!(shown(cx, "task-monitor-t-needs-you"));
    assert!(shown(cx, "task-decision-d2-quiet"));
    assert!(!shown(cx, "task-decision-d1-quiet"));
    assert!(shown(cx, "task-monitor-t-waiting"));
}

#[gpui::test]
fn card_controls_dispatch_the_lifecycle_actions(cx: &mut gpui::TestAppContext) {
    let (window, lead, worker, mut visual) = setup(cx);
    let cx = &mut visual;
    show(&window, lead, cx);
    click(cx, "task-control-t-tasks.pause");
    assert_eq!(lifecycle_of(&window, cx), TaskLifecycle::Paused);
    show(&window, lead, cx);
    click(cx, "task-control-t-tasks.resume");
    // Resume checks the recorded worktree first; this one is fictional, so
    // the task is blocked with task-8's recovery condition, not resumed blind.
    assert_eq!(lifecycle_of(&window, cx), TaskLifecycle::Blocked);
    show(&window, lead, cx);
    click(cx, "task-control-t-tasks.cancel");
    assert_eq!(lifecycle_of(&window, cx), TaskLifecycle::Cancelled);
    // Pause, resume and cancel went through task-8's events, as the user.
    window
        .update(cx, |shell, _, _| {
            let store = locked(&shell.store).unwrap();
            let events = store.tasks().unwrap().events_after(0, 1_000).unwrap();
            for kind in [
                "task.pause_requested",
                "task.resume_requested",
                "task.cancel_requested",
            ] {
                let event = events.iter().find(|e| e.kind == kind).unwrap();
                assert_eq!(event.payload["requester"], "user", "{kind}");
            }
        })
        .unwrap();
    let _ = worker;
}

#[gpui::test]
fn worker_buttons_open_the_worker_and_its_changes(cx: &mut gpui::TestAppContext) {
    let (window, lead, worker, mut visual) = setup(cx);
    let cx = &mut visual;
    let opened = Rc::new(RefCell::new(Vec::new()));
    let workers = Rc::new(RefCell::new(Vec::new()));
    let (seen, started) = (opened.clone(), workers.clone());
    let _subscriptions = window
        .update(cx, |shell, _, cx| {
            let view = shell.sessions[&lead].clone();
            [
                cx.subscribe(&view, move |_, _, e: &OpenTaskChanges, _| {
                    seen.borrow_mut().push(e.cwd.clone())
                }),
                cx.subscribe(&view, move |_, _, e: &OpenOrchestrationWorker, _| {
                    started.borrow_mut().push(e.0)
                }),
            ]
        })
        .unwrap();
    show(&window, lead, cx);
    click(cx, "task-worker-t-a-changes");
    assert_eq!(*opened.borrow(), vec!["/work/monocode-a".to_owned()]);
    click(cx, "task-worker-t-a-open");
    assert_eq!(*workers.borrow(), vec![worker]);
}

#[gpui::test]
fn no_secret_from_a_worker_error_appears_in_the_timeline(cx: &mut gpui::TestAppContext) {
    let (window, lead, _, mut visual) = setup(cx);
    let cx = &mut visual;
    window
        .update(cx, |shell, _, _| {
            let store = locked(&shell.store).unwrap();
            let error = format!("503 overloaded; retry with Bearer {SECRET}");
            recovery::interrupt(
                &store,
                "x",
                monocode_core::usage::Failure::Temporary,
                &error,
                None,
                20,
            )
            .unwrap();
        })
        .unwrap();
    show(&window, lead, cx);
    assert!(!shown(cx, "task-monitor-t-timeline"));
    click(cx, "task-monitor-t-timeline-toggle");
    cx.refresh().unwrap();
    assert!(shown(cx, "task-monitor-t-timeline"));
    let tasks = window.update(cx, |shell, _, _| monitors(shell)).unwrap();
    let task = &tasks[0];
    assert!(!task.timeline.is_empty());
    let first = &task.timeline[0];
    assert!(shown(cx, &format!("task-timeline-t-{}", first.seq)));
    assert!(shown(cx, "task-monitor-t-recovery-0"));
    let live = Default::default();
    let mut printed: Vec<String> = task.timeline.iter().map(|e| timeline_line(e, 30)).collect();
    printed.extend(task.recovery.iter().cloned());
    printed.extend(waiting_line(task));
    printed.push(counts_line(task, &live));
    printed.push(times_line(task, &live, 30));
    let text = printed.join("\n");
    assert!(text.contains("temporary"), "{text}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(!text.contains("Zx81kQ2vB7nP0wLmT4yR"), "{text}");
}

#[gpui::test]
fn with_the_coordinator_off_the_plan_card_shows_no_monitor(cx: &mut gpui::TestAppContext) {
    let (window, lead, _, mut visual) = setup(cx);
    let cx = &mut visual;
    window
        .update(cx, |shell, _, _| {
            locked(&shell.store)
                .unwrap()
                .save_setting(FLAG, "disabled")
                .unwrap()
        })
        .unwrap();
    show(&window, lead, cx);
    assert!(!shown(cx, "task-monitor-t"));
    assert!(!shown(cx, "task-controls"));
}
