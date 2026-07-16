//! `noeta check` — statically validate a file or directory: the load → link → type-check
//! front half of `run`, stopping before codegen.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use noeta_diagnostics::{Diagnostic, render_mapped};
use noeta_pm::{graph, manifest};
use noeta_runner::resolve_providers;
use noeta_span::SourceMap;

use crate::{OutputFormat, compose};

/// The `--format json` report: the whole outcome of a `noeta check` run in one serializable object.
#[derive(serde::Serialize)]
pub(crate) struct CheckReport {
    /// The number of `.noe` files checked (entries walked).
    files_checked: usize,
    /// The number of unique error-severity diagnostics.
    errors: usize,
    /// The number of unique warning-severity diagnostics.
    warnings: usize,
    /// Every unique diagnostic, resolved to files + line/column (see [`noeta_diagnostics::to_json`]).
    diagnostics: Vec<noeta_diagnostics::JsonDiagnostic>,
}

/// `noeta check [PATH]` — statically validate source without running or building it: parse every
/// `.noe` file and verify it type-checks, printing all diagnostics and exiting non-zero if any is an
/// error. This is `cmd_run`'s front half — load → (activate tiers) → `check_all` — stopping before
/// `execute_real_host`, so it has no side effects.
///
/// `PATH` (default `.`) is a file or a directory; a directory is walked recursively and **every**
/// `.noe` file is checked as its own entry. The loader links only an entry's *directory-sibling*
/// modules (there is no cross-directory module graph), so checking each file as an entry is what
/// guarantees a library module no single entry imports is still parsed and type-checked. A module
/// shared by several entries is therefore linked (and its diagnostics produced) once per importer;
/// diagnostics are deduplicated globally by their source file + span + code so each is reported once.
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
    if let Some(code) = compose::maybe_delegate(path) {
        return code;
    }
    use noeta_diagnostics::Severity;

    // The active tier set — resolved once and applied to every file — is the union of a `--target`'s
    // live tiers (from `noeta.toml`) and any explicit `--tier` flags, exactly as `cmd_run` resolves
    // it. A bad target fails fast before any file is read.
    let mut active: Vec<String> = match target {
        Some(name) => match manifest::resolve_active_tiers(path, name) {
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
    // The target's tier → provider map steers which declaration's config attribute activation
    // stamps (provider dispatch); empty without a target.
    let providers = match resolve_providers(path, target) {
        Ok(map) => map,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(1);
        }
    };
    let active_refs: Vec<&str> = active.iter().map(String::as_str).collect();

    // The set of entry files to check: the file itself, or every `.noe` file under the directory.
    let entries: Vec<PathBuf> = if path.is_dir() {
        noe_files(path)
    } else {
        vec![path.to_path_buf()]
    };
    if entries.is_empty() {
        eprintln!("noeta: no `.noe` files found under `{}`", path.display());
        return ExitCode::from(2);
    }

    // Deduplicate diagnostics across every entry's workspace. `SourceId`s are workspace-local (each
    // load restarts them at 0), so the key is the *file name* the diagnostic renders against plus its
    // byte span and code — never the id. The map's key order (name, then offset, then code) is also
    // the render order, so output is deterministic. Each value keeps the workspace's `SourceMap`
    // (shared via `Rc` so a workspace's diagnostics don't each clone it) so a diagnostic — and any of
    // its cross-file labels — resolves against the right source in both the human and JSON paths.
    type MapDiag = (std::rc::Rc<SourceMap>, Diagnostic);
    let mut diags: std::collections::BTreeMap<(String, u32, u32, &'static str), MapDiag> =
        std::collections::BTreeMap::new();
    let mut fold = |sources: &std::rc::Rc<SourceMap>, diag: &Diagnostic| {
        let key = (
            sources.source(diag.span.source).name().to_string(),
            diag.span.start,
            diag.span.end,
            diag.code.code(),
        );
        diags
            .entry(key)
            .or_insert_with(|| (std::rc::Rc::clone(sources), diag.clone()));
    };

    let mut unreadable = false;
    for entry in &entries {
        // Resolve the entry's dependency packages so a cross-package `use <dep-key>.…` type-checks
        // accurately under `noeta check`, matching `run` (package-manager P2.1c). A resolution
        // failure is reported like an unreadable file — it doesn't abort the whole check.
        let deps = match graph::resolve_graph(entry) {
            Ok(graph) => graph.packages,
            Err(err) => {
                eprintln!("noeta: {}: {err}", entry.display());
                unreadable = true;
                continue;
            }
        };
        match noeta_loader::load_with_deps(entry, manifest::root_edition(entry), &deps) {
            Err(err) => {
                // One unreadable file does not abort the whole run — record it and keep checking the
                // rest, so `check` reports as much as it can in a single pass.
                eprintln!("noeta: cannot read {}: {err}", entry.display());
                unreadable = true;
            }
            Ok(Err(load_diagnostics)) => {
                // Lex/parse errors — each carries the single source it renders against. Wrap it in a
                // one-element `SourceMap` (any `SourceId` resolves back to that source), matching how
                // `cmd_run` renders load diagnostics single-source.
                for ld in &load_diagnostics {
                    let sources = std::rc::Rc::new(SourceMap::new(vec![ld.source.clone()]));
                    fold(&sources, &ld.diagnostic);
                }
            }
            Ok(Ok(linked)) => {
                // Activate the resolved dev-tiers before checking, as `run`/`build`/`dump` do; with no
                // active tiers the program is checked as-is. Tier-activation diagnostics resolve
                // against the same workspace sources. Checking rides `Loaded::check`/`check_under`
                // so the per-source editions travel structurally (audit-3 F8).
                let loaded = crate::context::loaded(linked);
                let program_diags = if active_refs.is_empty() {
                    loaded.check().diagnostics
                } else {
                    let activated =
                        noeta_check::activate_tiers_with(&loaded.program, &active_refs, &providers);
                    let mut ds = activated.diagnostics;
                    ds.extend(
                        crate::context::check_under(&activated.program, &loaded.editions)
                            .diagnostics,
                    );
                    ds
                };
                let sources = std::rc::Rc::new(loaded.sources);
                for d in &program_diags {
                    fold(&sources, d);
                }
            }
        }
    }

    // Count severities once (independent of output format): errors gate the exit code, warnings print
    // but pass.
    let mut errors = 0usize;
    let mut warnings = 0usize;
    for (_, diag) in diags.values() {
        match diag.severity {
            Severity::Error => errors += 1,
            Severity::Warning => warnings += 1,
            Severity::Note => {}
        }
    }
    let n = entries.len();

    match format {
        OutputFormat::Human => {
            // Render each unique diagnostic against its workspace sources (color disabled), then a
            // summary line — all to stderr, as the other commands do.
            let mut stderr = io::stderr();
            for (sources, diag) in diags.values() {
                let _ = stderr.write_all(render_mapped(sources, std::iter::once(diag)).as_bytes());
            }
            let files = if n == 1 { "file" } else { "files" };
            eprintln!("checked {n} {files}: {errors} error(s), {warnings} warning(s)");
        }
        OutputFormat::Json => {
            // A single machine-readable report on stdout, so a tool can pipe `noeta check --format
            // json` and parse it. Operational `cannot read` errors stay on stderr; the exit code
            // still reflects them.
            let report = CheckReport {
                files_checked: n,
                errors,
                warnings,
                diagnostics: diags
                    .values()
                    .map(|(sources, diag)| noeta_diagnostics::to_json(sources, diag))
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
    } else if unreadable {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}

/// Collect every `.noe` file under `root`, recursively, in sorted order (so discovery and thus the
/// check order are deterministic). Hand-rolled in the style of the loader's `read_siblings` — a
/// depth-first `read_dir` walk that silently skips directories it cannot read (a partial tree still
/// checks what it can). Symlinked directories are followed by `read_dir` as ordinary entries; cycles
/// are not guarded against, matching the loader's own assumptions about a normal source tree.
pub(crate) fn noe_files(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut dirs = Vec::new();
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().is_some_and(|ext| ext == "noe") {
                out.push(p);
            }
        }
        stack.extend(dirs);
    }
    out.sort();
    out
}
