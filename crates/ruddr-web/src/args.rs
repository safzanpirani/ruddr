//! `ruddr web` flags and environment defaults, ported from
//! `parseWebArguments` in web/server.ts. Flags are GNU style: `--flag value`
//! and `--flag=value`.

use ruddr_core::{Error, Result};
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 4519;

#[derive(Debug, Clone)]
pub struct WebArguments {
    /// The executable that starts runs and lists models: this binary.
    pub ruddr: PathBuf,
    pub host: String,
    pub port: u16,
    pub roots: Vec<PathBuf>,
    pub state_dirs: Vec<PathBuf>,
    pub interval: Duration,
    pub open: bool,
    pub token_file: Option<PathBuf>,
    pub update_available: Option<String>,
    /// Registries discovery reads. `None` reads the default registries.
    /// Tests pass `Some(vec![])` to stay away from the real registry.
    pub registries: Option<Vec<PathBuf>>,
    /// Where `tui.json` lives. Tests point it at a temporary directory.
    pub config_dir: PathBuf,
}

/// Parses `ruddr web` arguments. `environment` looks up a variable by name.
pub fn parse_web_arguments(argv: &[String], environment: &dyn Fn(&str) -> Option<String>) -> Result<WebArguments> {
    let non_empty = |name: &str| environment(name).filter(|value| !value.is_empty());
    let port = match non_empty("RUDDR_WEB_PORT") {
        Some(raw) => parse_port(&raw).ok_or_else(|| Error::usage("web port must be 0-65535"))?,
        None => DEFAULT_PORT,
    };
    let mut args = WebArguments {
        ruddr: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ruddr")),
        host: non_empty("RUDDR_WEB_HOST").unwrap_or_else(|| DEFAULT_HOST.to_string()),
        port,
        roots: Vec::new(),
        state_dirs: Vec::new(),
        interval: Duration::from_millis(1000),
        open: false,
        token_file: None,
        update_available: non_empty("RUDDR_UPDATE_AVAILABLE"),
        registries: None,
        config_dir: ruddr_core::paths::config_dir(),
    };
    let mut index = 0;
    while index < argv.len() {
        let raw = &argv[index];
        index += 1;
        let (flag, inline) = match raw.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag.to_string(), Some(value.to_string())),
            _ => (raw.clone(), None),
        };
        let mut value = || -> Result<String> {
            if let Some(value) = inline.clone() {
                return Ok(value);
            }
            match argv.get(index) {
                Some(next) if !next.starts_with("--") => {
                    index += 1;
                    Ok(next.clone())
                }
                _ => Err(Error::usage(format!("{flag} requires a value"))),
            }
        };
        match flag.as_str() {
            "--host" => args.host = value()?,
            "--port" => args.port = parse_port(&value()?).ok_or_else(|| Error::usage("--port must be 0-65535"))?,
            "--root" => args.roots.push(ruddr_core::paths::absolute(value()?.as_ref())),
            "--state-dir" => args.state_dirs.push(ruddr_core::paths::absolute(value()?.as_ref())),
            "--interval" => {
                let interval = parse_interval(&value()?).ok_or_else(|| Error::usage("--interval must look like 500ms or 2s"))?;
                if interval < Duration::from_millis(100) {
                    return Err(Error::usage("--interval must be at least 100ms"));
                }
                args.interval = interval;
            }
            "--token-file" => args.token_file = Some(ruddr_core::paths::absolute(value()?.as_ref())),
            "--open" => {
                if inline.is_some() {
                    return Err(Error::usage("--open takes no value"));
                }
                args.open = true;
            }
            _ => return Err(Error::usage(format!("unknown web flag {raw}"))),
        }
    }
    if args.roots.is_empty() {
        args.roots.push(ruddr_core::paths::absolute(".scratch".as_ref()));
    }
    Ok(args)
}

/// A whole number in 0..=65535. Like JavaScript's `Number`, surrounding
/// whitespace is ignored and an empty value means 0.
fn parse_port(raw: &str) -> Option<u16> {
    let text = raw.trim();
    if text.is_empty() {
        return Some(0);
    }
    let value: f64 = text.parse().ok()?;
    if !value.is_finite() || value.fract() != 0.0 || !(0.0..=65535.0).contains(&value) {
        return None;
    }
    Some(value as u16)
}

/// `500ms`, `2s`, or `1.5s`: digits, an optional fraction, then `ms` or `s`.
fn parse_interval(raw: &str) -> Option<Duration> {
    let (number, scale) = if let Some(number) = raw.strip_suffix("ms") {
        (number, 1.0)
    } else {
        (raw.strip_suffix('s')?, 1000.0)
    };
    let (whole, fraction) = match number.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (number, None),
    };
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || fraction.is_some_and(|fraction| !digits(fraction)) {
        return None;
    }
    let millis = number.parse::<f64>().ok()? * scale;
    if !millis.is_finite() {
        return None;
    }
    Some(Duration::from_secs_f64(millis / 1000.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse(argv: &[&str], env: &[(&str, &str)]) -> Result<WebArguments> {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let env: Vec<(String, String)> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        parse_web_arguments(&argv, &|name| env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()))
    }

    #[test]
    fn parses_flags_and_rejects_bad_values() {
        let args = parse(
            &["--host", "100.64.0.1", "--port", "0", "--interval", "2s", "--state-dir", "/tmp/x"],
            &[],
        )
        .unwrap();
        assert_eq!(args.host, "100.64.0.1");
        assert_eq!(args.port, 0);
        assert_eq!(args.interval, Duration::from_secs(2));
        assert_eq!(args.state_dirs, vec![ruddr_core::paths::absolute(Path::new("/tmp/x"))]);
        for bad in [
            &["--interval", "5"][..],
            &["--bogus"],
            &["--interval", "50ms"],
            &["--port", "70000"],
            &["--host"],
            &["--port", "--open"],
        ] {
            let error = parse(bad, &[]).unwrap_err();
            assert_eq!(error.exit, ruddr_core::Exit::Usage, "{bad:?}");
        }
        assert_eq!(parse(&[], &[("RUDDR_WEB_PORT", "NaN")]).unwrap_err().exit, ruddr_core::Exit::Usage);
    }

    #[test]
    fn reads_environment_defaults_and_inline_values() {
        let args = parse(
            &["--port=9000", "--interval=500ms", "--root=/w/.scratch", "--open"],
            &[("RUDDR_WEB_HOST", "::1")],
        )
        .unwrap();
        assert_eq!((args.host.as_str(), args.port, args.open), ("::1", 9000, true));
        assert_eq!(args.interval, Duration::from_millis(500));
        assert_eq!(args.roots, vec![ruddr_core::paths::absolute(Path::new("/w/.scratch"))]);
        let args = parse(&[], &[("RUDDR_WEB_PORT", "8123"), ("RUDDR_WEB_HOST", "")]).unwrap();
        assert_eq!((args.host.as_str(), args.port), (DEFAULT_HOST, 8123));
        assert!(args.roots[0].ends_with(".scratch"));
        assert_eq!(parse_interval("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_interval("1.s"), None);
    }
}
