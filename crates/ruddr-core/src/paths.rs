//! Well-known locations and environment overrides. Settings named for the
//! earlier `rudder` and `codex-rudder` releases keep working.

use std::path::{Path, PathBuf};

/// The first non-empty variable among `names`.
pub fn env_any(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
}

pub fn home_dir() -> PathBuf {
    env_any(&["HOME", "USERPROFILE"])
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Joins a relative path onto the current directory without touching the
/// filesystem (no symlink resolution), like Go's `filepath.Abs`.
pub fn absolute(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    normalize(&joined)
}

/// Removes `.` and `..` components lexically.
pub fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `$XDG_CONFIG_HOME/ruddr` or `~/.config/ruddr`: models.json, tui.json, web-token.
pub fn config_dir() -> PathBuf {
    env_any(&["XDG_CONFIG_HOME"])
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
        .join("ruddr")
}

/// `$XDG_STATE_HOME` or `~/.local/state`.
pub fn state_home() -> PathBuf {
    env_any(&["XDG_STATE_HOME"])
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local").join("state"))
}

/// `$XDG_CACHE_HOME/ruddr` or `~/.cache/ruddr`. Update checks and remote
/// shell probes live beside the registry under `state_home()/ruddr` instead.
pub fn cache_dir() -> PathBuf {
    env_any(&["XDG_CACHE_HOME"])
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cache"))
        .join("ruddr")
}

/// The registry directory new runs register in.
pub fn registry_dir() -> PathBuf {
    match env_any(&["RUDDR_REGISTRY_DIR", "RUDDER_REGISTRY_DIR", "CODEX_RUDDER_REGISTRY_DIR"]) {
        Some(dir) => absolute(Path::new(&dir)),
        None => state_home().join("ruddr").join("runs"),
    }
}

/// Every registry directory discovery reads, including pre-rename ones.
pub fn registry_dirs_for_discovery() -> Vec<PathBuf> {
    if let Some(dir) = env_any(&["RUDDR_REGISTRY_DIR", "RUDDER_REGISTRY_DIR", "CODEX_RUDDER_REGISTRY_DIR"]) {
        return vec![absolute(Path::new(&dir))];
    }
    let home = state_home();
    ["ruddr", "rudder", "codex-rudder"]
        .iter()
        .map(|name| home.join(name).join("runs"))
        .collect()
}

/// `CWD/.scratch/ruddr`: where runs started without `--state-dir` live.
pub fn default_runs_dir(cwd: &Path) -> PathBuf {
    cwd.join(".scratch").join("ruddr")
}

/// `CWD/.scratch/ruddr-tui`: where the TUI and web dashboard start runs.
pub fn launch_runs_dir(cwd: &Path) -> PathBuf {
    cwd.join(".scratch").join("ruddr-tui")
}

/// Creates `dir` as a private directory that ignores itself in Git, so run
/// files never reach the workspace's `git status`. An existing .gitignore is
/// left alone.
pub fn ensure_ignored_runs_dir(dir: &Path) -> std::io::Result<()> {
    crate::fsutil::create_private_dir(dir)?;
    match crate::fsutil::create_private_file_new(&dir.join(".gitignore")) {
        Ok(mut file) => std::io::Write::write_all(&mut file, b"*\n"),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// A fresh, not-yet-created run directory name: `<base>/<YYYYMMDD-HHMMSS>-<hex>`.
pub fn new_run_dir_name(base: &Path) -> PathBuf {
    let stamp = crate::time::now_rfc3339();
    let compact: String = stamp.chars().filter(|c| c.is_ascii_digit()).take(14).collect();
    let (date, time) = compact.split_at(8.min(compact.len()));
    base.join(format!("{date}-{time}-{}", crate::fsutil::random_hex(3)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_lexically() {
        assert_eq!(normalize(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
    }

    #[test]
    fn runs_dir_ignores_itself_once() {
        let base = std::env::temp_dir().join(format!("ruddr-paths-{}", crate::fsutil::random_hex(4)));
        let dir = launch_runs_dir(&base);
        ensure_ignored_runs_dir(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join(".gitignore")).unwrap(), "*\n");
        std::fs::write(dir.join(".gitignore"), "custom\n").unwrap();
        ensure_ignored_runs_dir(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join(".gitignore")).unwrap(), "custom\n");
        let name = new_run_dir_name(&dir);
        let leaf = name.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(leaf.len(), "20261002-091944-a1b2c3".len(), "{leaf}");
        std::fs::remove_dir_all(base).unwrap();
    }
}
