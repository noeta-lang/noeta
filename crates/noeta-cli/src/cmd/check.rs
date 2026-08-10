//! `noeta check` — statically validate a file or directory: the load → link → type-check
//! front half of `run`, stopping before codegen.

use std::io::{self, Write};
use std::process::ExitCode;

use noeta_diagnostics::render_mapped_colored;
use noeta_pm::manifest;
use noeta_project::{ProjectCheckOptions, project_check};

use crate::{OutputFormat, compose};

// The project walk itself lives in `noeta-project`, because `noeta check`, the editor and the MCP
// `check` tool must answer "is this project clean" with one function rather than three. These are
// the pieces of it the CLI's *other* directory commands (`test`, `bench`, `doc`, `expand`) share.
pub(crate) use noeta_project::project::{entry_pool, noe_files, pool_modules};

/// The `--format json` report: the whole outcome of a `noeta check` run in one serializable object.
#[derive(serde::Serialize)]
pub(crate) struct CheckReport {
    /// The number of `.noe` files checked (entries walked).
    files_checked: usize,
    /// The number of unique error-severity diagnostics.
    errors: usize,
    /// The number of unique warning-severity diagnostics.
    warnings: usize,
    /// The dev tiers whose blocks were also checked, beyond the stripped shipping shape — sorted,
    /// deduplicated across every entry. Empty when the sources declare no code-tier block.
    tiers_checked: Vec<String>,
    /// Every unique diagnostic, resolved to files + line/column (see [`noeta_diagnostics::to_json`]).
    diagnostics: Vec<noeta_diagnostics::JsonDiagnostic>,
}

/// `noeta check [PATH]` — statically validate source without running or building it: parse every
/// `.noe` file and verify it type-checks, printing all diagnostics and exiting non-zero if any is an
/// error. This is `cmd_run`'s front half — load → (activate tiers) → `check_all` — stopping before
/// `execute_real_host`, so it has no side effects.
///
/// **This command is a printer.** The check itself is [`noeta_project::project_check`], which the LSP's
/// `workspace/diagnostic` and the MCP `check` tool also call: the three surfaces used to walk,
/// activate and sweep in three places and disagreed about what "clean" meant — this one was the
/// only one that swept the tier bodies at all. What is left here is argument resolution, rendering
/// and the exit code.
///
/// **`check` covers every shape of the source, not just the one that ships.** A dev-tier block is
/// stripped before lowering, so mirroring `run`'s empty active-tier set meant a `@test` body never
/// reached the checker at all: `noeta check .` said "0 errors" about a file `noeta test` could not
/// compile, and a whole session's worth of `@test` blocks could accumulate errors that nothing
/// reported until the test run. So each entry is checked once as it ships (no tiers) and then **once
/// per code tier its own blocks name** — the exact shape `noeta test`/`noeta bench`/`noeta <tier>`
/// activates. `--tier`/`--target` still select a shape explicitly (their union is checked as one, as
/// before); the per-tier sweep covers whatever that selection leaves out.
///
/// `PATH` (default `.`) is a file or a directory; a directory is walked recursively and **every**
/// `.noe` file is checked as its own entry, because the loader links only an entry's own module
/// pool — a library module no entry imports would otherwise never be type-checked.
///
/// With `--format json` the whole result is emitted as one machine-readable object on stdout (for
/// CI, editors, and the MCP server) instead of human-rendered diagnostics on stderr; the exit code
/// is identical in both modes.
pub(crate) fn cmd_check(
    path: &std::path::Path,
    tiers: &[String],
    target: &Option<String>,
    format: OutputFormat,
) -> ExitCode {
    // Delegate to the app's composed toolchain if its dependency graph carries native crates —
    // otherwise the check runs in a process that cannot link the program at all.
    if let Err(code) = compose::maybe_delegate(path) {
        return code;
    }

    // The active tier set — resolved once and applied to every file — is the union of a `--target`'s
    // live tiers (from `noeta.toml`) and any explicit `--tier` flags, exactly as `cmd_run` resolves
    // it. A bad target fails fast before any file is read.
    //
    // `resolve_active_tiers` takes an **entry file** and searches from that file's parent, which is
    // where `noeta run`'s copy of this resolution starts. `check`'s `PATH` is a file *or a
    // directory*, and a directory is already the place to search from — passing it straight through
    // searched its PARENT and walked right past the manifest sitting inside it, so
    // `noeta check --target dev app` failed with "no `noeta.toml` found at or above ``" on the very
    // project `noeta run --target dev app/main.noe` compiles. Naming a file inside the directory
    // makes the two questions the same question.
    let anchor = if path.is_dir() {
        path.join(manifest::MANIFEST_NAME)
    } else {
        path.to_path_buf()
    };
    let mut active: Vec<String> = match target {
        Some(name) => match manifest::resolve_active_tiers(&anchor, name) {
            Ok(tiers) => tiers,
            Err(err) => {
                eprintln!("noeta: {err}");
                return ExitCode::from(1);
            }
        },
        None => Vec::new(),
    };
    for tier in tiers {
        if !active.contains(tier) {
            active.push(tier.clone());
        }
    }

    // The target travels whole: its live *tiers* select which shapes are checked, and the target
    // itself selects the **dependency set** — `[targets.<name>.dependencies]` layered onto the
    // globals, exactly as `noeta run --target` layers them. Passing only the first half checked a
    // dev-target build against the default target's dependencies and reported E0019 against every
    // import of a dev-only dependency on a project `noeta run --target dev` compiles and runs.
    let checked = project_check(
        path,
        &ProjectCheckOptions::new()
            .with_tiers(active)
            .with_target(target.as_deref()),
    );
    if checked.files_checked == 0 {
        eprintln!("noeta: no `.noe` files found under `{}`", path.display());
        return ExitCode::from(2);
    }
    // Operational failures — an unreadable file, a dependency graph that would not resolve — are
    // reported on stderr in both output modes and gate the exit code, but they are not diagnostics
    // about the code and never enter the JSON report's `diagnostics`.
    for problem in &checked.problems {
        eprintln!("noeta: {problem}");
    }
    // A process that cannot link the program was supposed to have delegated above; if it did not
    // (a composition that is not built, inside a toolchain that guards against re-composing), say
    // so rather than letting the unresolved-import cascade stand as the answer.
    if !checked.uncomposed.is_empty() {
        eprintln!(
            "noeta: {}",
            noeta_pm::composed::explain(&checked.uncomposed, path)
        );
    }

    let errors = checked.errors();
    let warnings = checked.warnings();
    let n = checked.files_checked;

    match format {
        OutputFormat::Human => {
            // Render each unique diagnostic against its pool's sources, then a summary line — all
            // to stderr, as the other commands do. `--format json` renders through `to_json` and
            // never reaches here, so the machine-readable form cannot pick up escape sequences.
            let color = noeta_diagnostics::stderr_color();
            let mut stderr = io::stderr();
            for entry in &checked.diagnostics {
                let _ = stderr.write_all(
                    render_mapped_colored(
                        &entry.sources,
                        std::iter::once(&entry.diagnostic),
                        color,
                    )
                    .as_bytes(),
                );
            }
            let files = if n == 1 { "file" } else { "files" };
            // Name the tiers whose blocks were checked too, so the summary reports what it looked
            // at — the shipping shape alone reads identically to the shipping shape *plus* every
            // tier, and only one of those means "your `@test` bodies compile".
            let tiers = if checked.tiers_checked.is_empty() {
                String::new()
            } else {
                format!(" (tiers: {})", checked.tiers_checked.join(", "))
            };
            eprintln!("checked {n} {files}{tiers}: {errors} error(s), {warnings} warning(s)");
        }
        OutputFormat::Json => {
            // A single machine-readable report on stdout, so a tool can pipe `noeta check --format
            // json` and parse it. Operational `cannot read` errors stay on stderr; the exit code
            // still reflects them.
            let report = CheckReport {
                files_checked: n,
                errors,
                warnings,
                tiers_checked: checked.tiers_checked.clone(),
                diagnostics: checked
                    .diagnostics
                    .iter()
                    .map(|entry| noeta_diagnostics::to_json(&entry.sources, &entry.diagnostic))
                    .collect(),
            };
            match serde_json::to_string_pretty(&report) {
                Ok(json) => println!("{json}"),
                Err(err) => eprintln!("noeta: cannot serialize check report: {err}"),
            }
        }
    }

    if errors > 0 {
        ExitCode::from(1)
    } else if !checked.problems.is_empty() {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}
