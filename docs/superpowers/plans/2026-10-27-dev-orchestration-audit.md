# Dev orchestration audit — 2026-10-27

Scope: Monocode Native Dev only; a dedicated Orchestration QA Mono, the real airobotics project for read-only MCP checks, and disposable repositories under `airobotics/.monocode-qa-20261027`. No production task was resumed or changed. Main was not updated during this audit.

## Reproduced defects and fixes

1. A successful account-backed conversation did not clear that account's earlier authentication failure, so worker eligibility could remain blocked. Successful, non-cancelled subscription turns with an actual assistant response now refresh only the exact account used. API-key turns cannot validate subscription credentials; newer failures and quota cooldowns are preserved.
2. Worker instructions used an unqualified `monocode-gpui` command that was absent from PATH. The first live worker hit command-not-found and searched its environment. Briefs now use the quoted absolute executable path and explicitly prohibit printing environment credentials.
3. The worker reporting schema did not explain field types or commit formatting. The first worker submitted an invalid validation value. Briefs now specify arrays of strings, empty arrays, and commit hashes without subjects.
4. Planner instructions called verification entries “check descriptions,” although the coordinator executes them as commands. The first plan produced `cat beta.txt prints exactly BETA_QA_OK`; the coordinator treated the prose as filenames and retried correct work. Planner instructions now require direct executable commands without shell syntax or prose suffixes, with concrete examples; worker validation must include each exact command.
5. Tool output could persist the app's own ephemeral capability tokens. A test worker printed them while trying to locate the executable. Dev was quit to invalidate those tokens. Known app/visual capability values are now redacted from tool input, output and metadata before persistence/display. This protects known capability values in tool snapshots; it is not a general guarantee against arbitrary secrets or encoded/fragmented disclosure.

## Automated validation

- Full core suite after fixes: 874 passed, 2 ignored.
- Mono UI suite: 66 passed.
- Runtime suite after fixes: 31 passed, 1 ignored.
- Regression coverage includes exact-account/billing-mode isolation, newer auth failures, cooldown preservation, executable path quoting, capability redaction, first-turn/resumed/changed-project runtime context.
- Release build and Dev bundle signature verification passed.

## Live evidence

- Initial real airobotics MCP request: registration discovered, digital-twin connection failed; ToolSearch exposed EHOSTUNREACH. Claude's diagnosis matched the saved tool evidence. A separate configured Codex MCP executable was missing; unrelated registrations were preserved.
- Changed the QA Mono's sole assignment to repo-a: mono_probe connected and its real ping tool returned `mono-local-probe:repo-a`. MonoCode visual tools remained discoverable.
- First two-worker plan exposed the reporting and verification bugs above. The default worker limit of one was identified; setting two and applying the policy worked. This first run was cancelled, with its history/worktrees preserved.
- Restart restored the Mono and cancelled task without relaunching that task (three historical attempts remained three).
- Corrected gamma/delta plan used `grep -qx GAMMA_QA_OK gamma.txt` and `grep -qx DELTA_QA_OK delta.txt`. Both workers entered Running at the same persisted timestamp, 1793102089765, in separate worktrees.
- Pause moved both attempts to Interrupted; Resume created one replacement per assignment, both at timestamp 1793102141385. Both replacements succeeded; both assignments were Accepted with no review findings, malformed reports, command-not-found errors or tool errors.
- Corrected task was cancelled after both assignments were accepted, before integration. No merge/push/deploy was performed. Main QA checkout marker files remained absent.
- Dev-launched Network.framework probe returned TCP_READY for the Jetson MCP port.
- Returned the QA Mono to sole assignment airobotics. Provider reported digital-twin connected from the assigned project and the real get_sync_status tool completed with a nonempty response and no error flag. Injected monocode_visual remained connected.

## Limits and remaining work

- Final Dev UI check showed the QA conversation idle (Send present, no Stop). Live lead-turn cancellation is not confirmed: a standalone delay command was rejected by the provider, one text response completed before cancellation, and subsequent UI interactions were interrupted by app-state changes. Worker Pause/Resume/Cancel and automated stop regressions passed; these are distinct from a live lead-command Stop test.
- Intermittent macOS/network access remains unproven across future upgrades. One successful real MCP call does not establish a permanent fix for the earlier EHOSTUNREACH/localNetworkDenied incidents.
- Airobotics startup reported over 12 million added lines. Performance/source of that count has not been established; no changes were made to Git ignore rules or production files.
- Multiple-provider selection/fallback, dependency routing and authority gates passed automated tests. This audit's live workers used Claude; it did not live-test every provider or perform production integration/deployment.
- Scratch artifacts and cancelled QA histories are retained for inspection. Existing graph registrations and the knowledge-graph .mcp.json were not modified.
