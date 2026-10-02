//! The committed client bundle must match web/client. This recomputes the
//! input hash exactly as scripts/build-web.ts does and compares it with
//! assets/SOURCE_HASH. When it fails, run `bun scripts/build-web.ts` and
//! commit the result.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn walk(repo: &Path, relative: &str, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(repo.join(relative)).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = format!("{relative}/{name}");
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            walk(repo, &path, out);
        } else if kind.is_file() && !name.ends_with(".test.ts") {
            out.push(path);
        }
    }
}

fn source_hash(repo: &Path) -> String {
    let mut files = vec!["web/index.html".to_string(), "scripts/build-web.ts".to_string()];
    walk(repo, "web/client", &mut files);
    let mut entries: Vec<(String, Vec<u8>)> = files
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(repo.join(&path)).unwrap();
            // Fold CRLF to LF so a Windows checkout hashes the same.
            let mut folded = Vec::with_capacity(bytes.len());
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
                    i += 1;
                    continue;
                }
                folded.push(bytes[i]);
                i += 1;
            }
            (path, folded)
        })
        .collect();
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(repo.join("package.json")).unwrap()).unwrap();
    // The Pierre libraries are bundled at development time, so they are
    // development dependencies.
    let mut pierre: Vec<(String, String)> = manifest["devDependencies"]
        .as_object()
        .map(|deps| {
            deps.iter()
                .filter(|(name, _)| name.starts_with("@pierre/"))
                .map(|(n, v)| (n.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();
    pierre.sort();
    let pinned: String = pierre.iter().map(|(name, version)| format!("{name}@{version}\n")).collect();
    entries.push(("package.json#@pierre".into(), pinned.into_bytes()));
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut outer = Sha256::new();
    for (path, content) in entries {
        outer.update(format!("{path}\0{}\n", hex(&Sha256::digest(&content))).as_bytes());
    }
    hex(&outer.finalize())
}

#[test]
fn bundle_matches_sources() {
    let repo: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    if !repo.join("web/client").is_dir() {
        eprintln!("web/client is not in this checkout; skipping the bundle check");
        return;
    }
    let current = source_hash(&repo);
    assert_eq!(
        ruddr_web::assets::SOURCE_HASH.trim(),
        current,
        "the committed web client bundle is stale; run `bun scripts/build-web.ts` and commit crates/ruddr-web/assets"
    );
}

#[test]
fn bundle_has_an_html_entry() {
    assert!(
        ruddr_web::assets::FILES
            .iter()
            .any(|(path, kind, _)| *path == ruddr_web::assets::INDEX_PATH && kind.starts_with("text/html"))
    );
}
