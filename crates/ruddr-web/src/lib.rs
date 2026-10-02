//! `ruddr web`: the browser dashboard server. Serves the bundled client from
//! web/client (embedded at build time), the token-gated JSON API, and the
//! server-sent event streams. Port of web/server.ts and web_command.go.

/// Entry point for `ruddr web ARGS...`.
pub fn web_command(args: Vec<String>) -> ruddr_core::Result<()> {
    let _ = args;
    Err(ruddr_core::Error::failed("ruddr web is not ported yet"))
}
