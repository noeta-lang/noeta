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
    /// typed form — its package's edition (editions arc). Every db query that
    /// lexes/parses/checks this source does so under it, so the IDE stack honors a future edition's
    /// grammar/rules exactly as the batch compiler does. A string (not the enum) because a salsa
    /// input field must be `Update`, which the leaf `Edition` enum does not implement; the queries
    /// read it back with [`edition_of`]. Editing it invalidates exactly the queries that read it.
    #[returns(ref)]
    pub edition: noeta_lexer::Edition,
    /// The module path this source's **location** derives — its identity, which the linker writes in
    /// as its `namespace` (namespace-derivation arc). A [`Source`] carries a file name, not an
    /// identity, so the derivation has to be an input beside it or the query graph would fall back
    /// to declared namespaces while the batch loader derives, and the editor would disagree with the
    /// compiler about which module a file is.
    #[returns(ref)]
    pub module_path: DerivedPath,
}

/// The derived path at `index`, or [`ModulePath::Declared`](noeta_loader::ModulePath::Declared)
/// when the caller supplied none — so a workspace built without a package on disk behaves exactly
/// as it did before derivation.
fn path_at(paths: &[noeta_loader::ModulePath], index: usize) -> noeta_loader::ModulePath {
    paths.get(index).cloned().unwrap_or_default()
}

/// Build (or rebuild) the [`SourceProgram`] input from a [`Source`] and the language edition its
/// package is written against.
pub fn source_program(
    db: &LangDatabase,
    source: &Source,
    edition: noeta_lexer::Edition,
) -> SourceProgram {
    source_program_at(db, source, edition, noeta_loader::ModulePath::Declared)
}

/// [`source_program`] for a source whose module path its **location** derives — the workspace
/// builders' constructor (`noeta_loader::read_workspace` hands the paths over beside the sources).
pub fn source_program_at(
    db: &LangDatabase,
    source: &Source,
    edition: noeta_lexer::Edition,
    path: noeta_loader::ModulePath,
) -> SourceProgram {
    SourceProgram::new(
        db,
        source.id().0,
        source.name().to_string(),
        source.text().to_string(),
        edition,
        DerivedPath(path),
    )
}

/// The language edition a [`SourceProgram`] declares. The input stores the enum itself (cross-
/// cutting audit finding 4): the typed → string → lenient-re-parse round-trip is gone, so a
/// non-canonical edition can no longer silently compile as the default — invalid values are
/// unrepresentable here, rejected where strings genuinely enter (manifest parse, hard error).
fn edition_of(db: &dyn salsa::Database, src: SourceProgram) -> noeta_lexer::Edition {
    *src.edition(db)
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
    /// main's flat per-map fields, including the noeta-ext-abi `TypeRecipe` rename — the bundle's
    /// field types live in `noeta_check::Sites` and follow that rename through the re-export.)
    pub sites: noeta_check::Sites,
    /// Method-bundle bindings by target type (kernel-methods K4) — what member completion reads
    /// to offer bound methods.
    pub bundle_bindings: std::collections::HashMap<String, Vec<(String, String)>>,
    /// Every `@packed` struct's flat layout by type name — the IDE storage-fact index hover and
    /// inlay hints read (see [`noeta_check::Checked::packed_layouts`]).
    pub packed_layouts: std::collections::HashMap<String, noeta_ast::reflect::PackedLayout>,
    /// Every method declaration's derived receiver discipline, keyed by its name span — the IDE's
    /// receiver-hint index (see [`noeta_check::Checked::method_receivers`]).
    pub method_receivers: std::collections::HashMap<Span, noeta_check::Receiver>,
}

/// Compiler output: a [`Module`], or the first construct outside the VM's subset.
#[derive(Debug, Clone)]
pub struct Bytecode(pub Result<Module, Unsupported>);

/// Linker output (M1.9.3): the merged [`Program`] of an entry and its resolved imports, or the
/// `use`-resolution diagnostics (entry parse errors, E0019, E0020). The whole-workspace analogue
/// of [`Ast`] — what [`linked_checked`] and [`linked_bytecode`] build on.
#[derive(Debug, Clone)]
pub struct LinkedProgram {
    pub program: Result<Program, Vec<Diagnostic>>,
    /// Sources minted by **compile-time directive expansion** during this link, ids continuing past
    /// every member and dependency module (see [`linked_from`]). Empty for a program with no
    /// expanding directive, which is nearly every program.
    ///
    /// **This memo owns the generated text**, and deliberately so: unlike a member or a dependency
    /// module, an expansion has no file and no salsa input behind it — it is produced *by* this
    /// query. Minting a [`SourceProgram`] input per expansion instead would leak a slot per
    /// expansion ever produced, because salsa 0.27 cannot delete an input (see [`release_source`]).
    /// Consumers borrow the text out of the memo for as long as they borrow the db.
    pub expansions: Vec<noeta_loader::ExpandedSource>,
    /// The **non-`.noe` files the expansion hooks reported reading** (an OpenAPI spec, say) — the
    /// editor's rebuild trigger for foreign inputs. Populated whether the link **succeeded or
    /// failed**: a hook that failed because its spec was missing still reported the path, and that
    /// is exactly the read that must be watched, so that *creating* the file re-runs the expansion.
    /// A consumer (`ImpactSession`) watches these alongside the `.noe` members. Empty for the
    /// overwhelming majority of programs, which declare no expanding directive.
    pub reads: Vec<String>,
}

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

/// Give a newtype the [`salsa::Update`] salsa needs for an **input field or memoized output that
/// wants backdating**. Unlike [`replace_update!`], this compares old vs. new (the type is already
/// [`PartialEq`]) and reports "unchanged" when they are equal — so an edit that leaves the value
/// identical (a member-text change that does not touch the dependency graph) does *not* invalidate
/// the queries that read it. Used for the per-package `@name` tables and the per-package renamed
/// text-tier sets, which change only when a manifest's bindings or a dependency's declarations do.
macro_rules! backdating_update {
    ($ty:ty) => {
        // SAFETY: `old_pointer` points at an initialized `$ty` (salsa only calls `maybe_update`
        // when a previous value exists). We compare; on a difference we overwrite in place (dropping
        // the old value) and report "changed", otherwise leave it and report "unchanged" — a valid,
        // backdating `Update`.
        unsafe impl salsa::Update for $ty {
            unsafe fn maybe_update(old_pointer: *mut Self, new_value: Self) -> bool {
                let old = unsafe { &mut *old_pointer };
                if *old == new_value {
                    false
                } else {
                    *old = new_value;
                    true
                }
            }
        }
    };
}

/// The whole program's per-package `@name` resolution tables ([`noeta_span::PackageUses`]) as a
/// [`Workspace`] input field. A newtype because the foreign `PackageUses` cannot carry this crate's
/// [`salsa::Update`] impl (orphan rule); backdating so a member-text edit that leaves the dependency
/// graph unchanged does not invalidate the per-package text-tier lexing. Built on the query path
/// (`noeta_pm::graph::resolve_graph_query`) exactly like the dependency modules beside it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceUses(pub noeta_span::PackageUses);

backdating_update!(WorkspaceUses);

/// The module path a source's **location** derives (`noeta_loader::derive`) as a [`SourceProgram`]
/// input field. Newtype for the same [`salsa::Update`]/orphan reason as [`WorkspaceUses`];
/// backdating, because a file's path changes only when the file moves — an edit to its text must not
/// invalidate the link.
///
/// The default, [`ModulePath::Declared`](noeta_loader::ModulePath::Declared), means the source was
/// reached with no package context, so its own `namespace` declaration stands — every single-file
/// query and every lone script.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DerivedPath(pub noeta_loader::ModulePath);

backdating_update!(DerivedPath);

/// The local `@name`s each package binds to a **text** (verbatim-body) tier, keyed by the binding
/// package's [`PackageOrigin`] — the per-package input to [`tokens_in`]'s lex, memoizing what
/// [`noeta_loader::renamed_text_tier_locals`] returned. Newtype for the same [`salsa::Update`]/orphan reason as
/// [`WorkspaceUses`]; backdating so it invalidates the workspace-aware lexes only when a `[tiers]`
/// binding or a dependency's `@tier(…, text)` declaration actually changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RenamedTextTiers(std::collections::HashMap<noeta_span::PackageOrigin, Vec<String>>);

backdating_update!(RenamedTextTiers);

/// The installed extensions' tiers ([`noeta_loader::ExtTiers`]) as an [`ExtEnv`] field. A newtype
/// for the same [`salsa::Update`]/orphan reason as [`WorkspaceUses`], and backdating so re-seeding
/// an identical registry does not invalidate every lex.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtTierEnv(pub noeta_loader::ExtTiers);

backdating_update!(ExtTierEnv);

/// The **extension environment** as an explicit salsa input (singleton): the installed
/// extensions' tiers, which [`ast`] and [`workspace_text_tiers`] fold into lexing. Previously these
/// were read straight off the process-global registry inside tracked queries — a hidden non-salsa
/// input, sound only while the global never changes after first query, and a silent-staleness
/// landmine for an embedder pairing a `LangDatabase` with a per-session registry. Constructors that
/// know their registry seed this via [`seed_ext_env`]; an unseeded db falls back to the global
/// default (the documented single-registry stance for the CLI tools), and the fallback is *recorded
/// on this input* so it stays one dependency.
///
/// It carries each tier's **provider root** and not only its name, because per-package resolution is
/// scoped: `[tiers] notes = "speckit:json"` names *speckit's* `json`, and answering "is that a
/// verbatim tier?" by bare name says yes for std's `@json` — capturing prose the compiler lexes as
/// code. See [`noeta_loader::ExtTiers::is_verbatim_scoped`].
#[salsa::input(singleton, debug)]
pub struct ExtEnv {
    #[returns(ref)]
    pub ext_tiers: ExtTierEnv,
}

/// Create (or overwrite) the db's [`ExtEnv`] from an explicit tier set. What an embedder with a
/// per-session registry calls after assembling it (`ExtTiers::from_registry`).
pub fn seed_ext_env(db: &mut dyn salsa::Database, tiers: noeta_loader::ExtTiers) {
    use salsa::Setter as _;
    match ExtEnv::try_get(db) {
        Some(env) => {
            env.set_ext_tiers(db).to(ExtTierEnv(tiers));
        }
        None => {
            ExtEnv::new(db, ExtTierEnv(tiers));
        }
    }
}

/// The extension tiers for `db`: the seeded [`ExtEnv`], or — first read of an unseeded db — the
/// process-global default registry's, captured onto the input so later reads depend on salsa state,
/// not the global.
fn ext_tiers(db: &dyn salsa::Database) -> noeta_loader::ExtTiers {
    match ExtEnv::try_get(db) {
        Some(env) => env.ext_tiers(db).0.clone(),
        None => noeta_loader::ExtTiers::from_process_registry(),
    }
}

/// The **verbatim-body** tier names the extensions installed for `db` declare — the program-wide
/// (unscoped) half of the lexer's seed, for an ambient `@json` that no `[tiers]` binding renames.
fn ext_verbatim_tier_names(db: &dyn salsa::Database) -> Vec<String> {
    ext_tiers(db).verbatim_names()
}

/// Tokenize the source. Memoized; re-runs only when `SourceProgram::text` changes.
#[salsa::tracked(returns(ref))]
pub fn tokens(db: &dyn salsa::Database, src: SourceProgram) -> Tokens {
    let source = source_of(db, src);
    let mut lexed = noeta_lexer::lex_in(
        &source,
        edition_of(db, src),
        &noeta_lexer::TextTiers::default(),
    );
    // Stamped for the same reason as in [`tokens_in`]: a lex error must name the file it is in.
    noeta_loader::retarget_diagnostics(&mut lexed.diagnostics, source.id());
    Tokens(lexed)
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
    let mut parsed = noeta_parser::parse_in(&source, &toks.0.tokens, edition_of(db, src), &set);
    // Stamped for the same reason as the lex diagnostics: a few parser spans carry the default
    // entry id (see [`tokens`]).
    noeta_loader::retarget_diagnostics(&mut parsed.diagnostics, source.id());
    Ast(parsed)
}

/// Type-check the AST and return the checker's diagnostics. Depends on [`ast`]. The pipeline's
/// front-end gate: a program with type errors is rejected before either backend runs (so both
/// backends surface the identical compile-time result — see the conformance differential).
#[salsa::tracked(returns(ref))]
pub fn checked(db: &dyn salsa::Database, src: SourceProgram) -> Checked {
    let parsed = ast(db, src);
    from_check_output(noeta_check::check_all_cancellable(
        &parsed.0.program,
        // A single-source check has no package graph, so provenance stays unknown and the orphan
        // rule stands down; the whole-workspace queries below carry the real map.
        noeta_check::CheckOptions::for_sources(source_edition_map(db, src)),
        &|| db.unwind_if_revision_cancelled(),
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
    from_check_output(noeta_check::check_all_cancellable(
        &parsed.0.program,
        noeta_check::CheckOptions::for_sources(source_edition_map(db, src)).with_expr_types(),
        &|| db.unwind_if_revision_cancelled(),
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
        method_receivers: out.method_receivers,
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
// The module graph (M1.9.3; entry-parametric since the ide-workspaces rework)
// ---------------------------------------------------------------------------
//
// A multi-file program is a [`Workspace`]: a flat set of member `SourceProgram` inputs (a
// directory's `.noe` files) plus resolved dependency modules. The [`linked_from`] query resolves
// ONE member's — the entry's — `use` declarations against the other members' declared namespaces
// (reusing each source's memoized [`ast_in`]) and merges the resolved declarations into one
// [`Program`]; [`linked_checked_from`] and [`linked_bytecode_from`] are the whole-program checker
// and compiler over that merge — the workspace analogues of [`checked`] and [`bytecode`].
//
// ```text
//   Workspace (input: member SourcePrograms, SHARED by every entry)
//        │
//        ├── ast_in(member_0) ──┐
//        ├── ast_in(member_1) ──┤
//        ├── ast_in(member_n) ──┴──►  linked_from(ws, entry)  ──►  linked_checked_from(ws, entry)
//        │                                    │
//        │                                    └──────────────────►  linked_bytecode_from(ws, entry)
// ```
//
// The entry is a QUERY PARAMETER, not a workspace field: salsa memoizes the link/check per
// `(ws, entry)`, while the per-source `tokens_in`/`ast_in` memoize once per file no matter how
// many entries link over the same workspace — the sharing that lets an editor keep one workspace
// (one set of inputs) per directory instead of one per open document (audit-4 finding 6).
// Resolution lives in [`linked_from`], so it depends on every member's `ast_in` — editing any
// member re-links — but the per-source parses stay independent: editing one module never
// recomputes another's parse. That is the incremental boundary M2's hot reload builds on.
//
// The classic single-entry surface ([`linked`], [`linked_checked`], [`linked_checked_ide`],
// [`linked_bytecode`]) remains as thin wrappers that link from the workspace's FIRST member —
// the conventional entry every [`workspace`]-constructed workspace puts at index 0 — so the
// compile-path consumers (conformance, MCP `check`) read exactly what they always did.

/// A multi-file program: the member sources — each a memoized [`SourceProgram`] input — and, for
/// a package (package-manager P2.1c), its resolved dependency packages' modules. Mutating any one
/// source invalidates exactly the queries that read it. There is no distinguished entry member:
/// the entry is a parameter of [`linked_from`] and friends, so one workspace serves every member
/// as an entry (memoized per `(ws, entry)`).
#[salsa::input(debug)]
pub struct Workspace {
    #[returns(ref)]
    pub members: Vec<SourceProgram>,
    /// The resolved dependency packages' modules (empty for a lone/sibling-only workspace). Each
    /// carries its re-root info; [`linked_from`] re-roots and links them as closed units.
    #[returns(ref)]
    pub dep_modules: Vec<DepModule>,
    /// The whole program's per-package `@name` resolution tables (`[directives]`/`[tiers]`), resolved
    /// on the query path (`noeta_pm::graph::resolve_graph_query`) alongside `dep_modules`. Empty for a
    /// lone/sibling-only workspace (no manifest, no bindings). [`workspace_renamed_text_tiers`] reads
    /// it to lex a package's renamed text tiers (`[tiers] docs = "std:doc"`) verbatim in the editor,
    /// exactly as the loader does under `noeta run`/`noeta check`.
    #[returns(ref)]
    pub package_uses: WorkspaceUses,
    /// **Whether the caller resolved a complete dependency graph**, and if so the declared
    /// native-package roots it found (`noeta_loader::native_dep_roots`).
    ///
    /// This is what makes a link *strict*. `Some(roots)` means every legitimate import root is
    /// known — the std extensions plus these — so a `use` resolving to nothing is `E0019`. `None`
    /// means the caller has no graph (a scratch buffer, a synthetic program) and the link stays
    /// lenient about foreign roots, flagging only a missing intra-project module.
    ///
    /// It is a workspace field rather than a per-call flag because it is a fact about *this
    /// program's* resolution, not about who is asking. `noeta check` was strict and the editor was
    /// lenient, so a file whose `use` named nothing at all showed clean in the editor and failed on
    /// the command line — the same class of disagreement as an unswept tier body, one layer down.
    #[returns(ref)]
    pub native_roots: NativeRoots,
}

/// [`Workspace::native_roots`] as a salsa input field: `None` = no resolved graph (lenient),
/// `Some(roots)` = a complete one. Newtype for the same [`salsa::Update`]/orphan reason as
/// [`WorkspaceUses`]; backdating, because it changes only when the dependency graph does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeRoots(pub Option<Vec<String>>);

backdating_update!(NativeRoots);

/// The workspace's conventional entry: its **first member**. What the classic single-entry query
/// surface ([`linked`], [`linked_checked`], …) links from — every [`workspace`]/
/// [`workspace_with_deps`]-constructed workspace puts the entry at index 0, preserving their
/// behavior exactly. Panics on an (unconstructible via the public helpers) empty workspace.
pub fn workspace_entry(db: &dyn salsa::Database, ws: Workspace) -> SourceProgram {
    ws.members(db)[0]
}

/// One dependency package module in a salsa [`Workspace`] (package-manager P2.1c): its source input
/// plus the re-root info [`linked`] applies before merging it (`root`→`prefix`, then each of the
/// package's local dependency keys → the target package's global segment). `renames` is a **flat**
/// list of `[local0, global0, local1, global1, …]` pairs — a `BTreeMap` is not a salsa input field
/// type, so the query rebuilds it (see [`reroot_map`]).
#[salsa::input(debug)]
pub struct DepModule {
    pub src: SourceProgram,
    /// The package's own root segment — what its modules derive under standalone, and therefore the
    /// leading segment of its intra-package `use`s (`noeta_loader::DepPackage::root`).
    #[returns(ref)]
    pub root: String,
    /// The prefix this package's modules derive under in this consumer's build — the import key alone,
    /// or `{key}.{package segment}` for a scope-array member (`noeta_loader::DepPackage::prefix`).
    #[returns(ref)]
    pub prefix: Vec<String>,
    #[returns(ref)]
    pub renames: Vec<String>,
}

impl DepModule {
    /// The consumer's **import key** for this module's package — the first segment of
    /// [`Self::prefix`]. What the package is labeled by (`PackageOrigin::Dependency`) and addressed
    /// as a whole; the segments after it, if any, belong to the package rather than to the manifest.
    pub fn import_key(self, db: &dyn salsa::Database) -> &str {
        self.prefix(db).first().map_or("", String::as_str)
    }
}

/// A dependency package's sources + re-root info, the ergonomic input to [`workspace_with_deps`]
/// (package-manager P2.1c). Mirrors `noeta_loader::DepPackage` but with already-labeled [`Source`]s.
#[derive(Debug)]
pub struct DepSources {
    pub root: String,
    /// The prefix this package's modules derive under here — the import key alone, or
    /// `{key}.{package segment}` for a scope-array member.
    pub prefix: Vec<String>,
    pub renames: Vec<(String, String)>,
    pub modules: Vec<Source>,
    /// The module path each of `modules` derives, index-aligned with it. Empty for a caller that
    /// builds dependency sources without a package on disk (then each module's own `namespace`
    /// declaration stands, as before derivation).
    pub paths: Vec<noeta_loader::ModulePath>,
    /// This package's language edition — its modules are parsed and checked under it, exactly as
    /// the CLI's `load_with_deps` does (editions arc). Typed: a value resolution never produced is
    /// unrepresentable, instead of a free string silently degrading to the default.
    pub edition: noeta_lexer::Edition,
}

/// Build a [`Workspace`] input from the entry [`Source`], its sibling module sources (as produced by
/// `noeta_loader::read_workspace`), and the root package's edition. Each source becomes a
/// [`SourceProgram`] under `root_edition`; no dependency packages (use [`workspace_with_deps`]).
/// The entry becomes the first member, so the classic [`linked`] surface links from it.
pub fn workspace(
    db: &LangDatabase,
    entry: &Source,
    modules: &[Source],
    root_edition: noeta_lexer::Edition,
    paths: &[noeta_loader::ModulePath],
) -> Workspace {
    let members = std::iter::once(entry)
        .chain(modules)
        .enumerate()
        .map(|(i, s)| source_program_at(db, s, root_edition, path_at(paths, i)))
        .collect();
    // No manifest on this deps-free path → no `[tiers]`/`[directives]` bindings, so no renamed text
    // tiers (a member's own `@tier(…, text)` is discovered by the per-file token scan regardless).
    Workspace::new(
        db,
        members,
        Vec::new(),
        WorkspaceUses::default(),
        NativeRoots::default(),
    )
}

/// Build a [`Workspace`] that also links **dependency packages** (package-manager P2.1c): the entry
/// and siblings take `root_edition`; each dependency's modules take that package's own edition. Each
/// dep module becomes a [`DepModule`] input carrying its re-root info, so cross-package
/// `use <dep-key>.…` resolves in the salsa graph exactly as in the CLI's `load_with_deps`.
///
/// `package_uses` is the whole program's per-package `@name` resolution tables
/// (`[directives]`/`[tiers]`), resolved on the query path (`noeta_pm::graph::resolve_graph_query`)
/// exactly as `noeta_project::workspace::sync` does — so a renamed text tier
/// (`[tiers] docs = "std:doc"`) lexes verbatim through this path, not only the editor's. A caller
/// with no manifest bindings (an inline source, a synthetic filesystem-only dependency graph) passes
/// an empty [`PackageUses`](noeta_span::PackageUses), which is behavior-identical to before this
/// parameter existed.
pub fn workspace_with_deps(
    db: &LangDatabase,
    entry: &Source,
    modules: &[Source],
    deps: &[DepSources],
    package_uses: &noeta_span::PackageUses,
    root_edition: noeta_lexer::Edition,
    paths: &[noeta_loader::ModulePath],
) -> Workspace {
    let members = std::iter::once(entry)
        .chain(modules)
        .enumerate()
        .map(|(i, s)| source_program_at(db, s, root_edition, path_at(paths, i)))
        .collect();
    let mut dep_inputs = Vec::new();
    for dep in deps {
        let renames = flatten_renames(&dep.renames);
        for (i, src) in dep.modules.iter().enumerate() {
            let sp = source_program_at(db, src, dep.edition, path_at(&dep.paths, i));
            dep_inputs.push(DepModule::new(
                db,
                sp,
                dep.root.clone(),
                dep.prefix.clone(),
                renames.clone(),
            ));
        }
    }
    Workspace::new(
        db,
        members,
        dep_inputs,
        WorkspaceUses(package_uses.clone()),
        NativeRoots::default(),
    )
}

/// Reclaim the resident content of a [`SourceProgram`] whose file was **deleted** from a workspace
/// (audit F9 residual a). salsa 0.27 has **no public API to delete an input**: inputs live in an
/// append-only table (`salsa::input`), and the only teardown paths (`evict_lru`, revision GC) act on
/// LRU-configured *tracked functions*, never on inputs or on non-LRU memos. So a source that vanishes
/// cannot have its input slot freed. What *can* be reclaimed is everything unbounded the slot anchors:
///
/// 1. the input's own **text** (the largest per-file allocation) — cleared to the empty string;
/// 2. every **downstream memo** keyed on this source — `tokens_in`/`ast_in` and, when the source was
///    ever linked as an entry, `linked_from`/`linked_checked*_from`/`linked_bytecode_from` — which
///    salsa would otherwise keep resident at full size (a stale-but-live AST/`Module`/type-index)
///    until an LRU eviction that never comes. Clearing the text *invalidates* those memos but does
///    not shrink them; reading each leaf query once here **recomputes it over the now-empty source**,
///    so salsa overwrites the fat memo in place with an empty-program equivalent.
///
/// The fixed-size input struct itself (its `Id` and now-empty fields) stays resident — a bounded,
/// per-deleted-file remainder salsa 0.27 cannot free. The [`WorkspaceCache`](crate) reuses these
/// tombstones for the next genuinely-new file (see `noeta-ide`'s `workspace::sync`), so the input
/// table is bounded by the *concurrent* file high-water mark, not the total ever seen.
///
/// `ws` is the source's (former) workspace — the recompute keys the workspace-parametric leaves on
/// it. Idempotent: releasing an already-emptied source is a cheap re-confirmation.
pub fn release_source(db: &mut LangDatabase, ws: Workspace, src: SourceProgram) {
    use salsa::Setter as _;
    // 0. The tier shapes this source had *before* it was emptied. Their memos
    //    (`shape_activated_from`/`shape_checked_from`) are keyed on the shape, so once the text is
    //    gone `entry_code_tiers` comes back empty and nothing would name them again — they would stay
    //    resident at full size (a whole activated `Program`) forever. Captured here, recomputed in
    //    step 2 over the emptied source. Empty for nearly every file, which costs nothing.
    //    (Only the swept single-tier shapes: an editor session asks for no other, and a caller that
    //    supplies an explicit multi-tier selection — `noeta check --tier a --tier b` — runs on a
    //    throwaway database it drops whole.)
    let tiers: Vec<String> = entry_code_tiers(db, ws, src).clone();
    // 1. Free the source text and name (the unbounded per-file allocations the input holds).
    src.set_name(db).to(String::new());
    src.set_text(db).to(String::new());
    // 2. Overwrite the fat downstream memos with empty-program equivalents by recomputing each leaf
    //    over the emptied source. The workspace-parametric family is what an editor session populates
    //    (every open document links through `tokens_in`/`ast_in` and, as an entry, the `linked_*`
    //    queries); reading the two leaves below transitively recomputes all of them, tiny.
    let _ = ast_in(db, ws, src); // recomputes tokens_in + ast_in
    let _ = linked_checked_ide_from(db, ws, src); // recomputes linked_from + the ide check as entry
    let _ = linked_bytecode_from(db, ws, src); // recomputes linked_checked_from + linked_from + Module
    let _ = entry_code_tiers(db, ws, src); // now empty
    for tier in tiers {
        // Recomputes shape_activated_from + shape_checked_from over the emptied source, overwriting
        // each fat memo (an activated `Program` and its `Checked`) with an empty-program equivalent.
        let _ = shape_checked_from(db, ws, src, vec![tier]);
    }
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

/// The workspace's declared text-tier names (text-tiers arc): the union of every member file's
/// `@tier(<name>, …, text: "…")` declarations — members and dependency modules alike —
/// sorted and deduped. Derived from the per-file [`tokens`] scans, so an edit that adds or
/// removes a declaration changes this value and invalidates exactly the workspace-aware lexes
/// ([`tokens_in`]); any other edit backdates (the value compares equal) and they stay memoized.
#[salsa::tracked(returns(ref))]
pub fn workspace_text_tiers(db: &dyn salsa::Database, ws: Workspace) -> Vec<String> {
    let mut names: Vec<String> = ws
        .members(db)
        .iter()
        .copied()
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

/// The local `@name`s each package binds to a **text** (verbatim-body) tier, keyed by the binding
/// package's [`PackageOrigin`] — the per-package input to [`tokens_in`]'s lex (per-package
/// tier-naming arc, sub-step 3g).
///
/// **Not a twin of the loader's resolution — the same function.** This query supplies the two inputs
/// the editor has and the batch loader does not phrase the same way, and
/// [`noeta_loader::renamed_text_tier_locals`] does the resolving:
///
/// * the **program-declared** text tiers of each dependency, from the dependency modules' own
///   [`tokens`] scan, indexed by the dependency's link segment (its [`PackageOrigin::Dependency`]
///   key) — the segment a binding's `provider_roots` carries for a `.noe` provider; and
/// * the installed extensions' tiers, read through the [`ExtEnv`] input (as [`workspace_text_tiers`]
///   already reads ext tiers) so this stays a salsa dependency rather than a process global.
///
/// Enumerating those two is genuinely per-surface: the loader walks a `Vec<Lexed>` beside a
/// `PackageMap` it just built, the editor walks salsa's `dep_modules`. Deciding what they *mean* is
/// not, and that half is where the divergence was: this query used to match an ext tier by bare
/// name, so `[tiers] notes = "speckit:json"` — a dependency's **code** tier that happens to share a
/// name with std's verbatim `@json` — captured its body as prose here while the loader lexed it as
/// code. `noeta check` passed a file `noeta run` could not lex.
///
/// Empty for a workspace with no `@name` bindings (a lone/sibling-only directory). Backdates on the
/// dependency graph, not on member text (see [`RenamedTextTiers`]).
#[salsa::tracked(returns(ref))]
fn workspace_renamed_text_tiers(db: &dyn salsa::Database, ws: Workspace) -> RenamedTextTiers {
    let uses = &ws.package_uses(db).0;
    if uses.is_empty() {
        return RenamedTextTiers::default();
    }
    let declared = ws.dep_modules(db).iter().filter_map(|dm| {
        let decls = &tokens(db, dm.src(db)).0.text_tier_decls;
        (!decls.is_empty()).then(|| (dm.import_key(db).to_string(), decls.clone()))
    });
    RenamedTextTiers(noeta_loader::renamed_text_tier_locals(
        uses,
        declared,
        &ext_tiers(db),
    ))
}

/// The workspace's program-wide verbatim-body set as a [`noeta_lexer::TextTiers`] — what both
/// widenings below start from.
fn global_text_tiers(db: &dyn salsa::Database, ws: Workspace) -> noeta_lexer::TextTiers {
    noeta_lexer::TextTiers::with(workspace_text_tiers(db, ws).iter().cloned())
}

/// The verbatim-body text-tier set the workspace-aware lex applies to **one** source: the whole
/// workspace's global set ([`workspace_text_tiers`]) widened by the local `@name`s the source's own
/// package renamed onto a text tier ([`workspace_renamed_text_tiers`]). A source whose package
/// renamed nothing lexes with exactly the global set.
fn source_text_tiers(
    db: &dyn salsa::Database,
    ws: Workspace,
    src: SourceProgram,
) -> noeta_lexer::TextTiers {
    let renamed = workspace_renamed_text_tiers(db, ws);
    // The memoized map, not a rebuilt one: this runs once per source, and rebuilding an N-entry map
    // each time is what made a whole-directory lex quadratic (see [`workspace_package_map`]).
    let locals = workspace_package_map(db, ws)
        .0
        .source_package(SourceId(src.id(db)))
        .and_then(|origin| renamed.0.get(origin));
    noeta_loader::widened_text_tiers(&global_text_tiers(db, ws), locals.into_iter().flatten())
}

/// The union of every package's verbatim-body text tiers — the workspace-wide set the parser and
/// directive expansion consult, for the `${…}`-hole reason [`noeta_loader::widened_text_tiers`]
/// gives.
fn workspace_text_tiers_union(db: &dyn salsa::Database, ws: Workspace) -> noeta_lexer::TextTiers {
    noeta_loader::widened_text_tiers(
        &global_text_tiers(db, ws),
        workspace_renamed_text_tiers(db, ws).0.values().flatten(),
    )
}

/// Workspace-aware tokenization: like [`tokens`], but lexing with the source's package text-tier
/// set ([`source_text_tiers`]) — so a text tier declared in one file (or dependency package), or one
/// a package **renamed** through a `[tiers]` binding, captures `@<name> { … }` bodies verbatim in
/// that package's members. What [`linked`] reads; the per-file [`tokens`] stays the single-file
/// surface (its two-pass covers same-file declarations).
#[salsa::tracked(returns(ref))]
pub fn tokens_in(db: &dyn salsa::Database, ws: Workspace, src: SourceProgram) -> Tokens {
    let set = source_text_tiers(db, ws, src);
    let source = source_of(db, src);
    let mut lexed = noeta_lexer::lex_in(&source, edition_of(db, src), &set);
    // A handful of lexer spans are built with the default entry id, so a lex error in any member
    // but the first pointed at the wrong file (see `noeta_loader::retarget_diagnostics`).
    noeta_loader::retarget_diagnostics(&mut lexed.diagnostics, source.id());
    Tokens(lexed)
}

/// Workspace-aware parse over [`tokens_in`] — the [`linked`] pipeline's counterpart of [`ast`].
#[salsa::tracked(returns(ref))]
pub fn ast_in(db: &dyn salsa::Database, ws: Workspace, src: SourceProgram) -> Ast {
    let source = source_of(db, src);
    let toks = tokens_in(db, ws, src);
    // The whole workspace's verbatim-body tier union — a superset of what `tokens_in` lexed this
    // source with — so a nested tier body inside a `${…}` hole re-lexes correctly (an inline
    // `@html { … }` loop), matching the loader's parser set.
    let set = workspace_text_tiers_union(db, ws);
    let mut parsed = noeta_parser::parse_in(&source, &toks.0.tokens, edition_of(db, src), &set);
    // Stamped for the same reason as in [`tokens_in`]: a parse error must name the file it is in.
    noeta_loader::retarget_diagnostics(&mut parsed.diagnostics, source.id());
    Ast(parsed)
}

/// Resolve and merge the workspace **from `entry`** (any member): the entry's imports against the
/// other members' declared namespaces, producing one merged [`Program`] (or the load diagnostics).
/// Memoized per `(ws, entry)`; depends on every member's [`ast_in`] (so editing any module
/// re-links), but not on any cross-module edge — the per-source parse queries remain independent
/// and memoize once per file no matter how many entries link over the same workspace. The merge
/// means both backends run the linked program unchanged, so the differential oracle is preserved
/// by construction.
///
/// **Compile-time directive expansion runs here**, through the loader's one
/// [`run_expansion`](noeta_loader::run_expansion) — the same decision the CLI's `link`/
/// `link_with_deps`/`ParsedDir::link_entry` make. Without it the editor and the compiler disagreed
/// about what a decorated type's members are: a generated method resolved under `noeta run`/
/// `noeta check` and showed as an unknown name in the editor. The generated sources come back in
/// [`LinkedProgram::expansions`] rather than becoming inputs (see that field).
#[salsa::tracked(returns(ref))]
pub fn linked_from(db: &dyn salsa::Database, ws: Workspace, entry: SourceProgram) -> LinkedProgram {
    let entry_tokens = tokens_in(db, ws, entry);
    let entry_ast = ast_in(db, ws, entry);
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
        // No reads: expansion has not run (the entry did not even parse).
        return unlinked(entry_diags, Vec::new());
    }

    let entry_source = source_of(db, entry);
    let broken = broken_modules(db, ws);
    // Read each other member's `ast_in` (this is what makes the link a dependent of every module).
    // Only a cleanly-parsed module contributes; `link_parsed` keeps just the ones declaring a
    // namespace. Broken siblings come from [`broken_modules`] — kept, not dropped, so a `use` of a
    // namespace one of them declares reports that file's parse error instead of the misleading
    // "no module" cascade (see `noeta_loader::BrokenModule`).
    //
    // A workspace whose sources carry **derived** module paths needs those written into the programs
    // (the path becomes the module's `namespace`), and salsa's parsed ASTs are shared and immutable —
    // so those programs are cloned. A workspace with no package derives nothing and clones nothing,
    // which is every lone script and every conformance case.
    //
    // The test is `!is_declared()` and not `derived().is_some()`, because the pass this gates does
    // not only *write* paths — it also refuses the ones the filesystem cannot spell (E0074), and an
    // illegal path is not a derived one. Gating on `derived()` meant a workspace whose every member
    // was `Declared` or `Illegal` skipped the pass, so `noeta check` (and the LSP, and the MCP
    // `check` tool) accepted a package whose only module is `my-utils.noe` while `noeta run` on that
    // same file reported it. Any legally-derived sibling — or any dependency, every one of whose
    // modules derives — flipped the flag and hid the divergence, which is why it survived: the
    // guard test for the data-directory rule has a `src/main.noe` beside the illegal file.
    let derives = ws
        .members(db)
        .iter()
        .copied()
        .chain(ws.dep_modules(db).iter().map(|dm| dm.src(db)))
        .any(|m| !m.module_path(db).0.is_declared());
    let mut module_owned: Vec<(SourceProgram, Program)> = Vec::new();
    let mut module_programs: Vec<&Program> = Vec::new();
    for &m in ws.members(db) {
        if m == entry {
            continue;
        }
        let toks = tokens_in(db, ws, m);
        let parsed = ast_in(db, ws, m);
        if toks.0.diagnostics.is_empty() && parsed.0.diagnostics.is_empty() {
            if derives {
                module_owned.push((m, parsed.0.program.clone()));
            } else {
                module_programs.push(&parsed.0.program);
            }
        }
    }

    // Dependency-package modules (package-manager P2.1c): re-rooted clones (owned, because the
    // rewrite mutates the parsed AST) that drive their own imports as closed units — the salsa twin
    // of the CLI's `link_with_deps`. Depends on each dep module's `ast`, so editing a path-dependency
    // source re-links, but leaves sibling parses untouched.
    let mut dep_programs: Vec<Program> = Vec::new();
    let mut dep_srcs: Vec<SourceProgram> = Vec::new();
    for &dm in ws.dep_modules(db) {
        let src = dm.src(db);
        let toks = tokens_in(db, ws, src);
        let parsed = ast_in(db, ws, src);
        let diagnostics: Vec<Diagnostic> = toks
            .0
            .diagnostics
            .iter()
            .chain(parsed.0.diagnostics.iter())
            .cloned()
            .collect();
        if diagnostics.is_empty() {
            let mut program = parsed.0.program.clone();
            noeta_loader::reroot_program(
                &mut program,
                dm.root(db),
                dm.prefix(db),
                &reroot_map(dm.renames(db)),
            );
            dep_programs.push(program);
            dep_srcs.push(src);
        }
    }
    // A dependency module that does not parse is a hard error, exactly as in the CLI's
    // `link_with_deps`. Unlike a workspace member it is not a file the editor checks in its own
    // right, so nothing else would ever report it — dropping it left the consumer with only the
    // "no module" cascade at its `use`.
    if !broken.deps.is_empty() {
        return unlinked(
            broken
                .deps
                .iter()
                .flat_map(|m| m.diagnostics.iter().cloned())
                .collect(),
            // No reads: expansion has not run (a dependency module failed to parse).
            Vec::new(),
        );
    }

    // No dependencies → the exact single-package path (byte-for-byte unchanged); otherwise the
    // deps-aware linker with the re-rooted dep programs as candidates and import drivers.
    let broken_refs: Vec<&noeta_loader::BrokenModule> = broken
        .members
        .iter()
        .filter(|m| m.source.id() != SourceId(entry.id(db)))
        .collect();
    // Derivation decides identity, applied here exactly as the batch loader applies it — after
    // re-rooting, over every unit at once (a collision is a program-wide fact). Nothing to do, and
    // nothing cloned, when the workspace carries no derived paths.
    let mut entry_owned = derives.then(|| entry_ast.0.program.clone());
    // The files the units render diagnostics against, rebuilt from the inputs: modules first, then
    // dependency modules, in the order the two lists were filled.
    let unit_sources: Vec<Source> = if derives {
        module_owned
            .iter()
            .map(|(src, _)| *src)
            .chain(dep_srcs.iter().copied())
            .map(|src| source_of(db, src))
            .collect()
    } else {
        Vec::new()
    };
    if let Some(entry_program) = entry_owned.as_mut() {
        let dep_offset = module_owned.len();
        let mut units = vec![noeta_loader::DerivedUnit {
            source: &entry_source,
            path: &entry.module_path(db).0,
            program: entry_program,
        }];
        units.extend(
            module_owned
                .iter_mut()
                .enumerate()
                .map(|(i, (src, program))| noeta_loader::DerivedUnit {
                    source: &unit_sources[i],
                    path: &src.module_path(db).0,
                    program,
                }),
        );
        units.extend(dep_programs.iter_mut().zip(&dep_srcs).enumerate().map(
            |(i, (program, src))| noeta_loader::DerivedUnit {
                source: &unit_sources[dep_offset + i],
                path: &src.module_path(db).0,
                program,
            },
        ));
        let path_diagnostics = noeta_loader::apply_derived_paths(units);
        if !path_diagnostics.is_empty() {
            return unlinked(
                path_diagnostics.into_iter().map(|d| d.diagnostic).collect(),
                Vec::new(),
            );
        }
        module_programs = module_owned.iter().map(|(_, p)| p).collect();
    }
    let entry_program: &Program = entry_owned.as_ref().unwrap_or(&entry_ast.0.program);

    let dep_refs: Vec<&Program> = dep_programs.iter().collect();
    // Strictness comes from the workspace, not from who is asking: a workspace built by a caller
    // that resolved the dependency graph carries the native roots and adjudicates a foreign import
    // root exactly as `noeta check` does; one built without a graph (a scratch buffer, a synthetic
    // program) stays lenient and flags only a missing intra-project module. See
    // [`Workspace::native_roots`] for why that used to differ per surface.
    let result = noeta_loader::link_parsed_with_deps(
        &entry_source,
        entry_program,
        &module_programs,
        &dep_refs,
        &broken_refs,
        ws.native_roots(db).0.as_deref(),
    );
    let noeta_loader::Linkage {
        mut program,
        source_maps,
    } = match result {
        Ok(linkage) => linkage,
        // The link itself failed (before expansion). No reads yet.
        Err(load) => return unlinked(load.into_iter().map(|d| d.diagnostic).collect(), Vec::new()),
    };

    // Compile-time directive expansion, through the loader's single decision point. The sources are
    // handed over as a **provider**: reconstructing every member's `Source` clones its whole text, a
    // price only a program that actually expands should pay (the guard inside `run_expansion` runs
    // first, and returns before calling this for every program without an expanding directive).
    //
    // The first unused `SourceId` is the member count plus the dependency-module count, which is the
    // workspace's id layout: members occupy `0..members.len()` and dependency modules continue past
    // them (the editor's `WorkspaceCache` writes the same layout down in its `first_dep_id`). Nothing
    // here re-derives an offset; both counts are read off the workspace.
    let members = ws.members(db);
    let dep_modules = ws.dep_modules(db);
    let next_id = (members.len() + dep_modules.len()) as u32;
    let expansion = noeta_loader::run_expansion(
        &mut program,
        &source_maps,
        || {
            members
                .iter()
                .copied()
                .chain(dep_modules.iter().map(|dm| dm.src(db)))
                .map(|src| source_of(db, src))
                .collect()
        },
        next_id,
        edition_of(db, entry),
        &workspace_text_tiers_union(db, ws),
    );
    let (expansions, reads, diagnostics) = expansion;
    // A failed expansion fails the link, exactly as it does under `noeta run`/`noeta check` —
    // silently checking a program without the members it declares is the divergence this whole seam
    // exists to prevent. The E0062 diagnostic blames the directive's own span, in the user's file,
    // so the per-document view already renders it. But the `reads` survive the failure and travel
    // even here: the commonest failure is a spec that does not exist *yet*, and the file's later
    // appearance can only re-run the expansion if the watcher was told to watch it.
    if !diagnostics.is_empty() {
        return unlinked(
            diagnostics.into_iter().map(|d| d.diagnostic).collect(),
            reads,
        );
    }
    LinkedProgram {
        program: Ok(program),
        expansions,
        reads,
    }
}

/// A link that produced no program: the diagnostics, and no expansions (nothing was generated —
/// expansion runs only over a program that linked). `reads` may still be non-empty when the failure
/// *was* the expansion — a hook that read a spec and then failed still reported the spec.
fn unlinked(diagnostics: Vec<Diagnostic>, reads: Vec<String>) -> LinkedProgram {
    LinkedProgram {
        program: Err(diagnostics),
        expansions: Vec::new(),
        reads,
    }
}

/// Every source in a workspace that fails to lex/parse, and so is **missing from the link pool**.
///
/// Split by role, because the two are treated differently and by different consumers:
/// - `members` are the workspace's own `.noe` files. Each is checked in its own right (the editor
///   publishes its parse error against it directly), so a broken member is not the *entry's* error —
///   it only re-attributes an unresolved `use`.
/// - `deps` are dependency-package modules. Those are never anyone's entry, so nothing else will
///   ever report them and [`linked_from`] makes them a hard error.
///
/// Entry-independent, so one memo serves every entry in the directory — and it is the seam the IDE
/// reads to explain a `use` at the *consumer's* span (see `noeta_ide`), which is not something the
/// linked program's diagnostics can express: their spans belong to the broken file, and a
/// per-document diagnostics view must filter to its own source.
#[derive(Debug, Clone, Default)]
pub struct BrokenModules {
    pub members: Vec<noeta_loader::BrokenModule>,
    pub deps: Vec<noeta_loader::BrokenModule>,
}

replace_update!(BrokenModules);

impl BrokenModules {
    /// Whether nothing in the workspace is broken — the overwhelmingly common case, and the cheap
    /// early-out for consumers that would otherwise walk a document's `use` statements.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty() && self.deps.is_empty()
    }

    /// The broken module a `use <path>.<name>` names, whichever role it plays — the single
    /// matching rule, borrowed from the linker so the IDE's explanation and the linker's
    /// attribution can never disagree about which file a `use` is about.
    pub fn for_use(&self, path: &[String], name: &str) -> Option<&noeta_loader::BrokenModule> {
        noeta_loader::broken_module_for(self.members.iter().chain(&self.deps), path, name)
    }
}

/// [`BrokenModules`] for `ws`: every member and dependency module whose lex/parse failed, with the
/// namespace it declares recovered from its tokens (`noeta_loader::namespace_from_tokens`).
#[salsa::tracked(returns(ref))]
pub fn broken_modules(db: &dyn salsa::Database, ws: Workspace) -> BrokenModules {
    let collect = |src: SourceProgram| -> Option<noeta_loader::BrokenModule> {
        // Note: a broken module's namespace comes from its TOKENS — it has no usable AST.
        let toks = tokens_in(db, ws, src);
        let parsed = ast_in(db, ws, src);
        let diagnostics: Vec<Diagnostic> = toks
            .0
            .diagnostics
            .iter()
            .chain(parsed.0.diagnostics.iter())
            .cloned()
            .collect();
        if diagnostics.is_empty() {
            return None;
        }
        let source = source_of(db, src);
        let namespace = noeta_loader::namespace_from_tokens(&source, &toks.0.tokens);
        Some(noeta_loader::BrokenModule {
            source,
            namespace,
            diagnostics,
        })
    };
    BrokenModules {
        members: ws.members(db).iter().filter_map(|&m| collect(m)).collect(),
        deps: ws
            .dep_modules(db)
            .iter()
            .filter_map(|&dm| {
                let mut module = collect(dm.src(db))?;
                // A dependency's modules are addressed by the CONSUMER's dependency key, not the
                // package's own root (`namespace greet.hello` is imported as `use hi.hello.…`), so
                // the recovered namespace is re-rooted exactly as `reroot_program` re-roots a
                // parsed one. Without this a broken dependency module could never match the `use`
                // that names it.
                if let Some(ns) = module.namespace.as_mut() {
                    noeta_loader::reroot_path(
                        ns,
                        dm.root(db),
                        dm.prefix(db),
                        &reroot_map(dm.renames(db)),
                    );
                }
                Some(module)
            })
            .collect(),
    }
}

/// Classic single-entry link: [`linked_from`] the workspace's first member (the conventional
/// entry — see [`workspace_entry`]). The compile-path surface; behavior-identical to the
/// pre-entry-parametric query for every [`workspace`]-constructed workspace.
pub fn linked(db: &dyn salsa::Database, ws: Workspace) -> &LinkedProgram {
    linked_from(db, ws, workspace_entry(db, ws))
}

/// **The whole workspace's [`Provenance`](noeta_check::Provenance) — the one query every consumer
/// should call.**
///
/// [`workspace_editions`], [`workspace_packages`] and `Workspace::package_uses` are the three halves
/// (yes, three) of one answer, and asking for them separately is how `noeta-ide`'s impact session and
/// `noeta-mcp`'s `test` tool each ended up passing two of the three: an empty `uses` does not read as
/// "unknown", it reads as *no package binds any `@name`*, and every `@directive` in the project then
/// reports a spurious `E0036`. One query, so there is nothing to half-ask for.
pub fn workspace_provenance(db: &dyn salsa::Database, ws: Workspace) -> noeta_check::Provenance {
    workspace_provenance_memo(db, ws).0.clone()
}

/// [`workspace_provenance`] as a **memo**. It is a fold over every member and dependency module —
/// two maps built by hand plus a clone of the `@name` tables — and it is read by every query that
/// checks or activates anything, so recomputing it per query execution made a project sweep rebuild
/// it once per entry per shape. Backdating: adding a file changes it, editing one does not.
#[salsa::tracked(returns(ref))]
fn workspace_provenance_memo(db: &dyn salsa::Database, ws: Workspace) -> WorkspaceProvenance {
    WorkspaceProvenance(noeta_check::Provenance::of(
        workspace_editions(db, ws),
        workspace_packages(db, ws),
        ws.package_uses(db).0.clone(),
    ))
}

/// [`noeta_check::Provenance`] as a memoized query output. A newtype for the same
/// [`salsa::Update`]/orphan reason as [`WorkspaceUses`].
#[derive(Debug, Clone, PartialEq)]
struct WorkspaceProvenance(noeta_check::Provenance);

backdating_update!(WorkspaceProvenance);

/// The per-source [`EditionMap`](noeta_lexer::EditionMap) for a whole workspace — every member
/// source (and dependency module) under its own package's edition, keyed by `SourceId`. The salsa
/// analogue of the loader's `Linked::editions`, so [`linked_checked_from`] applies each package's
/// edition per declaration over the merged program. Entry-independent: the map covers all members.
/// Public so a consumer that checks a *derived* program (e.g. `noeta-mcp` re-checking a
/// tier-activated linked program) can apply the same per-source editions — the `SourceId`s survive
/// activation, so the map stays valid.
///
/// # Why this one is *not* memoized, unlike [`workspace_packages`]
///
/// It has the same shape — a fold over every member building an N-entry map — and that shape made
/// `workspace_packages` quadratic. What made *that* quadratic was not the shape but a **second
/// caller**: `source_text_tiers` asked it which package one source belongs to, and that query runs
/// once per source, so a directory of N files rebuilt an N-entry map N times.
///
/// This function has exactly one caller, [`workspace_provenance_memo`], which is itself
/// `#[salsa::tracked]` — so it already runs once per workspace and a memo of its own would buy
/// nothing while costing another salsa entry on every check.
///
/// **The invariant to preserve is the caller count, not the memo.** Adding a caller that runs per
/// *source* (or per entry, or per shape) reintroduces exactly the quadratic that `workspace_packages`
/// had; the fix then is to memoize this the way `workspace_package_map` is, and to have the
/// per-source caller read the memo rather than this wrapper.
pub fn workspace_editions(db: &dyn salsa::Database, ws: Workspace) -> noeta_lexer::EditionMap {
    let mut map = noeta_lexer::EditionMap::new();
    for src in ws
        .members(db)
        .iter()
        .copied()
        .chain(ws.dep_modules(db).iter().map(|dm| dm.src(db)))
    {
        map.set(SourceId(src.id(db)), edition_of(db, src));
    }
    map
}

/// The per-source [`PackageMap`](noeta_span::PackageMap) for a whole workspace — every member
/// source under the root package, every dependency module under its own package's global key. The
/// salsa analogue of the loader's `Linked::packages`, so [`linked_checked_from`] can enforce the
/// package orphan rule over the merged program. Entry-independent, like [`workspace_editions`], and
/// public for the same reason: a consumer re-checking a *derived* program (a tier-activated one,
/// say) keeps the same `SourceId`s and so the same map.
pub fn workspace_packages(db: &dyn salsa::Database, ws: Workspace) -> noeta_span::PackageMap {
    workspace_package_map(db, ws).0.clone()
}

/// [`workspace_packages`]' memoized body — a newtype for the same [`salsa::Update`]/orphan reason as
/// [`RenamedTextTiers`], backdating so re-deriving an identical map does not invalidate every lex.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct WorkspacePackageMap(noeta_span::PackageMap);

backdating_update!(WorkspacePackageMap);

/// The workspace package map, built **once per workspace** rather than once per consumer.
///
/// Memoized because [`source_text_tiers`] asks it which package one source belongs to, and
/// `source_text_tiers` runs once per source ([`tokens_in`] is tracked per `(ws, src)`) — so an
/// unmemoized rebuild made a whole-directory lex quadratic in the member count. Measured on a
/// directory of N siblings: `PackageMap::set` plus its hashing and table growth was **37% of the
/// whole `noeta check`** at N=384, and the run grew as ~480·N² instructions — one
/// `HashMap<SourceId, PackageOrigin>` insert per *pair* of sources, which is exactly N rebuilds of
/// an N-entry map. Nothing here depends on the entry, so one map per workspace is all that is ever
/// needed.
#[salsa::tracked(returns(ref))]
fn workspace_package_map(db: &dyn salsa::Database, ws: Workspace) -> WorkspacePackageMap {
    let mut map = noeta_span::PackageMap::new();
    for src in ws.members(db).iter().copied() {
        map.set(SourceId(src.id(db)), noeta_span::PackageOrigin::Root);
    }
    for dm in ws.dep_modules(db).iter() {
        map.set(
            SourceId(dm.src(db).id(db)),
            noeta_span::PackageOrigin::Dependency(dm.import_key(db).to_string()),
        );
    }
    WorkspacePackageMap(map)
}

/// Type-check the program linked from `entry` — the workspace analogue of [`checked`], memoized
/// per `(ws, entry)`. A load failure carries its diagnostics straight through (there is no
/// program to check).
#[salsa::tracked(returns(ref))]
pub fn linked_checked_from(
    db: &dyn salsa::Database,
    ws: Workspace,
    entry: SourceProgram,
) -> Checked {
    match &linked_from(db, ws, entry).program {
        // The shared helper maps every checker output field — both the LSP track's
        // `expr_types`/`f32_literal_sites` and the prelude-redesign handle-site maps.
        Ok(program) => from_check_output(noeta_check::check_all_cancellable(
            program,
            noeta_check::CheckOptions::for_workspace(workspace_provenance(db, ws)),
            &|| db.unwind_if_revision_cancelled(),
        )),
        Err(diags) => Checked {
            diagnostics: diags.clone(),
            expr_types: std::collections::HashMap::new(),
            sites: noeta_check::Sites::default(),
            bundle_bindings: std::collections::HashMap::new(),
            packed_layouts: std::collections::HashMap::new(),
            method_receivers: std::collections::HashMap::new(),
        },
    }
}

/// Classic single-entry check: [`linked_checked_from`] the workspace's first member.
pub fn linked_checked(db: &dyn salsa::Database, ws: Workspace) -> &Checked {
    linked_checked_from(db, ws, workspace_entry(db, ws))
}

/// The IDE-flavored whole-workspace check: like [`linked_checked_from`], but the result's
/// [`Checked::expr_types`] is populated (via [`noeta_check::check_all_with_types`]) — the merged,
/// multi-file span→type index the LSP reads for cross-module hover and member navigation. The
/// compile path stays on [`linked_checked_from`] and never builds the index.
#[salsa::tracked(returns(ref))]
pub fn linked_checked_ide_from(
    db: &dyn salsa::Database,
    ws: Workspace,
    entry: SourceProgram,
) -> Checked {
    match &linked_from(db, ws, entry).program {
        Ok(program) => from_check_output(noeta_check::check_all_cancellable(
            program,
            noeta_check::CheckOptions::for_workspace(workspace_provenance(db, ws))
                .with_expr_types(),
            &|| db.unwind_if_revision_cancelled(),
        )),
        Err(diags) => Checked {
            diagnostics: diags.clone(),
            expr_types: std::collections::HashMap::new(),
            sites: noeta_check::Sites::default(),
            bundle_bindings: std::collections::HashMap::new(),
            packed_layouts: std::collections::HashMap::new(),
            method_receivers: std::collections::HashMap::new(),
        },
    }
}

// ---------------------------------------------------------------------------
// The dev-tier shapes of an entry (the tier-aware check)
// ---------------------------------------------------------------------------
//
// `linked_checked_from` checks exactly ONE shape of a file: the stripped, shipping one. A
// `@test { … }` block is dropped before the checker sees it, so its body's type error is
// invisible to every consumer of that query — which is why a green `noeta check` could be
// followed by a `noeta test` that does not compile.
//
// `entry_diagnostics` is the whole answer to **"which shapes of one entry"**, and every surface
// reads it: `noeta check`'s project walk, the editor's per-document publish, the MCP `check` tool.
// The three differ in *which entries* they sweep and in nothing else — a surface that grew its own
// shape list is precisely the drift this family exists to make impossible.
//
// ```text
//   entry_diagnostics(ws, entry, selection, flavor)
//        └─ entry_shapes(ws, entry, selection)
//             ├─► []      ─► linked_checked_from / linked_checked_ide_from   (shipping shape)
//             │              (which one is `flavor` — see `CheckFlavor`)
//             └─► [tier]  ─► shape_activated_from(ws, entry, shape)  (backdates)
//                                └─► shape_checked_from(ws, entry, shape)
//
//   entry_code_tiers(ws, entry) -> Vec<String>   (backdates; what `entry_shapes` sweeps)
// ```
//
// Four properties make this affordable on the editor's per-keystroke path:
//
// 1. **It is its own query family.** Hover, inlay hints, completion and semantic tokens read
//    `linked_checked_ide_from` and pay nothing for it; only the diagnostics publish walks it.
// 2. **An entry with no code-tier block pays nothing** — `entry_code_tiers` is an AST walk over an
//    already-linked program and comes back empty for nearly every file, and no check runs.
// 3. **A tier pass records no `expr_types`.** It exists to produce diagnostics, so it runs the
//    cheaper compile-flavored options, not the IDE-flavored ones.
// 4. **`shape_activated_from` backdates.** Activation for tier T *drops* every other tier's block,
//    so an edit inside a `@bench` body leaves the `@test`-activated program byte-identical and
//    salsa skips the `@test` check entirely; so does an edit that does not change the AST at all.
//    Activation re-runs (its input, `linked_from`, never backdates) but that is one AST walk, and
//    the expensive half — the type check — is what gets skipped.
//
// One tier per *swept* pass, never a joint one: no build compiles `@test` and `@bench` together,
// and a joint pass would invent collisions between two blocks' same-named helpers. A multi-name
// shape reaches `shape_activated_from` only from a caller that explicitly asked for the union
// (`noeta check --tier a --tier b`, `--target`), which is a build that really does exist.

/// One dev tier's **activated** shape of an entry: the program that tier's blocks build, with every
/// other tier's block dropped — plus activation's own diagnostics (`E0036` for an unknown tier).
///
/// `program` is `None` when the entry did not link (there is nothing to activate); the shipping
/// shape's query already carries the link diagnostics, so this contributes none.
#[derive(Debug, Clone, PartialEq)]
pub struct TierActivated {
    pub program: Option<Program>,
    pub diagnostics: Vec<Diagnostic>,
}

// Backdating, deliberately — this is the narrowing that makes the editor path affordable (see
// above): an edit that leaves *this* tier's shape unchanged must not re-run its type check.
backdating_update!(TierActivated);

/// Every **code** tier the entry's own `@<tier> { … }` blocks name — the shapes besides the
/// shipping one that a tool can still build out of this source, in first-appearance order.
///
/// The salsa form of [`noeta_check::code_tiers_in`], so the editor and the MCP tool ask the same
/// question the CLI's `noeta check` asks. Text tiers (`@doc`, any `text:` tier), expression tiers,
/// and a *dependency's* blocks contribute none — a text/expression body holds no statements to
/// type-check, and a dependency's tiers are not this entry's to check.
///
/// Backdates: the value changes only when a tier block is added, removed or renamed, so ordinary
/// editing inside a block leaves it equal and its readers stay memoized.
#[salsa::tracked(returns(ref))]
pub fn entry_code_tiers(
    db: &dyn salsa::Database,
    ws: Workspace,
    entry: SourceProgram,
) -> Vec<String> {
    match &linked_from(db, ws, entry).program {
        Ok(program) => noeta_check::code_tiers_in(program, &workspace_provenance(db, ws)),
        Err(_) => Vec::new(),
    }
}

/// The entry's program with **exactly the tiers in `shape`** live — the shape `noeta test`/
/// `noeta bench`/`noeta <tier>` (one name) or an explicit `--tier a --tier b`/`--target` selection
/// (several) compiles. See the module-level note on why this is its own (backdating) query.
///
/// The sweep only ever asks for one name at a time; a multi-name shape comes from a caller that
/// *chose* the union, which is the one case where blocks of two tiers legitimately compile
/// together.
#[salsa::tracked(returns(ref))]
pub fn shape_activated_from(
    db: &dyn salsa::Database,
    ws: Workspace,
    entry: SourceProgram,
    shape: Vec<String>,
) -> TierActivated {
    match &linked_from(db, ws, entry).program {
        Ok(program) => {
            let names: Vec<&str> = shape.iter().map(String::as_str).collect();
            let activated =
                noeta_check::activate_tiers(program, &names, &workspace_provenance(db, ws));
            TierActivated {
                program: Some(activated.program),
                diagnostics: activated.diagnostics,
            }
        }
        Err(_) => TierActivated {
            program: None,
            diagnostics: Vec::new(),
        },
    }
}

/// Type-check the entry as `shape` builds it — the tier-aware analogue of [`linked_checked_from`],
/// memoized per `(ws, entry, shape)`.
///
/// The `SourceId`s survive activation, so the workspace's edition and package maps stay valid over
/// the derived program (that is why [`workspace_editions`]/[`workspace_packages`] are public).
/// Deliberately *not* the IDE flavor: this feeds diagnostics only, and an `expr_types` index over a
/// shape the user is not editing would be paid for on every keystroke and read by nothing.
#[salsa::tracked(returns(ref))]
pub fn shape_checked_from(
    db: &dyn salsa::Database,
    ws: Workspace,
    entry: SourceProgram,
    shape: Vec<String>,
) -> Checked {
    let activated = shape_activated_from(db, ws, entry, shape);
    let Some(program) = &activated.program else {
        return Checked {
            diagnostics: Vec::new(),
            expr_types: std::collections::HashMap::new(),
            sites: noeta_check::Sites::default(),
            bundle_bindings: std::collections::HashMap::new(),
            packed_layouts: std::collections::HashMap::new(),
            method_receivers: std::collections::HashMap::new(),
        };
    };
    let mut checked = from_check_output(noeta_check::check_all_cancellable(
        program,
        noeta_check::CheckOptions::for_workspace(workspace_provenance(db, ws)),
        &|| db.unwind_if_revision_cancelled(),
    ));
    // Activation's own `E0036` (a block naming an unknown tier) is part of what this shape reports.
    let mut diagnostics = activated.diagnostics.clone();
    diagnostics.append(&mut checked.diagnostics);
    checked.diagnostics = diagnostics;
    checked
}

/// Which flavor of the **shipping** check a caller needs — the one axis on which the surfaces may
/// legitimately differ, because it changes what is *recorded*, never what is *reported*.
///
/// [`Ide`](CheckFlavor::Ide) additionally builds the span→type index hover, inlay hints and
/// completion read, so the editor's diagnostics ride the memo those features already paid for
/// instead of running a second check on the same keystroke. The diagnostics are identical either
/// way (`with_expr_types` only adds recording), which is what makes this a flavor rather than a
/// second answer to the question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckFlavor {
    /// The compile-path check: diagnostics only, no `expr_types` index.
    Compile,
    /// The editor's check: additionally records `expr_types`.
    Ide,
}

/// **Every shape of `entry` that a "is this clean?" answer must cover**, in check order — the one
/// place that question is answered, for `noeta check`, the LSP and the MCP `check` tool alike.
///
/// The first shape is the caller's explicit selection (`--tier`/`--target`; empty for the stripped
/// shape that ships, which is what the editor and the agent ask for). After it, one shape per code
/// tier the entry's *own* blocks name that the selection did not already make live — the exact
/// shape `noeta test`/`noeta bench`/`noeta <tier>` compiles.
///
/// One tier per swept shape, never a joint one: no build compiles `@test` and `@bench` together,
/// and a joint pass would invent collisions between two blocks' same-named helpers.
pub fn entry_shapes(
    db: &dyn salsa::Database,
    ws: Workspace,
    entry: SourceProgram,
    selection: &[String],
) -> Vec<Vec<String>> {
    let mut shapes = vec![selection.to_vec()];
    for tier in entry_code_tiers(db, ws, entry) {
        if !selection.iter().any(|s| s == tier) {
            shapes.push(vec![tier.clone()]);
        }
    }
    shapes
}

/// **The diagnostics of every shape of `entry`** ([`entry_shapes`]), deduplicated on
/// [`diagnostic_key`] so a fault outside any tier block — which every shape reports — appears once.
///
/// This is the whole of "which shapes of one entry", and it is deliberately the *only* answer:
/// `noeta check`, the editor and the MCP tool differ in **which entries** they sweep (a project
/// walk, the open document, the requested file) and in nothing else. A surface that grew its own
/// shape list is exactly the drift this function exists to make impossible.
///
/// A plain function, not a tracked query: it is a loop over already-memoized per-shape results, and
/// a memo of its own would only add a second copy of the diagnostics that can never backdate. An
/// entry with no code-tier block — nearly every file — pays for one AST walk and no extra check.
pub fn entry_diagnostics(
    db: &dyn salsa::Database,
    ws: Workspace,
    entry: SourceProgram,
    selection: &[String],
    flavor: CheckFlavor,
) -> Vec<Diagnostic> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for shape in entry_shapes(db, ws, entry, selection) {
        // An empty shape is the shipping program itself — no activation to run, and the flavored
        // query the caller's other features already read.
        let diagnostics = if shape.is_empty() {
            match flavor {
                CheckFlavor::Compile => &linked_checked_from(db, ws, entry).diagnostics,
                CheckFlavor::Ide => &linked_checked_ide_from(db, ws, entry).diagnostics,
            }
        } else {
            &shape_checked_from(db, ws, entry, shape).diagnostics
        };
        for diagnostic in diagnostics {
            if seen.insert(diagnostic_key(diagnostic)) {
                out.push(diagnostic.clone());
            }
        }
    }
    out
}

/// The identity a diagnostic is deduplicated by when several *shapes* of one entry are checked:
/// where it is and what it is, never which pass produced it. The same key `noeta check` folds its
/// per-tier passes into, so the CLI, the MCP tool and the editor never disagree about how many
/// times one fault is reported.
pub fn diagnostic_key(d: &Diagnostic) -> (SourceId, u32, u32, &'static str) {
    (d.span.source, d.span.start, d.span.end, d.code.code())
}

/// Classic single-entry ide check: [`linked_checked_ide_from`] the workspace's first member.
pub fn linked_checked_ide(db: &dyn salsa::Database, ws: Workspace) -> &Checked {
    linked_checked_ide_from(db, ws, workspace_entry(db, ws))
}

/// Compile the program linked from `entry` to a [`Module`] — the workspace analogue of
/// [`bytecode`], memoized per `(ws, entry)`. Callers gate on [`linked_from`] being `Ok` (and
/// [`linked_checked_from`] being empty) before reaching a real `Module`; when the link failed
/// there is nothing to compile, so an empty program stands in (a valid, never-observed `Module`).
#[salsa::tracked(returns(ref))]
pub fn linked_bytecode_from(
    db: &dyn salsa::Database,
    ws: Workspace,
    entry: SourceProgram,
) -> Bytecode {
    match &linked_from(db, ws, entry).program {
        Ok(program) => {
            let checked = linked_checked_from(db, ws, entry);
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

/// Classic single-entry compile: [`linked_bytecode_from`] the workspace's first member.
pub fn linked_bytecode(db: &dyn salsa::Database, ws: Workspace) -> &Bytecode {
    linked_bytecode_from(db, ws, workspace_entry(db, ws))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seed the process-default registry with the std units — these tests are their own
    /// assembling driver (audit-6 F2): an unseeded db falls back to the process default for the
    /// extensions' verbatim-tier names, and the checker behind `checked`/`workspace_checked`
    /// resolves std names against the same default.
    fn seed_std() {
        noeta_stdlib::registry::default_seeded();
    }

    fn db_and_src(text: &str) -> (LangDatabase, SourceProgram) {
        seed_std();
        let db = LangDatabase::default();
        let source = Source::new(SourceId::FIRST, "test.noe", text);
        let src = source_program(&db, &source, noeta_lexer::Edition::DEFAULT);
        (db, src)
    }

    #[test]
    fn the_ext_env_input_drives_workspace_text_tiers_and_invalidates() {
        seed_std();
        // The extensions' verbatim-tier set is a real salsa INPUT: seeding it changes the
        // memoized answer, re-seeding invalidates. (Before, tracked queries read the process
        // global directly — a change could never invalidate a memoized parse.)
        let mut db = LangDatabase::default();
        let source = Source::new(SourceId::FIRST, "test.noe", "echo 1;\n");
        let src = source_program(&db, &source, noeta_lexer::Edition::DEFAULT);
        let ws = Workspace::new(
            &db,
            vec![src],
            Vec::new(),
            WorkspaceUses::default(),
            NativeRoots::default(),
        );
        seed_ext_env(
            &mut db,
            noeta_loader::ExtTiers::verbatim_under("std", ["blueprint".to_string()]),
        );
        assert!(
            workspace_text_tiers(&db, ws).contains(&"blueprint".to_string()),
            "the seeded tier set flows into the workspace tier union"
        );
        // Re-seeding the input invalidates the memoized answer.
        seed_ext_env(&mut db, noeta_loader::ExtTiers::default());
        assert!(
            !workspace_text_tiers(&db, ws).contains(&"blueprint".to_string()),
            "re-seeding the ExtEnv input must invalidate"
        );
    }

    #[test]
    fn a_long_check_is_cancelled_mid_module_by_a_concurrent_write() {
        // audit F9 residual (b): a whole-program check is ONE salsa query, so before the
        // per-declaration cancellation poll a pending input write could not take effect until the
        // entire module had been checked. With the poll, a superseded check unwinds promptly.
        seed_std();
        let mut db = LangDatabase::default();
        // A large module whose *check* dominates: many top-level declarations, each a real
        // check/infer unit, so `check_all` runs long enough for a concurrent write to land mid-run.
        let mut text = String::new();
        for i in 0..20_000 {
            text.push_str(&format!(
                "fn f{i}(x: int): int {{\n  y = x + {i}\n  return y * 2\n}}\n"
            ));
        }
        let source = Source::new(SourceId::FIRST, "big.noe", &text);
        let src = source_program(&db, &source, noeta_lexer::Edition::DEFAULT);

        // Warm lex+parse so the reader's `checked` skips straight to `check_all`: the ONLY long,
        // cancellable work left is the checker's per-declaration loop, so an observed cancellation
        // is attributable to the poll under test (not to a lex/parse query boundary).
        let _ = ast(&db, src);

        let reader_db = db.clone();
        let handle = std::thread::spawn(move || {
            // The long check; `catch` absorbs the cancellation unwind into an `Err`.
            salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
                checked(&reader_db, src).diagnostics.len()
            }))
        });

        // Let the reader get into the check, then write — this flags cancellation for the reader's
        // in-flight query and blocks until it unwinds.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let start = std::time::Instant::now();
        {
            use salsa::Setter as _;
            src.set_text(&mut db).to("echo 1;\n".to_string());
        }
        let write_elapsed = start.elapsed();

        let outcome = handle.join().expect("reader thread panicked");
        assert!(
            outcome.is_err(),
            "the in-flight check must unwind with salsa::Cancelled, got {outcome:?}"
        );
        // Prompt: the write returned once the reader aborted at its next per-declaration poll, not
        // after grinding through all 20k declarations.
        assert!(
            write_elapsed < std::time::Duration::from_secs(5),
            "cancellation must be prompt, write blocked for {write_elapsed:?}"
        );
        // The session is not corrupted by the unwind: the next query recomputes cleanly over the
        // new (now-tiny) text.
        assert!(checked(&db, src).diagnostics.is_empty());
    }

    /// A [`LangDatabase`] that records the name of every tracked query salsa actually **executes**
    /// — the only way to prove memoization narrows rather than merely returning the right answer.
    fn logging_db(log: std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> LangDatabase {
        LangDatabase {
            storage: salsa::Storage::new(Some(Box::new(move |event: salsa::Event| {
                if let salsa::EventKind::WillExecute { database_key } = event.kind {
                    log.lock().unwrap().push(format!("{database_key:?}"));
                }
            }))),
        }
    }

    /// How many times a query whose name contains `needle` executed. Non-destructive — the caller
    /// clears the log explicitly, so asking two questions about one revision does not lose the
    /// second answer.
    fn executions(log: &std::sync::Mutex<Vec<String>>, needle: &str) -> usize {
        log.lock()
            .unwrap()
            .iter()
            .filter(|k| k.contains(needle))
            .count()
    }

    /// A file's `@test` and `@bench` blocks are checked as two separate builds — and editing inside
    /// one must not re-check the other.
    ///
    /// This is the property that makes the tier sweep affordable on the editor's per-keystroke path.
    /// `shape_activated_from` *drops* every tier but its own, so an edit inside `@bench` leaves the
    /// `@test`-activated program identical, its backdating `Update` reports "unchanged", and salsa
    /// skips the `@test` type check — the expensive half. Activation itself re-runs (its input,
    /// `linked_from`, never backdates), which is one AST walk.
    #[test]
    fn editing_one_tier_block_does_not_recheck_another() {
        seed_std();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut db = logging_db(std::sync::Arc::clone(&log));
        let program = |bench_body: &str| {
            format!(
                "fn add(a: int, b: int): int {{ return a + b }}\n\n\
                 @test {{\n    fn adds(): void {{ assert(add(1, 2) == 3) }}\n}}\n\n\
                 @bench {{\n    fn adding(): void {{ echo {bench_body} }}\n}}\n"
            )
        };
        let source = Source::new(SourceId::FIRST, "tiered.noe", program("add(1, 2)"));
        let src = source_program(&db, &source, noeta_lexer::Edition::DEFAULT);
        let ws = Workspace::new(
            &db,
            vec![src],
            Vec::new(),
            WorkspaceUses::default(),
            NativeRoots::default(),
        );

        assert_eq!(entry_code_tiers(&db, ws, src), &["test", "bench"]);
        assert!(entry_diagnostics(&db, ws, src, &[], CheckFlavor::Compile).is_empty());
        // Both tier shapes were checked the first time round (the shipping shape is its own query).
        assert_eq!(executions(&log, "shape_checked_from"), 2);

        // An edit *inside the `@bench` block only*.
        log.lock().unwrap().clear();
        {
            use salsa::Setter as _;
            src.set_text(&mut db).to(program("add(2, 3)"));
        }
        assert!(entry_diagnostics(&db, ws, src, &[], CheckFlavor::Compile).is_empty());
        assert_eq!(
            executions(&log, "shape_activated_from"),
            2,
            "activation re-runs for both shapes — it is one AST walk"
        );
        assert_eq!(
            executions(&log, "shape_checked_from"),
            1,
            "only the `@bench` shape changed, so only it may be re-checked"
        );
    }

    /// The overwhelmingly common file declares no code-tier block, and must pay for **no** extra
    /// type check at all — not one per edit, not one ever.
    #[test]
    fn a_file_with_no_tier_block_runs_no_extra_check() {
        seed_std();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut db = logging_db(std::sync::Arc::clone(&log));
        let source = Source::new(SourceId::FIRST, "plain.noe", "echo 1 + 2\n");
        let src = source_program(&db, &source, noeta_lexer::Edition::DEFAULT);
        let ws = Workspace::new(
            &db,
            vec![src],
            Vec::new(),
            WorkspaceUses::default(),
            NativeRoots::default(),
        );

        assert!(entry_diagnostics(&db, ws, src, &[], CheckFlavor::Compile).is_empty());
        assert_eq!(executions(&log, "shape_checked_from"), 0);
        log.lock().unwrap().clear();
        {
            use salsa::Setter as _;
            src.set_text(&mut db).to("echo 2 + 3\n".to_string());
        }
        assert!(entry_diagnostics(&db, ws, src, &[], CheckFlavor::Compile).is_empty());
        assert_eq!(
            executions(&log, "shape_checked_from"),
            0,
            "no code-tier block, no tier check"
        );
    }

    /// A type error inside a `@test` body is invisible to the shipping-shape query (the block is
    /// stripped before the checker sees it) and must be reported by the tier sweep.
    #[test]
    fn the_tier_sweep_sees_a_stripped_blocks_type_error() {
        seed_std();
        let db = LangDatabase::default();
        let source = Source::new(
            SourceId::FIRST,
            "tiered.noe",
            "fn add(a: int, b: int): int { return a + b }\n\n@test {\n    fn adds(): void { n: int = \"lots\" }\n}\n",
        );
        let src = source_program(&db, &source, noeta_lexer::Edition::DEFAULT);
        let ws = Workspace::new(
            &db,
            vec![src],
            Vec::new(),
            WorkspaceUses::default(),
            NativeRoots::default(),
        );

        assert!(
            linked_checked(&db, ws).diagnostics.is_empty(),
            "the shipping shape strips the block — that is the whole point"
        );
        let swept = entry_diagnostics(&db, ws, src, &[], CheckFlavor::Compile);
        assert!(
            swept.iter().any(|d| d.code.code() == "E0007"),
            "the tier sweep must see it; got {swept:?}"
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
        // Self-seed the process-default registry: these module-graph tests run the checker, which
        // resolves against the registry — depending on a sibling test to seed it first is a test-
        // isolation hazard (order is not guaranteed across runs).
        seed_std();
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
            &[],
        );

        let prog = match &linked(&db, ws).program {
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
        seed_std();
        let entry_text = "namespace App.Main;\nuse App.A.Foo;\nf = Foo { x: 1 };\necho f.x;\n";
        let a_text = "namespace App.A;\npub class Foo { x: int }\n";
        let raw = noeta_loader::RawModule::declared("a.noe", a_text);
        let loader = noeta_loader::link(
            "main.noe",
            entry_text,
            noeta_lexer::Edition::DEFAULT,
            std::slice::from_ref(&raw),
            noeta_loader::ModulePath::Declared,
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
            &[],
        );
        let salsa = match &linked(&db, ws).program {
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
        seed_std();
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
        let ws = Workspace::new(
            &db,
            vec![entry_src, a_src, b_src],
            Vec::new(),
            WorkspaceUses::default(),
            NativeRoots::default(),
        );

        assert!(linked(&db, ws).program.is_ok());
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
        assert!(linked(&db, ws).program.is_ok());
    }

    #[test]
    fn one_workspace_serves_two_entries_and_shares_the_parses() {
        // The entry-parametric family (ide-workspaces): TWO members of ONE workspace each link as
        // an entry — memoized per (ws, entry) — while the per-source workspace-aware parse
        // (`ast_in`) memoizes ONCE per file across both links. This is the sharing that lets the
        // editor keep one workspace per directory instead of one per open document.
        seed_std();
        let db = LangDatabase::default();
        let main = Source::new(
            SourceId(0),
            "main.noe",
            "namespace App.Main;\nuse App.A.Foo;\nf = Foo { x: 1 };\necho f.x;\n",
        );
        let a = Source::new(
            SourceId(1),
            "a.noe",
            "namespace App.A;\npub class Foo { pub x: int }\n",
        );
        let main_src = source_program(&db, &main, noeta_lexer::Edition::DEFAULT);
        let a_src = source_program(&db, &a, noeta_lexer::Edition::DEFAULT);
        let ws = Workspace::new(
            &db,
            vec![main_src, a_src],
            Vec::new(),
            WorkspaceUses::default(),
            NativeRoots::default(),
        );

        // Link from `main` (imports the sibling), then from `a` (a library module, no imports).
        let from_main = linked_from(&db, ws, main_src);
        assert!(from_main.program.is_ok(), "{:?}", from_main.program);
        let a_ast_after_first_link = ast_in(&db, ws, a_src) as *const Ast;
        let from_a = linked_from(&db, ws, a_src);
        assert!(from_a.program.is_ok(), "{:?}", from_a.program);

        // The second entry's link did not re-parse the shared member: identical memoized value.
        assert_eq!(
            a_ast_after_first_link,
            ast_in(&db, ws, a_src) as *const Ast,
            "ast_in must memoize once per (ws, src) across entries"
        );
        // The two merges are per-entry: main's merge carries the qualified Foo, a's own merge is
        // just its own declarations.
        let main_prog = from_main.program.as_ref().unwrap();
        assert!(
            main_prog
                .stmts
                .iter()
                .any(|s| matches!(s, noeta_ast::Stmt::Class(c) if c.name == "App.A.Foo"))
        );
        // Both entries check over the shared workspace.
        assert!(
            linked_checked_from(&db, ws, main_src)
                .diagnostics
                .is_empty()
        );
        assert!(linked_checked_from(&db, ws, a_src).diagnostics.is_empty());
        // And the classic surface is exactly "link from the first member".
        assert_eq!(
            linked(&db, ws) as *const LinkedProgram,
            linked_from(&db, ws, main_src) as *const LinkedProgram,
            "`linked` must be the memoized first-member link"
        );
    }

    #[test]
    fn workspace_with_deps_resolves_cross_package_use() {
        // package-manager P2.1c: a dependency package keyed `hi` (its own root segment is `greet`)
        // links into the salsa graph; the entry's `use hi.hello.greeting` resolves after re-root.
        seed_std();
        let db = LangDatabase::default();
        let entry = Source::new(
            SourceId(0),
            "main.noe",
            "use hi.hello.greeting;\necho greeting();\n",
        );
        let dep = DepSources {
            paths: Vec::new(),
            root: "greet".to_string(),
            prefix: vec!["hi".to_string()],
            renames: Vec::new(),
            modules: vec![Source::new(
                SourceId(1),
                "hello.noe",
                "namespace greet.hello;\npub fn greeting(): string { return \"hi\"; }\n",
            )],
            edition: noeta_lexer::Edition::DEFAULT,
        };
        let ws = workspace_with_deps(
            &db,
            &entry,
            &[],
            std::slice::from_ref(&dep),
            &noeta_span::PackageUses::new(),
            noeta_lexer::Edition::DEFAULT,
            &[],
        );
        let linked = linked(&db, ws);
        assert!(
            linked.program.is_ok(),
            "cross-package use must resolve: {:?}",
            linked.program
        );
        // The dependency's `greeting` fn was merged into the linked program (and its `use` no longer
        // survives as an unresolved import).
        let program = linked.program.as_ref().unwrap();
        assert!(
            program
                .stmts
                .iter()
                .any(|s| format!("{s:?}").contains("greeting")),
            "the dependency's greeting declaration must be linked in"
        );
    }

    #[test]
    fn workspace_with_deps_reroots_a_scope_members_intra_package_use() {
        // The salsa twin of the loader's scope-array case: `para/db` reached through
        // `para = [{ package = "para/db" }, … ]` derives under the TWO-segment prefix `para.db`, and
        // its own modules import each other by the segment they derive under standalone (`db`). The
        // query path must splice in the whole prefix, exactly as the batch loader does — a single
        // segment would have made `use db.open` address `para.open`, which is nothing.
        seed_std();
        let db = LangDatabase::default();
        let entry = Source::new(
            SourceId(0),
            "main.noe",
            "use para.db.query.run;\necho run();\n",
        );
        let derived = |segments: &[&str]| {
            noeta_loader::ModulePath::Derived(segments.iter().map(|s| (*s).to_string()).collect())
        };
        let dep = DepSources {
            root: "db".to_string(),
            prefix: vec!["para".to_string(), "db".to_string()],
            renames: Vec::new(),
            modules: vec![
                Source::new(
                    SourceId(1),
                    "db.noe",
                    "pub fn open(): string { return \"conn\"; }\n",
                ),
                Source::new(
                    SourceId(2),
                    "query.noe",
                    "use db.open;\npub fn run(): string { return open(); }\n",
                ),
            ],
            paths: vec![derived(&["para", "db"]), derived(&["para", "db", "query"])],
            edition: noeta_lexer::Edition::DEFAULT,
        };
        let ws = workspace_with_deps(
            &db,
            &entry,
            &[],
            std::slice::from_ref(&dep),
            &noeta_span::PackageUses::new(),
            noeta_lexer::Edition::DEFAULT,
            &[],
        );
        let linked = linked(&db, ws);
        assert!(
            linked.program.is_ok(),
            "a scope member's intra-package `use db.open` must re-root to `para.db.open`: {:?}",
            linked.program
        );
    }

    #[test]
    fn workspace_with_deps_lexes_a_renamed_text_tier_verbatim() {
        // Per-package tier-naming arc (3g), harness seam: the `workspace_with_deps` path (used by
        // `noeta-mcp` and `noeta-conformance`) now carries the whole program's `@name` tables, so a
        // `[tiers] docs = "std:doc"` binding — the root package renaming std's `doc` **text** tier
        // under a local `@docs` — lexes the `@docs { … }` body verbatim, exactly as the loader does
        // under `noeta run`/`noeta check` and the editor does through `sync`. The db twin of the IDE's
        // `workspace_captures_a_renamed_text_tier_bound_in_the_manifest`.
        let mut db = LangDatabase::default();
        // Seed std's `doc` as the known verbatim ext tier the `docs = "std:doc"` binding lands on.
        seed_ext_env(
            &mut db,
            noeta_loader::ExtTiers::verbatim_under("std", ["doc".to_string()]),
        );
        // Load-bearing: the bare `"` (and `<angle>` bits) make this a hard lex error as code; only the
        // per-package text-tier resolution captures it verbatim as one `DocText`.
        let entry = Source::new(
            SourceId(0),
            "main.noe",
            "@docs {\n# Widget\n\nA bare \" quote and <angle> bits: invalid as code, fine as markdown.\n}\nfn add(a: int, b: int): int { return a + b }\n",
        );
        // The root package binds local `@docs` → std's `doc` tier, keyed by `PackageOrigin::Root`
        // (members are Root, see `workspace_packages`) — the same shape `resolve_graph_query` yields
        // for a `[tiers]` table.
        let mut package_uses = noeta_span::PackageUses::new();
        package_uses.set(
            noeta_span::PackageOrigin::Root,
            "docs".to_string(),
            noeta_span::PackageUse {
                provider_roots: vec!["std".to_string()],
                exported: "doc".to_string(),
            },
        );
        let ws = workspace_with_deps(
            &db,
            &entry,
            &[],
            &[],
            &package_uses,
            noeta_lexer::Edition::DEFAULT,
            &[],
        );
        let entry_src = ws.members(&db)[0];
        let toks = tokens_in(&db, ws, entry_src);
        assert!(
            toks.0
                .tokens
                .iter()
                .any(|t| t.kind == noeta_lexer::TokenKind::DocText),
            "the renamed tier's body must be captured as one verbatim DocText token, got {:?}",
            toks.0.tokens.iter().map(|t| t.kind).collect::<Vec<_>>()
        );
    }
}
