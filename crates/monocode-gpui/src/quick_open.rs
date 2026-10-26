//! Cmd+P's project-file search, `>` command palette and `@` project switcher.
use crate::{input, theme::*};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Role, Subscription, Window, div,
    prelude::*, px,
};
use monocode_core::{composer, files::ProjectFile, project_appearance::ProjectAppearance};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Which list the popup shows, decided by the query's first character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Files,
    Commands,
    Projects,
}
impl Mode {
    fn of(query: &str) -> Self {
        match query.chars().next() {
            Some('>') => Mode::Commands,
            Some('@') => Mode::Projects,
            _ => Mode::Files,
        }
    }
    fn seed(self) -> &'static str {
        match self {
            Mode::Files => "",
            Mode::Commands => ">",
            Mode::Projects => "@",
        }
    }
    fn placeholder(self) -> &'static str {
        match self {
            Mode::Files => "Search files by name…  (> commands, @ projects)",
            Mode::Commands => "Run a command…",
            Mode::Projects => "Switch project…",
        }
    }
}
/// An open project as the rail lists it.
#[derive(Clone)]
pub(crate) struct ProjectRow {
    pub path: String,
    pub label: String,
    pub active: bool,
    pub appearance: ProjectAppearance,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    New,
    Project,
    SwitchProject,
    Model,
    Settings,
    Sidebar,
    Panel,
    Terminal,
    Layout,
    WorkspaceStyle,
    Plan,
    Operator,
    Orchestrator,
}
const COMMANDS: &[(Command, &str, &str)] = &[
    (Command::New, "New conversation", "⌘N"),
    (Command::Project, "Open project", "⌘O"),
    (Command::SwitchProject, "Switch project", "⌘⇧P"),
    (Command::Model, "Switch model", "⌘."),
    (Command::Settings, "Open settings", "⌘,"),
    (Command::Sidebar, "Toggle sidebar", "⌘B"),
    (Command::Panel, "Toggle files and changes", "⌘⇧B"),
    (Command::Terminal, "Toggle terminal", "⌘J"),
    (Command::Layout, "Switch workspace layout", "⌘⇧L"),
    (
        Command::WorkspaceStyle,
        "Switch rounded or boxy workspace",
        "⌘⌥L",
    ),
    (Command::Plan, "Toggle Plan mode", ""),
    (Command::Operator, "Toggle Operator", ""),
    (Command::Orchestrator, "Toggle Orchestrator", ""),
];
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Picked {
    File(String),
    Command(Command),
    Project(String),
    Dismiss,
}
#[derive(Clone)]
struct Row {
    label: String,
    detail: String,
    path: Option<String>,
    command: Option<Command>,
    /// Index into `projects`.
    project: Option<usize>,
}
pub(crate) struct QuickOpen {
    pub(crate) search: Entity<input::Input>,
    cwd: String,
    files: Vec<ProjectFile>,
    /// The file listing starts on demand: project mode never needs it.
    files_requested: bool,
    projects: Vec<ProjectRow>,
    rows: Vec<Row>,
    active: usize,
    scroll: gpui::ScrollHandle,
    loading: bool,
    error: Option<String>,
    cancel: Arc<AtomicBool>,
    _changed: Subscription,
}
impl EventEmitter<Picked> for QuickOpen {}
impl Focusable for QuickOpen {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.search.focus_handle(cx)
    }
}
impl Drop for QuickOpen {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
impl QuickOpen {
    pub(crate) fn new(
        cwd: String,
        mode: Mode,
        projects: Vec<ProjectRow>,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            input::Input::field(
                "Go to file, run a command or switch project",
                mode.placeholder(),
                content(0.35),
                cx,
            )
        });
        if !mode.seed().is_empty() {
            search.update(cx, |input, cx| input.set_text(mode.seed().into(), cx));
        }
        let changed = cx.subscribe(&search, |this, _, _: &input::Changed, cx| {
            this.active = 0;
            this.rank(cx);
            cx.notify();
        });
        let mut this = Self {
            search,
            cwd,
            files: Vec::new(),
            files_requested: false,
            projects,
            rows: Vec::new(),
            active: 0,
            scroll: gpui::ScrollHandle::new(),
            loading: true,
            error: None,
            cancel: Arc::new(AtomicBool::new(false)),
            _changed: changed,
        };
        this.rank(cx);
        this
    }
    pub(crate) fn mode(&self, cx: &App) -> Mode {
        Mode::of(self.search.read(cx).content.trim())
    }
    fn request_files(&mut self, cx: &mut Context<Self>) {
        if self.files_requested {
            return;
        }
        self.files_requested = true;
        let cwd = self.cwd.clone();
        let flag = self.cancel.clone();
        let work = cx.background_executor().spawn(async move {
            monocode_core::files::list_project_files_sync_cancellable(&cwd, Some(&flag))
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(files) => this.files = files,
                    Err(e) => this.error = Some(e),
                };
                this.rank(cx);
                cx.notify();
            });
        })
        .detach();
    }
    fn rank(&mut self, cx: &mut Context<Self>) {
        let mode = self.mode(cx);
        let placeholder = mode.placeholder();
        self.search
            .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
        if mode == Mode::Files {
            self.request_files(cx);
        }
        let query = self.search.read(cx).content.trim().to_owned();
        let query = query.as_str();
        self.rows = if let Some(query) = query.strip_prefix('@') {
            self.project_rows(query.trim())
        } else if let Some(query) = query.strip_prefix('>') {
            let mut commands: Vec<_> = COMMANDS
                .iter()
                .filter_map(|(command, label, hint)| {
                    composer::score(query.trim(), label).map(|score| {
                        (
                            score,
                            Row {
                                label: (*label).into(),
                                detail: if cfg!(target_os = "macos") {
                                    (*hint).into()
                                } else {
                                    hint.replace('⌘', "Ctrl+").replace('⇧', "Shift+")
                                },
                                path: None,
                                command: Some(*command),
                                project: None,
                            },
                        )
                    })
                })
                .collect();
            commands.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
            commands.into_iter().map(|(_, row)| row).collect()
        } else {
            composer::rank_files(&self.files, query, 80)
                .into_iter()
                .map(|file| Row {
                    label: file.name.clone(),
                    detail: file.relative.clone(),
                    path: Some(file.path.clone()),
                    command: None,
                    project: None,
                })
                .collect()
        };
        self.active = self.active.min(self.rows.len().saturating_sub(1));
        // Landing on the active project would make Enter a no-op.
        if mode == Mode::Projects
            && self.active == 0
            && self.rows.len() > 1
            && self.is_active_project(0)
        {
            self.active = 1;
        }
        self.scroll.scroll_to_item(self.active);
    }
    /// Projects in rail order, or ranked by label first and path second.
    fn project_rows(&self, query: &str) -> Vec<Row> {
        let row = |(ix, project): (usize, &ProjectRow)| Row {
            label: project.label.clone(),
            detail: project.path.clone(),
            path: None,
            command: None,
            project: Some(ix),
        };
        if query.is_empty() {
            return self.projects.iter().enumerate().map(row).collect();
        }
        let mut scored: Vec<_> = self
            .projects
            .iter()
            .enumerate()
            .filter_map(|entry| {
                let (_, project) = entry;
                let label = composer::score(query, &project.label);
                let path = composer::score(query, &project.path);
                label.max(path).map(|score| (score, row(entry)))
            })
            .collect();
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        scored.into_iter().map(|(_, row)| row).collect()
    }
    /// Only file search waits on the listing; the other modes are in memory.
    fn empty_message(&self, cx: &App) -> String {
        if self.mode(cx) == Mode::Files {
            if self.loading {
                return "Loading project files…".to_owned();
            }
            if let Some(error) = &self.error {
                return error.clone();
            }
        }
        "No results".to_owned()
    }
    fn is_active_project(&self, ix: usize) -> bool {
        self.rows
            .get(ix)
            .and_then(|row| row.project)
            .and_then(|p| self.projects.get(p))
            .is_some_and(|project| project.active)
    }
    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        if !self.rows.is_empty() {
            self.active =
                (self.active as isize + delta).rem_euclid(self.rows.len() as isize) as usize;
            self.scroll.scroll_to_item(self.active);
            cx.notify();
        }
    }
    fn pick(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.search.read(cx).is_composing() {
            return;
        }
        if let Some(row) = self.rows.get(ix) {
            if let Some(path) = &row.path {
                cx.emit(Picked::File(path.clone()));
            } else if let Some(command) = row.command {
                cx.emit(Picked::Command(command));
            } else if let Some(project) = row.project.and_then(|p| self.projects.get(p)) {
                cx.emit(Picked::Project(project.path.clone()));
            }
        }
    }
}
impl Render for QuickOpen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = self
            .rows
            .iter()
            .enumerate()
            .map(|(ix, row)| {
                div()
                    .id(("quick-open-row", ix))
                    .role(Role::ListItem)
                    .aria_label(row.detail.clone())
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .flex()
                    .gap_2p5()
                    .items_center()
                    .bg(if self.active == ix {
                        content(0.1)
                    } else {
                        content(0.)
                    })
                    .hover(|s| s.bg(content(0.07)))
                    .child(if row.path.is_some() {
                        crate::file_icons::file_type_icon(&row.label, false, false, 18.)
                    } else if let Some(project) = row.project.and_then(|p| self.projects.get(p)) {
                        crate::project_settings::project_icon(
                            &project.appearance,
                            &project.path,
                            18.,
                        )
                    } else {
                        icon("sparkles", 16.).into_any_element()
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(13.))
                            .child(div().truncate().child(row.label.clone()))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(content(0.4))
                                    .truncate()
                                    .child(row.detail.clone()),
                            ),
                    )
                    .when(self.is_active_project(ix), |d| {
                        d.child(
                            div()
                                .text_size(px(11.))
                                .text_color(content(0.4))
                                .child("Active"),
                        )
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.pick(ix, cx)))
                    .into_any_element()
            })
            .collect();
        div()
            .id("quick-open")
            .role(Role::Dialog)
            .aria_label("Go to file, run a command or switch project")
            .w_full()
            .flex()
            .flex_col()
            .capture_action(cx.listener(|this, _: &input::Up, _, cx| {
                this.step(-1, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &input::Down, _, cx| {
                this.step(1, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &input::Submit, _, cx| {
                this.pick(this.active, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|_, _: &crate::shell::DismissMenu, _, cx| {
                cx.emit(Picked::Dismiss);
                cx.stop_propagation();
            }))
            .child(
                div()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(content(0.08))
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .child(icon("search", 16.).text_color(content(0.5)))
                    .child(div().flex_1().min_w_0().child(self.search.clone())),
            )
            .child(
                div()
                    .id("quick-open-results")
                    .role(Role::ListBox)
                    .aria_label("Results")
                    .p_1p5()
                    .max_h(px(380.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(rows)
                    .when(self.rows.is_empty(), |d| {
                        d.child(
                            div()
                                .px_3()
                                .py_6()
                                .text_size(px(13.))
                                .text_color(content(0.45))
                                .child(self.empty_message(cx)),
                        )
                    }),
            )
            .child(
                div()
                    .px_4()
                    .py_2()
                    .border_t_1()
                    .border_color(content(0.07))
                    .text_size(px(10.))
                    .text_color(content(0.35))
                    .child("↑ ↓ navigate    ↵ open    Esc close    > commands    @ projects"),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use monocode_core::project_appearance::ProjectAppearance;
    use std::{cell::RefCell, rc::Rc};

    fn row(path: &str, label: &str, active: bool) -> ProjectRow {
        ProjectRow {
            path: path.into(),
            label: label.into(),
            active,
            appearance: ProjectAppearance::default(),
        }
    }
    fn open(mode: Mode, projects: Vec<ProjectRow>, cx: &mut TestAppContext) -> Entity<QuickOpen> {
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        cx.update(|cx| cx.new(|cx| QuickOpen::new(cwd, mode, projects, cx)))
    }
    fn set(picker: &Entity<QuickOpen>, text: &str, cx: &mut TestAppContext) {
        let search = cx.update(|cx| picker.read(cx).search.clone());
        cx.update(|cx| search.update(cx, |input, cx| input.set_text(text.into(), cx)));
        cx.run_until_parked();
    }
    fn labels(picker: &Entity<QuickOpen>, cx: &mut TestAppContext) -> Vec<String> {
        cx.update(|cx| {
            picker
                .read(cx)
                .rows
                .iter()
                .map(|r| r.label.clone())
                .collect()
        })
    }
    fn mode(picker: &Entity<QuickOpen>, cx: &mut TestAppContext) -> Mode {
        cx.update(|cx| picker.read(cx).mode(cx))
    }
    fn placeholder(picker: &Entity<QuickOpen>, cx: &mut TestAppContext) -> String {
        cx.update(|cx| picker.read(cx).search.read(cx).placeholder().to_owned())
    }

    #[gpui::test]
    fn project_mode_ranks_the_label_before_the_path(cx: &mut TestAppContext) {
        let picker = open(
            Mode::Projects,
            vec![
                row("/work/another-project", "another-project", false),
                row("/srv/mono-api", "api", false),
                row("/work/monocode", "monocode", false),
            ],
            cx,
        );
        set(&picker, "@mono", cx);
        assert_eq!(labels(&picker, cx), ["monocode", "api"]);
    }

    #[gpui::test]
    fn project_mode_keeps_rail_order_for_an_empty_query(cx: &mut TestAppContext) {
        let picker = open(
            Mode::Projects,
            vec![row("/b", "b", false), row("/a", "a", false)],
            cx,
        );
        assert_eq!(labels(&picker, cx), ["b", "a"]);
    }

    #[gpui::test]
    fn project_mode_starts_on_the_first_project_that_is_not_active(cx: &mut TestAppContext) {
        let picker = open(
            Mode::Projects,
            vec![row("/a", "a", true), row("/b", "b", false)],
            cx,
        );
        assert_eq!(cx.update(|cx| picker.read(cx).active), 1);
        let only = open(Mode::Projects, vec![row("/a", "a", true)], cx);
        assert_eq!(cx.update(|cx| only.read(cx).active), 0);
    }

    #[gpui::test]
    fn picking_a_project_row_emits_its_path(cx: &mut TestAppContext) {
        let picker = open(
            Mode::Projects,
            vec![row("/a", "a", true), row("/b", "b", false)],
            cx,
        );
        let picked = Rc::new(RefCell::new(None));
        let sink = picked.clone();
        cx.update(|cx| {
            cx.subscribe(&picker, move |_, event: &Picked, _| {
                *sink.borrow_mut() = Some(event.clone());
            })
            .detach()
        });
        cx.update(|cx| picker.update(cx, |p, cx| p.pick(p.active, cx)));
        assert_eq!(*picked.borrow(), Some(Picked::Project("/b".into())));
    }

    #[gpui::test]
    fn the_at_prefix_switches_to_projects_and_deleting_it_returns_to_files(
        cx: &mut TestAppContext,
    ) {
        let picker = open(Mode::Files, vec![row("/a", "a", false)], cx);
        assert_eq!(mode(&picker, cx), Mode::Files);
        set(&picker, "@", cx);
        assert_eq!(mode(&picker, cx), Mode::Projects);
        assert_eq!(labels(&picker, cx), ["a"]);
        assert_eq!(placeholder(&picker, cx), "Switch project…");
        set(&picker, "", cx);
        assert_eq!(mode(&picker, cx), Mode::Files);
        assert_eq!(
            placeholder(&picker, cx),
            "Search files by name…  (> commands, @ projects)"
        );
    }

    #[gpui::test]
    fn opening_in_project_mode_seeds_the_at_prefix(cx: &mut TestAppContext) {
        let picker = open(Mode::Projects, vec![], cx);
        assert_eq!(
            cx.update(|cx| picker.read(cx).search.read(cx).content.to_string()),
            "@"
        );
        assert_eq!(placeholder(&picker, cx), "Switch project…");
    }

    #[gpui::test]
    fn an_unmatched_project_query_says_no_results_not_loading(cx: &mut TestAppContext) {
        let picker = open(Mode::Projects, vec![row("/a", "alpha", false)], cx);
        set(&picker, "@zzz", cx);
        assert!(labels(&picker, cx).is_empty());
        assert_eq!(
            cx.update(|cx| picker.read(cx).empty_message(cx)),
            "No results"
        );
        // File mode still reports a listing that is in flight.
        let files = open(Mode::Files, Vec::new(), cx);
        assert_eq!(
            cx.update(|cx| files.read(cx).empty_message(cx)),
            "Loading project files…"
        );
    }

    #[gpui::test]
    fn the_command_list_offers_switch_project(cx: &mut TestAppContext) {
        let picker = open(Mode::Commands, vec![], cx);
        set(&picker, ">switch proj", cx);
        assert!(cx.update(|cx| {
            picker
                .read(cx)
                .rows
                .iter()
                .any(|r| r.command == Some(Command::SwitchProject))
        }));
    }
}
