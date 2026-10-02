//! `ruddr web`: the browser dashboard server. It serves the client bundled
//! from web/client (embedded at build time by scripts/build-web.ts), the
//! token-gated JSON API, and the server-sent event streams. Port of
//! web/server.ts and web_command.go.

pub mod activity;
pub mod app;
pub mod args;
pub mod assets;
pub mod files;
pub mod git;
pub mod http;
pub mod launch;
pub mod sse;
pub mod token;

use app::App;
use args::WebArguments;
use ruddr_core::{Error, Result};
use std::sync::Arc;

/// Entry point for `ruddr web ARGS...`.
pub fn web_command(args: Vec<String>) -> Result<()> {
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h" | "help") {
        print_usage();
        return Ok(());
    }
    let parsed = args::parse_web_arguments(&args, &|name| std::env::var(name).ok())?;
    let token = token::load_token(&parsed.token_file.clone().unwrap_or_else(token::default_token_file))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::failed(format!("start the web server runtime: {e}")))?;
    let result = runtime.block_on(serve(parsed, token));
    // Open event streams and blocking reads must not delay exit.
    runtime.shutdown_background();
    result
}

/// The axum router: one handler owns the whole surface, as in server.ts.
pub fn router(app: Arc<App>) -> axum::Router {
    axum::Router::new().fallback(move |request: axum::extract::Request| app.clone().handle(request))
}

async fn serve(args: WebArguments, token: String) -> Result<()> {
    let (host, port, open) = (args.host.clone(), args.port, args.open);
    let app = App::new(args, token.clone());
    app.refresh_sessions().await;
    let listener = tokio::net::TcpListener::bind((host.as_str(), port))
        .await
        .map_err(|e| Error::failed(format!("listen on {host}:{port}: {e}")))?;
    let bound = listener.local_addr()?.port();
    let shown = if host == "0.0.0.0" || host == "::" {
        app::hostname()
    } else {
        host.clone()
    };
    let shown = if shown.contains(':') { format!("[{shown}]") } else { shown };
    let url = format!("http://{shown}:{bound}/?token={token}");
    println!("Ruddr web is serving {} sessions", app.sessions().len());
    println!("Open {url}");
    if !is_loopback(&host) {
        println!(
            "Warning: this address is reachable from other machines. Anyone with the token can steer your agents. Prefer a Tailscale address."
        );
    }
    if open {
        open_browser(&url);
    }
    tokio::select! {
        served = axum::serve(listener, router(app)) => served.map_err(|e| Error::failed(format!("web server: {e}"))),
        _ = shutdown_signal() => Ok(()),
    }
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "::1" | "localhost")
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut interrupt), Ok(mut terminate)) = (signal(SignalKind::interrupt()), signal(SignalKind::terminate())) else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let spawned = std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    // The printed link is enough when no opener exists.
    if let Ok(mut child) = spawned {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

pub fn print_usage() {
    eprint!(
        "Ruddr web dashboard

Usage:
  ruddr web [--host 127.0.0.1] [--port 4519] [--root DIR]... [--state-dir DIR]...
            [--interval 1s] [--token-file FILE] [--open]

Serves the TUI's sessions dashboard to a browser: live chat with streaming
tool calls, inline diffs for available edit patches, activity, output, and the working
tree diff with a file tree. It can steer, prompt, continue, interrupt, and
start sessions, and it shares the TUI's theme.

Every API call needs the access token in ~/.config/ruddr/web-token, created
on first use. Open the printed link once; it stores the token in a cookie.
The server listens on 127.0.0.1 by default. To reach it from a phone, pass a
private address such as a Tailscale IP with --host. Anyone with the token can
steer your agents, so do not expose it on a public interface.

Sessions come from the global registry plus .scratch below the current
directory; --root and --state-dir add more. RUDDR_WEB_HOST and RUDDR_WEB_PORT
set the defaults.
"
    );
}
