//! `ruddr run` flags. Flags are GNU style (`--flag value`, `--flag=value`);
//! a custom app-server command follows `--`. The parsed flags keep their
//! original spelling so `--detach` can re-run the same command line in the
//! background.

use crate::config::RunConfig;
use ruddr_core::{Error, Result};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Text,
    Bool,
    Duration,
    Repeat,
}

struct Spec {
    name: &'static str,
    kind: Kind,
    value: &'static str,
    help: &'static str,
}

const SPECS: &[Spec] = &[
    Spec {
        name: "provider",
        kind: Kind::Text,
        value: "NAME",
        help: "provider: codex, claude, opencode, pi, or droid (default codex)",
    },
    Spec {
        name: "cwd",
        kind: Kind::Text,
        value: "DIR",
        help: "working directory for the provider session (default the current directory)",
    },
    Spec {
        name: "prompt-file",
        kind: Kind::Text,
        value: "FILE",
        help: "file containing the initial task; - reads it from stdin",
    },
    Spec {
        name: "image",
        kind: Kind::Repeat,
        value: "FILE",
        help: "attach a png, jpg, gif, or webp image to the initial task (repeatable)",
    },
    Spec {
        name: "state-dir",
        kind: Kind::Text,
        value: "DIR",
        help: "directory for state, trace, and output (default CWD/.scratch/ruddr/<time>-<id>)",
    },
    Spec {
        name: "model",
        kind: Kind::Text,
        value: "ID",
        help: "provider model (default the provider's catalog default)",
    },
    Spec {
        name: "effort",
        kind: Kind::Text,
        value: "LEVEL",
        help: "reasoning effort override",
    },
    Spec {
        name: "sandbox",
        kind: Kind::Text,
        value: "MODE",
        help: "read-only, workspace-write, or danger-full-access (default workspace-write)",
    },
    Spec {
        name: "approval-policy",
        kind: Kind::Text,
        value: "POLICY",
        help: "Codex approval policy; adapters require never (default never)",
    },
    Spec {
        name: "claude-path",
        kind: Kind::Text,
        value: "PATH",
        help: "Claude Code executable for --provider claude",
    },
    Spec {
        name: "opencode-path",
        kind: Kind::Text,
        value: "PATH",
        help: "OpenCode 2 executable for --provider opencode",
    },
    Spec {
        name: "pi-path",
        kind: Kind::Text,
        value: "PATH",
        help: "Pi executable for --provider pi",
    },
    Spec {
        name: "droid-path",
        kind: Kind::Text,
        value: "PATH",
        help: "Factory Droid executable for --provider droid",
    },
    Spec {
        name: "ephemeral",
        kind: Kind::Bool,
        value: "",
        help: "do not persist the provider session",
    },
    Spec {
        name: "resume-thread",
        kind: Kind::Text,
        value: "ID",
        help: "resume this provider thread/session before starting the turn",
    },
    Spec {
        name: "fork-thread",
        kind: Kind::Text,
        value: "ID",
        help: "fork this thread before starting the turn",
    },
    Spec {
        name: "fork-before-turn",
        kind: Kind::Text,
        value: "ID",
        help: "when forking, exclude this turn and everything after it",
    },
    Spec {
        name: "fork-through-turn",
        kind: Kind::Text,
        value: "ID",
        help: "when forking, include history through this turn",
    },
    Spec {
        name: "turn-timeout",
        kind: Kind::Duration,
        value: "DURATION",
        help: "maximum active turn duration, applied per turn; 0 disables the watchdog (default 1h)",
    },
    Spec {
        name: "idle",
        kind: Kind::Bool,
        value: "",
        help: "stay alive after a turn completes and accept prompt commands on the control socket",
    },
    Spec {
        name: "idle-timeout",
        kind: Kind::Duration,
        value: "DURATION",
        help: "exit after this long idle; 0 disables (default 4h)",
    },
    Spec {
        name: "detach",
        kind: Kind::Bool,
        value: "",
        help: "start the controller in the background and return once it is running",
    },
    Spec {
        name: "config",
        kind: Kind::Repeat,
        value: "KEY=VALUE",
        help: "Codex config override for this run (repeatable)",
    },
];

/// The `ruddr run --help` text.
pub fn usage() -> String {
    let mut text = String::from(
        "Usage: ruddr run [--provider codex|claude|opencode|pi|droid] --prompt-file FILE [--state-dir DIR] [options]\n\
         \x20                [-- APP_SERVER_COMMAND...]\n\nOptions:\n",
    );
    for spec in SPECS {
        let flag = if spec.value.is_empty() {
            format!("--{}", spec.name)
        } else {
            format!("--{} {}", spec.name, spec.value)
        };
        text.push_str(&format!("  {flag:<28} {}\n", spec.help));
    }
    text.push_str(
        "\nDurations use Go syntax (3600s, 20m, 1h); bare integers are invalid. Without --state-dir the run\n\
         uses CWD/.scratch/ruddr/<time>-<id>, which ignores itself in Git, and prints the path. A command\n\
         after -- replaces the default codex app-server command.\n",
    );
    text
}

/// One parsed flag in its original spelling.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub name: String,
    pub value: Option<String>,
    /// Written as `--name=value`.
    pub inline: bool,
}

/// A parsed `ruddr run` command line.
#[derive(Debug, Clone)]
pub struct Parsed {
    pub cfg: RunConfig,
    pub detach: bool,
    pub help: bool,
    pub tokens: Vec<Token>,
    /// The command after `--`, if one was given.
    pub child_args: Option<Vec<String>>,
}

fn parse_bool(name: &str, value: &str) -> Result<bool> {
    match value {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Ok(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Ok(false),
        _ => Err(Error::usage(format!("invalid boolean value {value:?} for --{name}"))),
    }
}

pub fn parse(args: &[String]) -> Result<Parsed> {
    let (flag_args, child_args) = match args.iter().position(|a| a == "--") {
        Some(marker) => {
            let child = args[marker + 1..].to_vec();
            if child.is_empty() {
                return Err(Error::failed("app-server command after -- is empty"));
            }
            (&args[..marker], Some(child))
        }
        None => (args, None),
    };
    let mut parsed = Parsed {
        cfg: RunConfig::default(),
        detach: false,
        help: false,
        tokens: Vec::new(),
        child_args,
    };
    let mut index = 0;
    while index < flag_args.len() {
        let arg = &flag_args[index];
        index += 1;
        if arg == "-h" || arg == "--help" || arg == "-help" {
            parsed.help = true;
            continue;
        }
        let Some(body) = arg.strip_prefix("--") else {
            if arg.starts_with('-') && arg.len() > 1 {
                return Err(Error::usage(format!(
                    "unknown flag {arg}; Ruddr flags take two dashes, as in -{arg}"
                )));
            }
            let rest = flag_args[index - 1..].join(" ");
            return Err(Error::failed(format!(
                "unexpected run arguments {rest:?}; put a custom Codex app-server command after --"
            )));
        };
        let (name, inline) = match body.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (body, None),
        };
        let Some(spec) = SPECS.iter().find(|s| s.name == name) else {
            return Err(Error::usage(format!("unknown flag --{name}")));
        };
        let token = if spec.kind == Kind::Bool {
            Token {
                name: name.into(),
                inline: inline.is_some(),
                value: inline,
            }
        } else {
            match inline {
                Some(value) => Token {
                    name: name.into(),
                    value: Some(value),
                    inline: true,
                },
                None => {
                    let Some(value) = flag_args.get(index) else {
                        return Err(Error::usage(format!("flag needs an argument: --{name}")));
                    };
                    index += 1;
                    Token {
                        name: name.into(),
                        value: Some(value.clone()),
                        inline: false,
                    }
                }
            }
        };
        apply(&mut parsed, spec, token.value.as_deref().unwrap_or("true"))?;
        parsed.tokens.push(token);
    }
    Ok(parsed)
}

fn apply(parsed: &mut Parsed, spec: &Spec, value: &str) -> Result<()> {
    let cfg = &mut parsed.cfg;
    let duration = |value: &str| {
        ruddr_core::duration::parse(value).map_err(|e| Error::usage(format!("invalid value {value:?} for --{}: {e}", spec.name)))
    };
    match spec.name {
        "provider" => cfg.provider = value.into(),
        "cwd" => cfg.cwd = PathBuf::from(value),
        "prompt-file" => cfg.prompt_file = PathBuf::from(value),
        "image" => cfg.images.push(PathBuf::from(value)),
        "state-dir" => cfg.state_dir = PathBuf::from(value),
        "model" => cfg.model = value.into(),
        "effort" => cfg.effort = value.into(),
        "sandbox" => cfg.sandbox = value.into(),
        "approval-policy" => cfg.approval_policy = value.into(),
        "claude-path" => cfg.claude_path = value.into(),
        "opencode-path" => cfg.opencode_path = value.into(),
        "pi-path" => cfg.pi_path = value.into(),
        "droid-path" => cfg.droid_path = value.into(),
        "ephemeral" => cfg.ephemeral = parse_bool(spec.name, value)?,
        "resume-thread" => cfg.resume_thread_id = value.into(),
        "fork-thread" => cfg.fork_thread_id = value.into(),
        "fork-before-turn" => cfg.fork_before_turn_id = value.into(),
        "fork-through-turn" => cfg.fork_through_turn_id = value.into(),
        "turn-timeout" => cfg.turn_timeout = duration(value)?,
        "idle" => cfg.idle = parse_bool(spec.name, value)?,
        "idle-timeout" => cfg.idle_timeout = duration(value)?,
        "detach" => parsed.detach = parse_bool(spec.name, value)?,
        "config" => {
            ruddr_core::models::parse_config_override(value)?;
            cfg.codex_config.push(value.into());
        }
        other => unreachable!("flag --{other} has no handler"),
    }
    Ok(())
}

/// Sets a flag's value, keeping its spelling, or appends the flag.
pub fn set_flag(tokens: &mut Vec<Token>, name: &str, value: &str) {
    match tokens.iter_mut().find(|t| t.name == name) {
        Some(token) => token.value = Some(value.into()),
        None => tokens.push(Token {
            name: name.into(),
            value: Some(value.into()),
            inline: false,
        }),
    }
}

/// Renders flags back into arguments.
pub fn render(tokens: &[Token]) -> Vec<String> {
    let mut args = Vec::new();
    for token in tokens {
        match (&token.value, token.inline) {
            (Some(value), true) => args.push(format!("--{}={value}", token.name)),
            (Some(value), false) => {
                args.push(format!("--{}", token.name));
                args.push(value.clone());
            }
            (None, _) => args.push(format!("--{}", token.name)),
        }
    }
    args
}

/// The background controller's arguments: the same command line without
/// `--detach`, with `--prompt-file` pointing at the stored prompt and the
/// custom command after `--` kept as is.
pub fn detached_child_args(tokens: &[Token], prompt_file: &str, child_args: Option<&[String]>) -> Vec<String> {
    let mut kept: Vec<Token> = tokens.iter().filter(|t| t.name != "detach").cloned().collect();
    set_flag(&mut kept, "prompt-file", prompt_file);
    let mut args = render(&kept);
    if let Some(child) = child_args {
        args.push("--".into());
        args.extend(child.iter().cloned());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn parses_every_flag_in_both_spellings() {
        let parsed = parse(&strings(&[
            "--provider=droid",
            "--cwd",
            "/w",
            "--prompt-file",
            "-",
            "--state-dir=/s",
            "--model",
            "m",
            "--effort",
            "high",
            "--sandbox",
            "read-only",
            "--approval-policy",
            "never",
            "--claude-path",
            "/c",
            "--opencode-path",
            "/o",
            "--pi-path",
            "/p",
            "--droid-path",
            "/d",
            "--ephemeral",
            "--resume-thread",
            "r",
            "--fork-thread",
            "f",
            "--fork-before-turn",
            "b",
            "--fork-through-turn",
            "t",
            "--turn-timeout",
            "20m",
            "--idle=true",
            "--idle-timeout=0",
            "--detach",
            "--config",
            "a=b",
            "--config=c=d",
            "--image",
            "a.png",
            "--image=/b.jpg",
        ]))
        .unwrap();
        let cfg = &parsed.cfg;
        assert_eq!(cfg.provider, "droid");
        assert_eq!(cfg.cwd, PathBuf::from("/w"));
        assert_eq!(cfg.prompt_file, PathBuf::from("-"));
        assert_eq!(cfg.state_dir, PathBuf::from("/s"));
        assert_eq!(
            (cfg.model.as_str(), cfg.effort.as_str(), cfg.sandbox.as_str()),
            ("m", "high", "read-only")
        );
        assert_eq!(
            (
                cfg.claude_path.as_str(),
                cfg.opencode_path.as_str(),
                cfg.pi_path.as_str(),
                cfg.droid_path.as_str()
            ),
            ("/c", "/o", "/p", "/d")
        );
        assert!(cfg.ephemeral && cfg.idle && parsed.detach);
        assert_eq!((cfg.resume_thread_id.as_str(), cfg.fork_thread_id.as_str()), ("r", "f"));
        assert_eq!((cfg.fork_before_turn_id.as_str(), cfg.fork_through_turn_id.as_str()), ("b", "t"));
        assert_eq!(cfg.turn_timeout, std::time::Duration::from_secs(1200));
        assert_eq!(cfg.idle_timeout, std::time::Duration::ZERO);
        assert_eq!(cfg.codex_config, ["a=b", "c=d"]);
        assert_eq!(cfg.images, [PathBuf::from("a.png"), PathBuf::from("/b.jpg")]);
        assert!(parsed.child_args.is_none());
    }

    #[test]
    fn defaults_match_go() {
        let parsed = parse(&[]).unwrap();
        assert_eq!(parsed.cfg.provider, "codex");
        assert_eq!(parsed.cfg.sandbox, "workspace-write");
        assert_eq!(parsed.cfg.approval_policy, "never");
        assert_eq!(parsed.cfg.turn_timeout, std::time::Duration::from_secs(3600));
        assert_eq!(parsed.cfg.idle_timeout, std::time::Duration::from_secs(4 * 3600));
        assert!(!parsed.detach && !parsed.cfg.idle);
    }

    #[test]
    fn rejects_bad_flags_with_usage_errors() {
        for (args, wanted) in [
            (vec!["--bogus"], "unknown flag --bogus"),
            (vec!["-provider", "pi"], "two dashes"),
            (vec!["--model"], "needs an argument"),
            (vec!["--turn-timeout", "3600"], "invalid value"),
            (vec!["--turn-timeout", "-1s"], "invalid value"),
            (vec!["--idle=maybe"], "invalid boolean"),
            (vec!["--config", "no-equals-sign"], "KEY=VALUE"),
        ] {
            let error = parse(&strings(&args)).unwrap_err();
            assert_eq!(error.exit, ruddr_core::Exit::Usage, "{args:?}");
            assert!(error.message.contains(wanted), "{args:?}: {error}");
        }
        let bare = parse(&strings(&["--prompt-file", "p.md", "codex", "app-server"])).unwrap_err();
        assert!(bare.message.contains("after --") && bare.exit == ruddr_core::Exit::Failed, "{bare}");
        let empty = parse(&strings(&["--prompt-file", "p.md", "--"])).unwrap_err();
        assert!(empty.message.contains("after -- is empty"));
        assert!(parse(&strings(&["--help"])).unwrap().help);
    }

    #[test]
    fn child_command_follows_the_separator() {
        let parsed = parse(&strings(&["--prompt-file", "p.md", "--", "broker", "--state-dir", "x"])).unwrap();
        assert_eq!(parsed.child_args.as_deref().unwrap(), strings(&["broker", "--state-dir", "x"]));
        assert_eq!(parsed.cfg.state_dir, PathBuf::new());
    }

    #[test]
    fn detached_args_drop_detach_and_use_the_stored_prompt() {
        let parsed = parse(&strings(&[
            "--detach",
            "--provider=claude",
            "--prompt-file",
            "-",
            "--detach=true",
            "--state-dir",
            "x",
            "--",
            "codex",
            "--detach",
        ]))
        .unwrap();
        let args = detached_child_args(&parsed.tokens, "/abs/x/prompt.md", parsed.child_args.as_deref());
        assert_eq!(
            args,
            strings(&[
                "--provider=claude",
                "--prompt-file",
                "/abs/x/prompt.md",
                "--state-dir",
                "x",
                "--",
                "codex",
                "--detach"
            ])
        );
        let inline = parse(&strings(&["--prompt-file=p.md"])).unwrap();
        assert_eq!(
            detached_child_args(&inline.tokens, "/q.md", None),
            strings(&["--prompt-file=/q.md"])
        );
    }

    #[test]
    fn set_flag_replaces_or_appends() {
        let mut tokens = parse(&strings(&["--prompt-file", "p.md"])).unwrap().tokens;
        set_flag(&mut tokens, "state-dir", "/s");
        assert_eq!(render(&tokens), strings(&["--prompt-file", "p.md", "--state-dir", "/s"]));
        let mut tokens = parse(&strings(&["--state-dir=old"])).unwrap().tokens;
        set_flag(&mut tokens, "state-dir", "/s");
        assert_eq!(render(&tokens), strings(&["--state-dir=/s"]));
    }

    #[test]
    fn usage_lists_every_flag() {
        let text = usage();
        for spec in SPECS {
            assert!(text.contains(&format!("--{}", spec.name)), "{}", spec.name);
        }
    }
}
