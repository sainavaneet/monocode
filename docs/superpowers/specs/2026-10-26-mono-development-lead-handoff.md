# Mono as development lead: corrected plan and handoff

Written 2026-10-26 after checking every claim against the code. Mono plans,
delegates, answers workers, reviews and reports. The app persists state,
routes decisions, detects failures, switches authorized accounts and resumes
interrupted work. The plan sections below were written before the build;
**exists** marks what was already there then.

**Update 2026-10-27:** phases 0 to 8 are now built on `feat/project-switcher`
behind the `native-task-coordinator` setting (off by default). The sections
below keep the original plan text; **Built** at the end says what exists,
where, and what remains (phase 9, background execution).

## Baseline (verified)

Native checkout: `/Users/navaneet/Developer/monocode-gpui-preview`, branch
`feat/project-switcher`. Tauri is still the production app; the native app is
a preview that shares `com.monocode.desktop/monocode.db` with it.

Commits on the branch, oldest first:

| Commit | Change |
|---|---|
| `94f81de9`, `44d161d9` | Project switcher design |
| `37ae3b00` | Project switcher on Cmd+Shift+P, palette on Cmd+Option+P |
| `70c54ad7` | Account switch keeps the conversation (carries the provider thread) |
| `e70d42ee` | API key billing design |
| `2194cdb5` | API key billing in core (`api_keys` module, `Settings.use_api_key`) |
| `1fa8dea5` | API key controls in the app (footer button, Settings, Mono switch) |
| `922a2db7` | Turn checkpoints tolerate a nested repository without a commit |
| next | Code blocks in task text no longer abort the app |

Other worktree: `/Users/navaneet/Developer/monocode-gpui-orchestration`, branch
`fix/orchestration-plan-card`, based at `e70d42ee`, **five commits behind**,
with uncommitted edits to `orchestration.rs`, `conversation.rs`, `shell.rs`,
`shell/orchestration.rs`, `transcript.rs`, `transcript/remote_plans.rs` and a
new `conversation/orchestration_card.rs`. The newer commits also touch
`shell.rs`, `conversation.rs` and `transcript.rs`, so reconciling this branch
is the first task, not an afterthought.

Installed apps: `Monocode Native Preview.app` (desktop data, the user's live
agents), `Monocode Native Dev.app` (bundle id `dev.usemono.gpui.dev`, data in
`Monocode Development`, for checking). Never quit the Preview without asking;
quitting kills every local agent (`process::shutdown_all`).

Test commands (run from the checkout, `--test-threads=1` for the GUI crate):

```sh
cargo test --locked --manifest-path crates/monocode-gpui/Cargo.toml -p monocode-core --profile native-dev
cargo test --locked --manifest-path crates/monocode-gpui/Cargo.toml -p monocode-gpui --profile native-dev -- --test-threads=1
cargo clippy --locked --manifest-path crates/monocode-gpui/Cargo.toml -p monocode-core -p monocode-gpui --all-targets --profile native-dev -- -D warnings
```

Baseline at handoff: core 605 passed, GUI 677 passed, Clippy and rustfmt clean.
Commit with explicit paths only; attribution lines are not used in this repo.

## Existing foundations to build on, not around

| Need | Exists | Where |
|---|---|---|
| Plan with tasks, file scopes, dependencies | `Plan { tasks: [Task { id, title, prompt, files, depends_on }] }` | `monocode-core/src/orchestration.rs` |
| Worker scheduling that caps concurrency and serializes overlapping file scopes | `schedule_orchestration`, `start_orchestration`, `stop_orchestration` | `monocode-gpui/src/shell/orchestration.rs` |
| Mono launching and steering sessions | `sessions.start` (with `workspaceMode`, `worktreeBase`, `worktreeCwd`, `notifyOnComplete`), `sessions.send`, `sessions.stop`, `worktrees.create` | `monocode-core/src/mono/api.rs`, `bridge.rs` |
| Completion routing back to Mono | host completion handling | `monocode-gpui/src/monos/host.rs` |
| Provider-native questions normalized to one event | `Protocol::question` producing `agent::Question` | `monocode-core/src/provider/questions.rs` |
| App command authorization and session binding | `app_access.rs`, visual bridge environment | `monocode-core/src/app_access.rs`, `visual_bridge.rs` |
| Durable messages with delivery receipts | `follow_up::Message` and receipts beside the transcript | `monocode-core/src/follow_up.rs` |
| Limit classification and in-place account switch | `usage::Limit::from_error`, `account_recovery_limit`, `switch_account`, `switch_usage_account`, `accounts::transfer` | `monocode-gpui/src/conversation/accounts.rs`, `monocode-core/src/accounts/` |
| Per-account usage and health cache | `accounts::Key`, usage snapshots | `monocode-gpui/src/accounts.rs` |
| Workspace snapshots tied to commits | `checkpoint.rs` turn captures | `monocode-core/src/checkpoint.rs` |
| Child process ownership per session | `process::owned_by`, registrations | `monocode-core/src/process.rs` |
| Habits with timeouts | `TIMEOUT_MS` 1 h, `APPROVAL_WAIT_MS` 12 h, `GRACE_MS` 2 h | `monocode-core/src/mono/habits.rs` |
| Agents running outside the UI | the fork's remote host and `runtime/remote` | precedent for background execution |

Not present today: worker questions routed to Mono (all questions go to the
user), decision requests, task records, leases, account allowlists and
cooldowns, verification records.

## Behaviour rules

| Situation | Behaviour |
|---|---|
| User asks for implementation | Mono owns the task through verification |
| User asks for a plan only | Inspect and plan, launch nothing |
| Automatic execution enabled | Plans proceed into execution |
| Worker asks a routine technical question | Mono decides and resumes it |
| Mono needs the user | Ask in Mono's chat and wait; no timeout, no inferred answer |
| Independent work remains | Continue only unaffected assignments |
| Worker stops | Classify; never assume success |
| Worker claims success | Review evidence against acceptance criteria |
| Auth or usage failure | Recover with eligible authorized accounts, bounded retries |
| No eligible account | Preserve progress, wait or block with a clear reason |
| Pause or cancel | Stop new work now, interrupt active work safely |
| App restarts | Every attempt that was running is Interrupted; reconcile before resuming |
| Task finishes | Report with evidence; distinguish implemented, committed, merged, pushed, deployed |

Tasks run outside the habit scheduler. The habit timeouts must not apply to
task workers.

Authority is a configurable capability policy per Mono, never inferred from
the request text.

## Durable model

New tables use the `native_` prefix and live beside `native_sessions`,
`native_rows`, `native_settings` and `native_reminders`. Never alter the
desktop schema that the Tauri app reads. Store account ids only, never keys
or tokens. Every mutation carries a revision; a stale writer cannot overwrite
newer requirements or resolve a superseded decision.

Entities: Task, TaskProject, AcceptanceCriterion, PlanRevision, Assignment
(extend `orchestration::Task`), WorkerAttempt, Workspace, DecisionRequest,
DecisionAnswer, Verification (references a checkpoint commit), Event (stable
id, sequence), PendingDelivery, ResourceLease (holder, expiry).

States, kept separate:

- Task stage: Planning, Ready, Implementing, Reviewing, Integrating,
  Verifying, Completed. Review or verification may send work back.
- Task lifecycle: Active, Waiting, Paused, Blocked, Cancelled, Completed.
- Assignment: Queued, Ready, Running, WaitingForDecision, NeedsReview,
  Accepted, Failed, Cancelled.
- Attempt: Created, Starting, Running, Waiting, Succeeded, Failed,
  Interrupted, Cancelled.

A successful attempt moves its assignment to NeedsReview. Only review accepts.
Only satisfied criteria complete the task. Transitions are defined in one
place with preconditions, who may request them, persisted changes, events,
eligibility effects and recovery behaviour.

## Coordinator

One core service, independent of any mounted view, registered as a global in
the GUI process. The lease is a database row with expiry because the Tauri app
is the realistic competing coordinator, not a second window.

Loop: read eligible work, check lifecycle, dependencies, authority and
resources, acquire the lease, persist a launch intent with a unique operation
id, dispatch through `sessions.start`, reconcile the receipt, observe events,
schedule the next action. At-least-once delivery with idempotent handling,
modelled on `follow_up.rs` receipts: a duplicated result does not start two
reviews, a retried launch resolves to the existing session, a replayed answer
is not submitted twice.

## Planning, workspaces, worker protocol

Planning: resolve the project, inspect the repository, recall memory and prior
decisions, write observable acceptance criteria, produce a versioned plan,
validate scopes and dependencies, choose workspaces, persist, then launch only
when the request or saved policy authorizes execution. Each assignment carries
objective, requirements, allowed files, dependencies, workspace, verification
expectations, result format, and how to ask Mono or report failure. Workers do
not inherit Mono's conversation.

Workspaces: one dedicated branch and worktree per substantial task by default,
created through the existing worktree commands, with base commit, branch and
ownership recorded before launch. Investigation reads the existing checkout;
overlapping file scopes serialize (the scheduler already does this); never
copy dirty changes silently; never delete a dirty worktree automatically.

Worker protocol: app commands `tasks.worker.status`, `ask`, `result`,
`blocked`, bound to task, assignment and attempt through the visual bridge
session binding. A result is a claim; a session that ends without one needs
reconciliation. Provider-native questions arrive through the existing
`Question` event; the new part is routing ordinary development questions to
Mono and sending the answer back through the provider's reply path.
Permission requests stay on the existing authorization path.

## Decisions

Worker question: persist the request, assignment to WaitingForDecision, queue
an event for Mono, Mono answers from the request, prior decisions, conventions,
memory and scope, or escalates to the user. Persist the answer, deliver it to
the exact worker once, resolve on acknowledgment. Queue events if Mono is busy;
never run two Mono turns on one conversation. A crashed waiting worker keeps
its question for the replacement.

User decision: shown in Mono's chat with the question, why it matters, the
recommendation, alternatives, what is paused and what continues, tied to a
decision id. Free-text replies resolve a decision only when unambiguous.
No default answer after any time. Revisions are recorded, not rewritten.

## Recovery

Reuse `Limit::from_error` for classification and the in-place account switch
for both Mono's chat and workers. Add: account allowlist per Mono and project,
cooldown and health state on the usage cache, spending limits where usage data
allows, cross-provider permission. Sequence: record the failure, stop dispatch
from the attempt, make sure the old worker cannot mutate the workspace,
reconcile completed tool operations, select an account, resume the provider
thread when supported, otherwise a replacement session with a durable handoff
(objective, revision, decisions, workspace, completed work, validation, pending
questions, next step; no credentials). One retry budget per attempt regardless
of account changes. Cancelled tasks never recover. Snapshot the API key billing
choice into the task policy since it changes which account bills a turn.

## Review, integration, pause, cancel, steer, restart, monitoring

As in the original plan, with these fixes:

- Verification records reference checkpoint commits; a later change
  invalidates them.
- Restart treats every attempt that was Running as Interrupted, because quit
  kills the process groups.
- Missing or moved worktrees produce an explicit recovery condition.
- The monitoring card distinguishes last worker activity, last status sync and
  last Mono review, and shows counts and stages, never percentages.
- Sidebar badge: `2 working`, `Needs your decision`, `Reviewing`,
  `Recovering`, `Paused`.

## Autonomy policy per Mono

Automatic execution, workspace preference, worker concurrency, account
fallback list, provider fallback, retry limits, spending controls (only where
enforceable), commit/merge/push/deploy authority, restart continuation,
worktree retention, API key billing. Snapshotted into each task; reducing
authority later applies to active work deliberately.

## Order of work

| Phase | Scope | Exit | Status |
|---|---|---|---|
| 0 | Rebase `fix/orchestration-plan-card` onto `feat/project-switcher`; spike the `native_` task tables | Branch merges cleanly; migration runs on a copy of the shared database | Built (tasks 1, 2) |
| 1 | Models, migrations, states, events, lease | Transaction and transition tests pass | Built (task-2) |
| 2 | Single-worker coordinator over `sessions.start` with workspace records | One assignment launches and reports durably | Built (tasks 3, 4) |
| 3 | Worker to Mono to user decisions | Automatic answers and persistent waiting both work and survive restart | Built (task-6) |
| 4 | Review and correction | A failed check produces a correction and a verified completion | Built (task-5) |
| 5 | Account and provider recovery for Mono and workers | Failures resume without duplicate edits or account cycling | Built (task-7) |
| 6 | Restart, pause, cancel, steer | Interruptions preserve correct state | Built (task-8; policy task-9) |
| 7 | Monitoring card and timeline | Reflect persisted state and live events | Built (task-10) |
| 8 | Parallel scheduling and integration | Ownership and dependency order hold | Built (task-11) |
| 9 | Background execution on the remote host model | UI and host cannot both execute one task | **Not built** |

Feature-flag each phase until its end-to-end check passes. Use fake providers
(the python app-server fixtures in `provider.rs` and `parked.rs` are the
pattern), temporary Git repositories, and `Store::open(":memory:")`.

## Verification matrix

Unchanged from the original plan, plus: tasks ignore habit timeouts; a quit
during a running attempt restarts as Interrupted; the Tauri app running
alongside cannot take the task lease; a changed API key billing preference
does not alter an in-flight task's policy.

**Built:** each extra has a core test; see **Built > Proof** for the names.

## Release acceptance

One task started in Mono's chat that plans, creates a workspace, launches a
worker, answers one question automatically, escalates one to the user and
waits, resumes, survives a simulated account failure, catches a failed check,
verifies the correction, reports with workspace and test evidence, and keeps
its state across an app restart.

**Built:** proven end to end by
`tasks::acceptance_tests::a_task_from_a_mono_chat_survives_questions_failures_corrections_and_a_restart`;
see **Built > Proof**.

## Built (2026-10-27)

Twelve tasks on `feat/project-switcher`, each reviewed before it landed
(tasks 9 and 10 were built on side branches and merged; the others were
committed on the branch directly), then a final whole-branch review and fix
batch (below). Core paths
are under `crates/monocode-core/src/`, app paths under
`crates/monocode-gpui/src/`.

### Turning it on

Everything is off unless the setting `native-task-coordinator` is exactly
`enabled` (`tasks::coordinator::FLAG`, read by `coordinator::enabled`). There
is no UI toggle. Set it with `Store::save_setting("native-task-coordinator",
"enabled")`. On the shared desktop database the native app keeps its settings
in `monocode.db.gpui.sqlite3` (table `settings`); on a native-only database
they are in `native_settings`. With it off, the coordinator, review, verify,
integration, inbox and monitoring passes return at once, worker and Mono task
commands refuse, no task tables are created, and the plan card's Start runs
the old in-app scheduler. Use the Dev app (`Monocode Development` data) to try
it, not the Preview, which shares the user's live agents.

### What exists, by phase

| Phase | Core | App |
|---|---|---|
| 0 | plan card merged from `fix/orchestration-plan-card` (`orchestration.rs`); `native_task*` tables created lazily on the first writable `Store::tasks()` | `conversation/orchestration_card.rs`, `shell/orchestration.rs` |
| 1 | `tasks/model.rs`, `states.rs` (the one transition table with requesters, preconditions, events, eligibility, recovery), `events.rs` (events, deliveries, leases), `store.rs` | — |
| 2 | `tasks/worker.rs` + `app_access.rs` `worker_action` (`tasks.worker.status/ask/result/blocked`, bound by session only); `tasks/coordinator.rs`, `launch.rs` (intent before dispatch, receipts, launch confirmation timeout), `workspaces.rs` (worktree per assignment, branch `mono/<task>/<assignment>`, base commit, owner), `brief.rs` | `runtime/coordinator.rs` (ticker, launcher, startup), `shell/app_commands` (worker route without operator access) |
| 3 | `tasks/decisions.rs` (routing, answer, escalate, hand-off once, user reply matching), `inbox.rs` (Mono queue, receipts, reminders), `waiting.rs`, `app_access/mono_tasks.rs` (`tasks.decisions.*`, `tasks.review.*`, `tasks.criteria.verify`) | `monos/tasks.rs` (task pump: inbox to Mono's chat, answers to workers), `monos/decision_card.rs`, `conversation/task_questions.rs` (provider questions on worker sessions go to Mono) |
| 4 | `tasks/review.rs`, `checks.rs` (no-shell check runner), `findings.rs`, `verify.rs` (stage driving, criterion rounds), `completion.rs` (verifications on checkpoint commits, `FinalReport`) | — |
| 5 | `usage/failure.rs` (failure categories), `accounts/eligibility.rs` (allowlists, cooldown and health), `tasks/recovery.rs`, `replace.rs`, `handoff.rs` (durable redacted hand-off), one retry budget per lineage | `conversation/task_recovery.rs`, `shell/task_recovery.rs` (stops), `conversation/mono_accounts.rs` (Mono chat switches account in place) |
| 6 | `tasks/lifecycle.rs` (pause, resume, cancel), `steer.rs`, `restart.rs` + `takeover.rs` (startup, lease takeover, Interrupted attempts, missing worktree condition), `workspace_check.rs` | `conversation/task_controls.rs`, plan-card controls |
| 6 (policy) | `tasks/autonomy.rs` (per-Mono and per-project policy, snapshot into each task with revisions, live reductions), `policy.rs` (snapshot incl. API key billing, `start_plan`), `app_access/mono_policy.rs` (`tasks.policy.show/apply`, `tasks.unblock`) | `monos/autonomy.rs`, `monos/autonomy_view.rs` |
| 7 | `tasks/view.rs` (monitor read model, badges), `timeline.rs` | `conversation/task_monitor.rs`, `monos/task_overview.rs`, sidebar badge |
| 8 | `tasks/guard.rs` (workspace ownership), `integration.rs` + `merging.rs` (integration branch `mono/<task>/integration`, dependency-order merges, conflicts to corrections or decisions) | `shell/orchestration.rs` (Start under the flag creates a coordinator task for Mono-owned chats) |

### Controller decisions that changed the plan

- Task tables are created on first writable use, not at open, so opening the
  shared database changes nothing (task-2).
- A worker's `failed` claim is only recorded; the coordinator classifies it
  (task-3).
- An attempt stays Starting until the app confirms the session started;
  unconfirmed after `LAUNCH_CONFIRM_TIMEOUT_MS` (60 s) it fails with
  `launch.unconfirmed`. Investigation (checkout mode) briefs are read-only
  (task-4).
- Acceptance criteria carry `checks`. The coordinator drives Implementing to
  Completed; check-less, missing or several-worktree criteria go to Mono
  (`mono_review`); checkout work never runs checks (task-5).
- Routed questions keep the provider request open (no 120 s auto-deny); open
  decisions are re-sent to Mono at most 3 times, 30 minutes apart; a provider
  `test_guard` refuses non-fixture binaries in test builds (task-6).
- Failure categories are Temporary, UsageExhausted, SharedQuota, Auth,
  ProviderUnavailable, Configuration, Policy, Unknown. The account allowlist
  is read live (Mono, then project, then the start account); only billing and
  the start account are snapshotted. A stop counts as done only when the turn
  ended in every window or the session is gone for 10 s; a stop stalled for
  2 minutes is reported (task-7).
- A takeover is a restart only when the previous holder is provably dead;
  otherwise its workers are held. Unknown holders need `confirmedStopped` to
  resume. Only working tasks are leased each tick (task-8).
- The allowlist is edited from the policy UI; every other policy field is
  snapshotted with revisions; live reductions apply at once, more authority
  needs `tasks.policy.apply`. Commit authority is enforced at review (task-9).
- Coordinator tasks only start from Mono-owned chats; other chats keep the
  in-app scheduler. Integration merges onto a task branch in its own
  worktree, never the user's branch, and stops at the first conflict
  (task-11).

### Proof

- Release acceptance, one core test driving the app's entry points with a
  fake launcher, a temporary Git repository and a file-backed store:
  `tasks::acceptance_tests::a_task_from_a_mono_chat_survives_questions_failures_corrections_and_a_restart`.
  Steps: `policy::start_plan` from a Mono chat (as the plan card's Start);
  worktree creation and launch; a competing holder refused; a routine
  question answered through `tasks.decisions.answer` and handed to the worker
  once; an escalated question open for 30 days of ticks and answered by the
  user's reply; usage exhaustion recovered on the next allowed account in the
  same worktree with a hand-off and one retry spent, while a flipped API key
  preference leaves the snapshot alone; a failing criterion sent back and
  relaunched with the finding; the app restarted with the running attempt
  Interrupted and continued; the correction reviewed, verified and reported
  with the worktree, commits, checkpoints and check evidence.
- The app-side pumps those steps rely on are covered in the GUI suite:
  `shell::orchestration::tests::with_the_coordinator_on_start_persists_a_task_the_coordinator_schedules`,
  `runtime::coordinator_tests::{a_launched_worker_runs_its_brief_in_its_worktree_and_reports_durably, a_worker_that_hits_its_usage_limit_continues_on_the_next_allowed_account, startup_interrupts_workers_that_died_with_the_last_run}`,
  `monos::task_tests::{a_routed_provider_question_gets_monos_answer_through_its_reply_once, an_escalated_decision_is_a_card_in_monos_chat_that_the_user_answers, habit_timeouts_never_stop_a_task_worker}`.
- Matrix extras: habit timeouts,
  `tasks::lifecycle::tests::the_coordinator_never_times_out_a_task_worker_past_habit_timeouts`;
  quit during a running attempt,
  `tasks::restart::tests::running_workers_are_interrupted_on_restart_and_continue_on_their_lineage`;
  the other app cannot take the lease,
  `tasks::coordinator::tests::two_coordinators_cannot_both_hold_a_task_lease`,
  `tasks::restart::tests::a_lease_another_app_holds_keeps_startup_passive_until_it_expires`
  and `a_live_holder_taken_over_keeps_its_workers_held_until_it_is_gone`;
  API key preference,
  `tasks::handoff::tests::the_api_key_choice_is_snapshotted_when_the_task_is_created`
  and
  `tasks::autonomy::tests::the_snapshot_taken_at_creation_ignores_later_policy_and_billing_changes`.

### Known gaps (deferred minors)

From the controller ledger, accepted for later:

- Plan card: streaming reveal replay when the plan tag appears mid-stream;
  stringly typed status labels and index-matched rows in
  `orchestration_card.rs`; untested shell glue; planning state split across
  `composer_features.rs` and `orchestration_card.rs`.
- Store: `TaskStore::state` race; idempotent inserts ignore a differing
  payload; no events for
  requirement edits; no secondary indexes; `tasks()` re-runs `CREATE TABLE IF
  NOT EXISTS` per call.
- Worker protocol: `NO_SESSION` matched by string between `app_access.rs` and
  `store/shared.rs`. (The Succeeded/NeedsReview requester sets are now pinned
  by `states::tests::succeeded_and_needs_review_allow_the_same_requesters`.)
- Coordinator: verification no longer runs Git under the store lock, but
  other steps of the tick still do (review preparation, launch preparation,
  integration preparation, recovery's workspace checks), and the GUI takes
  the store lock on the UI thread in places (`refresh_cards`, `batch_failed`); GUI install/confirm receiver untested;
  `dirtyBase` lost on a crash between `save_workspace` and the intent; no
  remote:// refusal or rate limit on the worker route; first-call race refusal
  does not say retry.
- Review and verification: `Coordinator::verify` stops at the first per-task
  error (tests only); snapshot refs, worktrees and integration branches are
  never cleaned up (retention not built); checks block the background thread
  up to their timeout; `checkpoint.rs` is 1296 lines.
- Decisions: `batch_failed` is not one transaction; the decision card cannot
  answer multi-part questions; `refresh_cards`/`batch_failed` lock on the UI
  thread; a criterion check can race the coordinator's Git index lock;
  reminders stop after 3 with no passive indicator; `test_guard` uses an
  exe-name heuristic; the inbox re-scans history each tick.
- Recovery: `record_failure` to the shared extras database is not atomic with
  the task transaction; a stalled stop writes every tick after the first;
  "failed to start" is matched loosely; grace is measured from the first pump
  sighting.
- Restart and steer: the `assignmentIds` takeover filter and the startup
  panic guard are untested; `store/shared.rs` grew (`session_persisted`).
- Policy: the commit check counts all commits since base, not only those after
  a revocation; fail-closed check untested; core tests rely on unique Mono ids
  for the live policy registry.
- Monitoring: last status sync is derived from lease expiry; recovery lines
  truncate at 160 characters; `ProgressStatus` and `WorkerState` both exist;
  badge and card differ on Starting with a busy session.
- Integration: stops at the first conflict; a revoked-merge clean record is
  usable by Mono's manual Verifying; a committed file that is now gitignored
  is falsely refused.

- Authority over a worker's own shell: the app cannot technically stop a
  worker from pushing, merging or deploying with its own tools if told to.
  What is enforced: the brief states what the effective policy forbids, an
  answer that mentions such a step reaches the worker after a reminder that
  it cannot grant authority (`brief::handoff_message`), Mono's help says
  answers cannot grant authority, commits are enforced at review
  (`UnauthorizedCommits`), merges only by integration with Merge authority,
  and checks may never push. Mono's answers themselves are not refused for
  granting authority, and a provider-native reply carries no reminder.
- Check confinement is best effort, not a sandbox: a program the project
  itself runs (`npm run`, `make`, `cargo run`, `./gradlew`, a test runner)
  can do anything its scripts do, including reaching outside the workspace,
  and the argument rules are heuristics over argv. Known consequences:
  a route pattern whose first segment exists at `/` (such as `/users` on
  macOS) is refused as a path; `bash scripts/test.sh` is refused (run the
  script directly); `node --import`/`-r` are refused; `-o`, `-d` and `-f`
  are treated as path options for every program; the shell-character rule
  makes `git diff HEAD~1` unrunnable; `python -m timeit` still runs code
  given inline; and an unknown interpreter option that takes a separate
  value can hide a following `-e`.
- **Before enabling the flag for real use:** UI-thread store locks during the
  coordinator tick (Git work in the remaining locked steps above, and the GUI
  locks on the UI thread) must be removed.

### Final review fixes (2026-10-27)

- A criterion with several checks is satisfied only when every one of its
  checks passed on one snapshot; a check that cannot run is recorded as not
  passing (`completion::criterion_satisfied`, shared by the completion
  precondition, the next round, the report and the task view).
- A worker that reports a result is stopped; nothing relaunches in its
  workspace and its claim is not reviewed until the app confirms the stop.
- Checks are confined to their workspace, best effort (`tasks/confine.rs`):
  path-option values and arguments that are (or would be created beside)
  something outside the workspace, or climb out with `..`, are refused, while
  patterns such as `/health` pass; shells, wrappers (`env`, `xargs`, `sudo`,
  `timeout`, `npx`, `pnpm dlx` ...) and interpreters given inline code
  (`python -c`, `node -e`, an awk program ...) are refused; runners
  (`uv run`, `poetry run`, `bundle exec`, `pnpm exec`, `npx` ...) are judged
  by the program they wrap; `name=/abs` arguments are checked after the `=`;
  Git may only read (read-only subcommands and the listing forms of
  `branch`/`tag`/`remote`/`stash`/`worktree`), never with `-C`/`--git-dir`/
  `--work-tree`/`-c`/`--exec-path`, aliases or `!`, and never commits, merges
  or rebases, whatever the task's authority (a check verifies; `rebase
  --exec` runs programs, and history changes leave the tree unchanged).
  Mono's own `tasks.criteria.verify` checks go through the same confinement.
- A declared check that cannot run no longer blocks completion for good: Mono
  may waive it with `tasks.criteria.waive` and a required reason, shown as
  "waived: <reason>" in the final report and the timeline. A check that ran
  and failed cannot be waived (`tasks/waive.rs`).
- Verification's locked steps run no Git; the final report is built inside
  the completing transaction (`tasks/report.rs`).
- Plan assignment ids must differ ignoring case, and `integration` is
  reserved; recording a verification is idempotent.
- With the flag off the GUI shows no Autonomy settings and Mono chats switch
  accounts exactly as before; the ticker does nothing beyond one flag check.
  A worker's routed question is never auto-skipped when routing fails, a
  KeepOpen permission stays pending, launches fall back to any live window,
  and a recreated worker view takes its billing pin from the task policy.

### Remaining

- **Phase 9, background execution on the remote host model**: not built.
  Workers run only while the native app runs; quitting interrupts them and the
  next start recovers them. The UI-versus-host exclusivity this phase needs
  would build on the existing task lease (`coordinator::lease_resource`,
  holder liveness in `restart.rs`).
- Mono's own planning turn (inspect the repository, recall memory, write
  criteria) is Mono's prompt work; the app persists and starts what Mono's
  plan card holds. Plans from chats no Mono owns still use the in-app
  scheduler.
- Worktree retention and clean-up, spending controls, and deployment evidence
  are not built (`deployed` is never inferred).
