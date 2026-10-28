//! Opt-in local MCP smoke test. Never run as part of cargo test.
//! Usage: mono_mcp_probe PROJECT_A PROJECT_B OUTPUT_JSON
//! Projects must contain only disposable local MCP registrations.
use monocode_core::{
    agent::{AccessMode, MonoContext, Settings},
    mono::Mono,
    provider::{self, Event, Kind, Request},
};
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
fn main() -> Result<(), String> {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).is_some_and(|a| a == "visual-mcp") {
        return monocode_core::visual_bridge::mcp_stdio();
    }
    if args.len() != 4 {
        return Err("Expected PROJECT_A PROJECT_B OUTPUT_JSON".into());
    }
    let binary = provider::resolve(Kind::Claude).ok_or("Claude CLI unavailable")?;
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let bridge = monocode_core::visual_bridge::Server::start(444)?;
    let mut provider_id = None;
    let mut results = Vec::new();
    for (index, project) in [&args[1], &args[1], &args[2]].into_iter().enumerate() {
        if index == 2 {
            provider_id = None;
        }
        let mono = Mono {
            projects: vec![project.clone()],
            ..Default::default()
        };
        let settings = Settings {
            access: AccessMode::FullAccess,
            environment: bridge.environment(&executable),
            mono: Some(MonoContext {
                mono,
                root: PathBuf::from(project).join("monos"),
                executable: executable.clone(),
            }),
            ..Default::default()
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let timer = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(90));
            timer.store(true, Ordering::Relaxed);
        });
        let mut ping = false;
        let mut visual = false;
        let mut statuses = Vec::new();
        let result=provider::run(Request {
            kind:Kind::Claude,binary:binary.clone(),cwd:std::env::temp_dir(),
            provider_id:provider_id.clone(),model:None,settings,
            prompt:"This is a local MCP smoke test. Call mcp__mono_probe__ping with an empty object. Then call mcp__monocode_visual__visual_check with source `return () => visual.text('MCP smoke test');`. Use ToolSearch if needed to discover those two exact tools. Do not use Bash, edit files, or call other tools. Report the ping marker and finish.".into(),
        },cancel,|events| {
            for event in events {
                match event {
                    Event::SessionId(id)=>provider_id=Some(id),
                    Event::Tool(t) if t.state==monocode_core::tool::ToolState::Completed=>{
                        if t.name=="mcp__mono_probe__ping" { ping=t.output.contains("mono-local-probe"); }
                        if t.name=="mcp__monocode_visual__visual_check" { visual = !t.output.contains("isError\":true"); }
                        if t.name=="MCP connection status" { statuses.push(serde_json::from_str::<serde_json::Value>(&t.output).unwrap_or_default()); }
                    }
                    _=>{}
                }
            }
            Ok(())
        });
        results.push(json!({"turn":index+1,"project":project,"success":result.is_ok(),"pingCalled":ping,"injectedToolCalled":visual,"mcp":statuses}));
        std::fs::write(&args[3], serde_json::to_vec_pretty(&results).unwrap())
            .map_err(|e| e.to_string())?;
        println!(
            "turn {}: completed={}, project tool={}, injected tool={}",
            index + 1,
            result.is_ok(),
            ping,
            visual
        );
        if result.is_err() || !ping || !visual {
            return Err("Local MCP smoke test did not pass; see the sanitized report.".into());
        }
    }
    Ok(())
}
