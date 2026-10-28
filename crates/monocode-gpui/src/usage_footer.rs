use crate::{
    popover::{Anchor, AnchorSlot, Side, popover},
    theme::*,
};
use gpui::{
    App, Context, FocusHandle, FontWeight, KeyBinding, Role, Window, actions, div, prelude::*, px,
    rgb,
};
#[cfg(test)]
use monocode_core::usage::{FetchError, Snapshot};
use monocode_core::{provider::Kind, usage};
use std::time::{Duration, Instant};

mod resets;

actions!(usage_footer, [DismissUsage, ToggleUsage, RefreshUsage]);
pub(crate) fn bind(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", DismissUsage, Some("UsageDetails")),
        KeyBinding::new("enter", ToggleUsage, Some("UsageChip")),
        KeyBinding::new("space", ToggleUsage, Some("UsageChip")),
        KeyBinding::new("enter", RefreshUsage, Some("UsageRefresh")),
        KeyBinding::new("space", RefreshUsage, Some("UsageRefresh")),
    ]);
}

use crate::accounts::{Cached as CachedUsage, Key};

fn index(kind: Kind) -> usize {
    match kind {
        Kind::Codex => 0,
        Kind::Claude => 1,
        _ => {
            2 + Kind::ALL
                .iter()
                .filter(|k| !matches!(k, Kind::Codex | Kind::Claude))
                .position(|k| *k == kind)
                .unwrap()
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum UsageFooterEvent {
    /// The API button beside the chip: bill the saved key, or the login again.
    ApiKey {
        kind: Kind,
        on: bool,
    },
    Mcp,
    ManageMcp,
}
impl gpui::EventEmitter<UsageFooterEvent> for UsageFooter {}
const API_KEY_MISSING: &str = "Add an API key in Settings → Providers first.";
/// Its own cached surface: clock ticks and probe completions don't rebuild chat.
pub(crate) struct UsageFooter {
    kind: Kind,
    key: Key,
    project: String,
    session: Option<i64>,
    show_accounts: bool,
    confirm_reset: Option<String>,
    _accounts_observation: Option<gpui::Subscription>,
    _account_events: Option<gpui::Subscription>,
    cache: [CachedUsage; Kind::ALL.len()],
    anchor: AnchorSlot,
    trigger: FocusHandle,
    refresh_focus: FocusHandle,
    details: FocusHandle,
    previous_focus: Option<FocusHandle>,
    open: bool,
    mcp_open: bool,
    mcp_note: String,
    mcp_request: u64,
    mcp_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    mcp_servers: Vec<(String, String)>,
    /// UsageFooter's `footer` variant: the classic layout's bordered strip.
    classic: bool,
    show_remaining: bool,
    /// "Use API key" per provider id, mirrored from the preferences.
    api_keys: std::collections::HashMap<String, bool>,
    /// Why the API button did not switch on, until dismissed.
    api_key_note: Option<String>,
    _clock: gpui::Task<()>,
}
impl Drop for UsageFooter {
    fn drop(&mut self) {
        if let Some(cancel) = &self.mcp_cancel {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
impl UsageFooter {
    pub(crate) fn show_mcp(&mut self, cx: &mut Context<Self>) {
        self.mcp_open = !self.mcp_open;
        self.open = false;
        if self.mcp_open && self.mcp_cancel.is_none() {
            self.refresh_mcp(cx);
        }
        cx.notify();
    }
    fn refresh_mcp(&mut self, cx: &mut Context<Self>) {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        if let Some(cancel) = self.mcp_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.mcp_request += 1;
        let request = self.mcp_request;
        let project = self.project.clone();
        self.mcp_servers.clear();
        if project.is_empty() {
            self.mcp_note = "Select one valid project to initialize its MCP connections.".into();
            cx.notify();
            return;
        }
        self.mcp_note = format!("Checking project MCP servers · {project}");
        let kind = self.kind;
        let profile = self.key.profile.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.mcp_cancel = Some(cancel.clone());
        let task = cx.background_executor().spawn(async move {
            monocode_core::mcp::project_connections(
                kind,
                std::path::Path::new(&project),
                &profile,
                cancel,
            )
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.mcp_request != request { return; }
                this.mcp_cancel = None;
                match result {
                    Ok(servers) => {
                        this.mcp_note = if servers.is_empty() {
                            format!("{} · No MCP registrations in this project's {} configuration.", this.project, kind.label())
                        } else {
                            format!("{} · Independent {} connection check at {}. This does not reconnect an existing chat; its provider owns that connection.", this.project, kind.label(), chrono::Local::now().format("%H:%M:%S"))
                        };
                        this.mcp_servers = servers;
                    }
                    Err(error) => this.mcp_note = format!("{} · {error}", this.project),
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }
    pub(crate) fn project_without_session(&mut self, project: String, cx: &mut Context<Self>) {
        self.context(self.kind, self.key.profile.clone(), None, project, None, cx);
    }
    pub(crate) fn context(
        &mut self,
        kind: Kind,
        profile: monocode_core::accounts::Profile,
        model: Option<&str>,
        project: String,
        session: Option<i64>,
        cx: &mut Context<Self>,
    ) {
        let changed_project =
            self.project != project || self.kind != kind || self.key.profile != profile;
        let key = Key::new(kind, profile, model);
        if self.project != project || self.session != session {
            self.mcp_open = false;
        }
        self.project = project;
        self.session = session;
        if self.key != key {
            self.kind = kind;
            self.key = key;
            self.open = false;
            self.api_key_note = None;
            self.show_accounts = false;
            self.confirm_reset = None;
            self.cache[index(kind)] = Default::default();
            self.refresh(false, cx);
            cx.notify();
        }
        if changed_project {
            // A changed project invalidates the shown result, but a check
            // launches provider processes that connect to every MCP server:
            // it runs only while the panel is open to show it.
            if let Some(cancel) = self.mcp_cancel.take() {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            self.mcp_request += 1;
            self.mcp_servers.clear();
            self.mcp_note = if self.project.is_empty() {
                "Select one valid project to initialize its MCP connections.".into()
            } else {
                format!(
                    "{} · Open this panel to check its MCP servers.",
                    self.project
                )
            };
            if self.mcp_open {
                self.refresh_mcp(cx);
            }
            cx.notify();
        }
    }
    pub(crate) fn new(kind: Kind, cx: &mut Context<Self>) -> Self {
        let clock = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(30))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        this.refresh(false, cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let observation = crate::accounts::get(cx).map(|center| {
            cx.observe(&center, |this, center, cx| {
                if let Some(state) = center.read(cx).usage.get(&this.key).cloned() {
                    this.cache[index(this.kind)] = state;
                }
                cx.notify();
            })
        });
        let mut this = Self {
            _account_events: crate::accounts::get(cx).map(|center| {
                cx.subscribe(
                    &center,
                    |_this, center, event: &crate::accounts::AccountEvent, cx| {
                        let modal_auth =
                            matches!(event, crate::accounts::AccountEvent::AuthStarted)
                                && center.read(cx).has_modal_auth();
                        if modal_auth
                            || matches!(
                                event,
                                crate::accounts::AccountEvent::Select { .. }
                                    | crate::accounts::AccountEvent::PickerDismiss
                                    | crate::accounts::AccountEvent::Manage
                            )
                        {
                            cx.spawn(async move |this, cx| {
                                let _ =
                                    this.update_in(cx, |this, window, cx| this.dismiss(window, cx));
                            })
                            .detach();
                        }
                    },
                )
            }),
            kind,
            key: Key::new(kind, Default::default(), None),
            project: String::new(),
            session: None,
            show_accounts: false,
            confirm_reset: None,
            _accounts_observation: observation,
            cache: Default::default(),
            anchor: AnchorSlot::default(),
            trigger: cx.focus_handle(),
            refresh_focus: cx.focus_handle(),
            details: cx.focus_handle(),
            previous_focus: None,
            open: false,
            mcp_open: false,
            mcp_note: String::new(),
            mcp_request: 0,
            mcp_cancel: None,
            mcp_servers: Vec::new(),
            classic: false,
            show_remaining: false,
            api_keys: Default::default(),
            api_key_note: None,
            _clock: clock,
        };
        this.refresh(false, cx);
        this
    }
    pub(crate) fn set_api_keys(
        &mut self,
        keys: std::collections::HashMap<String, bool>,
        cx: &mut Context<Self>,
    ) {
        if self.api_keys != keys {
            self.api_keys = keys;
            cx.notify();
        }
    }
    fn api_key_enabled(&self) -> bool {
        self.api_keys.get(self.kind.id()).copied().unwrap_or(false)
    }
    /// Switching on needs a saved key; otherwise the chat would quietly stay
    /// on the login. Switching off never needs one.
    fn toggle_api_key(&mut self, cx: &mut Context<Self>) {
        let kind = self.kind;
        if self.api_key_enabled() {
            self.api_key_note = None;
            cx.emit(UsageFooterEvent::ApiKey { kind, on: false });
        } else {
            match monocode_core::api_keys::status(kind) {
                Ok(status) if status.configured => {
                    self.api_key_note = None;
                    cx.emit(UsageFooterEvent::ApiKey { kind, on: true });
                }
                Ok(_) => self.api_key_note = Some(API_KEY_MISSING.into()),
                Err(error) => self.api_key_note = Some(error),
            }
        }
        cx.notify();
    }
    fn api_key_button(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let on = self.api_key_enabled();
        let label = format!("Use {} API key", self.kind.label());
        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .child(
                div()
                    .id("api-key-toggle")
                    .debug_selector(|| "api-key-toggle".into())
                    .role(Role::Button)
                    .aria_label(label)
                    .aria_description(if on {
                        "API billing on. Click to use the plan for this provider's chats and Monos on their next turn."
                    } else {
                        "API billing off. Click to use the saved API key for this provider's chats and Monos on their next turn."
                    })
                    .h(px(20.))
                    .px(px(6.))
                    .rounded_md()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .text_size(px(10.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .cursor_pointer()
                    .map(|d| {
                        if on {
                            d.bg(content(0.12)).text_color(content(0.95))
                        } else {
                            d.text_color(content(0.4)).hover(|s| s.bg(content(0.08)))
                        }
                    })
                    .child(
                        div()
                            .size(px(4.))
                            .rounded_full()
                            .bg(content(if on { 0.95 } else { 0.25 })),
                    )
                    .child("API")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_api_key(cx))),
            )
            .when_some(self.api_key_note.clone(), |d, note| {
                d.child(
                    div()
                        .id("api-key-note")
                        .debug_selector(|| "api-key-note".into())
                        .role(Role::Alert)
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .text_size(px(10.))
                        .text_color(content(0.7))
                        .child(note)
                        .child(
                            div()
                                .id("dismiss-api-key-note")
                                .role(Role::Button)
                                .aria_label("Dismiss API key message")
                                .cursor_pointer()
                                .text_color(content(0.45))
                                .child("×")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.api_key_note = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .into_any_element()
    }
    pub(crate) fn set_remaining(&mut self, remaining: bool, cx: &mut Context<Self>) {
        if self.show_remaining != remaining {
            self.show_remaining = remaining;
            cx.notify();
        }
    }
    fn display_percent(&self, used: f64) -> f64 {
        if self.show_remaining {
            100. - used
        } else {
            used
        }
    }
    fn percent_label(&self, used: f64) -> String {
        format!(
            "{:.0}% {}",
            self.display_percent(used),
            if self.show_remaining { "left" } else { "used" }
        )
    }
    pub(crate) fn set_classic(&mut self, classic: bool, cx: &mut Context<Self>) {
        if self.classic != classic {
            self.classic = classic;
            cx.notify();
        }
    }
    #[cfg(test)]
    pub(crate) fn provider(&mut self, kind: Kind, cx: &mut Context<Self>) {
        if self.kind != kind {
            self.kind = kind;
            self.api_key_note = None;
            self.key = Key::new(kind, Default::default(), None);
            self.open = false;
            self.refresh(false, cx);
            cx.notify();
        }
    }
    fn refresh(&mut self, force: bool, cx: &mut Context<Self>) {
        if let Some(center) = crate::accounts::get(cx) {
            let key = self.key.clone();
            center.update(cx, |center, cx| center.refresh(key.clone(), force, cx));
            if let Some(state) = center.read(cx).usage.get(&key).cloned() {
                self.cache[index(self.kind)] = state;
            }
            cx.notify();
            return;
        }
        if cfg!(test) {
            return;
        }
        let slot = index(self.kind);
        let state = &mut self.cache[slot];
        let reset = state.snapshot.as_ref().is_some_and(|s| {
            s.windows.iter().any(|w| {
                w.resets_at
                    .is_some_and(|at| at > s.updated_at && at <= usage::now_ms())
            })
        });
        let interval = if reset { 300 } else { 900 };
        if state.fetching
            || (!force
                && state
                    .attempted
                    .is_some_and(|at| at.elapsed() < Duration::from_secs(interval)))
        {
            return;
        }
        state.fetching = true;
        state.attempted = Some(Instant::now());
        let kind = self.kind;
        let work = cx
            .background_executor()
            .spawn(async move { usage::fetch(kind) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                // Store by the requested provider, even if the user changed tabs.
                this.cache[slot].complete(result);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    #[cfg(test)]
    pub(crate) fn kind(&self) -> Kind {
        self.kind
    }
    fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.dismiss(window, cx);
        } else {
            self.previous_focus = window.focused(cx).filter(|f| *f != self.trigger);
            self.open = true;
            self.show_accounts = false;
            if let Some(center) = crate::accounts::get(cx) {
                let kind = self.kind;
                let key = self.key.clone();
                center.update(cx, |center, cx| {
                    if center
                        .reset_actions
                        .get(&key)
                        .is_some_and(|a| !a.applying() && a.row.is_none())
                    {
                        center.reset_actions.remove(&key);
                    }
                    center.hide_emails();
                    center.load_provider(kind, false, cx);
                });
            }
            window.focus(&self.details, cx);
            cx.notify();
        }
    }
    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        self.open = false;
        self.confirm_reset = None;
        window.focus(self.previous_focus.as_ref().unwrap_or(&self.trigger), cx);
        cx.notify();
    }
    fn login_view(&self, cx: &App) -> bool {
        let state = &self.cache[index(self.kind)];
        monocode_core::accounts::login_arguments(
            self.kind,
            monocode_core::accounts::AuthAction::SignIn,
        )
        .is_some()
            && state.snapshot.as_ref().is_none_or(|s| s.windows.is_empty())
            && (state.error.as_ref().is_some_and(|e| e.unavailable)
                || crate::accounts::get(cx).is_some_and(|center| {
                    center
                        .read(cx)
                        .has_footer_auth(self.kind, &self.key.profile.id)
                }))
    }
    /// React's AccountSwitchRow, above the sign-in panel.
    fn account_row(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let label = crate::accounts::get(cx)
            .and_then(|center| {
                center
                    .read(cx)
                    .book
                    .list(self.kind)
                    .into_iter()
                    .find(|a| a.id == self.key.profile.id)
            })
            .map(|a| a.label)
            .unwrap_or_else(|| "Removed account".into());
        div()
            .id("usage-switch-account")
            .role(Role::Button)
            .aria_label(format!("Switch account from {label}"))
            .h_8()
            .px(px(10.))
            .rounded_lg()
            .bg(content(0.045))
            .border_1()
            .border_color(content(0.06))
            .hover(|s| s.bg(content(0.08)))
            .cursor_pointer()
            .flex()
            .items_center()
            .gap_2()
            .text_size(px(11.))
            .child(div().flex_1().min_w_0().truncate().child(label))
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(content(0.4))
                    .child("Switch"),
            )
            .child(icon("chevron-right", 12.).text_color(content(0.35)))
            .on_click(cx.listener(|this, _, window, cx| this.open_accounts(window, cx)))
            .into_any_element()
    }
    fn details(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.show_accounts
            && let Some(center) = crate::accounts::get(cx)
        {
            let kind = self.kind;
            let selected = self.key.profile.id.clone();
            let project = self.project.clone();
            let session = self.session;
            let weak = cx.weak_entity();
            let picker = center.update(cx, |center, cx| {
                center.picker(
                    kind,
                    &selected,
                    &project,
                    session,
                    move |window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.show_accounts = false;
                            window.focus(&this.details, cx);
                            cx.notify();
                        });
                    },
                    cx,
                )
            });
            return div()
                .id("footer-account-picker")
                .role(Role::Dialog)
                .aria_label("Choose account")
                .track_focus(&self.details)
                .key_context("UsageDetails")
                .on_action(
                    cx.listener(|this, _: &DismissUsage, window, cx| this.dismiss(window, cx)),
                )
                .child(picker)
                .into_any_element();
        }

        if self.login_view(cx)
            && let Some(center) = crate::accounts::get(cx)
        {
            let panel = center.update(cx, |center, cx| {
                center.footer_auth_panel(self.kind, &self.key.profile.id, cx)
            });
            return div()
                .id("usage-details-content")
                .role(Role::Dialog)
                .aria_label(format!("{} usage details", self.kind.label()))
                .track_focus(&self.details)
                .key_context("UsageDetails")
                .on_action(
                    cx.listener(|this, _: &DismissUsage, window, cx| this.dismiss(window, cx)),
                )
                .flex()
                .flex_col()
                .when(monocode_core::accounts::supports_accounts(self.kind), |d| {
                    d.child(div().px(px(10.)).pt(px(10.)).child(self.account_row(cx)))
                })
                .child(panel)
                .into_any_element();
        }
        let state = &self.cache[index(self.kind)];
        let now = usage::now_ms();
        let updated = match &state.snapshot {
            Some(snapshot) if snapshot.updated_at > 0 => {
                match (now - snapshot.updated_at).max(0) / 60_000 {
                    0 => "Updated just now".to_owned(),
                    minutes @ 1..60 => format!("Updated {minutes}m ago"),
                    minutes => format!("Updated {}h ago", minutes / 60),
                }
            }
            _ => "Rate-limit details".into(),
        };
        let windows = state.snapshot.iter().flat_map(|s| &s.windows);
        let has_windows = state
            .snapshot
            .as_ref()
            .is_some_and(|s| !s.windows.is_empty());
        div()
            .id("usage-details-content")
            .role(Role::Dialog)
            .aria_label(format!("{} usage details", self.kind.label()))
            .track_focus(&self.details)
            .key_context("UsageDetails")
            .on_action(cx.listener(|this, _: &DismissUsage, window, cx| {
                this.dismiss(window, cx);
            }))
            .flex()
            .flex_col()
            .text_size(px(11.))
            .text_color(content(1.))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(10.))
                    .px_1()
                    .pt(px(2.))
                    .pb(px(10.))
                    .child(
                        div()
                            .size_7()
                            .flex_shrink_0()
                            .rounded_lg()
                            .bg(content(0.06))
                            .border_1()
                            .border_color(content(0.07))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(provider_icon(self.kind.id(), 16.)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .line_height(px(16.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(format!("{} usage", self.kind.label())),
                            )
                            .child(
                                div()
                                    .mt(px(2.))
                                    .text_size(px(10.))
                                    .line_height(px(16.))
                                    .text_color(content(0.4))
                                    .child(updated),
                            )
                            .when(monocode_core::accounts::supports_accounts(self.kind), |d| {
                                d.child(self.account_line(cx))
                            }),
                    )
                    .when(state.fetching, |d| {
                        d.child(
                            div()
                                .mt(px(2.))
                                .flex()
                                .items_center()
                                .gap_1()
                                .text_size(px(10.))
                                .text_color(content(0.4))
                                .child(icon("refresh", 10.).text_color(content(0.4)))
                                .child("Updating"),
                        )
                    }),
            )
            .when(
                state.error.is_some() && (has_windows || self.has_banked_resets()),
                |d| {
                    d.child(
                        div()
                            .mb_2()
                            .rounded_lg()
                            .px(px(10.))
                            .py_2()
                            .bg(gpui::rgba(0xfbbf241a))
                            .text_size(px(10.))
                            .line_height(px(16.))
                            .text_color(rgb(0xfcd34d))
                            .child("Couldn’t refresh. Showing the last available snapshot."),
                    )
                },
            )
            .when(!has_windows && !self.has_banked_resets(), |d| {
                let loading = state.fetching || (state.snapshot.is_none() && state.error.is_none());
                d.child(
                    div()
                        .rounded_lg()
                        .bg(content(0.04))
                        .border_1()
                        .border_color(content(0.06))
                        .px_3()
                        .py_4()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(content(0.65))
                                .child(if loading {
                                    "Loading usage…"
                                } else if state.error.as_ref().is_some_and(|e| e.unavailable) {
                                    "Not connected"
                                } else {
                                    "Usage unavailable"
                                }),
                        )
                        .children(state.error.as_ref().map(|error| {
                            div()
                                .mt_1()
                                .max_w(px(240.))
                                .text_center()
                                .text_size(px(10.))
                                .line_height(px(16.))
                                .text_color(content(0.4))
                                .child(error.message.clone())
                        })),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .children(windows.map(|w| self.window_card(w, now))),
            )
            .children(
                state
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.extra_usage_balance)
                    .map(|balance| {
                        div()
                            .mt(px(6.))
                            .rounded_lg()
                            .bg(content(0.045))
                            .border_1()
                            .border_color(content(0.06))
                            .px_3()
                            .py(px(10.))
                            .flex()
                            .justify_between()
                            .items_baseline()
                            .gap_3()
                            .child(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(content(0.65))
                                    .child("Extra usage balance"),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(format!("${balance:.2}")),
                            )
                    }),
            )
            .children(self.suggestion(cx))
            .children(self.banked_resets(cx))
            .into_any()
    }
    /// The active account under the title: "Default account Pro · email ›".
    fn account_line(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let kind = self.kind;
        let id = self.key.profile.id.clone();
        let center = crate::accounts::get(cx);
        let label = center
            .as_ref()
            .and_then(|center| {
                center
                    .read(cx)
                    .book
                    .list(kind)
                    .into_iter()
                    .find(|a| a.id == id)
            })
            .map(|a| a.label)
            .unwrap_or_else(|| "Removed account".into());
        let subtitle = center
            .and_then(|center| center.update(cx, |center, cx| center.subtitle(kind, &id, cx)));
        div()
            .id("usage-switch-account")
            .role(Role::Button)
            .aria_label(format!("Switch {} account", kind.label()))
            .mt_1()
            .ml(px(-4.))
            .max_w_full()
            .px_1()
            .py(px(2.))
            .rounded_sm()
            .flex()
            .items_center()
            .gap_1()
            .text_size(px(10.))
            .text_color(content(0.55))
            .cursor_pointer()
            .hover(|s| s.bg(content(0.1)))
            .child(
                div()
                    .max_w(gpui::relative(0.6))
                    .flex_shrink_0()
                    .truncate()
                    .child(label),
            )
            .children(subtitle.map(|d| d.text_color(content(0.35))))
            .child(icon("chevron-right", 10.).text_color(content(0.55)))
            .on_click(cx.listener(|this, _, window, cx| this.open_accounts(window, cx)))
            .into_any_element()
    }
    fn open_accounts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_reset = None;
        self.show_accounts = true;
        if let Some(center) = crate::accounts::get(cx) {
            center.update(cx, |center, cx| {
                center.open_picker(
                    self.kind,
                    &self.key.profile.id,
                    &self.project,
                    self.session,
                    window,
                    cx,
                )
            });
        }
        cx.notify();
    }
    fn window_card(&self, w: &usage::Window, now: i64) -> gpui::Div {
        let used = w.used_percent.clamp(0., 100.);
        let shown = self.display_percent(used);
        let label = match w.minutes {
            1_440 => "Daily limit",
            10_080 => "Weekly limit",
            43_200 => "Monthly limit",
            _ => "5-hour limit",
        };
        div()
            .rounded_lg()
            .bg(content(0.045))
            .border_1()
            .border_color(content(0.06))
            .px_3()
            .py(px(10.))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_baseline()
                    .gap_3()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(content(0.65))
                            .child(label),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .font_weight(FontWeight::MEDIUM)
                            .child(format!(
                                "{shown:.0}% {}",
                                if self.show_remaining {
                                    "remaining"
                                } else {
                                    "used"
                                }
                            )),
                    ),
            )
            .child(
                div()
                    .mt_2()
                    .h(px(6.))
                    .w_full()
                    .rounded_full()
                    .bg(content(0.1))
                    .overflow_hidden()
                    .child(
                        div()
                            .h_full()
                            .w(gpui::relative(shown as f32 / 100.))
                            .rounded_full()
                            .bg(usage_color(used)),
                    ),
            )
            .child(
                div()
                    .mt(px(6.))
                    .flex()
                    .justify_between()
                    .gap_3()
                    .text_size(px(10.))
                    .line_height(px(16.))
                    .text_color(content(0.4))
                    .child(format!(
                        "{:.0}% {}",
                        100. - shown,
                        if self.show_remaining {
                            "used"
                        } else {
                            "remaining"
                        }
                    ))
                    .child(div().truncate().child(w.reset_label(now))),
            )
    }
    fn suggestion(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let center = crate::accounts::get(cx)?;
        let account = center
            .read(cx)
            .suggestion(self.kind, &self.key.profile.id)?;
        let kind = self.kind;
        let project = self.project.clone();
        let session = self.session;
        let (status, exhausted) = {
            let center = center.read(cx);
            (
                center.status(kind, &account.id),
                center.status(kind, &self.key.profile.id).tone == crate::accounts::Tone::Exhausted,
            )
        };
        Some(
            div()
                .mt_2()
                .rounded_lg()
                .bg(content(0.045))
                .border_1()
                .border_color(content(0.06))
                .px_3()
                .py(px(10.))
                .flex()
                .items_center()
                .gap(px(10.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_size(px(10.))
                                .line_height(px(16.))
                                .text_color(content(0.45))
                                .child(format!(
                                    "{} · switch to",
                                    if exhausted {
                                        "Out of usage"
                                    } else {
                                        "Running low"
                                    }
                                )),
                        )
                        .child(
                            div()
                                .mt(px(2.))
                                .flex()
                                .items_center()
                                .gap_2()
                                .min_w_0()
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(account.label.clone()),
                                )
                                .child(
                                    crate::accounts::ui::status_label(&status).text_size(px(10.)),
                                ),
                        ),
                )
                .child(
                    crate::settings::widgets::primary_button(
                        "switch-suggested-account",
                        "Switch account",
                        false,
                    )
                    .h_7()
                    .flex_shrink_0()
                    .px(px(10.))
                    .rounded_md()
                    .text_size(px(11.))
                    .child("Switch")
                    .on_click(cx.listener(move |_, _, _, cx| {
                        center.update(cx, |center, cx| {
                            center.choose(kind, account.id.clone(), project.clone(), session, cx)
                        });
                    })),
                )
                .into_any_element(),
        )
    }
}
use crate::accounts::ui::bar_color as usage_color;
impl Render for UsageFooter {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = &self.cache[index(self.kind)];
        let now = usage::now_ms();
        let label = if let Some(snapshot) = &state.snapshot {
            snapshot
                .windows
                .iter()
                .map(|w| {
                    format!(
                        "{:.0}% {}",
                        self.display_percent(w.used_percent),
                        w.countdown(now)
                    )
                })
                .collect::<Vec<_>>()
                .join(" · ")
        } else if state.fetching || state.error.is_none() {
            "···".into()
        } else if state.error.as_ref().is_some_and(|e| e.unavailable) {
            "not connected".into()
        } else {
            "usage unavailable".into()
        };
        let description = state
            .snapshot
            .as_ref()
            .map(|s| {
                s.windows
                    .iter()
                    .map(|w| {
                        format!(
                            "{} · {}",
                            self.percent_label(w.used_percent),
                            w.reset_label(now)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" · ")
            })
            .or_else(|| state.error.as_ref().map(|e| e.message.clone()))
            .unwrap_or_else(|| "Loading usage".into());
        let fetching = state.fetching;
        let tightest = state
            .snapshot
            .as_ref()
            .and_then(|s| s.windows.iter().map(|w| w.used_percent).reduce(f64::max));
        let weak = cx.weak_entity();
        let classic = self.classic;
        div()
            .id("usage-footer")
            .role(Role::Group)
            .aria_label("Provider usage")
            .h(px(if classic { 28. } else { 30. }))
            .w_full()
            .flex_shrink_0()
            .when(!classic, |d| d.pb(px(2.)))
            .flex()
            .items_center()
            .gap(px(6.))
            .text_size(px(11.))
            .text_color(content(0.55))
            .child(
                div()
                    .id("usage-provider-chip")
                    .role(Role::Button)
                    .aria_label(format!("{} usage details", self.kind.label()))
                    .aria_description(description)
                    .track_focus(&self.trigger)
                    .key_context("UsageChip")
                    .tab_index(0)
                    .relative()
                    .h(px(20.))
                    .map(|d| {
                        if classic {
                            d.mx(px(-4.)).px(px(4.)).rounded_md()
                        } else {
                            d.px(px(8.)).rounded_full()
                        }
                    })
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .hover(|s| s.bg(content(0.1)).text_color(content(1.)))
                    .child(self.anchor.probe())
                    .child(provider_icon(self.kind.id(), 12.))
                    .when_some(tightest, |d, percent| {
                        d.child(
                            div()
                                .w(px(32.))
                                .h(px(4.))
                                .rounded_full()
                                .bg(content(0.1))
                                .overflow_hidden()
                                .child(
                                    div()
                                        .h_full()
                                        .w(gpui::relative(percent as f32 / 100.))
                                        .rounded_full()
                                        .bg(usage_color(percent)),
                                ),
                        )
                    })
                    .child(label)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle(window, cx)))
                    .on_action(
                        cx.listener(|this, _: &ToggleUsage, window, cx| this.toggle(window, cx)),
                    ),
            )
            .child(
                div()
                    .id("refresh-usage")
                    .role(Role::Button)
                    .aria_label("Refresh usage")
                    .track_focus(&self.refresh_focus)
                    .tab_index(0)
                    .key_context("UsageRefresh")
                    .map(|d| {
                        if classic {
                            d.size(px(18.)).rounded_md()
                        } else {
                            d.size(px(20.)).ml(px(-4.)).rounded_full()
                        }
                    })
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .opacity(if fetching { 0.5 } else { 1. })
                    .when(!fetching, |d| d.hover(|s| s.bg(content(0.1))))
                    .child(icon("refresh", 12.))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(true, cx)))
                    .on_action(cx.listener(|this, _: &RefreshUsage, _, cx| this.refresh(true, cx))),
            )
            .when(monocode_core::api_keys::supports(self.kind), |d| {
                d.child(self.api_key_button(cx))
            })
            .child(
                div()
                    .id("mcp-settings-button")
                    .debug_selector(|| "mcp-settings-button".into())
                    .role(Role::Button)
                    .aria_label("MCP server status")
                    .h(px(20.))
                    .px(px(6.))
                    .rounded_md()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .text_size(px(10.))
                    .text_color(content(0.5))
                    .cursor_pointer()
                    .hover(|s| s.bg(content(0.08)))
                    .child(if self.mcp_cancel.is_some() {
                        "MCP · Connecting"
                    } else {
                        "MCP"
                    })
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(UsageFooterEvent::Mcp))),
            )
            .when(self.mcp_open, |d| {
                let weak = cx.entity().downgrade();
                d.child(
                    popover("mcp-status-details", Anchor::Slot(self.anchor.clone()))
                        .side(Side::Top)
                        .gap(7.)
                        .width(360.)
                        .max_height(420.)
                        .padding(12.)
                        .on_dismiss(move |_, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.mcp_open = false;
                                cx.notify();
                            });
                        })
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("MCP servers for this project"),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(content(0.5))
                                        .child(self.mcp_note.clone()),
                                )
                                .child(
                                    div()
                                        .id("mcp-server-list")
                                        .max_h(px(260.))
                                        .overflow_y_scroll()
                                        .children(self.mcp_servers.iter().map(|(name, status)| {
                                            let color = if status == "connected" {
                                                0x34d399
                                            } else if status == "disabled" {
                                                0x94a3b8
                                            } else if status.contains("failed") {
                                                0xf87171
                                            } else {
                                                0xfbbf24
                                            };
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                .py_1()
                                                .child(
                                                    div()
                                                        .size(px(6.))
                                                        .rounded_full()
                                                        .bg(rgb(color)),
                                                )
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .text_size(px(12.))
                                                        .child(name.clone()),
                                                )
                                                .child(
                                                    div()
                                                        .max_w(px(180.))
                                                        .text_size(px(11.))
                                                        .text_color(content(0.6))
                                                        .child(status.clone()),
                                                )
                                        })),
                                )
                                .child(
                                    div()
                                        .id("check-project-mcp")
                                        .role(Role::Button)
                                        .aria_label("Check project MCP connections")
                                        .text_size(px(12.))
                                        .py_1()
                                        .cursor_pointer()
                                        .child("Check connections ↻")
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.refresh_mcp(cx)),
                                        ),
                                )
                                .child(
                                    div()
                                        .id("manage-mcp")
                                        .role(Role::Button)
                                        .aria_label("Manage MCP servers")
                                        .text_size(px(12.))
                                        .py_2()
                                        .cursor_pointer()
                                        .child("Manage MCP servers →")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.mcp_open = false;
                                            cx.emit(UsageFooterEvent::ManageMcp);
                                            cx.notify();
                                        })),
                                ),
                        ),
                )
            })
            .when(self.open, |d| {
                d.child(
                    popover("usage-details", Anchor::Slot(self.anchor.clone()))
                        .side(Side::Top)
                        .gap(7.)
                        .width(if self.show_accounts { 340. } else { 300. })
                        .max_height(460.)
                        .padding(if !self.show_accounts && self.login_view(cx) {
                            0.
                        } else {
                            10.
                        })
                        .on_dismiss(move |window, cx| {
                            let _ = weak.update(cx, |this, cx| this.dismiss(window, cx));
                        })
                        .child(self.details(cx)),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};

    /// A check launches provider processes that connect to every MCP
    /// server, so it runs only while the panel is open; a project change
    /// cancels the previous result either way.
    #[gpui::test]
    fn connection_checks_run_only_while_the_panel_is_open_and_a_project_change_cancels_them(
        cx: &mut gpui::TestAppContext,
    ) {
        let first = std::env::temp_dir().join("mono-mcp-context-test-first");
        let second = std::env::temp_dir().join("mono-mcp-context-test-second");
        let view = cx.new(|cx| UsageFooter::new(Kind::Claude, cx));
        view.update(cx, |footer, cx| {
            footer.context(
                Kind::Claude,
                Default::default(),
                None,
                first.display().to_string(),
                Some(1),
                cx,
            );
            assert!(footer.mcp_cancel.is_none(), "closed panel: no check");
            assert!(footer.mcp_note.contains("Open this panel"));
            footer.show_mcp(cx);
            let cancel = footer.mcp_cancel.clone().expect("opening the panel checks");
            let request = footer.mcp_request;
            footer.context(
                Kind::Claude,
                Default::default(),
                None,
                second.display().to_string(),
                Some(2),
                cx,
            );
            assert!(cancel.load(std::sync::atomic::Ordering::Relaxed));
            assert!(footer.mcp_request > request);
            assert!(footer.mcp_note.contains(&second.display().to_string()));
            assert!(footer.mcp_servers.is_empty());
            assert!(!footer.mcp_open);
            assert!(
                footer.mcp_cancel.is_none(),
                "the panel closed: no new check"
            );
            footer.show_mcp(cx);
            assert!(footer.mcp_cancel.is_some());
        });
    }

    #[gpui::test]
    fn api_key_toggle_needs_a_saved_key_and_reports_the_change(cx: &mut gpui::TestAppContext) {
        use monocode_core::api_keys;
        cx.update(bind);
        api_keys::init(
            std::env::temp_dir().join(format!("monocode-api-keys-{}", std::process::id())),
        );
        api_keys::clear(Kind::Codex).unwrap();
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |_, cx| {
                cx.new(|cx| UsageFooter::new(Kind::Codex, cx))
            })
            .unwrap()
        });
        let events = Rc::new(RefCell::new(Vec::new()));
        let sink = events.clone();
        window
            .update(cx, |_, _, cx| {
                let footer = cx.entity();
                cx.subscribe(&footer, move |_, _, event: &UsageFooterEvent, _| {
                    sink.borrow_mut().push(event.clone());
                })
                .detach();
            })
            .unwrap();
        let visual = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        visual.refresh().unwrap();
        let toggle = visual.debug_bounds("api-key-toggle").unwrap();
        visual.simulate_click(toggle.center(), Default::default());
        visual.run_until_parked();
        assert!(
            events.borrow().is_empty(),
            "no key saved, so nothing switches on"
        );
        assert!(
            visual.debug_bounds("api-key-note").is_some(),
            "the refusal explains where to add a key"
        );

        api_keys::set(Kind::Codex, "sk-test-1234").unwrap();
        let toggle = visual.debug_bounds("api-key-toggle").unwrap();
        visual.simulate_click(toggle.center(), Default::default());
        visual.run_until_parked();
        assert_eq!(
            events.borrow().as_slice(),
            [UsageFooterEvent::ApiKey {
                kind: Kind::Codex,
                on: true
            }]
        );
        assert!(visual.debug_bounds("api-key-note").is_none());

        window
            .update(visual, |footer, _, cx| {
                footer.set_api_keys([("codex".to_owned(), true)].into_iter().collect(), cx);
            })
            .unwrap();
        visual.refresh().unwrap();
        let toggle = visual.debug_bounds("api-key-toggle").unwrap();
        visual.simulate_click(toggle.center(), Default::default());
        visual.run_until_parked();
        assert_eq!(
            events.borrow().last(),
            Some(&UsageFooterEvent::ApiKey {
                kind: Kind::Codex,
                on: false
            })
        );

        window
            .update(visual, |footer, _, cx| footer.provider(Kind::Pi, cx))
            .unwrap();
        visual.refresh().unwrap();
        assert!(
            visual.debug_bounds("api-key-toggle").is_none(),
            "Pi has no API key billing"
        );
        api_keys::clear(Kind::Codex).unwrap();
    }

    #[gpui::test]
    fn details_open_with_keyboard_escape_restores_focus_and_provider_caches_stay_separate(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(bind);
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |_, cx| {
                cx.new(|cx| UsageFooter::new(Kind::Codex, cx))
            })
            .unwrap()
        });
        window
            .update(cx, |footer, w, cx| {
                footer.cache[0].complete(Ok(Snapshot {
                    windows: vec![usage::Window {
                        used_percent: 28.,
                        minutes: 300,
                        resets_at: None,
                    }],
                    updated_at: usage::now_ms(),
                    ..Default::default()
                }));
                w.focus(&footer.trigger, cx);
            })
            .unwrap();
        cx.refresh().unwrap();
        cx.simulate_keystrokes(window.into(), "enter");
        cx.run_until_parked();
        window
            .update(cx, |footer, w, cx| {
                assert!(footer.open);
                assert!(footer.details.is_focused(w));
                cx.notify();
            })
            .unwrap();
        cx.simulate_keystrokes(window.into(), "escape");
        cx.run_until_parked();
        window
            .update(cx, |footer, w, cx| {
                assert!(!footer.open);
                assert!(footer.trigger.is_focused(w));
                footer.provider(Kind::Claude, cx);
                assert!(footer.cache[index(footer.kind)].snapshot.is_none());
                // A late Codex response belongs to Codex, while Claude stays in view.
                footer.cache[0].complete(Ok(Snapshot {
                    windows: vec![usage::Window {
                        used_percent: 42.,
                        minutes: 300,
                        resets_at: None,
                    }],
                    updated_at: usage::now_ms(),
                    ..Default::default()
                }));
                assert!(footer.cache[index(footer.kind)].snapshot.is_none());
                footer.provider(Kind::Codex, cx);
                assert_eq!(
                    footer.cache[0].snapshot.as_ref().unwrap().windows[0].used_percent,
                    42.
                );
            })
            .unwrap();
    }
    #[test]
    fn failed_refresh_keeps_snapshot_but_sign_out_clears_it() {
        let mut state = CachedUsage {
            snapshot: Some(Snapshot::default()),
            fetching: true,
            ..Default::default()
        };
        state.complete(Err(FetchError {
            message: "Offline".into(),
            unavailable: false,
        }));
        assert!(state.snapshot.is_some());
        assert!(!state.fetching);
        state.complete(Err(FetchError {
            message: "Signed out".into(),
            unavailable: true,
        }));
        assert!(state.snapshot.is_none());
        state.complete(Ok(Snapshot::default()));
        assert!(state.error.is_none());
    }
}
