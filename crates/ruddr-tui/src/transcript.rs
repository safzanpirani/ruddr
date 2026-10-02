//! The chat transcript, built incrementally from `events.jsonl` lines.
//!
//! The reader thread hands over complete lines; [`Transcript::apply_line`]
//! folds each one into the entry list without re-parsing history. Agent text
//! that arrives after the history load streams through a newline gate: the
//! completed lines wait in a queue that [`Transcript::drain`] commits on a
//! ~50 ms tick (faster when the queue grows), and the line still arriving
//! shows as plain text once the queue is empty. Committed text renders as
//! markdown. Every entry carries a version that changes whenever its rendered
//! form changes, so the render cache re-renders only that entry.
//!
//! The same lines also feed the tool details shown in the activity tab, the
//! latest commentary update, and the context-window fallback for older runs.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    User,
    Agent,
    Tool,
    Thought,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Completed,
    Failed,
}

impl ToolStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolStatus::Running => "running",
            ToolStatus::Completed => "completed",
            ToolStatus::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub kind: EntryKind,
    /// The whole text, trimmed: what copy and search see.
    pub text: String,
    pub status: Option<ToolStatus>,
    pub item_id: Option<String>,
    /// Rejected prompts stay in place, hidden, so indices never shift.
    pub hidden: bool,
    pub version: u64,
}

/// An agent message whose text is still being revealed.
#[derive(Debug)]
struct Stream {
    raw: String,
    /// Byte offset of the committed (markdown) text. Always a line boundary,
    /// or the end of `raw` once the message completed.
    shown: usize,
    completed: bool,
    /// When the oldest queued line became ready.
    queued_since: Option<Instant>,
}

impl Stream {
    /// Where the queue of complete lines ends.
    fn queue_end(&self) -> usize {
        if self.completed {
            self.raw.len()
        } else {
            self.raw.rfind('\n').map(|i| i + 1).unwrap_or(0)
        }
    }

    fn queued_lines(&self) -> usize {
        let end = self.queue_end();
        if end <= self.shown {
            0
        } else {
            self.raw[self.shown..end].split_inclusive('\n').count()
        }
    }
}

/// How many queued lines one drain tick commits. One line per tick reads
/// smoothly; a growing backlog or a long wait catches up so the reveal never
/// trails far behind the provider.
pub fn lines_to_commit(queued: usize, waited: Duration) -> usize {
    if queued == 0 {
        0
    } else if queued >= 16 || waited >= Duration::from_millis(400) {
        queued
    } else {
        1 + queued / 4
    }
}

pub const DRAIN_TICK: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolDetail {
    pub id: String,
    pub kind: String,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub status: Option<ToolStatus>,
    pub output: Option<String>,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<i64>,
    pub query: Option<String>,
    pub tool_name: Option<String>,
    pub input: Option<Value>,
    pub agent_thread_id: Option<String>,
    pub agent_path: Option<String>,
    pub activity_kind: Option<String>,
    pub timestamp_ms: Option<i64>,
}

impl ToolDetail {
    pub fn status(&self) -> ToolStatus {
        self.status.unwrap_or(ToolStatus::Running)
    }
}

/// The latest context-window snapshot from `thread/tokenUsage/updated`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextUsage {
    pub tokens: i64,
    pub window: Option<i64>,
}

#[derive(Debug, Default)]
pub struct Transcript {
    root_threads: HashSet<String>,
    context_thread: Option<String>,
    entries: Vec<Entry>,
    agent_index: HashMap<String, usize>,
    agent_raw: HashMap<String, String>,
    tool_index: HashMap<String, usize>,
    rejected: HashSet<String>,
    streams: HashMap<usize, Stream>,
    /// False while the history loads: text then appears at once.
    live: bool,
    clock: u64,
    /// Changes whenever any entry changes.
    pub generation: u64,
    tool_order: Vec<String>,
    tools: HashMap<String, ToolDetail>,
    messages_by_thread: HashMap<String, String>,
    turns_by_thread: HashMap<String, (Option<String>, Option<i64>)>,
    /// Changes whenever tool details or the commentary change.
    pub tool_generation: u64,
    pub context: Option<ContextUsage>,
    /// The latest full commentary update (`phase: "commentary"`).
    pub commentary: Option<String>,
}

fn str_of(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

pub fn flatten_summary(value: &Value) -> String {
    match value {
        Value::String(text) => text.trim().to_string(),
        Value::Array(parts) => parts
            .iter()
            .map(flatten_summary)
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        Value::Object(record) => flatten_summary(record.get("text").or_else(|| record.get("content")).unwrap_or(&Value::Null)),
        _ => String::new(),
    }
}

/// Joins `**A** **B**` summaries with a dot and drops the bold markers.
pub fn clean_thought(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("**") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let gap = after.len() - after.trim_start().len();
        if gap > 0 && after[gap..].starts_with("**") {
            out.push_str(" · ");
            rest = &after[gap + 2..];
        } else {
            rest = after;
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

impl Transcript {
    pub fn new(root_thread: Option<&str>) -> Transcript {
        Transcript {
            root_threads: root_thread.map(|t| HashSet::from([t.to_string()])).unwrap_or_default(),
            context_thread: root_thread.map(str::to_string),
            ..Default::default()
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Ends the history load: text that arrives from now on streams.
    pub fn finish_history(&mut self) {
        self.live = true;
    }

    /// True while queued lines wait for a drain tick.
    pub fn draining(&self) -> bool {
        self.streams.values().any(|s| s.queued_lines() > 0 || s.completed)
    }

    /// The committed markdown text and the in-progress plain line of an
    /// entry. Non-agent and finished entries return their whole text.
    pub fn display(&self, index: usize) -> (&str, &str) {
        let entry = &self.entries[index];
        match self.streams.get(&index) {
            Some(stream) => {
                let committed = &stream.raw[..stream.shown];
                let tail = if stream.completed || stream.shown < stream.queue_end() {
                    ""
                } else {
                    &stream.raw[stream.shown..]
                };
                (committed, tail)
            }
            None => (entry.text.as_str(), ""),
        }
    }

    /// True for an agent entry whose text is still being revealed.
    pub fn is_streaming(&self, index: usize) -> bool {
        self.streams.contains_key(&index)
    }

    fn bump(&mut self, index: usize) {
        self.clock += 1;
        self.entries[index].version = self.clock;
        self.generation += 1;
    }

    fn push(&mut self, mut entry: Entry) -> usize {
        self.clock += 1;
        entry.version = self.clock;
        self.entries.push(entry);
        self.generation += 1;
        self.entries.len() - 1
    }

    /// Commits queued lines. Returns true when anything on screen changed.
    pub fn drain(&mut self, now: Instant) -> bool {
        let mut changed = Vec::new();
        let mut finished = Vec::new();
        for (&index, stream) in self.streams.iter_mut() {
            let queued = stream.queued_lines();
            if queued == 0 {
                if stream.completed {
                    finished.push(index);
                }
                continue;
            }
            let waited = stream.queued_since.map(|t| now.saturating_duration_since(t)).unwrap_or_default();
            let commit = lines_to_commit(queued, waited);
            let end = stream.queue_end();
            let mut shown = stream.shown;
            for line in stream.raw[stream.shown..end].split_inclusive('\n').take(commit) {
                shown += line.len();
            }
            stream.shown = shown;
            stream.queued_since = if stream.queued_lines() > 0 { Some(now) } else { None };
            if stream.completed && stream.shown >= stream.raw.len() {
                finished.push(index);
            }
            changed.push(index);
        }
        for index in &finished {
            self.streams.remove(index);
        }
        for &index in changed.iter().chain(&finished) {
            self.bump(index);
        }
        !changed.is_empty() || !finished.is_empty()
    }

    /// Shows every queued line at once.
    pub fn commit_all(&mut self) {
        let indices: Vec<usize> = self.streams.keys().copied().collect();
        for index in indices {
            let stream = self.streams.get_mut(&index).unwrap();
            stream.shown = stream.queue_end();
            stream.queued_since = None;
            if stream.completed {
                self.streams.remove(&index);
            }
            self.bump(index);
        }
    }

    fn write_agent(&mut self, id: Option<&str>, text: String, completed: bool, now: Instant) {
        let trimmed = text.trim().to_string();
        let existing = id.and_then(|id| self.agent_index.get(id).copied());
        let previous_raw = id.and_then(|id| self.agent_raw.get(id).cloned()).unwrap_or_default();
        if let Some(id) = id {
            self.agent_raw.insert(id.to_string(), text.clone());
        }
        let index = match existing {
            Some(index) => {
                if trimmed.is_empty() {
                    return;
                }
                self.entries[index].text = trimmed;
                index
            }
            None => {
                if trimmed.is_empty() {
                    return;
                }
                let index = self.push(Entry {
                    kind: EntryKind::Agent,
                    text: trimmed,
                    status: None,
                    item_id: id.map(str::to_string),
                    hidden: false,
                    version: 0,
                });
                if let Some(id) = id {
                    self.agent_index.insert(id.to_string(), index);
                }
                index
            }
        };
        if self.live {
            match self.streams.get_mut(&index) {
                Some(stream) => {
                    // The completed item is authoritative. When it disagrees
                    // with what the deltas already showed, show it whole.
                    if !text.starts_with(&stream.raw[..stream.shown]) {
                        stream.shown = text.len();
                    }
                    if stream.queued_since.is_none() {
                        stream.queued_since = Some(now);
                    }
                    stream.raw = text;
                    stream.completed |= completed;
                }
                None => {
                    // Text already on screen stays; only the new part streams.
                    let shown = if existing.is_some() {
                        let common = previous_raw.len().min(text.len());
                        if text.as_bytes()[..common] == previous_raw.as_bytes()[..common] {
                            text[..common].rfind('\n').map(|i| i + 1).unwrap_or(0)
                        } else {
                            text.len()
                        }
                    } else {
                        0
                    };
                    let stream = Stream {
                        raw: text,
                        shown,
                        completed,
                        queued_since: Some(now),
                    };
                    if !(stream.completed && stream.shown >= stream.raw.len()) {
                        self.streams.insert(index, stream);
                    }
                }
            }
        }
        self.bump(index);
    }

    /// Folds one `events.jsonl` line into the transcript. Returns true when
    /// the chat, the tool details, or the context snapshot changed.
    pub fn apply_line(&mut self, line: &str, now: Instant) -> bool {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            // A bounded tail may begin in the middle of a record.
            return false;
        };
        let generation = (self.generation, self.tool_generation, self.context);
        self.apply_event(&event, now);
        generation != (self.generation, self.tool_generation, self.context)
    }

    fn apply_event(&mut self, event: &Value, now: Instant) {
        let method = event.get("method").and_then(Value::as_str).unwrap_or("");
        let params = event.get("params").unwrap_or(&Value::Null);
        if method == "ruddr/prompt/rejected" {
            if let Some(id) = str_of(params, "promptId") {
                for index in 0..self.entries.len() {
                    let entry = &self.entries[index];
                    if entry.kind == EntryKind::User && entry.item_id.as_deref() == Some(&id) && !entry.hidden {
                        self.entries[index].hidden = true;
                        self.bump(index);
                    }
                }
                self.rejected.insert(id);
            }
            return;
        }
        if method == "thread/tokenUsage/updated" {
            self.apply_token_usage(params);
            return;
        }
        let item = params.get("item").unwrap_or(&Value::Null);
        let thread = str_of(params, "threadId");
        self.apply_tool_detail(event, method, params, item, thread.as_deref());
        let item_type = str_of(item, "type");
        if item_type.as_deref() == Some("userMessage")
            && str_of(item, "origin").as_deref() == Some("ruddr")
            && let Some(thread) = &thread
        {
            self.root_threads.insert(thread.clone());
        }
        if !method.starts_with("item/") {
            return;
        }
        // Sub-agent items carry a different threadId; keep the root conversation.
        if let Some(thread) = &thread
            && !self.root_threads.is_empty()
            && !self.root_threads.contains(thread)
        {
            return;
        }
        if method == "item/agentMessage/delta" {
            if let (Some(id), Some(delta)) = (str_of(params, "itemId"), str_of(params, "delta"))
                && !delta.is_empty()
            {
                let text = self.agent_raw.get(&id).cloned().unwrap_or_default() + &delta;
                self.write_agent(Some(&id), text, false, now);
            }
            return;
        }
        let Some(item_type) = item_type else { return };
        let completed = method == "item/completed";
        let item_id = str_of(item, "id");
        match item_type.as_str() {
            "userMessage" => {
                // Ruddr records each prompt as a `text` item; providers report
                // their own user messages, steers included, as `content`.
                let text = str_of(item, "text")
                    .unwrap_or_else(|| flatten_summary(item.get("content").unwrap_or(&Value::Null)))
                    .trim()
                    .to_string();
                if completed && !text.is_empty() {
                    // The provider echoes the prompt Ruddr already recorded.
                    let duplicate = self.entries.last().is_some_and(|p| p.kind == EntryKind::User && p.text == text);
                    if !duplicate {
                        let hidden = item_id.as_ref().is_some_and(|id| self.rejected.contains(id));
                        self.push(Entry {
                            kind: EntryKind::User,
                            text,
                            status: None,
                            item_id,
                            hidden,
                            version: 0,
                        });
                    }
                }
            }
            "agentMessage" => {
                let text = str_of(item, "text").unwrap_or_default();
                match item_id {
                    None => {
                        if completed && !text.trim().is_empty() {
                            self.write_agent(None, text, true, now);
                        }
                    }
                    // The completed item carries the authoritative text; it
                    // supersedes the deltas, including a partial run recovered
                    // from a bounded tail.
                    Some(id) => {
                        if completed || !text.is_empty() {
                            self.write_agent(Some(&id), text, completed, now);
                        }
                    }
                }
            }
            "reasoning" => {
                if completed {
                    let text = clean_thought(&flatten_summary(item.get("summary").unwrap_or(&Value::Null)));
                    if !text.is_empty() {
                        self.push(Entry {
                            kind: EntryKind::Thought,
                            text,
                            status: None,
                            item_id: None,
                            hidden: false,
                            version: 0,
                        });
                    }
                }
            }
            "commandExecution" | "fileChange" | "webSearch" | "toolCall" | "subAgentActivity" => {
                let Some(id) = item_id else { return };
                let raw = ["command", "query", "toolName"]
                    .iter()
                    .find_map(|key| nonempty(str_of(item, key)))
                    .unwrap_or_else(|| {
                        if item_type == "fileChange" {
                            "file changes".into()
                        } else {
                            item_type.clone()
                        }
                    });
                let flat = raw.split_whitespace().collect::<Vec<_>>().join(" ");
                let label = if flat.chars().count() > 160 {
                    format!("{}…", flat.chars().take(160).collect::<String>())
                } else {
                    flat
                };
                let exit_failed = item.get("exitCode").and_then(Value::as_i64).is_some_and(|code| code != 0);
                let status = if method == "item/started" {
                    ToolStatus::Running
                } else if str_of(item, "status").as_deref() == Some("failed") || exit_failed {
                    ToolStatus::Failed
                } else if completed {
                    ToolStatus::Completed
                } else {
                    ToolStatus::Running
                };
                match self.tool_index.get(&id).copied() {
                    Some(index) => {
                        let entry = &mut self.entries[index];
                        if entry.text != label || entry.status != Some(status) {
                            entry.text = label;
                            entry.status = Some(status);
                            self.bump(index);
                        }
                    }
                    None => {
                        let index = self.push(Entry {
                            kind: EntryKind::Tool,
                            text: label,
                            status: Some(status),
                            item_id: Some(id.clone()),
                            hidden: false,
                            version: 0,
                        });
                        self.tool_index.insert(id, index);
                    }
                }
            }
            _ => {}
        }
    }

    fn apply_token_usage(&mut self, params: &Value) {
        if let (Some(root), Some(thread)) = (&self.context_thread, params.get("threadId").and_then(Value::as_str))
            && root != thread
        {
            return;
        }
        let usage = params.get("tokenUsage").unwrap_or(&Value::Null);
        let tokens = usage.get("last").and_then(|l| l.get("totalTokens")).and_then(Value::as_f64);
        self.context = match tokens {
            Some(tokens) if tokens.is_finite() && tokens >= 0.0 => {
                let window = usage
                    .get("modelContextWindow")
                    .or_else(|| usage.get("contextWindow"))
                    .and_then(Value::as_f64)
                    .filter(|w| w.is_finite() && *w > 0.0);
                Some(ContextUsage {
                    tokens: tokens as i64,
                    window: window.map(|w| w as i64),
                })
            }
            _ => None,
        };
    }

    fn apply_tool_detail(&mut self, event: &Value, method: &str, params: &Value, item: &Value, thread: Option<&str>) {
        let item_type = str_of(item, "type");
        if method == "item/completed"
            && item_type.as_deref() == Some("agentMessage")
            && let Some(text) = nonempty(str_of(item, "text"))
        {
            if str_of(item, "phase").as_deref() == Some("commentary") {
                self.commentary = Some(text.clone());
                self.tool_generation += 1;
            }
            if let Some(thread) = thread {
                let previous = self.messages_by_thread.insert(thread.to_string(), text);
                if self.tools.values().any(|d| d.agent_thread_id.as_deref() == Some(thread)) && previous.is_none() {
                    self.tool_generation += 1;
                }
            }
        }
        if method == "turn/completed"
            && let Some(thread) = thread
        {
            let turn = params.get("turn").unwrap_or(&Value::Null);
            self.turns_by_thread.insert(
                thread.to_string(),
                (str_of(turn, "status"), turn.get("durationMs").and_then(Value::as_i64)),
            );
            if self.tools.values().any(|d| d.agent_thread_id.as_deref() == Some(thread)) {
                self.tool_generation += 1;
            }
        }
        let (Some(id), Some(kind)) = (str_of(item, "id"), item_type) else {
            return;
        };
        if !method.starts_with("item/")
            || !matches!(
                kind.as_str(),
                "commandExecution" | "webSearch" | "fileChange" | "toolCall" | "subAgentActivity"
            )
        {
            return;
        }
        let status = if method == "item/started" || (method == "item/updated" && str_of(item, "status").as_deref() == Some("inProgress")) {
            ToolStatus::Running
        } else if str_of(item, "status").as_deref() == Some("failed")
            || item.get("exitCode").and_then(Value::as_i64).is_some_and(|c| c != 0)
        {
            ToolStatus::Failed
        } else {
            ToolStatus::Completed
        };
        let sub_agent = kind == "subAgentActivity";
        if !self.tools.contains_key(&id) {
            self.tool_order.push(id.clone());
            // Bound memory over very long sessions.
            if self.tool_order.len() > 1000 {
                let dropped = self.tool_order.remove(0);
                self.tools.remove(&dropped);
            }
        }
        let detail = self.tools.entry(id.clone()).or_default();
        detail.id = id;
        detail.kind = kind;
        detail.command = str_of(item, "command")
            .or_else(|| if sub_agent { str_of(item, "agentPath") } else { None })
            .or(detail.command.take());
        detail.cwd = str_of(item, "cwd").or(detail.cwd.take());
        detail.status = Some(status);
        detail.output = str_of(item, "aggregatedOutput").or(detail.output.take());
        detail.exit_code = item.get("exitCode").and_then(Value::as_i64).or(detail.exit_code);
        detail.duration_ms = item.get("durationMs").and_then(Value::as_i64).or(detail.duration_ms);
        detail.query = str_of(item, "query").or(detail.query.take());
        detail.tool_name = str_of(item, "toolName")
            .or_else(|| if sub_agent { Some("subAgentActivity".into()) } else { None })
            .or(detail.tool_name.take());
        detail.input = item.get("input").filter(|v| !v.is_null()).cloned().or(detail.input.take());
        detail.agent_thread_id = str_of(item, "agentThreadId").or(detail.agent_thread_id.take());
        detail.agent_path = str_of(item, "agentPath").or(detail.agent_path.take());
        detail.activity_kind = str_of(item, "kind").or(detail.activity_kind.take());
        detail.timestamp_ms = event.get("emittedAtMs").and_then(Value::as_i64).or(detail.timestamp_ms);
        self.tool_generation += 1;
    }

    /// Tool details in first-seen order, with sub-agent rows joined to the
    /// child thread's final message and turn.
    pub fn tool_details(&self) -> Vec<ToolDetail> {
        self.tool_order
            .iter()
            .filter_map(|id| self.tools.get(id))
            .map(|detail| {
                let mut detail = detail.clone();
                if detail.kind == "subAgentActivity"
                    && let Some(thread) = detail.agent_thread_id.clone()
                {
                    if let Some(message) = self.messages_by_thread.get(&thread) {
                        detail.output = Some(message.clone());
                    }
                    if let Some((status, duration)) = self.turns_by_thread.get(&thread) {
                        detail.duration_ms = duration.or(detail.duration_ms);
                        if let Some(status) = status {
                            detail.status = Some(if status == "completed" {
                                ToolStatus::Completed
                            } else {
                                ToolStatus::Failed
                            });
                        }
                    }
                }
                detail
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn load(lines: &[String]) -> Transcript {
        let mut t = Transcript::new(None);
        let now = Instant::now();
        for line in lines {
            t.apply_line(line, now);
        }
        t.finish_history();
        t
    }

    fn texts(t: &Transcript) -> Vec<(EntryKind, String)> {
        t.entries().iter().filter(|e| !e.hidden).map(|e| (e.kind, e.text.clone())).collect()
    }

    fn delta(id: &str, text: &str) -> String {
        json!({"method":"item/agentMessage/delta","params":{"threadId":"t","itemId":id,"delta":text}}).to_string()
    }

    fn agent(id: &str, text: &str) -> String {
        json!({"method":"item/completed","params":{"threadId":"t","item":{"type":"agentMessage","id":id,"text":text}}}).to_string()
    }

    #[test]
    fn folds_deltas_tools_and_prompts() {
        let t = load(&[
            r#"{"method":"item/completed","params":{"threadId":"t","item":{"type":"userMessage","origin":"ruddr","id":"p1","text":"hi"}}}"#.into(),
            r#"{"method":"item/completed","params":{"threadId":"t","item":{"type":"userMessage","content":[{"text":"hi"}]}}}"#.into(),
            delta("a", "Hel"),
            delta("a", "lo"),
            r#"{"method":"item/started","params":{"threadId":"t","item":{"type":"commandExecution","id":"c","command":"ls  -la"}}}"#.into(),
            r#"{"method":"item/completed","params":{"threadId":"t","item":{"type":"commandExecution","id":"c","command":"ls -la","exitCode":1}}}"#.into(),
            r#"{"method":"item/completed","params":{"threadId":"sub","item":{"type":"agentMessage","id":"x","text":"sub agent"}}}"#.into(),
            r#"{"method":"item/completed","params":{"threadId":"t","item":{"type":"reasoning","summary":["**Plan** **Act**"]}}}"#.into(),
        ]);
        assert_eq!(
            texts(&t),
            vec![
                (EntryKind::User, "hi".into()),
                (EntryKind::Agent, "Hello".into()),
                (EntryKind::Tool, "ls -la".into()),
                (EntryKind::Thought, "Plan · Act".into()),
            ]
        );
        assert_eq!(t.entries()[2].status, Some(ToolStatus::Failed));
    }

    #[test]
    fn completed_text_supersedes_partial_deltas_and_root_thread_filters() {
        let mut t = Transcript::new(Some("root"));
        let now = Instant::now();
        for line in [
            json!({"method":"item/agentMessage/delta","params":{"threadId":"root","itemId":"a","delta":"partial"}}).to_string(),
            json!({"method":"item/agentMessage/delta","params":{"threadId":"other","itemId":"b","delta":"old thread"}}).to_string(),
            json!({"method":"item/completed","params":{"threadId":"root","item":{"type":"agentMessage","id":"a","text":"Final answer"}}})
                .to_string(),
        ] {
            t.apply_line(&line, now);
        }
        assert_eq!(texts(&t), vec![(EntryKind::Agent, "Final answer".into())]);
    }

    #[test]
    fn rejected_prompts_hide_in_place() {
        let mut t = load(&[
            r#"{"method":"item/completed","params":{"threadId":"t","item":{"type":"userMessage","origin":"ruddr","id":"p1","text":"no"}}}"#
                .into(),
        ]);
        let version = t.entries()[0].version;
        assert!(t.apply_line(r#"{"method":"ruddr/prompt/rejected","params":{"promptId":"p1"}}"#, Instant::now()));
        assert!(texts(&t).is_empty());
        assert!(t.entries()[0].version > version, "hiding changes the entry version");
    }

    #[test]
    fn history_appears_at_once() {
        let t = load(&[delta("a", "one\ntwo\nthr")]);
        assert!(!t.is_streaming(0), "history is not animated");
        assert_eq!(t.display(0), ("one\ntwo\nthr", ""));
    }

    #[test]
    fn streams_behind_a_newline_gate() {
        let mut t = load(&[]);
        let start = Instant::now();
        t.apply_line(&delta("a", "# Title\nsecond li"), start);
        // The first complete line waits in the queue; nothing partial shows yet.
        assert_eq!(t.display(0), ("", ""));
        assert!(t.draining());
        assert!(t.drain(start + DRAIN_TICK));
        assert_eq!(
            t.display(0),
            ("# Title\n", "second li"),
            "the committed line is markdown, the arriving line plain"
        );
        let version = t.entries()[0].version;
        t.apply_line(&delta("a", "ne\nthird"), start + DRAIN_TICK);
        assert!(t.entries()[0].version > version);
        assert_eq!(t.display(0), ("# Title\n", ""), "the plain tail hides while lines are queued");
        t.drain(start + DRAIN_TICK * 2);
        assert_eq!(t.display(0), ("# Title\nsecond line\n", "third"));
        t.apply_line(&agent("a", "# Title\nsecond line\nthird line"), start + DRAIN_TICK * 2);
        t.drain(start + DRAIN_TICK * 3);
        assert_eq!(t.display(0), (t.entries()[0].text.as_str(), ""));
        assert!(!t.is_streaming(0), "a completed, drained message stops streaming");
        assert!(!t.draining());
    }

    #[test]
    fn drain_speeds_up_with_the_backlog() {
        assert_eq!(lines_to_commit(0, Duration::ZERO), 0);
        assert_eq!(lines_to_commit(1, Duration::ZERO), 1);
        assert_eq!(lines_to_commit(8, Duration::ZERO), 3);
        assert_eq!(lines_to_commit(16, Duration::ZERO), 16);
        assert_eq!(lines_to_commit(3, Duration::from_millis(500)), 3);
        let mut t = load(&[]);
        let start = Instant::now();
        let long: String = (0..40).map(|i| format!("line {i}\n")).collect();
        t.apply_line(&delta("a", &long), start);
        t.drain(start);
        assert_eq!(t.display(0).0, long, "a 40-line burst catches up in one tick");
    }

    #[test]
    fn whole_messages_arriving_live_also_stream() {
        let mut t = load(&[]);
        let start = Instant::now();
        t.apply_line(&agent("a", "one\ntwo"), start);
        assert_eq!(t.display(0), ("", ""));
        t.drain(start);
        assert_eq!(t.display(0).0, "one\n");
        t.drain(start + DRAIN_TICK);
        assert_eq!(t.display(0), ("one\ntwo", ""));
        assert!(!t.is_streaming(0));
    }

    #[test]
    fn continuing_history_streams_only_new_text() {
        let mut t = load(&[delta("a", "done line\nhalf")]);
        let start = Instant::now();
        t.apply_line(&delta("a", " line\nnext"), start);
        assert_eq!(t.display(0), ("done line\n", ""), "the visible line stays, the rest queues");
        t.drain(start);
        assert_eq!(t.display(0), ("done line\nhalf line\n", "next"));
    }

    #[test]
    fn only_the_changed_entry_gets_a_new_version() {
        let mut t = load(&[agent("a", "first"), delta("b", "x")]);
        let first = t.entries()[0].version;
        t.apply_line(&delta("b", "y\n"), Instant::now());
        t.drain(Instant::now());
        assert_eq!(t.entries()[0].version, first);
    }

    #[test]
    fn joins_tool_lifecycle_into_details() {
        let t = load(&[
            json!({"method":"item/started","params":{"item":{"id":"tool-1","type":"commandExecution","command":"go test ./...","cwd":"/work/parser","status":"inProgress"}}}).to_string(),
            json!({"method":"item/completed","params":{"item":{"id":"tool-1","type":"commandExecution","command":"go test ./...","cwd":"/work/parser","status":"completed","aggregatedOutput":"ok parser","exitCode":0,"durationMs":1250}}}).to_string(),
            json!({"method":"item/started","params":{"item":{"id":"claude","type":"toolCall","toolName":"Grep","input":{},"command":"Grep","status":"inProgress"}}}).to_string(),
            json!({"method":"item/updated","params":{"item":{"id":"claude","type":"toolCall","toolName":"Grep","input":{"pattern":"provider"},"command":"Grep provider","status":"inProgress"}}}).to_string(),
        ]);
        let details = t.tool_details();
        assert_eq!(details.len(), 2);
        assert_eq!(details[0].status(), ToolStatus::Completed);
        assert_eq!(details[0].output.as_deref(), Some("ok parser"));
        assert_eq!(details[0].duration_ms, Some(1250));
        assert_eq!(details[1].status(), ToolStatus::Running);
        assert_eq!(details[1].input, Some(json!({"pattern":"provider"})));
        assert_eq!(details[1].command.as_deref(), Some("Grep provider"));
    }

    #[test]
    fn joins_sub_agents_to_the_child_answer() {
        let t = load(&[
            json!({"method":"item/started","emittedAtMs":1787942847416i64,"params":{"threadId":"parent","item":{"id":"sub","type":"subAgentActivity","kind":"completed","agentThreadId":"child","agentPath":"/root/tests_review"}}}).to_string(),
            json!({"method":"item/completed","params":{"threadId":"child","item":{"id":"m","type":"agentMessage","phase":"final_answer","text":"Child review found no issues."}}}).to_string(),
            json!({"method":"item/completed","emittedAtMs":1787942847417i64,"params":{"threadId":"parent","item":{"id":"sub","type":"subAgentActivity","kind":"completed","agentThreadId":"child","agentPath":"/root/tests_review"}}}).to_string(),
            json!({"method":"turn/completed","params":{"threadId":"child","turn":{"status":"completed","durationMs":121565}}}).to_string(),
        ]);
        let detail = &t.tool_details()[0];
        assert_eq!(detail.command.as_deref(), Some("/root/tests_review"));
        assert_eq!(detail.output.as_deref(), Some("Child review found no issues."));
        assert_eq!(detail.duration_ms, Some(121565));
        assert_eq!(detail.tool_name.as_deref(), Some("subAgentActivity"));
        assert_eq!(detail.activity_kind.as_deref(), Some("completed"));
        assert_eq!(detail.timestamp_ms, Some(1787942847417));
    }

    #[test]
    fn keeps_the_latest_commentary_and_root_context() {
        let t = load(&[
            "partial tail record".into(),
            json!({"method":"item/completed","params":{"item":{"type":"agentMessage","phase":"commentary","text":"First update"}}})
                .to_string(),
            json!({"method":"item/completed","params":{"item":{"type":"agentMessage","phase":"commentary","text":"Full latest update"}}})
                .to_string(),
            json!({"method":"item/completed","params":{"item":{"type":"agentMessage","phase":"final","text":"Long final handoff"}}})
                .to_string(),
        ]);
        assert_eq!(t.commentary.as_deref(), Some("Full latest update"));
        let mut t = Transcript::new(Some("root"));
        let now = Instant::now();
        t.apply_line(&json!({"method":"thread/tokenUsage/updated","params":{"threadId":"root","tokenUsage":{"total":{"totalTokens":900000},"last":{"totalTokens":42000},"modelContextWindow":200000}}}).to_string(), now);
        t.apply_line(
            &json!({"method":"thread/tokenUsage/updated","params":{"threadId":"child","tokenUsage":{"last":{"totalTokens":7}}}})
                .to_string(),
            now,
        );
        assert_eq!(
            t.context,
            Some(ContextUsage {
                tokens: 42000,
                window: Some(200000)
            })
        );
    }

    #[test]
    fn cleans_thought_markers() {
        assert_eq!(
            clean_thought("**Inspecting tests** **Planning fix**"),
            "Inspecting tests · Planning fix"
        );
        assert_eq!(clean_thought("plain **bold** text"), "plain bold text");
    }
}
