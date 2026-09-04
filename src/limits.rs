//! Per-request workload limits.
//!
//! `m1-mcp` is a single-process **stdio** server: every tool call runs on the
//! one server task, so an oversized request — a giant inline `source`, a huge
//! file behind a `path`, or a project tree with an unreasonable number of
//! scripts — would monopolise the server and could exhaust memory. These
//! constants bound the work any one request may trigger; an over-limit request
//! fails fast with a structured error naming the limit rather than being
//! serviced.
//!
//! They are deliberately plain constants, not configuration: the server is an
//! analysis/format bridge for an agent's own edits, and these ceilings sit far
//! above any legitimate interactive payload, so there is nothing for a user to
//! tune.

/// Maximum size, in bytes, of the M1 source analysed in a single request —
/// both inline `source` and the contents read from a `path`. Scripts in the
/// reference corpora are tens of KB; 2 MiB is generous headroom while keeping
/// one request from allocating unboundedly on the single server task.
///
/// (m1-workspace caps *any* MoTeC file read at 64 MiB, sized for whole-project
/// XML; an interactive analysis payload is held to this tighter per-request
/// bound.)
pub const MAX_REQUEST_SOURCE_BYTES: u64 = 2 * 1024 * 1024;

/// Maximum number of `.m1scr` files a single project-wide operation — an
/// `m1_typecheck` given a `project`, or `m1_symbols` — will walk. The reference
/// corpora carry a few hundred scripts; 2000 is comfortable headroom while
/// bounding the parse work one request can trigger against a pathological or
/// unexpectedly large project tree.
pub const MAX_PROJECT_SCRIPTS: usize = 2000;

/// Hard ceiling on diagnostic and formatting-warning records returned by a
/// whole-project check, regardless of its requested per-file limit.
pub const MAX_PROJECT_RESPONSE_DIAGNOSTICS: usize = 5000;
