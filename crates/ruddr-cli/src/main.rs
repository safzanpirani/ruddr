//! `ruddr`: one binary for the CLI, the run controller, the provider
//! adapters, the TUI, and the web dashboard. This file only routes the first
//! argument to the crate or module that owns the command; each owner parses
//! its own flags and returns a `ruddr_core::Result`, whose error carries the
//! exit code (0 success, 1 failed, 2 usage, 3 still running, 4 stale).

mod commands;

use ruddr_core::{Error, Result};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(error) = dispatch(args) {
        if !error.message.is_empty() {
            eprintln!("ruddr: {}", error.message);
        }
        std::process::exit(error.code());
    }
}

fn dispatch(mut args: Vec<String>) -> Result<()> {
    if args.is_empty() {
        commands::print_usage();
        return Err(Error::usage("a command is required"));
    }
    let command = args.remove(0);
    match command.as_str() {
        "run" => ruddr_runner::run_command(args),
        // Hidden: the provider adapters run as app-server children of `run`.
        "app-server" => ruddr_adapters::app_server_command(args),
        "tui" => ruddr_tui::tui_command(args),
        "web" => ruddr_web::web_command(args),
        "--remote" => commands::remote(args),
        "-h" | "--help" | "help" => {
            commands::print_usage();
            Ok(())
        }
        "version" | "--version" | "-V" => commands::version(args),
        other => commands::dispatch(other, args),
    }
}
