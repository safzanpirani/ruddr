//! Pure session logic the TUI needs on top of `ruddr-core`: presentation
//! helpers, prompt routing, run arguments, the model catalog fallback, deja
//! hits, palette scoring, and git diff parsing. Mirrors tui/core.ts.

use ruddr_core::state::{RunState, Status, TokenUsage};
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub type Session = RunState;

/// Milliseconds since the Unix epoch for an RFC 3339 timestamp.
pub fn parse_time(value: &str) -> Option<i64> {
    ruddr_core::time::parse_rfc3339_ms(value)
}

pub fn now_ms() -> i64 {
    ruddr_core::time::now_ms()
}

pub fn provider(session: &Session) -> &str {
    if session.provider.is_empty() { "codex" } else { &session.provider }
}

pub fn is_live(status: Status) -> bool {
    matches!(status, Status::Active | Status::Idle | Status::Starting)
}

/// Sessions the user can stop: an active turn or an idle controller.
pub fn stoppable(status: Status) -> bool {
    matches!(status, Status::Active | Status::Idle)
}

pub fn opt(value: &str) -> Option<&str> {
    if value.is_empty() { None } else { Some(value) }
}

pub fn filter_sessions<'a>(sessions: &'a [Session], query: &str) -> Vec<&'a Session> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return sessions.iter().collect();
    }
    sessions
        .iter()
        .filter(|session| {
            let project = project_name(session);
            [
                Some(session.status.as_str()),
                Some(provider(session)),
                opt(&session.cwd),
                Some(project.as_str()),
                session.thread_id.as_deref(),
                session.turn_id.as_deref(),
                opt(&session.model),
                session.effort.as_deref(),
                Some(session.state_dir.as_str()),
            ]
            .into_iter()
            .flatten()
            .any(|field| field.to_lowercase().contains(&needle))
        })
        .collect()
}

/// Moves sessions named by `--state-dir` to the front in argument order.
pub fn prioritize_explicit(sessions: &mut [&Session], explicit: &[PathBuf]) {
    if explicit.is_empty() {
        return;
    }
    let rank = |s: &Session| explicit.iter().position(|d| Path::new(&s.state_dir) == d).unwrap_or(usize::MAX);
    sessions.sort_by_key(|s| rank(s));
}

// --- time -----------------------------------------------------------------

pub fn format_age(value: &str, now: i64) -> String {
    let Some(timestamp) = parse_time(value) else {
        return "unknown".into();
    };
    match ((now - timestamp) / 1000).max(0) {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

pub fn format_elapsed(start: &str, end: Option<&str>, now: i64) -> String {
    let Some(start) = parse_time(start) else { return "unknown".into() };
    let end = end.and_then(parse_time).filter(|end| *end >= start).unwrap_or(now);
    match ((end - start) / 1000).max(0) {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

pub fn format_duration(ms: i64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 10_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{:.0}s", ms as f64 / 1000.0)
    }
}

// --- presentation ---------------------------------------------------------

pub fn status_glyph(status: Status) -> &'static str {
    match status {
        Status::Active => "●",
        Status::Idle => "◌",
        Status::Starting | Status::Stopping => "◐",
        Status::Completed => "✓",
        Status::Failed => "×",
        Status::Interrupted => "■",
        Status::Stale => "!",
    }
}

pub fn project_name(session: &Session) -> String {
    let path = if session.cwd.is_empty() {
        Path::new(&session.state_dir).parent().map(Path::to_path_buf).unwrap_or_default()
    } else {
        PathBuf::from(&session.cwd)
    };
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| session.state_dir.clone())
}

pub fn format_token_count(value: i64) -> String {
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

/// Session totals describe billed work, not the current context window.
pub fn format_token_usage(usage: &TokenUsage) -> String {
    let mut parts = Vec::new();
    if usage.total_tokens > 0 {
        parts.push(format!("{} total", format_token_count(usage.total_tokens)));
    }
    if usage.cost_usd > 0.0 {
        parts.push(format!("${:.2}", usage.cost_usd));
    }
    parts.join(" · ")
}

/// The context window fill as a ratio, when both numbers are known.
pub fn context_ratio(usage: &TokenUsage) -> Option<(i64, f64)> {
    let used = usage.context_tokens.filter(|t| *t >= 0)?;
    if usage.context_window <= 0 {
        return None;
    }
    Some((used, (used as f64 / usage.context_window as f64).clamp(0.0, 1.0)))
}

pub fn context_meter(usage: &TokenUsage, cells: usize) -> Option<String> {
    let (used, ratio) = context_ratio(usage)?;
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
    let dash = |v: Option<&str>| v.unwrap_or("—").to_string();
    if let Some(locator) = session.state_dir.strip_prefix(crate::history::PREFIX) {
        return vec![
            (
                "status".into(),
                format!("◇ history, read-only    updated {}", format_age(&session.updated_at, now)),
            ),
            ("provider".into(), provider(session).to_string()),
            ("session".into(), dash(session.thread_id.as_deref())),
            ("cwd".into(), dash(opt(&session.cwd))),
            ("source".into(), locator.to_string()),
        ];
    }
    let mut rows = vec![
        (
            "status".into(),
            format!(
                "{} {}    {}",
                status_glyph(session.status),
                session.status,
                format_elapsed(&session.started_at, session.completed_at.as_deref(), now)
            ),
        ),
        ("provider".into(), provider(session).to_string()),
        (
            "model".into(),
            format!(
                "{}{}",
                dash(opt(&session.model)),
                session.effort.as_deref().map(|e| format!(" / {e}")).unwrap_or_default()
            ),
        ),
        ("thread".into(), dash(session.thread_id.as_deref())),
        ("turn".into(), dash(session.turn_id.as_deref())),
        ("cwd".into(), dash(opt(&session.cwd))),
        (
            "runtime".into(),
            format!(
                "pid {}    steers {}{}",
                session.pid,
                session.steers,
                if session.turns > 0 {
                    format!("    turns {}", session.turns)
                } else {
                    String::new()
                }
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

pub fn events_path(session: &Session) -> PathBuf {
    artifact_path(&session.events_path, session, ruddr_core::state::EVENTS_FILE)
}

pub fn trace_path(session: &Session) -> PathBuf {
    artifact_path(&session.trace_path, session, ruddr_core::state::TRACE_FILE)
}

pub fn output_path(session: &Session) -> PathBuf {
    artifact_path(&session.output_path, session, ruddr_core::state::OUTPUT_FILE)
}

fn artifact_path(recorded: &str, session: &Session, name: &str) -> PathBuf {
    if recorded.is_empty() {
        Path::new(&session.state_dir).join(name)
    } else {
        PathBuf::from(recorded)
    }
}

// --- prompt routing -------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptRoute {
    Steer,
    Prompt,
    Continue,
}

/// Active turns get steered, idle sessions get a new turn over the control
/// channel, finished threads get a continuation run. Never converts routes.
pub fn prompt_route(session: &Session) -> Option<PromptRoute> {
    if crate::history::is_history(&session.state_dir) {
        return None;
    }
    match session.status {
        Status::Active if session.turn_id.is_some() => Some(PromptRoute::Steer),
        Status::Idle => Some(PromptRoute::Prompt),
        status if status.is_terminal() && session.thread_id.is_some() && !session.cwd.is_empty() => Some(PromptRoute::Continue),
        _ => None,
    }
}

/// Checks that fresh state still takes the route the prompt was typed for.
/// A steer also needs the same turn; a turn that ended while typing must not
/// become an idle prompt or a new run.
pub fn revalidate_route(fresh: &Session, route: PromptRoute, observed_turn: Option<&str>) -> Result<(), String> {
    let now = prompt_route(fresh);
    if now != Some(route) {
        return Err(format!("The session is now {}; the prompt was not sent", fresh.status));
    }
    if route == PromptRoute::Steer && fresh.turn_id.as_deref() != observed_turn {
        return Err("The turn changed while you typed; the steer was not sent".into());
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct LaunchOverrides {
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Absolute image paths for the first turn.
    pub images: Vec<String>,
}

pub fn continuation_args(session: &Session, prompt_file: &str, state_dir: &str, overrides: &LaunchOverrides) -> Vec<String> {
    let sandbox = if session.sandbox.is_empty() {
        "workspace-write"
    } else {
        &session.sandbox
    };
    let mut args: Vec<String> = [
        "run",
        "--detach",
        "--provider",
        provider(session),
        "--cwd",
        &session.cwd,
        "--resume-thread",
        session.thread_id.as_deref().unwrap_or_default(),
        "--prompt-file",
        prompt_file,
        "--state-dir",
        state_dir,
        "--sandbox",
        sandbox,
        "--approval-policy",
        "never",
        "--idle",
    ]
    .map(String::from)
    .to_vec();
    if let Some(model) = overrides.model.clone().or_else(|| opt(&session.model).map(String::from)) {
        args.extend(["--model".into(), model]);
    }
    // A different model may not support the old effort, so a model override
    // carries only the effort chosen with it.
    let effort = overrides
        .effort
        .clone()
        .or_else(|| if overrides.model.is_some() { None } else { session.effort.clone() });
    if let Some(effort) = effort {
        args.extend(["--effort".into(), effort]);
    }
    push_images(&mut args, overrides);
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
    push_images(&mut args, overrides);
    args
}

fn push_images(args: &mut Vec<String>, overrides: &LaunchOverrides) {
    for image in &overrides.images {
        args.extend(["--image".into(), image.clone()]);
    }
}

/// The image files a paste names, when the whole paste is image paths: what
/// a terminal sends when files are dropped on it. Paths may be quoted,
/// backslash-escaped on Unix, or `file://` URLs. Windows paths keep their
/// backslashes, including inside double quotes.
pub fn pasted_image_paths(text: &str) -> Option<Vec<PathBuf>> {
    let mut words = vec![];
    let mut word = String::new();
    let (mut quote, mut started) = (None, false);
    let mut chars = text.trim().chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, '"') => (quote, started) = (Some(c), true),
            (None, '\'') if !cfg!(windows) => (quote, started) = (Some(c), true),
            (Some(q), c) if c == q => quote = None,
            (None | Some('"'), '\\') if !cfg!(windows) => {
                word.extend(chars.next());
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (_, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        words.push(word);
    }
    let paths: Vec<PathBuf> = words
        .into_iter()
        .map(|w| match w.strip_prefix("file://") {
            Some(url) => PathBuf::from(percent_decode(url)),
            None => PathBuf::from(w),
        })
        .collect();
    let all_images = paths
        .iter()
        .all(|p| p.is_absolute() && ruddr_core::images::has_image_extension(p) && p.is_file());
    (!paths.is_empty() && all_images).then_some(paths)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
        match (bytes[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The argument of a `/cd` draft in the new-session prompt: the whole draft
/// is one line that is `/cd` or starts with `/cd `.
pub fn cd_argument(draft: &str) -> Option<&str> {
    let draft = draft.trim();
    if draft.contains('\n') {
        return None;
    }
    match draft.strip_prefix("/cd") {
        Some("") => Some(""),
        Some(rest) if rest.starts_with(char::is_whitespace) => Some(rest.trim()),
        _ => None,
    }
}

/// Resolves a `/cd` argument to an existing directory. An empty argument
/// returns `start`, the directory the TUI was launched from; `~` expands to
/// `home`; a relative path joins `current`.
pub fn resolve_cd(arg: &str, current: &Path, start: &Path, home: &Path) -> Result<PathBuf, String> {
    let path = match arg {
        "" => start.to_path_buf(),
        "~" => home.to_path_buf(),
        _ => match arg.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => current.join(arg),
        },
    };
    let path = ruddr_core::paths::normalize(&path);
    if path.is_dir() {
        Ok(path)
    } else {
        Err(format!("{arg} is not a directory"))
    }
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

/// The built-in catalog, for when `ruddr models --json` is unavailable.
pub fn fallback_models() -> Vec<ModelInfo> {
    ruddr_core::models::builtin_catalog()
        .into_iter()
        .map(|m| ModelInfo {
            provider: m.provider,
            id: Some(m.id),
            label: Some(m.label).filter(|label| !label.is_empty()),
            efforts: m.efforts,
            default: m.default,
            available: m.available,
            note: Some(m.note).filter(|note| !note.is_empty()),
        })
        .collect()
}

pub fn parse_model_catalog(json: &str) -> Vec<ModelInfo> {
    let parsed: Vec<ModelInfo> = serde_json::from_str::<Vec<Value>>(json)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .filter(|m: &ModelInfo| !m.available || m.id.is_some())
        .collect();
    if parsed.is_empty() { fallback_models() } else { parsed }
}

// --- deja -----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct DejaHit {
    pub provider: String,
    pub session_id: String,
    pub project: String,
    pub date: String,
    pub opening_prompt: String,
    pub locator: String,
    pub excerpt: String,
}

/// Read structured metadata first. Resume commands are never executed as shell text.
pub fn parse_deja_hit(hit: &Value) -> Option<DejaHit> {
    let text = |key: &str| hit.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let resume = text("resume");
    let prefixes = [
        ("claude --resume ", "claude"),
        ("codex resume ", "codex"),
        ("droid --resume ", "droid"),
        ("pi --session ", "pi"),
        ("opencode2 -s ", "opencode"),
    ];
    let parsed = prefixes
        .iter()
        .find_map(|(prefix, provider)| resume.strip_prefix(prefix).map(|id| (*provider, id)));
    let source = text("source");
    let provider = if source.is_empty() { parsed?.0.to_string() } else { source };
    if !ruddr_history::Provider::ALL.iter().any(|p| p.name() == provider) {
        return None;
    }
    let session_id = parsed
        .filter(|(p, id)| *p == provider && !id.is_empty() && (*p == "pi" || !id.contains(char::is_whitespace)))
        .map(|(_, id)| id.to_string())
        .unwrap_or_default();
    let locator = text("path");
    if session_id.is_empty() && locator.is_empty() {
        return None;
    }
    let excerpt = hit
        .get("matches")
        .and_then(Value::as_array)
        .map(|matches| {
            matches
                .iter()
                .filter_map(|m| m.get("text").and_then(Value::as_str))
                .take(3)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| text("openingPrompt"));
    Some(DejaHit {
        provider,
        session_id,
        locator,
        excerpt,
        project: text("project"),
        date: text("date"),
        opening_prompt: text("openingPrompt"),
    })
}

// --- deletion -------------------------------------------------------------

/// Another agent's history is read-only: never Ruddr's to delete.
pub fn deletable(session: &Session) -> bool {
    !crate::history::is_history(&session.state_dir) && (session.status.is_terminal() || session.status == Status::Stale)
}

/// Removes a finished or stale run directory and its registry entries. The
/// directory goes only when its state.json confirms it holds that run and
/// the run is not live again.
pub fn delete_session(session: &Session) -> Result<(), String> {
    if !deletable(session) {
        return Err(format!("Session is {}; stop it before deleting", session.status));
    }
    let dir = ruddr_core::paths::absolute(Path::new(&session.state_dir));
    if dir.join(ruddr_core::state::STATE_FILE).exists()
        && let Ok(persisted) = ruddr_core::state::read_state(&dir)
    {
        if !persisted.status.is_terminal() && ruddr_core::process::alive(persisted.pid) {
            return Err(format!("Session is now {}; stop it before deleting", persisted.status));
        }
        std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    ruddr_core::registry::unregister(&dir, &ruddr_core::paths::registry_dirs_for_discovery());
    Ok(())
}

// --- palette & polling helpers --------------------------------------------

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

/// Backs diff polling off while nothing changes.
pub fn next_diff_poll_ms(previous: u64, changed: bool) -> u64 {
    if changed { 1_000 } else { (previous * 2).clamp(1_000, 8_000) }
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
    /// M, A, D, or R.
    pub status: char,
}

/// `diff --git a/x b/y` names the post-image path `y`.
pub fn diff_file_path(rest: &str) -> String {
    let rest = rest.trim_matches('"');
    match rest.rfind(" b/").or_else(|| rest.rfind(" \"b/")) {
        Some(index) => {
            let tail = rest[index..].trim_start_matches([' ', '"']);
            tail.strip_prefix("b/").unwrap_or(tail).trim_end_matches('"').to_string()
        }
        None => rest.to_string(),
    }
}

pub fn parse_git_diff(content: &str) -> (Vec<DiffLine>, Vec<DiffFile>) {
    let mut lines = Vec::new();
    let mut files: Vec<DiffFile> = Vec::new();
    let (mut old, mut new) = (0u32, 0u32);
    let mut in_hunk = false;
    for raw in content.lines() {
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            files.push(DiffFile {
                path: diff_file_path(rest),
                status: 'M',
                ..Default::default()
            });
            in_hunk = false;
            lines.push(DiffLine {
                kind: DiffKind::FileHeader,
                text: raw.into(),
                old: None,
                new: None,
                file: files.len() - 1,
            });
            continue;
        }
        let file = files.len().saturating_sub(1);
        let (kind, o, n) = if raw.starts_with("@@") {
            let mut parts = raw.split_whitespace().skip(1);
            let start = |p: Option<&str>| p.and_then(|p| p.get(1..)?.split(',').next()?.parse::<u32>().ok());
            let (a, b) = (start(parts.next()), start(parts.next()));
            in_hunk = a.is_some() && b.is_some();
            old = a.unwrap_or(0);
            new = b.unwrap_or(0);
            (DiffKind::Hunk, None, None)
        } else if files.is_empty() || is_diff_meta(raw) {
            if let Some(f) = files.last_mut() {
                if raw.starts_with("new file mode ") {
                    f.status = 'A';
                } else if raw.starts_with("deleted file mode ") {
                    f.status = 'D';
                } else if raw.starts_with("rename from ") {
                    f.status = 'R';
                }
            }
            (DiffKind::Meta, None, None)
        } else if raw.starts_with('+') {
            files[file].added += 1;
            let n = in_hunk.then(|| {
                new += 1;
                new - 1
            });
            (DiffKind::Add, None, n)
        } else if raw.starts_with('-') {
            files[file].removed += 1;
            let o = in_hunk.then(|| {
                old += 1;
                old - 1
            });
            (DiffKind::Del, o, None)
        } else if in_hunk {
            old += 1;
            new += 1;
            (DiffKind::Context, Some(old - 1), Some(new - 1))
        } else {
            (DiffKind::Context, None, None)
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

/// One row of the changed-file tree.
#[derive(Debug, Clone, PartialEq)]
pub enum TreeEntry {
    Dir {
        path: String,
        name: String,
        depth: usize,
        expanded: bool,
    },
    File {
        index: usize,
        name: String,
        depth: usize,
    },
}

/// The changed files as a directory tree in diff order. A collapsed
/// directory hides everything below it.
pub fn diff_tree(files: &[DiffFile], collapsed: &std::collections::HashSet<String>) -> Vec<TreeEntry> {
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (index, file) in files.iter().enumerate() {
        let parts: Vec<&str> = file.path.split('/').collect();
        let mut hidden = false;
        for depth in 0..parts.len() - 1 {
            let path = parts[..=depth].join("/");
            if seen.insert(path.clone()) {
                let expanded = !collapsed.contains(&path);
                entries.push(TreeEntry::Dir {
                    path: path.clone(),
                    name: parts[depth].to_string(),
                    depth,
                    expanded,
                });
            }
            if collapsed.contains(&path) {
                hidden = true;
                break;
            }
        }
        if !hidden {
            entries.push(TreeEntry::File {
                index,
                name: parts[parts.len() - 1].to_string(),
                depth: parts.len() - 1,
            });
        }
    }
    entries
}

fn is_diff_meta(raw: &str) -> bool {
    raw.starts_with("+++")
        || raw.starts_with("---")
        || raw.starts_with("index ")
        || raw.starts_with("new file mode ")
        || raw.starts_with("deleted file mode ")
        || raw.starts_with("old mode ")
        || raw.starts_with("new mode ")
        || raw.starts_with("similarity index ")
        || raw.starts_with("rename from ")
        || raw.starts_with("rename to ")
        || raw.starts_with("Binary files ")
        || raw == "\\ No newline at end of file"
}

#[cfg(test)]
pub mod tests_support {
    use super::*;
    use ruddr_core::state::STATE_VERSION;

    pub fn session(status: Status) -> Session {
        RunState {
            version: STATE_VERSION,
            provider: "codex".into(),
            pid: 0,
            child_pid: 0,
            status,
            thread_id: None,
            turn_id: None,
            model: String::new(),
            effort: None,
            cwd: String::new(),
            sandbox: String::new(),
            state_dir: "/w/.scratch/run".into(),
            socket_path: String::new(),
            socket_dir: None,
            events_path: String::new(),
            trace_path: String::new(),
            output_path: String::new(),
            stderr_path: String::new(),
            steers: 0,
            idle: false,
            turns: 0,
            last_turn: None,
            token_usage: None,
            started_at: String::new(),
            updated_at: String::new(),
            completed_at: None,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::session;
    use super::*;

    #[test]
    fn dropped_image_files_paste_as_attachments() {
        let root = std::env::temp_dir().join(format!("ruddr-tui-drop-{}", std::process::id()));
        std::fs::create_dir_all(root.join("my shots")).unwrap();
        let (a, b) = (root.join("my shots").join("a b.png"), root.join("c.JPG"));
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();
        std::fs::write(root.join("notes.txt"), b"x").unwrap();
        let (a_s, b_s) = (a.display().to_string(), b.display().to_string());
        #[cfg(unix)]
        {
            assert_eq!(pasted_image_paths(&format!("'{a_s}' {b_s}\n")), Some(vec![a.clone(), b.clone()]));
            assert_eq!(pasted_image_paths(&a_s.replace(' ', "\\ ")), Some(vec![a.clone()]));
        }
        #[cfg(windows)]
        {
            // Windows Terminal quotes paths containing spaces and leaves
            // directory separators intact. The temp directory may have spaces.
            let quote = |s: &str| if s.contains(' ') { format!("\"{s}\"") } else { s.to_owned() };
            assert_eq!(
                pasted_image_paths(&format!("{} {}\r\n", quote(&a_s), quote(&b_s))),
                Some(vec![a.clone(), b.clone()])
            );
            assert_eq!(pasted_image_paths(&quote(&b_s)), Some(vec![b.clone()]));
            let apostrophe = root.join("O'Brien.png");
            std::fs::write(&apostrophe, b"x").unwrap();
            assert_eq!(
                pasted_image_paths(&quote(&apostrophe.display().to_string())),
                Some(vec![apostrophe])
            );
        }
        assert_eq!(pasted_image_paths(&format!("\"{a_s}\"")), Some(vec![a.clone()]));
        assert_eq!(
            pasted_image_paths(&format!("file://{}", a_s.replace(' ', "%20"))),
            Some(vec![a.clone()])
        );
        assert_eq!(pasted_image_paths(&format!("look at {b_s}")), None, "prose stays text");
        assert_eq!(pasted_image_paths(&root.join("notes.txt").display().to_string()), None);
        assert_eq!(pasted_image_paths(&root.join("gone.png").display().to_string()), None);
        assert_eq!(pasted_image_paths("c.JPG"), None, "relative paths stay text");
        assert_eq!(pasted_image_paths(""), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn launches_attach_images_with_the_image_flag() {
        let overrides = LaunchOverrides {
            images: vec!["/a.png".into(), "/b.png".into()],
            ..Default::default()
        };
        let args = new_session_args("codex", "/w", "/p", "/d", &overrides, None).join(" ");
        assert!(args.ends_with("--image /a.png --image /b.png"), "{args}");
        let mut s = session(Status::Completed);
        s.thread_id = Some("t".into());
        s.cwd = "/w".into();
        assert!(
            continuation_args(&s, "/p", "/d", &overrides)
                .join(" ")
                .ends_with("--image /a.png --image /b.png")
        );
    }

    #[test]
    fn cd_drafts_are_single_line_commands() {
        assert_eq!(cd_argument("/cd ../api"), Some("../api"));
        assert_eq!(cd_argument("  /cd   ~/work  "), Some("~/work"));
        assert_eq!(cd_argument("/cd"), Some(""));
        assert_eq!(cd_argument("/cdx"), None);
        assert_eq!(cd_argument("/cd x\nfix the bug"), None, "a multi-line draft is a prompt");
        assert_eq!(cd_argument("please /cd x"), None);
    }

    #[test]
    fn cd_resolves_relative_home_and_reset_paths() {
        let root = std::env::temp_dir().join(format!("ruddr-tui-cd-{}", std::process::id()));
        let (start, other, home) = (root.join("start"), root.join("other"), root.join("home"));
        for dir in [&start, &other, &home.join("repo")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        assert_eq!(resolve_cd("../other", &start, &start, &home).unwrap(), other);
        assert_eq!(resolve_cd(other.to_str().unwrap(), &start, &start, &home).unwrap(), other);
        assert_eq!(resolve_cd("~/repo", &other, &start, &home).unwrap(), home.join("repo"));
        assert_eq!(resolve_cd("~", &other, &start, &home).unwrap(), home);
        assert_eq!(
            resolve_cd("", &other, &start, &home).unwrap(),
            start,
            "a bare /cd returns to the launch directory"
        );
        let missing = resolve_cd("nope", &start, &start, &home).unwrap_err();
        assert_eq!(missing, "nope is not a directory");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn routes_never_cross() {
        let mut s = session(Status::Active);
        assert_eq!(prompt_route(&s), None, "an active run without a turn cannot be steered");
        s.turn_id = Some("turn".into());
        assert_eq!(prompt_route(&s), Some(PromptRoute::Steer));
        s.status = Status::Idle;
        assert_eq!(prompt_route(&s), Some(PromptRoute::Prompt));
        s.status = Status::Completed;
        assert_eq!(prompt_route(&s), None);
        s.thread_id = Some("t".into());
        s.cwd = "/x".into();
        assert_eq!(prompt_route(&s), Some(PromptRoute::Continue));
        s.status = Status::Interrupted;
        assert_eq!(prompt_route(&s), Some(PromptRoute::Continue), "every terminal status continues");
        s.status = Status::Stale;
        assert_eq!(prompt_route(&s), None);
    }

    #[test]
    fn revalidation_refuses_changed_routes_and_turns() {
        let mut fresh = session(Status::Active);
        fresh.turn_id = Some("t2".into());
        assert!(revalidate_route(&fresh, PromptRoute::Steer, Some("t2")).is_ok());
        assert!(
            revalidate_route(&fresh, PromptRoute::Steer, Some("t1")).is_err(),
            "a new turn is not the observed turn"
        );
        fresh.status = Status::Idle;
        let error = revalidate_route(&fresh, PromptRoute::Steer, Some("t2")).unwrap_err();
        assert!(error.contains("idle"), "{error}");
        assert!(revalidate_route(&fresh, PromptRoute::Prompt, None).is_ok());
        fresh.status = Status::Completed;
        assert!(
            revalidate_route(&fresh, PromptRoute::Prompt, None).is_err(),
            "an idle prompt never becomes a continuation"
        );
    }

    #[test]
    fn continues_with_thread_cwd_and_model_settings() {
        let mut s = session(Status::Completed);
        s.thread_id = Some("thread-1".into());
        s.cwd = "/work".into();
        s.model = "gpt-6-sol".into();
        s.effort = Some("high".into());
        s.sandbox = "read-only".into();
        let args = continuation_args(&s, "/p", "/d", &LaunchOverrides::default());
        let joined = args.join(" ");
        assert!(
            joined.starts_with("run --detach --provider codex --cwd /work --resume-thread thread-1"),
            "{joined}"
        );
        assert!(joined.contains("--sandbox read-only --approval-policy never --idle"), "{joined}");
        assert!(joined.ends_with("--model gpt-6-sol --effort high"), "{joined}");
        let switched = continuation_args(
            &s,
            "/p",
            "/d",
            &LaunchOverrides {
                model: Some("gpt-6-luna".into()),
                effort: None,
                ..Default::default()
            },
        );
        assert!(
            switched.join(" ").ends_with("--model gpt-6-luna"),
            "a new model drops the old effort"
        );
        s.provider = "claude".into();
        s.model.clear();
        s.effort = None;
        let claude = continuation_args(&s, "/p", "/d", &LaunchOverrides::default());
        assert!(!claude.contains(&"--model".to_string()), "no invented model");
    }

    #[test]
    fn new_sessions_always_run_idle_and_detached() {
        let args = new_session_args(
            "pi",
            "/w",
            "/p",
            "/d",
            &LaunchOverrides {
                model: Some("m".into()),
                effort: Some("low".into()),
                ..Default::default()
            },
            Some("t"),
        );
        assert_eq!(&args[..2], ["run", "--detach"]);
        assert!(args.contains(&"--idle".to_string()));
        assert!(args.join(" ").ends_with("--resume-thread t --model m --effort low"));
    }

    #[test]
    fn filters_across_project_ids_status_and_model() {
        let mut a = session(Status::Completed);
        a.cwd = "/work/parser".into();
        a.model = "gpt-parser".into();
        let mut b = session(Status::Active);
        b.state_dir = "/active".into();
        b.cwd = "/work/payments".into();
        b.thread_id = Some("thread-pay".into());
        b.effort = Some("xhigh".into());
        let runs = vec![a, b];
        assert_eq!(filter_sessions(&runs, "payments").len(), 1);
        assert_eq!(filter_sessions(&runs, "completed").len(), 1);
        assert_eq!(filter_sessions(&runs, "PARSER").len(), 1);
        assert_eq!(filter_sessions(&runs, "thread-pay").len(), 1);
        assert_eq!(filter_sessions(&runs, "xhigh").len(), 1);
        assert_eq!(filter_sessions(&runs, "  ").len(), 2);
    }

    #[test]
    fn explicit_sessions_lead_in_argument_order() {
        let mut runs: Vec<Session> = ["/a", "/b", "/c"]
            .iter()
            .map(|d| Session {
                state_dir: d.to_string(),
                ..session(Status::Completed)
            })
            .collect();
        runs[0].status = Status::Active;
        let mut refs: Vec<&Session> = runs.iter().collect();
        prioritize_explicit(&mut refs, &[PathBuf::from("/c"), PathBuf::from("/b")]);
        let order: Vec<&str> = refs.iter().map(|s| s.state_dir.as_str()).collect();
        assert_eq!(order, ["/c", "/b", "/a"]);
    }

    #[test]
    fn labels_usage_without_treating_totals_as_context() {
        let usage = TokenUsage {
            total_tokens: 1_234_567,
            cost_usd: 0.5,
            ..Default::default()
        };
        assert_eq!(format_token_usage(&usage), "1.2M total · $0.50");
        assert_eq!(context_meter(&usage, 8), None);
        let usage = TokenUsage {
            context_tokens: Some(50_000),
            context_window: 200_000,
            ..Default::default()
        };
        assert_eq!(context_meter(&usage, 8).unwrap(), "▰▰▱▱▱▱▱▱ 50.0K · 25%");
    }

    #[test]
    fn formats_time() {
        let start = "2026-10-02T10:00:00Z";
        let now = parse_time("2026-10-02T11:30:05Z").unwrap();
        assert_eq!(format_elapsed(start, None, now), "1h 30m");
        assert_eq!(format_elapsed(start, Some("2026-10-02T10:00:42Z"), now), "42s");
        assert_eq!(format_age(start, now), "1h ago");
        assert_eq!(format_age("", now), "unknown");
        assert_eq!(format_duration(1250), "1.2s");
        assert_eq!(format_duration(250), "250ms");
    }

    #[test]
    fn parses_diff_numbers_status_and_meta() {
        let (lines, files) = parse_git_diff(
            "diff --git a/x b/x\nindex 1..2\n--- a/x\n+++ b/x\n@@ -3,2 +3,2 @@\n ctx\n-old\n+new\n\\ No newline at end of file\ndiff --git a/n b/n\nnew file mode 100644\n@@ -0,0 +1 @@\n+hi\n",
        );
        assert_eq!(
            files[0],
            DiffFile {
                path: "x".into(),
                added: 1,
                removed: 1,
                status: 'M'
            }
        );
        assert_eq!(files[1].status, 'A');
        assert_eq!(lines[5].old, Some(3));
        assert_eq!(lines[6].old, Some(4));
        assert_eq!(lines[7].new, Some(4));
        assert_eq!(lines[8].kind, DiffKind::Meta, "no-newline markers do not advance line numbers");
        assert_eq!(lines.last().unwrap().new, Some(1));
        assert_eq!(diff_file_path("a/dir/old b/dir/new"), "dir/new");
    }

    #[test]
    fn builds_a_collapsible_file_tree() {
        let (_, files) = parse_git_diff(
            "diff --git a/src/api.ts b/src/api.ts\n@@ -1 +1,2 @@\n-old\n+new\n+next\ndiff --git a/src/ui/view.ts b/src/ui/view.ts\n@@ -1 +1 @@\n-before\n+after\ndiff --git a/README.md b/README.md\n+docs\n",
        );
        let dir = |path: &str, name: &str, depth: usize, expanded: bool| TreeEntry::Dir {
            path: path.into(),
            name: name.into(),
            depth,
            expanded,
        };
        let file = |index: usize, name: &str, depth: usize| TreeEntry::File {
            index,
            name: name.into(),
            depth,
        };
        assert_eq!(
            diff_tree(&files, &Default::default()),
            vec![
                dir("src", "src", 0, true),
                file(0, "api.ts", 1),
                dir("src/ui", "ui", 1, true),
                file(1, "view.ts", 2),
                file(2, "README.md", 0)
            ]
        );
        assert_eq!((files[0].added, files[0].removed, files[2].added), (2, 1, 1));
        let collapsed = std::collections::HashSet::from(["src".to_string()]);
        assert_eq!(
            diff_tree(&files, &collapsed),
            vec![dir("src", "src", 0, false), file(2, "README.md", 0)]
        );
    }

    #[test]
    fn deja_hits_need_resume() {
        let values: Value = serde_json::from_str(r#"[{"resume":"claude --resume abc","project":"p"},{"resume":"rm -rf /"}]"#).unwrap();
        let hits: Vec<_> = values.as_array().unwrap().iter().filter_map(parse_deja_hit).collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].provider, "claude");
        assert_eq!(hits[0].session_id, "abc");
    }

    #[test]
    fn model_catalog_falls_back_on_bad_json() {
        assert_eq!(parse_model_catalog("not json"), fallback_models());
        let parsed =
            parse_model_catalog(r#"[{"provider":"opencode","available":false,"note":"not installed"},{"provider":"pi","id":"x"}]"#);
        assert_eq!(parsed.len(), 2);
        assert!(!parsed[0].available);
        let sol = fallback_models()
            .into_iter()
            .find(|m| m.id.as_deref() == Some("gpt-6-sol"))
            .unwrap();
        assert_eq!(sol.efforts.last().map(String::as_str), Some("ultra"));
    }

    #[test]
    fn palette_scoring() {
        assert!(palette_score("New session", "n", "", "new").unwrap() > palette_score("Renew", "", "", "new").unwrap());
        assert_eq!(palette_score("Quit", "q", "", "zzz"), None);
    }

    #[test]
    fn diff_polling_backs_off() {
        assert_eq!(next_diff_poll_ms(1000, false), 2000);
        assert_eq!(next_diff_poll_ms(8000, false), 8000);
        assert_eq!(next_diff_poll_ms(8000, true), 1000);
    }

    #[test]
    fn deletes_only_finished_state_and_its_registry_entries() {
        let root = std::env::temp_dir().join(format!("ruddr-tui-delete-{}", ruddr_core::fsutil::random_hex(4)));
        let dir = root.join("run");
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = session(Status::Completed);
        s.state_dir = dir.to_string_lossy().into_owned();
        ruddr_core::state::persist_state(&s).unwrap();
        let mut live = s.clone();
        live.status = Status::Active;
        assert!(delete_session(&live).is_err(), "live sessions are refused");
        // A stale view of a run that came back to life is refused too.
        let mut revived = s.clone();
        revived.status = Status::Active;
        revived.pid = std::process::id() as i64;
        ruddr_core::state::persist_state(&revived).unwrap();
        let mut stale = s.clone();
        stale.status = Status::Stale;
        assert!(delete_session(&stale).unwrap_err().contains("now active"));
        ruddr_core::state::persist_state(&s).unwrap();
        delete_session(&s).unwrap();
        assert!(!dir.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
