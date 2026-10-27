use super::*;
use gpui::{
    Bounds, TestAppContext, VisualTestContext, WindowBounds, WindowHandle, WindowOptions, point,
    size,
};

fn window(section: Section, cx: &mut TestAppContext) -> WindowHandle<SettingsView> {
    let window = cx.update(|cx| {
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(1200.), px(800.)),
                ))),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|cx| {
                    let mut view = SettingsView::new(
                        section,
                        Preferences::default(),
                        SettingsContext::default(),
                        cx,
                    );
                    view.discovery_task = None;
                    view.discovery_loading = false;
                    view
                })
            },
        )
        .unwrap()
    });
    cx.refresh().unwrap();
    cx.run_until_parked();
    window
}

#[gpui::test]
fn workspace_style_is_only_offered_under_the_new_layout(cx: &mut TestAppContext) {
    let window = window(Section::Appearance, cx);
    let visual = &mut VisualTestContext::from_window(window.into(), cx);
    for classic in [false, true, false] {
        window
            .update(visual, |view, _, cx| {
                view.ctx.classic = classic;
                view.ctx.new_layout_boxy = true;
                cx.notify();
            })
            .unwrap();
        visual.run_until_parked();
        assert_eq!(
            visual
                .debug_bounds("settings-new-layout-style-boxy")
                .is_some(),
            !classic
        );
        assert_eq!(
            visual
                .debug_bounds("settings-new-layout-style-rounded")
                .is_some(),
            !classic
        );
        assert!(
            visual
                .debug_bounds("settings-collapsed-project-rail-hidden")
                .is_none()
        );
    }
}

#[gpui::test]
fn skills_document_has_its_own_full_height_pane_and_compact_actions(cx: &mut TestAppContext) {
    let window = window(Section::Skills, cx);
    window
        .update(cx, |view, _, cx| {
            let path = std::path::PathBuf::from("/project/.agents/skills/review/SKILL.md");
            view.discovery.skills = vec![data::Skill {
                name: "review".into(),
                description: "Review changes".into(),
                path: path.clone(),
                scope: "project".into(),
                provider: "agents".into(),
            }];
            view.skill_preview = Some(path);
            view.skill_preview_text = Some(Ok(skill_document::DocumentData::new(format!(
                "---\nname: review\n---\n# Review\n\n{}",
                "Instructions.\n\n".repeat(150)
            ))));
            cx.notify();
        })
        .unwrap();
    let visual = &mut VisualTestContext::from_window(window.into(), cx);
    visual.run_until_parked();
    let left = visual.debug_bounds("skills-list-scroll").unwrap();
    let right = visual.debug_bounds("skill-preview-pane").unwrap();
    let body = visual.debug_bounds("skill-document").unwrap();
    assert_eq!(left.size.height, px(800.));
    assert_eq!(right.size.height, px(800.));
    assert!(right.left() >= left.right());
    assert!(body.bottom() <= right.bottom());
    assert!(body.size.height > px(600.));
    assert!(visual.debug_bounds("transcript").is_none());
    for id in [
        "skill-preview-action",
        "skill-copy-action",
        "skill-reveal-action",
    ] {
        assert_eq!(
            visual.debug_bounds(id).unwrap().size,
            size(px(20.), px(20.))
        );
    }
    let source = visual
        .debug_bounds("settings-skill-document-source")
        .unwrap();
    assert!(source.size.width < px(100.));
    visual.simulate_click(source.center(), Default::default());
    visual.run_until_parked();
    window
        .update(visual, |view, _, _| {
            assert_eq!(view.prefs.skill_source_paths.len(), 1)
        })
        .unwrap();
    visual.simulate_resize(size(px(700.), px(800.)));
    visual.run_until_parked();
    // Deliver the frame that applies the measured Skills container breakpoint.
    visual.update(|window, cx| {
        window.simulate_next_frame(cx);
    });
    visual.run_until_parked();
    let left = visual.debug_bounds("skills-list-scroll").unwrap();
    let right = visual.debug_bounds("skill-preview-pane").unwrap();
    assert!(
        right.top() >= left.bottom(),
        "list={left:?}, preview={right:?}"
    );
    assert_eq!(left.size.width, px(700.));
    assert_eq!(right.size.width, px(700.));
    assert!(visual.debug_bounds("skill-document").unwrap().size.height > px(200.));
}

#[gpui::test]
fn mono_mascot_is_beside_name_and_sidebar_switch_stays_visible(cx: &mut TestAppContext) {
    let window = window(Section::Monos, cx);
    window
        .update(cx, |view, _, cx| {
            view.ctx.monos = vec![MonoSetting {
                id: "layout-mono".into(),
                name: "Big Boss".into(),
                mascot: "ghost".into(),
                color: 0xee99bb,
                projects: vec!["monocode".into(), "mono-web".into()],
                show: false,
                folders: false,
            }];
            cx.notify();
        })
        .unwrap();
    let visual = &mut VisualTestContext::from_window(window.into(), cx);
    visual.run_until_parked();
    let mascot = visual.debug_bounds("settings-mono-mascot").unwrap();
    let switch = visual
        .debug_bounds("settings-toggle-mono-sidebar-show")
        .unwrap();
    let reset = visual.debug_bounds("settings-mono-reset").unwrap();
    assert_eq!(mascot.size, size(px(16.), px(16.)));
    assert_eq!(switch.size, size(px(36.), px(20.)));
    let name = visual.debug_bounds("settings-mono-name").unwrap();
    assert!(mascot.right() < name.left());
    assert!(mascot.left() < px(200.));
    assert!(switch.left() > px(800.));
    assert!(switch.right() <= reset.left());
    assert!(reset.right() <= px(1200.));
    assert!((mascot.top() - switch.top()).abs() < px(4.));
    assert!(
        visual
            .debug_bounds("settings-toggle-mono-sidebar-folders")
            .is_none()
    );
    visual.simulate_click(reset.center(), Default::default());
    visual.run_until_parked();
    assert!(visual.debug_bounds("settings-management-dialog").is_some());
    window
        .update(visual, |view, window, cx| {
            view.close_management(window, cx);
            cx.notify();
        })
        .unwrap();
    visual.simulate_resize(size(px(700.), px(800.)));
    visual.run_until_parked();
    let switch = visual
        .debug_bounds("settings-toggle-mono-sidebar-show")
        .unwrap();
    assert_eq!(switch.size, size(px(36.), px(20.)));
    assert!(switch.right() <= px(700.));
}

fn layout_mono() -> MonoSetting {
    MonoSetting {
        id: "layout-mono".into(),
        name: "Big Boss".into(),
        mascot: "ghost".into(),
        color: 0xee99bb,
        projects: vec!["monocode".into()],
        show: false,
        folders: false,
    }
}

#[gpui::test]
fn mono_autonomy_settings_are_absent_with_the_task_coordinator_off(cx: &mut TestAppContext) {
    let window = window(Section::Monos, cx);
    window
        .update(cx, |view, _, cx| {
            view.ctx.monos = vec![layout_mono()];
            view.ctx.tasks_enabled = false;
            cx.notify();
        })
        .unwrap();
    let visual = &mut VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(size(px(1200.), px(4000.)));
    visual.run_until_parked();
    assert!(
        visual
            .debug_bounds("settings-mono-sidebar-switch")
            .is_some()
    );
    for selector in [
        "settings-mono-autonomy-note",
        "settings-toggle-autonomy-automatic-execution",
        "settings-toggle-autonomy-push",
    ] {
        assert!(visual.debug_bounds(selector).is_none(), "{selector}");
    }
}

#[gpui::test]
fn mono_autonomy_rows_edit_each_monos_policy_through_settings_changed(cx: &mut TestAppContext) {
    use std::{cell::RefCell, rc::Rc};
    let window = window(Section::Monos, cx);
    let changes = Rc::new(RefCell::new(Vec::new()));
    let sink = changes.clone();
    window
        .update(cx, |view, _, cx| {
            view.ctx.tasks_enabled = true;
            view.ctx.monos = vec![MonoSetting {
                id: "layout-mono".into(),
                name: "Big Boss".into(),
                mascot: "ghost".into(),
                color: 0xee99bb,
                projects: vec!["monocode".into()],
                show: false,
                folders: false,
            }];
            let entity = cx.entity();
            cx.subscribe(&entity, move |_, _, event: &SettingsEvent, _| {
                if let SettingsEvent::Changed(prefs) = event {
                    sink.borrow_mut().push(*prefs.clone());
                }
            })
            .detach();
            cx.notify();
        })
        .unwrap();
    let visual = &mut VisualTestContext::from_window(window.into(), cx);
    // Tall enough that every row is on screen without scrolling.
    visual.simulate_resize(size(px(1200.), px(4000.)));
    visual.run_until_parked();
    assert!(visual.debug_bounds("settings-mono-autonomy-note").is_some());
    let last = |changes: &Rc<RefCell<Vec<Preferences>>>| {
        changes
            .borrow()
            .last()
            .expect("a change was emitted")
            .mono_policy("layout-mono")
            .policy
    };
    for (selector, check) in [
        (
            "settings-toggle-autonomy-automatic-execution",
            (|p| p.automatic_execution) as fn(&monocode_core::tasks::autonomy::Autonomy) -> bool,
        ),
        ("settings-autonomy-workers-3", |p| p.worker_limit == 3),
        ("settings-toggle-autonomy-push", |p| p.push),
        ("settings-autonomy-workspace-checkout", |p| {
            p.workspace == monocode_core::tasks::autonomy::WorkspacePreference::Checkout
        }),
    ] {
        let bounds = visual
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is not on screen"));
        visual.simulate_click(bounds.center(), Default::default());
        visual.run_until_parked();
        assert!(check(&last(&changes)), "{selector}");
    }
    // Earlier edits are kept: each change builds on the saved policy.
    let policy = last(&changes);
    assert!(policy.automatic_execution && policy.push);
    assert_eq!(policy.worker_limit, 3);
}

#[gpui::test]
fn providers_page_saves_clears_and_enables_api_keys(cx: &mut TestAppContext) {
    use monocode_core::{api_keys, provider::Kind};
    use std::{cell::RefCell, rc::Rc};
    api_keys::init(std::env::temp_dir().join(format!("monocode-api-keys-{}", std::process::id())));
    api_keys::clear(Kind::Claude).unwrap();
    let window = window(Section::Providers, cx);
    let changes = Rc::new(RefCell::new(Vec::new()));
    let sink = changes.clone();
    window
        .update(cx, |_, _, cx| {
            let view = cx.entity();
            cx.subscribe(&view, move |_, _, event: &SettingsEvent, _| {
                if let SettingsEvent::Changed(prefs) = event {
                    sink.borrow_mut().push(*prefs.clone());
                }
            })
            .detach();
        })
        .unwrap();
    let visual = &mut VisualTestContext::from_window(window.into(), cx);
    visual.refresh().unwrap();
    assert!(
        visual
            .debug_bounds("settings-api-key-claude-input")
            .is_some()
    );
    assert!(
        visual
            .debug_bounds("settings-api-key-claude-clear")
            .is_none(),
        "nothing to clear yet"
    );

    window
        .update(visual, |view, _, cx| {
            let input = view.api_key_input(Kind::Claude, cx);
            input.update(cx, |input, cx| {
                input.set_text("sk-ant-test-9876".into(), cx)
            });
        })
        .unwrap();
    visual.refresh().unwrap();
    let save = visual.debug_bounds("settings-api-key-claude-save").unwrap();
    visual.simulate_click(save.center(), Default::default());
    visual.run_until_parked();
    assert_eq!(
        api_keys::status(Kind::Claude).unwrap().hint.as_deref(),
        Some("9876")
    );
    window
        .update(visual, |view, _, cx| {
            assert_eq!(
                view.api_key_input(Kind::Claude, cx).read(cx).content,
                "",
                "the field clears after saving"
            );
        })
        .unwrap();

    let toggle = visual
        .debug_bounds("settings-toggle-api-key-claude")
        .unwrap();
    visual.simulate_click(toggle.center(), Default::default());
    visual.run_until_parked();
    assert!(
        changes
            .borrow()
            .last()
            .unwrap()
            .api_key_enabled(Kind::Claude)
    );

    let clear = visual
        .debug_bounds("settings-api-key-claude-clear")
        .unwrap();
    visual.simulate_click(clear.center(), Default::default());
    visual.run_until_parked();
    assert!(!api_keys::status(Kind::Claude).unwrap().configured);
    assert!(
        !changes
            .borrow()
            .last()
            .unwrap()
            .api_key_enabled(Kind::Claude),
        "clearing the key switches billing back to the login"
    );
}
