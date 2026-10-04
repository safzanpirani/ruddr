//! Port of opencode/runtime.test.ts, plus an end-to-end run against a fake
//! `opencode2 serve --stdio`.

use super::*;
use crate::protocol::tests::Collector;
use crate::protocol::{Latch, handle};
use std::collections::VecDeque;
use std::net::TcpListener;

#[derive(Default)]
struct FakeBackend {
    prompts: Mutex<Vec<(String, bool)>>,
    interrupted: AtomicBool,
    steer_gate: Mutex<Option<Arc<Latch>>>,
    snapshots: Mutex<VecDeque<Snapshot>>,
    arrived: Condvar,
}

impl FakeBackend {
    fn finish(&self, snapshot: Snapshot) {
        lock(&self.snapshots).push_back(snapshot);
        self.arrived.notify_all();
    }
    fn prompts(&self) -> Vec<(String, bool)> {
        lock(&self.prompts).clone()
    }
}

impl Backend for Arc<FakeBackend> {
    fn open(&self, thread: &mut OpenCodeThread, resumed: bool) -> AResult<String> {
        Ok(if resumed { thread.id.clone() } else { "ses_test".into() })
    }
    fn prompt(&self, _session: &str, text: &str, steer: bool, _effort: Option<&str>) -> AResult<String> {
        let count = {
            let mut prompts = lock(&self.prompts);
            prompts.push((text.to_string(), steer));
            prompts.len()
        };
        let gate = lock(&self.steer_gate).clone();
        if steer && let Some(gate) = gate {
            gate.wait(Duration::from_secs(15));
        }
        Ok(format!("msg_{count}"))
    }
    fn wait(&self, _session: &str) -> AResult<Snapshot> {
        let mut snapshots = lock(&self.snapshots);
        loop {
            if let Some(snapshot) = snapshots.pop_front() {
                return Ok(snapshot);
            }
            snapshots = self.arrived.wait(snapshots).unwrap();
        }
    }
    fn interrupt(&self, _session: &str) -> AResult<()> {
        self.interrupted.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn close(&self, _session: Option<&str>, _remove: bool) {}
}

fn adapter(backend: &Arc<FakeBackend>) -> (Arc<OpenCodeAdapter>, Arc<Collector>) {
    let emitted = Collector::new();
    let adapter = Arc::new(OpenCodeAdapter::with_backend(
        emitted.clone(),
        "opencode2".into(),
        Box::new(backend.clone()),
    ));
    (adapter, emitted)
}

fn call(adapter: &OpenCodeAdapter, emitted: &Collector, id: i64, method: &str, params: Value) {
    handle(
        adapter,
        emitted,
        json!({ "id": id, "method": method, "params": params }).as_object().unwrap(),
    );
}

fn assistant(id: &str, content: Value) -> Map<String, Value> {
    json!({ "id": id, "type": "assistant", "content": content })
        .as_object()
        .unwrap()
        .clone()
}

#[test]
fn preserves_same_turn_steering_and_normalizes_final_output() {
    let backend = Arc::new(FakeBackend::default());
    let (adapter, emitted) = adapter(&backend);
    call(&adapter, &emitted, 1, "initialize", json!({}));
    call(
        &adapter,
        &emitted,
        2,
        "thread/start",
        json!({ "cwd": "/tmp/work", "sandbox": "workspace-write", "model": "openrouter/deepseek/model" }),
    );
    call(
        &adapter,
        &emitted,
        3,
        "turn/start",
        json!({ "threadId": "ses_test", "input": [{ "type": "text", "text": "first" }] }),
    );
    let turn_id = emitted.result(json!(3))["turn"]["id"].as_str().unwrap().to_string();
    call(
        &adapter,
        &emitted,
        4,
        "turn/steer",
        json!({ "threadId": "ses_test", "expectedTurnId": turn_id, "input": [{ "type": "text", "text": "correction" }] }),
    );
    assert_eq!(backend.prompts(), [("first".to_string(), false), ("correction".to_string(), true)]);
    assert_eq!(emitted.result(json!(4))["turnId"], turn_id.as_str());

    backend.finish(Snapshot {
        outcome: Some("succeeded".into()),
        messages: vec![assistant(
            "msg_assistant",
            json!([{ "type": "reasoning", "text": "checked" }, { "type": "text", "text": "OPENCODE_OK" }]),
        )],
        tokens: json!({ "input": 10, "output": 4, "cache": { "read": 2 } }).as_object().cloned(),
        cost: 0.01,
        context_window: 0.0,
    });
    emitted.wait_for_method("turn/completed");
    let users: Vec<Value> = emitted
        .completed_items()
        .into_iter()
        .filter(|item| item["type"] == "userMessage")
        .collect();
    assert_eq!(
        users.iter().map(|item| item["text"].as_str().unwrap()).collect::<Vec<_>>(),
        ["correction"]
    );
    assert!(
        emitted
            .completed_items()
            .iter()
            .any(|item| item["type"] == "reasoning" && item["summary"][0]["text"] == "checked")
    );
    let message = emitted
        .completed_items()
        .into_iter()
        .find(|item| item["type"] == "agentMessage")
        .unwrap();
    assert_eq!(
        (message["text"].as_str(), message["phase"].as_str()),
        (Some("OPENCODE_OK"), Some("final_answer"))
    );
    let usage = emitted.notification("thread/tokenUsage/updated").unwrap();
    assert_eq!(usage["params"]["tokenUsage"]["total"]["totalTokens"], 16);
    let completed = emitted.notification("turn/completed").unwrap();
    assert_eq!(
        completed["params"],
        json!({ "threadId": "ses_test", "turn": { "id": turn_id, "status": "completed" } })
    );
    adapter.close();
}

#[test]
fn waits_for_an_accepted_steer_and_the_following_idle_snapshot() {
    let backend = Arc::new(FakeBackend::default());
    let gate = Latch::new();
    *lock(&backend.steer_gate) = Some(gate.clone());
    let (adapter, emitted) = adapter(&backend);
    call(&adapter, &emitted, 1, "initialize", json!({}));
    call(
        &adapter,
        &emitted,
        2,
        "thread/start",
        json!({ "cwd": "/tmp/work", "sandbox": "workspace-write" }),
    );
    call(
        &adapter,
        &emitted,
        3,
        "turn/start",
        json!({ "threadId": "ses_test", "input": [{ "type": "text", "text": "first" }] }),
    );
    let turn_id = emitted.result(json!(3))["turn"]["id"].as_str().unwrap().to_string();
    let steering = {
        let (adapter, emitted) = (adapter.clone(), emitted.clone());
        thread::spawn(move || {
            call(
                &adapter,
                &emitted,
                4,
                "turn/steer",
                json!({ "threadId": "ses_test", "expectedTurnId": turn_id, "input": [{ "type": "text", "text": "correction" }] }),
            )
        })
    };
    while backend.prompts().len() < 2 {
        thread::sleep(Duration::from_millis(2));
    }
    backend.finish(Snapshot {
        outcome: Some("succeeded".into()),
        messages: vec![assistant("early", json!([{ "type": "text", "text": "EARLY" }]))],
        ..Default::default()
    });
    thread::sleep(Duration::from_millis(30));
    assert!(emitted.notification("turn/completed").is_none());

    gate.set();
    steering.join().unwrap();
    backend.finish(Snapshot {
        outcome: Some("succeeded".into()),
        messages: vec![assistant("final", json!([{ "type": "text", "text": "FINAL" }]))],
        ..Default::default()
    });
    emitted.wait_for_method("turn/completed");
    assert!(!emitted.text().contains("EARLY"));
    assert!(emitted.text().contains("FINAL"));
    adapter.close();
}

#[test]
fn interrupt_marks_the_turn_interrupted() {
    let backend = Arc::new(FakeBackend::default());
    let (adapter, emitted) = adapter(&backend);
    call(&adapter, &emitted, 1, "initialize", json!({}));
    call(
        &adapter,
        &emitted,
        2,
        "thread/start",
        json!({ "cwd": "/tmp/work", "sandbox": "workspace-write" }),
    );
    call(
        &adapter,
        &emitted,
        3,
        "turn/start",
        json!({ "threadId": "ses_test", "input": [{ "type": "text", "text": "first" }] }),
    );
    let turn_id = emitted.result(json!(3))["turn"]["id"].as_str().unwrap().to_string();
    call(
        &adapter,
        &emitted,
        4,
        "turn/interrupt",
        json!({ "threadId": "ses_test", "turnId": turn_id }),
    );
    assert!(backend.interrupted.load(Ordering::SeqCst));
    backend.finish(Snapshot {
        outcome: Some("succeeded".into()),
        ..Default::default()
    });
    emitted.wait_for_method("turn/completed");
    assert_eq!(
        emitted.notification("turn/completed").unwrap()["params"]["turn"]["status"],
        "interrupted"
    );
    adapter.close();
}

type Handler = Arc<dyn Fn(&str, &str, &mut TcpStream) + Send + Sync>;

/// A loopback HTTP server that logs request paths and lets the handler
/// write any response, including none.
fn fake_http(handler: Handler) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let paths = Arc::new(Mutex::new(Vec::new()));
    let log = paths.clone();
    thread::spawn(move || {
        for stream in listener.incoming().map_while(Result::ok) {
            let (handler, log) = (handler.clone(), log.clone());
            thread::spawn(move || {
                let mut stream = stream;
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header.trim().is_empty() {
                        break;
                    }
                    if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let mut parts = request.split_whitespace();
                let method = parts.next().unwrap().to_string();
                let path = parts.next().unwrap().to_string();
                lock(&log).push(path.clone());
                handler(&method, &path, &mut stream);
            });
        }
    });
    (base, paths)
}

fn respond(stream: &mut TcpStream, status: u16, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

#[test]
fn http_requests_time_out_and_server_announcements_stay_on_loopback() {
    let (base, _) = fake_http(Arc::new(|_, _, _| thread::sleep(Duration::from_secs(5))));
    let backend = HttpBackend::new(Duration::from_millis(10));
    backend.attach(&base, "private");
    let error = backend.prompt("ses_test", "hello", false, None).unwrap_err();
    assert!(error.message.contains("timed out after 10ms"), "{error}");

    assert_eq!(validated_loopback_url("http://localhost:4096/").unwrap(), "http://localhost:4096");
    assert_eq!(validated_loopback_url("http://[::1]:4096").unwrap(), "http://[::1]:4096");
    assert_eq!(validated_loopback_url("http://127.0.0.1:80").unwrap(), "http://127.0.0.1");
    assert!(
        validated_loopback_url("http://192.0.2.1:4096")
            .unwrap_err()
            .message
            .contains("loopback host")
    );
    assert!(
        validated_loopback_url("http://user:secret@127.0.0.1:4096")
            .unwrap_err()
            .message
            .contains("credentials")
    );
    assert!(
        validated_loopback_url("http://127.0.0.1:4096/api")
            .unwrap_err()
            .message
            .contains("must not contain")
    );
    assert!(
        validated_loopback_url("http://127.0.0.1:4096/?q=1")
            .unwrap_err()
            .message
            .contains("must not contain")
    );
    assert!(
        validated_loopback_url("ftp://127.0.0.1")
            .unwrap_err()
            .message
            .contains("loopback host")
    );
    assert!(validated_loopback_url("not a url").unwrap_err().message.contains("valid URL"));
    backend.close(None, false);
}

#[test]
fn waits_on_experimental_session_routes_and_falls_back_to_legacy_routes() {
    for legacy in [false, true] {
        let (base, paths) = fake_http(Arc::new(move |_, path, stream| {
            if path.starts_with("/api/experimental/") == legacy {
                respond(stream, 404, "");
            } else if path.ends_with("/wait") {
                let _ = write!(stream, "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
            } else {
                respond(
                    stream,
                    200,
                    r#"{"data":{"info":{"outcome":"success","cost":0.5},"messages":[{"id":"m1"}]}}"#,
                );
            }
        }));
        let backend = HttpBackend::new(Duration::from_secs(1));
        backend.attach(&base, "test");
        let snapshot = backend.wait("ses_test").unwrap();
        assert_eq!(snapshot.outcome.as_deref(), Some("success"));
        assert_eq!(snapshot.cost, 0.5);
        assert_eq!(snapshot.messages, vec![json!({ "id": "m1" }).as_object().unwrap().clone()]);
        let experimental = [
            "/api/experimental/session/ses_test/wait",
            "/api/experimental/session/ses_test/export",
        ];
        let expected: Vec<&str> = if legacy {
            vec![
                experimental[0],
                "/api/session/ses_test/wait",
                experimental[1],
                "/api/session/ses_test/export",
            ]
        } else {
            experimental.to_vec()
        };
        assert_eq!(*lock(&paths), expected);
        backend.close(None, false);
    }
}

#[test]
fn does_not_retry_session_routes_after_a_non_404_failure() {
    let (base, paths) = fake_http(Arc::new(|_, _, stream| respond(stream, 500, "boom")));
    let backend = HttpBackend::new(Duration::from_secs(1));
    backend.attach(&base, "test");
    assert_eq!(backend.wait("ses_test").unwrap_err().message, "OpenCode API 500: boom");
    assert_eq!(*lock(&paths), ["/api/experimental/session/ses_test/wait"]);
    backend.close(None, false);
}

#[test]
fn keeps_response_bodies_bounded_by_timeout_and_shutdown() {
    for close_early in [false, true] {
        let started = Latch::new();
        let body_started = started.clone();
        let (base, _) = fake_http(Arc::new(move |_, _, stream| {
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{{\"id\":");
            let _ = stream.flush();
            body_started.set();
            thread::sleep(Duration::from_secs(5));
        }));
        let backend = Arc::new(HttpBackend::new(if close_early {
            Duration::from_secs(10)
        } else {
            Duration::from_millis(20)
        }));
        backend.attach(&base, "test");
        let pending = {
            let backend = backend.clone();
            thread::spawn(move || backend.prompt("ses_test", "hello", false, None))
        };
        assert!(started.wait(Duration::from_secs(5)));
        if close_early {
            thread::sleep(Duration::from_millis(10));
            backend.close(None, false);
        }
        let error = pending.join().unwrap().unwrap_err();
        assert!(error.message.contains(if close_early { "closing" } else { "timed out" }), "{error}");
        backend.close(None, false);
    }
}

#[test]
fn rejects_oversized_or_invalid_http_headers_lengths_and_chunks() {
    for (response, expected) in [
        (
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", usize::MAX),
            "exceeds 64 MiB",
        ),
        (
            "HTTP/1.1 200 OK\r\nContent-Length: invalid\r\n\r\n".into(),
            "invalid content length",
        ),
        (
            format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n", usize::MAX),
            "exceeds 64 MiB",
        ),
        (
            format!("HTTP/1.1 200 OK\r\nX-Large: {}\r\n\r\n", "x".repeat(MAX_HTTP_HEADER_BYTES)),
            "header exceeds 64 KiB",
        ),
        (
            format!("HTTP/1.1 200 OK\r\n{}\r\n", "X-Small: x\r\n".repeat(MAX_HTTP_HEADER_BYTES / 8)),
            "headers exceed 64 KiB",
        ),
    ] {
        let (base, _) = fake_http(Arc::new(move |_, _, stream| {
            let _ = stream.write_all(response.as_bytes());
        }));
        let backend = HttpBackend::new(Duration::from_secs(1));
        backend.attach(&base, "test");
        let error = backend.prompt("ses_test", "hello", false, None).unwrap_err();
        assert!(error.message.contains(expected), "{error}");
        backend.close(None, false);
    }
}

#[test]
fn a_continuous_partial_http_header_does_not_extend_the_deadline() {
    let (base, _) = fake_http(Arc::new(move |_, _, stream| {
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nX-Trickle: ");
        for _ in 0..200 {
            if stream.write_all(b"x").is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }));
    let backend = HttpBackend::new(Duration::from_millis(50));
    backend.attach(&base, "test");
    let started = Instant::now();
    let error = backend.prompt("ses_test", "hello", false, None).unwrap_err();
    assert!(error.message.contains("timed out"), "{error}");
    assert!(started.elapsed() < Duration::from_millis(500));
    backend.close(None, false);
}

#[test]
fn installs_distinct_ruddr_agents_without_discarding_inline_config() {
    let existing = json!({ "theme": "ruddr", "agents": { "existing": { "mode": "primary" } } }).to_string();
    let config: Value = serde_json::from_str(&ruddr_config_content(Some("read-only"), Some(&existing)).unwrap()).unwrap();
    assert_eq!(config["theme"], "ruddr");
    assert_eq!(config["default_agent"], "ruddr-read-only");
    assert_eq!(config["agents"]["existing"]["mode"], "primary");
    let permissions = |agent: &str| config["agents"][agent]["permissions"].as_array().unwrap().clone();
    assert!(permissions("ruddr-read-only").contains(&json!({ "action": "*", "resource": "*", "effect": "deny" })));
    assert!(permissions("ruddr-workspace-write").contains(&json!({ "action": "external_directory", "resource": "*", "effect": "deny" })));
    assert_eq!(
        permissions("ruddr-danger-full-access"),
        [json!({ "action": "*", "resource": "*", "effect": "allow" })]
    );
    assert!(ruddr_config_content(None, Some("[1]")).is_err());
}

#[test]
fn encodes_uri_components_and_base64() {
    assert_eq!(encode_uri_component("ses/a b?"), "ses%2Fa%20b%3F");
    assert_eq!(base64(b"opencode:pw"), "b3BlbmNvZGU6cHc=");
    assert_eq!(base64(b"ab"), "YWI=");
    assert_eq!(base64(b""), "");
}

#[cfg(unix)]
#[test]
fn runs_a_turn_against_a_fake_opencode_server() {
    let fake = crate::testing::FakeDir::new();
    let emitted = Collector::new();
    let adapter = OpenCodeAdapter::new(emitted.clone(), fake.script("opencode"));
    let cwd = fake.dir.to_string_lossy().into_owned();
    call(&adapter, &emitted, 1, "initialize", json!({}));
    call(
        &adapter,
        &emitted,
        2,
        "thread/start",
        json!({ "cwd": cwd, "sandbox": "read-only", "model": "openrouter/deepseek/model", "ephemeral": true }),
    );
    assert_eq!(emitted.result(json!(2))["thread"]["id"], "ses_fake");
    assert_eq!(fake.argv(), ["serve", "--stdio"]);
    let config: Value = serde_json::from_str(&fake.read("config.json")).unwrap();
    assert_eq!(config["default_agent"], "ruddr-read-only");

    call(
        &adapter,
        &emitted,
        3,
        "turn/start",
        json!({ "threadId": "ses_fake", "input": [{ "type": "text", "text": "hello" }] }),
    );
    assert_eq!(emitted.result(json!(3))["turn"]["id"], "msg_1");
    emitted.wait_for_method("turn/completed");
    let items = emitted.completed_items();
    assert_eq!(items.iter().find(|i| i["type"] == "agentMessage").unwrap()["text"], "FAKE_OK");
    let tool = items.iter().find(|i| i["id"] == "call_1").unwrap();
    assert_eq!((tool["command"].as_str(), tool["output"].as_str()), (Some("ls"), Some("README.md")));
    let usage = emitted.notification("thread/tokenUsage/updated").unwrap();
    assert_eq!(
        usage["params"]["tokenUsage"]["total"],
        json!({ "inputTokens": 9, "cachedInputTokens": 2, "outputTokens": 4, "totalTokens": 13 })
    );
    assert_eq!(usage["params"]["costUsd"], 0.5);
    adapter.close();

    let requests = fake.records("requests.jsonl");
    assert!(requests.iter().all(|request| request["auth"] == true), "{requests:?}");
    assert_eq!(
        requests[0],
        json!({ "method": "POST", "path": "/api/session", "auth": true, "body": {
            "location": { "directory": cwd }, "agent": "ruddr-read-only",
            "model": { "providerID": "openrouter", "id": "deepseek/model" },
        } })
    );
    assert_eq!(requests[1]["body"], json!({ "text": "hello" }));
    // An ephemeral session is deleted when the adapter closes.
    assert_eq!(requests.last().unwrap()["method"], "DELETE");
}

#[cfg(unix)]
#[test]
fn resume_model_and_effort_reach_the_session_api_before_the_prompt() {
    for (model, effort) in [
        (None, None),
        (None, Some("high")),
        (Some("new/model"), None),
        (Some("new/model#low"), None),
        (Some("new/model#low"), Some("high")),
    ] {
        let fake = crate::testing::FakeDir::new();
        let emitted = Collector::new();
        let adapter = OpenCodeAdapter::new(emitted.clone(), fake.script("opencode"));
        adapter.dispatch("initialize", &json!({})).unwrap();
        let response = adapter
            .dispatch(
                "thread/resume",
                &json!({
                    "threadId": "ses_fake", "cwd": fake.dir, "sandbox": "read-only", "model": model,
                }),
            )
            .unwrap();
        assert_eq!(response["model"], if model.is_some() { "new/model" } else { "stored/original" });
        assert_eq!(
            response["reasoningEffort"],
            if model == Some("new/model") { Value::Null } else { json!("low") }
        );
        adapter
            .dispatch(
                "turn/start",
                &json!({
                    "threadId": "ses_fake", "effort": effort, "input": [{ "type": "text", "text": "hello" }],
                }),
            )
            .unwrap();
        emitted.wait_for_method("turn/completed");
        adapter.close();
        let requests = fake.records("requests.jsonl");
        let prompt_index = requests.iter().position(|r| r["path"] == "/api/session/ses_fake/prompt").unwrap();
        let switches: Vec<_> = requests
            .iter()
            .enumerate()
            .filter(|(_, r)| r["path"] == "/api/session/ses_fake/model")
            .collect();
        assert_eq!(switches.len(), usize::from(model.is_some()) + usize::from(effort.is_some()));
        for (index, _) in &switches {
            assert!(*index < prompt_index);
        }
        if let Some((_, switch)) = switches.last() {
            let mut expected = if model.is_some() {
                json!({ "providerID": "new", "id": "model" })
            } else {
                json!({ "providerID": "stored", "id": "original" })
            };
            if let Some(variant) = effort.or_else(|| model.and_then(|m| m.split_once('#').map(|(_, v)| v))) {
                expected["variant"] = json!(variant);
            }
            assert_eq!(switch["body"], json!({ "model": expected }));
        }
        assert_eq!(requests[prompt_index]["body"], json!({ "text": "hello" }));
    }
}

#[cfg(unix)]
#[test]
fn fresh_effort_is_applied_and_a_rejected_variant_never_sends_a_prompt() {
    for reject in [false, true] {
        let fake = crate::testing::FakeDir::new();
        let emitted = Collector::new();
        let adapter = OpenCodeAdapter::new(emitted.clone(), fake.script("opencode"));
        adapter.dispatch("initialize", &json!({})).unwrap();
        adapter
            .dispatch("thread/start", &json!({ "cwd": fake.dir, "model": "new/model" }))
            .unwrap();
        if reject {
            fake.write("reject_model", "yes");
        }
        let result = adapter.dispatch(
            "turn/start",
            &json!({
                "threadId": "ses_fake", "effort": "high", "input": [{ "type": "text", "text": "hello" }],
            }),
        );
        if reject {
            assert!(result.unwrap_err().message.contains("model switch rejected"));
        } else {
            result.unwrap();
            emitted.wait_for_method("turn/completed");
        }
        adapter.close();
        let requests = fake.records("requests.jsonl");
        let switch = requests.iter().find(|r| r["path"] == "/api/session/ses_fake/model").unwrap();
        assert_eq!(
            switch["body"],
            json!({ "model": { "providerID": "new", "id": "model", "variant": "high" } })
        );
        assert_eq!(requests.iter().any(|r| r["path"] == "/api/session/ses_fake/prompt"), !reject);
    }
}

#[cfg(unix)]
#[test]
fn a_rejected_resume_model_does_not_return_a_configured_thread() {
    let fake = crate::testing::FakeDir::new();
    fake.write("reject_model", "yes");
    let emitted = Collector::new();
    let adapter = OpenCodeAdapter::new(emitted, fake.script("opencode"));
    adapter.dispatch("initialize", &json!({})).unwrap();
    let error = adapter
        .dispatch(
            "thread/resume",
            &json!({ "threadId": "ses_fake", "cwd": fake.dir, "model": "new/model" }),
        )
        .unwrap_err();
    assert!(error.message.contains("model switch rejected"));
    assert!(lock(&adapter.inner.state).thread.is_none());
    adapter.close();
    assert!(
        !fake
            .records("requests.jsonl")
            .iter()
            .any(|r| r["path"] == "/api/session/ses_fake/prompt")
    );
}
