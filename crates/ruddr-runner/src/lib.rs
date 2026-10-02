//! The run controller: `ruddr run`, the app-server child, turns, steering,
//! idle sessions, the watchdog, logs, the control server, detaching, and
//! process-tree termination. Port of runner.go, control.go, detach.go,
//! output.go, and process_*.go.

/// Entry point for `ruddr run ARGS...`.
pub fn run_command(args: Vec<String>) -> ruddr_core::Result<()> {
    let _ = args;
    Err(ruddr_core::Error::failed("ruddr run is not ported yet"))
}
