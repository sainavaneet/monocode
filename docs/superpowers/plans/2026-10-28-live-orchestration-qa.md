# Live orchestration QA — 2026-10-28

## Scope and build identity

Tested the current working tree with the full native check command and the running app through the project-scoped CLI supplied in this thread. The connected app is `/Applications/Monocode Native.app` (executable dated Oct 27 22:21); the handoff installed a different executable into `/Applications/Monocode Native Dev.app` (Oct 28 01:33). Live results below must not be attributed to that newer Dev build.

No production task was resumed, no project files were changed by test sessions, and no app was replaced or restarted. Five clearly titled QA sessions were retained for inspection: one deliberate draft and four submitted sessions, all idle at the final live snapshot.

## Live results

| Case | Result | Evidence |
| --- | --- | --- |
| Model discovery | Pass | Claude, Codex and Gemini advertised as available; other harnesses unavailable. Availability does not establish authentication. |
| Draft-only launch | Pass | Session 2281931375787323 remained idle with no turns and a saved draft. |
| Draft protection | Pass | Sending into the draft-only session was rejected and the draft preserved. |
| Real Codex response | Pass | Session 1567411258365502 returned exactly `MONOCODE_LIVE_QA_A_20261028`. |
| Follow-up to completed session | Fail | Three send attempts were rejected as busy/loading/draft despite an idle completed turn; cause of the rejection needs separate confirmation. |
| Draft status after submission | Fail | `sessions.read` reports `hasDraft:true`, while the shared database has `is_draft=0`, no draft block and an empty message queue. |
| Stop active turn | Pass | Session 895643637422233 was observed busy, Stop acknowledged, then idle; durable queue status was paused. This is an ordinary session Stop test, not a Mono lead-button test. |
| Claude response | Blocked | Session 7783687082880548 persisted `authentication_failed`. No assistant response. |
| Gemini response | Blocked | Session 1329875162275732 persisted a trust-policy error: the selected profile has not trusted this project for Full Access. |
| Error visibility to callers | Gap | `sessions.read` omitted the saved Claude/Gemini system error notices. Read-only inspection of only these QA session records established the reasons. |

Raw session snapshots: `/tmp/monocode-live-qa-20261028.json`.

## Automated validation

- `npm run native:check`: packaging/script tests 29 passed; formatting and strict Clippy passed; core 881 passed, 2 ignored. GUI 750 passed, 11 ignored. The whole command exited successfully before the new regression/fix.
- Core coverage includes durable dispatch, lease/idempotency, worktree isolation, dependencies, scope conflicts, recovery budgets, policy/authority gates, pause/resume/cancel, review, integration conflicts, verification and completion reporting. These are automated tests, not real-provider durable-plan runs.
- Added a regression for draft status after submitting an app-launched session. It failed on the current source: live and persisted draft state were empty, but `sessions.read` returned true. Fixed the shell’s draft-session cache to track `SessionChanged` events. This source fix has not been installed into either running app. Follow-up rejection in the older live binary remains a separate finding.
- Full gate log: `/tmp/monocode-native-check-20261028.log`.
- Regression log (expected failure before fix): `/tmp/monocode-draft-regression-20261028.log`.
- After fix: all 13 app-command tests passed, including the new regression (`/tmp/monocode-draft-fixed-20261028.log`). Formatting, strict Clippy and `git diff --check` passed again.
- Final full GUI rerun after the fix: **751 passed, 0 failed, 11 ignored**, exit 0. Log: `/tmp/monocode-gui-final-20261028.log`. The ignored cases are nine Metal renderer checks, one remote-host fixture, and one manual frame-cost measurement.

## Coverage limits

This thread has ordinary project Operator access, not a Mono-scoped task controller or desktop UI automation. It cannot start/drive a durable Mono plan through the provided actions. No new live proof of plan Start, dependency scheduling, review/integration, final-report delivery, restart recovery, account failover, or rendered task controls was obtained. The earlier audit documents previous evidence; it is not counted as a fresh pass. Provider authentication and Gemini trust settings were left unchanged. Remote fixtures and optional hardware/performance tests are separate from the default gate.
