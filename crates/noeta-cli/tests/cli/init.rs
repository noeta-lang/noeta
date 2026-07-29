//! `noeta init`: the scaffold is complete, immediately usable by every tier tool, safe on
//! non-empty directories, and its manifest round-trips through the real parser.

use crate::support::*;

/// A fresh scratch dir under the per-test-target tmp (hermetic, survives for inspection).
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn init_in(dir: &std::path::Path, extra: &[&str]) -> assert_cmd::assert::Assert {
    let mut cmd = lang();
    cmd.arg("init").arg(dir).arg("--no-git").args(extra);
    cmd.assert()
}

#[test]
fn init_scaffolds_a_runnable_project() {
    let dir = scratch("init_runnable");
    init_in(&dir, &[])
        .success()
        .stdout(predicate::str::contains("created noeta.toml"))
        .stdout(predicate::str::contains("initialized Noeta package"));

    for rel in [
        "noeta.toml",
        "src/main.noe",
        ".gitignore",
        ".vscode/launch.json",
        ".vscode/extensions.json",
        "AGENTS.md",
        "SYNTAX.md",
    ] {
        assert!(dir.join(rel).exists(), "missing scaffold file {rel}");
    }

    // The scaffold type-checks clean…
    lang().args(["check"]).arg(&dir).assert().success();
    // …runs (the @debug block stripped)…
    lang()
        .args(["run"])
        .arg(dir.join("src/main.noe"))
        .assert()
        .success()
        .stdout(predicate::str::contains("Hello, Noeta!"))
        .stdout(predicate::str::contains("greet(Noeta)").not());
    // …compiles the @debug block in under the development target…
    lang()
        .args(["run"])
        .arg(dir.join("src/main.noe"))
        .args(["--target", "development"])
        .assert()
        .success()
        .stdout(predicate::str::contains("greet(Noeta)"));
    // …its tests pass…
    lang()
        .args(["test"])
        .arg(dir.join("src/main.noe"))
        .assert()
        .success()
        .stdout(predicate::str::contains("2 passed, 0 failed"));
    // …and the production target gates the test tier off (a no-op, not a failure).
    lang()
        .args(["test"])
        .arg(dir.join("src/main.noe"))
        .args(["--target", "production"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "not active in target `production`",
        ));
}

#[test]
fn init_manifest_parses_with_both_targets() {
    let dir = scratch("init_manifest");
    init_in(&dir, &["--name", "acme/webapp"]).success();

    let manifest = noeta_pm::manifest::load(&dir.join("noeta.toml")).expect("manifest parses");
    let pkg = manifest.package().expect("has [package]");
    assert_eq!(pkg.name.company, "acme");
    assert_eq!(pkg.name.package, "webapp");

    let dev = manifest
        .active_tier_providers("development")
        .expect("development target resolves");
    for tier in ["test", "bench", "doc", "debug"] {
        assert_eq!(
            dev.get(tier).map(String::as_str),
            Some("std"),
            "tier {tier}"
        );
    }
    let prod = manifest
        .active_tier_providers("production")
        .expect("production target resolves");
    assert!(prod.is_empty(), "production must make no tiers live");
}

#[test]
fn init_refuses_an_existing_package() {
    let dir = scratch("init_refuse");
    init_in(&dir, &[]).success();
    init_in(&dir, &[])
        .failure()
        .stderr(predicate::str::contains("already a Noeta package"));
}

#[test]
fn init_never_overwrites_existing_files() {
    let dir = scratch("init_preserve");
    std::fs::write(dir.join(".gitignore"), "# mine\n").unwrap();
    init_in(&dir, &[])
        .success()
        .stdout(predicate::str::contains("exists  .gitignore"));
    assert_eq!(
        std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
        "# mine\n",
        "pre-existing file must be left byte-identical"
    );
}

#[test]
fn init_rejects_a_malformed_name() {
    let dir = scratch("init_badname");
    init_in(&dir, &["--name", "no-slash"])
        .failure()
        .stderr(predicate::str::contains("company/package"));
    assert!(
        !dir.join("noeta.toml").exists(),
        "a rejected name must not leave a partial scaffold"
    );
}

#[test]
fn init_names_the_package_after_the_directory() {
    let base = scratch("init_dirname");
    let dir = base.join("My-Web App");
    init_in(&dir, &[])
        .success()
        .stdout(predicate::str::contains("`local/my_web_app`"));
    let manifest = std::fs::read_to_string(dir.join("noeta.toml")).unwrap();
    assert!(manifest.contains("name = \"local/my_web_app\""));
}

#[test]
fn init_creates_a_git_repository_unless_nested() {
    if !git_available() {
        return;
    }
    // Fresh directory → a repo. The one fixture that must live *outside* the workspace checkout,
    // so it cannot hang off `temp_root()` like every other: `CARGO_TARGET_TMPDIR` is inside this
    // repo's own worktree, where `init` would (correctly) decline to nest a repository. Keyed by
    // pid instead, since a fixed name under the shared system temp dir collides across the
    // concurrent worktrees this repository is routinely worked in.
    let dir = std::env::temp_dir().join(format!("noeta_cli_test_init_git_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    lang()
        .arg("init")
        .arg(&dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("created git repository"));
    assert!(dir.join(".git").exists());

    // Inside an existing worktree → no nested repo.
    let nested = dir.join("nested");
    lang()
        .arg("init")
        .arg(&nested)
        .assert()
        .success()
        .stdout(predicate::str::contains("created git repository").not());
    assert!(!nested.join(".git").exists());
}
