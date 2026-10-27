//! A provider question from a task worker's session goes to the Mono that
//! owns the task, not to the user, while the task coordinator is on. The
//! question stays pending here until the app hands Mono's (or the user's)
//! answer back through the provider's own reply. Other sessions' questions,
//! and every permission request, keep their existing path. A question that
//! cannot be routed because of a storage error fails closed: it is kept open
//! (never auto-skipped) and routed again on the next task pass.
use super::*;
use monocode_core::agent::Question;
use monocode_core::tasks::{NewEvent, coordinator};

/// The task event recording that a worker's question could not be routed.
pub(crate) const UNROUTED: &str = "task.question_unrouted";

/// Whether a question whose routing failed belongs to a task worker and so
/// must stay open: the coordinator is on and the session is (or, when that
/// cannot be read, may be) a task attempt's. Records the failure on the
/// attempt's task when it is known.
fn hold_unrouted(
    store: &monocode_core::store::Store,
    session: i64,
    question: &Question,
    error: &str,
    now: u64,
) -> bool {
    if !coordinator::enabled(store) {
        return false;
    }
    let attempt = (|| {
        let Some(tasks) = store.existing_tasks().map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let Ok(bound) = store.persisted_session_id(session) else {
            return Ok(None);
        };
        let attempt = tasks
            .attempt_for_session(&bound)
            .map_err(|e| e.to_string())?;
        let Some(attempt) = attempt else {
            return Ok(None);
        };
        let event = NewEvent {
            id: format!(
                "question_unrouted:{}:{}",
                attempt.id,
                monocode_core::mono::files::content_hash(&question.id)
            ),
            task_id: attempt.task_id.clone(),
            assignment_id: Some(attempt.assignment_id.clone()),
            attempt_id: Some(attempt.id.clone()),
            kind: UNROUTED.into(),
            payload: serde_json::json!({ "question": question.id, "error": error }),
        };
        tasks.append_event(&event, now).map_err(|e| e.to_string())?;
        Ok::<_, String>(Some(attempt))
    })();
    match attempt {
        Ok(found) => found.is_some(),
        // Unknown: fail closed rather than let it auto-resolve.
        Err(error) => {
            eprintln!("Task question for session {session}: {error}");
            true
        }
    }
}

impl Conversation {
    /// Persists each question of a task attempt session as a decision for
    /// its Mono; returns the ones that stay with the user.
    pub(crate) fn route_task_questions(&mut self, questions: Vec<Question>) -> Vec<Question> {
        if questions.is_empty() || self.settings.mono.is_some() {
            return questions;
        }
        let Ok(store) = locked(&self.store) else {
            return questions;
        };
        let now = monocode_core::time::now_ms();
        let mut remaining = Vec::new();
        for question in questions {
            let routed = monocode_core::tasks::decisions::route_question(
                &store,
                self.summary.id,
                &question,
                now,
            );
            match routed {
                Ok(Some(decision)) => {
                    // No timeout and no inferred answer: a routed question
                    // never auto-resolves; it waits for Mono or the user.
                    if let Some(controls) = &self.controls {
                        let _ = controls.send(Control {
                            id: question.id.clone(),
                            decision: Decision::KeepOpen,
                        });
                    }
                    self.task_questions.push((decision.id, question.id));
                }
                Ok(None) => remaining.push(question),
                Err(error) => {
                    eprintln!("Task question for session {}: {error}", self.summary.id);
                    let error = error.to_string();
                    if hold_unrouted(&store, self.summary.id, &question, &error, now) {
                        if let Some(controls) = &self.controls {
                            let _ = controls.send(Control {
                                id: question.id.clone(),
                                decision: Decision::KeepOpen,
                            });
                        }
                        self.unrouted_questions.push(question);
                    } else {
                        remaining.push(question);
                    }
                }
            }
        }
        let held = !self.task_questions.is_empty() || !self.unrouted_questions.is_empty();
        if remaining.is_empty() && held {
            self.status = "Waiting for Mono's answer".into();
        }
        remaining
    }

    /// Routes again the questions a storage error held back: routed ones go
    /// to Mono, ones that turn out not to be a worker's go to the user.
    pub(crate) fn retry_task_questions(&mut self, cx: &mut Context<Self>) {
        if self.unrouted_questions.is_empty() {
            return;
        }
        if !self.busy || self.controls.is_none() {
            // The provider no longer waits on them.
            self.unrouted_questions.clear();
            return;
        }
        let questions = std::mem::take(&mut self.unrouted_questions);
        let remaining = self.route_task_questions(questions);
        self.show_user_questions(remaining, cx);
        cx.notify();
    }

    /// Shows provider questions to the user, one card each.
    pub(crate) fn show_user_questions(
        &mut self,
        questions: Vec<monocode_core::agent::Question>,
        cx: &mut Context<Self>,
    ) {
        for request in questions {
            let card = cx.new(|cx| crate::questions::QuestionCard::new(request, cx));
            let events = cx.subscribe(&card, |this, _, reply: &crate::questions::Answered, cx| {
                if this
                    .questions
                    .front()
                    .is_none_or(|(card, _)| card.read(cx).id() != reply.id)
                {
                    return;
                }
                let before = this.needs_approval();
                let decision = reply
                    .answers
                    .clone()
                    .map_or(Decision::Deny, Decision::Answer);
                if this.controls.as_ref().is_none_or(|controls| {
                    controls
                        .send(Control {
                            id: reply.id.clone(),
                            decision,
                        })
                        .is_err()
                }) {
                    this.status = "The provider is no longer waiting for this answer".into();
                    cx.notify();
                    return;
                }
                if let Some((card, _)) = this.questions.pop_front() {
                    this.question_overlay.release(card, cx);
                }
                this.status = if !this.approvals.is_empty() {
                    "Waiting for approval"
                } else if !this.questions.is_empty() {
                    "Waiting for your answer"
                } else {
                    "Working…"
                }
                .into();
                if before != this.needs_approval() {
                    cx.emit(SessionChanged);
                }
                cx.notify();
            });
            let keep_open = cx.subscribe(
                &card,
                |this, _, request: &crate::questions::KeptOpen, cx| {
                    if let Some(controls) = &this.controls {
                        let _ = controls.send(Control {
                            id: request.id.clone(),
                            decision: Decision::KeepOpen,
                        });
                    }
                    cx.emit(SessionChanged);
                    cx.notify();
                },
            );
            self.questions.push_back((card, vec![events, keep_open]));
            self.status = "Waiting for your answer".into();
        }
    }

    /// Whether the provider is still waiting on its question for `decision`.
    pub(crate) fn task_question_waiting(&self, decision: &str) -> bool {
        self.busy
            && self.controls.is_some()
            && self.task_questions.iter().any(|(d, _)| d == decision)
    }

    /// The provider-native question routed as `decision`, while it waits.
    pub(crate) fn task_question_id(&self, decision: &str) -> Option<&str> {
        self.task_questions
            .iter()
            .find(|(d, _)| d == decision)
            .map(|(_, id)| id.as_str())
    }

    /// Sends the answer through the provider's reply path.
    pub(crate) fn answer_task_question(
        &mut self,
        decision: &str,
        answers: Vec<Vec<String>>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let index = self
            .task_questions
            .iter()
            .position(|(d, _)| d == decision)
            .ok_or("The worker is not waiting on this question")?;
        let id = self.task_questions[index].1.clone();
        let sent = self.controls.as_ref().is_some_and(|controls| {
            controls
                .send(Control {
                    id,
                    decision: Decision::Answer(answers),
                })
                .is_ok()
        });
        if !sent {
            return Err("The provider is no longer waiting for this answer".into());
        }
        self.task_questions.remove(index);
        if self.task_questions.is_empty() && self.questions.is_empty() && self.approvals.is_empty()
        {
            self.status = "Working…".into();
        }
        cx.notify();
        Ok(())
    }

    /// A question the provider withdrew (or that expired) is not pending.
    pub(crate) fn forget_task_question(&mut self, question: &str) {
        let before = self.task_questions.len() + self.unrouted_questions.len();
        self.task_questions.retain(|(_, id)| id != question);
        self.unrouted_questions.retain(|q| q.id != question);
        let after = self.task_questions.len() + self.unrouted_questions.len();
        if after != before && after == 0 && self.status == "Waiting for Mono's answer" {
            self.status = if self.busy { "Working…" } else { "Ready" }.into();
        }
    }

    #[cfg(test)]
    pub(crate) fn test_status(&self) -> &str {
        &self.status
    }

    /// Test support: a provider turn waiting on its questions, whose replies
    /// arrive on the returned channel.
    #[cfg(test)]
    pub(crate) fn test_provider_waiting(&mut self) -> std::sync::mpsc::Receiver<Control> {
        let (controls, replies) = std::sync::mpsc::channel();
        self.controls = Some(controls);
        self.busy = true;
        replies
    }
}
