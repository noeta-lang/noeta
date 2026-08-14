//! **`project_check` — the one answer to "is this project clean".**
//!
//! Three surfaces used to answer that question, and they answered it differently. `noeta check`
//! walked the directory, grouped the `.noe` files by module pool, and checked **every** file as its
//! own entry, once as it ships and once per code tier its own blocks name. The editor checked the
//! **open documents**. The MCP `check` tool checked the **first member** of one workspace. A
//! library module no entry imported was type-checked by exactly one of the three, and each surface
//! carried its own copy of the walk, the tier activation and the sweep.
//!
//! There is one implementation here, and the surfaces differ only in **which entries** they hand
//! it:
//!
//! | surface | entries |
//! |---|---|
//! | `noeta check [PATH]` | every `.noe` under `PATH` ([`project_check`]) |
//! | MCP `check` | the same, for a path; the one inline buffer for `source` |
//! | LSP `workspace/diagnostic` | the same, with the editor's unsaved buffers overlaid |
//! | LSP `textDocument/diagnostic` + push | the one open document ([`noeta_db::entry_diagnostics`]) |
//!
//! The narrow editor path is a different **entry set**, never a different **shape set**: it calls
//! the same [`noeta_db::entry_diagnostics`] this module calls, so a document the editor happens to
//! have open is reported exactly as `noeta check` reports it. Splitting on shapes is the drift this
//! module exists to end; splitting on entries is the whole reason it is affordable — whole-project
//! checking on every keystroke is not, so the project answer is a *pull* and the per-edit answer
//! narrows to the edited document.
//!
//! # The engine
//!
//! The sweep drives **salsa**, not the loader's directory reader, and that was measured rather than
//! assumed.
//!
//! The decisive argument is not speed but singularity. The editor's per-document path *must* be
//! salsa — it overlays unsaved buffers and reuses work across keystrokes — so a filesystem-based
//! project walk would have left "which shapes of one entry" answered twice, by `parse_dir` for the
//! batch and by the query family for the editor, which is precisely the drift being fixed.
//!
//! Speed then had to not be an obstacle, and is not. The two engines buy the same sharing by
//! different means: `noeta_loader::parse_dir` reads, lexes and parses a pool once and links every
//! entry against the shared pool; salsa memoizes `ast_in` per source, so a pool's parse likewise
//! happens once however many entries link against it — and lazily, which the batch reader cannot
//! be. Measured min-of-7, release, on this repo:
//!
//! | invocation | `parse_dir` | salsa |
//! |---|---|---|
//! | `check tests/conformance` (1260 files, 45 packages) | 286 ms | 286 ms |
//! | `check tests/conformance/std` (111 entries, one pool) | 40 ms | 48 ms |
//! | `check tests/conformance/std/math_basics.noe` | 18 ms | **4 ms** |
//!
//! The last row is the lazy parse: a single entry no longer pays for its whole directory. The
//! middle row is salsa's per-query bookkeeping over 111 entries, the price of the first.
//!
//! One database **per pool**, dropped when the pool is done: the per-entry link memo holds a whole
//! merged program, so a thousand-file project must not accumulate a thousand of them at once. Peak
//! RSS over that 1260-file tree is 30 MB against the batch reader's 24 MB.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use noeta_db::{CheckFlavor, LangDatabase, SourceProgram};
use noeta_diagnostics::Diagnostic;
use noeta_span::{Source, SourceId, SourceMap};

use crate::workspace::{self, WorkspaceCache, disk_noe_uris, path_to_uri, uri_to_path};

/// What a project check covers beyond "every `.noe` file under the root".
///
/// Deliberately opaque with builder methods rather than public fields: a new option must not be
/// something a caller can forget to set, and the three surfaces must not be able to disagree about
/// the default.
#[derive(Debug, Clone, Default)]
pub struct ProjectCheckOptions {
    selection: Vec<String>,
    overlay: BTreeMap<PathBuf, String>,
    target: Option<String>,
}

impl ProjectCheckOptions {
    /// The default: every entry, in its shipping shape plus one shape per code tier its own blocks
    /// name, reading every file from disk.
    pub fn new() -> ProjectCheckOptions {
        ProjectCheckOptions::default()
    }

    /// The caller's **explicit** tier selection — `noeta check --tier a --tier b`, or the live
    /// tiers of a `--target`. Checked as one shape (their union is a build that really exists); the
    /// per-tier sweep then covers whatever the selection left out.
    pub fn with_tiers(mut self, tiers: impl IntoIterator<Item = String>) -> ProjectCheckOptions {
        self.selection = tiers.into_iter().collect();
        self
    }

    /// The **build target** the check is about — `noeta check --target dev`.
    ///
    /// A target is not only a tier selection ([`Self::with_tiers`] carries that half): it is also a
    /// *dependency* selection, because `[targets.<name>.dependencies]` layers onto the globals. The
    /// two halves must travel together — a check that activated a target's tiers while resolving the
    /// global dependency set reported E0019 against every import of a dev-only dependency on a
    /// project `noeta run --target dev` compiles.
    ///
    /// `None` (the default) is the global dependency set, which is what a surface with no target
    /// concept — the LSP, the MCP `check` tool — asks for.
    pub fn with_target(mut self, target: Option<&str>) -> ProjectCheckOptions {
        self.target = target.map(str::to_string);
        self
    }

    /// **Unsaved editor buffers**, overlaid on the disk scan by path. What makes the editor's
    /// project-wide answer describe the project the user is looking at rather than the one on disk;
    /// a batch caller passes none.
    pub fn with_overlay(
        mut self,
        overlay: impl IntoIterator<Item = (PathBuf, String)>,
    ) -> ProjectCheckOptions {
        self.overlay = overlay.into_iter().collect();
        self
    }
}

/// One diagnostic together with the [`SourceMap`] that resolves its spans.
///
/// The map travels *with* the diagnostic because a project spans several pools, each numbering its
/// own `SourceId`s from zero, and an entry whose directives expanded at compile time carries
/// generated sources beyond its pool's map. A caller that rendered every diagnostic against one map
/// would name the wrong file.
#[derive(Debug, Clone)]
pub struct ProjectDiagnostic {
    /// Shared per pool (and per link, for the rare entry with expansions) — cloning one is a
    /// refcount bump, not a copy of every file's text.
    pub sources: Arc<SourceMap>,
    pub diagnostic: Diagnostic,
}

/// The whole outcome of a project check: what was looked at, and everything wrong with it.
#[derive(Debug, Clone, Default)]
pub struct ProjectCheck {
    /// Every **unique** diagnostic, ordered by file name, then offset, then code — which is also
    /// the render order, so output is deterministic. Deduplicated across entries, pools and shapes
    /// by that same key, so a module linked by ten importers reports its fault once.
    pub diagnostics: Vec<ProjectDiagnostic>,
    /// How many entries were checked.
    pub files_checked: usize,
    /// The dev tiers whose blocks were checked beyond the shipping shape, sorted and deduplicated
    /// across every entry. Empty when the sources declare no code-tier block.
    pub tiers_checked: Vec<String>,
    /// **Operational** failures, already rendered as sentences: a file that could not be read, a
    /// dependency graph that could not be resolved. Not diagnostics — they are about the check, not
    /// about the code — but a run that reports one has not checked what it was asked to.
    pub problems: Vec<String>,
    /// Native packages the program needs whose extensions **this process does not carry**, unioned
    /// over the pools. Non-empty means the answer is untrustworthy: whole namespaces are absent, so
    /// every file would report an unresolved-import cascade for code a composed toolchain compiles
    /// cleanly. A caller reports the cause instead of the cascade.
    pub uncomposed: Vec<String>,
}

impl ProjectCheck {
    /// How many unique error-severity diagnostics — what gates an exit code.
    pub fn errors(&self) -> usize {
        self.count(noeta_diagnostics::Severity::Error)
    }

    /// How many unique warning-severity diagnostics.
    pub fn warnings(&self) -> usize {
        self.count(noeta_diagnostics::Severity::Warning)
    }

    fn count(&self, severity: noeta_diagnostics::Severity) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.diagnostic.severity == severity)
            .count()
    }
}

/// Accumulates a project check across pools, deduplicating as it goes.
///
/// The dedup key is the **file name** a diagnostic renders against plus its byte span and code —
/// never the `SourceId`, which is pool-local (each pool restarts them at 0). The key's ordering
/// (name, offset, code) is the render order.
#[derive(Default)]
struct Fold {
    diagnostics: BTreeMap<(String, u32, u32, &'static str), ProjectDiagnostic>,
    tiers: BTreeSet<String>,
    problems: Vec<String>,
    uncomposed: BTreeSet<String>,
    files_checked: usize,
}

impl Fold {
    fn push(&mut self, sources: &Arc<SourceMap>, diagnostic: &Diagnostic) {
        let key = (
            sources.source(diagnostic.span.source).name().to_string(),
            diagnostic.span.start,
            diagnostic.span.end,
            diagnostic.code.code(),
        );
        self.diagnostics
            .entry(key)
            .or_insert_with(|| ProjectDiagnostic {
                sources: Arc::clone(sources),
                diagnostic: diagnostic.clone(),
            });
    }

    fn finish(self) -> ProjectCheck {
        ProjectCheck {
            diagnostics: self.diagnostics.into_values().collect(),
            files_checked: self.files_checked,
            tiers_checked: self.tiers.into_iter().collect(),
            problems: self.problems,
            uncomposed: self.uncomposed.into_iter().collect(),
        }
    }
}

/// **Check a whole project**: every `.noe` file under `root` (or `root` itself, when it is a file)
/// as its own entry, in every shape it can be built in.
///
/// `root` is walked recursively and every `.noe` file becomes an entry, because the loader links
/// only an entry's own *module pool* — a library module no entry imports is otherwise never
/// type-checked at all. Entries are grouped by pool ([`entry_pool`]): every entry of one pool
/// shares its sources, its manifest, its dependency graph and its parsed modules, so the pool is
/// read and resolved once and each entry links against it.
pub fn project_check(root: &Path, options: &ProjectCheckOptions) -> ProjectCheck {
    let entries: Vec<PathBuf> = if root.is_dir() {
        noe_files(root)
    } else {
        vec![root.to_path_buf()]
    };
    let mut fold = Fold {
        files_checked: entries.len(),
        ..Fold::default()
    };
    // Group by module pool. `BTreeMap` so pools — and therefore any operational messages — come out
    // in a deterministic order.
    let mut by_pool: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for entry in entries {
        by_pool.entry(entry_pool(&entry).0).or_default().push(entry);
    }
    for (pool, pool_entries) in by_pool {
        let sources = pool_sources(&pool, options);
        let entry_uris: Vec<String> = pool_entries.iter().map(|p| path_to_uri(p)).collect();
        sweep_pool(sources, &entry_uris, options, &mut fold);
    }
    fold.finish()
}

/// **Check an explicit member list as one pool**, sweeping every member as an entry — the same
/// engine [`project_check`] runs, for a caller whose sources are not (only) files.
///
/// The MCP `check` tool's inline `source` is the case this exists for: one in-memory member, no
/// directory to walk, and — because it is the same function — the same shapes, the same dedup and
/// the same tier reporting a checked *file* gets.
pub fn check_sources(
    sources: Vec<(String, String)>,
    options: &ProjectCheckOptions,
) -> ProjectCheck {
    let mut fold = Fold {
        files_checked: sources.len(),
        ..Fold::default()
    };
    sweep_pool(sources, &[], options, &mut fold);
    fold.finish()
}

/// The `(uri, text)` members of one pool: its `.noe` files (the package walk when the pool is a
/// package, the flat directory scan otherwise), plus any overlaid buffer that lives in it and is
/// not yet on disk. Sorted, which is the [`SourceId`] order every consumer of the pool agrees on.
fn pool_sources(pool: &Path, options: &ProjectCheckOptions) -> Vec<(String, String)> {
    let mut uris: Vec<String> = disk_noe_uris(pool);
    for path in options.overlay.keys() {
        let uri = path_to_uri(path);
        if entry_pool(path).0 == pool && !uris.contains(&uri) {
            uris.push(uri);
        }
    }
    uris.sort();
    uris.dedup();
    uris.into_iter()
        .map(|uri| {
            let text = uri_to_path(&uri)
                .and_then(|path| {
                    options
                        .overlay
                        .get(&path)
                        .cloned()
                        .or_else(|| std::fs::read_to_string(&path).ok())
                })
                .unwrap_or_default();
            (uri, text)
        })
        .collect()
}

/// One pool: build its salsa workspace once, then sweep each requested entry against it. An empty
/// `entry_uris` sweeps **every** member.
///
/// The database is local to the pool and dropped with it. That is deliberate — the per-entry link
/// memo holds a whole merged program, so keeping one database for a whole repository would hold
/// every pool's every entry's merged program resident simultaneously.
fn sweep_pool(
    sources: Vec<(String, String)>,
    entry_uris: &[String],
    options: &ProjectCheckOptions,
    fold: &mut Fold,
) {
    // Requested entries the pool walk did not yield — a `tools/` beside a `Cargo.toml`, a
    // `target/`, a dot-directory. They are still files *of* the package, and the loader links them
    // as such (see `outside_the_pool`), so each needs the pool's members beside it: keep a copy of
    // the sources for them. Only when there is such an entry, which is the rare case.
    let members: BTreeSet<&str> = sources.iter().map(|(uri, _)| uri.as_str()).collect();
    let outside: Vec<String> = entry_uris
        .iter()
        .filter(|uri| !members.contains(uri.as_str()))
        .cloned()
        .collect();
    let pool_sources = (!outside.is_empty()).then(|| sources.clone());

    let mut db = LangDatabase::default();
    let Some(cache) = workspace::sync(&mut db, None, sources, options.target.as_deref()) else {
        // **The pool walk yielded no member**, which is not the same thing as "these files cannot
        // be read". A package whose only `.noe` files live in a data directory — every project
        // wired for `noeta migrate`, whose `migrations/` holds programs and whose `src/` may not
        // exist yet — has all of them pruned from the walk ([`noeta_loader::read_package_modules`])
        // and so has an empty member set. Reporting `cannot read <file>` for each of them said the
        // one thing that is provably false about a file the walk deliberately stepped past, and
        // said it while counting zero errors: `noeta check .` printed "0 error(s)" and exited 2 on
        // a project `noeta run migrations/…` executes.
        //
        // With no members, every requested entry is outside the pool by construction, so this is
        // not a second answer — it is the same one `outside_the_pool` gives a pruned entry, over
        // the empty pool the walk produced.
        drop(db);
        let missed: Vec<&str> = entry_uris.iter().map(String::as_str).collect();
        outside_the_pool(Vec::new(), &missed, options, fold);
        return;
    };
    // A dependency graph that would not resolve **stops this pool**, reported and unchecked.
    //
    // The editor deliberately stays quiet about the routine half of these and carries on with what
    // it has (see `WorkspaceCache::dep_degraded`), because a flaky network must not nag while you
    // type. A batch check cannot do that: checking a program whose dependencies are missing turns
    // one accurate sentence — "this package requires noeta >=0.3" — into a hundred unresolved-import
    // errors about code that compiles, which is the same "confident, detailed and wrong" answer the
    // uncomposed refusal exists to avoid. The problem is reported and the exit code reflects it.
    if let Some(err) = cache.dep_error.as_ref().or(cache.dep_degraded.as_ref()) {
        fold.problems
            .push(format!("{}: {err}", display(&pool_name(&cache))));
        return;
    }
    fold.uncomposed.extend(cache.uncomposed.iter().cloned());
    let map = Arc::new(source_map_of(&db, &cache, &[]));
    let owned: Vec<String>;
    let entry_uris = if entry_uris.is_empty() {
        owned = cache.source_uris.clone();
        &owned
    } else {
        entry_uris
    };
    let mut missed: Vec<&str> = Vec::new();
    for uri in entry_uris {
        match cache.find_member(uri).and_then(|(_, m)| m.input()) {
            Some(program) => sweep_entry(&db, &cache, &map, program, options, fold),
            // A requested entry the pool scan did not yield: it links against the pool from
            // *outside* it, which is what the loader does for the same file.
            None => missed.push(uri),
        }
    }
    // The database is dropped here with the pool. Deliberately NOT `release_all`: that exists so a
    // long-lived editor session can reclaim a *closed* directory's memos while keeping the salsa
    // input slots, and it pays for a full recompute per source to do it. Dropping the whole
    // database frees the same memory for free.
    drop(db);
    if !missed.is_empty() {
        outside_the_pool(pool_sources.unwrap_or_default(), &missed, options, fold);
    }
}

/// **Entries the pool walk pruned**, each linked against the pool's members — the answer
/// `noeta_loader::read_siblings` gives the same file.
///
/// A package's walk prunes whole subtrees ([`noeta_loader::is_outside_package`]): a nested cargo
/// crate, a dot-directory, `target/`, a nested package. The loader applies that rule only to the
/// directories it *descends into*, never to the **entry** it was handed — so `noeta run
/// app/tools/probe.noe` gives `probe.noe` every module of `app`, wherever `tools/` sits in that
/// classification. The salsa surface looked the entry up among the walked members, missed, and
/// checked it as a **one-member** workspace: E0019 for imports the loader resolves, and — worse —
/// silence where two files derive one module path, because a lone link has nothing to collide
/// against. `noeta check` reported a clean tree that `noeta run` refuses outright.
///
/// The repair is this side, not a prune of the check walk. Pruning would have made `check` stop
/// *claiming* to cover these files, which ends the contradiction by dropping the coverage: the
/// silent E0073 would then be a gap nobody reports rather than an agreement. Linking the entry the
/// way the loader links it makes both surfaces answer the same question, which is the property this
/// whole test binary exists to hold.
///
/// One database for all of them, re-synced per entry: they share the pool's sources, so
/// [`workspace::sync`]'s reuse-by-URI keeps the members' inputs and swaps only the entry. They do
/// **not** share a workspace — two pruned siblings are not each other's modules under the loader
/// either, since neither is in the walk that produced the other's pool.
///
/// # The one entry that gets *no* siblings
///
/// `noeta_loader::read_siblings` does not hand every pruned entry the package: its **first** rule
/// is that a
/// program in a data directory (`migrations/`, `seeds/`) links against nothing of the package at
/// all. A migration is not a module and the package's modules are not its concern — it reaches its
/// dependency packages through the graph and writes the rest itself. Handing it the package here
/// instead made `noeta check .` resolve `use app.lib.helper` inside a migration that `noeta run`
/// refuses with E0019: a silent acceptance, which is the serious direction.
fn outside_the_pool(
    pool: Vec<(String, String)>,
    entry_uris: &[&str],
    options: &ProjectCheckOptions,
    fold: &mut Fold,
) {
    let mut db = LangDatabase::default();
    let mut cache: Option<WorkspaceCache> = None;
    for uri in entry_uris {
        // A URI with no path is an inline buffer, which is a member of its own pool by
        // construction and never lands here; there is nothing to read for it.
        let Some(path) = uri_to_path(uri) else {
            continue;
        };
        let text = match options.overlay.get(&path) {
            Some(text) => text.clone(),
            None => match std::fs::read_to_string(&path) {
                Ok(text) => text,
                // One unreadable file never aborts the run.
                Err(err) => {
                    fold.problems
                        .push(format!("cannot read {}: {err}", path.display()));
                    continue;
                }
            },
        };
        // The loader's own rule, not a second spelling of it: a data-directory program links with
        // no package siblings (`read_siblings`'s `holds_program` arm), every other pruned entry
        // links against the whole pool.
        let mut sources = match entry_pool(&path).1 {
            Some(root) if root.holds_program(&path) => Vec::new(),
            _ => pool.clone(),
        };
        sources.push(((*uri).to_string(), text));
        // The pool's own sort order, extended: `SourceId` assignment follows the member list, and
        // every consumer of a pool agrees on sorted-by-URI.
        sources.sort_by(|(a, _), (b, _)| a.cmp(b));
        cache = workspace::sync(&mut db, cache.take(), sources, options.target.as_deref());
        let Some(cache) = cache.as_ref() else {
            continue;
        };
        // A dependency graph that would not resolve stops this the same way it stops the pool
        // sweep, and for the same reason: an unresolved graph turns one accurate sentence into a
        // hundred unresolved-import errors about code that compiles.
        //
        // Reaching this function at all means the failure is unreported. The pool sweep returns
        // the moment it sees one, so either it saw none — and this entry, a file of the same
        // package under the same target, will see none either — or **there was no pool**, which is
        // the data-directory-only package: with no member to resolve from, this is the first and
        // only place the package's graph is resolved, and staying quiet here would have traded the
        // false `cannot read` for a silent exit 0 on a project `noeta run` refuses outright.
        if let Some(err) = cache.dep_error.as_ref().or(cache.dep_degraded.as_ref()) {
            fold.problems
                .push(format!("{}: {err}", display(&pool_name(cache))));
            return;
        }
        fold.uncomposed.extend(cache.uncomposed.iter().cloned());
        let map = Arc::new(source_map_of(&db, cache, &[]));
        if let Some(program) = cache.find_member(uri).and_then(|(_, m)| m.input()) {
            sweep_entry(&db, cache, &map, program, options, fold);
        }
    }
}

/// How a member URI is named in an operational message — its path, or the URI itself for a source
/// that has none (an inline buffer).
fn display(uri: &str) -> String {
    uri_to_path(uri).map_or_else(|| uri.to_string(), |p| p.display().to_string())
}

/// What a pool is called in an operational message: its first member, which is the file whose
/// manifest the failed resolve was about.
fn pool_name(cache: &WorkspaceCache) -> String {
    cache.source_uris.first().cloned().unwrap_or_default()
}

/// **One entry, every shape.** The single call every surface's per-entry answer goes through.
fn sweep_entry(
    db: &LangDatabase,
    cache: &WorkspaceCache,
    map: &Arc<SourceMap>,
    entry: SourceProgram,
    options: &ProjectCheckOptions,
    fold: &mut Fold,
) {
    let ws = cache.workspace;
    fold.tiers
        .extend(noeta_db::entry_code_tiers(db, ws, entry).iter().cloned());
    // An entry whose directives expanded at compile time has sources of its own — the generated
    // declarations — that the pool's map does not hold, so its diagnostics render against an
    // extended map. That is the rare path: with no expanding directive (nearly every program) the
    // pool's `Arc` is reused untouched and this loop clones nothing.
    let expansions = &noeta_db::linked_from(db, ws, entry).expansions;
    let sources = if expansions.is_empty() {
        Arc::clone(map)
    } else {
        Arc::new(source_map_of(db, cache, expansions))
    };
    for diagnostic in
        noeta_db::entry_diagnostics(db, ws, entry, &options.selection, CheckFlavor::Compile)
    {
        fold.push(&sources, &diagnostic);
    }
}

/// The [`SourceMap`] a pool's diagnostics render against: every member and dependency module by
/// `SourceId`, continued through one link's generated sources.
///
/// Each source is named by its **path**, not its `file:` URI, because that is what a diagnostic
/// renders as a location and what the cross-pool dedup key is built from.
fn source_map_of(
    db: &LangDatabase,
    cache: &WorkspaceCache,
    expansions: &[noeta_loader::ExpandedSource],
) -> SourceMap {
    SourceMap::new(
        cache
            .sources_with(expansions)
            .enumerate()
            .map(|(index, source)| {
                let name = uri_to_path(source.uri)
                    .map(|p| p.display().to_string())
                    // A generated source is named for the directive that produced it, not for a
                    // file; it has no path and must keep that name.
                    .unwrap_or_else(|| source.uri.to_string());
                Source::new(SourceId(index as u32), name, source.text(db))
            })
            .collect(),
    )
}

/// The **module pool** an entry links against: the package it belongs to (walked recursively, every
/// module carrying the path its location derives) or, for a file in no package, its own directory
/// (flat, each module identified by the `namespace` it declares).
///
/// Returned as a grouping key plus the package root, because entries are batched by pool — every
/// entry of one pool shares its sources, manifest, dependency graph and parsed module set, and is
/// read/lexed/parsed once for all of them. Grouping by *package* rather than by directory is what
/// makes a project check see the same program `noeta run` links: an app's `src/deep/nested.noe` is
/// a module of the app, not an unrelated file in another directory.
pub fn entry_pool(entry: &Path) -> (PathBuf, Option<noeta_loader::PackageRoot>) {
    match noeta_pm::sources::package_root(entry) {
        Some(root) => (root.dir.clone(), Some(root)),
        // An empty parent (a bare relative name like `noeta check foo.noe`) is the current
        // directory: the flat scan reads `.` for it while keeping the pool's module names
        // unprefixed, so the entry-to-pool name match still holds.
        None => (
            entry
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .to_path_buf(),
            None,
        ),
    }
}

/// Read a pool's modules — the package walk, or the flat directory scan for a file in no package.
pub fn pool_modules(
    dir: &Path,
    root: Option<&noeta_loader::PackageRoot>,
) -> Vec<noeta_loader::RawModule> {
    match root {
        Some(root) => noeta_loader::read_package_modules(root),
        None => noeta_loader::read_dir_modules(dir),
    }
}

/// Collect every `.noe` file under `root`, recursively, in sorted order (so discovery and thus the
/// check order are deterministic). Hand-rolled in the style of the loader's `read_siblings` — a
/// depth-first `read_dir` walk that silently skips directories it cannot read (a partial tree still
/// checks what it can). Symlinked directories are followed by `read_dir` as ordinary entries; cycles
/// are not guarded against, matching the loader's own assumptions about a normal source tree.
///
/// **This walk is deliberately wider than the loader's** ([`noeta_loader::derive::is_outside_package`]),
/// and the difference is the point rather than drift. The loader answers *which files are modules of
/// this package* and prunes everything else out of the pool; this answers *which files should be
/// checked as entries*, and a file the pool prunes is exactly the one nothing else will look at.
/// Narrowing this to the loader's rule silently deletes that: two files deriving one module path
/// across a Cargo-crate prune is E0073 today only because this walk still finds the pruned one.
///
/// It stops at two kinds of directory, for two different reasons:
///
/// * **A dot-directory.** `.git` is metadata; the one that bites is `.claude/worktrees/`, a whole
///   second copy of every module, so checking a package swept an agent's in-progress branch into
///   the same program and reported its errors against a consumer that never referenced it.
/// * **A nested package** — a directory holding its own [`noeta_loader::MANIFEST_NAME`]. Not
///   because its files do not deserve checking, but because they cannot be checked *from here*: a
///   nested package has its own dependencies and its own lockfile, and every verb resolves an entry
///   against the root it is standing in. Sweeping one up resolved its files against the OUTER
///   package's dependency versions, so a nested package pinning anything else failed provenance
///   verification and simply did not check — reported as "N files failed to check", naming no
///   boundary. Its modules derive under its own root, so nothing this walk covers can collide with
///   them, and nothing is lost by leaving them to it.
///
/// A nested package is not skipped work: it is entered. `noeta test examples/app` (or a CI step
/// that `cd`s there first) runs it as what it is, with its own manifest resolving.
pub fn noe_files(root: &Path) -> Vec<PathBuf> {
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
                // A dot-directory, or a package of its own — see this function's doc for why those
                // two and not the loader's wider prune.
                let skip = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'))
                    || p.join(noeta_loader::MANIFEST_NAME).is_file();
                if skip {
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
