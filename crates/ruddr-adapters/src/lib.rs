//! App-server adapters for providers that do not speak the Codex app-server
//! protocol natively: Claude Code, OpenCode, Pi, and Factory Droid. Each one
//! runs as `ruddr app-server --provider NAME`, a child process that speaks
//! line-delimited JSON-RPC on stdio exactly like `codex app-server`.
//! Port of adapter/, claude/, opencode/, pi/, droid/.
//!
//! The adapters implement the lifecycle subset `ruddr run` uses:
//! `initialize`, `thread/start`, `thread/resume` (and `thread/fork` for
//! Droid), `turn/start`, `turn/steer`, and `turn/interrupt`, and they emit
//! `turn/started`, `item/*`, `item/agentMessage/delta`,
//! `thread/tokenUsage/updated`, and `turn/completed`. They never read or
//! store provider credentials; each provider CLI authenticates itself.

pub mod child;
pub mod claude;
pub mod opencode;
pub mod protocol;

#[cfg(test)]
mod testing;

use protocol::{Adapter, Emit, WriterSink};
use std::sync::Arc;

const USAGE: &str = "usage: ruddr app-server --provider claude|opencode|pi|droid [--executable PATH]";

/// Entry point for the hidden `ruddr app-server --provider NAME` command.
/// `args` may start with `app-server`; the rest are its flags.
pub fn app_server_command(args: Vec<String>) -> ruddr_core::Result<()> {
    let (provider, executable) = parse_args(&args)?;
    let emit: Emit = Arc::new(WriterSink::new(std::io::stdout()));
    let adapter = new_adapter(&provider, executable, emit.clone())?;
    protocol::serve(adapter.as_ref(), std::io::stdin().lock(), emit.as_ref())
        .map_err(|error| ruddr_core::Error::failed(format!("app-server {provider}: {error}")))
}

/// Builds the adapter for a provider. Without an explicit executable, each
/// one takes its `RUDDR_<PROVIDER>_PATH` override (or the `RUDDER_` spelling
/// earlier releases read), then looks on PATH under the names the TypeScript
/// adapters and the Go runner used. `ruddr thread` relies on this: it starts
/// the adapter without `--executable`.
pub fn new_adapter(provider: &str, executable: Option<String>, emit: Emit) -> ruddr_core::Result<Box<dyn Adapter>> {
    let resolve = |variable: &str, names: &[&str]| resolve_executable(executable.clone(), variable, names);
    Ok(match provider {
        "claude" => Box::new(claude::ClaudeAdapter::new(emit, resolve("CLAUDE", &["claude"]))),
        "opencode" => Box::new(opencode::OpenCodeAdapter::new(
            emit,
            resolve("OPENCODE", &["opencode2", "opencode-next"]),
        )),
        "codex" => {
            return Err(ruddr_core::Error::usage(
                "codex speaks the app-server protocol itself; run `codex app-server`",
            ));
        }
        other => {
            return Err(ruddr_core::Error::usage(format!(
                "unsupported provider {other:?}; expected claude, opencode, pi, or droid"
            )));
        }
    })
}

/// `--executable`, then the environment override, then PATH. When nothing is
/// found, the first name is kept so the spawn error names it.
fn resolve_executable(explicit: Option<String>, provider: &str, names: &[&str]) -> String {
    let variables = [format!("RUDDR_{provider}_PATH"), format!("RUDDER_{provider}_PATH")];
    explicit
        .or_else(|| ruddr_core::paths::env_any(&[variables[0].as_str(), variables[1].as_str()]))
        .or_else(|| child::find_on_path(names).map(|path| path.to_string_lossy().into_owned()))
        .unwrap_or_else(|| names[0].to_string())
}

fn parse_args(args: &[String]) -> ruddr_core::Result<(String, Option<String>)> {
    let mut provider = None;
    let mut executable = None;
    let mut rest = args.iter().peekable();
    if rest.peek().map(|arg| arg.as_str()) == Some("app-server") {
        rest.next();
    }
    while let Some(arg) = rest.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_string())),
            _ => (arg.as_str(), None),
        };
        let slot = match name {
            "--provider" => &mut provider,
            "--executable" => &mut executable,
            "-h" | "--help" => return Err(ruddr_core::Error::usage(USAGE)),
            _ => return Err(ruddr_core::Error::usage(format!("unknown app-server argument {arg:?}\n{USAGE}"))),
        };
        let value = match inline {
            Some(value) => value,
            None => rest
                .next()
                .cloned()
                .ok_or_else(|| ruddr_core::Error::usage(format!("{name} needs a value\n{USAGE}")))?,
        };
        if value.is_empty() {
            return Err(ruddr_core::Error::usage(format!("{name} needs a value\n{USAGE}")));
        }
        *slot = Some(value);
    }
    let provider = provider.ok_or_else(|| ruddr_core::Error::usage(format!("--provider is required\n{USAGE}")))?;
    Ok((provider, executable))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_gnu_style_flags() {
        assert_eq!(parse_args(&args(&["app-server", "--provider", "pi"])).unwrap(), ("pi".into(), None));
        assert_eq!(
            parse_args(&args(&["--provider=droid", "--executable", "/opt/droid"])).unwrap(),
            ("droid".into(), Some("/opt/droid".into()))
        );
        for bad in [&["--provider"][..], &["--model", "x"], &[], &["--provider="], &["-provider", "pi"]] {
            let error = parse_args(&args(bad)).unwrap_err();
            assert_eq!(error.exit, ruddr_core::Exit::Usage, "{bad:?}");
        }
        let sink: Emit = Arc::new(protocol::tests::Collector::default());
        assert_eq!(
            new_adapter("codex", None, sink.clone()).err().unwrap().exit,
            ruddr_core::Exit::Usage
        );
        assert_eq!(new_adapter("gemini", None, sink).err().unwrap().exit, ruddr_core::Exit::Usage);
    }

    #[test]
    fn resolves_the_executable_from_the_flag_then_the_environment_then_path() {
        assert_eq!(resolve_executable(Some("/opt/x".into()), "RUDDR_TEST_UNSET", &["pi"]), "/opt/x");
        assert_eq!(
            resolve_executable(None, "RUDDR_TEST_UNSET", &["ruddr-no-such-binary", "x"]),
            "ruddr-no-such-binary"
        );
        #[cfg(unix)]
        assert!(resolve_executable(None, "RUDDR_TEST_UNSET", &["ruddr-no-such-binary", "sh"]).ends_with("/sh"));
    }
}
