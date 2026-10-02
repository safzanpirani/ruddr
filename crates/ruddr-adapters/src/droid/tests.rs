//! Port of droid/runtime.test.ts.

use super::*;
use crate::protocol::handle;
use crate::protocol::tests::Collector;

#[derive(Default)]
struct FakeDroidClient {
    requests: Mutex<Vec<(String, Value, Option<String>)>>,
    event: Mutex<Option<EventFn>>,
    executable: Mutex<Option<String>>,
}

impl FakeDroidClient {
    fn notify(&self, notification: Value) {
        self.notify_session(notification, "droid-session");
    }
    fn notify_session(&self, notification: Value, session: &str) {
        let on_event = lock(&self.event).clone().unwrap();
        on_event(
            json!({ "sessionId": session, "notification": notification })
                .as_object()
                .unwrap()
                .clone(),
        );
    }
    fn requests(&self) -> Vec<(String, Value, Option<String>)> {
        lock(&self.requests).clone()
    }
    fn methods(&self) -> Vec<String> {
        self.requests().into_iter().map(|(method, _, _)| method).collect()
    }
}

impl DroidClient for Arc<FakeDroidClient> {
    fn start(&self, executable: &str, _cwd: &str, on_event: EventFn) -> AResult<()> {
        *lock(&self.executable) = Some(executable.into());
        *lock(&self.event) = Some(on_event);
        Ok(())
    }
    fn request(&self, method: &str, params: Value, id: Option<String>, _timeout: Option<Duration>) -> AResult<Map<String, Value>> {
        lock(&self.requests).push((method.into(), params, id));
        let result = match method {
            "droid.initialize_session" => json!({ "sessionId": "droid-session" }),
            "droid.fork_session" => json!({ "newSessionId": "droid-fork" }),
            "droid.get_context_stats" => json!({ "used": 40, "remaining": 960, "limit": 1000 }),
            _ => json!({}),
        };
        Ok(result.as_object().cloned().unwrap())
    }
    fn close(&self) {}
}

struct Started {
    adapter: DroidAdapter,
    client: Arc<FakeDroidClient>,
    emitted: Arc<Collector>,
    thread_id: String,
    turn_id: String,
}

impl Started {
    fn call(&self, id: i64, method: &str, params: Value) {
        handle(
            &self.adapter,
            self.emitted.as_ref(),
            json!({ "id": id, "method": method, "params": params }).as_object().unwrap(),
        );
    }
    fn completed_items(&self) -> Vec<Value> {
        self.emitted.completed_items()
    }
}

fn adapter(pickup: Duration) -> (DroidAdapter, Arc<FakeDroidClient>, Arc<Collector>) {
    let client = Arc::new(FakeDroidClient::default());
    let emitted = Collector::new();
    let adapter = DroidAdapter::with_client(emitted.clone(), "droid".into(), Box::new(client.clone()), pickup);
    (adapter, client, emitted)
}

fn call(adapter: &DroidAdapter, emitted: &Collector, id: i64, method: &str, params: Value) {
    handle(
        adapter,
        emitted,
        json!({ "id": id, "method": method, "params": params }).as_object().unwrap(),
    );
}

fn start_thread(sandbox: &str) -> Started {
    let (adapter, client, emitted) = adapter(STEER_PICKUP_TIMEOUT);
    call(&adapter, &emitted, 1, "initialize", json!({}));
    call(
        &adapter,
        &emitted,
        2,
        "thread/start",
        json!({ "cwd": "/tmp/work", "sandbox": sandbox, "model": "glm-5.3-flash", "providerPath": "/opt/droid", "ephemeral": false }),
    );
    let thread_id = emitted.result(json!(2))["thread"]["id"].as_str().unwrap().to_string();
    call(
        &adapter,
        &emitted,
        3,
        "turn/start",
        json!({ "threadId": thread_id, "input": [{ "type": "text", "text": "first" }] }),
    );
    let turn_id = emitted.result(json!(3))["turn"]["id"].as_str().unwrap().to_string();
    Started {
        adapter,
        client,
        emitted,
        thread_id,
        turn_id,
    }
}

#[test]
fn starts_a_session_reports_tools_and_messages_and_completes_the_turn() {
    let s = start_thread("workspace-write");
    assert_eq!(s.thread_id, "droid-session");
    assert_eq!(lock(&s.client.executable).as_deref(), Some("/opt/droid"));
    let requests = s.client.requests();
    assert_eq!(requests[0].0, "droid.initialize_session");
    let params = &requests[0].1;
    assert_eq!(params["cwd"], "/tmp/work");
    assert_eq!(params["modelId"], "glm-5.3-flash");
    assert_eq!(params["autonomyLevel"], "medium");
    assert_eq!(params["autoRejectPermissionRequests"], true);
    assert!(params["machineId"].as_str().is_some_and(|m| !m.is_empty()));
    assert_eq!(requests[1], ("droid.add_user_message".into(), json!({ "text": "first" }), None));

    let c = &s.client;
    c.notify(json!({ "type": "tool_call", "toolUse": { "id": "tool-1", "name": "Execute", "input": {} } }));
    c.notify(json!({ "type": "tool_call", "toolUse": { "id": "tool-1", "name": "Execute", "input": { "command": "echo one" } } }));
    c.notify(json!({ "type": "tool_result", "toolUseId": "tool-1", "content": "one\n", "isError": false }));
    // A subagent's events carry its own session ID and stay out of the turn.
    c.notify_session(
        json!({ "type": "tool_call", "toolUse": { "id": "child-tool", "name": "Read", "input": {} } }),
        "child-session",
    );
    c.notify(json!({ "type": "assistant_text_delta", "messageId": "m-2", "blockIndex": 1, "textDelta": "DROID" }));
    c.notify(json!({ "type": "create_message", "message": { "id": "m-2", "role": "assistant",
        "content": [{ "type": "thinking", "thinking": "checked" }, { "type": "text", "text": "DROID_OK" }] } }));
    c.notify(json!({ "type": "session_token_usage_changed", "sessionId": "droid-session",
        "tokenUsage": { "inputTokens": 100, "outputTokens": 10, "cacheReadTokens": 50, "cacheCreationTokens": 0 },
        "lastCallTokenUsage": { "inputTokens": 20, "outputTokens": 5, "cacheReadTokens": 10 } }));
    c.notify(json!({ "type": "agent_turn_completed", "reason": "completed", "turnId": "droid-turn" }));
    s.emitted.wait_for_method("turn/completed");

    let items = s.completed_items();
    let tool = items.iter().find(|i| i["id"] == "tool-1").unwrap();
    assert_eq!(
        (
            tool["type"].as_str(),
            tool["status"].as_str(),
            tool["command"].as_str(),
            tool["aggregatedOutput"].as_str()
        ),
        (Some("commandExecution"), Some("completed"), Some("echo one"), Some("one\n"))
    );
    assert!(!s.emitted.text().contains("child-tool"));
    let updated = s.emitted.notification("item/updated").unwrap();
    assert_eq!(
        (&updated["params"]["item"]["id"], &updated["params"]["item"]["command"]),
        (&json!("tool-1"), &json!("echo one"))
    );
    let delta = s.emitted.notification("item/agentMessage/delta").unwrap();
    assert_eq!(
        (&delta["params"]["itemId"], &delta["params"]["delta"]),
        (&json!("m-2-text-1"), &json!("DROID"))
    );
    assert_eq!(
        items.iter().find(|i| i["type"] == "reasoning").unwrap()["summary"][0]["text"],
        "checked"
    );
    let message = items.iter().find(|i| i["type"] == "agentMessage").unwrap();
    assert_eq!(
        (message["id"].as_str(), message["text"].as_str(), message["phase"].as_str()),
        (Some("m-2-text-1"), Some("DROID_OK"), Some("final_answer"))
    );
    let usage = s.emitted.notifications("thread/tokenUsage/updated").pop().unwrap();
    assert_eq!(
        usage["params"]["tokenUsage"],
        json!({ "total": { "inputTokens": 150, "cachedInputTokens": 50, "outputTokens": 10, "totalTokens": 160 },
            "last": { "totalTokens": 40 }, "modelContextWindow": 1000 })
    );
    assert_eq!(
        s.emitted.notification("turn/completed").unwrap()["params"]["turn"]["status"],
        "completed"
    );
    s.adapter.close();
}

#[test]
fn narration_beside_a_tool_call_is_commentary() {
    let s = start_thread("workspace-write");
    let c = &s.client;
    c.notify(json!({ "type": "create_message", "message": { "id": "m-1", "role": "assistant", "content": [{ "type": "text", "text": "looking" }] } }));
    c.notify(json!({ "type": "tool_call", "toolUse": { "id": "tool-1", "name": "Create", "input": { "file_path": "/tmp/work/a.txt" } } }));
    c.notify(json!({ "type": "tool_result", "toolUseId": "tool-1", "content": "ok", "isError": false }));
    c.notify(json!({ "type": "create_message", "message": { "id": "m-2", "role": "assistant", "content": [{ "type": "text", "text": "done" }] } }));
    c.notify(json!({ "type": "agent_turn_completed", "reason": "completed" }));
    s.emitted.wait_for_method("turn/completed");
    let items = s.completed_items();
    let messages: Vec<(String, String)> = items
        .iter()
        .filter(|i| i["type"] == "agentMessage")
        .map(|i| (i["text"].as_str().unwrap().into(), i["phase"].as_str().unwrap().into()))
        .collect();
    assert_eq!(
        messages,
        [("looking".into(), "commentary".into()), ("done".into(), "final_answer".into())]
    );
    let tool = items.iter().find(|i| i["id"] == "tool-1").unwrap();
    assert_eq!(
        (tool["type"].as_str(), tool["command"].as_str()),
        (Some("fileChange"), Some("Create /tmp/work/a.txt"))
    );
    s.adapter.close();
}

#[test]
fn keeps_the_turn_open_until_a_queued_steer_runs() {
    let s = start_thread("workspace-write");
    s.call(
        4,
        "turn/steer",
        json!({ "threadId": s.thread_id, "expectedTurnId": s.turn_id, "input": [{ "type": "text", "text": "correction" }] }),
    );
    assert_eq!(s.emitted.result(json!(4)), json!({ "turnId": s.turn_id }));
    let steer = s
        .client
        .requests()
        .into_iter()
        .find(|(m, p, _)| m == "droid.add_user_message" && p["text"] == "correction")
        .unwrap();
    let steer_id = steer.2.expect("the steer carries its own request ID");
    let users: Vec<Value> = s.completed_items().into_iter().filter(|i| i["type"] == "userMessage").collect();
    assert_eq!(
        users.iter().map(|i| i["text"].as_str().unwrap()).collect::<Vec<_>>(),
        ["correction"]
    );

    // Droid finished its turn before it read the steer, then runs the steer
    // as a new Droid turn.
    s.client.notify(json!({ "type": "agent_turn_completed", "reason": "completed" }));
    thread::sleep(Duration::from_millis(30));
    assert!(s.emitted.notification("turn/completed").is_none());
    s.client.notify(json!({ "type": "create_message", "requestId": steer_id,
        "message": { "id": "u-2", "role": "user", "content": [{ "type": "text", "text": "correction" }] } }));
    s.client.notify(json!({ "type": "create_message", "message": { "id": "m-3", "role": "assistant", "content": [{ "type": "text", "text": "corrected" }] } }));
    s.client.notify(json!({ "type": "agent_turn_completed", "reason": "completed" }));
    s.emitted.wait_for_method("turn/completed");
    assert_eq!(s.emitted.notifications("turn/completed").len(), 1);
    let message = s.completed_items().into_iter().find(|i| i["type"] == "agentMessage").unwrap();
    assert_eq!(
        (message["text"].as_str(), message["phase"].as_str()),
        (Some("corrected"), Some("final_answer"))
    );
    s.adapter.close();
}

#[test]
fn ends_a_deferred_turn_when_droid_discards_the_queued_steer() {
    let s = start_thread("workspace-write");
    s.call(
        4,
        "turn/steer",
        json!({ "threadId": s.thread_id, "expectedTurnId": s.turn_id, "input": [{ "type": "text", "text": "late" }] }),
    );
    s.client.notify(json!({ "type": "agent_turn_completed", "reason": "completed" }));
    thread::sleep(Duration::from_millis(30));
    assert!(s.emitted.notification("turn/completed").is_none());
    s.client.notify(json!({ "type": "queued_messages_discarded" }));
    s.emitted.wait_for_method("turn/completed");
    assert_eq!(
        s.emitted.notification("turn/completed").unwrap()["params"]["turn"]["status"],
        "completed"
    );
    s.adapter.close();
}

#[test]
fn ends_a_deferred_turn_when_the_steer_is_never_picked_up() {
    let (adapter, client, emitted) = adapter(Duration::from_millis(50));
    call(&adapter, &emitted, 1, "initialize", json!({}));
    call(
        &adapter,
        &emitted,
        2,
        "thread/start",
        json!({ "cwd": "/tmp", "sandbox": "workspace-write" }),
    );
    call(
        &adapter,
        &emitted,
        3,
        "turn/start",
        json!({ "threadId": "droid-session", "input": [{ "type": "text", "text": "first" }] }),
    );
    let turn_id = emitted.result(json!(3))["turn"]["id"].as_str().unwrap().to_string();
    call(
        &adapter,
        &emitted,
        4,
        "turn/steer",
        json!({ "threadId": "droid-session", "expectedTurnId": turn_id, "input": [{ "type": "text", "text": "late" }] }),
    );
    client.notify(json!({ "type": "agent_turn_completed", "reason": "completed" }));
    emitted.wait_for_method("turn/completed");
    assert_eq!(
        emitted.notification("turn/completed").unwrap()["params"]["turn"]["status"],
        "completed"
    );
    adapter.close();
}

#[test]
fn interrupts_and_reports_the_turn_as_interrupted() {
    let s = start_thread("workspace-write");
    s.call(4, "turn/interrupt", json!({ "threadId": s.thread_id, "turnId": s.turn_id }));
    assert_eq!(
        s.client.requests().last().unwrap(),
        &("droid.interrupt_session".into(), json!({}), None)
    );
    s.client.notify(json!({ "type": "agent_turn_completed", "reason": "cancelled" }));
    s.emitted.wait_for_method("turn/completed");
    assert_eq!(
        s.emitted.notification("turn/completed").unwrap()["params"]["turn"],
        json!({ "id": s.turn_id, "status": "interrupted" })
    );
    s.adapter.close();
}

#[test]
fn reports_a_rejected_permission_as_a_failed_turn() {
    let s = start_thread("read-only");
    let params = &s.client.requests()[0].1;
    assert_eq!(
        (&params["autonomyLevel"], &params["autoRejectPermissionRequests"]),
        (&json!("off"), &json!(true))
    );
    s.client
        .notify(json!({ "type": "tool_call", "toolUse": { "id": "tool-1", "name": "Execute", "input": { "command": "echo hi" } } }));
    s.client
        .notify(json!({ "type": "agent_turn_completed", "reason": "permission_rejected" }));
    s.emitted.wait_for_method("turn/completed");
    let turn = &s.emitted.notification("turn/completed").unwrap()["params"]["turn"];
    assert_eq!(turn["status"], "failed");
    assert!(turn["error"]["message"].as_str().unwrap().contains("autonomy off"));
    // A tool left open by the turn's end gets a terminal status.
    assert_eq!(
        s.completed_items().into_iter().find(|i| i["id"] == "tool-1").unwrap()["status"],
        "failed"
    );
    s.adapter.close();
}

#[test]
fn resumes_and_forks_sessions_and_rejects_unsupported_thread_options() {
    let (resume, resume_client, resumed) = adapter(STEER_PICKUP_TIMEOUT);
    call(&resume, &resumed, 1, "initialize", json!({}));
    call(
        &resume,
        &resumed,
        2,
        "thread/resume",
        json!({ "threadId": "old-session", "cwd": "/tmp", "sandbox": "danger-full-access", "model": "glm-5.3-flash", "excludeTurns": true }),
    );
    assert_eq!(resumed.result(json!(2)), json!({ "thread": { "id": "old-session" } }));
    assert_eq!(resume_client.methods(), ["droid.load_session", "droid.update_session_settings"]);
    assert_eq!(
        resume_client.requests()[0].1,
        json!({ "sessionId": "old-session", "autoRejectPermissionRequests": true })
    );
    assert_eq!(
        resume_client.requests()[1].1,
        json!({ "modelId": "glm-5.3-flash", "autonomyLevel": "high" })
    );
    resume.close();

    let (fork, fork_client, forked) = adapter(STEER_PICKUP_TIMEOUT);
    call(&fork, &forked, 1, "initialize", json!({}));
    call(
        &fork,
        &forked,
        2,
        "thread/fork",
        json!({ "threadId": "old-session", "cwd": "/tmp", "sandbox": "workspace-write", "lastTurnId": "t-1" }),
    );
    assert!(forked.text().contains("--fork-through-turn"));
    call(
        &fork,
        &forked,
        3,
        "thread/fork",
        json!({ "threadId": "old-session", "cwd": "/tmp", "sandbox": "workspace-write" }),
    );
    assert_eq!(forked.result(json!(3)), json!({ "thread": { "id": "droid-fork" } }));
    let sessions: Vec<(String, Value)> = fork_client
        .requests()
        .into_iter()
        .map(|(m, p, _)| (m, p["sessionId"].clone()))
        .collect();
    assert_eq!(
        sessions,
        [
            ("droid.load_session".into(), json!("old-session")),
            ("droid.fork_session".into(), Value::Null),
            ("droid.load_session".into(), json!("droid-fork")),
            ("droid.update_session_settings".into(), Value::Null),
        ]
    );
    fork.close();

    let (ephemeral, _, rejected) = adapter(STEER_PICKUP_TIMEOUT);
    call(&ephemeral, &rejected, 1, "initialize", json!({}));
    call(
        &ephemeral,
        &rejected,
        2,
        "thread/start",
        json!({ "cwd": "/tmp", "sandbox": "read-only", "ephemeral": true }),
    );
    assert!(rejected.text().contains("--ephemeral is not supported"));
    ephemeral.close();
}

#[test]
fn forks_for_ruddr_thread_fork_without_cwd_or_sandbox() {
    let (fork, client, emitted) = adapter(STEER_PICKUP_TIMEOUT);
    call(&fork, &emitted, 1, "initialize", json!({}));
    call(
        &fork,
        &emitted,
        2,
        "thread/fork",
        json!({ "threadId": "old-session", "excludeTurns": true }),
    );
    assert_eq!(emitted.result(json!(2)), json!({ "thread": { "id": "droid-fork" } }));
    assert_eq!(client.methods(), ["droid.load_session", "droid.fork_session", "droid.load_session"]);
    // Thread history beyond fork stays unsupported and fails at once.
    call(&fork, &emitted, 3, "thread/list", json!({}));
    assert_eq!(emitted.response(json!(3)).unwrap()["error"]["code"], -32601);
    fork.close();
}

#[cfg(unix)]
#[test]
fn client_declines_interactive_requests_and_times_out_unanswered_calls() {
    let fake = crate::testing::FakeDir::new();
    let client = SubprocessDroidClient::new(Duration::from_millis(500));
    let events = Arc::new(Mutex::new(Vec::<Map<String, Value>>::new()));
    let sink = events.clone();
    client
        .start(
            &fake.script("droid"),
            &fake.dir.to_string_lossy(),
            Arc::new(move |event| lock(&sink).push(event)),
        )
        .unwrap();
    let started = client
        .request(
            "droid.initialize_session",
            json!({ "machineId": "test", "cwd": "/tmp" }),
            None,
            Some(Duration::from_secs(30)),
        )
        .unwrap();
    assert_eq!(Value::Object(started), json!({ "sessionId": "droid_test_session" }));
    assert_eq!(
        fake.argv(),
        ["exec", "--input-format", "stream-jsonrpc", "--output-format", "stream-jsonrpc"]
    );
    let responses = || -> Vec<Value> {
        lock(&events)
            .iter()
            .filter(|e| e["notification"]["type"] == "test_server_response")
            .map(|e| e["notification"]["response"].clone())
            .collect()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while responses().len() < 3 {
        assert!(std::time::Instant::now() < deadline, "no server responses");
        thread::sleep(Duration::from_millis(5));
    }
    let responses = responses();
    let find = |id: &str| responses.iter().find(|r| r["id"] == id).cloned().unwrap();
    assert_eq!(find("perm-1")["type"], "response");
    assert_eq!(find("perm-1")["result"], json!({ "selectedOption": "cancel" }));
    assert_eq!(find("perm-1")["factoryApiVersion"], "1.0.0");
    assert_eq!(find("ask-1")["result"], json!({ "cancelled": true, "answers": [] }));
    assert_eq!(find("future-1")["error"]["code"], -32601);

    assert_eq!(
        client.request("droid.fail", json!({}), None, None).unwrap_err().message,
        "session not found"
    );
    let error = client.request("droid.never_respond", json!({}), None, None).unwrap_err();
    assert!(error.message.contains("timed out after 500ms"), "{error}");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !lock(&events).iter().any(|e| e["notification"]["type"] == "ruddr_error") {
        assert!(std::time::Instant::now() < deadline, "no ruddr_error event");
        thread::sleep(Duration::from_millis(5));
    }
    client.close();
}
