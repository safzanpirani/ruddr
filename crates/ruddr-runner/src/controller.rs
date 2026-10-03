//! The controller's shared state and its link to the app-server child:
//! bounded JSON-RPC calls, the stdout reader, event handling, the private
//! logs, and the turn and session lifecycle.
//!
//! Every lifecycle flag lives in one mutex with one condition variable.
//! Waiters (turn waits, the idle loop, prompt hand-offs, the stdin write gate)
//! re-check their condition on every notification. Lock order is lifecycle,
//! then state store or pending calls; nothing that holds another lock takes
//! the lifecycle lock.

use crate::config::RunConfig;
use crate::output::append_private_output;
use crate::store::StateStore;
use crate::text::{flatten_strings, one_line, single_line, trace_stamp};
use ruddr_core::jsonrpc::Message;
use ruddr_core::state::{Status, TokenUsage};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Once};
use std::time::{Duration, Instant};

/// The longest provider output line accepted.
pub const MAX_RPC_LINE_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_RPC_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_IDLE_TURN_START_TIMEOUT: Duration = Duration::from_secs(40);
pub const DEFAULT_INTERRUPT_TIMEOUT: Duration = Duration::from_secs(30);

/// A failed JSON-RPC call.
#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    /// The app-server answered with an error.
    Response { code: i64, message: String },
    /// No answer: a write failure, a timeout, or the session ended.
    Other(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Response { code, message } => write!(f, "{message} ({code})"),
            CallError::Other(message) => f.write_str(message),
        }
    }
}

/// A turn/start failure. An ambiguous one may have started a turn the
/// controller cannot track, so it ends the session.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnStartError {
    pub ambiguous: bool,
    pub message: String,
}

impl TurnStartError {
    fn plain(message: String) -> Self {
        TurnStartError { ambiguous: false, message }
    }
    fn ambiguous(message: String) -> Self {
        TurnStartError { ambiguous: true, message }
    }
}

/// A prompt handed from the control channel to the idle loop.
pub struct PromptRequest {
    pub id: u64,
    pub text: String,
    pub images: Vec<String>,
    pub observed_turns: u32,
}

/// Turn and session lifecycle. A turn is identified by its generation; it is
/// settled once a newer generation exists or `turn_ended` is set.
#[derive(Default)]
pub struct Lifecycle {
    pub turn_gen: u64,
    pub turn_ended: bool,
    pub turn_count: u32,
    pub last_turn: Option<Status>,
    /// The session reached its final status.
    pub session_ended: bool,
    /// Waiters must stop: the session ended or the provider output closed.
    pub session_closed: bool,
    /// `stop` was accepted while idle.
    pub stop_requested: bool,
    /// The idle loop is ready to take a prompt.
    pub idle_waiting: bool,
    pub prompt_slot: Option<PromptRequest>,
    pub prompt_replies: HashMap<u64, Result<(), String>>,
    pub next_prompt: u64,
    /// Someone owns the stdin write gate.
    pub write_busy: bool,
    pub reader_done: bool,
    pub child_exited: bool,
}

impl Lifecycle {
    pub fn turn_settled(&self, generation: u64) -> bool {
        self.turn_gen != generation || self.turn_ended
    }
}

#[derive(Default)]
struct OutputState {
    started: bool,
    breaks: usize,
}

struct WriteJob {
    data: Vec<u8>,
    reply: SyncSender<io::Result<()>>,
}

pub struct Controller {
    pub cfg: RunConfig,
    pub store: StateStore,
    lifecycle: Mutex<Lifecycle>,
    wake: Condvar,
    events: Mutex<Option<File>>,
    trace: Mutex<Option<File>>,
    stderr: Mutex<Option<File>>,
    output: Mutex<OutputState>,
    result_error: Mutex<String>,
    pending: Mutex<HashMap<String, SyncSender<Map<String, Value>>>>,
    next_id: AtomicU64,
    prompt_event_id: AtomicU64,
    stdin: Mutex<Option<Sender<WriteJob>>>,
    stdin_broken: AtomicBool,
    child_pid: AtomicU32,
    child_reaped: AtomicBool,
    child_started: AtomicBool,
    /// The run owns teardown of the provider tree.
    pub stop_child: AtomicBool,
    cancel_once: Once,
    pub server: Mutex<Option<crate::control_server::ServerHandle>>,
}

impl Controller {
    pub fn new(cfg: RunConfig, store: StateStore) -> Arc<Controller> {
        Arc::new(Controller {
            cfg,
            store,
            lifecycle: Mutex::new(Lifecycle {
                turn_gen: 1,
                ..Default::default()
            }),
            wake: Condvar::new(),
            events: Mutex::new(None),
            trace: Mutex::new(None),
            stderr: Mutex::new(None),
            output: Mutex::new(OutputState::default()),
            result_error: Mutex::new(String::new()),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            prompt_event_id: AtomicU64::new(0),
            stdin: Mutex::new(None),
            stdin_broken: AtomicBool::new(false),
            child_pid: AtomicU32::new(0),
            child_reaped: AtomicBool::new(false),
            child_started: AtomicBool::new(false),
            stop_child: AtomicBool::new(false),
            cancel_once: Once::new(),
            server: Mutex::new(None),
        })
    }

    // ----- lifecycle plumbing -----

    pub fn lock(&self) -> MutexGuard<'_, Lifecycle> {
        self.lifecycle.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn notify(&self) {
        self.wake.notify_all();
    }

    /// Waits until `ready` holds or `deadline` passes. Returns the guard and
    /// whether `ready` held.
    pub fn wait_for<'a>(
        &'a self,
        mut guard: MutexGuard<'a, Lifecycle>,
        deadline: Option<Instant>,
        mut ready: impl FnMut(&mut Lifecycle) -> bool,
    ) -> (MutexGuard<'a, Lifecycle>, bool) {
        loop {
            if ready(&mut guard) {
                return (guard, true);
            }
            match deadline {
                None => guard = self.wake.wait(guard).unwrap_or_else(|e| e.into_inner()),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return (guard, false);
                    }
                    guard = self.wake.wait_timeout(guard, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
                }
            }
        }
    }

    /// Releases every waiter: pending calls see a disconnect and the loops see
    /// `session_closed`.
    fn close_session(&self, lifecycle: &mut Lifecycle) {
        lifecycle.session_closed = true;
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.notify();
    }

    // ----- logs -----

    /// Creates events.jsonl, trace.log, provider.stderr.log, and an empty
    /// output.md, each new and 0600.
    pub fn open_logs(&self) -> ruddr_core::Result<()> {
        let state = self.store.snapshot();
        let open = |path: &str| -> ruddr_core::Result<File> {
            let path = std::path::Path::new(path);
            let file = ruddr_core::fsutil::create_private_file_new(path)
                .map_err(|e| ruddr_core::Error::failed(format!("open {}: {e}", path.display())))?;
            ruddr_core::fsutil::set_mode(path, 0o600)?;
            Ok(file)
        };
        let events = open(&state.events_path)?;
        let trace = open(&state.trace_path)?;
        let stderr = open(&state.stderr_path)?;
        open(&state.output_path)?;
        *self.events.lock().unwrap_or_else(|e| e.into_inner()) = Some(events);
        *self.trace.lock().unwrap_or_else(|e| e.into_inner()) = Some(trace);
        *self.stderr.lock().unwrap_or_else(|e| e.into_inner()) = Some(stderr);
        Ok(())
    }

    pub fn close_logs(&self) {
        self.stderr.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.trace.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.events.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    /// Appends one trace record: `<RFC 3339 seconds> <message on one line>`.
    pub fn trace(&self, message: impl AsRef<str>) {
        let mut trace = self.trace.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(file) = trace.as_mut() {
            let _ = writeln!(file, "{} {}", trace_stamp(), single_line(message.as_ref()));
        }
    }

    #[cfg(test)]
    pub fn set_trace_file(&self, file: File) {
        *self.trace.lock().unwrap() = Some(file);
    }

    #[cfg(test)]
    pub fn pending_len(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    #[cfg(test)]
    pub fn register_pending(&self, id: &str) -> mpsc::Receiver<Map<String, Value>> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.pending.lock().unwrap().insert(id.into(), tx);
        rx
    }

    /// Feeds one provider output line, as the reader thread would.
    #[cfg(test)]
    pub fn deliver(&self, line: &str) {
        self.handle_line(line.as_bytes().to_vec());
    }

    fn append_event(&self, line: &[u8]) -> io::Result<()> {
        match self.events.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            Some(file) => file.write_all(line),
            None => Err(io::Error::other("events log is not open")),
        }
    }

    fn record_prompt_attempt(&self, id: &str, text: &str, images: &[String]) -> io::Result<()> {
        let thread = self.store.snapshot().thread_id.unwrap_or_default();
        let event = json!({
            "method": "item/completed",
            "params": {
                "threadId": thread,
                "item": {"id": id, "type": "userMessage", "text": text, "images": images, "origin": "ruddr", "status": "pending"},
            },
        });
        self.append_event(&json_line(&event))
    }

    fn record_prompt_decision(&self, id: &str, decision: &str) -> io::Result<()> {
        self.append_event(&json_line(
            &json!({"method": format!("ruddr/prompt/{decision}"), "params": {"promptId": id}}),
        ))
    }

    /// Marks a turn boundary in output.md; it is written before the next
    /// message, so a turn without output still leaves a rule.
    pub fn append_output_separator(&self) {
        let mut output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        if output.started {
            output.breaks += 1;
        }
    }

    /// Appends one completed agent message to output.md, in arrival order.
    pub fn record_agent_message(&self, text: &str) -> io::Result<()> {
        let mut output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        let content = if output.started {
            format!("\n{}{text}\n", "---\n\n".repeat(output.breaks))
        } else {
            format!("{text}\n")
        };
        append_private_output(std::path::Path::new(&self.store.snapshot().output_path), &content)?;
        output.started = true;
        output.breaks = 0;
        Ok(())
    }

    pub fn private_result_error(&self) -> String {
        self.result_error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn append_result_error(&self, text: &str) {
        let mut result = self.result_error.lock().unwrap_or_else(|e| e.into_inner());
        if !result.is_empty() {
            result.push_str("; ");
        }
        result.push_str(text);
    }

    // ----- the child process -----

    /// Starts the app-server child with piped stdio and its stderr in
    /// provider.stderr.log, then starts the reader and waiter threads.
    pub fn start_child(self: &Arc<Self>) -> ruddr_core::Result<()> {
        use std::process::{Command, Stdio};
        let program = &self.cfg.child_command[0];
        let stderr = match self.stderr.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            Some(file) => Stdio::from(file.try_clone()?),
            None => Stdio::null(),
        };
        let mut command = Command::new(program);
        command
            .args(&self.cfg.child_command[1..])
            .current_dir(&self.cfg.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr);
        crate::process::configure_child(&mut command);
        let executable = std::path::Path::new(program)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut child = command
            .spawn()
            .map_err(|e| ruddr_core::Error::failed(format!("start {executable}: {e}")))?;
        let pid = child.id();
        self.child_pid.store(pid, Ordering::SeqCst);
        self.child_started.store(true, Ordering::SeqCst);
        let stdout = child.stdout.take().expect("piped stdout");
        let stdin = child.stdin.take().expect("piped stdin");
        self.attach_stdin(Box::new(stdin));
        let reader = Arc::clone(self);
        if let Err(error) = std::thread::Builder::new()
            .name("ruddr-reader".into())
            .spawn(move || reader.read_child(io::BufReader::new(stdout)))
        {
            self.reap_failed_start(&mut child, true);
            return Err(error.into());
        }
        let waiter = Arc::clone(self);
        let child_slot = Arc::new(Mutex::new(Some(child)));
        let waiting_child = child_slot.clone();
        if let Err(error) = std::thread::Builder::new().name("ruddr-waiter".into()).spawn(move || {
            // Like Go's exec.Cmd, reap only once the provider output is drained.
            drop(waiter.wait_for(waiter.lock(), None, |l| l.reader_done));
            let mut child = waiting_child.lock().unwrap_or_else(|e| e.into_inner()).take().expect("owned child");
            let _ = child.wait();
            waiter.child_reaped.store(true, Ordering::SeqCst);
            waiter.lock().child_exited = true;
            waiter.notify();
        }) {
            if let Some(mut child) = child_slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
                self.reap_failed_start(&mut child, false);
            }
            return Err(error.into());
        }
        if let Err(e) = self.store.update(|state| state.child_pid = pid as i64) {
            self.stop_child.store(true, Ordering::SeqCst);
            self.shutdown_child();
            return Err(e.context("persist child pid"));
        }
        self.trace(format!(
            "[start] child pid={pid} executable={executable} args={}",
            self.cfg.child_command.len() - 1
        ));
        Ok(())
    }

    fn reap_failed_start(&self, child: &mut std::process::Child, reader_missing: bool) {
        self.stop_child.store(true, Ordering::SeqCst);
        self.close_stdin();
        self.terminate(true);
        let deadline = Instant::now() + Duration::from_secs(3);
        let reaped = loop {
            match child.try_wait() {
                Ok(Some(_)) => break true,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                _ => break false,
            }
        };
        self.child_reaped.store(reaped, Ordering::SeqCst);
        let mut lifecycle = self.lock();
        lifecycle.child_exited = reaped;
        lifecycle.reader_done |= reader_missing;
        self.notify();
    }

    /// Hands child stdin to a writer thread, so every write can be bounded.
    pub fn attach_stdin(&self, mut writer: Box<dyn Write + Send>) {
        let (tx, rx) = mpsc::channel::<WriteJob>();
        let spawned = std::thread::Builder::new().name("ruddr-stdin".into()).spawn(move || {
            for job in rx {
                let result = writer.write_all(&job.data).and_then(|_| writer.flush());
                let _ = job.reply.send(result);
            }
            // Dropping the writer closes the child's stdin.
        });
        if spawned.is_ok() {
            *self.stdin.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        }
    }

    fn close_stdin(&self) {
        self.stdin.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    /// Signals the provider tree. The default is SIGTERM; `force` kills.
    pub fn terminate(&self, force: bool) {
        crate::process::terminate_process_tree(
            self.child_pid.load(Ordering::SeqCst),
            force,
            self.child_reaped.load(Ordering::SeqCst),
        );
    }

    pub fn child_pid(&self) -> u32 {
        self.child_pid.load(Ordering::SeqCst)
    }

    /// Stops the child: closes stdin, waits for it to exit, and escalates to
    /// a kill. When the run owns teardown, the whole tree is killed even after
    /// the parent exits, because descendants may have ignored SIGTERM.
    pub fn shutdown_child(&self) {
        if !self.child_started.load(Ordering::SeqCst) {
            return;
        }
        let owns_teardown = self.stop_child.load(Ordering::SeqCst);
        if owns_teardown {
            self.terminate(false);
        }
        self.close_stdin();
        let deadline = Instant::now() + Duration::from_secs(3);
        let (guard, exited) = self.wait_for(self.lock(), Some(deadline), |l| l.child_exited);
        drop(guard);
        if !exited {
            self.terminate(true);
            let (guard, exited) = self.wait_for(self.lock(), Some(Instant::now() + Duration::from_secs(5)), |l| l.child_exited);
            drop(guard);
            if !exited {
                self.trace("[warn] provider did not exit after SIGKILL; leaving it");
            }
        }
        if self.stop_child.load(Ordering::SeqCst) {
            self.terminate(true);
        }
        drop(self.wait_for(self.lock(), Some(Instant::now() + Duration::from_secs(1)), |l| l.reader_done));
    }

    // ----- JSON-RPC -----

    /// Sends a request and waits for its response, all within `timeout`.
    pub fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, CallError> {
        let deadline = Instant::now() + timeout;
        let id = format!("ruddr-{}", self.next_id.fetch_add(1, Ordering::SeqCst) + 1);
        let (tx, rx) = mpsc::sync_channel(1);
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), tx);
        struct Unregister<'a>(&'a Controller, &'a str);
        impl Drop for Unregister<'_> {
            fn drop(&mut self) {
                self.0.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(self.1);
            }
        }
        let _unregister = Unregister(self, &id);
        let ended = |c: &Controller| CallError::Other(format!("session ended while waiting for {method}: {}", c.store.snapshot().status));
        if self.lock().session_closed {
            return Err(ended(self));
        }
        let line = Message::request(id.clone(), method, params).to_line();
        self.write_line(line, deadline.saturating_duration_since(Instant::now()))
            .map_err(CallError::Other)?;
        let timed_out = || CallError::Other(format!("{method} timed out after {}", ruddr_core::duration::format(timeout)));
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timed_out());
        }
        match rx.recv_timeout(remaining) {
            Ok(mut response) => match response.get("error").filter(|e| e.is_object()) {
                Some(error) => Err(CallError::Response {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: error.get("message").and_then(Value::as_str).unwrap_or_default().to_string(),
                }),
                None => Ok(response.remove("result").unwrap_or(Value::Null)),
            },
            Err(RecvTimeoutError::Timeout) => Err(timed_out()),
            Err(RecvTimeoutError::Disconnected) => Err(ended(self)),
        }
    }

    pub fn notify_rpc(&self, method: &str, params: Value) -> Result<(), String> {
        self.write_line(Message::notification(method, params).to_line(), DEFAULT_RPC_WRITE_TIMEOUT)
    }

    /// Writes one line to the child's stdin. Waiting for the write gate and
    /// the write itself share `timeout`; a write that overruns it abandons
    /// stdin, so the provider sees no partial frame followed by another.
    pub fn write_line(&self, data: Vec<u8>, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        {
            let (mut lifecycle, ready) = self.wait_for(self.lock(), Some(deadline), |l| l.session_closed || !l.write_busy);
            if lifecycle.session_closed {
                return Err("session ended before app-server stdin write".into());
            }
            if !ready {
                return Err("app-server stdin write timed out".into());
            }
            lifecycle.write_busy = true;
        }
        struct Gate<'a>(&'a Controller);
        impl Drop for Gate<'_> {
            fn drop(&mut self) {
                self.0.lock().write_busy = false;
                self.0.notify();
            }
        }
        let _gate = Gate(self);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("app-server stdin write timed out".into());
        }
        let sender = self.stdin.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(sender) = sender.filter(|_| !self.stdin_broken.load(Ordering::SeqCst)) else {
            return Err("app-server stdin is closed".into());
        };
        let (reply, result) = mpsc::sync_channel(1);
        if sender.send(WriteJob { data, reply }).is_err() {
            return Err("app-server stdin is closed".into());
        }
        match result.recv_timeout(remaining) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => self.fail_stdin(format!("write app-server stdin: {e}")),
            Err(RecvTimeoutError::Timeout) => self.fail_stdin(format!(
                "app-server stdin write timed out after {}",
                ruddr_core::duration::format(remaining)
            )),
            Err(RecvTimeoutError::Disconnected) => self.fail_stdin("app-server stdin is closed".into()),
        }
    }

    fn fail_stdin(&self, error: String) -> Result<(), String> {
        self.stdin_broken.store(true, Ordering::SeqCst);
        self.close_stdin();
        self.stop_child.store(true, Ordering::SeqCst);
        self.fail(&error);
        // Closing the sender cannot interrupt a writer blocked on ChildStdin.
        self.terminate(false);
        Err(error)
    }

    // ----- provider output -----

    /// Reads provider output until EOF: every line goes to events.jsonl, then
    /// responses wake their callers and notifications update the run.
    pub fn read_child(self: &Arc<Self>, mut reader: impl BufRead) {
        let result = loop {
            match read_line_limited(&mut reader, MAX_RPC_LINE_BYTES) {
                Ok(None) => break Ok(()),
                Ok(Some(line)) => self.handle_line(line),
                Err(e) => break Err(e),
            }
        };
        match result {
            Err(e) => self.fail(&format!("read provider output: {e}")),
            Ok(()) if self.cfg.idle => self.end_session(
                Status::Failed,
                "provider output closed while the idle session was expected to remain available",
            ),
            Ok(()) if !self.store.snapshot().status.is_terminal() => self.fail("provider output closed before turn completed"),
            Ok(()) => {}
        }
        let mut lifecycle = self.lock();
        self.close_session(&mut lifecycle);
        lifecycle.reader_done = true;
        self.notify();
    }

    fn handle_line(&self, mut line: Vec<u8>) {
        line.push(b'\n');
        let _ = self.append_event(&line);
        line.pop();
        let message = match serde_json::from_slice::<Value>(&line) {
            Ok(Value::Object(message)) => message,
            Ok(_) => return self.trace("[warn] invalid provider JSON: not an object"),
            Err(e) => return self.trace(format!("[warn] invalid provider JSON: {e}")),
        };
        let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
        if !method.is_empty() {
            let method = method.to_string();
            return self.handle_server_message(message, &method);
        }
        if let Some(id) = rpc_id(message.get("id")) {
            // Claim the reply once: a duplicate never reaches a mailbox again.
            let mailbox = self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            if let Some(mailbox) = mailbox {
                let _ = mailbox.try_send(message);
            }
        }
    }

    fn handle_server_message(&self, message: Map<String, Value>, method: &str) {
        if let Some(id) = message.get("id") {
            return self.reject_server_request(id, method);
        }
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "turn/started" => {
                let thread = str_at(&params, &["threadId"]);
                let turn = str_at(&params, &["turn", "id"]);
                if !self.is_root_turn(thread, turn) {
                    return self.trace(format!("[turn] nested started thread={thread} turn={turn}"));
                }
                let result = {
                    let lifecycle = self.lock();
                    let open = !lifecycle.turn_ended && !lifecycle.session_ended;
                    self.store.update(|state| {
                        state.turn_id = Some(turn.to_string());
                        if open {
                            state.status = Status::Active;
                        }
                    })
                };
                if let Err(e) = result {
                    self.stop_child.store(true, Ordering::SeqCst);
                    self.fail(&format!("persist started turn: {e}"));
                    self.terminate(false);
                    return;
                }
                self.trace(format!("[turn] started {turn}"));
            }
            "turn/completed" => self.handle_turn_completed(&params),
            "item/started" | "item/updated" | "item/completed" => self.handle_item(method, &params),
            "error" => self.trace(format!("[warn] {}", str_at(&params, &["error", "message"]))),
            "thread/tokenUsage/updated" => self.handle_token_usage(&params),
            _ => {}
        }
    }

    /// Answers a server-initiated request with an error, echoing its exact ID,
    /// so an approval prompt never hangs the run.
    pub fn reject_server_request(&self, id: &Value, method: &str) {
        self.trace(format!("[warn] unsupported server request {method}"));
        let response = json!({
            "id": id,
            "error": {"code": -32601, "message": "Ruddr cannot answer this interactive request; run with approvalPolicy=never"},
        });
        let _ = self.write_line(json_line(&response), DEFAULT_RPC_WRITE_TIMEOUT);
    }

    fn is_root_turn(&self, thread: &str, turn: &str) -> bool {
        let state = self.store.snapshot();
        if !thread.is_empty() && thread != state.thread_id.as_deref().unwrap_or_default() {
            return false;
        }
        !turn.is_empty() && state.turn_id.as_deref().is_none_or(|current| current == turn)
    }

    fn handle_turn_completed(&self, params: &Value) {
        let thread = str_at(params, &["threadId"]);
        let turn = str_at(params, &["turn", "id"]);
        if !self.is_root_turn(thread, turn) {
            return self.trace(format!("[turn] nested completed thread={thread} turn={turn}"));
        }
        let status = match str_at(params, &["turn", "status"]) {
            "completed" => Status::Completed,
            "interrupted" => Status::Interrupted,
            "failed" | "inProgress" | "" => Status::Failed,
            other => {
                self.trace(format!("[warn] unknown turn status {other}; recording failed"));
                Status::Failed
            }
        };
        let error = params
            .pointer("/turn/error")
            .filter(|e| e.is_object())
            .map(|e| str_at(e, &["message"]))
            .unwrap_or_default();
        self.finish_turn(status, error);
    }

    fn handle_item(&self, method: &str, params: &Value) {
        let empty = Map::new();
        let item = params.get("item").and_then(Value::as_object).unwrap_or(&empty);
        let field = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or_default();
        let item_type = field("type");
        if method == "item/updated" {
            return;
        }
        let completed = method == "item/completed";
        let prefix = if !completed {
            "[in_progress]"
        } else if field("status") == "failed" {
            "[failed]"
        } else {
            "[completed]"
        };
        let command = field("command");
        match item_type {
            "commandExecution" => self.trace(format!("{prefix} $ {}", one_line(command, 240))),
            "fileChange" => self.trace(format!(
                "{prefix} {}",
                one_line(if command.is_empty() { "file changes" } else { command }, 240)
            )),
            "webSearch" | "toolCall" => self.trace(format!(
                "{prefix} {}",
                one_line(if command.is_empty() { field("toolName") } else { command }, 240)
            )),
            "reasoning" if completed => self.trace(format!(
                "[think] {}",
                one_line(&flatten_strings(item.get("summary").unwrap_or(&Value::Null)), 240)
            )),
            "reasoning" => {}
            "agentMessage" if completed => {
                let text = field("text");
                if text.is_empty() {
                    return;
                }
                if let Err(e) = self.record_agent_message(text) {
                    self.stop_child.store(true, Ordering::SeqCst);
                    self.fail(&format!("persist agent output: {e}"));
                    self.terminate(false);
                    return;
                }
                self.trace(format!("[say] {}", single_line(text)));
            }
            "agentMessage" => {}
            other if completed && other != "userMessage" => self.trace(format!("{prefix} {other}")),
            _ => {}
        }
    }

    fn handle_token_usage(&self, params: &Value) {
        let state = self.store.snapshot();
        let thread = str_at(params, &["threadId"]);
        if let Some(current) = state.thread_id.as_deref().filter(|t| !t.is_empty())
            && !thread.is_empty()
            && thread != current
        {
            return;
        }
        let int = |path: &str| params.pointer(path).and_then(Value::as_i64).unwrap_or(0);
        let last = params.pointer("/tokenUsage/last").filter(|v| !v.is_null());
        let cost = params.get("costUsd").and_then(Value::as_f64).unwrap_or(0.0);
        let mut usage = TokenUsage {
            input_tokens: int("/tokenUsage/total/inputTokens"),
            cached_input_tokens: int("/tokenUsage/total/cachedInputTokens"),
            output_tokens: int("/tokenUsage/total/outputTokens"),
            total_tokens: int("/tokenUsage/total/totalTokens"),
            context_tokens: None,
            context_window: match int("/tokenUsage/modelContextWindow") {
                0 => int("/tokenUsage/contextWindow"),
                window => window,
            },
            cost_usd: cost,
        };
        if usage.total_tokens == 0 && usage.input_tokens == 0 && usage.output_tokens == 0 && cost == 0.0 && last.is_none() {
            return;
        }
        if let Some(tokens) = last.and_then(|l| l.get("totalTokens")).and_then(Value::as_i64).filter(|t| *t >= 0) {
            usage.context_tokens = Some(tokens);
        }
        if let Some(previous) = &state.token_usage {
            if usage.cost_usd == 0.0 {
                usage.cost_usd = previous.cost_usd;
            }
            if usage.context_window == 0 {
                usage.context_window = previous.context_window;
            }
        }
        let summary = format!(
            "[usage] in={} cached={} out={} total={}",
            usage.input_tokens, usage.cached_input_tokens, usage.output_tokens, usage.total_tokens
        );
        let cost = usage.cost_usd;
        if let Err(e) = self.store.update(|state| state.token_usage = Some(usage)) {
            return self.trace(format!("[warn] persist token usage: {e}"));
        }
        if cost > 0.0 {
            self.trace(format!("{summary} cost=${cost:.4}"));
        } else {
            self.trace(summary);
        }
    }

    // ----- turn and session lifecycle -----

    /// Opens a turn and sends turn/start. Turn one reuses the first
    /// generation; later turns (idle mode) open a new one. The prompt text
    /// reaches only events.jsonl and a truncated trace line.
    pub fn start_turn(&self, prompt: &str, images: &[String], timeout: Duration) -> Result<(), TurnStartError> {
        let turn_number = {
            let mut lifecycle = self.lock();
            lifecycle.turn_count += 1;
            if lifecycle.turn_count > 1 {
                lifecycle.turn_gen += 1;
                lifecycle.turn_ended = false;
            }
            lifecycle.turn_count
        };
        let update = self.store.update(|state| {
            state.turns = turn_number;
            if turn_number > 1 {
                state.turn_id = None;
                state.error = None;
                state.completed_at = None;
            }
        });
        if let Err(e) = update {
            self.abandon_turn();
            return Err(TurnStartError::plain(format!("persist turn count: {e}")));
        }
        if turn_number > 1 {
            self.append_output_separator();
            self.trace(format!("[turn] prompt #{turn_number}: {}", one_line(prompt, 180)));
        }
        let state = self.store.snapshot();
        let thread = state.thread_id.clone().unwrap_or_default();
        let mut params = json!({"threadId": thread, "input": ruddr_core::images::user_input(prompt, images)});
        if !self.cfg.effort.is_empty() {
            params["effort"] = Value::String(self.cfg.effort.clone());
        }
        let prompt_id = format!("ruddr-prompt-{}", self.prompt_event_id.fetch_add(1, Ordering::SeqCst) + 1);
        if let Err(e) = self.record_prompt_attempt(&prompt_id, prompt, images) {
            return Err(TurnStartError::plain(match self.rollback_rejected_turn(turn_number) {
                Ok(()) => format!("record prompt attempt: {e}"),
                Err(rollback) => format!("record prompt attempt: {e}; rollback: {rollback}"),
            }));
        }
        let result = match self.call("turn/start", params, timeout) {
            Ok(result) => result,
            Err(error) => {
                let accepted = self.store.snapshot().turn_id.is_some();
                if !accepted && matches!(error, CallError::Response { .. }) {
                    if let Err(e) = self.record_prompt_decision(&prompt_id, "rejected") {
                        return Err(TurnStartError::ambiguous(format!("record rejected prompt: {e}")));
                    }
                    if let Err(e) = self.rollback_rejected_turn(turn_number) {
                        return Err(TurnStartError::ambiguous(format!(
                            "turn/start rejection could not be rolled back: {e}"
                        )));
                    }
                    return Err(TurnStartError::plain(format!("start turn: {error}")));
                }
                let decision = if accepted { "accepted" } else { "unknown" };
                if let Err(e) = self.record_prompt_decision(&prompt_id, decision) {
                    return Err(TurnStartError::ambiguous(format!("record ambiguous prompt outcome: {e}")));
                }
                return Err(TurnStartError::ambiguous(format!("start turn outcome is ambiguous: {error}")));
            }
        };
        let turn_id = str_at(&result, &["turn", "id"]).to_string();
        if turn_id.is_empty() {
            if let Err(e) = self.record_prompt_decision(&prompt_id, "unknown") {
                return Err(TurnStartError::ambiguous(format!(
                    "turn/start response returned no turn id; record unknown prompt outcome: {e}"
                )));
            }
            return Err(TurnStartError::ambiguous(
                "turn/start outcome is ambiguous: response returned no turn id".into(),
            ));
        }
        if let Err(e) = self.record_prompt_decision(&prompt_id, "accepted") {
            return Err(TurnStartError::ambiguous(format!("record accepted prompt: {e}")));
        }
        let update = {
            let lifecycle = self.lock();
            let open = !lifecycle.turn_ended && !lifecycle.session_ended;
            self.store.update(|state| {
                state.turn_id = Some(turn_id.clone());
                if open {
                    state.status = Status::Active;
                }
            })
        };
        if let Err(e) = update {
            return Err(TurnStartError::ambiguous(format!(
                "turn/start outcome is ambiguous: persist active turn: {e}"
            )));
        }
        self.trace(format!("[turn] active thread={thread} turn={turn_id}"));
        Ok(())
    }

    /// Undoes the bookkeeping of a turn the provider rejected.
    pub fn rollback_rejected_turn(&self, turn_number: u32) -> Result<(), String> {
        self.store
            .update(|state| {
                if state.turns == turn_number {
                    state.turns = turn_number - 1;
                }
                state.turn_id = None;
            })
            .map_err(|e| format!("persist rejected turn rollback: {e}"))?;
        {
            let mut output = self.output.lock().unwrap_or_else(|e| e.into_inner());
            output.breaks = output.breaks.saturating_sub(1);
        }
        let mut lifecycle = self.lock();
        if lifecycle.turn_ended {
            return Err("rejected turn ended before rollback".into());
        }
        lifecycle.turn_ended = true;
        if lifecycle.turn_count == turn_number {
            lifecycle.turn_count -= 1;
        }
        self.notify();
        Ok(())
    }

    /// Settles an open turn without a terminal status, so a failed turn/start
    /// can return the session to idle.
    fn abandon_turn(&self) {
        let mut lifecycle = self.lock();
        if !lifecycle.turn_ended {
            lifecycle.turn_ended = true;
            self.notify();
        }
    }

    /// Waits for the current turn to settle, enforcing the turn watchdog.
    pub fn wait_turn(&self) {
        let lifecycle = self.lock();
        let generation = lifecycle.turn_gen;
        let timeout = self.cfg.turn_timeout;
        let deadline = (!timeout.is_zero()).then(|| Instant::now() + timeout);
        let (lifecycle, settled) = self.wait_for(lifecycle, deadline, |l| l.turn_settled(generation));
        drop(lifecycle);
        if !settled {
            self.stop_child.store(true, Ordering::SeqCst);
            self.trace(format!(
                "[error] active turn exceeded watchdog {}",
                ruddr_core::duration::format(timeout)
            ));
            self.end_session(Status::Failed, "turn watchdog expired");
            self.terminate(false);
        }
    }

    /// Ends the open turn. Returns false when no turn was open.
    pub fn finish_turn(&self, status: Status, error: &str) -> bool {
        let mut lifecycle = self.lock();
        if lifecycle.turn_ended || lifecycle.session_ended {
            return false;
        }
        lifecycle.turn_ended = true;
        lifecycle.last_turn = Some(status);
        self.persist_terminal(&lifecycle, status, error);
        self.notify();
        true
    }

    /// Ends the whole run: settles any open turn, persists the terminal status
    /// even when idle, and releases every waiter.
    pub fn end_session(&self, status: Status, error: &str) {
        let mut lifecycle = self.lock();
        if lifecycle.session_ended {
            return;
        }
        lifecycle.session_ended = true;
        if !lifecycle.turn_ended {
            lifecycle.turn_ended = true;
            lifecycle.last_turn = Some(status);
        }
        self.persist_terminal(&lifecycle, status, error);
        self.close_session(&mut lifecycle);
    }

    /// Claims the observed turn and ends its session atomically. Returns false
    /// when that turn settled or a newer one started.
    pub fn end_session_if_turn_open(&self, generation: u64, status: Status, error: &str) -> bool {
        let mut lifecycle = self.lock();
        if lifecycle.turn_gen != generation || lifecycle.turn_ended || lifecycle.session_ended {
            return false;
        }
        lifecycle.session_ended = true;
        lifecycle.turn_ended = true;
        lifecycle.last_turn = Some(status);
        // Publish teardown ownership before waking the shutdown path.
        self.stop_child.store(true, Ordering::SeqCst);
        self.persist_terminal(&lifecycle, status, error);
        self.close_session(&mut lifecycle);
        true
    }

    /// Ends the run as interrupted after a signal.
    pub fn cancel_session(&self) {
        self.cancel_once.call_once(|| {
            self.stop_child.store(true, Ordering::SeqCst);
            self.trace("[interrupt] controller received a stop signal");
            self.end_session(Status::Interrupted, "");
            self.terminate(false);
        });
    }

    /// Ends the whole session as failed.
    pub fn fail(&self, error: &str) {
        self.end_session(Status::Failed, error);
    }

    fn persist_terminal(&self, lifecycle: &Lifecycle, status: Status, error: &str) {
        if !error.is_empty() {
            self.append_result_error(error);
            self.trace(format!("[error] {}", one_line(error, 500)));
        }
        // A completed turn is not a completed idle session: never expose a
        // terminal status to waiters while the session stays open.
        let idle_open = self.cfg.idle && !lifecycle.session_ended;
        let last_turn = lifecycle.last_turn;
        let result = self.store.update(|state| {
            state.status = status;
            state.error = redacted_state_error(status, error);
            state.completed_at = Some(ruddr_core::time::now_rfc3339());
            if last_turn.is_some() {
                state.last_turn = last_turn;
            }
            if idle_open {
                state.status = Status::Idle;
                state.error = None;
                state.completed_at = None;
            }
        });
        if let Err(e) = result {
            let message = format!("persist terminal state: {e}");
            self.append_result_error(&message);
            self.trace(format!("[error] {}", one_line(&message, 500)));
        }
        self.trace(format!("[turn] {status}"));
    }

    /// Restores the last turn's terminal status so the run exits with a
    /// truthful state instead of `idle`.
    pub fn persist_final_idle_exit(&self) {
        let mut lifecycle = self.lock();
        if lifecycle.session_ended {
            return;
        }
        lifecycle.session_ended = true;
        let status = lifecycle.last_turn.unwrap_or(Status::Completed);
        if let Err(e) = self.store.update(|state| {
            state.status = status;
            state.completed_at = Some(ruddr_core::time::now_rfc3339());
        }) {
            self.append_result_error(&format!("persist final state: {e}"));
        }
        self.close_session(&mut lifecycle);
    }

    /// Guards against exiting with a non-terminal status between turns.
    pub fn ensure_terminal_exit(&self) {
        if !self.store.snapshot().status.is_terminal() {
            self.persist_final_idle_exit();
        }
    }

    /// The failure an interrupted turn settled with, if any.
    pub fn interrupt_settlement_result(&self) -> Result<(), String> {
        let last_turn = self.lock().last_turn;
        let state = self.store.snapshot();
        if state.status == Status::Failed || last_turn == Some(Status::Failed) {
            let result = self.private_result_error();
            if !result.is_empty() {
                return Err(result);
            }
            return Err(state
                .error
                .unwrap_or_else(|| "provider failed before the interrupted turn settled".into()));
        }
        Ok(())
    }
}

/// The generic error `state.json` may carry; details stay in the private logs.
pub fn redacted_state_error(status: Status, error: &str) -> Option<String> {
    if error.is_empty() {
        return None;
    }
    Some(match status {
        Status::Interrupted => "turn interrupted; see trace.log and provider.stderr.log".into(),
        _ => "turn failed; see trace.log and provider.stderr.log".into(),
    })
}

/// A JSON-RPC response ID as a correlation key: a non-empty string or an
/// integer. Floats and other values never match a pending call.
pub fn rpc_id(id: Option<&Value>) -> Option<String> {
    match id? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) if number.is_i64() => Some(number.to_string()),
        _ => None,
    }
}

/// The string at `path`, or "".
pub fn str_at<'a>(value: &'a Value, path: &[&str]) -> &'a str {
    let mut current = value;
    for key in path {
        match current.get(key) {
            Some(next) => current = next,
            None => return "",
        }
    }
    current.as_str().unwrap_or_default()
}

fn json_line(value: &Value) -> Vec<u8> {
    let mut line = serde_json::to_vec(value).expect("JSON values always serialize");
    line.push(b'\n');
    line
}

/// Reads one line without its line ending, or `None` at EOF. A line longer
/// than `limit` is an error, like Go's `bufio.Scanner`.
pub fn read_line_limited(reader: &mut impl BufRead, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let read = reader.by_ref().take(limit as u64 + 1).read_until(b'\n', &mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    } else if line.len() > limit {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "token too long"));
    }
    Ok(Some(line))
}
