//! Listing a project's workspace symbols by loading its `Project.m1prj` — the
//! channels, parameters, constants, functions, tables and objects an agent can
//! reference from a script.

use std::path::Path;

use m1_typecheck::symbols::SymbolKind;
use m1_typecheck::types::ValueType;
use schemars::JsonSchema;
use serde::Serialize;

/// One workspace symbol in agent-friendly form.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SymbolDto {
    /// Fully-qualified path (`Root.Engine.Speed`).
    pub path: String,
    /// `channel` | `parameter` | `constant` | `function` | `method` | `table`
    /// | `group` | `reference` | `object` | `other`.
    pub kind: String,
    /// Value type (`Number`, `Boolean`, `Enum`, `String`, `Unknown`).
    pub value_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security: Option<String>,
}

/// The outcome of listing a project's symbols.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SymbolsOutcome {
    pub symbols: Vec<SymbolDto>,
    /// Total symbols in the project (before any `filter` was applied).
    pub total: usize,
    pub load_report: crate::loader::ProjectLoadReport,
}

fn kind_str(k: SymbolKind) -> &'static str {
    match k {
        SymbolKind::Channel => "channel",
        SymbolKind::Parameter => "parameter",
        SymbolKind::Constant => "constant",
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Table => "table",
        SymbolKind::Group => "group",
        SymbolKind::Reference => "reference",
        SymbolKind::Object => "object",
        SymbolKind::Other => "other",
    }
}

fn value_type_str(vt: ValueType) -> &'static str {
    match vt {
        ValueType::Boolean => "Boolean",
        ValueType::Integer => "Integer",
        ValueType::Unsigned => "Unsigned",
        ValueType::Float => "Float",
        ValueType::Enum(_) => "Enum",
        ValueType::String => "String",
        ValueType::Unknown => "Unknown",
    }
}

/// Load the project at `project_path` (a `Project.m1prj`) and list its symbols.
/// When `filter` is given, only symbols whose path contains it
/// (case-insensitive) are returned; `total` still reports the full count.
/// `limit` caps the returned list (0 = no cap).
pub fn list(
    project_path: &Path,
    filter: Option<&str>,
    limit: usize,
) -> Result<SymbolsOutcome, String> {
    // Bound the whole-project work before loading anything (fail fast): refuse
    // a project whose tree carries more than `MAX_PROJECT_SCRIPTS` scripts.
    crate::loader::check_project_script_budget(project_path)?;

    // Load the project fully (parameters.m1cfg + .m1dbc), so parameter types/
    // units and CAN signals are populated rather than Unknown/absent.
    let loaded = crate::loader::load_project_full(project_path)?;
    let project = loaded.project;
    let table = project.symbols();
    let total = table.len();

    let needle = filter.map(|f| f.to_ascii_lowercase());
    let mut symbols: Vec<SymbolDto> = table
        .iter()
        .filter(|s| match &needle {
            Some(n) => s.path.to_ascii_lowercase().contains(n),
            None => true,
        })
        .map(|s| SymbolDto {
            path: s.path.clone(),
            kind: kind_str(s.kind).to_string(),
            value_type: value_type_str(s.value_type).to_string(),
            unit: s.unit.clone(),
            security: s.security.clone(),
        })
        .collect();

    symbols.sort_by(|a, b| a.path.cmp(&b.path));
    if limit > 0 {
        symbols.truncate(limit);
    }

    Ok(SymbolsOutcome {
        symbols,
        total,
        load_report: loaded.report,
    })
}
