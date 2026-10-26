//! The per-Mono autonomy policy: defaults, serialization, the snapshot taken
//! into a task, the immediate effect of reduced authority, the explicit apply
//! step and the policy's decisions in the coordinator and recovery.
use super::*;
use crate::accounts::eligibility::{self, AllowedAccount, Allowlist, Scope};
use crate::orchestration::WorkspaceMode;
use crate::store::Store;
use crate::tasks::coordinator::{Coordinator, ExplicitStart, Policy};
use crate::tasks::policy;
use crate::tasks::recovery::tests::{fail, stop_all, Recorded};
use crate::tasks::recovery::{Outcome, RETRY_BUDGET};
use crate::tasks::review::tests::{git, open, repo, Repo, NOW};
use crate::tasks::store::tests::{assignment, force, memory, new_task, plan};
use crate::tasks::{
    confirm_started, AssignmentState, NewTask, Requester, State, Subject, Task, TaskError,
    TaskLifecycle, TaskStage,
};
use std::cell::RefCell;

/// The live policy as the app would publish it, changeable mid-test.
struct Live(RefCell<Autonomy>);
impl Live {
    fn new(autonomy: Autonomy) -> Self {
        Self(RefCell::new(autonomy))
    }
    fn set(&self, autonomy: Autonomy) {
        *self.0.borrow_mut() = autonomy;
    }
}
impl Policy for Live {
    fn autonomy(&self, _: &Task) -> Autonomy {
        self.0.borrow().clone()
    }
}

fn generous() -> Autonomy {
    Autonomy {
        automatic_execution: true,
        worker_limit: 3,
        push: true,
        merge: true,
        ..Autonomy::default()
    }
}

fn new(repo: &Repo, id: &str) -> NewTask {
    owned_by(repo, id, "mono-1")
}

fn owned_by(repo: &Repo, id: &str, mono: &str) -> NewTask {
    NewTask {
        id: id.into(),
        mono_id: Some(mono.into()),
        project: repo.root.to_string_lossy().into_owned(),
        title: "Ship it".into(),
        request: "Implement the feature".into(),
        criteria: vec![],
    }
}

fn writes(id: &str, files: &[&str]) -> crate::tasks::Assignment {
    crate::tasks::Assignment {
        files: files.iter().map(|f| (*f).to_owned()).collect(),
        workspace: None,
        ..assignment(id, &[])
    }
}

fn reads(id: &str) -> crate::tasks::Assignment {
    crate::tasks::Assignment {
        files: vec![".".into()],
        workspace: Some(WorkspaceMode::Checkout),
        ..assignment(id, &[])
    }
}

/// Task `id` with its snapshot taken from `live`, planned and at `stage`.
fn planned(
    store: &Store,
    repo: &Repo,
    id: &str,
    live: &Autonomy,
    assignments: Vec<crate::tasks::Assignment>,
    stage: TaskStage,
) {
    planned_for(store, &new(repo, id), live, assignments, stage);
}

/// As [`planned`], for a task described by `new`.
fn planned_for(
    store: &Store,
    new: &NewTask,
    live: &Autonomy,
    assignments: Vec<crate::tasks::Assignment>,
    stage: TaskStage,
) {
    let id = new.id.as_str();
    policy::create_task_with(store, new, "codex", live, NOW).unwrap();
    let tasks = store.tasks().unwrap();
    tasks.save_plan(id, 1, &plan(assignments), NOW).unwrap();
    let t = Subject::Task(id);
    let mut stages = vec![TaskStage::Ready];
    if stage == TaskStage::Implementing {
        stages.push(TaskStage::Implementing);
    }
    for stage in stages {
        let revision = tasks.revision(t).unwrap();
        tasks
            .transition(t, State::Stage(stage), Requester::Mono, revision, NOW)
            .unwrap();
    }
}

fn task(store: &Store, id: &str) -> Task {
    store.tasks().unwrap().task(id).unwrap().unwrap()
}

fn status(store: &Store, task: &str, id: &str) -> AssignmentState {
    let tasks = store.tasks().unwrap();
    tasks.assignment(task, id).unwrap().unwrap().status
}

fn brief(store: &Store, launcher: &Recorded) -> String {
    let session = launcher.last().0.id;
    store.follow_ups(session).unwrap().draft.unwrap().text
}

#[test]
fn the_default_policy_starts_nothing_on_its_own_and_grants_only_commits() {
    let _published = tests_lock();
    let defaults = Autonomy::default();
    assert!(!defaults.automatic_execution);
    assert_eq!(defaults.workspace, WorkspacePreference::Automatic);
    assert_eq!(defaults.worker_limit, 1);
    assert!(!defaults.provider_fallback);
    assert_eq!(defaults.retry_limit, RETRY_BUDGET);
    assert!(defaults.allows(Authority::Commit));
    for authority in [Authority::Merge, Authority::Push, Authority::Deploy] {
        assert!(!defaults.allows(authority), "{authority:?}");
    }
    assert!(defaults.restart_continuation);
    assert_eq!(defaults.worktree_retention, Retention::Keep);
    assert_eq!(defaults.api_key_billing, None);
    // A Mono the app published nothing for, and the app's policy before any
    // publication, keep task-4's explicit-start behaviour.
    assert_eq!(live(Some("mono-never-published"), "/repo"), defaults);
    let store = memory();
    let created = store
        .tasks()
        .unwrap()
        .create_task(&new_task("t"), NOW)
        .unwrap();
    let app = ExplicitStart.autonomy(&created);
    assert!(!app.automatic_execution);
    assert_eq!(app.worker_limit, 1);
}

#[test]
fn a_mono_policy_round_trips_and_a_project_overrides_it() {
    let mut mono = MonoAutonomy {
        policy: Autonomy {
            workspace: WorkspacePreference::Worktree,
            worktree_retention: Retention::RemoveClean,
            api_key_billing: Some(true),
            provider_fallback: true,
            retry_limit: 5,
            deploy: true,
            restart_continuation: false,
            ..generous()
        },
        ..MonoAutonomy::default()
    };
    let checkout = Autonomy {
        workspace: WorkspacePreference::Checkout,
        ..Autonomy::default()
    };
    mono.set_project("/repo/", Some(checkout.clone()));
    let text = serde_json::to_string(&mono).unwrap();
    for key in [
        "automaticExecution",
        "workerLimit",
        "worktreeRetention",
        "apiKeyBilling",
    ] {
        assert!(text.contains(key), "{key} missing from {text}");
    }
    let back: MonoAutonomy = serde_json::from_str(&text).unwrap();
    assert_eq!(back, mono);
    // Older or partial saves fill in the defaults.
    assert_eq!(
        serde_json::from_str::<MonoAutonomy>("{}").unwrap(),
        MonoAutonomy::default()
    );
    let partial: MonoAutonomy = serde_json::from_str(r#"{"policy":{"push":true}}"#).unwrap();
    assert!(partial.policy.push);
    assert_eq!(partial.policy.worker_limit, 1);
    // The project override wins for its project (paths normalised), the
    // Mono's policy everywhere else, and clearing it falls back.
    assert_eq!(mono.for_project("/repo"), &checkout);
    assert_eq!(mono.for_project("/elsewhere"), &mono.policy);
    mono.set_project("/repo", None);
    assert_eq!(mono.for_project("/repo"), &mono.policy);
    // Out-of-range numbers are clamped, never trusted.
    let wild = Autonomy {
        worker_limit: 0,
        retry_limit: 999,
        ..Autonomy::default()
    }
    .normalized();
    assert_eq!(wild.worker_limit, 1);
    assert_eq!(wild.retry_limit, MAX_RETRIES);
    let wide = Autonomy {
        worker_limit: 999,
        ..Autonomy::default()
    };
    assert_eq!(wide.normalized().worker_limit, MAX_WORKERS);
    // The published registry answers per Mono and project.
    let _published = tests_lock();
    let mut published = std::collections::HashMap::new();
    published.insert("mono-autonomy-test".to_owned(), back.clone());
    publish(published);
    assert_eq!(live(Some("mono-autonomy-test"), "/repo/"), checkout);
    assert_eq!(live(Some("mono-autonomy-test"), "/other"), back.policy);
    assert_eq!(live(None, "/repo"), Autonomy::default());
}

#[test]
fn the_snapshot_taken_at_creation_ignores_later_policy_and_billing_changes() {
    let repo = repo();
    let store = open(&repo);
    policy::create_task_with(&store, &new(&repo, "t"), "codex", &generous(), NOW).unwrap();
    let tasks = store.tasks().unwrap();
    let snapshot = policy::of(&tasks, "t").unwrap().unwrap();
    assert_eq!(snapshot.autonomy, generous());
    assert_eq!(snapshot.revision, 0);
    assert!(!snapshot.use_api_key);
    // The app turns API billing on and the Mono's policy grows: neither
    // changes what the task captured.
    store
        .save_ui_state(r#"{"preferences":{"api_key_enabled":{"codex":true}}}"#)
        .unwrap();
    let wider = Autonomy {
        worker_limit: 6,
        deploy: true,
        api_key_billing: Some(true),
        ..generous()
    };
    let again = policy::capture_with(&store, &task(&store, "t"), "codex", &wider, NOW + 5).unwrap();
    assert_eq!(again, snapshot);
    assert_eq!(policy::of(&tasks, "t").unwrap().unwrap(), snapshot);
    // A coordinator whose live policy grants more never exceeds the snapshot.
    let coordinator = Coordinator::new("gpui-1", Live::new(wider));
    let effective = coordinator.autonomy(&store, &task(&store, "t")).unwrap();
    assert_eq!(effective.worker_limit, 3);
    assert!(!effective.deploy);
    assert!(effective.push);
    // Billing chosen by the policy, not the app preference, when it says so.
    let billed = Autonomy {
        api_key_billing: Some(false),
        ..Autonomy::default()
    };
    policy::create_task_with(&store, &new(&repo, "u"), "codex", &billed, NOW).unwrap();
    assert!(!policy::of(&tasks, "u").unwrap().unwrap().use_api_key);
    policy::create_task_with(&store, &new(&repo, "v"), "codex", &Autonomy::default(), NOW).unwrap();
    assert!(policy::of(&tasks, "v").unwrap().unwrap().use_api_key);
}

#[test]
fn reducing_authority_stops_new_disallowed_actions_without_stopping_running_workers() {
    let repo = repo();
    let store = open(&repo);
    let snapshot = Autonomy {
        worker_limit: 2,
        ..generous()
    };
    planned(
        &store,
        &repo,
        "t",
        &snapshot,
        vec![
            writes("a", &["src"]),
            writes("b", &["docs"]),
            writes("c", &["tests"]),
        ],
        TaskStage::Ready,
    );
    let live = Live::new(snapshot.clone());
    let coordinator = Coordinator::new("gpui-1", live);
    let launcher = Recorded::default();
    // Automatic execution starts the task, and two workers run.
    let tick = coordinator.tick(&store, &launcher, NOW).unwrap();
    assert_eq!(tick.launched.len(), 2, "{tick:?}");
    assert_eq!(task(&store, "t").stage, TaskStage::Implementing);
    let brief_a = brief(&store, &launcher);
    assert!(brief_a.contains("Commit your work there"), "{brief_a}");
    assert!(coordinator.authorize(&store, "t", Authority::Push).is_ok());

    // The user lowers concurrency and revokes push, commits and automatic
    // execution. Nothing is re-snapshotted.
    coordinator.policy().set(Autonomy {
        worker_limit: 1,
        commit: false,
        ..Autonomy::default()
    });
    let tick = coordinator.tick(&store, &launcher, NOW + 1).unwrap();
    assert!(tick.launched.is_empty(), "{tick:?}");
    assert_eq!(status(&store, "t", "a"), AssignmentState::Running);
    assert_eq!(status(&store, "t", "b"), AssignmentState::Running);
    assert!(
        crate::tasks::recovery::pending_stops(&store)
            .unwrap()
            .is_empty(),
        "running workers are not stopped"
    );
    // One worker finishing leaves one running: still at the new limit.
    force(
        &store,
        Subject::Assignment { task: "t", id: "a" },
        State::Assignment(AssignmentState::Accepted),
    );
    let tick = coordinator.tick(&store, &launcher, NOW + 2).unwrap();
    assert!(tick.launched.is_empty(), "{tick:?}");
    assert_eq!(status(&store, "t", "c"), AssignmentState::Ready);
    // Push is refused from now on, with the reason.
    let refused = coordinator.authorize(&store, "t", Authority::Push);
    assert!(
        matches!(&refused, Err(TaskError::Invalid(why)) if why.contains("push")),
        "{refused:?}"
    );
    assert!(coordinator
        .authorize(&store, "t", Authority::Merge)
        .is_err());
    // A new launch is told not to commit.
    force(
        &store,
        Subject::Assignment { task: "t", id: "b" },
        State::Assignment(AssignmentState::Accepted),
    );
    let tick = coordinator.tick(&store, &launcher, NOW + 3).unwrap();
    assert_eq!(tick.launched.len(), 1, "{tick:?}");
    let brief_c = brief(&store, &launcher);
    assert!(brief_c.contains("Do not commit"), "{brief_c}");
    assert!(!brief_c.contains("Commit your work there"), "{brief_c}");
    // The snapshot itself is unchanged.
    let tasks = store.tasks().unwrap();
    assert_eq!(policy::of(&tasks, "t").unwrap().unwrap().autonomy, snapshot);
    // Another Ready task whose snapshot allows automatic execution waits.
    planned(
        &store,
        &repo,
        "u",
        &snapshot,
        vec![writes("x", &["lib"])],
        TaskStage::Ready,
    );
    coordinator.tick(&store, &launcher, NOW + 4).unwrap();
    assert_eq!(task(&store, "u").stage, TaskStage::Ready);
}

#[test]
fn applying_the_policy_re_snapshots_one_active_task_with_a_revision_bump() {
    let repo = repo();
    let store = open(&repo);
    planned(
        &store,
        &repo,
        "t",
        &Autonomy::default(),
        vec![writes("a", &["src"]), writes("b", &["docs"])],
        TaskStage::Implementing,
    );
    let live = Live::new(generous());
    let coordinator = Coordinator::new("gpui-1", live);
    let launcher = Recorded::default();
    // Raising authority does not reach the task by itself.
    let tick = coordinator.tick(&store, &launcher, NOW).unwrap();
    assert_eq!(tick.launched.len(), 1, "{tick:?}");
    assert!(coordinator.authorize(&store, "t", Authority::Push).is_err());

    let stale = apply(&store, "t", 1, &generous(), NOW + 1);
    assert!(matches!(stale, Err(TaskError::Stale { .. })), "{stale:?}");
    let applied = apply(&store, "t", 0, &generous(), NOW + 2).unwrap();
    assert_eq!(applied.revision, 1);
    assert_eq!(applied.autonomy, generous());
    let tasks = store.tasks().unwrap();
    assert_eq!(policy::of(&tasks, "t").unwrap().unwrap(), applied);
    let history: i64 = store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM native_task_events WHERE task_id='t' AND kind='task.policy'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(history, 2, "revisions are recorded, not rewritten");
    // Now the task may run more workers and push.
    let tick = coordinator.tick(&store, &launcher, NOW + 3).unwrap();
    assert_eq!(tick.launched.len(), 1, "{tick:?}");
    assert!(coordinator.authorize(&store, "t", Authority::Push).is_ok());
    // A second apply from the old revision is refused.
    assert!(matches!(
        apply(&store, "t", 0, &Autonomy::default(), NOW + 4),
        Err(TaskError::Stale { .. })
    ));
    // Finished and never-started tasks are left alone.
    force(
        &store,
        Subject::Task("t"),
        State::Lifecycle(TaskLifecycle::Cancelled),
    );
    assert!(matches!(
        apply(&store, "t", 1, &generous(), NOW + 5),
        Err(TaskError::Invalid(_))
    ));
    tasks.create_task(&new(&repo, "fresh"), NOW).unwrap();
    assert!(matches!(
        apply(&store, "fresh", 0, &generous(), NOW + 6),
        Err(TaskError::Invalid(_))
    ));
}

#[test]
fn the_workspace_preference_picks_the_workspace_and_never_writes_into_the_checkout() {
    let _published = tests_lock();
    // A dedicated worktree even for an investigation.
    let repo_a = repo();
    let store = open(&repo_a);
    let isolated = Autonomy {
        workspace: WorkspacePreference::Worktree,
        ..Autonomy::default()
    };
    planned(
        &store,
        &repo_a,
        "t",
        &isolated,
        vec![reads("look")],
        TaskStage::Implementing,
    );
    let launcher = Recorded::default();
    let coordinator = Coordinator::new("gpui-1", ExplicitStart);
    coordinator.tick(&store, &launcher, NOW).unwrap();
    let workspaces = store.tasks().unwrap().workspaces("t").unwrap();
    assert_eq!(workspaces[0].mode, WorkspaceMode::Worktree);

    // The current checkout: investigations read it, writing work is refused.
    let repo_b = repo();
    let store = open(&repo_b);
    let in_place = Autonomy {
        workspace: WorkspacePreference::Checkout,
        ..Autonomy::default()
    };
    let before = git(&repo_b.root, &["status", "--porcelain"]);
    planned(
        &store,
        &repo_b,
        "t",
        &in_place,
        vec![writes("a", &["src"])],
        TaskStage::Implementing,
    );
    let tick = coordinator.tick(&store, &launcher, NOW).unwrap();
    assert!(tick.launched.is_empty(), "{tick:?}");
    let blocked = task(&store, "t");
    assert_eq!(blocked.lifecycle, TaskLifecycle::Blocked);
    let reason = blocked.blocked_reason.unwrap_or_default();
    assert!(reason.contains("current checkout"), "{reason}");
    assert!(store
        .tasks()
        .unwrap()
        .attempts("t", "a")
        .unwrap()
        .is_empty());
    assert_eq!(git(&repo_b.root, &["status", "--porcelain"]), before);
    assert_eq!(git(&repo_b.root, &["worktree", "list"]).lines().count(), 1);
    planned(
        &store,
        &repo_b,
        "u",
        &in_place,
        vec![reads("look")],
        TaskStage::Implementing,
    );
    coordinator.tick(&store, &launcher, NOW + 1).unwrap();
    let workspaces = store.tasks().unwrap().workspaces("u").unwrap();
    assert_eq!(workspaces[0].mode, WorkspaceMode::Checkout);
}

#[test]
fn recovery_takes_its_retry_limit_and_provider_fallback_from_the_snapshot() {
    let _published = tests_lock();
    // One retry allowed: the second failure exhausts the lineage.
    let repo_a = repo();
    let store = open(&repo_a);
    let once = Autonomy {
        retry_limit: 1,
        ..Autonomy::default()
    };
    planned(
        &store,
        &repo_a,
        "t",
        &once,
        vec![writes("a", &["src"])],
        TaskStage::Implementing,
    );
    let launcher = Recorded::default();
    let coordinator = Coordinator::new("gpui-1", ExplicitStart);
    coordinator.tick(&store, &launcher, NOW).unwrap();
    let session = launcher.last().0.id;
    confirm_started(&store, session, NOW).unwrap();
    let Outcome::Retry { at, .. } = fail(&store, session, "503", NOW + 1) else {
        panic!("expected a retry");
    };
    stop_all(&store, at);
    coordinator.tick(&store, &launcher, at).unwrap();
    let session = launcher.last().0.id;
    confirm_started(&store, session, at).unwrap();
    assert_eq!(fail(&store, session, "503", at + 1), Outcome::Exhausted);

    // Provider fallback is the policy's (snapshot and live), whatever the
    // allowlist's own flag says.
    const MONO: &str = "mono-autonomy-fallback";
    for allowed in [false, true] {
        let repo = repo();
        let store = open(&repo);
        let project = repo.root.to_string_lossy().into_owned();
        let list = Allowlist {
            accounts: vec![
                AllowedAccount::new("codex", "default"),
                AllowedAccount {
                    priority: 1,
                    ..AllowedAccount::new("claude", "default")
                },
            ],
            provider_fallback: !allowed,
        };
        eligibility::save_allowlist(&store, Scope::Project(&project), &list).unwrap();
        let snapshot = Autonomy {
            provider_fallback: allowed,
            ..Autonomy::default()
        };
        let mut live = std::collections::HashMap::new();
        live.insert(
            MONO.to_owned(),
            MonoAutonomy {
                policy: snapshot.clone(),
                ..MonoAutonomy::default()
            },
        );
        publish(live);
        planned_for(
            &store,
            &owned_by(&repo, "t", MONO),
            &snapshot,
            vec![writes("a", &["src"])],
            TaskStage::Implementing,
        );
        let launcher = Recorded::default();
        coordinator.tick(&store, &launcher, NOW).unwrap();
        let session = launcher.last().0.id;
        confirm_started(&store, session, NOW).unwrap();
        let outcome = fail(&store, session, "codex is not installed", NOW + 1);
        if allowed {
            assert!(
                matches!(&outcome, Outcome::Retry { provider, .. } if provider == "claude"),
                "{outcome:?}"
            );
        } else {
            assert!(matches!(outcome, Outcome::Blocked(_)), "{outcome:?}");
        }
    }
}

#[test]
fn a_mono_and_project_allowlist_overrides_the_monos_own_list() {
    let store = memory();
    let mono = Allowlist::only("codex", "account-a");
    let override_list = Allowlist::only("codex", "account-b");
    eligibility::save_allowlist(&store, Scope::Mono("mono-1"), &mono).unwrap();
    eligibility::save_allowlist(
        &store,
        Scope::MonoProject("mono-1", "/repo/"),
        &override_list,
    )
    .unwrap();
    let pick = |project: &str| {
        eligibility::allowlist_for(&store, Some("mono-1"), project)
            .unwrap()
            .unwrap()
    };
    assert_eq!(pick("/repo"), override_list);
    assert_eq!(pick("/other"), mono);
    eligibility::clear_allowlist(&store, Scope::MonoProject("mono-1", "/repo")).unwrap();
    assert_eq!(pick("/repo"), mono);
}

#[test]
fn a_commit_made_after_commit_authority_was_revoked_is_sent_back_at_review() {
    use crate::tasks::review::tests::{feature, CHECK};
    use crate::tasks::review::{Finding, ReviewOutcome};
    for revoked in [false, true] {
        let repo = repo();
        let store = open(&repo);
        let checked = crate::tasks::Assignment {
            verification: vec![CHECK.into()],
            ..assignment("a", &[])
        };
        planned(
            &store,
            &repo,
            "t",
            &Autonomy::default(),
            vec![checked],
            TaskStage::Implementing,
        );
        let coordinator = Coordinator::new("gpui-1", Live::new(Autonomy::default()));
        let launcher = Recorded::default();
        coordinator.tick(&store, &launcher, NOW).unwrap();
        let session = launcher.last().0.id;
        confirm_started(&store, session, NOW).unwrap();
        // The user revokes commits while the worker runs; it commits anyway.
        if revoked {
            coordinator.policy().set(Autonomy {
                commit: false,
                ..Autonomy::default()
            });
        }
        let tree =
            std::path::PathBuf::from(&store.tasks().unwrap().workspaces("t").unwrap()[0].path);
        feature(&tree);
        git(&tree, &["add", "."]);
        git(&tree, &["commit", "-q", "-m", "Add the feature"]);
        let sha = git(&tree, &["rev-parse", "--short", "HEAD"]);
        crate::app_access::worker_action(
            &store,
            session,
            "r1",
            "tasks.worker.result",
            &serde_json::json!({"outcome": "succeeded", "summary": "Added the feature",
                "validation": ["test -f src/feature.rs passed"]}),
            NOW + 1,
        )
        .unwrap();
        let tick = coordinator.tick(&store, &launcher, NOW + 2).unwrap();
        assert_eq!(tick.reviews.len(), 1, "{tick:?}");
        let reviews = coordinator.review(&store, NOW + 3).unwrap();
        let review = &reviews[0];
        if !revoked {
            assert_eq!(review.outcome, ReviewOutcome::Accepted, "{review:?}");
            continue;
        }
        assert_eq!(review.outcome, ReviewOutcome::Correction, "{review:?}");
        let found = review
            .findings
            .iter()
            .find_map(|f| match f {
                Finding::UnauthorizedCommits { commits } => Some(commits.clone()),
                _ => None,
            })
            .expect("the commit is a finding");
        assert_eq!(found.len(), 1);
        assert!(
            sha.starts_with(&found[0]) || found[0].starts_with(&sha),
            "{found:?} {sha}"
        );
        assert!(review.findings.iter().any(|f| f
            .describe()
            .contains("Commits made without commit authority")));
        assert_eq!(status(&store, "t", "a"), AssignmentState::Queued);
    }
}

#[test]
fn turning_provider_fallback_off_or_lowering_retries_reaches_running_tasks() {
    const MONO: &str = "mono-autonomy-live-recovery";
    let _published = tests_lock();
    let publish_live = |live: Autonomy| {
        let mut policies = std::collections::HashMap::new();
        policies.insert(
            MONO.to_owned(),
            MonoAutonomy {
                policy: live,
                ..MonoAutonomy::default()
            },
        );
        publish(policies);
    };
    let coordinator = Coordinator::new("gpui-1", ExplicitStart);
    let launch = |store: &Store, repo: &Repo, snapshot: &Autonomy, launcher: &Recorded| {
        let new = owned_by(repo, "t", MONO);
        planned_for(
            store,
            &new,
            snapshot,
            vec![writes("a", &["src"])],
            TaskStage::Implementing,
        );
        coordinator.tick(store, launcher, NOW).unwrap();
        let session = launcher.last().0.id;
        confirm_started(store, session, NOW).unwrap();
        session
    };

    // The task started with provider fallback; the user turns it off.
    let repo_a = repo();
    let store = open(&repo_a);
    let project = repo_a.root.to_string_lossy().into_owned();
    let list = Allowlist {
        accounts: vec![
            AllowedAccount::new("codex", "default"),
            AllowedAccount {
                priority: 1,
                ..AllowedAccount::new("claude", "default")
            },
        ],
        provider_fallback: true,
    };
    eligibility::save_allowlist(&store, Scope::Project(&project), &list).unwrap();
    let fallback = Autonomy {
        provider_fallback: true,
        ..Autonomy::default()
    };
    publish_live(fallback.clone());
    let launcher = Recorded::default();
    let session = launch(&store, &repo_a, &fallback, &launcher);
    publish_live(Autonomy::default());
    let outcome = fail(&store, session, "codex is not installed", NOW + 1);
    assert!(matches!(outcome, Outcome::Blocked(_)), "{outcome:?}");

    // The task started with three retries; the user lowers it to one.
    let repo_b = repo();
    let store = open(&repo_b);
    publish_live(Autonomy::default());
    let launcher = Recorded::default();
    let session = launch(&store, &repo_b, &Autonomy::default(), &launcher);
    publish_live(Autonomy {
        retry_limit: 1,
        ..Autonomy::default()
    });
    let Outcome::Retry { at, .. } = fail(&store, session, "503", NOW + 1) else {
        panic!("expected a retry");
    };
    stop_all(&store, at);
    coordinator.tick(&store, &launcher, at).unwrap();
    let session = launcher.last().0.id;
    confirm_started(&store, session, at).unwrap();
    assert_eq!(fail(&store, session, "503", at + 1), Outcome::Exhausted);

    // A retry already scheduled is refused once the limit drops below it.
    let repo_c = repo();
    let store = open(&repo_c);
    publish_live(Autonomy::default());
    let launcher = Recorded::default();
    let session = launch(&store, &repo_c, &Autonomy::default(), &launcher);
    let Outcome::Retry { at, .. } = fail(&store, session, "503", NOW + 1) else {
        panic!("expected a retry");
    };
    publish_live(Autonomy {
        retry_limit: 0,
        ..Autonomy::default()
    });
    stop_all(&store, at);
    coordinator.tick(&store, &launcher, at).unwrap();
    assert_eq!(launcher.count(), 1, "no replacement past the lowered limit");
    assert_eq!(status(&store, "t", "a"), AssignmentState::Failed);
}
