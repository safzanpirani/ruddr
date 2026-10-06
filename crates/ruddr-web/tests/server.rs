//! Port of web/server.test.ts against the Rust router. Everything is local:
//! a fake `ruddr` script records launches, a fake controller on a Unix
//! socket records control requests, and no provider ever starts.
#![cfg(unix)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use ruddr_web::app::App;
use ruddr_web::args::WebArguments;
use ruddr_web::files::{self, EVENTS_INITIAL_BYTES};
use ruddr_web::sse;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_stream::StreamExt;

const TOKEN: &str = "tttttttttttttttttttttttttttttttttttttttt";

struct Fixture {
    root: PathBuf,
    state_dir: PathBuf,
    argv_log: PathBuf,
    socket: PathBuf,
    requests: Arc<Mutex<Vec<Value>>>,
    app: Arc<App>,
}

impl Fixture {
    async fn new() -> Fixture {
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("ruddr-web-test-{}", ruddr_core::fsutil::random_hex(6)));
        let state_dir = root.join(".scratch").join("run");
        std::fs::create_dir_all(&state_dir).unwrap();
        let argv_log = root.join("argv.log");
        std::fs::write(&argv_log, "").unwrap();
        let fake = root.join("fake-ruddr");
        // Records each call's argv and the contents of any file argument.
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\nwhile [ -d '{log}.gate' ]; do sleep 0.01; done\nprintf '%s\\n' \"$*\" >> '{log}'\nfor a in \"$@\"; do if [ -f \"$a\" ]; then cat \"$a\" >> '{log}'; fi; done\n\
                 printf done > '{log}.done'\n\
                 if [ \"$1\" = models ]; then echo '[{{\"provider\":\"codex\",\"id\":\"fake-model\",\"available\":true}}]'; exit 0; fi\n\
                 if [ \"$1\" = update ]; then echo 'update failed' >&2; exit 1; fi\necho accepted\n",
                log = argv_log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Unix socket paths must stay short; the temp dir can be long on macOS.
        let socket = PathBuf::from(format!("/tmp/rw-{}.sock", ruddr_core::fsutil::random_hex(6)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let fixture = Fixture {
            root: root.clone(),
            state_dir: state_dir.clone(),
            argv_log,
            socket,
            requests,
            app: App::new(arguments(&fake, &root), TOKEN.into()),
        };
        fixture.start_controller();
        fixture.write_state("active", json!({}));
        std::fs::write(state_dir.join("events.jsonl"), "").unwrap();
        fixture.app.refresh_sessions().await;
        fixture
    }

    fn state_json(&self, status: &str, extra: Value) -> Value {
        let dir = self.state_dir.to_string_lossy().into_owned();
        let mut state = json!({
            "version": 1, "provider": "codex", "pid": std::process::id(), "status": status,
            "threadId": "thread-1", "turnId": "turn-1", "model": "", "cwd": self.root, "sandbox": "workspace-write",
            "stateDir": dir, "socketPath": self.socket, "eventsPath": self.state_dir.join("events.jsonl"),
            "tracePath": "", "outputPath": "", "stderrPath": "", "steers": 0,
            "startedAt": "2026-10-02T09:00:00Z", "updatedAt": "2026-10-02T09:00:01Z",
        });
        for (key, value) in extra.as_object().unwrap() {
            if value.is_null() {
                state.as_object_mut().unwrap().remove(key);
            } else {
                state[key] = value.clone();
            }
        }
        state
    }

    fn write_state(&self, status: &str, extra: Value) {
        std::fs::write(self.state_dir.join("state.json"), self.state_json(status, extra).to_string()).unwrap();
    }

    /// A controller that records each request and accepts it.
    fn start_controller(&self) {
        let listener = UnixListener::bind(&self.socket).unwrap();
        let requests = self.requests.clone();
        let reply_state = self.state_json("active", json!({}));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).is_err() {
                    continue;
                }
                requests
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(line.trim()).unwrap_or(Value::Null));
                let reply = json!({ "ok": true, "state": reply_state });
                let _ = stream.write_all(format!("{reply}\n").as_bytes());
            }
        });
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    fn argv_log(&self) -> String {
        std::fs::read_to_string(&self.argv_log).unwrap()
    }

    async fn completed_argv_log(&self) -> String {
        let done = self.argv_log.with_extension("log.done");
        tokio::time::timeout(Duration::from_secs(10), async {
            while !done.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake ruddr did not finish writing argv within 10 seconds");
        self.argv_log()
    }

    fn dir_param(&self) -> String {
        encode(&self.state_dir.to_string_lossy())
    }

    async fn send(&self, request: Request<Body>) -> Response {
        self.app.clone().handle(request).await
    }

    async fn get(&self, path: &str) -> Response {
        self.send(builder(path, "GET", true, false).body(Body::empty()).unwrap()).await
    }

    async fn post(&self, path: &str, body: Value) -> Response {
        self.send(builder(path, "POST", true, true).body(Body::from(body.to_string())).unwrap())
            .await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn arguments(fake: &Path, root: &Path) -> WebArguments {
    let argv: Vec<String> = vec!["--root".into(), root.join(".scratch").to_string_lossy().into_owned()];
    let mut args = ruddr_web::args::parse_web_arguments(&argv, &|_| None).unwrap();
    args.ruddr = fake.to_path_buf();
    args.registries = Some(Vec::new());
    args.config_dir = root.join("config");
    args
}

fn builder(path: &str, method: &str, auth: bool, mutation: bool) -> axum::http::request::Builder {
    let mut builder = Request::builder().method(method).uri(path).header(header::HOST, "127.0.0.1:4519");
    if auth {
        builder = builder.header(header::COOKIE, format!("ruddr_web={TOKEN}"));
    }
    if mutation {
        builder = builder.header("x-ruddr-request", "1");
    }
    builder
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

async fn body_json(response: Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Reads server-sent events from a response body.
struct Events {
    stream: axum::body::BodyDataStream,
    buffer: Vec<u8>,
    raw: String,
}

impl Events {
    fn new(response: Response) -> Events {
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/event-stream; charset=utf-8");
        Events {
            stream: response.into_body().into_data_stream(),
            buffer: Vec::new(),
            raw: String::new(),
        }
    }

    /// The next `event:`/`data:` block, or `None` when the stream ends.
    async fn next(&mut self) -> Option<(String, Value)> {
        loop {
            if let Some(end) = self.buffer.windows(2).position(|w| w == b"\n\n") {
                let block = String::from_utf8(self.buffer.drain(..end + 2).collect()).unwrap();
                let field = |name: &str| block.lines().find_map(|line| line.strip_prefix(name)).map(str::to_string);
                if let (Some(event), Some(data)) = (field("event: "), field("data: ")) {
                    return Some((event, serde_json::from_str(&data).unwrap()));
                }
                continue;
            }
            let chunk = tokio::time::timeout(Duration::from_secs(10), self.stream.next())
                .await
                .expect("stream timed out")?;
            let chunk = chunk.unwrap();
            self.raw.push_str(&String::from_utf8_lossy(&chunk));
            self.buffer.extend_from_slice(&chunk);
        }
    }

    /// Everything until the stream ends, or `None` if it stays open past `limit`.
    async fn drain(&mut self, limit: Duration) -> Option<String> {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            match tokio::time::timeout_at(deadline, self.stream.next()).await {
                Err(_) => return None,
                Ok(None) => return Some(self.raw.clone()),
                Ok(Some(chunk)) => self.raw.push_str(&String::from_utf8_lossy(&chunk.unwrap())),
            }
        }
    }
}

// ---------------------------------------------------------------- auth

#[tokio::test]
async fn rejects_api_calls_without_the_token() {
    let f = Fixture::new().await;
    let anonymous = f.send(builder("/api/meta", "GET", false, false).body(Body::empty()).unwrap()).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(f.get("/api/meta").await.status(), StatusCode::OK);
    let bearer = Request::builder()
        .uri("/api/meta")
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let response = f.send(bearer).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let meta = body_json(response).await;
    assert_eq!(meta["theme"], "ruddr");
    assert_eq!(meta["providers"], json!(["codex", "claude", "opencode", "pi", "omp", "droid"]));
    assert!(meta["themes"].as_array().unwrap().len() > 10);
    assert!(meta.get("updateAvailable").is_none());
}

#[tokio::test]
async fn trades_a_valid_query_token_for_an_http_only_cookie() {
    let f = Fixture::new().await;
    let response = f
        .send(
            builder(&format!("/?token={TOKEN}"), "GET", false, false)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/");
    let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(cookie.contains("HttpOnly; SameSite=Strict"), "{cookie}");
    assert!(cookie.starts_with(&format!("ruddr_web={TOKEN};")));
    let wrong = f
        .send(builder("/?token=wrong", "GET", false, false).body(Body::empty()).unwrap())
        .await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    let page = f.send(builder("/", "GET", false, false).body(Body::empty()).unwrap()).await;
    assert_eq!(page.status(), StatusCode::OK);
    assert!(page.headers()[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/html"));
    assert_eq!(
        f.send(builder("/nope.js", "GET", false, false).body(Body::empty()).unwrap())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn requires_the_custom_header_and_a_matching_origin_on_mutations() {
    let f = Fixture::new().await;
    let plain = f
        .send(builder("/api/stop", "POST", true, false).body(Body::from("{}")).unwrap())
        .await;
    assert_eq!(plain.status(), StatusCode::FORBIDDEN);
    let cross = builder("/api/stop", "POST", true, true)
        .header(header::ORIGIN, "https://evil.example")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(f.send(cross).await.status(), StatusCode::FORBIDDEN);
    let same = builder("/api/stop", "POST", true, true)
        .header(header::ORIGIN, "http://127.0.0.1:4519")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(
        f.send(same).await.status(),
        StatusCode::CONFLICT,
        "same-origin passes the check and reaches the route"
    );
    assert!(f.requests().is_empty());
}

#[tokio::test]
async fn creates_a_private_token_file_once() {
    let f = Fixture::new().await;
    let file = f.root.join("config").join("web-token");
    let first = ruddr_web::token::load_token(&file).unwrap();
    assert_eq!(first.len(), 32);
    assert_eq!(ruddr_web::token::load_token(&file).unwrap(), first);
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(file.parent().unwrap()), 0o700);
}

#[tokio::test]
async fn token_creation_is_race_safe_repairs_modes_and_refuses_links() {
    let f = Fixture::new().await;
    let file = f.root.join("token-race");
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let file = file.clone();
            std::thread::spawn(move || ruddr_web::token::load_token(&file).unwrap())
        })
        .collect();
    let tokens: std::collections::HashSet<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(tokens.len(), 1);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(&ruddr_web::token::load_token(&file).unwrap(), tokens.iter().next().unwrap());
    assert_eq!(mode(&file), 0o600);
    let link = f.root.join("token-link");
    std::os::unix::fs::symlink(&file, &link).unwrap();
    assert!(ruddr_web::token::load_token(&link).is_err());
    let invalid = f.root.join("invalid-token");
    std::fs::write(&invalid, "invalid").unwrap();
    let error = ruddr_web::token::load_token(&invalid).unwrap_err();
    assert!(error.message.contains("invalid"), "{error}");
    assert_eq!(std::fs::read_to_string(&invalid).unwrap(), "invalid");
}

#[tokio::test]
async fn rejects_malformed_cookies_and_login_bodies_without_throwing() {
    let f = Fixture::new().await;
    let malformed = builder("/api/meta", "GET", false, false)
        .header(header::COOKIE, "ruddr_web=%")
        .body(Body::empty())
        .unwrap();
    assert_eq!(f.send(malformed).await.status(), StatusCode::UNAUTHORIZED);
    for token in [Value::Null, json!(42), json!({}), json!("wrong")] {
        let login = builder("/api/login", "POST", false, true)
            .body(Body::from(json!({ "token": token }).to_string()))
            .unwrap();
        assert_eq!(f.send(login).await.status(), StatusCode::UNAUTHORIZED, "{token}");
    }
    let null = builder("/api/login", "POST", false, true).body(Body::from("null")).unwrap();
    assert_eq!(f.send(null).await.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn requires_same_origin_login() {
    let f = Fixture::new().await;
    let body = || Body::from(json!({ "token": TOKEN }).to_string());
    assert_eq!(
        f.send(builder("/api/login", "POST", false, false).body(body()).unwrap())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let evil = builder("/api/login", "POST", false, true)
        .header(header::ORIGIN, "https://evil.example")
        .body(body())
        .unwrap();
    assert_eq!(f.send(evil).await.status(), StatusCode::FORBIDDEN);
    let response = f.send(builder("/api/login", "POST", false, true).body(body()).unwrap()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(response.headers()[header::SET_COOKIE].to_str().unwrap().contains("HttpOnly"));
    assert_eq!(body_json(response).await, json!({ "ok": true }));
}

// ---------------------------------------------------------------- sessions

#[tokio::test]
async fn lists_verified_sessions_with_their_state_file() {
    let f = Fixture::new().await;
    let sessions = body_json(f.get("/api/sessions").await).await;
    assert_eq!(sessions.as_array().unwrap().len(), 1);
    assert_eq!(sessions[0]["stateDir"], f.state_dir.to_string_lossy().as_ref());
    assert_eq!(sessions[0]["stateFile"], f.state_dir.join("state.json").to_string_lossy().as_ref());
    assert_eq!(sessions[0]["status"], "active");
}

#[tokio::test]
async fn refuses_to_read_files_of_unknown_directories() {
    let f = Fixture::new().await;
    assert_eq!(
        f.get(&format!("/api/run/output?dir={}", encode("/etc"))).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.get(&format!("/api/run/events?dir={}", encode(&f.root.to_string_lossy())))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(f.get("/api/run/activity").await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn steers_with_the_expected_turn_and_never_converts_the_route() {
    let f = Fixture::new().await;
    let dir = f.state_dir.to_string_lossy().into_owned();
    let stale = f
        .post("/api/prompt", json!({ "stateDir": dir, "route": "prompt", "message": "hi" }))
        .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    let moved = f
        .post(
            "/api/prompt",
            json!({ "stateDir": dir, "route": "steer", "turnId": "old", "message": "hi" }),
        )
        .await;
    assert_eq!(moved.status(), StatusCode::CONFLICT);
    assert!(f.requests().is_empty());
    let ok = f
        .post(
            "/api/prompt",
            json!({ "stateDir": dir, "route": "steer", "turnId": "turn-1", "message": "  go left \n" }),
        )
        .await;
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(body_json(ok).await, json!({ "status": "steered turn turn-1" }));
    assert_eq!(
        f.requests(),
        vec![json!({ "command": "steer", "text": "go left", "expectedTurnId": "turn-1" })]
    );
    assert_eq!(f.argv_log(), "", "steering never launches a run");
}

#[tokio::test]
async fn prompts_idle_sessions_over_the_control_socket() {
    let f = Fixture::new().await;
    f.write_state("idle", json!({ "turnId": null }));
    let dir = f.state_dir.to_string_lossy().into_owned();
    let response = f
        .post("/api/prompt", json!({ "stateDir": dir, "route": "prompt", "message": "next" }))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(f.requests(), vec![json!({ "command": "prompt", "text": "next" })]);
    let empty = f
        .post("/api/prompt", json!({ "stateDir": dir, "route": "prompt", "message": "   " }))
        .await;
    assert_eq!(empty.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn interrupts_active_turns_and_stops_idle_sessions() {
    let f = Fixture::new().await;
    let mut body = json!({ "stateDir": f.state_dir, "status": "active", "turnId": "turn-1" });
    assert_eq!(f.post("/api/stop", body.clone()).await.status(), StatusCode::OK);
    f.write_state("idle", json!({}));
    body["status"] = json!("idle");
    let stopped = f.post("/api/stop", body.clone()).await;
    assert_eq!(stopped.status(), StatusCode::OK);
    assert_eq!(body_json(stopped).await, json!({ "status": "shutdown requested" }));
    f.write_state("completed", json!({}));
    assert_eq!(f.post("/api/stop", body).await.status(), StatusCode::CONFLICT);
    assert_eq!(
        f.requests(),
        vec![
            json!({ "command": "interrupt", "expectedTurnId": "turn-1" }),
            json!({ "command": "shutdown" })
        ]
    );
}

#[tokio::test]
async fn stop_rejects_changed_or_missing_delivery_intent() {
    let f = Fixture::new().await;
    for body in [
        json!({ "stateDir": f.state_dir }),
        json!({ "stateDir": f.state_dir, "status": "active" }),
        json!({ "stateDir": f.state_dir, "status": "active", "turnId": "previous-turn" }),
        json!({ "stateDir": f.state_dir, "status": "idle" }),
    ] {
        assert_eq!(f.post("/api/stop", body).await.status(), StatusCode::CONFLICT);
    }
    f.write_state("idle", json!({}));
    assert_eq!(
        f.post(
            "/api/stop",
            json!({ "stateDir": f.state_dir, "status": "active", "turnId": "turn-1" })
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert!(f.requests().is_empty(), "a stale stop must never reach the controller");
}

#[tokio::test]
async fn a_dead_controller_reads_as_stale_and_is_never_controlled() {
    let f = Fixture::new().await;
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    f.write_state("active", json!({ "pid": dead }));
    let dir = f.state_dir.to_string_lossy().into_owned();
    let steer = f
        .post(
            "/api/prompt",
            json!({ "stateDir": dir, "route": "steer", "turnId": "turn-1", "message": "x" }),
        )
        .await;
    assert_eq!(steer.status(), StatusCode::CONFLICT);
    assert!(body_json(steer).await["error"].as_str().unwrap().contains("stale"));
    assert_eq!(f.post("/api/stop", json!({ "stateDir": dir })).await.status(), StatusCode::CONFLICT);
    assert!(f.requests().is_empty());
}

#[tokio::test]
async fn streams_the_event_log_as_a_reset_and_then_appends_whole_lines() {
    let f = Fixture::new().await;
    let events = f.state_dir.join("events.jsonl");
    std::fs::write(&events, "{\"a\":1}\n{\"b\":").unwrap();
    let mut stream = Events::new(f.get(&format!("/api/run/events?dir={}", f.dir_param())).await);
    assert_eq!(
        stream.next().await.unwrap(),
        ("reset".into(), json!({ "text": "{\"a\":1}\n", "truncated": false }))
    );
    assert!(stream.raw.starts_with("retry: 1500\n\n"));
    let mut file = std::fs::OpenOptions::new().append(true).open(&events).unwrap();
    file.write_all(b"2}\n{\"c\":3}\n").unwrap();
    assert_eq!(
        stream.next().await.unwrap(),
        ("append".into(), json!({ "text": "{\"b\":2}\n{\"c\":3}\n" }))
    );
}

// ---------------------------------------------------------------- session boundary regressions

#[tokio::test]
async fn ignores_artifact_paths_supplied_by_state_metadata_and_refuses_symlink_artifacts() {
    let f = Fixture::new().await;
    let secret = f.root.join("outside-output");
    std::fs::write(&secret, "OUTSIDE").unwrap();
    std::fs::write(f.state_dir.join("output.md"), "INSIDE").unwrap();
    f.write_state("completed", json!({ "outputPath": secret }));
    f.app.refresh_sessions().await;
    let response = f.get(&format!("/api/run/output?dir={}", f.dir_param())).await;
    assert_eq!(body_json(response).await, json!({ "text": "INSIDE" }));
    std::fs::remove_file(f.state_dir.join("output.md")).unwrap();
    std::os::unix::fs::symlink(&secret, f.state_dir.join("output.md")).unwrap();
    let refused = f.get(&format!("/api/run/output?dir={}", f.dir_param())).await;
    assert_eq!(refused.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!body_json(refused).await.to_string().contains("OUTSIDE"));
}

#[tokio::test]
async fn does_not_discover_a_state_dir_outside_the_directory_holding_state_json() {
    let f = Fixture::new().await;
    let forged = f.root.join(".scratch").join("forged");
    std::fs::create_dir_all(&forged).unwrap();
    let mut state = f.state_json("completed", json!({}));
    state["stateDir"] = json!(f.root);
    std::fs::write(forged.join("state.json"), state.to_string()).unwrap();
    f.app.refresh_sessions().await;
    assert!(f.app.session_for(Some(&f.root.to_string_lossy())).is_none());
    assert_eq!(f.app.sessions().len(), 1);
}

#[tokio::test]
async fn rejects_steering_without_a_turn_id_and_never_relaunches_rejected_steers() {
    let f = Fixture::new().await;
    let dir = f.state_dir.to_string_lossy().into_owned();
    f.write_state("active", json!({ "turnId": null }));
    let response = f
        .post("/api/prompt", json!({ "stateDir": dir, "route": "steer", "message": "left" }))
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    f.write_state("completed", json!({}));
    let stale = f
        .post(
            "/api/prompt",
            json!({ "stateDir": dir, "route": "steer", "turnId": "turn-1", "message": "left" }),
        )
        .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(f.argv_log(), "");
    assert!(f.requests().is_empty());
}

#[tokio::test]
async fn rejects_cached_directory_replacements_and_closes_an_existing_event_stream() {
    let f = Fixture::new().await;
    let outside = f.root.join("outside-run");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("output.md"), "OUTSIDE_FIXTURE").unwrap();
    std::fs::write(outside.join("events.jsonl"), "{\"outside\":true}\n").unwrap();
    let mut stream = Events::new(f.get(&format!("/api/run/events?dir={}", f.dir_param())).await);
    assert_eq!(stream.next().await.unwrap().0, "reset");
    let saved = f.root.join(".scratch").join("run.saved");
    std::fs::rename(&f.state_dir, &saved).unwrap();
    std::os::unix::fs::symlink(&outside, &f.state_dir).unwrap();
    for route in ["output", "activity", "events", "diff"] {
        let response = f.get(&format!("/api/run/{route}?dir={}", f.dir_param())).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{route}");
    }
    let text = stream.drain(Duration::from_secs(2)).await.expect("the stream closes on its own");
    assert!(!text.contains("outside"), "{text}");
    f.app.refresh_sessions().await;
    assert!(f.app.session_for(Some(&f.state_dir.to_string_lossy())).is_none());
    // A fresh directory at the same path is a different directory.
    std::fs::remove_file(&f.state_dir).unwrap();
    std::fs::create_dir(&f.state_dir).unwrap();
    f.write_state("active", json!({}));
    f.app.refresh_sessions().await;
    assert!(f.app.session_for(Some(&f.state_dir.to_string_lossy())).is_none());
}

// ---------------------------------------------------------------- stream regressions

#[test]
fn retains_a_complete_record_when_the_tail_starts_exactly_on_a_line_boundary() {
    let dir = scratch("aligned");
    let file = dir.join("aligned-boundary");
    let kept = "{\"text\":\"😀\"}\n";
    std::fs::write(&file, format!("{{}}\n{kept}")).unwrap();
    let tail = files::read_aligned_tail(&file, kept.len() as u64).unwrap();
    assert_eq!(tail.text, kept);
    assert_eq!(tail.offset, (3 + kept.len()) as u64);
    assert!(tail.pending.is_empty());
    assert!(!tail.skipping);
    assert!(tail.truncated);
}

#[test]
fn rejects_a_log_replaced_between_the_size_probe_and_the_range_read() {
    let dir = scratch("rotation");
    let file = dir.join("rotation-race");
    std::fs::write(&file, "{}\n").unwrap();
    let tail = files::read_aligned_tail(&file, 100).unwrap();
    std::fs::rename(&file, dir.join("rotation-race.old")).unwrap();
    std::fs::write(&file, "{\"replacement\":true}\n").unwrap();
    assert_eq!(
        files::read_range(&file, tail.offset, tail.offset + 4, &tail.identity).unwrap(),
        None
    );
    let replacement = files::read_aligned_tail(&file, 100).unwrap();
    assert_eq!(replacement.text, "{\"replacement\":true}\n");
    let bytes = files::read_range(&file, 0, replacement.offset, &replacement.identity)
        .unwrap()
        .unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), replacement.text);
}

#[test]
fn skips_an_oversized_completed_record_and_recovers_the_next_record() {
    let limit = EVENTS_INITIAL_BYTES;
    let mut record = json!({ "text": "x".repeat(limit as usize + 100) }).to_string().into_bytes();
    record.push(b'\n');
    let split = 4 * 1024 * 1024;
    let first = files::consume_event_bytes(&[], false, &record[..split], limit);
    assert_eq!(first.pending.len(), split);
    let mut rest = record[split..].to_vec();
    rest.extend_from_slice("{\"valid\":\"😀\"}\n".as_bytes());
    let next = files::consume_event_bytes(&first.pending, first.skipping, &rest, limit);
    assert_eq!(next.text, "{\"valid\":\"😀\"}\n");
    assert!(next.pending.is_empty());
    assert!(next.oversized);
    assert!(!next.skipping);
    let unfinished = files::consume_event_bytes(&[], false, &vec![b'x'; limit as usize + 1], limit);
    assert!(unfinished.skipping);
    assert!(unfinished.pending.is_empty());
    assert_eq!(
        files::consume_event_bytes(&unfinished.pending, unfinished.skipping, b"end\n{}\n", limit).text,
        "{}\n"
    );
    assert_eq!(files::consume_event_bytes(&[], false, b"1234567\n{}\n", 8).text, "1234567\n{}\n");
}

#[test]
fn keeps_byte_offsets_and_partial_utf8_across_tail_boundaries() {
    let dir = scratch("utf8");
    let file = dir.join("utf8-events");
    let bytes = "{\"text\":\"é\"}\n{\"text\":\"😀\"}\n".as_bytes();
    let partial = &bytes[..bytes.len() - 4];
    std::fs::write(&file, partial).unwrap();
    let tail = files::read_aligned_tail(&file, partial.len() as u64 - 1).unwrap();
    assert_eq!(tail.offset, partial.len() as u64);
    assert_eq!(tail.text, "");
    let mut joined = tail.pending.clone();
    joined.extend_from_slice(&bytes[partial.len()..]);
    assert_eq!(String::from_utf8(joined).unwrap(), "{\"text\":\"😀\"}\n");
}

#[tokio::test]
async fn cleans_up_cancellation_and_bounded_slow_readers() {
    let (starts, cleanups) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let counted = |starts: Arc<AtomicUsize>, cleanups: Arc<AtomicUsize>| {
        move |_sender: sse::SseSender| -> Box<dyn FnOnce() + Send> {
            starts.fetch_add(1, Ordering::SeqCst);
            Box::new(move || {
                cleanups.fetch_add(1, Ordering::SeqCst);
            })
        }
    };
    // The client goes away: dropping the body runs the cleanup once.
    let response = sse::event_stream(counted(starts.clone(), cleanups.clone()));
    drop(response);
    assert_eq!((starts.load(Ordering::SeqCst), cleanups.load(Ordering::SeqCst)), (1, 1));
    // A reader that falls 48 MiB behind is closed while the producer runs.
    let slow_cleanups = cleanups.clone();
    let slow = sse::event_stream(move |sender| {
        let mut accepted = 0;
        for _ in 0..60 {
            if sender.send("append", &json!({ "text": "x".repeat(1024 * 1024) })) {
                accepted += 1;
            }
        }
        assert!(accepted < 60 && sender.is_closed());
        Box::new(move || {
            slow_cleanups.fetch_add(1, Ordering::SeqCst);
        })
    });
    assert_eq!(
        cleanups.load(Ordering::SeqCst),
        2,
        "a producer that closes before returning still gets cleaned up"
    );
    let mut stream = Events::new(slow);
    let mut appended = 0;
    while let Some((event, _)) = stream.next().await {
        assert_eq!(event, "append");
        appended += 1;
    }
    assert!(appended > 0 && appended < 60);
    assert_eq!(cleanups.load(Ordering::SeqCst), 2, "the cleanup runs once");
}

#[tokio::test]
async fn streams_records_larger_than_an_append_chunk_and_resets_after_rotation() {
    let f = Fixture::new().await;
    let file = f.state_dir.join("events.jsonl");
    std::fs::write(&file, "").unwrap();
    let mut stream = Events::new(f.get(&format!("/api/run/events?dir={}", f.dir_param())).await);
    assert_eq!(stream.next().await.unwrap().0, "reset");
    let record = format!("{}\n", json!({ "text": format!("é{}", "x".repeat(4 * 1024 * 1024 + 100)) }));
    std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap()
        .write_all(record.as_bytes())
        .unwrap();
    let (event, data) = stream.next().await.unwrap();
    assert_eq!(event, "append");
    assert_eq!(data["text"].as_str().unwrap(), record);
    std::fs::rename(&file, f.state_dir.join("events.jsonl.old")).unwrap();
    // Write the replacement whole, so the stream never sees it empty.
    let replacement = f.state_dir.join("events.jsonl.new");
    std::fs::write(&replacement, "{\"new\":true}\n").unwrap();
    std::fs::rename(&replacement, &file).unwrap();
    assert_eq!(
        stream.next().await.unwrap(),
        ("reset".into(), json!({ "text": "{\"new\":true}\n", "truncated": false }))
    );
    std::fs::write(&file, "{}\n").unwrap();
    assert_eq!(stream.next().await.unwrap().0, "reset");
}

#[tokio::test]
async fn reports_a_problem_for_a_symlinked_event_log() {
    let f = Fixture::new().await;
    let outside = f.root.join("outside-events");
    std::fs::write(&outside, "{\"outside\":true}\n").unwrap();
    std::fs::remove_file(f.state_dir.join("events.jsonl")).unwrap();
    std::os::unix::fs::symlink(&outside, f.state_dir.join("events.jsonl")).unwrap();
    let mut stream = Events::new(f.get(&format!("/api/run/events?dir={}", f.dir_param())).await);
    let (event, data) = stream.next().await.unwrap();
    assert_eq!(event, "problem");
    assert!(!data.to_string().contains("outside\\\":true"));
}

// ---------------------------------------------------------------- launches

#[tokio::test]
async fn new_sessions_validate_cwd_and_continuations_launch_detached_with_private_artifacts() {
    let f = Fixture::new().await;
    let invalid = f.post("/api/new", json!({ "cwd": f.root.join("missing"), "message": "go" })).await;
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert_eq!(f.argv_log(), "");
    f.write_state("completed", json!({ "model": "gpt-x", "effort": "high" }));
    let dir = f.state_dir.to_string_lossy().into_owned();
    let response = f
        .post(
            "/api/prompt",
            json!({ "stateDir": dir, "route": "continue", "message": "follow-up" }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let result = body_json(response).await;
    assert_eq!(result["status"], "Started a new run for thread thread-1");
    let started = PathBuf::from(result["stateDir"].as_str().unwrap());
    assert_eq!(started.parent().unwrap(), f.root.join(".scratch").join("ruddr-tui"));
    assert_eq!(
        std::fs::read_to_string(started.parent().unwrap().join(".gitignore")).unwrap(),
        "*\n"
    );
    assert_eq!(mode(&started), 0o700);
    assert_eq!(mode(&started.join("prompt.md")), 0o600);
    assert_eq!(mode(&started.join("launch.stderr.log")), 0o600);
    assert_eq!(std::fs::read_to_string(started.join("prompt.md")).unwrap(), "follow-up\n");
    let log = f.completed_argv_log().await;
    let expected = format!(
        "run --detach --provider codex --cwd {root} --resume-thread thread-1 --prompt-file {dir}/prompt.md --state-dir {dir} \
         --sandbox workspace-write --approval-policy never --idle --model gpt-x --effort high\nfollow-up\n",
        root = f.root.display(),
        dir = started.display()
    );
    assert_eq!(log, expected);
    assert!(f.requests().is_empty(), "a continuation never touches the old controller");
}

#[tokio::test]
async fn new_sessions_launch_with_the_chosen_provider_and_model() {
    let f = Fixture::new().await;
    let work = f.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let unknown = f
        .post("/api/new", json!({ "provider": "gemini", "cwd": work, "message": "go" }))
        .await;
    assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);
    let response =
        f.post("/api/new", json!({ "provider": "droid", "model": "glm-5.3-flash", "effort": "", "cwd": format!(" {} ", work.display()), "message": " start \n" })).await;
    assert_eq!(response.status(), StatusCode::OK);
    let result = body_json(response).await;
    assert_eq!(result["status"], "Started droid session");
    let started = PathBuf::from(result["stateDir"].as_str().unwrap());
    assert!(started.starts_with(work.join(".scratch").join("ruddr-tui")));
    let expected = format!(
        "run --detach --provider droid --cwd {work} --prompt-file {dir}/prompt.md --state-dir {dir} --sandbox workspace-write \
         --approval-policy never --idle --model glm-5.3-flash\nstart\n",
        work = work.display(),
        dir = started.display()
    );
    assert_eq!(f.completed_argv_log().await, expected);
}

#[tokio::test]
async fn launch_log_waits_for_the_fake_after_the_startup_window() {
    let f = Fixture::new().await;
    let gate = f.argv_log.with_extension("log.gate");
    std::fs::create_dir(&gate).unwrap();
    let dir = ruddr_web::launch::launch_session(
        &f.root.join("fake-ruddr"),
        &f.root,
        "gated prompt",
        |prompt, state| {
            Ok(vec![
                "run".into(),
                "--prompt-file".into(),
                prompt.display().to_string(),
                "--state-dir".into(),
                state.display().to_string(),
            ])
        },
        |_| {},
        Duration::ZERO,
    )
    .unwrap();
    // The accepted launch still has a live child. HTTP acceptance cannot fence log reads.
    assert_eq!(f.argv_log(), "");
    std::fs::remove_dir(gate).unwrap();
    assert_eq!(
        f.completed_argv_log().await,
        format!(
            "run --detach --prompt-file {dir}/prompt.md --state-dir {dir}\ngated prompt\n",
            dir = dir.display()
        )
    );
}

#[tokio::test]
async fn reports_a_launch_that_fails_during_startup() {
    let f = Fixture::new().await;
    let failing = f.root.join("failing-ruddr");
    std::fs::write(&failing, "#!/bin/sh\necho 'provider exploded' >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&failing, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut args = arguments(&failing, &f.root);
    args.ruddr = failing;
    let app = App::new(args, TOKEN.into());
    let request = builder("/api/new", "POST", true, true)
        .body(Body::from(json!({ "cwd": f.root, "message": "go" }).to_string()))
        .unwrap();
    let response = app.handle(request).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body_json(response).await["error"], "provider exploded");
}

// ---------------------------------------------------------------- other routes

#[tokio::test]
async fn lists_models_from_the_binary() {
    let f = Fixture::new().await;
    let models = body_json(f.get("/api/models").await).await;
    assert_eq!(models, json!([{ "provider": "codex", "id": "fake-model", "available": true }]));
    assert!(f.argv_log().starts_with("models --json\n"));
}

#[tokio::test]
async fn serves_output_activity_and_diff_for_verified_sessions() {
    let f = Fixture::new().await;
    std::fs::write(f.state_dir.join("output.md"), "done").unwrap();
    std::fs::write(f.state_dir.join("trace.log"), "2026-10-02T09:00:00Z [say] hello\n").unwrap();
    assert_eq!(
        body_json(f.get(&format!("/api/run/output?dir={}", f.dir_param())).await).await,
        json!({ "text": "done" })
    );
    let activity = body_json(f.get(&format!("/api/run/activity?dir={}", f.dir_param())).await).await;
    assert_eq!(activity["activities"][0]["text"], "hello");
    let diff = body_json(f.get(&format!("/api/run/diff?dir={}&force=1", f.dir_param())).await).await;
    assert_eq!(diff["cwd"], f.root.to_string_lossy().as_ref());
    // The fixture root is not a Git repository: the run's recorded edits stand in.
    assert_eq!(diff["recorded"], "Not a Git repository", "{diff}");
    assert!(diff.get("error").is_none(), "no Git usage text reaches the page: {diff}");
    assert_eq!((diff["content"].as_str(), &diff["untracked"]), (Some(""), &json!([])));
    let edit = json!({"method": "item/completed", "params": {"item": {
        "type": "fileChange", "id": "e1", "status": "completed", "toolName": "Write",
        "input": {"file_path": f.root.join("notes.md").to_string_lossy(), "content": "hello\n"}}}});
    std::fs::write(f.state_dir.join("events.jsonl"), format!("{edit}\n")).unwrap();
    let diff = body_json(f.get(&format!("/api/run/diff?dir={}&force=1", f.dir_param())).await).await;
    let content = diff["content"].as_str().unwrap();
    assert!(content.starts_with("diff --git a/notes.md b/notes.md\nnew file mode"), "{content}");
}

#[tokio::test]
async fn deletes_finished_sessions_and_saves_themes() {
    let f = Fixture::new().await;
    let dir = f.state_dir.to_string_lossy().into_owned();
    let live = f.post("/api/delete", json!({ "stateDir": dir })).await;
    assert_eq!(
        live.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "an active session is never deleted"
    );
    assert!(f.state_dir.exists());
    f.write_state("completed", json!({}));
    let deleted = f.post("/api/delete", json!({ "stateDir": dir })).await;
    assert_eq!(body_json(deleted).await, json!({ "status": "Session deleted" }));
    assert!(!f.state_dir.exists());
    assert!(f.app.sessions().is_empty());

    assert_eq!(
        f.post("/api/theme", json!({ "name": "nope" })).await.status(),
        StatusCode::BAD_REQUEST
    );
    let saved = f.post("/api/theme", json!({ "name": "tokyonight" })).await;
    assert_eq!(body_json(saved).await, json!({ "status": "Theme Tokyo Night saved" }));
    assert_eq!(body_json(f.get("/api/meta").await).await["theme"], "tokyonight");
    assert_eq!(mode(&f.root.join("config").join("tui.json")), 0o600);
    assert_eq!(f.post("/api/update", json!({})).await.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn serves_over_loopback() {
    let f = Fixture::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = ruddr_web::router(f.app.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let request = format!("GET /api/meta HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n");
    let response = tokio::task::spawn_blocking(move || {
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut text = String::new();
        std::io::Read::read_to_string(&mut stream, &mut text).unwrap();
        text
    })
    .await
    .unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("\"dejaAvailable\""));
    server.abort();
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ruddr-web-{name}-{}", ruddr_core::fsutil::random_hex(6)));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
