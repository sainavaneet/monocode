//! A single presentation of the lead's activity, including delegated sessions.
use super::*;
use monocode_core::tasks::lifecycle::Display;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DelegatedWorker {
    pub session: i64,
    pub title: String,
    pub model: String,
    pub state: &'static str,
}

pub(crate) struct OpenMonoActivity;
impl EventEmitter<OpenMonoActivity> for Conversation {}

pub(crate) struct Activity {
    pub state: &'static str,
    pub title: String,
    pub detail: String,
    pub session: Option<i64>,
}

impl Activity {
    fn new(state: &'static str, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            state,
            title: title.into(),
            detail: detail.into(),
            session: None,
        }
    }
}

fn delegated_activity(workers: &[DelegatedWorker]) -> Option<Activity> {
    // Attention takes precedence over another worker still running.
    let worker = workers
        .iter()
        .find(|w| matches!(w.state, "input" | "failed" | "stopped" | "unavailable"))
        .or_else(|| workers.iter().find(|w| w.state == "working"))
        .or_else(|| workers.iter().find(|w| w.state == "draft"))
        .or_else(|| workers.first())?;
    let (state, label, next) = match worker.state {
        "input" => (
            "needs-you",
            "Worker needs input",
            "Open the worker to answer or approve.",
        ),
        "failed" => (
            "needs-you",
            "Worker failed",
            "Mono is awaiting the completion report.",
        ),
        "stopped" => (
            "needs-you",
            "Worker stopped",
            "Mono is awaiting the completion report.",
        ),
        "unavailable" => (
            "needs-you",
            "Worker unavailable",
            "Open the worker to check its status.",
        ),
        "draft" => (
            "paused",
            "Worker has not started",
            "Open the worker to review its draft.",
        ),
        "done" if workers.iter().all(|w| w.state == "done") => (
            "waiting",
            "Receiving results",
            "Waiting for the completion update to reach Mono.",
        ),
        _ => (
            "waiting",
            "Waiting for workers",
            "Mono resumes when the completion report arrives.",
        ),
    };
    let more = if workers.len() > 1 {
        format!(" · {} workers", workers.len())
    } else {
        String::new()
    };
    Some(Activity {
        state,
        title: format!("{label} · {}{more}", worker.title),
        detail: format!("{} · {next}", worker.model),
        session: Some(worker.session),
    })
}

impl Conversation {
    pub(crate) fn mcp_footer_project(&self) -> Result<std::path::PathBuf, String> {
        let cwd = std::path::Path::new(&self.summary.cwd);
        match &self.settings.mono {
            Some(mono) => monocode_core::mono::project::directory(&mono.mono, cwd),
            None => Ok(cwd.to_path_buf()),
        }
    }

    pub(crate) fn set_mono_workers(
        &mut self,
        workers: Vec<DelegatedWorker>,
        cx: &mut Context<Self>,
    ) {
        if self.mono_ui.workers != workers {
            self.mono_ui.workers = workers;
            cx.notify();
        }
    }

    pub(crate) fn mono_activity(&self) -> Activity {
        if self.mono_ui.project_restart_required && self.busy {
            return Activity::new(
                "needs-you",
                "Project changed · restart required",
                "This turn still uses the previous project. Stop it or let it finish; queued messages start with the new project configuration.",
            );
        }

        if self.needs_approval() || !self.questions.is_empty() {
            return Activity::new(
                "needs-you",
                "Needs your input",
                "Answer the question or review the permission request below.",
            );
        }
        if self.settings.usage_limit.is_some() {
            return Activity::new(
                "needs-you",
                "Account limit reached",
                "Review the account notice below to resume.",
            );
        }
        if self.responding() {
            return Activity::new(
                "working",
                "Mono is working",
                "Planning, using tools, or reviewing results.",
            );
        }
        if monocode_core::usage::Failure::classify(&self.status)
            == monocode_core::usage::Failure::Auth
            || self.status.starts_with("No eligible account")
        {
            return Activity::new(
                "needs-you",
                "Account needs attention",
                "The provider could not authenticate. Check the selected account before retrying.",
            );
        }
        if let Some(task) = self
            .task_monitor
            .tasks
            .iter()
            .find(|t| matches!(t.control.display, Display::Blocked | Display::Waiting))
        {
            return Activity::new(
                "needs-you",
                format!("{} · {}", task.control.display.label(), task.control.title),
                "Review the task's status and available actions below.",
            );
        }
        if let Some(worker) = self
            .task_monitor
            .tasks
            .iter()
            .filter(|t| t.control.display == Display::Active)
            .flat_map(|t| &t.workers)
            .find(|w| {
                super::task_monitor::worker_state(w, &self.task_monitor.live)
                    == monocode_core::tasks::view::WorkerState::NeedsInput
            })
        {
            let mut activity = Activity::new(
                "needs-you",
                "Task worker needs input",
                "Open the worker to answer or approve its request.",
            );
            activity.session = worker.session;
            return activity;
        }
        if let Some(activity) = delegated_activity(&self.mono_ui.workers) {
            return activity;
        }
        if let Some(task) = self
            .task_monitor
            .tasks
            .iter()
            .find(|t| t.control.display == Display::Active)
        {
            return Activity::new(
                "waiting",
                format!("{} · {}", task.activity.label(), task.control.title),
                super::task_monitor::counts_line(task, &self.task_monitor.live),
            );
        }
        if let Some(task) = self.task_monitor.tasks.iter().find(|t| {
            matches!(
                t.control.display,
                Display::Paused | Display::Pausing | Display::Cancelling
            )
        }) {
            return Activity::new(
                "paused",
                format!("{} · {}", task.control.display.label(), task.control.title),
                "See the task controls below.",
            );
        }
        Activity::new("idle", "Ready", "Send a message to start or continue work.")
    }

    pub(super) fn mono_activity_strip(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut activity = self.mono_activity();
        if activity.state == "working" {
            activity.title =
                crate::transcript::mono_work_label(self.transcript.read(cx).source_rows(), 0);
        }
        let mono = &self.settings.mono.as_ref().unwrap().mono;
        let color = if activity.state == "needs-you" {
            tint(AMBER_400, 0.9)
        } else {
            tint(mono.color, 0.95)
        };
        let active = matches!(activity.state, "working" | "waiting");
        if activity.state == "idle"
            || (activity.state != "needs-you"
                && !self.responding()
                && !self.task_monitor.tasks.is_empty()
                && self.mono_ui.workers.is_empty())
        {
            return div().into_any_element();
        }
        if activity.state != "needs-you" {
            return div()
                .id("mono-activity-strip")
                .debug_selector(|| "mono-activity-strip".into())
                .role(Role::Button)
                .aria_label(format!("{}. Show activity", activity.title))
                .cursor_pointer()
                .h(px(28.))
                .mb_1()
                .px_3()
                .flex()
                .items_center()
                .gap_2()
                .min_w_0()
                .text_size(px(11.))
                .text_color(content(0.55))
                .hover(|s| s.text_color(content(0.85)))
                .child(crate::monos::avatar::avatar(
                    &mono.mascot,
                    mono.color,
                    activity.state,
                    18.,
                    true,
                ))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .child(if active && !cx.reduce_motion() {
                            crate::activity::shimmer(
                                activity.title,
                                (monocode_core::time::now_ms() % 1600) as f32 / 1000.,
                            )
                        } else {
                            div().child(activity.title).into_any_element()
                        }),
                )
                .on_click(cx.listener(|_, _, _, cx| cx.emit(OpenMonoActivity)))
                .into_any_element();
        }

        div()
            .id("mono-activity-strip")
            .role(Role::Status)
            .aria_label(format!(
                "Mono status: {}. {}",
                activity.title, activity.detail
            ))
            .debug_selector(|| "mono-activity-strip".into())
            .w_full()
            .min_w_0()
            .mb_2()
            .px_3()
            .py_2()
            .rounded_lg()
            .border_1()
            .border_color(if active {
                tint(mono.color, 0.3)
            } else {
                content(0.09)
            })
            .bg(if active {
                tint(mono.color, 0.065)
            } else {
                content(0.035)
            })
            .flex()
            .items_center()
            .gap_2()
            .child(crate::monos::avatar::avatar(
                &mono.mascot,
                mono.color,
                activity.state,
                36.,
                false,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(color)
                            .child(activity.title),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(content(0.5))
                            .child(activity.detail),
                    )
                    .children(self.mono_ui.workers.iter().take(4).map(|worker| {
                        let session = worker.session;
                        div()
                            .id(("mono-live-worker", session as u64))
                            .role(Role::Button)
                            .aria_label(format!(
                                "Open worker: {} · {} · {}",
                                worker.title, worker.model, worker.state
                            ))
                            .mt_1()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(content(0.04))
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(11.))
                                    .truncate()
                                    .child(format!("{} · {}", worker.title, worker.model)),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(content(0.6))
                                    .child(worker.state),
                            )
                            .hover(|s| s.bg(content(0.09)))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(super::orchestration_card::OpenOrchestrationWorker(session))
                            }))
                    })),
            )
            .when_some(activity.session, |d, session| {
                d.child(
                    icon_button("mono-open-active-worker", "Open worker", "arrow-up-right")
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(super::orchestration_card::OpenOrchestrationWorker(session))
                        })),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn worker(state: &'static str) -> DelegatedWorker {
        DelegatedWorker {
            session: 7,
            title: "Neon Pop review".into(),
            model: "Claude Sonnet".into(),
            state,
        }
    }
    #[test]
    fn delegated_activity_keeps_waiting_until_completion_is_delivered() {
        let a = delegated_activity(&[worker("working")]).unwrap();
        assert_eq!(a.state, "waiting");
        assert!(a.title.contains("Neon Pop review"));
        assert!(a.detail.contains("Claude Sonnet"));
        let a = delegated_activity(&[worker("done")]).unwrap();
        assert_eq!(a.state, "waiting");
        assert!(a.title.starts_with("Receiving results"));
        assert!(delegated_activity(&[]).is_none());
    }
    #[test]
    fn worker_attention_takes_priority_over_parallel_work() {
        for state in ["input", "failed", "stopped", "unavailable"] {
            let a = delegated_activity(&[worker("working"), worker(state)]).unwrap();
            assert_eq!(a.state, "needs-you");
            assert_eq!(a.session, Some(7));
        }
        assert_eq!(
            delegated_activity(&[worker("draft")]).unwrap().state,
            "paused"
        );
    }
}
