//! Recovery through the coordinator against temporary Git repositories. A
//! fake provider "fails" by ending the worker's turn with the error text each
//! category is recognised by; the fake launcher stands in for the app.
use super::*;
use crate::accounts::eligibility::{self, AllowedAccount, Allowlist, Scope};
use crate::accounts::{Account, Book};
use crate::agent::Settings;
use crate::store::{Session, Store};
use crate::tasks::coordinator::{Coordinator, ExplicitStart, Launcher};
use crate::tasks::review::tests::{open, repo, scoped, Repo, NOW};
use crate::tasks::store::tests::plan;
use crate::tasks::{
    confirm_started, AssignmentState, AttemptState, NewTask, Requester, State, Subject,
    TaskLifecycle, TaskStage,
};
use std::cell::RefCell;

/// Records each session the coordinator asks the app to start.
#[derive(Default)]
pub(in crate::tasks) struct Recorded(pub(in crate::tasks) RefCell<Vec<(Session, Settings)>>);
impl Launcher for Recorded {
    fn start(&self, session: &Session, settings: &Settings) -> Result<(), String> {
        self.0
            .borrow_mut()
            .push((session.clone(), settings.clone()));
        Ok(())
    }
}

impl Recorded {
    pub(in crate::tasks) fn last(&self) -> (Session, Settings) {
        self.0.borrow().last().cloned().unwrap()
    }
    pub(in crate::tasks) fn count(&self) -> usize {
        self.0.borrow().len()
    }
}

/// The accounts the app knows for codex, besides the default.
pub(in crate::tasks) fn accounts(store: &Store, ids: &[&str]) {
    let book = Book {
        accounts: ids
            .iter()
            .map(|id| Account {
                id: (*id).into(),
                provider: "codex".into(),
                label: format!("Account {id}"),
            })
            .collect(),
        ..Book::default()
    };
    store.save_provider_accounts(&book).unwrap();
}

pub(in crate::tasks) fn allow(store: &Store, repo: &Repo, entries: &[(&str, Option<&str>)]) {
    let list = Allowlist {
        accounts: entries
            .iter()
            .enumerate()
            .map(|(i, (id, group))| AllowedAccount {
                quota_group: group.map(str::to_owned),
                priority: i as i64,
                ..AllowedAccount::new("codex", id)
            })
            .collect(),
        provider_fallback: false,
    };
    let project = repo.root.to_string_lossy().into_owned();
    eligibility::save_allowlist(store, Scope::Project(&project), &list).unwrap();
}

/// Task `t` created with its policy snapshot, one worktree assignment `a`,
/// started by Mono, launched and confirmed. Returns the worker's session.
pub(in crate::tasks) fn launched(store: &Store, repo: &Repo, launcher: &Recorded) -> i64 {
    launched_unconfirmed(store, repo, launcher);
    let session = launcher.last().0.id;
    assert!(confirm_started(store, session, NOW).unwrap());
    session
}

/// As [`launched`], but the app never confirms the start.
pub(in crate::tasks) fn launched_unconfirmed(store: &Store, repo: &Repo, launcher: &Recorded) {
    let new = NewTask {
        id: "t".into(),
        mono_id: Some("mono-1".into()),
        project: repo.root.to_string_lossy().into_owned(),
        title: "Ship it".into(),
        request: "Implement the feature".into(),
        criteria: vec![],
    };
    policy::create_task(store, &new, "codex", NOW).unwrap();
    let tasks = store.tasks().unwrap();
    tasks
        .save_plan("t", 1, &plan(vec![scoped(&[])]), NOW)
        .unwrap();
    let t = Subject::Task("t");
    for stage in [TaskStage::Ready, TaskStage::Implementing] {
        let revision = tasks.revision(t).unwrap();
        tasks
            .transition(t, State::Stage(stage), Requester::Mono, revision, NOW)
            .unwrap();
    }
    coordinator().tick(store, launcher, NOW).unwrap();
}

pub(in crate::tasks) fn coordinator() -> Coordinator {
    Coordinator::new("gpui-1", ExplicitStart)
}

pub(in crate::tasks) fn fail(store: &Store, session: i64, error: &str, now: u64) -> Outcome {
    let end = TurnEnd {
        error: Some(error.into()),
        ..TurnEnd::default()
    };
    turn_ended(store, session, &end, now).unwrap()
}

/// The app stops every worker recovery asked it to.
pub(in crate::tasks) fn stop_all(store: &Store, now: u64) -> usize {
    let stops = pending_stops(store).unwrap();
    for stop in &stops {
        stopped(store, &stop.delivery, now).unwrap();
    }
    stops.len()
}

pub(in crate::tasks) fn attempts(store: &Store) -> Vec<crate::tasks::Attempt> {
    store.tasks().unwrap().attempts("t", "a").unwrap()
}

pub(in crate::tasks) fn kinds(store: &Store, kind: &str) -> Vec<crate::tasks::Event> {
    let all = store.tasks().unwrap().events_after(0, 10_000).unwrap();
    all.into_iter().filter(|e| e.kind == kind).collect()
}

fn task(store: &Store) -> crate::tasks::Task {
    store.tasks().unwrap().task("t").unwrap().unwrap()
}

fn healthy(store: &Store, account: &str, now: u64) -> bool {
    eligibility::health(store)
        .unwrap()
        .get("codex", account)
        .available(now)
}

#[test]
fn a_temporary_failure_backs_off_on_the_same_account() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    let first = attempts(&store)[0].clone();
    assert_eq!(first.account_id.as_deref(), Some("default"));
    let at = NOW + 10;
    let outcome = fail(&store, session, "503 Service Unavailable", at);
    let due = at + backoff_ms(1);
    assert_eq!(
        outcome,
        Outcome::Retry {
            at: due,
            provider: "codex".into(),
            account: "default".into()
        }
    );
    assert_eq!(attempts(&store)[0].status, AttemptState::Interrupted);
    // Progress is preserved: the assignment keeps its slot while recovering.
    let tasks = store.tasks().unwrap();
    assert_eq!(
        tasks.assignment("t", "a").unwrap().unwrap().status,
        AssignmentState::Running
    );
    // A repeated report of the same end changes nothing.
    assert_eq!(fail(&store, session, "503", at + 1), Outcome::Ignored);
    assert_eq!(kinds(&store, "attempt.interrupted").len(), 1);
    // The old worker must be stopped before anything replaces it.
    coordinator().tick(&store, &launcher, due).unwrap();
    assert_eq!(launcher.count(), 1);
    assert_eq!(stop_all(&store, due), 1);
    coordinator().tick(&store, &launcher, due - 1).unwrap();
    assert_eq!(launcher.count(), 1, "backoff not yet over");
    let tick = coordinator().tick(&store, &launcher, due).unwrap();
    assert_eq!(tick.launched.len(), 1, "{tick:?}");
    let (replacement, settings) = launcher.last();
    assert_eq!(settings.account.id, "default");
    assert_eq!(attempts(&store).len(), 2);
    // The next temporary failure waits twice as long.
    confirm_started(&store, replacement.id, due).unwrap();
    match fail(&store, replacement.id, "Overloaded", due + 1) {
        Outcome::Retry { at, .. } => assert_eq!(at, due + 1 + backoff_ms(2)),
        other => panic!("{other:?}"),
    }
    assert!(backoff_ms(2) == 2 * BACKOFF_BASE_MS && backoff_ms(30) == BACKOFF_MAX_MS);
}

#[test]
fn exhaustion_cools_the_account_down_and_switches_to_the_next_allowed_one() {
    let repo = repo();
    let store = open(&repo);
    accounts(&store, &["account-b"]);
    allow(&store, &repo, &[("default", None), ("account-b", None)]);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    let end = TurnEnd {
        error: Some("Usage limit reached".into()),
        usage_limited: true,
        resets_at: Some(NOW + 3_600_000),
        ..TurnEnd::default()
    };
    let outcome = turn_ended(&store, session, &end, NOW + 5).unwrap();
    assert!(
        matches!(&outcome, Outcome::Retry { account, at, .. } if account == "account-b" && *at == NOW + 5),
        "{outcome:?}"
    );
    assert!(!healthy(&store, "default", NOW + 3_599_999));
    stop_all(&store, NOW + 6);
    coordinator().tick(&store, &launcher, NOW + 6).unwrap();
    let (_, settings) = launcher.last();
    assert_eq!(settings.account.id, "account-b");
    assert_eq!(attempts(&store)[1].account_id.as_deref(), Some("account-b"));
}

#[test]
fn auth_failures_mark_the_account_unhealthy_and_switch() {
    let repo = repo();
    let store = open(&repo);
    accounts(&store, &["account-b"]);
    allow(&store, &repo, &[("default", None), ("account-b", None)]);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    let outcome = fail(&store, session, "401 Unauthorized: token expired", NOW + 5);
    assert!(matches!(&outcome, Outcome::Retry { account, .. } if account == "account-b"));
    assert!(!healthy(&store, "default", NOW + 999_999_999));
}

#[test]
fn a_shared_quota_never_switches_among_the_accounts_sharing_it() {
    let repo = repo();
    let store = open(&repo);
    accounts(&store, &["account-b"]);
    allow(
        &store,
        &repo,
        &[("default", Some("org")), ("account-b", Some("org"))],
    );
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    let reset = NOW + 600_000;
    let end = TurnEnd {
        error: Some("Your organization's usage limit has been reached".into()),
        resets_at: Some(reset),
        ..TurnEnd::default()
    };
    assert_eq!(
        turn_ended(&store, session, &end, NOW + 5).unwrap(),
        Outcome::Waiting { until: reset }
    );
    assert!(!healthy(&store, "default", reset - 1) && !healthy(&store, "account-b", reset - 1));
    stop_all(&store, NOW + 6);
    for now in [NOW + 6, NOW + 60_000, reset - 1] {
        coordinator().tick(&store, &launcher, now).unwrap();
    }
    assert_eq!(launcher.count(), 1, "nothing switches before the reset");
    coordinator().tick(&store, &launcher, reset).unwrap();
    assert_eq!(launcher.last().1.account.id, "default");
    // Without a known reset it blocks rather than cycling.
    let repo = self::repo();
    let store = open(&repo);
    accounts(&store, &["account-b"]);
    allow(
        &store,
        &repo,
        &[("default", Some("org")), ("account-b", Some("org"))],
    );
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    let outcome = fail(&store, session, "Workspace credits exhausted", NOW + 5);
    assert!(matches!(outcome, Outcome::Blocked(_)), "{outcome:?}");
}

#[test]
fn configuration_policy_and_unavailable_providers_block_without_touching_accounts() {
    for (error, needle) in [
        ("model_not_found: gpt-9", "configuration"),
        ("content_policy_violation", "policy"),
        ("codex is not installed", "provider fallback"),
    ] {
        let repo = repo();
        let store = open(&repo);
        accounts(&store, &["account-b"]);
        allow(&store, &repo, &[("default", None), ("account-b", None)]);
        let launcher = Recorded::default();
        let session = launched(&store, &repo, &launcher);
        let outcome = fail(&store, session, error, NOW + 5);
        assert!(
            matches!(&outcome, Outcome::Blocked(reason) if reason.contains(needle)),
            "{error}: {outcome:?}"
        );
        assert!(healthy(&store, "default", NOW + 6), "{error}");
        stop_all(&store, NOW + 6);
        coordinator()
            .tick(&store, &launcher, NOW + 10_000_000)
            .unwrap();
        assert_eq!(launcher.count(), 1, "{error}: no account cycling");
        assert_eq!(task(&store).lifecycle, TaskLifecycle::Blocked);
    }
}

#[test]
fn an_unknown_failure_is_retried_once_then_blocks() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    let outcome = fail(&store, session, "Something odd happened", NOW + 5);
    let Outcome::Retry { at, account, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(account, "default");
    stop_all(&store, at);
    coordinator().tick(&store, &launcher, at).unwrap();
    let replacement = launcher.last().0.id;
    confirm_started(&store, replacement, at).unwrap();
    let outcome = fail(&store, replacement, "Something odd happened again", at + 1);
    assert!(matches!(outcome, Outcome::Blocked(_)), "{outcome:?}");
}

#[test]
fn no_eligible_account_blocks_the_task_with_a_clear_reason_and_tells_mono() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    // The default allowlist is only the account the task started with.
    let session = launched(&store, &repo, &launcher);
    let outcome = fail(&store, session, "Not signed in", NOW + 5);
    let Outcome::Blocked(reason) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(reason.contains("No eligible account"), "{reason}");
    assert!(
        reason.contains("codex/default needs to sign in again"),
        "{reason}"
    );
    let task = task(&store);
    assert_eq!(task.lifecycle, TaskLifecycle::Blocked);
    assert_eq!(task.blocked_reason.as_deref(), Some(reason.as_str()));
    // Progress is preserved and Mono hears about it.
    let tasks = store.tasks().unwrap();
    assert_eq!(tasks.workspaces("t").unwrap().len(), 1);
    let owed = tasks
        .pending_deliveries(crate::tasks::review::MONO_REVIEW)
        .unwrap();
    assert_eq!(owed.len(), 1);
    let batch = crate::tasks::inbox::next_batch(&store, "mono-1", NOW + 6)
        .unwrap()
        .unwrap();
    assert!(
        batch.message.contains("Recovery is blocked"),
        "{}",
        batch.message
    );
    assert!(
        batch.message.contains("needs to sign in again"),
        "{}",
        batch.message
    );
    // Signing in and unblocking resumes on the same lineage.
    eligibility::mark_healthy(&store, "codex", "default", NOW + 7).unwrap();
    let t = Subject::Task("t");
    let revision = tasks.revision(t).unwrap();
    let active = State::Lifecycle(TaskLifecycle::Active);
    tasks
        .transition(t, active, Requester::User, revision, NOW + 8)
        .unwrap();
    stop_all(&store, NOW + 8);
    coordinator().tick(&store, &launcher, NOW + 9).unwrap();
    assert_eq!(launcher.count(), 2);
}

#[test]
fn cancelled_tasks_never_recover_and_paused_ones_wait_for_resume() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    let tasks = store.tasks().unwrap();
    let t = Subject::Task("t");
    let cancel = State::Lifecycle(TaskLifecycle::Cancelled);
    tasks
        .transition(
            t,
            cancel,
            Requester::User,
            tasks.revision(t).unwrap(),
            NOW + 1,
        )
        .unwrap();
    assert_eq!(fail(&store, session, "503", NOW + 2), Outcome::Abandoned);
    stop_all(&store, NOW + 3);
    for now in [NOW + 3, NOW + 10_000_000] {
        coordinator().tick(&store, &launcher, now).unwrap();
    }
    assert_eq!(launcher.count(), 1);

    let repo = self::repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    let tasks = store.tasks().unwrap();
    let paused = State::Lifecycle(TaskLifecycle::Paused);
    tasks
        .transition(
            t,
            paused,
            Requester::User,
            tasks.revision(t).unwrap(),
            NOW + 1,
        )
        .unwrap();
    let Outcome::Retry { at, .. } = fail(&store, session, "503", NOW + 2) else {
        panic!("a paused task still records its recovery");
    };
    stop_all(&store, at);
    coordinator()
        .tick(&store, &launcher, at + 1_000_000)
        .unwrap();
    assert_eq!(launcher.count(), 1, "paused tasks do not recover");
    let active = State::Lifecycle(TaskLifecycle::Active);
    tasks
        .transition(
            t,
            active,
            Requester::User,
            tasks.revision(t).unwrap(),
            at + 2,
        )
        .unwrap();
    coordinator()
        .tick(&store, &launcher, at + 1_000_001)
        .unwrap();
    assert_eq!(launcher.count(), 2);
}

#[test]
fn the_retry_budget_holds_across_account_changes() {
    let repo = repo();
    let store = open(&repo);
    let ids = [
        "account-b",
        "account-c",
        "account-d",
        "account-e",
        "account-f",
    ];
    accounts(&store, &ids);
    let mut entries = vec![("default", None)];
    entries.extend(ids.iter().map(|id| (*id, None)));
    allow(&store, &repo, &entries);
    let launcher = Recorded::default();
    let mut session = launched(&store, &repo, &launcher);
    let mut now = NOW;
    let mut used = Vec::new();
    for _ in 0..RETRY_BUDGET {
        now += 10;
        let outcome = fail(&store, session, "usage_limit_reached", now);
        let Outcome::Retry { account, .. } = outcome else {
            panic!("{outcome:?}");
        };
        used.push(account);
        stop_all(&store, now);
        coordinator().tick(&store, &launcher, now).unwrap();
        session = launcher.last().0.id;
        confirm_started(&store, session, now).unwrap();
    }
    assert_eq!(used, ["account-b", "account-c", "account-d"]);
    // Eligible accounts remain, but the lineage has spent its budget.
    let outcome = fail(&store, session, "usage_limit_reached", now + 10);
    assert_eq!(outcome, Outcome::Exhausted);
    let tasks = store.tasks().unwrap();
    assert_eq!(
        tasks.assignment("t", "a").unwrap().unwrap().status,
        AssignmentState::Failed
    );
    let lineage = recovery(&tasks, &attempts(&store).last().unwrap().id)
        .unwrap()
        .unwrap();
    assert_eq!(lineage.root_attempt, attempts(&store)[0].id);
    assert_eq!(lineage.retries_used, RETRY_BUDGET);
    stop_all(&store, now + 11);
    coordinator()
        .tick(&store, &launcher, now + 1_000_000)
        .unwrap();
    assert_eq!(launcher.count(), 1 + RETRY_BUDGET as usize);
    assert_eq!(kinds(&store, "recovery.exhausted").len(), 1);
}

fn unblock(store: &Store, now: u64) {
    let tasks = store.tasks().unwrap();
    let t = Subject::Task("t");
    let active = State::Lifecycle(TaskLifecycle::Active);
    let revision = tasks.revision(t).unwrap();
    tasks
        .transition(t, active, Requester::User, revision, now)
        .unwrap();
}

#[test]
fn allowing_another_account_then_unblocking_resumes_on_it() {
    let repo = repo();
    let store = open(&repo);
    accounts(&store, &["account-b"]);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    let outcome = fail(&store, session, "Not signed in", NOW + 5);
    assert!(matches!(outcome, Outcome::Blocked(_)), "{outcome:?}");
    stop_all(&store, NOW + 6);
    // The user allows a second account (read live), then unblocks.
    allow(&store, &repo, &[("default", None), ("account-b", None)]);
    unblock(&store, NOW + 7);
    coordinator().tick(&store, &launcher, NOW + 8).unwrap();
    assert_eq!(launcher.count(), 2);
    assert_eq!(launcher.last().1.account.id, "account-b");
}

#[test]
fn a_paused_task_blocked_by_configuration_is_blocked_when_it_resumes() {
    let repo = repo();
    let store = open(&repo);
    accounts(&store, &["account-b"]);
    allow(&store, &repo, &[("default", None), ("account-b", None)]);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    let tasks = store.tasks().unwrap();
    let t = Subject::Task("t");
    let paused = State::Lifecycle(TaskLifecycle::Paused);
    tasks
        .transition(
            t,
            paused,
            Requester::User,
            tasks.revision(t).unwrap(),
            NOW + 1,
        )
        .unwrap();
    let outcome = fail(&store, session, "model_not_found: gpt-9", NOW + 2);
    assert!(matches!(outcome, Outcome::Blocked(_)), "{outcome:?}");
    assert_eq!(task(&store).lifecycle, TaskLifecycle::Paused);
    stop_all(&store, NOW + 3);
    unblock(&store, NOW + 4);
    coordinator().tick(&store, &launcher, NOW + 5).unwrap();
    assert_eq!(launcher.count(), 1, "resuming does not relaunch");
    let task = task(&store);
    assert_eq!(task.lifecycle, TaskLifecycle::Blocked);
    assert!(
        task.blocked_reason
            .as_deref()
            .unwrap_or("")
            .contains("configuration"),
        "{:?}",
        task.blocked_reason
    );
    assert_eq!(kinds(&store, "recovery.blocked").len(), 1);
    // An explicit unblock retries on the same account, even when the live
    // allowlist now prefers another one.
    allow(&store, &repo, &[("account-b", None), ("default", None)]);
    unblock(&store, NOW + 6);
    coordinator().tick(&store, &launcher, NOW + 7).unwrap();
    assert_eq!(launcher.count(), 2);
    assert_eq!(launcher.last().1.account.id, "default");
}

#[test]
fn a_stop_that_never_completes_is_reported_once_and_blocks_the_replacement() {
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    let session = launched(&store, &repo, &launcher);
    let Outcome::Retry { at, .. } = fail(&store, session, "503", NOW + 1) else {
        panic!("expected a retry");
    };
    // The app never confirms the stop.
    let late = (NOW + 1 + STOP_STALL_MS).max(at);
    let tick = coordinator().tick(&store, &launcher, late - 1).unwrap();
    assert!(tick.stalled.is_empty());
    let tick = coordinator().tick(&store, &launcher, late).unwrap();
    assert_eq!(tick.stalled.len(), 1, "{tick:?}");
    let tick = coordinator()
        .tick(&store, &launcher, late + 10_000)
        .unwrap();
    assert!(tick.stalled.is_empty(), "reported once");
    assert_eq!(
        launcher.count(),
        1,
        "no replacement over an unstopped worker"
    );
    assert_eq!(kinds(&store, "recovery.stop_stalled").len(), 1);
    let batch = crate::tasks::inbox::next_batch(&store, "mono-1", late + 10_001)
        .unwrap()
        .unwrap();
    assert!(
        batch.message.contains("A failed worker has not stopped"),
        "{}",
        batch.message
    );
    // Once the app confirms the stop, recovery continues.
    stop_all(&store, late + 10_002);
    coordinator()
        .tick(&store, &launcher, late + 10_003)
        .unwrap();
    assert_eq!(launcher.count(), 2);
}
