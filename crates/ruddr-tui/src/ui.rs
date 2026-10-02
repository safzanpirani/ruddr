//! Rendering and motion. Every animation derives from wall-clock time, so a
//! slow frame never desynchronises it. Anything that moves asks for its next
//! frame with `app.animate`; when nothing visible moves, nothing redraws.

use crate::app::*;
use crate::cache::{Key, hash_query};
use crate::core::*;
use crate::theme::{Palette, Rgb};
use crate::view::{self, SPINNER, SPINNER_MS, seconds, shimmer};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout as Split, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ruddr_core::state::Status;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

const LOGO: [&str; 3] = ["┏━┓╻ ╻╺┳┓╺┳┓┏━┓", "┣┳┛┃ ┃ ┃┃ ┃┃┣┳┛", "╹┗╸┗━┛╺┻┛╺┻┛╹┗╸"];
/// Smooth transitions (easing, sliding) run at this frame interval.
const TRANSITION: Duration = Duration::from_millis(16);

fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

fn progress(since: Instant, ms: u64) -> f32 {
    (since.elapsed().as_secs_f32() * 1000.0 / ms as f32).clamp(0.0, 1.0)
}

/// Asks for a frame on the next spinner tick. Spinners, pulses, and
/// shimmers all share this clock, so together they cost one frame per tick.
fn tick(app: &mut App) {
    let elapsed = app.started.elapsed().as_millis() as u64;
    app.animate(Duration::from_millis(SPINNER_MS - elapsed % SPINNER_MS));
}

fn spinner(app: &mut App) -> &'static str {
    tick(app);
    SPINNER[(app.started.elapsed().as_millis() as u64 / SPINNER_MS) as usize % SPINNER.len()]
}

/// A slow breathing pulse between 0 and 1. It asks for frames.
fn pulse(app: &mut App, period: f32) -> f32 {
    tick(app);
    0.5 - 0.5 * (seconds(app) * std::f32::consts::TAU / period).cos()
}

/// Whether the caret of a blinking cursor shows now; asks for the next flip.
fn blink(app: &mut App, since: Instant) -> bool {
    let elapsed = since.elapsed().as_millis() as u64;
    app.animate(Duration::from_millis(530 - elapsed % 530));
    (elapsed / 530).is_multiple_of(2)
}

/// Asks for a frame when a relative time label next changes: every second
/// while it counts seconds, then every minute or hour. `elapsed` labels keep
/// seconds up to an hour ("12m 5s").
fn clock_tick(app: &mut App, timestamp: &str, now: i64, elapsed: bool) {
    let Some(at) = parse_time(timestamp) else { return };
    let ms = (now - at).max(0) as u64;
    let unit = if ms < 60_000 || (elapsed && ms < 3_600_000) {
        1_000
    } else if ms < 3_600_000 || elapsed {
        60_000
    } else {
        3_600_000
    };
    app.animate(Duration::from_millis(unit - ms % unit));
}

fn status_color(p: &Palette, status: Status) -> Rgb {
    match status {
        Status::Active => p.success,
        Status::Idle | Status::Starting => p.accent,
        Status::Failed | Status::Stale => p.danger,
        Status::Interrupted | Status::Stopping => p.warning,
        Status::Completed => p.dim,
    }
}

fn panel<'a>(p: &Palette, title: Line<'a>, focused: bool) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { p.accent.c() } else { p.border.c() }))
        .style(Style::new().bg(p.background.c()).fg(p.text.c()))
        .title(title)
}

fn title<'a>(p: &Palette, text: impl Into<String>, focused: bool) -> Line<'a> {
    Line::from(Span::styled(
        format!(" {} ", text.into()),
        Style::new()
            .fg(if focused { p.accent.c() } else { p.dim.c() })
            .add_modifier(Modifier::BOLD),
    ))
}

/// Text whose colour sweeps between two colours.
fn gradient<'a>(text: &str, from: Rgb, to: Rgb, phase: f32, style: Style) -> Vec<Span<'a>> {
    let count = text.chars().count().max(1) as f32;
    text.chars()
        .enumerate()
        .map(|(i, c)| {
            let t = 0.5 + 0.5 * ((i as f32 / count) * std::f32::consts::TAU - phase).sin();
            Span::styled(c.to_string(), style.fg(from.mix(to, t).c()))
        })
        .collect()
}

fn any_working(app: &App) -> bool {
    app.sessions.iter().any(|s| matches!(s.status, Status::Active | Status::Starting))
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let was_mobile = app.mobile_now;
    app.mobile_now = app.args.mobile || area.width <= app.mobile_threshold;
    // Switching to the chat-first layout hides the session list, so keys go
    // to the chat until the drawer opens.
    if app.mobile_now && !was_mobile && !app.drawer {
        app.focus = Focus::Artifact;
    }
    app.hits.clear();
    app.next_frame = None;
    app.frames += 1;
    let p = *app.palette();
    frame.render_widget(Block::default().style(Style::new().bg(p.background.c()).fg(p.text.c())), area);
    if app.splash {
        draw_splash(frame, app, area);
        return;
    }
    let mobile = app.mobile_now;
    let action_bar = if mobile { 3 } else { 0 };
    let [header, body, footer, bar] = Split::vertical([
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(1),
        Constraint::Length(action_bar),
    ])
    .areas(area);
    draw_header(frame, app, header);

    match app.layout() {
        Layout::Classic => {
            app.body_area = body;
            let wanted = match (app.sessions_ratio, app.sessions_width) {
                (Some(r), _) => (r * body.width as f64).round() as u16,
                (None, Some(w)) => w,
                _ => crate::app::sessions_default(body.width),
            };
            let list_width = wanted.clamp(crate::app::SESSIONS_MIN, crate::app::sessions_max(body.width));
            let [list, side] = Split::horizontal([Constraint::Length(list_width), Constraint::Min(20)]).areas(body);
            draw_sessions(frame, app, list, app.focus == Focus::Sessions || app.dragging_sessions);
            // The list's right border drags to resize it.
            app.hits.push((
                Rect {
                    x: list.right().saturating_sub(1),
                    width: 1,
                    ..list
                },
                Hit::SessionsDivider,
            ));
            draw_main(frame, app, side);
        }
        Layout::Beta => {
            draw_main(frame, app, body);
            let target = if app.drawer { 1.0 } else { 0.0 };
            app.drawer_anim += (target - app.drawer_anim) * 0.35;
            if (app.drawer_anim - target).abs() < 0.02 {
                app.drawer_anim = target;
            } else {
                app.animate(TRANSITION);
            }
            if app.drawer_anim > 0.0 {
                let full = (body.width.saturating_sub(4)).clamp(20, 52);
                let width = (full as f32 * ease_out(app.drawer_anim)).round() as u16;
                if width > 2 {
                    // Dim everything behind the drawer.
                    let backdrop = Rect {
                        x: body.x + width,
                        width: body.width - width,
                        ..body
                    };
                    dim_region(frame, backdrop, &p, 0.55 * app.drawer_anim);
                    app.hits.push((backdrop, Hit::Backdrop));
                    let drawer = Rect { width, ..body };
                    frame.render_widget(Clear, drawer);
                    draw_sessions(frame, app, drawer, true);
                }
            }
        }
    }
    draw_footer(frame, app, footer);
    if mobile {
        draw_action_bar(frame, app, bar);
    }
    draw_toasts(
        frame,
        app,
        Rect {
            height: area.height.saturating_sub(1 + action_bar),
            ..area
        },
    );
    if app.prompt.is_some() {
        draw_prompt(frame, app, area);
    }
    if app.picker.is_some() {
        draw_picker(frame, app, area);
    }
    if app.help {
        draw_help(frame, app, area);
    }
}

fn dim_region(frame: &mut Frame, area: Rect, p: &Palette, amount: f32) {
    let buf = frame.buffer_mut();
    let area = area.intersection(buf.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            let fg = match cell.fg {
                ratatui::style::Color::Rgb(r, g, b) => Rgb(r, g, b),
                _ => p.text,
            };
            let bg = match cell.bg {
                ratatui::style::Color::Rgb(r, g, b) => Rgb(r, g, b),
                _ => p.background,
            };
            cell.set_fg(fg.mix(p.background, amount).c());
            cell.set_bg(bg.mix(Rgb(0, 0, 0), amount * 0.5).c());
        }
    }
}

fn draw_splash(frame: &mut Frame, app: &mut App, area: Rect) {
    app.animate(TRANSITION);
    let p = *app.palette();
    let t = progress(app.started, 900);
    let width = LOGO[0].chars().count() as u16;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(6) / 2;
    for (row, text) in LOGO.iter().enumerate() {
        let chars: Vec<char> = text.chars().collect();
        let spans: Vec<Span> = chars
            .iter()
            .enumerate()
            .map(|(i, c)| {
                // Each glyph rises in along a diagonal wave.
                let local = ((t * 1.6) - (i as f32 / chars.len() as f32) * 0.8 - row as f32 * 0.08).clamp(0.0, 1.0);
                let colour = p
                    .background
                    .mix(p.accent.mix(p.success, i as f32 / chars.len() as f32), ease_out(local));
                Span::styled(
                    if local > 0.05 { c.to_string() } else { " ".into() },
                    Style::new().fg(colour.c()).bold(),
                )
            })
            .collect();
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect {
                x,
                y: y + row as u16,
                width: width.min(area.width),
                height: 1,
            },
        );
    }
    let tag = "steer your agents";
    let fade = ease_out((t - 0.45) / 0.55);
    let tag_x = area.x + area.width.saturating_sub(tag.len() as u16) / 2;
    frame.render_widget(
        Paragraph::new(Span::styled(tag, Style::new().fg(p.background.mix(p.dim, fade).c()).italic())),
        Rect {
            x: tag_x,
            y: y + 4,
            width: (tag.len() as u16).min(area.width),
            height: 1,
        },
    );
}

fn draw_header(frame: &mut Frame, app: &mut App, area: Rect) {
    let p = *app.palette();
    let session = app.current().cloned();
    let selected_working = session.as_ref().is_some_and(|s| s.status == Status::Active);
    // The logo shimmers only while the selected session works.
    let phase = if selected_working {
        tick(app);
        seconds(app) * 0.8
    } else {
        1.2
    };
    let mut spans = vec![Span::raw(" ")];
    spans.extend(gradient("◆ ruddr", p.accent, p.success, phase, Style::new().bold()));
    let mut essential = spans.len();
    if let Some(s) = &session {
        spans.push(Span::styled("  │  ", Style::new().fg(p.border.c())));
        spans.push(Span::styled(project_name(s), Style::new().fg(p.text.c()).bold()));
        essential = spans.len();
        if let Some(branch) = opt(&s.cwd).and_then(|c| app.branches.get(c)).filter(|b| !b.is_empty()) {
            spans.push(Span::styled(format!(":{branch}"), Style::new().fg(p.dim.c())));
        }
        spans.push(Span::styled(
            format!("  {} {}", provider(s), s.model) + &s.effort.as_ref().map(|e| format!(" · {e}")).unwrap_or_default(),
            Style::new().fg(p.dim.c()),
        ));
        if selected_working && !app.mobile_now {
            spans.push(Span::raw("  "));
            let glyph = spinner(app);
            spans.push(Span::styled(format!("{glyph} "), Style::new().fg(p.success.c())));
            spans.extend(shimmer("working", p.dim, p.success, seconds(app)));
            spans.push(Span::styled(
                format!(" {}", format_elapsed(&s.started_at, None, now_ms())),
                Style::new().fg(p.dim.c()),
            ));
        }
    }
    // Right side: context meter, live count, update badge.
    let mut right: Vec<Span> = Vec::new();
    if let Some((_, ratio)) = session
        .as_ref()
        .and_then(|s| context_usage(app, s))
        .as_ref()
        .and_then(context_ratio)
    {
        let ratio = ratio as f32;
        app.meter_anim += (ratio - app.meter_anim) * 0.15;
        if (ratio - app.meter_anim).abs() > 0.005 {
            app.animate(TRANSITION);
        } else {
            app.meter_anim = ratio;
        }
        let cells = 10;
        let filled = app.meter_anim * cells as f32;
        let colour = if ratio > 0.85 {
            p.danger
        } else if ratio > 0.6 {
            p.warning
        } else {
            p.accent
        };
        right.push(Span::styled("ctx ", Style::new().fg(p.dim.c())));
        for i in 0..cells {
            let amount = (filled - i as f32).clamp(0.0, 1.0);
            right.push(Span::styled(
                if amount > 0.5 { "▰" } else { "▱" },
                Style::new().fg(p.border.mix(colour, amount.max(0.15)).c()),
            ));
        }
        right.push(Span::styled(format!(" {}%  ", (ratio * 100.0).round()), Style::new().fg(p.dim.c())));
    }
    let live = app.sessions.iter().filter(|s| is_live(s.status)).count();
    if live > 0 {
        let glow = if any_working(app) {
            p.success.mix(p.background, 0.5 * pulse(app, 2.4))
        } else {
            p.success
        };
        right.push(Span::styled("● ", Style::new().fg(glow.c())));
        right.push(Span::styled(format!("{live} live "), Style::new().fg(p.text.c())));
    }
    if app.show_frames {
        right.push(Span::styled(format!(" f{} ", app.frames), Style::new().fg(p.dim.c())));
    }
    if let Some(version) = &app.update {
        right.push(Span::styled(
            format!(" ↑ {version} "),
            Style::new().fg(p.background.c()).bg(p.accent.c()).bold(),
        ));
    }
    let mut right_width: u16 = right.iter().map(|s| s.content.width() as u16).sum();
    let left_needed: u16 = spans.iter().take(essential).map(|s| s.content.width() as u16).sum();
    // Narrow screens drop the context meter before the project name.
    if right_width + left_needed + 2 > area.width
        && let Some(start) = right.iter().position(|s| s.content == "ctx ")
    {
        right.drain(start..start + 12);
        right_width = right.iter().map(|s| s.content.width() as u16).sum();
    }
    let left_room = area.width.saturating_sub(right_width + 2) as usize;
    let mut used = 0;
    let mut left = Vec::new();
    for span in spans {
        let w = span.content.width();
        if used + w > left_room {
            let keep: String = span.content.chars().take(left_room.saturating_sub(used + 1)).collect();
            if !keep.is_empty() {
                left.push(Span::styled(keep + "…", span.style));
            }
            break;
        }
        used += w;
        left.push(span);
    }
    frame.render_widget(Paragraph::new(Line::from(left)).style(Style::new().bg(p.panel.c())), area);
    if right_width < area.width {
        frame.render_widget(
            Paragraph::new(Line::from(right)).style(Style::new().bg(p.panel.c())),
            Rect {
                x: area.right() - right_width - 1,
                width: right_width + 1,
                ..area
            },
        );
    }
}

/// The session's token usage, with the context snapshot recovered from its
/// events when the controller never recorded one.
fn context_usage(app: &App, session: &Session) -> Option<ruddr_core::state::TokenUsage> {
    let mut usage = session.token_usage.clone().unwrap_or_default();
    if usage.context_tokens.is_none()
        && app.sources.scope.0 == session.state_dir
        && let Some(context) = app.sources.transcript.context
    {
        usage.context_tokens = Some(context.tokens);
        if let Some(window) = context.window {
            usage.context_window = window;
        }
    }
    if session.token_usage.is_none() && usage.context_tokens.is_none() {
        None
    } else {
        Some(usage)
    }
}

fn draw_sessions(frame: &mut Frame, app: &mut App, area: Rect, focused: bool) {
    let p = *app.palette();
    let visible: Vec<Session> = app.visible().into_iter().cloned().collect();
    let live = visible.iter().filter(|s| is_live(s.status)).count();
    let history = app.history.as_ref().map(|h| h.loading && h.loaded_at.is_none());
    let header = match (history, app.filter.is_empty()) {
        (_, false) => format!("{} · /{}", if history.is_some() { "history" } else { "sessions" }, app.filter),
        (Some(true), true) => "history · loading…".to_string(),
        (Some(false), true) => format!("history · every agent · {}", visible.len()),
        (None, true) => format!("sessions · {live} live · {}", visible.len()),
    };
    let block = panel(&p, title(&p, header, focused), focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if visible.is_empty() {
        let scratch = std::env::current_dir().unwrap_or_default().join(".scratch");
        let text = if history == Some(true) {
            "Reading Codex, Claude, Pi,\nOpenCode, and Droid sessions…".to_string()
        } else if history.is_some() && app.filter.is_empty() {
            "No agent sessions found.\n\nPress H to go back to\nRuddr sessions.".to_string()
        } else if app.filter.is_empty() {
            format!(
                "No sessions yet.\n\nWatching the global registry\nand {}.\n\nPress n to start one.",
                crate::app::short_path(&scratch)
            )
        } else {
            "Nothing matches the filter.".to_string()
        };
        frame.render_widget(
            Paragraph::new(text).style(Style::new().fg(p.dim.c())).centered(),
            Rect {
                y: inner.y + inner.height / 3,
                ..inner
            },
        );
        return;
    }
    let per = 2usize;
    let rows = (inner.height as usize / per).max(1);
    let selected = app.selected_index().unwrap_or(0);
    if selected < app.list_offset {
        app.list_offset = selected;
    } else if selected >= app.list_offset + rows {
        app.list_offset = selected + 1 - rows;
    }
    app.list_offset = app.list_offset.min(visible.len().saturating_sub(rows));
    // The highlight glides toward the selected row.
    app.sel_anim += (selected as f32 - app.sel_anim) * 0.45;
    if (app.sel_anim - selected as f32).abs() < 0.05 {
        app.sel_anim = selected as f32;
    } else {
        app.animate(TRANSITION);
    }
    let now = now_ms();
    let highlight_row = ((app.sel_anim - app.list_offset as f32) * per as f32).round() as i32;
    for (slot, session) in visible.iter().enumerate().skip(app.list_offset).take(rows) {
        let y = inner.y + ((slot - app.list_offset) * per) as u16;
        let rect = Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: per as u16,
        };
        app.hits.push((rect, Hit::Session(slot)));
        let colour = status_color(&p, session.status);
        let mut bg = p.background;
        if let Some((_, changed)) = app.seen.get(&session.state_dir) {
            let flash = 1.0 - progress(*changed, 1600);
            if flash > 0.0 {
                bg = bg.mix(colour, 0.35 * flash);
                app.animate(Duration::from_millis(33));
            }
        }
        let glyph = match session.status {
            Status::Active | Status::Starting => spinner(app).to_string(),
            status => status_glyph(status).to_string(),
        };
        let age = format_age(&session.updated_at, now);
        clock_tick(app, &session.updated_at, now, false);
        let info = app.history.as_ref().and_then(|h| h.infos.get(&session.state_dir));
        let name = match info {
            Some(info) if !info.title.is_empty() => info.title.clone(),
            _ => project_name(session),
        };
        let room = (inner.width as usize).saturating_sub(age.len() + 5);
        let name: String = if name.chars().count() > room {
            name.chars().take(room.saturating_sub(1)).collect::<String>() + "…"
        } else {
            name
        };
        let gap = (inner.width as usize).saturating_sub(name.width() + age.len() + 4);
        let line1 = Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{glyph} "), Style::new().fg(colour.c())),
            Span::styled(name, Style::new().fg(p.text.c()).bold()),
            Span::raw(" ".repeat(gap)),
            Span::styled(age, Style::new().fg(p.dim.c())),
        ]);
        let mut meta = match info {
            Some(_) => format!("    {} · {}", provider(session), project_name(session)),
            None => format!("    {} · {}", provider(session), opt(&session.model).unwrap_or("default")),
        };
        if let Some(usage) = &session.token_usage
            && usage.total_tokens > 0
        {
            meta.push_str(&format!(" · {}", format_token_count(usage.total_tokens)));
        }
        let line2 = Line::from(Span::styled(meta, Style::new().fg(p.dim.c())));
        frame.render_widget(Paragraph::new(vec![line1, line2]).style(Style::new().bg(bg.c())), rect);
    }
    // Draw the gliding highlight over the rows it covers.
    let selection_bg = if focused { p.selected } else { p.selected.mix(p.background, 0.45) };
    for offset in 0..per as i32 {
        let row = highlight_row + offset;
        if row < 0 || row >= inner.height as i32 {
            continue;
        }
        let y = inner.y + row as u16;
        let buf = frame.buffer_mut();
        for x in inner.left()..inner.right() {
            buf[(x, y)].set_bg(selection_bg.c());
        }
        buf[(inner.x, y)].set_symbol("▌").set_fg(p.accent.c());
    }
    // Scroll indicator.
    if visible.len() > rows {
        let track = inner.height as f32;
        let thumb = (track * rows as f32 / visible.len() as f32).max(1.0);
        let top = track * app.list_offset as f32 / visible.len() as f32;
        let buf = frame.buffer_mut();
        for i in 0..inner.height {
            let on = (i as f32) >= top.floor() && (i as f32) < (top + thumb).ceil();
            buf[(area.right() - 1, inner.y + i)]
                .set_symbol(if on { "┃" } else { "│" })
                .set_fg(if on { p.accent.mix(p.border, 0.4).c() } else { p.border.c() });
        }
    }
}

fn draw_main(frame: &mut Frame, app: &mut App, area: Rect) {
    let p = *app.palette();
    let session = app.current().cloned().map(|mut s| {
        s.token_usage = context_usage(app, &s);
        s
    });
    if let Some(s) = session.as_ref().filter(|s| s.completed_at.is_none() && !s.status.is_terminal()) {
        clock_tick(app, &s.started_at, now_ms(), true);
    }
    let details: Vec<Line> = match (&session, app.details, app.layout()) {
        (Some(s), Details::Full, Layout::Classic) => detail_lines(&p, s, false),
        (Some(s), Details::Compact, Layout::Classic) => detail_lines(&p, s, true),
        (Some(s), d, Layout::Beta) if d != Details::Hidden => vec![compact_line(&p, s)],
        _ => vec![],
    };
    let details_height = match app.layout() {
        Layout::Classic if !details.is_empty() => details.len() as u16 + 2,
        Layout::Beta if !details.is_empty() => 1,
        _ => 0,
    };
    let [details_area, tabs_area, artifact_area] =
        Split::vertical([Constraint::Length(details_height), Constraint::Length(2), Constraint::Min(3)]).areas(area);
    if details_height > 0 {
        if app.layout() == Layout::Classic {
            frame.render_widget(
                Paragraph::new(details).block(panel(&p, title(&p, "details", false), false)),
                details_area,
            );
        } else {
            frame.render_widget(Paragraph::new(details), details_area);
        }
    }
    draw_tabs(frame, app, tabs_area);
    draw_artifact(frame, app, artifact_area);
}

fn compact_line<'a>(p: &Palette, s: &Session) -> Line<'a> {
    let mut spans = vec![
        Span::styled(
            format!(" {} ", status_glyph(s.status)),
            Style::new().fg(status_color(p, s.status).c()),
        ),
        Span::styled(s.status.to_string(), Style::new().fg(status_color(p, s.status).c())),
        Span::styled(
            format!(" · {}", format_elapsed(&s.started_at, s.completed_at.as_deref(), now_ms())),
            Style::new().fg(p.dim.c()),
        ),
    ];
    if let Some(usage) = &s.token_usage {
        let text = format_token_usage(usage);
        if !text.is_empty() {
            spans.push(Span::styled(format!(" · {text}"), Style::new().fg(p.dim.c())));
        }
    }
    Line::from(spans)
}

fn detail_lines<'a>(p: &Palette, s: &Session, compact: bool) -> Vec<Line<'a>> {
    session_details(s, now_ms())
        .into_iter()
        .filter(|(k, _)| !compact || matches!(k.as_str(), "status" | "model" | "tokens" | "error"))
        .map(|(k, v)| {
            let colour = match k.as_str() {
                "error" => p.danger,
                "status" => status_color(p, s.status),
                _ => p.text,
            };
            Line::from(vec![
                Span::styled(format!(" {k:<9}"), Style::new().fg(p.dim.c())),
                Span::styled(v, Style::new().fg(colour.c())),
            ])
        })
        .collect()
}

fn draw_tabs(frame: &mut Frame, app: &mut App, area: Rect) {
    let p = *app.palette();
    let mut x = area.x + 1;
    let mut spans = vec![Span::raw(" ")];
    let mut target = (0.0, 0.0);
    let (added, removed): (u32, u32) = app.sources.diff.files.iter().fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
    for tab in Tab::ALL {
        let mut label = format!(" {} {} ", tab.index() + 1, tab.title());
        if tab == Tab::Diff && !app.sources.diff.files.is_empty() {
            label = format!(" 4 diff +{added} −{removed} ");
        }
        let width = label.width() as u16;
        let active = tab == app.tab;
        if active {
            target = (x as f32, width as f32);
        }
        app.hits.push((
            Rect {
                x,
                y: area.y,
                width,
                height: 2,
            },
            Hit::Tab(tab),
        ));
        spans.push(Span::styled(
            label,
            if active {
                Style::new().fg(p.accent.c()).bold()
            } else {
                Style::new().fg(p.dim.c())
            },
        ));
        spans.push(Span::raw(" "));
        x += width + 1;
    }
    if app.tab_anim.1 == 0.0 {
        app.tab_anim = target;
    }
    // The underline slides and stretches toward the active tab.
    app.tab_anim.0 += (target.0 - app.tab_anim.0) * 0.35;
    app.tab_anim.1 += (target.1 - app.tab_anim.1) * 0.35;
    app.tab_settling = (app.tab_anim.0 - target.0).abs() > 0.3 || (app.tab_anim.1 - target.1).abs() > 0.3;
    if app.tab_settling {
        app.animate(TRANSITION);
    } else {
        app.tab_anim = target;
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), Rect { height: 1, ..area });
    {
        let buf = frame.buffer_mut();
        let y = area.y + 1;
        for cx in area.left()..area.right() {
            buf[(cx, y)].set_symbol("─").set_fg(p.border.c());
        }
        let start = app.tab_anim.0.round() as u16;
        let end = (app.tab_anim.0 + app.tab_anim.1).round() as u16;
        for cx in start.max(area.left())..end.min(area.right()) {
            buf[(cx, y)].set_symbol("━").set_fg(p.accent.c());
        }
    }
    // Follow state on the right of the tab strip.
    let hint = if app.follow { "● live" } else { "‖ paused · G" };
    let working = app.current().is_some_and(|s| s.status == Status::Active);
    let colour = if !app.follow {
        p.warning
    } else if working {
        p.success.mix(p.background, 0.4 * pulse(app, 2.0))
    } else {
        p.success
    };
    let w = hint.width() as u16;
    if area.width > x - area.x + w + 2 {
        frame.render_widget(
            Paragraph::new(Span::styled(hint, Style::new().fg(colour.c()))),
            Rect {
                x: area.right() - w - 1,
                y: area.y,
                width: w,
                height: 1,
            },
        );
    }
}

fn draw_artifact(frame: &mut Frame, app: &mut App, area: Rect) {
    let p = *app.palette();
    let focused = app.focus == Focus::Artifact && !(app.layout() == Layout::Beta && app.drawer);
    app.hits.push((area, Hit::Artifact));
    if app.tab == Tab::Trace {
        app.sync_activity();
    }
    // Diff file tree on wide screens.
    let mut body = area;
    if app.tab == Tab::Diff && !app.mobile_now && !app.sources.diff.files.is_empty() && area.width >= 80 {
        let wanted = match (app.tree_ratio, app.tree_width) {
            (Some(r), _) => (r * area.width as f64).round() as u16,
            (None, Some(w)) => w,
            _ => 30,
        };
        let width = wanted.clamp(20, 60.min(area.width.saturating_sub(40)).max(20));
        app.diff_area = area;
        let [tree, rest] = Split::horizontal([Constraint::Length(width), Constraint::Min(20)]).areas(area);
        draw_tree(frame, app, tree, &p);
        body = rest;
    }
    let block = panel(&p, Line::default(), focused).borders(Borders::ALL);
    let inner = block.inner(body);
    app.artifact_inner = inner;
    let query = app.artifact_query.get(&app.tab).cloned().unwrap_or_default();
    let mark = Style::new().bg(p.warning.mix(p.background, 0.55).c()).fg(p.text.c());
    let current_mark = Style::new().bg(p.warning.c()).fg(p.background.c()).bold();
    let width = inner.width.saturating_sub(1);

    // Render every block through the cache; only changed blocks re-render.
    app.blocks = view::build_blocks(app);
    if app.cursor.is_some_and(|c| c >= app.blocks.len()) {
        app.cursor = None;
    }
    let mut cache = std::mem::take(&mut app.cache);
    cache.scope(&format!("{}\u{0}{:?}", app.sources.scope.0, app.tab));
    cache.begin_frame();
    let query_hash = if query.is_empty() { 0 } else { hash_query(&query) };
    let mut spans = Vec::with_capacity(app.blocks.len());
    let mut total = 0usize;
    for (index, b) in app.blocks.iter().enumerate() {
        let current = !query.is_empty() && app.cursor == Some(index);
        let key = Key {
            version: b.version,
            width,
            theme: app.theme,
            query: query_hash,
            current,
            anim: b.anim,
        };
        let lines = cache.lines(b.id, key, &query, if current { current_mark } else { mark }, || {
            view::render_block(app, b, &p)
        });
        spans.push((total, lines.len()));
        total += lines.len();
    }
    app.block_spans = spans;
    let height = inner.height as usize;
    let max = total.saturating_sub(height);
    app.artifact_rows = total;
    app.artifact_height = height;
    if app.follow {
        app.scroll_target = max;
        app.unseen_base = None;
    } else {
        if app.unseen_base.is_none() {
            app.unseen_base = Some(total);
        }
        app.scroll_target = app.scroll_target.min(max);
    }
    if app.reveal_cursor {
        app.reveal_cursor = false;
        if let Some((first, len)) = app.cursor.and_then(|c| app.block_spans.get(c).copied()).filter(|(_, len)| *len > 0) {
            let last = first + len - 1;
            if first < app.scroll_target {
                app.scroll_target = first.saturating_sub(1);
            } else if last >= app.scroll_target + height {
                app.scroll_target = (last + 2).saturating_sub(height).min(first.saturating_sub(1)).min(max);
            }
        }
    }
    // Ease toward the scroll target; big jumps settle in a few frames.
    let target = app.scroll_target as f32;
    app.scroll_pos += (target - app.scroll_pos) * 0.4;
    if (app.scroll_pos - target).abs() < 0.5 {
        app.scroll_pos = target;
    } else {
        app.animate(TRANSITION);
    }
    let scroll = (app.scroll_pos.round() as usize).min(max);

    // Collect only the rows on screen.
    let bar = if focused { p.selected } else { p.selected.mix(p.background, 0.5) };
    let mut lines: Vec<Line> = Vec::with_capacity(height);
    let mut screen_blocks = Vec::with_capacity(height);
    let mut animated = false;
    for (index, (start, len)) in app.block_spans.iter().enumerate() {
        if start + len <= scroll || *start >= scroll + height {
            continue;
        }
        let b = &app.blocks[index];
        animated |= view::animates(b);
        let Some(cached) = cache.peek(b.id) else { continue };
        let from = scroll.saturating_sub(*start);
        let to = (*len).min(scroll + height - start);
        for line in &cached[from..to] {
            let mut line = line.clone();
            if app.cursor == Some(index) {
                line.style = line.style.bg(bar.c());
                for span in &mut line.spans {
                    if span.style.bg.is_some() {
                        span.style.bg = Some(bar.c());
                    }
                }
            }
            lines.push(line);
            screen_blocks.push(Some(index));
        }
    }
    app.cache = cache;
    app.screen_blocks = screen_blocks;
    if animated {
        tick(app);
    }

    let mut title_spans = vec![Span::styled(
        format!(" {} ", app.tab.title()),
        Style::new().fg(if focused { p.accent.c() } else { p.dim.c() }).bold(),
    )];
    if !query.is_empty() {
        let count = app.match_count(&query);
        title_spans.push(Span::styled(format!("/{query} · {count} "), Style::new().fg(p.warning.c())));
    }
    frame.render_widget(Paragraph::new(lines).block(block.title(Line::from(title_spans))), body);
    // Scrollbar.
    if total > height && inner.height > 2 {
        let track = inner.height as f32;
        let thumb = (track * height as f32 / total as f32).max(1.0);
        let top = (track - thumb) * scroll as f32 / max.max(1) as f32;
        let buf = frame.buffer_mut();
        for i in 0..inner.height {
            let on = (i as f32) >= top.floor() && (i as f32) < (top + thumb).ceil();
            if on {
                buf[(body.right() - 1, inner.y + i)]
                    .set_symbol("┃")
                    .set_fg(p.accent.mix(p.border, 0.3).c());
            }
        }
    }
    // "New below" pill when the user scrolled away from a live stream.
    if let Some(base) = app.unseen_base {
        let unseen = total.saturating_sub(base);
        if unseen > 0 && !app.follow {
            let label = format!(" ↓ {unseen} new · G ");
            let w = label.width() as u16;
            let rect = Rect {
                x: body.x + body.width.saturating_sub(w) / 2,
                y: body.bottom().saturating_sub(2),
                width: w.min(body.width),
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Span::styled(label, Style::new().fg(p.background.c()).bg(p.accent.c()).bold())),
                rect,
            );
        }
    }
}

fn draw_tree(frame: &mut Frame, app: &mut App, area: Rect, p: &Palette) {
    let files = app.sources.diff.files.clone();
    let block = panel(
        p,
        title(p, format!("files · {}", files.len()), app.dragging_tree),
        app.dragging_tree,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let entries = diff_tree(&files, &app.collapsed_dirs);
    // The file that owns the cursor row.
    let current = app.cursor.and_then(|c| {
        app.blocks
            .get(..=c.min(app.blocks.len().saturating_sub(1)))?
            .iter()
            .rev()
            .find_map(|b| b.diff_header.clone())
    });
    let current_row = entries
        .iter()
        .position(|e| matches!(e, TreeEntry::File { index, .. } if Some(&files[*index].path) == current.as_ref()));
    let rows = inner.height as usize;
    let offset = current_row
        .map(|r| r.saturating_sub(rows.saturating_sub(1)))
        .unwrap_or(0)
        .min(entries.len().saturating_sub(rows));
    for (row, entry) in entries.iter().enumerate().skip(offset).take(rows) {
        let y = inner.y + (row - offset) as u16;
        let rect = Rect { y, height: 1, ..inner };
        app.hits.push((rect, Hit::TreeRow(row)));
        let line = match entry {
            TreeEntry::Dir { name, depth, expanded, .. } => Line::from(vec![
                Span::raw("  ".repeat(*depth)),
                Span::styled(if *expanded { "▾ " } else { "▸ " }, Style::new().fg(p.dim.c())),
                Span::styled(format!("{name}/"), Style::new().fg(p.dim.c())),
            ]),
            TreeEntry::File { index, name, depth } => {
                let file = &files[*index];
                let touched = app.sources.diff.touched.contains(&file.path);
                let selected = current.as_deref() == Some(&file.path);
                let stats = format!("{} +{} −{} ", file.status, file.added, file.removed);
                let indent = "  ".repeat(*depth);
                let room = (inner.width as usize).saturating_sub(stats.width() + indent.width() + 3);
                let name: String = if name.width() > room {
                    name.chars().take(room.saturating_sub(1)).collect::<String>() + "…"
                } else {
                    name.clone()
                };
                let gap = (inner.width as usize)
                    .saturating_sub(indent.width() + name.width() + stats.width() + 2)
                    .max(1);
                let status_colour = match file.status {
                    'A' => p.success,
                    'D' => p.danger,
                    'R' => p.warning,
                    _ => p.dim,
                };
                let folded = app.folded.contains(&file.path);
                Line::from(vec![
                    Span::raw(indent),
                    Span::styled(
                        if touched {
                            "●"
                        } else if folded {
                            "▸"
                        } else {
                            " "
                        },
                        Style::new().fg(if touched { p.accent.c() } else { p.dim.c() }),
                    ),
                    Span::raw(" "),
                    Span::styled(name, Style::new().fg(if selected { p.accent.c() } else { p.text.c() })),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(format!("{} ", file.status), Style::new().fg(status_colour.c())),
                    Span::styled(format!("+{}", file.added), Style::new().fg(p.success.c())),
                    Span::styled(format!(" −{} ", file.removed), Style::new().fg(p.danger.c())),
                ])
                .style(if selected { Style::new().bg(p.selected.c()) } else { Style::new() })
            }
        };
        frame.render_widget(Paragraph::new(line), rect);
    }
    app.tree_entries = entries;
    // The right border drags to resize the sidebar.
    app.hits.push((
        Rect {
            x: area.right().saturating_sub(1),
            width: 1,
            ..area
        },
        Hit::TreeDivider,
    ));
}

fn help_segments(app: &App) -> Vec<(String, String)> {
    let session = app.current();
    let can_stop = session.is_some_and(|s| stoppable(s.status));
    let drawer = app.layout() == Layout::Beta && app.drawer;
    let sessions = app.focus == Focus::Sessions || drawer;
    let mut segments: Vec<(&str, &str)> = if sessions {
        if app.layout() == Layout::Classic {
            vec![("j/k", "select"), ("/", "filter"), ("Tab", "chat"), ("R-click", "menu")]
        } else {
            vec![("j/k", "select"), ("/", "filter"), ("Enter", "open"), ("Esc", "close")]
        }
    } else if app.tab == Tab::Diff {
        vec![
            ("]c ]f", "next hunk/file"),
            ("[c [f", "previous"),
            ("Enter", "fold"),
            ("Z", "fold all"),
        ]
    } else if app.tab == Tab::Trace {
        vec![("j/k", "rows"), ("Enter", "expand"), ("c", "copy"), ("/", "search")]
    } else {
        vec![("j/k", "rows"), ("c", "copy"), ("/", "search")]
    };
    if app.artifact_query.get(&app.tab).is_some_and(|q| !q.is_empty()) && !sessions {
        segments.push(("n/N", "matches"));
    }
    segments.extend([("s", "prompt"), ("n", "new"), ("m", "model")]);
    if app.deja_available {
        segments.push(("f", "find"));
    }
    if can_stop {
        segments.push(("x x", "stop"));
    }
    segments.extend([(":", "commands"), ("t", "theme"), ("?", "help"), ("q", "quit")]);
    segments.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn draw_footer(frame: &mut Frame, app: &mut App, area: Rect) {
    let p = *app.palette();
    if let Some(target) = app.search.as_ref().map(|s| s.target) {
        let (label, hint) = match target {
            SearchTarget::Sessions => ("filter", "Enter keep · Esc clear"),
            SearchTarget::Artifact => ("search", "Enter jump · n/N matches · Esc clear"),
            SearchTarget::Deja => ("deja find", "Enter search · Esc cancel"),
        };
        let started = app.started;
        let caret = if blink(app, started) { "▏" } else { " " };
        let text = app.search.as_ref().map(|s| s.text.clone()).unwrap_or_default();
        let line = Line::from(vec![
            Span::styled(format!(" {label} "), Style::new().fg(p.background.c()).bg(p.accent.c()).bold()),
            Span::styled(" › ", Style::new().fg(p.accent.c())),
            Span::styled(text, Style::new().fg(p.text.c())),
            Span::styled(caret, Style::new().fg(p.accent.c())),
            Span::styled(format!("   {hint}"), Style::new().fg(p.dim.c())),
        ]);
        frame.render_widget(Paragraph::new(line).style(Style::new().bg(p.panel.c())), area);
        return;
    }
    if let Some((_, armed)) = app.stop_armed.clone() {
        app.animate(Duration::from_millis(50));
        let remaining = 1.0 - progress(armed, 2000);
        let cells = 12;
        let filled = (remaining * cells as f32).round() as usize;
        let line = Line::from(vec![
            Span::styled(
                " ■ press x again to stop ",
                Style::new().fg(p.background.c()).bg(p.danger.c()).bold(),
            ),
            Span::styled(
                format!(" {}{}", "━".repeat(filled), " ".repeat(cells - filled)),
                Style::new().fg(p.danger.c()),
            ),
        ]);
        frame.render_widget(Paragraph::new(line).style(Style::new().bg(p.panel.c())), area);
        return;
    }
    let mut spans = vec![Span::raw(" ")];
    let mut width = 1;
    for (key, label) in help_segments(app) {
        let piece = key.width() + label.width() + 4;
        if width + piece > area.width as usize {
            break;
        }
        width += piece;
        spans.push(Span::styled(key, Style::new().fg(p.accent.c()).bold()));
        spans.push(Span::styled(format!(" {label}"), Style::new().fg(p.dim.c())));
        spans.push(Span::styled(" · ", Style::new().fg(p.border.c())));
    }
    spans.pop();
    frame.render_widget(Paragraph::new(Line::from(spans)).style(Style::new().bg(p.panel.c())), area);
}

/// The mobile action bar: the keys a phone cannot press, as four tappable
/// buttons, each three rows tall and taking the tap anywhere on its box.
fn draw_action_bar(frame: &mut Frame, app: &mut App, area: Rect) {
    let p = *app.palette();
    let can_stop = app.current().is_some_and(|s| stoppable(s.status));
    let promptable = app.current().is_some_and(|s| prompt_route(s).is_some());
    let count = app.visible().len();
    let sessions = format!("≡ {count}");
    let buttons: Vec<(&str, Cmd, Rgb, bool)> = vec![
        (sessions.as_str(), Cmd::Sessions, p.accent, true),
        (
            if promptable { "✎ prompt" } else { "✎ new" },
            if promptable { Cmd::Prompt } else { Cmd::New },
            p.accent,
            true,
        ),
        // Stop arms first and needs a second tap, like `x x`.
        ("■ stop", Cmd::Stop, if can_stop { p.danger } else { p.dim }, can_stop),
        ("⋯ more", Cmd::Palette, p.accent, true),
    ];
    app.buttons = buttons.iter().map(|b| b.1.clone()).collect();
    let n = buttons.len() as u16;
    let gap = 1;
    let width = area.width.saturating_sub(gap * (n - 1)) / n;
    for (i, (label, _, colour, enabled)) in buttons.iter().enumerate() {
        let x = area.x + i as u16 * (width + gap);
        let w = if i as u16 == n - 1 { area.right().saturating_sub(x) } else { width };
        let rect = Rect {
            x,
            y: area.y,
            width: w,
            height: 3,
        };
        app.hits.push((rect, Hit::Button(i)));
        let surface = if *enabled { p.background.mix(p.accent, 0.18) } else { p.panel };
        let lines = vec![
            Line::raw(""),
            Line::from(Span::styled(label.to_string(), Style::new().fg(colour.c()).bold())),
        ];
        frame.render_widget(Paragraph::new(lines).centered().style(Style::new().bg(surface.c())), rect);
    }
}

fn draw_toasts(frame: &mut Frame, app: &mut App, area: Rect) {
    let p = *app.palette();
    let mut y = area.bottom();
    let mut wake: Option<Duration> = None;
    let mut want = |d: Duration| wake = Some(wake.map_or(d, |w| w.min(d)));
    for toast in app.toasts.iter().rev() {
        let age = toast.born.elapsed();
        let life = toast.lifetime();
        let enter = ease_out(age.as_secs_f32() / 0.22);
        let exit = 1.0 - ((age.as_secs_f32() - (life.as_secs_f32() - 0.5)) / 0.5).clamp(0.0, 1.0);
        // Sliding in and fading out move every frame; otherwise only the
        // lifetime bar moves, one cell at a time.
        if enter < 1.0 || exit < 1.0 {
            want(TRANSITION);
        } else {
            want(Duration::from_millis(150));
        }
        let (glyph, colour) = match toast.kind {
            Kind::Success => ("✓", p.success),
            Kind::Warning => ("!", p.warning),
            Kind::Error => ("✗", p.danger),
            Kind::Info => ("›", p.accent),
        };
        let max_width = (area.width as usize).saturating_sub(4).min(72);
        let text: String = if toast.text.width() > max_width.saturating_sub(5) {
            toast.text.chars().take(max_width.saturating_sub(6)).collect::<String>() + "…"
        } else {
            toast.text.clone()
        };
        let first_line = text.lines().next().unwrap_or("").to_string();
        let width = (first_line.width() as u16 + 5).min(area.width);
        let height = 3;
        if y < area.y + height {
            break;
        }
        y -= height;
        // Slides in from the right edge, fades out at the end.
        let offset = ((1.0 - enter) * (width as f32 + 2.0)).round() as u16;
        let x = area.right().saturating_sub(width + 1) + offset;
        if x >= area.right() {
            continue;
        }
        let visible_width = width.min(area.right() - x);
        let rect = Rect {
            x,
            y,
            width: visible_width,
            height,
        };
        let bg = p.panel;
        let fg = bg.mix(p.text, exit);
        let edge = bg.mix(colour, exit);
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(edge.c()))
            .style(Style::new().bg(bg.c()));
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("{glyph} "), Style::new().fg(edge.c()).bold()),
                Span::styled(first_line, Style::new().fg(fg.c())),
            ]))
            .block(block),
            rect,
        );
        // Lifetime bar along the bottom border.
        let remaining = 1.0 - age.as_secs_f32() / life.as_secs_f32();
        let bar = ((visible_width.saturating_sub(2)) as f32 * remaining).round() as u16;
        let buf = frame.buffer_mut();
        for i in 0..bar {
            if x + 1 + i < area.right() {
                buf[(x + 1 + i, y + 2)].set_symbol("─").set_fg(edge.mix(bg, 0.3).c());
            }
        }
    }
    if let Some(after) = wake {
        app.animate(after);
    }
}

/// Grows a modal from 85% to full size as it opens.
fn modal_rect(screen: Rect, width: u16, height: u16, opened: Instant, anchor: Option<(u16, u16)>) -> Rect {
    let t = ease_out(progress(opened, 170));
    let scale = 0.85 + 0.15 * t;
    let w = ((width.min(screen.width.saturating_sub(2))) as f32 * scale).round().max(4.0) as u16;
    let h = ((height.min(screen.height.saturating_sub(2))) as f32 * (0.7 + 0.3 * t))
        .round()
        .max(3.0) as u16;
    match anchor {
        Some((x, y)) => Rect {
            x: x.min(screen.right().saturating_sub(w + 1)),
            y: y.min(screen.bottom().saturating_sub(h + 1)),
            width: w,
            height: h,
        },
        None => Rect {
            x: screen.x + screen.width.saturating_sub(w) / 2,
            y: screen.y + screen.height.saturating_sub(h) / 3,
            width: w,
            height: h,
        },
    }
}

fn draw_prompt(frame: &mut Frame, app: &mut App, screen: Rect) {
    let p = *app.palette();
    let Some(prompt) = &app.prompt else { return };
    let (opened, typed) = (prompt.opened, prompt.typed);
    let (label, colour) = match prompt.kind {
        PromptKind::Route(PromptRoute::Steer) => ("steer the active turn", p.warning),
        PromptKind::Route(PromptRoute::Prompt) => ("next turn", p.accent),
        PromptKind::Route(PromptRoute::Continue) => ("continue in a new run", p.success),
        PromptKind::New if prompt.resume.is_some() => ("resume a past session", p.success),
        PromptKind::New => ("new session", p.success),
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let place = match &prompt.target {
        Some(t) => project_name(t),
        None => cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
    };
    let model = prompt
        .model
        .as_ref()
        .map(|m| m.name())
        .or_else(|| prompt.target.as_ref().and_then(|t| opt(&t.model).map(String::from)))
        .unwrap_or_else(|| "default model".into());
    let effort = prompt.effort.clone().or_else(|| {
        if prompt.model.is_none() {
            prompt.target.as_ref().and_then(|t| t.effort.clone())
        } else {
            None
        }
    });

    let width = screen.width.saturating_sub(4).min(96);
    let inner_width = width.saturating_sub(4).max(8) as usize;
    // Hard-wrap the draft and find the caret.
    let mut lines: Vec<Vec<char>> = vec![vec![]];
    let mut caret = (0usize, 0usize);
    for (i, c) in prompt.text.iter().enumerate() {
        if i == prompt.cursor {
            caret = (lines.len() - 1, lines.last().unwrap().len());
        }
        if *c == '\n' {
            lines.push(vec![]);
            continue;
        }
        if lines.last().unwrap().len() >= inner_width {
            lines.push(vec![]);
        }
        lines.last_mut().unwrap().push(*c);
    }
    if prompt.cursor >= prompt.text.len() {
        let last = lines.last().unwrap().len();
        caret = if last >= inner_width {
            (lines.len(), 0)
        } else {
            (lines.len() - 1, last)
        };
        if caret.0 == lines.len() {
            lines.push(vec![]);
        }
    }
    let text_rows = lines.len().clamp(3, 12) as u16;
    let height = text_rows + 5;
    let area = modal_rect(screen, width, height, opened, None);
    // Soft shadow under the modal.
    dim_region(
        frame,
        Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width,
            height: area.height,
        }
        .intersection(screen),
        &p,
        0.5,
    );
    frame.render_widget(Clear, area);
    let open = ease_out(progress(opened, 220));
    let border = p.border.mix(colour, open);
    // Narrow screens keep what fits: the label, then the place, then the
    // model chip; a chip that does not fit moves inside the box.
    let border_room = area.width.saturating_sub(4) as usize;
    let label_text = format!(" {label} ");
    let place_text = format!(" {place} ");
    let show_place = label_text.width() + place_text.width() <= border_room;
    let mut chip_text = format!(" {} · {}", prompt.provider, model);
    if let Some(effort) = &effort {
        chip_text.push_str(&format!(" · {effort}"));
    }
    chip_text.push(' ');
    let used = label_text.width() + if show_place { place_text.width() } else { 0 };
    let chip_on_border = used + chip_text.width() < border_room;
    let mut header = vec![Span::styled(label_text, Style::new().fg(p.background.c()).bg(colour.c()).bold())];
    if show_place {
        header.push(Span::styled(place_text, Style::new().fg(p.text.c())));
    }
    let changeable = !matches!(prompt.kind, PromptKind::Route(PromptRoute::Steer | PromptRoute::Prompt));
    let key = |k: &'static str| Span::styled(k, Style::new().fg(p.accent.c()));
    let note = |t: &'static str| Span::styled(t, Style::new().fg(p.dim.c()));
    let mut footer = vec![key(" enter"), note(" send · ")];
    if area.width >= 60 {
        footer.extend([key("alt+enter"), note(" newline · ")]);
        if changeable {
            footer.extend([key("tab"), note(" model · ")]);
        }
    }
    footer.extend([key("esc"), note(" cancel ")]);
    let footer_width: usize = footer.iter().map(|s| s.content.width()).sum();
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border.c()))
        .style(Style::new().bg(p.panel.c()).fg(p.text.c()))
        .title(Line::from(header))
        .title_bottom(Line::from(footer));
    if chip_on_border {
        block = block.title(Line::from(Span::styled(chip_text.clone(), Style::new().fg(p.dim.c()))).right_aligned());
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut text_area = Rect {
        x: inner.x + 1,
        y: inner.y + 1,
        width: inner.width.saturating_sub(2),
        height: inner.height.saturating_sub(2),
    };
    if !chip_on_border {
        frame.render_widget(
            Paragraph::new(Span::styled(chip_text.trim().to_string(), Style::new().fg(p.dim.c()))),
            Rect { height: 1, ..text_area },
        );
        text_area.y += 1;
        text_area.height = text_area.height.saturating_sub(1);
    }
    let rows = text_area.height as usize;
    let first = caret.0.saturating_sub(rows.saturating_sub(1));
    if prompt.text.is_empty() {
        let hint = match prompt.kind {
            PromptKind::Route(PromptRoute::Steer) => "Redirect the agent mid-turn…",
            PromptKind::New => "Describe the task for the new session…",
            _ => "Type your message…",
        };
        frame.render_widget(Paragraph::new(Span::styled(hint, Style::new().fg(p.dim.c()).italic())), text_area);
    } else {
        let shown: Vec<Line> = lines
            .iter()
            .skip(first)
            .take(rows)
            .map(|l| Line::from(l.iter().collect::<String>()))
            .collect();
        frame.render_widget(Paragraph::new(shown).style(Style::new().fg(p.text.c())), text_area);
    }
    let count = format!(" {} ", prompt.text.len());
    if open < 1.0 {
        app.animate(TRANSITION);
    }
    // A blinking block caret that stays solid while typing.
    let solid = typed.elapsed() < Duration::from_millis(500);
    if solid {
        app.animate(Duration::from_millis(500) - typed.elapsed());
    }
    let show = solid || blink(app, typed);
    if show && caret.0 >= first && caret.0 - first < rows {
        let cx = text_area.x + caret.1 as u16;
        let cy = text_area.y + (caret.0 - first) as u16;
        if cx < text_area.right() {
            let buf = frame.buffer_mut();
            buf[(cx, cy)].set_bg(colour.c()).set_fg(p.background.c());
        }
    }
    // Character counter.
    let cw = count.len() as u16;
    if area.width as usize > footer_width + cw as usize + 6 {
        frame.render_widget(
            Paragraph::new(Span::styled(count, Style::new().fg(p.border.mix(p.dim, 0.5).c()))),
            Rect {
                x: area.right() - cw - 2,
                y: area.bottom() - 1,
                width: cw,
                height: 1,
            },
        );
    }
    app.hits.push((area, Hit::Overlay));
}

fn draw_picker(frame: &mut Frame, app: &mut App, screen: Rect) {
    let p = *app.palette();
    let started = app.started;
    let mut wants_transition = false;
    let caret_on = app.picker.as_ref().is_some_and(|pk| pk.filterable) && blink(app, started);
    let Some(picker) = &mut app.picker else { return };
    let visible = picker.visible();
    if picker.index >= visible.len() {
        picker.index = visible.len().saturating_sub(1);
    }
    picker.sel_anim += (picker.index as f32 - picker.sel_anim) * 0.5;
    if (picker.sel_anim - picker.index as f32).abs() < 0.05 {
        picker.sel_anim = picker.index as f32;
    } else {
        wants_transition = true;
    }
    if picker.opened.elapsed() < Duration::from_millis(250) {
        wants_transition = true;
    }
    let two_line = matches!(picker.kind, PickerKind::Deja);
    let per: u16 = if two_line { 2 } else { 1 };
    let longest = picker
        .items
        .iter()
        .map(|i| i.label.width() + i.key.width() + if two_line { 0 } else { i.hint.width().min(40) } + 10)
        .max()
        .unwrap_or(30) as u16;
    let width = match picker.kind {
        PickerKind::Menu | PickerKind::Confirm => longest.clamp(30, 56).max(picker.title.width() as u16 + 6),
        PickerKind::Theme => 56,
        _ => longest.clamp(44, 92),
    };
    let max_rows = (screen.height.saturating_sub(8) / per).clamp(3, 16);
    let rows = (visible.len() as u16).clamp(1, max_rows);
    let height = rows * per + 2 + if picker.filterable { 2 } else { 0 };
    let area = modal_rect(screen, width, height, picker.opened, picker.anchor);
    if picker.anchor.is_none() {
        dim_region(frame, screen, &p, 0.35 * ease_out(progress(picker.opened, 200)));
    }
    frame.render_widget(Clear, area);
    let danger = picker.kind == PickerKind::Confirm;
    let edge = if danger { p.danger } else { p.accent };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(p.border.mix(edge, ease_out(progress(picker.opened, 220))).c()))
        .style(Style::new().bg(p.panel.c()).fg(p.text.c()))
        .title(Span::styled(format!(" {} ", picker.title), Style::new().fg(edge.c()).bold()));
    let block = if picker.kind == PickerKind::Model {
        block.title_bottom(Line::from(Span::styled(
            " ←/→ effort · enter pick · esc ",
            Style::new().fg(p.dim.c()),
        )))
    } else {
        block
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut hits = vec![(area, Hit::Overlay)];
    let mut list_area = inner;
    if picker.filterable {
        let query = Line::from(vec![
            Span::styled(" › ", Style::new().fg(edge.c())),
            Span::styled(picker.query.clone(), Style::new().fg(p.text.c())),
            Span::styled(if caret_on { "▏" } else { " " }, Style::new().fg(edge.c())),
            Span::styled(
                if picker.query.is_empty() { "type to filter" } else { "" },
                Style::new().fg(p.border.mix(p.dim, 0.5).c()).italic(),
            ),
        ]);
        frame.render_widget(Paragraph::new(query), Rect { height: 1, ..inner });
        let buf = frame.buffer_mut();
        for x in inner.left()..inner.right() {
            buf[(x, inner.y + 1)].set_symbol("─").set_fg(p.border.c());
        }
        list_area = Rect {
            y: inner.y + 2,
            height: inner.height.saturating_sub(2),
            ..inner
        };
    }
    let rows = (list_area.height / per) as usize;
    let offset = picker
        .index
        .saturating_sub(rows.saturating_sub(1))
        .min(visible.len().saturating_sub(rows));
    if visible.is_empty() {
        frame.render_widget(Paragraph::new(Span::styled("  no matches", Style::new().fg(p.dim.c()))), list_area);
    }
    let highlight = ((picker.sel_anim - offset as f32) * per as f32).round() as i32;
    for (slot, &item_index) in visible.iter().enumerate().skip(offset).take(rows) {
        let item = &picker.items[item_index];
        let y = list_area.y + ((slot - offset) as u16) * per;
        let rect = Rect {
            y,
            height: per,
            ..list_area
        };
        hits.push((rect, Hit::PickItem(slot)));
        let selected = slot == picker.index;
        let disabled = item.disabled.is_some();
        let label_colour = if disabled {
            p.border.mix(p.dim, 0.5)
        } else if item.danger {
            p.danger
        } else if selected {
            p.accent
        } else {
            p.text
        };
        let mut spans = vec![Span::raw("  ")];
        if let Some(sw) = &item.swatch {
            for colour in [sw.background, sw.accent, sw.success, sw.warning, sw.danger] {
                spans.push(Span::styled("█", Style::new().fg(colour.c())));
            }
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            item.label.clone(),
            Style::new()
                .fg(label_colour.c())
                .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
        ));
        let mut right = String::new();
        if picker.kind == PickerKind::Model && !item.efforts.is_empty() {
            let effort = picker.effort_for(item_index).cloned().unwrap_or_else(|| "default".into());
            right = if selected { format!("‹ {effort} ›") } else { effort };
        } else if let Some(reason) = &item.disabled {
            if selected {
                right = reason.clone();
            }
        } else if !two_line && !item.hint.is_empty() && picker.kind != PickerKind::Palette {
            right = item.hint.clone();
        }
        let key = if item.key.is_empty() {
            String::new()
        } else {
            format!(" {} ", item.key)
        };
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        let room = (list_area.width as usize).saturating_sub(used + key.width() + 2);
        let right: String = if right.width() > room {
            right.chars().take(room.saturating_sub(1)).collect::<String>() + "…"
        } else {
            right
        };
        let gap = room.saturating_sub(right.width()) + 1;
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(right, Style::new().fg(p.dim.c())));
        if !key.is_empty() {
            spans.push(Span::styled(key, Style::new().fg(p.dim.c()).bg(p.background.c())));
        }
        let mut lines = vec![Line::from(spans)];
        if two_line {
            let hint: String = item.hint.chars().take((list_area.width as usize).saturating_sub(6)).collect();
            lines.push(Line::from(Span::styled(format!("    {hint}"), Style::new().fg(p.dim.c()))));
        }
        frame.render_widget(Paragraph::new(lines), rect);
    }
    for offset_row in 0..per as i32 {
        let row = highlight + offset_row;
        if row < 0 || row >= list_area.height as i32 {
            continue;
        }
        let y = list_area.y + row as u16;
        let buf = frame.buffer_mut();
        for x in list_area.left()..list_area.right() {
            buf[(x, y)].set_bg(p.selected.c());
        }
        buf[(list_area.x, y)].set_symbol("▌").set_fg(edge.c());
    }
    // The selected palette command's hint sits on the bottom border.
    if picker.kind == PickerKind::Palette
        && let Some(item) = picker.selected().map(|i| &picker.items[i])
    {
        let text = item.disabled.clone().unwrap_or_else(|| item.hint.clone());
        if !text.is_empty() {
            let text: String = text.chars().take(area.width.saturating_sub(6) as usize).collect();
            let w = text.width() as u16 + 2;
            frame.render_widget(
                Paragraph::new(Span::styled(format!(" {text} "), Style::new().fg(p.dim.c()).italic())),
                Rect {
                    x: area.x + 2,
                    y: area.bottom() - 1,
                    width: w.min(area.width.saturating_sub(4)),
                    height: 1,
                },
            );
        }
    }
    app.hits.extend(hits);
    if wants_transition {
        app.animate(TRANSITION);
    }
}

fn draw_help(frame: &mut Frame, app: &mut App, screen: Rect) {
    let p = *app.palette();
    let sections: [(&str, &[(&str, &str)]); 4] = [
        (
            "navigate",
            &[
                ("j / k  ↑ / ↓", "select session or row"),
                ("Tab", "switch focus / open sessions"),
                ("1-4  o  h / l", "chat · activity · output · diff"),
                ("g / G", "top / follow live"),
                ("PgUp PgDn ^d ^u", "page"),
                ("/", "filter sessions or search pane"),
                ("n / N", "next / previous match"),
            ],
        ),
        (
            "act",
            &[
                ("s  Enter", "steer, prompt, or continue"),
                ("n", "new session"),
                ("R", "continue thread in a new run"),
                ("m", "choose model + effort"),
                ("f", "find a past session (deja)"),
                ("H", "browse every agent's sessions + diffs"),
                ("x x", "interrupt turn / end idle session"),
                ("D", "delete finished session"),
            ],
        ),
        (
            "rows",
            &[
                ("Enter  click", "expand tool / fold file"),
                ("]c [c  ]f [f", "next / previous hunk or file"),
                ("Z", "fold every diff file"),
                ("< >  drag", "narrow / widen the session list"),
            ],
        ),
        (
            "other",
            &[
                (": ^k", "command palette"),
                ("t", "theme with live preview"),
                ("i", "cycle details"),
                ("c", "copy row (OSC 52)"),
                ("right-click", "session or row menu"),
                ("q", "quit"),
            ],
        ),
    ];
    let mut lines = Vec::new();
    for (name, rows) in sections {
        lines.push(Line::from(Span::styled(format!(" {name}"), Style::new().fg(p.accent.c()).bold())));
        for (k, v) in rows {
            lines.push(Line::from(vec![
                Span::styled(format!("   {k:<17}"), Style::new().fg(p.warning.c())),
                Span::styled(*v, Style::new().fg(p.text.c())),
            ]));
        }
        lines.push(Line::raw(""));
    }
    lines.pop();
    let area = modal_rect(
        screen,
        60,
        lines.len() as u16 + 2,
        app.started.max(Instant::now() - Duration::from_secs(1)),
        None,
    );
    dim_region(frame, screen, &p, 0.35);
    frame.render_widget(Clear, area);
    let block = panel(
        &p,
        Line::from(gradient(" ruddr · keys ", p.accent, p.success, 1.2, Style::new().bold())),
        true,
    )
    .style(Style::new().bg(p.panel.c()));
    frame.render_widget(Paragraph::new(lines).block(block), area);
    app.hits.push((area, Hit::Overlay));
}
