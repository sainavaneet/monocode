//! The task timeline the monitoring card expands: decisions, launches,
//! failures, corrections and verifications, read from the task's events.
//!
//! Text is built from fixed phrases, record ids, account ids and redacted
//! reasons only (`handoff::redact`); free text a worker, a provider or the
//! user wrote (questions, requirements, prompts) never appears here.
use super::events::Event;
use super::findings::ReviewOutcome;
use super::handoff::redact;
use std::collections::HashMap;

/// Entries kept per task, newest first.
pub const TIMELINE_LIMIT: usize = 40;
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
}

impl TimelineKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Decision => "Decision",
            Self::Launch => "Launch",
            Self::Failure => "Failure",
            Self::Correction => "Correction",
            Self::Verification => "Verification",
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
            .map(|id| self.titles.get(id).cloned().unwrap_or_else(|| id.into()))
            .unwrap_or_else(|| "The task".into())
    }

    fn runner(&self, event: &Event) -> String {
        let Some(runner) = event.attempt_id.as_ref().and_then(|a| self.runners.get(a)) else {
            return String::new();
        };
        match &runner.account_id {
            Some(account) => format!(" on {}/{account}", runner.provider),
            None => format!(" on {}", runner.provider),
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

fn requester(event: &Event) -> &str {
    event.payload["requester"].as_str().unwrap_or("the app")
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
            let by = event.payload["by"].as_str().unwrap_or("someone");
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
        "task.completed" => (Verification, "The task is complete".to_owned()),
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

/// The task's timeline, newest first, at most [`TIMELINE_LIMIT`] entries.
pub fn entries(events: &[Event], names: &Names) -> Vec<TimelineEntry> {
    events
        .iter()
        .rev()
        .filter_map(|event| entry(event, names))
        .take(TIMELINE_LIMIT)
        .collect()
}
