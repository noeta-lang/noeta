//! The query graph: the compile pipeline as a [salsa] database.
//!
//! M1.1 threads the existing straight-line pipeline (lex → parse → compile) through salsa
//! **before** the type checker (M1.7) needs it, so later slices edit a graph rather than
//! rewrite a pipeline. This slice is deliberately *behavior-preserving*: every query is a
//! thin wrapper that calls the existing `lang_lexer::lex` / `lang_parser::parse` /
//! `lang_compiler::compile` function and memoizes the result. The differential oracle proves
//! the wrap changes nothing — the VM still reproduces the tree-walker byte-for-byte.
//!
//! ```text
//!   SourceProgram (input)
//!        │
//!        ▼
//!     tokens(db)  ──►  ast(db)  ──►  checked(db)   ──►  bytecode(db)
//!     (lang-lexer)    (lang-parser)  (lang-check)       (lang-compiler)
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

use lang_ast::Program;
use lang_bytecode::Module;
use lang_compiler::Unsupported;
use lang_diagnostics::Diagnostic;
use lang_lexer::Lexed;
use lang_parser::Parsed;
use lang_span::{Source, SourceId, Span};

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
}

/// Build (or rebuild) the [`SourceProgram`] input from a [`Source`].
pub fn source_program(db: &LangDatabase, source: &Source) -> SourceProgram {
    SourceProgram::new(
        db,
        source.id().0,
        source.name().to_string(),
        source.text().to_string(),
    )
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

/// Type-checker output: the checker's diagnostics (empty ⇒ well-typed) **and** the `type_of`
/// site map, both produced by one `lang_check::check_all` run. Carrying the map here lets the
/// [`bytecode`] query and the eval path read it from this memoized query instead of each
/// re-running the checker (the redundant-passes dedup — the map is a pure function of the AST).
#[derive(Debug, Clone)]
pub struct Checked {
    pub diagnostics: Vec<Diagnostic>,
    pub type_of_sites: std::collections::HashMap<Span, lang_ast::reflect::TypeRepr>,
    /// The `List<packed>` construction-site layout map (P-PACK 2.1), carried here for the same
    /// reason as `type_of_sites`: the eval reference reads it to lay flat lists out identically to
    /// the VM, computed once per check.
    pub packed_list_sites: std::collections::HashMap<Span, lang_ast::reflect::PackedLayout>,
    /// Call-site-typed native-call recipes (`json.parse::<T>`), carried here for the same reason as
    /// `packed_list_sites`: the lowering bakes them into `Rvalue::ExtCall`, computed once per check.
    pub ext_call_sites: std::collections::HashMap<Span, lang_stdlib::TypeRecipe>,
    /// `map(...)` call sites whose result element type is packed (P-PACK 2.6 category B), carried here
    /// for the same reason as `packed_list_sites`: the VM builds a flat `map` result at these spans.
    pub map_packed_sites: std::collections::HashMap<Span, lang_ast::reflect::PackedLayout>,
    /// The fusable `list[i].field` set (P-PACK 2.5+), carried here for the same reason as
    /// `packed_list_sites`: both backends read it to fuse indexed field reads identically.
    pub index_field_sites: std::collections::HashSet<Span>,
    /// Streaming-`for` site set (Track I.2), carried here for the same reason as the other site
    /// maps: the lowering sets `Stmt::For.stream` from it so both backends stream the same loops.
    pub for_stream_sites: std::collections::HashSet<Span>,
    /// Per-binding destructor-relevance (Phase 3.2b), threaded into the compiler so the drop pass
    /// annotates each `DropVar` — carried here for the same reason as `type_of_sites` (compute the
    /// checker's result once, reuse it without a second run).
    pub destructor_relevance: lang_check::DestructorRelevance,
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

/// Tokenize the source. Memoized; re-runs only when `SourceProgram::text` changes.
#[salsa::tracked(returns(ref))]
pub fn tokens(db: &dyn salsa::Database, src: SourceProgram) -> Tokens {
    let source = source_of(db, src);
    Tokens(lang_lexer::lex(&source))
}

/// Parse the token stream into an AST. Depends on [`tokens`].
#[salsa::tracked(returns(ref))]
pub fn ast(db: &dyn salsa::Database, src: SourceProgram) -> Ast {
    let source = source_of(db, src);
    let toks = tokens(db, src);
    Ast(lang_parser::parse(&source, &toks.0.tokens))
}

/// Type-check the AST and return the checker's diagnostics. Depends on [`ast`]. The pipeline's
/// front-end gate: a program with type errors is rejected before either backend runs (so both
/// backends surface the identical compile-time result — see the conformance differential).
#[salsa::tracked(returns(ref))]
pub fn checked(db: &dyn salsa::Database, src: SourceProgram) -> Checked {
    let parsed = ast(db, src);
    let out = lang_check::check_all(&parsed.0.program);
    Checked {
        diagnostics: out.diagnostics,
        type_of_sites: out.type_of_sites,
        packed_list_sites: out.packed_list_sites,
        ext_call_sites: out.ext_call_sites,
        map_packed_sites: out.map_packed_sites,
        index_field_sites: out.index_field_sites,
        for_stream_sites: out.for_stream_sites,
        destructor_relevance: out.destructor_relevance,
    }
}

/// Compile the AST to a [`Module`], or report the first unsupported construct. Depends on [`ast`]
/// and [`checked`] — the latter only to reuse its `type_of_sites` map (which the compiler needs
/// to bake full-fidelity `type_of` constants) rather than re-deriving it. Execution is still
/// gated on `checked`'s diagnostics by the caller; reading the map here does not couple them
/// semantically, it only avoids a second checker run.
#[salsa::tracked(returns(ref))]
pub fn bytecode(db: &dyn salsa::Database, src: SourceProgram) -> Bytecode {
    let parsed = ast(db, src);
    let checked = checked(db, src);
    let sites = checked.type_of_sites.clone();
    let packed = checked.packed_list_sites.clone();
    let map_packed = checked.map_packed_sites.clone();
    let index_fields = checked.index_field_sites.clone();
    let ext = checked.ext_call_sites.clone();
    Bytecode(lang_compiler::compile_with_sites(
        &parsed.0.program,
        sites,
        packed,
        map_packed,
        index_fields,
        ext,
        checked.for_stream_sites.clone(),
        &checked.destructor_relevance,
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
/// [`SourceProgram`] input. Mutating any one source invalidates exactly the queries that read it.
#[salsa::input(debug)]
pub struct Workspace {
    pub entry: SourceProgram,
    #[returns(ref)]
    pub modules: Vec<SourceProgram>,
}

/// Build a [`Workspace`] input from the entry [`Source`] and its sibling module sources (as
/// produced by `lang_loader::read_workspace`). Each becomes a [`SourceProgram`] input.
pub fn workspace(db: &LangDatabase, entry: &Source, modules: &[Source]) -> Workspace {
    let entry_input = source_program(db, entry);
    let module_inputs = modules.iter().map(|s| source_program(db, s)).collect();
    Workspace::new(db, entry_input, module_inputs)
}

/// Resolve and merge the workspace: the entry's imports against the modules' declared namespaces,
/// producing one merged [`Program`] (or the load diagnostics). Depends on every source's [`ast`]
/// (so editing any module re-links), but not on any cross-module edge — the per-source parse
/// queries remain independent. The merge means both backends run the linked program unchanged, so
/// the differential oracle is preserved by construction.
#[salsa::tracked(returns(ref))]
pub fn linked(db: &dyn salsa::Database, ws: Workspace) -> LinkedProgram {
    let entry_src = ws.entry(db);
    let entry_tokens = tokens(db, entry_src);
    let entry_ast = ast(db, entry_src);
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
        let toks = tokens(db, m);
        let parsed = ast(db, m);
        if toks.0.diagnostics.is_empty() && parsed.0.diagnostics.is_empty() {
            module_programs.push(&parsed.0.program);
        }
    }

    match lang_loader::link_parsed(&entry_source, &entry_ast.0.program, &module_programs) {
        Ok(program) => LinkedProgram(Ok(program)),
        Err(load) => LinkedProgram(Err(load.into_iter().map(|d| d.diagnostic).collect())),
    }
}

/// Type-check the linked program — the workspace analogue of [`checked`]. A load failure carries
/// its diagnostics straight through (there is no program to check).
#[salsa::tracked(returns(ref))]
pub fn linked_checked(db: &dyn salsa::Database, ws: Workspace) -> Checked {
    match &linked(db, ws).0 {
        Ok(program) => {
            let out = lang_check::check_all(program);
            Checked {
                diagnostics: out.diagnostics,
                type_of_sites: out.type_of_sites,
                packed_list_sites: out.packed_list_sites,
                ext_call_sites: out.ext_call_sites,
                map_packed_sites: out.map_packed_sites,
                index_field_sites: out.index_field_sites,
                for_stream_sites: out.for_stream_sites,
                destructor_relevance: out.destructor_relevance,
            }
        }
        Err(diags) => Checked {
            diagnostics: diags.clone(),
            type_of_sites: std::collections::HashMap::new(),
            packed_list_sites: std::collections::HashMap::new(),
            ext_call_sites: std::collections::HashMap::new(),
            map_packed_sites: std::collections::HashMap::new(),
            index_field_sites: std::collections::HashSet::new(),
            for_stream_sites: std::collections::HashSet::new(),
            destructor_relevance: lang_check::DestructorRelevance::default(),
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
            let sites = checked.type_of_sites.clone();
            let packed = checked.packed_list_sites.clone();
            let map_packed = checked.map_packed_sites.clone();
            let index_fields = checked.index_field_sites.clone();
            let ext = checked.ext_call_sites.clone();
            Bytecode(lang_compiler::compile_with_sites(
                program,
                sites,
                packed,
                map_packed,
                index_fields,
                ext,
                checked.for_stream_sites.clone(),
                &checked.destructor_relevance,
                false,
            ))
        }
        Err(_) => Bytecode(lang_compiler::compile(&Program {
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
        let source = Source::new(SourceId::FIRST, "test.lang", text);
        let src = source_program(&db, &source);
        (db, src)
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
        let mut slot = Tokens(lang_lexer::Lexed::default());
        let mut replacement = lang_lexer::Lexed::default();
        replacement.tokens.push(lang_lexer::Token {
            kind: lang_lexer::TokenKind::Semicolon,
            span: lang_span::Span::new(0, 1),
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
        let direct = lang_compiler::compile(&ast(&db, src).0.program);
        assert_eq!(bytecode(&db, src).0.is_ok(), direct.is_ok());
    }

    // ----- the module graph (M1.9.3) -----

    #[test]
    fn module_graph_links_checks_and_compiles_a_used_module() {
        let db = LangDatabase::default();
        let entry = Source::new(
            SourceId(0),
            "main.lang",
            "namespace App.Main;\nuse App.A.Foo;\nf = Foo { x: 1 };\necho f.x;\n",
        );
        let a = Source::new(
            SourceId(1),
            "a.lang",
            "namespace App.A;\npub class Foo { pub x: int }\n",
        );
        let ws = workspace(&db, &entry, std::slice::from_ref(&a));

        let prog = match &linked(&db, ws).0 {
            Ok(p) => p,
            Err(e) => panic!("link failed: {e:?}"),
        };
        // The real `Foo` is merged in and its `use` dropped (resolved, no opaque stub).
        assert!(
            prog.stmts
                .iter()
                .any(|s| matches!(s, lang_ast::Stmt::Class(c) if c.name == "Foo"))
        );
        assert!(
            !prog
                .stmts
                .iter()
                .any(|s| matches!(s, lang_ast::Stmt::Use { .. }))
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
        let raw = lang_loader::RawModule {
            name: "a.lang".into(),
            text: a_text.into(),
        };
        let loader =
            lang_loader::link("main.lang", entry_text, std::slice::from_ref(&raw)).unwrap();

        let db = LangDatabase::default();
        let entry = Source::new(SourceId(0), "main.lang", entry_text);
        let a = Source::new(SourceId(1), "a.lang", a_text);
        let ws = workspace(&db, &entry, std::slice::from_ref(&a));
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
            "main.lang",
            "namespace App.Main;\nuse App.A.Foo;\n",
        );
        let a = Source::new(
            SourceId(1),
            "a.lang",
            "namespace App.A;\npub class Foo { pub x: int }\n",
        );
        let b = Source::new(
            SourceId(2),
            "b.lang",
            "namespace App.B;\npub class Bar { y: int }\n",
        );
        let entry_src = source_program(&db, &entry);
        let a_src = source_program(&db, &a);
        let b_src = source_program(&db, &b);
        let ws = Workspace::new(&db, entry_src, vec![a_src, b_src]);

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
}
