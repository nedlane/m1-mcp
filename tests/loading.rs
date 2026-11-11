use m1_mcp::loader;
use std::path::Path;

fn project(root: &Path) -> std::path::PathBuf {
    let path = root.join("Project.m1prj");
    std::fs::write(&path, "<Project><ComponentStream><List><Component Classname=\"BuiltIn.FuncUser\" Name=\"Root.Update\" Filename=\"Update.m1scr\"/></List></ComponentStream></Project>").unwrap();
    path
}

#[test]
fn duplicate_script_basenames_are_explicitly_ambiguous() {
    let dir = tempfile::tempdir().unwrap();
    let project = project(dir.path());
    for sub in ["a", "b"] {
        std::fs::create_dir(dir.path().join(sub)).unwrap();
    }
    let a = dir.path().join("a/Update.m1scr");
    let b = dir.path().join("b/Update.m1scr");
    std::fs::write(&a, "Out = 1;\n").unwrap();
    std::fs::write(&b, "Out = true;\n").unwrap();
    let error = loader::load_project_full_with_inline(&project, Some((&a, "Out = 99;\n")))
        .err()
        .expect("basename-keyed project analyses cannot distinguish these files");
    assert!(error.contains("ambiguous script filename"), "{error}");
    assert!(
        error.contains(&a.display().to_string()) && error.contains(&b.display().to_string()),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(a).unwrap(), "Out = 1;\n");
    assert_eq!(std::fs::read_to_string(b).unwrap(), "Out = true;\n");
}

#[test]
fn unsaved_path_cannot_replace_a_different_file_with_the_same_name() {
    let dir = tempfile::tempdir().unwrap();
    let project = project(dir.path());
    let existing = dir.path().join("Update.m1scr");
    std::fs::write(&existing, "Out = true;\n").unwrap();
    let unsaved = dir.path().join("new/Update.m1scr");
    let error = loader::load_project_full_with_inline(&project, Some((&unsaved, "Out = 99;\n")))
        .err()
        .expect("unsaved basename collision must be reported");
    assert!(error.contains("ambiguous script filename"), "{error}");
    assert!(!unsaved.exists());
    assert_eq!(std::fs::read_to_string(existing).unwrap(), "Out = true;\n");
}

#[test]
fn normalized_context_replaces_only_its_file_and_unsaved_paths_are_included() {
    let dir = tempfile::tempdir().unwrap();
    let project = project(dir.path());
    let existing = dir.path().join("Update.m1scr");
    let other = dir.path().join("Other.m1scr");
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(&existing, "Out = 1;\n").unwrap();
    std::fs::write(&other, "Out = true;\n").unwrap();
    let context = dir.path().join("sub/.././Update.m1scr");
    let loaded =
        loader::load_project_full_with_inline(&project, Some((&context, "Out = 99;\n"))).unwrap();
    let snapshot: Vec<_> = loaded
        .script_paths
        .iter()
        .zip(&loaded.scripts)
        .map(|(p, s)| (p.as_path(), s.cst.source()))
        .collect();
    assert!(snapshot.contains(&(existing.as_path(), "Out = 99;\n")));
    assert!(snapshot.contains(&(other.as_path(), "Out = true;\n")));
    let unsaved = dir.path().join("new/Unsaved.m1scr");
    let loaded =
        loader::load_project_full_with_inline(&project, Some((&unsaved, "Out = 2;\n"))).unwrap();
    assert_eq!(loaded.report.script_count, 3);
    assert!(loaded.script_paths.contains(&unsaved));
    assert!(!unsaved.exists());
    assert_eq!(std::fs::read_to_string(existing).unwrap(), "Out = 1;\n");
}

#[cfg(unix)]
#[test]
fn unreadable_subtrees_cannot_produce_complete_load_reports() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let project = project(dir.path());
    let hidden = dir.path().join("Hidden");
    std::fs::create_dir(&hidden).unwrap();
    std::fs::write(hidden.join("Bad.m1scr"), "broken {").unwrap();
    std::fs::write(hidden.join("Bad.m1dbc"), "<broken").unwrap();
    std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o0)).unwrap();
    let denied = std::fs::read_dir(&hidden).is_err();
    let preflight = loader::check_project_script_budget(&project);
    let loaded = loader::load_project_full(&project);
    std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o700)).unwrap();
    if !denied {
        eprintln!("permission test requires an unprivileged user");
        return;
    }
    assert!(preflight.unwrap_err().contains("Hidden"));
    let error = loaded.err().expect("incomplete discovery must fail");
    assert!(error.contains("Hidden"), "{error}");
    let loaded = loader::load_project_full(&project).unwrap();
    assert_eq!(loaded.report.script_count, 1);
    assert_eq!(loaded.report.skipped_dbcs.len(), 1);
}

#[test]
fn depth_truncation_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let project = project(dir.path());
    let mut deep = dir.path().to_path_buf();
    for _ in 0..65 {
        deep.push("d");
    }
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("Update.m1scr"), "Out = 1;\n").unwrap();
    assert!(
        loader::check_project_script_budget(&project)
            .unwrap_err()
            .contains("depth")
    );
    assert!(
        loader::load_project_full(&project)
            .err()
            .expect("depth must fail")
            .contains("depth")
    );
}

#[test]
fn unsaved_script_counts_toward_project_budget() {
    let dir = tempfile::tempdir().unwrap();
    let project = project(dir.path());
    for i in 0..m1_mcp::limits::MAX_PROJECT_SCRIPTS {
        std::fs::write(dir.path().join(format!("Script{i}.m1scr")), "Out = 1;\n").unwrap();
    }
    let unsaved = dir.path().join("Unsaved.m1scr");
    let error = loader::load_project_full_with_inline(&project, Some((&unsaved, "Out = 1;\n")))
        .err()
        .expect("the request cannot add a script beyond the parse budget");
    assert!(error.contains("script per-request limit"), "{error}");
}

#[test]
fn independent_checks_keep_duplicate_paths_but_semantic_checks_refuse_them() {
    use m1_mcp::project_check::{CheckKind, ProjectCheckOptions, check_project};
    let dir = tempfile::tempdir().unwrap();
    let project = project(dir.path());
    for (sub, source) in [("a", "Out=1;"), ("b", "Out=true;")] {
        std::fs::create_dir(dir.path().join(sub)).unwrap();
        std::fs::write(dir.path().join(sub).join("Update.m1scr"), source).unwrap();
    }
    for check in [CheckKind::Lint, CheckKind::Format] {
        let result = check_project(
            &project,
            &ProjectCheckOptions {
                checks: vec![check],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result.files.len(), 2);
        for (file, sub) in result.files.iter().zip(["a", "b"]) {
            assert_eq!(
                Path::new(&file.path),
                dir.path().join(sub).join("Update.m1scr")
            );
            assert!(file.error.is_none());
            assert_eq!(file.lint.is_some(), check == CheckKind::Lint);
            assert_eq!(file.format.is_some(), check == CheckKind::Format);
        }
    }
    assert!(m1_mcp::symbols::list(&project, None, 0).is_ok());
    let error = check_project(
        &project,
        &ProjectCheckOptions {
            filter: Some("a/".into()),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(error.contains("ambiguous script filename"), "{error}");
}

#[cfg(unix)]
#[test]
fn skipped_non_utf8_names_still_count_toward_unsaved_script_budget() {
    use std::os::unix::ffi::OsStringExt;
    let dir = tempfile::tempdir().unwrap();
    let project = project(dir.path());
    for i in 0..m1_mcp::limits::MAX_PROJECT_SCRIPTS - 1 {
        std::fs::write(dir.path().join(format!("Script{i}.m1scr")), "").unwrap();
    }
    std::fs::write(
        dir.path()
            .join(std::ffi::OsString::from_vec(b"bad\xff.m1scr".to_vec())),
        "",
    )
    .unwrap();
    let unsaved = dir.path().join("Unsaved.m1scr");
    let error = loader::load_project_full_with_inline(&project, Some((&unsaved, "Out = 1;")))
        .err()
        .expect("skipped names still consume a discovered-input slot");
    assert!(error.contains("2001"), "{error}");
    let existing = dir.path().join("Script0.m1scr");
    let loaded =
        loader::load_project_full_with_inline(&project, Some((&existing, "Out = 1;"))).unwrap();
    assert_eq!(
        loaded.report.script_count,
        m1_mcp::limits::MAX_PROJECT_SCRIPTS - 1
    );
    assert_eq!(loaded.report.skipped_scripts.len(), 1);
}
