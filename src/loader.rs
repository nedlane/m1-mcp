//! Loading an M1 project the way the CLI does — not a bare `Project::load`.
//!
//! `Project::load` alone reads only `Project.m1prj`, leaving every parameter
//! Unknown-typed/unit-less and every CAN signal absent, so T030/T041/T042 go
//! dark on exactly the real projects this server exists to serve. The CLI
//! additionally layers in the discovered `parameters.m1cfg` and every `.m1dbc`;
//! this module does the same so an agent gets the same model as the CLI.

use std::path::{Component, Path, PathBuf};

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
    /// Full logical paths aligned one-for-one with `scripts`. Keeping paths in
    /// the shared snapshot avoids lossy filename joins for whole-project tools.
    pub script_paths: Vec<std::path::PathBuf>,
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
    discover_inputs(project_dir(project_path)).map(|_| ())
}

fn project_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// Compare logical document paths without opening an unsaved context file.
fn logical_path(path: &Path) -> Result<PathBuf, String> {
    let absolute = std::path::absolute(path).map_err(|e| e.to_string())?;
    let mut normalized = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            _ => normalized.push(part.as_os_str()),
        }
    }
    Ok(normalized)
}

#[derive(Default)]
struct ProjectInputs {
    scripts: Vec<PathBuf>,
    dbcs: Vec<PathBuf>,
}

/// The pinned workspace walker drops traversal errors. A project result must
/// account for the whole tree, so collect both input kinds in one fallible walk.
fn discover_inputs(root: &Path) -> Result<ProjectInputs, String> {
    let mut inputs = ProjectInputs::default();
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = pending.pop() {
        if depth > 64 {
            return Err(format!(
                "{}: project discovery exceeds maximum directory depth 64",
                dir.display()
            ));
        }
        let at_dir = |e: std::io::Error| format!("{}: {e}", dir.display());
        for entry in std::fs::read_dir(&dir).map_err(at_dir)? {
            let entry = entry.map_err(at_dir)?;
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|e| format!("{}: {e}", path.display()))?;
            if kind.is_dir() {
                pending.push((path, depth + 1));
                continue;
            }
            // Match workspace's deliberate descendant-symlink exclusion.
            if kind.is_symlink() {
                continue;
            }
            let target = match path.extension().and_then(|ext| ext.to_str()) {
                Some(m1_workspace::SCRIPT_EXT) => &mut inputs.scripts,
                Some(m1_workspace::DBC_EXT) => &mut inputs.dbcs,
                _ => continue,
            };
            if !kind.is_file() {
                return Err(format!(
                    "{}: expected a regular project input file",
                    path.display()
                ));
            }
            target.push(path);
            check_script_count(inputs.scripts.len())?;
        }
    }
    inputs.scripts.sort();
    inputs.dbcs.sort();
    Ok(inputs)
}

fn check_script_count(count: usize) -> Result<(), String> {
    if count > limits::MAX_PROJECT_SCRIPTS {
        return Err(format!(
            "project has at least {count} .m1scr files, which exceeds the {} script per-request limit; \
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
    load_project_full_with_inline(project_path, None)
}

/// Load one project snapshot while replacing the script named by `inline`
/// with request-provided source. The logical inline path is never read.
pub fn load_project_full_with_inline(
    project_path: &Path,
    inline: Option<(&Path, &str)>,
) -> Result<LoadedProject, String> {
    load_project(project_path, inline, true)
}

/// Load sources for callers that never use basename-keyed script semantics.
pub(crate) fn load_project_independent(project_path: &Path) -> Result<LoadedProject, String> {
    load_project(project_path, None, false)
}

fn load_project(
    project_path: &Path,
    inline: Option<(&Path, &str)>,
    require_unique_names: bool,
) -> Result<LoadedProject, String> {
    let metadata = std::fs::metadata(project_path)
        .map_err(|e| format!("failed to load project {}: {e}", project_path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "failed to load project {}: expected a regular file",
            project_path.display()
        ));
    }
    let dir = project_dir(project_path);
    let inputs = discover_inputs(dir)?;
    let inline_path = inline.map(|(path, _)| logical_path(path)).transpose()?;
    let mut script_names = std::collections::HashMap::new();
    let mut replaces_existing = false;
    for path in &inputs.scripts {
        let normalized = logical_path(path)?;
        replaces_existing |= inline_path.as_ref() == Some(&normalized);
    }
    let inline_name = inline.and_then(|(path, _)| path.file_name()?.to_str());
    check_script_count(
        inputs.scripts.len() + usize::from(inline_name.is_some() && !replaces_existing),
    )?;
    if require_unique_names {
        for path in inputs
            .scripts
            .iter()
            .chain(inline.map(|(path, _)| path.to_path_buf()).as_ref())
        {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let normalized = logical_path(path)?;
            if let Some(previous) = script_names.insert(name.to_owned(), normalized.clone())
                && previous != normalized
            {
                return Err(format!(
                    "ambiguous script filename {name:?}: {} and {}; project analyses identify scripts by filename",
                    previous.display(),
                    normalized.display()
                ));
            }
        }
    }
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

    if let Some(cfg) = m1_workspace::find_config_file(dir) {
        p = p
            .with_config(&cfg)
            .map_err(|e| format!("config {}: {e}", cfg.display()))?;
        report.configuration = ConfigurationLoad::Loaded {
            path: path_string(&cfg),
        };
    }

    let dbc_files = inputs.dbcs;
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
    let mut script_paths = Vec::new();
    let mut replaced_inline = false;
    for script in inputs.scripts {
        let Some(name) = script.file_name().and_then(|name| name.to_str()) else {
            report.skipped_scripts.push(SkippedInput {
                path: path_string(&script),
                error: "script file name is not valid UTF-8".to_string(),
            });
            continue;
        };
        let source = match inline {
            Some((_, source)) if Some(logical_path(&script)?) == inline_path => {
                replaced_inline = true;
                Ok(source.to_string())
            }
            _ => m1_workspace::read_text(&script).map_err(|error| error.to_string()),
        };
        match source {
            Ok(source) => {
                sources.push((name.to_string(), source));
                script_paths.push(script);
            }
            Err(error) => report.skipped_scripts.push(SkippedInput {
                path: path_string(&script),
                error,
            }),
        }
    }
    if !replaced_inline && let (Some(name), Some((path, source))) = (inline_name, inline) {
        sources.push((name.to_string(), source.to_string()));
        script_paths.push(path.to_path_buf());
    }
    check_script_count(sources.len())?;
    let scripts = parsed::parse_all(&sources);
    report.script_count = scripts.len();

    Ok(LoadedProject {
        project: p,
        script_paths,
        scripts,
        report,
    })
}
