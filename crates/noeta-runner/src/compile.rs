//! The L2 compile front-end (dev-deps D3c): a source file → its runnable [`Module`]. Extracted from
//! `noeta-cli` so the CLI's `run`/`dump`/`build` path and the standalone lean `noeta-runner` binary
//! compile through ONE implementation — the drift firewall — and a source deploy (PHP-style) never
//! links the dev toolchain (L3). Nothing here reaches `noeta-fmt`/`-lsp`/`-dap`/`-mcp` or a
//! formatter parser; `noeta-pm` is present for manifest + target/tier resolution only.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use noeta_diagnostics::{Diagnostic, render, render_mapped};
use noeta_pm::manifest;
use noeta_span::{Source, SourceId, SourceMap};

/// A resolved startup-cache slot: an open cache, the content key for this program, and the workspace
/// `SourceMap` (so a cache hit renders diagnostics against real source without re-parsing).
struct CacheSlot {
    cache: noeta_cache::Cache,
    key: noeta_cache::CacheKey,
    sources: SourceMap,
}

/// A compiled whole-file program: the runnable module plus the sources its spans resolve against.
#[derive(Debug)]
pub struct Compiled {
    pub module: Arc<noeta_bytecode::Module>,
    pub sources: SourceMap,
}

/// A whole-file compile failure, carrying what's needed to render it. [`report`](Self::report)
/// prints it and yields the process exit code, matching each command's prior behavior.
#[derive(Debug)]
pub enum CompileFailure {
    /// A message rendered as `lang: {0}` with exit 1 (target resolution / compiler-internal error).
    Message(String),
    /// The entry file could not be read (exit 2).
    Unreadable(String),
    /// Load-time (lex/parse) diagnostics, each paired with its own source (exit 1).
    Load(Vec<noeta_loader::LoadDiagnostic>),
    /// Tier-activation or type-check diagnostics, rendered against `sources` (exit 1).
    Diagnostics {
        sources: SourceMap,
        diagnostics: Vec<Diagnostic>,
    },
}

impl CompileFailure {
    /// Print the failure to stderr and return the process exit code.
    pub fn report(&self) -> std::process::ExitCode {
        use std::process::ExitCode;
        match self {
            CompileFailure::Message(msg) => {
                eprintln!("lang: {msg}");
                ExitCode::from(1)
            }
            CompileFailure::Unreadable(msg) => {
                eprintln!("lang: {msg}");
                ExitCode::from(2)
            }
            CompileFailure::Load(diagnostics) => {
                let mut stderr = std::io::stderr();
                for ld in diagnostics {
                    let _ = stderr.write_all(render(&ld.source, &ld.diagnostic).as_bytes());
                }
                ExitCode::from(1)
            }
            CompileFailure::Diagnostics {
                sources,
                diagnostics,
            } => {
                let _ = std::io::stderr()
                    .write_all(render_mapped(sources, diagnostics.iter()).as_bytes());
                ExitCode::from(1)
            }
        }
    }
}

/// Resolve a target's tier → provider map (provider dispatch), or an empty map when no target is
/// selected. Shared by the compile pipeline and (via re-export) the CLI's other commands.
pub fn resolve_providers(
    entry: &Path,
    target: &Option<String>,
) -> Result<BTreeMap<String, String>, String> {
    match target {
        None => Ok(BTreeMap::new()),
        Some(name) => manifest::resolve_active_tier_providers(entry, name),
    }
}

/// Compile an already-typechecked program straight to a bytecode [`Module`] for the real (VM)
/// execution path (isolates I.4a). Runs the same Core-IR lowering + precise-RC drop + reuse passes,
/// then IR → bytecode. Every program that parses and type-checks compiles to bytecode (the
/// differential holds the VM at 100% coverage by construction), so an `Err` here is an internal
/// invariant break, surfaced rather than silently downgraded.
pub fn compile_real(
    program: &noeta_ast::Program,
    checked: &noeta_check::Checked,
) -> Result<noeta_bytecode::Module, String> {
    noeta_compiler::compile_with_sites(
        program,
        checked.sites.clone(),
        // Real execution runs isolates on OS threads (I.4b): lower `isolate f(args)` to
        // `SpawnIsolate`. The differential/salsa paths pass false (byte-identical cooperative sandbox).
        true,
        // A production compile — no debug info (the debugger's `noeta dap` compiles with debug = true).
        false,
    )
    .map_err(|u| {
        format!(
            "internal error: the VM cannot compile this program: {}",
            u.reason
        )
    })
}

/// The whole-file compile pipeline, shared by `run`/`dump`/`build` and cache-aware in one place: any
/// command (or the standalone runner) that wants "a source file → its runnable [`Module`]" goes
/// through here, so the startup cache is applied exactly once.
///
/// Resolves the active tier set (target ∪ `--tier`), then consults the startup cache: on a **hit**
/// the decoded module is returned directly (the whole front-end is skipped); on a **miss** it loads →
/// activates tiers → type-checks → compiles, populates the cache (best-effort), and returns.
pub fn compile_whole_file(
    file: &Path,
    tiers: &[String],
    target: &Option<String>,
    no_cache: bool,
) -> Result<Compiled, CompileFailure> {
    // The active tier set is the union of any `--target`'s live tiers (from `noeta.toml`) and any
    // explicit `--tier` flags, resolved before loading so a bad target fails fast.
    let mut active: Vec<String> = match target {
        Some(name) => {
            manifest::resolve_active_tiers(file, name).map_err(CompileFailure::Message)?
        }
        None => Vec::new(),
    };
    for tier in tiers {
        if !active.contains(tier) {
            active.push(tier.clone());
        }
    }

    // The target's tier → provider map (provider dispatch): decides which declaration's config
    // attribute activation stamps, so it is part of the compiled program — and of the cache key.
    let providers = resolve_providers(file, target).map_err(CompileFailure::Message)?;

    // The entry's dependency packages (package-manager P2.1): their sources feed both the cache key
    // (so a dep change never serves stale bytecode) and the loader (so `use <dep-key>.…` resolves).
    let deps = manifest::dependency_packages(file).map_err(CompileFailure::Message)?;

    // The entry's effective language edition (follow-on F1) — part of the compilation identity, so it
    // is folded into the cache key below.
    let edition = manifest::root_edition(file);

    // Startup cache (M3): on a hit, return the cached module — load/check/compile all skipped.
    let cache = open_startup_cache(file, &active, &providers, &deps, edition, no_cache);
    if let Some(slot) = &cache
        && let Some(blob) = slot.cache.load(&slot.key)
        && let Ok(module) = noeta_bundle::read(&blob)
    {
        return Ok(Compiled {
            module: Arc::new(module),
            sources: slot.sources.clone(),
        });
    }

    // Miss: load + link (sibling `.noe` modules the entry `use`s are resolved and merged; a lone file
    // links to itself; dependency packages re-rooted under their keys), activate any dev-tiers,
    // type-check, and compile to bytecode.
    let linked = match noeta_loader::load_with_deps(file, edition, &deps) {
        Err(err) => {
            return Err(CompileFailure::Unreadable(format!(
                "cannot read {}: {err}",
                file.display()
            )));
        }
        Ok(Err(load_diagnostics)) => return Err(CompileFailure::Load(load_diagnostics)),
        Ok(Ok(linked)) => linked,
    };
    let sources = linked.sources;
    // Which edition governs each source (entry/siblings = root package's; each dependency's own),
    // keyed by `SourceId` — the checker recovers a declaration's edition from its span. Captured
    // before `linked.program` is moved below; SourceIds survive tier activation, so the map stays
    // valid against the activated program.
    let editions = linked.editions;
    // Activation inlines each `@<tier> { … }` block; with no active tiers the program runs as-is and
    // every tier block is stripped at lowering (the default). Activation is only done when needed.
    let program = if active.is_empty() {
        linked.program
    } else {
        let active_refs: Vec<&str> = active.iter().map(String::as_str).collect();
        let activated = noeta_check::activate_tiers_with(&linked.program, &active_refs, &providers);
        if !activated.diagnostics.is_empty() {
            return Err(CompileFailure::Diagnostics {
                sources,
                diagnostics: activated.diagnostics,
            });
        }
        activated.program
    };
    let checked = noeta_check::check_all_with_editions(&program, editions);
    if !checked.diagnostics.is_empty() {
        return Err(CompileFailure::Diagnostics {
            sources,
            diagnostics: checked.diagnostics,
        });
    }
    let module = match compile_real(&program, &checked) {
        Ok(module) => Arc::new(module),
        Err(err) => return Err(CompileFailure::Message(err)),
    };

    // Populate the cache, best-effort, then bound its size (oldest-first eviction). Both run only on
    // this already-slow miss path. Panic-isolated: a cache write must never abort an otherwise-
    // successful run (`noeta_bundle::write`'s postcard encode carries an `.expect`). `AssertUnwindSafe`:
    // on unwind we observe none of the captured state (`slot`/`module` are only read then discarded).
    if let Some(slot) = &cache {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = slot.cache.store(&slot.key, &noeta_bundle::write(&module));
            let _ = slot.cache.prune_to(noeta_cache::max_bytes());
        }));
    }
    Ok(Compiled { module, sources })
}

/// Build the startup-cache slot for a source run: open the cache and compute the content key from
/// the raw workspace (entry + sibling module sources) + runtime version + binary identity + the
/// active tier set. Returns `None` — meaning "run uncached" — when caching is disabled
/// (`--no-cache` or `NOETA_NO_CACHE`), the running binary can't be identified, the entry can't be
/// read, or the cache directory can't be opened.
fn open_startup_cache(
    file: &Path,
    active: &[String],
    providers: &BTreeMap<String, String>,
    deps: &[noeta_loader::DepPackage],
    edition: noeta_pm::edition::Edition,
    no_cache: bool,
) -> Option<CacheSlot> {
    if no_cache || std::env::var_os("NOETA_NO_CACHE").is_some() {
        return None;
    }
    // The binary's build identity is mandatory: without it a same-version local toolchain rebuild
    // would reuse stale bytecode. If we can't obtain it, we must not cache.
    let binary = noeta_cache::binary_identity()?;
    // Read the entry + sibling sources (no lex/parse) — both the key material and, on a hit, the
    // SourceMap for rendering. SourceIds here match `noeta_loader::load_with_deps`'s assignment.
    let workspace = noeta_loader::read_workspace(file).ok()?;
    let mut key = noeta_cache::KeyBuilder::new();
    // Which file is the *entry* is part of the key, not just the source set: a directory of dir-flat
    // modules compiles to a different program per entry, so `noeta run a.noe` and `noeta run b.noe`
    // in one directory must not collide.
    key.entry(source_key_name(&workspace.entry));
    key.source(
        source_key_name(&workspace.entry),
        workspace.entry.text().as_bytes(),
    );
    for module in &workspace.modules {
        key.source(source_key_name(module), module.text().as_bytes());
    }
    // Dependency packages are part of the compiled program: fold each dependency's identity, edition,
    // and sources into the key so any dependency change invalidates the cache.
    key_deps(&mut key, deps);
    // The **root** package's edition is part of the compilation identity (follow-on F1): a future
    // edition that changes what the front-end accepts or how it lowers must not reuse another
    // edition's cached bytecode, so the entry's effective edition is key material. (Each *dependency's*
    // edition is folded per-dep by `key_deps`.) Distinct from tier names.
    key.source("<edition>", edition.as_str().as_bytes());
    key.runtime_version(noeta_bundle::RUNTIME_VERSION)
        .binary_identity(binary);
    for tier in active {
        key.tier(tier);
    }
    // The provider selection changes what activation stamps, so it is key material — encoded
    // distinctly from bare tier names; `BTreeMap` iteration keeps it deterministic.
    for (tier, provider) in providers {
        key.tier(format!("{tier}={provider}"));
    }
    let key = key.finish();

    let cache = noeta_cache::Cache::open()?;
    // Rebuild the exact Source sequence `load_with_deps` assigns SourceIds to, so a cached module's
    // spans resolve. `read_workspace` gave entry (id 0) + siblings; dependency modules continue the
    // ids in the same order the loader parses them.
    let mut sources = Vec::with_capacity(1 + workspace.modules.len());
    sources.push(workspace.entry);
    sources.extend(workspace.modules);
    let mut next_id = sources.len() as u32;
    for dep in deps {
        for module in &dep.modules {
            sources.push(Source::new(SourceId(next_id), &module.name, &module.text));
            next_id += 1;
        }
    }
    Some(CacheSlot {
        cache,
        key,
        sources: SourceMap::new(sources),
    })
}

/// The cache-key name for a source: its file name, so the key is independent of the path the program
/// was invoked through (`./app.noe` and `app.noe` share an entry).
fn source_key_name(source: &Source) -> &str {
    Path::new(source.name())
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_else(|| source.name())
}

/// Fold the dependency packages into the startup-cache key: each dependency's key→root binding
/// (re-rooting changes the linked program even when the sources are byte-identical), its **edition**,
/// its local dependency renames, and every module's source text.
///
/// A dependency's edition is key material for the same reason its sources are (editions arc S2): it
/// changes how *that* package compiles, so a dep whose `noeta.toml` edition bumps must invalidate its
/// cached bytecode even when the dep's source bytes are unchanged. Before S2 only the *root* package's
/// edition was keyed, so a dependency-edition change could serve a stale artifact.
fn key_deps(key: &mut noeta_cache::KeyBuilder, deps: &[noeta_loader::DepPackage]) {
    for dep in deps {
        key.source(format!("<dep {}>", dep.key), dep.root.as_bytes());
        key.source(format!("<dep-edition {}>", dep.key), dep.edition.as_bytes());
        for (local, global) in &dep.dep_renames {
            key.source(format!("<rename {} {local}>", dep.key), global.as_bytes());
        }
        for module in &dep.modules {
            key.source(&module.name, module.text.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep_at_edition(edition: &str) -> noeta_loader::DepPackage {
        noeta_loader::DepPackage {
            key: "lib".to_string(),
            root: "lib".to_string(),
            modules: vec![noeta_loader::RawModule {
                name: "lib.noe".to_string(),
                text: "namespace lib.api;\npub fn f(): int { return 1; }\n".to_string(),
            }],
            dep_renames: Default::default(),
            native: false,
            edition: edition.to_string(),
        }
    }

    fn key_of(deps: &[noeta_loader::DepPackage]) -> String {
        let mut key = noeta_cache::KeyBuilder::new();
        key_deps(&mut key, deps);
        key.finish().as_hex().to_string()
    }

    #[test]
    fn a_dependencys_edition_is_part_of_the_cache_key() {
        // Two dependency sets with byte-identical sources, differing only in the dependency's edition,
        // must produce different cache keys — otherwise a dep-edition bump would serve stale bytecode
        // (editions arc S2). Uses raw edition strings, so it does not depend on which editions the
        // toolchain currently accepts.
        let key_2026 = key_of(std::slice::from_ref(&dep_at_edition("2026")));
        let key_other = key_of(std::slice::from_ref(&dep_at_edition("2099")));
        assert_ne!(
            key_2026, key_other,
            "a dependency's edition change must change the cache key"
        );

        // And identical inputs still produce the same key (the change above is the edition, nothing else).
        assert_eq!(
            key_2026,
            key_of(std::slice::from_ref(&dep_at_edition("2026")))
        );
    }
}
