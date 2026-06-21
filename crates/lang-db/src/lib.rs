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
//!     tokens(db)  ──►  ast(db)  ──►  bytecode(db)
//!     (lang-lexer)    (lang-parser)  (lang-compiler)
//! ```
//!
//! The checker (`checked_ast`) will slot in between [`ast`] and [`bytecode`] without
//! re-threading anything — that is the whole point of landing the plumbing now.
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

use lang_bytecode::Module;
use lang_compiler::Unsupported;
use lang_lexer::Lexed;
use lang_parser::Parsed;
use lang_span::{Source, SourceId};

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

/// Compiler output: a [`Module`], or the first construct outside the VM's subset.
#[derive(Debug, Clone)]
pub struct Bytecode(pub Result<Module, Unsupported>);

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
replace_update!(Bytecode);

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

/// Compile the AST to a [`Module`], or report the first unsupported construct. Depends on
/// [`ast`]. The type-checker query (M1.7) will be inserted between [`ast`] and here.
#[salsa::tracked(returns(ref))]
pub fn bytecode(db: &dyn salsa::Database, src: SourceProgram) -> Bytecode {
    let parsed = ast(db, src);
    Bytecode(lang_compiler::compile(&parsed.0.program))
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
}
