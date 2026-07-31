//! `--target` (object-model slice 6g): the `noeta.toml` build-target manifest.

use crate::support::*;

// --- `--target` (object-model slice 6g: the `noeta.toml` build-target manifest) ----

/// Write a `noeta.toml` alongside a program in its private temp directory, returning the program
/// path. The manifest is discovered by walking up from the entry file's directory.
fn temp_project(name: &str, manifest: &str, src: &str) -> PathBuf {
    let path = temp_program(name, src);
    std::fs::write(path.parent().unwrap().join("noeta.toml"), manifest).expect("write noeta.toml");
    path
}

const TIERED_PROGRAM: &str = "fn f(x: int): void {\n\
         @debug { echo \"dbg ${x}\" }\n\
         echo \"out ${x}\"\n\
     }\n\
     @test fn t(): void { assert(1 + 1 == 2) }\n\
     f(5)\n";

#[test]
fn run_target_activates_its_tiers() {
    // A target that makes the `debug` tier live compiles the `@debug` block in, exactly as
    // `--tier debug` would — but driven by `noeta.toml`.
    let file = temp_project(
        "prof_run",
        "[tiers]\ndebug = \"std\"\n[targets.dev.tiers]\ndebug = true\n",
        TIERED_PROGRAM,
    );
    lang()
        .arg("run")
        .arg(&file)
        .arg("--target")
        .arg("dev")
        .assert()
        .success()
        .stdout("dbg 5\nout 5\n");
}

#[test]
fn run_target_activates_its_tiers_via_the_array_spelling() {
    // The canonical array form (`tiers = ["debug"]` on the target) activates the tier exactly like
    // the boolean sub-table does — end to end through a real `noeta run`.
    let file = temp_project(
        "prof_run_array",
        "[tiers]\ndebug = \"std\"\n[targets.dev]\ntiers = [\"debug\"]\n",
        TIERED_PROGRAM,
    );
    lang()
        .arg("run")
        .arg(&file)
        .arg("--target")
        .arg("dev")
        .assert()
        .success()
        .stdout("dbg 5\nout 5\n");
}

#[test]
fn a_renamed_std_tier_activates_and_strips_under_its_local_name() {
    // The headline of per-package tier naming: a package renames std's `debug` tier to a local
    // `@dbg` (`[tiers] dbg = "std:debug"`). `@dbg` is judged by its identity `(std, debug)`, so it
    // activates under `--tier dbg` (the block runs) and strips without it — exactly as `@debug` would,
    // proving activation keys on the tier's identity, not the hardcoded built-in name.
    let src = "fn f(x: int): void {\n\
               @dbg { echo \"dbg ${x}\" }\n\
               echo \"out ${x}\"\n\
               }\n\
               f(5)\n";
    let file = temp_project("rename_dbg", "[tiers]\ndbg = \"std:debug\"\n", src);
    lang()
        .arg("run")
        .arg(&file)
        .arg("--tier")
        .arg("dbg")
        .assert()
        .success()
        .stdout("dbg 5\nout 5\n");
    // Without activating it, the renamed block strips like any inactive tier.
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("out 5\n");
}

#[test]
fn run_minimalist_target_strips_everything() {
    // A target that opts into no tiers leaves every tier block stripped (same as a bare run).
    let file = temp_project("prof_run_min", "[targets.prod]\n", TIERED_PROGRAM);
    lang()
        .arg("run")
        .arg(&file)
        .arg("--target")
        .arg("prod")
        .assert()
        .success()
        .stdout("out 5\n");
}

#[test]
fn test_target_gates_the_runner() {
    // `lang test --target prod`, where `prod` does not make `test` live, runs nothing and says so.
    let file = temp_project("prof_test_gate", "[targets.prod]\n", TIERED_PROGRAM);
    lang()
        .arg("test")
        .arg(&file)
        .arg("--target")
        .arg("prod")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "tier `test` is not active in target `prod`",
        ));
}

#[test]
fn test_target_with_tier_live_runs() {
    let file = temp_project(
        "prof_test_live",
        "[tiers]\ntest = \"std\"\n[targets.dev.tiers]\ntest = true\n",
        TIERED_PROGRAM,
    );
    lang()
        .arg("test")
        .arg(&file)
        .arg("--target")
        .arg("dev")
        .assert()
        .success()
        .stdout(predicate::str::contains("1 passed, 0 failed, 1 total"));
}

#[test]
fn run_unknown_target_is_an_error() {
    let file = temp_project(
        "prof_unknown",
        "[tiers]\ndebug = \"std\"\n[targets.dev.tiers]\ndebug = true\n",
        TIERED_PROGRAM,
    );
    lang()
        .arg("run")
        .arg(&file)
        .arg("--target")
        .arg("ghost")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("unknown target `ghost`"));
}

#[test]
fn run_target_without_manifest_is_an_error() {
    // `--target` with no `noeta.toml` anywhere above the entry is a clear error, not a silent run.
    let file = temp_program("prof_no_manifest", "echo \"hi\"\n");
    lang()
        .arg("run")
        .arg(&file)
        .arg("--target")
        .arg("dev")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("no `noeta.toml`"));
}
