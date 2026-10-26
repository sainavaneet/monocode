//! The durable hand-off a replacement worker starts from when the worker it
//! replaces failed: objective, revision, decisions, workspace, completed
//! work, validation, pending questions and the next step. Never credentials:
//! it holds ids, text the task already has, Git facts from the workspace and
//! a redacted failure reason.
//!
//! Recorded once per replacement as the event `handoff:<attempt>` and turned
//! into a section of the replacement's brief.
use super::events::NewEvent;
use super::worker::{event, payload_of};
use super::{Assignment, Attempt, DecisionState, Task, TaskError, TaskStore, Workspace};
use crate::orchestration::WorkspaceMode;
use serde::{Deserialize, Serialize};
use std::path::Path;

const HANDOFF: &str = "attempt.handoff";
/// Bounds what the record copies from the workspace.
const MAX_COMMITS: usize = 30;
const MAX_FILES: usize = 60;
const MAX_REASON: usize = 300;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handoff {
    pub task_id: String,
    pub assignment_id: String,
    pub from_attempt: String,
    pub to_attempt: String,
    pub objective: String,
    /// The assignment and plan revisions the work is against.
    pub revision: Revision,
    pub decisions: Vec<HandedDecision>,
    pub workspace: HandedWorkspace,
    pub completed_work: CompletedWork,
    /// The checks the assignment expects.
    pub validation: Vec<String>,
    /// Questions still waiting for an answer.
    pub pending_questions: Vec<String>,
    pub next_step: String,
    pub failure: HandedFailure,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Revision {
    pub assignment: u64,
    pub plan: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandedDecision {
    pub id: String,
    pub question: String,
    pub answer: Option<String>,
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandedWorkspace {
    pub mode: WorkspaceMode,
    pub path: String,
    pub branch: Option<String>,
    pub base_commit: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletedWork {
    pub head: Option<String>,
    /// `<sha> <subject>` since the base commit, newest first.
    pub commits: Vec<String>,
    /// `git status --porcelain` lines: edits not yet committed.
    pub uncommitted: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandedFailure {
    pub category: String,
    pub reason: String,
}

pub(super) fn handoff_event(attempt: &str) -> String {
    format!("handoff:{attempt}")
}

/// The hand-off recorded for `attempt`, if it replaced another.
pub fn of(tasks: &TaskStore<'_>, attempt: &str) -> Result<Option<Handoff>, TaskError> {
    payload_of(tasks.conn, &handoff_event(attempt))?
        .map(|payload| serde_json::from_str(&payload).map_err(TaskError::from))
        .transpose()
}

/// Removes anything shaped like a secret and bounds the length, so provider
/// error text can be kept with the task.
pub fn redact(text: &str) -> String {
    static SECRET: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern = SECRET.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b(?:sk|pk|rk|ghp|gho|ghs|xox[abprs]|glpat|aws)[-_][A-Za-z0-9_\-]{6,}|\bBearer\s+\S+|\beyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-\.]+|\b[A-Za-z0-9]{40,}\b",
        )
        .unwrap()
    });
    pattern
        .replace_all(text, "[redacted]")
        .chars()
        .take(MAX_REASON)
        .collect()
}

fn lines(cwd: &Path, args: &[&str], limit: usize) -> Vec<String> {
    crate::git::git(cwd, args)
        .map(|out| {
            out.lines()
                .filter(|l| !l.trim().is_empty())
                .take(limit)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// What the workspace holds now: the work the failed attempt completed.
fn completed_work(workspace: &Workspace) -> CompletedWork {
    let cwd = Path::new(&workspace.path);
    let head = crate::git::git(cwd, &["rev-parse", "HEAD"])
        .ok()
        .map(|h| h.trim().to_owned());
    let commits = match (&workspace.mode, &workspace.base_commit) {
        (WorkspaceMode::Worktree, Some(base)) => lines(
            cwd,
            &["log", "--format=%h %s", &format!("{base}..HEAD")],
            MAX_COMMITS,
        ),
        _ => Vec::new(),
    };
    CompletedWork {
        head,
        commits,
        uncommitted: lines(cwd, &["status", "--porcelain"], MAX_FILES),
    }
}

/// Everything a replacement needs to continue `from`'s work.
pub(super) struct Inputs<'a> {
    pub task: &'a Task,
    pub assignment: &'a Assignment,
    pub workspace: &'a Workspace,
    pub from: &'a Attempt,
    pub to: &'a Attempt,
    pub category: &'a str,
    pub reason: &'a str,
}

pub(super) fn build(tasks: &TaskStore<'_>, inputs: &Inputs<'_>) -> Result<Handoff, TaskError> {
    let decisions =
        super::decisions::assignment_decisions(tasks, &inputs.task.id, &inputs.assignment.id)?;
    let pending_questions: Vec<String> = decisions
        .iter()
        .filter(|d| d.state == DecisionState::Open)
        .map(|d| format!("{}: {}", d.id, d.question))
        .collect();
    let next_step = if pending_questions.is_empty() {
        "Continue the objective from the workspace as it is now: review the completed work above, finish what remains, run the expected verification, then report once with tasks.worker.result.".to_owned()
    } else {
        "Review the completed work above and continue what the pending questions do not block; their answers arrive as messages. Report once with tasks.worker.result when done.".to_owned()
    };
    let w = inputs.workspace;
    Ok(Handoff {
        task_id: inputs.task.id.clone(),
        assignment_id: inputs.assignment.id.clone(),
        from_attempt: inputs.from.id.clone(),
        to_attempt: inputs.to.id.clone(),
        objective: inputs.assignment.prompt.clone(),
        revision: Revision {
            assignment: inputs.assignment.revision,
            plan: inputs.task.plan_revision,
        },
        decisions: decisions
            .iter()
            .map(|d| HandedDecision {
                id: d.id.clone(),
                question: d.question.clone(),
                answer: d.answer.clone(),
                state: d.state.as_str().into(),
            })
            .collect(),
        workspace: HandedWorkspace {
            mode: w.mode,
            path: w.path.clone(),
            branch: w.branch.clone(),
            base_commit: w.base_commit.clone(),
        },
        completed_work: completed_work(w),
        validation: inputs.assignment.verification.clone(),
        pending_questions,
        next_step,
        failure: HandedFailure {
            category: inputs.category.into(),
            reason: redact(inputs.reason),
        },
    })
}

/// Records the hand-off for the replacement (once).
pub(super) fn record(
    tasks: &TaskStore<'_>,
    handoff: &Handoff,
    to: &Attempt,
    now: u64,
) -> Result<(), TaskError> {
    let new: NewEvent = event(
        to,
        handoff_event(&to.id),
        HANDOFF,
        serde_json::to_value(handoff)?,
    );
    tasks.append_event(&new, now)?;
    Ok(())
}

fn bullets(items: &[String], none: &str) -> String {
    if items.is_empty() {
        format!("  {none}")
    } else {
        items
            .iter()
            .map(|i| format!("  - {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The brief section a replacement reads first.
pub fn text(handoff: &Handoff) -> String {
    let decisions: Vec<String> = handoff
        .decisions
        .iter()
        .map(|d| match &d.answer {
            Some(answer) => format!("{}: {} → {answer}", d.id, d.question),
            None => format!("{}: {} (not answered yet)", d.id, d.question),
        })
        .collect();
    let work = &handoff.completed_work;
    format!(
        "Hand-off from attempt {from} (it stopped: {category}, {reason}).\n\
         You replace that worker; it no longer runs. Its work is in the workspace below (HEAD {head}).\n\
         Commits since the base:\n{commits}\n\
         Uncommitted changes:\n{uncommitted}\n\
         Decisions:\n{decisions}\n\
         Pending questions:\n{pending}\n\
         Next step: {next}\n\n",
        from = handoff.from_attempt,
        category = handoff.failure.category.replace('_', " "),
        reason = handoff.failure.reason,
        head = work.head.as_deref().unwrap_or("unknown"),
        commits = bullets(&work.commits, "none"),
        uncommitted = bullets(&work.uncommitted, "none"),
        decisions = bullets(&decisions, "none"),
        pending = bullets(&handoff.pending_questions, "none"),
        next = handoff.next_step,
    )
}

#[cfg(test)]
#[path = "handoff_tests.rs"]
mod tests;
