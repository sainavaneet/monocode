//! Live task progress in the Mono's Sessions panel.
use super::*;
use crate::conversation::task_monitor::{
    counts_line, elapsed, stage_label, verification_line, worker_detail, worker_state,
};
use monocode_core::tasks::view::{TaskMonitor, WorkerState};

pub(super) fn overview_line(task: &TaskMonitor) -> String {
    format!(
        "{} · {} · {} · Mono: {}",
        task.control.title,
        task.control.display.label(),
        stage_label(task.stage),
        task.activity.label()
    )
}

pub(super) fn is_live_overview_task(task: &TaskMonitor) -> bool {
    task.control.display != monocode_core::tasks::lifecycle::Display::Completed
}

fn state_color(state: WorkerState) -> gpui::Hsla {
    match state {
        WorkerState::Working | WorkerState::Starting => crate::theme::accent_color(),
        WorkerState::Accepted => rgb(0x6fcca5).into(),
        WorkerState::Failed | WorkerState::Cancelled => rgb(0xf18b91).into(),
        WorkerState::NeedsInput | WorkerState::NeedsReview | WorkerState::Stopped => {
            rgb(0xe5b96b).into()
        }
        _ => content(0.4),
    }
}

fn pill(label: impl Into<SharedString>, color: gpui::Hsla) -> gpui::Div {
    div()
        .flex_shrink_0()
        .px_2()
        .py(px(3.))
        .rounded_md()
        .bg(gpui::Hsla { a: 0.1, ..color })
        .text_color(color)
        .text_size(px(10.))
        .font_weight(gpui::FontWeight::MEDIUM)
        .child(label.into())
}

impl Monos {
    pub(super) fn task_overview(&self, mono: &Mono, cx: &mut Context<Self>) -> Option<AnyElement> {
        // This panel is a live-session overview. Completed tasks keep their
        // final report in the conversation card, but no longer masquerade as
        // current session activity here.
        let tasks: Vec<_> = self
            .task_monitors
            .get(&mono.id)?
            .iter()
            .filter(|task| is_live_overview_task(task))
            .collect();
        if tasks.is_empty() {
            return None;
        }
        let live = self
            .chats
            .get(&mono.id)
            .map(|chat| chat.read(cx).task_monitor.live.clone())
            .unwrap_or_default();
        let now = monocode_core::time::now_ms();
        let animate = !cx.reduce_motion();
        Some(
            div()
                .id("mono-overview-tasks")
                .debug_selector(|| "mono-overview-tasks".into())
                .w_full()
                .min_w_0()
                .pt_2()
                .flex()
                .flex_col()
                .gap_3()
                .children(tasks.into_iter().map(|task| {
                    let id = task.control.task_id.clone();
                    let accepted = task
                        .workers
                        .iter()
                        .filter(|w| worker_state(w, &live) == WorkerState::Accepted)
                        .count();
                    div()
                        .id(SharedString::from(format!("overview-task-{id}")))
                        .debug_selector(move || format!("mono-overview-task-{id}"))
                        .role(Role::Group)
                        .aria_label(overview_line(task))
                        .w_full()
                        .min_w_0()
                        .px_3()
                        .py_2()
                        .rounded_xl()
                        .border_1()
                        .border_color(content(0.08))
                        .bg(content(0.025))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(content(0.45))
                                        .child("TASK PROGRESS"),
                                )
                                .child(pill(
                                    task.control.display.label(),
                                    match task.control.display {
                                        monocode_core::tasks::lifecycle::Display::Completed => {
                                            rgb(0x6fcca5).into()
                                        }
                                        monocode_core::tasks::lifecycle::Display::Active => {
                                            crate::theme::accent_color()
                                        }
                                        _ => rgb(0xe5b96b).into(),
                                    },
                                )),
                        )
                        .child(
                            div()
                                .text_size(px(14.))
                                .line_height(px(20.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(content(0.95))
                                .child(task.control.title.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .text_size(px(11.))
                                .child(
                                    div()
                                        .text_color(content(0.7))
                                        .child(stage_label(task.stage)),
                                )
                                .child(div().text_color(content(0.45)).child(format!(
                                    "{accepted} of {} accepted",
                                    task.workers.len()
                                ))),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!(
                                    "task-progress-{id}",
                                    id = task.control.task_id
                                )))
                                .debug_selector(|| "task-progress-segments".into())
                                .flex()
                                .gap_1()
                                .w_full()
                                .children(task.workers.iter().map(|worker| {
                                    let state = worker_state(worker, &live);
                                    div().flex_1().h(px(4.)).rounded_full().bg(
                                        if matches!(state, WorkerState::Queued | WorkerState::Ready)
                                        {
                                            content(0.09)
                                        } else {
                                            state_color(state)
                                        },
                                    )
                                })),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .text_size(px(11.))
                                .text_color(content(0.5))
                                .child(counts_line(task, &live))
                                .child("·")
                                .child(icon("response-activity", 12.))
                                .child(task.activity.label()),
                        )
                        .children(verification_line(task).map(|line| {
                            div()
                                .text_size(px(11.))
                                .text_color(content(0.5))
                                .child(line)
                        }))
                        .children(task.workers.iter().enumerate().map(|(index, worker)| {
                            let session = worker.session;
                            let state = worker_state(worker, &live);
                            let color = state_color(state);
                            let working =
                                matches!(state, WorkerState::Working | WorkerState::Starting);
                            let selector = format!(
                                "overview-worker-{}-{}",
                                task.control.task_id, worker.assignment_id
                            );
                            let detail = worker_detail(worker, state, &live, now);
                            let marker = if working && animate {
                                crate::session_mark::spinner(
                                    SharedString::from(format!(
                                        "overview-running-{}-{}",
                                        task.control.task_id, worker.assignment_id
                                    )),
                                    crate::pulse::Rate::Half,
                                )
                                .into_any_element()
                            } else {
                                div()
                                    .text_size(px(11.))
                                    .text_color(color)
                                    .child(if state == WorkerState::Accepted {
                                        "✓".into()
                                    } else {
                                        format!("{:02}", index + 1)
                                    })
                                    .into_any_element()
                            };
                            div()
                                .id(SharedString::from(selector.clone()))
                                .debug_selector(move || selector.clone())
                                .w_full()
                                .min_w_0()
                                .px_2()
                                .py(px(5.))
                                .rounded_lg()
                                .border_1()
                                .border_color(if working {
                                    gpui::Hsla { a: 0.25, ..color }
                                } else {
                                    content(0.07)
                                })
                                .bg(if working {
                                    gpui::Hsla { a: 0.045, ..color }
                                } else {
                                    content(0.02)
                                })
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .child(
                                            div()
                                                .size(px(18.))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .child(marker),
                                        )
                                        .child(provider_icon(
                                            worker.provider.as_deref().unwrap_or(""),
                                            12.,
                                        ))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .text_size(px(12.))
                                                .font_weight(gpui::FontWeight::MEDIUM)
                                                .text_color(content(
                                                    if state == WorkerState::Queued {
                                                        0.55
                                                    } else {
                                                        0.9
                                                    },
                                                ))
                                                .child(worker.title.clone()),
                                        )
                                        .children(detail.map(|detail| {
                                            div()
                                                .id(SharedString::from(format!(
                                                    "overview-detail-{}-{}",
                                                    task.control.task_id, worker.assignment_id
                                                )))
                                                .debug_selector(|| {
                                                    "overview-worker-activity".into()
                                                })
                                                .max_w(px(104.))
                                                .truncate()
                                                .text_size(px(10.))
                                                .text_color(color)
                                                .child(detail)
                                        }))
                                        .child(pill(state.label(), color)),
                                )
                                .when_some(session, |d, session| {
                                    d.cursor_pointer()
                                        .role(Role::Button)
                                        .aria_label(format!("Open worker: {}", worker.title))
                                        .hover(|s| s.bg(content(0.06)))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.open_task_session(session, cx)
                                        }))
                                })
                                .children(
                                    worker
                                        .detached
                                        .iter()
                                        .filter(|id| Some(**id) != session)
                                        .enumerate()
                                        .map(|(index, &previous)| {
                                            div()
                                                .id(("previous-worker-session", previous as u64))
                                                .debug_selector(move || {
                                                    format!("previous-worker-session-{previous}")
                                                })
                                                .role(Role::Button)
                                                .aria_label(format!(
                                                    "Open previous attempt {}: {}",
                                                    index + 1,
                                                    worker.title
                                                ))
                                                .cursor_pointer()
                                                .ml(px(22.))
                                                .mt_1()
                                                .px_2()
                                                .py_1()
                                                .rounded_md()
                                                .text_size(px(11.))
                                                .text_color(content(0.55))
                                                .hover(|s| s.bg(content(0.08)))
                                                .child(format!(
                                                    "Previous attempt {} · View history",
                                                    index + 1
                                                ))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    cx.stop_propagation();
                                                    this.open_task_session(previous, cx);
                                                }))
                                        }),
                                )
                        }))
                        .when(!task.timeline.is_empty(), |d| {
                            d.child(
                                div()
                                    .pt_2()
                                    .border_t_1()
                                    .border_color(content(0.08))
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(content(0.45))
                                            .child("RECENT UPDATES"),
                                    )
                                    .children(task.timeline.iter().take(2).map(|entry| {
                                        div()
                                            .flex()
                                            .gap_1()
                                            .items_center()
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_size(px(11.))
                                                    .text_color(content(0.65))
                                                    .child(entry.text.clone()),
                                            )
                                            .child(
                                                div()
                                                    .flex_shrink_0()
                                                    .text_size(px(10.))
                                                    .text_color(content(0.35))
                                                    .child(format!(
                                                        "{} ago",
                                                        elapsed(entry.at, now)
                                                    )),
                                            )
                                    })),
                            )
                        })
                }))
                .into_any_element(),
        )
    }
}
