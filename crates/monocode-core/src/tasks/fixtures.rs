//! Test support shared by this crate's tests and the app's: a task with one
//! running worker. Not used by the app itself.
use super::{
    AssignmentState, AttemptState, NewAttempt, NewTask, Requester, State, Subject, TaskError,
    TaskStage,
};
use crate::orchestration::{Plan, Task as Assignment};
use crate::store::Store;

/// Task `t` owned by `mono` in `project`, Implementing, with assignment `a`
/// ("Assignment a", files `src`) Running and attempt `x` Running and bound to
/// a new worker session, whose handle is returned.
pub fn running_worker(store: &Store, mono: &str, project: &str) -> Result<i64, TaskError> {
    let session = store
        .create("codex", project)
        .map_err(TaskError::Storage)?
        .id;
    let tasks = store.tasks()?;
    tasks.create_task(
        &NewTask {
            id: "t".into(),
            mono_id: Some(mono.into()),
            project: project.into(),
            title: "Ship it".into(),
            request: "Implement the feature".into(),
            criteria: vec![],
        },
        1,
    )?;
    let plan = Plan {
        title: "Plan".into(),
        summary: "Approach".into(),
        tasks: vec![Assignment {
            id: "a".into(),
            title: "Assignment a".into(),
            prompt: "Do the work".into(),
            files: vec!["src".into()],
            ..Assignment::default()
        }],
    };
    tasks.save_plan("t", 1, &plan, 1)?;
    let t = Subject::Task("t");
    for stage in [TaskStage::Ready, TaskStage::Implementing] {
        let rev = tasks.revision(t)?;
        tasks.transition(t, State::Stage(stage), Requester::Mono, rev, 1)?;
    }
    let a = Subject::Assignment { task: "t", id: "a" };
    for to in [AssignmentState::Ready, AssignmentState::Running] {
        let rev = tasks.revision(a)?;
        tasks.transition(a, State::Assignment(to), Requester::Coordinator, rev, 1)?;
    }
    tasks.create_attempt(
        &NewAttempt {
            id: "x".into(),
            task_id: "t".into(),
            assignment_id: "a".into(),
            operation_id: "launch-x".into(),
            provider: "codex".into(),
            account_id: None,
        },
        1,
    )?;
    let x = Subject::Attempt("x");
    for to in [AttemptState::Starting, AttemptState::Running] {
        let rev = tasks.revision(x)?;
        tasks.transition(x, State::Attempt(to), Requester::Coordinator, rev, 1)?;
    }
    let rev = tasks.revision(x)?;
    let bound = store
        .persisted_session_id(session)
        .map_err(TaskError::Storage)?;
    tasks.bind_attempt_session("x", rev, &bound, 1)?;
    Ok(session)
}
