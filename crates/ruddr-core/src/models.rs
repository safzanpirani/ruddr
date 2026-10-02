//! The model catalog: the source of truth for per-provider default models and
//! for the TUI and web pickers. `~/.config/ruddr/models.json` (or
//! `$RUDDR_MODELS_FILE`) adds models, changes built-in ones, picks provider
//! defaults, and hides built-in models. A Codex model's `config` map becomes
//! `-c KEY=VALUE` flags on the default `codex app-server` command.

use crate::error::{Context, Error, Result};
use crate::provider::Provider;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const MODELS_FILE_ENV: &str = "RUDDR_MODELS_FILE";

/// One selectable catalog entry, as `ruddr models --json` prints it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModel {
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "null_default")]
    pub efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub context_window: i64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub default: bool,
    #[serde(default)]
    pub available: bool,
    /// Codex config overrides applied when a run uses this model.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty", deserialize_with = "null_default")]
    pub config: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// `config` for entries that models.json added or changed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
}

/// One models.json entry. It adds a model, changes a built-in one, makes one
/// the provider default, or hides a built-in model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelEntry {
    pub provider: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "null_default")]
    pub efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub context_window: i64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub default: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
    /// Passed to `codex app-server` as `-c KEY=VALUE` for runs on this model,
    /// for settings in `~/.codex/config.toml` the model rejects.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty", deserialize_with = "null_default")]
    pub config: BTreeMap<String, String>,
}

/// The whole models.json file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsFile {
    /// Go releases wrote `null` for an empty list.
    #[serde(default, deserialize_with = "null_default")]
    pub models: Vec<ModelEntry>,
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}
fn is_false(value: &bool) -> bool {
    !*value
}

fn null_default<'de, D: Deserializer<'de>, T: Deserialize<'de> + Default>(deserializer: D) -> std::result::Result<T, D::Error> {
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

const CODEX_EFFORTS: &[&str] = &["none", "low", "medium", "high", "xhigh", "max"];
const SOL_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];
const LUNA_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const PI_EFFORTS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];
const DROID_EFFORTS: &[&str] = &["low", "high", "max"];

/// The built-in catalog. Keep it short; users add models through models.json.
pub fn builtin_catalog() -> Vec<ProviderModel> {
    let model = |provider: Provider, id: &str, label: &str, efforts: &[&str], default: bool| ProviderModel {
        provider: provider.as_str().into(),
        id: id.into(),
        label: label.into(),
        efforts: efforts.iter().map(|e| e.to_string()).collect(),
        context_window: 0,
        default,
        available: true,
        config: BTreeMap::new(),
        note: String::new(),
        source: String::new(),
    };
    use Provider::*;
    let deepseek = "openrouter/deepseek/deepseek-v4-flash-vision-exp";
    vec![
        model(Codex, "gpt-6-astra", "GPT-6-Astra", CODEX_EFFORTS, true),
        model(Codex, "gpt-6.1-sol", "GPT-6.1-Sol", SOL_EFFORTS, false),
        model(Codex, "gpt-6-sol", "GPT-6-Sol", SOL_EFFORTS, false),
        model(Codex, "gpt-6-luna", "GPT-6-Luna", LUNA_EFFORTS, false),
        model(Codex, "gpt-5.6-sol", "GPT-5.6-Sol", CODEX_EFFORTS, false),
        model(Codex, "gpt-5.6-terra", "GPT-5.6-Terra", CODEX_EFFORTS, false),
        model(Codex, "gpt-5.6-luna", "GPT-5.6-Luna", CODEX_EFFORTS, false),
        model(Claude, "claude-fable-5-1", "Claude Fable 5.1", &[], false),
        model(Claude, "claude-fable-5", "Claude Fable 5", &[], false),
        model(Claude, "claude-opus-5-5", "Claude Opus 5.5", &[], true),
        model(Claude, "claude-opus-5", "Claude Opus 5", &[], false),
        model(Claude, "claude-sonnet-5", "Claude Sonnet 5", &[], false),
        model(Claude, "claude-haiku-4-5-20251001", "Claude Haiku 4.5", &[], false),
        model(OpenCode, deepseek, "DeepSeek V4 Flash Vision Exp", &[], true),
        model(Pi, deepseek, "DeepSeek V4 Flash Vision Exp", PI_EFFORTS, true),
        model(Droid, "glm-5.3-flash", "GLM-5.3-Flash", DROID_EFFORTS, true),
    ]
}

/// `$RUDDR_MODELS_FILE`, else `<config dir>/models.json`.
pub fn models_file_path() -> PathBuf {
    match crate::paths::env_any(&[MODELS_FILE_ENV]) {
        Some(path) => crate::paths::absolute(std::path::Path::new(&path)),
        None => crate::paths::config_dir().join("models.json"),
    }
}

/// Reads models.json. A missing file means no entries. An invalid file is an
/// error, because silently ignoring it would run the wrong default model.
pub fn read_models_file() -> Result<(ModelsFile, PathBuf)> {
    let path = models_file_path();
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((ModelsFile::default(), path)),
        Err(e) => return Err(Error::failed(format!("{}: {e}", path.display()))),
    };
    let file: ModelsFile = serde_json::from_slice(&raw).map_err(|e| Error::failed(format!("{}: {e}", path.display())))?;
    validate_models_file(&file).map_err(|e| e.context(path.display()))?;
    Ok((file, path))
}

fn validate_models_file(file: &ModelsFile) -> Result<()> {
    let mut defaults: BTreeMap<&str, &str> = BTreeMap::new();
    for (index, entry) in file.models.iter().enumerate() {
        if entry.provider.is_empty() || Provider::parse(&entry.provider).is_err() {
            return Err(Error::failed(format!("models[{index}]: unsupported provider {:?}", entry.provider)));
        }
        if entry.id.trim().is_empty() {
            return Err(Error::failed(format!("models[{index}]: id is required")));
        }
        if !entry.config.is_empty() && entry.provider != "codex" {
            return Err(Error::failed(format!("models[{index}]: config applies only to codex models")));
        }
        if let Some(key) = entry.config.keys().find(|key| key.trim().is_empty() || key.contains('=')) {
            return Err(Error::failed(format!("models[{index}]: invalid config key {key:?}")));
        }
        if entry.default && entry.hidden {
            return Err(Error::failed(format!("models[{index}]: a hidden model cannot be the default")));
        }
        if entry.default {
            if let Some(previous) = defaults.insert(&entry.provider, &entry.id) {
                return Err(Error::failed(format!("{} has two defaults: {previous} and {}", entry.provider, entry.id)));
            }
        }
    }
    Ok(())
}

/// The built-in catalog with models.json applied.
pub fn load_catalog() -> Result<Vec<ProviderModel>> {
    let (file, _) = read_models_file()?;
    Ok(merge_catalog(&builtin_catalog(), &file.models))
}

/// Applies models.json entries over a catalog, in file order.
pub fn merge_catalog(builtin: &[ProviderModel], entries: &[ModelEntry]) -> Vec<ProviderModel> {
    let mut catalog = builtin.to_vec();
    for entry in entries {
        let index = catalog.iter().position(|m| m.provider == entry.provider && m.id == entry.id);
        if entry.hidden {
            if let Some(index) = index {
                catalog.remove(index);
            }
            continue;
        }
        let index = match index {
            Some(index) => index,
            None => {
                catalog.push(ProviderModel {
                    provider: entry.provider.clone(),
                    id: entry.id.clone(),
                    label: String::new(),
                    efforts: Vec::new(),
                    context_window: 0,
                    default: false,
                    available: true,
                    config: BTreeMap::new(),
                    note: String::new(),
                    source: String::new(),
                });
                catalog.len() - 1
            }
        };
        if entry.default {
            for model in catalog.iter_mut().filter(|m| m.provider == entry.provider) {
                model.default = false;
            }
        }
        let model = &mut catalog[index];
        model.source = "config".into();
        if !entry.label.is_empty() {
            model.label = entry.label.clone();
        }
        if !entry.efforts.is_empty() {
            model.efforts = entry.efforts.clone();
        }
        if entry.context_window > 0 {
            model.context_window = entry.context_window;
        }
        if !entry.config.is_empty() {
            model.config = entry.config.clone();
        }
        if entry.default {
            model.default = true;
        }
    }
    catalog
}

/// The provider's default model after models.json, or `None` when the
/// provider has none.
pub fn default_model(provider: Provider) -> Result<Option<String>> {
    Ok(load_catalog()?.into_iter().find(|m| m.provider == provider.as_str() && m.default).map(|m| m.id))
}

/// The catalog's config overrides for a Codex model as sorted `KEY=VALUE`
/// strings, so the child command is deterministic.
pub fn codex_config(model: &str) -> Result<Vec<String>> {
    let catalog = load_catalog()?;
    let entry = catalog.iter().find(|m| m.provider == "codex" && m.id == model && !m.config.is_empty());
    Ok(entry.map(|m| m.config.iter().map(|(k, v)| format!("{k}={v}")).collect()).unwrap_or_default())
}

/// Validates one `--config KEY=VALUE` override and splits it.
pub fn parse_config_override(text: &str) -> Result<(String, String)> {
    match text.split_once('=') {
        Some((key, value)) if !key.trim().is_empty() => Ok((key.to_string(), value.to_string())),
        _ => Err(Error::usage(format!("config override {text:?} must be KEY=VALUE"))),
    }
}

/// Splits a comma-separated `--efforts` value, dropping blanks.
pub fn parse_efforts(text: &str) -> Vec<String> {
    text.split(',').map(str::trim).filter(|e| !e.is_empty()).map(str::to_string).collect()
}

/// Writes models.json as private, pretty-printed JSON.
pub fn write_models_file(path: &std::path::Path, file: &ModelsFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context(format!("create {}", parent.display()))?;
        crate::fsutil::set_mode(parent, 0o700).context(format!("secure {}", parent.display()))?;
    }
    let mut raw = serde_json::to_vec_pretty(file)?;
    raw.push(b'\n');
    crate::fsutil::write_private_atomic(path, &raw).context(format!("write {}", path.display()))
}

/// The result of a models.json edit. The CLI prints `message (path)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    pub message: String,
    pub path: PathBuf,
}

/// `ruddr models add PROVIDER ID [options]`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AddModel {
    pub provider: String,
    pub id: String,
    /// Replaces the label when non-empty.
    pub label: String,
    /// Replaces the efforts when set (see [`parse_efforts`]).
    pub efforts: Option<Vec<String>>,
    pub make_default: bool,
    /// `KEY=VALUE` overrides to add (Codex only).
    pub set_config: Vec<String>,
    /// Override keys to remove (Codex only).
    pub unset_config: Vec<String>,
}

fn edit_provider(provider: &str) -> Result<Provider> {
    if provider.is_empty() {
        return Err(Error::usage("unsupported provider \"\"; expected codex, claude, opencode, pi, or droid"));
    }
    Provider::parse(provider)
}

fn edit_id(id: &str) -> Result<()> {
    if id.trim().is_empty() {
        return Err(Error::usage("a model id is required"));
    }
    Ok(())
}

fn is_builtin(provider: &str, id: &str) -> bool {
    builtin_catalog().iter().any(|m| m.provider == provider && m.id == id)
}

/// Adds a model or changes an existing entry, and optionally makes it the
/// provider default.
pub fn add_model(add: &AddModel) -> Result<Edit> {
    let provider = edit_provider(&add.provider)?;
    edit_id(&add.id)?;
    if (!add.set_config.is_empty() || !add.unset_config.is_empty()) && provider != Provider::Codex {
        return Err(Error::usage("--config applies only to codex models"));
    }
    let overrides = add.set_config.iter().map(|o| parse_config_override(o)).collect::<Result<Vec<_>>>()?;
    let (mut file, path) = read_models_file()?;
    let index = match file.models.iter().position(|e| e.provider == add.provider && e.id == add.id) {
        Some(index) => index,
        None => {
            file.models.push(ModelEntry { provider: add.provider.clone(), id: add.id.clone(), ..Default::default() });
            file.models.len() - 1
        }
    };
    if add.make_default {
        for entry in file.models.iter_mut().filter(|e| e.provider == add.provider) {
            entry.default = false;
        }
    }
    let entry = &mut file.models[index];
    entry.hidden = false;
    if !add.label.is_empty() {
        entry.label = add.label.clone();
    }
    if let Some(efforts) = &add.efforts {
        entry.efforts = efforts.clone();
    }
    for (key, value) in overrides {
        entry.config.insert(key, value);
    }
    for key in &add.unset_config {
        entry.config.remove(key);
    }
    if add.make_default {
        entry.default = true;
    }
    validate_models_file(&file).map_err(|e| e.context(path.display()))?;
    write_models_file(&path, &file)?;
    let mut message = format!("added {} {}", add.provider, add.id);
    if add.make_default {
        message.push_str(" as the default");
    }
    Ok(Edit { message, path })
}

/// Makes a catalog model the provider default.
pub fn set_default_model(provider: &str, id: &str) -> Result<Edit> {
    edit_provider(provider)?;
    edit_id(id)?;
    let (mut file, path) = read_models_file()?;
    let known = merge_catalog(&builtin_catalog(), &file.models).iter().any(|m| m.provider == provider && m.id == id);
    if !known {
        return Err(Error::failed(format!(
            "{provider} {id} is not in the catalog; add it with `ruddr models add {provider} {id} --default`"
        )));
    }
    for entry in file.models.iter_mut().filter(|e| e.provider == provider) {
        entry.default = false;
    }
    match file.models.iter_mut().find(|e| e.provider == provider && e.id == id) {
        Some(entry) => entry.default = true,
        None => file.models.push(ModelEntry { provider: provider.into(), id: id.into(), default: true, ..Default::default() }),
    }
    write_models_file(&path, &file)?;
    Ok(Edit { message: format!("{provider} now defaults to {id}"), path })
}

/// Removes a models.json entry. Built-in models cannot be deleted, only
/// hidden.
pub fn remove_model(provider: &str, id: &str) -> Result<Edit> {
    edit_provider(provider)?;
    edit_id(id)?;
    let (mut file, path) = read_models_file()?;
    let index = file.models.iter().position(|e| e.provider == provider && e.id == id);
    let message = if is_builtin(provider, id) {
        let hidden = ModelEntry { provider: provider.into(), id: id.into(), hidden: true, ..Default::default() };
        match index {
            Some(index) => file.models[index] = hidden,
            None => file.models.push(hidden),
        }
        format!("hid built-in {provider} {id}")
    } else if let Some(index) = index {
        file.models.remove(index);
        format!("removed {provider} {id}")
    } else {
        return Err(Error::failed(format!("{provider} {id} is not in {}", path.display())));
    };
    write_models_file(&path, &file)?;
    Ok(Edit { message, path })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests that point RUDDR_MODELS_FILE somewhere hold this lock, because
    /// the environment is process-wide.
    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) fn with_models_file<T>(body: Option<&str>, test: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ruddr-models-{}", crate::fsutil::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("models.json");
        if let Some(body) = body {
            std::fs::write(&path, body).unwrap();
        }
        // SAFETY: ENV_LOCK serializes every test that reads or writes this variable.
        unsafe { std::env::set_var(MODELS_FILE_ENV, &path) };
        let result = test(&path);
        unsafe { std::env::remove_var(MODELS_FILE_ENV) };
        let _ = std::fs::remove_dir_all(dir);
        result
    }

    #[test]
    fn catalog_defaults() {
        with_models_file(None, |_| {
            assert_eq!(default_model(Provider::Codex).unwrap().as_deref(), Some("gpt-6-astra"));
            assert_eq!(default_model(Provider::Claude).unwrap().as_deref(), Some("claude-opus-5-5"));
            assert_eq!(default_model(Provider::Droid).unwrap().as_deref(), Some("glm-5.3-flash"));
        });
        let catalog = builtin_catalog();
        for provider in Provider::ALL {
            let defaults = catalog.iter().filter(|m| m.provider == provider.as_str() && m.default).count();
            assert_eq!(defaults, 1, "{provider} needs exactly one default");
        }
        assert!(catalog.iter().all(|m| !m.available || !m.id.is_empty()));
        let pi = catalog.iter().find(|m| m.provider == "pi").unwrap();
        assert_eq!(pi.efforts[..2], ["off".to_string(), "minimal".to_string()]);
        assert!(catalog.iter().any(|m| m.provider == "claude" && m.id == "claude-fable-5-1" && m.available));
    }

    #[test]
    fn gpt6_sol_and_luna_entries() {
        let catalog = builtin_catalog();
        for (id, efforts) in [("gpt-6.1-sol", SOL_EFFORTS), ("gpt-6-sol", SOL_EFFORTS), ("gpt-6-luna", LUNA_EFFORTS)] {
            let model = catalog.iter().find(|m| m.provider == "codex" && m.id == id).unwrap();
            assert!(model.available && !model.default);
            assert_eq!(model.efforts, efforts);
        }
    }

    #[test]
    fn edits_add_override_and_hide_models() {
        with_models_file(None, |path| {
            let added = add_model(&AddModel {
                provider: "opencode".into(),
                id: "opencode/deepseek-v4-flash".into(),
                label: "Zen Flash".into(),
                make_default: true,
                ..Default::default()
            })
            .unwrap();
            assert_eq!(added.message, "added opencode opencode/deepseek-v4-flash as the default");
            assert_eq!(added.path, path);
            add_model(&AddModel {
                provider: "codex".into(),
                id: "gpt-7-preview".into(),
                efforts: Some(parse_efforts("low, high,")),
                ..Default::default()
            })
            .unwrap();
            assert_eq!(remove_model("codex", "gpt-5.6-luna").unwrap().message, "hid built-in codex gpt-5.6-luna");
            assert_eq!(set_default_model("claude", "claude-sonnet-5").unwrap().message, "claude now defaults to claude-sonnet-5");

            let catalog = load_catalog().unwrap();
            let find = |p: &str, id: &str| catalog.iter().find(|m| m.provider == p && m.id == id).cloned();
            let zen = find("opencode", "opencode/deepseek-v4-flash").unwrap();
            assert!(zen.default && zen.label == "Zen Flash" && zen.source == "config");
            assert_eq!(default_model(Provider::OpenCode).unwrap().as_deref(), Some("opencode/deepseek-v4-flash"));
            let preview = find("codex", "gpt-7-preview").unwrap();
            assert_eq!(preview.efforts, ["low", "high"]);
            assert!(!preview.default);
            assert!(find("codex", "gpt-5.6-luna").is_none());
            assert_eq!(default_model(Provider::Claude).unwrap().as_deref(), Some("claude-sonnet-5"));
            for provider in Provider::ALL {
                assert_eq!(catalog.iter().filter(|m| m.provider == provider.as_str() && m.default).count(), 1);
            }

            assert_eq!(remove_model("codex", "gpt-7-preview").unwrap().message, "removed codex gpt-7-preview");
            assert!(set_default_model("codex", "gpt-7-preview").is_err());
            assert!(remove_model("codex", "gpt-7-preview").is_err());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
            }
        });
    }

    #[test]
    fn config_overrides_are_sorted_and_codex_only() {
        with_models_file(Some(r#"{"models":[{"provider":"codex","id":"gpt-6-sol","config":{"features.b":"false","features.a":"1"}}]}"#), |_| {
            assert_eq!(codex_config("gpt-6-sol").unwrap(), ["features.a=1", "features.b=false"]);
            assert!(codex_config("gpt-6-astra").unwrap().is_empty());
        });
        with_models_file(Some(r#"{"models":[]}"#), |_| {
            add_model(&AddModel {
                provider: "codex".into(),
                id: "gpt-6-sol".into(),
                set_config: vec!["features.x=false".into(), "y=1".into()],
                ..Default::default()
            })
            .unwrap();
            assert_eq!(codex_config("gpt-6-sol").unwrap(), ["features.x=false", "y=1"]);
            add_model(&AddModel { provider: "codex".into(), id: "gpt-6-sol".into(), unset_config: vec!["y".into()], ..Default::default() })
                .unwrap();
            assert_eq!(codex_config("gpt-6-sol").unwrap(), ["features.x=false"]);
            let error = add_model(&AddModel {
                provider: "claude".into(),
                id: "claude-opus-5-5".into(),
                set_config: vec!["a=b".into()],
                ..Default::default()
            })
            .unwrap_err();
            assert_eq!(error.exit, crate::Exit::Usage);
        });
        with_models_file(Some(r#"{"models":[{"provider":"claude","id":"claude-opus-5-5","config":{"a":"b"}}]}"#), |_| {
            assert!(load_catalog().unwrap_err().message.contains("only to codex"));
        });
        assert!(parse_config_override("no-equals-sign").is_err());
        assert!(parse_config_override(" =x").is_err());
        assert_eq!(parse_config_override("a=b=c").unwrap(), ("a".into(), "b=c".into()));
    }

    #[test]
    fn invalid_files_fail_loudly_and_go_nulls_read() {
        for body in [
            r#"{"models":[{"provider":"codex","id":"x","defualt":true}]}"#,
            r#"{"models":[{"provider":"nope","id":"x"}]}"#,
            r#"{"models":[{"provider":"codex","id":""}]}"#,
            r#"{"models":[{"provider":"codex","id":"a","default":true},{"provider":"codex","id":"b","default":true}]}"#,
            r#"{"models":[{"provider":"codex","id":"a","default":true,"hidden":true}]}"#,
        ] {
            with_models_file(Some(body), |_| assert!(default_model(Provider::Codex).is_err(), "accepted {body}"));
        }
        with_models_file(Some(r#"{"models":null}"#), |_| {
            assert_eq!(default_model(Provider::Codex).unwrap().as_deref(), Some("gpt-6-astra"));
        });
    }

    #[test]
    fn catalog_json_matches_go_field_names() {
        let model = &builtin_catalog()[0];
        let value = serde_json::to_value(model).unwrap();
        assert_eq!(value["provider"], "codex");
        assert_eq!(value["available"], true);
        assert_eq!(value["default"], true);
        assert!(value.get("config").is_none() && value.get("source").is_none());
    }
}
