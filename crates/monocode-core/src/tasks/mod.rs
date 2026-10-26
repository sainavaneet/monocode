//! Durable development tasks that Mono leads from request to verification.
//!
//! Records live in `native_task*` tables beside the native sessions. Every
//! mutation names the revision it read and fails on a stale one, so a stale
//! writer cannot overwrite newer requirements or resolve a superseded decision.
pub mod brief;
pub mod checks;
pub mod completion;
pub mod coordinator;
pub mod events;
pub mod findings;
mod launch;
pub mod model;
pub mod review;
pub mod states;
pub mod store;
pub mod verify;
pub mod worker;
pub mod workspaces;

pub use completion::{complete, final_report, live_verifications, FinalReport};
pub use events::{DeliveryOutcome, DeliveryState, Event, Lease, NewEvent, PendingDelivery};
pub use launch::{confirm_started, start_failed};
pub use model::{
    AcceptanceCriterion, Assignment, Attempt, DecisionRequest, DecisionScope, DecisionState,
    NewAttempt, NewDecision, NewTask, Task, Verification, Workspace,
};
pub use review::{Finding, Review, ReviewOutcome};
pub use states::{
    authorize, AssignmentState, AttemptState, Eligibility, Precondition, Recovery, Requester,
    State, Subject, TaskLifecycle, TaskStage, Transition, TRANSITIONS,
};
pub use store::TaskStore;
pub use verify::{Round, RoundOutcome};
pub use worker::{Claim, ClaimOutcome, WorkerQuestion};

/// Why a task store operation was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskError {
    NotFound(String),
    /// The caller read `expected`; the record is now at `actual`.
    Stale {
        subject: String,
        expected: u64,
        actual: u64,
    },
    /// Not in the transition table. `from` is None when the state belongs to
    /// another kind of subject.
    IllegalTransition {
        from: Option<State>,
        to: State,
    },
    NotAllowed {
        requester: Requester,
        from: State,
        to: State,
    },
    Precondition(Precondition),
    /// A decision or delivery is not in a state that accepts the change.
    InvalidState {
        subject: String,
        state: String,
    },
    LeaseHeld {
        resource: String,
        holder: String,
        expires_at: u64,
    },
    Invalid(String),
    Storage(String),
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(subject) => write!(f, "{subject} was not found."),
            Self::Stale {
                subject,
                expected,
                actual,
            } => write!(
                f,
                "{subject} changed (revision {actual}, expected {expected}). Reload and retry."
            ),
            Self::IllegalTransition {
                from: Some(from),
                to,
            } => write!(
                f,
                "A {} cannot move from {} to {}.",
                from.machine(),
                from.as_str(),
                to.as_str()
            ),
            Self::IllegalTransition { from: None, to } => {
                write!(f, "This record has no {} state.", to.machine())
            }
            Self::NotAllowed {
                requester,
                from,
                to,
            } => write!(
                f,
                "The {} may not move a {} from {} to {}.",
                requester.as_str(),
                from.machine(),
                from.as_str(),
                to.as_str()
            ),
            Self::Precondition(precondition) => write!(f, "Not yet: {precondition:?}."),
            Self::InvalidState { subject, state } => write!(f, "{subject} is already {state}."),
            Self::LeaseHeld {
                resource,
                holder,
                expires_at,
            } => write!(f, "{resource} is held by {holder} until {expires_at}."),
            Self::Invalid(message) | Self::Storage(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for TaskError {}

impl From<rusqlite::Error> for TaskError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

impl From<serde_json::Error> for TaskError {
    fn from(error: serde_json::Error) -> Self {
        Self::Storage(error.to_string())
    }
}
