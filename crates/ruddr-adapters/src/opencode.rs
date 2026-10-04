//! The OpenCode 2 adapter. Port of opencode/runtime.ts.
//!
//! The adapter starts `opencode2 serve --stdio`, which announces a loopback
//! URL on its first stdout line, and drives the session over that HTTP API
//! with a random Basic password passed only in the child's environment. A turn
//! is one prompt; a steer is a prompt with `delivery: "steer"`. The turn ends
//! when the session's `wait` route returns, after any accepted steer has been
//! folded in.

use crate::child::ChildProcess;
use crate::protocol::{
    AResult, Adapter, AdapterError, Emit, LineReader, MAX_LINE_BYTES, lock, num, number, optional_string, read_text_input, record,
    required_string, text_content, uuid_v4,
};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HTTP_HEADER_BYTES: usize = 64 * 1024;
const MAX_HTTP_BODY_BYTES: usize = MAX_LINE_BYTES;

#[derive(Debug, Clone, PartialEq)]
pub struct OpenCodeThread {
    pub id: String,
    pub cwd: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub executable: String,
    pub ephemeral: bool,
    pub sandbox: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub outcome: Option<String>,
    pub messages: Vec<Map<String, Value>>,
    pub tokens: Option<Map<String, Value>>,
    pub cost: f64,
    pub context_window: f64,
}

/// The OpenCode session API the adapter needs. Every method may run while
/// another one is blocked on a different thread.
pub trait Backend: Send + Sync {
    fn open(&self, thread: &mut OpenCodeThread, resumed: bool) -> AResult<String>;
    fn prompt(&self, session: &str, text: &str, steer: bool, effort: Option<&str>) -> AResult<String>;
    fn wait(&self, session: &str) -> AResult<Snapshot>;
    fn interrupt(&self, session: &str) -> AResult<()>;
    fn close(&self, session: Option<&str>, remove_session: bool);
}

struct Turn {
    serial: u64,
    id: String,
    interrupted: bool,
    settling: bool,
    steer_generation: u64,
    pending_steers: u32,
}

#[derive(Default)]
struct State {
    initialized: bool,
    closed: bool,
    thread: Option<OpenCodeThread>,
    turn: Option<Turn>,
    turn_serial: u64,
    emitted_messages: HashSet<String>,
    completion: Option<Arc<crate::protocol::Latch>>,
}

struct Inner {
    emit: Emit,
    backend: Box<dyn Backend>,
    executable: String,
    state: Mutex<State>,
    steers_settled: Condvar,
}

pub struct OpenCodeAdapter {
    inner: Arc<Inner>,
}

impl OpenCodeAdapter {
    pub fn new(emit: Emit, executable: String) -> OpenCodeAdapter {
        OpenCodeAdapter::with_backend(emit, executable, Box::new(HttpBackend::new(DEFAULT_HTTP_TIMEOUT)))
    }

    pub fn with_backend(emit: Emit, executable: String, backend: Box<dyn Backend>) -> OpenCodeAdapter {
        OpenCodeAdapter {
            inner: Arc::new(Inner {
                emit,
                backend,
                executable,
                state: Mutex::new(State::default()),
                steers_settled: Condvar::new(),
            }),
        }
    }
}

impl Adapter for OpenCodeAdapter {
    fn dispatch(&self, method: &str, params: &Value) -> AResult<Value> {
        let inner = &self.inner;
        match method {
            "initialize" => {
                lock(&inner.state).initialized = true;
                Ok(json!({
                    "serverInfo": { "name": "ruddr-opencode2-adapter", "version": "1" },
                    "capabilities": { "experimentalApi": true },
                }))
            }
            "initialized" => Ok(Value::Null),
            "thread/start" => inner.acquire_thread(params, false),
            "thread/resume" => inner.acquire_thread(params, true),
            "turn/start" => Inner::start_turn(inner, params),
            "turn/steer" => inner.steer_turn(params),
            "turn/interrupt" => inner.interrupt_turn(params),
            _ => Err(AdapterError::not_found(format!(
                "method {method} is not supported by the OpenCode 2 adapter"
            ))),
        }
    }

    fn close(&self) {
        let (thread, completion) = {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return;
            }
            state.closed = true;
            (state.thread.clone(), state.completion.clone())
        };
        self.inner
            .backend
            .close(thread.as_ref().map(|t| t.id.as_str()), thread.as_ref().is_some_and(|t| t.ephemeral));
        if let Some(completion) = completion {
            completion.wait(Duration::from_secs(1));
        }
    }
}

impl Inner {
    fn acquire_thread(&self, params: &Value, resumed: bool) -> AResult<Value> {
        {
            let state = lock(&self.state);
            if !state.initialized {
                return Err(AdapterError::invalid("initialize must run first"));
            }
            if state.thread.is_some() {
                return Err(AdapterError::invalid("a thread is already configured"));
            }
        }
        let input = record(params, "thread parameters")?;
        let requested = if resumed {
            required_string(input.get("threadId"), "threadId")?
        } else {
            String::new()
        };
        let mut thread = OpenCodeThread {
            id: requested,
            cwd: required_string(input.get("cwd"), "cwd")?,
            executable: optional_string(input.get("providerPath")).unwrap_or_else(|| self.executable.clone()),
            ephemeral: input.get("ephemeral") == Some(&Value::Bool(true)),
            sandbox: optional_string(input.get("sandbox")),
            model: optional_string(input.get("model")),
            effort: None,
        };
        thread.id = self.backend.open(&mut thread, resumed)?;
        let id = thread.id.clone();
        let model = thread.model.clone().unwrap_or_default();
        let effort = thread.effort.clone();
        lock(&self.state).thread = Some(thread);
        if resumed {
            let snapshot = self.backend.wait(&id)?;
            let mut state = lock(&self.state);
            for message in snapshot.messages {
                if let Some(message_id) = optional_string(message.get("id")) {
                    state.emitted_messages.insert(message_id);
                }
            }
        }
        Ok(json!({ "thread": { "id": id }, "model": model, "reasoningEffort": effort }))
    }

    fn start_turn(self: &Arc<Self>, params: &Value) -> AResult<Value> {
        let thread_id = {
            let state = lock(&self.state);
            let Some(thread) = &state.thread else {
                return Err(AdapterError::invalid("thread/start or thread/resume must run first"));
            };
            if state.turn.is_some() {
                return Err(AdapterError::invalid("a turn is already active"));
            }
            thread.id.clone()
        };
        let input = record(params, "turn parameters")?;
        require_thread(&thread_id, input)?;
        let effort = optional_string(input.get("effort"));
        let id = self
            .backend
            .prompt(&thread_id, &read_text_input(input.get("input"))?, false, effort.as_deref())?;
        let mut state = lock(&self.state);
        state.turn_serial += 1;
        let serial = state.turn_serial;
        state.turn = Some(Turn {
            serial,
            id: id.clone(),
            interrupted: false,
            settling: false,
            steer_generation: 0,
            pending_steers: 0,
        });
        self.emit.emit(json!({
            "method": "turn/started",
            "params": { "threadId": thread_id, "turn": { "id": id, "status": "inProgress" } },
        }));
        let done = crate::protocol::Latch::new();
        state.completion = Some(done.clone());
        let inner = self.clone();
        thread::spawn(move || {
            inner.complete_when_idle(thread_id, serial);
            done.set();
        });
        Ok(json!({ "turn": { "id": id, "status": "inProgress" } }))
    }

    fn steer_turn(&self, params: &Value) -> AResult<Value> {
        let input = record(params, "steer parameters")?;
        let (thread_id, turn_id, serial) = {
            let mut state = lock(&self.state);
            let (thread_id, turn) = require_turn(&mut state, input, "expectedTurnId")?;
            turn.pending_steers += 1;
            (thread_id, turn.id.clone(), turn.serial)
        };
        let text = read_text_input(input.get("input"));
        let sent = text.clone().and_then(|text| self.backend.prompt(&thread_id, &text, true, None));
        {
            let mut state = lock(&self.state);
            if let Some(turn) = state.turn.as_mut().filter(|turn| turn.serial == serial) {
                turn.pending_steers -= 1;
                if sent.is_ok() {
                    turn.steer_generation += 1;
                }
            }
            self.steers_settled.notify_all();
        }
        sent?;
        let text = text?;
        // Codex reports a steer as its own userMessage item, which is what
        // puts it in the transcript. OpenCode echoes nothing back.
        self.emit.emit(json!({
            "method": "item/completed",
            "params": { "threadId": thread_id, "item": { "id": uuid_v4(), "type": "userMessage", "status": "completed", "text": text } },
        }));
        Ok(json!({ "turnId": turn_id }))
    }

    fn interrupt_turn(&self, params: &Value) -> AResult<Value> {
        let input = record(params, "interrupt parameters")?;
        let thread_id = {
            let mut state = lock(&self.state);
            let (thread_id, turn) = require_turn(&mut state, input, "turnId")?;
            turn.interrupted = true;
            thread_id
        };
        self.backend.interrupt(&thread_id)?;
        Ok(json!({}))
    }

    fn complete_when_idle(&self, thread_id: String, serial: u64) {
        let snapshot = loop {
            let snapshot = match self.backend.wait(&thread_id) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    let mut state = lock(&self.state);
                    let status = match state.turn.as_ref().filter(|turn| turn.serial == serial) {
                        Some(turn) if turn.interrupted => "interrupted",
                        Some(_) => "failed",
                        None => return,
                    };
                    self.finish_turn(&mut state, &thread_id, status, Some(error.message));
                    return;
                }
            };
            let mut state = lock(&self.state);
            let Some(turn) = state.turn.as_ref().filter(|turn| turn.serial == serial) else {
                return;
            };
            if turn.pending_steers > 0 {
                // A steer is in flight. Wait for it; if it was accepted, the
                // session goes busy again and the next idle snapshot counts.
                let generation = turn.steer_generation;
                loop {
                    match state.turn.as_ref().filter(|turn| turn.serial == serial) {
                        None => return,
                        Some(turn) if turn.pending_steers == 0 => break,
                        Some(_) => state = self.steers_settled.wait(state).unwrap_or_else(|e| e.into_inner()),
                    }
                }
                if state.turn.as_ref().is_some_and(|turn| turn.steer_generation != generation) {
                    continue;
                }
            }
            state.turn.as_mut().expect("turn is current").settling = true;
            break snapshot;
        };
        let mut state = lock(&self.state);
        self.emit_snapshot(&mut state, &thread_id, &snapshot);
        let interrupted = state.turn.as_ref().is_some_and(|turn| turn.interrupted);
        let status = if interrupted {
            "interrupted"
        } else {
            match snapshot.outcome.as_deref() {
                Some("failed") => "failed",
                Some("interrupted") => "interrupted",
                _ => "completed",
            }
        };
        let message = (status == "failed").then(|| "OpenCode session failed".to_string());
        self.finish_turn(&mut state, &thread_id, status, message);
    }

    fn emit_snapshot(&self, state: &mut State, thread_id: &str, snapshot: &Snapshot) {
        for message in &snapshot.messages {
            if message.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let id = optional_string(message.get("id")).unwrap_or_else(uuid_v4);
            if !state.emitted_messages.insert(id.clone()) {
                continue;
            }
            let parts: Vec<&Map<String, Value>> = message
                .get("content")
                .and_then(Value::as_array)
                .map(|parts| parts.iter().filter_map(Value::as_object).collect())
                .unwrap_or_default();
            for part in &parts {
                let kind = part.get("type").and_then(Value::as_str);
                if kind == Some("reasoning")
                    && let Some(text) = part.get("text").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty())
                {
                    self.emit.emit(json!({
                        "method": "item/completed",
                        "params": { "threadId": thread_id, "item": {
                            "id": format!("{id}-reasoning"),
                            "type": "reasoning",
                            "status": "completed",
                            "summary": [{ "type": "summary_text", "text": text }],
                        } },
                    }));
                }
                if kind == Some("tool") {
                    self.emit_tool(thread_id, part);
                }
            }
            let text = parts
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            let text = text.trim();
            if !text.is_empty() {
                self.emit.emit(json!({
                    "method": "item/completed",
                    "params": { "threadId": thread_id,
                        "item": { "id": id, "type": "agentMessage", "status": "completed", "text": text, "phase": "final_answer" } },
                }));
            }
        }
        if snapshot.tokens.is_some() || snapshot.cost != 0.0 {
            let tokens = snapshot.tokens.clone().unwrap_or_default();
            let cache_read = number(tokens.get("cache").and_then(|cache| cache.get("read")));
            let input = number(tokens.get("input")) + cache_read;
            let output = number(tokens.get("output")) + number(tokens.get("reasoning"));
            let mut token_usage = json!({ "total": {
                "inputTokens": num(input),
                "cachedInputTokens": num(cache_read),
                "outputTokens": num(output),
                "totalTokens": num(input + output),
            } });
            if snapshot.context_window != 0.0 {
                token_usage["modelContextWindow"] = num(snapshot.context_window);
            }
            self.emit.emit(json!({
                "method": "thread/tokenUsage/updated",
                "params": { "threadId": thread_id, "tokenUsage": token_usage, "costUsd": num(snapshot.cost) },
            }));
        }
    }

    fn emit_tool(&self, thread_id: &str, part: &Map<String, Value>) {
        let state = part.get("state").and_then(Value::as_object).cloned().unwrap_or_default();
        let tool_id = optional_string(part.get("id")).unwrap_or_else(uuid_v4);
        let name = optional_string(part.get("name")).unwrap_or_else(|| "tool".into());
        let input = state.get("input").and_then(Value::as_object).cloned().unwrap_or_default();
        let output_source = state.get("output").filter(|v| !v.is_null()).or_else(|| state.get("error"));
        let output = text_content(output_source, true);
        let failed = state.get("status").and_then(Value::as_str) == Some("error");
        let command = tool_command(&name, &input);
        self.emit.emit(json!({
            "method": "item/started",
            "params": { "threadId": thread_id, "item": {
                "id": tool_id, "type": "toolCall", "status": "inProgress", "toolName": name, "command": command, "input": input,
            } },
        }));
        self.emit.emit(json!({
            "method": "item/completed",
            "params": { "threadId": thread_id, "item": {
                "id": tool_id, "type": "toolCall", "status": if failed { "failed" } else { "completed" },
                "toolName": name, "command": command, "input": input, "output": output,
            } },
        }));
    }

    fn finish_turn(&self, state: &mut State, thread_id: &str, status: &str, message: Option<String>) {
        let Some(turn) = state.turn.take() else { return };
        self.steers_settled.notify_all();
        let mut turn_json = json!({ "id": turn.id, "status": status });
        if let Some(message) = message.filter(|m| !m.is_empty()) {
            turn_json["error"] = json!({ "message": message });
        }
        self.emit
            .emit(json!({ "method": "turn/completed", "params": { "threadId": thread_id, "turn": turn_json } }));
    }
}

fn require_thread(thread_id: &str, input: &Map<String, Value>) -> AResult<()> {
    if required_string(input.get("threadId"), "threadId")? != thread_id {
        return Err(AdapterError::invalid("threadId does not match the configured OpenCode session"));
    }
    Ok(())
}

fn require_turn<'a>(state: &'a mut State, input: &Map<String, Value>, turn_key: &str) -> AResult<(String, &'a mut Turn)> {
    let thread_id = state.thread.as_ref().map(|t| t.id.clone()).unwrap_or_default();
    require_thread(&thread_id, input)?;
    let Some(turn) = state.turn.as_mut() else {
        return Err(AdapterError::invalid("there is no active OpenCode turn"));
    };
    if turn.settling {
        return Err(AdapterError::invalid("the active OpenCode turn is settling"));
    }
    if required_string(input.get(turn_key), turn_key)? != turn.id {
        return Err(AdapterError::invalid(format!("{turn_key} does not match the active OpenCode turn")));
    }
    Ok((thread_id, turn))
}

fn tool_command(name: &str, input: &Map<String, Value>) -> String {
    optional_string(input.get("command")).unwrap_or_else(|| format!("{name} {}", crate::protocol::compact(input)))
}

// The HTTP backend.

/// A failed API call. `status` is set for an HTTP error response.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpError {
    pub status: Option<u16>,
    pub message: String,
}

impl HttpError {
    fn other(message: impl Into<String>) -> HttpError {
        HttpError {
            status: None,
            message: message.into(),
        }
    }
}

impl From<HttpError> for AdapterError {
    fn from(error: HttpError) -> Self {
        AdapterError::failed(error.message)
    }
}

#[derive(Default)]
struct Server {
    process: Option<Arc<ChildProcess>>,
    base_url: Option<String>,
    password: String,
}

struct Pending {
    stream: TcpStream,
    aborted: Arc<AtomicBool>,
}

/// Talks to `opencode2 serve --stdio` over HTTP on loopback.
pub struct HttpBackend {
    timeout: Duration,
    server: Mutex<Server>,
    pending: Mutex<HashMap<u64, Pending>>,
    next_request: AtomicU64,
}

impl HttpBackend {
    pub fn new(timeout: Duration) -> HttpBackend {
        HttpBackend {
            timeout,
            server: Mutex::new(Server::default()),
            pending: Mutex::new(HashMap::new()),
            next_request: AtomicU64::new(0),
        }
    }

    /// Points the backend at a running server without starting one.
    #[cfg(test)]
    pub fn attach(&self, base_url: &str, password: &str) {
        let mut server = lock(&self.server);
        server.base_url = Some(base_url.into());
        server.password = password.into();
    }

    fn start_server(&self, executable: &str, cwd: &str, sandbox: Option<&str>) -> AResult<()> {
        let password = format!("{}{}", uuid_v4(), uuid_v4());
        let existing = std::env::var("OPENCODE_CONFIG_CONTENT").ok().filter(|text| !text.is_empty());
        let config = ruddr_config_content(sandbox, existing.as_deref())?;
        let mut command = Command::new(executable);
        command
            .args(["serve", "--stdio"])
            .current_dir(cwd)
            .env("OPENCODE_CONFIG_CONTENT", config)
            .env("OPENCODE_SERVER_PASSWORD", &password);
        let (process, stdout) = ChildProcess::spawn(command).map_err(|error| AdapterError::failed(spawn_error(executable, &error)))?;
        {
            let mut server = lock(&self.server);
            server.process = Some(process);
            server.password = password;
        }
        let (lines, announced) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = LineReader::new(stdout, MAX_LINE_BYTES);
            let first = reader.next_line();
            let _ = lines.send(first);
            // Keep draining so the server never blocks on a full stdout pipe.
            while let Ok(Some(_)) = reader.next_line() {}
        });
        let first = match announced.recv_timeout(ANNOUNCE_TIMEOUT) {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(AdapterError::failed("OpenCode 2 server exited before reporting its URL"));
            }
            Ok(Err(error)) => return Err(AdapterError::failed(error.to_string())),
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(AdapterError::failed("OpenCode 2 server did not report its URL")),
        };
        let info: Value = serde_json::from_str(&first).map_err(|error| AdapterError::failed(error.to_string()))?;
        let info = record(&info, "OpenCode server announcement")?;
        let url = validated_loopback_url(&required_string(info.get("url"), "OpenCode server URL")?)?;
        lock(&self.server).base_url = Some(url);
        Ok(())
    }

    // OpenCode 2.0.15 moved these routes under /api/experimental; earlier 2.0
    // releases serve them under /api/session.
    fn request_session_route(&self, session: &str, route: &str, method: &str, timeout: Option<Duration>) -> Result<Value, HttpError> {
        let session = encode_uri_component(session);
        match self.request(method, &format!("/api/experimental/session/{session}/{route}"), None, timeout) {
            Err(error) if error.status == Some(404) => self.request(method, &format!("/api/session/{session}/{route}"), None, timeout),
            other => other,
        }
    }

    /// One HTTP request. `timeout` of `None` waits as long as the server
    /// takes; `close` still aborts it.
    pub fn request(&self, method: &str, path: &str, body: Option<&Value>, timeout: Option<Duration>) -> Result<Value, HttpError> {
        let (base, password) = {
            let server = lock(&self.server);
            (server.base_url.clone(), server.password.clone())
        };
        let base = base.ok_or_else(|| HttpError::other("OpenCode 2 server is not running"))?;
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let timed_out = || {
            HttpError::other(format!(
                "OpenCode API request timed out after {}ms",
                timeout.unwrap_or_default().as_millis()
            ))
        };
        let (host, port) = split_origin(&base)?;
        let connect_host = host.trim_start_matches('[').trim_end_matches(']').to_string();
        let address = (connect_host.as_str(), port)
            .to_socket_addrs()
            .map_err(|error| HttpError::other(format!("OpenCode API connect failed: {error}")))?
            .next()
            .ok_or_else(|| HttpError::other("OpenCode API host has no address"))?;
        let connect_budget = deadline.map_or(DEFAULT_HTTP_TIMEOUT, |d| d.saturating_duration_since(Instant::now()));
        let stream = TcpStream::connect_timeout(&address, connect_budget.max(Duration::from_millis(1))).map_err(|error| {
            if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) {
                timed_out()
            } else {
                HttpError::other(format!("OpenCode API connect failed: {error}"))
            }
        })?;
        let aborted = Arc::new(AtomicBool::new(false));
        let key = self.next_request.fetch_add(1, Ordering::Relaxed);
        if let Ok(clone) = stream.try_clone() {
            lock(&self.pending).insert(
                key,
                Pending {
                    stream: clone,
                    aborted: aborted.clone(),
                },
            );
        }
        let outcome = exchange(stream, method, &host, port, path, body, &password, deadline);
        lock(&self.pending).remove(&key);
        if aborted.load(Ordering::SeqCst) {
            return Err(HttpError::other("OpenCode backend is closing"));
        }
        let (status, reason, body) = outcome.map_err(|error| match error {
            Exchange::TimedOut => timed_out(),
            Exchange::Io(message) => HttpError::other(message),
        })?;
        if !(200..300).contains(&status) {
            let detail = String::from_utf8_lossy(&body).trim().to_string();
            let detail = if detail.is_empty() { reason } else { detail };
            return Err(HttpError {
                status: Some(status),
                message: format!("OpenCode API {status}: {detail}"),
            });
        }
        if status == 204 {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&body).map_err(|error| HttpError::other(format!("OpenCode API returned invalid JSON: {error}")))
    }

    fn switch_model(&self, session: &str, model: &Value) -> AResult<()> {
        self.request(
            "POST",
            &format!("/api/session/{}/model", encode_uri_component(session)),
            Some(&json!({ "model": model })),
            Some(self.timeout),
        )?;
        Ok(())
    }

    fn abort_pending(&self) {
        for (_, pending) in lock(&self.pending).drain() {
            pending.aborted.store(true, Ordering::SeqCst);
            let _ = pending.stream.shutdown(Shutdown::Both);
        }
    }
}

impl Backend for HttpBackend {
    fn open(&self, thread: &mut OpenCodeThread, resumed: bool) -> AResult<String> {
        let agent = ruddr_agent(thread.sandbox.as_deref());
        self.start_server(&thread.executable, &thread.cwd, thread.sandbox.as_deref())?;
        let session = encode_uri_component(&thread.id);
        let result = if resumed {
            let result = self.request("GET", &format!("/api/session/{session}"), None, Some(self.timeout))?;
            self.request(
                "POST",
                &format!("/api/session/{session}/agent"),
                Some(&json!({ "agent": agent })),
                Some(self.timeout),
            )?;
            result
        } else {
            let mut body = json!({ "location": { "directory": thread.cwd }, "agent": agent });
            if let Some(model) = &thread.model {
                body["model"] = parse_model(model)?;
            }
            self.request("POST", "/api/session", Some(&body), Some(self.timeout))?
        };
        let data = record(result.get("data").unwrap_or(&Value::Null), "session data")?;
        let id = required_string(data.get("id"), "session id")?;
        let model = if resumed && let Some(model) = &thread.model {
            let model = parse_model(model)?;
            self.switch_model(&id, &model)?;
            model
        } else {
            data.get("model").cloned().unwrap_or(Value::Null)
        };
        thread.model = if model.is_null() {
            None
        } else {
            Some(format!(
                "{}/{}",
                required_string(model.get("providerID"), "model providerID")?,
                required_string(model.get("id"), "model id")?
            ))
        };
        thread.effort = optional_string(model.get("variant"));
        Ok(id)
    }

    fn prompt(&self, session: &str, text: &str, steer: bool, effort: Option<&str>) -> AResult<String> {
        if let Some(effort) = effort {
            let result = self.request(
                "GET",
                &format!("/api/session/{}", encode_uri_component(session)),
                None,
                Some(self.timeout),
            )?;
            let mut model = result["data"]["model"].clone();
            if !model.is_object() {
                return Err(AdapterError::invalid(
                    "OpenCode effort requires a session model; pass --model provider/model",
                ));
            }
            model["variant"] = json!(effort);
            self.switch_model(session, &model)?;
        }
        let mut body = json!({ "text": text });
        if steer {
            body["delivery"] = json!("steer");
        }
        let path = format!("/api/session/{}/prompt", encode_uri_component(session));
        let result = self.request("POST", &path, Some(&body), Some(self.timeout))?;
        let result = record(&result, "prompt response")?;
        let data = record(result.get("data").unwrap_or(&Value::Null), "prompt data")?;
        required_string(data.get("id"), "prompt id")
    }

    fn wait(&self, session: &str) -> AResult<Snapshot> {
        self.request_session_route(session, "wait", "POST", None)?;
        let result = self.request_session_route(session, "export", "GET", Some(self.timeout))?;
        let result = record(&result, "export response")?;
        let data = record(result.get("data").unwrap_or(&Value::Null), "export data")?;
        let info = record(data.get("info").unwrap_or(&Value::Null), "session info")?;
        Ok(Snapshot {
            outcome: optional_string(info.get("outcome")),
            messages: data
                .get("messages")
                .and_then(Value::as_array)
                .map(|m| m.iter().filter_map(Value::as_object).cloned().collect())
                .unwrap_or_default(),
            tokens: info.get("tokens").and_then(Value::as_object).cloned(),
            cost: number(info.get("cost")),
            context_window: 0.0,
        })
    }

    fn interrupt(&self, session: &str) -> AResult<()> {
        self.request(
            "POST",
            &format!("/api/session/{}/interrupt", encode_uri_component(session)),
            None,
            Some(self.timeout),
        )?;
        Ok(())
    }

    fn close(&self, session: Option<&str>, remove_session: bool) {
        self.abort_pending();
        let running = lock(&self.server).base_url.is_some();
        if remove_session
            && running
            && let Some(session) = session
        {
            let path = format!("/api/session/{}", encode_uri_component(session));
            let _ = self.request("DELETE", &path, None, Some(Duration::from_secs(1)));
        }
        self.abort_pending();
        let process = {
            let mut server = lock(&self.server);
            server.base_url = None;
            server.password.clear();
            server.process.take()
        };
        if let Some(process) = process {
            process.shut_down(Duration::from_secs(1));
        }
    }
}

enum Exchange {
    TimedOut,
    Io(String),
}

#[allow(clippy::too_many_arguments)]
fn exchange(
    stream: TcpStream,
    method: &str,
    host: &str,
    port: u16,
    path: &str,
    body: Option<&Value>,
    password: &str,
    deadline: Option<Instant>,
) -> Result<(u16, String, Vec<u8>), Exchange> {
    let io = |error: std::io::Error| match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => Exchange::TimedOut,
        _ => Exchange::Io(format!("OpenCode API request failed: {error}")),
    };
    let remaining = || -> Result<Option<Duration>, Exchange> {
        match deadline {
            None => Ok(None),
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() { Err(Exchange::TimedOut) } else { Ok(Some(left)) }
            }
        }
    };
    let payload = body.map(|body| serde_json::to_vec(body).unwrap_or_default()).unwrap_or_default();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nAuthorization: Basic {}\r\nAccept: application/json\r\nConnection: close\r\n",
        base64(format!("opencode:{password}").as_bytes())
    );
    if body.is_some() {
        head.push_str("Content-Type: application/json\r\n");
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n", payload.len()));
    let mut writer = stream.try_clone().map_err(io)?;
    for bytes in [head.as_bytes(), &payload] {
        let mut offset = 0;
        while offset < bytes.len() {
            writer.set_write_timeout(remaining()?).map_err(io)?;
            let written = writer.write(&bytes[offset..bytes.len().min(offset + 8192)]).map_err(io)?;
            if written == 0 {
                return Err(Exchange::Io("OpenCode API closed the connection during write".into()));
            }
            offset += written;
        }
    }

    let mut reader = BufReader::new(stream);
    let read_line = |reader: &mut BufReader<TcpStream>| -> Result<String, Exchange> {
        let mut line = Vec::new();
        loop {
            reader.get_ref().set_read_timeout(remaining()?).map_err(io)?;
            let available = reader.fill_buf().map_err(io)?;
            if available.is_empty() {
                break;
            }
            let count = available.iter().position(|b| *b == b'\n').map_or(available.len(), |i| i + 1);
            if count > MAX_HTTP_HEADER_BYTES.saturating_sub(line.len()) {
                return Err(Exchange::Io("OpenCode API header exceeds 64 KiB".into()));
            }
            let complete = available[count - 1] == b'\n';
            line.extend_from_slice(&available[..count]);
            reader.consume(count);
            if complete {
                break;
            }
        }
        if line.is_empty() {
            return Err(Exchange::Io("OpenCode API closed the connection".into()));
        }
        Ok(String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_string())
    };
    let status_line = read_line(&mut reader)?;
    let mut parts = status_line.splitn(3, ' ');
    let _version = parts.next();
    let status: u16 = parts
        .next()
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| Exchange::Io(format!("invalid HTTP status line {status_line:?}")))?;
    let reason = parts.next().unwrap_or_default().to_string();
    let mut length = None;
    let mut chunked = false;
    let mut header_bytes = status_line.len() + 2;
    loop {
        let header = read_line(&mut reader)?;
        header_bytes += header.len() + 2;
        if header_bytes > MAX_HTTP_HEADER_BYTES {
            return Err(Exchange::Io("OpenCode API headers exceed 64 KiB".into()));
        }
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => {
                    length = Some(
                        value
                            .trim()
                            .parse::<usize>()
                            .map_err(|_| Exchange::Io("invalid content length".into()))?,
                    );
                }
                "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
                _ => {}
            }
        }
    }
    let mut body = Vec::new();
    let read_exact = |reader: &mut BufReader<TcpStream>, count: usize, body: &mut Vec<u8>| -> Result<(), Exchange> {
        let start = body.len();
        let end = start
            .checked_add(count)
            .filter(|end| *end <= MAX_HTTP_BODY_BYTES)
            .ok_or_else(|| Exchange::Io("OpenCode API body exceeds 64 MiB".into()))?;
        body.resize(end, 0);
        let mut filled = 0;
        while filled < count {
            reader.get_ref().set_read_timeout(remaining()?).map_err(io)?;
            let read = reader.read(&mut body[start + filled..]).map_err(io)?;
            if read == 0 {
                return Err(Exchange::Io("OpenCode API closed the connection mid-body".into()));
            }
            filled += read;
        }
        Ok(())
    };
    if status == 204 || status == 304 || method == "HEAD" {
        // No body.
    } else if chunked {
        loop {
            let size_line = read_line(&mut reader)?;
            let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| Exchange::Io("invalid chunk size".into()))?;
            if size == 0 {
                // Trailers end with an empty line.
                while !read_line(&mut reader)?.is_empty() {}
                break;
            }
            read_exact(&mut reader, size, &mut body)?;
            read_line(&mut reader)?;
        }
    } else if let Some(length) = length {
        read_exact(&mut reader, length, &mut body)?;
    } else {
        loop {
            reader.get_ref().set_read_timeout(remaining()?).map_err(io)?;
            let mut chunk = [0u8; 8192];
            let read = reader.read(&mut chunk).map_err(io)?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
            if body.len() > MAX_HTTP_BODY_BYTES {
                return Err(Exchange::Io("OpenCode API body exceeds 64 MiB".into()));
            }
        }
    }
    Ok((status, reason, body))
}

fn spawn_error(executable: &str, error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        format!("OpenCode 2 executable not found at {executable}; install opencode2 or pass --opencode-path")
    } else {
        format!("start OpenCode 2 server: {error}")
    }
}

/// Splits `http://host:port` into host (brackets kept for IPv6) and port.
fn split_origin(origin: &str) -> Result<(String, u16), HttpError> {
    let (scheme, authority) = origin
        .split_once("://")
        .ok_or_else(|| HttpError::other("invalid OpenCode server URL"))?;
    if scheme != "http" {
        return Err(HttpError::other(
            "Ruddr's HTTP client speaks plain HTTP to the loopback OpenCode server only",
        ));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !port.contains(']') => (host.to_string(), port.parse().map_err(|_| HttpError::other("invalid port"))?),
        _ => (authority.to_string(), 80),
    };
    Ok((host, port))
}

/// Accepts only an HTTP(S) URL on a loopback host with no credentials,
/// path, query, or fragment, and returns its origin.
pub fn validated_loopback_url(value: &str) -> AResult<String> {
    let invalid = || AdapterError::invalid("OpenCode server URL must be a valid URL");
    let (scheme, rest) = value.split_once("://").ok_or_else(invalid)?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme.is_empty() || !scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) {
        return Err(invalid());
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let (credentials, host_port) = match authority.rsplit_once('@') {
        Some((credentials, host_port)) => (Some(credentials), host_port),
        None => (None, authority),
    };
    let (host, port) = if let Some(after) = host_port.strip_prefix('[') {
        let close = after.find(']').ok_or_else(invalid)?;
        let host = format!("[{}]", &after[..close]);
        let port = after[close + 1..].strip_prefix(':').map(str::to_string);
        if !after[close + 1..].is_empty() && port.is_none() {
            return Err(invalid());
        }
        (host, port)
    } else {
        match host_port.split_once(':') {
            Some((host, port)) => (host.to_ascii_lowercase(), Some(port.to_string())),
            None => (host_port.to_ascii_lowercase(), None),
        }
    };
    if host.is_empty() {
        return Err(invalid());
    }
    let port = match port.as_deref() {
        None | Some("") => None,
        Some(port) => Some(port.parse::<u16>().map_err(|_| invalid())?),
    };
    let loopback = matches!(host.as_str(), "127.0.0.1" | "[::1]" | "localhost");
    if !(scheme == "http" || scheme == "https") || !loopback {
        return Err(AdapterError::invalid("OpenCode server URL must use HTTP on a loopback host"));
    }
    let has_path = !tail.is_empty() && tail != "/";
    if credentials.is_some_and(|c| !c.is_empty()) || has_path {
        return Err(AdapterError::invalid(
            "OpenCode server URL must not contain credentials, a path, query, or fragment",
        ));
    }
    let default_port = if scheme == "http" { 80 } else { 443 };
    Ok(match port {
        Some(port) if port != default_port => format!("{scheme}://{host}:{port}"),
        _ => format!("{scheme}://{host}"),
    })
}

fn ruddr_agent(sandbox: Option<&str>) -> &'static str {
    match sandbox.unwrap_or("workspace-write") {
        "read-only" => "ruddr-read-only",
        "danger-full-access" => "ruddr-danger-full-access",
        _ => "ruddr-workspace-write",
    }
}

/// The inline OpenCode config Ruddr passes: the caller's own
/// `OPENCODE_CONFIG_CONTENT` plus one agent per sandbox, with the run's
/// sandbox as the default agent.
pub fn ruddr_config_content(sandbox: Option<&str>, existing: Option<&str>) -> AResult<String> {
    // TODO(review): Define how Ruddr should isolate inherited OpenCode plugins and agents before changing config precedence.
    let mut configured = match existing {
        Some(text) => {
            let value: Value = serde_json::from_str(text).map_err(|error| AdapterError::failed(error.to_string()))?;
            record(&value, "OPENCODE_CONFIG_CONTENT")?.clone()
        }
        None => Map::new(),
    };
    let mut agents = configured.get("agents").and_then(Value::as_object).cloned().unwrap_or_default();
    let allow_all = json!({ "action": "*", "resource": "*", "effect": "allow" });
    let mut read_only = vec![json!({ "action": "*", "resource": "*", "effect": "deny" })];
    read_only.extend(
        ["read", "glob", "grep", "lsp", "webfetch", "websearch"]
            .map(|action| json!({ "action": action, "resource": "*", "effect": "allow" })),
    );
    agents.insert(
        "ruddr-read-only".into(),
        json!({ "description": "Ruddr read-only agent", "mode": "primary", "permissions": read_only }),
    );
    agents.insert(
        "ruddr-workspace-write".into(),
        json!({ "description": "Ruddr workspace-write agent", "mode": "primary", "permissions": [
            allow_all,
            { "action": "external_directory", "resource": "*", "effect": "deny" },
            { "action": "read", "resource": "*.env", "effect": "deny" },
            { "action": "read", "resource": "*.env.*", "effect": "deny" },
            { "action": "read", "resource": "*.env.example", "effect": "allow" },
        ] }),
    );
    agents.insert(
        "ruddr-danger-full-access".into(),
        json!({ "description": "Ruddr unrestricted agent", "mode": "primary", "permissions": [allow_all] }),
    );
    configured.insert("default_agent".into(), json!(ruddr_agent(sandbox)));
    configured.insert("agents".into(), Value::Object(agents));
    Ok(Value::Object(configured).to_string())
}

fn parse_model(model: &str) -> AResult<Value> {
    let (model, variant) = model
        .split_once('#')
        .map_or((model, None), |(model, variant)| (model, Some(variant)));
    match model.find('/') {
        Some(slash) if slash > 0 && slash < model.len() - 1 => {
            let mut result = json!({ "providerID": &model[..slash], "id": &model[slash + 1..] });
            if let Some(variant) = variant {
                if variant.is_empty() {
                    return Err(AdapterError::invalid("OpenCode model variant must not be empty"));
                }
                result["variant"] = json!(variant);
            }
            Ok(result)
        }
        _ => Err(AdapterError::invalid("OpenCode models must use provider/model syntax")),
    }
}

/// `encodeURIComponent`.
pub fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Standard base64 with padding, for the Basic authorization header.
pub fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8) | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests;
