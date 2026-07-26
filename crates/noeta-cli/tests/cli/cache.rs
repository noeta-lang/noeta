//! End-to-end tests for `noeta cache` (ls/clean) — driven through the real binary against a
//! private seeded cache root (`NOETA_CACHE_DIR`), so the category scan, the compose staleness
//! decision, and the reporting are exercised exactly as a user sees them.

use std::path::Path;

use crate::support::*;

/// A fresh private cache root for one test — never the shared per-target cache, since these tests
/// delete from it.
fn fresh_root(tag: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("noeta-cache-verb-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create cache root");
    dir
}

/// The `noeta` command pointed at `root`.
fn noeta_at(root: &Path) -> Command {
    let mut cmd = lang();
    cmd.env("NOETA_CACHE_DIR", root);
    cmd
}

fn seed_noeb(root: &Path, name: &str, payload: usize) {
    std::fs::write(root.join(format!("{name}.noeb")), vec![b'b'; payload]).unwrap();
}

/// Seed one compose entry with an optional `identity` marker (the file `crate::compose` stamps;
/// its name is part of the on-disk contract, so it is spelled out here).
fn seed_compose(root: &Path, key: &str, identity: Option<&str>, payload: usize) {
    let dir = root.join("compose").join(key);
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("bin").join("noeta-composed"), vec![b'x'; payload]).unwrap();
    if let Some(id) = identity {
        std::fs::write(dir.join("identity"), id).unwrap();
    }
}

fn seed_pkg(root: &Path, key: &str, payload: usize) {
    let dir = root.join("pkg").join(key);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.noe"), vec![b'p'; payload]).unwrap();
}

/// The build identity of the `noeta` binary these tests spawn — the same size+mtime fingerprint
/// `noeta_cache::binary_identity()` computes for itself, so a compose entry stamped with it reads
/// as "current" to the spawned process.
fn spawned_binary_identity() -> String {
    let exe = assert_cmd::cargo::cargo_bin("noeta");
    let meta = std::fs::metadata(&exe).expect("the noeta binary exists");
    let mtime = meta
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    format!(
        "{}:{}.{:09}",
        meta.len(),
        mtime.as_secs(),
        mtime.subsec_nanos()
    )
}

#[test]
fn cache_ls_summarizes_categories_and_lists_compose_entries() {
    let root = fresh_root("ls");
    seed_noeb(&root, "aaaa", 100);
    seed_noeb(&root, "bbbb", 50);
    seed_compose(
        &root,
        "49069baa993a0e4d7e533c9865ea77fb",
        Some("some-id"),
        1000,
    );
    seed_pkg(&root, "p1", 30);

    noeta_at(&root)
        .args(["cache", "ls"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains(root.display().to_string())
                .and(predicate::str::contains("bytecode"))
                .and(predicate::str::contains("2 entries"))
                .and(predicate::str::contains("compose"))
                .and(predicate::str::contains("pkg"))
                .and(predicate::str::contains("total"))
                // The compose entry appears individually, abbreviated, with a last-used time.
                .and(predicate::str::contains("49069baa993a0e4d…"))
                .and(predicate::str::contains("last used just now")),
        );
}

#[test]
fn cache_with_no_subcommand_defaults_to_ls() {
    let root = fresh_root("default");
    seed_noeb(&root, "cccc", 10);

    noeta_at(&root).arg("cache").assert().success().stdout(
        predicate::str::contains("bytecode")
            .and(predicate::str::contains("compose"))
            .and(predicate::str::contains("pkg")),
    );
}

#[test]
fn cache_clean_removes_stale_compose_entries_and_reports_reclaimed_bytes() {
    let root = fresh_root("clean");
    seed_compose(&root, "current", Some(&spawned_binary_identity()), 10);
    seed_compose(&root, "stale", Some("999:999.000000000"), 2048);
    seed_compose(&root, "premarker", None, 1024);
    seed_noeb(&root, "keepme", 10);
    seed_pkg(&root, "p1", 10);

    noeta_at(&root)
        .args(["cache", "clean"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("removed 2 stale composed toolchains")
                .and(predicate::str::contains("reclaimed"))
                .and(predicate::str::contains("kept 1 current")),
        );

    // Exactly the stale entries went; the current one and every other category survive.
    assert!(root.join("compose").join("current").is_dir());
    assert!(!root.join("compose").join("stale").exists());
    assert!(!root.join("compose").join("premarker").exists());
    assert!(root.join("keepme.noeb").is_file());
    assert!(root.join("pkg").join("p1").is_dir());

    // A second clean finds nothing stale.
    noeta_at(&root)
        .args(["cache", "clean"])
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing stale to clean"));
}

#[test]
fn cache_clean_all_wipes_the_three_categories_but_not_unrelated_state() {
    let root = fresh_root("clean-all");
    seed_noeb(&root, "dddd", 100);
    seed_compose(&root, "k1", Some("any-id"), 1000);
    seed_pkg(&root, "p1", 30);
    // Unrelated cache-root residents (bench baselines, watch state) are not cache categories.
    std::fs::create_dir_all(root.join("bench-baselines")).unwrap();
    std::fs::write(root.join("bench-baselines").join("x.json"), b"{}").unwrap();

    noeta_at(&root)
        .args(["cache", "clean", "--all"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("bytecode: removed 1 entry")
                .and(predicate::str::contains("compose: removed 1 entry"))
                .and(predicate::str::contains("pkg: removed 1 entry"))
                .and(predicate::str::contains("reclaimed")),
        );

    assert!(!root.join("dddd.noeb").exists());
    assert!(!root.join("compose").exists());
    assert!(!root.join("pkg").exists());
    assert!(root.join("bench-baselines").join("x.json").is_file());
}

#[test]
fn cache_help_says_deleting_is_safe() {
    // The task's contract: the help text must say the caches are re-derivable / safe to delete.
    lang()
        .args(["cache", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("re-derivable").and(predicate::str::contains("safe")));
}

#[cfg(unix)]
#[test]
fn cache_refuses_cleanly_when_no_root_resolves() {
    // With no override and no way to derive a home-based root, every cache verb refuses with a
    // clear error instead of erroring uglily.
    for args in [
        vec!["cache"],
        vec!["cache", "ls"],
        vec!["cache", "clean"],
        vec!["cache", "clean", "--all"],
    ] {
        lang()
            .env_remove("NOETA_CACHE_DIR")
            .env_remove("XDG_CACHE_HOME")
            .env_remove("HOME")
            .args(&args)
            .assert()
            .failure()
            .stderr(predicate::str::contains(
                "no cache directory could be resolved",
            ));
    }
}
