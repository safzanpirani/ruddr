//! A stand-in for the `ruddr` binary that only knows `run`, so tests can
//! exercise signals, `--detach`, and `--prompt-file -` in real processes
//! without building the whole CLI. `--detach` re-runs this executable as
//! `ruddr_run run ARGS...`, exactly as the real binary re-runs itself.
//!
//! This is a test fixture. Examples build with `cargo test` and never ship.

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("run") {
        args.remove(0);
    }
    if let Err(error) = ruddr_runner::run_command(args) {
        if !error.message.is_empty() {
            eprintln!("ruddr: {}", error.message);
        }
        std::process::exit(error.code());
    }
}
