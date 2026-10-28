use super::tests::setup;
use super::*;
use gpui::{TestAppContext, VisualTestContext};
use monocode_core::tasks::{coordinator::FLAG, policy};
use monocode_core::transcript::Row;

#[gpui::test]
fn a_mono_reply_renders_a_plan_and_start_persists_its_task(cx: &mut TestAppContext) {
    let s = setup(cx);
    locked(&s.store)
        .unwrap()
        .save_setting(FLAG, "enabled")
        .unwrap();
    s.window
        .update(cx, |_, window, cx| {
            s.manager.update(cx, |m, cx| {
                m.settings = None;
                m.open(&s.id, window, cx);
            });
        })
        .unwrap();
    cx.run_until_parked();
    let chat = cx.update(|cx| s.manager.read(cx).chats[&s.id].clone());
    cx.update(|cx| chat.update(cx, |v, cx| {
        let text = r#"Ready. <monocode_orchestration>{"title":"Demo","summary":"Write docs","tasks":[{"id":"docs","title":"Write docs","prompt":"Create DEMO.md","files":["DEMO.md"],"verification":["test -s DEMO.md"],"workspace":"worktree"}]}</monocode_orchestration>"#;
        v.transcript.update(cx, |t, cx| t.patch(0, vec![Row {
            role: "assistant".into(), text: text.into(), first: true, sent_at: None,
        }], cx));
        v.finish_orchestration_plan(cx);
        assert!(v.orchestration_plan.is_some());
        cx.notify();
    }));
    let visual = &mut VisualTestContext::from_window(s.window.into(), cx);
    visual.run_until_parked();
    visual.refresh().unwrap();
    assert!(visual.debug_bounds("orchestration-card").is_some());
    let start = visual.debug_bounds("start-orchestration").unwrap();
    visual.simulate_click(start.center(), Default::default());
    visual.run_until_parked();
    let db = locked(&s.store).unwrap();
    let tasks = policy::started_plans(&db, s.session).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].mono_id.as_deref(), Some(s.id.as_str()));
    assert_eq!(tasks[0].project, s.project);
    assert_eq!(tasks[0].criteria.len(), 1);
    drop(db);
    chat.read_with(visual, |v, _| assert!(v.orchestration_plan.is_none()));
    chat.update(visual, |v, cx| {
        v.transcript.update(cx, |t, cx| {
            t.patch(
                1,
                vec![
                    Row {
                        role: "mono-system".into(),
                        text: "Worker needs recovery".into(),
                        first: true,
                        sent_at: None,
                    },
                    Row {
                        role: "assistant".into(),
                        text: "I will inspect the failure.".into(),
                        first: true,
                        sent_at: None,
                    },
                ],
                cx,
            );
        });
        v.finish_orchestration_plan(cx);
        assert!(
            v.orchestration_plan.is_none(),
            "an inbox reply must not resurrect the earlier plan"
        );
    });
}

#[gpui::test]
fn mono_activity_tracks_only_its_outstanding_handoffs(cx: &mut TestAppContext) {
    let s = setup(cx);
    s.window
        .update(cx, |_, window, cx| {
            s.manager.update(cx, |m, cx| {
                m.settings = None;
                m.open(&s.id, window, cx);
            });
        })
        .unwrap();
    cx.run_until_parked();
    let chat = cx.update(|cx| s.manager.read(cx).chats[&s.id].clone());
    cx.update(|cx| {
        let source = chat.read(cx).summary.id;
        s.manager.update(cx, |m, cx| {
            m.roster
                .get_mut(&s.id)
                .unwrap()
                .launches
                .push(monocode_core::mono::Launch {
                    session_id: 987654,
                    cwd: s.project.clone(),
                    title: "Neon Pop review".into(),
                    harness: "claude".into(),
                    model: Some("Sonnet".into()),
                    origin_session: source,
                    origin_turn: 1,
                    notify_on_complete: true,
                    completed: false,
                    dismissed: false,
                    input_notified: false,
                });
            m.refresh_cards(cx);
        });
        assert_eq!(
            chat.read(cx).mono_status(),
            "needs-you",
            "missing worker must not appear idle"
        );
    });
    let visual = &mut VisualTestContext::from_window(s.window.into(), cx);
    visual.run_until_parked();
    visual.refresh().unwrap();
    assert!(visual.debug_bounds("mono-activity-strip").is_some());
    for mode in ["completed", "dismissed", "other-chat"] {
        s.manager.update(visual, |m, cx| {
            let launch = m
                .roster
                .get_mut(&s.id)
                .unwrap()
                .launches
                .last_mut()
                .unwrap();
            launch.completed = mode == "completed";
            launch.dismissed = mode == "dismissed";
            if mode == "other-chat" {
                launch.origin_session = 1;
            }
            m.refresh_cards(cx);
        });
        chat.read_with(visual, |v, _| assert_eq!(v.mono_status(), "idle", "{mode}"));
    }
}

#[gpui::test]
fn changing_a_running_monos_project_exposes_restart_required(cx: &mut TestAppContext) {
    let s = setup(cx);
    s.window
        .update(cx, |_, window, cx| {
            s.manager.update(cx, |m, cx| {
                m.settings = None;
                m.open(&s.id, window, cx);
            });
        })
        .unwrap();
    cx.run_until_parked();
    let chat = cx.update(|cx| s.manager.read(cx).chats[&s.id].clone());
    cx.update(|cx| {
        chat.update(cx, |v, _| v.busy = true);
        s.manager.update(cx, |m, cx| {
            m.roster.get_mut(&s.id).unwrap().projects =
                vec![std::env::temp_dir().display().to_string()];
            m.configure_chat(&s.id, cx);
        });
        assert_eq!(chat.read(cx).mono_status(), "needs-you");
        chat.update(cx, |v, _| v.busy = false);
        assert_eq!(chat.read(cx).mono_status(), "idle");
    });
}

#[gpui::test]
fn invalid_workspace_proposal_offers_repair_without_starting(cx: &mut TestAppContext) {
    let s = setup(cx);
    locked(&s.store)
        .unwrap()
        .save_setting(FLAG, "enabled")
        .unwrap();
    s.manager.update(cx, |m, cx| {
        m.settings = None;
        cx.notify();
    });
    let chat = s.manager.read_with(cx, |m, _| m.chats[&s.id].clone());
    chat.update(cx, |v, cx| {
        let text = r#"<monocode_orchestration>{"title":"Demo","summary":"","tasks":[{"id":"a","title":"A","prompt":"Use base pipeline","files":["src"],"workspace":"worktree feat/welder-spacing (base pipeline)"}]}</monocode_orchestration>"#;
        v.transcript.update(cx, |t, cx| t.patch(0, vec![Row { role: "assistant".into(), text: text.into(), first: true, sent_at: None }], cx));
        v.finish_orchestration_plan(cx);
        assert!(v.orchestration_plan.is_none());
        assert!(v.orchestration_error.as_ref().unwrap().contains("Assignment 1"));
        cx.notify();
    });
    let visual = &mut VisualTestContext::from_window(s.window.into(), cx);
    visual.run_until_parked();
    visual.refresh().unwrap();
    assert!(visual.debug_bounds("repair-plan-error").is_some());
    assert!(visual.debug_bounds("start-orchestration").is_none());
    assert!(
        policy::started_plans(&locked(&s.store).unwrap(), s.session)
            .unwrap()
            .is_empty()
    );
}

#[gpui::test]
fn validated_proposal_survives_recreation_and_dismissal_stays_dismissed(cx: &mut TestAppContext) {
    let s = setup(cx);
    locked(&s.store)
        .unwrap()
        .save_setting(FLAG, "enabled")
        .unwrap();
    let chat = s.manager.read_with(cx, |m, _| m.chats[&s.id].clone());
    let summary = chat.update(cx, |v, cx| {
        let text = r#"<monocode_orchestration>{"title":"Saved work","summary":"Existing findings","tasks":[{"id":"a","title":"A","prompt":"Keep base pipeline and prior findings","files":["src"],"workspace":"worktree"}]}</monocode_orchestration>"#;
        v.transcript.update(cx, |t, cx| t.patch(0, vec![Row { role: "assistant".into(), text: text.into(), first: true, sent_at: None }], cx));
        v.finish_orchestration_plan(cx);
        assert!(v.orchestration_plan.is_some());
        v.summary.clone()
    });
    let restored = cx.new(|cx| Conversation::new(summary.clone(), s.store.clone(), cx));
    restored.update(cx, |v, _| {
        v.restore_saved_proposal();
        let plan = v.orchestration_plan.as_ref().unwrap();
        assert_eq!(plan.summary, "Existing findings");
        assert_eq!(
            plan.tasks[0].prompt,
            "Keep base pipeline and prior findings"
        );
        assert!(!v.busy);
        v.clear_saved_proposal();
    });
    let reopened = cx.new(|cx| Conversation::new(summary, s.store.clone(), cx));
    reopened.update(cx, |v, _| {
        v.restore_saved_proposal();
        assert!(v.orchestration_plan.is_none());
    });
    assert!(
        policy::started_plans(&locked(&s.store).unwrap(), s.session)
            .unwrap()
            .is_empty()
    );
}

#[gpui::test]
fn long_completion_failures_leave_the_mono_composer_visible(cx: &mut TestAppContext) {
    let s = setup(cx);
    s.window
        .update(cx, |_, window, cx| {
            s.manager.update(cx, |m, cx| {
                m.settings = None;
                m.open(&s.id, window, cx);
            });
        })
        .unwrap();
    cx.run_until_parked();
    let chat = cx.update(|cx| s.manager.read(cx).chats[&s.id].clone());
    chat.update(cx, |v, cx| {
        v.busy = true;
        let mut context = v.settings.mono.clone().unwrap();
        context.mono.outbox = (0..12)
            .map(|i| monocode_core::mono::Message {
                id: format!("long-completion-{i}"),
                text: "Worker finished".into(),
                internal: true,
                error: Some(format!(
                    "Claude Code reached its usage limit.\n{}",
                    "Previous worker transcript\n".repeat(1000)
                )),
                sent_at: None,
                attachments: Vec::new(),
            })
            .collect();
        v.configure_mono(
            context.mono,
            context.root,
            v.settings.environment.clone(),
            cx,
        );
        cx.notify();
    });
    let visual = &mut VisualTestContext::from_window(s.window.into(), cx);
    visual.run_until_parked();
    visual.refresh().unwrap();
    let updates = visual.debug_bounds("mono-completion-updates").unwrap();
    assert!(updates.size.height <= px(160.));
    let stop = visual
        .debug_bounds("mono-stop")
        .expect("composer controls remain visible");
    assert!(stop.top() >= updates.bottom());
    let viewport = s
        .window
        .update(visual, |_, window, _| window.viewport_size())
        .unwrap();
    assert!(stop.bottom() <= viewport.height);
    chat.read_with(visual, |v, _| {
        assert!(
            v.settings.mono.as_ref().unwrap().mono.outbox[0]
                .error
                .as_ref()
                .unwrap()
                .len()
                > 20_000,
            "presentation must preserve the original error and queued result"
        );
    });
}

#[gpui::test]
fn mono_stop_stays_available_with_a_draft_and_stops_the_reply(cx: &mut TestAppContext) {
    let s = setup(cx);
    s.window
        .update(cx, |_, window, cx| {
            s.manager.update(cx, |m, cx| {
                m.settings = None;
                m.open(&s.id, window, cx);
            });
        })
        .unwrap();
    cx.run_until_parked();
    let chat = cx.update(|cx| s.manager.read(cx).chats[&s.id].clone());
    chat.update(cx, |v, cx| {
        v.busy = true;
        v.input.update(cx, |input, cx| {
            input.set_text("Keep my follow-up".into(), cx)
        });
        cx.notify();
    });
    let visual = &mut VisualTestContext::from_window(s.window.into(), cx);
    visual.run_until_parked();
    visual.refresh().unwrap();
    let stop = visual
        .debug_bounds("mono-stop")
        .expect("Mono must expose Stop while typing");
    visual.simulate_click(stop.center(), Default::default());
    visual.run_until_parked();
    chat.read_with(visual, |v, cx| {
        assert!(!v.responding());
        assert_eq!(v.input.read(cx).content, "Keep my follow-up");
    });
    visual.refresh().unwrap();
    assert!(
        visual.debug_bounds("mono-stop").is_some(),
        "show cancellation in progress until the provider finishes"
    );
}

/// A quick worker (sessions.start) that stopped reports to its Mono once,
/// with the app's outcome and the end of its transcript; a session the
/// window has not loaded yet is still waited for, and a deleted one is not.
#[gpui::test]
fn a_stopped_quick_worker_reports_once_with_its_outcome(cx: &mut TestAppContext) {
    let s = setup(cx);
    s.window
        .update(cx, |_, window, cx| {
            s.manager.update(cx, |m, cx| {
                m.settings = None;
                m.open(&s.id, window, cx);
            });
        })
        .unwrap();
    cx.run_until_parked();
    let worker = {
        let mut db = locked(&s.store).unwrap();
        let worker = db.create("codex", &s.project).unwrap().id;
        let row = |role: &str, text: &str| Row {
            role: role.into(),
            text: text.into(),
            first: true,
            sent_at: None,
        };
        let long = "narration ".repeat(2_000);
        db.write_rows(
            worker,
            0,
            &[
                row("user", "Fix the parser"),
                row("assistant", &long),
                row("assistant", "FINAL: parser fixed, tests pass"),
            ],
        )
        .unwrap();
        worker
    };
    let gone = 424242;
    let source = s.session;
    s.manager.update(cx, |m, _| {
        let launches = &mut m.roster.get_mut(&s.id).unwrap().launches;
        for (session_id, title) in [(worker, "Parser fix"), (gone, "Removed")] {
            launches.push(monocode_core::mono::Launch {
                session_id,
                cwd: s.project.clone(),
                title: title.into(),
                harness: "codex".into(),
                model: None,
                origin_session: source,
                origin_turn: 3,
                notify_on_complete: true,
                completed: false,
                dismissed: false,
                input_notified: false,
            });
        }
    });
    // Not loaded yet: nothing is reported.
    s.manager.update(cx, |m, cx| m.check_completions(cx));
    cx.run_until_parked();
    let reports = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            let m = s.manager.read(cx);
            m.roster
                .get(&s.id)
                .unwrap()
                .outbox
                .iter()
                .filter(|q| q.internal && q.text.starts_with("[Delegated work completed]"))
                .map(|q| q.text.clone())
                .collect::<Vec<_>>()
        })
    };
    assert!(
        reports(cx).is_empty(),
        "unloaded sessions are still waited for"
    );
    // The check asked the window to load them: now the stopped worker and
    // the removed session both count as over.
    s.manager.update(cx, |m, cx| m.check_completions(cx));
    cx.run_until_parked();
    let sent = reports(cx);
    assert_eq!(sent.len(), 1, "{sent:?}");
    let report = &sent[0];
    assert!(
        report.contains(&format!("Session {worker}: Parser fix\nOutcome: finished")),
        "{report}"
    );
    assert!(
        report.contains("FINAL: parser fixed, tests pass"),
        "the end of a long transcript survives"
    );
    assert!(report.contains("characters omitted"), "{report}");
    assert!(
        report.contains(&format!(
            "Session {gone}: Removed\nOutcome: session not found"
        )),
        "{report}"
    );
    cx.update(|cx| {
        let m = s.manager.read(cx);
        assert!(
            m.roster
                .get(&s.id)
                .unwrap()
                .launches
                .iter()
                .all(|l| l.completed)
        );
    });
}
