//! Durable task records. Account fields hold account ids only, never keys or
//! tokens.
use super::states::{state_enum, AttemptState, Requester, TaskLifecycle, TaskStage};
use crate::orchestration::WorkspaceMode;
use serde::{Deserialize, Serialize};

/// One unit of a task's plan. The plan card's task type, extended with its
/// workspace, status, revision and verification expectations.
pub type Assignment = crate::orchestration::Task;

/// An observable outcome the task must show before it completes. It is
/// satisfied by the latest live [`Verification`] that names it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceCriterion {
    pub id: String,
    pub description: String,
    pub required: bool,
    /// Command lines that demonstrate the criterion, run like plan checks
    /// (no shell) on the task's result. None: Mono verifies it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewTask {
    pub id: String,
    /// The Mono that owns the task, when one does.
    pub mono_id: Option<String>,
    /// The project root the task works in.
    pub project: String,
    pub title: String,
    /// The user's request as given.
    pub request: String,
    pub criteria: Vec<AcceptanceCriterion>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub mono_id: Option<String>,
    pub project: String,
    pub title: String,
    pub request: String,
    pub stage: TaskStage,
    pub lifecycle: TaskLifecycle,
    pub criteria: Vec<AcceptanceCriterion>,
    /// Bumped each time the plan's assignments are replaced.
    pub plan_revision: u64,
    pub blocked_reason: Option<String>,
    pub revision: u64,
    /// Unix milliseconds.
    pub created_at: u64,
    pub updated_at: u64,
}

/// A launch intent. `operation_id` is unique, so retrying the same launch
/// resolves to the attempt already recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewAttempt {
    pub id: String,
    pub task_id: String,
    pub assignment_id: String,
    pub operation_id: String,
    pub provider: String,
    pub account_id: Option<String>,
}

/// One worker session working an assignment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub id: String,
    pub task_id: String,
    pub assignment_id: String,
    pub operation_id: String,
    pub provider: String,
    pub account_id: Option<String>,
    /// The worker session once the launch is reconciled.
    pub session_id: Option<String>,
    pub status: AttemptState,
    pub revision: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

/// Where an assignment's work happens, recorded before launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    pub id: String,
    pub task_id: String,
    pub assignment_id: Option<String>,
    pub mode: WorkspaceMode,
    /// The repository root the worktree or checkout belongs to.
    pub repository: String,
    pub path: String,
    pub branch: Option<String>,
    pub base_commit: Option<String>,
    /// The coordinator or session that owns the workspace.
    pub owner: String,
    pub revision: u64,
    pub created_at: u64,
}

state_enum!(
    /// What a decision request blocks while it is open.
    DecisionScope {
        Assignment => "assignment",
        Dependents => "dependents",
        Task => "task",
    }
);
state_enum!(
    DecisionState {
        Open => "open",
        Answered => "answered",
        /// The answer reached the asker and was acknowledged.
        Resolved => "resolved",
    }
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewDecision {
    pub id: String,
    pub task_id: String,
    pub assignment_id: Option<String>,
    pub attempt_id: Option<String>,
    pub question: String,
    /// Why it matters, the recommendation and alternatives, as asked.
    pub context: String,
    pub scope: DecisionScope,
    pub asked_by: Requester,
}

/// A question for Mono or the user, with its answer once given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecisionRequest {
    pub id: String,
    pub task_id: String,
    pub assignment_id: Option<String>,
    pub attempt_id: Option<String>,
    pub question: String,
    pub context: String,
    pub scope: DecisionScope,
    pub asked_by: Requester,
    pub state: DecisionState,
    pub answer: Option<String>,
    pub answered_by: Option<Requester>,
    pub revision: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

/// A check run against a checkpoint commit. A later change to the workspace
/// invalidates it rather than letting a stale result stand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verification {
    pub id: String,
    pub task_id: String,
    pub assignment_id: Option<String>,
    pub criterion_id: Option<String>,
    pub checkpoint_commit: String,
    pub check: String,
    pub passed: bool,
    /// A reference to the evidence, such as a summary or an output path.
    pub evidence: String,
    pub invalidated: bool,
    pub created_at: u64,
}
