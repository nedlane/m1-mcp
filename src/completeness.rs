//! Analysis-completeness telemetry for a loaded M1 project.
//!
//! A clean diagnostic result can still contain unknown expression types,
//! opaque references, or calls outside the embedded intrinsic catalogue. This
//! module exposes `m1-typecheck`'s coverage report without turning those gaps
//! into findings or a pass/fail gate.

use std::path::Path;

use m1_typecheck::completeness::CompletenessReport;
use schemars::JsonSchema;
use serde::Serialize;

use crate::loader::{self, ConfigurationLoad, ProjectLoadReport};

/// Coverage telemetry for the selected project scripts. Every
/// [`CompletenessReport`] field is retained under its upstream name; the three
/// derived metrics make the report directly usable by an MCP client, and
/// `load_report` names the exact auxiliary inputs behind the model.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct CompletenessOutcome {
    pub scripts_total: usize,
    pub scripts_analysed: usize,
    pub scripts_with_syntax_errors: usize,
    pub scripts_skipped_deep: usize,
    pub expressions_total: usize,
    pub expressions_typed: usize,
    /// Percentage of expression nodes with a known type, rounded to one
    /// decimal. Empty input is 100 percent because nothing is unknown.
    pub typed_percent: f64,
    pub references_total: usize,
    pub references_resolved: usize,
    pub references_opaque: usize,
    pub references_unresolved: usize,
    /// Percentage of references resolved to a local, project symbol, or
    /// builtin, rounded to one decimal.
    pub resolved_percent: f64,
    pub intrinsic_calls_total: usize,
    pub intrinsic_calls_unmodelled: usize,
    pub when_subjects_total: usize,
    pub when_subjects_incomplete: usize,
    pub cfg_loaded: bool,
    pub dbc_loaded: bool,
    /// Firmware/manual target represented by the embedded intrinsic catalogue.
    pub catalogue_target: String,
    pub load_report: ProjectLoadReport,
}

impl CompletenessOutcome {
    fn from_report(report: CompletenessReport, load_report: ProjectLoadReport) -> Self {
        Self {
            scripts_total: report.scripts_total,
            scripts_analysed: report.scripts_analysed(),
            scripts_with_syntax_errors: report.scripts_with_syntax_errors,
            scripts_skipped_deep: report.scripts_skipped_deep,
            expressions_total: report.expressions_total,
            expressions_typed: report.expressions_typed,
            typed_percent: report.typed_percent(),
            references_total: report.references_total,
            references_resolved: report.references_resolved,
            references_opaque: report.references_opaque,
            references_unresolved: report.references_unresolved,
            resolved_percent: report.resolved_percent(),
            intrinsic_calls_total: report.intrinsic_calls_total,
            intrinsic_calls_unmodelled: report.intrinsic_calls_unmodelled,
            when_subjects_total: report.when_subjects_total,
            when_subjects_incomplete: report.when_subjects_incomplete,
            cfg_loaded: report.cfg_loaded,
            dbc_loaded: report.dbc_loaded,
            catalogue_target: report.catalogue_target.to_string(),
            load_report,
        }
    }
}

/// Analyse a project, optionally retaining only scripts whose filename contains
/// `script_filter` (case-insensitive). The full script set is used first for
/// user-function return inference; the filter only narrows the reported
/// coverage population.
pub fn analyze_project(
    project_path: &Path,
    script_filter: Option<&str>,
) -> Result<CompletenessOutcome, String> {
    loader::check_project_script_budget(project_path)?;
    let mut loaded = loader::load_project_full(project_path)?;

    let cfg_loaded = matches!(
        loaded.report.configuration,
        ConfigurationLoad::Loaded { .. }
    );
    let dbc_loaded = !loaded.report.loaded_dbcs.is_empty();

    loaded.project.infer_return_types(&loaded.scripts);
    if let Some(filter) = script_filter {
        let filter = filter.to_ascii_lowercase();
        loaded
            .scripts
            .retain(|script| script.name.to_ascii_lowercase().contains(&filter));
    }

    let report = m1_typecheck::completeness::analyze(
        Some(&loaded.project),
        &loaded.scripts,
        cfg_loaded,
        dbc_loaded,
    );
    Ok(CompletenessOutcome::from_report(report, loaded.report))
}
