//! `noeta mcp` driven the way a real client drives it: a spawned process, JSON-RPC over stdio.
//!
//! Everything else that tests the MCP calls its tool functions in-process (`crates/noeta-mcp`'s unit
//! tests) or over an in-memory duplex inside one runtime. Neither can see the defect this module
//! exists for: the server's *runtime threads* were 2 MiB (tokio's default), so a file-based tool over
//! an ordinary real-world module overflowed a worker's stack and **aborted the whole process** —
//! killing the client's session mid-request. Only a real `noeta mcp` process, over real pipes, with a
//! real file on disk, exercises that.

use crate::support::*;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdout, Stdio};

/// A file with the shape that broke the server: several modules of ordinary, moderately nested code.
/// Nesting stays well inside the parser's inline limit — that is the whole point, since it was
/// *unremarkable* code that overflowed — but deep and voluminous enough that the whole front end
/// runs over it.
fn realistic_workspace(name: &str) -> PathBuf {
    let dir = temp_root().join(format!("noeta_cli_test_{name}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");

    let mut sibling = String::from("namespace app.calc;\n");
    for i in 0..40 {
        sibling.push_str(&format!(
            "pub fn step{i}(a: int, b: int): int {{\n\
             \x20 if a > b {{\n\
             \x20   for x in [1, 2, 3] {{\n\
             \x20     if x > b {{\n\
             \x20       if a > 0 {{\n\
             \x20         return a + b + x + {i}\n\
             \x20       }}\n\
             \x20     }}\n\
             \x20   }}\n\
             \x20 }}\n\
             \x20 return {i}\n\
             }}\n"
        ));
    }
    std::fs::write(dir.join("calc.noe"), &sibling).expect("write sibling module");

    let mut entry = String::from(
        "use app.calc.{step0, step1}\n@attribute(Function)\nstruct Tagged { note: string }\n",
    );
    for i in 0..40 {
        entry.push_str(&format!(
            "#[Tagged(\"n{i}\")]\n\
             fn local{i}(a: int): int {{\n\
             \x20 if a > 0 {{\n\
             \x20   for x in [1, 2] {{\n\
             \x20     if x > 0 {{\n\
             \x20       if a > x {{\n\
             \x20         return step0(a, x) + step1(a, x) + {i}\n\
             \x20       }}\n\
             \x20     }}\n\
             \x20   }}\n\
             \x20 }}\n\
             \x20 return 0\n\
             }}\n"
        ));
    }
    entry.push_str("echo local0(3)\n");
    let path = dir.join("main.noe");
    std::fs::write(&path, &entry).expect("write entry module");
    path
}

/// A live `noeta mcp` process with its stdio pipes and a monotonically increasing request id.
struct Session {
    child: Child,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Session {
    /// Spawn the server and complete the `initialize` handshake.
    fn start() -> Session {
        let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("noeta"))
            .arg("mcp")
            .env(
                "NOETA_CACHE_DIR",
                concat!(env!("CARGO_TARGET_TMPDIR"), "/noeta-cache"),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn `noeta mcp`");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut session = Session {
            child,
            stdout,
            next_id: 1,
        };
        let init = session.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "cli-test", "version": "0" },
            }),
        );
        assert!(
            init.get("result").is_some(),
            "initialize should succeed: {init}"
        );
        session.notify("notifications/initialized");
        session
    }

    fn send(&mut self, message: &serde_json::Value) {
        let stdin = self.child.stdin.as_mut().expect("piped stdin");
        writeln!(stdin, "{message}").expect("write to the server");
        stdin.flush().expect("flush the server's stdin");
    }

    fn notify(&mut self, method: &str) {
        let message = serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": {} });
        self.send(&message);
    }

    /// Send a request and read its response. A **missing** response is the failure this module is
    /// about: the server died (or the task carrying the request did), so the client waits forever.
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let message =
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send(&message);
        let mut line = String::new();
        let read = self
            .stdout
            .read_line(&mut line)
            .expect("read from the server");
        assert_ne!(
            read, 0,
            "the server closed stdout without answering `{method}` — it died mid-request"
        );
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("malformed response {line:?}: {e}"))
    }

    /// Call one tool over the wire and return its `result`, asserting it is not an error.
    fn call_tool(&mut self, tool: &str, file: &PathBuf) -> serde_json::Value {
        let response = self.request(
            "tools/call",
            serde_json::json!({ "name": tool, "arguments": { "file": file } }),
        );
        assert!(
            response.get("error").is_none(),
            "`{tool}` returned an error: {response}"
        );
        response
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("`{tool}` returned neither result nor error: {response}"))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_file_tools_answer_over_stdio_and_the_session_survives() {
    // Every file-based tool, in one session, over a realistic multi-module workspace. Each `call_tool`
    // asserts a response came back at all — which is what the 2 MiB worker stack made impossible:
    // the first call aborted the process, so the remaining calls read EOF.
    let file = realistic_workspace("mcp_stdio_file_tools");
    let mut session = Session::start();
    for tool in ["check", "symbols", "module_graph", "reflect"] {
        let result = session.call_tool(tool, &file);
        assert!(
            result.get("content").is_some() || result.get("structuredContent").is_some(),
            "`{tool}` should answer with content: {result}"
        );
    }
    // Still alive after all four, and still answering: a session an agent can keep using.
    let tools = session.request("tools/list", serde_json::json!({}));
    assert!(
        tools["result"]["tools"]
            .as_array()
            .is_some_and(|t| !t.is_empty()),
        "the session should still serve tools/list: {tools}"
    );
}

#[test]
fn mcp_module_graph_covers_a_project_with_dependencies() {
    // `module_graph` paired the workspace's *member* inputs with the whole source list — members
    // plus every dependency package's modules — and indexed past the end, panicking on any project
    // with a `noeta.toml` dependency. A panic no longer kills the request either way (it comes back
    // as a JSON-RPC error), but it must not happen at all.
    let root = temp_root().join("noeta_cli_test_mcp_module_graph_deps");
    let _ = std::fs::remove_dir_all(&root);
    let dep = root.join("dep");
    let app = root.join("app");
    std::fs::create_dir_all(&dep).expect("create dep dir");
    std::fs::create_dir_all(&app).expect("create app dir");
    std::fs::write(
        dep.join("noeta.toml"),
        "[package]\nname = \"acme/dep\"\nversion = \"0.1.0\"\n",
    )
    .expect("write dep manifest");
    std::fs::write(
        dep.join("dep.noe"),
        "namespace dep;\npub fn twice(a: int): int { return a * 2 }\n",
    )
    .expect("write dep module");
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n[dependencies]\ndep = { path = \"../dep\" }\n",
    )
    .expect("write app manifest");
    std::fs::write(
        app.join("helper.noe"),
        "namespace app.helper;\npub fn one(): int { return 1 }\n",
    )
    .expect("write app sibling");
    let entry = app.join("main.noe");
    std::fs::write(
        &entry,
        "use dep.{twice}\nuse app.helper.{one}\necho twice(one())\n",
    )
    .expect("write app entry");

    let mut session = Session::start();
    let result = session.call_tool("module_graph", &entry);
    let text = result.to_string();
    assert!(
        text.contains("main.noe") && text.contains("helper.noe"),
        "the graph should carry both member modules: {text}"
    );
}
