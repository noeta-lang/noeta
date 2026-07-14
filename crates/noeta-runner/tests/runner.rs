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
fn refuses_a_missing_file_and_a_non_bundle() {
    let out = runner()
        .arg("/nonexistent/app.noeb")
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read"));

    let path = temp_bundle("plain.txt", b"echo \"not a bundle\";");
    let out = runner().arg(&path).output().expect("runs");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a `.noeb` bundle"));
    std::fs::remove_file(path).ok();
}

#[test]
fn no_arguments_prints_usage() {
    let out = runner().output().expect("runs");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
}
