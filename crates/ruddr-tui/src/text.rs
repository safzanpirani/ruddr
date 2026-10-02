// Styled text helpers: markdown, code highlighting, word wrap, search marks.

use crate::theme::Palette;
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

/// One logical line plus the copy group it belongs to (chat entry, diff line…).
#[derive(Clone)]
pub struct Row {
    pub line: Line<'static>,
    pub group: usize,
    /// Continuation indent applied to wrapped rows.
    pub indent: u16,
}

impl Row {
    pub fn new(line: Line<'static>, group: usize) -> Row {
        Row { line, group, indent: 0 }
    }
    pub fn indent(mut self, indent: u16) -> Row {
        self.indent = indent;
        self
    }
}

pub fn line_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Greedy word wrap that keeps span styles, prefers breaking at spaces, and
/// styles every case-insensitive `query` match with `mark`.
pub fn wrap_rows(rows: &[Row], width: usize, query: &str, mark: Style, current_mark: Style, current_group: Option<usize>) -> Vec<Row> {
    let width = width.max(4);
    let needle: Vec<char> = query.to_lowercase().chars().collect();
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut cells: Vec<(char, Style)> = Vec::new();
        for span in &row.line.spans {
            let style = row.line.style.patch(span.style);
            cells.extend(span.content.chars().filter(|c| *c != '\t' && *c != '\r').map(|c| (c, style)));
        }
        if !needle.is_empty() {
            let lower: Vec<char> = cells.iter().map(|(c, _)| c.to_lowercase().next().unwrap_or(*c)).collect();
            let style = if Some(row.group) == current_group { current_mark } else { mark };
            let mut i = 0;
            while i + needle.len() <= lower.len() {
                if lower[i..i + needle.len()] == needle[..] {
                    for cell in &mut cells[i..i + needle.len()] {
                        cell.1 = cell.1.patch(style);
                    }
                    i += needle.len();
                } else {
                    i += 1;
                }
            }
        }
        let widths: Vec<usize> = cells.iter().map(|(c, _)| c.width().unwrap_or(0)).collect();
        if widths.iter().sum::<usize>() <= width {
            out.push(Row {
                line: to_line(&cells, 0, row.line.style),
                group: row.group,
                indent: row.indent,
            });
            continue;
        }
        let mut start = 0;
        let mut first = true;
        while start < cells.len() {
            let indent = if first { 0 } else { (row.indent as usize).min(width / 2) };
            let room = width - indent;
            let mut end = start;
            let mut used = 0;
            while end < cells.len() && used + widths[end] <= room {
                used += widths[end];
                end += 1;
            }
            if end == start {
                end = start + 1;
            }
            if end < cells.len() {
                if let Some(space) = (start + 1..end).rev().find(|i| cells[*i].0 == ' ') {
                    if space - start > room / 3 {
                        end = space + 1;
                    }
                }
            }
            let mut piece = &cells[start..end];
            while piece.len() > 1 && piece.last().is_some_and(|c| c.0 == ' ') && end < cells.len() {
                piece = &piece[..piece.len() - 1];
            }
            out.push(Row {
                line: to_line(piece, indent, row.line.style),
                group: row.group,
                indent: row.indent,
            });
            start = end;
            first = false;
        }
    }
    out
}

fn to_line(cells: &[(char, Style)], indent: usize, line_style: Style) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if indent > 0 {
        spans.push(Span::raw(" ".repeat(indent)));
    }
    let mut current = String::new();
    let mut style = cells.first().map(|c| c.1).unwrap_or_default();
    for (c, s) in cells {
        if *s != style && !current.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut current), style));
        }
        style = *s;
        current.push(*c);
    }
    if !current.is_empty() {
        spans.push(Span::styled(current, style));
    }
    Line::from(spans).style(line_style)
}

// --- markdown -------------------------------------------------------------

/// Inline markdown: **bold**, *italic*, `code`, [text](url).
pub fn inline(text: &str, base: Style, p: &Palette) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut buf = String::new();
    let mut i = 0;
    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>, style: Style| {
        if !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(buf), style));
        }
    };
    let find = |from: usize, pat: &[char]| (from..chars.len().saturating_sub(pat.len() - 1)).find(|j| chars[*j..*j + pat.len()] == *pat);
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            if let Some(end) = find(i + 1, &['`']) {
                flush(&mut buf, &mut spans, base);
                let code: String = chars[i + 1..end].iter().collect();
                spans.push(Span::styled(code, base.fg(p.warning.c()).bg(p.panel.c())));
                i = end + 1;
                continue;
            }
        } else if c == '*' && chars.get(i + 1) == Some(&'*') {
            if let Some(end) = find(i + 2, &['*', '*']) {
                flush(&mut buf, &mut spans, base);
                let inner: String = chars[i + 2..end].iter().collect();
                spans.extend(inline(&inner, base.add_modifier(Modifier::BOLD).fg(p.text.c()), p));
                i = end + 2;
                continue;
            }
        } else if (c == '*' || c == '_')
            && chars.get(i + 1).is_some_and(|n| !n.is_whitespace())
            && (i == 0 || !chars[i - 1].is_alphanumeric())
        {
            if let Some(end) = find(i + 1, &[c]) {
                if end > i + 1 {
                    flush(&mut buf, &mut spans, base);
                    let inner: String = chars[i + 1..end].iter().collect();
                    spans.push(Span::styled(inner, base.add_modifier(Modifier::ITALIC)));
                    i = end + 1;
                    continue;
                }
            }
        } else if c == '[' {
            if let Some(close) = find(i + 1, &[']']) {
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(paren) = find(close + 2, &[')']) {
                        flush(&mut buf, &mut spans, base);
                        let label: String = chars[i + 1..close].iter().collect();
                        spans.push(Span::styled(label, base.fg(p.accent.c()).add_modifier(Modifier::UNDERLINED)));
                        i = paren + 1;
                        continue;
                    }
                }
            }
        }
        buf.push(c);
        i += 1;
    }
    flush(&mut buf, &mut spans, base);
    spans
}

pub fn markdown(text: &str, group: usize, p: &Palette, base: Style) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut fence: Option<String> = None;
    for raw in text.lines() {
        let trimmed = raw.trim_start();
        if let Some(lang) = trimmed.strip_prefix("```") {
            if fence.is_some() {
                fence = None;
                rows.push(Row::new(Line::styled("╰─", Style::new().fg(p.border.c())), group));
            } else {
                let lang = lang.trim().to_string();
                rows.push(Row::new(
                    Line::from(vec![
                        Span::styled("╭─ ", Style::new().fg(p.border.c())),
                        Span::styled(
                            if lang.is_empty() { "code".into() } else { lang.clone() },
                            Style::new().fg(p.dim.c()).italic(),
                        ),
                    ]),
                    group,
                ));
                fence = Some(lang);
            }
            continue;
        }
        if let Some(lang) = &fence {
            let mut spans = vec![Span::styled("│ ", Style::new().fg(p.border.c()))];
            spans.extend(highlight_code(raw, lang, p));
            rows.push(Row::new(Line::from(spans), group).indent(2));
            continue;
        }
        let level = trimmed.chars().take_while(|c| *c == '#').count();
        if level > 0 && level <= 6 && trimmed.chars().nth(level) == Some(' ') {
            let style = base
                .fg(if level == 1 { p.accent.c() } else { p.warning.c() })
                .add_modifier(Modifier::BOLD);
            rows.push(Row::new(Line::from(inline(trimmed[level..].trim(), style, p)), group));
            continue;
        }
        if trimmed == "---" || trimmed == "***" {
            rows.push(Row::new(Line::styled("─".repeat(24), Style::new().fg(p.border.c())), group));
            continue;
        }
        let indent = raw.len() - trimmed.len();
        if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            let mut spans = vec![Span::raw(" ".repeat(indent)), Span::styled("• ", Style::new().fg(p.accent.c()))];
            spans.extend(inline(rest, base, p));
            rows.push(Row::new(Line::from(spans), group).indent(indent as u16 + 2));
            continue;
        }
        let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 && trimmed[digits..].starts_with(". ") {
            let mut spans = vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(trimmed[..digits + 2].to_string(), Style::new().fg(p.accent.c())),
            ];
            spans.extend(inline(&trimmed[digits + 2..], base, p));
            rows.push(Row::new(Line::from(spans), group).indent((indent + digits + 2) as u16));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("> ") {
            let mut spans = vec![Span::styled("▎ ", Style::new().fg(p.dim.c()))];
            spans.extend(inline(rest, base.fg(p.dim.c()).italic(), p));
            rows.push(Row::new(Line::from(spans), group).indent(2));
            continue;
        }
        rows.push(Row::new(Line::from(inline(raw, base, p)), group));
    }
    rows
}

const KEYWORDS: &[&str] = &[
    "fn",
    "let",
    "mut",
    "pub",
    "struct",
    "enum",
    "impl",
    "use",
    "mod",
    "match",
    "if",
    "else",
    "for",
    "while",
    "loop",
    "return",
    "func",
    "package",
    "import",
    "type",
    "const",
    "var",
    "def",
    "class",
    "async",
    "await",
    "function",
    "export",
    "from",
    "interface",
    "true",
    "false",
    "nil",
    "null",
    "None",
    "self",
    "this",
    "go",
    "defer",
    "in",
    "of",
    "new",
    "try",
    "catch",
    "throw",
    "where",
    "then",
    "do",
    "done",
    "fi",
    "echo",
    "with",
    "as",
    "break",
    "continue",
    "static",
    "trait",
    "range",
    "select",
    "case",
    "switch",
    "default",
];

/// A small lexer: comments, strings, numbers, keywords, and call names.
pub fn highlight_code(line: &str, lang: &str, p: &Palette) -> Vec<Span<'static>> {
    let hash_comments = matches!(
        lang,
        "sh" | "bash" | "zsh" | "shell" | "python" | "py" | "toml" | "yaml" | "yml" | "ruby" | "rb"
    );
    let chars: Vec<char> = line.chars().collect();
    let mut spans = Vec::new();
    let mut i = 0;
    let text = p.text.c();
    while i < chars.len() {
        let c = chars[i];
        let rest: String = chars[i..].iter().collect();
        if rest.starts_with("//") || (hash_comments && c == '#') || rest.starts_with("--") && matches!(lang, "sql" | "lua") {
            spans.push(Span::styled(rest, Style::new().fg(p.dim.c()).italic()));
            break;
        }
        if c == '"' || c == '\'' || c == '`' {
            let end = (i + 1..chars.len())
                .find(|j| chars[*j] == c && chars[*j - 1] != '\\')
                .map(|j| j + 1)
                .unwrap_or(chars.len());
            spans.push(Span::styled(
                chars[i..end].iter().collect::<String>(),
                Style::new().fg(p.success.c()),
            ));
            i = end;
            continue;
        }
        if c.is_ascii_digit() {
            let end = (i..chars.len())
                .find(|j| !(chars[*j].is_ascii_alphanumeric() || chars[*j] == '.' || chars[*j] == '_'))
                .unwrap_or(chars.len());
            spans.push(Span::styled(
                chars[i..end].iter().collect::<String>(),
                Style::new().fg(p.warning.c()),
            ));
            i = end;
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let end = (i..chars.len())
                .find(|j| !(chars[*j].is_alphanumeric() || chars[*j] == '_'))
                .unwrap_or(chars.len());
            let word: String = chars[i..end].iter().collect();
            let style = if KEYWORDS.contains(&word.as_str()) {
                Style::new().fg(p.accent.c()).add_modifier(Modifier::BOLD)
            } else if chars.get(end) == Some(&'(') {
                Style::new().fg(p.warning.c())
            } else if word.chars().next().is_some_and(char::is_uppercase) {
                Style::new().fg(p.accent.mix(p.success, 0.5).c())
            } else {
                Style::new().fg(text)
            };
            spans.push(Span::styled(word, style));
            i = end;
            continue;
        }
        let style = if "{}[]()<>=+-*/!&|:;,.".contains(c) {
            Style::new().fg(p.dim.mix(p.text, 0.4).c())
        } else {
            Style::new().fg(text)
        };
        spans.push(Span::styled(c.to_string(), style));
        i += 1;
    }
    spans
}

pub fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for k in 0..4 {
            if k <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * k) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::themes;

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(base64(b"hi!"), "aGkh");
        assert_eq!(base64(b"a"), "YQ==");
    }

    #[test]
    fn wraps_at_spaces() {
        let rows = vec![Row::new(Line::from("hello brave new world"), 0)];
        let wrapped = wrap_rows(&rows, 12, "", Style::new(), Style::new(), None);
        let texts: Vec<String> = wrapped.iter().map(|r| line_text(&r.line)).collect();
        assert_eq!(texts, vec!["hello brave", "new world"]);
    }

    #[test]
    fn inline_markdown_strips_markers() {
        let p = &themes()[0].palette;
        let text: String = inline("a **b** `c` [d](http://x)", Style::new(), p)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(text, "a b c d");
    }
}
