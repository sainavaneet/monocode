# Mono project MCP loading fix

## Confirmed cause

Digitaltwin (`9a14c2a8d35b6dc70893ec4592520f9b`, session `6829932253101104`) has `/Users/navaneet/Developer/airobotics/knowledge-graph` assigned. Mono chat creation uses HOME, and the previous runtime passed the saved chat cwd through to the provider without consulting the assignment. Claude therefore searched the wrong project configuration scope. MonoCode's injected `--mcp-config` is additive: the live probe confirmed it coexists with native project registrations when cwd is correct.

## Behavior

- One assigned project: canonical existing project directory is the provider launch cwd, including sessions previously saved at HOME. Composer discovery follows the assigned directory.
- Multiple projects: conversation launch is blocked with an explicit instruction to assign exactly one. No implicit first-project choice or config merge.
- No assigned project: retain the saved conversation directory, shown in the UI.
- A persisted receipt binds provider context to directory, provider, account and project configuration digest. A changed binding starts fresh provider context and supplies Mono's existing transcript brief; saved conversation history remains intact. Unchanged bindings resume normally. Existing sessions migrate on their first updated turn.
- Provider process reuse includes the project configuration digest. Config changes cannot reuse a parked process with an old tool inventory.
- Changing assignments during a running turn shows a restart-required notice and prevents queued work from being steered into the old project. Stop the turn or let it finish; the next turn uses the new assignment.
- Claude loads its own `.mcp.json` and `.claude` settings. Existing settings sources, account configuration and injected visual MCP remain intact. A project registration using reserved `monocode_visual` is rejected explicitly.
- Codex keeps its native `.codex/config.toml` and project trust behavior. Claude JSON is never imported into Codex.
- An expandable `MCP connection status` transcript entry shows configuration path/presence, provider-discovered registrations, connection state and exposed tool names. Claude's reported source is retained when supplied. Configuration presence alone is not labeled a successful connection. Codex uses its thread inventory API; older versions without it report unknown status without aborting the turn. The Codex report currently covers one inventory page (up to 100 servers).

## Verification

- Runtime fixture: first launch in assigned project; unchanged context resumes; project/config changes reset context; injected visual config and Claude settings-source arguments remain present.
- Unit coverage: multi-project rejection, invalid path, provider-specific config digest, reserved-server conflict, distinct missing-registration/connection-failure states, secret-free diagnostics, optional Codex inventory failure.
- Manual real Claude/local MCP probe: three successful turns (project alpha, resumed alpha, then project beta), each calling `mcp__mono_probe__ping` and injected `mcp__monocode_visual__visual_check`. The local server call log independently recorded alpha/alpha/beta.
- Deliberately unreachable local HTTP registration was reported with source `project` and `connection failed (registration discovered)`. Working local registration was source `project`; injected registration was source `dynamic`.
- Repeatable opt-in runner: `crates/monocode-core/examples/mono_mcp_probe.rs`. This runs separately from cargo tests, whose real-provider launch guard remains intact.
- Knowledge-graph `.mcp.json` SHA-256 was checked before/after and is unchanged. No user-scope registration or alternate graph server was used as a workaround.

The Jetson's current reachability is not established by this fix or by the local probe. A working configuration loader does not establish network connectivity. Live provider testing covered Claude; Codex diagnostics were checked against the installed app-server schema and regression tests, not a live Codex tool call.

## Final result

Installed and relaunched Monocode Native Dev; verified its project configuration notice in the accessibility tree and screenshot. Main app binary unchanged. Core suite: 857 passed, 2 ignored. GUI suite: 733 passed, 11 ignored. Additional running-project-change GUI regression: 1 passed. Final all-target clippy with warnings denied, formatting, diff checks and optimized release build passed. Installed bundle signature verified.

Dev bundle backup: `/Applications/Monocode Native Dev (before MCP fix 20261027-153211).app`.
Dev database backup: `/Users/navaneet/Library/Application Support/Monocode Backups/dev-mcp-20261027-153440`.

## Automatic project connection initialization

The footer now starts a native MCP check when the selected project/provider/account changes, without waiting for a model prompt. It uses only that project's provider-native registrations for display. Claude runs `mcp list`; Codex initializes an ephemeral native app-server thread in the project and reads `mcpServerStatus/list`. Neither sends a model turn or calls a project tool. Provider trust and project MCP approvals are preserved. The check process is bounded and cleaned up; actual chat connections remain owned by the conversation's provider session.

Changing projects cancels the old check, clears its results and rejects late completion. The panel exposes Check connections for retries. Account profiles are applied to the native subprocess. The panel no longer substitutes global registrations for an empty project configuration.

Live local verification: Codex connected beta_graph and reported offline_probe as a connection failure. The fixture recorded initialize, tools/list and resource discovery, with zero tools/call requests. Claude reported project approval required for the unapproved local fixture. No user MCP configuration or trust settings were changed.

Final automatic-check validation: four footer tests passed, including automatic initialization/cancellation on project changes. A trusted isolated Codex fixture returned beta_graph connected and offline_probe connection failed. An untrusted fixture was rejected with an explicit project-trust error before thread initialization. Codex config/read must confirm the current project layer is enabled, preventing a global same-named server from masking ignored project configuration. No model turn was submitted. The protected knowledge-graph .mcp.json checksum remains unchanged.
