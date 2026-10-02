//! The private global run registry: one `<sha256(stateDir)>.run` file per
//! run, holding only the absolute state-directory path. The TUI and web
//! dashboard list every registered run; nothing else lives here.

use crate::error::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn entry_name(state_dir: &Path) -> String {
    let digest = Sha256::digest(state_dir.to_string_lossy().as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{hex}.run")
}

/// Records `state_dir` in the registry (idempotent).
pub fn register(state_dir: &Path) -> Result<()> {
    let absolute = crate::paths::absolute(state_dir);
    let dir = crate::paths::registry_dir();
    crate::fsutil::create_private_dir(&dir).context("create run registry")?;
    let line = format!("{}\n", absolute.display());
    crate::fsutil::write_private_atomic(&dir.join(entry_name(&absolute)), line.as_bytes()).context("register run")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_names_match_go() {
        // printf /tmp/x | shasum -a 256, the name Go's registerRunStateDir used.
        assert_eq!(entry_name(Path::new("/tmp/x")), "2e56aa36f538b33b48f37ef51e54ddb5cb9c7935e65c296b7494a17e8dff2a12.run");
    }

    #[test]
    fn registers_reads_and_unregisters() {
        let root = std::env::temp_dir().join(format!("ruddr-registry-{}", crate::fsutil::random_hex(4)));
        let registry = root.join("runs");
        std::fs::create_dir_all(&registry).unwrap();
        std::fs::write(registry.join("a.run"), "/w/run-a\n").unwrap();
        std::fs::write(registry.join("b.run"), "/w/run-b\n").unwrap();
        std::fs::write(registry.join("note.txt"), "/ignored\n").unwrap();
        let mut dirs = registered_in(&[registry.clone()]);
        dirs.sort();
        assert_eq!(dirs, vec![PathBuf::from("/w/run-a"), PathBuf::from("/w/run-b")]);
        assert_eq!(unregister(Path::new("/w/run-a"), &[registry.clone()]), 1);
        assert_eq!(registered_in(&[registry]), vec![PathBuf::from("/w/run-b")]);
        std::fs::remove_dir_all(root).unwrap();
    }
}
