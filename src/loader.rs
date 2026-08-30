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
use schemars::JsonSchema;
use serde::Serialize;

use crate::limits;

/// Whether project parameter configuration was found and loaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConfigurationLoad {
    /// No `.m1cfg` file was discovered from the project directory or its
    /// ancestors, so parameter types and calibrated values may be unavailable.
    Missing,
    /// The discovered configuration was loaded into the project model.
    Loaded { path: String },
}

/// Aggregate state of the project's discovered `.m1dbc` inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DbcLoadState {
    /// No `.m1dbc` files were found under the project directory.
    NoneFound,
    /// Every discovered `.m1dbc` was loaded.
    Complete,
    /// At least one discovered `.m1dbc` could not be loaded. The successfully
    /// loaded files remain available in the partial model.
    Partial,
}

/// One auxiliary project input that could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct SkippedInput {
    pub path: String,
    pub error: String,
}

/// What contributed to a loaded project model.
///
/// A report accompanies every successful project load so an empty diagnostic,
/// symbol, or CAN result cannot be mistaken for proof that inputs which failed
/// to load were clean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ProjectLoadReport {
    /// The main project file. A failure to load this file is returned as a tool
    /// error instead of producing a report for a nonexistent model.
    pub project: String,
    pub configuration: ConfigurationLoad,
    pub dbc_state: DbcLoadState,
    /// Successfully loaded `.m1dbc` files, in deterministic discovery order.
    pub loaded_dbcs: Vec<String>,
    /// Number of scripts successfully read and parsed into the shared script
    /// set. Syntax diagnostics do not make a readable script "skipped".
    pub script_count: usize,
    pub skipped_dbcs: Vec<SkippedInput>,
    pub skipped_scripts: Vec<SkippedInput>,
}

/// A fully augmented project and its parse-once script set, paired with the
/// report that describes every loaded or skipped auxiliary input.
pub struct LoadedProject {
    pub project: Project,
    pub scripts: Vec<ParsedScript>,
    pub report: ProjectLoadReport,
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

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

/// Load `project_path` (`Project.m1prj`), layer in the discovered
/// `parameters.m1cfg` and every readable `.m1dbc`, then read and parse every
/// project script once. Malformed auxiliary DBCs and unreadable scripts are
/// retained in the report instead of silently disappearing; a malformed main
/// project (or discovered configuration) remains an error.
pub fn load_project_full(project_path: &Path) -> Result<LoadedProject, String> {
    let mut p = Project::load(project_path).map_err(|e| format!("failed to load project: {e}"))?;

    let mut report = ProjectLoadReport {
        project: path_string(project_path),
        configuration: ConfigurationLoad::Missing,
        dbc_state: DbcLoadState::NoneFound,
        loaded_dbcs: Vec::new(),
        script_count: 0,
        skipped_dbcs: Vec::new(),
        skipped_scripts: Vec::new(),
    };

    let Some(dir) = project_path.parent() else {
        return Ok(LoadedProject {
            project: p,
            scripts: Vec::new(),
            report,
        });
    };

    if let Some(cfg) = m1_workspace::find_config_file(dir) {
        p = p
            .with_config(&cfg)
            .map_err(|e| format!("config {}: {e}", cfg.display()))?;
        report.configuration = ConfigurationLoad::Loaded {
            path: path_string(&cfg),
        };
    }

    let dbc_files = m1_workspace::find_dbc_files(dir);
    report.dbc_state = if dbc_files.is_empty() {
        DbcLoadState::NoneFound
    } else {
        DbcLoadState::Complete
    };
    for dbc in dbc_files {
        let rel = dbc
            .strip_prefix(dir)
            .unwrap_or(&dbc)
            .to_string_lossy()
            .into_owned();
        match p.augment_dbc(&dbc, &rel) {
            Ok(()) => report.loaded_dbcs.push(path_string(&dbc)),
            Err(error) => {
                report.dbc_state = DbcLoadState::Partial;
                report.skipped_dbcs.push(SkippedInput {
                    path: path_string(&dbc),
                    error: error.to_string(),
                });
            }
        }
    }

    let mut sources = Vec::new();
    for script in m1_workspace::find_scripts(dir) {
        let Some(name) = script.file_name().and_then(|name| name.to_str()) else {
            report.skipped_scripts.push(SkippedInput {
                path: path_string(&script),
                error: "script file name is not valid UTF-8".to_string(),
            });
            continue;
        };
        match m1_workspace::read_text(&script) {
            Ok(source) => sources.push((name.to_string(), source)),
            Err(error) => report.skipped_scripts.push(SkippedInput {
                path: path_string(&script),
                error: error.to_string(),
            }),
        }
    }
    let scripts = parsed::parse_all(&sources);
    report.script_count = scripts.len();

    Ok(LoadedProject {
        project: p,
        scripts,
        report,
    })
}
