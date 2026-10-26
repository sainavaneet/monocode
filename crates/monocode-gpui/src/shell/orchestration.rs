//! Orchestrator's reviewed plans create real native sessions. Dependencies and
//! overlapping file scopes gate dispatch; approvals stay in each worker's chat.
//! With the task coordinator on (`native-task-coordinator`), Start instead
//! persists the plan as a task and the coordinator schedules it.
use super::*;
use crate::conversation::orchestration_card::TaskProgress;
use crate::conversation::task_controls::TaskControl;
use monocode_core::{
    agent::Settings,
    orchestration::{self, Plan, Task},
};
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Pending,
    Starting,
    Running,
    Done,
    Failed,
    Blocked,
}

struct Assignment {
    task: Task,
    session: Option<i64>,
    state: State,
    report: String,
}
pub(super) struct Run {
    cwd: String,
    provider: String,
    settings: Settings,
    tasks: Vec<Assignment>,
    stopped: bool,
}
/// Carries out a pause, resume, cancel or steer the user chose on a plan
/// card's task row, as the user, through the same core path as Mono's
/// `tasks.*` commands (the transition table, revisions, interrupt before
/// stop). The store is used off the main thread; the card shows the result.
pub(crate) fn apply_task_control(
    store: Database,
    control: TaskControl,
    view: gpui::WeakEntity<Conversation>,
    cx: &mut App,
) {
    let request = control.clone();
    let work = cx.background_executor().spawn(async move {
        let mut input = serde_json::json!({"revision": request.revision});
        if let Some(requirements) = &request.requirements {
            input["requirements"] = serde_json::json!(requirements);
        }
        if request.confirmed_stopped {
            input["confirmedStopped"] = serde_json::json!(true);
        }
        let store = locked(&store)?;
        // Pause, resume and cancel act on the task as it is now (routine
        // coordinator updates bump its revision between the card's
        // refreshes; the table still refuses a move its state does not
        // allow). A steer keeps the revision the card showed: new
        // requirements written against a changed plan are refused.
        let refresh = request.action != "tasks.steer";
        if let Some(task) = store
            .tasks()
            .map_err(|e| e.to_string())?
            .task(&request.task_id)
            .map_err(|e| e.to_string())?
            .filter(|_| refresh)
        {
            input["revision"] = serde_json::json!(task.revision);
        }
        monocode_core::app_access::task_control(
            &store,
            &request.task_id,
            request.action,
            &input,
            monocode_core::tasks::Requester::User,
            monocode_core::time::now_ms(),
        )
    });
    cx.spawn(async move |cx| {
        let result = work.await;
        let _ = view.update(cx, |view, cx| view.task_control_done(&control, result, cx));
    })
    .detach();
}

impl Shell {
    pub(super) fn observe_orchestration(
        &mut self,
        view: &Entity<Conversation>,
        id: i64,
        cx: &mut Context<Self>,
    ) {
        use crate::conversation::{
            composer_features::{StartOrchestration, StopOrchestration},
            orchestration_card::OpenOrchestrationWorker,
        };
        self.observations.push(cx.subscribe(
            view,
            move |this, _, event: &StartOrchestration, cx| {
                this.start_orchestration(id, event.0.clone(), cx)
            },
        ));
        self.observations.push(
            cx.subscribe(view, move |this, _, _: &StopOrchestration, cx| {
                this.stop_orchestration(id, cx)
            }),
        );
        let store = self.store.clone();
        self.observations.push(
            cx.subscribe(view, move |_, view, control: &TaskControl, cx| {
                apply_task_control(store.clone(), control.clone(), view.downgrade(), cx)
            }),
        );
        self.observations.push(cx.subscribe(
            view,
            |this, _, event: &OpenOrchestrationWorker, cx| {
                // Activation needs the window, so the next render opens it.
                this.pending_activation = Some(event.0);
                cx.notify();
            },
        ));
    }
    fn start_orchestration(&mut self, lead: i64, plan: Plan, cx: &mut Context<Self>) {
        if self.orchestrations.contains_key(&lead) {
            return;
        }
        if let Err(error) = orchestration::validate(&plan) {
            self.error = Some(error);
            cx.notify();
            return;
        }
        let Some(view) = self.sessions.get(&lead) else {
            return;
        };
        let conversation = view.read(cx);
        if conversation.busy {
            return;
        }
        let cwd = conversation
            .settings
            .worktree_cwd
            .clone()
            .unwrap_or_else(|| conversation.summary.cwd.clone());
        if self.orchestrations.values().any(|run| run.cwd == cwd) {
            self.error = Some("An orchestration is already using this checkout.".into());
            cx.notify();
            return;
        }
        // With the task coordinator on, the plan becomes a persisted task
        // and the coordinator schedules it (dependencies, file scopes,
        // workspace ownership and the Mono's worker limit).
        if locked(&self.store).is_ok_and(|store| monocode_core::tasks::coordinator::enabled(&store))
        {
            let provider = conversation.summary.provider.clone();
            self.start_native_orchestration(lead, plan, cwd, provider, cx);
            return;
        }
        let mut settings = conversation.settings.clone();
        settings.plan = false;
        settings.orchestrator = false;
        settings.operator = false;
        settings.operator_enabled = false;
        settings.handoff = None;
        settings.turns.clear();
        settings.environment.clear();
        settings.attachments.clear();
        let provider = conversation.summary.provider.clone();
        let tasks = plan
            .tasks
            .into_iter()
            .map(|task| Assignment {
                task,
                session: None,
                state: State::Pending,
                report: String::new(),
            })
            .collect();
        self.orchestrations.insert(
            lead,
            Run {
                cwd,
                provider,
                settings,
                tasks,
                stopped: false,
            },
        );
        self.schedule_orchestration(lead, cx);
    }
    /// The Mono that owns `lead` (its chat or one of its launches), whose
    /// policy the task takes; None for a session no Mono started.
    fn owning_mono(&self, lead: i64, cx: &App) -> Option<String> {
        let monos = self.monos.as_ref()?.read(cx);
        monos
            .roster
            .monos
            .iter()
            .find(|m| m.session_id == Some(lead) || m.launches.iter().any(|l| l.session_id == lead))
            .map(|m| m.id.clone())
    }
    /// Persists the reviewed plan as a task started by the user, off the
    /// main thread; the plan card closes once it is saved.
    fn start_native_orchestration(
        &mut self,
        lead: i64,
        plan: Plan,
        cwd: String,
        provider: String,
        cx: &mut Context<Self>,
    ) {
        use monocode_core::tasks::{NewTask, Requester, policy};
        let store = self.store.clone();
        let new = NewTask {
            id: format!(
                "{}{}",
                policy::plan_task_prefix(lead),
                monocode_core::mono::new_id()
            ),
            mono_id: self.owning_mono(lead, cx),
            project: cwd,
            title: plan.title.clone(),
            request: if plan.summary.trim().is_empty() {
                plan.title.clone()
            } else {
                plan.summary.clone()
            },
            criteria: policy::plan_criteria(&plan),
        };
        let work = cx.background_executor().spawn(async move {
            let store = locked(&store)?;
            if !policy::started_plans(&store, lead)
                .map_err(|e| e.to_string())?
                .is_empty()
            {
                return Err("A task from this chat's plan is still running.".to_owned());
            }
            let now = monocode_core::time::now_ms();
            policy::start_plan(&store, &new, &plan, &provider, Requester::User, now)
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        if let Some(view) = this.sessions.get(&lead) {
                            view.update(cx, |v, cx| {
                                v.orchestration_plan = None;
                                cx.notify();
                            });
                        }
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn stop_orchestration(&mut self, lead: i64, cx: &mut Context<Self>) {
        if let Some(run) = self.orchestrations.get_mut(&lead) {
            run.stopped = true;
            for task in &mut run.tasks {
                if matches!(task.state, State::Pending | State::Starting) {
                    task.state = State::Blocked;
                }
                if task.state == State::Running
                    && let Some(view) = task.session.and_then(|id| self.sessions.get(&id))
                {
                    view.update(cx, |v, cx| v.stop_if_busy(cx));
                }
            }
        }
        self.schedule_orchestration(lead, cx);
    }
    pub(super) fn orchestration_session_changed(&mut self, id: i64, cx: &mut Context<Self>) {
        let leads: Vec<_> = self
            .orchestrations
            .iter()
            .filter(|(lead, run)| **lead == id || run.tasks.iter().any(|t| t.session == Some(id)))
            .map(|(lead, _)| *lead)
            .collect();
        for lead in leads {
            if let Some(run) = self.orchestrations.get_mut(&lead) {
                for task in &mut run.tasks {
                    if task.session == Some(id)
                        && task.state == State::Running
                        && let Some(view) = self.sessions.get(&id)
                    {
                        let view = view.read(cx);
                        if !view.busy {
                            let (success, report) = view.external_outcome(cx);
                            task.report = report;
                            task.state = if success { State::Done } else { State::Failed };
                        }
                    }
                }
            }
            self.schedule_orchestration(lead, cx);
        }
    }
    /// The plan card's per-assignment status, with workers that wait on an
    /// approval or answer shown as needing input.
    fn orchestration_progress(&self, lead: i64, cx: &App) -> Vec<TaskProgress> {
        let Some(summary) = self.orchestration_summaries.get(&lead) else {
            return Vec::new();
        };
        summary
            .tasks
            .iter()
            .map(|task| {
                let needs_input = task
                    .session
                    .and_then(|id| self.sessions.get(&id))
                    .is_some_and(|view| view.read(cx).needs_approval());
                TaskProgress {
                    label: task.label(summary, needs_input),
                    session: task.session,
                }
            })
            .collect()
    }
    fn schedule_orchestration(&mut self, lead: i64, cx: &mut Context<Self>) {
        let Some(run) = self.orchestrations.get_mut(&lead) else {
            return;
        };
        // Failed dependencies transitively block their dependants.
        loop {
            let failed: HashSet<_> = run
                .tasks
                .iter()
                .filter(|t| matches!(t.state, State::Failed | State::Blocked))
                .map(|t| t.task.id.clone())
                .collect();
            let mut changed = false;
            for task in &mut run.tasks {
                if task.state == State::Pending
                    && task.task.depends_on.iter().any(|id| failed.contains(id))
                {
                    task.state = State::Blocked;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let mut occupied: Vec<Vec<String>> = run
            .tasks
            .iter()
            .filter(|t| matches!(t.state, State::Starting | State::Running))
            .map(|t| t.task.files.clone())
            .collect();
        let completed: HashSet<_> = run
            .tasks
            .iter()
            .filter(|t| t.state == State::Done)
            .map(|t| t.task.id.clone())
            .collect();
        let reports = run
            .tasks
            .iter()
            .filter(|t| t.state == State::Done)
            .map(|t| format!("{}: {}", t.task.title, t.report))
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut jobs = Vec::new();
        if !run.stopped {
            for (ix, task) in run.tasks.iter_mut().enumerate() {
                if occupied.len() >= 4 {
                    break;
                }
                if task.state != State::Pending
                    || !task.task.depends_on.iter().all(|id| completed.contains(id))
                    || occupied
                        .iter()
                        .any(|files| orchestration::overlaps(files, &task.task.files))
                {
                    continue;
                }
                task.state = State::Starting;
                occupied.push(task.task.files.clone());
                jobs.push((
                    ix,
                    task.task.clone(),
                    run.cwd.clone(),
                    run.provider.clone(),
                    run.settings.clone(),
                    reports.clone(),
                ));
            }
        }
        let done = run
            .tasks
            .iter()
            .all(|t| matches!(t.state, State::Done | State::Failed | State::Blocked));
        // The sidebar lists the lead's agents from this, during and after the run.
        let summary = orchestration::Summary {
            live: !done,
            tasks: run
                .tasks
                .iter()
                .map(|t| orchestration::SummaryTask {
                    session: t.session,
                    title: t.task.title.clone(),
                    provider: run.provider.clone(),
                    model: run.settings.model.clone(),
                    status: match t.state {
                        State::Pending | State::Starting => "queued",
                        State::Running => "running",
                        State::Done => "completed",
                        State::Failed => "failed",
                        State::Blocked => "blocked",
                    }
                    .into(),
                })
                .collect(),
        };
        if self.orchestration_summaries.get(&lead) != Some(&summary) {
            let store = self.store.clone();
            let saved = summary.clone();
            cx.background_executor()
                .spawn(async move {
                    let _ = locked(&store).and_then(|s| s.save_orchestration_summary(lead, &saved));
                })
                .detach();
            self.orchestration_summaries.insert(lead, summary);
            self.invalidate_chrome(cx);
        }
        let progress = self.orchestration_progress(lead, cx);
        if let Some(view) = self.sessions.get(&lead) {
            view.update(cx, |v, cx| v.set_orchestration_progress(Some(progress), cx));
        }
        let Some(run) = self.orchestrations.get_mut(&lead) else {
            return;
        };
        if done {
            let stopped = run.stopped;
            let reports = run
                .tasks
                .iter()
                .map(|t| {
                    format!(
                        "{} [{}]\n{}",
                        t.task.title,
                        if t.state == State::Done {
                            "completed"
                        } else {
                            "incomplete"
                        },
                        t.report
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            let ready = self
                .sessions
                .get(&lead)
                .is_some_and(|v| v.read(cx).ready_for_external(cx));
            if stopped || ready {
                self.orchestrations.remove(&lead);
                if let Some(view) = self.sessions.get(&lead) {
                    view.update(cx, |v, cx| {
                        v.orchestration_plan = None;
                        v.set_orchestration_progress(None, cx);
                        if !stopped {
                            v.send_coordinator_report(format!("The orchestration workers have finished. Summarize and verify the combined results, identify incomplete work and report the outcome to the user. Preserve their changes.\n\n{reports}"), cx);
                        }
                    });
                }
            }
        }
        for (ix, task, cwd, provider, mut settings, reports) in jobs {
            let store = self.store.clone();
            let project = self.sessions[&lead].read(cx).summary.cwd.clone();
            let work = cx.background_executor().spawn(async move {
                let root = std::path::Path::new(&cwd)
                    .canonicalize()
                    .map_err(|e| e.to_string())?;
                // Existing symlink parents must not redirect an assignment outside the checkout.
                for file in &task.files {
                    let mut path = root.join(file);
                    while !path.exists() {
                        if !path.pop() {
                            break;
                        }
                    }
                    if !path
                        .canonicalize()
                        .map_err(|e| e.to_string())?
                        .starts_with(&root)
                    {
                        return Err(format!("Assignment scope escapes the checkout: {file}"));
                    }
                }
                let store = locked(&store)?;
                let mut summary = store.create(&provider, &project)?;
                summary.title = format!("Agent · {}", task.title);
                store.title(summary.id, &summary.title)?;
                settings.worktree_cwd = Some(cwd);
                store.save_agent_settings(summary.id, &settings)?;
                Ok::<_, String>((
                    summary,
                    settings,
                    orchestration::worker_prompt(&task, &reports),
                ))
            });
            cx.spawn(async move |this, cx| {
                let result = work.await;
                let _ = this.update(cx, |this, cx| {
                    match result {
                        Ok((summary, settings, prompt)) => {
                            let id = summary.id;
                            this.add(summary, cx);
                            let start = this.orchestrations.get(&lead).is_some_and(|run| {
                                !run.stopped && run.tasks[ix].state == State::Starting
                            });
                            if start {
                                if let Some(run) = this.orchestrations.get_mut(&lead) {
                                    run.tasks[ix].session = Some(id);
                                    run.tasks[ix].state = State::Running;
                                }
                                this.sessions[&id].update(cx, |v, cx| {
                                    v.install_transfer(settings, Some(prompt), cx)
                                });
                            }
                        }
                        Err(error) => {
                            if let Some(run) = this.orchestrations.get_mut(&lead) {
                                run.tasks[ix].state = State::Failed;
                                run.tasks[ix].report = error;
                            }
                        }
                    }
                    this.invalidate_chrome(cx);
                    this.schedule_orchestration(lead, cx);
                    cx.notify();
                });
            })
            .detach();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use monocode_core::tasks::{AssignmentState, TaskLifecycle, TaskStage};

    #[gpui::test]
    fn with_the_coordinator_on_start_persists_a_task_the_coordinator_schedules(
        cx: &mut TestAppContext,
    ) {
        let (window, _, lead) = crate::shell::tests::window(cx);
        let sessions = window
            .update(cx, |shell, _, cx| {
                locked(&shell.store)
                    .unwrap()
                    .save_setting(monocode_core::tasks::coordinator::FLAG, "enabled")
                    .unwrap();
                shell.sessions[&lead].update(cx, |v, cx| {
                    v.install_transfer(Settings::default(), None, cx)
                });
                let part = |id: &str, file: &str, deps: &[&str]| Task {
                    id: id.into(),
                    title: format!("Part {id}"),
                    prompt: "Implement it".into(),
                    files: vec![file.into()],
                    depends_on: deps.iter().map(|d| (*d).to_owned()).collect(),
                    verification: vec![format!("test -f {file}")],
                    ..Task::default()
                };
                let plan = Plan {
                    title: "Two parts".into(),
                    summary: "Build both parts".into(),
                    tasks: vec![part("a", "src/a.rs", &[]), part("b", "src/b.rs", &["a"])],
                };
                shell.sessions[&lead].update(cx, |v, _| v.orchestration_plan = Some(plan.clone()));
                shell.start_orchestration(lead, plan, cx);
                // The in-app scheduler does not run it.
                assert!(!shell.orchestrations.contains_key(&lead));
                shell.sessions.len()
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |shell, _, cx| {
                let store = locked(&shell.store).unwrap();
                let started = monocode_core::tasks::policy::started_plans(&store, lead).unwrap();
                assert_eq!(started.len(), 1);
                let task = &started[0];
                assert_eq!(
                    (task.stage, task.lifecycle),
                    (TaskStage::Implementing, TaskLifecycle::Active)
                );
                assert_eq!(task.project, "/work/monocode");
                assert_eq!(task.criteria.len(), 2, "each part's checks are a criterion");
                let tasks = store.tasks().unwrap();
                let assignments = tasks.assignments(&task.id).unwrap();
                assert_eq!(assignments.len(), 2);
                assert!(
                    assignments
                        .iter()
                        .all(|a| a.status == AssignmentState::Queued)
                );
                assert!(
                    monocode_core::tasks::policy::of(&tasks, &task.id)
                        .unwrap()
                        .is_some()
                );
                // Nothing was launched by the app itself; the plan card closed.
                assert_eq!(shell.sessions.len(), sessions);
                assert!(shell.sessions[&lead].read(cx).orchestration_plan.is_none());
                assert!(shell.error.is_none(), "{:?}", shell.error);
            })
            .unwrap();
    }
    #[gpui::test]
    fn reviewed_dispatch_caps_workers_serializes_scopes_and_stops_before_launch(
        cx: &mut TestAppContext,
    ) {
        let (window, _, lead) = crate::shell::tests::window(cx);
        window
            .update(cx, |shell, _, cx| {
                shell.sessions[&lead].update(cx, |v, cx| {
                    v.install_transfer(Settings::default(), None, cx)
                });
                let tasks = ["src", "src/main.rs", "tests", "docs", "examples", "scripts"]
                    .into_iter()
                    .enumerate()
                    .map(|(ix, file)| Task {
                        id: format!("task-{ix}"),
                        title: format!("Assignment {ix}"),
                        prompt: "Inspect and report".into(),
                        files: vec![file.into()],
                        depends_on: Vec::new(),
                        ..Task::default()
                    })
                    .collect();
                shell.start_orchestration(
                    lead,
                    Plan {
                        title: "Review".into(),
                        summary: String::new(),
                        tasks,
                    },
                    cx,
                );
                let run = shell.orchestrations.get(&lead).unwrap();
                assert_eq!(
                    run.tasks
                        .iter()
                        .filter(|t| t.state == State::Starting)
                        .count(),
                    4
                );
                assert!(
                    run.tasks[1].state == State::Pending,
                    "overlapping src/main.rs waits for src"
                );
                shell.stop_orchestration(lead, cx);
                assert!(!shell.orchestrations.contains_key(&lead));
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |shell, _, cx| {
                assert!(shell.sessions.values().all(|v| !v.read(cx).busy))
            })
            .unwrap();
    }
}
