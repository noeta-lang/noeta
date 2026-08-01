//! End-to-end tests for the lean production runner: compile a tiny program to a `.noeb` bundle
//! through the salsa pipeline, then run the standalone `noeta-runner` binary over it as a host
//! process. Mirrors `noeta-wasm-runner`'s harness — the difference is only the host (real, not WASI).

use std::process::Command;

use noeta_span::{Source, SourceId};

/// Compile `text` through the salsa pipeline and wrap it as a `.noeb` bundle.
fn build_bundle(text: &str) -> Vec<u8> {
    // Own assembling driver (audit-6 F2): seed the std units before the front-end runs.
    noeta_stdlib::registry::default_seeded();
    let db = noeta_db::LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "test.noe", text);
    let src = noeta_db::source_program(&db, &source, noeta_db::Edition::DEFAULT);
    let module = noeta_db::bytecode(&db, src)
        .0
        .as_ref()
        .expect("test program compiles");
    noeta_bundle::write(module)
}

/// One test's bundle, in a fixture directory of its own.
///
/// The *directory* is the point, not the file name. The runner dispatches by content, so any file
/// that is not a bundle is compiled as `.noe` source — and the loader then links the entry's
/// sibling directory as the project. A fixture written straight into the shared system temp dir
/// therefore drags every stray `.noe` file any other process left in `/tmp` into this test's
/// program, and the test fails with a diagnostic about somebody else's code:
/// `[E0019] no module para.aether in this project`, whose `help` listed a sibling agent session's
/// scratch files. `TempPath` carries its directory's guard, so the tree lives exactly as long as
/// the path does and holds nothing but what this test put there.
fn temp_bundle(name: &str, bytes: &[u8]) -> noeta_test_temp::TempPath {
    let dir = noeta_test_temp::TempDir::new("runner");
    std::fs::write(dir.join(name), bytes).expect("write bundle");
    dir.into_child(name)
}

fn runner() -> Command {
    Command::new(env!("CARGO_BIN_EXE_noeta-runner"))
}

#[test]
fn runs_a_bundle_on_the_real_host() {
    let path = temp_bundle("hello.noeb", &build_bundle("echo \"hello\";"));
    let out = runner().arg(path.path()).output().expect("runner runs");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello\n");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn passes_the_program_argument_vector() {
    // `args.all()` is `[<bundle>, <pass-through…>]` — the program sees its own argv, argv[0] first.
    let path = temp_bundle(
        "args.noeb",
        &build_bundle("use std.{args};\nfor a in args.all() { echo a; }"),
    );
    let out = runner()
        .arg(path.path())
        .arg("alpha")
        .arg("beta")
        .output()
        .expect("runner runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("alpha"), "stdout: {stdout}");
    assert!(stdout.contains("beta"), "stdout: {stdout}");
}

#[test]
fn runs_a_noe_source_file_directly() {
    // PHP-style: point the runner at `.noe` source; it compiles on the fly (no pre-built bundle).
    let dir = noeta_test_temp::TempDir::new("runner-src");
    let src = dir.join("app.noe");
    std::fs::write(&src, "echo \"from source\";").expect("write source");
    let out = runner().arg(&src).output().expect("runner runs");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "from source\n",
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn reports_a_compile_error_in_source() {
    // A compile failure renders diagnostics and exits non-zero — the CLI's exact pipeline. An
    // undefined name is a guaranteed compile error (E0005).
    let dir = noeta_test_temp::TempDir::new("runner-err");
    let src = dir.join("bad.noe");
    std::fs::write(&src, "echo undefined_name_xyz;").expect("write source");
    let out = runner().arg(&src).output().expect("runner runs");
    assert_ne!(out.status.code(), Some(0));
    assert!(
        !String::from_utf8_lossy(&out.stderr).is_empty(),
        "expected a diagnostic on stderr"
    );
}

#[test]
fn renders_an_abort_with_its_traceback() {
    let text = "fn boom(): int {\n  panic(\"kaboom\");\n}\necho boom();";
    let path = temp_bundle("abort.noeb", &build_bundle(text));
    let out = runner().arg(path.path()).output().expect("runner runs");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr.contains("kaboom"), "stderr: {stderr}");
}

#[test]
fn writes_the_programs_own_stderr_stream() {
    // `std.io`'s `err`/`errln` are observable program output, buffered into `RunResult.stderr`
    // exactly as `echo` is buffered into `stdout`. This is the tail that IS the chokepoint
    // (`run_compiled_module` → `RunTail`), so the property is pinned here at its source: a tail
    // that stopped calling it and hand-wrote the epilogue again would have to remember the stream,
    // which four of seven copies did not (plans/parallel-path-audit.md row 1).
    let text = "use std.io\necho \"to stdout\"\nio.errln(\"to stderr\")\n";
    let path = temp_bundle("streams.noeb", &build_bundle(text));
    let out = runner().arg(path.path()).output().expect("runner runs");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "to stdout\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "to stderr\n");
}

#[test]
fn program_stderr_precedes_the_diagnostics_and_the_traceback() {
    // The canonical order: the program's own stderr, then the run's diagnostics, then the abort
    // traceback. A program that reports progress on stderr and then aborts must not have its
    // report appear *after* the failure that followed it.
    let text = "use std.io\nfn boom(): int {\n  panic(\"kaboom\");\n}\nio.errln(\"step one\")\necho boom();";
    let path = temp_bundle("order.noeb", &build_bundle(text));
    let out = runner().arg(path.path()).output().expect("runner runs");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0));
    let program = stderr.find("step one").expect("program stderr is written");
    let diagnostic = stderr.find("kaboom").expect("the diagnostic is rendered");
    let traceback = stderr
        .find("stack trace")
        .expect("the traceback is rendered");
    assert!(program < diagnostic, "stderr out of order: {stderr:?}");
    assert!(diagnostic < traceback, "stderr out of order: {stderr:?}");
}

#[test]
fn an_exit_code_above_255_is_reported_as_a_failure_not_truncated() {
    // A process status is a `u8`, so an out-of-range code has to be *converted*. `as u8` truncates:
    // 256 becomes 0 — a failure reported as a success. `RunTail::status` clamps to 1 instead, and
    // this is the one conversion in the tree.
    let path = temp_bundle("big_exit.noeb", &build_bundle("use std.os\nos.exit(256)\n"));
    let out = runner().arg(path.path()).output().expect("runner runs");
    assert_eq!(
        out.status.code(),
        Some(1),
        "256 must not truncate to 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn refuses_a_missing_file() {
    let out = runner()
        .arg("/nonexistent/app.noeb")
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read"));
}

#[test]
fn treats_a_non_bundle_file_as_source() {
    // Dispatch is by content (bundle magic), not extension — a non-bundle file is compiled as `.noe`
    // source, exactly as `noeta run` sniffs it. Valid source runs; garbage would be a compile error.
    let path = temp_bundle("plain.txt", b"echo \"i am source\";");
    let out = runner().arg(path.path()).output().expect("runs");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "i am source\n",
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn a_fixture_is_alone_in_its_directory() {
    // The isolation itself, asserted rather than assumed. What makes the source tests above robust
    // is not the unique file *name* but the private *directory* — the loader links an entry's
    // siblings as its project, so a fixture back under the shared temp root would make this suite's
    // result depend on whatever else happens to be in `/tmp`. That is exactly how it failed.
    let path = temp_bundle("only.noeb", &build_bundle("echo 1;"));
    let siblings: Vec<_> = std::fs::read_dir(path.dir())
        .expect("the fixture directory exists")
        .map(|entry| entry.expect("read the fixture directory").file_name())
        .collect();
    assert_eq!(
        siblings,
        vec![std::ffi::OsString::from("only.noeb")],
        "a fixture shares its directory with nothing"
    );
    assert_ne!(
        path.dir(),
        std::env::temp_dir(),
        "fixtures never sit directly in the shared system temp dir"
    );
}

#[test]
fn the_dependency_graph_links_no_dev_tooling() {
    // The dev-deps security invariant, regression-guarded: in the runner's OWN crate graph (isolated
    // `-p` resolution, which is what `build --exe`/the composer use), NO L3 dev tooling is linked —
    // no fmt/LSP/DAP/MCP, no formatter parsers (malva). `-e features` reflects the real feature
    // resolution, so the `fmt-config` gate on `noeta-pm` is honoured (workspace-unified builds would
    // turn it on — the artifact must never be built that way). Best-effort: skip if cargo can't run,
    // so an infra gap never false-fails; a real regression (a forbidden crate appearing) fails loud.
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let Ok(output) = std::process::Command::new(env!("CARGO"))
        .current_dir(&workspace)
        .args(["tree", "-p", "noeta-runner", "-e", "features"])
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let tree = String::from_utf8_lossy(&output.stdout);
    for forbidden in [
        "noeta-fmt v",
        "noeta-lsp v",
        "noeta-dap v",
        "noeta-mcp v",
        "noeta-prof v",
        // The html/css tier-body formatters are no longer toolchain crates — they arrive as a
        // `package.dev-native` dependency, composed formatter-only. `malva` (the CSS formatter's
        // heavy backend) must still never reach the lean runtime, dev-native or not.
        "malva v",
    ] {
        assert!(
            !tree.contains(forbidden),
            "the lean runtime links dev tooling (`{}`) — a dev-deps security regression. \
             Something added an L3 dependency or un-gated a dev capability:\n{tree}",
            forbidden.trim_end_matches(" v")
        );
    }
}

#[test]
fn no_arguments_prints_usage() {
    let out = runner().output().expect("runs");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
}
