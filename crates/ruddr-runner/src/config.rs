//! The run configuration, its validation, and the provider defaults that pick
//! the model and the app-server child command.

use ruddr_core::provider::{self, Provider};
use ruddr_core::{Error, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Everything one controller needs. Empty strings mean "not set", as in the
/// command line.
#[derive(Clone)]
pub struct RunConfig {
    pub provider: String,
    pub cwd: PathBuf,
    pub prompt_file: PathBuf,
    pub state_dir: PathBuf,
    pub model: String,
    pub effort: String,
    pub sandbox: String,
    pub approval_policy: String,
    pub claude_path: String,
    pub opencode_path: String,
    pub pi_path: String,
    pub droid_path: String,
    /// The OpenCode, Pi, or Droid executable sent to the adapter.
    pub provider_path: String,
    pub ephemeral: bool,
    pub resume_thread_id: String,
    pub fork_thread_id: String,
    pub fork_before_turn_id: String,
    pub fork_through_turn_id: String,
    /// The per-turn watchdog; zero disables it.
    pub turn_timeout: Duration,
    pub idle: bool,
    /// Exit after this long idle; zero disables it.
    pub idle_timeout: Duration,
    /// The app-server command and arguments.
    pub child_command: Vec<String>,
    /// `--config KEY=VALUE` overrides for the default codex app-server.
    pub codex_config: Vec<String>,
    /// `--image FILE` attachments for the first turn, absolute after validation.
    pub images: Vec<PathBuf>,
    /// Record the run in the global registry the TUI and web dashboard read.
    pub register_run: bool,
    /// Test seams that keep lifecycle tests fast and deterministic.
    pub idle_turn_start_timeout: Option<Duration>,
    pub interrupt_timeout: Option<Duration>,
    pub before_state_reserve: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            provider: "codex".into(),
            cwd: std::env::current_dir().unwrap_or_default(),
            prompt_file: PathBuf::new(),
            state_dir: PathBuf::new(),
            model: String::new(),
            effort: String::new(),
            sandbox: "workspace-write".into(),
            approval_policy: "never".into(),
            claude_path: String::new(),
            opencode_path: String::new(),
            pi_path: String::new(),
            droid_path: String::new(),
            provider_path: String::new(),
            ephemeral: false,
            resume_thread_id: String::new(),
            fork_thread_id: String::new(),
            fork_before_turn_id: String::new(),
            fork_through_turn_id: String::new(),
            turn_timeout: Duration::from_secs(3600),
            idle: false,
            idle_timeout: Duration::from_secs(4 * 3600),
            child_command: Vec::new(),
            codex_config: Vec::new(),
            images: Vec::new(),
            register_run: false,
            idle_turn_start_timeout: None,
            interrupt_timeout: None,
            before_state_reserve: None,
        }
    }
}

impl std::fmt::Debug for RunConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The child command can carry broker arguments; keep it out of debug output.
        f.debug_struct("RunConfig")
            .field("provider", &self.provider)
            .field("cwd", &self.cwd)
            .field("state_dir", &self.state_dir)
            .field("model", &self.model)
            .field("idle", &self.idle)
            .finish_non_exhaustive()
    }
}

/// Checks a configuration before any file is created, and normalizes the
/// provider name and working directory.
pub fn validate_run_config(cfg: &mut RunConfig) -> Result<()> {
    let provider = Provider::parse(&cfg.provider)?;
    cfg.provider = provider.as_str().into();
    if cfg.child_command.is_empty() {
        return Err(Error::failed("provider command is empty"));
    }
    cfg.cwd = ruddr_core::paths::absolute(&cfg.cwd);
    for image in &mut cfg.images {
        *image = ruddr_core::images::checked_image(image).map_err(Error::failed)?;
    }
    if provider == Provider::Codex && cfg.model.is_empty() {
        return Err(Error::failed("model is required"));
    }
    let forking = !cfg.fork_thread_id.is_empty();
    let selecting = !cfg.fork_before_turn_id.is_empty() || !cfg.fork_through_turn_id.is_empty();
    if provider.is_adapter() {
        if cfg.approval_policy != "never" {
            return Err(Error::failed(format!(
                "{provider} runs require --approval-policy never because Ruddr has no interactive approval surface"
            )));
        }
        if provider == Provider::Droid {
            if selecting {
                return Err(Error::failed(
                    "droid forks copy the whole session; drop --fork-before-turn and --fork-through-turn",
                ));
            }
            if cfg.ephemeral {
                return Err(Error::failed("droid sessions always persist; drop --ephemeral"));
            }
        } else if forking || selecting {
            return Err(Error::failed(format!(
                "{provider} runs do not yet support --fork-thread or fork turn selectors; use --resume-thread"
            )));
        }
    }
    if cfg.idle && cfg.ephemeral && provider == Provider::Claude {
        return Err(Error::failed("--idle requires a persisted Claude session; drop --ephemeral"));
    }
    if !cfg.resume_thread_id.is_empty() && forking {
        return Err(Error::failed("--resume-thread and --fork-thread are mutually exclusive"));
    }
    if !cfg.fork_before_turn_id.is_empty() && !cfg.fork_through_turn_id.is_empty() {
        return Err(Error::failed("--fork-before-turn and --fork-through-turn are mutually exclusive"));
    }
    if selecting && !forking {
        return Err(Error::failed("fork turn selectors require --fork-thread"));
    }
    match cfg.sandbox.as_str() {
        "read-only" | "workspace-write" | "danger-full-access" => Ok(()),
        other => Err(Error::failed(format!("unsupported sandbox {other:?}"))),
    }
}

/// Fills in the default model and the app-server child command.
///
/// Codex runs `codex app-server --listen stdio://` plus `-c KEY=VALUE` from
/// the model catalog and `--config`, or the custom command after `--`. The
/// other providers run this binary as `ruddr app-server --provider NAME
/// [--executable PATH]`.
pub fn configure_provider_defaults(cfg: &mut RunConfig, child_args: &[String]) -> Result<()> {
    let provider = Provider::parse(&cfg.provider)?;
    cfg.provider = provider.as_str().into();
    if provider != Provider::Codex && !cfg.codex_config.is_empty() {
        return Err(Error::usage("--config applies only to --provider codex"));
    }
    if cfg.model.is_empty() {
        cfg.model = ruddr_core::models::default_model(provider)?.unwrap_or_default();
    }
    if provider == Provider::Codex {
        if !child_args.is_empty() {
            if !cfg.codex_config.is_empty() {
                return Err(Error::usage(
                    "--config applies to the default codex app-server command; add -c KEY=VALUE to the command after -- instead",
                ));
            }
            cfg.child_command = child_args.to_vec();
            return Ok(());
        }
        let mut command: Vec<String> = ["codex", "app-server", "--listen", "stdio://"].map(String::from).to_vec();
        let overrides = ruddr_core::models::codex_config(&cfg.model)?;
        for value in overrides.iter().chain(&cfg.codex_config) {
            command.push("-c".into());
            command.push(value.clone());
        }
        cfg.child_command = command;
        return Ok(());
    }
    if !child_args.is_empty() {
        let flag = provider.path_flag().unwrap_or_default();
        return Err(Error::failed(if provider == Provider::Claude {
            format!("a command after -- is supported only for Codex; use --{flag} for Claude Code")
        } else {
            format!("a command after -- is supported only for Codex; use --{flag} for {provider}")
        }));
    }
    let flag_value = match provider {
        Provider::Claude => cfg.claude_path.clone(),
        Provider::OpenCode => cfg.opencode_path.clone(),
        Provider::Pi => cfg.pi_path.clone(),
        Provider::Droid => cfg.droid_path.clone(),
        Provider::Codex => String::new(),
    };
    let executable = provider::resolve_executable(provider, &flag_value)?;
    if provider == Provider::Claude {
        cfg.claude_path = executable.clone().unwrap_or_default();
    } else {
        cfg.provider_path = executable.clone().unwrap_or_default();
    }
    let ruddr = std::env::current_exe().map_err(|e| Error::failed(format!("locate the Ruddr executable: {e}")))?;
    cfg.child_command = adapter_command(&ruddr, provider, executable.as_deref());
    Ok(())
}

/// The adapter child: `ruddr app-server --provider NAME [--executable PATH]`.
pub fn adapter_command(ruddr: &Path, provider: Provider, executable: Option<&str>) -> Vec<String> {
    let mut command = vec![
        ruddr.to_string_lossy().into_owned(),
        "app-server".into(),
        "--provider".into(),
        provider.as_str().into(),
    ];
    if let Some(path) = executable.filter(|p| !p.is_empty()) {
        command.push("--executable".into());
        command.push(path.into());
    }
    command
}

/// A fresh state directory for a run without `--state-dir`:
/// `<cwd>/.scratch/ruddr/<YYYYMMDD-HHMMSS>-<hex>`. Its parent ignores itself
/// in Git, so run files never reach the workspace's `git status`.
pub fn default_state_dir(cwd: &Path) -> Result<PathBuf> {
    let base = ruddr_core::paths::default_runs_dir(&ruddr_core::paths::absolute(cwd));
    ruddr_core::paths::ensure_ignored_runs_dir(&base).map_err(|e| Error::failed(format!("choose a state directory: {e}")))?;
    Ok(ruddr_core::paths::new_run_dir_name(&base))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> RunConfig {
        RunConfig {
            model: "test-model".into(),
            sandbox: "read-only".into(),
            child_command: vec!["codex".into()],
            ..Default::default()
        }
    }

    #[test]
    fn rejects_fork_selector_misuse() {
        let mut both = RunConfig {
            fork_thread_id: "s".into(),
            fork_before_turn_id: "a".into(),
            fork_through_turn_id: "b".into(),
            ..base()
        };
        assert!(validate_run_config(&mut both).unwrap_err().message.contains("mutually exclusive"));
        let mut orphan = RunConfig {
            fork_before_turn_id: "a".into(),
            ..base()
        };
        assert!(
            validate_run_config(&mut orphan)
                .unwrap_err()
                .message
                .contains("require --fork-thread")
        );
        let mut resume_selector = RunConfig {
            resume_thread_id: "s".into(),
            fork_through_turn_id: "b".into(),
            ..base()
        };
        assert!(validate_run_config(&mut resume_selector).is_err());
        let mut conflict = RunConfig {
            resume_thread_id: "r".into(),
            fork_thread_id: "f".into(),
            ..base()
        };
        assert!(
            validate_run_config(&mut conflict)
                .unwrap_err()
                .message
                .contains("mutually exclusive")
        );
        let mut sandbox = RunConfig {
            sandbox: "yolo".into(),
            ..base()
        };
        assert!(
            validate_run_config(&mut sandbox)
                .unwrap_err()
                .message
                .contains("unsupported sandbox")
        );
        let mut ok = base();
        validate_run_config(&mut ok).unwrap();
        assert!(ok.cwd.is_absolute());
    }

    #[test]
    fn droid_forks_whole_sessions_only() {
        let droid = RunConfig {
            provider: "droid".into(),
            model: "glm-5.3-flash".into(),
            sandbox: "workspace-write".into(),
            ..base()
        };
        let mut fork = RunConfig {
            fork_thread_id: "source".into(),
            ..droid.clone()
        };
        validate_run_config(&mut fork).unwrap();
        let mut selector = RunConfig {
            fork_through_turn_id: "turn-a".into(),
            ..fork.clone()
        };
        assert!(validate_run_config(&mut selector).unwrap_err().message.contains("whole session"));
        let mut ephemeral = RunConfig {
            ephemeral: true,
            ..droid.clone()
        };
        assert!(validate_run_config(&mut ephemeral).unwrap_err().message.contains("--ephemeral"));
        let mut pi = RunConfig {
            provider: "pi".into(),
            ..fork
        };
        assert!(
            validate_run_config(&mut pi)
                .unwrap_err()
                .message
                .contains("do not yet support --fork-thread")
        );
        let mut approval = RunConfig {
            approval_policy: "on-request".into(),
            ..droid.clone()
        };
        assert!(
            validate_run_config(&mut approval)
                .unwrap_err()
                .message
                .contains("--approval-policy never")
        );
        let mut claude = RunConfig {
            provider: "claude".into(),
            idle: true,
            ephemeral: true,
            ..base()
        };
        assert!(
            validate_run_config(&mut claude)
                .unwrap_err()
                .message
                .contains("persisted Claude session")
        );
    }

    #[test]
    fn codex_defaults_and_config_overrides() {
        ruddr_core_models_env(
            r#"{"models":[{"provider":"codex","id":"gpt-6-sol","config":{"features.b":"false","features.a":"1"}}]}"#,
            || {
                let mut cfg = RunConfig {
                    model: String::new(),
                    ..RunConfig::default()
                };
                configure_provider_defaults(&mut cfg, &[]).unwrap();
                assert_eq!(cfg.model, "gpt-6-astra");
                assert_eq!(cfg.child_command.join(" "), "codex app-server --listen stdio://");

                let mut sol = RunConfig {
                    model: "gpt-6-sol".into(),
                    codex_config: vec!["model_verbosity=low".into()],
                    ..RunConfig::default()
                };
                configure_provider_defaults(&mut sol, &[]).unwrap();
                assert_eq!(
                    sol.child_command,
                    [
                        "codex",
                        "app-server",
                        "--listen",
                        "stdio://",
                        "-c",
                        "features.a=1",
                        "-c",
                        "features.b=false",
                        "-c",
                        "model_verbosity=low"
                    ]
                );

                let mut custom = RunConfig {
                    codex_config: vec!["a=b".into()],
                    ..RunConfig::default()
                };
                let error = configure_provider_defaults(&mut custom, &["broker".into()]).unwrap_err();
                assert!(error.message.contains("command after --") && error.exit == ruddr_core::Exit::Usage);
                let mut broker = RunConfig::default();
                configure_provider_defaults(&mut broker, &["broker".into(), "app-server-bridge".into()]).unwrap();
                assert_eq!(broker.child_command, ["broker", "app-server-bridge"]);

                let mut claude_config = RunConfig {
                    provider: "claude".into(),
                    codex_config: vec!["a=b".into()],
                    ..RunConfig::default()
                };
                assert!(
                    configure_provider_defaults(&mut claude_config, &[])
                        .unwrap_err()
                        .message
                        .contains("only to --provider codex")
                );
            },
        );
    }

    #[test]
    fn adapters_run_this_binary() {
        ruddr_core_models_env(r#"{"models":[]}"#, || {
            let mut claude = RunConfig {
                provider: "claude".into(),
                claude_path: "/opt/explicit".into(),
                ..RunConfig::default()
            };
            configure_provider_defaults(&mut claude, &[]).unwrap();
            assert_eq!(claude.model, "claude-opus-5-5");
            assert_eq!(
                claude.child_command[1..],
                ["app-server", "--provider", "claude", "--executable", "/opt/explicit"]
            );
            assert_eq!(claude.child_command[0], std::env::current_exe().unwrap().to_string_lossy());

            let mut claude_child = RunConfig {
                provider: "claude".into(),
                ..RunConfig::default()
            };
            let error = configure_provider_defaults(&mut claude_child, &["claude".into(), "-p".into()]).unwrap_err();
            assert!(error.message.contains("--claude-path"));

            for (name, model) in [
                ("opencode", "openrouter/deepseek/deepseek-v4-flash-vision-exp"),
                ("pi", "openrouter/deepseek/deepseek-v4-flash-vision-exp"),
                ("droid", "glm-5.3-flash"),
            ] {
                let mut cfg = RunConfig {
                    provider: name.into(),
                    ..RunConfig::default()
                };
                match name {
                    "opencode" => cfg.opencode_path = "/opt/opencode2".into(),
                    "pi" => cfg.pi_path = "/opt/pi".into(),
                    _ => cfg.droid_path = "/opt/droid".into(),
                }
                configure_provider_defaults(&mut cfg, &[]).unwrap();
                assert_eq!(cfg.model, model);
                assert!(cfg.provider_path.starts_with("/opt/"));
                assert_eq!(cfg.child_command[1..5], ["app-server", "--provider", name, "--executable"]);
                assert_eq!(cfg.child_command[5], cfg.provider_path);
            }
            assert_eq!(
                configure_provider_defaults(
                    &mut RunConfig {
                        provider: "other".into(),
                        ..RunConfig::default()
                    },
                    &[]
                )
                .unwrap_err()
                .exit,
                ruddr_core::Exit::Usage
            );
        });
    }

    #[test]
    fn default_state_dirs_are_fresh_and_ignored() {
        let cwd = std::env::temp_dir().join(format!("ruddr-default-dir-{}", ruddr_core::fsutil::random_hex(4)));
        std::fs::create_dir_all(&cwd).unwrap();
        let first = default_state_dir(&cwd).unwrap();
        let second = default_state_dir(&cwd).unwrap();
        let base = cwd.join(".scratch").join("ruddr");
        assert_eq!(first.parent().unwrap(), base);
        assert_ne!(first, second);
        assert_eq!(std::fs::read_to_string(base.join(".gitignore")).unwrap(), "*\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&base).unwrap().permissions().mode() & 0o777, 0o700);
        }
        std::fs::remove_dir_all(cwd).unwrap();
    }

    /// Points RUDDR_MODELS_FILE at a private file for the duration of `test`.
    pub(crate) fn ruddr_core_models_env(body: &str, test: impl FnOnce()) {
        let _guard = crate::test_support::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ruddr-runner-models-{}", ruddr_core::fsutil::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("models.json");
        std::fs::write(&path, body).unwrap();
        // SAFETY: ENV_LOCK serializes the tests that touch the environment.
        unsafe { std::env::set_var(ruddr_core::models::MODELS_FILE_ENV, &path) };
        test();
        unsafe { std::env::remove_var(ruddr_core::models::MODELS_FILE_ENV) };
        let _ = std::fs::remove_dir_all(dir);
    }
}
