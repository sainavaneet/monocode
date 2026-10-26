# Mono as development lead: corrected plan and handoff

Written 2026-10-26 after checking every claim against the code. Mono plans,
delegates, answers workers, reviews and reports. The app persists state,
routes decisions, detects failures, switches authorized accounts and resumes
interrupted work. Nothing below is built yet except where marked **exists**.

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

| Phase | Scope | Exit |
|---|---|---|
| 0 | Rebase `fix/orchestration-plan-card` onto `feat/project-switcher`; spike the `native_` task tables | Branch merges cleanly; migration runs on a copy of the shared database |
| 1 | Models, migrations, states, events, lease | Transaction and transition tests pass |
| 2 | Single-worker coordinator over `sessions.start` with workspace records | One assignment launches and reports durably |
| 3 | Worker to Mono to user decisions | Automatic answers and persistent waiting both work and survive restart |
| 4 | Review and correction | A failed check produces a correction and a verified completion |
| 5 | Account and provider recovery for Mono and workers | Failures resume without duplicate edits or account cycling |
| 6 | Restart, pause, cancel, steer | Interruptions preserve correct state |
| 7 | Monitoring card and timeline | Reflect persisted state and live events |
| 8 | Parallel scheduling and integration | Ownership and dependency order hold |
| 9 | Background execution on the remote host model | UI and host cannot both execute one task |

Feature-flag each phase until its end-to-end check passes. Use fake providers
(the python app-server fixtures in `provider.rs` and `parked.rs` are the
pattern), temporary Git repositories, and `Store::open(":memory:")`.

## Verification matrix

Unchanged from the original plan, plus: tasks ignore habit timeouts; a quit
during a running attempt restarts as Interrupted; the Tauri app running
alongside cannot take the task lease; a changed API key billing preference
does not alter an in-flight task's policy.

## Release acceptance

One task started in Mono's chat that plans, creates a workspace, launches a
worker, answers one question automatically, escalates one to the user and
waits, resumes, survives a simulated account failure, catches a failed check,
verifies the correction, reports with workspace and test evidence, and keeps
its state across an app restart.
