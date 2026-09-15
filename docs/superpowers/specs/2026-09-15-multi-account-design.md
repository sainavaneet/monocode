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

Linking happens at account creation and is repaired on app start if a target is
missing. On Windows, symlinks fall back to copies for files and junctions for
directories.

The user's existing `~/.claude` and `~/.codex` logins are the "Default" account
for each provider. Its profile dir is `null` and no variable is set, so current
behaviour is unchanged when only one account exists.

## Data model

New table `accounts` in the existing SQLite database:

```
id TEXT PRIMARY KEY, provider TEXT NOT NULL, label TEXT NOT NULL,
profile_dir TEXT, identity TEXT, created_at INTEGER NOT NULL, sort INTEGER NOT NULL
```

`sessions` gains `account_id TEXT` (nullable, null = default account). The
frontend `Session` type gains `accountId?: string`, and `HarnessSessionInput`
gains `accountId?: string`.

## Backend commands (Rust, `accounts.rs`)

- `accounts_list(provider?)` returns accounts with identity.
- `accounts_create(provider, label)` creates the profile dir, links shared
  config, inserts the row, returns the account.
- `accounts_rename(id, label)`, `accounts_remove(id)` (removes dir and, for
  Claude on macOS, the Keychain item).
- `accounts_env(id)` returns the env map for a session: `{}` for default,
  otherwise `{CLAUDE_CONFIG_DIR: dir}` or `{CODEX_HOME: dir}`.
- `accounts_identity(id)` reads the email: Claude from `<dir>/.claude.json`
  `oauthAccount.emailAddress`, Codex from the `id_token` JWT in `auth.json`.
- `accounts_login_ready(id)` returns whether a credential now exists (polled
  during login).
- `harness_spawn`, `harness_exec`, and `pty_spawn` accept an optional
  `env: HashMap<String, String>`. Keys are validated against the allowlist
  `{CLAUDE_CONFIG_DIR, CODEX_HOME}`; anything else is rejected.
- `fetch_claude_usage(profile_dir?)` reads the credential for that profile
  (hashed Keychain service or file) and refreshes tokens back to the same store.

## Frontend

- `src/lib/accounts.ts`: types, list/create/remove wrappers, default account per
  provider (localStorage, `monocode.defaultAccount.<provider>`), the
  `subscribe/snapshot` store pattern used elsewhere in `settings.ts`.
- `src/lib/accounts/selectFailover.ts`: pure function choosing the next account
  from the usage snapshot: same provider, not the failing one, lowest used
  percent across its windows, excluding accounts whose tightest window is at
  100 percent.
- `src/lib/accounts/rateLimitDetect.ts`: pure `isRateLimitFailure(provider,
  message)` matching "hit your limit", "usage limit", "rate limit",
  "rate_limit", "429", "too many requests", "quota".
- Settings → Providers: each Claude/Codex row gains an Accounts list showing
  label, email, 5-hour and weekly usage bars, a "Default" marker, rename,
  remove, and "Add account". Add opens the in-app terminal running
  `claude auth login` or `codex login` with the profile env, polls
  `accounts_login_ready`, then labels the account with its email.
- Settings → General: toggle "Switch account automatically on rate limit",
  default on (`monocode.autoSwitchAccount`).
- Composer model picker: an account row for Claude and Codex when the provider
  has more than one account. Selecting an account on a live session rebinds the
  session, kills the child, and respawns with the new env. Claude resumes with
  `--resume <claude session id>`; Codex resumes with `thread/resume`. Both work
  because the history folders are shared.
- Footer usage chips show usage for the active session's account.

## Automatic failover

When a turn fails and `isRateLimitFailure` matches:

1. If auto-switch is off, or this turn already failed over once, show the error
   as today plus the soonest reset time across accounts.
2. Otherwise refresh usage for the provider's accounts, call `selectFailover`.
3. If none: same as step 1.
4. Else rebind the session to the chosen account, respawn with resume, emit a
   `status` event "Switched to <label> after rate limit", and re-send the same
   turn text and attachments once.

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
