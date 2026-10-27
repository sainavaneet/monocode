//! The transition table checked against the handoff, written independently of it.
use super::*;
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
    "assignment:accepted>queued",
    "assignment:running>queued",
    "assignment:waiting_for_decision>queued",
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
    // The user's Start on a reviewed plan starts it explicitly.
    assert!(allowed(
        State::Stage(TaskStage::Ready),
        State::Stage(TaskStage::Implementing),
        Requester::User
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
        .preconditions,
        [
            Precondition::TaskActive,
            Precondition::AssignmentsAccepted,
            Precondition::CriteriaSatisfied,
            Precondition::NoBlockingDecision,
            Precondition::FinalReportRecorded
        ]
    );
    assert_eq!(
        rule(
            State::Lifecycle(TaskLifecycle::Active),
            State::Lifecycle(TaskLifecycle::Completed)
        )
        .preconditions,
        [Precondition::StageCompleted]
    );
    let launch = rule(
        State::Assignment(AssignmentState::Ready),
        State::Assignment(AssignmentState::Running),
    );
    assert_eq!(launch.preconditions, [Precondition::TaskActive]);
    assert_eq!(launch.recovery, Recovery::Interrupt);
    // Reopening accepted work needs an active task; other send-backs do not.
    let reopen = |from| {
        rule(
            State::Assignment(from),
            State::Assignment(AssignmentState::Queued),
        )
    };
    assert_eq!(
        reopen(AssignmentState::Accepted).preconditions,
        [Precondition::TaskActive]
    );
    assert!(reopen(AssignmentState::NeedsReview)
        .preconditions
        .is_empty());
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

/// The Succeeded → NeedsReview cascade (`apply`) runs only when the
/// requester who finished the attempt may also move the assignment: the two
/// rules must allow the same requesters, or a success would never reach
/// review.
#[test]
fn succeeded_and_needs_review_allow_the_same_requesters() {
    let rule = |to: State| {
        TRANSITIONS
            .iter()
            .find(|t| t.to == to)
            .unwrap_or_else(|| panic!("no rule to {}", to.as_str()))
    };
    let mut succeeded = rule(State::Attempt(AttemptState::Succeeded)).requesters.to_vec();
    let mut review = rule(State::Assignment(AssignmentState::NeedsReview))
        .requesters
        .to_vec();
    succeeded.sort_by_key(|r| r.as_str());
    review.sort_by_key(|r| r.as_str());
    assert_eq!(succeeded, review);
}
