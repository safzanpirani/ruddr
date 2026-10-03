//! JSON-RPC plumbing shared by every adapter: the stdio loop, the emitter,
//! line framing, error codes, and parameter helpers. Port of
//! adapter/protocol.ts.
//!
//! Adapters write plain JSON objects (`serde_json::Value`) instead of
//! `ruddr_core::jsonrpc::Message`, because a parse error answers with
//! `"id": null`, which `Message` cannot represent.

use serde_json::{Map, Value, json};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The longest line the adapters accept, on stdin or from a provider CLI.
pub const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;

/// Receives every message an adapter emits, in emission order.
pub trait Sink: Send + Sync {
    fn emit(&self, message: Value);
}

pub type Emit = Arc<dyn Sink>;

/// Writes each message as one line and flushes it, like `codex app-server`.
pub struct WriterSink<W: Write + Send>(Mutex<W>);

impl<W: Write + Send> WriterSink<W> {
    pub fn new(writer: W) -> Self {
        WriterSink(Mutex::new(writer))
    }
}

impl<W: Write + Send> Sink for WriterSink<W> {
    fn emit(&self, message: Value) {
        let mut line = serde_json::to_vec(&message).unwrap_or_default();
        line.push(b'\n');
        let mut writer = lock(&self.0);
        // The runner owns the other end. When it is gone there is nobody left
        // to report a failed write to.
        let _ = writer.write_all(&line);
        let _ = writer.flush();
    }
}

/// Locks a mutex and keeps going if another thread panicked while holding it.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidParams,
    MethodNotFound,
    Failed,
}

/// An error a request handler returns. The kind picks the JSON-RPC code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterError {
    pub kind: ErrorKind,
    pub message: String,
}

pub type AResult<T> = Result<T, AdapterError>;

impl AdapterError {
    pub fn invalid(message: impl Into<String>) -> Self {
        AdapterError {
            kind: ErrorKind::InvalidParams,
            message: message.into(),
        }
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        AdapterError {
            kind: ErrorKind::MethodNotFound,
            message: message.into(),
        }
    }
    pub fn failed(message: impl Into<String>) -> Self {
        AdapterError {
            kind: ErrorKind::Failed,
            message: message.into(),
        }
    }
    pub fn code(&self) -> i64 {
        match self.kind {
            ErrorKind::MethodNotFound => -32601,
            ErrorKind::InvalidParams => -32602,
            ErrorKind::Failed => -32000,
        }
    }
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<String> for AdapterError {
    fn from(message: String) -> Self {
        AdapterError::failed(message)
    }
}

/// One provider adapter. `dispatch` runs on the stdin thread, one request at
/// a time, in arrival order.
pub trait Adapter: Send + Sync {
    fn dispatch(&self, method: &str, params: &Value) -> AResult<Value>;
    fn close(&self);
}

/// Answers one request. A request with an `id` (even `null`) gets a response;
/// a notification that fails becomes an `error` notification.
pub fn handle(adapter: &dyn Adapter, emit: &dyn Sink, request: &Map<String, Value>) {
    let method = request.get("method").and_then(Value::as_str).unwrap_or_default();
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    let id = request.get("id");
    match adapter.dispatch(method, &params) {
        Ok(result) => {
            if let Some(id) = id {
                emit.emit(json!({ "id": id, "result": result }));
            }
        }
        Err(error) => match id {
            Some(id) => emit.emit(json!({ "id": id, "error": { "code": error.code(), "message": error.message } })),
            None => emit.emit(json!({ "method": "error", "params": { "error": { "message": error.message } } })),
        },
    }
}

/// Reads requests from `input` until EOF, then closes the adapter. A line
/// over 64 MiB stops the loop with an error after the adapter closes.
pub fn serve<R: Read>(adapter: &dyn Adapter, input: R, emit: &dyn Sink) -> io::Result<()> {
    let mut lines = LineReader::new(input, MAX_LINE_BYTES);
    let outcome = loop {
        match lines.next_line() {
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&line) {
                    Ok(Value::Object(request)) if request.get("method").is_some_and(Value::is_string) => {
                        handle(adapter, emit, &request);
                    }
                    Ok(_) => emit.emit(parse_error("JSON-RPC message must contain a method")),
                    Err(error) => emit.emit(parse_error(&error.to_string())),
                }
            }
        }
    };
    adapter.close();
    outcome
}

fn parse_error(message: &str) -> Value {
    json!({ "id": null, "error": { "code": -32700, "message": message } })
}

/// Splits a byte stream into lines. Lines end at `\n`; a trailing `\r` is
/// dropped; a final line without a newline is still returned. Invalid UTF-8
/// becomes U+FFFD. A line longer than `max_bytes` is an error.
pub struct LineReader<R: Read> {
    inner: BufReader<R>,
    max_bytes: usize,
}

impl<R: Read> LineReader<R> {
    pub fn new(reader: R, max_bytes: usize) -> Self {
        LineReader {
            inner: BufReader::with_capacity(64 * 1024, reader),
            max_bytes,
        }
    }

    pub fn next_line(&mut self) -> io::Result<Option<String>> {
        let mut line = Vec::new();
        loop {
            let available = match self.inner.fill_buf() {
                Ok(available) => available,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            if available.is_empty() {
                return Ok(if line.is_empty() { None } else { Some(finish_line(line)) });
            }
            let (chunk, found) = match available.iter().position(|b| *b == b'\n') {
                Some(index) => (&available[..index], true),
                None => (available, false),
            };
            if line.len() + chunk.len() > self.max_bytes {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "JSON-RPC line exceeds 64 MiB"));
            }
            line.extend_from_slice(chunk);
            let consumed = chunk.len() + usize::from(found);
            self.inner.consume(consumed);
            if found {
                return Ok(Some(finish_line(line)));
            }
        }
    }
}

fn finish_line(mut line: Vec<u8>) -> String {
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    match String::from_utf8(line) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    }
}

// Parameter helpers. They return the same messages as the TypeScript ones.

pub fn record<'a>(value: &'a Value, label: &str) -> AResult<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| AdapterError::invalid(format!("{label} must be an object")))
}

pub fn required_string(value: Option<&Value>, label: &str) -> AResult<String> {
    match value.and_then(Value::as_str) {
        Some(text) if !text.is_empty() => Ok(text.to_string()),
        _ => Err(AdapterError::invalid(format!("{label} must be a non-empty string"))),
    }
}

pub fn optional_string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).filter(|text| !text.is_empty()).map(str::to_string)
}

/// Joins the text items of a turn input, as Codex's `input` array carries them.
pub fn read_text_input(value: Option<&Value>) -> AResult<String> {
    let items = value
        .and_then(Value::as_array)
        .ok_or_else(|| AdapterError::invalid("input must be an array"))?;
    let items: Vec<_> = items.iter().filter_map(Value::as_object).collect();
    let field = |kind: &str, key: &str| -> Vec<String> {
        items
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some(kind))
            .filter_map(|item| item.get(key).and_then(Value::as_str))
            .map(String::from)
            .collect()
    };
    let text = field("text", "text").join("\n");
    let text = text.trim();
    if text.is_empty() {
        return Err(AdapterError::invalid("input must contain text"));
    }
    // These providers get no image items, so the agent opens each file
    // with its own read tool.
    let images = field("localImage", "path");
    if images.is_empty() {
        return Ok(text.to_string());
    }
    let mut prompt = text.to_string();
    prompt.push_str("\n\nAttached images (open each one with your file-reading tool):");
    for path in images {
        prompt.push_str(&format!("\n- {path}"));
    }
    Ok(prompt)
}

/// A finite JSON number, or 0.
pub fn number(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).filter(|n| n.is_finite()).unwrap_or(0.0)
}

/// Whether the value is a finite JSON number.
pub fn finite(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|n| n.is_finite())
}

/// A number as JSON, written as an integer when it has no fraction. The Go
/// runner decodes token counts into int64 and rejects `150.0`.
pub fn num(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9.0e15 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

/// Extracts text from a provider's tool output: a string, an array of parts,
/// or an object with a `text`/`content`/`message` field (the TypeScript
/// `textContent` helpers; Droid's omits `message`).
pub fn text_content(value: Option<&Value>, with_message: bool) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| text_content(Some(item), with_message))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Object(map)) => {
            let next = [map.get("text"), map.get("content")]
                .into_iter()
                .chain(with_message.then(|| map.get("message")))
                .flatten()
                .find(|value| !value.is_null());
            text_content(next, with_message)
        }
        _ => String::new(),
    }
}

/// `JSON.stringify` of a map: `{}` when empty.
pub fn compact(value: &Map<String, Value>) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".into())
}

/// A random RFC 4122 version 4 UUID.
pub fn uuid_v4() -> String {
    let mut bytes = random_bytes();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format_uuid(&bytes)
}

/// A time-ordered version 7 UUID, as `Bun.randomUUIDv7` makes them.
pub fn uuid_v7() -> String {
    let mut bytes = random_bytes();
    let millis = ruddr_core::time::now_ms().max(0) as u64;
    for (index, byte) in bytes.iter_mut().take(6).enumerate() {
        *byte = (millis >> (8 * (5 - index))) as u8;
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format_uuid(&bytes)
}

fn random_bytes() -> [u8; 16] {
    let hex = ruddr_core::fsutil::random_hex(16);
    let mut bytes = [0u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap_or(0);
    }
    bytes
}

fn format_uuid(bytes: &[u8; 16]) -> String {
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// A flag that one thread sets and others wait on with a deadline.
#[derive(Default)]
pub struct Latch {
    set: Mutex<bool>,
    changed: Condvar,
}

impl Latch {
    pub fn new() -> Arc<Latch> {
        Arc::new(Latch::default())
    }
    pub fn set(&self) {
        *lock(&self.set) = true;
        self.changed.notify_all();
    }
    pub fn is_set(&self) -> bool {
        *lock(&self.set)
    }
    /// Waits until the latch is set or the timeout passes; true when set.
    pub fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut set = lock(&self.set);
        while !*set {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            set = self.changed.wait_timeout(set, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
        true
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Collects emitted messages for assertions and lets tests wait for one.
    #[derive(Default)]
    pub struct Collector {
        messages: Mutex<Vec<Value>>,
        changed: Condvar,
    }

    impl Sink for Collector {
        fn emit(&self, message: Value) {
            lock(&self.messages).push(message);
            self.changed.notify_all();
        }
    }

    impl Collector {
        pub fn new() -> Arc<Collector> {
            Arc::new(Collector::default())
        }
        pub fn all(&self) -> Vec<Value> {
            lock(&self.messages).clone()
        }
        /// Waits up to 15 seconds for the predicate to hold over the messages.
        pub fn wait_for(&self, predicate: impl Fn(&[Value]) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut messages = lock(&self.messages);
            while !predicate(&messages) {
                let now = Instant::now();
                assert!(
                    now < deadline,
                    "timed out waiting for adapter event: {}",
                    Value::Array(messages.clone())
                );
                messages = self.changed.wait_timeout(messages, deadline - now).unwrap().0;
            }
        }
        pub fn result(&self, id: Value) -> Value {
            self.all()
                .into_iter()
                .find(|message| message.get("id") == Some(&id) && message.get("result").is_some())
                .and_then(|message| message.get("result").cloned())
                .unwrap_or_else(|| panic!("no result for {id}: {}", Value::Array(self.all())))
        }
        pub fn response(&self, id: Value) -> Option<Value> {
            self.all().into_iter().find(|message| message.get("id") == Some(&id))
        }
        pub fn notifications(&self, method: &str) -> Vec<Value> {
            notifications(&self.all(), method)
        }
        pub fn notification(&self, method: &str) -> Option<Value> {
            self.notifications(method).into_iter().next()
        }
        pub fn completed_items(&self) -> Vec<Value> {
            self.notifications("item/completed")
                .into_iter()
                .map(|message| message["params"]["item"].clone())
                .collect()
        }
        pub fn wait_for_method(&self, method: &str) {
            self.wait_for(|messages| !notifications(messages, method).is_empty());
        }
        pub fn text(&self) -> String {
            Value::Array(self.all()).to_string()
        }
    }

    pub fn notifications(messages: &[Value], method: &str) -> Vec<Value> {
        messages
            .iter()
            .filter(|message| message.get("method").and_then(Value::as_str) == Some(method))
            .cloned()
            .collect()
    }

    fn collect(chunks: Vec<Vec<u8>>, max: usize) -> io::Result<Vec<String>> {
        struct Chunks(std::collections::VecDeque<Vec<u8>>);
        impl Read for Chunks {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let Some(mut chunk) = self.0.pop_front() else { return Ok(0) };
                let n = chunk.len().min(buf.len());
                buf[..n].copy_from_slice(&chunk[..n]);
                if n < chunk.len() {
                    self.0.push_front(chunk.split_off(n));
                }
                Ok(n)
            }
        }
        let mut reader = LineReader::new(Chunks(chunks.into()), max);
        let mut lines = Vec::new();
        while let Some(line) = reader.next_line()? {
            lines.push(line);
        }
        Ok(lines)
    }

    fn strings(chunks: &[&str]) -> Vec<Vec<u8>> {
        chunks.iter().map(|chunk| chunk.as_bytes().to_vec()).collect()
    }

    #[test]
    fn splits_lines_across_chunk_boundaries_and_trims_cr() {
        assert_eq!(
            collect(strings(&["one\ntw", "o\r\nthree"]), MAX_LINE_BYTES).unwrap(),
            ["one", "two", "three"]
        );
    }

    #[test]
    fn keeps_empty_lines_and_drops_nothing_at_the_end() {
        assert_eq!(collect(strings(&["a\n\nb\n"]), MAX_LINE_BYTES).unwrap(), ["a", "", "b"]);
        assert!(collect(vec![], MAX_LINE_BYTES).unwrap().is_empty());
        assert_eq!(collect(strings(&["\n"]), MAX_LINE_BYTES).unwrap(), [""]);
    }

    #[test]
    fn rejoins_multi_byte_characters_split_across_chunks() {
        let encoded = "é日\n".as_bytes().to_vec();
        let chunks = vec![encoded[0..1].to_vec(), encoded[1..4].to_vec(), encoded[4..].to_vec()];
        assert_eq!(collect(chunks, MAX_LINE_BYTES).unwrap(), ["é日"]);
    }

    #[test]
    fn counts_bytes_not_characters_against_the_line_limit() {
        // 24 MiB of three-byte characters is 8 Mi characters: under the limit
        // only when the guard measures UTF-8 bytes.
        let chunk = "日".repeat(1024 * 1024).into_bytes();
        let fits = vec![chunk.clone(), chunk.clone(), chunk.clone(), b"\n".to_vec()];
        assert_eq!(collect(fits, MAX_LINE_BYTES).unwrap().len(), 1);
        let error = collect(vec![chunk; 23], MAX_LINE_BYTES).unwrap_err();
        assert!(error.to_string().contains("64 MiB"));
    }

    #[test]
    fn reads_a_long_single_line_without_quadratic_buffering() {
        let megabyte = vec![b'x'; 1024 * 1024];
        let mut chunks = vec![megabyte; 32];
        chunks.push(b"\n".to_vec());
        let started = Instant::now();
        let lines = collect(chunks, MAX_LINE_BYTES).unwrap();
        assert_eq!(lines[0].len(), 32 * 1024 * 1024);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn text_input_lists_attached_images_as_paths() {
        let input = serde_json::json!([
            {"type": "text", "text": "what is wrong here?"},
            {"type": "localImage", "path": "/tmp/shot.png"},
            {"type": "localImage", "path": "/tmp/two.jpg"},
        ]);
        assert_eq!(
            read_text_input(Some(&input)).unwrap(),
            "what is wrong here?\n\nAttached images (open each one with your file-reading tool):\n- /tmp/shot.png\n- /tmp/two.jpg"
        );
        let plain = serde_json::json!([{"type": "text", "text": " hi "}]);
        assert_eq!(read_text_input(Some(&plain)).unwrap(), "hi");
        let image_only = serde_json::json!([{"type": "localImage", "path": "/tmp/shot.png"}]);
        assert!(read_text_input(Some(&image_only)).is_err(), "an image still needs a prompt");
    }

    #[test]
    fn numbers_keep_integers_integral() {
        assert_eq!(num(150.0).to_string(), "150");
        assert_eq!(num(0.12).to_string(), "0.12");
        assert_eq!(
            text_content(Some(&json!([{ "text": "a" }, "b", { "content": [{ "text": "c" }] }])), true),
            "a\nb\nc"
        );
        assert_eq!(text_content(Some(&json!({ "message": "m" })), true), "m");
        assert_eq!(text_content(Some(&json!({ "message": "m" })), false), "");
    }

    #[test]
    fn uuids_have_version_and_variant() {
        let v4 = uuid_v4();
        assert_eq!(v4.len(), 36);
        assert_eq!(&v4[14..15], "4");
        assert!(matches!(&v4[19..20], "8" | "9" | "a" | "b"));
        assert_eq!(&uuid_v7()[14..15], "7");
        assert_ne!(uuid_v4(), uuid_v4());
    }
}
