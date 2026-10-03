//! TUI state and input handling. Rendering lives in `ui.rs`; the event loop
//! in `lib.rs`. Background work reports back as [`Msg`] values.

use crate::actions;
use crate::activity::{self, Activities, Activity};
use crate::cache::RenderCache;
use crate::core::*;
use crate::tail::{Batch, Source, Tailer};
use crate::theme::{self, Palette, themes};
use crate::transcript::{DRAIN_TICK, ToolDetail, Transcript};
use crate::view::{self, Block};
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ruddr_core::state::Status;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

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
    pub fn next(self) -> Tab {
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
    ChangeDir,
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
    History,
    EditsOnly,
    Theme,
    Refresh,
    Copy,
    CopyText(String),
    Activate,
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
    pub fn new(label: impl Into<String>, action: Action) -> PickItem {
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
    pub fn hint(mut self, hint: impl Into<String>) -> PickItem {
        self.hint = hint.into();
        self
    }
    pub fn key(mut self, key: impl Into<String>) -> PickItem {
        self.key = key.into();
        self
    }
    pub fn disabled_if(mut self, condition: bool, reason: &str) -> PickItem {
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
    pub fn new(kind: PickerKind, title: impl Into<String>, items: Vec<PickItem>, filterable: bool) -> Picker {
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
    /// Absolute paths of the images sent with this prompt.
    pub images: Vec<PathBuf>,
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
    Input(Event),
    Tail(Batch),
    Toast(String, Kind),
    /// A launch spawned its controller: list and select the new run.
    Spawned(String),
    /// A prompt was not sent; the draft comes back.
    Bounced {
        state_dir: String,
        text: String,
        images: Vec<PathBuf>,
        error: String,
    },
    /// A clipboard image saved for the open prompt, or why none was.
    Attached(Result<PathBuf, String>),
    Models(Vec<ModelInfo>),
    Deja(Result<Vec<DejaHit>, String>),
    Updated(Result<String, String>),
    Branch(String, String),
    Diff {
        state_dir: String,
        result: Result<String, String>,
        touched: Vec<String>,
        /// Why the diff shows the run's recorded edits instead of `git diff`.
        recorded: Option<String>,
    },
    /// The newest sessions from every agent's history.
    History(Vec<ruddr_history::SessionInfo>),
    /// A past session found for a deja hit, to show in the history list.
    Found(Result<ruddr_history::SessionInfo, String>),
    /// Lines added and removed by history sessions, keyed by state directory
    /// with the `updated_ms` they were read at; `done` ends a scan.
    HistoryEdits {
        stats: Vec<(String, i64, u32, u32)>,
        done: bool,
    },
    /// A history session read on a background thread.
    HistoryLoaded {
        generation: u64,
        state_dir: String,
        loaded: Arc<LoadedHistory>,
    },
}

/// A history session read once: its chat lines, its assistant messages, and
/// its edits as a unified diff.
#[derive(Debug)]
pub struct LoadedHistory {
    pub chat: Vec<String>,
    pub output: Vec<String>,
    pub diff: Result<String, String>,
}

/// The history sessions read most recently, newest first, so moving back to
/// one shows it without another read.
#[derive(Debug, Default)]
pub struct HistoryCache(VecDeque<(String, Arc<LoadedHistory>)>);

impl HistoryCache {
    const CAPACITY: usize = 32;

    pub fn get(&mut self, state_dir: &str) -> Option<Arc<LoadedHistory>> {
        let index = self.0.iter().position(|(dir, _)| dir == state_dir)?;
        let entry = self.0.remove(index)?;
        let loaded = entry.1.clone();
        self.0.push_front(entry);
        Some(loaded)
    }

    pub fn insert(&mut self, state_dir: String, loaded: Arc<LoadedHistory>) {
        self.0.retain(|(dir, _)| *dir != state_dir);
        self.0.push_front((state_dir, loaded));
        self.0.truncate(Self::CAPACITY);
    }
}

/// How long the screen keeps the previous frame while a history session
/// loads. Most loads finish well inside it, so switching sessions never
/// draws an empty pane first.
pub const LOAD_HOLD: Duration = Duration::from_millis(150);

/// The session list shows every agent's history instead of Ruddr's runs.
#[derive(Debug, Default)]
pub struct HistoryMode {
    /// State directory (`history:<locator>`) -> the session it names.
    pub infos: HashMap<String, ruddr_history::SessionInfo>,
    pub runs: Vec<Session>,
    pub loading: bool,
    pub loaded_at: Option<Instant>,
    /// The run selected when the history opened, selected again on close.
    pub runs_selected: Option<String>,
    /// A session opened from a deja search, kept in the list even when it
    /// is older than the newest sessions the list loads.
    pub pinned: Option<ruddr_history::SessionInfo>,
    /// State directory -> (`updated_ms` when read, lines added, lines
    /// removed) of the session's file edits.
    pub edits: HashMap<String, (i64, u32, u32)>,
    pub scanning: bool,
    /// The list shows only sessions whose edits are known and non-empty.
    pub edits_only: bool,
}

impl HistoryMode {
    /// Lines added and removed by a session, once its scan has read it.
    pub fn edit_stat(&self, state_dir: &str) -> Option<(u32, u32)> {
        self.edits.get(state_dir).map(|&(_, added, removed)| (added, removed))
    }

    fn has_edits(&self, state_dir: &str) -> bool {
        self.edit_stat(state_dir).is_some_and(|(added, removed)| added + removed > 0)
    }

    fn keep_pinned(&mut self) {
        let Some(info) = &self.pinned else { return };
        let dir = format!("{}{}", crate::history::PREFIX, info.locator);
        if !self.infos.contains_key(&dir) {
            self.runs.push(crate::history::run_state(info));
            self.infos.insert(dir, info.clone());
        }
    }
}

/// How often an open history list re-reads the stores.
const HISTORY_REFRESH: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    Session(usize),
    Tab(Tab),
    Button(usize),
    PickItem(usize),
    /// A row of the diff file tree, by index into `tree_entries`.
    TreeRow(usize),
    /// The tree's right border, which drags to resize the sidebar.
    TreeDivider,
    /// The session list's right border, which drags to resize the list.
    SessionsDivider,
    Artifact,
    Backdrop,
    Overlay,
}

/// output.md split into markdown paragraphs. Fenced code keeps its blank
/// lines inside one paragraph.
#[derive(Default)]
pub struct OutputDoc {
    pub paragraphs: Vec<(u64, u64, String)>,
    in_fence: bool,
    clock: u64,
    pub generation: u64,
}

impl OutputDoc {
    pub fn push_line(&mut self, line: &str) {
        self.clock += 1;
        self.generation += 1;
        if line.trim_start().starts_with("```") {
            self.in_fence = !self.in_fence;
        }
        let blank = line.trim().is_empty();
        if blank && !self.in_fence {
            if self.paragraphs.last().is_some_and(|p| !p.2.is_empty()) {
                self.paragraphs.push((self.clock, self.clock, String::new()));
            }
            return;
        }
        if self.paragraphs.is_empty() {
            self.paragraphs.push((self.clock, self.clock, String::new()));
        }
        let last = self.paragraphs.last_mut().unwrap();
        if !last.2.is_empty() {
            last.2.push('\n');
        }
        last.2.push_str(line);
        last.1 = self.clock;
        if self.paragraphs.len() > 2000 {
            self.paragraphs.drain(..self.paragraphs.len() - 2000);
        }
    }
}

#[derive(Default)]
pub struct DiffState {
    pub lines: Vec<DiffLine>,
    pub files: Vec<DiffFile>,
    pub raw: String,
    pub error: Option<String>,
    pub touched: HashSet<String>,
    pub generation: u64,
    pub delay_ms: u64,
    pub next: Option<Instant>,
    pub pending: bool,
    pub loaded: bool,
    /// Width of the line-number gutter.
    pub gutter: usize,
    /// Set when the diff comes from the run's recorded edits: why Git could
    /// not describe the working directory.
    pub recorded: Option<String>,
}

/// Everything read for the selected session.
#[derive(Default)]
pub struct Sources {
    /// state dir, thread id, events path: a change starts over.
    pub scope: (String, Option<String>, PathBuf),
    /// Batches from an older reader or history load are dropped.
    pub generation: u64,
    pub tailer: Option<Tailer>,
    pub transcript: Transcript,
    pub activities: Activities,
    pub output: OutputDoc,
    pub loaded: HashSet<Source>,
    pub diff: DiffState,
    pub tools: Vec<ToolDetail>,
    pub tools_seen: u64,
    pub activity_view: Vec<Activity>,
    pub activity_details: Vec<Option<usize>>,
    pub activity_key: (u64, u64),
}

#[derive(Debug)]
pub struct Args {
    pub roots: Vec<PathBuf>,
    pub state_dirs: Vec<PathBuf>,
    pub interval: Duration,
    pub theme: Option<String>,
    pub beta: bool,
    pub mobile: bool,
    pub update: Option<String>,
}

pub struct App {
    pub args: Args,
    pub exe: PathBuf,
    /// Where new sessions start; `/cd` in the new-session prompt moves it.
    pub launch_cwd: PathBuf,
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
    pub history: Option<HistoryMode>,
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
    pub collapsed_dirs: HashSet<String>,
    pub tree_entries: Vec<TreeEntry>,
    /// The artifact area the diff tree splits, for divider drags.
    pub diff_area: Rect,
    pub dragging_tree: bool,
    pub sessions_ratio: Option<f64>,
    pub sessions_width: Option<u16>,
    /// The classic layout's body, which the session list splits.
    pub body_area: Rect,
    pub dragging_sessions: bool,
    pub expanded: HashSet<u64>,
    pub cursor: Option<usize>,
    pub reveal_cursor: bool,
    pub follow: bool,
    pub scroll_target: usize,
    pub scroll_pos: f32,
    pub unseen_base: Option<usize>,
    pub sources: Sources,
    pub cache: RenderCache,
    pub tail_generation: u64,
    pub history_cache: HistoryCache,
    /// When the oldest history load still pending started; draws wait for
    /// it up to `LOAD_HOLD`.
    pub loading_since: Option<Instant>,
    pub branches: HashMap<String, String>,

    pub list_offset: usize,
    pub sel_anim: f32,
    pub tab_anim: (f32, f32),
    pub meter_anim: f32,
    pub tab_settling: bool,
    pub seen: HashMap<String, (Status, Instant)>,
    pub toasts: Vec<Toast>,
    pub stop_armed: Option<(String, Instant)>,
    pub bracket: Option<char>,
    pub busy: bool,

    /// Layout from the last frame, for mouse hits and paging.
    pub hits: Vec<(Rect, Hit)>,
    pub blocks: Vec<Block>,
    /// First row and row count of each block.
    pub block_spans: Vec<(usize, usize)>,
    /// The block shown on each artifact screen row.
    pub screen_blocks: Vec<Option<usize>>,
    pub artifact_inner: Rect,
    pub artifact_rows: usize,
    pub artifact_height: usize,
    pub buttons: Vec<Cmd>,
    pub mobile_now: bool,
    pub match_count: Option<(u64, String, usize)>,

    pub last_refresh: Instant,
    pub last_drain: Instant,
    /// The soonest moment something visible needs a new frame.
    pub next_frame: Option<Instant>,
    pub dirty: bool,
    pub frames: u64,
    pub show_frames: bool,
    pub tx: Sender<Msg>,
    pub rx: Receiver<Msg>,
    pub quit: bool,
}

/// The narrowest session list that still shows a name and an age.
pub const SESSIONS_MIN: u16 = 24;

/// The session list leaves at least 60 columns for the chat and diff.
pub fn sessions_max(body: u16) -> u16 {
    body.saturating_sub(60).max(SESSIONS_MIN)
}

/// The list width before the user resizes it.
pub fn sessions_default(body: u16) -> u16 {
    (body / 3).clamp(30, 52)
}

impl App {
    /// Applies a new session-list width; `save` also writes it to tui.json.
    pub fn set_sessions_width(&mut self, width: u16, save: bool) {
        self.sessions_width = Some(width);
        self.sessions_ratio = Some(width as f64 / self.body_area.width.max(1) as f64);
        if save
            && let Some(ratio) = self.sessions_ratio
            && let Err(e) = theme::persist_sessions(width, ratio)
        {
            self.toast(format!("Session list resized, but could not save: {e}"), Kind::Error);
        }
    }

    pub fn new(args: Args) -> Self {
        let (tx, rx) = channel();
        let config = theme::read_config();
        let theme = args.theme.as_deref().or(config.theme.as_deref()).and_then(theme::find).unwrap_or(0);
        let launch_layout = if args.beta { Layout::Beta } else { Layout::Classic };
        let mut app = App {
            update: args.update.clone(),
            args,
            exe: actions::ruddr_exe(),
            launch_cwd: std::env::current_dir().unwrap_or_default(),
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
            deja_available: actions::on_path("deja"),
            deja_hits: vec![],
            history: None,
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
            collapsed_dirs: HashSet::new(),
            tree_entries: vec![],
            diff_area: Rect::default(),
            dragging_tree: false,
            sessions_ratio: config.sessions_ratio,
            sessions_width: config.sessions_width,
            body_area: Rect::default(),
            dragging_sessions: false,
            expanded: HashSet::new(),
            cursor: None,
            reveal_cursor: false,
            follow: true,
            scroll_target: 0,
            scroll_pos: 0.0,
            unseen_base: None,
            sources: Sources::default(),
            cache: RenderCache::default(),
            tail_generation: 0,
            history_cache: HistoryCache::default(),
            loading_since: None,
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
            busy: false,
            hits: vec![],
            blocks: vec![],
            block_spans: vec![],
            screen_blocks: vec![],
            artifact_inner: Rect::default(),
            artifact_rows: 0,
            artifact_height: 0,
            buttons: vec![],
            mobile_now: false,
            match_count: None,
            last_refresh: Instant::now(),
            last_drain: Instant::now(),
            next_frame: None,
            dirty: true,
            frames: 0,
            show_frames: std::env::var("RUDDR_TUI_FRAMES").is_ok_and(|v| v == "1"),
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
            app.seen.insert(session.state_dir.clone(), (session.status, epoch));
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
        if self.mobile_now { Layout::Beta } else { self.launch_layout }
    }

    /// Asks for a frame `after` from now; the loop sleeps until the soonest.
    pub fn animate(&mut self, after: Duration) {
        let at = Instant::now() + after;
        if self.next_frame.is_none_or(|t| at < t) {
            self.next_frame = Some(at);
        }
    }

    pub fn visible(&self) -> Vec<&Session> {
        let mut sessions = filter_sessions(&self.sessions, &self.filter);
        if let Some(history) = self.history.as_ref().filter(|h| h.edits_only) {
            sessions.retain(|s| history.has_edits(&s.state_dir));
        }
        prioritize_explicit(&mut sessions, &self.args.state_dirs);
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
        self.dirty = true;
    }

    /// Re-reads every state.json. Marks the screen dirty only on a change.
    pub fn refresh(&mut self) {
        let sessions: Vec<Session> = match &self.history {
            Some(history) => {
                if !history.loading && history.loaded_at.is_none_or(|at| at.elapsed() >= HISTORY_REFRESH) {
                    self.load_history();
                }
                self.history.as_ref().map(|h| h.runs.clone()).unwrap_or_default()
            }
            None => ruddr_core::session::discover(&ruddr_core::session::Discover {
                state_dirs: self.args.state_dirs.clone(),
                roots: self.args.roots.clone(),
                registries: None,
            })
            .into_iter()
            .map(|s| s.state)
            .collect(),
        };
        if sessions != self.sessions {
            self.sessions = sessions;
            self.dirty = true;
        }
        let now = Instant::now();
        for session in &self.sessions {
            let entry = self.seen.entry(session.state_dir.clone()).or_insert((session.status, now));
            if entry.0 != session.status {
                *entry = (session.status, now);
            }
        }
        let visible: Vec<String> = self.visible().iter().map(|s| s.state_dir.clone()).collect();
        if self.selected.as_ref().is_none_or(|s| !visible.contains(s)) {
            self.selected = visible.first().cloned();
            self.reset_artifact();
            self.dirty = true;
        }
        self.ensure_sources();
        if let Some(cwd) = self.current().map(|s| s.cwd.clone()).filter(|c| !c.is_empty())
            && !self.branches.contains_key(&cwd)
        {
            self.branches.insert(cwd.clone(), String::new());
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let branch = actions::current_branch(&cwd);
                let _ = tx.send(Msg::Branch(cwd, branch));
            });
        }
        self.last_refresh = Instant::now();
    }

    /// Lists every agent's newest sessions on a background thread.
    fn load_history(&mut self) {
        let Some(history) = &mut self.history else { return };
        history.loading = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let sessions = ruddr_history::list_sessions(&ruddr_history::Stores::discover(), crate::history::LIMIT);
            let _ = tx.send(Msg::History(sessions));
        });
    }

    fn on_history(&mut self, sessions: Vec<ruddr_history::SessionInfo>) {
        let Some(history) = &mut self.history else { return };
        let first = history.loaded_at.is_none();
        history.loading = false;
        history.loaded_at = Some(Instant::now());
        history.runs = sessions.iter().map(crate::history::run_state).collect();
        let counts: Vec<String> = ruddr_history::Provider::ALL
            .iter()
            .filter_map(|p| {
                let n = sessions.iter().filter(|s| s.provider == *p).count();
                (n > 0).then(|| format!("{n} {}", p.name()))
            })
            .collect();
        history.infos = sessions
            .into_iter()
            .map(|info| (format!("{}{}", crate::history::PREFIX, info.locator), info))
            .collect();
        history.keep_pinned();
        self.scan_edits();
        if first {
            self.toasts.retain(|t| !t.text.starts_with("Loading sessions"));
            let text = if counts.is_empty() {
                "No agent sessions found".to_string()
            } else {
                counts.join(" · ")
            };
            self.toast(text, Kind::Info);
        }
        self.refresh();
    }

    /// Reads every listed session whose edits are unknown or out of date on
    /// a background thread, sending their line counts in batches.
    fn scan_edits(&mut self) {
        let Some(history) = &mut self.history else { return };
        if history.scanning {
            return;
        }
        let mut stale: Vec<(String, ruddr_history::SessionInfo)> = history
            .infos
            .iter()
            .filter(|(dir, info)| history.edits.get(*dir).is_none_or(|e| e.0 != info.updated_ms))
            .map(|(dir, info)| (dir.clone(), info.clone()))
            .collect();
        if stale.is_empty() {
            return;
        }
        // Newest first, so the top of the list fills in first.
        stale.sort_by_key(|(_, info)| std::cmp::Reverse(info.updated_ms));
        history.scanning = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut stats = Vec::new();
            for (index, (dir, info)) in stale.iter().enumerate() {
                let (added, removed) = match ruddr_history::load(info) {
                    Ok(transcript) => parse_git_diff(&ruddr_history::unified_diff(&transcript))
                        .1
                        .iter()
                        .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed)),
                    Err(_) => (0, 0),
                };
                stats.push((dir.clone(), info.updated_ms, added, removed));
                let done = index + 1 == stale.len();
                if done || stats.len() == 25 {
                    let batch = Msg::HistoryEdits {
                        stats: std::mem::take(&mut stats),
                        done,
                    };
                    if tx.send(batch).is_err() {
                        return;
                    }
                }
            }
        });
    }

    fn on_history_edits(&mut self, stats: Vec<(String, i64, u32, u32)>, done: bool) {
        let Some(history) = &mut self.history else { return };
        for (dir, updated_ms, added, removed) in stats {
            history.edits.insert(dir, (updated_ms, added, removed));
        }
        if done {
            history.scanning = false;
        }
        if history.edits_only {
            // The filtered list grew; keep the selection on a visible row.
            self.refresh();
        }
    }

    /// Limits the history list to sessions that edited files, or shows all.
    fn toggle_edits_only(&mut self) {
        let Some(history) = &mut self.history else {
            return self.toast("The edits filter works in the history list; press H first", Kind::Warning);
        };
        history.edits_only = !history.edits_only;
        let text = if history.edits_only {
            "Showing only sessions that edited files"
        } else {
            "Showing every session"
        };
        self.toast(text, Kind::Info);
        self.list_offset = 0;
        self.refresh();
    }

    /// Switches the session list between Ruddr's runs and every agent's
    /// history, keeping each side's selection.
    fn toggle_history(&mut self) {
        match self.history.take() {
            Some(history) => {
                self.selected = history.runs_selected;
                self.toast("Showing Ruddr sessions", Kind::Info);
            }
            None => {
                self.history = Some(HistoryMode {
                    runs_selected: self.selected.take(),
                    ..Default::default()
                });
                self.toast("Loading sessions from every agent…", Kind::Info);
            }
        }
        self.filter.clear();
        self.list_offset = 0;
        self.reset_artifact();
        self.refresh();
    }

    /// Finds a deja hit's session on a background thread; `Msg::Found`
    /// selects it in the history list so its chat and diff can be read.
    fn show_deja_hit(&mut self, hit: DejaHit) {
        let Some(provider) = ruddr_history::Provider::ALL.into_iter().find(|p| p.name() == hit.provider) else {
            return self.toast(format!("Cannot open {} sessions", hit.provider), Kind::Warning);
        };
        self.toast("Opening session…", Kind::Info);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let found = ruddr_history::find_session(&ruddr_history::Stores::discover(), provider, &hit.session_id)
                .ok_or_else(|| format!("No {} transcript found for {}", hit.provider, hit.session_id));
            let _ = tx.send(Msg::Found(found));
        });
    }

    fn on_found(&mut self, info: ruddr_history::SessionInfo) {
        let history = self.history.get_or_insert_with(|| HistoryMode {
            runs_selected: self.selected.clone(),
            ..Default::default()
        });
        history.pinned = Some(info.clone());
        history.keep_pinned();
        self.toasts.retain(|t| t.text != "Opening session…");
        self.filter.clear();
        self.selected = Some(format!("{}{}", crate::history::PREFIX, info.locator));
        self.reset_artifact();
        self.refresh();
    }

    fn load_models(&self) {
        let (exe, tx) = (self.exe.clone(), self.tx.clone());
        std::thread::spawn(move || {
            if let Ok(out) = actions::run_ruddr(&exe, &["models", "--json"], Duration::from_secs(10)) {
                let _ = tx.send(Msg::Models(parse_model_catalog(&out)));
            }
        });
    }

    pub fn reset_artifact(&mut self) {
        self.follow = self.tab != Tab::Diff;
        self.cursor = None;
        self.scroll_target = 0;
        self.scroll_pos = 0.0;
        self.unseen_base = None;
        self.sources.diff.next = None;
    }

    /// Starts a reader thread when the selected session (or its thread)
    /// changes. History loads first; later lines stream.
    pub fn ensure_sources(&mut self) {
        let Some(session) = self.current().cloned() else {
            self.loading_since = None;
            if self.sources.tailer.is_some() || !self.sources.scope.0.is_empty() {
                self.sources = Sources::default();
                self.dirty = true;
            }
            return;
        };
        let scope = (session.state_dir.clone(), session.thread_id.clone(), events_path(&session));
        if self.sources.scope == scope {
            return;
        }
        self.tail_generation += 1;
        if crate::history::is_history(&session.state_dir) {
            return self.load_history_session(scope);
        }
        self.loading_since = None;
        let files = vec![
            (Source::Events, events_path(&session)),
            (Source::Trace, trace_path(&session)),
            (Source::Output, output_path(&session)),
        ];
        let tailer = Tailer::spawn(self.tail_generation, files, self.tx.clone(), Msg::Tail);
        let same_dir = self.sources.scope.0 == session.state_dir;
        let diff = if same_dir {
            std::mem::take(&mut self.sources.diff)
        } else {
            DiffState {
                delay_ms: 1000,
                ..Default::default()
            }
        };
        self.sources = Sources {
            scope,
            generation: self.tail_generation,
            tailer: Some(tailer),
            transcript: Transcript::new(session.thread_id.as_deref()),
            diff,
            ..Default::default()
        };
        if !same_dir {
            self.expanded.clear();
        }
        self.dirty = true;
    }

    /// Reads a history session once: its chat, its assistant messages as the
    /// output, and its edits as the diff. Nothing streams afterwards. A
    /// session read recently comes from the cache before the next draw.
    fn load_history_session(&mut self, scope: (String, Option<String>, PathBuf)) {
        let state_dir = scope.0.clone();
        let info = self.history.as_ref().and_then(|h| h.infos.get(&state_dir)).cloned();
        let generation = self.tail_generation;
        self.sources = Sources {
            scope,
            generation,
            transcript: Transcript::new(None),
            diff: DiffState {
                pending: true,
                ..Default::default()
            },
            ..Default::default()
        };
        self.expanded.clear();
        self.dirty = true;
        if let Some(loaded) = self.history_cache.get(&state_dir) {
            return self.apply_history(generation, &state_dir, &loaded);
        }
        let Some(info) = info else {
            self.loading_since = None;
            return;
        };
        self.loading_since.get_or_insert_with(Instant::now);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let loaded = match ruddr_history::load(&info) {
                Ok(transcript) => LoadedHistory {
                    chat: ruddr_history::app_server::chat_lines(&transcript.events),
                    output: ruddr_history::app_server::output_lines(&transcript.events),
                    diff: Ok(ruddr_history::unified_diff(&transcript)),
                },
                Err(e) => LoadedHistory {
                    chat: vec![],
                    output: vec![],
                    diff: Err(e),
                },
            };
            let _ = tx.send(Msg::HistoryLoaded {
                generation,
                state_dir,
                loaded: Arc::new(loaded),
            });
        });
    }

    fn on_history_loaded(&mut self, generation: u64, state_dir: String, loaded: Arc<LoadedHistory>) {
        self.history_cache.insert(state_dir.clone(), loaded.clone());
        self.apply_history(generation, &state_dir, &loaded);
    }

    /// Shows a loaded history session if it is still the selected one.
    fn apply_history(&mut self, generation: u64, state_dir: &str, loaded: &LoadedHistory) {
        if self.sources.generation != generation || self.sources.scope.0 != state_dir {
            return;
        }
        for (source, lines) in [
            (Source::Events, loaded.chat.clone()),
            (Source::Output, loaded.output.clone()),
            (Source::Trace, vec![]),
        ] {
            self.on_tail(Batch {
                generation,
                source,
                reset: false,
                history_done: true,
                lines,
            });
        }
        self.on_diff(state_dir.to_string(), loaded.diff.clone(), vec![], None);
        self.loading_since = None;
    }

    /// Whether drawing now would show a history session before it loads.
    pub fn holding(&self, now: Instant) -> bool {
        self.loading_since.is_some_and(|t| now.duration_since(t) < LOAD_HOLD)
    }

    pub fn on_tail(&mut self, batch: Batch) {
        if self.sources.scope.0.is_empty() || self.sources.generation != batch.generation {
            return;
        }
        let now = Instant::now();
        let mut changed = batch.reset || batch.history_done;
        match batch.source {
            Source::Events => {
                if batch.reset {
                    // A rewritten log after the history load streams like
                    // any other new text.
                    self.sources.transcript = Transcript::new(self.sources.scope.1.as_deref());
                    if self.sources.loaded.contains(&Source::Events) {
                        self.sources.transcript.finish_history();
                    }
                }
                for line in &batch.lines {
                    changed |= self.sources.transcript.apply_line(line, now);
                }
                if batch.history_done {
                    self.sources.transcript.commit_all();
                    self.sources.transcript.finish_history();
                }
            }
            Source::Trace => {
                if batch.reset {
                    self.sources.activities = Activities::default();
                }
                for line in &batch.lines {
                    changed |= self.sources.activities.apply_line(line);
                }
            }
            Source::Output => {
                if batch.reset {
                    self.sources.output = OutputDoc::default();
                }
                for line in &batch.lines {
                    self.sources.output.push_line(line);
                }
                changed |= !batch.lines.is_empty();
            }
        }
        if batch.history_done {
            self.sources.loaded.insert(batch.source);
        }
        if changed {
            self.dirty = true;
        }
    }

    /// Commits queued streaming lines on the drain tick.
    pub fn drain_stream(&mut self, now: Instant) {
        if now.duration_since(self.last_drain) >= DRAIN_TICK {
            self.last_drain = now;
            if self.sources.transcript.drain(now) {
                self.dirty = true;
            }
        }
    }

    /// When the next drain tick is due, if lines are queued.
    pub fn drain_deadline(&self) -> Option<Instant> {
        self.sources.transcript.draining().then(|| self.last_drain + DRAIN_TICK)
    }

    /// Rebuilds the activity rows when the trace or the tool details moved.
    pub fn sync_activity(&mut self) {
        let key = (self.sources.activities.generation, self.sources.transcript.tool_generation);
        if self.sources.activity_key == key {
            return;
        }
        self.sources.activity_key = key;
        if self.sources.tools_seen != self.sources.transcript.tool_generation {
            self.sources.tools = self.sources.transcript.tool_details();
            self.sources.tools_seen = self.sources.transcript.tool_generation;
        }
        self.sources.activity_view = self.sources.activities.view(self.sources.transcript.commentary.as_deref());
        self.sources.activity_details = activity::attach_details(&self.sources.activity_view, &self.sources.tools);
    }

    /// Starts a background `git diff` when the diff tab is open and due.
    pub fn poll_diff(&mut self, now: Instant) {
        if self.tab != Tab::Diff || self.sources.diff.pending {
            return;
        }
        if self.sources.diff.next.is_some_and(|t| now < t) {
            return;
        }
        let Some(session) = self.current().cloned() else { return };
        if crate::history::is_history(&session.state_dir) {
            // Loaded with the session; a past session's edits do not change.
            return;
        }
        self.sources.diff.pending = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let recorded = |reason: String| {
                let result = actions::recorded_diff(&events_path(&session), &session.cwd);
                (result, vec![], Some(reason))
            };
            let (result, touched, recorded) = if session.cwd.is_empty() {
                recorded("No working directory".to_string())
            } else if !actions::is_git_work_tree(&session.cwd) {
                recorded("Not a Git repository".to_string())
            } else {
                match actions::workspace_diff(&session.cwd) {
                    Ok(raw) => {
                        let paths: Vec<String> = parse_git_diff(&raw).1.into_iter().map(|f| f.path).collect();
                        let touched = actions::touched_since(&session.cwd, &paths, &session.started_at);
                        (Ok(raw), touched, None)
                    }
                    Err(e) => recorded(format!(
                        "Git diff failed ({})",
                        e.lines().next().unwrap_or("").trim_end_matches('.')
                    )),
                }
            };
            let _ = tx.send(Msg::Diff {
                state_dir: session.state_dir,
                result,
                touched,
                recorded,
            });
        });
    }

    pub fn diff_deadline(&self) -> Option<Instant> {
        if self.tab != Tab::Diff || self.sources.diff.pending || self.history.is_some() {
            return None;
        }
        Some(self.sources.diff.next.unwrap_or_else(Instant::now))
    }

    fn on_diff(&mut self, state_dir: String, result: Result<String, String>, touched: Vec<String>, recorded: Option<String>) {
        if self.sources.scope.0 != state_dir {
            return;
        }
        let diff = &mut self.sources.diff;
        diff.pending = false;
        let (raw, error) = match result {
            Ok(raw) => (raw, None),
            Err(e) => (String::new(), Some(e)),
        };
        let touched: HashSet<String> = touched.into_iter().collect();
        let changed = raw != diff.raw || error != diff.error || touched != diff.touched || recorded != diff.recorded || !diff.loaded;
        if changed {
            (diff.lines, diff.files) = parse_git_diff(&raw);
            diff.gutter = diff
                .lines
                .iter()
                .filter_map(|l| l.old.max(l.new))
                .max()
                .unwrap_or(1)
                .to_string()
                .len()
                .max(2);
            diff.raw = raw;
            diff.error = error;
            diff.touched = touched;
            diff.recorded = recorded;
            diff.generation += 1;
            diff.loaded = true;
            self.dirty = true;
        }
        diff.delay_ms = next_diff_poll_ms(diff.delay_ms.max(1000), changed);
        diff.next = Some(Instant::now() + Duration::from_millis(diff.delay_ms));
    }

    /// Drops expired toasts and disarmed stops; ends the splash.
    pub fn housekeeping(&mut self) {
        let before = self.toasts.len();
        self.toasts.retain(|t| t.born.elapsed() < t.lifetime());
        if self.toasts.len() != before {
            self.dirty = true;
        }
        if self
            .stop_armed
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(2))
        {
            self.stop_armed = None;
            self.dirty = true;
        }
        if self.splash && self.started.elapsed() > Duration::from_millis(1100) {
            self.splash = false;
            self.dirty = true;
        }
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

    /// Blocks that occupy at least one row, in order.
    fn present_blocks(&self) -> Vec<usize> {
        self.block_spans
            .iter()
            .enumerate()
            .filter(|(_, (_, len))| *len > 0)
            .map(|(i, _)| i)
            .collect()
    }

    fn move_cursor(&mut self, delta: isize) {
        let present = self.present_blocks();
        if present.is_empty() {
            return;
        }
        let position = match self.cursor.and_then(|c| present.iter().position(|g| *g == c)) {
            Some(p) => (p as isize + delta).clamp(0, present.len() as isize - 1) as usize,
            None => {
                // Start from what is on screen.
                let top = if self.follow {
                    self.artifact_rows.saturating_sub(self.artifact_height)
                } else {
                    self.scroll_target
                };
                let first = present
                    .iter()
                    .position(|b| self.block_spans[*b].0 + self.block_spans[*b].1 > top)
                    .unwrap_or(0);
                if delta < 0 {
                    let bottom = top + self.artifact_height;
                    present.iter().rposition(|b| self.block_spans[*b].0 < bottom).unwrap_or(first)
                } else {
                    first
                }
            }
        };
        self.cursor = Some(present[position]);
        self.follow = false;
        self.reveal_cursor = true;
    }

    fn jump(&mut self, forward: bool, hunk: bool) {
        let matches: Vec<usize> = self
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| if hunk { b.hunk } else { b.diff_header.is_some() })
            .map(|(i, _)| i)
            .collect();
        if matches.is_empty() {
            return;
        }
        let current = self.cursor;
        let next = if forward {
            matches.iter().find(|i| current.is_none_or(|c| **i > c)).or(matches.first())
        } else {
            matches.iter().rev().find(|i| current.is_none_or(|c| **i < c)).or(matches.last())
        };
        if let Some(&target) = next {
            self.cursor = Some(target);
            self.follow = false;
            self.reveal_cursor = true;
        }
    }

    fn block_matches(&self, query: &str) -> Vec<usize> {
        let query = query.to_lowercase();
        (0..self.blocks.len())
            .filter(|i| view::block_text(self, &self.blocks[*i]).to_lowercase().contains(&query))
            .collect()
    }

    fn jump_match(&mut self, forward: bool) {
        let query = self.artifact_query.get(&self.tab).cloned().unwrap_or_default();
        if query.is_empty() {
            return;
        }
        let hits = self.block_matches(&query);
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

    /// Search matches in the current pane, cached per content change.
    pub fn match_count(&mut self, query: &str) -> usize {
        let key = view::content_key(self);
        if let Some((k, q, n)) = &self.match_count
            && *k == key
            && q == query
        {
            return *n;
        }
        let count = self.block_matches(query).len();
        self.match_count = Some((key, query.to_string(), count));
        count
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

    fn cursor_block(&self) -> Option<&Block> {
        self.cursor.and_then(|c| self.blocks.get(c))
    }

    /// Enter on a row: fold a diff file, expand a tool, or run a row action.
    fn activate(&mut self) -> bool {
        let Some(block) = self.cursor_block().cloned() else { return false };
        if let Some(path) = &block.diff_header {
            if !self.folded.remove(path) {
                self.folded.insert(path.clone());
            }
            return true;
        }
        match block.r {
            view::BlockRef::Activity(index) => {
                let Some(activity) = self.sources.activity_view.get(index) else {
                    return false;
                };
                if activity.kind != activity::ActivityKind::Tool {
                    return false;
                }
                let uid = activity.uid;
                if !self.expanded.remove(&uid) {
                    self.expanded.insert(uid);
                }
                self.reveal_cursor = true;
                true
            }
            view::BlockRef::Empty if self.current().is_some_and(|s| prompt_route(s).is_some()) => {
                self.run(Cmd::Prompt);
                true
            }
            _ => false,
        }
    }

    // --- keys -----------------------------------------------------------

    pub fn on_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Mouse(mouse) => self.on_mouse(mouse),
            Event::Paste(text) => self.on_paste(text),
            Event::Resize(..) => {}
            _ => return,
        }
        self.dirty = true;
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            if let Some(p) = &mut self.prompt
                && !p.text.is_empty()
            {
                p.text.clear();
                p.cursor = 0;
                return;
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
            KeyCode::Char('<') | KeyCode::Char('>') if self.layout() == Layout::Classic => {
                let current = self.sessions_width.unwrap_or_else(|| sessions_default(self.body_area.width));
                let step: i32 = if key.code == KeyCode::Char('>') { 4 } else { -4 };
                let width = (current as i32 + step).clamp(SESSIONS_MIN as i32, sessions_max(self.body_area.width) as i32) as u16;
                self.set_sessions_width(width, true);
            }
            KeyCode::Char('D') => self.ask_delete_selected(),
            KeyCode::Char('H') => self.run(Cmd::History),
            KeyCode::Char('e') => self.run(Cmd::EditsOnly),
            KeyCode::Char(c @ (']' | '[')) => self.bracket = Some(c),
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
                } else if !(self.focus == Focus::Artifact && self.activate()) {
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
            KeyCode::Char(_) if key.modifiers.contains(KeyModifiers::CONTROL) => return,
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
            KeyCode::Char('r') if picker.kind == PickerKind::Deja && key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(Action::Deja(hit)) = picker.selected().map(|i| picker.items[i].action.clone()) {
                    self.picker = None;
                    let hit = self.deja_hits[hit].clone();
                    self.open_new_prompt(Some(hit));
                }
                return;
            }
            KeyCode::Backspace if picker.filterable => {
                picker.query.pop();
                picker.index = 0;
            }
            KeyCode::Char(c) if picker.filterable && !key.modifiers.contains(KeyModifiers::CONTROL) => {
                picker.query.push(c);
                picker.index = 0;
            }
            _ => {}
        }
        self.preview_theme();
    }

    fn preview_theme(&mut self) {
        if let Some(picker) = &self.picker
            && picker.kind == PickerKind::Theme
            && let Some(Action::Theme(index)) = picker.selected().map(|i| picker.items[i].action.clone())
        {
            self.theme = index;
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
                self.show_deja_hit(hit);
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
            KeyCode::Tab if !matches!(prompt.kind, PromptKind::Route(PromptRoute::Steer | PromptRoute::Prompt)) => self.run(Cmd::Model),
            KeyCode::Char('v') if ctrl => self.paste_image(),
            KeyCode::Backspace if prompt.cursor == 0 && !prompt.images.is_empty() => {
                prompt.images.pop();
            }
            KeyCode::Backspace if alt || ctrl => prompt.delete_word(),
            // Most terminals send Ctrl+Backspace as Ctrl+H.
            KeyCode::Char('w' | 'h') if ctrl => prompt.delete_word(),
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
            KeyCode::Char(c) if !ctrl => prompt.insert(c),
            _ => {}
        }
    }

    fn on_paste(&mut self, text: String) {
        let clean = text.replace("\r\n", "\n").replace('\r', "\n");
        if self.prompt.is_some() {
            // A terminal pastes nothing for a copied image, and pastes the
            // paths of files dropped on it.
            if clean.trim().is_empty() {
                return self.paste_image();
            }
            if let Some(paths) = pasted_image_paths(&clean) {
                return self.attach_images(paths);
            }
        }
        if let Some(prompt) = &mut self.prompt {
            for c in clean.chars() {
                prompt.insert(c);
            }
            prompt.typed = Instant::now();
        } else if let Some(search) = &mut self.search {
            search.text.push_str(clean.lines().next().unwrap_or(""));
        } else if let Some(picker) = &mut self.picker
            && picker.filterable
        {
            picker.query.push_str(clean.lines().next().unwrap_or(""));
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

    /// The artifact block under a screen position.
    fn block_at(&self, x: u16, y: u16) -> Option<usize> {
        let area = self.artifact_inner;
        if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
            return None;
        }
        self.screen_blocks.get((y - area.y) as usize).copied().flatten()
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
                    Some(Hit::TreeRow(row)) => match self.tree_entries.get(row).cloned() {
                        Some(TreeEntry::Dir { path, .. }) => {
                            if !self.collapsed_dirs.remove(&path) {
                                self.collapsed_dirs.insert(path);
                            }
                        }
                        Some(TreeEntry::File { index, .. }) => {
                            if let Some(path) = self.sources.diff.files.get(index).map(|f| f.path.clone())
                                && let Some(block) = self.blocks.iter().position(|b| b.diff_header.as_deref() == Some(&path))
                            {
                                self.cursor = Some(block);
                                self.follow = false;
                                self.reveal_cursor = true;
                                self.focus = Focus::Artifact;
                            }
                        }
                        None => {}
                    },
                    Some(Hit::TreeDivider) => self.dragging_tree = true,
                    Some(Hit::SessionsDivider) => self.dragging_sessions = true,
                    Some(Hit::Backdrop) => {
                        self.drawer = false;
                        self.focus = Focus::Artifact;
                    }
                    Some(Hit::Artifact) => {
                        self.focus = Focus::Artifact;
                        // A click selects the row; on a tool, a diff file, or
                        // an action row it also toggles or runs it.
                        if let Some(block) = self.block_at(mouse.column, mouse.row) {
                            let same = self.cursor == Some(block);
                            self.cursor = Some(block);
                            self.follow = false;
                            let toggles = self.blocks.get(block).is_some_and(|b| {
                                b.diff_header.is_some() || matches!(b.r, view::BlockRef::Activity(_) | view::BlockRef::Empty)
                            });
                            if toggles || same {
                                self.activate();
                            }
                        }
                    }
                    _ => {}
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_tree => {
                let area = self.diff_area;
                let maximum = 60.min(area.width.saturating_sub(40)).max(20);
                let width = (mouse.column.saturating_sub(area.x) + 1).clamp(20, maximum);
                self.tree_width = Some(width);
                self.tree_ratio = Some(width as f64 / area.width.max(1) as f64);
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_sessions => {
                let area = self.body_area;
                let width = (mouse.column.saturating_sub(area.x) + 1).clamp(SESSIONS_MIN, sessions_max(area.width));
                self.set_sessions_width(width, false);
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging_sessions => {
                self.dragging_sessions = false;
                if let Some(width) = self.sessions_width {
                    self.set_sessions_width(width, true);
                }
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging_tree => {
                self.dragging_tree = false;
                if let (Some(width), Some(ratio)) = (self.tree_width, self.tree_ratio)
                    && let Err(e) = theme::persist_tree(width, ratio)
                {
                    self.toast(format!("Sidebar resized, but could not save: {e}"), Kind::Error);
                }
            }
            MouseEventKind::Down(MouseButton::Right) => match hit {
                Some(Hit::Session(index)) => {
                    if let Some(dir) = self.visible().get(index).map(|s| s.state_dir.clone()) {
                        self.select(dir);
                        self.open_session_menu(Some((mouse.column, mouse.row)));
                    }
                }
                Some(Hit::Artifact) => {
                    if let Some(block) = self.block_at(mouse.column, mouse.row) {
                        self.cursor = Some(block);
                        self.follow = false;
                        self.focus = Focus::Artifact;
                        self.open_row_menu(block, (mouse.column, mouse.row));
                    }
                }
                _ => {}
            },
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
            Cmd::ChangeDir => {
                self.open_new_prompt(None);
                if let Some(prompt) = &mut self.prompt {
                    prompt.text = "/cd ".chars().collect();
                    prompt.cursor = prompt.text.len();
                }
            }
            Cmd::Model => self.open_model_picker(),
            Cmd::Find => {
                if !self.deja_available {
                    self.toast("deja is not on PATH; install it to search past sessions", Kind::Warning);
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
                let paths: Vec<String> = self.sources.diff.files.iter().map(|f| f.path.clone()).collect();
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
            Cmd::History => self.toggle_history(),
            Cmd::EditsOnly => self.toggle_edits_only(),
            Cmd::Theme => self.open_theme_picker(),
            Cmd::Refresh => {
                self.refresh();
                self.sources.diff.next = None;
                self.toast("Sessions refreshed", Kind::Info);
            }
            Cmd::Copy => {
                let text = match self.cursor {
                    Some(block) => self.blocks.get(block).map(|b| view::block_copy(self, b)),
                    None => self.blocks.iter().rev().map(|b| view::block_copy(self, b)).find(|t| !t.is_empty()),
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
            Cmd::Activate => {
                self.activate();
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
                let (mut ok, mut failed) = (0, None);
                for dir in dirs.iter().filter(|dir| !crate::history::is_history(dir)) {
                    if let Some(session) = self.sessions.iter().find(|s| &s.state_dir == dir) {
                        match delete_session(session) {
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
        let can_stop = session.as_ref().is_some_and(|s| stoppable(s.status));
        let broken: Vec<String> = self
            .sessions
            .iter()
            .filter(|s| matches!(s.status, Status::Failed | Status::Stale))
            .map(|s| s.state_dir.clone())
            .collect();
        let cmd = |label: &str, key: &str, cmd: Cmd| PickItem::new(label, Action::Cmd(cmd)).key(key);
        let mut items = vec![
            cmd("Send a prompt", "s", Cmd::Prompt)
                .hint("steer, prompt, or continue the selected session")
                .disabled_if(route.is_none(), "no promptable session selected"),
            cmd("New session", "n", Cmd::New).hint("pick a provider and model, then type the first prompt"),
            cmd("Change new-session directory", "", Cmd::ChangeDir).hint(format!(
                "now {}; or type /cd DIR in a new-session prompt",
                short_path(&self.launch_cwd)
            )),
            cmd("Continue thread in a new run", "R", Cmd::Continue)
                .hint("finished sessions only")
                .disabled_if(route != Some(PromptRoute::Continue), "select a finished session with a thread"),
            cmd("Choose model", "m", Cmd::Model),
            cmd("Find a past session", "f", Cmd::Find)
                .hint("deja search")
                .disabled_if(!self.deja_available, "deja is not on PATH"),
            cmd(
                if self.history.is_some() {
                    "Back to Ruddr sessions"
                } else {
                    "Browse every agent's sessions"
                },
                "H",
                Cmd::History,
            )
            .hint("Codex, Claude, Pi, OpenCode, and Droid history, read-only, with each session's diff"),
            cmd(
                if self.history.as_ref().is_some_and(|h| h.edits_only) {
                    "Show every history session"
                } else {
                    "Only sessions that edited files"
                },
                "e",
                Cmd::EditsOnly,
            )
            .hint("history list: hide sessions with an empty diff")
            .disabled_if(self.history.is_none(), "open the history list with H first"),
            cmd(
                if session.as_ref().is_some_and(|s| s.status == Status::Idle) {
                    "End idle session"
                } else {
                    "Interrupt turn"
                },
                "x x",
                Cmd::StopNow,
            )
            .disabled_if(!can_stop, "no active or idle session"),
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
                if session.status == Status::Idle {
                    "End idle session"
                } else {
                    "Interrupt turn"
                },
                Action::Cmd(Cmd::StopNow),
            )
            .disabled_if(!stoppable(session.status), "not running"),
            PickItem::new("Open chat", Action::Cmd(Cmd::Tab(Tab::Chat))),
            PickItem::new("Open diff", Action::Cmd(Cmd::Tab(Tab::Diff))),
            PickItem::new("Copy state directory", Action::Cmd(Cmd::CopyText(session.state_dir.clone()))),
        ];
        if let Some(thread) = &session.thread_id {
            items.push(PickItem::new("Copy thread id", Action::Cmd(Cmd::CopyText(thread.clone()))));
        }
        if !session.cwd.is_empty() {
            items.push(PickItem::new(
                "Copy working directory",
                Action::Cmd(Cmd::CopyText(session.cwd.clone())),
            ));
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

    /// Right-click on an artifact row.
    fn open_row_menu(&mut self, block: usize, anchor: (u16, u16)) {
        let Some(b) = self.blocks.get(block).cloned() else { return };
        let mut items = Vec::new();
        let copy = view::block_copy(self, &b);
        if !copy.is_empty() {
            items.push(PickItem::new("Copy row", Action::Cmd(Cmd::CopyText(copy))).key("c"));
        }
        if let Some(path) = &b.diff_header {
            items.push(
                PickItem::new(
                    if self.folded.contains(path) { "Unfold file" } else { "Fold file" },
                    Action::Cmd(Cmd::Activate),
                )
                .key("Enter"),
            );
            items.push(PickItem::new("Copy path", Action::Cmd(Cmd::CopyText(path.clone()))));
        }
        if let view::BlockRef::Activity(index) = b.r
            && let Some(a) = self
                .sources
                .activity_view
                .get(index)
                .filter(|a| a.kind == activity::ActivityKind::Tool)
        {
            let label = if self.expanded.contains(&a.uid) {
                "Collapse tool"
            } else {
                "Expand tool"
            };
            items.push(PickItem::new(label, Action::Cmd(Cmd::Activate)).key("Enter"));
        }
        if !self.follow && self.tab != Tab::Diff {
            items.push(PickItem::new("Resume live follow", Action::Cmd(Cmd::Follow)).key("G"));
        }
        if items.is_empty() {
            return;
        }
        let mut picker = Picker::new(PickerKind::Menu, self.tab.title(), items, false);
        picker.anchor = Some(anchor);
        self.picker = Some(picker);
    }

    fn ask_delete_selected(&mut self) {
        let Some(session) = self.current().cloned() else { return };
        if crate::history::is_history(&session.state_dir) {
            return self.toast("Sessions from agent history are read-only", Kind::Warning);
        }
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
                    .find(|m| m.provider == crate::core::provider(target) && m.id.as_deref() == opt(&target.model))
                    .cloned()
            });
        let mut items = Vec::new();
        let mut select = None;
        for (i, model) in self.models.iter().enumerate() {
            if provider.as_ref().is_some_and(|p| p != &model.provider) {
                continue;
            }
            let unavailable = model.note.clone().unwrap_or("unavailable".into());
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
                unavailable.clone()
            })
            .disabled_if(!model.available, &format!("{} is {unavailable}", model.provider));
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
        const CONTINUE_HINT: &str = "Select a finished session with a thread and working directory to continue";
        let session = self.current().cloned();
        let route = session.as_ref().and_then(prompt_route);
        let (Some(session), Some(route)) = (session, route) else {
            let text = match (wanted, self.current()) {
                (_, Some(s)) if crate::history::is_history(&s.state_dir) => "Sessions from agent history are read-only".to_string(),
                (Some(PromptRoute::Continue), _) => CONTINUE_HINT.to_string(),
                (_, Some(s)) => format!("Session is {}; press n for a new session", s.status),
                (_, None) => "No session selected; press n for a new session".to_string(),
            };
            return self.toast(text, Kind::Warning);
        };
        if wanted.is_some_and(|w| w != route) {
            return self.toast(CONTINUE_HINT, Kind::Warning);
        }
        let (model, effort) = match (&self.pending_model, route) {
            (Some((m, e)), PromptRoute::Continue) if m.provider == crate::core::provider(&session) => (Some(m.clone()), e.clone()),
            _ => (None, None),
        };
        self.prompt = Some(Prompt {
            kind: PromptKind::Route(route),
            text: vec![],
            cursor: 0,
            provider: crate::core::provider(&session).to_string(),
            target: Some(session),
            model,
            effort,
            resume: None,
            images: vec![],
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
            .or_else(|| self.current().map(|s| crate::core::provider(s).to_string()))
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
            images: vec![],
            opened: Instant::now(),
            typed: Instant::now(),
        });
    }

    /// Reads the clipboard image on a helper thread. It is saved under the
    /// directory the prompt's session runs in, so a sandboxed agent can
    /// read it.
    fn paste_image(&mut self) {
        let Some(prompt) = &self.prompt else { return };
        let cwd = match &prompt.target {
            Some(session) if !session.cwd.is_empty() => PathBuf::from(&session.cwd),
            _ => self.launch_cwd.clone(),
        };
        let tx = self.tx.clone();
        self.toast("Reading the clipboard…", Kind::Info);
        std::thread::spawn(move || {
            let base = ruddr_core::paths::launch_runs_dir(&cwd);
            let result = ruddr_core::paths::ensure_ignored_runs_dir(&base)
                .map_err(|e| format!("create {}: {e}", base.display()))
                .and_then(|()| actions::paste_clipboard_image(&base.join("images")));
            let _ = tx.send(Msg::Attached(result));
        });
    }

    fn attach_images(&mut self, paths: Vec<PathBuf>) {
        let Some(prompt) = &mut self.prompt else { return };
        let mut added = 0;
        for path in paths {
            if prompt.images.len() >= ruddr_core::images::MAX_IMAGES {
                return self.toast(
                    format!("A prompt carries at most {} images", ruddr_core::images::MAX_IMAGES),
                    Kind::Warning,
                );
            }
            if !prompt.images.contains(&path) {
                prompt.images.push(path);
                added += 1;
            }
        }
        prompt.typed = Instant::now();
        if added > 0 {
            let count = prompt.images.len();
            self.toast(format!("{count} image{} attached", if count == 1 { "" } else { "s" }), Kind::Info);
        }
    }

    fn submit(&mut self) {
        let Some(prompt) = self.prompt.take() else { return };
        let message: String = prompt.text.iter().collect::<String>().trim().to_string();
        if message.is_empty() {
            if !prompt.images.is_empty() {
                self.toast("Type a message to send with the images", Kind::Warning);
            }
            self.prompt = Some(prompt);
            return;
        }
        if prompt.kind == PromptKind::New
            && let Some(arg) = cd_argument(&message)
        {
            let start = std::env::current_dir().unwrap_or_default();
            let mut prompt = prompt;
            match resolve_cd(arg, &self.launch_cwd, &start, &ruddr_core::paths::home_dir()) {
                Ok(dir) => {
                    prompt.text.clear();
                    prompt.cursor = 0;
                    self.toast(format!("New sessions start in {}", short_path(&dir)), Kind::Info);
                    self.launch_cwd = dir;
                }
                Err(error) => self.toast(error, Kind::Warning),
            }
            self.prompt = Some(prompt);
            return;
        }
        let (exe, tx) = (self.exe.clone(), self.tx.clone());
        let images: Vec<String> = prompt.images.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        let draft_images = prompt.images.clone();
        let overrides = LaunchOverrides {
            model: prompt.model.as_ref().and_then(|m| m.id.clone()),
            effort: prompt.effort.clone(),
            images: images.clone(),
        };
        if prompt.kind == PromptKind::New {
            let cwd = self.launch_cwd.clone();
            let provider = prompt.provider.clone();
            let resume = prompt.resume.clone();
            self.toast(
                if let Some(r) = &resume {
                    format!("Resuming {} session…", r.provider)
                } else {
                    format!("Starting {provider} session…")
                },
                Kind::Info,
            );
            std::thread::spawn(move || {
                let cwd_s = cwd.to_string_lossy().into_owned();
                let spawned = tx.clone();
                let result = actions::launch(
                    &exe,
                    &cwd,
                    &message,
                    |p, d| new_session_args(&provider, &cwd_s, p, d, &overrides, resume.as_ref().map(|r| r.session_id.as_str())),
                    |dir| {
                        let _ = spawned.send(Msg::Spawned(dir.to_string_lossy().into_owned()));
                    },
                );
                send_result(&tx, result.map(|d| format!("Started {provider} session in {}", short_path(&d))));
            });
            self.pending_model = None;
            return;
        }
        // Re-resolve the target from fresh state so a turn that ended while
        // typing never turns into a different route.
        let target = prompt.target.clone().unwrap();
        let PromptKind::Route(route) = prompt.kind else { return };
        self.refresh();
        let Some(session) = self.sessions.iter().find(|s| s.state_dir == target.state_dir).cloned() else {
            self.prompt = Some(prompt);
            return self.toast("The session disappeared; prompt kept, not sent", Kind::Error);
        };
        if let Err(error) = revalidate_route(&session, route, target.turn_id.as_deref()) {
            self.prompt = Some(prompt);
            return self.toast(format!("{error}; prompt kept"), Kind::Warning);
        }
        self.follow = true;
        self.cursor = None;
        self.toast(
            match route {
                PromptRoute::Steer => "Steering…",
                PromptRoute::Prompt => "Sending prompt…",
                PromptRoute::Continue => "Starting continuation…",
            },
            Kind::Info,
        );
        let observed_turn = target.turn_id.clone();
        std::thread::spawn(move || {
            let state_dir = session.state_dir.clone();
            let result = match route {
                PromptRoute::Steer | PromptRoute::Prompt => {
                    actions::send_prompt(Path::new(&state_dir), route, observed_turn.as_deref(), &message, &images)
                }
                PromptRoute::Continue => {
                    let spawned = tx.clone();
                    actions::launch(
                        &exe,
                        Path::new(&session.cwd),
                        &message,
                        |p, d| continuation_args(&session, p, d, &overrides),
                        |dir| {
                            let _ = spawned.send(Msg::Spawned(dir.to_string_lossy().into_owned()));
                        },
                    )
                    .map(|d| format!("Continued in {}", short_path(&d)))
                }
            };
            let _ = tx.send(match result {
                Ok(text) => Msg::Toast(text, Kind::Success),
                // A control request that never went out returns the draft.
                Err(error) if route != PromptRoute::Continue => Msg::Bounced {
                    state_dir,
                    text: message,
                    images: draft_images,
                    error,
                },
                Err(error) => Msg::Toast(error, Kind::Error),
            });
        });
        self.pending_model = None;
    }

    fn request_stop(&mut self, now: bool) {
        let Some(session) = self.current().cloned() else { return };
        if !stoppable(session.status) {
            return self.toast("Only an active or idle session can be stopped", Kind::Warning);
        }
        if self.busy {
            return;
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
        let idle = session.status == Status::Idle;
        self.toast(if idle { "Ending idle session…" } else { "Interrupting turn…" }, Kind::Warning);
        self.busy = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = actions::stop(Path::new(&session.state_dir), session.status, session.turn_id.as_deref());
            send_result(&tx, result);
        });
    }

    fn run_deja(&mut self, terms: String) {
        if terms.trim().is_empty() {
            return;
        }
        self.toast(format!("Searching past sessions for “{}”…", terms.trim()), Kind::Info);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut command = std::process::Command::new("deja");
            command.arg("find").args(terms.split_whitespace()).args(["--json", "--quiet"]);
            let result = match actions::run_bounded(command, Duration::from_secs(30), 8 * 1024 * 1024) {
                Ok((out, _, true, _)) => Ok(parse_deja_hits(&out)),
                Ok((_, err, false, _)) => Err(if err.trim().is_empty() {
                    "deja find failed".to_string()
                } else {
                    err.trim().to_string()
                }),
                Err(e) => Err(format!("deja find {e}")),
            };
            let _ = tx.send(Msg::Deja(result));
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
        let (exe, tx) = (self.exe.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Updated(actions::run_ruddr(&exe, &["update"], Duration::from_secs(600))));
        });
    }

    pub fn on_msg(&mut self, msg: Msg) {
        if !matches!(msg, Msg::Input(_) | Msg::Tail(_)) {
            self.dirty = true;
        }
        match msg {
            Msg::Input(event) => self.on_event(event),
            Msg::Tail(batch) => self.on_tail(batch),
            Msg::Toast(text, kind) => {
                self.busy = false;
                self.toast(text, kind);
                self.refresh();
            }
            Msg::Spawned(dir) => {
                let path = PathBuf::from(&dir);
                if !self.args.state_dirs.contains(&path) {
                    self.args.state_dirs.push(path);
                }
                self.filter.clear();
                self.selected = Some(dir);
                self.reset_artifact();
                self.refresh();
            }
            Msg::Bounced {
                state_dir,
                text,
                images,
                error,
            } => {
                self.toast(format!("{error}; the draft is back in the editor"), Kind::Error);
                self.refresh();
                if self.prompt.is_none() && self.selected.as_deref() == Some(&state_dir) {
                    self.open_prompt(None);
                    if let Some(prompt) = &mut self.prompt {
                        prompt.text = text.chars().collect();
                        prompt.cursor = prompt.text.len();
                        prompt.images = images;
                    }
                }
            }
            Msg::Attached(Ok(path)) => self.attach_images(vec![path]),
            Msg::Attached(Err(error)) => self.toast(error, Kind::Warning),
            Msg::Models(models) => self.models = models,
            Msg::History(sessions) => self.on_history(sessions),
            Msg::Found(Ok(info)) => self.on_found(info),
            Msg::Found(Err(e)) => self.toast(e, Kind::Error),
            Msg::HistoryEdits { stats, done } => self.on_history_edits(stats, done),
            Msg::HistoryLoaded {
                generation,
                state_dir,
                loaded,
            } => self.on_history_loaded(generation, state_dir, loaded),
            Msg::Branch(cwd, branch) => {
                self.branches.insert(cwd, branch);
            }
            Msg::Diff {
                state_dir,
                result,
                touched,
                recorded,
            } => self.on_diff(state_dir, result, touched, recorded),
            Msg::Deja(Err(e)) => self.toast(e, Kind::Error),
            Msg::Deja(Ok(hits)) if hits.is_empty() => self.toast("No past sessions matched", Kind::Warning),
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
                self.toast(format!("{} past sessions found", hits.len()), Kind::Success);
                self.deja_hits = hits;
                self.picker = Some(Picker::new(PickerKind::Deja, "find a past session", items, true));
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

pub fn short_path(path: &Path) -> String {
    let home = ruddr_core::paths::home_dir();
    match path.strip_prefix(&home) {
        Ok(rest) if home != Path::new(".") => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

fn copy_osc52(text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", crate::text::base64(text.as_bytes()));
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_list_width_keeps_room_for_the_main_pane() {
        assert_eq!(sessions_default(150), 50);
        assert_eq!(sessions_default(60), 30);
        assert_eq!(sessions_max(200), 140);
        assert_eq!(sessions_max(70), SESSIONS_MIN, "a narrow body still allows the minimum");
    }

    #[test]
    fn output_paragraphs_keep_fences_whole() {
        let mut doc = OutputDoc::default();
        for line in ["# Title", "", "```sh", "a", "", "b", "```", "", "", "tail"] {
            doc.push_line(line);
        }
        let texts: Vec<&str> = doc.paragraphs.iter().map(|p| p.2.as_str()).collect();
        assert_eq!(texts, vec!["# Title", "```sh\na\n\nb\n```", "tail"]);
        let ids: HashSet<u64> = doc.paragraphs.iter().map(|p| p.0).collect();
        assert_eq!(ids.len(), 3, "paragraph ids are unique");
    }

    #[test]
    fn a_pinned_history_session_survives_a_reload() {
        let info = |id: &str| ruddr_history::SessionInfo {
            provider: ruddr_history::Provider::Claude,
            locator: format!("/p/{id}.jsonl"),
            id: id.into(),
            cwd: "/w".into(),
            title: String::new(),
            updated_ms: 1,
        };
        let mut history = HistoryMode {
            pinned: Some(info("old")),
            ..Default::default()
        };
        history.keep_pinned();
        history.keep_pinned();
        assert_eq!(history.runs.len(), 1, "pinned once");
        assert!(history.infos.contains_key("history:/p/old.jsonl"));
        // A reload that already lists the session does not add it twice.
        history.runs = vec![crate::history::run_state(&info("new")), crate::history::run_state(&info("old"))];
        history.infos = [info("new"), info("old")]
            .into_iter()
            .map(|i| (format!("history:{}", i.locator), i))
            .collect();
        history.keep_pinned();
        assert_eq!(history.runs.len(), 2);
    }

    #[test]
    fn the_history_cache_keeps_the_most_recent_sessions() {
        let loaded = |text: &str| {
            Arc::new(LoadedHistory {
                chat: vec![text.into()],
                output: vec![],
                diff: Ok(String::new()),
            })
        };
        let mut cache = HistoryCache::default();
        for i in 0..HistoryCache::CAPACITY {
            cache.insert(format!("history:{i}"), loaded(&i.to_string()));
        }
        // Reading the oldest makes it the newest, so the next insert evicts "1".
        assert_eq!(cache.get("history:0").unwrap().chat, ["0"]);
        cache.insert("history:new".into(), loaded("new"));
        assert!(cache.get("history:1").is_none());
        assert!(cache.get("history:0").is_some());
        // A reload replaces the entry instead of adding a second one.
        cache.insert("history:0".into(), loaded("again"));
        assert_eq!(cache.get("history:0").unwrap().chat, ["again"]);
        assert_eq!(cache.0.len(), HistoryCache::CAPACITY);
    }

    #[test]
    fn the_edits_filter_keeps_only_scanned_sessions_with_changes() {
        let mut history = HistoryMode::default();
        history.edits.insert("history:a".into(), (1, 12, 3));
        history.edits.insert("history:b".into(), (1, 0, 0));
        assert_eq!(history.edit_stat("history:a"), Some((12, 3)));
        assert!(history.has_edits("history:a"));
        assert!(!history.has_edits("history:b"), "an empty diff is filtered out");
        assert!(!history.has_edits("history:c"), "an unscanned session waits for its scan");
    }

    #[test]
    fn prompt_editing_moves_by_words_and_lines() {
        let mut p = Prompt {
            kind: PromptKind::New,
            text: vec![],
            cursor: 0,
            target: None,
            provider: "codex".into(),
            model: None,
            effort: None,
            resume: None,
            images: vec![],
            opened: Instant::now(),
            typed: Instant::now(),
        };
        for c in "one two\nthree".chars() {
            p.insert(c);
        }
        p.vertical(-1);
        assert_eq!(p.cursor, 5);
        p.delete_word();
        assert_eq!(p.text.iter().collect::<String>(), "one wo\nthree");
        assert_eq!(p.line_end(), 6);
    }
}
