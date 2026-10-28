//! The runtime must use assigned project context, including old HOME sessions.
use super::*;
use std::{os::unix::fs::PermissionsExt, path::PathBuf};
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn mono_project_first_turn_resume_and_changes_use_native_context() {
    let root = Fixture(std::env::temp_dir().join(format!(
        "mono-project-runtime-{}",
        monocode_core::mono::new_id()
    )));
    std::fs::create_dir_all(&root.0).unwrap();
    let first = root.0.join("first");
    let second = root.0.join("second");
    for path in [&first, &second] {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(
            path.join(".mcp.json"),
            r#"{"mcpServers":{"test":{"command":"unused"}}}"#,
        )
        .unwrap();
    }
    let script = root.0.join("claude-fixture");
    std::fs::write(&script, r#"#!/usr/bin/env python3
import json,sys,os
with open('launch.json','w') as f: json.dump({'cwd':os.getcwd(),'args':sys.argv[1:]},f)
for line in sys.stdin:
    v=json.loads(line)
    if v.get('type')=='control_request':
        print(json.dumps({'type':'control_response','response':{'subtype':'success','request_id':v['request_id'],'response':{}}}),flush=True)
    if v.get('type')=='user':
        print(json.dumps({'type':'system','subtype':'init','session_id':'fixture-thread','tools':['mcp__test__ping'],'mcp_servers':[{'name':'test','status':'connected'}]}),flush=True)
        print(json.dumps({'type':'assistant','message':{'content':[{'type':'text','text':'OK'}]}}),flush=True)
        print(json.dumps({'type':'result','subtype':'success','result':'OK'}),flush=True)
        break
"#).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let db = Store::open_monocode(&root.0.join("sessions.db")).unwrap();
    let session = db.create("claude", root.0.to_str().unwrap()).unwrap().id;
    let store = Arc::new(Mutex::new(db));
    monocode_core::accounts::eligibility::mark_unhealthy(
        &locked(&store).unwrap(),
        "claude",
        "default",
        1,
    )
    .unwrap();
    monocode_core::accounts::eligibility::mark_unhealthy(
        &locked(&store).unwrap(),
        "claude",
        "account-other",
        1,
    )
    .unwrap();
    for (index, project) in [&first, &first, &second, &second].into_iter().enumerate() {
        if index == 3 {
            std::fs::write(second.join(".mcp.json"), r#"{"mcpServers":{}}"#).unwrap();
        }
        let summary = locked(&store)
            .unwrap()
            .sessions()
            .unwrap()
            .into_iter()
            .find(|s| s.id == session)
            .unwrap();
        let mono = monocode_core::mono::Mono {
            projects: vec![project.display().to_string()],
            ..Default::default()
        };
        let settings = Settings {
            mono: Some(monocode_core::agent::MonoContext {
                mono,
                root: root.0.join("monos"),
                executable: script.clone(),
            }),
            ..Default::default()
        };
        let (tx, rx) = async_channel::unbounded();
        let (_control, controls) = std::sync::mpsc::channel();
        run_turn_using(
            TurnInput {
                summary,
                prompt: "Check MCP".into(),
                settings,
            },
            controls,
            store.clone(),
            Arc::new(AtomicBool::new(false)),
            tx,
            |_| Some(script.clone()),
        );
        let updates = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(updates.last().unwrap().status, "Ready");
        let health =
            monocode_core::accounts::eligibility::health(&locked(&store).unwrap()).unwrap();
        assert!(!health.get("claude", "default").unhealthy);
        assert!(health.get("claude", "account-other").unhealthy);
        let launch: serde_json::Value =
            serde_json::from_slice(&std::fs::read(project.join("launch.json")).unwrap()).unwrap();
        assert_eq!(
            launch["cwd"].as_str().unwrap(),
            project.canonicalize().unwrap().to_str().unwrap()
        );
        let args = launch["args"].as_array().unwrap();
        assert_eq!(
            args.iter().any(|v| v == "--resume"),
            index == 1,
            "only an unchanged project context resumes"
        );
        assert!(
            args.iter()
                .any(|v| v == "--setting-sources=user,project,local")
        );
        assert!(
            args.iter()
                .any(|v| v.as_str().is_some_and(|s| s.contains("monocode_visual")))
        );
    }
}
