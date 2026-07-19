//! End-to-end tests for `noeta migrate` — driven through the real binary against a temporary SQLite
//! file so the CLI glue, exit codes, dsn resolution, and the apply/status/dry-run/reset/new flows are
//! all exercised. The engine itself is unit-tested in `noeta-para-db`; these prove the verb wiring.

use super::support::*;

/// A private project directory seeded with the given `(filename, sql)` migrations under `migrations/`.
fn project(name: &str, migrations: &[(&str, &str)]) -> PathBuf {
    let files: Vec<(String, &str)> = migrations
        .iter()
        .map(|(f, sql)| (format!("migrations/{f}"), *sql))
        .collect();
    let refs: Vec<(&str, &str)> = files.iter().map(|(f, sql)| (f.as_str(), *sql)).collect();
    temp_dir(name, &refs)
}

const M1: (&str, &str) = (
    "0001_users.sql",
    "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);",
);
const M2: (&str, &str) = (
    "0002_seed.sql",
    "INSERT INTO users (id, name) VALUES (1, 'Ada');\nINSERT INTO users (id, name) VALUES (2, 'Bob');",
);

#[test]
fn apply_then_rerun_is_idempotent() {
    let dir = project("migrate_apply", &[M1, M2]);

    lang()
        .current_dir(&dir)
        .args(["migrate", "--db", "sqlite:app.db"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Applied 2 migration(s)"))
        .stdout(predicate::str::contains("applied 0001_users.sql"));

    // Re-running applies nothing.
    lang()
        .current_dir(&dir)
        .args(["migrate", "--db", "sqlite:app.db"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Already up to date"));
}

#[test]
fn status_reports_applied_and_pending() {
    let dir = project("migrate_status", &[M1, M2]);

    // Before applying: both pending.
    lang()
        .current_dir(&dir)
        .args(["migrate", "--db", "sqlite:app.db", "--status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("0 applied, 2 pending"));

    lang()
        .current_dir(&dir)
        .args(["migrate", "--db", "sqlite:app.db"])
        .assert()
        .success();

    lang()
        .current_dir(&dir)
        .args(["migrate", "--db", "sqlite:app.db", "--status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("2 applied, 0 pending"));
}

#[test]
fn dry_run_lists_without_applying() {
    let dir = project("migrate_dryrun", &[M1]);

    lang()
        .current_dir(&dir)
        .args(["migrate", "--db", "sqlite:app.db", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Would apply 1 migration(s)"));

    // The dry-run did not apply anything: a real status still shows it pending.
    lang()
        .current_dir(&dir)
        .args(["migrate", "--db", "sqlite:app.db", "--status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("0 applied, 1 pending"));
}

#[test]
fn new_scaffolds_a_timestamped_file() {
    let dir = temp_dir("migrate_new", &[]);

    lang()
        .current_dir(&dir)
        .args(["migrate", "new", "add posts table"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Created"))
        .stdout(predicate::str::contains("_add_posts_table.sql"));

    // Exactly one .sql file landed under migrations/.
    let created: Vec<_> = std::fs::read_dir(dir.join("migrations"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "sql"))
        .collect();
    assert_eq!(created.len(), 1);
}

#[test]
fn reset_reapplies_with_yes() {
    let dir = project("migrate_reset", &[M1, M2]);
    lang()
        .current_dir(&dir)
        .args(["migrate", "--db", "sqlite:app.db"])
        .assert()
        .success();

    lang()
        .current_dir(&dir)
        .args(["migrate", "--db", "sqlite:app.db", "--reset", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "dropped the schema and re-applied 2",
        ));
}

#[test]
fn reset_without_yes_is_refused_in_a_non_tty() {
    let dir = project("migrate_reset_refuse", &[M1]);
    lang()
        .current_dir(&dir)
        .args(["migrate", "--db", "sqlite:app.db", "--reset"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("needs confirmation"));
}

#[test]
fn a_failing_migration_stops_and_keeps_the_prior() {
    let dir = project(
        "migrate_fail",
        &[
            ("0001_ok.sql", "CREATE TABLE ok (id INTEGER);"),
            (
                "0002_bad.sql",
                "CREATE TABLE bad (id INTEGER); NONSENSE SQL;",
            ),
        ],
    );

    lang()
        .current_dir(&dir)
        .args(["migrate", "--db", "sqlite:app.db"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("0002_bad.sql"))
        .stderr(predicate::str::contains("rolled back"));

    // The first migration committed; the failed one is still pending.
    lang()
        .current_dir(&dir)
        .args(["migrate", "--db", "sqlite:app.db", "--status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 applied, 1 pending"));
}

#[test]
fn editing_an_applied_migration_is_rejected() {
    let dir = project(
        "migrate_drift",
        &[("0001_a.sql", "CREATE TABLE a (id INTEGER);")],
    );
    lang()
        .current_dir(&dir)
        .args(["migrate", "--db", "sqlite:app.db"])
        .assert()
        .success();

    // Edit the already-applied file, then re-run.
    std::fs::write(
        dir.join("migrations/0001_a.sql"),
        "CREATE TABLE a (id INTEGER, extra TEXT);",
    )
    .unwrap();
    lang()
        .current_dir(&dir)
        .args(["migrate", "--db", "sqlite:app.db"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("was edited after it was applied"));
}

#[test]
fn no_dsn_configured_is_a_usage_error() {
    let dir = project("migrate_no_dsn", &[M1]);
    lang()
        .current_dir(&dir)
        .env_remove("DATABASE_URL")
        .args(["migrate", "--status"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no database configured"));
}

#[test]
fn dsn_is_read_from_the_database_url_env() {
    let dir = project("migrate_env_dsn", &[M1]);
    lang()
        .current_dir(&dir)
        .env("DATABASE_URL", "sqlite:env.db")
        .args(["migrate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Applied 1 migration(s)"));
}

// --- Seeds -------------------------------------------------------------------------------------

/// A seed inserting Ada, written idempotently so a re-run is a no-op (the documented idiom).
const SEED_IDEMPOTENT: (&str, &str) = (
    "0001_users.sql",
    "INSERT OR IGNORE INTO users (id, name) VALUES (10, 'Ada');",
);

/// A project with migrations under `migrations/` and seeds under `seeds/`.
fn project_with_seeds(name: &str, migrations: &[(&str, &str)], seeds: &[(&str, &str)]) -> PathBuf {
    let mut files: Vec<(String, &str)> = migrations
        .iter()
        .map(|(f, sql)| (format!("migrations/{f}"), *sql))
        .collect();
    files.extend(seeds.iter().map(|(f, sql)| (format!("seeds/{f}"), *sql)));
    let refs: Vec<(&str, &str)> = files.iter().map(|(f, sql)| (f.as_str(), *sql)).collect();
    temp_dir(name, &refs)
}

#[test]
fn migrate_seed_flag_applies_then_seeds() {
    let dir = project_with_seeds("migrate_seed_flag", &[M1], &[SEED_IDEMPOTENT]);

    lang()
        .current_dir(&dir)
        .args(["migrate", "--db", "sqlite:app.db", "--seed"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Applied 1 migration(s)"))
        .stdout(predicate::str::contains("Ran 1 seed file(s)"))
        .stdout(predicate::str::contains("seeded 0001_users.sql"));
}

#[test]
fn migrate_seed_subcommand_errors_when_a_migration_is_pending() {
    let dir = project_with_seeds("migrate_seed_pending", &[M1], &[SEED_IDEMPOTENT]);

    // Nothing migrated yet: seeding a stale schema is refused with guidance.
    lang()
        .current_dir(&dir)
        .args(["migrate", "--db", "sqlite:app.db", "seed"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("migration(s) are still pending"))
        .stderr(predicate::str::contains("--seed"));
}

#[test]
fn migrate_seed_subcommand_runs_when_current_and_is_rerunnable() {
    let dir = project_with_seeds("migrate_seed_current", &[M1], &[SEED_IDEMPOTENT]);

    lang()
        .current_dir(&dir)
        .args(["migrate", "--db", "sqlite:app.db"])
        .assert()
        .success();

    // Seeds-only against the up-to-date schema, twice — the idempotent idiom keeps it a no-op.
    for _ in 0..2 {
        lang()
            .current_dir(&dir)
            .args(["migrate", "--db", "sqlite:app.db", "seed"])
            .assert()
            .success()
            .stdout(predicate::str::contains("Ran 1 seed file(s)"));
    }
}

#[test]
fn reset_seed_is_the_full_dev_loop() {
    let dir = project_with_seeds("migrate_reset_seed", &[M1], &[SEED_IDEMPOTENT]);
    lang()
        .current_dir(&dir)
        .args(["migrate", "--db", "sqlite:app.db"])
        .assert()
        .success();

    lang()
        .current_dir(&dir)
        .args([
            "migrate",
            "--db",
            "sqlite:app.db",
            "--reset",
            "--seed",
            "--yes",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "dropped the schema and re-applied 1",
        ))
        .stdout(predicate::str::contains("Ran 1 seed file(s)"));
}

#[test]
fn new_seed_scaffolds_under_the_seeds_directory() {
    let dir = temp_dir("migrate_new_seed", &[]);

    lang()
        .current_dir(&dir)
        .args(["migrate", "new", "--seed", "demo users"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Created"))
        .stdout(predicate::str::contains("_demo_users.sql"));

    // The file landed under seeds/, not migrations/, with the idempotent-idiom template.
    let created: Vec<_> = std::fs::read_dir(dir.join("seeds"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "sql"))
        .collect();
    assert_eq!(created.len(), 1);
    let body = std::fs::read_to_string(created[0].path()).unwrap();
    assert!(body.contains("INSERT OR IGNORE"), "{body}");
    assert!(!dir.join("migrations").exists());
}

#[test]
fn seeds_dir_override_flag_is_honored() {
    let dir = temp_dir(
        "migrate_seeds_dir_flag",
        &[
            ("migrations/0001_users.sql", M1.1),
            ("data/0001_users.sql", SEED_IDEMPOTENT.1),
        ],
    );

    lang()
        .current_dir(&dir)
        .args([
            "migrate",
            "--db",
            "sqlite:app.db",
            "--seed",
            "--seeds-dir",
            "data",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Ran 1 seed file(s)"));
}

#[test]
fn seeds_dir_is_read_from_the_manifest_db_table() {
    let dir = temp_dir(
        "migrate_seeds_manifest",
        &[
            (
                "noeta.toml",
                "[db]\nurl = \"sqlite:app.db\"\nmigrations = \"migrations\"\nseeds = \"fixtures\"\n",
            ),
            ("migrations/0001_users.sql", M1.1),
            ("fixtures/0001_users.sql", SEED_IDEMPOTENT.1),
        ],
    );

    lang()
        .current_dir(&dir)
        .env_remove("DATABASE_URL")
        .args(["migrate", "--seed"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Ran 1 seed file(s)"));
}

#[test]
fn dsn_is_read_from_the_manifest_db_table() {
    let dir = temp_dir(
        "migrate_manifest_dsn",
        &[
            (
                "noeta.toml",
                "[db]\nurl = \"sqlite:manifest.db\"\nmigrations = \"migrations\"\n",
            ),
            ("migrations/0001_a.sql", "CREATE TABLE a (id INTEGER);"),
        ],
    );
    lang()
        .current_dir(&dir)
        .env_remove("DATABASE_URL")
        .args(["migrate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Applied 1 migration(s)"));
}
