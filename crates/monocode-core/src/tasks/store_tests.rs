//! Durable task store behaviour against an in-memory database.
use super::*;
use crate::orchestration::{Plan, WorkspaceMode};
use crate::store::Store;
use crate::tasks::states::State;
use crate::tasks::{DeliveryOutcome, DeliveryState, NewEvent};
use std::path::Path;

pub(in crate::tasks) fn memory() -> Store {
    Store::open(Path::new(":memory:")).unwrap()
}

pub(in crate::tasks) fn new_task(id: &str) -> NewTask {
    NewTask {
        id: id.into(),
        mono_id: Some("mono-1".into()),
        project: "/repo".into(),
        title: "Ship it".into(),
        request: "Implement the feature".into(),
        criteria: vec![AcceptanceCriterion {
            id: "c1".into(),
            description: "Tests pass".into(),
            required: true,
            checks: Vec::new(),
        }],
    }
}

pub(in crate::tasks) fn assignment(id: &str, deps: &[&str]) -> Assignment {
    Assignment {
        id: id.into(),
        title: format!("Assignment {id}"),
        prompt: "Do the work".into(),
        files: vec!["src".into()],
        depends_on: deps.iter().map(|d| (*d).to_owned()).collect(),
        workspace: Some(WorkspaceMode::Worktree),
        verification: vec!["cargo test".into()],
        ..Assignment::default()
    }
}

pub(in crate::tasks) fn plan(tasks: Vec<Assignment>) -> Plan {
    Plan {
        title: "Plan".into(),
        summary: "Approach".into(),
        tasks,
    }
}

pub(in crate::tasks) fn new_attempt(id: &str, task: &str, assignment: &str) -> NewAttempt {
    NewAttempt {
        id: id.into(),
        task_id: task.into(),
        assignment_id: assignment.into(),
        operation_id: format!("launch-{id}"),
        provider: "codex".into(),
        account_id: Some("account-7".into()),
    }
}

/// Puts a subject straight into a state, as a fixture.
pub(in crate::tasks) fn force(store: &Store, subject: Subject<'_>, state: State) {
    let (table, keys) = match subject {
        Subject::Task(id) => ("native_tasks", format!("id='{id}'")),
        Subject::Assignment { task, id } => (
            "native_task_assignments",
            format!("task_id='{task}' AND id='{id}'"),
        ),
        Subject::Attempt(id) => ("native_task_attempts", format!("id='{id}'")),
    };
    store
        .connection()
        .execute(
            &format!(
                "UPDATE {table} SET {}='{}' WHERE {keys}",
                state.column(),
                state.as_str()
            ),
            [],
        )
        .unwrap();
}

pub(in crate::tasks) fn revision(store: &Store, subject: Subject<'_>) -> u64 {
    store.tasks().unwrap().revision(subject).unwrap()
}

#[test]
fn schema_is_additive_and_idempotent_on_the_shared_desktop_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("monocode.db");
    let store = Store::open_monocode(&path).unwrap();
    let before: Vec<String> = sql_column(&store, "SELECT type || ' ' || name || ' ' || COALESCE(sql,'') FROM sqlite_master WHERE tbl_name NOT LIKE 'native_task%' ORDER BY name");
    store.tasks().unwrap();
    store.tasks().unwrap();
    let after: Vec<String> = sql_column(&store, "SELECT type || ' ' || name || ' ' || COALESCE(sql,'') FROM sqlite_master WHERE tbl_name NOT LIKE 'native_task%' ORDER BY name");
    assert_eq!(before, after, "desktop schema untouched");
    let tables = sql_column(&store, "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'native_task%' ORDER BY name");
    assert_eq!(
        tables,
        [
            "native_task_assignments",
            "native_task_attempts",
            "native_task_decisions",
            "native_task_deliveries",
            "native_task_events",
            "native_task_leases",
            "native_task_recoveries",
            "native_task_verifications",
            "native_task_workspaces",
            "native_tasks",
        ]
    );
    store
        .tasks()
        .unwrap()
        .create_task(&new_task("t"), 1)
        .unwrap();
    assert!(store.tasks().unwrap().task("t").unwrap().is_some());
}

fn sql_column(store: &Store, query: &str) -> Vec<String> {
    let mut statement = store.connection().prepare(query).unwrap();
    let rows = statement
        .query_map([], |row| row.get::<_, Option<String>>(0))
        .unwrap();
    rows.map(|row| row.unwrap().unwrap_or_default()).collect()
}

#[test]
fn task_and_plan_round_trip() {
    let store = memory();
    let tasks = store.tasks().unwrap();
    let created = tasks.create_task(&new_task("t"), 10).unwrap();
    assert_eq!(created.stage, TaskStage::Planning);
    assert_eq!(created.lifecycle, TaskLifecycle::Active);
    assert_eq!(created.revision, 1);
    assert_eq!(tasks.task("t").unwrap().unwrap(), created);
    assert!(tasks.task("missing").unwrap().is_none());
    assert!(matches!(
        tasks.create_task(&new_task("t"), 11),
        Err(TaskError::Storage(_))
    ));

    let revision = tasks
        .save_plan(
            "t",
            1,
            &plan(vec![assignment("a", &[]), assignment("b", &["a"])]),
            20,
        )
        .unwrap();
    assert_eq!(revision, 2);
    let task = tasks.task("t").unwrap().unwrap();
    assert_eq!(task.plan_revision, 1);
    assert_eq!(task.updated_at, 20);
    let saved = tasks.assignments("t").unwrap();
    assert_eq!(saved.len(), 2);
    let expected = Assignment {
        status: AssignmentState::Queued,
        revision: 1,
        ..assignment("b", &["a"])
    };
    assert_eq!(saved[1], expected);
    assert_eq!(tasks.assignment("t", "b").unwrap().unwrap(), expected);
    let stored = tasks.plan("t").unwrap().unwrap();
    assert_eq!(
        (stored.title.as_str(), stored.summary.as_str()),
        ("Plan", "Approach")
    );
    assert_eq!(stored.tasks, saved);
    assert!(tasks.plan("missing").unwrap().is_none());
}

#[test]
fn attempt_workspace_decision_and_verification_round_trip() {
    let store = memory();
    let tasks = store.tasks().unwrap();
    tasks.create_task(&new_task("t"), 1).unwrap();
    tasks
        .save_plan("t", 1, &plan(vec![assignment("a", &[])]), 2)
        .unwrap();

    let attempt = tasks
        .create_attempt(&new_attempt("x", "t", "a"), 3)
        .unwrap();
    assert_eq!(attempt.status, AttemptState::Created);
    assert_eq!(attempt.account_id.as_deref(), Some("account-7"));
    assert_eq!(attempt.session_id, None);
    let bound = tasks.bind_attempt_session("x", 1, "session-9", 4).unwrap();
    assert_eq!(bound, 2);
    let attempt = tasks.attempt("x").unwrap().unwrap();
    assert_eq!(attempt.session_id.as_deref(), Some("session-9"));
    assert_eq!(tasks.attempts("t", "a").unwrap(), vec![attempt]);
    // A retried launch intent with the same operation id is one attempt.
    let mut retried = new_attempt("y", "t", "a");
    retried.operation_id = "launch-x".into();
    assert_eq!(tasks.create_attempt(&retried, 5).unwrap().id, "x");

    let workspace = Workspace {
        id: "w".into(),
        task_id: "t".into(),
        assignment_id: Some("a".into()),
        mode: WorkspaceMode::Worktree,
        repository: "/repo".into(),
        path: "/repo-worktrees/t-a".into(),
        branch: Some("task/t-a".into()),
        base_commit: Some("abc123".into()),
        owner: "coordinator-1".into(),
        revision: 1,
        created_at: 6,
    };
    assert_eq!(tasks.save_workspace(&workspace).unwrap(), workspace);
    assert_eq!(tasks.workspaces("t").unwrap(), vec![workspace]);

    let decision = tasks
        .create_decision(
            &NewDecision {
                id: "d".into(),
                task_id: "t".into(),
                assignment_id: Some("a".into()),
                attempt_id: Some("x".into()),
                question: "Which crate?".into(),
                context: "Two candidates".into(),
                scope: DecisionScope::Assignment,
                asked_by: Requester::Worker,
            },
            7,
        )
        .unwrap();
    assert_eq!(decision.state, DecisionState::Open);
    assert_eq!(tasks.decision("d").unwrap().unwrap(), decision);
    let answered = tasks
        .answer_decision("d", 1, "Use core", Requester::Mono, 8)
        .unwrap();
    assert_eq!(answered, 2);
    let decision = tasks.decision("d").unwrap().unwrap();
    assert_eq!(decision.state, DecisionState::Answered);
    assert_eq!(decision.answer.as_deref(), Some("Use core"));
    assert_eq!(decision.answered_by, Some(Requester::Mono));
    tasks.resolve_decision("d", 2, 9).unwrap();
    assert_eq!(
        tasks.decision("d").unwrap().unwrap().state,
        DecisionState::Resolved
    );

    let verification = Verification {
        id: "v".into(),
        task_id: "t".into(),
        assignment_id: Some("a".into()),
        criterion_id: Some("c1".into()),
        checkpoint_commit: "def456".into(),
        check: "cargo test".into(),
        passed: true,
        evidence: "606 passed".into(),
        invalidated: false,
        created_at: 10,
    };
    tasks.record_verification(&verification).unwrap();
    assert_eq!(tasks.verifications("t").unwrap(), vec![verification]);
}

#[test]
fn recording_a_verification_again_returns_the_stored_one() {
    let store = memory();
    let tasks = store.tasks().unwrap();
    tasks.create_task(&new_task("t"), 1).unwrap();
    let first = Verification {
        id: "v1".into(),
        task_id: "t".into(),
        assignment_id: None,
        criterion_id: Some("c1".into()),
        checkpoint_commit: "abc".into(),
        check: "cargo test".into(),
        passed: true,
        evidence: "{}".into(),
        invalidated: false,
        created_at: 10,
    };
    tasks.record_verification(&first).unwrap();
    let retried = Verification {
        passed: false,
        created_at: 11,
        ..first.clone()
    };
    let stored = tasks.record_verification(&retried).unwrap();
    assert_eq!(stored, first);
    assert_eq!(tasks.verifications("t").unwrap(), vec![first]);
}

#[test]
fn stale_revisions_are_rejected_without_changes() {
    let store = memory();
    let tasks = store.tasks().unwrap();
    tasks.create_task(&new_task("t"), 1).unwrap();
    tasks
        .save_plan("t", 1, &plan(vec![assignment("a", &[])]), 2)
        .unwrap();
    let stale = |result: Result<u64, TaskError>, actual| match result {
        Err(TaskError::Stale {
            expected,
            actual: found,
            ..
        }) => {
            assert_eq!((expected, found), (1, actual))
        }
        other => panic!("expected stale, got {other:?}"),
    };
    // Task requirements and plan.
    stale(
        tasks.save_plan("t", 1, &plan(vec![assignment("z", &[])]), 3),
        2,
    );
    let criteria = vec![AcceptanceCriterion {
        id: "c2".into(),
        description: "Docs updated".into(),
        required: false,
        checks: vec!["cargo doc".into()],
    }];
    stale(tasks.update_criteria("t", 1, &criteria, 3), 2);
    assert_eq!(tasks.update_criteria("t", 2, &criteria, 3).unwrap(), 3);
    assert_eq!(tasks.task("t").unwrap().unwrap().criteria, criteria);
    // Assignment requirements: a stale writer cannot overwrite newer ones.
    let newer = Assignment {
        prompt: "Newer requirements".into(),
        revision: 1,
        ..assignment("a", &[])
    };
    assert_eq!(tasks.update_assignment("t", &newer, 4).unwrap(), 2);
    let older = Assignment {
        prompt: "Older requirements".into(),
        revision: 1,
        ..assignment("a", &[])
    };
    stale(tasks.update_assignment("t", &older, 5), 2);
    assert_eq!(
        tasks.assignment("t", "a").unwrap().unwrap().prompt,
        "Newer requirements"
    );
    // Transitions and decisions.
    stale(
        tasks.transition(
            Subject::Task("t"),
            State::Stage(TaskStage::Ready),
            Requester::Mono,
            1,
            6,
        ),
        3,
    );
    tasks
        .create_decision(
            &NewDecision {
                id: "d".into(),
                task_id: "t".into(),
                assignment_id: None,
                attempt_id: None,
                question: "Q".into(),
                context: String::new(),
                scope: DecisionScope::Task,
                asked_by: Requester::Mono,
            },
            7,
        )
        .unwrap();
    tasks
        .answer_decision("d", 1, "First", Requester::User, 8)
        .unwrap();
    stale(
        tasks.answer_decision("d", 1, "Superseded", Requester::User, 9),
        2,
    );
    assert_eq!(
        tasks.decision("d").unwrap().unwrap().answer.as_deref(),
        Some("First")
    );
    assert!(matches!(
        tasks.transition(
            Subject::Task("nope"),
            State::Stage(TaskStage::Ready),
            Requester::Mono,
            1,
            1
        ),
        Err(TaskError::NotFound(_))
    ));
    // A plan cannot be replaced once work has started.
    tasks
        .transition(
            Subject::Task("t"),
            State::Stage(TaskStage::Ready),
            Requester::Mono,
            3,
            10,
        )
        .unwrap();
    tasks
        .transition(
            Subject::Task("t"),
            State::Stage(TaskStage::Implementing),
            Requester::Coordinator,
            4,
            11,
        )
        .unwrap();
    assert!(matches!(
        tasks.save_plan("t", 5, &plan(vec![assignment("z", &[])]), 12),
        Err(TaskError::Precondition(_))
    ));
}

#[test]
fn events_replay_idempotently_in_sequence() {
    let store = memory();
    let tasks = store.tasks().unwrap();
    tasks.create_task(&new_task("t"), 1).unwrap();
    let event = NewEvent {
        id: "op-1".into(),
        task_id: "t".into(),
        assignment_id: Some("a".into()),
        attempt_id: None,
        kind: "worker.result".into(),
        payload: serde_json::json!({"claim": "done"}),
    };
    let first = tasks.append_event(&event, 2).unwrap();
    let replayed = tasks.append_event(&event, 3).unwrap();
    assert_eq!(first, replayed, "a replay returns the stored event");
    let second = tasks
        .append_event(
            &NewEvent {
                id: "op-2".into(),
                ..event.clone()
            },
            4,
        )
        .unwrap();
    assert!(second.seq > first.seq);
    let all = tasks.events_after(0, 100).unwrap();
    assert_eq!(all.iter().filter(|e| e.id == "op-1").count(), 1);
    assert_eq!(all.last().unwrap(), &second);
    assert_eq!(all[0].kind, "task.created");
    assert_eq!(
        tasks.events_after(first.seq, 100).unwrap(),
        vec![second.clone()]
    );
    assert_eq!(tasks.events_after(0, 1).unwrap().len(), 1);
    assert_eq!(first.payload["claim"], "done");
    assert_eq!(first.created_at, 2);
}

#[test]
fn deliveries_record_destination_payload_state_and_retries() {
    let store = memory();
    let tasks = store.tasks().unwrap();
    tasks.create_task(&new_task("t"), 1).unwrap();
    let event = tasks.events_after(0, 1).unwrap().remove(0);
    let delivery = tasks
        .enqueue_delivery("dl-1", &event.id, "mono:mono-1", "event:task:t@1", 2)
        .unwrap();
    assert_eq!(delivery.state, DeliveryState::Pending);
    assert_eq!((delivery.retries, delivery.revision), (0, 1));
    // Enqueueing again is idempotent.
    assert_eq!(
        tasks
            .enqueue_delivery("dl-1", &event.id, "mono:other", "other", 3)
            .unwrap(),
        delivery
    );
    assert_eq!(
        tasks.pending_deliveries("mono:mono-1").unwrap(),
        vec![delivery]
    );
    assert_eq!(
        tasks
            .record_delivery("dl-1", 1, DeliveryOutcome::Failed("busy".into()), 4)
            .unwrap(),
        2
    );
    let retried = tasks.pending_deliveries("mono:mono-1").unwrap().remove(0);
    assert_eq!(retried.retries, 1);
    assert_eq!(retried.last_error.as_deref(), Some("busy"));
    assert!(matches!(
        tasks.record_delivery("dl-1", 1, DeliveryOutcome::Delivered, 5),
        Err(TaskError::Stale { .. })
    ));
    tasks
        .record_delivery("dl-1", 2, DeliveryOutcome::Delivered, 5)
        .unwrap();
    tasks
        .record_delivery("dl-1", 3, DeliveryOutcome::Acknowledged, 6)
        .unwrap();
    assert!(tasks.pending_deliveries("mono:mono-1").unwrap().is_empty());
    let done = tasks.delivery("dl-1").unwrap().unwrap();
    assert_eq!(done.state, DeliveryState::Acknowledged);
    assert_eq!(done.updated_at, 6);
    // Acknowledged deliveries are final.
    assert!(matches!(
        tasks.record_delivery("dl-1", 4, DeliveryOutcome::Delivered, 7),
        Err(TaskError::InvalidState { .. })
    ));
}

#[test]
fn leases_expire_and_can_be_taken_over() {
    let store = memory();
    let tasks = store.tasks().unwrap();
    let lease = tasks.acquire_lease("task:t", "gui", 1_000, 100).unwrap();
    assert_eq!(lease.holder, "gui");
    assert_eq!(lease.expires_at, 1_100);
    assert_eq!(lease.generation, 1);
    // The Tauri app cannot take a live lease.
    assert_eq!(
        tasks.acquire_lease("task:t", "tauri", 1_000, 500),
        Err(TaskError::LeaseHeld {
            resource: "task:t".into(),
            holder: "gui".into(),
            expires_at: 1_100,
        })
    );
    // The holder renews without a new generation.
    let renewed = tasks.acquire_lease("task:t", "gui", 1_000, 600).unwrap();
    assert_eq!((renewed.expires_at, renewed.generation), (1_600, 1));
    // After expiry another holder takes over with a new generation.
    let taken = tasks
        .acquire_lease("task:t", "tauri", 1_000, 1_600)
        .unwrap();
    assert_eq!((taken.holder.as_str(), taken.generation), ("tauri", 2));
    assert_eq!(tasks.lease("task:t").unwrap(), Some(taken));
    // Only the holder releases.
    assert!(!tasks.release_lease("task:t", "gui").unwrap());
    assert!(tasks.release_lease("task:t", "tauri").unwrap());
    assert_eq!(tasks.lease("task:t").unwrap(), None);
    // The fencing generation keeps growing across a release.
    let again = tasks.acquire_lease("task:t", "gui", 10, 2_000).unwrap();
    assert_eq!(again.generation, 3);
    assert!(matches!(
        tasks.acquire_lease("task:t", "", 10, 2_000),
        Err(TaskError::Invalid(_))
    ));
}

#[test]
fn replanning_never_reuses_assignment_revisions_or_event_ids() {
    let store = memory();
    let tasks = store.tasks().unwrap();
    tasks.create_task(&new_task("t"), 1).unwrap();
    tasks
        .save_plan(
            "t",
            1,
            &plan(vec![assignment("a", &[]), assignment("b", &[])]),
            2,
        )
        .unwrap();
    let a = Subject::Assignment { task: "t", id: "a" };
    let ready = State::Assignment(AssignmentState::Ready);
    tasks
        .transition(a, ready, Requester::Coordinator, 1, 3)
        .unwrap();
    // A writer read plan v1's assignment at revision 1.
    let stale = tasks.assignment("t", "b").unwrap().unwrap();
    // Re-plan; b is dropped then comes back in v3.
    tasks
        .save_plan("t", 2, &plan(vec![assignment("a", &[])]), 4)
        .unwrap();
    tasks
        .save_plan(
            "t",
            3,
            &plan(vec![assignment("a", &[]), assignment("b", &[])]),
            5,
        )
        .unwrap();
    let replanned = tasks.assignments("t").unwrap();
    assert!(replanned.iter().all(|x| x.revision > 2), "{replanned:?}");
    assert!(matches!(
        tasks.update_assignment(
            "t",
            &Assignment {
                prompt: "Stale".into(),
                ..stale
            },
            6
        ),
        Err(TaskError::Stale { .. })
    ));
    // The same transition after a re-plan is a new event.
    tasks
        .transition(a, ready, Requester::Coordinator, revision(&store, a), 7)
        .unwrap();
    let readied = tasks
        .events_after(0, 1_000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "assignment.ready")
        .count();
    assert_eq!(readied, 2);
}

#[test]
fn writable_desktop_stores_create_task_tables_on_first_use_and_readers_never_do() {
    let task_tables = |store: &Store| {
        sql_column(
            store,
            "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'native_task%'",
        )
        .len()
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("monocode.db");
    // An existing desktop database from before task tables existed.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(include_str!("../store/shared_schema.sql"))
        .unwrap();
    drop(conn);
    let original = std::fs::read(&path).unwrap();
    let reader = Store::read_monocode(&path).unwrap();
    assert!(reader.tasks().unwrap().task("t").is_err());
    assert_eq!(task_tables(&reader), 0, "a reader creates nothing");
    drop(reader);
    let store = Store::open_monocode(&path).unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        original,
        "opening changes nothing"
    );
    store
        .tasks()
        .unwrap()
        .create_task(&new_task("t"), 1)
        .unwrap();
    assert_eq!(task_tables(&store), 10);
    drop(store);
    let reader = Store::read_monocode(&path).unwrap();
    assert!(reader.tasks().unwrap().task("t").unwrap().is_some());
    let memory = Store::open_monocode(Path::new(":memory:")).unwrap();
    memory
        .tasks()
        .unwrap()
        .create_task(&new_task("t"), 1)
        .unwrap();
    assert_eq!(task_tables(&memory), 10);
}

#[test]
fn the_integration_id_is_reserved_for_the_integration_branch() {
    for reserved in ["integration", "Integration"] {
        let store = memory();
        let tasks = store.tasks().unwrap();
        tasks.create_task(&new_task("t"), 1).unwrap();
        let refused = tasks
            .save_plan("t", 1, &plan(vec![assignment(reserved, &[])]), 2)
            .unwrap_err();
        assert!(
            matches!(&refused, TaskError::Invalid(why) if why.contains("reserved")),
            "{reserved}: {refused:?}"
        );
        assert!(tasks.assignments("t").unwrap().is_empty());
    }
}
