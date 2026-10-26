//! A provider question from a task worker's session goes to the Mono that
//! owns the task, not to the user, while the task coordinator is on. The
//! question stays pending here until the app hands Mono's (or the user's)
//! answer back through the provider's own reply. Other sessions' questions,
//! and every permission request, keep their existing path.
use super::*;
use monocode_core::agent::Question;

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
                Ok(Some(decision)) => self.task_questions.push((decision.id, question.id)),
                Ok(None) => remaining.push(question),
                Err(error) => {
                    eprintln!("Task question for session {}: {error}", self.summary.id);
                    remaining.push(question);
                }
            }
        }
        if remaining.is_empty() && !self.task_questions.is_empty() {
            self.status = "Waiting for Mono's answer".into();
        }
        remaining
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
    pub(super) fn forget_task_question(&mut self, question: &str) {
        self.task_questions.retain(|(_, id)| id != question);
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
