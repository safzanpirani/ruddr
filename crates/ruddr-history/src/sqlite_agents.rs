//! OpenClaw (`$OPENCLAW_STATE_DIR/agents/<agent>/agent/openclaw-agent.sqlite`,
//! default `~/.openclaw`) and Hermes Agent (`$HERMES_HOME/state.db`, default
//! `~/.hermes`), which keep transcripts in SQLite. Each session renders as
//! Pi-format JSONL and goes through the Pi reader, the same way dejavu reads
//! them; locators use dejavu's `openclaw://` and `hermes://` encoding, so a
//! deja hit's locator loads here unchanged.
//!
//! OpenClaw stores one Pi entry per `transcript_events` row, zstd-compressing
//! large rows. Hermes stores OpenAI-style chat rows, mapped onto Pi `message`
//! entries. Databases open read-only and only transcript tables are queried:
//! OpenClaw keeps auth profiles in the same file, and those are never read.

use crate::{Event, Provider, SessionInfo, title_from};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Map, Value, json};
use std::io::Read;
use std::path::Path;

fn scheme(provider: Provider) -> &'static str {
    match provider {
        Provider::OpenClaw => "openclaw",
        _ => "hermes",
    }
}

fn encode(text: &str, keep: &[u8]) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || keep.contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn is_windows_absolute(path: &str) -> bool {
    let b = path.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/')
}

/// `<scheme>://<db>#<session>`, percent-encoded except `/` and `:` in the
/// path, with a leading `/` before a Windows drive.
pub fn locator(provider: Provider, database: &Path, session: &str) -> String {
    let path = database.to_string_lossy();
    let slash = if is_windows_absolute(&path) { "/" } else { "" };
    format!(
        "{}://{slash}{}#{}",
        scheme(provider),
        encode(&path, b"-_.!~*'()/:"),
        encode(session, b"-_.!~*'()")
    )
}

/// The session ID in an `openclaw://` or `hermes://` locator.
pub fn session_id(locator: &str) -> Option<String> {
    parse_locator(locator).ok().map(|(_, _, id)| id)
}

fn parse_locator(locator: &str) -> Result<(Provider, String, String), String> {
    let invalid = || format!("invalid session locator: {locator}");
    let (provider, rest) = if let Some(rest) = locator.strip_prefix("openclaw://") {
        (Provider::OpenClaw, rest)
    } else if let Some(rest) = locator.strip_prefix("hermes://") {
        (Provider::Hermes, rest)
    } else {
        return Err(invalid());
    };
    let (path, session) = rest.rsplit_once('#').ok_or_else(invalid)?;
    let path = decode(path).ok_or_else(invalid)?;
    let path = match path.strip_prefix('/') {
        Some(windows) if is_windows_absolute(windows) => windows.to_string(),
        _ => path,
    };
    let session = decode(session).filter(|s| !s.is_empty()).ok_or_else(invalid)?;
    Ok((provider, path, session))
}

/// A read-only connection; a WAL database without a `-shm` file reads as
/// immutable, as OpenCode's does.
fn open(database: &Path) -> rusqlite::Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    match Connection::open_with_flags(database, flags)
        .and_then(|c| c.query_row("SELECT 1 FROM sqlite_master LIMIT 1", [], |_| Ok(())).map(|_| c))
    {
        Ok(connection) => Ok(connection),
        Err(_) => Connection::open_with_flags(
            format!("file:{}?immutable=1", encode(&database.to_string_lossy(), b"-_.~/:")),
            flags | OpenFlags::SQLITE_OPEN_URI,
        ),
    }
}

/// The columns a table has, so older and newer schemas both read.
fn columns(db: &Connection, table: &str) -> Vec<String> {
    let Ok(mut statement) = db.prepare(&format!("PRAGMA table_info({table})")) else {
        return Vec::new();
    };
    statement
        .query_map([], |row| row.get::<_, String>(1))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

fn select_list(have: &[String], table: &str, wanted: &[&str]) -> String {
    wanted
        .iter()
        .map(|name| {
            if have.iter().any(|c| c == name) {
                format!("{table}.{name}")
            } else {
                "NULL".to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The newest `limit` sessions in one database that have transcript rows.
pub fn list(provider: Provider, database: &Path, limit: usize) -> Vec<SessionInfo> {
    let Ok(db) = open(database) else { return Vec::new() };
    let rows: Vec<(String, String, String, f64)> = match provider {
        Provider::OpenClaw => {
            let sql = "SELECT e.session_id,
                    COALESCE((SELECT json_extract(h.event_json, '$.cwd') FROM transcript_events h
                              WHERE h.session_id = e.session_id AND json_extract(h.event_json, '$.type') = 'session'
                              ORDER BY h.seq LIMIT 1), ''),
                    COALESCE(NULLIF(w.display_name, ''), NULLIF(n.label, ''), NULLIF(n.display_name, ''), w.session_key, ''),
                    MAX(e.created_at)
                FROM transcript_events e
                LEFT JOIN session_windows w ON w.session_id = e.session_id
                LEFT JOIN session_nodes n ON n.session_key = w.session_key
                GROUP BY e.session_id ORDER BY 4 DESC LIMIT ?1";
            query_sessions(&db, sql, limit)
        }
        _ => {
            let have = columns(&db, "sessions");
            let picked = select_list(&have, "s", &["cwd", "title"]);
            let sql = format!(
                "SELECT s.id, {picked},
                    (SELECT content FROM messages u WHERE u.session_id = s.id AND u.role = 'user' ORDER BY u.id LIMIT 1),
                    MAX(m.timestamp) * 1000
                 FROM sessions s JOIN messages m ON m.session_id = s.id
                 GROUP BY s.id ORDER BY 5 DESC LIMIT ?1"
            );
            let Ok(mut statement) = db.prepare(&sql) else { return Vec::new() };
            statement
                .query_map([limit as i64], |row| {
                    let title: Option<String> = row.get(2)?;
                    let first: Option<String> = row.get(3)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        title.filter(|t| !t.is_empty()).or(first).unwrap_or_default(),
                        row.get::<_, Option<f64>>(4)?.unwrap_or(0.0),
                    ))
                })
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default()
        }
    };
    rows.into_iter()
        .map(|(id, cwd, title, updated)| SessionInfo {
            provider,
            locator: locator(provider, database, &id),
            id,
            cwd,
            title: title_from(&title),
            updated_ms: updated as i64,
        })
        .collect()
}

fn query_sessions(db: &Connection, sql: &str, limit: usize) -> Vec<(String, String, String, f64)> {
    let Ok(mut statement) = db.prepare(sql) else { return Vec::new() };
    statement
        .query_map([limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
            ))
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

/// A whole session, read through the Pi reader.
pub fn events(locator: &str) -> Result<Vec<Event>, String> {
    Ok(crate::pi::events(&render(locator)?))
}

/// The session as Pi-format JSONL.
pub fn render(locator: &str) -> Result<String, String> {
    let (provider, database, session) = parse_locator(locator)?;
    let db = open(Path::new(&database)).map_err(|e| format!("open {database}: {e}"))?;
    let lines = match provider {
        Provider::OpenClaw => openclaw_lines(&db, &session),
        _ => hermes_lines(&db, &session),
    }
    .map_err(|e| format!("read {locator}: {e}"))?;
    if lines.is_empty() {
        return Err(format!("no transcript rows for {locator}"));
    }
    Ok(lines.join("\n") + "\n")
}

fn openclaw_lines(db: &Connection, session: &str) -> rusqlite::Result<Vec<String>> {
    let mut statement = db.prepare("SELECT event_json, event_zstd FROM transcript_events WHERE session_id = ?1 ORDER BY seq")?;
    let rows = statement
        .query_map([session], |row| {
            Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(text, compressed)| text.or_else(|| compressed.and_then(|bytes| decompress(&bytes))))
        .map(|line| line.replace('\n', " "))
        .collect())
}

fn decompress(bytes: &[u8]) -> Option<String> {
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(bytes).ok()?;
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).ok()?;
    String::from_utf8(out).ok()
}

fn iso(seconds: Option<f64>) -> Value {
    seconds
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(|s| {
            let time = std::time::UNIX_EPOCH + std::time::Duration::from_millis((s * 1000.0) as u64);
            Value::String(ruddr_core::time::format_rfc3339(time))
        })
        .unwrap_or(Value::Null)
}

fn hermes_lines(db: &Connection, session: &str) -> rusqlite::Result<Vec<String>> {
    let have = columns(db, "sessions");
    let header = db
        .query_row(
            &format!(
                "SELECT {} FROM sessions s WHERE s.id = ?1",
                select_list(&have, "s", &["started_at", "cwd"])
            ),
            [session],
            |row| Ok((row.get::<_, Option<f64>>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    let mut lines = Vec::new();
    if let Some((started, cwd)) = header {
        let mut row = json!({ "type": "session", "version": 3, "id": session, "timestamp": iso(started) });
        if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
            row["cwd"] = json!(cwd);
        }
        lines.push(row.to_string());
    }
    let have = columns(db, "messages");
    let picked = select_list(
        &have,
        "m",
        &[
            "id",
            "role",
            "content",
            "tool_calls",
            "tool_call_id",
            "tool_name",
            "timestamp",
            "reasoning",
            "reasoning_content",
            "active",
        ],
    );
    let mut statement = db.prepare(&format!("SELECT {picked} FROM messages m WHERE m.session_id = ?1 ORDER BY m.id"))?;
    let rows = statement
        .query_map([session], |row| {
            Ok(HermesRow {
                id: row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                role: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                content: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                tool_calls: row.get(3)?,
                tool_call_id: row.get(4)?,
                tool_name: row.get(5)?,
                timestamp: row.get(6)?,
                reasoning: row.get::<_, Option<String>>(7)?.or(row.get::<_, Option<String>>(8)?),
                active: row.get::<_, Option<i64>>(9)?.unwrap_or(1) != 0,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut parent: Option<String> = None;
    for row in rows.into_iter().filter(|row| row.active) {
        let Some(message) = hermes_message(&row) else { continue };
        let id = format!("m{}", row.id);
        lines.push(
            json!({ "type": "message", "id": id, "parentId": parent, "timestamp": iso(row.timestamp), "message": message }).to_string(),
        );
        parent = Some(id);
    }
    Ok(lines)
}

struct HermesRow {
    id: i64,
    role: String,
    content: String,
    tool_calls: Option<String>,
    tool_call_id: Option<String>,
    tool_name: Option<String>,
    timestamp: Option<f64>,
    reasoning: Option<String>,
    active: bool,
}

fn hermes_message(row: &HermesRow) -> Option<Value> {
    match row.role.as_str() {
        "user" => Some(json!({ "role": "user", "content": [{ "type": "text", "text": row.content }] })),
        "assistant" => {
            let mut content = Vec::new();
            if let Some(reasoning) = row.reasoning.as_deref().filter(|r| !r.trim().is_empty()) {
                content.push(json!({ "type": "thinking", "thinking": reasoning }));
            }
            if !row.content.is_empty() {
                content.push(json!({ "type": "text", "text": row.content }));
            }
            content.extend(tool_calls(row.tool_calls.as_deref()));
            Some(json!({ "role": "assistant", "content": content }))
        }
        "tool" => Some(json!({
            "role": "toolResult", "toolCallId": row.tool_call_id, "toolName": row.tool_name,
            "content": [{ "type": "text", "text": row.content }], "isError": false,
        })),
        _ => None,
    }
}

/// OpenAI `tool_calls` as Pi `toolCall` blocks with parsed arguments.
fn tool_calls(raw: Option<&str>) -> Vec<Value> {
    let Some(Value::Array(calls)) = raw.and_then(|r| serde_json::from_str::<Value>(r).ok()) else {
        return Vec::new();
    };
    calls
        .iter()
        .filter_map(Value::as_object)
        .map(|call| {
            let function = call.get("function").and_then(Value::as_object);
            let name = function.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or("tool");
            let arguments = match function.and_then(|f| f.get("arguments")) {
                Some(Value::String(text)) => serde_json::from_str(text).unwrap_or(json!({ "input": text })),
                Some(other) => other.clone(),
                None => Value::Object(Map::new()),
            };
            json!({ "type": "toolCall", "id": call.get("id"), "name": name, "arguments": arguments })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChangeKind;

    fn temp_db(name: &str, schema: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ruddr-history-{name}-{}", ruddr_core::fsutil::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store#1.sqlite");
        Connection::open(&path).unwrap().execute_batch(schema).unwrap();
        path
    }

    #[test]
    fn locators_match_dejavu_and_round_trip() {
        let made = locator(Provider::Hermes, Path::new("/home/u/my dir#2/state.db"), "s 1");
        assert_eq!(made, "hermes:///home/u/my%20dir%232/state.db#s%201");
        assert_eq!(
            parse_locator(&made).unwrap(),
            (Provider::Hermes, "/home/u/my dir#2/state.db".to_string(), "s 1".to_string())
        );
        let windows = locator(Provider::OpenClaw, Path::new(r"C:\Users\me\a.sqlite"), "x");
        assert!(windows.starts_with("openclaw:///C:%5CUsers"));
        assert_eq!(parse_locator(&windows).unwrap().1, r"C:\Users\me\a.sqlite");
        assert!(parse_locator("opencode:///x#y").is_err());
    }

    #[test]
    fn openclaw_sessions_list_with_cwd_and_title_and_read_zstd_rows() {
        let path = temp_db(
            "openclaw",
            "CREATE TABLE transcript_events (session_id TEXT, seq INTEGER, event_json TEXT, created_at INTEGER, event_zstd BLOB);
             CREATE TABLE session_windows (session_id TEXT, session_key TEXT, display_name TEXT);
             CREATE TABLE session_nodes (session_key TEXT, label TEXT, display_name TEXT);
             INSERT INTO session_windows VALUES ('s1', 'agent:main:telegram:direct:1', NULL);",
        );
        let db = Connection::open(&path).unwrap();
        db.execute(
            "INSERT INTO transcript_events VALUES ('s1', 0, ?1, 1000, NULL)",
            [r#"{"type":"session","version":4,"id":"s1","cwd":"/w"}"#],
        )
        .unwrap();
        let edit = r#"{"type":"message","id":"a","parentId":null,"message":{"role":"assistant","content":[{"type":"toolCall","id":"t","name":"write","arguments":{"path":"/w/n.txt","content":"hi\n"}}]}}"#;
        // A raw (uncompressed) zstd frame: magic, single-segment header, one raw last block.
        let mut frame = vec![0x28, 0xB5, 0x2F, 0xFD, 0x20, edit.len() as u8];
        frame.extend_from_slice(&(((edit.len() as u32) << 3) | 1).to_le_bytes()[..3]);
        frame.extend_from_slice(edit.as_bytes());
        db.execute("INSERT INTO transcript_events VALUES ('s1', 1, NULL, 2000, ?1)", [frame])
            .unwrap();
        drop(db);
        let sessions = list(Provider::OpenClaw, &path, 10);
        assert_eq!(sessions.len(), 1);
        let info = &sessions[0];
        assert_eq!((info.id.as_str(), info.cwd.as_str(), info.updated_ms), ("s1", "/w", 2000));
        assert_eq!(info.title, "agent:main:telegram:direct:1");
        let changes: Vec<_> = events(&info.locator)
            .unwrap()
            .into_iter()
            .filter_map(|e| match e {
                Event::FileChange { path, kind, .. } => Some((path, kind)),
                _ => None,
            })
            .collect();
        assert_eq!(changes, [("/w/n.txt".to_string(), ChangeKind::Add)]);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn hermes_sessions_become_pi_messages_with_tool_calls_and_edits() {
        let path = temp_db(
            "hermes",
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, started_at REAL, cwd TEXT, title TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, session_id TEXT, role TEXT, content TEXT, tool_calls TEXT,
                tool_call_id TEXT, tool_name TEXT, timestamp REAL, reasoning TEXT, active INTEGER DEFAULT 1);
             INSERT INTO sessions VALUES ('h1', 100, '/home/u', NULL);
             INSERT INTO messages VALUES (1, 'h1', 'user', 'fix the typo', NULL, NULL, NULL, 100, NULL, 1);
             INSERT INTO messages VALUES (2, 'h1', 'assistant', '', '[{\"id\":\"c1\",\"function\":{\"name\":\"patch\",\"arguments\":\"{\\\"mode\\\":\\\"replace\\\",\\\"path\\\":\\\"/home/u/a.md\\\",\\\"old_string\\\":\\\"teh\\\",\\\"new_string\\\":\\\"the\\\"}\"}}]', NULL, NULL, 101, NULL, 1);
             INSERT INTO messages VALUES (3, 'h1', 'tool', 'ok', NULL, 'c1', 'patch', 102, NULL, 1);
             INSERT INTO messages VALUES (4, 'h1', 'assistant', 'rewound', NULL, NULL, NULL, 103, NULL, 0);
             INSERT INTO messages VALUES (5, 'h1', 'assistant', 'Fixed.', NULL, NULL, NULL, 104, NULL, 1);",
        );
        let sessions = list(Provider::Hermes, &path, 10);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, "fix the typo", "no title falls back to the first prompt");
        assert_eq!((sessions[0].cwd.as_str(), sessions[0].updated_ms), ("/home/u", 104_000));
        let events = events(&sessions[0].locator).unwrap();
        assert!(events.iter().any(|e| matches!(e, Event::User { text } if text == "fix the typo")));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::FileChange { path, hunks, .. } if path == "/home/u/a.md" && hunks.contains("+the")))
        );
        assert!(events.iter().any(|e| matches!(e, Event::Assistant { text } if text == "Fixed.")));
        assert!(!events.iter().any(|e| matches!(e, Event::Assistant { text } if text == "rewound")));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
