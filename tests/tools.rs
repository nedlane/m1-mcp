//! Integration tests for the m1-mcp tool implementations. These drive the
//! in-process analyser functions directly (the same functions the MCP tools
//! call), asserting on the serializable DTOs.

use m1_mcp::analyze::{self, DiagnosticScope, DiagnosticSourceDto, Input, LintFixOutcome};
use m1_mcp::doc::{self, DocKind};
use m1_mcp::{can, completeness, limits, loader, project_check, symbols};

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

#[test]
fn doc_and_typecheck_results_name_the_catalogue_target() {
    let docs: doc::DocResults = doc::search("Absolute", 1).into();
    assert_eq!(
        docs.catalogue_target,
        m1_typecheck::intrinsics::active_target()
    );

    let checked = analyze::typecheck(&inline("local x = 1;\n"), None).expect("typecheck runs");
    assert_eq!(
        checked.catalogue_target,
        m1_typecheck::intrinsics::active_target()
    );
}

// ---- analysers ------------------------------------------------------------

/// A tiny well-formed M1 script body used across the analyser tests.
const GOOD: &str = "Engine Speed Warning is True\n";

fn inline(source: impl Into<String>) -> Input {
    Input::Inline {
        source: source.into(),
        context_path: None,
    }
}

fn inline_at(source: impl Into<String>, context_path: impl Into<std::path::PathBuf>) -> Input {
    Input::Inline {
        source: source.into(),
        context_path: Some(context_path.into()),
    }
}

fn has_code(diagnostics: &[analyze::DiagnosticDto], code: &str) -> bool {
    diagnostics.iter().any(|d| d.code == code)
}

const MINIMAL_PROJECT: &str = r#"<?xml version="1.0"?>
<MoTeCM1BuildSession>
 <Project Name="Load Report" TargetHardware="ecu120">
  <ComponentStream><List>
   <Component Classname="BuiltIn.GroupCompound" Name="Root.Test"/>
  </List></ComponentStream>
 </Project>
</MoTeCM1BuildSession>
"#;

const EMPTY_CONFIG: &str = r#"<?xml version="1.0"?>
<Configuration><Group Name=""/></Configuration>
"#;

const VALID_DBC: &str = r#"<?xml version="1.0"?>
<DBC><ComponentStream><List>
 <Component Classname="BuiltIn.CAN.DBC" Name="Good"/>
 <Component Classname="BuiltIn.CAN.Message" Name="Good.Status">
  <Props CANId="100" DLC="8" Transmit="RX"/>
 </Component>
</List></ComponentStream></DBC>
"#;

const PIPELINE_PROJECT: &str = r#"<?xml version="1.0"?>
<MoTeCM1BuildSession>
 <Project Name="Pipeline" TargetHardware="ecu120">
  <ComponentStream><List>
   <Component Classname="BuiltIn.GroupCompound" Name="Root.Foo"/>
   <Component Classname="BuiltIn.Parameter" Name="Root.Foo.Gain.Value"><Props/></Component>
   <Component Classname="BuiltIn.Table" Name="Root.Foo.Map"><Props/></Component>
   <Component Classname="BuiltIn.Channel" Name="Root.Foo.Speed">
    <Props Qty="rad/s"><Locale><Default Unit="%"/></Locale></Props>
   </Component>
   <Component Classname="BuiltIn.Channel" Name="Root.Foo.Menu">
    <Props Storage="Flash" Security="Tune"/>
   </Component>
   <Component Classname="BuiltIn.FuncUser" Filename="Foo.Update.m1scr" Name="Root.Foo.Update">
    <Props SelectedTrigger="Root.Events.On 100Hz"/>
   </Component>
  </List></ComponentStream>
 </Project>
</MoTeCM1BuildSession>
"#;

const PIPELINE_SOURCE: &str = concat!(
    "local f = 1.5; if (f == 2.5) { }\n",
    "local x = Calculate.Max(1, 2, 3);\n",
);

fn write_pipeline_project(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let project = dir.join("Project.m1prj");
    let script = dir.join("Foo.Update.m1scr");
    std::fs::write(&project, PIPELINE_PROJECT).unwrap();
    std::fs::write(dir.join("parameters.m1cfg"), EMPTY_CONFIG).unwrap();
    std::fs::write(&script, PIPELINE_SOURCE).unwrap();
    (project, script)
}

fn write_minimal_project(dir: &std::path::Path) -> std::path::PathBuf {
    let project = dir.join("Project.m1prj");
    std::fs::write(&project, MINIMAL_PROJECT).unwrap();
    project
}

#[test]
fn typecheck_reports_no_error_count_for_reasonable_source() {
    let out = analyze::typecheck(&inline(GOOD), None).expect("typecheck runs");
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
fn typecheck_runs_the_complete_upstream_project_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    let (project, script) = write_pipeline_project(dir.path());

    let out = analyze::typecheck(&Input::Path(script), Some(&project)).expect("typecheck runs");
    let codes = out
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code.as_str())
        .collect::<Vec<_>>();
    for expected in ["T041", "T092", "T095", "T111"] {
        assert!(codes.contains(&expected), "missing {expected}: {codes:?}");
    }
    assert!(
        codes.contains(&"T002"),
        "missing default source rule: {codes:?}"
    );
    assert!(
        !codes.contains(&"T064"),
        "T064 must remain opt-in: {codes:?}"
    );
}

#[test]
fn typecheck_discovers_select_and_filters_source_and_project_findings() {
    let dir = tempfile::tempdir().unwrap();
    let (project, script) = write_pipeline_project(dir.path());
    std::fs::write(
        dir.path().join("m1-tools.toml"),
        "[diagnostics]\nselect = [\"T064\"]\n",
    )
    .unwrap();

    let out = analyze::typecheck(&Input::Path(script), Some(&project)).expect("typecheck runs");
    assert_eq!(
        out.diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code.as_str())
            .collect::<Vec<_>>(),
        vec!["T064"]
    );
}

#[test]
fn typecheck_select_activates_opt_in_project_rule_t089() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("Project.m1prj");
    std::fs::write(
        &project,
        r#"<?xml version="1.0"?>
<MoTeCM1BuildSession>
 <Project Name="Schedule" TargetHardware="ecu120">
  <ComponentStream><List>
   <Component Classname="BuiltIn.GroupCompound" Name="Root.Events"/>
   <Component Classname="BuiltIn.EventKernel" Name="Root.Events.On 100Hz"/>
   <Component Classname="BuiltIn.EventKernel" Name="Root.Events.On 500Hz"/>
   <Component Classname="BuiltIn.GroupCompound" Name="Root.Ctrl"/>
   <Component Classname="BuiltIn.FuncUser" Filename="Ctrl.Slow.m1scr" Name="Root.Ctrl.Slow">
    <Props SelectedTrigger="Parent.Events.On 100Hz"/>
   </Component>
   <Component Classname="BuiltIn.FuncUser" Filename="Ctrl.Fast.m1scr" Name="Root.Ctrl.Fast">
    <Props SelectedTrigger="Parent.Events.On 500Hz"/>
   </Component>
   <Component Classname="BuiltIn.Channel" Name="Root.Ctrl.Slow Out"><Props Type="f32"/></Component>
   <Component Classname="BuiltIn.Channel" Name="Root.Ctrl.Fast Out"><Props Type="f32"/></Component>
  </List></ComponentStream>
 </Project>
</MoTeCM1BuildSession>
"#,
    )
    .unwrap();
    let slow = dir.path().join("Ctrl.Slow.m1scr");
    std::fs::write(&slow, "Slow Out = 1.0;\n").unwrap();
    std::fs::write(
        dir.path().join("Ctrl.Fast.m1scr"),
        "Fast Out = Slow Out + 1.0;\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("m1-tools.toml"),
        "[diagnostics]\nselect = [\"T089\"]\n",
    )
    .unwrap();

    let out = analyze::typecheck(&Input::Path(slow), Some(&project)).expect("typecheck runs");
    assert_eq!(out.diagnostics.len(), 1, "unexpected findings: {out:?}");
    assert_eq!(out.diagnostics[0].code, "T089");
    assert_eq!(out.diagnostics[0].scope, DiagnosticScope::Project);
}

#[test]
fn typecheck_discovers_ignore_and_ignore_symbols_for_project_findings() {
    let dir = tempfile::tempdir().unwrap();
    let (project, script) = write_pipeline_project(dir.path());
    std::fs::write(
        dir.path().join("m1-tools.toml"),
        concat!(
            "[diagnostics]\n",
            "ignore = [\"T095\"]\n",
            "ignore_symbols = [",
            "\"T041:Root.Foo.Gain.Value\", ",
            "\"T092:Root.Foo.Map\"",
            "]\n",
        ),
    )
    .unwrap();

    let out = analyze::typecheck(&Input::Path(script), Some(&project)).expect("typecheck runs");
    let codes = out
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code.as_str())
        .collect::<Vec<_>>();
    for suppressed in ["T041", "T092", "T095"] {
        assert!(
            !codes.contains(&suppressed),
            "unexpected {suppressed}: {codes:?}"
        );
    }
    assert!(
        codes.contains(&"T111"),
        "unrelated finding missing: {codes:?}"
    );
}

#[test]
fn typecheck_flags_a_syntax_error() {
    // Unterminated construct → the parser should emit a syntax diagnostic.
    let out = analyze::typecheck(&inline("if ("), None).expect("runs");
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
fn project_check_reports_each_file_and_separates_project_findings() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("Project.m1prj");
    std::fs::write(
        &project,
        r#"<?xml version="1.0"?>
<MoTeCM1BuildSession>
 <Project Name="Whole Project" TargetHardware="ecu120">
  <ComponentStream><List>
   <Component Classname="BuiltIn.GroupCompound" Name="Root.Test"/>
   <Component Classname="BuiltIn.Parameter" Name="Root.Test.Missing.Value"><Props/></Component>
   <Component Classname="BuiltIn.FuncUser" Filename="Test.Broken.m1scr" Name="Root.Test.Broken"/>
   <Component Classname="BuiltIn.FuncUser" Filename="Test.Style.m1scr" Name="Root.Test.Style"/>
  </List></ComponentStream>
 </Project>
</MoTeCM1BuildSession>
"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("parameters.m1cfg"), EMPTY_CONFIG).unwrap();
    let broken = dir.path().join("Test.Broken.m1scr");
    let style = dir.path().join("Test.Style.m1scr");
    std::fs::write(&broken, "if (\n").unwrap();
    std::fs::write(&style, "local equal=A==B;\n").unwrap();

    let out =
        project_check::check_project(&project, &project_check::ProjectCheckOptions::default())
            .expect("project check runs");

    assert_eq!(out.totals.files, 2);
    assert_eq!(out.files.len(), 2);
    assert!(
        out.files
            .iter()
            .any(|file| file.path == broken.display().to_string())
    );
    assert!(
        out.files
            .iter()
            .any(|file| file.path == style.display().to_string())
    );
    assert!(
        out.totals.errors > 0,
        "source syntax error must count: {out:?}"
    );
    assert!(
        out.totals.lint_findings > 0,
        "lint finding must count: {out:?}"
    );
    assert!(
        out.totals.files_needing_format > 0,
        "format drift must count: {out:?}"
    );
    assert!(
        out.project_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "T041"),
        "project finding must be separate: {out:?}"
    );
    assert_eq!(out.load_report.project, project.display().to_string());

    let filtered = project_check::check_project(
        &project,
        &project_check::ProjectCheckOptions {
            checks: vec![
                project_check::CheckKind::Lint,
                project_check::CheckKind::Format,
            ],
            filter: Some("style".to_string()),
            per_file_diagnostic_limit: 1,
        },
    )
    .expect("filtered project check runs");
    assert_eq!(filtered.files.len(), 1);
    assert!(filtered.files[0].typecheck.is_none());
    assert_eq!(
        filtered.files[0]
            .lint
            .as_ref()
            .expect("lint selected")
            .diagnostics
            .len(),
        1
    );
    assert!(
        filtered.files[0]
            .lint
            .as_ref()
            .expect("lint selected")
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.source
                == DiagnosticSourceDto::Path {
                    path: style.display().to_string()
                })
    );
    assert!(filtered.files[0].format.is_some());
    assert!(filtered.files[0].diagnostics_truncated);
    assert!(filtered.diagnostics_truncated);
    assert!(filtered.project_diagnostics.is_empty());
}

#[test]
fn inline_context_path_anchors_project_group_and_backing_function() {
    let dir = tempfile::tempdir().unwrap();
    let scripts_dir = dir.path().join("Scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let project = dir.path().join("Project.m1prj");
    std::fs::write(
        &project,
        r#"<?xml version="1.0"?>
<MoTeCM1BuildSession>
 <Project Name="Context" TargetHardware="ecu150">
  <ComponentStream>
   <List>
    <Component Classname="BuiltIn.GroupCompound" Name="Root.Ctrl"/>
    <Component Classname="BuiltIn.Parameter" Name="Root.Ctrl.Gain">
     <Props Type="u32" Security="Calibration"/>
    </Component>
    <Component Classname="BuiltIn.FuncUserParam"
      Filename="Ctrl.Scale.m1scr" Name="Root.Ctrl.Scale">
     <Signature Name="" ReturnType="f32">
      <Params><Param Name="Input" Type="f32" Attrs="0"/></Params>
     </Signature>
    </Component>
   </List>
  </ComponentStream>
 </Project>
</MoTeCM1BuildSession>
"#,
    )
    .unwrap();

    let source = "local mixed = Calculate.Max(1.0, Gain);\nlocal inputCopy = In.Input;\n";
    let standalone = analyze::typecheck(&inline(source), None).expect("standalone check runs");
    let without_context = analyze::typecheck(&inline(source), Some(&project))
        .expect("project-only inline check runs");

    let context_path = scripts_dir.join("Ctrl.Scale.m1scr");
    assert!(
        !context_path.exists(),
        "the logical script must stay unsaved"
    );
    let with_context = analyze::typecheck(&inline_at(source, context_path.clone()), Some(&project))
        .expect("contextual inline check runs");

    assert!(
        !has_code(&standalone.diagnostics, "T065") && !has_code(&standalone.diagnostics, "T099"),
        "standalone source has neither project nor path context: {:?}",
        standalone.diagnostics
    );
    assert!(
        !has_code(&without_context.diagnostics, "T065"),
        "without a group, relative Gain stays opaque: {:?}",
        without_context.diagnostics
    );
    assert!(
        !has_code(&without_context.diagnostics, "T099"),
        "without a backing function, the return contract is unknown: {:?}",
        without_context.diagnostics
    );
    assert!(
        has_code(&with_context.diagnostics, "T065"),
        "context must resolve Ctrl.Gain as u32: {:?}",
        with_context.diagnostics
    );
    assert!(
        has_code(&with_context.diagnostics, "T099"),
        "context must resolve Ctrl.Scale's declared return: {:?}",
        with_context.diagnostics
    );
    let contextual = with_context
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "T065" || diagnostic.code == "T099")
        .expect("contextual analysis should produce a source diagnostic");
    assert_eq!(
        contextual.source,
        DiagnosticSourceDto::Inline,
        "context_path is analysis context, not file-backed provenance"
    );
    assert_eq!(
        serde_json::to_value(&contextual.source).unwrap(),
        serde_json::json!({ "kind": "inline" })
    );
    assert!(
        !context_path.exists(),
        "typecheck must not create or read the logical script"
    );
}

#[test]
fn inline_source_replaces_a_stale_project_script_without_overwriting_it() {
    let dir = tempfile::tempdir().unwrap();
    let scripts_dir = dir.path().join("Scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let project = dir.path().join("Project.m1prj");
    std::fs::write(&project, MINIMAL_PROJECT).unwrap();
    let context_path = scripts_dir.join("Ctrl.Scale.m1scr");
    let stale_source = "Out.Result = 1;\n";
    let inline_source = "Out.Result = 2;\n";
    std::fs::write(&context_path, stale_source).unwrap();

    let loaded =
        loader::load_project_full_with_inline(&project, Some((&context_path, inline_source)))
            .expect("project loads");
    let contextual = loaded
        .scripts
        .iter()
        .find(|script| script.name == "Ctrl.Scale.m1scr")
        .expect("the contextual script is included in the project pass");

    assert_eq!(contextual.cst.source(), inline_source);
    assert_eq!(
        std::fs::read_to_string(&context_path).unwrap(),
        stale_source,
        "context_path must not be overwritten"
    );
}

#[test]
fn lint_returns_consistent_counts() {
    let out = analyze::lint(&inline(GOOD), false).expect("lint runs");
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
    let out = analyze::lint(&inline("Result = A == B;\n"), true).expect("lint runs");
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
    let out = analyze::lint(&inline(source), true).expect("lint runs");
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
    let out = analyze::lint(&inline("Result = 1\n"), true).expect("lint runs");
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

    // Inline bytes are request-owned and must remain capped even when the
    // logical context path matches the same exclusion. Only file reads are
    // bypassed by exclusion.
    let oversized_inline = "A".repeat(limits::MAX_REQUEST_SOURCE_BYTES as usize + 1);
    let err = analyze::lint(
        &inline_at(oversized_inline, dir.path().join("inline.generated.m1scr")),
        false,
    )
    .expect_err("excluded inline source must still respect the request cap");
    assert!(
        err.contains("exceeds") && err.contains("per-request limit"),
        "inline exclusion must not bypass the cap: {err}"
    );
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
    let first = analyze::lint(&inline(original), true).expect("lint runs");
    let Some(LintFixOutcome::Fixed { source }) = first.fix else {
        panic!("expected fixed source, got {:?}", first.fix);
    };
    assert_eq!(source, "Result = A eq B and C;\n");

    let second = analyze::lint(&inline(source), true).expect("lint runs again");
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
    let out = analyze::format(&inline(GOOD), true).expect("format runs");
    assert!(out.formatted.is_none(), "check_only must not return text");
}

#[test]
fn format_returns_text() {
    let out = analyze::format(&inline(GOOD), false).expect("format runs");
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
    let err = analyze::typecheck(&inline_at(big, "Scripts/Unsaved.m1scr"), None)
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
        analyze::lint(&inline(at), false).is_ok(),
        "source exactly at the limit must be accepted"
    );
}

#[test]
fn project_script_budget_rejects_huge_tree() {
    let dir = tempfile::tempdir().unwrap();
    let proj = write_minimal_project(dir.path());
    for i in 0..(limits::MAX_PROJECT_SCRIPTS + 1) {
        std::fs::write(dir.path().join(format!("s{i}.m1scr")), "").unwrap();
    }
    let err = loader::check_project_script_budget(&proj)
        .expect_err("a project over the script cap must be rejected");
    assert!(
        err.contains("exceeds") && err.contains("script"),
        "error should name the script limit: {err}"
    );
    let err = loader::load_project_full(&proj)
        .err()
        .expect("the authoritative loader walk must enforce the same cap");
    assert!(
        err.contains("exceeds") && err.contains("script"),
        "loader error should name the script limit: {err}"
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
fn project_load_report_makes_missing_inputs_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let project = write_minimal_project(dir.path());
    std::fs::write(dir.path().join("Test.Update.m1scr"), GOOD).unwrap();

    let loaded = loader::load_project_full(&project).expect("project loads");
    assert_eq!(loaded.report.project, project.to_string_lossy());
    assert_eq!(
        loaded.report.configuration,
        loader::ConfigurationLoad::Missing
    );
    assert_eq!(loaded.report.dbc_state, loader::DbcLoadState::NoneFound);
    assert_eq!(loaded.report.script_count, 1);
    assert!(loaded.report.loaded_dbcs.is_empty());
    assert!(loaded.report.skipped_dbcs.is_empty());
    assert!(loaded.report.skipped_scripts.is_empty());

    let json = serde_json::to_value(&loaded.report).unwrap();
    assert_eq!(json["configuration"]["state"], "missing");
    assert_eq!(json["dbc_state"], "none_found");
}

#[test]
fn project_load_report_names_loaded_and_skipped_auxiliary_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let project = write_minimal_project(dir.path());
    let config = dir.path().join("parameters.m1cfg");
    let good_dbc = dir.path().join("good.m1dbc");
    let bad_dbc = dir.path().join("bad.m1dbc");
    std::fs::write(&config, EMPTY_CONFIG).unwrap();
    std::fs::write(&good_dbc, VALID_DBC).unwrap();
    std::fs::write(&bad_dbc, "<not-closed").unwrap();

    let loaded = loader::load_project_full(&project).expect("partial project loads");
    assert_eq!(
        loaded.report.configuration,
        loader::ConfigurationLoad::Loaded {
            path: config.to_string_lossy().into_owned(),
        }
    );
    assert_eq!(loaded.report.dbc_state, loader::DbcLoadState::Partial);
    assert_eq!(
        loaded.report.loaded_dbcs,
        vec![good_dbc.to_string_lossy().into_owned()]
    );
    assert_eq!(loaded.report.skipped_dbcs.len(), 1);
    assert_eq!(
        loaded.report.skipped_dbcs[0].path,
        bad_dbc.to_string_lossy()
    );
    assert!(!loaded.report.skipped_dbcs[0].error.is_empty());
    assert!(
        loaded
            .project
            .symbols()
            .iter()
            .any(|symbol| symbol.path == "Good.Status"),
        "successfully loaded DBC data must remain in the partial model"
    );
}

#[test]
fn project_load_report_names_unreadable_scripts() {
    let dir = tempfile::tempdir().unwrap();
    let project = write_minimal_project(dir.path());
    std::fs::write(dir.path().join("readable.m1scr"), GOOD).unwrap();
    let oversized = dir.path().join("oversized.m1scr");
    let file = std::fs::File::create(&oversized).unwrap();
    file.set_len(m1_workspace::MAX_TEXT_FILE_BYTES + 1).unwrap();

    let loaded = loader::load_project_full(&project).expect("project stays usable");
    assert_eq!(loaded.report.script_count, 1);
    assert_eq!(loaded.report.skipped_scripts.len(), 1);
    assert_eq!(
        loaded.report.skipped_scripts[0].path,
        oversized.to_string_lossy()
    );
    assert!(
        loaded.report.skipped_scripts[0]
            .error
            .contains("read limit")
    );

    let checked =
        project_check::check_project(&project, &project_check::ProjectCheckOptions::default())
            .expect("project check keeps readable results");
    assert_eq!(checked.files.len(), 2);
    let skipped = checked
        .files
        .iter()
        .find(|file| file.path == oversized.to_string_lossy())
        .expect("skipped input remains associated with a file result");
    assert!(
        skipped
            .error
            .as_deref()
            .is_some_and(|error| error.contains("read limit"))
    );
    assert!(skipped.typecheck.is_none());
    assert_eq!(checked.load_report.skipped_scripts.len(), 1);
}

#[test]
fn malformed_main_project_remains_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("Project.m1prj");
    std::fs::write(&project, "<not-closed").unwrap();

    let error = loader::load_project_full(&project)
        .err()
        .expect("malformed main project must fail");
    assert!(error.contains("failed to load project"), "{error}");
    assert!(
        analyze::typecheck(&inline(GOOD), Some(&project)).is_err(),
        "the MCP analyser must surface the load failure as a tool error"
    );
}

#[test]
fn project_tools_expose_the_shared_load_report() {
    let dir = tempfile::tempdir().unwrap();
    let project = write_minimal_project(dir.path());
    std::fs::write(dir.path().join("Test.Update.m1scr"), GOOD).unwrap();

    let checked = analyze::typecheck(&inline(GOOD), Some(&project)).expect("typecheck runs");
    assert_eq!(checked.load_report.as_ref().unwrap().script_count, 1);

    let listed = symbols::list(&project, None, 200).expect("symbols run");
    assert_eq!(listed.load_report.script_count, 1);

    let inspected = can::inspect(&project, None, 200).expect("CAN inspection runs");
    assert_eq!(inspected.load_report.script_count, 1);
    let json = serde_json::to_value(inspected).unwrap();
    assert!(json.get("modules").is_some(), "CAN fields stay top-level");
    assert!(json.get("load_report").is_some());
    assert!(
        json.get("can").is_none(),
        "the wrapper must remain flattened"
    );

    let schema = serde_json::to_value(schemars::schema_for!(can::CanOutcome)).unwrap();
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"].get("modules").is_some());
    assert!(schema["properties"].get("load_report").is_some());
}

#[test]
fn malformed_auxiliary_dbc_returns_partial_can_result_with_warning_details() {
    let dir = tempfile::tempdir().unwrap();
    let project = write_minimal_project(dir.path());
    let bad_dbc = dir.path().join("bad.m1dbc");
    std::fs::write(&bad_dbc, "<not-closed").unwrap();

    let inspected = can::inspect(&project, None, 200)
        .expect("a malformed auxiliary DBC must not blank the CAN result");
    assert_eq!(
        inspected.load_report.dbc_state,
        loader::DbcLoadState::Partial
    );
    assert_eq!(inspected.load_report.skipped_dbcs.len(), 1);
    assert_eq!(
        inspected.load_report.skipped_dbcs[0].path,
        bad_dbc.to_string_lossy()
    );
    assert_eq!(inspected.can.total_messages, 0);
}

#[test]
fn can_result_names_a_syntax_error_script_omitted_from_bus_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let project = write_minimal_project(dir.path());
    std::fs::write(dir.path().join("Good.m1dbc"), VALID_DBC).unwrap();
    std::fs::write(
        dir.path().join("CAN Init.m1scr"),
        "DBC.Good.Init(1);\nlocal broken = ;\n",
    )
    .unwrap();

    let inspected = can::inspect(&project, None, 200).expect("partial CAN result is returned");

    assert_eq!(inspected.load_report.script_count, 1);
    assert!(inspected.load_report.skipped_scripts.is_empty());
    assert_eq!(inspected.can.skipped_scripts.len(), 1);
    assert_eq!(inspected.can.skipped_scripts[0].script, "CAN Init.m1scr");
    assert!(
        inspected.can.skipped_scripts[0]
            .reason
            .contains("syntax diagnostic")
    );
    let module = inspected
        .can
        .modules
        .iter()
        .find(|module| module.name == "Good")
        .expect("DBC module remains visible");
    assert!(!module.initialised, "unsafe Init calls must not bind a bus");
}

#[test]
fn completeness_reports_fully_typed_and_opaque_scripts() {
    let dir = tempfile::tempdir().unwrap();
    let project = write_minimal_project(dir.path());
    let config = dir.path().join("parameters.m1cfg");
    let dbc = dir.path().join("model.m1dbc");
    std::fs::write(&config, EMPTY_CONFIG).unwrap();
    std::fs::write(&dbc, VALID_DBC).unwrap();
    std::fs::write(dir.path().join("Complete.m1scr"), "local x = 1 + 2;\n").unwrap();
    std::fs::write(
        dir.path().join("Opaque.m1scr"),
        "local x = Engine.Speed + 1;\n",
    )
    .unwrap();

    let complete = completeness::analyze_project(&project, Some("complete"))
        .expect("complete script is analysed");
    assert_eq!(complete.scripts_total, 1);
    assert_eq!(complete.scripts_analysed, 1);
    assert_eq!(complete.scripts_with_syntax_errors, 0);
    assert_eq!(complete.scripts_skipped_deep, 0);
    assert!(complete.expressions_total > 0);
    assert_eq!(complete.expressions_typed, complete.expressions_total);
    assert_eq!(complete.typed_percent, 100.0);
    assert!(complete.cfg_loaded);
    assert!(complete.dbc_loaded);
    assert_eq!(complete.load_report.script_count, 2);
    assert_eq!(
        complete.catalogue_target,
        m1_typecheck::intrinsics::active_target()
    );
    let json = serde_json::to_value(&complete).unwrap();
    for field in [
        "scripts_total",
        "scripts_analysed",
        "scripts_with_syntax_errors",
        "scripts_skipped_deep",
        "expressions_total",
        "expressions_typed",
        "typed_percent",
        "references_total",
        "references_resolved",
        "references_opaque",
        "references_unresolved",
        "resolved_percent",
        "intrinsic_calls_total",
        "intrinsic_calls_unmodelled",
        "when_subjects_total",
        "when_subjects_incomplete",
        "cfg_loaded",
        "dbc_loaded",
        "catalogue_target",
        "load_report",
    ] {
        assert!(
            json.get(field).is_some(),
            "missing completeness field {field}"
        );
    }
    let schema =
        serde_json::to_value(schemars::schema_for!(completeness::CompletenessOutcome)).unwrap();
    assert_eq!(schema["type"], "object");

    let opaque = completeness::analyze_project(&project, Some("OPAQUE"))
        .expect("script filter is case-insensitive");
    assert_eq!(opaque.scripts_total, 1);
    assert!(opaque.references_total > 0);
    assert!(opaque.references_opaque > 0);
    assert!(opaque.expressions_typed < opaque.expressions_total);
    assert!(opaque.typed_percent < 100.0);
}

#[test]
fn completeness_distinguishes_syntax_and_depth_skips() {
    let dir = tempfile::tempdir().unwrap();
    let project = write_minimal_project(dir.path());
    std::fs::write(dir.path().join("Broken.m1scr"), "local x = ;\n").unwrap();

    let outcome =
        completeness::analyze_project(&project, Some("Broken")).expect("report is telemetry");
    assert_eq!(outcome.scripts_total, 1);
    assert_eq!(outcome.scripts_analysed, 0);
    assert_eq!(outcome.scripts_with_syntax_errors, 1);
    assert_eq!(outcome.scripts_skipped_deep, 0);
    assert_eq!(outcome.expressions_total, 0);

    let depth = m1_core::MAX_RECURSION_DEPTH + 100;
    let deep = format!("local x = {}1{};\n", "(".repeat(depth), ")".repeat(depth));
    std::fs::write(dir.path().join("Deep.m1scr"), deep).unwrap();
    let outcome = completeness::analyze_project(&project, Some("Deep"))
        .expect("deep input is skipped, not fatal");
    assert_eq!(outcome.scripts_total, 1);
    assert_eq!(outcome.scripts_analysed, 0);
    assert_eq!(outcome.scripts_with_syntax_errors, 0);
    assert_eq!(outcome.scripts_skipped_deep, 1);
    assert_eq!(outcome.expressions_total, 0);
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
    let err = analyze::typecheck(&inline(GOOD), Some(&proj))
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
    let inline = analyze::format(&inline(allman), false).unwrap();
    assert!(
        !inline.formatted.unwrap().contains(") {"),
        "inline format must use the default Allman, not the project's kr"
    );

    // A logical path discovers the same config while the source remains the
    // request buffer. The path itself need not exist and must not be created.
    let context_path = dir.path().join("unsaved.m1scr");
    let contextual = analyze::format(&inline_at(allman, context_path.clone()), false).unwrap();
    assert!(
        contextual.formatted.unwrap().contains(") {"),
        "context_path must discover the project's K&R format config"
    );
    assert!(!context_path.exists(), "format must not write context_path");
}

#[test]
fn lint_discovers_project_config_from_inline_context_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".m1lint.toml"), "ignore = [\"L004\"]\n").unwrap();
    let source = "x = a == b;\n";

    let without_context = analyze::lint(&inline(source), false).expect("default lint runs");
    assert!(
        without_context
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L004"),
        "default lint should flag ==: {:?}",
        without_context.diagnostics
    );

    let context_path = dir.path().join("unsaved.m1scr");
    let with_context = analyze::lint(&inline_at(source, context_path.clone()), false)
        .expect("contextual lint runs");
    assert!(
        !with_context
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L004"),
        "context_path must discover .m1lint.toml: {:?}",
        with_context.diagnostics
    );
    assert!(
        !context_path.exists(),
        "lint must not read or create context_path"
    );
}
