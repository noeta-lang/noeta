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
        "[directives]\ndebug = \"std\"\n[targets.dev.tiers]\ndebug = true\n",
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
        "[directives]\ndebug = \"std\"\n[targets.dev]\ntiers = [\"debug\"]\n",
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
    // `@dbg` (`[directives] dbg = "std:debug"`). `@dbg` is judged by its identity `(std, debug)`, so it
    // activates under `--tier dbg` (the block runs) and strips without it — exactly as `@debug` would,
    // proving activation keys on the tier's identity, not the hardcoded built-in name.
    let src = "fn f(x: int): void {\n\
               @dbg { echo \"dbg ${x}\" }\n\
               echo \"out ${x}\"\n\
               }\n\
               f(5)\n";
    let file = temp_project("rename_dbg", "[directives]\ndbg = \"std:debug\"\n", src);
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
        "[directives]\ntest = \"std\"\n[targets.dev.tiers]\ntest = true\n",
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
        "[directives]\ndebug = \"std\"\n[targets.dev.tiers]\ndebug = true\n",
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

/// `--target` resolves against the same manifest whether the surface is `run` (an entry file) or
/// `check` (a file **or a directory**).
///
/// `manifest::resolve_active_tiers` searches upward from its argument's *parent*, because its
/// argument is an entry file. `noeta check`'s `PATH` may be a directory, and a directory is already
/// the place to search from — handing it over unchanged searched the parent and walked straight past
/// the `noeta.toml` inside it. So `noeta check --target dev <dir>` refused a project
/// `noeta run --target dev <dir>/main.noe` compiles, with an operational error naming an empty path.
#[test]
fn check_resolves_a_target_against_the_directory_it_is_given() {
    let file = temp_project(
        "check_target_dir",
        "[directives]\ndebug = \"std\"\n[targets.dev.tiers]\ndebug = true\n",
        TIERED_PROGRAM,
    );
    let dir = file.parent().expect("the project directory");

    // The run surface: the manifest is found from the entry's own directory.
    lang()
        .args(["run", "--target", "dev"])
        .arg(&file)
        .assert()
        .success();

    // The check surface, given that same directory, must find the same manifest — not its parent's.
    lang()
        .args(["check", "--target", "dev"])
        .arg(dir)
        .assert()
        .success()
        .stderr(predicate::str::contains("no `noeta.toml`").not());
}

/// **A `--target`'s `[targets.<t>.dependencies]` is in the checked program, not only the run one.**
///
/// `--target` selects two things: which tiers are live, and which *dependencies* are resolved
/// (`[targets.dev.dependencies]` layered onto the globals — dev-deps D2). `noeta check` carried the
/// first half and dropped the second, resolving the global set however the target was spelled, so a
/// dev-only dependency was absent from the checker's program and every import of it was an E0019 on
/// a project `noeta run --target dev` compiles and runs.
///
/// The control at the bottom is what makes this test about *targets*: with no `--target` the
/// dependency really is out of the program, and both surfaces say so.
#[test]
fn check_resolves_a_targets_own_dependencies() {
    let base = temp_root().join("noeta_cli_test_check_target_deps");
    let _ = std::fs::remove_dir_all(&base);
    for (path, text) in [
        (
            "app/noeta.toml",
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
             [targets.dev.dependencies]\ndevtools = { path = \"../devlib\" }\n",
        ),
        ("app/main.noe", "use devtools.api.marker\necho marker()\n"),
        (
            "devlib/noeta.toml",
            "[package]\nname = \"acme/devtools\"\nversion = \"1.0.0\"\n",
        ),
        (
            "devlib/api.noe",
            "pub fn marker(): string { return \"dev tooling linked\" }\n",
        ),
    ] {
        let full = base.join(path);
        std::fs::create_dir_all(full.parent().expect("a parent")).expect("create dirs");
        std::fs::write(&full, text).expect("write fixture");
    }
    let app = base.join("app");

    lang()
        .current_dir(&app)
        .args(["run", "--target", "dev", "main.noe"])
        .assert()
        .success()
        .stdout("dev tooling linked\n");

    lang()
        .current_dir(&app)
        .args(["check", "--target", "dev", "main.noe"])
        .assert()
        .success()
        .stderr(predicate::str::contains("E0019").not());

    // The control: `devtools` is declared ONLY under `[targets.dev]`, so the default program does
    // not have it — and neither surface pretends otherwise.
    lang()
        .current_dir(&app)
        .args(["run", "main.noe"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0019"));

    lang()
        .current_dir(&app)
        .args(["check", "main.noe"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0019"));
}
