# Super-agent orchestration: audit, plan and what landed (2026-10-28)

Scope: the native app on `feat/project-switcher`, installed into Monocode
Native Dev only. Main and the Preview app's live agents were not touched.

## How orchestration works (verified against the code)

A Mono is the lead. It has two ways to get work done:

1. **Quick worker.** `sessions.start {prompt, project, brief, notifyOnComplete}`
   through the app CLI. The brief (objective, context, constraints,
   deliverable, verification) is validated (`mono/delegation.rs`) and
   rendered into the worker's first prompt. The app sends Mono one
   completion report per turn group once every session it started stops
   (`monos/host.rs::check_completions`). No durable state, no review gate,
   no worktree isolation unless Mono asks for `workspaceMode: worktree`.
2. **Durable task plan** (only with `native-task-coordinator` enabled, which
   Dev has). Mono answers with a `<monocode_orchestration>` JSON plan; the
   plan card shows it; the user presses Start; `policy::start_plan` creates a
   task with assignments, acceptance criteria (one per assignment, with the
   assignment's `verification` commands as checks) and a policy snapshot.
   The coordinator (`tasks/coordinator.rs`, ticked every 5 s from
   `runtime/coordinator.rs`) leases the task, creates one worktree per
   assignment (`mono/<task>/<assignment>` from the base commit), launches a
   worker with a self-contained brief (`tasks/brief.rs`), and observes its
   `tasks.worker.*` reports. A succeeded claim is reviewed (checks run
   without a shell, confined to the worktree); check-less work goes to Mono
   through its task inbox; Mono answers worker questions or escalates to the
   user with a decision card; recovery switches accounts on usage or auth
   failures with one retry budget per lineage; restart marks running
   attempts Interrupted and continues; integration merges accepted branches
   onto `mono/<task>/integration` in dependency order; verification rounds
   run the criteria checks on the result; completion builds a final report
   from Git evidence (implemented, committed, merged, pushed, deployed).

The UI shows: the plan card, the task monitor card (stage, Mono activity,
worker rows, workspace and branch, counts, three times, recovery lines,
decisions waiting, expandable timeline), lifecycle controls, the Mono
overview's task summary, and a sidebar badge.

## Gaps found and their fixes

Four targeted audits (uncommitted diff review, quick-worker path, soul
editor conflict, Git under the store lock) plus direct reading.

| # | Gap | Fix | Proof |
|---|---|---|---|
| 1 | The final report never reached Mono or the user: `complete_with` recorded it and the card hid completed tasks | The completing transaction enqueues a Mono inbox item ("Task complete" with the evidence flags, per-assignment branch and commits, criteria, waived checks, and what to tell the user); completed tasks stay on the card for 24 h as a compact report card with a Hide button (`lifecycle::recent_completions`, `view::ReportSummary`, `task_monitor::task_report_card`) | `review_tests::completion_needs_criteria_no_open_decision_and_a_final_report`, `task_monitor_tests::a_completed_task_shows_its_report_until_the_user_hides_it` |
| 2 | Three contradictory instruction blocks reached Mono; the plan preamble "Prepare an orchestration plan" was appended to every turn; `brief` undocumented in help | One lead playbook (`delegation::policy(durable)`: answer directly, quick worker, durable plan) built into `mono::context_with`; `orchestration::PLAN_FORMAT` split from the Orchestrator preamble and appended only as "Plan format (route 3)"; help documents `brief` and the completion report | `delegation::tests::the_playbook_names_each_route_once…`, `mono::tests::the_context_states_the_session_project…` |
| 3 | A Mono with two projects could not chat (`project::directory` errored) or start a plan | The session runs in the assigned project the chat's directory lies in, else the first that exists, and the context says so; a plan starts in the one assigned project every assignment's `repository` resolves to (`Mono::plan_project`) | `project::tests::mono_project_is_explicit_and_never_merged`, `mono::tests::a_plan_runs_in_the_one_assigned_project…` |
| 4 | Quick-worker completion groups packed the chat id into 32 bits (lossy on the shared database); reports were head-truncated with no outcome; a worker waiting on the user stalled the group in silence; a not-yet-loaded session counted as finished | `Launch.origin_session` + `origin_turn` (legacy packed values unpacked on load); each report line carries the app's outcome and keeps the end of the transcript (`response::limit_keeping_end`); a one-time "[Delegated work needs the user]" notice; unloaded sessions are tracked and waited for, deleted ones count as over | `task_plan_tests::a_stopped_quick_worker_reports_once_with_its_outcome` |
| 5 | Worker rows showed no live activity, elapsed time or why a row was queued | `LiveWorker.activity/started_at` from the session's live turn; `WorkerView.depends_on/since`; a detail line per row ("Edit src/lib.rs · 3m 12s", "After task-1", "Waiting for a free worker slot") | `task_monitor_tests::a_worker_session_change_reaches_the_card…` |
| 6 | Soul editor: a render between save and reload reverted the editor to the stale copy and left a dead hash, so every later save conflicted; a real conflict had no way out | The saved version updates the loaded files at once and known hashes are remembered so a late reload never replaces the editor; a real conflict re-reads the disk, auto-recovers hash drift, else offers Keep mine / Take theirs | `monos::tests::the_soul_editor_keeps_its_saved_text…`, `a_real_soul_conflict_is_resolved…` |
| 7 | `git worktree add` (a full checkout) and `git status` ran while the app held its single store mutex, stalling UI-thread store reads | The tick first reads its launch candidates without Git (`Coordinator::launch_candidates`), creates their worktrees unlocked (`workspaces::warm_up`, idempotent), then the locked launch adopts them; the dirty check is cached for 60 s; decision-reply matching on every user message moved off the UI thread | `coordinator_workspace_tests::a_warmed_up_worktree_is_adopted…` |
| 8 | Regressions in the uncommitted work: a pending provider switch's internal stop stamped every queued Mono request as failed; project switches auto-launched provider processes probing every MCP server; a malformed `.mcp.json` blocked every Mono turn; an assignment without `repository` compared un-canonicalised roots and could skip overlap serialisation; proxy base URLs survived key stripping; capability tokens were redacted from tool output but not from echoed text | Only the user's own Stop holds the outbox (`stop_by_user`); MCP checks run only while the panel is open; an unreadable `.mcp.json` registers nothing; both repository forms canonicalise; `ANTHROPIC_BASE_URL`/`OPENAI_BASE_URL` go with their keys; text and reasoning events are redacted too | usage_footer, accounts, capabilities tests |
| 9 | `cargo fmt --check` failed on ten files and one mobile label assertion was stale | Formatted; assertion updated | `npm run native:check` gate |

## Validation

- Script tests: 29 passed.
- Core suite, GUI suite, rustfmt and strict Clippy: see the session report
  for the final counts (recorded after the gate ran).
- Dev install: release build, bundle, ZIP backup of the previous Dev
  executable, atomic replace, ad-hoc re-sign, bundle id and data mode
  unchanged, launched.

## Still open (not built here)

- Automatic per-repository integration for multi-repository tasks (a
  decision is raised instead).
- Worktree retention and clean-up, spending controls, deployment evidence.
- Phase 9 background execution on the remote host model.
- The remaining locked Git in launch (`git::snapshot` for the worktree
  cwd check, `handoff::completed_work`, `Profile::transfer` on an account
  change) is small per launch; the full prepare/run/record split of the
  launch path is the next step if it shows up in practice.
- `decision_now` in `refresh_cards`, `batch_failed`, `hand_off` and
  `route_task_questions` still lock the store on the UI thread (rare,
  short).
- The quick-worker path still has no restart test and `sessions.read` gives
  Mono only prose; Mono is told to inspect the worktree's changes.
