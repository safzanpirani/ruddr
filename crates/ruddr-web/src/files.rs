//! Reads of run artifacts. Every open refuses symbolic links and FIFOs
//! (`O_NOFOLLOW | O_NONBLOCK`) and checks that it got a regular file. The
//! event stream reads with byte offsets, aligns on record boundaries before
//! decoding UTF-8, and pins the log's identity so a rotation between a size
//! probe and a read cannot splice two files together.

pub use ruddr_core::fsutil::directory_identity;
use ruddr_core::fsutil::file_identity;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// The event stream loads at most this much on a reset, and skips records
/// longer than this.
pub const EVENTS_INITIAL_BYTES: u64 = 6 * 1024 * 1024;
/// The most bytes one stream tick reads.
pub const EVENTS_CHUNK_BYTES: u64 = 4 * 1024 * 1024;

/// Opens `path` read-only without following a final symbolic link and
/// without blocking on a FIFO.
pub fn open_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(io::Error::other("refusing to follow a symbolic link"));
    }
    options.open(path)
}

fn open_regular(path: &Path, what: &str) -> io::Result<(File, Metadata)> {
    let file = open_no_follow(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other(format!("The {what} must be a regular file")));
    }
    Ok((file, metadata))
}

fn read_at(file: &mut File, offset: u64, length: u64) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = Vec::with_capacity(length as usize);
    file.take(length).read_to_end(&mut buffer)?;
    Ok(buffer)
}

/// The last `max_bytes` of a run artifact, or "" when it does not exist.
pub fn read_artifact_tail(path: &Path, max_bytes: u64) -> io::Result<String> {
    let (mut file, metadata) = match open_regular(path, "run artifact") {
        Ok(opened) => opened,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(e),
    };
    let size = metadata.len();
    let start = size.saturating_sub(max_bytes);
    Ok(String::from_utf8_lossy(&read_at(&mut file, start, size - start)?).into_owned())
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlignedTail {
    /// Complete records, decoded.
    pub text: String,
    /// The byte offset just past what was read.
    pub offset: u64,
    /// Whether the tail started after the beginning of the file.
    pub truncated: bool,
    /// The unfinished last record, kept as bytes.
    pub pending: Vec<u8>,
    /// The tail began inside a record that has not ended yet.
    pub skipping: bool,
    pub identity: String,
}

/// Reads the last `max_bytes` of an event log, aligned to record boundaries
/// before decoding, keeping the unfinished last record as bytes.
pub fn read_aligned_tail(path: &Path, max_bytes: u64) -> io::Result<AlignedTail> {
    let (mut file, metadata) = open_regular(path, "event log")?;
    let size = metadata.len();
    let start = size.saturating_sub(max_bytes);
    let bytes = read_at(&mut file, start, size - start)?;
    // A tail that starts right after a newline starts on a record.
    let partial_start = start > 0 && read_at(&mut file, start - 1, 1)?.first() != Some(&b'\n');
    let first = if partial_start {
        bytes.iter().position(|&b| b == b'\n')
    } else {
        None
    };
    let skipping = partial_start && first.is_none();
    let aligned: &[u8] = match (partial_start, first) {
        (false, _) => &bytes,
        (true, Some(first)) => &bytes[first + 1..],
        (true, None) => &[],
    };
    let complete = aligned.iter().rposition(|&b| b == b'\n').map_or(0, |last| last + 1);
    Ok(AlignedTail {
        text: String::from_utf8_lossy(&aligned[..complete]).into_owned(),
        offset: start + bytes.len() as u64,
        truncated: start > 0,
        pending: aligned[complete..].to_vec(),
        skipping,
        identity: file_identity(&file)?,
    })
}

/// Reads `from..to` of the event log only if it is still the file named by
/// `identity` and has not shrunk below `from`. `None` means it was replaced.
pub fn read_range(path: &Path, from: u64, to: u64, expected_identity: &str) -> io::Result<Option<Vec<u8>>> {
    let (mut file, metadata) = open_regular(path, "event log")?;
    if file_identity(&file)? != expected_identity || metadata.len() < from {
        return Ok(None);
    }
    Ok(Some(read_at(&mut file, from, to.saturating_sub(from))?))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Consumed {
    /// Complete records within the limit, decoded.
    pub text: String,
    pub pending: Vec<u8>,
    pub skipping: bool,
    /// At least one record exceeded the limit and was dropped.
    pub oversized: bool,
}

/// Appends `chunk` to the unfinished record and returns the complete records.
/// Records longer than `max_bytes`, finished or not, are skipped.
pub fn consume_event_bytes(pending: &[u8], skipping: bool, chunk: &[u8], max_bytes: u64) -> Consumed {
    let mut chunk = chunk;
    if skipping {
        match chunk.iter().position(|&b| b == b'\n') {
            Some(first) => chunk = &chunk[first + 1..],
            None => {
                return Consumed {
                    text: String::new(),
                    pending: Vec::new(),
                    skipping: true,
                    oversized: false,
                };
            }
        }
    }
    let mut combined = Vec::with_capacity(pending.len() + chunk.len());
    combined.extend_from_slice(pending);
    combined.extend_from_slice(chunk);
    let max = max_bytes as usize;
    let mut kept: Vec<u8> = Vec::new();
    let mut oversized = false;
    let mut start = 0;
    let mut kept_start = 0;
    while let Some(relative) = combined[start..].iter().position(|&b| b == b'\n') {
        let end = start + relative + 1;
        if end - start > max {
            kept.extend_from_slice(&combined[kept_start..start]);
            kept_start = end;
            oversized = true;
        }
        start = end;
    }
    kept.extend_from_slice(&combined[kept_start..start]);
    let mut pending = combined[start..].to_vec();
    let skipping = pending.len() > max;
    if skipping {
        pending.clear();
        oversized = true;
    }
    Consumed {
        text: String::from_utf8_lossy(&kept).into_owned(),
        pending,
        skipping,
        oversized,
    }
}

/// What one tick of the event stream sends.
#[derive(Debug, Clone, PartialEq)]
pub enum TailEvent {
    Reset { text: String, truncated: bool },
    Append { text: String },
    Problem { error: String },
}

impl TailEvent {
    pub fn name(&self) -> &'static str {
        match self {
            TailEvent::Reset { .. } => "reset",
            TailEvent::Append { .. } => "append",
            TailEvent::Problem { .. } => "problem",
        }
    }

    pub fn data(&self) -> serde_json::Value {
        match self {
            TailEvent::Reset { text, truncated } => serde_json::json!({ "text": text, "truncated": truncated }),
            TailEvent::Append { text } => serde_json::json!({ "text": text }),
            TailEvent::Problem { error } => serde_json::json!({ "error": error }),
        }
    }
}

pub const OVERSIZED_PROBLEM: &str = "An event exceeded the 6 MiB record limit and was skipped";

/// Follows one `events.jsonl`: a reset with the bounded tail first, then
/// appends of whole records, and another reset after rotation or truncation.
#[derive(Debug, Default)]
pub struct EventTail {
    offset: u64,
    identity: String,
    pending: Vec<u8>,
    skipping: bool,
}

/// Lets a tick re-check that its directory is still the verified one after
/// each read. Returning false ends the stream.
pub type Verify<'a> = &'a dyn Fn() -> bool;

#[derive(Debug, PartialEq)]
pub enum Tick {
    Events(Vec<TailEvent>),
    /// The directory no longer verifies; close the stream.
    Close,
}

impl EventTail {
    pub fn tick(&mut self, path: &Path, verify: Verify) -> Tick {
        if !verify() {
            return Tick::Close;
        }
        match self.try_tick(path, verify) {
            Ok(tick) => tick,
            Err(e) => Tick::Events(vec![TailEvent::Problem { error: e.to_string() }]),
        }
    }

    fn try_tick(&mut self, path: &Path, verify: Verify) -> io::Result<Tick> {
        let (file, metadata) = match open_regular(path, "event log") {
            Ok(opened) => opened,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if self.offset == 0 && self.identity.is_empty() {
                    self.identity = "missing".into();
                    return Ok(Tick::Events(vec![TailEvent::Reset {
                        text: String::new(),
                        truncated: false,
                    }]));
                }
                return Ok(Tick::Events(Vec::new()));
            }
            Err(e) => return Err(e),
        };
        let identity = file_identity(&file)?;
        drop(file);
        if identity != self.identity || metadata.len() < self.offset {
            let tail = read_aligned_tail(path, EVENTS_INITIAL_BYTES)?;
            if !verify() {
                return Ok(Tick::Close);
            }
            self.identity = tail.identity;
            self.offset = tail.offset;
            self.pending = tail.pending;
            self.skipping = tail.skipping;
            return Ok(Tick::Events(vec![TailEvent::Reset {
                text: tail.text,
                truncated: tail.truncated,
            }]));
        }
        if metadata.len() == self.offset {
            return Ok(Tick::Events(Vec::new()));
        }
        let to = metadata.len().min(self.offset + EVENTS_CHUNK_BYTES);
        let Some(chunk) = read_range(path, self.offset, to, &self.identity)? else {
            // Replaced between the probe and the read: reset on the next tick.
            self.identity.clear();
            return Ok(Tick::Events(Vec::new()));
        };
        if !verify() {
            return Ok(Tick::Close);
        }
        self.offset += chunk.len() as u64;
        let next = consume_event_bytes(&self.pending, self.skipping, &chunk, EVENTS_INITIAL_BYTES);
        self.pending = next.pending;
        self.skipping = next.skipping;
        let mut events = Vec::new();
        if !next.text.is_empty() {
            events.push(TailEvent::Append { text: next.text });
        }
        if next.oversized {
            events.push(TailEvent::Problem {
                error: OVERSIZED_PROBLEM.into(),
            });
        }
        Ok(Tick::Events(events))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn replacement_resets_equal_and_larger_logs_and_discards_pending_bytes() {
        for (replacement, text, pending) in [
            ("new\ntwo", "new\n", "two\n"),
            ("replacement\ncomplete\n", "replacement\ncomplete\n", "\n"),
        ] {
            let dir = std::env::temp_dir().join(format!("ruddr-web-rotation-{}", ruddr_core::fsutil::random_hex(6)));
            std::fs::create_dir(&dir).unwrap();
            let path = dir.join("events.jsonl");
            std::fs::write(&path, "old\npar").unwrap();
            let initial = read_aligned_tail(&path, 100).unwrap();
            let mut tail = EventTail::default();
            assert_eq!(
                tail.tick(&path, &|| true),
                Tick::Events(vec![TailEvent::Reset {
                    text: "old\n".into(),
                    truncated: false,
                }])
            );
            assert_eq!(tail.tick(&path, &|| true), Tick::Events(vec![]));
            // All read handles have closed before rotation. Retain the old ID.
            std::fs::rename(&path, dir.join("old.jsonl")).unwrap();
            std::fs::write(&path, replacement).unwrap();
            assert_eq!(
                read_range(&path, initial.offset, initial.offset + 4, &initial.identity).unwrap(),
                None
            );
            assert_eq!(
                tail.tick(&path, &|| true),
                Tick::Events(vec![TailEvent::Reset {
                    text: text.into(),
                    truncated: false,
                }])
            );
            OpenOptions::new().append(true).open(&path).unwrap().write_all(b"\n").unwrap();
            assert_eq!(
                tail.tick(&path, &|| true),
                Tick::Events(vec![TailEvent::Append { text: pending.into() }])
            );
            std::fs::write(&path, "x\n").unwrap();
            assert_eq!(
                tail.tick(&path, &|| true),
                Tick::Events(vec![TailEvent::Reset {
                    text: "x\n".into(),
                    truncated: false,
                }])
            );
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
