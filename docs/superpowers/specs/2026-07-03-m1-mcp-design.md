# m1-mcp — design

## Problem

AI coding agents (Claude Code, Cursor, the M1 editors' agent modes) that edit
MoTeC **M1 scripts** (`.m1scr`) have no live access to the M1 toolchain or to
the M1 language reference. They get only *static* help: per-repo `AGENTS.md`
files and the M1 Development Manual PDF they must grep by hand. So an agent
writing M1 code guesses at builtin signatures, enum spellings, and type rules
instead of consulting ground truth, and cannot check its own edits.

## Goal

A **Model Context Protocol (MCP) server** that exposes the M1 toolchain to any
MCP-capable agent as live tools, so an agent can *look up* M1 language
semantics and *run* the analysers on the code it writes — without shelling out
to, or bundling, the whole toolchain.

## Non-goals

- Not a language server (that is `m1-lsp`, for editors). MCP tools are
  one-shot request/response, not a stateful `textDocument/*` session.
- Does **not** redistribute the proprietary MoTeC M1 Development Manual. Doc
  search covers the toolchain's own **intrinsics catalogue** (the builtin
  functions/enums/classes/types that `m1-typecheck` already ships publicly in
  `assets/m1-intrinsics.json`), never the copyrighted PDF text.
- No write/edit tools in v1 (formatting returns text; it does not write files).

## Architecture

A single self-contained Rust binary, `m1-mcp`, built on the official Rust MCP
SDK (`rmcp` 2.1, stdio transport). It depends on the M1 toolchain **library
crates** via versioned git tags — exactly like every other repo in the stack
(`m1-doc` consumes `m1-typecheck` the same way) — and calls them **in-process**.
No PATH dependency on installed CLIs; the agent points at one binary.

```
agent (Claude Code / Cursor)  ──stdio/JSON-RPC──▶  m1-mcp
                                                     ├─ m1_typecheck  (lib: intrinsics, Project, rules::check_script)
                                                     ├─ m1_lint       (lib: Runner + Registry)
                                                     └─ m1_fmt        (lib: format_str)
```

### Tools (the full v1 set)

| Tool | Backed by | Purpose |
| --- | --- | --- |
| `m1_doc_search` | `m1_typecheck::intrinsics` | Fuzzy/substring search across builtin functions, enums, classes, types → ranked matches with kind, signature, doc snippet. **The headline: agents cite real M1 semantics instead of guessing.** |
| `m1_doc_lookup` | `m1_typecheck::intrinsics` | Exact lookup of one builtin name → full detail (all overloads, params + types, enum members, class doc). |
| `m1_typecheck` | `m1_typecheck::rules::check_script` | Type-check M1 source (inline text or file path; optional `Project.m1prj` root for cross-references) → diagnostics (code, severity, line/col, message). |
| `m1_lint` | `m1_lint::runner::Runner` | Lint M1 source with the default rule set → L0xx diagnostics. |
| `m1_format` | `m1_fmt::format_str` | Format M1 source → formatted text + any warnings; `check_only` mode reports whether it is already formatted. |
| `m1_symbols` | `m1_typecheck::Project` | Load a project and list its workspace symbols (path, kind, type, unit, security), optional name substring filter. |

All tool results are returned as **structured JSON** (`rmcp::Json<T>`) so agents
get machine-readable diagnostics, not prose to re-parse. Project-backed tools
also return a shared load report naming the configuration, loaded DBCs, readable
script count, and every skipped auxiliary input.

### Data flow

Each tool call: deserialize typed params → build the relevant M1 lib input
(read a file tolerantly via `m1_workspace::read_text` when a path is given, or
use inline `source`) → call the lib → map the lib's result type to a small
serializable DTO → return `Json`. Stateless; no caching in v1.

MCP-native project calls build an augmented model and parse-once script set
through `loader::load_project_full`. Its report distinguishes missing
configuration, no DBC files, complete DBC loading, and a partial load with
per-path errors. The MCP CAN wrapper preflights that loader before delegating to
`m1-can`, flattens the shared CAN outcome so its established fields stay stable,
then adds the preflight report.

### Error handling

- Bad input (neither `source` nor `path`, unreadable file, unparseable
  project) → a structured tool error, never a panic.
- An unreadable script or malformed auxiliary DBC produces a partial result
  with its path and error in `load_report`; it is never presented as a complete
  clean result.
- Syntax errors in M1 source are reported as diagnostics, not failures — the
  same contract the CLIs use.
- Doc lookups that miss return an empty result set, not an error.

### Testing

Integration tests drive each tool's inner function over known M1 snippets and
the intrinsics catalogue, asserting on the DTOs. A corpus smoke test runs
`m1_symbols` over EV-M1 when `$M1_CORPUS_PATH` is present (skips otherwise,
matching the rest of the stack).

## Repo wiring (house pattern)

`nedlane/m1-mcp`, GPL-3.0-or-later, mirrors the stack: `CI` (test/clippy/fmt/
audit/doc), `release.yml` (prebuilt Linux/macOS/Windows binaries, provenance +
SHA256SUMS), `dependabot.yml` (daily, `m1-*` group), `AGENTS.md`, `README.md`.
Added to `m1-tools/m1-tools.repos` via PR. No MSRV job: `m1-mcp` is a **leaf
binary** (nothing pins it as a dependency) with a large async runtime dep
(`rmcp`/`tokio`) whose transitive MSRV floats, so a pinned MSRV gate would be
pure friction with no consumer to protect.

## Local Claude integration

Registered in the user's Claude Code MCP config so `m1-mcp` is available to
every session as a stdio server (`claude mcp add m1 -- /path/to/m1-mcp`).
