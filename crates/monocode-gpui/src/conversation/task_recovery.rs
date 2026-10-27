//! A task worker's turn ending is reported to recovery
//! (`monocode_core::tasks::recovery::turn_ended`), which classifies a failure
//! and recovers it, or hands a session that ended without a result to Mono.
//! Recovery ignores every session that is not the live attempt of a task
//! worker, and does nothing while the task coordinator is off.
use super::*;
use monocode_core::tasks::lifecycle;
use monocode_core::tasks::recovery::{self, TurnEnd};

/// How a finished turn ended, from the status the runtime reported and the
/// usage limit it detected.
pub(crate) fn turn_end(
    status: &str,
    stopped: bool,
    limit: Option<&monocode_core::usage::Limit>,
) -> TurnEnd {
    // A turn that ends while waiting on a question or approval did not fail.
    let failed = !stopped
        && status != "Ready"
        && !status.starts_with("Waiting for")
        && !status.trim().is_empty();
    TurnEnd {
        stopped,
        error: failed.then(|| status.to_owned()),
        usage_limited: !stopped && limit.is_some(),
        resets_at: limit
            .and_then(|l| l.resets_at)
            .and_then(|at| u64::try_from(at).ok()),
    }
}

/// The billing choice snapshotted into the task policy of the worker whose
/// session is `session`, so a worker view recreated from the store bills as
/// its launch did. None for every other session, or with the coordinator off.
pub(crate) fn worker_billing(
    store: &monocode_core::store::Store,
    session: i64,
) -> Result<Option<bool>, String> {
    if !monocode_core::tasks::coordinator::enabled(store) {
        return Ok(None);
    }
    let Some(tasks) = store.existing_tasks().map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let Ok(bound) = store.persisted_session_id(session) else {
        return Ok(None);
    };
    let Some(attempt) = tasks
        .attempt_for_session(&bound)
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    let policy =
        monocode_core::tasks::policy::of(&tasks, &attempt.task_id).map_err(|e| e.to_string())?;
    Ok(policy.map(|p| p.use_api_key))
}

impl Conversation {
    /// The running turn's final update arrived.
    #[cfg(test)]
    pub(crate) fn end_turn_for_test(&mut self) {
        self.busy = false;
        self.stopping = false;
    }

    /// Whether a turn still runs here: busy, or stopped but not yet ended.
    pub(crate) fn turn_running(&self) -> bool {
        self.busy || self.stopping
    }

    /// Whether queued follow-ups wait (a stopped session pauses them).
    #[cfg(test)]
    pub(crate) fn follow_ups_paused(&self) -> bool {
        self.follow_ups.state.paused
    }

    /// Called when a turn finishes; Mono chats and remote sessions are never
    /// task workers. A worker session its task no longer owns (stopped,
    /// cancelled, failed or replaced) says so in its status, so whatever the
    /// user types there is visibly detached from the task.
    pub(crate) fn report_task_turn(&mut self, stopped: bool, cx: &mut Context<Self>) {
        if self.settings.mono.is_some() || self.is_remote() {
            return;
        }
        let end = turn_end(&self.status, stopped, self.settings.usage_limit.as_ref());
        let store = self.store.clone();
        let session = self.summary.id;
        let work = cx.background_executor().spawn(async move {
            let now = monocode_core::time::now_ms();
            let store = locked(&store)?;
            if let Err(error) = recovery::turn_ended(&store, session, &end, now) {
                eprintln!("Task recovery for session {session}: {error}");
            }
            lifecycle::detached(&store, session).map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| match work.await {
            Ok(Some(notice)) => {
                let _ = this.update(cx, |this, cx| this.show_detached(notice, cx));
            }
            Ok(None) => {}
            Err(error) => eprintln!("Task recovery for session {session}: {error}"),
        })
        .detach();
    }

    /// Shows, once loaded (also after a restart), whether this session is a
    /// task worker its task no longer owns.
    pub(crate) fn refresh_detached(&mut self, cx: &mut Context<Self>) {
        if self.settings.mono.is_some() || self.is_remote() {
            return;
        }
        let store = self.store.clone();
        let session = self.summary.id;
        let work = cx.background_executor().spawn(async move {
            let store = locked(&store)?;
            lifecycle::detached(&store, session).map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Some(notice)) = work.await {
                let _ = this.update(cx, |this, cx| this.show_detached(notice, cx));
            }
        })
        .detach();
    }

    /// Shows that this worker session no longer belongs to its task.
    pub(crate) fn show_detached(&mut self, notice: String, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.status = notice;
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn status_for_test(&self) -> &str {
        &self.status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finished_turns_map_to_what_recovery_needs() {
        let ok = turn_end("Ready", false, None);
        assert_eq!(ok, TurnEnd::default());
        let stopped = turn_end("Ready", true, None);
        assert!(stopped.stopped && stopped.error.is_none());
        let limit = monocode_core::usage::Limit {
            resets_at: Some(1_800_000_000_000),
            resume_at_reset: false,
        };
        let limited = turn_end("Usage limit reached", false, Some(&limit));
        assert_eq!(limited.error.as_deref(), Some("Usage limit reached"));
        assert!(limited.usage_limited);
        assert_eq!(limited.resets_at, Some(1_800_000_000_000));
        let failed = turn_end("503 Service Unavailable", false, None);
        assert_eq!(failed.error.as_deref(), Some("503 Service Unavailable"));
        assert!(!failed.usage_limited);
    }
}
