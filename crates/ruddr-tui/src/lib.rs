//! The Ruddr terminal UI (ratatui). Entry point: [`tui_command`].

mod core;
mod text;
mod theme;
mod ui;

use crate::core::*;
use crate::theme::{themes, Palette};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::Rect;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Tab {
    Chat,
    Trace,
    Output,
    Diff,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Chat, Tab::Trace, Tab::Output, Tab::Diff];
    pub fn title(self) -> &'static str {
        match self {
            Tab::Chat => "chat",
            Tab::Trace => "activity",
            Tab::Output => "output",
            Tab::Diff => "diff",
        }
    }
    pub fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap()
    }
    fn next(self) -> Tab {
        Tab::ALL[(self.index() + 1) % 4]
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sessions,
    Artifact,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Classic,
    Beta,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Details {
    Compact,
    Full,
    Hidden,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Info,
    Success,
    Warning,
    Error,
}

pub struct Toast {
    pub text: String,
    pub kind: Kind,
    pub born: Instant,
}

impl Toast {
    pub fn lifetime(&self) -> Duration {
        match self.kind {
            Kind::Error | Kind::Warning => Duration::from_secs(8),
            _ => Duration::from_millis(4500),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    Prompt,
    New,
    Continue,
    Model,
    Find,
    Stop,
    StopNow,
    Tab(Tab),
    Fold,
    Search,
    Filter,
    Follow,
    Details,
    Sessions,
    Theme,
    Refresh,
    Copy,
    CopyText(String),
    Update,
    AskDelete(Vec<String>, String),
    Delete(Vec<String>),
    Palette,
    Help,
    Quit,
    Nothing,
}

#[derive(Clone)]
pub enum Action {
    Cmd(Cmd),
    Model(usize),
    Theme(usize),
    Deja(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Palette,
    Model,
    Theme,
    Deja,
    Menu,
    Confirm,
}

#[derive(Clone)]
pub struct PickItem {
    pub label: String,
    pub hint: String,
    pub key: String,
    pub disabled: Option<String>,
    pub danger: bool,
    pub action: Action,
    pub swatch: Option<Palette>,
    /// Model pickers cycle these with ←/→.
    pub efforts: Vec<String>,
}

impl PickItem {
    fn new(label: impl Into<String>, action: Action) -> PickItem {
        PickItem {
            label: label.into(),
            hint: String::new(),
            key: String::new(),
            disabled: None,
            danger: false,
            action,
            swatch: None,
            efforts: vec![],
        }
    }
    fn hint(mut self, hint: impl Into<String>) -> PickItem {
        self.hint = hint.into();
        self
    }
    fn key(mut self, key: impl Into<String>) -> PickItem {
        self.key = key.into();
        self
    }
    fn disabled_if(mut self, condition: bool, reason: &str) -> PickItem {
        if condition {
            self.disabled = Some(reason.into());
        }
        self
    }
}

pub struct Picker {
    pub kind: PickerKind,
    pub title: String,
    pub items: Vec<PickItem>,
    pub query: String,
    pub filterable: bool,
    pub index: usize,
    pub opened: Instant,
    pub anchor: Option<(u16, u16)>,
    pub effort: HashMap<usize, usize>,
    pub revert_theme: Option<usize>,
    pub sel_anim: f32,
}

impl Picker {
    fn new(kind: PickerKind, title: impl Into<String>, items: Vec<PickItem>, filterable: bool) -> Picker {
        let index = items.iter().position(|i| i.disabled.is_none()).unwrap_or(0);
        Picker {
            kind,
            title: title.into(),
            items,
            query: String::new(),
            filterable,
            index,
            opened: Instant::now(),
            anchor: None,
            effort: HashMap::new(),
            revert_theme: None,
            sel_anim: index as f32,
        }
    }

    /// Visible item indices, best match first.
    pub fn visible(&self) -> Vec<usize> {
        if !self.filterable || self.query.trim().is_empty() {
            return (0..self.items.len()).collect();
        }
        let mut scored: Vec<(u32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| palette_score(&item.label, &item.key, &item.hint, &self.query).map(|s| (s, i)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.into_iter().map(|(_, i)| i).collect()
    }

    pub fn selected(&self) -> Option<usize> {
        self.visible().get(self.index).copied()
    }

    fn move_by(&mut self, delta: isize) {
        let count = self.visible().len();
        if count > 0 {
            self.index = ((self.index as isize + delta).rem_euclid(count as isize)) as usize;
        }
    }

    pub fn effort_for(&self, item: usize) -> Option<&String> {
        let efforts = &self.items.get(item)?.efforts;
        efforts.get(*self.effort.get(&item)?)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Route(PromptRoute),
    New,
}

pub struct Prompt {
    pub kind: PromptKind,
    pub text: Vec<char>,
    pub cursor: usize,
    pub target: Option<Session>,
    pub provider: String,
    pub model: Option<ModelInfo>,
    pub effort: Option<String>,
    pub resume: Option<DejaHit>,
    pub opened: Instant,
    pub typed: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SearchTarget {
    Sessions,
    Artifact,
    Deja,
}

pub struct Search {
    pub target: SearchTarget,
    pub text: String,
}

pub enum Msg {
    Toast(String, Kind),
    Refresh,
    Models(Vec<ModelInfo>),
    Deja(Result<Vec<DejaHit>, String>),
    Updated(Result<String, String>),
    Branch(String, String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    Session(usize),
    Tab(Tab),
    Button(usize),
    PickItem(usize),
    TreeFile(usize),
    Artifact,
    Backdrop,
    Overlay,
}

/// Cached artifact sources for the selected session.
#[derive(Default)]
pub struct Sources {
    pub state_dir: String,
    pub sig: HashMap<&'static str, (u64, Option<SystemTime>)>,
    pub entries: Vec<ChatEntry>,
    pub trace: String,
    pub output: String,
    pub diff: Vec<DiffLine>,
    pub diff_files: Vec<DiffFile>,
    pub diff_raw: String,
    pub diff_error: Option<String>,
    pub diff_delay_ms: u64,
    pub diff_next: Option<Instant>,
    pub checked: Option<Instant>,
}

pub struct Args {
    pub ruddr: String,
    pub roots: Vec<PathBuf>,
    pub state_dirs: Vec<PathBuf>,
    pub interval: Duration,
    pub theme: Option<String>,
    pub beta: bool,
    pub mobile: bool,
    pub update: Option<String>,
}

fn parse_args(argv: Vec<String>) -> Result<Args, String> {
    let env = |names: &[&str]| names.iter().find_map(|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty()));
    let mut args = Args {
        ruddr: String::new(),
        roots: vec![],
        state_dirs: vec![],
        interval: Duration::from_millis(1000),
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
        let mut value = |name: &str| inline.clone().or_else(|| iter.next()).ok_or(format!("{name} requires a value"));
        match flag.as_str() {
            "--ruddr" => args.ruddr = value("--ruddr")?,
            "--root" => args.roots.push(value("--root")?.into()),
            "--state-dir" => args.state_dirs.push(value("--state-dir")?.into()),
            "--interval" => args.interval = parse_interval(&value("--interval")?)?,
            "--theme" => args.theme = Some(value("--theme")?),
            "--beta" => args.beta = true,
            "--mobile" => args.mobile = true,
            // --all is accepted for compatibility; every registered run is listed.
            "--all" | "--rs" => {}
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if args.ruddr.is_empty() {
        return Err("--ruddr is required (launch the TUI through ruddr tui --rs)".into());
    }
    if let Some(name) = &args.theme {
        if theme::find(name).is_none() {
            return Err(format!("unknown TUI theme {name}"));
        }
    }
    if args.roots.is_empty() {
        args.roots.push(PathBuf::from(".scratch"));
    }
    Ok(args)
}

fn parse_interval(value: &str) -> Result<Duration, String> {
    let ms = if let Some(ms) = value.strip_suffix("ms") {
        ms.parse::<f64>().ok()
    } else if let Some(s) = value.strip_suffix('s') {
        s.parse::<f64>().ok().map(|s| s * 1000.0)
    } else {
        None
    }
    .ok_or("--interval must use milliseconds or seconds, for example 500ms or 2s")?;
    if ms < 100.0 {
        return Err("--interval must be at least 100ms".into());
    }
    Ok(Duration::from_millis(ms as u64))
}

pub struct App {
    pub args: Args,
    pub started: Instant,
    pub sessions: Vec<Session>,
    pub selected: Option<String>,
    pub filter: String,
    pub tab: Tab,
    pub focus: Focus,
    pub launch_layout: Layout,
    pub mobile_threshold: u16,
    pub details: Details,
    pub theme: usize,
    pub tree_ratio: Option<f64>,
    pub tree_width: Option<u16>,
    pub models: Vec<ModelInfo>,
    pub pending_model: Option<(ModelInfo, Option<String>)>,
    pub deja_available: bool,
    pub deja_hits: Vec<DejaHit>,
    pub update: Option<String>,
    pub updating: bool,

    pub picker: Option<Picker>,
    pub prompt: Option<Prompt>,
    pub search: Option<Search>,
    pub help: bool,
    pub drawer: bool,
    pub drawer_anim: f32,
    pub splash: bool,

    pub artifact_query: HashMap<Tab, String>,
    pub folded: HashSet<String>,
    pub cursor: Option<usize>,
    pub reveal_cursor: bool,
    pub follow: bool,
    pub scroll_target: usize,
    pub scroll_pos: f32,
    pub unseen_base: Option<usize>,
    pub reveal: Option<(String, usize)>,
    pub sources: Sources,
    pub branches: HashMap<String, String>,

    pub list_offset: usize,
    pub sel_anim: f32,
    pub tab_anim: (f32, f32),
    pub meter_anim: f32,
    pub tab_settling: bool,
    pub seen: HashMap<String, (String, Instant)>,
    pub toasts: Vec<Toast>,
    pub stop_armed: Option<(String, Instant)>,
    pub bracket: Option<char>,

    pub hits: Vec<(Rect, Hit)>,
    pub artifact_rows: usize,
    pub artifact_height: usize,
    pub group_rows: Vec<(usize, usize)>,
    pub groups: Vec<String>,
    pub group_meta: Vec<GroupMeta>,
    pub buttons: Vec<Cmd>,
    pub mobile_now: bool,
    pub last_refresh: Instant,
    pub tx: Sender<Msg>,
    pub rx: Receiver<Msg>,
    pub quit: bool,
}

#[derive(Clone, Default)]
pub struct GroupMeta {
    pub diff_header: Option<String>,
    pub hunk: bool,
}

impl App {
    fn new(args: Args) -> Self {
        let (tx, rx) = channel();
        let config = theme::read_config();
        let theme = args.theme.as_deref().or(config.theme.as_deref()).and_then(theme::find).unwrap_or(0);
        let launch_layout = if args.beta { Layout::Beta } else { Layout::Classic };
        let mut app = App {
            update: args.update.clone(),
            args,
            started: Instant::now(),
            sessions: vec![],
            selected: None,
            filter: String::new(),
            tab: Tab::Chat,
            focus: Focus::Sessions,
            launch_layout,
            mobile_threshold: config.mobile_width_threshold.unwrap_or(64),
            details: Details::Full,
            theme,
            tree_ratio: config.diff_tree_ratio,
            tree_width: config.diff_tree_width,
            models: fallback_models(),
            pending_model: None,
            deja_available: which("deja"),
            deja_hits: vec![],
            updating: false,
            picker: None,
            prompt: None,
            search: None,
            help: false,
            drawer: false,
            drawer_anim: 0.0,
            splash: true,
            artifact_query: HashMap::new(),
            folded: HashSet::new(),
            cursor: None,
            reveal_cursor: false,
            follow: true,
            scroll_target: 0,
            scroll_pos: 0.0,
            unseen_base: None,
            reveal: None,
            sources: Sources::default(),
            branches: HashMap::new(),
            list_offset: 0,
            sel_anim: 0.0,
            tab_anim: (0.0, 0.0),
            meter_anim: 0.0,
            tab_settling: false,
            seen: HashMap::new(),
            toasts: vec![],
            stop_armed: None,
            bracket: None,
            hits: vec![],
            artifact_rows: 0,
            artifact_height: 0,
            group_rows: vec![],
            groups: vec![],
            group_meta: vec![],
            buttons: vec![],
            mobile_now: false,
            last_refresh: Instant::now(),
            tx,
            rx,
            quit: false,
        };
        if app.launch_layout == Layout::Beta {
            app.focus = Focus::Artifact;
        }
        app.refresh();
        // Mark everything present at launch as seen so only new runs flash.
        let epoch = Instant::now() - Duration::from_secs(60);
        for session in &app.sessions {
            app.seen.insert(session.state_dir.clone(), (session.status.clone(), epoch));
        }
        app.load_models();
        if let Some(version) = &app.update {
            let text = format!("Ruddr {version} is available · open the palette (:) and pick Update");
            app.toast(text, Kind::Info);
        }
        app
    }

    pub fn palette(&self) -> &Palette {
        &themes()[self.theme].palette
    }

    pub fn layout(&self) -> Layout {
        if self.mobile_now {
            Layout::Beta
        } else {
            self.launch_layout
        }
    }

    pub fn visible(&self) -> Vec<&Session> {
        let mut sessions = filter_sessions(&self.sessions, &self.filter);
        // Sessions named with --state-dir go first, in argument order.
        let explicit: Vec<PathBuf> = self.args.state_dirs.iter().map(|d| absolute(d)).collect();
        sessions.sort_by_key(|s| explicit.iter().position(|d| Path::new(&s.state_dir) == d).unwrap_or(usize::MAX));
        sessions
    }

    pub fn selected_index(&self) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        self.visible().iter().position(|s| &s.state_dir == selected)
    }

    pub fn current(&self) -> Option<&Session> {
        let selected = self.selected.as_ref()?;
        self.sessions.iter().find(|s| &s.state_dir == selected)
    }

    pub fn toast(&mut self, text: impl Into<String>, kind: Kind) {
        let text = text.into();
        self.toasts.retain(|t| t.text != text);
        self.toasts.push(Toast {
            text,
            kind,
            born: Instant::now(),
        });
        if self.toasts.len() > 3 {
            self.toasts.remove(0);
        }
    }

    fn refresh(&mut self) {
        self.sessions = discover(&self.args.state_dirs, &self.args.roots, &default_registry_dirs());
        let now = Instant::now();
        for session in &self.sessions {
            let entry = self.seen.entry(session.state_dir.clone()).or_insert((session.status.clone(), now));
            if entry.0 != session.status {
                *entry = (session.status.clone(), now);
            }
        }
        let visible: Vec<String> = self.visible().iter().map(|s| s.state_dir.clone()).collect();
        if self.selected.as_ref().is_none_or(|s| !visible.contains(s)) {
            self.selected = visible.first().cloned();
            self.reset_artifact();
        }
        if let Some(cwd) = self.current().and_then(|s| s.cwd.clone()) {
            if !self.branches.contains_key(&cwd) {
                self.branches.insert(cwd.clone(), String::new());
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    if let Ok(out) = Command::new("git")
                        .args(["-C", &cwd, "branch", "--show-current"])
                        .stderr(Stdio::null())
                        .output()
                    {
                        let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        let _ = tx.send(Msg::Branch(cwd, branch));
                    }
                });
            }
        }
        self.last_refresh = Instant::now();
    }

    fn load_models(&self) {
        let (ruddr, tx) = (self.args.ruddr.clone(), self.tx.clone());
        std::thread::spawn(move || {
            if let Ok(out) = Command::new(&ruddr)
                .args(["models", "--json"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
            {
                if out.status.success() {
                    let _ = tx.send(Msg::Models(parse_model_catalog(&String::from_utf8_lossy(&out.stdout))));
                }
            }
        });
    }

    pub fn reset_artifact(&mut self) {
        self.follow = self.tab != Tab::Diff;
        self.cursor = None;
        self.scroll_target = 0;
        self.scroll_pos = 0.0;
        self.unseen_base = None;
        self.sources.checked = None;
        self.sources.diff_next = None;
        self.reveal = None;
    }

    /// Re-reads artifact files when they change; git diff backs off while idle.
    pub fn poll_sources(&mut self) {
        let Some(session) = self.current().cloned() else {
            self.sources = Sources::default();
            return;
        };
        if self.sources.state_dir != session.state_dir {
            self.sources = Sources {
                state_dir: session.state_dir.clone(),
                diff_delay_ms: 1000,
                ..Default::default()
            };
        }
        if self.sources.checked.is_some_and(|t| t.elapsed() < Duration::from_millis(200)) {
            return;
        }
        self.sources.checked = Some(Instant::now());
        let sig = |path: &Option<String>| {
            path.as_deref()
                .and_then(|p| std::fs::metadata(p).ok())
                .map(|m| (m.len(), m.modified().ok()))
                .unwrap_or((0, None))
        };
        let chat_sig = sig(&session.events_path);
        if self.sources.sig.get("chat") != Some(&chat_sig) {
            self.sources.sig.insert("chat", chat_sig);
            let events = read_tail(session.events_path.as_deref(), 768 * 1024);
            let first_load = self.sources.entries.is_empty();
            self.sources.entries = parse_chat_transcript(&events, session.thread_id.as_deref());
            // History appears at once; only newly streamed text types out.
            if let Some(last) = self.sources.entries.iter().rev().find(|e| e.kind == EntryKind::Agent) {
                let id = last.item_id.clone().unwrap_or_default();
                let len = last.text.chars().count();
                match &self.reveal {
                    _ if first_load => self.reveal = Some((id, len)),
                    Some((rid, _)) if *rid == id => {}
                    _ => self.reveal = Some((id, 0)),
                }
            }
        }
        let trace_sig = sig(&session.trace_path);
        if self.sources.sig.get("trace") != Some(&trace_sig) {
            self.sources.sig.insert("trace", trace_sig);
            self.sources.trace = read_tail(session.trace_path.as_deref(), 256 * 1024);
        }
        let output_sig = sig(&session.output_path);
        if self.sources.sig.get("output") != Some(&output_sig) {
            self.sources.sig.insert("output", output_sig);
            self.sources.output = read_tail(session.output_path.as_deref(), 256 * 1024);
        }
        let due = self.sources.diff_next.is_none_or(|t| Instant::now() >= t);
        if self.tab == Tab::Diff && due {
            let (raw, error) = match session.cwd.as_deref() {
                None => (String::new(), Some("No working directory.".to_string())),
                Some(cwd) => match Command::new("git")
                    .args(["-C", cwd, "diff", "--no-color", "--no-ext-diff", "HEAD"])
                    .stdin(Stdio::null())
                    .output()
                {
                    Ok(o) if o.status.success() => (String::from_utf8_lossy(&o.stdout).into_owned(), None),
                    Ok(o) => (String::new(), Some(String::from_utf8_lossy(&o.stderr).trim().to_string())),
                    Err(e) => (String::new(), Some(format!("git: {e}"))),
                },
            };
            let changed = raw != self.sources.diff_raw || error != self.sources.diff_error;
            if changed {
                (self.sources.diff, self.sources.diff_files) = parse_git_diff(&raw);
                self.sources.diff_raw = raw;
                self.sources.diff_error = error;
            }
            self.sources.diff_delay_ms = next_diff_poll_ms(self.sources.diff_delay_ms, changed);
            self.sources.diff_next = Some(Instant::now() + Duration::from_millis(self.sources.diff_delay_ms));
        }
    }

    /// True while something on screen moves; drives the frame rate.
    pub fn animating(&self) -> bool {
        let live = self.sessions.iter().any(|s| matches!(s.status.as_str(), "active" | "starting"));
        let revealing = self.reveal.as_ref().is_some_and(|(id, n)| {
            self.sources
                .entries
                .iter()
                .rev()
                .find(|e| e.kind == EntryKind::Agent)
                .is_some_and(|e| e.item_id.as_deref().unwrap_or("") == id && e.text.chars().count() > *n)
        });
        let flashing = self.seen.values().any(|(_, t)| t.elapsed() < Duration::from_millis(1600));
        live || revealing
            || flashing
            || self.splash
            || !self.toasts.is_empty()
            || (self.scroll_pos - self.scroll_target as f32).abs() > 0.3
            || self
                .picker
                .as_ref()
                .is_some_and(|p| p.opened.elapsed() < Duration::from_millis(250) || (p.sel_anim - p.index as f32).abs() > 0.02)
            || self.prompt.is_some()
            || (self.drawer_anim - if self.drawer { 1.0 } else { 0.0 }).abs() > 0.01
            || self.stop_armed.is_some()
            || self.search.is_some()
            || self.selected_index().is_some_and(|i| (self.sel_anim - i as f32).abs() > 0.05)
            || self.tab_settling
    }

    pub fn tick_reveal(&mut self) {
        let Some((id, revealed)) = self.reveal.clone() else { return };
        let Some(last) = self.sources.entries.iter().rev().find(|e| e.kind == EntryKind::Agent) else {
            return;
        };
        if last.item_id.as_deref().unwrap_or("") != id {
            return;
        }
        let next = typewriter_reveal(revealed, last.text.chars().count(), 4);
        self.reveal = Some((id, next));
    }

    // --- selection ------------------------------------------------------

    fn select_offset(&mut self, delta: isize) {
        let visible: Vec<String> = self.visible().iter().map(|s| s.state_dir.clone()).collect();
        if visible.is_empty() {
            return;
        }
        let index = self.selected_index().unwrap_or(0);
        let next = (index as isize + delta).clamp(0, visible.len() as isize - 1) as usize;
        self.select(visible[next].clone());
    }

    pub fn select(&mut self, dir: String) {
        if self.selected.as_ref() != Some(&dir) {
            self.selected = Some(dir);
            self.stop_armed = None;
            self.reset_artifact();
            self.refresh();
        }
    }

    pub fn set_tab(&mut self, tab: Tab) {
        if self.tab != tab {
            self.tab = tab;
            self.reset_artifact();
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let count = self.groups.len();
        if count == 0 {
            return;
        }
        let present: Vec<usize> = {
            let mut seen = Vec::new();
            for &(group, _) in &self.group_rows {
                if seen.last() != Some(&group) {
                    seen.push(group);
                }
            }
            seen
        };
        if present.is_empty() {
            return;
        }
        let position = match self.cursor.and_then(|c| present.iter().position(|g| *g == c)) {
            Some(p) => (p as isize + delta).clamp(0, present.len() as isize - 1) as usize,
            None => {
                // Start from what is on screen.
                let top = self.scroll_target;
                present
                    .iter()
                    .position(|g| self.group_rows.iter().any(|&(gg, r)| gg == *g && r >= top))
                    .unwrap_or(0)
            }
        };
        self.cursor = Some(present[position]);
        self.follow = false;
        self.reveal_cursor = true;
    }

    fn jump(&mut self, forward: bool, hunk: bool) {
        let matches: Vec<usize> = self
            .group_meta
            .iter()
            .enumerate()
            .filter(|(_, m)| if hunk { m.hunk } else { m.diff_header.is_some() })
            .map(|(i, _)| i)
            .collect();
        let current = self.cursor.unwrap_or(0);
        let next = if forward {
            matches.iter().find(|i| **i > current || self.cursor.is_none())
        } else {
            matches.iter().rev().find(|i| **i < current)
        };
        if let Some(&target) = next {
            self.cursor = Some(target);
            self.follow = false;
            self.reveal_cursor = true;
        }
    }

    fn jump_match(&mut self, forward: bool) {
        let query = self.artifact_query.get(&self.tab).cloned().unwrap_or_default().to_lowercase();
        if query.is_empty() {
            return;
        }
        let hits: Vec<usize> = self
            .groups
            .iter()
            .enumerate()
            .filter(|(_, t)| t.to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect();
        if hits.is_empty() {
            return self.toast(format!("No matches for “{query}”"), Kind::Warning);
        }
        let current = self.cursor;
        let next = if forward {
            hits.iter().find(|i| current.is_none_or(|c| **i > c)).or(hits.first())
        } else {
            hits.iter().rev().find(|i| current.is_none_or(|c| **i < c)).or(hits.last())
        };
        let index = hits.iter().position(|h| Some(h) == next).unwrap_or(0);
        self.cursor = next.copied();
        self.follow = false;
        self.reveal_cursor = true;
        self.toast(format!("match {}/{}", index + 1, hits.len()), Kind::Info);
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.artifact_rows.saturating_sub(self.artifact_height);
        let current = if self.follow { max } else { self.scroll_target };
        let next = (current as isize + delta).clamp(0, max as isize) as usize;
        self.scroll_target = next;
        self.follow = next >= max && self.tab != Tab::Diff;
        if self.follow {
            self.cursor = None;
        }
    }

    fn toggle_fold(&mut self) {
        let Some(path) = self.cursor.and_then(|c| self.group_meta.get(c)).and_then(|m| m.diff_header.clone()) else {
            return;
        };
        if !self.folded.remove(&path) {
            self.folded.insert(path);
        }
    }

    // --- keys -----------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            if let Some(p) = &mut self.prompt {
                if !p.text.is_empty() {
                    p.text.clear();
                    p.cursor = 0;
                    return;
                }
            }
            self.quit = true;
            return;
        }
        if self.splash {
            self.splash = false;
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        if self.picker.is_some() {
            return self.on_picker_key(key);
        }
        if self.prompt.is_some() {
            return self.on_prompt_key(key);
        }
        if self.search.is_some() {
            return self.on_search_key(key);
        }
        if ctrl && key.code == KeyCode::Char('k') {
            return self.run(Cmd::Palette);
        }
        if ctrl {
            match key.code {
                KeyCode::Char('d') => self.scroll_by(self.artifact_height as isize / 2),
                KeyCode::Char('u') => self.scroll_by(-(self.artifact_height as isize / 2)),
                KeyCode::Char('n') => self.select_offset(1),
                KeyCode::Char('p') => self.select_offset(-1),
                _ => {}
            }
            return;
        }
        if let Some(bracket) = self.bracket.take() {
            let forward = bracket == ']';
            match key.code {
                KeyCode::Char('c') | KeyCode::Char('h') => return self.jump(forward, true),
                KeyCode::Char('f') => return self.jump(forward, false),
                _ => {}
            }
        }
        let drawer_open = self.layout() == Layout::Beta && self.drawer;
        let sessions_keys = self.focus == Focus::Sessions || drawer_open;
        let has_query = self.artifact_query.get(&self.tab).is_some_and(|q| !q.is_empty());
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char(':') => self.run(Cmd::Palette),
            KeyCode::Char('/') => self.run(if sessions_keys { Cmd::Filter } else { Cmd::Search }),
            KeyCode::Char('t') => self.run(Cmd::Theme),
            KeyCode::Char('m') => self.run(Cmd::Model),
            KeyCode::Char('f') => self.run(Cmd::Find),
            KeyCode::Char('r') => self.run(Cmd::Refresh),
            KeyCode::Char('R') => self.run(Cmd::Continue),
            KeyCode::Char('o') => self.run(Cmd::Tab(self.tab.next())),
            KeyCode::Char(c @ '1'..='4') => self.run(Cmd::Tab(Tab::ALL[(c as u8 - b'1') as usize])),
            KeyCode::Char('s') => self.run(Cmd::Prompt),
            KeyCode::Char('n') if has_query && !sessions_keys => self.jump_match(true),
            KeyCode::Char('N') if has_query && !sessions_keys => self.jump_match(false),
            KeyCode::Char('n') => self.run(Cmd::New),
            KeyCode::Char('x') => self.run(Cmd::Stop),
            KeyCode::Char('i') => self.run(Cmd::Details),
            KeyCode::Char('c') => self.run(Cmd::Copy),
            KeyCode::Char('Z') => self.run(Cmd::Fold),
            KeyCode::Char('D') => self.ask_delete_selected(),
            KeyCode::Char(']') | KeyCode::Char('[') => {
                if let KeyCode::Char(c) = key.code {
                    self.bracket = Some(c);
                }
            }
            KeyCode::Tab | KeyCode::BackTab => self.run(Cmd::Sessions),
            KeyCode::Esc => {
                if drawer_open {
                    self.drawer = false;
                    self.focus = Focus::Artifact;
                } else if self.cursor.is_some() {
                    self.cursor = None;
                } else if has_query {
                    self.artifact_query.remove(&self.tab);
                } else if !self.filter.is_empty() {
                    self.filter.clear();
                    self.refresh();
                } else if self.focus == Focus::Sessions && self.layout() == Layout::Classic {
                    self.focus = Focus::Artifact;
                }
            }
            KeyCode::Char('G') | KeyCode::End => self.run(Cmd::Follow),
            KeyCode::Char('g') | KeyCode::Home => {
                self.follow = false;
                self.scroll_target = 0;
                self.cursor = None;
            }
            KeyCode::PageDown => self.scroll_by(self.artifact_height as isize - 2),
            KeyCode::PageUp => self.scroll_by(-(self.artifact_height as isize - 2)),
            KeyCode::Enter => {
                if drawer_open {
                    self.drawer = false;
                    self.focus = Focus::Artifact;
                } else if self.focus == Focus::Artifact
                    && self
                        .cursor
                        .and_then(|c| self.group_meta.get(c))
                        .is_some_and(|m| m.diff_header.is_some())
                {
                    self.toggle_fold();
                } else {
                    self.run(Cmd::Prompt);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if sessions_keys {
                    self.select_offset(1)
                } else {
                    self.move_cursor(1)
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if sessions_keys {
                    self.select_offset(-1)
                } else {
                    self.move_cursor(-1)
                }
            }
            KeyCode::Left | KeyCode::Char('h') if !sessions_keys => self.run(Cmd::Tab(Tab::ALL[(self.tab.index() + 3) % 4])),
            KeyCode::Right | KeyCode::Char('l') if !sessions_keys => self.run(Cmd::Tab(self.tab.next())),
            _ => {}
        }
    }

    fn on_search_key(&mut self, key: KeyEvent) {
        let Some(search) = &mut self.search else { return };
        let target = search.target;
        match key.code {
            KeyCode::Esc => {
                match target {
                    SearchTarget::Sessions => self.filter.clear(),
                    SearchTarget::Artifact => {
                        self.artifact_query.remove(&self.tab);
                    }
                    SearchTarget::Deja => {}
                }
                self.search = None;
                self.refresh();
                return;
            }
            KeyCode::Enter => {
                let text = search.text.clone();
                self.search = None;
                match target {
                    SearchTarget::Deja => self.run_deja(text),
                    SearchTarget::Artifact => {
                        self.cursor = None;
                        self.jump_match(true)
                    }
                    SearchTarget::Sessions => {}
                }
                return;
            }
            KeyCode::Backspace => {
                search.text.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => search.text.clear(),
            KeyCode::Char(c) => search.text.push(c),
            _ => return,
        }
        let text = search.text.clone();
        match target {
            SearchTarget::Sessions => {
                self.filter = text;
                self.refresh();
            }
            SearchTarget::Artifact => {
                self.artifact_query.insert(self.tab, text);
            }
            SearchTarget::Deja => {}
        }
    }

    fn on_picker_key(&mut self, key: KeyEvent) {
        let Some(picker) = &mut self.picker else { return };
        match key.code {
            KeyCode::Esc => {
                if let Some(theme) = picker.revert_theme {
                    self.theme = theme;
                }
                self.picker = None;
                return;
            }
            KeyCode::Enter => return self.commit_picker(),
            KeyCode::Up => picker.move_by(-1),
            KeyCode::Down | KeyCode::Tab => picker.move_by(1),
            KeyCode::BackTab => picker.move_by(-1),
            KeyCode::PageDown => picker.move_by(8),
            KeyCode::PageUp => picker.move_by(-8),
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => picker.move_by(-1),
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => picker.move_by(1),
            KeyCode::Left | KeyCode::Right if picker.kind == PickerKind::Model => {
                if let Some(item) = picker.selected() {
                    let count = picker.items[item].efforts.len();
                    if count > 0 {
                        let current = picker.effort.get(&item).map(|e| *e as isize).unwrap_or(-1);
                        let delta = if key.code == KeyCode::Right { 1 } else { -1 };
                        // -1 means the provider default effort.
                        let next = (current + delta + 1).rem_euclid(count as isize + 1) - 1;
                        if next < 0 {
                            picker.effort.remove(&item);
                        } else {
                            picker.effort.insert(item, next as usize);
                        }
                    }
                }
            }
            KeyCode::Char('j') if !picker.filterable => picker.move_by(1),
            KeyCode::Char('k') if !picker.filterable => picker.move_by(-1),
            KeyCode::Char('y') if picker.kind == PickerKind::Confirm => {
                picker.index = 0;
                return self.commit_picker();
            }
            KeyCode::Char('n') if picker.kind == PickerKind::Confirm => {
                self.picker = None;
                return;
            }
            KeyCode::Backspace if picker.filterable => {
                picker.query.pop();
                picker.index = 0;
            }
            KeyCode::Char(c) if picker.filterable => {
                picker.query.push(c);
                picker.index = 0;
            }
            _ => {}
        }
        self.preview_theme();
    }

    fn preview_theme(&mut self) {
        if let Some(picker) = &self.picker {
            if picker.kind == PickerKind::Theme {
                if let Some(Action::Theme(index)) = picker.selected().map(|i| picker.items[i].action.clone()) {
                    self.theme = index;
                }
            }
        }
    }

    fn commit_picker(&mut self) {
        let Some(picker) = self.picker.take() else { return };
        let Some(index) = picker.selected() else { return };
        let item = picker.items[index].clone();
        if let Some(reason) = &item.disabled {
            self.toast(reason.clone(), Kind::Warning);
            self.picker = Some(picker);
            return;
        }
        match item.action {
            Action::Cmd(cmd) => self.run(cmd),
            Action::Theme(theme) => {
                self.theme = theme;
                let name = &themes()[theme];
                match theme::persist_theme(&name.name) {
                    Ok(()) => self.toast(format!("Theme {} saved", name.label), Kind::Success),
                    Err(e) => self.toast(format!("Could not save theme: {e}"), Kind::Error),
                }
            }
            Action::Model(model) => {
                let info = self.models[model].clone();
                let effort = picker.effort_for(index).cloned();
                let label = format!("{}{}", info.name(), effort.as_ref().map(|e| format!(" · {e}")).unwrap_or_default());
                if let Some(prompt) = &mut self.prompt {
                    prompt.provider = info.provider.clone();
                    prompt.model = Some(info);
                    prompt.effort = effort;
                } else {
                    self.pending_model = Some((info, effort));
                }
                self.toast(format!("Model: {label}"), Kind::Info);
            }
            Action::Deja(hit) => {
                let hit = self.deja_hits[hit].clone();
                self.open_new_prompt(Some(hit));
            }
        }
    }

    fn on_prompt_key(&mut self, key: KeyEvent) {
        let Some(prompt) = &mut self.prompt else { return };
        prompt.typed = Instant::now();
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.prompt = None,
            KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => prompt.insert('\n'),
            KeyCode::Char('j') if ctrl => prompt.insert('\n'),
            KeyCode::Enter => self.submit(),
            KeyCode::Tab
                if prompt.kind != PromptKind::Route(PromptRoute::Steer) && prompt.kind != PromptKind::Route(PromptRoute::Prompt) =>
            {
                self.run(Cmd::Model)
            }
            KeyCode::Backspace if alt || ctrl => prompt.delete_word(),
            KeyCode::Char('w') if ctrl => prompt.delete_word(),
            KeyCode::Char('u') if ctrl => {
                prompt.text.drain(..prompt.cursor);
                prompt.cursor = 0;
            }
            KeyCode::Char('a') if ctrl => prompt.cursor = prompt.line_start(),
            KeyCode::Char('e') if ctrl => prompt.cursor = prompt.line_end(),
            KeyCode::Backspace => {
                if prompt.cursor > 0 {
                    prompt.cursor -= 1;
                    prompt.text.remove(prompt.cursor);
                }
            }
            KeyCode::Delete => {
                if prompt.cursor < prompt.text.len() {
                    prompt.text.remove(prompt.cursor);
                }
            }
            KeyCode::Left if alt || ctrl => prompt.word_left(),
            KeyCode::Right if alt || ctrl => prompt.word_right(),
            KeyCode::Left => prompt.cursor = prompt.cursor.saturating_sub(1),
            KeyCode::Right => prompt.cursor = (prompt.cursor + 1).min(prompt.text.len()),
            KeyCode::Home => prompt.cursor = prompt.line_start(),
            KeyCode::End => prompt.cursor = prompt.line_end(),
            KeyCode::Up => prompt.vertical(-1),
            KeyCode::Down => prompt.vertical(1),
            KeyCode::Char(c) => prompt.insert(c),
            _ => {}
        }
    }

    fn on_paste(&mut self, text: String) {
        let clean = text.replace("\r\n", "\n").replace('\r', "\n");
        if let Some(prompt) = &mut self.prompt {
            for c in clean.chars() {
                prompt.insert(c);
            }
        } else if let Some(search) = &mut self.search {
            search.text.push_str(clean.lines().next().unwrap_or(""));
        } else if let Some(picker) = &mut self.picker {
            if picker.filterable {
                picker.query.push_str(clean.lines().next().unwrap_or(""));
            }
        }
    }

    // --- mouse ----------------------------------------------------------

    fn hit_at(&self, x: u16, y: u16) -> Option<Hit> {
        self.hits
            .iter()
            .rev()
            .find(|(r, _)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
            .map(|(_, h)| *h)
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        let hit = self.hit_at(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let down = mouse.kind == MouseEventKind::ScrollDown;
                if let Some(picker) = &mut self.picker {
                    picker.move_by(if down { 1 } else { -1 });
                    self.preview_theme();
                } else if matches!(hit, Some(Hit::Session(_))) {
                    self.select_offset(if down { 1 } else { -1 });
                } else {
                    self.scroll_by(if down { 3 } else { -3 });
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.splash {
                    self.splash = false;
                    return;
                }
                match hit {
                    Some(Hit::PickItem(position)) => {
                        if let Some(picker) = &mut self.picker {
                            picker.index = position;
                            self.commit_picker();
                        }
                    }
                    Some(Hit::Overlay) => {}
                    _ if self.picker.is_some() => {
                        if let Some(theme) = self.picker.as_ref().and_then(|p| p.revert_theme) {
                            self.theme = theme;
                        }
                        self.picker = None;
                    }
                    _ if self.help => self.help = false,
                    Some(Hit::Session(index)) => {
                        if let Some(dir) = self.visible().get(index).map(|s| s.state_dir.clone()) {
                            self.select(dir);
                        }
                        if self.layout() == Layout::Beta {
                            self.drawer = false;
                            self.focus = Focus::Artifact;
                        } else {
                            self.focus = Focus::Sessions;
                        }
                    }
                    Some(Hit::Tab(tab)) => self.run(Cmd::Tab(tab)),
                    Some(Hit::Button(index)) => {
                        if let Some(cmd) = self.buttons.get(index).cloned() {
                            self.run(cmd);
                        }
                    }
                    Some(Hit::TreeFile(file)) => {
                        if let Some(path) = self.sources.diff_files.get(file).map(|f| f.path.clone()) {
                            if let Some(group) = self.group_meta.iter().position(|m| m.diff_header.as_deref() == Some(&path)) {
                                self.cursor = Some(group);
                                self.follow = false;
                                self.reveal_cursor = true;
                                self.focus = Focus::Artifact;
                            }
                        }
                    }
                    Some(Hit::Backdrop) => {
                        self.drawer = false;
                        self.focus = Focus::Artifact;
                    }
                    Some(Hit::Artifact) => self.focus = Focus::Artifact,
                    _ => {}
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if let Some(Hit::Session(index)) = hit {
                    if let Some(dir) = self.visible().get(index).map(|s| s.state_dir.clone()) {
                        self.select(dir);
                        self.open_session_menu(Some((mouse.column, mouse.row)));
                    }
                }
            }
            _ => {}
        }
    }

    // --- commands -------------------------------------------------------

    pub fn run(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Nothing => {}
            Cmd::Quit => self.quit = true,
            Cmd::Help => self.help = true,
            Cmd::Palette => self.open_palette(),
            Cmd::Prompt => self.open_prompt(None),
            Cmd::Continue => self.open_prompt(Some(PromptRoute::Continue)),
            Cmd::New => {
                self.open_new_prompt(None);
                self.run(Cmd::Model);
            }
            Cmd::Model => self.open_model_picker(),
            Cmd::Find => {
                if !self.deja_available {
                    self.toast("deja is not on PATH; install it to resume past sessions", Kind::Warning);
                } else {
                    self.search = Some(Search {
                        target: SearchTarget::Deja,
                        text: String::new(),
                    });
                }
            }
            Cmd::Stop => self.request_stop(false),
            Cmd::StopNow => self.request_stop(true),
            Cmd::Tab(tab) => {
                self.set_tab(tab);
                if self.layout() == Layout::Classic || !self.drawer {
                    self.focus = Focus::Artifact;
                }
            }
            Cmd::Fold => {
                if self.tab != Tab::Diff {
                    return self.toast("Folding works in the diff tab", Kind::Warning);
                }
                let paths: Vec<String> = self.sources.diff_files.iter().map(|f| f.path.clone()).collect();
                if paths.iter().all(|p| self.folded.contains(p)) {
                    self.folded.clear();
                } else {
                    self.folded.extend(paths);
                }
            }
            Cmd::Search => {
                self.focus = Focus::Artifact;
                let text = self.artifact_query.get(&self.tab).cloned().unwrap_or_default();
                self.search = Some(Search {
                    target: SearchTarget::Artifact,
                    text,
                });
            }
            Cmd::Filter => {
                self.focus = Focus::Sessions;
                if self.layout() == Layout::Beta {
                    self.drawer = true;
                }
                self.search = Some(Search {
                    target: SearchTarget::Sessions,
                    text: self.filter.clone(),
                });
            }
            Cmd::Follow => {
                self.follow = true;
                self.cursor = None;
                self.unseen_base = None;
            }
            Cmd::Details => {
                self.details = match self.details {
                    Details::Hidden => Details::Compact,
                    Details::Compact => Details::Full,
                    Details::Full => Details::Hidden,
                }
            }
            Cmd::Sessions => {
                if self.layout() == Layout::Beta {
                    self.drawer = !self.drawer;
                    self.focus = if self.drawer { Focus::Sessions } else { Focus::Artifact };
                } else {
                    self.focus = if self.focus == Focus::Sessions {
                        Focus::Artifact
                    } else {
                        Focus::Sessions
                    };
                }
            }
            Cmd::Theme => self.open_theme_picker(),
            Cmd::Refresh => {
                self.refresh();
                self.sources.checked = None;
                self.sources.diff_next = None;
                self.toast("Sessions refreshed", Kind::Info);
            }
            Cmd::Copy => {
                let text = match self.cursor {
                    Some(group) => self.groups.get(group).cloned(),
                    None => self.groups.last().cloned(),
                };
                match text {
                    Some(text) if !text.is_empty() => self.run(Cmd::CopyText(text)),
                    _ => self.toast("Nothing to copy", Kind::Warning),
                }
            }
            Cmd::CopyText(text) => {
                copy_osc52(&text);
                let preview: String = text.chars().take(40).collect::<String>().replace('\n', " ");
                self.toast(
                    format!("Copied “{preview}{}”", if text.chars().count() > 40 { "…" } else { "" }),
                    Kind::Success,
                );
            }
            Cmd::Update => self.run_update(),
            Cmd::AskDelete(dirs, question) => {
                let count = dirs.len();
                let mut items = vec![
                    PickItem::new(
                        if count == 1 {
                            "Yes, delete it".to_string()
                        } else {
                            format!("Yes, delete {count} sessions")
                        },
                        Action::Cmd(Cmd::Delete(dirs)),
                    )
                    .key("y"),
                    PickItem::new("Cancel", Action::Cmd(Cmd::Nothing)).key("n"),
                ];
                items[0].danger = true;
                let mut picker = Picker::new(PickerKind::Confirm, question, items, false);
                picker.index = 1;
                picker.sel_anim = 1.0;
                self.picker = Some(picker);
            }
            Cmd::Delete(dirs) => {
                let registries = default_registry_dirs();
                let (mut ok, mut failed) = (0, None);
                for dir in &dirs {
                    if let Some(session) = self.sessions.iter().find(|s| &s.state_dir == dir) {
                        match delete_session(session, &registries) {
                            Ok(()) => ok += 1,
                            Err(e) => failed = Some(e),
                        }
                    }
                }
                self.refresh();
                match failed {
                    Some(e) => self.toast(e, Kind::Error),
                    None => self.toast(format!("Deleted {ok} session{}", if ok == 1 { "" } else { "s" }), Kind::Success),
                }
            }
        }
    }

    fn open_palette(&mut self) {
        let session = self.current().cloned();
        let route = session.as_ref().and_then(prompt_route);
        let stoppable = session.as_ref().is_some_and(|s| matches!(s.status.as_str(), "active" | "idle"));
        let broken: Vec<String> = self
            .sessions
            .iter()
            .filter(|s| matches!(s.status.as_str(), "failed" | "stale"))
            .map(|s| s.state_dir.clone())
            .collect();
        let cmd = |label: &str, key: &str, cmd: Cmd| PickItem::new(label, Action::Cmd(cmd)).key(key);
        let mut items = vec![
            cmd("Send a prompt", "s", Cmd::Prompt)
                .hint("steer, prompt, or continue the selected session")
                .disabled_if(route.is_none(), "no promptable session selected"),
            cmd("New session", "n", Cmd::New).hint("pick a provider and model, then type the first prompt"),
            cmd("Continue thread in a new run", "R", Cmd::Continue)
                .hint("finished sessions only")
                .disabled_if(route != Some(PromptRoute::Continue), "select a finished session with a thread"),
            cmd("Choose model", "m", Cmd::Model),
            cmd("Find a past session", "f", Cmd::Find)
                .hint("deja search")
                .disabled_if(!self.deja_available, "deja is not on PATH"),
            cmd(
                if session.as_ref().is_some_and(|s| s.status == "idle") {
                    "End idle session"
                } else {
                    "Interrupt turn"
                },
                "x x",
                Cmd::StopNow,
            )
            .disabled_if(!stoppable, "no active or idle session"),
            cmd("Show chat", "1", Cmd::Tab(Tab::Chat)),
            cmd("Show activity", "2", Cmd::Tab(Tab::Trace)),
            cmd("Show output", "3", Cmd::Tab(Tab::Output)),
            cmd("Show diff", "4", Cmd::Tab(Tab::Diff)).hint("tracked changes against HEAD"),
            cmd("Fold or unfold every diff file", "Z", Cmd::Fold).disabled_if(self.tab != Tab::Diff, "diff tab only"),
            cmd("Search this pane", "/", Cmd::Search),
            cmd("Filter sessions", "/", Cmd::Filter).hint("project, thread, status, or model"),
            cmd("Resume live follow", "End", Cmd::Follow).disabled_if(self.follow, "already following"),
            cmd("Cycle session details", "i", Cmd::Details),
            cmd(
                if self.layout() == Layout::Beta {
                    "Open sessions"
                } else {
                    "Focus sessions"
                },
                "Tab",
                Cmd::Sessions,
            ),
            cmd("Change theme", "t", Cmd::Theme).hint("live preview"),
            cmd("Refresh sessions", "r", Cmd::Refresh),
            cmd("Copy selected row", "c", Cmd::Copy),
            cmd(
                "Delete selected session",
                "D",
                Cmd::AskDelete(session.iter().map(|s| s.state_dir.clone()).collect(), "Delete this session?".into()),
            )
            .disabled_if(!session.as_ref().is_some_and(deletable), "only finished or stale sessions"),
            cmd(
                &format!("Delete {} failed or stale sessions", broken.len()),
                "",
                Cmd::AskDelete(broken.clone(), format!("Delete {} failed or stale sessions?", broken.len())),
            )
            .disabled_if(broken.is_empty(), "none to delete"),
            cmd(
                &self
                    .update
                    .as_ref()
                    .map(|v| format!("Update Ruddr to {v}"))
                    .unwrap_or("Update Ruddr".into()),
                "",
                Cmd::Update,
            )
            .hint("runs ruddr update; restart the TUI afterwards")
            .disabled_if(
                self.update.is_none() || self.updating,
                "no newer release found on the last daily check",
            ),
            cmd("Keyboard help", "?", Cmd::Help),
            cmd("Quit", "q", Cmd::Quit),
        ];
        for item in &mut items {
            item.danger = item.label.starts_with("Delete");
        }
        self.picker = Some(Picker::new(PickerKind::Palette, "commands", items, true));
    }

    fn open_session_menu(&mut self, anchor: Option<(u16, u16)>) {
        let Some(session) = self.current().cloned() else { return };
        let route = prompt_route(&session);
        let mut items = vec![
            PickItem::new("Send a prompt", Action::Cmd(Cmd::Prompt))
                .key("s")
                .disabled_if(route.is_none(), "cannot take a prompt"),
            PickItem::new("Continue in a new run", Action::Cmd(Cmd::Continue))
                .key("R")
                .disabled_if(route != Some(PromptRoute::Continue), "finished sessions only"),
            PickItem::new(
                if session.status == "idle" {
                    "End idle session"
                } else {
                    "Interrupt turn"
                },
                Action::Cmd(Cmd::StopNow),
            )
            .disabled_if(!matches!(session.status.as_str(), "active" | "idle"), "not running"),
            PickItem::new("Copy state directory", Action::Cmd(Cmd::CopyText(session.state_dir.clone()))),
        ];
        if let Some(thread) = &session.thread_id {
            items.push(PickItem::new("Copy thread id", Action::Cmd(Cmd::CopyText(thread.clone()))));
        }
        if let Some(cwd) = &session.cwd {
            items.push(PickItem::new("Copy working directory", Action::Cmd(Cmd::CopyText(cwd.clone()))));
        }
        let mut delete = PickItem::new(
            "Delete session",
            Action::Cmd(Cmd::AskDelete(
                vec![session.state_dir.clone()],
                format!("Delete {}?", project_name(&session)),
            )),
        )
        .key("D")
        .disabled_if(!deletable(&session), "stop it before deleting");
        delete.danger = true;
        items.push(delete);
        let filtered: Vec<String> = self
            .visible()
            .iter()
            .filter(|s| deletable(s))
            .map(|s| s.state_dir.clone())
            .collect();
        if !self.filter.trim().is_empty() && !filtered.is_empty() {
            let mut all = PickItem::new(
                format!("Delete {} matching sessions", filtered.len()),
                Action::Cmd(Cmd::AskDelete(
                    filtered.clone(),
                    format!("Delete {} sessions matching “{}”?", filtered.len(), self.filter.trim()),
                )),
            );
            all.danger = true;
            items.push(all);
        }
        let mut picker = Picker::new(PickerKind::Menu, project_name(&session), items, false);
        picker.anchor = anchor;
        self.picker = Some(picker);
    }

    fn ask_delete_selected(&mut self) {
        let Some(session) = self.current().cloned() else { return };
        if !deletable(&session) {
            return self.toast(format!("Session is {}; stop it before deleting", session.status), Kind::Warning);
        }
        self.run(Cmd::AskDelete(
            vec![session.state_dir.clone()],
            format!("Delete {}?", project_name(&session)),
        ));
    }

    fn open_theme_picker(&mut self) {
        let items = themes()
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mut item = PickItem::new(t.label.clone(), Action::Theme(i)).hint(t.source.clone());
                item.key = if i == self.theme { "current".into() } else { String::new() };
                item.swatch = Some(t.palette);
                item
            })
            .collect();
        let mut picker = Picker::new(PickerKind::Theme, "theme · live preview", items, true);
        picker.index = self.theme;
        picker.sel_anim = self.theme as f32;
        picker.revert_theme = Some(self.theme);
        self.picker = Some(picker);
    }

    fn open_model_picker(&mut self) {
        // A prompt for an existing thread can only switch models within its provider.
        let provider = match &self.prompt {
            Some(p) if p.kind == PromptKind::Route(PromptRoute::Continue) => Some(p.provider.clone()),
            Some(p) if p.resume.is_some() => Some(p.provider.clone()),
            _ => None,
        };
        let current = self
            .prompt
            .as_ref()
            .and_then(|p| p.model.clone())
            .or_else(|| self.pending_model.as_ref().map(|m| m.0.clone()))
            .or_else(|| {
                let target = self.prompt.as_ref()?.target.as_ref()?;
                self.models
                    .iter()
                    .find(|m| m.provider == target.provider() && m.id == target.model)
                    .cloned()
            });
        let mut items = Vec::new();
        let mut select = None;
        for (i, model) in self.models.iter().enumerate() {
            if provider.as_ref().is_some_and(|p| p != &model.provider) {
                continue;
            }
            let mut item = PickItem::new(
                if model.available {
                    format!("{}{}", model.name(), if model.default { "  ★" } else { "" })
                } else {
                    model.provider.clone()
                },
                Action::Model(i),
            )
            .hint(if model.available {
                model.provider.clone()
            } else {
                model.note.clone().unwrap_or("unavailable".into())
            })
            .disabled_if(
                !model.available,
                &format!("{} is {}", model.provider, model.note.clone().unwrap_or("unavailable".into())),
            );
            item.efforts = model.efforts.clone();
            if current.as_ref() == Some(model) || (current.is_none() && select.is_none() && model.default) {
                select = Some(items.len());
            }
            items.push(item);
        }
        let mut picker = Picker::new(PickerKind::Model, "model · ←/→ effort", items, true);
        if let Some(index) = select {
            picker.index = index;
            picker.sel_anim = index as f32;
        }
        self.picker = Some(picker);
    }

    fn open_prompt(&mut self, wanted: Option<PromptRoute>) {
        let Some(session) = self.current().cloned() else {
            return self.toast("No session selected · press n to start one", Kind::Warning);
        };
        let Some(route) = prompt_route(&session) else {
            return self.toast(format!("A {} session cannot take a prompt", session.status), Kind::Warning);
        };
        if wanted.is_some_and(|w| w != route) {
            return self.toast("Continue works on finished sessions with a thread", Kind::Warning);
        }
        let (model, effort) = match (&self.pending_model, route) {
            (Some((m, e)), PromptRoute::Continue) if m.provider == session.provider() => (Some(m.clone()), e.clone()),
            _ => (None, None),
        };
        self.prompt = Some(Prompt {
            kind: PromptKind::Route(route),
            text: vec![],
            cursor: 0,
            provider: session.provider().to_string(),
            target: Some(session),
            model,
            effort,
            resume: None,
            opened: Instant::now(),
            typed: Instant::now(),
        });
    }

    fn open_new_prompt(&mut self, resume: Option<DejaHit>) {
        let (model, effort) = match &self.pending_model {
            Some((m, e)) if resume.as_ref().is_none_or(|r| r.provider == m.provider) => (Some(m.clone()), e.clone()),
            _ => (None, None),
        };
        let provider = resume
            .as_ref()
            .map(|r| r.provider.clone())
            .or_else(|| model.as_ref().map(|m| m.provider.clone()))
            .or_else(|| self.current().map(|s| s.provider().to_string()))
            .unwrap_or_else(|| "codex".into());
        self.prompt = Some(Prompt {
            kind: PromptKind::New,
            text: vec![],
            cursor: 0,
            target: None,
            provider,
            model,
            effort,
            resume,
            opened: Instant::now(),
            typed: Instant::now(),
        });
    }

    fn submit(&mut self) {
        let Some(prompt) = self.prompt.take() else { return };
        let message: String = prompt.text.iter().collect::<String>().trim().to_string();
        if message.is_empty() {
            self.prompt = Some(prompt);
            return;
        }
        let ruddr = self.args.ruddr.clone();
        let tx = self.tx.clone();
        let overrides = LaunchOverrides {
            model: prompt.model.as_ref().and_then(|m| m.id.clone()),
            effort: prompt.effort.clone(),
        };
        if prompt.kind == PromptKind::New {
            let cwd = std::env::current_dir().unwrap_or_default().to_string_lossy().into_owned();
            let provider = prompt.provider.clone();
            let resume = prompt.resume.clone();
            self.toast(format!("Starting {provider} session…"), Kind::Info);
            std::thread::spawn(move || {
                let result = launch(&ruddr, &cwd, &message, |p, d| {
                    new_session_args(&provider, &cwd, p, d, &overrides, resume.as_ref().map(|r| r.session_id.as_str()))
                });
                send_result(&tx, result.map(|d| format!("Started {}", short_path(&d))));
            });
            self.pending_model = None;
            return;
        }
        // Re-resolve the target from fresh state so a turn that ended while
        // typing never turns into a different route.
        let target = prompt.target.clone().unwrap();
        self.refresh();
        let Some(session) = self.sessions.iter().find(|s| s.state_dir == target.state_dir).cloned() else {
            return self.toast("The session disappeared", Kind::Error);
        };
        let route = prompt_route(&session);
        if route.is_none()
            || PromptKind::Route(route.unwrap()) != prompt.kind
            || (route == Some(PromptRoute::Steer) && session.turn_id != target.turn_id)
        {
            self.prompt = Some(prompt);
            return self.toast("The session changed state; prompt kept, not sent", Kind::Warning);
        }
        self.follow = true;
        self.cursor = None;
        self.toast(
            match route.unwrap() {
                PromptRoute::Steer => "Steering…",
                PromptRoute::Prompt => "Sending prompt…",
                PromptRoute::Continue => "Starting continuation…",
            },
            Kind::Info,
        );
        std::thread::spawn(move || {
            let result = match route.unwrap() {
                PromptRoute::Steer => run_with_stdin(&ruddr, &steer_args(&session, "-"), &message).map(|_| "Steer delivered".to_string()),
                PromptRoute::Prompt => {
                    run_with_stdin(&ruddr, &idle_prompt_args(&session, "-"), &message).map(|_| "Prompt sent".to_string())
                }
                PromptRoute::Continue => {
                    let cwd = session.cwd.clone().unwrap_or_default();
                    launch(&ruddr, &cwd, &message, |p, d| continuation_args(&session, p, d, &overrides))
                        .map(|d| format!("Continued in {}", short_path(&d)))
                }
            };
            send_result(&tx, result);
        });
        self.pending_model = None;
    }

    fn request_stop(&mut self, now: bool) {
        let Some(session) = self.current().cloned() else { return };
        if !matches!(session.status.as_str(), "active" | "idle") {
            return self.toast("Only an active or idle session can be stopped", Kind::Warning);
        }
        let armed = self
            .stop_armed
            .as_ref()
            .is_some_and(|(dir, at)| *dir == session.state_dir && at.elapsed() < Duration::from_secs(2));
        if !now && !armed {
            self.stop_armed = Some((session.state_dir.clone(), Instant::now()));
            return;
        }
        self.stop_armed = None;
        let idle = session.status == "idle";
        self.toast(if idle { "Ending idle session…" } else { "Interrupting turn…" }, Kind::Warning);
        let (ruddr, tx) = (self.args.ruddr.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let args = [
                if idle { "stop" } else { "interrupt" }.to_string(),
                "--state-dir".into(),
                session.state_dir.clone(),
            ];
            send_result(
                &tx,
                run_with_stdin(&ruddr, &args, "").map(|out| {
                    if out.is_empty() {
                        if idle {
                            "Shutdown requested".into()
                        } else {
                            "Interrupt requested".into()
                        }
                    } else {
                        out
                    }
                }),
            );
        });
    }

    fn run_deja(&mut self, terms: String) {
        if terms.trim().is_empty() {
            return;
        }
        self.toast(format!("Searching past sessions for “{}”…", terms.trim()), Kind::Info);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut args = vec!["find".to_string()];
            args.extend(terms.split_whitespace().map(String::from));
            args.extend(["--json".into(), "--quiet".into()]);
            let result = Command::new("deja").args(&args).stdin(Stdio::null()).stderr(Stdio::null()).output();
            let _ = tx.send(Msg::Deja(match result {
                Ok(out) if out.status.success() => Ok(parse_deja_hits(&String::from_utf8_lossy(&out.stdout))),
                Ok(out) => Err(format!("deja find exited {}", out.status.code().unwrap_or(-1))),
                Err(e) => Err(e.to_string()),
            }));
        });
    }

    fn run_update(&mut self) {
        if self.updating || self.update.is_none() {
            return;
        }
        self.updating = true;
        self.toast(
            format!("Updating Ruddr to {}…", self.update.clone().unwrap_or_default()),
            Kind::Info,
        );
        let (ruddr, tx) = (self.args.ruddr.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Updated(run_with_stdin(&ruddr, &["update".into()], "")));
        });
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Toast(text, kind) => {
                self.toast(text, kind);
                self.refresh();
            }
            Msg::Refresh => self.refresh(),
            Msg::Models(models) => self.models = models,
            Msg::Branch(cwd, branch) => {
                self.branches.insert(cwd, branch);
            }
            Msg::Deja(Err(e)) => self.toast(e, Kind::Error),
            Msg::Deja(Ok(hits)) if hits.is_empty() => self.toast("No resumable sessions matched", Kind::Warning),
            Msg::Deja(Ok(hits)) => {
                let items = hits
                    .iter()
                    .enumerate()
                    .map(|(i, h)| {
                        let prompt: String = h.opening_prompt.chars().take(110).collect();
                        PickItem::new(
                            format!("{}  {}", h.project.rsplit('/').next().unwrap_or(&h.project), h.date),
                            Action::Deja(i),
                        )
                        .hint(if prompt.is_empty() { h.session_id.clone() } else { prompt })
                        .key(h.provider.clone())
                    })
                    .collect();
                self.toast(format!("{} resumable sessions found", hits.len()), Kind::Success);
                self.deja_hits = hits;
                self.picker = Some(Picker::new(PickerKind::Deja, "resume a past session", items, true));
            }
            Msg::Updated(result) => {
                self.updating = false;
                match result {
                    Ok(_) => {
                        let version = self.update.take().unwrap_or_default();
                        self.toast(format!("Ruddr {version} installed · quit and relaunch ruddr tui"), Kind::Success);
                    }
                    Err(e) => self.toast(format!("Update failed: {e}"), Kind::Error),
                }
            }
        }
    }
}

impl Prompt {
    fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += 1;
    }
    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .iter()
            .rposition(|c| *c == '\n')
            .map(|i| i + 1)
            .unwrap_or(0)
    }
    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .iter()
            .position(|c| *c == '\n')
            .map(|i| self.cursor + i)
            .unwrap_or(self.text.len())
    }
    fn word_left(&mut self) {
        while self.cursor > 0 && self.text[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        while self.cursor > 0 && !self.text[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
    }
    fn word_right(&mut self) {
        while self.cursor < self.text.len() && self.text[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
        while self.cursor < self.text.len() && !self.text[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
    }
    fn delete_word(&mut self) {
        let end = self.cursor;
        self.word_left();
        self.text.drain(self.cursor..end);
    }
    fn vertical(&mut self, delta: isize) {
        let column = self.cursor - self.line_start();
        if delta < 0 {
            let start = self.line_start();
            if start == 0 {
                self.cursor = 0;
                return;
            }
            self.cursor = start - 1;
            let previous = self.line_start();
            self.cursor = (previous + column).min(start - 1);
        } else {
            let end = self.line_end();
            if end == self.text.len() {
                self.cursor = end;
                return;
            }
            self.cursor = end + 1;
            let next_end = self.line_end();
            self.cursor = (end + 1 + column).min(next_end);
        }
    }
}

fn send_result(tx: &Sender<Msg>, result: Result<String, String>) {
    let _ = tx.send(match result {
        Ok(text) => Msg::Toast(text, Kind::Success),
        Err(text) => Msg::Toast(text, Kind::Error),
    });
}

fn short_path(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    }
}

fn which(binary: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(binary).is_file()))
}

fn copy_osc52(text: &str) {
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", text::base64(text.as_bytes()));
    let _ = out.flush();
}

fn run_with_stdin(ruddr: &str, args: &[String], input: &str) -> Result<String, String> {
    let mut child = Command::new(ruddr)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn ruddr: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            format!("ruddr exited with {}", output.status)
        } else {
            stderr
        })
    }
}

/// Creates a private state directory under CWD/.scratch/ruddr-tui and starts
/// a detached `ruddr run`, so the session outlives this TUI.
fn launch(ruddr: &str, cwd: &str, message: &str, build: impl FnOnce(&str, &str) -> Vec<String>) -> Result<String, String> {
    let base = Path::new(cwd).join(".scratch").join("ruddr-tui");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&base)
        .map_err(|e| e.to_string())?;
    let _ = std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700));
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let dir = base.join(format!("{}-{:08x}", now_seconds(), nanos ^ std::process::id()));
    std::fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|e| e.to_string())?;
    let private = |path: &Path| std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path);
    let prompt = dir.join("prompt.md");
    private(&prompt)
        .and_then(|mut f| writeln!(f, "{message}"))
        .map_err(|e| e.to_string())?;
    let stderr = private(&dir.join("launch.stderr.log")).map_err(|e| e.to_string())?;
    let (prompt_s, dir_s) = (prompt.to_string_lossy().into_owned(), dir.to_string_lossy().into_owned());
    let status = Command::new(ruddr)
        .args(build(&prompt_s, &dir_s))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .status()
        .map_err(|e| format!("spawn ruddr: {e}"))?;
    if !status.success() {
        let log = read_tail(Some(&dir.join("launch.stderr.log").to_string_lossy()), 4096);
        return Err(if log.is_empty() {
            format!("Session exited during startup; see {dir_s}")
        } else {
            log
        });
    }
    Ok(dir_s)
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> std::io::Result<()> {
    let mut last_frame = Instant::now();
    while !app.quit {
        while let Ok(msg) = app.rx.try_recv() {
            app.on_msg(msg);
        }
        if app.splash && app.started.elapsed() > Duration::from_millis(1100) {
            app.splash = false;
        }
        app.toasts.retain(|t| t.born.elapsed() < t.lifetime());
        if app.stop_armed.as_ref().is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(2)) {
            app.stop_armed = None;
        }
        app.poll_sources();
        app.tick_reveal();
        terminal.draw(|frame| ui::draw(frame, app))?;
        let frame_budget = if app.animating() {
            Duration::from_millis(33)
        } else {
            Duration::from_millis(250)
        };
        let timeout = frame_budget
            .saturating_sub(last_frame.elapsed())
            .min(app.args.interval.saturating_sub(app.last_refresh.elapsed()));
        if event::poll(timeout)? {
            // Drain everything queued so a burst of keys renders once.
            loop {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => app.on_key(key),
                    Event::Mouse(mouse) => app.on_mouse(mouse),
                    Event::Paste(text) => app.on_paste(text),
                    _ => {}
                }
                if app.quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        if last_frame.elapsed() >= frame_budget {
            last_frame = Instant::now();
        }
        if app.last_refresh.elapsed() >= app.args.interval {
            app.refresh();
        }
    }
    Ok(())
}

/// Entry point for `ruddr tui ARGS...`. The CLI passes `--ruddr` with its
/// own executable path until the TUI calls the core crates directly.
pub fn tui_command(argv: Vec<String>) -> ruddr_core::Result<()> {
    let args = parse_args(argv).map_err(ruddr_core::Error::usage)?;
    let mut app = App::new(args);
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    let result = run(&mut terminal, &mut app);
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    result.map_err(|error| ruddr_core::Error::failed(error.to_string()))
}
