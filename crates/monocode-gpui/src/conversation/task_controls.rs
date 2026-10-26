//! Pause, resume, cancel and steer for the durable tasks a Mono leads, on the
//! plan card (`orchestration_card`), next to its Start and Stop. The rows
//! come from `monocode_core::tasks::lifecycle::controls`; a click is carried
//! out by `shell::orchestration::apply_task_control` as the user. Steer takes
//! the new requirements from the composer. The monitoring card (task-10)
//! replaces this minimal row.
use super::*;
use monocode_core::tasks::lifecycle::{ControlRow, Display};

/// The user chose a control on a task row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TaskControl {
    pub(crate) task_id: String,
    pub(crate) revision: u64,
    /// The Mono command it matches: `tasks.pause`, `tasks.resume`,
    /// `tasks.cancel` or `tasks.steer`.
    pub(crate) action: &'static str,
    /// The new requirements, for a steer.
    pub(crate) requirements: Option<String>,
    /// The user confirmed that another app's workers of the task stopped.
    pub(crate) confirmed_stopped: bool,
}
impl EventEmitter<TaskControl> for Conversation {}

/// The controls a task row offers, as (action, label): only actions the
/// transition table and `tasks.resume` accept in its state. Resume appears
/// when the task is paused or held after a restart or takeover (`resumable`),
/// never for a wait on a question or a block.
pub(crate) fn offered(row: &ControlRow) -> Vec<(&'static str, &'static str)> {
    let mut actions = Vec::new();
    match row.display {
        Display::Active | Display::Waiting | Display::Blocked => {
            actions.push(("tasks.pause", "Pause"))
        }
        Display::Pausing | Display::Paused => {}
        Display::Cancelling | Display::Cancelled | Display::Completed => return actions,
    }
    if row.resumable && row.confirm_stopped {
        // Workers held from another app this one cannot check: resuming is
        // the user's explicit confirmation that they stopped.
        actions.push(("tasks.resume", "Other app stopped: Resume"));
    } else if row.resumable {
        actions.push(("tasks.resume", "Resume"));
    }
    actions.push(("tasks.steer", "Steer"));
    actions.push(("tasks.cancel", "Cancel"));
    actions
}

impl Conversation {
    /// The rows of the Mono's unfinished tasks; empty hides them.
    pub(crate) fn set_task_controls(&mut self, rows: Vec<ControlRow>, cx: &mut Context<Self>) {
        if self.task_controls == rows {
            return;
        }
        self.task_controls = rows;
        cx.notify();
    }

    /// A control was carried out: show what it says, or its error.
    pub(crate) fn task_control_done(
        &mut self,
        control: &TaskControl,
        result: Result<serde_json::Value, String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(value) => {
                if let Some(row) = self
                    .task_controls
                    .iter_mut()
                    .find(|row| row.task_id == control.task_id)
                {
                    if let Some(revision) = value["revision"].as_u64() {
                        row.revision = revision;
                    }
                    if let Some(display) = value["display"]
                        .as_str()
                        .and_then(|d| serde_json::from_value(serde_json::json!(d)).ok())
                    {
                        row.display = display;
                    }
                }
                if control.action == "tasks.steer" {
                    self.input
                        .update(cx, |input, cx| input.set_text(String::new(), cx));
                }
            }
            Err(error) => self.orchestration_error = Some(error),
        }
        cx.notify();
    }

    pub(crate) fn request_task_control(&mut self, control: TaskControl, cx: &mut Context<Self>) {
        let control = if control.action == "tasks.steer" {
            let text = self.input.read(cx).content.trim().to_owned();
            if text.is_empty() {
                self.orchestration_error =
                    Some("Type the new requirements in the composer, then choose Steer.".into());
                cx.notify();
                return;
            }
            TaskControl {
                requirements: Some(text),
                ..control
            }
        } else {
            control
        };
        self.orchestration_error = None;
        cx.emit(control);
    }

    pub(super) fn task_control_rows(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.task_controls.is_empty() {
            return None;
        }
        let rows = self.task_controls.iter().map(|row| {
            let buttons = offered(row).into_iter().map(|(action, label)| {
                let control = TaskControl {
                    task_id: row.task_id.clone(),
                    revision: row.revision,
                    action,
                    requirements: None,
                    confirmed_stopped: action == "tasks.resume" && row.confirm_stopped,
                };
                div()
                    .id(SharedString::from(format!("task-{}-{label}", row.task_id)))
                    .role(Role::Button)
                    .aria_label(label)
                    .cursor_pointer()
                    .flex_shrink_0()
                    .px(px(6.))
                    .py(px(2.))
                    .rounded_md()
                    .bg(content(0.08))
                    .hover(|s| s.bg(content(0.14)))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.request_task_control(control.clone(), cx)
                    }))
            });
            let line = div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().flex_1().min_w_0().truncate().child(row.title.clone()))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(10.))
                        .text_color(content(0.6))
                        .child(row.display.label()),
                )
                .children(buttons);
            div()
                .flex()
                .flex_col()
                .child(line)
                .when_some(row.reason.clone(), |row, reason| {
                    row.child(
                        div()
                            .text_size(px(11.))
                            .text_color(content(0.5))
                            .child(reason),
                    )
                })
        });
        Some(
            div()
                .id("task-controls")
                .flex()
                .flex_col()
                .gap_1()
                .children(rows)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(display: Display, resumable: bool) -> ControlRow {
        ControlRow {
            task_id: "t".into(),
            title: "T".into(),
            revision: 1,
            display,
            reason: None,
            resumable,
            confirm_stopped: false,
        }
    }

    #[test]
    fn each_state_offers_only_the_controls_that_apply() {
        let actions = |d, r| {
            offered(&row(d, r))
                .into_iter()
                .map(|(a, _)| a)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            actions(Display::Active, false),
            ["tasks.pause", "tasks.steer", "tasks.cancel"]
        );
        assert_eq!(
            actions(Display::Pausing, true),
            ["tasks.resume", "tasks.steer", "tasks.cancel"]
        );
        // Waiting on a question: no resume; held after a restart: resume.
        assert!(!actions(Display::Waiting, false).contains(&"tasks.resume"));
        assert!(actions(Display::Waiting, true).contains(&"tasks.resume"));
        assert!(!actions(Display::Blocked, false).contains(&"tasks.resume"));
        let held = ControlRow {
            confirm_stopped: true,
            ..row(Display::Waiting, true)
        };
        assert!(offered(&held).contains(&("tasks.resume", "Other app stopped: Resume")));
        assert!(actions(Display::Cancelling, false).is_empty());
        assert!(actions(Display::Completed, false).is_empty());
    }
}
