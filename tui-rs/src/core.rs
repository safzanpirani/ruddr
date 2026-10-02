// Pure session logic: discovery, ordering, transcript parsing, and the
// `ruddr` argument builders. Mirrors tui/core.ts.

use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub total_tokens: Option<u64>,
    pub context_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub provider: Option<String>,
    pub pid: i64,
    pub status: String,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cwd: Option<String>,
    pub sandbox: Option<String>,
    pub state_dir: String,
    pub events_path: Option<String>,
    pub trace_path: Option<String>,
    pub output_path: Option<String>,
    pub steers: Option<u64>,
    pub turns: Option<u64>,
    pub token_usage: Option<TokenUsage>,
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
    pub completed_at: Option<String>,
    pub error: Option<String>,
}

impl Session {
    pub fn provider(&self) -> &str {
        self.provider.as_deref().unwrap_or("codex")
    }
}

pub fn is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "interrupted")
}

pub fn default_registry_dirs() -> Vec<PathBuf> {
    for name in ["RUDDR_REGISTRY_DIR", "RUDDER_REGISTRY_DIR", "CODEX_RUDDER_REGISTRY_DIR"] {
        if let Ok(value) = std::env::var(name) {
            if !value.is_empty() {
                return vec![absolute(Path::new(&value))];
            }
        }
    }
    let state_home = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local").join("state"));
    // Registries written before the rename stay visible.
    ["ruddr", "rudder", "codex-rudder"]
        .iter()
        .map(|name| state_home.join(name).join("runs"))
        .collect()
}

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/"))
}

pub fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

pub fn process_alive(pid: i64) -> bool {
    if pid <= 0 || pid > i32::MAX as i64 {
        return false;
    }
    // SAFETY: signal 0 only checks for existence and permission.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn discover(state_dirs: &[PathBuf], roots: &[PathBuf], registries: &[PathBuf]) -> Vec<Session> {
    let mut files = BTreeSet::new();
    for dir in state_dirs {
        files.insert(absolute(dir).join("state.json"));
    }
    for root in roots {
        collect_state_files(&absolute(root), &mut files);
    }
    for registry in registries {
        let Ok(entries) = fs::read_dir(registry) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !name.to_string_lossy().ends_with(".run") {
                continue;
            }
            if let Ok(text) = fs::read_to_string(entry.path()) {
                let dir = text.trim();
                if !dir.is_empty() {
                    files.insert(absolute(Path::new(dir)).join("state.json"));
                }
            }
        }
    }
    let mut sessions: Vec<Session> = files
        .iter()
        .filter_map(|file| {
            let mut session: Session = serde_json::from_slice(&fs::read(file).ok()?).ok()?;
            if session.state_dir.is_empty() {
                return None;
            }
            if !is_terminal(&session.status) && !process_alive(session.pid) {
                session.error = Some(format!("Ruddr pid {} is not running; persisted state is stale", session.pid));
                session.status = "stale".into();
            }
            Some(session)
        })
        .collect();
    sort_sessions(&mut sessions);
    sessions
}

fn collect_state_files(dir: &Path, out: &mut BTreeSet<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        if kind.is_file() && name == "state.json" {
            out.insert(entry.path());
        } else if kind.is_dir() && name != ".git" && name != "node_modules" {
            collect_state_files(&entry.path(), out);
        }
    }
}

fn status_rank(status: &str) -> u8 {
    match status {
        "active" => 0,
        "idle" => 1,
        "starting" => 2,
        "stale" => 4,
        _ => 3,
    }
}

pub fn sort_sessions(sessions: &mut [Session]) {
    sessions.sort_by(|left, right| {
        status_rank(&left.status)
            .cmp(&status_rank(&right.status))
            .then_with(|| parse_time(&right.updated_at).cmp(&parse_time(&left.updated_at)))
    });
}

pub fn filter_sessions<'a>(sessions: &'a [Session], query: &str) -> Vec<&'a Session> {
    let needle = query.trim().to_lowercase();
    sessions
        .iter()
        .filter(|session| {
            needle.is_empty()
                || [
                    Some(session.status.as_str()),
                    Some(session.provider()),
                    session.cwd.as_deref(),
                    session.model.as_deref(),
                    session.thread_id.as_deref(),
                    Some(session.state_dir.as_str()),
                ]
                .into_iter()
                .flatten()
                .any(|field| field.to_lowercase().contains(&needle))
        })
        .collect()
}

// --- time -----------------------------------------------------------------

/// Parses the RFC 3339 timestamps Go writes into Unix seconds.
pub fn parse_time(value: &Option<String>) -> Option<i64> {
    let text = value.as_deref()?;
    let bytes = text.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let num = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut rest = &text[19..];
    if let Some(stripped) = rest.strip_prefix('.') {
        let digits = stripped.find(|c: char| !c.is_ascii_digit()).unwrap_or(stripped.len());
        rest = &stripped[digits..];
    }
    let offset = if rest.is_empty() || rest == "Z" {
        0
    } else {
        let sign = if rest.starts_with('-') { -1 } else { 1 };
        let hours: i64 = rest.get(1..3)?.parse().ok()?;
        let minutes: i64 = rest.get(4..6)?.parse().ok()?;
        sign * (hours * 3600 + minutes * 60)
    };
    // Days from civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hour * 3600 + minute * 60 + second - offset)
}

pub fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn format_age(value: &Option<String>, now: i64) -> String {
    let Some(timestamp) = parse_time(value) else {
        return "unknown".into();
    };
    let seconds = (now - timestamp).max(0);
    match seconds {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

pub fn format_elapsed(start: &Option<String>, end: &Option<String>, now: i64) -> String {
    let Some(start) = parse_time(start) else { return "unknown".into() };
    let end = parse_time(end).filter(|end| *end >= start).unwrap_or(now);
    let seconds = (end - start).max(0);
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

// --- presentation ---------------------------------------------------------

pub fn status_glyph(status: &str) -> &'static str {
    match status {
        "active" => "●",
        "idle" => "◌",
        "starting" => "◐",
        "completed" => "✓",
        "failed" => "×",
        "interrupted" => "■",
        "stale" => "!",
        _ => "?",
    }
}

pub fn project_name(session: &Session) -> String {
    let path = session
        .cwd
        .clone()
        .filter(|cwd| !cwd.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&session.state_dir).parent().map(Path::to_path_buf).unwrap_or_default());
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| session.state_dir.clone())
}

pub fn format_token_count(value: u64) -> String {
    if value >= 1_000_000 {
        if value >= 10_000_000 {
            format!("{}M", value / 1_000_000)
        } else {
            format!("{:.1}M", value as f64 / 1_000_000.0)
        }
    } else if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

pub fn format_token_usage(usage: &TokenUsage) -> String {
    let mut parts = Vec::new();
    if let Some(total) = usage.total_tokens.filter(|t| *t > 0) {
        parts.push(format!("{} total", format_token_count(total)));
    }
    if let Some(cost) = usage.cost_usd.filter(|c| *c > 0.0) {
        parts.push(format!("${cost:.2}"));
    }
    parts.join(" · ")
}

pub fn context_meter(usage: &TokenUsage, cells: usize) -> Option<String> {
    let window = usage.context_window.filter(|w| *w > 0)?;
    let used = usage.context_tokens?;
    let ratio = (used as f64 / window as f64).clamp(0.0, 1.0);
    let filled = (ratio * cells as f64).round() as usize;
    Some(format!(
        "{}{} {} · {}%",
        "▰".repeat(filled),
        "▱".repeat(cells - filled),
        format_token_count(used),
        (ratio * 100.0).round()
    ))
}

pub fn session_details(session: &Session, now: i64) -> Vec<(String, String)> {
    let mut rows = vec![
        (
            "status".into(),
            format!(
                "{} {}    {}",
                status_glyph(&session.status),
                session.status,
                format_elapsed(&session.started_at, &session.completed_at, now)
            ),
        ),
        ("provider".into(), session.provider().to_string()),
        (
            "model".into(),
            format!(
                "{}{}",
                session.model.as_deref().unwrap_or("—"),
                session.effort.as_deref().map(|e| format!(" / {e}")).unwrap_or_default()
            ),
        ),
        ("thread".into(), session.thread_id.clone().unwrap_or_else(|| "—".into())),
        ("cwd".into(), session.cwd.clone().unwrap_or_else(|| "—".into())),
        (
            "runtime".into(),
            format!(
                "pid {}    steers {}{}",
                session.pid,
                session.steers.unwrap_or(0),
                session.turns.map(|t| format!("    turns {t}")).unwrap_or_default()
            ),
        ),
    ];
    if let Some(usage) = &session.token_usage {
        let mut text = format_token_usage(usage);
        if let Some(meter) = context_meter(usage, 8) {
            text = if text.is_empty() { meter } else { format!("{text}    ctx {meter}") };
        }
        if !text.is_empty() {
            rows.push(("tokens".into(), text));
        }
    }
    if let Some(error) = &session.error {
        rows.push(("error".into(), error.clone()));
    }
    rows
}

// --- artifacts ------------------------------------------------------------

/// Reads at most `max_bytes` from the end of a file, starting on a line boundary.
pub fn read_tail(path: Option<&str>, max_bytes: u64) -> String {
    let Some(path) = path.filter(|p| !p.is_empty()) else {
        return String::new();
    };
    let Ok(mut file) = fs::File::open(path) else { return String::new() };
    let Ok(size) = file.metadata().map(|m| m.len()) else {
        return String::new();
    };
    let start = size.saturating_sub(max_bytes);
    let read_start = start.saturating_sub(1);
    if file.seek(SeekFrom::Start(read_start)).is_err() {
        return String::new();
    }
    let mut buffer = Vec::with_capacity((size - read_start) as usize);
    if file.read_to_end(&mut buffer).is_err() {
        return String::new();
    }
    let on_boundary = start == 0 || buffer.first() == Some(&b'\n');
    let body = if start == 0 { &buffer[..] } else { &buffer[1..] };
    let mut text = String::from_utf8_lossy(body).into_owned();
    if !on_boundary {
        text = match text.find('\n') {
            Some(index) => text[index + 1..].to_string(),
            None => text,
        };
    }
    text.trim_end().to_string()
}

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

#[derive(Debug, Clone, PartialEq)]
pub struct ChatEntry {
    pub kind: EntryKind,
    pub text: String,
    pub status: Option<ToolStatus>,
    pub item_id: Option<String>,
}

fn flatten_summary(value: &Value) -> String {
    match value {
        Value::String(text) => text.trim().to_string(),
        Value::Array(parts) => parts
            .iter()
            .map(flatten_summary)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        Value::Object(record) => flatten_summary(record.get("text").or_else(|| record.get("content")).unwrap_or(&Value::Null)),
        _ => String::new(),
    }
}

fn clean_thought(text: &str) -> String {
    text.replace("**", "").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Folds events.jsonl into the root conversation: prompts, agent messages
/// (streamed deltas included), reasoning summaries, and one-line tool entries.
pub fn parse_chat_transcript(content: &str, root_thread: Option<&str>) -> Vec<ChatEntry> {
    let mut entries: Vec<ChatEntry> = Vec::new();
    let mut tool_index: HashMap<String, usize> = HashMap::new();
    let mut agent_index: HashMap<String, usize> = HashMap::new();
    let mut agent_text: HashMap<String, String> = HashMap::new();
    let mut rejected: HashSet<String> = HashSet::new();
    let mut roots: HashSet<String> = root_thread.map(|t| HashSet::from([t.to_string()])).unwrap_or_default();

    let write_agent = |entries: &mut Vec<ChatEntry>,
                       agent_index: &mut HashMap<String, usize>,
                       agent_text: &mut HashMap<String, String>,
                       id: &str,
                       text: String| {
        let trimmed = text.trim().to_string();
        agent_text.insert(id.to_string(), text);
        let entry = ChatEntry {
            kind: EntryKind::Agent,
            text: trimmed.clone(),
            status: None,
            item_id: Some(id.into()),
        };
        if let Some(&index) = agent_index.get(id) {
            if !trimmed.is_empty() {
                entries[index] = entry;
            }
        } else if !trimmed.is_empty() {
            agent_index.insert(id.to_string(), entries.len());
            entries.push(entry);
        }
    };

    for line in content.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else { continue };
        let method = event.get("method").and_then(Value::as_str).unwrap_or("");
        let params = event.get("params").cloned().unwrap_or(Value::Null);
        let str_of = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
        if method == "ruddr/prompt/rejected" {
            if let Some(id) = str_of(&params, "promptId") {
                rejected.insert(id);
            }
            continue;
        }
        let item = params.get("item").cloned().unwrap_or(Value::Null);
        let thread = str_of(&params, "threadId");
        let item_type = str_of(&item, "type");
        if item_type.as_deref() == Some("userMessage") && str_of(&item, "origin").as_deref() == Some("ruddr") {
            if let Some(thread) = &thread {
                roots.insert(thread.clone());
            }
        }
        if !method.starts_with("item/") {
            continue;
        }
        if let Some(thread) = &thread {
            if !roots.is_empty() && !roots.contains(thread) {
                continue;
            }
        }
        if method == "item/agentMessage/delta" {
            if let (Some(id), Some(delta)) = (str_of(&params, "itemId"), str_of(&params, "delta")) {
                let text = agent_text.get(&id).cloned().unwrap_or_default() + &delta;
                write_agent(&mut entries, &mut agent_index, &mut agent_text, &id, text);
            }
            continue;
        }
        let Some(item_type) = item_type else { continue };
        let completed = method == "item/completed";
        let item_id = str_of(&item, "id");
        match item_type.as_str() {
            "userMessage" => {
                let text = str_of(&item, "text")
                    .unwrap_or_else(|| flatten_summary(item.get("content").unwrap_or(&Value::Null)))
                    .trim()
                    .to_string();
                if completed && !text.is_empty() {
                    let duplicate = entries.last().is_some_and(|p| p.kind == EntryKind::User && p.text == text);
                    if !duplicate {
                        entries.push(ChatEntry {
                            kind: EntryKind::User,
                            text,
                            status: None,
                            item_id,
                        });
                    }
                }
            }
            "agentMessage" => {
                let text = str_of(&item, "text").unwrap_or_default();
                match item_id {
                    None => {
                        if completed && !text.trim().is_empty() {
                            entries.push(ChatEntry {
                                kind: EntryKind::Agent,
                                text: text.trim().into(),
                                status: None,
                                item_id: None,
                            });
                        }
                    }
                    Some(id) => {
                        if completed || !text.is_empty() {
                            write_agent(&mut entries, &mut agent_index, &mut agent_text, &id, text);
                        }
                    }
                }
            }
            "reasoning" => {
                if completed {
                    let text = clean_thought(&flatten_summary(item.get("summary").unwrap_or(&Value::Null)));
                    if !text.is_empty() {
                        entries.push(ChatEntry {
                            kind: EntryKind::Thought,
                            text,
                            status: None,
                            item_id: None,
                        });
                    }
                }
            }
            "commandExecution" | "fileChange" | "webSearch" | "toolCall" | "subAgentActivity" => {
                let Some(id) = item_id else { continue };
                let raw = ["command", "query", "toolName"]
                    .iter()
                    .filter_map(|key| str_of(&item, key).filter(|v| !v.is_empty()))
                    .next()
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
                } else if str_of(&item, "status").as_deref() == Some("failed") || exit_failed {
                    ToolStatus::Failed
                } else if completed {
                    ToolStatus::Completed
                } else {
                    ToolStatus::Running
                };
                let entry = ChatEntry {
                    kind: EntryKind::Tool,
                    text: label,
                    status: Some(status),
                    item_id: Some(id.clone()),
                };
                if let Some(&index) = tool_index.get(&id) {
                    entries[index] = entry;
                } else {
                    tool_index.insert(id, entries.len());
                    entries.push(entry);
                }
            }
            _ => {}
        }
    }
    entries
        .into_iter()
        .filter(|e| e.kind != EntryKind::User || e.item_id.as_ref().is_none_or(|id| !rejected.contains(id)))
        .collect()
}

// --- prompt routing -------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptRoute {
    Steer,
    Prompt,
    Continue,
}

/// Active turns get steered, idle sessions get a new turn over the control
/// socket, finished threads get a continuation run. Never converts routes.
pub fn prompt_route(session: &Session) -> Option<PromptRoute> {
    match session.status.as_str() {
        "active" if session.turn_id.is_some() => Some(PromptRoute::Steer),
        "idle" => Some(PromptRoute::Prompt),
        status if is_terminal(status) && session.thread_id.is_some() && session.cwd.is_some() => Some(PromptRoute::Continue),
        _ => None,
    }
}

pub fn steer_args(session: &Session, message_file: &str) -> Vec<String> {
    let turn = session.turn_id.clone().unwrap_or_default();
    [
        "steer",
        "--state-dir",
        &session.state_dir,
        "--expected-turn-id",
        &turn,
        "--message-file",
        message_file,
    ]
    .map(String::from)
    .to_vec()
}

pub fn idle_prompt_args(session: &Session, message_file: &str) -> Vec<String> {
    ["prompt", "--state-dir", &session.state_dir, "--message-file", message_file]
        .map(String::from)
        .to_vec()
}

pub struct LaunchOverrides {
    pub model: Option<String>,
    pub effort: Option<String>,
}

pub fn continuation_args(session: &Session, prompt_file: &str, state_dir: &str, overrides: &LaunchOverrides) -> Vec<String> {
    let mut args: Vec<String> = [
        "run",
        "--detach",
        "--provider",
        session.provider(),
        "--cwd",
        session.cwd.as_deref().unwrap_or_default(),
        "--resume-thread",
        session.thread_id.as_deref().unwrap_or_default(),
        "--prompt-file",
        prompt_file,
        "--state-dir",
        state_dir,
        "--sandbox",
        session.sandbox.as_deref().unwrap_or("workspace-write"),
        "--approval-policy",
        "never",
        "--idle",
    ]
    .map(String::from)
    .to_vec();
    if let Some(model) = overrides.model.clone().or_else(|| session.model.clone()) {
        args.extend(["--model".into(), model]);
    }
    if let Some(effort) = overrides
        .effort
        .clone()
        .or_else(|| if overrides.model.is_some() { None } else { session.effort.clone() })
    {
        args.extend(["--effort".into(), effort]);
    }
    args
}

pub fn new_session_args(
    provider: &str,
    cwd: &str,
    prompt_file: &str,
    state_dir: &str,
    overrides: &LaunchOverrides,
    resume_thread: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "run",
        "--detach",
        "--provider",
        provider,
        "--cwd",
        cwd,
        "--prompt-file",
        prompt_file,
        "--state-dir",
        state_dir,
        "--sandbox",
        "workspace-write",
        "--approval-policy",
        "never",
        "--idle",
    ]
    .map(String::from)
    .to_vec();
    if let Some(thread) = resume_thread {
        args.extend(["--resume-thread".into(), thread.into()]);
    }
    if let Some(model) = &overrides.model {
        args.extend(["--model".into(), model.clone()]);
    }
    if let Some(effort) = &overrides.effort {
        args.extend(["--effort".into(), effort.clone()]);
    }
    args
}

// --- models ---------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ModelInfo {
    pub provider: String,
    pub id: Option<String>,
    pub label: Option<String>,
    #[serde(default)]
    pub efforts: Vec<String>,
    #[serde(default)]
    pub default: bool,
    #[serde(default = "yes")]
    pub available: bool,
    pub note: Option<String>,
}

fn yes() -> bool {
    true
}

impl ModelInfo {
    pub fn name(&self) -> String {
        self.label
            .clone()
            .or_else(|| self.id.clone())
            .unwrap_or_else(|| self.provider.clone())
    }
}

/// Embedded fallback for ruddr binaries without `ruddr models`.
pub fn fallback_models() -> Vec<ModelInfo> {
    let m = |provider: &str, id: &str, label: &str, efforts: &[&str], default: bool| ModelInfo {
        provider: provider.into(),
        id: Some(id.into()),
        label: Some(label.into()),
        efforts: efforts.iter().map(|e| e.to_string()).collect(),
        default,
        available: true,
        note: None,
    };
    let sol = ["low", "medium", "high", "xhigh", "max", "ultra"];
    vec![
        m("codex", "gpt-6-astra", "GPT-6-Astra", &[], true),
        m("codex", "gpt-6.1-sol", "GPT-6.1-Sol", &sol, false),
        m("codex", "gpt-6-sol", "GPT-6-Sol", &sol, false),
        m("codex", "gpt-6-luna", "GPT-6-Luna", &sol[..5], false),
        m("claude", "claude-opus-5-5", "Claude Opus 5.5", &[], true),
        m("claude", "claude-fable-5-1", "Claude Fable 5.1", &[], false),
        m("claude", "claude-sonnet-5", "Claude Sonnet 5", &[], false),
        m(
            "opencode",
            "openrouter/deepseek/deepseek-v4-flash-vision-exp",
            "DeepSeek V4 Flash Vision Exp",
            &[],
            true,
        ),
        m(
            "pi",
            "openrouter/deepseek/deepseek-v4-flash-vision-exp",
            "DeepSeek V4 Flash Vision Exp",
            &["off", "minimal", "low", "medium", "high", "xhigh", "max"],
            true,
        ),
        m("droid", "glm-5.3-flash", "GLM-5.3-Flash", &["low", "high", "max"], true),
    ]
}

pub fn parse_model_catalog(json: &str) -> Vec<ModelInfo> {
    let parsed: Vec<ModelInfo> = serde_json::from_str::<Vec<Value>>(json)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .filter(|m: &ModelInfo| !m.available || m.id.is_some())
        .collect();
    if parsed.is_empty() {
        fallback_models()
    } else {
        parsed
    }
}

// --- deja -----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct DejaHit {
    pub provider: String,
    pub session_id: String,
    pub project: String,
    pub date: String,
    pub opening_prompt: String,
}

/// `deja find --json` hits carry a resume command; its id is the thread id.
pub fn parse_deja_hits(json: &str) -> Vec<DejaHit> {
    let Ok(parsed) = serde_json::from_str::<Value>(json) else {
        return vec![];
    };
    let Some(hits) = parsed.get("hits").and_then(Value::as_array) else {
        return vec![];
    };
    hits.iter()
        .filter_map(|hit| {
            let resume = hit.get("resume")?.as_str()?;
            let (provider, id) = if let Some(id) = resume.strip_prefix("claude --resume ") {
                ("claude", id)
            } else {
                ("codex", resume.strip_prefix("codex resume ")?)
            };
            if id.is_empty() || id.contains(char::is_whitespace) {
                return None;
            }
            let text = |key: &str| hit.get(key).and_then(Value::as_str).unwrap_or("").to_string();
            Some(DejaHit {
                provider: provider.into(),
                session_id: id.into(),
                project: text("project"),
                date: text("date"),
                opening_prompt: text("openingPrompt"),
            })
        })
        .collect()
}

// --- deletion -------------------------------------------------------------

pub fn deletable(session: &Session) -> bool {
    is_terminal(&session.status) || session.status == "stale"
}

/// Removes a finished or stale run directory and its registry entries, after
/// re-reading state.json to confirm the directory really holds that run.
pub fn delete_session(session: &Session, registries: &[PathBuf]) -> Result<(), String> {
    if !deletable(session) {
        return Err(format!("Session is {}; stop it before deleting", session.status));
    }
    let dir = absolute(Path::new(&session.state_dir));
    if let Ok(bytes) = fs::read(dir.join("state.json")) {
        let persisted: Session = serde_json::from_slice(&bytes).map_err(|_| "Cannot verify the session state; refresh before deleting")?;
        if absolute(Path::new(&persisted.state_dir)) == dir {
            if !is_terminal(&persisted.status) && process_alive(persisted.pid) {
                return Err(format!("Session is now {}; stop it before deleting", persisted.status));
            }
            fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
        }
    }
    for registry in registries {
        let Ok(entries) = fs::read_dir(registry) else { continue };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().ends_with(".run") {
                continue;
            }
            if let Ok(target) = fs::read_to_string(entry.path()) {
                if absolute(Path::new(target.trim())) == dir {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
    Ok(())
}

// --- palette & motion helpers ---------------------------------------------

/// Scores every query term against label, key, and hint; any miss drops it.
pub fn palette_score(label: &str, key: &str, hint: &str, query: &str) -> Option<u32> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Some(1);
    }
    let label_l = label.to_lowercase();
    let haystack = format!("{label} {key} {hint}").to_lowercase();
    let mut total = 0;
    for term in needle.split_whitespace() {
        total += if label_l.starts_with(term) {
            3
        } else if label_l.contains(term) {
            2
        } else if haystack.contains(term) {
            1
        } else {
            return None;
        };
    }
    Some(total)
}

/// Reveals streamed text in steps; a large backlog drains proportionally.
pub fn typewriter_reveal(revealed: usize, target: usize, step: usize) -> usize {
    if revealed >= target {
        return target;
    }
    target.min(revealed + step.max((target - revealed).div_ceil(8)))
}

/// Backs diff polling off while nothing changes.
pub fn next_diff_poll_ms(previous: u64, changed: bool) -> u64 {
    if changed {
        1_000
    } else {
        (previous * 2).clamp(1_000, 8_000)
    }
}

// --- git diff -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    FileHeader,
    Meta,
    Hunk,
    Add,
    Del,
    Context,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub file: usize,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DiffFile {
    pub path: String,
    pub added: u32,
    pub removed: u32,
}

pub fn parse_git_diff(content: &str) -> (Vec<DiffLine>, Vec<DiffFile>) {
    let mut lines = Vec::new();
    let mut files: Vec<DiffFile> = Vec::new();
    let (mut old, mut new) = (0u32, 0u32);
    for raw in content.lines() {
        let file = files.len().saturating_sub(1);
        let (kind, o, n) = if let Some(rest) = raw.strip_prefix("diff --git ") {
            let path = rest
                .rsplit_once(" b/")
                .map(|(_, b)| b.to_string())
                .unwrap_or_else(|| rest.to_string());
            files.push(DiffFile {
                path,
                ..Default::default()
            });
            lines.push(DiffLine {
                kind: DiffKind::FileHeader,
                text: raw.into(),
                old: None,
                new: None,
                file: files.len() - 1,
            });
            continue;
        } else if raw.starts_with("@@") {
            let mut parts = raw.split_whitespace().skip(1);
            let start = |p: Option<&str>| p.and_then(|p| p[1..].split(',').next()?.parse::<u32>().ok()).unwrap_or(0);
            old = start(parts.next());
            new = start(parts.next());
            (DiffKind::Hunk, None, None)
        } else if files.is_empty()
            || raw.starts_with("+++")
            || raw.starts_with("---")
            || raw.starts_with("index ")
            || raw.starts_with("new file")
            || raw.starts_with("deleted file")
            || raw.starts_with("similarity")
            || raw.starts_with("rename ")
            || raw.starts_with("Binary")
        {
            (DiffKind::Meta, None, None)
        } else if raw.starts_with('+') {
            new += 1;
            files[file].added += 1;
            (DiffKind::Add, None, Some(new - 1))
        } else if raw.starts_with('-') {
            old += 1;
            files[file].removed += 1;
            (DiffKind::Del, Some(old - 1), None)
        } else {
            old += 1;
            new += 1;
            (DiffKind::Context, Some(old - 1), Some(new - 1))
        };
        lines.push(DiffLine {
            kind,
            text: raw.into(),
            old: o,
            new: n,
            file,
        });
    }
    (lines, files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc3339() {
        assert_eq!(parse_time(&Some("1970-01-01T00:00:00Z".into())), Some(0));
        assert_eq!(
            parse_time(&Some("2026-10-02T10:00:00.123456+05:30".into())),
            parse_time(&Some("2026-10-02T04:30:00Z".into()))
        );
    }

    #[test]
    fn transcript_folds_deltas_and_tools() {
        let events = [
            r#"{"method":"item/completed","params":{"threadId":"t","item":{"type":"userMessage","origin":"ruddr","id":"p1","text":"hi"}}}"#,
            r#"{"method":"item/completed","params":{"threadId":"t","item":{"type":"userMessage","content":[{"text":"hi"}]}}}"#,
            r#"{"method":"item/agentMessage/delta","params":{"threadId":"t","itemId":"a","delta":"Hel"}}"#,
            r#"{"method":"item/agentMessage/delta","params":{"threadId":"t","itemId":"a","delta":"lo"}}"#,
            r#"{"method":"item/started","params":{"threadId":"t","item":{"type":"commandExecution","id":"c","command":"ls  -la"}}}"#,
            r#"{"method":"item/completed","params":{"threadId":"t","item":{"type":"commandExecution","id":"c","command":"ls -la","exitCode":1}}}"#,
            r#"{"method":"item/completed","params":{"threadId":"sub","item":{"type":"agentMessage","id":"x","text":"sub agent"}}}"#,
        ]
        .join("\n");
        let entries = parse_chat_transcript(&events, None);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].text, "hi");
        assert_eq!(entries[1].text, "Hello");
        assert_eq!(entries[2].text, "ls -la");
        assert_eq!(entries[2].status, Some(ToolStatus::Failed));
    }

    #[test]
    fn rejected_prompts_drop() {
        let events = [
            r#"{"method":"item/completed","params":{"threadId":"t","item":{"type":"userMessage","origin":"ruddr","id":"p1","text":"no"}}}"#,
            r#"{"method":"ruddr/prompt/rejected","params":{"promptId":"p1"}}"#,
        ]
        .join("\n");
        assert!(parse_chat_transcript(&events, None).is_empty());
    }

    #[test]
    fn routes_never_cross() {
        let mut session = Session {
            status: "active".into(),
            ..Default::default()
        };
        assert_eq!(prompt_route(&session), None);
        session.turn_id = Some("turn".into());
        assert_eq!(prompt_route(&session), Some(PromptRoute::Steer));
        session.status = "completed".into();
        assert_eq!(prompt_route(&session), None);
        session.thread_id = Some("t".into());
        session.cwd = Some("/x".into());
        assert_eq!(prompt_route(&session), Some(PromptRoute::Continue));
        session.status = "stale".into();
        assert_eq!(prompt_route(&session), None);
    }

    #[test]
    fn parses_diff_numbers() {
        let (lines, files) = parse_git_diff("diff --git a/x b/x\nindex 1..2\n--- a/x\n+++ b/x\n@@ -3,2 +3,2 @@\n ctx\n-old\n+new\n");
        assert_eq!(
            files,
            vec![DiffFile {
                path: "x".into(),
                added: 1,
                removed: 1
            }]
        );
        assert_eq!(lines[5].old, Some(3));
        assert_eq!(lines[6].old, Some(4));
        assert_eq!(lines[7].new, Some(4));
    }

    #[test]
    fn deja_hits_need_resume() {
        let hits = parse_deja_hits(r#"{"hits":[{"resume":"claude --resume abc","project":"p"},{"resume":"rm -rf /"}]}"#);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].provider, "claude");
        assert_eq!(hits[0].session_id, "abc");
    }

    #[test]
    fn palette_scoring() {
        assert!(palette_score("New session", "n", "", "new").unwrap() > palette_score("Renew", "", "", "new").unwrap());
        assert_eq!(palette_score("Quit", "q", "", "zzz"), None);
    }

    #[test]
    fn typewriter_catches_up() {
        assert_eq!(typewriter_reveal(0, 10, 24), 10);
        assert_eq!(typewriter_reveal(0, 800, 24), 100);
    }

    #[test]
    fn sorts_live_first() {
        let mut sessions = vec![
            Session {
                status: "completed".into(),
                updated_at: Some("2026-01-02T00:00:00Z".into()),
                ..Default::default()
            },
            Session {
                status: "active".into(),
                updated_at: Some("2026-01-01T00:00:00Z".into()),
                ..Default::default()
            },
        ];
        sort_sessions(&mut sessions);
        assert_eq!(sessions[0].status, "active");
    }
}
