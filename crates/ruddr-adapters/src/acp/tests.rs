use super::*;
use crate::protocol::Latch;
use crate::protocol::handle;
use crate::protocol::tests::Collector;

/// Records requests; `session/prompt` blocks until the test answers it.
#[derive(Default)]
struct FakeAcpClient {
    requests: Mutex<Vec<(String, Value)>>,
    notifications: Mutex<Vec<(String, Value)>>,
    event: Mutex<Option<EventFn>>,
    /// The response of the next main prompt, set when the test releases it.
    prompt_result: Mutex<Option<AResult<Value>>>,
    prompt_done: Arc<Latch>,
    /// Text the agent "says" when it receives a `/steer` prompt.
    steer_reply: Mutex<Option<String>>,
}

impl FakeAcpClient {
    fn new() -> Arc<FakeAcpClient> {
        Arc::new(FakeAcpClient {
            prompt_done: Latch::new(),
            ..FakeAcpClient::default()
        })
    }
    fn update(&self, update: Value) {
        let on_event = lock(&self.event).clone().unwrap();
        on_event(json!({ "sessionId": "acp-1", "update": update }).as_object().unwrap().clone());
    }
    fn finish(&self, result: AResult<Value>) {
        *lock(&self.prompt_result) = Some(result);
        self.prompt_done.set();
    }
    fn requests(&self) -> Vec<(String, Value)> {
        lock(&self.requests).clone()
    }
}

impl AcpClient for Arc<FakeAcpClient> {
    fn start(&self, config: &AcpThread, on_event: EventFn) -> AResult<String> {
        *lock(&self.event) = Some(on_event);
        Ok(if config.resumed { config.id.clone() } else { "acp-1".into() })
    }
    fn request(&self, method: &str, params: Value, _timeout: Option<Duration>) -> AResult<Value> {
        lock(&self.requests).push((method.into(), params.clone()));
        let text = params["prompt"][0]["text"].as_str().unwrap_or_default().to_string();
        if text.starts_with("/steer") {
            if let Some(reply) = lock(&self.steer_reply).clone() {
                self.update(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": reply } }));
            }
            // Updates are delivered on the adapter's worker thread; let it catch up.
            thread::sleep(Duration::from_millis(50));
            return Ok(json!({ "stopReason": "end_turn" }));
        }
        if method == "session/prompt" {
            assert!(self.prompt_done.wait(Duration::from_secs(15)), "prompt never released");
            return lock(&self.prompt_result).take().unwrap();
        }
        Ok(json!({}))
    }
    fn notify(&self, method: &str, params: Value) -> AResult<()> {
        lock(&self.notifications).push((method.into(), params));
        Ok(())
    }
    fn close(&self) {}
}

fn new_adapter(flavor: Flavor, client: &Arc<FakeAcpClient>) -> (Arc<AcpAdapter>, Arc<Collector>) {
    let emitted = Collector::new();
    let adapter = AcpAdapter::with_client(flavor, emitted.clone(), flavor.slug().into(), Box::new(client.clone()));
    (Arc::new(adapter), emitted)
}

fn call(adapter: &AcpAdapter, emitted: &Collector, id: i64, method: &str, params: Value) {
    handle(
        adapter,
        emitted,
        json!({ "id": id, "method": method, "params": params }).as_object().unwrap(),
    );
}

fn start(adapter: &AcpAdapter, emitted: &Collector) -> (String, String) {
    call(adapter, emitted, 1, "initialize", json!({}));
    call(
        adapter,
        emitted,
        2,
        "thread/start",
        json!({ "cwd": "/tmp/work", "sandbox": "workspace-write", "model": AGENT_DEFAULT_MODEL }),
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

fn wait_until(check: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !check() {
        assert!(std::time::Instant::now() < deadline, "condition never held");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_turn_streams_text_and_tools_and_ends_at_the_prompt_response() {
    let client = FakeAcpClient::new();
    let (adapter, emitted) = new_adapter(Flavor::Hermes, &client);
    let (thread_id, _) = start(&adapter, &emitted);
    assert_eq!(thread_id, "acp-1");
    wait_until(|| client.requests().iter().any(|(m, _)| m == "session/prompt"));
    assert_eq!(
        client.requests()[0].1,
        json!({ "sessionId": "acp-1", "prompt": [{ "type": "text", "text": "first" }] })
    );
    client.update(json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": "plan it" } }));
    client.update(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "Reading " } }));
    client.update(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "the file." } }));
    client.update(
        json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "read_file: notes.txt", "kind": "read",
        "status": "pending", "rawInput": { "path": "notes.txt" } }),
    );
    client.update(
        json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
        "content": [{ "type": "content", "content": { "type": "text", "text": "beta" } }] }),
    );
    client.update(
        json!({ "sessionUpdate": "tool_call", "toolCallId": "t2", "title": "patch a.md", "kind": "edit", "status": "in_progress",
        "content": [{ "type": "diff", "path": "/w/a.md", "oldText": "teh", "newText": "the" }] }),
    );
    client.update(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "It says beta." } }));
    client.update(json!({ "sessionUpdate": "usage_update", "used": 1200, "size": 200000 }));
    wait_until(|| emitted.notification("thread/tokenUsage/updated").is_some());
    client.finish(Ok(json!({ "stopReason": "end_turn" })));
    emitted.wait_for_method("turn/completed");

    assert_eq!(emitted.notifications("item/agentMessage/delta").len(), 3);
    let items = emitted.completed_items();
    let messages: Vec<(&str, &str)> = items
        .iter()
        .filter(|i| i["type"] == "agentMessage")
        .map(|i| (i["text"].as_str().unwrap(), i["phase"].as_str().unwrap()))
        .collect();
    assert_eq!(messages, [("Reading the file.", "commentary"), ("It says beta.", "final_answer")]);
    assert_eq!(
        items.iter().find(|i| i["type"] == "reasoning").unwrap()["summary"][0]["text"],
        "plan it"
    );
    let read = items.iter().find(|i| i["id"] == "t1").unwrap();
    assert_eq!(
        (read["status"].as_str(), read["output"].as_str()),
        (Some("completed"), Some("beta"))
    );
    assert_eq!(read["command"], "read_file: notes.txt");
    // The unfinished edit ends with the turn and keeps its diff as input.
    let edit = items.iter().find(|i| i["id"] == "t2").unwrap();
    assert_eq!(edit["status"], "failed");
    assert_eq!(edit["input"], json!({ "path": "/w/a.md", "oldText": "teh", "newText": "the" }));
    let usage = emitted.notification("thread/tokenUsage/updated").unwrap();
    assert_eq!(usage["params"]["tokenUsage"]["last"]["totalTokens"], 1200);
    assert_eq!(usage["params"]["tokenUsage"]["modelContextWindow"], 200000);
    assert_eq!(
        emitted.notification("turn/completed").unwrap()["params"]["turn"]["status"],
        "completed"
    );
    adapter.close();
}

#[test]
fn hermes_steers_with_a_slash_command_and_hides_the_confirmation() {
    let client = FakeAcpClient::new();
    *lock(&client.steer_reply) = Some("⏩ Steer queued for the active turn: focus".into());
    let (adapter, emitted) = new_adapter(Flavor::Hermes, &client);
    let (thread_id, turn_id) = start(&adapter, &emitted);
    call(
        &adapter,
        &emitted,
        4,
        "turn/steer",
        json!({ "threadId": thread_id, "expectedTurnId": turn_id, "input": [{ "type": "text", "text": "focus" }] }),
    );
    assert_eq!(emitted.result(json!(4))["turnId"], json!(turn_id));
    let steer = client
        .requests()
        .into_iter()
        .find(|(_, p)| p["prompt"][0]["text"] == "/steer focus");
    assert!(steer.is_some(), "{:?}", client.requests());
    client.finish(Ok(json!({ "stopReason": "end_turn" })));
    emitted.wait_for_method("turn/completed");
    let items = emitted.completed_items();
    assert!(items.iter().any(|i| i["type"] == "userMessage" && i["text"] == "focus"));
    assert!(!emitted.text().contains("Steer queued"), "the confirmation is not answer text");

    // A steer Hermes does not confirm fails instead of becoming a new turn.
    let client = FakeAcpClient::new();
    *lock(&client.steer_reply) = Some("No active turn — queued for the next turn. (1 queued)".into());
    let (adapter, emitted) = new_adapter(Flavor::Hermes, &client);
    let (thread_id, turn_id) = start(&adapter, &emitted);
    call(
        &adapter,
        &emitted,
        4,
        "turn/steer",
        json!({ "threadId": thread_id, "expectedTurnId": turn_id, "input": [{ "type": "text", "text": "late" }] }),
    );
    let error = emitted.response(json!(4)).unwrap()["error"]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(error.contains("rejected the steer"), "{error}");
    client.finish(Ok(json!({ "stopReason": "end_turn" })));
    adapter.close();
}

#[test]
fn openclaw_rejects_steers_and_interrupt_cancels_the_session() {
    let client = FakeAcpClient::new();
    let (adapter, emitted) = new_adapter(Flavor::OpenClaw, &client);
    let (thread_id, turn_id) = start(&adapter, &emitted);
    call(
        &adapter,
        &emitted,
        4,
        "turn/steer",
        json!({ "threadId": thread_id, "expectedTurnId": turn_id, "input": [{ "type": "text", "text": "x" }] }),
    );
    let error = emitted.response(json!(4)).unwrap()["error"]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(error.contains("OpenClaw runs cannot be steered"), "{error}");
    assert!(!client.requests().iter().any(|(_, p)| p["prompt"][0]["text"] == "/steer x"));
    call(
        &adapter,
        &emitted,
        5,
        "turn/interrupt",
        json!({ "threadId": thread_id, "turnId": turn_id }),
    );
    assert_eq!(
        lock(&client.notifications).clone(),
        [("session/cancel".to_string(), json!({ "sessionId": "acp-1" }))]
    );
    client.finish(Ok(json!({ "stopReason": "cancelled" })));
    emitted.wait_for_method("turn/completed");
    assert_eq!(
        emitted.notification("turn/completed").unwrap()["params"]["turn"]["status"],
        "interrupted"
    );
    adapter.close();
}

#[test]
fn refusals_and_process_failures_fail_the_turn() {
    let client = FakeAcpClient::new();
    let (adapter, emitted) = new_adapter(Flavor::Hermes, &client);
    start(&adapter, &emitted);
    client.finish(Ok(json!({ "stopReason": "refusal" })));
    emitted.wait_for_method("turn/completed");
    let turn = &emitted.notification("turn/completed").unwrap()["params"]["turn"];
    assert_eq!(
        (turn["status"].as_str(), turn["error"]["message"].as_str()),
        (Some("failed"), Some("Hermes refused the prompt"))
    );
    adapter.close();

    let client = FakeAcpClient::new();
    let (adapter, emitted) = new_adapter(Flavor::Hermes, &client);
    start(&adapter, &emitted);
    let on_event = lock(&client.event).clone().unwrap();
    on_event(failure_event("Hermes ACP output closed"));
    emitted.wait_for_method("turn/completed");
    let turn = &emitted.notification("turn/completed").unwrap()["params"]["turn"];
    assert_eq!(
        (turn["status"].as_str(), turn["error"]["message"].as_str()),
        (Some("failed"), Some("Hermes ACP output closed"))
    );
    client.finish(Err(AdapterError::failed("Hermes ACP output closed")));
    adapter.close();
}

#[test]
fn classify_answers_agent_requests_and_routes_updates() {
    let message = |v: Value| v.as_object().unwrap().clone();
    match classify(&message(
        json!({ "jsonrpc": "2.0", "id": 7, "method": "session/request_permission", "params": {} }),
    )) {
        Incoming::Reply(reply) => assert_eq!(reply["result"]["outcome"]["outcome"], "cancelled"),
        _ => panic!("permission requests are answered"),
    }
    match classify(&message(
        json!({ "jsonrpc": "2.0", "id": "x", "method": "fs/read_text_file", "params": {} }),
    )) {
        Incoming::Reply(reply) => assert_eq!(reply["error"]["code"], -32601),
        _ => panic!("unsupported requests are refused"),
    }
    match classify(&message(
        json!({ "jsonrpc": "2.0", "id": "ruddr-hermes-1", "result": { "sessionId": "s" } }),
    )) {
        Incoming::Response(id, Ok(result)) => assert_eq!((id.as_str(), result["sessionId"].as_str()), ("ruddr-hermes-1", Some("s"))),
        _ => panic!("responses match by ID"),
    }
    match classify(&message(
        json!({ "jsonrpc": "2.0", "id": 3, "error": { "code": -32000, "message": "no session" } }),
    )) {
        Incoming::Response(id, Err(error)) => assert_eq!((id.as_str(), error.as_str()), ("3", "no session")),
        _ => panic!("errors fail the call"),
    }
    assert!(matches!(
        classify(&message(
            json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "update": {} } })
        )),
        Incoming::Event(_)
    ));
}

#[cfg(unix)]
#[test]
fn the_subprocess_client_initializes_creates_loads_and_sets_the_model() {
    let fake = crate::testing::FakeDir::new();
    let client = SubprocessAcpClient::new(Flavor::Hermes, Duration::from_secs(5), Duration::from_secs(30));
    let mut config = AcpThread {
        flavor: Flavor::Hermes,
        id: String::new(),
        cwd: fake.dir.to_string_lossy().into_owned(),
        model: Some("anthropic:claude-fable-5".into()),
        executable: fake.script("acp"),
        sandbox: "workspace-write".into(),
        resumed: false,
    };
    let session = client.start(&config, Arc::new(|_| {})).unwrap();
    assert_eq!(session, "fake-acp-session");
    assert_eq!(fake.argv(), ["acp"]);
    let methods: Vec<String> = fake
        .records("requests")
        .iter()
        .map(|r| r["method"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(methods, ["initialize", "session/new", "session/set_mode", "session/set_model"]);
    assert_eq!(fake.records("requests")[2]["params"]["modeId"], "accept_edits");
    client.close();

    let client = SubprocessAcpClient::new(Flavor::Hermes, Duration::from_secs(5), Duration::from_secs(30));
    config.resumed = true;
    config.id = "old-session".into();
    config.model = None;
    assert_eq!(client.start(&config, Arc::new(|_| {})).unwrap(), "old-session");
    let last = fake.records("requests").last().cloned().unwrap();
    // The fake's load response offers no modes, so none is set.
    assert_eq!(last["method"], "session/load");
    assert_eq!(last["params"]["sessionId"], "old-session");
    client.close();
}
