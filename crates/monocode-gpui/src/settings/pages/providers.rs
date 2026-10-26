use crate::{
    input::Input,
    settings::{
        SettingsView,
        widgets::{note, secondary_button},
    },
    theme::*,
};
use gpui::{AnyElement, Context, Entity, SharedString, div, prelude::*, px};
use monocode_core::{api_keys, provider::Kind};
/// Static ids for the API key rows: row, toggle, input, save, clear.
fn api_key_ids(
    kind: Kind,
) -> (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
) {
    match kind {
        Kind::Codex => (
            "api-key-codex",
            "api-key-codex",
            "settings-api-key-codex-input",
            "api-key-codex-save",
            "api-key-codex-clear",
        ),
        _ => (
            "api-key-claude",
            "api-key-claude",
            "settings-api-key-claude-input",
            "api-key-claude-save",
            "api-key-claude-clear",
        ),
    }
}
impl SettingsView {
    pub(crate) fn api_key_input(&mut self, kind: Kind, cx: &mut Context<Self>) -> Entity<Input> {
        self.api_key_inputs
            .entry(kind)
            .or_insert_with(|| {
                cx.new(|cx| {
                    let mut field = Input::field(
                        format!("{} API key", kind.label()),
                        "Paste key",
                        content(0.4),
                        cx,
                    );
                    field.set_secret(true);
                    field
                })
            })
            .clone()
    }
    fn save_api_key(&mut self, kind: Kind, cx: &mut Context<Self>) {
        let input = self.api_key_input(kind, cx);
        let key = input.read(cx).content.to_string();
        match api_keys::set(kind, &key) {
            Ok(status) => {
                self.api_key_notice = None;
                input.update(cx, |input, cx| input.set_text(String::new(), cx));
                if !status.configured && self.prefs.api_key_enabled(kind) {
                    self.update(cx, |p| {
                        p.api_key_enabled.insert(kind.id().to_owned(), false);
                    });
                }
            }
            Err(error) => self.api_key_notice = Some((kind, error)),
        }
        cx.notify();
    }
    fn clear_api_key(&mut self, kind: Kind, cx: &mut Context<Self>) {
        match api_keys::clear(kind) {
            Ok(_) => {
                self.api_key_notice = None;
                // Never leave the switch on without a key behind it.
                self.update(cx, |p| {
                    p.api_key_enabled.insert(kind.id().to_owned(), false);
                });
            }
            Err(error) => self.api_key_notice = Some((kind, error)),
        }
        cx.notify();
    }
    fn api_key_rows(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = self.prefs.clone();
        let mut rows = Vec::new();
        for (ix, kind) in [Kind::Claude, Kind::Codex].into_iter().enumerate() {
            let status = api_keys::status(kind);
            let configured = status.as_ref().is_ok_and(|s| s.configured);
            let summary = match (&self.api_key_notice, &status) {
                (Some((noted, text)), _) if *noted == kind => text.clone(),
                (_, Ok(s)) if s.configured => format!(
                    "Key ending in …{}. Set as {}.",
                    s.hint.clone().unwrap_or_default(),
                    api_keys::env_var(kind).unwrap_or_default()
                ),
                (_, Ok(_)) => format!(
                    "No key saved. The CLI keeps using your {} login.",
                    kind.label()
                ),
                (_, Err(error)) => error.clone(),
            };
            let (row_id, toggle_id, input_id, save_id, clear_id) = api_key_ids(kind);
            let input = self.api_key_input(kind, cx);
            let mut controls = vec![
                self.toggle(
                    toggle_id,
                    format!("Use {} API key", kind.label()),
                    configured && p.api_key_enabled(kind),
                    !configured,
                    cx,
                    move |this, on, cx| {
                        this.update(cx, |p| {
                            p.api_key_enabled.insert(kind.id().to_owned(), on);
                        })
                    },
                ),
                div()
                    .id(input_id)
                    .debug_selector(move || input_id.into())
                    .w(px(208.))
                    .h(px(28.))
                    .px_2()
                    .rounded_md()
                    .bg(content(0.05))
                    .flex()
                    .items_center()
                    .child(input)
                    .into_any_element(),
                secondary_button(save_id, "Save", false, false)
                    .debug_selector(move || format!("settings-{save_id}"))
                    .child("Save")
                    .on_click(cx.listener(move |this, _, _, cx| this.save_api_key(kind, cx)))
                    .into_any_element(),
            ];
            if configured {
                controls.push(
                    secondary_button(clear_id, "Clear", true, false)
                        .debug_selector(move || format!("settings-{clear_id}"))
                        .child("Clear")
                        .on_click(cx.listener(move |this, _, _, cx| this.clear_api_key(kind, cx)))
                        .into_any_element(),
                );
            }
            let label = div()
                .flex()
                .items_center()
                .gap_2()
                .child(provider_icon(kind.id(), 16.))
                .child(format!("{} API key", kind.label()));
            rows.push(self.row(Some(row_id), ix == 0, label, Some(summary.into()), controls));
        }
        rows
    }
    pub(super) fn providers_page(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = self.prefs.clone();
        let accounts = if let Some(center) = crate::accounts::get(cx) {
            center.update(cx, |center, cx| {
                center.mask_emails = p.mask_emails;
                center.show_remaining = p.show_remaining_usage;
                for kind in monocode_core::accounts::PROVIDERS {
                    center.load_provider(kind, false, cx);
                }
                center.settings_rows(cx)
            })
        } else {
            vec![note("Provider accounts load with the workspace.")]
        };
        let usage = vec![
            self.row(
                Some("show-remaining-usage"),
                true,
                "Show remaining usage",
                Some("Show remaining capacity in usage meters.".into()),
                vec![self.toggle(
                    "show-remaining-usage",
                    "Show remaining usage",
                    p.show_remaining_usage,
                    false,
                    cx,
                    |this, on, cx| this.update(cx, |p| p.show_remaining_usage = on),
                )],
            ),
            self.row(
                Some("mask-emails"),
                false,
                "Mask account emails",
                Some("Hide email addresses until clicked.".into()),
                vec![self.toggle(
                    "mask-emails",
                    "Mask account emails",
                    p.mask_emails,
                    false,
                    cx,
                    |this, on, cx| this.update(cx, |p| p.mask_emails = on),
                )],
            ),
        ];
        let mut options = vec![(
            "global".into(),
            SharedString::from("Global"),
            Some(icon("globe", 14.).into_any_element()),
        )];
        options.extend(self.ctx.projects.iter().map(|(path, label)| {
            (
                path.clone(),
                SharedString::from(label.clone()),
                Some(icon("folder", 14.).into_any_element()),
            )
        }));
        let scope = self.provider_scope.clone();
        let action = self.select(
            "provider-scope",
            "Provider defaults scope",
            &scope,
            options,
            None,
            cx,
            |this, value, cx| {
                this.provider_scope = value;
                cx.notify();
            },
        );
        let api_key_rows = self.api_key_rows(cx);
        let binaries = self.discovery.binaries.clone();
        let mut clis = Vec::new();
        for (index, kind) in Kind::ALL.into_iter().enumerate() {
            let installed = binaries
                .iter()
                .find(|(k, _)| *k == kind)
                .and_then(|(_, p)| p.as_ref());
            let project = if scope == "global" {
                None
            } else {
                p.project_providers.get(&scope)
            };
            let selected = project
                .and_then(|p| p.provider.as_deref())
                .or(p.default_provider.as_deref())
                == Some(kind.id());
            let globally_hidden = p.hidden_providers.iter().any(|id| id == kind.id());
            let locked = scope != "global" && globally_hidden;
            let visible = !globally_hidden
                && !project.is_some_and(|p| p.hidden.iter().any(|id| id == kind.id()));
            let models = self
                .ctx
                .catalogs
                .get(kind.id())
                .cloned()
                .unwrap_or_default();
            let selected_model = project
                .and_then(|p| p.models.get(kind.id()))
                .or(p.default_models.get(kind.id()))
                .cloned()
                .or_else(|| models.first().map(|m| m.0.clone()));
            let label = div()
                .flex()
                .items_center()
                .gap_2()
                .child(provider_icon(kind.id(), 16.))
                .child(kind.label())
                .child({
                    let slot = self
                        .binary_anchors
                        .borrow_mut()
                        .entry(kind)
                        .or_default()
                        .clone();
                    secondary_button(
                        SharedString::from(format!("binary-{}", kind.id())),
                        format!("Show {} CLI details", kind.label()),
                        false,
                        false,
                    )
                    .relative()
                    .size_6()
                    .p_0()
                    .justify_center()
                    .child(slot.probe())
                    .child(icon("folder-open", 14.))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_management(
                            super::super::management::Target::Cli(kind),
                            window,
                            cx,
                        )
                    }))
                })
                .child(
                    div()
                        .rounded_full()
                        .bg(content(0.1))
                        .px(px(6.))
                        .py(px(2.))
                        .text_size(px(10.))
                        .text_color(content(0.6))
                        .child(if installed.is_some() {
                            "INSTALLED"
                        } else {
                            "NOT INSTALLED"
                        }),
                );
            let mut controls = vec![
                secondary_button(
                    SharedString::from(format!("default-provider-{}", kind.id())),
                    if selected {
                        "Default"
                    } else {
                        "Use by default"
                    },
                    false,
                    selected || selected_model.is_none() || installed.is_none(),
                )
                .child(if selected {
                    "Default"
                } else {
                    "Use by default"
                })
                .when(
                    !selected && selected_model.is_some() && installed.is_some(),
                    |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| {
                            let scope = this.provider_scope.clone();
                            this.update(cx, |p| {
                                if scope == "global" {
                                    p.default_provider = Some(kind.id().into());
                                } else {
                                    p.project_providers.entry(scope).or_default().provider =
                                        Some(kind.id().into());
                                }
                            })
                        }))
                    },
                )
                .into_any_element(),
            ];
            if let Some(model) = &selected_model {
                controls.insert(
                    0,
                    self.select(
                        kind.id(),
                        kind.label(),
                        model,
                        models
                            .iter()
                            .map(|(id, name)| (id.clone(), SharedString::from(name.clone()), None))
                            .collect(),
                        None,
                        cx,
                        move |this, model, cx| {
                            let scope = this.provider_scope.clone();
                            this.update(cx, |p| {
                                if scope == "global" {
                                    p.default_models.insert(kind.id().into(), model);
                                } else {
                                    p.project_providers
                                        .entry(scope)
                                        .or_default()
                                        .models
                                        .insert(kind.id().into(), model);
                                }
                            });
                        },
                    ),
                );
            }
            if installed.is_some() {
                controls.push(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(content(0.5))
                                .child(if locked {
                                    "Hidden globally"
                                } else {
                                    "Show in picker"
                                }),
                        )
                        .child(self.toggle(
                            kind.id(),
                            "Show provider in the model picker",
                            visible,
                            locked,
                            cx,
                            move |this, on, cx| {
                                let scope = this.provider_scope.clone();
                                this.update(cx, |p| {
                                    let hidden = if scope == "global" {
                                        &mut p.hidden_providers
                                    } else {
                                        &mut p.project_providers.entry(scope).or_default().hidden
                                    };
                                    hidden.retain(|id| id != kind.id());
                                    if !on {
                                        hidden.push(kind.id().into());
                                    }
                                })
                            },
                        ))
                        .into_any_element(),
                );
            }
            let description = installed
                .map(|_| {
                    format!(
                        "{} {} available.",
                        models.len(),
                        if models.len() == 1 { "model" } else { "models" }
                    )
                })
                .unwrap_or_else(|| {
                    format!(
                        "Install {} and sign in with {}.",
                        kind.label(),
                        kind.login_hint()
                    )
                });
            clis.push(self.row(None, index == 0, label, Some(description.into()), controls));
        }
        if self.discovery_loading {
            clis.insert(0, note("Checking installed CLIs…"));
        }
        let advanced=vec![
            self.row(Some("idle-agents"),true,"Keep idle agents ready",Some("Keep up to this many idle agents running for faster follow-ups. Uses additional memory.".into()),vec![self.segmented("idle-agents","Keep idle agents ready",&p.idle_agents.to_string(),&[("0","None"),("1","1"),("2","2"),("3","3"),("5","5")],cx,|this,value,cx|if let Ok(value)=value.parse(){this.update(cx,|p|p.idle_agents=value)})]),
            self.row(Some("claude-hooks"),false,"Claude Code hooks",Some("Run your configured Claude Code hooks. Changes apply on the next turn.".into()),vec![self.toggle("claude-hooks","Claude Code hooks",p.claude_hooks,false,cx,|this,on,cx|this.update(cx,|p|p.claude_hooks=on))])];
        vec![
            self.group(
                Some("provider-accounts"),
                true,
                "Accounts",
                Some("Manage sign-ins and choose accounts for each project.".into()),
                Some(self.refresh_button("refresh-provider-accounts", "Refresh", cx)),
                accounts,
            ),
            self.group(None, false, "Usage and privacy", None, None, usage),
            self.group(
                Some("api-keys"),
                false,
                "API keys",
                Some("Bill a provider's API key instead of its subscription login. Saved keys stay on this Mac in a file only you can read and are handed to the CLI as an environment variable. Use API key takes effect on a conversation's next turn and keeps the same conversation.".into()),
                None,
                api_key_rows,
            ),
            self.group(
                Some("agent-clis"),
                false,
                "Agent CLIs",
                Some("Choose available agents and default models for new conversations.".into()),
                Some(action),
                clis,
            ),
            self.group(None, false, "Advanced", None, None, advanced),
        ]
    }
}
