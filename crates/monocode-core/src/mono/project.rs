//! Provider-native project selection for a Mono. Never merge project configs.
use super::Mono;
use crate::provider::Kind;
use std::path::{Path, PathBuf};

pub const REVISION_ENV: &str = "MONOCODE_MONO_PROJECT_REVISION";

/// Where a Mono's provider session runs: its one assigned project, or with
/// several, the assigned project `fallback` lies in (the chat's own project),
/// else the first assigned one that exists. One project's configuration is
/// loaded; configurations are never merged, and the context names the
/// chosen one so the Mono can say so.
pub fn directory(mono: &Mono, fallback: &Path) -> Result<PathBuf, String> {
    let resolve = |project: &String| -> Result<PathBuf, String> {
        let path = crate::scm::expand_home(Path::new(project));
        if !path.is_absolute() || !path.is_dir() {
            return Err("Mono's assigned project is unavailable. Choose an existing absolute project directory in Mono settings.".into());
        }
        path.canonicalize()
            .map_err(|_| "Cannot resolve Mono's assigned project directory.".into())
    };
    match mono.projects.as_slice() {
        [] => Ok(fallback.to_path_buf()),
        [project] => resolve(project),
        projects => {
            let wanted = fallback.canonicalize().ok();
            let containing = projects.iter().find(|project| {
                let Ok(path) = resolve(project) else {
                    return false;
                };
                wanted.as_ref().is_some_and(|w| w.starts_with(&path))
            });
            match containing {
                Some(project) => resolve(project),
                None => projects
                    .iter()
                    .find_map(|project| resolve(project).ok())
                    .ok_or_else(|| {
                        "None of Mono's assigned projects exists. Choose existing absolute project directories in Mono settings.".to_string()
                    }),
            }
        }
    }
}

/// File contents stay local; only a digest enters the transport cache key.
/// Provider-specific files are observed, never translated between providers.
pub fn revision(kind: Kind, cwd: &Path) -> Result<String, String> {
    let files: &[&str] = match kind {
        Kind::Claude => &[
            ".mcp.json",
            ".claude/settings.json",
            ".claude/settings.local.json",
        ],
        Kind::Codex => &[".codex/config.toml"],
        _ => &[],
    };
    let mut stamp = format!("{}:{}", kind.id(), cwd.display());
    for file in files {
        let path = cwd.join(file);
        let value = match std::fs::read_to_string(&path) {
            Ok(value) => value,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "<absent>".into(),
            Err(_) => {
                return Err(format!(
                    "Cannot read project configuration {}",
                    path.display()
                ))
            }
        };
        stamp.push_str(file);
        stamp.push_str(&super::files::content_hash(&value));
    }
    if kind == Kind::Claude {
        let names = claude_servers(cwd)?;
        if names.iter().any(|n| n == "monocode_visual") {
            return Err("Project MCP server name monocode_visual conflicts with MonoCode's injected server. Rename that project registration before starting Mono.".into());
        }
    }
    Ok(super::files::content_hash(&stamp))
}

pub fn claude_servers(cwd: &Path) -> Result<Vec<String>, String> {
    let path = cwd.join(".mcp.json");
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => {
            return Err(format!(
                "Cannot read project MCP configuration {}",
                path.display()
            ))
        }
    };
    // A file Claude cannot read registers nothing; it must not stop the
    // Mono from talking at all. The footer's MCP check reports it.
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return Ok(Vec::new());
    };
    Ok(value
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
        .map(|servers| servers.keys().cloned().collect())
        .unwrap_or_default())
}

/// Project registrations only. Never substitute user-scope servers for a project's graph.
pub fn registrations(kind: Kind, cwd: &Path) -> Result<Vec<(String, bool)>, String> {
    match kind {
        Kind::Claude => Ok(claude_servers(cwd)?
            .into_iter()
            .map(|name| (name, true))
            .collect()),
        Kind::Codex => {
            let path = cwd.join(".codex/config.toml");
            let raw = match std::fs::read_to_string(path) {
                Ok(raw) => raw,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(_) => return Err("Cannot read this project's Codex configuration".into()),
            };
            let value: toml::Value = toml::from_str(&raw)
                .map_err(|_| "Invalid project Codex configuration".to_string())?;
            Ok(value
                .get("mcp_servers")
                .and_then(toml::Value::as_table)
                .into_iter()
                .flatten()
                .map(|(name, server)| {
                    (
                        name.clone(),
                        server
                            .get("enabled")
                            .and_then(toml::Value::as_bool)
                            .unwrap_or(true),
                    )
                })
                .collect())
        }
        _ => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registrations_follow_each_project_and_provider() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(
            a.path().join(".mcp.json"),
            r#"{"mcpServers":{"project-a":{"url":"http://localhost:1/mcp"}}}"#,
        )
        .unwrap();
        std::fs::create_dir(a.path().join(".codex")).unwrap();
        std::fs::write(
            a.path().join(".codex/config.toml"),
            "[mcp_servers.codex-only]\nenabled = false\nurl = 'http://localhost:2/mcp'\n",
        )
        .unwrap();
        assert_eq!(
            registrations(Kind::Claude, a.path()).unwrap(),
            vec![("project-a".into(), true)]
        );
        assert_eq!(
            registrations(Kind::Codex, a.path()).unwrap(),
            vec![("codex-only".into(), false)]
        );
        assert!(registrations(Kind::Claude, b.path()).unwrap().is_empty());
        assert!(registrations(Kind::Codex, b.path()).unwrap().is_empty());
        std::fs::write(a.path().join(".codex/config.toml"), "[bad").unwrap();
        assert!(registrations(Kind::Codex, a.path()).is_err());
    }

    #[test]
    fn mono_project_is_explicit_and_never_merged() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let mut mono = Mono {
            projects: vec![a.path().display().to_string()],
            ..Default::default()
        };
        assert_eq!(
            directory(&mono, b.path()).unwrap(),
            a.path().canonicalize().unwrap()
        );
        // Several projects: the one the chat's directory lies in, else the
        // first that exists. Never an error that stops the chat.
        mono.projects.push(b.path().display().to_string());
        std::fs::create_dir_all(b.path().join("src")).unwrap();
        assert_eq!(
            directory(&mono, &b.path().join("src")).unwrap(),
            b.path().canonicalize().unwrap()
        );
        assert_eq!(
            directory(&mono, Path::new("/nowhere")).unwrap(),
            a.path().canonicalize().unwrap()
        );
        mono.projects = vec!["relative-project".into(), b.path().display().to_string()];
        assert_eq!(
            directory(&mono, Path::new("/nowhere")).unwrap(),
            b.path().canonicalize().unwrap()
        );
        mono.projects = vec!["relative-project".into()];
        assert!(directory(&mono, b.path()).is_err());
        mono.projects = vec!["relative-project".into(), "/also/missing".into()];
        assert!(directory(&mono, b.path()).unwrap_err().contains("None of"));
    }
    #[test]
    fn a_malformed_project_mcp_file_registers_nothing_instead_of_stopping_the_mono() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".mcp.json"), "{").unwrap();
        assert!(claude_servers(root.path()).unwrap().is_empty());
        assert!(revision(Kind::Claude, root.path()).is_ok());
        std::fs::write(root.path().join(".mcp.json"), "{}").unwrap();
        assert!(claude_servers(root.path()).unwrap().is_empty());
    }
    #[test]
    fn configuration_revision_is_provider_specific_and_rejects_collisions() {
        let root = tempfile::tempdir().unwrap();
        let codex = revision(Kind::Codex, root.path()).unwrap();
        let before = revision(Kind::Claude, root.path()).unwrap();
        std::fs::write(
            root.path().join(".mcp.json"),
            r#"{"mcpServers":{"probe":{"url":"http://127.0.0.1:1/mcp"}}}"#,
        )
        .unwrap();
        assert_ne!(before, revision(Kind::Claude, root.path()).unwrap());
        assert_eq!(codex, revision(Kind::Codex, root.path()).unwrap());
        std::fs::write(
            root.path().join(".mcp.json"),
            r#"{"mcpServers":{"monocode_visual":{}}}"#,
        )
        .unwrap();
        assert!(revision(Kind::Claude, root.path())
            .unwrap_err()
            .contains("conflicts"));
    }
}
