//! End-to-end tests for the wasm runner, driven native — the runner is target-agnostic by
//! design, so the same binary logic that ships as `wasm32-wasip1` is exercised here as a host
//! process over real temp files. The wasm-executed differential is the W1.3 oracle's job.

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
/// Per-process and per-call, never the shared system temp dir: fixtures dropped there are visible
/// to every other process on the machine, and a Noeta entry point drags its *siblings* in — the
/// loader links the containing directory as the project. `TempPath` carries the directory's guard,
/// so the tree lives exactly as long as the path does.
fn temp_bundle(name: &str, bytes: &[u8]) -> noeta_test_temp::TempPath {
    let dir = noeta_test_temp::TempDir::new("wasm-runner");
    std::fs::write(dir.join(name), bytes).expect("write bundle");
    dir.into_child(name)
}

fn runner() -> Command {
    Command::new(env!("CARGO_BIN_EXE_noeta-wasm-runner"))
}

#[test]
fn runs_a_bundle_on_the_wasi_host() {
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
fn sandbox_mode_matches_a_native_sandbox_run() {
    // `--sandbox` must reproduce the deterministic native run exactly — the property the W1.3
    // oracle asserts over the whole corpus, sampled here for one seeded-PRNG program.
    let text = "use std.random;\nrandom.seed(7);\necho random.int(0, 100);";
    let bundle = build_bundle(text);

    let db = noeta_db::LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "test.noe", text);
    let src = noeta_db::source_program(&db, &source, noeta_db::Edition::DEFAULT);
    let module = noeta_db::bytecode(&db, src).0.as_ref().expect("compiles");
    let native = noeta_vm::VmBackend::new().run_module(module);

    let path = temp_bundle("seeded.noeb", &bundle);
    let out = runner()
        .arg("--sandbox")
        .arg(path.path())
        .output()
        .expect("runner runs");
    assert_eq!(String::from_utf8_lossy(&out.stdout), native.stdout);
    assert_eq!(out.status.code(), Some(native.exit_code));

    // The env channel (`NOETA_WASM_SANDBOX=1`) selects the same configuration — it exists for
    // stapled artifacts, whose argv belongs to the program (W1.2).
    let out = runner()
        .env("NOETA_WASM_SANDBOX", "1")
        .arg(path.path())
        .output()
        .expect("runner runs");
    assert_eq!(String::from_utf8_lossy(&out.stdout), native.stdout);
    assert_eq!(out.status.code(), Some(native.exit_code));
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
    // exactly as `echo` is buffered into `stdout`. This tail wrote stdout and forgot stderr, so
    // every byte a wasm-hosted program wrote to its error stream vanished — silently, with a zero
    // exit. One of seven hand-written copies of this epilogue (plans/parallel-path-audit.md row 1);
    // three of them still drop the stream, which is why this asserts the behaviour rather than
    // trusting the shared helper it does not yet call.
    let text = "use std.io\necho \"to stdout\"\nio.errln(\"to stderr\")\n";
    let path = temp_bundle("streams.noeb", &build_bundle(text));
    let out = runner().arg(path.path()).output().expect("runner runs");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "to stdout\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "to stderr\n");
}

#[test]
fn program_stderr_precedes_the_traceback() {
    // The order every run tail writes: the program's own stderr, then diagnostics, then the
    // traceback. A program that reports progress on stderr and then aborts must not have its
    // report appear *after* the failure that followed it.
    let text = "use std.io\nio.errln(\"step one\")\npanic(\"kaboom\");\n";
    let path = temp_bundle("order.noeb", &build_bundle(text));
    let out = runner().arg(path.path()).output().expect("runner runs");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0));
    let program = stderr.find("step one").expect("program stderr is written");
    let abort = stderr.find("kaboom").expect("the traceback is written");
    assert!(program < abort, "stderr out of order: {stderr:?}");
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
    let out = runner().arg(path.path()).output().expect("runs");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a `.noeb` bundle"));
}

#[test]
fn no_arguments_prints_usage() {
    let out = runner().output().expect("runs");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
}
