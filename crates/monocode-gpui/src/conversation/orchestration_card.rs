//! The Orchestrator's plan card above the composer: the reviewed plan with
//! each assignment's scope, dependencies and instructions, then live status
//! per worker while the run dispatches.
use super::composer_features::{StartOrchestration, StopOrchestration};
use super::*;
use crate::right_panel::{DIFF_ADD, RED_400};
use monocode_core::orchestration::{Plan, Task};

const SUMMARY_LIMIT: usize = 1_200;
const TASKS_MAX_HEIGHT: f32 = 340.;
const PROMPT_MAX_HEIGHT: f32 = 220.;

/// One assignment's state, in plan order, as the shell's run reports it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TaskProgress {
    pub(crate) label: &'static str,
    pub(crate) session: Option<i64>,
}
/// Opens a worker's session from the plan card.
pub(crate) struct OpenOrchestrationWorker(pub i64);
impl EventEmitter<OpenOrchestrationWorker> for Conversation {}

fn status_color(label: &str) -> gpui::Hsla {
    match label {
        "Working" => tint(FUCHSIA_300, 0.9),
        "Done" => tint(DIFF_ADD, 0.9),
        "Failed" => tint(RED_400, 0.9),
        "Needs input" | "Needs review" => tint(AMBER_400, 0.9),
        _ => content(0.5),
    }
}

fn status_chip(label: &'static str) -> impl IntoElement {
    let color = status_color(label);
    div()
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

fn scope_line(task: &Task) -> String {
    let scopes = task.files.join(", ");
    if task.depends_on.is_empty() {
        scopes
    } else {
        format!("{scopes} · after {}", task.depends_on.join(", "))
    }
}

/// "2 working · 1 done", or the assignment count before the run starts.
fn headline(plan: Option<&Plan>, progress: Option<&[TaskProgress]>) -> String {
    let Some(progress) = progress else {
        return plan.map_or(String::new(), |plan| match plan.tasks.len() {
            1 => "1 agent".into(),
            n => format!("{n} agents"),
        });
    };
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for task in progress {
        match counts.iter_mut().find(|(label, _)| *label == task.label) {
            Some((_, n)) => *n += 1,
            None => counts.push((task.label, 1)),
        }
    }
    counts
        .iter()
        .map(|(label, n)| format!("{n} {}", label.to_lowercase()))
        .collect::<Vec<_>>()
        .join(" · ")
}

impl Conversation {
    pub(super) fn finish_orchestration_plan(&mut self, cx: &mut Context<Self>) {
        if !self.planning_orchestration {
            return;
        }
        self.planning_orchestration = false;
        let rows = self.transcript.read(cx).source_rows();
        let start = rows.iter().rposition(|row| row.role == "user").unwrap_or(0);
        let text: String = rows[start..]
            .iter()
            .filter(|row| row.role == "assistant")
            .map(|row| row.text.as_str())
            .collect();
        self.orchestration_open.clear();
        match monocode_core::orchestration::parse(&text) {
            Ok(plan) => self.orchestration_plan = Some(plan),
            Err(error) => self.orchestration_error = Some(error),
        }
    }
    pub(crate) fn set_orchestration_progress(
        &mut self,
        progress: Option<Vec<TaskProgress>>,
        cx: &mut Context<Self>,
    ) {
        if self.orchestration_progress == progress {
            return;
        }
        self.orchestration_progress = progress;
        if self.needs_history_restore() {
            self.load(cx);
        }
        cx.notify();
    }
    pub(super) fn orchestration_card(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.orchestration_plan.is_none()
            && self.orchestration_error.is_none()
            && self.orchestration_progress.is_none()
        {
            return None;
        }
        let plan = self.orchestration_plan.as_ref();
        let progress = self.orchestration_progress.as_deref();
        let card = div()
            .id("orchestration-card")
            .mb_2()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(tint(FUCHSIA_300, 0.2))
            .bg(tint(FUCHSIA_300, 0.04))
            .flex()
            .flex_col()
            .gap_2()
            .text_size(px(12.))
            .child(self.orchestration_header(plan, progress))
            .when_some(plan, |card, plan| {
                card.when(!plan.summary.trim().is_empty(), |card| {
                    card.child(
                        div()
                            .line_height(px(18.))
                            .text_color(content(0.6))
                            .child(monocode_core::response::limit(&plan.summary, SUMMARY_LIMIT)),
                    )
                })
                .child(self.orchestration_tasks(plan, progress, cx))
            })
            .when_some(self.orchestration_error.clone(), |card, error| {
                card.child(self.orchestration_error_row(error, cx))
            })
            .child(self.orchestration_actions(cx));
        Some(card.into_any_element())
    }
    fn orchestration_header(
        &self,
        plan: Option<&Plan>,
        progress: Option<&[TaskProgress]>,
    ) -> impl IntoElement {
        let live = progress.is_some_and(|tasks| {
            tasks
                .iter()
                .any(|t| matches!(t.label, "Working" | "Queued"))
        });
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                icon(if live { "loader" } else { "share" }, 16.).text_color(tint(FUCHSIA_300, 0.8)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(content(0.9))
                    .child(plan.map_or_else(|| "Orchestrator".into(), |p| p.title.clone())),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(content(0.45))
                    .child(headline(plan, progress)),
            )
    }
    fn orchestration_tasks(
        &self,
        plan: &Plan,
        progress: Option<&[TaskProgress]>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id("orchestration-tasks")
            .max_h(px(TASKS_MAX_HEIGHT))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1()
            .children(plan.tasks.iter().enumerate().map(|(ix, task)| {
                let state = progress.and_then(|tasks| tasks.get(ix)).cloned();
                self.orchestration_task(task, state, cx)
            }))
    }
    fn orchestration_task(
        &self,
        task: &Task,
        state: Option<TaskProgress>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.orchestration_open.contains(&task.id);
        let id = task.id.clone();
        let session = state.as_ref().and_then(|state| state.session);
        let toggle = div()
            .id(SharedString::from(format!(
                "orchestration-task-{}",
                task.id
            )))
            .role(Role::Button)
            .aria_label(SharedString::from(format!(
                "Show instructions for {}",
                task.title
            )))
            .cursor_pointer()
            .flex()
            .items_center()
            .gap_2()
            .child(
                icon(
                    if open {
                        "chevron-down"
                    } else {
                        "chevron-right"
                    },
                    12.,
                )
                .text_color(content(0.4)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .truncate()
                            .text_color(content(0.85))
                            .child(task.title.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(10.))
                            .text_color(content(0.4))
                            .child(format!("{} · {}", task.id, scope_line(task))),
                    ),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.orchestration_open.remove(&id) {
                    this.orchestration_open.insert(id.clone());
                }
                cx.notify();
            }));
        div()
            .p_2()
            .rounded_md()
            .bg(content(0.03))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(toggle))
                    .when_some(state.map(|state| state.label), |row, label| {
                        row.child(status_chip(label))
                    })
                    .when_some(session, |row, session| {
                        row.child(Self::open_worker_button(&task.id, session, cx))
                    }),
            )
            .when(open, |row| {
                row.child(
                    div()
                        .id(SharedString::from(format!(
                            "orchestration-prompt-{}",
                            task.id
                        )))
                        .max_h(px(PROMPT_MAX_HEIGHT))
                        .overflow_y_scroll()
                        .p_2()
                        .rounded_md()
                        .bg(content(0.04))
                        .text_size(px(11.))
                        .line_height(px(17.))
                        .text_color(content(0.7))
                        .child(task.prompt.clone()),
                )
            })
            .into_any_element()
    }
    fn open_worker_button(task: &str, session: i64, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(SharedString::from(format!("orchestration-open-{task}")))
            .role(Role::Button)
            .aria_label("Open agent session")
            .cursor_pointer()
            .flex_shrink_0()
            .p(px(3.))
            .rounded_md()
            .hover(|s| s.bg(content(0.1)))
            .child(icon("external-link", 12.).text_color(content(0.55)))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(OpenOrchestrationWorker(session))))
    }
    fn orchestration_error_row(&self, error: String, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().flex_1().text_color(tint(AMBER_400, 0.9)).child(error))
            .child(
                tool_pill("dismiss-plan-error", "Dismiss")
                    .child("Dismiss")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.orchestration_error = None;
                        cx.notify();
                    })),
            )
    }
    fn orchestration_actions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.orchestration_progress.is_some();
        let reviewable = self.orchestration_plan.is_some() && !running;
        div()
            .flex()
            .justify_end()
            .gap_2()
            .when(reviewable, |row| {
                row.child(
                    tool_pill("dismiss-orchestration", "Dismiss plan")
                        .child("Dismiss")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.orchestration_plan = None;
                            this.orchestration_open.clear();
                            cx.notify();
                        })),
                )
                .child(
                    tool_pill("start-orchestration", "Start agents")
                        .bg(tint(FUCHSIA_300, 0.18))
                        .child("Start agents")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(plan) = this.orchestration_plan.clone() {
                                cx.emit(StartOrchestration(plan));
                            }
                        })),
                )
            })
            .when(running, |row| {
                row.child(
                    tool_pill("stop-orchestration", "Stop agents")
                        .child("Stop agents")
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(StopOrchestration))),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(labels: &[&'static str]) -> Vec<TaskProgress> {
        labels
            .iter()
            .map(|label| TaskProgress {
                label,
                session: None,
            })
            .collect()
    }

    #[test]
    fn headline_counts_agents_before_start_and_states_while_running() {
        let task = Task {
            id: "a".into(),
            title: "A".into(),
            prompt: "p".into(),
            files: vec!["src".into()],
            depends_on: vec![],
        };
        let plan = Plan {
            title: "T".into(),
            summary: String::new(),
            tasks: vec![task.clone(), task],
        };
        assert_eq!(headline(Some(&plan), None), "2 agents");
        let running = progress(&["Working", "Done", "Working"]);
        assert_eq!(headline(Some(&plan), Some(&running)), "2 working · 1 done");
    }

    #[test]
    fn scope_line_lists_dependencies_after_scopes() {
        let task = Task {
            id: "b".into(),
            title: "B".into(),
            prompt: "p".into(),
            files: vec!["src".into(), "tests".into()],
            depends_on: vec!["a".into()],
        };
        assert_eq!(scope_line(&task), "src, tests · after a");
    }
}
