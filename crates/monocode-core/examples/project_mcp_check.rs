//! Opt-in native project MCP connection check, without a model turn.
use std::{
    path::Path,
    sync::{atomic::AtomicBool, Arc},
};
fn main() -> Result<(), String> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("Expected PROVIDER PROJECT".into());
    }
    let servers = monocode_core::mcp::project_connections(
        monocode_core::provider::Kind::from_id(&args[1]),
        Path::new(&args[2]),
        &Default::default(),
        Arc::new(AtomicBool::new(false)),
    )?;
    println!("{}", serde_json::to_string(&servers).unwrap());
    Ok(())
}
