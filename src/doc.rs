//! Doc reference over the M1 **intrinsics catalogue** — the builtin library
//! functions, project-object methods, firmware enumerations, package classes
//! and data types that `m1-typecheck` ships in `assets/m1-intrinsics.json`.
//!
//! This is the catalogue M1 Build itself resolves against; surfacing it to an
//! agent lets it cite real M1 semantics (signatures, enum spellings, which
//! functions are calibration-only) instead of guessing. It is derived toolchain
//! data — it is **not** the proprietary MoTeC manual, which is never bundled.

use m1_typecheck::intrinsics::{self, Intrinsics, Overload};
use schemars::JsonSchema;
use serde::Serialize;

/// The kind of catalogue entry a match points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DocKind {
    /// A library function, e.g. `Calculate.Choose`.
    Function,
    /// A method callable on a project object, e.g. `.AsInteger()`.
    Method,
    /// A builtin firmware/module enumeration type.
    Enum,
    /// A single enumerator of a builtin enumeration.
    EnumMember,
    /// A package class (e.g. `MoTeC Input.Sensor`).
    Class,
    /// A builtin scalar data type name.
    Type,
}

/// The result of a doc search or lookup. Wraps the entry list in an object
/// because the MCP spec requires a tool's output schema root to be an object,
/// not a bare array.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DocResults {
    pub count: usize,
    pub matches: Vec<DocEntry>,
}

impl From<Vec<DocEntry>> for DocResults {
    fn from(matches: Vec<DocEntry>) -> Self {
        DocResults {
            count: matches.len(),
            matches,
        }
    }
}

/// One catalogue entry returned from a search or lookup.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DocEntry {
    /// Fully-qualified display name (`Calculate.Choose`, `Output Drive
    /// Enumeration.High Side`, `MoTeC Input.Sensor`).
    pub name: String,
    pub kind: DocKind,
    /// The owning object/enum/type, when the entry is a member of one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// A rendered signature for callables (`Choose(index: Number, ...) ->
    /// Number`), or the enumerator value / type note for non-callables.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Documentation text (may be empty).
    #[serde(skip_serializing_if = "str::is_empty")]
    pub doc: String,
    /// Calibration-only: usable only in M1 Tune calibration methods, never in
    /// ECU `.m1scr` scripts. Agents editing scripts must avoid these.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub calibration_only: bool,
    /// Stateful ("must call every execution") function.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stateful: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub deprecated: bool,
}

/// Render an overload as `Name(p1: T1, p2: T2) -> Returns`.
fn signature(qualified_name: &str, ov: &Overload) -> String {
    let params = ov
        .params
        .iter()
        .map(|p| format!("{}: {}", p.name, p.ty))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{qualified_name}({params}) -> {}", ov.returns)
}

/// Best match rank for `needle` against `haystack` (lower is better; `None`
/// means no match). 0 = exact (case-insensitive), 1 = prefix, 2 = substring.
fn name_rank(haystack: &str, needle: &str) -> Option<u8> {
    let (h, n) = (haystack.to_ascii_lowercase(), needle.to_ascii_lowercase());
    if h == n {
        Some(0)
    } else if h.starts_with(&n) {
        Some(1)
    } else if h.contains(&n) {
        Some(2)
    } else {
        None
    }
}

/// A scored entry used while ranking a search.
struct Scored {
    rank: u8,
    entry: DocEntry,
}

/// Walk the whole catalogue, yielding every entry paired with its owning names
/// so callers can match on either the leaf or the qualified name.
fn each_entry(intr: &'static Intrinsics, mut f: impl FnMut(&str, &str, DocEntry)) {
    // Library functions: object.function
    for (object, obj) in &intr.library {
        for ov in &obj.functions {
            let qualified = format!("{object}.{}", ov.name);
            let entry = DocEntry {
                name: qualified.clone(),
                kind: DocKind::Function,
                parent: Some(object.clone()),
                signature: Some(signature(&qualified, ov)),
                doc: ov.doc.clone(),
                calibration_only: ov.calibration_only,
                stateful: ov.stateful,
                deprecated: ov.deprecated,
            };
            f(&ov.name, &qualified, entry);
        }
    }
    // Project-object methods (AsInteger/Set/Lookup/…)
    for ov in &intr.object_methods {
        let qualified = format!(".{}", ov.name);
        let entry = DocEntry {
            name: ov.name.clone(),
            kind: DocKind::Method,
            parent: None,
            signature: Some(signature(&qualified, ov)),
            doc: ov.doc.clone(),
            calibration_only: ov.calibration_only,
            stateful: ov.stateful,
            deprecated: ov.deprecated,
        };
        f(&ov.name, &ov.name, entry);
    }
    // Builtin enumerations and their members
    for en in &intr.enums {
        let entry = DocEntry {
            name: en.name.clone(),
            kind: DocKind::Enum,
            parent: None,
            signature: Some(format!("{} members", en.members.len())),
            doc: String::new(),
            calibration_only: false,
            stateful: false,
            deprecated: false,
        };
        f(&en.name, &en.name, entry);
        for m in &en.members {
            let qualified = format!("{}.{}", en.name, m.name);
            let mut doc = m.doc.clone();
            if !m.severity.is_empty() {
                doc = if doc.is_empty() {
                    format!("indicator: {}", m.severity)
                } else {
                    format!("{doc} (indicator: {})", m.severity)
                };
            }
            let entry = DocEntry {
                name: qualified.clone(),
                kind: DocKind::EnumMember,
                parent: Some(en.name.clone()),
                signature: Some(format!("= {}", m.value)),
                doc,
                calibration_only: false,
                stateful: false,
                deprecated: false,
            };
            f(&m.name, &qualified, entry);
        }
    }
    // Package classes
    for (name, doc) in &intr.classes {
        let entry = DocEntry {
            name: name.clone(),
            kind: DocKind::Class,
            parent: None,
            signature: None,
            doc: doc.clone(),
            calibration_only: false,
            stateful: false,
            deprecated: false,
        };
        f(name, name, entry);
    }
    // Builtin data types
    for ty in &intr.data_types {
        let entry = DocEntry {
            name: ty.clone(),
            kind: DocKind::Type,
            parent: None,
            signature: None,
            doc: String::new(),
            calibration_only: false,
            stateful: false,
            deprecated: false,
        };
        f(ty, ty, entry);
    }
}

/// Search the catalogue for `query`, returning up to `limit` best matches.
///
/// Ranking: an entry matches if the query is found in its leaf name, its
/// qualified name, or (weakest) its documentation. Name matches outrank doc
/// matches; exact/prefix/substring ties break by name.
pub fn search(query: &str, limit: usize) -> Vec<DocEntry> {
    let intr = intrinsics::get();
    let mut scored: Vec<Scored> = Vec::new();
    each_entry(intr, |leaf, qualified, entry| {
        // Rank by the strongest of leaf-name / qualified-name / doc match.
        let name_hit = name_rank(leaf, query)
            .into_iter()
            .chain(name_rank(qualified, query))
            .min();
        let rank = match name_hit {
            Some(r) => r,
            None if entry
                .doc
                .to_ascii_lowercase()
                .contains(&query.to_ascii_lowercase()) =>
            {
                3
            }
            None => return,
        };
        scored.push(Scored { rank, entry });
    });
    scored.sort_by(|a, b| {
        a.rank
            .cmp(&b.rank)
            .then_with(|| a.entry.name.cmp(&b.entry.name))
    });
    scored.truncate(limit);
    scored.into_iter().map(|s| s.entry).collect()
}

/// Full detail for an exact catalogue name. Accepts a library function
/// (`Calculate.Choose` or bare `Choose`), an object method, an enum type
/// (all members expanded), an enum member, a class, or a data type. Returns
/// every entry whose leaf or qualified name equals `name` (case-insensitive),
/// so an overloaded function yields all its signatures.
pub fn lookup(name: &str) -> Vec<DocEntry> {
    let intr = intrinsics::get();
    let want = name.to_ascii_lowercase();
    let mut out = Vec::new();

    // Exact enum-type lookup expands to the type entry plus every member.
    if let Some(en) = intr
        .enums
        .iter()
        .find(|e| e.name.eq_ignore_ascii_case(name))
    {
        each_entry(intr, |_, _, entry| {
            if entry.parent.as_deref() == Some(en.name.as_str()) || entry.name == en.name {
                out.push(entry);
            }
        });
        return out;
    }

    each_entry(intr, |leaf, qualified, entry| {
        if leaf.to_ascii_lowercase() == want || qualified.to_ascii_lowercase() == want {
            out.push(entry);
        }
    });
    out
}
