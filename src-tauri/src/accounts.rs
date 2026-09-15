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
const DEFAULT_ACCOUNT_ID: &str = "default";
pub const KEYCHAIN_SERVICE_PREFIX: &str = "Claude Code-credentials";

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

/// Applies a caller-supplied env to a child after checking the allowlist.
/// The frontend only ever needs to point a CLI at a profile directory, so
/// anything else is rejected rather than forwarded.
pub fn apply_env(cmd: &mut Command, env: Option<&HashMap<String, String>>) -> Result<(), String> {
    let Some(env) = env else { return Ok(()) };
    validate_env(env)?;
    for (key, value) in env {
        cmd.env(key, value);
    }
    Ok(())
}

/// Claude Code stores a non-default profile's OAuth blob under a per-profile
/// Keychain service: the prefix plus the first 8 hex chars of sha256(dir).
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
    if link.symlink_metadata().is_ok() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        if target.is_dir() {
            std::os::windows::fs::symlink_dir(target, link).or_else(|_| copy_dir(target, link))
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
/// Returns the entries that were skipped because the source is missing or
/// the link failed.
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
    let Ok(raw) = std::fs::read_to_string(&source) else {
        return;
    };
    let Some(stripped) = strip_claude_json(&raw) else {
        return;
    };
    let Ok(text) = serde_json::to_string_pretty(&stripped) else {
        return;
    };
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

/// The directory the CLI reads for this account: its profile, or the real
/// `~/.claude` / `~/.codex` for the default account.
fn config_dir(account: &Account) -> Option<PathBuf> {
    match &account.profile_dir {
        Some(dir) => Some(PathBuf::from(dir)),
        None => provider_home(&account.provider),
    }
}

fn claude_json_path(account: &Account) -> Option<PathBuf> {
    match &account.profile_dir {
        Some(dir) => Some(PathBuf::from(dir).join(".claude.json")),
        None => Some(PathBuf::from(dirs_home()?).join(".claude.json")),
    }
}

fn read_identity(account: &Account) -> Option<String> {
    match account.provider.as_str() {
        "claude" => {
            let raw = std::fs::read_to_string(claude_json_path(account)?).ok()?;
            claude_identity_from_json(&raw)
        }
        "codex" => {
            let raw = std::fs::read_to_string(config_dir(account)?.join("auth.json")).ok()?;
            codex_identity_from_auth(&raw)
        }
        _ => None,
    }
}

fn keychain_service_for(account: &Account) -> String {
    match &account.profile_dir {
        Some(dir) => claude_keychain_service(Path::new(dir)),
        None => KEYCHAIN_SERVICE_PREFIX.to_string(),
    }
}

#[cfg(target_os = "macos")]
fn security_status(args: &[&str]) -> bool {
    Command::new("security")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn claude_keychain_ready(account: &Account) -> bool {
    let service = keychain_service_for(account);
    security_status(&["find-generic-password", "-s", &service])
}

#[cfg(not(target_os = "macos"))]
fn claude_keychain_ready(_account: &Account) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn claude_keychain_delete(account: &Account) {
    let service = keychain_service_for(account);
    let _ = security_status(&["delete-generic-password", "-s", &service]);
}

#[cfg(not(target_os = "macos"))]
fn claude_keychain_delete(_account: &Account) {}

fn login_ready(account: &Account) -> bool {
    let Some(dir) = config_dir(account) else {
        return false;
    };
    match account.provider.as_str() {
        "claude" => dir.join(".credentials.json").exists() || claude_keychain_ready(account),
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
        id: DEFAULT_ACCOUNT_ID.into(),
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
    /// Runs `f` against the stored accounts and persists when they changed.
    fn with<T>(
        &self,
        app: &AppHandle,
        f: impl FnOnce(&mut Vec<Account>) -> Result<T, String>,
    ) -> Result<T, String> {
        let file = accounts_root(app)?.join(ACCOUNTS_FILE);
        let mut guard = self
            .loaded
            .lock()
            .map_err(|_| "accounts store poisoned".to_string())?;
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

fn list_all(app: &AppHandle, store: &AccountStore) -> Result<Vec<Account>, String> {
    let extra = store.with(app, |accounts| Ok(accounts.clone()))?;
    // Repair links the user may have created since (a new plugins dir, a
    // first CLAUDE.md). Idempotent and a handful of stat calls.
    for account in &extra {
        if let (Some(profile), Some(source)) =
            (&account.profile_dir, provider_home(&account.provider))
        {
            link_shared(Path::new(profile), &source, &account.provider);
        }
    }
    let mut all = vec![default_account("claude"), default_account("codex")];
    all.extend(extra);
    Ok(all.into_iter().map(with_identity).collect())
}

/// Every account, defaults first. The default for each provider is the
/// user's real `~/.claude` / `~/.codex` login and is never stored.
#[tauri::command(async)]
pub fn accounts_list(
    app: AppHandle,
    store: State<'_, AccountStore>,
) -> Result<Vec<Account>, String> {
    list_all(&app, &store)
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
    if id == DEFAULT_ACCOUNT_ID {
        return Err("accounts: the default account cannot be removed".into());
    }
    let removed = store.with(&app, |accounts| {
        let Some(index) = accounts.iter().position(|a| a.id == id) else {
            return Err("accounts: not found".into());
        };
        Ok(accounts.remove(index))
    })?;
    if removed.provider == "claude" {
        claude_keychain_delete(&removed);
    }
    if let Some(dir) = &removed.profile_dir {
        let _ = std::fs::remove_dir_all(dir);
    }
    Ok(())
}

#[tauri::command(async)]
pub fn accounts_login_ready(
    app: AppHandle,
    store: State<'_, AccountStore>,
    id: String,
    provider: String,
) -> Result<bool, String> {
    let accounts = list_all(&app, &store)?;
    let Some(account) = accounts
        .into_iter()
        .find(|a| a.id == id && a.provider == provider)
    else {
        return Err("accounts: not found".into());
    };
    Ok(login_ready(&account))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("mc-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

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
    fn apply_env_sets_allowed_keys_on_the_command() {
        let mut env = HashMap::new();
        env.insert(CODEX_HOME_ENV.to_string(), "/tmp/b".to_string());
        let mut cmd = Command::new("true");
        apply_env(&mut cmd, Some(&env)).unwrap();
        assert!(cmd
            .get_envs()
            .any(|(k, v)| k == CODEX_HOME_ENV && v == Some(std::ffi::OsStr::new("/tmp/b"))));
        assert!(apply_env(&mut Command::new("true"), None).is_ok());
    }

    #[test]
    fn keychain_service_uses_first_8_hex_of_sha256() {
        let service = claude_keychain_service(Path::new("/tmp/profile"));
        let prefix = format!("{KEYCHAIN_SERVICE_PREFIX}-");
        assert!(service.starts_with(&prefix));
        assert_eq!(service.len(), prefix.len() + 8);
        assert!(service[prefix.len()..]
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
        assert_ne!(service, claude_keychain_service(Path::new("/tmp/other")));
    }

    #[test]
    fn shared_entries_exclude_credentials() {
        assert!(!shared_entries("claude").contains(&".credentials.json"));
        assert!(!shared_entries("codex").contains(&"auth.json"));
        assert!(shared_entries("claude").contains(&"projects"));
        assert!(shared_entries("codex").contains(&"sessions"));
        assert!(shared_entries("cursor").is_empty());
    }

    #[test]
    fn link_shared_links_existing_and_skips_missing() {
        let root = temp_root("acct");
        let source = root.join("source");
        let profile = root.join("profile");
        std::fs::create_dir_all(source.join("projects")).unwrap();
        std::fs::write(source.join("CLAUDE.md"), "hi").unwrap();
        std::fs::write(source.join(".credentials.json"), "{}").unwrap();
        std::fs::create_dir_all(&profile).unwrap();

        let skipped = link_shared(&profile, &source, "claude");

        assert!(profile.join("CLAUDE.md").exists());
        assert!(profile.join("projects").is_dir());
        assert!(skipped.contains(&"settings.json".to_string()));
        assert!(!profile.join(".credentials.json").exists());
        // Linking again is a no-op rather than an error.
        let again = link_shared(&profile, &source, "claude");
        assert_eq!(again, skipped);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn strip_claude_json_removes_account_fields() {
        let raw = r#"{"oauthAccount":{"emailAddress":"a@b.c"},"userID":"u","mcpServers":{"x":{}}}"#;
        let out = strip_claude_json(raw).unwrap();
        assert!(out.get("oauthAccount").is_none());
        assert!(out.get("userID").is_none());
        assert!(out.get("mcpServers").is_some());
        assert!(strip_claude_json("not json").is_none());
    }

    #[test]
    fn identity_from_claude_json_reads_email() {
        let raw = r#"{"oauthAccount":{"emailAddress":"a@b.c"}}"#;
        assert_eq!(claude_identity_from_json(raw), Some("a@b.c".to_string()));
        assert_eq!(claude_identity_from_json("{}"), None);
    }

    #[test]
    fn identity_from_codex_auth_reads_jwt_email() {
        // payload {"email":"c@d.e"} in base64url without padding.
        let raw = r#"{"tokens":{"id_token":"xxx.eyJlbWFpbCI6ImNAZC5lIn0.yyy"}}"#;
        assert_eq!(codex_identity_from_auth(raw), Some("c@d.e".to_string()));
        assert_eq!(codex_identity_from_auth(r#"{"tokens":{}}"#), None);
    }

    #[test]
    fn login_ready_checks_provider_credential_files() {
        let root = temp_root("login");
        let claude = Account {
            id: "c".into(),
            provider: "claude".into(),
            label: "C".into(),
            profile_dir: Some(root.join("c").to_string_lossy().into_owned()),
            identity: None,
            created_at: 0,
        };
        let codex = Account {
            id: "x".into(),
            provider: "codex".into(),
            label: "X".into(),
            profile_dir: Some(root.join("x").to_string_lossy().into_owned()),
            identity: None,
            created_at: 0,
        };
        std::fs::create_dir_all(root.join("c")).unwrap();
        std::fs::create_dir_all(root.join("x")).unwrap();
        assert!(!login_ready(&codex));
        std::fs::write(root.join("x/auth.json"), "{}").unwrap();
        assert!(login_ready(&codex));
        std::fs::write(root.join("c/.credentials.json"), "{}").unwrap();
        assert!(login_ready(&claude));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn store_roundtrip_persists_accounts() {
        let root = temp_root("store");
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
        assert_eq!(loaded[0].profile_dir.as_deref(), Some("/tmp/x"));
        assert!(load_accounts_file(&root.join("missing.json")).is_empty());
        let _ = std::fs::remove_dir_all(root);
    }
}
