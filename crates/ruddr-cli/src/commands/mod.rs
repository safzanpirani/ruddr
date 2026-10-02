//! Every command except run, app-server, tui, and web. Port of main.go,
//! group.go, result.go, registry.go, thread_commands.go, models.go,
//! provider.go, skill.go, update.go, and remote.go.

use ruddr_core::{Error, Result};

pub fn dispatch(command: &str, args: Vec<String>) -> Result<()> {
    let _ = args;
    match command {
        "status" | "peek" | "wait" | "result" | "steer" | "prompt" | "stop" | "interrupt" | "thread" | "models" | "skill"
        | "update" => Err(Error::failed(format!("ruddr {command} is not ported yet"))),
        other => Err(Error::usage(format!("unknown command {other:?}; run ruddr --help"))),
    }
}

pub fn remote(args: Vec<String>) -> Result<()> {
    let _ = args;
    Err(Error::failed("ruddr --remote is not ported yet"))
}

pub fn version(_args: Vec<String>) -> Result<()> {
    println!("ruddr {}", ruddr_core::VERSION);
    Ok(())
}

pub fn print_usage() {
    eprintln!("Ruddr {} - live steering for coding agents (Rust rewrite in progress)", ruddr_core::VERSION);
}
