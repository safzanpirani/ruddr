//! Text helpers for trace records. Every trace record stays on one line:
//! `peek` and the TUI read trace.log line by line, and provider text can
//! carry newlines that would otherwise forge records.

use serde_json::Value;

/// Collapses runs of whitespace into single spaces and trims the ends.
pub fn single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// [`single_line`], truncated to `limit` bytes on a character boundary with
/// a trailing ellipsis.
pub fn one_line(value: &str, limit: usize) -> String {
    let value = single_line(value);
    if value.len() <= limit {
        return value;
    }
    let mut cut = limit;
    while cut > 0 && !value.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &value[..cut])
}

/// Joins the text inside a reasoning summary: strings, arrays of parts, and
/// objects with `text` or `content`.
pub fn flatten_strings(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(flatten_strings)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        Value::Object(map) => ["text", "content"]
            .iter()
            .filter_map(|key| map.get(*key))
            .map(flatten_strings)
            .find(|part| !part.is_empty())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// The current UTC time with whole seconds, like Go's `time.RFC3339`.
pub fn trace_stamp() -> String {
    let now = std::time::SystemTime::now();
    let seconds = now.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    ruddr_core::time::format_rfc3339(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collapses_and_flattens() {
        assert_eq!(one_line("hello\n  world", 20), "hello world");
        let long = "complete message ".repeat(40);
        assert_eq!(single_line(&long), long.trim());
        assert_eq!(flatten_strings(&json!([{"text": "first"}, "second"])), "first second");
        assert_eq!(flatten_strings(&json!({"content": [{"text": "deep"}]})), "deep");
    }

    #[test]
    fn truncates_on_character_boundaries() {
        for limit in [1, 5, 15, 16, 39] {
            let got = one_line(&"é".repeat(20), limit);
            assert!(got.len() <= limit + "…".len(), "limit {limit}: {got}");
        }
        assert!(one_line("日本語のテキスト", 7).ends_with('…'));
    }

    #[test]
    fn stamps_whole_seconds() {
        let stamp = trace_stamp();
        assert_eq!(stamp.len(), "2026-10-02T09:19:44Z".len(), "{stamp}");
        assert!(stamp.ends_with('Z'));
    }
}
