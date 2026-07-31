//! The L2 compile front-end (dev-deps D3c): a source file → its runnable [`Module`]. Extracted from
//! `noeta-cli` so the CLI's `run`/`dump`/`build` path and the standalone lean `noeta-runner` binary
//! compile through ONE implementation — the drift firewall — and a source deploy (PHP-style) never
//! links the dev toolchain (L3). Nothing here reaches `noeta-fmt`/`-lsp`/`-dap`/`-mcp` or a
//! formatter parser; `noeta-pm` is present for manifest + target/tier resolution only.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use noeta_diagnostics::{Diagnostic, has_errors, render, render_mapped};
use noeta_pm::manifest;
use noeta_span::{Source, SourceMap};

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
    /// The non-blocking diagnostics (warnings/notes) the compile produced — tier activation's and
    /// the type-check's. A warning describes the program without condemning it, so it never fails
    /// the compile; it rides out here instead, and the caller renders it against `sources` before
    /// doing whatever it does with the module. Never dropped: proceeding must not mean going quiet.
    pub warnings: Vec<Diagnostic>,
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
    /// A bytecode-backend [`Unsupported`](noeta_compiler::Unsupported) as a reportable failure.
    ///
    /// When the compiler knew where it stopped, this is a real diagnostic rendered against real
    /// source — the same `ariadne` output a type error gets, with the offending construct under a
    /// caret. That is the whole point: before it, an internal invariant break arrived as one line
    /// of prose with no file and no line, which reads as a broken toolchain rather than as one
    /// expression in one function. A span-less `Unsupported` still degrades to that line, which is
    /// the honest rendering when there is nothing to point at.
    pub fn from_unsupported(
        sources: &SourceMap,
        unsupported: &noeta_compiler::Unsupported,
    ) -> CompileFailure {
        match unsupported.diagnostic() {
            Some(diagnostic) => CompileFailure::Diagnostics {
                sources: sources.clone(),
                diagnostics: vec![diagnostic],
            },
            None => CompileFailure::Message(unsupported.to_string()),
        }
    }

    /// The failure as renderable text plus its process exit code — for front-ends that replay
    /// failures over a wire (the DAP's `output` events, MCP tool results) instead of printing.
    pub fn to_text(&self) -> (String, u8) {
        match self {
            CompileFailure::Message(msg) => (format!("noeta: {msg}\n"), 1),
            CompileFailure::Unreadable(msg) => (format!("noeta: {msg}\n"), 2),
            CompileFailure::Load(diagnostics) => {
                let mut text = String::new();
                for ld in diagnostics {
                    text.push_str(&render(&ld.source, &ld.diagnostic));
                }
                (text, 1)
            }
            CompileFailure::Diagnostics {
                sources,
                diagnostics,
            } => (render_mapped(sources, diagnostics.iter()), 1),
        }
    }

    /// Print the failure to stderr and return the process exit code.
    pub fn report(&self) -> std::process::ExitCode {
        let (text, code) = self.to_text();
        let _ = std::io::stderr().write_all(text.as_bytes());
        std::process::ExitCode::from(code)
    }
}

/// Resolve the root package's tier → provider map (provider dispatch) from its `[tiers]` table — who
/// provides each tier the root names, **independent of the build target** (a tier's provider is
/// package-level; the target only selects which are live). A bare script with no manifest yields an
/// empty map (its tiers resolve ambiently). The `target` is accepted for call-site symmetry but no
/// longer selects providers. Shared by the compile pipeline and (via re-export) the CLI's commands.
pub fn resolve_providers(
    entry: &Path,
    _target: &Option<String>,
) -> Result<BTreeMap<String, String>, String> {
    manifest::resolve_tier_providers(entry).map_err(|err| err.to_string())
}

/// Compile an already-typechecked program straight to a bytecode [`Module`] for the real (VM)
/// execution path (isolates I.4a). Runs the same Core-IR lowering + precise-RC drop + reuse passes,
/// then IR → bytecode. Every program that parses and type-checks compiles to bytecode (the
/// differential holds the VM at 100% coverage by construction), so an `Err` here is an internal
/// invariant break, surfaced rather than silently downgraded.
///
/// The `Err` is the compiler's own [`Unsupported`](noeta_compiler::Unsupported), **not** a
/// pre-rendered string: it carries the span, and a caller holding the program's [`SourceMap`] turns
/// it into a real diagnostic with [`CompileFailure::from_unsupported`]. A caller with no source map
/// falls back to `to_string()`, which is the old one-line rendering.
pub fn compile_real(
    program: &noeta_ast::Program,
    checked: &noeta_check::Checked,
) -> Result<noeta_bytecode::Module, noeta_compiler::Unsupported> {
    noeta_compiler::compile_with_sites(
        program,
        checked.sites.clone(),
        // Real execution runs isolates on OS threads (I.4b): lower `isolate f(args)` to
        // `SpawnIsolate`. The differential/salsa paths pass false (byte-identical cooperative sandbox).
        true,
        // A production compile — no debug info (the debugger's `noeta dap` compiles with debug = true).
        false,
    )
}

/// The resolved **selection facts** for an entry — everything the front-end decides from manifests
/// alone, before any source is lexed: the active tier set (target ∪ explicit tiers), the target's
/// tier → provider map, the dependency packages, and the root package's edition. Resolved ONCE and
/// shared by the cache key and the loader, so no consumer can pick a divergent subset (the drift
/// that left the debugger/profiler unable to see dependency packages).
#[derive(Debug)]
pub struct FrontFacts {
    pub active: Vec<String>,
    pub providers: BTreeMap<String, String>,
    pub deps: Vec<noeta_loader::DepPackage>,
    /// The whole program's per-package `@`-name resolution tables (`[directives]`; `[tiers]` later),
    /// keyed by [`noeta_span::PackageOrigin`]. Resolved with the dependency graph and carried to the
    /// checker so a `@name` resolves in the package that wrote it.
    pub package_uses: noeta_span::PackageUses,
    pub edition: noeta_pm::edition::Edition,
}

/// A dependency graph a single invocation already resolved (the compose probe) and hands back so the
/// command path does not resolve it twice — the re-rooted packages the loader links, plus the whole
/// program's per-package `@`-name tables ([`resolve_front_with`]).
#[derive(Debug)]
pub struct ResolvedFront {
    pub packages: Vec<noeta_loader::DepPackage>,
    pub package_uses: noeta_span::PackageUses,
}

/// Resolve the selection facts for `file` (see [`FrontFacts`]). A bad target fails fast, before
/// anything loads.
pub fn resolve_front(
    file: &Path,
    tiers: &[String],
    target: &Option<String>,
) -> Result<FrontFacts, CompileFailure> {
    resolve_front_with(file, tiers, target, None)
}

/// As [`resolve_front`], optionally reusing a dependency graph the **same invocation** already
/// resolved under the default selection (audit-5 F2): the CLI's `compose::maybe_delegate` fully
/// resolves the graph to decide whether to delegate to a composed toolchain, and hands it back
/// when it doesn't — the command path must not resolve the identical graph a second time.
pub fn resolve_front_with(
    file: &Path,
    tiers: &[String],
    target: &Option<String>,
    reused: Option<ResolvedFront>,
) -> Result<FrontFacts, CompileFailure> {
    // The active tier set is the union of any `--target`'s live tiers (from `noeta.toml`) and any
    // explicit `--tier` flags.
    let mut active: Vec<String> = match target {
        Some(name) => manifest::resolve_active_tiers(file, name)
            .map_err(|err| CompileFailure::Message(err.to_string()))?,
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
    // The entry's dependency packages (package-manager P2.1), resolved for the selected target
    // (dev-deps D2: `[targets.<name>.dependencies]` layer onto the globals). Their sources feed
    // both the cache key (so a dep or target-dep change never serves stale bytecode — the dep
    // fold covers the content, so the target name itself needs no extra key material) and the
    // loader (so `use <dep-key>.…` resolves).
    let (deps, package_uses) = match (target, reused) {
        // The caller's pre-resolved graph IS this selection (both are the default,
        // lock-refreshing resolve) — reuse it rather than resolving the same graph twice.
        (None, Some(reused)) => (reused.packages, reused.package_uses),
        // A `--target` layers `[targets.<name>.dependencies]` onto the globals — a legitimately
        // *different* selection than the compose probe's default resolve, so the target path
        // re-resolves rather than contorting the probe to anticipate every target (audit-5 F2).
        _ => manifest::dependency_selection_for(file, target.as_deref())
            .map_err(|err| CompileFailure::Message(err.to_string()))?,
    };
    // The entry's effective language edition (follow-on F1) — part of the compilation identity.
    let edition = manifest::root_edition(file);
    Ok(FrontFacts {
        active,
        providers,
        deps,
        package_uses,
        edition,
    })
}

/// A loaded, linked, tier-activated program, ready to type-check — the shared *front half* every
/// program-taking tool goes through (run/dump/build via [`compile_whole_file`]; the debugger,
/// profiler, agent debug tools, and REPL bootstrap via [`load_default_project`]), so they all see
/// the same dependency packages, tier activation, and per-source editions as `noeta run`.
#[derive(Debug)]
pub struct Loaded {
    pub program: noeta_ast::Program,
    pub sources: SourceMap,
    /// Which edition governs each source (entry/siblings = root package's; each dependency's own),
    /// keyed by `SourceId`. SourceIds survive tier activation, so the map stays valid against the
    /// activated program.
    pub editions: noeta_edition::EditionMap,
    /// Which **package** each source came from, keyed by `SourceId` (the loader's `Linked::packages`)
    /// — the provenance the package orphan rule (E0070) reads. `SourceId`s survive tier activation,
    /// so the map stays valid against the activated program, exactly as [`Loaded::editions`] does.
    pub packages: noeta_span::PackageMap,
    /// The per-package `@`-name resolution tables (`[directives]`; `[tiers]` later), keyed by
    /// [`noeta_span::PackageOrigin`] — read by the checker via a span's `SourceId` so a `@name`
    /// resolves in the package that wrote it. Carried alongside `packages`, from the same resolve.
    pub package_uses: noeta_span::PackageUses,
    /// Non-blocking diagnostics the front half already produced (tier activation's warnings), to be
    /// rendered alongside whatever the later type-check reports. Carried rather than dropped for the
    /// same reason [`Compiled::warnings`] is.
    pub warnings: Vec<Diagnostic>,
}

impl Loaded {
    /// The check configuration this loaded program carries: its per-source editions and package
    /// provenance. One place, so no caller can thread half of it (a `check_all` alone would silently
    /// drop both, and the orphan rule would then never fire on a real dependency graph).
    fn check_options(&self) -> noeta_check::CheckOptions {
        noeta_check::CheckOptions {
            editions: self.editions.clone(),
            packages: self.packages.clone(),
            package_uses: self.package_uses.clone(),
            ..noeta_check::CheckOptions::default()
        }
    }

    /// Type-check the loaded program under its per-source editions and package provenance — the one
    /// blessed way, so no caller can forget to thread them.
    pub fn check(&self) -> noeta_check::Checked {
        noeta_check::check_all_with(&self.program, self.check_options())
    }

    /// As [`Loaded::check`], but the session flavor: keeps the [`noeta_check::SessionChecker`]
    /// alive so REPL/debug-console fragments extend the whole-program typing environment.
    pub fn check_session(&self) -> (noeta_check::Checked, noeta_check::SessionChecker) {
        noeta_check::check_all_session_opts(&self.program, self.check_options())
    }
}

/// Load + link `file` (sibling `.noe` modules the entry `use`s resolved and merged; a lone file
/// links to itself; dependency packages re-rooted under their keys) and activate the selected
/// tiers. The back half of the pipeline behind [`compile_whole_file`]'s cache probe.
pub fn load_project(file: &Path, facts: &FrontFacts) -> Result<Loaded, CompileFailure> {
    // The front-end (loader/checker/compiler) consumes the extension registry as data and does not
    // link the std units (audit-6 F2) — the assembling driver seeds. A composed binary's earlier
    // explicit `install` wins; after any install this is a no-op.
    noeta_stdlib::registry::default_seeded();
    let linked = load_linked(file, facts)?;
    let sources = linked.sources;
    let editions = linked.editions;
    let packages = linked.packages;
    // Activation inlines each `@<tier> { … }` block; with no active tiers the program runs as-is and
    // every tier block is stripped at lowering (the default). Activation is only done when needed.
    let (program, warnings) = if facts.active.is_empty() {
        (linked.program, Vec::new())
    } else {
        let active_refs: Vec<&str> = facts.active.iter().map(String::as_str).collect();
        // Activation resolves each `@name` per the package that wrote it (per-package naming arc):
        // the whole-program `[tiers]`/`[directives]` bindings and the span→package map.
        let ctx = noeta_check::TierContext {
            uses: &facts.package_uses,
            packages: &packages,
        };
        let mut activated = noeta_check::activate_tiers_with(&linked.program, &active_refs, &ctx);
        // Only an *error* stops the load; anything advisory rides out on `Loaded::warnings` for the
        // caller to report.
        if has_errors(&activated.diagnostics) {
            return Err(CompileFailure::Diagnostics {
                sources,
                diagnostics: activated.diagnostics,
            });
        }
        (
            activated.program,
            std::mem::take(&mut activated.diagnostics),
        )
    };
    Ok(Loaded {
        program,
        sources,
        editions,
        packages,
        package_uses: facts.package_uses.clone(),
        warnings,
    })
}

/// The raw load + link step under `facts` — [`load_project`] without tier activation, for callers
/// that stage activation themselves (the CLI's test/bench/doc prologue resolves providers per
/// verb before activating).
pub fn load_linked(
    file: &Path,
    facts: &FrontFacts,
) -> Result<noeta_loader::Linked, CompileFailure> {
    match noeta_loader::load_with_deps(
        file,
        facts.edition,
        &facts.deps,
        &facts.package_uses,
        noeta_pm::sources::package_root(file).as_ref(),
    ) {
        Err(err) => Err(CompileFailure::Unreadable(format!(
            "cannot read {}: {err}",
            file.display()
        ))),
        Ok(Err(load_diagnostics)) => Err(CompileFailure::Load(load_diagnostics)),
        Ok(Ok(linked)) => Ok(linked),
    }
}

/// [`load_project`] under the **default selection** (no `--target`/`--tier`): what the debugger,
/// profiler, agent debug tools, and REPL bootstrap call — dependency packages and the package
/// edition resolve exactly as `noeta run`'s pipeline resolves them (the drift firewall), with no
/// startup cache (their compiles are bespoke: debug info, session compilers).
pub fn load_default_project(file: &Path) -> Result<Loaded, CompileFailure> {
    load_default_project_with(file, None)
}

/// As [`load_default_project`], optionally reusing an already-resolved default-selection
/// dependency graph (see [`resolve_front_with`]) — the REPL bootstrap's compose probe resolves
/// the same graph moments earlier.
pub fn load_default_project_with(
    file: &Path,
    reused: Option<ResolvedFront>,
) -> Result<Loaded, CompileFailure> {
    let facts = resolve_front_with(file, &[], &None, reused)?;
    load_project(file, &facts)
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
    compile_whole_file_with(file, tiers, target, no_cache, None)
}

/// As [`compile_whole_file`], optionally reusing an already-resolved default-selection
/// dependency graph (see [`resolve_front_with`]) — how `run`/`dump`/`build` avoid resolving the
/// graph twice per invocation after their compose probe (audit-5 F2).
pub fn compile_whole_file_with(
    file: &Path,
    tiers: &[String],
    target: &Option<String>,
    no_cache: bool,
    reused: Option<ResolvedFront>,
) -> Result<Compiled, CompileFailure> {
    let facts = resolve_front_with(file, tiers, target, reused)?;

    // Startup cache (M3): on a hit, return the cached module — load/check/compile all skipped.
    let cache = open_startup_cache(
        file,
        &facts.active,
        &facts.providers,
        &facts.deps,
        facts.edition,
        no_cache,
    );
    if let Some(slot) = &cache
        && let Some(blob) = slot.cache.load(&slot.key)
        && let Ok(module) = noeta_bundle::read(&blob)
    {
        return Ok(Compiled {
            module: Arc::new(module),
            sources: slot.sources.clone(),
            // A program that warns is never *stored* (see below), so a hit is by construction a
            // warning-free program — there is nothing the skipped front-end would have said.
            warnings: Vec::new(),
        });
    }

    // Miss: load → link → activate → check → compile.
    let loaded = load_project(file, &facts)?;
    let checked = loaded.check();
    // Errors block the compile; warnings do not — they ride out on `Compiled::warnings`. A
    // well-formed program must still produce a module (and run), or every advisory lint would be a
    // hard stop.
    if has_errors(&checked.diagnostics) {
        return Err(CompileFailure::Diagnostics {
            sources: loaded.sources,
            diagnostics: checked.diagnostics,
        });
    }
    let mut warnings = loaded.warnings.clone();
    warnings.extend(checked.diagnostics.iter().cloned());
    let module = match compile_real(&loaded.program, &checked) {
        Ok(module) => Arc::new(module),
        // The source map is already in hand here — nothing to thread — so the run path renders an
        // internal compile failure exactly like a type error.
        Err(u) => return Err(CompileFailure::from_unsupported(&loaded.sources, &u)),
    };
    let sources = loaded.sources;

    // Populate the cache, best-effort, then bound its size (oldest-first eviction). Both run only on
    // this already-slow miss path. Panic-isolated: a cache write must never abort an otherwise-
    // successful run (`noeta_bundle::write`'s postcard encode carries an `.expect`). `AssertUnwindSafe`:
    // on unwind we observe none of the captured state (`slot`/`module` are only read then discarded).
    // A program that warns is deliberately *not* cached: the cache short-circuits the whole
    // front-end, so a stored module would make the warning appear on the first run and never again —
    // a lint you cannot see is worse than no lint. Warning-free programs (the overwhelming majority,
    // and every program once its warnings are addressed) still get the fast path.
    if let Some(slot) = &cache
        && warnings.is_empty()
    {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = slot.cache.store(&slot.key, &noeta_bundle::write(&module));
            let _ = slot.cache.prune_to(noeta_cache::max_bytes());
        }));
    }
    Ok(Compiled {
        module,
        sources,
        warnings,
    })
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
    let workspace =
        noeta_loader::read_workspace(file, noeta_pm::sources::package_root(file).as_ref()).ok()?;
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
    // …and each source's **derived module path** ([`noeta_loader::derive`]). A source is keyed by its
    // file *name* on purpose (so `./app.noe` and `app.noe` share an entry), but a module's identity
    // now comes from its whole location: two files named `pieces.noe` in different directories are
    // different modules with identical key material, and moving a file changes what it is without
    // changing a byte of it. Fold the path in, or a move serves the pre-move program back.
    for (index, path) in workspace.paths.iter().enumerate() {
        key.source(
            format!("<module-path {index}>"),
            module_path_key(path).as_bytes(),
        );
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
    // The exact Source sequence `load_with_deps` assigns SourceIds to, so a cached module's spans
    // resolve — built by the LOADER's own `workspace_sources` (the single ordering authority; this
    // used to be a hand-rolled copy held in lockstep by a comment).
    let sources = noeta_loader::workspace_sources(&workspace, deps);
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

/// A derived module path as cache-key material — the dotted path, or a distinct marker for the two
/// non-derived outcomes (no package context; a name that cannot be a path segment), so they key
/// differently from each other and from every real path.
fn module_path_key(path: &noeta_loader::ModulePath) -> String {
    match path {
        noeta_loader::ModulePath::Declared => "<declared>".to_string(),
        noeta_loader::ModulePath::Derived(segments) => segments.join("."),
        noeta_loader::ModulePath::Illegal { segment, .. } => format!("<illegal {segment}>"),
    }
}

/// Fold the dependency packages into the startup-cache key: each dependency's root→prefix binding
/// (re-rooting changes the linked program even when the sources are byte-identical), its **edition**,
/// its local dependency renames, and every module's source text.
///
/// A dependency's edition is key material for the same reason its sources are (editions arc S2): it
/// changes how *that* package compiles, so a dep whose `noeta.toml` edition bumps must invalidate its
/// cached bytecode even when the dep's source bytes are unchanged. Before S2 only the *root* package's
/// edition was keyed, so a dependency-edition change could serve a stale artifact.
fn key_deps(key: &mut noeta_cache::KeyBuilder, deps: &[noeta_loader::DepPackage]) {
    for dep in deps {
        // The whole derived prefix, not just the import key: a scope-array member derives (and
        // re-roots to) two segments, so keying the first alone would serve one member's cached
        // bytecode for another.
        let prefix = dep.prefix.join(".");
        key.source(format!("<dep {prefix}>"), dep.root.as_bytes());
        key.source(
            format!("<dep-edition {prefix}>"),
            dep.edition.as_str().as_bytes(),
        );
        for (local, global) in &dep.dep_renames {
            key.source(format!("<rename {prefix} {local}>"), global.as_bytes());
        }
        for module in &dep.modules {
            key.source(&module.name, module.text.as_bytes());
            // A dependency module's identity is its derived path too — a package that moves a file
            // ships a different API under identical bytes.
            key.source(
                format!("<dep-path {}>", module.name),
                module_path_key(&module.path).as_bytes(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_dep() -> noeta_loader::DepPackage {
        noeta_loader::DepPackage {
            prefix: vec!["lib".to_string()],
            root: "lib".to_string(),
            modules: vec![noeta_loader::RawModule {
                path: noeta_loader::ModulePath::Declared,
                name: "lib.noe".to_string(),
                text: "namespace lib.api;\npub fn f(): int { return 1; }\n".to_string(),
            }],
            dep_renames: Default::default(),
            native: false,
            edition: noeta_edition::Edition::DEFAULT,
            directives: Default::default(),
        }
    }

    /// A reused default-selection graph carrying one dep and no `@`-name tables.
    fn a_reused() -> ResolvedFront {
        ResolvedFront {
            packages: vec![a_dep()],
            package_uses: noeta_span::PackageUses::new(),
        }
    }

    fn key_of(deps: &[noeta_loader::DepPackage]) -> String {
        let mut key = noeta_cache::KeyBuilder::new();
        key_deps(&mut key, deps);
        key.finish().as_hex().to_string()
    }

    #[test]
    fn resolve_front_reuses_a_default_selection_graph_and_reresolves_for_a_target() {
        // audit-5 F2: a caller (compose::maybe_delegate) that already resolved the DEFAULT
        // selection hands its deps in; resolve_front_with must adopt them verbatim instead of
        // resolving again…
        let dir = noeta_test_temp::TempDir::new("resolve-once");
        let entry = dir.join("main.noe");
        std::fs::write(&entry, "echo 1\n").unwrap();
        let facts = resolve_front_with(&entry, &[], &None, Some(a_reused()))
            .expect("default selection resolves");
        assert_eq!(
            facts.deps.len(),
            1,
            "the pre-resolved default-selection deps must be adopted, not re-resolved"
        );
        assert_eq!(facts.deps[0].key(), "lib");
        // …but a `--target` is a legitimately different selection ([targets.<name>.dependencies]
        // layer onto the globals), so the pre-resolved deps are ignored and the target selection
        // resolves fresh — here the manifest-less entry's target fails to resolve, proving the
        // handed-in deps were NOT silently used for it.
        let target = Some("dev".to_string());
        assert!(
            resolve_front_with(&entry, &[], &target, Some(a_reused())).is_err(),
            "a --target selection must re-resolve (and here fail: no manifest declares `dev`)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An internal compile failure must land in front of the user the way a type error does:
    /// against real source, with the offending construct under a caret. Before the span it was one
    /// line of prose with no file and no line — indistinguishable from a broken toolchain, which is
    /// exactly how it was (mis)read.
    #[test]
    fn a_located_internal_failure_renders_against_real_source() {
        use noeta_span::{Source, SourceId};

        let text = "enum Shape { Circle(int); }\nx = Shape.Circle\n";
        let sources = SourceMap::new(vec![Source::new(SourceId::FIRST, "main.noe", text)]);
        let at = text
            .find("Shape.Circle")
            .expect("the construct is in the source") as u32;
        let unsupported = noeta_compiler::Unsupported {
            reason: "`Shape.Circle` is a data-carrying variant used without arguments".to_string(),
            span: Some(noeta_span::Span::new_in(SourceId::FIRST, at, at + 12)),
        };
        let (text, code) = CompileFailure::from_unsupported(&sources, &unsupported).to_text();
        assert_eq!(code, 1);
        assert!(text.contains("E0068"), "carries its catalog code: {text}");
        assert!(text.contains("main.noe"), "names the file: {text}");
        assert!(text.contains("Shape.Circle"), "shows the source: {text}");

        // With no span there is nothing to point at, and the honest rendering is the one sentence.
        let bare = noeta_compiler::Unsupported {
            reason: "something".to_string(),
            span: None,
        };
        let (text, code) = CompileFailure::from_unsupported(&sources, &bare).to_text();
        assert_eq!(code, 1);
        assert_eq!(
            text,
            "noeta: internal error: the VM cannot compile this program: something\n"
        );
    }

    #[test]
    fn a_dependencys_edition_is_part_of_the_cache_key() {
        // A dep-edition bump must invalidate cached bytecode (editions arc S2). With the edition
        // now TYPED end to end, a divergent value is unrepresentable until a second edition ships
        // — so this pins the mechanism instead: `key_deps` folds the edition tag, i.e. a key built
        // by the same recipe minus the edition line differs. (When Edition grows a second variant,
        // strengthen this back to two real editions.)
        let dep = a_dep();
        let with_edition = key_of(std::slice::from_ref(&dep));
        let mut without = noeta_cache::KeyBuilder::new();
        // The key_deps recipe, minus the `<dep-edition>` fold — must NOT collide.
        without.source(
            format!("<dep {}>", dep.prefix.join(".")),
            dep.root.as_bytes(),
        );
        for (local, global) in &dep.dep_renames {
            without.source(
                format!("<rename {} {local}>", dep.prefix.join(".")),
                global.as_bytes(),
            );
        }
        for module in &dep.modules {
            without.source(&module.name, module.text.as_bytes());
        }
        assert_ne!(
            with_edition,
            without.finish().as_hex().to_string(),
            "key_deps must fold the dependency's edition into the cache key"
        );
    }
}
