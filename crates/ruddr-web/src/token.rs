//! The per-machine access token in `~/.config/ruddr/web-token`. The first
//! server creates it exclusively as 0600; later servers read it and repair
//! its mode. A symlink or a non-regular file is refused.

use ruddr_core::{Error, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn default_token_file() -> PathBuf {
    ruddr_core::paths::config_dir().join("web-token")
}

/// Reads the stable access token, creating it on first use.
pub fn load_token(file: &Path) -> Result<String> {
    if let Some(parent) = file.parent() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
            .create(parent)
            .map_err(|e| Error::failed(format!("create {}: {e}", parent.display())))?;
    }
    match ruddr_core::fsutil::create_private_file_new(file) {
        Ok(mut handle) => {
            let token = new_token()?;
            handle.write_all(format!("{token}\n").as_bytes())?;
            return Ok(token);
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(Error::failed(format!("create {}: {e}", file.display()))),
    }
    let mut existing = open_no_follow(file).map_err(|e| Error::failed(format!("open {}: {e}", file.display())))?;
    if !existing.metadata()?.is_file() {
        return Err(Error::failed("The web token must be a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        existing.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    // Another server may have exclusively created the file and still be writing it.
    for _ in 0..20 {
        existing.seek(SeekFrom::Start(0))?;
        let mut buffer = Vec::with_capacity(4096);
        (&mut existing).take(4096).read_to_end(&mut buffer)?;
        let token = String::from_utf8_lossy(&buffer).trim().to_string();
        if valid_token(&token) {
            return Ok(token);
        }
        if !buffer.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(Error::failed(
        "The existing web token is invalid; replace the token file explicitly",
    ))
}

fn open_no_follow(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(std::io::Error::other("refusing to follow a symbolic link"));
    }
    options.open(path)
}

/// 32 to 256 characters of base64url.
fn valid_token(token: &str) -> bool {
    (32..=256).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// 24 random bytes as unpadded base64url: 32 characters.
fn new_token() -> Result<String> {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(|e| Error::failed(format!("generate the web token: {e}")))?;
    Ok(base64url(&bytes))
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        let symbols = chunk.len() + 1;
        for i in 0..symbols {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// Compares in time that depends only on the lengths.
pub fn token_matches(expected: &str, candidate: Option<&str>) -> bool {
    let Some(candidate) = candidate else { return false };
    if candidate.is_empty() || expected.len() != candidate.len() {
        return false;
    }
    expected.bytes().zip(candidate.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_base64url_without_padding() {
        assert_eq!(base64url(b"\xfb\xff\xbf"), "-_-_");
        assert_eq!(base64url(b"ab"), "YWI");
        assert_eq!(new_token().unwrap().len(), 32);
        assert!(valid_token(&new_token().unwrap()));
    }

    #[test]
    fn matches_tokens_exactly() {
        assert!(token_matches("abc", Some("abc")));
        assert!(!token_matches("abc", Some("abd")));
        assert!(!token_matches("abc", Some("ab")));
        assert!(!token_matches("abc", None));
        assert!(!token_matches("", Some("")));
    }
}
