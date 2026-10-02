//! The Diff tab: the working tree against `HEAD`, read with a bounded,
//! timed `git diff` and cached with an adaptive delay, plus the untracked
//! files, the branch name, and the files edited since a session started.
//! Port of tui/git.ts and the git helpers in web/server.ts.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const DIFF_MAX_BYTES: usize = 2 * 1024 * 1024;
const STDERR_MAX_BYTES: usize = 64 * 1024;
const DIFF_TIMEOUT: Duration = Duration::from_secs(3);
const DIFF_REFRESH: Duration = Duration::from_millis(1000);
const DIFF_MAX_DELAY: Duration = Duration::from_millis(8000);
/// The TypeScript server left these unbounded; a hung git must not hold a
/// request open forever.
const AUX_TIMEOUT: Duration = Duration::from_secs(10);
const BRANCH_CACHE: Duration = Duration::from_secs(10);
const UNTRACKED_LIMIT: usize = 500;

#[derive(Debug, Clone, PartialEq)]
pub struct DiffResult {
    pub content: String,
    pub error: Option<String>,
}

struct CacheEntry {
    read_at: Instant,
    delay: Duration,
    result: DiffResult,
}

#[derive(Default)]
pub struct Git {
    cache: Mutex<HashMap<String, CacheEntry>>,
    /// One read per working directory at a time; a request that waited on
    /// another read takes its result.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    branches: Mutex<HashMap<String, (Option<String>, Instant)>>,
}

/// Doubles the delay while the diff stays the same, up to 8 s, and drops it
/// back to 1 s when the diff moves.
fn next_delay(previous: Duration, changed: bool) -> Duration {
    if changed {
        DIFF_REFRESH
    } else {
        (previous * 2).clamp(DIFF_REFRESH, DIFF_MAX_DELAY)
    }
}

impl Git {
    pub async fn workspace_diff(&self, cwd: &str, force: bool) -> DiffResult {
        let asked = Instant::now();
        let lock = self.locks.lock().unwrap().entry(cwd.to_string()).or_default().clone();
        let _guard = lock.lock().await;
        {
            let cache = self.cache.lock().unwrap();
            if let Some(entry) = cache.get(cwd) {
                // Another request finished a read while this one waited.
                if entry.read_at >= asked || (!force && entry.read_at.elapsed() < entry.delay) {
                    return entry.result.clone();
                }
            }
        }
        let result = read_diff(cwd).await;
        let mut cache = self.cache.lock().unwrap();
        let previous = cache.get(cwd);
        let changed = previous.is_none_or(|entry| entry.result != result);
        let delay = next_delay(previous.map_or(DIFF_REFRESH, |entry| entry.delay), changed);
        cache.insert(
            cwd.to_string(),
            CacheEntry {
                read_at: Instant::now(),
                delay,
                result: result.clone(),
            },
        );
        result
    }

    pub async fn branch(&self, cwd: &str) -> Option<String> {
        if let Some((name, read_at)) = self.branches.lock().unwrap().get(cwd)
            && read_at.elapsed() < BRANCH_CACHE
        {
            return name.clone();
        }
        let name = match output(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await {
            Some((0, stdout)) => Some(String::from_utf8_lossy(&stdout).trim().to_string()).filter(|name| !name.is_empty()),
            _ => None,
        };
        self.branches
            .lock()
            .unwrap()
            .insert(cwd.to_string(), (name.clone(), Instant::now()));
        name
    }
}

/// Runs `git -C cwd ARGS` and returns its exit code and stdout.
async fn output(cwd: &str, args: &[&str]) -> Option<(i32, Vec<u8>)> {
    let child = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(AUX_TIMEOUT, child).await.ok()?.ok()?;
    Some((out.status.code().unwrap_or(-1), out.stdout))
}

/// Whether `cwd` is inside a Git work tree that `git diff` can describe.
pub async fn is_work_tree(cwd: &str) -> bool {
    matches!(output(cwd, &["rev-parse", "--is-inside-work-tree"]).await, Some((0, out)) if out.trim_ascii() == b"true")
}

pub async fn untracked_files(cwd: &str) -> Vec<String> {
    match output(cwd, &["ls-files", "--others", "--exclude-standard", "-z"]).await {
        Some((0, stdout)) => String::from_utf8_lossy(&stdout)
            .split('\0')
            .filter(|path| !path.is_empty())
            .take(UNTRACKED_LIMIT)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

async fn read_diff(cwd: &str) -> DiffResult {
    let head = match run_diff(cwd, &["HEAD", "--"]).await {
        Ok(head) => head,
        Err(error) => {
            return DiffResult {
                content: String::new(),
                error: Some(error),
            };
        }
    };
    if head.1 == 0 {
        return DiffResult {
            content: head.0,
            error: None,
        };
    }
    // No HEAD yet: show the index and the working tree separately.
    let (staged, unstaged) = tokio::join!(run_diff(cwd, &["--cached", "--"]), run_diff(cwd, &["--"]));
    match (staged, unstaged) {
        (Ok((staged, 0)), Ok((unstaged, 0))) => {
            let parts: Vec<String> = [staged, unstaged].into_iter().filter(|part| !part.is_empty()).collect();
            DiffResult {
                content: parts.join("\n"),
                error: None,
            }
        }
        (Err(error), _) | (_, Err(error)) => DiffResult {
            content: String::new(),
            error: Some(error),
        },
        _ => DiffResult {
            content: String::new(),
            error: Some(if head.0.is_empty() {
                "Git diff is unavailable.".into()
            } else {
                head.0
            }),
        },
    }
}

/// One `git diff` with stdout capped at 2 MiB and a 3 s deadline. Returns
/// stdout on success (or truncation) and trimmed stderr otherwise, with the
/// exit code; truncated output counts as success.
async fn run_diff(cwd: &str, args: &[&str]) -> Result<(String, i32), String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["diff", "--no-ext-diff", "--no-textconv", "--no-color", "--unified=3"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let work = async {
        let read_stdout = async {
            let mut kept = Vec::new();
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let n = stdout.read(&mut buffer).await.unwrap_or(0);
                if n == 0 {
                    return (kept, false);
                }
                let remaining = DIFF_MAX_BYTES - kept.len();
                if n > remaining {
                    kept.extend_from_slice(&buffer[..remaining]);
                    return (kept, true);
                }
                kept.extend_from_slice(&buffer[..n]);
            }
        };
        let read_stderr = async {
            // Drain everything so git never blocks, but keep only the start.
            let mut kept = Vec::new();
            let mut buffer = vec![0u8; 16 * 1024];
            loop {
                let n = stderr.read(&mut buffer).await.unwrap_or(0);
                if n == 0 {
                    return kept;
                }
                let room = STDERR_MAX_BYTES.saturating_sub(kept.len());
                kept.extend_from_slice(&buffer[..n.min(room)]);
            }
        };
        let ((stdout, truncated), stderr) = tokio::join!(
            async {
                let result = read_stdout.await;
                if result.1 {
                    let _ = child.start_kill();
                }
                result
            },
            read_stderr
        );
        let status = child.wait().await.ok().and_then(|status| status.code()).unwrap_or(-1);
        (stdout, truncated, stderr, status)
    };
    let Ok((stdout, truncated, stderr, status)) = tokio::time::timeout(DIFF_TIMEOUT, work).await else {
        return Err("Git diff timed out after 3 seconds.".into());
    };
    if truncated {
        let mut text = String::from_utf8_lossy(&stdout).into_owned();
        text.push_str(&format!("\n\\ Diff truncated at {} MiB", DIFF_MAX_BYTES / 1024 / 1024));
        return Ok((text, 0));
    }
    if status == 0 {
        Ok((String::from_utf8_lossy(&stdout).into_owned(), 0))
    } else {
        Ok((String::from_utf8_lossy(&stderr).trim().to_string(), status))
    }
}

/// The `b/` paths of `diff --git a/X b/Y` headers.
pub fn diff_paths(content: &str) -> Vec<String> {
    content
        .split('\n')
        .filter_map(|line| {
            let rest = line.strip_prefix("diff --git a/")?;
            if line.contains(['\r', '\u{2028}', '\u{2029}']) {
                return None;
            }
            // `.+?` takes at least one character before ` b/`.
            let split = rest.char_indices().skip(1).find(|(index, _)| rest[*index..].starts_with(" b/"))?.0;
            let path = &rest[split + 3..];
            (!path.is_empty()).then(|| path.to_string())
        })
        .collect()
}

/// Files whose modification time is at or after the session start (less a
/// second of clock slack): the session's edits.
pub fn touched_since(cwd: &Path, paths: &BTreeSet<String>, started_at: &str) -> Vec<String> {
    let Some(since) = ruddr_core::time::parse_rfc3339_ms(started_at) else {
        return Vec::new();
    };
    paths
        .iter()
        .filter(|path| {
            std::fs::metadata(cwd.join(path))
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .is_some_and(|modified| modified.as_millis() as i64 >= since - 1000)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_diff_header_paths() {
        let diff = "diff --git a/x.ts b/x.ts\n--- a/x.ts\n+++ b/x.ts\ndiff --git a/a b/c b/a b/c\nnot a header";
        // Like the lazy regex, the first ` b/` ends the old path.
        assert_eq!(diff_paths(diff), vec!["x.ts", "c b/a b/c"]);
    }

    #[test]
    fn backs_off_while_the_diff_is_unchanged() {
        assert_eq!(next_delay(Duration::from_secs(1), false), Duration::from_secs(2));
        assert_eq!(next_delay(Duration::from_secs(8), false), Duration::from_secs(8));
        assert_eq!(next_delay(Duration::from_secs(4), true), Duration::from_secs(1));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reports_a_missing_repository_as_an_error() {
        let dir = std::env::temp_dir().join(format!("ruddr-web-git-{}", ruddr_core::fsutil::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let git = Git::default();
        let result = git.workspace_diff(dir.to_str().unwrap(), true).await;
        assert!(result.content.is_empty());
        assert!(result.error.is_some(), "{result:?}");
        assert_eq!(git.branch(dir.to_str().unwrap()).await, None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
