//! Integration tests for the m1-mcp tool implementations. These drive the
//! in-process analyser functions directly (the same functions the MCP tools
//! call), asserting on the serializable DTOs.

use m1_mcp::analyze::{self, Input};
use m1_mcp::doc::{self, DocKind};
use m1_mcp::{can, limits, loader};

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
fn typecheck_catches_cfg_typed_intrinsic_overload_mismatch() {
    // EV-M1 regression: Calculate.Max has homogeneous Integer/Integer and
    // Floating Point/Floating Point overloads. A u32 calibration parameter
    // mixed with 1.0 must surface T065 through the MCP tool, not pass clean and
    // wait for M1 Build Error 1302.
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("UQR-EV").join("01.00");
    let scripts_dir = project_dir.join("Scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    std::fs::write(
        project_dir.join("Project.m1prj"),
        r#"<?xml version="1.0"?>
<MoTeCM1BuildSession>
 <Project Name="EV" TargetHardware="ecu150">
  <ComponentStream>
   <List>
    <Component Classname="BuiltIn.GroupCompound" Name="Root.CAN"/>
    <Component Classname="BuiltIn.GroupCompound" Name="Root.CAN.DTI FSIC Rear"/>
    <Component Classname="BuiltIn.Parameter" Name="Root.CAN.DTI FSIC Rear.Pole Pairs">
     <Props Security="Calibration"/>
    </Component>
    <Component Classname="BuiltIn.FuncUser"
      Filename="CAN.Inverters Transcieve 200hz.m1scr"
      Name="Root.CAN.Inverters Transcieve 200hz"/>
   </List>
  </ComponentStream>
 </Project>
</MoTeCM1BuildSession>
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("parameters.m1cfg"),
        r#"<?xml version="1.0"?>
<Configuration>
 <Group Name="">
  <Parameter Name="CAN.DTI FSIC Rear.Pole Pairs">
   <Cell Type="u32"><![CDATA[10]]></Cell>
  </Parameter>
 </Group>
</Configuration>
"#,
    )
    .unwrap();
    let script = scripts_dir.join("CAN.Inverters Transcieve 200hz.m1scr");
    std::fs::write(
        &script,
        "local fRearPolePairs = Calculate.Max(1.0, DTI FSIC Rear.Pole Pairs);\n",
    )
    .unwrap();

    let out = analyze::typecheck(
        &Input::Path(script),
        Some(&project_dir.join("Project.m1prj")),
    )
    .expect("typecheck runs");
    let hit = out
        .diagnostics
        .iter()
        .find(|d| d.code == "T065")
        .expect("MCP must surface the intrinsic overload mismatch");
    assert_eq!(hit.severity, "error");
    assert!(
        hit.message.contains("Floating Point, Unsigned Integer"),
        "unexpected T065 message: {}",
        hit.message
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

// ---- per-request workload limits (issue #11) ------------------------------

#[test]
fn typecheck_rejects_oversized_inline_source() {
    // One byte over the inline-source cap must be refused with a clear message
    // naming the limit — not parsed.
    let big = "A".repeat(limits::MAX_REQUEST_SOURCE_BYTES as usize + 1);
    let err = analyze::typecheck(&Input::Inline(big), None)
        .expect_err("oversize inline source must be rejected");
    assert!(
        err.contains("exceeds") && err.contains("per-request limit"),
        "error should name the limit: {err}"
    );
}

#[test]
fn lint_rejects_oversized_file() {
    // A file over the cap is rejected on its size (checked before reading),
    // across every source-consuming tool (lint shares the same input path).
    let dir = tempfile::tempdir().unwrap();
    let scr = dir.path().join("big.m1scr");
    std::fs::write(
        &scr,
        vec![b'A'; limits::MAX_REQUEST_SOURCE_BYTES as usize + 1],
    )
    .unwrap();
    let err = analyze::lint(&Input::Path(scr)).expect_err("oversize file must be rejected");
    assert!(
        err.contains("exceeds") && err.contains("per-request limit"),
        "error should name the limit: {err}"
    );
}

#[test]
fn source_at_the_limit_is_accepted() {
    // The boundary is inclusive: source of exactly the cap is analysed, not
    // rejected. (Newlines keep the parser cheap.)
    let at = "\n".repeat(limits::MAX_REQUEST_SOURCE_BYTES as usize);
    assert!(
        analyze::lint(&Input::Inline(at)).is_ok(),
        "source exactly at the limit must be accepted"
    );
}

#[test]
fn project_script_budget_rejects_huge_tree() {
    let dir = tempfile::tempdir().unwrap();
    let proj = dir.path().join("Project.m1prj");
    std::fs::write(&proj, "<Project/>").unwrap();
    for i in 0..(limits::MAX_PROJECT_SCRIPTS + 1) {
        std::fs::write(dir.path().join(format!("s{i}.m1scr")), "").unwrap();
    }
    let err = loader::check_project_script_budget(&proj)
        .expect_err("a project over the script cap must be rejected");
    assert!(
        err.contains("exceeds") && err.contains("script"),
        "error should name the script limit: {err}"
    );
}

#[test]
fn project_script_budget_allows_small_tree() {
    let dir = tempfile::tempdir().unwrap();
    let proj = dir.path().join("Project.m1prj");
    std::fs::write(&proj, "<Project/>").unwrap();
    for i in 0..3 {
        std::fs::write(dir.path().join(format!("s{i}.m1scr")), "").unwrap();
    }
    assert!(loader::check_project_script_budget(&proj).is_ok());
}

#[test]
fn typecheck_rejects_over_budget_project_before_loading() {
    // The budget guard fires before the project is loaded: the error is the
    // script-limit message, not a project-parse failure on the dummy .m1prj.
    let dir = tempfile::tempdir().unwrap();
    let proj = dir.path().join("Project.m1prj");
    std::fs::write(&proj, "<Project/>").unwrap();
    for i in 0..(limits::MAX_PROJECT_SCRIPTS + 1) {
        std::fs::write(dir.path().join(format!("s{i}.m1scr")), "").unwrap();
    }
    let err = analyze::typecheck(&Input::Inline(GOOD.to_string()), Some(&proj))
        .expect_err("over-budget project must be rejected");
    assert!(
        err.contains("exceeds") && err.contains("script"),
        "error should name the script limit: {err}"
    );
}

#[test]
fn analyze_errors_on_missing_input() {
    // A non-existent path is an error, not a panic.
    let err = analyze::lint(&Input::Path("/no/such/file.m1scr".into()));
    assert!(err.is_err(), "unreadable path should error cleanly");
}

// ---- config discovery (audit: format/lint honour project config) ----

#[test]
fn format_reads_brace_style_from_project_config() {
    let dir = tempfile::tempdir().unwrap();
    // A project that pins K&R braces via the unified config.
    std::fs::write(
        dir.path().join("m1-tools.toml"),
        "[format]\nbrace_style = \"kr\"\n",
    )
    .unwrap();
    // An Allman-braced script committed in that project.
    let scr = dir.path().join("x.m1scr");
    let allman = "if (A)\n{\n\tValue = 1;\n}\n";
    std::fs::write(&scr, allman).unwrap();

    // Via the file path, config is discovered, so the Allman source is
    // reformatted toward the project's K&R (brace joined onto the control line).
    let by_path = analyze::format(&Input::Path(scr), false).unwrap();
    let out = by_path.formatted.unwrap();
    assert!(
        out.contains(") {"),
        "expected K&R brace from config, got:\n{out}"
    );

    // The identical source formatted inline (no config) keeps Allman.
    let inline = analyze::format(&Input::Inline(allman.to_string()), false).unwrap();
    assert!(
        !inline.formatted.unwrap().contains(") {"),
        "inline format must use the default Allman, not the project's kr"
    );
}

// ---- CAN model (m1_can) ---------------------------------------------------

/// A DBC module declaring one or more `(message, can_id)` frames, as a `.m1dbc`.
fn m1dbc(module: &str, messages: &[(&str, u32)]) -> String {
    let mut s = String::from("<?xml version=\"1.0\"?>\n<DBC>\n <ComponentStream>\n  <List>\n");
    s.push_str(&format!(
        "   <Component Classname=\"BuiltIn.CAN.DBC\" Name=\"{module}\"/>\n"
    ));
    for (msg, id) in messages {
        s.push_str(&format!(
            "   <Component Classname=\"BuiltIn.CAN.Message\" Name=\"{module}.{msg}\">\n\
             \x20   <Props CANId=\"{id}\" DLC=\"8\" Transmit=\"RX\" Endian=\"Little\"/>\n\
             \x20  </Component>\n"
        ));
    }
    s.push_str("  </List>\n </ComponentStream>\n</DBC>\n");
    s
}

/// A project mirroring the real corpora's CAN layout: several `.m1dbc` modules
/// bound to buses by one `CAN Init` script, with the bus symbols valued the way
/// the real projects value them (a `.m1prj` constant, a `parameters.m1cfg` cell).
///
/// - `Alpha` (bus 1) and `Beta` (bus 2) both declare id 133 — different buses,
///   not a clash (this is exactly what EV-M1 does with `SBG DBC`/`DTI FSIC RL`).
/// - `Alpha` and `Epsilon` are both on bus 1 and both declare id 155 — a real clash.
/// - `Gamma` is bound to the parameter `Spare Bus` (cfg: 2) and `Delta` is never
///   initialised; they share id 144, which can be neither proven nor dismissed.
/// - `Zeta` is bound to the constant `Active Bus` (`.m1prj`: 0) and `Eta` to a
///   literal 0; they share id 177 — a clash, and a retune cannot undo it.
/// - `Theta` (`Spare Bus`, cfg: 2) and `Iota` (literal 1) share id 188 — safe,
///   but only for this calibration.
fn can_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("UQR-X").join("01.00");
    let scripts_dir = project_dir.join("Scripts");
    let dbc_dir = project_dir.join("dbc");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    std::fs::create_dir_all(&dbc_dir).unwrap();

    std::fs::write(
        project_dir.join("Project.m1prj"),
        r#"<?xml version="1.0"?>
<MoTeCM1BuildSession>
 <Project Name="X" TargetHardware="ecu150">
  <ComponentStream>
   <List>
    <Component Classname="BuiltIn.GroupCompound" Name="Root.CAN"/>
    <Component Classname="BuiltIn.Parameter" Name="Root.CAN.Spare Bus">
     <Props Type="s32" Security="Calibration"/>
    </Component>
    <Component Classname="BuiltIn.Constant" Name="Root.CAN.Active Bus">
     <Props Type="s32" Value="0"/>
    </Component>
    <Component Classname="BuiltIn.FuncUser" Filename="CAN.CAN Init.m1scr" Name="Root.CAN.CAN Init"/>
    <Component Classname="BuiltIn.CAN.DBCRoot" Name="DBC"/>
    <Component Classname="BuiltIn.CAN.DBC" Name="DBC.Alpha"/>
    <Component Classname="BuiltIn.CAN.DBC" Name="DBC.Beta"/>
    <Component Classname="BuiltIn.CAN.DBC" Name="DBC.Gamma"/>
    <Component Classname="BuiltIn.CAN.DBC" Name="DBC.Delta"/>
    <Component Classname="BuiltIn.CAN.DBC" Name="DBC.Epsilon"/>
    <Component Classname="BuiltIn.CAN.DBC" Name="DBC.Zeta"/>
    <Component Classname="BuiltIn.CAN.DBC" Name="DBC.Eta"/>
    <Component Classname="BuiltIn.CAN.DBC" Name="DBC.Theta"/>
    <Component Classname="BuiltIn.CAN.DBC" Name="DBC.Iota"/>
   </List>
  </ComponentStream>
 </Project>
</MoTeCM1BuildSession>
"#,
    )
    .unwrap();

    for (module, messages) in [
        ("Alpha", &[("Status", 133u32), ("Extra", 155)][..]),
        ("Beta", &[("Status", 133)][..]),
        ("Gamma", &[("Status", 144)][..]),
        ("Delta", &[("Status", 144)][..]),
        ("Epsilon", &[("Status", 155)][..]),
        ("Zeta", &[("Status", 177)][..]),
        ("Eta", &[("Status", 177)][..]),
        ("Theta", &[("Status", 188)][..]),
        ("Iota", &[("Status", 188)][..]),
    ] {
        std::fs::write(
            dbc_dir.join(format!("{module}.m1dbc")),
            m1dbc(module, messages),
        )
        .unwrap();
    }

    std::fs::write(
        scripts_dir.join("CAN.CAN Init.m1scr"),
        "DBC.Alpha.Init(1);\nDBC.Beta.Init(2);\nDBC.Gamma.Init(Spare Bus);\nDBC.Epsilon.Init(1);\n\
         DBC.Zeta.Init(Active Bus);\nDBC.Eta.Init(0);\nDBC.Theta.Init(Spare Bus);\nDBC.Iota.Init(1);\n",
    )
    .unwrap();

    // The calibration: a parameter's value lives only here (real exports drop
    // the implicit `Root.` prefix, as this one does).
    std::fs::write(
        dir.path().join("parameters.m1cfg"),
        r#"<?xml version="1.0"?>
<Configuration>
 <Group Name="">
  <Parameter Name="CAN.Spare Bus">
   <Cell Type="s32"><![CDATA[2]]></Cell>
  </Parameter>
 </Group>
</Configuration>
"#,
    )
    .unwrap();

    let project = project_dir.join("Project.m1prj");
    (dir, project)
}

#[test]
fn can_binds_each_dbc_module_to_the_bus_its_init_call_names() {
    let (_dir, project) = can_fixture();
    let out = can::inspect(&project, None, 200).expect("can inspect runs");

    let alpha = out.modules.iter().find(|m| m.name == "Alpha").unwrap();
    assert!(alpha.initialised);
    assert_eq!(alpha.bus.as_deref(), Some("1"));
    assert_eq!(alpha.bus_kind, "literal");
    assert_eq!(alpha.message_count, 2);
    let init = &alpha.init_calls[0];
    assert_eq!(init.script, "CAN.CAN Init.m1scr");
    assert_eq!(init.line, 1, "1-based line of the Init call");
    assert!(init.call.starts_with("DBC.Alpha.Init"), "{}", init.call);

    assert_eq!(alpha.bus_value, Some(1), "a literal resolves to itself");
    assert!(!alpha.bus_calibrated);

    // A parameter bus resolves through parameters.m1cfg — the only place a
    // parameter's value exists — and is marked as calibration-sourced.
    let gamma = out.modules.iter().find(|m| m.name == "Gamma").unwrap();
    assert_eq!(gamma.bus.as_deref(), Some("Spare Bus"));
    assert_eq!(gamma.bus_kind, "parameter");
    assert_eq!(gamma.bus_value, Some(2), "from the .m1cfg cell");
    assert!(gamma.bus_calibrated, "a retune can move it");

    // A constant bus resolves from the .m1prj and is NOT calibration-dependent.
    let zeta = out.modules.iter().find(|m| m.name == "Zeta").unwrap();
    assert_eq!(zeta.bus.as_deref(), Some("Active Bus"));
    assert_eq!(zeta.bus_kind, "constant");
    assert_eq!(zeta.bus_value, Some(0), "from the .m1prj Props Value");
    assert!(!zeta.bus_calibrated);
}

#[test]
fn can_matches_a_constant_bus_against_a_literal_one() {
    let (_dir, project) = can_fixture();
    let out = can::inspect(&project, None, 200).expect("can inspect runs");

    // `Active Bus` is a constant with value 0, so `Init(Active Bus)` and
    // `Init(0)` are the same bus — provable without any calibration.
    let o = out
        .id_overlaps
        .iter()
        .find(|o| o.can_id == 177)
        .expect("id 177 is declared twice");
    assert_eq!(o.verdict, "same-bus", "{}", o.note);
    assert!(
        !o.depends_on_calibration,
        "a constant is fixed by the project, so no retune caveat: {}",
        o.note
    );
}

#[test]
fn can_flags_a_verdict_that_rests_on_calibration() {
    let (_dir, project) = can_fixture();
    let out = can::inspect(&project, None, 200).expect("can inspect runs");

    // `Spare Bus` (cfg: 2) vs literal 1 — different buses today, but retuning
    // the parameter to 1 would make it a clash. The verdict says so.
    let o = out
        .id_overlaps
        .iter()
        .find(|o| o.can_id == 188)
        .expect("id 188 is declared twice");
    assert_eq!(o.verdict, "different-bus", "{}", o.note);
    assert!(
        o.depends_on_calibration,
        "the bus came from parameters.m1cfg: {}",
        o.note
    );
    assert!(
        o.note.contains("retune"),
        "the note must spell the caveat out: {}",
        o.note
    );
}

#[test]
fn can_reports_a_dbc_that_no_script_initialises() {
    let (_dir, project) = can_fixture();
    let out = can::inspect(&project, None, 200).expect("can inspect runs");

    assert_eq!(out.uninitialised_modules, vec!["Delta".to_string()]);
    let delta = out.modules.iter().find(|m| m.name == "Delta").unwrap();
    assert!(!delta.initialised);
    assert!(delta.bus.is_none());
    assert_eq!(delta.bus_kind, "none");
}

#[test]
fn can_does_not_call_the_same_id_on_different_buses_a_clash() {
    let (_dir, project) = can_fixture();
    let out = can::inspect(&project, None, 200).expect("can inspect runs");

    let o = out
        .id_overlaps
        .iter()
        .find(|o| o.can_id == 133)
        .expect("id 133 is declared by two modules");
    assert_eq!(o.verdict, "different-bus", "{}", o.note);
    assert_eq!(o.can_id_hex, "0x85");
    assert!(o.bus.is_none(), "no shared bus when the buses differ");
    let buses: Vec<_> = o.messages.iter().map(|m| m.bus.clone()).collect();
    assert_eq!(
        buses,
        vec![Some("1".to_string()), Some("2".to_string())],
        "each member carries the bus its module was Init'd on"
    );
}

#[test]
fn can_flags_the_same_id_on_the_same_bus() {
    let (_dir, project) = can_fixture();
    let out = can::inspect(&project, None, 200).expect("can inspect runs");

    let o = out
        .id_overlaps
        .iter()
        .find(|o| o.can_id == 155)
        .expect("id 155 is declared twice");
    assert_eq!(o.verdict, "same-bus", "{}", o.note);
    assert_eq!(o.bus.as_deref(), Some("1"));
    let paths: Vec<_> = o.messages.iter().map(|m| m.path.as_str()).collect();
    assert_eq!(paths, vec!["Alpha.Extra", "Epsilon.Status"]);
}

#[test]
fn can_leaves_a_non_static_bus_undecided() {
    let (_dir, project) = can_fixture();
    let out = can::inspect(&project, None, 200).expect("can inspect runs");

    let o = out
        .id_overlaps
        .iter()
        .find(|o| o.can_id == 144)
        .expect("id 144 is declared twice");
    assert_eq!(
        o.verdict, "unknown",
        "an uninitialised module has no bus at all, so nothing is proven: {}",
        o.note
    );
    assert!(o.bus.is_none());
    assert!(
        !o.depends_on_calibration,
        "an undecided verdict rests on nothing, calibration included"
    );
}

#[test]
fn can_lists_messages_with_id_direction_and_bus() {
    let (_dir, project) = can_fixture();
    let out = can::inspect(&project, None, 200).expect("can inspect runs");

    assert_eq!(out.total_messages, 10);
    let m = out
        .messages
        .iter()
        .find(|m| m.path == "Beta.Status")
        .unwrap();
    assert_eq!(m.module, "Beta");
    assert_eq!(m.can_id, Some(133));
    assert_eq!(m.can_id_hex.as_deref(), Some("0x85"));
    assert_eq!(m.dlc, Some(8));
    assert_eq!(m.direction.as_deref(), Some("RX"));
    assert_eq!(m.bus.as_deref(), Some("2"));

    // The guidance travels with the data, so an agent reading only the tool
    // output still learns the bus rule.
    assert!(
        out.guidance.iter().any(|g| g.contains("Init")),
        "guidance must state the Init/bus rule"
    );
}

#[test]
fn can_filter_and_limit_narrow_messages_but_not_the_verdicts() {
    let (_dir, project) = can_fixture();
    let out = can::inspect(&project, Some("alpha"), 1).expect("can inspect runs");

    assert_eq!(out.messages.len(), 1, "limit caps the returned list");
    assert!(out.messages[0].path.starts_with("Alpha."));
    assert_eq!(out.total_messages, 10, "total is the unfiltered count");
    assert!(
        out.id_overlaps.iter().any(|o| o.can_id == 144),
        "overlaps are computed over every message, not the filtered subset"
    );
}

#[test]
fn can_rejects_an_over_budget_project() {
    let dir = tempfile::tempdir().unwrap();
    let scripts = dir.path().join("Scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    for i in 0..=limits::MAX_PROJECT_SCRIPTS {
        std::fs::write(scripts.join(format!("s{i}.m1scr")), "A is True\n").unwrap();
    }
    std::fs::write(dir.path().join("Project.m1prj"), "<MoTeCM1BuildSession/>").unwrap();
    let err = can::inspect(&dir.path().join("Project.m1prj"), None, 200).unwrap_err();
    assert!(err.contains("exceeds"), "unexpected error: {err}");
}
