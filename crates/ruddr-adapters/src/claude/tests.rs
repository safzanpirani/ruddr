//! Port of claude/runtime.test.ts and claude/app-server.test.ts, plus tests
//! of the stream-json transport against a fake `claude` CLI.

use super::cli::{self, claude_argv, permission_decision, sandbox_settings, user_message};
use super::*;
use crate::protocol::tests::Collector;
use crate::protocol::{Sink, handle, serve};
use serde_json::{Value, json};
use std::sync::mpsc::Sender;

struct FakeStream {
    options: QueryOptions,
    prompt: Arc<PromptQueue>,
    events: Mutex<Option<Sender<StreamItem>>>,
    ignore_close: bool,
}

impl FakeStream {
    fn push(&self, message: Value) {
        if let Some(events) = lock(&self.events).as_ref() {
            events.send(StreamItem::Message(message)).unwrap();
        }
    }
}

impl ClaudeQuery for FakeStream {
    fn close(&self) {
        if self.ignore_close {
            return;
        }
        if let Some(events) = lock(&self.events).take() {
            let _ = events.send(StreamItem::End(None));
        }
    }
}

#[derive(Default)]
struct FakeFactory {
    streams: Mutex<Vec<Arc<FakeStream>>>,
    fail_first: bool,
    ignore_close: bool,
    attempts: Mutex<u32>,
}

impl FakeFactory {
    fn stream(&self, index: usize) -> Arc<FakeStream> {
        lock(&self.streams)[index].clone()
    }
    fn last(&self) -> Arc<FakeStream> {
        lock(&self.streams).last().cloned().expect("a query was created")
    }
    fn count(&self) -> usize {
        lock(&self.streams).len()
    }
}

impl QueryFactory for Arc<FakeFactory> {
    fn create(&self, options: &QueryOptions, prompt: Arc<PromptQueue>, events: Sender<StreamItem>) -> Result<Arc<dyn ClaudeQuery>, String> {
        let mut attempts = lock(&self.attempts);
        *attempts += 1;
        if self.fail_first && *attempts == 1 {
            return Err("query construction failed".into());
        }
        let stream = Arc::new(FakeStream {
            options: options.clone(),
            prompt,
            events: Mutex::new(Some(events)),
            ignore_close: self.ignore_close,
        });
        lock(&self.streams).push(stream.clone());
        Ok(stream)
    }
}

struct Harness {
    adapter: ClaudeAdapter,
    emitted: Arc<Collector>,
    factory: Arc<FakeFactory>,
}

impl Harness {
    fn new(factory: FakeFactory) -> Harness {
        Harness::with_settle(factory, Duration::from_secs(1))
    }
    fn with_settle(factory: FakeFactory, settle: Duration) -> Harness {
        let emitted = Collector::new();
        let factory = Arc::new(factory);
        let adapter = ClaudeAdapter::with_factory(emitted.clone(), "claude".into(), Box::new(factory.clone()), settle);
        Harness { adapter, emitted, factory }
    }
    fn call(&self, id: Value, method: &str, params: Value) {
        let request = json!({ "id": id, "method": method, "params": params });
        handle(&self.adapter, self.emitted.as_ref(), request.as_object().unwrap());
    }
    fn start(&self, params: Value) -> String {
        self.call(json!("init"), "initialize", json!({}));
        self.call(json!("thread"), "thread/start", params);
        self.emitted.result(json!("thread"))["thread"]["id"].as_str().unwrap().to_string()
    }
    fn turn(&self, id: Value, thread_id: &str, text: &str) {
        self.call(
            id,
            "turn/start",
            json!({ "threadId": thread_id, "input": [{ "type": "text", "text": text }] }),
        );
    }
    fn completed_count(&self) -> usize {
        self.emitted.notifications("turn/completed").len()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.adapter.close();
    }
}

fn text_of(message: &Value) -> String {
    message["message"]["content"][0]["text"].as_str().unwrap_or_default().to_string()
}

fn success(result: &str, extra: Value) -> Value {
    let mut message = json!({ "type": "result", "subtype": "success", "is_error": false, "result": result, "queued_turn_count": 0 });
    message.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    message
}

fn event(event: Value) -> Value {
    json!({ "type": "stream_event", "parent_tool_use_id": null, "event": event })
}

#[test]
fn prompt_queue_preserves_steering_order_and_closes() {
    let queue = PromptQueue::default();
    queue.push(user_message("first")).unwrap();
    queue.push(user_message("second")).unwrap();
    assert_eq!(text_of(&queue.next().unwrap()), "first");
    assert_eq!(text_of(&queue.next().unwrap()), "second");
    queue.close();
    assert!(queue.next().is_none());
    assert!(queue.push(user_message("late")).unwrap_err().contains("closed"));
}

fn thread(id: &str, sandbox: Sandbox, persist: bool, resumed: bool) -> ThreadConfig {
    ThreadConfig {
        id: id.into(),
        cwd: "/tmp/project".into(),
        model: None,
        sandbox,
        effort: None,
        claude_path: None,
        persist_session: persist,
        resumed,
    }
}

#[test]
fn maps_danger_full_access_and_read_only_with_fresh_and_resume_identity() {
    let fresh = build_query_options(&thread("fresh-id", Sandbox::DangerFullAccess, true, false), "claude");
    assert_eq!(fresh.permission_mode, "bypassPermissions");
    assert!(!fresh.can_use_tool());
    assert_eq!(fresh.session_id.as_deref(), Some("fresh-id"));
    assert_eq!(fresh.resume, None);
    let argv = claude_argv(&fresh);
    assert!(argv.contains(&"--allow-dangerously-skip-permissions".to_string()));
    assert!(!argv.contains(&"--permission-prompt-tool".to_string()));
    assert!(argv.contains(&"--session-id=fresh-id".to_string()));

    let resumed = build_query_options(&thread("resume-id", Sandbox::ReadOnly, false, true), "claude");
    assert_eq!(resumed.permission_mode, "plan");
    assert_eq!(resumed.resume.as_deref(), Some("resume-id"));
    assert!(!resumed.persist_session);
    let argv = claude_argv(&resumed);
    assert!(argv.contains(&"--resume=resume-id".to_string()));
    assert!(argv.contains(&"--no-session-persistence".to_string()));
    assert!(!argv.iter().any(|arg| arg.starts_with("--session-id")));
}

#[test]
fn builds_the_sdk_argv_in_sdk_order() {
    let mut config = thread("abc", Sandbox::WorkspaceWrite, true, false);
    config.model = Some("sonnet".into());
    config.effort = Some("high".into());
    config.claude_path = Some("/opt/claude".into());
    let options = build_query_options(&config, "claude");
    assert_eq!(options.executable, "/opt/claude");
    assert_eq!(
        claude_argv(&options),
        [
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "stream-json",
            "--thinking",
            "adaptive",
            "--effort",
            "high",
            "--model",
            "sonnet",
            "--permission-prompt-tool",
            "stdio",
            "--setting-sources=user,project,local",
            "--permission-mode",
            "acceptEdits",
            "--include-partial-messages",
            "--session-id=abc",
            "--settings",
            &sandbox_settings("/tmp/project"),
        ]
    );
}

#[test]
fn does_not_approve_bash_in_read_only_mode() {
    let options = build_query_options(&thread("read-only-id", Sandbox::ReadOnly, true, false), "claude");
    assert!(!claude_argv(&options).contains(&"--settings".to_string()));
    let input = json!({ "command": "touch probe-file" });
    let decision = permission_decision(Sandbox::ReadOnly, "Bash", input.as_object().unwrap(), &Map::new());
    assert_eq!(decision["behavior"], "deny");
}

#[test]
fn runs_workspace_bash_inside_claudes_command_sandbox() {
    let settings: Value = serde_json::from_str(&sandbox_settings("/tmp/project")).unwrap();
    assert_eq!(
        settings,
        json!({ "sandbox": {
            "enabled": true,
            "failIfUnavailable": true,
            "autoAllowBashIfSandboxed": true,
            "allowUnsandboxedCommands": false,
            "filesystem": { "allowWrite": ["/tmp/project"] },
        } })
    );
    let decide = |input: Value, request: Value| {
        permission_decision(
            Sandbox::WorkspaceWrite,
            "Bash",
            input.as_object().unwrap(),
            request.as_object().unwrap(),
        )
    };
    assert_eq!(
        decide(json!({ "command": "awk '{ print $1 }' inventory.csv" }), json!({})),
        json!({ "behavior": "allow" })
    );
    assert_eq!(
        decide(json!({ "command": "pwd", "dangerouslyDisableSandbox": true }), json!({})),
        json!({ "behavior": "deny", "message": "Ruddr denied a request to run Bash outside the workspace sandbox." })
    );
    assert_eq!(
        decide(
            json!({ "command": "cat ../outside.txt" }),
            json!({ "blocked_path": "/tmp/outside.txt" })
        ),
        json!({ "behavior": "deny", "message": "Ruddr denied Bash access outside the workspace sandbox." })
    );
    assert_eq!(
        decide(
            json!({ "command": "make verify" }),
            json!({ "matched_ask_rule": { "source": "projectSettings", "tool_name": "Bash", "rule_content": "Bash(make *)" } })
        ),
        json!({ "behavior": "deny", "message": "Ruddr cannot override an explicit interactive approval rule." })
    );
    let question = permission_decision(Sandbox::WorkspaceWrite, "AskUserQuestion", &Map::new(), &Map::new());
    assert_eq!(question["behavior"], "deny");
}

#[test]
fn answers_control_requests_like_the_sdk() {
    let request = |value: Value| value.as_object().unwrap().clone();
    let allowed = cli::answer_control_request(
        Sandbox::WorkspaceWrite,
        true,
        &request(json!({ "subtype": "can_use_tool", "tool_name": "Bash", "input": { "command": "ls" }, "tool_use_id": "toolu_9" })),
    );
    assert_eq!(allowed, Ok(json!({ "behavior": "allow", "toolUseID": "toolu_9" })));
    let bypass = cli::answer_control_request(Sandbox::DangerFullAccess, false, &request(json!({ "subtype": "can_use_tool" })));
    assert_eq!(bypass, Err("canUseTool callback is not provided.".into()));
    assert_eq!(
        cli::answer_control_request(Sandbox::ReadOnly, true, &request(json!({ "subtype": "elicitation" }))),
        Ok(json!({ "action": "decline" }))
    );
    for subtype in [
        "request_user_dialog",
        "oauth_token_refresh",
        "mcp_message",
        "hook_callback",
        "future_thing",
    ] {
        assert!(
            cli::answer_control_request(Sandbox::ReadOnly, true, &request(json!({ "subtype": subtype }))).is_err(),
            "{subtype}"
        );
    }
}

#[test]
fn returns_method_not_found_for_unsupported_methods() {
    let harness = Harness::new(FakeFactory::default());
    harness.call(json!("unknown"), "thread/archive", json!({}));
    assert_eq!(
        harness.emitted.all(),
        [
            json!({ "id": "unknown", "error": { "code": -32601, "message": "method thread/archive is not supported by the Claude adapter" } })
        ]
    );
}

#[test]
fn allows_a_retry_after_query_construction_fails() {
    let harness = Harness::new(FakeFactory {
        fail_first: true,
        ..Default::default()
    });
    let thread_id = harness.start(json!({ "cwd": "/tmp/project", "sandbox": "read-only" }));
    harness.turn(json!("first"), &thread_id, "hello");
    assert_eq!(
        harness.emitted.response(json!("first")).unwrap()["error"]["message"],
        "query construction failed"
    );
    harness.turn(json!("retry"), &thread_id, "hello");
    assert!(harness.emitted.result(json!("retry"))["turn"]["id"].is_string());
    assert_eq!(*lock(&harness.factory.attempts), 2);
}

#[test]
fn preserves_distinct_identical_messages_while_suppressing_a_repeated_result_fallback() {
    let harness = Harness::new(FakeFactory::default());
    let thread_id = harness.start(json!({ "cwd": "/tmp/project", "sandbox": "read-only" }));
    harness.turn(json!("turn"), &thread_id, "hello");
    let stream = harness.factory.last();
    for index in [0, 1] {
        stream.push(event(
            json!({ "type": "content_block_start", "index": index, "content_block": { "type": "text", "text": "Done." } }),
        ));
        stream.push(event(json!({ "type": "content_block_stop", "index": index })));
    }
    stream.push(success("Done.", json!({})));
    harness.emitted.wait_for_method("turn/completed");
    let messages: Vec<Value> = harness
        .emitted
        .completed_items()
        .into_iter()
        .filter(|item| item["type"] == "agentMessage")
        .collect();
    assert_eq!(messages.len(), 2);
    assert_eq!(
        (messages[0]["phase"].as_str(), messages[0]["text"].as_str()),
        (Some("commentary"), Some("Done."))
    );
    assert_eq!(
        (messages[1]["phase"].as_str(), messages[1]["text"].as_str()),
        (Some("final_answer"), Some("Done."))
    );
    assert_ne!(messages[0]["id"], messages[1]["id"]);
}

#[test]
fn normalizes_thinking_tools_commentary_final_output_and_completion() {
    let harness = Harness::new(FakeFactory::default());
    let thread_id = harness.start(json!({ "cwd": "/tmp/project", "sandbox": "workspace-write", "provider": "claude" }));
    harness.turn(json!("turn"), &thread_id, "initial");
    let turn_id = harness.emitted.result(json!("turn"))["turn"]["id"].as_str().unwrap().to_string();
    let stream = harness.factory.last();
    assert_eq!(text_of(&stream.prompt.next().unwrap()), "initial");
    for value in [
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "", "signature": "" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "thinking_delta", "thinking": "Inspecting the failure" } }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "text", "text": "" } }),
        json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "text_delta", "text": "I found the issue." } }),
        json!({ "type": "content_block_stop", "index": 1 }),
        json!({ "type": "content_block_start", "index": 2, "content_block": { "type": "tool_use", "id": "tool-1", "name": "Bash", "input": {} } }),
        json!({ "type": "content_block_delta", "index": 2, "delta": { "type": "input_json_delta", "partial_json": "{\"command\":\"bun test\"}" } }),
        json!({ "type": "content_block_stop", "index": 2 }),
    ] {
        stream.push(event(value));
    }
    stream.push(json!({ "type": "user", "parent_tool_use_id": null, "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "tool-1", "content": "pass" }] } }));
    stream.push(success(
        "Done.",
        json!({ "total_cost_usd": 0, "usage": {}, "modelUsage": {}, "permission_denials": [] }),
    ));
    harness.emitted.wait_for_method("turn/completed");

    let text = harness.emitted.text();
    let items = harness.emitted.completed_items();
    assert!(
        items
            .iter()
            .any(|item| item["type"] == "reasoning" && item["summary"][0]["text"] == "Inspecting the failure")
    );
    assert!(
        harness
            .emitted
            .notifications("item/started")
            .iter()
            .any(|m| m["params"]["item"]["id"] == "tool-1")
    );
    assert!(
        items
            .iter()
            .any(|item| item["text"] == "I found the issue." && item["phase"] == "commentary")
    );
    assert!(items.iter().any(|item| item["text"] == "Done." && item["phase"] == "final_answer"));
    let tool = items.iter().find(|item| item["id"] == "tool-1").unwrap();
    assert_eq!(
        (tool["type"].as_str(), tool["command"].as_str()),
        (Some("commandExecution"), Some("bun test"))
    );
    assert_eq!(tool["aggregatedOutput"], "pass");
    assert_eq!(
        harness.emitted.notification("turn/completed").unwrap()["params"]["turn"]["id"],
        turn_id.as_str()
    );
    assert!(text.contains(&turn_id));

    // Assistant text streams as it arrives, in Codex's shape, and the
    // completed item that follows shares the streamed item's id.
    let deltas = harness.emitted.notifications("item/agentMessage/delta");
    assert_eq!(
        deltas.iter().map(|d| d["params"]["delta"].as_str().unwrap()).collect::<Vec<_>>(),
        ["I found the issue."]
    );
    let streamed = &deltas[0]["params"]["itemId"];
    assert!(
        items
            .iter()
            .any(|item| &item["id"] == streamed && item["text"] == "I found the issue.")
    );
}

#[test]
fn queues_steer_text_on_the_same_turn() {
    let harness = Harness::new(FakeFactory::default());
    let thread_id = harness.start(json!({ "cwd": "/tmp", "sandbox": "workspace-write" }));
    harness.turn(json!(3), &thread_id, "first");
    let turn_id = harness.emitted.result(json!(3))["turn"]["id"].as_str().unwrap().to_string();
    harness.call(
        json!(4),
        "turn/steer",
        json!({ "threadId": thread_id, "expectedTurnId": turn_id, "input": [{ "type": "text", "text": "steer" }] }),
    );
    let stream = harness.factory.last();
    assert_eq!(text_of(&stream.prompt.next().unwrap()), "first");
    assert_eq!(text_of(&stream.prompt.next().unwrap()), "steer");
    assert_eq!(harness.emitted.result(json!(4))["turnId"], turn_id.as_str());
    // The steer reaches the transcript as its own user message.
    let users: Vec<Value> = harness
        .emitted
        .completed_items()
        .into_iter()
        .filter(|item| item["type"] == "userMessage")
        .collect();
    assert_eq!(
        users.iter().map(|item| item["text"].as_str().unwrap()).collect::<Vec<_>>(),
        ["steer"]
    );
    // A steer naming another turn is rejected and queues nothing.
    harness.call(
        json!(5),
        "turn/steer",
        json!({ "threadId": thread_id, "expectedTurnId": "other", "input": [{ "type": "text", "text": "x" }] }),
    );
    assert_eq!(harness.emitted.response(json!(5)).unwrap()["error"]["code"], -32602);
}

#[test]
fn context_tracks_the_latest_main_request_and_preserves_cached_input_across_deltas() {
    let harness = Harness::new(FakeFactory::default());
    let thread_id = harness.start(json!({ "cwd": "/tmp", "sandbox": "read-only" }));
    harness.turn(json!(3), &thread_id, "hello");
    let stream = harness.factory.last();
    let usage_totals = || -> Vec<i64> {
        harness
            .emitted
            .notifications("thread/tokenUsage/updated")
            .iter()
            .map(|m| m["params"]["tokenUsage"]["last"]["totalTokens"].as_i64().unwrap())
            .collect()
    };
    stream.push(event(json!({ "type": "message_start", "message": { "model": "main", "usage": {
        "input_tokens": 1000, "cache_read_input_tokens": 2000, "cache_creation_input_tokens": 300, "output_tokens": 10 } } })));
    stream.push(event(
        json!({ "type": "message_delta", "usage": { "input_tokens": null, "cache_read_input_tokens": null, "output_tokens": 20 } }),
    ));
    harness
        .emitted
        .wait_for(|m| crate::protocol::tests::notifications(m, "thread/tokenUsage/updated").len() == 2);
    assert_eq!(usage_totals()[1], 3320);
    stream.push(json!({ "type": "stream_event", "parent_tool_use_id": "tool-sub", "event": {
        "type": "message_start", "message": { "model": "sub", "usage": { "input_tokens": 999999 } } } }));
    stream.push(json!({ "type": "stream_event", "parent_tool_use_id": "tool-sub", "event": {
        "type": "message_delta", "usage": { "output_tokens": 9999 } } }));
    stream.push(event(
        json!({ "type": "message_start", "message": { "model": "main", "usage": { "input_tokens": 500, "output_tokens": 3 } } }),
    ));
    stream.push(success(
        "done",
        json!({ "modelUsage": {
            "main": { "inputTokens": 2400000, "outputTokens": 100000, "contextWindow": 200000 },
            "sub": { "inputTokens": 0, "outputTokens": 0, "contextWindow": 1000000 },
        } }),
    ));
    harness.emitted.wait_for_method("turn/completed");
    assert_eq!(usage_totals(), [3310, 3320, 503, 503]);
    let last = harness.emitted.notifications("thread/tokenUsage/updated").pop().unwrap();
    assert_eq!(last["params"]["tokenUsage"]["total"]["totalTokens"], 2500000);
    assert_eq!(last["params"]["tokenUsage"]["last"]["totalTokens"], 503);
    assert_eq!(last["params"]["tokenUsage"]["modelContextWindow"], 200000);
}

#[test]
fn result_status_recognizes_claude_abort_terminal_reasons() {
    assert_eq!(result_status(&json!({ "subtype": "success", "is_error": false })), "completed");
    assert_eq!(
        result_status(&json!({ "subtype": "error_during_execution", "terminal_reason": "aborted_tools", "errors": [] })),
        "interrupted"
    );
    assert_eq!(
        result_status(&json!({ "subtype": "error_during_execution", "terminal_reason": "model_error", "errors": ["boom"] })),
        "failed"
    );
    assert_eq!(
        result_status(&json!({ "subtype": "error_during_execution", "errors": ["Request was Aborted."] })),
        "interrupted"
    );
}

#[test]
fn second_turn_resumes_the_session_and_usage_accumulates_across_turns() {
    let harness = Harness::new(FakeFactory::default());
    let thread_id = harness.start(json!({ "cwd": "/tmp", "sandbox": "workspace-write" }));
    let finish = |id: i64, cost: f64| {
        harness.turn(json!(id), &thread_id, &format!("task {id}"));
        harness.factory.last().push(success(
            &format!("done {id}"),
            json!({ "total_cost_usd": cost, "modelUsage": {},
                "usage": { "input_tokens": 100, "cache_read_input_tokens": 40, "cache_creation_input_tokens": 10, "output_tokens": 25 } }),
        ));
        harness
            .emitted
            .wait_for(|m| crate::protocol::tests::notifications(m, "turn/completed").len() as i64 >= id - 2);
    };
    finish(3, 0.05);
    assert_eq!(harness.factory.stream(0).options.session_id.as_deref(), Some(thread_id.as_str()));
    assert_eq!(harness.factory.stream(0).options.resume, None);
    finish(4, 0.07);
    assert_eq!(harness.factory.stream(1).options.resume.as_deref(), Some(thread_id.as_str()));
    assert_eq!(harness.factory.stream(1).options.session_id, None);

    let usage = harness.emitted.notifications("thread/tokenUsage/updated");
    assert_eq!(usage.len(), 2);
    let last = &usage[1]["params"];
    assert_eq!(last["threadId"], thread_id.as_str());
    assert_eq!(
        last["tokenUsage"]["total"],
        json!({ "inputTokens": 300, "cachedInputTokens": 80, "outputTokens": 50, "totalTokens": 350 })
    );
    assert!((last["costUsd"].as_f64().unwrap() - 0.12).abs() < 1e-9);
    let all = harness.emitted.all();
    let usage_index = all.iter().position(|m| m["method"] == "thread/tokenUsage/updated").unwrap();
    let completed_index = all.iter().position(|m| m["method"] == "turn/completed").unwrap();
    assert!(usage_index < completed_index);
}

#[test]
fn ephemeral_sessions_refuse_a_second_turn() {
    let harness = Harness::new(FakeFactory::default());
    let thread_id = harness.start(json!({ "cwd": "/tmp", "sandbox": "workspace-write", "persistSession": false }));
    harness.turn(json!(3), &thread_id, "one");
    harness.factory.last().push(success("done", json!({})));
    harness.emitted.wait_for_method("turn/completed");
    harness.turn(json!(4), &thread_id, "two");
    let error = harness.emitted.response(json!(4)).unwrap();
    assert!(error["error"]["message"].as_str().unwrap().contains("single turn"));
}

#[test]
fn an_ephemeral_thread_without_persist_session_is_not_persisted() {
    let harness = Harness::new(FakeFactory::default());
    let thread_id = harness.start(json!({ "cwd": "/tmp", "sandbox": "workspace-write", "ephemeral": true }));
    harness.turn(json!(3), &thread_id, "one");
    assert!(!harness.factory.last().options.persist_session);
}

#[test]
fn refuses_a_new_query_while_the_previous_stream_is_still_emitting() {
    let harness = Harness::with_settle(
        FakeFactory {
            ignore_close: true,
            ..Default::default()
        },
        Duration::from_millis(5),
    );
    let thread_id = harness.start(json!({ "cwd": "/tmp", "sandbox": "workspace-write" }));
    harness.turn(json!(3), &thread_id, "one");
    harness.factory.last().push(success("done", json!({})));
    harness.emitted.wait_for_method("turn/completed");
    harness.turn(json!(4), &thread_id, "two");
    let response = harness.emitted.response(json!(4)).unwrap();
    assert!(response["error"]["message"].as_str().unwrap().contains("did not settle"));
    assert_eq!(harness.factory.count(), 1);
    harness.factory.stream(0).push(success("late", json!({})));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(harness.completed_count(), 1);
}

#[test]
fn uses_cumulative_model_usage_once_for_queued_and_error_results() {
    let harness = Harness::new(FakeFactory::default());
    let thread_id = harness.start(json!({ "cwd": "/tmp", "sandbox": "workspace-write" }));
    harness.turn(json!(3), &thread_id, "one");
    let main = json!({ "inputTokens": 10, "cacheCreationInputTokens": 2, "cacheReadInputTokens": 3, "outputTokens": 4,
        "webSearchRequests": 0, "costUSD": 0.01, "contextWindow": 200000, "maxOutputTokens": 8000 });
    let sub = json!({ "inputTokens": 20, "cacheCreationInputTokens": 5, "cacheReadInputTokens": 7, "outputTokens": 6,
        "webSearchRequests": 0, "costUSD": 0.02, "contextWindow": 100000, "maxOutputTokens": 8000 });
    let stream = harness.factory.last();
    stream.push(
        json!({ "type": "result", "subtype": "success", "is_error": false, "result": "queued", "queued_turn_count": 1,
        "total_cost_usd": 0.01, "usage": { "input_tokens": 999, "output_tokens": 999 }, "modelUsage": { "claude-main": main } }),
    );
    std::thread::sleep(Duration::from_millis(20));
    assert!(harness.emitted.notifications("thread/tokenUsage/updated").is_empty());
    stream.push(
        json!({ "type": "result", "subtype": "error_during_execution", "is_error": true, "errors": ["model error"],
        "terminal_reason": "model_error", "queued_turn_count": 0, "total_cost_usd": 0.03,
        "usage": { "input_tokens": 999, "output_tokens": 999 }, "modelUsage": { "claude-main": main, "claude-subagent": sub } }),
    );
    harness.emitted.wait_for_method("turn/completed");
    let usage = harness.emitted.notification("thread/tokenUsage/updated").unwrap();
    assert_eq!(
        usage["params"]["tokenUsage"]["total"],
        json!({ "inputTokens": 47, "cachedInputTokens": 10, "outputTokens": 10, "totalTokens": 57 })
    );
    assert!(usage["params"]["tokenUsage"].get("modelContextWindow").is_none());
    assert!((usage["params"]["costUsd"].as_f64().unwrap() - 0.03).abs() < 1e-9);
    let completed = harness.emitted.notification("turn/completed").unwrap();
    assert_eq!(completed["params"]["turn"]["status"], "failed");
    assert_eq!(
        completed["params"]["turn"]["error"],
        json!({ "code": -32000, "message": "model error" })
    );
}

#[test]
fn interrupt_completes_the_turn_and_closes_the_query() {
    let harness = Harness::new(FakeFactory::default());
    let thread_id = harness.start(json!({ "cwd": "/tmp", "sandbox": "workspace-write" }));
    harness.turn(json!(3), &thread_id, "one");
    let turn_id = harness.emitted.result(json!(3))["turn"]["id"].as_str().unwrap().to_string();
    harness.call(json!(4), "turn/interrupt", json!({ "threadId": thread_id, "turnId": turn_id }));
    assert_eq!(harness.emitted.result(json!(4)), json!({}));
    let completed = harness.emitted.notification("turn/completed").unwrap();
    assert_eq!(completed["params"]["turn"], json!({ "id": turn_id, "status": "interrupted" }));
    // The stream ends after the interrupt without a second completion.
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(harness.completed_count(), 1);
}

/// Port of claude/app-server.test.ts: the shared transport keeps response
/// IDs, reports errors with the right codes, keeps order, and stops at EOF.
#[test]
fn shared_transport_preserves_response_ids_errors_order_and_eof_shutdown() {
    let emitted = Collector::new();
    let adapter = ClaudeAdapter::new(emitted.clone(), "claude".into());
    let input = [
        "not JSON".to_string(),
        json!({ "id": "init", "method": "initialize", "params": {} }).to_string(),
        json!({ "id": 2, "method": "thread/start", "params": [] }).to_string(),
        json!({ "id": null, "method": "unsupported" }).to_string(),
        json!({ "method": "unsupported" }).to_string(),
        String::new(),
    ]
    .join("\n");
    serve(&adapter, input.as_bytes(), emitted.as_ref() as &dyn Sink).unwrap();
    let messages = emitted.all();
    assert_eq!(messages.len(), 5, "{messages:?}");
    assert_eq!((&messages[0]["id"], &messages[0]["error"]["code"]), (&Value::Null, &json!(-32700)));
    assert_eq!(messages[1]["id"], "init");
    assert_eq!(messages[1]["result"]["serverInfo"]["name"], "ruddr-claude-adapter");
    assert_eq!((&messages[2]["id"], &messages[2]["error"]["code"]), (&json!(2), &json!(-32602)));
    assert_eq!((&messages[3]["id"], &messages[3]["error"]["code"]), (&Value::Null, &json!(-32601)));
    assert_eq!(messages[4]["method"], "error");
}

#[cfg(unix)]
mod cli_transport {
    use super::*;
    use crate::testing::FakeDir;

    fn adapter(fake: &FakeDir) -> (ClaudeAdapter, Arc<Collector>, String) {
        let emitted = Collector::new();
        let adapter = ClaudeAdapter::new(emitted.clone(), fake.script("claude"));
        let request = |id: &str, method: &str, params: Value| {
            handle(
                &adapter,
                emitted.as_ref(),
                json!({ "id": id, "method": method, "params": params }).as_object().unwrap(),
            );
        };
        request("init", "initialize", json!({}));
        let cwd = fake.dir.to_string_lossy().into_owned();
        request(
            "thread",
            "thread/start",
            json!({ "cwd": cwd, "sandbox": "workspace-write", "model": "sonnet" }),
        );
        let thread_id = emitted.result(json!("thread"))["thread"]["id"].as_str().unwrap().to_string();
        (adapter, emitted, thread_id)
    }

    fn start_turn(adapter: &ClaudeAdapter, emitted: &Collector, thread_id: &str, text: &str) -> String {
        let request = json!({ "id": "turn", "method": "turn/start",
            "params": { "threadId": thread_id, "input": [{ "type": "text", "text": text }] } });
        handle(adapter, emitted, request.as_object().unwrap());
        emitted.result(json!("turn"))["turn"]["id"].as_str().unwrap().to_string()
    }

    #[test]
    fn drives_the_cli_over_stream_json_with_recorded_message_shapes() {
        let fake = FakeDir::new();
        fake.write("replay.jsonl", include_str!("../../testdata/claude/turn.jsonl"));
        let (adapter, emitted, thread_id) = adapter(&fake);
        start_turn(&adapter, &emitted, &thread_id, "run the tests");
        emitted.wait_for_method("turn/completed");

        let argv = fake.argv();
        assert_eq!(
            argv[..6],
            ["-p", "--output-format", "stream-json", "--verbose", "--input-format", "stream-json"]
        );
        assert!(argv.contains(&format!("--session-id={thread_id}")));
        assert!(argv.windows(2).any(|pair| pair == ["--model", "sonnet"]));
        assert!(argv.windows(2).any(|pair| pair == ["--permission-prompt-tool", "stdio"]));
        let env = fake.read("env");
        // Like the SDK, an inherited value wins over the defaults.
        let inherited = |name: &str, default: &str| format!("{name}={}", std::env::var(name).unwrap_or_else(|_| default.into()));
        assert!(env.contains(&inherited("CLAUDE_CODE_ENTRYPOINT", "sdk-ts")), "{env}");
        assert!(env.contains(&inherited("CLAUDE_AGENT_SDK_VERSION", cli::SDK_VERSION)), "{env}");
        assert!(env.contains("NODE_OPTIONS=<unset>"));
        assert_eq!(
            std::fs::canonicalize(fake.read("cwd")).unwrap(),
            std::fs::canonicalize(&fake.dir).unwrap()
        );

        // The CLI saw initialize, the prompt, and the answers to its requests.
        let stdin = fake.wait_for("stdin.jsonl", |lines| lines.len() >= 4);
        assert_eq!(
            stdin[0]["request"],
            json!({ "subtype": "initialize", "forwardSubagentText": false })
        );
        assert_eq!(stdin[1], user_message("run the tests"));
        let answer = |id: &str| stdin.iter().find(|line| line["response"]["request_id"] == id).cloned().unwrap();
        assert_eq!(
            answer("perm-1")["response"],
            json!({ "subtype": "success", "request_id": "perm-1", "response": { "behavior": "allow", "toolUseID": "toolu_01" } })
        );
        assert_eq!(
            answer("hook-1")["response"],
            json!({ "subtype": "error", "request_id": "hook-1", "error": "No hook callback found for ID: hook_0" })
        );

        let items = emitted.completed_items();
        assert!(
            items
                .iter()
                .any(|i| i["type"] == "reasoning" && i["summary"][0]["text"] == "The user wants the tests run.")
        );
        let commentary = items.iter().find(|i| i["phase"] == "commentary").unwrap();
        assert_eq!(commentary["text"], "Running the tests.");
        let deltas = emitted.notifications("item/agentMessage/delta");
        assert_eq!(
            deltas.iter().map(|d| d["params"]["delta"].as_str().unwrap()).collect::<String>(),
            "Running the tests.All three tests pass."
        );
        assert_eq!(deltas[0]["params"]["itemId"], commentary["id"]);
        let tool = items.iter().find(|i| i["id"] == "toolu_01").unwrap();
        assert_eq!(tool["type"], "commandExecution");
        assert_eq!(tool["command"], "cargo test");
        assert_eq!(tool["aggregatedOutput"], "test result: ok. 3 passed");
        assert_eq!(
            items.iter().find(|i| i["phase"] == "final_answer").unwrap()["text"],
            "All three tests pass."
        );
        let usage = emitted.notifications("thread/tokenUsage/updated").pop().unwrap();
        assert_eq!(
            usage["params"]["tokenUsage"]["total"],
            json!({ "inputTokens": 33652, "cachedInputTokens": 32800, "outputTokens": 73, "totalTokens": 33725 })
        );
        assert_eq!(usage["params"]["tokenUsage"]["modelContextWindow"], 200000);
        assert_eq!(
            emitted.notification("turn/completed").unwrap()["params"]["turn"]["status"],
            "completed"
        );
        adapter.close();
    }

    #[test]
    fn interrupt_sends_the_control_request_and_ends_stdin() {
        let fake = FakeDir::new();
        let (adapter, emitted, thread_id) = adapter(&fake);
        let turn_id = start_turn(&adapter, &emitted, &thread_id, "hang");
        fake.wait_for("stdin.jsonl", |lines| lines.len() >= 2);
        let request = json!({ "id": "stop", "method": "turn/interrupt", "params": { "threadId": thread_id, "turnId": turn_id } });
        handle(&adapter, emitted.as_ref(), request.as_object().unwrap());
        assert_eq!(
            emitted.notification("turn/completed").unwrap()["params"]["turn"]["status"],
            "interrupted"
        );
        let stdin = fake.wait_for("stdin.jsonl", |lines| lines.iter().any(|l| l["request"]["subtype"] == "interrupt"));
        assert_eq!(stdin.last().unwrap()["type"], "control_request");
        adapter.close();
    }

    #[test]
    fn a_nonzero_exit_reports_the_exit_code() {
        let fake = FakeDir::new();
        fake.write("exit_code", "3");
        let emitted = Collector::new();
        let adapter = ClaudeAdapter::new(emitted.clone(), fake.script("claude"));
        let request = |id: &str, method: &str, params: Value| {
            handle(
                &adapter,
                emitted.as_ref(),
                json!({ "id": id, "method": method, "params": params }).as_object().unwrap(),
            );
        };
        request("init", "initialize", json!({}));
        let cwd = fake.dir.to_string_lossy().into_owned();
        request("thread", "thread/start", json!({ "cwd": cwd, "sandbox": "danger-full-access" }));
        let thread_id = emitted.result(json!("thread"))["thread"]["id"].as_str().unwrap().to_string();
        start_turn(&adapter, &emitted, &thread_id, "hang");
        // Bypass mode has no permission callback, so closing the prompt queue
        // ends stdin at once and the fake exits with code 3.
        lock(&adapter.inner.state).queue.clone().unwrap().close();
        emitted.wait_for_method("turn/completed");
        let completed = emitted.notification("turn/completed").unwrap();
        assert_eq!(completed["params"]["turn"]["status"], "failed");
        assert_eq!(
            completed["params"]["turn"]["error"]["message"],
            "Claude Code process exited with code 3"
        );
        assert!(!fake.argv().contains(&"--permission-prompt-tool".to_string()));
        adapter.close();
    }
}
