//! Mono as development lead, in the app: Mono's task actions, the task inbox
//! that delivers worker questions and review hand-offs to Mono's chat, the
//! hand-off of answers to workers, and the user's replies to decision cards.
//! Everything here does nothing unless `native-task-coordinator` is enabled.
use super::host::Effect;
use super::*;
use crate::conversation::task_monitor::LiveWorker;
use monocode_core::app_access as app;
use monocode_core::mono::bridge::Call;
use monocode_core::tasks::decisions::{self, Handoff, ReplyMatch};
use monocode_core::tasks::view::{self, TaskMonitor};
use monocode_core::tasks::{Requester, coordinator, inbox};
use monocode_core::time::now_ms;

/// What one pass found: per Mono the batch to deliver, answers owed to
/// workers with their session handles, and per Mono its tasks' monitoring
/// view (which carries their controls).
type Pass = (
    Vec<(String, inbox::Batch)>,
    Vec<(i64, Handoff)>,
    Vec<(String, Vec<TaskMonitor>)>,
);

impl Monos {
    /// `tasks.decisions.*`, `tasks.review.*` and `tasks.criteria.verify` from
    /// a Mono's chat. A habit run cannot act on tasks; the Mono comes from
    /// the calling session, never from the input.
    pub(super) fn task_call(&mut self, call: Call, mono: Mono, cx: &mut Context<Self>) {
        if self.running.contains_key(&call.session) {
            let _ = call
                .reply
                .send(Err("A habit run cannot act on tasks".into()));
            return;
        }
        let action = call.action.clone();
        let input = call.input.clone();
        let id = mono.id.clone();
        self.call_work(
            call,
            mono.id,
            move |store| {
                // A criterion check can run for minutes: hold the store only
                // to prepare it and to record it, as the coordinator does.
                if action == "tasks.criteria.verify" {
                    let run = app::prepare_mono_verify(&*locked(&store)?, &id, &input)?;
                    let outcome = run.run(now_ms());
                    let value = app::finish_mono_verify(&*locked(&store)?, &run, outcome)?;
                    return Ok((value, Effect::None));
                }
                let value =
                    app::mono_task_action(&*locked(&store)?, &id, &action, &input, now_ms())?;
                // An escalation's card goes into the Mono's chat, with its own
                // id so it can be marked answered later.
                let effect = value.get("card").cloned().map_or(Effect::None, |mut card| {
                    card["id"] = serde_json::json!(mono::new_id());
                    Effect::Card(card)
                });
                Ok((value, effect))
            },
            cx,
        );
    }

    /// One pass of the task inbox and answer hand-offs.
    pub(super) fn pump_tasks(&mut self, cx: &mut Context<Self>) {
        if self.pumping_tasks {
            return;
        }
        let monos: Vec<(String, Option<i64>)> = self
            .roster
            .monos
            .iter()
            .map(|m| (m.id.clone(), m.session_id))
            .collect();
        if monos.is_empty() {
            return;
        }
        self.pumping_tasks = true;
        let store = self.store.clone();
        let work = cx.background_executor().spawn(async move {
            let db = locked(&store)?;
            if !coordinator::enabled(&db) {
                return Ok::<Option<Pass>, String>(None);
            }
            let now = now_ms();
            let mut batches = Vec::new();
            let mut monitors = Vec::new();
            for (mono, session) in monos {
                let tasks = view::monitor(&db, &mono).map_err(|e| e.to_string())?;
                monitors.push((mono.clone(), tasks));
                if let Some(session) = session {
                    inbox::acknowledge_delivered(&db, &mono, session, now)
                        .map_err(|e| e.to_string())?;
                }
                if let Some(batch) =
                    inbox::next_batch(&db, &mono, now).map_err(|e| e.to_string())?
                {
                    batches.push((mono, batch));
                }
            }
            let mut handoffs = Vec::new();
            for handoff in db
                .tasks()
                .map_err(|e| e.to_string())?
                .pending_handoffs()
                .map_err(|e| e.to_string())?
            {
                if let Ok(handle) = db.resolve_session_id(&handoff.session_id) {
                    handoffs.push((handle, handoff));
                }
            }
            Ok(Some((batches, handoffs, monitors)))
        });
        cx.spawn(async move |this, cx| {
            let pass = work.await;
            let _ = this.update(cx, |this, cx| {
                this.pumping_tasks = false;
                match pass {
                    Ok(Some((batches, handoffs, monitors))) => {
                        this.show_task_monitors(monitors, cx);
                        for (mono, batch) in batches {
                            this.deliver_batch(&mono, batch, cx);
                        }
                        for (session, handoff) in handoffs {
                            this.hand_off(session, handoff, cx);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => eprintln!("Mono task inbox: {error}"),
                }
            });
        })
        .detach();
    }

    /// Each Mono's tasks as the store has them now: the chat's monitoring
    /// card and controls, the live state of their worker sessions, and the
    /// sidebar badge.
    fn show_task_monitors(
        &mut self,
        monitors: Vec<(String, Vec<TaskMonitor>)>,
        cx: &mut Context<Self>,
    ) {
        let live: HashMap<i64, LiveWorker> = self
            .owner
            .upgrade()
            .map(|owner| {
                owner
                    .read(cx)
                    .mono_session_views(cx)
                    .iter()
                    .map(|s| (s.id, LiveWorker::from(s)))
                    .collect()
            })
            .unwrap_or_default();
        let mut changed = false;
        for (mono, tasks) in monitors {
            if let Some(chat) = self.chats.get(&mono) {
                let rows = tasks.iter().map(|t| t.control.clone()).collect();
                let sessions: Vec<i64> = tasks
                    .iter()
                    .flat_map(|t| t.workers.iter().filter_map(|w| w.session))
                    .collect();
                let shown = tasks.clone();
                chat.update(cx, |chat, cx| {
                    chat.set_task_controls(rows, cx);
                    chat.set_task_monitor(shown, cx);
                    for session in sessions {
                        if let Some(state) = live.get(&session) {
                            chat.set_worker_live(session, *state, cx);
                        }
                    }
                });
            }
            if self.task_monitors.get(&mono) != Some(&tasks) {
                self.task_monitors.insert(mono, tasks);
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }

    /// The sidebar badge of a Mono's tasks (see `tasks::view::Badge`).
    pub(crate) fn task_badge(&self, mono: &str) -> Option<view::Badge> {
        self.task_monitors
            .get(mono)
            .and_then(|tasks| view::badge(tasks))
    }

    /// Every Mono's badge, in a stable order.
    pub(super) fn task_badges(&self) -> Vec<(String, view::Badge)> {
        let mut badges: Vec<_> = self
            .task_monitors
            .iter()
            .filter_map(|(mono, tasks)| view::badge(tasks).map(|b| (mono.clone(), b)))
            .collect();
        badges.sort();
        badges
    }

    /// A worker session changed in the app: its Mono's card shows it now,
    /// before the next store read.
    pub(crate) fn task_worker_changed(
        &mut self,
        session: i64,
        live: LiveWorker,
        cx: &mut Context<Self>,
    ) {
        for (mono, tasks) in &self.task_monitors {
            let ours = tasks
                .iter()
                .any(|t| t.workers.iter().any(|w| w.session == Some(session)));
            if let (true, Some(chat)) = (ours, self.chats.get(mono)) {
                chat.update(cx, |chat, cx| chat.set_worker_live(session, live, cx));
            }
        }
    }

    /// Opens a worker's session in the Mono panel, as a session card does.
    pub(super) fn open_task_session(&mut self, session: i64, cx: &mut Context<Self>) {
        self.route = Some(Route::Session(session));
        self.show_overview(cx);
        self.overview_tab = OverviewTab::Sessions;
        cx.notify();
    }

    /// Opens an assignment's worktree changes in the app's Changes review.
    pub(super) fn open_task_changes(&mut self, cwd: String, cx: &mut Context<Self>) {
        let (Some(handle), Some(owner)) = (self.parent_window, self.owner.upgrade()) else {
            return;
        };
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| {
                owner.update(cx, |shell, cx| shell.mono_open_changes(cwd, window, cx))
            });
        });
    }

    /// Queues the batch in the Mono's outbox under its own id, once. The
    /// outbox starts an internal message only when the chat is idle, so a
    /// task event never starts a second turn on Mono's conversation.
    fn deliver_batch(&mut self, mono: &str, batch: inbox::Batch, cx: &mut Context<Self>) {
        let delivered = self
            .chats
            .get(mono)
            .is_some_and(|v| v.read(cx).mono_delivered().contains(&batch.id));
        let failed = self
            .roster
            .get(mono)
            .and_then(|m| m.outbox.iter().find(|q| q.id == batch.id))
            .and_then(|q| q.error.clone());
        if let Some(error) = failed {
            self.batch_failed(mono, &batch.id, &error, cx);
            return;
        }
        let Some(owner) = self.roster.get_mut(mono) else {
            return;
        };
        if delivered || owner.outbox.iter().any(|m| m.id == batch.id) {
            return;
        }
        owner.outbox.push(Message {
            id: batch.id,
            text: batch.message,
            sent_at: Some(now_ms()),
            internal: true,
            error: None,
            attachments: Vec::new(),
        });
        self.save(cx);
        self.configure_chat(mono, cx);
        self.ensure_chat(mono, cx);
        self.dispatch(mono, cx);
    }

    /// Mono's turn for a batch failed (usage limit, provider error): the batch
    /// leaves the outbox and its events are retried in a new one, until one
    /// has failed too often; that is shown in Mono's chat and stops holding
    /// later events.
    fn batch_failed(&mut self, mono: &str, batch: &str, error: &str, cx: &mut Context<Self>) {
        if let Some(owner) = self.roster.get_mut(mono) {
            owner.outbox.retain(|q| q.id != batch);
        }
        let abandoned = locked(&self.store).and_then(|db| {
            inbox::batch_failed(&db, mono, batch, error, now_ms()).map_err(|e| e.to_string())
        });
        match abandoned {
            Ok(abandoned) if !abandoned.is_empty() => {
                let note = format!(
                    "Task events could not reach this chat after {} attempts ({}): {error}. They stay recorded; run tasks.decisions.list to see open questions.",
                    inbox::INBOX_RETRY_LIMIT,
                    abandoned.join(", ")
                );
                if let Some(owner) = self.roster.get_mut(mono) {
                    owner.pending_reports.push(note);
                }
            }
            Ok(_) => {}
            Err(error) => eprintln!("Mono task inbox: {error}"),
        }
        self.save(cx);
        self.configure_chat(mono, cx);
    }

    /// The decision's current record, for its card.
    pub(super) fn decision_now(&self, id: &str) -> Option<monocode_core::tasks::DecisionRequest> {
        let db = locked(&self.store).ok()?;
        let tasks = db.existing_tasks().ok()??;
        tasks.decision(id).ok()?
    }

    /// Hands an answer to its worker: through the provider's reply when the
    /// provider is still waiting on the routed question, otherwise as the
    /// worker's next message once it is idle. The receipt resolves it.
    fn hand_off(&mut self, session: i64, handoff: Handoff, cx: &mut Context<Self>) {
        let decision = &handoff.decision;
        let key = format!("{}@{}", decision.id, decision.revision);
        if self.task_sent.contains(&key) {
            return;
        }
        let Some(view) = self
            .owner
            .upgrade()
            .and_then(|owner| owner.update(cx, |owner, cx| owner.mono_ensure_session(session, cx)))
        else {
            return;
        };
        let answer = decision.answer.clone().unwrap_or_default();
        let via = match &handoff.question {
            Some(question) if view.read(cx).task_question_waiting(&decision.id) => {
                let id = decision.id.clone();
                monocode_core::provider::answers_from_text(question, &answer).and_then(|answers| {
                    view.update(cx, |v, cx| v.answer_task_question(&id, answers, cx))
                        .map(|_| "provider_reply")
                })
            }
            _ => {
                let idle = {
                    let v = view.read(cx);
                    !v.busy
                        && !v.mono_external_pending()
                        && v.task_question_id(&decision.id).is_none()
                        && v.input.read(cx).content.trim().is_empty()
                };
                if !idle {
                    return;
                }
                let message = decisions::answer_message(decision);
                view.update(cx, |v, cx| v.mono_external_send(message, false, cx))
                    .map(|_| "message")
            }
        };
        let via = match via {
            Ok(via) => via,
            Err(error) => {
                eprintln!("Answer for decision {}: {error}", decision.id);
                return;
            }
        };
        // Sent: never send this revision again, even if recording fails.
        self.task_sent.insert(key);
        let attempt = decision.attempt_id.clone().unwrap_or_default();
        let resolved = locked(&self.store).and_then(|db| {
            db.tasks()
                .and_then(|tasks| {
                    tasks.resolve_handoff(&decision.id, decision.revision, &attempt, via, now_ms())
                })
                .map_err(|e| e.to_string())
        });
        if let Err(error) = resolved {
            eprintln!("Answer for decision {}: {error}", decision.id);
        }
    }

    /// A message the user sent in a Mono's chat answers a decision card only
    /// when it does so unambiguously; otherwise the app asks.
    pub(super) fn user_reply(&mut self, mono: &str, text: &str, cx: &mut Context<Self>) {
        let matched = locked(&self.store).and_then(|db| {
            if !coordinator::enabled(&db) {
                return Ok(ReplyMatch::None);
            }
            match db.existing_tasks().map_err(|e| e.to_string())? {
                Some(tasks) => tasks
                    .match_user_reply(mono, text)
                    .map_err(|e| e.to_string()),
                None => Ok(ReplyMatch::None),
            }
        });
        match matched {
            Ok(ReplyMatch::Resolves {
                decision,
                revision,
                answer,
            }) => {
                self.record_user_answer(mono, &decision, revision, &answer, cx);
            }
            // Asked once per decision: the first reply after its card that
            // does not answer it, not every later message to Mono.
            Ok(ReplyMatch::Unclear(open)) => {
                let open: Vec<String> = open
                    .into_iter()
                    .filter(|id| self.task_noted.insert(id.clone()))
                    .collect();
                if open.is_empty() {
                    return;
                }
                let note = format!(
                    "This message did not answer {}. Choose an option on the decision card, or start your reply with the decision id and a colon (for example `{}: …`).",
                    if open.len() == 1 {
                        format!("decision {}", open[0])
                    } else {
                        format!("the open decisions ({})", open.join(", "))
                    },
                    open[0]
                );
                if let Some(owner) = self.roster.get_mut(mono) {
                    owner.pending_reports.push(note);
                }
                self.save(cx);
            }
            Ok(ReplyMatch::None) => {}
            Err(error) => eprintln!("Decision reply: {error}"),
        }
    }

    /// A choice on a decision card: the user's unambiguous answer, which Mono
    /// also sees in its chat.
    pub(super) fn answer_user_decision(
        &mut self,
        mono: &str,
        decision: &str,
        revision: u64,
        answer: &str,
        cx: &mut Context<Self>,
    ) {
        if self.record_user_answer(mono, decision, revision, answer, cx) {
            self.enqueue(mono, format!("{decision}: {answer}"), Vec::new(), false, cx);
        }
    }

    fn record_user_answer(
        &mut self,
        mono: &str,
        decision: &str,
        revision: u64,
        answer: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let result = locked(&self.store).and_then(|db| {
            let tasks = db.tasks().map_err(|e| e.to_string())?;
            // Only this Mono's open user decisions take its user's answer.
            let open = tasks.user_decisions(mono).map_err(|e| e.to_string())?;
            if !open.iter().any(|(d, _)| d.id == decision) {
                return Err(format!("Decision {decision} is no longer waiting for you."));
            }
            tasks
                .answer_as(decision, revision, answer, Requester::User, now_ms())
                .map_err(|e| e.to_string())
        });
        match result {
            Ok(_) => {
                self.refresh_cards(cx);
                true
            }
            Err(error) => {
                self.error = Some(error);
                self.changed(cx);
                false
            }
        }
    }
}
