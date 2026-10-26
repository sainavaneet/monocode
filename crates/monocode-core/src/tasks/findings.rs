//! What review and verification record: findings, review outcomes, the
//! review record, and the helpers that name snapshot refs and check scopes.
use serde::{Deserialize, Serialize};
use std::path::Path;

/// How much failing output a worker brief repeats.
const BRIEF_OUTPUT_CHARS: usize = 1_500;
/// Characters that make a file scope a glob pattern.
const GLOB_CHARS: &[char] = &['*', '?', '['];

/// Something review found in a claim, its workspace or its checks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Finding {
    /// The claim no longer belongs to the current run of the assignment.
    Superseded {
        reason: String,
    },
    /// The assignment's requirements changed after the attempt started.
    StaleRevision {
        reason: String,
    },
    OutOfScope {
        path: String,
    },
    FailedCheck {
        check: String,
        exit_code: Option<i32>,
        timed_out: bool,
        output: String,
    },
    /// A required acceptance criterion's check failed on the task's result.
    CriterionNotMet {
        criterion: String,
        check: String,
        exit_code: Option<i32>,
        timed_out: bool,
        output: String,
    },
    /// Running the checks changed the workspace, so no result describes the
    /// snapshot they started from.
    ModifiedByCheck {
        files: Vec<String>,
    },
    /// The step needs a shell, escapes the workspace or is prose; not run.
    UnrunnableCheck {
        check: String,
        reason: String,
    },
    /// A plan check the worker's validation does not mention. Informational:
    /// review runs every check itself.
    UnreportedCheck {
        check: String,
    },
    /// The plan gives no check, so nothing could verify the work.
    NoChecks,
    /// The worker claimed success but listed work still to do.
    RemainingWork {
        item: String,
    },
    Workspace {
        reason: String,
    },
    /// Review deliberately did not check this mechanically.
    NotChecked {
        reason: String,
    },
    CorrectionLimit {
        corrections: usize,
    },
    /// The worktree has commits although the task's policy does not allow
    /// it to commit (revoked while the worker ran).
    UnauthorizedCommits {
        commits: Vec<String>,
    },
    /// Merging the branch onto the task's integration branch conflicted.
    IntegrationConflict {
        branch: String,
        onto: String,
        files: Vec<String>,
    },
    /// The worktree holds changes no commit has, so integration cannot
    /// merge them.
    UncommittedWork {
        files: Vec<String>,
    },
}

fn check_failure(check: &str, exit_code: Option<i32>, timed_out: bool, output: &str) -> String {
    let how = match (timed_out, exit_code) {
        (true, _) => "timed out".to_owned(),
        (false, Some(code)) => format!("failed with exit code {code}"),
        (false, None) => "was killed".to_owned(),
    };
    if output.is_empty() {
        return format!("`{check}` {how} with no output.");
    }
    let start = output.chars().count().saturating_sub(BRIEF_OUTPUT_CHARS);
    let end: String = output.chars().skip(start).collect();
    format!("`{check}` {how}. End of its output:\n{end}")
}

impl Finding {
    /// Mechanically wrong: the work goes back to a worker.
    pub(super) fn is_corrective(&self) -> bool {
        matches!(
            self,
            Self::StaleRevision { .. }
                | Self::OutOfScope { .. }
                | Self::FailedCheck { .. }
                | Self::CriterionNotMet { .. }
                | Self::ModifiedByCheck { .. }
                | Self::UnauthorizedCommits { .. }
                | Self::IntegrationConflict { .. }
                | Self::UncommittedWork { .. }
        )
    }

    /// Not decidable without judgement.
    pub(super) fn needs_mono(&self) -> bool {
        matches!(
            self,
            Self::UnrunnableCheck { .. }
                | Self::NoChecks
                | Self::RemainingWork { .. }
                | Self::Workspace { .. }
                | Self::NotChecked { .. }
                | Self::CorrectionLimit { .. }
        )
    }

    /// One line for the next worker's brief.
    pub fn describe(&self) -> String {
        match self {
            Self::Superseded { reason } => format!("The claim was superseded: {reason}"),
            Self::StaleRevision { reason } => reason.clone(),
            Self::OutOfScope { path } => {
                format!("{path} was changed outside the files you may change.")
            }
            Self::FailedCheck {
                check,
                exit_code,
                timed_out,
                output,
            } => check_failure(check, *exit_code, *timed_out, output),
            Self::CriterionNotMet {
                criterion,
                check,
                exit_code,
                timed_out,
                output,
            } => format!(
                "Acceptance criterion {criterion} is not met: {}",
                check_failure(check, *exit_code, *timed_out, output)
            ),
            Self::ModifiedByCheck { files } => format!(
                "A check modified the workspace: {}. Checks must leave tracked and untracked files unchanged (ignore build outputs).",
                files.join(", ")
            ),
            Self::UnrunnableCheck { check, reason } => {
                format!("`{check}` could not be run: {reason}")
            }
            Self::UnreportedCheck { check } => {
                format!("`{check}` was not in the validation you reported.")
            }
            Self::NoChecks => "The plan gave no check to verify the work.".into(),
            Self::RemainingWork { item } => format!("You reported remaining work: {item}"),
            Self::Workspace { reason } => format!("The workspace could not be inspected: {reason}"),
            Self::NotChecked { reason } => reason.clone(),
            Self::CorrectionLimit { corrections } => {
                format!("The work was already sent back {corrections} times.")
            }
            Self::UnauthorizedCommits { commits } => format!(
                "Commits made without commit authority: {}. The Mono's policy no longer allows this task to commit: undo them (keep the changes uncommitted, for example with git reset --soft to the base commit) and report again.",
                commits.join(", ")
            ),
            Self::IntegrationConflict {
                branch,
                onto,
                files,
            } => format!(
                "Merging your branch onto the task's integration branch {branch} (at {onto}) conflicted in {}. Rebase your branch onto it (git rebase {branch}), resolve the conflicts keeping the intent of both changes, make sure the checks pass, commit, and report again.",
                files.join(", ")
            ),
            Self::UncommittedWork { files } => format!(
                "Your worktree has uncommitted changes ({}); integration merges committed work only. Commit them on your branch and report again.",
                files.join(", ")
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewOutcome {
    /// Every check ran and passed on the inspected snapshot.
    Accepted,
    /// Sent back to Queued with the findings for the next attempt.
    Correction,
    /// Left for Mono's judgement (a `mono_review` delivery).
    NeedsMonoReview,
    /// The claim is no longer current; nothing changed.
    Superseded,
}

/// One review, persisted as an `assignment.reviewed` event: `reviewed:<attempt>`
/// for a worker's claim, `verified:<task>@<round>` when the task's acceptance
/// criteria sent the work back. A correction is a review with that outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    pub attempt_id: String,
    pub assignment_id: String,
    pub outcome: ReviewOutcome,
    /// The workspace snapshot inspected and checked.
    pub checkpoint: Option<String>,
    pub changed_files: Vec<String>,
    pub findings: Vec<Finding>,
    /// The verification records of the checks that ran.
    pub verifications: Vec<String>,
}

/// A Git ref component from a record id. Injective: ASCII letters, digits
/// and `-` stay, every other byte becomes `_XX` (hex), so `a.1` and `a-1`
/// never share a ref. The result is always a valid ref component.
pub(super) fn ref_part(text: &str) -> String {
    if text.is_empty() {
        return "_".into();
    }
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' {
                char::from(b).to_string()
            } else {
                format!("_{b:02X}")
            }
        })
        .collect()
}

/// `refs/monocode/tasks/<task>/<parts...>`, each part escaped. Callers use
/// fixed first parts (`attempts`, `criteria`, `rounds`, `refresh`) so no
/// two kinds of snapshot share a name.
pub(super) fn task_ref(task: &str, parts: &[&str]) -> String {
    let mut reference = format!("refs/monocode/tasks/{}", ref_part(task));
    for part in parts {
        reference.push('/');
        reference.push_str(&ref_part(part));
    }
    reference
}

/// The changed paths outside `files`, or why scope cannot be checked
/// mechanically: a glob scope (there is no glob matcher here), or a plan
/// mistake (no scope at all, or a blank one). Mono judges those. `.` is the
/// whole workspace.
pub(super) fn out_of_scope(paths: &[String], files: &[String]) -> Result<Vec<String>, String> {
    if files.is_empty() {
        return Err(
            "The assignment has no file scope, so its changes cannot be checked against one."
                .into(),
        );
    }
    let mut scopes = Vec::new();
    for scope in files {
        let mut scope = scope.trim();
        if scope.is_empty() {
            return Err("The assignment has a blank file scope.".into());
        }
        while let Some(rest) = scope.strip_prefix("./") {
            scope = rest;
        }
        let scope = scope.trim_end_matches('/');
        if scope.is_empty() || scope == "." {
            return Ok(Vec::new());
        }
        if scope.contains(GLOB_CHARS) {
            return Err(format!(
                "The file scope `{scope}` is a pattern; review cannot check the changed files against it."
            ));
        }
        scopes.push(scope);
    }
    Ok(paths
        .iter()
        .filter(|path| !scopes.iter().any(|s| Path::new(path).starts_with(s)))
        .cloned()
        .collect())
}
