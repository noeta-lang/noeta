//! The **database migration engine** (aether DB6) — the single implementation behind both the
//! `noeta migrate` CLI verb and the programmatic `Connection.migrate(dir)` surface. It drives any
//! [`SqlDriver`] and reads plain-SQL migration files, so SQLite and Postgres migrate through the same
//! code with no per-backend branch above the driver seam.
//!
//! # Design
//!
//! **Migrations are plain `.sql` files** in a project directory (default `migrations/`), ordered by a
//! **sortable filename prefix**. The engine sorts lexicographically over the whole filename, so any
//! zero-padded/monotonic scheme works; `migrate new` scaffolds a UTC-timestamp prefix
//! (`YYYYMMDDHHMMSS_name.sql`) because timestamps never collide across the many concurrent branches
//! this project is developed on, while still sorting chronologically. Migration bodies are run
//! **verbatim** in the target dialect's native SQL (via [`SqlDriver::execute_batch`], no `?`→`$N`
//! rewrite), so there is no portability translation — write portable SQL, or maintain per-dialect
//! directories later (a documented, deferred option).
//!
//! **A tracking table** [`TRACKING_TABLE`] records each applied migration's filename, a **sha256
//! checksum** of its contents, and the time it was applied. Two integrity gates run before anything is
//! applied: a checksum mismatch on an already-applied file is a hard error (someone edited history),
//! and an applied row whose file has vanished is a hard error (a migration was deleted).
//!
//! **Transactionality.** Each migration applies inside its own transaction (`BEGIN` … body … record …
//! `COMMIT`); the first failure rolls that migration back and stops, naming the file. Postgres has
//! fully transactional DDL and SQLite is transactional for the ordinary DDL migrations use, so a
//! half-applied migration never lands.
//!
//! **Forward-only.** There are no down/rollback files: a down migration is routinely wrong against
//! production data, and `--reset` (drop the schema, re-apply from zero) covers the development loop.
//!
//! **Seeds.** Plain `.sql` files under a project `seeds/` directory, discovered and ordered by the
//! very same [`load_dir`] loader migrations use, run **after** migrations by [`seed`]. Seeds are
//! re-runnable development data, **never** recorded in the tracking table: each runs in its own
//! transaction every time it is invoked, so idempotency is the seed author's concern (the
//! `INSERT OR IGNORE` / `ON CONFLICT DO NOTHING` idiom). [`seed_only`] refuses to run when migrations
//! are pending — seeding a stale schema is a footgun.

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::driver::{SqlDriver, SqlValue};

/// The migration tracking table — one row per applied migration. Portable DDL: `CURRENT_TIMESTAMP`
/// and this column set work identically on SQLite and Postgres, so the runner needs no per-dialect
/// tracking schema.
pub const TRACKING_TABLE: &str = "_noeta_migrations";

/// The extension a migration file must carry.
const SQL_EXTENSION: &str = "sql";

/// A discovered migration file: its filename (the ordering key), its verbatim SQL body, and the
/// sha256 hex checksum of that body (the history-integrity fingerprint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    /// The file name including extension, e.g. `20260719_143000_init.sql`. The lexicographic order
    /// key.
    pub name: String,
    /// The verbatim file contents, applied as a single native-SQL batch.
    pub sql: String,
    /// Lowercase sha256 hex of `sql`.
    pub checksum: String,
}

impl Migration {
    /// Build a migration from a filename and its contents, computing the checksum.
    pub fn new(name: impl Into<String>, sql: impl Into<String>) -> Migration {
        let name = name.into();
        let sql = sql.into();
        let checksum = sha256_hex(sql.as_bytes());
        Migration {
            name,
            sql,
            checksum,
        }
    }
}

/// One row of the tracking table: a migration that has already been applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedRecord {
    pub filename: String,
    pub checksum: String,
    pub applied_at: String,
}

/// One migration's place in the plan: applied or pending, with the recorded time when applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRow {
    pub name: String,
    pub checksum: String,
    pub applied: bool,
    pub applied_at: Option<String>,
}

/// The result of planning: every migration's status (in order) plus the indices of the pending ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub statuses: Vec<StatusRow>,
    /// Indices into the sorted migration list that are not yet applied, in apply order.
    pub pending: Vec<usize>,
}

/// A migration-engine failure, each naming the offending file where relevant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrateError {
    /// An already-applied migration file's contents no longer match the recorded checksum — its
    /// history was edited. Naming the file (edited-history detection).
    ChecksumDrift {
        filename: String,
        recorded: String,
        found: String,
    },
    /// The tracking table records a migration whose file is gone — it was deleted after being applied.
    DeletedApplied { filename: String },
    /// A migration failed while applying; the transaction was rolled back.
    Apply { filename: String, message: String },
    /// A seed file failed while running; its transaction was rolled back. Distinct from
    /// [`MigrateError::Apply`] so output names it as a seed, not a migration (seeds are untracked
    /// dev data — the prior seeds stay committed, the same stop-on-first-failure shape).
    Seed { filename: String, message: String },
    /// `migrate seed` (seeds-only) found pending migrations: the schema is stale, so seeding is
    /// refused (seeding a stale schema is a footgun). Carries how many migrations are pending.
    PendingMigrations { pending: usize },
    /// A database error outside a single migration (reading the tracking table, `BEGIN`/`COMMIT`).
    Db(String),
    /// A filesystem error discovering or reading migration files.
    Io(String),
    /// A `migrate new <name>` whose name has no usable characters.
    InvalidName(String),
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MigrateError::ChecksumDrift {
                filename,
                recorded,
                found,
            } => write!(
                f,
                "migration `{filename}` was edited after it was applied: recorded checksum \
                 {recorded:.12}… but the file now hashes to {found:.12}…. A migration's history is \
                 immutable — revert the edit, or make the change in a new migration (`migrate new`).",
            ),
            MigrateError::DeletedApplied { filename } => write!(
                f,
                "migration `{filename}` is recorded as applied but its file is gone. Restore it, or \
                 (in development) run `migrate --reset` to rebuild the schema from the current files.",
            ),
            MigrateError::Apply { filename, message } => {
                write!(
                    f,
                    "migration `{filename}` failed and was rolled back: {message}"
                )
            }
            MigrateError::Seed { filename, message } => {
                write!(f, "seed `{filename}` failed and was rolled back: {message}")
            }
            MigrateError::PendingMigrations { pending } => write!(
                f,
                "cannot seed: {pending} migration(s) are still pending, so the schema is out of \
                 date. Run `noeta migrate` first (or `noeta migrate --seed` to migrate then seed) — \
                 seeding a stale schema is a footgun.",
            ),
            MigrateError::Db(message) => write!(f, "database error: {message}"),
            MigrateError::Io(message) => write!(f, "{message}"),
            MigrateError::InvalidName(name) => {
                write!(
                    f,
                    "`{name}` is not a usable migration name (needs letters or digits)"
                )
            }
        }
    }
}

impl std::error::Error for MigrateError {}

/// Lowercase sha256 hex of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Discover every `*.sql` file directly under `dir`, sorted by filename, each with its checksum.
/// The shared loader for **both** migrations and seeds — [`seed`] reuses it for the `seeds/` dir
/// (a seed simply ignores the computed checksum, since seeds are not tracked history).
///
/// A missing directory is an error (the caller asked to migrate but there is nowhere to read from);
/// an *empty* existing directory yields an empty list (migrate is then a clean no-op). Sub-directories
/// and non-`.sql` files are ignored — leaving room for a future per-dialect `dir/postgres/` overlay
/// without disturbing v1.
pub fn load_dir(dir: &Path) -> Result<Vec<Migration>, MigrateError> {
    if !dir.exists() {
        return Err(MigrateError::Io(format!(
            "migrations directory `{}` does not exist",
            dir.display()
        )));
    }
    let entries = std::fs::read_dir(dir).map_err(|e| {
        MigrateError::Io(format!(
            "cannot read migrations directory `{}`: {e}",
            dir.display()
        ))
    })?;

    let mut migrations = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| MigrateError::Io(format!("reading `{}`: {e}", dir.display())))?;
        let path = entry.path();
        let is_sql = path.is_file()
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case(SQL_EXTENSION));
        if !is_sql {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let sql = std::fs::read_to_string(&path).map_err(|e| {
            MigrateError::Io(format!("cannot read migration `{}`: {e}", path.display()))
        })?;
        migrations.push(Migration::new(name, sql));
    }
    migrations.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(migrations)
}

/// Plan the migration run against the current tracking state — a **pure** function over the sorted
/// migrations and the applied records, so ordering / drift / deleted-file / pending computation is
/// unit-testable with no database. Returns the two integrity errors ([`MigrateError::ChecksumDrift`],
/// [`MigrateError::DeletedApplied`]) before any migration would be touched.
pub fn plan(migrations: &[Migration], applied: &[AppliedRecord]) -> Result<Plan, MigrateError> {
    // Integrity gates: every applied record must still have a matching file with a matching checksum.
    for record in applied {
        match migrations.iter().find(|m| m.name == record.filename) {
            None => {
                return Err(MigrateError::DeletedApplied {
                    filename: record.filename.clone(),
                });
            }
            Some(m) if m.checksum != record.checksum => {
                return Err(MigrateError::ChecksumDrift {
                    filename: record.filename.clone(),
                    recorded: record.checksum.clone(),
                    found: m.checksum.clone(),
                });
            }
            Some(_) => {}
        }
    }

    let mut statuses = Vec::with_capacity(migrations.len());
    let mut pending = Vec::new();
    for (index, m) in migrations.iter().enumerate() {
        match applied.iter().find(|r| r.filename == m.name) {
            Some(record) => statuses.push(StatusRow {
                name: m.name.clone(),
                checksum: m.checksum.clone(),
                applied: true,
                applied_at: Some(record.applied_at.clone()),
            }),
            None => {
                pending.push(index);
                statuses.push(StatusRow {
                    name: m.name.clone(),
                    checksum: m.checksum.clone(),
                    applied: false,
                    applied_at: None,
                });
            }
        }
    }
    Ok(Plan { statuses, pending })
}

/// Create the tracking table if it does not exist. Portable across SQLite and Postgres.
fn ensure_tracking_table(driver: &mut dyn SqlDriver) -> Result<(), MigrateError> {
    driver
        .execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {TRACKING_TABLE} (\
                 filename TEXT PRIMARY KEY, \
                 checksum TEXT NOT NULL, \
                 applied_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP\
             )"
        ))
        .map_err(MigrateError::Db)
}

/// Read every applied-migration record from the tracking table, ordered by filename.
fn read_applied(driver: &mut dyn SqlDriver) -> Result<Vec<AppliedRecord>, MigrateError> {
    let rows = driver
        .query(
            &format!(
                "SELECT filename, checksum, applied_at FROM {TRACKING_TABLE} ORDER BY filename"
            ),
            &[],
        )
        .map_err(MigrateError::Db)?;
    Ok(rows
        .into_iter()
        .map(|row| AppliedRecord {
            filename: column_text(&row, "filename"),
            checksum: column_text(&row, "checksum"),
            applied_at: column_text(&row, "applied_at"),
        })
        .collect())
}

/// Read a text column from a row, tolerating whatever scalar the driver returns (Postgres surfaces
/// `applied_at` as a timestamp value rendered to text; SQLite returns it as text already).
fn column_text(row: &crate::driver::Row, name: &str) -> String {
    row.iter()
        .find(|(col, _)| col == name)
        .map(|(_, value)| match value {
            SqlValue::Text(s) => s.clone(),
            SqlValue::Int(n) => n.to_string(),
            SqlValue::Float(f) => f.to_string(),
            SqlValue::Bool(b) => b.to_string(),
            SqlValue::Null => String::new(),
        })
        .unwrap_or_default()
}

/// Apply one migration inside its own transaction: `BEGIN`, run the verbatim body, record the row,
/// `COMMIT`. Any failure rolls back (best-effort) and reports the file.
fn apply_one(driver: &mut dyn SqlDriver, migration: &Migration) -> Result<(), MigrateError> {
    driver.execute_batch("BEGIN").map_err(MigrateError::Db)?;

    let body = (|| -> Result<(), String> {
        driver.execute_batch(&migration.sql)?;
        driver.execute(
            &format!("INSERT INTO {TRACKING_TABLE} (filename, checksum) VALUES (?, ?)"),
            &[
                SqlValue::Text(migration.name.clone()),
                SqlValue::Text(migration.checksum.clone()),
            ],
        )?;
        Ok(())
    })();

    match body {
        Ok(()) => driver.execute_batch("COMMIT").map_err(MigrateError::Db),
        Err(message) => {
            // Best-effort rollback: report the original failure regardless of whether the rollback
            // itself errored (a driver that lost the connection can't roll back either).
            let _ = driver.execute_batch("ROLLBACK");
            Err(MigrateError::Apply {
                filename: migration.name.clone(),
                message,
            })
        }
    }
}

/// Apply every pending migration in order, each in its own transaction. Returns the names applied.
/// Runs the integrity gates first (via [`plan`]); stops at the first failure with the prior
/// migrations left committed.
pub fn apply(
    driver: &mut dyn SqlDriver,
    migrations: &[Migration],
) -> Result<Vec<String>, MigrateError> {
    ensure_tracking_table(driver)?;
    let applied = read_applied(driver)?;
    let plan = plan(migrations, &applied)?;

    let mut done = Vec::with_capacity(plan.pending.len());
    for &index in &plan.pending {
        let migration = &migrations[index];
        apply_one(driver, migration)?;
        done.push(migration.name.clone());
    }
    Ok(done)
}

/// The full status of every migration (applied/pending + recorded time), running the integrity gates.
pub fn status(
    driver: &mut dyn SqlDriver,
    migrations: &[Migration],
) -> Result<Vec<StatusRow>, MigrateError> {
    ensure_tracking_table(driver)?;
    let applied = read_applied(driver)?;
    Ok(plan(migrations, &applied)?.statuses)
}

/// The names of the pending migrations, without applying anything (backs `--dry-run`).
pub fn pending(
    driver: &mut dyn SqlDriver,
    migrations: &[Migration],
) -> Result<Vec<String>, MigrateError> {
    ensure_tracking_table(driver)?;
    let applied = read_applied(driver)?;
    let plan = plan(migrations, &applied)?;
    Ok(plan
        .pending
        .into_iter()
        .map(|i| migrations[i].name.clone())
        .collect())
}

/// **Destructive.** Drop the whole schema (via [`SqlDriver::reset`]) and re-apply every migration from
/// zero. Returns the names re-applied. The caller is responsible for confirming intent.
pub fn reset(
    driver: &mut dyn SqlDriver,
    migrations: &[Migration],
) -> Result<Vec<String>, MigrateError> {
    driver.reset().map_err(MigrateError::Db)?;
    apply(driver, migrations)
}

/// Run every **seed** file in `seeds` in filename order, each in its own transaction, returning the
/// names run. Seeds are re-runnable development data, **never tracked** in [`TRACKING_TABLE`]: this
/// applies every file every time it is called, so idempotency is the seed author's concern (use
/// `INSERT OR IGNORE` on SQLite / `ON CONFLICT DO NOTHING` on Postgres to make a re-run a no-op). A
/// seed uses the same discovery/ordering as a migration — [`load_dir`] loads both — but no checksum
/// is recorded (a seed is not history). The first failure rolls that seed back and stops, naming the
/// file, with the prior seeds left committed — the same stop-on-first-failure shape as [`apply`].
pub fn seed(driver: &mut dyn SqlDriver, seeds: &[Migration]) -> Result<Vec<String>, MigrateError> {
    let mut done = Vec::with_capacity(seeds.len());
    for file in seeds {
        seed_one(driver, file)?;
        done.push(file.name.clone());
    }
    Ok(done)
}

/// Run one seed file inside its own transaction: `BEGIN`, the verbatim body, `COMMIT` — no tracking
/// insert (seeds are not history). Any failure rolls back (best-effort) and reports the file.
fn seed_one(driver: &mut dyn SqlDriver, file: &Migration) -> Result<(), MigrateError> {
    driver.execute_batch("BEGIN").map_err(MigrateError::Db)?;
    match driver.execute_batch(&file.sql) {
        Ok(()) => driver.execute_batch("COMMIT").map_err(MigrateError::Db),
        Err(message) => {
            let _ = driver.execute_batch("ROLLBACK");
            Err(MigrateError::Seed {
                filename: file.name.clone(),
                message,
            })
        }
    }
}

/// Run seeds **only**, requiring the schema to be up to date first: if any migration under
/// `migrations` is still pending, returns [`MigrateError::PendingMigrations`] and runs no seed
/// (seeding a stale schema is a footgun). Backs `noeta migrate seed`. Runs the migration integrity
/// gates (via [`plan`]) as part of the pending check, so a drifted/deleted migration still errors.
pub fn seed_only(
    driver: &mut dyn SqlDriver,
    migrations: &[Migration],
    seeds: &[Migration],
) -> Result<Vec<String>, MigrateError> {
    ensure_tracking_table(driver)?;
    let applied = read_applied(driver)?;
    let plan = plan(migrations, &applied)?;
    if !plan.pending.is_empty() {
        return Err(MigrateError::PendingMigrations {
            pending: plan.pending.len(),
        });
    }
    seed(driver, seeds)
}

/// Build the filename for a new migration: `{prefix}_{slug}.sql`, where `slug` is `name` lowercased
/// with every run of non-alphanumeric characters collapsed to a single `_`. Pure (the caller supplies
/// the timestamp `prefix`), so it is testable without a clock.
pub fn scaffold_filename(prefix: &str, name: &str) -> Result<String, MigrateError> {
    let mut slug = String::with_capacity(name.len());
    let mut prev_underscore = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_underscore = false;
        } else if !prev_underscore {
            slug.push('_');
            prev_underscore = true;
        }
    }
    let slug = slug.trim_matches('_');
    if slug.is_empty() {
        return Err(MigrateError::InvalidName(name.to_string()));
    }
    Ok(format!("{prefix}_{slug}.{SQL_EXTENSION}"))
}

/// The starter body written into a freshly scaffolded migration file.
pub const SCAFFOLD_TEMPLATE: &str = "-- Migration: write forward-only SQL below. This file's contents are checksummed once applied,\n\
     -- so edit it only before it runs; make later changes in a new migration.\n";

/// The starter body written into a freshly scaffolded seed file — re-runnable dev data, so it
/// documents the idempotent-insert idiom inline.
pub const SEED_SCAFFOLD_TEMPLATE: &str = "-- Seed: re-runnable development data. This file runs on every `noeta migrate seed` / `--seed`,\n\
     -- each in its own transaction, and is NOT tracked. Make inserts idempotent so a re-run is a\n\
     -- no-op: `INSERT OR IGNORE` on SQLite, `... ON CONFLICT DO NOTHING` on Postgres.\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn migration(name: &str, sql: &str) -> Migration {
        Migration::new(name, sql)
    }

    fn applied(m: &Migration, at: &str) -> AppliedRecord {
        AppliedRecord {
            filename: m.name.clone(),
            checksum: m.checksum.clone(),
            applied_at: at.to_string(),
        }
    }

    #[test]
    fn checksum_is_stable_lowercase_hex_of_contents() {
        // Known SHA-256 of the empty input and of "abc".
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Migration::new hashes its body.
        assert_eq!(migration("0001_x.sql", "abc").checksum, sha256_hex(b"abc"));
    }

    #[test]
    fn plan_orders_and_computes_pending() {
        // Two applied, one new → only the new one is pending, statuses are in filename order.
        let a = migration("0001_a.sql", "CREATE TABLE a(id INT);");
        let b = migration("0002_b.sql", "CREATE TABLE b(id INT);");
        let c = migration("0003_c.sql", "CREATE TABLE c(id INT);");
        let migrations = vec![a.clone(), b.clone(), c.clone()];
        let records = vec![applied(&a, "t1"), applied(&b, "t2")];

        let plan = plan(&migrations, &records).unwrap();
        assert_eq!(plan.pending, vec![2]);
        assert_eq!(
            plan.statuses
                .iter()
                .map(|s| (s.name.as_str(), s.applied))
                .collect::<Vec<_>>(),
            vec![
                ("0001_a.sql", true),
                ("0002_b.sql", true),
                ("0003_c.sql", false)
            ]
        );
        assert_eq!(plan.statuses[0].applied_at.as_deref(), Some("t1"));
        assert_eq!(plan.statuses[2].applied_at, None);
    }

    #[test]
    fn plan_detects_checksum_drift_on_an_applied_file() {
        let a = migration("0001_a.sql", "CREATE TABLE a(id INT);");
        // The tracking table recorded a *different* checksum → the file was edited after applying.
        let record = AppliedRecord {
            filename: "0001_a.sql".into(),
            checksum: "deadbeef".into(),
            applied_at: "t1".into(),
        };
        let err = plan(&[a], &[record]).unwrap_err();
        match err {
            MigrateError::ChecksumDrift { filename, .. } => assert_eq!(filename, "0001_a.sql"),
            other => panic!("expected drift, got {other:?}"),
        }
    }

    #[test]
    fn plan_detects_a_deleted_applied_migration() {
        // A record with no corresponding file.
        let record = AppliedRecord {
            filename: "0001_gone.sql".into(),
            checksum: "abc".into(),
            applied_at: "t1".into(),
        };
        let err = plan(&[], &[record]).unwrap_err();
        assert_eq!(
            err,
            MigrateError::DeletedApplied {
                filename: "0001_gone.sql".into()
            }
        );
    }

    #[test]
    fn plan_with_nothing_applied_makes_everything_pending() {
        let a = migration("0001_a.sql", "SELECT 1;");
        let b = migration("0002_b.sql", "SELECT 2;");
        let plan = plan(&[a, b], &[]).unwrap();
        assert_eq!(plan.pending, vec![0, 1]);
    }

    #[test]
    fn seed_scaffold_template_documents_the_idempotent_idiom() {
        // The seed starter body names both drivers' idempotent-insert idioms, so a re-run is a no-op.
        assert!(SEED_SCAFFOLD_TEMPLATE.contains("INSERT OR IGNORE"));
        assert!(SEED_SCAFFOLD_TEMPLATE.contains("ON CONFLICT DO NOTHING"));
        assert!(SEED_SCAFFOLD_TEMPLATE.contains("NOT tracked"));
    }

    #[test]
    fn scaffold_filename_slugifies_and_prefixes() {
        assert_eq!(
            scaffold_filename("20260719143000", "Add Users Table").unwrap(),
            "20260719143000_add_users_table.sql"
        );
        assert_eq!(
            scaffold_filename("0004", "create--posts!!").unwrap(),
            "0004_create_posts.sql"
        );
    }

    #[test]
    fn scaffold_filename_rejects_an_empty_slug() {
        assert!(matches!(
            scaffold_filename("0001", "!!!").unwrap_err(),
            MigrateError::InvalidName(_)
        ));
    }

    #[test]
    fn load_dir_errors_when_missing_but_is_empty_when_bare() {
        let missing = std::path::Path::new("/does/not/exist/noeta-migrate-xyz");
        assert!(matches!(load_dir(missing), Err(MigrateError::Io(_))));

        let dir = std::env::temp_dir().join(format!("noeta-migrate-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load_dir(&dir).unwrap(), Vec::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_dir_reads_sorts_and_ignores_non_sql() {
        let dir = std::env::temp_dir().join(format!("noeta-migrate-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0002_b.sql"), "SELECT 2;").unwrap();
        std::fs::write(dir.join("0001_a.sql"), "SELECT 1;").unwrap();
        std::fs::write(dir.join("README.md"), "not a migration").unwrap();

        let migrations = load_dir(&dir).unwrap();
        assert_eq!(
            migrations
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            vec!["0001_a.sql", "0002_b.sql"]
        );
        assert_eq!(migrations[0].sql, "SELECT 1;");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// End-to-end tests against a real in-memory SQLite driver (behind `ring-sqlite`, the default
/// feature). They exercise the whole engine over a live database: apply, idempotent re-run, status,
/// reset, dry-run, and a mid-migration failure leaving the prior migration committed.
#[cfg(all(test, feature = "ring-sqlite"))]
mod sqlite_e2e {
    use super::*;
    use crate::sqlite::SqliteDriver;

    fn mem() -> SqliteDriver {
        SqliteDriver::open_in_memory().unwrap()
    }

    fn table_exists(driver: &mut dyn SqlDriver, table: &str) -> bool {
        let rows = driver
            .query(
                "SELECT name FROM sqlite_master WHERE type='table' AND name = ?",
                &[SqlValue::Text(table.to_string())],
            )
            .unwrap();
        !rows.is_empty()
    }

    fn migrations() -> Vec<Migration> {
        vec![
            Migration::new(
                "0001_users.sql",
                "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);",
            ),
            Migration::new(
                "0002_posts.sql",
                "CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT);\n\
                 INSERT INTO posts (id, title) VALUES (1, 'hello');",
            ),
        ]
    }

    #[test]
    fn apply_then_rerun_is_a_noop() {
        let mut driver = mem();
        let migrations = migrations();

        let first = apply(&mut driver, &migrations).unwrap();
        assert_eq!(first, vec!["0001_users.sql", "0002_posts.sql"]);
        assert!(table_exists(&mut driver, "users"));
        assert!(table_exists(&mut driver, "posts"));
        // The multi-statement second migration ran fully (its INSERT landed).
        let rows = driver
            .query("SELECT COUNT(*) AS n FROM posts", &[])
            .unwrap();
        assert_eq!(rows[0][0].1, SqlValue::Int(1));

        // Re-running applies nothing.
        let second = apply(&mut driver, &migrations).unwrap();
        assert!(second.is_empty());
    }

    #[test]
    fn status_reports_applied_and_pending() {
        let mut driver = mem();
        let migrations = migrations();
        apply(&mut driver, &migrations[..1]).unwrap(); // apply only the first

        let rows = status(&mut driver, &migrations).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].applied);
        assert!(rows[0].applied_at.is_some());
        assert!(!rows[1].applied);
    }

    #[test]
    fn dry_run_lists_pending_without_applying() {
        let mut driver = mem();
        let migrations = migrations();

        let names = pending(&mut driver, &migrations).unwrap();
        assert_eq!(names, vec!["0001_users.sql", "0002_posts.sql"]);
        // Nothing was created — dry-run only read.
        assert!(!table_exists(&mut driver, "users"));
    }

    #[test]
    fn reset_drops_everything_and_reapplies() {
        let mut driver = mem();
        let migrations = migrations();
        apply(&mut driver, &migrations).unwrap();
        driver
            .execute("INSERT INTO users (id, name) VALUES (1, 'Ada')", &[])
            .unwrap();

        let reapplied = reset(&mut driver, &migrations).unwrap();
        assert_eq!(reapplied, vec!["0001_users.sql", "0002_posts.sql"]);
        // The user row is gone (schema was dropped), the schema is back.
        assert!(table_exists(&mut driver, "users"));
        let rows = driver
            .query("SELECT COUNT(*) AS n FROM users", &[])
            .unwrap();
        assert_eq!(rows[0][0].1, SqlValue::Int(0));
    }

    #[test]
    fn a_failing_migration_rolls_back_and_leaves_the_prior_one() {
        let mut driver = mem();
        let migrations = vec![
            Migration::new("0001_ok.sql", "CREATE TABLE ok (id INTEGER);"),
            Migration::new(
                "0002_bad.sql",
                "CREATE TABLE bad (id INTEGER); NONSENSE SQL HERE;",
            ),
        ];

        let err = apply(&mut driver, &migrations).unwrap_err();
        match err {
            MigrateError::Apply { filename, .. } => assert_eq!(filename, "0002_bad.sql"),
            other => panic!("expected an apply error, got {other:?}"),
        }
        // The first migration is committed; the failed one left nothing behind (its CREATE rolled back).
        assert!(table_exists(&mut driver, "ok"));
        assert!(!table_exists(&mut driver, "bad"));
        // And it is recorded as applied so a re-run resumes at the failing one.
        let rows = status(&mut driver, &migrations).unwrap();
        assert!(rows[0].applied);
        assert!(!rows[1].applied);
    }

    #[test]
    fn editing_an_applied_migration_is_detected_on_the_next_run() {
        let mut driver = mem();
        let original = vec![Migration::new("0001_a.sql", "CREATE TABLE a (id INTEGER);")];
        apply(&mut driver, &original).unwrap();

        // Same filename, different contents → checksum drift.
        let edited = vec![Migration::new(
            "0001_a.sql",
            "CREATE TABLE a (id INTEGER, extra TEXT);",
        )];
        let err = apply(&mut driver, &edited).unwrap_err();
        assert!(matches!(err, MigrateError::ChecksumDrift { .. }));
    }

    fn count(driver: &mut dyn SqlDriver, table: &str) -> i64 {
        let rows = driver
            .query(&format!("SELECT COUNT(*) AS n FROM {table}"), &[])
            .unwrap();
        match rows[0][0].1 {
            SqlValue::Int(n) => n,
            ref other => panic!("expected an int count, got {other:?}"),
        }
    }

    #[test]
    fn seeds_run_in_filename_order_and_are_never_tracked() {
        let mut driver = mem();
        apply(&mut driver, &migrations()).unwrap();

        let seeds = vec![
            Migration::new(
                "0002_b.sql",
                "INSERT INTO users (id, name) VALUES (2, 'Bob');",
            ),
            Migration::new(
                "0001_a.sql",
                "INSERT INTO users (id, name) VALUES (1, 'Ada');",
            ),
        ];
        // `load_dir` sorts; `seed` runs whatever order it is handed — so sort first, like the callers.
        let mut ordered = seeds.clone();
        ordered.sort_by(|a, b| a.name.cmp(&b.name));
        let ran = seed(&mut driver, &ordered).unwrap();
        assert_eq!(ran, vec!["0001_a.sql", "0002_b.sql"]);
        assert_eq!(count(&mut driver, "users"), 2);

        // Seeds are NOT recorded in the tracking table — they are re-runnable data, not history.
        let tracked = driver
            .query(
                &format!("SELECT filename FROM {TRACKING_TABLE} ORDER BY filename"),
                &[],
            )
            .unwrap();
        let names: Vec<_> = tracked.iter().map(|r| column_text(r, "filename")).collect();
        assert_eq!(names, vec!["0001_users.sql", "0002_posts.sql"]);
    }

    #[test]
    fn a_plain_seed_reruns_every_time_but_an_idempotent_one_is_a_noop() {
        let mut driver = mem();
        apply(&mut driver, &migrations()).unwrap();

        // A plain INSERT re-inserts on every run (idempotency is the author's concern).
        let plain = vec![Migration::new(
            "0001_plain.sql",
            "INSERT INTO posts (id, title) VALUES (2, 'again');",
        )];
        seed(&mut driver, &plain).unwrap();
        seed(&mut driver, &plain).unwrap_err(); // second run trips the PK — proves it re-ran

        // The documented idempotent idiom makes a re-run a no-op.
        let idempotent = vec![Migration::new(
            "0001_idem.sql",
            "INSERT OR IGNORE INTO posts (id, title) VALUES (3, 'once');",
        )];
        seed(&mut driver, &idempotent).unwrap();
        let after_first = count(&mut driver, "posts");
        seed(&mut driver, &idempotent).unwrap();
        assert_eq!(count(&mut driver, "posts"), after_first);
    }

    #[test]
    fn a_failing_seed_stops_and_leaves_the_prior_committed() {
        let mut driver = mem();
        apply(&mut driver, &migrations()).unwrap();

        let seeds = vec![
            Migration::new(
                "0001_ok.sql",
                "INSERT INTO users (id, name) VALUES (1, 'Ada');",
            ),
            Migration::new("0002_bad.sql", "NONSENSE SQL HERE;"),
        ];
        let err = seed(&mut driver, &seeds).unwrap_err();
        match err {
            MigrateError::Seed { filename, .. } => assert_eq!(filename, "0002_bad.sql"),
            other => panic!("expected a seed error, got {other:?}"),
        }
        // The first seed committed before the second failed.
        assert_eq!(count(&mut driver, "users"), 1);
    }

    #[test]
    fn seed_only_refuses_when_a_migration_is_pending() {
        let mut driver = mem();
        // Apply only the first migration, leaving the second pending.
        let migrations = migrations();
        apply(&mut driver, &migrations[..1]).unwrap();

        let seeds = vec![Migration::new(
            "0001_a.sql",
            "INSERT INTO users (id, name) VALUES (1, 'Ada');",
        )];
        let err = seed_only(&mut driver, &migrations, &seeds).unwrap_err();
        assert_eq!(err, MigrateError::PendingMigrations { pending: 1 });
        // No seed ran — `users` is empty (the schema exists from the first migration).
        assert_eq!(count(&mut driver, "users"), 0);
    }

    #[test]
    fn seed_only_runs_seeds_once_the_schema_is_current() {
        let mut driver = mem();
        let migrations = migrations();
        apply(&mut driver, &migrations).unwrap();

        let seeds = vec![Migration::new(
            "0001_a.sql",
            "INSERT INTO users (id, name) VALUES (1, 'Ada');",
        )];
        let ran = seed_only(&mut driver, &migrations, &seeds).unwrap();
        assert_eq!(ran, vec!["0001_a.sql"]);
        assert_eq!(count(&mut driver, "users"), 1);
    }
}

/// A full migration round-trip against a **live** PostgreSQL, run only when `NOETA_PG_TEST_DSN` is set
/// (the same env gate the driver's own live tests use), so the default suite stays hermetic. Proves
/// the engine is genuinely driver-agnostic: apply, idempotent re-run, status, and reset all work over
/// Postgres transactional DDL exactly as over SQLite.
#[cfg(all(test, feature = "ring-postgres"))]
mod postgres_e2e {
    use super::*;
    use crate::pg::PostgresDriver;

    #[test]
    fn migrate_round_trip_against_a_live_server() {
        let Ok(dsn) = std::env::var("NOETA_PG_TEST_DSN") else {
            return; // no server configured — skip (the hermetic + SQLite tests still ran)
        };
        let mut driver = PostgresDriver::connect(&dsn).expect("connect to NOETA_PG_TEST_DSN");
        // Start from a clean schema so repeated CI runs are deterministic.
        driver.reset().expect("reset");

        let migrations = vec![
            Migration::new(
                "0001_widgets.sql",
                "CREATE TABLE widgets (id INT PRIMARY KEY, name TEXT NOT NULL);",
            ),
            Migration::new(
                "0002_seed.sql",
                "INSERT INTO widgets (id, name) VALUES (1, 'alpha');\n\
                 INSERT INTO widgets (id, name) VALUES (2, 'beta');",
            ),
        ];

        let applied = apply(&mut driver, &migrations).unwrap();
        assert_eq!(applied, vec!["0001_widgets.sql", "0002_seed.sql"]);
        let rows = driver
            .query("SELECT COUNT(*) AS n FROM widgets", &[])
            .unwrap();
        assert_eq!(rows[0][0].1, SqlValue::Int(2));

        // Idempotent re-run.
        assert!(apply(&mut driver, &migrations).unwrap().is_empty());

        // Status reports both applied.
        let st = status(&mut driver, &migrations).unwrap();
        assert!(st.iter().all(|s| s.applied));

        // Seeds run against the up-to-date schema, untracked and re-runnable via the Postgres idiom.
        let seeds = vec![Migration::new(
            "0001_widgets.sql",
            "INSERT INTO widgets (id, name) VALUES (3, 'gamma') ON CONFLICT DO NOTHING;",
        )];
        let ran = seed_only(&mut driver, &migrations, &seeds).unwrap();
        assert_eq!(ran, vec!["0001_widgets.sql"]);
        assert_eq!(
            driver
                .query("SELECT COUNT(*) AS n FROM widgets", &[])
                .unwrap()[0][0]
                .1,
            SqlValue::Int(3)
        );
        // Re-running the idempotent seed is a no-op (ON CONFLICT DO NOTHING), and it was never tracked.
        seed(&mut driver, &seeds).unwrap();
        assert_eq!(
            driver
                .query("SELECT COUNT(*) AS n FROM widgets", &[])
                .unwrap()[0][0]
                .1,
            SqlValue::Int(3)
        );

        // Reset wipes and re-applies.
        let reapplied = reset(&mut driver, &migrations).unwrap();
        assert_eq!(reapplied.len(), 2);

        driver.reset().expect("final cleanup");
    }
}
