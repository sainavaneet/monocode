//! Reviewable orchestration plans. Workers share a checkout; overlapping scopes
//! are serialized even when they have no explicit dependency.
use crate::tasks::AssignmentState;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Component, Path},
};

/// orchestrationSummary.ts: what the sidebar shows of a lead's run, kept after
/// it ends so the lead still lists its agents.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Summary {
    pub tasks: Vec<SummaryTask>,
    /// The run is still dispatching or waiting on its workers.
    pub live: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SummaryTask {
    pub session: Option<i64>,
    pub title: String,
    pub provider: String,
    pub model: Option<String>,
    /// queued, running, completed, failed or blocked.
    pub status: String,
}
/// What the plan card and the sidebar show for one assignment of a run. The
/// label is defined once here, so a renamed label cannot drift from the
/// colors and the live state matched on the status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressStatus {
    NeedsInput,
    Saved,
    Queued,
    Working,
    Done,
    Failed,
    NeedsReview,
}
impl ProgressStatus {
    pub const ALL: [Self; 7] = [
        Self::NeedsInput,
        Self::Saved,
        Self::Queued,
        Self::Working,
        Self::Done,
        Self::Failed,
        Self::NeedsReview,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::NeedsInput => "Needs input",
            Self::Saved => "Saved",
            Self::Queued => "Queued",
            Self::Working => "Working",
            Self::Done => "Done",
            Self::Failed => "Failed",
            Self::NeedsReview => "Needs review",
        }
    }
    /// The run is still moving this assignment.
    pub fn live(self) -> bool {
        matches!(self, Self::Working | Self::Queued)
    }
}
impl SummaryTask {
    /// orchestrationTaskLabel's state, given whether the worker needs input.
    pub fn status_of(&self, summary: &Summary, needs_input: bool) -> ProgressStatus {
        if needs_input {
            return ProgressStatus::NeedsInput;
        }
        if !summary.live && matches!(self.status.as_str(), "running" | "queued") {
            return ProgressStatus::Saved;
        }
        match self.status.as_str() {
            "running" => ProgressStatus::Working,
            "completed" => ProgressStatus::Done,
            "failed" => ProgressStatus::Failed,
            "blocked" => ProgressStatus::NeedsReview,
            _ => ProgressStatus::Queued,
        }
    }
    /// orchestrationTaskLabel, given whether the worker needs input.
    pub fn label(&self, summary: &Summary, needs_input: bool) -> &'static str {
        self.status_of(summary, needs_input).label()
    }
}

/// One assignment of a plan. Durable task fields are optional in JSON, so plans
/// written before they existed still parse and new plans look the same.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub prompt: String,
    pub files: Vec<String>,
    #[serde(default, alias = "dependsOn")]
    pub depends_on: Vec<String>,
    /// Where the work happens; None leaves the choice to the coordinator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceMode>,
    /// Explicit repository within the task workspace; never inferred from prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Reviewed Git ref or commit to base an isolated worktree on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
    #[serde(default, skip_serializing_if = "is_queued")]
    pub status: AssignmentState,
    /// The durable record's revision; zero before it is persisted.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub revision: u64,
    /// The checks the worker is expected to run and report.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verification: Vec<String>,
}
fn is_queued(status: &AssignmentState) -> bool {
    *status == AssignmentState::Queued
}
fn is_zero(revision: &u64) -> bool {
    *revision == 0
}
/// How an assignment's workspace is provided.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    /// Read or edit the project's existing checkout.
    Checkout,
    /// A dedicated branch and worktree.
    Worktree,
}
impl WorkspaceMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Checkout => "checkout",
            Self::Worktree => "worktree",
        }
    }
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "checkout" => Some(Self::Checkout),
            "worktree" => Some(Self::Worktree),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub title: String,
    pub summary: String,
    pub tasks: Vec<Task>,
}
/// How a plan is written: the JSON block, its schema and the rules each
/// field follows. Shared by Orchestrator mode and a Mono's own context, so
/// a plan reads the same wherever it was written.
macro_rules! plan_format {
    () => {
        r#"Return a short explanation followed by exactly one JSON block between <monocode_orchestration> and </monocode_orchestration>.
Schema: {"title":"short title","summary":"approach","tasks":[{"id":"task-1","title":"assignment title","prompt":"complete self-contained instructions, validation and expected report","files":["src/path"],"depends_on":[],"workspace":"worktree","verification":["cargo test -p crate"],"repository":"optional repository directory","base_ref":"optional branch or commit"}]}
"workspace" is exactly "worktree" (an isolated branch and worktree; the default for work that changes files) or "checkout" (read-only investigation of the user's checkout), or omitted. "verification" is an array of executable command lines, not descriptions: checks execute directly in the worktree without a shell, so no pipes, redirection, shell variables, inline interpreter code or prose suffixes. For example, use "grep -qx ALPHA_QA_OK alpha.txt", never "cat alpha.txt prints exactly ALPHA_QA_OK". Put expected outcomes and manual checks in the task prompt. Workers must repeat each exact verification command in their reported validation strings.
"repository" is the repository directory (absolute or relative to the task workspace), and "base_ref" is a Git branch or commit. For work spanning multiple repositories, specify repository on every assignment. Each worktree assignment receives its own isolated branch; never instruct parallel workers to check out a shared branch. Mentioning a repository in the prompt does not select it. Never append branch names, paths, or explanations to workspace. Put branch/base requirements in the task prompt; these are requirements for review, not a guarantee that the coordinator uses a named branch.
Use 1 to 12 assignments, unique ASCII IDs, exact project-relative file or directory scopes ('.' for the whole project), and dependency IDs. Independent assignments may run concurrently up to the configured worker limit; overlapping scopes in the same repository run serially. Worker provider, account, access and billing follow the task policy and allowed-account list; do not promise a fixed concurrency or fallback provider. Include verification in the assignments. Never claim that workers have already started."#
    };
}
pub const PLAN_FORMAT: &str = plan_format!();
/// Orchestrator mode's prefix: a plan for this request, nothing else.
pub const INSTRUCTIONS: &str = concat!(
    "Prepare an orchestration plan for the user's request. Inspect the project with read-only tools. Do not edit or launch workers yet. The user will review and start the plan in MonoCode.\n",
    plan_format!()
);
const OPEN: &str = "<monocode_orchestration>";
const CLOSE: &str = "</monocode_orchestration>";
/// The reply as the transcript shows it, or None when nothing is hidden. The
/// plan card presents the plan, so its JSON block stays out of the prose, also
/// while it streams. A tag that is not followed by JSON stays visible.
pub fn display_text(text: &str) -> Option<String> {
    let mut shown = String::with_capacity(text.len());
    let mut rest = text;
    let mut hidden = false;
    while let Some((before, after)) = rest.split_once(OPEN) {
        shown.push_str(before);
        let body = after.trim_start();
        if !(body.is_empty() || body.starts_with('{')) {
            shown.push_str(OPEN);
            rest = after;
            continue;
        }
        hidden = true;
        rest = after.split_once(CLOSE).map_or("", |(_, tail)| tail);
    }
    shown.push_str(rest);
    // A streaming reply can end inside the opening tag itself.
    if let Some(start) = shown.rfind('<') {
        if shown.len() - start > 1 && OPEN.starts_with(&shown[start..]) {
            shown.truncate(start);
            hidden = true;
        }
    }
    hidden.then(|| shown.trim_end().to_owned())
}
pub fn parse(text: &str) -> Result<Plan, String> {
    let body = text
        .split_once(OPEN)
        .and_then(|(_, s)| s.split_once(CLOSE).map(|(s, _)| s))
        .ok_or("The response did not include a task plan. Retry Orchestrator to prepare one.")?;
    if body.len() > 256 * 1024 {
        return Err("Task plan is too large.".into());
    }
    let value: serde_json::Value =
        serde_json::from_str(body.trim()).map_err(|e| format!("Invalid task plan: {e}"))?;
    if let Some(tasks) = value.get("tasks").and_then(|v| v.as_array()) {
        for (index, task) in tasks.iter().enumerate() {
            if let Some(workspace) = task.get("workspace").filter(|v| !v.is_null()) {
                if !matches!(workspace.as_str(), Some("checkout" | "worktree")) {
                    return Err(format!("Assignment {} has an invalid workspace. Use exactly \"checkout\" or \"worktree\". Keep branch names and base-branch requirements in the task prompt. The proposal was not started.", index + 1));
                }
            }
        }
    }
    let plan: Plan =
        serde_json::from_value(value).map_err(|e| format!("Invalid task plan: {e}"))?;
    validate(&plan)?;
    Ok(plan)
}
pub fn validate(plan: &Plan) -> Result<(), String> {
    if plan.title.trim().is_empty()
        || plan.title.len() > 160
        || plan.summary.len() > 4_000
        || plan.tasks.is_empty()
        || plan.tasks.len() > 12
    {
        return Err("Provide a title and 1–12 assignments.".into());
    }
    let mut ids = HashSet::new();
    for task in &plan.tasks {
        if task.id.is_empty()
            || task.id.len() > 64
            || !task
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            // Branch names come from IDs, and they collide on a
            // case-insensitive file system.
            || !ids.insert(task.id.to_ascii_lowercase())
        {
            return Err(
                "Task IDs must be unique ignoring case and contain only letters, numbers, '-' and '_'.".into(),
            );
        }
        for (name, value) in [
            ("repository", &task.repository),
            ("base_ref", &task.base_ref),
        ] {
            if value.as_ref().is_some_and(|v| {
                v.trim().is_empty()
                    || v.len() > 4096
                    || v.chars().any(char::is_control)
                    || v.starts_with('-')
            }) {
                return Err(format!("Assignment {} has an invalid {name}.", task.id));
            }
        }
        if task.title.trim().is_empty()
            || task.title.len() > 160
            || task.prompt.trim().is_empty()
            || task.prompt.len() > 30_000
            || task.files.is_empty()
            || task.files.len() > 64
            || task.depends_on.len() > 12
        {
            return Err(format!(
                "Assignment {} needs a title, instructions and file scopes.",
                task.id
            ));
        }
        if task.verification.len() > 16
            || task
                .verification
                .iter()
                .any(|step| step.trim().is_empty() || step.len() > 2_000)
        {
            return Err(format!(
                "Assignment {} has an empty or oversized verification step.",
                task.id
            ));
        }
        for file in &task.files {
            if file.trim().is_empty()
                || file.len() > 512
                || file.contains('\\')
                || file.contains(':')
                || Path::new(file).is_absolute()
                || Path::new(file).components().any(|c| {
                    matches!(
                        c,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                })
            {
                return Err(format!("Invalid file scope: {file}"));
            }
        }
    }
    let mut completed = HashSet::new();
    loop {
        let before = completed.len();
        for task in &plan.tasks {
            if task
                .depends_on
                .iter()
                .all(|id| completed.contains(id.as_str()))
            {
                completed.insert(task.id.as_str());
            }
        }
        if completed.len() == plan.tasks.len() {
            return Ok(());
        }
        if completed.len() == before {
            return Err("Task dependencies contain a cycle or unknown task ID.".into());
        }
    }
}
pub fn overlaps(a: &[String], b: &[String]) -> bool {
    a.iter().any(|a| {
        b.iter().any(|b| {
            a == "." || b == "." || Path::new(a).starts_with(b) || Path::new(b).starts_with(a)
        })
    })
}
pub fn worker_prompt(task: &Task, previous: &str) -> String {
    format!("You are completing assignment {} in a MonoCode orchestration.\n{}\n\n{}\n\nWrite only within these project-relative scopes: {}. Other agents may work in this checkout; preserve their changes and do not reset the repository. If you need a change outside your scopes, report the blocker. Run focused checks and report your changes, validation and any remaining issues.\n\nDependency reports:\n{}", task.id, task.title, task.prompt, task.files.join(", "), previous)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_errors_explain_repair_without_accepting_ambiguous_branches() {
        for workspace in [
            serde_json::json!("worktree feat/welder-spacing (base pipeline)"),
            serde_json::json!({"mode":"worktree"}),
        ] {
            let value = serde_json::json!({"title":"Work","summary":"","tasks":[{"id":"a","title":"A","prompt":"p","files":["src"],"workspace":workspace}]});
            let error = parse(&format!("{OPEN}{value}{CLOSE}")).unwrap_err();
            assert!(error.contains("Assignment 1"));
            assert!(error.contains("task prompt"));
            assert!(error.contains("not started"));
        }
        for workspace in [
            serde_json::json!("worktree"),
            serde_json::json!("checkout"),
            serde_json::Value::Null,
        ] {
            let value = serde_json::json!({"title":"Work","summary":"","tasks":[{"id":"a","title":"A","prompt":"p","files":["src"],"workspace":workspace}]});
            assert!(parse(&format!("{OPEN}{value}{CLOSE}")).is_ok());
        }
    }
    #[test]
    fn progress_labels_are_defined_once_and_keep_their_text() {
        let summary = |live| Summary {
            tasks: Vec::new(),
            live,
        };
        let label = |status: &str, live, input| {
            let task = SummaryTask {
                status: status.into(),
                ..SummaryTask::default()
            };
            task.label(&summary(live), input)
        };
        assert_eq!(label("running", true, false), "Working");
        assert_eq!(label("queued", true, false), "Queued");
        assert_eq!(label("completed", true, false), "Done");
        assert_eq!(label("failed", true, false), "Failed");
        assert_eq!(label("blocked", true, false), "Needs review");
        assert_eq!(label("running", false, false), "Saved");
        assert_eq!(label("running", true, true), "Needs input");
        let live: Vec<_> = ProgressStatus::ALL
            .into_iter()
            .filter(|s| s.live())
            .collect();
        assert_eq!(live, [ProgressStatus::Queued, ProgressStatus::Working]);
    }
    fn task(id: &str, deps: &[&str], file: &str) -> Task {
        Task {
            id: id.into(),
            title: id.into(),
            prompt: "Work".into(),
            files: vec![file.into()],
            depends_on: deps.iter().map(|s| (*s).into()).collect(),
            ..Task::default()
        }
    }
    #[test]
    fn rejects_ids_that_differ_only_by_case() {
        let plan = Plan {
            title: "Work".into(),
            summary: String::new(),
            tasks: vec![task("Api", &[], "src"), task("api", &[], "tests")],
        };
        let refused = validate(&plan).unwrap_err();
        assert!(refused.contains("ignoring case"), "{refused}");
    }
    #[test]
    fn rejects_cycles_unknown_dependencies_and_scope_escapes() {
        let mut plan = Plan {
            title: "Work".into(),
            summary: String::new(),
            tasks: vec![task("a", &["b"], "src"), task("b", &["a"], "tests")],
        };
        assert!(validate(&plan).is_err());
        plan.tasks[1].depends_on.clear();
        assert!(validate(&plan).is_ok());
        plan.tasks[1].files = vec!["../elsewhere".into()];
        assert!(validate(&plan).is_err());
        assert!(overlaps(&["src".into()], &["src/main.rs".into()]));
        assert!(!overlaps(&["src".into()], &["src2/main.rs".into()]));
    }
    #[test]
    fn old_format_plan_still_parses_with_assignment_defaults() {
        let old = r#"{"title":"T","summary":"","tasks":[{"id":"a","title":"A","prompt":"p","files":["src"],"dependsOn":["b"]},{"id":"b","title":"B","prompt":"p","files":["tests"]}]}"#;
        let plan: Plan = serde_json::from_str(old).unwrap();
        let task = &plan.tasks[0];
        assert_eq!(task.depends_on, vec!["b".to_owned()]);
        assert_eq!(task.workspace, None);
        assert_eq!(task.status, crate::tasks::AssignmentState::Queued);
        assert_eq!(task.revision, 0);
        assert!(task.verification.is_empty());
        assert!(validate(&plan).is_ok());
        // Defaults stay out of the JSON, so plans written today look as before.
        let written = serde_json::to_value(&plan.tasks[1]).unwrap();
        assert_eq!(
            written,
            serde_json::json!({"id":"b","title":"B","prompt":"p","files":["tests"],"depends_on":[]})
        );
    }
    #[test]
    fn assignment_fields_round_trip_through_plan_json() {
        let task = Task {
            workspace: Some(WorkspaceMode::Worktree),
            status: crate::tasks::AssignmentState::NeedsReview,
            revision: 3,
            verification: vec!["cargo test -p monocode-core".into()],
            ..task("a", &[], "src")
        };
        let json = serde_json::to_string(&task).unwrap();
        assert!(json.contains(r#""workspace":"worktree""#));
        assert!(json.contains(r#""status":"needs_review""#));
        let back: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(back, task);
        let mut plan = Plan {
            title: "Work".into(),
            summary: String::new(),
            tasks: vec![Task {
                verification: vec![String::new()],
                ..task.clone()
            }],
        };
        assert!(validate(&plan).is_err(), "blank verification step");
        plan.tasks[0].verification = vec!["x".repeat(2_001)];
        assert!(validate(&plan).is_err(), "oversized verification step");
    }
    #[test]
    fn display_text_hides_plan_json_including_while_streaming() {
        let plan =
            "Ready.\n\n<monocode_orchestration>\n{\"title\":\"x\"}\n</monocode_orchestration>\n";
        assert_eq!(display_text(plan).as_deref(), Some("Ready."));
        let tail =
            "Ready.\n<monocode_orchestration>{\"title\":\"x\"}</monocode_orchestration>\nAfter.";
        assert_eq!(display_text(tail).as_deref(), Some("Ready.\n\nAfter."));
        assert_eq!(
            display_text("Ready.\n<monocode_orchestration>\n{\"tit").as_deref(),
            Some("Ready.")
        );
        assert_eq!(
            display_text("Ready.\n<monocode_orch").as_deref(),
            Some("Ready.")
        );
        assert_eq!(display_text("Ready.\n<").as_deref(), None);
        assert_eq!(display_text("No plan here."), None);
        let prose = "The <monocode_orchestration> tag wraps the plan.";
        assert_eq!(display_text(prose), None);
        let parsed = parse(plan.replace("{\"title\":\"x\"}", r#"{"title":"T","summary":"","tasks":[{"id":"a","title":"A","prompt":"p","files":["src"]}]}"#).as_str());
        assert_eq!(parsed.unwrap().tasks.len(), 1);
    }
}
