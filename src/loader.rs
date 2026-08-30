//! Loading an M1 project the way the CLI does — not a bare `Project::load`.
//!
//! `Project::load` alone reads only `Project.m1prj`, leaving every parameter
//! Unknown-typed/unit-less and every CAN signal absent, so T030/T041/T042 go
//! dark on exactly the real projects this server exists to serve. The CLI
//! additionally layers in the discovered `parameters.m1cfg` and every `.m1dbc`;
//! this module does the same so an agent gets the same model as the CLI.

use std::path::Path;

use m1_typecheck::parsed::{self, ParsedScript};
use m1_typecheck::project::Project;

use crate::limits;

/// Bound the whole-project work a single request can trigger: error if the
/// project rooted at `project_path` contains more than
/// [`limits::MAX_PROJECT_SCRIPTS`] `.m1scr` files. Both project-wide operations
/// (`m1_typecheck` given a `project`, and `m1_symbols`) call this first, so an
/// over-limit project fails fast rather than walking and parsing an unbounded
/// number of scripts on the single server task. The check is a directory walk
/// only — no file contents are read — and runs before the project model is
/// loaded.
pub fn check_project_script_budget(project_path: &Path) -> Result<(), String> {
    let Some(root) = project_path.parent() else {
        return Ok(());
    };
    let count = m1_workspace::find_scripts(root).len();
    if count > limits::MAX_PROJECT_SCRIPTS {
        return Err(format!(
            "project has {count} .m1scr files, which exceeds the {} script per-request limit; \
             narrow the project or run the CLI directly",
            limits::MAX_PROJECT_SCRIPTS,
        ));
    }
    Ok(())
}

/// Load `project_path` (`Project.m1prj`) and layer in the discovered
/// `parameters.m1cfg` (parameter types/units) and every `.m1dbc` under the
/// project directory (CAN signal types/ranges). A malformed `.m1dbc` is skipped
/// (kept non-fatal, like the CLI) rather than blanking the whole model.
pub fn load_project_full(project_path: &Path) -> Result<Project, String> {
    let mut p = Project::load(project_path).map_err(|e| format!("failed to load project: {e}"))?;

    let Some(dir) = project_path.parent() else {
        return Ok(p);
    };

    if let Some(cfg) = m1_workspace::find_config_file(dir) {
        p = p
            .with_config(&cfg)
            .map_err(|e| format!("config {}: {e}", cfg.display()))?;
    }

    for dbc in m1_workspace::find_dbc_files(dir) {
        let rel = dbc
            .strip_prefix(dir)
            .unwrap_or(&dbc)
            .to_string_lossy()
            .into_owned();
        // A malformed/unreadable DBC must not blank the model — skip just it.
        let _ = p.augment_dbc(&dbc, &rel);
    }
    Ok(p)
}

/// Parse every `.m1scr` under the project directory, named by file name (the
/// key the project uses to map a script to its group/function), matching the
/// CLI's whole-project pass set.
pub fn gather_project_scripts(project_path: &Path) -> Vec<ParsedScript> {
    gather_project_scripts_with_inline(project_path, None)
}

/// Parse every project script while replacing the script identified by
/// `inline` with request-provided source. Matching uses the filename because
/// that is how `Project.m1prj` maps scripts to functions. If no on-disk file
/// has that name, the inline script is still added to the project-wide set.
/// The logical path is never read.
pub fn gather_project_scripts_with_inline(
    project_path: &Path,
    inline: Option<(&Path, &str)>,
) -> Vec<ParsedScript> {
    let Some(root) = project_path.parent() else {
        return Vec::new();
    };
    let inline_name = inline.and_then(|(path, _)| path.file_name()?.to_str());
    let mut replaced_inline = false;
    let mut pairs: Vec<(String, String)> = m1_workspace::find_scripts(root)
        .iter()
        .filter_map(|f| {
            let name = f.file_name()?.to_str()?.to_string();
            let source = match inline {
                Some((_, source)) if Some(name.as_str()) == inline_name => {
                    replaced_inline = true;
                    source.to_string()
                }
                _ => m1_workspace::read_text(f).ok()?,
            };
            Some((name, source))
        })
        .collect();

    if !replaced_inline && let (Some(name), Some((_, source))) = (inline_name, inline) {
        pairs.push((name.to_string(), source.to_string()));
    }
    parsed::parse_all(&pairs)
}
