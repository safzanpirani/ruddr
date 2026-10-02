//! The artifact pane as a list of blocks: one per chat entry, activity row,
//! output paragraph, or diff line. Each block has a stable id and a version
//! its source bumps on change; `ui.rs` renders blocks through the render
//! cache so an unchanged block costs nothing per frame.

use crate::activity::{self, ActivityKind};
use crate::app::{App, Tab};
use crate::core::*;
use crate::text::{Row, highlight_code, markdown};
use crate::theme::{Palette, Rgb};
use crate::transcript::{EntryKind, ToolStatus};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ruddr_core::state::Status;

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Spinner frames advance this often; animated blocks re-render no faster.
pub const SPINNER_MS: u64 = 80;

#[derive(Clone, Debug, PartialEq)]
pub enum BlockRef {
    Chat(usize),
    /// The working or waiting row under a live session's chat.
    Live,
    /// The empty-state row; it opens the prompt when clicked.
    Empty,
    Activity(usize),
    Output(usize),
    Diff(usize),
    /// A one-line message (diff errors, clean tree).
    Note,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub id: u64,
    pub version: u64,
    /// A frame counter for blocks that animate; 0 for still blocks.
    pub anim: u64,
    pub r: BlockRef,
    pub diff_header: Option<String>,
    pub hunk: bool,
}

impl Block {
    fn new(tag: u64, value: u64, version: u64, r: BlockRef) -> Block {
        Block {
            id: (tag << 56) | (value & ((1 << 56) - 1)),
            version,
            anim: 0,
            r,
            diff_header: None,
            hunk: false,
        }
    }
}

pub fn spinner_frame(app: &App) -> u64 {
    app.started.elapsed().as_millis() as u64 / SPINNER_MS
}

fn working(status: Status) -> bool {
    matches!(status, Status::Active | Status::Starting)
}

fn mix_hash(values: &[u64]) -> u64 {
    values
        .iter()
        .fold(0xcbf29ce484222325u64, |h, v| (h ^ v).wrapping_mul(0x100000001b3))
}

/// Changes whenever the current pane's content could have changed.
pub fn content_key(app: &App) -> u64 {
    let s = &app.sources;
    mix_hash(&[
        app.tab as u64,
        s.transcript.generation,
        s.activities.generation,
        s.transcript.tool_generation,
        s.output.generation,
        s.diff.generation,
        app.folded.len() as u64,
        app.expanded.len() as u64,
        s.scope.0.len() as u64,
        crate::cache::hash_query(&s.scope.0),
    ])
}

pub fn build_blocks(app: &App) -> Vec<Block> {
    let session = app.current();
    let frame = spinner_frame(app);
    let mut blocks = Vec::new();
    let status = session.map(|s| s.status);
    match app.tab {
        Tab::Chat => {
            for (index, entry) in app.sources.transcript.entries().iter().enumerate() {
                if entry.hidden {
                    continue;
                }
                let mut block = Block::new(1, index as u64, entry.version, BlockRef::Chat(index));
                if entry.kind == EntryKind::Tool && entry.status == Some(ToolStatus::Running) {
                    block.anim = frame + 1;
                }
                blocks.push(block);
            }
            if blocks.is_empty() && session.is_some() {
                blocks.push(Block::new(9, 1, status.map(|s| s as u64).unwrap_or(0) + 1, BlockRef::Empty));
            }
        }
        Tab::Trace => {
            let details = &app.sources.activity_details;
            for (index, a) in app.sources.activity_view.iter().enumerate() {
                let expanded = app.expanded.contains(&a.uid);
                let detail_version = if details.get(index).copied().flatten().is_some() {
                    app.sources.transcript.tool_generation
                } else {
                    0
                };
                let version = mix_hash(&[a.version, expanded as u64, detail_version]);
                blocks.push(Block::new(2, a.uid, version, BlockRef::Activity(index)));
            }
            if blocks.is_empty() && session.is_some() {
                blocks.push(Block::new(9, 2, 1, BlockRef::Note));
            }
        }
        Tab::Output => {
            for (index, (uid, version, text)) in app.sources.output.paragraphs.iter().enumerate() {
                if !text.is_empty() {
                    blocks.push(Block::new(3, *uid, *version, BlockRef::Output(index)));
                }
            }
            if blocks.is_empty() && session.is_some() {
                blocks.push(Block::new(9, 3, 1, BlockRef::Note));
            }
        }
        Tab::Diff => {
            let diff = &app.sources.diff;
            if !diff.loaded || diff.error.is_some() || diff.lines.is_empty() {
                if session.is_some() {
                    let version = mix_hash(&[diff.generation, diff.loaded as u64]);
                    blocks.push(Block::new(9, 4, version, BlockRef::Note));
                }
                return blocks;
            }
            if diff.recorded.is_some() {
                // Says where the diff came from when Git could not provide it.
                blocks.push(Block::new(9, 7, diff.generation, BlockRef::Note));
            }
            for (index, line) in diff.lines.iter().enumerate() {
                let file = &diff.files[line.file];
                let folded = app.folded.contains(&file.path);
                if line.kind == DiffKind::Meta || (folded && line.kind != DiffKind::FileHeader) {
                    continue;
                }
                let mut block = Block::new(4, index as u64, mix_hash(&[diff.generation, folded as u64]), BlockRef::Diff(index));
                if line.kind == DiffKind::FileHeader {
                    block.diff_header = Some(file.path.clone());
                }
                block.hunk = line.kind == DiffKind::Hunk;
                blocks.push(block);
            }
        }
    }
    // The working row follows the conversation and the activity feed.
    if matches!(app.tab, Tab::Chat | Tab::Trace)
        && let Some(status) = status
    {
        if working(status) {
            let mut block = Block::new(9, 5, status as u64, BlockRef::Live);
            block.anim = frame + 1;
            blocks.push(block);
        } else if status == Status::Idle && app.tab == Tab::Chat {
            blocks.push(Block::new(9, 6, 1, BlockRef::Live));
        }
    }
    blocks
}

/// The text search matches against.
pub fn block_text(app: &App, block: &Block) -> String {
    match block.r {
        BlockRef::Chat(index) => app.sources.transcript.entries()[index].text.clone(),
        BlockRef::Activity(index) => {
            let a = &app.sources.activity_view[index];
            activity::search_text(a, detail_for(app, index))
        }
        BlockRef::Output(index) => app.sources.output.paragraphs[index].2.clone(),
        BlockRef::Diff(index) => diff_text(app, index),
        BlockRef::Live | BlockRef::Empty | BlockRef::Note => String::new(),
    }
}

/// The text `c` copies.
pub fn block_copy(app: &App, block: &Block) -> String {
    match block.r {
        BlockRef::Activity(index) => activity::copy_text(&app.sources.activity_view[index], detail_for(app, index)),
        _ => block_text(app, block),
    }
}

fn diff_text(app: &App, index: usize) -> String {
    let diff = &app.sources.diff;
    let line = &diff.lines[index];
    if line.kind == DiffKind::FileHeader {
        diff.files[line.file].path.clone()
    } else {
        line.text.get(1..).unwrap_or("").to_string()
    }
}

fn detail_for(app: &App, index: usize) -> Option<&crate::transcript::ToolDetail> {
    app.sources
        .activity_details
        .get(index)
        .copied()
        .flatten()
        .and_then(|d| app.sources.tools.get(d))
}

/// Whether a visible block needs frames: a spinner, a shimmer.
pub fn animates(block: &Block) -> bool {
    block.anim != 0
}

pub fn render_block(app: &App, block: &Block, p: &Palette) -> Vec<Row> {
    match &block.r {
        BlockRef::Chat(index) => render_chat(app, *index, block.anim.saturating_sub(1), p),
        BlockRef::Live => render_live(app, block.anim.saturating_sub(1), p),
        BlockRef::Empty => {
            let starting = app.current().is_some_and(|s| s.status == Status::Starting);
            if starting {
                vec![Row::new(Line::styled("  Session is starting…", Style::new().fg(p.dim.c())))]
            } else {
                vec![Row::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        "No conversation yet. Press s or click here to send a prompt.",
                        Style::new().fg(p.accent.c()).underlined(),
                    ),
                ]))]
            }
        }
        BlockRef::Activity(index) => render_activity(app, *index, p),
        BlockRef::Output(index) => {
            let mut rows = Vec::new();
            for mut row in markdown(&app.sources.output.paragraphs[*index].2, p, Style::new().fg(p.text.c())) {
                row.line.spans.insert(0, Span::raw(" "));
                rows.push(row);
            }
            rows.push(Row::new(Line::raw("")));
            rows
        }
        BlockRef::Diff(index) => render_diff(app, *index, p),
        BlockRef::Note => render_note(app, p),
    }
}

fn render_note(app: &App, p: &Palette) -> Vec<Row> {
    let history = crate::history::is_history(&app.sources.scope.0);
    let (text, colour) = match app.tab {
        Tab::Trace if history => (
            "Agent history has no Ruddr activity log; tool calls are in the chat.".to_string(),
            p.dim,
        ),
        Tab::Trace => ("No activity has been recorded yet.".to_string(), p.dim),
        Tab::Output if history => ("This session has no assistant messages.".to_string(), p.dim),
        Tab::Output => ("No output has been written yet.".to_string(), p.dim),
        _ => {
            let diff = &app.sources.diff;
            match &diff.error {
                Some(error) => (format!("× {error}"), p.danger),
                None if !diff.loaded && history => ("Reading the session…".to_string(), p.dim),
                None if !diff.loaded => ("Reading git diff…".to_string(), p.dim),
                None if history => ("This session edited no files.".to_string(), p.dim),
                None => match &diff.recorded {
                    Some(reason) if diff.lines.is_empty() => (format!("{reason} · this session has recorded no file edits yet."), p.dim),
                    Some(reason) => (format!("{reason} · showing the edits this session recorded"), p.dim),
                    None => ("✓ Working tree clean against HEAD.".to_string(), p.success),
                },
            }
        }
    };
    vec![Row::new(Line::styled(format!("  {text}"), Style::new().fg(colour.c())))]
}

fn spinner(frame: u64) -> &'static str {
    SPINNER[frame as usize % SPINNER.len()]
}

/// A bright band that travels across dim text.
pub fn shimmer(text: &str, base: Rgb, bright: Rgb, seconds: f32) -> Vec<Span<'static>> {
    let count = text.chars().count() as f32;
    let head = (seconds * 14.0) % (count + 12.0) - 6.0;
    text.chars()
        .enumerate()
        .map(|(i, c)| {
            let d = (i as f32 - head).abs();
            let t = (1.0 - d / 5.0).max(0.0);
            Span::styled(c.to_string(), Style::new().fg(base.mix(bright, t).c()))
        })
        .collect()
}

fn render_live(app: &App, frame: u64, p: &Palette) -> Vec<Row> {
    let Some(s) = app.current() else { return vec![] };
    if s.status == Status::Idle {
        return vec![Row::new(Line::from(vec![
            Span::styled("  ◌ ", Style::new().fg(p.accent.c())),
            Span::styled("waiting for your next prompt · s to send", Style::new().fg(p.dim.c())),
        ]))];
    }
    let seconds = frame as f32 * SPINNER_MS as f32 / 1000.0;
    let mut spans = vec![Span::styled(format!("  {} ", spinner(frame)), Style::new().fg(p.accent.c()))];
    let label = if s.status == Status::Starting {
        "starting session"
    } else if app.tab == Tab::Trace {
        "working"
    } else {
        "thinking"
    };
    spans.extend(shimmer(label, p.dim, p.accent, seconds));
    spans.push(Span::styled(
        format!("  {}", format_elapsed(&s.started_at, None, now_ms())),
        Style::new().fg(p.border.c()),
    ));
    vec![Row::new(Line::from(spans))]
}

fn render_chat(app: &App, index: usize, frame: u64, p: &Palette) -> Vec<Row> {
    let transcript = &app.sources.transcript;
    let entry = &transcript.entries()[index];
    let mut rows = Vec::new();
    match entry.kind {
        EntryKind::User => {
            rows.push(Row::new(Line::raw("")));
            let bubble = p.background.mix(p.accent, 0.08);
            rows.push(Row::new(
                Line::from(vec![
                    Span::styled("▌ ", Style::new().fg(p.accent.c())),
                    Span::styled("you", Style::new().fg(p.accent.c()).bold()),
                ])
                .style(Style::new().bg(bubble.c())),
            ));
            for line in entry.text.lines() {
                rows.push(
                    Row::new(
                        Line::from(vec![
                            Span::styled("▌ ", Style::new().fg(p.accent.c())),
                            Span::styled(line.to_string(), Style::new().fg(p.text.c())),
                        ])
                        .style(Style::new().bg(bubble.c())),
                    )
                    .indent(2),
                );
            }
            rows.push(Row::new(Line::raw("")));
        }
        EntryKind::Agent => {
            let provider = app.current().map(|s| crate::core::provider(s).to_string()).unwrap_or_default();
            rows.push(Row::new(Line::from(vec![
                Span::styled("◆ ", Style::new().fg(p.success.c())),
                Span::styled(provider, Style::new().fg(p.success.c()).bold()),
            ])));
            let streaming = transcript.is_streaming(index);
            let (committed, tail) = transcript.display(index);
            // A committed blank line stays, so the next line does not jump
            // down when it commits.
            let committed = committed.trim_start();
            let tail = if committed.is_empty() { tail.trim() } else { tail.trim_end() };
            // Completed lines render as markdown; the line still arriving
            // stays plain until its newline lands.
            let mut body = markdown(committed, p, Style::new().fg(p.text.c()));
            for line in tail.lines() {
                body.push(Row::new(Line::styled(line.to_string(), Style::new().fg(p.text.c()))));
            }
            for row in &mut body {
                row.line.spans.insert(0, Span::raw("  "));
                row.indent += 2;
            }
            if streaming {
                match body.last_mut() {
                    Some(last) => last.line.spans.push(Span::styled("▍", Style::new().fg(p.accent.c()))),
                    None => body.push(Row::new(Line::from(vec![
                        Span::raw("  "),
                        Span::styled("▍", Style::new().fg(p.accent.c())),
                    ]))),
                }
            }
            rows.extend(body);
            rows.push(Row::new(Line::raw("")));
        }
        EntryKind::Thought => {
            rows.push(
                Row::new(Line::from(vec![
                    Span::styled("  ∴ ", Style::new().fg(p.dim.c())),
                    Span::styled(entry.text.clone(), Style::new().fg(p.dim.c()).italic()),
                ]))
                .indent(4),
            );
        }
        EntryKind::Tool => {
            let (glyph, colour) = match entry.status {
                Some(ToolStatus::Running) => (spinner(frame).to_string(), p.warning),
                Some(ToolStatus::Failed) => ("✗".into(), p.danger),
                _ => ("✓".into(), p.success),
            };
            let mut spans = vec![Span::styled(format!("  {glyph} "), Style::new().fg(colour.c()))];
            spans.extend(highlight_code(&entry.text, "sh", p).into_iter().map(|s| {
                let fg = s.style.fg.unwrap_or(p.dim.c());
                let dimmed = match fg {
                    ratatui::style::Color::Rgb(r, g, b) => Rgb(r, g, b).mix(p.background, 0.3).c(),
                    other => other,
                };
                s.fg(dimmed)
            }));
            rows.push(Row::new(Line::from(spans)).indent(4));
        }
    }
    rows
}

const TOOL_OUTPUT_LINES: usize = 40;

fn render_activity(app: &App, index: usize, p: &Palette) -> Vec<Row> {
    let a = &app.sources.activity_view[index];
    let detail = detail_for(app, index);
    let short = a.timestamp.get(11..19).unwrap_or("").to_string();
    let mut spans = vec![Span::styled(format!(" {short:<8} "), Style::new().fg(p.border.mix(p.dim, 0.5).c()))];
    let expanded = app.expanded.contains(&a.uid);
    match a.kind {
        ActivityKind::Thought => spans.push(Span::styled(a.text.clone(), Style::new().fg(p.dim.c()).italic())),
        ActivityKind::Tool => {
            let state = detail.map(|d| d.status()).or(a.tool_status).unwrap_or(ToolStatus::Running);
            let (glyph, colour) = match state {
                ToolStatus::Completed => ("✓", p.success),
                ToolStatus::Failed => ("×", p.danger),
                ToolStatus::Running => ("◐", p.warning),
            };
            let sub_agent = detail.is_some_and(|d| d.kind == "subAgentActivity");
            let label = if sub_agent {
                "agent".to_string()
            } else {
                a.label.clone().unwrap_or("tool".into())
            };
            let text = match detail {
                Some(d) if sub_agent => format!(
                    "{} {}",
                    d.activity_kind.as_deref().unwrap_or("activity"),
                    d.agent_path.as_deref().or(d.agent_thread_id.as_deref()).unwrap_or("sub-agent")
                ),
                _ => a.text.clone(),
            };
            let duration = detail
                .and_then(|d| d.duration_ms)
                .or(a.duration_ms)
                .map(|ms| format!("  {}", format_duration(ms)))
                .unwrap_or_default();
            let exit = detail
                .and_then(|d| d.exit_code)
                .filter(|c| *c != 0)
                .map(|c| format!("  exit {c}"))
                .unwrap_or_default();
            spans.push(Span::styled(if expanded { "▾ " } else { "▸ " }, Style::new().fg(p.dim.c())));
            spans.push(Span::styled(format!("{glyph} "), Style::new().fg(colour.c())));
            spans.push(Span::styled(format!("{label}  "), Style::new().fg(colour.c()).bold()));
            spans.push(Span::styled(text, Style::new().fg(p.text.c())));
            spans.push(Span::styled(duration, Style::new().fg(p.dim.c())));
            spans.push(Span::styled(exit, Style::new().fg(p.danger.c())));
        }
        ActivityKind::Message => {
            spans.push(Span::styled("› ", Style::new().fg(p.accent.c())));
            spans.push(Span::styled(a.text.clone(), Style::new().fg(p.text.c())));
        }
        ActivityKind::Warning => spans.push(Span::styled(format!("! {}", a.text), Style::new().fg(p.warning.c()))),
        ActivityKind::Error => spans.push(Span::styled(format!("× {}", a.text), Style::new().fg(p.danger.c()))),
        ActivityKind::Status => {
            let label = a.label.as_deref().map(|l| format!("{l} ")).unwrap_or_default();
            spans.push(Span::styled(format!("• {label}{}", a.text), Style::new().fg(p.dim.c())));
        }
    }
    let mut rows = vec![Row::new(Line::from(spans)).indent(12)];
    if expanded && a.kind == ActivityKind::Tool {
        rows.extend(render_tool_detail(detail, &a.text, p));
    }
    rows
}

fn render_tool_detail(detail: Option<&crate::transcript::ToolDetail>, fallback: &str, p: &Palette) -> Vec<Row> {
    let rail = || Span::styled("           │ ", Style::new().fg(p.border.c()));
    let field = |label: &str, value: String, colour: Rgb| {
        Row::new(Line::from(vec![
            rail(),
            Span::styled(format!("{label:<8}"), Style::new().fg(p.dim.c())),
            Span::styled(value, Style::new().fg(colour.c())),
        ]))
        .indent(21)
    };
    let mut rows = Vec::new();
    let Some(d) = detail else {
        rows.push(Row::new(Line::from(vec![
            rail(),
            Span::styled("No additional tool detail was captured.", Style::new().fg(p.dim.c()).italic()),
        ])));
        rows.push(Row::new(Line::styled("           ╰", Style::new().fg(p.border.c()))));
        return rows;
    };
    let command = d
        .command
        .clone()
        .or_else(|| d.query.clone())
        .or_else(|| d.tool_name.clone())
        .unwrap_or_else(|| fallback.to_string());
    let status = format!(
        "{}{}{}",
        d.status().as_str(),
        d.exit_code.map(|c| format!(" · exit {c}")).unwrap_or_default(),
        d.duration_ms.map(|ms| format!(" · {}", format_duration(ms))).unwrap_or_default()
    );
    let status_colour = match d.status() {
        ToolStatus::Completed => p.success,
        ToolStatus::Failed => p.danger,
        ToolStatus::Running => p.warning,
    };
    rows.push(field("command", command, p.text));
    rows.push(field("status", status, status_colour));
    if let Some(cwd) = &d.cwd {
        rows.push(field("cwd", cwd.clone(), p.accent));
    }
    if let Some(thread) = &d.agent_thread_id {
        rows.push(field("thread", thread.clone(), p.accent));
    }
    if let Some(input) = d.input.as_ref().filter(|i| i.as_object().is_some_and(|o| !o.is_empty())) {
        rows.push(Row::new(Line::from(vec![
            rail(),
            Span::styled("input", Style::new().fg(p.dim.c())),
        ])));
        for line in serde_json::to_string_pretty(input).unwrap_or_default().lines() {
            let mut spans = vec![rail()];
            spans.extend(highlight_code(line, "json", p));
            rows.push(Row::new(Line::from(spans)).indent(13));
        }
    }
    if let Some(output) = d.output.as_ref().filter(|o| !o.trim().is_empty()) {
        let lines: Vec<&str> = output.trim_end().lines().collect();
        let clipped = lines.len() > TOOL_OUTPUT_LINES;
        let label = if clipped {
            format!("output · last {TOOL_OUTPUT_LINES} of {} lines", lines.len())
        } else {
            "output".into()
        };
        rows.push(Row::new(Line::from(vec![rail(), Span::styled(label, Style::new().fg(p.dim.c()))])));
        for line in &lines[lines.len().saturating_sub(TOOL_OUTPUT_LINES)..] {
            rows.push(
                Row::new(Line::from(vec![
                    rail(),
                    Span::styled(line.to_string(), Style::new().fg(p.text.c())),
                ]))
                .indent(13),
            );
        }
    }
    rows.push(Row::new(Line::styled("           ╰", Style::new().fg(p.border.c()))));
    rows
}

fn render_diff(app: &App, index: usize, p: &Palette) -> Vec<Row> {
    let diff = &app.sources.diff;
    let line = &diff.lines[index];
    let file = &diff.files[line.file];
    let gutter = diff.gutter;
    let number = |n: Option<u32>| n.map(|n| format!("{n:>gutter$}")).unwrap_or_else(|| " ".repeat(gutter));
    match line.kind {
        DiffKind::FileHeader => {
            let folded = app.folded.contains(&file.path);
            let status_colour = match file.status {
                'A' => p.success,
                'D' => p.danger,
                'R' => p.warning,
                _ => p.accent,
            };
            let mut spans = vec![
                Span::styled(if folded { " ▸ " } else { " ▾ " }, Style::new().fg(p.accent.c())),
                Span::styled(file.path.clone(), Style::new().fg(p.text.c()).bold()),
                Span::styled(format!("  {}", file.status), Style::new().fg(status_colour.c()).bold()),
                Span::styled(format!("  +{}", file.added), Style::new().fg(p.success.c())),
                Span::styled(format!(" −{}", file.removed), Style::new().fg(p.danger.c())),
            ];
            if folded {
                spans.push(Span::styled("  folded", Style::new().fg(p.dim.c()).italic()));
            }
            if diff.touched.contains(&file.path) {
                spans.push(Span::styled("  ●", Style::new().fg(p.accent.c())));
                spans.push(Span::styled(" this session", Style::new().fg(p.dim.c())));
            }
            vec![
                Row::new(Line::raw("")),
                Row::new(Line::from(spans).style(Style::new().bg(p.panel.c()))),
            ]
        }
        DiffKind::Hunk => {
            let context = line.text.splitn(3, "@@").nth(2).unwrap_or("").trim().to_string();
            vec![Row::new(
                Line::from(vec![
                    Span::styled(format!(" {} ", "┄".repeat(gutter * 2 + 1)), Style::new().fg(p.border.c())),
                    Span::styled(
                        line.text.split("@@").nth(1).unwrap_or("").trim().to_string(),
                        Style::new().fg(p.accent.c()),
                    ),
                    Span::styled(format!("  {context}"), Style::new().fg(p.dim.c()).italic()),
                ])
                .style(Style::new().bg(p.background.mix(p.accent, 0.08).c())),
            )]
        }
        _ => {
            let (sign, bg, gutter_bg, sign_colour) = match line.kind {
                DiffKind::Add => (
                    "+",
                    Some(p.background.mix(p.success, 0.14)),
                    Some(p.background.mix(p.success, 0.26)),
                    p.success,
                ),
                DiffKind::Del => (
                    "-",
                    Some(p.background.mix(p.danger, 0.14)),
                    Some(p.background.mix(p.danger, 0.26)),
                    p.danger,
                ),
                _ => (" ", None, None, p.dim),
            };
            let code = line.text.get(1..).unwrap_or("");
            let lang = file.path.rsplit('.').next().unwrap_or("");
            let mut spans = vec![
                Span::styled(
                    format!(" {} {} ", number(line.old), number(line.new)),
                    Style::new()
                        .fg(p.dim.mix(p.border, 0.3).c())
                        .bg(gutter_bg.unwrap_or(p.background).c()),
                ),
                Span::styled(format!("{sign} "), Style::new().fg(sign_colour.c())),
            ];
            spans.extend(highlight_code(code, lang, p));
            let mut l = Line::from(spans);
            if let Some(bg) = bg {
                l = l.style(Style::new().bg(bg.c()));
            }
            vec![Row::new(l).indent(gutter as u16 * 2 + 5)]
        }
    }
}

/// Elapsed seconds since launch, for time-driven effects.
pub fn seconds(app: &App) -> f32 {
    app.started.elapsed().as_secs_f32()
}
