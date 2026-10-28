use super::*;
use crate::conversation::task_monitor::LiveWorker;
use monocode_core::agent::Settings;
pub(crate) struct MonoSession {
    pub id: i64,
    pub title: String,
    pub cwd: String,
    pub busy: bool,
    pub has_messages: bool,
    pub archived: bool,
    pub updated_at: u64,
    pub needs_input: bool,
    pub branch: Option<String>,
    pub model: SharedString,
    /// The tool its current turn runs, while it runs.
    pub activity: Option<String>,
    /// When its current turn started, while it runs.
    pub started_at: Option<u64>,
}
impl From<&MonoSession> for LiveWorker {
    fn from(session: &MonoSession) -> Self {
        Self {
            busy: session.busy,
            needs_input: session.needs_input,
            updated_at: session.updated_at,
            activity: session.activity.clone(),
            started_at: session.started_at,
        }
    }
}
impl Shell {
    /// Reuse the normal chat cards, including their provider, approval and
    /// activity indicators. These are references to saved workers, not copies.
    pub(super) fn mono_worker_groups(
        &self,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> HashMap<String, AnyElement> {
        if compact {
            return HashMap::new();
        }
        let Some(manager) = self.monos.clone() else {
            return HashMap::new();
        };
        let monos = manager.read(cx);
        let groups: Vec<_> = monos
            .roster
            .monos
            .iter()
            .map(|mono| {
                (
                    mono.id.clone(),
                    monos.collapsed_workers.contains(&mono.id),
                    mono.launches
                        .iter()
                        .map(|launch| launch.session_id)
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        let mut result = HashMap::new();
        for (mono, collapsed, ids) in groups {
            let mut seen = HashSet::new();
            let mut ids: Vec<_> = ids
                .into_iter()
                .filter(|id| {
                    seen.insert(*id)
                        && self.session_summaries.contains_key(id)
                        && !self.session_metadata(*id, cx).summary.archived
                })
                .collect();
            ids.sort_by_key(|id| {
                let state = self.session_metadata(*id, cx);
                (
                    !state.needs_approval(),
                    !state.busy,
                    std::cmp::Reverse(state.summary.updated_at),
                )
            });
            if ids.is_empty() {
                continue;
            }
            let running = ids
                .iter()
                .filter(|id| self.session_metadata(**id, cx).busy)
                .count();
            let count = ids.len();
            let toggle = mono.clone();
            let selector = format!("mono-workers-{mono}");
            let header = div()
                .id(SharedString::from(selector.clone()))
                .debug_selector(move || selector.clone())
                .role(Role::Button)
                .aria_label(format!("Agent sessions, {count}"))
                .h(px(28.))
                .px_2()
                .rounded_md()
                .flex()
                .items_center()
                .gap_1p5()
                .text_size(px(11.))
                .text_color(content(0.5))
                .hover(|s| s.bg(content(0.05)).text_color(content(0.8)))
                .child(icon(
                    if collapsed {
                        "chevron-right"
                    } else {
                        "chevron-down"
                    },
                    12.,
                ))
                .child(div().flex_1().child("Agent sessions"))
                .child(if running > 0 {
                    format!("{running} running")
                } else {
                    count.to_string()
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(manager) = this.monos.clone() {
                        manager.update(cx, |monos, cx| {
                            if !monos.collapsed_workers.remove(&toggle) {
                                monos.collapsed_workers.insert(toggle.clone());
                            }
                            cx.notify();
                        });
                    }
                    this.notify_sidebar(cx);
                }));
            let mut group = div()
                .ml(px(8.))
                .mb_2()
                .border_l_1()
                .border_color(content(0.08))
                .pl_1()
                .child(header);
            if !collapsed {
                for id in ids {
                    group = group.child(self.session_card(id, cx));
                }
            }
            result.insert(mono, group.into_any_element());
        }
        result
    }
    pub(crate) fn mono_install_app(
        &mut self,
        summary: Session,
        settings: Settings,
        draft: bool,
        cx: &mut Context<Self>,
    ) {
        let id = summary.id;
        self.draft_sessions.insert(id);
        self.session_worktrees
            .insert(id, settings.worktree_cwd.clone());
        self.add_loaded(summary, cx);
        self.sessions[&id].update(cx, |view, cx| view.install_app_launch(settings, !draft, cx));
        self.invalidate_chrome(cx);
    }
    pub(crate) fn close_mono_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_right_panel_open(false, window, cx);
    }
    /// As a restart would: the store's session list omits hidden launches.
    #[cfg(test)]
    pub(crate) fn mono_forget_session(&mut self, id: i64) {
        self.order.retain(|s| *s != id);
        self.session_summaries.remove(&id);
        self.sessions.remove(&id);
    }
    #[cfg(test)]
    pub(crate) fn mono_manager(&self) -> Entity<crate::monos::Monos> {
        self.monos.as_ref().unwrap().clone()
    }
    #[cfg(all(target_os = "macos", not(test)))]
    pub(crate) fn mono_quick_compose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_quick_composer(window, cx);
    }
    pub(super) fn mono_active(&self, cx: &App) -> bool {
        self.monos
            .as_ref()
            .is_some_and(|m| m.read(cx).active.is_some())
    }
    pub(crate) fn mono_session_views(&self, cx: &App) -> Vec<MonoSession> {
        // A Mono's launches are usually hidden from the sidebar, so they are
        // known by summary only, outside the sidebar's order.
        let listed: HashSet<_> = self.order.iter().collect();
        let hidden = self
            .session_summaries
            .keys()
            .filter(|id| !listed.contains(id));
        self.order
            .iter()
            .chain(hidden)
            .map(|&id| self.mono_session_view(id, cx))
            .collect()
    }
    fn mono_session_view(&self, id: i64, cx: &App) -> MonoSession {
        let metadata = self.session_metadata(id, cx);
        let live = self.sessions.get(&id).map(|v| v.read(cx));
        let (activity, started_at) = live
            .filter(|v| v.busy)
            .map(|v| {
                let (tool, started_at, _) = v.transcript.read(cx).live_turn(&v.summary.cwd);
                (tool, started_at)
            })
            .unwrap_or((None, None));
        MonoSession {
            activity,
            started_at,
            id,
            title: metadata.summary.title.clone(),
            cwd: metadata.summary.cwd.clone(),
            busy: metadata.busy || live.is_some_and(|v| v.mono_external_pending()),
            has_messages: metadata.summary.has_messages,
            archived: metadata.summary.archived,
            updated_at: metadata.summary.updated_at,
            needs_input: metadata.needs_approval(),
            branch: live.and_then(|v| v.branch_name()).map(str::to_owned),
            model: live.map(|v| v.mono_model_label()).unwrap_or_default(),
        }
    }
    /// The store no longer has this session a Mono launched (checked by
    /// `mono_track_sessions`), so nothing will ever run in it.
    pub(crate) fn mono_session_missing(&self, id: i64) -> bool {
        self.mono_missing_sessions.contains(&id)
    }
    /// Routes again every worker question a storage error held back.
    pub(crate) fn retry_task_questions(&mut self, cx: &mut Context<Self>) {
        for view in self.sessions.values() {
            view.update(cx, |view, cx| view.retry_task_questions(cx));
        }
    }
    /// A session changed: a Mono's task card that shows it as a worker
    /// updates now (busy, waiting on input, last activity).
    pub(super) fn task_worker_changed(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(monos) = self.monos.clone() else {
            return;
        };
        if !self.sessions.contains_key(&id) && !self.session_summaries.contains_key(&id) {
            return;
        }
        let live = LiveWorker::from(&self.mono_session_view(id, cx));
        monos.update(cx, |monos, cx| monos.task_worker_changed(id, live, cx));
    }
    /// An assignment's worktree changes, from a Mono's task card: the
    /// Changes review of that checkout.
    pub(crate) fn mono_open_changes(
        &mut self,
        cwd: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_review(
            crate::review::ReviewTab::Changes {
                cwd,
                path: None,
                staged: None,
            },
            window,
            cx,
        );
    }
    /// Load the sessions a Mono launched that the sidebar hides, so its panel
    /// can list and open them. They stay out of the sidebar.
    pub(crate) fn mono_track_sessions(&mut self, ids: Vec<i64>, cx: &mut Context<Self>) {
        let missing: Vec<_> = ids
            .into_iter()
            .filter(|id| !self.session_summaries.contains_key(id) && !self.deleting.contains(id))
            .collect();
        if missing.is_empty() {
            return;
        }
        let store = self.store.clone();
        let work = cx.background_executor().spawn(async move {
            let store = locked(&store)?;
            let mut found = Vec::new();
            let mut gone = Vec::new();
            for id in missing {
                match store.mono_session(id) {
                    Ok(Some(summary)) => found.push(summary),
                    Ok(None) => gone.push(id),
                    Err(_) => {}
                }
            }
            Ok::<_, String>((found, gone))
        });
        cx.spawn(async move |this, cx| {
            let Ok((found, gone)) = work.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.mono_missing_sessions.extend(gone);
                for summary in found {
                    this.mono_missing_sessions.remove(&summary.id);
                    if this.session_summaries.contains_key(&summary.id) {
                        continue;
                    }
                    this.sidebar_hidden_sessions.insert(summary.id);
                    this.session_summaries.insert(summary.id, summary);
                }
                this.notify_sidebar(cx);
                if let Some(monos) = &this.monos {
                    monos.update(cx, |_, cx| cx.notify());
                }
            });
        })
        .detach();
    }
    pub(crate) fn mono_session_entity(&self, id: i64) -> Option<Entity<Conversation>> {
        self.sessions.get(&id).cloned()
    }
    pub(crate) fn mono_ensure_session(
        &mut self,
        id: i64,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Conversation>> {
        self.ensure_session(id, cx)
    }
    pub(crate) fn mono_groups(&self, project: &str) -> monocode_core::session_groups::Groups {
        self.groups.get(project).cloned().unwrap_or_default()
    }
    pub(crate) fn mono_apply_groups(
        &mut self,
        project: String,
        groups: monocode_core::session_groups::Groups,
        cx: &mut Context<Self>,
    ) {
        self.groups.insert(project, groups);
        self.invalidate_chrome(cx);
    }
    pub(crate) fn mono_remove(
        &mut self,
        id: i64,
        delete: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if delete {
            self.delete_session(id, window, cx);
        } else if self.session_summaries.contains_key(&id)
            && !self.session_metadata(id, cx).summary.archived
        {
            self.toggle_archive(id, cx);
        }
    }
}
