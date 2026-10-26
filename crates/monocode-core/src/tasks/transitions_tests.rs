//! Transitions applied to stored records: every pair, preconditions and the
//! flows the behaviour rules depend on.
use super::*;
use crate::store::Store;
use crate::tasks::model::{DecisionScope, NewDecision, Verification};
use crate::tasks::store::tests::{
    assignment, force, memory, new_attempt, new_task, plan, revision,
};

/// Persists a final report for `task`, which completion requires.
fn report(store: &Store, task: &str) {
    let event = crate::tasks::NewEvent {
        id: format!("final_report:{task}"),
        task_id: task.into(),
        assignment_id: None,
        attempt_id: None,
        kind: "task.final_report".into(),
        payload: serde_json::json!({}),
    };
    store.tasks().unwrap().append_event(&event, 1).unwrap();
}

/// Fixtures where every precondition holds, one task per machine so sweeping
/// one subject never invalidates another's precondition.
fn sweep_fixture(store: &Store) {
    let tasks = store.tasks().unwrap();
    for id in ["stage", "lifecycle", "assign", "attempt"] {
        tasks.create_task(&new_task(id), 1).unwrap();
        tasks
            .save_plan(id, 1, &plan(vec![assignment("a", &[])]), 1)
            .unwrap();
        tasks
            .create_attempt(&new_attempt(&format!("{id}-x"), id, "a"), 1)
            .unwrap();
        force(
            store,
            Subject::Attempt(&format!("{id}-x")),
            State::Attempt(AttemptState::Succeeded),
        );
        tasks
            .record_verification(&Verification {
                id: format!("{id}-v"),
                task_id: id.into(),
                assignment_id: None,
                criterion_id: Some("c1".into()),
                checkpoint_commit: "abc".into(),
                check: "cargo test".into(),
                passed: true,
                evidence: String::new(),
                invalidated: false,
                created_at: 1,
            })
            .unwrap();
    }
    force(
        store,
        Subject::Assignment {
            task: "stage",
            id: "a",
        },
        State::Assignment(AssignmentState::Accepted),
    );
    force(
        store,
        Subject::Task("lifecycle"),
        State::Stage(TaskStage::Completed),
    );
    report(store, "stage");
    tasks
        .create_attempt(&new_attempt("swept", "attempt", "a"), 1)
        .unwrap();
    force(
        store,
        Subject::Assignment {
            task: "attempt",
            id: "a",
        },
        State::Assignment(AssignmentState::Accepted),
    );
}

#[test]
fn every_legal_transition_persists_and_every_illegal_one_changes_nothing() {
    let store = memory();
    sweep_fixture(&store);
    let tasks = store.tasks().unwrap();
    let subjects = [
        Subject::Task("stage"),
        Subject::Task("lifecycle"),
        Subject::Assignment {
            task: "assign",
            id: "a",
        },
        Subject::Attempt("swept"),
    ];
    let mut legal = 0;
    let mut illegal = 0;
    for (machine, subject) in State::machines().into_iter().zip(subjects) {
        for from in machine {
            for to in machine {
                force(&store, subject, *from);
                // Send-backs invalidate verifications; each pair starts fresh.
                let fresh = "UPDATE native_task_verifications SET invalidated=0";
                store.connection().execute(fresh, []).unwrap();
                let rule = TRANSITIONS
                    .iter()
                    .find(|rule| rule.to == *to && rule.from.contains(from));
                let requester = rule.map_or(Requester::Coordinator, |rule| rule.requesters[0]);
                let before = revision(&store, subject);
                let events = tasks.events_after(0, 10_000).unwrap().len();
                let result = tasks.transition(subject, *to, requester, before, 50);
                let now = tasks.state(subject, *to).unwrap();
                if let Some(rule) = rule {
                    assert_eq!(result.unwrap(), before + 1, "{from:?}>{to:?}");
                    assert_eq!(now, *to);
                    let log = tasks.events_after(0, 10_000).unwrap();
                    assert!(log.len() > events);
                    assert_eq!(log[events].kind, rule.event);
                    legal += 1;
                } else {
                    assert!(
                        matches!(result, Err(TaskError::IllegalTransition { .. })),
                        "{from:?}>{to:?}: {result:?}"
                    );
                    assert_eq!(now, *from);
                    assert_eq!(revision(&store, subject), before);
                    assert_eq!(tasks.events_after(0, 10_000).unwrap().len(), events);
                    illegal += 1;
                }
            }
        }
    }
    let listed: usize = TRANSITIONS.iter().map(|rule| rule.from.len()).sum();
    assert_eq!(legal, listed);
    assert_eq!(legal + illegal, 49 + 36 + 64 + 64);
}

#[test]
fn transitions_enforce_requester_subject_and_preconditions() {
    let store = memory();
    let tasks = store.tasks().unwrap();
    tasks.create_task(&new_task("t"), 1).unwrap();
    tasks
        .save_plan(
            "t",
            1,
            &plan(vec![assignment("a", &[]), assignment("b", &["a"])]),
            2,
        )
        .unwrap();
    let a = Subject::Assignment { task: "t", id: "a" };
    let b = Subject::Assignment { task: "t", id: "b" };
    let to = |state| State::Assignment(state);
    // Wrong requester and a state from another subject's machine.
    assert!(matches!(
        tasks.transition(a, to(AssignmentState::Ready), Requester::Worker, 1, 3),
        Err(TaskError::NotAllowed { .. })
    ));
    assert!(matches!(
        tasks.transition(a, State::Stage(TaskStage::Ready), Requester::Mono, 1, 3),
        Err(TaskError::IllegalTransition { .. })
    ));
    // b depends on a, which is not accepted yet.
    assert_eq!(
        tasks.transition(b, to(AssignmentState::Ready), Requester::Coordinator, 1, 3),
        Err(TaskError::Precondition(Precondition::DependenciesAccepted))
    );
    tasks
        .transition(a, to(AssignmentState::Ready), Requester::Coordinator, 1, 3)
        .unwrap();
    // A paused task launches nothing.
    tasks
        .transition(
            Subject::Task("t"),
            State::Lifecycle(TaskLifecycle::Paused),
            Requester::User,
            2,
            4,
        )
        .unwrap();
    assert_eq!(
        tasks.transition(
            a,
            to(AssignmentState::Running),
            Requester::Coordinator,
            2,
            5
        ),
        Err(TaskError::Precondition(Precondition::TaskActive))
    );
    tasks
        .transition(
            Subject::Task("t"),
            State::Lifecycle(TaskLifecycle::Active),
            Requester::User,
            3,
            6,
        )
        .unwrap();
    tasks
        .transition(
            a,
            to(AssignmentState::Running),
            Requester::Coordinator,
            2,
            7,
        )
        .unwrap();
    // A claim of success needs a succeeded attempt.
    assert_eq!(
        tasks.transition(
            a,
            to(AssignmentState::NeedsReview),
            Requester::Coordinator,
            3,
            8
        ),
        Err(TaskError::Precondition(Precondition::AttemptSucceeded))
    );
    // An open decision keeps the assignment waiting.
    tasks
        .transition(
            a,
            to(AssignmentState::WaitingForDecision),
            Requester::Worker,
            3,
            8,
        )
        .unwrap();
    tasks
        .create_decision(
            &NewDecision {
                id: "d".into(),
                task_id: "t".into(),
                assignment_id: Some("a".into()),
                attempt_id: None,
                question: "Q".into(),
                context: String::new(),
                scope: DecisionScope::Assignment,
                asked_by: Requester::Worker,
            },
            9,
        )
        .unwrap();
    assert_eq!(
        tasks.transition(a, to(AssignmentState::Running), Requester::Mono, 4, 10),
        Err(TaskError::Precondition(Precondition::NoOpenDecision))
    );
    tasks
        .answer_decision("d", 1, "Yes", Requester::Mono, 11)
        .unwrap();
    tasks
        .transition(a, to(AssignmentState::Running), Requester::Mono, 4, 12)
        .unwrap();
    // Completion needs accepted work, no unresolved decision, every required
    // criterion verified at a checkpoint and a persisted report.
    tasks.resolve_decision("d", 2, 12).unwrap();
    for id in ["a", "b"] {
        let accepted = State::Assignment(AssignmentState::Accepted);
        force(&store, Subject::Assignment { task: "t", id }, accepted);
    }
    tasks
        .transition(
            Subject::Task("t"),
            State::Stage(TaskStage::Ready),
            Requester::Mono,
            4,
            13,
        )
        .unwrap();
    force(
        &store,
        Subject::Task("t"),
        State::Stage(TaskStage::Verifying),
    );
    assert_eq!(
        tasks.transition(
            Subject::Task("t"),
            State::Stage(TaskStage::Completed),
            Requester::Mono,
            5,
            14
        ),
        Err(TaskError::Precondition(Precondition::CriteriaSatisfied))
    );
    let check = |id: &str, passed, at| Verification {
        id: id.into(),
        task_id: "t".into(),
        assignment_id: None,
        criterion_id: Some("c1".into()),
        checkpoint_commit: "abc".into(),
        check: "cargo test".into(),
        passed,
        evidence: String::new(),
        invalidated: false,
        created_at: at,
    };
    tasks.record_verification(&check("pass", true, 15)).unwrap();
    tasks
        .record_verification(&check("fail", false, 16))
        .unwrap();
    assert_eq!(
        tasks.transition(
            Subject::Task("t"),
            State::Stage(TaskStage::Completed),
            Requester::Mono,
            5,
            17
        ),
        Err(TaskError::Precondition(Precondition::CriteriaSatisfied)),
        "the latest verification failed"
    );
    tasks
        .record_verification(&check("again", true, 18))
        .unwrap();
    let complete = || {
        let completed = State::Stage(TaskStage::Completed);
        tasks.transition(Subject::Task("t"), completed, Requester::Mono, 5, 19)
    };
    let unreported = Err(TaskError::Precondition(Precondition::FinalReportRecorded));
    assert_eq!(complete(), unreported);
    report(&store, "t");
    complete().unwrap();
}

#[test]
fn a_succeeded_attempt_moves_its_assignment_to_review_once() {
    let store = memory();
    let tasks = store.tasks().unwrap();
    tasks.create_task(&new_task("t"), 1).unwrap();
    tasks
        .save_plan("t", 1, &plan(vec![assignment("a", &[])]), 2)
        .unwrap();
    let a = Subject::Assignment { task: "t", id: "a" };
    tasks
        .transition(
            a,
            State::Assignment(AssignmentState::Ready),
            Requester::Coordinator,
            1,
            3,
        )
        .unwrap();
    tasks
        .transition(
            a,
            State::Assignment(AssignmentState::Running),
            Requester::Coordinator,
            2,
            4,
        )
        .unwrap();
    tasks
        .create_attempt(&new_attempt("x", "t", "a"), 5)
        .unwrap();
    let x = Subject::Attempt("x");
    for (revision, state) in [(1, AttemptState::Starting), (2, AttemptState::Running)] {
        tasks
            .transition(
                x,
                State::Attempt(state),
                Requester::Coordinator,
                revision,
                6,
            )
            .unwrap();
    }
    tasks
        .transition(
            x,
            State::Attempt(AttemptState::Succeeded),
            Requester::Worker,
            3,
            7,
        )
        .unwrap();
    let assignment = tasks.assignment("t", "a").unwrap().unwrap();
    assert_eq!(assignment.status, AssignmentState::NeedsReview);
    assert_eq!(assignment.revision, 4);
    // A duplicated result is stale and does not start a second review.
    assert!(matches!(
        tasks.transition(
            x,
            State::Attempt(AttemptState::Succeeded),
            Requester::Worker,
            3,
            8
        ),
        Err(TaskError::Stale { .. })
    ));
    let reviews = tasks
        .events_after(0, 100)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "assignment.needs_review")
        .count();
    assert_eq!(reviews, 1);
}

#[test]
fn integration_needs_accepted_work_and_cancelled_tasks_stay_cancelled() {
    let store = memory();
    let tasks = store.tasks().unwrap();
    tasks.create_task(&new_task("t"), 1).unwrap();
    tasks
        .save_plan("t", 1, &plan(vec![assignment("a", &[])]), 2)
        .unwrap();
    force(
        &store,
        Subject::Task("t"),
        State::Stage(TaskStage::Reviewing),
    );
    let integrate = |revision| {
        tasks.transition(
            Subject::Task("t"),
            State::Stage(TaskStage::Integrating),
            Requester::Mono,
            revision,
            3,
        )
    };
    assert_eq!(
        integrate(2),
        Err(TaskError::Precondition(Precondition::AssignmentsAccepted))
    );
    // Cancelling every assignment leaves nothing accepted to integrate.
    force(
        &store,
        Subject::Assignment { task: "t", id: "a" },
        State::Assignment(AssignmentState::Cancelled),
    );
    assert_eq!(
        integrate(2),
        Err(TaskError::Precondition(Precondition::AssignmentsAccepted))
    );
    tasks
        .transition(
            Subject::Task("t"),
            State::Lifecycle(TaskLifecycle::Cancelled),
            Requester::User,
            2,
            4,
        )
        .unwrap();
    for to in TaskLifecycle::ALL {
        assert!(tasks
            .transition(
                Subject::Task("t"),
                State::Lifecycle(*to),
                Requester::User,
                3,
                5
            )
            .is_err());
    }
}

/// Moves an assignment from Ready through a fresh attempt to Running.
fn launch(store: &Store, task: &str, attempt: &str) {
    let tasks = store.tasks().unwrap();
    let a = Subject::Assignment { task, id: "a" };
    let running = State::Assignment(AssignmentState::Running);
    tasks
        .transition(a, running, Requester::Coordinator, revision(store, a), 1)
        .unwrap();
    tasks
        .create_attempt(&new_attempt(attempt, task, "a"), 1)
        .unwrap();
    let x = Subject::Attempt(attempt);
    for state in [AttemptState::Starting, AttemptState::Running] {
        tasks
            .transition(
                x,
                State::Attempt(state),
                Requester::Coordinator,
                revision(store, x),
                1,
            )
            .unwrap();
    }
}

fn status(store: &Store, task: &str) -> AssignmentState {
    store
        .tasks()
        .unwrap()
        .assignment(task, "a")
        .unwrap()
        .unwrap()
        .status
}

#[test]
fn only_the_latest_attempt_can_claim_review() {
    let store = memory();
    let tasks = store.tasks().unwrap();
    tasks.create_task(&new_task("t"), 1).unwrap();
    tasks
        .save_plan("t", 1, &plan(vec![assignment("a", &[])]), 1)
        .unwrap();
    let a = Subject::Assignment { task: "t", id: "a" };
    let to = |state| State::Assignment(state);
    tasks
        .transition(a, to(AssignmentState::Ready), Requester::Coordinator, 1, 1)
        .unwrap();
    launch(&store, "t", "first");
    let succeed = |id| {
        let x = Subject::Attempt(id);
        let state = State::Attempt(AttemptState::Succeeded);
        tasks.transition(x, state, Requester::Worker, revision(&store, x), 2)
    };
    succeed("first").unwrap();
    assert_eq!(status(&store, "t"), AssignmentState::NeedsReview);
    // Review sends it back; a second attempt runs.
    for state in [AssignmentState::Queued, AssignmentState::Ready] {
        tasks
            .transition(a, to(state), Requester::Coordinator, revision(&store, a), 3)
            .unwrap();
    }
    launch(&store, "t", "second");
    // The first attempt's old success is not a claim for the second run.
    assert_eq!(
        tasks.transition(
            a,
            to(AssignmentState::NeedsReview),
            Requester::Worker,
            revision(&store, a),
            4
        ),
        Err(TaskError::Precondition(Precondition::AttemptSucceeded))
    );
    // A replaced attempt that finishes late does not start a review.
    tasks
        .create_attempt(&new_attempt("third", "t", "a"), 5)
        .unwrap();
    succeed("second").unwrap();
    assert_eq!(status(&store, "t"), AssignmentState::Running);
    for state in [
        AttemptState::Starting,
        AttemptState::Running,
        AttemptState::Succeeded,
    ] {
        let x = Subject::Attempt("third");
        tasks
            .transition(
                x,
                State::Attempt(state),
                Requester::Coordinator,
                revision(&store, x),
                6,
            )
            .unwrap();
    }
    assert_eq!(status(&store, "t"), AssignmentState::NeedsReview);
}

#[test]
fn verification_send_back_reopens_accepted_work_and_completes_again() {
    let store = memory();
    let tasks = store.tasks().unwrap();
    tasks.create_task(&new_task("t"), 1).unwrap();
    tasks
        .save_plan("t", 1, &plan(vec![assignment("a", &[])]), 1)
        .unwrap();
    let t = Subject::Task("t");
    let a = Subject::Assignment { task: "t", id: "a" };
    let stage = |state| {
        tasks
            .transition(
                t,
                State::Stage(state),
                Requester::Mono,
                revision(&store, t),
                2,
            )
            .unwrap()
    };
    let assign = |state| {
        tasks
            .transition(
                a,
                State::Assignment(state),
                Requester::Coordinator,
                revision(&store, a),
                2,
            )
            .unwrap()
    };
    let review = |attempt: &str| {
        launch(&store, "t", attempt);
        let x = Subject::Attempt(attempt);
        let succeeded = State::Attempt(AttemptState::Succeeded);
        tasks
            .transition(x, succeeded, Requester::Worker, revision(&store, x), 3)
            .unwrap();
        assign(AssignmentState::Accepted);
    };
    stage(TaskStage::Ready);
    stage(TaskStage::Implementing);
    assign(AssignmentState::Ready);
    review("first");
    for next in [
        TaskStage::Reviewing,
        TaskStage::Integrating,
        TaskStage::Verifying,
    ] {
        stage(next);
    }
    // Verification fails and sends the work back.
    stage(TaskStage::Implementing);
    assign(AssignmentState::Queued);
    assign(AssignmentState::Ready);
    review("second");
    for next in [
        TaskStage::Reviewing,
        TaskStage::Integrating,
        TaskStage::Verifying,
    ] {
        stage(next);
    }
    tasks
        .record_verification(&Verification {
            id: "v".into(),
            task_id: "t".into(),
            assignment_id: Some("a".into()),
            criterion_id: Some("c1".into()),
            checkpoint_commit: "abc".into(),
            check: "cargo test".into(),
            passed: true,
            evidence: String::new(),
            invalidated: false,
            created_at: 4,
        })
        .unwrap();
    report(&store, "t");
    stage(TaskStage::Completed);
    assert_eq!(
        tasks.task("t").unwrap().unwrap().stage,
        TaskStage::Completed
    );
}

#[test]
fn paused_or_cancelled_tasks_neither_advance_nor_launch() {
    let store = memory();
    let tasks = store.tasks().unwrap();
    tasks.create_task(&new_task("t"), 1).unwrap();
    tasks
        .save_plan("t", 1, &plan(vec![assignment("a", &[])]), 1)
        .unwrap();
    let t = Subject::Task("t");
    tasks
        .transition(t, State::Stage(TaskStage::Ready), Requester::Mono, 2, 2)
        .unwrap();
    tasks
        .create_attempt(&new_attempt("x", "t", "a"), 2)
        .unwrap();
    for lifecycle in [TaskLifecycle::Paused, TaskLifecycle::Cancelled] {
        force(&store, t, State::Lifecycle(lifecycle));
        let stuck = Err(TaskError::Precondition(Precondition::TaskActive));
        for (from, to) in [
            (TaskStage::Ready, TaskStage::Implementing),
            (TaskStage::Implementing, TaskStage::Reviewing),
            (TaskStage::Integrating, TaskStage::Verifying),
            (TaskStage::Verifying, TaskStage::Completed),
        ] {
            force(&store, t, State::Stage(from));
            let result =
                tasks.transition(t, State::Stage(to), Requester::Mono, revision(&store, t), 3);
            assert_eq!(result, stuck, "{lifecycle:?} {from:?}>{to:?}");
        }
        let start = State::Attempt(AttemptState::Starting);
        let x = Subject::Attempt("x");
        assert_eq!(
            tasks.transition(x, start, Requester::Coordinator, revision(&store, x), 3),
            stuck
        );
    }
}

#[test]
fn the_latest_attempt_is_the_last_recorded_even_when_clocks_disagree() {
    let store = memory();
    let tasks = store.tasks().unwrap();
    tasks.create_task(&new_task("t"), 1).unwrap();
    tasks
        .save_plan("t", 1, &plan(vec![assignment("a", &[])]), 1)
        .unwrap();
    let a = Subject::Assignment { task: "t", id: "a" };
    tasks
        .transition(
            a,
            State::Assignment(AssignmentState::Ready),
            Requester::Coordinator,
            1,
            1,
        )
        .unwrap();
    // Another process with a fast clock recorded the first attempt; this
    // process's slower clock records the second one "earlier".
    tasks
        .create_attempt(&new_attempt("first", "t", "a"), 900)
        .unwrap();
    tasks
        .create_attempt(&new_attempt("second", "t", "a"), 100)
        .unwrap();
    let ids: Vec<_> = tasks
        .attempts("t", "a")
        .unwrap()
        .into_iter()
        .map(|attempt| attempt.id)
        .collect();
    assert_eq!(ids, ["first", "second"]);
    tasks
        .transition(
            a,
            State::Assignment(AssignmentState::Running),
            Requester::Coordinator,
            revision(&store, a),
            2,
        )
        .unwrap();
    let run = |id: &str, last: AttemptState, requester| {
        let x = Subject::Attempt(id);
        for (state, by) in [
            (AttemptState::Starting, Requester::Coordinator),
            (AttemptState::Running, Requester::Coordinator),
            (last, requester),
        ] {
            let rev = revision(&store, x);
            tasks
                .transition(x, State::Attempt(state), by, rev, 3)
                .unwrap();
        }
    };
    for id in ["first", "second"] {
        let x = Subject::Attempt(id);
        tasks
            .bind_attempt_session(id, revision(&store, x), &format!("s-{id}"), 3)
            .unwrap();
    }
    assert_eq!(
        tasks.attempt_for_session("s-second").unwrap().unwrap().id,
        "second"
    );
    // The older attempt's success is not the current run's claim.
    run("first", AttemptState::Succeeded, Requester::Worker);
    assert_eq!(status(&store, "t"), AssignmentState::Running);
    run("second", AttemptState::Succeeded, Requester::Worker);
    assert_eq!(status(&store, "t"), AssignmentState::NeedsReview);
}

#[test]
fn a_paused_or_cancelled_task_cannot_send_accepted_work_back() {
    let store = memory();
    let tasks = store.tasks().unwrap();
    tasks.create_task(&new_task("t"), 1).unwrap();
    tasks
        .save_plan("t", 1, &plan(vec![assignment("a", &[])]), 1)
        .unwrap();
    let a = Subject::Assignment { task: "t", id: "a" };
    let queued = State::Assignment(AssignmentState::Queued);
    for lifecycle in [TaskLifecycle::Paused, TaskLifecycle::Cancelled] {
        force(&store, Subject::Task("t"), State::Lifecycle(lifecycle));
        force(&store, a, State::Assignment(AssignmentState::Accepted));
        assert_eq!(
            tasks.transition(a, queued, Requester::Mono, revision(&store, a), 2),
            Err(TaskError::Precondition(Precondition::TaskActive)),
            "{lifecycle:?}"
        );
    }
    // Other send-backs stay allowed while paused, so review can still settle.
    force(
        &store,
        Subject::Task("t"),
        State::Lifecycle(TaskLifecycle::Paused),
    );
    force(&store, a, State::Assignment(AssignmentState::NeedsReview));
    tasks
        .transition(a, queued, Requester::Mono, revision(&store, a), 2)
        .unwrap();
    force(
        &store,
        Subject::Task("t"),
        State::Lifecycle(TaskLifecycle::Active),
    );
    force(&store, a, State::Assignment(AssignmentState::Accepted));
    tasks
        .transition(a, queued, Requester::Coordinator, revision(&store, a), 2)
        .unwrap();
}
