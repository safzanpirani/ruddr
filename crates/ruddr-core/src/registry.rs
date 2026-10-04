//! The private global run registry: one `<sha256(stateDir)>.run` file per
//! run, holding only the absolute state-directory path. The TUI and web
//! dashboard list every registered run; nothing else lives here.

use crate::error::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn entry_name(state_dir: &Path) -> String {
    let digest = Sha256::digest(state_dir.to_string_lossy().as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{hex}.run")
}

/// Records `state_dir` in the registry (idempotent).
pub fn register(state_dir: &Path) -> Result<()> {
    let absolute = crate::paths::absolute(state_dir);
    register_in(&absolute, &crate::paths::registry_dir())
}

fn register_in(absolute: &Path, dir: &Path) -> Result<()> {
    crate::fsutil::create_private_dir(dir).context("create run registry")?;
    let _lock = RegistryLock::acquire(dir).context("lock run registry")?;
    let line = format!("{}\n", absolute.display());
    crate::fsutil::write_private_atomic(&dir.join(entry_name(absolute)), line.as_bytes()).context("register run")
}

/// Every registered state directory across current and pre-rename registries.
pub fn registered_state_dirs() -> Vec<PathBuf> {
    registered_in(&crate::paths::registry_dirs_for_discovery())
}

pub fn registered_in(registries: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for registry in registries {
        let Ok(entries) = std::fs::read_dir(registry) else { continue };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().ends_with(".run") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                let dir = text.trim();
                if !dir.is_empty() {
                    dirs.push(PathBuf::from(dir));
                }
            }
        }
    }
    dirs
}

/// Removes every registry entry that points at `state_dir`. Returns how many.
pub fn unregister(state_dir: &Path, registries: &[PathBuf]) -> usize {
    let target = crate::paths::absolute(state_dir);
    let mut removed = 0;
    for registry in registries {
        let Ok(entries) = std::fs::read_dir(registry) else { continue };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().ends_with(".run") {
                continue;
            }
            let points_here = std::fs::read_to_string(entry.path())
                .map(|text| crate::paths::absolute(Path::new(text.trim())) == target)
                .unwrap_or(false);
            if points_here && std::fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// One registry entry whose target is confirmed absent.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PruneEntry {
    pub entry: PathBuf,
    pub state_dir: PathBuf,
}

/// An entry or registry that could not be inspected. It is always retained.
#[derive(Debug, serde::Serialize)]
pub struct PruneIssue {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PruneReport {
    pub applied: bool,
    pub entries: Vec<PruneEntry>,
    pub kept: usize,
    pub unreadable: Vec<PruneIssue>,
}

impl PruneReport {
    fn issue(&mut self, path: PathBuf, error: impl std::fmt::Display) {
        self.unreadable.push(PruneIssue {
            path,
            error: error.to_string(),
        });
    }
}

/// Inspects current and legacy registries. Only NotFound targets qualify.
/// Registry entries are independent atomic files, so unlinking one entry is
/// the registry update. The registration lock protects the check and unlink;
/// another run never loses its entry to a stale snapshot of the registry.
pub fn prune(registries: &[PathBuf], apply: bool) -> Result<PruneReport> {
    let mut report = PruneReport {
        applied: apply,
        ..PruneReport::default()
    };
    for registry in registries {
        let entries = match std::fs::read_dir(registry) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                report.issue(registry.clone(), error);
                continue;
            }
        };
        let _lock = if apply {
            Some(RegistryLock::acquire(registry).context("lock run registry")?)
        } else {
            None
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    report.issue(registry.clone(), error);
                    continue;
                }
            };
            if !entry.file_name().to_string_lossy().ends_with(".run") {
                continue;
            }
            let path = entry.path();
            let target = match read_entry(&path) {
                Ok(target) => target,
                Err(error) => {
                    report.issue(path, error);
                    continue;
                }
            };
            match std::fs::metadata(&target) {
                Ok(_) => report.kept += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if apply && let Err(error) = std::fs::remove_file(&path) {
                        report.issue(path, error);
                        continue;
                    }
                    report.entries.push(PruneEntry {
                        entry: path,
                        state_dir: target,
                    });
                }
                Err(error) => report.issue(path, format!("{}: {error}", target.display())),
            }
        }
    }
    report.entries.sort_by(|a, b| a.entry.cmp(&b.entry));
    report.unreadable.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(report)
}

fn read_entry(path: &Path) -> io::Result<PathBuf> {
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "entry is not a regular file"));
    }
    let mut text = String::new();
    File::open(path)?.take(64 * 1024 + 1).read_to_string(&mut text)?;
    let target = Path::new(text.trim());
    if text.len() > 64 * 1024 || !target.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "entry must contain an absolute state directory",
        ));
    }
    Ok(target.to_path_buf())
}

/// An OS-held lock survives neither process exit nor a crash. Keep the lock
/// file in place so every writer locks the same inode. Windows share_mode(0)
/// supplies the same exclusive-open contract without another dependency.
struct RegistryLock {
    _file: File,
}

impl RegistryLock {
    fn acquire(registry: &Path) -> io::Result<Self> {
        let path = registry.join(".registry.lock");
        let start = Instant::now();
        loop {
            match Self::try_acquire(&path) {
                Ok(lock) => return Ok(lock),
                Err(error) if lock_busy(&error) => {
                    if start.elapsed() >= Duration::from_secs(5) {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "run registry is locked"));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
    }

    #[cfg(unix)]
    fn try_acquire(path: &Path) -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        let file = crate::fsutil::open_private_append(path)?;
        // SAFETY: file owns this valid descriptor for the lifetime of the lock.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { _file: file })
    }

    #[cfg(windows)]
    fn try_acquire(path: &Path) -> io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .share_mode(0)
            .open(path)
            .map(|file| Self { _file: file })
    }
}

fn lock_busy(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        error.kind() == io::ErrorKind::WouldBlock
    }
    #[cfg(windows)]
    {
        error.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_names_match_go() {
        // printf /tmp/x | shasum -a 256, the name Go's registerRunStateDir used.
        assert_eq!(
            entry_name(Path::new("/tmp/x")),
            "2e56aa36f538b33b48f37ef51e54ddb5cb9c7935e65c296b7494a17e8dff2a12.run"
        );
    }

    #[test]
    fn registers_reads_and_unregisters() {
        let root = std::env::temp_dir().join(format!("ruddr-registry-{}", crate::fsutil::random_hex(4)));
        let registry = root.join("runs");
        std::fs::create_dir_all(&registry).unwrap();
        std::fs::write(registry.join("a.run"), "/w/run-a\n").unwrap();
        std::fs::write(registry.join("b.run"), "/w/run-b\n").unwrap();
        std::fs::write(registry.join("note.txt"), "/ignored\n").unwrap();
        let mut dirs = registered_in(std::slice::from_ref(&registry));
        dirs.sort();
        assert_eq!(dirs, vec![PathBuf::from("/w/run-a"), PathBuf::from("/w/run-b")]);
        assert_eq!(unregister(Path::new("/w/run-a"), std::slice::from_ref(&registry)), 1);
        assert_eq!(registered_in(&[registry]), vec![PathBuf::from("/w/run-b")]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prune_only_removes_confirmed_missing_entries() {
        let root = std::env::temp_dir().join(format!("ruddr-prune-{}", crate::fsutil::random_hex(4)));
        let registry = root.join("registry");
        let present = root.join("present");
        let absent = root.join("absent");
        std::fs::create_dir_all(&present).unwrap();
        std::fs::write(present.join("trace.log"), "preserve transcript").unwrap();
        register_in(&present, &registry).unwrap();
        register_in(&absent, &registry).unwrap();
        // An unreadable entry must never qualify for removal.
        std::fs::create_dir(registry.join("directory.run")).unwrap();
        std::fs::write(registry.join("bad.run"), "relative-path").unwrap();
        std::fs::write(registry.join("note.txt"), "keep").unwrap();
        let regs = std::slice::from_ref(&registry);
        let dry = prune(regs, false).unwrap();
        assert_eq!(dry.entries.len(), 1);
        assert_eq!(dry.entries[0].state_dir, absent);
        assert_eq!(dry.kept, 1);
        assert_eq!(dry.unreadable.len(), 2);
        assert!(registry.join(entry_name(&absent)).exists());
        let applied = prune(regs, true).unwrap();
        assert_eq!(applied.entries.len(), 1);
        assert_eq!(applied.unreadable.len(), 2);
        assert!(!registry.join(entry_name(&absent)).exists());
        assert!(registry.join("bad.run").exists());
        assert_eq!(std::fs::read_to_string(present.join("trace.log")).unwrap(), "preserve transcript");
        assert_eq!(std::fs::read_to_string(registry.join("note.txt")).unwrap(), "keep");
        assert!(prune(regs, true).unwrap().entries.is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(registry.join(".registry.lock")).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prune_and_registration_share_the_lock_and_keep_a_revived_run() {
        let root = std::env::temp_dir().join(format!("ruddr-prune-lock-{}", crate::fsutil::random_hex(4)));
        let registry = root.join("registry");
        let target = root.join("run");
        register_in(&target, &registry).unwrap();
        let lock = RegistryLock::acquire(&registry).unwrap();
        assert!(lock_busy(
            &RegistryLock::try_acquire(&registry.join(".registry.lock")).err().unwrap()
        ));
        let prune_registry = registry.clone();
        let pruning = std::thread::spawn(move || prune(&[prune_registry], true).unwrap());
        std::fs::create_dir(&target).unwrap();
        let register_registry = registry.clone();
        let register_target = target.clone();
        let registering = std::thread::spawn(move || register_in(&register_target, &register_registry).unwrap());
        drop(lock);
        assert!(pruning.join().unwrap().entries.is_empty());
        registering.join().unwrap();
        assert_eq!(registered_in(&[registry]), vec![target]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn prune_keeps_permission_errors() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("ruddr-prune-perms-{}", crate::fsutil::random_hex(4)));
        let registry = root.join("registry");
        let private = root.join("private");
        std::fs::create_dir_all(&private).unwrap();
        let target = private.join("absent");
        register_in(&target, &registry).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o0)).unwrap();
        let denied = std::fs::metadata(&target).is_err_and(|e| e.kind() == io::ErrorKind::PermissionDenied);
        let result = prune(std::slice::from_ref(&registry), true);
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        if denied {
            let report = result.unwrap();
            assert!(report.entries.is_empty());
            assert_eq!(report.unreadable.len(), 1);
            assert!(registry.join(entry_name(&target)).exists());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
