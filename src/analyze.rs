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
use m1_typecheck::cross_script::{self, ChannelTaints};
use m1_typecheck::diagnostics::TypeDiagnostic;
use schemars::JsonSchema;
use serde::Serialize;

use crate::{limits, loader};

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
    ///
    /// Both inline source and a file's contents are held to the per-request
    /// size cap ([`limits::MAX_REQUEST_SOURCE_BYTES`]); a file's size is checked
    /// (cheap metadata read) *before* its bytes are read, so an oversized file
    /// is rejected without being loaded into memory.
    fn resolve(&self) -> Result<(String, Option<&Path>), String> {
        match self {
            Input::Inline(s) => {
                let len = s.len() as u64;
                if len > limits::MAX_REQUEST_SOURCE_BYTES {
                    return Err(over_limit_msg("inline `source`", len));
                }
                Ok((s.clone(), None))
            }
            Input::Path(p) => {
                let meta = std::fs::metadata(p)
                    .map_err(|e| format!("cannot read {}: {e}", p.display()))?;
                if meta.len() > limits::MAX_REQUEST_SOURCE_BYTES {
                    return Err(over_limit_msg(&format!("file {}", p.display()), meta.len()));
                }
                let text = m1_workspace::read_text(p).map_err(|e| e.to_string())?;
                Ok((text, Some(p.as_path())))
            }
        }
    }
}

/// A uniform "too big" error naming the offending input, its size, and the cap.
fn over_limit_msg(what: &str, size: u64) -> String {
    format!(
        "{what} is {size} bytes, which exceeds the {} byte ({} MiB) per-request limit; \
         analyse a smaller snippet or split the file",
        limits::MAX_REQUEST_SOURCE_BYTES,
        limits::MAX_REQUEST_SOURCE_BYTES / (1024 * 1024),
    )
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
    let (source, script_path) = input.resolve()?;

    // Bound the whole-project work before loading anything: refuse a project
    // whose tree carries more than `MAX_PROJECT_SCRIPTS` scripts (fail fast, on
    // a directory walk only — see `loader::check_project_script_budget`).
    if let Some(pp) = project_path {
        loader::check_project_script_budget(pp)?;
    }

    // Load the project fully (m1cfg + .m1dbc), not a bare `Project::load`.
    let mut project = match project_path {
        Some(p) => Some(loader::load_project_full(p)?),
        None => None,
    };

    // Whole-project context: solve the cross-script channel-taint graph, infer
    // user-function return types, and run the project-wide passes — the same
    // work the CLI does. A bare per-file check silently dropped cross-script
    // T080/T081, inferred return types, and the T088–T107 project audits, so a
    // project the tool claims to check came back falsely clean.
    let mut project_diags: Vec<DiagnosticDto> = Vec::new();
    let channels = match (project.as_mut(), project_path) {
        (Some(p), Some(pp)) => {
            let scripts = loader::gather_project_scripts(pp);
            p.infer_return_types(&scripts);
            let channels = cross_script::solve(p, &scripts);

            let mut pd: Vec<TypeDiagnostic> = Vec::new();
            // Default-on passes, matching the CLI (T089 rate-inversion stays off).
            pd.extend(m1_typecheck::schedule::check(
                p, &scripts, true, false, true,
            ));
            pd.extend(m1_typecheck::schedule::check_usage(p, &scripts, true, true));
            pd.extend(m1_typecheck::schedule::check_multi_writers(p, &scripts));
            pd.extend(m1_typecheck::schedule::check_cross_fn_assignment(
                p, &scripts,
            ));
            pd.extend(m1_typecheck::schedule::check_reachability(p, &scripts));
            pd.extend(m1_typecheck::dbc_init::check(p, &scripts));
            project_diags = pd
                .iter()
                .map(|d| to_dto(d.code.as_str(), &d.inner))
                .collect();

            channels
        }
        _ => ChannelTaints::default(),
    };

    let enabled: HashSet<String> = HashSet::new();
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
    diagnostics.extend(project_diags);

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

/// Lint `input`. When it comes from a file path, the project's lint config
/// (`.m1lint.toml` / unified `m1-tools.toml`) is discovered from that file's
/// directory so the active rule set and thresholds match the project's CLI/CI;
/// inline source falls back to the default rule set.
pub fn lint(input: &Input) -> Result<LintOutcome, String> {
    let (source, path) = input.resolve()?;

    let registry = match path
        .and_then(Path::parent)
        .and_then(|dir| m1_lint::config::Config::discover(dir).ok())
    {
        Some(cfg) => m1_lint::registry::Registry::from_config(&cfg),
        None => m1_lint::registry::Registry::default(),
    };
    let runner = m1_lint::runner::Runner::new(registry);
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

/// Resolve `FormatOptions` for a file in `dir` from the unified `m1-tools.toml`
/// `[format]` section then the tool-specific `.m1fmt.toml`, so the server
/// formats a project with the same settings as its CLI/CI (e.g. a
/// `brace_style = "kr"` project is not reformatted to the default Allman its CI
/// then rejects). Mirrors `m1-fmt`'s own `resolve_opts` config layering; a
/// future toolchain bump can call the shared `m1_fmt::config::resolve_options`
/// added in m1-fmt v0.17.0 instead of this local copy.
fn resolve_format_options(dir: &Path) -> m1_fmt::FormatOptions {
    let mut o = m1_fmt::FormatOptions::default();

    // Layer 1: the unified m1-tools.toml [format] section.
    if let Some(tc) = m1_workspace::config::M1ToolsConfig::discover(dir) {
        let f = tc.format;
        if let Some(n) = f.line_width {
            o.line_width = n;
        }
        if let Some(n) = f.max_blank_lines {
            o.max_blank_lines = n;
        }
        if let Some(n) = f.indent_width {
            o.indent_width = n;
        }
        if let Some(s) = f
            .indent_style
            .as_deref()
            .and_then(m1_fmt::config::parse_indent_style)
        {
            o.indent_style = s;
        }
        if let Some(s) = f
            .brace_style
            .as_deref()
            .and_then(m1_fmt::config::parse_brace_style)
        {
            o.brace_style = s;
        }
        if let Some(n) = f.continuation_indent {
            o.continuation_indent = n;
        }
        if let Some(b) = f.align_assignments {
            o.align_assignments = b;
        }
        if let Some(b) = f.reflow_comments {
            o.reflow_comments = b;
        }
        if let Some(b) = f.final_blank_line {
            o.final_blank_line = b;
        }
    }

    // Layer 2: the tool-specific .m1fmt.toml overrides the unified file.
    if let Some(cfg) = m1_fmt::config::discover(dir) {
        if let Some(n) = cfg.max_line_length {
            o.line_width = n;
        }
        if let Some(n) = cfg.max_blank_lines {
            o.max_blank_lines = n;
        }
        if let Some(n) = cfg.indent_width {
            o.indent_width = n;
        }
        if let Some(s) = cfg.indent_style {
            o.indent_style = s;
        }
        if let Some(s) = cfg.brace_style {
            o.brace_style = s;
        }
        if let Some(n) = cfg.continuation_indent {
            o.continuation_indent = n;
        }
        if let Some(b) = cfg.align_assignments {
            o.align_assignments = b;
        }
        if let Some(b) = cfg.reflow_comments {
            o.reflow_comments = b;
        }
        if let Some(b) = cfg.final_blank_line {
            o.final_blank_line = b;
        }
    }
    o
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

/// Format `input`. When it comes from a file path, the project's format config
/// (`.m1fmt.toml` / unified `m1-tools.toml`) is discovered from that file's
/// directory — so, e.g., a project that sets `brace_style = "kr"` is formatted
/// K&R, not the default Allman its CI would then reject. Inline source uses the
/// defaults. In `check_only` mode the formatted text is not returned — only
/// whether the source is already formatted (`changed == false`).
pub fn format(input: &Input, check_only: bool) -> Result<FormatOutcome, String> {
    let (source, path) = input.resolve()?;

    let opts = path.and_then(Path::parent).map(resolve_format_options);
    let result = match &opts {
        Some(o) => m1_fmt::format_str_with(&source, o),
        None => m1_fmt::format_str(&source),
    }
    .map_err(|e| format!("format failed: {e}"))?;
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
