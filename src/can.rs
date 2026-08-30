//! MCP-facing CAN inspection with the shared project-load report attached.
//!
//! Bus binding and overlap verdicts stay in `m1-can`. This wrapper performs the
//! same bounded project load first so every DBC or script that `m1-can` had to
//! omit is visible beside its otherwise backwards-compatible response.

use std::path::Path;

use schemars::JsonSchema;
use serde::Serialize;

/// The existing `m1-can` result plus the exact auxiliary inputs that
/// contributed to its project model. Flattening preserves every established
/// top-level CAN response field.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CanOutcome {
    #[serde(flatten)]
    pub can: m1_can::CanOutcome,
    pub load_report: crate::loader::ProjectLoadReport,
}

pub fn inspect(
    project_path: &Path,
    filter: Option<&str>,
    limit: usize,
) -> Result<CanOutcome, String> {
    crate::loader::check_project_script_budget(project_path)?;
    let loaded = crate::loader::load_project_full(project_path)?;
    let can = m1_can::inspect(project_path, filter, limit)?;
    Ok(CanOutcome {
        can,
        load_report: loaded.report,
    })
}
