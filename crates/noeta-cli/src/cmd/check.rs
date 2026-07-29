//! `noeta check` — statically validate a file or directory: the load → link → type-check
//! front half of `run`, stopping before codegen.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use noeta_diagnostics::{Diagnostic, render_mapped};
use noeta_pm::{graph, manifest};
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
/// guarantees a library module no single entry imports is still parsed and type-checked. Each
/// directory's sources are read, lexed, and parsed **once** (and its dependency graph resolved
/// once) — every entry links against that shared pool (audit-4 F4); a module shared by several
/// entries still produces its diagnostics once per importer's link, deduplicated globally by
/// source file + span + code so each is reported once.
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
    // The compose probe hands back the graph it resolved — reused below for the directory whose
    // manifest the probe resolved against (audit-5 F2).
    let mut resolved = match compose::maybe_delegate(path) {
        Err(code) => return code,
        Ok(resolved) => resolved,
    };
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

    // Group the entries by directory (audit-4 F4): an entry's workspace is exactly its
    // directory's `.noe` files, so every entry in one directory shares the same sources, the
    // same manifest (hence dependency graph and edition), and the same parsed sibling pool.
    // Loading each entry independently re-lexed/re-parsed the whole directory and re-resolved
    // the dependency graph once **per entry** (N entries → N× the work); here each directory is
    // read, resolved, lexed, and parsed once, and each entry links against the shared pool
    // (`noeta_loader::parse_dir`/`link_entry` — per-entry semantics identical to
    // `load_with_deps`). Diagnostics still dedup/order by (file, span, code), so output is
    // unchanged.
    let mut by_dir: std::collections::BTreeMap<PathBuf, Vec<&PathBuf>> =
        std::collections::BTreeMap::new();
    for entry in &entries {
        // An empty parent (a bare relative name like `noeta check foo.noe`) is the current
        // directory: `read_dir_modules` scans `.` for it while keeping the pool's module names
        // unprefixed, so the entry-to-pool name match below still holds.
        let dir = entry
            .parent()
            .unwrap_or_else(|| std::path::Path::new(""))
            .to_path_buf();
        by_dir.entry(dir).or_default().push(entry);
    }

    // The directory the compose probe's graph belongs to: the checked path itself when it is a
    // directory, else the checked file's parent. Only that group may reuse it — another
    // directory could resolve a different (nested) manifest.
    let probe_dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .unwrap_or_else(|| std::path::Path::new(""))
            .to_path_buf()
    };
    let mut unreadable = false;
    for (dir, dir_entries) in &by_dir {
        // Resolve the directory's dependency packages so a cross-package `use <dep-key>.…`
        // type-checks accurately under `noeta check`, matching `run` (package-manager P2.1c).
        // One resolve serves every entry in the directory (they share the manifest) — the compose
        // probe's graph for the probed directory itself (audit-5 F2); a failure is still reported
        // per entry — like an unreadable file, it doesn't abort the check.
        let reusable = if *dir == probe_dir {
            resolved.take()
        } else {
            None
        };
        let (deps, package_uses) = match reusable {
            Some(graph) => (graph.packages, graph.package_uses),
            None => match graph::resolve_graph(dir_entries[0]) {
                Ok(graph) => (graph.packages, graph.package_uses),
                Err(err) => {
                    for entry in dir_entries {
                        eprintln!("noeta: {}: {err}", entry.display());
                    }
                    unreadable = true;
                    continue;
                }
            },
        };
        // Read + lex + parse the directory once; all entries share the parsed pool and one
        // SourceMap (ids are directory-stable, and the dedup key never uses them).
        let parsed = noeta_loader::parse_dir(
            noeta_loader::read_dir_modules(dir),
            manifest::root_edition(dir_entries[0]),
            &deps,
        );
        let sources = std::rc::Rc::new(parsed.source_map());
        // Check one entry of a parsed directory: link it against the shared pool, then
        // activate the resolved dev-tiers before checking, as `run`/`build`/`dump` do (with no
        // active tiers the program is checked as-is). Checking rides `check_under` with the
        // directory's `CheckOptions`, so the per-source editions and package provenance travel
        // structurally (audit-3 F8).
        // Entry lex/parse errors' spans live in the entry, and the shared map renders them
        // against it — the same (file, span, code) dedup key as the per-entry load produced.
        let mut check_entry =
            |parsed: &noeta_loader::ParsedDir, shared: &std::rc::Rc<SourceMap>, index: usize| {
                match parsed.link_entry(index) {
                    Err(load_diagnostics) => {
                        for ld in &load_diagnostics {
                            fold(shared, &ld.diagnostic);
                        }
                    }
                    Ok(linked) => {
                        // An entry whose directive expanded at compile time has sources of its own —
                        // the generated declarations — that the directory's shared map does not hold,
                        // so it renders against its own extended map and edition map. That is the rare
                        // path: with no expanding directive (nearly every program) `expansions` is
                        // empty and the shared `Rc<SourceMap>` is reused untouched, so this loop —
                        // once per file in the directory — still clones nothing.
                        let (sources, editions);
                        if linked.expansions.is_empty() {
                            sources = std::rc::Rc::clone(shared);
                            editions = None;
                        } else {
                            sources = std::rc::Rc::new(parsed.source_map_with(&linked.expansions));
                            editions = Some(parsed.editions_with(&linked.expansions));
                        }
                        // Package provenance needs no expansion twin: generated sources are
                        // deliberately unattributed, so the directory's map covers every source the
                        // orphan rule may judge (see `ParsedDir::packages`).
                        let opts = noeta_check::CheckOptions {
                            editions: editions.unwrap_or_else(|| parsed.editions().clone()),
                            packages: parsed.packages().clone(),
                            package_uses: package_uses.clone(),
                            ..noeta_check::CheckOptions::default()
                        };
                        let program = linked.program;
                        let program_diags = if active_refs.is_empty() {
                            crate::context::check_under(&program, &opts).diagnostics
                        } else {
                            let ctx = noeta_check::TierContext {
                                uses: &opts.package_uses,
                                packages: &opts.packages,
                            };
                            let activated =
                                noeta_check::activate_tiers_with(&program, &active_refs, &ctx);
                            let mut ds = activated.diagnostics;
                            ds.extend(
                                crate::context::check_under(&activated.program, &opts).diagnostics,
                            );
                            ds
                        };
                        for d in &program_diags {
                            fold(&sources, d);
                        }
                    }
                }
            };
        for entry in dir_entries {
            let name = entry.display().to_string();
            match parsed.module_index(&name) {
                Some(index) => check_entry(&parsed, &sources, index),
                // An entry the directory scan didn't yield: either the file itself is
                // unreadable (report it — one unreadable file does not abort the whole run) or
                // the scan can't see it (an unreadable parent); then the entry links alone,
                // exactly as the per-entry sibling scan degrades.
                None => match std::fs::read_to_string(entry) {
                    Err(err) => {
                        eprintln!("noeta: cannot read {}: {err}", entry.display());
                        unreadable = true;
                    }
                    Ok(text) => {
                        let lone = noeta_loader::parse_dir(
                            vec![noeta_loader::RawModule { name, text }],
                            manifest::root_edition(entry),
                            &deps,
                        );
                        let lone_sources = std::rc::Rc::new(lone.source_map());
                        check_entry(&lone, &lone_sources, 0);
                    }
                },
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
                // Skip dotted directories, matching `noeta fmt`'s walker. `.git` is the obvious
                // one, but the case that actually bites is `.claude/worktrees/` — a git worktree
                // holds a SECOND copy of every module, so checking a package (or a path/patched
                // dependency) swept an agent's in-progress branch into the same program and
                // reported its errors against a consumer that never referenced it.
                if p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'))
                {
                    continue;
                }
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
