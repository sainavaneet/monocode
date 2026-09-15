# API-key accounts for Claude Code and Codex

Date: 2026-09-15
Status: approved
Extends: 2026-09-15-multi-account-design.md

## Goal

Let the user add an API key for Claude Code (Anthropic) or Codex (OpenAI) as
an account, keep it switched off until explicitly enabled, pick it per session
like any other account, and see plainly in the footer when a session is
billing the API.

## Decisions

- **Manual only.** Automatic rate-limit failover never selects an API-key
  account, even when enabled. The user picks it in the model picker.
- **Off by default.** A new API-key account has `enabled: false`. While off it
  is hidden from the picker; sessions that reference it fall back to the
  provider default.
- **Secret never reaches the frontend.** The key is stored in the macOS
  Keychain (service `MonoCode API Key`, account `<provider>:<id>`), with a
  `0600` file `<profile>/api_key` as fallback where the Keychain is not
  available. `accounts_list` returns only a hint: the last four characters.
- **Rust injects the key.** `harness_spawn` takes an optional
  `account: { provider, id }`. After the allowlisted env is applied, the
  backend looks the account up and sets `ANTHROPIC_API_KEY` or
  `OPENAI_API_KEY` itself. The frontend env allowlist is unchanged.

## Data model

`Account` gains `kind: "subscription" | "apiKey"` (default `subscription`
when absent) and `enabled: boolean` (default `true` when absent). API-key
accounts also store `hint: string` (last four characters, non-secret).

## Profile directory

An API-key account gets a profile directory like any other account, with the
same shared links, so CLAUDE.md, plugins, skills and history apply. It holds
no OAuth credential, so the env key is the only auth the CLI finds. For
Claude the seeded `.claude.json` also contains
`customApiKeyResponses.approved = [<last 20 chars>]`, which is how Claude Code
records that the user approved this key, so no interactive prompt blocks the
first turn.

## Backend commands

- `accounts_create_api_key(provider, label, key)` returns the account.
- `accounts_set_enabled(id, provider, enabled)`.
- `accounts_remove` also deletes the Keychain item or key file.
- `harness_spawn(..., account?)` injects the secret env for API-key accounts.

## Frontend

- `usableAccountsFor(provider)` returns enabled accounts; the picker and the
  footer's "has choice" logic use it. Settings lists every account.
- `selectFailover` skips `kind === "apiKey"` and disabled accounts.
- `accountIdFor` treats a disabled account like an unknown one.
- Settings → Providers: "Add API key" opens an inline form with a name and a
  masked key field. API-key rows show an Enabled toggle, the hint, Rename and
  Remove. No usage fetch, no Sign in.
- Model picker option label: `<label> · API key`.
- Footer chip for an API-key session: provider icon, account label, and an
  amber `API` badge with the text "pay as you go" in place of usage bars. No
  usage request is made for API-key accounts.

## Testing

- Rust: kind/enabled serde defaults, hint derivation, secret file fallback
  round-trip, env injection picks the right variable, `.claude.json` seeding
  includes the approval entry.
- Vitest: `usableAccountsFor`, `accountIdFor` with a disabled account,
  `selectFailover` excluding API-key accounts, `spawnChild` passing `account`,
  `accountModelSetting` label.
