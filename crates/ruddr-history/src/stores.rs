//! Where each agent keeps its transcripts. A set variable replaces the
//! home-directory default the way the agent itself resolves it: Claude Code
//! `$CLAUDE_CONFIG_DIR/projects`, Codex `$CODEX_HOME/sessions`, Pi
//! `$PI_CODING_AGENT_DIR/sessions` (otherwise every `~/.pi/*/sessions`
//! profile), OpenCode `$OPENCODE_DB` or `$XDG_DATA_HOME/opencode/*.db`, and
//! Droid `~/.factory/sessions`.

use crate::Provider;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, Default)]
pub struct Stores {
    pub claude: Option<PathBuf>,
    pub codex: Option<PathBuf>,
    pub pi: Vec<PathBuf>,
    pub opencode: Vec<PathBuf>,
    pub droid: Option<PathBuf>,
}

impl Stores {
    /// The stores this machine has, from the environment and home directory.
    pub fn discover() -> Stores {
        Stores::from_env(
            &|name| std::env::var(name).ok().filter(|v| !v.is_empty()),
            &ruddr_core::paths::home_dir(),
        )
    }

    pub fn from_env(env: &dyn Fn(&str) -> Option<String>, home: &Path) -> Stores {
        let configured = |name: &str, fallback: PathBuf| env(name).map(PathBuf::from).unwrap_or(fallback);
        let existing = |path: PathBuf| path.exists().then_some(path);
        let pi = match env("PI_CODING_AGENT_DIR") {
            Some(dir) => {
                let dir = dir
                    .strip_prefix('~')
                    .map(|rest| home.join(rest.trim_start_matches(['/', '\\'])))
                    .unwrap_or_else(|| PathBuf::from(&dir));
                existing(dir.join("sessions")).into_iter().collect()
            }
            None => {
                let mut profiles: Vec<PathBuf> = std::fs::read_dir(home.join(".pi"))
                    .map(|entries| {
                        entries
                            .flatten()
                            .map(|e| e.path().join("sessions"))
                            .filter(|p| p.is_dir())
                            .collect()
                    })
                    .unwrap_or_default();
                profiles.sort();
                profiles
            }
        };
        let opencode_data = configured("XDG_DATA_HOME", home.join(".local").join("share")).join("opencode");
        let opencode = match env("OPENCODE_DB") {
            Some(db) if db == ":memory:" => Vec::new(),
            Some(db) => {
                let path = PathBuf::from(&db);
                vec![if path.is_absolute() { path } else { opencode_data.join(db) }]
            }
            None => ["opencode.db", "opencode-next.db", "opencode-local.db"]
                .iter()
                .map(|name| opencode_data.join(name))
                .collect(),
        }
        .into_iter()
        .filter(|path| path.is_file())
        .collect();
        Stores {
            claude: existing(configured("CLAUDE_CONFIG_DIR", home.join(".claude")).join("projects")),
            codex: existing(configured("CODEX_HOME", home.join(".codex")).join("sessions")),
            pi,
            opencode,
            droid: existing(home.join(".factory").join("sessions")),
        }
    }

    /// Every JSONL transcript with its provider and modification time
    /// (Unix ms). OpenCode sessions live in SQLite and are listed separately.
    pub fn transcript_files(&self) -> Vec<(Provider, PathBuf, i64)> {
        let mut files = Vec::new();
        if let Some(root) = &self.claude {
            // <projects>/<encoded project>/<session>.jsonl; subagent logs sit deeper.
            collect(root, 2, Provider::Claude, &mut files);
        }
        if let Some(root) = &self.codex {
            // <sessions>/YYYY/MM/DD/rollout-*.jsonl
            collect(root, 4, Provider::Codex, &mut files);
        }
        for root in &self.pi {
            collect(root, 2, Provider::Pi, &mut files);
        }
        if let Some(root) = &self.droid {
            collect(root, 2, Provider::Droid, &mut files);
        }
        files
    }
}

/// Collects `*.jsonl` files exactly `depth` levels below `dir`.
fn collect(dir: &Path, depth: usize, provider: Provider, out: &mut Vec<(Provider, PathBuf, i64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        let path = entry.path();
        if depth > 1 {
            if kind.is_dir() {
                collect(&path, depth - 1, provider, out);
            }
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            out.push((provider, path, mtime));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_replace_defaults_and_pi_profiles_are_found() {
        let home = std::env::temp_dir().join(format!("ruddr-history-stores-{}", ruddr_core::fsutil::random_hex(4)));
        for dir in [
            ".claude/projects/-w/",
            ".codex/sessions/2026/10/02",
            ".pi/agent/sessions/--w--",
            ".pi/juna/sessions/--w--",
            ".factory/sessions/-w",
            "custom/projects/-w",
        ] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        for file in [
            ".claude/projects/-w/a.jsonl",
            ".codex/sessions/2026/10/02/rollout-x.jsonl",
            ".pi/agent/sessions/--w--/p.jsonl",
            ".pi/juna/sessions/--w--/q.jsonl",
            ".factory/sessions/-w/d.jsonl",
            ".factory/sessions/-w/d.settings.json",
            "custom/projects/-w/c.jsonl",
        ] {
            std::fs::write(home.join(file), "{}\n").unwrap();
        }
        let none = |_: &str| None;
        let stores = Stores::from_env(&none, &home);
        assert_eq!(stores.pi.len(), 2, "both Pi profiles");
        let mut found: Vec<(Provider, String)> = stores
            .transcript_files()
            .into_iter()
            .map(|(p, path, _)| (p, path.file_name().unwrap().to_string_lossy().into_owned()))
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec![
                (Provider::Codex, "rollout-x.jsonl".into()),
                (Provider::Claude, "a.jsonl".into()),
                (Provider::Pi, "p.jsonl".into()),
                (Provider::Pi, "q.jsonl".into()),
                (Provider::Droid, "d.jsonl".into()),
            ]
        );
        let custom = home.join("custom").to_string_lossy().into_owned();
        let env = move |name: &str| (name == "CLAUDE_CONFIG_DIR").then(|| custom.clone());
        let stores = Stores::from_env(&env, &home);
        assert_eq!(stores.claude.unwrap(), home.join("custom/projects"));
        std::fs::remove_dir_all(home).unwrap();
    }
}
