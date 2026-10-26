//! The process's one task coordinator (`monocode_core::tasks::coordinator`).
//! It ticks off the main thread and does nothing unless the
//! `native-task-coordinator` setting is `enabled`. Sessions it reserves are
//! handed to the Shell, which installs and submits them like `sessions.start`,
//! and the result is reported back: confirmed (attempt Running) or failed.
use super::{Database, locked};
use crate::shell::Shell;
use gpui::{App, AsyncApp, Global, WeakEntity};
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

/// Reviews workers' success claims. Only collecting the requests and
/// recording the outcomes hold the store lock; snapshotting the workspace and
/// running the plan's checks (which may take minutes) do not.
fn review_claims(coordinator: &Coordinator<ExplicitStart>, store: &Database) -> Result<(), String> {
    let now = monocode_core::time::now_ms;
    let jobs = {
        let store = locked(store)?;
        if !tasks::coordinator::enabled(&store) {
            return Ok(());
        }
        coordinator
            .review_jobs(&store, now())
            .map_err(|e| e.to_string())?
    };
    // One failing review must not stop the others; each failure counts
    // toward the request's retry limit, after which Mono reviews it.
    for job in jobs {
        let inspection = job.inspect();
        let store = locked(store)?;
        if let Err(error) = coordinator.record_review(&store, &job, inspection, now()) {
            eprintln!("Task coordinator review {}: {error}", job.delivery_id());
            let error = error.to_string();
            if let Err(failed) = coordinator.review_failed(&store, &job, &error, now()) {
                eprintln!("Task coordinator review {}: {failed}", job.delivery_id());
            }
        }
    }
    Ok(())
}

/// Moves tasks whose work is accepted toward completion and checks their
/// acceptance criteria, again running the checks without the store lock.
fn verify_tasks(coordinator: &Coordinator<ExplicitStart>, store: &Database) -> Result<(), String> {
    let now = monocode_core::time::now_ms;
    let (jobs, errors) = {
        let store = locked(store)?;
        if !tasks::coordinator::enabled(&store) {
            return Ok(());
        }
        coordinator
            .verify_jobs(&store, now())
            .map_err(|e| e.to_string())?
    };
    // A task that could not be prepared is skipped; the others still run.
    for error in errors {
        eprintln!("Task coordinator verification {error}");
    }
    // Each failure to record counts toward the round's retry limit, after
    // which Mono gets the task instead of the checks running again.
    for job in jobs {
        let inspection = job.inspect();
        let store = locked(store)?;
        if let Err(error) = coordinator.record_round(&store, &job, inspection, now()) {
            eprintln!("Task coordinator verification: {error}");
            let error = error.to_string();
            if let Err(failed) = coordinator.round_failed(&store, &job, &error, now()) {
                eprintln!("Task coordinator verification: {failed}");
            }
        }
    }
    Ok(())
}

/// Carries out recovery's stop requests: each failed worker's session is
/// stopped through the Shell's `sessions.stop` path, then the request is
/// recorded as done so its replacement may launch.
async fn stop_failed_workers(store: &Database, cx: &mut AsyncApp) {
    let lookup = store.clone();
    let requests = cx
        .background_executor()
        .spawn(async move {
            let store = locked(&lookup)?;
            let stops = tasks::recovery::pending_stops(&store).map_err(|e| e.to_string())?;
            Ok::<_, String>(
                stops
                    .into_iter()
                    .map(|stop| {
                        let handle = store.resolve_session_id(&stop.session_id).ok();
                        (stop.delivery, handle)
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .await;
    let requests = match requests {
        Ok(requests) if !requests.is_empty() => requests,
        Ok(_) => return,
        Err(error) => {
            eprintln!("Task recovery: {error}");
            return;
        }
    };
    let sessions: Vec<i64> = requests.iter().filter_map(|(_, h)| *h).collect();
    cx.update(|cx| {
        let shell = cx.try_global::<Service>().and_then(|s| s.shell.upgrade());
        if let Some(shell) = shell {
            shell.update(cx, |shell, cx| shell.stop_task_workers(&sessions, cx));
        }
    });
    let store = store.clone();
    let recorded = cx
        .background_executor()
        .spawn(async move {
            let store = locked(&store)?;
            let now = monocode_core::time::now_ms();
            for (delivery, _) in requests {
                tasks::recovery::stopped(&store, &delivery, now).map_err(|e| e.to_string())?;
            }
            Ok::<_, String>(())
        })
        .await;
    if let Err(error) = recorded {
        eprintln!("Task recovery: could not record a stopped worker: {error}");
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
    // Wakes the stop pump: after each tick and after each confirmation.
    let (pumps, pumped) = async_channel::unbounded::<()>();
    let stopping = store.clone();
    cx.spawn(async move |cx| {
        while pumped.recv().await.is_ok() {
            while pumped.try_recv().is_ok() {}
            stop_failed_workers(&stopping, cx).await;
        }
    })
    .detach();
    let confirmations = store.clone();
    let confirmed = pumps.clone();
    cx.spawn(async move |cx| {
        while let Ok((session, settings)) = receive.recv().await {
            let id = session.id;
            // A launch whose attempt stopped waiting (it timed out) is not
            // installed: two workers never share a worktree.
            let check = confirmations.clone();
            let allowed = cx
                .background_executor()
                .spawn(async move {
                    locked(&check)
                        .and_then(|store| {
                            tasks::recovery::launch_allowed(&store, id).map_err(|e| e.to_string())
                        })
                        .unwrap_or(true)
                })
                .await;
            let installed = cx.update(|cx| {
                if !allowed {
                    return Err("The worker's launch was given up before the app started it.");
                }
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
                        // A late confirmation records an orphan and asks
                        // for its session to be stopped.
                        Ok(()) => tasks::confirm_started(&store, id, now).map(|_| ()),
                        Err(_) if !allowed => tasks::confirm_started(&store, id, now).map(|_| ()),
                        Err(reason) => tasks::start_failed(&store, id, reason, now),
                    }
                    .map_err(|e| e.to_string())
                })
                .await;
            if let Err(error) = recorded {
                eprintln!("Task coordinator: could not record worker {id}'s start: {error}");
            }
            let _ = confirmed.try_send(());
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
                if let Err(error) = review_claims(&coordinator, &store) {
                    eprintln!("Task coordinator review: {error}");
                }
                if let Err(error) = verify_tasks(&coordinator, &store) {
                    eprintln!("Task coordinator verification: {error}");
                }
                let _ = pumps.try_send(());
            }
        })
        .detach();
}
