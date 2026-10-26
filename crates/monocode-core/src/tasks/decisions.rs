//! Worker questions routed to the owning Mono, Mono's answers and escalations
//! to the user, and the hand-off of each answer to the worker that asked.
//!
//! A question (from `tasks.worker.ask` or a provider-native question on an
//! attempt session) is a decision request plus one delivery to the Mono that
//! owns the task. Mono answers it or escalates it to the user; nothing
//! answers it by default. Every change bumps the decision's revision and
//! appends an event, so a stale writer cannot resolve a superseded request.
//!
//! Acknowledgment rule: a decision resolves at the app's receipt of the
//! hand-off to the current attempt's worker: the provider accepted the reply
//! to its question, the worker session accepted the answer as its next
//! message, or a replacement worker's brief that carried the answer was
//! confirmed started. Resolution is one revision-checked transaction, so it
//! happens once and every other path then finds it resolved.
use super::events::append;
use super::model::{Attempt, DecisionRequest, DecisionScope, DecisionState};
use super::review::{acknowledge, MONO_REVIEW};
use super::states::{apply, is_latest_attempt, AssignmentState, AttemptState, Requester, State};
use super::states::{Subject, TaskLifecycle};
use super::store::{event_for, guard, Key, TaskStore};
use super::worker::{settle, WorkerQuestion};
use super::{Assignment, TaskError};
use crate::agent::Question;
use crate::store::Store;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

/// Where worker questions wait for the owning Mono.
pub const MONO_DECISION: &str = "mono_decision";
/// Every destination whose deliveries reach the owning Mono's inbox.
pub const MONO_DESTINATIONS: [&str; 2] = [MONO_DECISION, MONO_REVIEW];
/// Where answers wait for their hand-off to the worker.
pub const WORKER: &str = "worker";
const ESCALATED: &str = "decision.escalated";
const PROVIDER_CONTEXT: &str = "The worker's provider asked this through its question tool; the answer goes back through the provider's reply.";

fn key(id: &str) -> Key<'_> {
    Key::new("native_task_decisions", "id=?", vec![id], "decision")
}

fn provider_event(decision: &str) -> String {
    format!("provider-question:{decision}")
}

/// Queues `decision` for the Mono that owns its task, inside the caller's
/// transaction. Idempotent by decision.
pub(super) fn queue_for_mono(conn: &Connection, decision: &str, now: u64) -> Result<(), TaskError> {
    TaskStore { conn }.enqueue_delivery(
        &format!("mono-decision:{decision}"),
        &format!("decision:{decision}@1"),
        MONO_DECISION,
        &format!("decision:{decision}"),
        now,
    )?;
    Ok(())
}

/// What Mono asks the user, shown in Mono's chat as a decision card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Escalation {
    /// The question as the user should read it; the worker's by default.
    pub question: Option<String>,
    pub why: String,
    pub recommendation: String,
    pub alternatives: Vec<String>,
}

/// An answer owed to the worker that asked.
#[derive(Clone, Debug, PartialEq)]
pub struct Handoff {
    pub decision: DecisionRequest,
    /// The worker's persisted session id.
    pub session_id: String,
    /// The provider-native question, when the worker asked through one.
    pub question: Option<Question>,
}

/// How a user's free-text reply in Mono's chat relates to open decisions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplyMatch {
    /// No user decision is open for this Mono.
    None,
    /// The reply answers exactly this decision.
    Resolves {
        decision: String,
        revision: u64,
        answer: String,
    },
    /// Decisions are open but the reply does not clearly answer one of them.
    Unclear(Vec<String>),
}

/// The text the worker receives with Mono's (or the user's) answer.
pub fn answer_message(decision: &DecisionRequest) -> String {
    let by = match decision.answered_by {
        Some(Requester::User) => "The user",
        _ => "Mono",
    };
    format!(
        "{by} answered your question (decision {id}).\nQuestion: {question}\nAnswer: {answer}\n\nContinue your assignment with this answer.",
        id = decision.id,
        question = decision.question,
        answer = decision.answer.as_deref().unwrap_or_default(),
    )
}

/// Routes a provider-native question from `session` to Mono when the session
/// is a current task attempt and the coordinator is on. Returns None (the
/// question stays with the user, as today) otherwise.
pub fn route_question(
    store: &Store,
    session: i64,
    question: &Question,
    now: u64,
) -> Result<Option<DecisionRequest>, TaskError> {
    if !super::coordinator::enabled(store) {
        return Ok(None);
    }
    let Some(tasks) = store.existing_tasks()? else {
        return Ok(None);
    };
    let Ok(bound) = store.persisted_session_id(session) else {
        return Ok(None);
    };
    let Some(attempt) = tasks.attempt_for_session(&bound)? else {
        return Ok(None);
    };
    let live = matches!(
        attempt.status,
        AttemptState::Running | AttemptState::Waiting
    );
    if !live || !is_latest_attempt(tasks.conn, Subject::Attempt(&attempt.id))? {
        return Ok(None);
    }
    let asked = WorkerQuestion {
        id: format!(
            "question-{}-{}",
            attempt.id,
            crate::mono::files::content_hash(&question.id)
        ),
        question: crate::provider::describe(question),
        context: PROVIDER_CONTEXT.into(),
        scope: DecisionScope::Assignment,
    };
    let original = crate::provider::question_json(question);
    tasks
        .ask_with(&attempt.id, &asked, Some(original), now)
        .map(Some)
}

/// Why the decision can no longer be answered, if it cannot.
fn superseded(conn: &Connection, decision: &DecisionRequest) -> Result<Option<String>, TaskError> {
    let lifecycle: Option<String> = conn
        .query_row(
            "SELECT lifecycle FROM native_tasks WHERE id=?1",
            [&decision.task_id],
            |row| row.get(0),
        )
        .optional()?;
    if matches!(
        lifecycle.as_deref().and_then(TaskLifecycle::parse),
        Some(TaskLifecycle::Cancelled | TaskLifecycle::Completed) | None
    ) {
        return Ok(Some("its task is finished".into()));
    }
    let Some(assignment) = &decision.assignment_id else {
        return Ok(None);
    };
    let status: Option<String> = conn
        .query_row(
            "SELECT status FROM native_task_assignments WHERE task_id=?1 AND id=?2",
            [&decision.task_id, assignment],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match status.as_deref().and_then(AssignmentState::parse) {
        Some(AssignmentState::Cancelled) | None => Some("its assignment was cancelled".into()),
        _ => None,
    })
}

/// Whether `assignment` depends on `on`, directly or through others.
fn depends_on(all: &[Assignment], assignment: &str, on: &str) -> bool {
    let mut seen = Vec::new();
    let mut pending = vec![assignment.to_owned()];
    while let Some(id) = pending.pop() {
        let Some(current) = all.iter().find(|a| a.id == id) else {
            continue;
        };
        for dependency in &current.depends_on {
            if dependency == on {
                return true;
            }
            if !seen.contains(dependency) {
                seen.push(dependency.clone());
                pending.push(dependency.clone());
            }
        }
    }
    false
}

fn describe_assignments(all: &[&Assignment]) -> String {
    if all.is_empty() {
        return "nothing else".into();
    }
    all.iter()
        .map(|a| format!("{} ({})", a.title, a.status.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

impl TaskStore<'_> {
    fn decision_or_missing(&self, id: &str) -> Result<DecisionRequest, TaskError> {
        self.decision(id)?
            .ok_or_else(|| TaskError::NotFound(format!("decision:{id}")))
    }

    /// The latest escalation card of the decision, if Mono asked the user.
    pub fn escalation(&self, id: &str) -> Result<Option<Value>, TaskError> {
        let payload: Option<String> = self
            .conn
            .query_row(
                "SELECT payload FROM native_task_events
                 WHERE kind=?1 AND json_extract(payload,'$.decisionId')=?2
                 ORDER BY seq DESC LIMIT 1",
                [ESCALATED, id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(payload.map(|p| serde_json::from_str(&p)).transpose()?)
    }

    /// The provider-native question a decision was routed from, if any.
    pub fn provider_question(&self, id: &str) -> Result<Option<Question>, TaskError> {
        let payload: Option<String> = self
            .conn
            .query_row(
                "SELECT payload FROM native_task_events WHERE id=?1",
                [provider_event(id)],
                |row| row.get(0),
            )
            .optional()?;
        Ok(payload
            .map(|p| serde_json::from_str::<Value>(&p))
            .transpose()?
            .and_then(|v| crate::provider::question_from_json(&v)))
    }

    /// Unresolved decisions of the tasks `mono` owns, oldest first.
    pub fn mono_decisions(&self, mono: &str) -> Result<Vec<DecisionRequest>, TaskError> {
        let mut statement = self.conn.prepare(
            "SELECT d.id FROM native_task_decisions d JOIN native_tasks t ON t.id=d.task_id
             WHERE t.mono_id=?1 AND d.state<>'resolved' ORDER BY d.created_at,d.rowid",
        )?;
        let ids = statement
            .query_map([mono], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.iter().map(|id| self.decision_or_missing(id)).collect()
    }

    /// Decisions of `task` already answered, newest first, for context.
    pub fn prior_decisions(&self, task: &str) -> Result<Vec<DecisionRequest>, TaskError> {
        let mut statement = self.conn.prepare(
            "SELECT id FROM native_task_decisions WHERE task_id=?1 AND state<>'open'
             ORDER BY rowid DESC LIMIT 10",
        )?;
        let ids = statement
            .query_map([task], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.iter().map(|id| self.decision_or_missing(id)).collect()
    }

    /// Open decisions of `mono`'s tasks that Mono escalated to the user, with
    /// their cards.
    pub fn user_decisions(&self, mono: &str) -> Result<Vec<(DecisionRequest, Value)>, TaskError> {
        let mut open = Vec::new();
        for decision in self.mono_decisions(mono)? {
            if decision.state != DecisionState::Open {
                continue;
            }
            if let Some(card) = self.escalation(&decision.id)? {
                open.push((decision, card));
            }
        }
        Ok(open)
    }

    /// Whether a user's free-text reply answers exactly one open user
    /// decision: it starts with the decision's id and a colon, or exactly one
    /// is open and the reply is one of its listed choices (the
    /// recommendation or an alternative). Anything else is unclear, and the
    /// app asks rather than guesses.
    pub fn match_user_reply(&self, mono: &str, text: &str) -> Result<ReplyMatch, TaskError> {
        let open = self.user_decisions(mono)?;
        if open.is_empty() {
            return Ok(ReplyMatch::None);
        }
        let reply = text.trim();
        for (decision, _) in &open {
            let named = reply
                .strip_prefix(decision.id.as_str())
                .and_then(|rest| rest.strip_prefix(':'))
                .map(str::trim)
                .filter(|answer| !answer.is_empty());
            if let Some(answer) = named {
                return Ok(ReplyMatch::Resolves {
                    decision: decision.id.clone(),
                    revision: decision.revision,
                    answer: answer.into(),
                });
            }
        }
        if let [(decision, card)] = open.as_slice() {
            let wanted = reply.trim_end_matches('.').trim();
            let choices = std::iter::once(&card["recommendation"])
                .chain(card["alternatives"].as_array().into_iter().flatten())
                .filter_map(Value::as_str);
            for choice in choices {
                if !wanted.is_empty() && choice.eq_ignore_ascii_case(wanted) {
                    return Ok(ReplyMatch::Resolves {
                        decision: decision.id.clone(),
                        revision: decision.revision,
                        answer: choice.into(),
                    });
                }
            }
        }
        Ok(ReplyMatch::Unclear(
            open.into_iter().map(|(d, _)| d.id).collect(),
        ))
    }

    /// The card that asks the user: the question, why it matters, the
    /// recommendation, alternatives, and what this pauses and what continues.
    fn card(
        &self,
        decision: &DecisionRequest,
        escalation: &Escalation,
        revision: u64,
    ) -> Result<Value, TaskError> {
        let all = self.assignments(&decision.task_id)?;
        let own = decision.assignment_id.as_deref().unwrap_or_default();
        let finished = |a: &Assignment| {
            matches!(
                a.status,
                AssignmentState::Accepted | AssignmentState::Cancelled
            )
        };
        let paused_by = |a: &Assignment| {
            a.id == own
                || match decision.scope {
                    DecisionScope::Assignment => false,
                    DecisionScope::Dependents => depends_on(&all, &a.id, own),
                    DecisionScope::Task => true,
                }
        };
        let (paused, continues): (Vec<&Assignment>, Vec<&Assignment>) = all
            .iter()
            .filter(|a| !finished(a))
            .partition(|a| paused_by(a));
        let task = self.task(&decision.task_id)?;
        let mut paused = describe_assignments(&paused);
        if decision.scope == DecisionScope::Task {
            paused = format!("New launches for the whole task; {paused}");
        }
        Ok(json!({
            "type": "decision",
            "decisionId": decision.id,
            "revision": revision,
            "taskId": decision.task_id,
            "taskTitle": task.map(|t| t.title).unwrap_or_default(),
            "question": escalation.question.clone().unwrap_or_else(|| decision.question.clone()),
            "why": escalation.why,
            "recommendation": escalation.recommendation,
            "alternatives": escalation.alternatives,
            "paused": paused,
            "continues": describe_assignments(&continues),
        }))
    }

    /// Mono asks the user. The decision stays Open, now answerable only by
    /// the user; returns the card for Mono's chat.
    pub fn escalate(
        &self,
        id: &str,
        expected_revision: u64,
        escalation: &Escalation,
        now: u64,
    ) -> Result<Value, TaskError> {
        let tx = self.begin()?;
        let decision = self.decision_or_missing(id)?;
        let key = key(id);
        let state = guard(&tx, &key, "state", expected_revision)?;
        if DecisionState::parse(&state) != Some(DecisionState::Open) {
            return Err(TaskError::InvalidState {
                subject: key.label,
                state,
            });
        }
        if self.escalation(id)?.is_some() {
            return Err(TaskError::Invalid(format!(
                "Decision {id} is already with the user."
            )));
        }
        if let Some(reason) = superseded(&tx, &decision)? {
            return Err(TaskError::Invalid(format!(
                "Decision {id} is superseded: {reason}."
            )));
        }
        let revision = key.bump(
            &tx,
            expected_revision,
            "answered_by=answered_by",
            vec![],
            now,
        )?;
        let card = self.card(&decision, escalation, revision)?;
        let mut event = event_for(
            &decision.task_id,
            &key.label,
            revision,
            ESCALATED,
            card.clone(),
        );
        event.assignment_id.clone_from(&decision.assignment_id);
        event.attempt_id.clone_from(&decision.attempt_id);
        append(&tx, &event, now)?;
        tx.commit()?;
        Ok(card)
    }

    /// Answers an Open decision as Mono, or as the user once Mono escalated
    /// it. A stale revision, a superseded request or the wrong answerer is
    /// refused; the answer then waits for its hand-off to the worker.
    pub fn answer_as(
        &self,
        id: &str,
        expected_revision: u64,
        answer: &str,
        by: Requester,
        now: u64,
    ) -> Result<u64, TaskError> {
        let tx = self.begin()?;
        let decision = self.decision_or_missing(id)?;
        let key = key(id);
        let state = guard(&tx, &key, "state", expected_revision)?;
        if DecisionState::parse(&state) != Some(DecisionState::Open) {
            return Err(TaskError::InvalidState {
                subject: key.label,
                state,
            });
        }
        if let Some(reason) = superseded(&tx, &decision)? {
            return Err(TaskError::Invalid(format!(
                "Decision {id} is superseded: {reason}."
            )));
        }
        let escalated = self.escalation(id)?.is_some();
        match (by, escalated) {
            (Requester::Mono, false) | (Requester::User, true) => {}
            (Requester::Mono, true) => {
                return Err(TaskError::Invalid(format!(
                    "Decision {id} is with the user; only the user answers it."
                )))
            }
            _ => {
                return Err(TaskError::Invalid(format!(
                    "Only Mono answers decision {id} unless it asks the user."
                )))
            }
        }
        // A provider question's answer must fit its parts before it is kept.
        if let Some(question) = self.provider_question(id)? {
            crate::provider::answers_from_text(&question, answer).map_err(TaskError::Invalid)?;
        }
        let who = by.as_str();
        let revision = key.bump(
            &tx,
            expected_revision,
            "state='answered',answer=?,answered_by=?",
            vec![&answer, &who],
            now,
        )?;
        let payload = json!({"answer": answer, "by": by});
        let mut event = event_for(
            &decision.task_id,
            &key.label,
            revision,
            "decision.answered",
            payload,
        );
        event.assignment_id.clone_from(&decision.assignment_id);
        event.attempt_id.clone_from(&decision.attempt_id);
        let answered = append(&tx, &event, now)?;
        TaskStore { conn: &tx }.enqueue_delivery(
            &format!("answer:{id}"),
            &answered.id,
            WORKER,
            &format!("decision:{id}"),
            now,
        )?;
        tx.commit()?;
        Ok(revision)
    }

    /// Answered decisions whose worker is live: the asking attempt is still
    /// its assignment's latest, Running or Waiting, with a session.
    pub fn pending_handoffs(&self) -> Result<Vec<Handoff>, TaskError> {
        let mut statement = self.conn.prepare(
            "SELECT id FROM native_task_decisions WHERE state='answered' AND attempt_id IS NOT NULL
             ORDER BY updated_at,rowid",
        )?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut handoffs = Vec::new();
        for id in ids {
            let decision = self.decision_or_missing(&id)?;
            let Some(attempt) = self.attempt(decision.attempt_id.as_deref().unwrap_or_default())?
            else {
                continue;
            };
            let live = matches!(
                attempt.status,
                AttemptState::Running | AttemptState::Waiting
            );
            let Some(session_id) = attempt.session_id.clone().filter(|_| live) else {
                continue;
            };
            if !is_latest_attempt(self.conn, Subject::Attempt(&attempt.id))? {
                continue;
            }
            let question = self.provider_question(&id)?;
            handoffs.push(Handoff {
                decision,
                session_id,
                question,
            });
        }
        Ok(handoffs)
    }

    /// Records the hand-off of an answer to `attempt`'s worker (`via` names
    /// the channel) and resumes it. Returns false, changing nothing, when
    /// the decision is no longer Answered at `expected_revision` for that
    /// attempt: an earlier hand-off already resolved it, or it changed.
    pub fn resolve_handoff(
        &self,
        id: &str,
        expected_revision: u64,
        attempt: &str,
        via: &str,
        now: u64,
    ) -> Result<bool, TaskError> {
        let tx = self.begin()?;
        let Some(decision) = self.decision(id)? else {
            return Ok(false);
        };
        let current = decision.state == DecisionState::Answered
            && decision.revision == expected_revision
            && decision.attempt_id.as_deref() == Some(attempt);
        if !current {
            return Ok(false);
        }
        resolve(&tx, &decision, attempt, via, now)?;
        tx.commit()?;
        Ok(true)
    }

    /// Why `assignment` may not launch now, if a decision holds it: its own
    /// open question; a task-wide question asked elsewhere in the task; or a
    /// `dependents` question on an assignment it depends on. Answered
    /// questions still hold others until their worker has the answer. A
    /// question whose asking assignment failed or was cancelled holds
    /// nothing: no worker of it will take the answer until it is queued
    /// again.
    pub fn launch_blocked(
        &self,
        task: &str,
        assignment: &Assignment,
        all: &[Assignment],
    ) -> Result<Option<String>, TaskError> {
        let mut statement = self.conn.prepare(
            "SELECT d.id,d.scope,d.state,COALESCE(d.assignment_id,'') FROM native_task_decisions d
             WHERE d.task_id=?1 AND d.state<>'resolved'
               AND NOT EXISTS(SELECT 1 FROM native_task_assignments a
                 WHERE a.task_id=d.task_id AND a.id=d.assignment_id
                   AND a.status IN ('cancelled','failed'))
             ORDER BY d.rowid",
        )?;
        let rows = statement
            .query_map([task], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (id, scope, state, asker) in rows {
            let held = if asker == assignment.id {
                state == DecisionState::Open.as_str()
            } else {
                match DecisionScope::parse(&scope) {
                    Some(DecisionScope::Task) => true,
                    Some(DecisionScope::Dependents) => depends_on(all, &assignment.id, &asker),
                    _ => false,
                }
            };
            if held {
                return Ok(Some(format!("Waiting for decision {id}.")));
            }
        }
        Ok(None)
    }
}

/// Resolves an Answered decision handed to `attempt` and resumes the attempt
/// and its assignment when no other question of theirs is open.
pub(super) fn resolve(
    conn: &Connection,
    decision: &DecisionRequest,
    attempt: &str,
    via: &str,
    now: u64,
) -> Result<(), TaskError> {
    let key = key(&decision.id);
    let revision = key.bump(conn, decision.revision, "state='resolved'", vec![], now)?;
    let payload = json!({"attemptId": attempt, "via": via});
    let mut event = event_for(
        &decision.task_id,
        &key.label,
        revision,
        "decision.resolved",
        payload,
    );
    event.assignment_id.clone_from(&decision.assignment_id);
    event.attempt_id = Some(attempt.into());
    append(conn, &event, now)?;
    let delivery = format!("answer:{}", decision.id);
    let owed: Option<String> = conn
        .query_row(
            "SELECT state FROM native_task_deliveries WHERE id=?1",
            [&delivery],
            |row| row.get(0),
        )
        .optional()?;
    if owed.is_some_and(|state| state != "acknowledged") {
        acknowledge(conn, &delivery, now)?;
    }
    let tasks = TaskStore { conn };
    if !is_latest_attempt(conn, Subject::Attempt(attempt))? {
        return Ok(());
    }
    let open: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM native_task_decisions WHERE attempt_id=?1 AND state='open')",
        [attempt],
        |row| row.get(0),
    )?;
    if open {
        return Ok(());
    }
    let waiting = State::Attempt(AttemptState::Waiting);
    if tasks.state(Subject::Attempt(attempt), waiting)? == waiting {
        let running = State::Attempt(AttemptState::Running);
        settle(
            conn,
            Subject::Attempt(attempt),
            running,
            Requester::Coordinator,
            now,
        )?;
    }
    if let Some(assignment) = &decision.assignment_id {
        let subject = Subject::Assignment {
            task: &decision.task_id,
            id: assignment,
        };
        let deciding = State::Assignment(AssignmentState::WaitingForDecision);
        if tasks.state(subject, deciding)? == deciding {
            let revision = tasks.revision(subject)?;
            let running = State::Assignment(AssignmentState::Running);
            match apply(
                conn,
                subject,
                running,
                Requester::Coordinator,
                revision,
                now,
            ) {
                Ok(_) | Err(TaskError::Precondition(_)) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

/// Moves the assignment's unresolved decisions from earlier attempts to the
/// replacement `attempt`, so a crashed worker's question and its answer
/// reach the worker that takes over. Inside the caller's transaction.
pub(super) fn inherit(conn: &Connection, attempt: &Attempt, now: u64) -> Result<(), TaskError> {
    let mut statement = conn.prepare(
        "SELECT id,revision,COALESCE(attempt_id,'') FROM native_task_decisions
         WHERE task_id=?1 AND assignment_id=?2 AND state<>'resolved'
           AND COALESCE(attempt_id,'')<>?3",
    )?;
    let rows = statement
        .query_map(
            [&attempt.task_id, &attempt.assignment_id, &attempt.id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    super::store::read_ms(row, 1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    for (id, revision, from) in rows {
        let key = key(&id);
        let to = attempt.id.as_str();
        let revision = key.bump(conn, revision, "attempt_id=?", vec![&to], now)?;
        let payload = json!({"from": from, "to": to});
        let mut event = event_for(
            &attempt.task_id,
            &key.label,
            revision,
            "decision.inherited",
            payload,
        );
        event.assignment_id = Some(attempt.assignment_id.clone());
        event.attempt_id = Some(attempt.id.clone());
        append(conn, &event, now)?;
    }
    Ok(())
}

/// The decisions a (replacement) attempt's brief carries: every decision of
/// its assignment, oldest first.
pub(super) fn assignment_decisions(
    tasks: &TaskStore<'_>,
    task: &str,
    assignment: &str,
) -> Result<Vec<DecisionRequest>, TaskError> {
    let mut statement = tasks.conn.prepare(
        "SELECT id FROM native_task_decisions WHERE task_id=?1 AND assignment_id=?2 ORDER BY rowid",
    )?;
    let ids = statement
        .query_map([task, assignment], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    ids.iter().map(|id| tasks.decision_or_missing(id)).collect()
}

#[cfg(test)]
#[path = "decisions_tests.rs"]
pub(super) mod tests;

#[cfg(test)]
#[path = "decisions_launch_tests.rs"]
mod launch_tests;
