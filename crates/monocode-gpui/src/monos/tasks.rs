//! Mono as development lead, in the app: Mono's task actions, the task inbox
//! that delivers worker questions and review hand-offs to Mono's chat, the
//! hand-off of answers to workers, and the user's replies to decision cards.
//! Everything here does nothing unless `native-task-coordinator` is enabled.
use super::host::Effect;
use super::*;
use monocode_core::app_access as app;
use monocode_core::mono::bridge::Call;
use monocode_core::tasks::decisions::{self, Handoff, ReplyMatch};
use monocode_core::tasks::{Requester, coordinator, inbox};
use monocode_core::time::now_ms;

/// What one pass found: per Mono the batch to deliver, and answers owed to
/// workers with their session handles.
type Pass = (Vec<(String, inbox::Batch)>, Vec<(i64, Handoff)>);

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
                let db = locked(&store)?;
                let value = app::mono_task_action(&db, &id, &action, &input, now_ms())?;
                // An escalation's card goes into the Mono's chat.
                let effect = value
                    .get("card")
                    .cloned()
                    .map_or(Effect::None, Effect::Card);
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
            for (mono, session) in monos {
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
            Ok(Some((batches, handoffs)))
        });
        cx.spawn(async move |this, cx| {
            let pass = work.await;
            let _ = this.update(cx, |this, cx| {
                this.pumping_tasks = false;
                match pass {
                    Ok(Some((batches, handoffs))) => {
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

    /// Queues the batch in the Mono's outbox under its own id, once. The
    /// outbox starts an internal message only when the chat is idle, so a
    /// task event never starts a second turn on Mono's conversation.
    fn deliver_batch(&mut self, mono: &str, batch: inbox::Batch, cx: &mut Context<Self>) {
        let delivered = self
            .chats
            .get(mono)
            .is_some_and(|v| v.read(cx).mono_delivered().contains(&batch.id));
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
                let answers = monocode_core::provider::answers_from_text(question, &answer);
                let id = decision.id.clone();
                view.update(cx, |v, cx| v.answer_task_question(&id, answers, cx))
                    .map(|_| "provider_reply")
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
            Ok(ReplyMatch::Unclear(open)) => {
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
            Ok(_) => true,
            Err(error) => {
                self.error = Some(error);
                self.changed(cx);
                false
            }
        }
    }
}
