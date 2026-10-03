//! The Ruddr terminal UI (ratatui). Entry point: [`tui_command`].
//!
//! Threads: one reads terminal input, one tails the selected session's
//! artifact files, and short-lived ones run control requests, launches, and
//! git. They all report to the UI thread over one channel. The UI thread
//! sleeps until a message, a streaming drain tick, a refresh, or a frame some
//! visible animation asked for, and redraws only when something changed,
//! never faster than 60 frames per second.

mod actions;
mod activity;
mod app;
mod cache;
mod core;
mod history;
mod tail;
mod text;
mod theme;
mod transcript;
mod ui;
mod view;

use crate::app::{App, Args, Msg};
use ratatui::crossterm::event::{self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture};
use ratatui::crossterm::execute;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

/// The shortest gap between two frames: 60 per second.
const FRAME: Duration = Duration::from_millis(16);

pub const USAGE: &str = "Ruddr live sessions TUI

Usage:
  ruddr tui [--root DIR]... [--state-dir DIR]... [--all] [--interval 500ms] [--theme NAME] [--beta] [--mobile]

Shows live runs first, then every finished run from the global registry plus
.scratch below the current directory. --root and --state-dir may be repeated;
--all is accepted for compatibility and has no effect.
The refresh interval accepts milliseconds or seconds and must be at least 100ms.
Press n for a new session. It starts in the current directory; send /cd DIR as
the prompt to start it and later new sessions in DIR instead. In any prompt,
Ctrl+V attaches the clipboard image, and dropped image files attach as images.
Press t inside the TUI to preview and save a theme. --theme overrides the saved
theme for one launch; RUDDR_TUI_THEME provides the same environment override.
--beta enables the chat-first layout; RUDDR_TUI_BETA=1 provides the same
override. The default layout keeps the sessions dashboard visible. Terminals
64 columns wide or narrower get the single-column mobile layout with a tappable
action bar; --mobile or RUDDR_TUI_MOBILE=1 forces it, and mobileWidthThreshold
in tui.json changes the width.
Press H to browse every agent's local sessions (Codex, Claude, Pi, OpenCode,
Droid) read-only, with each session's chat and file diff. In that list, e shows
only the sessions that edited files.
";

fn parse_args(argv: Vec<String>) -> Result<Args, String> {
    let env = |names: &[&str]| names.iter().find_map(|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty()));
    let mut args = Args {
        roots: vec![],
        state_dirs: vec![],
        interval: Duration::from_millis(500),
        theme: env(&["RUDDR_TUI_THEME", "RUDDER_TUI_THEME"]),
        beta: env(&["RUDDR_TUI_BETA", "RUDDER_TUI_BETA"]).as_deref() == Some("1"),
        mobile: env(&["RUDDR_TUI_MOBILE", "RUDDER_TUI_MOBILE"]).as_deref() == Some("1"),
        update: env(&["RUDDR_UPDATE_AVAILABLE"]).map(|v| v.trim().to_string()),
    };
    let mut iter = argv.into_iter();
    while let Some(arg) = iter.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| {
            inline
                .clone()
                .or_else(|| iter.next())
                .filter(|v| !v.is_empty())
                .ok_or(format!("{name} requires a value"))
        };
        let absolute = |v: String| ruddr_core::paths::absolute(&PathBuf::from(v));
        match flag.as_str() {
            "--root" => args.roots.push(absolute(value("--root")?)),
            // state.json records an absolute stateDir; resolve here so the
            // explicit-session comparisons match a relative argument.
            "--state-dir" => args.state_dirs.push(absolute(value("--state-dir")?)),
            "--interval" => args.interval = parse_interval(&value("--interval")?)?,
            "--theme" => args.theme = Some(value("--theme")?),
            "--beta" => args.beta = true,
            "--mobile" => args.mobile = true,
            // Every registered run is listed; --all stays accepted for scripts.
            "--all" => {}
            other => return Err(format!("unknown TUI argument {other}")),
        }
    }
    if let Some(name) = &args.theme
        && theme::find(name).is_none()
    {
        return Err(format!("unknown TUI theme {name}"));
    }
    if args.roots.is_empty() && args.state_dirs.is_empty() {
        args.roots.push(std::env::current_dir().unwrap_or_default().join(".scratch"));
    }
    Ok(args)
}

fn parse_interval(value: &str) -> Result<Duration, String> {
    let invalid = || "--interval must use milliseconds or seconds, for example 500ms or 2s".to_string();
    let (digits, scale) = if let Some(ms) = value.strip_suffix("ms") {
        (ms, 1)
    } else if let Some(s) = value.strip_suffix('s') {
        (s, 1000)
    } else {
        return Err(invalid());
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let ms = digits.parse::<u64>().map_err(|_| invalid())?.saturating_mul(scale);
    if ms < 100 {
        return Err("--interval must be at least 100ms".into());
    }
    Ok(Duration::from_millis(ms))
}

/// Forwards terminal input to the UI channel until `stop` is set.
fn spawn_input(tx: std::sync::mpsc::Sender<Msg>, stop: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("ruddr-tui-input".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match event::poll(Duration::from_millis(100)) {
                    Ok(true) => match event::read() {
                        Ok(event) => {
                            if tx.send(Msg::Input(event)).is_err() {
                                return;
                            }
                        }
                        Err(_) => return,
                    },
                    Ok(false) => {}
                    Err(_) => return,
                }
            }
        })
        .expect("spawn the input thread");
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> std::io::Result<()> {
    let mut last_draw = Instant::now() - FRAME;
    while !app.quit {
        let now = Instant::now();
        // Sleep until the soonest of: a frame we owe, an animation frame, a
        // streaming drain tick, a diff poll, or the session refresh.
        let mut wake = app.last_refresh + app.args.interval;
        if let Some(since) = app.loading_since.filter(|_| app.holding(now)) {
            // The load's message wakes the loop; this caps the wait.
            wake = wake.min(since + app::LOAD_HOLD);
        } else if app.dirty {
            wake = wake.min(last_draw + FRAME);
        }
        for deadline in [app.next_frame, app.drain_deadline(), app.diff_deadline()].into_iter().flatten() {
            wake = wake.min(deadline);
        }
        match app.rx.recv_timeout(wake.saturating_duration_since(now)) {
            Ok(msg) => {
                app.on_msg(msg);
                // Take everything already queued so a burst renders once.
                while let Ok(msg) = app.rx.try_recv() {
                    app.on_msg(msg);
                    if app.quit {
                        break;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let now = Instant::now();
        app.drain_stream(now);
        app.poll_diff(now);
        if now.duration_since(app.last_refresh) >= app.args.interval {
            app.refresh();
        }
        app.housekeeping();
        if app.next_frame.is_some_and(|t| now >= t) {
            app.next_frame = None;
            app.dirty = true;
        }
        if app.dirty && now.duration_since(last_draw) >= FRAME && !app.holding(now) && !app.quit {
            app.dirty = false;
            last_draw = now;
            terminal.draw(|frame| ui::draw(frame, app))?;
        }
    }
    Ok(())
}

/// Entry point for `ruddr tui ARGS...`.
pub fn tui_command(argv: Vec<String>) -> ruddr_core::Result<()> {
    if argv.len() == 1 && matches!(argv[0].as_str(), "--help" | "-h" | "help") {
        eprint!("{USAGE}");
        return Ok(());
    }
    let args = parse_args(argv).map_err(ruddr_core::Error::usage)?;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(ruddr_core::Error::failed("the TUI requires an interactive terminal"));
    }
    let mut app = App::new(args);
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    let stop = Arc::new(AtomicBool::new(false));
    spawn_input(app.tx.clone(), stop.clone());
    let result = run(&mut terminal, &mut app);
    stop.store(true, Ordering::Relaxed);
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    result.map_err(|error| ruddr_core::Error::failed(format!("TUI exited: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flags_and_intervals() {
        let args = parse_args(vec![
            "--state-dir=rel/run".into(),
            "--interval".into(),
            "2s".into(),
            "--all".into(),
            "--beta".into(),
        ])
        .unwrap();
        assert!(args.state_dirs[0].is_absolute() && args.state_dirs[0].ends_with("rel/run"));
        assert!(args.roots.is_empty(), "an explicit state dir skips the default root");
        assert_eq!(args.interval, Duration::from_secs(2));
        assert!(args.beta);
        let args = parse_args(vec![]).unwrap();
        assert!(args.roots[0].ends_with(".scratch"));
        assert_eq!(args.interval, Duration::from_millis(500));
        assert!(parse_args(vec!["--rs".into()]).unwrap_err().contains("unknown TUI argument --rs"));
        assert!(parse_args(vec!["--ruddr".into(), "x".into()]).is_err(), "--ruddr is gone");
        assert!(parse_args(vec!["--root".into()]).unwrap_err().contains("requires a value"));
        assert!(parse_args(vec!["--theme".into(), "no-such-theme".into()]).is_err());
        assert_eq!(parse_interval("100ms").unwrap(), Duration::from_millis(100));
        assert!(parse_interval("99ms").is_err());
        assert!(parse_interval("500").is_err(), "bare numbers stay invalid");
        assert!(parse_interval("1.5s").is_err());
    }
}
