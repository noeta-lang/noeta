//! End-to-end tests for the wasm runner, driven native — the runner is target-agnostic by
//! design, so the same binary logic that ships as `wasm32-wasip1` is exercised here as a host
//! process over real temp files. The wasm-executed differential is the W1.3 oracle's job.

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
    let path =
        std::env::temp_dir().join(format!("noeta-wasm-runner-{}-{name}", std::process::id()));
    std::fs::write(&path, bytes).expect("write bundle");
    path
}

fn runner() -> Command {
    Command::new(env!("CARGO_BIN_EXE_noeta-wasm-runner"))
}

#[test]
fn runs_a_bundle_on_the_wasi_host() {
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
fn sandbox_mode_matches_a_native_sandbox_run() {
    // `--sandbox` must reproduce the deterministic native run exactly — the property the W1.3
    // oracle asserts over the whole corpus, sampled here for one seeded-PRNG program.
    let text = "use std.random;\nrandom.seed(7);\necho random.int(0, 100);";
    let bundle = build_bundle(text);

    let db = noeta_db::LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "test.noe", text);
    let src = noeta_db::source_program(&db, &source);
    let module = noeta_db::bytecode(&db, src).0.as_ref().expect("compiles");
    let native = noeta_vm::VmBackend::new().run_module(module);

    let path = temp_bundle("seeded.noeb", &bundle);
    let out = runner()
        .arg("--sandbox")
        .arg(&path)
        .output()
        .expect("runner runs");
    assert_eq!(String::from_utf8_lossy(&out.stdout), native.stdout);
    assert_eq!(out.status.code(), Some(native.exit_code));
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
