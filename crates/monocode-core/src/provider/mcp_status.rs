//! Persist provider-reported MCP startup status without endpoint credentials.
use super::*;
pub(super) const STATUS_ID: &str = "monocode-mcp-inventory";

fn report(request: &Request, servers: Vec<Value>, tools: Vec<String>, note: &str) -> ToolCall {
    let mut tool = ToolCall::new("monocode-mcp-startup", "MCP connection status");
    tool.state = ToolState::Completed;
    let source = match request.kind {
        Kind::Claude => request.cwd.join(".mcp.json"),
        Kind::Codex => request.cwd.join(".codex/config.toml"),
        _ => request.cwd.clone(),
    };
    tool.input = json!({"provider":request.kind.id(),"launchDirectory":request.cwd,
        "projectConfiguration":source,"projectConfigurationPresent":source.is_file(), "accountId":request.settings.account.id, "configurationRevision":request.settings.environment.get(crate::mono::project::REVISION_ENV)})
    .to_string();
    tool.output = serde_json::to_string_pretty(
        &json!({"servers":servers,"availableTools":tools,"note":note}),
    )
    .unwrap_or_default();
    tool.bound_output();
    tool
}
fn state(raw: &str) -> &'static str {
    match raw {
        "connected" | "ready" => "connected",
        "failed" => "connection failed (registration discovered)",
        "needs-auth" | "authenticationRequired" => {
            "authentication required (registration discovered)"
        }
        "disabled" => "disabled",
        "pending" | "starting" | "notStarted" => "connecting",
        "cancelled" => "cancelled",
        _ => "provider did not report connection status",
    }
}
impl Protocol {
    pub(super) fn mono_mcp_status(&self, frame: &Value) -> Option<ToolCall> {
        self.request.settings.mono.as_ref()?;
        if self.request.kind != Kind::Claude
            || frame["type"] != "system"
            || frame["subtype"] != "init"
        {
            return None;
        }
        let configured =
            crate::mono::project::claude_servers(&self.request.cwd).unwrap_or_default();
        let mut servers = frame["mcp_servers"].as_array().into_iter().flatten().filter_map(|s| {
            let name = s["name"].as_str()?;
            Some(json!({"name":name,"status":state(s["status"].as_str().unwrap_or("")),
                "registeredInAssignedProject":configured.iter().any(|n| n==name),
                "providerReportedSource":s["source"].as_str().filter(|s| matches!(*s,"project"|"user"|"local"|"dynamic"))}))
        }).collect::<Vec<_>>();
        for name in &configured {
            if !servers.iter().any(|s| s["name"] == *name) {
                servers.push(json!({"name":name,"status":"configured in project but not reported by provider; check project trust and MCP approval","registeredInAssignedProject":true}));
            }
        }
        let tools = frame["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|t| t.starts_with("mcp__"))
            .map(str::to_owned)
            .collect();
        Some(report(&self.request, servers, tools, "Configuration presence is separate from provider discovery and connection health. Tool names are those exposed in this initialization; deferred tools may appear later. Project files are loaded by Claude, not merged by MonoCode."))
    }

    pub(super) fn codex_mcp_status(&self, frame: &Value) -> Option<ToolCall> {
        self.request.settings.mono.as_ref()?;
        if self.request.kind != Kind::Codex || frame["id"] != STATUS_ID {
            return None;
        }
        if frame.get("error").is_some_and(|error| !error.is_null()) {
            return Some(report(&self.request, Vec::new(), Vec::new(), "Codex did not provide an MCP inventory. Connection health is unknown, not missing registration."));
        }
        let servers = frame["result"]["data"].as_array().into_iter().flatten().filter_map(|s| {
            Some(json!({"name":s["name"].as_str()?,"status":state(s["runtimeStatus"].as_str().unwrap_or(""))}))
        }).collect();
        let tools = frame["result"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|s| {
                s["tools"]
                    .as_object()
                    .into_iter()
                    .flat_map(|t| t.keys())
                    .map(|name| format!("{}:{name}", s["name"].as_str().unwrap_or("server")))
            })
            .collect();
        Some(report(&self.request, servers, tools, "Codex inventory for this thread, using native Codex configuration and project trust. The project path is a configuration candidate, not a claim that a config layer was trusted. Claude .mcp.json is not imported. This report covers the first inventory page (up to 100 servers)."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(kind: Kind, cwd: PathBuf) -> Request {
        Request {
            kind,
            cwd: cwd.clone(),
            binary: "fixture".into(),
            prompt: "check".into(),
            provider_id: None,
            model: None,
            settings: Settings {
                mono: Some(crate::agent::MonoContext {
                    mono: crate::mono::Mono::default(),
                    root: cwd,
                    executable: "fixture".into(),
                }),
                ..Default::default()
            },
        }
    }
    #[test]
    fn registration_connection_and_missing_inventory_are_distinct_without_secrets() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".mcp.json"),r#"{"mcpServers":{"up":{},"down":{"headers":{"Authorization":"SECRET"}},"missing":{}}}"#).unwrap();
        let p = Protocol::new(request(Kind::Claude, root.path().into()));
        let t=p.mono_mcp_status(&json!({"type":"system","subtype":"init","mcp_servers":[{"name":"up","status":"connected","source":"project"},{"name":"down","status":"failed","error":"SECRET"},{"name":"monocode_visual","status":"connected"}],"tools":["Read","mcp__up__ping"]})).unwrap();
        assert!(t
            .output
            .contains("connection failed (registration discovered)"));
        assert!(t.output.contains("configured in project but not reported"));
        assert!(t.output.contains("mcp__up__ping"));
        assert!(t.output.contains("monocode_visual"));
        assert!(!t.output.contains("SECRET"));
        assert!(t.input.contains(".mcp.json"));
    }
    #[test]
    fn optional_codex_inventory_failure_does_not_abort_the_turn() {
        let root = tempfile::tempdir().unwrap();
        let mut p = Protocol::new(request(Kind::Codex, root.path().into()));
        let events = p
            .frame(
                json!({"id":STATUS_ID,"error":{"message":"unsupported inventory"}}),
                &mut Vec::new(),
            )
            .unwrap();
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::Tool(t) if t.output.contains("unknown, not missing"))));
    }
}
