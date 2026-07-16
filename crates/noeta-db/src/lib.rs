//! The query graph: the compile pipeline as a [salsa] database.
//!
//! M1.1 threads the existing straight-line pipeline (lex → parse → compile) through salsa
//! **before** the type checker (M1.7) needs it, so later slices edit a graph rather than
//! rewrite a pipeline. This slice is deliberately *behavior-preserving*: every query is a
//! thin wrapper that calls the existing `noeta_lexer::lex` / `noeta_parser::parse` /
//! `noeta_compiler::compile` function and memoizes the result. The differential oracle proves
//! the wrap changes nothing — the VM still reproduces the tree-walker byte-for-byte.
//!
//! ```text
//!   SourceProgram (input)
//!        │
//!        ▼
//!     tokens(db)  ──►  ast(db)  ──►  checked(db)   ──►  bytecode(db)
//!     (noeta-lexer)    (noeta-parser)  (noeta-check)       (noeta-compiler)
//! ```
//!
//! The checker query (`checked`, added in M1.7) slotted in between [`ast`] and [`bytecode`]
//! with no re-threading — exactly what landing the plumbing early bought.
//!
//! ## Foreign results and `Update`
//!
//! salsa memoizes a tracked function's return value and needs it to implement
//! [`salsa::Update`]. Our artifacts — `Lexed`, `Parsed`, `Module` — live in upstream crates,
//! so we cannot implement the trait on them directly (orphan rule). Each is wrapped in a
//! local newtype ([`Tokens`], [`Ast`], [`Bytecode`]) with an always-replace `Update` impl
//! (see the `replace_update!` macro). Always-replace is sound: it unconditionally overwrites
//! the slot and reports "changed", so salsa never serves a stale value. It forgoes
//! *backdating* (a re-lex/parse/compile always re-runs dependents) — a precision trade-off,
//! never a correctness one, and exactly right for pass-through plumbing.

use noeta_ast::Program;
use noeta_bytecode::Module;
use noeta_compiler::Unsupported;
use noeta_diagnostics::Diagnostic;
use noeta_lexer::Lexed;
use noeta_parser::Parsed;
use noeta_span::{Source, SourceId, Span};

/// Re-export of the language [`Edition`](noeta_lexer::Edition) (and its `EditionMap`) so a crate that
/// feeds the db — building [`SourceProgram`]/[`Workspace`] inputs — can name the edition to thread
/// without a separate `noeta-lexer` dependency.
pub use noeta_lexer::{Edition, EditionMap};

/// The salsa database for the compile pipeline. Construct with `LangDatabase::default()`.
#[salsa::db]
#[derive(Default, Clone)]
pub struct LangDatabase {
    storage: salsa::Storage<Self>,
}

#[salsa::db]
impl salsa::Database for LangDatabase {}

impl std::fmt::Debug for LangDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LangDatabase").finish_non_exhaustive()
    }
}

/// The one input: a source file (its id, name, and text). Everything downstream is a query
/// derived from this. Mutating an input (a future incremental-edit / LSP concern) invalidates
/// exactly the queries that read the changed field.
#[salsa::input(debug)]
pub struct SourceProgram {
    pub id: u32,
    #[returns(ref)]
    pub name: String,
    #[returns(ref)]
    pub text: String,
    /// The language [`Edition`](noeta_lexer::Edition) this source is written against, in canonical
    /// string form (`"2026"`) — its package's edition (editions arc). Every db query that
    /// lexes/parses/checks this source does so under it, so the IDE stack honors a future edition's
    /// grammar/rules exactly as the batch compiler does. A string (not the enum) because a salsa
    /// input field must be `Update`, which the leaf `Edition` enum does not implement; the queries
    /// parse it back with [`edition_of`]. Editing it invalidates exactly the queries that read it.
    #[returns(ref)]
    pub edition: String,
}

/// Build (or rebuild) the [`SourceProgram`] input from a [`Source`] and the language edition its
/// package is written against.
pub fn source_program(
    db: &LangDatabase,
    source: &Source,
    edition: noeta_lexer::Edition,
) -> SourceProgram {
    SourceProgram::new(
        db,
        source.id().0,
        source.name().to_string(),
        source.text().to_string(),
        edition.as_str().to_string(),
    )
}

/// The language edition a [`SourceProgram`] declares, parsed from its canonical string form back to
/// the enum. An unrecognised value (only reachable if a caller stored a non-canonical string) falls
/// back to the default edition rather than failing a query.
fn edition_of(db: &dyn salsa::Database, src: SourceProgram) -> noeta_lexer::Edition {
    noeta_lexer::Edition::parse(src.edition(db)).unwrap_or_default()
}

/// A one-source [`EditionMap`](noeta_lexer::EditionMap) for the single-file query family: the source
/// governs itself under its own edition, everything else defaults. The workspace family builds a
/// multi-source map in [`workspace_editions`] instead.
fn source_edition_map(db: &dyn salsa::Database, src: SourceProgram) -> noeta_lexer::EditionMap {
    let mut map = noeta_lexer::EditionMap::new();
    map.set(SourceId(src.id(db)), edition_of(db, src));
    map
}

/// Reconstruct a [`Source`] from the input fields (cheap; recomputes line starts).
fn source_of(db: &dyn salsa::Database, src: SourceProgram) -> Source {
    Source::new(
        SourceId(src.id(db)),
        src.name(db).clone(),
        src.text(db).clone(),
    )
}

/// Lexer output, wrapped so salsa can memoize it. See the module docs on `Update`.
#[derive(Debug, Clone)]
pub struct Tokens(pub Lexed);

/// Parser output (AST + diagnostics), wrapped for salsa.
#[derive(Debug, Clone)]
pub struct Ast(pub Parsed);

/// Type-checker output: the checker's diagnostics (empty ⇒ well-typed) **and** its compile-input
/// [`Sites`] bundle, both produced by one `noeta_check::check_all` run. Carrying the bundle here
/// lets the [`bytecode`] query and the eval path read it from this memoized query instead of each
/// re-running the checker (the redundant-passes dedup — the bundle is a pure function of the AST).
///
/// [`Sites`]: noeta_check::Sites
#[derive(Debug, Clone)]
pub struct Checked {
    pub diagnostics: Vec<Diagnostic>,
    /// Every expression's inferred type, keyed by span — the IDE hover index. Empty except in the
    /// result of the [`checked_ide`] query (the LSP path); the compile-path [`checked`] query leaves
    /// it empty so `noeta run`/differential pay nothing for it.
    pub expr_types: std::collections::HashMap<Span, noeta_ast::reflect::TypeRepr>,
    /// The checker's compile-input bundle (every span-keyed codegen hint + destructor relevance),
    /// consumed as a unit by the compiler and the eval reference. (The T2 `Sites` bundling subsumed
    /// main's flat per-map fields, including the noeta-native `TypeRecipe` rename — the bundle's
    /// field types live in `noeta_check::Sites` and follow that rename through the re-export.)
    pub sites: noeta_check::Sites,
    /// Method-bundle bindings by target type (kernel-methods K4) — what member completion reads
    /// to offer bound methods.
    pub bundle_bindings: std::collections::HashMap<String, Vec<(String, String)>>,
    /// Every `@packed` struct's flat layout by type name — the IDE storage-fact index hover and
    /// inlay hints read (see [`noeta_check::Checked::packed_layouts`]).
    pub packed_layouts: std::collections::HashMap<String, noeta_ast::reflect::PackedLayout>,
}

/// Compiler output: a [`Module`], or the first construct outside the VM's subset.
#[derive(Debug, Clone)]
pub struct Bytecode(pub Result<Module, Unsupported>);

/// Linker output (M1.9.3): the merged [`Program`] of an entry and its resolved imports, or the
/// `use`-resolution diagnostics (entry parse errors, E0019, E0020). The whole-workspace analogue
/// of [`Ast`] — what [`linked_checked`] and [`linked_bytecode`] build on.
#[derive(Debug, Clone)]
pub struct LinkedProgram(pub Result<Program, Vec<Diagnostic>>);

/// Give a foreign-result newtype the two traits salsa needs for a memoized output, both in
/// the conservative "always changed" direction:
///
/// - [`PartialEq`] — salsa compares old vs. new output to decide whether to *backdate* (treat
///   it as unchanged). We return `false` unconditionally, so salsa never backdates: dependents
///   re-run whenever this query re-executes. (The inner `Lexed`/`Parsed`/`Module` are not
///   `PartialEq`, and a structural compare buys nothing for pass-through plumbing.) The impl is
///   intentionally non-reflexive — it exists only to gate backdating, never as a value compare.
/// - [`salsa::Update`] — overwrites the slot in place and reports "changed". The crate's only
///   `unsafe`; trivially sound.
macro_rules! replace_update {
    ($ty:ty) => {
        impl PartialEq for $ty {
            fn eq(&self, _other: &Self) -> bool {
                false
            }
        }

        // SAFETY: `old_pointer` points at an initialized `$ty` (salsa only calls
        // `maybe_update` when a previous value exists). We overwrite it with `new_value`
        // (dropping the old value in place) and report that it changed — a valid `Update`.
        unsafe impl salsa::Update for $ty {
            unsafe fn maybe_update(old_pointer: *mut Self, new_value: Self) -> bool {
                unsafe {
                    *old_pointer = new_value;
                }
                true
            }
        }
    };
}

replace_update!(Tokens);
replace_update!(Ast);
replace_update!(Checked);
replace_update!(Bytecode);
replace_update!(LinkedProgram);

/// The **extension environment** as an explicit salsa input (singleton): the installed
/// extensions' verbatim-body tier names, which [`ast`] and [`workspace_text_tiers`] fold into
/// lexing. Previously these were read straight off the process-global registry inside tracked
/// queries — a hidden non-salsa input, sound only while the global never changes after first
/// query, and a silent-staleness landmine for an embedder pairing a `LangDatabase` with a
/// per-session registry. Constructors that know their registry seed this via [`seed_ext_env`];
/// an unseeded db falls back to the global default (the documented single-registry stance for
/// the CLI tools), and the fallback is *recorded on this input* so it stays one dependency.
#[salsa::input(singleton, debug)]
pub struct ExtEnv {
    #[returns(ref)]
    pub verbatim_tier_names: Vec<String>,
}

/// Create (or overwrite) the db's [`ExtEnv`] from an explicit tier-name set. What an embedder
/// with a per-session registry calls after assembling it.
pub fn seed_ext_env(db: &mut dyn salsa::Database, verbatim_tier_names: Vec<String>) {
    use salsa::Setter as _;
    match ExtEnv::try_get(db) {
        Some(env) => {
            env.set_verbatim_tier_names(db).to(verbatim_tier_names);
        }
        None => {
            ExtEnv::new(db, verbatim_tier_names);
        }
    }
}

/// The extension verbatim-tier names for `db`: the seeded [`ExtEnv`], or — first read of an
/// unseeded db — the process-global default registry's, captured onto the input so later reads
/// depend on salsa state, not the global.
fn ext_verbatim_tier_names(db: &dyn salsa::Database) -> Vec<String> {
    match ExtEnv::try_get(db) {
        Some(env) => env.verbatim_tier_names(db).clone(),
        None => noeta_stdlib::registry::ext_verbatim_tier_names()
            .into_iter()
            .map(str::to_string)
            .collect(),
    }
}

/// Tokenize the source. Memoized; re-runs only when `SourceProgram::text` changes.
#[salsa::tracked(returns(ref))]
pub fn tokens(db: &dyn salsa::Database, src: SourceProgram) -> Tokens {
    let source = source_of(db, src);
    Tokens(noeta_lexer::lex_in(
        &source,
        edition_of(db, src),
        &noeta_lexer::TextTiers::default(),
    ))
}

/// Parse the token stream into an AST. Depends on [`tokens`].
#[salsa::tracked(returns(ref))]
pub fn ast(db: &dyn salsa::Database, src: SourceProgram) -> Ast {
    let source = source_of(db, src);
    let toks = tokens(db, src);
    // The file's own verbatim-body tiers plus the installed extensions', so a `${…}` hole with a
    // nested `@html { … }` (an inline loop body) re-lexes its body verbatim.
    let mut names = toks.0.text_tier_decls.clone();
    names.extend(ext_verbatim_tier_names(db));
    let set = noeta_lexer::TextTiers::with(names);
    Ast(noeta_parser::parse_in(
        &source,
        &toks.0.tokens,
        edition_of(db, src),
        &set,
    ))
}

/// Type-check the AST and return the checker's diagnostics. Depends on [`ast`]. The pipeline's
/// front-end gate: a program with type errors is rejected before either backend runs (so both
/// backends surface the identical compile-time result — see the conformance differential).
#[salsa::tracked(returns(ref))]
pub fn checked(db: &dyn salsa::Database, src: SourceProgram) -> Checked {
    let parsed = ast(db, src);
    from_check_output(noeta_check::check_all_with_editions(
        &parsed.0.program,
        source_edition_map(db, src),
    ))
}

/// The IDE-flavored type-check: like [`checked`], but the result's [`Checked::expr_types`] is
/// populated (the span→type hover index), because it runs the checker via
/// [`noeta_check::check_all_with_types`]. The LSP reads diagnostics *and* hover types from this one
/// query — a single checker run per document version — while the compile path stays on [`checked`]
/// and never builds the index. Diagnostics are identical between the two.
#[salsa::tracked(returns(ref))]
pub fn checked_ide(db: &dyn salsa::Database, src: SourceProgram) -> Checked {
    let parsed = ast(db, src);
    from_check_output(noeta_check::check_all_with(
        &parsed.0.program,
        noeta_check::CheckOptions {
            record_expr_types: true,
            editions: source_edition_map(db, src),
            ..noeta_check::CheckOptions::default()
        },
    ))
}

/// Project a `noeta_check` result into this crate's memoized [`Checked`]. Shared by [`checked`] and
/// [`checked_ide`] so the two stay field-for-field in sync.
fn from_check_output(out: noeta_check::Checked) -> Checked {
    Checked {
        diagnostics: out.diagnostics,
        expr_types: out.expr_types,
        sites: out.sites,
        bundle_bindings: out.bundle_bindings,
        packed_layouts: out.packed_layouts,
    }
}

/// Compile the AST to a [`Module`], or report the first unsupported construct. Depends on [`ast`]
/// and [`checked`] — the latter only to reuse its [`Sites`] bundle (which the compiler needs, e.g.
/// to bake full-fidelity `type_of` constants) rather than re-deriving it. Execution is still
/// gated on `checked`'s diagnostics by the caller; reading the bundle here does not couple them
/// semantically, it only avoids a second checker run.
///
/// [`Sites`]: noeta_check::Sites
#[salsa::tracked(returns(ref))]
pub fn bytecode(db: &dyn salsa::Database, src: SourceProgram) -> Bytecode {
    let parsed = ast(db, src);
    let checked = checked(db, src);
    Bytecode(noeta_compiler::compile_with_sites(
        &parsed.0.program,
        checked.sites.clone(),
        false,
        // No debug info on the salsa/IDE bytecode path (the debugger uses its own direct compile).
        false,
    ))
}

// ---------------------------------------------------------------------------
// The module graph (M1.9.3)
// ---------------------------------------------------------------------------
//
// A multi-file program is a [`Workspace`]: one entry `SourceProgram` plus its sibling module
// inputs. The [`linked`] query resolves the entry's `use` declarations against the modules'
// declared namespaces (reusing each source's memoized [`ast`]) and merges the resolved
// declarations into one [`Program`]; [`linked_checked`] and [`linked_bytecode`] are the
// whole-program checker and compiler over that merge — the workspace analogues of [`checked`]
// and [`bytecode`].
//
// ```text
//   Workspace (input: entry + module SourcePrograms)
//        │
//        ├── ast(entry) ─────┐
//        ├── ast(module_1) ──┤
//        ├── ast(module_n) ──┴──►  linked  ──►  linked_checked
//        │                            │
//        │                            └──────►  linked_bytecode
// ```
//
// Resolution lives in [`linked`], so it depends on every module's `ast` — editing a module
// re-links — but the per-source `tokens`/`ast` queries stay independent: editing one module
// never recomputes another's parse. That is the incremental boundary M2's hot reload builds on.

/// A multi-file program: the entry source plus its sibling module sources, each a memoized
/// [`SourceProgram`] input, and — for a package (package-manager P2.1c) — its resolved dependency
/// packages' modules. Mutating any one source invalidates exactly the queries that read it.
#[salsa::input(debug)]
pub struct Workspace {
    pub entry: SourceProgram,
    #[returns(ref)]
    pub modules: Vec<SourceProgram>,
    /// The resolved dependency packages' modules (empty for a lone/sibling-only workspace). Each
    /// carries its re-root info; [`linked`] re-roots and links them as closed units.
    #[returns(ref)]
    pub dep_modules: Vec<DepModule>,
}

/// One dependency package module in a salsa [`Workspace`] (package-manager P2.1c): its source input
/// plus the re-root info [`linked`] applies before merging it (`root`→`key`, then each of the
/// package's local dependency keys → the target package's global segment). `renames` is a **flat**
/// list of `[local0, global0, local1, global1, …]` pairs — a `BTreeMap` is not a salsa input field
/// type, so the query rebuilds it (see [`reroot_map`]).
#[salsa::input(debug)]
pub struct DepModule {
    pub src: SourceProgram,
    #[returns(ref)]
    pub root: String,
    #[returns(ref)]
    pub key: String,
    #[returns(ref)]
    pub renames: Vec<String>,
}

/// A dependency package's sources + re-root info, the ergonomic input to [`workspace_with_deps`]
/// (package-manager P2.1c). Mirrors `noeta_loader::DepPackage` but with already-labeled [`Source`]s.
#[derive(Debug)]
pub struct DepSources {
    pub root: String,
    pub key: String,
    pub renames: Vec<(String, String)>,
    pub modules: Vec<Source>,
    /// This package's language edition (canonical string, e.g. `"2026"`) — its modules are parsed
    /// and checked under it, exactly as the CLI's `load_with_deps` does (editions arc).
    pub edition: String,
}

/// Build a [`Workspace`] input from the entry [`Source`], its sibling module sources (as produced by
/// `noeta_loader::read_workspace`), and the root package's edition. Each source becomes a
/// [`SourceProgram`] under `root_edition`; no dependency packages (use [`workspace_with_deps`]).
pub fn workspace(
    db: &LangDatabase,
    entry: &Source,
    modules: &[Source],
    root_edition: noeta_lexer::Edition,
) -> Workspace {
    let entry_input = source_program(db, entry, root_edition);
    let module_inputs = modules
        .iter()
        .map(|s| source_program(db, s, root_edition))
        .collect();
    Workspace::new(db, entry_input, module_inputs, Vec::new())
}

/// Build a [`Workspace`] that also links **dependency packages** (package-manager P2.1c): the entry
/// and siblings take `root_edition`; each dependency's modules take that package's own edition. Each
/// dep module becomes a [`DepModule`] input carrying its re-root info, so cross-package
/// `use <dep-key>.…` resolves in the salsa graph exactly as in the CLI's `load_with_deps`.
pub fn workspace_with_deps(
    db: &LangDatabase,
    entry: &Source,
    modules: &[Source],
    deps: &[DepSources],
    root_edition: noeta_lexer::Edition,
) -> Workspace {
    let entry_input = source_program(db, entry, root_edition);
    let module_inputs = modules
        .iter()
        .map(|s| source_program(db, s, root_edition))
        .collect();
    let mut dep_inputs = Vec::new();
    for dep in deps {
        let renames = flatten_renames(&dep.renames);
        let dep_edition = noeta_lexer::Edition::parse(&dep.edition).unwrap_or_default();
        for src in &dep.modules {
            let sp = source_program(db, src, dep_edition);
            dep_inputs.push(DepModule::new(
                db,
                sp,
                dep.root.clone(),
                dep.key.clone(),
                renames.clone(),
            ));
        }
    }
    Workspace::new(db, entry_input, module_inputs, dep_inputs)
}

/// Flatten a rename map to the `[local0, global0, …]` pairs a [`DepModule`] stores.
fn flatten_renames(map: &[(String, String)]) -> Vec<String> {
    map.iter()
        .flat_map(|(local, global)| [local.clone(), global.clone()])
        .collect()
}

/// Rebuild the leading-segment rename map from a [`DepModule`]'s flat `renames` pairs.
fn reroot_map(flat: &[String]) -> std::collections::BTreeMap<String, String> {
    flat.chunks_exact(2)
        .map(|pair| (pair[0].clone(), pair[1].clone()))
        .collect()
}

/// Resolve and merge the workspace: the entry's imports against the modules' declared namespaces,
/// producing one merged [`Program`] (or the load diagnostics). Depends on every source's [`ast`]
/// (so editing any module re-links), but not on any cross-module edge — the per-source parse
/// The workspace's declared text-tier names (text-tiers arc): the union of every member file's
/// `@tier(<name>, …, text: "…")` declarations — entry, siblings, and dependency modules alike —
/// sorted and deduped. Derived from the per-file [`tokens`] scans, so an edit that adds or
/// removes a declaration changes this value and invalidates exactly the workspace-aware lexes
/// ([`tokens_in`]); any other edit backdates (the value compares equal) and they stay memoized.
#[salsa::tracked(returns(ref))]
pub fn workspace_text_tiers(db: &dyn salsa::Database, ws: Workspace) -> Vec<String> {
    let mut names: Vec<String> = std::iter::once(ws.entry(db))
        .chain(ws.modules(db).iter().copied())
        .chain(ws.dep_modules(db).iter().map(|dm| dm.src(db)))
        .flat_map(|src| tokens(db, src).0.text_tier_decls.iter().cloned())
        .collect();
    // Plus the installed extensions' verbatim-body tiers (`doc`, native `@json`/`@sql`) — no
    // member file declares these, so the LSP/pipeline must seed them like the loader does. Read
    // through the [`ExtEnv`] input (seeded, or the global default).
    names.extend(ext_verbatim_tier_names(db));
    names.sort();
    names.dedup();
    names
}

/// Workspace-aware tokenization: like [`tokens`], but lexing with the whole workspace's text-tier
/// set ([`workspace_text_tiers`]) — so a text tier declared in one file (or dependency package)
/// captures `@<name> { … }` bodies verbatim in every member. What [`linked`] reads; the per-file
/// [`tokens`] stays the single-file surface (its two-pass covers same-file declarations).
#[salsa::tracked(returns(ref))]
pub fn tokens_in(db: &dyn salsa::Database, ws: Workspace, src: SourceProgram) -> Tokens {
    let set = noeta_lexer::TextTiers::with(workspace_text_tiers(db, ws).iter().cloned());
    let source = source_of(db, src);
    Tokens(noeta_lexer::lex_in(&source, edition_of(db, src), &set))
}

/// Workspace-aware parse over [`tokens_in`] — the [`linked`] pipeline's counterpart of [`ast`].
#[salsa::tracked(returns(ref))]
pub fn ast_in(db: &dyn salsa::Database, ws: Workspace, src: SourceProgram) -> Ast {
    let source = source_of(db, src);
    let toks = tokens_in(db, ws, src);
    // The whole workspace's verbatim-body tier set — the same one `tokens_in` lexed with — so a
    // nested tier body inside a `${…}` hole re-lexes correctly (an inline `@html { … }` loop).
    let set = noeta_lexer::TextTiers::with(workspace_text_tiers(db, ws).iter().cloned());
    Ast(noeta_parser::parse_in(
        &source,
        &toks.0.tokens,
        edition_of(db, src),
        &set,
    ))
}

/// queries remain independent. The merge means both backends run the linked program unchanged, so
/// the differential oracle is preserved by construction.
#[salsa::tracked(returns(ref))]
pub fn linked(db: &dyn salsa::Database, ws: Workspace) -> LinkedProgram {
    let entry_src = ws.entry(db);
    let entry_tokens = tokens_in(db, ws, entry_src);
    let entry_ast = ast_in(db, ws, entry_src);
    // The entry must lex and parse before it can be linked; surface its load diagnostics otherwise
    // (rendered against the entry, like every import error).
    let entry_diags: Vec<Diagnostic> = entry_tokens
        .0
        .diagnostics
        .iter()
        .chain(entry_ast.0.diagnostics.iter())
        .cloned()
        .collect();
    if !entry_diags.is_empty() {
        return LinkedProgram(Err(entry_diags));
    }

    let entry_source = source_of(db, entry_src);
    // Read each module's `ast` (this is what makes `linked` a dependent of every module). Only a
    // cleanly-parsed module contributes; `link_parsed` keeps just the ones declaring a namespace.
    let mut module_programs: Vec<&Program> = Vec::new();
    for &m in ws.modules(db) {
        let toks = tokens_in(db, ws, m);
        let parsed = ast_in(db, ws, m);
        if toks.0.diagnostics.is_empty() && parsed.0.diagnostics.is_empty() {
            module_programs.push(&parsed.0.program);
        }
    }

    // Dependency-package modules (package-manager P2.1c): re-rooted clones (owned, because the
    // rewrite mutates the parsed AST) that drive their own imports as closed units — the salsa twin
    // of the CLI's `link_with_deps`. Depends on each dep module's `ast`, so editing a path-dependency
    // source re-links, but leaves sibling parses untouched.
    let mut dep_programs: Vec<Program> = Vec::new();
    for &dm in ws.dep_modules(db) {
        let src = dm.src(db);
        let toks = tokens_in(db, ws, src);
        let parsed = ast_in(db, ws, src);
        if toks.0.diagnostics.is_empty() && parsed.0.diagnostics.is_empty() {
            let mut program = parsed.0.program.clone();
            noeta_loader::reroot_program(
                &mut program,
                dm.root(db),
                dm.key(db),
                &reroot_map(dm.renames(db)),
            );
            dep_programs.push(program);
        }
    }

    // No dependencies → the exact single-package path (byte-for-byte unchanged); otherwise the
    // deps-aware linker with the re-rooted dep programs as candidates and import drivers.
    let result = if dep_programs.is_empty() {
        noeta_loader::link_parsed(&entry_source, &entry_ast.0.program, &module_programs)
    } else {
        let dep_refs: Vec<&Program> = dep_programs.iter().collect();
        // The IDE query links from re-rooted dep *sources* but does not carry the resolved native
        // package set, so it stays lenient on foreign roots (`None`) — it flags a missing
        // intra-project module, never an import it cannot fully see (module-namespaces).
        noeta_loader::link_parsed_with_deps(
            &entry_source,
            &entry_ast.0.program,
            &module_programs,
            &dep_refs,
            None,
        )
    };
    match result {
        Ok(program) => LinkedProgram(Ok(program)),
        Err(load) => LinkedProgram(Err(load.into_iter().map(|d| d.diagnostic).collect())),
    }
}

/// The per-source [`EditionMap`](noeta_lexer::EditionMap) for a whole workspace — every member
/// source (entry, siblings, dependency modules) under its own package's edition, keyed by
/// `SourceId`. The salsa analogue of the loader's `Linked::editions`, so [`linked_checked`] applies
/// each package's edition per declaration over the merged program. Public so a consumer that checks
/// a *derived* program (e.g. `noeta-mcp` re-checking a tier-activated linked program) can apply the
/// same per-source editions — the `SourceId`s survive activation, so the map stays valid.
pub fn workspace_editions(db: &dyn salsa::Database, ws: Workspace) -> noeta_lexer::EditionMap {
    let mut map = noeta_lexer::EditionMap::new();
    for src in std::iter::once(ws.entry(db))
        .chain(ws.modules(db).iter().copied())
        .chain(ws.dep_modules(db).iter().map(|dm| dm.src(db)))
    {
        map.set(SourceId(src.id(db)), edition_of(db, src));
    }
    map
}

/// Type-check the linked program — the workspace analogue of [`checked`]. A load failure carries
/// its diagnostics straight through (there is no program to check).
#[salsa::tracked(returns(ref))]
pub fn linked_checked(db: &dyn salsa::Database, ws: Workspace) -> Checked {
    match &linked(db, ws).0 {
        // The shared helper maps every checker output field — both the LSP track's
        // `expr_types`/`f32_literal_sites` and the prelude-redesign handle-site maps.
        Ok(program) => from_check_output(noeta_check::check_all_with_editions(
            program,
            workspace_editions(db, ws),
        )),
        Err(diags) => Checked {
            diagnostics: diags.clone(),
            expr_types: std::collections::HashMap::new(),
            sites: noeta_check::Sites::default(),
            bundle_bindings: std::collections::HashMap::new(),
            packed_layouts: std::collections::HashMap::new(),
        },
    }
}

/// The IDE-flavored whole-workspace check: like [`linked_checked`], but the result's
/// [`Checked::expr_types`] is populated (via [`noeta_check::check_all_with_types`]) — the merged,
/// multi-file span→type index the LSP reads for cross-module hover and member navigation. The
/// compile path stays on [`linked_checked`] and never builds the index.
#[salsa::tracked(returns(ref))]
pub fn linked_checked_ide(db: &dyn salsa::Database, ws: Workspace) -> Checked {
    match &linked(db, ws).0 {
        Ok(program) => from_check_output(noeta_check::check_all_with(
            program,
            noeta_check::CheckOptions {
                record_expr_types: true,
                editions: workspace_editions(db, ws),
                ..noeta_check::CheckOptions::default()
            },
        )),
        Err(diags) => Checked {
            diagnostics: diags.clone(),
            expr_types: std::collections::HashMap::new(),
            sites: noeta_check::Sites::default(),
            bundle_bindings: std::collections::HashMap::new(),
            packed_layouts: std::collections::HashMap::new(),
        },
    }
}

/// Compile the linked program to a [`Module`] — the workspace analogue of [`bytecode`]. Callers
/// gate on [`linked`] being `Ok` (and [`linked_checked`] being empty) before reaching a real
/// `Module`; when the link failed there is nothing to compile, so an empty program stands in (a
/// valid, never-observed `Module`).
#[salsa::tracked(returns(ref))]
pub fn linked_bytecode(db: &dyn salsa::Database, ws: Workspace) -> Bytecode {
    match &linked(db, ws).0 {
        Ok(program) => {
            let checked = linked_checked(db, ws);
            Bytecode(noeta_compiler::compile_with_sites(
                program,
                checked.sites.clone(),
                false,
                // No debug info on the salsa/IDE bytecode path.
                false,
            ))
        }
        Err(_) => Bytecode(noeta_compiler::compile(&Program {
            stmts: Vec::new(),
            span: Span::empty_at(0),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_and_src(text: &str) -> (LangDatabase, SourceProgram) {
        let db = LangDatabase::default();
        let source = Source::new(SourceId::FIRST, "test.noe", text);
        let src = source_program(&db, &source, noeta_lexer::Edition::DEFAULT);
        (db, src)
    }

    #[test]
    fn the_ext_env_input_drives_workspace_text_tiers_and_invalidates() {
        // The extensions' verbatim-tier set is a real salsa INPUT: seeding it changes the
        // memoized answer, re-seeding invalidates. (Before, tracked queries read the process
        // global directly — a change could never invalidate a memoized parse.)
        let mut db = LangDatabase::default();
        let source = Source::new(SourceId::FIRST, "test.noe", "echo 1;\n");
        let src = source_program(&db, &source, noeta_lexer::Edition::DEFAULT);
        let ws = Workspace::new(&db, src, Vec::new(), Vec::new());
        seed_ext_env(&mut db, vec!["blueprint".to_string()]);
        assert!(
            workspace_text_tiers(&db, ws).contains(&"blueprint".to_string()),
            "the seeded tier set flows into the workspace tier union"
        );
        // Re-seeding the input invalidates the memoized answer.
        seed_ext_env(&mut db, Vec::new());
        assert!(
            !workspace_text_tiers(&db, ws).contains(&"blueprint".to_string()),
            "re-seeding the ExtEnv input must invalidate"
        );
    }

    #[test]
    fn pipeline_flows_through_queries() {
        let (db, src) = db_and_src("echo 1 + 2;\n");
        assert!(tokens(&db, src).0.diagnostics.is_empty());
        assert!(ast(&db, src).0.diagnostics.is_empty());
        assert!(bytecode(&db, src).0.is_ok());
    }

    #[test]
    fn queries_are_memoized_stable() {
        // The same input handed to a query twice yields the identical cached reference.
        let (db, src) = db_and_src("let x = 1;\necho x;\n");
        let a = ast(&db, src) as *const Ast;
        let b = ast(&db, src) as *const Ast;
        assert_eq!(
            a, b,
            "second call must return the memoized value, not recompute"
        );
    }

    #[test]
    fn checker_diagnostics_flow_through_the_query() {
        // A well-typed program checks clean; a non-exhaustive match surfaces E0011 through the
        // `checked` query — the front-end gate both backends consult.
        let (db, ok) = db_and_src("echo 1 + 2;\n");
        assert!(checked(&db, ok).diagnostics.is_empty());

        let (db, bad) = db_and_src("enum E { A; B; }\necho match E.A { E.A => 1 };\n");
        let diags = &checked(&db, bad).diagnostics;
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code.to_string(), "E0011");
    }

    #[test]
    fn update_replaces_in_place() {
        // Directly exercise the `unsafe` always-replace `Update` impl (salsa only invokes it
        // on input mutation, which the pass-through usage never does — so this is the miri
        // gate for the crate's only unsafe block). After `maybe_update`, the slot must hold
        // the new value and the call must report "changed".
        use salsa::Update;
        let mut slot = Tokens(noeta_lexer::Lexed::default());
        let mut replacement = noeta_lexer::Lexed::default();
        replacement.tokens.push(noeta_lexer::Token {
            kind: noeta_lexer::TokenKind::Semicolon,
            span: noeta_span::Span::new(0, 1),
        });
        let new = Tokens(replacement);
        // SAFETY: `&mut slot` is a valid, initialized, exclusively-borrowed `Tokens`.
        let changed = unsafe { Tokens::maybe_update(&mut slot as *mut Tokens, new) };
        assert!(changed, "always-replace must report a change");
        assert_eq!(
            slot.0.tokens.len(),
            1,
            "slot must hold the replacement value"
        );
    }

    #[test]
    fn unsupported_program_is_carried_not_panicked() {
        // A construct outside the VM subset surfaces as Unsupported through the query, the
        // same as a direct `compile` call — the differential harness skips these.
        let (db, src) = db_and_src("fn outer() { fn inner() { return 1; } return inner(); }\n");
        let direct = noeta_compiler::compile(&ast(&db, src).0.program);
        assert_eq!(bytecode(&db, src).0.is_ok(), direct.is_ok());
    }

    // ----- the module graph (M1.9.3) -----

    #[test]
    fn module_graph_links_checks_and_compiles_a_used_module() {
        let db = LangDatabase::default();
        let entry = Source::new(
            SourceId(0),
            "main.noe",
            "namespace App.Main;\nuse App.A.Foo;\nf = Foo { x: 1 };\necho f.x;\n",
        );
        let a = Source::new(
            SourceId(1),
            "a.noe",
            "namespace App.A;\npub class Foo { pub x: int }\n",
        );
        let ws = workspace(
            &db,
            &entry,
            std::slice::from_ref(&a),
            noeta_lexer::Edition::DEFAULT,
        );

        let prog = match &linked(&db, ws).0 {
            Ok(p) => p,
            Err(e) => panic!("link failed: {e:?}"),
        };
        // The real `Foo` is merged in under its qualified identity `App.A.Foo` (arc Phase B — the
        // salsa `linked` query qualifies in lockstep with the CLI loader) and its `use` dropped
        // (resolved, no opaque stub).
        assert!(
            prog.stmts
                .iter()
                .any(|s| matches!(s, noeta_ast::Stmt::Class(c) if c.name == "App.A.Foo"))
        );
        assert!(
            !prog
                .stmts
                .iter()
                .any(|s| matches!(s, noeta_ast::Stmt::Use { .. }))
        );
        // The whole-workspace checker and compiler run over the merge.
        assert!(linked_checked(&db, ws).diagnostics.is_empty());
        assert!(linked_bytecode(&db, ws).0.is_ok());
    }

    #[test]
    fn module_graph_reproduces_the_standalone_loader() {
        // The salsa link must produce the byte-identical merge of the text-based loader — the tie
        // that keeps the query layer behavior-preserving (and the differential oracle valid).
        let entry_text = "namespace App.Main;\nuse App.A.Foo;\nf = Foo { x: 1 };\necho f.x;\n";
        let a_text = "namespace App.A;\npub class Foo { x: int }\n";
        let raw = noeta_loader::RawModule {
            name: "a.noe".into(),
            text: a_text.into(),
        };
        let loader = noeta_loader::link(
            "main.noe",
            entry_text,
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&raw),
        )
        .unwrap();

        let db = LangDatabase::default();
        let entry = Source::new(SourceId(0), "main.noe", entry_text);
        let a = Source::new(SourceId(1), "a.noe", a_text);
        let ws = workspace(
            &db,
            &entry,
            std::slice::from_ref(&a),
            noeta_lexer::Edition::DEFAULT,
        );
        let salsa = match &linked(&db, ws).0 {
            Ok(p) => p.clone(),
            Err(e) => panic!("{e:?}"),
        };
        assert_eq!(
            salsa, loader.program,
            "the salsa graph must reproduce the loader's merge"
        );
    }

    #[test]
    fn editing_one_module_leaves_another_modules_parse_memoized() {
        // The incremental boundary: a `Workspace` of two modules; editing one must not recompute
        // the other's parse. (Pointer identity of the memoized `ast` is the same signal the
        // `queries_are_memoized_stable` test uses.)
        use salsa::Setter as _;
        let mut db = LangDatabase::default();
        let entry = Source::new(
            SourceId(0),
            "main.noe",
            "namespace App.Main;\nuse App.A.Foo;\n",
        );
        let a = Source::new(
            SourceId(1),
            "a.noe",
            "namespace App.A;\npub class Foo { pub x: int }\n",
        );
        let b = Source::new(
            SourceId(2),
            "b.noe",
            "namespace App.B;\npub class Bar { y: int }\n",
        );
        let entry_src = source_program(&db, &entry, noeta_lexer::Edition::DEFAULT);
        let a_src = source_program(&db, &a, noeta_lexer::Edition::DEFAULT);
        let b_src = source_program(&db, &b, noeta_lexer::Edition::DEFAULT);
        let ws = Workspace::new(&db, entry_src, vec![a_src, b_src], Vec::new());

        assert!(linked(&db, ws).0.is_ok());
        let a_ast_before = ast(&db, a_src) as *const Ast;

        // Edit module B — which the entry does not import — by adding a field.
        b_src
            .set_text(&mut db)
            .to("namespace App.B;\npub class Bar { y: int\n  z: int }\n".to_string());

        // Module A was not recomputed: its memoized parse is the identical value.
        let a_ast_after = ast(&db, a_src) as *const Ast;
        assert_eq!(
            a_ast_before, a_ast_after,
            "editing module B must not recompute module A's ast"
        );
        // The link itself re-runs (it depends on every module) and stays well-formed.
        assert!(linked(&db, ws).0.is_ok());
    }

    #[test]
    fn workspace_with_deps_resolves_cross_package_use() {
        // package-manager P2.1c: a dependency package keyed `hi` (its own root segment is `greet`)
        // links into the salsa graph; the entry's `use hi.hello.greeting` resolves after re-root.
        let db = LangDatabase::default();
        let entry = Source::new(
            SourceId(0),
            "main.noe",
            "use hi.hello.greeting;\necho greeting();\n",
        );
        let dep = DepSources {
            root: "greet".to_string(),
            key: "hi".to_string(),
            renames: Vec::new(),
            modules: vec![Source::new(
                SourceId(1),
                "hello.noe",
                "namespace greet.hello;\npub fn greeting(): string { return \"hi\"; }\n",
            )],
            edition: "2026".to_string(),
        };
        let ws = workspace_with_deps(
            &db,
            &entry,
            &[],
            std::slice::from_ref(&dep),
            noeta_lexer::Edition::DEFAULT,
        );
        let linked = linked(&db, ws);
        assert!(
            linked.0.is_ok(),
            "cross-package use must resolve: {:?}",
            linked.0
        );
        // The dependency's `greeting` fn was merged into the linked program (and its `use` no longer
        // survives as an unresolved import).
        let program = linked.0.as_ref().unwrap();
        assert!(
            program
                .stmts
                .iter()
                .any(|s| format!("{s:?}").contains("greeting")),
            "the dependency's greeting declaration must be linked in"
        );
    }
}
