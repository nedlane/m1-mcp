//! Whole-project validation with bounded, per-file results.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use m1_typecheck::project_check::{self as upstream, SourceInput};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::analyze::{self, DiagnosticDto, DiagnosticScope, DiagnosticSourceDto};
use crate::{limits, loader};

const DEFAULT_PER_FILE_DIAGNOSTIC_LIMIT: usize = 100;
const MAX_PER_FILE_DIAGNOSTIC_LIMIT: usize = 1000;

/// An analyser selected for a whole-project check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    Typecheck,
    Lint,
    Format,
}

/// Options for one whole-project validation request.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ProjectCheckOptions {
    /// Checks to run. The default runs typecheck, lint, and format.
    pub checks: Vec<CheckKind>,
    /// Optional case-insensitive substring matched against each script path.
    pub filter: Option<String>,
    /// Maximum diagnostic records returned for each file.
    pub per_file_diagnostic_limit: usize,
}

impl Default for ProjectCheckOptions {
    fn default() -> Self {
        Self {
            checks: vec![CheckKind::Typecheck, CheckKind::Lint, CheckKind::Format],
            filter: None,
            per_file_diagnostic_limit: DEFAULT_PER_FILE_DIAGNOSTIC_LIMIT,
        }
    }
}

/// Type-check findings belonging to one script.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FileTypecheckResult {
    pub diagnostics: Vec<DiagnosticDto>,
    pub error_count: usize,
    pub warning_count: usize,
}

/// Lint findings belonging to one script.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FileLintResult {
    pub diagnostics: Vec<analyze::LintDiagnosticDto>,
    pub error_count: usize,
    pub warning_count: usize,
    pub excluded: bool,
}

/// Check-only formatter result for one script.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FileFormatResult {
    pub changed: bool,
    pub warnings: Vec<analyze::FormatWarningDto>,
}

/// Results associated with one discovered script.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProjectFileResult {
    pub path: String,
    /// Present when the script could not be read or associated with the loaded
    /// project snapshot. No analyser result is fabricated for skipped input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub typecheck: Option<FileTypecheckResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lint: Option<FileLintResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<FileFormatResult>,
    pub diagnostics_truncated: bool,
}

/// Aggregate counts before response truncation.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ProjectCheckTotals {
    pub files: usize,
    pub files_checked: usize,
    pub errors: usize,
    pub warnings: usize,
    pub lint_findings: usize,
    pub files_needing_format: usize,
    pub diagnostics_returned: usize,
}

/// Bounded result of checking every selected project script.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProjectCheckOutcome {
    pub files: Vec<ProjectFileResult>,
    /// Type-check findings anchored in the project model rather than a script.
    pub project_diagnostics: Vec<DiagnosticDto>,
    pub totals: ProjectCheckTotals,
    pub diagnostics_truncated: bool,
    pub load_report: loader::ProjectLoadReport,
}

struct ReadableFile {
    path: PathBuf,
    source: String,
}

fn diagnostic_source(path: &Path) -> DiagnosticSourceDto {
    DiagnosticSourceDto::Path {
        path: path.display().to_string(),
    }
}

fn retain_bounded<T>(values: &mut Vec<T>, per_file: &mut usize, global: &mut usize) -> bool {
    let keep = values.len().min(*per_file).min(*global);
    let truncated = keep < values.len();
    values.truncate(keep);
    *per_file -= keep;
    *global -= keep;
    truncated
}

/// Check every matching script in `project_path` using one loaded project and
/// one shared parsed-script snapshot for the complete type-check pipeline.
pub fn check_project(
    project_path: &Path,
    options: &ProjectCheckOptions,
) -> Result<ProjectCheckOutcome, String> {
    if options.checks.is_empty() {
        return Err("`checks` must contain at least one of typecheck, lint, or format".to_string());
    }
    loader::check_project_script_budget(project_path)?;
    let loaded = loader::load_project_full(project_path)?;
    check_loaded_project(project_path, options, loaded)
}

fn check_loaded_project(
    project_path: &Path,
    options: &ProjectCheckOptions,
    mut loaded: loader::LoadedProject,
) -> Result<ProjectCheckOutcome, String> {
    let project_dir = project_path
        .parent()
        .ok_or_else(|| "project path has no parent directory".to_string())?;
    let selected: HashSet<_> = options.checks.iter().copied().collect();
    let filter = options.filter.as_deref().map(str::to_lowercase);
    let per_file_limit = options
        .per_file_diagnostic_limit
        .min(MAX_PER_FILE_DIAGNOSTIC_LIMIT);
    let mut global_budget = limits::MAX_PROJECT_RESPONSE_DIAGNOSTICS;
    let mut diagnostics_truncated = false;

    let mut snapshot = loaded
        .script_paths
        .iter()
        .zip(&loaded.scripts)
        .map(|(path, script)| (path.clone(), Some(script.cst.source().to_string()), None))
        .collect::<Vec<_>>();
    snapshot.extend(loaded.report.skipped_scripts.iter().map(|script| {
        (
            PathBuf::from(&script.path),
            None,
            Some(script.error.clone()),
        )
    }));
    snapshot.sort_by(|left, right| left.0.cmp(&right.0));
    snapshot.retain(|(path, _, _)| {
        filter
            .as_ref()
            .is_none_or(|needle| path.to_string_lossy().to_lowercase().contains(needle))
    });

    let mut readable = Vec::new();
    let mut files = Vec::with_capacity(snapshot.len());
    for (path, source, error) in snapshot {
        let path_text = path.to_string_lossy().into_owned();
        match source {
            Some(source) => {
                readable.push(ReadableFile {
                    path: path.clone(),
                    source,
                });
                files.push(ProjectFileResult {
                    path: path_text,
                    error: None,
                    typecheck: None,
                    lint: None,
                    format: None,
                    diagnostics_truncated: false,
                });
            }
            None => files.push(ProjectFileResult {
                error,
                path: path_text,
                typecheck: None,
                lint: None,
                format: None,
                diagnostics_truncated: false,
            }),
        }
    }

    let mut totals = ProjectCheckTotals {
        files: files.len(),
        files_checked: readable.len(),
        ..ProjectCheckTotals::default()
    };
    let mut project_diagnostics = Vec::new();
    if selected.contains(&CheckKind::Typecheck) {
        let inputs = readable
            .iter()
            .map(|file| SourceInput::at_path(&file.path, &file.source))
            .collect::<Vec<_>>();
        let result = upstream::check(
            Some(&mut loaded.project),
            &loaded.scripts,
            &inputs,
            &upstream::ProjectCheckOptions::discover(Some(project_dir)),
        );
        for diagnostic in &result.project_diagnostics {
            let diagnostic = analyze::type_to_dto(
                diagnostic,
                DiagnosticScope::Project,
                diagnostic_source(project_path),
                Some(project_path),
            )?;
            totals.errors += usize::from(diagnostic.severity == "error");
            totals.warnings += usize::from(diagnostic.severity == "warning");
            if global_budget > 0 {
                project_diagnostics.push(diagnostic);
                global_budget -= 1;
                totals.diagnostics_returned += 1;
            } else {
                diagnostics_truncated = true;
            }
        }
        for (file, source_result) in files
            .iter_mut()
            .filter(|file| file.error.is_none())
            .zip(result.sources)
        {
            let source = diagnostic_source(Path::new(&file.path));
            let mut diagnostics = Vec::with_capacity(per_file_limit.min(global_budget));
            let mut file_budget = per_file_limit;
            let mut error_count = 0;
            let mut warning_count = 0;
            for diagnostic in &source_result.syntax_errors {
                let diagnostic = analyze::to_dto(
                    "syntax",
                    diagnostic,
                    DiagnosticScope::Source,
                    source.clone(),
                );
                error_count += usize::from(diagnostic.severity == "error");
                warning_count += usize::from(diagnostic.severity == "warning");
                if file_budget > 0 && global_budget > 0 {
                    diagnostics.push(diagnostic);
                    file_budget -= 1;
                    global_budget -= 1;
                    totals.diagnostics_returned += 1;
                } else {
                    file.diagnostics_truncated = true;
                    diagnostics_truncated = true;
                }
            }
            for diagnostic in &source_result.diagnostics {
                let diagnostic = analyze::type_to_dto(
                    diagnostic,
                    DiagnosticScope::Source,
                    source.clone(),
                    Some(project_path),
                )?;
                error_count += usize::from(diagnostic.severity == "error");
                warning_count += usize::from(diagnostic.severity == "warning");
                if file_budget > 0 && global_budget > 0 {
                    diagnostics.push(diagnostic);
                    file_budget -= 1;
                    global_budget -= 1;
                    totals.diagnostics_returned += 1;
                } else {
                    file.diagnostics_truncated = true;
                    diagnostics_truncated = true;
                }
            }
            totals.errors += error_count;
            totals.warnings += warning_count;
            file.typecheck = Some(FileTypecheckResult {
                error_count,
                warning_count,
                diagnostics,
            });
        }
    }

    for (file, source) in files
        .iter_mut()
        .filter(|file| file.error.is_none())
        .zip(&readable)
    {
        if selected.contains(&CheckKind::Lint) {
            let mut lint = analyze::lint_loaded(&source.path, &source.source, false)?;
            totals.errors += lint.error_count;
            totals.warnings += lint.warning_count;
            totals.lint_findings += lint.diagnostics.len();
            let mut file_budget = per_file_limit.saturating_sub(
                file.typecheck
                    .as_ref()
                    .map_or(0, |typecheck| typecheck.diagnostics.len()),
            );
            file.diagnostics_truncated |=
                retain_bounded(&mut lint.diagnostics, &mut file_budget, &mut global_budget);
            totals.diagnostics_returned += lint.diagnostics.len();
            diagnostics_truncated |= file.diagnostics_truncated;
            file.lint = Some(FileLintResult {
                diagnostics: lint.diagnostics,
                error_count: lint.error_count,
                warning_count: lint.warning_count,
                excluded: lint.excluded,
            });
        }
        if selected.contains(&CheckKind::Format) {
            let mut format = analyze::format_loaded(&source.path, &source.source, true)?;
            totals.files_needing_format += usize::from(format.changed);
            totals.warnings += format.warnings.len();
            let retained = file
                .typecheck
                .as_ref()
                .map_or(0, |typecheck| typecheck.diagnostics.len())
                + file.lint.as_ref().map_or(0, |lint| lint.diagnostics.len());
            let mut file_budget = per_file_limit.saturating_sub(retained);
            file.diagnostics_truncated |=
                retain_bounded(&mut format.warnings, &mut file_budget, &mut global_budget);
            totals.diagnostics_returned += format.warnings.len();
            diagnostics_truncated |= file.diagnostics_truncated;
            file.format = Some(FileFormatResult {
                changed: format.changed,
                warnings: format.warnings,
            });
        }
    }

    Ok(ProjectCheckOutcome {
        files,
        project_diagnostics,
        totals,
        diagnostics_truncated,
        load_report: loaded.report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL_PROJECT: &str = r#"<?xml version="1.0"?>
<MoTeCM1BuildSession>
 <Project Name="Snapshot" TargetHardware="ecu120">
  <ComponentStream><List>
   <Component Classname="BuiltIn.GroupCompound" Name="Root.Test"/>
  </List></ComponentStream>
 </Project>
</MoTeCM1BuildSession>
"#;

    fn project_in(dir: &Path) -> PathBuf {
        let project = dir.join("Project.m1prj");
        std::fs::write(&project, MINIMAL_PROJECT).unwrap();
        project
    }

    #[test]
    fn loaded_snapshot_is_stable_when_directory_changes() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_in(dir.path());
        let original = dir.path().join("Original.m1scr");
        let added = dir.path().join("Added.m1scr");
        std::fs::write(&original, "local value = 1;\n").unwrap();
        let loaded = loader::load_project_full(&project).unwrap();

        std::fs::remove_file(&original).unwrap();
        std::fs::write(&added, "local value = 2;\n").unwrap();
        let outcome = check_loaded_project(
            &project,
            &ProjectCheckOptions {
                checks: vec![CheckKind::Lint],
                ..ProjectCheckOptions::default()
            },
            loaded,
        )
        .unwrap();

        assert_eq!(outcome.files.len(), 1);
        assert_eq!(outcome.files[0].path, original.display().to_string());
        assert!(outcome.files[0].error.is_none());
        assert!(outcome.files[0].lint.is_some());
        assert!(
            !outcome
                .files
                .iter()
                .any(|file| file.path == added.display().to_string())
        );
    }

    #[test]
    fn format_warnings_contribute_to_aggregate_warning_total() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_in(dir.path());
        std::fs::write(
            dir.path().join("Long.m1scr"),
            format!("// {}\n", "unbreakable".repeat(20)),
        )
        .unwrap();

        let outcome = check_project(
            &project,
            &ProjectCheckOptions {
                checks: vec![CheckKind::Format],
                ..ProjectCheckOptions::default()
            },
        )
        .unwrap();
        let format = outcome.files[0].format.as_ref().unwrap();

        assert!(!format.warnings.is_empty());
        assert_eq!(outcome.totals.warnings, format.warnings.len());
    }
}
