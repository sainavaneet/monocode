# Multi-account support for Claude Code and Codex

Date: 2026-09-15
Status: approved

## Goal

Let a user log in to several Claude Code accounts and several Codex accounts,
choose which account a session runs on, see each account's usage, and have a
session fail over to another account automatically when it hits a rate limit.

## Non-goals

- Accounts for providers other than Claude Code and Codex.
- Sharing credentials between machines.
- Changing the app bundle identifier.

## Mechanism

Both CLIs read their login from a directory chosen by an environment variable:

| Provider | Variable | Credential location |
| --- | --- | --- |
| Claude Code | `CLAUDE_CONFIG_DIR` | macOS Keychain item `Claude Code-credentials-<hash>` where `hash = sha256(dir).hex[0..8]`, else `<dir>/.credentials.json` |
| Codex | `CODEX_HOME` | `<dir>/auth.json` |

MonoCode already spawns every session as a child process with a controlled env
(`harness_spawn`). Each extra account gets its own profile directory, and the
session that uses it gets the matching variable in its env. Nothing global
changes, so two sessions can run on two accounts at once.

## Profile directories

Location: `<app data dir>/accounts/<provider>/<account id>/`.

Only credentials live there. Everything else is linked back to the user's real
config dir so all accounts behave the same and history is shared:

- Claude: symlink `CLAUDE.md`, `settings.json`, `agents`, `commands`, `hooks`,
  `plugins`, `rules`, `skills`, `projects`, `history.jsonl` (if present). Copy
  `~/.claude.json` once with `oauthAccount` and `userID` removed so MCP servers
  and onboarding state carry over. Never link `.credentials.json`.
- Codex: symlink `config.toml`, `AGENTS.md`, `skills`, `rules`, `hooks.json`,
  `plugins`, `sessions`, `memories` (if present). Never link `auth.json`.

Linking happens at account creation and is repaired whenever accounts are
listed (Settings, picker) if a target appeared since. On Windows, symlinks fall back to copies for files and junctions for
directories.

The user's existing `~/.claude` and `~/.codex` logins are the "Default" account
for each provider. Its profile dir is `null` and no variable is set, so current
behaviour is unchanged when only one account exists.

## Data model

Extra accounts are stored as JSON in `<app data dir>/accounts/accounts.json`
(`id`, `provider`, `label`, `profileDir`, `createdAt`). The default account for
each provider is synthesised at read time and never stored. Identity (email)
is read live from the profile's `.claude.json` or `auth.json`.

A session's account id lives in its existing `modelSettings` under the key
`account`. It therefore persists with the session, flows to the adapters
through `HarnessSessionInput.modelSettings`, and needs no schema change.
`mergeModelSettings` preserves the key across model changes, and the
"last used settings" memory never records it, so new sessions start on the
provider default chosen in Settings.

## Backend commands (Rust, `accounts.rs`)

- `accounts_list()` returns every account (defaults first) with identity, and
  repairs missing shared links for each profile while it is at it.
- `accounts_create(provider, label)` creates the profile dir, links shared
  config, inserts the row, returns the account.
- `accounts_rename(id, label)`, `accounts_remove(id)` (removes dir and, for
  Claude on macOS, the Keychain item).
- `accounts_list` includes each account's identity: Claude from
  `<dir>/.claude.json` `oauthAccount.emailAddress`, Codex from the `id_token`
  JWT in `auth.json`. The frontend derives the env map from `profileDir`.
- `accounts_login_ready(id, provider)` returns whether a credential now exists
  (polled during login).
- `harness_spawn` and `harness_exec` accept an optional
  `env: HashMap<String, String>`. Keys are validated against the allowlist
  `{CLAUDE_CONFIG_DIR, CODEX_HOME}`; anything else is rejected.
- `fetch_claude_usage(profile_dir?)` reads the credential for that profile
  (hashed Keychain service or file) and refreshes tokens back to the same store.

## Frontend

- `src/lib/accounts.ts`: types, list/create/remove wrappers, default account per
  provider (localStorage, `monocode.defaultAccount.<provider>`), the
  `subscribe/snapshot` store pattern used elsewhere in `settings.ts`, and
  `accountIdFor(provider, modelSettings)` which resolves the effective account.
- `src/lib/accounts/selectFailover.ts`: pure function choosing the next account
  from the usage snapshot: same provider, not the failing one, lowest used
  percent across its windows, excluding accounts whose tightest window is at
  100 percent.
- `src/lib/accounts/rateLimitDetect.ts`: pure `isRateLimitFailure(message)`
  matching "hit your limit", "usage limit", "rate limit",
  "rate_limit", "429", "too many requests", "quota".
- Settings → Providers: each Claude/Codex row gains an Accounts list showing
  label, email, 5-hour and weekly usage bars, a "Default" marker, rename,
  remove, and "Add account". Add opens an embedded terminal (the normal shell)
  and types `export CLAUDE_CONFIG_DIR='<dir>' && claude auth login` (or the
  Codex / PowerShell equivalent) into it, polls `accounts_login_ready`, then
  shows the account's email.
- Settings → General: toggle "Switch account automatically on rate limit",
  default on (`monocode.autoSwitchAccount`).
- Composer model picker: an account row for Claude and Codex when the provider
  has more than one account. Selecting an account on a live session rebinds the
  session, kills the child, and respawns with the new env. Claude resumes with
  `--resume <claude session id>`; Codex resumes with `thread/resume`. Both work
  because the history folders are shared.
- Footer usage chips show usage for the active session's account.

## Automatic failover

Implemented in `src/lib/accounts/failover.ts` and applied in
`sendHarnessTurn` (`src/lib/harness/registry.ts`), so the app's transcript and
turn bookkeeping are untouched:

1. The turn runs with a wrapped `onEvent`. A `session.error` whose message
   matches `isRateLimitFailure` is held back; a thrown rate-limit error is
   caught.
2. If auto-switch is off, the provider has no accounts, or the failure is not a
   rate limit, the held error is delivered (or the error rethrown) unchanged.
3. Otherwise usage is refreshed for every account of the provider and
   `selectFailover` picks the one with the most quota left.
4. If none qualifies, the original error is delivered unchanged.
5. Else the wrapper emits `status` "Switched to <label> after a rate limit",
   emits `session.configChanged` with `modelSettings.account = <id>` so the
   session, picker and footer follow the new account, and calls the adapter's
   `sendTurn` once more with the new settings. The adapter's settings key
   includes the account, so it respawns under the new login and resumes the
   same conversation. Only one retry per turn.

## Error handling

- Login never completes: the terminal stays open; the account row shows
  "Not signed in" and can be removed.
- Profile dir link targets missing: skip that link, log, continue.
- Env allowlist violation: command returns an error, session does not start.
- Respawn after switch fails: emit `session.error` with the message and leave
  the session bound to the new account so the user can retry.

## Testing

- Vitest: `rateLimitDetect`, `selectFailover`, account store persistence, link
  plan builder (which paths, which are excluded), picker rendering with one vs
  many accounts.
- Rust: env allowlist, profile dir creation and link plan on a temp dir,
  Keychain service hash, identity extraction from sample `.claude.json` and
  `auth.json`.
- Manual: log in two accounts per provider, start sessions on each, force a
  rate-limit error, confirm switch, resume, and status line.

## Build order

Phase 1: data model, backend commands, env plumbing, login flow, per-session
account selection, per-account usage.
Phase 2: live switching and automatic failover.
