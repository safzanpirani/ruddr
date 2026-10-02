//! The Activity tab: `trace.log` lines folded into activities, with tool
//! details from `events.jsonl` attached. Port of `parseTraceActivities`,
//! `parseToolEventDetails`, and `attachToolDetails` in tui/core.ts.

use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceActivity {
    pub timestamp: String,
    pub kind: &'static str,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
}

fn timestamp_ms(text: &str) -> Option<i64> {
    ruddr_core::time::parse_rfc3339_ms(text)
}

/// Splits `TIMESTAMP [TAG] TEXT`, as the regex `^(\S+) \[([^\]]+)\](?: (.*))?$`
/// does. `.` in that regex stops at line terminators, so a line holding one
/// does not match.
fn split_trace_line(line: &str) -> Option<(&str, &str, &str)> {
    let space = line.find(char::is_whitespace)?;
    if space == 0 || !line[space..].starts_with(' ') {
        return None;
    }
    let (timestamp, rest) = (&line[..space], &line[space + 1..]);
    let rest = rest.strip_prefix('[')?;
    let close = rest.find(']')?;
    if close == 0 {
        return None;
    }
    let (tag, after) = (&rest[..close], &rest[close + 1..]);
    let text = match after {
        "" => "",
        _ => after.strip_prefix(' ')?,
    };
    if text.contains(['\r', '\n', '\u{2028}', '\u{2029}']) {
        return None;
    }
    Some((timestamp, tag, text))
}

/// Joins `** **` runs into ` · `, drops the remaining `**`, and trims.
fn clean_thought(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("**") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let spaces = after.len() - after.trim_start().len();
        if spaces > 0 && after[spaces..].starts_with("**") {
            out.push_str(" · ");
            rest = &after[spaces + 2..];
        } else {
            // The regex retries one character later, as in `*** **`.
            out.push('*');
            rest = &rest[start + 1..];
        }
    }
    out.push_str(rest);
    out.replace("**", "").trim().to_string()
}

fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return if path.is_empty() { "" } else { "/" };
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

fn parse_tool(text: &str) -> (String, String) {
    if text == "file changes" {
        return ("files".into(), "changed".into());
    }
    let command = text.strip_prefix("$ ").unwrap_or(text);
    let shell = [
        "/bin/zsh -lc ",
        "/bin/bash -lc ",
        "/bin/sh -lc ",
        "zsh -lc ",
        "bash -lc ",
        "sh -lc ",
    ]
    .iter()
    .find_map(|prefix| command.strip_prefix(prefix))
    .filter(|script| !script.is_empty());
    if let Some(quoted) = shell {
        // Mirrors the TypeScript: strip a leading quote, then a trailing copy
        // of the first character.
        let quote = quoted.chars().next().unwrap_or_default();
        let mut script = if quote == '"' || quote == '\'' { &quoted[1..] } else { quoted };
        if script.ends_with(quote) {
            script = &script[..script.len() - quote.len_utf8()];
        }
        return ("shell".into(), script.to_string());
    }
    let executable = if command.starts_with(char::is_whitespace) {
        ""
    } else {
        command.split(char::is_whitespace).next().unwrap_or("")
    };
    let label = basename(executable);
    (
        if label.is_empty() { "tool".into() } else { label.to_string() },
        command.to_string(),
    )
}

pub fn parse_trace_activities(content: &str) -> Vec<TraceActivity> {
    let mut activities: Vec<TraceActivity> = Vec::new();
    for line in content.split('\n') {
        let Some((timestamp, tag, raw)) = split_trace_line(line) else {
            continue;
        };
        let activity = |kind, text: String, label: Option<String>| TraceActivity {
            timestamp: timestamp.to_string(),
            kind,
            text,
            label,
            tool_status: None,
            duration_ms: None,
        };
        match tag {
            "usage" => {}
            "think" => {
                let text = clean_thought(raw);
                if !text.is_empty() {
                    activities.push(activity("thought", text, None));
                }
            }
            "say" => {
                if !raw.is_empty() {
                    activities.push(activity("message", raw.to_string(), None));
                }
            }
            "in_progress" | "completed" | "failed" => {
                let status = match tag {
                    "in_progress" => "running",
                    "completed" => "completed",
                    _ => "failed",
                };
                let (label, text) = parse_tool(raw);
                if status != "running" {
                    let running = activities.iter().rposition(|a| {
                        a.kind == "tool" && a.tool_status == Some("running") && a.label.as_deref() == Some(&label) && a.text == text
                    });
                    if let Some(index) = running {
                        let duration = match (timestamp_ms(&activities[index].timestamp), timestamp_ms(timestamp)) {
                            (Some(started), Some(finished)) => Some((finished - started).max(0)),
                            _ => None,
                        };
                        activities[index] = TraceActivity {
                            tool_status: Some(status),
                            duration_ms: duration,
                            ..activity("tool", text, Some(label))
                        };
                        continue;
                    }
                }
                activities.push(TraceActivity {
                    tool_status: Some(status),
                    ..activity("tool", text, Some(label))
                });
            }
            "warn" => activities.push(activity("warning", raw.to_string(), None)),
            "error" => activities.push(activity("error", raw.to_string(), None)),
            other => activities.push(activity("status", raw.to_string(), Some(other.to_string()))),
        }
    }
    let latest_message = activities.iter().rposition(|a| a.kind == "message");
    activities
        .into_iter()
        .enumerate()
        .filter(|(index, a)| a.kind != "message" || Some(*index) == latest_message)
        .map(|(_, a)| a)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolEventDetail {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<Value>,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_thread_id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_path: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity_kind: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp_ms: Option<Value>,
}

/// `a ?? b`: a missing or null value falls back.
fn coalesce(value: Option<&Value>, fallback: Option<Value>) -> Option<Value> {
    match value {
        Some(Value::Null) | None => fallback,
        Some(value) => Some(value.clone()),
    }
}

fn truthy_str(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|s| !s.is_empty())
}

const TOOL_TYPES: [&str; 5] = ["commandExecution", "webSearch", "fileChange", "toolCall", "subAgentActivity"];

pub fn parse_tool_event_details(content: &str) -> Vec<ToolEventDetail> {
    let mut by_id: HashMap<String, ToolEventDetail> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut messages_by_thread: HashMap<String, String> = HashMap::new();
    let mut turns_by_thread: HashMap<String, Map<String, Value>> = HashMap::new();
    for line in content.split('\n') {
        // A bounded tail may begin in the middle of a JSONL record.
        let Ok(event) = serde_json::from_str::<Value>(line) else { continue };
        let method = event.get("method").and_then(Value::as_str);
        let params = event.get("params");
        let item = params.and_then(|p| p.get("item")).filter(|i| !i.is_null());
        let thread = truthy_str(params.and_then(|p| p.get("threadId")));
        let item_type = item.and_then(|i| i.get("type")).and_then(Value::as_str);
        if method == Some("item/completed")
            && item_type == Some("agentMessage")
            && let (Some(text), Some(thread)) = (truthy_str(item.and_then(|i| i.get("text"))), thread)
        {
            messages_by_thread.insert(thread.to_string(), text.to_string());
        }
        if method == Some("turn/completed")
            && let Some(thread) = thread
        {
            let turn = params
                .and_then(|p| p.get("turn"))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            turns_by_thread.insert(thread.to_string(), turn);
        }
        let Some(item) = item else { continue };
        let (Some(id), Some(kind)) = (truthy_str(item.get("id")), item_type.filter(|t| !t.is_empty())) else {
            continue;
        };
        if !method.is_some_and(|m| m.starts_with("item/")) || !TOOL_TYPES.contains(&kind) {
            continue;
        }
        let previous = by_id.get(id).cloned();
        if previous.is_none() {
            order.push(id.to_string());
        }
        let field = |name: &str| item.get(name);
        let exit_code = field("exitCode");
        let failed_exit = exit_code.is_some_and(|code| code.as_f64() != Some(0.0));
        let status = if method == Some("item/started")
            || (method == Some("item/updated") && field("status").and_then(Value::as_str) == Some("inProgress"))
        {
            "running"
        } else if field("status").and_then(Value::as_str) == Some("failed") || failed_exit {
            "failed"
        } else {
            "completed"
        };
        let sub_agent = kind == "subAgentActivity";
        let prev = |pick: fn(&ToolEventDetail) -> &Option<Value>| previous.as_ref().and_then(|p| pick(p).clone());
        let command = coalesce(
            field("command"),
            if sub_agent {
                coalesce(field("agentPath"), prev(|p| &p.command))
            } else {
                prev(|p| &p.command)
            },
        );
        let tool_name = coalesce(
            field("toolName"),
            if sub_agent {
                Some(Value::String("subAgentActivity".into()))
            } else {
                prev(|p| &p.tool_name)
            },
        );
        let detail = ToolEventDetail {
            id: id.to_string(),
            kind: kind.to_string(),
            command,
            cwd: coalesce(field("cwd"), prev(|p| &p.cwd)),
            status,
            output: coalesce(field("aggregatedOutput"), prev(|p| &p.output)),
            exit_code: coalesce(exit_code, prev(|p| &p.exit_code)),
            duration_ms: coalesce(field("durationMs"), prev(|p| &p.duration_ms)),
            query: coalesce(field("query"), prev(|p| &p.query)),
            tool_name,
            input: coalesce(field("input"), prev(|p| &p.input)),
            agent_thread_id: coalesce(field("agentThreadId"), prev(|p| &p.agent_thread_id)),
            agent_path: coalesce(field("agentPath"), prev(|p| &p.agent_path)),
            activity_kind: coalesce(field("kind"), prev(|p| &p.activity_kind)),
            timestamp_ms: coalesce(event.get("emittedAtMs"), prev(|p| &p.timestamp_ms)),
        };
        by_id.insert(id.to_string(), detail);
    }
    for detail in by_id.values_mut() {
        if detail.kind != "subAgentActivity" {
            continue;
        }
        let Some(thread) = detail
            .agent_thread_id
            .as_ref()
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
        else {
            continue;
        };
        if let Some(message) = messages_by_thread.get(&thread) {
            detail.output = Some(Value::String(message.clone()));
        }
        if let Some(turn) = turns_by_thread.get(&thread) {
            detail.duration_ms = coalesce(turn.get("durationMs"), detail.duration_ms.take());
            if let Some(status) = truthy_str(turn.get("status")) {
                detail.status = if status == "completed" { "completed" } else { "failed" };
            }
        }
    }
    order.into_iter().filter_map(|id| by_id.remove(&id)).collect()
}

/// Text of a detail field for matching: strings as is, other values as
/// JavaScript would print them in a template literal.
fn text_of(value: &Option<Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(false)) => String::new(),
        Some(other) => other.to_string(),
    }
}

pub fn attach_tool_details(activities: &[TraceActivity], details: &[ToolEventDetail]) -> Vec<Option<ToolEventDetail>> {
    let mut used: HashSet<String> = HashSet::new();
    activities
        .iter()
        .map(|activity| {
            if activity.kind != "tool" {
                return None;
            }
            let summary = activity.text.strip_suffix('…').unwrap_or(&activity.text).to_lowercase();
            let label = activity.label.as_deref().map(str::to_lowercase);
            let candidates: Vec<&ToolEventDetail> = details
                .iter()
                .filter(|detail| {
                    if used.contains(&detail.id) {
                        return false;
                    }
                    let text = format!(
                        "{} {} {}",
                        text_of(&detail.command),
                        text_of(&detail.query),
                        text_of(&detail.tool_name)
                    )
                    .to_lowercase();
                    if !summary.is_empty() && (text.contains(&summary) || summary.contains(text.trim())) {
                        return true;
                    }
                    if activity.label.as_deref() == Some("files") {
                        return detail.kind == "fileChange";
                    }
                    if label.as_deref().is_some_and(|l| l.contains("websearch")) {
                        return detail.kind == "webSearch";
                    }
                    if label.as_deref() == Some("subagentactivity") {
                        return detail.kind == "subAgentActivity";
                    }
                    activity.label.as_deref() == Some("shell") && detail.kind == "commandExecution"
                })
                .collect();
            let mut found = candidates.first().copied();
            if label.as_deref() == Some("subagentactivity") {
                let activity_time = timestamp_ms(&activity.timestamp);
                found = activity_time.and_then(|at| {
                    candidates
                        .iter()
                        .filter_map(|d| d.timestamp_ms.as_ref().and_then(Value::as_f64).map(|t| ((t - at as f64).abs(), *d)))
                        .filter(|(distance, _)| *distance < 2000.0)
                        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
                        .map(|(_, d)| d)
                });
            }
            let found = found.cloned();
            if let Some(detail) = &found {
                used.insert(detail.id.clone());
            }
            found
        })
        .collect()
}

/// The `/api/run/activity` body.
pub fn activity_json(trace: &str, events: &str) -> Value {
    let activities = parse_trace_activities(trace);
    let details = attach_tool_details(&activities, &parse_tool_event_details(events));
    let rows: Vec<Value> = activities
        .iter()
        .zip(details)
        .map(|(activity, detail)| {
            let mut row = serde_json::to_value(activity).unwrap_or_default();
            if let (Some(detail), Some(object)) = (detail, row.as_object_mut()) {
                object.insert("detail".into(), serde_json::to_value(detail).unwrap_or_default());
            }
            row
        })
        .collect();
    serde_json::json!({ "activities": rows })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_tool_runs_and_keeps_the_latest_message() {
        let trace = "\
2026-10-02T09:00:00Z [say] first
2026-10-02T09:00:01Z [think] **Plan** **next**
2026-10-02T09:00:02Z [in_progress] $ /bin/zsh -lc 'ls -la'
2026-10-02T09:00:04Z [completed] $ /bin/zsh -lc 'ls -la'
2026-10-02T09:00:05Z [usage] 10 tokens
2026-10-02T09:00:06Z [completed] file changes
2026-10-02T09:00:07Z [warn] careful
2026-10-02T09:00:08Z [turn] started
2026-10-02T09:00:09Z [say] second
not a trace line";
        let activities = parse_trace_activities(trace);
        let kinds: Vec<&str> = activities.iter().map(|a| a.kind).collect();
        assert_eq!(kinds, ["thought", "tool", "tool", "warning", "status", "message"]);
        assert_eq!(activities[0].text, "Plan · next");
        assert_eq!(
            (activities[1].label.as_deref(), activities[1].text.as_str()),
            (Some("shell"), "ls -la")
        );
        assert_eq!(
            (activities[1].tool_status, activities[1].duration_ms),
            (Some("completed"), Some(2000))
        );
        assert_eq!(
            (activities[2].label.as_deref(), activities[2].text.as_str()),
            (Some("files"), "changed")
        );
        assert_eq!(activities[4].label.as_deref(), Some("turn"));
        assert_eq!(activities[5].text, "second");
        assert_eq!(parse_tool("$ /usr/bin/git status"), ("git".into(), "/usr/bin/git status".into()));
        assert_eq!(clean_thought("*** **x"), "* · x");
        assert_eq!(clean_thought("  **a**  "), "a");
    }

    #[test]
    fn attaches_event_details_to_tool_activities() {
        let events = [
            r#"{"method":"item/started","emittedAtMs":1,"params":{"item":{"id":"c1","type":"commandExecution","command":"ls -la","cwd":"/w"}}}"#,
            r#"{"method":"item/completed","params":{"item":{"id":"c1","type":"commandExecution","aggregatedOutput":"out","exitCode":2}}}"#,
            r#"{"method":"item/completed","params":{"item":{"id":"f1","type":"fileChange"}}}"#,
            r#"{"method":"item/completed","params":{"item":{"id":"m","type":"agentMessage","text":"hi"}}}"#,
            r#"{"partial"#,
        ]
        .join("\n");
        let details = parse_tool_event_details(&events);
        assert_eq!(details.len(), 2);
        assert_eq!(details[0].status, "failed");
        assert_eq!(details[0].command, Some(Value::String("ls -la".into())));
        assert_eq!(details[0].exit_code, Some(serde_json::json!(2)));
        let body = activity_json(
            "2026-10-02T09:00:02Z [completed] $ ls -la\n2026-10-02T09:00:03Z [completed] file changes",
            &events,
        );
        assert_eq!(body["activities"][0]["detail"]["id"], "c1");
        assert_eq!(body["activities"][0]["detail"]["output"], "out");
        assert_eq!(body["activities"][1]["detail"]["id"], "f1");
    }

    #[test]
    fn sub_agent_details_take_the_child_thread_result() {
        let events = [
            r#"{"method":"item/started","emittedAtMs":1791000000500,"params":{"item":{"id":"s1","type":"subAgentActivity","agentPath":"reviewer","agentThreadId":"child"}}}"#,
            r#"{"method":"item/completed","params":{"threadId":"child","item":{"id":"a","type":"agentMessage","text":"child says"}}}"#,
            r#"{"method":"turn/completed","params":{"threadId":"child","turn":{"status":"failed","durationMs":42}}}"#,
        ]
        .join("\n");
        let details = parse_tool_event_details(&events);
        assert_eq!(details[0].output, Some(Value::String("child says".into())));
        assert_eq!(
            (details[0].status, details[0].duration_ms.clone()),
            ("failed", Some(serde_json::json!(42)))
        );
        assert_eq!(details[0].tool_name, Some(Value::String("subAgentActivity".into())));
        let activity = TraceActivity {
            timestamp: "2026-10-03T04:00:00Z".into(),
            kind: "tool",
            text: "unrelated".into(),
            label: Some("subAgentActivity".into()),
            tool_status: Some("running"),
            duration_ms: None,
        };
        assert_eq!(
            attach_tool_details(&[activity], &details)[0].as_ref().map(|d| d.id.as_str()),
            Some("s1")
        );
    }
}
