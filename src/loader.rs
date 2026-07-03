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
    let Some(root) = project_path.parent() else {
        return Vec::new();
    };
    let pairs: Vec<(String, String)> = m1_workspace::find_scripts(root)
        .iter()
        .filter_map(|f| {
            Some((
                f.file_name()?.to_str()?.to_string(),
                m1_workspace::read_text(f).ok()?,
            ))
        })
        .collect();
    parsed::parse_all(&pairs)
}
