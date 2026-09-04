//! Optional parity check over each available read-only reference corpus.
//!
//! Set `M1_PROJECT` and `M1_CORPUS_PATH` to test an explicit corpus. Without
//! overrides, the test checks the conventional sibling EV-M1 and AV-M1 paths
//! when present and skips cleanly when neither corpus is available.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use m1_mcp::analyze::{self, DiagnosticScope, Input};
use m1_mcp::loader;
use m1_typecheck::project_check::{self, ProjectCheckOptions};

fn explicit_corpus() -> Option<(PathBuf, PathBuf)> {
    let project = std::env::var_os("M1_PROJECT").map(PathBuf::from)?;
    let scripts = std::env::var_os("M1_CORPUS_PATH").map(PathBuf::from)?;
    Some((project, scripts))
}

fn available_corpora() -> Vec<(PathBuf, PathBuf)> {
    if let Some(corpus) = explicit_corpus() {
        return vec![corpus];
    }
    [
        (
            "../m1-example/UQR-EV/01.00/Project.m1prj",
            "../m1-example/UQR-EV/01.00/Scripts",
        ),
        ("../AV-M1/UQR-AV/01.00/Project.m1prj", "../AV-M1/UQR-AV"),
    ]
    .into_iter()
    .map(|(project, scripts)| {
        (
            Path::new(env!("CARGO_MANIFEST_DIR")).join(project),
            Path::new(env!("CARGO_MANIFEST_DIR")).join(scripts),
        )
    })
    .filter(|(project, scripts)| project.is_file() && scripts.is_dir())
    .collect()
}

fn counts<'a>(codes: impl IntoIterator<Item = &'a str>) -> BTreeMap<String, usize> {
    let mut result = BTreeMap::new();
    for code in codes {
        *result.entry(code.to_string()).or_default() += 1;
    }
    result
}

#[test]
fn mcp_and_cli_pipeline_have_equal_project_code_counts_on_available_corpora() {
    let corpora = available_corpora();
    if corpora.is_empty() {
        eprintln!("reference corpora absent; skipping");
        return;
    }

    for (project_path, scripts_dir) in corpora {
        assert!(project_path.is_file(), "missing {}", project_path.display());
        assert!(scripts_dir.is_dir(), "missing {}", scripts_dir.display());
        let source_path = m1_workspace::find_scripts(&scripts_dir)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("no scripts under {}", scripts_dir.display()));

        let mcp = analyze::typecheck(&Input::Path(source_path), Some(&project_path))
            .unwrap_or_else(|error| panic!("MCP {}: {error}", project_path.display()));
        let mcp_counts = counts(
            mcp.diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.scope == DiagnosticScope::Project)
                .map(|diagnostic| diagnostic.code.as_str()),
        );

        let mut loaded = loader::load_project_full(&project_path)
            .unwrap_or_else(|error| panic!("CLI model {}: {error}", project_path.display()));
        let options = ProjectCheckOptions::discover(project_path.parent());
        let cli = project_check::check(Some(&mut loaded.project), &loaded.scripts, &[], &options);
        let cli_counts = counts(
            cli.project_diagnostics
                .iter()
                .map(|diagnostic| diagnostic.code.as_str()),
        );

        assert_eq!(mcp_counts, cli_counts, "corpus {}", project_path.display());
    }
}
