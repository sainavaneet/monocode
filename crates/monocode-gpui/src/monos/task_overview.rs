//! The Mono overview's summary of the tasks the Mono leads, above its
//! sessions: per task its state, stage, what Mono is doing, worker counts
//! and the badge. It reads the same persisted view as the chat's
//! monitoring card (`tasks::view`), refreshed by the task pump.
use super::*;
use crate::conversation::task_monitor::{counts_line, stage_label, verification_line};
use monocode_core::tasks::view::TaskMonitor;
use std::collections::HashMap;

/// One task's overview line: `Ship it · Active · Implementing · Mono:
/// Supervising`.
pub(super) fn overview_line(task: &TaskMonitor) -> String {
    format!(
        "{} · {} · {} · Mono: {}",
        task.control.title,
        task.control.display.label(),
        stage_label(task.stage),
        task.activity.label()
    )
}

impl Monos {
    pub(super) fn task_overview(&self, mono: &Mono) -> Option<AnyElement> {
        let tasks = self.task_monitors.get(&mono.id).filter(|t| !t.is_empty())?;
        let live = HashMap::new();
        Some(
            div()
                .id("mono-overview-tasks")
                .debug_selector(|| "mono-overview-tasks".into())
                .px(px(6.))
                .pt_2()
                .flex()
                .flex_col()
                .gap_1()
                .children(tasks.iter().map(|task| {
                    let id = task.control.task_id.clone();
                    let details = [counts_line(task, &live)]
                        .into_iter()
                        .chain(verification_line(task))
                        .chain(task.badge().map(|b| b.label()))
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(" · ");
                    div()
                        .debug_selector(move || format!("mono-overview-task-{id}"))
                        .px(px(10.))
                        .py_2()
                        .rounded_md()
                        .bg(content(0.04))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .truncate()
                                .text_size(px(13.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(content(1.))
                                .child(overview_line(task)),
                        )
                        .when(!details.is_empty(), |d| {
                            d.child(
                                div()
                                    .truncate()
                                    .text_size(px(11.))
                                    .text_color(content(0.5))
                                    .child(details),
                            )
                        })
                }))
                .into_any_element(),
        )
    }
}
