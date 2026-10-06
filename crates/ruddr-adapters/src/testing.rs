//! Fake provider CLIs for tests. Each fake is this test binary run again
//! through a small shell wrapper: the wrapper records its argv, sets
//! `RUDDR_ADAPTERS_FAKE`, and runs only `testing::fake_provider_entry`. The
//! test harness prints to stdout, so the wrapper sends that to /dev/null and
//! hands the fake its real stdout as file descriptor 3. Unix only.

#![cfg(unix)]

use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FAKE_ENV: &str = "RUDDR_ADAPTERS_FAKE";
const FAKE_DIR_ENV: &str = "RUDDR_ADAPTERS_FAKE_DIR";

/// A scratch directory holding one fake's wrapper script and its records.
pub struct FakeDir {
    pub dir: PathBuf,
}

impl FakeDir {
    pub fn new() -> FakeDir {
        let dir = std::env::temp_dir().join(format!("ruddr-adapters-{}", ruddr_core::fsutil::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        FakeDir { dir }
    }

    /// Writes the wrapper for a fake of `kind` and returns its path.
    pub fn script(&self, kind: &str) -> String {
        let exe = std::env::current_exe().unwrap();
        let path = self.dir.join(format!("fake-{kind}"));
        let dir = self.dir.display();
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{dir}/argv'\nexport {FAKE_ENV}='{kind}'\nexport {FAKE_DIR_ENV}='{dir}'\n\
             exec '{}' --exact testing::fake_provider_entry --nocapture --test-threads=1 -q 3>&1 1>/dev/null\n",
            exe.display()
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn write(&self, name: &str, contents: &str) {
        std::fs::write(self.path(name), contents).unwrap();
    }

    pub fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.path(name)).unwrap_or_default()
    }

    pub fn argv(&self) -> Vec<String> {
        self.read("argv").lines().map(str::to_string).collect()
    }

    /// The JSON lines a fake recorded in `name`.
    pub fn records(&self, name: &str) -> Vec<Value> {
        self.read(name).lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
    }

    /// Waits up to 15 seconds for the recorded lines to satisfy `predicate`.
    pub fn wait_for(&self, name: &str, predicate: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let records = self.records(name);
            if predicate(&records) {
                return records;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {name}: {records:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for FakeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn fake_provider_entry() {
    let Ok(kind) = std::env::var(FAKE_ENV) else { return };
    let dir = PathBuf::from(std::env::var(FAKE_DIR_ENV).unwrap());
    // SAFETY: the wrapper script opens descriptor 3 as this process's stdout.
    let out = Arc::new(Mutex::new(unsafe { File::from_raw_fd(3) }));
    let code = match kind.as_str() {
        "pi" => fake_pi(&out),
        "acp" => fake_acp(&dir, &out),
        "droid" => fake_droid(&out),
        "claude" => fake_claude(&dir, &out),
        "opencode" => fake_opencode(&dir, &out),
        other => panic!("unknown fake {other}"),
    };
    std::process::exit(code);
}

fn write(out: &Mutex<File>, message: &Value) {
    let mut file = out.lock().unwrap();
    let _ = writeln!(file, "{message}");
    let _ = file.flush();
}

fn append(path: &Path, line: &str) {
    let mut file = OpenOptions::new().create(true).append(true).open(path).unwrap();
    writeln!(file, "{line}").unwrap();
}

fn stdin_lines() -> impl Iterator<Item = Value> {
    BufReader::new(std::io::stdin())
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
}

/// The stand-in for `pi --mode rpc` from pi/testdata/fake-pi.ts.
fn fake_pi(out: &Mutex<File>) -> i32 {
    for command in stdin_lines() {
        let id = command["id"].clone();
        match command["type"].as_str().unwrap_or_default() {
            "get_state" => {
                write(
                    out,
                    &json!({ "type": "response", "id": id, "success": true, "data": { "sessionId": "pi_test_session" } }),
                );
                write(
                    out,
                    &json!({ "type": "extension_ui_request", "id": "ui-select", "method": "select" }),
                );
                write(
                    out,
                    &json!({ "type": "extension_ui_request", "id": "ui-unknown", "method": "futurePrompt" }),
                );
                write(
                    out,
                    &json!({ "type": "extension_ui_request", "id": "ui-notify", "method": "notify" }),
                );
            }
            "extension_ui_response" => write(out, &json!({ "type": "test_ui_response", "response": command })),
            "never_respond" => {}
            _ => write(out, &json!({ "type": "response", "id": id, "success": true })),
        }
    }
    0
}

/// A stand-in for `hermes acp` / `openclaw acp`: it records each request in
/// `requests`, answers `initialize` with `loadSession`, and gives
/// `session/new` a fixed session ID.
fn fake_acp(dir: &Path, out: &Mutex<File>) -> i32 {
    for message in stdin_lines() {
        append(&dir.join("requests"), &message.to_string());
        let Some(id) = message.get("id").cloned() else { continue };
        let result = match message["method"].as_str().unwrap_or_default() {
            "initialize" => json!({ "protocolVersion": 1, "agentCapabilities": { "loadSession": true } }),
            "session/new" => json!({ "sessionId": "fake-acp-session",
                "modes": { "currentModeId": "default", "availableModes": [{ "id": "default" }, { "id": "accept_edits" }] } }),
            _ => json!({}),
        };
        write(out, &json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }
    0
}

/// The stand-in for `droid exec --input-format stream-jsonrpc` from
/// droid/testdata/fake-droid.ts. It sends three server requests after
/// session start, reports the answers as notifications, and never answers
/// `droid.never_respond`.
fn fake_droid(out: &Mutex<File>) -> i32 {
    let send = |message: Value| {
        let mut envelope = json!({ "jsonrpc": "2.0", "factoryApiVersion": "1.0.0" });
        envelope.as_object_mut().unwrap().extend(message.as_object().unwrap().clone());
        write(out, &envelope);
    };
    let notify = |notification: Value| {
        send(json!({
            "type": "notification",
            "method": "droid.session_notification",
            "params": { "sessionId": "droid_test_session", "notification": notification },
        }))
    };
    for message in stdin_lines() {
        if message["type"] == "response" {
            notify(json!({ "type": "test_server_response", "response": message }));
            continue;
        }
        let id = message["id"].clone();
        match message["method"].as_str().unwrap_or_default() {
            "droid.initialize_session" => {
                send(json!({ "type": "response", "id": id, "result": { "sessionId": "droid_test_session" } }));
                send(json!({ "type": "request", "id": "perm-1", "method": "droid.request_permission",
                    "params": { "toolUses": [], "options": [{ "value": "proceed_once" }, { "value": "cancel" }] } }));
                send(
                    json!({ "type": "request", "id": "ask-1", "method": "droid.ask_user", "params": { "toolCallId": "tool-ask", "questions": [] } }),
                );
                send(json!({ "type": "request", "id": "future-1", "method": "droid.future_prompt", "params": {} }));
            }
            "droid.fail" => send(json!({ "type": "response", "id": id, "error": { "code": -32004, "message": "session not found" } })),
            "droid.never_respond" => {}
            _ => send(json!({ "type": "response", "id": id, "result": {} })),
        }
    }
    0
}

/// A stand-in for `claude -p --input-format stream-json`. It records its
/// environment and every stdin line, answers `initialize`, and on the first
/// user message replays `replay.jsonl` from its directory. A prompt of
/// "hang" replays nothing. `exit_code` in the directory sets the exit status.
fn fake_claude(dir: &Path, out: &Mutex<File>) -> i32 {
    std::fs::write(dir.join("pid"), std::process::id().to_string()).unwrap();
    let env: Vec<String> = ["CLAUDE_CODE_ENTRYPOINT", "CLAUDE_AGENT_SDK_VERSION", "NODE_OPTIONS", "DEBUG"]
        .iter()
        .map(|name| format!("{name}={}", std::env::var(name).unwrap_or_else(|_| "<unset>".into())))
        .collect();
    std::fs::write(dir.join("env"), env.join("\n")).unwrap();
    std::fs::write(dir.join("cwd"), std::env::current_dir().unwrap().display().to_string()).unwrap();
    let mut replayed = false;
    for message in stdin_lines() {
        append(&dir.join("stdin.jsonl"), &message.to_string());
        if message["type"] == "control_request" && message["request"]["subtype"] == "initialize" {
            let mode = std::fs::read_to_string(dir.join("initialize_mode")).unwrap_or_default();
            if mode == "eof" {
                return 0;
            }
            if mode == "error" || mode == "wrong-id" {
                write(
                    out,
                    &json!({ "type": "control_response", "response": {
                    "subtype": if mode == "error" { "error" } else { "success" },
                    "request_id": if mode == "error" { message["request_id"].clone() } else { json!("unrelated") },
                    "error": "initialization rejected",
                } }),
                );
                continue;
            }
            write(
                out,
                &json!({ "type": "control_response", "response": {
                    "subtype": "success", "request_id": message["request_id"], "response": { "commands": [], "models": [] },
                } }),
            );
        }
        if message["type"] == "user" && !replayed {
            replayed = true;
            if message["message"]["content"][0]["text"] == "hang" {
                continue;
            }
            let replay = std::fs::read_to_string(dir.join("replay.jsonl")).unwrap_or_default();
            for line in replay.lines().filter(|line| !line.trim().is_empty()) {
                let mut file = out.lock().unwrap();
                let _ = writeln!(file, "{line}");
                let _ = file.flush();
            }
        }
    }
    std::fs::read_to_string(dir.join("exit_code"))
        .ok()
        .and_then(|code| code.trim().parse().ok())
        .unwrap_or(0)
}

/// A stand-in for `opencode2 serve --stdio`: it announces a loopback URL,
/// then serves the OpenCode 2 session API until stdin closes. It records each
/// request, whether the Basic credentials matched OPENCODE_SERVER_PASSWORD,
/// and the config content it was given.
fn fake_opencode(dir: &Path, out: &Mutex<File>) -> i32 {
    std::fs::write(
        dir.join("config.json"),
        std::env::var("OPENCODE_CONFIG_CONTENT").unwrap_or_default(),
    )
    .unwrap();
    let password = std::env::var("OPENCODE_SERVER_PASSWORD").unwrap_or_default();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    write(out, &json!({ "url": url }));
    let dir = dir.to_path_buf();
    std::thread::spawn(move || {
        let prompts = Arc::new(Mutex::new(0));
        let model = Arc::new(Mutex::new(json!({ "providerID": "stored", "id": "original", "variant": "low" })));
        for stream in listener.incoming().map_while(Result::ok) {
            let dir = dir.clone();
            let password = password.clone();
            let prompts = prompts.clone();
            let model = model.clone();
            std::thread::spawn(move || serve_opencode(stream, &dir, &password, &prompts, &model));
        }
    });
    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink);
    0
}

fn serve_opencode(stream: TcpStream, dir: &Path, password: &str, prompts: &Mutex<u32>, model: &Mutex<Value>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    reader.read_line(&mut request_line).unwrap();
    let mut length = 0;
    let mut authorization = String::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':').unwrap();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().unwrap(),
            "authorization" => authorization = value.trim().to_string(),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let expected = format!("Basic {}", crate::opencode::base64(format!("opencode:{password}").as_bytes()));
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    append(
        &dir.join("requests.jsonl"),
        &json!({ "method": method, "path": path, "auth": authorization == expected, "body": body }).to_string(),
    );
    let (status, response) = match (method.as_str(), path.as_str()) {
        ("POST", "/api/session") => {
            if let Some(selected) = body.get("model") {
                *model.lock().unwrap() = selected.clone();
            }
            (200, json!({ "data": { "id": "ses_fake", "model": *model.lock().unwrap() } }))
        }
        ("GET", "/api/session/ses_fake") => (200, json!({ "data": { "id": "ses_fake", "model": *model.lock().unwrap() } })),
        ("POST", "/api/session/ses_fake/agent") => (204, Value::Null),
        ("POST", "/api/session/ses_fake/model") => {
            if dir.join("reject_model").exists() {
                (400, json!({ "error": "model switch rejected" }))
            } else {
                *model.lock().unwrap() = body["model"].clone();
                (204, Value::Null)
            }
        }
        ("POST", "/api/session/ses_fake/prompt") => {
            let mut count = prompts.lock().unwrap();
            *count += 1;
            (200, json!({ "data": { "id": format!("msg_{count}") } }))
        }
        ("POST", "/api/experimental/session/ses_fake/wait") => {
            std::thread::sleep(Duration::from_millis(50));
            (204, Value::Null)
        }
        ("GET", "/api/experimental/session/ses_fake/export") => (
            200,
            json!({ "data": {
                "info": { "outcome": "succeeded", "tokens": { "input": 7, "output": 3, "reasoning": 1, "cache": { "read": 2 } }, "cost": 0.5 },
                "messages": [
                    { "id": "msg_1", "type": "user", "content": [{ "type": "text", "text": "hello" }] },
                    { "id": "msg_a", "type": "assistant", "content": [
                        { "type": "tool", "id": "call_1", "name": "bash", "state": { "status": "completed", "input": { "command": "ls" }, "output": "README.md" } },
                        { "type": "text", "text": "FAKE_OK" },
                    ] },
                ],
            } }),
        ),
        ("DELETE", "/api/session/ses_fake") => (204, Value::Null),
        _ => (404, json!({ "error": "not found" })),
    };
    let body = if status == 204 { String::new() } else { response.to_string() };
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        _ => "Not Found",
    };
    let mut stream = stream;
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}
