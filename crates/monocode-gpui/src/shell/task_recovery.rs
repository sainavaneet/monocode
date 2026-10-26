//! Recovery's requests to stop a failed task worker reach its session
//! through the same path as `sessions.stop`, so a replacement never shares
//! the worktree with a worker that is still running.
use super::*;

impl Shell {
    /// Stops each session open in this window. A session that is not open
    /// here is not running in this app, which is just as stopped.
    pub(crate) fn stop_task_workers(&mut self, sessions: &[i64], cx: &mut Context<Self>) {
        for id in sessions {
            if let Some(view) = self.sessions.get(id).cloned() {
                view.update(cx, |view, cx| view.stop_app_session(cx));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use monocode_core::tasks::recovery::{self, TurnEnd};

    #[gpui::test]
    fn a_failed_workers_session_is_stopped_through_the_stop_path(cx: &mut gpui::TestAppContext) {
        let (window, _, _) = super::super::tests::window(cx);
        window
            .update(cx, |shell, _, cx| {
                let session = {
                    let store = locked(&shell.store).unwrap();
                    store
                        .save_setting(monocode_core::tasks::coordinator::FLAG, "enabled")
                        .unwrap();
                    monocode_core::tasks::fixtures::running_worker(
                        &store,
                        "mono-1",
                        "/work/monocode",
                    )
                    .unwrap()
                };
                let summary = locked(&shell.store)
                    .unwrap()
                    .sessions()
                    .unwrap()
                    .into_iter()
                    .find(|s| s.id == session)
                    .unwrap();
                shell.add(summary, cx);
                let view = shell.ensure_session(session, cx).unwrap();
                assert!(!view.read(cx).follow_ups_paused());
                let requests = {
                    let store = locked(&shell.store).unwrap();
                    let end = TurnEnd {
                        error: Some("503 Service Unavailable".into()),
                        ..TurnEnd::default()
                    };
                    let outcome = recovery::turn_ended(&store, session, &end, 10).unwrap();
                    assert!(
                        matches!(outcome, recovery::Outcome::Retry { .. }),
                        "{outcome:?}"
                    );
                    recovery::pending_stops(&store).unwrap()
                };
                assert_eq!(requests.len(), 1);
                let handle = locked(&shell.store)
                    .unwrap()
                    .resolve_session_id(&requests[0].session_id)
                    .unwrap();
                assert_eq!(handle, session);
                shell.stop_task_workers(&[handle], cx);
                // The same effect as sessions.stop: queued work pauses too.
                assert!(view.read(cx).follow_ups_paused());
                let store = locked(&shell.store).unwrap();
                recovery::stopped(&store, &requests[0].delivery, 11).unwrap();
                assert!(recovery::pending_stops(&store).unwrap().is_empty());
            })
            .unwrap();
    }
}
