//! A reader thread per selected session. It keeps a byte offset into each
//! artifact file (`events.jsonl`, `trace.log`, `output.md`), reads only the
//! bytes appended since the last look, and sends complete lines over the UI
//! channel. A partial last line waits until its newline arrives. The first
//! read starts at most [`HISTORY_BYTES`] from the end, on a line boundary.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

/// How much history the first read loads per file.
pub const HISTORY_BYTES: u64 = 1024 * 1024;
/// The most one read hands over at a time, so a burst stays responsive.
const CHUNK_BYTES: u64 = 512 * 1024;
const FAST_POLL: Duration = Duration::from_millis(25);
const SLOW_POLL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    Events,
    Trace,
    Output,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    /// Which tailer sent it; batches from a replaced tailer are dropped.
    pub generation: u64,
    pub source: Source,
    /// The file shrank or was replaced: drop what was read before.
    pub reset: bool,
    /// The last batch of the initial history read.
    pub history_done: bool,
    pub lines: Vec<String>,
}

/// Incremental line reader over one file.
#[derive(Debug)]
pub struct FileTail {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
    started: bool,
    identity: Option<FileIdentity>,
    /// The history window began mid-record: drop bytes up to the next newline.
    skipping: bool,
}

#[cfg(unix)]
type FileIdentity = (u64, u64);
#[cfg(not(unix))]
type FileIdentity = Option<std::time::SystemTime>;

fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    }
    #[cfg(not(unix))]
    {
        metadata.created().ok()
    }
}

impl FileTail {
    pub fn new(path: PathBuf) -> FileTail {
        FileTail {
            path,
            offset: 0,
            partial: Vec::new(),
            started: false,
            identity: None,
            skipping: false,
        }
    }

    /// Reads what was appended. Returns `(reset, lines, caught_up)`; lines
    /// exclude a trailing partial line. `caught_up` is false when more bytes
    /// remain past this chunk.
    pub fn poll(&mut self) -> std::io::Result<(bool, Vec<String>, bool)> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((false, Vec::new(), true)),
            Err(e) => return Err(e),
        };
        let metadata = file.metadata()?;
        let size = metadata.len();
        let identity = file_identity(&metadata);
        let mut reset = false;
        if size < self.offset || self.identity.as_ref().is_some_and(|previous| *previous != identity) {
            // Truncated or replaced: start over.
            self.offset = 0;
            self.partial.clear();
            self.skipping = false;
            reset = true;
        }
        self.identity = Some(identity);
        if !self.started {
            self.started = true;
            if size > HISTORY_BYTES {
                // Keep the first line only when the byte before the window
                // is a newline.
                let start = size - HISTORY_BYTES;
                file.seek(SeekFrom::Start(start - 1))?;
                let mut before = [0u8; 1];
                file.read_exact(&mut before)?;
                self.offset = start;
                self.skipping = before[0] != b'\n';
            }
        }
        if size == self.offset {
            return Ok((reset, Vec::new(), true));
        }
        let want = (size - self.offset).min(CHUNK_BYTES);
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buffer = Vec::with_capacity(want as usize);
        file.by_ref().take(want).read_to_end(&mut buffer)?;
        self.offset += buffer.len() as u64;
        let caught_up = self.offset >= size;
        let mut data = std::mem::take(&mut self.partial);
        data.extend_from_slice(&buffer);
        if self.skipping {
            match data.iter().position(|b| *b == b'\n') {
                Some(index) => {
                    data.drain(..=index);
                    self.skipping = false;
                }
                None => return Ok((reset, Vec::new(), caught_up)),
            }
        }
        let complete = data.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
        self.partial = data.split_off(complete);
        let lines = data[..complete.saturating_sub(1).min(data.len())]
            .split(|b| *b == b'\n')
            .filter(|_| complete > 0)
            .map(|line| String::from_utf8_lossy(line.strip_suffix(b"\r").unwrap_or(line)).into_owned())
            .collect();
        Ok((reset, lines, caught_up))
    }
}

/// The background reader for one session. Dropping it stops the thread at
/// its next poll.
pub struct Tailer {
    stop: Arc<AtomicBool>,
}

impl Drop for Tailer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Tailer {
    /// Starts the thread. `send` gets every batch; when it returns false the
    /// receiver is gone and the thread exits.
    pub fn spawn<M: Send + 'static>(generation: u64, files: Vec<(Source, PathBuf)>, tx: Sender<M>, wrap: fn(Batch) -> M) -> Tailer {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::Builder::new()
            .name("ruddr-tui-tail".into())
            .spawn(move || run(generation, files, flag, tx, wrap))
            .expect("spawn the artifact reader thread");
        Tailer { stop }
    }
}

fn run<M>(generation: u64, files: Vec<(Source, PathBuf)>, stop: Arc<AtomicBool>, tx: Sender<M>, wrap: fn(Batch) -> M) {
    let mut tails: Vec<(Source, FileTail, bool)> = files.into_iter().map(|(s, p)| (s, FileTail::new(p), false)).collect();
    let mut idle = Duration::ZERO;
    while !stop.load(Ordering::Relaxed) {
        let mut busy = false;
        for (source, tail, history_done) in &mut tails {
            let (reset, lines, caught_up) = match tail.poll() {
                Ok(result) => result,
                Err(_) => (false, Vec::new(), true),
            };
            let finishing_history = !*history_done && caught_up;
            if lines.is_empty() && !reset && !finishing_history {
                busy |= !caught_up;
                continue;
            }
            busy = true;
            if finishing_history {
                *history_done = true;
            }
            let batch = Batch {
                generation,
                source: *source,
                reset,
                history_done: finishing_history,
                lines,
            };
            if tx.send(wrap(batch)).is_err() {
                return;
            }
            busy |= !caught_up;
        }
        // Poll fast while data flows and back off while the files sit still.
        idle = if busy { Duration::ZERO } else { (idle + FAST_POLL).min(SLOW_POLL) };
        std::thread::sleep(if busy { Duration::from_millis(5) } else { idle.max(FAST_POLL) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ruddr-tui-tail-{name}-{}", ruddr_core::fsutil::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("events.jsonl")
    }

    fn append(path: &PathBuf, text: &str) {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }

    #[test]
    fn hands_over_complete_lines_only() {
        let path = temp("lines");
        let mut tail = FileTail::new(path.clone());
        assert_eq!(tail.poll().unwrap(), (false, vec![], true), "a missing file is just empty");
        append(&path, "one\ntw");
        assert_eq!(tail.poll().unwrap(), (false, vec!["one".to_string()], true));
        assert_eq!(tail.poll().unwrap().1, Vec::<String>::new());
        append(&path, "o\r\nthree\n");
        assert_eq!(tail.poll().unwrap().1, vec!["two".to_string(), "three".to_string()]);
        std::fs::write(&path, "new\n").unwrap();
        assert_eq!(tail.poll().unwrap(), (true, vec!["new".to_string()], true), "truncation resets");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn history_starts_on_a_line_boundary() {
        let path = temp("history");
        let line = format!("{}\n", "x".repeat(1000));
        let count = (HISTORY_BYTES as usize / line.len()) + 50;
        let mut text = String::new();
        for i in 0..count {
            text.push_str(&format!("{i:05}{}", &line[5..]));
        }
        std::fs::write(&path, &text).unwrap();
        let mut tail = FileTail::new(path.clone());
        let mut lines = Vec::new();
        loop {
            let (_, mut batch, caught_up) = tail.poll().unwrap();
            lines.append(&mut batch);
            if caught_up {
                break;
            }
        }
        assert!(lines.len() < count && lines.len() >= count - 51, "{}", lines.len());
        assert!(lines.iter().all(|l| l.len() == 1000), "no partial first record");
        assert_eq!(lines.last().unwrap()[..5], format!("{:05}", count - 1));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn replacement_resets_even_when_it_does_not_shrink() {
        for (replacement, expected, pending) in [
            ("new\ntwo", vec!["new"], "two"),
            ("replacement\ncomplete\n", vec!["replacement", "complete"], ""),
        ] {
            let path = temp("replacement");
            std::fs::write(&path, "old\npar").unwrap();
            let mut tail = FileTail::new(path.clone());
            assert_eq!(tail.poll().unwrap(), (false, vec!["old".into()], true));
            // Keep the old inode alive so an immediate inode reuse cannot
            // hide the replacement from the test.
            std::fs::rename(&path, path.with_extension("old")).unwrap();
            std::fs::write(&path, replacement).unwrap();
            let (reset, lines, caught_up) = tail.poll().unwrap();
            assert!(reset);
            assert!(caught_up);
            assert_eq!(lines, expected);
            append(&path, "\n");
            assert_eq!(tail.poll().unwrap().1, vec![pending]);
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn the_thread_streams_appends() {
        let path = temp("thread");
        append(&path, "old\n");
        let (tx, rx) = std::sync::mpsc::channel();
        let tailer = Tailer::spawn(7, vec![(Source::Events, path.clone())], tx, |b| b);
        let first = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            (first.generation, first.history_done, first.lines.clone()),
            (7, true, vec!["old".to_string()])
        );
        append(&path, "new\n");
        let next = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!((next.history_done, next.lines), (false, vec!["new".to_string()]));
        drop(tailer);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
