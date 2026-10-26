//! Recovery's requests to stop a failed task worker reach its session
//! through the same path as `sessions.stop`, so a replacement never shares
//! the worktree with a worker that is still running.
use super::*;

impl Shell {
    /// Stops `id` if it is open in this window, through the `sessions.stop`
    /// path (its queued follow-ups pause too). None when it is not open
    /// here; otherwise whether its turn is still running (stopping is not
    /// stopped until the turn ends).
    pub(crate) fn stop_task_worker(&mut self, id: i64, cx: &mut Context<Self>) -> Option<bool> {
        let view = self.sessions.get(&id).cloned()?;
        view.update(cx, |view, cx| view.stop_app_session(cx));
        Some(view.read(cx).turn_running())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::coordinator::{STOP_ABSENT_GRACE_MS, stop_sessions};
    use monocode_core::tasks::recovery::{self, TurnEnd};

    /// A task worker's session open in this window, whose turn just failed;
    /// returns the window's Shell, the worker view and the stop request.
    fn failed_worker(
        shell: &mut Shell,
        cx: &mut Context<Shell>,
    ) -> (Entity<Shell>, Entity<Conversation>, (String, Option<i64>)) {
        let session = {
            let store = locked(&shell.store).unwrap();
            store
                .save_setting(monocode_core::tasks::coordinator::FLAG, "enabled")
                .unwrap();
            monocode_core::tasks::fixtures::running_worker(&store, "mono-1", "/work/monocode")
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
        let requests = recovery::pending_stops(&store).unwrap();
        assert_eq!(requests.len(), 1);
        let handle = store.resolve_session_id(&requests[0].session_id).unwrap();
        assert_eq!(handle, session);
        (
            cx.entity(),
            view,
            (requests[0].delivery.clone(), Some(handle)),
        )
    }

    #[gpui::test]
    fn a_stop_reaches_the_worker_in_any_window_and_waits_for_its_turn_to_end(
        cx: &mut gpui::TestAppContext,
    ) {
        let (first, _, _) = super::super::tests::window(cx);
        let (shell1, view, request) = first
            .update(cx, |shell, _, cx| {
                let found = failed_worker(shell, cx);
                // Its turn is still running when the stop arrives.
                found.1.update(cx, |view, _| view.busy = true);
                found
            })
            .unwrap();
        // A second window opened later is now the coordinator's latest Shell.
        let (second, _, _) = super::super::tests::window(cx);
        let shell2 = second.update(cx, |_, _, cx| cx.entity()).unwrap();
        let requests = vec![request.clone()];
        let mut absent = std::collections::HashMap::new();
        let now = 1_000_000;
        cx.update(|cx| {
            // No window to look in proves nothing; an unknown session neither.
            assert!(stop_sessions(&[], &requests, &mut absent, now, cx).is_empty());
            let unknown = [(request.0.clone(), None)];
            let only_second = std::slice::from_ref(&shell2);
            assert!(stop_sessions(only_second, &unknown, &mut absent, now, cx).is_empty());
            // Open in no live window: done only after the grace period.
            assert!(stop_sessions(only_second, &requests, &mut absent, now, cx).is_empty());
            let later = now + STOP_ABSENT_GRACE_MS - 1;
            assert!(stop_sessions(only_second, &requests, &mut absent, later, cx).is_empty());
            let after = now + STOP_ABSENT_GRACE_MS;
            assert_eq!(
                stop_sessions(only_second, &requests, &mut absent, after, cx),
                vec![request.0.clone()]
            );
            // Found in the first window and stopped, but not yet ended.
            let shells = [shell2.clone(), shell1.clone()];
            assert!(stop_sessions(&shells, &requests, &mut absent, now, cx).is_empty());
            assert!(
                view.read(cx).follow_ups_paused(),
                "stopped like sessions.stop"
            );
            assert!(view.read(cx).turn_running());
            // The turn ends: now the stop is done, with no grace needed.
            view.update(cx, |view, _| view.end_turn_for_test());
            assert_eq!(
                stop_sessions(&shells, &requests, &mut absent, now, cx),
                vec![request.0.clone()]
            );
        });
        first
            .update(cx, |shell, _, _| {
                let store = locked(&shell.store).unwrap();
                recovery::stopped(&store, &request.0, 11).unwrap();
                assert!(recovery::pending_stops(&store).unwrap().is_empty());
            })
            .unwrap();
    }
}
