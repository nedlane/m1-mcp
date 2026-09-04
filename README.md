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
| `m1_typecheck` | Type-check M1 source (inline text or a file path), returning diagnostics with code, severity and line/column. Pass a `Project.m1prj` to enable cross-script and reference-keyword checks and receive a project-load report. |
| `m1_check_project` | Run typecheck, lint and check-only format across every project script in one bounded request, with per-file results, separate project findings and aggregate totals. |
| `m1_completeness` | Report how much of a project's source was analysed, typed and resolved, including opaque references, unmodelled intrinsics, skipped scripts and input-load state. Optional `filter` narrows the reported filenames. |
| `m1_lint` | Lint M1 source with the project-configured `L0xx` rules. Findings include stable rule names and fixability. Optional fix mode returns verified fixed text without writing files. |
| `m1_lint_rule` | Look up one exact `L0xx` code and return its severity, default state, fixability, summary, and full explanation. |
| `m1_format` | Format M1 source to the M1 style, or (in `check_only` mode) just report whether it is already formatted. |
| `m1_symbols` | Load a `Project.m1prj` and list its workspace symbols — channels, parameters, constants, functions, tables, objects — with kind, value type, unit, security and a project-load report. |
| `m1_can` | Inspect a project's CAN setup: every `.m1dbc` module with the bus a script binds it to, every message with its CAN id, and each repeated id judged `same-bus`, `different-bus` or `unknown`. The response also reports project inputs that could not be loaded. |

The `m1_can` handler delegates in-process to the versioned
[`m1-can`](https://github.com/nedlane/m1-can) library. The CLI, MCP server, and
other consumers therefore share one bus-binding and overlap implementation.

For an unsaved buffer, pass its text as `source` and its intended `.m1scr` path
as `context_path`. The typechecker uses that filename to find the script's
project group and backing function. Lint and format use its parent directories
for config discovery. The server never reads source from or writes to
`context_path`; the request's `source` remains authoritative. `context_path` is
invalid with a file `path`, which already supplies both content and context.

### Checking CAN

A `.m1dbc` carries no CAN bus of its own. A script binds it with
`DBC.<Name>.Init(<bus>)` — conventionally all in one `CAN Init` script — and a
DBC that is used but never initialised is M1 Build **Error 1375** (the
`m1-typecheck` **T107** rule). CAN identifiers are therefore *per bus*: the same
id on two different buses is not a conflict.

That is easy to get wrong by reading the `.m1dbc` files alone, which is what
`m1_can` exists for. It reports each module's bus binding (with the `Init` call
site) and classifies every repeated CAN id:

- **`same-bus`** — a real clash: two messages with that id on one bus.
- **`different-bus`** — proven safe. The real EV corpus relies on this:
  `SBG DBC.Init(2)` and `DTI FSIC RL.Init(1)` both declare ids 133/173.
- **`unknown`** — at least one module is uninitialised, or its bus is a symbol
  the project carries no value for, so nothing is proven either way. Report it
  as unknown; do not guess.

A bus argument that names a symbol is **resolved to a number** where the project
knows one — a constant's `.m1prj` `Value`, or a parameter's cell in
`parameters.m1cfg` — and that number is reported as `bus_value`. So AV-M1's
`DBC.Dash.Init(Active Bus)` reads as bus 0 and `DBC.Datalogger.Init(Datalogger
Bus)` as bus 2, rather than two opaque names.

A value that came from the `.m1cfg` is the project's **current calibration**, so
a verdict resting on one is marked `depends_on_calibration: true`: it holds for
the loaded calibration and a retune can change it. Verdicts from literals and
project constants alone are retune-proof. Two modules bound to the *same* symbol
are the same bus either way.

### Project-load reports

Every project-backed result includes `load_report`. It names the main project,
the discovered parameter configuration, each successfully loaded `.m1dbc`, and
the number of readable scripts. Missing configuration and a project with no DBC
files are separate explicit states.

A malformed main `Project.m1prj` remains a tool error. A malformed auxiliary
DBC or unreadable script does not discard the usable model: the response keeps
the partial result and lists every skipped path with its error. Treat a result
with skipped inputs as partial, even when its diagnostics or CAN overlaps are
otherwise empty.

### Analysis completeness and firmware target

Zero diagnostics does not prove that every expression or reference was
understood. `m1_completeness` reports the silent surface directly: scripts
analysed or skipped for syntax/depth, typed expressions, resolved/opaque/
unresolved references, intrinsic calls missing from the catalogue, and
incomplete `when` subjects. The percentages are telemetry, not a pass/fail
gate. Its `load_report` identifies any project inputs omitted from the model.

Doc-search, lookup, typecheck and completeness results include
`catalogue_target`, naming the firmware/manual capture behind builtin signatures
and enums. Those tools accept an optional `firmware` assertion. An unavailable
target is rejected before analysis with an invalid-parameters error whose data
lists `known_targets`; the server never silently substitutes another catalogue.

The doc tools cover the toolchain's own **intrinsics catalogue** (the builtins
M1 Build itself resolves against). They do **not** redistribute the proprietary
MoTeC M1 Development Manual.

### Lint fixes and rule details

Path input discovers the same `m1-tools.toml` and `.m1lint.toml` settings as
the CLI. A path matching the configured `exclude` globs is not read, parsed or
fixed; the result has `excluded: true`, empty diagnostics and, when a fix was
requested, an `unchanged` fix outcome. Inline source uses the default rule set.

`m1_lint` accepts `fix: true`. It runs the pinned linter's safe fixed-point
fixer with the same project configuration used to produce the findings. The
diagnostics describe the submitted source, and the separate `fix` result has
one of three explicit outcomes:

- `unchanged`: no enabled fix changed valid source, or syntax errors prevented
  the fixer from running.
- `fixed`: `source` contains the verified fixed text.
- `unsafe`: `error` explains why the linter rejected the rewrite.

A path remains read-only in every case. The server returns text and never
writes it back. Each lint finding includes `name` and `fixable`; parser
diagnostics use `name: "syntax-error"` and `fixable: false`. Both kinds retain
the submitted source identity and exact position and byte ranges.

Call `m1_lint_rule` with an exact uppercase code such as `L004` to get the
stable rule name, default severity, whether it is enabled by default, whether
it is fixable, its one-line summary, and the linter's full explanation.

## Scope and limits

`m1-mcp` is an **analysis and format bridge** — it lets an agent look up M1
semantics and run the read-only analysers (`m1_doc_search`, `m1_doc_lookup`,
`m1_typecheck`, `m1_check_project`, `m1_completeness`, `m1_lint`,
`m1_lint_rule`, `m1_format`, `m1_symbols`, `m1_can`) over the code it writes.
It is **not** full toolchain parity: it does not evaluate M1 scripts, does not
mutate projects or write files back, and does not generate docs or
visualisations. Use the individual CLIs (or the LSP/editor integrations) for
those.

Because the server speaks MCP over **stdio** on a single process, every request
is bounded so no one call can monopolise it:

- **Inline `source` and file reads** are capped at **2 MiB** per request; a
  larger payload (or a `path` to a larger file, rejected on its size before it
  is read) returns a structured error naming the limit.
- **Project-wide operations** (`m1_typecheck` given a `project`,
  `m1_check_project`, `m1_completeness`, `m1_symbols` and `m1_can`) walk at most
  **2000** `.m1scr` files; a larger project tree is rejected before it is
  loaded.
- **Whole-project diagnostic responses** return at most **5000** diagnostic or
  formatting-warning records. `m1_check_project` also defaults to 100 records
  per file and reports whenever either bound truncates its response.

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

## Dependency updates

The M1 libraries are pinned to release tags. Dependabot groups compatible
updates to the other M1 crates, but it does not update `m1-typecheck` or
`m1-can`. Its Cargo updater evaluates git-tag candidates one at a time before
building a group, while these two releases are coupled: a new `m1-can` tag may
require the matching `m1-typecheck` tag. During a release cascade, that has made
Dependabot select an older tag and fail dependency resolution instead of
opening a pull request.

Update the `m1-typecheck` and `m1-can` tags together in `Cargo.toml` after both
releases exist, refresh `Cargo.lock`, and run the build and test commands above.
Dependabot remains a backstop for compatible updates to the rest of the M1
toolchain.

`m1-mcp` is part of the [M1 toolchain](https://github.com/C-Nucifora/m1-tools)
and depends on its sibling crates via versioned git tags.
