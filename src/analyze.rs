//! Running the M1 analysers (`m1-typecheck`, `m1-lint`, `m1-fmt`) in-process
//! over agent-supplied source, and mapping their result types to small
//! serializable DTOs.
//!
//! Source comes either inline (`source`) or from a file path (read through the
//! shared tolerant decoder, since MoTeC `.m1scr` files declare UTF-8 but emit
//! Windows-1252 bytes for non-ASCII characters). Line/column in the DTOs are
//! **1-based**, matching the CLIs' human-facing output.

use std::path::{Path, PathBuf};

use m1_core::{Diagnostic, Severity};
use m1_lint::diagnostic::LintCode;
use m1_typecheck::diagnostics::{RelatedPlace, TypeDiagnostic};
use m1_typecheck::project_check::{self, ProjectCheckOptions, SourceInput};
use schemars::JsonSchema;
use serde::Serialize;

use crate::{limits, loader};

/// Whether a finding belongs to source text or the project model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticScope {
    Source,
    Project,
}

/// The document a diagnostic belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiagnosticSourceDto {
    /// Source supplied directly in the MCP request.
    Inline,
    /// A file on disk. `path` is the same logical path the analyser used.
    Path { path: String },
}

/// The declaration side of a two-location type diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RelatedLocationDto {
    /// Project file containing the declaration.
    pub path: String,
    /// 1-based line in `path`.
    pub line: u32,
    pub message: String,
}

/// A single diagnostic in agent-friendly form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct DiagnosticDto {
    /// Rule code (`T030`, `L012`, or `syntax`).
    pub code: String,
    /// `error` | `warning` | `info` | `hint`.
    pub severity: String,
    /// Whether this finding is anchored in script source or in the project.
    pub scope: DiagnosticScope,
    /// Inline input, a script path, or the project file for this finding.
    pub source: DiagnosticSourceDto,
    /// 1-based start line. Retained for compatibility with the original DTO.
    pub line: u32,
    /// 1-based start column. Retained for compatibility with the original DTO.
    pub column: u32,
    /// 1-based end line of the half-open source range.
    pub end_line: u32,
    /// 1-based end column of the half-open source range.
    pub end_column: u32,
    /// 0-based byte offset where the half-open range starts.
    pub byte_start: usize,
    /// 0-based byte offset where the half-open range ends.
    pub byte_end: usize,
    /// Project symbol a project-level finding concerns, when supplied upstream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Declaration or signature locations related to this finding.
    pub related: Vec<RelatedLocationDto>,
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

fn source_dto(path: Option<&Path>) -> DiagnosticSourceDto {
    match path {
        Some(path) => DiagnosticSourceDto::Path {
            path: path.display().to_string(),
        },
        None => DiagnosticSourceDto::Inline,
    }
}

pub(crate) fn to_dto(
    code: &str,
    d: &Diagnostic,
    scope: DiagnosticScope,
    source: DiagnosticSourceDto,
) -> DiagnosticDto {
    DiagnosticDto {
        code: code.to_string(),
        severity: severity_str(d.severity).to_string(),
        scope,
        source,
        line: d.range.start.line + 1,
        column: d.range.start.column + 1,
        end_line: d.range.end.line + 1,
        end_column: d.range.end.column + 1,
        byte_start: d.byte_range.start,
        byte_end: d.byte_range.end,
        subject: None,
        related: Vec::new(),
        message: d.message.clone(),
    }
}

pub(crate) fn type_to_dto(
    diagnostic: &TypeDiagnostic,
    scope: DiagnosticScope,
    source: DiagnosticSourceDto,
    project_path: Option<&Path>,
) -> Result<DiagnosticDto, String> {
    let mut dto = to_dto(diagnostic.code.as_str(), &diagnostic.inner, scope, source);
    dto.subject.clone_from(&diagnostic.subject);
    dto.related = diagnostic
        .related
        .iter()
        .map(|related| {
            let project_path = project_path.ok_or_else(|| {
                format!(
                    "{} diagnostic carries a related location without a project path",
                    diagnostic.code.as_str()
                )
            })?;
            let (path, line) = match &related.place {
                RelatedPlace::Project { line } => (project_path.to_path_buf(), *line),
                RelatedPlace::Dbc { path, line } => {
                    let path = Path::new(path);
                    let path = if path.is_absolute() {
                        path.to_path_buf()
                    } else {
                        project_path
                            .parent()
                            .unwrap_or_else(|| Path::new(""))
                            .join(path)
                    };
                    (path, *line)
                }
            };
            Ok(RelatedLocationDto {
                path: path.display().to_string(),
                line: line + 1,
                message: related.message.clone(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(dto)
}

/// Where the source to analyse comes from.
pub enum Input {
    /// Request-provided source. `context_path` gives the unsaved buffer a
    /// logical filename and config-discovery location; it is never read.
    Inline {
        source: String,
        context_path: Option<PathBuf>,
    },
    Path(PathBuf),
}

struct ResolvedInput<'a> {
    source: String,
    script_path: Option<&'a Path>,
    inline: bool,
}

impl Input {
    /// Resolve the source text and the script path (if any) for the analysers.
    ///
    /// Both inline source and a file's contents are held to the per-request
    /// size cap ([`limits::MAX_REQUEST_SOURCE_BYTES`]); a file's size is checked
    /// (cheap metadata read) *before* its bytes are read, so an oversized file
    /// is rejected without being loaded into memory.
    fn resolve(&self) -> Result<ResolvedInput<'_>, String> {
        match self {
            Input::Inline {
                source,
                context_path,
            } => {
                let len = source.len() as u64;
                if len > limits::MAX_REQUEST_SOURCE_BYTES {
                    return Err(over_limit_msg("inline `source`", len));
                }
                Ok(ResolvedInput {
                    source: source.clone(),
                    script_path: context_path.as_deref(),
                    inline: true,
                })
            }
            Input::Path(p) => {
                let meta = std::fs::metadata(p)
                    .map_err(|e| format!("cannot read {}: {e}", p.display()))?;
                if meta.len() > limits::MAX_REQUEST_SOURCE_BYTES {
                    return Err(over_limit_msg(&format!("file {}", p.display()), meta.len()));
                }
                let text = m1_workspace::read_text(p).map_err(|e| e.to_string())?;
                Ok(ResolvedInput {
                    source: text,
                    script_path: Some(p.as_path()),
                    inline: false,
                })
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
    /// Firmware/manual target represented by the intrinsic catalogue used for
    /// builtin signatures and enum membership.
    pub catalogue_target: String,
    /// True when a `Project.m1prj` was loaded so cross-script/reference checks
    /// could run; false for a standalone snippet.
    pub project_loaded: bool,
    /// Exact auxiliary inputs that contributed to the project model. Omitted
    /// for standalone source checks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_report: Option<loader::ProjectLoadReport>,
}

/// Type-check `input`. If `project_path` names a `Project.m1prj`, it is loaded
/// so cross-script and reference-keyword resolution runs; otherwise the script
/// is checked standalone.
pub fn typecheck(input: &Input, project_path: Option<&Path>) -> Result<TypecheckOutcome, String> {
    let resolved = input.resolve()?;
    // `context_path` is analysis context, not the source document: diagnostics
    // for request-provided text must retain the explicit inline provenance
    // introduced by issue #30.
    let input_source = if resolved.inline {
        DiagnosticSourceDto::Inline
    } else {
        source_dto(resolved.script_path)
    };

    // Bound the whole-project work before loading anything: refuse a project
    // whose tree carries more than `MAX_PROJECT_SCRIPTS` scripts (fail fast, on
    // a directory walk only — see `loader::check_project_script_budget`).
    if let Some(pp) = project_path {
        loader::check_project_script_budget(pp)?;
    }

    let inline = if resolved.inline {
        resolved
            .script_path
            .map(|path| (path, resolved.source.as_str()))
    } else {
        None
    };

    // Load the project fully (m1cfg + .m1dbc), not a bare `Project::load`.
    // Request-provided source replaces its on-disk counterpart in the shared
    // script snapshot without reading the logical context path.
    let mut loaded = match project_path {
        Some(p) => Some(loader::load_project_full_with_inline(p, inline)?),
        None => None,
    };

    // One upstream entry point owns the full pass list and diagnostics policy.
    // Discover from the logical script directory first, then the project
    // directory, matching the CLI while keeping inline context paths read-only.
    let discovery_dir = resolved
        .script_path
        .and_then(Path::parent)
        .or_else(|| project_path.and_then(Path::parent));
    let options = ProjectCheckOptions::discover(discovery_dir);
    let source_input = match resolved.script_path {
        Some(path) => SourceInput::at_path(path, &resolved.source),
        None => SourceInput::inline(&resolved.source),
    };
    let result = match loaded.as_mut() {
        Some(loaded) => project_check::check(
            Some(&mut loaded.project),
            &loaded.scripts,
            &[source_input],
            &options,
        ),
        None => project_check::check(None, &[], &[source_input], &options),
    };
    let source_result = result
        .sources
        .first()
        .ok_or_else(|| "type-check pipeline returned no source result".to_string())?;

    let mut diagnostics: Vec<DiagnosticDto> = source_result
        .syntax_errors
        .iter()
        .map(|diagnostic| {
            to_dto(
                "syntax",
                diagnostic,
                DiagnosticScope::Source,
                input_source.clone(),
            )
        })
        .collect();
    diagnostics.extend(
        source_result
            .diagnostics
            .iter()
            .map(|diagnostic| {
                type_to_dto(
                    diagnostic,
                    DiagnosticScope::Source,
                    input_source.clone(),
                    project_path,
                )
            })
            .collect::<Result<Vec<_>, String>>()?,
    );
    diagnostics.extend(
        result
            .project_diagnostics
            .iter()
            .map(|diagnostic| {
                type_to_dto(
                    diagnostic,
                    DiagnosticScope::Project,
                    source_dto(project_path),
                    project_path,
                )
            })
            .collect::<Result<Vec<_>, String>>()?,
    );

    let error_count = diagnostics.iter().filter(|d| d.severity == "error").count();
    let warning_count = diagnostics
        .iter()
        .filter(|d| d.severity == "warning")
        .count();

    let project_loaded = loaded.is_some();
    let load_report = loaded.map(|loaded| loaded.report);
    Ok(TypecheckOutcome {
        diagnostics,
        error_count,
        warning_count,
        catalogue_target: m1_typecheck::intrinsics::active_target().to_string(),
        project_loaded,
        load_report,
    })
}

/// The outcome of linting one script.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LintOutcome {
    pub diagnostics: Vec<LintDiagnosticDto>,
    pub error_count: usize,
    pub warning_count: usize,
    /// True when a path input matched the resolved lint configuration's
    /// `exclude` globs. Excluded files are not read, parsed, diagnosed, or
    /// fixed, matching the `m1-lint` CLI.
    pub excluded: bool,
    /// Present when the caller requested a safe, read-only fix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<LintFixOutcome>,
}

/// A lint diagnostic with stable rule metadata. Syntax diagnostics use the
/// synthetic `syntax-error` name and are never fixable.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LintDiagnosticDto {
    /// The common diagnostic provenance and exact range fields.
    #[serde(flatten)]
    pub diagnostic: DiagnosticDto,
    /// Stable rule name (`eq-operator-preferred`, or `syntax-error`).
    pub name: String,
    /// Whether the pinned linter has a verified mechanical fix for this rule.
    pub fixable: bool,
}

impl std::ops::Deref for LintDiagnosticDto {
    type Target = DiagnosticDto;

    fn deref(&self) -> &Self::Target {
        &self.diagnostic
    }
}

/// Result of a requested lint fix. Fixing is read-only: `source` is returned
/// to the caller and a path input is never written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum LintFixOutcome {
    /// No enabled safe fix changed the source, including when syntax errors
    /// prevent the fixer from running or the path is excluded.
    Unchanged,
    /// The source after `fix_source_stable` reached a safe fixed point.
    Fixed { source: String },
    /// The linter rejected every proposed edit as unsafe.
    Unsafe { error: String },
}

/// Static metadata for one exact lint rule code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct LintRuleMetadata {
    pub code: String,
    pub name: String,
    /// The rule's severity before project-specific overrides.
    pub severity: String,
    pub enabled_by_default: bool,
    pub fixable: bool,
    pub summary: String,
    pub explanation: String,
}

fn lint_dto(
    code: Option<LintCode>,
    diagnostic: &Diagnostic,
    source: DiagnosticSourceDto,
) -> LintDiagnosticDto {
    let code_text = code.map_or_else(|| "syntax".to_string(), |code| code.to_string());
    LintDiagnosticDto {
        diagnostic: to_dto(&code_text, diagnostic, DiagnosticScope::Source, source),
        name: code.map_or("syntax-error", |code| code.name()).to_string(),
        fixable: code.is_some_and(|code| code.fixable()),
    }
}

/// Resolve the effective project lint configuration in the same order as the
/// `m1-lint` CLI: defaults, unified `m1-tools.toml`, then `.m1lint.toml`.
/// Keep the `Config` until exclusion is checked; converting it directly to a
/// `Registry` would discard its path globs.
fn lint_config(path: Option<&Path>) -> Result<m1_lint::config::Config, String> {
    let Some(dir) = path.and_then(Path::parent) else {
        return Ok(m1_lint::config::Config::default());
    };

    let mut config = m1_lint::config::Config::default();
    if let Some(tools) = m1_workspace::config::M1ToolsConfig::discover(dir) {
        config
            .apply_tools_config(&tools)
            .map_err(|error| error.to_string())?;
    }
    config
        .apply_discovered_file(dir)
        .map_err(|error| error.to_string())?;
    Ok(config)
}

fn lint_fix_outcome(result: Result<Option<String>, m1_lint::fix::FixError>) -> LintFixOutcome {
    match result {
        Ok(Some(source)) => LintFixOutcome::Fixed { source },
        Ok(None) => LintFixOutcome::Unchanged,
        Err(error) => LintFixOutcome::Unsafe {
            error: error.to_string(),
        },
    }
}

/// Lint `input`. When it comes from a file path, the project's lint config
/// (`.m1lint.toml` / unified `m1-tools.toml`) is discovered from that file's
/// directory so the active rule set and thresholds match the project's CLI/CI;
/// inline source uses `context_path` for discovery when supplied, otherwise it
/// falls back to the default rule set. When `fix` is true, the same configured
/// runner computes a safe fixed point and returns it without writing path
/// input. Syntax errors bypass fixing.
pub fn lint(input: &Input, fix: bool) -> Result<LintOutcome, String> {
    // Excluded file-backed paths must be skipped before any read, but inline
    // bytes already belong to the request and always remain subject to the
    // request cap—even when their logical context path matches an exclusion.
    if let Input::Inline { source, .. } = input {
        let len = source.len() as u64;
        if len > limits::MAX_REQUEST_SOURCE_BYTES {
            return Err(over_limit_msg("inline `source`", len));
        }
    }
    let path = match input {
        Input::Inline { context_path, .. } => context_path.as_deref(),
        Input::Path(path) => Some(path.as_path()),
    };
    let config = lint_config(path)?;
    if path.is_some_and(|path| config.is_excluded(path)) {
        return Ok(LintOutcome {
            diagnostics: Vec::new(),
            error_count: 0,
            warning_count: 0,
            excluded: true,
            fix: fix.then_some(LintFixOutcome::Unchanged),
        });
    }

    let resolved = input.resolve()?;
    let input_source = if resolved.inline {
        DiagnosticSourceDto::Inline
    } else {
        source_dto(resolved.script_path)
    };

    let registry = m1_lint::registry::Registry::from_config(&config);
    let runner = m1_lint::runner::Runner::new(registry);
    let run = runner.run_source(&resolved.source);

    let mut diagnostics: Vec<LintDiagnosticDto> = run
        .syntax_errors
        .iter()
        .map(|diagnostic| lint_dto(None, diagnostic, input_source.clone()))
        .collect();
    diagnostics.extend(run.diagnostics.iter().map(|diagnostic| {
        lint_dto(
            Some(diagnostic.code),
            &diagnostic.inner,
            input_source.clone(),
        )
    }));

    let error_count = diagnostics.iter().filter(|d| d.severity == "error").count();
    let warning_count = diagnostics
        .iter()
        .filter(|d| d.severity == "warning")
        .count();
    let fix = fix.then(|| {
        if run.syntax_errors.is_empty() {
            lint_fix_outcome(runner.fix_source_stable(&resolved.source))
        } else {
            LintFixOutcome::Unchanged
        }
    });

    Ok(LintOutcome {
        diagnostics,
        error_count,
        warning_count,
        excluded: false,
        fix,
    })
}

/// Look up the pinned linter's metadata and full explanation for an exact
/// uppercase L-code such as `L004`.
pub fn lint_rule(code: &str) -> Result<LintRuleMetadata, String> {
    let code = LintCode::from_code_str(code)
        .ok_or_else(|| format!("unknown lint rule `{code}`; expected an exact L-code"))?;
    Ok(LintRuleMetadata {
        code: code.to_string(),
        name: code.name().to_string(),
        severity: code.severity().to_string(),
        enabled_by_default: !code.off_by_default(),
        fixable: code.fixable(),
        summary: code.summary().to_string(),
        explanation: m1_lint::report::explain(code).to_string(),
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
/// K&R, not the default Allman its CI would then reject. Inline source without
/// a `context_path` uses the defaults. In `check_only` mode the formatted text
/// is not returned, only whether the source is already formatted
/// (`changed == false`).
pub fn format(input: &Input, check_only: bool) -> Result<FormatOutcome, String> {
    let resolved = input.resolve()?;

    let opts = resolved
        .script_path
        .and_then(Path::parent)
        .map(resolve_format_options);
    let result = match &opts {
        Some(o) => m1_fmt::format_str_with(&resolved.source, o),
        None => m1_fmt::format_str(&resolved.source),
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

#[cfg(test)]
mod tests {
    use super::{LintFixOutcome, lint_fix_outcome};

    #[test]
    fn rejected_linter_fix_maps_to_unsafe_outcome() {
        assert_eq!(
            lint_fix_outcome(Err(m1_lint::fix::FixError::TokensChanged)),
            LintFixOutcome::Unsafe {
                error: "fix would change program semantics".to_string(),
            }
        );
    }
}
