//! Drives the `claude` CLI over its stream-json protocol. This replaces
//! `@anthropic-ai/claude-agent-sdk`, whose `query()` spawned the same CLI.
//!
//! Everything here mirrors the SDK's bundled `sdk.mjs` (version 0.3.245):
//!
//! - The argv comes from `ProcessTransport.initialize()`: the fixed
//!   `--output-format stream-json --verbose --input-format stream-json`
//!   prefix, then one flag per option the TypeScript adapter set, in the SDK's
//!   order. The SDK leaves print mode implicit (stdout is a pipe); Ruddr adds
//!   `-p` because `claude --help` documents both stream-json formats as
//!   print-only.
//! - The environment follows `query()`: `CLAUDE_CODE_ENTRYPOINT=sdk-ts` and
//!   `CLAUDE_AGENT_SDK_VERSION` unless already set, with `NODE_OPTIONS` and
//!   `DEBUG` removed.
//! - The control protocol follows `Query`: an `initialize` control request
//!   first, user messages as `{"type":"user",...}` lines, `can_use_tool`
//!   requests answered with the permission decision plus `toolUseID`, and the
//!   SDK's replies to every other request subtype.
//! - Closing the prompt queue ends stdin, but with a permission callback the
//!   SDK waits for the first `result` first (`streamInput` and
//!   `waitForFirstResult`). Closing the query ends stdin at once, ends the
//!   message stream, and terminates the process after 2 s (SIGTERM, then
//!   SIGKILL 5 s later), as `ProcessTransport.close()` does.

use super::{PromptQueue, QueryFactory, QueryOptions, Sandbox, StreamItem};
use crate::child::{ChildProcess, describe_exit};
use crate::protocol::{LineReader, MAX_LINE_BYTES, lock};
use serde_json::{Map, Value, json};
use std::process::Command;
use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// The Agent SDK release the stream-json protocol was ported from.
pub const SDK_VERSION: &str = "0.3.245";
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);

/// The SDK's "claude_code" system prompt preset sends no `systemPrompt` in
/// `initialize`, so the CLI keeps its default prompt. `forwardSubagentText`
/// false keeps subagent text out of the main stream.
pub fn initialize_request() -> Value {
    json!({ "subtype": "initialize", "forwardSubagentText": false })
}

/// The CLI arguments for one query, after the executable.
pub fn claude_argv(options: &QueryOptions) -> Vec<String> {
    let mut args: Vec<String> = ["-p", "--output-format", "stream-json", "--verbose", "--input-format", "stream-json"]
        .map(String::from)
        .to_vec();
    // thinking: { type: "adaptive" }
    args.extend(["--thinking".into(), "adaptive".into()]);
    if let Some(effort) = &options.effort {
        args.extend(["--effort".into(), effort.clone()]);
    }
    if let Some(model) = &options.model {
        args.extend(["--model".into(), model.clone()]);
    }
    if options.can_use_tool() {
        args.extend(["--permission-prompt-tool".into(), "stdio".into()]);
    }
    if let Some(resume) = &options.resume {
        args.push(format!("--resume={resume}"));
    }
    args.push("--setting-sources=user,project,local".into());
    args.extend(["--permission-mode".into(), options.permission_mode.into()]);
    if options.permission_mode == "bypassPermissions" {
        args.push("--allow-dangerously-skip-permissions".into());
    }
    args.push("--include-partial-messages".into());
    if let Some(session_id) = &options.session_id {
        args.push(format!("--session-id={session_id}"));
    }
    if !options.persist_session {
        args.push("--no-session-persistence".into());
    }
    if options.sandbox == Sandbox::WorkspaceWrite {
        args.extend(["--settings".into(), sandbox_settings(&options.cwd)]);
    }
    args
}

/// The `--settings` JSON the SDK builds from the `sandbox` option, with the
/// keys in the SDK's order.
pub fn sandbox_settings(cwd: &str) -> String {
    let cwd = serde_json::to_string(cwd).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"{{"sandbox":{{"enabled":true,"failIfUnavailable":true,"autoAllowBashIfSandboxed":true,"allowUnsandboxedCommands":false,"filesystem":{{"allowWrite":[{cwd}]}}}}}}"#
    )
}

/// The decision for a `can_use_tool` request. Ruddr has no approval surface:
/// it denies everything except Bash inside Claude's workspace command sandbox.
pub fn permission_decision(sandbox: Sandbox, tool_name: &str, input: &Map<String, Value>, request: &Map<String, Value>) -> Value {
    let deny = |message: &str| json!({ "behavior": "deny", "message": message });
    if tool_name == "AskUserQuestion" {
        return deny("Ruddr cannot answer interactive questions; proceed with best judgment.");
    }
    if tool_name == "Bash" && sandbox == Sandbox::WorkspaceWrite {
        if input.get("dangerouslyDisableSandbox") == Some(&Value::Bool(true)) {
            return deny("Ruddr denied a request to run Bash outside the workspace sandbox.");
        }
        if request
            .get("blocked_path")
            .and_then(Value::as_str)
            .is_some_and(|path| !path.is_empty())
        {
            return deny("Ruddr denied Bash access outside the workspace sandbox.");
        }
        if request.get("matched_ask_rule").is_some_and(Value::is_object) {
            return deny("Ruddr cannot override an explicit interactive approval rule.");
        }
        // The CLI can ask about commands its classifier cannot auto-allow. The
        // active sandbox still confines them.
        return json!({ "behavior": "allow" });
    }
    deny("Ruddr has no interactive approval surface for this operation.")
}

/// The reply to a control request the CLI sends. `Err` carries the text of an
/// error response.
pub fn answer_control_request(sandbox: Sandbox, can_use_tool: bool, request: &Map<String, Value>) -> Result<Value, String> {
    let subtype = request.get("subtype").and_then(Value::as_str).unwrap_or_default();
    match subtype {
        "can_use_tool" => {
            if !can_use_tool {
                return Err("canUseTool callback is not provided.".into());
            }
            let tool_name = request.get("tool_name").and_then(Value::as_str).unwrap_or_default();
            let input = request.get("input").and_then(Value::as_object).cloned().unwrap_or_default();
            let mut decision = permission_decision(sandbox, tool_name, &input, request);
            if let (Some(object), Some(id)) = (decision.as_object_mut(), request.get("tool_use_id")) {
                object.insert("toolUseID".into(), id.clone());
            }
            Ok(decision)
        }
        // The SDK declines elicitations when no handler is registered.
        "elicitation" => Ok(json!({ "action": "decline" })),
        "hook_callback" => Err(format!(
            "No hook callback found for ID: {}",
            request.get("callback_id").and_then(Value::as_str).unwrap_or_default()
        )),
        "mcp_message" => Err(format!(
            "SDK MCP server not found: {}",
            request.get("server_name").and_then(Value::as_str).unwrap_or_default()
        )),
        "oauth_token_refresh" => Err("getOAuthToken callback is not provided.".into()),
        "host_auth_token_refresh" => Err("getHostAuthToken callback is not provided.".into()),
        // The SDK leaves dialogs unanswered for another client to settle.
        // Ruddr has no other client, so it rejects them instead of hanging.
        "request_user_dialog" => Err("Ruddr cannot answer interactive dialogs".into()),
        other => Err(format!("Unsupported control request subtype: {other}")),
    }
}

/// The SDK's user-message envelope for a prompt or steer.
pub fn user_message(text: &str) -> Value {
    json!({
        "type": "user",
        "session_id": "",
        "parent_tool_use_id": null,
        "origin": { "kind": "human" },
        "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
    })
}

/// Spawns one `claude` process per query.
pub struct CliFactory;

impl QueryFactory for CliFactory {
    fn create(
        &self,
        options: &QueryOptions,
        prompt: Arc<PromptQueue>,
        events: Sender<StreamItem>,
    ) -> Result<Arc<dyn super::ClaudeQuery>, String> {
        let query = CliQuery::spawn(options, events)?;
        let pump = query.clone();
        thread::spawn(move || {
            while let Some(message) = prompt.next() {
                if pump.is_closed() {
                    break;
                }
                let _ = pump.child.enqueue(serde_json::to_vec(&message).unwrap_or_default());
            }
            pump.end_input();
        });
        Ok(query)
    }
}

#[derive(Default)]
struct QueryState {
    closed: bool,
    input_ended: bool,
    first_result: bool,
    events: Option<Sender<StreamItem>>,
    last_error_result: Option<String>,
    initializing: Option<(String, SyncSender<Result<(), String>>)>,
}

pub struct CliQuery {
    child: Arc<ChildProcess>,
    sandbox: Sandbox,
    can_use_tool: bool,
    state: Mutex<QueryState>,
}

impl CliQuery {
    fn spawn(options: &QueryOptions, events: Sender<StreamItem>) -> Result<Arc<CliQuery>, String> {
        Self::spawn_with_timeout(options, events, INITIALIZE_TIMEOUT)
    }

    fn spawn_with_timeout(options: &QueryOptions, events: Sender<StreamItem>, timeout: Duration) -> Result<Arc<CliQuery>, String> {
        let mut command = Command::new(&options.executable);
        command.args(claude_argv(options)).current_dir(&options.cwd);
        if std::env::var_os("CLAUDE_CODE_ENTRYPOINT").is_none() {
            command.env("CLAUDE_CODE_ENTRYPOINT", "sdk-ts");
        }
        if std::env::var_os("CLAUDE_AGENT_SDK_VERSION").is_none() {
            command.env("CLAUDE_AGENT_SDK_VERSION", SDK_VERSION);
        }
        command.env_remove("NODE_OPTIONS");
        if truthy(std::env::var("DEBUG_CLAUDE_AGENT_SDK").ok().as_deref()) {
            command.env("DEBUG", "1");
        } else {
            command.env_remove("DEBUG");
        }
        let (child, stdout) = ChildProcess::spawn(command).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "Claude Code executable not found at {}; install claude or pass --claude-path",
                    options.executable
                )
            } else {
                format!("Failed to spawn Claude Code process: {error}")
            }
        })?;
        let query = Arc::new(CliQuery {
            child,
            sandbox: options.sandbox,
            can_use_tool: options.can_use_tool(),
            state: Mutex::new(QueryState {
                events: Some(events),
                ..Default::default()
            }),
        });
        let id = ruddr_core::fsutil::random_hex(6);
        let (reply, initialized) = mpsc::sync_channel(1);
        lock(&query.state).initializing = Some((id.clone(), reply));
        let reader = query.clone();
        thread::spawn(move || reader.read(stdout));
        let request = json!({ "request_id": id, "type": "control_request", "request": initialize_request() });
        let started = std::time::Instant::now();
        let outcome = query
            .child
            .write(serde_json::to_vec(&request).unwrap_or_default(), timeout)
            .and_then(|()| {
                initialized
                    .recv_timeout(timeout.saturating_sub(started.elapsed()))
                    .unwrap_or_else(|_| {
                        Err(format!(
                            "Claude initialize did not return a successful response within {}ms",
                            timeout.as_millis()
                        ))
                    })
            });
        if let Err(error) = outcome {
            lock(&query.state).initializing.take();
            super::ClaudeQuery::close(query.as_ref());
            super::ClaudeQuery::shutdown(query.as_ref());
            return Err(error);
        }
        Ok(query)
    }

    fn is_closed(&self) -> bool {
        lock(&self.state).closed
    }

    fn send_control_request(&self, request: Value) {
        let id = ruddr_core::fsutil::random_hex(6);
        let line = json!({ "request_id": id, "type": "control_request", "request": request });
        let _ = self.child.enqueue(serde_json::to_vec(&line).unwrap_or_default());
    }

    fn end_input(&self) {
        let mut state = lock(&self.state);
        state.input_ended = true;
        if !self.can_use_tool || state.first_result {
            self.child.close_stdin();
        }
    }

    fn forward(&self, item: StreamItem) {
        let state = lock(&self.state);
        if !state.closed
            && let Some(events) = &state.events
        {
            let _ = events.send(item);
        }
    }

    fn read(self: Arc<Self>, stdout: std::process::ChildStdout) {
        let mut lines = LineReader::new(stdout, MAX_LINE_BYTES);
        let failure = loop {
            let line = match lines.next_line() {
                Ok(Some(line)) => line,
                Ok(None) => break None,
                Err(error) => break Some(error.to_string()),
            };
            if line.trim().is_empty() {
                continue;
            }
            // The SDK skips stdout lines that are not JSON.
            let Ok(Value::Object(message)) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match message.get("type").and_then(Value::as_str) {
                Some("control_response") => {
                    let response = message.get("response").and_then(Value::as_object);
                    if let Some(response) = response {
                        let mut state = lock(&self.state);
                        if state
                            .initializing
                            .as_ref()
                            .is_some_and(|(id, _)| response.get("request_id").and_then(Value::as_str) == Some(id))
                        {
                            let (_, reply) = state.initializing.take().unwrap();
                            let result = match response.get("subtype").and_then(Value::as_str) {
                                Some("success") => Ok(()),
                                _ => Err(format!(
                                    "Claude initialize failed: {}",
                                    response.get("error").and_then(Value::as_str).unwrap_or("invalid response")
                                )),
                            };
                            let _ = reply.send(result);
                            continue;
                        }
                    }
                    if let Some(response) = response.filter(|r| r.get("subtype").and_then(Value::as_str) == Some("error")) {
                        eprintln!(
                            "ruddr: claude control request failed: {}",
                            response.get("error").and_then(Value::as_str).unwrap_or("unknown error")
                        );
                    }
                }
                Some("control_request") => self.answer(&message),
                Some("control_cancel_request" | "keep_alive" | "transcript_mirror") => {}
                kind => {
                    if kind == Some("result") {
                        self.note_result(&message);
                    } else if !(kind == Some("system") && message.get("subtype").and_then(Value::as_str) == Some("session_state_changed")) {
                        lock(&self.state).last_error_result = None;
                    }
                    self.forward(StreamItem::Message(Value::Object(message)));
                }
            }
        };
        if let Some((_, reply)) = lock(&self.state).initializing.take() {
            let _ = reply.send(Err(failure
                .clone()
                .unwrap_or_else(|| "Claude output closed before initialization completed".into())));
        }
        // Like the SDK, the stream ends with the process: a failed exit or a
        // read error ends it with an error.
        let error = failure.or_else(|| {
            if !self.child.wait_timeout(Duration::from_secs(5)) {
                return None;
            }
            let status = self.child.try_status()?;
            let exit = describe_exit(status)?;
            Some(match lock(&self.state).last_error_result.clone() {
                Some(text) => format!("Claude Code returned an error result: {text}"),
                None => format!("Claude Code process {exit}"),
            })
        });
        self.forward(StreamItem::End(error));
        lock(&self.state).events.take();
    }

    fn note_result(&self, message: &Map<String, Value>) {
        let mut state = lock(&self.state);
        state.first_result = true;
        state.last_error_result = if message.get("is_error") == Some(&Value::Bool(true)) {
            if message.get("subtype").and_then(Value::as_str) == Some("success") {
                message.get("result").and_then(Value::as_str).map(str::to_string)
            } else {
                let errors: Vec<String> = message
                    .get("errors")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|e| e.trim().to_string())
                    .filter(|e| !e.is_empty())
                    .collect();
                Some(errors.join("; "))
            }
            .filter(|text| !text.is_empty())
        } else {
            None
        };
        if state.input_ended {
            self.child.close_stdin();
        }
    }

    fn answer(&self, message: &Map<String, Value>) {
        let request_id = message.get("request_id").cloned().unwrap_or(Value::Null);
        let request = message.get("request").and_then(Value::as_object).cloned().unwrap_or_default();
        let response = match answer_control_request(self.sandbox, self.can_use_tool, &request) {
            Ok(response) => json!({ "subtype": "success", "request_id": request_id, "response": response }),
            Err(error) => json!({ "subtype": "error", "request_id": request_id, "error": error }),
        };
        if lock(&self.state).closed {
            return;
        }
        let line = json!({ "type": "control_response", "response": response });
        let _ = self.child.enqueue(serde_json::to_vec(&line).unwrap_or_default());
    }
}

impl super::ClaudeQuery for CliQuery {
    fn interrupt(&self) {
        if !self.is_closed() {
            self.send_control_request(json!({ "subtype": "interrupt" }));
        }
    }

    fn close(&self) {
        let events = {
            let mut state = lock(&self.state);
            if state.closed {
                return;
            }
            state.closed = true;
            state.events.take()
        };
        if let Some(events) = events {
            let _ = events.send(StreamItem::End(None));
        }
        self.child.close_stdin();
        let child = self.child.clone();
        thread::spawn(move || {
            if !child.wait_timeout(Duration::from_secs(2)) {
                child.terminate();
                if !child.wait_timeout(Duration::from_secs(5)) {
                    child.kill();
                }
            }
        });
    }

    fn shutdown(&self) {
        self.child.shut_down(Duration::ZERO);
    }
}

fn truthy(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testing::FakeDir;

    #[test]
    fn initialization_errors_eof_and_missing_matching_replies_reject_and_reap_the_query() {
        for (mode, expected) in [
            ("error", "initialize failed"),
            ("eof", "before initialization"),
            ("wrong-id", "within"),
        ] {
            let fake = FakeDir::new();
            fake.write("initialize_mode", mode);
            let options = QueryOptions {
                executable: fake.script("claude"),
                cwd: fake.dir.to_string_lossy().into_owned(),
                model: None,
                effort: None,
                sandbox: Sandbox::ReadOnly,
                permission_mode: "plan",
                persist_session: true,
                resume: None,
                session_id: None,
            };
            let (events, _receiver) = mpsc::channel();
            let started = std::time::Instant::now();
            let error = match CliQuery::spawn_with_timeout(&options, events, Duration::from_secs(2)) {
                Ok(query) => {
                    super::super::ClaudeQuery::shutdown(query.as_ref());
                    panic!("accepted a query with initialization mode {mode}");
                }
                Err(error) => error,
            };
            assert!(error.contains(expected), "{mode}: {error}");
            assert!(started.elapsed() < Duration::from_secs(5));
            let pid: i64 = fake.read("pid").trim().parse().unwrap();
            assert!(!ruddr_core::process::alive(pid), "{mode} left Claude running");
            assert!(fake.records("stdin.jsonl").iter().all(|line| line["type"] != "user"));
        }
    }
}
