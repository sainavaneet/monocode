//! Steering a task: new requirements while it runs.
//!
//! A steer is recorded as a new plan revision (event `steer:<task>@<plan
//! revision>`, never rewriting an earlier one). The affected assignments are
//! the ones named, plus, when a revised plan is given, those whose
//! requirements or file scope changed and those it adds or drops. Only they
//! are touched: a live worker of an affected assignment is Cancelled (its
//! worktree, commits and claim stay as evidence) and asked to stop, and the
//! assignment is queued again, so the next worker starts from the new
//! requirements once that stop is confirmed (`guard`); a dropped one is
//! Cancelled. An assignment the table cannot move (a cancelled one) is
//! reported as skipped, with why. Every other worker continues. When
//! nothing says which assignments are affected, nothing is stopped and Mono
//! is asked to decide.
use super::events::{append, NewEvent};
use super::guard::task_or_missing;
use super::lifecycle::cancel_attempt;
use super::notices::notify;
use super::park::{move_rows, WAITING_ROWS};
use super::recovery::SUPERSEDED;
use super::states::apply;
use super::store::ms;
use super::{
    Assignment, AssignmentState, Requester, State, Subject, Task, TaskError, TaskLifecycle,
    TaskStore,
};
use crate::orchestration::{Plan, WorkspaceMode};
use crate::store::Store;
use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::json;

/// The longest requirements text a steer records.
pub const MAX_REQUIREMENTS: usize = 20_000;
/// How many earlier requirement revisions a worker brief repeats.
const BRIEF_REVISIONS: usize = 10;
const STEERED: &str = "task.steered";

/// New requirements, the assignments they affect if known, and the revised
/// plan if there is one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Steer {
    pub requirements: String,
    pub assignments: Vec<String>,
    pub plan: Option<Plan>,
}

/// An assignment the steer named or changed but could not move.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Skipped {
    pub id: String,
    pub reason: String,
}

/// What a steer did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Steered {
    pub task_id: String,
    pub revision: u64,
    pub plan_revision: u64,
    /// Queued again for the new requirements.
    pub affected: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Named or changed, but not movable.
    pub skipped: Vec<Skipped>,
    /// Attempts asked to stop.
    pub stopping: Vec<String>,
    /// Nothing said which assignments are affected; Mono was asked.
    pub assessing: bool,
}

/// What a revised plan changes against the current assignments.
#[derive(Default)]
struct Diff {
    changed: Vec<Assignment>,
    added: Vec<(usize, Assignment)>,
    removed: Vec<String>,
}

/// What the steer touches, before anything is written.
struct Assessment {
    diff: Diff,
    /// Named or changed (and not removed).
    candidates: Vec<String>,
    /// Nothing says what is affected.
    unknown: bool,
}

fn changed(old: &Assignment, new: &Assignment) -> bool {
    old.title != new.title
        || old.prompt != new.prompt
        || old.files != new.files
        || old.depends_on != new.depends_on
        || old.verification != new.verification
        || old.workspace != new.workspace
}

fn diff(current: &[Assignment], plan: &Plan) -> Diff {
    let mut out = Diff::default();
    for (position, new) in plan.tasks.iter().enumerate() {
        match current.iter().find(|a| a.id == new.id) {
            Some(old) if changed(old, new) => out.changed.push(Assignment {
                status: old.status,
                revision: old.revision,
                ..new.clone()
            }),
            Some(_) => {}
            None => out.added.push((position, new.clone())),
        }
    }
    out.removed = current
        .iter()
        .filter(|a| !plan.tasks.iter().any(|n| n.id == a.id))
        .map(|a| a.id.clone())
        .collect();
    out
}

/// Checks who asks, the requirements and the task's state.
fn check(task: &Task, requester: Requester, requirements: &str) -> Result<(), TaskError> {
    if !matches!(requester, Requester::User | Requester::Mono) {
        return Err(TaskError::Invalid(
            "Only the user or Mono steers a task.".into(),
        ));
    }
    if requirements.is_empty() || requirements.len() > MAX_REQUIREMENTS {
        return Err(TaskError::Invalid(format!(
            "Give the new requirements (1 to {MAX_REQUIREMENTS} bytes)."
        )));
    }
    if matches!(
        task.lifecycle,
        TaskLifecycle::Cancelled | TaskLifecycle::Completed
    ) {
        return Err(TaskError::InvalidState {
            subject: format!("task:{}", task.id),
            state: task.lifecycle.as_str().into(),
        });
    }
    Ok(())
}

/// Which assignments the steer touches.
fn assess(
    task: &Task,
    assignments: &[Assignment],
    steer: &Steer,
    requester: Requester,
) -> Result<Assessment, TaskError> {
    for named in &steer.assignments {
        if !assignments.iter().any(|a| &a.id == named) {
            return Err(TaskError::NotFound(format!("assignment:{named}")));
        }
    }
    let diff = match &steer.plan {
        Some(plan) => {
            crate::orchestration::validate(plan).map_err(TaskError::Invalid)?;
            diff(assignments, plan)
        }
        None => Diff::default(),
    };
    let mut candidates = steer.assignments.clone();
    for a in &diff.changed {
        if !candidates.contains(&a.id) {
            candidates.push(a.id.clone());
        }
    }
    candidates.retain(|a| !diff.removed.contains(a));
    let unknown = candidates.is_empty() && diff.added.is_empty() && diff.removed.is_empty();
    if unknown && requester == Requester::Mono {
        return Err(TaskError::Invalid(
            "Name the affected assignments (assignmentIds) or pass the revised plan.".into(),
        ));
    }
    // Reopening accepted work needs an Active task (the table's rule).
    let reopens_accepted = assignments
        .iter()
        .any(|a| candidates.contains(&a.id) && a.status == AssignmentState::Accepted);
    if reopens_accepted && task.lifecycle != TaskLifecycle::Active {
        return Err(TaskError::Invalid(
            "Accepted work is reopened only while the task is active; resume it first.".into(),
        ));
    }
    Ok(Assessment {
        diff,
        candidates,
        unknown,
    })
}

/// Records the steer as a new plan revision and applies it (see the module
/// docs). `revision` is the task revision the caller read.
pub fn steer(
    store: &Store,
    id: &str,
    revision: u64,
    requester: Requester,
    steer: &Steer,
    now: u64,
) -> Result<Steered, TaskError> {
    let requirements = steer.requirements.trim();
    let tasks = store.tasks()?;
    let current = task_or_missing(&tasks, id)?;
    check(&current, requester, requirements)?;
    let assessment = assess(&current, &tasks.assignments(id)?, steer, requester)?;
    let tx = tasks.begin()?;
    let key = Subject::Task(id).key();
    let revision = key.bump(&tx, revision, "plan_revision=plan_revision+1", vec![], now)?;
    let plan_revision = task_or_missing(&TaskStore { conn: &tx }, id)?.plan_revision;
    let mut steered = Steered {
        task_id: id.into(),
        revision,
        plan_revision,
        affected: Vec::new(),
        added: assessment
            .diff
            .added
            .iter()
            .map(|(_, a)| a.id.clone())
            .collect(),
        removed: assessment.diff.removed.clone(),
        skipped: Vec::new(),
        stopping: Vec::new(),
        assessing: assessment.unknown,
    };
    if !assessment.unknown {
        apply_changes(&tx, id, &assessment, &mut steered, now)?;
    }
    record(&tx, requirements, requester, &steered, now)?;
    tx.commit()?;
    Ok(steered)
}

/// Writes the revised plan, then stops dropped and affected work.
fn apply_changes(
    conn: &Connection,
    task: &str,
    assessment: &Assessment,
    steered: &mut Steered,
    now: u64,
) -> Result<(), TaskError> {
    apply_plan(conn, task, &assessment.diff, now)?;
    for assignment in &assessment.diff.removed {
        let moved = stop(conn, task, assignment, AssignmentState::Cancelled, now)?;
        steered.stopping.extend(moved.stopping);
    }
    for assignment in &assessment.candidates {
        let moved = stop(conn, task, assignment, AssignmentState::Queued, now)?;
        steered.stopping.extend(moved.stopping);
        match moved.skipped {
            Some(reason) => steered.skipped.push(Skipped {
                id: assignment.clone(),
                reason,
            }),
            None => steered.affected.push(assignment.clone()),
        }
    }
    Ok(())
}

/// The steer's history entry, and Mono's question when nothing says what
/// it affects.
fn record(
    conn: &Connection,
    requirements: &str,
    requester: Requester,
    steered: &Steered,
    now: u64,
) -> Result<(), TaskError> {
    let task = &steered.task_id;
    let payload = json!({
        "requirements": requirements, "requester": requester,
        "planRevision": steered.plan_revision, "affected": steered.affected,
        "added": steered.added, "removed": steered.removed, "skipped": steered.skipped,
        "stopping": steered.stopping, "assessing": steered.assessing,
    });
    let event = NewEvent {
        id: format!("steer:{task}@{}", steered.plan_revision),
        task_id: task.clone(),
        assignment_id: None,
        attempt_id: None,
        kind: STEERED.into(),
        payload,
    };
    append(conn, &event, now)?;
    if steered.assessing {
        notify(conn, task, None, "task.steer_assess", requirements, now)?;
    }
    Ok(())
}

/// Writes the revised plan's changed assignments and inserts its new ones
/// (Queued, at a revision no earlier row of that id used).
fn apply_plan(conn: &Connection, task: &str, diff: &Diff, now: u64) -> Result<(), TaskError> {
    let tasks = TaskStore { conn };
    for assignment in &diff.changed {
        tasks.update_assignment(task, assignment, now)?;
    }
    for (position, a) in &diff.added {
        let first: i64 = conn.query_row(
            "SELECT COALESCE(MAX(revision),0)+1 FROM native_task_assignments WHERE task_id=?1",
            [task],
            |row| row.get(0),
        )?;
        conn.execute(
            "INSERT INTO native_task_assignments(task_id,id,position,title,prompt,files,depends_on,workspace,verification,status,revision,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'queued',?10,?11)",
            params![task, a.id, *position as i64, a.title, a.prompt,
                serde_json::to_string(&a.files)?, serde_json::to_string(&a.depends_on)?,
                a.workspace.map(WorkspaceMode::as_str), serde_json::to_string(&a.verification)?,
                first, ms(now)],
        )?;
    }
    Ok(())
}

/// What stopping one assignment did.
struct Moved {
    stopping: Vec<String>,
    /// Why the assignment could not be moved, if it could not.
    skipped: Option<String>,
}

/// Moves the assignment to `to` (Queued again, or Cancelled when the plan
/// dropped it) and then stops its live worker (Cancelled, stop requested)
/// and drops a pending recovery of it. An assignment the table cannot move
/// (other than one already there) is left alone and reported.
fn stop(
    conn: &Connection,
    task: &str,
    assignment: &str,
    to: AssignmentState,
    now: u64,
) -> Result<Moved, TaskError> {
    let tasks = TaskStore { conn };
    let subject = Subject::Assignment {
        task,
        id: assignment,
    };
    let target = State::Assignment(to);
    let from = tasks.state(subject, target)?;
    if from != target {
        if super::authorize(from, target, Requester::Coordinator).is_err() {
            return Ok(Moved {
                stopping: Vec::new(),
                skipped: Some(format!(
                    "it is {} and cannot become {}",
                    from.as_str(),
                    to.as_str()
                )),
            });
        }
        let revision = tasks.revision(subject)?;
        apply(conn, subject, target, Requester::Coordinator, revision, now)?;
    }
    let mut stopping = Vec::new();
    if let Some(attempt) = tasks.attempts(task, assignment)?.pop() {
        if cancel_attempt(conn, &attempt, now)? {
            stopping.push(attempt.id);
        }
    }
    move_rows(conn, task, Some(assignment), &WAITING_ROWS, SUPERSEDED, now)?;
    Ok(Moved {
        stopping,
        skipped: None,
    })
}

/// The requirement revisions a new worker's brief carries, oldest first, or
/// None when the task was never steered.
pub(super) fn brief_note(tasks: &TaskStore<'_>, task: &str) -> Result<Option<String>, TaskError> {
    let mut statement = tasks.conn.prepare(
        "SELECT json_extract(payload,'$.planRevision'),json_extract(payload,'$.requirements')
         FROM native_task_events WHERE task_id=?1 AND kind=?2 ORDER BY seq DESC LIMIT ?3",
    )?;
    let mut revisions = statement
        .query_map(params![task, STEERED, BRIEF_REVISIONS as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if revisions.is_empty() {
        return Ok(None);
    }
    revisions.reverse();
    let lines: Vec<String> = revisions
        .iter()
        .map(|(n, text)| format!("- Plan revision {n}: {text}"))
        .collect();
    Ok(Some(format!(
        "The requirements were revised while the task ran; the latest revision wins where they differ:\n{}",
        lines.join("\n")
    )))
}

#[cfg(test)]
#[path = "steer_tests.rs"]
mod tests;
