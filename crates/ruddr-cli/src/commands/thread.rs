//! `ruddr thread list|search|read|turns|fork|name|archive|unarchive`. Each
//! action starts a short-lived app-server, initializes it, makes one call,
//! and prints the raw result as indented JSON. Callers depend on complete
//! metadata and pagination cursors, so the result's bytes are re-indented,
//! never re-encoded. Port of thread_commands.go.

use super::args;
use ruddr_core::{Error, Result};
use serde_json::{Map, Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// The whole command, from spawn to reply.
const SESSION_TIMEOUT: Duration = Duration::from_secs(90);
/// One write to the child's stdin.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the child gets to exit after stdin closes, and after each signal.
const EXIT_GRACE: Duration = Duration::from_secs(3);
/// The longest JSON-RPC line accepted from the child.
const MAX_LINE_BYTES: u64 = 64 * 1024 * 1024;

const ACTIONS: &str = "list, search, read, turns, fork, name, archive, or unarchive";
const PROVIDERS: [&str; 5] = ["codex", "claude", "opencode", "pi", "droid"];

fn specs() -> Vec<args::Spec> {
    vec![
        args::value(
            "provider",
            "NAME",
            "provider whose app-server answers: codex (default), claude, opencode, pi, or droid",
        ),
        args::value(
            "cwd",
            "DIR",
            "working directory for the app-server (default: the current directory)",
        ),
        args::value("cwd-filter", "DIR", "list: only threads whose cwd is exactly DIR"),
        args::value("cursor", "CURSOR", "opaque pagination cursor"),
        args::value("limit", "N", "page size; zero uses the server default"),
        args::flag("archived", "list and search: operate on archived threads"),
        args::flag("include-turns", "read: include the thread's turns"),
        args::value("before-turn", "TURN_ID", "fork: exclude this turn and everything after it"),
        args::value("through-turn", "TURN_ID", "fork: include history through this turn"),
    ]
}

pub fn thread_command(argv: Vec<String>) -> Result<()> {
    let Some((action, rest)) = argv.split_first() else {
        return Err(Error::failed(format!("thread action is required: {ACTIONS}")));
    };
    if matches!(action.as_str(), "-h" | "--help" | "help") {
        eprint!(
            "{}",
            args::help_text("thread ACTION [options] [-- APP_SERVER_COMMAND...]", &specs())
        );
        return Ok(());
    }
    let (flag_args, child) = match rest.iter().position(|a| a == "--") {
        Some(marker) => {
            let child = rest[marker + 1..].to_vec();
            if child.is_empty() {
                return Err(Error::failed("app-server command after -- is empty"));
            }
            (&rest[..marker], Some(child))
        }
        None => (rest, None),
    };
    let parsed = args::parse(&format!("thread {action}"), &specs(), flag_args)?;
    let (method, params) = build_request(action, &parsed)?;
    let command = child_command(&parsed, child)?;
    let cwd = parsed
        .string("cwd")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let raw = invoke(&ruddr_core::paths::absolute(&cwd), &command, &method, Value::Object(params))?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match raw {
        None => writeln!(out, "null")?,
        Some(raw) => writeln!(out, "{}", indent_json(&raw))?,
    }
    Ok(())
}

/// The default app-server: `codex app-server` for Codex, and this binary's
/// own adapter for the other providers, as `ruddr run` starts them.
fn child_command(parsed: &args::Parsed, child: Option<Vec<String>>) -> Result<Vec<String>> {
    let provider = parsed.string_or("provider", "codex");
    if !PROVIDERS.contains(&provider.as_str()) {
        return Err(Error::failed(format!(
            "unsupported provider {provider:?}; expected codex, claude, opencode, pi, or droid"
        )));
    }
    if let Some(child) = child {
        if provider != "codex" {
            return Err(Error::failed("a command after -- is supported only for Codex"));
        }
        return Ok(child);
    }
    if provider == "codex" {
        return Ok(["codex", "app-server", "--listen", "stdio://"].map(String::from).to_vec());
    }
    let exe = std::env::current_exe().map_err(|e| Error::failed(format!("locate the Ruddr executable: {e}")))?;
    Ok(vec![exe.display().to_string(), "app-server".into(), "--provider".into(), provider])
}

fn require_thread_id(action: &str, positionals: &[String]) -> Result<String> {
    if positionals.len() != 1 || positionals[0].trim().is_empty() {
        return Err(Error::failed(format!("thread {action} requires exactly one THREAD_ID")));
    }
    Ok(positionals[0].clone())
}

/// The JSON-RPC method and params for one action.
pub fn build_request(action: &str, parsed: &args::Parsed) -> Result<(String, Map<String, Value>)> {
    let positionals = &parsed.positionals;
    let mut params = Map::new();
    if let Some(cursor) = parsed.string("cursor").filter(|c| !c.is_empty()) {
        params.insert("cursor".into(), json!(cursor));
    }
    let limit = parsed.int("limit", 0)?;
    if limit > 0 {
        params.insert("limit".into(), json!(limit));
    }
    let archived = parsed.bool("archived");
    let method = match action {
        "list" => {
            if let Some(filter) = parsed.string("cwd-filter").filter(|c| !c.is_empty()) {
                params.insert("cwd".into(), json!(filter));
            }
            if archived {
                params.insert("archived".into(), json!(true));
            }
            "thread/list"
        }
        "search" => {
            if positionals.is_empty() {
                return Err(Error::failed("thread search requires a search term"));
            }
            params.insert("searchTerm".into(), json!(positionals.join(" ")));
            if archived {
                params.insert("archived".into(), json!(true));
            }
            "thread/search"
        }
        "read" => {
            let id = require_thread_id(action, positionals)?;
            params = Map::new();
            params.insert("threadId".into(), json!(id));
            params.insert("includeTurns".into(), json!(parsed.bool("include-turns")));
            "thread/read"
        }
        "turns" => {
            params.insert("threadId".into(), json!(require_thread_id(action, positionals)?));
            "thread/turns/list"
        }
        "fork" => {
            let id = require_thread_id(action, positionals)?;
            let before = parsed.string("before-turn").filter(|t| !t.is_empty());
            let through = parsed.string("through-turn").filter(|t| !t.is_empty());
            if before.is_some() && through.is_some() {
                return Err(Error::failed("--before-turn and --through-turn are mutually exclusive"));
            }
            params = Map::new();
            params.insert("threadId".into(), json!(id));
            params.insert("excludeTurns".into(), json!(true));
            if let Some(before) = before {
                params.insert("beforeTurnId".into(), json!(before));
            }
            if let Some(through) = through {
                params.insert("lastTurnId".into(), json!(through));
            }
            "thread/fork"
        }
        "name" => {
            if positionals.len() < 2 {
                return Err(Error::failed("thread name requires THREAD_ID and NAME"));
            }
            params = Map::new();
            params.insert("threadId".into(), json!(positionals[0]));
            params.insert("name".into(), json!(positionals[1..].join(" ")));
            "thread/name/set"
        }
        "archive" | "unarchive" => {
            let id = require_thread_id(action, positionals)?;
            params = Map::new();
            params.insert("threadId".into(), json!(id));
            if action == "archive" {
                "thread/archive"
            } else {
                "thread/unarchive"
            }
        }
        other => return Err(Error::failed(format!("unknown thread action {other:?}"))),
    };
    Ok((method.to_string(), params))
}

/// Starts the app-server, initializes it, makes one call, and returns the
/// raw `result` text (`None` for a void result).
pub fn invoke(cwd: &std::path::Path, command: &[String], method: &str, params: Value) -> Result<Option<String>> {
    let mut session = Session::start(cwd, command)?;
    let result = (|| {
        session
            .call(
                "initialize",
                json!({
                    "clientInfo": {"name": "ruddr", "title": "Ruddr", "version": ruddr_core::VERSION},
                    "capabilities": {"experimentalApi": true},
                }),
            )
            .map_err(|e| e.context("initialize app-server"))?;
        session.write(&json!({"method": "initialized", "params": {}}))?;
        session.call(method, params)
    })();
    session.close();
    result
}

type Ack = Sender<std::io::Result<()>>;

struct Session {
    child: Child,
    lines: Receiver<std::io::Result<String>>,
    writer: Option<Sender<(Vec<u8>, Ack)>>,
    next_id: u64,
    deadline: Instant,
}

impl Session {
    fn start(cwd: &std::path::Path, command: &[String]) -> Result<Session> {
        let mut process = Command::new(&command[0]);
        process
            .args(&command[1..])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut process, 0);
        let mut child = process.spawn().map_err(|e| Error::failed(format!("start {}: {e}", command[0])))?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");

        let (line_tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                match reader.by_ref().take(MAX_LINE_BYTES + 1).read_until(b'\n', &mut line) {
                    Ok(0) => return,
                    Ok(_) if line.len() as u64 > MAX_LINE_BYTES => {
                        let _ = line_tx.send(Err(std::io::Error::other("app-server line is too long")));
                        return;
                    }
                    Ok(_) => {
                        if line_tx.send(Ok(String::from_utf8_lossy(&line).into_owned())).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = line_tx.send(Err(e));
                        return;
                    }
                }
            }
        });

        let (writer, jobs) = mpsc::channel::<(Vec<u8>, Ack)>();
        std::thread::spawn(move || {
            let mut stdin = stdin;
            for (bytes, ack) in jobs {
                let _ = ack.send(stdin.write_all(&bytes).and_then(|()| stdin.flush()));
            }
            // Dropping stdin here closes the child's input.
        });

        Ok(Session {
            child,
            lines,
            writer: Some(writer),
            next_id: 0,
            deadline: Instant::now() + SESSION_TIMEOUT,
        })
    }

    /// Writes one line, bounded so a child that stops reading cannot hang us.
    fn write(&self, message: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(message)?;
        bytes.push(b'\n');
        let (ack, done) = mpsc::channel();
        let writer = self.writer.as_ref().ok_or_else(|| Error::failed("app-server input is closed"))?;
        writer.send((bytes, ack)).map_err(|_| Error::failed("app-server input is closed"))?;
        match done.recv_timeout(WRITE_TIMEOUT) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(Error::failed(format!("write to app-server: {e}"))),
            Err(_) => Err(Error::failed(format!(
                "write to app-server did not finish within {}",
                ruddr_core::duration::format(WRITE_TIMEOUT)
            ))),
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Option<String>> {
        self.next_id += 1;
        let id = format!("ruddr-query-{}", self.next_id);
        self.write(&json!({"id": id, "method": method, "params": params}))?;
        loop {
            let wait = self.deadline.saturating_duration_since(Instant::now());
            let line = match self.lines.recv_timeout(wait) {
                Ok(Ok(line)) => line,
                Ok(Err(e)) => return Err(Error::failed(format!("read app-server: {e}"))),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(Error::failed(format!(
                        "app-server did not answer {method} within {}",
                        ruddr_core::duration::format(SESSION_TIMEOUT)
                    )));
                }
                Err(RecvTimeoutError::Disconnected) => return Err(Error::failed("app-server closed before responding")),
            };
            let text = line.trim();
            if text.is_empty() {
                continue;
            }
            let message: Value = serde_json::from_str(text).map_err(|e| Error::failed(format!("parse app-server response: {e}")))?;
            let message_id = message.get("id").filter(|v| !v.is_null());
            if message.get("method").is_some_and(|m| m.is_string()) {
                // Server-initiated requests get an explicit refusal so they
                // never hang; notifications are ignored.
                if let Some(request_id) = message_id {
                    self.write(&json!({
                        "id": request_id,
                        "error": {"code": -32601, "message": "Ruddr thread command cannot answer interactive requests"},
                    }))?;
                }
                continue;
            }
            let matches = match message_id {
                Some(Value::String(s)) => *s == id,
                Some(Value::Number(n)) => n.to_string() == id,
                _ => false,
            };
            if !matches {
                continue;
            }
            if let Some(error) = message.get("error").filter(|e| !e.is_null()) {
                let text = error.get("message").and_then(Value::as_str).unwrap_or("");
                let code = error.get("code").map(Value::to_string).unwrap_or_else(|| "0".into());
                return Err(Error::failed(format!("{text} ({code})")));
            }
            // A void result arrives as an omitted `result`; that is success.
            return Ok(top_level_member(text, "result").map(str::to_string));
        }
    }

    /// Closes stdin, gives the child time to exit, then ends its process tree.
    fn close(&mut self) {
        self.writer = None;
        if wait_for_exit(&mut self.child, EXIT_GRACE) {
            return;
        }
        terminate(&mut self.child, false);
        if wait_for_exit(&mut self.child, EXIT_GRACE) {
            return;
        }
        terminate(&mut self.child, true);
        wait_for_exit(&mut self.child, EXIT_GRACE);
    }
}

fn wait_for_exit(child: &mut Child, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return true,
            Ok(None) if Instant::now() >= deadline => return false,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// Signals the child's process group (Unix) or kills the child (Windows).
fn terminate(child: &mut Child, force: bool) {
    #[cfg(unix)]
    {
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        if let Ok(pid) = i32::try_from(child.id()) {
            // SAFETY: signals the process group this command created for the child.
            unsafe { libc::kill(-pid, signal) };
        }
    }
    #[cfg(not(unix))]
    {
        let _ = force;
        let _ = child.kill();
    }
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// `b[i]` is a quote; returns the index after the closing quote.
fn skip_string(b: &[u8], mut i: usize) -> Option<usize> {
    i += 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

fn skip_value(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => skip_string(b, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = skip_string(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            None
        }
        _ => {
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            Some(j)
        }
    }
}

/// The raw text of a top-level member of a JSON object, exactly as sent.
pub fn top_level_member<'a>(object: &'a str, key: &str) -> Option<&'a str> {
    let b = object.as_bytes();
    let mut i = skip_ws(b, 0);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    loop {
        i = skip_ws(b, i);
        if b.get(i) != Some(&b'"') {
            return None;
        }
        let key_end = skip_string(b, i)?;
        let name = &object[i + 1..key_end - 1];
        i = skip_ws(b, key_end);
        if b.get(i) != Some(&b':') {
            return None;
        }
        i = skip_ws(b, i + 1);
        let value_end = skip_value(b, i)?;
        if name == key {
            return Some(&object[i..value_end]);
        }
        i = skip_ws(b, value_end);
        match b.get(i) {
            Some(b',') => i += 1,
            _ => return None,
        }
    }
}

/// Re-indents JSON text with two spaces, like Go's `json.Indent`: member
/// order, number spelling, and string escapes stay exactly as received.
pub fn indent_json(raw: &str) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(raw.len() * 2);
    let mut depth = 0usize;
    let mut pending_open = false;
    let mut in_string = false;
    let mut escaped = false;
    let newline = |out: &mut Vec<u8>, depth: usize| {
        out.push(b'\n');
        out.extend(std::iter::repeat_n(b' ', depth * 2));
    };
    for &c in raw.trim().as_bytes() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => {}
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if !pending_open {
                    newline(&mut out, depth);
                }
                pending_open = false;
                out.push(c);
            }
            _ => {
                if pending_open {
                    newline(&mut out, depth);
                    pending_open = false;
                }
                match c {
                    b'{' | b'[' => {
                        out.push(c);
                        depth += 1;
                        pending_open = true;
                    }
                    b',' => {
                        out.push(c);
                        newline(&mut out, depth);
                    }
                    b':' => out.extend_from_slice(b": "),
                    b'"' => {
                        out.push(c);
                        in_string = true;
                    }
                    _ => out.push(c),
                }
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(list: &[&str]) -> args::Parsed {
        let argv: Vec<String> = list.iter().map(|s| s.to_string()).collect();
        args::parse("thread", &specs(), &argv).unwrap()
    }

    #[test]
    fn builds_each_action_like_go() {
        let (method, params) = build_request("list", &parse(&["--limit", "20", "--cwd-filter", "/w", "--archived"])).unwrap();
        assert_eq!(method, "thread/list");
        assert_eq!(Value::Object(params), json!({"limit": 20, "cwd": "/w", "archived": true}));

        let (method, params) = build_request("search", &parse(&["parser", "regression", "--limit", "10"])).unwrap();
        assert_eq!(method, "thread/search");
        assert_eq!(Value::Object(params), json!({"limit": 10, "searchTerm": "parser regression"}));

        let (method, params) = build_request("read", &parse(&["--include-turns", "--cursor", "c", "T"])).unwrap();
        assert_eq!(method, "thread/read");
        assert_eq!(Value::Object(params), json!({"threadId": "T", "includeTurns": true}));

        let (method, params) = build_request("turns", &parse(&["T", "--cursor", "c"])).unwrap();
        assert_eq!(method, "thread/turns/list");
        assert_eq!(Value::Object(params), json!({"cursor": "c", "threadId": "T"}));

        let (method, params) = build_request("fork", &parse(&["--before-turn", "B", "T"])).unwrap();
        assert_eq!(method, "thread/fork");
        assert_eq!(
            Value::Object(params),
            json!({"threadId": "T", "excludeTurns": true, "beforeTurnId": "B"})
        );
        let (_, params) = build_request("fork", &parse(&["--through-turn", "L", "T"])).unwrap();
        assert_eq!(
            Value::Object(params),
            json!({"threadId": "T", "excludeTurns": true, "lastTurnId": "L"})
        );
        assert!(build_request("fork", &parse(&["--before-turn", "B", "--through-turn", "L", "T"])).is_err());

        let (method, params) = build_request("name", &parse(&["T", "Parser", "work"])).unwrap();
        assert_eq!(method, "thread/name/set");
        assert_eq!(Value::Object(params), json!({"threadId": "T", "name": "Parser work"}));

        assert_eq!(build_request("archive", &parse(&["T"])).unwrap().0, "thread/archive");
        assert_eq!(build_request("unarchive", &parse(&["T"])).unwrap().0, "thread/unarchive");
        assert!(build_request("archive", &parse(&[])).is_err());
        assert!(build_request("search", &parse(&[])).is_err());
        assert!(build_request("bogus", &parse(&[])).is_err());
    }

    #[test]
    fn adapter_providers_run_this_binary() {
        let command = child_command(&parse(&["--provider", "droid"]), None).unwrap();
        assert_eq!(command[1..], ["app-server", "--provider", "droid"].map(String::from));
        let codex = child_command(&parse(&[]), None).unwrap();
        assert_eq!(codex, ["codex", "app-server", "--listen", "stdio://"].map(String::from));
        assert!(child_command(&parse(&["--provider", "droid"]), Some(vec!["x".into()])).is_err());
        assert!(child_command(&parse(&["--provider", "nope"]), None).is_err());
    }

    #[test]
    fn extracts_raw_members() {
        let line = r#"{"id":"ruddr-query-2", "result" : {"b":1.50,"a":"x\"}"},"extra":[1]}"#;
        assert_eq!(top_level_member(line, "result"), Some(r#"{"b":1.50,"a":"x\"}"}"#));
        assert_eq!(top_level_member(line, "id"), Some(r#""ruddr-query-2""#));
        assert_eq!(top_level_member(r#"{"id":"x"}"#, "result"), None);
        assert_eq!(top_level_member(r#"{"id":"x","result":null}"#, "result"), Some("null"));
    }

    #[test]
    fn indents_like_go_and_keeps_order() {
        let raw = r#"{"z":1.50,"a":[],"m":{},"s":"a,b:{c}","list":[{"k":true},2]}"#;
        let want = "{\n  \"z\": 1.50,\n  \"a\": [],\n  \"m\": {},\n  \"s\": \"a,b:{c}\",\n  \"list\": [\n    {\n      \"k\": true\n    },\n    2\n  ]\n}";
        assert_eq!(indent_json(raw), want);
        assert_eq!(indent_json("null"), "null");
    }
}
