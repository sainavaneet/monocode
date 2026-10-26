//! The Mono inbox: retries of a failing message, and reminders of
//! questions Mono has not answered.
use crate::store::Store;
use crate::tasks::decisions::tests::{
    ask, events, independent, mono_received, running, task_with, NOW,
};
use crate::tasks::inbox;
use std::path::Path;

#[test]
fn a_failing_inbox_message_is_retried_then_surfaced_without_blocking_later_events() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    task_with(&store, "/project", vec![independent("a"), independent("b")]);
    let worker = running(&store, "a", "x");
    let first = ask(&store, worker, "q1", "First question?", "assignment");
    let mut now = NOW + 2;
    let mut seen = Vec::new();
    for attempt in 1..=inbox::INBOX_RETRY_LIMIT {
        let batch = inbox::next_batch(&store, "mono-1", now).unwrap().unwrap();
        // Each retry is a new batch; a failed one never comes back.
        assert!(
            !seen.contains(&batch.id),
            "attempt {attempt} reused {}",
            batch.id
        );
        seen.push(batch.id.clone());
        // While it is out, asking again returns it, not an earlier batch.
        let again = inbox::next_batch(&store, "mono-1", now).unwrap().unwrap();
        assert_eq!(again.id, batch.id, "attempt {attempt}");
        assert!(batch.message.contains("First question?"));
        let abandoned =
            inbox::batch_failed(&store, "mono-1", &batch.id, "Usage limit", now).unwrap();
        if attempt < inbox::INBOX_RETRY_LIMIT {
            assert!(abandoned.is_empty(), "attempt {attempt}");
        } else {
            assert_eq!(abandoned, vec![format!("mono-decision:{first}")]);
        }
        now += 1;
    }
    // Later events are not held behind the abandoned one.
    let other = running(&store, "b", "y");
    ask(&store, other, "q2", "Second question?", "assignment");
    let next = inbox::next_batch(&store, "mono-1", now).unwrap().unwrap();
    assert!(next.message.contains("Second question?"));
    assert!(!next.message.contains("First question?"));
    assert_eq!(events(&store, "mono.inbox_abandoned").len(), 1);
}

#[test]
fn an_unanswered_question_is_sent_to_mono_again_after_the_interval() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    task_with(&store, "/project", vec![independent("a")]);
    let worker = running(&store, "a", "x");
    let chat = store.create("codex", "/home").unwrap().id;
    ask(&store, worker, "q1", "Which parser?", "assignment");
    let mut now = NOW + 2;
    let mut reminders = 0;
    while let Some(batch) = inbox::next_batch(&store, "mono-1", now).unwrap() {
        assert!(batch.message.contains("Which parser?"));
        mono_received(&store, chat, &batch);
        inbox::acknowledge_delivered(&store, "mono-1", chat, now).unwrap();
        // Not again before the interval.
        assert!(
            inbox::next_batch(&store, "mono-1", now + inbox::REMIND_AFTER_MS - 1)
                .unwrap()
                .is_none()
        );
        now += inbox::REMIND_AFTER_MS;
        reminders += 1;
        assert!(
            reminders <= inbox::MAX_REMINDERS + 1,
            "reminders are capped"
        );
    }
    assert_eq!(reminders, inbox::MAX_REMINDERS + 1);
}
