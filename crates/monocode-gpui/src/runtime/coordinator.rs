//! The process's one task coordinator (`monocode_core::tasks::coordinator`).
//! It ticks off the main thread and does nothing unless the
//! `native-task-coordinator` setting is `enabled`. Sessions it reserves are
//! handed to the Shell, which installs and submits them like `sessions.start`,
//! and the result is reported back: confirmed (attempt Running) or failed.
use super::{Database, locked};
use crate::shell::Shell;
use gpui::{App, AsyncApp, Entity, Global, WeakEntity};
use monocode_core::agent::Settings;
use monocode_core::store::Session;
use monocode_core::tasks;
use monocode_core::tasks::coordinator::{
    Coordinator, ExplicitStart, Launcher, TICK_INTERVAL_MS, Tick,
};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Where coordinator launches are installed (the latest Shell to start its
/// app bridge), and every Shell that may hold a worker session.
struct Service {
    shell: WeakEntity<Shell>,
    shells: Vec<WeakEntity<Shell>>,
    /// Stop requests whose session was open in no window, since when.
    absent: HashMap<String, u64>,
}

impl Service {
    fn live_shells(&self) -> Vec<Entity<Shell>> {
        self.shells.iter().filter_map(WeakEntity::upgrade).collect()
    }

    /// Where a launched worker is installed.
    fn launch_shell(&self) -> Option<Entity<Shell>> {
        preferred_or_live(&self.shell, &self.shells)
    }
}

/// `preferred` while it is alive, otherwise the most recent live one of
/// `all`: closing the latest window must not strand worker launches while
/// another window is still open.
pub(crate) fn preferred_or_live<T: 'static>(
    preferred: &WeakEntity<T>,
    all: &[WeakEntity<T>],
) -> Option<Entity<T>> {
    preferred
        .upgrade()
        .or_else(|| all.iter().rev().find_map(WeakEntity::upgrade))
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
/// A reporting worker's stop that stalls goes into `tick.stalled`.
fn review_claims(
    coordinator: &Coordinator<ExplicitStart>,
    store: &Database,
    tick: &mut Tick,
) -> Result<(), String> {
    let now = monocode_core::time::now_ms;
    let jobs = {
        let store = locked(store)?;
        if !tasks::coordinator::enabled(&store) {
            return Ok(());
        }
        coordinator
            .review_jobs(&store, now(), tick)
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

/// Integrates tasks whose accepted work spans several worktrees: merging
/// onto each task's integration branch runs without the store lock;
/// preparing (with the merge-authority check) and recording hold it.
fn integrate_tasks(
    coordinator: &Coordinator<ExplicitStart>,
    store: &Database,
) -> Result<(), String> {
    let now = monocode_core::time::now_ms;
    let (jobs, errors) = {
        let store = locked(store)?;
        if !tasks::coordinator::enabled(&store) {
            return Ok(());
        }
        coordinator
            .integration_jobs(&store, now())
            .map_err(|e| e.to_string())?
    };
    for error in errors {
        eprintln!("Task coordinator integration {error}");
    }
    // A failed record is retried by the next pass; Git work is idempotent
    // (merged branches are skipped).
    for job in jobs {
        let record = job.run();
        let store = locked(store)?;
        if let Err(error) = coordinator.record_integration(&store, &job, record, now()) {
            eprintln!("Task coordinator integration {}: {error}", job.task_id());
        }
    }
    Ok(())
}

/// How long a stop request's session must be open in no window before the
/// stop counts as done (a torn-down window's provider may still be exiting).
pub(crate) const STOP_ABSENT_GRACE_MS: u64 = 10_000;

/// Stops of sessions that are no longer in the store and cannot be found:
/// such a session cannot run, so each counts as done once the absent grace
/// has passed (no window is needed to prove it).
pub(crate) fn gone_stops(
    deliveries: &[String],
    absent: &mut HashMap<String, u64>,
    now: u64,
) -> Vec<String> {
    let mut done = Vec::new();
    for delivery in deliveries {
        let since = *absent.entry(delivery.clone()).or_insert(now);
        if now >= since.saturating_add(STOP_ABSENT_GRACE_MS) {
            absent.remove(delivery);
            done.push(delivery.clone());
        }
    }
    done
}

/// Stops each requested session in every window that has it and returns the
/// requests that are done: the session's turn has ended everywhere it is
/// open, or it is open in none of the (at least one) live windows. A session
/// whose id cannot be resolved, or with no window to look in, stays pending,
/// and so does its replacement. Called again on every tick until done.
pub(crate) fn stop_sessions(
    shells: &[Entity<Shell>],
    requests: &[(String, Option<i64>)],
    absent: &mut HashMap<String, u64>,
    now: u64,
    cx: &mut App,
) -> Vec<String> {
    if shells.is_empty() {
        return Vec::new();
    }
    let mut done = Vec::new();
    for (delivery, handle) in requests {
        let Some(handle) = handle else {
            continue;
        };
        let running: Vec<bool> = shells
            .iter()
            .filter_map(|shell| shell.update(cx, |shell, cx| shell.stop_task_worker(*handle, cx)))
            .collect();
        if running.is_empty() {
            // Open nowhere: its window may have just been torn down while
            // the provider still exits, so wait out a grace period first.
            let since = *absent.entry(delivery.clone()).or_insert(now);
            if now >= since.saturating_add(STOP_ABSENT_GRACE_MS) {
                absent.remove(delivery);
                done.push(delivery.clone());
            }
        } else {
            absent.remove(delivery);
            if !running.iter().any(|still| *still) {
                done.push(delivery.clone());
            }
        }
    }
    done
}

/// Carries out recovery's stop requests: each failed worker's session is
/// stopped through the Shell's `sessions.stop` path in every window, and the
/// request is recorded as done only once its turn has ended, so its
/// replacement never shares the worktree with it.
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
                        let handle = match store.resolve_session_id(&stop.session_id) {
                            Ok(handle) => Some(handle),
                            // Deleted: it cannot run anywhere (see below).
                            Err(_) if stop.gone => None,
                            Err(error) => {
                                eprintln!("Task recovery: session {}: {error}", stop.session_id);
                                None
                            }
                        };
                        (stop.delivery, handle, stop.gone)
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
    let done = cx.update(|cx| {
        let Some(service) = cx.try_global::<Service>() else {
            return Vec::new();
        };
        let shells = service.live_shells();
        let mut absent = std::mem::take(&mut cx.global_mut::<Service>().absent);
        let now = monocode_core::time::now_ms();
        let (gone, open): (Vec<_>, Vec<_>) = requests
            .iter()
            .cloned()
            .partition(|(_, handle, gone)| *gone && handle.is_none());
        let gone: Vec<String> = gone.into_iter().map(|(delivery, ..)| delivery).collect();
        let open: Vec<(String, Option<i64>)> = open
            .into_iter()
            .map(|(delivery, handle, _)| (delivery, handle))
            .collect();
        let mut done = stop_sessions(&shells, &open, &mut absent, now, cx);
        done.extend(gone_stops(&gone, &mut absent, now));
        cx.global_mut::<Service>().absent = absent;
        done
    });
    if done.is_empty() {
        return;
    }
    let store = store.clone();
    let recorded = cx
        .background_executor()
        .spawn(async move {
            let store = locked(&store)?;
            let now = monocode_core::time::now_ms();
            for delivery in done {
                tasks::recovery::stopped(&store, &delivery, now).map_err(|e| e.to_string())?;
            }
            Ok::<_, String>(())
        })
        .await;
    if let Err(error) = recorded {
        eprintln!("Task recovery: could not record a stopped worker: {error}");
    }
}

/// One coordinator tick. The flag is read once, holding the store only for
/// that: with `native-task-coordinator` off nothing else touches the store
/// and the stop pump is not woken. Returns whether the tick ran.
pub(crate) fn tick_pass(
    coordinator: &Coordinator<ExplicitStart>,
    store: &Database,
    launcher: &impl Launcher,
) -> bool {
    if !locked(store).is_ok_and(|store| tasks::coordinator::enabled(&store)) {
        return false;
    }
    // Tasks taken over since startup are reconciled the same way:
    // the store is locked only to read and write, never for Git.
    match reconcile(store, false) {
        Ok(taken) if !taken.errors.is_empty() => {
            eprintln!("Task coordinator takeover: {}", taken.errors.join("; "))
        }
        Ok(_) => {}
        Err(error) => eprintln!("Task coordinator takeover: {error}"),
    }
    let mut tick = locked(store).map_err(|e| e.to_string()).and_then(|store| {
        coordinator
            .tick(&store, launcher, monocode_core::time::now_ms())
            .map_err(|e| e.to_string())
    });
    // Review's stalled stops join the tick's, so one log line reports both.
    let mut detached = Tick::default();
    let target = match &mut tick {
        Ok(tick) => tick,
        Err(_) => &mut detached,
    };
    if let Err(error) = review_claims(coordinator, store, target) {
        eprintln!("Task coordinator review: {error}");
    }
    match &tick {
        Ok(tick) => {
            if !tick.errors.is_empty() {
                eprintln!("Task coordinator: {}", tick.errors.join("; "));
            }
            report_stalled(&tick.stalled);
        }
        Err(error) => eprintln!("Task coordinator: {error}"),
    }
    report_stalled(&detached.stalled);
    if let Err(error) = verify_tasks(coordinator, store) {
        eprintln!("Task coordinator verification: {error}");
    }
    if let Err(error) = integrate_tasks(coordinator, store) {
        eprintln!("Task coordinator integration: {error}");
    }
    true
}

/// Logs the workers whose stop has not been confirmed past the stall limit.
fn report_stalled(stalled: &[String]) {
    if !stalled.is_empty() {
        eprintln!(
            "Task recovery: the stop of {} has not been confirmed; no replacement starts until it is.",
            stalled.join(", ")
        );
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

/// The process's one coordinator: startup reconciliation and the ticker
/// share its lease holder, so a task this process reconciled is never taken
/// over by itself.
fn coordinator() -> &'static Coordinator<ExplicitStart> {
    static COORDINATOR: OnceLock<Coordinator<ExplicitStart>> = OnceLock::new();
    COORDINATOR.get_or_init(|| Coordinator::new(holder(), ExplicitStart))
}

/// The restart sequence, run once at launch off the main thread and only
/// when `native-task-coordinator` is enabled (`Coordinator::startup`): take
/// each unfinished task's lease (staying passive where another app holds
/// it; the ticker retries), mark workers that died with the last run as
/// Interrupted, check recorded worktrees, restore waiting decisions, and
/// leave paused tasks paused. The ticker then resumes what the policy allows.
pub(crate) fn reconcile_startup(store: Database) -> Option<std::thread::JoinHandle<()>> {
    STARTING.store(true, Ordering::SeqCst);
    let spawned = std::thread::Builder::new()
        .name("task-startup".into())
        .spawn(move || {
            // Cleared however the thread ends, so a panic cannot stop the
            // ticker for good.
            struct Started;
            impl Drop for Started {
                fn drop(&mut self) {
                    STARTING.store(false, Ordering::SeqCst);
                }
            }
            let _started = Started;
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_startup(&store)))
                    .unwrap_or_else(|panic| {
                        let what = panic
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_owned())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic".into());
                        Err(format!("startup panicked: {what}"))
                    });
            match result {
                Ok(startup) if startup.enabled => {
                    if !startup.errors.is_empty() {
                        eprintln!("Task startup: {}", startup.errors.join("; "));
                    }
                    if !startup.missing.is_empty() {
                        eprintln!(
                            "Task startup: missing or moved workspaces block their tasks: {}",
                            startup.missing.join(", ")
                        );
                    }
                }
                Ok(_) => {}
                Err(error) => eprintln!("Task startup: {error}"),
            }
        });
    spawned
        .map_err(|error| {
            STARTING.store(false, Ordering::SeqCst);
            eprintln!("Task startup could not start: {error}")
        })
        .ok()
}

/// The startup sequence with the store locked only to read and write: the
/// Git checks of recorded workspaces run unlocked, so the first window can
/// open meanwhile.
pub(crate) fn run_startup(store: &Database) -> Result<tasks::restart::Startup, String> {
    reconcile(store, true)
}

/// Startup's sequence over every unfinished task (`every`, at launch) or
/// only those with work or workers (each tick, so idle tasks stay free for
/// another app). The store is locked only to read and write, never for Git.
fn reconcile(store: &Database, every: bool) -> Result<tasks::restart::Startup, String> {
    let now = monocode_core::time::now_ms;
    let prepared = {
        let store = locked(store)?;
        if every {
            coordinator().startup_prepare(&store, now())
        } else {
            coordinator().takeover_prepare(&store, now())
        }
    };
    let (mut startup, pending) = prepared.map_err(|e| e.to_string())?;
    let problems = tasks::restart::check_pending(&pending);
    coordinator()
        .startup_finish(&*locked(store)?, &mut startup, problems, now())
        .map_err(|e| e.to_string())?;
    Ok(startup)
}

/// True while startup reconciliation runs; the ticker waits for it, so no
/// task resumes before its workspaces were checked.
static STARTING: AtomicBool = AtomicBool::new(false);

/// Registers the coordinator once per process; later calls only retarget the
/// Shell that installs launched workers.
pub(crate) fn install(store: Database, shell: WeakEntity<Shell>, cx: &mut App) {
    if let Some(service) = cx.try_global::<Service>() {
        let mut shells: Vec<_> = service
            .shells
            .iter()
            .filter(|s| s.upgrade().is_some() && *s != &shell)
            .cloned()
            .collect();
        shells.push(shell.clone());
        let absent = std::mem::take(&mut cx.global_mut::<Service>().absent);
        cx.set_global(Service {
            shell,
            shells,
            absent,
        });
        return;
    }
    cx.set_global(Service {
        shell: shell.clone(),
        shells: vec![shell],
        absent: HashMap::new(),
    });
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
                    locked(&check).and_then(|store| {
                        tasks::recovery::launch_allowed(&store, id).map_err(|e| e.to_string())
                    })
                })
                .await;
            // Fail closed: when it cannot be checked it is not installed;
            // the attempt stays Starting and the confirmation timeout
            // recovers it.
            let allowed = match allowed {
                Ok(allowed) => allowed,
                Err(error) => {
                    eprintln!("Task coordinator: worker {id} was not started: {error}");
                    continue;
                }
            };
            let installed = cx.update(|cx| {
                if !allowed {
                    return Err("The worker's launch was given up before the app started it.");
                }
                let shell = cx.try_global::<Service>().and_then(Service::launch_shell);
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
            let coordinator = coordinator();
            let launcher = ShellLauncher(launches);
            loop {
                executor
                    .timer(Duration::from_millis(TICK_INTERVAL_MS))
                    .await;
                if STARTING.load(Ordering::SeqCst) {
                    continue;
                }
                if tick_pass(coordinator, &store, &launcher) {
                    let _ = pumps.try_send(());
                }
            }
        })
        .detach();
}
