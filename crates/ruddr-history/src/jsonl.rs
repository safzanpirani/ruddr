//! JSONL parsing and the tree walks for Claude, Pi, and omp, which record
//! every branch of a conversation in one file.

use serde_json::Value;
use std::collections::HashMap;

pub fn parse(text: &str) -> Vec<Value> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(Value::is_object)
        .collect()
}

fn id<'a>(entry: &'a Value, key: &str) -> Option<&'a str> {
    entry.get(key).and_then(Value::as_str).filter(|v| !v.is_empty())
}

/// Claude's active branch: from the recorded leaf (or the last entry with a
/// uuid) back through `parentUuid`, following `logicalParentUuid` across a
/// compaction boundary.
pub fn claude_branch(entries: Vec<Value>) -> Vec<Value> {
    let by_id: HashMap<String, usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| id(e, "uuid").map(|u| (u.to_string(), i)))
        .collect();
    let leaf = entries
        .iter()
        .rev()
        .find(|e| id(e, "type") == Some("last-prompt"))
        .and_then(|e| id(e, "leafUuid"))
        .and_then(|leaf| by_id.get(leaf).copied())
        .or_else(|| entries.iter().rposition(|e| id(e, "uuid").is_some()));
    walk(&entries, leaf, &by_id, &["parentUuid", "logicalParentUuid"])
}

/// Pi's active branch: from the last entry back through `parentId`. The
/// `session` header and omp's `title` row are not tree nodes.
pub fn pi_branch(entries: Vec<Value>) -> Vec<Value> {
    let tree: Vec<Value> = entries
        .into_iter()
        .filter(|e| !matches!(id(e, "type"), Some("session" | "title")))
        .collect();
    let by_id: HashMap<String, usize> = tree
        .iter()
        .enumerate()
        .filter_map(|(i, e)| id(e, "id").map(|u| (u.to_string(), i)))
        .collect();
    let leaf = tree.len().checked_sub(1);
    walk(&tree, leaf, &by_id, &["parentId"])
}

fn walk(entries: &[Value], leaf: Option<usize>, by_id: &HashMap<String, usize>, parent_keys: &[&str]) -> Vec<Value> {
    let mut branch = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut current = leaf;
    while let Some(index) = current {
        if !seen.insert(index) {
            break; // A cycle in a damaged file.
        }
        let entry = &entries[index];
        branch.push(entry.clone());
        current = parent_keys
            .iter()
            .find_map(|key| id(entry, key))
            .and_then(|parent| by_id.get(parent).copied());
    }
    branch.reverse();
    branch
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_follows_the_recorded_leaf_across_compaction() {
        let entries = vec![
            json!({"uuid": "a", "parentUuid": null}),
            json!({"uuid": "b", "parentUuid": "a"}),
            json!({"uuid": "dead", "parentUuid": "a"}),
            json!({"uuid": "c", "parentUuid": null, "logicalParentUuid": "b"}),
            json!({"uuid": "d", "parentUuid": "c"}),
            json!({"type": "last-prompt", "leafUuid": "d"}),
        ];
        let ids: Vec<String> = claude_branch(entries)
            .iter()
            .map(|e| e["uuid"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, ["a", "b", "c", "d"]);
    }

    #[test]
    fn pi_follows_parent_ids_from_the_last_entry() {
        let entries = vec![
            json!({"type": "session", "cwd": "/w"}),
            json!({"id": "1", "parentId": null}),
            json!({"id": "2", "parentId": "1"}),
            json!({"id": "x", "parentId": "1"}),
            json!({"id": "3", "parentId": "2"}),
        ];
        let ids: Vec<String> = pi_branch(entries).iter().map(|e| e["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(ids, ["1", "2", "3"]);
    }
}
