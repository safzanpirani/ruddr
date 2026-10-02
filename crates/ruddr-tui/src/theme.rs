//! Themes come from `ruddr_core::THEMES_JSON`, which the web dashboard
//! shares. `bun scripts/sync-opencode-themes.ts DIR` refreshes the OpenCode
//! palettes from OpenCode's theme assets.
//! The selected theme persists in ~/.config/ruddr/tui.json.

use ratatui::style::Color;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Clone, Deserialize)]
struct RawPalette {
    background: String,
    panel: String,
    border: String,
    text: String,
    dim: String,
    accent: String,
    selected: String,
    danger: String,
    success: String,
    warning: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RawTheme {
    name: String,
    label: String,
    source: String,
    palette: RawPalette,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub background: Rgb,
    pub panel: Rgb,
    pub border: Rgb,
    pub text: Rgb,
    pub dim: Rgb,
    pub accent: Rgb,
    pub selected: Rgb,
    pub danger: Rgb,
    pub success: Rgb,
    pub warning: Rgb,
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    pub label: String,
    pub source: String,
    pub palette: Palette,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn parse(hex: &str) -> Rgb {
        let hex = hex.trim_start_matches('#');
        let full: String = if hex.len() == 3 {
            hex.chars().flat_map(|c| [c, c]).collect()
        } else {
            format!("{hex:0<6}")
        };
        let channel = |i: usize| u8::from_str_radix(&full[i..i + 2], 16).unwrap_or(0);
        Rgb(channel(0), channel(2), channel(4))
    }

    /// Mixes `other` into self; 0 returns self, 1 returns other.
    pub fn mix(self, other: Rgb, amount: f32) -> Rgb {
        let t = amount.clamp(0.0, 1.0);
        let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Rgb(lerp(self.0, other.0), lerp(self.1, other.1), lerp(self.2, other.2))
    }

    pub fn c(self) -> Color {
        Color::Rgb(self.0, self.1, self.2)
    }
}

pub fn themes() -> &'static [Theme] {
    static THEMES: OnceLock<Vec<Theme>> = OnceLock::new();
    THEMES.get_or_init(|| {
        let raw: Vec<RawTheme> = serde_json::from_str(ruddr_core::THEMES_JSON).expect("embedded themes.json");
        raw.into_iter()
            .map(|t| Theme {
                name: t.name,
                label: t.label,
                source: t.source,
                palette: Palette {
                    background: Rgb::parse(&t.palette.background),
                    panel: Rgb::parse(&t.palette.panel),
                    border: Rgb::parse(&t.palette.border),
                    text: Rgb::parse(&t.palette.text),
                    dim: Rgb::parse(&t.palette.dim),
                    accent: Rgb::parse(&t.palette.accent),
                    selected: Rgb::parse(&t.palette.selected),
                    danger: Rgb::parse(&t.palette.danger),
                    success: Rgb::parse(&t.palette.success),
                    warning: Rgb::parse(&t.palette.warning),
                },
            })
            .collect()
    })
}

pub fn find(name: &str) -> Option<usize> {
    themes().iter().position(|t| t.name == name)
}

pub fn config_path() -> PathBuf {
    ruddr_core::paths::config_dir().join("tui.json")
}

#[derive(Debug, Clone, Default)]
pub struct TuiConfig {
    pub theme: Option<String>,
    pub diff_tree_ratio: Option<f64>,
    pub diff_tree_width: Option<u16>,
    /// The classic layout's session list, as a share of the screen width.
    pub sessions_ratio: Option<f64>,
    pub sessions_width: Option<u16>,
    pub mobile_width_threshold: Option<u16>,
    raw: Map<String, Value>,
}

/// Reads tui.json, falling back to files written before the rename.
pub fn read_config() -> TuiConfig {
    let home = ruddr_core::paths::config_dir().parent().map(PathBuf::from).unwrap_or_default();
    let candidates = [
        config_path(),
        home.join("rudder").join("tui.json"),
        home.join("codex-rudder").join("tui.json"),
    ];
    for path in candidates {
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let Ok(Value::Object(raw)) = serde_json::from_str::<Value>(&text) else {
            return TuiConfig::default();
        };
        return TuiConfig {
            theme: raw.get("theme").and_then(Value::as_str).map(str::to_string),
            diff_tree_ratio: raw.get("diffTreeRatio").and_then(Value::as_f64).filter(|r| *r > 0.0 && *r < 1.0),
            diff_tree_width: raw.get("diffTreeWidth").and_then(Value::as_u64).map(|w| w as u16),
            sessions_ratio: raw.get("sessionsRatio").and_then(Value::as_f64).filter(|r| *r > 0.0 && *r < 1.0),
            sessions_width: raw.get("sessionsWidth").and_then(Value::as_u64).map(|w| w as u16),
            mobile_width_threshold: raw
                .get("mobileWidthThreshold")
                .and_then(Value::as_u64)
                .filter(|w| *w <= 500)
                .map(|w| w as u16),
            raw,
        };
    }
    TuiConfig::default()
}

/// Saves the theme without dropping keys this front end does not know.
pub fn persist_theme(name: &str) -> std::io::Result<()> {
    persist(&[("theme", Value::String(name.into()))])
}

/// Saves the diff sidebar size the way the Bun TUI does.
pub fn persist_tree(width: u16, ratio: f64) -> std::io::Result<()> {
    persist(&[("diffTreeWidth", Value::from(width)), ("diffTreeRatio", Value::from(ratio))])
}

/// Saves the session list size.
pub fn persist_sessions(width: u16, ratio: f64) -> std::io::Result<()> {
    persist(&[("sessionsWidth", Value::from(width)), ("sessionsRatio", Value::from(ratio))])
}

fn persist(updates: &[(&str, Value)]) -> std::io::Result<()> {
    let mut config = read_config();
    for (key, value) in updates {
        config.raw.insert(key.to_string(), value.clone());
    }
    let path = config_path();
    if let Some(dir) = path.parent() {
        ruddr_core::fsutil::create_private_dir(dir)?;
    }
    let mut data = serde_json::to_vec_pretty(&Value::Object(config.raw)).map_err(std::io::Error::other)?;
    data.push(b'\n');
    ruddr_core::fsutil::write_private_atomic(&path, &data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_themes_load() {
        assert!(themes().len() > 10);
        assert_eq!(themes()[0].name, "ruddr");
        assert_eq!(themes()[0].palette.accent, Rgb(0x67, 0xd4, 0xff));
    }

    #[test]
    fn mixes_colors() {
        assert_eq!(Rgb(0, 0, 0).mix(Rgb(200, 100, 50), 0.5), Rgb(100, 50, 25));
        assert_eq!(Rgb::parse("#abc"), Rgb(0xaa, 0xbb, 0xcc));
    }
}
