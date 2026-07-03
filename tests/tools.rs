//! Integration tests for the m1-mcp tool implementations. These drive the
//! in-process analyser functions directly (the same functions the MCP tools
//! call), asserting on the serializable DTOs.

use m1_mcp::analyze::{self, Input};
use m1_mcp::doc::{self, DocKind};

// ---- doc reference --------------------------------------------------------

#[test]
fn doc_search_finds_a_known_library_function() {
    // `Absolute` is a real Calculate library function in the M1 catalogue.
    let hits = doc::search("Absolute", 20);
    assert!(
        hits.iter()
            .any(|e| e.kind == DocKind::Function && e.name == "Calculate.Absolute"),
        "expected Calculate.Absolute, got: {:?}",
        hits.iter().map(|e| &e.name).collect::<Vec<_>>()
    );
    // A function hit renders a signature with a return type.
    let f = hits
        .iter()
        .find(|e| e.name == "Calculate.Absolute")
        .unwrap();
    let sig = f.signature.as_ref().expect("function has a signature");
    assert!(
        sig.contains("->"),
        "signature should show a return type: {sig}"
    );
}

#[test]
fn doc_search_is_case_insensitive_and_capped() {
    let hits = doc::search("absolute", 1);
    assert!(!hits.is_empty(), "case-insensitive search should match");
    assert!(hits.len() <= 1, "limit must cap the result count");
}

#[test]
fn doc_search_empty_for_nonsense() {
    let hits = doc::search("zzz_no_such_intrinsic_qqq", 20);
    assert!(
        hits.is_empty(),
        "unknown term yields no matches, not an error"
    );
}

#[test]
fn doc_lookup_expands_enum_members() {
    // Find some enum in the catalogue, then look it up and confirm members expand.
    let some_enum = doc::search("Enumeration", 50)
        .into_iter()
        .find(|e| e.kind == DocKind::Enum);
    if let Some(en) = some_enum {
        let detail = doc::lookup(&en.name);
        let members = detail
            .iter()
            .filter(|e| e.kind == DocKind::EnumMember)
            .count();
        assert!(
            members > 0,
            "looking up enum {} should expand its members",
            en.name
        );
    }
}

// ---- analysers ------------------------------------------------------------

/// A tiny well-formed M1 script body used across the analyser tests.
const GOOD: &str = "Engine Speed Warning is True\n";

#[test]
fn typecheck_reports_no_error_count_for_reasonable_source() {
    let out = analyze::typecheck(&Input::Inline(GOOD.to_string()), None).expect("typecheck runs");
    // We don't assert zero diagnostics (standalone snippets can warn), but the
    // structured counts must be consistent with the diagnostics list.
    let errors = out
        .diagnostics
        .iter()
        .filter(|d| d.severity == "error")
        .count();
    assert_eq!(out.error_count, errors);
    assert!(!out.project_loaded, "no project was supplied");
}

#[test]
fn typecheck_flags_a_syntax_error() {
    // Unterminated construct → the parser should emit a syntax diagnostic.
    let out = analyze::typecheck(&Input::Inline("if (".to_string()), None).expect("runs");
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.code == "syntax" || d.severity == "error"),
        "broken source should produce an error diagnostic: {:?}",
        out.diagnostics
    );
}

#[test]
fn lint_returns_consistent_counts() {
    let out = analyze::lint(&Input::Inline(GOOD.to_string())).expect("lint runs");
    let warns = out
        .diagnostics
        .iter()
        .filter(|d| d.severity == "warning")
        .count();
    assert_eq!(out.warning_count, warns);
}

#[test]
fn format_check_only_omits_output() {
    let out = analyze::format(&Input::Inline(GOOD.to_string()), true).expect("format runs");
    assert!(out.formatted.is_none(), "check_only must not return text");
}

#[test]
fn format_returns_text() {
    let out = analyze::format(&Input::Inline(GOOD.to_string()), false).expect("format runs");
    assert!(
        out.formatted.is_some(),
        "non-check mode returns formatted text"
    );
}

#[test]
fn analyze_errors_on_missing_input() {
    // A non-existent path is an error, not a panic.
    let err = analyze::lint(&Input::Path("/no/such/file.m1scr".into()));
    assert!(err.is_err(), "unreadable path should error cleanly");
}
