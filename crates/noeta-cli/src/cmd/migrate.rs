//! `noeta migrate` — apply a project's plain-SQL database migrations (para/db). A thin CLI over the
//! one migration engine in `noeta-para-db`: it resolves the connection string and migrations
//! directory, opens the driver the dsn scheme selects, and drives `migrate::{apply, status, pending,
//! reset}`. `migrate new` needs no database — it only scaffolds a file.
//!
//! Exit codes follow the CLI convention: `0` success; `2` for a usage/config problem (no dsn
//! configured, a missing migrations directory, `--reset` without confirmation); `1` for a failure
//! that ran but did not complete (connect failure, a SQL error, checksum drift, a deleted migration).

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use noeta_para_db::conn::open_driver;
use noeta_para_db::migrate::{self, MigrateError, SCAFFOLD_TEMPLATE};
use noeta_pm::manifest;

/// The default migrations directory when none is configured or passed.
const DEFAULT_DIR: &str = "migrations";

/// The environment variable consulted for the connection string (after `--db`, before `[db] url`).
const DATABASE_URL_ENV: &str = "DATABASE_URL";

/// The parsed `noeta migrate` invocation.
pub(crate) struct MigrateArgs {
    /// `Some((name, dir))` for `migrate new <name>`; `None` for the apply/status/reset flags.
    pub(crate) new: Option<(String, Option<PathBuf>)>,
    pub(crate) db: Option<String>,
    pub(crate) dir: Option<PathBuf>,
    pub(crate) status: bool,
    pub(crate) dry_run: bool,
    pub(crate) reset: bool,
    pub(crate) yes: bool,
}

/// Run `noeta migrate`.
pub(crate) fn cmd_migrate(args: MigrateArgs) -> ExitCode {
    // `migrate new` is database-free: scaffold a file and return.
    if let Some((name, dir)) = args.new {
        return match scaffold_new(&name, dir.as_deref()) {
            Ok(path) => {
                println!("Created {}", path.display());
                ExitCode::SUCCESS
            }
            Err(err) => usage_error(&err),
        };
    }

    let dir = resolve_dir(args.dir.as_deref());
    let dsn = match resolve_dsn(args.db.as_deref()) {
        Ok(dsn) => dsn,
        Err(err) => return usage_error(&err),
    };

    // Discover + checksum the migration files (a missing directory is a usage error).
    let migrations = match migrate::load_dir(&dir) {
        Ok(migrations) => migrations,
        Err(err) => return usage_error(&err.to_string()),
    };

    let mut driver = match open_driver(&dsn) {
        Ok(driver) => driver,
        Err(err) => return run_error(&format!("cannot open database: {err}")),
    };
    let driver = driver.as_mut();

    if args.status {
        return match migrate::status(driver, &migrations) {
            Ok(rows) => {
                print_status(&dir, &rows);
                ExitCode::SUCCESS
            }
            Err(err) => run_error(&err.to_string()),
        };
    }

    if args.dry_run {
        return match migrate::pending(driver, &migrations) {
            Ok(names) => {
                print_pending(&names);
                ExitCode::SUCCESS
            }
            Err(err) => run_error(&err.to_string()),
        };
    }

    if args.reset {
        if !confirm_reset(args.yes, &dsn) {
            // Either the user declined at the prompt (a clean cancel) or there was no terminal to ask
            // on (a usage error telling them to pass `--yes`).
            return if io::stdin().is_terminal() {
                println!("Aborted; database unchanged.");
                ExitCode::SUCCESS
            } else {
                usage_error("`--reset` needs confirmation: pass `--yes` (no interactive terminal)")
            };
        }
        return match migrate::reset(driver, &migrations) {
            Ok(applied) => {
                println!(
                    "Reset: dropped the schema and re-applied {} migration(s).",
                    applied.len()
                );
                for name in &applied {
                    println!("  applied {name}");
                }
                ExitCode::SUCCESS
            }
            Err(err) => run_error(&err.to_string()),
        };
    }

    // Default: apply every pending migration.
    match migrate::apply(driver, &migrations) {
        Ok(applied) if applied.is_empty() => {
            println!("Already up to date ({} migration(s)).", migrations.len());
            ExitCode::SUCCESS
        }
        Ok(applied) => {
            println!(
                "Applied {} migration(s) from {}:",
                applied.len(),
                dir.display()
            );
            for name in &applied {
                println!("  applied {name}");
            }
            ExitCode::SUCCESS
        }
        Err(err) => run_error(&err.to_string()),
    }
}

/// Scaffold `migrations/<UTC-timestamp>_<slug>.sql` (creating the directory), returning the new path.
fn scaffold_new(name: &str, dir: Option<&Path>) -> Result<PathBuf, String> {
    let dir = dir
        .map(Path::to_path_buf)
        .or_else(|| manifest_dir_value(|db| db.migrations.clone()).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DIR));
    let filename = migrate::scaffold_filename(&utc_timestamp(), name)
        .map_err(|e: MigrateError| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| {
        format!(
            "cannot create migrations directory `{}`: {e}",
            dir.display()
        )
    })?;
    let path = dir.join(filename);
    if path.exists() {
        return Err(format!("`{}` already exists", path.display()));
    }
    std::fs::write(&path, SCAFFOLD_TEMPLATE)
        .map_err(|e| format!("cannot write `{}`: {e}", path.display()))?;
    Ok(path)
}

/// Resolve the migrations directory: the `--dir` flag, else `[db] migrations`, else `migrations/`.
fn resolve_dir(flag: Option<&Path>) -> PathBuf {
    flag.map(Path::to_path_buf)
        .or_else(|| manifest_dir_value(|db| db.migrations.clone()).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DIR))
}

/// Resolve the connection string, highest priority first: the `--db` flag, `DATABASE_URL`, then the
/// `[db] url` in the nearest `noeta.toml`. Absent everywhere is a usage error.
fn resolve_dsn(flag: Option<&str>) -> Result<String, String> {
    if let Some(dsn) = flag {
        return Ok(dsn.to_string());
    }
    if let Ok(dsn) = std::env::var(DATABASE_URL_ENV)
        && !dsn.is_empty()
    {
        return Ok(dsn);
    }
    if let Some(url) = manifest_dir_value(|db| db.url.clone()) {
        return Ok(url);
    }
    Err(format!(
        "no database configured: pass `--db <dsn>`, set `{DATABASE_URL_ENV}`, or add a `[db]` \
         table with `url = \"…\"` to noeta.toml"
    ))
}

/// Read a value out of the nearest `noeta.toml`'s `[db]` table, if the manifest exists and parses.
/// Manifest problems are lenient here (the flag/env layers still apply); a genuinely malformed
/// manifest surfaces through the other verbs that parse it strictly.
fn manifest_dir_value<T>(pick: impl Fn(&manifest::DbConfig) -> Option<T>) -> Option<T> {
    let cwd = std::env::current_dir().ok()?;
    let path = manifest::find(&cwd)?;
    let manifest = manifest::load(&path).ok()?;
    pick(manifest.db())
}

/// Confirm a destructive `--reset`: `--yes` skips the prompt; otherwise, on a terminal, require the
/// user to type `yes`. Returns whether to proceed.
fn confirm_reset(yes: bool, dsn: &str) -> bool {
    if yes {
        return true;
    }
    if !io::stdin().is_terminal() {
        return false;
    }
    print!("This will DROP ALL DATA in `{dsn}` and re-apply from zero. Type 'yes' to continue: ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    line.trim().eq_ignore_ascii_case("yes")
}

/// Format a UTC `YYYYMMDDHHMMSS` timestamp for a new migration's filename prefix.
fn utc_timestamp() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    )
}

/// Print the applied/pending status table.
fn print_status(dir: &Path, rows: &[migrate::StatusRow]) {
    if rows.is_empty() {
        println!("No migrations under {}.", dir.display());
        return;
    }
    println!("Migrations under {}:", dir.display());
    for row in rows {
        if row.applied {
            let at = row.applied_at.as_deref().unwrap_or("");
            println!("  [applied] {}  ({at})", row.name);
        } else {
            println!("  [pending] {}", row.name);
        }
    }
    let pending = rows.iter().filter(|r| !r.applied).count();
    println!("{} applied, {pending} pending.", rows.len() - pending);
}

/// Print the dry-run pending list.
fn print_pending(names: &[String]) {
    if names.is_empty() {
        println!("No pending migrations.");
        return;
    }
    println!("Would apply {} migration(s):", names.len());
    for name in names {
        println!("  {name}");
    }
}

/// Report a usage/config problem (exit code 2), the CLI convention for bad input.
fn usage_error(message: &str) -> ExitCode {
    eprintln!("noeta migrate: {message}");
    ExitCode::from(2)
}

/// Report a run that started but failed (exit code 1): a connection failure or a migration error.
fn run_error(message: &str) -> ExitCode {
    eprintln!("noeta migrate: {message}");
    ExitCode::from(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_timestamp_is_fourteen_digits() {
        let ts = utc_timestamp();
        assert_eq!(ts.len(), 14, "{ts}");
        assert!(ts.chars().all(|c| c.is_ascii_digit()), "{ts}");
    }

    #[test]
    fn db_flag_wins_over_env_and_manifest() {
        assert_eq!(
            resolve_dsn(Some("sqlite::memory:")).unwrap(),
            "sqlite::memory:"
        );
    }
}
