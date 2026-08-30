//! Integration tests for the m1-mcp tool implementations. These drive the
//! in-process analyser functions directly (the same functions the MCP tools
//! call), asserting on the serializable DTOs.

use m1_mcp::analyze::{self, DiagnosticScope, DiagnosticSourceDto, Input, LintFixOutcome};
use m1_mcp::doc::{self, DocKind};
use m1_mcp::{limits, loader};

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
    let hit = out
        .diagnostics
        .iter()
        .find(|d| d.code == "syntax" || d.severity == "error")
        .unwrap_or_else(|| panic!("broken source should produce an error: {out:?}"));
    assert_eq!(hit.scope, DiagnosticScope::Source);
    assert_eq!(hit.source, DiagnosticSourceDto::Inline);
    assert!(hit.line >= 1 && hit.column >= 1);
    assert!(hit.end_line >= hit.line);
    assert!(hit.byte_end >= hit.byte_start);
    assert_eq!(
        serde_json::to_value(&hit.source).unwrap(),
        serde_json::json!({ "kind": "inline" }),
        "inline diagnostics need an explicit serialized source marker"
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
        concat!(
            "local fRearPolePairs = Calculate.Max(1.0, DTI FSIC Rear.Pole Pairs);\n",
            "DTI FSIC Rear.Pole Pairs = 1.5;\n",
        ),
    )
    .unwrap();

    let project = project_dir.join("Project.m1prj");
    let out =
        analyze::typecheck(&Input::Path(script.clone()), Some(&project)).expect("typecheck runs");
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
    assert_eq!(hit.scope, DiagnosticScope::Source);
    assert_eq!(
        hit.source,
        DiagnosticSourceDto::Path {
            path: script.display().to_string()
        }
    );
    assert!(hit.end_line >= hit.line);
    assert!(hit.byte_end > hit.byte_start);

    let mismatch = out
        .diagnostics
        .iter()
        .find(|d| d.code == "T030")
        .expect("float assignment should produce a two-location T030");
    let related = mismatch
        .related
        .first()
        .expect("T030 should retain its declaration location");
    assert_eq!(related.path, project.display().to_string());
    assert!(related.line >= 1);
    assert!(related.message.contains("declared"));
}

#[test]
fn typecheck_resolves_related_declarations_to_project_or_dbc_files() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("Project.m1prj");
    std::fs::write(
        &project,
        r#"<?xml version="1.0"?>
<MoTeCM1BuildSession>
 <Project Name="Related" TargetHardware="ecu120">
  <DataTypes><Type Name="Switch State" Storage="enum" Default="Off"><Enum Name="Off" ContainerOrder="0"/><Enum Name="On" ContainerOrder="1"/></Type></DataTypes>
  <ComponentStream>
   <List>
    <Component Classname="BuiltIn.GroupCompound" Name="Root.Ctrl"/>
    <Component Classname="BuiltIn.FuncUserParam" Filename="Helper.m1scr" Name="Root.Ctrl.Helper">
     <Signature Name="" ReturnType="f32">
      <Params><Param Name="BusA" Type="f32" Attrs="0"/></Params>
     </Signature>
    </Component>
    <Component Classname="BuiltIn.FuncUser" Filename="Caller.m1scr" Name="Root.Ctrl.Caller"/>
    <Component Classname="BuiltIn.Parameter" Name="Root.Ctrl.SwitchMode.Value"><Props/></Component>
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
<Configuration><Group Name=""><Parameter Name="Root.Ctrl.SwitchMode.Value"><Cell Type="enum"><![CDATA[On]]></Cell></Parameter></Group></Configuration>
"#,
    )
    .unwrap();

    let dbc_xml = |root: &str| {
        format!(
            r#"<?xml version="1.0"?>
<DBC>
 <ComponentStream>
  <List>



   <Component Classname="BuiltIn.CAN.DBC" Name="{root}"/>
   <Component Classname="BuiltIn.CAN.Message" Name="{root}.Frame"><Props CANId="100" DLC="8"/></Component>
   <Component Classname="BuiltIn.CAN.Signal" Name="{root}.Frame.Count"><Props Type="u32" StartBit="0" Length="10"/></Component>
  </List>
 </ComponentStream>
</DBC>
"#
        )
    };
    let dbc_a = dir.path().join("BusA.m1dbc");
    let dbc_b = dir.path().join("BusB.m1dbc");
    std::fs::write(&dbc_a, dbc_xml("BusA")).unwrap();
    std::fs::write(&dbc_b, dbc_xml("BusB")).unwrap();

    let caller = dir.path().join("Caller.m1scr");
    std::fs::write(
        &caller,
        "BusB.Frame.Count = 1.5;\nSwitchMode.Value = 3;\nlocal result = Helper();\n",
    )
    .unwrap();
    let helper = dir.path().join("Helper.m1scr");
    std::fs::write(&helper, "Out = 1.0;\n").unwrap();

    let out = analyze::typecheck(&Input::Path(caller), Some(&project)).expect("typecheck runs");
    let assignment = out
        .diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.code == "T030"
                && diagnostic.message.contains("Unsigned")
                && diagnostic
                    .related
                    .iter()
                    .any(|related| related.message.contains("BusB.Frame.Count"))
        })
        .unwrap_or_else(|| panic!("DBC assignment should produce T030: {out:?}"));
    assert_eq!(assignment.related.len(), 1);
    assert_eq!(assignment.related[0].path, dbc_b.display().to_string());

    let enum_assignment = out
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "T030" && diagnostic.message.contains("Switch State"))
        .unwrap_or_else(|| panic!("enum assignment should produce T030: {out:?}"));
    assert_eq!(enum_assignment.related.len(), 1);
    assert_eq!(
        enum_assignment.related[0].path,
        project.display().to_string()
    );

    let arity = out
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "T085")
        .unwrap_or_else(|| panic!("bad Helper call should produce T085: {out:?}"));
    assert_eq!(arity.related.len(), 1);
    assert_eq!(arity.related[0].path, project.display().to_string());
    assert_ne!(
        arity.related[0].path,
        dir.path().join("Helper.m1scr").display().to_string()
    );

    // T098 quotes both the unused argument (`BusA`) and the defining function.
    // The DBC root deliberately shares the function's declaration line, so
    // provenance resolution must use the final defining-symbol token.
    let helper_out =
        analyze::typecheck(&Input::Path(helper), Some(&project)).expect("typecheck runs");
    let unused = helper_out
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "T098")
        .unwrap_or_else(|| panic!("unused BusA argument should produce T098: {helper_out:?}"));
    assert_eq!(unused.related.len(), 1);
    assert_eq!(unused.related[0].path, project.display().to_string());
}

#[test]
fn typecheck_identifies_project_diagnostic_path_and_subject() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("Project.m1prj");
    std::fs::write(
        &project,
        r#"<?xml version="1.0"?>
<MoTeCM1BuildSession>
 <Project Name="Schedule" TargetHardware="ecu120">
  <ComponentStream>
   <List>
    <Component Classname="BuiltIn.GroupCompound" Name="Root.Events"/>
    <Component Classname="BuiltIn.EventKernel" Name="Root.Events.On 100Hz"/>
    <Component Classname="BuiltIn.GroupCompound" Name="Root.Ctrl"/>
    <Component Classname="BuiltIn.FuncUser" Filename="Ctrl.Alpha.m1scr" Name="Root.Ctrl.Alpha">
     <Props SelectedTrigger="Parent.Events.On 100Hz"/>
    </Component>
    <Component Classname="BuiltIn.FuncUser" Filename="Ctrl.Beta.m1scr" Name="Root.Ctrl.Beta">
     <Props SelectedTrigger="Parent.Events.On 100Hz"/>
    </Component>
    <Component Classname="BuiltIn.Channel" Name="Root.Ctrl.A Out"><Props Type="f32"/></Component>
    <Component Classname="BuiltIn.Channel" Name="Root.Ctrl.B Out"><Props Type="f32"/></Component>
   </List>
  </ComponentStream>
 </Project>
</MoTeCM1BuildSession>
"#,
    )
    .unwrap();
    let alpha = dir.path().join("Ctrl.Alpha.m1scr");
    let beta = dir.path().join("Ctrl.Beta.m1scr");
    std::fs::write(&alpha, "A Out = B Out + 1.0;\n").unwrap();
    std::fs::write(&beta, "B Out = A Out + 1.0;\n").unwrap();

    let out = analyze::typecheck(&Input::Path(alpha), Some(&project)).expect("typecheck runs");
    let cycle = out
        .diagnostics
        .iter()
        .find(|d| d.code == "T088")
        .unwrap_or_else(|| panic!("same-rate cycle should produce T088: {out:?}"));
    assert_eq!(cycle.scope, DiagnosticScope::Project);
    assert_eq!(
        cycle.source,
        DiagnosticSourceDto::Path {
            path: project.display().to_string()
        }
    );
    assert_eq!(cycle.subject.as_deref(), Some("Root.Ctrl.Alpha"));
    assert_eq!((cycle.line, cycle.column), (1, 1));
    assert_eq!((cycle.byte_start, cycle.byte_end), (0, 0));
}

#[test]
fn lint_returns_consistent_counts() {
    let out = analyze::lint(&Input::Inline(GOOD.to_string()), false).expect("lint runs");
    let warns = out
        .diagnostics
        .iter()
        .filter(|d| d.severity == "warning")
        .count();
    assert_eq!(out.warning_count, warns);
    assert!(!out.excluded);
    assert!(out.fix.is_none(), "no fix was requested");
}

#[test]
fn lint_finding_reports_name_and_fixability_and_returns_safe_fix() {
    let out =
        analyze::lint(&Input::Inline("Result = A == B;\n".to_string()), true).expect("lint runs");
    let finding = out
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "L004")
        .expect("L004 finding");
    assert_eq!(finding.name, "eq-operator-preferred");
    assert!(finding.fixable);
    assert_eq!(finding.scope, DiagnosticScope::Source);
    assert_eq!(finding.source, DiagnosticSourceDto::Inline);
    assert!(finding.byte_end > finding.byte_start);
    assert!(finding.end_line >= finding.line);
    assert!(finding.subject.is_none());
    assert!(finding.related.is_empty());
    assert_eq!(
        out.fix,
        Some(LintFixOutcome::Fixed {
            source: "Result = A eq B;\n".to_string(),
        })
    );
}

#[test]
fn unfixable_lint_finding_returns_unchanged() {
    let source = format!("// {}\n", "x".repeat(90));
    let out = analyze::lint(&Input::Inline(source), true).expect("lint runs");
    let finding = out
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "L001")
        .expect("L001 finding");
    assert_eq!(finding.name, "line-too-long");
    assert!(!finding.fixable);
    assert_eq!(out.fix, Some(LintFixOutcome::Unchanged));
}

#[test]
fn syntax_errors_bypass_lint_fixes() {
    // The upstream fixer can repair a missing semicolon. MCP fix mode must not
    // invoke it when the original parse has any syntax error.
    let out = analyze::lint(&Input::Inline("Result = 1\n".to_string()), true).expect("lint runs");
    let syntax = out
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "syntax")
        .expect("syntax diagnostic");
    assert_eq!(syntax.name, "syntax-error");
    assert!(!syntax.fixable);
    assert_eq!(out.fix, Some(LintFixOutcome::Unchanged));
}

#[test]
fn lint_path_fix_is_returned_without_writing_the_file() {
    let dir = tempfile::tempdir().unwrap();
    // Keep the fixture independent of any user-global lint configuration.
    std::fs::write(dir.path().join(".m1lint.toml"), "").unwrap();
    let path = dir.path().join("read-only.m1scr");
    let source = "Result = A == B;\n";
    std::fs::write(&path, source).unwrap();

    let out = analyze::lint(&Input::Path(path.clone()), true).expect("lint runs");
    let finding = out
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "L004")
        .expect("L004 finding");
    assert_eq!(
        finding.source,
        DiagnosticSourceDto::Path {
            path: path.display().to_string(),
        }
    );
    assert_eq!(
        out.fix,
        Some(LintFixOutcome::Fixed {
            source: "Result = A eq B;\n".to_string(),
        })
    );
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        source,
        "lint fix mode must never write path input"
    );
}

#[test]
fn lint_skips_project_excluded_paths_before_diagnostics_or_fixes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".m1lint.toml"),
        "exclude = [\"*.generated.m1scr\"]\n",
    )
    .unwrap();
    let path = dir.path().join("model.generated.m1scr");
    let source = "Result = A == B;\n";
    std::fs::write(&path, source).unwrap();

    let out = analyze::lint(&Input::Path(path.clone()), true).expect("excluded path is skipped");
    assert!(out.excluded);
    assert!(out.diagnostics.is_empty());
    assert_eq!((out.error_count, out.warning_count), (0, 0));
    assert_eq!(out.fix, Some(LintFixOutcome::Unchanged));
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        source,
        "an excluded path must not be fixed or written"
    );

    // Prove exclusion happens before `Input::resolve`: an otherwise rejected
    // oversized path is still skipped, exactly as the CLI skips before reads.
    let oversized = dir.path().join("large.generated.m1scr");
    let file = std::fs::File::create(&oversized).unwrap();
    file.set_len(limits::MAX_REQUEST_SOURCE_BYTES + 1).unwrap();
    let skipped = analyze::lint(&Input::Path(oversized), false)
        .expect("excluded file must not be read or size-checked");
    assert!(skipped.excluded);
    assert!(skipped.diagnostics.is_empty());
    assert!(skipped.fix.is_none());
}

#[test]
fn lint_fix_uses_the_same_project_config_as_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("m1-tools.toml"),
        "[diagnostics]\nselect = [\"L004\"]\n",
    )
    .unwrap();
    // Stop user-global `.m1lint.toml` fallback from affecting the fixture.
    std::fs::write(dir.path().join(".m1lint.toml"), "").unwrap();
    let path = dir.path().join("configured.m1scr");
    std::fs::write(&path, "Result = A == B && C;\n").unwrap();

    let out = analyze::lint(&Input::Path(path), true).expect("lint runs");
    assert_eq!(
        out.diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code.as_str())
            .collect::<Vec<_>>(),
        vec!["L004"]
    );
    assert_eq!(
        out.fix,
        Some(LintFixOutcome::Fixed {
            source: "Result = A eq B && C;\n".to_string(),
        }),
        "the disabled L005 rule must not rewrite `&&`"
    );
}

#[test]
fn lint_fix_output_is_a_stable_fixed_point() {
    let original = "Result = A == B && C;\n";
    let first = analyze::lint(&Input::Inline(original.to_string()), true).expect("lint runs");
    let Some(LintFixOutcome::Fixed { source }) = first.fix else {
        panic!("expected fixed source, got {:?}", first.fix);
    };
    assert_eq!(source, "Result = A eq B and C;\n");

    let second = analyze::lint(&Input::Inline(source), true).expect("lint runs again");
    assert_eq!(second.fix, Some(LintFixOutcome::Unchanged));
}

#[test]
fn exact_lint_rule_lookup_returns_full_metadata() {
    let rule = analyze::lint_rule("L004").expect("known exact code");
    assert_eq!(rule.code, "L004");
    assert_eq!(rule.name, "eq-operator-preferred");
    assert_eq!(rule.severity, "warning");
    assert!(rule.enabled_by_default);
    assert!(rule.fixable);
    assert!(!rule.summary.is_empty());
    assert!(rule.explanation.contains("--fix rewrites"));

    let opt_in = analyze::lint_rule("L017").expect("known opt-in code");
    assert!(!opt_in.enabled_by_default);
    assert!(!opt_in.fixable);
    assert!(analyze::lint_rule("l004").is_err(), "lookup is exact");
    assert!(
        analyze::lint_rule("L013").is_err(),
        "reserved code is unknown"
    );
}

#[test]
fn lint_tool_schemas_expose_fix_and_rule_metadata() {
    let outcome = serde_json::to_value(schemars::schema_for!(analyze::LintOutcome)).unwrap();
    assert_eq!(outcome["type"], "object");
    let outcome_schema = outcome.to_string();
    for field in [
        "name",
        "fixable",
        "excluded",
        "scope",
        "source",
        "end_line",
        "byte_start",
        "outcome",
        "unchanged",
        "fixed",
        "unsafe",
    ] {
        assert!(
            outcome_schema.contains(field),
            "LintOutcome schema missing {field}: {outcome_schema}"
        );
    }

    let lint_params =
        serde_json::to_value(schemars::schema_for!(m1_mcp::server::LintParams)).unwrap();
    assert_eq!(lint_params["type"], "object");
    assert!(lint_params["properties"].get("fix").is_some());
    assert!(
        lint_params["required"]
            .as_array()
            .is_none_or(|required| !required.iter().any(|field| field == "fix")),
        "fix must remain optional"
    );

    let rule = serde_json::to_value(schemars::schema_for!(analyze::LintRuleMetadata)).unwrap();
    assert_eq!(rule["type"], "object");
    for field in [
        "code",
        "name",
        "severity",
        "enabled_by_default",
        "fixable",
        "summary",
        "explanation",
    ] {
        assert!(
            rule["properties"].get(field).is_some(),
            "LintRuleMetadata schema missing {field}"
        );
    }
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
    let err = analyze::lint(&Input::Path(scr), false).expect_err("oversize file must be rejected");
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
        analyze::lint(&Input::Inline(at), false).is_ok(),
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
    let err = analyze::lint(&Input::Path("/no/such/file.m1scr".into()), false);
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
