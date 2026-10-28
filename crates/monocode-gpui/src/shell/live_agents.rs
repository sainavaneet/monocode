//! LiveAgentsPreview.tsx: the sidebar's card of working and unseen finished
//! sessions, across every project.
use super::*;
use gpui::AnimationExt;

const LIVE_AGENT_MIN: usize = 1;
const LIVE_AGENT_CAP: usize = 4;
// Tailwind amber-400 and emerald-400.
const AMBER: u32 = 0xfbbf24;
const EMERALD: u32 = 0x34d399;

struct LiveAgent {
    id: i64,
    /// Present on the Mono lead itself.
    mono_id: Option<String>,
    /// Present on an orchestrated worker, so it renders beneath its lead.
    parent_mono_id: Option<String>,
    task_title: Option<String>,
    working: bool,
    cwd: String,
    provider: String,
    title: String,
    activity: String,
    started_at: Option<u64>,
    duration_ms: Option<u64>,
    needs_approval: bool,
    done: bool,
}

fn activity_order(
    agent: &LiveAgent,
    monos: &HashMap<String, usize>,
) -> (usize, u8, bool, bool, u64) {
    let group = agent
        .mono_id
        .as_ref()
        .or(agent.parent_mono_id.as_ref())
        .and_then(|mono| monos.get(mono))
        .copied()
        .unwrap_or(usize::MAX);
    let level = if agent.mono_id.is_some() {
        0
    } else if agent.parent_mono_id.is_some() {
        1
    } else {
        2
    };
    (
        group,
        level,
        !agent.needs_approval,
        agent.done,
        agent.started_at.unwrap_or(u64::MAX),
    )
}

/// liveAgents.ts's formatLiveElapsed.
fn live_elapsed(ms: u64) -> String {
    let seconds = ((ms + 500) / 1000).max(1);
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let (minutes, rest) = (seconds / 60, seconds % 60);
    if minutes < 60 {
        return if rest > 0 {
            format!("{minutes}m {rest}s")
        } else {
            format!("{minutes}m")
        };
    }
    let (hours, rest) = (minutes / 60, minutes % 60);
    if rest > 0 {
        format!("{hours}h {rest}m")
    } else {
        format!("{hours}h")
    }
}

impl Shell {
    /// Activity is global: enter the Mono's project before opening its tab.
    /// Right-panel worker navigation intentionally stays in the current project.
    pub(crate) fn open_activity_mono(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(manager) = self.monos.clone() else {
            return;
        };
        let project = {
            let roster = manager.read(cx);
            let Some(mono) = roster.roster.get(id) else {
                return;
            };
            mono.projects
                .iter()
                .find(|p| **p == self.workspace.project)
                .or_else(|| mono.projects.first())
                .cloned()
        };
        if let Some(project) = project.filter(|p| *p != self.workspace.project) {
            self.select_project(project, window, cx);
        }
        manager.update(cx, |manager, cx| manager.open_home(id, window, cx));
        self.invalidate_chrome(cx);
        cx.notify();
    }

    fn live_agents(&self, cx: &App) -> Vec<LiveAgent> {
        let mut agents: Vec<_> = self
            .session_summaries
            .keys()
            .filter_map(|id| {
                let view = self.sessions.get(id)?.read(cx);
                let done = !view.busy && self.unseen_finished.contains(id);
                if view.summary.archived || !(view.responding() || done) {
                    return None;
                }
                let (tool, started_at, duration_ms) =
                    view.transcript.read(cx).live_turn(&view.summary.cwd);
                let needs_approval = view.needs_approval();
                let owner = self
                    .monos
                    .as_ref()
                    .and_then(|manager| manager.read(cx).task_worker_owner(*id));
                Some(LiveAgent {
                    id: *id,
                    mono_id: None,
                    parent_mono_id: owner.as_ref().map(|(mono, _)| mono.clone()),
                    task_title: owner.map(|(_, task)| task),
                    working: view.responding(),
                    cwd: view.summary.cwd.clone(),
                    provider: view.summary.provider.clone(),
                    title: view.display_title(),
                    activity: if needs_approval {
                        "Need approval".into()
                    } else if done {
                        "Done".into()
                    } else {
                        tool.unwrap_or_else(|| "Working".into())
                    },
                    started_at,
                    duration_ms: duration_ms.filter(|_| done),
                    needs_approval,
                    done,
                })
            })
            .collect();
        if let Some(manager) = &self.monos {
            let manager = manager.read(cx);
            for mono in &manager.roster.monos {
                let Some(chat) = manager.open_chat(&mono.id) else {
                    continue;
                };
                let view = chat.read(cx);
                let activity = view.mono_activity();
                if activity.state == "idle" {
                    continue;
                }
                let (_, started_at, _) = view.transcript.read(cx).live_turn(&view.summary.cwd);
                agents.push(LiveAgent {
                    id: view.summary.id,
                    mono_id: Some(mono.id.clone()),
                    parent_mono_id: None,
                    task_title: None,
                    working: view.responding(),
                    cwd: view.summary.cwd.clone(),
                    provider: view.summary.provider.clone(),
                    title: mono.label(),
                    activity: if activity.state == "working" {
                        crate::transcript::mono_work_label(
                            view.transcript.read(cx).source_rows(),
                            0,
                        )
                    } else {
                        activity.title
                    },
                    started_at: started_at.filter(|_| view.responding()),
                    duration_ms: None,
                    needs_approval: activity.state == "needs-you",
                    done: false,
                });
            }
        }
        // A Mono is the parent in Activity: lead first, then its orchestrated
        // workers. Standalone sessions follow the Mono groups.
        let mono_order: HashMap<_, _> = self
            .monos
            .as_ref()
            .map(|manager| {
                manager
                    .read(cx)
                    .roster
                    .monos
                    .iter()
                    .enumerate()
                    .map(|(index, mono)| (mono.id.clone(), index))
                    .collect()
            })
            .unwrap_or_default();
        agents.sort_by_key(|agent| activity_order(agent, &mono_order));
        agents
    }

    pub(super) fn live_agents_preview(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let agents = if self.workspace.preferences.working_agents
            && self.workspace.sidebar_activity_visible
        {
            self.live_agents(cx)
        } else {
            Vec::new()
        };
        let shown = agents.len() >= LIVE_AGENT_MIN;
        self.live_marks
            .retain(|id, _| shown && agents.iter().any(|a| a.id == *id && !a.done));
        let ticking = shown && agents.iter().any(|a| !a.done && a.started_at.is_some());
        if !ticking {
            self.live_tick = None;
        } else if self.live_tick.is_none() {
            self.live_tick = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(1))
                        .await;
                    if this.update(cx, |this, cx| this.notify_sidebar(cx)).is_err() {
                        break;
                    }
                }
            }));
        }
        if !shown {
            self.live_agents_expanded = false;
            return None;
        }
        let count = agents.len();
        let extra = count.saturating_sub(LIVE_AGENT_CAP);
        let expanded = self.live_agents_expanded;
        let now = monocode_core::time::now_ms();
        let cards: Vec<_> = agents
            .into_iter()
            .take(if expanded { count } else { LIVE_AGENT_CAP })
            .map(|agent| self.live_agent_card(agent, now, cx))
            .collect();
        Some(
            div()
                .id("live-agents")
                .debug_selector(|| "sidebar-activity-card".into())
                .role(Role::Group)
                .aria_label(format!("{count} working agents"))
                .flex_shrink_0()
                .px_2()
                .when(self.workspace.boxy_layout(), |d| d.pb_2())
                .child(
                    div()
                        .overflow_hidden()
                        .rounded_lg()
                        .border_t_1()
                        .border_color(content(0.08))
                        .when(count > 1, |d| {
                            d.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .px(px(14.))
                                    .py_1p5()
                                    .child(
                                        div()
                                            .size(px(6.))
                                            .flex_shrink_0()
                                            .rounded_full()
                                            .bg(accent_color())
                                            .shadow(vec![
                                                BoxShadow::new(px(0.), px(0.), accent_color())
                                                    .blur_radius(px(8.)),
                                            ]),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(px(12.))
                                            .text_color(content(0.5))
                                            .child("Activity"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(content(0.4))
                                            .child(count.to_string()),
                                    ),
                            )
                        })
                        .child(
                            div()
                                .id("live-agents-list")
                                .flex()
                                .flex_col()
                                .gap_px()
                                .px_1()
                                .when(extra == 0, |d| d.pb_1())
                                .when(expanded, |d| d.max_h(px(360.)).overflow_y_scroll())
                                .children(cards),
                        )
                        .when(extra > 0, |d| {
                            d.child(
                                div()
                                    .id("live-agents-more")
                                    .group("live-agents-more")
                                    .role(Role::Button)
                                    .aria_expanded(expanded)
                                    .cursor_pointer()
                                    .w_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .gap_1()
                                    .px_2()
                                    .py_1p5()
                                    .text_size(px(11.))
                                    .text_color(content(0.5))
                                    .hover(|s| s.bg(content(0.08)).text_color(content(1.)))
                                    .child(
                                        icon(
                                            if expanded {
                                                "chevron-up"
                                            } else {
                                                "chevron-down"
                                            },
                                            12.,
                                        )
                                        .text_color(content(0.5))
                                        .group_hover("live-agents-more", |s| {
                                            s.text_color(content(1.))
                                        }),
                                    )
                                    .child(if expanded {
                                        "Show less".to_owned()
                                    } else {
                                        format!("{extra} more")
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.live_agents_expanded = !this.live_agents_expanded;
                                        this.notify_sidebar(cx);
                                    })),
                            )
                        }),
                )
                .into_any(),
        )
    }

    fn live_agent_card(
        &mut self,
        agent: LiveAgent,
        now: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = agent.id;
        let mono_id = agent.mono_id.clone();
        let project = self.project_label(&agent.cwd);
        let elapsed = if agent.done {
            agent.duration_ms.map(live_elapsed)
        } else {
            agent
                .started_at
                .map(|at| live_elapsed(now.saturating_sub(at)))
        };
        if agent.parent_mono_id.is_some() {
            let task = agent
                .task_title
                .clone()
                .unwrap_or_else(|| agent.title.clone());
            let assignment = agent
                .title
                .strip_prefix(&task)
                .and_then(|title| title.strip_prefix(':'))
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .unwrap_or(&agent.activity);
            let detail = if assignment == agent.activity {
                agent.activity.clone()
            } else {
                format!("{assignment} · {}", agent.activity)
            };
            let status: AnyElement = if agent.needs_approval {
                icon("alert-circle", 12.)
                    .text_color(rgb(AMBER))
                    .into_any_element()
            } else if agent.done {
                icon("check", 12.)
                    .text_color(rgb(EMERALD))
                    .into_any_element()
            } else if agent.working && !cx.reduce_motion() {
                crate::session_mark::spinner(
                    ("orchestration-worker", id as usize),
                    crate::pulse::Rate::Half,
                )
                .into_any_element()
            } else {
                icon("clock", 12.)
                    .text_color(content(0.5))
                    .into_any_element()
            };
            let label = format!("{task}, {detail}. Open worker session");
            return div()
                .id(("live-agent", id as usize))
                .debug_selector(move || format!("live-agent-{id}"))
                .role(Role::Button)
                .aria_label(label)
                .cursor_pointer()
                .ml_3()
                .pl_2()
                .pr_2()
                .py_1()
                .border_l_1()
                .border_color(content(0.12))
                .rounded_r_md()
                .flex()
                .items_center()
                .gap_2()
                .hover(|style| style.bg(content(0.06)))
                .child(
                    div()
                        .size(px(14.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(status),
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
                                .text_size(px(11.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(content(0.8))
                                .child(task),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(10.))
                                .text_color(if agent.needs_approval {
                                    rgb(AMBER).into()
                                } else {
                                    content(0.45)
                                })
                                .child(detail),
                        ),
                )
                .children(elapsed.map(|elapsed| {
                    div()
                        .flex_shrink_0()
                        .text_size(px(10.))
                        .text_color(content(0.35))
                        .child(elapsed)
                }))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.selected_sessions.clear();
                    this.selection_anchor = Some(id);
                    this.open(id, window, cx);
                    this.invalidate_chrome(cx);
                    cx.notify();
                }))
                .into_any_element();
        }
        if let Some(mono_id) = mono_id {
            let mark = self.monos.as_ref().and_then(|manager| {
                let manager = manager.read(cx);
                manager.roster.get(&mono_id).map(|mono| {
                    crate::monos::avatar::avatar(
                        &mono.mascot,
                        mono.color,
                        if agent.needs_approval {
                            "needs-you"
                        } else if agent.working {
                            "working"
                        } else {
                            "waiting"
                        },
                        24.,
                        true,
                    )
                })
            });
            let label = format!("{}, {}. Open conversation", agent.title, agent.activity);
            let activity = if agent.working && !agent.needs_approval && !cx.reduce_motion() {
                crate::activity::status_shimmer(agent.activity.clone())
            } else {
                div().child(agent.activity).into_any_element()
            };
            let row = div()
                .id(("live-agent", id as usize))
                .debug_selector(move || format!("live-agent-{id}"))
                .role(Role::Button)
                .aria_label(label)
                .cursor_pointer()
                .w_full()
                .h(px(54.))
                .rounded_lg()
                .px_2()
                .py_2()
                .flex()
                .items_center()
                .gap_2()
                .hover(|s| s.bg(content(0.04)))
                .children(mark)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(content(0.9))
                                .truncate()
                                .child(agent.title),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(if agent.needs_approval {
                                    rgb(AMBER).into()
                                } else {
                                    content(0.5)
                                })
                                .truncate()
                                .child(activity),
                        ),
                )
                .children(elapsed.map(|elapsed| {
                    div()
                        .min_w(px(38.))
                        .flex_shrink_0()
                        .text_right()
                        .text_size(px(10.))
                        .text_color(content(0.35))
                        .child(elapsed)
                }))
                .child(
                    icon(
                        if agent.needs_approval {
                            "alert-circle"
                        } else {
                            "chevron-right"
                        },
                        12.,
                    )
                    .text_color(if agent.needs_approval {
                        rgb(AMBER).into()
                    } else {
                        content(0.3)
                    }),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_activity_mono(&mono_id, window, cx);
                }));
            return if cx.reduce_motion() {
                row.into_any_element()
            } else {
                row.with_animation(
                    ("mono-status-enter", id as usize),
                    gpui::Animation::new(std::time::Duration::from_millis(180)),
                    |row, progress| {
                        let eased = 1. - (1. - progress).powi(3);
                        row.opacity(eased)
                    },
                )
                .into_any_element()
            };
        }
        let selected = self.selected_sessions.contains(&id)
            || (self.workspace.active == Some(id) && !self.mono_active(cx));
        let status: AnyElement = if agent.needs_approval {
            icon("alert-circle", 12.)
                .text_color(rgb(AMBER))
                .into_any_element()
        } else if agent.done {
            icon("check", 12.)
                .text_color(rgb(EMERALD))
                .into_any_element()
        } else if !agent.working {
            icon("clock", 12.)
                .text_color(content(0.5))
                .into_any_element()
        } else {
            let provider = agent.provider.clone();
            let mark = self
                .live_marks
                .entry(id)
                .or_insert_with(|| {
                    cx.new(|cx| {
                        let mut mark = SessionMark::tab(provider.clone());
                        mark.set(&provider, MarkState::Working, cx);
                        mark
                    })
                })
                .clone();
            div()
                .size(px(12.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .child(mark)
                .into_any_element()
        };
        let label = [
            Some(agent.title.clone()),
            Some(project.clone()),
            Some(agent.activity.clone()),
            elapsed.clone(),
        ]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
        div()
            .id(("live-agent", id as usize))
            .debug_selector(move || format!("live-agent-{id}"))
            .role(Role::Button)
            .aria_label(label)
            .cursor_pointer()
            .w_full()
            .flex()
            .flex_col()
            .rounded_md()
            .px_2()
            .py_1p5()
            .when(selected, |d| d.bg(content(0.1)))
            .when(!selected, |d| d.hover(|s| s.bg(content(0.08))))
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(self.project_mark(&agent.cwd, 8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.))
                            .line_height(px(18.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(content(1.))
                            .child(agent.title),
                    ),
            )
            .child(
                div()
                    .mt_1()
                    .pl_4()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .text_size(px(11.))
                    .line_height(px(14.))
                    .text_color(if agent.needs_approval {
                        rgb(AMBER).into()
                    } else if agent.done {
                        rgb(EMERALD).into()
                    } else {
                        content(0.5)
                    })
                    .child(status)
                    .child(div().min_w_0().truncate().child(agent.activity)),
            )
            .child(
                div()
                    .mt_1()
                    .pl_4()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .text_size(px(11.))
                    .line_height(px(14.))
                    .text_color(content(0.45))
                    .child(provider_icon(&agent.provider, 12.))
                    .child(div().flex_1().min_w_0().truncate().child(project))
                    .children(elapsed.map(|elapsed| div().flex_shrink_0().child(elapsed))),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.selected_sessions.clear();
                this.selection_anchor = Some(id);
                this.open(id, window, cx);
                this.invalidate_chrome(cx);
                cx.notify();
            }))
            .into_any()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: i64, mono: Option<&str>, parent: Option<&str>) -> LiveAgent {
        LiveAgent {
            id,
            mono_id: mono.map(str::to_owned),
            parent_mono_id: parent.map(str::to_owned),
            task_title: parent.map(|_| "Task".into()),
            working: true,
            cwd: "/project".into(),
            provider: "fixture".into(),
            title: format!("Agent {id}"),
            activity: "Working".into(),
            started_at: Some(id as u64),
            duration_ms: None,
            needs_approval: false,
            done: false,
        }
    }

    #[test]
    fn activity_groups_each_orchestration_beneath_its_mono() {
        let order = HashMap::from([("mono-a".into(), 0), ("mono-b".into(), 1)]);
        let mut agents = [
            agent(5, None, None),
            agent(4, None, Some("mono-b")),
            agent(2, None, Some("mono-a")),
            agent(3, Some("mono-b"), None),
            agent(1, Some("mono-a"), None),
        ];
        agents.sort_by_key(|agent| activity_order(agent, &order));
        assert_eq!(
            agents.iter().map(|agent| agent.id).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
    }
}
