//! Append-only private writes to `output.md`. The controller only appends to
//! the file it created: it never follows a replacement symlink or recreates an
//! output that disappeared during a run.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Appends `content` to the existing regular file at `path`.
pub fn append_private_output(path: &Path, content: &str) -> io::Result<()> {
    let before = std::fs::symlink_metadata(path)?;
    if !before.file_type().is_file() {
        return Err(io::Error::other("output artifact is not a regular file"));
    }
    let mut file = OpenOptions::new().append(true).open(path)?;
    let opened = file.metadata()?;
    if !same_file(&before, &opened) {
        return Err(io::Error::other("output artifact changed while opening"));
    }
    ruddr_core::fsutil::set_mode(path, 0o600)?;
    append_chunk(&mut file, opened.len(), content.as_bytes())
}

#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    // Stable std has no file identity on Windows; the regular-file check
    // above already refused a symlink.
    a.len() == b.len()
}

/// A file that can drop a partial append.
pub trait Truncate {
    fn truncate(&mut self, size: u64) -> io::Result<()>;
}

impl Truncate for File {
    fn truncate(&mut self, size: u64) -> io::Result<()> {
        self.set_len(size)
    }
}

/// Writes `content` and restores the previous length when the write fails
/// partway. An abrupt crash can still leave a partial last item, as with the
/// other logs.
pub fn append_chunk<W: Write + Truncate>(file: &mut W, size: u64, content: &[u8]) -> io::Result<()> {
    if let Err(error) = file.write_all(content) {
        return match file.truncate(size) {
            Ok(()) => Err(error),
            Err(truncate) => Err(io::Error::new(error.kind(), format!("{error}; restore output length: {truncate}"))),
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Partial {
        data: Vec<u8>,
        fail_with_zero: bool,
    }

    impl Write for Partial {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.data.len() >= "first\n".len() + 2 {
                return if self.fail_with_zero {
                    Ok(0)
                } else {
                    Err(io::Error::other("disk full"))
                };
            }
            self.data.extend_from_slice(&buf[..2]);
            Ok(2)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Truncate for Partial {
        fn truncate(&mut self, size: u64) -> io::Result<()> {
            self.data.truncate(size as usize);
            Ok(())
        }
    }

    #[test]
    fn partial_writes_roll_back() {
        for fail_with_zero in [false, true] {
            let mut partial = Partial {
                data: b"first\n".to_vec(),
                fail_with_zero,
            };
            assert!(append_chunk(&mut partial, 6, b"\nsecond\n").is_err());
            assert_eq!(partial.data, b"first\n");
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinks() {
        let dir = std::env::temp_dir().join(format!("ruddr-output-{}", ruddr_core::fsutil::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target.md");
        std::fs::write(&target, "untouched").unwrap();
        let path = dir.join("output.md");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(append_private_output(&path, "private completion").is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
