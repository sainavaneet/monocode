//! What a worker attempt reports: questions, its result claim, and blockers.
//!
//! The attempt is always the one bound to the caller's session; nothing here
//! takes an identity from what a worker writes. Each report runs in one
//! transaction with the transitions it asks for, checked against the table.
use super::events::{append, NewEvent};
use super::model::{Attempt, DecisionRequest, DecisionScope, NewDecision};
use super::states::{
    apply, is_latest_attempt, state_enum, AssignmentState, AttemptState, Requester, State, Subject,
};
use super::store::{insert_decision, TaskStore};
use super::TaskError;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;

state_enum!(
    /// What the worker says happened. Either way it is a claim to review.
    ClaimOutcome {
        Succeeded => "succeeded",
        Failed => "failed",
    }
);

/// A worker's result. It never completes anything by itself: a succeeded
/// claim moves the assignment to NeedsReview, and only review accepts it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Claim {
    pub outcome: ClaimOutcome,
    /// What changed.
    pub summary: String,
    /// The worktree, checkout or branch the work is in.
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub commits: Vec<String>,
    /// Checks the worker ran, with their results.
    #[serde(default)]
    pub validation: Vec<String>,
    /// References to logs, outputs or files that support the claim.
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub remaining_work: Vec<String>,
    #[serde(default)]
    pub limitations: Vec<String>,
}

/// A worker's question for Mono. `id` is derived from the request, so a
/// retried request finds the question it already asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerQuestion {
    pub id: String,
    pub question: String,
    pub context: String,
    pub scope: DecisionScope,
}

/// The claim is an event with a stable id per attempt, so an attempt has at
/// most one and a replayed result finds it.
pub(super) fn claim_event(attempt: &str) -> String {
    format!("claim:{attempt}")
}

pub(super) fn event(
    attempt: &Attempt,
    id: String,
    kind: &str,
    payload: serde_json::Value,
) -> NewEvent {
    NewEvent {
        id,
        task_id: attempt.task_id.clone(),
        assignment_id: Some(attempt.assignment_id.clone()),
        attempt_id: Some(attempt.id.clone()),
        kind: kind.into(),
        payload,
    }
}

fn payload_of(conn: &Connection, event_id: &str) -> Result<Option<String>, TaskError> {
    Ok(conn
        .query_row(
            "SELECT payload FROM native_task_events WHERE id=?1",
            [event_id],
            |row| row.get(0),
        )
        .optional()?)
}

/// Moves `subject` to `to` unless it is already there.
pub(super) fn settle(
    conn: &Connection,
    subject: Subject<'_>,
    to: State,
    requester: Requester,
    now: u64,
) -> Result<(), TaskError> {
    let tasks = TaskStore { conn };
    if tasks.state(subject, to)? == to {
        return Ok(());
    }
    let revision = tasks.revision(subject)?;
    apply(conn, subject, to, requester, revision, now).map(|_| ())
}

impl TaskStore<'_> {
    /// The newest attempt bound to `session_id`, if the session is a worker.
    pub fn attempt_for_session(&self, session_id: &str) -> Result<Option<Attempt>, TaskError> {
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM native_task_attempts WHERE session_id=?1
                 ORDER BY rowid DESC LIMIT 1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match id {
            Some(id) => self.attempt(&id)?,
            None => None,
        })
    }

    /// The decisions an attempt asked for, oldest first.
    pub fn attempt_decisions(&self, attempt_id: &str) -> Result<Vec<DecisionRequest>, TaskError> {
        let mut statement = self.conn.prepare(
            "SELECT id FROM native_task_decisions WHERE attempt_id=?1 ORDER BY created_at,rowid",
        )?;
        let ids = statement
            .query_map([attempt_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.iter()
            .filter_map(|id| self.decision(id).transpose())
            .collect()
    }

    /// The result an attempt claimed, if it reported one.
    pub fn claim(&self, attempt_id: &str) -> Result<Option<Claim>, TaskError> {
        payload_of(self.conn, &claim_event(attempt_id))?
            .map(|payload| serde_json::from_str(&payload).map_err(TaskError::from))
            .transpose()
    }

    /// The attempt, if it is still its assignment's newest one. A replaced
    /// attempt must not hold up or redirect the current run.
    fn current_attempt(&self, id: &str) -> Result<Attempt, TaskError> {
        let attempt = self
            .attempt(id)?
            .ok_or_else(|| TaskError::NotFound(format!("attempt:{id}")))?;
        if !is_latest_attempt(self.conn, Subject::Attempt(id))? {
            return Err(TaskError::Invalid(
                "This attempt was replaced by a newer attempt for its assignment.".into(),
            ));
        }
        Ok(attempt)
    }

    /// Persists the worker's question, moves the attempt to Waiting and its
    /// assignment to WaitingForDecision. Asking again with the same id
    /// returns the stored request.
    pub fn ask(
        &self,
        attempt_id: &str,
        question: &WorkerQuestion,
        now: u64,
    ) -> Result<DecisionRequest, TaskError> {
        let tx = self.begin()?;
        let attempt = self.current_attempt(attempt_id)?;
        if let Some(existing) = self.decision(&question.id)? {
            let same = existing.attempt_id.as_deref() == Some(attempt_id)
                && existing.question == question.question
                && existing.context == question.context
                && existing.scope == question.scope;
            return if same {
                Ok(existing)
            } else {
                Err(TaskError::Invalid(
                    "This request ID was already used for a different question.".into(),
                ))
            };
        }
        let new = NewDecision {
            id: question.id.clone(),
            task_id: attempt.task_id.clone(),
            assignment_id: Some(attempt.assignment_id.clone()),
            attempt_id: Some(attempt.id.clone()),
            question: question.question.clone(),
            context: question.context.clone(),
            scope: question.scope,
            asked_by: Requester::Worker,
        };
        insert_decision(&tx, &new, now)?;
        let waiting = State::Attempt(AttemptState::Waiting);
        settle(
            &tx,
            Subject::Attempt(attempt_id),
            waiting,
            Requester::Worker,
            now,
        )?;
        let assignment = Subject::Assignment {
            task: &attempt.task_id,
            id: &attempt.assignment_id,
        };
        let deciding = State::Assignment(AssignmentState::WaitingForDecision);
        settle(&tx, assignment, deciding, Requester::Worker, now)?;
        tx.commit()?;
        self.decision(&question.id)?
            .ok_or_else(|| TaskError::NotFound(format!("decision:{}", question.id)))
    }

    /// Records the attempt's result claim. A succeeded claim moves the
    /// attempt to Succeeded (and its assignment to NeedsReview when it is the
    /// latest attempt). Any other claim only records it: the attempt stays
    /// Running or Waiting until the coordinator classifies it on its own
    /// authority, since the table does not let a worker fail an attempt.
    /// Returns the claim and whether this call recorded it; the same claim
    /// again returns the stored one, and a different one is refused.
    pub fn claim_result(
        &self,
        attempt_id: &str,
        claim: &Claim,
        now: u64,
    ) -> Result<(Claim, bool), TaskError> {
        let tx = self.begin()?;
        if let Some(existing) = self.claim(attempt_id)? {
            return if existing == *claim {
                Ok((existing, false))
            } else {
                Err(TaskError::Invalid(
                    "This attempt already reported a different result; a result is final.".into(),
                ))
            };
        }
        let attempt = self
            .attempt(attempt_id)?
            .ok_or_else(|| TaskError::NotFound(format!("attempt:{attempt_id}")))?;
        if !matches!(
            attempt.status,
            AttemptState::Running | AttemptState::Waiting
        ) {
            return Err(TaskError::InvalidState {
                subject: format!("attempt:{attempt_id}"),
                state: attempt.status.as_str().into(),
            });
        }
        let payload = serde_json::to_value(claim)?;
        let claimed = event(&attempt, claim_event(attempt_id), "attempt.claim", payload);
        append(&tx, &claimed, now)?;
        if claim.outcome == ClaimOutcome::Succeeded {
            let succeeded = State::Attempt(AttemptState::Succeeded);
            let subject = Subject::Attempt(attempt_id);
            apply(
                &tx,
                subject,
                succeeded,
                Requester::Worker,
                attempt.revision,
                now,
            )?;
        }
        tx.commit()?;
        Ok((claim.clone(), true))
    }

    /// Records why the worker cannot continue and moves the attempt to
    /// Waiting. The assignment stays Running until the coordinator or Mono
    /// decides what to do. `key` identifies the report, so a retry is one.
    pub fn report_blocked(
        &self,
        attempt_id: &str,
        key: &str,
        reason: &str,
        now: u64,
    ) -> Result<Attempt, TaskError> {
        let tx = self.begin()?;
        let attempt = self.current_attempt(attempt_id)?;
        let id = format!("blocked:{attempt_id}:{key}");
        let payload = json!({ "reason": reason });
        if let Some(existing) = payload_of(&tx, &id)? {
            if serde_json::from_str::<serde_json::Value>(&existing)? != payload {
                return Err(TaskError::Invalid(
                    "This request ID was already used for a different blocker.".into(),
                ));
            }
            return Ok(attempt);
        }
        let waiting = State::Attempt(AttemptState::Waiting);
        settle(
            &tx,
            Subject::Attempt(attempt_id),
            waiting,
            Requester::Worker,
            now,
        )?;
        append(&tx, &event(&attempt, id, "attempt.blocked", payload), now)?;
        tx.commit()?;
        self.attempt(attempt_id)?
            .ok_or_else(|| TaskError::NotFound(format!("attempt:{attempt_id}")))
    }
}
