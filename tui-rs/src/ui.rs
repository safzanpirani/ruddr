// Rendering and motion. Every animation derives from wall-clock time, so a
// slow frame never desynchronises it.

use crate::core::*;
use crate::text::{highlight_code, line_text, markdown, wrap_rows, Row};
use crate::theme::{Palette, Rgb};
use crate::*;
use ratatui::layout::{Constraint, Layout as Split, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const LOGO: [&str; 3] = ["┏━┓╻ ╻╺┳┓╺┳┓┏━┓", "┣┳┛┃ ┃ ┃┃ ┃┃┣┳┛", "╹┗╸┗━┛╺┻┛╺┻┛╹┗╸"];

fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

fn progress(since: Instant, ms: u64) -> f32 {
    (since.elapsed().as_secs_f32() * 1000.0 / ms as f32).clamp(0.0, 1.0)
}

fn seconds(app: &App) -> f32 {
    app.started.elapsed().as_secs_f32()
}

fn spinner(app: &App) -> &'static str {
    SPINNER[(app.started.elapsed().as_millis() / 80) as usize % SPINNER.len()]
}

/// A slow breathing pulse between 0 and 1.
fn pulse(app: &App, period: f32) -> f32 {
    0.5 - 0.5 * (seconds(app) * std::f32::consts::TAU / period).cos()
}

fn status_color(p: &Palette, status: &str) -> Rgb {
    match status {
        "active" => p.success,
        "idle" | "starting" => p.accent,
        "failed" | "stale" => p.danger,
        "interrupted" => p.warning,
        _ => p.dim,
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

/// Text whose colour sweeps between two colours over time.
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

/// A bright band that travels across dim text.
fn shimmer<'a>(text: &str, base: Rgb, bright: Rgb, app: &App) -> Vec<Span<'a>> {
    let count = text.chars().count() as f32;
    let head = (seconds(app) * 14.0) % (count + 12.0) - 6.0;
    text.chars()
        .enumerate()
        .map(|(i, c)| {
            let d = (i as f32 - head).abs();
            let t = (1.0 - d / 5.0).max(0.0);
            Span::styled(c.to_string(), Style::new().fg(base.mix(bright, t).c()))
        })
        .collect()
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    app.mobile_now = app.args.mobile || area.width <= app.mobile_threshold;
    app.hits.clear();
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
            let list_width = (area.width / 3).clamp(30, 52);
            let [list, side] = Split::horizontal([Constraint::Length(list_width), Constraint::Min(20)]).areas(body);
            draw_sessions(frame, app, list, app.focus == Focus::Sessions);
            draw_main(frame, app, side);
        }
        Layout::Beta => {
            draw_main(frame, app, body);
            let target = if app.drawer { 1.0 } else { 0.0 };
            app.drawer_anim += (target - app.drawer_anim) * 0.35;
            if (app.drawer_anim - target).abs() < 0.02 {
                app.drawer_anim = target;
            }
            if app.drawer_anim > 0.0 {
                let full = (body.width.saturating_sub(4)).min(52).max(20);
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
            let shade = Rgb(0, 0, 0);
            cell.set_fg(fg.mix(p.background, amount).c());
            cell.set_bg(bg.mix(shade, amount * 0.5).c());
        }
    }
}

fn draw_splash(frame: &mut Frame, app: &App, area: Rect) {
    let p = app.palette();
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
    let mut spans = vec![Span::raw(" ")];
    spans.extend(gradient("◆ ruddr", p.accent, p.success, seconds(app) * 0.8, Style::new().bold()));
    let mut essential = spans.len();
    let session = app.current().cloned();
    if let Some(s) = &session {
        spans.push(Span::styled("  │  ", Style::new().fg(p.border.c())));
        spans.push(Span::styled(project_name(s), Style::new().fg(p.text.c()).bold()));
        essential = spans.len();
        if let Some(branch) = s.cwd.as_ref().and_then(|c| app.branches.get(c)).filter(|b| !b.is_empty()) {
            spans.push(Span::styled(format!(":{branch}"), Style::new().fg(p.dim.c())));
        }
        spans.push(Span::styled(
            format!("  {} {}", s.provider(), s.model.as_deref().unwrap_or(""))
                + &s.effort.as_ref().map(|e| format!(" · {e}")).unwrap_or_default(),
            Style::new().fg(p.dim.c()),
        ));
        if s.status == "active" && !app.mobile_now {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(format!("{} ", spinner(app)), Style::new().fg(p.success.c())));
            spans.extend(shimmer("working", p.dim, p.success, app));
            spans.push(Span::styled(
                format!(" {}", format_elapsed(&s.started_at, &None, now_seconds())),
                Style::new().fg(p.dim.c()),
            ));
        }
    }
    // Right side: context meter, live count, update badge.
    let mut right: Vec<Span> = Vec::new();
    if let Some(usage) = session.as_ref().and_then(|s| s.token_usage.clone()) {
        if let (Some(window), Some(used)) = (usage.context_window.filter(|w| *w > 0), usage.context_tokens) {
            let ratio = (used as f32 / window as f32).clamp(0.0, 1.0);
            app.meter_anim += (ratio - app.meter_anim) * 0.15;
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
    }
    let live = app
        .sessions
        .iter()
        .filter(|s| matches!(s.status.as_str(), "active" | "idle" | "starting"))
        .count();
    if live > 0 {
        let glow = p.success.mix(p.background, 0.5 * pulse(app, 2.4));
        right.push(Span::styled("● ", Style::new().fg(glow.c())));
        right.push(Span::styled(format!("{live} live "), Style::new().fg(p.text.c())));
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
    if right_width + left_needed + 2 > area.width {
        if let Some(start) = right.iter().position(|s| s.content == "ctx ") {
            right.drain(start..start + 12);
            right_width = right.iter().map(|s| s.content.width() as u16).sum();
        }
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

fn draw_sessions(frame: &mut Frame, app: &mut App, area: Rect, focused: bool) {
    let p = *app.palette();
    let visible: Vec<Session> = app.visible().into_iter().cloned().collect();
    let live = visible.iter().filter(|s| !is_terminal(&s.status) && s.status != "stale").count();
    let header = if app.filter.is_empty() {
        format!("sessions · {live} live · {}", visible.len())
    } else {
        format!("sessions · /{}", app.filter)
    };
    let block = panel(&p, title(&p, header, focused), focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if visible.is_empty() {
        let text = if app.filter.is_empty() {
            "No sessions yet.\n\nPress n to start one."
        } else {
            "Nothing matches the filter."
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
    }
    let now = now_seconds();
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
        let colour = status_color(&p, &session.status);
        let mut bg = p.background;
        if let Some((_, changed)) = app.seen.get(&session.state_dir) {
            let flash = 1.0 - progress(*changed, 1600);
            if flash > 0.0 {
                bg = bg.mix(colour, 0.35 * flash);
            }
        }
        let glyph = match session.status.as_str() {
            "active" | "starting" => spinner(app).to_string(),
            status => status_glyph(status).to_string(),
        };
        let glyph_colour = if session.status == "idle" {
            colour.mix(p.background, 0.5 * pulse(app, 3.0))
        } else {
            colour
        };
        let age = format_age(&session.updated_at, now);
        let name = project_name(session);
        let room = (inner.width as usize).saturating_sub(age.len() + 5);
        let name: String = if name.chars().count() > room {
            name.chars().take(room.saturating_sub(1)).collect::<String>() + "…"
        } else {
            name
        };
        let gap = (inner.width as usize).saturating_sub(name.width() + age.len() + 4);
        let line1 = Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{glyph} "), Style::new().fg(glyph_colour.c())),
            Span::styled(name, Style::new().fg(p.text.c()).bold()),
            Span::raw(" ".repeat(gap)),
            Span::styled(age, Style::new().fg(p.dim.c())),
        ]);
        let mut meta = format!("    {} · {}", session.provider(), session.model.as_deref().unwrap_or("default"));
        if let Some(usage) = &session.token_usage {
            if let Some(total) = usage.total_tokens.filter(|t| *t > 0) {
                meta.push_str(&format!(" · {}", format_token_count(total)));
            }
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
    let session = app.current().cloned();
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
            format!(" {} ", status_glyph(&s.status)),
            Style::new().fg(status_color(p, &s.status).c()),
        ),
        Span::styled(s.status.clone(), Style::new().fg(status_color(p, &s.status).c())),
        Span::styled(
            format!(" · {}", format_elapsed(&s.started_at, &s.completed_at, now_seconds())),
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
    session_details(s, now_seconds())
        .into_iter()
        .filter(|(k, _)| !compact || matches!(k.as_str(), "status" | "model" | "tokens" | "error"))
        .map(|(k, v)| {
            let colour = match k.as_str() {
                "error" => p.danger,
                "status" => status_color(p, &s.status),
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
    let files = &app.sources.diff_files;
    for tab in Tab::ALL {
        let mut label = format!(" {} {} ", tab.index() + 1, tab.title());
        if tab == Tab::Diff && !files.is_empty() {
            let added: u32 = files.iter().map(|f| f.added).sum();
            let removed: u32 = files.iter().map(|f| f.removed).sum();
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
    if !app.tab_settling {
        app.tab_anim = target;
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), Rect { height: 1, ..area });
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
    // Follow state on the right of the tab strip.
    let hint = if app.follow { "● live" } else { "‖ paused · G" };
    let colour = if app.follow {
        p.success.mix(p.background, 0.4 * pulse(app, 2.0))
    } else {
        p.warning
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

struct Built {
    rows: Vec<Row>,
    groups: Vec<String>,
    meta: Vec<GroupMeta>,
}

fn build_chat(app: &App, p: &Palette) -> Built {
    let session = app.current();
    let mut rows = Vec::new();
    let mut groups = Vec::new();
    let entries = &app.sources.entries;
    let last_agent = entries.iter().rposition(|e| e.kind == EntryKind::Agent);
    for (index, entry) in entries.iter().enumerate() {
        let group = groups.len();
        match entry.kind {
            EntryKind::User => {
                rows.push(Row::new(Line::raw(""), group));
                let bubble = p.background.mix(p.accent, 0.08);
                rows.push(Row::new(
                    Line::from(vec![
                        Span::styled("▌ ", Style::new().fg(p.accent.c())),
                        Span::styled("you", Style::new().fg(p.accent.c()).bold()),
                    ])
                    .style(Style::new().bg(bubble.c())),
                    group,
                ));
                for line in entry.text.lines() {
                    rows.push(
                        Row::new(
                            Line::from(vec![
                                Span::styled("▌ ", Style::new().fg(p.accent.c())),
                                Span::styled(line.to_string(), Style::new().fg(p.text.c())),
                            ])
                            .style(Style::new().bg(bubble.c())),
                            group,
                        )
                        .indent(2),
                    );
                }
                rows.push(Row::new(Line::raw(""), group));
            }
            EntryKind::Agent => {
                let mut text = entry.text.clone();
                let mut typing = false;
                if Some(index) == last_agent {
                    if let Some((id, revealed)) = &app.reveal {
                        if entry.item_id.as_deref().unwrap_or("") == id && *revealed < text.chars().count() {
                            text = text.chars().take(*revealed).collect();
                            typing = true;
                        }
                    }
                }
                let provider = session.map(|s| s.provider().to_string()).unwrap_or_default();
                rows.push(Row::new(
                    Line::from(vec![
                        Span::styled("◆ ", Style::new().fg(p.success.c())),
                        Span::styled(provider, Style::new().fg(p.success.c()).bold()),
                    ]),
                    group,
                ));
                let mut body = markdown(&text, group, p, Style::new().fg(p.text.c()));
                for row in &mut body {
                    row.line.spans.insert(0, Span::raw("  "));
                    row.indent += 2;
                }
                if typing {
                    if let Some(last) = body.last_mut() {
                        last.line.spans.push(Span::styled("▍", Style::new().fg(p.accent.c())));
                    }
                }
                rows.extend(body);
                rows.push(Row::new(Line::raw(""), group));
            }
            EntryKind::Thought => {
                rows.push(
                    Row::new(
                        Line::from(vec![
                            Span::styled("  ∴ ", Style::new().fg(p.dim.c())),
                            Span::styled(entry.text.clone(), Style::new().fg(p.dim.c()).italic()),
                        ]),
                        group,
                    )
                    .indent(4),
                );
            }
            EntryKind::Tool => {
                let (glyph, colour) = match entry.status {
                    Some(ToolStatus::Running) => (spinner(app).to_string(), p.warning),
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
                rows.push(Row::new(Line::from(spans), group).indent(4));
            }
        }
        groups.push(entry.text.clone());
    }
    if let Some(s) = session {
        let group = groups.len();
        match s.status.as_str() {
            "active" | "starting" => {
                let mut spans = vec![Span::styled(format!("  {} ", spinner(app)), Style::new().fg(p.accent.c()))];
                spans.extend(shimmer(
                    if s.status == "starting" { "starting session" } else { "thinking" },
                    p.dim,
                    p.accent,
                    app,
                ));
                spans.push(Span::styled(
                    format!("  {}", format_elapsed(&s.started_at, &None, now_seconds())),
                    Style::new().fg(p.border.c()),
                ));
                rows.push(Row::new(Line::from(spans), group));
                groups.push(String::new());
            }
            "idle" => {
                let glow = p.dim.mix(p.accent, 0.5 * pulse(app, 3.0));
                rows.push(Row::new(
                    Line::from(vec![
                        Span::styled("  ◌ ", Style::new().fg(glow.c())),
                        Span::styled("waiting for your next prompt · s to send", Style::new().fg(p.dim.c())),
                    ]),
                    group,
                ));
                groups.push(String::new());
            }
            _ => {}
        }
    }
    if rows.is_empty() {
        rows.push(Row::new(Line::styled("  No messages yet.", Style::new().fg(p.dim.c())), 0));
        groups.push(String::new());
    }
    let meta = vec![GroupMeta::default(); groups.len()];
    Built { rows, groups, meta }
}

fn build_trace(app: &App, p: &Palette) -> Built {
    let mut rows = Vec::new();
    let mut groups = Vec::new();
    for line in app.sources.trace.lines() {
        let Some((time, rest)) = line.split_once(' ') else { continue };
        let (tag, body) = rest.strip_prefix('[').and_then(|r| r.split_once(']')).unwrap_or(("", rest));
        if tag == "usage" {
            continue;
        }
        let (glyph, colour) = match tag {
            "error" | "failed" => ("✗", p.danger),
            "warn" => ("!", p.warning),
            "completed" => ("✓", p.success),
            "in_progress" => ("›", p.accent),
            "think" => ("∴", p.dim),
            "say" => ("◆", p.text),
            _ => ("·", p.accent.mix(p.success, 0.5)),
        };
        let short = time.get(11..19).unwrap_or(time).to_string();
        let body = body.trim_start().to_string();
        let style = if tag == "think" {
            Style::new().fg(p.dim.c()).italic()
        } else {
            Style::new().fg(p.text.c())
        };
        rows.push(
            Row::new(
                Line::from(vec![
                    Span::styled(format!(" {short} "), Style::new().fg(p.border.mix(p.dim, 0.5).c())),
                    Span::styled(format!("{glyph} "), Style::new().fg(colour.c())),
                    Span::styled(format!("{tag:<11} "), Style::new().fg(colour.c())),
                    Span::styled(body.clone(), style),
                ]),
                groups.len(),
            )
            .indent(24),
        );
        groups.push(body);
    }
    if rows.is_empty() {
        rows.push(Row::new(Line::styled("  No activity yet.", Style::new().fg(p.dim.c())), 0));
        groups.push(String::new());
    }
    let meta = vec![GroupMeta::default(); groups.len()];
    Built { rows, groups, meta }
}

fn build_output(app: &App, p: &Palette) -> Built {
    if app.sources.output.is_empty() {
        return Built {
            rows: vec![Row::new(Line::styled("  No output yet.", Style::new().fg(p.dim.c())), 0)],
            groups: vec![String::new()],
            meta: vec![GroupMeta::default()],
        };
    }
    // One group per paragraph keeps copy useful.
    let mut rows = Vec::new();
    let mut groups = Vec::new();
    for block in app.sources.output.split("\n\n") {
        let group = groups.len();
        for mut row in markdown(block, group, p, Style::new().fg(p.text.c())) {
            row.line.spans.insert(0, Span::raw(" "));
            rows.push(row);
        }
        rows.push(Row::new(Line::raw(""), group));
        groups.push(block.to_string());
    }
    let meta = vec![GroupMeta::default(); groups.len()];
    Built { rows, groups, meta }
}

fn build_diff(app: &App, p: &Palette) -> Built {
    if let Some(error) = &app.sources.diff_error {
        return Built {
            rows: vec![Row::new(Line::styled(format!("  {error}"), Style::new().fg(p.danger.c())), 0)],
            groups: vec![error.clone()],
            meta: vec![GroupMeta::default()],
        };
    }
    if app.sources.diff.is_empty() {
        return Built {
            rows: vec![Row::new(
                Line::styled("  ✓ Working tree clean against HEAD.", Style::new().fg(p.success.c())),
                0,
            )],
            groups: vec![String::new()],
            meta: vec![GroupMeta::default()],
        };
    }
    let gutter = app
        .sources
        .diff
        .iter()
        .filter_map(|l| l.old.max(l.new))
        .max()
        .unwrap_or(1)
        .to_string()
        .len();
    let add_bg = p.background.mix(p.success, 0.14);
    let del_bg = p.background.mix(p.danger, 0.14);
    let add_gutter = p.background.mix(p.success, 0.26);
    let del_gutter = p.background.mix(p.danger, 0.26);
    let hunk_bg = p.background.mix(p.accent, 0.08);
    let mut rows = Vec::new();
    let mut groups = Vec::new();
    let mut meta = Vec::new();
    let lang_for = |path: &str| path.rsplit('.').next().unwrap_or("").to_string();
    for line in &app.sources.diff {
        let file = &app.sources.diff_files[line.file];
        let folded = app.folded.contains(&file.path);
        if folded && line.kind != DiffKind::FileHeader {
            continue;
        }
        let group = groups.len();
        let mut m = GroupMeta::default();
        let number = |n: Option<u32>| n.map(|n| format!("{n:>gutter$}")).unwrap_or_else(|| " ".repeat(gutter));
        let row = match line.kind {
            DiffKind::FileHeader => {
                m.diff_header = Some(file.path.clone());
                let row_bg = p.panel;
                rows.push(Row::new(Line::raw(""), group));
                Row::new(
                    Line::from(vec![
                        Span::styled(if folded { " ▸ " } else { " ▾ " }, Style::new().fg(p.accent.c())),
                        Span::styled(file.path.clone(), Style::new().fg(p.text.c()).bold()),
                        Span::styled(format!("  +{}", file.added), Style::new().fg(p.success.c())),
                        Span::styled(format!(" −{}", file.removed), Style::new().fg(p.danger.c())),
                        Span::styled(
                            if folded { "  folded".to_string() } else { String::new() },
                            Style::new().fg(p.dim.c()).italic(),
                        ),
                    ])
                    .style(Style::new().bg(row_bg.c())),
                    group,
                )
            }
            DiffKind::Meta => continue,
            DiffKind::Hunk => {
                m.hunk = true;
                let context = line.text.splitn(3, "@@").nth(2).unwrap_or("").trim().to_string();
                Row::new(
                    Line::from(vec![
                        Span::styled(format!(" {} ", "┄".repeat(gutter * 2 + 1)), Style::new().fg(p.border.c())),
                        Span::styled(
                            line.text.split("@@").nth(1).unwrap_or("").trim().to_string(),
                            Style::new().fg(p.accent.c()),
                        ),
                        Span::styled(format!("  {context}"), Style::new().fg(p.dim.c()).italic()),
                    ])
                    .style(Style::new().bg(hunk_bg.c())),
                    group,
                )
            }
            DiffKind::Add | DiffKind::Del | DiffKind::Context => {
                let (sign, bg, gutter_bg, sign_colour) = match line.kind {
                    DiffKind::Add => ("+", Some(add_bg), Some(add_gutter), p.success),
                    DiffKind::Del => ("-", Some(del_bg), Some(del_gutter), p.danger),
                    _ => (" ", None, None, p.dim),
                };
                let code = line.text.get(1..).unwrap_or("");
                let mut spans = vec![
                    Span::styled(
                        format!(" {} {} ", number(line.old), number(line.new)),
                        Style::new()
                            .fg(p.dim.mix(p.border, 0.3).c())
                            .bg(gutter_bg.unwrap_or(p.background).c()),
                    ),
                    Span::styled(format!("{sign} "), Style::new().fg(sign_colour.c())),
                ];
                spans.extend(highlight_code(code, &lang_for(&file.path), p));
                let mut l = Line::from(spans);
                if let Some(bg) = bg {
                    l = l.style(Style::new().bg(bg.c()));
                }
                Row::new(l, group).indent(gutter as u16 * 2 + 5)
            }
        };
        rows.push(row);
        groups.push(
            line.text
                .get(if line.kind == DiffKind::FileHeader { 0 } else { 1 }..)
                .unwrap_or("")
                .to_string(),
        );
        if line.kind == DiffKind::FileHeader {
            *groups.last_mut().unwrap() = file.path.clone();
        }
        meta.push(m);
    }
    Built { rows, groups, meta }
}

fn draw_artifact(frame: &mut Frame, app: &mut App, area: Rect) {
    let p = *app.palette();
    let focused = app.focus == Focus::Artifact && !(app.layout() == Layout::Beta && app.drawer);
    app.hits.push((area, Hit::Artifact));
    let built = match app.tab {
        Tab::Chat => build_chat(app, &p),
        Tab::Trace => build_trace(app, &p),
        Tab::Output => build_output(app, &p),
        Tab::Diff => build_diff(app, &p),
    };
    // Diff file tree on wide screens.
    let mut body = area;
    if app.tab == Tab::Diff && !app.mobile_now && !app.sources.diff_files.is_empty() && area.width >= 80 {
        let wanted = match (app.tree_ratio, app.tree_width) {
            (Some(r), _) => (r * area.width as f64).round() as u16,
            (None, Some(w)) => w,
            _ => 30,
        };
        let width = wanted.clamp(20, 60.min(area.width.saturating_sub(40)).max(20));
        let [tree, rest] = Split::horizontal([Constraint::Length(width), Constraint::Min(20)]).areas(area);
        draw_tree(frame, app, tree, &p);
        body = rest;
    }
    let block = panel(&p, Line::default(), focused).borders(Borders::ALL);
    let inner = block.inner(body);
    let query = app.artifact_query.get(&app.tab).cloned().unwrap_or_default();
    let mark = Style::new().bg(p.warning.mix(p.background, 0.55).c()).fg(p.text.c());
    let current_mark = Style::new().bg(p.warning.c()).fg(p.background.c()).bold();
    let mut wrapped = wrap_rows(
        &built.rows,
        inner.width.saturating_sub(1) as usize,
        &query,
        mark,
        current_mark,
        app.cursor,
    );
    app.groups = built.groups;
    app.group_meta = built.meta;
    app.group_rows = wrapped.iter().enumerate().map(|(i, r)| (r.group, i)).collect();
    let height = inner.height as usize;
    let total = wrapped.len();
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
        if let Some(cursor) = app.cursor {
            let first = app.group_rows.iter().find(|(g, _)| *g == cursor).map(|(_, r)| *r);
            let last = app.group_rows.iter().rev().find(|(g, _)| *g == cursor).map(|(_, r)| *r);
            if let (Some(first), Some(last)) = (first, last) {
                if first < app.scroll_target {
                    app.scroll_target = first.saturating_sub(1);
                } else if last >= app.scroll_target + height {
                    app.scroll_target = (last + 2).saturating_sub(height).min(first.saturating_sub(1)).min(max);
                }
            }
        }
    }
    // Ease toward the scroll target; big jumps settle in a few frames.
    let target = app.scroll_target as f32;
    app.scroll_pos += (target - app.scroll_pos) * 0.4;
    if (app.scroll_pos - target).abs() < 0.5 {
        app.scroll_pos = target;
    }
    let scroll = (app.scroll_pos.round() as usize).min(max);
    // Cursor highlight.
    if let Some(cursor) = app.cursor {
        let bar = if focused { p.selected } else { p.selected.mix(p.background, 0.5) };
        for row in wrapped.iter_mut().filter(|r| r.group == cursor) {
            row.line.style = row.line.style.bg(bar.c());
            for span in &mut row.line.spans {
                if span.style.bg.is_some() {
                    span.style.bg = Some(bar.c());
                }
            }
        }
    }
    let mut title_spans = vec![Span::styled(
        format!(" {} ", app.tab.title()),
        Style::new().fg(if focused { p.accent.c() } else { p.dim.c() }).bold(),
    )];
    if !query.is_empty() {
        let count = app
            .groups
            .iter()
            .filter(|g| g.to_lowercase().contains(&query.to_lowercase()))
            .count();
        title_spans.push(Span::styled(format!("/{query} · {count} "), Style::new().fg(p.warning.c())));
    }
    let lines: Vec<Line> = wrapped.into_iter().skip(scroll).take(height).map(|r| r.line).collect();
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
            let bob = (pulse(app, 1.6) * 1.0).round() as u16;
            let rect = Rect {
                x: body.x + body.width.saturating_sub(w) / 2,
                y: body.bottom().saturating_sub(2 + bob),
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
    let block = panel(p, title(p, format!("files · {}", app.sources.diff_files.len()), false), false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let current = app.cursor.and_then(|c| {
        // The file that owns the cursor row.
        app.group_meta[..=c.min(app.group_meta.len().saturating_sub(1))]
            .iter()
            .rev()
            .find_map(|m| m.diff_header.clone())
    });
    for (i, file) in app.sources.diff_files.iter().enumerate().take(inner.height as usize) {
        let y = inner.y + i as u16;
        let rect = Rect { y, height: 1, ..inner };
        app.hits.push((rect, Hit::TreeFile(i)));
        let (dir, name) = file
            .path
            .rsplit_once('/')
            .map(|(d, n)| (format!("{d}/"), n.to_string()))
            .unwrap_or((String::new(), file.path.clone()));
        let stats = format!("+{} −{}", file.added, file.removed);
        let room = (inner.width as usize).saturating_sub(stats.width() + 4);
        let mut dir_shown = dir.clone();
        if dir.width() + name.width() > room {
            let keep = room.saturating_sub(name.width() + 1);
            let tail: String = dir.chars().rev().take(keep).collect::<Vec<_>>().into_iter().rev().collect();
            dir_shown = if keep > 2 { format!("…{tail}") } else { String::new() };
        }
        let folded = app.folded.contains(&file.path);
        let selected = current.as_deref() == Some(&file.path);
        let name: String = if name.width() > room {
            name.chars().take(room.saturating_sub(1)).collect::<String>() + "…"
        } else {
            name
        };
        let gap = (inner.width as usize)
            .saturating_sub(dir_shown.width() + name.width() + stats.width() + 3)
            .max(1);
        let line = Line::from(vec![
            Span::styled(if folded { "▸ " } else { "▾ " }, Style::new().fg(p.dim.c())),
            Span::styled(dir_shown, Style::new().fg(p.dim.c())),
            Span::styled(name, Style::new().fg(if selected { p.accent.c() } else { p.text.c() })),
            Span::raw(" ".repeat(gap)),
            Span::styled(format!("+{}", file.added), Style::new().fg(p.success.c())),
            Span::styled(format!(" −{} ", file.removed), Style::new().fg(p.danger.c())),
        ]);
        let style = if selected { Style::new().bg(p.selected.c()) } else { Style::new() };
        frame.render_widget(Paragraph::new(line).style(style), rect);
    }
}

fn help_segments(app: &App) -> Vec<(String, String)> {
    let session = app.current();
    let stoppable = session.is_some_and(|s| matches!(s.status.as_str(), "active" | "idle"));
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
    if stoppable {
        segments.push(("x x", "stop"));
    }
    segments.extend([(":", "commands"), ("t", "theme"), ("?", "help"), ("q", "quit")]);
    segments.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn draw_footer(frame: &mut Frame, app: &mut App, area: Rect) {
    let p = *app.palette();
    if let Some(search) = &app.search {
        let (label, hint) = match search.target {
            SearchTarget::Sessions => ("filter", "Enter keep · Esc clear"),
            SearchTarget::Artifact => ("search", "Enter jump · n/N matches · Esc clear"),
            SearchTarget::Deja => ("deja find", "Enter search · Esc cancel"),
        };
        let caret = if (app.started.elapsed().as_millis() / 530) % 2 == 0 {
            "▏"
        } else {
            " "
        };
        let line = Line::from(vec![
            Span::styled(format!(" {label} "), Style::new().fg(p.background.c()).bg(p.accent.c()).bold()),
            Span::styled(" › ", Style::new().fg(p.accent.c())),
            Span::styled(search.text.clone(), Style::new().fg(p.text.c())),
            Span::styled(caret, Style::new().fg(p.accent.c())),
            Span::styled(format!("   {hint}"), Style::new().fg(p.dim.c())),
        ]);
        frame.render_widget(Paragraph::new(line).style(Style::new().bg(p.panel.c())), area);
        return;
    }
    if let Some((_, armed)) = &app.stop_armed {
        let remaining = 1.0 - progress(*armed, 2000);
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

fn draw_action_bar(frame: &mut Frame, app: &mut App, area: Rect) {
    let p = *app.palette();
    let session = app.current();
    let stoppable = session.is_some_and(|s| matches!(s.status.as_str(), "active" | "idle"));
    let mut buttons: Vec<(&str, Cmd, Rgb)> = vec![
        ("prompt", Cmd::Prompt, p.accent),
        ("new", Cmd::New, p.success),
        ("tab", Cmd::Tab(app.tab.next()), p.text),
        ("list", Cmd::Sessions, p.text),
    ];
    if stoppable {
        buttons.push(("stop", Cmd::StopNow, p.danger));
    }
    buttons.push(("menu", Cmd::Palette, p.dim));
    app.buttons = buttons.iter().map(|b| b.1.clone()).collect();
    let n = buttons.len() as u16;
    let width = area.width / n;
    for (i, (label, _, colour)) in buttons.iter().enumerate() {
        let x = area.x + i as u16 * width;
        let w = if i as u16 == n - 1 { area.right() - x } else { width };
        let rect = Rect {
            x,
            y: area.y,
            width: w,
            height: 3,
        };
        app.hits.push((rect, Hit::Button(i)));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(colour.mix(p.background, 0.4).c()))
            .style(Style::new().bg(p.panel.c()));
        frame.render_widget(
            Paragraph::new(Span::styled(*label, Style::new().fg(colour.c()).bold()))
                .centered()
                .block(block),
            rect,
        );
    }
}

fn draw_toasts(frame: &mut Frame, app: &App, area: Rect) {
    let p = app.palette();
    let mut y = area.bottom();
    for toast in app.toasts.iter().rev() {
        let age = toast.born.elapsed();
        let life = toast.lifetime();
        let enter = ease_out(age.as_secs_f32() / 0.22);
        let exit = 1.0 - ((age.as_secs_f32() - (life.as_secs_f32() - 0.5)) / 0.5).clamp(0.0, 1.0);
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
    let (label, colour) = match prompt.kind {
        PromptKind::Route(PromptRoute::Steer) => ("steer the active turn", p.warning),
        PromptKind::Route(PromptRoute::Prompt) => ("next turn", p.accent),
        PromptKind::Route(PromptRoute::Continue) => ("continue in a new run", p.success),
        PromptKind::New if prompt.resume.is_some() => ("resume a past session", p.success),
        PromptKind::New => ("new session", p.success),
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let place = match (&prompt.target, prompt.kind) {
        (Some(t), _) => project_name(t),
        (None, _) => cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
    };
    let model = prompt
        .model
        .as_ref()
        .map(|m| m.name())
        .or_else(|| prompt.target.as_ref().and_then(|t| t.model.clone()))
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
    let area = modal_rect(screen, width, height, prompt.opened, None);
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
    let open = ease_out(progress(prompt.opened, 220));
    let border = p.border.mix(colour, open);
    let header = Line::from(vec![
        Span::styled(format!(" {label} "), Style::new().fg(p.background.c()).bg(colour.c()).bold()),
        Span::styled(format!(" {place} "), Style::new().fg(p.text.c())),
    ]);
    let mut chip = vec![Span::styled(
        format!(" {} · {}", prompt.provider, model),
        Style::new().fg(p.dim.c()),
    )];
    if let Some(effort) = &effort {
        chip.push(Span::styled(format!(" · {effort}"), Style::new().fg(p.dim.c())));
    }
    chip.push(Span::raw(" "));
    let changeable = !matches!(
        prompt.kind,
        PromptKind::Route(PromptRoute::Steer) | PromptKind::Route(PromptRoute::Prompt)
    );
    let footer = Line::from(vec![
        Span::styled(" enter", Style::new().fg(p.accent.c())),
        Span::styled(" send · ", Style::new().fg(p.dim.c())),
        Span::styled("alt+enter", Style::new().fg(p.accent.c())),
        Span::styled(" newline · ", Style::new().fg(p.dim.c())),
        Span::styled(if changeable { "tab" } else { "" }, Style::new().fg(p.accent.c())),
        Span::styled(if changeable { " model · " } else { "" }, Style::new().fg(p.dim.c())),
        Span::styled("esc", Style::new().fg(p.accent.c())),
        Span::styled(" cancel ", Style::new().fg(p.dim.c())),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border.c()))
        .style(Style::new().bg(p.panel.c()).fg(p.text.c()))
        .title(header)
        .title(Line::from(chip).right_aligned())
        .title_bottom(footer);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let text_area = Rect {
        x: inner.x + 1,
        y: inner.y + 1,
        width: inner.width.saturating_sub(2),
        height: inner.height.saturating_sub(2),
    };
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
    // A blinking block caret that stays solid while typing.
    let blink = prompt.typed.elapsed() < Duration::from_millis(500) || (prompt.typed.elapsed().as_millis() / 530) % 2 == 0;
    if blink && caret.0 >= first && caret.0 - first < rows {
        let cx = text_area.x + caret.1 as u16;
        let cy = text_area.y + (caret.0 - first) as u16;
        if cx < text_area.right() {
            let buf = frame.buffer_mut();
            buf[(cx, cy)].set_bg(colour.c()).set_fg(p.background.c());
        }
    }
    // Character counter.
    let count = format!(" {} ", prompt.text.len());
    let cw = count.len() as u16;
    if area.width > cw + 4 {
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
    let Some(picker) = &mut app.picker else { return };
    let visible = picker.visible();
    if picker.index >= visible.len() {
        picker.index = visible.len().saturating_sub(1);
    }
    picker.sel_anim += (picker.index as f32 - picker.sel_anim) * 0.5;
    if (picker.sel_anim - picker.index as f32).abs() < 0.05 {
        picker.sel_anim = picker.index as f32;
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
    app.hits.push((area, Hit::Overlay));
    let mut list_area = inner;
    if picker.filterable {
        let caret = if (app.started.elapsed().as_millis() / 530) % 2 == 0 {
            "▏"
        } else {
            " "
        };
        let query = Line::from(vec![
            Span::styled(" › ", Style::new().fg(edge.c())),
            Span::styled(picker.query.clone(), Style::new().fg(p.text.c())),
            Span::styled(caret, Style::new().fg(edge.c())),
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
        app.hits.push((rect, Hit::PickItem(slot)));
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
            let hint: String = item.hint.chars().take(list_area.width as usize - 6).collect();
            lines.push(Line::from(Span::styled(format!("    {hint}"), Style::new().fg(p.dim.c()))));
        }
        frame.render_widget(Paragraph::new(lines), rect);
        if picker.kind == PickerKind::Palette && selected && !item.hint.is_empty() && item.disabled.is_none() {
            // Show the hint of the selected command under the list.
        }
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
    if picker.kind == PickerKind::Palette {
        if let Some(item) = picker.selected().map(|i| &picker.items[i]) {
            let text = item.disabled.clone().unwrap_or_else(|| item.hint.clone());
            if !text.is_empty() {
                let text: String = text.chars().take(area.width.saturating_sub(6) as usize).collect();
                let w = text.width() as u16 + 2;
                frame.render_widget(
                    Paragraph::new(Span::styled(format!(" {text} "), Style::new().fg(p.dim.c()).italic())),
                    Rect {
                        x: area.x + 2,
                        y: area.bottom() - 1,
                        width: w.min(area.width - 4),
                        height: 1,
                    },
                );
            }
        }
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
                ("x x", "interrupt turn / end idle session"),
                ("D", "delete finished session"),
            ],
        ),
        (
            "diff",
            &[
                ("]c [c", "next / previous hunk"),
                ("]f [f", "next / previous file"),
                ("Enter  Z", "fold file / fold all"),
            ],
        ),
        (
            "other",
            &[
                (": ^k", "command palette"),
                ("t", "theme with live preview"),
                ("i", "cycle details"),
                ("c", "copy row (OSC 52)"),
                ("right-click", "session menu"),
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
        Line::from(gradient(" ruddr · keys ", p.accent, p.success, seconds(app), Style::new().bold())),
        true,
    )
    .style(Style::new().bg(p.panel.c()));
    frame.render_widget(Paragraph::new(lines).block(block), area);
    app.hits.push((area, Hit::Overlay));
}

#[allow(dead_code)]
fn plain(line: &Line) -> String {
    line_text(line)
}
