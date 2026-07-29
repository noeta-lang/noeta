//! Multi-file module loading and linking (M1.9).
//!
//! A program is rooted at an *entry* `.noe` file. The other `.noe` files of its **package** are
//! candidate *modules*, and a module's identity is **derived from where its file sits** (see
//! [`derive`]) — the package's import prefix plus the file's path inside the package.
//! The entry's `use dirscan.deep.nested.{Scanner}` declarations resolve against those derived
//! paths; each resolved name's real declaration is **merged into one [`Program`]** ahead of
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

pub mod derive;
pub mod expand;
mod qualify;

use std::collections::HashSet;
use std::io;
use std::path::Path;

use noeta_ast::{Program, Stmt, UseName};
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_span::{PackageOrigin, Source, SourceId, SourceMap, Span};

pub use derive::{
    MANIFEST_NAME, ModulePath, PackageRoot, derive_module_path, read_package_modules,
};
pub use expand::ExpandedSource;

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
    /// Which **package** each source was read from, keyed by `SourceId` — the provenance the merged
    /// program otherwise destroys. The entry and its siblings are [`PackageOrigin::Root`]; each
    /// dependency package's modules carry that package's global key. The checker consults this per
    /// declaration (via a span's `SourceId`) to enforce the package orphan rule — an
    /// `impl Trait for Type` must live in the same package as the trait or as the type.
    ///
    /// **Compile-time expansion sources are deliberately absent.** Generated code is attributed to
    /// no package, so an impl a directive synthesized is never judged by the orphan rule: the
    /// generating directive may sit on a *dependency's* declaration, which would make "the root
    /// package" the wrong answer rather than merely a missing one.
    pub packages: noeta_span::PackageMap,
    /// Per-package `@`-name resolution tables (`[directives]` and `[tiers]` alike), keyed by
    /// [`PackageOrigin`](noeta_span::PackageOrigin) — the parallel of [`Self::packages`] for
    /// extension `@name` resolution. Built by the package manager from each package's manifest in
    /// its own dependency context; the checker resolves a `@name` by the package that wrote it.
    /// The loader itself has no manifest data, so its own constructions leave this empty; the
    /// driver that resolved the graph fills it in (it holds the resolved tables).
    pub package_uses: noeta_span::PackageUses,
    /// Every **non-Noeta file** a compile-time directive expansion read (an OpenAPI spec and the
    /// documents it `$ref`s, say), as the hooks reported them.
    ///
    /// These are inputs to the program exactly as its `.noe` files are — the difference is only
    /// that the compiler cannot discover them by parsing, so a hook has to say. A consumer that
    /// caches or watches (the salsa layer, `--watch`) registers these alongside the sources;
    /// editing one must re-run the expansion, or the program is built from a spec that no longer
    /// exists in that form.
    pub reads: Vec<String>,
}

/// A diagnostic produced while loading, paired with the source it renders against.
#[derive(Debug)]
pub struct LoadDiagnostic {
    pub source: Source,
    pub diagnostic: Diagnostic,
}

/// Re-point each diagnostic at **the file its span actually indexes**.
///
/// The linking core ([`link_parsed`]/[`link_parsed_with_deps`]) resolves imports over
/// [`Program`]s — it never sees a [`Source`], so it can only build its diagnostics against the
/// entry as a provisional render target. That is right for an entry's own bad `use` and wrong for
/// every other unit: a dependency package's module drives its own `use`s, so an E0019/E0020 raised
/// there carries *that file's* span, and rendering it against the entry's text prints an arbitrary
/// slice of the wrong file (or nothing at all).
///
/// `sources` is the id-ordered source table — `sources[i].id() == SourceId(i)`, the layout every
/// loader path builds. An id outside it is left alone: expansion diagnostics already carry their
/// own generated source, which is not in this table.
pub fn attribute_to_spans(diagnostics: &mut [LoadDiagnostic], sources: &[Source]) {
    for load in diagnostics {
        if let Some(source) = sources.get(load.diagnostic.span.source.0 as usize) {
            load.source = source.clone();
        }
    }
}

/// A module that failed to lex/parse, and so is **absent from the link pool**.
///
/// Dropping these on the floor is what made a syntax error in one file surface as
/// `[E0019] no module \`a.b.c\` in this project` at some *other* file's `use` — the consumer was sent
/// to inspect its own import and the package's naming while the real fault sat unreported in a file
/// it was never told about. A broken module is therefore kept, not discarded:
///
/// - A **dependency package's** broken module is a hard error (see [`link_with_deps`]): a package is
///   a closed unit that must be internally valid, and its files are never anyone's entry, so no other
///   pass would ever report them.
/// - A **sibling's** broken module keeps the historical skip-and-continue policy (a lone script must
///   not fail because an unrelated file in its directory is mid-edit), but a `use` that resolves to
///   nothing is checked against these first: if a broken module *declares that namespace*, its parse
///   error is reported instead of the cascading E0019 ([`link_core`]).
///
/// `namespace` is what makes that attribution possible, and it is read off the module's **token
/// stream**, not its AST: the parser yields no output at all on a hard failure, so a broken module's
/// "partial" program is empty and could never have named itself. See [`namespace_from_tokens`].
#[derive(Debug, Clone)]
pub struct BrokenModule {
    pub source: Source,
    pub namespace: Option<Vec<String>>,
    pub diagnostics: Vec<Diagnostic>,
}

/// The `namespace a.b.c;` a module declares, read straight off its **token stream**.
///
/// The AST cannot answer this for the module that needs it most: chumsky produces no output on a
/// hard parse failure, so a broken module's program is empty. Its `namespace` line, on the other
/// hand, is the file's first statement and virtually always intact — and it is the one fact needed
/// to tell a consumer's unresolved `use` which broken file it is really about. Scanning tokens is
/// deliberately more forgiving than parsing: it stops at the first token that is not part of the
/// dotted path and answers `None` rather than erroring (a second diagnostic about the file that is
/// already reporting one would be noise).
///
/// Public so the salsa layer (`noeta-db`), which lexes and parses through its own memoized queries,
/// builds [`BrokenModule`]s that attribute identically to the CLI's.
pub fn namespace_from_tokens(
    source: &Source,
    tokens: &[noeta_lexer::Token],
) -> Option<Vec<String>> {
    use noeta_lexer::TokenKind;
    let start = tokens
        .iter()
        .position(|t| t.kind == TokenKind::NamespaceKw)?;
    let mut path: Vec<String> = Vec::new();
    // Alternating `Ident` `.` `Ident` …; the path is complete only on a segment (never a trailing dot).
    let mut want_ident = true;
    for token in &tokens[start + 1..] {
        match token.kind {
            TokenKind::Ident if want_ident => {
                let text = &source.text()[token.span.start as usize..token.span.end as usize];
                path.push(text.to_string());
                want_ident = false;
            }
            TokenKind::Dot if !want_ident => want_ident = true,
            _ => break,
        }
    }
    (!want_ident).then_some(path)
}

impl BrokenModule {
    /// This module's parse errors as [`LoadDiagnostic`]s rendering against its own file.
    pub fn load_diagnostics(&self) -> Vec<LoadDiagnostic> {
        self.diagnostics
            .iter()
            .map(|diagnostic| LoadDiagnostic {
                source: self.source.clone(),
                diagnostic: diagnostic.clone(),
            })
            .collect()
    }
}

/// Load and link the program rooted at `entry_path`. Returns the linked program, or the
/// load-time (lex/parse) diagnostics of the entry, each paired with its source. An `io::Error`
/// is only for a failure to read the entry file itself.
pub fn load(
    entry_path: &Path,
    root_edition: noeta_lexer::Edition,
    root: Option<&PackageRoot>,
) -> io::Result<Result<Linked, Vec<LoadDiagnostic>>> {
    let text = std::fs::read_to_string(entry_path)?;
    let name = entry_path.display().to_string();
    let siblings = read_siblings(entry_path, root);
    Ok(link(
        &name,
        &text,
        root_edition,
        &siblings,
        entry_module_path(entry_path, root),
    ))
}

/// One sibling module's identity (display name + source text + the module path its location
/// derives), before parsing. Public so the linker can be driven from in-memory sources in tests.
#[derive(Debug)]
pub struct RawModule {
    pub name: String,
    pub text: String,
    /// The module path derived from where this file sits ([`derive`]). [`ModulePath::Declared`] —
    /// the default, and what an in-memory caller with no package on disk gets — means nothing was
    /// derived and the file's own `namespace` declaration stands.
    pub path: ModulePath,
}

impl RawModule {
    /// A module with **no** derived path: its `namespace` declaration is its identity. The
    /// in-memory/no-package constructor (tests, a lone script's flat sibling scan).
    pub fn declared(name: impl Into<String>, text: impl Into<String>) -> RawModule {
        RawModule {
            name: name.into(),
            text: text.into(),
            path: ModulePath::Declared,
        }
    }
}

/// Load and link the program rooted at `entry_path` **with its dependency packages** — the
/// dependency-aware twin of [`load`] (package-manager P2.1). The entry's siblings resolve as before;
/// each [`DepPackage`]'s modules are additionally re-rooted and linked in. An `io::Error` is only for
/// a failure to read the entry file itself.
pub fn load_with_deps(
    entry_path: &Path,
    root_edition: noeta_lexer::Edition,
    deps: &[DepPackage],
    package_uses: &noeta_span::PackageUses,
    root: Option<&PackageRoot>,
) -> io::Result<Result<Linked, Vec<LoadDiagnostic>>> {
    let text = std::fs::read_to_string(entry_path)?;
    let name = entry_path.display().to_string();
    let siblings = read_siblings(entry_path, root);
    Ok(link_with_deps(
        &name,
        &text,
        root_edition,
        &siblings,
        deps,
        package_uses,
        entry_module_path(entry_path, root),
    ))
}

/// A dependency package's sources, to be linked into the entry under the consumer's import root
/// (package-manager P2.1, model R1). `root` is the package's own namespace root segment (the
/// `package` half of its `[package] name`); `key` is the consumer's dependency-table key. The loader
/// **re-roots** the package's modules from `root` to `key` — rewriting the leading segment of each
/// module's `namespace` and its intra-package `use`s — so the consumer addresses the package as
/// `use <key>.<sub>.Name` while the package's own imports (written against `root`) keep resolving.
///
/// A dependency module's own `use`s **drive** imports, exactly as a sibling's do (a package is a
/// closed unit: its internal cross-references must resolve) — the re-rooting above is what makes its
/// intra-package `use`s address the same modules the consumer's key does.
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
    /// The package's language **edition** — the semantics its source is written against (editions
    /// arc), carried per package from resolution and applied per source (each dependency's modules
    /// lex/parse/check under *its* edition; the entry and siblings under the root's). Typed — the
    /// validated enum, not a free string — so a value that resolution never produced is
    /// unrepresentable here rather than silently degrading to the default. (`noeta-edition` is the
    /// bottom-of-DAG vocabulary crate, so depending on the type costs the loader nothing.)
    pub edition: noeta_lexer::Edition,
    /// This package's resolved `@name` bindings — its `[directives]` and `[tiers]` merged into one map
    /// (local `@name` → the provider namespace root(s) and exported name, per-package naming arc; a
    /// `@name` is one namespace, so the two tables cannot collide). Resolved by the package manager in
    /// **this** package's dependency context; the loader keys them by this package's [`PackageOrigin`]
    /// into the checker's per-package `@name` tables so a `@name` resolves in the source that wrote it.
    /// Empty for a package that uses no extension `@`-directives or tiers.
    pub directives: std::collections::HashMap<String, noeta_span::PackageUse>,
}

/// Re-root a namespace/use path in place: replace its leading segment per the rules
/// (package-manager P2.1/P2.4). If the leading segment is the package's own `root`, it becomes the
/// package's global `key`; otherwise, if it is one of the package's local dependency keys, it becomes
/// that dependency's global segment (`renames`). A path leading with anything else — `std`, or a
/// malformed package path — is left untouched.
/// Public so a caller holding a namespace **outside** a [`Program`] can address it the same way
/// [`reroot_program`] addresses a parsed one — the salsa layer recovers a broken dependency module's
/// namespace from its tokens (it has no usable AST), and that namespace must be re-rooted to the
/// consumer's key or it will never match the `use` that names it.
pub fn reroot_path(
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
///
/// **A module's path is now derived, not declared** ([`derive`]), so the `namespace` rewrite is no
/// longer what gives a dependency its identity — the derivation already produces it under the
/// consumer's key. What the rewrite still does is put a *declared* namespace into the consumer's
/// naming space so [`apply_derived_paths`] can compare like with like: a package that declares
/// `namespace greet.hello` and is keyed `hi` re-roots to `hi.hello`, which is exactly what
/// `hello.noe` derives, so the declaration is a restatement rather than a contradiction. (It is a
/// no-op once the declarations are gone.)
///
/// A declared `namespace` only ever leads with the package's own root, so it is rewritten
/// `root` → `key`; a `use` may lead with the package root (an intra-package reference) or one of the package's local
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
    /// The module path each source's location derives, **index-aligned with the `SourceId`s**: the
    /// entry at 0, the modules at `1..`. A [`Source`] carries a file name, not an identity, and the
    /// salsa graph rebuilds programs from `Source`s alone — so the derivation has to travel beside
    /// them or the query path would fall back to declared namespaces while the batch path derives.
    pub paths: Vec<ModulePath>,
}

/// Read the entry file and its sibling `.noe` modules into labeled [`Source`]s, without lexing,
/// parsing, or linking — the file-system front of the salsa module graph. An `io::Error` is only
/// for a failure to read the entry itself; unreadable siblings are skipped (as in [`read_siblings`]).
/// The exact `Source` sequence (and `SourceId` assignment) `link_with_deps` builds for a
/// workspace: entry = 0, siblings `1..=S` (the `RawWorkspace` order), then each dependency
/// package's modules in package order. THE one place the ordering lives — the startup cache's
/// hit path reconstructs a `SourceMap` for span rendering without re-parsing, and it must agree
/// with the loader's assignment or a cache HIT would attribute panic tracebacks, breakpoints,
/// and diagnostics to the wrong files (an ordering change here is loud; a second hand-rolled
/// copy drifting was silent).
pub fn workspace_sources(workspace: &RawWorkspace, deps: &[DepPackage]) -> Vec<Source> {
    let mut sources = Vec::with_capacity(1 + workspace.modules.len());
    sources.push(workspace.entry.clone());
    sources.extend(workspace.modules.iter().cloned());
    let mut next_id = sources.len() as u32;
    for dep in deps {
        for module in &dep.modules {
            sources.push(Source::new(SourceId(next_id), &module.name, &module.text));
            next_id += 1;
        }
    }
    sources
}

pub fn read_workspace(entry_path: &Path, root: Option<&PackageRoot>) -> io::Result<RawWorkspace> {
    let text = std::fs::read_to_string(entry_path)?;
    let entry = Source::new(SourceId(0), entry_path.display().to_string(), text);
    let mut paths = vec![entry_module_path(entry_path, root)];
    let modules = read_siblings(entry_path, root)
        .into_iter()
        .enumerate()
        .map(|(i, raw)| {
            paths.push(raw.path);
            Source::new(SourceId((i + 1) as u32), raw.name, raw.text)
        })
        .collect();
    Ok(RawWorkspace {
        entry,
        modules,
        paths,
    })
}

/// The module path the **entry** file's own location derives — it is a file of the package like any
/// other, and a sibling may legitimately `use` what it declares.
fn entry_module_path(entry_path: &Path, root: Option<&PackageRoot>) -> ModulePath {
    let Some(root) = root else {
        return ModulePath::Declared;
    };
    root.relative(entry_path)
        .map_or(ModulePath::Declared, |relative| {
            derive_module_path(&root.prefix, relative)
        })
}

/// Gather the entry's **sibling modules**: every other `.noe` file of the package, in sorted order
/// (so `SourceId` assignment and resolution are deterministic).
///
/// With a [`PackageRoot`] this is the package walk ([`read_package_modules`]) — recursive and
/// pruned, exactly as a *dependency* package has always been walked, so `src/deep/nested.noe` is a
/// module of the app just as a dependency's `inner/deep.noe` is a module of the dependency. Without
/// one (a lone script in a directory that is not a package) it stays the flat scan of the entry's
/// own directory: a bare `noeta run` must not recursively swallow whatever tree it happens to stand
/// in, and with no package there is no prefix to derive under either.
///
/// A read failure yields no siblings — a lone file simply links to itself.
fn read_siblings(entry_path: &Path, root: Option<&PackageRoot>) -> Vec<RawModule> {
    let mut modules = match root {
        Some(root) => read_package_modules(root),
        None => {
            let Some(dir) = entry_path.parent() else {
                return Vec::new();
            };
            read_dir_modules(dir)
        }
    };
    // The entry is not its own sibling. Compared by full path under a package walk (two files can
    // share a name in different directories) and by file name otherwise (the flat scan's paths are
    // spelled however the invocation spelled them).
    match root {
        Some(_) => modules.retain(|m| !same_file(Path::new(&m.name), entry_path)),
        None => {
            let entry_name = entry_path.file_name();
            modules.retain(|m| Path::new(&m.name).file_name() != entry_name);
        }
    }
    modules
}

/// Whether two paths name the same file. The package walk spells its paths from the package root
/// while the invocation spells the entry however it likes, so the cheap comparison is tried first
/// and canonicalization only settles the cases it misses — the entry appearing in its own sibling
/// pool would merge every one of its declarations twice.
fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
}

/// Read every `.noe` file directly in `dir` (flat — the sibling scan's scope) as a [`RawModule`],
/// in sorted order. A directory that cannot be read yields no modules; unreadable files are skipped
/// — both matching [`read_siblings`]'s tolerance (it is this scan minus the entry).
///
/// This is the **no-package** scan: nothing here derives a module path, so every module's
/// `namespace` declaration stands. A directory that *is* a package is a tree, and its modules have
/// derived paths — use [`read_package_modules`] with its [`PackageRoot`].
pub fn read_dir_modules(dir: &Path) -> Vec<RawModule> {
    // A bare relative entry (`noeta test app.noe`) has parent `""` — the current directory —
    // but `read_dir("")` errors, which silently dropped every sibling: an E0019 from the very
    // directory the user stands in, while the byte-equivalent `./app.noe` linked fine. Scan `.`
    // instead, but keep the produced paths rooted at the ORIGINAL (empty) prefix, so module
    // names stay byte-equal to how the invocation addresses the entry (`m.noe`, not `./m.noe`)
    // — `noeta check`'s entry-to-pool index match compares those names.
    let bare = dir.as_os_str().is_empty();
    let scan: &Path = if bare { Path::new(".") } else { dir };
    let Ok(entries) = std::fs::read_dir(scan) else {
        return Vec::new();
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| {
            if bare {
                std::path::PathBuf::from(e.file_name())
            } else {
                e.path()
            }
        })
        .filter(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "noe"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|p| {
            let text = std::fs::read_to_string(&p).ok()?;
            Some(RawModule::declared(p.display().to_string(), text))
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
    entry_path: ModulePath,
) -> Result<Linked, Vec<LoadDiagnostic>> {
    // The entry is always SourceId 0; siblings follow. Each module keeps its own source so its
    // spans stay valid and its diagnostics render against it. The deps-free path: entry + siblings
    // are one package, so every source takes the root edition and the root package.
    let entry = Source::new(SourceId(0), entry_name, entry_text);
    let mut sources: Vec<Source> = vec![entry.clone()];
    let mut editions = noeta_lexer::EditionMap::new();
    let mut packages = noeta_span::PackageMap::new();
    editions.set(SourceId(0), root_edition);
    packages.set(SourceId(0), PackageOrigin::Root);
    for (i, raw) in siblings.iter().enumerate() {
        let id = SourceId((i + 1) as u32);
        sources.push(Source::new(id, raw.name.as_str(), raw.text.as_str()));
        editions.set(id, root_edition);
        packages.set(id, PackageOrigin::Root);
    }
    // The deps-free path has no manifest and so no `[tiers]`/`[directives]` bindings — an empty
    // `PackageUses` means [`lex_program`] contributes no per-package renamed text tiers (only a
    // file's own `@tier(…, text)` declarations, which its per-file scan discovers regardless).
    let (lexeds, text_tiers) = lex_program(
        &sources,
        &editions,
        &packages,
        &noeta_span::PackageUses::new(),
    );

    // Entry + siblings parse under the root package's edition (deps-free: no dependency packages,
    // so no other editions are in play). `link_with_deps` is the twin that also links dependencies,
    // each under its own edition.
    let mut entry_parsed =
        noeta_parser::parse_in(&entry, &lexeds[0].tokens, root_edition, &text_tiers);
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

    // Parse each sibling under the root edition. Only cleanly-parsed modules contribute to the
    // resolution pool (a broken module cannot be resolved against), but a broken one is now *kept*
    // rather than dropped: if the entry imports the namespace it declares, the linker reports its
    // parse error instead of the cascading "no module" (see [`BrokenModule`]). Every sibling's
    // `Source` is retained (whether or not it parsed) so the `SourceMap` indices line up with the
    // `SourceId`s the parser stamped onto spans.
    let mut module_programs: Vec<(usize, Program)> = Vec::new();
    let mut broken: Vec<BrokenModule> = Vec::new();
    for (index, (source, lexed)) in sources.iter().zip(&lexeds).enumerate().skip(1) {
        match parse_module(
            source,
            lexed,
            root_edition,
            &text_tiers,
            &siblings[index - 1].path,
        ) {
            Ok(program) => module_programs.push((index, program)),
            Err(module) => broken.push(*module),
        }
    }

    // Derivation decides identity: each file's path becomes its `namespace` (see
    // [`apply_derived_paths`]). Runs before linking, so a collision or a contradicted declaration is
    // reported against the files themselves rather than as the "no module"/"has no export" cascade
    // it used to become at whoever imported them.
    let mut units = vec![DerivedUnit {
        source: &entry,
        path: &entry_path,
        program: &mut entry_parsed.program,
    }];
    units.extend(
        module_programs
            .iter_mut()
            .map(|(index, program)| DerivedUnit {
                source: &sources[*index],
                path: &siblings[*index - 1].path,
                program,
            }),
    );
    let path_diagnostics = apply_derived_paths(units);
    if !path_diagnostics.is_empty() {
        return Err(path_diagnostics);
    }

    let refs: Vec<&Program> = module_programs.iter().map(|(_, p)| p).collect();
    let broken_refs: Vec<&BrokenModule> = broken.iter().collect();
    let Linkage {
        mut program,
        source_maps,
    } = link_parsed(&entry, &entry_parsed.program, &refs, &broken_refs).map_err(|mut d| {
        attribute_to_spans(&mut d, &sources);
        d
    })?;
    let reads = expand_into(
        &mut program,
        &source_maps,
        &mut sources,
        &mut editions,
        root_edition,
        &text_tiers,
    )?;
    Ok(Linked {
        program,
        entry,
        sources: SourceMap::new(sources),
        editions,
        packages,
        package_uses: noeta_span::PackageUses::new(),
        reads,
    })
}

/// Run compile-time directive expansion over the linked program, appending each expansion's source
/// to `sources` and its edition to `editions` in lock-step.
///
/// The one place expansion happens, called from every link path so the CLI and the IDE cannot end
/// up with different ideas of what a decorated type's members are. Generated code takes the root
/// package's edition: it was written by the extension the *root* installed, for this program.
///
/// Expansion sources continue the id numbering, so a diagnostic inside generated code resolves
/// through the same [`SourceMap`] as one in a hand-written file.
fn expand_into(
    program: &mut Program,
    source_maps: &std::collections::HashMap<SourceId, qualify::UnitMap>,
    sources: &mut Vec<Source>,
    editions: &mut noeta_lexer::EditionMap,
    root_edition: noeta_lexer::Edition,
    text_tiers: &noeta_lexer::TextTiers,
) -> Result<Vec<String>, Vec<LoadDiagnostic>> {
    let next_id = sources.len() as u32;
    let (expansions, reads, diagnostics) = run_expansion(
        program,
        source_maps,
        || sources.clone(),
        next_id,
        root_edition,
        text_tiers,
    );
    // A failed expansion fails the link (a program cannot be checked without the members its
    // directives declare). The batch CLI is single-shot — it does not watch — so the error-path
    // reads have no consumer here and are dropped with the error; the salsa path
    // (`noeta_db::linked_from`) is the one that keeps them, for the watcher.
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    for expansion in expansions {
        editions.set(expansion.source.id(), root_edition);
        sources.push(expansion.source);
    }
    Ok(reads)
}

/// Decide whether to expand, and run it — the **one** place either happens.
///
/// Split from [`expand_into`] because the other callers cannot append to a shared source list at
/// all: [`ParsedDir::link_entry`] links from `&self` over an immutable directory parse, and
/// `noeta_db::linked_from` is a salsa query over inputs it may not mutate. Both hand their caller
/// the expansion sources to render against instead ([`EntryLink::expansions`]). Every path must
/// nonetheless agree on when a directive expands and what its output is — otherwise the editor and
/// the compiler disagree about a decorated type's members, which is the whole reason this function
/// is one function — so the decision lives here and the callers differ only in what they do with the
/// sources that come back.
///
/// `next_id` is the first unused [`SourceId`], and `sources` is a **provider**: the caller's sources
/// in id order, materialized only if there is something to expand. The provider exists for
/// `noeta_db::linked_from`, whose sources live in salsa inputs and cost a text clone each to
/// reconstruct — a price no program without an expanding directive (nearly all of them) should pay
/// per keystroke. Callers that already hold a slice pass a cheap clone of it.
///
/// Returns the expansion sources (already id'd, each with the directive that produced it), every
/// file the hooks reported reading, and the diagnostics.
///
/// The three are returned **side by side, not as a `Result`**, because `reads` must survive a
/// failed expansion: a hook that failed because its spec was missing still reported that spec, and
/// the rebuild trigger needs it so that *creating* the file re-runs the expansion. A `Result` would
/// have discarded the reads on the `Err`, which is the exact case that matters most. Callers fail
/// the link when `diagnostics` is non-empty, but register `reads` either way.
pub fn run_expansion(
    program: &mut Program,
    source_maps: &std::collections::HashMap<SourceId, qualify::UnitMap>,
    sources: impl FnOnce() -> Vec<Source>,
    next_id: u32,
    root_edition: noeta_lexer::Edition,
    text_tiers: &noeta_lexer::TextTiers,
) -> (Vec<ExpandedSource>, Vec<String>, Vec<LoadDiagnostic>) {
    let registry = noeta_ext_abi::registry::single_registry_process();
    if !expand::has_expansions(program, registry) {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    // Expansion ids continue past the sources this link already has. That numbering is **per
    // render**, deliberately: in the directory mode two different entries of the same directory can
    // each expand and each start at the same next id, because an entry's expansions are only ever
    // resolved through that entry's own map ([`ParsedDir::source_map_with`]). Do not "fix" this into
    // a directory-global counter — the ids would then be gapped and meaningless in every map that
    // holds only one entry's expansions.
    let sources = sources();
    let expanded = expand::expand_program(
        program,
        source_maps,
        &sources,
        next_id,
        root_edition,
        text_tiers,
        registry,
    );
    (expanded.sources, expanded.reads, expanded.diagnostics)
}

/// Link the entry against its sibling modules **and its dependency packages** (package-manager P2.1).
/// Each [`DepPackage`]'s modules are parsed, re-rooted from the package's own root segment to the
/// consumer's dependency key ([`reroot_program`]), and linked as a closed unit (their own `use`s
/// drive imports). SourceIds continue past the siblings (entry = 0, siblings `1..=S`, dependency
/// modules after), so every declaration's spans and diagnostics still render against their own file.
/// A dependency module that fails to lex/parse is a **hard error** attributed to that file (see
/// [`BrokenModule`]); a broken *sibling* keeps the skip policy but is still available to attribute an
/// unresolved `use`.
pub fn link_with_deps(
    entry_name: &str,
    entry_text: &str,
    root_edition: noeta_lexer::Edition,
    siblings: &[RawModule],
    deps: &[DepPackage],
    package_uses: &noeta_span::PackageUses,
    entry_path: ModulePath,
) -> Result<Linked, Vec<LoadDiagnostic>> {
    // Assemble every module's `Source` up front — entry = 0, siblings `1..=S`, dependency modules
    // continuing the sequence — then lex them as one program (see [`lex_program`]: a text tier
    // declared in any file, a dependency package's included, captures verbatim bodies in every
    // file) before any parsing. The `editions` and `packages` side-tables are built in lock-step:
    // the entry and its siblings take the root package's edition and are the root package, each
    // dependency's modules take that package's own edition and its global key.
    let entry = Source::new(SourceId(0), entry_name, entry_text);
    let mut next_id: u32 = 1;
    let mut sources: Vec<Source> = vec![entry.clone()];
    let mut editions = noeta_lexer::EditionMap::new();
    let mut packages = noeta_span::PackageMap::new();
    editions.set(SourceId(0), root_edition);
    packages.set(SourceId(0), PackageOrigin::Root);
    for raw in siblings {
        sources.push(Source::new(
            SourceId(next_id),
            raw.name.as_str(),
            raw.text.as_str(),
        ));
        editions.set(SourceId(next_id), root_edition);
        packages.set(SourceId(next_id), PackageOrigin::Root);
        next_id += 1;
    }
    let sibling_end = sources.len();
    for dep in deps {
        for raw in &dep.modules {
            sources.push(Source::new(
                SourceId(next_id),
                raw.name.as_str(),
                raw.text.as_str(),
            ));
            editions.set(SourceId(next_id), dep.edition);
            packages.set(
                SourceId(next_id),
                PackageOrigin::Dependency(dep.key.clone()),
            );
            next_id += 1;
        }
    }
    let (lexeds, text_tiers) = lex_program(&sources, &editions, &packages, package_uses);

    // The entry parses under the root package's edition.
    let mut entry_parsed =
        noeta_parser::parse_in(&entry, &lexeds[0].tokens, root_edition, &text_tiers);
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

    // Parse the siblings (pure decl-sources) under the root edition. A broken sibling is kept for
    // attribution rather than dropped (see [`BrokenModule`]).
    let mut sibling_programs: Vec<(usize, Program)> = Vec::new();
    let mut broken: Vec<BrokenModule> = Vec::new();
    for (index, (source, lexed)) in sources[1..sibling_end]
        .iter()
        .zip(&lexeds[1..sibling_end])
        .enumerate()
    {
        match parse_module(
            source,
            lexed,
            root_edition,
            &text_tiers,
            &siblings[index].path,
        ) {
            Ok(program) => sibling_programs.push((index + 1, program)),
            Err(module) => broken.push(*module),
        }
    }

    // Parse + re-root each dependency package's modules under *that package's* edition (the sources
    // continue past the siblings in the same package order they were assembled above).
    let (mut dep_programs, broken_deps) =
        parse_dep_programs(&sources, &lexeds, sibling_end, deps, &text_tiers);

    // A dependency package that does not parse is a hard error, reported against the offending file
    // and span, and reported *before* linking so the cascade it would otherwise produce at the
    // consumer's `use` (`no module \`para.thing.broken\``) never fires. A package is a closed unit:
    // unlike a sibling it is never anyone's entry, so this is the only pass that will ever look at
    // it, and "skip it quietly" means the fault is never reported anywhere at all.
    if !broken_deps.is_empty() {
        return Err(broken_deps
            .iter()
            .flat_map(BrokenModule::load_diagnostics)
            .collect());
    }

    // The entry, its siblings, and every dependency module take the path their location derives —
    // dependency modules **after** re-rooting, so a declared namespace is compared in the consumer's
    // own naming space (see [`apply_derived_paths`]). One pass over all of them, because a collision
    // is a program-wide fact: two files of one package deriving one path is the same error as a
    // dependency module colliding with a sibling.
    let mut units = vec![DerivedUnit {
        source: &entry,
        path: &entry_path,
        program: &mut entry_parsed.program,
    }];
    units.extend(
        sibling_programs
            .iter_mut()
            .map(|(index, program)| DerivedUnit {
                source: &sources[*index],
                path: &siblings[*index - 1].path,
                program,
            }),
    );
    units.extend(dep_programs.iter_mut().map(|(index, program)| DerivedUnit {
        source: &sources[*index],
        path: dep_module_path(deps, *index - sibling_end),
        program,
    }));
    let path_diagnostics = apply_derived_paths(units);
    if !path_diagnostics.is_empty() {
        return Err(path_diagnostics);
    }

    let sibling_refs: Vec<&Program> = sibling_programs.iter().map(|(_, p)| p).collect();
    let dep_refs: Vec<&Program> = dep_programs.iter().map(|(_, p)| p).collect();
    let broken_refs: Vec<&BrokenModule> = broken.iter().collect();
    let native_roots = native_dep_roots(deps);
    let Linkage {
        mut program,
        source_maps,
    } = link_parsed_with_deps(
        &entry,
        &entry_parsed.program,
        &sibling_refs,
        &dep_refs,
        &broken_refs,
        Some(&native_roots),
    )
    .map_err(|mut d| {
        attribute_to_spans(&mut d, &sources);
        d
    })?;
    let reads = expand_into(
        &mut program,
        &source_maps,
        &mut sources,
        &mut editions,
        root_edition,
        &text_tiers,
    )?;
    Ok(Linked {
        program,
        entry,
        sources: SourceMap::new(sources),
        editions,
        packages,
        // Carry the resolved per-package `@name` tables through, so a caller reading `linked.package_uses`
        // (the tier-name CLI fallback, the checker's activation) resolves `@name`s per the package that
        // wrote them rather than reaching an empty table — the same map that drove the text-tier lex above.
        package_uses: package_uses.clone(),
        reads,
    })
}

/// Parse + re-root each dependency package's modules under *that package's* edition. The dependency
/// modules occupy `sources[offset..]` in package order (the order the caller assembled them); a
/// module that fails to lex/parse is returned as a [`BrokenModule`] rather than silently skipped
/// (its `Source` stays either way, so ids keep lining up), for the caller to report against its own
/// file. Shared by [`link_with_deps`] and the directory batch ([`parse_dir`]).
fn parse_dep_programs(
    sources: &[Source],
    lexeds: &[noeta_lexer::Lexed],
    offset: usize,
    deps: &[DepPackage],
    text_tiers: &noeta_lexer::TextTiers,
) -> (Vec<(usize, Program)>, Vec<BrokenModule>) {
    let mut dep_programs: Vec<(usize, Program)> = Vec::new();
    let mut broken: Vec<BrokenModule> = Vec::new();
    let mut dep_idx = offset;
    for dep in deps {
        for raw in &dep.modules {
            match parse_module(
                &sources[dep_idx],
                &lexeds[dep_idx],
                dep.edition,
                text_tiers,
                &raw.path,
            ) {
                Ok(mut program) => {
                    reroot_program(&mut program, &dep.root, &dep.key, &dep.dep_renames);
                    dep_programs.push((dep_idx, program));
                }
                Err(module) => broken.push(*module),
            }
            dep_idx += 1;
        }
    }
    (dep_programs, broken)
}

/// The `flat`-th dependency module's derived path, across every package's modules in the order the
/// sources were assembled (package order, each package's modules in scan order) — the same walk
/// [`parse_dep_programs`] does, so the two agree by construction.
fn dep_module_path(deps: &[DepPackage], flat: usize) -> &ModulePath {
    let mut remaining = flat;
    for dep in deps {
        if remaining < dep.modules.len() {
            return &dep.modules[remaining].path;
        }
        remaining -= dep.modules.len();
    }
    &ModulePath::Declared
}

/// A resolved dependency graph is complete knowledge: the always-legitimate non-std roots are the
/// declared native-package keys (their members live in the composed toolchain, not the link pool).
fn native_dep_roots(deps: &[DepPackage]) -> Vec<String> {
    deps.iter()
        .filter(|d| d.native)
        .map(|d| d.key.clone())
        .collect()
}

/// One directory module's parse outcome inside a [`ParsedDir`] — the [`parse_module`] result, whose
/// two roles fall straight out of it. As an **entry**, `Err`'s diagnostics are reported (exactly as
/// [`link_with_deps`] reports the entry's); as a **sibling**, `Ok` joins the pool and `Err` becomes
/// the [`BrokenModule`] that attributes a consumer's unresolved `use`. Both roles come from this one
/// parse.
type ModuleParse = Result<Program, Box<BrokenModule>>;

/// A directory's modules (plus the entry's dependency packages) lexed and parsed **once**, and
/// linkable against *any* member as the entry — `noeta check`'s directory mode (audit-4 F4).
/// Checking a directory treats every `.noe` file as its own entry; loading each entry through
/// [`load_with_deps`] re-lexed and re-parsed the whole directory per entry (N entries → N× the
/// work). Here ids are assigned once, in directory (sorted-path) order with dependency modules
/// after, and every entry's link shares the same parsed pool — so one [`SourceMap`] renders
/// every entry's diagnostics. Semantics per entry are identical to [`load_with_deps`]: same
/// sibling set (every *other* cleanly-parsed module), same dependency pool, same text-tier
/// union and per-package editions; only the `SourceId` numbering differs (entry-first there,
/// directory-order here), which diagnostics never observe (they render by source name).
#[derive(Debug)]
pub struct ParsedDir {
    sources: Vec<Source>,
    editions: noeta_lexer::EditionMap,
    /// Which package each source was read from, keyed by the shared `SourceId` numbering — the
    /// directory-mode twin of `Linked::packages`. Directory modules are the root package;
    /// dependency modules carry their package's global key.
    packages: noeta_span::PackageMap,
    modules: Vec<ModuleParse>,
    dep_programs: Vec<(usize, Program)>,
    /// Dependency-package modules that failed to lex/parse — a hard error for *every* entry in the
    /// directory (a dependency is shared by all of them), reported against the dependency's own
    /// file. `noeta check` dedups by (file, span, code), so one broken dependency file prints once
    /// however many entries the directory holds.
    broken_deps: Vec<BrokenModule>,
    /// What the *derivation* found wrong with the directory's files — a name that cannot be a path
    /// segment, two files deriving one path, a `namespace` contradicting the file's location. A
    /// property of the file set, so it fails every entry in it ([`ParsedDir::link_entry`]), like a
    /// broken dependency module.
    path_diagnostics: Vec<LoadDiagnostic>,
    native_roots: Vec<String>,
    /// The union of text tiers declared anywhere in the directory (or its dependencies), from the
    /// one program-wide lex — kept because an expansion's generated source is lexed and parsed with
    /// the same tier set as the files around it, or a `@tier { … }` body in generated code would
    /// tokenize differently than the identical body written by hand.
    text_tiers: noeta_lexer::TextTiers,
    /// The root package's edition, which every directory module was parsed under and which generated
    /// code takes too (it is written by the extension the *root* installed).
    root_edition: noeta_lexer::Edition,
}

/// What linking one directory entry produced — [`ParsedDir::link_entry`]'s result.
///
/// More than a `Program` because expansion appends sources, and a [`ParsedDir`] is shared, immutable
/// and read by every entry: an entry that expanded cannot push into the directory's source list, so
/// it hands its own sources back for the caller to render against
/// ([`ParsedDir::source_map_with`] / [`ParsedDir::editions_with`]).
#[derive(Debug)]
pub struct EntryLink {
    /// The linked program, with any generated members already spliced in.
    pub program: Program,
    /// Sources produced by compile-time expansion, ids continuing past every directory and
    /// dependency source, each paired with the directive that produced it. **Empty** for the
    /// overwhelmingly common case of a program with no expanding directive — the caller then reuses
    /// the shared map untouched.
    pub expansions: Vec<ExpandedSource>,
    /// Every file an expansion hook reported reading, in expansion order (a rebuild trigger).
    pub reads: Vec<String>,
}

/// Lex + parse a directory's `modules` (from [`read_dir_modules`]) and its dependency packages
/// once, for per-entry linking via [`ParsedDir::link_entry`]. Mirrors [`link_with_deps`]'s
/// assembly: one program-wide lex (text-tier union spans every file, dependencies included),
/// entry/sibling sources under the root package's edition, each dependency's under its own.
/// `package_uses` (the whole program's resolved `@name` tables) lets that lex honor a package's
/// renamed text tiers, exactly as [`link_with_deps`] does.
pub fn parse_dir(
    modules: Vec<RawModule>,
    root_edition: noeta_lexer::Edition,
    deps: &[DepPackage],
    package_uses: &noeta_span::PackageUses,
) -> ParsedDir {
    let mut sources: Vec<Source> = Vec::with_capacity(modules.len());
    let mut editions = noeta_lexer::EditionMap::new();
    let mut packages = noeta_span::PackageMap::new();
    let mut next_id: u32 = 0;
    for raw in &modules {
        sources.push(Source::new(
            SourceId(next_id),
            raw.name.as_str(),
            raw.text.as_str(),
        ));
        editions.set(SourceId(next_id), root_edition);
        packages.set(SourceId(next_id), PackageOrigin::Root);
        next_id += 1;
    }
    for dep in deps {
        for raw in &dep.modules {
            sources.push(Source::new(
                SourceId(next_id),
                raw.name.as_str(),
                raw.text.as_str(),
            ));
            editions.set(SourceId(next_id), dep.edition);
            packages.set(
                SourceId(next_id),
                PackageOrigin::Dependency(dep.key.clone()),
            );
            next_id += 1;
        }
    }
    let (lexeds, text_tiers) = lex_program(&sources, &editions, &packages, package_uses);

    // Parse every directory module once — [`parse_module`] parses even after lex errors (as the
    // entry role always did) and chains lex + parse diagnostics onto the partial program, which is
    // exactly what both the entry and the sibling role need here.
    let mut parsed_modules: Vec<ModuleParse> = sources[..modules.len()]
        .iter()
        .zip(&lexeds)
        .zip(&modules)
        .map(|((source, lexed), raw)| {
            parse_module(source, lexed, root_edition, &text_tiers, &raw.path)
        })
        .collect();

    let (mut dep_programs, broken_deps) =
        parse_dep_programs(&sources, &lexeds, modules.len(), deps, &text_tiers);

    // Derivation applies once for the whole directory, not per entry: a collision or a contradicted
    // declaration is a fact about the *files*, identical whichever of them is being checked. The
    // diagnostics ride on the `ParsedDir` and every entry's link reports them (the CLI dedups by
    // file+span+code, exactly as it does for a broken dependency).
    let mut units: Vec<DerivedUnit> = parsed_modules
        .iter_mut()
        .enumerate()
        .filter_map(|(index, parse)| {
            parse.as_mut().ok().map(|program| DerivedUnit {
                source: &sources[index],
                path: &modules[index].path,
                program,
            })
        })
        .collect();
    units.extend(dep_programs.iter_mut().map(|(index, program)| DerivedUnit {
        source: &sources[*index],
        path: dep_module_path(deps, *index - modules.len()),
        program,
    }));
    let path_diagnostics = apply_derived_paths(units);

    let native_roots = native_dep_roots(deps);
    ParsedDir {
        sources,
        editions,
        packages,
        modules: parsed_modules,
        dep_programs,
        broken_deps,
        path_diagnostics,
        native_roots,
        text_tiers,
        root_edition,
    }
}

impl ParsedDir {
    /// The index of the directory module whose display name is `name`, for [`Self::link_entry`].
    /// `None` when the directory scan didn't yield it (an unreadable file, or a path spelled
    /// differently than the scan spells it — the caller falls back to a lone-file load).
    pub fn module_index(&self, name: &str) -> Option<usize> {
        (0..self.modules.len())
            .find(|&i| self.sources[i].name() == name)
            // A package's modules are spelled from the package root while the invocation spells the
            // entry however it likes; falling back to "is it the same file" keeps an entry linking
            // against its own package instead of degrading to a lone-file check.
            .or_else(|| {
                (0..self.modules.len())
                    .find(|&i| same_file(Path::new(self.sources[i].name()), Path::new(name)))
            })
    }

    /// Every source (directory modules + dependency modules) under the shared id numbering —
    /// one map renders every entry's diagnostics.
    pub fn source_map(&self) -> SourceMap {
        SourceMap::new(self.sources.clone())
    }

    /// Which edition governs each source, keyed by the shared `SourceId` numbering.
    pub fn editions(&self) -> &noeta_lexer::EditionMap {
        &self.editions
    }

    /// Which package each source came from, keyed by the shared `SourceId` numbering. An entry's
    /// expansions are deliberately absent (as in `Linked::packages`), so the orphan rule never
    /// judges generated code.
    pub fn packages(&self) -> &noeta_span::PackageMap {
        &self.packages
    }

    /// The shared sources **plus** one entry's expansions ([`EntryLink::expansions`]), so a
    /// generated member's span resolves to the source it was generated into.
    ///
    /// Only an entry that expanded needs this; with an empty `expansions` it is exactly
    /// [`Self::source_map`].
    pub fn source_map_with(&self, expansions: &[ExpandedSource]) -> SourceMap {
        let mut sources = Vec::with_capacity(self.sources.len() + expansions.len());
        sources.extend(self.sources.iter().cloned());
        sources.extend(expansions.iter().map(|e| e.source.clone()));
        SourceMap::new(sources)
    }

    /// [`Self::editions`] extended with one entry's expansions, each at the root package's edition —
    /// generated code was written by the extension the *root* installed, for this program. With an
    /// empty `expansions` it is a clone of [`Self::editions`].
    pub fn editions_with(&self, expansions: &[ExpandedSource]) -> noeta_lexer::EditionMap {
        let mut editions = self.editions.clone();
        for expansion in expansions {
            editions.set(expansion.source.id(), self.root_edition);
        }
        editions
    }

    /// Link the directory module at `index` as the entry, against every *other* cleanly-parsed
    /// module (the sibling pool) and the dependency programs — [`load_with_deps`]'s per-entry
    /// semantics over the shared parse. An entry with lex/parse diagnostics returns them, as the
    /// per-entry load did; their spans resolve against [`Self::source_map`].
    ///
    /// Compile-time directive expansion runs here too, through the same [`run_expansion`] the
    /// whole-program link paths use — without it `noeta check` would disagree with `noeta run` about
    /// a program using an expanding directive (a generated method would be "unknown method" under
    /// one and fine under the other). A failed expansion comes back as `Err`, exactly as in
    /// [`link`]/[`link_with_deps`]; the sources it produced come back in
    /// [`EntryLink::expansions`], for [`Self::source_map_with`] to render against.
    pub fn link_entry(&self, index: usize) -> Result<EntryLink, Vec<LoadDiagnostic>> {
        let entry_source = &self.sources[index];
        let entry = match &self.modules[index] {
            Ok(program) => program,
            Err(broken) => return Err(broken.load_diagnostics()),
        };
        // A broken dependency module fails every entry in the directory (see [`Self::broken_deps`]),
        // before linking, so it is not overwritten by the "no module" cascade it would cause.
        if !self.broken_deps.is_empty() {
            return Err(self
                .broken_deps
                .iter()
                .flat_map(BrokenModule::load_diagnostics)
                .collect());
        }
        // Same standing as a broken dependency: the file set is not linkable, and saying so beats
        // the "no module"/"has no export" cascade the bad derivation would produce downstream.
        if !self.path_diagnostics.is_empty() {
            return Err(self
                .path_diagnostics
                .iter()
                .map(|d| LoadDiagnostic {
                    source: d.source.clone(),
                    diagnostic: d.diagnostic.clone(),
                })
                .collect());
        }
        let siblings: Vec<&Program> = self
            .modules
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != index)
            .filter_map(|(_, m)| m.as_ref().ok())
            .collect();
        // A broken *sibling* is not itself an error here — it is reported when it is the entry of
        // its own pass — but it is still what an unresolved `use` of its namespace should point at.
        let broken: Vec<&BrokenModule> = self
            .modules
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != index)
            .filter_map(|(_, m)| m.as_ref().err().map(Box::as_ref))
            .collect();
        let dep_refs: Vec<&Program> = self.dep_programs.iter().map(|(_, p)| p).collect();
        let Linkage {
            mut program,
            source_maps,
        } = link_parsed_with_deps(
            entry_source,
            entry,
            &siblings,
            &dep_refs,
            &broken,
            Some(&self.native_roots),
        )
        .map_err(|mut d| {
            attribute_to_spans(&mut d, &self.sources);
            d
        })?;
        let (expansions, reads, diagnostics) = run_expansion(
            &mut program,
            &source_maps,
            || self.sources.clone(),
            self.sources.len() as u32,
            self.root_edition,
            &self.text_tiers,
        );
        // Directory mode (`noeta check`) is single-shot — it does not watch — so a failed
        // expansion's reads have no rebuild trigger to feed and are dropped with the error.
        if !diagnostics.is_empty() {
            return Err(diagnostics);
        }
        Ok(EntryLink {
            program,
            expansions,
            reads,
        })
    }
}

/// Lex every module of a program as one unit (text-tiers arc): each file lexes with the default
/// text-tier set first; if any file declares a text tier (`@tier(x, …, text: "…")`), the union of
/// all declarations is applied and every file re-lexes with it — so a tier declared in one file
/// (or one dependency package) captures `@x { … }` bodies verbatim in every other. Only programs
/// declaring text tiers pay the second pass.
///
/// **Per-package renamed text tiers (per-package naming arc, sub-step 3g).** A `[tiers]` binding may
/// map a *local* `@name` onto a text provider tier (`docs = "std:doc"`, `@docs { # markdown }`). The
/// lexer only knows the provider's *exported* name (`doc`) as verbatim-capturing, so the local name
/// must be added — but **only for the package that bound it**. Two packages can bind different locals,
/// or the same local to different meanings, so the augmentation is keyed by [`PackageOrigin`] (from
/// `packages`) and each source re-lexes with *its own* package's set. `package_uses` carries every
/// package's `@name` bindings (the root's under [`PackageOrigin::Root`], each dependency's under its
/// link segment); an empty map (a bare script, a single-file check) is exactly today's behavior.
fn lex_program(
    sources: &[Source],
    editions: &noeta_lexer::EditionMap,
    packages: &noeta_span::PackageMap,
    package_uses: &noeta_span::PackageUses,
) -> (Vec<noeta_lexer::Lexed>, noeta_lexer::TextTiers) {
    // Each source lexes under ITS OWN package's edition (editions arc): the map was built in
    // lock-step with the sources, so a future edition that changes tokenization (a promoted
    // keyword, a new literal syntax) applies per package — the multi-package leg the arc's
    // "already at the point that would consult it" claim depends on.
    let edition_of = |source: &Source| editions.source_edition(source.id());
    let lexeds: Vec<_> = sources
        .iter()
        .map(|source| {
            noeta_lexer::lex_in(
                source,
                edition_of(source),
                &noeta_lexer::TextTiers::default(),
            )
        })
        .collect();
    // Program-wide verbatim-body tiers come from two sources: a program's own `@tier(…, text/expr)`
    // (found by the lexer's per-file token scan) and the installed extensions' declarations (`doc`,
    // and any native `@json`/`@sql` — no `.noe` file declares these). These apply to EVERY file (a
    // tier declared in one file names the same tier everywhere), unchanged by the per-package layer.
    let mut global_names: Vec<String> = lexeds
        .iter()
        .flat_map(|l| l.text_tier_decls.iter().cloned())
        .collect();
    global_names.extend(
        noeta_ext_abi::registry::single_registry_process()
            .ext_verbatim_tier_names()
            .into_iter()
            .map(str::to_string),
    );
    let global = noeta_lexer::TextTiers::with(global_names.clone());
    // The per-package layer: each origin's local `@name`s that resolve to a text tier (see
    // [`renamed_text_tier_locals`]). Keyed by origin so a rename in package A never forces verbatim
    // capture of the same spelling in package B.
    let renamed = renamed_text_tier_locals(package_uses, packages, &lexeds);

    // The set the *parser* and *expansion* see is the union of every contribution — the parser
    // consults it only to re-lex `${…}` interpolation holes (a nested `@name { … }` inside a hole),
    // never to decide a top-level block's text-vs-code (that is already baked into the lexer's tokens
    // by the per-package pass below); generated code has no single owning package, so the union is
    // the only meaningful set there too. The union is broader than any one package's set, which only
    // matters for the exotic nested-tier-in-a-hole case — safe, since it merely captures more prose.
    let mut union = global.clone();
    for locals in renamed.values() {
        for name in locals {
            union.insert(name.clone());
        }
    }

    // Fast path: the default `{doc}` covers every program-wide name and no package renamed a text
    // tier — the first pass already lexed correctly, so nothing re-lexes (the common case: no text
    // tier beyond `doc`, no `[tiers]` text binding).
    let default = noeta_lexer::TextTiers::default();
    if global_names.iter().all(|name| default.contains(name)) && renamed.is_empty() {
        return (lexeds, global);
    }

    // Re-lex each source with ITS OWN package's set: the program-wide `global` plus the local names
    // that package bound to a text tier. A source whose package renamed nothing re-lexes with exactly
    // `global` (unchanged from the old behavior); only a package with a text-tier binding widens.
    let relexed = sources
        .iter()
        .map(|source| {
            let mut set = global.clone();
            if let Some(origin) = packages.source_package(source.id())
                && let Some(locals) = renamed.get(origin)
            {
                for name in locals {
                    set.insert(name.clone());
                }
            }
            noeta_lexer::lex_in(source, edition_of(source), &set)
        })
        .collect();
    (relexed, union)
}

/// The local `@name`s each package binds to a **text** (verbatim-body) tier — the per-package input
/// to [`lex_program`]'s re-lex, keyed by the binding package's [`PackageOrigin`]. A `[tiers]` binding
/// `local = "provider[:exported]"` makes `local` a text tier for that package iff the provider tier it
/// names is itself a text (or expression) tier — resolved two ways, mirroring the checker's
/// `TierRegistry::resolve_at`:
///
/// * an **extension** provider — `find_ext_tier_scoped(provider_roots, exported)` lands on the tier
///   the binding named (scoped so `std`'s `doc` and a third party's same-named tier never conflate),
///   and it is a text tier when its `.text`/`.expr` is set (the same predicate `ext_verbatim_tier_names`
///   uses); or
/// * a **program-declared** provider — a dependency shipping `@tier(exported, …, text: "…")`. At lex
///   time there is no parsed AST, but the first lex pass already scanned each source's own text-tier
///   declarations ([`noeta_lexer::Lexed::text_tier_decls`]); indexed by the declaring source's package
///   segment, that answers "does the provider this local names declare `exported` as text?" — matched
///   against `provider_roots` (a `.noe` `@tier` runner is re-rooted to the consumer's link segment,
///   which is exactly the dependency's [`PackageOrigin::Dependency`] key, so the two line up).
fn renamed_text_tier_locals(
    package_uses: &noeta_span::PackageUses,
    packages: &noeta_span::PackageMap,
    lexeds: &[noeta_lexer::Lexed],
) -> std::collections::HashMap<PackageOrigin, Vec<String>> {
    if package_uses.is_empty() {
        return std::collections::HashMap::new();
    }
    // Program-declared text tiers, indexed by the declaring package's link segment. A dependency's
    // modules carry `PackageOrigin::Dependency(key)`, and that `key` is the very segment a `@tier`
    // runner is re-rooted to — so `provider_roots` (which carries that segment) matches here.
    let mut declared_by_segment: std::collections::HashMap<&str, std::collections::HashSet<&str>> =
        std::collections::HashMap::new();
    for (index, lexed) in lexeds.iter().enumerate() {
        if lexed.text_tier_decls.is_empty() {
            continue;
        }
        if let Some(PackageOrigin::Dependency(key)) =
            packages.source_package(SourceId(index as u32))
        {
            let entry = declared_by_segment.entry(key.as_str()).or_default();
            for name in &lexed.text_tier_decls {
                entry.insert(name.as_str());
            }
        }
    }

    let registry = noeta_ext_abi::registry::single_registry_process();
    let mut out: std::collections::HashMap<PackageOrigin, Vec<String>> =
        std::collections::HashMap::new();
    for (origin, local, use_) in package_uses.iter() {
        // An extension provider whose named tier captures verbatim (a text or expression tier).
        let ext_text = registry
            .find_ext_tier_scoped(&use_.provider_roots, &use_.exported)
            .is_some_and(|tier| tier.text.is_some() || tier.expr.is_some());
        // Or a dependency-declared `@tier(exported, …, text: "…")` under one of the provider roots.
        let declared_text = use_.provider_roots.iter().any(|root| {
            declared_by_segment
                .get(root.as_str())
                .is_some_and(|names| names.contains(use_.exported.as_str()))
        });
        if ext_text || declared_text {
            out.entry(origin.clone()).or_default().push(local.clone());
        }
    }
    out
}

/// Parse an already-lexed source, yielding its [`Program`] when both lex and parse are clean and a
/// [`BrokenModule`] — its diagnostics, plus the namespace it declares — when they are not. The
/// shared helper behind [`link`]'s sibling loop, [`link_with_deps`], and [`parse_dir`].
///
/// A broken module still cannot be resolved against, so it stays out of the pool; what changed is
/// that its errors are no longer *discarded*. The source is parsed even after a lex error (as the
/// entry role always was), so one parse serves both roles.
fn parse_module(
    source: &Source,
    lexed: &noeta_lexer::Lexed,
    edition: noeta_lexer::Edition,
    text_tiers: &noeta_lexer::TextTiers,
    path: &ModulePath,
) -> Result<Program, Box<BrokenModule>> {
    // Parse under the owning package's edition — the entry/sibling's root edition or a
    // dependency's own — so a future edition-gated grammar applies per package.
    let parsed = noeta_parser::parse_in(source, &lexed.tokens, edition, text_tiers);
    let diagnostics: Vec<Diagnostic> = lexed
        .diagnostics
        .iter()
        .chain(parsed.diagnostics.iter())
        .cloned()
        .map(|d| retarget(d, source.id()))
        .collect();
    if diagnostics.is_empty() {
        Ok(parsed.program)
    } else {
        Err(Box::new(BrokenModule {
            source: source.clone(),
            // A *derived* path is known whether or not the file parses — it comes from the file's
            // location, not its contents — so a broken module attributes a consumer's unresolved
            // `use` even when the syntax error precedes (or replaces) any `namespace` line.
            namespace: match path {
                ModulePath::Derived(derived) => Some(derived.clone()),
                _ => namespace_from_tokens(source, &lexed.tokens),
            },
            diagnostics,
        }))
    }
}

/// Stamp `id` onto a diagnostic's spans. Lex/parse diagnostics are single-file by construction, but
/// a few lexer/parser spans are built with the default entry id (`Span::new` → `SourceId::FIRST`) —
/// harmless where the file being lexed *is* id 0, a misattribution anywhere else (a sibling, or a
/// dependency module several ids along). Rendering a module's own error against the shared
/// [`SourceMap`] depends on this.
fn retarget(mut diagnostic: Diagnostic, id: SourceId) -> Diagnostic {
    diagnostic.span.source = id;
    for label in &mut diagnostic.labels {
        label.span.source = id;
    }
    diagnostic
}

/// One parsed file and the module path its **location** derives — the input to
/// [`apply_derived_paths`]. Public so the salsa linker (`noeta-db`) applies derivation identically:
/// the editor and the compiler must not disagree about which module a file is.
#[derive(Debug)]
pub struct DerivedUnit<'a> {
    pub source: &'a Source,
    pub path: &'a ModulePath,
    pub program: &'a mut Program,
}

/// Make each file's **derived** path its module path, and report every way the filesystem says
/// something the program cannot mean.
///
/// This is where derivation becomes the linker's truth: a derived path is written into the program
/// as its `namespace`, so everything downstream (`module_namespace`, resolution, qualification)
/// reads one identity with one origin. Three things are errors here rather than silence:
///
/// * a **`namespace` declaration that disagrees** with the derived path (E0072) — the declaration is
///   still accepted while it is being removed from the ecosystem, but only as a *restatement*;
/// * **two files deriving one path** (E0073), naming both files. This used to be silent: the second
///   file's exports vanished and the failure surfaced at whoever imported them;
/// * a **name that is not a legal path segment** (E0074), with the rename to make.
///
/// A file with no derived path ([`ModulePath::Declared`] — a lone script, an in-memory caller) is
/// left exactly as it was, so nothing that never had a package changes behavior.
///
/// Called **after** dependency re-rooting ([`reroot_program`]), because that is what puts a declared
/// namespace into the consumer's own naming space, which is the space the derivation is in.
pub fn apply_derived_paths(units: Vec<DerivedUnit<'_>>) -> Vec<LoadDiagnostic> {
    let mut diagnostics = Vec::new();
    // Derived path → the file that claimed it first (both are named when a second one does).
    let mut claimed: std::collections::HashMap<Vec<String>, String> =
        std::collections::HashMap::new();
    for unit in units {
        let derived = match unit.path {
            ModulePath::Declared => continue,
            ModulePath::Illegal { segment, rename_to } => {
                diagnostics.push(LoadDiagnostic {
                    source: unit.source.clone(),
                    diagnostic: Diagnostic::error(
                        DiagnosticCode::IllegalModulePath,
                        first_line(unit.source),
                        format!(
                            "`{segment}` cannot be part of a module path — a module's path is \
                             derived from where its file sits, and every segment of it has to be \
                             spellable in a `use`"
                        ),
                    )
                    .with_help(format!("rename it to `{rename_to}`")),
                });
                continue;
            }
            ModulePath::Derived(derived) => derived,
        };
        if let Some(first) = claimed.get(derived) {
            diagnostics.push(LoadDiagnostic {
                source: unit.source.clone(),
                diagnostic: Diagnostic::error(
                    DiagnosticCode::ModulePathCollision,
                    first_line(unit.source),
                    format!(
                        "two files derive the module path `{}`: `{first}` and `{}`",
                        derived.join("."),
                        unit.source.name()
                    ),
                )
                .with_help(
                    "one module path is one module — rename or move one of the files so their \
                     paths differ",
                ),
            });
            continue;
        }
        claimed.insert(derived.clone(), unit.source.name().to_string());

        match unit.program.stmts.iter_mut().find_map(|stmt| match stmt {
            Stmt::Namespace { path, span } => Some((path, span)),
            _ => None,
        }) {
            Some((declared, span)) if declared != derived => {
                let message = format!(
                    "this module declares `namespace {}`, but its path derives as `{}`",
                    declared.join("."),
                    derived.join(".")
                );
                diagnostics.push(LoadDiagnostic {
                    source: unit.source.clone(),
                    diagnostic: Diagnostic::error(
                        DiagnosticCode::ModulePathMismatch,
                        *span,
                        message,
                    )
                    .with_help(
                        "a module's path is the package's import prefix plus the file's path \
                         inside the package — delete the declaration, or move the file to where \
                         it says it lives",
                    ),
                });
            }
            // A declaration that agrees is a restatement — leave it be (it is removed corpus-wide
            // in a later slice, and the syntax with it).
            Some(_) => {}
            None => unit.program.stmts.insert(
                0,
                Stmt::Namespace {
                    path: derived.clone(),
                    span: first_line(unit.source),
                },
            ),
        }
    }
    diagnostics
}

/// A span covering `source`'s first line — what a diagnostic about the *file* (not about anything
/// written in it) points at.
fn first_line(source: &Source) -> Span {
    let end = source
        .text()
        .find('\n')
        .unwrap_or_else(|| source.text().len());
    Span::new_in(source.id(), 0, end as u32)
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
    broken: &[&BrokenModule],
) -> Result<Linkage, Vec<LoadDiagnostic>> {
    // Sibling-only linking has no resolved dependency graph, so it is lenient: it can flag a missing
    // intra-project module but must not adjudicate foreign roots (see [`RetainPolicy`]).
    link_core(
        entry,
        entry_program,
        modules,
        modules,
        broken,
        RetainPolicy::Lenient,
    )
}

/// The cross-package variant (package-manager P2.1): like [`link_parsed`], but `dep_modules` are the
/// re-rooted source modules of the entry's dependency packages. They are **both** resolution
/// candidates *and* import drivers — a package is a closed unit, so its own `use`s (already re-rooted
/// to the consumer's key) resolve its internal cross-references, the same way a sibling's do.
/// `dep_modules` must already be re-rooted (see [`reroot_program`]); the caller
/// ([`link_with_deps`]) does that. Every std import inside a dependency (`use std.…`) resolves against
/// no module here and is retained (deduped) so the compiler binds it downstream, exactly as an
/// entry's std imports are.
/// `native_roots` gates dependency-import strictness (module-namespaces): `Some(roots)` means the
/// caller resolved the **complete** dependency graph (the CLI), so every legitimate import root is
/// known — std extensions plus these declared native-package roots — and any other unresolved import
/// is an error. `None` means the caller (the IDE `linked` query) lacks that graph and stays lenient.
/// `broken` are the modules that failed to lex/parse and so are missing from `pool` — consulted only
/// when a `use` resolves to nothing, to report the real parse error in place of a misleading
/// "no module" ([`BrokenModule`]).
pub fn link_parsed_with_deps(
    entry: &Source,
    entry_program: &Program,
    siblings: &[&Program],
    dep_modules: &[&Program],
    broken: &[&BrokenModule],
    native_roots: Option<&[String]>,
) -> Result<Linkage, Vec<LoadDiagnostic>> {
    // Sibling and dependency modules alike join the resolution pool *and* drive imports: a `use` is
    // file-scoped, so a module that writes `use std.{env}` must get `env` bound in the merged
    // program whether it is a sibling of the entry or a file inside a package.
    let pool: Vec<&Program> = siblings.iter().chain(dep_modules).copied().collect();
    let native: HashSet<String> = native_roots.unwrap_or_default().iter().cloned().collect();
    let retain = match native_roots {
        Some(_) => RetainPolicy::Complete {
            native_roots: &native,
        },
        None => RetainPolicy::Lenient,
    };
    link_core(entry, entry_program, &pool, &pool, broken, retain)
}

/// Where a top-level name in **one compilation unit** came from — a declaration that unit makes, or
/// the namespace one of its imports pulled it from. Lets a file name the same declaration twice
/// (`use webclient.client.Client` written twice, or once per grouped list) without complaint, while
/// a genuine clash *within that file* — same local name, different namespace, or an import over a
/// declaration — is the E0020 collision.
///
/// This decides collisions only. Whether a resolved declaration is *merged* is a program-wide
/// question with a program-wide answer (`merged_q`, keyed on the qualified identity): several files
/// legitimately import one declaration, and it must land in the linked program exactly once.
#[derive(Clone)]
enum Origin {
    Local,
    Import(Vec<String>),
}

/// One **compilation unit's** top-level name table, seeded with the declarations the unit makes
/// itself (each [`Origin::Local`]). A unit is one file — the entry, or a pooled module driving its
/// own `use`s — because that is the scope a `use` binds in: two files importing different
/// declarations under the same local name is not a clash, and never was one for the reader.
fn unit_origins(stmts: &[Stmt]) -> std::collections::HashMap<String, Origin> {
    stmts
        .iter()
        .filter_map(decl_name)
        .map(|n| (n.to_string(), Origin::Local))
        .collect()
}

/// The shared linking core: resolve the entry's imports (and any `drivers`' imports) against the
/// `pool`, merging each resolved declaration once and retaining every unresolved `use` (deduped) for
/// the compiler's downstream binding. `drivers` is empty for single-package linking, so that path
/// is unchanged bar one refinement: a duplicate *identical* import (same namespace + name) is now
/// skipped rather than flagged, which a closed dependency unit needs and no well-formed program
/// relied on erroring. `drivers` is normally the `pool` itself: a `use` is file-scoped, so every
/// loaded module's own imports must be honored, or a module that writes `use std.{env}` finds `env`
/// unbound in the merged program while the entry's imports leak in to cover for it.
fn link_core(
    entry: &Source,
    entry_program: &Program,
    pool: &[&Program],
    drivers: &[&Program],
    broken: &[&BrokenModule],
    retain: RetainPolicy,
) -> Result<Linkage, Vec<LoadDiagnostic>> {
    // For the complete policy: the always-retained roots are the installed extensions. The loader
    // is already global-registry-coupled (verbatim-tier names below), so the process default —
    // seeded by the assembling driver (audit-6 F2) — is the lens.
    let reg = noeta_ext_abi::registry::single_registry_process();
    // A module contributes only if it declares a namespace to resolve against.
    // The **entry** is a resolution candidate alongside the pool: it declares a namespace like any
    // other module, and a sibling may legitimately `use` it (two files of one project importing each
    // other). Leaving it out made such a `use` an "unknown module" error the moment sibling imports
    // started resolving. Its declarations are already in the program, so resolving to one merges
    // nothing — see the `Origin::Local` arm in `drive_use`.
    let module_views: Vec<ModuleView> = pool
        .iter()
        .copied()
        .chain(std::iter::once(entry_program))
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

    let entry_map = build_module_map(&entry_ns, &entry_program.stmts, &module_views, reg, false);
    let module_maps: std::collections::HashMap<Vec<String>, qualify::UnitMap> = module_views
        .iter()
        .map(|mv| {
            // The entry is a resolution candidate in `module_views` too, but it is not a *merged*
            // unit — its statements are the program's tail, in its own scope — so it keeps its
            // short handle names (see `build_module_map`).
            let is_entry = mv.namespace == entry_ns;
            (
                mv.namespace.clone(),
                build_module_map(&mv.namespace, mv.stmts, &module_views, reg, !is_entry),
            )
        })
        .collect();

    let mut imported: Vec<Stmt> = Vec::new();
    // Qualified identities already merged (explicit imports and their transitive same-module
    // dependencies alike) — the dedup key for the reachability closure, keyed on the dotted identity
    // no local name can collide with, so a declaration pulled two ways merges exactly once.
    //
    // Seeded with the **entry's own** declarations: they are already in the program (they are the
    // program's tail), so a dependency module that imports one of them must not merge a second copy.
    // The entry only resolves as a module at all when it declares a namespace, so a namespace-less
    // entry seeds nothing.
    let mut merged_q: HashSet<String> = if entry_ns.is_empty() {
        HashSet::new()
    } else {
        entry_program
            .stmts
            .iter()
            .filter_map(qualifiable_decl_name)
            .map(|n| format!("{}.{n}", entry_ns.join(".")))
            .collect()
    };
    let mut errors: Vec<LoadDiagnostic> = Vec::new();
    // Retained (unresolved) imports — std imports and opaque-stub fallbacks — deduped by (path, name)
    // across the entry and every dependency so a shared `use std.…` isn't bound twice.
    let mut seen_retained: HashSet<(Vec<String>, String, String)> = HashSet::new();
    let mut dep_retained: Vec<Stmt> = Vec::new();
    let mut entry_stmts: Vec<Stmt> = Vec::with_capacity(entry_program.stmts.len());

    // Every broken file names itself once however many `use`s cascade off it.
    let mut reported_broken: HashSet<String> = HashSet::new();
    // Broken files whose namespace could *not* be read (the syntax error precedes the `namespace`
    // declaration) — nameable as a hint on an E0019, since one of them may well be the module the
    // user is looking for.
    let unattributable: Vec<&str> = broken
        .iter()
        .filter(|m| m.namespace.is_none())
        .map(|m| m.source.name())
        .collect();

    // Resolve one `use`'s names against the pool. Resolved names merge their declaration (deduped on
    // its qualified identity); unresolved names are collected for retention. Returns the
    // still-unresolved names.
    //
    // `origins` is the **driving unit's own** name table (see the driver loop): a `use` is
    // file-scoped, so collision detection is per file. Merge dedup is *not* — a declaration two
    // files import merges once — which is why the two jobs are split across `origins` (per unit) and
    // `merged_q` (program-wide).
    let mut drive_use = |path: &[String],
                         names: &[UseName],
                         origins: &mut std::collections::HashMap<String, Origin>,
                         imported: &mut Vec<Stmt>,
                         errors: &mut Vec<LoadDiagnostic>|
     -> Vec<UseName> {
        let mut unresolved = Vec::new();
        for name in names {
            match resolve(&module_views, path, &name.name) {
                // Keyed on the import's *local* (alias-aware) name: `use App.A.User as AUser` and
                // `use App.B.User as BUser` bind distinct locals and coexist, while two imports (or an
                // import and a local decl) sharing one local name *in the same file* are the E0020
                // clash.
                Resolution::Resolved(decl) => match origins.get(name.local()) {
                    None => {
                        origins.insert(name.local().to_string(), Origin::Import(path.to_vec()));
                        // Merge the imported declaration under its module's qualified identity, then
                        // drag in its same-module transitive dependencies — every internal helper it
                        // calls and every module-local type it names (params/returns/fields/bodies,
                        // and a `@tier` config, now just one edge of the general closure). Without
                        // this, an exported declaration that references anything non-leaf in its own
                        // module leaves that reference out of the merged program (E0005/E0004).
                        //
                        // Guarded on the qualified identity, not on this unit's name table: several
                        // files legitimately import the same declaration (and the entry's own
                        // declarations are seeded in), and it must land in the program exactly once.
                        if merged_q.insert(format!("{}.{}", path.join("."), name.name)) {
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
                    }
                    // Same declaration re-imported (same namespace) — merge once, ignore the rest.
                    Some(Origin::Import(p)) if p.as_slice() == path => {}
                    // A different declaration under the same local name — ambiguous.
                    Some(_) => errors.push(collision_error(entry, path, name)),
                },
                // The dotted path names a whole loaded module (`use geometry.vec` where a module
                // declares `namespace geometry.vec;`): a **module import**. Merge every `pub`
                // declaration (qualified, plus its same-module closure); the importing module's
                // QMap ([`build_module_map`]) aliases each one under the local binding
                // (`vec.Vec2` → `geometry.vec.Vec2`), so qualified references resolve. Tried only
                // after item resolution — a module `geometry` exporting an item `vec` keeps today's
                // item-import meaning.
                Resolution::NoModule | Resolution::Missing
                    if module_with_namespace(&module_views, path, &name.name).is_some() =>
                {
                    let module = module_with_namespace(&module_views, path, &name.name)
                        .expect("guard checked the module exists");
                    let full_path = module.namespace.clone();
                    match origins.get(name.local()) {
                        None => {
                            origins.insert(
                                name.local().to_string(),
                                Origin::Import(full_path.clone()),
                            );
                            for decl in module.stmts.iter().filter(|s| decl_is_public(s)) {
                                let Some(dname) = qualifiable_decl_name(decl) else {
                                    continue;
                                };
                                if merged_q.insert(format!("{}.{dname}", full_path.join("."))) {
                                    let mut decl = decl.clone();
                                    if let Some(map) = module_maps.get(&full_path) {
                                        qualify::qualify_stmt(&mut decl, map);
                                    }
                                    imported.push(decl);
                                    merge_module_closure(
                                        &full_path,
                                        dname,
                                        &module_views,
                                        &module_maps,
                                        &mut merged_q,
                                        imported,
                                    );
                                }
                            }
                        }
                        // The same module re-imported under the same local name — merge once.
                        Some(Origin::Import(p)) if *p == full_path => {}
                        // A different declaration or module under the same local name — ambiguous.
                        Some(_) => errors.push(collision_error(entry, path, name)),
                    }
                }
                // No loaded module declares this namespace. Whether that is an error depends on how
                // much of the dependency graph the caller knows — see [`RetainPolicy`]. Either way,
                // a std extension (`std.http`) and a declared native-dependency root are always
                // retained (resolved downstream by the checker/compiler or the composed toolchain).
                Resolution::NoModule => {
                    let root = path.first();
                    let retained = match &retain {
                        // A **native extension root** (`std`, or a native dependency's own root like
                        // `para` for `para/api`) is always retained: its non-`.noe` modules
                        // (`para.url`, `std.http`) are resolved downstream by the registry, never by
                        // a loaded file. This must hold even when that root *also* names a loaded
                        // project namespace — a native package ships both `.noe` modules (`para.api`)
                        // and native modules (`para.url`) under one root — otherwise a dependency
                        // module's own `use para.url` is misread as a missing project module and the
                        // whole link fails (which silently broke `--watch` for `@openapi` programs:
                        // the impact session links Lenient, so it never saw the native module).
                        RetainPolicy::Lenient => {
                            root.is_some_and(|r| reg.is_extension_root(r))
                                || !root.is_some_and(|r| project_roots.contains(r))
                        }
                        RetainPolicy::Complete { native_roots } => root
                            .is_some_and(|r| reg.is_extension_root(r) || native_roots.contains(r)),
                    };
                    if retained {
                        // A retained (native) import binds a name in this file just as a resolved
                        // one does, so it answers the same one-name-one-meaning question. Without
                        // this it was invisible to the collision table, and a file importing BOTH a
                        // loaded `.noe` module and a native module of the same leaf name
                        // (`use pet_proxy.client` + `use std.http.client`) silently kept only the
                        // `.noe` meaning — `client.new(…)` resolved to the file's own package and
                        // surfaced much later as a missing function. Import-vs-import only: a
                        // native import that merely shares a name with a *declaration* is left to
                        // the checker's own shadowing rules, which already see both.
                        if canonical_use_binding(reg, path, &name.name).is_some() {
                            match origins.get(name.local()) {
                                None => {
                                    origins.insert(
                                        name.local().to_string(),
                                        Origin::Import(path.to_vec()),
                                    );
                                }
                                Some(Origin::Import(p)) if p.as_slice() != path => {
                                    errors.push(collision_error(entry, path, name));
                                }
                                Some(_) => {}
                            }
                        }
                        unresolved.push(name.clone());
                    // A namespace that IS declared, by a file that simply failed to parse, is not a
                    // missing module: it is a syntax error the consumer was never shown.
                    } else if let Some(module) =
                        broken_module_for(broken.iter().copied(), path, &name.name)
                    {
                        // The namespace *is* declared — by a file that failed to parse, which is
                        // the only reason it is missing from the pool. Report that file's parse
                        // error (the real fault, at its own span) and drop the "no module" here,
                        // which is merely its cascade and sends the reader to the wrong file.
                        if reported_broken.insert(module.source.name().to_string()) {
                            errors.extend(module.load_diagnostics());
                        }
                    } else {
                        let suggestion = noeta_diagnostics::closest(
                            &path.join("."),
                            import_targets.iter().map(String::as_str),
                        );
                        errors.push(unknown_module_error(
                            entry,
                            path,
                            name,
                            suggestion,
                            &unattributable,
                        ));
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
    // The tail runs as one flat scope, so qualified-chain shadowing must see every top-level
    // binding, not just the current statement's (`vec = …` in one statement shadows a `vec` module
    // alias in the next).
    let entry_bound: HashSet<String> = entry_program
        .stmts
        .iter()
        .flat_map(qualify::bound_value_names)
        .collect();
    // The **no-shadowing** rule's import half (the checker enforces the binder half as E0059, but
    // a user-module `use` is consumed by this linker before the checker ever sees it): a value
    // binding anywhere in a unit may not reuse the local name a `use` binds — one name, one
    // meaning. Checked per unit (the `use` is file-scoped), for the entry and every dependency
    // driver alike; only imports that actually resolve to a loaded module or item fire, so a
    // retained extern `use` (std — the checker's tables cover it) is not double-reported.
    for stmts in std::iter::once(&entry_program.stmts).chain(drivers.iter().map(|d| &d.stmts)) {
        let unit_bound: HashSet<String> =
            stmts.iter().flat_map(qualify::bound_value_names).collect();
        for stmt in stmts.iter() {
            if let Stmt::Use { path, names, .. } = stmt {
                for n in names {
                    if unit_bound.contains(n.local())
                        && (module_declares(&module_views, path, &n.name)
                            || module_with_namespace(&module_views, path, &n.name).is_some())
                    {
                        errors.push(shadowed_import_error(entry, path, n));
                    }
                }
            }
        }
    }

    // Dotted references that missed the entry's QMap (with spans) — filtered against the loaded
    // modules below to diagnose a qualified reference that lacks its `use`.
    let mut dotted_misses: Vec<(String, noeta_span::Span)> = Vec::new();
    // The entry unit's name table, seeded with its own declarations (each `Local`) — an import that
    // would shadow one of them, or clash with a differently-sourced import *in this same file*, is
    // the E0020 collision.
    let mut entry_origins = unit_origins(&entry_program.stmts);
    for stmt in &entry_program.stmts {
        match stmt {
            Stmt::Use { path, names, span } => {
                let unresolved =
                    drive_use(path, names, &mut entry_origins, &mut imported, &mut errors);
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
                // **A tier block's own `use`s drive linking too.** The block-scope overlay
                // ([`qualify::UnitMap::tier_scopes`]) already qualifies the block's references to
                // the module's identity — but qualifying a name is not linking it: a `.noe` module
                // only reaches the merged program when some `use` *merges* its declarations, and
                // the collection above walks the entry's **top-level** statements only. So
                // `@test { use probe.lib.side.{Thing} … }` produced a perfectly qualified
                // `probe.lib.side.Thing` that nothing declared — `noeta check` saw a qualified
                // name it does not adjudicate and reported nothing, and `noeta test` failed every
                // use site with "cannot find type … in this scope". A std import inside the same
                // block worked throughout, because an extension module resolves through the
                // registry and never needs the unit graph at all.
                //
                // Driven unconditionally, not per active tier: which tiers are live is the *build
                // target*'s call, taken downstream in `noeta_check::tiers` (the loader has no
                // active set, and the linked program is one memoized salsa value shared by
                // `check`/`run`/`test`). Merging is safe regardless — every merged declaration
                // lands under its **qualified** identity, so it binds no short name, collides with
                // nothing, and is unreferenced (hence stripped with the block) in a build where
                // the tier is inactive. This is also exactly what a top-level `use` that only the
                // `@test` block references already does.
                //
                // The unresolved remainder is deliberately **dropped** rather than retained: the
                // `use` statement itself stays *inside* the block (a rewrite table, not a hoisted
                // import), so an inactive build drops it with the block and leaves nothing
                // dangling. Activation inlines the block's items — the `use` among them — and the
                // retained binding is created then.
                if let Stmt::TierBlock { items, .. } = other {
                    // Block-scoped name table: the unit's, plus the block's own declarations. A
                    // `use` binds in one scope, so the E0020 question is answered per scope —
                    // and a block importing the same declaration the file already imports is the
                    // same import, not a clash.
                    let mut block_origins = entry_origins.clone();
                    block_origins.extend(unit_origins(items));
                    for item in items {
                        if let Stmt::Use { path, names, .. } = item {
                            drive_use(path, names, &mut block_origins, &mut imported, &mut errors);
                        }
                    }
                }
                // The entry's own declarations and statements qualify against the entry's map (its
                // own namespace + its resolved imports), with the whole tail's bindings in scope.
                let mut stmt = other.clone();
                qualify::qualify_stmt_scoped(
                    &mut stmt,
                    &entry_map,
                    &entry_bound,
                    &mut dotted_misses,
                );
                entry_stmts.push(stmt);
            }
        }
    }

    // Each dependency module's `use`s also drive imports (closed unit); their unresolved remainder
    // (std imports) is retained up front, ahead of the entry's statements.
    //
    // **Each driver gets its own name table.** A `use` binds a name in *one file*, so the E0020
    // collision question — "does this local name already mean something here?" — is answered per
    // file, exactly as the shadowing check above recomputes `unit_bound` per unit. One table shared
    // across every pooled module made two unrelated packages that each declare a `Middleware` (say
    // `para.aether` and `para.api`, two packages sharing the `para` import root) unlinkable: the
    // first package's own file to import its own `Middleware` claimed the name for the whole
    // program, and the second package's import of *its* `Middleware` was reported as a clash.
    for driver in drivers {
        let mut origins = unit_origins(&driver.stmts);
        // This driver's α-rename table (empty for a namespace-less module, which contributes
        // nothing to the merged program anyway). Its retained `use`s must be *aliased* to the same
        // canonical names its merged bodies were rewritten to, so the binding the backends create
        // and the reference that reads it are one decision, taken here.
        let handles = module_namespace(driver)
            .and_then(|ns| module_maps.get(&ns))
            .map(|m| m.handles.clone())
            .unwrap_or_default();
        for stmt in &driver.stmts {
            if let Stmt::Use { path, names, span } = stmt {
                let unresolved = drive_use(path, names, &mut origins, &mut imported, &mut errors);
                let unresolved = unresolved
                    .into_iter()
                    .map(|mut n| {
                        if let Some(canonical) = handles.get(n.local()) {
                            n.alias = Some(canonical.clone());
                        }
                        n
                    })
                    .collect();
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

    // **Qualified references require an import.** A dotted reference that missed the entry's QMap
    // but resolves to a loaded module's declaration is a spelled-out FQN with no `use` bringing it
    // in (`geometry.vec.Vec2 { … }` cold). Treating the FQN as its own implicit import was
    // considered and rejected: it would make a file's dependency set invisible — a second, silent
    // way to import next to the explicit `use` block the rest of the design leans on. Instead the
    // reference is a targeted error carrying the exact `use` to add (and a privacy message when
    // the declaration exists but is not `pub`). Chains suppressed by a local binding never land in
    // the miss list, so an ordinary member access on a module-named local stays silent; candidates
    // matching no loaded module fall through to the checker's own unknown-name error.
    let mut reported: HashSet<&str> = HashSet::new();
    for (name, span) in &dotted_misses {
        if !reported.insert(name.as_str()) {
            continue;
        }
        let Some((mpath, dname)) = split_fqn(name, &module_views) else {
            continue;
        };
        let namespace = mpath.join(".");
        let message = match resolve(&module_views, &mpath, dname) {
            Resolution::Resolved(_) => {
                format!("qualified reference `{name}` requires an import — add `use {namespace}`")
            }
            Resolution::Private => format!("`{dname}` is private to module `{namespace}`"),
            Resolution::Missing | Resolution::NoModule => continue,
        };
        errors.push(LoadDiagnostic {
            source: entry.clone(),
            diagnostic: Diagnostic::error(DiagnosticCode::UnresolvedImport, *span, message),
        });
    }

    // **A data attribute is a link root.** A `#[...]` exists precisely so that something which never
    // names a declaration can still find it — `attributes_of::<Tool>()` discovers it, `invoke` calls
    // it by name — so an annotated declaration in a pooled module belongs to the program even though
    // no `use` reaches it. Without this rule the manifest held only the annotated declarations the
    // entry happened to import *by name*: a sibling's `pub fn` that nothing imported and a
    // module-private one were both absent, so the registration mechanism attributes exist for could
    // not see its own registrations, and `attributes_of` / `roles_of` contradicted their documented
    // "every `#[T(...)]` attribute in the program".
    //
    // Scoped to the annotation, not to the file: only the *annotated* declaration is a root, dragged
    // in with the same same-module closure an imported one gets. An unannotated, unimported helper
    // stays out, so this is emphatically not "compile the whole directory" — a module contributes
    // exactly what it asked to be discovered by, plus whatever that needs in order to run.
    // Visibility does not gate it either, for the same reason it does not gate the closure: `#[Tool]`
    // on a non-`pub` fn is a registration, and reflection dispatches by name, not by import.
    //
    // The entry is a `module_views` member too when it declares a namespace, but `merged_q` is
    // pre-seeded with its declarations (they are the program's tail), so its own annotated
    // declarations are never merged a second time.
    for mv in &module_views {
        for stmt in mv.stmts {
            let Some(name) = qualifiable_decl_name(stmt) else {
                continue;
            };
            if !carries_data_attribute(stmt) {
                continue;
            }
            let name = name.to_string();
            if merge_one_dep(
                &name,
                mv,
                &mv.namespace,
                &module_maps,
                &mut merged_q,
                &mut imported,
            ) {
                expand_module_refs(
                    vec![name],
                    mv,
                    &mv.namespace,
                    &module_maps,
                    &mut merged_q,
                    &mut imported,
                );
            }
        }
    }

    // A standalone `impl Trait for T {}` in a pooled module (a sibling, or a dependency's own module)
    // has no import name, so the `use`-driven merge above never pulls it — yet coherence requires an
    // impl to travel with its target type (a `dyn Trait` coercion or a bound check needs to see it).
    // An **inline** impl rides on its type's `class`/`struct` declaration and is already merged with it;
    // this closes the **standalone** case. Merge each pooled standalone impl whose (qualified) target
    // type is in the program — so the impl only lands when the type it refers to is present — deduped
    // by the impl's own **span**, so a declaration reached more than once contributes once.
    //
    // **To a fixpoint, over the whole program's types.** "Is the target type present?" has an answer
    // that *grows while this loop runs*, and getting either half of that wrong drops an impl in
    // silence — the type still links and its inherent methods still dispatch, so the only symptom is
    // that the trait went missing, in a *consumer* of the package and never in the package's own
    // tests. Two ways it grew:
    //
    // - An impl's own dependency closure (below) merges the declarations its bodies name, so a type
    //   can arrive *because of another impl*: `impl Codec for MyCodec { fn decoder(): dyn Decoder {
    //     return MyDecoder.new() } }` is what brings `MyDecoder` in, and `impl Decoder for MyDecoder`
    //   is only eligible afterwards. One pass over a pre-loop snapshot never saw it, and no
    //   source order can rescue a single pass — the two impls may be written either way round, in
    //   either file — so iterate until a round merges nothing new.
    // - The **entry's own** declarations are the program's tail (`entry_stmts`), not `imported`, so a
    //   sibling's `impl Decoder for Target` against a `Target` the entry declares saw an absent type
    //   and was dropped.
    //
    // The dedup set answers the *same* question from the other side — "is this impl already in the
    // program?" — so it is seeded from the program rather than starting empty, and it is keyed on the
    // **span**: the identity of a declaration is where it is written.
    //
    // Both halves of that were wrong, and each dropped an impl silently. Starting empty: an entry
    // that declares a `namespace` is a `module_views` member like any other, so the scan below meets
    // the entry's own `impl Marker for Box2` again — with the entry's types now in `merged_types`
    // (above), an unseeded set made that a second copy and coherence correctly called it E0027
    // "implemented more than once". (Named declarations are protected from exactly this by
    // `merged_q`'s entry seeding; an impl introduces no name, so this is where it is seeded.)
    // Keying on `(target, trait)`: that is the identity of an impl's *coherence slot*, not of an
    // impl, so two different modules each writing `impl Decoder for Target` collapsed to whichever
    // the scan reached first, and the program ran with one of the two bodies and no diagnostic at
    // all. Those are a genuine conflict, and E0027 is what says so — the linker's job is to carry
    // each declaration into the program exactly once, not to adjudicate which of two should win.
    let mut seen_impls: HashSet<Span> = imported
        .iter()
        .chain(entry_stmts.iter())
        .filter_map(standalone_impl_span)
        .collect();
    loop {
        // Owned, and recomputed per round: the scan below pushes into `imported`, so it cannot hold
        // a borrow of it, and the round after must see whatever this one merged.
        let merged_types: HashSet<String> = imported
            .iter()
            .chain(entry_stmts.iter())
            .filter_map(decl_name)
            .map(str::to_string)
            .collect();
        let before = imported.len();
        for mv in &module_views {
            let Some(map) = module_maps.get(&mv.namespace) else {
                continue;
            };
            for stmt in mv.stmts {
                if !matches!(stmt, Stmt::Impl(_)) {
                    continue;
                }
                let mut cloned = stmt.clone();
                qualify::qualify_stmt(&mut cloned, map);
                if let Stmt::Impl(decl) = &cloned
                    && merged_types.contains(decl.target.as_str())
                    && seen_impls.insert(decl.span)
                {
                    imported.push(cloned);
                    // The impl's method bodies may reference same-module free declarations — an
                    // internal helper `fn`, a module-local type — that no `use` names and that the
                    // target type's own closure never reached (they are the impl's dependencies, not
                    // the type's). They must travel with the impl or the checker fails E0005 on a
                    // body it cannot see, and *only across the package boundary*: inside the module
                    // every declaration is present, so this hole is invisible until a consumer
                    // imports the type. Seed the same closure the `use`-driven merge runs, from the
                    // impl's own (pre-qualification, short-named) references.
                    let refs = qualify::referenced_names(stmt);
                    let mut work = Vec::new();
                    for name in refs {
                        if merge_one_dep(
                            &name,
                            mv,
                            &mv.namespace,
                            &module_maps,
                            &mut merged_q,
                            &mut imported,
                        ) {
                            work.push(name);
                        }
                    }
                    expand_module_refs(
                        work,
                        mv,
                        &mv.namespace,
                        &module_maps,
                        &mut merged_q,
                        &mut imported,
                    );
                }
            }
        }
        // Nothing merged ⇒ no type became present ⇒ no further impl can become eligible. `merged_q`
        // and `seen_impls` bound the total number of merges, so this terminates.
        if imported.len() == before {
            break;
        }
    }

    // A merged binding keeps its short name (it gains no qualified identity), so two modules that
    // each export a capturing declaration over a same-named module binding would land two `x`s in
    // one flat scope — and the later one would silently win for both. That is a genuine ambiguity
    // the linker must not adjudicate, so it is an error naming both sides. (Declarations cannot
    // reach this: they merge under qualified identities.)
    //
    // A merged statement keeps the span of the module it was cloned from, so the module each one
    // came from is its source — the same identity `source_maps` below keys on.
    let module_of = |stmt: &Stmt| -> String {
        module_views
            .iter()
            .find(|mv| {
                mv.stmts
                    .first()
                    .is_some_and(|first| first.span().source == stmt.span().source)
            })
            .map_or_else(|| "another module".to_string(), |mv| mv.namespace.join("."))
    };
    let mut binding_origin: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for (name, module) in imported.iter().flat_map(|stmt| match stmt {
        Stmt::Binding { name, .. } => vec![(name.clone(), module_of(stmt))],
        Stmt::Destructure { targets, .. } => targets
            .iter()
            .map(|(name, _)| (name.clone(), module_of(stmt)))
            .collect(),
        _ => Vec::new(),
    }) {
        if let Some(first) = binding_origin.insert(name.clone(), module.clone())
            && first != module
        {
            errors.push(LoadDiagnostic {
                source: entry.clone(),
                diagnostic: Diagnostic::error(
                    DiagnosticCode::NameCollision,
                    Span::empty_at(0),
                    format!(
                        "the module-level binding `{name}` is needed by declarations merged from \
                         both `{first}` and `{module}`, and a binding keeps its own name in the \
                         merged program"
                    ),
                )
                .with_help(
                    "rename one of them, or move the shared value behind a function both modules call",
                ),
            });
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    // The qualification map of every file that has one, keyed by the source it belongs to, so a
    // later pass can rewrite names *as that file would have*. Directive expansion is the one such
    // pass: its generated members are written against the imports of the file the directive sits
    // in, but they are parsed after this function has already qualified everything, so they would
    // otherwise reach the checker with bare names that resolve to nothing.
    let mut source_maps: std::collections::HashMap<SourceId, qualify::UnitMap> =
        std::collections::HashMap::new();
    source_maps.insert(entry.id(), entry_map);
    for mv in &module_views {
        // A module's source is its first statement's — every module has at least the `namespace`
        // declaration that got it into `module_views` at all.
        if let (Some(first), Some(map)) = (mv.stmts.first(), module_maps.get(&mv.namespace)) {
            source_maps.insert(first.span().source, map.clone());
        }
    }

    // Merged declarations, then dependency std-imports, then the entry's own statements.
    let mut stmts = imported;
    stmts.append(&mut dep_retained);
    stmts.append(&mut entry_stmts);
    Ok(Linkage {
        program: Program {
            stmts,
            span: entry_program.span,
        },
        source_maps,
    })
}

/// A linked program, plus the per-file qualification maps that produced it.
///
/// The maps travel with the program because linking is the **only** pass that still knows each
/// file's namespace and its own `use`s — after this, a `User` from module `App.A` and a `User` in
/// the entry are indistinguishable. Anything that has to introduce *new* code written in some
/// file's terms therefore has to borrow that file's map, and this is where it comes from.
#[derive(Debug)]
pub struct Linkage {
    pub program: Program,
    /// Keyed by the [`SourceId`] of the file the map belongs to. A file with no `namespace` and no
    /// imports has an empty map, which makes qualification a no-op — the correct answer, not a
    /// missing one.
    pub source_maps: std::collections::HashMap<SourceId, qualify::UnitMap>,
}

/// Filter `names` down to those not yet retained under `path`, recording the fresh ones — so a
/// `use std.…` shared by the entry and several dependencies is retained exactly once.
///
/// Keyed on the **binding** as well as the imported name: the entry keeps its short handle names
/// while every merged unit's are α-renamed to their canonical identity, so `use std.http.url` can
/// legitimately need two retained forms — one binding `url` for the entry, one binding
/// `std.http.url` for the dependency bodies that were rewritten to it. Deduping on the imported
/// name alone dropped the second and left those bodies calling an unbound name.
fn retain_fresh(
    seen: &mut HashSet<(Vec<String>, String, String)>,
    path: &[String],
    names: Vec<UseName>,
) -> Vec<UseName> {
    names
        .into_iter()
        .filter(|n| seen.insert((path.to_vec(), n.name.clone(), n.local().to_string())))
        .collect()
}

/// A candidate module viewed for resolution: its declared namespace path and a borrow of its
/// statements (so resolution clones only the declarations it actually imports).
struct ModuleView<'a> {
    namespace: Vec<String>,
    stmts: &'a [Stmt],
}

/// Whether `stmt` is a top-level **value binding** introducing `name` — `x = …` or a destructure
/// naming it. Unlike a class/fn/type it gains no qualified identity (it stays `x` in the flat merged
/// scope), so it is not a [`qualifiable_decl_name`]; it is still a thing a merged declaration can
/// need, through a `use (…)` capture.
fn binds_top_level_value(stmt: &Stmt, name: &str) -> bool {
    match stmt {
        Stmt::Binding { name: bound, .. } => bound == name,
        Stmt::Destructure { targets, .. } => targets.iter().any(|(bound, _)| bound == name),
        _ => false,
    }
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
        Stmt::Class(decl) => Some(decl.name.as_str()),
        Stmt::Struct(decl) => Some(decl.name.as_str()),
        Stmt::Enum(decl) => Some(decl.name.as_str()),
        Stmt::Fn(decl) => Some(decl.name.as_str()),
        // A user-defined trait is a qualifiable declaration (L1): a `dyn Trait` type, a `<T: Trait>`
        // bound, or an `impl Trait for T` referencing a module-local trait drags its declaration into
        // the merged program via the cross-module closure — without this a package-local trait
        // (e.g. aether's `Middleware`) is "unknown" once the package is linked as a dependency.
        Stmt::Trait(decl) => Some(decl.name.as_str()),
        _ => None,
    }
}

/// Whether a top-level declaration carries a `#[...]` **data attribute** anywhere inside it — on
/// itself, or on a member the reflection manifest keys under it (a method, a field, an enum
/// variant, a parameter, an in-body `impl` block's method).
///
/// This is the "is it a link root" test: an annotated declaration is part of the program whether or
/// not a `use` reaches it, because the annotation *is* the reference (see the root loop in
/// [`link_core`]). Members count because their attributes are keyed under the owning declaration
/// (`Type.method`, `Type.field`, `build#target`) — a `#[Route]` on a method of a class nothing
/// imported is exactly as discoverable-by-design as one on a free function.
///
/// Only `#[...]` data attributes count; a `@derive`/`@role`/`@semantic`/`@packed` **directive** does
/// not. A directive drives codegen on a declaration that is already in the program — it is not a
/// registration something else goes looking for — so making it a root would drag in declarations no
/// reflection query can ever return. (A `@role` still reaches the manifest, transitively: it rides
/// on an `@attribute` struct, and the *applications* of that struct are what this test finds.)
fn carries_data_attribute(stmt: &Stmt) -> bool {
    let on_fn = |decl: &noeta_ast::FnDecl| {
        !decl.attrs.is_empty() || decl.params.iter().any(|p| !p.attrs.is_empty())
    };
    let on_impls =
        |impls: &[noeta_ast::ImplBlock]| impls.iter().any(|block| block.methods.iter().any(&on_fn));
    match stmt {
        Stmt::Fn(decl) => on_fn(decl),
        Stmt::Struct(decl) => {
            !decl.decorators.attrs.is_empty()
                || decl.fields.iter().any(|f| !f.attrs.is_empty())
                || decl.methods.iter().any(&on_fn)
                || on_impls(&decl.impls)
        }
        Stmt::Class(decl) => {
            !decl.decorators.attrs.is_empty()
                || decl.fields.iter().any(|f| !f.attrs.is_empty())
                || decl.methods.iter().any(&on_fn)
                || on_impls(&decl.impls)
        }
        Stmt::Enum(decl) => {
            !decl.decorators.attrs.is_empty()
                || decl.variants.iter().any(|v| !v.attrs.is_empty())
                || decl.methods.iter().any(&on_fn)
                || on_impls(&decl.impls)
        }
        Stmt::Trait(decl) => {
            !decl.decorators.attrs.is_empty() || decl.methods.iter().any(|m| on_fn(&m.sig))
        }
        _ => false,
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
    module_maps: &std::collections::HashMap<Vec<String>, qualify::UnitMap>,
    merged_q: &mut HashSet<String>,
    imported: &mut Vec<Stmt>,
) {
    let Some(module) = module_views.iter().find(|m| m.namespace == path) else {
        return;
    };
    // The root is already merged (under its local name); record its qualified identity so it is not
    // merged again, then expand its references to a fixpoint.
    merged_q.insert(format!("{}.{root}", path.join(".")));
    expand_module_refs(
        vec![root.to_string()],
        module,
        path,
        module_maps,
        merged_q,
        imported,
    );
}

/// Merge one same-module declaration `name` and report whether it was **freshly** merged.
///
/// Merges only when `name` is a top-level declaration of `module` (not a param, builtin, extern, or
/// another module's export — those resolve elsewhere) and is not already merged (deduped through
/// `merged_q` on the qualified identity, so a declaration reached two ways lands once). The boolean
/// tells the caller whether to expand this declaration's own references in turn.
///
/// A top-level **value binding** is deliberately NOT reachable this way: the names driving this come
/// from `referenced_names`, a harmless over-approximation for declarations (merging one that turns
/// out to be unneeded is inert) but not for a binding, which *runs* its initializer. A parameter
/// that happens to share a module binding's name would otherwise drag that binding, and its side
/// effects, into a program that never asked for it. Bindings arrive through
/// [`merge_one_capture`] instead, whose seed — a `use (…)` clause — is exact.
fn merge_one_dep(
    name: &str,
    module: &ModuleView,
    path: &[String],
    module_maps: &std::collections::HashMap<Vec<String>, qualify::UnitMap>,
    merged_q: &mut HashSet<String>,
    imported: &mut Vec<Stmt>,
) -> bool {
    merge_matching(
        module,
        path,
        module_maps,
        merged_q,
        imported,
        name,
        |stmt| qualifiable_decl_name(stmt) == Some(name),
    )
}

/// Merge the same-module top-level **value binding** a merged declaration's `use (…)` names, and
/// report whether it was freshly merged. A capture is the one way a sealed body reaches a module
/// binding, so this seed is exact rather than an over-approximation — see [`merge_one_dep`].
fn merge_one_capture(
    name: &str,
    module: &ModuleView,
    path: &[String],
    module_maps: &std::collections::HashMap<Vec<String>, qualify::UnitMap>,
    merged_q: &mut HashSet<String>,
    imported: &mut Vec<Stmt>,
) -> bool {
    merge_matching(
        module,
        path,
        module_maps,
        merged_q,
        imported,
        name,
        |stmt| binds_top_level_value(stmt, name),
    )
}

/// The shared body of [`merge_one_dep`] and [`merge_one_capture`]: find the first same-module
/// statement `matches` accepts, merge it under its qualified identity (deduped), and report whether
/// this call is the one that merged it.
fn merge_matching(
    module: &ModuleView,
    path: &[String],
    module_maps: &std::collections::HashMap<Vec<String>, qualify::UnitMap>,
    merged_q: &mut HashSet<String>,
    imported: &mut Vec<Stmt>,
    name: &str,
    matches: impl Fn(&Stmt) -> bool,
) -> bool {
    let Some(decl) = module.stmts.iter().find(|s| matches(s)) else {
        return false;
    };
    if !merged_q.insert(format!("{}.{name}", path.join("."))) {
        return false;
    }
    let mut decl = decl.clone();
    if let Some(map) = module_maps.get(path) {
        qualify::qualify_stmt(&mut decl, map);
    }
    imported.push(decl);
    true
}

/// Drive the reachability worklist to a fixpoint: pop an already-merged declaration, merge every
/// fresh same-module declaration it references, and enqueue each for its own expansion. `merged_q`
/// membership makes cycles terminate. Shared by the two things that introduce a same-module
/// reference into the merged program — an imported declaration (seeded with its own name) and a
/// standalone `impl` block (seeded with its body's references).
fn expand_module_refs(
    mut work: Vec<String>,
    module: &ModuleView,
    path: &[String],
    module_maps: &std::collections::HashMap<Vec<String>, qualify::UnitMap>,
    merged_q: &mut HashSet<String>,
    imported: &mut Vec<Stmt>,
) {
    while let Some(name) = work.pop() {
        let Some(decl) = module
            .stmts
            .iter()
            .find(|s| qualifiable_decl_name(s) == Some(&name) || binds_top_level_value(s, &name))
        else {
            continue;
        };
        // A merged binding expands too: its initializer (`conn = connect()`) names declarations of
        // the same module, which have to travel with it.
        for referenced in qualify::referenced_names(decl) {
            if merge_one_dep(&referenced, module, path, module_maps, merged_q, imported) {
                work.push(referenced);
            }
        }
        // A `use (…)` capture is the ONE way a sealed body reaches a module-level value binding, so
        // the capture list is the exact edge set from a declaration to the bindings it needs. Merge
        // each one: without it an imported (or attribute-discovered) `pub fn f() use (x)` lands in
        // the program with its binding left behind in the module it came from, and `x` resolves to
        // nothing — E0005 "cannot capture `x`" against a declaration the consumer never wrote.
        //
        // Precise on purpose: seeded from `captures`, never from `referenced_names` — see
        // `merge_one_dep` for why a binding must not ride the coarser seed.
        for captured in captures_of(decl) {
            if merge_one_capture(&captured, module, path, module_maps, merged_q, imported) {
                work.push(captured);
            }
        }
        // A merged **binding** carries an initializer that has to evaluate in the consumer's
        // program, and an initializer names its module's other bindings directly — top-level code
        // is not a sealed body, so `todos = repository(conn)` reaches `conn` with no capture clause
        // to declare it. Those are edges too, and the *free* names are the exact set: subtracting
        // everything the statement binds itself keeps a closure parameter inside the initializer
        // from dragging a same-named module binding along with it.
        if binds_any_top_level_value(decl) {
            let bound = qualify::bound_value_names(decl);
            for referenced in qualify::referenced_names(decl) {
                if bound.contains(&referenced) {
                    continue;
                }
                if merge_one_capture(&referenced, module, path, module_maps, merged_q, imported) {
                    work.push(referenced);
                }
            }
        }
    }
}

/// Whether `stmt` is a top-level value binding at all (`x = …` or a destructure) — the statements
/// whose initializers run in the merged program, so their free names are merge edges.
fn binds_any_top_level_value(stmt: &Stmt) -> bool {
    matches!(stmt, Stmt::Binding { .. } | Stmt::Destructure { .. })
}

/// Every `use (…)` capture on a top-level declaration: a free function's own, and the methods' of a
/// class/struct/`impl` block. Shallow by design — a *nested* `fn`'s captures resolve at ITS
/// declaration site (inside the enclosing body), so a module binding it names is necessarily
/// captured by the enclosing top-level function too, and is reached through that.
fn captures_of(stmt: &Stmt) -> Vec<String> {
    let names = |methods: &[noeta_ast::FnDecl]| -> Vec<String> {
        methods
            .iter()
            .flat_map(|m| m.captures.iter().map(|(n, _)| n.clone()))
            .collect()
    };
    match stmt {
        Stmt::Fn(decl) => decl.captures.iter().map(|(n, _)| n.clone()).collect(),
        Stmt::Class(decl) => names(&decl.methods)
            .into_iter()
            .chain(decl.impls.iter().flat_map(|b| names(&b.methods)))
            .collect(),
        Stmt::Struct(decl) => names(&decl.methods)
            .into_iter()
            .chain(decl.impls.iter().flat_map(|b| names(&b.methods)))
            .collect(),
        Stmt::Impl(decl) => names(&decl.methods),
        _ => Vec::new(),
    }
}

/// The loaded module whose namespace is exactly `path` + `name` — the target of a **whole-module**
/// import (`use geometry.vec` where a module declares `namespace geometry.vec;`). `None` when the
/// dotted path names no loaded module (it is then an item import, an extern, or unresolved).
fn module_with_namespace<'m, 'a>(
    modules: &'m [ModuleView<'a>],
    path: &[String],
    name: &str,
) -> Option<&'m ModuleView<'a>> {
    modules.iter().find(|m| {
        m.namespace.len() == path.len() + 1
            && m.namespace[..path.len()] == *path
            && m.namespace.last().is_some_and(|last| last == name)
    })
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
///
/// `canonical_handles` additionally fills [`qualify::UnitMap::handles`] — the α-rename of this
/// unit's **native** `use` bindings (see [`native_use_handles`]). It is on for every *merged* unit
/// and off for the entry, because the merged program's flat global scope **is** the entry's scope:
/// the entry's own short names are already the program's, and every other file's file-scoped
/// bindings are renamed into it, exactly as their declarations are qualified into it.
fn build_module_map(
    own_ns: &[String],
    own_stmts: &[Stmt],
    modules: &[ModuleView],
    reg: &noeta_ext_abi::registry::Registry,
    canonical_handles: bool,
) -> qualify::UnitMap {
    let mut map = qualify::QMap::new();
    // Identity entries (`App.Models.User` → itself) look like no-ops but are load-bearing: the
    // member-chain collapse in `qualify` turns a chain into a flat `Ident(FQN)` only on a map
    // *hit*, so a spelled-out FQN reference needs its own key to collapse.
    if !own_ns.is_empty() {
        let prefix = own_ns.join(".");
        for stmt in own_stmts {
            if let Some(name) = qualifiable_decl_name(stmt) {
                let qualified = format!("{prefix}.{name}");
                map.insert(qualified.clone(), qualified.clone());
                map.insert(name.to_string(), qualified);
            }
        }
    }
    // A unit's own declarations shadow an import of the same name, whichever scope the import sits
    // in — so the set is the unit's, shared by the unit map and every tier-block overlay below.
    let declared: HashSet<&str> = own_stmts.iter().filter_map(decl_name).collect();
    add_module_import_aliases(&mut map, own_stmts, modules);
    add_native_type_aliases(&mut map, own_stmts, &declared, reg);
    let handles = if canonical_handles {
        native_use_handles(own_stmts, modules, reg)
    } else {
        qualify::QMap::new()
    };
    // A tier block's own `use`s (`@test { use std.test.{Skip} … }`) bind inside the block only, so
    // they get a **per-block overlay** instead of joining the tables above — see
    // [`qualify::UnitMap::tier_scopes`]. Without this the block's references were qualified against
    // the unit's table alone: `#[Skip("…")]` stayed the bare `Skip` the runner (which matches the
    // qualified `std.test.Skip`) never recognizes, so activation lifted a test whose skip had
    // silently evaporated.
    let mut tier_scopes = std::collections::HashMap::new();
    for stmt in own_stmts {
        let Stmt::TierBlock { items, span, .. } = stmt else {
            continue;
        };
        if !items.iter().any(|s| matches!(s, Stmt::Use { .. })) {
            continue;
        }
        let mut names = map.clone();
        add_module_import_aliases(&mut names, items, modules);
        add_native_type_aliases(&mut names, items, &declared, reg);
        let mut block_handles = handles.clone();
        if canonical_handles {
            block_handles.extend(native_use_handles(items, modules, reg));
        }
        tier_scopes.insert(
            *span,
            qualify::UnitMap {
                names,
                handles: block_handles,
                tier_scopes: std::collections::HashMap::new(),
            },
        );
    }
    qualify::UnitMap {
        names: map,
        handles,
        tier_scopes,
    }
}

/// Fold the **loaded-module** imports among `use_stmts` into a rewrite map: an import that resolves
/// to a real module qualifies to that module's identity, keyed by the import's local (alias-aware)
/// name. Split out of [`build_module_map`] so a tier block's own `use`s can be folded into a
/// block-scoped overlay through the exact same rule.
fn add_module_import_aliases(map: &mut qualify::QMap, use_stmts: &[Stmt], modules: &[ModuleView]) {
    for stmt in use_stmts {
        let Stmt::Use { path, names, .. } = stmt else {
            continue;
        };
        for n in names {
            if module_declares(modules, path, &n.name) {
                let qualified = format!("{}.{}", path.join("."), n.name);
                map.insert(qualified.clone(), qualified.clone());
                map.insert(n.local().to_string(), qualified);
            } else if let Some(module) = module_with_namespace(modules, path, &n.name) {
                // A whole-module import (`use geometry.vec`): every `pub` declaration aliases
                // under the local binding — `vec.Vec2` → `geometry.vec.Vec2` — so qualified
                // references (struct-literal heads, annotations, patterns, member chains)
                // rewrite to the FQN the merged program declares.
                let ns = module.namespace.join(".");
                for decl in module.stmts.iter().filter(|s| decl_is_public(s)) {
                    if let Some(dname) = qualifiable_decl_name(decl) {
                        let qualified = format!("{ns}.{dname}");
                        map.insert(qualified.clone(), qualified.clone());
                        map.insert(format!("{}.{dname}", n.local()), qualified);
                    }
                }
            }
        }
    }
}

/// A unit's **native `use` handles**: each import that binds a name in the *value* namespace and
/// resolves to no loaded file → the canonical name the merged program binds it under.
///
/// A `use` binds in one file; the merged program has one flat global scope. A leaf name is
/// therefore not a usable binding key across units — `use std.http.url` in a dependency and
/// `use para.url` in a package it never heard of both want to bind `url`, and whichever `use`
/// executed last used to win for the whole program (silently, and only at run time: the checker
/// keeps its own table). The identity a native import resolves to is unique, so it is the binding
/// name: `use std.http.url` binds `std.http.url`, `use std.http.url.{decode as d}` binds
/// `std.http.url.decode`. Because every canonical name is dotted, it can never collide with the
/// entry's own short-named bindings either.
///
/// [`Registry::classify_use`](noeta_ext_abi::registry::Registry::classify_use) is the classifier —
/// the same one the checker and both backends consult — so the name recorded here is by
/// construction the name they resolve the import to. Imports that resolve to a loaded `.noe` module
/// are excluded: those are merged and rewritten through [`qualify::UnitMap::names`] instead.
fn native_use_handles(
    own_stmts: &[Stmt],
    modules: &[ModuleView],
    reg: &noeta_ext_abi::registry::Registry,
) -> qualify::QMap {
    let mut handles = qualify::QMap::new();
    for stmt in own_stmts {
        let Stmt::Use { path, names, .. } = stmt else {
            continue;
        };
        for n in names {
            if module_declares(modules, path, &n.name)
                || module_with_namespace(modules, path, &n.name).is_some()
            {
                continue;
            }
            if let Some(canonical) = canonical_use_binding(reg, path, &n.name) {
                handles.insert(n.local().to_string(), canonical);
            }
        }
    }
    handles
}

/// The canonical binding name of a native import, or `None` when the import binds nothing in the
/// **value** namespace (a type/enum/class/trait import binds in the type namespace, which the
/// qualified-identity rewrite already covers, and an unresolvable target binds nothing at all).
fn canonical_use_binding(
    reg: &noeta_ext_abi::registry::Registry,
    path: &[String],
    name: &str,
) -> Option<String> {
    use noeta_ext_abi::registry::UseKind;
    match reg.classify_use(path, name) {
        // A whole module (`use std.http.url`) or a navigable namespace group (`use std.http`):
        // the handle *is* the qualified path.
        UseKind::Module(qualified) | UseKind::Namespace(qualified) => Some(qualified),
        // A selective member import (`use std.http.url.{decode}`) binds one function value; its
        // canonical name is the function's own qualified identity.
        UseKind::MemberFn { module, func } => Some(format!("{module}.{func}")),
        _ => None,
    }
}

/// Fold a module's **native type** imports into its rewrite map, so every spelling of a native type
/// rewrites to the one qualified identity the rest of the toolchain keys on (`std.http.Framing`,
/// `std.test.Skip`) — the FQN the checker seeds its symbol tables under, the reflection manifest
/// carries, both backends build shapes from, and the test/bench/mcp/doc runners read. Mirrors the
/// checker's `use` classification (`classify_use`).
///
/// Two spellings reach here. A **leaf** import (`use std.test.{Skip}`) binds the short local name;
/// a **group** import (`use std.http`, or a concrete module `use std.test.mod`) binds the projected
/// dotted form (`http.Framing`). Aliasing the group form is what makes `http.Framing.Sse` work at
/// all: the collapse in [`qualify`] turns the dotted prefix into a single `Ident(std.http.Framing)`
/// with `.Sse` still on it, which is exactly the shape a leaf-imported `Framing.Sse` reaches the
/// backends as. Without the alias the chain stayed `((http).Framing).Sse`, the compiler saw a member
/// access on the namespace handle, and the program died at *compile* time with an internal
/// "type member used as a value" — while `use std.http.{Framing}` + `Framing.Sse` worked. That
/// asymmetry is not a curiosity: two packages exporting the same short name (`std.http.Framing` and
/// `para.ai.provider.Framing`) cannot both be leaf-imported, so the dotted form is the *only*
/// spelling available to a program that needs both.
///
/// A **local declaration wins**: a name a file declares itself is not rewritten to a native type of
/// the same name, matching the shadowing rule the rest of the prelude follows. `declared` is the
/// unit's own declaration names, passed in rather than derived from `use_stmts` so a tier-block
/// overlay (whose `use_stmts` are one block's imports) still honors the whole file's shadowing.
fn add_native_type_aliases(
    map: &mut qualify::QMap,
    use_stmts: &[Stmt],
    declared: &HashSet<&str>,
    reg: &noeta_ext_abi::registry::Registry,
) {
    use noeta_ext_abi::registry::UseKind;
    let mut alias = |local: String, qualified: String| {
        if declared.contains(local.as_str()) {
            return;
        }
        map.insert(qualified.clone(), qualified.clone());
        map.insert(local, qualified);
    };
    for stmt in use_stmts {
        let Stmt::Use { path, names, .. } = stmt else {
            continue;
        };
        for n in names {
            let local = n.local();
            match reg.classify_use(path, &n.name) {
                // A leaf import of a native attribute struct. (Other leaf-imported native types are
                // the checker's `extern_types` job and already resolve; aliasing them here too would
                // rewrite a *value* spelling the backends bind under the short name.)
                UseKind::ExtStruct(qualified) if reg.is_ext_attribute(&qualified) => {
                    alias(local.to_string(), qualified);
                }
                // A group import — every native **value type** under the namespace, by its dotted
                // spelling. Enums, fielded types (classes/structs), and attribute structs only: a
                // native **trait** is keyed by its *short* name throughout the checker (`impl
                // fx.Pixels for T`, a `T: Pixels` bound, `dyn Pixels`), so rewriting its dotted
                // spelling to the qualified identity makes the `impl` name a trait the checker has
                // never heard of. A trait is a contract, not a value — nothing constructs one — so
                // it needs no dotted alias in the first place.
                UseKind::Namespace(prefix) | UseKind::Module(prefix) => {
                    for (rel, q) in reg.namespace_types(&prefix) {
                        let is_value_type = reg.find_enum_qualified(&q).is_some()
                            || reg.resolve_fielded(&q).is_some()
                            || reg.is_ext_attribute(&q);
                        if is_value_type {
                            alias(format!("{local}.{rel}"), q);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Split a dotted reference candidate into `(module path, declaration name)` when its prefix is a
/// loaded module's namespace — `geometry.vec.Vec2` → `(["geometry", "vec"], "Vec2")`. `None` when
/// no loaded module matches (an ordinary member chain like `customer.name`). Chain candidates
/// arrive one prefix at a time (the collapse walk visits every length), so a single split
/// suffices.
fn split_fqn<'c>(candidate: &'c str, modules: &[ModuleView]) -> Option<(Vec<String>, &'c str)> {
    let (prefix, dname) = candidate.rsplit_once('.')?;
    modules
        .iter()
        .find(|m| {
            m.namespace.len() == prefix.split('.').count()
                && m.namespace.iter().map(String::as_str).eq(prefix.split('.'))
        })
        .map(|m| (m.namespace.clone(), dname))
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
/// `unparseable` are broken modules whose own `namespace` could not be recovered, so the linker
/// cannot tell whether one of them *is* this module; naming them keeps the reader from concluding
/// the module does not exist when it does and merely does not parse.
fn unknown_module_error(
    entry: &Source,
    path: &[String],
    name: &UseName,
    suggestion: Option<&str>,
    unparseable: &[&str],
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
    if !unparseable.is_empty() {
        diagnostic.help(format!(
            "these files failed to parse, so the modules they declare are not loaded: {}",
            unparseable.join(", ")
        ));
    }
    LoadDiagnostic {
        source: entry.clone(),
        diagnostic,
    }
}

/// The broken module that declares the namespace a `use <path>.<name>` names — either exactly
/// (`path`, an item import) or as `path` + `name` (a whole-module import), mirroring [`resolve`] and
/// [`module_with_namespace`]. `None` when no broken module accounts for the import, which is when
/// "no module" is the honest answer.
///
/// **THE** matching rule, public and iterator-shaped because two layers ask the same question and
/// must not answer it differently: the linker, deciding whether to report a parse error in place of
/// an E0019, and the IDE (`noeta_ide`), explaining an import at the *consumer's* own span. A module
/// whose namespace could not be recovered (`namespace: None`) matches nothing — it cannot be shown
/// to be the module in question.
pub fn broken_module_for<'b>(
    broken: impl IntoIterator<Item = &'b BrokenModule>,
    path: &[String],
    name: &str,
) -> Option<&'b BrokenModule> {
    broken.into_iter().find(|m| {
        m.namespace.as_deref().is_some_and(|ns| {
            ns == path
                || (ns.len() == path.len() + 1
                    && ns[..path.len()] == *path
                    && ns.last().is_some_and(|last| last == name))
        })
    })
}

/// Build the `E0020` diagnostic for an import whose local name collides with another top-level name
/// **in the same file** (a second import of it, or a declaration that file makes), pointed at the
/// imported name.
///
/// `entry` is the *provisional* render target — the linking core has no [`Source`] for a dependency
/// module, so the file the span really indexes is resolved afterwards by [`attribute_to_spans`].
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

/// Build the `E0020` diagnostic for an import whose local name a value binding in the same unit
/// also uses (the no-shadowing rule's import half — the binder half is the checker's E0059).
/// Points at the imported name; the fix is a rename on either side.
fn shadowed_import_error(entry: &Source, path: &[String], name: &UseName) -> LoadDiagnostic {
    let namespace = path.join(".");
    LoadDiagnostic {
        source: entry.clone(),
        diagnostic: Diagnostic::error(
            DiagnosticCode::NameCollision,
            name.span,
            format!(
                "imported `{}` collides with a local binding of the same name",
                name.local()
            ),
        )
        .with_help(format!(
            "every name means one thing per scope — rename the binding, or import under an \
             alias (`use {namespace}.{} as …`)",
            name.name
        )),
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
        Stmt::Class(decl) => Some(decl.name.as_str()),
        Stmt::Struct(decl) => Some(decl.name.as_str()),
        Stmt::Enum(decl) => Some(decl.name.as_str()),
        Stmt::Fn(decl) => Some(decl.name.as_str()),
        // A user-defined trait is an importable name (L1) — `use pkg.mod.{MyTrait}` brings it into
        // scope for `dyn MyTrait`, a `<T: MyTrait>` bound, or an `impl MyTrait for T`.
        Stmt::Trait(decl) => Some(decl.name.as_str()),
        _ => None,
    }
}

/// The identity of a **standalone** `impl Trait for Target`: its span. `None` for every other
/// statement.
///
/// An impl declares no name, so it is absent from every name-keyed table the linker dedups with
/// (`merged_q`, `unit_origins`), and "where it is written" is what identifies it instead — stable
/// under [`qualify::qualify_stmt`], which rewrites names and leaves spans alone, so a merged clone
/// still answers to its source statement. Deliberately *not* `(target, trait)`: that names the
/// coherence slot rather than the declaration, so two modules that both fill it would collapse into
/// one silently instead of reaching the checker as the E0027 they are.
fn standalone_impl_span(stmt: &Stmt) -> Option<Span> {
    match stmt {
        Stmt::Impl(decl) => Some(decl.span),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_ast::Expr;

    /// Seeding wrappers: production drivers seed the process-default registry before the loader
    /// runs (audit-6 F2 — the loader consumes the registry as data and does not link the std
    /// units); these tests are their own driver, so the wrappers seed via the dev-dependency.
    /// Local items shadow the `use super::*` glob, so every call below goes through them.
    fn link(
        entry_name: &str,
        entry_text: &str,
        root_edition: noeta_lexer::Edition,
        siblings: &[RawModule],
    ) -> Result<Linked, Vec<LoadDiagnostic>> {
        noeta_stdlib::registry::default_seeded();
        super::link(
            entry_name,
            entry_text,
            root_edition,
            siblings,
            ModulePath::Declared,
        )
    }

    fn link_with_deps(
        entry_name: &str,
        entry_text: &str,
        root_edition: noeta_lexer::Edition,
        siblings: &[RawModule],
        deps: &[DepPackage],
    ) -> Result<Linked, Vec<LoadDiagnostic>> {
        noeta_stdlib::registry::default_seeded();
        super::link_with_deps(
            entry_name,
            entry_text,
            root_edition,
            siblings,
            deps,
            &noeta_span::PackageUses::new(),
            ModulePath::Declared,
        )
    }

    fn module(name: &str, text: &str) -> RawModule {
        RawModule::declared(name, text)
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
            .any(|s| matches!(s, Stmt::Class(c) if leaf(c.name.as_str()) == name))
    }

    fn has_fn(linked: &Linked, name: &str) -> bool {
        linked
            .program
            .stmts
            .iter()
            .any(|s| matches!(s, Stmt::Fn(f) if leaf(f.name.as_str()) == name))
    }

    fn has_struct(linked: &Linked, name: &str) -> bool {
        linked
            .program
            .stmts
            .iter()
            .any(|s| matches!(s, Stmt::Struct(d) if leaf(d.name.as_str()) == name))
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
                "namespace a;\npub fn ay(): int {\n  1\n}\n",
            )],
            dep_renames: Default::default(),
            native: false,
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
        };
        let dep_b = DepPackage {
            key: "b".to_string(),
            root: "b".to_string(),
            modules: vec![module(
                "b.noe",
                "namespace b;\npub fn bee(): int {\n  2\n}\n",
            )],
            dep_renames: Default::default(),
            native: false,
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
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
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
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
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
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
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
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

    // --- broken modules surface their own parse errors (the "no module" cascade) ---------------

    /// A dependency package whose `broken.noe` has a syntax error, plus a clean sibling module.
    fn dep_with_broken_module() -> DepPackage {
        DepPackage {
            key: "para".to_string(),
            root: "para".to_string(),
            modules: vec![
                module(
                    "pkg/thing.noe",
                    "namespace para.thing;\npub fn greet(): string { return \"hi\"; }\n",
                ),
                module(
                    "pkg/broken.noe",
                    "namespace para.thing.broken;\npub fn Something(): string {\n  let ] = ;\n}\n",
                ),
            ],
            dep_renames: Default::default(),
            native: false,
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
        }
    }

    #[test]
    fn a_dependency_module_that_does_not_parse_reports_its_own_parse_error() {
        // THE wart: a syntax error inside a dependency package used to be swallowed — the module
        // simply never reached the link pool — and the consumer was handed
        // `[E0019] no module `para.thing.broken` in this project` at its own `use`, sending it to
        // inspect its import and the package's naming while the real fault sat unreported in a file
        // it was never told about. The parse error must be the diagnostic, attributed to the
        // offending FILE, and the E0019 cascade must not be printed alongside it.
        let entry = "use para.thing.broken.Something;\necho Something();\n";
        let errors = link_with_deps(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[],
            std::slice::from_ref(&dep_with_broken_module()),
        )
        .unwrap_err();
        assert!(
            errors.iter().all(|e| e.source.name() == "pkg/broken.noe"),
            "every diagnostic must be attributed to the broken file, got {errors:?}"
        );
        assert!(
            !errors
                .iter()
                .any(|e| e.diagnostic.code == DiagnosticCode::UnresolvedImport),
            "the misleading `no module` cascade must be suppressed, got {errors:?}"
        );
    }

    #[test]
    fn a_broken_dependency_module_fails_even_when_nothing_imports_it() {
        // A package is a closed unit that must be internally valid, and its files are never anyone's
        // entry — so this is the only pass that will ever look at them. "Skip it quietly" means the
        // fault is reported nowhere at all, however long the file stays broken.
        let entry = "use para.thing.greet;\necho greet();\n";
        let errors = link_with_deps(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[],
            std::slice::from_ref(&dep_with_broken_module()),
        )
        .unwrap_err();
        assert!(
            errors.iter().all(|e| e.source.name() == "pkg/broken.noe"),
            "expected the unreferenced broken module's parse error, got {errors:?}"
        );
    }

    #[test]
    fn a_native_package_contributing_no_modules_is_still_retained() {
        // The guard on the fix above: a NATIVE package's modules are *legitimately* absent from the
        // link pool (they live in its Rust extension, composed downstream) — that is not a broken
        // module, and a `use` under its key must still be retained, never flagged. A pure-Noeta
        // package with a broken file and a native package with no files must not be confused.
        let native = DepPackage {
            key: "imgfx".to_string(),
            root: "imgfx".to_string(),
            modules: Vec::new(),
            dep_renames: Default::default(),
            native: true,
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
        };
        let entry = "use imgfx.fx;\necho fx.double(21);\n";
        let linked = link_with_deps(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[],
            std::slice::from_ref(&native),
        )
        .expect("a native package's absent modules are not a parse failure");
        assert!(
            linked
                .program
                .stmts
                .iter()
                .any(|s| matches!(s, Stmt::Use { path, .. } if path == &["imgfx".to_string()])),
            "the native import must still be retained"
        );
    }

    #[test]
    fn a_broken_sibling_attributes_the_import_that_cascades_off_it() {
        // The same wart in its intra-project shape. A broken *sibling* keeps the skip-and-continue
        // policy (a lone script must not fail because an unrelated file in its directory is
        // mid-edit), so it is only reported when something actually imports the namespace it
        // declares — and then it is the parse error, at the sibling's own span, not the E0019.
        let entry =
            "namespace App.Orders;\nuse App.Models.User;\npub fn make(): ?User { return none; }\n";
        let sibling = module(
            "models.noe",
            "namespace App.Models;\npub struct User { let ] = ; }\n",
        );
        let errors = link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&sibling),
        )
        .unwrap_err();
        assert!(
            errors.iter().all(|e| e.source.name() == "models.noe"),
            "expected the broken sibling's own parse errors, got {errors:?}"
        );
        assert!(
            !errors
                .iter()
                .any(|e| e.diagnostic.code == DiagnosticCode::UnresolvedImport),
            "the `no module `App.Models`` cascade must be suppressed, got {errors:?}"
        );
    }

    #[test]
    fn an_unimported_broken_sibling_does_not_fail_the_entry() {
        // The other half of the sibling policy: a file the entry never imports must not break it.
        // (`noeta check` visits that file as its own entry and reports it there.)
        let entry = "namespace App.Orders;\necho 1;\n";
        let sibling = module("scratch.noe", "namespace App.Scratch;\nlet ] = ;\n");
        link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&sibling),
        )
        .expect("an unimported broken sibling is not the entry's error");
    }

    #[test]
    fn attribution_survives_a_syntax_error_before_the_namespace() {
        // Reading the namespace off the *tokens* rather than the ast buys this: even when the
        // syntax error comes first, the `namespace` line is still a token sequence, so the broken
        // file is still identified as the module the consumer wanted.
        let entry =
            "namespace App.Orders;\nuse App.Models.User;\npub fn make(): ?User { return none; }\n";
        let sibling = module("models.noe", "let ] = ;\nnamespace App.Models;\n");
        let errors = link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&sibling),
        )
        .unwrap_err();
        assert!(
            errors.iter().all(|e| e.source.name() == "models.noe"),
            "expected the broken sibling's own parse errors, got {errors:?}"
        );
    }

    #[test]
    fn a_broken_module_that_cannot_name_itself_is_still_pointed_at() {
        // When not even the tokens yield a namespace — here an unterminated block comment swallows
        // the whole file — the linker cannot tell which module the broken file *would* have been,
        // so "no module" is the honest answer. The file is named on the diagnostic all the same, so
        // the reader is never left concluding the module does not exist when it does and merely
        // does not parse.
        let entry =
            "namespace App.Orders;\nuse App.Models.User;\npub fn make(): ?User { return none; }\n";
        let sibling = module(
            "models.noe",
            "/* namespace App.Models;\npub struct User {}\n",
        );
        let errors = link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&sibling),
        )
        .unwrap_err();
        let err = errors
            .iter()
            .find(|e| e.diagnostic.code == DiagnosticCode::UnresolvedImport)
            .unwrap_or_else(|| panic!("expected E0019 for `App.Models`, got {errors:?}"));
        assert!(
            err.diagnostic
                .help
                .as_deref()
                .is_some_and(|h| h.contains("models.noe")),
            "the E0019 should name the file that failed to parse, got {:?}",
            err.diagnostic.help
        );
    }

    #[test]
    fn namespace_is_recovered_from_tokens_when_the_ast_is_not() {
        // The mechanism the attribution rests on: the namespace comes off the TOKEN stream, never
        // the ast.
        //
        // This originally asserted the ast could name nothing, because a hard parse failure
        // produced no output at all. That premise stopped holding when statement recovery learned
        // to resync past a failed statement's own brace group — this fixture now recovers far
        // enough to salvage its `namespace` statement. The token path is not thereby redundant:
        // recovery is best-effort and salvages nothing when the fault PRECEDES the `namespace`
        // line (see `attribution_survives_an_error_before_the_namespace`), so attribution must
        // never depend on how much of the ast happened to survive. What is pinned here is the
        // token path's answer, which is the one attribution actually uses.
        let text = "namespace App.Models.Deep;\npub struct User { let ] = ; }\n";
        let source = Source::new(SourceId(0), "models.noe", text);
        let lexed = noeta_lexer::lex_in(
            &source,
            noeta_lexer::Edition::DEFAULT,
            &noeta_lexer::TextTiers::default(),
        );
        let parsed = noeta_parser::parse_in(
            &source,
            &lexed.tokens,
            noeta_lexer::Edition::DEFAULT,
            &noeta_lexer::TextTiers::default(),
        );
        assert!(!parsed.diagnostics.is_empty(), "the fixture must not parse");
        assert_eq!(
            namespace_from_tokens(&source, &lexed.tokens),
            Some(vec![
                "App".to_string(),
                "Models".to_string(),
                "Deep".to_string()
            ])
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
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
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
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
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
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
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
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
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
    fn two_packages_may_each_import_their_own_declaration_of_one_name() {
        // The `para` scope, reduced: two packages share the import root `para`, and each declares
        // *and internally imports* a `Middleware`. Every `use` here is file-scoped — no one file
        // binds `Middleware` twice — so this must link clean. It did not: the collision table was
        // one flat bare-name map spanning every compilation unit, so `para-aether`'s own file
        // claimed `Middleware` for the whole program and `para-api`'s claim of *its* `Middleware`
        // came back as E0020.
        let aether = DepPackage {
            key: "para".to_string(),
            root: "para".to_string(),
            modules: vec![
                module(
                    "aether.noe",
                    "namespace para.aether;\npub trait Middleware { fn run(): int }\n",
                ),
                module(
                    "aether_use.noe",
                    "namespace para.aether.serve;\nuse para.aether.{Middleware};\n\
                     pub fn drive(m: dyn Middleware): int { m.run() }\n",
                ),
            ],
            dep_renames: Default::default(),
            native: false,
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
        };
        let api = DepPackage {
            key: "para".to_string(),
            root: "para".to_string(),
            modules: vec![
                module(
                    "api.noe",
                    "namespace para.api;\npub trait Middleware { fn call(): int }\n",
                ),
                module(
                    "api_use.noe",
                    "namespace para.api.middleware;\nuse para.api.{Middleware};\n\
                     pub fn apply(m: dyn Middleware): int { m.call() }\n",
                ),
            ],
            dep_renames: Default::default(),
            native: false,
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
        };
        let entry = "use para.aether.serve.{drive};\nuse para.api.middleware.{apply};\n";
        let linked = link_with_deps(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[],
            &[aether, api],
        )
        .unwrap();

        // Both traits are present, each under its own qualified identity, and exactly once.
        let traits: Vec<&str> = linked
            .program
            .stmts
            .iter()
            .filter_map(|s| match s {
                Stmt::Trait(t) => Some(t.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            traits,
            vec!["para.aether.Middleware", "para.api.Middleware"],
            "each package's own `Middleware` merges exactly once, under its own identity"
        );
        assert!(has_fn(&linked, "drive"));
        assert!(has_fn(&linked, "apply"));
    }

    #[test]
    fn one_declaration_imported_by_two_files_merges_once() {
        // Per-unit collision scoping must not turn into per-unit *merging*: two sibling modules
        // importing the same declaration each see a free name in their own table, so only the
        // program-wide qualified-identity guard keeps the declaration from landing twice.
        let models = module(
            "models.noe",
            "namespace App.Models;\npub class User { id: int }\n",
        );
        let a = module(
            "a.noe",
            "namespace App.A;\nuse App.Models.User;\npub fn one(u: User): int { u.id }\n",
        );
        let b = module(
            "b.noe",
            "namespace App.B;\nuse App.Models.User;\npub fn two(u: User): int { u.id }\n",
        );
        let entry = "use App.A.one;\nuse App.B.two;\n";
        let linked = link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[models, a, b],
        )
        .unwrap();
        let users = linked
            .program
            .stmts
            .iter()
            .filter(|s| matches!(s, Stmt::Class(c) if leaf(c.name.as_str()) == "User"))
            .count();
        assert_eq!(users, 1, "`User` must be merged exactly once");
    }

    #[test]
    fn a_module_importing_the_entrys_own_declaration_does_not_duplicate_it() {
        // The entry declares `Config` in its own namespace and a sibling imports it back. The
        // declaration is already the program's tail, so the import must merge nothing — the entry's
        // identities are seeded into the merge-dedup set for exactly this.
        let helper = module(
            "helper.noe",
            "namespace App.Helper;\nuse App.Root.Config;\npub fn read(c: Config): int { c.n }\n",
        );
        let entry = "namespace App.Root;\nuse App.Helper.read;\npub class Config { n: int }\n";
        let linked = link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&helper),
        )
        .unwrap();
        let configs = linked
            .program
            .stmts
            .iter()
            .filter(|s| matches!(s, Stmt::Class(c) if leaf(c.name.as_str()) == "Config"))
            .count();
        assert_eq!(configs, 1, "`Config` must not be merged alongside itself");
    }

    #[test]
    fn a_dependency_modules_own_collision_is_still_e0020_and_blames_its_own_file() {
        // Per-unit scoping is not per-unit silence: two imports of the same local name *inside one
        // dependency file* remain the clash, and the diagnostic must render against that file — not
        // the entry, whose text does not contain the span at all.
        let dep = DepPackage {
            key: "pkg".to_string(),
            root: "pkg".to_string(),
            modules: vec![
                module("a.noe", "namespace pkg.a;\npub class User { id: int }\n"),
                module("b.noe", "namespace pkg.b;\npub class User { n: int }\n"),
                module(
                    "c.noe",
                    "namespace pkg.c;\nuse pkg.a.User;\nuse pkg.b.User;\npub fn go(): int { 1 }\n",
                ),
            ],
            dep_renames: Default::default(),
            native: false,
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
        };
        let errs = link_with_deps(
            "main.noe",
            "use pkg.c.go;\n",
            noeta_lexer::Edition::DEFAULT,
            &[],
            std::slice::from_ref(&dep),
        )
        .unwrap_err();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].diagnostic.code, DiagnosticCode::NameCollision);
        assert_eq!(
            errs[0].source.name(),
            "c.noe",
            "the diagnostic must render against the file its span indexes"
        );
        // And the span really does index that file — the rendered slice is the imported name.
        assert_eq!(errs[0].source.slice(errs[0].diagnostic.span), "User");
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
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
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
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
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
    fn linking_records_which_package_each_source_came_from() {
        // Package provenance is the thing linking otherwise destroys: the merged program is one
        // flat statement list, and nothing in it says which package a declaration arrived from.
        // The `packages` side-table is the only carrier — the entry and its siblings are the root
        // package, each dependency's modules that package's global key — and the checker's orphan
        // rule (E0070) reads it through each declaration's span.
        let dep = DepPackage {
            key: "geo".to_string(),
            root: "shapes".to_string(),
            modules: vec![module(
                "circle.noe",
                "namespace shapes.circle;\npub fn area(r: float): float { return r * r; }\n",
            )],
            dep_renames: Default::default(),
            native: false,
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
        };
        let sibling = module(
            "lib.noe",
            "namespace App.Lib;\npub fn two(): int { return 2; }\n",
        );
        let linked = link_with_deps(
            "main.noe",
            "use geo.circle.area;\nuse App.Lib.two;\necho two();\n",
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&sibling),
            std::slice::from_ref(&dep),
        )
        .unwrap();
        assert_eq!(
            linked.packages.source_package(SourceId(0)),
            Some(&PackageOrigin::Root),
            "the entry is the root package"
        );
        assert_eq!(
            linked.packages.source_package(SourceId(1)),
            Some(&PackageOrigin::Root),
            "a sibling module is the SAME package as the entry — the orphan rule\'s boundary is \
             the package, not the file"
        );
        assert_eq!(
            linked.packages.source_package(SourceId(2)),
            Some(&PackageOrigin::Dependency("geo".to_string())),
            "a dependency module carries its package\'s global key (not its own root segment)"
        );
        // Never guessed: a source the loader did not read is unknown, not "the root package".
        assert_eq!(linked.packages.source_package(SourceId(9)), None);
    }

    #[test]
    fn a_deps_free_link_puts_every_source_in_the_root_package() {
        let sibling = module(
            "lib.noe",
            "namespace App.Lib;\npub fn two(): int { return 2; }\n",
        );
        let linked = link(
            "main.noe",
            "use App.Lib.two;\necho two();\n",
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&sibling),
        )
        .unwrap();
        assert_eq!(
            linked.packages.source_package(SourceId(0)),
            Some(&PackageOrigin::Root)
        );
        assert_eq!(
            linked.packages.source_package(SourceId(1)),
            Some(&PackageOrigin::Root)
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
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
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

    /// A `@derive(<ImportedTrait>)` names a type, so the linker must qualify it like any other
    /// type reference. It did not: a declaration's `#[...]` attributes qualified, its `impl` blocks
    /// qualified, a `trait` declaration's own name qualified — but a directive's payload was never
    /// walked. The trait therefore registered under `App.Shapes.Describable` while the derive still
    /// said `Describable`, so deriving an imported trait failed with "unknown trait". Writing the
    /// qualified name instead was no escape: `@derive` takes a bare trait name.
    #[test]
    fn qualifies_the_trait_named_by_an_imported_derive() {
        let shapes = module(
            "shapes.noe",
            "namespace App.Shapes;\npub trait Describable {\n  fn describe(): string { return \"a thing\" }\n}\n",
        );
        let entry = "namespace App.Main;\nuse App.Shapes.Describable;\n@derive(Describable)\nstruct Point { x: int }\n";
        let linked = link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&shapes),
        )
        .unwrap();
        let derived: Vec<&str> = linked
            .program
            .stmts
            .iter()
            .filter_map(|s| match s {
                Stmt::Struct(d) => Some(d),
                _ => None,
            })
            .flat_map(|d| d.decorators.derives.iter())
            .map(|spec| spec.name.as_str())
            .collect();
        assert_eq!(
            derived,
            vec!["App.Shapes.Describable"],
            "the derive must name the trait's qualified identity, as the merged declaration does"
        );
        assert!(
            linked
                .program
                .stmts
                .iter()
                .any(|s| matches!(s, Stmt::Trait(t) if t.name == "App.Shapes.Describable")),
            "and that identity is what the trait declaration merged under"
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
            lit.type_name.clone().expect("named literal").to_string()
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
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
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
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
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
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
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

    /// D: a **standalone `impl`** that travels with its target type drags in what its method
    /// bodies reference. A standalone impl has no import name, so it is merged by the coherence
    /// pass (it must accompany its type), not by a `use` — and that pass, unlike the `use`-driven
    /// merge, once forgot to walk the impl's bodies. So an internal helper called only from inside
    /// `impl Trait for T { … }` was left out of the merged program, and every consumer of `T`
    /// failed E0005 on a body the package itself compiled cleanly — a hole invisible until the
    /// package boundary was crossed.
    #[test]
    fn a_standalone_impl_drags_in_what_its_bodies_reference() {
        let dep = DepPackage {
            key: "svc".to_string(),
            root: "svc".to_string(),
            modules: vec![module(
                "lib.noe",
                "namespace svc.lib;\n\
                 fn helper(): string { return \"H\"; }\n\
                 pub trait Shape { fn go(): string }\n\
                 pub struct S {\n  tag: string\n  fn new(): S { return S { tag: \"s\" }; }\n}\n\
                 impl Shape for S {\n  fn go(): string { return helper() ~ self.tag; }\n}\n",
            )],
            dep_renames: Default::default(),
            native: false,
            edition: noeta_lexer::Edition::DEFAULT,
            directives: Default::default(),
        };
        let entry = "use svc.lib.{Shape, S};\necho S.new().go();\n";
        let linked = link_with_deps(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            &[],
            std::slice::from_ref(&dep),
        )
        .unwrap();
        assert!(has_struct(&linked, "S"), "the imported type is merged");
        assert!(
            has_fn(&linked, "helper"),
            "the helper called only from the standalone impl's body must travel with it"
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

    // --- a sibling module's extension imports ---------------------------------------------------

    /// The `use std.…` of every retained import in `linked`, as `path.name` strings.
    fn retained_uses(linked: &Linked) -> Vec<String> {
        linked
            .program
            .stmts
            .iter()
            .filter_map(|s| match s {
                Stmt::Use { path, names, .. } => Some(
                    names
                        .iter()
                        .map(|n| format!("{}.{}", path.join("."), n.name))
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn a_contributing_siblings_std_import_is_carried_into_the_merged_program() {
        // A `use` is file-scoped, so a sibling that imports an extension module must get it bound
        // in the merged program. Before siblings drove their own imports, `twice`'s `math.round(…)`
        // reached a merged program with no `math` binding and the entry failed with E0005 "cannot
        // find `math` in this scope" — pointing at a sibling line that checks clean on its own. In
        // effect a non-entry module could not use the standard library at all.
        let sibling = module(
            "helper.noe",
            "namespace Demo.Helper;\n             use std.math\n             pub fn twice(v: float): int { return math.round(v * 2.0); }\n",
        );
        let linked = link(
            "main.noe",
            "use Demo.Helper.twice\necho twice(2.5);\n",
            noeta_lexer::Edition::default(),
            &[sibling],
        )
        .expect("links");
        assert!(has_fn(&linked, "twice"), "the declaration merges");
        assert!(
            retained_uses(&linked).contains(&"std.math".to_string()),
            "the sibling's `use std.math` must ride along: {:?}",
            retained_uses(&linked)
        );
    }

    // --- a data attribute is a link root -------------------------------------------------------
    //
    // `#[...]` exists so something that never names a declaration can still find it. An annotated
    // declaration therefore belongs to the program whether or not a `use` reaches it — otherwise
    // `attributes_of` / `roles_of` see only what the entry happened to import, and the registration
    // mechanism cannot see its own registrations.

    /// The module every root test links against: three `#[Marked]` functions (one imported, one
    /// exported-but-unreferenced, one module-private) plus an unannotated, unreferenced helper.
    fn marked_module() -> RawModule {
        module(
            "tools.noe",
            "namespace app.tools;\n\
             @attribute(Function)\n@role(Semantic.TrustBoundary)\npub struct Marked { name: string }\n\
             #[Marked(\"a\")]\npub fn imported(): string { return \"a\"; }\n\
             #[Marked(\"b\")]\npub fn unimported(): string { return \"b\"; }\n\
             #[Marked(\"c\")]\nfn hidden(): string { return \"c\"; }\n\
             pub fn unannotated(): string { return \"d\"; }\n",
        )
    }

    #[test]
    fn an_annotated_sibling_declaration_links_without_an_import() {
        let linked = link(
            "main.noe",
            "use app.tools.{Marked, imported}\necho imported();\n",
            noeta_lexer::Edition::default(),
            &[marked_module()],
        )
        .expect("links");
        assert!(has_fn(&linked, "imported"), "the imported one merges");
        assert!(
            has_fn(&linked, "unimported"),
            "an exported-but-unreferenced annotated fn is a root"
        );
        assert!(
            has_fn(&linked, "hidden"),
            "visibility does not gate the rule — a `#[Marked]` non-`pub` fn is a registration"
        );
    }

    /// The rule is scoped to the *annotation*, not to the file: an unannotated declaration nothing
    /// references stays out, so this is not "compile the whole directory".
    #[test]
    fn an_unannotated_unreferenced_sibling_declaration_stays_out() {
        let linked = link(
            "main.noe",
            "use app.tools.{Marked, imported}\necho imported();\n",
            noeta_lexer::Edition::default(),
            &[marked_module()],
        )
        .expect("links");
        assert!(
            !has_fn(&linked, "unannotated"),
            "an unannotated, unreferenced declaration must not be dragged in"
        );
    }

    /// A module the entry never imports *at all* still contributes its annotated declarations —
    /// the case a `#[Tool]`-scanning framework depends on, where no file references the tools.
    #[test]
    fn an_entirely_unimported_module_contributes_its_annotated_declarations() {
        let linked = link(
            "main.noe",
            "echo 1;\n",
            noeta_lexer::Edition::default(),
            &[marked_module()],
        )
        .expect("links");
        assert!(has_fn(&linked, "imported"));
        assert!(has_fn(&linked, "unimported"));
        assert!(has_fn(&linked, "hidden"));
        assert!(
            has_struct(&linked, "Marked"),
            "the attribute struct rides in on the closure, so the manifest can materialize it"
        );
        assert!(!has_fn(&linked, "unannotated"));
    }

    /// An annotated declaration is merged with the same **closure** an imported one gets, so the
    /// helper it calls travels with it and the merged program still compiles.
    #[test]
    fn an_annotated_root_drags_in_its_same_module_helper() {
        let sibling = module(
            "tools.noe",
            "namespace app.tools;\n\
             @attribute(Function)\npub struct Marked { name: string }\n\
             #[Marked(\"a\")]\npub fn tool(): string { return shout(); }\n\
             fn shout(): string { return \"hi\"; }\n",
        );
        let linked = link(
            "main.noe",
            "echo 1;\n",
            noeta_lexer::Edition::default(),
            &[sibling],
        )
        .expect("links");
        assert!(has_fn(&linked, "tool"));
        assert!(
            has_fn(&linked, "shout"),
            "the annotated root's internal helper must ride along"
        );
    }

    /// The entry declares a namespace, so it is a resolution candidate *and* a `module_views`
    /// member — its own annotated declarations must not be merged a second time.
    #[test]
    fn a_namespaced_entrys_own_annotated_declaration_merges_once() {
        let linked = link(
            "main.noe",
            "namespace app.main;\n\
             @attribute(Function)\nstruct Marked { name: string }\n\
             #[Marked(\"a\")]\nfn tool(): string { return \"a\"; }\n\
             echo tool();\n",
            noeta_lexer::Edition::default(),
            &[],
        )
        .expect("links");
        let tools = linked
            .program
            .stmts
            .iter()
            .filter(|s| matches!(s, Stmt::Fn(f) if leaf(f.name.as_str()) == "tool"))
            .count();
        assert_eq!(tools, 1, "the entry's own declaration is not duplicated");
    }

    // --- a tier block's own `use`s (block-scoped qualification) --------------------------------

    /// The attribute names on the first `Stmt::Fn` inside the program's first `@<tier>` block.
    fn block_fn_attrs(linked: &Linked) -> Vec<String> {
        linked
            .program
            .stmts
            .iter()
            .find_map(|s| match s {
                Stmt::TierBlock { items, .. } => Some(items),
                _ => None,
            })
            .expect("a tier block")
            .iter()
            .find_map(|s| match s {
                Stmt::Fn(decl) => Some(decl.attrs.iter().map(|a| a.name.to_string()).collect()),
                _ => None,
            })
            .expect("a fn inside the block")
    }

    /// The attribute names on the first *top-level* `Stmt::Fn`.
    fn top_fn_attrs(linked: &Linked) -> Vec<String> {
        linked
            .program
            .stmts
            .iter()
            .find_map(|s| match s {
                Stmt::Fn(decl) => Some(decl.attrs.iter().map(|a| a.name.to_string()).collect()),
                _ => None,
            })
            .expect("a top-level fn")
    }

    #[test]
    fn a_tier_blocks_own_use_qualifies_references_inside_the_block() {
        // The `use` sits *inside* the `@test` block, so it only reaches the top-level statement
        // stream once the tier activates — after the linker has run. The qualifier must still see
        // it, or `#[Skip(…)]` stays the bare `Skip` that neither the checker's attribute table nor
        // the test runner (which matches the qualified `std.test.Skip`) recognizes, and the skip
        // silently evaporates.
        let linked = link(
            "main.noe",
            "@test {\n  use std.test.{Skip}\n  #[Skip(\"later\")]\n  fn f(text: string): string { return text; }\n}\necho 1;\n",
            noeta_lexer::Edition::default(),
            &[],
        )
        .expect("links");
        assert_eq!(
            block_fn_attrs(&linked),
            vec![noeta_ast::reflect::TEST_ATTR_SKIP.to_string()]
        );
    }

    #[test]
    fn a_tier_blocks_use_does_not_qualify_references_outside_it() {
        // The overlay is scoped to the block: the same name on a top-level fn is untouched, so it
        // still resolves to nothing and still earns its "cannot be used as an attribute" error.
        let linked = link(
            "main.noe",
            "@test {\n  use std.test.{Skip}\n  fn inside(): void { }\n}\n#[Skip(\"outside\")]\nfn outside(): void { }\necho 1;\n",
            noeta_lexer::Edition::default(),
            &[],
        )
        .expect("links");
        assert_eq!(top_fn_attrs(&linked), vec!["Skip".to_string()]);
    }

    #[test]
    fn a_tier_blocks_use_is_not_hoisted_out_of_the_block() {
        // The overlay is a *rewrite table*, not an import: the `use` statement itself stays inside
        // the block, so a build with the tier inactive drops it with the block and no import is
        // left dangling in the merged program.
        let linked = link(
            "main.noe",
            "@test {\n  use std.test.{Skip}\n  #[Skip(\"later\")]\n  fn f(text: string): string { return text; }\n}\necho 1;\n",
            noeta_lexer::Edition::default(),
            &[],
        )
        .expect("links");
        assert!(
            !linked
                .program
                .stmts
                .iter()
                .any(|s| matches!(s, Stmt::Use { .. })),
            "no top-level `use` may appear: {:?}",
            linked.program.stmts
        );
    }

    /// A sibling module exporting the type a tier block imports — the `.noe` half of the repro that
    /// qualification alone could not fix (a std import inside a block always worked, because an
    /// extension module resolves through the registry and never needs the unit graph).
    fn side_module() -> RawModule {
        module(
            "side.noe",
            "namespace probe.lib.side;\npub struct Thing { n: int }\npub fn make(): int { return 3 }\n",
        )
    }

    #[test]
    fn a_tier_blocks_use_links_a_loaded_module() {
        // Qualifying a name is not linking it: the block-scope overlay rewrote `Thing` to
        // `probe.lib.side.Thing`, but nothing merged the declaration, so `noeta check` reported
        // nothing and `noeta test` failed with "cannot find type `probe.lib.side.Thing` in this
        // scope". The block's `use` must drive the merge exactly as a top-level one does.
        let linked = link(
            "main.noe",
            "@test {\n  use probe.lib.side.{Thing}\n  fn t(): void { x = Thing { n: 3 } }\n}\necho 1;\n",
            noeta_lexer::Edition::default(),
            &[side_module()],
        )
        .expect("links");
        assert!(
            has_struct(&linked, "Thing"),
            "the block-imported declaration must be in the merged program: {:?}",
            linked.program.stmts
        );
    }

    #[test]
    fn a_tier_blocks_whole_module_use_links_the_module() {
        // The second import form (`use probe.lib.side` + `side.Thing`) merges every `pub`
        // declaration, and failed identically before the fix.
        let linked = link(
            "main.noe",
            "@test {\n  use probe.lib.side\n  fn t(): void { x = side.Thing { n: side.make() } }\n}\necho 1;\n",
            noeta_lexer::Edition::default(),
            &[side_module()],
        )
        .expect("links");
        assert!(has_struct(&linked, "Thing"), "the module's type merges");
        assert!(has_fn(&linked, "make"), "the module's fn merges");
    }

    #[test]
    fn a_tier_blocks_use_of_a_module_is_still_not_hoisted() {
        // Linking the module must not hoist the import: the `use` stays inside the block, so an
        // inactive build drops it with the block. The merged declaration is harmless there — it
        // carries a qualified identity, so it binds no short name and nothing references it.
        let linked = link(
            "main.noe",
            "@test {\n  use probe.lib.side.{Thing}\n  fn t(): void { x = Thing { n: 3 } }\n}\necho 1;\n",
            noeta_lexer::Edition::default(),
            &[side_module()],
        )
        .expect("links");
        assert!(
            !linked
                .program
                .stmts
                .iter()
                .any(|s| matches!(s, Stmt::Use { .. })),
            "no top-level `use` may appear: {:?}",
            linked.program.stmts
        );
    }

    #[test]
    fn a_renamed_text_tier_captures_verbatim_per_package() {
        // 3g at the loader seam: a root package binds std's `doc` **text** tier under a local `@docs`
        // (`PackageUses`: Root → `docs` = provider `std`, exported `doc`). The body is a hard lex error
        // if tokenized as code — a bare `"` opens an unterminated string — so it links cleanly ONLY if
        // `docs` reached the lexer's text-tier set for the Root package. The control (no binding) fails.
        noeta_stdlib::registry::default_seeded();
        let src = "@docs {\n# Heading with a bare \" quote and <angle> bits\n}\n\
                   fn add(a: int, b: int): int { return a + b }\n";

        let mut uses = noeta_span::PackageUses::new();
        uses.set(
            PackageOrigin::Root,
            "docs".to_string(),
            noeta_span::PackageUse {
                provider_roots: vec!["std".to_string()],
                exported: "doc".to_string(),
            },
        );
        super::link_with_deps(
            "main.noe",
            src,
            noeta_lexer::Edition::default(),
            &[],
            &[],
            &uses,
            ModulePath::Declared,
        )
        .expect("a renamed text tier captures verbatim and links");

        // Control: with no binding, `docs` is unknown (only the bare `doc` is a default text tier), so
        // `@docs { … }` tokenizes as code and the bare quote is a lex error — the fix is load-bearing.
        let errs = super::link_with_deps(
            "main.noe",
            src,
            noeta_lexer::Edition::default(),
            &[],
            &[],
            &noeta_span::PackageUses::new(),
            ModulePath::Declared,
        )
        .expect_err("without the binding, the markdown body is lexed as code and fails");
        assert!(!errs.is_empty());
    }

    /// Whether the merged program declares a top-level value binding `name`.
    fn has_binding(linked: &Linked, name: &str) -> bool {
        linked
            .program
            .stmts
            .iter()
            .any(|s| matches!(s, Stmt::Binding { name: n, .. } if n == name))
    }

    /// A `use (…)` capture is a reference to a module-level binding, so importing the function has
    /// to bring the binding with it. Without this the import lands in the program and its capture
    /// resolves to nothing — E0005 against a declaration the consumer never wrote, and the only way
    /// to consume the package was to not use captures at all.
    #[test]
    fn an_imported_fn_drags_in_the_binding_it_captures() {
        let sibling = module(
            "lib.noe",
            "namespace app.lib;\n\
             fn seed(): int { return 7; }\n\
             x = seed();\n\
             pub fn reader() use (x): int { return x; }\n",
        );
        let entry = "use app.lib.reader;\nn = reader();\necho n;\n";
        let linked = link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&sibling),
        )
        .expect("links");
        assert!(has_fn(&linked, "reader"));
        assert!(
            has_binding(&linked, "x"),
            "the captured module binding travels with the import: {:?}",
            linked.program.stmts
        );
        assert!(
            has_fn(&linked, "seed"),
            "and the binding's own initializer keeps expanding the closure"
        );
    }

    /// A method's captures count too — the declaration site of a method's `use (…)` is the module
    /// top level, exactly like a free function's.
    #[test]
    fn an_imported_class_drags_in_the_binding_its_method_captures() {
        let sibling = module(
            "lib.noe",
            "namespace app.lib;\n\
             base = 10;\n\
             pub class Counter {\n\
               n: int\n\
               fn new(): Counter { return Counter { n: 0 }; }\n\
               fn total() use (base): int { return self.n + base; }\n\
             }\n",
        );
        let entry = "use app.lib.Counter;\nc = Counter.new();\necho c.total();\n";
        let linked = link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&sibling),
        )
        .expect("links");
        assert!(
            has_binding(&linked, "base"),
            "a method's capture drags its module binding too: {:?}",
            linked.program.stmts
        );
    }

    /// An **attribute-discovered** root (a declaration nothing imports, merged because its `#[…]`
    /// annotation is the reference) reaches the same closure — including through a callee. This is
    /// the shape that made `noeta check .` report errors against a *sibling* file: checking the
    /// module that merely declares the attribute dragged the annotated fn and its helper in, and
    /// left the helper's captured binding behind.
    #[test]
    fn an_attribute_root_drags_in_the_binding_its_callee_captures() {
        let annotated = module(
            "routes.noe",
            "namespace app.routes;\n\
             use app.attrs.Mark;\n\
             x = 3;\n\
             fn helper() use (x): int { return x; }\n\
             #[Mark(\"/cap\")]\n\
             fn handler(): int { return helper(); }\n",
        );
        let entry = "namespace app.attrs;\n@attribute\npub struct Mark { path: string }\n";
        let linked = link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&annotated),
        )
        .expect("links");
        assert!(has_fn(&linked, "handler"), "the annotated root is merged");
        assert!(has_fn(&linked, "helper"), "and its callee");
        assert!(
            has_binding(&linked, "x"),
            "and the callee's captured binding: {:?}",
            linked.program.stmts
        );
    }

    /// A binding is merged from the **capture** seed only, never from the reference seed. A
    /// parameter that shares a module binding's name is not a reference to it, and merging a
    /// binding is not inert — it runs its initializer in the consumer's program.
    #[test]
    fn a_parameter_sharing_a_binding_name_does_not_drag_the_binding() {
        let sibling = module(
            "lib.noe",
            "namespace app.lib;\n\
             limit = 99;\n\
             pub fn clamp(limit: int): int { return limit; }\n",
        );
        let entry = "use app.lib.clamp;\necho clamp(3);\n";
        let linked = link(
            "main.noe",
            entry,
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&sibling),
        )
        .expect("links");
        assert!(has_fn(&linked, "clamp"));
        assert!(
            !has_binding(&linked, "limit"),
            "the module binding is not dragged in by a same-named parameter: {:?}",
            linked.program.stmts
        );
    }

    /// A merged binding keeps its own name, so two modules needing same-named bindings is a real
    /// ambiguity — named, not silently resolved in favour of whichever merged last.
    #[test]
    fn two_modules_needing_a_same_named_binding_collide() {
        let a = module(
            "a.noe",
            "namespace app.a;\n\
             shared = 1;\n\
             pub fn from_a() use (shared): int { return shared; }\n",
        );
        let b = module(
            "b.noe",
            "namespace app.b;\n\
             shared = 2;\n\
             pub fn from_b() use (shared): int { return shared; }\n",
        );
        let entry = "use app.a.from_a;\nuse app.b.from_b;\necho from_a() + from_b();\n";
        let errors = link("main.noe", entry, noeta_lexer::Edition::DEFAULT, &[a, b])
            .expect_err("the two `shared` bindings cannot both be `shared`");
        assert!(
            errors
                .iter()
                .any(|e| e.diagnostic.message.contains("shared")
                    && e.diagnostic.message.contains("app.a")
                    && e.diagnostic.message.contains("app.b")),
            "the error names the binding and both modules: {:?}",
            errors
                .iter()
                .map(|e| e.diagnostic.message.clone())
                .collect::<Vec<_>>()
        );
    }
}
