//! `ruddr update [--check]`, the once-a-day release check, and the notice
//! `ruddr version` prints. Port of update.go.
//!
//! The check result lives in `update-check.json` beside the run registry
//! (`~/.local/state/ruddr/` by default), so the check happens at most once a
//! day across every command.

use super::args;
use super::skill;
use ruddr_core::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const REPOSITORY: &str = "safzanpirani/ruddr";
pub const CHECK_DISABLE_ENV: &str = "RUDDR_NO_UPDATE_CHECK";
const CHECK_INTERVAL_MS: i64 = 24 * 3600 * 1000;
/// The background check on `version` must not hold the command up.
const CHECK_TIMEOUT: Duration = Duration::from_secs(3);
/// `ruddr update` looks the release up with this bound.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(15);
/// Downloads of checksums and binaries.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_DOWNLOAD_BYTES: u64 = 256 << 20;

/// The network calls an update makes. Tests substitute a fake.
pub trait Http {
    /// Requests `url` without following redirects and returns the
    /// `Location` header, or an error naming the status when there is none.
    fn redirect_location(&self, url: &str, timeout: Duration) -> std::result::Result<String, String>;
    /// Downloads `url`, which must answer 200.
    fn get(&self, url: &str, timeout: Duration) -> std::result::Result<Vec<u8>, String>;
}

/// The real client. It honors the usual proxy environment variables.
pub struct Ureq;

impl Ureq {
    fn agent(timeout: Duration, follow: bool) -> ureq::Agent {
        let mut config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .user_agent(format!("ruddr/{}", ruddr_core::VERSION));
        if !follow {
            config = config.max_redirects(0).max_redirects_will_error(false);
        }
        config.build().into()
    }
}

impl Http for Ureq {
    fn redirect_location(&self, url: &str, timeout: Duration) -> std::result::Result<String, String> {
        let response = Self::agent(timeout, false).head(url).call().map_err(|e| e.to_string())?;
        match response.headers().get("location").and_then(|v| v.to_str().ok()) {
            Some(location) if !location.is_empty() => Ok(location.to_string()),
            _ => Err(format!("{url} returned {} without a release redirect", response.status())),
        }
    }

    fn get(&self, url: &str, timeout: Duration) -> std::result::Result<Vec<u8>, String> {
        let mut response = Self::agent(timeout, true).get(url).call().map_err(|e| e.to_string())?;
        if response.status() != 200 {
            return Err(format!("{url} returned {}", response.status()));
        }
        response
            .body_mut()
            .with_config()
            .limit(MAX_DOWNLOAD_BYTES)
            .read_to_vec()
            .map_err(|e| e.to_string())
    }
}

/// The cached result of the last release lookup.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheck {
    pub checked_at: String,
    #[serde(default)]
    pub latest: String,
    #[serde(default)]
    pub current: String,
}

pub fn checks_disabled() -> bool {
    std::env::var(CHECK_DISABLE_ENV).is_ok_and(|v| v == "1")
}

/// `update-check.json` beside the run registry.
pub fn cache_path() -> PathBuf {
    let registry = ruddr_core::paths::registry_dir();
    registry
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or(registry)
        .join("update-check.json")
}

pub fn read_check(path: &Path) -> Option<UpdateCheck> {
    let check: UpdateCheck = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    ruddr_core::time::parse_rfc3339_ms(&check.checked_at)?;
    Some(check)
}

pub fn write_check(path: &Path, check: &UpdateCheck) -> Result<()> {
    if let Some(parent) = path.parent() {
        ruddr_core::fsutil::create_private_dir(parent)?;
    }
    ruddr_core::fsutil::write_private_atomic(path, &serde_json::to_vec(check)?)?;
    Ok(())
}

/// A missing, unreadable, other-version, future, or day-old cache is stale.
pub fn check_is_stale(path: &Path, now_ms: i64) -> bool {
    let Some(check) = read_check(path) else { return true };
    let age = now_ms - ruddr_core::time::parse_rfc3339_ms(&check.checked_at).unwrap_or(0);
    check.current != ruddr_core::VERSION || !(0..=CHECK_INTERVAL_MS).contains(&age)
}

/// The newer release the last check found, if any.
pub fn available_update(path: &Path, disabled: bool) -> Option<String> {
    if disabled {
        return None;
    }
    let check = read_check(path)?;
    (compare_versions(&check.latest, ruddr_core::VERSION) > 0).then_some(check.latest)
}

/// Performs a bounded lookup when the cache is older than a day. A failed
/// lookup also counts toward the interval and keeps the last known release.
pub fn refresh_check(http: &dyn Http, path: &Path, disabled: bool) {
    if disabled || !check_is_stale(path, ruddr_core::time::now_ms()) {
        return;
    }
    let latest = fetch_latest_version(http, CHECK_TIMEOUT).unwrap_or_else(|_| read_check(path).map(|c| c.latest).unwrap_or_default());
    let check = UpdateCheck {
        checked_at: ruddr_core::time::now_rfc3339(),
        latest,
        current: ruddr_core::VERSION.to_string(),
    };
    let _ = write_check(path, &check);
}

pub fn notice(latest: &str) -> String {
    format!(
        "ruddr {latest} is available (installed {}); run `ruddr update` to install it",
        ruddr_core::VERSION
    )
}

/// `ruddr version`: the version, a cached-daily release check, and a notice.
pub fn version_command(argv: Vec<String>) -> Result<()> {
    args::no_positionals("version", &args::parse("version", &[], &argv)?)?;
    println!("ruddr {}", ruddr_core::VERSION);
    let path = cache_path();
    let disabled = checks_disabled();
    refresh_check(&Ureq, &path, disabled);
    if let Some(latest) = available_update(&path, disabled) {
        eprintln!("{}", notice(&latest));
    }
    Ok(())
}

/// Asks GitHub for the newest release tag through the redirecting
/// `releases/latest` URL, which unauthenticated checks can use without the
/// API's rate limit.
pub fn fetch_latest_version(http: &dyn Http, timeout: Duration) -> Result<String> {
    let url = format!("https://github.com/{REPOSITORY}/releases/latest");
    let location = http.redirect_location(&url, timeout).map_err(Error::failed)?;
    version_from_release_url(&location)
}

pub fn version_from_release_url(location: &str) -> Result<String> {
    let marker = "/releases/tag/";
    let index = location
        .rfind(marker)
        .ok_or_else(|| Error::failed(format!("unexpected release location {location:?}")))?;
    let tag = location[index + marker.len()..].trim_start_matches('v');
    if parse_version(tag).is_none() {
        return Err(Error::failed(format!("unexpected release tag {tag:?}")));
    }
    Ok(tag.to_string())
}

/// `MAJOR[.MINOR[.PATCH]]`, ignoring a leading `v` and any `-pre` or `+build` suffix.
pub fn parse_version(text: &str) -> Option<[u64; 3]> {
    let text = text.trim();
    let text = text.strip_prefix('v').unwrap_or(text);
    let text = &text[..text.find(['-', '+']).unwrap_or(text.len())];
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() > 3 {
        return None;
    }
    let mut parsed = [0u64; 3];
    for (slot, part) in parsed.iter_mut().zip(&parts) {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    Some(parsed)
}

/// Orders two versions; an unparsable one sorts lowest.
pub fn compare_versions(a: &str, b: &str) -> i32 {
    match (parse_version(a), parse_version(b)) {
        (Some(left), Some(right)) => left.cmp(&right) as i32,
        (Some(_), None) => 1,
        (None, Some(_)) => -1,
        (None, None) => 0,
    }
}

/// `ruddr-<goos>-<goarch>[.exe]`, the release asset for this platform.
pub fn release_asset_name() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    format!("ruddr-{os}-{arch}{suffix}")
}

fn download_url(tag: &str, asset: &str) -> String {
    format!("https://github.com/{REPOSITORY}/releases/download/{tag}/{asset}")
}

/// The lowercase SHA-256 for `asset` in a `sha256sum`-style listing.
pub fn checksum_for(checksums: &str, asset: &str) -> Option<String> {
    checksums.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        (fields.len() == 2 && fields[1].trim_start_matches('*') == asset).then(|| fields[0].to_lowercase())
    })
}

/// How this executable reached the machine, which decides how to replace it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Channel {
    Npm(PathBuf),
    Bun(PathBuf),
    /// A repository checkout, which updates through `git pull`.
    Source,
    /// A standalone binary, replaced in place.
    Binary,
}

impl Channel {
    pub fn kind(&self) -> &'static str {
        match self {
            Channel::Npm(_) => "npm",
            Channel::Bun(_) => "bun",
            Channel::Source => "source",
            Channel::Binary => "binary",
        }
    }
}

/// Inspects the directory around the executable. The npm package keeps the
/// binary beside its `package.json` and launcher scripts; a checkout also
/// has `scripts/install-local.sh`.
pub fn detect_channel(executable: &Path) -> Channel {
    let dir = executable.parent().unwrap_or(Path::new("."));
    if !is_package_root(dir) {
        return Channel::Binary;
    }
    if dir.join("scripts").join("install-local.sh").exists() {
        return Channel::Source;
    }
    if dir.to_string_lossy().replace('\\', "/").contains("/.bun/") {
        Channel::Bun(dir.to_path_buf())
    } else {
        Channel::Npm(dir.to_path_buf())
    }
}

fn is_package_root(dir: &Path) -> bool {
    let Ok(data) = std::fs::read(dir.join("package.json")) else {
        return false;
    };
    let named_ruddr = serde_json::from_slice::<serde_json::Value>(&data)
        .ok()
        .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(|n| n == "ruddr"))
        .unwrap_or(false);
    named_ruddr && dir.join("scripts").join("npm-binary.cjs").exists()
}

/// Replaces `executable` with `binary`. The new file is written beside it and
/// renamed into place, so the path never holds a partial binary. Windows
/// cannot overwrite a running executable, but it can rename it away first.
pub fn swap_executable(executable: &Path, binary: &[u8]) -> Result<()> {
    let dir = executable.parent().unwrap_or(Path::new("."));
    let temporary = dir.join(format!(".ruddr-update-{}", ruddr_core::fsutil::random_hex(6)));
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        file.write_all(binary)?;
        file.sync_all()?;
        drop(file);
        ruddr_core::fsutil::set_mode(&temporary, 0o755)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(Error::failed(format!("cannot write next to {}: {e}", executable.display())));
    }
    #[cfg(windows)]
    let old = {
        let mut old = executable.as_os_str().to_owned();
        old.push(".old");
        let old = PathBuf::from(old);
        let _ = std::fs::remove_file(&old);
        if let Err(e) = std::fs::rename(executable, &old) {
            let _ = std::fs::remove_file(&temporary);
            return Err(Error::failed(format!("move the current binary aside: {e}")));
        }
        old
    };
    if let Err(e) = std::fs::rename(&temporary, executable) {
        let _ = std::fs::remove_file(&temporary);
        #[cfg(windows)]
        let _ = std::fs::rename(&old, executable);
        return Err(Error::failed(format!("replace {}: {e}", executable.display())));
    }
    Ok(())
}

/// Everything `ruddr update` touches, so tests can point it elsewhere.
pub struct Context<'a> {
    pub http: &'a dyn Http,
    pub cache_path: PathBuf,
    pub executable: PathBuf,
    /// The directories `skill install` writes to by default.
    pub skill_dirs: Vec<PathBuf>,
    /// Pass `skill_dirs` to the new binary's `skill install`. Off in real
    /// updates, so a release that adds a default directory installs there.
    pub pass_skill_dirs: bool,
}

pub fn update_command(argv: Vec<String>) -> Result<()> {
    let parsed = args::parse(
        "update",
        &[args::flag("check", "report whether a newer release exists without installing it")],
        &argv,
    )?;
    if !parsed.positionals.is_empty() {
        return Err(Error::failed(format!(
            "unexpected update arguments {:?}",
            parsed.positionals.join(" ")
        )));
    }
    let executable = std::env::current_exe().map_err(|e| Error::failed(format!("locate Ruddr executable: {e}")))?;
    let context = Context {
        http: &Ureq,
        cache_path: cache_path(),
        executable,
        skill_dirs: skill::default_skill_dirs(&ruddr_core::paths::home_dir()),
        pass_skill_dirs: false,
    };
    let stdout = std::io::stdout();
    update(&mut stdout.lock(), &context, parsed.bool("check"))
}

pub fn update(out: &mut dyn Write, context: &Context, check_only: bool) -> Result<()> {
    let latest = fetch_latest_version(context.http, LOOKUP_TIMEOUT).map_err(|e| e.context("look up the latest release"))?;
    let check = UpdateCheck {
        checked_at: ruddr_core::time::now_rfc3339(),
        latest: latest.clone(),
        current: ruddr_core::VERSION.to_string(),
    };
    let _ = write_check(&context.cache_path, &check);
    let current = ruddr_core::VERSION;
    if compare_versions(&latest, current) <= 0 {
        writeln!(out, "ruddr {current} is up to date")?;
        // Still sync the skill: it may predate this binary or have been edited.
        refresh_skill(out, context, false);
        return Ok(());
    }
    if check_only {
        writeln!(out, "{}", notice(&latest))?;
        return Ok(());
    }
    let channel = detect_channel(&context.executable);
    writeln!(out, "updating ruddr {current} -> {latest} via {}", channel.kind())?;
    out.flush()?;
    match channel {
        Channel::Npm(root) => package_manager_update(out, "npm", &npm_update_args(&root, &latest))?,
        Channel::Bun(_) => package_manager_update(out, "bun", &["add".into(), "-g".into(), format!("ruddr@{latest}").into()])?,
        Channel::Source => {
            refresh_skill(out, context, false);
            return Err(Error::failed(
                "this Ruddr was installed from a source checkout; run `git pull` there and rerun scripts/install-local.sh",
            ));
        }
        Channel::Binary => replace_executable(out, context, &latest)?,
    }
    // The new binary carries the current skill text. Package-manager
    // postinstall hooks can be disabled, so do not rely on them.
    refresh_skill(out, context, true);
    Ok(())
}

/// Reinstalls the delegate skill. After an upgrade the new executable does
/// it, so the skill matches the new release rather than this binary. A
/// failure is reported but does not fail the update.
fn refresh_skill(out: &mut dyn Write, context: &Context, with_new_binary: bool) {
    if !with_new_binary {
        if let Err(error) = skill::install_into(out, &context.skill_dirs) {
            eprintln!("ruddr: skill install failed: {error}");
        }
        return;
    }
    let _ = out.flush();
    let mut command = std::process::Command::new(&context.executable);
    command.args(["skill", "install"]);
    if context.pass_skill_dirs {
        for dir in &context.skill_dirs {
            command.arg("--dir").arg(dir);
        }
    }
    match command.output() {
        Ok(output) => {
            let _ = out.write_all(&output.stdout);
            let _ = std::io::stderr().write_all(&output.stderr);
            if !output.status.success() {
                eprintln!("ruddr: skill install after update failed: {}", output.status);
            }
        }
        Err(error) => eprintln!("ruddr: skill install after update failed: {error}"),
    }
}

fn npm_update_args(package_root: &Path, latest: &str) -> Vec<OsString> {
    let mut arguments = vec!["install".into(), "-g".into()];
    if let Some(prefix) = npm_prefix(package_root) {
        arguments.extend(["--prefix".into(), prefix.as_os_str().to_owned()]);
    }
    arguments.push(format!("ruddr@{latest}").into());
    arguments
}

/// Global npm packages live in <prefix>/lib/node_modules on Unix and
/// <prefix>/node_modules on Windows. Preserve the existing invocation when
/// the detected package root does not follow the native global layout.
fn npm_prefix(package_root: &Path) -> Option<&Path> {
    if !package_root.is_absolute() {
        return None;
    }
    let modules = package_root.parent()?;
    if modules.file_name()? != "node_modules" {
        return None;
    }
    let prefix = modules.parent()?;
    #[cfg(not(windows))]
    let prefix = {
        if prefix.file_name()? != "lib" {
            return None;
        }
        prefix.parent()?
    };
    Some(prefix)
}

fn package_manager_update(out: &mut dyn Write, tool: &str, arguments: &[OsString]) -> Result<()> {
    let joined = arguments.iter().map(|arg| arg.to_string_lossy()).collect::<Vec<_>>().join(" ");
    let status = std::process::Command::new(tool).args(arguments).status().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::failed(format!("{tool} is not on PATH; add it to PATH and rerun `ruddr update`"))
        } else {
            Error::failed(format!("{tool} {joined}: {e}"))
        }
    })?;
    if !status.success() {
        return Err(Error::failed(format!("{tool} {joined}: {status}")));
    }
    writeln!(out, "ruddr updated; restart any running `ruddr tui` to use it")?;
    Ok(())
}

/// Downloads this platform's release binary, verifies it against the
/// published checksums, and swaps it into place.
fn replace_executable(out: &mut dyn Write, context: &Context, latest: &str) -> Result<()> {
    let tag = format!("v{latest}");
    let asset = release_asset_name();
    let checksums = context
        .http
        .get(&download_url(&tag, "checksums.txt"), DOWNLOAD_TIMEOUT)
        .map_err(|e| Error::failed(format!("download checksums: {e}")))?;
    let expected = checksum_for(&String::from_utf8_lossy(&checksums), &asset)
        .ok_or_else(|| Error::failed(format!("release {tag} has no prebuilt binary {asset}")))?;
    writeln!(out, "downloading {asset}")?;
    let binary = context
        .http
        .get(&download_url(&tag, &asset), DOWNLOAD_TIMEOUT)
        .map_err(|e| Error::failed(format!("download {asset}: {e}")))?;
    let actual: String = Sha256::digest(&binary).iter().map(|b| format!("{b:02x}")).collect();
    if actual != expected {
        return Err(Error::failed(format!(
            "{asset} checksum mismatch: expected {expected}, got {actual}"
        )));
    }
    swap_executable(&context.executable, &binary)?;
    writeln!(out, "ruddr {latest} installed at {}", context.executable.display())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ruddr-update-{name}-{}", ruddr_core::fsutil::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// Answers from fixed tables and records every request.
    #[derive(Default)]
    struct FakeHttp {
        location: Option<String>,
        files: HashMap<String, Vec<u8>>,
        calls: RefCell<Vec<String>>,
    }

    impl Http for FakeHttp {
        fn redirect_location(&self, url: &str, _: Duration) -> std::result::Result<String, String> {
            self.calls.borrow_mut().push(format!("HEAD {url}"));
            self.location.clone().ok_or_else(|| "offline".to_string())
        }
        fn get(&self, url: &str, _: Duration) -> std::result::Result<Vec<u8>, String> {
            self.calls.borrow_mut().push(format!("GET {url}"));
            self.files.get(url).cloned().ok_or_else(|| format!("{url} returned 404 Not Found"))
        }
    }

    fn release_location(version: &str) -> Option<String> {
        Some(format!("https://github.com/{REPOSITORY}/releases/tag/v{version}"))
    }

    #[test]
    fn compares_versions_like_go() {
        for (a, b, want) in [
            ("0.3.0", "0.3.0", 0),
            ("v0.3.1", "0.3.0", 1),
            ("0.3.0", "0.10.0", -1),
            ("1.0.0", "0.99.99", 1),
            ("0.4.0-rc1", "0.4.0", 0),
            ("garbage", "0.1.0", -1),
            ("0.1.0", "garbage", 1),
        ] {
            assert_eq!(compare_versions(a, b), want, "{a} vs {b}");
        }
    }

    #[test]
    fn reads_the_version_from_the_release_redirect() {
        assert_eq!(
            version_from_release_url("https://github.com/safzanpirani/ruddr/releases/tag/v0.4.2").unwrap(),
            "0.4.2"
        );
        assert!(version_from_release_url("https://github.com/safzanpirani/ruddr/releases").is_err());
    }

    #[test]
    fn finds_checksums() {
        let checksums = "abc  ruddr-darwin-arm64\nDEF *ruddr-windows-amd64.exe\n";
        assert_eq!(checksum_for(checksums, "ruddr-darwin-arm64").as_deref(), Some("abc"));
        assert_eq!(checksum_for(checksums, "ruddr-windows-amd64.exe").as_deref(), Some("def"));
        assert_eq!(checksum_for(checksums, "ruddr-linux-amd64"), None);
    }

    #[test]
    fn names_release_assets_like_go() {
        let name = release_asset_name();
        assert!(name.starts_with("ruddr-"), "{name}");
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert_eq!(name, "ruddr-darwin-arm64");
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        assert_eq!(name, "ruddr-linux-amd64");
    }

    #[test]
    fn detects_install_channels() {
        let root = temp_dir("channel");
        assert_eq!(detect_channel(&root.join("bin").join("ruddr")), Channel::Binary);

        let npm = root.join("lib").join("node_modules").join("ruddr");
        write(&npm.join("package.json"), r#"{"name":"ruddr"}"#);
        write(&npm.join("scripts").join("npm-binary.cjs"), "");
        assert_eq!(detect_channel(&npm.join("ruddr")), Channel::Npm(npm.clone()));

        let bun = root.join(".bun").join("install").join("global").join("node_modules").join("ruddr");
        write(&bun.join("package.json"), r#"{"name":"ruddr"}"#);
        write(&bun.join("scripts").join("npm-binary.cjs"), "");
        assert_eq!(detect_channel(&bun.join("ruddr")), Channel::Bun(bun.clone()));

        let checkout = root.join("checkout");
        write(&checkout.join("package.json"), r#"{"name":"ruddr"}"#);
        write(&checkout.join("scripts").join("npm-binary.cjs"), "");
        write(&checkout.join("scripts").join("install-local.sh"), "");
        assert_eq!(detect_channel(&checkout.join("ruddr")), Channel::Source);

        let other = root.join("other");
        write(&other.join("package.json"), r#"{"name":"not-ruddr"}"#);
        write(&other.join("scripts").join("npm-binary.cjs"), "");
        assert_eq!(detect_channel(&other.join("ruddr")), Channel::Binary);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn npm_update_preserves_the_installed_prefix() {
        let base = std::env::temp_dir();
        for prefix in [base.join("custom prefix"), base.join("system"), base.join("lib")] {
            let modules = if cfg!(windows) { prefix.clone() } else { prefix.join("lib") };
            let root = modules.join("node_modules").join("ruddr");
            let arguments = npm_update_args(&root, "99.0.0");
            let mut command = std::process::Command::new("npm");
            command.args(&arguments);
            let expected: Vec<OsString> = vec![
                "install".into(),
                "-g".into(),
                "--prefix".into(),
                prefix.into_os_string(),
                "ruddr@99.0.0".into(),
            ];
            assert_eq!(command.get_args().collect::<Vec<_>>(), expected);
        }
    }

    #[test]
    fn npm_update_keeps_the_fallback_for_unrecognized_layouts() {
        let base = std::env::temp_dir();
        for root in [PathBuf::new(), PathBuf::from("lib/node_modules/ruddr"), base.join("ruddr")] {
            assert_eq!(
                npm_update_args(&root, "99.0.0"),
                vec![OsString::from("install"), "-g".into(), "ruddr@99.0.0".into()],
            );
        }
        #[cfg(not(windows))]
        assert!(npm_prefix(&base.join("project/node_modules/ruddr")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn npm_update_preserves_non_unicode_prefixes() {
        use std::os::unix::ffi::OsStringExt;
        let prefix = std::env::temp_dir().join(OsString::from_vec(b"prefix-\xff".to_vec()));
        let root = prefix.join("lib/node_modules/ruddr");
        assert_eq!(npm_update_args(&root, "99.0.0")[3], prefix.as_os_str());
    }

    fn check(age_ms: i64, latest: &str) -> UpdateCheck {
        let at =
            std::time::SystemTime::now() - Duration::from_millis(age_ms.max(0) as u64) + Duration::from_millis((-age_ms).max(0) as u64);
        UpdateCheck {
            checked_at: ruddr_core::time::format_rfc3339(at),
            latest: latest.into(),
            current: ruddr_core::VERSION.into(),
        }
    }

    #[test]
    fn caches_the_check_for_a_day() {
        let root = temp_dir("cache");
        let path = root.join("state").join("update-check.json");
        let now = ruddr_core::time::now_ms();
        assert!(check_is_stale(&path, now), "a missing cache is stale");
        assert_eq!(available_update(&path, false), None);

        write_check(&path, &check(0, "99.0.0")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(!check_is_stale(&path, ruddr_core::time::now_ms()));
        assert_eq!(available_update(&path, false).as_deref(), Some("99.0.0"));
        assert_eq!(available_update(&path, true), None, "disabled checks report nothing");

        write_check(&path, &check(48 * 3600 * 1000, "99.0.0")).unwrap();
        assert!(check_is_stale(&path, ruddr_core::time::now_ms()), "a two-day-old cache is stale");
        write_check(&path, &check(-48 * 3600 * 1000, "")).unwrap();
        assert!(
            check_is_stale(&path, ruddr_core::time::now_ms()),
            "a future cache must not suppress checks"
        );
        write_check(&path, &check(0, ruddr_core::VERSION)).unwrap();
        assert_eq!(available_update(&path, false), None, "the same version is not an update");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_failed_check_is_cached_and_keeps_the_last_release() {
        for previous in ["", "99.0.0"] {
            let root = temp_dir("failed");
            let path = root.join("update-check.json");
            if !previous.is_empty() {
                write_check(&path, &check(48 * 3600 * 1000, previous)).unwrap();
            }
            let http = FakeHttp::default();
            refresh_check(&http, &path, false);
            refresh_check(&http, &path, false);
            assert_eq!(http.calls.borrow().len(), 1, "one network call per day");
            assert_eq!(read_check(&path).unwrap().latest, previous);
            assert!(!check_is_stale(&path, ruddr_core::time::now_ms()));
            assert_eq!(available_update(&path, false), (!previous.is_empty()).then(|| previous.to_string()));
            refresh_check(&http, &path, true);
            assert_eq!(http.calls.borrow().len(), 1);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn swaps_the_executable_atomically() {
        let dir = temp_dir("swap");
        let target = dir.join("ruddr");
        std::fs::write(&target, "old").unwrap();
        swap_executable(&target, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o111, 0);
        }
        // Windows cannot delete a running executable, so the previous binary
        // stays beside it as ruddr.old until the next update removes it.
        let expected = if cfg!(windows) { 2 } else { 1 };
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), expected, "temporary file left behind");
        #[cfg(windows)]
        assert_eq!(std::fs::read_to_string(dir.join("ruddr.old")).unwrap(), "old");
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn context<'a>(http: &'a FakeHttp, root: &Path) -> Context<'a> {
        Context {
            http,
            cache_path: root.join("state").join("update-check.json"),
            executable: root.join("bin").join("ruddr"),
            skill_dirs: vec![
                root.join("home").join(".claude").join("skills"),
                root.join("home").join(".agents").join("skills"),
            ],
            pass_skill_dirs: true,
        }
    }

    /// An up-to-date binary still resyncs the skill, so a stale or edited
    /// copy is replaced by `ruddr update` alone.
    #[test]
    fn up_to_date_update_refreshes_the_skill() {
        let root = temp_dir("current");
        let http = FakeHttp {
            location: release_location(ruddr_core::VERSION),
            ..Default::default()
        };
        let context = context(&http, &root);
        let stale = context.skill_dirs[0].join(skill::DELEGATE_SKILL_NAME).join("SKILL.md");
        write(&stale, "old skill");
        let mut out = Vec::new();
        update(&mut out, &context, false).unwrap();
        assert!(String::from_utf8_lossy(&out).contains("is up to date"));
        for dir in &context.skill_dirs {
            let installed = std::fs::read_to_string(dir.join(skill::DELEGATE_SKILL_NAME).join("SKILL.md")).unwrap();
            assert_eq!(installed, skill::DELEGATE_SKILL);
        }
        assert_eq!(read_check(&context.cache_path).unwrap().latest, ruddr_core::VERSION);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn check_only_reports_without_downloading() {
        let root = temp_dir("check");
        let http = FakeHttp {
            location: release_location("99.0.0"),
            ..Default::default()
        };
        let context = context(&http, &root);
        let mut out = Vec::new();
        update(&mut out, &context, true).unwrap();
        assert!(String::from_utf8_lossy(&out).contains("ruddr 99.0.0 is available"));
        assert_eq!(http.calls.borrow().len(), 1);
        assert!(!context.executable.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn release(binary: &[u8], checksum: Option<String>) -> FakeHttp {
        let asset = release_asset_name();
        let sum = checksum.unwrap_or_else(|| Sha256::digest(binary).iter().map(|b| format!("{b:02x}")).collect());
        let mut files = HashMap::new();
        files.insert(download_url("v99.0.0", "checksums.txt"), format!("{sum}  {asset}\n").into_bytes());
        files.insert(download_url("v99.0.0", &asset), binary.to_vec());
        FakeHttp {
            location: release_location("99.0.0"),
            files,
            ..Default::default()
        }
    }

    #[test]
    fn a_checksum_mismatch_leaves_the_binary_alone() {
        let root = temp_dir("mismatch");
        let http = release(b"new binary", Some("0".repeat(64)));
        let context = context(&http, &root);
        write(&context.executable, "old binary");
        let error = update(&mut Vec::new(), &context, false).unwrap_err();
        assert!(error.message.contains("checksum mismatch"), "{}", error.message);
        assert_eq!(std::fs::read_to_string(&context.executable).unwrap(), "old binary");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The verified binary replaces this one, and then the new binary
    /// installs its own skill into the default directories.
    #[cfg(unix)]
    #[test]
    fn a_binary_update_swaps_and_runs_the_new_skill_install() {
        let root = temp_dir("binary");
        let marker = root.join("skill-install-args");
        let script = format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", marker.display());
        let http = release(script.as_bytes(), None);
        let context = context(&http, &root);
        write(&context.executable, "old binary");
        let mut out = Vec::new();
        update(&mut out, &context, false).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("via binary") && text.contains("ruddr 99.0.0 installed at"), "{text}");
        assert_eq!(std::fs::read_to_string(&context.executable).unwrap(), script);
        let ran = std::fs::read_to_string(&marker).unwrap();
        let want = format!(
            "skill\ninstall\n--dir\n{}\n--dir\n{}\n",
            context.skill_dirs[0].display(),
            context.skill_dirs[1].display()
        );
        assert_eq!(ran, want);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_source_checkout_refuses_but_still_refreshes_the_skill() {
        let root = temp_dir("source");
        let http = FakeHttp {
            location: release_location("99.0.0"),
            ..Default::default()
        };
        let mut context = context(&http, &root);
        let checkout = root.join("checkout");
        write(&checkout.join("package.json"), r#"{"name":"ruddr"}"#);
        write(&checkout.join("scripts").join("npm-binary.cjs"), "");
        write(&checkout.join("scripts").join("install-local.sh"), "");
        context.executable = checkout.join("ruddr");
        let error = update(&mut Vec::new(), &context, false).unwrap_err();
        assert!(error.message.contains("git pull"), "{}", error.message);
        assert!(context.skill_dirs[0].join(skill::DELEGATE_SKILL_NAME).join("SKILL.md").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
