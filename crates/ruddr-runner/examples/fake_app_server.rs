//! A fake `codex app-server` for the runner's lifecycle tests. It speaks
//! line-delimited JSON-RPC on stdio and follows the flags it is started with,
//! so each test picks its scenario without touching shared environment
//! variables. `--request-log FILE` records every line the controller sends,
//! so tests can assert the exact JSON-RPC requests.
//!
//! `--detach-helper STATUS --detach-dir DIR` instead stands in for a detached
//! controller: it records its arguments, then writes a state with STATUS, or
//! with `crash` prints a diagnostic and exits 2.
//!
//! This is a test fixture. Examples build with `cargo test` and never ship.

use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::time::Duration;

#[derive(Default)]
struct Options {
    flags: HashSet<String>,
    values: std::collections::HashMap<String, String>,
}

impl Options {
    fn parse() -> Options {
        let mut options = Options::default();
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let Some(name) = arg.strip_prefix("--") else { continue };
            if VALUE_FLAGS.contains(&name) {
                options.values.insert(name.into(), args.next().unwrap_or_default());
            } else {
                options.flags.insert(name.into());
            }
        }
        options
    }
    fn on(&self, name: &str) -> bool {
        self.flags.contains(name)
    }
    fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }
    fn millis(&self, name: &str) -> Option<Duration> {
        self.value(name).and_then(|v| v.parse().ok()).map(Duration::from_millis)
    }
}

const VALUE_FLAGS: &[&str] = &[
    "request-log",
    "resume-model",
    "resume-effort",
    "grandchild-pid-file",
    "expect-fork-before",
    "expect-fork-through",
    "turn-response-delay-ms",
    "interrupt-completion-delay-ms",
    "eof-marker",
    "detach-helper",
    "detach-dir",
];

struct Out(std::io::Stdout);

impl Out {
    fn send(&mut self, value: Value) {
        let mut lock = self.0.lock();
        let _ = writeln!(lock, "{value}");
        let _ = lock.flush();
    }
    fn result(&mut self, id: &Value, result: Value) {
        self.send(json!({"id": id, "result": result}));
    }
    fn error(&mut self, id: &Value, code: i64, message: &str) {
        self.send(json!({"id": id, "error": {"code": code, "message": message}}));
    }
    fn note(&mut self, method: &str, params: Value) {
        self.send(json!({"method": method, "params": params}));
    }
}

const APPROVAL_ID: u64 = 9_007_199_254_740_993;

fn main() {
    let options = Options::parse();
    if let Some(status) = options.value("detach-helper") {
        detach_helper(status, options.value("detach-dir").unwrap_or("."));
    }
    if let Some(pid_file) = options.value("grandchild-pid-file") {
        spawn_grandchild(pid_file, options.on("term-ignoring-grandchild"));
    }
    let mut log = options
        .value("request-log")
        .map(|path| std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap());
    let mut out = Out(std::io::stdout());
    let mut turn_counter = 0;
    let mut current_turn = "turn-test".to_string();
    let mut rejected_second = false;
    let mut deferred_interrupt: Option<Value> = None;
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if let Some(log) = log.as_mut() {
            let _ = writeln!(log, "{line}");
            let _ = log.flush();
        }
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(Value::as_str).unwrap_or_default();
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let param = |key: &str| params.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
        if method.is_empty() {
            // The controller's answer to our interactive request.
            if id == json!(APPROVAL_ID) && request.get("error").is_some() {
                out.note(
                    "item/completed",
                    json!({"item": {"id": "message-test", "type": "agentMessage", "text": "DONE"}}),
                );
                out.note("turn/completed", json!({"turn": {"id": current_turn, "status": "completed"}}));
            }
            continue;
        }
        match method {
            "initialize" => {
                if options.on("ignore-initialize") {
                    continue;
                }
                out.result(
                    &id,
                    json!({"userAgent": "fake", "codexHome": "/tmp", "platformFamily": "unix", "platformOs": "test"}),
                );
            }
            "initialized" => {}
            "thread/start" => {
                if options.on("expect-ephemeral") && params.get("ephemeral") != Some(&json!(true)) {
                    out.error(&id, -32602, "ephemeral option missing");
                    continue;
                }
                out.result(&id, json!({"thread": {"id": "thread-test"}}));
            }
            "thread/resume" => {
                if param("threadId") != "source-thread" {
                    out.error(&id, -32602, "wrong source thread");
                    continue;
                }
                out.result(
                    &id,
                    json!({
                        "thread": {"id": "thread-resumed"},
                        "model": options.value("resume-model"),
                        "reasoningEffort": options.value("resume-effort"),
                    }),
                );
            }
            "thread/fork" => {
                if param("threadId") != "source-thread" {
                    out.error(&id, -32602, "wrong source thread");
                    continue;
                }
                if param("beforeTurnId") == "turn-missing" || param("lastTurnId") == "turn-missing" {
                    out.error(&id, -32602, "unknown turn turn-missing");
                    continue;
                }
                if let Some(want) = options.value("expect-fork-before")
                    && (param("beforeTurnId") != want || params.get("lastTurnId").is_some())
                {
                    out.error(&id, -32602, "beforeTurnId mismatch");
                    continue;
                }
                if let Some(want) = options.value("expect-fork-through")
                    && (param("lastTurnId") != want || params.get("beforeTurnId").is_some())
                {
                    out.error(&id, -32602, "lastTurnId mismatch");
                    continue;
                }
                let thread = if options.on("fork-same-id") {
                    param("threadId")
                } else {
                    "thread-forked".to_owned()
                };
                out.result(&id, json!({"thread": {"id": thread}}));
            }
            "turn/start" => {
                if options.on("multi-turn") {
                    turn_counter += 1;
                    current_turn = format!("turn-{turn_counter}");
                    if turn_counter > 1 && !rejected_second && options.on("reject-second-turn") {
                        rejected_second = true;
                        out.error(&id, -32602, "second turn rejected");
                        turn_counter -= 1;
                        current_turn = format!("turn-{turn_counter}");
                        continue;
                    }
                    if turn_counter > 1 && options.on("ambiguous-second-turn") {
                        out.note(
                            "turn/started",
                            json!({"threadId": "thread-test", "turn": {"id": current_turn, "status": "inProgress"}}),
                        );
                        continue;
                    }
                    if options.on("delay-turn-start") {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    out.result(&id, json!({"turn": {"id": current_turn, "status": "inProgress"}}));
                    out.note(
                        "turn/started",
                        json!({"threadId": "thread-test", "turn": {"id": current_turn, "status": "inProgress"}}),
                    );
                    if let Some(old) = deferred_interrupt.take() {
                        out.error(&old, -32602, "old turn already completed");
                    }
                    if !options.on("multi-turn-hold") {
                        out.note(
                            "thread/tokenUsage/updated",
                            json!({
                                "threadId": "thread-test",
                                "tokenUsage": {
                                    "total": {"totalTokens": 100 * turn_counter, "inputTokens": 80 * turn_counter, "cachedInputTokens": 10 * turn_counter, "outputTokens": 20 * turn_counter},
                                    "last": {"totalTokens": 90},
                                    "modelContextWindow": 1000,
                                },
                            }),
                        );
                        out.note(
                            "item/completed",
                            json!({"item": {"id": format!("message-{turn_counter}"), "type": "agentMessage", "text": format!("TURN {turn_counter}")}}),
                        );
                        out.note("turn/completed", json!({"turn": {"id": current_turn, "status": "completed"}}));
                        if options.on("exit-after-turn") {
                            return;
                        }
                    }
                    continue;
                }
                if options.on("complete-before-turn-response") {
                    out.note(
                        "turn/started",
                        json!({"threadId": "thread-test", "turn": {"id": "turn-test", "status": "inProgress"}}),
                    );
                    out.note(
                        "item/completed",
                        json!({"item": {"id": "message-early", "type": "agentMessage", "text": "EARLY"}}),
                    );
                    out.note("turn/completed", json!({"turn": {"id": "turn-test", "status": "completed"}}));
                    out.result(&id, json!({"turn": {"id": "turn-test", "status": "completed"}}));
                    return;
                }
                if options.on("turn-response-no-id") {
                    out.result(&id, json!({"turn": {"status": "inProgress"}}));
                    return;
                }
                if options.on("early-item-before-turn-response") {
                    out.note(
                        "item/completed",
                        json!({"item": {"id": "message-live", "type": "agentMessage", "text": "LIVE BEFORE RESPONSE"}}),
                    );
                }
                if let Some(delay) = options.millis("turn-response-delay-ms") {
                    std::thread::sleep(delay);
                }
                out.result(&id, json!({"turn": {"id": "turn-test", "status": "inProgress"}}));
                out.note(
                    "turn/started",
                    json!({"threadId": "thread-test", "turn": {"id": "turn-test", "status": "inProgress"}}),
                );
                if options.on("nested-turn") {
                    out.note(
                        "turn/started",
                        json!({"threadId": "thread-child", "turn": {"id": "turn-child", "status": "inProgress"}}),
                    );
                    out.note(
                        "turn/completed",
                        json!({"threadId": "thread-child", "turn": {"id": "turn-child", "status": "completed"}}),
                    );
                    out.note(
                        "item/completed",
                        json!({"threadId": "thread-test", "turnId": "turn-test", "item": {"id": "message-root", "type": "agentMessage", "text": "ROOT DONE"}}),
                    );
                    out.note(
                        "turn/completed",
                        json!({"threadId": "thread-test", "turn": {"id": "turn-test", "status": "completed"}}),
                    );
                    continue;
                }
                if options.on("multi-output-error") {
                    out.note(
                        "item/completed",
                        json!({"item": {"id": "message-first", "type": "agentMessage", "text": "FIRST"}}),
                    );
                    out.note(
                        "item/completed",
                        json!({"item": {"id": "message-second", "type": "agentMessage", "text": "SECOND"}}),
                    );
                    out.note(
                        "turn/completed",
                        json!({"turn": {"id": "turn-test", "status": "failed", "error": {"code": 99, "message": "SECRET_ECHO from prompt"}}}),
                    );
                    continue;
                }
                if options.on("server-request") {
                    out.send(
                        json!({"id": APPROVAL_ID, "method": "item/commandExecution/requestApproval", "params": {"command": "rm -rf /"}}),
                    );
                    continue;
                }
                if options.on("complete-on-start") {
                    out.note(
                        "item/completed",
                        json!({"item": {"id": "message-test", "type": "agentMessage", "text": "DONE"}}),
                    );
                    out.note("turn/completed", json!({"turn": {"id": "turn-test", "status": "completed"}}));
                }
            }
            "turn/steer" => {
                let expected = if options.on("multi-turn") {
                    current_turn.clone()
                } else {
                    "turn-test".into()
                };
                if param("threadId") != "thread-test" || param("expectedTurnId") != expected {
                    out.error(&id, -32600, "wrong turn");
                    continue;
                }
                out.result(&id, json!({"turnId": expected}));
                out.note(
                    "item/completed",
                    json!({"item": {"id": "message-test", "type": "agentMessage", "text": "STEERED"}}),
                );
                out.note("turn/completed", json!({"turn": {"id": expected, "status": "completed"}}));
            }
            "turn/interrupt" => {
                if param("threadId") != "thread-test" || param("turnId") != current_turn {
                    out.error(&id, -32602, "interrupt targeted the wrong turn");
                    continue;
                }
                if options.on("defer-interrupt-error") {
                    deferred_interrupt = Some(id.clone());
                    out.note("turn/completed", json!({"turn": {"id": current_turn, "status": "completed"}}));
                    continue;
                }
                if options.on("interrupt-complete-first") {
                    out.note("turn/completed", json!({"turn": {"id": current_turn, "status": "interrupted"}}));
                }
                out.result(&id, json!({}));
                if options.on("exit-after-interrupt-ack") {
                    return;
                }
                if options.on("interrupt-ack-only") {
                    continue;
                }
                if !options.on("interrupt-complete-first") {
                    if let Some(delay) = options.millis("interrupt-completion-delay-ms") {
                        std::thread::sleep(delay);
                    }
                    out.note("turn/completed", json!({"turn": {"id": current_turn, "status": "interrupted"}}));
                }
            }
            _ if !id.is_null() => out.error(&id, -32601, &format!("unsupported {method}")),
            _ => {}
        }
    }
    if let Some(marker) = options.value("eof-marker") {
        let _ = std::fs::write(marker, "stdin closed\n");
    }
}

/// Starts a grandchild in the fake's process group and records its PID. The
/// TERM-ignoring variant proves that teardown escalates to SIGKILL.
fn spawn_grandchild(pid_file: &str, ignore_term: bool) {
    use std::process::{Command, Stdio};
    let mut command = if ignore_term {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            r#"trap '' TERM; echo $$ > "$1"; while :; do sleep 1; done"#,
            "ruddr-grandchild",
            pid_file,
        ]);
        command
    } else {
        let mut command = Command::new("sleep");
        command.arg("60");
        command
    };
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let Ok(child) = command.spawn() else { return };
    if !ignore_term {
        let _ = std::fs::write(pid_file, child.id().to_string());
        return;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if std::fs::read_to_string(pid_file).map(|s| !s.trim().is_empty()).unwrap_or(false) {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn detach_helper(status: &str, dir: &str) -> ! {
    let args: Vec<String> = std::env::args().collect();
    let _ = std::fs::write(std::path::Path::new(dir).join("helper.args"), args.join("\n"));
    if status == "crash" {
        eprintln!("provider binary not found");
        std::process::exit(2);
    }
    let state = json!({
        "version": 2, "provider": "codex", "pid": std::process::id(), "status": status, "stateDir": dir,
        "startedAt": "2026-10-02T00:00:00Z", "updatedAt": "2026-10-02T00:00:00Z",
    });
    let _ = std::fs::write(
        std::path::Path::new(dir).join("state.json"),
        serde_json::to_vec_pretty(&state).unwrap(),
    );
    std::process::exit(0);
}
