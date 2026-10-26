//! A task worker's turn ending is reported to recovery
//! (`monocode_core::tasks::recovery::turn_ended`), which classifies a failure
//! and recovers it, or hands a session that ended without a result to Mono.
//! Recovery ignores every session that is not the live attempt of a task
//! worker, and does nothing while the task coordinator is off.
use super::*;
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

impl Conversation {
    /// Whether queued follow-ups wait (a stopped session pauses them).
    #[cfg(test)]
    pub(crate) fn follow_ups_paused(&self) -> bool {
        self.follow_ups.state.paused
    }

    /// Called when a turn finishes; Mono chats and remote sessions are never
    /// task workers.
    pub(super) fn report_task_turn(&mut self, stopped: bool, cx: &mut Context<Self>) {
        if self.settings.mono.is_some() || self.is_remote() {
            return;
        }
        let end = turn_end(&self.status, stopped, self.settings.usage_limit.as_ref());
        let store = self.store.clone();
        let session = self.summary.id;
        cx.background_executor()
            .spawn(async move {
                let now = monocode_core::time::now_ms();
                let result = locked(&store).map_err(|e| e.to_string()).and_then(|store| {
                    recovery::turn_ended(&store, session, &end, now).map_err(|e| e.to_string())
                });
                if let Err(error) = result {
                    eprintln!("Task recovery for session {session}: {error}");
                }
            })
            .detach();
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
