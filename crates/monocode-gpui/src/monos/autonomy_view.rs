//! The Mono's Autonomy settings: its policy (or a project's override), the
//! account fallback list, the spending note, and its tasks with Apply and
//! Unblock.
use super::autonomy::{Control, Field, SPENDING_NOTE, fields};
use super::panels::{button, card, heading};
use super::*;
use gpui::{Div, FontWeight, Stateful};
use monocode_core::accounts::eligibility::AllowedAccount;
use monocode_core::tasks::TaskLifecycle;
use monocode_core::tasks::autonomy::{Autonomy, MonoAutonomy};

const WORK: [&str; 6] = [
    "autonomy-worker-access",
    "autonomy-automatic-execution",
    "autonomy-workers",
    "autonomy-workspace",
    "autonomy-restart",
    "autonomy-retention",
];
const AUTHORITY: [&str; 4] = [
    "autonomy-commit",
    "autonomy-merge",
    "autonomy-push",
    "autonomy-deploy",
];
const RECOVERY: [&str; 3] = [
    "autonomy-retries",
    "autonomy-provider-fallback",
    "autonomy-billing",
];

/// A label and description above or beside a control that may wrap.
fn row(label: &str, description: &str, control: AnyElement, first: bool) -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_3()
        .px_3()
        .py_2()
        .when(!first, |d| d.border_t_1().border_color(content(0.04)))
        .child(
            div()
                .flex_1()
                .min_w(px(240.))
                .child(
                    div()
                        .text_size(px(13.))
                        .line_height(px(20.))
                        .text_color(content(0.9))
                        .child(label.to_owned()),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .line_height(px(18.))
                        .text_color(content(0.4))
                        .child(description.to_owned()),
                ),
        )
        .child(div().flex_shrink_0().child(control))
}

fn chip(id: String, label: &str, selected: bool) -> Stateful<Div> {
    let selector = id.clone();
    div()
        .id(SharedString::from(id))
        .debug_selector(move || selector.clone())
        .role(Role::RadioButton)
        .aria_label(label.to_owned())
        .aria_selected(selected)
        .cursor_pointer()
        .px_2()
        .py_1()
        .rounded_md()
        .text_size(px(12.))
        .map(|d| {
            if selected {
                d.bg(content(0.1)).text_color(content(1.))
            } else {
                d.text_color(content(0.55))
                    .hover(|s| s.text_color(content(1.)))
            }
        })
        .child(label.to_owned())
}

fn switch(id: String, label: &str, on: bool) -> Stateful<Div> {
    let selector = id.clone();
    div()
        .id(SharedString::from(id))
        .debug_selector(move || selector.clone())
        .role(Role::Switch)
        .aria_label(label.to_owned())
        .aria_selected(on)
        .cursor_pointer()
        .w(px(36.))
        .h(px(20.))
        .flex_shrink_0()
        .rounded_full()
        .bg(if on { accent_color() } else { content(0.2) })
        .child(
            div()
                .size(px(16.))
                .mt(px(2.))
                .ml(px(if on { 18. } else { 2. }))
                .rounded_full()
                .bg(gpui::white()),
        )
}

fn project_label(project: &str) -> String {
    std::path::Path::new(project)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| project.to_owned())
}

impl Monos {
    pub(super) fn autonomy_page(&mut self, mono: &Mono, cx: &mut Context<Self>) -> AnyElement {
        let policy = self.prefs.mono_policy(&mono.id);
        let current = match &self.autonomy.scope {
            Some(project) => policy.for_project(project).clone(),
            None => policy.policy.clone(),
        };
        let all = fields();
        let section = |ids: &[&str], this: &Self, cx: &mut Context<Self>| {
            card().children(
                ids.iter()
                    .filter_map(|id| all.iter().find(|f| f.id == *id))
                    .enumerate()
                    .map(|(ix, field)| this.autonomy_field(field, &current, ix == 0, cx))
                    .collect::<Vec<_>>(),
            )
        };
        let work = section(&WORK, self, cx);
        let authority = section(&AUTHORITY, self, cx);
        let recovery = section(&RECOVERY, self, cx);
        div()
            .id("mono-autonomy-page")
            .debug_selector(|| "mono-autonomy-page".into())
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_8()
            .pt_6()
            .pb_8()
            .child(div().text_size(px(14.)).font_weight(FontWeight::MEDIUM).child("Autonomy"))
            .child(
                div()
                    .pt_2()
                    .text_size(px(12.))
                    .line_height(px(18.))
                    .text_color(content(0.45))
                    .child(format!(
                        "What {} may do on its own. A task keeps the policy it started with: less authority applies to it at once, more reaches it when you apply the policy below.",
                        mono.label()
                    )),
            )
            .child(self.autonomy_scope(mono, &policy, cx))
            .when_some(self.autonomy.error.clone(), |d, error| {
                d.child(
                    div()
                        .pt_2()
                        .text_size(px(12.))
                        .text_color(rgb(0xff6467))
                        .child(error),
                )
            })
            .child(heading("Work", None))
            .child(work)
            .child(heading(
                "Authority",
                Some("Steps that change history beyond the task's worktree or ship it. Revoking one reaches running tasks too: new steps are refused, and while commit is revoked, review sends back work whose branch has commits."),
            ))
            .child(authority)
            .child(heading("Recovery and billing", None))
            .child(recovery.child(self.autonomy_accounts(cx)))
            .child(
                div()
                    .id("mono-autonomy-spending-note")
                    .debug_selector(|| "mono-autonomy-spending-note".into())
                    .pt_3()
                    .px_1()
                    .text_size(px(12.))
                    .line_height(px(18.))
                    .text_color(content(0.4))
                    .child(SPENDING_NOTE),
            )
            .child(heading("Tasks", Some("This Mono's unfinished tasks.")))
            .child(self.autonomy_tasks(&policy, cx))
            .into_any_element()
    }

    fn autonomy_field(
        &self,
        field: &Field,
        current: &Autonomy,
        first: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let id = format!("mono-{}", field.id);
        let control = match &field.control {
            Control::Switch { get, set } => {
                let (on, set) = (get(current), *set);
                switch(id, field.label, on)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.edit_autonomy(|p| set(p, !on), cx)),
                    )
                    .into_any_element()
            }
            Control::Choice { options, get, set } => {
                let (value, set) = (get(current), *set);
                div()
                    .flex()
                    .gap_1()
                    .children(options.iter().map(|&(option, text)| {
                        chip(format!("{id}-{option}"), text, value == option).on_click(cx.listener(
                            move |this, _, _, cx| this.edit_autonomy(|p| set(p, option), cx),
                        ))
                    }))
                    .into_any_element()
            }
        };
        row(field.label, field.description, control, first)
    }

    /// Which policy the page edits: the Mono's, or one project's override.
    fn autonomy_scope(
        &self,
        mono: &Mono,
        policy: &MonoAutonomy,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let scope = self.autonomy.scope.clone();
        let chips =
            std::iter::once(
                chip(
                    "mono-autonomy-scope-all".into(),
                    "All projects",
                    scope.is_none(),
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_autonomy_scope(None, cx))),
            )
            .chain(mono.projects.iter().enumerate().map(|(ix, project)| {
                let chosen = project.clone();
                chip(
                    format!("mono-autonomy-scope-{ix}"),
                    &project_label(project),
                    scope.as_deref() == Some(project.as_str()),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_autonomy_scope(Some(chosen.clone()), cx)
                }))
            }))
            .collect::<Vec<_>>();
        let note =
            match &scope {
                None => None,
                Some(project) if policy.has_override(project) => Some(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child("This project has its own policy.")
                        .child(
                            button("mono-autonomy-use-mono", "Use the Mono's policy")
                                .debug_selector(|| "mono-autonomy-use-mono".into())
                                .on_click(cx.listener(|this, _, _, cx| this.use_mono_autonomy(cx))),
                        ),
                ),
                Some(_) => Some(div().child(
                    "This project follows the Mono's policy; a change here gives it its own.",
                )),
            };
        div()
            .pt_4()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().flex().flex_wrap().gap_1().children(chips))
            .when_some(note, |d, note| {
                d.child(note.text_size(px(12.)).text_color(content(0.5)))
            })
            .into_any_element()
    }

    /// The live account fallback list for the scope, in priority order.
    fn autonomy_accounts(&self, cx: &mut Context<Self>) -> Div {
        let listed: Vec<AllowedAccount> = self
            .autonomy
            .allowlist
            .as_ref()
            .map(|list| list.ordered().into_iter().cloned().collect())
            .unwrap_or_default();
        let label = |account: &AllowedAccount| {
            self.autonomy
                .known
                .iter()
                .find(|(known, _)| {
                    known.provider == account.provider && known.account_id == account.account_id
                })
                .map(|(_, label)| label.clone())
                .unwrap_or_else(|| format!("{}/{} (removed)", account.provider, account.account_id))
        };
        let description = if self.autonomy.scope.is_some() {
            "Accounts this project's tasks may move to after a usage or sign-in failure, first to last. With none, the Mono's list applies. Changes apply at the next recovery decision."
        } else {
            "Accounts this Mono's tasks may move to after a usage or sign-in failure, first to last. With none, the project's list applies, else only the account a task started on. Changes apply at the next recovery decision."
        };
        let rows = listed.iter().enumerate().map(|(ix, account)| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_size(px(12.))
                .child(div().flex_1().min_w_0().truncate().child(format!(
                    "{}. {}",
                    ix + 1,
                    label(account)
                )))
                .when(ix > 0, |d| {
                    d.child(
                        button(SharedString::from(format!("mono-autonomy-up-{ix}")), "Up")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.edit_allowlist(|list| list.swap(ix, ix - 1), cx)
                            })),
                    )
                })
                .child({
                    let id = format!("mono-autonomy-remove-{ix}");
                    let selector = id.clone();
                    button(SharedString::from(id), "Remove")
                        .debug_selector(move || selector.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.edit_allowlist(
                                |list| {
                                    list.remove(ix);
                                },
                                cx,
                            )
                        }))
                })
        });
        let addable = self.autonomy.known.iter().filter(|(known, _)| {
            !listed
                .iter()
                .any(|a| a.provider == known.provider && a.account_id == known.account_id)
        });
        let adds = addable.map(|(known, text)| {
            let account = known.clone();
            chip(
                format!("mono-autonomy-add-{}-{}", known.provider, known.account_id),
                &format!("+ {text}"),
                false,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                let account = account.clone();
                this.edit_allowlist(move |list| list.push(account), cx)
            }))
        });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(content(0.04))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(content(0.9))
                    .child("Account fallback list"),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .line_height(px(18.))
                    .text_color(content(0.4))
                    .child(description),
            )
            .children(rows)
            .when(self.autonomy.loaded, |d| {
                d.child(div().flex().flex_wrap().gap_1().children(adds))
            })
    }

    /// The Mono's unfinished tasks, with Apply and Unblock.
    fn autonomy_tasks(&self, policy: &MonoAutonomy, cx: &mut Context<Self>) -> AnyElement {
        if self.autonomy.tasks.is_empty() {
            return card()
                .child(
                    div()
                        .px_3()
                        .py_3()
                        .text_size(px(12.))
                        .text_color(content(0.4))
                        .child("No unfinished tasks."),
                )
                .into_any_element();
        }
        let rows = self
            .autonomy
            .tasks
            .clone()
            .into_iter()
            .enumerate()
            .map(|(ix, standing)| {
                let task = &standing.task;
                let live = policy.for_project(&task.project).clone().normalized();
                let differs = standing
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.autonomy != live);
                let blocked = task.lifecycle == TaskLifecycle::Blocked;
                let status = match &standing.snapshot {
                    None => "takes the policy when it first launches".to_owned(),
                    Some(s) if differs => {
                        format!("policy revision {}; yours has changed since", s.revision)
                    }
                    Some(s) => format!("policy revision {}, current", s.revision),
                };
                let row_id = format!("mono-autonomy-task-{}", task.id);
                let selector = row_id.clone();
                let apply = standing.clone();
                let unblock = standing.clone();
                div()
                    .id(SharedString::from(row_id))
                    .debug_selector(move || selector.clone())
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .when(ix > 0, |d| d.border_t_1().border_color(content(0.04)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .text_color(content(0.9))
                                    .truncate()
                                    .child(task.title.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(content(0.4))
                                    .child(format!("{} · {status}", task.lifecycle.as_str())),
                            )
                            .when_some(
                                task.blocked_reason.clone().filter(|_| blocked),
                                |d, reason| {
                                    d.child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(rgb(0xff6467))
                                            .child(reason),
                                    )
                                },
                            ),
                    )
                    .when(differs, |d| {
                        let id = format!("mono-autonomy-apply-{}", task.id);
                        let selector = id.clone();
                        d.child(
                            button(SharedString::from(id), "Apply policy")
                                .debug_selector(move || selector.clone())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.apply_task_policy(&apply, cx)
                                })),
                        )
                    })
                    .when(blocked, |d| {
                        let id = format!("mono-autonomy-unblock-{}", task.id);
                        let selector = id.clone();
                        d.child(
                            button(SharedString::from(id), "Unblock")
                                .debug_selector(move || selector.clone())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.unblock_task(&unblock, cx)
                                })),
                        )
                    })
            });
        card().children(rows.collect::<Vec<_>>()).into_any_element()
    }
}
