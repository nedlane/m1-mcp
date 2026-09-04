//! Black-box MCP contract tests over the compiled stdio server.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

struct StdioClient {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Receiver<Result<String, String>>,
    stdout_thread: JoinHandle<()>,
    stderr_thread: JoinHandle<String>,
}

impl StdioClient {
    fn start(log_filter: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_m1-mcp"))
            .env("RUST_LOG", log_filter)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start m1-mcp");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        let stderr = child.stderr.take().expect("child stderr");
        let (tx, rx) = mpsc::channel();
        let stdout_thread = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let item = line.map_err(|error| error.to_string());
                if tx.send(item).is_err() {
                    return;
                }
            }
        });
        let stderr_thread = std::thread::spawn(move || {
            let mut output = String::new();
            BufReader::new(stderr)
                .read_to_string(&mut output)
                .expect("read server stderr");
            output
        });
        Self {
            child,
            stdin: Some(stdin),
            stdout: rx,
            stdout_thread,
            stderr_thread,
        }
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().expect("server stdin remains open");
        serde_json::to_writer(&mut *stdin, &message).expect("write request");
        stdin.write_all(b"\n").expect("terminate request");
        stdin.flush().expect("flush request");
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = self
                .stdout
                .recv_timeout(remaining)
                .unwrap_or_else(|error| panic!("response {id} timed out: {error}"))
                .unwrap_or_else(|error| panic!("read response {id}: {error}"));
            let frame: Value = serde_json::from_str(&line)
                .unwrap_or_else(|error| panic!("non-JSON stdout frame `{line}`: {error}"));
            assert_eq!(frame["jsonrpc"], "2.0", "invalid stdout frame: {frame}");
            if frame.get("id") == Some(&json!(id)) {
                return frame;
            }
        }
    }

    fn initialize(&mut self) {
        let response = self.request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "m1-mcp-contract", "version": "1"},
            }),
        );
        assert_eq!(response["result"]["serverInfo"]["name"], "m1-mcp");
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }));
    }

    fn call(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.request(
            id,
            "tools/call",
            json!({"name": name, "arguments": arguments}),
        )
    }

    fn finish(mut self) -> String {
        drop(self.stdin.take());
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("poll m1-mcp") {
                break status;
            }
            if Instant::now() >= deadline {
                self.child.kill().expect("kill hung m1-mcp");
                panic!("m1-mcp did not exit after stdin closed");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success(), "m1-mcp exited with {status}");
        self.stdout_thread.join().expect("stdout reader joins");
        for line in self.stdout.try_iter() {
            let line = line.expect("read trailing stdout");
            serde_json::from_str::<Value>(&line)
                .unwrap_or_else(|error| panic!("non-JSON trailing stdout `{line}`: {error}"));
        }
        self.stderr_thread.join().expect("stderr reader joins")
    }
}

fn structured(response: &Value) -> Value {
    assert_eq!(
        response["result"]["isError"], false,
        "tool failed: {response}"
    );
    response["result"]["structuredContent"]
        .as_object()
        .unwrap_or_else(|| panic!("missing structured result: {response}"));
    response["result"]["structuredContent"].clone()
}

fn write_project_fixture(dir: &Path) -> PathBuf {
    let project = dir.join("Project.m1prj");
    std::fs::write(
        &project,
        r#"<?xml version="1.0"?>
<MoTeCM1BuildSession><Project Name="Protocol" TargetHardware="ecu120">
<ComponentStream><List>
<Component Classname="BuiltIn.GroupCompound" Name="Root.Test"/>
<Component Classname="BuiltIn.FuncUser" Filename="Test.Update.m1scr" Name="Root.Test.Update"/>
</List></ComponentStream></Project></MoTeCM1BuildSession>
"#,
    )
    .unwrap();
    std::fs::write(dir.join("Test.Update.m1scr"), "DBC.Good.Init(1);\n").unwrap();
    std::fs::write(
        dir.join("Good.m1dbc"),
        r#"<?xml version="1.0"?>
<DBC><ComponentStream><List>
<Component Classname="BuiltIn.CAN.DBC" Name="Good"/>
<Component Classname="BuiltIn.CAN.Message" Name="Good.Status">
<Props CANId="100" DLC="8" Transmit="RX"/>
</Component>
</List></ComponentStream></DBC>
"#,
    )
    .unwrap();
    project
}

#[test]
fn stdio_protocol_exposes_tools_errors_and_clean_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let project = write_project_fixture(dir.path());
    let mut client = StdioClient::start("debug");
    client.initialize();

    let listed = client.request(2, "tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().expect("tools array");
    let mut names = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "m1_can",
            "m1_check_project",
            "m1_completeness",
            "m1_doc_lookup",
            "m1_doc_search",
            "m1_format",
            "m1_lint",
            "m1_lint_rule",
            "m1_symbols",
            "m1_typecheck",
        ]
    );
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
        assert_eq!(tool["outputSchema"]["type"], "object", "{tool}");
    }

    let docs = structured(&client.call(3, "m1_doc_search", json!({"query": "Absolute"})));
    assert!(docs["count"].as_u64().is_some_and(|count| count > 0));
    let analysis =
        structured(&client.call(4, "m1_typecheck", json!({"source": "local value = 1;\n"})));
    assert!(analysis["diagnostics"].is_array());
    let symbols = structured(&client.call(
        5,
        "m1_symbols",
        json!({"project": project.display().to_string()}),
    ));
    assert!(symbols["total"].as_u64().is_some_and(|total| total > 0));
    let can = structured(&client.call(
        6,
        "m1_can",
        json!({"project": project.display().to_string()}),
    ));
    assert_eq!(can["modules"][0]["name"], "Good");

    let missing = client.call(7, "m1_typecheck", json!({}));
    assert_eq!(missing["error"]["code"], -32602);
    assert!(
        missing["error"]["message"]
            .as_str()
            .unwrap()
            .contains("either")
    );
    let exclusive = client.call(
        8,
        "m1_typecheck",
        json!({"source": "A = 1;\n", "path": "A.m1scr"}),
    );
    assert_eq!(exclusive["error"]["code"], -32602);
    assert!(
        exclusive["error"]["message"]
            .as_str()
            .unwrap()
            .contains("exactly one")
    );

    let stderr = client.finish();
    assert!(
        stderr.contains("starting m1-mcp"),
        "logging missing: {stderr}"
    );
}

fn find_project(start: &Path) -> Option<PathBuf> {
    if start.is_file()
        && start
            .file_name()
            .is_some_and(|name| name == "Project.m1prj")
    {
        return Some(start.to_path_buf());
    }
    start
        .ancestors()
        .map(|dir| dir.join("Project.m1prj"))
        .find(|path| path.is_file())
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, (u64, Option<SystemTime>)> {
    fn visit(path: &Path, snapshot: &mut BTreeMap<PathBuf, (u64, Option<SystemTime>)>) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, snapshot);
            } else if let Ok(metadata) = entry.metadata() {
                snapshot.insert(path, (metadata.len(), metadata.modified().ok()));
            }
        }
    }
    let mut snapshot = BTreeMap::new();
    visit(root, &mut snapshot);
    snapshot
}

#[test]
fn optional_corpus_smoke_is_read_only_across_project_tools() {
    let Some(corpus) = std::env::var_os("M1_CORPUS_PATH").map(PathBuf::from) else {
        eprintln!("M1_CORPUS_PATH not set; skipping read-only MCP corpus smoke test");
        return;
    };
    let project = std::env::var_os("M1_PROJECT")
        .map(PathBuf::from)
        .or_else(|| find_project(&corpus))
        .unwrap_or_else(|| panic!("no Project.m1prj above {}", corpus.display()));
    let root = project.parent().expect("project directory");
    let source = m1_workspace::find_scripts(&corpus)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no scripts under {}", corpus.display()));
    let before = snapshot_tree(root);

    let mut client = StdioClient::start("off");
    client.initialize();
    structured(&client.call(
        2,
        "m1_symbols",
        json!({"project": project.display().to_string(), "limit": 1}),
    ));
    structured(&client.call(
        3,
        "m1_typecheck",
        json!({
            "path": source.display().to_string(),
            "project": project.display().to_string(),
        }),
    ));
    structured(&client.call(
        4,
        "m1_can",
        json!({"project": project.display().to_string(), "limit": 1}),
    ));
    client.finish();

    assert_eq!(
        snapshot_tree(root),
        before,
        "MCP corpus calls changed files"
    );
}
