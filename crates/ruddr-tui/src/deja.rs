//! Dejavu browser state, bounded background requests, and JSON contracts.
//! One request runs at a time. Edits replace the pending search; generations
//! discard results from older inputs and closed panels without spawning a backlog.

use crate::actions;
use crate::core::{DejaHit, parse_deja_hit};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

pub const DEBOUNCE: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub provider: String,
    pub id: String,
    pub locator: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Find(String),
    Memories(String),
    Memory(String),
    Last,
    Query(Target, String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Page {
    Sessions,
    Memories,
    Last,
    Question(Target),
    Confirm(Target, String),
    Text,
}

#[derive(Clone, Debug)]
pub struct MemoryHit {
    pub project: String,
    pub name: String,
    pub path: String,
    pub excerpt: String,
}

#[derive(Debug)]
pub enum Reply {
    Sessions(Vec<DejaHit>),
    Memories(Vec<MemoryHit>),
    Last(Option<DejaHit>),
    Text(String),
}

pub struct Response {
    pub generation: u64,
    pub result: Result<Reply, String>,
}

pub enum Effect {
    Open(DejaHit),
    Resume(DejaHit),
}

enum Click {
    Result(usize),
    Key(KeyEvent),
}

pub struct Browser {
    pub visible: bool,
    pub page: Page,
    pub input: String,
    pub sessions: Vec<DejaHit>,
    pub memories: Vec<MemoryHit>,
    pub selected: usize,
    pub status: String,
    pub text: String,
    pub scroll: u16,
    pub generation: u64,
    hits: Vec<(ratatui::layout::Rect, Click)>,
    running: Option<u64>,
    pending: Option<(Instant, Request)>,
}

impl Default for Browser {
    fn default() -> Self {
        Self {
            visible: false,
            page: Page::Sessions,
            input: String::new(),
            sessions: vec![],
            memories: vec![],
            selected: 0,
            status: String::new(),
            text: String::new(),
            scroll: 0,
            generation: 0,
            hits: vec![],
            running: None,
            pending: None,
        }
    }
}

impl Browser {
    pub fn open(&mut self, page: Page, now: Instant) {
        self.close();
        self.visible = true;
        self.page = page;
        self.input.clear();
        self.text.clear();
        self.scroll = 0;
        self.status = match self.page {
            Page::Sessions | Page::Memories => "Type to search".into(),
            Page::Question(_) => "Enter reviews the question before any paid call".into(),
            _ => String::new(),
        };
        if self.page == Page::Last {
            self.schedule(Request::Last, now);
        }
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.generation += 1;
        self.pending = None;
        self.sessions.clear();
        self.memories.clear();
        self.selected = 0;
        self.hits.clear();
    }

    fn schedule(&mut self, request: Request, at: Instant) {
        self.generation += 1;
        self.pending = Some((at, request));
        self.status = "Waiting…".into();
    }

    pub fn edit(&mut self, now: Instant) {
        self.hits.clear();
        self.sessions.clear();
        self.memories.clear();
        self.selected = 0;
        self.generation += 1;
        self.pending = None;
        if self.input.trim().is_empty() {
            self.status = "Type to search".into();
        } else {
            let request = match self.page {
                Page::Sessions => Request::Find(self.input.clone()),
                Page::Memories => Request::Memories(self.input.clone()),
                _ => return,
            };
            self.schedule(request, now + DEBOUNCE);
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        if self.running.is_some() {
            None
        } else {
            self.pending.as_ref().map(|(at, _)| *at)
        }
    }

    pub fn take_request(&mut self, now: Instant) -> Option<(u64, Request)> {
        if !self.visible || self.running.is_some() || self.deadline().is_none_or(|at| now < at) {
            return None;
        }
        let (_, request) = self.pending.take()?;
        self.running = Some(self.generation);
        self.status = if matches!(request, Request::Query(..)) {
            "Query running · Esc closes view; paid call continues".into()
        } else {
            "Searching…".into()
        };
        Some((self.generation, request))
    }

    pub fn receive(&mut self, response: Response) -> Option<Effect> {
        if self.running == Some(response.generation) {
            self.running = None;
        }
        if !self.visible || response.generation != self.generation {
            return None;
        }
        self.status.clear();
        match response.result {
            Err(error) => self.status = error,
            Ok(Reply::Sessions(hits)) => {
                self.status = format!("{} ranked sessions", hits.len());
                self.sessions = hits;
            }
            Ok(Reply::Memories(hits)) => {
                self.status = format!("{} memory files", hits.len());
                self.memories = hits;
            }
            Ok(Reply::Last(Some(hit))) => {
                self.close();
                return Some(Effect::Open(hit));
            }
            Ok(Reply::Last(None)) => self.status = "No previous session in this repo/cwd".into(),
            Ok(Reply::Text(text)) => {
                self.text = text;
                self.scroll = 0;
                self.page = Page::Text;
            }
        }
        None
    }

    pub fn paste(&mut self, text: &str, now: Instant) {
        if matches!(self.page, Page::Sessions | Page::Memories | Page::Question(_)) {
            self.input.extend(text.chars().filter_map(|c| {
                if c.is_whitespace() {
                    Some(' ')
                } else if c.is_control() {
                    None
                } else {
                    Some(c)
                }
            }));
            if !matches!(self.page, Page::Question(_)) {
                self.edit(now);
            }
        }
    }

    pub fn move_by(&mut self, delta: isize) {
        if self.page == Page::Text {
            self.scroll = (self.scroll as isize + delta).clamp(0, u16::MAX as isize) as u16;
        } else {
            let len = if self.page == Page::Memories {
                self.memories.len()
            } else {
                self.sessions.len()
            };
            self.selected = (self.selected as isize + delta).clamp(0, len.saturating_sub(1) as isize) as usize;
        }
    }

    pub fn mouse(&mut self, mouse: MouseEvent, now: Instant) -> Option<Effect> {
        match mouse.kind {
            MouseEventKind::ScrollDown => self.move_by(1),
            MouseEventKind::ScrollUp => self.move_by(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, click)) = self.hits.iter().find(|(r, _)| r.contains((mouse.column, mouse.row).into())) {
                    let key = match click {
                        Click::Result(index) => {
                            self.selected = *index;
                            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
                        }
                        Click::Key(key) => *key,
                    };
                    return self.key(key, now);
                }
            }
            _ => {}
        }
        None
    }

    pub fn key(&mut self, key: KeyEvent, now: Instant) -> Option<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.close(),
            KeyCode::Char('n') if matches!(self.page, Page::Confirm(..)) => self.close(),
            // Only an explicit y authorizes a call. Repeated Enter cannot pay.
            KeyCode::Char('y') if !ctrl && matches!(self.page, Page::Confirm(..)) => {
                if let Page::Confirm(target, question) = self.page.clone() {
                    self.page = Page::Text;
                    self.text = format!("Question: {question}");
                    self.schedule(Request::Query(target, question), now);
                }
            }
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down | KeyCode::Tab => self.move_by(1),
            KeyCode::PageUp => self.move_by(-8),
            KeyCode::PageDown => self.move_by(8),
            KeyCode::Home if self.page == Page::Text => self.scroll = 0,
            KeyCode::Enter => match self.page.clone() {
                Page::Sessions => {
                    if let Some(hit) = self.sessions.get(self.selected).cloned() {
                        self.close();
                        return Some(Effect::Open(hit));
                    }
                    if let Some((at, _)) = &mut self.pending {
                        *at = now;
                    }
                }
                Page::Memories => {
                    if let Some(hit) = self.memories.get(self.selected) {
                        let request = Request::Memory(hit.path.clone());
                        self.page = Page::Text;
                        self.text = format!("{} / {}", hit.project, hit.name);
                        self.schedule(request, now);
                    }
                }
                Page::Question(target) if !self.input.trim().is_empty() => {
                    self.page = Page::Confirm(target, self.input.trim().to_string());
                    self.status = "Paid model call · price depends on dejavu's model and session size".into();
                }
                _ => {}
            },
            KeyCode::Char('r') if ctrl && self.page == Page::Sessions => {
                if let Some(hit) = self.sessions.get(self.selected).cloned() {
                    if hit.session_id.is_empty() {
                        self.status = "deja supplied no resume target for this session".into();
                    } else {
                        self.close();
                        return Some(Effect::Resume(hit));
                    }
                }
            }
            _ if matches!(self.page, Page::Sessions | Page::Memories | Page::Question(_)) => {
                match key.code {
                    KeyCode::Backspace => {
                        self.input.pop();
                    }
                    KeyCode::Char('u') if ctrl => self.input.clear(),
                    KeyCode::Char(c) if !ctrl && !c.is_control() => self.input.push(c),
                    _ => return None,
                }
                if !matches!(self.page, Page::Question(_)) {
                    self.edit(now);
                }
            }
            _ => {}
        }
        None
    }
}

fn array<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>, String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("deja JSON is missing {key}"))
}

pub fn parse_reply(request: &Request, output: &str) -> Result<Reply, String> {
    let value: Value = serde_json::from_str(output).map_err(|e| format!("Invalid deja JSON: {e}"))?;
    match request {
        Request::Find(_) => Ok(Reply::Sessions(array(&value, "hits")?.iter().filter_map(parse_deja_hit).collect())),
        Request::Last => {
            let cards = array(&value, "sessions")?;
            let hit = cards
                .first()
                .map(|v| parse_deja_hit(v).ok_or("Unsupported deja session card"))
                .transpose()?;
            Ok(Reply::Last(hit))
        }
        Request::Memories(_) => {
            let hits = value
                .as_array()
                .ok_or("deja memory JSON must be an array")?
                .iter()
                .filter_map(|v| {
                    Some(MemoryHit {
                        path: v.get("path")?.as_str()?.into(),
                        project: v.get("project")?.as_str()?.into(),
                        name: v.get("name")?.as_str()?.into(),
                        excerpt: v
                            .get("snippets")
                            .and_then(Value::as_array)
                            .map(|s| s.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n"))
                            .unwrap_or_default(),
                    })
                })
                .collect();
            Ok(Reply::Memories(hits))
        }
        Request::Memory(_) => Ok(Reply::Text(
            value
                .get("content")
                .and_then(Value::as_str)
                .ok_or("deja memory JSON has no content")?
                .into(),
        )),
        Request::Query(..) => Ok(Reply::Text(
            value
                .get("answer")
                .and_then(Value::as_str)
                .ok_or("deja query JSON has no answer")?
                .into(),
        )),
    }
}

/// A locator pins the query to one transcript. Never interpolate CLI output
/// into shell code, and never widen a failed lookup into a cross-session query.
fn query_locator(target: &Target) -> Result<String, String> {
    if let Some(locator) = &target.locator {
        return Ok(locator.clone());
    }
    let provider = ruddr_history::Provider::ALL
        .into_iter()
        .find(|p| p.name() == target.provider)
        .ok_or("Unknown session provider")?;
    let stores = ruddr_history::Stores::discover();
    let info = if provider == ruddr_history::Provider::OpenCode {
        ruddr_history::list_sessions(&stores, usize::MAX)
            .into_iter()
            .find(|s| s.provider == provider && s.id == target.id)
    } else {
        ruddr_history::find_session(&stores, provider, &target.id)
    };
    info.map(|s| s.locator)
        .ok_or_else(|| format!("No {} transcript found for {}", target.provider, target.id))
}

pub fn command(request: &Request, cwd: &std::path::Path) -> Result<Command, String> {
    let mut cmd = Command::new("deja");
    cmd.current_dir(cwd);
    match request {
        Request::Find(terms) => {
            cmd.arg("find")
                .args(["--json", "--quiet", "--limit", "30", "--"])
                .args(terms.split_whitespace());
        }
        Request::Memories(phrase) => {
            cmd.args(["memory", "search", "--json", "--limit", "30", "--", phrase]);
        }
        Request::Memory(path) => {
            cmd.args(["memory", "show", "--json", "--", path]);
        }
        Request::Last => {
            cmd.args(["last", "--json", "--quiet", "--list", "--limit", "1"]);
        }
        Request::Query(target, question) => {
            cmd.args(["query", "--json", "--", &query_locator(target)?, question]);
        }
    }
    Ok(cmd)
}

pub fn execute(request: Request, cwd: PathBuf) -> Result<Reply, String> {
    let timeout = if matches!(request, Request::Query(..)) {
        Duration::from_secs(180)
    } else {
        Duration::from_secs(30)
    };
    let (out, err, ok, truncated) = actions::run_bounded(command(&request, &cwd)?, timeout, 8 * 1024 * 1024)?;
    if truncated {
        return Err("deja output exceeded 8 MiB; narrow the search".into());
    }
    // `last` returns exit 1 with a valid empty sessions array.
    if !ok && !(request == Request::Last && matches!(parse_reply(&request, &out), Ok(Reply::Last(None)))) {
        return Err(if err.trim().is_empty() {
            "deja command failed".into()
        } else {
            err.trim().chars().take(1000).collect()
        });
    }
    parse_reply(&request, &out)
}

/// Map lowercase matches back to original characters. Lowercasing can expand
/// Unicode characters, so byte offsets from folded text cannot slice the input.
fn matched_chars(text: &str, query: &str) -> Vec<bool> {
    let mut folded = String::new();
    let ranges: Vec<_> = text
        .chars()
        .map(|c| {
            let start = folded.len();
            folded.extend(c.to_lowercase());
            start..folded.len()
        })
        .collect();
    let mut matched = vec![false; ranges.len()];
    for term in query.split_whitespace().map(str::to_lowercase) {
        for (start, _) in folded.match_indices(&term) {
            let end = start + term.len();
            for (i, range) in ranges.iter().enumerate() {
                if range.start < end && range.end > start {
                    matched[i] = true;
                }
            }
        }
    }
    matched
}

fn excerpt(text: &str, query: &str, limit: usize) -> String {
    let chars: Vec<_> = text.chars().collect();
    let matched = matched_chars(text, query);
    let start = matched.iter().position(|m| *m).unwrap_or(0).saturating_sub(24);
    let mut out = if start > 0 { "…".into() } else { String::new() };
    out.extend(chars.iter().skip(start).take(limit).map(|c| if c.is_control() { ' ' } else { *c }));
    if chars.len() > start + limit {
        out.push('…');
    }
    out
}

fn highlighted(text: &str, query: &str, base: ratatui::style::Style, accent: ratatui::style::Style) -> ratatui::text::Line<'static> {
    let matched = matched_chars(text, query);
    ratatui::text::Line::from(
        text.chars()
            .enumerate()
            .map(|(i, c)| ratatui::text::Span::styled(c.to_string(), if matched[i] { accent } else { base }))
            .collect::<Vec<_>>(),
    )
}

pub fn draw(frame: &mut ratatui::Frame, browser: &mut Browser, screen: ratatui::layout::Rect, p: &crate::theme::Palette) {
    use ratatui::layout::{Constraint, Layout, Rect};
    use ratatui::style::Style;
    use ratatui::text::Line;
    use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
    browser.hits.clear();
    let margin = if screen.width <= 64 { 0 } else { 2 };
    let area = Rect {
        x: screen.x + margin,
        y: screen.y + 1,
        width: screen.width.saturating_sub(2 * margin),
        height: screen.height.saturating_sub(2),
    };
    let title = match browser.page {
        Page::Sessions => "find a past session",
        Page::Memories => "search project memories",
        Page::Last => "continue where I left off",
        Page::Question(_) => "query selected session",
        Page::Confirm(..) => "confirm paid query",
        Page::Text => "dejavu preview / answer",
    };
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(format!(" {title} "))
        .border_style(Style::new().fg(p.accent.c()))
        .style(Style::new().fg(p.text.c()).bg(p.panel.c()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let parts = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .split(inner);
    let base = Style::new().fg(p.text.c());
    let accent = Style::new().fg(p.accent.c()).bold();
    let entry = match &browser.page {
        Page::Sessions | Page::Memories | Page::Question(_) => format!("› {}▏", browser.input),
        Page::Confirm(_, question) => format!("Question: {question}"),
        _ => String::new(),
    };
    // Keep the end of long input visible on narrow terminals.
    let capacity = parts[0].width as usize * 2;
    let entry: String = entry.chars().skip(entry.chars().count().saturating_sub(capacity)).collect();
    frame.render_widget(Paragraph::new(entry).style(accent).wrap(Wrap { trim: false }), parts[0]);
    frame.render_widget(
        Paragraph::new(browser.status.as_str())
            .style(Style::new().fg(p.dim.c()))
            .wrap(Wrap { trim: false }),
        parts[1],
    );
    let plain = |c| KeyEvent::new(c, KeyModifiers::NONE);
    let controls = match browser.page {
        Page::Sessions => vec![
            ("Enter open", plain(KeyCode::Enter)),
            ("^R resume", KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            ("Esc close", plain(KeyCode::Esc)),
        ],
        Page::Memories => vec![("Enter preview", plain(KeyCode::Enter)), ("Esc close", plain(KeyCode::Esc))],
        Page::Question(_) => vec![("Enter review", plain(KeyCode::Enter)), ("Esc cancel", plain(KeyCode::Esc))],
        Page::Confirm(..) => vec![("y pay & ask", plain(KeyCode::Char('y'))), ("n cancel", plain(KeyCode::Char('n')))],
        _ => vec![
            ("PgUp", plain(KeyCode::PageUp)),
            ("PgDn", plain(KeyCode::PageDown)),
            ("Esc close", plain(KeyCode::Esc)),
        ],
    };
    let buttons = Layout::horizontal(vec![Constraint::Ratio(1, controls.len() as u32); controls.len()]).split(parts[3]);
    for ((label, key), rect) in controls.into_iter().zip(buttons.iter().copied()) {
        let block = Block::default().borders(Borders::ALL).border_style(Style::new().fg(p.border.c()));
        frame.render_widget(Paragraph::new(label).centered().style(accent).block(block), rect);
        browser.hits.push((rect, Click::Key(key)));
    }
    match &browser.page {
        Page::Sessions | Page::Memories => {
            let rows = (parts[2].height / 3).max(1) as usize;
            let start = browser.selected / rows * rows;
            let len = if browser.page == Page::Sessions {
                browser.sessions.len()
            } else {
                browser.memories.len()
            };
            for i in start..len.min(start + rows) {
                let (label, snippet) = if browser.page == Page::Sessions {
                    let hit = &browser.sessions[i];
                    (
                        format!(
                            "{}. {} · {} · {}",
                            i + 1,
                            hit.provider,
                            hit.date,
                            hit.project.rsplit('/').next().unwrap_or(&hit.project)
                        ),
                        hit.excerpt.as_str(),
                    )
                } else {
                    let hit = &browser.memories[i];
                    (format!("{}. {} · {}", i + 1, hit.name, hit.project), hit.excerpt.as_str())
                };
                let y = parts[2].y + ((i - start) * 3) as u16;
                let rect = Rect {
                    y,
                    height: 3.min(parts[2].bottom().saturating_sub(y)),
                    ..parts[2]
                };
                browser.hits.push((rect, Click::Result(i)));
                let style = if i == browser.selected { base.bg(p.selected.c()) } else { base };
                frame.render_widget(Paragraph::new(Line::from(label)).style(style.bold()), Rect { height: 1, ..rect });
                let snippet = excerpt(snippet, &browser.input, (rect.width as usize * 2).saturating_sub(4));
                frame.render_widget(
                    Paragraph::new(highlighted(
                        &snippet,
                        &browser.input,
                        style,
                        accent.bg(if i == browser.selected { p.selected.c() } else { p.panel.c() }),
                    ))
                    .style(style)
                    .wrap(Wrap { trim: false }),
                    Rect {
                        y: y + 1,
                        height: rect.height.saturating_sub(1),
                        ..rect
                    },
                );
            }
        }
        Page::Question(target) | Page::Confirm(target, _) => {
            let label = target.locator.as_deref().unwrap_or(&target.id);
            frame.render_widget(Paragraph::new(format!("Session: {}\n{}\n\nDejavu sends this session to its configured model. The model may charge for input and output. Ruddr has no price estimate.", target.provider, label)).wrap(Wrap { trim: false }), parts[2]);
        }
        Page::Text => {
            let rows: Vec<_> = browser
                .text
                .lines()
                .map(|line| crate::text::Row::new(Line::from(line.to_string())))
                .collect();
            let lines = crate::text::wrap_rows(&rows, parts[2].width as usize, "", accent);
            let max = lines.len().saturating_sub(parts[2].height as usize).min(u16::MAX as usize) as u16;
            browser.scroll = browser.scroll.min(max);
            frame.render_widget(Paragraph::new(lines).scroll((browser.scroll, 0)), parts[2]);
        }
        Page::Last => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn target() -> Target {
        Target {
            provider: "codex".into(),
            id: "one".into(),
            locator: Some("/sessions/one.jsonl".into()),
        }
    }

    #[test]
    fn parses_ranked_hits_from_every_provider_and_matching_excerpts() {
        let hits = json!({"hits": [
            {"source":"claude", "path":"/c.jsonl", "resume":"claude --resume c", "matches":[{"text":"MATCH"}], "openingPrompt":"opening"},
            {"source":"codex", "path":"/x.jsonl", "resume":"codex resume x"},
            {"source":"pi", "path":"/pi session.jsonl", "resume":"pi --session /pi session.jsonl"},
            {"source":"opencode", "path":"opencode:///db#ses_a", "resume":"opencode2 -s ses_a"},
            {"source":"droid", "path":"/d.jsonl", "resume":"droid --resume d"},
            {"source":"codex", "path":"/no-resume.jsonl", "resume":null}
        ]});
        let Reply::Sessions(hits) = parse_reply(&Request::Find("match".into()), &hits.to_string()).unwrap() else {
            panic!()
        };
        assert_eq!(
            hits.iter().map(|h| h.provider.as_str()).collect::<Vec<_>>(),
            ["claude", "codex", "pi", "opencode", "droid", "codex"]
        );
        assert_eq!(hits[0].excerpt, "MATCH");
        assert_eq!(hits[2].session_id, "/pi session.jsonl");
        assert_eq!(hits[3].session_id, "ses_a");
        assert!(hits[5].session_id.is_empty());
    }

    #[test]
    fn parses_last_memory_and_query_contracts() {
        assert!(matches!(parse_reply(&Request::Last, r#"{"sessions":[]}"#), Ok(Reply::Last(None))));
        assert!(matches!(
            parse_reply(&Request::Last, r#"{"sessions":[{"source":"codex","path":"/x.jsonl"}]}"#),
            Ok(Reply::Last(Some(_)))
        ));
        let Reply::Memories(hits) = parse_reply(
            &Request::Memories("x".into()),
            r#"[{"project":"p","name":"a.md","path":"/a.md","snippets":["one","two"]}]"#,
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(hits[0].excerpt, "one\ntwo");
        for (request, json, expected) in [
            (Request::Memory("/a.md".into()), r#"{"content":"memory text"}"#, "memory text"),
            (
                Request::Query(target(), "q".into()),
                r#"{"answer":"answer text","costUsd":0.001}"#,
                "answer text",
            ),
        ] {
            let Reply::Text(text) = parse_reply(&request, json).unwrap() else {
                panic!()
            };
            assert_eq!(text, expected);
        }
        assert!(parse_reply(&Request::Last, "not JSON").is_err());
        assert!(parse_reply(&Request::Find("x".into()), "{}").is_err());
        assert!(parse_reply(&Request::Query(target(), "q".into()), "{}").is_err());
    }

    #[test]
    fn typing_debounces_coalesces_and_discards_stale_results() {
        let now = Instant::now();
        let mut b = Browser::default();
        b.open(Page::Sessions, now);
        b.paste("first", now);
        assert!(b.take_request(now + DEBOUNCE - Duration::from_millis(1)).is_none());
        let (first, _) = b.take_request(now + DEBOUNCE).unwrap();
        b.input = "second".into();
        b.edit(now + DEBOUNCE);
        b.input = "third".into();
        b.edit(now + DEBOUNCE);
        assert!(b.deadline().is_none());
        assert!(b.take_request(now + DEBOUNCE * 2).is_none());
        b.receive(Response {
            generation: first,
            result: Err("stale error".into()),
        });
        assert!(!b.status.contains("stale error"));
        let (third, request) = b.take_request(now + DEBOUNCE * 2).unwrap();
        assert_eq!(request, Request::Find("third".into()));
        b.receive(Response {
            generation: third,
            result: Ok(Reply::Sessions(vec![])),
        });
        assert_eq!(b.status, "0 ranked sessions");
    }

    #[test]
    fn clearing_or_closing_cannot_reopen_an_old_search() {
        let now = Instant::now();
        let mut b = Browser::default();
        b.open(Page::Sessions, now);
        b.paste("search", now);
        let (generation, _) = b.take_request(now + DEBOUNCE).unwrap();
        b.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL), now);
        assert!(b.pending.is_none());
        b.close();
        b.open(Page::Memories, now);
        b.receive(Response {
            generation,
            result: Ok(Reply::Text("obsolete".into())),
        });
        assert_eq!(b.page, Page::Memories);
        assert!(b.text.is_empty());
    }

    #[test]
    fn query_needs_explicit_confirmation_and_pins_the_target() {
        let now = Instant::now();
        let mut b = Browser::default();
        b.open(Page::Question(target()), now);
        b.paste("What changed?", now);
        assert!(b.take_request(now + DEBOUNCE).is_none());
        b.key(key(KeyCode::Enter), now);
        assert_eq!(b.page, Page::Confirm(target(), "What changed?".into()));
        b.key(key(KeyCode::Enter), now);
        b.paste("y", now);
        assert!(b.take_request(now).is_none());
        b.key(key(KeyCode::Char('y')), now);
        let (_, request) = b.take_request(now).unwrap();
        assert_eq!(request, Request::Query(target(), "What changed?".into()));
        b.key(key(KeyCode::Char('y')), now);
        assert!(b.take_request(now).is_none());
    }

    #[test]
    fn rejecting_query_never_schedules_it() {
        let now = Instant::now();
        for code in [KeyCode::Esc, KeyCode::Char('n')] {
            let mut b = Browser::default();
            b.open(Page::Question(target()), now);
            b.paste("question", now);
            b.key(key(KeyCode::Enter), now);
            b.key(key(code), now);
            assert!(!b.visible);
            assert!(b.take_request(now).is_none());
        }
    }

    #[test]
    fn memory_preview_uses_the_exact_path() {
        let now = Instant::now();
        let mut b = Browser::default();
        b.open(Page::Memories, now);
        b.memories.push(MemoryHit {
            project: "p".into(),
            name: "x".into(),
            path: "/memory a/x.md".into(),
            excerpt: "x".into(),
        });
        b.key(key(KeyCode::Enter), now);
        assert_eq!(b.take_request(now).unwrap().1, Request::Memory("/memory a/x.md".into()));
    }

    #[test]
    fn command_arguments_preserve_boundaries_and_current_directory() {
        let cwd = std::path::Path::new("/workspace");
        let cases = [
            (
                Request::Find("alpha --model".into()),
                vec!["find", "--json", "--quiet", "--limit", "30", "--", "alpha", "--model"],
            ),
            (
                Request::Memories("two words".into()),
                vec!["memory", "search", "--json", "--limit", "30", "--", "two words"],
            ),
            (Request::Last, vec!["last", "--json", "--quiet", "--list", "--limit", "1"]),
            (
                Request::Query(target(), "$(touch nope) --model bad".into()),
                vec!["query", "--json", "--", "/sessions/one.jsonl", "$(touch nope) --model bad"],
            ),
        ];
        for (request, expected) in cases {
            let cmd = command(&request, cwd).unwrap();
            assert_eq!(cmd.get_current_dir(), Some(cwd));
            assert_eq!(cmd.get_args().map(|a| a.to_str().unwrap()).collect::<Vec<_>>(), expected);
        }
    }

    #[test]
    fn unicode_highlights_preserve_original_characters() {
        assert_eq!(matched_chars("İ猫 Rust", "i rust"), [true, false, false, true, true, true, true]);
        let text = "İ猫 Rust";
        let line = highlighted(text, "i rust", ratatui::style::Style::new(), ratatui::style::Style::new().bold());
        assert_eq!(crate::text::line_text(&line), text);
        let snippet = excerpt(&format!("{} MATCH ending", "prefix ".repeat(100)), "match", 60);
        assert!(snippet.contains("MATCH"));
        assert!(snippet.starts_with('…'));
    }

    #[test]
    fn mobile_controls_cover_three_rows_and_clicks_open_results() {
        use ratatui::{Terminal, backend::TestBackend, layout::Rect};
        let now = Instant::now();
        let mut b = Browser::default();
        b.open(Page::Sessions, now);
        b.sessions.push(parse_deja_hit(&json!({"source":"codex", "path":"/one.jsonl", "resume":"codex resume one", "matches":[{"text":"matched browser excerpt"}]})).unwrap());
        b.input = "browser".into();
        for (width, height) in [(46, 34), (120, 38)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|f| draw(f, &mut b, Rect::new(0, 0, width, height), &crate::theme::themes()[0].palette))
                .unwrap();
            assert!(b.hits.iter().all(|(rect, _)| rect.height == 3));
            let (row, _) = b.hits.iter().find(|(_, action)| matches!(action, Click::Result(0))).unwrap();
            let effect = b.mouse(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: row.x + 1,
                    row: row.y + 2,
                    modifiers: KeyModifiers::NONE,
                },
                now,
            );
            assert!(matches!(effect, Some(Effect::Open(_))));
            b.open(Page::Sessions, now);
            b.sessions
                .push(parse_deja_hit(&json!({"source":"codex", "path":"/one.jsonl"})).unwrap());
        }
    }

    #[test]
    fn last_opens_only_the_current_response_and_errors_stay_visible() {
        let now = Instant::now();
        let mut b = Browser::default();
        b.open(Page::Last, now);
        let (generation, request) = b.take_request(now).unwrap();
        assert_eq!(request, Request::Last);
        let hit = parse_deja_hit(&json!({"source":"droid", "path":"/d.jsonl"})).unwrap();
        assert!(matches!(
            b.receive(Response {
                generation,
                result: Ok(Reply::Last(Some(hit)))
            }),
            Some(Effect::Open(_))
        ));
        assert!(!b.visible);
        b.open(Page::Last, now);
        let (generation, _) = b.take_request(now).unwrap();
        b.receive(Response {
            generation,
            result: Err("deja is unavailable".into()),
        });
        assert_eq!(b.status, "deja is unavailable");
        assert!(b.visible);
    }
}
