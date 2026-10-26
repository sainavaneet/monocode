//! The process's one task coordinator (`monocode_core::tasks::coordinator`).
//! It ticks off the main thread and does nothing unless the
//! `native-task-coordinator` setting is `enabled`. Sessions it reserves are
//! handed to the Shell, which installs and submits them like `sessions.start`,
//! and the result is reported back: confirmed (attempt Running) or failed.
use super::{Database, locked};
use crate::shell::Shell;
use gpui::{App, Global, WeakEntity};
use monocode_core::agent::Settings;
use monocode_core::store::Session;
use monocode_core::tasks;
use monocode_core::tasks::coordinator::{Coordinator, ExplicitStart, Launcher, TICK_INTERVAL_MS};
use std::time::Duration;

/// Where coordinator launches are installed; the latest Shell to start its
/// app bridge.
struct Service {
    shell: WeakEntity<Shell>,
}
impl Global for Service {}

/// Hands reserved sessions to the main thread.
struct ShellLauncher(async_channel::Sender<(Session, Settings)>);
impl Launcher for ShellLauncher {
    fn start(&self, session: &Session, settings: &Settings) -> Result<(), String> {
        self.0
            .try_send((session.clone(), settings.clone()))
            .map_err(|_| "The app is closing; the worker was not started.".into())
    }
}

/// A name for this process in task leases; never shared with another process.
fn holder() -> String {
    format!(
        "monocode-native:{}:{}",
        std::process::id(),
        monocode_core::mono::new_id()
    )
}

/// Registers the coordinator once per process; later calls only retarget the
/// Shell that installs launched workers.
pub(crate) fn install(store: Database, shell: WeakEntity<Shell>, cx: &mut App) {
    if cx.has_global::<Service>() {
        cx.set_global(Service { shell });
        return;
    }
    cx.set_global(Service { shell });
    let (launches, receive) = async_channel::unbounded::<(Session, Settings)>();
    let confirmations = store.clone();
    cx.spawn(async move |cx| {
        while let Ok((session, settings)) = receive.recv().await {
            let id = session.id;
            let installed = cx.update(|cx| {
                let shell = cx.try_global::<Service>().and_then(|s| s.shell.upgrade());
                match shell {
                    Some(shell) => {
                        shell.update(cx, |shell, cx| {
                            shell.install_task_launch(session, settings, cx)
                        });
                        Ok(())
                    }
                    None => Err("No Monocode window was open to start the worker in."),
                }
            });
            // The attempt runs only once the app confirms it; a failure is
            // recorded on the attempt rather than left Starting.
            let store = confirmations.clone();
            let recorded = cx
                .background_executor()
                .spawn(async move {
                    let store = locked(&store)?;
                    let now = monocode_core::time::now_ms();
                    match installed {
                        Ok(()) => tasks::confirm_started(&store, id, now).map(|_| ()),
                        Err(reason) => tasks::start_failed(&store, id, reason, now),
                    }
                    .map_err(|e| e.to_string())
                })
                .await;
            if let Err(error) = recorded {
                eprintln!("Task coordinator: could not record worker {id}'s start: {error}");
            }
        }
    })
    .detach();
    let executor = cx.background_executor().clone();
    cx.background_executor()
        .spawn(async move {
            let coordinator = Coordinator::new(holder(), ExplicitStart);
            let launcher = ShellLauncher(launches);
            loop {
                executor
                    .timer(Duration::from_millis(TICK_INTERVAL_MS))
                    .await;
                let tick = locked(&store).map_err(|e| e.to_string()).and_then(|store| {
                    coordinator
                        .tick(&store, &launcher, monocode_core::time::now_ms())
                        .map_err(|e| e.to_string())
                });
                match tick {
                    Ok(tick) if !tick.errors.is_empty() => {
                        eprintln!("Task coordinator: {}", tick.errors.join("; "))
                    }
                    Err(error) => eprintln!("Task coordinator: {error}"),
                    Ok(_) => {}
                }
            }
        })
        .detach();
}
