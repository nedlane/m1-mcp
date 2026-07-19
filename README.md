# m1-mcp

A [Model Context Protocol](https://modelcontextprotocol.io) server that gives AI
agents live access to the MoTeC **M1 script** toolchain. An agent editing
`.m1scr` code can look up M1 language semantics and run the analysers on its own
edits — instead of guessing at builtin signatures and enum spellings, then
shipping code it never checked.

It wraps the M1 toolchain library crates (`m1-typecheck`, `m1-lint`, `m1-fmt`)
**in-process**, so it is a single self-contained binary with no dependency on
having the CLIs installed on your `PATH`.

## Tools

| Tool | What it does |
| --- | --- |
| `m1_doc_search` | Search the M1 builtin catalogue — library functions, project-object methods, firmware enumerations, package classes, data types — and get ranked matches with signatures and docs. |
| `m1_doc_lookup` | Full detail for one exact builtin name: every overload of a function, all members of an enum, a class summary, or a data type. |
| `m1_typecheck` | Type-check M1 source (inline text or a file path), returning diagnostics with code, severity and line/column. Pass a `Project.m1prj` to enable cross-script and reference-keyword checks. |
| `m1_lint` | Lint M1 source with the default M1 rule set (the `L0xx` rules). |
| `m1_format` | Format M1 source to the M1 style, or (in `check_only` mode) just report whether it is already formatted. |
| `m1_symbols` | Load a `Project.m1prj` and list its workspace symbols — channels, parameters, constants, functions, tables, objects — with kind, value type, unit and security. |

The doc tools cover the toolchain's own **intrinsics catalogue** (the builtins
M1 Build itself resolves against). They do **not** redistribute the proprietary
MoTeC M1 Development Manual.

## Scope and limits

`m1-mcp` is an **analysis and format bridge** — it lets an agent look up M1
semantics and run the read-only analysers (`m1_doc_search`, `m1_doc_lookup`,
`m1_typecheck`, `m1_lint`, `m1_format`, `m1_symbols`) over the code it writes.
It is **not** full toolchain parity: it does not evaluate M1 scripts, does not
mutate projects or write files back, and does not generate docs or
visualisations. Use the individual CLIs (or the LSP/editor integrations) for
those.

Because the server speaks MCP over **stdio** on a single process, every request
is bounded so no one call can monopolise it:

- **Inline `source` and file reads** are capped at **2 MiB** per request; a
  larger payload (or a `path` to a larger file, rejected on its size before it
  is read) returns a structured error naming the limit.
- **Project-wide operations** (`m1_typecheck` given a `project`, and
  `m1_symbols`) walk at most **2000** `.m1scr` files; a larger project tree is
  rejected before it is loaded.

The limits are compile-time constants (see `src/limits.rs`), sized well above
any realistic interactive request — a project that hits them should be run
through the CLI directly.

## Install

Download the binary for your platform from the
[latest release](https://github.com/nedlane/m1-mcp/releases/latest), rename it to
`m1-mcp` (`m1-mcp.exe` on Windows), mark it executable, and put it on your
`PATH`. Or build from source:

```sh
cargo install --git https://github.com/nedlane/m1-mcp --tag v0.1.0
```

## Use it with Claude Code

Register it as a stdio MCP server:

```sh
claude mcp add m1 -- m1-mcp
```

Then, in any session, ask Claude to work on M1 scripts and it will use the tools
to check builtin signatures and run typecheck/lint/format on its edits. Any
MCP-capable client (Cursor, Zed, …) can point at the `m1-mcp` binary the same
way — it speaks MCP over stdio.

Set `RUST_LOG=debug` for verbose tracing on stderr (stdout carries the
protocol).

## Build and test

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

`m1-mcp` is part of the [M1 toolchain](https://github.com/C-Nucifora/m1-tools)
and depends on its sibling crates via versioned git tags.
