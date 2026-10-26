# API key billing

Port the fork's per-provider API keys to the native app with the same
semantics: a saved key per provider, a global "Use API key" switch per
provider, and billing of the next turn to that key instead of the login.

## Behaviour (matches the React app)

- Providers: Claude Code (`ANTHROPIC_API_KEY`) and Codex (`OPENAI_API_KEY`).
- Keys are stored in `<data dir>/api-keys/<provider>` with mode 0600. The
  release native app shares the data directory with the Tauri app, so keys
  saved in either app are visible to both. The UI only ever learns whether a
  key exists and its last four characters.
- "Use API key" is one switch per provider, stored in native preferences. It
  applies to every local conversation and Mono of that provider on its next
  turn and keeps the same conversation. Remote sessions ignore it.
- Turning the switch on requires a saved key; otherwise the switch stays off
  and the UI says "Add an API key in Settings → Providers first."
- At launch, when the switch is on, the key is put in the CLI's environment
  after the account profile has stripped inherited keys. If the switch is on
  but no key is saved, the turn fails before launch with "Use API key is on for
  <provider>, but no key is saved. Add one in Settings › Providers."
- Codex additionally gets `-c model_provider="monocode-api"` and the
  `monocode-api` provider definition, because app-server ignores the key when a
  ChatGPT login is cached. Thread start and resume pass `modelProvider`:
  `monocode-api` while billing the key, otherwise the `model_provider` from the
  account's `config.toml` (default `openai`), so a rollout saved under API
  billing resumes correctly on the plan and vice versa.
- A warm (parked) CLI is not reused when the billing mode differs from the one
  it was started with.
- UI: an "API keys" group in Settings → Providers with a row per provider
  (saved-key status, key entry with Save, Clear, Use API key toggle); a compact
  "API" toggle button beside the footer usage chip for Claude and Codex that
  shows pressed while key billing is on; the same switch and key entry in the
  Mono settings dialog's General section.

## Code

- `monocode-core/src/api_keys.rs`: `env_var(kind)`, `path(data_dir, kind)`,
  `read`, `status` (configured + hint), `set` (0600, rejects whitespace),
  `clear`, `apply(command, kind, key)`, `codex_model_provider(home)`.
- `agent::Settings.api_key: Option<String>` with `#[serde(skip)]`, resolved by
  the conversation at launch from the preference and the key file, never
  persisted. `provider.rs` applies it after `account.apply` and sets
  `modelProvider` on Codex thread params. `parked::Binding` and the idle-pool
  reuse check include whether a key is in use.
- `settings::Preferences.api_key_enabled: HashMap<String, bool>` keyed by
  provider id, with `api_key_enabled(kind)`.
- `settings/pages/providers.rs`: "API keys" group. `usage_footer.rs`: the API
  toggle. `monos/panels.rs`: General section row.

## Tests

Core: path and env mapping, set/read/clear round trip with 0600, whitespace
rejection, hint, Codex args, model provider from config, parked reuse split by
billing mode, Codex thread params carry `modelProvider`. GPUI: launch resolves
the key or fails with the message, preference round trip, footer toggle
on/off and the no-key note, settings group renders status and saves a key,
Mono row toggles the same preference.
