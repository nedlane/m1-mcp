# AGENTS.md — m1-mcp

A Model Context Protocol server that exposes the MoTeC M1 script toolchain to AI
agents as live tools.

## Why this exists

Agents that edit `.m1scr` scripts had only static help (per-repo docs, the
manual PDF). They guessed at builtin signatures and enum spellings and could not
check their own edits. `m1-mcp` closes that gap: it makes the toolchain callable
over MCP so an agent can look up M1 semantics and run typecheck/lint/format on
the code it writes.

## Architecture

A single binary on the official Rust MCP SDK (`rmcp`, stdio transport). It
depends on the toolchain **library crates** (`m1-typecheck`, `m1-lint`,
`m1-fmt`, `m1-can`) via versioned git tags and calls them **in-process** — there is no
PATH dependency on installed CLIs.

- `doc` — reference over `m1-typecheck`'s intrinsics catalogue (`m1_doc_search`,
  `m1_doc_lookup`).
- `analyze` — runs the analysers over agent-supplied source (`m1_typecheck`,
  `m1_lint`, `m1_format`).
- `symbols` — lists a project's workspace symbols (`m1_symbols`).
- `m1-can` — supplies the shared CAN picture behind `m1_can`: which `.m1dbc`
  module a script binds to which bus via `DBC.<Name>.Init(<bus>)`, and whether a
  repeated CAN id is a real clash. Keep that logic in the shared crate.
- `server` — the `rmcp` tool router wiring the above; each tool returns
  structured JSON.

## Constraints

- **Never bundle the MoTeC manual.** Doc tools cover only the intrinsics
  catalogue that `m1-typecheck` already ships publicly — derived toolchain data,
  not the proprietary PDF.
- **Library crates via git tags only** — never a path or `[patch]` override, so
  local builds match external consumers.
- **stdout is the protocol channel.** All logging goes to stderr.
- Tool output schemas must have an **object** root (MCP requirement) — wrap list
  results in a struct, never return a bare array.
- Bad input is a structured tool error, never a panic; syntax errors in M1
  source are reported as diagnostics, not failures.
- **CAN answers are per bus, and never guessed.** A `.m1dbc` has no bus until a
  script calls `DBC.<Name>.Init(<bus>)`, so `m1_can` only calls a repeated CAN id
  a clash when both modules are provably on the same bus, only calls it safe when
  their buses provably differ, and otherwise says `unknown`. Keep that three-way
  honesty — an agent acting on a false "conflict" edits working vehicle code.
- **A calibration value is not a constant.** A bus resolved from
  `parameters.m1cfg` is the current calibration; one from a `.m1prj` constant is
  fixed. `m1_can` keeps them apart (`depends_on_calibration`) instead of
  flattening both into "the bus number" — a retune must not silently invalidate
  an answer an agent was given as proof.
- No MSRV gate: this is a leaf binary (nothing pins it) with a large async dep
  whose transitive MSRV floats.

## Build / test gate

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

Integration tests (`tests/tools.rs`) drive the analyser functions directly over
known M1 snippets and the intrinsics catalogue.

## Releases

`release.yml` publishes prebuilt Linux/macOS/Windows binaries (with provenance +
`SHA256SUMS`) whenever the `Cargo.toml` version changes. When an upstream
toolchain crate cuts a new release, bump its `tag` here and open the PR
promptly — do not wait for Dependabot.
