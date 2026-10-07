//! The Factory Droid adapter. Port of droid/runtime.ts.
//!
//! The adapter runs `droid exec --input-format stream-jsonrpc
//! --output-format stream-jsonrpc` and speaks Factory's JSON-RPC envelope
//! (`factoryApiVersion` 1.0.0). It is verified against droid 0.228.0,
//! 0.230.0, and 0.234.0, which speak Factory protocols 1.233.0, 1.241.0, and
//! 1.246.0. Factory publishes no schema; these behaviors were observed live:
//!
//! - `droid.load_session` works without `droid.initialize_session`. Calling
//!   initialize first would create a stray empty session, so resume and fork
//!   only load.
//! - `droid.fork_session` returns `newSessionId` but leaves the process on the
//!   source session, so the fork is loaded before any turn runs.
//! - A message sent during a turn is queued. Between tool calls it lands in
//!   the same Droid turn; near the end of a turn Droid first emits
//!   `agent_turn_completed` and then runs the message as a new Droid turn. A
//!   Ruddr turn therefore stays open while a steer is pending.
//! - `droid.add_user_message` keeps a caller-chosen `messageId`, so each
//!   turn's user message carries the Ruddr turn ID. `droid.execute_rewind` at
//!   a message, with no files to restore or delete, writes a new session that
//!   holds everything before that message and leaves files alone. Fork
//!   boundaries use it.
//! - `autoRejectPermissionRequests: true` ends the whole turn with reason
//!   `permission_rejected` instead of handing the model an error. At autonomy
//!   `off`, even `echo` needs permission.

use crate::child::hostname;
use crate::claude::summarize_tool;
use crate::protocol::{
    AResult, Adapter, AdapterError, Emit, lock, num, number, optional_string, read_text_input, record, required_string, text_content,
    uuid_v4,
};
use crate::rpc::{EventFn, Incoming, Labels, RpcProcess};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const FACTORY_API_VERSION: &str = "1.0.0";
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(30);
// Session start and load read settings, MCP configuration, and session files,
// so they get the same budget the Droid SDK uses.
const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(60);
// How long a completed Droid turn waits for a queued steer to start the next
// Droid turn before Ruddr gives up on it and ends the turn.
const STEER_PICKUP_TIMEOUT: Duration = Duration::from_secs(15);

/// Ruddr's approval policy is always "never", so Droid rejects every
/// permission request instead of waiting for an approver. The sandbox picks
/// how much Droid may do without asking.
fn autonomy_level(sandbox: &str) -> &'static str {
    match sandbox {
        "read-only" => "off",
        "danger-full-access" => "high",
        _ => "medium",
    }
}

fn parse_sandbox(value: Option<&Value>) -> AResult<String> {
    match value.and_then(Value::as_str) {
        Some(sandbox @ ("read-only" | "workspace-write" | "danger-full-access")) => Ok(sandbox.to_string()),
        _ => Err(AdapterError::invalid(
            "sandbox must be read-only, workspace-write, or danger-full-access",
        )),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DroidThread {
    pub id: String,
    pub cwd: String,
    pub executable: String,
    /// `None` only for a fork made by `ruddr thread fork`, which names no
    /// sandbox and leaves the forked session's settings alone.
    pub sandbox: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

pub trait DroidClient: Send + Sync {
    fn start(&self, executable: &str, cwd: &str, on_event: EventFn) -> AResult<()>;
    /// One request. `id` overrides the generated request ID; `timeout`
    /// overrides the default deadline.
    fn request(&self, method: &str, params: Value, id: Option<String>, timeout: Option<Duration>) -> AResult<Map<String, Value>>;
    fn close(&self);
}

struct Turn {
    serial: u64,
    id: String,
    interrupted: bool,
    settling: bool,
    // add_user_message request IDs that Droid has accepted but not yet turned
    // into a user message. A steer sent near the end of a Droid turn is
    // queued and runs as a new Droid turn, so its agent_turn_completed must
    // not end the Ruddr turn.
    pending_steers: HashSet<String>,
    deferred_reason: Option<String>,
    deferred_timer: u64,
    last_error: Option<String>,
    // The latest text-only assistant message, held back so the last one of
    // the turn is reported as the final answer.
    pending_text: Option<Map<String, Value>>,
}

struct Tool {
    name: String,
    input: Map<String, Value>,
    started_at: Instant,
}

#[derive(Default)]
struct State {
    initialized: bool,
    closed: bool,
    thread: Option<DroidThread>,
    turn: Option<Turn>,
    turn_serial: u64,
    timer_serial: u64,
    steer_serial: u64,
    // In first-seen order, like the TypeScript Map.
    tools: Vec<(String, Tool)>,
    latest_usage: Option<Map<String, Value>>,
    context_window: f64,
    context_used: Option<f64>,
}

enum Task {
    Event(Map<String, Value>),
    DeferredExpired { serial: u64, timer: u64 },
    SteerFailed { serial: u64 },
}

struct Inner {
    emit: Emit,
    client: Box<dyn DroidClient>,
    executable: String,
    steer_pickup: Duration,
    state: Mutex<State>,
    tasks: Mutex<Option<Sender<Task>>>,
}

pub struct DroidAdapter {
    inner: Arc<Inner>,
}

impl DroidAdapter {
    pub fn new(emit: Emit, executable: String) -> DroidAdapter {
        DroidAdapter::with_client(
            emit,
            executable,
            Box::new(SubprocessDroidClient::new(DEFAULT_RPC_TIMEOUT)),
            STEER_PICKUP_TIMEOUT,
        )
    }

    pub fn with_client(emit: Emit, executable: String, client: Box<dyn DroidClient>, steer_pickup: Duration) -> DroidAdapter {
        DroidAdapter {
            inner: Arc::new(Inner {
                emit,
                client,
                executable,
                steer_pickup,
                state: Mutex::new(State::default()),
                tasks: Mutex::new(None),
            }),
        }
    }
}

impl Adapter for DroidAdapter {
    fn dispatch(&self, method: &str, params: &Value) -> AResult<Value> {
        let inner = &self.inner;
        match method {
            "initialize" => {
                lock(&inner.state).initialized = true;
                Ok(json!({
                    "serverInfo": { "name": "ruddr-droid-adapter", "version": "1" },
                    "capabilities": { "experimentalApi": true },
                }))
            }
            "initialized" => Ok(Value::Null),
            "thread/start" => Inner::acquire_thread(inner, params, Mode::Start),
            "thread/resume" => Inner::acquire_thread(inner, params, Mode::Resume),
            "thread/fork" => Inner::acquire_thread(inner, params, Mode::Fork),
            "turn/start" => inner.start_turn(params),
            "turn/steer" => inner.steer_turn(params),
            "turn/interrupt" => inner.interrupt_turn(params),
            _ => Err(AdapterError::not_found(format!(
                "method {method} is not supported by the Droid adapter"
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
            if let Some(turn) = state.turn.as_mut() {
                turn.deferred_timer = 0;
            }
        }
        self.inner.client.close();
        lock(&self.inner.tasks).take();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Start,
    Resume,
    Fork,
}

impl Inner {
    fn acquire_thread(self: &Arc<Self>, params: &Value, mode: Mode) -> AResult<Value> {
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
        if mode == Mode::Start && input.get("ephemeral") == Some(&Value::Bool(true)) {
            return Err(AdapterError::invalid("Droid sessions always persist; --ephemeral is not supported"));
        }
        let boundary = match (optional_string(input.get("beforeTurnId")), optional_string(input.get("lastTurnId"))) {
            (Some(_), Some(_)) => {
                return Err(AdapterError::invalid(
                    "--fork-before-turn and --fork-through-turn are mutually exclusive",
                ));
            }
            (Some(turn), None) => Some(Boundary::Before(turn)),
            (None, Some(turn)) => Some(Boundary::Through(turn)),
            (None, None) => None,
        };
        if boundary.is_some() && mode != Mode::Fork {
            return Err(AdapterError::invalid("fork turn selectors need thread/fork"));
        }
        // `ruddr thread fork` sends only the thread ID: it forks from the
        // app-server's working directory and leaves the session settings
        // alone. `ruddr run` always sends cwd and sandbox.
        let bare_fork = mode == Mode::Fork && input.get("cwd").is_none() && input.get("sandbox").is_none();
        let cwd = if bare_fork {
            std::env::current_dir()
                .map(|dir| dir.to_string_lossy().into_owned())
                .map_err(|e| AdapterError::failed(e.to_string()))?
        } else {
            required_string(input.get("cwd"), "cwd")?
        };
        let mut thread = DroidThread {
            id: String::new(),
            cwd,
            executable: optional_string(input.get("providerPath")).unwrap_or_else(|| self.executable.clone()),
            sandbox: if bare_fork {
                None
            } else {
                Some(parse_sandbox(input.get("sandbox"))?)
            },
            model: optional_string(input.get("model")),
            effort: optional_string(input.get("effort")),
        };
        // Droid notifications are handled one at a time, in arrival order, so
        // a turn completion never overtakes the messages and tool results
        // before it.
        let (tasks, receiver) = mpsc::channel::<Task>();
        let worker = self.clone();
        thread::spawn(move || {
            for task in receiver {
                worker.run_task(task);
            }
        });
        *lock(&self.tasks) = Some(tasks.clone());
        let on_event: EventFn = Arc::new(move |event| {
            let _ = tasks.send(Task::Event(event));
        });
        self.client.start(&thread.executable, &thread.cwd, on_event)?;
        let mut settings = Map::new();
        if let Some(model) = &thread.model {
            settings.insert("modelId".into(), json!(model));
        }
        if let Some(effort) = &thread.effort {
            settings.insert("reasoningEffort".into(), json!(effort));
        }
        if let Some(sandbox) = &thread.sandbox {
            settings.insert("autonomyLevel".into(), json!(autonomy_level(sandbox)));
        }
        if mode == Mode::Start {
            let mut params = settings.clone();
            params.insert("machineId".into(), json!(hostname()));
            params.insert("cwd".into(), json!(thread.cwd));
            params.insert("autoRejectPermissionRequests".into(), json!(true));
            let result = self
                .client
                .request("droid.initialize_session", Value::Object(params), None, Some(DEFAULT_START_TIMEOUT))?;
            thread.id = required_string(result.get("sessionId"), "Droid session id")?;
        } else {
            let mut session = required_string(input.get("threadId"), "threadId")?;
            let loaded = self.load_session(&session)?;
            if mode == Mode::Fork {
                // Both fork_session and execute_rewind write a new session but
                // leave this process on the source, so the copy is loaded
                // before any turn.
                let rewind_at = match &boundary {
                    Some(boundary) => rewind_point(&loaded, boundary)?,
                    None => None,
                };
                let forked = match rewind_at {
                    Some(message) => self.client.request(
                        "droid.execute_rewind",
                        json!({ "sessionId": session, "messageId": message, "filesToRestore": [], "filesToDelete": [], "forkTitle": "Ruddr fork" }),
                        None,
                        Some(DEFAULT_START_TIMEOUT),
                    )?,
                    None => self.client.request("droid.fork_session", json!({}), None, None)?,
                };
                session = required_string(forked.get("newSessionId"), "Droid fork session id")?;
                self.load_session(&session)?;
            }
            if !bare_fork || !settings.is_empty() {
                self.client
                    .request("droid.update_session_settings", Value::Object(settings), None, None)?;
            }
            thread.id = session;
        }
        let id = thread.id.clone();
        lock(&self.state).thread = Some(thread);
        Ok(json!({ "thread": { "id": id } }))
    }

    fn load_session(&self, session: &str) -> AResult<Map<String, Value>> {
        self.client.request(
            "droid.load_session",
            json!({ "sessionId": session, "autoRejectPermissionRequests": true }),
            None,
            Some(DEFAULT_START_TIMEOUT),
        )
    }

    fn start_turn(&self, params: &Value) -> AResult<Value> {
        let (thread_id, current_effort) = {
            let state = lock(&self.state);
            let Some(thread) = &state.thread else {
                return Err(AdapterError::invalid("thread/start, thread/resume, or thread/fork must run first"));
            };
            if state.turn.is_some() {
                return Err(AdapterError::invalid("a turn is already active"));
            }
            (thread.id.clone(), thread.effort.clone())
        };
        let input = record(params, "turn parameters")?;
        require_thread(&thread_id, input)?;
        let text = read_text_input(input.get("input"))?;
        if let Some(effort) = optional_string(input.get("effort"))
            && Some(&effort) != current_effort.as_ref()
        {
            self.client
                .request("droid.update_session_settings", json!({ "reasoningEffort": effort }), None, None)?;
            if let Some(thread) = lock(&self.state).thread.as_mut() {
                thread.effort = Some(effort);
            }
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
                pending_steers: HashSet::new(),
                deferred_reason: None,
                deferred_timer: 0,
                last_error: None,
                pending_text: None,
            });
            self.emit.emit(json!({
                "method": "turn/started",
                "params": { "threadId": thread_id, "turn": { "id": id, "status": "inProgress" } },
            }));
            (id, serial)
        };
        // The turn ID doubles as the Droid message ID, which is what lets a
        // later fork name this turn as a boundary.
        if let Err(error) = self
            .client
            .request("droid.add_user_message", json!({ "messageId": turn_id, "text": text }), None, None)
        {
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
        let (thread_id, turn_id, serial, request_id, text) = {
            let mut state = lock(&self.state);
            let thread_id = require_thread_id(&state, input)?;
            require_turn(&mut state, input, "expectedTurnId")?;
            let text = read_text_input(input.get("input"))?;
            state.steer_serial += 1;
            let request_id = format!("ruddr-droid-steer-{}", state.steer_serial);
            let turn = state.turn.as_mut().expect("turn is active");
            turn.pending_steers.insert(request_id.clone());
            (thread_id, turn.id.clone(), turn.serial, request_id, text)
        };
        if let Err(error) = self.client.request(
            "droid.add_user_message",
            json!({ "messageId": format!("{STEER_MESSAGE_PREFIX}{}", uuid_v4()), "text": text }),
            Some(request_id.clone()),
            None,
        ) {
            if let Some(turn) = lock(&self.state).turn.as_mut().filter(|turn| turn.serial == serial) {
                turn.pending_steers.remove(&request_id);
            }
            self.enqueue(Task::SteerFailed { serial });
            return Err(error);
        }
        // Codex reports a steer as its own userMessage item, which is what
        // puts it in the transcript. Droid's own echo carries no marker that
        // it came from a steer, so the adapter emits it once Droid accepts it.
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
            let turn = require_turn(&mut state, input, "turnId")?;
            turn.interrupted = true;
        }
        self.client.request("droid.interrupt_session", json!({}), None, None)?;
        Ok(json!({}))
    }

    fn enqueue(&self, task: Task) {
        if let Some(tasks) = lock(&self.tasks).as_ref() {
            let _ = tasks.send(task);
        }
    }

    fn run_task(&self, task: Task) {
        match task {
            Task::Event(params) => self.handle_event(&params),
            Task::DeferredExpired { serial, timer } => {
                let expired = {
                    let mut state = lock(&self.state);
                    match state
                        .turn
                        .as_mut()
                        .filter(|turn| turn.serial == serial && turn.deferred_timer == timer && timer != 0)
                    {
                        Some(turn) => {
                            turn.pending_steers.clear();
                            true
                        }
                        None => false,
                    }
                };
                if expired {
                    self.complete_if_steers_settled(serial);
                }
            }
            Task::SteerFailed { serial } => self.complete_if_steers_settled(serial),
        }
    }

    fn handle_event(&self, params: &Map<String, Value>) {
        let Some(notification) = params.get("notification").and_then(Value::as_object) else {
            return;
        };
        let mut state = lock(&self.state);
        let Some(thread_id) = state.thread.as_ref().map(|t| t.id.clone()) else {
            return;
        };
        // Subagent sessions report through the same stream; only the Ruddr
        // session's own events belong to the turn.
        let session = optional_string(params.get("sessionId")).or_else(|| optional_string(notification.get("sessionId")));
        if session.is_some_and(|session| session != thread_id) {
            return;
        }
        let kind = notification.get("type").and_then(Value::as_str).unwrap_or_default();
        if kind == "session_token_usage_changed" {
            state.latest_usage = Some(notification.clone());
            state.context_used = None;
            self.emit_usage(&state);
            return;
        }
        let Some(turn) = state.turn.as_mut().filter(|turn| !turn.settling) else {
            return;
        };
        let serial = turn.serial;
        match kind {
            "assistant_text_delta" => {
                let (Some(message_id), Some(delta)) = (
                    optional_string(notification.get("messageId")),
                    optional_string(notification.get("textDelta")),
                ) else {
                    return;
                };
                self.emit.emit(json!({
                    "method": "item/agentMessage/delta",
                    "params": { "threadId": thread_id, "itemId": text_item_id(&message_id, notification.get("blockIndex")), "delta": delta },
                }));
            }
            "create_message" => {
                let Some(message) = notification.get("message").and_then(Value::as_object) else {
                    return;
                };
                match message.get("role").and_then(Value::as_str) {
                    Some("assistant") => self.handle_assistant_message(&mut state, &thread_id, message),
                    Some("user") => {
                        if let Some(request_id) = optional_string(notification.get("requestId"))
                            && turn.pending_steers.remove(&request_id)
                            && turn.pending_steers.is_empty()
                        {
                            // The steer started a new Droid turn; its completion
                            // ends the Ruddr turn instead of the deferred one.
                            clear_deferred(turn);
                        }
                    }
                    _ => {}
                }
            }
            "tool_call" => self.handle_tool_call(&mut state, &thread_id, notification),
            "tool_result" => self.handle_tool_result(&mut state, &thread_id, notification),
            "error" => {
                let message = optional_string(notification.get("message")).unwrap_or_else(|| "Droid reported an error".into());
                turn.last_error = Some(message.clone());
                self.emit
                    .emit(json!({ "method": "error", "params": { "error": { "message": message } } }));
            }
            "queued_messages_discarded" => {
                turn.pending_steers.clear();
                drop(state);
                self.complete_if_steers_settled(serial);
            }
            "agent_turn_completed" => {
                let reason = optional_string(notification.get("reason")).unwrap_or_else(|| "completed".into());
                if !turn.interrupted && !turn.pending_steers.is_empty() {
                    clear_deferred(turn);
                    turn.deferred_reason = Some(reason);
                    state.timer_serial += 1;
                    let timer = state.timer_serial;
                    state.turn.as_mut().expect("turn is current").deferred_timer = timer;
                    let tasks = lock(&self.tasks).clone();
                    let pickup = self.steer_pickup;
                    thread::spawn(move || {
                        thread::sleep(pickup);
                        if let Some(tasks) = tasks {
                            let _ = tasks.send(Task::DeferredExpired { serial, timer });
                        }
                    });
                    return;
                }
                drop(state);
                self.complete_turn(serial, &reason);
            }
            "ruddr_error" => {
                turn.last_error = Some(optional_string(notification.get("message")).unwrap_or_else(|| "Droid process failed".into()));
                drop(state);
                self.complete_turn(serial, "process_exit");
            }
            _ => {}
        }
    }

    fn complete_if_steers_settled(&self, serial: u64) {
        let reason = {
            let state = lock(&self.state);
            match state
                .turn
                .as_ref()
                .filter(|turn| turn.serial == serial && turn.pending_steers.is_empty())
            {
                Some(turn) => turn.deferred_reason.clone(),
                None => None,
            }
        };
        if let Some(reason) = reason {
            self.complete_turn(serial, &reason);
        }
    }

    fn handle_assistant_message(&self, state: &mut State, thread_id: &str, message: &Map<String, Value>) {
        let message_id = optional_string(message.get("id")).unwrap_or_else(uuid_v4);
        let content = message.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
        let uses_tools = content
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"));
        for (index, block) in content.iter().enumerate() {
            let Some(block) = block.as_object() else { continue };
            let kind = block.get("type").and_then(Value::as_str);
            let field = |key: &str| block.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
            if kind == Some("thinking")
                && let Some(thinking) = field("thinking")
            {
                self.flush_text(state, thread_id, "commentary");
                self.emit.emit(json!({
                    "method": "item/completed",
                    "params": { "threadId": thread_id, "item": {
                        "id": format!("{message_id}-thinking-{index}"), "type": "reasoning", "status": "completed",
                        "summary": [{ "type": "summary_text", "text": thinking }],
                    } },
                }));
            } else if kind == Some("text")
                && let Some(text) = field("text")
            {
                self.flush_text(state, thread_id, "commentary");
                let item = json!({ "id": text_item_id(&message_id, Some(&json!(index))), "type": "agentMessage", "status": "completed", "text": text })
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                // Text beside a tool call is narration; only a text-only
                // message can be the final answer.
                if uses_tools {
                    self.emit_agent_message(thread_id, item, "commentary");
                } else if let Some(turn) = state.turn.as_mut() {
                    turn.pending_text = Some(item);
                }
            }
        }
    }

    fn flush_text(&self, state: &mut State, thread_id: &str, phase: &str) {
        if let Some(item) = state.turn.as_mut().and_then(|turn| turn.pending_text.take()) {
            self.emit_agent_message(thread_id, item, phase);
        }
    }

    fn emit_agent_message(&self, thread_id: &str, mut item: Map<String, Value>, phase: &str) {
        item.insert("phase".into(), json!(phase));
        self.emit
            .emit(json!({ "method": "item/completed", "params": { "threadId": thread_id, "item": item } }));
    }

    fn handle_tool_call(&self, state: &mut State, thread_id: &str, notification: &Map<String, Value>) {
        let Some(tool_use) = notification.get("toolUse").and_then(Value::as_object) else {
            return;
        };
        let Some(id) = optional_string(tool_use.get("id")) else { return };
        let input = tool_use.get("input").and_then(Value::as_object).cloned().unwrap_or_default();
        // Droid streams a tool call's input: the same ID repeats as the
        // arguments fill in.
        if let Some((_, tool)) = state.tools.iter_mut().find(|(key, _)| *key == id) {
            tool.input = input;
            self.emit.emit(
                json!({ "method": "item/updated", "params": { "threadId": thread_id, "item": tool_item(&id, tool, "inProgress", None) } }),
            );
            return;
        }
        self.flush_text(state, thread_id, "commentary");
        let tool = Tool {
            name: optional_string(tool_use.get("name")).unwrap_or_else(|| "tool".into()),
            input,
            started_at: Instant::now(),
        };
        self.emit.emit(
            json!({ "method": "item/started", "params": { "threadId": thread_id, "item": tool_item(&id, &tool, "inProgress", None) } }),
        );
        state.tools.push((id, tool));
    }

    fn handle_tool_result(&self, state: &mut State, thread_id: &str, notification: &Map<String, Value>) {
        let Some(id) = optional_string(notification.get("toolUseId")) else {
            return;
        };
        let Some(position) = state.tools.iter().position(|(key, _)| *key == id) else {
            return;
        };
        let (_, tool) = state.tools.remove(position);
        let status = if notification.get("isError") == Some(&Value::Bool(true)) {
            "failed"
        } else {
            "completed"
        };
        let output = text_content(notification.get("content"), false);
        self.emit.emit(json!({ "method": "item/completed", "params": { "threadId": thread_id, "item": tool_item(&id, &tool, status, Some(&output)) } }));
    }

    fn complete_turn(&self, serial: u64, reason: &str) {
        let thread_id = {
            let mut state = lock(&self.state);
            let Some(thread_id) = state.thread.as_ref().map(|t| t.id.clone()) else {
                return;
            };
            let Some(turn) = state.turn.as_mut().filter(|turn| turn.serial == serial && !turn.settling) else {
                return;
            };
            turn.settling = true;
            clear_deferred(turn);
            self.flush_text(&mut state, &thread_id, "final_answer");
            thread_id
        };
        let stats = self.client.request("droid.get_context_stats", json!({}), None, None);
        let mut state = lock(&self.state);
        if let Ok(stats) = stats {
            let limit = number(stats.get("limit"));
            if limit != 0.0 {
                state.context_window = limit;
            }
            if let Some(used) = crate::protocol::finite(stats.get("used")) {
                state.context_used = Some(used);
            }
            self.emit_usage(&state);
        }
        for (id, tool) in std::mem::take(&mut state.tools) {
            self.emit.emit(
                json!({ "method": "item/completed", "params": { "threadId": thread_id, "item": tool_item(&id, &tool, "failed", None) } }),
            );
        }
        let Some(turn) = state.turn.take() else { return };
        let status = if turn.interrupted || reason == "cancelled" {
            "interrupted"
        } else if reason == "completed" {
            "completed"
        } else {
            "failed"
        };
        let mut turn_json = json!({ "id": turn.id, "status": status });
        if status == "failed" {
            let message = turn
                .last_error
                .clone()
                .unwrap_or_else(|| describe_reason(state.thread.as_ref(), reason));
            turn_json["error"] = json!({ "message": message });
        }
        self.emit
            .emit(json!({ "method": "turn/completed", "params": { "threadId": thread_id, "turn": turn_json } }));
    }

    fn emit_usage(&self, state: &State) {
        let (Some(thread), Some(latest)) = (&state.thread, &state.latest_usage) else {
            return;
        };
        let tokens = latest.get("tokenUsage").and_then(Value::as_object).cloned().unwrap_or_default();
        let last_call = latest.get("lastCallTokenUsage").and_then(Value::as_object);
        let input = number(tokens.get("inputTokens"));
        let output = number(tokens.get("outputTokens"));
        let cache_read = number(tokens.get("cacheReadTokens"));
        let cache_creation = number(tokens.get("cacheCreationTokens"));
        let total = input + output + cache_read + cache_creation;
        if total == 0.0 {
            return;
        }
        // Droid's inputTokens exclude cache reads; Codex counts them as input.
        let last = state.context_used.or_else(|| {
            last_call.map(|call| number(call.get("inputTokens")) + number(call.get("cacheReadTokens")) + number(call.get("outputTokens")))
        });
        let mut token_usage = json!({ "total": {
            "inputTokens": num(input + cache_read + cache_creation),
            "cachedInputTokens": num(cache_read),
            "outputTokens": num(output),
            "totalTokens": num(total),
        } });
        if let Some(last) = last {
            token_usage["last"] = json!({ "totalTokens": num(last) });
        }
        if state.context_window != 0.0 {
            token_usage["modelContextWindow"] = num(state.context_window);
        }
        self.emit
            .emit(json!({ "method": "thread/tokenUsage/updated", "params": { "threadId": thread.id, "tokenUsage": token_usage } }));
    }
}

/// Steer messages carry this prefix so a fork boundary never mistakes one for
/// the start of a turn.
const STEER_MESSAGE_PREFIX: &str = "steer-";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Boundary {
    /// `--fork-before-turn`: drop this turn and everything after it.
    Before(String),
    /// `--fork-through-turn`: keep history through this turn.
    Through(String),
}

/// The message to rewind at for a fork boundary, or `None` when the fork
/// keeps the whole session. `loaded` is the `droid.load_session` result,
/// whose `session.messages` lists the session in order.
fn rewind_point(loaded: &Map<String, Value>, boundary: &Boundary) -> AResult<Option<String>> {
    let messages = loaded
        .get("session")
        .and_then(|session| session.get("messages"))
        .and_then(Value::as_array)
        .ok_or_else(|| AdapterError::failed("Droid did not return the session's messages"))?;
    let turn = match boundary {
        Boundary::Before(turn) | Boundary::Through(turn) => turn,
    };
    let Some(index) = messages
        .iter()
        .position(|message| message.get("id").and_then(Value::as_str) == Some(turn.as_str()))
    else {
        let older = loaded.get("hasOlderMessages") == Some(&Value::Bool(true));
        return Err(AdapterError::invalid(format!(
            "turn {turn} is not in the Droid session{}; only turns Ruddr started after 0.6.7 can bound a fork",
            if older { " window Droid loaded" } else { "" }
        )));
    };
    match boundary {
        Boundary::Before(_) => Ok(Some(turn.clone())),
        Boundary::Through(_) => Ok(messages[index + 1..]
            .iter()
            .find(|message| starts_turn(message))
            .and_then(|message| message.get("id").and_then(Value::as_str).map(str::to_string))),
    }
}

/// Whether a Droid session message is the user message that starts a turn:
/// visible user text, not a tool result, hook record, injected context, or
/// Ruddr steer.
fn starts_turn(message: &Value) -> bool {
    let id = message.get("id").and_then(Value::as_str).unwrap_or("");
    if message.get("role").and_then(Value::as_str) != Some("user")
        || id.is_empty()
        || id.starts_with("context-")
        || id.starts_with(STEER_MESSAGE_PREFIX)
        || message.get("visibility").is_some_and(|visibility| !visibility.is_null())
    {
        return false;
    }
    let content = message.get("content").and_then(Value::as_array);
    content.is_some_and(|blocks| {
        blocks.iter().any(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            && !blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
    })
}

fn describe_reason(thread: Option<&DroidThread>, reason: &str) -> String {
    if reason == "permission_rejected"
        && let Some(sandbox) = thread.and_then(|t| t.sandbox.as_deref())
    {
        return format!(
            "Droid stopped at a tool call that needs approval; the {sandbox} sandbox runs Droid at autonomy {}",
            autonomy_level(sandbox)
        );
    }
    format!("Droid turn ended: {reason}")
}

fn clear_deferred(turn: &mut Turn) {
    turn.deferred_timer = 0;
    turn.deferred_reason = None;
}

fn text_item_id(message_id: &str, block_index: Option<&Value>) -> String {
    let index = block_index.and_then(Value::as_f64).filter(|n| n.is_finite()).unwrap_or(0.0);
    format!("{message_id}-text-{}", num(index))
}

fn tool_item(id: &str, tool: &Tool, status: &str, output: Option<&str>) -> Value {
    let lower = tool.name.to_lowercase();
    let mut item = json!({
        "id": id,
        "status": status,
        "toolName": tool.name,
        "input": tool.input,
        "durationMs": tool.started_at.elapsed().as_millis() as u64,
    });
    if let Some(output) = output.filter(|o| !o.is_empty()) {
        item["aggregatedOutput"] = json!(output);
    }
    let field = |key: &str| optional_string(tool.input.get(key));
    if lower == "execute" {
        item["type"] = json!("commandExecution");
        item["command"] = json!(field("command").unwrap_or_else(|| tool.name.clone()));
    } else if ["create", "edit", "multiedit", "applypatch"].contains(&lower.as_str()) {
        item["type"] = json!("fileChange");
        item["command"] = json!(summarize_tool(&tool.name, &tool.input, false));
    } else if lower == "websearch" || lower == "fetchurl" {
        item["type"] = json!("webSearch");
        if let Some(query) = field("query").or_else(|| field("url")) {
            item["query"] = json!(query);
        }
        item["command"] = json!(summarize_tool(&tool.name, &tool.input, false));
    } else {
        item["type"] = json!("toolCall");
        item["command"] = json!(summarize_tool(&tool.name, &tool.input, false));
    }
    item
}

fn require_thread(thread_id: &str, input: &Map<String, Value>) -> AResult<()> {
    if required_string(input.get("threadId"), "threadId")? != thread_id {
        return Err(AdapterError::invalid("threadId does not match the configured Droid session"));
    }
    Ok(())
}

fn require_thread_id(state: &State, input: &Map<String, Value>) -> AResult<String> {
    let thread_id = state.thread.as_ref().map(|t| t.id.clone()).unwrap_or_default();
    require_thread(&thread_id, input)?;
    Ok(thread_id)
}

fn require_turn<'a>(state: &'a mut State, input: &Map<String, Value>, turn_key: &str) -> AResult<&'a mut Turn> {
    require_thread_id(state, input)?;
    let Some(turn) = state.turn.as_mut() else {
        return Err(AdapterError::invalid("there is no active Droid turn"));
    };
    if turn.settling {
        return Err(AdapterError::invalid("the active Droid turn is settling"));
    }
    if required_string(input.get(turn_key), turn_key)? != turn.id {
        return Err(AdapterError::invalid(format!("{turn_key} does not match the active Droid turn")));
    }
    Ok(turn)
}

/// Runs `droid exec` in stream JSON-RPC mode.
pub struct SubprocessDroidClient {
    rpc_timeout: Duration,
    process: Mutex<Option<Arc<RpcProcess>>>,
    sequence: AtomicU64,
}

impl SubprocessDroidClient {
    pub fn new(rpc_timeout: Duration) -> SubprocessDroidClient {
        SubprocessDroidClient {
            rpc_timeout,
            process: Mutex::new(None),
            sequence: AtomicU64::new(0),
        }
    }
}

fn envelope(kind: &str, fields: Value) -> Value {
    let mut message = json!({ "jsonrpc": "2.0", "factoryApiVersion": FACTORY_API_VERSION, "type": kind });
    if let (Some(message), Some(fields)) = (message.as_object_mut(), fields.as_object()) {
        message.extend(fields.clone());
    }
    message
}

fn classify(message: &Map<String, Value>) -> Incoming {
    match message.get("type").and_then(Value::as_str) {
        Some("response") => {
            let id = match message.get("id") {
                Some(Value::String(id)) => id.clone(),
                Some(Value::Number(id)) => id.to_string(),
                _ => return Incoming::Skip,
            };
            let outcome = match message.get("error").and_then(Value::as_object) {
                Some(error) => Err(optional_string(error.get("message")).unwrap_or_else(|| "Droid request failed".into())),
                None => Ok(message.get("result").and_then(Value::as_object).cloned().unwrap_or_default()),
            };
            Incoming::Response(id, outcome)
        }
        // Ruddr runs unattended, so Droid's interactive requests are declined
        // at once instead of being left to hang the turn.
        Some("request") => {
            let id = message.get("id").cloned().unwrap_or(Value::Null);
            let method = message.get("method").and_then(Value::as_str).unwrap_or("undefined");
            Incoming::Reply(match method {
                "droid.request_permission" => envelope("response", json!({ "id": id, "result": { "selectedOption": "cancel" } })),
                "droid.ask_user" => envelope("response", json!({ "id": id, "result": { "cancelled": true, "answers": [] } })),
                other => envelope(
                    "response",
                    json!({ "id": id, "error": { "code": -32601, "message": format!("Ruddr does not answer {other}") } }),
                ),
            })
        }
        _ => match (
            message.get("method").and_then(Value::as_str),
            message.get("params").and_then(Value::as_object),
        ) {
            (Some("droid.session_notification"), Some(params)) => Incoming::Event(params.clone()),
            _ => Incoming::Skip,
        },
    }
}

fn failure_event(message: &str) -> Map<String, Value> {
    json!({ "notification": { "type": "ruddr_error", "message": message } })
        .as_object()
        .cloned()
        .unwrap_or_default()
}

impl DroidClient for SubprocessDroidClient {
    fn start(&self, executable: &str, cwd: &str, on_event: EventFn) -> AResult<()> {
        // In stream JSON-RPC mode Droid takes its session settings from
        // initialize_session and update_session_settings, not from flags.
        let mut command = Command::new(executable);
        command
            .args(["exec", "--input-format", "stream-jsonrpc", "--output-format", "stream-jsonrpc"])
            .current_dir(cwd);
        let labels = Labels {
            prefix: "Droid",
            process: "Droid process",
            client: "Droid client",
            message: "Droid JSON-RPC message",
        };
        let process = RpcProcess::spawn(command, labels, self.rpc_timeout, classify, on_event, failure_event).map_err(|error| {
            AdapterError::failed(if error.kind() == std::io::ErrorKind::NotFound {
                format!("Droid executable not found at {executable}; install droid or pass --droid-path")
            } else {
                format!("start Droid: {error}")
            })
        })?;
        *lock(&self.process) = Some(process);
        Ok(())
    }

    fn request(&self, method: &str, params: Value, id: Option<String>, timeout: Option<Duration>) -> AResult<Map<String, Value>> {
        let process = lock(&self.process)
            .clone()
            .ok_or_else(|| AdapterError::failed("Droid process is not running"))?;
        let id = id.unwrap_or_else(|| format!("ruddr-droid-{}", self.sequence.fetch_add(1, Ordering::SeqCst) + 1));
        let timeout = timeout.unwrap_or(self.rpc_timeout);
        let message = envelope("request", json!({ "id": id, "method": method, "params": params }));
        let timeout_message = format!("Droid {method} timed out after {}ms", timeout.as_millis());
        process.call(&id, &message, timeout, timeout_message).map_err(AdapterError::failed)
    }

    fn close(&self) {
        if let Some(process) = lock(&self.process).take() {
            process.close(Duration::from_secs(2));
        }
    }
}

#[cfg(test)]
mod tests;
