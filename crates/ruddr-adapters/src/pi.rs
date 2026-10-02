//! The Pi adapter. Port of pi/runtime.ts.
//!
//! The adapter runs `pi --mode rpc` and sends it JSON commands with string
//! IDs. A turn is a `prompt` command; a steer is a `steer` command; the turn
//! ends at `agent_settled`, unless a steer was accepted after that event, in
//! which case the next `agent_settled` ends it.

use crate::protocol::{
    AResult, Adapter, AdapterError, Emit, compact, finite, lock, num, number, optional_string, read_text_input, record, required_string,
    text_content, uuid_v4,
};
use crate::rpc::{EventFn, Incoming, Labels, RpcProcess};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(30);
// Launching the Pi binary is far slower and more variable than a steady-state
// RPC, so the startup handshake gets its own budget.
const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(60);
// UI requests Pi does not wait on; every other extension UI request is
// cancelled so it cannot hang a turn.
const FIRE_AND_FORGET_UI_METHODS: [&str; 5] = ["notify", "setStatus", "setWidget", "setTitle", "set_editor_text"];

#[derive(Debug, Clone, PartialEq)]
pub struct PiThread {
    pub id: String,
    pub cwd: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub executable: String,
    pub sandbox: String,
    pub ephemeral: bool,
    pub resumed: bool,
}

pub trait PiClient: Send + Sync {
    /// Starts Pi and returns its session ID.
    fn start(&self, config: &PiThread, on_event: EventFn) -> AResult<String>;
    /// Sends one command and returns Pi's whole response message.
    fn send(&self, command: Value) -> AResult<Map<String, Value>>;
    fn close(&self);
}

struct Turn {
    serial: u64,
    id: String,
    interrupted: bool,
    settling: bool,
    steer_generation: u64,
    pending_steers: u32,
    assistant_messages: Vec<Map<String, Value>>,
}

#[derive(Default)]
struct State {
    initialized: bool,
    closed: bool,
    thread: Option<PiThread>,
    turn: Option<Turn>,
    turn_serial: u64,
    tools: HashMap<String, Map<String, Value>>,
}

struct Inner {
    emit: Emit,
    client: Box<dyn PiClient>,
    executable: String,
    state: Mutex<State>,
    steers_settled: Condvar,
}

pub struct PiAdapter {
    inner: Arc<Inner>,
}

impl PiAdapter {
    pub fn new(emit: Emit, executable: String) -> PiAdapter {
        PiAdapter::with_client(
            emit,
            executable,
            Box::new(SubprocessPiClient::new(DEFAULT_RPC_TIMEOUT, DEFAULT_START_TIMEOUT)),
        )
    }

    pub fn with_client(emit: Emit, executable: String, client: Box<dyn PiClient>) -> PiAdapter {
        PiAdapter {
            inner: Arc::new(Inner {
                emit,
                client,
                executable,
                state: Mutex::new(State::default()),
                steers_settled: Condvar::new(),
            }),
        }
    }
}

impl Adapter for PiAdapter {
    fn dispatch(&self, method: &str, params: &Value) -> AResult<Value> {
        let inner = &self.inner;
        match method {
            "initialize" => {
                lock(&inner.state).initialized = true;
                Ok(json!({
                    "serverInfo": { "name": "ruddr-pi-adapter", "version": "1" },
                    "capabilities": { "experimentalApi": true },
                }))
            }
            "initialized" => Ok(Value::Null),
            "thread/start" => Inner::acquire_thread(inner, params, false),
            "thread/resume" => Inner::acquire_thread(inner, params, true),
            "turn/start" => inner.start_turn(params),
            "turn/steer" => inner.steer_turn(params),
            "turn/interrupt" => inner.interrupt_turn(params),
            _ => Err(AdapterError::not_found(format!(
                "method {method} is not supported by the Pi adapter"
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
        let mut thread = PiThread {
            id: if resumed {
                required_string(input.get("threadId"), "threadId")?
            } else {
                uuid_v4()
            },
            cwd: required_string(input.get("cwd"), "cwd")?,
            executable: optional_string(input.get("providerPath")).unwrap_or_else(|| self.executable.clone()),
            sandbox: required_string(input.get("sandbox"), "sandbox")?,
            ephemeral: input.get("ephemeral") == Some(&Value::Bool(true)),
            resumed,
            model: optional_string(input.get("model")),
            effort: optional_string(input.get("effort")),
        };
        // Pi events are handled one at a time on a worker thread, so the
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

    fn start_turn(&self, params: &Value) -> AResult<Value> {
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
        let text = read_text_input(input.get("input"))?;
        if let Some(effort) = optional_string(input.get("effort")) {
            self.client.send(json!({ "type": "set_thinking_level", "level": effort }))?;
        }
        let (turn_id, serial) = {
            let mut state = lock(&self.state);
            state.turn_serial += 1;
            let serial = state.turn_serial;
            let id = uuid_v4();
            state.turn = Some(Turn {
                serial,
                id: id.clone(),
                interrupted: false,
                settling: false,
                steer_generation: 0,
                pending_steers: 0,
                assistant_messages: Vec::new(),
            });
            self.emit.emit(json!({
                "method": "turn/started",
                "params": { "threadId": thread_id, "turn": { "id": id, "status": "inProgress" } },
            }));
            (id, serial)
        };
        if let Err(error) = self.client.send(json!({ "type": "prompt", "message": text })) {
            let mut state = lock(&self.state);
            if state.turn.as_ref().is_some_and(|turn| turn.serial == serial) {
                state.turn = None;
            }
            return Err(error);
        }
        Ok(json!({ "turn": { "id": turn_id, "status": "inProgress" } }))
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
        let sent = text
            .clone()
            .and_then(|text| self.client.send(json!({ "type": "steer", "message": text })));
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
        // puts it in the transcript. Pi echoes nothing back.
        self.emit.emit(json!({
            "method": "item/completed",
            "params": { "threadId": thread_id, "item": { "id": uuid_v4(), "type": "userMessage", "status": "completed", "text": text } },
        }));
        Ok(json!({ "turnId": turn_id }))
    }

    fn interrupt_turn(&self, params: &Value) -> AResult<Value> {
        let input = record(params, "interrupt parameters")?;
        {
            let mut state = lock(&self.state);
            let (_, turn) = require_turn(&mut state, input, "turnId")?;
            turn.interrupted = true;
        }
        self.client.send(json!({ "type": "abort" }))?;
        Ok(json!({}))
    }

    fn handle_event(&self, event: Map<String, Value>) {
        let mut state = lock(&self.state);
        if state.thread.is_none() {
            return;
        }
        let Some(turn) = state.turn.as_mut() else { return };
        let serial = turn.serial;
        let generation = turn.steer_generation;
        let tool_id = optional_string(event.get("toolCallId"));
        match event.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message_end" => {
                if let Some(message) = event.get("message").and_then(Value::as_object)
                    && message.get("role").and_then(Value::as_str) == Some("assistant")
                {
                    turn.assistant_messages.push(message.clone());
                }
            }
            // A tool event without a toolCallId is malformed; the TypeScript
            // adapter failed the whole Pi process on one, Ruddr skips it.
            "tool_execution_start" => {
                let Some(id) = tool_id else { return };
                state.tools.insert(id, event.clone());
                self.emit_tool(&state, "item/started", &event, "inProgress");
            }
            "tool_execution_update" => {
                let Some(id) = tool_id else { return };
                let merged = merge_tool(&mut state, &id, &event);
                self.emit_tool(&state, "item/updated", &merged, "inProgress");
            }
            "tool_execution_end" => {
                let Some(id) = tool_id else { return };
                let merged = merge_tool(&mut state, &id, &event);
                state.tools.remove(&id);
                let status = if event.get("isError") == Some(&Value::Bool(true)) {
                    "failed"
                } else {
                    "completed"
                };
                self.emit_tool(&state, "item/completed", &merged, status);
            }
            "agent_settled" => {
                drop(state);
                self.complete_turn(serial, None, None, generation);
            }
            "ruddr_error" => {
                let message = optional_string(event.get("error")).unwrap_or_else(|| "Pi RPC process failed".into());
                drop(state);
                self.complete_turn(serial, Some("failed"), Some(message), generation);
            }
            _ => {}
        }
    }

    fn emit_tool(&self, state: &State, method: &str, event: &Map<String, Value>, status: &str) {
        let Some(thread) = &state.thread else { return };
        let Some(id) = optional_string(event.get("toolCallId")) else {
            return;
        };
        let name = optional_string(event.get("toolName")).unwrap_or_else(|| "tool".into());
        let input = event.get("args").and_then(Value::as_object).cloned().unwrap_or_default();
        let result = event
            .get("result")
            .and_then(Value::as_object)
            .or_else(|| event.get("partialResult").and_then(Value::as_object))
            .cloned()
            .unwrap_or_default();
        let command = optional_string(input.get("command")).unwrap_or_else(|| format!("{name} {}", compact(&input)));
        self.emit.emit(json!({
            "method": method,
            "params": { "threadId": thread.id, "item": {
                "id": id, "type": "toolCall", "status": status, "toolName": name, "command": command, "input": input,
                "output": text_content(result.get("content"), true),
            } },
        }));
    }

    fn complete_turn(&self, serial: u64, forced_status: Option<&str>, forced_error: Option<String>, settled_generation: u64) {
        let current = |state: &State| state.turn.as_ref().is_some_and(|turn| turn.serial == serial && !turn.settling);
        let mut state = lock(&self.state);
        if !current(&state) || state.thread.is_none() {
            return;
        }
        if forced_status.is_none() {
            let turn = state.turn.as_ref().expect("turn is current");
            if turn.pending_steers > 0 {
                while state
                    .turn
                    .as_ref()
                    .is_some_and(|turn| turn.serial == serial && turn.pending_steers > 0)
                {
                    state = self.steers_settled.wait(state).unwrap_or_else(|e| e.into_inner());
                }
                if !current(&state) {
                    return;
                }
            }
            if state.turn.as_ref().is_some_and(|turn| turn.steer_generation != settled_generation) {
                return;
            }
        }
        let thread_id = state.thread.as_ref().map(|t| t.id.clone()).unwrap_or_default();
        let (turn_id, interrupted, messages) = {
            let turn = state.turn.as_mut().expect("turn is current");
            turn.settling = true;
            (turn.id.clone(), turn.interrupted, std::mem::take(&mut turn.assistant_messages))
        };
        // TODO(review): Define terminal statuses for unfinished Pi tool calls before clearing them at turn completion.
        for (index, message) in messages.iter().enumerate() {
            let parts: Vec<&Map<String, Value>> = message
                .get("content")
                .and_then(Value::as_array)
                .map(|c| c.iter().filter_map(Value::as_object).collect())
                .unwrap_or_default();
            let joined = |kind: &str, field: &str| {
                parts
                    .iter()
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some(kind))
                    .filter_map(|part| part.get(field).and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim()
                    .to_string()
            };
            let reasoning = joined("thinking", "thinking");
            if !reasoning.is_empty() {
                self.emit.emit(json!({
                    "method": "item/completed",
                    "params": { "threadId": thread_id, "item": {
                        "id": format!("{turn_id}-reasoning-{index}"), "type": "reasoning", "status": "completed",
                        "summary": [{ "type": "summary_text", "text": reasoning }],
                    } },
                }));
            }
            let text = joined("text", "text");
            if !text.is_empty() {
                let phase = if index == messages.len() - 1 {
                    "final_answer"
                } else {
                    "commentary"
                };
                self.emit.emit(json!({
                    "method": "item/completed",
                    "params": { "threadId": thread_id, "item": {
                        "id": format!("{turn_id}-message-{index}"), "type": "agentMessage", "status": "completed", "text": text, "phase": phase,
                    } },
                }));
            }
        }
        drop(state);
        let stats = self.client.send(json!({ "type": "get_session_stats" }));
        let mut state = lock(&self.state);
        if let Ok(response) = stats
            && let Some(stats) = response.get("data").and_then(Value::as_object)
        {
            self.emit_usage(&thread_id, stats);
        }
        let stop_reason = messages.last().and_then(|message| optional_string(message.get("stopReason")));
        let status = forced_status.unwrap_or(if interrupted || stop_reason.as_deref() == Some("aborted") {
            "interrupted"
        } else if stop_reason.as_deref() == Some("error") {
            "failed"
        } else {
            "completed"
        });
        state.turn = None;
        self.steers_settled.notify_all();
        let mut turn = json!({ "id": turn_id, "status": status });
        if status == "failed" {
            turn["error"] = json!({ "message": forced_error.unwrap_or_else(|| "Pi model returned an error".into()) });
        }
        self.emit
            .emit(json!({ "method": "turn/completed", "params": { "threadId": thread_id, "turn": turn } }));
    }

    fn emit_usage(&self, thread_id: &str, stats: &Map<String, Value>) {
        let tokens = stats.get("tokens").and_then(Value::as_object).cloned().unwrap_or_default();
        let context = stats.get("contextUsage").and_then(Value::as_object).cloned().unwrap_or_default();
        let input = number(tokens.get("input"));
        let output = number(tokens.get("output"));
        let cached = number(tokens.get("cacheRead"));
        // `||` in the TypeScript: a zero total falls through to the next.
        let total = [number(tokens.get("total")), number(tokens.get("totalTokens"))]
            .into_iter()
            .find(|n| *n != 0.0)
            .unwrap_or(input + output + cached + number(tokens.get("cacheWrite")));
        let cost = number(stats.get("cost"));
        if total == 0.0 && cost == 0.0 {
            return;
        }
        let mut token_usage = json!({ "total": {
            "inputTokens": num(input + cached),
            "cachedInputTokens": num(cached),
            "outputTokens": num(output),
            "totalTokens": num(total),
        } });
        if let Some(used) = finite(context.get("tokens")).filter(|n| *n >= 0.0) {
            token_usage["last"] = json!({ "totalTokens": num(used) });
        }
        let window = number(context.get("contextWindow"));
        if window != 0.0 {
            token_usage["modelContextWindow"] = num(window);
        }
        self.emit.emit(json!({
            "method": "thread/tokenUsage/updated",
            "params": { "threadId": thread_id, "tokenUsage": token_usage, "costUsd": num(cost) },
        }));
    }
}

fn merge_tool(state: &mut State, id: &str, event: &Map<String, Value>) -> Map<String, Value> {
    let mut merged = state.tools.get(id).cloned().unwrap_or_default();
    merged.extend(event.clone());
    state.tools.insert(id.to_string(), merged.clone());
    merged
}

fn require_thread(thread_id: &str, input: &Map<String, Value>) -> AResult<()> {
    if required_string(input.get("threadId"), "threadId")? != thread_id {
        return Err(AdapterError::invalid("threadId does not match the configured Pi session"));
    }
    Ok(())
}

fn require_turn<'a>(state: &'a mut State, input: &Map<String, Value>, turn_key: &str) -> AResult<(String, &'a mut Turn)> {
    let thread_id = state.thread.as_ref().map(|t| t.id.clone()).unwrap_or_default();
    require_thread(&thread_id, input)?;
    let Some(turn) = state.turn.as_mut() else {
        return Err(AdapterError::invalid("there is no active Pi turn"));
    };
    if turn.settling {
        return Err(AdapterError::invalid("the active Pi turn is settling"));
    }
    if required_string(input.get(turn_key), turn_key)? != turn.id {
        return Err(AdapterError::invalid(format!("{turn_key} does not match the active Pi turn")));
    }
    Ok((thread_id, turn))
}

/// Runs `pi --mode rpc` and matches responses to commands by ID.
pub struct SubprocessPiClient {
    rpc_timeout: Duration,
    start_timeout: Duration,
    process: Mutex<Option<Arc<RpcProcess>>>,
    sequence: AtomicU64,
}

impl SubprocessPiClient {
    pub fn new(rpc_timeout: Duration, start_timeout: Duration) -> SubprocessPiClient {
        SubprocessPiClient {
            rpc_timeout,
            start_timeout: start_timeout.max(rpc_timeout),
            process: Mutex::new(None),
            sequence: AtomicU64::new(0),
        }
    }

    fn send_with_timeout(&self, command: Value, timeout: Duration) -> AResult<Map<String, Value>> {
        let process = lock(&self.process)
            .clone()
            .ok_or_else(|| AdapterError::failed("Pi RPC process is not running"))?;
        let id = format!("ruddr-pi-{}", self.sequence.fetch_add(1, Ordering::SeqCst) + 1);
        let kind = command.get("type").and_then(Value::as_str).unwrap_or("command").to_string();
        let mut message = command;
        if let Some(object) = message.as_object_mut() {
            object.insert("id".into(), json!(id));
        }
        let timeout_message = format!("Pi RPC {kind} timed out after {}ms", timeout.as_millis());
        process.call(&id, &message, timeout, timeout_message).map_err(AdapterError::failed)
    }
}

/// Pi's argv: RPC mode, the session selector, and the read-only tool set.
pub fn pi_args(config: &PiThread) -> Vec<String> {
    // TODO(review): Define Pi approval behavior for inherited project resources before changing --approve.
    let mut args: Vec<String> = ["--mode", "rpc", "--approve"].map(String::from).to_vec();
    if let Some(model) = &config.model {
        args.extend(["--model".into(), model.clone()]);
    }
    if let Some(effort) = &config.effort {
        args.extend(["--thinking".into(), effort.clone()]);
    }
    if config.ephemeral {
        args.push("--no-session".into());
    } else if config.resumed {
        args.extend(["--session".into(), config.id.clone()]);
    } else {
        args.extend(["--session-id".into(), config.id.clone()]);
    }
    if config.sandbox == "read-only" {
        args.extend(["--no-extensions", "--tools", "read,grep,find,ls"].map(String::from));
    }
    args
}

fn classify(message: &Map<String, Value>) -> Incoming {
    let kind = message.get("type").and_then(Value::as_str);
    let id = message.get("id").and_then(Value::as_str);
    match (kind, id) {
        (Some("response"), Some(id)) => {
            let outcome = if message.get("success") == Some(&Value::Bool(true)) {
                Ok(message.clone())
            } else {
                Err(optional_string(message.get("error")).unwrap_or_else(|| "Pi RPC command returned a malformed response".into()))
            };
            Incoming::Response(id.to_string(), outcome)
        }
        (Some("response"), None) => Incoming::Skip,
        (Some("extension_ui_request"), Some(id)) => {
            let method = optional_string(message.get("method"));
            if method.as_deref().is_some_and(|m| FIRE_AND_FORGET_UI_METHODS.contains(&m)) {
                Incoming::Skip
            } else {
                Incoming::Reply(json!({ "type": "extension_ui_response", "id": id, "cancelled": true }))
            }
        }
        _ => Incoming::Event(message.clone()),
    }
}

fn failure_event(message: &str) -> Map<String, Value> {
    json!({ "type": "ruddr_error", "error": message })
        .as_object()
        .cloned()
        .unwrap_or_default()
}

impl PiClient for SubprocessPiClient {
    fn start(&self, config: &PiThread, on_event: EventFn) -> AResult<String> {
        let mut command = Command::new(&config.executable);
        command.args(pi_args(config)).current_dir(&config.cwd);
        let labels = Labels {
            prefix: "Pi RPC",
            process: "Pi RPC process",
            client: "Pi RPC client",
            message: "Pi RPC message",
        };
        let process = RpcProcess::spawn(command, labels, self.rpc_timeout, classify, on_event, failure_event).map_err(|error| {
            AdapterError::failed(if error.kind() == std::io::ErrorKind::NotFound {
                format!("Pi executable not found at {}; install pi or pass --pi-path", config.executable)
            } else {
                format!("start Pi: {error}")
            })
        })?;
        *lock(&self.process) = Some(process);
        let response = self.send_with_timeout(json!({ "type": "get_state" }), self.start_timeout)?;
        let state = record(response.get("data").unwrap_or(&Value::Null), "Pi state")?;
        required_string(state.get("sessionId"), "Pi session id")
    }

    fn send(&self, command: Value) -> AResult<Map<String, Value>> {
        self.send_with_timeout(command, self.rpc_timeout)
    }

    fn close(&self) {
        if let Some(process) = lock(&self.process).take() {
            process.close(Duration::from_secs(1));
        }
    }
}

#[cfg(test)]
mod tests;
