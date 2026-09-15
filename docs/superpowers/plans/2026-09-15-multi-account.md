# Multi-Account Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let MonoCode hold several Claude Code and Codex logins, run each session on a chosen account, show per-account usage, and fail over to another account automatically when a turn hits a rate limit.

**Architecture:** Each extra account is a profile directory under the app data dir. A session that uses it gets `CLAUDE_CONFIG_DIR` or `CODEX_HOME` in its child env when `harness_spawn` runs. The chosen account id rides in the session's existing `modelSettings` under the key `account`, so it persists with no schema change and reaches the adapters through `HarnessSessionInput.modelSettings`. Shared config (CLAUDE.md, plugins, sessions, ...) is symlinked from the real config dir so every account behaves alike and resume works across accounts.

**Tech Stack:** Rust (Tauri 2, serde_json, sha2), TypeScript/React 19, Vitest, cargo test.

## Global Constraints

- Only providers `claude` and `codex` get accounts.
- Allowed child env keys: exactly `CLAUDE_CONFIG_DIR` and `CODEX_HOME`. Anything else is rejected.
- Claude Keychain service for a profile dir: `Claude Code-credentials-` + first 8 hex chars of sha256 of the dir path.
- Never link `.credentials.json` (Claude) or `auth.json` (Codex) into a profile.
- The account id key in `modelSettings` is the literal string `account`.
- Default account per provider has id `default`, `profileDir: null`, and sets no env.
- Files stay under 800 lines; new UI goes in new files, not appended to `SettingsView.tsx`.
- Commit messages: `<type>: <description>` with types feat, fix, refactor, docs, test, chore.
- All Bash commands use absolute paths.

---

### Task 1: Rust `accounts` module — env allowlist, keychain hash, link plan, JSON store, commands

**Files:**
- Create: `src-tauri/src/accounts.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod accounts;`, `.manage(accounts::AccountStore::default())`, register commands)
- Modify: `src-tauri/Cargo.toml` (add `sha2 = "0.10"`)

**Interfaces:**
- Produces:
  - `pub const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR"`, `pub const CODEX_HOME_ENV: &str = "CODEX_HOME"`
  - `pub fn validate_env(env: &HashMap<String, String>) -> Result<(), String>`
  - `pub fn apply_env(cmd: &mut std::process::Command, env: Option<&HashMap<String, String>>) -> Result<(), String>`
  - `pub fn claude_keychain_service(profile_dir: &Path) -> String`
  - `pub struct Account { id, provider, label, profile_dir: Option<String>, identity: Option<String>, created_at: i64 }` (serde camelCase)
  - Tauri commands: `accounts_list`, `accounts_create(provider, label)`, `accounts_rename(id, label)`, `accounts_remove(id)`, `accounts_login_ready(id) -> bool`

- [ ] **Step 1: Add the sha2 dependency**

In `src-tauri/Cargo.toml`, under `[dependencies]` after `base64 = "0.22"` add:

```toml
sha2 = "0.10"
```

- [ ] **Step 2: Write the failing Rust tests**

Create `src-tauri/src/accounts.rs` with only the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn validate_env_accepts_allowlisted_keys() {
        let mut env = HashMap::new();
        env.insert(CLAUDE_CONFIG_DIR_ENV.to_string(), "/tmp/a".to_string());
        env.insert(CODEX_HOME_ENV.to_string(), "/tmp/b".to_string());
        assert!(validate_env(&env).is_ok());
    }

    #[test]
    fn validate_env_rejects_other_keys() {
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), "/evil".to_string());
        assert!(validate_env(&env).is_err());
    }

    #[test]
    fn keychain_service_uses_first_8_hex_of_sha256() {
        // sha256("/tmp/profile") = 9a0b... computed once and pinned here.
        let service = claude_keychain_service(Path::new("/tmp/profile"));
        assert!(service.starts_with("Claude Code-credentials-"));
        assert_eq!(service.len(), "Claude Code-credentials-".len() + 8);
        assert!(service[service.len() - 8..]
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn shared_entries_exclude_credentials() {
        assert!(!shared_entries("claude").contains(&".credentials.json"));
        assert!(!shared_entries("codex").contains(&"auth.json"));
        assert!(shared_entries("claude").contains(&"projects"));
        assert!(shared_entries("codex").contains(&"sessions"));
    }

    #[test]
    fn link_shared_links_existing_and_skips_missing() {
        let root = std::env::temp_dir().join(format!("mc-acct-{}", uuid::Uuid::new_v4()));
        let source = root.join("source");
        let profile = root.join("profile");
        std::fs::create_dir_all(source.join("projects")).unwrap();
        std::fs::write(source.join("CLAUDE.md"), "hi").unwrap();
        std::fs::create_dir_all(&profile).unwrap();

        let skipped = link_shared(&profile, &source, "claude");

        assert!(profile.join("CLAUDE.md").exists());
        assert!(profile.join("projects").exists());
        assert!(skipped.contains(&"settings.json".to_string()));
        assert!(!profile.join(".credentials.json").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn strip_claude_json_removes_account_fields() {
        let raw = r#"{"oauthAccount":{"emailAddress":"a@b.c"},"userID":"u","mcpServers":{"x":{}}}"#;
        let out = strip_claude_json(raw).unwrap();
        assert!(out.get("oauthAccount").is_none());
        assert!(out.get("userID").is_none());
        assert!(out.get("mcpServers").is_some());
    }

    #[test]
    fn identity_from_claude_json_reads_email() {
        let raw = r#"{"oauthAccount":{"emailAddress":"a@b.c"}}"#;
        assert_eq!(claude_identity_from_json(raw), Some("a@b.c".to_string()));
    }

    #[test]
    fn identity_from_codex_auth_reads_jwt_email() {
        // payload {"email":"c@d.e"} base64url = eyJlbWFpbCI6ImNAZC5lIn0
        let raw = r#"{"tokens":{"id_token":"xxx.eyJlbWFpbCI6ImNAZC5lIn0.yyy"}}"#;
        assert_eq!(codex_identity_from_auth(raw), Some("c@d.e".to_string()));
    }

    #[test]
    fn store_roundtrip_persists_accounts() {
        let root = std::env::temp_dir().join(format!("mc-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("accounts.json");
        let accounts = vec![Account {
            id: "abc".into(),
            provider: "claude".into(),
            label: "Work".into(),
            profile_dir: Some("/tmp/x".into()),
            identity: None,
            created_at: 1,
        }];
        save_accounts_file(&file, &accounts).unwrap();
        let loaded = load_accounts_file(&file);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "abc");
        let _ = std::fs::remove_dir_all(root);
    }
}
```

- [ ] **Step 3: Run tests to verify they fail to compile**

Run: `cargo test --manifest-path /Users/navaneet/Developer/monocode/src-tauri/Cargo.toml accounts:: 2>&1 | tail -5`
Expected: compile errors, `cannot find function validate_env` etc. (You must also add `mod accounts;` to `lib.rs` for the module to be compiled; do that now, at line 3 alphabetical position before `mod chat_background;`.)

- [ ] **Step 4: Implement the module**

Replace the top of `src-tauri/src/accounts.rs` (above the test module) with:

```rust
//! Accounts: extra Claude Code / Codex logins, each isolated in a profile
//! directory that a session receives through `CLAUDE_CONFIG_DIR` or
//! `CODEX_HOME`. Only credentials live in a profile; everything else links
//! back to the user's real config dir so all accounts behave alike.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager, State};

use crate::dirs_home;

pub const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
pub const CODEX_HOME_ENV: &str = "CODEX_HOME";
const ALLOWED_ENV_KEYS: [&str; 2] = [CLAUDE_CONFIG_DIR_ENV, CODEX_HOME_ENV];
const ACCOUNTS_FILE: &str = "accounts.json";
const KEYCHAIN_SERVICE_PREFIX: &str = "Claude Code-credentials";

const CLAUDE_SHARED: [&str; 10] = [
    "CLAUDE.md",
    "settings.json",
    "agents",
    "commands",
    "hooks",
    "plugins",
    "rules",
    "skills",
    "projects",
    "history.jsonl",
];
const CODEX_SHARED: [&str; 8] = [
    "config.toml",
    "AGENTS.md",
    "skills",
    "rules",
    "hooks.json",
    "plugins",
    "sessions",
    "memories",
];
const CLAUDE_JSON_STRIP: [&str; 2] = ["oauthAccount", "userID"];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub profile_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    pub created_at: i64,
}

#[derive(Default)]
pub struct AccountStore {
    loaded: Mutex<Option<Vec<Account>>>,
}

pub fn validate_env(env: &HashMap<String, String>) -> Result<(), String> {
    for key in env.keys() {
        if !ALLOWED_ENV_KEYS.contains(&key.as_str()) {
            return Err(format!("env key not allowed: {key}"));
        }
    }
    Ok(())
}

pub fn apply_env(cmd: &mut Command, env: Option<&HashMap<String, String>>) -> Result<(), String> {
    let Some(env) = env else { return Ok(()) };
    validate_env(env)?;
    for (key, value) in env {
        cmd.env(key, value);
    }
    Ok(())
}

pub fn claude_keychain_service(profile_dir: &Path) -> String {
    let digest = Sha256::digest(profile_dir.to_string_lossy().as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{KEYCHAIN_SERVICE_PREFIX}-{}", &hex[..8])
}

fn shared_entries(provider: &str) -> &'static [&'static str] {
    match provider {
        "claude" => &CLAUDE_SHARED,
        "codex" => &CODEX_SHARED,
        _ => &[],
    }
}

fn provider_home(provider: &str) -> Option<PathBuf> {
    let home = PathBuf::from(dirs_home()?);
    match provider {
        "claude" => Some(home.join(".claude")),
        "codex" => Some(home.join(".codex")),
        _ => None,
    }
}

fn link_entry(target: &Path, link: &Path) -> std::io::Result<()> {
    if link.exists() || link.symlink_metadata().is_ok() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        if target.is_dir() {
            std::os::windows::fs::symlink_dir(target, link)
                .or_else(|_| copy_dir(target, link))
        } else {
            std::fs::copy(target, link).map(|_| ())
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        Ok(())
    }
}

#[cfg(windows)]
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

/// Links every shared entry that exists in `source` into `profile`.
/// Returns the entries that were skipped because the source is missing.
pub fn link_shared(profile: &Path, source: &Path, provider: &str) -> Vec<String> {
    let mut skipped = Vec::new();
    for entry in shared_entries(provider) {
        let target = source.join(entry);
        if !target.exists() {
            skipped.push((*entry).to_string());
            continue;
        }
        if let Err(error) = link_entry(&target, &profile.join(entry)) {
            eprintln!("accounts: link {entry} failed: {error}");
            skipped.push((*entry).to_string());
        }
    }
    skipped
}

fn strip_claude_json(raw: &str) -> Option<Value> {
    let mut value: Value = serde_json::from_str(raw).ok()?;
    let object = value.as_object_mut()?;
    for key in CLAUDE_JSON_STRIP {
        object.remove(key);
    }
    Some(value)
}

/// `~/.claude.json` holds MCP servers and onboarding state next to the
/// account identity. Copy it once with the identity removed so a fresh
/// profile skips onboarding and keeps the user's MCP servers.
fn seed_claude_json(profile: &Path) {
    let Some(home) = dirs_home() else { return };
    let source = PathBuf::from(home).join(".claude.json");
    let Ok(raw) = std::fs::read_to_string(&source) else { return };
    let Some(stripped) = strip_claude_json(&raw) else { return };
    let Ok(text) = serde_json::to_string_pretty(&stripped) else { return };
    let _ = std::fs::write(profile.join(".claude.json"), text);
}

fn claude_identity_from_json(raw: &str) -> Option<String> {
    let value: Value = serde_json::from_str(raw).ok()?;
    value
        .get("oauthAccount")?
        .get("emailAddress")?
        .as_str()
        .map(str::to_string)
}

fn decode_jwt_payload(token: &str) -> Option<Value> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn codex_identity_from_auth(raw: &str) -> Option<String> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let token = value.get("tokens")?.get("id_token")?.as_str()?;
    decode_jwt_payload(token)?
        .get("email")?
        .as_str()
        .map(str::to_string)
}

fn config_dir(account: &Account) -> Option<PathBuf> {
    match &account.profile_dir {
        Some(dir) => Some(PathBuf::from(dir)),
        None => provider_home(&account.provider),
    }
}

fn read_identity(account: &Account) -> Option<String> {
    let dir = config_dir(account)?;
    match account.provider.as_str() {
        "claude" => {
            let path = if account.profile_dir.is_some() {
                dir.join(".claude.json")
            } else {
                PathBuf::from(dirs_home()?).join(".claude.json")
            };
            claude_identity_from_json(&std::fs::read_to_string(path).ok()?)
        }
        "codex" => codex_identity_from_auth(&std::fs::read_to_string(dir.join("auth.json")).ok()?),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn keychain_has_item(service: &str) -> bool {
    Command::new("security")
        .args(["find-generic-password", "-s", service])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn keychain_delete_item(service: &str) {
    let _ = Command::new("security")
        .args(["delete-generic-password", "-s", service])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn login_ready(account: &Account) -> bool {
    let Some(dir) = config_dir(account) else { return false };
    match account.provider.as_str() {
        "claude" => {
            if dir.join(".credentials.json").exists() {
                return true;
            }
            #[cfg(target_os = "macos")]
            {
                let service = match &account.profile_dir {
                    Some(profile) => claude_keychain_service(Path::new(profile)),
                    None => KEYCHAIN_SERVICE_PREFIX.to_string(),
                };
                return keychain_has_item(&service);
            }
            #[allow(unreachable_code)]
            false
        }
        "codex" => dir.join("auth.json").exists(),
        _ => false,
    }
}

fn accounts_root(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("accounts");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn load_accounts_file(path: &Path) -> Vec<Account> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_accounts_file(path: &Path, accounts: &[Account]) -> Result<(), String> {
    let text = serde_json::to_string_pretty(accounts).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())
}

fn default_account(provider: &str) -> Account {
    Account {
        id: "default".into(),
        provider: provider.into(),
        label: "Default".into(),
        profile_dir: None,
        identity: None,
        created_at: 0,
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl AccountStore {
    fn with<T>(
        &self,
        app: &AppHandle,
        f: impl FnOnce(&mut Vec<Account>) -> Result<T, String>,
    ) -> Result<T, String> {
        let file = accounts_root(app)?.join(ACCOUNTS_FILE);
        let mut guard = self.loaded.lock().map_err(|_| "accounts store poisoned")?;
        let accounts = guard.get_or_insert_with(|| load_accounts_file(&file));
        let before = serde_json::to_string(&*accounts).unwrap_or_default();
        let result = f(accounts)?;
        let after = serde_json::to_string(&*accounts).unwrap_or_default();
        if before != after {
            save_accounts_file(&file, accounts)?;
        }
        Ok(result)
    }
}

fn with_identity(mut account: Account) -> Account {
    account.identity = read_identity(&account);
    account
}

/// Every account, defaults first. The default for each provider is the
/// user's real `~/.claude` / `~/.codex` login and is never stored.
#[tauri::command(async)]
pub fn accounts_list(
    app: AppHandle,
    store: State<'_, AccountStore>,
) -> Result<Vec<Account>, String> {
    let extra = store.with(&app, |accounts| Ok(accounts.clone()))?;
    let mut all = vec![default_account("claude"), default_account("codex")];
    all.extend(extra);
    Ok(all.into_iter().map(with_identity).collect())
}

#[tauri::command(async)]
pub fn accounts_create(
    app: AppHandle,
    store: State<'_, AccountStore>,
    provider: String,
    label: String,
) -> Result<Account, String> {
    if !matches!(provider.as_str(), "claude" | "codex") {
        return Err(format!("accounts: unsupported provider {provider}"));
    }
    let label = label.trim();
    if label.is_empty() {
        return Err("accounts: label is required".into());
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    let profile = accounts_root(&app)?.join(&provider).join(&id);
    std::fs::create_dir_all(&profile).map_err(|e| e.to_string())?;
    if let Some(source) = provider_home(&provider) {
        link_shared(&profile, &source, &provider);
    }
    if provider == "claude" {
        seed_claude_json(&profile);
    }
    let account = Account {
        id,
        provider,
        label: label.to_string(),
        profile_dir: Some(profile.to_string_lossy().into_owned()),
        identity: None,
        created_at: now_millis(),
    };
    store.with(&app, |accounts| {
        accounts.push(account.clone());
        Ok(())
    })?;
    Ok(account)
}

#[tauri::command(async)]
pub fn accounts_rename(
    app: AppHandle,
    store: State<'_, AccountStore>,
    id: String,
    label: String,
) -> Result<(), String> {
    let label = label.trim().to_string();
    if label.is_empty() {
        return Err("accounts: label is required".into());
    }
    store.with(&app, |accounts| {
        let Some(account) = accounts.iter_mut().find(|a| a.id == id) else {
            return Err("accounts: not found".into());
        };
        account.label = label;
        Ok(())
    })
}

#[tauri::command(async)]
pub fn accounts_remove(
    app: AppHandle,
    store: State<'_, AccountStore>,
    id: String,
) -> Result<(), String> {
    if id == "default" {
        return Err("accounts: the default account cannot be removed".into());
    }
    let removed = store.with(&app, |accounts| {
        let Some(index) = accounts.iter().position(|a| a.id == id) else {
            return Err("accounts: not found".into());
        };
        Ok(accounts.remove(index))
    })?;
    if let Some(dir) = removed.profile_dir {
        #[cfg(target_os = "macos")]
        if removed.provider == "claude" {
            keychain_delete_item(&claude_keychain_service(Path::new(&dir)));
        }
        let _ = std::fs::remove_dir_all(dir);
    }
    Ok(())
}

#[tauri::command(async)]
pub fn accounts_login_ready(
    app: AppHandle,
    store: State<'_, AccountStore>,
    id: String,
) -> Result<bool, String> {
    let accounts = accounts_list(app, store)?;
    let Some(account) = accounts.into_iter().find(|a| a.id == id) else {
        return Err("accounts: not found".into());
    };
    Ok(login_ready(&account))
}
```

Note: the `default` id is shared by both providers; callers always pair it with a provider. The frontend treats `{provider, id}` as the identity.

- [ ] **Step 5: Register in `lib.rs`**

Add `mod accounts;` at the top (alphabetically first). In the builder, after `.manage(pty::PtyHost::new())` add `.manage(accounts::AccountStore::default())`. In `generate_handler![` add:

```rust
            accounts::accounts_list,
            accounts::accounts_create,
            accounts::accounts_rename,
            accounts::accounts_remove,
            accounts::accounts_login_ready,
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --manifest-path /Users/navaneet/Developer/monocode/src-tauri/Cargo.toml accounts:: 2>&1 | tail -15`
Expected: `test result: ok. 9 passed`

- [ ] **Step 7: Commit**

```bash
git -C /Users/navaneet/Developer/monocode add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/accounts.rs src-tauri/src/lib.rs
git -C /Users/navaneet/Developer/monocode commit -m "feat: add accounts module with profile dirs and env allowlist"
```

---

### Task 2: Env parameter on `harness_spawn` and `harness_exec`; `spawnChild` accepts env

**Files:**
- Modify: `src-tauri/src/harness.rs:320-345` (`harness_spawn`), `:669-690` (`harness_exec`, `exec_capture`)
- Modify: `src/lib/harness/child.ts:236-255` (`spawnChild`)
- Test: `src/lib/harness/child.test.ts`

**Interfaces:**
- Consumes: `accounts::apply_env`
- Produces: `spawnChild(sessionId, command, args, cwd, env?: Record<string,string>)`; Tauri `harness_spawn { ..., env?: HashMap }`, `harness_exec { ..., env?: HashMap }`

- [ ] **Step 1: Write the failing frontend test**

Check how `child.test.ts` mocks `invoke` (top of the file) and add a test in the same style:

```ts
test("spawnChild forwards env to harness_spawn", async () => {
  invokeMock.mockResolvedValueOnce(123);
  await spawnChild("s1", "/bin/claude", ["--x"], "/tmp", {
    CLAUDE_CONFIG_DIR: "/tmp/p",
  });
  expect(invokeMock).toHaveBeenCalledWith("harness_spawn", {
    sessionId: "s1",
    command: "/bin/claude",
    args: ["--x"],
    cwd: "/tmp",
    env: { CLAUDE_CONFIG_DIR: "/tmp/p" },
  });
});
```

If the file has no `invoke` mock yet, add at top:

```ts
import { vi } from "vitest";
const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invokeMock(...a) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => undefined) }));
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `npx --prefix /Users/navaneet/Developer/monocode vitest run /Users/navaneet/Developer/monocode/src/lib/harness/child.test.ts 2>&1 | tail -15`
Expected: FAIL, `env` missing from the call.

- [ ] **Step 3: Implement the frontend change**

In `src/lib/harness/child.ts` change `spawnChild`:

```ts
export async function spawnChild(
  sessionId: string,
  command: string,
  args: string[],
  cwd: string,
  env?: Record<string, string>,
): Promise<void> {
  livePid.delete(sessionId);
  pendingExit.delete(sessionId);
  const pid = await invoke<number>("harness_spawn", {
    sessionId,
    command,
    args,
    cwd,
    ...(env && Object.keys(env).length > 0 ? { env } : {}),
  });
```

Note: the test above expects `env` present when given. When `env` is undefined, the key is omitted so existing tests keep passing.

- [ ] **Step 4: Implement the Rust change**

In `harness_spawn` add a parameter and apply it right after `prepare_child`:

```rust
pub fn harness_spawn(
    app: AppHandle,
    host: State<'_, HarnessHost>,
    session_id: String,
    command: String,
    args: Vec<String>,
    cwd: String,
    env: Option<HashMap<String, String>>,
) -> Result<u32, String> {
```

and after `prepare_child(&mut cmd, &command);`:

```rust
    crate::accounts::apply_env(&mut cmd, env.as_ref())?;
```

Do the same for `harness_exec` (add `env: Option<HashMap<String, String>>` after `cwd`) and pass it through to `exec_capture(&command, &args, cwd.as_deref(), env.as_ref())`, whose signature becomes `fn exec_capture(command: &str, args: &[String], cwd: Option<&str>, env: Option<&HashMap<String, String>>)` and which calls `crate::accounts::apply_env(&mut cmd, env)?;` after `prepare_child`. Update any other callers of `exec_capture` (grep) to pass `None`.

- [ ] **Step 5: Run tests**

Run: `npx --prefix /Users/navaneet/Developer/monocode vitest run /Users/navaneet/Developer/monocode/src/lib/harness/child.test.ts 2>&1 | tail -5`
Expected: PASS
Run: `cargo check --manifest-path /Users/navaneet/Developer/monocode/src-tauri/Cargo.toml 2>&1 | tail -3`
Expected: no errors.

- [ ] **Step 6: Commit**

```bash
git -C /Users/navaneet/Developer/monocode add src-tauri/src/harness.rs src/lib/harness/child.ts src/lib/harness/child.test.ts
git -C /Users/navaneet/Developer/monocode commit -m "feat: accept allowlisted env on harness spawn and exec"
```

---

### Task 3: `fetch_claude_usage` per profile directory

**Files:**
- Modify: `src-tauri/src/rate_limits.rs` (`fetch_claude_usage`, `read_claude_credentials`, `read_credentials_file`, `claude_credentials_path`, `ClaudeCredStore::Keychain`, `read_macos_keychain_credentials`, `write_macos_keychain_blob`, `keychain_find_args`)
- Modify: `src/lib/rateLimitsFetch.ts` (`fetchClaudeRateLimits(profileDir?: string | null)`)

**Interfaces:**
- Consumes: `accounts::claude_keychain_service`
- Produces: Tauri `fetch_claude_usage { profileDir?: string }`; `fetchClaudeRateLimits(profileDir?: string | null)`

- [ ] **Step 1: Write a failing Rust test**

Add to the existing `#[cfg(test)]` module in `rate_limits.rs` (create one if none):

```rust
#[test]
fn credentials_path_uses_profile_dir_when_given() {
    let path = claude_credentials_path(Some(Path::new("/tmp/profile"))).unwrap();
    assert_eq!(path, PathBuf::from("/tmp/profile/.credentials.json"));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --manifest-path /Users/navaneet/Developer/monocode/src-tauri/Cargo.toml rate_limits:: 2>&1 | tail -5`
Expected: compile error (wrong arity).

- [ ] **Step 3: Implement**

Thread an optional profile dir through:

```rust
#[tauri::command]
pub async fn fetch_claude_usage(profile_dir: Option<String>) -> Result<ClaudeUsageFetch, String> {
    tauri::async_runtime::spawn_blocking(move || {
        fetch_claude_usage_sync(profile_dir.as_deref().map(Path::new))
    })
    .await
    .map_err(|e| e.to_string())?
}

fn fetch_claude_usage_sync(profile_dir: Option<&Path>) -> Result<ClaudeUsageFetch, String> {
    let Some(mut creds) = read_claude_credentials(profile_dir) else { ... unchanged ... };
```

```rust
fn read_claude_credentials(profile_dir: Option<&Path>) -> Option<ClaudeCredentials> {
    #[cfg(target_os = "macos")]
    {
        if let Some(creds) = read_macos_keychain_credentials(profile_dir) {
            return Some(creds);
        }
    }
    read_credentials_file(profile_dir)
}

fn read_credentials_file(profile_dir: Option<&Path>) -> Option<ClaudeCredentials> {
    let path = claude_credentials_path(profile_dir)?;
    ...
}

fn claude_credentials_path(profile_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = profile_dir {
        return Some(dir.join(".credentials.json"));
    }
    ... existing home-based path ...
}
```

Keychain: change `ClaudeCredStore::Keychain { account: String }` to `Keychain { service: String, account: String }`. `keychain_find_args(service: &str)` uses the given service. `read_macos_keychain_credentials(profile_dir)` computes:

```rust
let service = match profile_dir {
    Some(dir) => crate::accounts::claude_keychain_service(dir),
    None => LEGACY_KEYCHAIN_SERVICE.to_string(),
};
```

and stores `service` in the `Keychain` variant. `write_macos_keychain_blob(service: &str, account: &str, raw: &str)` uses `service` instead of the constant. `persist_claude_credentials` passes `service` from the store.

- [ ] **Step 4: Frontend**

In `src/lib/rateLimitsFetch.ts`:

```ts
export async function fetchClaudeRateLimits(
  profileDir?: string | null,
): Promise<ProviderRateLimits> {
  try {
    const result = await invoke<ClaudeUsageFetch>("fetch_claude_usage", {
      profileDir: profileDir ?? null,
    });
```

- [ ] **Step 5: Run tests**

Run: `cargo test --manifest-path /Users/navaneet/Developer/monocode/src-tauri/Cargo.toml rate_limits:: 2>&1 | tail -5`
Expected: PASS.
Run: `npx --prefix /Users/navaneet/Developer/monocode tsc --noEmit -p /Users/navaneet/Developer/monocode 2>&1 | tail -3`
Expected: no errors.

- [ ] **Step 6: Commit**

```bash
git -C /Users/navaneet/Developer/monocode add src-tauri/src/rate_limits.rs src/lib/rateLimitsFetch.ts
git -C /Users/navaneet/Developer/monocode commit -m "feat: read Claude usage for a specific profile directory"
```

---

### Task 4: Frontend accounts store and `account` setting plumbing

**Files:**
- Create: `src/lib/accounts.ts`
- Create: `src/lib/accounts.test.ts`
- Modify: `src/lib/models.ts:356-378` (`mergeModelSettings`, `saveLastModelSettings`)
- Test: `src/lib/models.test.ts`

**Interfaces:**
- Produces:
  - `export type AccountProvider = "claude" | "codex"`
  - `export type Account = { id: string; provider: AccountProvider; label: string; profileDir: string | null; identity?: string; createdAt: number }`
  - `export const ACCOUNT_SETTING_ID = "account"`, `export const DEFAULT_ACCOUNT_ID = "default"`
  - `export function isAccountProvider(h: HarnessId): h is AccountProvider`
  - `export function accountEnv(account: Account | undefined): Record<string,string>` returns `{}` for default/undefined, else `{CLAUDE_CONFIG_DIR: dir}` or `{CODEX_HOME: dir}`
  - `export function accountIdFor(provider: AccountProvider, settings?: Record<string,string>): string` returns the id in settings if it names a known account of that provider, else the Settings default, else `default`
  - `export function withAccountSetting(settings: Record<string,string>, id: string): Record<string,string>`
  - `export function stripAccountSetting(settings: Record<string,string>): Record<string,string>`
  - Store: `refreshAccounts(): Promise<Account[]>`, `getAccountsSnapshot(): Account[]`, `subscribeAccounts(fn)`, `accountsFor(provider): Account[]`, `findAccount(provider, id): Account | undefined`
  - Wrappers: `createAccount(provider, label)`, `renameAccount(id, label)`, `removeAccount(id)`, `accountLoginReady(id)`
  - Default per provider: `loadDefaultAccountId(provider)`, `saveDefaultAccountId(provider, id)` (localStorage key `monocode.defaultAccount.<provider>`)
  - Auto switch: `loadAutoSwitchAccount(): boolean` (default true), `saveAutoSwitchAccount(v)` (key `monocode.autoSwitchAccount`)

- [ ] **Step 1: Write failing tests** in `src/lib/accounts.test.ts`

```ts
import { beforeEach, describe, expect, test, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...a: unknown[]) => invokeMock(...a),
}));

import {
  accountEnv,
  accountIdFor,
  ACCOUNT_SETTING_ID,
  loadAutoSwitchAccount,
  refreshAccounts,
  saveAutoSwitchAccount,
  saveDefaultAccountId,
  stripAccountSetting,
  withAccountSetting,
  type Account,
} from "./accounts";

const work: Account = {
  id: "w1",
  provider: "claude",
  label: "Work",
  profileDir: "/p/w1",
  createdAt: 1,
};
const dflt: Account = {
  id: "default",
  provider: "claude",
  label: "Default",
  profileDir: null,
  createdAt: 0,
};

beforeEach(() => {
  localStorage.clear();
  invokeMock.mockReset();
});

describe("accountEnv", () => {
  test("default account sets no env", () => {
    expect(accountEnv(dflt)).toEqual({});
    expect(accountEnv(undefined)).toEqual({});
  });
  test("claude profile sets CLAUDE_CONFIG_DIR", () => {
    expect(accountEnv(work)).toEqual({ CLAUDE_CONFIG_DIR: "/p/w1" });
  });
  test("codex profile sets CODEX_HOME", () => {
    expect(accountEnv({ ...work, provider: "codex" })).toEqual({
      CODEX_HOME: "/p/w1",
    });
  });
});

describe("accountIdFor", () => {
  test("returns the setting when it names a known account", async () => {
    invokeMock.mockResolvedValueOnce([dflt, work]);
    await refreshAccounts();
    expect(accountIdFor("claude", { [ACCOUNT_SETTING_ID]: "w1" })).toBe("w1");
  });
  test("falls back to the saved default when the setting is unknown", async () => {
    invokeMock.mockResolvedValueOnce([dflt, work]);
    await refreshAccounts();
    saveDefaultAccountId("claude", "w1");
    expect(accountIdFor("claude", { [ACCOUNT_SETTING_ID]: "gone" })).toBe("w1");
  });
  test("falls back to default when nothing is set", () => {
    expect(accountIdFor("claude", {})).toBe("default");
  });
});

describe("settings helpers", () => {
  test("withAccountSetting adds without mutating", () => {
    const base = { effort: "high" };
    const next = withAccountSetting(base, "w1");
    expect(next).toEqual({ effort: "high", account: "w1" });
    expect(base).toEqual({ effort: "high" });
  });
  test("stripAccountSetting removes the key", () => {
    expect(stripAccountSetting({ effort: "high", account: "w1" })).toEqual({
      effort: "high",
    });
  });
});

describe("auto switch", () => {
  test("defaults to on and persists off", () => {
    expect(loadAutoSwitchAccount()).toBe(true);
    saveAutoSwitchAccount(false);
    expect(loadAutoSwitchAccount()).toBe(false);
  });
});
```

The vitest environment is `node`; check `vitest.config.ts` and, if `localStorage` is undefined in tests, look at how `settings.test.ts` handles it and copy its setup (it likely uses a `// @vitest-environment jsdom` pragma or a stub). Add the same pragma at the top of this test file.

- [ ] **Step 2: Run to verify failure**

Run: `npx --prefix /Users/navaneet/Developer/monocode vitest run /Users/navaneet/Developer/monocode/src/lib/accounts.test.ts 2>&1 | tail -5`
Expected: FAIL, module not found.

- [ ] **Step 3: Implement `src/lib/accounts.ts`**

```ts
import { invoke } from "@tauri-apps/api/core";
import type { HarnessId } from "./session";

export type AccountProvider = "claude" | "codex";

export type Account = {
  id: string;
  provider: AccountProvider;
  label: string;
  profileDir: string | null;
  identity?: string;
  createdAt: number;
};

export const ACCOUNT_SETTING_ID = "account";
export const DEFAULT_ACCOUNT_ID = "default";
export const ACCOUNT_PROVIDERS: AccountProvider[] = ["claude", "codex"];

const ENV_KEY: Record<AccountProvider, string> = {
  claude: "CLAUDE_CONFIG_DIR",
  codex: "CODEX_HOME",
};
const DEFAULT_KEY_PREFIX = "monocode.defaultAccount.";
const AUTO_SWITCH_KEY = "monocode.autoSwitchAccount";
export const ACCOUNTS_CHANGE_EVENT = "monocode:accounts-change";

export function isAccountProvider(harness: HarnessId): harness is AccountProvider {
  return harness === "claude" || harness === "codex";
}

export function accountEnv(account: Account | undefined): Record<string, string> {
  if (!account?.profileDir) return {};
  return { [ENV_KEY[account.provider]]: account.profileDir };
}

export function withAccountSetting(
  settings: Record<string, string>,
  id: string,
): Record<string, string> {
  return { ...settings, [ACCOUNT_SETTING_ID]: id };
}

export function stripAccountSetting(
  settings: Record<string, string>,
): Record<string, string> {
  const { [ACCOUNT_SETTING_ID]: _omit, ...rest } = settings;
  return rest;
}

// --- store -----------------------------------------------------------------

let accounts: Account[] = [];
let version = 0;
let inflight: Promise<Account[]> | null = null;
const listeners = new Set<() => void>();

function emit() {
  version += 1;
  for (const listener of listeners) listener();
  window.dispatchEvent?.(new Event(ACCOUNTS_CHANGE_EVENT));
}

export function getAccountsSnapshot(): number {
  return version;
}

export function subscribeAccounts(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function listAccounts(): Account[] {
  return accounts;
}

export function accountsFor(provider: AccountProvider): Account[] {
  return accounts.filter((account) => account.provider === provider);
}

export function findAccount(
  provider: AccountProvider,
  id: string,
): Account | undefined {
  return accounts.find((a) => a.provider === provider && a.id === id);
}

export function refreshAccounts(): Promise<Account[]> {
  if (inflight) return inflight;
  inflight = invoke<Account[]>("accounts_list")
    .then((list) => {
      accounts = Array.isArray(list) ? list : [];
      emit();
      return accounts;
    })
    .catch((error: unknown) => {
      console.error("accounts_list failed", error);
      return accounts;
    })
    .finally(() => {
      inflight = null;
    });
  return inflight;
}

export async function createAccount(
  provider: AccountProvider,
  label: string,
): Promise<Account> {
  const account = await invoke<Account>("accounts_create", { provider, label });
  await refreshAccounts();
  return account;
}

export async function renameAccount(id: string, label: string): Promise<void> {
  await invoke("accounts_rename", { id, label });
  await refreshAccounts();
}

export async function removeAccount(id: string): Promise<void> {
  await invoke("accounts_remove", { id });
  await refreshAccounts();
}

export function accountLoginReady(id: string): Promise<boolean> {
  return invoke<boolean>("accounts_login_ready", { id });
}

// --- defaults ---------------------------------------------------------------

export function loadDefaultAccountId(provider: AccountProvider): string {
  try {
    return localStorage.getItem(DEFAULT_KEY_PREFIX + provider) ?? DEFAULT_ACCOUNT_ID;
  } catch {
    return DEFAULT_ACCOUNT_ID;
  }
}

export function saveDefaultAccountId(provider: AccountProvider, id: string) {
  try {
    if (id === DEFAULT_ACCOUNT_ID) localStorage.removeItem(DEFAULT_KEY_PREFIX + provider);
    else localStorage.setItem(DEFAULT_KEY_PREFIX + provider, id);
  } catch {
    // Preferences are best-effort.
  }
  emit();
}

/**
 * The account a session should run on: its own setting when that names a
 * known account, else the provider default from Settings, else `default`.
 */
export function accountIdFor(
  provider: AccountProvider,
  settings?: Record<string, string>,
): string {
  const chosen = settings?.[ACCOUNT_SETTING_ID];
  if (chosen && (chosen === DEFAULT_ACCOUNT_ID || findAccount(provider, chosen))) {
    return chosen;
  }
  const preferred = loadDefaultAccountId(provider);
  if (preferred === DEFAULT_ACCOUNT_ID || findAccount(provider, preferred)) {
    return preferred;
  }
  return DEFAULT_ACCOUNT_ID;
}

export function accountFor(
  provider: AccountProvider,
  settings?: Record<string, string>,
): Account | undefined {
  return findAccount(provider, accountIdFor(provider, settings));
}

export function loadAutoSwitchAccount(): boolean {
  try {
    return localStorage.getItem(AUTO_SWITCH_KEY) !== "false";
  } catch {
    return true;
  }
}

export function saveAutoSwitchAccount(value: boolean) {
  try {
    localStorage.setItem(AUTO_SWITCH_KEY, value ? "true" : "false");
  } catch {
    // best-effort
  }
}
```

- [ ] **Step 4: Preserve `account` across model changes**

In `src/lib/models.ts`, `mergeModelSettings` currently drops keys that are not model settings. Add:

```ts
import { ACCOUNT_SETTING_ID } from "./accounts";
...
export function mergeModelSettings(
  model: AgentModel,
  current?: Record<string, string>,
): Record<string, string> {
  const next = defaultModelSettings(model);
  if (!current) return next;
  for (const setting of model.settings ?? []) {
    const value = compatibleSettingValue(setting, current[setting.id]);
    if (value != null) next[setting.id] = value;
  }
  const account = current[ACCOUNT_SETTING_ID];
  if (account) next[ACCOUNT_SETTING_ID] = account;
  return next;
}
```

And in `saveLastModelSettings` (find it below `loadLastModelSettings`), strip the account so a new session uses the Settings default rather than the last session's account:

```ts
  const persisted = stripAccountSetting(settings);
```

and write `persisted` instead of `settings` to localStorage (keep the existing `mode` behaviour if the function has a second parameter; only the value stored changes).

Add a test to `src/lib/models.test.ts`:

```ts
test("mergeModelSettings keeps the account setting", () => {
  const model = resolveModel("claude", defaultModelId("claude"));
  const merged = mergeModelSettings(model, { account: "w1" });
  expect(merged.account).toBe("w1");
});
```

(Use whatever helpers the file already imports for building a model; `resolveModel` and `defaultModelId` exist in `models.ts`.)

- [ ] **Step 5: Run tests**

Run: `npx --prefix /Users/navaneet/Developer/monocode vitest run /Users/navaneet/Developer/monocode/src/lib/accounts.test.ts /Users/navaneet/Developer/monocode/src/lib/models.test.ts 2>&1 | tail -8`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git -C /Users/navaneet/Developer/monocode add src/lib/accounts.ts src/lib/accounts.test.ts src/lib/models.ts src/lib/models.test.ts
git -C /Users/navaneet/Developer/monocode commit -m "feat: add frontend accounts store and account model setting"
```

---

### Task 5: Adapters spawn with the account env and restart on account change

**Files:**
- Modify: `src/lib/harness/claude.ts` (`settingsKeyFor` ~1350, spawn at ~422)
- Modify: `src/lib/harness/codex.ts` (`Live` type ~line 60-90, `ensureLive` ~309, spawn at ~385)
- Modify: `src/lib/rateLimitsFetch.ts` (`fetchCodexRateLimits(account?)`)
- Test: `src/lib/harness/claudeLive.test.ts`, `src/lib/harness/codexLive.test.ts`

**Interfaces:**
- Consumes: `accountFor`, `accountEnv`, `accountIdFor` from `../accounts`; `spawnChild(..., env)`
- Produces: `fetchCodexRateLimits(account?: Account)`

- [ ] **Step 1: Failing test for Claude**

In `claudeLive.test.ts` find the existing test that asserts `spawnChild` was called (search `spawnChild`), and add a sibling test that seeds a non-default account and expects env. The accounts store is module state; seed it through `refreshAccounts` with a mocked `invoke` returning `[{id:"w1",provider:"claude",label:"W",profileDir:"/p/w1",createdAt:1}]`, then start a session with `modelSettings: { account: "w1" }` and assert the `spawnChild` mock's 5th argument equals `{ CLAUDE_CONFIG_DIR: "/p/w1" }`.

- [ ] **Step 2: Run to verify it fails**

Run: `npx --prefix /Users/navaneet/Developer/monocode vitest run /Users/navaneet/Developer/monocode/src/lib/harness/claudeLive.test.ts 2>&1 | tail -8`
Expected: FAIL, 5th argument undefined.

- [ ] **Step 3: Implement Claude**

In `claude.ts`:

```ts
import { accountEnv, accountFor, accountIdFor } from "../accounts";
```

`settingsKeyFor` must include the account so a change forces a respawn (which resumes the same Claude session):

```ts
function settingsKeyFor(input: HarnessSessionInput): string {
  return `${accountIdFor("claude", input.modelSettings)}|${claudeSettingsKey({ ...existing fields... })}`;
}
```

At the spawn:

```ts
  await spawnChild(
    input.sessionId,
    path,
    buildClaudeSpawnArgs(launch),
    input.cwd,
    accountEnv(accountFor("claude", input.modelSettings)),
  );
```

- [ ] **Step 4: Implement Codex**

`ensureLive` only restarts on cwd change today. Add an `accountId: string` field to the codex `Live` type and, in `ensureLive`:

```ts
  const accountId = accountIdFor("codex", input.modelSettings);
  const existing = liveByThread.get(input.sessionId);
  if (existing && existing.cwd === input.cwd && existing.accountId === accountId) {
    existing.onEvent = input.onEvent;
    return existing;
  }
  if (existing) {
    // A cwd change invalidates the thread; an account change must keep it so
    // the same conversation resumes under the new login.
    if (existing.cwd !== input.cwd) resumeByThread.delete(input.sessionId);
    await stopCodexSession(input.sessionId);
  }
```

Set `accountId` when constructing `live`, and spawn with:

```ts
  await spawnChild(
    input.sessionId,
    path,
    ["app-server"],
    input.cwd,
    accountEnv(accountFor("codex", input.modelSettings)),
  );
```

Add a codex test mirroring Step 1 in `codexLive.test.ts`.

- [ ] **Step 5: Codex usage per account**

In `rateLimitsFetch.ts`:

```ts
import { accountEnv, type Account } from "./accounts";

export async function fetchCodexRateLimits(account?: Account): Promise<ProviderRateLimits> {
  ...
  const childId = account && account.profileDir ? `${USAGE_CHILD_ID}-${account.id}` : USAGE_CHILD_ID;
```

Replace every `USAGE_CHILD_ID` inside the function body with `childId`, and pass `accountEnv(account)` as the 5th argument to `spawnChild`.

- [ ] **Step 6: Run tests**

Run: `npx --prefix /Users/navaneet/Developer/monocode vitest run /Users/navaneet/Developer/monocode/src/lib/harness 2>&1 | tail -8`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git -C /Users/navaneet/Developer/monocode add src/lib/harness/claude.ts src/lib/harness/codex.ts src/lib/harness/claudeLive.test.ts src/lib/harness/codexLive.test.ts src/lib/rateLimitsFetch.ts
git -C /Users/navaneet/Developer/monocode commit -m "feat: run Claude and Codex sessions on the selected account"
```

---

### Task 6: Settings UI — accounts per provider, login terminal, auto-switch toggle

**Files:**
- Create: `src/surfaces/settings/ProviderAccounts.tsx` (list + actions, under 300 lines)
- Create: `src/surfaces/settings/AccountLoginTerminal.tsx` (embedded terminal that runs the login command and polls readiness)
- Create: `src/lib/accountLogin.ts` + `src/lib/accountLogin.test.ts` (pure: builds the shell command line)
- Modify: `src/surfaces/SettingsView.tsx` (`ProviderRow` renders `<ProviderAccounts harness=... />` for claude/codex; `GeneralPage` adds the auto-switch toggle). Export `Row`, `Toggle`, `SecondaryButton` from SettingsView or move them to `src/surfaces/settings/controls.tsx` and import them back. Moving is preferred so the new files can use them.

**Interfaces:**
- Consumes: accounts store from Task 4; `fetchClaudeRateLimits(profileDir)`, `fetchCodexRateLimits(account)`; `TerminalView` (`src/surfaces/TerminalView.tsx`, props `{ id, cwd, active, onMetaChange }`); `writePty`, `killPty` from `src/lib/pty.ts`; `IS_WIN` from `src/lib/platform.ts`
- Produces: `loginCommand(provider, profileDir, isWindows): string`

- [ ] **Step 1: Failing test for the command builder** (`src/lib/accountLogin.test.ts`)

```ts
import { expect, test } from "vitest";
import { loginCommand } from "./accountLogin";

test("claude login on unix exports CLAUDE_CONFIG_DIR", () => {
  expect(loginCommand("claude", "/p/w 1", false)).toBe(
    "export CLAUDE_CONFIG_DIR='/p/w 1' && claude auth login\n",
  );
});

test("codex login on windows sets env in powershell", () => {
  expect(loginCommand("codex", "C:\\p\\w", true)).toBe(
    "$env:CODEX_HOME='C:\\p\\w'; codex login\n",
  );
});

test("single quotes in the path are escaped", () => {
  expect(loginCommand("claude", "/p/o'k", false)).toBe(
    "export CLAUDE_CONFIG_DIR='/p/o'\\''k' && claude auth login\n",
  );
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `npx --prefix /Users/navaneet/Developer/monocode vitest run /Users/navaneet/Developer/monocode/src/lib/accountLogin.test.ts 2>&1 | tail -5`
Expected: FAIL, module not found.

- [ ] **Step 3: Implement `src/lib/accountLogin.ts`**

```ts
import type { AccountProvider } from "./accounts";

const ENV_KEY: Record<AccountProvider, string> = {
  claude: "CLAUDE_CONFIG_DIR",
  codex: "CODEX_HOME",
};
const LOGIN: Record<AccountProvider, string> = {
  claude: "claude auth login",
  codex: "codex login",
};

function shQuote(value: string): string {
  return `'${value.replace(/'/g, `'\\''`)}'`;
}

function psQuote(value: string): string {
  return `'${value.replace(/'/g, "''")}'`;
}

/** The line typed into the login terminal. Ends with a newline. */
export function loginCommand(
  provider: AccountProvider,
  profileDir: string,
  isWindows: boolean,
): string {
  if (isWindows) {
    return `$env:${ENV_KEY[provider]}=${psQuote(profileDir)}; ${LOGIN[provider]}\n`;
  }
  return `export ${ENV_KEY[provider]}=${shQuote(profileDir)} && ${LOGIN[provider]}\n`;
}

export const LOGIN_POLL_MS = 2000;
```

- [ ] **Step 4: Run test to verify pass**

Run: `npx --prefix /Users/navaneet/Developer/monocode vitest run /Users/navaneet/Developer/monocode/src/lib/accountLogin.test.ts 2>&1 | tail -5`
Expected: PASS.

- [ ] **Step 5: Move shared controls**

Create `src/surfaces/settings/controls.tsx` and move `Row`, `Toggle`, `Select`, `SecondaryButton` there verbatim from `SettingsView.tsx` (lines ~1989, ~2181, ~2217, ~2371), exporting each. In `SettingsView.tsx` delete the originals and add `import { Row, Select, SecondaryButton, Toggle } from "./settings/controls";`. Run `tsc --noEmit` to confirm nothing else broke.

- [ ] **Step 6: Implement `AccountLoginTerminal.tsx`**

```tsx
import { useEffect, useRef, useState } from "react";
import { TerminalView } from "../TerminalView";
import { accountLoginReady, type Account } from "../../lib/accounts";
import { loginCommand, LOGIN_POLL_MS } from "../../lib/accountLogin";
import { killPty, writePty } from "../../lib/pty";
import { IS_WIN } from "../../lib/platform";
import { homeDir } from "../../lib/fs";
import { SecondaryButton } from "./controls";

const FIRST_WRITE_DELAY_MS = 600;

export function AccountLoginTerminal({
  account,
  onDone,
  onCancel,
}: {
  account: Account;
  onDone: () => void;
  onCancel: () => void;
}) {
  const ptyId = `account-login-${account.id}`;
  const [cwd, setCwd] = useState<string | null>(null);
  const [status, setStatus] = useState<"starting" | "waiting" | "ready">("starting");
  const wrote = useRef(false);

  useEffect(() => {
    void homeDir().then(setCwd);
  }, []);

  useEffect(() => {
    if (!cwd || !account.profileDir || wrote.current) return;
    const command = loginCommand(account.provider, account.profileDir, IS_WIN);
    const timer = window.setTimeout(() => {
      wrote.current = true;
      void writePty(ptyId, command)
        .then(() => setStatus("waiting"))
        .catch((error: unknown) => console.error("login write failed", error));
    }, FIRST_WRITE_DELAY_MS);
    return () => window.clearTimeout(timer);
  }, [account.profileDir, account.provider, cwd, ptyId]);

  useEffect(() => {
    if (status !== "waiting") return;
    const poll = window.setInterval(() => {
      void accountLoginReady(account.id).then((ready) => {
        if (!ready) return;
        setStatus("ready");
        window.clearInterval(poll);
        void killPty(ptyId).catch(() => undefined);
        onDone();
      });
    }, LOGIN_POLL_MS);
    return () => window.clearInterval(poll);
  }, [account.id, onDone, ptyId, status]);

  useEffect(() => {
    return () => {
      void killPty(ptyId).catch(() => undefined);
    };
  }, [ptyId]);

  return (
    <div className="mt-3 overflow-hidden rounded-md border border-content/10">
      <div className="flex items-center justify-between border-b border-content/10 px-3 py-1.5 text-[12px] text-content/60">
        <span>
          {status === "ready"
            ? "Signed in."
            : "Finish signing in below. This panel closes when the login is saved."}
        </span>
        <SecondaryButton onClick={onCancel}>Cancel</SecondaryButton>
      </div>
      <div className="h-72">
        {cwd ? <TerminalView id={ptyId} cwd={cwd} active /> : null}
      </div>
    </div>
  );
}
```

Check `TerminalView`'s `Props` type: if `onMetaChange` is required, pass `onMetaChange={() => undefined}`.

- [ ] **Step 7: Implement `ProviderAccounts.tsx`**

```tsx
import { useCallback, useEffect, useState, useSyncExternalStore } from "react";
import {
  accountsFor,
  createAccount,
  DEFAULT_ACCOUNT_ID,
  getAccountsSnapshot,
  loadDefaultAccountId,
  refreshAccounts,
  removeAccount,
  renameAccount,
  saveDefaultAccountId,
  subscribeAccounts,
  type Account,
  type AccountProvider,
} from "../../lib/accounts";
import {
  fetchClaudeRateLimits,
  fetchCodexRateLimits,
} from "../../lib/rateLimitsFetch";
import {
  formatUsagePercent,
  formatRateLimitWindowChipLabel,
  type ProviderRateLimits,
} from "../../lib/rateLimits";
import { SecondaryButton } from "./controls";
import { AccountLoginTerminal } from "./AccountLoginTerminal";

export function ProviderAccounts({ provider }: { provider: AccountProvider }) {
  useSyncExternalStore(subscribeAccounts, getAccountsSnapshot, getAccountsSnapshot);
  const accounts = accountsFor(provider);
  const [usage, setUsage] = useState<Record<string, ProviderRateLimits>>({});
  const [pendingLogin, setPendingLogin] = useState<Account | null>(null);
  const [now, setNow] = useState(() => Date.now());
  const defaultId = loadDefaultAccountId(provider);

  useEffect(() => {
    void refreshAccounts();
  }, []);

  const refreshUsage = useCallback(() => {
    for (const account of accounts) {
      const job =
        provider === "claude"
          ? fetchClaudeRateLimits(account.profileDir)
          : fetchCodexRateLimits(account);
      void job.then((limits) => {
        setUsage((prev) => ({ ...prev, [account.id]: limits }));
        setNow(Date.now());
      });
    }
  }, [accounts, provider]);

  useEffect(() => {
    refreshUsage();
  }, [refreshUsage]);

  const onAdd = async () => {
    const label = window.prompt("Name this account (for example Work):");
    if (!label?.trim()) return;
    try {
      const account = await createAccount(provider, label.trim());
      setPendingLogin(account);
    } catch (error) {
      console.error("accounts_create failed", error);
      window.alert(error instanceof Error ? error.message : String(error));
    }
  };

  const onRename = async (account: Account) => {
    const label = window.prompt("Rename account:", account.label);
    if (!label?.trim() || label.trim() === account.label) return;
    await renameAccount(account.id, label.trim()).catch(console.error);
  };

  const onRemove = async (account: Account) => {
    if (!window.confirm(`Remove ${account.label}? Its login will be deleted.`)) return;
    await removeAccount(account.id).catch(console.error);
    if (defaultId === account.id) saveDefaultAccountId(provider, DEFAULT_ACCOUNT_ID);
  };

  return (
    <div className="mt-3 rounded-md border border-content/10">
      {accounts.map((account) => (
        <AccountRow
          key={account.id}
          account={account}
          isDefault={defaultId === account.id}
          limits={usage[account.id]}
          now={now}
          onDefault={() => saveDefaultAccountId(provider, account.id)}
          onRename={() => void onRename(account)}
          onRemove={() => void onRemove(account)}
          onSignIn={() => setPendingLogin(account)}
        />
      ))}
      <div className="flex items-center justify-between px-3 py-2">
        <span className="text-[12px] text-content/45">
          Each account signs in once and keeps its own login. Config and history are shared.
        </span>
        <SecondaryButton onClick={() => void onAdd()}>Add account</SecondaryButton>
      </div>
      {pendingLogin ? (
        <AccountLoginTerminal
          account={pendingLogin}
          onDone={() => {
            setPendingLogin(null);
            void refreshAccounts().then(refreshUsage);
          }}
          onCancel={() => setPendingLogin(null)}
        />
      ) : null}
    </div>
  );
}

function AccountRow({
  account,
  isDefault,
  limits,
  now,
  onDefault,
  onRename,
  onRemove,
  onSignIn,
}: {
  account: Account;
  isDefault: boolean;
  limits?: ProviderRateLimits;
  now: number;
  onDefault: () => void;
  onRename: () => void;
  onRemove: () => void;
  onSignIn: () => void;
}) {
  const signedOut = limits?.status === "unavailable";
  const isCustom = account.id !== DEFAULT_ACCOUNT_ID;
  return (
    <div className="flex items-center gap-3 border-b border-content/5 px-3 py-2 last:border-b-0">
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2 text-[13px] text-content">
          {account.label}
          {isDefault ? (
            <span className="rounded-full bg-content/10 px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide text-content/60">
              Default
            </span>
          ) : null}
        </div>
        <div className="truncate text-[12px] text-content/45">
          {account.identity ?? (signedOut ? "Not signed in" : "Signed in")}
        </div>
      </div>
      <UsageSummary limits={limits} now={now} />
      {signedOut && isCustom ? (
        <SecondaryButton onClick={onSignIn}>Sign in</SecondaryButton>
      ) : null}
      <SecondaryButton onClick={onDefault} disabled={isDefault}>
        {isDefault ? "Default" : "Use by default"}
      </SecondaryButton>
      {isCustom ? (
        <>
          <SecondaryButton onClick={onRename}>Rename</SecondaryButton>
          <SecondaryButton onClick={onRemove} danger>
            Remove
          </SecondaryButton>
        </>
      ) : null}
    </div>
  );
}

function UsageSummary({ limits, now }: { limits?: ProviderRateLimits; now: number }) {
  if (!limits || limits.status === "idle" || limits.status === "fetching") {
    return <span className="text-[12px] text-content/35">···</span>;
  }
  if (limits.status === "unavailable") {
    return <span className="text-[12px] text-content/35">not connected</span>;
  }
  const windows = [limits.session, limits.weekly].filter(
    (w): w is NonNullable<typeof w> => w != null,
  );
  if (windows.length === 0) {
    return <span className="text-[12px] text-content/35">no usage data</span>;
  }
  return (
    <span className="flex items-center gap-2 text-[12px] tabular-nums text-content/70">
      {windows.map((w, i) => (
        <span key={i}>
          {formatUsagePercent(w.usedPercent)} {formatRateLimitWindowChipLabel(w, now)}
        </span>
      ))}
    </span>
  );
}
```

Check the exact `ProviderRateLimits` fields in `src/lib/rateLimits.ts:8-25` (`session`, `weekly`, `status`, `error`, `provider`) and the signature of `formatRateLimitWindowChipLabel(window, now)`; adjust imports if names differ.

- [ ] **Step 8: Mount in `SettingsView.tsx`**

In `ProviderRow`, after the `<Row ...>...</Row>` element, render accounts for the two providers. `Row` returns a single div; wrap in a fragment:

```tsx
  return (
    <>
      <Row ...existing...>...</Row>
      {isAccountProvider(harness) && available ? (
        <ProviderAccounts provider={harness} />
      ) : null}
    </>
  );
```

Import `isAccountProvider` from `../lib/accounts` and `ProviderAccounts` from `./settings/ProviderAccounts`.

In `GeneralPage`, add state and a row next to the other toggles:

```tsx
  const [autoSwitchAccount, setAutoSwitchAccount] = useState(loadAutoSwitchAccount);
  const onAutoSwitchAccount = (next: boolean) => {
    saveAutoSwitchAccount(next);
    setAutoSwitchAccount(next);
  };
  ...
  <Row
    label="Switch account on rate limit"
    description="When a Claude Code or Codex turn fails with a rate limit, retry it once on another signed-in account with the most quota left."
  >
    <Toggle label="Switch account on rate limit" on={autoSwitchAccount} onChange={onAutoSwitchAccount} />
  </Row>
```

- [ ] **Step 9: Type-check and run the app**

Run: `npx --prefix /Users/navaneet/Developer/monocode tsc --noEmit -p /Users/navaneet/Developer/monocode 2>&1 | tail -5`
Expected: no errors.
Then `npm --prefix /Users/navaneet/Developer/monocode run tauri dev` and confirm: Settings → Providers shows a Default account under Claude Code and Codex with usage; Add account opens the terminal and the login command runs.

- [ ] **Step 10: Commit**

```bash
git -C /Users/navaneet/Developer/monocode add src/surfaces/settings src/surfaces/SettingsView.tsx src/lib/accountLogin.ts src/lib/accountLogin.test.ts
git -C /Users/navaneet/Developer/monocode commit -m "feat: manage provider accounts and sign in from Settings"
```

---

### Task 7: Account row in the model picker

**Files:**
- Modify: `src/chrome/ModelPicker.tsx` (`pickerSettings` ~100, the `settings` memo ~185, `SETTING_ORDER`)
- Create: `src/lib/accountSetting.ts` + `src/lib/accountSetting.test.ts`

**Interfaces:**
- Produces: `accountModelSetting(provider: AccountProvider, accounts: Account[], current: string): ModelSetting | null` (null when fewer than two accounts)

- [ ] **Step 1: Failing test** (`src/lib/accountSetting.test.ts`)

```ts
import { expect, test } from "vitest";
import { accountModelSetting } from "./accountSetting";
import type { Account } from "./accounts";

const a: Account = { id: "default", provider: "claude", label: "Default", profileDir: null, createdAt: 0 };
const b: Account = { id: "w1", provider: "claude", label: "Work", profileDir: "/p", identity: "w@x.y", createdAt: 1 };

test("returns null with a single account", () => {
  expect(accountModelSetting("claude", [a], "default")).toBeNull();
});

test("builds a select with labels and identity", () => {
  const setting = accountModelSetting("claude", [a, b], "w1");
  expect(setting?.id).toBe("account");
  expect(setting?.kind).toBe("select");
  expect(setting?.value).toBe("w1");
  expect(setting?.options).toEqual([
    { value: "default", label: "Default" },
    { value: "w1", label: "Work · w@x.y" },
  ]);
});
```

- [ ] **Step 2: Run to verify failure**

Run: `npx --prefix /Users/navaneet/Developer/monocode vitest run /Users/navaneet/Developer/monocode/src/lib/accountSetting.test.ts 2>&1 | tail -5`
Expected: FAIL.

- [ ] **Step 3: Implement**

```ts
import { ACCOUNT_SETTING_ID, type Account, type AccountProvider } from "./accounts";
import type { ModelSetting } from "./models";

export function accountModelSetting(
  provider: AccountProvider,
  accounts: Account[],
  current: string,
): ModelSetting | null {
  const mine = accounts.filter((account) => account.provider === provider);
  if (mine.length < 2) return null;
  return {
    id: ACCOUNT_SETTING_ID,
    label: "Account",
    kind: "select",
    value: current,
    description: "Which signed-in account this session runs on.",
    options: mine.map((account) => ({
      value: account.id,
      label: account.identity ? `${account.label} · ${account.identity}` : account.label,
    })),
  };
}
```

- [ ] **Step 4: Wire into `ModelPicker.tsx`**

Add subscriptions and append the setting:

```ts
import { accountIdFor, isAccountProvider, listAccounts, getAccountsSnapshot, refreshAccounts, subscribeAccounts } from "../lib/accounts";
import { accountModelSetting } from "../lib/accountSetting";
...
  const accountsVersion = useSyncExternalStore(subscribeAccounts, getAccountsSnapshot, getAccountsSnapshot);
  useEffect(() => { if (open) void refreshAccounts(); }, [open]);
...
  const settings = useMemo(() => {
    void catalogVersion;
    void accountsVersion;
    const base = pickerSettings(current).filter(
      (setting) => !hideEffort || !isEffortSetting(setting),
    );
    if (!isAccountProvider(harness)) return base;
    const account = accountModelSetting(
      harness,
      listAccounts(),
      accountIdFor(harness, values),
    );
    return account ? [...base, account] : base;
  }, [accountsVersion, catalogVersion, current, harness, hideEffort, values]);
```

Add `"account"` to the end of `SETTING_ORDER` so it sorts last. Selecting an option already calls `onSettingsChange({ ...values, [setting.id]: value })` through the existing setting submenu; confirm by reading the select handler. The account row therefore writes `values.account` with no further change.

- [ ] **Step 5: Run tests and type-check**

Run: `npx --prefix /Users/navaneet/Developer/monocode vitest run /Users/navaneet/Developer/monocode/src/lib/accountSetting.test.ts 2>&1 | tail -5 && npx --prefix /Users/navaneet/Developer/monocode tsc --noEmit -p /Users/navaneet/Developer/monocode 2>&1 | tail -3`
Expected: PASS, no type errors.

- [ ] **Step 6: Commit**

```bash
git -C /Users/navaneet/Developer/monocode add src/chrome/ModelPicker.tsx src/lib/accountSetting.ts src/lib/accountSetting.test.ts
git -C /Users/navaneet/Developer/monocode commit -m "feat: choose the session account from the model picker"
```

---

### Task 8: Footer usage follows the active session's account

**Files:**
- Modify: `src/chrome/UsageFooter.tsx:30-110`
- Modify: the `UsageFooter` call site in `src/App.tsx` (grep `<UsageFooter`) to pass `session={{ harness, modelSettings }}`

- [ ] **Step 1: Extend the session prop and fetches**

```ts
export type UsageFooterSession = {
  harness: HarnessId;
  modelSettings?: Record<string, string>;
};
```

Inside the component:

```ts
  const claudeAccount = accountFor("claude", session?.harness === "claude" ? session.modelSettings : undefined);
  const codexAccount = accountFor("codex", session?.harness === "codex" ? session.modelSettings : undefined);
  const claudeKey = claudeAccount?.id ?? "default";
  const codexKey = codexAccount?.id ?? "default";
```

Use `fetchClaudeRateLimits(claudeAccount?.profileDir ?? null)` and `fetchCodexRateLimits(codexAccount)`. Add `claudeKey` and `codexKey` to the `refresh` callback deps and add an effect that forces a refresh when either key changes:

```ts
  useEffect(() => { void refresh(true); }, [claudeKey, codexKey, refresh]);
```

Show the account label in the chip tooltip when it is not the default: in `ProviderChip` accept an optional `accountLabel?: string` and prefix the `title` with `${accountLabel} · `.

- [ ] **Step 2: Pass `modelSettings` from App**

Find `<UsageFooter` in `App.tsx` and extend the `session` prop with the active session's `modelSettings`.

- [ ] **Step 3: Type-check, run app, commit**

Run: `npx --prefix /Users/navaneet/Developer/monocode tsc --noEmit -p /Users/navaneet/Developer/monocode 2>&1 | tail -3`
Expected: no errors. In the running app, switch the account in the picker and confirm the footer usage changes.

```bash
git -C /Users/navaneet/Developer/monocode add src/chrome/UsageFooter.tsx src/App.tsx
git -C /Users/navaneet/Developer/monocode commit -m "feat: show usage for the active session account"
```

---

### Task 9: Automatic failover on rate limit

**Files:**
- Create: `src/lib/accounts/rateLimitDetect.ts` + `.test.ts`
- Create: `src/lib/accounts/selectFailover.ts` + `.test.ts`
- Modify: `src/App.tsx` (`onSubmit` turn runner around lines 4805-4905)

**Interfaces:**
- Produces:
  - `isRateLimitFailure(message: string): boolean`
  - `selectFailover(input: { provider, accounts: Account[], usage: Record<string, ProviderRateLimits>, exclude: string }): Account | null`

- [ ] **Step 1: Failing tests**

`src/lib/accounts/rateLimitDetect.test.ts`:

```ts
import { expect, test } from "vitest";
import { isRateLimitFailure } from "./rateLimitDetect";

test.each([
  "You've hit your limit · resets 3pm",
  "usage_limit_reached",
  "Rate limit exceeded, retry after 30s",
  "HTTP 429 Too Many Requests",
  "You have reached your usage limit",
  "quota exceeded for this account",
])("detects %s", (message) => {
  expect(isRateLimitFailure(message)).toBe(true);
});

test.each([
  "Claude Code exited",
  "Working directory does not exist",
  "Codex app-server exited",
  "",
])("ignores %s", (message) => {
  expect(isRateLimitFailure(message)).toBe(false);
});
```

`src/lib/accounts/selectFailover.test.ts`:

```ts
import { expect, test } from "vitest";
import { selectFailover } from "./selectFailover";
import type { Account } from "../accounts";
import type { ProviderRateLimits } from "../rateLimits";

const acct = (id: string): Account => ({ id, provider: "claude", label: id, profileDir: id === "default" ? null : `/p/${id}`, createdAt: 0 });
const usage = (used: number): ProviderRateLimits => ({
  provider: "claude",
  status: "ok",
  session: { usedPercent: used, resetsAt: null, windowMinutes: 300 },
  weekly: undefined,
} as unknown as ProviderRateLimits);

test("picks the other account with the lowest usage", () => {
  const picked = selectFailover({
    provider: "claude",
    accounts: [acct("default"), acct("a"), acct("b")],
    usage: { default: usage(100), a: usage(60), b: usage(20) },
    exclude: "default",
  });
  expect(picked?.id).toBe("b");
});

test("skips exhausted accounts and returns null when none remain", () => {
  const picked = selectFailover({
    provider: "claude",
    accounts: [acct("default"), acct("a")],
    usage: { default: usage(100), a: usage(100) },
    exclude: "default",
  });
  expect(picked).toBeNull();
});

test("treats accounts without usage data as available", () => {
  const picked = selectFailover({
    provider: "claude",
    accounts: [acct("default"), acct("a")],
    usage: { default: usage(100) },
    exclude: "default",
  });
  expect(picked?.id).toBe("a");
});

test("never returns the excluded account", () => {
  const picked = selectFailover({
    provider: "claude",
    accounts: [acct("default")],
    usage: {},
    exclude: "default",
  });
  expect(picked).toBeNull();
});
```

Adjust the `usage` helper to the real `RateLimitWindow` shape in `src/lib/rateLimits.ts:8-16`.

- [ ] **Step 2: Run to verify failure**

Run: `npx --prefix /Users/navaneet/Developer/monocode vitest run /Users/navaneet/Developer/monocode/src/lib/accounts 2>&1 | tail -5`
Expected: FAIL, modules not found.

- [ ] **Step 3: Implement**

`src/lib/accounts/rateLimitDetect.ts`:

```ts
const PATTERNS: RegExp[] = [
  /hit your limit/i,
  /usage[_ ]limit/i,
  /rate[_ ]limit/i,
  /\b429\b/,
  /too many requests/i,
  /quota/i,
];

export function isRateLimitFailure(message: string | undefined | null): boolean {
  if (!message) return false;
  return PATTERNS.some((pattern) => pattern.test(message));
}
```

`src/lib/accounts/selectFailover.ts`:

```ts
import type { Account, AccountProvider } from "../accounts";
import type { ProviderRateLimits } from "../rateLimits";

const EXHAUSTED_PERCENT = 100;

function tightestUsed(limits: ProviderRateLimits | undefined): number {
  if (!limits || limits.status !== "ok") return 0;
  const windows = [limits.session, limits.weekly].filter((w) => w != null);
  return windows.reduce((max, w) => Math.max(max, w.usedPercent), 0);
}

export function selectFailover(input: {
  provider: AccountProvider;
  accounts: Account[];
  usage: Record<string, ProviderRateLimits>;
  exclude: string;
}): Account | null {
  const candidates = input.accounts
    .filter((a) => a.provider === input.provider && a.id !== input.exclude)
    .map((account) => ({ account, used: tightestUsed(input.usage[account.id]) }))
    .filter((entry) => entry.used < EXHAUSTED_PERCENT)
    .sort((a, b) => a.used - b.used);
  return candidates[0]?.account ?? null;
}
```

- [ ] **Step 4: Run tests**

Run: `npx --prefix /Users/navaneet/Developer/monocode vitest run /Users/navaneet/Developer/monocode/src/lib/accounts 2>&1 | tail -5`
Expected: PASS.

- [ ] **Step 5: Orchestrate in `App.tsx`**

Create `src/lib/accounts/failover.ts` with the async policy so App stays thin:

```ts
import { accountsFor, findAccount, refreshAccounts, type Account, type AccountProvider } from "../accounts";
import { fetchClaudeRateLimits, fetchCodexRateLimits } from "../rateLimitsFetch";
import type { ProviderRateLimits } from "../rateLimits";
import { selectFailover } from "./selectFailover";

/** Refreshes usage for every account of `provider` and picks the failover. */
export async function chooseFailoverAccount(
  provider: AccountProvider,
  currentId: string,
): Promise<Account | null> {
  await refreshAccounts();
  const accounts = accountsFor(provider);
  if (accounts.length < 2 || !findAccount(provider, currentId)) return null;
  const usage: Record<string, ProviderRateLimits> = {};
  await Promise.all(
    accounts.map(async (account) => {
      usage[account.id] =
        provider === "claude"
          ? await fetchClaudeRateLimits(account.profileDir)
          : await fetchCodexRateLimits(account);
    }),
  );
  return selectFailover({ provider, accounts, usage, exclude: currentId });
}
```

In `App.tsx`, inside the turn runner:

1. Add a local `let rateLimitMessage: string | null = null;` next to `providerFailureSeen`.
2. In `routePlanEvent` (which already sees every event), after `if (event.type === "session.error") providerFailureSeen = true;` add `if (event.type === "session.error" && isRateLimitFailure(event.message)) rateLimitMessage = event.message;`.
3. In the `catch`, after computing `message`, add `if (isRateLimitFailure(message)) rateLimitMessage = message;`.
4. The `onSubmit` runner takes `options`; add `options?.failedOver?: boolean` to its type.
5. After the `finally` block (where `buildSucceeded` is known), add:

```ts
        if (
          rateLimitMessage &&
          !options?.failedOver &&
          loadAutoSwitchAccount() &&
          isAccountProvider(current.harness)
        ) {
          const provider = current.harness;
          const fromId = accountIdFor(provider, current.modelSettings);
          const next = await chooseFailoverAccount(provider, fromId);
          if (next && turnGen.current.get(sessionId) === gen) {
            const modelSettings = withAccountSetting(current.modelSettings, next.id);
            setSessions((prev) =>
              prev.map((s) => (s.id === sessionId ? { ...s, modelSettings } : s)),
            );
            enqueueHarnessEvent(sessionId, {
              type: "status",
              text: `Switched to ${next.label} after a rate limit`,
            });
            void onSubmit(sessionId, text, attachments, { ...options, failedOver: true });
            return;
          }
        }
```

`onSubmit` is a `useCallback`; if it is not referenceable inside itself, store it in a ref (`onSubmitRef.current`) the way other callbacks in App do (search for `Ref.current = ` patterns). `text` and `attachments` are the runner's own parameters. Imports: `isRateLimitFailure` from `./lib/accounts/rateLimitDetect`, `chooseFailoverAccount` from `./lib/accounts/failover`, `accountIdFor`, `isAccountProvider`, `loadAutoSwitchAccount`, `withAccountSetting` from `./lib/accounts`.

Because the adapters' settings key includes the account (Task 5), the retried turn respawns the child under the new account and resumes the same provider conversation.

- [ ] **Step 6: Type-check and manual test**

Run: `npx --prefix /Users/navaneet/Developer/monocode tsc --noEmit -p /Users/navaneet/Developer/monocode 2>&1 | tail -3`
Expected: no errors.

Manual: with two Claude accounts signed in, temporarily make the failing check fire by sending a prompt while the default account is exhausted, or simulate by adding `if (text === "/fail-rl") throw new Error("usage_limit_reached")` at the top of the Claude adapter's `sendTurn` during testing (remove before commit). Confirm the status line appears, the model picker shows the new account, and the conversation continues.

- [ ] **Step 7: Commit**

```bash
git -C /Users/navaneet/Developer/monocode add src/lib/accounts src/App.tsx
git -C /Users/navaneet/Developer/monocode commit -m "feat: fail over to another account when a turn hits a rate limit"
```

---

### Task 10: Spec alignment, full check, docs

**Files:**
- Modify: `docs/superpowers/specs/2026-09-15-multi-account-design.md`
- Modify: `README.md` (short "Accounts" note under "Some notes")

- [ ] **Step 1: Update the spec to match what was built**

Replace the "Data model" section with: accounts live in `<app data dir>/accounts/accounts.json`; the session's account id lives in `modelSettings.account`; no SQLite changes. Replace the `pty_spawn` env bullet with: the login terminal is the normal shell and MonoCode types the `export VAR=... && <login>` line into it. Remove `accounts_env` and `accounts_identity` from the command list (identity is returned by `accounts_list`).

- [ ] **Step 2: README note**

Under "## Some notes" add:

```markdown
- **Multiple accounts.** Settings → Providers lets you sign in to more than one Claude Code or Codex account. Pick the account per session from the model picker, or let MonoCode switch to the account with the most quota when a turn hits a rate limit (General → Switch account on rate limit).
```

- [ ] **Step 3: Full check**

Run: `npm --prefix /Users/navaneet/Developer/monocode run check 2>&1 | tail -20`
Expected: vitest all green, `tsc` clean, `cargo fmt --check` clean, clippy no warnings, cargo tests pass. Fix anything that fails.

- [ ] **Step 4: Commit**

```bash
git -C /Users/navaneet/Developer/monocode add docs README.md
git -C /Users/navaneet/Developer/monocode commit -m "docs: describe multi-account support"
```
