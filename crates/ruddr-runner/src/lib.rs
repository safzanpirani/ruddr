//! The run controller: `ruddr run`, the app-server child, turns, steering,
//! idle sessions, the watchdog, logs, the control server, detaching, and
//! process-tree termination. Port of runner.go, control.go, detach.go,
//! output.go, and process_*.go.
//!
//! [`run_command`] is the CLI entry point. [`run_controller`] runs one
//! controller in the foreground from a [`RunConfig`]; tests and embedders use
//! it with their own [`CancelToken`].

pub mod args;
pub mod config;
pub mod control_server;
pub mod controller;
#[cfg(test)]
mod controller_tests;
pub mod detach;
pub mod output;
pub mod process;
pub mod run;
pub mod signals;
pub mod store;
pub mod text;

pub use config::RunConfig;
pub use run::run_controller;
pub use signals::CancelToken;

use ruddr_core::{Error, Result};
use std::ffi::OsString;

/// Entry point for `ruddr run ARGS...`.
pub fn run_command(args: Vec<String>) -> Result<()> {
    let parsed = args::parse(&args)?;
    if parsed.help {
        print!("{}", args::usage());
        return Ok(());
    }
    let args::Parsed {
        mut cfg,
        detach,
        mut tokens,
        child_args,
        ..
    } = parsed;
    cfg.register_run = true;
    if cfg.prompt_file.as_os_str().is_empty() {
        return Err(Error::usage("--prompt-file is required"));
    }
    config::configure_provider_defaults(&mut cfg, child_args.as_deref().unwrap_or_default())?;
    if cfg.state_dir.as_os_str().is_empty() {
        let dir = config::default_state_dir(&cfg.cwd)?;
        args::set_flag(&mut tokens, "state-dir", &dir.to_string_lossy());
        if !detach {
            eprintln!("ruddr: state-dir={}", dir.display());
        }
        cfg.state_dir = dir;
    }
    if cfg.prompt_file.as_os_str() == "-" {
        cfg.prompt_file = detach::write_stdin_prompt(&cfg.state_dir, std::io::stdin().lock())?;
    }
    if detach {
        let ruddr = std::env::current_exe().map_err(|e| Error::failed(format!("locate the Ruddr executable: {e}")))?;
        let mut command = vec![ruddr.into_os_string(), OsString::from("run")];
        let child = args::detached_child_args(&tokens, &cfg.prompt_file.to_string_lossy(), child_args.as_deref());
        command.extend(child.into_iter().map(OsString::from));
        let startup = detach::start_detached_run(&cfg.state_dir, &command, detach::STARTUP_WINDOW)?;
        println!(
            "detached run: state-dir={} pid={} status={}",
            startup.state_dir, startup.pid, startup.status
        );
        return Ok(());
    }
    let cancel = CancelToken::new();
    // Without handlers a signal still stops the controller, only less
    // cleanly; that is no reason to refuse the run.
    let _ = signals::install(&cancel);
    run_controller(cfg, &cancel)
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// Serializes tests that change process-wide environment variables.
    pub static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// A private temporary directory removed on drop.
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new(name: &str) -> TempDir {
            Self::under(&std::env::temp_dir(), name)
        }

        /// A directory under /tmp on Unix, for short socket paths.
        pub fn in_tmp(name: &str) -> TempDir {
            if cfg!(unix) {
                Self::under(Path::new("/tmp"), name)
            } else {
                Self::new(name)
            }
        }

        fn under(root: &Path, name: &str) -> TempDir {
            let dir = root.join(format!("ruddr-{name}-{}", ruddr_core::fsutil::random_hex(4)));
            ruddr_core::fsutil::create_private_dir(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl std::ops::Deref for TempDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
