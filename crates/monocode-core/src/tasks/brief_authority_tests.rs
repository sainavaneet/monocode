//! What the app can enforce of a task's authority on a worker it cannot
//! fully control: the brief states what the policy forbids, and an answer
//! that talks about forbidden steps arrives with a reminder that it cannot
//! grant them.
use super::*;

#[test]
fn worker_reporting_quotes_the_installed_app_path_and_explains_result_types() {
    let path =
        std::path::Path::new("/Applications/Mono's Native Dev.app/Contents/MacOS/monocode-gpui");
    let text = reporting(path);
    let command = text
        .lines()
        .next()
        .unwrap()
        .strip_prefix("Report through this exact app executable: ")
        .unwrap();
    let argv = shlex::split(command).unwrap();
    assert_eq!(argv[0], path.to_str().unwrap());
    assert_eq!(argv[1], "app");
    assert!(text.contains("arrays of strings"));
    assert!(text.contains("only commit hashes"));
    assert!(text.contains("Never print environment variables"));
}
use crate::tasks::autonomy::Authority;
use crate::tasks::decisions::{handoff_message, Handoff};
use crate::tasks::{
    DecisionRequest, DecisionScope, DecisionState, Requester, TaskLifecycle, TaskStage,
};

fn task() -> Task {
    Task {
        id: "t".into(),
        mono_id: Some("mono-1".into()),
        project: "/repo".into(),
        title: "Ship it".into(),
        request: "Implement the feature".into(),
        stage: TaskStage::Implementing,
        lifecycle: TaskLifecycle::Active,
        criteria: Vec::new(),
        plan_revision: 1,
        blocked_reason: None,
        revision: 1,
        created_at: 1,
        updated_at: 1,
    }
}

fn worktree() -> Workspace {
    Workspace {
        id: "t/a".into(),
        task_id: "t".into(),
        assignment_id: Some("a".into()),
        mode: WorkspaceMode::Worktree,
        repository: "/repo".into(),
        path: "/repo-worktrees/a".into(),
        branch: Some("mono/t/a".into()),
        base_commit: Some("abc".into()),
        owner: "task:t".into(),
        revision: 0,
        created_at: 1,
    }
}

fn brief(authority: &Autonomy) -> String {
    let assignment = crate::tasks::store::tests::assignment("a", &[]);
    worker_brief(&task(), &assignment, &worktree(), None, &[], authority)
}

#[test]
fn the_brief_states_what_the_policy_forbids() {
    let none = Autonomy {
        commit: false,
        merge: false,
        push: false,
        deploy: false,
        ..Autonomy::default()
    };
    let text = brief(&none);
    assert!(
        text.contains("does not allow you to commit, merge, push or deploy"),
        "{text}"
    );
    assert!(text.contains("cannot grant"), "{text}");

    let commits = Autonomy {
        commit: true,
        ..none.clone()
    };
    let text = brief(&commits);
    assert!(
        text.contains("does not allow you to merge, push or deploy"),
        "{text}"
    );

    let everything = Autonomy {
        commit: true,
        merge: true,
        push: true,
        deploy: true,
        ..Autonomy::default()
    };
    assert!(!brief(&everything).contains("does not allow you"));
}

fn handoff(answer: &str, prohibited: Vec<Authority>) -> Handoff {
    Handoff {
        decision: DecisionRequest {
            id: "d1".into(),
            task_id: "t".into(),
            assignment_id: Some("a".into()),
            attempt_id: Some("x".into()),
            question: "Should I push the branch?".into(),
            context: String::new(),
            scope: DecisionScope::Assignment,
            asked_by: Requester::Worker,
            state: DecisionState::Answered,
            answer: Some(answer.into()),
            answered_by: Some(Requester::Mono),
            revision: 2,
            created_at: 1,
            updated_at: 2,
        },
        session_id: "s".into(),
        question: None,
        prohibited,
    }
}

#[test]
fn an_answer_about_forbidden_steps_carries_a_reminder() {
    let forbidden = vec![Authority::Push, Authority::Deploy];
    let message = handoff_message(&handoff("Yes, push it to origin.", forbidden.clone()));
    assert!(
        message.starts_with("Reminder: this task's policy does not allow you to push or deploy"),
        "{message}"
    );
    assert!(message.contains("Yes, push it to origin."), "{message}");

    let plain = handoff_message(&handoff("Use nom.", forbidden));
    assert!(!plain.contains("Reminder"), "{plain}");
    let allowed = handoff_message(&handoff("Yes, push it.", Vec::new()));
    assert!(!allowed.contains("Reminder"), "{allowed}");
}
