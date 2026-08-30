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
use crate::{doc, symbols};

/// Guidance surfaced to the agent on connect.
const INSTRUCTIONS: &str = "\
Live access to the MoTeC M1 script toolchain. Use `m1_doc_search`/`m1_doc_lookup` to \
confirm builtin function signatures, enum spellings, and which functions are \
calibration-only BEFORE writing M1 script — do not guess. After editing a `.m1scr`, \
run `m1_typecheck`, `m1_lint`, and `m1_format` (check_only) on it. Pass either inline \
`source` or a file `path`; give `project` (a Project.m1prj) to enable cross-script and \
reference-keyword checks. `m1_lint` can return a safe fixed source without writing the file; \
use `m1_lint_rule` for the rationale and fix behavior of an exact L-code. `m1_symbols` lists a \
project's channels/parameters/functions.
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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DocSearchParams {
    /// Text to search for across builtin function names, enum members, class
    /// names, data types, and their documentation.
    pub query: String,
    /// Maximum number of matches to return (default 20).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DocLookupParams {
    /// Exact name to look up: a library function (`Calculate.Choose` or bare
    /// `Choose`), an object method, an enum type (members expanded), an enum
    /// member, a class, or a data type.
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LintParams {
    /// Inline M1 script text. Provide this OR `path`.
    #[serde(default)]
    pub source: Option<String>,
    /// Path to a `.m1scr` file. Provide this OR `source`.
    #[serde(default)]
    pub path: Option<String>,
    /// Return a verified safe fixed source without writing the input file.
    #[serde(default)]
    pub fix: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LintRuleParams {
    /// Exact uppercase lint rule code, such as `L004`.
    pub code: String,
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
        description = "Search the M1 builtin catalogue (library functions, project-object methods, firmware enums, package classes, data types) for a term. Returns ranked matches with signature and docs."
    )]
    async fn m1_doc_search(
        &self,
        Parameters(p): Parameters<DocSearchParams>,
    ) -> Json<doc::DocResults> {
        Json(doc::search(&p.query, p.limit.unwrap_or(20)).into())
    }

    /// Look up full detail for one exact M1 builtin name (all overloads, params,
    /// enum members, or class doc).
    #[tool(
        description = "Look up full detail for one exact M1 builtin name: a library function (all overloads), an object method, an enum type (members expanded), an enum member, a class, or a data type."
    )]
    async fn m1_doc_lookup(
        &self,
        Parameters(p): Parameters<DocLookupParams>,
    ) -> Json<doc::DocResults> {
        Json(doc::lookup(&p.name).into())
    }

    /// Type-check M1 script, returning diagnostics.
    #[tool(
        description = "Type-check M1 script (inline `source` or a file `path`), returning source/project-scoped diagnostics with paths, exact position/byte ranges, project subjects, and related declaration locations. Pass `project` (a Project.m1prj) to enable cross-script and reference-keyword checks."
    )]
    async fn m1_typecheck(
        &self,
        Parameters(p): Parameters<TypecheckParams>,
    ) -> Result<Json<analyze::TypecheckOutcome>, ErrorData> {
        let input = make_input(p.source, p.path)?;
        let project = p.project.map(PathBuf::from);
        analyze::typecheck(&input, project.as_deref())
            .map(Json)
            .map_err(|e| ErrorData::invalid_params(e, None))
    }

    /// Lint M1 script with the resolved rule set, optionally returning a safe
    /// fixed source.
    #[tool(
        description = "Lint M1 script (inline `source` or a read-only file `path`) with the project-configured rule set. Findings include source paths, exact position/byte ranges, stable rule names, and fixability. Set `fix: true` to return a verified safe fixed source; files are never written and syntax errors are never fixed.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn m1_lint(
        &self,
        Parameters(p): Parameters<LintParams>,
    ) -> Result<Json<analyze::LintOutcome>, ErrorData> {
        let input = make_input(p.source, p.path)?;
        analyze::lint(&input, p.fix)
            .map(Json)
            .map_err(|e| ErrorData::invalid_params(e, None))
    }

    /// Look up metadata and the full explanation for one exact lint rule code.
    #[tool(
        description = "Look up one exact M1 lint L-code. Returns its stable name, severity, default state, fixability, summary, and full explanation.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn m1_lint_rule(
        &self,
        Parameters(p): Parameters<LintRuleParams>,
    ) -> Result<Json<analyze::LintRuleMetadata>, ErrorData> {
        analyze::lint_rule(&p.code)
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
        description = "Load a Project.m1prj and list its workspace symbols (channels, parameters, constants, functions, tables, objects) with kind, value type, unit, and security. Optional case-insensitive path `filter`."
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
        description = "Inspect a project's CAN setup. Returns every `.m1dbc` module with the CAN bus a script binds it to (`DBC.<Name>.Init(<bus>)` — a DBC has NO bus until then, M1 Build Error 1375), every message with its CAN id and bus, and each repeated CAN id judged `same-bus` (a real clash), `different-bus` (proven safe — same id on separate buses is not a conflict) or `unknown` (bus is a parameter/expression, so nothing is proven). Use this for any CAN id / bus question instead of reading the .m1dbc files."
    )]
    async fn m1_can(
        &self,
        Parameters(p): Parameters<CanParams>,
    ) -> Result<Json<m1_can::CanOutcome>, ErrorData> {
        m1_can::inspect(
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
    use super::M1Server;

    #[test]
    fn lint_tools_publish_read_only_object_schemas() {
        let tools = M1Server::tool_router().list_all();
        for name in ["m1_lint", "m1_lint_rule"] {
            let tool = tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("missing {name} tool"));
            assert_eq!(
                tool.input_schema.get("type"),
                Some(&serde_json::json!("object"))
            );
            let output = tool.output_schema.as_ref().expect("output schema");
            assert_eq!(output.get("type"), Some(&serde_json::json!("object")));
            let annotations = tool.annotations.as_ref().expect("tool annotations");
            assert_eq!(annotations.read_only_hint, Some(true));
            assert_eq!(annotations.destructive_hint, Some(false));
            let expected_input = if name == "m1_lint" { "fix" } else { "code" };
            assert!(
                tool.input_schema["properties"]
                    .as_object()
                    .is_some_and(|properties| properties.contains_key(expected_input)),
                "{name} input schema missing {expected_input}"
            );
        }
    }
}
