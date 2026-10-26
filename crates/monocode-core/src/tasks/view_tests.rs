//! The monitoring view: per-state labels and badges, quiet questions,
//! detached workers, the three times, and no secret in any text it shows.
use super::*;
use crate::store::Store;
use crate::tasks::coordinator::FLAG;
use crate::tasks::decisions::tests::{
    answer, ask, decision, independent, mono, mono_received, running, task_with, NOW,
};
use crate::tasks::AttemptState;
use crate::tasks::{inbox, recovery, Requester};
use crate::usage::Failure;
use serde_json::json;
use std::path::Path;

/// When the tests read the view.
const SEEN: u64 = NOW + 100;

fn store_with_worker() -> (Store, i64) {
    let store = Store::open(Path::new(":memory:")).unwrap();
    task_with(&store, "/project", vec![independent("a")]);
    let worker = running(&store, "a", "x");
    (store, worker)
}

fn only(store: &Store) -> TaskMonitor {
    let mut all = monitor(store, "mono-1", SEEN).unwrap();
    assert_eq!(all.len(), 1);
    all.remove(0)
}

fn escalate(store: &Store, id: &str) {
    let revision = decision(store, id).revision;
    mono(
        store,
        "tasks.decisions.escalate",
        json!({"decisionId": id, "revision": revision,
               "why": "It changes the public API",
               "recommendation": "Keep the old API"}),
    )
    .unwrap();
}

/// Everything the card would print for the task.
fn all_text(view: &TaskMonitor) -> String {
    let mut text: Vec<String> = view.timeline.iter().map(|e| e.text.clone()).collect();
    text.extend(view.recovery.iter().cloned());
    text.extend(view.control.reason.iter().cloned());
    text.push(view.control.title.clone());
    text.extend(view.workers.iter().map(|w| w.title.clone()));
    text.join("\n")
}

#[test]
fn nothing_is_shown_while_the_coordinator_is_off() {
    let (store, _) = store_with_worker();
    store.save_setting(FLAG, "disabled").unwrap();
    assert!(monitor(&store, "mono-1", SEEN).unwrap().is_empty());
}

#[test]
fn each_task_state_shows_its_activity_and_badge() {
    let (store, worker) = store_with_worker();
    let view = only(&store);
    assert_eq!(view.stage, TaskStage::Implementing);
    assert_eq!(view.activity, MonoActivity::Supervising);
    assert_eq!(view.workers[0].status, AssignmentState::Running);
    assert_eq!(view.workers[0].session, Some(worker));
    assert_eq!(view.badge(), Some(Badge::Working(1)));
    assert_eq!(badge(&[view]).unwrap().label(), "1 working");

    // A worker question goes to Mono: Mono decides, the worker waits.
    let asked = ask(&store, worker, "q1", "Which parser?", "assignment");
    let view = only(&store);
    assert_eq!(view.activity, MonoActivity::Deciding);
    assert!(view.workers[0].needs_input);
    assert_eq!(view.badge(), None);

    // Escalated, it waits for the user and outranks everything.
    escalate(&store, &asked);
    let view = only(&store);
    assert_eq!(view.activity, MonoActivity::Waiting);
    assert_eq!(view.for_user(), 1);
    assert_eq!(view.badge(), Some(Badge::NeedsDecision));
    assert_eq!(Badge::NeedsDecision.label(), "Needs your decision");
}

#[test]
fn a_paused_task_shows_pausing_then_its_detached_worker() {
    let (store, worker) = store_with_worker();
    let revision = store.tasks().unwrap().task("t").unwrap().unwrap().revision;
    lifecycle::pause(&store, "t", revision, Requester::User, NOW + 5).unwrap();
    let view = only(&store);
    // The stop is not confirmed yet.
    assert_eq!(view.control.display, Display::Pausing);
    assert_eq!(view.activity, MonoActivity::Waiting);
    assert_eq!(view.badge(), Some(Badge::Paused));
    assert_eq!(Badge::Paused.label(), "Paused");
    // The interrupted worker's session no longer belongs to the task.
    assert_eq!(view.workers[0].session, None);
    assert_eq!(view.workers[0].detached, vec![worker]);
    assert!(view
        .timeline
        .iter()
        .any(|e| e.text == "Paused by user" && e.kind == timeline::TimelineKind::Correction));
    // The pause's own recovery row is not shown as recovery.
    assert!(!view.recovering);
    assert!(view.recovery.is_empty());
}

#[test]
fn badges_follow_their_precedence_across_tasks() {
    let (store, _) = store_with_worker();
    let base = only(&store);
    let with = |f: &dyn Fn(&mut TaskMonitor)| {
        let mut view = base.clone();
        f(&mut view);
        view
    };
    let working = base.clone();
    let reviewing = with(&|v| v.activity = MonoActivity::Reviewing);
    let paused = with(&|v| v.control.display = Display::Paused);
    let recovering = with(&|v| v.recovering = true);
    let deciding = with(&|v| {
        v.decisions.push(DecisionView {
            id: "d".into(),
            assignment_id: None,
            escalated: true,
            quiet: false,
        })
    });
    let all = [
        working.clone(),
        reviewing.clone(),
        paused.clone(),
        recovering.clone(),
        deciding,
    ];
    assert_eq!(badge(&all), Some(Badge::NeedsDecision));
    assert_eq!(badge(&all[..4]), Some(Badge::Recovering));
    assert_eq!(badge(&all[..3]), Some(Badge::Paused));
    assert_eq!(badge(&all[..2]), Some(Badge::Reviewing));
    assert_eq!(Badge::Reviewing.label(), "Reviewing");
    assert_eq!(Badge::Recovering.label(), "Recovering");
    // Workers are counted across tasks.
    assert_eq!(badge(&[working.clone(), working]), Some(Badge::Working(2)));
    assert_eq!(Badge::Working(2).label(), "2 working");
    assert_eq!(badge(&[]), None);
}

#[test]
fn a_secret_in_a_worker_error_never_reaches_the_timeline() {
    let (store, _) = store_with_worker();
    let secret = "sk-ant-api03-Zx81kQ2vB7nP0wLmT4yR";
    let token = "ghp_q8W2e4R6t8Y0u2I4o6P8a0S2d4F6g8H0j2K4";
    let titled = "xoxb-1234567890-abcdefghij";
    // Free text that is not an error: titles, and who answered.
    let conn = store.connection();
    conn.execute(
        "UPDATE native_tasks SET title=?1 WHERE id='t'",
        [format!("Ship it {titled}")],
    )
    .unwrap();
    conn.execute(
        "UPDATE native_task_assignments SET title=?1 WHERE task_id='t' AND id='a'",
        [format!("Assignment {titled}")],
    )
    .unwrap();
    store
        .tasks()
        .unwrap()
        .append_event(
            &crate::tasks::NewEvent {
                id: "answered-by-secret".into(),
                task_id: "t".into(),
                assignment_id: Some("a".into()),
                attempt_id: None,
                kind: "decision.answered".into(),
                payload: json!({"by": format!("mono {secret}"), "requester": token}),
            },
            NOW + 2,
        )
        .unwrap();
    let error = format!("503 Service Unavailable: request with Bearer {secret} and {token} failed");
    let outcome = recovery::interrupt(&store, "x", Failure::Temporary, &error, None, NOW + 3);
    assert!(
        matches!(outcome, Ok(recovery::Outcome::Retry { .. })),
        "{outcome:?}"
    );
    let view = only(&store);
    let text = all_text(&view);
    assert!(view
        .timeline
        .iter()
        .any(|e| e.kind == timeline::TimelineKind::Failure));
    assert!(text.contains("temporary"), "{text}");
    assert!(!text.contains(secret), "{text}");
    assert!(!text.contains("Zx81kQ2vB7nP0wLmT4yR"), "{text}");
    assert!(!text.contains(token), "{text}");
    assert!(!text.contains(titled), "{text}");
    assert!(text.contains("Answered by mono [redacted]"), "{text}");
    assert!(view.recovering);
    assert_eq!(view.badge(), Some(Badge::Recovering));
    assert_eq!(view.recovery.len(), 1);
}

#[test]
fn an_open_question_mono_stopped_being_reminded_of_is_quiet() {
    let (store, worker) = store_with_worker();
    let chat = store.create("codex", "/home").unwrap().id;
    let asked = ask(&store, worker, "q1", "Which parser?", "assignment");
    let mut now = NOW + 2;
    let mut quiet_before_last = false;
    while let Some(batch) = inbox::next_batch(&store, "mono-1", now).unwrap() {
        quiet_before_last |= only(&store).decisions[0].quiet;
        mono_received(&store, chat, &batch);
        inbox::acknowledge_delivered(&store, "mono-1", chat, now).unwrap();
        now += inbox::REMIND_AFTER_MS;
    }
    assert!(!quiet_before_last, "quiet only once the reminders stopped");
    let view = only(&store);
    assert_eq!(view.decisions[0].id, asked);
    assert!(view.decisions[0].quiet);
    assert!(!view.decisions[0].escalated);
}

#[test]
fn worker_activity_status_sync_and_mono_review_are_separate_times() {
    let (store, worker) = store_with_worker();
    let view = only(&store);
    assert_eq!(view.last_worker_activity, None);
    assert_eq!(view.last_status_sync, None);
    assert_eq!(view.last_mono_review, None);

    let asked = ask(&store, worker, "q1", "Which parser?", "assignment");
    assert_eq!(only(&store).last_worker_activity, Some(NOW + 1));
    answer(&store, &asked, "Use the existing parser");
    let view = only(&store);
    assert_eq!(view.last_mono_review, Some(NOW + 2));
    assert_eq!(view.last_worker_activity, Some(NOW + 1));

    let tasks = store.tasks().unwrap();
    let at = NOW + 50;
    tasks
        .acquire_lease(&lease_resource("t"), "holder", LEASE_TTL_MS, at)
        .unwrap();
    assert_eq!(only(&store).last_status_sync, Some(at));
    // Once the lease has expired it says nothing about a recent sync.
    let later = monitor(&store, "mono-1", at + LEASE_TTL_MS + 1).unwrap();
    assert_eq!(later[0].last_status_sync, None);
}

#[test]
fn a_worker_held_after_a_takeover_is_not_counted_as_working() {
    use crate::tasks::coordinator::{Coordinator, ExplicitStart};
    use crate::tasks::recovery::tests::{attempts, launched_unconfirmed, Recorded};
    use crate::tasks::review::tests::{open, repo, NOW};
    let repo = repo();
    let store = open(&repo);
    let launcher = Recorded::default();
    launched_unconfirmed(&store, &repo, &launcher);
    let session = launcher.last().0.id;
    assert!(crate::tasks::confirm_started(&store, session, NOW).unwrap());
    let running = monitor(&store, "mono-1", NOW + 1).unwrap().remove(0);
    assert_eq!(running.working(), 1);
    assert_eq!(running.workers[0].state(None), WorkerState::Working);
    // Another app (which cannot be checked) held the lease; once it lapses
    // this process takes the task over and holds that app's worker.
    let later = NOW + LEASE_TTL_MS + 1_000;
    store
        .tasks()
        .unwrap()
        .acquire_lease(
            &lease_resource("t"),
            "monocode-desktop",
            LEASE_TTL_MS,
            later,
        )
        .unwrap();
    let after = later + LEASE_TTL_MS + 2;
    Coordinator::new("gpui-2", ExplicitStart)
        .tick(&store, &launcher, after)
        .unwrap();
    assert_eq!(attempts(&store)[0].status, AttemptState::Interrupted);
    let held = monitor(&store, "mono-1", after + 1).unwrap().remove(0);
    assert_eq!(held.workers[0].status, AssignmentState::Running);
    assert_eq!(held.workers[0].state(None), WorkerState::Stopped);
    assert_eq!(held.working(), 0);
    assert_ne!(held.badge(), Some(Badge::Working(1)));
    assert_eq!(badge(&[held]), None);
}
