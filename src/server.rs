//! The MCP server: `M1Server` exposes the M1 toolchain as MCP tools over the
//! `rmcp` tool router. Each tool deserializes typed params, dispatches to the
//! in-process analysers in [`crate::doc`] / [`crate::analyze`] /
//! [`crate::symbols`], and returns structured JSON.

use std::path::PathBuf;

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::analyze::{self, Input};
use crate::{can, completeness, doc, symbols};

/// Guidance surfaced to the agent on connect.
const INSTRUCTIONS: &str = "\
Live access to the MoTeC M1 script toolchain. Use `m1_doc_search`/`m1_doc_lookup` to \
confirm builtin function signatures, enum spellings, and which functions are \
calibration-only BEFORE writing M1 script — do not guess. After editing a `.m1scr`, \
run `m1_typecheck`, `m1_lint`, and `m1_format` (check_only) on it. Pass either inline \
`source` or a file `path`; give `project` (a Project.m1prj) to enable cross-script and \
reference-keyword checks. Use `m1_completeness` before treating zero diagnostics as proof: it \
reports unknown types, opaque references, unmodelled intrinsics, and skipped scripts. \
`m1_symbols` lists a project's channels/parameters/functions.
CAN: never judge CAN traffic from the `.m1dbc` files alone — a DBC carries no bus until a script \
binds it with `DBC.<Name>.Init(<bus>)` (M1 Build Error 1375 if none does), and CAN identifiers are \
per bus. Call `m1_can` for any CAN question: it reports each DBC module's bus binding (with the \
`Init` call site) and classifies every repeated CAN id as `same-bus` (a real clash), \
`different-bus` (proven safe) or `unknown`. Two messages sharing an id on different buses are NOT \
a conflict.";

/// Turn the shared `source` / `path` params into an analysis input, requiring
/// exactly one of them.
fn make_input(source: Option<String>, path: Option<String>) -> Result<Input, ErrorData> {
    match (source, path) {
        (Some(s), None) => Ok(Input::Inline(s)),
        (None, Some(p)) => Ok(Input::Path(PathBuf::from(p))),
        (Some(_), Some(_)) => Err(ErrorData::invalid_params(
            "provide exactly one of `source` or `path`, not both",
            None,
        )),
        (None, None) => Err(ErrorData::invalid_params(
            "provide either `source` (inline M1 text) or `path` (a .m1scr file)",
            None,
        )),
    }
}

/// Validate an optional catalogue target before doing any project or source
/// work. The JSON-RPC error carries both a concise message and machine-readable
/// target data so clients can retry without parsing prose.
fn validate_firmware(requested: Option<&str>) -> Result<&'static str, ErrorData> {
    match requested {
        None => Ok(m1_typecheck::intrinsics::active_target()),
        Some(requested) => m1_typecheck::intrinsics::resolve_target(requested).map_err(|message| {
            ErrorData::invalid_params(
                message,
                Some(serde_json::json!({
                    "requested": requested,
                    "known_targets": m1_typecheck::intrinsics::known_targets(),
                })),
            )
        }),
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DocSearchParams {
    /// Text to search for across builtin function names, enum members, class
    /// names, data types, and their documentation.
    pub query: String,
    /// Maximum number of matches to return (default 20).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Optional firmware/manual catalogue target. Unknown targets are rejected
    /// with the list of targets embedded in this binary.
    #[serde(default)]
    pub firmware: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DocLookupParams {
    /// Exact name to look up: a library function (`Calculate.Choose` or bare
    /// `Choose`), an object method, an enum type (members expanded), an enum
    /// member, a class, or a data type.
    pub name: String,
    /// Optional firmware/manual catalogue target.
    #[serde(default)]
    pub firmware: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AnalyzeParams {
    /// Inline M1 script text. Provide this OR `path`.
    #[serde(default)]
    pub source: Option<String>,
    /// Path to a `.m1scr` file. Provide this OR `source`.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TypecheckParams {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    /// Optional path to a `Project.m1prj`. When given, the project is loaded so
    /// cross-script and reference-keyword checks run; otherwise the script is
    /// checked standalone.
    #[serde(default)]
    pub project: Option<String>,
    /// Optional firmware/manual catalogue target.
    #[serde(default)]
    pub firmware: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FormatParams {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    /// When true, do not return the formatted text — only report whether the
    /// source is already formatted (`changed`).
    #[serde(default)]
    pub check_only: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SymbolsParams {
    /// Path to a `Project.m1prj`.
    pub project: String,
    /// Only return symbols whose path contains this substring (case-insensitive).
    #[serde(default)]
    pub filter: Option<String>,
    /// Cap the number of symbols returned (default 200; the `total` field always
    /// reports the full count).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CanParams {
    /// Path to a `Project.m1prj`.
    pub project: String,
    /// Only return messages whose path contains this substring
    /// (case-insensitive). Bus bindings and id verdicts are always computed over
    /// every message, filtered or not.
    #[serde(default)]
    pub filter: Option<String>,
    /// Cap the number of messages returned (default 200; `total_messages`
    /// always reports the full count).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CompletenessParams {
    /// Path to a `Project.m1prj`.
    pub project: String,
    /// Optional case-insensitive substring used to retain matching script
    /// filenames in the coverage report.
    #[serde(default)]
    pub filter: Option<String>,
    /// Optional firmware/manual catalogue target.
    #[serde(default)]
    pub firmware: Option<String>,
}

/// The M1 toolchain MCP server.
#[derive(Debug, Clone, Default)]
pub struct M1Server;

#[tool_router]
impl M1Server {
    pub fn new() -> Self {
        Self
    }

    /// Search the M1 builtin catalogue (library functions, enums, classes,
    /// types) for a term. Use this to find the right builtin and confirm its
    /// name before writing script.
    #[tool(
        description = "Search the M1 builtin catalogue (library functions, project-object methods, firmware enums, package classes, data types) for a term. Returns ranked matches with signatures, docs, and `catalogue_target`. Optional `firmware` asserts the expected target."
    )]
    async fn m1_doc_search(
        &self,
        Parameters(p): Parameters<DocSearchParams>,
    ) -> Result<Json<doc::DocResults>, ErrorData> {
        validate_firmware(p.firmware.as_deref())?;
        Ok(Json(doc::search(&p.query, p.limit.unwrap_or(20)).into()))
    }

    /// Look up full detail for one exact M1 builtin name (all overloads, params,
    /// enum members, or class doc).
    #[tool(
        description = "Look up full detail for one exact M1 builtin name: a library function (all overloads), an object method, an enum type (members expanded), an enum member, a class, or a data type. Returns `catalogue_target`; optional `firmware` asserts it."
    )]
    async fn m1_doc_lookup(
        &self,
        Parameters(p): Parameters<DocLookupParams>,
    ) -> Result<Json<doc::DocResults>, ErrorData> {
        validate_firmware(p.firmware.as_deref())?;
        Ok(Json(doc::lookup(&p.name).into()))
    }

    /// Type-check M1 script, returning diagnostics.
    #[tool(
        description = "Type-check M1 script (inline `source` or a file `path`), returning source/project-scoped diagnostics with paths, exact position/byte ranges, project subjects, related declaration locations, and `catalogue_target`. Optional `firmware` asserts the expected target. Pass `project` (a Project.m1prj) to enable cross-script and reference-keyword checks and receive a load report naming skipped auxiliary inputs."
    )]
    async fn m1_typecheck(
        &self,
        Parameters(p): Parameters<TypecheckParams>,
    ) -> Result<Json<analyze::TypecheckOutcome>, ErrorData> {
        validate_firmware(p.firmware.as_deref())?;
        let input = make_input(p.source, p.path)?;
        let project = p.project.map(PathBuf::from);
        analyze::typecheck(&input, project.as_deref())
            .map(Json)
            .map_err(|e| ErrorData::invalid_params(e, None))
    }

    /// Report how much of a project's source the analyser could type and
    /// resolve. This is telemetry, not a finding gate.
    #[tool(
        description = "Report analysis completeness for a Project.m1prj: analysed and skipped scripts, typed expressions, resolved/opaque/unresolved references, intrinsic catalogue coverage, incomplete `when` subjects, input-load state, and the active firmware target. Optional `filter` is a case-insensitive script-filename substring."
    )]
    async fn m1_completeness(
        &self,
        Parameters(p): Parameters<CompletenessParams>,
    ) -> Result<Json<completeness::CompletenessOutcome>, ErrorData> {
        validate_firmware(p.firmware.as_deref())?;
        completeness::analyze_project(&PathBuf::from(&p.project), p.filter.as_deref())
            .map(Json)
            .map_err(|error| ErrorData::invalid_params(error, None))
    }

    /// Lint M1 script with the default rule set, returning diagnostics.
    #[tool(
        description = "Lint M1 script (inline `source` or a file `path`) with the default M1 rule set, returning source-scoped L0xx diagnostics with paths and exact position/byte ranges."
    )]
    async fn m1_lint(
        &self,
        Parameters(p): Parameters<AnalyzeParams>,
    ) -> Result<Json<analyze::LintOutcome>, ErrorData> {
        let input = make_input(p.source, p.path)?;
        analyze::lint(&input)
            .map(Json)
            .map_err(|e| ErrorData::invalid_params(e, None))
    }

    /// Format M1 script, returning the formatted text (or just whether it is
    /// already formatted, in `check_only` mode).
    #[tool(
        description = "Format M1 script (inline `source` or a file `path`) to the M1 style, returning the formatted text and any warnings. With `check_only: true`, only reports whether the source is already formatted."
    )]
    async fn m1_format(
        &self,
        Parameters(p): Parameters<FormatParams>,
    ) -> Result<Json<analyze::FormatOutcome>, ErrorData> {
        let input = make_input(p.source, p.path)?;
        analyze::format(&input, p.check_only)
            .map(Json)
            .map_err(|e| ErrorData::invalid_params(e, None))
    }

    /// List a project's workspace symbols.
    #[tool(
        description = "Load a Project.m1prj and list its workspace symbols (channels, parameters, constants, functions, tables, objects) with kind, value type, unit, security, and a report naming every loaded or skipped auxiliary input. Optional case-insensitive path `filter`."
    )]
    async fn m1_symbols(
        &self,
        Parameters(p): Parameters<SymbolsParams>,
    ) -> Result<Json<symbols::SymbolsOutcome>, ErrorData> {
        symbols::list(
            &PathBuf::from(&p.project),
            p.filter.as_deref(),
            p.limit.unwrap_or(200),
        )
        .map(Json)
        .map_err(|e| ErrorData::invalid_params(e, None))
    }

    /// Report the project's CAN model: DBC modules, the bus each is `Init`-ed
    /// on, and whether repeated CAN ids actually clash.
    #[tool(
        description = "Inspect a project's CAN setup. Returns every loaded `.m1dbc` module with the CAN bus a script binds it to (`DBC.<Name>.Init(<bus>)` — a DBC has NO bus until then, M1 Build Error 1375), every message with its CAN id and bus, and each repeated CAN id judged `same-bus` (a real clash), `different-bus` (proven safe — same id on separate buses is not a conflict) or `unknown` (bus is a parameter/expression, so nothing is proven). The load report names malformed DBCs and unreadable scripts so an omitted input cannot look clean. Use this for any CAN id / bus question instead of reading the .m1dbc files."
    )]
    async fn m1_can(
        &self,
        Parameters(p): Parameters<CanParams>,
    ) -> Result<Json<can::CanOutcome>, ErrorData> {
        can::inspect(
            &PathBuf::from(&p.project),
            p.filter.as_deref(),
            p.limit.unwrap_or(200),
        )
        .map(Json)
        .map_err(|e| ErrorData::invalid_params(e, None))
    }
}

#[tool_handler]
impl ServerHandler for M1Server {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo (InitializeResult) is #[non_exhaustive]; build from default
        // and set the fields we care about.
        let mut info = ServerInfo::default();
        info.instructions = Some(INSTRUCTIONS.to_string());
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info.name = "m1-mcp".to_string();
        info.server_info.version = env!("CARGO_PKG_VERSION").to_string();
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_firmware_is_a_structured_invalid_params_error() {
        let requested = "m1-build-no-such-target";
        let error = validate_firmware(Some(requested)).expect_err("unknown target must fail");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(error.message.contains("known targets"));

        let data = error.data.expect("target choices are machine-readable");
        assert_eq!(data["requested"], requested);
        assert_eq!(
            data["known_targets"],
            serde_json::json!(m1_typecheck::intrinsics::known_targets())
        );
    }

    #[test]
    fn active_firmware_is_accepted() {
        let active = m1_typecheck::intrinsics::active_target();
        assert_eq!(validate_firmware(None).unwrap(), active);
        assert_eq!(validate_firmware(Some(active)).unwrap(), active);
    }
}
