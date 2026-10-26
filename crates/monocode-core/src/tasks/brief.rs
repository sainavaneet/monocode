//! The first prompt of a task worker. Workers do not inherit Mono's
//! conversation, so it carries everything: objective, workspace, scope,
//! dependencies, expected checks and how to report.
use super::{Assignment, Task, Workspace};
use crate::orchestration::WorkspaceMode;

const REPORTING: &str = "Report through the app commands (`monocode-gpui app ACTION --json '{...}'`); the task and attempt come from this session:\n\
- tasks.worker.status {} shows your assignment and answered decisions.\n\
- tasks.worker.ask {question, context?, scope?} when you need a decision; then wait for the answer.\n\
- tasks.worker.blocked {reason} if you cannot continue.\n\
- tasks.worker.result {outcome: \"succeeded\"|\"failed\", summary, workspace, commits, validation, evidence, remainingWork, limitations} once, when done. It is a claim Mono reviews against the acceptance criteria; it does not complete the task.";

fn list(items: &[String], none: &str) -> String {
    if items.is_empty() {
        none.to_owned()
    } else {
        items.join(", ")
    }
}

/// The brief for `assignment` in `workspace`. A checkout workspace is the
/// user's own working copy, so its worker only reads: the brief lists no
/// writable files and forbids edits.
pub fn worker_brief(task: &Task, assignment: &Assignment, workspace: &Workspace) -> String {
    let (place, rules) = match workspace.mode {
        WorkspaceMode::Worktree => (
            format!(
                "your own worktree {} on branch {} from commit {}. Commit your work there; never touch the main checkout.",
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
         Workspace: {place}\n\
         {rules}\n\
         Depends on (already accepted): {deps}\n\
         Expected verification: {checks}\n\n\
         {REPORTING}",
        task_title = task.title,
        id = assignment.id,
        title = assignment.title,
        prompt = assignment.prompt,
        deps = list(&assignment.depends_on, "none"),
        checks = list(
            &assignment.verification,
            "run focused checks and report them"
        ),
    )
}
