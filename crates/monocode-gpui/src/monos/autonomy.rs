//! A Mono's autonomy policy in the app (`monocode_core::tasks::autonomy`).
//!
//! The policy lives in the preferences (`Preferences::mono_policies`), is
//! edited in Settings › Monos and in the Mono's own Autonomy settings, and is
//! saved through the shell like every preference; applying preferences
//! publishes it to the task coordinator. The account fallback list is not
//! part of it: it is task-7's live allowlist in `native_settings`, edited
//! here directly. The task list applies the current policy to a task (the
//! explicit step) or unblocks it.
use super::host::Effect;
use super::*;
use monocode_core::accounts::eligibility::{self, AllowedAccount, Allowlist, Scope};
use monocode_core::app_access as app;
use monocode_core::mono::bridge::Call;
use monocode_core::store::Store;
use monocode_core::tasks::Requester;
use monocode_core::tasks::autonomy::{
    self, Autonomy, MonoAutonomy, Retention, Standing, WorkspacePreference,
};
use monocode_core::time::now_ms;

/// The policy changed in the Mono's settings; the shell saves it.
pub(crate) struct AutonomyEdited(pub String, pub MonoAutonomy);
impl EventEmitter<AutonomyEdited> for Monos {}

/// How one policy field is edited.
pub(crate) enum Control {
    Switch {
        get: fn(&Autonomy) -> bool,
        set: fn(&mut Autonomy, bool),
    },
    Choice {
        options: &'static [(&'static str, &'static str)],
        get: fn(&Autonomy) -> String,
        set: fn(&mut Autonomy, &str),
    },
}

/// One policy field, as both settings views show it.
pub(crate) struct Field {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub control: Control,
}

/// Why spending is not capped, shown beside the policy.
pub(crate) const SPENDING_NOTE: &str = "Spending limits are not enforced. Providers report usage as plan windows (how much of a limit is used), not as cost, and API key billing is invoiced by the provider, so Monocode cannot see or cap what a task spends. Worker concurrency and the retry limit bound how much work a task starts; an account that runs out rests until its reported reset.";

/// Every count the policy accepts (`Autonomy::normalized`), so a saved
/// value always shows as selected.
const WORKERS: &[(&str, &str)] = &[
    ("1", "1"),
    ("2", "2"),
    ("3", "3"),
    ("4", "4"),
    ("5", "5"),
    ("6", "6"),
    ("7", "7"),
    ("8", "8"),
];
const RETRIES: &[(&str, &str)] = &[
    ("0", "0"),
    ("1", "1"),
    ("2", "2"),
    ("3", "3"),
    ("4", "4"),
    ("5", "5"),
    ("6", "6"),
    ("7", "7"),
    ("8", "8"),
    ("9", "9"),
    ("10", "10"),
];
const WORKSPACES: &[(&str, &str)] = &[
    ("automatic", "Automatic"),
    ("worktree", "Own worktree"),
    ("checkout", "Current checkout"),
];
const RETENTION: &[(&str, &str)] = &[("keep", "Keep"), ("remove-clean", "Remove clean")];
const BILLING: &[(&str, &str)] = &[
    ("app", "Follow app"),
    ("key", "API key"),
    ("login", "Login"),
];

/// Every field of the policy, in display order.
pub(crate) fn fields() -> [Field; 12] {
    [
        Field {
            id: "autonomy-automatic-execution",
            label: "Start planned work on its own",
            description: "When a plan is ready, its workers launch without waiting for you or the Mono to start it. Off: it plans, then waits for a start.",
            control: Control::Switch {
                get: |p| p.automatic_execution,
                set: |p, on| p.automatic_execution = on,
            },
        },
        Field {
            id: "autonomy-workers",
            label: "Workers at once",
            description: "How many workers one task runs at the same time. Lowering it starts no new worker until fewer are running; none is stopped.",
            control: Control::Choice {
                options: WORKERS,
                get: |p| p.clone().normalized().worker_limit.to_string(),
                set: |p, v| p.worker_limit = v.parse().unwrap_or(1),
            },
        },
        Field {
            id: "autonomy-workspace",
            label: "Workspace",
            description: "Automatic gives work that changes files its own worktree and lets investigations read your checkout. Current checkout runs read-only work only: work that would change files is refused, never written into your checkout.",
            control: Control::Choice {
                options: WORKSPACES,
                get: |p| {
                    match p.workspace {
                        WorkspacePreference::Automatic => "automatic",
                        WorkspacePreference::Worktree => "worktree",
                        WorkspacePreference::Checkout => "checkout",
                    }
                    .into()
                },
                set: |p, v| {
                    p.workspace = match v {
                        "worktree" => WorkspacePreference::Worktree,
                        "checkout" => WorkspacePreference::Checkout,
                        _ => WorkspacePreference::Automatic,
                    }
                },
            },
        },
        Field {
            id: "autonomy-restart",
            label: "Continue after a restart",
            description: "Work interrupted when the app quit continues once it reopens. Off: it waits for you or the Mono to resume it.",
            control: Control::Switch {
                get: |p| p.restart_continuation,
                set: |p, on| p.restart_continuation = on,
            },
        },
        Field {
            id: "autonomy-retention",
            label: "Worktrees when done",
            description: "Keep every worktree, or allow clean ones to be removed after the task completes. A worktree with uncommitted changes is never removed.",
            control: Control::Choice {
                options: RETENTION,
                get: |p| {
                    match p.worktree_retention {
                        Retention::Keep => "keep",
                        Retention::RemoveClean => "remove-clean",
                    }
                    .into()
                },
                set: |p, v| {
                    p.worktree_retention = if v == "remove-clean" {
                        Retention::RemoveClean
                    } else {
                        Retention::Keep
                    }
                },
            },
        },
        Field {
            id: "autonomy-commit",
            label: "Commit",
            description: "Workers commit in their task's own worktree. Off: they leave changes uncommitted for review.",
            control: Control::Switch {
                get: |p| p.commit,
                set: |p, on| p.commit = on,
            },
        },
        Field {
            id: "autonomy-merge",
            label: "Merge",
            description: "Merge reviewed work into its base branch.",
            control: Control::Switch {
                get: |p| p.merge,
                set: |p, on| p.merge = on,
            },
        },
        Field {
            id: "autonomy-push",
            label: "Push",
            description: "Push branches to a remote.",
            control: Control::Switch {
                get: |p| p.push,
                set: |p, on| p.push = on,
            },
        },
        Field {
            id: "autonomy-deploy",
            label: "Deploy",
            description: "Run deployment steps.",
            control: Control::Switch {
                get: |p| p.deploy,
                set: |p, on| p.deploy = on,
            },
        },
        Field {
            id: "autonomy-retries",
            label: "Retries per worker",
            description: "How often recovery retries a failed worker, across account changes, before handing it to the Mono.",
            control: Control::Choice {
                options: RETRIES,
                get: |p| p.clone().normalized().retry_limit.to_string(),
                set: |p, v| p.retry_limit = v.parse().unwrap_or(0),
            },
        },
        Field {
            id: "autonomy-provider-fallback",
            label: "Use another provider",
            description: "When a provider is unavailable, recovery may move the work to an allowed account of another provider.",
            control: Control::Switch {
                get: |p| p.provider_fallback,
                set: |p, on| p.provider_fallback = on,
            },
        },
        Field {
            id: "autonomy-billing",
            label: "API key billing",
            description: "Bill task workers to the saved API key or the login. Follow app uses the provider's API key switch when the task starts.",
            control: Control::Choice {
                options: BILLING,
                get: |p| {
                    match p.api_key_billing {
                        None => "app",
                        Some(true) => "key",
                        Some(false) => "login",
                    }
                    .into()
                },
                set: |p, v| {
                    p.api_key_billing = match v {
                        "key" => Some(true),
                        "login" => Some(false),
                        _ => None,
                    }
                },
            },
        },
    ]
}

/// What the Mono's Autonomy settings show besides the policy itself.
#[derive(Default)]
pub(super) struct PanelState {
    /// The project whose override is edited; None edits the Mono's policy.
    pub(super) scope: Option<String>,
    pub(super) tasks: Vec<Standing>,
    /// The live allowlist saved for the scope, if any.
    pub(super) allowlist: Option<Allowlist>,
    /// Accounts the app knows, with their labels.
    pub(super) known: Vec<(AllowedAccount, String)>,
    pub(super) error: Option<String>,
    /// The scope's account list has loaded; until then it cannot be edited
    /// (an edit would replace the saved list with one built from nothing).
    pub(super) loaded: bool,
    /// Bumped when the scope changes, so a load for the old scope is dropped.
    generation: u64,
}

fn allowlist_scope<'a>(mono: &'a str, scope: Option<&'a str>) -> Scope<'a> {
    match scope {
        Some(project) => Scope::MonoProject(mono, project),
        None => Scope::Mono(mono),
    }
}

/// What the panel reads from the store for one Mono and scope.
struct Loaded {
    tasks: Vec<Standing>,
    allowlist: Option<Allowlist>,
    known: Vec<(AllowedAccount, String)>,
}

fn load(store: &Store, mono: &str, scope: Option<&str>) -> Result<Loaded, String> {
    let tasks = autonomy::mono_tasks(store, mono).map_err(|e| e.to_string())?;
    let allowlist = eligibility::allowlist(store, allowlist_scope(mono, scope))?;
    let book = store.provider_accounts()?;
    let known = Kind::ALL
        .into_iter()
        .flat_map(|kind| book.list(kind))
        .map(|account| {
            let label = format!("{} · {}", account.provider, account.label);
            (AllowedAccount::new(&account.provider, &account.id), label)
        })
        .collect();
    Ok(Loaded {
        tasks,
        allowlist,
        known,
    })
}

impl Monos {
    /// `tasks.policy.*` and `tasks.unblock` from a Mono's chat. A habit run
    /// cannot act on tasks; the Mono comes from the calling session.
    pub(super) fn policy_call(&mut self, call: Call, mono: Mono, cx: &mut Context<Self>) {
        if self.running.contains_key(&call.session) {
            let _ = call
                .reply
                .send(Err("A habit run cannot act on tasks".into()));
            return;
        }
        let action = call.action.clone();
        let input = call.input.clone();
        let id = mono.id.clone();
        self.call_work(
            call,
            mono.id,
            move |store| {
                let store = locked(&store)?;
                app::mono_policy_action(&store, &id, &action, &input, now_ms())
                    .map(|value| (value, Effect::None))
            },
            cx,
        );
    }

    /// Reloads the active Mono's tasks, allowlist and known accounts.
    pub(super) fn refresh_autonomy(&mut self, cx: &mut Context<Self>) {
        let Some(mono) = self.active.clone() else {
            return;
        };
        let scope = self.autonomy.scope.clone();
        let generation = self.autonomy.generation;
        let store = self.store.clone();
        let work = cx
            .background_executor()
            .spawn(async move { load(&*locked(&store)?, &mono, scope.as_deref()) });
        cx.spawn(async move |this, cx| {
            let loaded = work.await;
            let _ = this.update(cx, |this, cx| {
                if this.autonomy.generation != generation {
                    return;
                }
                match loaded {
                    Ok(Loaded {
                        tasks,
                        allowlist,
                        known,
                    }) => {
                        this.autonomy.tasks = tasks;
                        this.autonomy.allowlist = allowlist;
                        this.autonomy.known = known;
                        this.autonomy.loaded = true;
                    }
                    Err(error) => this.autonomy.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Edits the Mono's policy, or the scope project's override of it.
    pub(super) fn set_autonomy_scope(&mut self, scope: Option<String>, cx: &mut Context<Self>) {
        self.autonomy.scope = scope;
        self.autonomy.generation += 1;
        self.autonomy.loaded = false;
        self.autonomy.allowlist = None;
        self.autonomy.known.clear();
        self.refresh_autonomy(cx);
        cx.notify();
    }

    /// Changes one field in the current scope and hands the policy to the
    /// shell to save; a project's first change creates its override.
    pub(super) fn edit_autonomy(
        &mut self,
        change: impl FnOnce(&mut Autonomy),
        cx: &mut Context<Self>,
    ) {
        let Some(mono) = self.active.clone() else {
            return;
        };
        let mut policy = self.prefs.mono_policy(&mono);
        match self.autonomy.scope.clone() {
            None => change(&mut policy.policy),
            Some(project) => {
                let mut scoped = policy.for_project(&project).clone();
                change(&mut scoped);
                policy.set_project(&project, Some(scoped));
            }
        }
        self.save_autonomy(mono, policy, cx);
    }

    /// Drops the scope project's override: it follows the Mono's policy.
    pub(super) fn use_mono_autonomy(&mut self, cx: &mut Context<Self>) {
        let (Some(mono), Some(project)) = (self.active.clone(), self.autonomy.scope.clone()) else {
            return;
        };
        let mut policy = self.prefs.mono_policy(&mono);
        policy.set_project(&project, None);
        self.save_autonomy(mono, policy, cx);
    }

    fn save_autonomy(&mut self, mono: String, policy: MonoAutonomy, cx: &mut Context<Self>) {
        self.prefs
            .mono_policies
            .insert(mono.clone(), policy.clone());
        cx.emit(AutonomyEdited(mono, policy));
        cx.notify();
    }

    /// Changes the scope's account fallback list (priority is list order)
    /// and saves it live; an empty list falls back to the next one in line.
    pub(super) fn edit_allowlist(
        &mut self,
        change: impl FnOnce(&mut Vec<AllowedAccount>),
        cx: &mut Context<Self>,
    ) {
        let Some(mono) = self.active.clone() else {
            return;
        };
        if !self.autonomy.loaded {
            return;
        }
        let scope = self.autonomy.scope.clone();
        let mut list = self.autonomy.allowlist.clone().unwrap_or_default();
        change(&mut list.accounts);
        for (ix, account) in list.accounts.iter_mut().enumerate() {
            account.priority = ix as i64;
        }
        self.autonomy.allowlist = (!list.accounts.is_empty()).then(|| list.clone());
        self.autonomy_work(
            move |store| {
                let scope = allowlist_scope(&mono, scope.as_deref());
                if list.accounts.is_empty() {
                    eligibility::clear_allowlist(store, scope)
                } else {
                    eligibility::save_allowlist(store, scope, &list)
                }
            },
            cx,
        );
    }

    /// Takes the Mono's current policy for the task's project into the task.
    pub(super) fn apply_task_policy(&mut self, standing: &Standing, cx: &mut Context<Self>) {
        let Some(mono) = self.active.clone() else {
            return;
        };
        let Some(revision) = standing.snapshot.as_ref().map(|s| s.revision) else {
            return;
        };
        let live = self
            .prefs
            .mono_policy(&mono)
            .for_project(&standing.task.project)
            .clone();
        let task = standing.task.id.clone();
        self.autonomy_work(
            move |store| {
                autonomy::apply(store, &task, revision, &live, now_ms())
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            },
            cx,
        );
    }

    /// The user unblocks a blocked task, through the transition table.
    pub(super) fn unblock_task(&mut self, standing: &Standing, cx: &mut Context<Self>) {
        let task = standing.task.id.clone();
        let revision = standing.task.revision;
        self.autonomy_work(
            move |store| {
                store
                    .tasks()
                    .and_then(|tasks| tasks.unblock(&task, revision, Requester::User, now_ms()))
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            },
            cx,
        );
    }

    /// Runs a store change off the main thread, then shows its error, if
    /// any, and reloads.
    fn autonomy_work(
        &mut self,
        work: impl FnOnce(&Store) -> Result<(), String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let work = cx
            .background_executor()
            .spawn(async move { work(&*locked(&store)?) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                this.autonomy.error = result.err();
                this.refresh_autonomy(cx);
            });
        })
        .detach();
    }
}
