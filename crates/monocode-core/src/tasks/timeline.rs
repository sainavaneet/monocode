//! The task timeline the monitoring card expands: decisions, launches,
//! failures, corrections and verifications, read from the task's events.
//!
//! Text is built from fixed phrases, record ids, account ids and redacted
//! reasons only (`handoff::redact`); free text a worker, a provider or the
//! user wrote (questions, requirements, prompts) never appears here.
use super::events::Event;
use super::findings::ReviewOutcome;
use super::handoff::redact;
use super::integration::Outcome;
use std::collections::HashMap;

/// Entries kept per task, newest first.
pub const TIMELINE_LIMIT: usize = 40;
/// The event kinds [`entry`] may turn into an entry; the view reads only
/// the newest of these, never a task's whole history.
pub const KINDS: &[&str] = &[
    "decision.requested",
    "decision.provider_question",
    "decision.escalated",
    "decision.answered",
    "attempt.started",
    "attempt.handoff",
    "attempt.failure",
    "attempt.dispatch_failed",
    "attempt.start_failed",
    "launch.unconfirmed",
    "assignment.failed",
    "recovery.blocked",
    "recovery.exhausted",
    "recovery.ended_without_result",
    "recovery.stop_stalled",
    "recovery.workspace_missing",
    "task.workspace_failed",
    "task.account_unavailable",
    "task.blocked",
    "task.steered",
    "assignment.requeued",
    "assignment.queued",
    "task.sent_back",
    "assignment.mono_note",
    "task.pause_requested",
    "task.resume_requested",
    "task.cancel_requested",
    "assignment.reviewed",
    "assignment.accepted",
    "task.verifying",
    "task.verified",
    "task.verification_failed",
    "verification.invalidated",
    "verification.waived",
    "task.completed",
    "task.integrating",
    "task.integration",
    "task.needs_integration",
    "workspace.owned_notice",
];
/// The longest reason an entry shows.
const REASON_LIMIT: usize = 160;

/// What kind of step an entry records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TimelineKind {
    Decision,
    Launch,
    Failure,
    Correction,
    Verification,
    Integration,
}

impl TimelineKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Decision => "Decision",
            Self::Launch => "Launch",
            Self::Failure => "Failure",
            Self::Correction => "Correction",
            Self::Verification => "Verification",
            Self::Integration => "Integration",
        }
    }
}

/// One step of a task, as the timeline shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineEntry {
    /// The event's append order; entries are unique by it.
    pub seq: i64,
    pub at: u64,
    pub kind: TimelineKind,
    pub assignment_id: Option<String>,
    pub text: String,
}

/// Who ran an attempt, by id only: provider and account id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Runner {
    pub provider: String,
    pub account_id: Option<String>,
}

/// What the timeline needs besides the events: assignment titles and the
/// provider and account of each attempt.
#[derive(Default)]
pub struct Names {
    pub titles: HashMap<String, String>,
    pub runners: HashMap<String, Runner>,
}

impl Names {
    fn assignment(&self, event: &Event) -> String {
        event
            .assignment_id
            .as_deref()
            .map(|id| self.titles.get(id).cloned().unwrap_or_else(|| safe(id)))
            .unwrap_or_else(|| "The task".into())
    }

    fn runner(&self, event: &Event) -> String {
        let Some(runner) = event.attempt_id.as_ref().and_then(|a| self.runners.get(a)) else {
            return String::new();
        };
        match &runner.account_id {
            Some(account) => format!(" on {}/{}", safe(&runner.provider), safe(account)),
            None => format!(" on {}", safe(&runner.provider)),
        }
    }
}

/// A reason as the card may show it: secrets removed, length bounded.
pub fn safe(text: &str) -> String {
    let clean = redact(text);
    let mut shown: String = clean.chars().take(REASON_LIMIT).collect();
    if clean.chars().count() > REASON_LIMIT {
        shown.push('…');
    }
    shown
}

fn text_field(event: &Event, key: &str) -> Option<String> {
    event.payload[key]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(safe)
}

fn with_reason(base: String, event: &Event) -> String {
    match text_field(event, "reason") {
        Some(reason) => format!("{base}: {reason}"),
        None => base,
    }
}

/// Who asked, from the payload: redacted, as all payload text.
fn requester(event: &Event) -> String {
    event.payload["requester"]
        .as_str()
        .map_or_else(|| "the app".into(), safe)
}

/// The timeline entry an event makes, if it is one the timeline shows.
pub fn entry(event: &Event, names: &Names) -> Option<TimelineEntry> {
    use TimelineKind::*;
    let who = names.assignment(event);
    let from = event.payload["from"].as_str().unwrap_or_default();
    let (kind, text) = match event.kind.as_str() {
        "decision.requested" => (Decision, format!("{who} asked a question")),
        "decision.provider_question" => (Decision, format!("{who} asked a question")),
        "decision.escalated" => (Decision, "Mono asked you to decide".to_owned()),
        "decision.answered" => {
            let by = event.payload["by"]
                .as_str()
                .map_or_else(|| "someone".into(), safe);
            (Decision, format!("Answered by {by}"))
        }
        "attempt.started" => (Launch, format!("Started {who}{}", names.runner(event))),
        "attempt.handoff" => {
            let category = event.payload["failure"]["category"]
                .as_str()
                .map(safe)
                .unwrap_or_else(|| "a stop".into());
            (
                Launch,
                format!("Relaunched {who}{} after {category}", names.runner(event)),
            )
        }
        "attempt.failure" => {
            let category = text_field(event, "category").unwrap_or_else(|| "failure".into());
            let account = text_field(event, "accountId")
                .map(|a| format!(" (account {a})"))
                .unwrap_or_default();
            (
                Failure,
                with_reason(format!("{who} stopped: {category}{account}"), event),
            )
        }
        "attempt.dispatch_failed" | "attempt.start_failed" | "launch.unconfirmed" => {
            (Failure, with_reason(format!("{who} did not start"), event))
        }
        "assignment.failed" => (Failure, format!("{who} failed")),
        "recovery.blocked" => (Failure, with_reason(format!("{who} is blocked"), event)),
        "recovery.exhausted" => (Failure, format!("{who} used its retries")),
        "recovery.ended_without_result" => (Failure, format!("{who} ended without a result")),
        "recovery.stop_stalled" => (Failure, format!("{who} has not stopped yet")),
        "recovery.workspace_missing" | "task.workspace_failed" => {
            (Failure, format!("{who}: its workspace is missing or moved"))
        }
        "task.account_unavailable" => (Failure, "No eligible account to run on".to_owned()),
        "task.blocked" => (Failure, "The task is blocked".to_owned()),
        "task.steered" => {
            let revision = event.payload["planRevision"]
                .as_u64()
                .map(|r| format!(" (plan revision {r})"))
                .unwrap_or_default();
            (
                Correction,
                format!("Requirements changed by {}{revision}", requester(event)),
            )
        }
        "assignment.requeued" => (Correction, format!("{who} was stopped and re-queued")),
        "assignment.queued" if matches!(from, "accepted" | "needs_review" | "failed") => (
            Correction,
            format!("{who} was sent back by {}", requester(event)),
        ),
        "task.sent_back" => (Correction, "Work was sent back for changes".to_owned()),
        "assignment.mono_note" => (Correction, format!("Mono left a note for {who}")),
        "task.pause_requested" => (Correction, format!("Paused by {}", requester(event))),
        "task.resume_requested" => (Correction, format!("Resumed by {}", requester(event))),
        "task.cancel_requested" => (Correction, format!("Cancelled by {}", requester(event))),
        "assignment.reviewed" => {
            let outcome = serde_json::from_value(event.payload["outcome"].clone()).ok();
            match outcome {
                Some(ReviewOutcome::Accepted) => {
                    (Verification, format!("Review passed the checks of {who}"))
                }
                Some(ReviewOutcome::Correction) => {
                    (Correction, format!("Review sent {who} back with findings"))
                }
                Some(ReviewOutcome::NeedsMonoReview) => {
                    (Verification, format!("{who} is waiting for Mono's review"))
                }
                _ => return None,
            }
        }
        "assignment.accepted" => (Verification, format!("{who} was accepted")),
        "task.verifying" => (Verification, "Verification started".to_owned()),
        "task.verified" => (Verification, "Verification passed".to_owned()),
        "task.verification_failed" => (Verification, "Verification failed".to_owned()),
        "verification.invalidated" => (
            Verification,
            "A verification no longer matches the workspace".to_owned(),
        ),
        "verification.waived" => {
            let check = text_field(event, "check").unwrap_or_else(|| "a check".into());
            let reason = text_field(event, "reason").unwrap_or_default();
            (
                Verification,
                format!("Mono judged `{check}` met without running it, waived: {reason}"),
            )
        }
        "task.completed" => (Verification, "The task is complete".to_owned()),
        "task.integrating" => (Integration, "Integration started".to_owned()),
        "task.integration" => (Integration, integration_text(event)?),
        "task.needs_integration" => (Decision, "Integration needs Mono's judgement".to_owned()),
        "workspace.owned_notice" => (
            Launch,
            format!("{who} waits for its worktree, which another attempt still uses"),
        ),
        _ => return None,
    };
    Some(TimelineEntry {
        seq: event.seq,
        at: event.created_at,
        kind,
        assignment_id: event.assignment_id.clone(),
        text,
    })
}

/// What one integration pass ended in: counts and ids only, never a path
/// or the text of a Git error.
fn integration_text(event: &Event) -> Option<String> {
    let merged = event.payload["merges"].as_array().map_or(0, Vec::len);
    let outcome: Outcome = serde_json::from_value(event.payload["outcome"].clone()).ok()?;
    Some(match outcome {
        Outcome::Clean => format!("Integration merged {merged} branch(es) cleanly"),
        Outcome::Conflict {
            assignment, files, ..
        } => format!(
            "Integration conflicted on {} file(s) merging {}",
            files.len(),
            safe(&assignment)
        ),
        Outcome::Uncommitted {
            assignment, files, ..
        } => format!(
            "{} has {} file(s) never committed",
            safe(&assignment),
            files.len()
        ),
        Outcome::NoCommits { assignment, .. } => {
            format!("{} has no commits to merge", safe(&assignment))
        }
        Outcome::Failed { .. } => "Integration failed".to_owned(),
    })
}

/// The task's timeline, newest first, at most [`TIMELINE_LIMIT`] entries.
pub fn entries(events: &[Event], names: &Names) -> Vec<TimelineEntry> {
    events
        .iter()
        .rev()
        .filter_map(|event| entry(event, names))
        .take(TIMELINE_LIMIT)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every event kind the task store writes (and a few it does not).
    const WRITTEN: &[&str] = &[
        "task.created",
        "task.plan_saved",
        "task.ready",
        "task.implementing",
        "task.reviewing",
        "task.integrating",
        "task.verifying",
        "task.completed",
        "task.active",
        "task.waiting",
        "task.paused",
        "task.blocked",
        "task.cancelled",
        "task.resumed",
        "task.replanning",
        "task.policy",
        "task.final_report",
        "task.steered",
        "task.steer_assess",
        "task.sent_back",
        "task.verified",
        "task.verification_failed",
        "task.verification_waiting",
        "task.needs_integration",
        "task.waiting_cause",
        "task.restart_held",
        "task.takeover_held",
        "task.restart_reconciled",
        "task.lease_taken_over",
        "task.account_unavailable",
        "task.workspace_failed",
        "task.pause_requested",
        "task.resume_requested",
        "task.cancel_requested",
        "assignment.queued",
        "assignment.ready",
        "assignment.running",
        "assignment.requeued",
        "assignment.waiting_for_decision",
        "assignment.needs_review",
        "assignment.accepted",
        "assignment.failed",
        "assignment.cancelled",
        "assignment.reviewed",
        "assignment.resumed",
        "assignment.mono_note",
        "attempt.created",
        "attempt.reserved",
        "attempt.launch_intent",
        "attempt.briefed",
        "attempt.starting",
        "attempt.started",
        "attempt.running",
        "attempt.waiting",
        "attempt.succeeded",
        "attempt.failed",
        "attempt.interrupted",
        "attempt.cancelled",
        "attempt.claim",
        "attempt.blocked",
        "attempt.failure",
        "attempt.handoff",
        "attempt.dispatch_failed",
        "attempt.start_failed",
        "attempt.stop_requested",
        "attempt.parked",
        "attempt.late_claim",
        "attempt.held_owner",
        "launch.unconfirmed",
        "launch.orphaned",
        "decision.requested",
        "decision.provider_question",
        "decision.escalated",
        "decision.answered",
        "decision.inherited",
        "decision.resolved",
        "recovery.blocked",
        "recovery.exhausted",
        "recovery.ended_without_result",
        "recovery.stop_stalled",
        "recovery.workspace_missing",
        "recovery.stopped_by_user",
        "recovery.block_reason",
        "verification.invalidated",
        "verification.waived",
        "worker.result",
        "mono.inbox",
        "mono.inbox_failed",
        "mono.inbox_abandoned",
        "task.integration",
        "workspace.owned",
        "workspace.owned_notice",
        "unknown.kind",
    ];

    fn event(kind: &str) -> Event {
        let mut event = plain_event(kind);
        if kind == "task.integration" {
            event.payload = serde_json::json!({"merges": [], "outcome": {"state": "clean"}});
        }
        event
    }

    fn plain_event(kind: &str) -> Event {
        Event {
            seq: 1,
            id: kind.into(),
            task_id: "t".into(),
            assignment_id: Some("a".into()),
            attempt_id: None,
            kind: kind.into(),
            // The payload that makes conditional kinds produce an entry.
            payload: serde_json::json!({"from": "accepted", "outcome": "accepted"}),
            created_at: 1,
        }
    }

    #[test]
    fn every_kind_that_makes_an_entry_is_read_by_the_view() {
        let names = Names::default();
        for kind in WRITTEN.iter().chain(KINDS) {
            let shown = entry(&event(kind), &names).is_some();
            assert_eq!(shown, KINDS.contains(kind), "{kind}");
        }
    }

    #[test]
    fn integration_passes_show_their_outcome_by_counts_only() {
        let names = Names::default();
        let text = |outcome: serde_json::Value| {
            let mut e = event("task.integration");
            e.payload = serde_json::json!({"merges": [{}, {}], "outcome": outcome});
            entry(&e, &names).map(|x| (x.kind, x.text))
        };
        let shown = |o| text(o).unwrap();
        assert_eq!(
            shown(serde_json::json!({"state": "clean"})).1,
            "Integration merged 2 branch(es) cleanly"
        );
        let conflict = serde_json::json!({"state": "conflict", "assignment": "a", "branch": "b", "files": ["x.rs", "Bearer sk-ant-api03-SECRETSECRET"]});
        let (kind, line) = shown(conflict);
        assert_eq!(kind, TimelineKind::Integration);
        assert_eq!(line, "Integration conflicted on 2 file(s) merging a");
        assert!(shown(serde_json::json!({"state": "uncommitted", "assignment": "a", "branch": "b", "files": ["x"]})).1.contains("1 file(s) never committed"));
        assert!(
            shown(serde_json::json!({"state": "noCommits", "assignment": "a", "branch": "b"}))
                .1
                .contains("no commits")
        );
        let failed = shown(
            serde_json::json!({"state": "failed", "reason": "token ghp_abcdefghijklmnopqrstuvwxyz0123456789 denied"}),
        );
        assert_eq!(failed.1, "Integration failed");
        assert!(text(serde_json::json!({"state": "bogus"})).is_none());
    }

    #[test]
    fn ownership_waits_and_integration_decisions_show() {
        let names = Names::default();
        let wait = entry(&event("workspace.owned_notice"), &names).unwrap();
        assert!(wait.text.contains("waits for its worktree"));
        let need = entry(&event("task.needs_integration"), &names).unwrap();
        assert_eq!(need.kind, TimelineKind::Decision);
        assert!(entry(&event("workspace.owned"), &names).is_none());
    }
}
