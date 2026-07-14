//! End-to-end tests for the lean production runner: compile a tiny program to a `.noeb` bundle
//! through the salsa pipeline, then run the standalone `noeta-runner` binary over it as a host
//! process. Mirrors `noeta-wasm-runner`'s harness — the difference is only the host (real, not WASI).

use std::process::Command;

use noeta_span::{Source, SourceId};

/// Compile `text` through the salsa pipeline and wrap it as a `.noeb` bundle.
fn build_bundle(text: &str) -> Vec<u8> {
    let db = noeta_db::LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "test.noe", text);
    let src = noeta_db::source_program(&db, &source);
    let module = noeta_db::bytecode(&db, src)
        .0
        .as_ref()
        .expect("test program compiles");
    noeta_bundle::write(module)
}

/// A fresh temp path for one test's bundle.
fn temp_bundle(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("noeta-runner-{}-{name}", std::process::id()));
    std::fs::write(&path, bytes).expect("write bundle");
    path
}

fn runner() -> Command {
    Command::new(env!("CARGO_BIN_EXE_noeta-runner"))
}

#[test]
fn runs_a_bundle_on_the_real_host() {
    let path = temp_bundle("hello.noeb", &build_bundle("echo \"hello\";"));
    let out = runner().arg(&path).output().expect("runner runs");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello\n");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::remove_file(path).ok();
}

#[test]
fn passes_the_program_argument_vector() {
    // `args.all()` is `[<bundle>, <pass-through…>]` — the program sees its own argv, argv[0] first.
    let path = temp_bundle(
        "args.noeb",
        &build_bundle("use std.{args};\nfor a in args.all() { echo a; }"),
    );
    let out = runner()
        .arg(&path)
        .arg("alpha")
        .arg("beta")
        .output()
        .expect("runner runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("alpha"), "stdout: {stdout}");
    assert!(stdout.contains("beta"), "stdout: {stdout}");
    std::fs::remove_file(path).ok();
}

#[test]
fn runs_a_noe_source_file_directly() {
    // PHP-style: point the runner at `.noe` source; it compiles on the fly (no pre-built bundle).
    let dir = std::env::temp_dir().join(format!("noeta-runner-src-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
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
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn reports_a_compile_error_in_source() {
    // A compile failure renders diagnostics and exits non-zero — the CLI's exact pipeline. An
    // undefined name is a guaranteed compile error (E0005).
    let dir = std::env::temp_dir().join(format!("noeta-runner-err-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let src = dir.join("bad.noe");
    std::fs::write(&src, "echo undefined_name_xyz;").expect("write source");
    let out = runner().arg(&src).output().expect("runner runs");
    assert_ne!(out.status.code(), Some(0));
    assert!(
        !String::from_utf8_lossy(&out.stderr).is_empty(),
        "expected a diagnostic on stderr"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn renders_an_abort_with_its_traceback() {
    let text = "fn boom(): int {\n  panic(\"kaboom\");\n}\necho boom();";
    let path = temp_bundle("abort.noeb", &build_bundle(text));
    let out = runner().arg(&path).output().expect("runner runs");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0));
    assert!(stderr.contains("kaboom"), "stderr: {stderr}");
    std::fs::remove_file(path).ok();
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
    let out = runner().arg(&path).output().expect("runs");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "i am source\n",
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0));
    std::fs::remove_file(path).ok();
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
        "noeta-html v",
        "noeta-css v",
        "noeta-prof v",
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
