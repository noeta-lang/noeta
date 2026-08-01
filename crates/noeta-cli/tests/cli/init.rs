//! `noeta init`: the scaffold is complete, immediately usable by every tier tool, safe on
//! non-empty directories, additive when re-run inside a package it already scaffolded, and
//! its manifest round-trips through the real parser.

use crate::support::*;

/// A fresh scratch dir under the per-test-target tmp (hermetic, survives for inspection).
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Every file the scaffold writes, in the order `init` reports them.
const SCAFFOLD: &[&str] = &[
    "noeta.toml",
    "src/main.noe",
    ".gitignore",
    ".vscode/launch.json",
    ".vscode/extensions.json",
    "AGENTS.md",
    "SYNTAX.md",
];

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

    for rel in SCAFFOLD {
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

/// Re-running `init` in a package is additive, not a refusal: the generated `SYNTAX.md` goes
/// stale between releases and the documented way to refresh it is to delete it and re-run.
/// A pre-existing `noeta.toml` used to abort the whole run, so that recipe could not work.
#[test]
fn init_refills_a_gap_in_an_existing_package() {
    let dir = scratch("init_refill");
    init_in(&dir, &[]).success();
    let manifest = std::fs::read_to_string(dir.join("noeta.toml")).unwrap();
    let main = std::fs::read_to_string(dir.join("src/main.noe")).unwrap();
    // The user edits their entry file, then deletes the stale language reference.
    std::fs::write(
        dir.join("src/main.noe"),
        "fn main() {\n  print(\"mine\")\n}\n",
    )
    .unwrap();
    std::fs::remove_file(dir.join("SYNTAX.md")).unwrap();

    init_in(&dir, &[])
        .success()
        .stdout(predicate::str::contains("created SYNTAX.md"))
        .stdout(predicate::str::contains("exists  noeta.toml"))
        .stdout(predicate::str::contains("updated Noeta package `local/"))
        .stdout(predicate::str::contains("1 created, 6 left unchanged"));

    assert!(
        dir.join("SYNTAX.md").exists(),
        "SYNTAX.md must be regenerated"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("noeta.toml")).unwrap(),
        manifest,
        "an existing manifest must never be rewritten"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("src/main.noe")).unwrap(),
        "fn main() {\n  print(\"mine\")\n}\n",
        "the user's edited entry file must survive untouched"
    );
    assert_ne!(main, "fn main() {\n  print(\"mine\")\n}\n");
}

/// A fully scaffolded package is a no-op that says so and succeeds — nothing rewritten.
#[test]
fn init_is_a_no_op_in_a_complete_package() {
    let dir = scratch("init_noop");
    init_in(&dir, &[]).success();
    let before: Vec<(String, String)> = SCAFFOLD
        .iter()
        .map(|rel| {
            (
                (*rel).to_string(),
                std::fs::read_to_string(dir.join(rel)).unwrap(),
            )
        })
        .collect();

    init_in(&dir, &[])
        .success()
        .stdout(predicate::str::contains("already fully scaffolded"))
        .stdout(predicate::str::contains("7 files left unchanged"))
        .stdout(predicate::str::contains("created").not());

    for (rel, contents) in before {
        assert_eq!(
            std::fs::read_to_string(dir.join(&rel)).unwrap(),
            contents,
            "{rel} must be byte-identical after a no-op init"
        );
    }
}

/// `--name` cannot rename a package that already has a manifest: it is reported as ignored
/// and the manifest keeps its own name.
#[test]
fn init_ignores_name_for_an_existing_package() {
    let dir = scratch("init_rename");
    init_in(&dir, &["--name", "acme/webapp"]).success();
    std::fs::remove_file(dir.join("AGENTS.md")).unwrap();

    init_in(&dir, &["--name", "other/thing"])
        .success()
        .stderr(predicate::str::contains("ignoring --name"))
        .stdout(predicate::str::contains(
            "updated Noeta package `acme/webapp`",
        ));
    let manifest = std::fs::read_to_string(dir.join("noeta.toml")).unwrap();
    assert!(manifest.contains("name = \"acme/webapp\""), "{manifest}");
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
