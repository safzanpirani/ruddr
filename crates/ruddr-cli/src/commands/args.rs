//! A small GNU-style flag parser shared by every command in this crate.
//!
//! Flags take `--name value` or `--name=value`. Boolean flags take no value,
//! or `--name=true|false`. Flags and positional arguments may be mixed, and
//! `--` ends flag parsing. A flag may also have a one-letter short form
//! (`-n 25`). `-h` and `--help` print the command's flag help and exit 0.

use ruddr_core::{Error, Exit, Result};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A switch: present or absent.
    Bool,
    /// One value; the last occurrence wins.
    Value,
    /// A repeatable value; every occurrence is kept in order.
    Multi,
}

#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub name: &'static str,
    pub short: Option<char>,
    pub kind: Kind,
    /// The value placeholder shown in help, such as `DIR`.
    pub value: &'static str,
    pub help: &'static str,
}

pub const fn flag(name: &'static str, help: &'static str) -> Spec {
    Spec {
        name,
        short: None,
        kind: Kind::Bool,
        value: "",
        help,
    }
}

pub const fn value(name: &'static str, value: &'static str, help: &'static str) -> Spec {
    Spec {
        name,
        short: None,
        kind: Kind::Value,
        value,
        help,
    }
}

pub const fn multi(name: &'static str, value: &'static str, help: &'static str) -> Spec {
    Spec {
        name,
        short: None,
        kind: Kind::Multi,
        value,
        help,
    }
}

impl Spec {
    pub const fn with_short(mut self, short: char) -> Spec {
        self.short = Some(short);
        self
    }
}

/// The result of parsing one command line.
#[derive(Debug, Default)]
pub struct Parsed {
    values: HashMap<&'static str, Vec<String>>,
    pub positionals: Vec<String>,
}

impl Parsed {
    pub fn bool(&self, name: &str) -> bool {
        self.values.get(name).and_then(|v| v.last()).is_some_and(|v| v == "true")
    }

    pub fn string(&self, name: &str) -> Option<String> {
        self.values.get(name).and_then(|v| v.last()).cloned()
    }

    pub fn string_or(&self, name: &str, default: &str) -> String {
        self.string(name).unwrap_or_else(|| default.to_string())
    }

    pub fn all(&self, name: &str) -> Vec<String> {
        self.values.get(name).cloned().unwrap_or_default()
    }

    /// A Go-syntax duration (`30s`, `10m`, `1h30m`). Bare integers are invalid.
    pub fn duration(&self, name: &str, default: Duration) -> Result<Duration> {
        match self.string(name) {
            None => Ok(default),
            Some(text) => {
                ruddr_core::duration::parse(&text).map_err(|e| Error::usage(format!("invalid value {text:?} for flag --{name}: {e}")))
            }
        }
    }

    pub fn int(&self, name: &str, default: i64) -> Result<i64> {
        match self.string(name) {
            None => Ok(default),
            Some(text) => text
                .trim()
                .parse()
                .map_err(|_| Error::usage(format!("invalid value {text:?} for flag --{name}: not an integer"))),
        }
    }
}

/// The error a help request returns: exit 0 with nothing more to print.
pub fn help_requested() -> Error {
    Error::new(Exit::Success, "")
}

/// Parses `args` against `specs`. `command` names the command in help text.
pub fn parse(command: &str, specs: &[Spec], args: &[String]) -> Result<Parsed> {
    let mut parsed = Parsed::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        index += 1;
        if arg == "--" {
            parsed.positionals.extend(args[index..].iter().cloned());
            break;
        }
        if arg == "-h" || arg == "--help" {
            eprint!("{}", help_text(command, specs));
            return Err(help_requested());
        }
        let (spec, inline) = if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (long, None),
            };
            let spec = specs
                .iter()
                .find(|s| s.name == name)
                .ok_or_else(|| unknown_flag(command, arg, specs))?;
            (spec, inline)
        } else if arg.len() > 1 && arg.starts_with('-') {
            let mut chars = arg[1..].chars();
            let letter = chars.next().unwrap_or('-');
            let rest: String = chars.collect();
            let spec = specs
                .iter()
                .find(|s| s.short == Some(letter))
                .ok_or_else(|| unknown_flag(command, arg, specs))?;
            let rest = rest.strip_prefix('=').map(str::to_string).unwrap_or(rest);
            (spec, if rest.is_empty() { None } else { Some(rest) })
        } else {
            parsed.positionals.push(arg.clone());
            continue;
        };
        let value = match spec.kind {
            Kind::Bool => match inline.as_deref() {
                None | Some("true") | Some("1") => "true".to_string(),
                Some("false") | Some("0") => "false".to_string(),
                Some(other) => {
                    return Err(Error::usage(format!("invalid boolean value {other:?} for flag --{}", spec.name)));
                }
            },
            Kind::Value | Kind::Multi => match inline {
                Some(value) => value,
                None => {
                    if index >= args.len() {
                        return Err(Error::usage(format!("flag needs an argument: --{}", spec.name)));
                    }
                    index += 1;
                    args[index - 1].clone()
                }
            },
        };
        let slot = parsed.values.entry(spec.name).or_default();
        if spec.kind == Kind::Multi {
            slot.push(value);
        } else {
            *slot = vec![value];
        }
    }
    Ok(parsed)
}

fn unknown_flag(command: &str, arg: &str, specs: &[Spec]) -> Error {
    eprint!("{}", help_text(command, specs));
    Error::usage(format!("flag provided but not defined: {arg}"))
}

/// The flag list printed by `--help` and after an unknown flag.
pub fn help_text(command: &str, specs: &[Spec]) -> String {
    let mut out = format!("Usage of ruddr {command}:\n");
    for spec in specs {
        let mut head = format!("--{}", spec.name);
        if let Some(short) = spec.short {
            head = format!("-{short}, {head}");
        }
        if spec.kind != Kind::Bool {
            head.push(' ');
            head.push_str(spec.value);
        }
        out.push_str(&format!("  {head}\n    \t{}\n", spec.help));
    }
    out
}

/// Fails with a usage error when a command got positional arguments it does
/// not take.
pub fn no_positionals(command: &str, parsed: &Parsed) -> Result<()> {
    if parsed.positionals.is_empty() {
        return Ok(());
    }
    Err(Error::usage(format!(
        "unexpected {command} arguments {:?}",
        parsed.positionals.join(" ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPECS: &[Spec] = &[
        multi("state-dir", "DIR", "run state directory"),
        flag("json", "print JSON"),
        value("timeout", "DURATION", "maximum wait"),
        value("n", "N", "lines").with_short('n'),
    ];

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_gnu_long_flags_and_positionals() {
        let parsed = parse(
            "status",
            SPECS,
            &args(&[
                "--state-dir",
                "a",
                "text",
                "--state-dir=b",
                "--json",
                "--timeout=5s",
                "-n",
                "3",
                "--",
                "--json",
            ]),
        )
        .unwrap();
        assert_eq!(parsed.all("state-dir"), vec!["a", "b"]);
        assert!(parsed.bool("json"));
        assert_eq!(parsed.duration("timeout", Duration::ZERO).unwrap(), Duration::from_secs(5));
        assert_eq!(parsed.int("n", 25).unwrap(), 3);
        assert_eq!(parsed.positionals, vec!["text", "--json"]);
    }

    #[test]
    fn short_flag_takes_attached_value_and_long_alias() {
        assert_eq!(parse("peek", SPECS, &args(&["-n40"])).unwrap().int("n", 0).unwrap(), 40);
        assert_eq!(parse("peek", SPECS, &args(&["--n", "7"])).unwrap().int("n", 0).unwrap(), 7);
    }

    #[test]
    fn rejects_unknown_flags_missing_values_and_bare_integer_durations() {
        assert_eq!(parse("status", SPECS, &args(&["--bogus"])).unwrap_err().exit, Exit::Usage);
        assert_eq!(parse("status", SPECS, &args(&["-state-dir", "x"])).unwrap_err().exit, Exit::Usage);
        assert_eq!(parse("status", SPECS, &args(&["--state-dir"])).unwrap_err().exit, Exit::Usage);
        let parsed = parse("wait", SPECS, &args(&["--timeout", "30"])).unwrap();
        assert_eq!(parsed.duration("timeout", Duration::ZERO).unwrap_err().exit, Exit::Usage);
    }

    #[test]
    fn help_exits_zero() {
        let error = parse("status", SPECS, &args(&["--help"])).unwrap_err();
        assert_eq!(error.code(), 0);
        assert!(error.message.is_empty());
    }

    #[test]
    fn a_lone_dash_is_positional() {
        assert_eq!(parse("steer", SPECS, &args(&["-"])).unwrap().positionals, vec!["-"]);
    }
}
