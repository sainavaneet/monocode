//! The Mono's Autonomy settings: editing the policy reaches the preferences
//! and the coordinator's live policy, a project can override it, the account
//! fallback list is the live allowlist, and the task list applies the policy
//! to a task or unblocks it. Mono's own commands reach the same tasks.
use super::tests::{Setup, call, setup};
use super::*;
use gpui::{TestAppContext, VisualTestContext};
use monocode_core::accounts::eligibility::{self, Scope};
use monocode_core::tasks::autonomy::{self, Autonomy, WorkspacePreference};
use monocode_core::tasks::{Requester, State, Subject, TaskLifecycle, coordinator::FLAG, policy};
use serde_json::json;

/// Task `t` owned by the setup's Mono, running, with the default policy
/// snapshotted into it.
fn task(s: &Setup) {
    let db = locked(&s.store).unwrap();
    db.save_setting(FLAG, "enabled").unwrap();
    monocode_core::tasks::fixtures::running_worker(&db, &s.id, &s.project).unwrap();
    let task = db.tasks().unwrap().task("t").unwrap().unwrap();
    policy::capture_with(&db, &task, "codex", &Autonomy::default(), 1).unwrap();
}

fn open_autonomy(s: &Setup, cx: &mut TestAppContext) -> VisualTestContext {
    cx.update(|cx| cx.set_reduce_motion(true));
    s.window
        .update(cx, |_, _, cx| {
            s.manager.update(cx, |m, cx| {
                m.settings = Some(SettingsSection::Autonomy);
                m.refresh_autonomy(cx);
                cx.notify();
            })
        })
        .unwrap();
    let visual = VisualTestContext::from_window(s.window.into(), cx);
    visual.run_until_parked();
    visual
}

/// Scrolls the page until `selector` is inside it, then clicks it.
fn click(visual: &mut VisualTestContext, selector: &'static str) {
    for _ in 0..40 {
        let target = visual
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is not on screen"));
        let page = visual.debug_bounds("mono-autonomy-page").unwrap();
        let y = target.center().y;
        if y > page.top() + px(24.) && y < page.bottom() - px(24.) {
            visual.simulate_click(target.center(), Default::default());
            visual.run_until_parked();
            return;
        }
        let step = if y > page.center().y { -120. } else { 120. };
        visual.simulate_event(gpui::ScrollWheelEvent {
            position: page.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(step))),
            modifiers: Default::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        visual.run_until_parked();
    }
    panic!("{selector} never scrolled into view");
}

fn saved(s: &Setup, visual: &mut VisualTestContext) -> autonomy::MonoAutonomy {
    s.window
        .update(visual, |shell, _, _| {
            shell.workspace.preferences.mono_policy(&s.id)
        })
        .unwrap()
}

fn lifecycle(s: &Setup) -> TaskLifecycle {
    let db = locked(&s.store).unwrap();
    db.tasks().unwrap().task("t").unwrap().unwrap().lifecycle
}

#[gpui::test]
fn the_autonomy_section_edits_the_policy_applies_it_and_unblocks_a_task(cx: &mut TestAppContext) {
    let before = monocode_core::provider::test_guard::refused();
    let s = setup(cx);
    task(&s);
    let visual = &mut open_autonomy(&s, cx);
    for selector in [
        "mono-autonomy-page",
        "mono-autonomy-spending-note",
        "mono-autonomy-task-t",
        "mono-autonomy-workers-2",
    ] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector}");
    }
    assert!(visual.debug_bounds("mono-autonomy-unblock-t").is_none());

    // Granting push saves the preference and reaches the live policy, but
    // not the running task.
    click(visual, "mono-autonomy-push");
    click(visual, "mono-autonomy-workers-2");
    let policy = saved(&s, visual);
    assert!(policy.policy.push);
    assert_eq!(policy.policy.worker_limit, 2);
    assert!(autonomy::live(Some(&s.id), &s.project).push);
    let snapshot = || {
        let db = locked(&s.store).unwrap();
        policy::of(&db.tasks().unwrap(), "t").unwrap().unwrap()
    };
    assert!(!snapshot().autonomy.push);

    // Apply takes it into the task with a revision bump.
    click(visual, "mono-autonomy-apply-t");
    let applied = snapshot();
    assert_eq!(applied.revision, 1);
    assert!(applied.autonomy.push);
    assert_eq!(applied.autonomy.worker_limit, 2);

    // A project override, then back to the Mono's policy.
    click(visual, "mono-autonomy-scope-0");
    click(visual, "mono-autonomy-workspace-worktree");
    let policy = saved(&s, visual);
    assert!(policy.has_override(&s.project));
    assert_eq!(
        policy.for_project(&s.project).workspace,
        WorkspacePreference::Worktree
    );
    assert_eq!(policy.policy.workspace, WorkspacePreference::Automatic);
    click(visual, "mono-autonomy-use-mono");
    assert!(!saved(&s, visual).has_override(&s.project));

    // The account fallback list is the live allowlist, saved per Mono.
    click(visual, "mono-autonomy-scope-all");
    click(visual, "mono-autonomy-add-codex-default");
    let listed = {
        let db = locked(&s.store).unwrap();
        eligibility::allowlist(&db, Scope::Mono(&s.id)).unwrap()
    };
    let listed = listed.expect("the Mono's allowlist is saved");
    assert_eq!(listed.accounts.len(), 1);
    assert_eq!(listed.accounts[0].account_id, "default");
    click(visual, "mono-autonomy-remove-0");
    let db_list = {
        let db = locked(&s.store).unwrap();
        eligibility::allowlist(&db, Scope::Mono(&s.id)).unwrap()
    };
    assert_eq!(db_list, None, "an empty list falls back to the default");

    // A blocked task offers Unblock, which goes through the table.
    {
        let db = locked(&s.store).unwrap();
        let tasks = db.tasks().unwrap();
        let t = Subject::Task("t");
        let blocked = State::Lifecycle(TaskLifecycle::Blocked);
        let revision = tasks.revision(t).unwrap();
        tasks
            .transition(t, blocked, Requester::Coordinator, revision, 2)
            .unwrap();
    }
    s.manager.update(visual, |m, cx| m.refresh_autonomy(cx));
    visual.run_until_parked();
    click(visual, "mono-autonomy-unblock-t");
    assert_eq!(lifecycle(&s), TaskLifecycle::Active);

    // Mono's own command sees the applied policy.
    let shown = call(
        &s,
        "policy-1",
        "tasks.policy.show",
        json!({"taskId": "t"}),
        visual,
    )
    .recv()
    .unwrap()
    .unwrap();
    assert_eq!(shown["revision"], 1);
    assert_eq!(shown["effective"]["push"], true);

    // Leave the process-wide policy as other tests expect it.
    autonomy::publish(Default::default());
    assert_eq!(monocode_core::provider::test_guard::refused(), before);
}
