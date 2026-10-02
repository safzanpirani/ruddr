//! The dashboard's state and routes. Every `/api` route needs the token;
//! mutations also need the same-origin header. Run files are read only for
//! sessions discovery verified, through the directory identity pinned at
//! verification. Port of `WebApp` in web/server.ts.

use crate::args::WebArguments;
use crate::assets;
use crate::files::{self, EventTail, Tick};
use crate::git::{self, Git};
use crate::http::{self, failure, json_response, json_text};
use crate::launch::{self, NewSession, PROVIDERS};
use crate::sse::{self, SseSender};
use crate::token::token_matches;
use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use axum::response::Response;
use ruddr_core::control::{self, Command};
use ruddr_core::paths::absolute;
use ruddr_core::session::{Discover, Session};
use ruddr_core::state::{RunState, Status};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

/// Request bodies larger than this are rejected.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const OUTPUT_TAIL_BYTES: u64 = 1024 * 1024;
const TRACE_TAIL_BYTES: u64 = 512 * 1024;
const ACTIVITY_EVENTS_TAIL_BYTES: u64 = 2 * 1024 * 1024;
const EVENTS_POLL: Duration = Duration::from_millis(150);
const STEER_TIMEOUT: Duration = Duration::from_secs(30);
const PROMPT_TIMEOUT: Duration = Duration::from_secs(60);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(35);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const DIRECTORY_LIMIT: usize = 40;
const DIRECTORY_CHANGED: &str = "The verified session directory changed";

#[derive(Debug, Clone, PartialEq)]
struct Verified {
    path: PathBuf,
    identity: String,
}

#[derive(Default)]
struct Snapshot {
    sessions: Vec<Session>,
    json: String,
}

pub struct App {
    args: Mutex<WebArguments>,
    token: String,
    snapshot: Mutex<Snapshot>,
    /// Bumped whenever the serialized session list changes.
    changes: watch::Sender<u64>,
    polling: Mutex<bool>,
    /// Each session directory's real path and identity when first verified.
    verified: Mutex<HashMap<PathBuf, Verified>>,
    git: Git,
}

type RouteResult = Result<Response, String>;

/// The JSON the client gets for one session: the state plus `stateFile`.
pub fn session_json(session: &Session) -> Value {
    let mut value = serde_json::to_value(&session.state).unwrap_or_default();
    if let Some(object) = value.as_object_mut() {
        object.insert("stateFile".into(), Value::String(session.state_file.to_string_lossy().into_owned()));
    }
    value
}

/// The route a typed prompt takes: active turns are steered, idle sessions
/// get a new turn, finished threads get a continuation run.
pub fn prompt_route(state: &RunState) -> Option<&'static str> {
    match state.status {
        Status::Active => Some("steer"),
        Status::Idle => Some("prompt"),
        Status::Completed | Status::Failed | Status::Interrupted
            if state.thread_id.as_deref().is_some_and(|t| !t.is_empty()) && !state.cwd.is_empty() =>
        {
            Some("continue")
        }
        _ => None,
    }
}

impl App {
    pub fn new(args: WebArguments, token: String) -> Arc<App> {
        Arc::new(App {
            args: Mutex::new(args),
            token,
            snapshot: Mutex::new(Snapshot {
                sessions: Vec::new(),
                json: "[]".into(),
            }),
            changes: watch::channel(0).0,
            polling: Mutex::new(false),
            verified: Mutex::new(HashMap::new()),
            git: Git::default(),
        })
    }

    pub fn sessions(&self) -> Vec<Session> {
        self.snapshot.lock().unwrap().sessions.clone()
    }

    fn sessions_json(&self) -> String {
        self.snapshot.lock().unwrap().json.clone()
    }

    fn args(&self) -> WebArguments {
        self.args.lock().unwrap().clone()
    }

    pub async fn refresh_sessions(self: &Arc<Self>) {
        let app = self.clone();
        let _ = tokio::task::spawn_blocking(move || app.refresh_blocking()).await;
    }

    /// Discovers sessions and keeps only those whose directory verifies.
    pub fn refresh_blocking(&self) {
        let args = self.args();
        let discovered = ruddr_core::session::discover(&Discover {
            state_dirs: args.state_dirs,
            roots: args.roots,
            registries: args.registries,
        });
        let sessions: Vec<Session> = discovered.into_iter().filter(|session| self.verify_discovered(session)).collect();
        let serialized = serde_json::to_string(&sessions.iter().map(session_json).collect::<Vec<_>>()).unwrap_or_else(|_| "[]".into());
        let changed = {
            let mut snapshot = self.snapshot.lock().unwrap();
            snapshot.sessions = sessions;
            let changed = snapshot.json != serialized;
            snapshot.json = serialized;
            changed
        };
        if changed {
            self.changes.send_modify(|version| *version += 1);
        }
    }

    /// Discovery must authorize the directory holding state.json, not a path
    /// that JSON supplies. The first verification pins the directory's real
    /// path and identity; a directory that later resolves elsewhere or was
    /// replaced stays excluded.
    fn verify_discovered(&self, session: &Session) -> bool {
        let state_dir = absolute(Path::new(&session.state.state_dir));
        if Some(state_dir.as_path()) != absolute(&session.state_file).parent() {
            return false;
        }
        let Ok(path) = std::fs::canonicalize(&state_dir) else {
            return false;
        };
        let Ok(identity) = files::directory_identity(&state_dir) else {
            return false;
        };
        if std::fs::canonicalize(&session.state_file).ok() != Some(path.join(ruddr_core::state::STATE_FILE)) {
            return false;
        }
        let mut verified = self.verified.lock().unwrap();
        let current = Verified { path, identity };
        match verified.get(&state_dir) {
            Some(previous) if *previous != current => false,
            _ => {
                verified.insert(state_dir, current);
                true
            }
        }
    }

    /// The verified real path of `session`'s directory, if it still resolves
    /// there and is still the same directory.
    pub fn verify_directory(&self, session: &Session) -> Result<PathBuf, String> {
        let key = absolute(Path::new(&session.state.state_dir));
        let verified = self.verified.lock().unwrap().get(&key).cloned().ok_or(DIRECTORY_CHANGED)?;
        if std::fs::canonicalize(&key).ok().as_ref() != Some(&verified.path) {
            return Err(DIRECTORY_CHANGED.into());
        }
        match files::directory_identity(&verified.path) {
            Ok(identity) if identity == verified.identity => Ok(verified.path),
            _ => Err(DIRECTORY_CHANGED.into()),
        }
    }

    /// Only directories of discovered sessions may be read or controlled.
    pub fn session_for(&self, state_dir: Option<&str>) -> Option<Session> {
        let wanted = absolute(Path::new(state_dir.filter(|dir| !dir.is_empty())?));
        self.snapshot
            .lock()
            .unwrap()
            .sessions
            .iter()
            .find(|s| absolute(Path::new(&s.state.state_dir)) == wanted)
            .cloned()
    }

    async fn known_session(self: &Arc<Self>, state_dir: Option<&str>) -> Option<Session> {
        let session = match self.session_for(state_dir) {
            Some(session) => session,
            None => {
                self.refresh_sessions().await;
                self.session_for(state_dir)?
            }
        };
        self.verify_directory(&session).ok()?;
        Some(session)
    }

    fn ensure_polling(self: &Arc<Self>) {
        let mut polling = self.polling.lock().unwrap();
        if *polling {
            return;
        }
        *polling = true;
        let app = self.clone();
        let interval = self.args.lock().unwrap().interval;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                {
                    let mut polling = app.polling.lock().unwrap();
                    if app.changes.receiver_count() == 0 {
                        *polling = false;
                        return;
                    }
                }
                app.refresh_sessions().await;
            }
        });
    }

    /// Handles one request. This is the whole HTTP surface.
    pub async fn handle(self: Arc<Self>, request: Request<Body>) -> Response {
        // TODO(review): Define allowed Host aliases for wildcard binds and reverse proxies before adding a DNS-rebinding Host allowlist.
        let path = request.uri().path().to_string();
        let query = request.uri().query().map(str::to_string);
        // Opening the printed link trades the query token for an HttpOnly cookie.
        if !path.starts_with("/api/") {
            if let Some(candidate) = http::query_param(query.as_deref(), "token") {
                if !token_matches(&self.token, Some(&candidate)) {
                    return http::text_response("Invalid Ruddr web token", StatusCode::UNAUTHORIZED);
                }
                let mut response = Response::new(Body::empty());
                *response.status_mut() = StatusCode::SEE_OTHER;
                let headers = response.headers_mut();
                headers.insert(header::LOCATION, HeaderValue::from_static("/"));
                headers.insert(header::SET_COOKIE, http::session_cookie(&self.token));
                headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                return response;
            }
            return assets::serve_static(&path).unwrap_or_else(|| http::text_response("Not found", StatusCode::NOT_FOUND));
        }
        let (parts, body) = request.into_parts();
        if path == "/api/login" && parts.method == Method::POST {
            if !http::is_same_origin_mutation(&parts.headers) {
                return failure("Cross-origin request rejected", StatusCode::FORBIDDEN);
            }
            let body = read_body(body).await;
            let candidate = body.get("token").and_then(Value::as_str).map(str::trim);
            if !token_matches(&self.token, candidate) {
                return failure("That token is not valid", StatusCode::UNAUTHORIZED);
            }
            let mut response = Response::new(Body::from(r#"{"ok":true}"#));
            let headers = response.headers_mut();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            headers.insert(header::SET_COOKIE, http::session_cookie(&self.token));
            return response;
        }
        if !http::is_authorized(&parts.headers, &self.token) {
            return failure("Unauthorized", StatusCode::UNAUTHORIZED);
        }
        if parts.method != Method::GET && !http::is_same_origin_mutation(&parts.headers) {
            return failure("Cross-origin request rejected", StatusCode::FORBIDDEN);
        }
        match self.route(&parts.method, &path, query.as_deref(), body).await {
            Ok(response) => response,
            Err(message) => failure(message, StatusCode::INTERNAL_SERVER_ERROR),
        }
    }

    async fn route(self: &Arc<Self>, method: &Method, path: &str, query: Option<&str>, body: Body) -> RouteResult {
        let param = |name: &str| http::query_param(query, name);
        if method == Method::GET {
            return match path {
                "/api/meta" => Ok(self.meta()),
                "/api/sessions" => {
                    self.refresh_sessions().await;
                    Ok(json_text(self.sessions_json(), StatusCode::OK))
                }
                "/api/sessions/stream" => Ok(self.session_stream()),
                "/api/run/events" => self.run_event_stream(param("dir").as_deref()).await,
                "/api/run/output" => self.run_output(param("dir").as_deref()).await,
                "/api/run/activity" => self.run_activity(param("dir").as_deref()).await,
                "/api/run/diff" => self.run_diff(param("dir").as_deref(), http::has_query_param(query, "force")).await,
                "/api/models" => Ok(self.models().await),
                "/api/deja" => self.deja(&param("q").unwrap_or_default()).await,
                "/api/dirs" => Ok(self.directories(param("path").unwrap_or_default()).await),
                _ => Ok(failure("Not found", StatusCode::NOT_FOUND)),
            };
        }
        if method != Method::POST {
            return Ok(failure("Not found", StatusCode::NOT_FOUND));
        }
        match path {
            "/api/prompt" => self.prompt(read_body(body).await).await,
            "/api/new" => self.new_session(read_body(body).await).await,
            "/api/stop" => self.stop(read_body(body).await).await,
            "/api/delete" => self.delete(read_body(body).await).await,
            "/api/theme" => self.theme(read_body(body).await).await,
            "/api/update" => self.update().await,
            _ => Ok(failure("Not found", StatusCode::NOT_FOUND)),
        }
    }

    fn meta(&self) -> Response {
        let args = self.args();
        let theme = assets::persisted_theme(&args.config_dir).filter(|name| assets::find_theme(name).is_some());
        let mut body = json!({
            "hostname": hostname(),
            "cwd": std::env::current_dir().unwrap_or_default().to_string_lossy(),
            "home": ruddr_core::paths::home_dir().to_string_lossy(),
            "providers": PROVIDERS,
            "theme": theme.as_deref().unwrap_or(assets::DEFAULT_THEME),
            "themes": assets::themes(),
            "dejaAvailable": which("deja").is_some(),
        });
        if let Some(update) = args.update_available {
            body["updateAvailable"] = Value::String(update);
        }
        json_response(&body, StatusCode::OK)
    }

    fn session_stream(self: &Arc<Self>) -> Response {
        let mut changes = self.changes.subscribe();
        self.ensure_polling();
        let app = self.clone();
        sse::event_stream(move |sender: SseSender| {
            let task = tokio::spawn(async move {
                app.refresh_sessions().await;
                changes.borrow_and_update();
                if !sender.send_serialized("sessions", &app.sessions_json()) {
                    return;
                }
                loop {
                    tokio::select! {
                        changed = changes.changed() => {
                            if changed.is_err() || !sender.send_serialized("sessions", &app.sessions_json()) {
                                return;
                            }
                        }
                        _ = sender.closed() => return,
                    }
                }
            });
            Box::new(move || task.abort())
        })
    }

    async fn run_event_stream(self: &Arc<Self>, state_dir: Option<&str>) -> RouteResult {
        let Some(session) = self.known_session(state_dir).await else {
            return Ok(failure("Unknown session", StatusCode::NOT_FOUND));
        };
        let Ok(directory) = self.verify_directory(&session) else {
            return Ok(failure("Unknown session", StatusCode::NOT_FOUND));
        };
        let events_path = directory.join(ruddr_core::state::EVENTS_FILE);
        let app = self.clone();
        Ok(sse::event_stream(move |sender: SseSender| {
            // TODO(review): Share one event-log reader per session if concurrent dashboard clients make per-client polling costly.
            let task = tokio::spawn(async move {
                let mut tail = Some(EventTail::default());
                loop {
                    let (app, session, path, mut state) =
                        (app.clone(), session.clone(), events_path.clone(), tail.take().unwrap_or_default());
                    let Ok((state, tick)) = tokio::task::spawn_blocking(move || {
                        let verify = || app.verify_directory(&session).is_ok();
                        let tick = state.tick(&path, &verify);
                        (state, tick)
                    })
                    .await
                    else {
                        sender.close();
                        return;
                    };
                    tail = Some(state);
                    match tick {
                        Tick::Close => {
                            sender.close();
                            return;
                        }
                        Tick::Events(events) => {
                            for event in events {
                                if !sender.send(event.name(), &event.data()) {
                                    return;
                                }
                            }
                        }
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(EVENTS_POLL) => {}
                        _ = sender.closed() => return,
                    }
                }
            });
            Box::new(move || task.abort())
        }))
    }

    async fn run_output(self: &Arc<Self>, state_dir: Option<&str>) -> RouteResult {
        let Some(session) = self.known_session(state_dir).await else {
            return Ok(failure("Unknown session", StatusCode::NOT_FOUND));
        };
        let directory = self.verify_directory(&session)?;
        let path = directory.join(ruddr_core::state::OUTPUT_FILE);
        let text = blocking(move || files::read_artifact_tail(&path, OUTPUT_TAIL_BYTES).map_err(|e| e.to_string())).await?;
        self.verify_directory(&session)?;
        Ok(json_response(&json!({ "text": text }), StatusCode::OK))
    }

    async fn run_activity(self: &Arc<Self>, state_dir: Option<&str>) -> RouteResult {
        let Some(session) = self.known_session(state_dir).await else {
            return Ok(failure("Unknown session", StatusCode::NOT_FOUND));
        };
        let directory = self.verify_directory(&session)?;
        let body = blocking(move || {
            let trace =
                files::read_artifact_tail(&directory.join(ruddr_core::state::TRACE_FILE), TRACE_TAIL_BYTES).map_err(|e| e.to_string())?;
            let events = files::read_artifact_tail(&directory.join(ruddr_core::state::EVENTS_FILE), ACTIVITY_EVENTS_TAIL_BYTES)
                .map_err(|e| e.to_string())?;
            Ok(crate::activity::activity_json(&trace, &events))
        })
        .await?;
        self.verify_directory(&session)?;
        Ok(json_response(&body, StatusCode::OK))
    }

    async fn run_diff(self: &Arc<Self>, state_dir: Option<&str>, force: bool) -> RouteResult {
        let Some(session) = self.known_session(state_dir).await else {
            return Ok(failure("Unknown session", StatusCode::NOT_FOUND));
        };
        let cwd = session.state.cwd.clone();
        if cwd.is_empty() {
            return Ok(json_response(
                &json!({ "content": "", "error": "This session has no working directory." }),
                StatusCode::OK,
            ));
        }
        let (result, branch, untracked) = tokio::join!(
            self.git.workspace_diff(&cwd, force),
            self.git.branch(&cwd),
            git::untracked_files(&cwd)
        );
        let mut paths: BTreeSet<String> = git::diff_paths(&result.content).into_iter().collect();
        paths.extend(untracked.iter().cloned());
        let started_at = session.state.started_at.clone();
        let cwd_path = PathBuf::from(&cwd);
        let touched = blocking(move || Ok(git::touched_since(&cwd_path, &paths, &started_at))).await?;
        self.verify_directory(&session)?;
        let mut body = json!({ "content": result.content });
        if let Some(error) = result.error {
            body["error"] = Value::String(error);
        }
        if let Some(branch) = branch {
            body["branch"] = Value::String(branch);
        }
        body["cwd"] = Value::String(cwd);
        body["untracked"] = json!(untracked);
        body["touched"] = json!(touched);
        Ok(json_response(&body, StatusCode::OK))
    }

    async fn models(&self) -> Response {
        let ruddr = self.args().ruddr;
        let catalog = match run_command(&ruddr, &["models", "--json"]).await {
            Ok(stdout) => parse_model_catalog(&stdout),
            Err(_) => None,
        };
        json_response(catalog.as_ref().unwrap_or(assets::fallback_models()), StatusCode::OK)
    }

    async fn deja(&self, query: &str) -> RouteResult {
        let terms: Vec<&str> = query.split_whitespace().collect();
        if terms.is_empty() {
            return Ok(json_response(&json!([]), StatusCode::OK));
        }
        let Some(deja) = which("deja") else {
            return Ok(failure("deja is not on PATH", StatusCode::NOT_FOUND));
        };
        let mut args = vec!["find"];
        args.extend(terms);
        args.extend(["--json", "--quiet"]);
        let output = tokio::process::Command::new(deja)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            let code = output.status.code().map_or("by signal".to_string(), |code| code.to_string());
            return Ok(failure(format!("deja find exited {code}"), StatusCode::INTERNAL_SERVER_ERROR));
        }
        Ok(json_response(
            &parse_deja_hits(&String::from_utf8_lossy(&output.stdout)),
            StatusCode::OK,
        ))
    }

    /// Directory completion for the new-session working directory field.
    async fn directories(&self, raw: String) -> Response {
        let body = blocking(move || Ok(list_directories(&raw)))
            .await
            .unwrap_or_else(|_| json!({ "entries": [] }));
        json_response(&body, StatusCode::OK)
    }

    async fn prompt(self: &Arc<Self>, input: Value) -> RouteResult {
        let message = http::str_field(&input, "message").map(str::trim).unwrap_or_default().to_string();
        if message.is_empty() {
            return Ok(failure("The prompt is empty", StatusCode::BAD_REQUEST));
        }
        self.refresh_sessions().await;
        let Some(session) = self.known_session(http::str_field(&input, "stateDir")).await else {
            return Ok(failure(
                "The prompt session is no longer available; the prompt was not sent",
                StatusCode::CONFLICT,
            ));
        };
        let state = &session.state;
        let route = prompt_route(state);
        // Never convert one route into another: a stale page must not turn a
        // steer into a new turn or a continuation.
        if route.is_none() || route != http::str_field(&input, "route") {
            return Ok(failure(
                format!("Session is now {}; the prompt was not sent", state.status),
                StatusCode::CONFLICT,
            ));
        }
        let turn = state.turn_id.clone().filter(|turn| !turn.is_empty());
        if route == Some("steer") && (turn.is_none() || turn.as_deref() != http::str_field(&input, "turnId")) {
            return Ok(failure("The active turn changed; the prompt was not sent", StatusCode::CONFLICT));
        }
        let state_dir = PathBuf::from(&state.state_dir);
        match route {
            Some("continue") => {
                let model = http::str_field(&input, "model").filter(|m| !m.is_empty()).map(str::to_string);
                let thread = state.thread_id.clone().unwrap_or_default();
                let cwd = PathBuf::from(&state.cwd);
                let source = state.clone();
                let started = self
                    .launch(cwd, message, move |prompt, dir| {
                        launch::continuation_run_arguments(&source, prompt, dir, model.as_deref())
                    })
                    .await?;
                let short: String = thread.chars().take(12).collect();
                Ok(json_response(
                    &json!({ "status": format!("Started a new run for thread {short}"), "stateDir": started }),
                    StatusCode::OK,
                ))
            }
            Some("steer") => {
                let expected = turn.unwrap_or_default();
                let request = control::Request {
                    command: Command::Steer,
                    text: Some(message),
                    expected_turn_id: Some(expected.clone()),
                };
                // Revalidate the live state the way `ruddr steer` does.
                let result = blocking(move || {
                    let live = ruddr_core::state::read_state(&state_dir).map_err(|e| e.message)?.displayed();
                    if live.status != Status::Active {
                        return Ok(Err(format!("Session is now {}; the prompt was not sent", live.status)));
                    }
                    if live.turn_id.as_deref() != Some(expected.as_str()) {
                        return Ok(Err("The active turn changed; the prompt was not sent".to_string()));
                    }
                    control::call(&state_dir, &request, STEER_TIMEOUT).map(Ok).map_err(|e| e.message)
                })
                .await?;
                self.refresh_sessions().await;
                match result {
                    Ok(live) => Ok(status_response(format!("steered turn {}", live.turn_id.unwrap_or_default()))),
                    Err(conflict) => Ok(failure(conflict, StatusCode::CONFLICT)),
                }
            }
            _ => {
                let request = control::Request {
                    command: Command::Prompt,
                    text: Some(message),
                    expected_turn_id: None,
                };
                let result = blocking(move || {
                    let live = ruddr_core::state::read_state(&state_dir).map_err(|e| e.message)?.displayed();
                    if live.status != Status::Idle {
                        return Ok(Err(format!("Session is now {}; the prompt was not sent", live.status)));
                    }
                    control::call(&state_dir, &request, PROMPT_TIMEOUT).map(Ok).map_err(|e| e.message)
                })
                .await?;
                self.refresh_sessions().await;
                match result {
                    Ok(live) => Ok(status_response(format!("started turn {}", live.turn_id.unwrap_or_default()))),
                    Err(conflict) => Ok(failure(conflict, StatusCode::CONFLICT)),
                }
            }
        }
    }

    async fn new_session(self: &Arc<Self>, input: Value) -> RouteResult {
        let message = http::str_field(&input, "message").map(str::trim).unwrap_or_default().to_string();
        if message.is_empty() {
            return Ok(failure("The first prompt is empty", StatusCode::BAD_REQUEST));
        }
        let provider = http::str_field(&input, "provider").unwrap_or("codex").to_string();
        if !PROVIDERS.contains(&provider.as_str()) {
            return Ok(failure(format!("Unknown provider {provider}"), StatusCode::BAD_REQUEST));
        }
        let raw_cwd = http::str_field(&input, "cwd")
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_string);
        let cwd = match raw_cwd {
            Some(raw) => absolute(&expand_home(&raw)),
            None => std::env::current_dir().unwrap_or_default(),
        };
        match std::fs::metadata(&cwd) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Ok(failure(format!("{} is not a directory", cwd.display()), StatusCode::BAD_REQUEST)),
            Err(_) => return Ok(failure(format!("{} does not exist", cwd.display()), StatusCode::BAD_REQUEST)),
        }
        let field = |name: &str| http::str_field(&input, name).filter(|v| !v.is_empty()).map(str::to_string);
        let (model, effort, resume) = (field("model"), field("effort"), field("resumeThreadId"));
        let launch_cwd = cwd.clone();
        let chosen = provider.clone();
        let started = self
            .launch(cwd, message, move |prompt, dir| {
                let options = NewSession {
                    provider: &chosen,
                    model: model.as_deref(),
                    effort: effort.as_deref(),
                    cwd: &launch_cwd,
                    resume_thread_id: resume.as_deref(),
                };
                Ok(launch::new_session_run_arguments(&options, prompt, dir))
            })
            .await?;
        Ok(json_response(
            &json!({ "status": format!("Started {provider} session"), "stateDir": started }),
            StatusCode::OK,
        ))
    }

    /// Starts a run and registers its directory for discovery.
    async fn launch(
        self: &Arc<Self>,
        cwd: PathBuf,
        message: String,
        arguments: impl FnOnce(&Path, &Path) -> ruddr_core::Result<Vec<String>> + Send + 'static,
    ) -> Result<String, String> {
        let app = self.clone();
        let ruddr = self.args().ruddr;
        let started = blocking(move || {
            launch::launch_session(
                &ruddr,
                &cwd,
                &message,
                arguments,
                |dir| app.args.lock().unwrap().state_dirs.push(dir.to_path_buf()),
                launch::STARTUP_WINDOW,
            )
            .map_err(|e| e.message)
        })
        .await?;
        self.refresh_sessions().await;
        Ok(started.to_string_lossy().into_owned())
    }

    async fn stop(self: &Arc<Self>, input: Value) -> RouteResult {
        self.refresh_sessions().await;
        let session = self.known_session(http::str_field(&input, "stateDir")).await;
        let Some(session) = session.filter(|s| matches!(s.state.status, Status::Active | Status::Idle)) else {
            return Ok(failure("Only an active or idle session can be stopped", StatusCode::CONFLICT));
        };
        let idle = session.state.status == Status::Idle;
        let state_dir = PathBuf::from(&session.state.state_dir);
        let status = blocking(move || {
            if idle {
                let request = control::Request {
                    command: Command::Stop,
                    text: None,
                    expected_turn_id: None,
                };
                control::call(&state_dir, &request, STOP_TIMEOUT).map_err(|e| e.message)?;
                return Ok("shutdown requested".to_string());
            }
            // Interrupt only the turn that is active now, as `ruddr interrupt` does.
            let live = ruddr_core::state::read_state(&state_dir).map_err(|e| e.message)?.displayed();
            if live.status != Status::Active {
                return Err(format!("turn is not active: status={}", live.status));
            }
            let turn = live.turn_id.unwrap_or_default();
            let request = control::Request {
                command: Command::Interrupt,
                text: None,
                expected_turn_id: Some(turn.clone()).filter(|t| !t.is_empty()),
            };
            control::call(&state_dir, &request, INTERRUPT_TIMEOUT).map_err(|e| e.message)?;
            Ok(format!("interrupt requested for turn {turn}"))
        })
        .await?;
        self.refresh_sessions().await;
        Ok(status_response(status))
    }

    async fn delete(self: &Arc<Self>, input: Value) -> RouteResult {
        self.refresh_sessions().await;
        let Some(session) = self.known_session(http::str_field(&input, "stateDir")).await else {
            return Ok(failure("Unknown session", StatusCode::NOT_FOUND));
        };
        let registries = self
            .args()
            .registries
            .unwrap_or_else(ruddr_core::paths::registry_dirs_for_discovery);
        let target = session.clone();
        let removed = blocking(move || delete_session_artifacts(&target, &registries)).await?;
        let state_dir = absolute(Path::new(&session.state.state_dir));
        self.args.lock().unwrap().state_dirs.retain(|dir| absolute(dir) != state_dir);
        self.refresh_sessions().await;
        Ok(status_response(if removed {
            "Session deleted"
        } else {
            "Removed the session from the registry"
        }))
    }

    async fn theme(&self, input: Value) -> RouteResult {
        let Some(theme) = http::str_field(&input, "name").and_then(assets::find_theme) else {
            return Ok(failure("Unknown theme", StatusCode::BAD_REQUEST));
        };
        let (name, label) = (
            theme["name"].as_str().unwrap_or_default().to_string(),
            theme["label"].as_str().unwrap_or_default().to_string(),
        );
        let config_dir = self.args().config_dir;
        blocking(move || assets::persist_theme(&config_dir, &name).map_err(|e| e.to_string())).await?;
        Ok(status_response(format!("Theme {label} saved")))
    }

    async fn update(&self) -> RouteResult {
        let (ruddr, target) = {
            let args = self.args.lock().unwrap();
            (args.ruddr.clone(), args.update_available.clone())
        };
        let Some(target) = target else {
            return Ok(failure("No newer release was found on the last daily check", StatusCode::CONFLICT));
        };
        run_command(&ruddr, &["update"]).await?;
        self.args.lock().unwrap().update_available = None;
        Ok(status_response(format!("Ruddr {target} installed; restart ruddr web to use it")))
    }
}

fn status_response(status: impl Into<String>) -> Response {
    json_response(&json!({ "status": status.into() }), StatusCode::OK)
}

async fn read_body(body: Body) -> Value {
    match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => http::parse_body(&bytes),
        Err(_) => Value::Object(Default::default()),
    }
}

/// Runs blocking work off the async threads.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(work).await.map_err(|e| e.to_string())?
}

/// Runs `ruddr ARGS` and returns stdout, or stderr as the error when it fails.
async fn run_command(program: &Path, args: &[&str]) -> Result<String, String> {
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let timeout = if args.first() == Some(&"update") {
        Duration::from_secs(600)
    } else {
        COMMAND_TIMEOUT
    };
    let output = match tokio::time::timeout(timeout, child).await {
        Ok(output) => output.map_err(|e| e.to_string())?,
        Err(_) => return Err(format!("ruddr {} did not finish in time", args[0])),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let code = output.status.code().unwrap_or(-1);
        return Err(if stderr.is_empty() {
            format!("ruddr {} exited {code}", args[0])
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Keeps catalog entries with a provider and either an id or
/// `available: false`. `None` means use the fallback catalog.
pub fn parse_model_catalog(text: &str) -> Option<Value> {
    let Value::Array(entries) = serde_json::from_str::<Value>(text).ok()? else {
        return None;
    };
    let valid: Vec<Value> = entries
        .into_iter()
        .filter(|model| {
            model.get("provider").is_some_and(Value::is_string)
                && (model.get("available") == Some(&Value::Bool(false)) || model.get("id").is_some_and(Value::is_string))
        })
        .collect();
    (!valid.is_empty()).then_some(Value::Array(valid))
}

/// `deja find --json` hits whose `resume` command names a Claude or Codex
/// session; the id doubles as `--resume-thread`.
pub fn parse_deja_hits(text: &str) -> Value {
    let Ok(parsed) = serde_json::from_str::<Value>(text) else {
        return json!([]);
    };
    let hits = parsed.get("hits").and_then(Value::as_array).cloned().unwrap_or_default();
    let string = |hit: &Value, name: &str| hit.get(name).and_then(Value::as_str).unwrap_or_default().to_string();
    let results: Vec<Value> = hits
        .iter()
        .filter_map(|hit| {
            let resume = string(hit, "resume");
            let (provider, id) = if let Some(id) = resume.strip_prefix("claude --resume ") {
                ("claude", id)
            } else {
                ("codex", resume.strip_prefix("codex resume ")?)
            };
            if id.is_empty() || id.contains(char::is_whitespace) {
                return None;
            }
            Some(json!({
                "provider": provider,
                "sessionId": id,
                "project": string(hit, "project"),
                "date": string(hit, "date"),
                "openingPrompt": string(hit, "openingPrompt"),
            }))
        })
        .collect();
    Value::Array(results)
}

/// `~` and `~/x` name the home directory, as in the TypeScript server.
fn expand_home(raw: &str) -> PathBuf {
    match raw.strip_prefix('~') {
        Some(rest) => ruddr_core::paths::home_dir().join(rest.trim_start_matches(['/', '\\'])),
        None => PathBuf::from(raw),
    }
}

/// Lists up to 40 visible subdirectories of the typed path, or of its parent
/// filtered by the typed last component.
pub fn list_directories(raw: &str) -> Value {
    let expanded = if raw.is_empty() {
        std::env::current_dir().unwrap_or_default()
    } else {
        expand_home(raw)
    };
    let target = absolute(&expanded);
    let (base, prefix) = if std::fs::metadata(&target).is_ok_and(|m| m.is_dir()) {
        (target, String::new())
    } else {
        let prefix = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        (target.parent().map(Path::to_path_buf).unwrap_or(target), prefix)
    };
    let mut entries: Vec<String> = match std::fs::read_dir(&base) {
        Ok(entries) => entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                (!name.starts_with('.') && name.starts_with(&prefix)).then(|| base.join(name).to_string_lossy().into_owned())
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    entries.sort();
    entries.truncate(DIRECTORY_LIMIT);
    json!({ "base": base.to_string_lossy(), "entries": entries })
}

/// Removes a finished or stale session's directory and registry entries.
/// Returns whether the directory itself was removed.
fn delete_session_artifacts(session: &Session, registries: &[PathBuf]) -> Result<bool, String> {
    let status = session.state.status;
    if !(status.is_terminal() || status == Status::Stale) {
        return Err(format!("Session is {status}; stop it before deleting"));
    }
    let state_dir = absolute(Path::new(&session.state.state_dir));
    let persisted: Option<Value> = std::fs::read(state_dir.join(ruddr_core::state::STATE_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let mut removed = false;
    if let Some(persisted) = persisted {
        let recorded = persisted.get("stateDir").and_then(Value::as_str).filter(|dir| !dir.is_empty());
        if recorded.is_some_and(|dir| absolute(Path::new(dir)) == state_dir) {
            let status = persisted.get("status").and_then(Value::as_str);
            let pid = persisted
                .get("pid")
                .and_then(|pid| pid.as_i64().or_else(|| pid.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)));
            let (Some(status), Some(pid)) = (status, pid) else {
                return Err("Cannot verify the session state; refresh before deleting".into());
            };
            let terminal = matches!(status, "completed" | "failed" | "interrupted");
            if !terminal && ruddr_core::process::alive(pid) {
                return Err(format!("Session is now {status}; stop it before deleting"));
            }
            match std::fs::remove_dir_all(&state_dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string()),
            }
            removed = true;
        }
    }
    ruddr_core::registry::unregister(&state_dir, registries);
    Ok(removed)
}

/// The first executable named `name` on PATH.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(str::to_string)
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for extension in &extensions {
            let candidate = dir.join(format!("{name}{extension}"));
            let Ok(metadata) = std::fs::metadata(&candidate) else { continue };
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = metadata.is_file();
            if executable {
                return Some(candidate);
            }
        }
    }
    None
}

pub fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buffer = [0u8; 256];
        // SAFETY: the buffer outlives the call and its length is passed.
        if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0 {
            let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
            return String::from_utf8_lossy(&buffer[..end]).into_owned();
        }
        String::new()
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_the_model_catalog() {
        assert_eq!(parse_model_catalog("not json"), None);
        assert_eq!(parse_model_catalog("[]"), None);
        let parsed = parse_model_catalog(r#"[{"provider":"codex","id":"a"},{"provider":"pi","available":false},{"id":"x"},null]"#).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 2);
    }

    #[test]
    fn parses_deja_hits() {
        let hits = parse_deja_hits(
            r#"{"hits":[{"resume":"claude --resume abc","project":"p","date":"d","openingPrompt":"o"},{"resume":"codex resume x1"},{"resume":"pi x"}]}"#,
        );
        assert_eq!(
            hits[0],
            json!({"provider":"claude","sessionId":"abc","project":"p","date":"d","openingPrompt":"o"})
        );
        assert_eq!(hits[1]["provider"], "codex");
        assert_eq!(hits.as_array().unwrap().len(), 2);
        assert_eq!(parse_deja_hits("nope"), json!([]));
    }

    #[test]
    fn lists_directories_by_prefix() {
        let root = std::env::temp_dir().join(format!("ruddr-web-dirs-{}", ruddr_core::fsutil::random_hex(4)));
        for name in ["alpha", "alpine", "beta", ".hidden"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        std::fs::write(root.join("alfile"), "").unwrap();
        let listed = list_directories(&root.join("al").to_string_lossy());
        assert_eq!(listed["base"], root.to_string_lossy().as_ref());
        let names: Vec<String> = listed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e.as_str().unwrap().rsplit('/').next().unwrap().to_string())
            .collect();
        assert_eq!(names, ["alpha", "alpine"]);
        assert_eq!(list_directories(&root.to_string_lossy())["entries"].as_array().unwrap().len(), 3);
        std::fs::remove_dir_all(root).unwrap();
    }
}
