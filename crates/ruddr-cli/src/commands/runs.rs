//! `status`, `peek`, and `wait` for one run or a group of runs, and the run
//! selection (`--state-dir`, repeatable, and `--root`) that `result`, `stop`,
//! and `interrupt` share. Port of group.go and the single-run commands in
//! main.go.

use super::args::{self, Parsed, Spec, multi};
use super::progress::Progress;
use ruddr_core::state::{RunState, STATE_FILE, Status, read_state};
use ruddr_core::{Error, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How far below a `--root` directory run discovery looks. A swarm layout
/// such as `ROOT/<agent>/run` sits well inside it.
pub const MAX_ROOT_DEPTH: usize = 4;

/// How often `wait` re-reads state.
pub const WAIT_TICK: Duration = Duration::from_millis(250);

/// A process-liveness probe. Commands pass `ruddr_core::process::alive`;
/// tests pass a closure so lifecycle checks stay deterministic.
pub type Alive<'a> = &'a dyn Fn(i64) -> bool;

pub fn process_alive(pid: i64) -> bool {
    ruddr_core::process::alive(pid)
}

/// The flags that select runs.
pub const SELECTION_SPECS: [Spec; 2] = [
    multi("state-dir", "DIR", "Ruddr run state directory (repeatable)"),
    multi("root", "DIR", "act on every run below DIR (repeatable)"),
];

/// One run in a group: its state directory and the label shown in tables
/// (the path relative to its `--root`, or the `--state-dir` as given).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRef {
    pub name: String,
    pub state_dir: PathBuf,
}

/// The repeatable `--state-dir` and `--root` flags.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    pub state_dirs: Vec<String>,
    pub roots: Vec<String>,
}

impl Selection {
    pub fn from_parsed(parsed: &Parsed) -> Result<Selection> {
        let selection = Selection {
            state_dirs: parsed.all("state-dir"),
            roots: parsed.all("root"),
        };
        if selection.state_dirs.iter().chain(&selection.roots).any(String::is_empty) {
            return Err(Error::usage("directory must not be empty"));
        }
        Ok(selection)
    }

    /// `Some` when the flags name at most one run the original way, so the
    /// command keeps its single-run output and exit behavior. The inner value
    /// is the one `--state-dir`, if any.
    pub fn single(&self) -> Option<Option<&str>> {
        if self.roots.is_empty() && self.state_dirs.len() <= 1 {
            return Some(self.state_dirs.first().map(String::as_str));
        }
        None
    }

    /// Every selected run, de-duplicated by absolute path, explicit state
    /// directories first and each root's runs sorted by name.
    pub fn resolve(&self) -> Result<Vec<RunRef>> {
        let mut refs = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut add = |run: RunRef| {
            if seen.insert(ruddr_core::paths::absolute(&run.state_dir)) {
                refs.push(run);
            }
        };
        for dir in &self.state_dirs {
            add(RunRef {
                name: dir.clone(),
                state_dir: PathBuf::from(dir),
            });
        }
        for root in &self.roots {
            let found = discover_runs(Path::new(root))?;
            if found.is_empty() {
                return Err(Error::failed(format!("no runs found below {root}")));
            }
            found.into_iter().for_each(&mut add);
        }
        Ok(refs)
    }
}

/// Finds state directories below `root`, at most [`MAX_ROOT_DEPTH`] levels
/// deep. It does not follow symlinks or descend into a run's own directory.
pub fn discover_runs(root: &Path) -> Result<Vec<RunRef>> {
    let metadata = std::fs::metadata(root).map_err(|e| Error::failed(format!("stat {}: {e}", root.display())))?;
    if !metadata.is_dir() {
        return Err(Error::failed(format!("--root {} is not a directory", root.display())));
    }
    let mut refs = Vec::new();
    walk(root, root, 0, &mut refs);
    refs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(refs)
}

fn walk(root: &Path, dir: &Path, depth: usize, refs: &mut Vec<RunRef>) {
    if dir.join(STATE_FILE).exists() {
        let name = if depth == 0 {
            dir.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.display().to_string())
        } else {
            let relative = dir.strip_prefix(root).unwrap_or(dir);
            relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/")
        };
        refs.push(RunRef {
            name,
            state_dir: dir.to_path_buf(),
        });
        return;
    }
    if depth >= MAX_ROOT_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut children: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|entry| entry.path())
        .collect();
    children.sort();
    for child in children {
        walk(root, &child, depth + 1, refs);
    }
}

/// One run as a group view shows it. A run whose state cannot be read shows
/// as `unreadable` instead of failing the whole command.
#[derive(Debug, Clone)]
pub enum View {
    Run(Box<RunState>),
    Unreadable { state_dir: String, error: String },
}

impl View {
    pub fn status(&self) -> &str {
        match self {
            View::Run(state) => state.status.as_str(),
            View::Unreadable { .. } => "unreadable",
        }
    }

    pub fn state(&self) -> Option<&RunState> {
        match self {
            View::Run(state) => Some(state),
            View::Unreadable { .. } => None,
        }
    }

    pub fn error(&self) -> &str {
        match self {
            View::Run(state) => state.error.as_deref().unwrap_or(""),
            View::Unreadable { error, .. } => error,
        }
    }

    fn is(&self, status: Status) -> bool {
        self.state().is_some_and(|s| s.status == status)
    }

    /// Whether a group wait should stop watching this run.
    pub fn settled(&self) -> bool {
        match self {
            View::Run(state) => state.status.is_terminal() || state.status == Status::Stale,
            View::Unreadable { .. } => true,
        }
    }

    /// Like [`settled`](Self::settled), and with `turn` an idle session also
    /// counts: its latest turn ended.
    pub fn turn_settled(&self, turn: bool) -> bool {
        self.settled() || (turn && self.is(Status::Idle))
    }

    /// Whether a settled run finished its work. An idle session succeeded
    /// when its latest turn completed; controllers that predate
    /// `lastTurnStatus` leave it empty, which counts as success.
    pub fn succeeded(&self) -> bool {
        match self.state() {
            Some(state) if state.status == Status::Idle => matches!(state.last_turn, None | Some(Status::Completed)),
            Some(state) => state.status == Status::Completed,
            None => false,
        }
    }

    /// Whether the run is stale or unreadable: its outcome is unknown.
    pub fn is_dead(&self) -> bool {
        matches!(self, View::Unreadable { .. }) || self.is(Status::Stale)
    }

    /// The ERROR column: the persisted error, or the outcome of an idle
    /// session's failed turn, which idle state does not keep as an error.
    pub fn row_error(&self) -> String {
        if self.error().is_empty() && self.is(Status::Idle) && !self.succeeded() {
            let last = self.state().and_then(|s| s.last_turn).map(|s| s.as_str()).unwrap_or("");
            return format!("last turn {last}");
        }
        self.error().to_string()
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            View::Run(state) => serde_json::to_value(state).unwrap_or(serde_json::Value::Null),
            View::Unreadable { state_dir, error } => {
                serde_json::json!({ "status": "unreadable", "stateDir": state_dir, "error": error })
            }
        }
    }
}

/// Reads one run for a group view.
pub fn read_view(run: &RunRef, alive: Alive) -> View {
    let state = match read_state(&run.state_dir) {
        Ok(state) => state,
        Err(error) => {
            return View::Unreadable {
                state_dir: run.state_dir.display().to_string(),
                error: error.message,
            };
        }
    };
    if !state.status.is_terminal() && !alive(state.pid) {
        // The controller persists its terminal state before exiting; re-read
        // before calling a run stale.
        if let Ok(last) = read_state(&run.state_dir)
            && last.status.is_terminal()
        {
            return View::Run(Box::new(last));
        }
        let mut state = state;
        state.error = Some(format!("Ruddr pid {} is not running; persisted state is stale", state.pid));
        state.status = Status::Stale;
        return View::Run(Box::new(state));
    }
    View::Run(Box::new(state))
}

/// Compacts whitespace to single spaces and cuts the text at `limit` bytes on
/// a character boundary.
pub fn one_line(text: &str, limit: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.len() <= limit {
        return joined;
    }
    let mut cut = limit;
    while cut > 0 && !joined.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &joined[..cut])
}

fn dash(text: &str) -> &str {
    if text.is_empty() { "-" } else { text }
}

pub fn format_token_count(count: i64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}K", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

/// The run's wall time: until now while it runs, until completion (or its
/// last update) once it has settled.
pub fn elapsed(view: &View, now_ms: i64) -> String {
    let Some(state) = view.state() else { return "-".into() };
    let Some(start) = ruddr_core::time::parse_rfc3339_ms(&state.started_at) else {
        return "-".into();
    };
    let end = if view.settled() {
        state
            .completed_at
            .as_deref()
            .and_then(ruddr_core::time::parse_rfc3339_ms)
            .or_else(|| ruddr_core::time::parse_rfc3339_ms(&state.updated_at))
    } else {
        Some(now_ms)
    };
    match end {
        Some(end) if end >= start => {
            let seconds = (end - start + 500) / 1000;
            ruddr_core::duration::format(Duration::from_secs(seconds as u64))
        }
        _ => "-".into(),
    }
}

/// Prints the group table, aligned like Go's tabwriter with two spaces of
/// padding. The last column is not padded.
pub fn print_table(out: &mut dyn Write, refs: &[RunRef], views: &[View], now_ms: i64) -> Result<()> {
    let mut rows = vec![["NAME", "STATUS", "PROVIDER", "MODEL", "TURNS", "TOKENS", "ELAPSED", "ERROR"].map(String::from)];
    for (run, view) in refs.iter().zip(views) {
        let state = view.state();
        let turns = state
            .filter(|s| s.turns > 0)
            .map(|s| s.turns.to_string())
            .unwrap_or_else(|| "-".into());
        let tokens = state
            .and_then(|s| s.token_usage.as_ref())
            .filter(|u| u.total_tokens > 0)
            .map(|u| format_token_count(u.total_tokens))
            .unwrap_or_else(|| "-".into());
        rows.push([
            run.name.clone(),
            view.status().to_string(),
            dash(state.map(|s| s.provider.as_str()).unwrap_or("")).to_string(),
            dash(state.map(|s| s.model.as_str()).unwrap_or("")).to_string(),
            turns,
            tokens,
            elapsed(view, now_ms),
            one_line(&view.row_error(), 100),
        ]);
    }
    let columns = rows[0].len();
    let widths: Vec<usize> = (0..columns - 1)
        .map(|c| rows.iter().map(|row| row[c].chars().count()).max().unwrap_or(0) + 2)
        .collect();
    for row in &rows {
        let mut line = String::new();
        for (c, cell) in row.iter().enumerate() {
            line.push_str(cell);
            if c < columns - 1 {
                line.push_str(&" ".repeat(widths[c] - cell.chars().count()));
            }
        }
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// When a group wait returns.
#[derive(Debug, Clone, Copy, Default)]
pub struct WaitOptions {
    /// Return once a run that was still running when the wait began settles,
    /// so repeated calls hand back runs one at a time.
    pub any: bool,
    /// Count an idle session as settled because its turn ended.
    pub turn: bool,
}

fn timed_out(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|d| Instant::now() > d)
}

/// Polls every run until all settle (or, with `any`, until one that was
/// running settles) or the deadline passes. Prints the table and fails
/// unless every run it reports on succeeded.
pub fn wait_for_runs(
    out: &mut dyn Write,
    refs: &[RunRef],
    deadline: Option<Instant>,
    options: WaitOptions,
    alive: Alive,
    tick: Duration,
    mut progress: Option<&mut Progress<'_>>,
) -> Result<()> {
    let mut views: Vec<Option<View>> = vec![None; refs.len()];
    let mut settled = vec![false; refs.len()];
    let mut already_settled: Option<Vec<bool>> = None;
    loop {
        for (i, run) in refs.iter().enumerate() {
            if !settled[i] {
                let view = read_view(run, alive);
                settled[i] = view.turn_settled(options.turn);
                if let Some(progress) = progress.as_deref_mut() {
                    progress.observe(run, &view, settled[i]);
                }
                views[i] = Some(view);
            }
        }
        let settled_count = settled.iter().filter(|s| **s).count();
        let before = already_settled.get_or_insert_with(|| settled.clone());
        let finished: Vec<usize> = (0..refs.len()).filter(|&i| settled[i] && !before[i]).collect();
        let all_settled = settled_count == refs.len();
        let any_finished = options.any && !finished.is_empty();
        if all_settled || any_finished || timed_out(deadline) {
            let views: Vec<View> = views.into_iter().map(|v| v.expect("every run was read")).collect();
            print_table(out, refs, &views, ruddr_core::time::now_ms())?;
            // --any judges only the runs this wait saw finish; otherwise
            // every settled run counts.
            let judged: Vec<usize> = if any_finished {
                finished.clone()
            } else {
                (0..refs.len()).filter(|&i| settled[i]).collect()
            };
            if any_finished {
                let names: Vec<&str> = finished.iter().map(|&i| refs[i].name.as_str()).collect();
                writeln!(out, "finished: {}", names.join(", "))?;
            }
            let failed = judged.iter().filter(|&&i| !views[i].succeeded()).count();
            let dead = judged.iter().filter(|&&i| views[i].is_dead()).count();
            if !all_settled && !any_finished {
                return Err(Error::running(format!(
                    "wait timed out: {} of {} runs still running",
                    refs.len() - settled_count,
                    refs.len()
                )));
            }
            if dead > 0 {
                return Err(Error::stale(format!(
                    "{failed} of {} runs did not complete; {dead} stale",
                    judged.len()
                )));
            }
            if failed > 0 {
                return Err(Error::failed(format!("{failed} of {} runs did not complete", judged.len())));
            }
            return Ok(());
        }
        std::thread::sleep(progress.as_ref().map_or(tick, |p| p.tick(tick)));
    }
}

/// Polls one run's persisted state until it is terminal (or, with `turn`,
/// idle), the controller disappears, or the deadline passes.
pub fn wait_for_run_state(
    out: &mut dyn Write,
    state_dir: &Path,
    deadline: Option<Instant>,
    turn: bool,
    alive: Alive,
    tick: Duration,
    mut progress: Option<&mut Progress<'_>>,
) -> Result<()> {
    let run = RunRef {
        name: state_dir.display().to_string(),
        state_dir: state_dir.to_path_buf(),
    };
    loop {
        let state = read_state(state_dir)?;
        if let Some(progress) = progress.as_deref_mut() {
            let view = View::Run(Box::new(state.clone()));
            progress.observe(&run, &view, view.turn_settled(turn));
        }
        if state.status.is_terminal() || (turn && state.status == Status::Idle) {
            return report_wait_result(out, &state);
        }
        if !alive(state.pid) {
            // The controller persists its terminal state and only then exits,
            // so a dead pid seen after a non-terminal read may mean the run
            // finished between the two checks. Re-read before calling it stale.
            if let Ok(last) = read_state(state_dir)
                && last.status.is_terminal()
            {
                if let Some(progress) = progress.as_deref_mut() {
                    progress.observe(&run, &View::Run(Box::new(last.clone())), true);
                }
                return report_wait_result(out, &last);
            }
            if let Some(progress) = progress.as_deref_mut() {
                let mut stale = state.clone();
                stale.status = Status::Stale;
                progress.observe(&run, &View::Run(Box::new(stale)), true);
            }
            return Err(Error::stale(format!(
                "Ruddr pid {} is not running; state is stale at status={}",
                state.pid, state.status
            )));
        }
        if timed_out(deadline) {
            return Err(Error::running("wait timed out"));
        }
        std::thread::sleep(progress.as_ref().map_or(tick, |p| p.tick(tick)));
    }
}

/// Prints the final status and maps it to the command's result.
fn report_wait_result(out: &mut dyn Write, state: &RunState) -> Result<()> {
    if state.status == Status::Idle {
        let Some(last) = state.last_turn else {
            writeln!(out, "idle")?;
            return Ok(());
        };
        writeln!(out, "idle (last turn {last})")?;
        if last != Status::Completed {
            return Err(Error::failed(format!("last turn ended with status {last}; see trace.log")));
        }
        return Ok(());
    }
    writeln!(out, "{}", state.status)?;
    if state.status == Status::Completed {
        return Ok(());
    }
    match state.error.as_deref() {
        Some(error) if !error.is_empty() => Err(Error::failed(error)),
        _ => Err(Error::failed(format!("turn ended with status {}", state.status))),
    }
}

/// The last `count` lines of a file.
pub fn tail_lines(path: &Path, count: usize) -> Result<Vec<String>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let size = std::fs::metadata(path)
        .map_err(|e| Error::failed(format!("open {}: {e}", path.display())))?
        .len();
    let mut window = 256 * 1024u64;
    loop {
        let text = ruddr_core::fsutil::read_tail(path, window).map_err(|e| Error::failed(format!("read trace: {e}")))?;
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() >= count || window >= size {
            return Ok(lines[lines.len().saturating_sub(count)..].iter().map(|s| s.to_string()).collect());
        }
        window = window.saturating_mul(4);
    }
}

fn require_state_dir(dir: Option<&str>) -> Result<&Path> {
    match dir {
        Some(dir) => Ok(Path::new(dir)),
        None => Err(Error::usage("--state-dir is required")),
    }
}

pub fn print_json(out: &mut dyn Write, value: &impl serde::Serialize) -> Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    writeln!(out, "{text}")?;
    Ok(())
}

pub fn status(out: &mut dyn Write, argv: Vec<String>) -> Result<()> {
    let mut specs = SELECTION_SPECS.to_vec();
    specs.push(args::flag("json", "print full state as JSON (an array for several runs)"));
    let parsed = args::parse("status", &specs, &argv)?;
    args::no_positionals("status", &parsed)?;
    let selection = Selection::from_parsed(&parsed)?;
    let json = parsed.bool("json");
    let Some(single) = selection.single() else {
        let refs = selection.resolve()?;
        let views: Vec<View> = refs.iter().map(|run| read_view(run, &process_alive)).collect();
        if json {
            let values: Vec<serde_json::Value> = views.iter().map(View::to_json).collect();
            return print_json(out, &values);
        }
        return print_table(out, &refs, &views, ruddr_core::time::now_ms());
    };
    let state = read_state(require_state_dir(single)?)?.displayed();
    if json {
        return print_json(out, &state);
    }
    writeln!(
        out,
        "{} provider={} thread={} turn={} pid={} steers={}",
        state.status,
        state.provider,
        state.thread_id.as_deref().unwrap_or(""),
        state.turn_id.as_deref().unwrap_or(""),
        state.pid,
        state.steers
    )?;
    if let Some(error) = state.error.as_deref().filter(|e| !e.is_empty()) {
        writeln!(out, "error: {error}")?;
    }
    Ok(())
}

pub fn peek(out: &mut dyn Write, argv: Vec<String>) -> Result<()> {
    let mut specs = SELECTION_SPECS.to_vec();
    specs.push(args::value("n", "N", "number of trace lines (5 per run with several runs)").with_short('n'));
    let parsed = args::parse("peek", &specs, &argv)?;
    args::no_positionals("peek", &parsed)?;
    let selection = Selection::from_parsed(&parsed)?;
    let Some(single) = selection.single() else {
        let count = parsed.int("n", 5)?.max(0) as usize;
        let refs = selection.resolve()?;
        return group_peek(out, &refs, count, &process_alive);
    };
    let count = parsed.int("n", 25)?.max(0) as usize;
    let state = read_state(require_state_dir(single)?)?;
    for line in tail_lines(Path::new(&state.trace_path), count)? {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// Prints the latest trace lines of every run under a name header.
pub fn group_peek(out: &mut dyn Write, refs: &[RunRef], count: usize, alive: Alive) -> Result<()> {
    for (i, run) in refs.iter().enumerate() {
        if i > 0 {
            writeln!(out)?;
        }
        let view = read_view(run, alive);
        writeln!(out, "== {}: {} ==", run.name, view.status())?;
        let Some(trace) = view.state().map(|s| s.trace_path.clone()).filter(|p| !p.is_empty()) else {
            continue;
        };
        match tail_lines(Path::new(&trace), count) {
            Ok(lines) => {
                for line in lines {
                    writeln!(out, "{line}")?;
                }
            }
            Err(error) => writeln!(out, "(trace unavailable: {error})")?,
        }
    }
    Ok(())
}

pub fn wait(out: &mut dyn Write, argv: Vec<String>) -> Result<()> {
    let mut specs = SELECTION_SPECS.to_vec();
    specs.push(args::value("timeout", "DURATION", "maximum wait, such as 10m; zero means no limit"));
    specs.push(args::value(
        "progress",
        "DURATION",
        "report progress to stderr, such as 1m; must be positive",
    ));
    specs.push(args::flag("any", "with several runs, return when the next running one finishes"));
    specs.push(args::flag(
        "turn",
        "return when the current turn ends; an idle session counts as done",
    ));
    let parsed = args::parse("wait", &specs, &argv)?;
    args::no_positionals("wait", &parsed)?;
    let selection = Selection::from_parsed(&parsed)?;
    let timeout = parsed.duration("timeout", Duration::ZERO)?;
    let deadline = (!timeout.is_zero()).then(|| Instant::now() + timeout);
    let interval = parsed.duration("progress", Duration::ZERO)?;
    if parsed.string("progress").is_some() && interval.is_zero() {
        return Err(Error::usage("--progress must be positive"));
    }
    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let mut progress = (!interval.is_zero()).then(|| Progress::new(&mut stderr, interval));
    let options = WaitOptions {
        any: parsed.bool("any"),
        turn: parsed.bool("turn"),
    };
    match selection.single() {
        None => {
            let refs = selection.resolve()?;
            wait_for_runs(out, &refs, deadline, options, &process_alive, WAIT_TICK, progress.as_mut())
        }
        Some(single) => {
            let dir = require_state_dir(single)?;
            wait_for_run_state(out, dir, deadline, options.turn, &process_alive, WAIT_TICK, progress.as_mut())
        }
    }
}
