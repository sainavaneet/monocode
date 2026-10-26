//! The transition table checked against the handoff, written independently of it.
use super::*;
use crate::store::Store;
use crate::tasks::model::{DecisionScope, NewDecision, Verification};
use crate::tasks::store::tests::{
    assignment, force, memory, new_attempt, new_task, plan, revision,
};
use std::collections::BTreeSet;

/// Every legal transition as `machine:from>to`, from the handoff's state lists.
const EXPECTED: &[&str] = &[
    "stage:planning>ready",
    "stage:ready>planning",
    "stage:ready>implementing",
    "stage:implementing>reviewing",
    "stage:reviewing>implementing",
    "stage:reviewing>integrating",
    "stage:integrating>implementing",
    "stage:integrating>verifying",
    "stage:verifying>implementing",
    "stage:verifying>completed",
    "lifecycle:active>waiting",
    "lifecycle:waiting>active",
    "lifecycle:active>paused",
    "lifecycle:waiting>paused",
    "lifecycle:blocked>paused",
    "lifecycle:paused>active",
    "lifecycle:active>blocked",
    "lifecycle:waiting>blocked",
    "lifecycle:blocked>active",
    "lifecycle:active>cancelled",
    "lifecycle:waiting>cancelled",
    "lifecycle:paused>cancelled",
    "lifecycle:blocked>cancelled",
    "lifecycle:active>completed",
    "assignment:queued>ready",
    "assignment:ready>queued",
    "assignment:ready>running",
    "assignment:running>waiting_for_decision",
    "assignment:waiting_for_decision>running",
    "assignment:running>needs_review",
    "assignment:waiting_for_decision>needs_review",
    "assignment:needs_review>accepted",
    "assignment:needs_review>queued",
    "assignment:running>failed",
    "assignment:waiting_for_decision>failed",
    "assignment:failed>queued",
    "assignment:queued>cancelled",
    "assignment:ready>cancelled",
    "assignment:running>cancelled",
    "assignment:waiting_for_decision>cancelled",
    "assignment:needs_review>cancelled",
    "assignment:failed>cancelled",
    "attempt:created>starting",
    "attempt:starting>running",
    "attempt:running>waiting",
    "attempt:waiting>running",
    "attempt:running>succeeded",
    "attempt:waiting>succeeded",
    "attempt:starting>failed",
    "attempt:running>failed",
    "attempt:waiting>failed",
    "attempt:starting>interrupted",
    "attempt:running>interrupted",
    "attempt:waiting>interrupted",
    "attempt:created>cancelled",
    "attempt:starting>cancelled",
    "attempt:running>cancelled",
    "attempt:waiting>cancelled",
];

fn key(from: State, to: State) -> String {
    format!("{}:{}>{}", from.machine(), from.as_str(), to.as_str())
}

#[test]
fn table_matches_the_handoff_state_machines_exactly() {
    let table: BTreeSet<String> = TRANSITIONS
        .iter()
        .flat_map(|rule| rule.from.iter().map(move |from| key(*from, rule.to)))
        .collect();
    let expected: BTreeSet<String> = EXPECTED.iter().map(|s| (*s).to_owned()).collect();
    assert_eq!(table, expected);
    // One place per transition: no pair is listed twice.
    let listed: usize = TRANSITIONS.iter().map(|rule| rule.from.len()).sum();
    assert_eq!(listed, expected.len());
}

#[test]
fn every_pair_in_every_machine_is_legal_only_when_listed() {
    let mut checked = 0;
    for machine in State::machines() {
        for from in machine {
            for to in machine {
                let legal = EXPECTED.contains(&key(*from, *to).as_str());
                for requester in Requester::ALL {
                    match authorize(*from, *to, *requester) {
                        Ok(rule) => {
                            assert!(legal, "{} allowed", key(*from, *to));
                            assert!(rule.requesters.contains(requester));
                        }
                        Err(TaskError::IllegalTransition { .. }) => {
                            assert!(!legal, "{} rejected", key(*from, *to))
                        }
                        Err(TaskError::NotAllowed { .. }) => assert!(legal),
                        Err(other) => panic!("unexpected {other}"),
                    }
                }
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 7 * 7 + 6 * 6 + 8 * 8 + 8 * 8);
    // Different machines never transition into each other.
    assert!(matches!(
        authorize(
            State::Stage(TaskStage::Planning),
            State::Lifecycle(TaskLifecycle::Active),
            Requester::Coordinator
        ),
        Err(TaskError::IllegalTransition { .. })
    ));
}

#[test]
fn requesters_follow_the_behaviour_rules() {
    use AssignmentState as A;
    let allowed = |from, to, who| authorize(from, to, who).is_ok();
    // Only review accepts: a worker's result is a claim.
    let accept = (
        State::Assignment(A::NeedsReview),
        State::Assignment(A::Accepted),
    );
    assert!(!allowed(accept.0, accept.1, Requester::Worker));
    assert!(!allowed(accept.0, accept.1, Requester::User));
    assert!(allowed(accept.0, accept.1, Requester::Mono));
    assert!(allowed(accept.0, accept.1, Requester::Coordinator));
    // Workers claim success on their attempt; they cannot cancel the task.
    assert!(allowed(
        State::Attempt(AttemptState::Running),
        State::Attempt(AttemptState::Succeeded),
        Requester::Worker
    ));
    assert!(!allowed(
        State::Lifecycle(TaskLifecycle::Active),
        State::Lifecycle(TaskLifecycle::Cancelled),
        Requester::Worker
    ));
    // The user can pause and cancel; only the coordinator launches.
    assert!(allowed(
        State::Lifecycle(TaskLifecycle::Active),
        State::Lifecycle(TaskLifecycle::Paused),
        Requester::User
    ));
    assert!(!allowed(
        State::Assignment(A::Ready),
        State::Assignment(A::Running),
        Requester::Mono
    ));
}

#[test]
fn completion_and_launch_carry_their_preconditions() {
    let rule = |from, to| authorize(from, to, Requester::Coordinator).unwrap();
    assert_eq!(
        rule(
            State::Stage(TaskStage::Verifying),
            State::Stage(TaskStage::Completed)
        )
        .precondition,
        Precondition::CriteriaSatisfied
    );
    assert_eq!(
        rule(
            State::Lifecycle(TaskLifecycle::Active),
            State::Lifecycle(TaskLifecycle::Completed)
        )
        .precondition,
        Precondition::StageCompleted
    );
    let launch = rule(
        State::Assignment(AssignmentState::Ready),
        State::Assignment(AssignmentState::Running),
    );
    assert_eq!(launch.precondition, Precondition::TaskActive);
    assert_eq!(launch.recovery, Recovery::Interrupt);
    let cancel = authorize(
        State::Lifecycle(TaskLifecycle::Active),
        State::Lifecycle(TaskLifecycle::Cancelled),
        Requester::User,
    )
    .unwrap();
    assert_eq!(cancel.recovery, Recovery::Never);
    assert_eq!(cancel.eligibility, Eligibility::Ineligible);
    // Every rule names its event and persisted change.
    for rule in TRANSITIONS {
        assert!(!rule.event.is_empty() && !rule.persists.is_empty());
        assert!(!rule.requesters.is_empty());
    }
}

#[test]
fn state_names_round_trip_through_text_and_serde() {
    for machine in State::machines() {
        for state in machine {
            assert_eq!(state.same_machine(state.as_str()), Some(*state));
        }
    }
    assert_eq!(
        serde_json::to_string(&AssignmentState::WaitingForDecision).unwrap(),
        r#""waiting_for_decision""#
    );
    assert_eq!(AssignmentState::default(), AssignmentState::Queued);
    assert_eq!(
        Requester::parse("coordinator"),
        Some(Requester::Coordinator)
    );
}

/// Fixtures where every precondition holds, one task per machine so sweeping
/// one subject never invalidates another's precondition.
fn sweep_fixture(store: &Store) {
    let tasks = store.tasks();
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
    let tasks = store.tasks();
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
    let tasks = store.tasks();
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
    // Completion needs every required criterion verified at a checkpoint.
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
    tasks
        .transition(
            Subject::Task("t"),
            State::Stage(TaskStage::Completed),
            Requester::Mono,
            5,
            19,
        )
        .unwrap();
}

#[test]
fn a_succeeded_attempt_moves_its_assignment_to_review_once() {
    let store = memory();
    let tasks = store.tasks();
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
    let tasks = store.tasks();
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
