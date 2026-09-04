//! Whole-project validation with bounded, per-file results.

use std::collections::{HashMap, HashSet};
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
    let mut loaded = loader::load_project_full(project_path)?;
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

    let sources_by_path: HashMap<_, _> = loaded
        .script_paths
        .iter()
        .zip(&loaded.scripts)
        .map(|(path, script)| (path.as_path(), script.cst.source()))
        .collect();
    let skipped_by_path: HashMap<_, _> = loaded
        .report
        .skipped_scripts
        .iter()
        .map(|script| (script.path.as_str(), script.error.as_str()))
        .collect();
    let paths = m1_workspace::find_scripts(project_dir)
        .into_iter()
        .filter(|path| {
            filter
                .as_ref()
                .is_none_or(|needle| path.to_string_lossy().to_lowercase().contains(needle))
        })
        .collect::<Vec<_>>();

    let mut readable = Vec::new();
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let path_text = path.to_string_lossy().into_owned();
        let source = sources_by_path.get(path.as_path()).copied();
        match source {
            Some(source) => {
                readable.push(ReadableFile {
                    path: path.clone(),
                    source: source.to_string(),
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
                error: Some(
                    skipped_by_path
                        .get(path_text.as_str())
                        .copied()
                        .unwrap_or("script was not present in the loaded project snapshot")
                        .to_string(),
                ),
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
        let input = analyze::Input::Inline {
            source: source.source.clone(),
            context_path: Some(source.path.clone()),
        };
        if selected.contains(&CheckKind::Lint) {
            let mut lint = analyze::lint(&input, false)?;
            let source_dto = diagnostic_source(&source.path);
            for diagnostic in &mut lint.diagnostics {
                diagnostic.diagnostic.source = source_dto.clone();
            }
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
            let mut format = analyze::format(&input, true)?;
            totals.files_needing_format += usize::from(format.changed);
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
