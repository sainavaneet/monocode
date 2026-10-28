//! Per-provider API keys the user stores in Settings › Providers.
//!
//! Keys live in `<data dir>/api-keys/<provider>` with mode 0600, the same path
//! the desktop app uses, so both apps share them. Callers learn only whether a
//! key exists and its last four characters; `apply` hands the key to the CLI
//! as an environment variable.
use crate::provider::Kind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Codex app-server ignores `OPENAI_API_KEY` for its built-in provider while a
/// ChatGPT login is cached, so an explicit env-key provider is selected.
pub const CODEX_MODEL_PROVIDER: &str = "monocode-api";
const CODEX_PROVIDER_DEFINITION: &str = "model_providers.monocode-api={name=\"OpenAI API\",base_url=\"https://api.openai.com/v1\",env_key=\"OPENAI_API_KEY\",wire_api=\"responses\",requires_openai_auth=false}";
const MAX_KEY_LENGTH: usize = 512;
static GEMINI_CATALOG_KEY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
pub fn configure_gemini_catalog(enabled: bool) {
    if GEMINI_CATALOG_KEY.swap(enabled, std::sync::atomic::Ordering::Relaxed) != enabled {
        crate::catalog::invalidate(Kind::Gemini, &crate::accounts::Profile::default());
    }
}
pub(crate) fn gemini_catalog_key() -> bool {
    GEMINI_CATALOG_KEY.load(std::sync::atomic::Ordering::Relaxed)
}
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub configured: bool,
    /// Last four characters, for "Key ending in …abcd".
    pub hint: Option<String>,
}

/// Where keys are stored for the global forms. Set once at startup.
pub fn init(data_dir: PathBuf) {
    let _ = DATA_DIR.set(data_dir);
}
fn data_dir() -> Result<&'static Path, String> {
    DATA_DIR
        .get()
        .map(PathBuf::as_path)
        .ok_or_else(|| "API keys are not available in this build".to_owned())
}
pub fn supports(kind: Kind) -> bool {
    env_var(kind).is_ok()
}
/// The env var the provider's CLI reads its key from.
pub fn env_var(kind: Kind) -> Result<&'static str, String> {
    match kind {
        Kind::Claude => Ok("ANTHROPIC_API_KEY"),
        Kind::Codex => Ok("OPENAI_API_KEY"),
        Kind::Gemini => Ok("GEMINI_API_KEY"),
        _ => Err(format!("{} does not take an API key", kind.label())),
    }
}
fn key_path(dir: &Path, kind: Kind) -> Result<PathBuf, String> {
    env_var(kind)?;
    Ok(dir.join("api-keys").join(kind.id()))
}
fn read(dir: &Path, kind: Kind) -> Result<Option<String>, String> {
    let path = key_path(dir, kind)?;
    match std::fs::read_to_string(&path) {
        Ok(raw) => {
            let key = raw.trim().to_owned();
            Ok((!key.is_empty()).then_some(key))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Could not read the saved API key: {e}")),
    }
}
fn status_of(key: Option<&str>) -> Status {
    Status {
        configured: key.is_some(),
        hint: key.map(|key| {
            let chars: Vec<char> = key.chars().collect();
            chars[chars.len().saturating_sub(4)..].iter().collect()
        }),
    }
}
pub fn status_in(dir: &Path, kind: Kind) -> Result<Status, String> {
    Ok(status_of(read(dir, kind)?.as_deref()))
}
pub fn set_in(dir: &Path, kind: Kind, key: &str) -> Result<Status, String> {
    let key = key.trim();
    if key.is_empty() {
        return clear_in(dir, kind);
    }
    if key.chars().any(char::is_whitespace) {
        return Err("An API key cannot contain spaces or line breaks".into());
    }
    if key.len() > MAX_KEY_LENGTH {
        return Err("That does not look like an API key".into());
    }
    let path = key_path(dir, kind)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Could not create the API key directory: {e}"))?;
    }
    write_secret(&path, key)?;
    Ok(status_of(Some(key)))
}
pub fn clear_in(dir: &Path, kind: Kind) -> Result<Status, String> {
    let path = key_path(dir, kind)?;
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("Could not remove the saved API key: {e}")),
    }
    Ok(status_of(None))
}
/// Put the stored key into the child's environment. Fails loudly when no key
/// is saved so a turn is never quietly billed to the subscription instead.
pub fn apply_in(dir: &Path, kind: Kind, command: &mut Command) -> Result<(), String> {
    let env = env_var(kind)?;
    let key = read(dir, kind)?.ok_or_else(|| {
        format!(
            "Use API key is on for {}, but no key is saved. Add one in Settings › Providers.",
            kind.label()
        )
    })?;
    command.env(env, &key);
    if kind == Kind::Gemini {
        // API billing has its own Gemini profile. Never rewrite or clear the
        // user's cached Google login. Project settings remain CLI-managed.
        let home = dir.join("provider-accounts/gemini/api-key");
        let config = home.join(".gemini");
        std::fs::create_dir_all(&config).map_err(|_| "Cannot create Gemini API profile")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| "Cannot protect Gemini API profile")?;
        }
        let path = config.join("settings.json");
        let mut settings: serde_json::Value = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw)
                .map_err(|_| "Cannot parse Gemini API profile settings")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
            Err(_) => return Err("Cannot read Gemini API profile settings".into()),
        };
        if !settings.is_object()
            || (!settings["security"].is_null() && !settings["security"].is_object())
            || (!settings["security"]["auth"].is_null()
                && !settings["security"]["auth"].is_object())
        {
            return Err("Invalid Gemini API profile authentication settings".into());
        }
        settings["security"]["auth"]["selectedType"] = "gemini-api-key".into();
        let temporary = tempfile::NamedTempFile::new_in(&config)
            .map_err(|_| "Cannot create Gemini API settings")?;
        write_secret(temporary.path(), &settings.to_string())?;
        temporary
            .persist(&path)
            .map_err(|_| "Cannot save Gemini API settings")?;
        command
            .env("GEMINI_CLI_HOME", home)
            .env("GOOGLE_API_KEY", &key)
            .env_remove("GOOGLE_GENAI_USE_VERTEXAI")
            .env_remove("GOOGLE_GENAI_USE_GCA")
            .env("GEMINI_CLI_AUTH_OVERRIDE", "gemini-api-key");
    }
    if kind == Kind::Codex {
        command.args([
            "-c",
            &format!("model_provider=\"{CODEX_MODEL_PROVIDER}\""),
            "-c",
            CODEX_PROVIDER_DEFINITION,
        ]);
    }
    Ok(())
}
pub fn status(kind: Kind) -> Result<Status, String> {
    status_in(data_dir()?, kind)
}
pub fn set(kind: Kind, key: &str) -> Result<Status, String> {
    set_in(data_dir()?, kind, key)
}
pub fn clear(kind: Kind) -> Result<Status, String> {
    clear_in(data_dir()?, kind)
}
pub fn apply(kind: Kind, command: &mut Command) -> Result<(), String> {
    apply_in(data_dir()?, kind, command)
}
fn write_secret(path: &Path, key: &str) -> Result<(), String> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("Could not save the API key: {e}"))?;
    file.write_all(key.as_bytes())
        .map_err(|e| format!("Could not save the API key: {e}"))?;
    file.sync_all().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }
    fn envs(command: &Command) -> Vec<(String, Option<String>)> {
        command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }
    fn args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn set_stores_the_key_owner_only_and_reports_its_last_four_characters() {
        let dir = dir();
        let status = set_in(dir.path(), Kind::Claude, "  sk-ant-example-1234  ").unwrap();
        assert_eq!(
            status,
            Status {
                configured: true,
                hint: Some("1234".into())
            }
        );
        let path = dir.path().join("api-keys/claude");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "sk-ant-example-1234"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(status_in(dir.path(), Kind::Claude).unwrap(), status);
        assert_eq!(
            status_in(dir.path(), Kind::Codex).unwrap(),
            Status {
                configured: false,
                hint: None
            }
        );
    }

    #[test]
    fn gemini_api_billing_is_child_scoped_and_does_not_expose_key_in_arguments() {
        let dir = dir();
        set_in(dir.path(), Kind::Gemini, "test-gemini-1234").unwrap();
        let mut command = Command::new("gemini");
        apply_in(dir.path(), Kind::Gemini, &mut command).unwrap();
        let env = envs(&command);
        assert!(env.contains(&("GEMINI_API_KEY".into(), Some("test-gemini-1234".into()))));
        assert!(env.contains(&("GOOGLE_API_KEY".into(), Some("test-gemini-1234".into()))));
        assert!(args(&command).is_empty());
        let overlay: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                dir.path()
                    .join("provider-accounts/gemini/api-key/.gemini/settings.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            overlay["security"]["auth"]["selectedType"],
            "gemini-api-key"
        );
        assert!(!overlay.to_string().contains("test-gemini-1234"));
        assert_eq!(
            status_in(dir.path(), Kind::Gemini).unwrap().hint.as_deref(),
            Some("1234")
        );
    }

    #[test]
    fn a_blank_key_clears_and_inner_whitespace_is_rejected() {
        let dir = dir();
        set_in(dir.path(), Kind::Codex, "sk-live").unwrap();
        assert!(set_in(dir.path(), Kind::Codex, "sk live").is_err());
        assert!(status_in(dir.path(), Kind::Codex).unwrap().configured);
        assert!(!set_in(dir.path(), Kind::Codex, "   ").unwrap().configured);
        assert!(!dir.path().join("api-keys/codex").exists());
        assert!(!clear_in(dir.path(), Kind::Codex).unwrap().configured);
    }

    #[test]
    fn apply_sets_the_env_and_codex_selects_the_api_provider() {
        let dir = dir();
        set_in(dir.path(), Kind::Claude, "sk-ant-1").unwrap();
        set_in(dir.path(), Kind::Codex, "sk-oa-2").unwrap();
        let mut claude = Command::new("claude");
        apply_in(dir.path(), Kind::Claude, &mut claude).unwrap();
        assert_eq!(
            envs(&claude),
            vec![("ANTHROPIC_API_KEY".to_owned(), Some("sk-ant-1".to_owned()))]
        );
        assert!(args(&claude).is_empty());
        let mut codex = Command::new("codex");
        apply_in(dir.path(), Kind::Codex, &mut codex).unwrap();
        assert_eq!(
            envs(&codex),
            vec![("OPENAI_API_KEY".to_owned(), Some("sk-oa-2".to_owned()))]
        );
        let args = args(&codex);
        assert_eq!(args[0], "-c");
        assert_eq!(args[1], "model_provider=\"monocode-api\"");
        assert!(args[3].contains("env_key=\"OPENAI_API_KEY\""));
        assert!(args[3].contains("requires_openai_auth=false"));
    }

    #[test]
    fn apply_fails_loudly_without_a_saved_key_and_for_other_providers() {
        let dir = dir();
        let mut command = Command::new("claude");
        let error = apply_in(dir.path(), Kind::Claude, &mut command).unwrap_err();
        assert!(error.contains("no key is saved"), "{error}");
        assert!(envs(&command).is_empty());
        assert!(!supports(Kind::Cursor));
        assert!(set_in(dir.path(), Kind::Cursor, "x").is_err());
        assert!(status(Kind::Claude).is_err(), "no global directory yet");
    }
}
