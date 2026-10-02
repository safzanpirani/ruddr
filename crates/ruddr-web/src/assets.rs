//! The embedded browser client, the theme list, and the fallback model
//! catalog. `scripts/build-web.ts` writes everything under `assets/`.

use axum::body::Body;
use axum::http::{HeaderValue, header};
use axum::response::Response;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

mod bundle {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/bundle.rs"));
}

pub use bundle::{FILES, INDEX_PATH};

/// The hash of the bundle inputs recorded by the last build.
pub const SOURCE_HASH: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/SOURCE_HASH"));

const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; connect-src 'self'; worker-src 'self' blob:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

fn files() -> &'static HashMap<&'static str, (&'static str, &'static [u8])> {
    static FILES_BY_PATH: OnceLock<HashMap<&'static str, (&'static str, &'static [u8])>> = OnceLock::new();
    FILES_BY_PATH.get_or_init(|| FILES.iter().map(|(path, kind, body)| (*path, (*kind, *body))).collect())
}

/// Serves a bundled file. `/` and extensionless paths get the HTML entry so
/// the client can route on its own.
pub fn serve_static(pathname: &str) -> Option<Response> {
    let files = files();
    let (kind, body) = files
        .get(pathname)
        .or_else(|| {
            (pathname == "/" || !pathname.contains('.'))
                .then(|| files.get(INDEX_PATH))
                .flatten()
        })
        .copied()?;
    let immutable = pathname.contains("/chunks/") || pathname.contains("/assets/");
    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(kind));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if immutable {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        }),
    );
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    if kind.starts_with("text/html") {
        headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CONTENT_SECURITY_POLICY));
    }
    Some(response)
}

pub const DEFAULT_THEME: &str = "ruddr";

/// Every theme as `{name, label, source, palette}`, shared with the TUI.
pub fn themes() -> &'static [Value] {
    static THEMES: OnceLock<Vec<Value>> = OnceLock::new();
    THEMES.get_or_init(|| serde_json::from_str(ruddr_core::THEMES_JSON).unwrap_or_default())
}

pub fn find_theme(name: &str) -> Option<&'static Value> {
    themes().iter().find(|theme| theme["name"] == name)
}

/// The catalog served when `ruddr models --json` fails.
pub fn fallback_models() -> &'static Value {
    static MODELS: OnceLock<Value> = OnceLock::new();
    MODELS.get_or_init(|| serde_json::to_value(ruddr_core::models::builtin_catalog()).unwrap_or(Value::Array(Vec::new())))
}

/// `tui.json` in `config_dir`, the file `ruddr tui` shares.
pub fn tui_config_path(config_dir: &Path) -> PathBuf {
    config_dir.join("tui.json")
}

/// The persisted theme name. Falls back to the `rudder` and `codex-rudder`
/// config files when the current one does not exist. Unreadable or invalid
/// files read as no theme.
pub fn persisted_theme(config_dir: &Path) -> Option<String> {
    let mut candidates = vec![tui_config_path(config_dir)];
    if let Some(home) = config_dir.parent() {
        candidates.push(home.join("rudder").join("tui.json"));
        candidates.push(home.join("codex-rudder").join("tui.json"));
    }
    for path in candidates {
        match std::fs::read(&path) {
            Ok(bytes) => {
                let parsed: Value = serde_json::from_slice(&bytes).ok()?;
                return parsed.get("theme").and_then(Value::as_str).map(str::to_string);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        }
    }
    None
}

/// Saves `name` as the theme in `tui.json`, keeping the file's other keys.
pub fn persist_theme(config_dir: &Path, name: &str) -> std::io::Result<()> {
    let path = tui_config_path(config_dir);
    let mut config = match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(map)) => map,
            _ => Default::default(),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
        Err(e) => return Err(e),
    };
    config.insert("theme".into(), Value::String(name.into()));
    ruddr_core::fsutil::create_private_dir(config_dir)?;
    let mut text = serde_json::to_string_pretty(&Value::Object(config)).map_err(std::io::Error::other)?;
    text.push('\n');
    ruddr_core::fsutil::write_private_atomic(&path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_the_entry_and_chunks() {
        let index = serve_static("/").unwrap();
        assert!(index.headers()[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/html"));
        assert!(index.headers().contains_key(header::CONTENT_SECURITY_POLICY));
        assert_eq!(index.headers()[header::CACHE_CONTROL], "no-cache");
        let chunk = FILES.iter().find(|(path, _, _)| path.starts_with("/chunks/")).unwrap().0;
        let response = serve_static(chunk).unwrap();
        assert_eq!(response.headers()[header::CACHE_CONTROL], "public, max-age=31536000, immutable");
        assert!(!response.headers().contains_key(header::CONTENT_SECURITY_POLICY));
        assert!(serve_static("/session/abc").is_some(), "client routes get the entry");
        assert!(serve_static("/missing.js").is_none());
    }

    #[test]
    fn themes_and_models_are_embedded() {
        assert_eq!(themes()[0]["name"], DEFAULT_THEME);
        assert!(find_theme("tokyonight").is_some());
        assert!(fallback_models().as_array().is_some_and(|models| !models.is_empty()));
    }

    #[test]
    fn persists_the_theme_and_keeps_other_keys() {
        let root = std::env::temp_dir().join(format!("ruddr-web-theme-{}", ruddr_core::fsutil::random_hex(4)));
        let config = root.join("ruddr");
        assert_eq!(persisted_theme(&config), None);
        std::fs::create_dir_all(root.join("rudder")).unwrap();
        std::fs::write(root.join("rudder").join("tui.json"), r#"{"theme":"aura"}"#).unwrap();
        assert_eq!(persisted_theme(&config).as_deref(), Some("aura"));
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            tui_config_path(&config),
            r#"{"theme":"ayu","mobileWidthThreshold":70,"future":true}"#,
        )
        .unwrap();
        persist_theme(&config, "tokyonight").unwrap();
        let saved: Value = serde_json::from_slice(&std::fs::read(tui_config_path(&config)).unwrap()).unwrap();
        assert_eq!(
            saved,
            serde_json::json!({"theme": "tokyonight", "mobileWidthThreshold": 70, "future": true})
        );
        assert_eq!(persisted_theme(&config).as_deref(), Some("tokyonight"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
