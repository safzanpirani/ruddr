//! The server side of the private control channel (`ruddr_core::control`):
//! a 0600 Unix socket in a 0700 directory, or a named pipe on Windows. One
//! request per connection, one JSON line each way. The commands steer the
//! active turn, prompt an idle session, interrupt a turn, stop an idle
//! session, or report status. No command converts into another.

use crate::controller::{Controller, DEFAULT_INTERRUPT_TIMEOUT, PromptRequest, read_line_limited, str_at};
use crate::text::one_line;
use ruddr_core::control::Response;
use ruddr_core::state::Status;
use ruddr_core::{Error, Result};
use serde_json::{Value, json};
use std::io::{self, BufReader, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long one control connection may take (Unix sockets only).
#[cfg(unix)]
const CONNECTION_DEADLINE: Duration = Duration::from_secs(120);
/// The largest request line accepted; prompts travel in it.
const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
/// How often the nonblocking listener checks for clients and for closing.
const ACCEPT_POLL: Duration = Duration::from_millis(25);

pub trait Connection: Read + Write + Send {}
impl<T: Read + Write + Send> Connection for T {}

/// A source of control connections. `accept` returns `WouldBlock` when no
/// client is waiting.
pub trait Acceptor: Send {
    fn accept(&mut self) -> io::Result<Box<dyn Connection>>;
}

pub struct ServerHandle {
    closing: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Remove this socket file when the server closes.
    owned_socket: Option<String>,
}

/// Starts listening at the state's `socket_path`.
pub fn start(controller: &Arc<Controller>) -> Result<()> {
    let state = controller.store.snapshot();
    let (acceptor, owned_socket) = listen(&state.socket_path)?;
    let closing = Arc::new(AtomicBool::new(false));
    let thread = spawn_accept_loop(controller, acceptor, closing.clone())?;
    *controller.server.lock().unwrap_or_else(|e| e.into_inner()) = Some(ServerHandle {
        closing,
        thread: Some(thread),
        owned_socket,
    });
    Ok(())
}

pub fn spawn_accept_loop(controller: &Arc<Controller>, acceptor: Box<dyn Acceptor>, closing: Arc<AtomicBool>) -> Result<JoinHandle<()>> {
    let controller = controller.clone();
    Ok(std::thread::Builder::new()
        .name("ruddr-control".into())
        .spawn(move || accept_loop(&controller, acceptor, &closing))?)
}

/// Stops accepting, removes the socket this run created, and removes the
/// private socket directory when it is empty. A path the run never bound is
/// left alone.
pub fn close(controller: &Controller) {
    let handle = controller.server.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(mut handle) = handle {
        handle.closing.store(true, Ordering::SeqCst);
        if let Some(thread) = handle.thread.take() {
            let _ = thread.join();
        }
        if let Some(path) = handle.owned_socket {
            remove_socket_file(&path);
        }
    }
    if let Some(dir) = controller.store.snapshot().socket_dir {
        let _ = std::fs::remove_dir(dir);
    }
}

#[cfg(unix)]
fn remove_socket_file(path: &str) {
    use std::os::unix::fs::FileTypeExt;
    if std::fs::symlink_metadata(path).map(|m| m.file_type().is_socket()).unwrap_or(false) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(not(unix))]
fn remove_socket_file(_path: &str) {}

#[cfg(unix)]
fn listen(path: &str) -> Result<(Box<dyn Acceptor>, Option<String>)> {
    use std::os::unix::net::UnixListener;
    let socket = std::path::Path::new(path);
    let parent = socket.parent().unwrap_or(std::path::Path::new("."));
    let metadata = std::fs::metadata(parent).map_err(|e| Error::failed(format!("inspect control socket parent: {e}")))?;
    check_socket_parent_mode(parent, metadata_mode(&metadata))?;
    remove_stale_socket(socket)?;
    let listener = UnixListener::bind(socket).map_err(|e| Error::failed(format!("listen on {path}: {e}")))?;
    if let Err(e) = ruddr_core::fsutil::set_mode(socket, 0o600).and_then(|_| listener.set_nonblocking(true)) {
        drop(listener);
        let _ = std::fs::remove_file(socket);
        return Err(e.into());
    }
    struct Unix(UnixListener);
    impl Acceptor for Unix {
        fn accept(&mut self) -> io::Result<Box<dyn Connection>> {
            let (stream, _) = self.0.accept()?;
            // Accepted sockets inherit O_NONBLOCK on BSD and macOS.
            stream.set_nonblocking(false)?;
            stream.set_read_timeout(Some(CONNECTION_DEADLINE))?;
            stream.set_write_timeout(Some(CONNECTION_DEADLINE))?;
            Ok(Box::new(stream))
        }
    }
    Ok((Box::new(Unix(listener)), Some(path.to_string())))
}

#[cfg(unix)]
fn metadata_mode(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode()
}

#[cfg(windows)]
fn listen(path: &str) -> Result<(Box<dyn Acceptor>, Option<String>)> {
    use interprocess::local_socket::traits::Listener as _;
    use interprocess::local_socket::{ListenerNonblockingMode, ListenerOptions};
    let name = ruddr_core::control::socket_name(path).map_err(|e| Error::failed(format!("control pipe name {path}: {e}")))?;
    // TODO(review): Define and verify an owner or logon SID ACL for the pipe instead of the default Windows descriptor.
    let listener = ListenerOptions::new()
        .name(name)
        .nonblocking(ListenerNonblockingMode::Accept)
        .create_sync()
        .map_err(|e| Error::failed(format!("listen on {path}: {e}")))?;
    struct Pipe(interprocess::local_socket::Listener);
    impl Acceptor for Pipe {
        fn accept(&mut self) -> io::Result<Box<dyn Connection>> {
            // TODO(review): Bound Windows pipe reads and writes and cancel stalled clients before releasing their handlers.
            Ok(Box::new(self.0.accept()?))
        }
    }
    Ok((Box::new(Pipe(listener)), None))
}

#[cfg(not(any(unix, windows)))]
fn listen(path: &str) -> Result<(Box<dyn Acceptor>, Option<String>)> {
    Err(Error::failed(format!("control channel {path} is not supported on this platform")))
}

/// Rejects a socket parent that other users can reach.
pub fn check_socket_parent_mode(dir: &std::path::Path, mode: u32) -> Result<()> {
    if mode & 0o077 != 0 {
        return Err(Error::failed(format!(
            "control socket parent {} must be owner-only, mode is {:o}",
            dir.display(),
            mode & 0o777
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn remove_stale_socket(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
        Ok(m) if !m.file_type().is_socket() => Err(Error::failed(format!(
            "refusing to remove non-socket control path {}",
            path.display()
        ))),
        Ok(_) => Ok(std::fs::remove_file(path)?),
    }
}

fn is_temporary(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset | io::ErrorKind::TimedOut
    ) {
        return true;
    }
    #[cfg(unix)]
    if let Some(code) = error.raw_os_error() {
        return [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM, libc::EPROTO].contains(&code);
    }
    false
}

/// Sleeps up to `duration`, waking early when the server closes.
fn pause(duration: Duration, closing: &AtomicBool) {
    let deadline = Instant::now() + duration;
    while !closing.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        std::thread::sleep((deadline - now).min(ACCEPT_POLL));
    }
}

fn accept_loop(controller: &Arc<Controller>, mut acceptor: Box<dyn Acceptor>, closing: &AtomicBool) {
    let mut retry = Duration::from_millis(5);
    while !closing.load(Ordering::SeqCst) {
        match acceptor.accept() {
            Ok(connection) => {
                retry = Duration::from_millis(5);
                let controller = controller.clone();
                let _ = std::thread::Builder::new()
                    .name("ruddr-control-conn".into())
                    .spawn(move || handle_connection(&controller, connection));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => pause(ACCEPT_POLL, closing),
            Err(e) if is_temporary(&e) => {
                if controller.lock().session_closed {
                    return;
                }
                controller.trace(format!("[warn] temporary control accept error: {e}"));
                pause(retry, closing);
                if retry < Duration::from_secs(1) {
                    retry *= 2;
                }
            }
            Err(e) => {
                controller.trace(format!("[warn] control socket closed after accept error: {e}"));
                return;
            }
        }
    }
}

/// Reads one request line, runs it, and writes one response line.
pub fn handle_connection(controller: &Controller, mut connection: Box<dyn Connection>) {
    let read = {
        let mut reader = BufReader::new(&mut connection);
        read_line_limited(&mut reader, MAX_REQUEST_BYTES)
    };
    let line = match read {
        Ok(Some(line)) => line,
        Ok(None) => return,
        Err(e) => {
            let _ = write_response(&mut connection, controller, Err(e.to_string()));
            return;
        }
    };
    let result = match serde_json::from_slice::<Value>(&line) {
        Ok(request) => dispatch(controller, &request),
        Err(e) => Err(e.to_string()),
    };
    let _ = write_response(&mut connection, controller, result);
}

fn write_response(
    connection: &mut Box<dyn Connection>,
    controller: &Controller,
    result: std::result::Result<(), String>,
) -> io::Result<()> {
    let response = Response {
        ok: result.is_ok(),
        error: result.err(),
        state: Some(controller.store.snapshot()),
    };
    let mut line = serde_json::to_vec(&response).map_err(io::Error::other)?;
    line.push(b'\n');
    connection.write_all(&line)?;
    connection.flush()
}

fn dispatch(controller: &Controller, request: &Value) -> std::result::Result<(), String> {
    let text = request.get("text").and_then(Value::as_str).unwrap_or_default();
    let expected = request.get("expectedTurnId").and_then(Value::as_str).unwrap_or_default();
    match request.get("command").and_then(Value::as_str).unwrap_or_default() {
        "status" => Ok(()),
        "steer" if text.is_empty() => Err("steering text is empty".into()),
        "steer" => steer(controller, text, expected),
        "interrupt" => interrupt(controller, expected),
        "prompt" if text.is_empty() => Err("prompt text is empty".into()),
        "prompt" => prompt(controller, text),
        // Go clients send "shutdown"; ruddr_core::control sends "stop".
        "stop" | "shutdown" => stop(controller),
        other => Err(format!("unknown control command {other:?}")),
    }
}

/// The status a control command sees: `stopping` once stop was accepted.
fn visible_status(controller: &Controller) -> String {
    if controller.lock().stop_requested && !controller.store.snapshot().status.is_terminal() {
        return "stopping".into();
    }
    controller.store.snapshot().status.to_string()
}

/// Adds direction to the active turn. Every turn/steer carries the thread and
/// the expected turn; a rejected steer is reported, never retried as a turn.
pub fn steer(controller: &Controller, text: &str, expected: &str) -> std::result::Result<(), String> {
    let state = controller.store.snapshot();
    let (Some(thread), Some(turn)) = (state.thread_id.clone(), state.turn_id.clone()) else {
        return Err(format!("turn is not steerable: status={}", visible_status(controller)));
    };
    if state.status != Status::Active {
        return Err(format!("turn is not steerable: status={}", visible_status(controller)));
    }
    if !expected.is_empty() && turn != expected {
        return Err(format!("active turn changed from {expected} to {turn}; steer was not sent"));
    }
    let params = json!({"threadId": thread, "expectedTurnId": turn, "input": [{"type": "text", "text": text}]});
    let result = controller
        .call("turn/steer", params, Duration::from_secs(30))
        .map_err(|e| e.to_string())?;
    let acknowledged = str_at(&result, &["turnId"]);
    if acknowledged != turn {
        return Err(format!("provider acknowledged unexpected turn {acknowledged}"));
    }
    if let Err(e) = controller.store.update(|state| state.steers += 1) {
        let message = format!("persist steer count: {e}");
        controller.stop_child.store(true, Ordering::SeqCst);
        controller.fail(&message);
        controller.terminate(false);
        return Err(message);
    }
    controller.trace(format!("[steer] accepted for turn {turn}: {}", one_line(text, 180)));
    Ok(())
}

/// Starts the next turn of an idle session. Valid only while idle; it is
/// never converted into a steer.
pub fn prompt(controller: &Controller, text: &str) -> std::result::Result<(), String> {
    if !controller.cfg.idle {
        return Err("session was not started with --idle; use a new run to continue the thread".into());
    }
    let state = controller.store.snapshot();
    let status = visible_status(controller);
    if status != "idle" {
        if status == "active" {
            return Err("a turn is active; steer it instead".into());
        }
        return Err(format!("session is not idle: status={status}"));
    }
    let id = {
        let deadline = Instant::now() + Duration::from_secs(5);
        let (mut lifecycle, ready) = controller.wait_for(controller.lock(), Some(deadline), |l| {
            l.session_closed || (l.idle_waiting && l.prompt_slot.is_none())
        });
        if lifecycle.session_closed {
            return Err("session ended before the prompt was accepted".into());
        }
        if !ready {
            return Err("session did not accept the prompt; it may no longer be idle".into());
        }
        lifecycle.next_prompt += 1;
        let id = lifecycle.next_prompt;
        lifecycle.prompt_slot = Some(PromptRequest {
            id,
            text: text.to_string(),
            observed_turns: state.turns,
        });
        lifecycle.idle_waiting = false;
        controller.notify();
        id
    };
    let deadline = Instant::now() + Duration::from_secs(45);
    let (mut lifecycle, _) = controller.wait_for(controller.lock(), Some(deadline), |l| {
        l.session_closed || l.prompt_replies.contains_key(&id)
    });
    match lifecycle.prompt_replies.remove(&id) {
        Some(Err(e)) => return Err(e),
        Some(Ok(())) => {}
        None if lifecycle.session_closed => return Err("session ended while waiting for turn/start".into()),
        None => return Err("timed out waiting for turn/start".into()),
    }
    drop(lifecycle);
    controller.trace(format!("[prompt] accepted: {}", one_line(text, 180)));
    Ok(())
}

/// Ends an idle session gracefully.
pub fn stop(controller: &Controller) -> std::result::Result<(), String> {
    if !controller.cfg.idle {
        return Err("session was not started with --idle".into());
    }
    let status = visible_status(controller);
    let mut lifecycle = controller.lock();
    if lifecycle.session_ended || lifecycle.stop_requested || controller.store.snapshot().status != Status::Idle {
        return Err(format!("session is not idle: status={status}"));
    }
    lifecycle.stop_requested = true;
    controller.notify();
    Ok(())
}

/// Interrupts the active turn. A plain run ends with `interrupted`. An idle
/// session waits for the provider to settle the turn and returns to idle; a
/// turn that does not settle in time fails the session.
pub fn interrupt(controller: &Controller, expected: &str) -> std::result::Result<(), String> {
    let (state, generation) = {
        let lifecycle = controller.lock();
        (controller.store.snapshot(), lifecycle.turn_gen)
    };
    let (Some(thread), Some(turn)) = (state.thread_id.clone(), state.turn_id.clone()) else {
        return Err(format!("turn is not active: status={}", state.status));
    };
    if state.status != Status::Active {
        return Err(format!("turn is not active: status={}", state.status));
    }
    if !expected.is_empty() && turn != expected {
        return Err(format!("active turn changed from {expected} to {turn}; interrupt was not sent"));
    }
    let timeout = controller.cfg.interrupt_timeout.unwrap_or(DEFAULT_INTERRUPT_TIMEOUT);
    let deadline = Instant::now() + timeout;
    let rpc = controller
        .call("turn/interrupt", json!({"threadId": thread, "turnId": turn}), timeout)
        .map(|_| ())
        .map_err(|e| e.to_string());
    if controller.cfg.idle && rpc.is_ok() {
        let (lifecycle, _) = controller.wait_for(controller.lock(), Some(deadline), |l| {
            l.turn_settled(generation) || l.session_closed
        });
        let settled = lifecycle.turn_settled(generation);
        let closed = lifecycle.session_closed;
        drop(lifecycle);
        if settled {
            return controller.interrupt_settlement_result();
        }
        if closed {
            return Err("session ended before the interrupted turn settled".into());
        }
        let error = format!(
            "interrupted turn {turn} did not settle within {}",
            ruddr_core::duration::format(timeout)
        );
        if !controller.end_session_if_turn_open(generation, Status::Failed, &error) {
            return controller.interrupt_settlement_result();
        }
        controller.terminate(false);
        return Err(error);
    }
    if controller.cfg.idle {
        // A late failure for an old interrupt cannot tear down the next turn
        // or an idle session that already settled.
        if !controller.end_session_if_turn_open(generation, Status::Interrupted, "") {
            return rpc;
        }
    } else {
        controller.stop_child.store(true, Ordering::SeqCst);
        controller.end_session(Status::Interrupted, "");
    }
    if let Err(e) = &rpc {
        controller.trace(format!("[warn] turn/interrupt failed; forcing local teardown: {e}"));
    }
    controller.terminate(false);
    if controller.store.snapshot().status == Status::Interrupted {
        return Ok(());
    }
    let result = controller.private_result_error();
    if !result.is_empty() {
        return Err(result);
    }
    rpc
}
