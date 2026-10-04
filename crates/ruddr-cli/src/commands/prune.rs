//! Registry maintenance. Run directories and their contents are never removed.

use super::{args, runs::print_json};
use ruddr_core::{Result, registry};
use std::io::Write;

pub fn run(out: &mut dyn Write, argv: Vec<String>) -> Result<()> {
    let specs = [
        args::flag("apply", "remove references to missing state directories; default is a dry run"),
        args::flag("json", "print entries, counts, and unreadable paths as JSON"),
    ];
    let parsed = args::parse("prune", &specs, &argv)?;
    args::no_positionals("prune", &parsed)?;
    let report = registry::prune(&ruddr_core::paths::registry_dirs_for_discovery(), parsed.bool("apply"))?;
    if parsed.bool("json") {
        let mut json = serde_json::to_value(&report)?;
        json["count"] = report.entries.len().into();
        return print_json(out, &json);
    }
    let action = if report.applied { "removed" } else { "would remove" };
    for entry in &report.entries {
        writeln!(out, "{action}: {} ({})", entry.state_dir.display(), entry.entry.display())?;
    }
    for issue in &report.unreadable {
        writeln!(out, "kept unreadable: {}: {}", issue.path.display(), issue.error)?;
    }
    writeln!(
        out,
        "{action} {} registry entries; kept {}; unreadable {}",
        report.entries.len(),
        report.kept,
        report.unreadable.len()
    )?;
    Ok(())
}
