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
use m1_lint::diagnostic::LintCode;
use m1_typecheck::cross_script::{self, ChannelTaints};
use m1_typecheck::diagnostics::{RelatedLocation, RelatedPlace, TypeDiagnostic};
use m1_typecheck::project::Project;
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

fn to_dto(
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

fn type_to_dto(
    diagnostic: &TypeDiagnostic,
    scope: DiagnosticScope,
    source: DiagnosticSourceDto,
    project_path: Option<&Path>,
    project: Option<&Project>,
) -> Result<DiagnosticDto, String> {
    let mut dto = to_dto(diagnostic.code.as_str(), &diagnostic.inner, scope, source);
    dto.subject.clone_from(&diagnostic.subject);
    dto.related = diagnostic
        .related
        .iter()
        .map(|related| {
            let RelatedPlace::Project { line } = related.place;
            let path = resolve_related_path_v050(diagnostic, related, line, project_path, project)?;
            Ok(RelatedLocationDto {
                path,
                line: line + 1,
                message: related.message.clone(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(dto)
}

/// Resolve the file behind m1-typecheck's project-related location.
///
/// In m1-typecheck v0.50.0, `RelatedPlace::Project` carries only a line even
/// though the declaration may live in `Project.m1prj` or a loaded `.m1dbc`.
/// Current related-location producers include the defining symbol path (full
/// or script-relative) in backticks, so resolve those complete tokens against
/// the active symbol table
/// and require one unambiguous defining file. This deliberately fails closed
/// instead of sending an agent to a plausible but incorrect path. Remove this
/// compatibility shim once upstream carries structured file identity.
fn resolve_related_path_v050(
    diagnostic: &TypeDiagnostic,
    related: &RelatedLocation,
    line: u32,
    project_path: Option<&Path>,
    project: Option<&Project>,
) -> Result<String, String> {
    let project_path = project_path.ok_or_else(|| {
        format!(
            "{} diagnostic carries a project related location without a project path",
            diagnostic.code.as_str()
        )
    })?;
    let project = project.ok_or_else(|| {
        format!(
            "{} diagnostic carries a project related location without a loaded project",
            diagnostic.code.as_str()
        )
    })?;

    let tokens: Vec<&str> = related.message.split('`').skip(1).step_by(2).collect();
    // T098 quotes the descriptive argument name before the defining function.
    // Other v0.50.0 producers put the defining symbol first; notably T030 may
    // later quote an enum type in the rendered target type.
    let token = match diagnostic.code.as_str() {
        "T098" => tokens.last(),
        _ => tokens.first(),
    }
    .copied()
    .ok_or_else(|| {
        format!(
            "{} related location at project line {} does not name a defining symbol",
            diagnostic.code.as_str(),
            line + 1
        )
    })?;
    let mut symbol_paths: Vec<String> = Vec::new();
    if let Some(symbol) = project.symbols().get(token) {
        symbol_paths.push(symbol.path.clone());
    } else {
        // A few producers render a path relative to the checked script's
        // group (for example `Helper` for `Root.Ctrl.Helper`). Only pay for a
        // suffix scan when the structured exact lookup fails.
        let suffix = format!(".{token}");
        symbol_paths.extend(
            project
                .symbols()
                .iter()
                .filter(|symbol| symbol.path.ends_with(&suffix))
                .map(|symbol| symbol.path.clone()),
        );
    }
    symbol_paths.sort();
    symbol_paths.dedup();

    let mut paths: Vec<PathBuf> = symbol_paths
        .iter()
        // SymbolTable may retain superseded entries; `get` returns the active
        // last-writer symbol used by type resolution.
        .filter_map(|path| project.symbols().get(path))
        .filter(|symbol| symbol.def_line == Some(line))
        .map(|symbol| {
            symbol
                .filename
                .as_deref()
                .filter(|filename| {
                    Path::new(filename)
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("m1dbc"))
                })
                .map_or_else(
                    || project_path.to_path_buf(),
                    |filename| {
                        project_path
                            .parent()
                            .unwrap_or_else(|| Path::new(""))
                            .join(filename)
                    },
                )
        })
        .collect();
    paths.sort();
    paths.dedup();

    match paths.as_slice() {
        [path] => Ok(path.display().to_string()),
        [] => Err(format!(
            "{} related location at project line {} does not identify a defining symbol",
            diagnostic.code.as_str(),
            line + 1
        )),
        _ => Err(format!(
            "{} related location at project line {} resolves to multiple files: {}",
            diagnostic.code.as_str(),
            line + 1,
            paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
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
    let input_source = source_dto(script_path);

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
                .map(|diagnostic| {
                    type_to_dto(
                        diagnostic,
                        DiagnosticScope::Project,
                        source_dto(Some(pp)),
                        Some(pp),
                        Some(&*p),
                    )
                })
                .collect::<Result<Vec<_>, String>>()?;

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
        result
            .diagnostics
            .iter()
            .map(|diagnostic| {
                type_to_dto(
                    diagnostic,
                    DiagnosticScope::Source,
                    input_source.clone(),
                    project_path,
                    project.as_ref(),
                )
            })
            .collect::<Result<Vec<_>, String>>()?,
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
    pub diagnostics: Vec<LintDiagnosticDto>,
    pub error_count: usize,
    pub warning_count: usize,
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
    /// prevent the fixer from running.
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

/// Resolve the effective project lint registry in the same order as the
/// `m1-lint` CLI: defaults, unified `m1-tools.toml`, then `.m1lint.toml`.
fn lint_registry(path: Option<&Path>) -> Result<m1_lint::registry::Registry, String> {
    let Some(dir) = path.and_then(Path::parent) else {
        return Ok(m1_lint::registry::Registry::default());
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
    Ok(m1_lint::registry::Registry::from_config(&config))
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
/// inline source falls back to the default rule set. When `fix` is true, the
/// same configured runner computes a safe fixed point and returns it without
/// writing path input. Syntax errors bypass fixing.
pub fn lint(input: &Input, fix: bool) -> Result<LintOutcome, String> {
    let (source, path) = input.resolve()?;
    let input_source = source_dto(path);

    let registry = lint_registry(path)?;
    let runner = m1_lint::runner::Runner::new(registry);
    let run = runner.run_source(&source);

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
            lint_fix_outcome(runner.fix_source_stable(&source))
        } else {
            LintFixOutcome::Unchanged
        }
    });

    Ok(LintOutcome {
        diagnostics,
        error_count,
        warning_count,
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
