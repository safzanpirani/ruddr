//! `ruddr models [--json]`, `models path`, and `models add|default|remove`.
//! Argument parsing and output live here. The catalog itself (built-in
//! models, `models.json` overrides, and edits) belongs to
//! `ruddr_core::models`, which the runner branch provides; [`Catalog`] is the
//! interface this command needs from it.

use super::args;
use ruddr_core::{Error, Result};
use std::io::Write;
use std::path::PathBuf;

const PROVIDERS: [&str; 5] = ["codex", "claude", "opencode", "pi", "droid"];

/// One catalog entry as `models` lists it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Row {
    pub provider: String,
    pub id: String,
    pub default: bool,
    pub available: bool,
    /// Why an unavailable provider has no models.
    pub note: String,
    /// Added or changed by models.json.
    pub from_file: bool,
}

/// What `models add` changes on one entry.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AddOptions {
    pub label: Option<String>,
    /// Replaces the entry's efforts when set.
    pub efforts: Option<Vec<String>>,
    pub make_default: bool,
    /// `KEY=VALUE` Codex config overrides to set, in order.
    pub set_config: Vec<(String, String)>,
    /// Config keys to remove.
    pub unset_config: Vec<String>,
}

/// What `models remove` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    /// A built-in model was marked hidden in models.json.
    HidBuiltin,
    /// A models.json entry was deleted.
    Removed,
}

/// The model catalog API this command needs. The integrator implements it
/// over `ruddr_core::models`.
pub trait Catalog {
    /// The models.json path (`RUDDR_MODELS_FILE`, else the config directory).
    fn file_path(&self) -> Result<PathBuf>;
    /// The merged catalog in display order. An invalid models.json is an error.
    fn rows(&self) -> Result<Vec<Row>>;
    /// The merged catalog as `models --json` prints it.
    fn to_json(&self) -> Result<serde_json::Value>;
    fn add(&self, provider: &str, id: &str, options: &AddOptions) -> Result<()>;
    /// Fails when the model is not in the merged catalog.
    fn set_default(&self, provider: &str, id: &str) -> Result<()>;
    /// Fails when the model is neither built in nor in models.json.
    fn remove(&self, provider: &str, id: &str) -> Result<Removal>;
}

/// Stands in until the integrator wires `ruddr_core::models` in.
/// The real catalog in `ruddr_core::models`.
pub struct Core;

impl Catalog for Core {
    fn file_path(&self) -> Result<PathBuf> {
        Ok(ruddr_core::models::file_path())
    }
    fn rows(&self) -> Result<Vec<Row>> {
        Ok(ruddr_core::models::rows()?
            .into_iter()
            .map(|r| Row {
                provider: r.provider,
                id: r.id,
                default: r.default,
                available: r.available,
                note: r.note,
                from_file: r.from_config,
            })
            .collect())
    }
    fn to_json(&self) -> Result<serde_json::Value> {
        Ok(serde_json::from_str(&ruddr_core::models::to_json()?)?)
    }
    fn add(&self, provider: &str, id: &str, options: &AddOptions) -> Result<()> {
        let options = ruddr_core::models::AddOptions {
            label: options.label.clone(),
            efforts: options.efforts.clone(),
            make_default: options.make_default,
            set_config: options.set_config.clone(),
            unset_config: options.unset_config.clone(),
        };
        ruddr_core::models::add(provider, id, &options)
    }
    fn set_default(&self, provider: &str, id: &str) -> Result<()> {
        ruddr_core::models::set_default(provider, id)
    }
    fn remove(&self, provider: &str, id: &str) -> Result<Removal> {
        Ok(match ruddr_core::models::remove(provider, id)? {
            ruddr_core::models::Removal::HidBuiltin => Removal::HidBuiltin,
            ruddr_core::models::Removal::Removed => Removal::Removed,
        })
    }
}

pub fn models_command(argv: Vec<String>) -> Result<()> {
    let stdout = std::io::stdout();
    run(&mut stdout.lock(), &Core, argv)
}

pub fn run(out: &mut dyn Write, catalog: &dyn Catalog, argv: Vec<String>) -> Result<()> {
    if let Some(first) = argv.first() {
        match first.as_str() {
            "add" | "default" | "remove" => return edit(out, catalog, first, &argv[1..]),
            "path" => {
                writeln!(out, "{}", catalog.file_path()?.display())?;
                return Ok(());
            }
            _ => {}
        }
    }
    let parsed = args::parse("models", &[args::flag("json", "print the catalog as JSON")], &argv)?;
    if let Some(extra) = parsed.positionals.first() {
        return Err(Error::usage(format!(
            "unknown models subcommand {extra:?}; expected add, default, remove, or path"
        )));
    }
    if parsed.bool("json") {
        return super::runs::print_json(out, &catalog.to_json()?);
    }
    for row in catalog.rows()? {
        if !row.available {
            writeln!(out, "{} ({})", row.provider, row.note)?;
            continue;
        }
        let marker = if row.default { "*" } else { " " };
        let suffix = if row.from_file { "  (models.json)" } else { "" };
        writeln!(out, "{marker} {} {}{suffix}", row.provider, row.id)?;
    }
    Ok(())
}

/// `add`, `default`, and `remove`. Flags may come before or after the
/// PROVIDER and ID arguments.
fn edit(out: &mut dyn Write, catalog: &dyn Catalog, action: &str, argv: &[String]) -> Result<()> {
    let specs = if action == "add" {
        vec![
            args::value("label", "TEXT", "display name"),
            args::value("efforts", "LIST", "comma-separated reasoning efforts the model accepts"),
            args::flag("default", "make it the provider default"),
            args::multi("config", "KEY=VALUE", "Codex config override for runs on this model (repeatable)"),
            args::multi("unset-config", "KEY", "remove a Codex config override (repeatable)"),
        ]
    } else {
        Vec::new()
    };
    let parsed = args::parse(&format!("models {action}"), &specs, argv)?;
    let [provider, id] = parsed.positionals.as_slice() else {
        return Err(Error::usage(format!("usage: ruddr models {action} PROVIDER MODEL_ID")));
    };
    if !PROVIDERS.contains(&provider.as_str()) {
        return Err(Error::usage(format!(
            "unsupported provider {provider:?}; expected codex, claude, opencode, pi, or droid"
        )));
    }
    let mut set_config = Vec::new();
    for pair in parsed.all("config") {
        match pair.split_once('=') {
            Some((key, value)) if !key.trim().is_empty() => set_config.push((key.to_string(), value.to_string())),
            _ => return Err(Error::usage(format!("config override {pair:?} must be KEY=VALUE"))),
        }
    }
    let unset_config = parsed.all("unset-config");
    if unset_config.iter().any(String::is_empty) {
        return Err(Error::usage("--unset-config needs a KEY"));
    }
    if (!set_config.is_empty() || !unset_config.is_empty()) && provider != "codex" {
        return Err(Error::usage("--config applies only to codex models"));
    }
    let message = match action {
        "add" => {
            let options = AddOptions {
                label: parsed.string("label").filter(|l| !l.is_empty()),
                efforts: parsed
                    .string("efforts")
                    .filter(|e| !e.is_empty())
                    .map(|list| list.split(',').map(str::trim).filter(|e| !e.is_empty()).map(String::from).collect()),
                make_default: parsed.bool("default"),
                set_config,
                unset_config,
            };
            catalog.add(provider, id, &options)?;
            let mut message = format!("added {provider} {id}");
            if options.make_default {
                message.push_str(" as the default");
            }
            message
        }
        "default" => {
            catalog.set_default(provider, id)?;
            format!("{provider} now defaults to {id}")
        }
        _ => match catalog.remove(provider, id)? {
            Removal::HidBuiltin => format!("hid built-in {provider} {id}"),
            Removal::Removed => format!("removed {provider} {id}"),
        },
    };
    writeln!(out, "{message} ({})", catalog.file_path()?.display())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records edits and serves a fixed catalog.
    #[derive(Default)]
    struct Fake {
        rows: Vec<Row>,
        added: RefCell<Vec<(String, String, AddOptions)>>,
        defaults: RefCell<Vec<(String, String)>>,
    }

    impl Catalog for Fake {
        fn file_path(&self) -> Result<PathBuf> {
            Ok(PathBuf::from("/cfg/ruddr/models.json"))
        }
        fn rows(&self) -> Result<Vec<Row>> {
            Ok(self.rows.clone())
        }
        fn to_json(&self) -> Result<serde_json::Value> {
            Ok(serde_json::json!([{"provider": "codex", "id": "gpt-6-astra", "default": true, "available": true}]))
        }
        fn add(&self, provider: &str, id: &str, options: &AddOptions) -> Result<()> {
            self.added.borrow_mut().push((provider.into(), id.into(), options.clone()));
            Ok(())
        }
        fn set_default(&self, provider: &str, id: &str) -> Result<()> {
            self.defaults.borrow_mut().push((provider.into(), id.into()));
            Ok(())
        }
        fn remove(&self, _: &str, id: &str) -> Result<Removal> {
            Ok(if id.starts_with("gpt") {
                Removal::HidBuiltin
            } else {
                Removal::Removed
            })
        }
    }

    fn run_text(catalog: &Fake, argv: &[&str]) -> Result<String> {
        let mut out = Vec::new();
        run(&mut out, catalog, argv.iter().map(|s| s.to_string()).collect())?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn lists_the_catalog_like_go() {
        let catalog = Fake {
            rows: vec![
                Row {
                    provider: "codex".into(),
                    id: "gpt-6-astra".into(),
                    default: true,
                    available: true,
                    ..Default::default()
                },
                Row {
                    provider: "codex".into(),
                    id: "gpt-7".into(),
                    available: true,
                    from_file: true,
                    ..Default::default()
                },
                Row {
                    provider: "pi".into(),
                    note: "pi is not installed".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            run_text(&catalog, &[]).unwrap(),
            "* codex gpt-6-astra\n  codex gpt-7  (models.json)\npi (pi is not installed)\n"
        );
        assert!(run_text(&catalog, &["--json"]).unwrap().starts_with("[\n  {"));
        assert_eq!(run_text(&catalog, &["path"]).unwrap(), "/cfg/ruddr/models.json\n");
        assert!(run_text(&catalog, &["bogus"]).is_err());
    }

    #[test]
    fn add_parses_flags_in_any_position() {
        let catalog = Fake::default();
        let text = run_text(
            &catalog,
            &[
                "add",
                "--label",
                "Sol",
                "codex",
                "gpt-6-sol",
                "--config",
                "features.x=false",
                "--config",
                "y=1",
                "--efforts",
                "low, high",
                "--default",
                "--unset-config",
                "z",
            ],
        )
        .unwrap();
        assert_eq!(text, "added codex gpt-6-sol as the default (/cfg/ruddr/models.json)\n");
        let (provider, id, options) = catalog.added.borrow()[0].clone();
        assert_eq!((provider.as_str(), id.as_str()), ("codex", "gpt-6-sol"));
        assert_eq!(
            options,
            AddOptions {
                label: Some("Sol".into()),
                efforts: Some(vec!["low".into(), "high".into()]),
                make_default: true,
                set_config: vec![("features.x".into(), "false".into()), ("y".into(), "1".into())],
                unset_config: vec!["z".into()],
            }
        );
    }

    #[test]
    fn rejects_config_where_it_cannot_apply() {
        let catalog = Fake::default();
        let error = run_text(&catalog, &["add", "claude", "claude-opus-5-5", "--config", "a=b"]).unwrap_err();
        assert_eq!(error.exit, ruddr_core::Exit::Usage);
        let error = run_text(&catalog, &["add", "codex", "x", "--config", "no-equals-sign"]).unwrap_err();
        assert_eq!(error.exit, ruddr_core::Exit::Usage);
        assert!(run_text(&catalog, &["add", "codex"]).is_err());
        assert!(run_text(&catalog, &["add", "nope", "x"]).is_err());
    }

    #[test]
    fn default_and_remove_report_what_changed() {
        let catalog = Fake::default();
        assert_eq!(
            run_text(&catalog, &["default", "claude", "claude-sonnet-5"]).unwrap(),
            "claude now defaults to claude-sonnet-5 (/cfg/ruddr/models.json)\n"
        );
        assert_eq!(catalog.defaults.borrow()[0], ("claude".into(), "claude-sonnet-5".into()));
        assert_eq!(
            run_text(&catalog, &["remove", "codex", "gpt-5.6-luna"]).unwrap(),
            "hid built-in codex gpt-5.6-luna (/cfg/ruddr/models.json)\n"
        );
        assert_eq!(
            run_text(&catalog, &["remove", "opencode", "x/y"]).unwrap(),
            "removed opencode x/y (/cfg/ruddr/models.json)\n"
        );
    }
}
