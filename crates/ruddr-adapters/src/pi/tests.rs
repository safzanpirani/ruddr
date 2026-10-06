//! Port of pi/runtime.test.ts.

use super::*;
use crate::protocol::Latch;
use crate::protocol::handle;
use crate::protocol::tests::Collector;

#[derive(Default)]
struct FakePiClient {
    commands: Mutex<Vec<Value>>,
    event: Mutex<Option<EventFn>>,
    context_tokens: Mutex<Option<Value>>,
    steer_gate: Mutex<Option<Arc<Latch>>>,
}

impl FakePiClient {
    fn new() -> Arc<FakePiClient> {
        let client = FakePiClient::default();
        *lock(&client.context_tokens) = Some(json!(12));
        Arc::new(client)
    }
    fn event(&self, event: Value) {
        let on_event = lock(&self.event).clone().unwrap();
        on_event(event.as_object().unwrap().clone());
    }
    fn commands(&self) -> Vec<Value> {
        lock(&self.commands).clone()
    }
}

impl PiClient for Arc<FakePiClient> {
    fn start(&self, config: &PiThread, on_event: EventFn) -> AResult<String> {
        *lock(&self.event) = Some(on_event);
        Ok(config.id.clone())
    }
    fn send(&self, command: Value) -> AResult<Map<String, Value>> {
        lock(&self.commands).push(command.clone());
        if command["type"] == "steer"
            && let Some(gate) = lock(&self.steer_gate).clone()
        {
            gate.wait(Duration::from_secs(15));
        }
        if command["type"] == "get_session_stats" {
            let tokens = lock(&self.context_tokens).clone().unwrap_or(Value::Null);
            return Ok(json!({ "data": {
                "tokens": { "input": 20, "output": 5, "cacheRead": 3, "cacheWrite": 0, "totalTokens": 28 },
                "cost": 0.02,
                "contextUsage": { "contextWindow": 1_000_000, "tokens": tokens },
            } })
            .as_object()
            .cloned()
            .unwrap());
        }
        Ok(json!({ "success": true }).as_object().cloned().unwrap())
    }
    fn close(&self) {}
}

fn adapter(client: &Arc<FakePiClient>) -> (Arc<PiAdapter>, Arc<Collector>) {
    flavored(Flavor::Pi, client)
}

fn flavored(flavor: Flavor, client: &Arc<FakePiClient>) -> (Arc<PiAdapter>, Arc<Collector>) {
    let emitted = Collector::new();
    let adapter = PiAdapter::with_client(flavor, emitted.clone(), flavor.slug().into(), Box::new(client.clone()));
    (Arc::new(adapter), emitted)
}

fn call(adapter: &PiAdapter, emitted: &Collector, id: i64, method: &str, params: Value) {
    handle(
        adapter,
        emitted,
        json!({ "id": id, "method": method, "params": params }).as_object().unwrap(),
    );
}

fn start(adapter: &PiAdapter, emitted: &Collector, sandbox: &str) -> (String, String) {
    call(adapter, emitted, 1, "initialize", json!({}));
    call(
        adapter,
        emitted,
        2,
        "thread/start",
        json!({ "cwd": "/tmp/work", "sandbox": sandbox, "model": "openrouter/deepseek/model" }),
    );
    let thread_id = emitted.result(json!(2))["thread"]["id"].as_str().unwrap().to_string();
    call(
        adapter,
        emitted,
        3,
        "turn/start",
        json!({ "threadId": thread_id, "input": [{ "type": "text", "text": "first" }] }),
    );
    let turn_id = emitted.result(json!(3))["turn"]["id"].as_str().unwrap().to_string();
    (thread_id, turn_id)
}

#[test]
fn steers_reports_tools_and_completes_after_agent_settled() {
    let client = FakePiClient::new();
    let (adapter, emitted) = adapter(&client);
    let (thread_id, turn_id) = start(&adapter, &emitted, "workspace-write");
    call(
        &adapter,
        &emitted,
        4,
        "turn/steer",
        json!({ "threadId": thread_id, "expectedTurnId": turn_id, "input": [{ "type": "text", "text": "correction" }] }),
    );
    assert_eq!(
        client.commands()[..2],
        [
            json!({ "type": "prompt", "message": "first" }),
            json!({ "type": "steer", "message": "correction" })
        ]
    );
    let users: Vec<Value> = emitted
        .completed_items()
        .into_iter()
        .filter(|item| item["type"] == "userMessage")
        .collect();
    assert_eq!(
        users.iter().map(|item| item["text"].as_str().unwrap()).collect::<Vec<_>>(),
        ["correction"]
    );

    client.event(json!({ "type": "tool_execution_start", "toolCallId": "tool-1", "toolName": "read", "args": { "path": "README.md" } }));
    client.event(
        json!({ "type": "tool_execution_end", "toolCallId": "tool-1", "toolName": "read", "args": { "path": "README.md" },
        "result": { "content": [{ "type": "text", "text": "ok" }] }, "isError": false }),
    );
    client.event(json!({ "type": "message_end", "message": { "role": "assistant", "content": [
        { "type": "thinking", "thinking": "verified" }, { "type": "text", "text": "PI_OK" } ], "stopReason": "stop" } }));
    client.event(json!({ "type": "agent_settled" }));
    emitted.wait_for_method("turn/completed");
    let items = emitted.completed_items();
    assert_eq!(items.iter().find(|i| i["type"] == "agentMessage").unwrap()["text"], "PI_OK");
    assert_eq!(items.iter().find(|i| i["type"] == "agentMessage").unwrap()["phase"], "final_answer");
    let tool = items.iter().find(|i| i["id"] == "tool-1").unwrap();
    assert_eq!((tool["status"].as_str(), tool["output"].as_str()), (Some("completed"), Some("ok")));
    assert_eq!(tool["command"], r#"read {"path":"README.md"}"#);
    let usage = emitted.notification("thread/tokenUsage/updated").unwrap();
    assert_eq!(usage["params"]["tokenUsage"]["total"]["totalTokens"], 28);
    assert_eq!(usage["params"]["tokenUsage"]["last"]["totalTokens"], 12);
    assert_eq!(usage["params"]["tokenUsage"]["modelContextWindow"], 1_000_000);
    assert_eq!(
        emitted.notification("turn/completed").unwrap()["params"]["turn"]["status"],
        "completed"
    );
    adapter.close();
}

#[test]
fn clears_unknown_context_after_compaction_instead_of_reusing_session_totals() {
    let client = FakePiClient::new();
    *lock(&client.context_tokens) = None;
    let (adapter, emitted) = adapter(&client);
    start(&adapter, &emitted, "read-only");
    client.event(json!({ "type": "agent_settled" }));
    emitted.wait_for_method("turn/completed");
    let usage = emitted.notification("thread/tokenUsage/updated").unwrap();
    assert!(usage["params"]["tokenUsage"].get("last").is_none());
    adapter.close();
}

#[test]
fn waits_for_an_accepted_steer_and_the_following_settled_event() {
    let client = FakePiClient::new();
    let gate = Latch::new();
    *lock(&client.steer_gate) = Some(gate.clone());
    let (adapter, emitted) = adapter(&client);
    let (thread_id, turn_id) = start(&adapter, &emitted, "workspace-write");
    let steering = {
        let (adapter, emitted) = (adapter.clone(), emitted.clone());
        thread::spawn(move || {
            call(
                &adapter,
                &emitted,
                4,
                "turn/steer",
                json!({ "threadId": thread_id, "expectedTurnId": turn_id, "input": [{ "type": "text", "text": "correction" }] }),
            )
        })
    };
    while !client.commands().iter().any(|c| c["type"] == "steer") {
        thread::sleep(Duration::from_millis(2));
    }
    client.event(json!({ "type": "agent_settled" }));
    thread::sleep(Duration::from_millis(30));
    assert!(emitted.notification("turn/completed").is_none());

    gate.set();
    steering.join().unwrap();
    thread::sleep(Duration::from_millis(30));
    assert!(emitted.notification("turn/completed").is_none());
    client.event(json!({ "type": "agent_settled" }));
    emitted.wait_for_method("turn/completed");
    assert_eq!(emitted.notifications("turn/completed").len(), 1);
    adapter.close();
}

#[test]
fn a_process_failure_fails_the_turn_and_interrupt_aborts() {
    let client = FakePiClient::new();
    let (adapter, emitted) = adapter(&client);
    let (thread_id, turn_id) = start(&adapter, &emitted, "workspace-write");
    call(
        &adapter,
        &emitted,
        4,
        "turn/interrupt",
        json!({ "threadId": thread_id, "turnId": turn_id }),
    );
    assert_eq!(client.commands().last().unwrap(), &json!({ "type": "abort" }));
    client.event(json!({ "type": "ruddr_error", "error": "Pi RPC output closed" }));
    emitted.wait_for_method("turn/completed");
    let turn = &emitted.notification("turn/completed").unwrap()["params"]["turn"];
    assert_eq!(turn["status"], "failed");
    assert_eq!(turn["error"]["message"], "Pi RPC output closed");
    adapter.close();
}

#[test]
fn builds_pi_argv_for_each_session_mode() {
    let mut config = PiThread {
        flavor: Flavor::Pi,
        id: "sid".into(),
        cwd: "/w".into(),
        model: Some("m".into()),
        effort: Some("high".into()),
        executable: "pi".into(),
        sandbox: "read-only".into(),
        ephemeral: false,
        resumed: false,
    };
    assert_eq!(
        pi_args(&config),
        [
            "--mode",
            "rpc",
            "--approve",
            "--model",
            "m",
            "--thinking",
            "high",
            "--session-id",
            "sid",
            "--no-extensions",
            "--tools",
            "read,grep,find,ls"
        ]
    );
    config.resumed = true;
    config.sandbox = "workspace-write".into();
    assert_eq!(pi_args(&config)[7..], ["--session", "sid"]);
    config.ephemeral = true;
    assert_eq!(pi_args(&config)[7..], ["--no-session"]);
}

#[cfg(unix)]
#[test]
fn rejects_interactive_extension_ui_and_times_out_unanswered_commands() {
    let fake = crate::testing::FakeDir::new();
    // The 500ms deadline is what this test asserts on; starting the fake gets
    // a generous budget of its own.
    let client = SubprocessPiClient::new(Flavor::Pi, Duration::from_millis(500), Duration::from_secs(30));
    let events = Arc::new(Mutex::new(Vec::<Map<String, Value>>::new()));
    let sink = events.clone();
    let config = PiThread {
        flavor: Flavor::Pi,
        id: "fresh".into(),
        cwd: fake.dir.to_string_lossy().into_owned(),
        model: None,
        effort: None,
        executable: fake.script("pi"),
        sandbox: "workspace-write".into(),
        ephemeral: false,
        resumed: false,
    };
    let session = client.start(&config, Arc::new(move |event| lock(&sink).push(event))).unwrap();
    assert_eq!(session, "pi_test_session");
    assert_eq!(fake.argv(), ["--mode", "rpc", "--approve", "--session-id", "fresh"]);
    let responses = || -> Vec<Value> {
        lock(&events)
            .iter()
            .filter(|e| e.get("type") == Some(&json!("test_ui_response")))
            .map(|e| e["response"].clone())
            .collect()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while responses().len() < 2 {
        assert!(std::time::Instant::now() < deadline, "no UI responses");
        thread::sleep(Duration::from_millis(5));
    }
    let responses = responses();
    assert!(responses.contains(&json!({ "type": "extension_ui_response", "id": "ui-select", "cancelled": true })));
    assert!(responses.contains(&json!({ "type": "extension_ui_response", "id": "ui-unknown", "cancelled": true })));
    assert!(!Value::Array(responses).to_string().contains("ui-notify"));

    let error = client.send(json!({ "type": "never_respond" })).unwrap_err();
    assert!(error.message.contains("timed out after 500ms"), "{error}");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !lock(&events).iter().any(|e| e.get("type") == Some(&json!("ruddr_error"))) {
        assert!(std::time::Instant::now() < deadline, "no ruddr_error event");
        thread::sleep(Duration::from_millis(5));
    }
    // The failed process is gone; later commands fail at once.
    assert_eq!(
        client.send(json!({ "type": "get_state" })).unwrap_err().message,
        "Pi RPC process is not running"
    );
    client.close();
}

#[test]
fn builds_omp_argv_for_each_session_mode() {
    let mut config = PiThread {
        flavor: Flavor::Omp,
        id: "sid".into(),
        cwd: "/w".into(),
        model: Some("anthropic/m".into()),
        effort: Some("high".into()),
        executable: "omp".into(),
        sandbox: "read-only".into(),
        ephemeral: false,
        resumed: false,
    };
    assert_eq!(
        pi_args(&config),
        [
            "--mode",
            "rpc",
            "--auto-approve",
            "--allow-home",
            "--model",
            "anthropic/m",
            "--thinking",
            "high",
            "--no-extensions",
            "--tools",
            "read,grep,find,glob"
        ]
    );
    config.resumed = true;
    config.sandbox = "workspace-write".into();
    config.model = None;
    config.effort = None;
    assert_eq!(pi_args(&config)[4..], ["--resume", "sid"]);
    config.ephemeral = true;
    assert_eq!(pi_args(&config)[4..], ["--no-session"]);
}

#[test]
fn omp_turns_end_at_session_settled_not_agent_settled() {
    let client = FakePiClient::new();
    let (adapter, emitted) = flavored(Flavor::Omp, &client);
    start(&adapter, &emitted, "workspace-write");
    assert_eq!(emitted.result(json!(1))["serverInfo"]["name"], "ruddr-omp-adapter");
    client.event(json!({ "type": "message_end", "message": { "role": "assistant", "content": [
        { "type": "text", "text": "OMP_OK" } ], "stopReason": "stop" } }));
    client.event(json!({ "type": "agent_settled" }));
    client.event(
        json!({ "type": "prompt_result", "id": "ruddr-omp-1", "agentInvoked": true, "status": "completed", "sessionSettled": true }),
    );
    thread::sleep(Duration::from_millis(30));
    assert!(emitted.notification("turn/completed").is_none());
    client.event(json!({ "type": "session_settled" }));
    emitted.wait_for_method("turn/completed");
    assert_eq!(emitted.notifications("turn/completed").len(), 1);
    let items = emitted.completed_items();
    assert_eq!(items.iter().find(|i| i["type"] == "agentMessage").unwrap()["text"], "OMP_OK");
    assert_eq!(
        emitted.notification("turn/completed").unwrap()["params"]["turn"]["status"],
        "completed"
    );
    adapter.close();
}

#[test]
fn omp_edits_report_file_changes_from_the_numbered_diff() {
    let client = FakePiClient::new();
    let (adapter, emitted) = flavored(Flavor::Omp, &client);
    start(&adapter, &emitted, "workspace-write");
    let args = json!({ "path": "/w/a.rs", "edits": [{ "op": "replace", "pos": "2#VY", "lines": ["new"] }] });
    client.event(json!({ "type": "tool_execution_start", "toolCallId": "e1", "toolName": "edit", "args": args }));
    client.event(
        json!({ "type": "tool_execution_end", "toolCallId": "e1", "toolName": "edit", "isError": false,
        "result": { "content": [{ "type": "text", "text": "Updated a.rs" }],
            "details": { "diff": " 1|keep\n-2|old\n+2|new", "op": "update" } } }),
    );
    client.event(json!({ "type": "tool_execution_start", "toolCallId": "r1", "toolName": "read", "args": { "path": "/w/a.rs" } }));
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while emitted.notifications("item/started").len() < 2 {
        assert!(std::time::Instant::now() < deadline, "tool events never arrived");
        thread::sleep(Duration::from_millis(5));
    }
    let started = emitted.notification("item/started").unwrap();
    assert_eq!(started["params"]["item"]["type"], "fileChange");
    assert!(started["params"]["item"].get("changes").is_none());
    let edit = emitted.completed_items().into_iter().find(|i| i["id"] == "e1").unwrap();
    assert_eq!(
        (edit["type"].as_str(), edit["status"].as_str()),
        (Some("fileChange"), Some("completed"))
    );
    assert_eq!(
        edit["changes"],
        json!([{ "path": "/w/a.rs", "kind": { "type": "update" }, "diff": "@@ -1,2 +1,2 @@\n keep\n-old\n+new\n" }])
    );
    let reads = emitted.notifications("item/started");
    assert_eq!(reads.last().unwrap()["params"]["item"]["type"], "toolCall");
    adapter.close();
}

#[test]
fn omp_prompt_that_never_reaches_the_agent_fails_the_turn_with_its_error() {
    let client = FakePiClient::new();
    let (adapter, emitted) = flavored(Flavor::Omp, &client);
    start(&adapter, &emitted, "workspace-write");
    client.event(json!({ "type": "prompt_result", "agentInvoked": false, "status": "error",
        "error": { "message": "No API key for anthropic", "retryable": false }, "sessionSettled": true }));
    emitted.wait_for_method("turn/completed");
    let turn = &emitted.notification("turn/completed").unwrap()["params"]["turn"];
    assert_eq!(turn["status"], "failed");
    assert_eq!(turn["error"]["message"], "No API key for anthropic");
    adapter.close();
}

#[test]
fn omp_reports_the_prompt_error_when_the_model_fails() {
    let client = FakePiClient::new();
    let (adapter, emitted) = flavored(Flavor::Omp, &client);
    let (thread_id, turn_id) = start(&adapter, &emitted, "workspace-write");
    client.event(json!({ "type": "prompt_result", "agentInvoked": true, "status": "error",
        "error": { "message": "overloaded", "retryable": true }, "sessionSettled": false }));
    client.event(json!({ "type": "session_settled" }));
    emitted.wait_for_method("turn/completed");
    let turn = &emitted.notification("turn/completed").unwrap()["params"]["turn"];
    assert_eq!(
        (turn["status"].as_str(), turn["error"]["message"].as_str()),
        (Some("failed"), Some("overloaded"))
    );

    // The next turn starts clean; an aborted prompt reads as interrupted.
    call(
        &adapter,
        &emitted,
        5,
        "turn/start",
        json!({ "threadId": thread_id, "input": [{ "type": "text", "text": "again" }] }),
    );
    assert_ne!(emitted.result(json!(5))["turn"]["id"], json!(turn_id));
    client.event(json!({ "type": "prompt_result", "agentInvoked": true, "status": "aborted", "sessionSettled": true }));
    client.event(json!({ "type": "session_settled" }));
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while emitted.notifications("turn/completed").len() < 2 {
        assert!(std::time::Instant::now() < deadline, "second turn never completed");
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        emitted.notifications("turn/completed")[1]["params"]["turn"]["status"],
        "interrupted"
    );
    adapter.close();
}

#[cfg(unix)]
#[test]
fn omp_client_takes_the_session_id_omp_reports() {
    let fake = crate::testing::FakeDir::new();
    let client = SubprocessPiClient::new(Flavor::Omp, Duration::from_secs(5), Duration::from_secs(30));
    let config = PiThread {
        flavor: Flavor::Omp,
        id: "ignored".into(),
        cwd: fake.dir.to_string_lossy().into_owned(),
        model: None,
        effort: None,
        executable: fake.script("pi"),
        sandbox: "workspace-write".into(),
        ephemeral: false,
        resumed: false,
    };
    let session = client.start(&config, Arc::new(|_| {})).unwrap();
    assert_eq!(session, "pi_test_session");
    assert_eq!(fake.argv(), ["--mode", "rpc", "--auto-approve", "--allow-home"]);
    client.close();
    assert_eq!(
        client.send(json!({ "type": "get_state" })).unwrap_err().message,
        "omp RPC process is not running"
    );
}
