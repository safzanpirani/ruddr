//! Owner-only file helpers. Run directories and socket parents are 0700;
//! content-bearing files and state files are 0600. On Windows, access comes
//! from the parent directory's NTFS ACL, and these helpers do not set ACLs.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Identifies the file behind an open handle. Appends leave this unchanged.
/// Windows creation times are not identities: NTFS can preserve them across
/// replacement through file tunneling. Query the volume and file index instead.
pub fn file_identity(file: &File) -> io::Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok(format!("{}:{}", metadata.ino(), metadata.dev()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: file owns a live handle and info is writable for the call.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        Ok(format!("{index}:{}", info.dwVolumeSerialNumber))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(io::Error::new(io::ErrorKind::Unsupported, "file identity is unavailable"))
    }
}

/// Identifies a directory through an open handle, refusing a final symlink.
pub fn directory_identity(path: &Path) -> io::Result<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT};
        // Directory handles need BACKUP_SEMANTICS. Inspect the link itself.
        options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::other("refusing to follow a symbolic link"));
    }
    if !metadata.is_dir() {
        return Err(io::Error::other("not a directory"));
    }
    file_identity(&file)
}

/// Creates `dir` (and parents) and forces the leaf to 0700.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    set_mode(dir, 0o700)
}

/// Creates a new directory that must not exist yet, as 0700.
pub fn create_private_dir_new(dir: &Path) -> io::Result<()> {
    #[cfg_attr(not(unix), allow(unused_mut))] // the mode is set on Unix only
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)?;
    set_mode(dir, 0o700)
}

pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

fn private_options() -> OpenOptions {
    #[cfg_attr(not(unix), allow(unused_mut))] // the mode is set on Unix only
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

/// Opens a new 0600 file, failing if it exists.
pub fn create_private_file_new(path: &Path) -> io::Result<File> {
    private_options().write(true).create_new(true).open(path)
}

/// Opens (creating if needed) a 0600 file for appending.
pub fn open_private_append(path: &Path) -> io::Result<File> {
    let file = private_options().append(true).create(true).open(path)?;
    set_mode(path, 0o600)?;
    Ok(file)
}

/// Writes `data` to a sibling temporary file and renames it into place, so
/// readers never see a partial file. The result is 0600.
pub fn write_private_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let temporary = path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), random_hex(4)));
    let result = (|| {
        let mut file = private_options().write(true).create_new(true).open(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Hex of `bytes` random bytes from the OS.
pub fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    if getrandom::fill(&mut buffer).is_err() {
        // Falls back to time and pid; only used for unique names.
        let seed = crate::time::now_ms() as u64 ^ (std::process::id() as u64).rotate_left(32);
        for (i, b) in buffer.iter_mut().enumerate() {
            *b = (seed >> ((i % 8) * 8)) as u8;
        }
    }
    buffer.iter().map(|b| format!("{b:02x}")).collect()
}

/// Reads at most `max_bytes` from the end of a file, starting on a line
/// boundary so a bounded tail never begins mid-record.
pub fn read_tail(path: &Path, max_bytes: u64) -> io::Result<String> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let start = size.saturating_sub(max_bytes);
    let read_start = start.saturating_sub(1);
    file.seek(SeekFrom::Start(read_start))?;
    let mut buffer = Vec::with_capacity((size - read_start) as usize);
    file.read_to_end(&mut buffer)?;
    let on_boundary = start == 0 || buffer.first() == Some(&b'\n');
    let body = if start == 0 { &buffer[..] } else { &buffer[1..] };
    let mut text = String::from_utf8_lossy(body).into_owned();
    if !on_boundary {
        text = match text.find('\n') {
            Some(index) => text[index + 1..].to_string(),
            None => String::new(),
        };
    }
    Ok(text)
}

/// The last `count` lines of a file.
pub fn tail_lines(path: &Path, count: usize) -> io::Result<Vec<String>> {
    let text = read_tail(path, 256 * 1024)?;
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    Ok(lines[lines.len().saturating_sub(count)..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ruddr-core-{name}-{}", random_hex(4)));
        create_private_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn identity_survives_appends_but_changes_on_replacement() {
        let dir = temp_dir("identity");
        let path = dir.join("events.jsonl");
        fs::write(&path, b"old\n").unwrap();
        let identity = file_identity(&File::open(&path).unwrap()).unwrap();
        OpenOptions::new().append(true).open(&path).unwrap().write_all(b"more\n").unwrap();
        assert_eq!(file_identity(&File::open(&path).unwrap()).unwrap(), identity);
        // Keep the old file allocated so its index cannot be reused.
        fs::rename(&path, dir.join("old.jsonl")).unwrap();
        fs::write(&path, b"new\nmore\n").unwrap();
        assert_ne!(file_identity(&File::open(&path).unwrap()).unwrap(), identity);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_identity_distinguishes_equal_creation_and_write_times() {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::Storage::FileSystem::SetFileTime;

        let dir = temp_dir("equal-times");
        let path = dir.join("events.jsonl");
        let time = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 30_000_000,
        };
        let write = |bytes: &[u8]| {
            fs::write(&path, bytes).unwrap();
            let file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
            // SAFETY: file owns a live handle; both timestamp pointers are valid.
            assert_ne!(unsafe { SetFileTime(file.as_raw_handle(), &time, std::ptr::null(), &time) }, 0);
        };
        write(b"old\n");
        let old = file_identity(&File::open(&path).unwrap()).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        fs::rename(&path, dir.join("old.jsonl")).unwrap();
        write(b"new\n");
        let replacement = fs::metadata(&path).unwrap();
        assert_eq!(metadata.created().unwrap(), replacement.created().unwrap());
        assert_eq!(metadata.modified().unwrap(), replacement.modified().unwrap());
        assert_eq!(metadata.len(), replacement.len());
        assert_ne!(file_identity(&File::open(&path).unwrap()).unwrap(), old);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn directory_identity_changes_on_replacement() {
        let dir = temp_dir("directory-identity");
        let path = dir.join("run");
        fs::create_dir(&path).unwrap();
        let identity = directory_identity(&path).unwrap();
        fs::write(path.join("events.jsonl"), b"new\n").unwrap();
        assert_eq!(directory_identity(&path).unwrap(), identity);
        fs::rename(&path, dir.join("old")).unwrap();
        fs::create_dir(&path).unwrap();
        assert_ne!(directory_identity(&path).unwrap(), identity);
        assert!(directory_identity(&dir.join("old/events.jsonl")).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn atomic_write_is_private_and_replaces() {
        let dir = temp_dir("atomic");
        let path = dir.join("state.json");
        write_private_atomic(&path, b"one").unwrap();
        write_private_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "no temporary files left behind");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tail_starts_on_a_line_boundary() {
        let dir = temp_dir("tail");
        let path = dir.join("log");
        fs::write(&path, "first line\nsecond\nthird\n").unwrap();
        assert_eq!(read_tail(&path, 12).unwrap(), "third\n");
        assert_eq!(read_tail(&path, 1000).unwrap(), "first line\nsecond\nthird\n");
        assert_eq!(tail_lines(&path, 2).unwrap(), vec!["second", "third"]);
        fs::remove_dir_all(dir).unwrap();
    }
}
