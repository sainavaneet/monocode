//! The monitoring card of the durable tasks a Mono leads, on the plan card:
//! objective and project, stage and what Mono is doing, each worker's state,
//! workspace and branch, the latest meaningful event, decisions waiting on
//! the user, verification counts, recovery and account changes (account ids
//! and redacted reasons only), lifecycle controls, and an expandable
//! timeline. It renders from persisted state (`tasks::view::monitor`, read
//! by the Mono task pump) plus live worker sessions (`LiveWorker`, pushed as
//! sessions change). Counts and stages only, never percentages.
use super::orchestration_card::OpenOrchestrationWorker;
use super::*;
use crate::right_panel::{DIFF_ADD, RED_400};
use monocode_core::tasks::TaskStage;
use monocode_core::tasks::lifecycle::Display;
use monocode_core::tasks::timeline::{TimelineEntry, TimelineKind};
use monocode_core::tasks::view::{Live, TaskMonitor, WorkerState, WorkerView};
use std::collections::{HashMap, HashSet};

const TIMELINE_MAX_HEIGHT: f32 = 220.;

/// A worker session as the app sees it now, between store reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LiveWorker {
    pub(crate) busy: bool,
    /// Waiting on a permission or an answer in its chat.
    pub(crate) needs_input: bool,
    pub(crate) updated_at: u64,
}

/// What the card shows: the persisted view and the live sessions.
#[derive(Default)]
pub(crate) struct TaskMonitorState {
    pub(crate) tasks: Vec<TaskMonitor>,
    pub(crate) live: HashMap<i64, LiveWorker>,
    /// Tasks whose timeline is expanded.
    timelines: HashSet<String>,
}

/// Opens an assignment's worktree changes (the app's Changes review).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OpenTaskChanges {
    pub(crate) cwd: String,
}
impl EventEmitter<OpenTaskChanges> for Conversation {}

impl LiveWorker {
    fn core(self) -> Live {
        Live {
            busy: self.busy,
            needs_input: self.needs_input,
        }
    }
}

/// A worker's state: the store's, refined by its live session (an approval
/// wait is Needs input; a busy session that is still starting is Working).
pub(crate) fn worker_state(worker: &WorkerView, live: &HashMap<i64, LiveWorker>) -> WorkerState {
    let live = worker.session.and_then(|s| live.get(&s)).map(|l| l.core());
    worker.state(live)
}

fn worker_color(state: WorkerState) -> gpui::Hsla {
    match state {
        WorkerState::Working => tint(FUCHSIA_300, 0.9),
        WorkerState::Accepted => tint(DIFF_ADD, 0.9),
        WorkerState::Failed | WorkerState::Cancelled => tint(RED_400, 0.9),
        WorkerState::NeedsInput | WorkerState::NeedsReview | WorkerState::Stopped => {
            tint(AMBER_400, 0.9)
        }
        WorkerState::Starting | WorkerState::Ready | WorkerState::Queued => content(0.5),
    }
}

fn display_color(display: Display) -> gpui::Hsla {
    match display {
        Display::Active => tint(FUCHSIA_300, 0.9),
        Display::Completed => tint(DIFF_ADD, 0.9),
        Display::Blocked | Display::Cancelling | Display::Cancelled => tint(RED_400, 0.9),
        Display::Waiting | Display::Pausing | Display::Paused => tint(AMBER_400, 0.9),
    }
}

/// "2 working · 1 needs review · 1 queued": workers by state, in a fixed
/// order, never a percentage.
pub(crate) fn counts_line(task: &TaskMonitor, live: &HashMap<i64, LiveWorker>) -> String {
    let states: Vec<WorkerState> = task.workers.iter().map(|w| worker_state(w, live)).collect();
    WorkerState::ALL
        .iter()
        .filter_map(|state| {
            let n = states.iter().filter(|s| *s == state).count();
            (n > 0).then(|| format!("{n} {}", state.label().to_lowercase()))
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// "Verification: 1 of 3 criteria passed, 1 failing".
pub(crate) fn verification_line(task: &TaskMonitor) -> Option<String> {
    let v = task.verification;
    if v.required == 0 {
        return None;
    }
    let failing = if v.failed > 0 {
        format!(", {} failing", v.failed)
    } else {
        String::new()
    };
    Some(format!(
        "Verification: {} of {} criteria passed{failing}",
        v.passed, v.required
    ))
}

fn ago(at: Option<u64>, now: u64) -> String {
    match at.map(|at| monocode_core::time::relative(at, now)) {
        None => "never".into(),
        Some(text) if text.is_empty() => "never".into(),
        Some(text) if text == "now" => "just now".into(),
        Some(text) if text.chars().next().is_some_and(|c| c.is_ascii_digit()) => {
            format!("{text} ago")
        }
        Some(text) => format!("on {text}"),
    }
}

/// The three times, kept apart: the workers' last activity (persisted or
/// seen live), the last status synchronization and Mono's last review.
pub(crate) fn times_line(task: &TaskMonitor, live: &HashMap<i64, LiveWorker>, now: u64) -> String {
    let seen = task
        .workers
        .iter()
        .filter_map(|w| w.session.and_then(|s| live.get(&s)))
        .map(|l| l.updated_at)
        .filter(|at| *at > 0)
        .max();
    let worker = task.last_worker_activity.max(seen);
    format!(
        "Worker activity {} · Status sync {} · Mono review {}",
        ago(worker, now),
        task.last_status_sync
            .map_or_else(|| "not synced".into(), |at| ago(Some(at), now)),
        ago(task.last_mono_review, now)
    )
}

/// Why the task waits or is blocked, as one line.
pub(crate) fn waiting_line(task: &TaskMonitor) -> Option<String> {
    let cause = match task.waiting_cause.as_deref() {
        Some("decision") => Some("Waiting on a decision"),
        Some("restart") => Some("Held after the app restarted"),
        Some("takeover") => Some("Held: another app may still run its workers"),
        _ => None,
    };
    match (cause, task.control.reason.as_deref()) {
        (Some(cause), Some(reason)) => Some(format!("{cause}: {reason}")),
        (Some(cause), None) => Some(cause.into()),
        (None, Some(reason)) => Some(reason.into()),
        (None, None) => None,
    }
}

/// A timeline entry as printed: its kind, text and age.
pub(crate) fn timeline_line(entry: &TimelineEntry, now: u64) -> String {
    format!(
        "{} · {} · {}",
        entry.kind.label(),
        entry.text,
        ago(Some(entry.at), now)
    )
}

fn kind_color(kind: TimelineKind) -> gpui::Hsla {
    match kind {
        TimelineKind::Failure => tint(RED_400, 0.85),
        TimelineKind::Verification => tint(DIFF_ADD, 0.85),
        TimelineKind::Decision => tint(AMBER_400, 0.85),
        TimelineKind::Launch | TimelineKind::Correction | TimelineKind::Integration => content(0.6),
    }
}

/// A small text line with its own debug selector.
fn line(selector: String, text: String, color: gpui::Hsla) -> gpui::Div {
    div()
        .debug_selector(move || selector.clone())
        .text_size(px(11.))
        .line_height(px(16.))
        .text_color(color)
        .child(text)
}

fn chip(selector: String, label: &'static str, color: gpui::Hsla) -> gpui::Div {
    div()
        .debug_selector(move || selector.clone())
        .flex_shrink_0()
        .px(px(6.))
        .py(px(1.))
        .rounded_md()
        .border_1()
        .border_color(color.opacity(0.35))
        .text_size(px(10.))
        .text_color(color)
        .child(label)
}

fn small_button(id: String, label: &'static str) -> gpui::Stateful<gpui::Div> {
    let selector = id.clone();
    div()
        .id(SharedString::from(id))
        .debug_selector(move || selector.clone())
        .role(Role::Button)
        .aria_label(label)
        .cursor_pointer()
        .flex_shrink_0()
        .px(px(6.))
        .py(px(1.))
        .rounded_md()
        .bg(content(0.06))
        .hover(|s| s.bg(content(0.12)))
        .text_size(px(10.))
        .child(label)
}

impl Conversation {
    /// The persisted view of the Mono's tasks; empty shows none (the
    /// coordinator is off or nothing is unfinished).
    pub(crate) fn set_task_monitor(&mut self, tasks: Vec<TaskMonitor>, cx: &mut Context<Self>) {
        if self.task_monitor.tasks == tasks {
            return;
        }
        let ids: HashSet<&str> = tasks.iter().map(|t| t.control.task_id.as_str()).collect();
        self.task_monitor
            .timelines
            .retain(|id| ids.contains(id.as_str()));
        self.task_monitor.tasks = tasks;
        cx.notify();
    }

    /// A worker session changed: shown at once if it is one of the card's.
    pub(crate) fn set_worker_live(
        &mut self,
        session: i64,
        live: LiveWorker,
        cx: &mut Context<Self>,
    ) {
        let ours = self
            .task_monitor
            .tasks
            .iter()
            .any(|t| t.workers.iter().any(|w| w.session == Some(session)));
        if !ours || self.task_monitor.live.get(&session) == Some(&live) {
            return;
        }
        self.task_monitor.live.insert(session, live);
        cx.notify();
    }

    pub(super) fn task_monitor_cards(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.task_monitor.tasks.is_empty() {
            return None;
        }
        let now = monocode_core::time::now_ms();
        Some(
            div()
                .id("task-monitor")
                .flex()
                .flex_col()
                .gap_2()
                .children(
                    self.task_monitor
                        .tasks
                        .iter()
                        .map(|task| self.task_monitor_card(task, now, cx)),
                )
                .into_any_element(),
        )
    }

    fn task_monitor_card(
        &self,
        task: &TaskMonitor,
        now: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = task.control.task_id.clone();
        let live = &self.task_monitor.live;
        let display = task.control.display.label();
        let project = std::path::Path::new(&task.project)
            .file_name()
            .map_or(task.project.clone(), |n| n.to_string_lossy().into_owned());
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(content(0.9))
                    .child(format!("{} · {project}", task.control.title)),
            )
            .child(chip(
                format!("task-monitor-{id}-state-{display}"),
                display,
                display_color(task.control.display),
            ));
        let stage = format!(
            "Stage: {} · Mono: {}",
            stage_label(task.stage),
            task.activity.label()
        );
        let counts = [counts_line(task, live)]
            .into_iter()
            .chain(verification_line(task))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        div()
            .id(SharedString::from(format!("task-monitor-{id}")))
            .debug_selector({
                let id = id.clone();
                move || format!("task-monitor-{id}")
            })
            .p_2()
            .rounded_md()
            .bg(content(0.03))
            .flex()
            .flex_col()
            .gap_1()
            .child(header)
            .child(
                line(
                    format!("task-monitor-{id}-objective"),
                    format!("Objective: {}", task.objective),
                    content(0.7),
                )
                .truncate(),
            )
            .child(line(
                format!("task-monitor-{id}-stage"),
                stage,
                content(0.6),
            ))
            .when_some(waiting_line(task), |card, text| {
                card.child(line(
                    format!("task-monitor-{id}-waiting"),
                    text,
                    tint(AMBER_400, 0.85),
                ))
            })
            .when(!counts.is_empty(), |card| {
                card.child(line(
                    format!("task-monitor-{id}-counts"),
                    counts,
                    content(0.6),
                ))
            })
            .child(line(
                format!("task-monitor-{id}-times"),
                times_line(task, live, now),
                content(0.45),
            ))
            .children(
                task.workers
                    .iter()
                    .map(|w| self.task_worker_row(&id, w, cx)),
            )
            .children(self.task_decision_lines(task))
            .children(task.recovery.iter().enumerate().map(|(ix, text)| {
                line(
                    format!("task-monitor-{id}-recovery-{ix}"),
                    format!("Recovery: {text}"),
                    tint(AMBER_400, 0.8),
                )
            }))
            .when_some(task.latest(), |card, latest| {
                card.child(line(
                    format!("task-monitor-{id}-latest"),
                    format!("Latest: {}", timeline_line(latest, now)),
                    content(0.55),
                ))
            })
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .children(self.task_control_buttons(&task.control, cx))
                    .child(div().flex_1())
                    .child(self.task_timeline_toggle(task, cx)),
            )
            .children(self.task_timeline(task, now))
            .into_any_element()
    }

    fn task_worker_row(
        &self,
        task: &str,
        worker: &WorkerView,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = worker_state(worker, &self.task_monitor.live);
        let label = state.label();
        let key = format!("task-worker-{task}-{}", worker.assignment_id);
        let place = match (&worker.workspace, &worker.branch) {
            (Some(path), Some(branch)) => format!("{path} · {branch}"),
            (Some(path), None) => path.clone(),
            (None, _) => "No workspace yet".into(),
        };
        let runner = match (&worker.provider, &worker.account_id) {
            (Some(provider), Some(account)) => format!(" · {provider}/{account}"),
            (Some(provider), None) => format!(" · {provider}"),
            _ => String::new(),
        };
        let open = worker.session.map(|session| {
            small_button(format!("{key}-open"), "Open worker")
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(OpenOrchestrationWorker(session))))
        });
        let changes = worker.workspace.clone().map(|cwd| {
            small_button(format!("{key}-changes"), "Changes").on_click(
                cx.listener(move |_, _, _, cx| cx.emit(OpenTaskChanges { cwd: cwd.clone() })),
            )
        });
        let detached = worker.detached.iter().map(|&session| {
            small_button(format!("{key}-detached-{session}"), "Detached session")
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(OpenOrchestrationWorker(session))))
        });
        div()
            .debug_selector({
                let key = key.clone();
                move || key.clone()
            })
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(content(0.85))
                            .child(worker.title.clone()),
                    )
                    .child(chip(
                        format!("{key}-state-{label}"),
                        label,
                        worker_color(state),
                    ))
                    .children(open)
                    .children(changes),
            )
            .child(line(
                format!("{key}-workspace"),
                format!("{place}{runner}"),
                content(0.4),
            ))
            .when(!worker.detached.is_empty(), |row| {
                row.child(div().flex().gap_1().children(detached))
            })
            .into_any_element()
    }

    fn task_decision_lines(&self, task: &TaskMonitor) -> Vec<gpui::Div> {
        let id = &task.control.task_id;
        let mut lines = Vec::new();
        let waiting = task.for_user();
        if waiting > 0 {
            lines.push(line(
                format!("task-monitor-{id}-needs-you"),
                format!(
                    "Needs your decision ({waiting}): answer on the decision card in this chat"
                ),
                tint(AMBER_400, 0.95),
            ));
        }
        for decision in task.decisions.iter().filter(|d| d.quiet) {
            // Passive: Mono's reminders stopped; nothing is answered for it.
            lines.push(line(
                format!("task-decision-{}-quiet", decision.id),
                format!(
                    "Decision {} is still open; Mono was reminded {} times and has not answered",
                    decision.id,
                    monocode_core::tasks::inbox::MAX_REMINDERS
                ),
                content(0.5),
            ));
        }
        lines
    }

    fn task_timeline_toggle(&self, task: &TaskMonitor, cx: &mut Context<Self>) -> impl IntoElement {
        let id = task.control.task_id.clone();
        let open = self.task_monitor.timelines.contains(&id);
        let label = format!(
            "{} timeline ({})",
            if open { "Hide" } else { "Show" },
            task.timeline.len()
        );
        let selector = format!("task-monitor-{id}-timeline-toggle");
        div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector.clone())
            .role(Role::Button)
            .aria_label(label.clone())
            .aria_expanded(open)
            .cursor_pointer()
            .px(px(6.))
            .py(px(2.))
            .rounded_md()
            .hover(|s| s.bg(content(0.1)))
            .text_size(px(11.))
            .text_color(content(0.55))
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.task_monitor.timelines.remove(&id) {
                    this.task_monitor.timelines.insert(id.clone());
                }
                cx.notify();
            }))
    }

    fn task_timeline(&self, task: &TaskMonitor, now: u64) -> Option<AnyElement> {
        let id = &task.control.task_id;
        if !self.task_monitor.timelines.contains(id) {
            return None;
        }
        Some(
            div()
                .id(SharedString::from(format!("task-monitor-{id}-timeline")))
                .debug_selector({
                    let id = id.clone();
                    move || format!("task-monitor-{id}-timeline")
                })
                .max_h(px(TIMELINE_MAX_HEIGHT))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_px()
                .when(task.timeline.is_empty(), |list| {
                    list.child(line(
                        format!("task-timeline-{id}-empty"),
                        "Nothing has happened yet".into(),
                        content(0.45),
                    ))
                })
                .children(task.timeline.iter().map(|entry| {
                    line(
                        format!("task-timeline-{id}-{}", entry.seq),
                        timeline_line(entry, now),
                        kind_color(entry.kind),
                    )
                }))
                .into_any_element(),
        )
    }
}

/// A stage as the card prints it: `Implementing`.
pub(crate) fn stage_label(stage: TaskStage) -> String {
    let text = stage.as_str();
    let mut chars = text.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}
