//! The four task state machines and the one table of their transitions.
//!
//! Each row says which states it leaves, the state it enters, who may ask for
//! it, what must already hold, what the store persists and emits, how it
//! changes scheduling eligibility, and what a restart does afterwards.
use super::events::append;
use super::store::{event_for, guard, Key, TaskStore};
use super::TaskError;
use rusqlite::{params_from_iter, Connection, OptionalExtension};
use serde_json::json;

macro_rules! state_enum {
    ($(#[$meta:meta])* $name:ident {
        $($(#[$doc:meta])* $variant:ident => $text:literal),+ $(,)?
    }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, ::serde::Serialize, ::serde::Deserialize)]
        pub enum $name { $($(#[$doc])* #[serde(rename = $text)] $variant),+ }
        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
            pub fn parse(text: &str) -> Option<Self> {
                match text { $($text => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}
pub(super) use state_enum;

state_enum!(
    /// Where the work is. Review or verification may send it back.
    TaskStage {
        Planning => "planning",
        Ready => "ready",
        Implementing => "implementing",
        Reviewing => "reviewing",
        Integrating => "integrating",
        Verifying => "verifying",
        Completed => "completed",
    }
);
state_enum!(
    /// Whether the work may proceed, independent of its stage.
    TaskLifecycle {
        Active => "active",
        Waiting => "waiting",
        Paused => "paused",
        Blocked => "blocked",
        Cancelled => "cancelled",
        Completed => "completed",
    }
);
state_enum!(
    /// One assignment (`orchestration::Task`) of a task's plan.
    #[derive(Default)]
    AssignmentState {
        #[default]
        Queued => "queued",
        Ready => "ready",
        Running => "running",
        WaitingForDecision => "waiting_for_decision",
        NeedsReview => "needs_review",
        Accepted => "accepted",
        Failed => "failed",
        Cancelled => "cancelled",
    }
);
state_enum!(
    /// One worker session working an assignment.
    AttemptState {
        Created => "created",
        Starting => "starting",
        Running => "running",
        Waiting => "waiting",
        Succeeded => "succeeded",
        Failed => "failed",
        Interrupted => "interrupted",
        Cancelled => "cancelled",
    }
);
state_enum!(
    /// Who asks for a change. Authority comes from this, never from text.
    Requester {
        User => "user",
        Mono => "mono",
        Worker => "worker",
        /// The app's coordinator service, including its review step.
        Coordinator => "coordinator",
    }
);

/// A state of any of the four machines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(tag = "machine", content = "state", rename_all = "snake_case")]
pub enum State {
    Stage(TaskStage),
    Lifecycle(TaskLifecycle),
    Assignment(AssignmentState),
    Attempt(AttemptState),
}

impl State {
    pub fn machine(self) -> &'static str {
        match self {
            Self::Stage(_) => "stage",
            Self::Lifecycle(_) => "lifecycle",
            Self::Assignment(_) => "assignment",
            Self::Attempt(_) => "attempt",
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stage(s) => s.as_str(),
            Self::Lifecycle(s) => s.as_str(),
            Self::Assignment(s) => s.as_str(),
            Self::Attempt(s) => s.as_str(),
        }
    }
    /// The column that holds this machine's state.
    pub fn column(self) -> &'static str {
        match self {
            Self::Stage(_) => "stage",
            Self::Lifecycle(_) => "lifecycle",
            Self::Assignment(_) | Self::Attempt(_) => "status",
        }
    }
    /// Parses `text` as a state of the same machine as `self`.
    pub fn same_machine(self, text: &str) -> Option<Self> {
        match self {
            Self::Stage(_) => TaskStage::parse(text).map(Self::Stage),
            Self::Lifecycle(_) => TaskLifecycle::parse(text).map(Self::Lifecycle),
            Self::Assignment(_) => AssignmentState::parse(text).map(Self::Assignment),
            Self::Attempt(_) => AttemptState::parse(text).map(Self::Attempt),
        }
    }
    /// Every state of each machine: stage, lifecycle, assignment, attempt.
    pub fn machines() -> [&'static [State]; 4] {
        use AssignmentState as A;
        use AttemptState as T;
        use TaskLifecycle as L;
        use TaskStage as S;
        const STAGE: &[State] = &[
            State::Stage(S::Planning),
            State::Stage(S::Ready),
            State::Stage(S::Implementing),
            State::Stage(S::Reviewing),
            State::Stage(S::Integrating),
            State::Stage(S::Verifying),
            State::Stage(S::Completed),
        ];
        const LIFECYCLE: &[State] = &[
            State::Lifecycle(L::Active),
            State::Lifecycle(L::Waiting),
            State::Lifecycle(L::Paused),
            State::Lifecycle(L::Blocked),
            State::Lifecycle(L::Cancelled),
            State::Lifecycle(L::Completed),
        ];
        const ASSIGNMENT: &[State] = &[
            State::Assignment(A::Queued),
            State::Assignment(A::Ready),
            State::Assignment(A::Running),
            State::Assignment(A::WaitingForDecision),
            State::Assignment(A::NeedsReview),
            State::Assignment(A::Accepted),
            State::Assignment(A::Failed),
            State::Assignment(A::Cancelled),
        ];
        const ATTEMPT: &[State] = &[
            State::Attempt(T::Created),
            State::Attempt(T::Starting),
            State::Attempt(T::Running),
            State::Attempt(T::Waiting),
            State::Attempt(T::Succeeded),
            State::Attempt(T::Failed),
            State::Attempt(T::Interrupted),
            State::Attempt(T::Cancelled),
        ];
        [STAGE, LIFECYCLE, ASSIGNMENT, ATTEMPT]
    }
}

/// What must already hold; the store checks it inside the transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Precondition {
    /// The task's lifecycle is Active.
    TaskActive,
    /// The task is still Planning or Ready, so its plan may be replaced.
    NotStarted,
    /// Every assignment this one depends on is Accepted.
    DependenciesAccepted,
    /// The assignment's latest attempt Succeeded: its result is a claim to
    /// review. An older attempt's success never counts for a newer run.
    AttemptSucceeded,
    /// The assignment has no Open decision request.
    NoOpenDecision,
    /// At least one assignment is Accepted and every other one is Cancelled.
    AssignmentsAccepted,
    /// The task has at least one required criterion, and the latest live
    /// verification of every required criterion passed.
    CriteriaSatisfied,
    /// No decision request of the task is unresolved, other than those of
    /// cancelled assignments.
    NoBlockingDecision,
    /// The task's final report is persisted.
    FinalReportRecorded,
    /// The task's stage is Completed.
    StageCompleted,
}

/// How a transition changes whether the coordinator may schedule work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eligibility {
    /// The subject (or its task's assignments) may now be scheduled.
    Eligible,
    /// The subject stops being scheduled.
    Ineligible,
    Unchanged,
}

/// What a restart does with a subject left in the transition's target state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// Continue scheduling as normal.
    Resume,
    /// Check the external effect (session, receipt) before acting again.
    Reconcile,
    /// The worker process died with the app: mark the attempt Interrupted.
    Interrupt,
    /// Keep waiting for the user or Mono; never infer an answer.
    Wait,
    /// Final: nothing recovers it.
    Never,
}

pub struct Transition {
    pub from: &'static [State],
    pub to: State,
    pub requesters: &'static [Requester],
    /// Checked in order; the first that fails is reported.
    pub preconditions: &'static [Precondition],
    /// The columns the store writes, besides `revision` and `updated_at`.
    pub persists: &'static str,
    /// The kind of the event appended in the same transaction.
    pub event: &'static str,
    pub eligibility: Eligibility,
    pub recovery: Recovery,
}

use Eligibility::{Eligible, Ineligible, Unchanged};
use Recovery::{Interrupt, Never, Reconcile, Resume, Wait};
use Requester::{Coordinator as C, Mono as M, User as U, Worker as W};
// Each machine's states by its `State` variant name, for the table below.
type Stage = TaskStage;
type Lifecycle = TaskLifecycle;
type Assignment = AssignmentState;
type Attempt = AttemptState;

macro_rules! rule {
    ($machine:ident [$($from:ident),+] -> $to:ident, [$($who:ident),+], [$($pre:ident),*],
     $persists:literal, $event:literal, $eligibility:expr, $recovery:expr) => {
        Transition {
            from: &[$(State::$machine($machine::$from)),+],
            to: State::$machine($machine::$to),
            requesters: &[$($who),+],
            preconditions: &[$(Precondition::$pre),*],
            persists: $persists,
            event: $event,
            eligibility: $eligibility,
            recovery: $recovery,
        }
    };
}

#[rustfmt::skip]
pub static TRANSITIONS: &[Transition] = &[
    // Task stage.
    rule!(Stage [Planning] -> Ready, [M, U], [], "stage", "task.ready", Eligible, Resume),
    rule!(Stage [Ready] -> Planning, [U, M], [], "stage", "task.replanning", Ineligible, Resume),
    rule!(Stage [Ready] -> Implementing, [C, M], [TaskActive], "stage", "task.implementing", Eligible, Resume),
    rule!(Stage [Implementing] -> Reviewing, [C, M], [TaskActive], "stage", "task.reviewing", Unchanged, Resume),
    rule!(Stage [Reviewing, Integrating, Verifying] -> Implementing, [C, M], [], "stage", "task.sent_back", Eligible, Resume),
    rule!(Stage [Reviewing] -> Integrating, [C, M], [TaskActive, AssignmentsAccepted], "stage", "task.integrating", Unchanged, Resume),
    rule!(Stage [Integrating] -> Verifying, [C, M], [TaskActive], "stage", "task.verifying", Unchanged, Resume),
    rule!(Stage [Verifying] -> Completed, [C, M], [TaskActive, AssignmentsAccepted, CriteriaSatisfied, NoBlockingDecision, FinalReportRecorded], "stage", "task.verified", Ineligible, Never),
    // Task lifecycle.
    rule!(Lifecycle [Active] -> Waiting, [C, M], [], "lifecycle", "task.waiting", Ineligible, Wait),
    rule!(Lifecycle [Waiting, Blocked] -> Active, [U, M, C], [], "lifecycle, blocked_reason", "task.active", Eligible, Resume),
    rule!(Lifecycle [Active, Waiting, Blocked] -> Paused, [U, M], [], "lifecycle", "task.paused", Ineligible, Wait),
    rule!(Lifecycle [Paused] -> Active, [U, M], [], "lifecycle", "task.resumed", Eligible, Resume),
    rule!(Lifecycle [Active, Waiting] -> Blocked, [C, M], [], "lifecycle, blocked_reason", "task.blocked", Ineligible, Wait),
    rule!(Lifecycle [Active, Waiting, Paused, Blocked] -> Cancelled, [U, M], [], "lifecycle", "task.cancelled", Ineligible, Never),
    rule!(Lifecycle [Active] -> Completed, [C, M], [StageCompleted], "lifecycle", "task.completed", Ineligible, Never),
    // Assignment.
    rule!(Assignment [Queued] -> Ready, [C], [DependenciesAccepted], "status", "assignment.ready", Eligible, Resume),
    rule!(Assignment [Ready, NeedsReview, Failed] -> Queued, [C, M], [], "status", "assignment.queued", Unchanged, Resume),
    rule!(Assignment [Accepted] -> Queued, [C, M], [TaskActive], "status", "assignment.queued", Unchanged, Resume),
    // Steering re-queues an affected assignment whose worker it stopped.
    rule!(Assignment [Running, WaitingForDecision] -> Queued, [C, M], [], "status", "assignment.requeued", Unchanged, Resume),
    rule!(Assignment [Ready] -> Running, [C], [TaskActive], "status", "assignment.running", Ineligible, Interrupt),
    rule!(Assignment [Running] -> WaitingForDecision, [W, C, M], [], "status", "assignment.waiting_for_decision", Ineligible, Wait),
    rule!(Assignment [WaitingForDecision] -> Running, [M, U, C], [NoOpenDecision], "status", "assignment.resumed", Ineligible, Interrupt),
    rule!(Assignment [Running, WaitingForDecision] -> NeedsReview, [C, W], [AttemptSucceeded], "status", "assignment.needs_review", Ineligible, Reconcile),
    rule!(Assignment [NeedsReview] -> Accepted, [C, M], [], "status", "assignment.accepted", Eligible, Resume),
    rule!(Assignment [Running, WaitingForDecision] -> Failed, [C, M], [], "status", "assignment.failed", Ineligible, Wait),
    rule!(Assignment [Queued, Ready, Running, WaitingForDecision, NeedsReview, Failed] -> Cancelled, [U, M, C], [], "status", "assignment.cancelled", Ineligible, Never),
    // Attempt.
    rule!(Attempt [Created] -> Starting, [C], [TaskActive], "status", "attempt.starting", Unchanged, Reconcile),
    rule!(Attempt [Starting, Waiting] -> Running, [C], [], "status", "attempt.running", Unchanged, Interrupt),
    rule!(Attempt [Running] -> Waiting, [W, C], [], "status", "attempt.waiting", Unchanged, Interrupt),
    rule!(Attempt [Running, Waiting] -> Succeeded, [W, C], [], "status; its assignment to NeedsReview", "attempt.succeeded", Unchanged, Never),
    rule!(Attempt [Starting, Running, Waiting] -> Failed, [C], [], "status", "attempt.failed", Unchanged, Never),
    rule!(Attempt [Starting, Running, Waiting] -> Interrupted, [C], [], "status", "attempt.interrupted", Unchanged, Never),
    rule!(Attempt [Created, Starting, Running, Waiting] -> Cancelled, [U, M, C], [], "status", "attempt.cancelled", Unchanged, Never),
];

/// The rule for `from -> to` when `requester` may ask for it.
pub fn authorize(
    from: State,
    to: State,
    requester: Requester,
) -> Result<&'static Transition, TaskError> {
    let rule = TRANSITIONS
        .iter()
        .find(|rule| rule.to == to && rule.from.contains(&from))
        .ok_or(TaskError::IllegalTransition {
            from: Some(from),
            to,
        })?;
    if !rule.requesters.contains(&requester) {
        return Err(TaskError::NotAllowed {
            requester,
            from,
            to,
        });
    }
    Ok(rule)
}

// Applying transitions to stored records.

/// A record with a state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subject<'a> {
    /// Its stage and lifecycle share the task's revision.
    Task(&'a str),
    Assignment {
        task: &'a str,
        id: &'a str,
    },
    Attempt(&'a str),
}

impl<'a> Subject<'a> {
    pub(super) fn key(self) -> Key<'a> {
        match self {
            Self::Task(id) => Key::new("native_tasks", "id=?", vec![id], "task"),
            Self::Assignment { task, id } => Key::new(
                "native_task_assignments",
                "task_id=? AND id=?",
                vec![task, id],
                "assignment",
            ),
            Self::Attempt(id) => Key::new("native_task_attempts", "id=?", vec![id], "attempt"),
        }
    }
    fn has_machine_of(self, state: State) -> bool {
        matches!(
            (self, state),
            (Self::Task(_), State::Stage(_) | State::Lifecycle(_))
                | (Self::Assignment { .. }, State::Assignment(_))
                | (Self::Attempt(_), State::Attempt(_))
        )
    }
}

impl TaskStore<'_> {
    pub fn revision(&self, subject: Subject<'_>) -> Result<u64, TaskError> {
        let key = subject.key();
        key.revision(self.conn)
    }

    /// The subject's current state in the machine `like` belongs to.
    pub fn state(&self, subject: Subject<'_>, like: State) -> Result<State, TaskError> {
        let key = subject.key();
        if !subject.has_machine_of(like) {
            return Err(TaskError::IllegalTransition {
                from: None,
                to: like,
            });
        }
        let revision = self.revision(subject)?;
        let text = guard(self.conn, &key, like.column(), revision)?;
        like.same_machine(&text)
            .ok_or_else(|| TaskError::Storage(format!("{} has unknown state {text}", key.label)))
    }

    /// Moves `subject` to `to` if the table allows it from its current state
    /// for `requester` and the precondition holds. Persists the state, bumps
    /// the revision and appends the rule's event in one transaction. A
    /// succeeded attempt also moves its assignment to NeedsReview.
    pub fn transition(
        &self,
        subject: Subject<'_>,
        to: State,
        requester: Requester,
        expected_revision: u64,
        now: u64,
    ) -> Result<u64, TaskError> {
        let tx = self.begin()?;
        let revision = apply(&tx, subject, to, requester, expected_revision, now)?;
        tx.commit()?;
        Ok(revision)
    }

    /// Returns a Blocked task to Active for `requester`, in one transaction
    /// through the table (which clears the blocked reason). A task that is
    /// not Blocked is refused. Returns the task's new revision.
    pub fn unblock(
        &self,
        task_id: &str,
        expected_revision: u64,
        requester: Requester,
        now: u64,
    ) -> Result<u64, TaskError> {
        let tx = self.begin()?;
        let subject = Subject::Task(task_id);
        let lifecycle = self.state(subject, State::Lifecycle(TaskLifecycle::Active))?;
        if lifecycle != State::Lifecycle(TaskLifecycle::Blocked) {
            return Err(TaskError::Invalid(format!(
                "Task {task_id} is not blocked (it is {}).",
                lifecycle.as_str()
            )));
        }
        let active = State::Lifecycle(TaskLifecycle::Active);
        let revision = apply(&tx, subject, active, requester, expected_revision, now)?;
        tx.commit()?;
        Ok(revision)
    }
}

/// The task, assignment and attempt a subject belongs to.
fn owners(
    conn: &Connection,
    subject: Subject<'_>,
) -> Result<(String, Option<String>, Option<String>), TaskError> {
    Ok(match subject {
        Subject::Task(id) => (id.into(), None, None),
        Subject::Assignment { task, id } => (task.into(), Some(id.into()), None),
        Subject::Attempt(id) => {
            let (task, assignment): (String, String) = conn
                .query_row(
                    "SELECT task_id,assignment_id FROM native_task_attempts WHERE id=?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .ok_or_else(|| TaskError::NotFound(subject.key().label))?;
            (task, Some(assignment), Some(id.into()))
        }
    })
}

pub(super) fn apply(
    conn: &Connection,
    subject: Subject<'_>,
    to: State,
    requester: Requester,
    expected: u64,
    now: u64,
) -> Result<u64, TaskError> {
    if !subject.has_machine_of(to) {
        return Err(TaskError::IllegalTransition { from: None, to });
    }
    let key = subject.key();
    let text = guard(conn, &key, to.column(), expected)?;
    let from = to
        .same_machine(&text)
        .ok_or_else(|| TaskError::Storage(format!("{} has unknown state {text}", key.label)))?;
    let rule = authorize(from, to, requester)?;
    let (task, assignment, attempt) = owners(conn, subject)?;
    for precondition in rule.preconditions {
        if !precondition_holds(conn, &task, assignment.as_deref(), *precondition)? {
            return Err(TaskError::Precondition(*precondition));
        }
    }
    let set = format!("{}=?", to.column());
    let revision = key.bump(conn, expected, &set, vec![&to.as_str()], now)?;
    // The table persists `blocked_reason` with the lifecycle: a task that
    // leaves Blocked shows no reason (it stays in the task's events).
    if from == State::Lifecycle(TaskLifecycle::Blocked) {
        conn.execute(
            "UPDATE native_tasks SET blocked_reason=NULL WHERE id=?1",
            [&task],
        )?;
    }
    let payload = json!({"from": from.as_str(), "to": to.as_str(), "requester": requester, "revision": revision});
    let mut event = event_for(&task, &key.label, revision, rule.event, payload);
    event.assignment_id.clone_from(&assignment);
    event.attempt_id = attempt;
    append(conn, &event, now)?;
    super::completion::invalidate_on_send_back(conn, &task, assignment.as_deref(), from, to)?;
    if to == State::Attempt(AttemptState::Succeeded) && is_latest_attempt(conn, subject)? {
        let id = assignment.unwrap_or_default();
        let parent = Subject::Assignment {
            task: &task,
            id: &id,
        };
        let tasks = TaskStore { conn };
        let review = State::Assignment(AssignmentState::NeedsReview);
        let status = tasks.state(parent, review)?;
        // The cascade is requested by whoever finished the attempt; the
        // Succeeded and NeedsReview rules allow the same requesters.
        if authorize(status, review, requester).is_ok() {
            let current = tasks.revision(parent)?;
            apply(conn, parent, review, requester, current, now)?;
        }
    }
    Ok(revision)
}

/// Whether `subject` is its assignment's newest attempt. A replaced attempt
/// that finishes late must not start a review of the current run. Newest is
/// by insertion order: two processes sharing the file may disagree on time.
pub(super) fn is_latest_attempt(
    conn: &Connection,
    subject: Subject<'_>,
) -> Result<bool, TaskError> {
    let Subject::Attempt(id) = subject else {
        return Ok(false);
    };
    let latest: String = conn.query_row(
        "SELECT l.id FROM native_task_attempts a JOIN native_task_attempts l
           ON l.task_id=a.task_id AND l.assignment_id=a.assignment_id
         WHERE a.id=?1 ORDER BY l.rowid DESC LIMIT 1",
        [id],
        |row| row.get(0),
    )?;
    Ok(latest == id)
}

fn precondition_holds(
    conn: &Connection,
    task: &str,
    assignment: Option<&str>,
    precondition: Precondition,
) -> Result<bool, TaskError> {
    let sql = match precondition {
        Precondition::TaskActive => "SELECT lifecycle='active' FROM native_tasks WHERE id=?1",
        Precondition::NotStarted => "SELECT stage IN ('planning','ready') FROM native_tasks WHERE id=?1",
        Precondition::StageCompleted => "SELECT stage='completed' FROM native_tasks WHERE id=?1",
        Precondition::DependenciesAccepted => "SELECT NOT EXISTS(SELECT 1 FROM json_each((SELECT depends_on FROM native_task_assignments WHERE task_id=?1 AND id=?2)) d
            WHERE NOT EXISTS(SELECT 1 FROM native_task_assignments a WHERE a.task_id=?1 AND a.id=d.value AND a.status='accepted'))",
        Precondition::AttemptSucceeded => "SELECT status='succeeded' FROM native_task_attempts WHERE task_id=?1 AND assignment_id=?2
            ORDER BY rowid DESC LIMIT 1",
        Precondition::NoOpenDecision => "SELECT NOT EXISTS(SELECT 1 FROM native_task_decisions WHERE task_id=?1 AND assignment_id=?2 AND state='open')",
        Precondition::AssignmentsAccepted => "SELECT NOT EXISTS(SELECT 1 FROM native_task_assignments WHERE task_id=?1 AND status NOT IN ('accepted','cancelled'))
            AND EXISTS(SELECT 1 FROM native_task_assignments WHERE task_id=?1 AND status='accepted')",
        Precondition::CriteriaSatisfied => "SELECT EXISTS(SELECT 1 FROM native_tasks t, json_each(t.criteria) c
              WHERE t.id=?1 AND json_extract(c.value,'$.required'))
            AND NOT EXISTS(SELECT 1 FROM native_tasks t, json_each(t.criteria) c
            WHERE t.id=?1 AND json_extract(c.value,'$.required') AND COALESCE((SELECT v.passed FROM native_task_verifications v
              WHERE v.task_id=t.id AND v.criterion_id=json_extract(c.value,'$.id') AND v.invalidated=0
              ORDER BY v.rowid DESC LIMIT 1),0)=0)",
        Precondition::NoBlockingDecision => "SELECT NOT EXISTS(SELECT 1 FROM native_task_decisions d WHERE d.task_id=?1 AND d.state<>'resolved'
            AND NOT EXISTS(SELECT 1 FROM native_task_assignments a WHERE a.task_id=d.task_id AND a.id=d.assignment_id AND a.status='cancelled'))",
        Precondition::FinalReportRecorded => "SELECT EXISTS(SELECT 1 FROM native_task_events WHERE id='final_report:'||?1)",
    };
    let mut statement = conn.prepare(sql)?;
    let keys = [task, assignment.unwrap_or_default()];
    let keys = &keys[..statement.parameter_count()];
    let held: Option<bool> = statement
        .query_row(params_from_iter(keys), |row| row.get(0))
        .optional()?
        .flatten();
    Ok(held.unwrap_or(false))
}

#[cfg(test)]
#[path = "states_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "transitions_tests.rs"]
mod transitions_tests;
