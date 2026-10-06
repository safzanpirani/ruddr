//! The ACP adapter: Hermes Agent (`hermes acp`) and OpenClaw (`openclaw acp`,
//! a bridge to the running Gateway) speak the Agent Client Protocol,
//! newline-delimited JSON-RPC 2.0 on stdio.
//!
//! A thread is an ACP session (`session/new`, or `session/load` to resume).
//! A turn is one `session/prompt` request; its response, with a
//! `stopReason`, ends the turn, and `session/update` notifications stream the
//! reply, reasoning, and tool calls in between. An interrupt is the
//! `session/cancel` notification. ACP has no steer, so only Hermes steers:
//! its ACP server takes `/steer <text>` as a prompt while a turn runs and
//! answers with a `⏩ Steer queued` message. OpenClaw rejects steers.
//!
//! The client declares no file-system or terminal capabilities, so the agent
//! uses its own tools, and every permission request is answered `cancelled`:
//! Ruddr has no interactive approval surface. The sandbox picks the agent's
//! session mode where it offers one: Hermes runs `workspace-write` in
//! `accept_edits` and `danger-full-access` in `dont_ask`. The model
//! [`AGENT_DEFAULT_MODEL`] keeps the agent's own configured model.

use crate::protocol::{
    AResult, Adapter, AdapterError, Emit, compact, lock, num, optional_string, read_text_input, record, required_string, uuid_v4,
};
use crate::rpc::{EventFn, Incoming, Labels, RpcProcess};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(60);
// Starting the agent and loading a long session replays its history.
const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(180);
// A prompt request lasts the whole turn; the runner's turn timeout is the
// real bound, this only keeps the call finite.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(48 * 3600);
const STEER_ACCEPTED: &str = "⏩ Steer queued";
/// The catalog model that leaves the agent's configured model in place.
pub const AGENT_DEFAULT_MODEL: &str = ruddr_core::models::AGENT_DEFAULT_MODEL;
const STEER_REPLIES: [&str; 3] = [STEER_ACCEPTED, "⚠️ Steer failed", "No active turn"];

/// Which ACP agent the adapter drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Hermes,
    OpenClaw,
}

impl Flavor {
    pub fn name(self) -> &'static str {
        match self {
            Flavor::Hermes => "Hermes",
            Flavor::OpenClaw => "OpenClaw",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Flavor::Hermes => "hermes",
            Flavor::OpenClaw => "openclaw",
        }
    }

    fn labels(self) -> Labels {
        match self {
            Flavor::Hermes => Labels {
                prefix: "Hermes ACP",
                process: "Hermes ACP process",
                client: "Hermes ACP client",
                message: "Hermes ACP message",
            },
            Flavor::OpenClaw => Labels {
                prefix: "OpenClaw ACP",
                process: "OpenClaw ACP process",
                client: "OpenClaw ACP client",
                message: "OpenClaw ACP message",
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AcpThread {
    pub flavor: Flavor,
    pub id: String,
    pub cwd: String,
    pub model: Option<String>,
    pub executable: String,
    pub sandbox: String,
    pub resumed: bool,
}

impl AcpThread {
    /// The ACP session mode this sandbox runs in, when the agent has one.
    pub fn mode(&self) -> Option<&'static str> {
        match (self.flavor, self.sandbox.as_str()) {
            (Flavor::Hermes, "workspace-write") => Some("accept_edits"),
            (Flavor::Hermes, "danger-full-access") => Some("dont_ask"),
            _ => None,
        }
    }
}

pub trait AcpClient: Send + Sync {
    /// Starts the agent, opens or loads the session, and returns its ID.
    fn start(&self, config: &AcpThread, on_event: EventFn) -> AResult<String>;
    /// Sends one request and returns its `result`.
    fn request(&self, method: &str, params: Value, timeout: Option<Duration>) -> AResult<Value>;
    fn notify(&self, method: &str, params: Value) -> AResult<()>;
    fn close(&self);
}

#[derive(Default)]
struct Turn {
    serial: u64,
    id: String,
    interrupted: bool,
    /// The assistant text since the last tool call, and its item ID.
    message: String,
    message_id: String,
    reasoning: String,
    messages: u32,
    /// A `/steer` request is in flight; its reply is not part of the answer.
    steering: bool,
    steer_reply: Option<String>,
}

#[derive(Default)]
struct State {
    initialized: bool,
    closed: bool,
    thread: Option<AcpThread>,
    turn: Option<Turn>,
    turn_serial: u64,
    tools: HashMap<String, Map<String, Value>>,
}

struct Inner {
    flavor: Flavor,
    emit: Emit,
    client: Box<dyn AcpClient>,
    executable: String,
    state: Mutex<State>,
}

pub struct AcpAdapter {
    inner: Arc<Inner>,
}

impl AcpAdapter {
    pub fn new(flavor: Flavor, emit: Emit, executable: String) -> AcpAdapter {
        let client = SubprocessAcpClient::new(flavor, DEFAULT_RPC_TIMEOUT, DEFAULT_START_TIMEOUT);
        AcpAdapter::with_client(flavor, emit, executable, Box::new(client))
    }

    pub fn with_client(flavor: Flavor, emit: Emit, executable: String, client: Box<dyn AcpClient>) -> AcpAdapter {
        AcpAdapter {
            inner: Arc::new(Inner {
                flavor,
                emit,
                client,
                executable,
                state: Mutex::new(State::default()),
            }),
        }
    }
}

impl Adapter for AcpAdapter {
    fn dispatch(&self, method: &str, params: &Value) -> AResult<Value> {
        let inner = &self.inner;
        match method {
            "initialize" => {
                lock(&inner.state).initialized = true;
                Ok(json!({
                    "serverInfo": { "name": format!("ruddr-{}-adapter", inner.flavor.slug()), "version": "1" },
                    "capabilities": { "experimentalApi": true },
                }))
            }
            "initialized" => Ok(Value::Null),
            "thread/start" => Inner::acquire_thread(inner, params, false),
            "thread/resume" => Inner::acquire_thread(inner, params, true),
            "turn/start" => Inner::start_turn(inner, params),
            "turn/steer" => inner.steer_turn(params),
            "turn/interrupt" => inner.interrupt_turn(params),
            _ => Err(AdapterError::not_found(format!(
                "method {method} is not supported by the {} adapter",
                inner.flavor.name()
            ))),
        }
    }

    fn close(&self) {
        {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return;
            }
            state.closed = true;
        }
        self.inner.client.close();
    }
}

impl Inner {
    fn acquire_thread(self: &Arc<Self>, params: &Value, resumed: bool) -> AResult<Value> {
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
        let mut thread = AcpThread {
            flavor: self.flavor,
            id: if resumed {
                required_string(input.get("threadId"), "threadId")?
            } else {
                String::new()
            },
            cwd: required_string(input.get("cwd"), "cwd")?,
            model: optional_string(input.get("model")).filter(|model| model != AGENT_DEFAULT_MODEL),
            executable: optional_string(input.get("providerPath")).unwrap_or_else(|| self.executable.clone()),
            sandbox: optional_string(input.get("sandbox")).unwrap_or_else(|| "workspace-write".into()),
            resumed,
        };
        // Updates are handled one at a time on a worker thread, so the
        // client's reader thread never waits on the adapter.
        let (events, receiver) = mpsc::channel::<Map<String, Value>>();
        let worker = self.clone();
        thread::spawn(move || {
            for event in receiver {
                worker.handle_event(event);
            }
        });
        let on_event: EventFn = Arc::new(move |event| {
            let _ = events.send(event);
        });
        thread.id = self.client.start(&thread, on_event)?;
        let id = thread.id.clone();
        lock(&self.state).thread = Some(thread);
        Ok(json!({ "thread": { "id": id } }))
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
        require_thread(self.flavor, &thread_id, input)?;
        let text = read_text_input(input.get("input"))?;
        let (turn_id, serial) = {
            let mut state = lock(&self.state);
            state.turn_serial += 1;
            let serial = state.turn_serial;
            let id = uuid_v4();
            state.turn = Some(Turn {
                serial,
                id: id.clone(),
                message_id: format!("{id}-message-0"),
                ..Turn::default()
            });
            self.emit.emit(json!({
                "method": "turn/started",
                "params": { "threadId": thread_id, "turn": { "id": id, "status": "inProgress" } },
            }));
            (id, serial)
        };
        // The prompt request lasts the whole turn, so it runs off the
        // dispatch thread and its response completes the turn.
        let worker = self.clone();
        let session = thread_id.clone();
        thread::spawn(move || {
            let prompt = json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] });
            let outcome = worker.client.request("session/prompt", prompt, Some(PROMPT_TIMEOUT));
            worker.complete_turn(serial, outcome);
        });
        Ok(json!({ "turn": { "id": turn_id, "status": "inProgress" } }))
    }

    fn steer_turn(&self, params: &Value) -> AResult<Value> {
        let input = record(params, "steer parameters")?;
        let (thread_id, turn_id, serial) = {
            let mut state = lock(&self.state);
            let (thread_id, turn) = require_turn(self.flavor, &mut state, input, "expectedTurnId")?;
            if self.flavor != Flavor::Hermes {
                return Err(AdapterError::invalid(format!(
                    "{} runs cannot be steered; interrupt the turn and send a new prompt",
                    self.flavor.name()
                )));
            }
            if turn.steering {
                return Err(AdapterError::invalid("a steer is already in flight"));
            }
            turn.steering = true;
            turn.steer_reply = None;
            (thread_id, turn.id.clone(), turn.serial)
        };
        let text = read_text_input(input.get("input"));
        let sent = text.clone().and_then(|text| {
            let prompt = json!({ "sessionId": thread_id, "prompt": [{ "type": "text", "text": format!("/steer {text}") }] });
            self.client.request("session/prompt", prompt, None)
        });
        let reply = {
            let mut state = lock(&self.state);
            match state.turn.as_mut().filter(|turn| turn.serial == serial) {
                Some(turn) => {
                    turn.steering = false;
                    turn.steer_reply.take()
                }
                None => None,
            }
        };
        sent?;
        let text = text?;
        match reply.as_deref() {
            Some(reply) if reply.starts_with(STEER_ACCEPTED) => {}
            Some(reply) => return Err(AdapterError::invalid(format!("Hermes rejected the steer: {reply}"))),
            None => return Err(AdapterError::invalid("Hermes did not confirm the steer")),
        }
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
            let (thread_id, turn) = require_turn(self.flavor, &mut state, input, "turnId")?;
            turn.interrupted = true;
            thread_id
        };
        self.client.notify("session/cancel", json!({ "sessionId": thread_id }))?;
        Ok(json!({}))
    }

    fn handle_event(&self, event: Map<String, Value>) {
        if event.get("ruddr_error").is_some() {
            let message = optional_string(event.get("ruddr_error")).unwrap_or_default();
            let serial = lock(&self.state).turn.as_ref().map(|turn| turn.serial);
            if let Some(serial) = serial {
                self.complete_turn(serial, Err(AdapterError::failed(message)));
            }
            return;
        }
        let Some(update) = event.get("update").and_then(Value::as_object) else {
            return;
        };
        let mut guard = lock(&self.state);
        let state = &mut *guard;
        let Some(thread_id) = state.thread.as_ref().map(|t| t.id.clone()) else {
            return;
        };
        // A loaded session replays its history before any turn starts.
        let Some(turn) = state.turn.as_mut() else { return };
        match update.get("sessionUpdate").and_then(Value::as_str).unwrap_or_default() {
            "agent_message_chunk" => {
                let text = chunk_text(update);
                if turn.steering && STEER_REPLIES.iter().any(|reply| text.starts_with(reply)) {
                    turn.steer_reply = Some(text);
                    return;
                }
                if text.is_empty() {
                    return;
                }
                turn.message.push_str(&text);
                self.emit.emit(json!({
                    "method": "item/agentMessage/delta",
                    "params": { "threadId": thread_id, "turnId": turn.id, "itemId": turn.message_id, "delta": text },
                }));
            }
            "agent_thought_chunk" => turn.reasoning.push_str(&chunk_text(update)),
            "tool_call" | "tool_call_update" => {
                let Some(id) = optional_string(update.get("toolCallId")) else {
                    return;
                };
                Self::flush_message(&self.emit, &thread_id, turn, "commentary");
                let mut merged = state.tools.get(&id).cloned().unwrap_or_default();
                for (key, value) in update {
                    if !value.is_null() {
                        merged.insert(key.clone(), value.clone());
                    }
                }
                state.tools.insert(id.clone(), merged.clone());
                let status = merged.get("status").and_then(Value::as_str).unwrap_or("pending");
                let (method, status) = match status {
                    "completed" => ("item/completed", "completed"),
                    "failed" => ("item/completed", "failed"),
                    _ if update.get("sessionUpdate").and_then(Value::as_str) == Some("tool_call") => ("item/started", "inProgress"),
                    _ => ("item/updated", "inProgress"),
                };
                if method == "item/completed" {
                    state.tools.remove(&id);
                }
                self.emit
                    .emit(json!({ "method": method, "params": { "threadId": thread_id, "item": tool_item(&id, &merged, status) } }));
            }
            "usage_update" => {
                let used = update.get("used").and_then(Value::as_f64);
                let size = update.get("size").and_then(Value::as_f64);
                if let Some(used) = used.filter(|n| n.is_finite() && *n >= 0.0) {
                    let mut usage = json!({ "last": { "totalTokens": num(used) } });
                    if let Some(size) = size.filter(|n| *n > 0.0) {
                        usage["modelContextWindow"] = num(size);
                    }
                    let mut params = json!({ "threadId": thread_id, "tokenUsage": usage });
                    if let Some(cost) = update.get("cost").and_then(|c| c.get("amount")).and_then(Value::as_f64) {
                        params["costUsd"] = num(cost);
                    }
                    self.emit.emit(json!({ "method": "thread/tokenUsage/updated", "params": params }));
                }
            }
            _ => {}
        }
    }

    /// Emits the reasoning and assistant text gathered so far as completed
    /// items, and starts a new message item.
    fn flush_message(emit: &Emit, thread_id: &str, turn: &mut Turn, phase: &str) {
        let reasoning = std::mem::take(&mut turn.reasoning);
        if !reasoning.trim().is_empty() {
            emit.emit(json!({
                "method": "item/completed",
                "params": { "threadId": thread_id, "item": {
                    "id": format!("{}-reasoning-{}", turn.id, turn.messages), "type": "reasoning", "status": "completed",
                    "summary": [{ "type": "summary_text", "text": reasoning.trim() }],
                } },
            }));
        }
        let text = std::mem::take(&mut turn.message);
        if !text.trim().is_empty() {
            emit.emit(json!({
                "method": "item/completed",
                "params": { "threadId": thread_id, "item": {
                    "id": turn.message_id, "type": "agentMessage", "status": "completed", "text": text.trim(), "phase": phase,
                } },
            }));
        }
        turn.messages += 1;
        turn.message_id = format!("{}-message-{}", turn.id, turn.messages);
    }

    fn complete_turn(&self, serial: u64, outcome: AResult<Value>) {
        let mut state = lock(&self.state);
        if !state.turn.as_ref().is_some_and(|turn| turn.serial == serial) {
            return;
        }
        let thread_id = state.thread.as_ref().map(|t| t.id.clone()).unwrap_or_default();
        let mut turn = state.turn.take().expect("turn is current");
        // Unfinished tool calls end with the turn.
        let open: Vec<(String, Map<String, Value>)> = state.tools.drain().collect();
        drop(state);
        Self::flush_message(&self.emit, &thread_id, &mut turn, "final_answer");
        for (id, tool) in open {
            self.emit
                .emit(json!({ "method": "item/completed", "params": { "threadId": thread_id, "item": tool_item(&id, &tool, "failed") } }));
        }
        let (status, error) = match &outcome {
            Ok(result) => match result.get("stopReason").and_then(Value::as_str) {
                Some("cancelled") => ("interrupted", None),
                _ if turn.interrupted => ("interrupted", None),
                Some("refusal") => ("failed", Some(format!("{} refused the prompt", self.flavor.name()))),
                _ => ("completed", None),
            },
            Err(_) if turn.interrupted => ("interrupted", None),
            Err(error) => ("failed", Some(error.message.clone())),
        };
        let mut done = json!({ "id": turn.id, "status": status });
        if let Some(message) = error {
            done["error"] = json!({ "message": message });
        }
        self.emit
            .emit(json!({ "method": "turn/completed", "params": { "threadId": thread_id, "turn": done } }));
    }
}

fn chunk_text(update: &Map<String, Value>) -> String {
    match update.get("content") {
        Some(Value::Object(content)) => optional_string(content.get("text")).unwrap_or_default(),
        Some(Value::Array(items)) => items.iter().filter_map(|item| item.get("text").and_then(Value::as_str)).collect(),
        _ => String::new(),
    }
}

/// An ACP tool call as a Codex `toolCall` item. Diff content becomes the
/// input's `path`, `oldText`, and `newText`, which the dashboards render.
fn tool_item(id: &str, tool: &Map<String, Value>, status: &str) -> Value {
    let title = optional_string(tool.get("title")).unwrap_or_default();
    let kind = optional_string(tool.get("kind")).unwrap_or_else(|| "tool".into());
    let mut input = tool.get("rawInput").and_then(Value::as_object).cloned().unwrap_or_default();
    let content = tool.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut output = Vec::new();
    for item in &content {
        match item.get("type").and_then(Value::as_str) {
            Some("diff") => {
                for key in ["path", "oldText", "newText"] {
                    if let Some(value) = item.get(key) {
                        input.insert(key.into(), value.clone());
                    }
                }
            }
            Some("content") => {
                if let Some(text) = item.get("content").and_then(|c| c.get("text")).and_then(Value::as_str) {
                    output.push(text.to_string());
                }
            }
            Some("terminal") => {}
            _ => {}
        }
    }
    if output.is_empty() {
        match tool.get("rawOutput") {
            Some(Value::String(text)) => output.push(text.clone()),
            Some(Value::Null) | None => {}
            Some(other) => output.push(other.to_string()),
        }
    }
    let name = if title.is_empty() { kind.clone() } else { title.clone() };
    let command = if title.is_empty() {
        format!("{kind} {}", compact(&input))
    } else {
        title
    };
    json!({
        "id": id, "type": "toolCall", "status": status, "toolName": name, "command": command,
        "input": input, "output": output.join("\n"),
    })
}

fn require_thread(flavor: Flavor, thread_id: &str, input: &Map<String, Value>) -> AResult<()> {
    if required_string(input.get("threadId"), "threadId")? != thread_id {
        return Err(AdapterError::invalid(format!(
            "threadId does not match the configured {} session",
            flavor.name()
        )));
    }
    Ok(())
}

fn require_turn<'a>(flavor: Flavor, state: &'a mut State, input: &Map<String, Value>, turn_key: &str) -> AResult<(String, &'a mut Turn)> {
    let thread_id = state.thread.as_ref().map(|t| t.id.clone()).unwrap_or_default();
    require_thread(flavor, &thread_id, input)?;
    let name = flavor.name();
    let Some(turn) = state.turn.as_mut() else {
        return Err(AdapterError::invalid(format!("there is no active {name} turn")));
    };
    if required_string(input.get(turn_key), turn_key)? != turn.id {
        return Err(AdapterError::invalid(format!("{turn_key} does not match the active {name} turn")));
    }
    Ok((thread_id, turn))
}

/// Runs `hermes acp` or `openclaw acp` and matches responses by ID.
pub struct SubprocessAcpClient {
    flavor: Flavor,
    rpc_timeout: Duration,
    start_timeout: Duration,
    process: Mutex<Option<Arc<RpcProcess>>>,
    sequence: AtomicU64,
}

impl SubprocessAcpClient {
    pub fn new(flavor: Flavor, rpc_timeout: Duration, start_timeout: Duration) -> SubprocessAcpClient {
        SubprocessAcpClient {
            flavor,
            rpc_timeout,
            start_timeout: start_timeout.max(rpc_timeout),
            process: Mutex::new(None),
            sequence: AtomicU64::new(0),
        }
    }

    fn process(&self) -> AResult<Arc<RpcProcess>> {
        lock(&self.process)
            .clone()
            .ok_or_else(|| AdapterError::failed(format!("{} is not running", self.flavor.labels().process)))
    }
}

/// The agent's argv. Both run their ACP server on stdio.
pub fn acp_args(_config: &AcpThread) -> Vec<String> {
    vec!["acp".into()]
}

fn classify(message: &Map<String, Value>) -> Incoming {
    let id = match message.get("id") {
        Some(Value::String(id)) => Some(id.clone()),
        Some(Value::Number(id)) => Some(id.to_string()),
        _ => None,
    };
    match (message.get("method").and_then(Value::as_str), id) {
        // A response to one of the adapter's requests.
        (None, Some(id)) => {
            let outcome = match (message.get("result"), message.get("error")) {
                (_, Some(error)) if !error.is_null() => {
                    Err(optional_string(error.get("message")).unwrap_or_else(|| "ACP request failed".into()))
                }
                (Some(Value::Object(result)), _) => Ok(result.clone()),
                _ => Ok(Map::new()),
            };
            Incoming::Response(id, outcome)
        }
        // The agent asks the client something; Ruddr approves nothing and
        // declared no file-system or terminal capabilities.
        (Some(method), Some(_)) => {
            let reply = if method == "session/request_permission" {
                json!({ "jsonrpc": "2.0", "id": message.get("id"), "result": { "outcome": { "outcome": "cancelled" } } })
            } else {
                json!({ "jsonrpc": "2.0", "id": message.get("id"), "error": { "code": -32601, "message": format!("{method} is not supported by Ruddr") } })
            };
            Incoming::Reply(reply)
        }
        (Some("session/update"), None) => match message.get("params") {
            Some(Value::Object(params)) => Incoming::Event(params.clone()),
            _ => Incoming::Skip,
        },
        _ => Incoming::Skip,
    }
}

fn failure_event(message: &str) -> Map<String, Value> {
    let mut event = Map::new();
    event.insert("ruddr_error".into(), json!(message));
    event
}

impl AcpClient for SubprocessAcpClient {
    fn start(&self, config: &AcpThread, on_event: EventFn) -> AResult<String> {
        let name = self.flavor.name();
        let slug = self.flavor.slug();
        let mut command = ruddr_core::provider::command(&config.executable);
        command.args(acp_args(config)).current_dir(&config.cwd);
        let process =
            RpcProcess::spawn(command, self.flavor.labels(), self.rpc_timeout, classify, on_event, failure_event).map_err(|error| {
                AdapterError::failed(if error.kind() == std::io::ErrorKind::NotFound {
                    format!(
                        "{name} executable not found at {}; install {slug} or pass --{slug}-path",
                        config.executable
                    )
                } else {
                    format!("start {name}: {error}")
                })
            })?;
        *lock(&self.process) = Some(process);
        let init = json!({
            "protocolVersion": 1,
            "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false },
            "clientInfo": { "name": "ruddr", "version": ruddr_core::VERSION },
        });
        let info = self.request("initialize", init, Some(self.start_timeout))?;
        let (session, opened) = if config.resumed {
            let can_load = info
                .get("agentCapabilities")
                .and_then(|caps| caps.get("loadSession"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !can_load {
                return Err(AdapterError::failed(format!("{name} cannot load an existing ACP session")));
            }
            let load = json!({ "sessionId": config.id, "cwd": config.cwd, "mcpServers": [] });
            (config.id.clone(), self.request("session/load", load, Some(self.start_timeout))?)
        } else {
            let created = self.request(
                "session/new",
                json!({ "cwd": config.cwd, "mcpServers": [] }),
                Some(self.start_timeout),
            )?;
            (required_string(created.get("sessionId"), &format!("{name} session id"))?, created)
        };
        // Only switch to a mode the session offers.
        if let Some(mode) = config.mode() {
            let offered = opened
                .get("modes")
                .and_then(|modes| modes.get("availableModes"))
                .and_then(Value::as_array)
                .is_some_and(|modes| modes.iter().any(|m| m.get("id").and_then(Value::as_str) == Some(mode)));
            if offered {
                self.request("session/set_mode", json!({ "sessionId": session, "modeId": mode }), None)?;
            }
        }
        if let Some(model) = &config.model {
            self.request("session/set_model", json!({ "sessionId": session, "modelId": model }), None)?;
        }
        Ok(session)
    }

    fn request(&self, method: &str, params: Value, timeout: Option<Duration>) -> AResult<Value> {
        let process = self.process()?;
        let id = format!("ruddr-{}-{}", self.flavor.slug(), self.sequence.fetch_add(1, Ordering::SeqCst) + 1);
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let timeout = timeout.unwrap_or(self.rpc_timeout);
        let timeout_message = format!("{} {method} timed out after {}ms", self.flavor.labels().prefix, timeout.as_millis());
        process
            .call(&id, &message, timeout, timeout_message)
            .map(Value::Object)
            .map_err(AdapterError::failed)
    }

    fn notify(&self, method: &str, params: Value) -> AResult<()> {
        self.process()?
            .notify(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .map_err(AdapterError::failed)
    }

    fn close(&self) {
        if let Some(process) = lock(&self.process).take() {
            process.close(Duration::from_secs(2));
        }
    }
}

#[cfg(test)]
mod tests;
