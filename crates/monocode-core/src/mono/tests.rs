use super::*;
use habits::{Habit, Schedule};
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn identity_projects_order_and_hidden_sessions_survive_restart() {
    let temp = tempfile::tempdir().unwrap();
    let db = Store::open(&temp.path().join("native.sqlite")).unwrap();
    let mut roster = Roster::default();
    let a = roster
        .create(vec!["/one/app".into(), "/two/app".into()])
        .id
        .clone();
    let other = roster.create(vec!["/else".into()]).id.clone();
    let b = roster.create(vec!["/one/app".into()]).id.clone();
    assert!(roster.get(&a).unwrap().project(Some("app")).is_err());
    assert!(roster.get(&a).unwrap().project(Some("/outside")).is_err());
    assert!(roster.get(&a).unwrap().project(None).is_err());
    assert_eq!(roster.get(&b).unwrap().project(None).unwrap(), "/one/app");
    roster.reorder(&[b.clone(), a.clone()]);
    roster.hidden_sessions.push(17);
    roster.save(&db).unwrap();
    let saved = Roster::load(&db).unwrap();
    assert_eq!(
        saved.monos.iter().map(|m| &m.id).collect::<Vec<_>>(),
        vec![&b, &other, &a]
    );
    assert_eq!(saved.hidden_sessions, vec![17]);
    assert_ne!(saved.monos[0].mascot, saved.monos[1].mascot);
}
#[test]
fn memory_supersedes_uniquely_redacts_and_archives_before_prompt_budget() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let id = "memory-test";
    let secret = "sk-1234567890abcdefghijklmnop";
    let entry = memory::entry(&format!("Use Rust. token={secret}"), "2026-10-19", None).unwrap();
    assert!(!entry.contains(secret));
    let first = memory::add("# User notes\n", &entry);
    assert_eq!(memory::add(&first, &entry), first);
    let new = memory::entry("Use GPUI", "2026-10-19", None).unwrap();
    let replaced = memory::replace(&first, "Use Rust", Some(&new), "2026-10-19").unwrap();
    assert!(replaced.contains("~~"));
    assert!(memory::replace(&replaced, "not present", None, "2026-10-19").is_err());
    let mut many = "# User notes\nDo not drop this user-written line.\n".to_string();
    for n in 0..205 {
        many.push_str(&memory::entry(&format!("fact number {n}"), "2026-10-18", None).unwrap());
        many.push('\n');
    }
    memory::edit(root, id, None, |_| Ok(many.clone())).unwrap();
    let file = memory::read(root, id, None).unwrap().text.unwrap();
    assert!(file.lines().count() <= 200);
    assert!(file.contains("Do not drop"));
    assert!(file.contains("fact number 204"));
    assert!(files::read(root, id, None, "memory/archive.md")
        .unwrap()
        .text
        .unwrap()
        .contains("fact number 0"));
    assert_eq!(
        memory::search(root, id, "fact number 204", None)
            .unwrap()
            .len(),
        1
    );
    assert!(memory::topic("../secret").is_err());
}
#[test]
fn habit_updates_retry_conflicts_without_losing_other_edits() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let id = "habit-test";
    let now = 1_792_411_200_000;
    let a = Habit::new(
        "A".into(),
        "Report changes".into(),
        Schedule::default(),
        now,
    )
    .unwrap();
    let b = Habit::new("B".into(), "Check builds".into(), Schedule::default(), now).unwrap();
    let inject = AtomicBool::new(true);
    habits::update(root, id, |all| {
        if inject.swap(false, Ordering::SeqCst) {
            habits::update(root, id, |other| {
                other.push(b.clone());
                Ok(())
            })?;
        }
        all.push(a.clone());
        Ok(())
    })
    .unwrap();
    let all = habits::load(root, id).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, b.id);
    assert_eq!(all[1].id, a.id);
    let mut h = a;
    h.enabled = false;
    h.next_run_at = now;
    assert!(!h.due(now));
    h.run_requested = true;
    assert!(h.due(now));
    h.running_since = Some(now);
    assert!(!h.due(now));
    assert!(habits::report("`NOTHING_TO_REPORT`").is_none());
    assert!(habits::report("The build failed.\nNOTHING_TO_REPORT")
        .unwrap()
        .contains("build failed"));
}
#[test]
fn schedules_are_strictly_future_and_reject_invalid_fields() {
    let now = 1_792_411_200_000;
    for kind in ["hourly", "daily", "weekdays", "weekly"] {
        let s = Schedule {
            schedule_kind: kind.into(),
            ..Default::default()
        };
        let next = s.next(now).unwrap();
        assert!(next > now);
        assert!(next - now <= 8 * 86400000);
        assert!(s.next(next).unwrap() > next);
    }
    assert!(Schedule {
        time: "25:00".into(),
        ..Default::default()
    }
    .validate()
    .is_err());
    assert!(Schedule {
        minute: 60,
        ..Default::default()
    }
    .validate()
    .is_err());
    let s: Schedule = serde_json::from_str(r#"{"kind":"hourly","minute":15}"#).unwrap();
    assert_eq!(s.minute, 15);
}
#[test]
fn delivery_receipt_is_saved_with_the_user_message() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("native.sqlite");
    let mut db = Store::open(&path).unwrap();
    let session = db.create("codex", "/project").unwrap();
    let row = Row {
        role: "user".into(),
        text: "hello".into(),
        first: true,
        sent_at: None,
    };
    db.write_rows_with_message(session.id, 0, std::slice::from_ref(&row), Some("message-1"))
        .unwrap();
    drop(db);
    let db = Store::open(&path).unwrap();
    assert_eq!(db.load(session.id).unwrap().rows, vec![row]);
    assert_eq!(
        db.setting(&format!("mono-delivered:{}:message-1", session.id))
            .unwrap()
            .as_deref(),
        Some("1")
    );
}

#[test]
fn the_context_states_the_session_project_for_a_multi_project_mono_and_the_plan_format_when_on() {
    let root = std::env::temp_dir().join(format!("mono-context-{}", new_id()));
    std::fs::create_dir_all(&root).unwrap();
    let files = files::load(&root, "m", None).unwrap();
    let mut mono = Mono {
        projects: vec!["/code/a".into(), "/code/b".into()],
        ..Default::default()
    };
    let exe = Path::new("/app/monocode-gpui");
    let quick = context_with(&mono, &files, exe, Some(Path::new("/code/a")), false);
    assert!(quick.contains("Your provider session runs in /code/a"));
    assert!(quick.contains("1. Answer directly"));
    assert!(!quick.contains("3. Durable task plan"));
    assert!(!quick.contains("<monocode_orchestration>"));
    let durable = context_with(&mono, &files, exe, Some(Path::new("/code/a")), true);
    assert!(durable.contains("3. Durable task plan"));
    assert!(durable.contains("Plan format (route 3):"));
    assert!(durable.contains("<monocode_orchestration>"));
    assert!(!durable.contains("Prepare an orchestration plan for the user's request"));
    // One project: nothing to disambiguate.
    mono.projects.truncate(1);
    let single = context_with(&mono, &files, exe, Some(Path::new("/code/a")), false);
    assert!(!single.contains("Your provider session runs in"));
    assert_eq!(
        context(&mono, &files, exe),
        context_with(&mono, &files, exe, None, false)
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_plan_runs_in_the_one_assigned_project_its_assignments_name() {
    use crate::orchestration::{Plan, Task};
    let task = |id: &str, repository: Option<&str>| Task {
        id: id.into(),
        repository: repository.map(str::to_owned),
        ..Default::default()
    };
    let plan = |tasks: Vec<Task>| Plan {
        title: "P".into(),
        summary: String::new(),
        tasks,
    };
    let mut mono = Mono {
        projects: vec!["/code/app".into()],
        ..Default::default()
    };
    assert_eq!(
        mono.plan_project(&plan(vec![task("a", None)])).unwrap(),
        "/code/app"
    );
    mono.projects.push("/code/lib".into());
    let by_path = plan(vec![task("a", Some("/code/lib")), task("b", Some("lib"))]);
    assert_eq!(mono.plan_project(&by_path).unwrap(), "/code/lib");
    let inside = plan(vec![task("a", Some("/code/app/packages/web"))]);
    assert_eq!(mono.plan_project(&inside).unwrap(), "/code/app");
    let missing = mono.plan_project(&plan(vec![task("a", None)])).unwrap_err();
    assert!(
        missing.contains("give assignment a a repository"),
        "{missing}"
    );
    let foreign = mono
        .plan_project(&plan(vec![task("a", Some("/elsewhere"))]))
        .unwrap_err();
    assert!(
        foreign.contains("not one of this Mono's projects"),
        "{foreign}"
    );
    let split = plan(vec![task("a", Some("app")), task("b", Some("lib"))]);
    assert!(mono
        .plan_project(&split)
        .unwrap_err()
        .contains("one plan per project"));
    mono.projects.clear();
    assert!(mono.plan_project(&by_path).is_err());
}
