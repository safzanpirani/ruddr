//! The Pi adapter, which also drives omp (oh-my-pi). Port of pi/runtime.ts.
//!
//! The adapter runs `pi --mode rpc` (or `omp --mode rpc`) and sends it JSON
//! commands with string IDs. A turn is a `prompt` command; a steer is a
//! `steer` command; the turn ends at Pi's `agent_settled` or omp's
//! `session_settled`, unless a steer was accepted after that event, in which
//! case the next one ends it. omp also reports a `prompt_result` for every
//! prompt; one that never reached the agent ends the turn itself, because no
//! settle event follows it.

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
// omp's `cancel` withdraws an earlier request and expects no answer.
const FIRE_AND_FORGET_UI_METHODS: [&str; 6] = ["notify", "setStatus", "setWidget", "setTitle", "set_editor_text", "cancel"];

/// Which CLI the adapter drives. omp is a Pi fork whose RPC mode keeps Pi's
/// commands and events but renames session flags and the settle event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Pi,
    Omp,
}

impl Flavor {
    /// The name errors and labels use.
    pub fn name(self) -> &'static str {
        match self {
            Flavor::Pi => "Pi",
            Flavor::Omp => "omp",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Flavor::Pi => "pi",
            Flavor::Omp => "omp",
        }
    }

    /// The event that says the session went quiet and the turn is over.
    fn settle_event(self) -> &'static str {
        match self {
            Flavor::Pi => "agent_settled",
            Flavor::Omp => "session_settled",
        }
    }

    fn labels(self) -> Labels {
        match self {
            Flavor::Pi => Labels {
                prefix: "Pi RPC",
                process: "Pi RPC process",
                client: "Pi RPC client",
                message: "Pi RPC message",
            },
            Flavor::Omp => Labels {
                prefix: "omp RPC",
                process: "omp RPC process",
                client: "omp RPC client",
                message: "omp RPC message",
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PiThread {
    pub flavor: Flavor,
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
    /// omp's `prompt_result` status and error, when one arrived.
    prompt_status: Option<String>,
    prompt_error: Option<String>,
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
    flavor: Flavor,
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
        PiAdapter::for_flavor(Flavor::Pi, emit, executable)
    }

    pub fn omp(emit: Emit, executable: String) -> PiAdapter {
        PiAdapter::for_flavor(Flavor::Omp, emit, executable)
    }

    fn for_flavor(flavor: Flavor, emit: Emit, executable: String) -> PiAdapter {
        let client = SubprocessPiClient::new(flavor, DEFAULT_RPC_TIMEOUT, DEFAULT_START_TIMEOUT);
        PiAdapter::with_client(flavor, emit, executable, Box::new(client))
    }

    pub fn with_client(flavor: Flavor, emit: Emit, executable: String, client: Box<dyn PiClient>) -> PiAdapter {
        PiAdapter {
            inner: Arc::new(Inner {
                flavor,
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
                    "serverInfo": { "name": format!("ruddr-{}-adapter", inner.flavor.slug()), "version": "1" },
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
        let mut thread = PiThread {
            flavor: self.flavor,
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
        // Provider events are handled one at a time on a worker thread, so the
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
        require_thread(self.flavor, &thread_id, input)?;
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
                prompt_status: None,
                prompt_error: None,
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
            let (thread_id, turn) = require_turn(self.flavor, &mut state, input, "expectedTurnId")?;
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
        // puts it in the transcript. Pi and omp echo nothing back.
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
            let (_, turn) = require_turn(self.flavor, &mut state, input, "turnId")?;
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
        let kind = event.get("type").and_then(Value::as_str).unwrap_or_default();
        if kind == self.flavor.settle_event() {
            drop(state);
            self.complete_turn(serial, None, None, generation);
            return;
        }
        match kind {
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
            // omp only. A prompt that never reached the agent (a local slash
            // command, or a failure before the model call) gets no settle
            // event, so its result ends the turn.
            "prompt_result" if self.flavor == Flavor::Omp => {
                turn.prompt_status = optional_string(event.get("status"));
                turn.prompt_error = event.get("error").and_then(|error| optional_string(error.get("message")));
                if event.get("agentInvoked") == Some(&Value::Bool(false)) {
                    drop(state);
                    self.complete_turn(serial, None, None, generation);
                }
            }
            "ruddr_error" => {
                let message = optional_string(event.get("error")).unwrap_or_else(|| format!("{} RPC process failed", self.flavor.name()));
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
        let mut item = json!({
            "id": id, "type": "toolCall", "status": status, "toolName": name, "command": command, "input": input,
            "output": text_content(result.get("content"), true),
        });
        // omp's hashline edits name anchored lines, not old and new text, so
        // the change comes from the result's numbered diff once it lands.
        if self.flavor == Flavor::Omp && matches!(name.as_str(), "edit" | "ast_edit") {
            item["type"] = json!("fileChange");
            let path = optional_string(input.get("path")).unwrap_or_default();
            let changes = omp_changes(result.get("details"), &path);
            if !changes.is_empty() {
                item["changes"] = Value::Array(changes);
            }
        }
        self.emit
            .emit(json!({ "method": method, "params": { "threadId": thread.id, "item": item } }));
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
        let (turn_id, interrupted, messages, prompt_status, prompt_error) = {
            let turn = state.turn.as_mut().expect("turn is current");
            turn.settling = true;
            (
                turn.id.clone(),
                turn.interrupted,
                std::mem::take(&mut turn.assistant_messages),
                turn.prompt_status.take(),
                turn.prompt_error.take(),
            )
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
        let ended = |reason: &str| stop_reason.as_deref() == Some(reason) || prompt_status.as_deref() == Some(reason);
        let status = forced_status.unwrap_or(if interrupted || ended("aborted") {
            "interrupted"
        } else if ended("error") {
            "failed"
        } else {
            "completed"
        });
        state.turn = None;
        self.steers_settled.notify_all();
        let mut turn = json!({ "id": turn_id, "status": status });
        if status == "failed" {
            let message = forced_error
                .or(prompt_error)
                .unwrap_or_else(|| format!("{} model returned an error", self.flavor.name()));
            turn["error"] = json!({ "message": message });
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

/// Codex-style `changes` from an omp edit result's `details`: one entry per
/// file, from `perFileResults` for a multi-file edit. Every entry is an
/// `update` carrying hunks, because a Codex `add` carries file content and
/// a created file's hunks already read as all additions.
fn omp_changes(details: Option<&Value>, call_path: &str) -> Vec<Value> {
    let Some(details) = details else { return Vec::new() };
    let files: Vec<&Value> = match details.get("perFileResults").and_then(Value::as_array) {
        Some(files) => files.iter().collect(),
        None => vec![details],
    };
    files
        .into_iter()
        .filter_map(|file| {
            let path = file.get("path").and_then(Value::as_str).unwrap_or(call_path);
            let hunks = ruddr_core::diff::numbered_diff(file.get("diff").and_then(Value::as_str).unwrap_or_default());
            (!path.is_empty() && !hunks.is_empty()).then(|| json!({ "path": path, "kind": { "type": "update" }, "diff": hunks }))
        })
        .collect()
}

fn merge_tool(state: &mut State, id: &str, event: &Map<String, Value>) -> Map<String, Value> {
    let mut merged = state.tools.get(id).cloned().unwrap_or_default();
    merged.extend(event.clone());
    state.tools.insert(id.to_string(), merged.clone());
    merged
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
    if turn.settling {
        return Err(AdapterError::invalid(format!("the active {name} turn is settling")));
    }
    if required_string(input.get(turn_key), turn_key)? != turn.id {
        return Err(AdapterError::invalid(format!("{turn_key} does not match the active {name} turn")));
    }
    Ok((thread_id, turn))
}

/// Runs `pi --mode rpc` or `omp --mode rpc` and matches responses to
/// commands by ID.
pub struct SubprocessPiClient {
    flavor: Flavor,
    rpc_timeout: Duration,
    start_timeout: Duration,
    process: Mutex<Option<Arc<RpcProcess>>>,
    sequence: AtomicU64,
}

impl SubprocessPiClient {
    pub fn new(flavor: Flavor, rpc_timeout: Duration, start_timeout: Duration) -> SubprocessPiClient {
        SubprocessPiClient {
            flavor,
            rpc_timeout,
            start_timeout: start_timeout.max(rpc_timeout),
            process: Mutex::new(None),
            sequence: AtomicU64::new(0),
        }
    }

    fn send_with_timeout(&self, command: Value, timeout: Duration) -> AResult<Map<String, Value>> {
        let labels = self.flavor.labels();
        let process = lock(&self.process)
            .clone()
            .ok_or_else(|| AdapterError::failed(format!("{} is not running", labels.process)))?;
        let id = format!("ruddr-{}-{}", self.flavor.slug(), self.sequence.fetch_add(1, Ordering::SeqCst) + 1);
        let kind = command.get("type").and_then(Value::as_str).unwrap_or("command").to_string();
        let mut message = command;
        if let Some(object) = message.as_object_mut() {
            object.insert("id".into(), json!(id));
        }
        let timeout_message = format!("{} {kind} timed out after {}ms", labels.prefix, timeout.as_millis());
        process.call(&id, &message, timeout, timeout_message).map_err(AdapterError::failed)
    }
}

/// The provider argv: RPC mode, the session selector, and the read-only tool
/// set. omp has no `--session-id`, so a fresh omp session takes the ID omp
/// picks, which `get_state` reports.
pub fn pi_args(config: &PiThread) -> Vec<String> {
    if config.flavor == Flavor::Omp {
        return omp_args(config);
    }
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

/// omp's argv. `--auto-approve` matches the `never` approval policy Ruddr
/// requires, because a tool approval prompt would be cancelled unanswered.
/// `--allow-home` keeps omp in the run's working directory when that is the
/// home directory, where omp otherwise moves to a temporary one.
fn omp_args(config: &PiThread) -> Vec<String> {
    let mut args: Vec<String> = ["--mode", "rpc", "--auto-approve", "--allow-home"].map(String::from).to_vec();
    if let Some(model) = &config.model {
        args.extend(["--model".into(), model.clone()]);
    }
    if let Some(effort) = &config.effort {
        args.extend(["--thinking".into(), effort.clone()]);
    }
    if config.ephemeral {
        args.push("--no-session".into());
    } else if config.resumed {
        args.extend(["--resume".into(), config.id.clone()]);
    }
    if config.sandbox == "read-only" {
        args.extend(["--no-extensions", "--tools", "read,grep,find,glob"].map(String::from));
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
                Err(optional_string(message.get("error")).unwrap_or_else(|| "RPC command returned a malformed response".into()))
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
        let (name, slug) = (self.flavor.name(), self.flavor.slug());
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
        let response = self.send_with_timeout(json!({ "type": "get_state" }), self.start_timeout)?;
        let state = record(response.get("data").unwrap_or(&Value::Null), &format!("{name} state"))?;
        required_string(state.get("sessionId"), &format!("{name} session id"))
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
