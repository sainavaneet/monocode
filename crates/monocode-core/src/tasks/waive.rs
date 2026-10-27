//! Mono's judgement for a declared check that cannot run.
//!
//! A criterion with checks needs every check to pass on one snapshot. A
//! check that could not run (prose, a shell pipeline, a missing program, a
//! refused argument) would block completion for good, so Mono may waive it
//! with a reason: a passing verification marked waived, on the snapshot the
//! check could not run against. A check that ran and failed is never waived;
//! the work must change instead.
use super::events::{append, NewEvent};
use super::{TaskError, TaskStore, Verification};
use serde_json::json;

const WAIVED: &str = "verification.waived";

impl Verification {
    fn evidence_json(&self) -> serde_json::Value {
        serde_json::from_str(&self.evidence).unwrap_or_default()
    }

    /// Why Mono waived this check, if it is a waiver.
    pub fn waived(&self) -> Option<String> {
        let evidence = self.evidence_json();
        (evidence["waived"] == true).then(|| evidence["reason"].as_str().unwrap_or("").to_owned())
    }

    /// Whether this records a check that could not run (and is not already
    /// Mono's waiver of one).
    pub fn unrunnable(&self) -> bool {
        let evidence = self.evidence_json();
        evidence["unrunnable"].is_string() && evidence["waived"] != true
    }
}

/// Records Mono's waiver of `check`, a declared check of `criterion_id`
/// whose newest live verification is one that could not run. `reason` is
/// required (and redacted). Returns the waiver.
pub fn waive_check(
    tasks: &TaskStore<'_>,
    task_id: &str,
    criterion_id: &str,
    check: &str,
    reason: &str,
    now: u64,
) -> Result<Verification, TaskError> {
    let reason = super::handoff::redact(reason.trim());
    if reason.is_empty() {
        return Err(TaskError::Invalid(
            "Give the reason you judge the criterion met without this check.".into(),
        ));
    }
    let task = tasks
        .task(task_id)?
        .ok_or_else(|| TaskError::NotFound(format!("task:{task_id}")))?;
    let criterion = task
        .criteria
        .iter()
        .find(|c| c.id == criterion_id)
        .ok_or_else(|| {
            TaskError::Invalid(format!("Task {task_id} has no criterion {criterion_id}."))
        })?;
    if !criterion.checks.iter().any(|c| c == check) {
        return Err(TaskError::Invalid(format!(
            "`{check}` is not a check of criterion {criterion_id}; only its declared checks can be waived."
        )));
    }
    let tx = tasks.begin()?;
    let newest = tasks.verifications(task_id)?.into_iter().rfind(|v| {
        !v.invalidated && v.criterion_id.as_deref() == Some(criterion_id) && v.check == check
    });
    let Some(blocked) = newest.filter(Verification::unrunnable) else {
        return Err(TaskError::Invalid(format!(
            "`{check}` has not been recorded as unable to run, so it cannot be waived: a check that ran and failed needs the work to change, and one not yet tried runs in the next verification round."
        )));
    };
    let mut evidence = blocked.evidence_json();
    evidence["waived"] = json!(true);
    evidence["reason"] = json!(reason);
    evidence["waivedVerification"] = json!(blocked.id);
    let waiver = Verification {
        id: format!("waived:{}", blocked.id),
        evidence: evidence.to_string(),
        passed: true,
        created_at: now,
        ..blocked.clone()
    };
    let stored = tasks.record_verification(&waiver)?;
    let event = NewEvent {
        id: format!("waived:{}", blocked.id),
        task_id: task_id.into(),
        assignment_id: blocked.assignment_id.clone(),
        attempt_id: None,
        kind: WAIVED.into(),
        payload: json!({"criterionId": criterion_id, "check": check, "reason": reason,
                        "checkpoint": blocked.checkpoint_commit}),
    };
    append(&tx, &event, now)?;
    tx.commit()?;
    Ok(stored)
}

#[cfg(test)]
#[path = "waive_tests.rs"]
mod tests;
