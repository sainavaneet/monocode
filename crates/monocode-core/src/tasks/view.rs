//! A read-only view of the tasks a Mono leads, for the monitoring card and
//! the sidebar badge: the stage and what Mono is doing, each worker with its
//! workspace and branch, what waits on the user, verification counts,
//! recovery and account changes without secrets, the timeline, and three
//! separate times (last worker activity, last status sync, last Mono review).
//! Nothing here writes to the store, and with the coordinator off it returns
//! nothing (it starts from `lifecycle::controls`).
use super::coordinator::{lease_resource, LEASE_TTL_MS};
use super::events::Event;
use super::guard::task_or_missing;
use super::lifecycle::{self, ControlRow, Display};
use super::recovery::{Recovery, COLUMNS, SCHEDULED};
use super::review::MONO_REVIEW;
use super::store::read_ms;
use super::timeline::{self, safe, Names, Runner, TimelineEntry};
use super::{AssignmentState, AttemptState, DecisionState, Task, TaskError, TaskStage, TaskStore};
use crate::store::Store;
use std::collections::HashMap;

/// What Mono is doing for a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonoActivity {
    Planning,
    Deciding,
    Reviewing,
    Waiting,
    Supervising,
    Idle,
}

impl MonoActivity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Planning => "Planning",
            Self::Deciding => "Deciding",
            Self::Reviewing => "Reviewing",
            Self::Waiting => "Waiting",
            Self::Supervising => "Supervising",
            Self::Idle => "Idle",
        }
    }
}

/// The sidebar badge of a Mono's tasks. When several apply, the first of
/// Needs your decision, Recovering, Paused, Reviewing, N working shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Badge {
    NeedsDecision,
    Recovering,
    Paused,
    Reviewing,
    Working(usize),
}

impl Badge {
    pub fn label(self) -> String {
        match self {
            Self::NeedsDecision => "Needs your decision".into(),
            Self::Recovering => "Recovering".into(),
            Self::Paused => "Paused".into(),
            Self::Reviewing => "Reviewing".into(),
            Self::Working(n) => format!("{n} working"),
        }
    }
}

/// The badge for all of a Mono's tasks: the highest-precedence state any of
/// them is in; workers are counted across tasks.
pub fn badge(tasks: &[TaskMonitor]) -> Option<Badge> {
    let working: usize = tasks.iter().map(TaskMonitor::working).sum();
    tasks
        .iter()
        .filter_map(TaskMonitor::badge)
        .min()
        .map(|badge| match badge {
            Badge::Working(_) => Badge::Working(working),
            other => other,
        })
}

/// What a worker is doing, as the card, the overview and the badge count
/// it. Defined once so its label and colour cannot drift apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WorkerState {
    /// Its current attempt runs (or is starting and its session is busy).
    Working,
    NeedsInput,
    /// Launched; the app has not confirmed the session runs yet.
    Starting,
    /// Running on paper, but its worker was stopped (paused, held after a
    /// restart or takeover, failed): nothing works on it now.
    Stopped,
    NeedsReview,
    Ready,
    Queued,
    Accepted,
    Failed,
    Cancelled,
}

impl WorkerState {
    /// Display order for counts.
    pub const ALL: [Self; 10] = [
        Self::Working,
        Self::NeedsInput,
        Self::Starting,
        Self::Stopped,
        Self::NeedsReview,
        Self::Ready,
        Self::Queued,
        Self::Accepted,
        Self::Failed,
        Self::Cancelled,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "Working",
            Self::NeedsInput => "Needs input",
            Self::Starting => "Starting",
            Self::Stopped => "Stopped",
            Self::NeedsReview => "Needs review",
            Self::Ready => "Ready",
            Self::Queued => "Queued",
            Self::Accepted => "Accepted",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
        }
    }
}

/// What the app sees of a worker's session right now, when it has it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Live {
    pub busy: bool,
    /// Waiting on a permission or an answer in its chat.
    pub needs_input: bool,
}

/// One assignment and the worker on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerView {
    pub assignment_id: String,
    pub title: String,
    pub status: AssignmentState,
    pub attempt: Option<AttemptState>,
    /// The session of the current worker, as an app session handle.
    pub session: Option<i64>,
    /// Sessions of workers that were stopped, cancelled, failed or replaced:
    /// nothing done there reports to the task.
    pub detached: Vec<i64>,
    pub provider: Option<String>,
    /// An account id, never a credential.
    pub account_id: Option<String>,
    pub workspace: Option<String>,
    pub branch: Option<String>,
    /// Waiting on an answer (from Mono or the user).
    pub needs_input: bool,
}

impl WorkerView {
    /// Its state from the store, refined by the live session when known.
    /// Only a current attempt that runs (or is starting with a busy
    /// session) counts as working; a held or stopped one never does.
    pub fn state(&self, live: Option<Live>) -> WorkerState {
        let needs_input = self.needs_input || live.is_some_and(|l| l.needs_input);
        match self.status {
            AssignmentState::WaitingForDecision => WorkerState::NeedsInput,
            AssignmentState::Running if needs_input && self.session.is_some() => {
                WorkerState::NeedsInput
            }
            AssignmentState::Running => match self.attempt {
                Some(AttemptState::Running | AttemptState::Waiting) if self.session.is_some() => {
                    WorkerState::Working
                }
                Some(AttemptState::Created | AttemptState::Starting) => {
                    if live.is_some_and(|l| l.busy) {
                        WorkerState::Working
                    } else {
                        WorkerState::Starting
                    }
                }
                _ => WorkerState::Stopped,
            },
            AssignmentState::NeedsReview => WorkerState::NeedsReview,
            AssignmentState::Ready => WorkerState::Ready,
            AssignmentState::Queued => WorkerState::Queued,
            AssignmentState::Accepted => WorkerState::Accepted,
            AssignmentState::Failed => WorkerState::Failed,
            AssignmentState::Cancelled => WorkerState::Cancelled,
        }
    }
}

/// An open decision of the task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecisionView {
    pub id: String,
    pub assignment_id: Option<String>,
    /// Mono asked the user; it waits for them.
    pub escalated: bool,
    /// Mono was reminded as often as it will be and has not answered.
    pub quiet: bool,
}

/// Counts of the task's acceptance criteria, never a percentage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VerificationProgress {
    pub required: usize,
    /// Required criteria whose latest live verification passed.
    pub passed: usize,
    /// Required criteria whose latest live verification failed.
    pub failed: usize,
}

/// One task as the monitoring card shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskMonitor {
    /// The id, title, revisions, display (Pausing included), the redacted
    /// reason and which lifecycle controls apply.
    pub control: ControlRow,
    /// What the user asked for (the request, secrets removed, bounded).
    pub objective: String,
    pub project: String,
    pub stage: TaskStage,
    /// Why the task waits (`waiting::DECISION`, `RESTART`, `TAKEOVER`).
    pub waiting_cause: Option<String>,
    pub activity: MonoActivity,
    pub workers: Vec<WorkerView>,
    pub decisions: Vec<DecisionView>,
    pub verification: VerificationProgress,
    /// Recovery and account changes, by account id, reasons redacted.
    pub recovery: Vec<String>,
    /// A recovery is scheduled and the task may carry it out.
    pub recovering: bool,
    /// Newest first.
    pub timeline: Vec<TimelineEntry>,
    pub last_worker_activity: Option<u64>,
    /// When a coordinator last synchronized the task (renewed its lease).
    pub last_status_sync: Option<u64>,
    pub last_mono_review: Option<u64>,
}

impl TaskMonitor {
    /// The latest meaningful event.
    pub fn latest(&self) -> Option<&TimelineEntry> {
        self.timeline.first()
    }

    /// Workers whose current attempt runs now, by the store.
    pub fn working(&self) -> usize {
        self.workers
            .iter()
            .filter(|w| w.state(None) == WorkerState::Working)
            .count()
    }

    /// Open decisions that wait for the user.
    pub fn for_user(&self) -> usize {
        self.decisions.iter().filter(|d| d.escalated).count()
    }

    /// This task's own badge (see [`Badge`] for the precedence).
    pub fn badge(&self) -> Option<Badge> {
        let display = self.control.display;
        if self.for_user() > 0 {
            Some(Badge::NeedsDecision)
        } else if self.recovering {
            Some(Badge::Recovering)
        } else if matches!(display, Display::Paused | Display::Pausing) {
            Some(Badge::Paused)
        } else if self.activity == MonoActivity::Reviewing {
            Some(Badge::Reviewing)
        } else {
            Some(self.working()).filter(|n| *n > 0).map(Badge::Working)
        }
    }
}

/// The monitoring view of every unfinished task `mono` leads, at `now`.
pub fn monitor(store: &Store, mono: &str, now: u64) -> Result<Vec<TaskMonitor>, TaskError> {
    let rows = lifecycle::controls(store, mono)?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let Some(tasks) = store.existing_tasks()? else {
        return Ok(Vec::new());
    };
    rows.into_iter()
        .map(|row| one(store, &tasks, row, now))
        .collect()
}

fn one(
    store: &Store,
    tasks: &TaskStore<'_>,
    mut control: ControlRow,
    now: u64,
) -> Result<TaskMonitor, TaskError> {
    let task = task_or_missing(tasks, &control.task_id)?;
    control.reason = control.reason.as_deref().map(safe);
    control.title = safe(&control.title);
    let events = timeline_events(tasks, &task.id)?;
    let (workers, names) = workers(store, tasks, &task)?;
    let decisions = decisions(tasks, &task.id)?;
    let (recovery, recovering) = recovery(tasks, &task, &names)?;
    let pending = pending_for_mono(tasks, &task.id)?;
    let activity = activity(&task, control.display, &decisions, &workers, &pending);
    Ok(TaskMonitor {
        waiting_cause: super::waiting::waiting_cause(tasks.conn, &task.id)?,
        objective: safe(&task.request),
        project: task.project.clone(),
        stage: task.stage,
        activity,
        verification: verification(tasks, &task)?,
        timeline: timeline::entries(&events, &names),
        last_worker_activity: latest(tasks, &task.id, FROM_WORKER)?,
        // A lease is renewed at each sync for its time to live; an expired
        // one says nothing about a recent sync.
        last_status_sync: tasks
            .lease(&lease_resource(&task.id))?
            .filter(|lease| lease.expires_at > now)
            .map(|lease| lease.expires_at.saturating_sub(LEASE_TTL_MS)),
        last_mono_review: latest(tasks, &task.id, FROM_MONO)?,
        workers,
        decisions,
        recovery,
        recovering,
        control,
    })
}

/// The newest events the timeline can show (a bounded tail, oldest first).
fn timeline_events(tasks: &TaskStore<'_>, task: &str) -> Result<Vec<Event>, TaskError> {
    let kinds = timeline::KINDS
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(",");
    // Some kinds make no entry (a superseded review), so read a margin.
    let limit = timeline::TIMELINE_LIMIT * 2;
    let mut statement = tasks.conn.prepare(&format!(
        "SELECT seq,id,assignment_id,attempt_id,kind,payload,created_at
         FROM native_task_events WHERE task_id=?1 AND kind IN ({kinds})
         ORDER BY seq DESC LIMIT {limit}"
    ))?;
    let rows = statement.query_map([task], |row| {
        let payload: String = row.get(5)?;
        Ok(Event {
            seq: row.get(0)?,
            id: row.get(1)?,
            task_id: task.into(),
            assignment_id: row.get(2)?,
            attempt_id: row.get(3)?,
            kind: row.get(4)?,
            payload: serde_json::from_str(&payload).unwrap_or_default(),
            created_at: read_ms(row, 6)?,
        })
    })?;
    let mut events = rows.collect::<Result<Vec<_>, _>>()?;
    events.reverse();
    Ok(events)
}

/// Something a worker did: a report, a question, a claim, or its start.
const FROM_WORKER: &str = "json_extract(payload,'$.requester')='worker'
    OR kind IN ('attempt.started','attempt.claim','worker.result','attempt.blocked',
                'decision.requested','decision.provider_question')";

/// A judgement Mono made: an answer, an acceptance or send-back of an
/// assignment, moving the task on from review, a note. Planning moves (a
/// plan saved, the task made ready) are not reviews.
const FROM_MONO: &str = "(json_extract(payload,'$.requester')='mono'
      AND (kind LIKE 'assignment.%'
           OR json_extract(payload,'$.from') IN ('reviewing','integrating','verifying')))
    OR json_extract(payload,'$.by')='mono' OR kind='assignment.mono_note'";

/// When the task's newest event matching `filter` happened (one indexed
/// aggregate, not a scan in the app).
fn latest(tasks: &TaskStore<'_>, task: &str, filter: &str) -> Result<Option<u64>, TaskError> {
    let at: Option<i64> = tasks.conn.query_row(
        &format!("SELECT MAX(created_at) FROM native_task_events WHERE task_id=?1 AND ({filter})"),
        [task],
        |row| row.get(0),
    )?;
    Ok(at.and_then(|at| u64::try_from(at).ok()))
}

fn workers(
    store: &Store,
    tasks: &TaskStore<'_>,
    task: &Task,
) -> Result<(Vec<WorkerView>, Names), TaskError> {
    let workspaces = tasks.workspaces(&task.id)?;
    let mut names = Names::default();
    let mut workers = Vec::new();
    let handle = |session: &Option<String>| {
        session
            .as_deref()
            .and_then(|s| store.resolve_session_id(s).ok())
    };
    for assignment in tasks.assignments(&task.id)? {
        let attempts = tasks.attempts(&task.id, &assignment.id)?;
        for attempt in &attempts {
            names.runners.insert(
                attempt.id.clone(),
                Runner {
                    provider: attempt.provider.clone(),
                    account_id: attempt.account_id.clone(),
                },
            );
        }
        let latest = attempts.last();
        let current = latest.filter(|a| {
            !matches!(
                a.status,
                AttemptState::Interrupted | AttemptState::Cancelled | AttemptState::Failed
            )
        });
        let detached = attempts
            .iter()
            .filter(|a| current.is_none_or(|c| c.id != a.id))
            .filter_map(|a| handle(&a.session_id))
            .collect();
        // The assignment's latest recorded workspace.
        let workspace = workspaces
            .iter()
            .rev()
            .find(|w| w.assignment_id.as_deref() == Some(assignment.id.as_str()));
        let title = safe(&assignment.title);
        names.titles.insert(assignment.id.clone(), title.clone());
        workers.push(WorkerView {
            needs_input: assignment.status == AssignmentState::WaitingForDecision
                || current.is_some_and(|a| a.status == AttemptState::Waiting),
            assignment_id: assignment.id,
            title,
            status: assignment.status,
            attempt: latest.map(|a| a.status),
            session: current.and_then(|a| handle(&a.session_id)),
            detached,
            provider: latest.map(|a| safe(&a.provider)),
            account_id: latest.and_then(|a| a.account_id.as_deref().map(safe)),
            workspace: workspace.map(|w| safe(&w.path)),
            branch: workspace.and_then(|w| w.branch.as_deref().map(safe)),
        });
    }
    Ok((workers, names))
}

fn decisions(tasks: &TaskStore<'_>, task: &str) -> Result<Vec<DecisionView>, TaskError> {
    let mut statement = tasks.conn.prepare(
        "SELECT id FROM native_task_decisions WHERE task_id=?1 AND state=?2 ORDER BY rowid",
    )?;
    let ids = statement
        .query_map([task, DecisionState::Open.as_str()], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut open = Vec::new();
    for id in ids {
        let Some(decision) = tasks.decision(&id)? else {
            continue;
        };
        let escalated = tasks.escalation(&id)?.is_some();
        open.push(DecisionView {
            quiet: !escalated && super::inbox::reminders_stopped(tasks, &id)?,
            id,
            assignment_id: decision.assignment_id,
            escalated,
        });
    }
    Ok(open)
}

/// Recovery rows still in effect, as lines naming accounts by id; and
/// whether one is scheduled for a task that may carry it out (a pause's own
/// rows wait for the resume, so they are not recovery).
fn recovery(
    tasks: &TaskStore<'_>,
    task: &Task,
    names: &Names,
) -> Result<(Vec<String>, bool), TaskError> {
    let mut statement = tasks.conn.prepare(&format!(
        "SELECT {COLUMNS} FROM native_task_recoveries WHERE task_id=?1
           AND state NOT IN ('replaced','superseded','abandoned') ORDER BY rowid"
    ))?;
    let rows = statement
        .query_map([&task.id], |row| {
            Ok(Recovery {
                attempt_id: row.get(0)?,
                task_id: row.get(1)?,
                assignment_id: row.get(2)?,
                root_attempt: row.get(3)?,
                retries_used: row.get(4)?,
                failure: row.get(5)?,
                reason: row.get(6)?,
                provider: row.get(7)?,
                failed_account: row.get(8)?,
                next_provider: row.get(9)?,
                next_account: row.get(10)?,
                state: row.get(11)?,
                due_at: read_ms(row, 12)?,
                replacement: row.get(13)?,
                revision: read_ms(row, 14)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let paused = super::lifecycle::PAUSED;
    let recovering = rows
        .iter()
        .any(|r| r.state == SCHEDULED && r.failure != paused)
        && task.lifecycle == super::TaskLifecycle::Active;
    let lines = rows
        .iter()
        .filter(|r| r.failure != paused)
        .map(|r| recovery_line(r, names))
        .collect();
    Ok((lines, recovering))
}

fn recovery_line(r: &Recovery, names: &Names) -> String {
    let who = names
        .titles
        .get(&r.assignment_id)
        .cloned()
        .unwrap_or_else(|| safe(&r.assignment_id));
    let from = match &r.failed_account {
        Some(account) => format!("{}/{account}", r.provider),
        None => r.provider.clone(),
    };
    let next = match (&r.next_provider, &r.next_account) {
        (Some(provider), Some(account)) if r.failed_account.as_ref() != Some(account) => {
            format!("switching to {provider}/{account}")
        }
        (Some(_), Some(_)) => "retrying on the same account".into(),
        _ => match r.state.as_str() {
            "held" => "held until resumed".into(),
            "blocked" | "block_pending" => "blocked".into(),
            "exhausted" => "out of retries".into(),
            "mono" => "handed to Mono".into(),
            other => safe(other),
        },
    };
    // Accounts are ids, but every stored text is redacted before it shows.
    safe(&format!(
        "{who}: {} on {from}, {next} (retry {} of {})",
        r.failure,
        r.retries_used,
        super::recovery::RETRY_BUDGET
    ))
}

/// The kinds of the task's events waiting in Mono's inbox (review hand-offs
/// and notices it has not acknowledged).
fn pending_for_mono(tasks: &TaskStore<'_>, task: &str) -> Result<Vec<String>, TaskError> {
    let mut statement = tasks.conn.prepare(
        "SELECT e.kind FROM native_task_deliveries d JOIN native_task_events e ON e.id=d.event_id
         WHERE e.task_id=?1 AND d.destination=?2 AND d.state<>'acknowledged'",
    )?;
    let kinds = statement
        .query_map([task, MONO_REVIEW], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(kinds)
}

fn activity(
    task: &Task,
    display: Display,
    decisions: &[DecisionView],
    workers: &[WorkerView],
    pending: &[String],
) -> MonoActivity {
    let assessing = pending.iter().any(|k| k == "task.steer_assess");
    if task.stage == TaskStage::Planning || assessing {
        MonoActivity::Planning
    } else if decisions.iter().any(|d| d.escalated)
        || matches!(
            display,
            Display::Waiting | Display::Paused | Display::Pausing | Display::Blocked
        )
    {
        MonoActivity::Waiting
    } else if decisions.iter().any(|d| !d.escalated) {
        MonoActivity::Deciding
    } else if !pending.is_empty()
        || matches!(
            task.stage,
            TaskStage::Reviewing | TaskStage::Integrating | TaskStage::Verifying
        )
        || workers
            .iter()
            .any(|w| w.status == AssignmentState::NeedsReview)
    {
        MonoActivity::Reviewing
    } else if workers
        .iter()
        .any(|w| w.state(None) == WorkerState::Working)
    {
        MonoActivity::Supervising
    } else {
        MonoActivity::Idle
    }
}

fn verification(tasks: &TaskStore<'_>, task: &Task) -> Result<VerificationProgress, TaskError> {
    let live: Vec<_> = tasks
        .verifications(&task.id)?
        .into_iter()
        .filter(|v| !v.invalidated)
        .collect();
    let mut latest: HashMap<&str, bool> = HashMap::new();
    for v in &live {
        if let Some(criterion) = &v.criterion_id {
            // Recorded oldest first, so the last one per criterion wins.
            latest.insert(criterion.as_str(), v.passed);
        }
    }
    let required: Vec<_> = task.criteria.iter().filter(|c| c.required).collect();
    let result = |passed| {
        required
            .iter()
            .filter(|c| latest.get(c.id.as_str()) == Some(&passed))
            .count()
    };
    Ok(VerificationProgress {
        required: required.len(),
        passed: result(true),
        failed: result(false),
    })
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
