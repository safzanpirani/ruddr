//! Provider selection. Codex speaks the app-server protocol natively; Claude
//! Code, OpenCode, Pi, omp, Factory Droid, Hermes, and OpenClaw run behind `ruddr app-server
//! --provider NAME`, which the runner starts as its own child. This module
//! names the providers, validates user input, and finds the provider
//! executables the adapters drive.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Codex,
    Claude,
    OpenCode,
    Pi,
    Omp,
    Droid,
    Hermes,
    OpenClaw,
}

impl Provider {
    /// Every provider, in the order usage text lists them.
    pub const ALL: [Provider; 8] = [
        Provider::Codex,
        Provider::Claude,
        Provider::OpenCode,
        Provider::Pi,
        Provider::Omp,
        Provider::Droid,
        Provider::Hermes,
        Provider::OpenClaw,
    ];

    /// Parses a provider name. An empty name means Codex, the default.
    pub fn parse(name: &str) -> Result<Provider> {
        match name {
            "" | "codex" => Ok(Provider::Codex),
            "claude" => Ok(Provider::Claude),
            "opencode" => Ok(Provider::OpenCode),
            "pi" => Ok(Provider::Pi),
            "omp" => Ok(Provider::Omp),
            "droid" => Ok(Provider::Droid),
            "hermes" => Ok(Provider::Hermes),
            "openclaw" => Ok(Provider::OpenClaw),
            other => Err(Error::usage(format!(
                "unsupported provider {other:?}; expected codex, claude, opencode, pi, omp, droid, hermes, or openclaw"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Codex => "codex",
            Provider::Claude => "claude",
            Provider::OpenCode => "opencode",
            Provider::Pi => "pi",
            Provider::Omp => "omp",
            Provider::Droid => "droid",
            Provider::Hermes => "hermes",
            Provider::OpenClaw => "openclaw",
        }
    }

    /// Whether the provider runs behind `ruddr app-server --provider NAME`.
    pub fn is_adapter(self) -> bool {
        self != Provider::Codex
    }

    /// Whether `run --fork-thread` works. Droid forks copy the whole session,
    /// so only Codex accepts the fork boundary selectors.
    pub fn supports_fork(self) -> bool {
        matches!(self, Provider::Codex | Provider::Droid)
    }

    /// The `run` flag that names the provider executable.
    pub fn path_flag(self) -> Option<&'static str> {
        match self {
            Provider::Codex => None,
            Provider::Claude => Some("claude-path"),
            Provider::OpenCode => Some("opencode-path"),
            Provider::Pi => Some("pi-path"),
            Provider::Omp => Some("omp-path"),
            Provider::Droid => Some("droid-path"),
            Provider::Hermes => Some("hermes-path"),
            Provider::OpenClaw => Some("openclaw-path"),
        }
    }

    /// The environment variable that names the provider executable when the
    /// flag is absent.
    pub fn path_env(self) -> Option<&'static str> {
        match self {
            Provider::Codex => None,
            Provider::Claude => Some("RUDDR_CLAUDE_PATH"),
            Provider::OpenCode => Some("RUDDR_OPENCODE_PATH"),
            Provider::Pi => Some("RUDDR_PI_PATH"),
            Provider::Omp => Some("RUDDR_OMP_PATH"),
            Provider::Droid => Some("RUDDR_DROID_PATH"),
            Provider::Hermes => Some("RUDDR_HERMES_PATH"),
            Provider::OpenClaw => Some("RUDDR_OPENCLAW_PATH"),
        }
    }

    /// Executable names searched on `PATH`, in order. Claude has none: the
    /// adapter finds `claude` itself when no path is given.
    pub fn executable_names(self) -> &'static [&'static str] {
        match self {
            Provider::Codex => &["codex"],
            Provider::Claude => &[],
            Provider::OpenCode => &["opencode2", "opencode-next"],
            Provider::Pi => &["pi"],
            Provider::Omp => &["omp"],
            Provider::Droid => &["droid"],
            Provider::Hermes => &["hermes"],
            Provider::OpenClaw => &["openclaw"],
        }
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Provider {
    type Err = Error;
    fn from_str(name: &str) -> Result<Provider> {
        Provider::parse(name)
    }
}

/// Maps a `RUDDR_*` variable to the `RUDDER_*` spelling earlier releases
/// read, so existing shell configuration keeps working.
pub fn previous_env_name(name: &str) -> String {
    format!("RUDDER_{}", name.strip_prefix("RUDDR_").unwrap_or(name))
}

/// The provider executable an adapter run should drive.
///
/// The explicit flag wins, then the environment. Claude may resolve to
/// `None`, which lets the adapter find `claude` itself. The other adapters
/// also search `PATH` and fail when nothing is found.
pub fn resolve_executable(provider: Provider, flag: &str) -> Result<Option<String>> {
    if !flag.is_empty() {
        return Ok(Some(flag.to_string()));
    }
    let Some(env) = provider.path_env() else { return Ok(None) };
    if provider == Provider::Claude {
        // Go releases read only RUDDR_CLAUDE_PATH for Claude.
        return Ok(crate::paths::env_any(&[env]));
    }
    if let Some(path) = crate::paths::env_any(&[env, &previous_env_name(env)]) {
        return Ok(Some(path));
    }
    for name in provider.executable_names() {
        if let Some(path) = look_path(name) {
            return Ok(Some(path.to_string_lossy().into_owned()));
        }
    }
    Err(Error::failed(format!(
        "{provider} support requires {} on PATH or {env}",
        provider.executable_names().join(" or ")
    )))
}

/// Finds an executable on `PATH`, like Go's `exec.LookPath`. A name that
/// contains a path separator is checked as given.
pub fn look_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH");
    let extensions = cfg!(windows).then(|| std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into()));
    look_path_in(name, path.as_deref(), extensions.as_deref())
}

// Explicit inputs keep Windows lookup testable without mutating the process environment.
fn look_path_in(name: &str, path: Option<&std::ffi::OsStr>, extensions: Option<&str>) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    if name.contains('/') || (extensions.is_some() && name.contains('\\')) {
        return executable_candidate(Path::new(name), extensions);
    }
    for dir in std::env::split_paths(path?) {
        // An empty PATH entry means the current directory; do not search it.
        if !dir.as_os_str().is_empty()
            && let Some(found) = executable_candidate(&dir.join(name), extensions)
        {
            return Some(found);
        }
    }
    None
}

fn executable_candidate(path: &Path, extensions: Option<&str>) -> Option<PathBuf> {
    if let Some(extensions) = extensions {
        if path.extension().is_some() && path.is_file() {
            return Some(path.to_path_buf());
        }
        for extension in extensions.split(';').filter(|e| !e.is_empty()) {
            let mut candidate = path.as_os_str().to_owned();
            candidate.push(extension);
            let candidate = PathBuf::from(candidate);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        return None;
    }
    let metadata = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    metadata.is_file().then(|| path.to_path_buf())
}

/// Builds a command with PATHEXT lookup on Windows. Unix retains native
/// Command lookup, including paths relative to the child's working directory.
/// Pass arguments with `arg`/`args`: Rust escapes resolved .cmd/.bat programs
/// using its batch-file rules and rejects arguments it cannot safely encode.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    let program = program.as_ref();
    #[cfg(windows)]
    if let Some(resolved) = program.to_str().and_then(look_path) {
        return std::process::Command::new(resolved);
    }
    std::process::Command::new(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_lookup_respects_path_pathext_and_explicit_paths() {
        let root = std::env::temp_dir().join(format!("ruddr-lookup-{}", crate::fsutil::random_hex(8)));
        let first = root.join("first space & dir");
        let second = root.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let path = std::env::join_paths([Path::new(""), &first, &second]).unwrap();
        for name in [
            "npm",
            "bun",
            "codex",
            "claude",
            "opencode2",
            "opencode-next",
            "pi",
            "omp",
            "droid",
            "hermes",
            "openclaw",
        ] {
            std::fs::write(first.join(format!("{name}.CMD")), "shim").unwrap();
            std::fs::write(second.join(format!("{name}.EXE")), "exe").unwrap();
            assert_eq!(
                look_path_in(name, Some(&path), Some(".EXE;;.CMD")),
                Some(first.join(format!("{name}.CMD")))
            );
        }
        std::fs::write(first.join("bun.EXE"), "exe").unwrap();
        assert_eq!(look_path_in("bun", Some(&path), Some(".EXE;.CMD")), Some(first.join("bun.EXE")));
        assert_eq!(look_path_in("bun", Some(&path), Some(".CMD;.EXE")), Some(first.join("bun.CMD")));
        assert_eq!(look_path_in("npm.CMD", Some(&path), Some(".EXE;.CMD")), Some(first.join("npm.CMD")));
        assert_eq!(
            look_path_in(first.join("npm").to_str().unwrap(), None, Some(".CMD")),
            Some(first.join("npm.CMD"))
        );
        std::fs::create_dir(first.join("directory.CMD")).unwrap();
        assert!(look_path_in("directory", Some(&path), Some(".CMD")).is_none());
        assert!(look_path_in("npm", Some(&path), Some(".BAT")).is_none());
        assert!(look_path_in("", Some(&path), Some(".CMD")).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn resolved_batch_command_preserves_argument_boundaries() {
        let root = std::env::temp_dir().join(format!("ruddr batch {}", crate::fsutil::random_hex(8)));
        std::fs::create_dir(&root).unwrap();
        let shim = root.join("tool.cmd");
        std::fs::write(&shim, "@echo off\r\necho \"%~1\"\r\necho \"%~2\"\r\n").unwrap();
        let output = command(root.join("tool")).args(["space & value", "tail"]).output().unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().replace("\r\n", "\n"),
            "\"space & value\"\n\"tail\"\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_and_names_providers() {
        assert_eq!(Provider::parse("").unwrap(), Provider::Codex);
        for provider in Provider::ALL {
            assert_eq!(Provider::parse(provider.as_str()).unwrap(), provider);
        }
        let error = Provider::parse("other").unwrap_err();
        assert_eq!(error.exit, crate::Exit::Usage);
        assert!(
            error
                .message
                .contains("expected codex, claude, opencode, pi, omp, droid, hermes, or openclaw")
        );
        assert!(Provider::Droid.supports_fork() && !Provider::Pi.supports_fork() && !Provider::Omp.supports_fork());
    }

    #[test]
    fn previous_names_keep_working() {
        assert_eq!(previous_env_name("RUDDR_CLAUDE_PATH"), "RUDDER_CLAUDE_PATH");
    }

    #[test]
    fn explicit_paths_win_and_claude_may_stay_unset() {
        assert_eq!(resolve_executable(Provider::Pi, "/opt/pi").unwrap().as_deref(), Some("/opt/pi"));
        assert_eq!(resolve_executable(Provider::Codex, "").unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn look_path_finds_executables_only() {
        assert!(look_path("sh").is_some());
        assert!(look_path("ruddr-no-such-binary-anywhere").is_none());
        assert_eq!(look_path("/bin/sh"), Some(PathBuf::from("/bin/sh")));
    }
}
