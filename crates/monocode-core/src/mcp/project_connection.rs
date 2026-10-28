//! Provider-native connection initialization without a model turn or tool call.
use crate::{accounts::Profile, provider::Kind};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

pub fn project_connections(
    kind: Kind,
    cwd: &Path,
    profile: &Profile,
    cancel: Arc<AtomicBool>,
) -> Result<Vec<(String, String)>, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("Project connection check cancelled".into());
    }
    let configured = crate::mono::project::registrations(kind, cwd)?;
    if configured.iter().all(|(_, enabled)| !enabled) {
        return Ok(configured
            .into_iter()
            .map(|(n, _)| (n, "disabled".into()))
            .collect());
    }
    let binary = crate::provider::resolve(kind).ok_or("Provider CLI is unavailable")?;
    crate::provider::test_guard::check(&binary)?;
    let mut command = Command::new(binary);
    crate::scm::shell::apply_gui_env(&mut command);
    profile.apply(kind, &mut command)?;
    command
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    match kind {
        Kind::Claude => {
            command.args(["mcp", "list"]).stdin(Stdio::null());
        }
        Kind::Codex => {
            command.arg("app-server").stdin(Stdio::piped());
        }
        _ => return Err("Automatic MCP connections are not supported for this provider".into()),
    }
    crate::process::isolate(&mut command);
    let mut child = command
        .spawn()
        .map_err(|_| "Could not start provider MCP initialization")?;
    let _registration = crate::process::register(child.id());
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::sync_channel(8);
    let reader = std::thread::spawn(move || {
        let mut input = BufReader::new(stdout);
        loop {
            let mut bytes = Vec::new();
            let result = input
                .by_ref()
                .take(8 * 1024 * 1024)
                .read_until(b'\n', &mut bytes);
            match result {
                Ok(0) | Err(_) => break,
                Ok(_) if bytes.len() >= 8 * 1024 * 1024 => break,
                Ok(_) => {
                    if tx
                        .send(String::from_utf8_lossy(&bytes).into_owned())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(40);
    let result = (|| {
        let mut stdin = child.stdin.take();
        let mut send = |value: Value| -> Result<(), String> {
            let input = stdin.as_mut().ok_or("Provider input unavailable")?;
            writeln!(input, "{value}").map_err(|_| "Provider input closed".to_string())
        };
        if kind == Kind::Codex {
            send(
                json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"monocode_mcp_check","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}}),
            )?;
        }
        let mut statuses = std::collections::HashMap::new();
        let mut thread = None;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("Project connection check cancelled".into());
            }
            if Instant::now() >= deadline {
                return Err("Project MCP initialization timed out".into());
            }
            let line = match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => line,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) if kind == Kind::Claude => break,
                Err(_) => return Err("Provider closed before reporting MCP connections".into()),
            };
            if kind == Kind::Claude {
                for (name, _) in &configured {
                    if line.trim_start().starts_with(&format!("{name}:")) {
                        let status = if line.contains("Pending approval") {
                            "project approval required"
                        } else if line.contains("Connected") {
                            "connected"
                        } else if line.contains("Needs authentication")
                            || line.contains("Needs auth")
                        {
                            "authentication required"
                        } else if line.contains("Failed") {
                            "connection failed"
                        } else {
                            "connection not verified"
                        };
                        statuses.insert(name.clone(), status.to_owned());
                    }
                }
                continue;
            }
            let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if frame.get("error").is_some() {
                return Err("Provider rejected project MCP initialization; check configuration and project trust".into());
            }
            match frame["id"].as_u64() {
                Some(1) => {
                    send(json!({"method":"initialized"}))?;
                    send(
                        json!({"id":4,"method":"config/read","params":{"cwd":cwd,"includeLayers":true}}),
                    )?;
                }
                Some(4) => {
                    let expected = cwd.join(".codex");
                    let expected = expected.canonicalize().unwrap_or(expected);
                    let loaded = frame["result"]["layers"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|layer| {
                            let folder = std::path::PathBuf::from(
                                layer["name"]["dotCodexFolder"].as_str().unwrap_or(""),
                            );
                            let folder = folder.canonicalize().unwrap_or(folder);
                            layer["name"]["type"] == "project"
                                && folder == expected
                                && layer["disabledReason"].is_null()
                        });
                    if !loaded {
                        return Err("Project MCP configuration is not enabled by Codex. Review project trust in Codex before connecting.".into());
                    }
                    send(
                        json!({"id":2,"method":"thread/start","params":{"cwd":cwd,"ephemeral":true,"approvalPolicy":"never","sandbox":"read-only"}}),
                    )?;
                }
                Some(2) => {
                    thread = frame["result"]["thread"]["id"].as_str().map(str::to_owned);
                    if thread.is_none() {
                        return Err("Provider returned no project context".into());
                    }
                    send(
                        json!({"id":3,"method":"mcpServerStatus/list","params":{"threadId":thread,"limit":100}}),
                    )?;
                }
                Some(3) => {
                    for server in frame["result"]["data"].as_array().into_iter().flatten() {
                        if let Some(name) = server["name"].as_str() {
                            let state = match server["runtimeStatus"].as_str().unwrap_or("") {
                                "connected" | "ready" => "connected",
                                "failed" => "connection failed",
                                "authenticationRequired" | "needs-auth" => {
                                    "authentication required"
                                }
                                "disabled" => "disabled",
                                "starting" | "pending" | "notStarted" => "connecting",
                                _ => "connection not verified",
                            };
                            statuses.insert(name.to_owned(), state.to_owned());
                        }
                    }
                    if configured.iter().any(|(n, enabled)| {
                        *enabled && statuses.get(n).is_some_and(|s| s == "connecting")
                    }) {
                        std::thread::sleep(Duration::from_millis(200));
                        send(
                            json!({"id":3,"method":"mcpServerStatus/list","params":{"threadId":thread,"limit":100}}),
                        )?;
                    } else {
                        break;
                    }
                }
                _ => {}
            }
        }
        Ok(configured
            .into_iter()
            .map(|(name, enabled)| {
                let status = if enabled {
                    statuses
                        .remove(&name)
                        .unwrap_or_else(|| "configured · not reported by provider".into())
                } else {
                    "disabled".into()
                };
                (name, status)
            })
            .collect())
    })();
    crate::process::terminate(&mut child);
    drop(rx);
    let _ = reader.join();
    result
}
