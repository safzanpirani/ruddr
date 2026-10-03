//! Image attachments: which files count as images, and the app-server
//! `UserInput` items that carry a prompt with them. Codex reads a
//! `localImage` item itself; the adapters turn it into a path the agent
//! opens with its own file tools.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// The most images one prompt may carry.
pub const MAX_IMAGES: usize = 10;

pub fn has_image_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Resolves an attachment to an absolute path and checks that it is an
/// existing file with an image extension.
pub fn checked_image(path: &Path) -> Result<PathBuf, String> {
    let absolute = crate::paths::absolute(path);
    if !has_image_extension(&absolute) {
        return Err(format!("image {} must end in .{}", path.display(), IMAGE_EXTENSIONS.join(", .")));
    }
    if !absolute.is_file() {
        return Err(format!("image {} is not a readable file", path.display()));
    }
    Ok(absolute)
}

/// Checks every attachment of one prompt.
pub fn checked_images(paths: &[String]) -> Result<Vec<String>, String> {
    if paths.len() > MAX_IMAGES {
        return Err(format!("a prompt may carry at most {MAX_IMAGES} images"));
    }
    paths
        .iter()
        .map(|p| checked_image(Path::new(p)).map(|p| p.to_string_lossy().into_owned()))
        .collect()
}

/// The `input` array of `turn/start` and `turn/steer`: the text, then one
/// `localImage` item per attachment.
pub fn user_input(text: &str, images: &[String]) -> Value {
    let mut input = vec![json!({"type": "text", "text": text})];
    input.extend(images.iter().map(|path| json!({"type": "localImage", "path": path})));
    Value::Array(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_puts_text_before_images() {
        assert_eq!(
            user_input("look", &["/a.png".into()]),
            json!([{"type": "text", "text": "look"}, {"type": "localImage", "path": "/a.png"}])
        );
        assert_eq!(user_input("hi", &[]), json!([{"type": "text", "text": "hi"}]));
    }

    #[test]
    fn attachments_must_be_existing_image_files() {
        let dir = std::env::temp_dir().join(format!("ruddr-images-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("shot.PNG");
        std::fs::write(&png, b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        assert_eq!(checked_image(&png).unwrap(), png);
        assert!(checked_image(&dir.join("notes.txt")).unwrap_err().contains("must end in"));
        assert!(checked_image(&dir.join("gone.png")).unwrap_err().contains("not a readable file"));
        assert!(checked_image(&dir).unwrap_err().contains("must end in"));
        let many = vec![png.to_string_lossy().into_owned(); MAX_IMAGES + 1];
        assert!(checked_images(&many).unwrap_err().contains("at most"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
