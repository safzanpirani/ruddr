//! Optional stderr observations for waits. Reporting never changes a wait's result.

use super::runs::{RunRef, View, elapsed};
use ruddr_core::state::{EVENTS_FILE, TRACE_FILE};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const TAIL_BYTES: u64 = 4096;

#[derive(PartialEq)]
struct LogSnapshot {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

#[derive(Default)]
struct Observation {
    status: String,
    emitted: Option<Instant>,
    logs: [Option<LogSnapshot>; 2],
    activity: Option<SystemTime>,
}

pub struct Progress<'a> {
    out: &'a mut dyn Write,
    interval: Duration,
    runs: HashMap<PathBuf, Observation>,
}

impl<'a> Progress<'a> {
    pub fn new(out: &'a mut dyn Write, interval: Duration) -> Self {
        Self {
            out,
            interval,
            runs: HashMap::new(),
        }
    }

    pub fn tick(&self, tick: Duration) -> Duration {
        tick.min(self.interval)
    }

    pub fn observe(&mut self, run: &RunRef, view: &View, settled: bool) {
        let now = Instant::now();
        let wall = SystemTime::now();
        let observation = self.runs.entry(run.state_dir.clone()).or_default();
        let state = view.state();
        let paths = [
            log_path(&run.state_dir, state.map(|s| s.trace_path.as_str()), TRACE_FILE),
            log_path(&run.state_dir, state.map(|s| s.events_path.as_str()), EVENTS_FILE),
        ];
        for (index, path) in paths.iter().enumerate() {
            if let Ok(meta) = std::fs::metadata(path) {
                let modified = meta.modified().ok();
                let snapshot = LogSnapshot {
                    path: path.clone(),
                    size: meta.len(),
                    modified,
                };
                if let Some(previous) = &observation.logs[index] {
                    if previous != &snapshot {
                        observation.activity = Some(wall);
                    }
                } else if let Some(modified) = modified {
                    observation.activity = Some(observation.activity.map_or(modified, |old| old.max(modified)));
                }
                observation.logs[index] = Some(snapshot);
            }
        }
        let changed = !observation.status.is_empty() && observation.status != view.status();
        let due = observation.emitted.is_none_or(|last| now.duration_since(last) >= self.interval);
        observation.status = view.status().into();
        if !changed && (settled || !due) {
            return;
        }
        observation.emitted = Some(now);
        let activity = observation
            .activity
            .and_then(|last| wall.duration_since(last).ok())
            .map(ruddr_core::duration::format)
            .unwrap_or_else(|| "unknown".into());
        let turns = state.map(|s| s.turns.to_string()).unwrap_or_else(|| "-".into());
        let trace = last_trace_line(&paths[0]).unwrap_or_else(|| "unavailable".into());
        // Broken stderr must not change stdout or the lifecycle exit code.
        let _ = writeln!(
            self.out,
            "progress: {} status={} turns={} elapsed={} activity={} trace={}",
            clean_line(&run.name, 512),
            view.status(),
            turns,
            elapsed(view, ruddr_core::time::now_ms()),
            activity,
            trace
        );
        let _ = self.out.flush();
    }
}

fn log_path(dir: &Path, configured: Option<&str>, fallback: &str) -> PathBuf {
    configured
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join(fallback))
}

fn clean_line(text: &str, limit: usize) -> String {
    let mut chars = text.chars().filter(|c| !c.is_control());
    let mut line: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        line.push('…');
    }
    line
}

fn last_trace_line(path: &Path) -> Option<String> {
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let start = size.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    // read_tail currently reads to EOF after seeking. Take also bounds a file
    // that keeps growing, and retains a suffix of a single oversized line.
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    text.lines().last().map(|line| clean_line(line, 120))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_tail_is_bounded_and_strips_terminal_controls() {
        let path = std::env::temp_dir().join(format!("ruddr-progress-{}", ruddr_core::fsutil::random_hex(4)));
        std::fs::write(&path, format!("{}\n\u{1b}latest\t{}\n", "x".repeat(100_000), "界".repeat(200))).unwrap();
        let line = last_trace_line(&path).unwrap();
        assert!(line.starts_with("latest"));
        assert_eq!(line.chars().count(), 121);
        assert!(!line.chars().any(char::is_control));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn activity_tracks_mtime_and_growth_and_stops_reporting_settled_runs() {
        use std::fs::FileTimes;
        let dir = std::env::temp_dir().join(format!("ruddr-activity-{}", ruddr_core::fsutil::random_hex(4)));
        std::fs::create_dir(&dir).unwrap();
        let events = dir.join(EVENTS_FILE);
        std::fs::write(&events, "old").unwrap();
        let old = SystemTime::now() - Duration::from_secs(300);
        File::options()
            .write(true)
            .open(&events)
            .unwrap()
            .set_times(FileTimes::new().set_modified(old))
            .unwrap();
        let old = std::fs::metadata(&events).unwrap().modified().unwrap();
        let state = serde_json::from_value(serde_json::json!({
            "version": 2, "pid": 1, "status": "active", "stateDir": dir,
            "startedAt": "2026-10-02T09:00:00Z", "updatedAt": "2026-10-02T09:00:00Z"
        }))
        .unwrap();
        let mut view = View::Run(Box::new(state));
        let run = RunRef {
            name: "test".into(),
            state_dir: dir.clone(),
        };
        let mut output = Vec::new();
        let mut progress = Progress::new(&mut output, Duration::from_secs(3600));
        progress.observe(&run, &view, false);
        assert_eq!(progress.runs[&dir].activity, Some(old));
        std::fs::write(&events, "new content").unwrap();
        // Same mtime, different size: the observed growth still counts.
        File::options()
            .write(true)
            .open(&events)
            .unwrap()
            .set_times(FileTimes::new().set_modified(old))
            .unwrap();
        progress.observe(&run, &view, false);
        assert!(progress.runs[&dir].activity.unwrap() > old);
        if let View::Run(state) = &mut view {
            state.status = ruddr_core::state::Status::Completed;
        }
        progress.observe(&run, &view, true);
        progress.observe(&run, &view, true);
        drop(progress);
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.lines().count(), 2, "{output}");
        assert!(output.contains("status=completed"));
        assert!(output.contains("activity=5m"), "{output}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
