//! The Claude Code adapter. Port of claude/runtime.ts.
//!
//! Each turn runs one `claude` query (one CLI process). The first turn of a
//! persisted session passes `--session-id`; later turns pass `--resume`,
//! because the CLI rejects a session ID that already exists. A steer is one
//! more user message on the running query's stdin.

pub mod cli;

use crate::protocol::{
    AResult, Adapter, AdapterError, Emit, Latch, compact, finite, lock, num, number, read_text_input, record, uuid_v4, uuid_v7,
};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sandbox {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl Sandbox {
    pub fn parse(value: Option<&Value>) -> AResult<Sandbox> {
        match value.and_then(Value::as_str) {
            Some("read-only") => Ok(Sandbox::ReadOnly),
            Some("workspace-write") => Ok(Sandbox::WorkspaceWrite),
            Some("danger-full-access") => Ok(Sandbox::DangerFullAccess),
            _ => Err(AdapterError::invalid(
                "sandbox must be read-only, workspace-write, or danger-full-access",
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Sandbox::ReadOnly => "read-only",
            Sandbox::WorkspaceWrite => "workspace-write",
            Sandbox::DangerFullAccess => "danger-full-access",
        }
    }
}

const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

#[derive(Debug, Clone, PartialEq)]
pub struct ThreadConfig {
    pub id: String,
    pub cwd: String,
    pub model: Option<String>,
    pub sandbox: Sandbox,
    pub effort: Option<String>,
    pub claude_path: Option<String>,
    pub persist_session: bool,
    pub resumed: bool,
}

/// What one query needs: the TypeScript adapter's SDK `Options`.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryOptions {
    pub executable: String,
    pub cwd: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub sandbox: Sandbox,
    pub permission_mode: &'static str,
    pub persist_session: bool,
    pub resume: Option<String>,
    pub session_id: Option<String>,
}

impl QueryOptions {
    /// Whether permission requests reach Ruddr. Bypass mode never asks.
    pub fn can_use_tool(&self) -> bool {
        self.permission_mode != "bypassPermissions"
    }
}

pub fn build_query_options(thread: &ThreadConfig, default_executable: &str) -> QueryOptions {
    let permission_mode = match thread.sandbox {
        Sandbox::ReadOnly => "plan",
        Sandbox::DangerFullAccess => "bypassPermissions",
        Sandbox::WorkspaceWrite => "acceptEdits",
    };
    QueryOptions {
        executable: thread.claude_path.clone().unwrap_or_else(|| default_executable.to_string()),
        cwd: thread.cwd.clone(),
        model: thread.model.clone(),
        effort: thread.effort.clone(),
        sandbox: thread.sandbox,
        permission_mode,
        persist_session: thread.persist_session,
        resume: thread.resumed.then(|| thread.id.clone()),
        session_id: (!thread.resumed).then(|| thread.id.clone()),
    }
}

/// The prompt stream of one query: steers queue behind the first prompt, and
/// a closed queue rejects new messages.
#[derive(Default)]
pub struct PromptQueue {
    state: Mutex<(VecDeque<Value>, bool)>,
    changed: Condvar,
}

impl PromptQueue {
    pub fn push(&self, message: Value) -> Result<(), String> {
        let mut state = lock(&self.state);
        if state.1 {
            return Err("Claude prompt queue is closed".into());
        }
        state.0.push_back(message);
        self.changed.notify_all();
        Ok(())
    }

    pub fn close(&self) {
        lock(&self.state).1 = true;
        self.changed.notify_all();
    }

    /// The next message, waiting for one; `None` once closed and drained.
    pub fn next(&self) -> Option<Value> {
        let mut state = lock(&self.state);
        loop {
            if let Some(message) = state.0.pop_front() {
                return Some(message);
            }
            if state.1 {
                return None;
            }
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// One item of a query's message stream. `End` carries the error that ended
/// it, if any.
pub enum StreamItem {
    Message(Value),
    End(Option<String>),
}

/// A running query: the SDK `Query` the TypeScript adapter held.
pub trait ClaudeQuery: Send + Sync {
    /// Asks the CLI to stop the current turn.
    fn interrupt(&self) {}
    /// Ends the message stream at once and lets the process exit.
    fn close(&self);
    /// Terminates the process if it is still running when the adapter exits.
    fn shutdown(&self) {}
}

pub trait QueryFactory: Send + Sync {
    fn create(&self, options: &QueryOptions, prompt: Arc<PromptQueue>, events: Sender<StreamItem>) -> Result<Arc<dyn ClaudeQuery>, String>;
}

struct TextBlock {
    id: String,
    text: String,
}

struct ToolBlock {
    index: i64,
    id: String,
    name: String,
    input: Map<String, Value>,
    partial_input: String,
    started_at: Instant,
    parent_tool_use_id: Option<String>,
}

struct TurnState {
    id: String,
    text_blocks: HashMap<i64, TextBlock>,
    pending_text: Vec<TextBlock>,
    thinking_blocks: HashMap<i64, TextBlock>,
    tools_by_index: HashMap<i64, String>,
    tools_by_id: HashMap<String, ToolBlock>,
    emitted_texts: HashSet<String>,
    emitted_block_ids: HashSet<String>,
    interrupt_requested: bool,
}

impl TurnState {
    fn new() -> TurnState {
        TurnState {
            id: uuid_v4(),
            text_blocks: HashMap::new(),
            pending_text: Vec::new(),
            thinking_blocks: HashMap::new(),
            tools_by_index: HashMap::new(),
            tools_by_id: HashMap::new(),
            emitted_texts: HashSet::new(),
            emitted_block_ids: HashSet::new(),
            interrupt_requested: false,
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Usage {
    input_tokens: f64,
    cache_creation_input_tokens: f64,
    cache_read_input_tokens: f64,
    output_tokens: f64,
}

#[derive(Default)]
struct State {
    initialized: bool,
    closed: bool,
    thread: Option<ThreadConfig>,
    turn: Option<TurnState>,
    queue: Option<Arc<PromptQueue>>,
    runtime: Option<Arc<dyn ClaudeQuery>>,
    stream: Option<Arc<Latch>>,
    runtimes: Vec<Arc<dyn ClaudeQuery>>,
    generation: u64,
    turns_completed: u64,
    totals: [f64; 4],
    cost_total: f64,
    context_window: f64,
    context_model: Option<String>,
    last_usage: Option<Usage>,
}

struct Inner {
    emit: Emit,
    factory: Box<dyn QueryFactory>,
    default_executable: String,
    settle_timeout: Duration,
    state: Mutex<State>,
}

pub struct ClaudeAdapter {
    inner: Arc<Inner>,
}

impl ClaudeAdapter {
    /// An adapter that runs the `claude` CLI found at `executable`.
    pub fn new(emit: Emit, executable: String) -> ClaudeAdapter {
        ClaudeAdapter::with_factory(emit, executable, Box::new(cli::CliFactory), Duration::from_secs(1))
    }

    pub fn with_factory(emit: Emit, executable: String, factory: Box<dyn QueryFactory>, settle_timeout: Duration) -> ClaudeAdapter {
        ClaudeAdapter {
            inner: Arc::new(Inner {
                emit,
                factory,
                default_executable: executable,
                settle_timeout,
                state: Mutex::new(State::default()),
            }),
        }
    }
}

impl Adapter for ClaudeAdapter {
    fn dispatch(&self, method: &str, params: &Value) -> AResult<Value> {
        let inner = &self.inner;
        match method {
            "initialize" => {
                lock(&inner.state).initialized = true;
                Ok(json!({
                    "serverInfo": { "name": "ruddr-claude-adapter", "version": "1" },
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
                "method {method} is not supported by the Claude adapter"
            ))),
        }
    }

    fn close(&self) {
        let (runtime, stream, runtimes) = {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return;
            }
            state.closed = true;
            if let Some(queue) = &state.queue {
                queue.close();
            }
            (state.runtime.clone(), state.stream.clone(), state.runtimes.clone())
        };
        if let Some(runtime) = runtime {
            runtime.close();
        }
        if let Some(stream) = stream {
            stream.wait(self.inner.settle_timeout);
        }
        // The SDK terminated every Claude process it still tracked when the
        // host process exited.
        for runtime in runtimes {
            runtime.shutdown();
        }
    }
}

// Claude configuration strings historically trim whitespace.
fn trimmed(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn required(value: Option<&Value>, label: &str) -> AResult<String> {
    trimmed(value).ok_or_else(|| AdapterError::invalid(format!("{label} is required")))
}

fn parse_effort(value: Option<&Value>) -> AResult<Option<String>> {
    match value {
        None => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s)) if EFFORTS.contains(&s.as_str()) => Ok(Some(s.clone())),
        _ => Err(AdapterError::invalid("Claude effort must be low, medium, high, xhigh, or max")),
    }
}

impl Inner {
    fn acquire_thread(&self, params: &Value, resumed: bool) -> AResult<Value> {
        let mut state = lock(&self.state);
        if !state.initialized {
            return Err(AdapterError::invalid("initialize must run first"));
        }
        if state.thread.is_some() {
            return Err(AdapterError::invalid("a thread is already configured"));
        }
        let input = record(params, "thread parameters")?;
        let id = if resumed {
            required(input.get("threadId"), "threadId")?
        } else {
            uuid_v4()
        };
        let cwd = required(input.get("cwd"), "cwd")?;
        let sandbox = Sandbox::parse(input.get("sandbox"))?;
        let effort = parse_effort(input.get("effort"))?;
        state.thread = Some(ThreadConfig {
            id: id.clone(),
            cwd,
            model: trimmed(input.get("model")),
            sandbox,
            effort,
            claude_path: trimmed(input.get("claudePath")),
            // The Go runner sent both fields; `ephemeral` alone also works.
            persist_session: match input.get("persistSession") {
                Some(value) => value != &Value::Bool(false),
                None => input.get("ephemeral") != Some(&Value::Bool(true)),
            },
            resumed,
        });
        Ok(json!({ "thread": { "id": id } }))
    }

    fn start_turn(self: &Arc<Self>, params: &Value) -> AResult<Value> {
        let mut state = lock(&self.state);
        let text = {
            let st = &mut *state;
            let Some(thread) = st.thread.as_mut() else {
                return Err(AdapterError::invalid("thread/start or thread/resume must run first"));
            };
            if st.turn.is_some() {
                return Err(AdapterError::invalid("a turn is already active"));
            }
            let input = record(params, "turn parameters")?;
            if required(input.get("threadId"), "threadId")? != thread.id {
                return Err(AdapterError::invalid("threadId does not match the configured Claude session"));
            }
            let text = read_text_input(input.get("input"))?;
            if let Some(effort) = parse_effort(input.get("effort"))? {
                thread.effort = Some(effort);
            }
            if st.turns_completed > 0 && !thread.persist_session {
                return Err(AdapterError::invalid("ephemeral Claude sessions support a single turn"));
            }
            text
        };
        if state.turns_completed > 0 {
            // Let the previous query's stream settle before starting a new one.
            let runtime = state.runtime.clone();
            let stream = state.stream.clone();
            drop(state);
            if let Some(runtime) = runtime {
                runtime.close();
            }
            let settled = stream.is_none_or(|stream| stream.wait(self.settle_timeout));
            if !settled {
                return Err(AdapterError::invalid(
                    "the previous Claude runtime did not settle; retry the turn after it closes",
                ));
            }
            state = lock(&self.state);
            state.runtime = None;
            state.queue = None;
            state.stream = None;
        }
        let thread = state.thread.as_ref().expect("thread is configured");
        let options = build_query_options(thread, &self.default_executable);
        let queue = Arc::new(PromptQueue::default());
        let (events, receiver) = mpsc::channel();
        let runtime = self.factory.create(&options, queue.clone(), events).map_err(AdapterError::failed)?;
        let turn = TurnState::new();
        let turn_id = turn.id.clone();
        let stream = Latch::new();
        state.generation += 1;
        state.turn = Some(turn);
        state.queue = Some(queue.clone());
        state.runtime = Some(runtime.clone());
        state.runtimes.push(runtime);
        state.stream = Some(stream.clone());
        let consumer = self.clone();
        let generation = state.generation;
        thread::spawn(move || consumer.consume(generation, receiver, stream));
        queue.push(cli::user_message(&text)).map_err(AdapterError::failed)?;
        // Emitted while holding the state lock, so no stream event of this
        // turn can overtake it.
        self.emit
            .emit(json!({ "method": "turn/started", "params": { "turn": { "id": turn_id, "status": "inProgress" } } }));
        Ok(json!({ "turn": { "id": turn_id, "status": "inProgress" } }))
    }

    fn steer_turn(&self, params: &Value) -> AResult<Value> {
        let state = lock(&self.state);
        let input = record(params, "steer parameters")?;
        let turn_id = require_active_turn(&state, input, "expectedTurnId")?;
        let text = read_text_input(input.get("input"))?;
        if let Some(queue) = &state.queue {
            queue.push(cli::user_message(&text)).map_err(AdapterError::failed)?;
        }
        // Codex reports a steer as its own userMessage item, which is what
        // puts it in the transcript. Nothing echoes it back here.
        let thread_id = state.thread.as_ref().map(|t| t.id.clone()).unwrap_or_default();
        self.emit.emit(json!({
            "method": "item/completed",
            "params": {
                "threadId": thread_id,
                "item": { "id": uuid_v7(), "type": "userMessage", "status": "completed", "text": text },
            },
        }));
        Ok(json!({ "turnId": turn_id }))
    }

    fn interrupt_turn(&self, params: &Value) -> AResult<Value> {
        let runtime = {
            let mut state = lock(&self.state);
            let input = record(params, "interrupt parameters")?;
            require_active_turn(&state, input, "turnId")?;
            if let Some(turn) = state.turn.as_mut() {
                turn.interrupt_requested = true;
            }
            if let Some(queue) = &state.queue {
                queue.close();
            }
            self.complete_turn(&mut state, "interrupted", None);
            state.runtime.clone()
        };
        if let Some(runtime) = runtime {
            runtime.interrupt();
            runtime.close();
        }
        Ok(json!({}))
    }

    fn consume(&self, generation: u64, receiver: Receiver<StreamItem>, done: Arc<Latch>) {
        let mut ended = None;
        for item in receiver.iter() {
            match item {
                StreamItem::Message(message) => {
                    let mut state = lock(&self.state);
                    if state.generation == generation {
                        self.handle_message(&mut state, &message);
                    }
                }
                StreamItem::End(error) => {
                    ended = Some(error);
                    break;
                }
            }
        }
        let error = ended
            .flatten()
            .unwrap_or_else(|| "Claude runtime stream ended before a terminal result".into());
        {
            let mut state = lock(&self.state);
            let status = (state.generation == generation)
                .then(|| {
                    state
                        .turn
                        .as_ref()
                        .map(|turn| if turn.interrupt_requested { "interrupted" } else { "failed" })
                })
                .flatten();
            if let Some(status) = status {
                self.complete_turn(&mut state, status, Some(error));
            }
        }
        done.set();
    }

    fn handle_message(&self, state: &mut State, message: &Value) {
        match message.get("type").and_then(Value::as_str) {
            Some("stream_event") => self.handle_stream_event(state, message),
            Some("user") => self.handle_tool_results(state, message),
            Some("result") => self.handle_result(state, message),
            _ => {}
        }
    }

    fn handle_stream_event(&self, state: &mut State, message: &Value) {
        if state.turn.is_none() {
            return;
        }
        let event = &message["event"];
        let parent = message
            .get("parent_tool_use_id")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .map(str::to_string);
        let kind = event.get("type").and_then(Value::as_str).unwrap_or_default();
        let index = event.get("index").and_then(Value::as_i64).unwrap_or(0);
        if parent.is_none() && (kind == "message_start" || kind == "message_delta") {
            let usage = if kind == "message_start" {
                let model = event["message"].get("model").and_then(Value::as_str).map(str::to_string);
                if state.context_model.is_some() && state.context_model != model {
                    state.context_window = 0.0;
                }
                state.context_model = model;
                state.last_usage = Some(Usage::default());
                &event["message"]["usage"]
            } else {
                &event["usage"]
            };
            if let Some(last) = state.last_usage.as_mut() {
                let take = |key: &str, slot: &mut f64| {
                    if let Some(value) = finite(usage.get(key)).filter(|v| *v >= 0.0) {
                        *slot = value;
                    }
                };
                take("input_tokens", &mut last.input_tokens);
                take("cache_creation_input_tokens", &mut last.cache_creation_input_tokens);
                take("cache_read_input_tokens", &mut last.cache_read_input_tokens);
                take("output_tokens", &mut last.output_tokens);
                self.emit_usage_snapshot(state);
            }
            return;
        }
        let delta = &event["delta"];
        let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or_default();
        if parent.is_some() && kind == "content_block_delta" && (delta_type == "text_delta" || delta_type == "thinking_delta") {
            return;
        }
        let thread_id = state.thread.as_ref().map(|t| t.id.clone()).unwrap_or_default();
        match kind {
            "content_block_start" => {
                let block = &event["content_block"];
                match block.get("type").and_then(Value::as_str).unwrap_or_default() {
                    "text" => {
                        let text = block.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
                        state
                            .turn
                            .as_mut()
                            .unwrap()
                            .text_blocks
                            .insert(index, TextBlock { id: uuid_v7(), text });
                    }
                    "thinking" => {
                        let text = block.get("thinking").and_then(Value::as_str).unwrap_or_default().to_string();
                        state
                            .turn
                            .as_mut()
                            .unwrap()
                            .thinking_blocks
                            .insert(index, TextBlock { id: uuid_v7(), text });
                    }
                    "tool_use" | "server_tool_use" | "mcp_tool_use" => {
                        self.flush_pending_text(state, "commentary");
                        let tool = ToolBlock {
                            index,
                            id: block.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
                            name: block.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                            input: block.get("input").and_then(Value::as_object).cloned().unwrap_or_default(),
                            partial_input: String::new(),
                            started_at: Instant::now(),
                            parent_tool_use_id: parent,
                        };
                        self.emit_tool("item/started", &tool, "inProgress", None);
                        let turn = state.turn.as_mut().unwrap();
                        turn.tools_by_index.insert(index, tool.id.clone());
                        turn.tools_by_id.insert(tool.id.clone(), tool);
                    }
                    _ => {}
                }
            }
            "content_block_delta" => match delta_type {
                "text_delta" => {
                    let text = delta.get("text").and_then(Value::as_str).unwrap_or_default();
                    let turn = state.turn.as_mut().unwrap();
                    let block = turn.text_blocks.entry(index).or_insert_with(|| TextBlock {
                        id: uuid_v7(),
                        text: String::new(),
                    });
                    block.text.push_str(text);
                    let item_id = block.id.clone();
                    // Codex streams partial assistant text this way and Ruddr
                    // readers render it live. The completed item that follows
                    // carries the same id and the authoritative text.
                    if !text.is_empty() {
                        self.emit.emit(json!({
                            "method": "item/agentMessage/delta",
                            "params": { "threadId": thread_id, "itemId": item_id, "delta": text },
                        }));
                    }
                }
                "thinking_delta" => {
                    let text = delta.get("thinking").and_then(Value::as_str).unwrap_or_default();
                    let turn = state.turn.as_mut().unwrap();
                    turn.thinking_blocks
                        .entry(index)
                        .or_insert_with(|| TextBlock {
                            id: uuid_v7(),
                            text: String::new(),
                        })
                        .text
                        .push_str(text);
                }
                "input_json_delta" => {
                    let turn = state.turn.as_mut().unwrap();
                    let Some(tool) = turn.tools_by_index.get(&index).and_then(|id| turn.tools_by_id.get_mut(id)) else {
                        return;
                    };
                    tool.partial_input
                        .push_str(delta.get("partial_json").and_then(Value::as_str).unwrap_or_default());
                    if let Some(parsed) = parse_json_record(&tool.partial_input) {
                        tool.input = parsed;
                        self.emit_tool("item/updated", tool, "inProgress", None);
                    }
                }
                _ => {}
            },
            "content_block_stop" => {
                let turn = state.turn.as_mut().unwrap();
                if let Some(text) = turn.text_blocks.remove(&index)
                    && !text.text.trim().is_empty()
                {
                    turn.pending_text.push(text);
                }
                if let Some(thinking) = turn.thinking_blocks.remove(&index)
                    && !thinking.text.trim().is_empty()
                {
                    self.emit.emit(json!({
                        "method": "item/completed",
                        "params": { "item": {
                            "id": thinking.id,
                            "type": "reasoning",
                            "status": "completed",
                            "summary": [{ "type": "summary_text", "text": thinking.text.trim() }],
                        } },
                    }));
                }
                if let Some(tool) = turn.tools_by_index.get(&index).and_then(|id| turn.tools_by_id.get_mut(id))
                    && !tool.partial_input.is_empty()
                {
                    if let Some(parsed) = parse_json_record(&tool.partial_input) {
                        tool.input = parsed;
                    }
                    self.emit_tool("item/updated", tool, "inProgress", None);
                }
            }
            _ => {}
        }
    }

    fn handle_tool_results(&self, state: &mut State, message: &Value) {
        let Some(turn) = state.turn.as_mut() else { return };
        if message
            .get("parent_tool_use_id")
            .and_then(Value::as_str)
            .is_some_and(|p| !p.is_empty())
        {
            return;
        }
        let Some(content) = message["message"].get("content").and_then(Value::as_array) else {
            return;
        };
        for block in content {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) else {
                continue;
            };
            let Some(tool) = turn.tools_by_id.remove(tool_use_id) else {
                continue;
            };
            turn.tools_by_index.remove(&tool.index);
            let output = extract_text(block.get("content"));
            let failed = block.get("is_error") == Some(&Value::Bool(true));
            self.emit_tool("item/completed", &tool, if failed { "failed" } else { "completed" }, Some(&output));
        }
    }

    fn handle_result(&self, state: &mut State, result: &Value) {
        if state.turn.is_none() {
            return;
        }
        if result.get("queued_turn_count").and_then(Value::as_f64).unwrap_or(0.0) > 0.0 {
            self.flush_pending_text(state, "commentary");
            return;
        }
        let status = result_status(result);
        if status == "completed" {
            let final_block = {
                let turn = state.turn.as_mut().expect("turn is active");
                let text = result.get("result").and_then(Value::as_str).unwrap_or_default();
                if turn.pending_text.is_empty()
                    && result.get("subtype").and_then(Value::as_str) == Some("success")
                    && !text.trim().is_empty()
                    && !turn.emitted_texts.contains(text.trim())
                {
                    turn.pending_text.push(TextBlock {
                        id: uuid_v7(),
                        text: text.to_string(),
                    });
                }
                turn.pending_text.pop()
            };
            self.flush_pending_text(state, "commentary");
            if let Some(block) = final_block {
                self.emit_agent_message(state, block, "final_answer");
            }
        } else {
            self.flush_pending_text(state, "commentary");
        }
        self.emit_token_usage(state, result);
        self.complete_turn(state, status, result_error(result));
    }

    // Accumulates usage and cost across turns and forwards them in Codex's
    // thread/tokenUsage/updated shape, plus a costUsd extension.
    fn emit_token_usage(&self, state: &mut State, result: &Value) {
        if state.thread.is_none() {
            return;
        }
        let per_model: Vec<&Value> = result
            .get("modelUsage")
            .and_then(Value::as_object)
            .map(|m| m.values().collect())
            .unwrap_or_default();
        let (mut input, mut cached, mut output) = (0.0, 0.0, 0.0);
        if !per_model.is_empty() {
            for usage in &per_model {
                input += number(usage.get("inputTokens"))
                    + number(usage.get("cacheCreationInputTokens"))
                    + number(usage.get("cacheReadInputTokens"));
                cached += number(usage.get("cacheReadInputTokens"));
                output += number(usage.get("outputTokens"));
            }
        } else if let Some(usage) = result.get("usage").filter(|u| u.is_object()) {
            input = number(usage.get("input_tokens"))
                + number(usage.get("cache_creation_input_tokens"))
                + number(usage.get("cache_read_input_tokens"));
            cached = number(usage.get("cache_read_input_tokens"));
            output = number(usage.get("output_tokens"));
        }
        state.totals[0] += input;
        state.totals[1] += cached;
        state.totals[2] += output;
        state.totals[3] += input + output;
        if let Some(cost) = finite(result.get("total_cost_usd")) {
            state.cost_total += cost;
        }
        let main = state
            .context_model
            .as_ref()
            .and_then(|model| result.get("modelUsage").and_then(|m| m.get(model)));
        let source = main.or_else(|| (per_model.len() == 1).then(|| per_model[0]));
        let window = number(source.and_then(|usage| usage.get("contextWindow")));
        if window > 0.0 {
            state.context_window = window;
        }
        self.emit_usage_snapshot(state);
    }

    fn emit_usage_snapshot(&self, state: &State) {
        let Some(thread) = &state.thread else { return };
        if state.totals[3] == 0.0 && state.cost_total == 0.0 && state.last_usage.is_none() {
            return;
        }
        let mut token_usage = json!({ "total": {
            "inputTokens": num(state.totals[0]),
            "cachedInputTokens": num(state.totals[1]),
            "outputTokens": num(state.totals[2]),
            "totalTokens": num(state.totals[3]),
        } });
        if let Some(last) = state.last_usage {
            let input = last.input_tokens + last.cache_creation_input_tokens + last.cache_read_input_tokens;
            token_usage["last"] = json!({
                "inputTokens": num(input),
                "cachedInputTokens": num(last.cache_read_input_tokens),
                "outputTokens": num(last.output_tokens),
                "totalTokens": num(input + last.output_tokens),
            });
        }
        if state.context_window > 0.0 {
            token_usage["modelContextWindow"] = num(state.context_window);
        }
        self.emit.emit(json!({
            "method": "thread/tokenUsage/updated",
            "params": { "threadId": thread.id, "tokenUsage": token_usage, "costUsd": num(state.cost_total) },
        }));
    }

    fn flush_pending_text(&self, state: &mut State, phase: &str) {
        let pending = state
            .turn
            .as_mut()
            .map(|turn| std::mem::take(&mut turn.pending_text))
            .unwrap_or_default();
        for block in pending {
            self.emit_agent_message(state, block, phase);
        }
    }

    fn emit_agent_message(&self, state: &mut State, block: TextBlock, phase: &str) {
        let Some(turn) = state.turn.as_mut() else { return };
        let text = block.text.trim().to_string();
        if text.is_empty() || turn.emitted_block_ids.contains(&block.id) {
            return;
        }
        turn.emitted_block_ids.insert(block.id.clone());
        turn.emitted_texts.insert(text.clone());
        self.emit.emit(json!({
            "method": "item/completed",
            "params": { "item": { "id": block.id, "type": "agentMessage", "status": "completed", "phase": phase, "text": text } },
        }));
    }

    fn emit_tool(&self, method: &str, tool: &ToolBlock, status: &str, output: Option<&str>) {
        self.emit
            .emit(json!({ "method": method, "params": { "item": normalize_tool(tool, status, output) } }));
    }

    fn complete_turn(&self, state: &mut State, status: &str, error: Option<String>) {
        let Some(turn) = state.turn.take() else { return };
        if let Some(queue) = &state.queue {
            queue.close();
        }
        state.turns_completed += 1;
        if let Some(thread) = state.thread.as_mut()
            && thread.persist_session
        {
            // The next turn resumes the persisted session; the CLI rejects a
            // reused --session-id.
            thread.resumed = true;
        }
        let mut turn_json = json!({ "id": turn.id, "status": status });
        if let Some(error) = error.filter(|e| !e.is_empty()) {
            turn_json["error"] = json!({ "code": -32000, "message": error });
        }
        self.emit
            .emit(json!({ "method": "turn/completed", "params": { "turn": turn_json } }));
    }
}

fn require_active_turn(state: &State, input: &Map<String, Value>, turn_key: &str) -> AResult<String> {
    let (Some(thread), Some(turn)) = (&state.thread, &state.turn) else {
        return Err(AdapterError::invalid("there is no active Claude turn"));
    };
    if required(input.get("threadId"), "threadId")? != thread.id {
        return Err(AdapterError::invalid("threadId does not match the active Claude session"));
    }
    if required(input.get(turn_key), turn_key)? != turn.id {
        return Err(AdapterError::invalid(format!("{turn_key} does not match the active Claude turn")));
    }
    Ok(turn.id.clone())
}

/// Maps a `result` message to a Ruddr turn status.
pub fn result_status(result: &Value) -> &'static str {
    let subtype = result.get("subtype").and_then(Value::as_str).unwrap_or_default();
    if subtype == "success" && result.get("is_error") != Some(&Value::Bool(true)) {
        return "completed";
    }
    if matches!(
        result.get("terminal_reason").and_then(Value::as_str),
        Some("aborted_tools" | "aborted_streaming")
    ) {
        return "interrupted";
    }
    let errors = if subtype == "success" {
        result.get("result").and_then(Value::as_str).unwrap_or_default().to_string()
    } else {
        result
            .get("errors")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    };
    let lower = errors.to_lowercase();
    if ["interrupt", "cancel", "abort"].iter().any(|word| lower.contains(word)) {
        "interrupted"
    } else {
        "failed"
    }
}

fn result_error(result: &Value) -> Option<String> {
    if result.get("subtype").and_then(Value::as_str) == Some("success") {
        return (result.get("is_error") == Some(&Value::Bool(true)))
            .then(|| result.get("result").and_then(Value::as_str).unwrap_or_default().to_string());
    }
    result
        .get("errors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find(|message| !message.starts_with("[ede_diagnostic]"))
        .map(str::to_string)
}

fn normalize_tool(tool: &ToolBlock, status: &str, output: Option<&str>) -> Value {
    let lower = tool.name.to_lowercase();
    let mut item = json!({
        "id": tool.id,
        "status": status,
        "toolName": tool.name,
        "input": tool.input,
        "durationMs": tool.started_at.elapsed().as_millis() as u64,
    });
    if let Some(output) = output.filter(|o| !o.is_empty()) {
        item["aggregatedOutput"] = json!(output);
    }
    if let Some(parent) = &tool.parent_tool_use_id {
        item["parentToolUseId"] = json!(parent);
    }
    let input = &tool.input;
    let field = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    if lower == "bash" || lower == "shell" {
        item["type"] = json!("commandExecution");
        item["command"] = json!(field("command").unwrap_or_else(|| tool.name.clone()));
        if let Some(cwd) = field("cwd") {
            item["cwd"] = json!(cwd);
        }
    } else if ["edit", "write", "notebookedit"].contains(&lower.as_str()) {
        item["type"] = json!("fileChange");
        item["command"] = json!(summarize_tool(&tool.name, input, true));
    } else if lower == "websearch" || lower == "webfetch" {
        item["type"] = json!("webSearch");
        if let Some(query) = field("query").or_else(|| field("url")) {
            item["query"] = json!(query);
        }
        item["command"] = json!(summarize_tool(&tool.name, input, true));
    } else {
        item["type"] = json!("toolCall");
        item["command"] = json!(summarize_tool(&tool.name, input, true));
    }
    item
}

/// `Name value` for the first descriptive input field, else `Name {json}`.
/// Claude's string helpers trim; Droid's keep whitespace.
pub fn summarize_tool(name: &str, input: &Map<String, Value>, trim: bool) -> String {
    for key in ["command", "file_path", "path", "query", "url", "pattern", "description"] {
        let value = input
            .get(key)
            .and_then(Value::as_str)
            .map(|s| if trim { s.trim() } else { s })
            .filter(|s| !s.is_empty());
        if let Some(value) = value {
            return format!("{name} {value}");
        }
    }
    let serialized = compact(input);
    if serialized == "{}" {
        name.to_string()
    } else {
        format!("{name} {serialized}")
    }
}

fn extract_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|entry| entry.get("text").and_then(Value::as_str))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn parse_json_record(text: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
