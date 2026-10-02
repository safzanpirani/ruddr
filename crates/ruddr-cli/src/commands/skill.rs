//! `ruddr skill install|show`. The delegate skill ships inside the binary so
//! every install channel can place it where coding agents look for skills.
//! Port of skill.go.

use super::args;
use ruddr_core::{Error, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const DELEGATE_SKILL: &str = include_str!("../../../../skills/ruddr-delegate/SKILL.md");
pub const DELEGATE_SKILL_NAME: &str = "ruddr-delegate";

pub fn skill_command(argv: Vec<String>) -> Result<()> {
    let Some((action, rest)) = argv.split_first() else {
        print_skill_usage();
        return Err(Error::usage("a skill subcommand is required"));
    };
    match action.as_str() {
        "install" => install_command(rest),
        "show" => {
            print!("{DELEGATE_SKILL}");
            Ok(())
        }
        "help" | "--help" | "-h" => {
            print_skill_usage();
            Ok(())
        }
        other => {
            print_skill_usage();
            Err(Error::failed(format!("unknown skill subcommand {other:?}")))
        }
    }
}

fn install_command(argv: &[String]) -> Result<()> {
    let specs = [args::multi(
        "dir",
        "DIR",
        "skills directory to install into; repeatable (default: ~/.claude/skills, ~/.agents/skills, and ~/.codex/skills when Codex is installed)",
    )];
    let parsed = args::parse("skill install", &specs, argv)?;
    if !parsed.positionals.is_empty() {
        return Err(Error::failed(format!(
            "unexpected skill install arguments {:?}",
            parsed.positionals.join(" ")
        )));
    }
    let dirs = parsed.all("dir");
    if dirs.iter().any(String::is_empty) {
        return Err(Error::usage("directory must not be empty"));
    }
    let targets: Vec<PathBuf> = if dirs.is_empty() {
        default_skill_dirs(&ruddr_core::paths::home_dir())
    } else {
        dirs.iter().map(PathBuf::from).collect()
    };
    let stdout = std::io::stdout();
    install_into(&mut stdout.lock(), &targets)
}

/// Installs into every target and reports each written path. Failures are
/// collected so one unwritable directory does not stop the others.
pub fn install_into(out: &mut dyn Write, targets: &[PathBuf]) -> Result<()> {
    let mut failures = Vec::new();
    for dir in targets {
        match install(dir) {
            Ok(path) => writeln!(out, "installed {}", path.display())?,
            Err(error) => failures.push(format!("{}: {error}", dir.display())),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Error::failed(failures.join("; ")))
    }
}

/// Claude Code, the shared `~/.agents` location, and Codex when it is installed.
pub fn default_skill_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![home.join(".claude").join("skills"), home.join(".agents").join("skills")];
    if home.join(".codex").is_dir() {
        dirs.push(home.join(".codex").join("skills"));
    }
    dirs
}

/// Writes `<dir>/ruddr-delegate/SKILL.md` atomically, replacing an earlier
/// copy, and returns the file path.
pub fn install(dir: &Path) -> std::io::Result<PathBuf> {
    let skill_dir = dir.join(DELEGATE_SKILL_NAME);
    std::fs::create_dir_all(&skill_dir)?;
    let path = skill_dir.join("SKILL.md");
    if std::fs::read(&path).is_ok_and(|existing| existing == DELEGATE_SKILL.as_bytes()) {
        return Ok(path);
    }
    let temporary = skill_dir.join(format!(".SKILL.md-{}", ruddr_core::fsutil::random_hex(6)));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        file.write_all(DELEGATE_SKILL.as_bytes())?;
        file.sync_all()?;
        drop(file);
        ruddr_core::fsutil::set_mode(&temporary, 0o644)?;
        std::fs::rename(&temporary, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map(|()| path)
}

fn print_skill_usage() {
    eprint!(
        "Usage:
  ruddr skill install [--dir DIR ...]   copy the ruddr-delegate skill into agent skill directories
  ruddr skill show                      print the skill

Without --dir the skill is installed into ~/.claude/skills, ~/.agents/skills,
and ~/.codex/skills when ~/.codex exists. ruddr update, the npm postinstall
hook, and scripts/install-local.sh run skill install for you.
"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ruddr-skill-{name}-{}", ruddr_core::fsutil::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn skill_is_embedded() {
        assert!(
            DELEGATE_SKILL.starts_with("---\nname: ruddr-delegate\n"),
            "{}",
            &DELEGATE_SKILL[..40]
        );
    }

    #[test]
    fn installs_and_replaces_a_stale_copy() {
        let dir = temp_dir("install");
        let path = install(&dir).unwrap();
        assert_eq!(path, dir.join("ruddr-delegate").join("SKILL.md"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), DELEGATE_SKILL);
        std::fs::write(&path, "stale").unwrap();
        install(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), DELEGATE_SKILL);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1,
            "temporary file left behind"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_installs_leave_one_file() {
        let dir = temp_dir("concurrent");
        // A leftover temporary path from an older installer must not block installs.
        std::fs::create_dir_all(dir.join(DELEGATE_SKILL_NAME).join("SKILL.md.tmp")).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(32));
        let handles: Vec<_> = (0..32)
            .map(|_| {
                let (dir, barrier) = (dir.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    install(&dir).unwrap();
                })
            })
            .collect();
        handles.into_iter().for_each(|h| h.join().unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.join(DELEGATE_SKILL_NAME).join("SKILL.md")).unwrap(),
            DELEGATE_SKILL
        );
        assert_eq!(
            std::fs::read_dir(dir.join(DELEGATE_SKILL_NAME)).unwrap().count(),
            2,
            "temporary files leaked"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn default_dirs_include_codex_only_when_installed() {
        let home = temp_dir("home");
        assert_eq!(default_skill_dirs(&home).len(), 2);
        std::fs::create_dir(home.join(".codex")).unwrap();
        let dirs = default_skill_dirs(&home);
        assert_eq!(dirs.len(), 3);
        assert_eq!(dirs[2], home.join(".codex").join("skills"));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn install_into_reports_paths_and_collects_failures() {
        let (a, b) = (temp_dir("a"), temp_dir("b"));
        let blocked = a.join("file");
        std::fs::write(&blocked, "not a directory").unwrap();
        let mut out = Vec::new();
        let error = install_into(&mut out, &[a.clone(), blocked.clone(), b.clone()]).unwrap_err();
        assert!(error.message.starts_with(&blocked.display().to_string()), "{}", error.message);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
        std::fs::remove_dir_all(a).unwrap();
        std::fs::remove_dir_all(b).unwrap();
    }
}
