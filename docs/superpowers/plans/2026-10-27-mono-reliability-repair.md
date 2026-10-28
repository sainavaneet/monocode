# Mono reliability repair

## Outcome

A Mono opened in the airobotics workspace can use the project's saved MCP configuration, assign work explicitly to DigitalTwin or cellforge, select only verified eligible accounts, run independent assignments concurrently in isolated worktrees, stop work predictably, and recover its saved proposal and task after restart. Validate in Dev before promoting to Main.

## Confirmed failures

- The saved 12-assignment Digitaltwin task is blocked before any recorded worker attempt. Both allowed Claude accounts are marked authentication-failed.
- Default account application returns before clearing inherited API environment overrides. An API-off subscription turn can therefore launch with an unintended API key.
- Assignment prompts target DigitalTwin and cellforge, but workspace preparation uses task.project (knowledge-graph). The first prepared worktree is in the wrong repository.
- The saved prompts tell concurrent workers to share named branches. This conflicts with the coordinator's dedicated per-assignment worktrees and needs an explicit integration strategy.
- macOS Network.framework reports localNetworkDenied for MonoCode children while Terminal reaches the Jetson. Jetson MCP responds locally; this is separate from missing registration and account authentication.
- Main and Dev are ad-hoc signed and share the copied executable's Mach-O UUID. Signing/duplicate identity is a suspected contributor, not a proven sole cause. No valid code-signing identity is installed.
- Extra indexed build bundles were archived and unregistered. Exactly three indexed installed applications remain.
- Proposal persistence and a dedicated Mono Stop reply button are already implemented and live-tested; stopping a reply is not the same as cancelling delegated workers.

## Ordered implementation

### 1. Authentication and launch preflight

- Remove inherited Claude/Codex API overrides for account-backed launches; explicit API mode reapplies only the selected stored key afterward.
- Preserve named-account isolation, OAuth credentials, project configuration and injected MonoCode tools.
- Test subscription/API-mode separation with fake credentials, never print real keys.
- Validate each account through a bounded provider check. Clear persisted authentication-failed health only after successful verification of that exact account and billing mode.
- Show account name, provider, failed check and recovery action. Do not equate a stale failure flag with a fresh sign-in failure.
- Choose an eligible account before creating a workspace. Check initial provider fallback as well as failure recovery; an enabled fallback switch still respects the allowed-account list.

### 2. Explicit repository and base routing

- Add durable optional repository and base-ref fields to assignments, including plan parsing, persistence, review card, steering and restart recovery.
- A single-repository task may default to its validated project. A parent-folder or multi-repository task must explicitly select a repository per assignment.
- Canonicalize and validate the target against Mono's assigned projects or explicitly approved workspace descendants. Reject ambiguity and symlink escapes before creating worktrees.
- Create isolated worktrees in the selected repository from the reviewed base commit. Display repository, base and branch in the proposal and live task.
- On resume, reject an existing workspace whose repository/base does not match the assignment; preserve its files and explain how to recover.
- Replace instructions to concurrently check out one shared branch with isolated branches and an explicit, ordered integration step. Successors must receive the verified predecessor commits they depend on.

### 3. MCP status and recovery

- Continue using each project's saved provider-native configuration. Never automatically substitute knowledge-graph's graph for another project or assume Claude .mcp.json applies to Codex.
- For a parent workspace, show the selected MCP configuration source explicitly. Require an explicit source when multiple assigned projects differ; never silently merge server names.
- Distinguish registration missing, project approval needed, OS local-network denial, endpoint unreachable, authentication required, and healthy connection with tools.
- Label the footer result as a timestamped independent probe; do not present it as the live conversation connection.
- Recheck on user request and relevant project/configuration change; bounded retry for transient connection failures, no retry loop for permission or authentication failures.
- Reinitialize the provider safely when context changes or a failed connection requires it. Do not restart a running tool or repeat a side-effecting request silently.
- Verify initialization, tools/list and one harmless actual tool call using a local fixture, then the real digital-twin endpoint when macOS allows access.

### 4. Installation identity and network permission

- Keep exactly three installed bundles, with separate Main/Dev bundle identifiers and data directories.
- Adopt a consistent valid signing identity when available. Do not create trust changes or reset the system privacy database silently.
- Investigate Main/Dev executable UUID collision and verify independent permission attribution; do not label changing a UUID as a proven permission fix without testing.
- Package temporary build bundles outside indexed application locations and archive old bundles as ZIP files; never leave extra registered .app backups.
- Verify signature, bundle identity, data mode and Local Network access after install, restart and a subsequent upgrade.
- If the OS still denies access, expose the precise reason and leave remediation explicitly blocked rather than claiming MCP is fixed.

### 5. Stop, status, parallelism and restart recovery

- Keep Stop reply available while drafting and awaiting tool/approval responses; show stopping until the provider settles.
- Provide a separate explicit pause/cancel-workers action with outstanding-worker counts. Pause prevents new dispatch; cancellation waits for worker termination confirmation.
- Test that queued internal notifications cannot silently restart a user-stopped task.
- Show planning, queued, waiting for dependency, running, needs input, blocked, stopping, completed, and failed from durable state. Collapse completed task details.
- Dispatch independent eligible assignments up to the configured worker limit, while serializing overlapping writes and integration within a repository.
- Restore saved proposals and durable tasks without reanalysis, duplicate sessions, duplicate worktrees or auto-starting a dismissed proposal.

### 6. Recover the existing Digitaltwin task

- Back up task records before any data repair. Preserve all 12 prompts, dependencies, criteria and history.
- Keep it blocked until account verification and repository preflight pass.
- Map DigitalTwin assignments to DigitalTwin and CellForge assignments to cellforge with reviewed base commits.
- Preserve the existing knowledge-graph worktree. Retire its task association only after verifying no worker ran and no work would be lost.
- Review integration and external-action policy: current task policy disallows push/deploy even though a later prompt requests push and a draft PR. Surface this mismatch; do not silently expand permissions.
- Show the repaired proposal and preflight results before resuming the real engineering work.

## Acceptance checks

1. Inherited invalid API key plus valid subscription cannot override API-off; API-on still uses the saved key; named accounts stay isolated.
2. Two temporary Git repos under one parent get correct isolated worktrees and bases; wrong/missing/unauthorized repositories fail before side effects.
3. Dependency outputs reach successors; independent repository work overlaps in time without shared-branch races.
4. Unavailable accounts create no workspace/session; verified allowed fallback launches; no account outside the allowlist is selected.
5. Local test MCP loads from the chosen source, lists and calls a tool; unreachable, missing, auth and OS-denied states remain distinct; injected tools survive.
6. Stop during reasoning, command execution, permission wait and delegated work behaves as labelled; draft and saved plan survive.
7. Quit/reopen preserves state with no duplicate launch or reanalysis; recovery works after a simulated interrupted launch.
8. Upgrade Main and Dev independently with data preserved and only three installed apps; recheck actual local-network access after each upgrade.

## Current status (2026-10-27)

Implemented and installed in Monocode Native Dev (2026-10-27 20:00 KST):

- Account-backed launches remove inherited API-key overrides; explicit API mode reapplies the selected key. Account selection precedes workspace creation; cross-provider fallback still requires an explicit allowed account.
- Optional repository/base-ref routing persists across restart, appears in plan review, validates workspace boundaries, and launches isolated worktrees in the selected repository. Same relative file scopes in different repositories can run concurrently.
- Structured successors refuse a base missing predecessor commits. Multi-repository integration raises a decision rather than attempting a cross-repository merge. Automatic per-repository integration is not yet implemented.
- MCP popover refreshes on opening and labels the result as a timestamped independent check, not the live chat connection.
- Intermediate macOS bundles use bundles.noindex. Packaging tests: 21 passed.

Verified live:

- Jetson MCP now responds from the MonoCode process environment: initialize, 31 listed tools, and successful read-only get_sync_status. Earlier localNetworkDenied was real; present success does not prove stable permission attribution after future reinstalls.
- Default Claude subscription probe completed AUTH_OK. The named assigned profile remains signed out. No persisted account health has been cleared.
- Existing Stop/proposal restoration fixes were already installed and live-tested in Main and Dev.
- Updated Dev reopened with saved conversations/tabs. Its airobotics MCP popover showed digital-twin connected at 20:00:46, explicitly labelled as an independent Claude check. Main app and production task data were not changed.
- Dev executable updated atomically after quitting the idle app, with a verified ZIP backup. Ad-hoc signature validation passed; bundle identifier and dev-data mode were unchanged. Installed executable SHA256: 0cc6a27b870553d779c74f1b7f6b93e73bcff3df93ae638535f929902983bcb7.

Test progress:

- Core suite: 871 passed, 2 ignored. Final routing suite after an additional checkout/base regression test: 8 passed.
- Full GUI run under concurrent build load: 731 passed, 10 failed, 11 ignored. Eight failures passed isolated reruns. Large TSX analysis reproduced its timeout in isolation; increased its input-scaled budget within the existing 30-second cap and the existing large-file regression now passes. Split animation regression now passes with deterministic setup and a fixed animation frame. MCP footer (5) and proposal display (3) targeted tests passed.

Still outstanding:

- Production task data repair and reviewed resume. Its 12 prompts/history and existing clean knowledge-graph worktree are preserved; no production DB mutation or worker launch was made.
- Automatic multi-repository dependency integration, richer MCP failure classification and safe live-provider reconnect.
- Stable signing identity / independent macOS network-permission attribution across upgrade. No valid signing identity is installed; ad-hoc signing is not a permanent permission guarantee.
- Full acceptance matrix for stop during every permission/tool state and upgrade/reopen scenarios.
- Existing production policy still disallows merge, push and deploy. Do not silently widen it or resume the task as though those actions were authorized by the policy.

Do not describe the overall repair as complete yet.
