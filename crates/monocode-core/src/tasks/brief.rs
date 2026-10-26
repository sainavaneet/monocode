//! The first prompt of a task worker. Workers do not inherit Mono's
//! conversation, so it carries everything: objective, workspace, scope,
//! dependencies, expected checks and how to report.
use super::autonomy::Autonomy;
use super::review::{Review, ReviewOutcome};
use super::{Assignment, DecisionRequest, DecisionState, Task, Workspace};
use crate::orchestration::WorkspaceMode;

const REPORTING: &str = "Report through the app commands (`monocode-gpui app ACTION --json '{...}'`); the task and attempt come from this session:\n\
- tasks.worker.status {} shows your assignment and answered decisions.\n\
- tasks.worker.ask {question, context?, scope?} when you need a decision; then end your turn: the answer arrives as your next message. Do not report success while a question is unanswered.\n\
- tasks.worker.blocked {reason} if you cannot continue.\n\
- tasks.worker.result {outcome: \"succeeded\"|\"failed\", summary, workspace, commits, validation, evidence, remainingWork, limitations} once, when done. It is a claim Mono reviews against the acceptance criteria; it does not complete the task.";

fn list(items: &[String], none: &str) -> String {
    if items.is_empty() {
        none.to_owned()
    } else {
        items.join(", ")
    }
}

/// What review found in the previous attempt, for the one that redoes it.
fn correction_text(correction: Option<&Review>) -> String {
    let Some(review) = correction else {
        return String::new();
    };
    let findings: Vec<String> = review
        .findings
        .iter()
        .map(|finding| format!("- {}", finding.describe()))
        .collect();
    let how = if review.outcome == ReviewOutcome::Correction {
        "Review sent back the previous attempt"
    } else {
        "Mono sent back the previous attempt after reviewing it"
    };
    format!(
        "{how} ({}). Address these findings first:\n{}\n\n",
        review.attempt_id,
        findings.join("\n")
    )
}

/// The event that keeps Mono's reason for sending `attempt` back.
pub fn mono_note_event(attempt: &str) -> String {
    format!("mono-note:{attempt}")
}

/// Questions asked for this assignment, with their answers: a replacement
/// worker takes over where the earlier one stopped.
fn decisions_text(decisions: &[DecisionRequest]) -> String {
    if decisions.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = decisions
        .iter()
        .map(|d| match (&d.answer, d.state) {
            (Some(answer), DecisionState::Answered | DecisionState::Resolved) => {
                format!("- Decision {}: {}\n  Answer: {answer}", d.id, d.question)
            }
            _ => format!(
                "- Decision {}: {}\n  Not answered yet; the answer arrives as a message.",
                d.id, d.question
            ),
        })
        .collect();
    format!(
        "Questions already asked for this assignment and their answers:\n{}\n\n",
        lines.join("\n")
    )
}

/// The brief for `assignment` in `workspace`. A checkout workspace is the
/// user's own working copy, so its worker only reads: the brief lists no
/// writable files and forbids edits. `correction` is the review that sent
/// the previous attempt back, if one did; `decisions` are the questions
/// asked for the assignment so far. `authority` is the task's effective
/// policy: whether the worker may commit in its worktree. Merging, pushing
/// and deploying are Mono's integration steps, never a worker's.
pub fn worker_brief(
    task: &Task,
    assignment: &Assignment,
    workspace: &Workspace,
    correction: Option<&Review>,
    decisions: &[DecisionRequest],
    authority: &Autonomy,
) -> String {
    let commits = if authority.commit {
        "Commit your work there"
    } else {
        "Do not commit: leave your changes uncommitted there for Mono to review"
    };
    let (place, rules) = match workspace.mode {
        WorkspaceMode::Worktree => (
            format!(
                "your own worktree {} on branch {} from commit {}. {commits}; never touch the main checkout. Leave merging, pushing and deploying to Mono.",
                workspace.path,
                workspace.branch.as_deref().unwrap_or("?"),
                workspace.base_commit.as_deref().unwrap_or("?"),
            ),
            format!(
                "Files you may change: {}. If you need a change outside them, report it as blocked.",
                assignment.files.join(", ")
            ),
        ),
        WorkspaceMode::Checkout => (
            format!(
                "the user's project checkout {}, read-only.",
                workspace.path
            ),
            format!(
                "This is an investigation. Inspect only: make no edits, create or delete no files, and run no command that changes the checkout or its Git state. Focus on: {}. Report your findings with tasks.worker.result.",
                assignment.files.join(", ")
            ),
        ),
    };
    format!(
        "You are a worker Mono launched for task \"{task_title}\", assignment {id}: {title}.\n\
         You do not have Mono's conversation; everything you need is here.\n\n\
         Objective:\n{prompt}\n\n\
         {corrections}\
         {decisions}\
         Workspace: {place}\n\
         {rules}\n\
         Depends on (already accepted): {deps}\n\
         Expected verification: {checks}\n\n\
         {REPORTING}",
        task_title = task.title,
        id = assignment.id,
        title = assignment.title,
        prompt = assignment.prompt,
        corrections = correction_text(correction),
        decisions = decisions_text(decisions),
        deps = list(&assignment.depends_on, "none"),
        checks = list(
            &assignment.verification,
            "run focused checks and report them"
        ),
    )
}
