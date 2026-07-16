//! Multi-file module loading and linking (M1.9).
//!
//! A program is rooted at an *entry* `.noe` file. Sibling `.noe` files in the entry's
//! directory are candidate *modules*, each declaring its identity with `namespace App.Models;`.
//! The entry's `use App.Models.{User}` declarations resolve against those modules' declared
//! namespaces; each resolved name's real declaration is **merged into one [`Program`]** ahead of
//! the entry's own statements, so both backends run the linked program unchanged and the
//! differential oracle is preserved by construction.
//!
//! Linking is *additive and backward-compatible*: a `use` that no loaded module provides is left
//! in place, so the runtime falls back to its M0 opaque-stub behavior — a single file with no
//! sibling modules links to exactly itself. Real module loading therefore lights up only when
//! sibling modules actually provide the imported names.
//!
//! Diagnostics produced while loading carry the [`Source`] they should render against (a sibling
//! module's parse error renders against that module, not the entry). *Check/runtime* diagnostics
//! that land on a declaration merged in from a sibling resolve to the right source too: every span
//! is tagged at parse time with its [`SourceId`], so the caller resolves it through the [`Linked`]
//! [`SourceMap`] rather than against the entry (slice F4).

mod qualify;

use std::collections::HashSet;
use std::io;
use std::path::Path;

use noeta_ast::{Program, Stmt, UseName};
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_lexer::lex;
use noeta_span::{Source, SourceId, SourceMap};

/// A loaded, linked program ready to type-check and run.
#[derive(Debug)]
pub struct Linked {
    /// The merged program: each resolved imported declaration, in resolution order, followed by
    /// the entry's own statements (with resolved names removed from their `use` lists).
    pub program: Program,
    /// The entry source — kept for the entry's name and the single-source rendering path.
    pub entry: Source,
    /// Every source the program is built from (entry + sibling modules), indexed by `SourceId`.
    /// A diagnostic may land on a declaration merged in from a sibling module; its span carries
    /// that module's `SourceId`, so it resolves to the right file through this map rather than
    /// always rendering against the entry.
    pub sources: SourceMap,
    /// Which language [`Edition`](noeta_lexer::Edition) each source was written against, keyed by
    /// `SourceId` — the parallel of [`Self::sources`] for editions. The entry and its siblings take
    /// the root package's edition; each dependency package's modules take that package's own
    /// edition. The checker consults this per declaration (via a span's `SourceId`) so a merged
    /// program applies each package's own edition rules — the editions compiler arc's whole point.
    pub editions: noeta_lexer::EditionMap,
}

/// A diagnostic produced while loading, paired with the source it renders against.
#[derive(Debug)]
pub struct LoadDiagnostic {
    pub source: Source,
    pub diagnostic: Diagnostic,
}

/// Load and link the program rooted at `entry_path`. Returns the linked program, or the
/// load-time (lex/parse) diagnostics of the entry, each paired with its source. An `io::Error`
/// is only for a failure to read the entry file itself.
pub fn load(
    entry_path: &Path,
    root_edition: noeta_lexer::Edition,
) -> io::Result<Result<Linked, Vec<LoadDiagnostic>>> {
    let text = std::fs::read_to_string(entry_path)?;
    let name = entry_path.display().to_string();
    let siblings = read_siblings(entry_path);
    Ok(link(&name, &text, root_edition, &siblings))
}

/// One sibling module's identity (display name + source text), before parsing. Public so the
/// linker can be driven from in-memory sources in tests.
#[derive(Debug)]
pub struct RawModule {
    pub name: String,
    pub text: String,
}

/// Load and link the program rooted at `entry_path` **with its dependency packages** — the
/// dependency-aware twin of [`load`] (package-manager P2.1). The entry's siblings resolve as before;
/// each [`DepPackage`]'s modules are additionally re-rooted and linked in. An `io::Error` is only for
/// a failure to read the entry file itself.
pub fn load_with_deps(
    entry_path: &Path,
    root_edition: noeta_lexer::Edition,
    deps: &[DepPackage],
) -> io::Result<Result<Linked, Vec<LoadDiagnostic>>> {
    let text = std::fs::read_to_string(entry_path)?;
    let name = entry_path.display().to_string();
    let siblings = read_siblings(entry_path);
    Ok(link_with_deps(&name, &text, root_edition, &siblings, deps))
}

/// Read every `.noe` file **under `dir` recursively** as a [`RawModule`], in sorted order (so
/// SourceId assignment stays deterministic). A dependency package is a directory *tree*, not the
/// single flat directory the entry's siblings live in, so this walks subdirectories. Names are the
/// files' display paths (for diagnostics). Unreadable files are skipped.
pub fn read_package_sources(dir: &Path) -> io::Result<Vec<RawModule>> {
    let mut paths = Vec::new();
    collect_noe_files(dir, &mut paths)?;
    paths.sort();
    Ok(paths
        .into_iter()
        .filter_map(|p| {
            let text = std::fs::read_to_string(&p).ok()?;
            Some(RawModule {
                name: p.display().to_string(),
                text,
            })
        })
        .collect())
}

/// Recursively gather `.noe` file paths under `dir` into `out`. A subdirectory that can't be read is
/// skipped (best-effort), matching the sibling scan's tolerance.
fn collect_noe_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            let _ = collect_noe_files(&path, out);
        } else if path.is_file() && path.extension().is_some_and(|ext| ext == "noe") {
            out.push(path);
        }
    }
    Ok(())
}

/// A dependency package's sources, to be linked into the entry under the consumer's import root
/// (package-manager P2.1, model R1). `root` is the package's own namespace root segment (the
/// `package` half of its `[package] name`); `key` is the consumer's dependency-table key. The loader
/// **re-roots** the package's modules from `root` to `key` — rewriting the leading segment of each
/// module's `namespace` and its intra-package `use`s — so the consumer addresses the package as
/// `use <key>.<sub>.Name` while the package's own imports (written against `root`) keep resolving.
///
/// Unlike a sibling, a dependency module's own `use`s **drive** imports (a package is a closed unit:
/// its internal cross-references must resolve), whereas same-app siblings stay pure decl-sources — so
/// wiring dependencies never changes single-package linking.
///
/// **Transitive dependencies** (package-manager P2.4). A package's own modules import *its own*
/// dependencies by *its own* local keys (`use jsonlib.parse.X`), which collide across packages (two
/// packages may both key a dep `jsonlib` pointing at different packages). `dep_renames` disambiguates:
/// it maps each of this package's local dependency keys to the **globally-unique segment** the resolver
/// assigned the package that key resolves to, so every `use` leading segment in the flat pool addresses
/// exactly one package. For a leaf/direct dependency with no dependencies of its own it is empty, so
/// single-level linking is byte-for-byte unchanged. `key` is this package's own global segment (the
/// consumer's dep-key for a direct dependency, a synthesized unique segment for a transitive-only one).
#[derive(Debug)]
pub struct DepPackage {
    pub key: String,
    pub root: String,
    pub modules: Vec<RawModule>,
    /// This package's local dependency keys → the global segment of the package each resolves to
    /// (transitive linking, P2.4). Empty for a leaf package; then re-rooting is just `root` → `key`.
    pub dep_renames: std::collections::BTreeMap<String, String>,
    /// Whether this package carries a **native** entry crate (package-manager Phase 3). A native
    /// package's modules are provided by its Rust extension, registered only in the *composed*
    /// toolchain — so the host loader cannot see them and must **retain** (not flag) a `use` under
    /// its key; the composed checker validates the members. A pure-Noeta package has all its modules
    /// in the link pool, so a `use` under its key that resolves to nothing is a genuine typo.
    pub native: bool,
    /// The package's language **edition** in canonical string form (`"2026"`) — the semantics its
    /// source is written against (editions arc). Carried per package from resolution so a later
    /// compiler pass can apply *each* package's edition to *its own* declarations; today the merged
    /// program still compiles under the root's edition, so this is recorded, not yet acted on. A
    /// string (not the `Edition` enum) because the loader sits below the manifest layer that owns it.
    pub edition: String,
}

/// Re-root a namespace/use path in place: replace its leading segment per the rules
/// (package-manager P2.1/P2.4). If the leading segment is the package's own `root`, it becomes the
/// package's global `key`; otherwise, if it is one of the package's local dependency keys, it becomes
/// that dependency's global segment (`renames`). A path leading with anything else — `std`, or a
/// malformed package path — is left untouched.
fn reroot_path(
    path: &mut [String],
    root: &str,
    key: &str,
    renames: &std::collections::BTreeMap<String, String>,
) {
    let Some(head) = path.first_mut() else {
        return;
    };
    if head.as_str() == root {
        *head = key.to_string();
    } else if let Some(global) = renames.get(head.as_str()) {
        *head = global.clone();
    }
}

/// Re-root a dependency module's `namespace` (its match key) and `use` paths (its import drivers).
/// The `namespace` only ever leads with the package's own root, so it is rewritten `root` → `key`; a
/// `use` may lead with the package root (an intra-package reference) or one of the package's local
/// dependency keys (a transitive reference), both handled by [`reroot_path`]. Touches only those two
/// statement kinds — both are consumed *during* linking (matching / import-driving) and never appear
/// in the merged declaration output — so re-rooting cannot alter what a package contributes, only how
/// it's addressed.
///
/// Public so a salsa-based linker (`noeta-db`) can re-root a dependency's parsed [`Program`] before
/// feeding it to [`link_parsed_with_deps`] — the CLI's [`link_with_deps`] does this inline, but the
/// db builds `Program`s through its own memoized parse and re-roots them itself (package-manager
/// P2.1c).
pub fn reroot_program(
    program: &mut Program,
    root: &str,
    key: &str,
    renames: &std::collections::BTreeMap<String, String>,
) {
    for stmt in &mut program.stmts {
        match stmt {
            Stmt::Namespace { path, .. } => reroot_path(path, root, key, renames),
            Stmt::Use { path, .. } => reroot_path(path, root, key, renames),
            _ => {}
        }
    }
}

/// The raw [`Source`]s of a workspace: the entry plus its sibling module files, each with its own
/// [`SourceId`] (entry = 0, siblings 1..) assigned identically to [`link`]. Lexing/parsing happen
/// downstream, so this only reads and labels files — it feeds the salsa module graph (`noeta-db`,
/// M1.9.3), which derives one memoized `SourceProgram` input per source.
#[derive(Debug)]
pub struct RawWorkspace {
    pub entry: Source,
    pub modules: Vec<Source>,
}

/// Read the entry file and its sibling `.noe` modules into labeled [`Source`]s, without lexing,
/// parsing, or linking — the file-system front of the salsa module graph. An `io::Error` is only
/// for a failure to read the entry itself; unreadable siblings are skipped (as in [`read_siblings`]).
pub fn read_workspace(entry_path: &Path) -> io::Result<RawWorkspace> {
    let text = std::fs::read_to_string(entry_path)?;
    let entry = Source::new(SourceId(0), entry_path.display().to_string(), text);
    let modules = read_siblings(entry_path)
        .into_iter()
        .enumerate()
        .map(|(i, raw)| Source::new(SourceId((i + 1) as u32), raw.name, raw.text))
        .collect();
    Ok(RawWorkspace { entry, modules })
}

/// Gather the `.noe` files in the entry's directory other than the entry itself, in sorted
/// order (so SourceId assignment and resolution are deterministic). A read failure yields no
/// siblings — a lone file simply links to itself.
fn read_siblings(entry_path: &Path) -> Vec<RawModule> {
    let Some(dir) = entry_path.parent() else {
        return Vec::new();
    };
    let entry_name = entry_path.file_name();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "noe"))
        .filter(|p| p.file_name() != entry_name)
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|p| {
            let text = std::fs::read_to_string(&p).ok()?;
            Some(RawModule {
                name: p.display().to_string(),
                text,
            })
        })
        .collect()
}

/// The pure core of [`load`], split out so it is testable from in-memory sources: link the entry
/// (`entry_name`/`entry_text`) against the given sibling modules.
pub fn link(
    entry_name: &str,
    entry_text: &str,
    root_edition: noeta_lexer::Edition,
    siblings: &[RawModule],
) -> Result<Linked, Vec<LoadDiagnostic>> {
    // The entry is always SourceId 0; siblings follow. Each module keeps its own source so its
    // spans stay valid and its diagnostics render against it. The deps-free path: entry + siblings
    // are one package, so every source takes the root edition.
    let entry = Source::new(SourceId(0), entry_name, entry_text);
    let mut sources: Vec<Source> = vec![entry.clone()];
    let mut editions = noeta_lexer::EditionMap::new();
    editions.set(SourceId(0), root_edition);
    for (i, raw) in siblings.iter().enumerate() {
        let id = SourceId((i + 1) as u32);
        sources.push(Source::new(id, raw.name.as_str(), raw.text.as_str()));
        editions.set(id, root_edition);
    }
    let (lexeds, text_tiers) = lex_program(&sources);

    // Entry + siblings parse under the root package's edition (deps-free: no dependency packages,
    // so no other editions are in play). `link_with_deps` is the twin that also links dependencies,
    // each under its own edition.
    let entry_parsed = noeta_parser::parse_in(&entry, &lexeds[0].tokens, root_edition, &text_tiers);
    let entry_diags: Vec<Diagnostic> = lexeds[0]
        .diagnostics
        .iter()
        .chain(entry_parsed.diagnostics.iter())
        .cloned()
        .collect();
    if !entry_diags.is_empty() {
        return Err(entry_diags
            .into_iter()
            .map(|diagnostic| LoadDiagnostic {
                source: entry.clone(),
                diagnostic,
            })
            .collect());
    }

    // Parse each sibling under the root edition. Only cleanly-parsed modules contribute (a module
    // that fails to lex/parse cannot be resolved against and is skipped — surfacing a
    // *referenced-but-broken* module's errors is the deferred SourceMap work). Keep the parsed
    // programs alive for [`link_parsed`]. Every sibling's `Source` is retained (whether or not it
    // parsed) so the `SourceMap` indices line up with the `SourceId`s the parser stamped onto spans.
    let mut module_programs: Vec<Program> = Vec::new();
    for (source, lexed) in sources.iter().zip(&lexeds).skip(1) {
        if let Some(program) = parse_clean(source, lexed, root_edition, &text_tiers) {
            module_programs.push(program);
        }
    }

    let refs: Vec<&Program> = module_programs.iter().collect();
    let program = link_parsed(&entry, &entry_parsed.program, &refs)?;
    Ok(Linked {
        program,
        entry,
        sources: SourceMap::new(sources),
        editions,
    })
}

/// Link the entry against its sibling modules **and its dependency packages** (package-manager P2.1).
/// Each [`DepPackage`]'s modules are parsed, re-rooted from the package's own root segment to the
/// consumer's dependency key ([`reroot_program`]), and linked as a closed unit (their own `use`s
/// drive imports). SourceIds continue past the siblings (entry = 0, siblings `1..=S`, dependency
/// modules after), so every declaration's spans and diagnostics still render against their own file.
/// A dependency module that fails to lex/parse is skipped (its `Source` is still retained so the
/// `SourceMap` indices line up), mirroring the sibling policy.
pub fn link_with_deps(
    entry_name: &str,
    entry_text: &str,
    root_edition: noeta_lexer::Edition,
    siblings: &[RawModule],
    deps: &[DepPackage],
) -> Result<Linked, Vec<LoadDiagnostic>> {
    // Assemble every module's `Source` up front — entry = 0, siblings `1..=S`, dependency modules
    // continuing the sequence — then lex them as one program (see [`lex_program`]: a text tier
    // declared in any file, a dependency package's included, captures verbatim bodies in every
    // file) before any parsing. The `editions` side-table is built in lock-step: the entry and its
    // siblings take the root package's edition, each dependency's modules that package's own.
    let entry = Source::new(SourceId(0), entry_name, entry_text);
    let mut next_id: u32 = 1;
    let mut sources: Vec<Source> = vec![entry.clone()];
    let mut editions = noeta_lexer::EditionMap::new();
    editions.set(SourceId(0), root_edition);
    for raw in siblings {
        sources.push(Source::new(
            SourceId(next_id),
            raw.name.as_str(),
            raw.text.as_str(),
        ));
        editions.set(SourceId(next_id), root_edition);
        next_id += 1;
    }
    let sibling_end = sources.len();
    // A dependency's edition is a `String` on `DepPackage` (the loader is below the manifest layer);
    // resolution already validated it against the closed set, so reconstruct the enum and fall back
    // to the default on the impossible parse failure rather than propagate an error the walker ruled
    // out. Recorded per module so the map keys by `SourceId`.
    let dep_editions: Vec<noeta_lexer::Edition> = deps
        .iter()
        .map(|dep| noeta_lexer::Edition::parse(&dep.edition).unwrap_or_default())
        .collect();
    for (dep, &dep_edition) in deps.iter().zip(&dep_editions) {
        for raw in &dep.modules {
            sources.push(Source::new(
                SourceId(next_id),
                raw.name.as_str(),
                raw.text.as_str(),
            ));
            editions.set(SourceId(next_id), dep_edition);
            next_id += 1;
        }
    }
    let (lexeds, text_tiers) = lex_program(&sources);

    // The entry parses under the root package's edition.
    let entry_parsed = noeta_parser::parse_in(&entry, &lexeds[0].tokens, root_edition, &text_tiers);
    let entry_diags: Vec<Diagnostic> = lexeds[0]
        .diagnostics
        .iter()
        .chain(entry_parsed.diagnostics.iter())
        .cloned()
        .collect();
    if !entry_diags.is_empty() {
        return Err(entry_diags
            .into_iter()
            .map(|diagnostic| LoadDiagnostic {
                source: entry.clone(),
                diagnostic,
            })
            .collect());
    }

    // Parse the siblings (pure decl-sources) under the root edition.
    let mut sibling_programs: Vec<Program> = Vec::new();
    for (source, lexed) in sources[1..sibling_end].iter().zip(&lexeds[1..sibling_end]) {
        if let Some(program) = parse_clean(source, lexed, root_edition, &text_tiers) {
            sibling_programs.push(program);
        }
    }

    // Parse + re-root each dependency package's modules under *that package's* edition (the sources
    // continue past the siblings in the same package order they were assembled above).
    let mut dep_programs: Vec<Program> = Vec::new();
    let mut dep_idx = sibling_end;
    for (dep, &dep_edition) in deps.iter().zip(&dep_editions) {
        for _ in &dep.modules {
            if let Some(mut program) = parse_clean(
                &sources[dep_idx],
                &lexeds[dep_idx],
                dep_edition,
                &text_tiers,
            ) {
                reroot_program(&mut program, &dep.root, &dep.key, &dep.dep_renames);
                dep_programs.push(program);
            }
            dep_idx += 1;
        }
    }

    let sibling_refs: Vec<&Program> = sibling_programs.iter().collect();
    let dep_refs: Vec<&Program> = dep_programs.iter().collect();
    // A resolved dependency graph is complete knowledge: the always-legitimate non-std roots are the
    // declared native-package keys (their members live in the composed toolchain, not the link pool).
    let native_roots: Vec<String> = deps
        .iter()
        .filter(|d| d.native)
        .map(|d| d.key.clone())
        .collect();
    let program = link_parsed_with_deps(
        &entry,
        &entry_parsed.program,
        &sibling_refs,
        &dep_refs,
        Some(&native_roots),
    )?;
    Ok(Linked {
        program,
        entry,
        sources: SourceMap::new(sources),
        editions,
    })
}

/// Lex every module of a program as one unit (text-tiers arc): each file lexes with the default
/// text-tier set first; if any file declares a text tier (`@tier(x, …, text: "…")`), the union of
/// all declarations is applied and every file re-lexes with it — so a tier declared in one file
/// (or one dependency package) captures `@x { … }` bodies verbatim in every other. Only programs
/// declaring text tiers pay the second pass.
fn lex_program(sources: &[Source]) -> (Vec<noeta_lexer::Lexed>, noeta_lexer::TextTiers) {
    let lexeds: Vec<_> = sources.iter().map(lex).collect();
    // Verbatim-body tiers come from two sources: a program's own `@tier(…, text/expr)` (found by
    // the lexer's per-file token scan) and the installed extensions' declarations (`doc`, and any
    // native `@json`/`@sql` — no `.noe` file declares these).
    let mut declared: Vec<String> = lexeds
        .iter()
        .flat_map(|l| l.text_tier_decls.iter().cloned())
        .collect();
    declared.extend(
        noeta_stdlib::registry::ext_verbatim_tier_names()
            .into_iter()
            .map(str::to_string),
    );
    let set = noeta_lexer::TextTiers::with(declared.clone());
    let default = noeta_lexer::TextTiers::default();
    if declared.iter().all(|name| default.contains(name)) {
        return (lexeds, set);
    }
    let relexed = sources
        .iter()
        .map(|source| noeta_lexer::lex_in(source, noeta_lexer::Edition::DEFAULT, &set))
        .collect();
    (relexed, set)
}

/// Parse an already-lexed source, yielding its [`Program`] only if both lex and parse are clean
/// (a module that fails cannot be resolved against and is skipped). The shared helper behind
/// [`link`]'s sibling loop and [`link_with_deps`].
fn parse_clean(
    source: &Source,
    lexed: &noeta_lexer::Lexed,
    edition: noeta_lexer::Edition,
    text_tiers: &noeta_lexer::TextTiers,
) -> Option<Program> {
    (lexed.diagnostics.is_empty())
        // Parse under the owning package's edition — the entry/sibling's root edition or a
        // dependency's own — so a future edition-gated grammar applies per package.
        .then(|| noeta_parser::parse_in(source, &lexed.tokens, edition, text_tiers))
        .filter(|parsed| parsed.diagnostics.is_empty())
        .map(|parsed| parsed.program)
}

/// Resolve and merge an *already-parsed* entry against already-parsed candidate modules — the pure
/// linking core shared by [`link`] (which lexes/parses first) and the salsa module-graph query
/// (`noeta-db`, M1.9.3), which feeds it programs straight from the memoized `ast` queries. `entry`
/// is the entry's [`Source`] (so import errors render against it); `modules` are the cleanly-parsed
/// candidate module programs (each declaring its `namespace`). Returns the merged [`Program`] — each
/// resolved import's declaration ahead of the entry's own statements — or the `use`-resolution
/// diagnostics (E0019 private/missing export, E0020 name collision).
pub fn link_parsed(
    entry: &Source,
    entry_program: &Program,
    modules: &[&Program],
) -> Result<Program, Vec<LoadDiagnostic>> {
    // Sibling-only linking has no resolved dependency graph, so it is lenient: it can flag a missing
    // intra-project module but must not adjudicate foreign roots (see [`RetainPolicy`]).
    link_core(entry, entry_program, modules, &[], RetainPolicy::Lenient)
}

/// The cross-package variant (package-manager P2.1): like [`link_parsed`], but `dep_modules` are the
/// re-rooted source modules of the entry's dependency packages. They are **both** resolution
/// candidates *and* import drivers — a package is a closed unit, so its own `use`s (already re-rooted
/// to the consumer's key) resolve its internal cross-references, unlike same-app siblings which stay
/// pure decl-sources. `dep_modules` must already be re-rooted (see [`reroot_program`]); the caller
/// ([`link_with_deps`]) does that. Every std import inside a dependency (`use std.…`) resolves against
/// no module here and is retained (deduped) so the compiler binds it downstream, exactly as an
/// entry's std imports are.
/// `native_roots` gates dependency-import strictness (module-namespaces): `Some(roots)` means the
/// caller resolved the **complete** dependency graph (the CLI), so every legitimate import root is
/// known — std extensions plus these declared native-package roots — and any other unresolved import
/// is an error. `None` means the caller (the IDE `linked` query) lacks that graph and stays lenient.
pub fn link_parsed_with_deps(
    entry: &Source,
    entry_program: &Program,
    siblings: &[&Program],
    dep_modules: &[&Program],
    native_roots: Option<&[String]>,
) -> Result<Program, Vec<LoadDiagnostic>> {
    // Dependency modules join the resolution pool; only they (not siblings) also drive imports.
    let pool: Vec<&Program> = siblings.iter().chain(dep_modules).copied().collect();
    let native: HashSet<String> = native_roots.unwrap_or_default().iter().cloned().collect();
    let retain = match native_roots {
        Some(_) => RetainPolicy::Complete {
            native_roots: &native,
        },
        None => RetainPolicy::Lenient,
    };
    link_core(entry, entry_program, &pool, dep_modules, retain)
}

/// Where a merged top-level name came from — its local declaration, or the namespace an import
/// pulled it from. Lets two `use`s that name the **same** declaration (the entry and a dependency
/// module both importing `webclient.client.Client`) merge it once, while a genuine clash (same name,
/// different namespace, or an import shadowing a local) is still an E0020 collision.
enum Origin {
    Local,
    Import(Vec<String>),
}

/// The shared linking core: resolve the entry's imports (and any `dep_drivers`' imports) against the
/// `pool`, merging each resolved declaration once and retaining every unresolved `use` (deduped) for
/// the compiler's downstream binding. `dep_drivers` is empty for single-package linking, so that path
/// is unchanged bar one refinement: a duplicate *identical* import (same namespace + name) is now
/// skipped rather than flagged, which a closed dependency unit needs and no well-formed program relied
/// on erroring.
fn link_core(
    entry: &Source,
    entry_program: &Program,
    pool: &[&Program],
    dep_drivers: &[&Program],
    retain: RetainPolicy,
) -> Result<Program, Vec<LoadDiagnostic>> {
    // For the complete policy: the always-retained roots are the std extensions. The loader is
    // already global-registry-coupled (verbatim-tier names below), so the default seed is the lens.
    let reg = noeta_stdlib::registry::default_seeded();
    // A module contributes only if it declares a namespace to resolve against.
    let module_views: Vec<ModuleView> = pool
        .iter()
        .filter_map(|prog| {
            module_namespace(prog).map(|namespace| ModuleView {
                namespace,
                stmts: &prog.stmts,
            })
        })
        .collect();

    // Namespace qualification (arc Phase B): each module's local type names → qualified identities.
    // A merged declaration is rewritten with the map of the module it came from (keyed by namespace,
    // one module per namespace); the entry's own statements with the entry's map. Both are empty for
    // a non-namespaced module, so single-namespace programs stay byte-identical.
    let entry_ns = module_namespace(entry_program).unwrap_or_default();

    // The **root segments of this project's own namespace tree** — the entry's namespace root plus
    // every loaded module's. Under the lenient policy, an unresolved `use` whose root is one of these
    // is an *intra-project* reference to a sibling that does not exist (`use App.Models.User` with no
    // `App.Models` module): a genuine error. Anything else is retained (the lenient path lacks the
    // dependency graph, so it must not adjudicate foreign roots). See [`RetainPolicy`].
    let project_roots: HashSet<String> = std::iter::once(&entry_ns)
        .chain(module_views.iter().map(|mv| &mv.namespace))
        .filter_map(|ns| ns.first().cloned())
        .collect();

    // Everything a `use` could legitimately target, for "did you mean" on an unresolved one: every
    // loaded module's dotted namespace (matches a mistyped module path like `App.Modles`) plus every
    // valid root — std extensions, this project's namespaces, and (complete policy) declared native
    // packages (matches a mistyped package name like `imgtx`).
    let mut import_targets: Vec<String> = module_views
        .iter()
        .map(|mv| mv.namespace.join("."))
        .collect();
    import_targets.extend(reg.extensions().iter().map(|e| e.root().to_string()));
    import_targets.extend(project_roots.iter().cloned());
    if let RetainPolicy::Complete { native_roots } = &retain {
        import_targets.extend(native_roots.iter().cloned());
    }

    let entry_map = build_module_map(&entry_ns, &entry_program.stmts, &module_views);
    let module_maps: std::collections::HashMap<Vec<String>, qualify::QMap> = module_views
        .iter()
        .map(|mv| {
            (
                mv.namespace.clone(),
                build_module_map(&mv.namespace, mv.stmts, &module_views),
            )
        })
        .collect();

    // Every merged top-level name → its origin. Seeded with the entry's own declarations (each
    // `Local`); an import that would shadow a local, or clash with a differently-sourced import, is a
    // collision. An import that re-names an already-merged declaration from the *same* namespace is a
    // no-op (the closed-unit dedup).
    let mut origins: std::collections::HashMap<String, Origin> = entry_program
        .stmts
        .iter()
        .filter_map(decl_name)
        .map(|n| (n.to_string(), Origin::Local))
        .collect();

    let mut imported: Vec<Stmt> = Vec::new();
    // Qualified identities already merged (explicit imports and their transitive same-module
    // dependencies alike) — the dedup key for the reachability closure, keyed on the dotted identity
    // no local name can collide with, so a declaration pulled two ways merges exactly once.
    let mut merged_q: HashSet<String> = HashSet::new();
    let mut errors: Vec<LoadDiagnostic> = Vec::new();
    // Retained (unresolved) imports — std imports and opaque-stub fallbacks — deduped by (path, name)
    // across the entry and every dependency so a shared `use std.…` isn't bound twice.
    let mut seen_retained: HashSet<(Vec<String>, String)> = HashSet::new();
    let mut dep_retained: Vec<Stmt> = Vec::new();
    let mut entry_stmts: Vec<Stmt> = Vec::with_capacity(entry_program.stmts.len());

    // Resolve one `use`'s names against the pool. Resolved names merge their declaration (deduped by
    // origin); unresolved names are collected for retention. Returns the still-unresolved names.
    let mut drive_use = |path: &[String],
                         names: &[UseName],
                         imported: &mut Vec<Stmt>,
                         errors: &mut Vec<LoadDiagnostic>|
     -> Vec<UseName> {
        let mut unresolved = Vec::new();
        for name in names {
            match resolve(&module_views, path, &name.name) {
                // Keyed on the import's *local* (alias-aware) name: `use App.A.User as AUser` and
                // `use App.B.User as BUser` bind distinct locals and coexist, while two imports (or an
                // import and a local decl) sharing one local name are still the E0020 clash.
                Resolution::Resolved(decl) => match origins.get(name.local()) {
                    None => {
                        origins.insert(name.local().to_string(), Origin::Import(path.to_vec()));
                        // Merge the imported declaration under its module's qualified identity, then
                        // drag in its same-module transitive dependencies — every internal helper it
                        // calls and every module-local type it names (params/returns/fields/bodies,
                        // and a `@tier` config, now just one edge of the general closure). Without
                        // this, an exported declaration that references anything non-leaf in its own
                        // module leaves that reference out of the merged program (E0005/E0004).
                        let mut decl = *decl;
                        if let Some(map) = module_maps.get(path) {
                            qualify::qualify_stmt(&mut decl, map);
                        }
                        imported.push(decl);
                        merge_module_closure(
                            path,
                            &name.name,
                            &module_views,
                            &module_maps,
                            &mut merged_q,
                            imported,
                        );
                    }
                    // Same declaration re-imported (same namespace) — merge once, ignore the rest.
                    Some(Origin::Import(p)) if p.as_slice() == path => {}
                    // A different declaration under the same local name — ambiguous.
                    Some(_) => errors.push(collision_error(entry, path, name)),
                },
                // No loaded module declares this namespace. Whether that is an error depends on how
                // much of the dependency graph the caller knows — see [`RetainPolicy`]. Either way,
                // a std extension (`std.http`) and a declared native-dependency root are always
                // retained (resolved downstream by the checker/compiler or the composed toolchain).
                Resolution::NoModule => {
                    let root = path.first();
                    let retained = match &retain {
                        RetainPolicy::Lenient => !root.is_some_and(|r| project_roots.contains(r)),
                        RetainPolicy::Complete { native_roots } => root
                            .is_some_and(|r| reg.is_extension_root(r) || native_roots.contains(r)),
                    };
                    if retained {
                        unresolved.push(name.clone());
                    } else {
                        let suggestion = noeta_diagnostics::closest(
                            &path.join("."),
                            import_targets.iter().map(String::as_str),
                        );
                        errors.push(unknown_module_error(entry, path, name, suggestion));
                    }
                }
                Resolution::Private => {
                    errors.push(import_error(entry, path, name, Visibility::Private))
                }
                Resolution::Missing => {
                    errors.push(import_error(entry, path, name, Visibility::Missing))
                }
            }
        }
        unresolved
    };

    // The entry: its `use`s drive imports; its other statements are the program's tail (in order).
    for stmt in &entry_program.stmts {
        match stmt {
            Stmt::Use { path, names, span } => {
                let unresolved = drive_use(path, names, &mut imported, &mut errors);
                let fresh = retain_fresh(&mut seen_retained, path, unresolved);
                if !fresh.is_empty() {
                    entry_stmts.push(Stmt::Use {
                        path: path.clone(),
                        names: fresh,
                        span: *span,
                    });
                }
            }
            other => {
                // The entry's own declarations and statements qualify against the entry's map (its
                // own namespace + its resolved imports).
                let mut stmt = other.clone();
                qualify::qualify_stmt(&mut stmt, &entry_map);
                entry_stmts.push(stmt);
            }
        }
    }

    // Each dependency module's `use`s also drive imports (closed unit); their unresolved remainder
    // (std imports) is retained up front, ahead of the entry's statements.
    for driver in dep_drivers {
        for stmt in &driver.stmts {
            if let Stmt::Use { path, names, span } = stmt {
                let unresolved = drive_use(path, names, &mut imported, &mut errors);
                let fresh = retain_fresh(&mut seen_retained, path, unresolved);
                if !fresh.is_empty() {
                    dep_retained.push(Stmt::Use {
                        path: path.clone(),
                        names: fresh,
                        span: *span,
                    });
                }
            }
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    // Merged declarations, then dependency std-imports, then the entry's own statements.
    let mut stmts = imported;
    stmts.append(&mut dep_retained);
    stmts.append(&mut entry_stmts);
    Ok(Program {
        stmts,
        span: entry_program.span,
    })
}

/// Filter `names` down to those not yet retained under `path`, recording the fresh ones — so a
/// `use std.…` shared by the entry and several dependencies is retained exactly once.
fn retain_fresh(
    seen: &mut HashSet<(Vec<String>, String)>,
    path: &[String],
    names: Vec<UseName>,
) -> Vec<UseName> {
    names
        .into_iter()
        .filter(|n| seen.insert((path.to_vec(), n.name.clone())))
        .collect()
}

/// A candidate module viewed for resolution: its declared namespace path and a borrow of its
/// statements (so resolution clones only the declarations it actually imports).
struct ModuleView<'a> {
    namespace: Vec<String>,
    stmts: &'a [Stmt],
}

/// The namespace a module declares (`namespace App.Models;`), if any.
fn module_namespace(program: &Program) -> Option<Vec<String>> {
    program.stmts.iter().find_map(|stmt| match stmt {
        Stmt::Namespace { path, .. } => Some(path.clone()),
        _ => None,
    })
}

/// The name a top-level declaration that gains a **qualified identity** introduces — a class, struct,
/// enum, or free function. `None` for everything else. Both user types *and* user functions are
/// namespace-scoped: two same-named ones from different namespaces coexist, each keyed on its qualified
/// identity. (A method is not top-level — it resolves through its type, so it is not here.)
fn qualifiable_decl_name(stmt: &Stmt) -> Option<&str> {
    match stmt {
        Stmt::Class(decl) => Some(&decl.name),
        Stmt::Struct(decl) => Some(&decl.name),
        Stmt::Enum(decl) => Some(&decl.name),
        Stmt::Fn(decl) => Some(&decl.name),
        // A user-defined trait is a qualifiable declaration (L1): a `dyn Trait` type, a `<T: Trait>`
        // bound, or an `impl Trait for T` referencing a module-local trait drags its declaration into
        // the merged program via the cross-module closure — without this a package-local trait
        // (e.g. aether's `Middleware`) is "unknown" once the package is linked as a dependency.
        Stmt::Trait(decl) => Some(&decl.name),
        _ => None,
    }
}

/// Merge `root`'s **same-module transitive dependencies** into `imported` — the cross-module
/// reachability closure. Importing an exported declaration must drag in every same-module
/// declaration it references, or the merged program is missing them and fails to compile: an
/// exported `pub fn` that calls an internal (non-`pub`) helper, or names a module-local type in a
/// parameter / return / field, would otherwise leave `helper` / that type out.
///
/// Starting from the explicitly-imported `root` (already merged by the caller under its local name),
/// follow every reference that names another top-level declaration of the **same** module — a called
/// helper, a param/return/field/body type, a `@tier` config — to a fixpoint, merging each reachable
/// declaration under its **qualified identity** (deduped through `merged_q`, so a declaration reached
/// two ways — or also explicitly imported — merges exactly once). Visibility does *not* gate an
/// intra-module reference, so a non-`pub` internal helper is pulled. Cross-module references are
/// separate `use`s that drive their own imports (a dependency re-drives its own `use`s), so the walk
/// stays scoped to `path`'s own declarations. The `@tier(…, config: T)` pull is now just one edge of
/// this closure — `referenced_names` sees the config type like any other reference.
fn merge_module_closure(
    path: &[String],
    root: &str,
    module_views: &[ModuleView],
    module_maps: &std::collections::HashMap<Vec<String>, qualify::QMap>,
    merged_q: &mut HashSet<String>,
    imported: &mut Vec<Stmt>,
) {
    let Some(module) = module_views.iter().find(|m| m.namespace == path) else {
        return;
    };
    // The module's own top-level declaration names — the only targets an intra-module reference can
    // resolve to (everything else — params, builtins, externs, other modules — is left to its own
    // resolution path).
    let own_names: HashSet<&str> = module
        .stmts
        .iter()
        .filter_map(qualifiable_decl_name)
        .collect();
    let qualified = |name: &str| format!("{}.{}", path.join("."), name);
    let find = |name: &str| {
        module
            .stmts
            .iter()
            .find(|s| qualifiable_decl_name(s) == Some(name))
    };

    // The root is already merged (under its local name); record its qualified identity so it is not
    // merged again, and expand its references. A worklist iterated to a fixpoint; `merged_q`
    // membership makes cycles terminate.
    merged_q.insert(qualified(root));
    let mut work: Vec<String> = vec![root.to_string()];
    while let Some(name) = work.pop() {
        let Some(decl) = find(&name) else { continue };
        for referenced in qualify::referenced_names(decl) {
            if own_names.contains(referenced.as_str()) && merged_q.insert(qualified(&referenced)) {
                // A fresh same-module declaration: merge it under its qualified identity and expand.
                if let Some(dep) = find(&referenced) {
                    let mut dep = dep.clone();
                    if let Some(map) = module_maps.get(path) {
                        qualify::qualify_stmt(&mut dep, map);
                    }
                    imported.push(dep);
                    work.push(referenced);
                }
            }
        }
    }
}

/// Whether some loaded module with namespace `path` declares a top-level, qualifiable `name` — i.e.
/// `name` is a *user* type or function reachable at `path`, as opposed to an extern (`std.…`, which is
/// no loaded module) or an opaque-stub fallback. Drives [`build_module_map`]: only names that resolve to
/// a real module declaration are qualified; everything else stays bare for its own resolution path.
fn module_declares(modules: &[ModuleView], path: &[String], name: &str) -> bool {
    modules.iter().any(|m| {
        m.namespace == path
            && m.stmts
                .iter()
                .any(|s| qualifiable_decl_name(s) == Some(name))
    })
}

/// Build a module's namespace-qualification map (arc Phase B): its local type/function names →
/// qualified identities. Two sources feed it:
///
/// - **Own declarations** qualify under this module's own namespace (`User` → `App.Models.User`,
///   `boom` → `App.Math.boom`), but only when the module *has* a namespace — a non-namespaced module
///   contributes nothing, so its names stay bare and the file is byte-identical.
/// - **Imports that resolve to a loaded module** qualify to that module's identity, keyed by the
///   import's local (alias-aware) name (`use App.A.User as AUser` → `AUser` → `App.A.User`). An
///   extern or opaque-stub import resolves to no module ([`module_declares`] is false) and is skipped.
fn build_module_map(
    own_ns: &[String],
    own_stmts: &[Stmt],
    modules: &[ModuleView],
) -> qualify::QMap {
    let mut map = qualify::QMap::new();
    if !own_ns.is_empty() {
        let prefix = own_ns.join(".");
        for stmt in own_stmts {
            if let Some(name) = qualifiable_decl_name(stmt) {
                map.insert(name.to_string(), format!("{prefix}.{name}"));
            }
        }
    }
    for stmt in own_stmts {
        if let Stmt::Use { path, names, .. } = stmt {
            for n in names {
                if module_declares(modules, path, &n.name) {
                    map.insert(
                        n.local().to_string(),
                        format!("{}.{}", path.join("."), n.name),
                    );
                }
            }
        }
    }
    map
}

/// How the linker treats a `use` whose namespace no loaded module declares — the choice turns on
/// how much of the dependency graph the caller can see, which is what makes "unknown import" a
/// reliable error in one context and a false positive in another.
enum RetainPolicy<'a> {
    /// **Incomplete** dependency knowledge (single file, sibling-only linking, the IDE `linked`
    /// query): only an *intra-project* import — a root the project itself declares — can be judged
    /// missing; every other unresolved `use` is retained, because it may be a dependency this path
    /// never resolved. Keeps the loader from flagging foreign roots it cannot see.
    Lenient,
    /// **Complete** dependency graph (the CLI, with a resolved manifest): every legitimate import
    /// root is known — the std extensions plus each declared native-dependency root (`native_roots`,
    /// whose members the composed toolchain validates). Anything else that resolves to no module is
    /// a genuine error: a missing intra-project module, a typo'd dependency module, or a `use` of an
    /// undeclared package — the case foreign, hard-to-spell package names most often hit.
    Complete { native_roots: &'a HashSet<String> },
}

/// The outcome of resolving one imported name against the loaded modules.
enum Resolution {
    /// The namespace exists and exports the name (a `pub` declaration): merge this clone. Boxed
    /// because the declaration statement is far larger than the unit variants.
    Resolved(Box<Stmt>),
    /// The namespace exists and declares the name, but it is not `pub`.
    Private,
    /// The namespace exists but declares no such name.
    Missing,
    /// No loaded module declares the namespace — fall back to the opaque stub (M0 behavior).
    NoModule,
}

/// Resolve the name `name` of namespace `path` against the loaded modules, honoring `pub`
/// visibility: only a `pub` declaration is importable.
fn resolve(modules: &[ModuleView], path: &[String], name: &str) -> Resolution {
    let Some(module) = modules.iter().find(|m| m.namespace == path) else {
        return Resolution::NoModule;
    };
    match module
        .stmts
        .iter()
        .find(|stmt| decl_name(stmt) == Some(name))
    {
        Some(decl) if decl_is_public(decl) => Resolution::Resolved(Box::new(decl.clone())),
        Some(_) => Resolution::Private,
        None => Resolution::Missing,
    }
}

/// Which `E0019` an unresolved import is.
enum Visibility {
    Private,
    Missing,
}

/// Build the `E0019` diagnostic for an import the resolved module does not export, pointed at the
/// imported name in the entry's `use`.
fn import_error(
    entry: &Source,
    path: &[String],
    name: &UseName,
    kind: Visibility,
) -> LoadDiagnostic {
    let namespace = path.join(".");
    let message = match kind {
        Visibility::Private => {
            format!("`{}` is private to module `{namespace}`", name.name)
        }
        Visibility::Missing => format!("module `{namespace}` has no export `{}`", name.name),
    };
    LoadDiagnostic {
        source: entry.clone(),
        diagnostic: Diagnostic::error(DiagnosticCode::UnresolvedImport, name.span, message),
    }
}

/// Build the `E0019` diagnostic for a `use` whose module namespace is declared nowhere in the
/// linked workspace — a typo'd or missing sibling/dependency module (`use App.Modles.User`). Only
/// raised in a complete link (the linker sees the whole pool); a single-file check never runs the
/// linker, so an isolated file's forward references stay lenient.
fn unknown_module_error(
    entry: &Source,
    path: &[String],
    name: &UseName,
    suggestion: Option<&str>,
) -> LoadDiagnostic {
    let namespace = path.join(".");
    let mut diagnostic = Diagnostic::error(
        DiagnosticCode::UnresolvedImport,
        name.span,
        format!("no module `{namespace}` in this project"),
    );
    if let Some(s) = suggestion {
        diagnostic.help(format!("did you mean `{s}`?"));
    }
    LoadDiagnostic {
        source: entry.clone(),
        diagnostic,
    }
}

/// Build the `E0020` diagnostic for an import whose name collides with another top-level name in
/// the entry (a second import of it, or a local declaration), pointed at the imported name.
fn collision_error(entry: &Source, path: &[String], name: &UseName) -> LoadDiagnostic {
    let namespace = path.join(".");
    LoadDiagnostic {
        source: entry.clone(),
        diagnostic: Diagnostic::error(
            DiagnosticCode::NameCollision,
            name.span,
            format!(
                "`{}` imported from `{namespace}` collides with another top-level name",
                name.name
            ),
        )
        .with_help("rename or remove the conflicting import or declaration"),
    }
}

/// Whether a top-level declaration is `pub` (importable). Statements that declare no name are
/// never importable.
fn decl_is_public(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Class(d) => d.is_public,
        Stmt::Struct(d) => d.is_public,
        Stmt::Enum(d) => d.is_public,
        Stmt::Fn(d) => d.is_public,
        Stmt::Trait(d) => d.is_public,
        _ => false,
    }
}

/// The name a top-level declaration introduces (a class, struct, enum, or function); `None` for
/// statements that declare no importable name.
fn decl_name(stmt: &Stmt) -> Option<&str> {
    match stmt {
        Stmt::Class(decl) => Some(&decl.name),
        Stmt::Struct(decl) => Some(&decl.name),
        Stmt::Enum(decl) => Some(&decl.name),
        Stmt::Fn(decl) => Some(&decl.name),
        // A user-defined trait is an importable name (L1) — `use pkg.mod.{MyTrait}` brings it into
        // scope for `dyn MyTrait`, a `<T: MyTrait>` bound, or an `impl MyTrait for T`.
        Stmt::Trait(decl) => Some(&decl.name),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_ast::Expr;

    fn module(name: &str, text: &str) -> RawModule {
        RawModule {
            name: name.to_string(),
            text: text.to_string(),
        }
    }

    // --- cross-package linking (package-manager P2.1) -----------------------------------------

    /// The leaf (short) segment of a possibly-qualified identity — `webclient.client.Client` →
    /// `Client`. A namespaced module's declarations carry qualified identities now (arc Phase B), so
    /// these link/re-root tests match on the leaf, which is what they are really asserting.
    fn leaf(name: &str) -> &str {
        name.rsplit_once('.').map_or(name, |(_, leaf)| leaf)
    }

    fn has_class(linked: &Linked, name: &str) -> bool {
        linked
            .program
            .stmts
            .iter()
            .any(|s| matches!(s, Stmt::Class(c) if leaf(&c.name) == name))
    }

    fn has_fn(linked: &Linked, name: &str) -> bool {
        linked
            .program
            .stmts
            .iter()
            .any(|s| matches!(s, Stmt::Fn(f) if leaf(&f.name) == name))
    }

    fn has_struct(linked: &Linked, name: &str) -> bool {
        linked
            .program
            .stmts
            .iter()
            .any(|s| matches!(s, Stmt::Struct(d) if leaf(&d.name) == name))
    }

    #[test]
    fn the_edition_map_keys_every_source_by_its_package() {
        // Two dependency packages, each one module; no siblings. SourceIds: entry = 0, dep A's
        // module = 1, dep B's module = 2. The editions map must record every one of them: the entry
        // under the root edition passed to `link_with_deps`, each dep module under that package's
        // own edition. (One edition ships today, so every value is `E2026`; what this proves is the
        // *keying* — the map is populated, not left empty, and covers each source — which is the
        // wiring the first edition-gated rule will consult once editions diverge.)
        let dep_a = DepPackage {
            key: "a".to_string(),
            root: "a".to_string(),
            modules: vec![module(
                "a.noe",
                "namespace a;\npub fn ay() -> int {\n  1\n}\n",
            )],
            dep_renames: Default::default(),
            native: false,
            edition: "2026".to_string(),
        };
        let dep_b = DepPackage {
            key: "b".to_string(),
            root: "b".to_string(),
            modules: vec![module(
                "b.noe",
                "namespace b;\npub fn bee() -> int {\n  2\n}\n",
            )],
            dep_renames: Default::default(),
            native: false,
            edition: "2026".to_string(),
        };
        // The entry need not import the deps: their sources are assembled (and thus keyed in the
        // editions map) whether or not a declaration is pulled into the merge.
        let entry = "x = 1;\n";
        let linked = link_with_deps(
            "main.noe",
            entry,
            noeta_lexer::Edition::E2026,
            &[],
            &[dep_a, dep_b],
        )
        .unwrap();

        // Every source is recorded — the map was populated, not left empty.
        assert_eq!(linked.editions.len(), 3, "entry + two dep modules");
        assert!(!linked.editions.is_empty());
        // The entry takes the root edition; each dependency module takes its package's edition.
        assert_eq!(
            linked.editions.source_edition(SourceId(0)),
            noeta_lexer::Edition::E2026,
            "entry under the root edition"
        );
        assert_eq!(
            linked.editions.source_edition(SourceId(1)),
            noeta_lexer::Edition::E2026,
            "dep a's module"
        );
        assert_eq!(
            linked.editions.source_edition(SourceId(2)),
            noeta_lexer::Edition::E2026,
            "dep b's module"
        );
        // An unrecorded source falls back to the default edition.
        assert_eq!(
            linked.editions.source_edition(SourceId(99)),
            noeta_lexer::Edition::DEFAULT
        );
    }

    #[test]
    fn a_dependency_package_is_imported_under_the_consumer_key() {
        // Package `guzzle/http` (root segment `http`) exposes `http.client.Client`; the consumer
        // keys it `webclient` and imports `use webclient.client.Client` — the loader re-roots
        // `http.*` → `webclient.*`.
        let dep = DepPackage {
            key: "webclient".to_string(),
            root: "http".to_string(),
            modules: vec![module(
                "client.noe",
                "namespace http.client;\npub class Client {\n  base: string\n}\n",
            )],
            dep_renames: Default::default(),
            native: false,
            edition: "2026".to_string(),
        };
        let entry = "use webclient.client.Client;\nc = Client { base: \"x\" };\n";
        let linked = link_with_deps(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[],
            std::slice::from_ref(&dep),
        )
        .unwrap();
        assert!(has_class(&linked, "Client"));
        // The consumer's `use` resolved — no opaque stub remains for it.
        assert!(
            !linked
                .program
                .stmts
                .iter()
                .any(|s| matches!(s, Stmt::Use { path, .. } if path == &["webclient".to_string(), "client".to_string()]))
        );
    }

    #[test]
    fn a_typoed_dependency_module_is_an_error() {
        // The dependency provides `webclient.client`, so `webclient` is one of the linked project's
        // roots. A typo in the module path (`webclient.clientt`) resolves to nothing under a known
        // root — a hard error (E0019), not a silent opaque stub. Foreign-package imports are exactly
        // the ones you fat-finger, and the loader *does* validate them (they are in the pool).
        let dep = DepPackage {
            key: "webclient".to_string(),
            root: "http".to_string(),
            modules: vec![module(
                "client.noe",
                "namespace http.client;\npub class Client {\n  base: string\n}\n",
            )],
            dep_renames: Default::default(),
            native: false,
            edition: "2026".to_string(),
        };
        let entry = "use webclient.clientt.Client;\nc = Client { base: \"x\" };\n";
        let errors = link_with_deps(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[],
            std::slice::from_ref(&dep),
        )
        .unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.diagnostic.code == DiagnosticCode::UnresolvedImport),
            "expected E0019 for the typo'd dependency module `webclient.clientt`, got {errors:?}"
        );
    }

    #[test]
    fn a_declared_native_dep_root_is_retained() {
        // A native package contributes no source modules (its members live in its Rust extension,
        // composed in downstream), so `use imgfx.fx` resolves to nothing in the pool. Because `imgfx`
        // is a *declared* native dependency (`native: true`), the loader retains the import for the
        // composed toolchain to validate — it does not flag it.
        let dep = DepPackage {
            key: "imgfx".to_string(),
            root: "imgfx".to_string(),
            modules: Vec::new(),
            dep_renames: Default::default(),
            native: true,
            edition: "2026".to_string(),
        };
        let entry = "use imgfx.fx;\necho fx.double(21);\n";
        let linked = link_with_deps(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[],
            std::slice::from_ref(&dep),
        )
        .expect("retained");
        assert!(
            linked
                .program
                .stmts
                .iter()
                .any(|s| matches!(s, Stmt::Use { path, .. } if path == &["imgfx".to_string()])),
            "the declared native import `use imgfx.fx` should be retained"
        );
    }

    #[test]
    fn an_undeclared_package_root_is_an_error() {
        // With a resolved dependency graph in hand (the complete policy), a `use` under a root that
        // is neither std nor any declared dependency — a misspelled package name (`imgtx` for
        // `imgfx`) or a package never added to the manifest — is an error, not a silent stub. This is
        // the foreign-package typo case: exactly what you cannot catch by eye.
        let dep = DepPackage {
            key: "imgfx".to_string(),
            root: "imgfx".to_string(),
            modules: Vec::new(),
            dep_renames: Default::default(),
            native: true,
            edition: "2026".to_string(),
        };
        let entry = "use imgtx.fx;\necho fx.double(21);\n";
        let errors = link_with_deps(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[],
            std::slice::from_ref(&dep),
        )
        .unwrap_err();
        let err = errors
            .iter()
            .find(|e| e.diagnostic.code == DiagnosticCode::UnresolvedImport)
            .unwrap_or_else(|| panic!("expected E0019 for `imgtx`, got {errors:?}"));
        // The declared `imgfx` is a plausible fix for the typo `imgtx` — offered as a hint.
        assert_eq!(
            err.diagnostic.help.as_deref(),
            Some("did you mean `imgfx`?")
        );
    }

    #[test]
    fn a_package_internal_import_resolves_after_reroot() {
        // The dependency's own module imports a sibling of the same package (`use http.models.Body`);
        // re-rooting rewrites it to `use webclient.models.Body`, and — because a dependency module's
        // `use`s drive imports — `Body` is pulled in even though the consumer never named it.
        let dep = DepPackage {
            key: "webclient".to_string(),
            root: "http".to_string(),
            modules: vec![
                module(
                    "client.noe",
                    "namespace http.client;\nuse http.models.Body;\npub class Client {\n  body: Body\n}\n",
                ),
                module(
                    "models.noe",
                    "namespace http.models;\npub class Body {\n  text: string\n}\n",
                ),
            ],
            dep_renames: Default::default(),
            native: false,
            edition: "2026".to_string(),
        };
        let entry = "use webclient.client.Client;\nc = Client { body: Body { text: \"hi\" } };\n";
        let linked = link_with_deps(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[],
            std::slice::from_ref(&dep),
        )
        .unwrap();
        assert!(has_class(&linked, "Client"));
        assert!(
            has_class(&linked, "Body"),
            "the package-internal Body must be linked in"
        );
    }

    #[test]
    fn two_packages_sharing_a_root_coexist_under_distinct_keys() {
        // Both packages have root segment `http` (e.g. `a/http` and `b/http`); distinct keys keep
        // them apart — the collision the dep-key decoupling exists to prevent.
        let a = DepPackage {
            key: "alpha".to_string(),
            root: "http".to_string(),
            modules: vec![module(
                "a.noe",
                "namespace http.core;\npub class Ping {\n  n: int\n}\n",
            )],
            dep_renames: Default::default(),
            native: false,
            edition: "2026".to_string(),
        };
        let b = DepPackage {
            key: "beta".to_string(),
            root: "http".to_string(),
            modules: vec![module(
                "b.noe",
                "namespace http.core;\npub class Pong {\n  n: int\n}\n",
            )],
            dep_renames: Default::default(),
            native: false,
            edition: "2026".to_string(),
        };
        let entry =
            "use alpha.core.Ping;\nuse beta.core.Pong;\np = Ping { n: 1 };\nq = Pong { n: 2 };\n";
        let linked = link_with_deps(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[],
            &[a, b],
        )
        .unwrap();
        assert!(has_class(&linked, "Ping"));
        assert!(has_class(&linked, "Pong"));
    }

    #[test]
    fn a_transitive_dependency_resolves_through_dep_renames() {
        // The consumer depends on `app` (root `app`), which itself depends on a package it keys
        // `jsonlib` (root `json`). The resolver gives the transitive package the global segment
        // `pkg_json`, so `app`'s internal `use jsonlib.parse.Value` must be rewritten to
        // `use pkg_json.parse.Value` — even though the consumer never mentions `jsonlib`/`pkg_json`.
        let mut app_renames = std::collections::BTreeMap::new();
        app_renames.insert("jsonlib".to_string(), "pkg_json".to_string());
        let app = DepPackage {
            key: "app".to_string(),
            root: "app".to_string(),
            modules: vec![module(
                "widget.noe",
                "namespace app.core;\nuse jsonlib.parse.Value;\npub class Widget {\n  v: Value\n}\n",
            )],
            dep_renames: app_renames,
            native: false,
            edition: "2026".to_string(),
        };
        let json = DepPackage {
            key: "pkg_json".to_string(),
            root: "json".to_string(),
            modules: vec![module(
                "parse.noe",
                "namespace json.parse;\npub class Value {\n  n: int\n}\n",
            )],
            dep_renames: Default::default(),
            native: false,
            edition: "2026".to_string(),
        };
        let entry = "use app.core.Widget;\nw = Widget { v: Value { n: 1 } };\n";
        let linked = link_with_deps(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[],
            &[app, json],
        )
        .unwrap();
        assert!(has_class(&linked, "Widget"));
        assert!(
            has_class(&linked, "Value"),
            "the transitive package's Value must link in via the dep-key rewrite"
        );
    }

    #[test]
    fn a_dependency_std_import_is_retained_for_the_compiler() {
        // A package that `use`s `std.*` internally: the loader can't resolve std (not a module), so
        // the `use std.math.sqrt` is retained in the merged program for the compiler to bind.
        let dep = DepPackage {
            key: "geo".to_string(),
            root: "shapes".to_string(),
            modules: vec![module(
                "circle.noe",
                "namespace shapes.circle;\nuse std.math.sqrt;\npub fn area(r: float): float { return 3.14 * r * r; }\n",
            )],
            dep_renames: Default::default(),
            native: false,
            edition: "2026".to_string(),
        };
        let entry = "use geo.circle.area;\necho area(2.0);\n";
        let linked = link_with_deps(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[],
            std::slice::from_ref(&dep),
        )
        .unwrap();
        // `use std.math.sqrt` survives (retained) so the native-module resolver still sees it.
        assert!(
            linked.program.stmts.iter().any(|s| matches!(
                s,
                Stmt::Use { path, .. } if path == &["std".to_string(), "math".to_string()]
            )),
            "the dependency's std import must be retained"
        );
    }

    #[test]
    fn links_a_used_module_declaration() {
        let models = module(
            "models.noe",
            "namespace App.Models;\npub class User {\n  name: string\n  id: int\n  fn new(name: string, id: int): User { return User { name: name, id: id }; }\n}\n",
        );
        let entry =
            "namespace App.Main;\nuse App.Models.User;\nu = User.new(\"Ada\", 7);\necho u.name;\n";
        let linked = link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&models),
        )
        .unwrap();
        // The real `User` class is merged in under its qualified identity `App.Models.User`
        // (arc Phase B); its `use` is dropped (no opaque stub for it).
        assert!(
            linked
                .program
                .stmts
                .iter()
                .any(|s| matches!(s, Stmt::Class(c) if c.name == "App.Models.User"))
        );
        // The entry's reference `User.new(...)` was rewritten to the qualified identity too, so it
        // binds the merged class at runtime.
        let Stmt::Binding { value, .. } = linked
            .program
            .stmts
            .iter()
            .find(|s| matches!(s, Stmt::Binding { name, .. } if name == "u"))
            .expect("the `u = …` binding")
        else {
            unreachable!()
        };
        let Expr::Call { callee, .. } = value else {
            panic!("a call")
        };
        let Expr::Member { receiver, .. } = &**callee else {
            panic!("a member")
        };
        assert!(matches!(&**receiver, Expr::Ident { name, .. } if name == "App.Models.User"));
        assert!(
            !linked
                .program
                .stmts
                .iter()
                .any(|s| matches!(s, Stmt::Use { .. }))
        );
    }

    #[test]
    fn unresolved_intra_project_module_is_an_error() {
        // The entry lives under `namespace App.Orders`, so `App` is one of the project's own roots.
        // No sibling provides `App.Models`, so `use App.Models.User` is an intra-project reference to
        // a module that does not exist — a hard error (E0019), not a silent opaque stub (the
        // check/run divergence for user modules).
        let entry =
            "namespace App.Orders;\nuse App.Models.User;\npub fn make(): ?User { return none; }\n";
        let errors = link("main.noe", entry, noeta_lexer::Edition::DEFAULT, &[]).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.diagnostic.code == DiagnosticCode::UnresolvedImport),
            "expected E0019 for the missing intra-project module `App.Models`, got {errors:?}"
        );
    }

    #[test]
    fn unresolved_external_root_is_retained_not_flagged() {
        // A `use` under a root that is NOT part of the project's namespace tree — an external/native
        // package (`imgfx`, resolved by the composed runtime) — must be retained, never flagged: the
        // loader cannot adjudicate roots it does not own. Here the entry declares `App.Orders`, so
        // `imgfx` is foreign; the link succeeds and keeps the `use` for downstream resolution.
        let entry = "namespace App.Orders;\nuse imgfx.fx;\npub fn go(): int { return 0; }\n";
        let linked = link("main.noe", entry, noeta_lexer::Edition::DEFAULT, &[])
            .expect("external-root use is retained, not an error");
        assert!(
            linked
                .program
                .stmts
                .iter()
                .any(|s| matches!(s, Stmt::Use { path, .. } if path == &["imgfx".to_string()])),
            "the foreign `use imgfx.fx` should be retained"
        );
    }

    #[test]
    fn importing_a_private_declaration_is_e0019() {
        // The namespace exists and declares `User`, but it is not `pub` → a hard error.
        let models = module(
            "models.noe",
            "namespace App.Models;\nclass User { id: int }\n",
        );
        let entry = "use App.Models.User;\n";
        let errs = link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&models),
        )
        .unwrap_err();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].diagnostic.code, DiagnosticCode::UnresolvedImport);
    }

    #[test]
    fn importing_a_missing_export_is_e0019() {
        // The namespace exists but declares no `Ghost`.
        let models = module(
            "models.noe",
            "namespace App.Models;\npub class User { id: int }\n",
        );
        let entry = "use App.Models.Ghost;\n";
        let errs = link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&models),
        )
        .unwrap_err();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].diagnostic.code, DiagnosticCode::UnresolvedImport);
    }

    #[test]
    fn an_import_colliding_with_a_local_declaration_is_e0020() {
        // The entry imports `User` but also declares its own `User` — the reference is ambiguous.
        let models = module(
            "models.noe",
            "namespace App.Models;\npub class User { id: int }\n",
        );
        let entry = "use App.Models.User;\nclass User { name: string }\n";
        let errs = link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&models),
        )
        .unwrap_err();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].diagnostic.code, DiagnosticCode::NameCollision);
    }

    #[test]
    fn two_imports_of_the_same_name_collide() {
        // Two modules each export `User`; importing both leaves an ambiguous reference.
        let models = module(
            "models.noe",
            "namespace App.Models;\npub class User { id: int }\n",
        );
        let people = module(
            "people.noe",
            "namespace App.People;\npub class User { name: string }\n",
        );
        let entry = "use App.Models.User;\nuse App.People.User;\n";
        let errs = link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[models, people],
        )
        .unwrap_err();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].diagnostic.code, DiagnosticCode::NameCollision);
    }

    #[test]
    fn two_same_named_types_from_distinct_namespaces_coexist_via_aliases() {
        // The whole arc's deliverable, at the linker level: two modules each export `User`; the entry
        // imports both under distinct aliases, so each merges under its own qualified identity and the
        // entry's references bind the right one — no collision (arc Phase B).
        let models = module(
            "models.noe",
            "namespace App.Models;\npub class User { id: int }\n",
        );
        let people = module(
            "people.noe",
            "namespace App.People;\npub class User { name: string }\n",
        );
        let entry = "use App.Models.User as MUser;\nuse App.People.User as PUser;\n\
                     a = MUser { id: 1 };\nb = PUser { name: \"x\" };\n";
        let linked = link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[models, people],
        )
        .unwrap();

        // Both classes are present under their full qualified identities — distinct, coexisting.
        let names: Vec<&str> = linked
            .program
            .stmts
            .iter()
            .filter_map(|s| match s {
                Stmt::Class(c) => Some(c.name.as_str()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"App.Models.User"), "got {names:?}");
        assert!(names.contains(&"App.People.User"), "got {names:?}");

        // Each aliased constructor was rewritten to the identity it resolves to.
        let ctor = |binding: &str| -> String {
            let Stmt::Binding { value, .. } = linked
                .program
                .stmts
                .iter()
                .find(|s| matches!(s, Stmt::Binding { name, .. } if name == binding))
                .expect("binding")
            else {
                unreachable!()
            };
            let Expr::Object(lit) = value else {
                panic!("object literal")
            };
            lit.type_name.clone()
        };
        assert_eq!(ctor("a"), "App.Models.User");
        assert_eq!(ctor("b"), "App.People.User");
    }

    #[test]
    fn two_same_named_functions_from_distinct_namespaces_coexist_via_aliases() {
        // Functions are namespace-scoped like types (arc Phase B): two modules each export a `scale`,
        // imported under distinct aliases, each merged under its own qualified identity so the entry's
        // aliased calls resolve to the right one.
        let metric = module(
            "metric.noe",
            "namespace App.Metric;\npub fn scale(n: int): int { return n * 10; }\n",
        );
        let audio = module(
            "audio.noe",
            "namespace App.Audio;\npub fn scale(n: int): int { return n + 100; }\n",
        );
        let entry = "use App.Metric.scale as mscale;\nuse App.Audio.scale as ascale;\n\
                     echo mscale(1);\necho ascale(1);\n";
        let linked = link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[metric, audio],
        )
        .unwrap();

        let fn_names: Vec<&str> = linked
            .program
            .stmts
            .iter()
            .filter_map(|s| match s {
                Stmt::Fn(f) => Some(f.name.as_str()),
                _ => None,
            })
            .collect();
        assert!(fn_names.contains(&"App.Metric.scale"), "got {fn_names:?}");
        assert!(fn_names.contains(&"App.Audio.scale"), "got {fn_names:?}");
    }

    #[test]
    fn entry_parse_error_is_reported_against_the_entry() {
        let errs = link("main.noe", "echo $;", noeta_lexer::Edition::DEFAULT, &[]).unwrap_err();
        assert!(!errs.is_empty());
        assert!(errs.iter().all(|e| e.source.name() == "main.noe"));
    }

    #[test]
    fn merged_declaration_spans_carry_the_sibling_source_id() {
        // The `User` class is declared in the sibling (SourceId 1) and merged into the entry
        // program. Its spans — including those deep in a method body — must stay tagged with the
        // sibling's id so a diagnostic on them renders against `models.noe`, not the entry.
        let models = module(
            "models.noe",
            "namespace App.Models;\npub class User {\n  id: int\n  fn bad(): int { return 1 / 0; }\n}\n",
        );
        let entry = "use App.Models.User;\nu = User { id: 1 };\n";
        let linked = link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&models),
        )
        .unwrap();

        // The merged class statement and everything under it belong to source 1 (the sibling).
        let class = linked
            .program
            .stmts
            .iter()
            .find(|s| matches!(s, Stmt::Class(c) if c.name == "App.Models.User"))
            .expect("merged User class");
        assert_eq!(class.span().source, SourceId(1));

        // The entry's own statements stay tagged with source 0.
        let entry_stmt = linked
            .program
            .stmts
            .iter()
            .find(|s| matches!(s, Stmt::Binding { .. }))
            .expect("an entry statement");
        assert_eq!(entry_stmt.span().source, SourceId(0));

        // The source map resolves the sibling id to the sibling file.
        assert_eq!(linked.sources.source(SourceId(1)).name(), "models.noe");
    }

    // --- transitive reachability closure (cross-module linker fix) -----------------------------
    //
    // Importing an exported declaration must drag in the same-module declarations it references —
    // an internal helper it calls (B) or a module-local type it names (C). Before the fix the
    // linker merged only the explicitly-imported roots plus the one `@tier` config edge, so any
    // multi-declaration module failed to compile at the consumer.

    /// B (dependency path): an exported `pub fn` calls a **non-`pub`** internal helper. The helper
    /// must be pulled into the merged program even though visibility would forbid importing it
    /// directly (an intra-module reference is not gated by `pub`).
    #[test]
    fn an_imported_fn_drags_in_its_internal_helper() {
        let dep = DepPackage {
            key: "mathx".to_string(),
            root: "mathx".to_string(),
            modules: vec![module(
                "lib.noe",
                "namespace mathx.lib;\n\
                 fn helper(n: int): int { return n * 2; }\n\
                 pub fn twice(n: int): int { return helper(n); }\n",
            )],
            dep_renames: Default::default(),
            native: false,
            edition: "2026".to_string(),
        };
        let entry = "use mathx.lib.twice;\necho twice(21);\n";
        let linked = link_with_deps(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[],
            std::slice::from_ref(&dep),
        )
        .unwrap();
        assert!(has_fn(&linked, "twice"), "the imported fn is merged");
        assert!(
            has_fn(&linked, "helper"),
            "its internal helper must be dragged in transitively"
        );
    }

    /// C (dependency path): an exported `pub fn` names a module-local type in its return (and
    /// constructs it). The type must be pulled in.
    #[test]
    fn an_imported_fn_drags_in_a_module_local_type() {
        let dep = DepPackage {
            key: "widgets".to_string(),
            root: "widgets".to_string(),
            modules: vec![module(
                "lib.noe",
                "namespace widgets.lib;\n\
                 struct Point {\n  x: int\n  y: int\n}\n\
                 pub fn origin(): Point { return Point { x: 0, y: 0 }; }\n",
            )],
            dep_renames: Default::default(),
            native: false,
            edition: "2026".to_string(),
        };
        let entry = "use widgets.lib.origin;\np = origin();\necho p.x;\n";
        let linked = link_with_deps(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[],
            std::slice::from_ref(&dep),
        )
        .unwrap();
        assert!(has_fn(&linked, "origin"), "the imported fn is merged");
        assert!(
            has_struct(&linked, "Point"),
            "the module-local return type must be dragged in transitively"
        );
    }

    /// B/C reach transitively: the pulled helper itself references a further internal type, which
    /// must also be pulled (the closure runs to a fixpoint, not one level deep).
    #[test]
    fn the_closure_is_transitive_to_a_fixpoint() {
        let dep = DepPackage {
            key: "chain".to_string(),
            root: "chain".to_string(),
            modules: vec![module(
                "lib.noe",
                "namespace chain.lib;\n\
                 struct Inner { v: int }\n\
                 fn wrap(n: int): Inner { return Inner { v: n }; }\n\
                 pub fn go(n: int): Inner { return wrap(n); }\n",
            )],
            dep_renames: Default::default(),
            native: false,
            edition: "2026".to_string(),
        };
        let entry = "use chain.lib.go;\ni = go(3);\necho i.v;\n";
        let linked = link_with_deps(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[],
            std::slice::from_ref(&dep),
        )
        .unwrap();
        assert!(has_fn(&linked, "go"));
        assert!(has_fn(&linked, "wrap"), "one hop away must be pulled");
        assert!(
            has_struct(&linked, "Inner"),
            "two hops away (a type the helper references) must be pulled too"
        );
    }

    /// B (plain sibling path): the same reachability holds for same-app sibling modules
    /// (`link_parsed`), not just package dependencies.
    #[test]
    fn a_sibling_imported_fn_drags_in_its_helper_and_type() {
        let sibling = module(
            "lib.noe",
            "namespace app.lib;\n\
             struct Box { n: int }\n\
             fn build(n: int): Box { return Box { n: n }; }\n\
             pub fn make(n: int): Box { return build(n); }\n",
        );
        let entry = "use app.lib.make;\nb = make(5);\necho b.n;\n";
        let linked = link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&sibling),
        )
        .unwrap();
        assert!(has_fn(&linked, "make"));
        assert!(
            has_fn(&linked, "build"),
            "the sibling's internal helper is pulled"
        );
        assert!(
            has_struct(&linked, "Box"),
            "the sibling's local type is pulled"
        );
    }
}
