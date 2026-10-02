//! App-server adapters for providers that do not speak the Codex app-server
//! protocol natively: Claude Code, OpenCode, Pi, and Factory Droid. Each one
//! runs as `ruddr app-server --provider NAME`, a child process that speaks
//! line-delimited JSON-RPC on stdio exactly like `codex app-server`.
//! Port of adapter/, claude/, opencode/, pi/, droid/.

/// Entry point for the hidden `ruddr app-server --provider NAME` command.
pub fn app_server_command(args: Vec<String>) -> ruddr_core::Result<()> {
    let _ = args;
    Err(ruddr_core::Error::failed("ruddr app-server is not ported yet"))
}
