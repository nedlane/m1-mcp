//! Running the M1 analysers (`m1-typecheck`, `m1-lint`, `m1-fmt`) in-process
//! over agent-supplied source, and mapping their result types to small
//! serializable DTOs.
//!
//! Source comes either inline (`source`) or from a file path (read through the
//! shared tolerant decoder, since MoTeC `.m1scr` files declare UTF-8 but emit
//! Windows-1252 bytes for non-ASCII characters). Line/column in the DTOs are
//! **1-based**, matching the CLIs' human-facing output.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use m1_core::{Diagnostic, Severity};
use m1_typecheck::cross_script::ChannelTaints;
use m1_typecheck::project::Project;
use schemars::JsonSchema;
use serde::Serialize;

/// A single diagnostic in agent-friendly form (1-based line/column).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiagnosticDto {
    /// Rule code (`T030`, `L012`, or `syntax`).
    pub code: String,
    /// `error` | `warning` | `info` | `hint`.
    pub severity: String,
    pub line: u32,
    pub column: u32,
    pub message: String,
}

fn severity_str(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
        Severity::Hint => "hint",
    }
}

fn to_dto(code: &str, d: &Diagnostic) -> DiagnosticDto {
    DiagnosticDto {
        code: code.to_string(),
        severity: severity_str(d.severity).to_string(),
        line: d.range.start.line + 1,
        column: d.range.start.column + 1,
        message: d.message.clone(),
    }
}

/// Where the source to analyse comes from.
pub enum Input {
    Inline(String),
    Path(PathBuf),
}

impl Input {
    /// Resolve the source text and the script path (if any) for the analysers.
    fn resolve(&self) -> std::io::Result<(String, Option<&Path>)> {
        match self {
            Input::Inline(s) => Ok((s.clone(), None)),
            Input::Path(p) => Ok((m1_workspace::read_text(p)?, Some(p.as_path()))),
        }
    }
}

/// The outcome of type-checking one script.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TypecheckOutcome {
    pub diagnostics: Vec<DiagnosticDto>,
    pub error_count: usize,
    pub warning_count: usize,
    /// True when a `Project.m1prj` was loaded so cross-script/reference checks
    /// could run; false for a standalone snippet.
    pub project_loaded: bool,
}

/// Type-check `input`. If `project_path` names a `Project.m1prj`, it is loaded
/// so cross-script and reference-keyword resolution runs; otherwise the script
/// is checked standalone.
pub fn typecheck(input: &Input, project_path: Option<&Path>) -> Result<TypecheckOutcome, String> {
    let (source, script_path) = input.resolve().map_err(|e| e.to_string())?;

    let project = match project_path {
        Some(p) => Some(Project::load(p).map_err(|e| format!("failed to load project: {e}"))?),
        None => None,
    };

    let enabled: HashSet<String> = HashSet::new();
    let channels = ChannelTaints::default();
    let result = m1_typecheck::rules::check_script_with_channels(
        &enabled,
        project.as_ref(),
        script_path,
        &source,
        &channels,
    );

    let mut diagnostics: Vec<DiagnosticDto> = result
        .syntax_errors
        .iter()
        .map(|d| to_dto("syntax", d))
        .collect();
    diagnostics.extend(
        result
            .diagnostics
            .iter()
            .map(|d| to_dto(d.code.as_str(), &d.inner)),
    );

    let error_count = diagnostics.iter().filter(|d| d.severity == "error").count();
    let warning_count = diagnostics
        .iter()
        .filter(|d| d.severity == "warning")
        .count();

    Ok(TypecheckOutcome {
        diagnostics,
        error_count,
        warning_count,
        project_loaded: project.is_some(),
    })
}

/// The outcome of linting one script.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LintOutcome {
    pub diagnostics: Vec<DiagnosticDto>,
    pub error_count: usize,
    pub warning_count: usize,
}

/// Lint `input` with the default M1 rule set.
pub fn lint(input: &Input) -> Result<LintOutcome, String> {
    let (source, _path) = input.resolve().map_err(|e| e.to_string())?;

    let runner = m1_lint::runner::Runner::new(m1_lint::registry::Registry::default());
    let run = runner.run_source(&source);

    let mut diagnostics: Vec<DiagnosticDto> = run
        .syntax_errors
        .iter()
        .map(|d| to_dto("syntax", d))
        .collect();
    diagnostics.extend(
        run.diagnostics
            .iter()
            .map(|d| to_dto(&d.code.to_string(), &d.inner)),
    );

    let error_count = diagnostics.iter().filter(|d| d.severity == "error").count();
    let warning_count = diagnostics
        .iter()
        .filter(|d| d.severity == "warning")
        .count();

    Ok(LintOutcome {
        diagnostics,
        error_count,
        warning_count,
    })
}

/// A formatting warning (kept but non-fatal).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FormatWarningDto {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

/// The outcome of formatting one script.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FormatOutcome {
    /// Whether formatting would change the input.
    pub changed: bool,
    /// The formatted text. Omitted in `check_only` mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub formatted: Option<String>,
    pub warnings: Vec<FormatWarningDto>,
}

/// Format `input`. In `check_only` mode the formatted text is not returned —
/// only whether the source is already formatted (`changed == false`).
pub fn format(input: &Input, check_only: bool) -> Result<FormatOutcome, String> {
    let (source, _path) = input.resolve().map_err(|e| e.to_string())?;

    let result = m1_fmt::format_str(&source).map_err(|e| format!("format failed: {e}"))?;
    let warnings = result
        .warnings
        .iter()
        .map(|w| FormatWarningDto {
            line: w.line,
            column: w.col,
            message: w.message.clone(),
        })
        .collect();

    Ok(FormatOutcome {
        changed: result.changed,
        formatted: if check_only {
            None
        } else {
            Some(result.output)
        },
        warnings,
    })
}
