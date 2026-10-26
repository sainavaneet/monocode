//! The owning Mono's task inbox: worker questions and review hand-offs,
//! delivered to Mono's conversation as one message at a time.
//!
//! Pending deliveries for a Mono (destinations in
//! [`MONO_DESTINATIONS`](super::decisions::MONO_DESTINATIONS)) are gathered
//! into a batch, marked Delivered and recorded as one `mono.inbox` event in a
//! single transaction. The app queues the batch in Mono's outbox under the
//! batch id; Mono's outbox only starts a turn when the conversation is idle.
//! Until Mono's transcript has the message (its `mono-delivered` receipt),
//! the same batch is returned again and no new one is made, so at most one
//! task message is in flight per Mono and a crash before the outbox was saved
//! re-sends the same batch rather than losing or duplicating it.
use super::decisions::{MONO_DECISION, MONO_DESTINATIONS};
use super::events::{append, NewEvent};
use super::findings::Finding;
use super::review::acknowledge;
use super::{DecisionRequest, DecisionState, PendingDelivery, TaskError, TaskStore};
use crate::store::Store;
use rusqlite::OptionalExtension;
use serde_json::{json, Value};

const INBOX: &str = "mono.inbox";
/// Caps one message; the rest waits for the next batch.
const MAX_PER_BATCH: usize = 20;
const HOW: &str = "Handle each item with the tasks.* app actions. Answer routine technical questions yourself from the request, prior decisions, project conventions, memory and your authorized scope; ask the user only when it needs them (escalate), and never answer for the user.";

/// One message for Mono carrying several task events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    /// Also the id of the Mono outbox message, so it is queued once.
    pub id: String,
    pub mono_id: String,
    pub message: String,
    pub deliveries: Vec<String>,
}

fn batch_of(payload: &Value) -> Option<Batch> {
    Some(Batch {
        id: payload["batchId"].as_str()?.into(),
        mono_id: payload["monoId"].as_str()?.into(),
        message: payload["message"].as_str()?.into(),
        deliveries: payload["deliveries"]
            .as_array()?
            .iter()
            .filter_map(|d| d.as_str().map(str::to_owned))
            .collect(),
    })
}

/// Batches handed to `mono` whose deliveries Mono has not acknowledged.
fn unacknowledged(tasks: &TaskStore<'_>, mono: &str) -> Result<Vec<Batch>, TaskError> {
    let mut statement = tasks.conn.prepare(
        "SELECT payload FROM native_task_events WHERE kind=?1
           AND json_extract(payload,'$.monoId')=?2 ORDER BY seq",
    )?;
    let payloads = statement
        .query_map([INBOX, mono], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut open = Vec::new();
    for payload in payloads {
        let Some(batch) = batch_of(&serde_json::from_str(&payload)?) else {
            continue;
        };
        let mut waiting = false;
        for id in &batch.deliveries {
            let state: Option<String> = tasks
                .conn
                .query_row(
                    "SELECT state FROM native_task_deliveries WHERE id=?1",
                    [id],
                    |row| row.get(0),
                )
                .optional()?;
            waiting |= state.as_deref() == Some("delivered");
        }
        if waiting {
            open.push(batch);
        }
    }
    Ok(open)
}

/// Pending deliveries owed to `mono`, oldest first.
fn owed(tasks: &TaskStore<'_>, mono: &str) -> Result<Vec<PendingDelivery>, TaskError> {
    let mut all = Vec::new();
    for destination in MONO_DESTINATIONS {
        for delivery in tasks.pending_deliveries(destination)? {
            let owner: Option<Option<String>> = tasks
                .conn
                .query_row(
                    "SELECT t.mono_id FROM native_task_events e JOIN native_tasks t ON t.id=e.task_id
                     WHERE e.id=?1",
                    [&delivery.event_id],
                    |row| row.get(0),
                )
                .optional()?;
            if owner.flatten().as_deref() == Some(mono) {
                all.push(delivery);
            }
        }
    }
    all.sort_by_key(|d| d.created_at);
    Ok(all)
}

/// The batch Mono should receive now: the one already handed out and not yet
/// in Mono's transcript, else a new one of everything pending, else None.
/// Nothing happens unless the task coordinator is enabled.
pub fn next_batch(store: &Store, mono: &str, now: u64) -> Result<Option<Batch>, TaskError> {
    if !super::coordinator::enabled(store) {
        return Ok(None);
    }
    let tasks = store.tasks()?;
    if let Some(batch) = unacknowledged(&tasks, mono)?.into_iter().next() {
        return Ok(Some(batch));
    }
    let tx = tasks.begin()?;
    let mut items = Vec::new();
    let mut deliveries = Vec::new();
    let mut task_id = None;
    for delivery in owed(&tasks, mono)? {
        if deliveries.len() >= MAX_PER_BATCH {
            break;
        }
        match describe(&tasks, &delivery)? {
            Some((task, text)) => {
                task_id.get_or_insert(task);
                items.push(text);
                tasks.record_on(&delivery, now)?;
                deliveries.push(delivery.id);
            }
            // Answered or withdrawn before Mono saw it: nothing to say.
            None => acknowledge(&tx, &delivery.id, now)?,
        }
    }
    let Some(task_id) = task_id else {
        tx.commit()?;
        return Ok(None);
    };
    let id = format!("task-inbox-{}", crate::mono::new_id());
    let numbered: Vec<String> = items
        .iter()
        .enumerate()
        .map(|(n, text)| format!("{}. {text}", n + 1))
        .collect();
    let message = format!("[Task events]\n{HOW}\n\n{}", numbered.join("\n\n"));
    let payload =
        json!({"batchId": id, "monoId": mono, "deliveries": deliveries, "message": message});
    let event = NewEvent {
        id: format!("inbox:{id}"),
        task_id,
        assignment_id: None,
        attempt_id: None,
        kind: INBOX.into(),
        payload: payload.clone(),
    };
    append(&tx, &event, now)?;
    tx.commit()?;
    Ok(batch_of(&payload))
}

/// Acknowledges every batch whose message reached Mono's transcript (the
/// `mono-delivered:<session>:<batch>` receipt of `mono_session`). Returns
/// the acknowledged batch ids.
pub fn acknowledge_delivered(
    store: &Store,
    mono: &str,
    mono_session: i64,
    now: u64,
) -> Result<Vec<String>, TaskError> {
    let Some(tasks) = store.existing_tasks()? else {
        return Ok(Vec::new());
    };
    let mut done = Vec::new();
    for batch in unacknowledged(&tasks, mono)? {
        let receipt = format!("mono-delivered:{mono_session}:{}", batch.id);
        if store
            .setting(&receipt)
            .map_err(TaskError::Storage)?
            .is_none()
        {
            continue;
        }
        let tx = tasks.begin()?;
        for id in &batch.deliveries {
            if tasks
                .delivery(id)?
                .is_some_and(|d| d.state.as_str() == "delivered")
            {
                acknowledge(&tx, id, now)?;
            }
        }
        tx.commit()?;
        done.push(batch.id);
    }
    Ok(done)
}

impl TaskStore<'_> {
    /// Marks a pending delivery Delivered inside the caller's transaction.
    fn record_on(&self, delivery: &PendingDelivery, now: u64) -> Result<(), TaskError> {
        let key = super::store::Key::new(
            "native_task_deliveries",
            "id=?",
            vec![&delivery.id],
            "delivery",
        );
        key.bump(
            self.conn,
            delivery.revision,
            "state='delivered'",
            vec![],
            now,
        )?;
        Ok(())
    }
}

/// The delivery as one item of Mono's message, with its task id; None when
/// it no longer needs Mono.
fn describe(
    tasks: &TaskStore<'_>,
    delivery: &PendingDelivery,
) -> Result<Option<(String, String)>, TaskError> {
    if delivery.destination == MONO_DECISION {
        let id = delivery
            .payload_ref
            .strip_prefix("decision:")
            .unwrap_or(&delivery.payload_ref);
        let Some(decision) = tasks.decision(id)? else {
            return Ok(None);
        };
        if decision.state != DecisionState::Open || tasks.escalation(id)?.is_some() {
            return Ok(None);
        }
        return Ok(Some((
            decision.task_id.clone(),
            question(tasks, &decision)?,
        )));
    }
    let Some(event) = event(tasks, &delivery.event_id)? else {
        return Ok(None);
    };
    Ok(Some((event.task_id.clone(), review(tasks, &event)?)))
}

fn event(tasks: &TaskStore<'_>, id: &str) -> Result<Option<super::Event>, TaskError> {
    let seq: Option<i64> = tasks
        .conn
        .query_row(
            "SELECT seq FROM native_task_events WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(seq) = seq else {
        return Ok(None);
    };
    Ok(tasks.events_after(seq - 1, 1)?.pop())
}

/// The task and assignment lines shared by every item.
fn context(
    tasks: &TaskStore<'_>,
    task_id: &str,
    assignment: Option<&str>,
) -> Result<String, TaskError> {
    let task = tasks
        .task(task_id)?
        .ok_or_else(|| TaskError::NotFound(format!("task:{task_id}")))?;
    let mut text = format!(
        "Task {} \"{}\" ({}, {}). Request: {}",
        task.id,
        task.title,
        task.stage.as_str(),
        task.lifecycle.as_str(),
        task.request
    );
    if let Some(found) = assignment.and_then(|id| tasks.assignment(task_id, id).transpose()) {
        let a = found?;
        text.push_str(&format!(
            "\nAssignment {} \"{}\" ({}), files {}: {}",
            a.id,
            a.title,
            a.status.as_str(),
            a.files.join(", "),
            a.prompt
        ));
    }
    Ok(text)
}

fn question(tasks: &TaskStore<'_>, decision: &DecisionRequest) -> Result<String, TaskError> {
    let mut text = format!(
        "Worker question, decision {} (revision {}, scope {}).\n{}\nQuestion: {}",
        decision.id,
        decision.revision,
        decision.scope.as_str(),
        context(tasks, &decision.task_id, decision.assignment_id.as_deref())?,
        decision.question
    );
    if !decision.context.is_empty() {
        text.push_str(&format!("\nContext: {}", decision.context));
    }
    let prior: Vec<String> = tasks
        .prior_decisions(&decision.task_id)?
        .into_iter()
        .filter_map(|d| Some(format!("- {} → {}", d.question, d.answer?)))
        .collect();
    if !prior.is_empty() {
        text.push_str(&format!(
            "\nPrior decisions in this task:\n{}",
            prior.join("\n")
        ));
    }
    text.push_str(&format!(
        "\nAnswer with tasks.decisions.answer {{\"decisionId\":\"{id}\",\"revision\":{rev},\"answer\":\"...\"}}, or ask the user with tasks.decisions.escalate {{\"decisionId\":\"{id}\",\"revision\":{rev},\"why\":\"...\",\"recommendation\":\"...\",\"alternatives\":[\"...\"]}}.",
        id = decision.id,
        rev = decision.revision
    ));
    Ok(text)
}

fn review(tasks: &TaskStore<'_>, event: &super::Event) -> Result<String, TaskError> {
    let findings: Vec<String> = event.payload["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| {
            serde_json::from_value::<Finding>(f.clone())
                .map(|f| format!("- {}", f.describe()))
                .unwrap_or_else(|_| format!("- {f}"))
        })
        .collect();
    let what = match event.kind.as_str() {
        "task.needs_integration" => "Integration needs your judgement",
        "task.verification_waiting" => "Verification needs your judgement",
        _ => "Review needs your judgement",
    };
    let mut text = format!(
        "{what} ({}).\n{}",
        event.id,
        context(tasks, &event.task_id, event.assignment_id.as_deref())?
    );
    if let Some(claim) = event
        .attempt_id
        .as_deref()
        .map(|attempt| tasks.claim(attempt))
        .transpose()?
        .flatten()
    {
        text.push_str(&format!(
            "\nWorker's claim ({}): {}",
            claim.outcome.as_str(),
            claim.summary
        ));
        if !claim.remaining_work.is_empty() {
            text.push_str(&format!(
                "\nRemaining work: {}",
                claim.remaining_work.join("; ")
            ));
        }
    }
    if !findings.is_empty() {
        text.push_str(&format!("\nFindings:\n{}", findings.join("\n")));
    }
    let revision = match event.assignment_id.as_deref() {
        Some(id) => tasks
            .assignment(&event.task_id, id)?
            .map_or(0, |a| a.revision),
        None => 0,
    };
    text.push_str(&format!(
        "\nResolve with tasks.review.accept {{\"taskId\":\"{t}\",\"assignmentId\":\"...\",\"revision\":{revision}}}, tasks.review.sendBack {{\"taskId\":\"{t}\",\"assignmentId\":\"...\",\"revision\":{revision},\"reason\":\"...\"}} or tasks.criteria.verify {{\"taskId\":\"{t}\",\"criterionId\":\"...\",\"check\":\"...\"}}.",
        t = event.task_id
    ));
    Ok(text)
}
