//! The private control channel between commands and a live controller. One
//! request per connection: the client writes one JSON line and reads one JSON
//! line back. The runner crate owns the server side.
//!
//! Transport: a Unix socket at `state.socket_path` (inside the 0700 state
//! directory, or a private temporary parent when that path is too long), and
//! a named pipe on Windows, where `socket_path` holds the pipe name
//! (`\\.\pipe\ruddr-<hex>`).

use crate::error::{Error, Result};
use crate::state::RunState;
use interprocess::local_socket::{prelude::*, GenericFilePath, GenericNamespaced, Name, Stream};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::Duration;

/// Unix socket paths must fit `sockaddr_un.sun_path` on every platform.
pub const MAX_SOCKET_PATH_BYTES: usize = 100;
pub const SOCKET_FILE: &str = ".ruddr.sock";
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Command {
    /// Add direction to the active turn. Requires `expected_turn_id`.
    Steer,
    /// Start the next turn of an idle session.
    Prompt,
    /// Interrupt the active turn (optionally only `expected_turn_id`).
    Interrupt,
    /// End an idle session gracefully.
    Stop,
    /// Report the live state without changing anything.
    Status,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub command: Command,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_turn_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub state: Option<RunState>,
}

/// Turns a recorded `socket_path` into a local-socket name.
pub fn socket_name(socket_path: &str) -> std::io::Result<Name<'_>> {
    if let Some(pipe) = socket_path.strip_prefix(r"\\.\pipe\") {
        pipe.to_ns_name::<GenericNamespaced>()
    } else {
        socket_path.to_fs_name::<GenericFilePath>()
    }
}

/// Fails promptly when the controller is gone, so commands never wait on a
/// dead run. Returns the exit code `Stale`.
pub fn ensure_controller_live(state: &RunState) -> Result<()> {
    if !crate::process::alive(state.pid) {
        return Err(Error::stale(format!(
            "Ruddr pid {} is not running; state is stale at status={}",
            state.pid, state.status
        )));
    }
    if state.socket_path.is_empty() {
        return Err(Error::failed(format!("Ruddr pid {} recorded no control socket", state.pid)));
    }
    Ok(())
}

/// Sends one request to the controller of `state_dir` and returns its reply.
/// The whole exchange is bounded by `timeout`.
pub fn send(state_dir: &Path, request: &Request, timeout: Duration) -> Result<Response> {
    let state = crate::state::read_state(state_dir)?;
    ensure_controller_live(&state)?;
    let socket_path = state.socket_path.clone();
    let pid = state.pid;
    let line = {
        let mut line = serde_json::to_vec(request)?;
        line.push(b'\n');
        line
    };
    // interprocess has no portable connect/read timeout; run the exchange on
    // a thread and stop waiting at the deadline.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = (|| -> std::io::Result<String> {
            let name = socket_name(&socket_path)?;
            let mut stream = Stream::connect(name)?;
            stream.write_all(&line)?;
            stream.flush()?;
            let mut reply = String::new();
            BufReader::new(stream).read_line(&mut reply)?;
            Ok(reply)
        })();
        let _ = tx.send(result);
    });
    let reply = match rx.recv_timeout(timeout) {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => return Err(Error::failed(format!("connect to Ruddr pid {pid} at {}: {e}", state.socket_path))),
        Err(_) => return Err(Error::failed(format!("Ruddr pid {pid} did not answer within {}", crate::duration::format(timeout)))),
    };
    if reply.trim().is_empty() {
        return Err(Error::failed(format!("Ruddr pid {pid} closed the control connection without a reply")));
    }
    Ok(serde_json::from_str(reply.trim())?)
}

/// Like [`send`], but an `ok: false` reply becomes an error.
pub fn call(state_dir: &Path, request: &Request, timeout: Duration) -> Result<RunState> {
    let response = send(state_dir, request, timeout)?;
    if !response.ok {
        return Err(Error::failed(response.error.unwrap_or_else(|| "the controller rejected the request".into())));
    }
    response.state.ok_or_else(|| Error::failed("the controller replied without state"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_serialize_like_go() {
        let request = Request { command: Command::Steer, text: Some("go left".into()), expected_turn_id: Some("t1".into()) };
        assert_eq!(serde_json::to_string(&request).unwrap(), r#"{"command":"steer","text":"go left","expectedTurnId":"t1"}"#);
        let request = Request { command: Command::Stop, text: None, expected_turn_id: None };
        assert_eq!(serde_json::to_string(&request).unwrap(), r#"{"command":"stop"}"#);
    }
}
