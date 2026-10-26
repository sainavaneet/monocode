//! The plan card's task controls and a stopped worker's session, in a Shell
//! window. No provider runs: the worker is the core fixture's bound session.
use super::*;
use crate::conversation::task_controls::TaskControl;
use monocode_core::tasks::lifecycle::{self, Display};
use monocode_core::tasks::{Requester, TaskLifecycle, coordinator::FLAG, recovery};

/// The fixture's running worker (task `t`, owned by `mono-1`) with the
/// coordinator on; returns its session handle.
fn running_worker(shell: &Shell) -> i64 {
    let store = locked(&shell.store).unwrap();
    store.save_setting(FLAG, "enabled").unwrap();
    monocode_core::tasks::fixtures::running_worker(&store, "mono-1", "/work/monocode").unwrap()
}

fn task(shell: &Shell) -> monocode_core::tasks::Task {
    let store = locked(&shell.store).unwrap();
    store.tasks().unwrap().task("t").unwrap().unwrap()
}

fn control(view: &Entity<Conversation>, action: &'static str, cx: &mut Context<Shell>) {
    view.update(cx, |view, cx| {
        let row = view.task_controls[0].clone();
        view.request_task_control(
            TaskControl {
                task_id: row.task_id,
                revision: row.revision,
                action,
                requirements: None,
            },
            cx,
        )
    });
}

#[gpui::test]
fn the_plan_card_pauses_steers_and_cancels_a_task_as_the_user(cx: &mut gpui::TestAppContext) {
    let (window, _, lead) = super::tests::window(cx);
    let view = window
        .update(cx, |shell, _, cx| {
            running_worker(shell);
            let rows = lifecycle::controls(&locked(&shell.store).unwrap(), "mono-1").unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].display, Display::Active);
            let view = shell.sessions[&lead].clone();
            view.update(cx, |view, cx| view.set_task_controls(rows, cx));
            control(&view, "tasks.pause", cx);
            view
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            assert_eq!(task(shell).lifecycle, TaskLifecycle::Paused);
            let store = locked(&shell.store).unwrap();
            assert_eq!(recovery::pending_stops(&store).unwrap().len(), 1);
            drop(store);
            assert_eq!(view.read(cx).task_controls[0].display, Display::Pausing);
            // Steer takes the new requirements from the composer.
            control(&view, "tasks.steer", cx);
            assert!(view.read(cx).orchestration_error.is_some());
            view.update(cx, |view, cx| {
                view.input
                    .update(cx, |input, cx| input.set_text("Also log errors".into(), cx))
            });
            control(&view, "tasks.steer", cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            assert!(view.read(cx).orchestration_error.is_none());
            assert!(view.read(cx).input.read(cx).content.is_empty());
            let store = locked(&shell.store).unwrap();
            let events = store.tasks().unwrap().events_after(0, 1_000).unwrap();
            let steered: Vec<_> = events.iter().filter(|e| e.kind == "task.steered").collect();
            assert_eq!(steered.len(), 1);
            assert_eq!(steered[0].payload["requirements"], "Also log errors");
            assert_eq!(steered[0].payload["requester"], "user");
            drop(store);
            control(&view, "tasks.cancel", cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, _| {
            assert_eq!(task(shell).lifecycle, TaskLifecycle::Cancelled)
        })
        .unwrap();
}

#[gpui::test]
fn a_stopped_workers_session_shows_it_is_detached_from_its_task(cx: &mut gpui::TestAppContext) {
    let (window, _, _) = super::tests::window(cx);
    let view = window
        .update(cx, |shell, _, cx| {
            let session = running_worker(shell);
            let summary = locked(&shell.store)
                .unwrap()
                .sessions()
                .unwrap()
                .into_iter()
                .find(|s| s.id == session)
                .unwrap();
            shell.add(summary, cx);
            let view = shell.ensure_session(session, cx).unwrap();
            let revision = task(shell).revision;
            let store = locked(&shell.store).unwrap();
            lifecycle::pause(&store, "t", revision, Requester::User, 10).unwrap();
            drop(store);
            // The stop lands and the worker's turn ends.
            view.update(cx, |view, cx| view.report_task_turn(true, cx));
            view
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            let status = view.read(cx).status_for_test().to_owned();
            assert!(
                status.contains("Detached from task \"Ship it\""),
                "{status}"
            );
            // The pause, not the stop, decided what happened to the task.
            assert_eq!(task(shell).lifecycle, TaskLifecycle::Paused);
            let store = locked(&shell.store).unwrap();
            let events = store.tasks().unwrap().events_after(0, 1_000).unwrap();
            assert!(
                events
                    .iter()
                    .all(|e| e.kind != "recovery.ended_without_result")
            );
        })
        .unwrap();
}
