//! Task persistence: additive `native_task*` tables and revision-checked writes.
use super::events::{append, NewEvent};
use super::model::*;
use super::states::*;
use super::TaskError;
use crate::orchestration::{Plan, WorkspaceMode};
use crate::store::Store;
use rusqlite::types::{ToSql, Type};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};
use rusqlite::{Transaction, TransactionBehavior};
use serde_json::json;

/// Only `CREATE ... IF NOT EXISTS` of new tables: the file is shared with the
/// desktop app, whose schema is never altered.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS native_tasks(id TEXT PRIMARY KEY, mono_id TEXT, project TEXT NOT NULL,
  title TEXT NOT NULL, request TEXT NOT NULL, stage TEXT NOT NULL, lifecycle TEXT NOT NULL,
  criteria TEXT NOT NULL, plan_title TEXT NOT NULL DEFAULT '', plan_summary TEXT NOT NULL DEFAULT '',
  plan_revision INTEGER NOT NULL DEFAULT 0, blocked_reason TEXT, revision INTEGER NOT NULL,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS native_task_assignments(task_id TEXT NOT NULL REFERENCES native_tasks(id),
  id TEXT NOT NULL, position INTEGER NOT NULL, title TEXT NOT NULL, prompt TEXT NOT NULL,
  files TEXT NOT NULL, depends_on TEXT NOT NULL, workspace TEXT, verification TEXT NOT NULL,
  status TEXT NOT NULL, revision INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  PRIMARY KEY(task_id,id));
CREATE TABLE IF NOT EXISTS native_task_attempts(id TEXT PRIMARY KEY, task_id TEXT NOT NULL,
  assignment_id TEXT NOT NULL, operation_id TEXT NOT NULL UNIQUE, provider TEXT NOT NULL,
  account_id TEXT, session_id TEXT, status TEXT NOT NULL, revision INTEGER NOT NULL,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  FOREIGN KEY(task_id,assignment_id) REFERENCES native_task_assignments(task_id,id));
CREATE TABLE IF NOT EXISTS native_task_workspaces(id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL REFERENCES native_tasks(id), assignment_id TEXT, mode TEXT NOT NULL,
  repository TEXT NOT NULL, path TEXT NOT NULL, branch TEXT, base_commit TEXT, owner TEXT NOT NULL,
  revision INTEGER NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS native_task_decisions(id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL REFERENCES native_tasks(id), assignment_id TEXT, attempt_id TEXT,
  question TEXT NOT NULL, context TEXT NOT NULL, scope TEXT NOT NULL, asked_by TEXT NOT NULL,
  state TEXT NOT NULL, answer TEXT, answered_by TEXT, revision INTEGER NOT NULL,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS native_task_verifications(id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL REFERENCES native_tasks(id), assignment_id TEXT, criterion_id TEXT,
  checkpoint_commit TEXT NOT NULL, check_name TEXT NOT NULL, passed INTEGER NOT NULL,
  evidence TEXT NOT NULL, invalidated INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS native_task_events(seq INTEGER PRIMARY KEY,
  id TEXT NOT NULL UNIQUE, task_id TEXT NOT NULL REFERENCES native_tasks(id), assignment_id TEXT,
  attempt_id TEXT, kind TEXT NOT NULL, payload TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS native_task_deliveries(id TEXT PRIMARY KEY,
  event_id TEXT NOT NULL REFERENCES native_task_events(id), destination TEXT NOT NULL,
  payload_ref TEXT NOT NULL, state TEXT NOT NULL, retries INTEGER NOT NULL, last_error TEXT,
  revision INTEGER NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS native_task_leases(resource TEXT PRIMARY KEY, holder TEXT NOT NULL,
  expires_at INTEGER NOT NULL, generation INTEGER NOT NULL);";

pub(crate) fn create_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)
}

pub(super) fn ms(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub(super) fn read_ms(row: &Row<'_>, index: usize) -> rusqlite::Result<u64> {
    Ok(u64::try_from(row.get::<_, i64>(index)?).unwrap_or(0))
}

fn conversion(index: usize, message: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, Type::Text, message.into())
}

fn parsed<T>(row: &Row<'_>, index: usize, parse: fn(&str) -> Option<T>) -> rusqlite::Result<T> {
    let text: String = row.get(index)?;
    parse(&text).ok_or_else(|| conversion(index, format!("unknown value {text}")))
}

fn json_column<T: serde::de::DeserializeOwned>(row: &Row<'_>, index: usize) -> rusqlite::Result<T> {
    let text: String = row.get(index)?;
    serde_json::from_str(&text).map_err(|e| conversion(index, e.to_string()))
}

/// Task records in the session database.
pub struct TaskStore<'a> {
    pub(super) conn: &'a Connection,
}

impl Store {
    pub fn tasks(&self) -> TaskStore<'_> {
        TaskStore {
            conn: self.connection(),
        }
    }
}

/// Locates one revisioned row.
pub(super) struct Key<'a> {
    table: &'static str,
    filter: &'static str,
    values: Vec<&'a str>,
    pub(super) label: String,
}

impl<'a> Key<'a> {
    pub(super) fn new(
        table: &'static str,
        filter: &'static str,
        values: Vec<&'a str>,
        kind: &str,
    ) -> Self {
        let label = format!("{kind}:{}", values.join("/"));
        Self {
            table,
            filter,
            values,
            label,
        }
    }

    /// Sets `set` (with `values` for its `?`s) and bumps the revision, only if
    /// the row is still at `expected`. Returns the new revision.
    pub(super) fn bump(
        &self,
        conn: &Connection,
        expected: u64,
        set: &str,
        values: Vec<&dyn ToSql>,
        now: u64,
    ) -> Result<u64, TaskError> {
        let (now, read) = (ms(now), ms(expected));
        let mut all: Vec<&dyn ToSql> = values;
        all.push(&now);
        all.extend(self.values.iter().map(|value| value as &dyn ToSql));
        all.push(&read);
        let sql = format!(
            "UPDATE {} SET {set},revision=revision+1,updated_at=? WHERE {} AND revision=?",
            self.table, self.filter
        );
        if conn.execute(&sql, params_from_iter(all))? == 1 {
            return Ok(expected + 1);
        }
        // Explain the refusal: missing or stale.
        guard(conn, self, "revision", expected)?;
        Err(TaskError::Storage(format!(
            "{} was not updated.",
            self.label
        )))
    }
}

impl Key<'_> {
    pub(super) fn revision(&self, conn: &Connection) -> Result<u64, TaskError> {
        let sql = format!("SELECT revision FROM {} WHERE {}", self.table, self.filter);
        conn.query_row(&sql, params_from_iter(&self.values), |row| read_ms(row, 0))
            .optional()?
            .ok_or_else(|| TaskError::NotFound(self.label.clone()))
    }
}

/// Reads `column` of the row if it is still at `expected`.
pub(super) fn guard(
    conn: &Connection,
    key: &Key<'_>,
    column: &str,
    expected: u64,
) -> Result<String, TaskError> {
    let sql = format!(
        "SELECT CAST({column} AS TEXT),revision FROM {} WHERE {}",
        key.table, key.filter
    );
    let (value, actual) = conn
        .query_row(&sql, params_from_iter(&key.values), |row| {
            Ok((row.get::<_, String>(0)?, read_ms(row, 1)?))
        })
        .optional()?
        .ok_or_else(|| TaskError::NotFound(key.label.clone()))?;
    if actual != expected {
        return Err(TaskError::Stale {
            subject: key.label.clone(),
            expected,
            actual,
        });
    }
    Ok(value)
}

const TASK: &str = "id,mono_id,project,title,request,stage,lifecycle,criteria,plan_revision,blocked_reason,revision,created_at,updated_at";
const ASSIGNMENT: &str = "id,title,prompt,files,depends_on,workspace,verification,status,revision";
const ATTEMPT: &str = "id,task_id,assignment_id,operation_id,provider,account_id,session_id,status,revision,created_at,updated_at";
const WORKSPACE: &str =
    "id,task_id,assignment_id,mode,repository,path,branch,base_commit,owner,revision,created_at";
const DECISION: &str = "id,task_id,assignment_id,attempt_id,question,context,scope,asked_by,state,answer,answered_by,revision,created_at,updated_at";
const VERIFICATION: &str = "id,task_id,assignment_id,criterion_id,checkpoint_commit,check_name,passed,evidence,invalidated,created_at";

fn task(row: &Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: row.get(0)?,
        mono_id: row.get(1)?,
        project: row.get(2)?,
        title: row.get(3)?,
        request: row.get(4)?,
        stage: parsed(row, 5, TaskStage::parse)?,
        lifecycle: parsed(row, 6, TaskLifecycle::parse)?,
        criteria: json_column(row, 7)?,
        plan_revision: read_ms(row, 8)?,
        blocked_reason: row.get(9)?,
        revision: read_ms(row, 10)?,
        created_at: read_ms(row, 11)?,
        updated_at: read_ms(row, 12)?,
    })
}

fn assignment(row: &Row<'_>) -> rusqlite::Result<Assignment> {
    let workspace: Option<String> = row.get(5)?;
    Ok(Assignment {
        id: row.get(0)?,
        title: row.get(1)?,
        prompt: row.get(2)?,
        files: json_column(row, 3)?,
        depends_on: json_column(row, 4)?,
        workspace: workspace
            .map(|mode| WorkspaceMode::parse(&mode).ok_or_else(|| conversion(5, mode.clone())))
            .transpose()?,
        verification: json_column(row, 6)?,
        status: parsed(row, 7, AssignmentState::parse)?,
        revision: read_ms(row, 8)?,
    })
}

fn attempt(row: &Row<'_>) -> rusqlite::Result<Attempt> {
    Ok(Attempt {
        id: row.get(0)?,
        task_id: row.get(1)?,
        assignment_id: row.get(2)?,
        operation_id: row.get(3)?,
        provider: row.get(4)?,
        account_id: row.get(5)?,
        session_id: row.get(6)?,
        status: parsed(row, 7, AttemptState::parse)?,
        revision: read_ms(row, 8)?,
        created_at: read_ms(row, 9)?,
        updated_at: read_ms(row, 10)?,
    })
}

fn workspace(row: &Row<'_>) -> rusqlite::Result<Workspace> {
    Ok(Workspace {
        id: row.get(0)?,
        task_id: row.get(1)?,
        assignment_id: row.get(2)?,
        mode: parsed(row, 3, WorkspaceMode::parse)?,
        repository: row.get(4)?,
        path: row.get(5)?,
        branch: row.get(6)?,
        base_commit: row.get(7)?,
        owner: row.get(8)?,
        revision: read_ms(row, 9)?,
        created_at: read_ms(row, 10)?,
    })
}

fn decision(row: &Row<'_>) -> rusqlite::Result<DecisionRequest> {
    let answered_by: Option<String> = row.get(10)?;
    Ok(DecisionRequest {
        id: row.get(0)?,
        task_id: row.get(1)?,
        assignment_id: row.get(2)?,
        attempt_id: row.get(3)?,
        question: row.get(4)?,
        context: row.get(5)?,
        scope: parsed(row, 6, DecisionScope::parse)?,
        asked_by: parsed(row, 7, Requester::parse)?,
        state: parsed(row, 8, DecisionState::parse)?,
        answer: row.get(9)?,
        answered_by: answered_by.as_deref().and_then(Requester::parse),
        revision: read_ms(row, 11)?,
        created_at: read_ms(row, 12)?,
        updated_at: read_ms(row, 13)?,
    })
}

fn verification(row: &Row<'_>) -> rusqlite::Result<Verification> {
    Ok(Verification {
        id: row.get(0)?,
        task_id: row.get(1)?,
        assignment_id: row.get(2)?,
        criterion_id: row.get(3)?,
        checkpoint_commit: row.get(4)?,
        check: row.get(5)?,
        passed: row.get(6)?,
        evidence: row.get(7)?,
        invalidated: row.get(8)?,
        created_at: read_ms(row, 9)?,
    })
}

pub(super) fn event_for(
    task: &str,
    label: &str,
    revision: u64,
    kind: &str,
    payload: serde_json::Value,
) -> NewEvent {
    NewEvent {
        id: format!("{label}@{revision}"),
        task_id: task.into(),
        assignment_id: None,
        attempt_id: None,
        kind: kind.into(),
        payload,
    }
}

impl TaskStore<'_> {
    /// Creates the tables if missing. Idempotent; never alters other tables.
    pub fn ensure_schema(&self) -> Result<(), TaskError> {
        Ok(create_tables(self.conn)?)
    }

    pub(super) fn begin(&self) -> Result<Transaction<'_>, TaskError> {
        Ok(Transaction::new_unchecked(
            self.conn,
            TransactionBehavior::Immediate,
        )?)
    }

    fn one<T>(
        &self,
        sql: &str,
        keys: &[&str],
        map: fn(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Option<T>, TaskError> {
        Ok(self
            .conn
            .query_row(sql, params_from_iter(keys), map)
            .optional()?)
    }

    fn all<T>(
        &self,
        sql: &str,
        keys: &[&str],
        map: fn(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>, TaskError> {
        let mut statement = self.conn.prepare(sql)?;
        let rows = statement.query_map(params_from_iter(keys), map)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// A new task: Planning and Active, at revision 1.
    pub fn create_task(&self, new: &NewTask, now: u64) -> Result<Task, TaskError> {
        let tx = self.begin()?;
        tx.execute(
            "INSERT INTO native_tasks(id,mono_id,project,title,request,stage,lifecycle,criteria,revision,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,'planning','active',?6,1,?7,?7)",
            params![new.id, new.mono_id, new.project, new.title, new.request,
                serde_json::to_string(&new.criteria)?, ms(now)],
        )?;
        let label = Subject::Task(&new.id).key().label;
        append(
            &tx,
            &event_for(&new.id, &label, 1, "task.created", json!({})),
            now,
        )?;
        tx.commit()?;
        self.task(&new.id)?.ok_or(TaskError::NotFound(label))
    }

    pub fn task(&self, id: &str) -> Result<Option<Task>, TaskError> {
        self.one(
            &format!("SELECT {TASK} FROM native_tasks WHERE id=?"),
            &[id],
            task,
        )
    }

    /// Replaces the task's acceptance criteria.
    pub fn update_criteria(
        &self,
        id: &str,
        expected_revision: u64,
        criteria: &[AcceptanceCriterion],
        now: u64,
    ) -> Result<u64, TaskError> {
        let criteria = serde_json::to_string(criteria)?;
        Subject::Task(id).key().bump(
            self.conn,
            expected_revision,
            "criteria=?",
            vec![&criteria],
            now,
        )
    }

    /// Replaces the plan's assignments, which start Queued at revision 1.
    /// Only while the task is Planning or Ready.
    pub fn save_plan(
        &self,
        id: &str,
        expected_revision: u64,
        plan: &Plan,
        now: u64,
    ) -> Result<u64, TaskError> {
        crate::orchestration::validate(plan).map_err(TaskError::Invalid)?;
        let tx = self.begin()?;
        let key = Subject::Task(id).key();
        let stage = guard(&tx, &key, "stage", expected_revision)?;
        if !matches!(
            TaskStage::parse(&stage),
            Some(TaskStage::Planning | TaskStage::Ready)
        ) {
            return Err(TaskError::Precondition(Precondition::NotStarted));
        }
        tx.execute("DELETE FROM native_task_assignments WHERE task_id=?1", [id])?;
        for (position, assignment) in plan.tasks.iter().enumerate() {
            tx.execute(
                "INSERT INTO native_task_assignments(task_id,id,position,title,prompt,files,depends_on,workspace,verification,status,revision,updated_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'queued',1,?10)",
                params![id, assignment.id, position as i64, assignment.title, assignment.prompt,
                    serde_json::to_string(&assignment.files)?,
                    serde_json::to_string(&assignment.depends_on)?,
                    assignment.workspace.map(WorkspaceMode::as_str),
                    serde_json::to_string(&assignment.verification)?, ms(now)],
            )?;
        }
        let revision = key.bump(
            &tx,
            expected_revision,
            "plan_title=?,plan_summary=?,plan_revision=plan_revision+1",
            vec![&plan.title, &plan.summary],
            now,
        )?;
        let payload = json!({"assignments": plan.tasks.len()});
        append(
            &tx,
            &event_for(id, &key.label, revision, "task.plan_saved", payload),
            now,
        )?;
        tx.commit()?;
        Ok(revision)
    }

    /// The saved plan with each assignment's durable status and revision.
    pub fn plan(&self, id: &str) -> Result<Option<Plan>, TaskError> {
        let Some((title, summary)) = self.one(
            "SELECT plan_title,plan_summary FROM native_tasks WHERE id=?",
            &[id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?
        else {
            return Ok(None);
        };
        Ok(Some(Plan {
            title,
            summary,
            tasks: self.assignments(id)?,
        }))
    }

    pub fn assignments(&self, task_id: &str) -> Result<Vec<Assignment>, TaskError> {
        self.all(
            &format!("SELECT {ASSIGNMENT} FROM native_task_assignments WHERE task_id=? ORDER BY position"),
            &[task_id],
            assignment,
        )
    }

    pub fn assignment(&self, task_id: &str, id: &str) -> Result<Option<Assignment>, TaskError> {
        self.one(
            &format!("SELECT {ASSIGNMENT} FROM native_task_assignments WHERE task_id=? AND id=?"),
            &[task_id, id],
            assignment,
        )
    }

    /// Writes an assignment's requirements, checked against its `revision`.
    /// Its status changes only through [`TaskStore::transition`].
    pub fn update_assignment(
        &self,
        task_id: &str,
        assignment: &Assignment,
        now: u64,
    ) -> Result<u64, TaskError> {
        let (files, depends_on, verification) = (
            serde_json::to_string(&assignment.files)?,
            serde_json::to_string(&assignment.depends_on)?,
            serde_json::to_string(&assignment.verification)?,
        );
        let workspace = assignment.workspace.map(WorkspaceMode::as_str);
        Subject::Assignment {
            task: task_id,
            id: &assignment.id,
        }
        .key()
        .bump(
            self.conn,
            assignment.revision,
            "title=?,prompt=?,files=?,depends_on=?,workspace=?,verification=?",
            vec![
                &assignment.title,
                &assignment.prompt,
                &files,
                &depends_on,
                &workspace,
                &verification,
            ],
            now,
        )
    }

    /// Records a launch intent as a Created attempt. A retry with the same
    /// operation id returns the attempt already recorded.
    pub fn create_attempt(&self, new: &NewAttempt, now: u64) -> Result<Attempt, TaskError> {
        let inserted = self.conn.execute(
            "INSERT INTO native_task_attempts(id,task_id,assignment_id,operation_id,provider,account_id,status,revision,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,'created',1,?7,?7) ON CONFLICT(operation_id) DO NOTHING",
            params![new.id, new.task_id, new.assignment_id, new.operation_id, new.provider, new.account_id, ms(now)],
        )?;
        let attempt = self
            .one(
                &format!("SELECT {ATTEMPT} FROM native_task_attempts WHERE operation_id=?"),
                &[&new.operation_id],
                attempt,
            )?
            .ok_or_else(|| TaskError::NotFound(format!("operation:{}", new.operation_id)))?;
        if inserted == 1 {
            let label = Subject::Attempt(&attempt.id).key().label;
            let mut event = event_for(&attempt.task_id, &label, 1, "attempt.created", json!({}));
            event.assignment_id = Some(attempt.assignment_id.clone());
            event.attempt_id = Some(attempt.id.clone());
            append(self.conn, &event, now)?;
        }
        Ok(attempt)
    }

    pub fn attempt(&self, id: &str) -> Result<Option<Attempt>, TaskError> {
        self.one(
            &format!("SELECT {ATTEMPT} FROM native_task_attempts WHERE id=?"),
            &[id],
            attempt,
        )
    }

    pub fn attempts(&self, task_id: &str, assignment_id: &str) -> Result<Vec<Attempt>, TaskError> {
        self.all(
            &format!("SELECT {ATTEMPT} FROM native_task_attempts WHERE task_id=? AND assignment_id=? ORDER BY created_at,rowid"),
            &[task_id, assignment_id],
            attempt,
        )
    }

    /// Records the worker session a launch resolved to.
    pub fn bind_attempt_session(
        &self,
        id: &str,
        expected_revision: u64,
        session_id: &str,
        now: u64,
    ) -> Result<u64, TaskError> {
        Subject::Attempt(id).key().bump(
            self.conn,
            expected_revision,
            "session_id=?",
            vec![&session_id],
            now,
        )
    }

    /// Records a workspace before launch, at revision 1.
    pub fn save_workspace(&self, w: &Workspace) -> Result<Workspace, TaskError> {
        self.conn.execute(
            "INSERT INTO native_task_workspaces(id,task_id,assignment_id,mode,repository,path,branch,base_commit,owner,revision,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,1,?10,?10)",
            params![w.id, w.task_id, w.assignment_id, w.mode.as_str(), w.repository, w.path, w.branch, w.base_commit, w.owner, ms(w.created_at)],
        )?;
        self.one(
            &format!("SELECT {WORKSPACE} FROM native_task_workspaces WHERE id=?"),
            &[&w.id],
            workspace,
        )?
        .ok_or_else(|| TaskError::NotFound(format!("workspace:{}", w.id)))
    }

    pub fn workspaces(&self, task_id: &str) -> Result<Vec<Workspace>, TaskError> {
        self.all(
            &format!("SELECT {WORKSPACE} FROM native_task_workspaces WHERE task_id=? ORDER BY created_at,rowid"),
            &[task_id],
            workspace,
        )
    }

    pub fn create_decision(
        &self,
        new: &NewDecision,
        now: u64,
    ) -> Result<DecisionRequest, TaskError> {
        let tx = self.begin()?;
        tx.execute(
            "INSERT INTO native_task_decisions(id,task_id,assignment_id,attempt_id,question,context,scope,asked_by,state,revision,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'open',1,?9,?9)",
            params![new.id, new.task_id, new.assignment_id, new.attempt_id, new.question, new.context,
                new.scope.as_str(), new.asked_by.as_str(), ms(now)],
        )?;
        let mut event = event_for(
            &new.task_id,
            &format!("decision:{}", new.id),
            1,
            "decision.requested",
            json!({"scope": new.scope}),
        );
        event.assignment_id.clone_from(&new.assignment_id);
        event.attempt_id.clone_from(&new.attempt_id);
        append(&tx, &event, now)?;
        tx.commit()?;
        self.decision(&new.id)?
            .ok_or_else(|| TaskError::NotFound(format!("decision:{}", new.id)))
    }

    pub fn decision(&self, id: &str) -> Result<Option<DecisionRequest>, TaskError> {
        self.one(
            &format!("SELECT {DECISION} FROM native_task_decisions WHERE id=?"),
            &[id],
            decision,
        )
    }

    /// Answers an Open decision. A stale or superseded answer is refused.
    pub fn answer_decision(
        &self,
        id: &str,
        expected_revision: u64,
        answer: &str,
        by: Requester,
        now: u64,
    ) -> Result<u64, TaskError> {
        let by = by.as_str();
        self.decide(
            id,
            expected_revision,
            DecisionState::Open,
            DecisionState::Answered,
            "answer=?,answered_by=?",
            vec![&answer, &by],
            now,
        )
    }

    /// Resolves an Answered decision once the asker acknowledged the answer.
    pub fn resolve_decision(
        &self,
        id: &str,
        expected_revision: u64,
        now: u64,
    ) -> Result<u64, TaskError> {
        self.decide(
            id,
            expected_revision,
            DecisionState::Answered,
            DecisionState::Resolved,
            "answered_by=answered_by",
            vec![],
            now,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn decide(
        &self,
        id: &str,
        expected_revision: u64,
        from: DecisionState,
        to: DecisionState,
        set: &str,
        values: Vec<&dyn ToSql>,
        now: u64,
    ) -> Result<u64, TaskError> {
        let tx = self.begin()?;
        let key = Key::new("native_task_decisions", "id=?", vec![id], "decision");
        let state = guard(&tx, &key, "state", expected_revision)?;
        if DecisionState::parse(&state) != Some(from) {
            return Err(TaskError::InvalidState {
                subject: key.label,
                state,
            });
        }
        let task_id: String = tx.query_row(
            "SELECT task_id FROM native_task_decisions WHERE id=?1",
            [id],
            |row| row.get(0),
        )?;
        let state = to.as_str();
        let mut all: Vec<&dyn ToSql> = vec![&state];
        all.extend(values);
        let revision = key.bump(&tx, expected_revision, &format!("state=?,{set}"), all, now)?;
        let kind = format!("decision.{}", to.as_str());
        append(
            &tx,
            &event_for(&task_id, &key.label, revision, &kind, json!({})),
            now,
        )?;
        tx.commit()?;
        Ok(revision)
    }

    pub fn record_verification(&self, v: &Verification) -> Result<(), TaskError> {
        self.conn.execute(
            "INSERT INTO native_task_verifications(id,task_id,assignment_id,criterion_id,checkpoint_commit,check_name,passed,evidence,invalidated,created_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![v.id, v.task_id, v.assignment_id, v.criterion_id, v.checkpoint_commit, v.check, v.passed, v.evidence, v.invalidated, ms(v.created_at)],
        )?;
        Ok(())
    }

    pub fn verifications(&self, task_id: &str) -> Result<Vec<Verification>, TaskError> {
        self.all(
            &format!("SELECT {VERIFICATION} FROM native_task_verifications WHERE task_id=? ORDER BY created_at,rowid"),
            &[task_id],
            verification,
        )
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
pub(super) mod tests;
