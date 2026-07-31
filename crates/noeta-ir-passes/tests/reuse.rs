//! Reuse-threading tests, at the level the pass decides: which self-updates get the in-place-reuse
//! token, and where the own-destructor exclusion set comes from.
//!
//! The exclusion set is a **semantic** gate, not an optimization heuristic — reusing the displaced
//! allocation means its `destruct` block never runs, which the copy-and-destroy baseline runs on
//! every self-update. So the pass has to be right about it even when the IR in hand is a hot-swap
//! fragment that does not carry the class declaration at all.

use noeta_ir::{Block, Decl, Program, Rvalue, Stmt};
use noeta_ir_passes::thread_reuse;
use noeta_span::{Source, SourceId};

fn lower(src: &str) -> Program {
    noeta_stdlib::registry::default_seeded();
    let source = Source::new(SourceId::FIRST, "reuse", src);
    let lexed = noeta_lexer::lex(&source);
    assert!(lexed.diagnostics.is_empty(), "lex: {:?}", lexed.diagnostics);
    let parsed = noeta_parser::parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "parse: {:?}",
        parsed.diagnostics
    );
    noeta_ir::lower(&parsed.program).expect("lowers")
}

fn ambient(names: &[&str]) -> std::collections::HashSet<String> {
    names.iter().map(|n| n.to_string()).collect()
}

/// Whether *any* `Rvalue::Object` of `type_name` in the program carries the reuse token. The
/// programs here have exactly one construction per type that could take it.
fn object_reuse(program: &Program, type_name: &str) -> bool {
    fn in_block(block: &Block, type_name: &str) -> bool {
        block.stmts.iter().any(|stmt| in_stmt(stmt, type_name))
    }
    fn in_stmt(stmt: &Stmt, type_name: &str) -> bool {
        let here = match stmt {
            Stmt::Let {
                rvalue:
                    Rvalue::Object {
                        type_name: name,
                        reuse,
                        ..
                    },
                ..
            } => *reuse && name == type_name,
            Stmt::Decl(Decl::Fn { func, .. }) => in_block(&func.body, type_name),
            Stmt::Decl(Decl::Class(class)) => class
                .methods
                .iter()
                .any(|(_, f)| in_block(&f.body, type_name)),
            _ => false,
        };
        let mut found = here;
        stmt.for_each_child_block(|b| found |= in_block(b, type_name));
        found
    }
    in_block(&program.top, type_name)
}

/// The whole-program baseline in both directions: a self-updated class carrying its own `destruct`
/// is excluded from reuse, the same class without one is not.
#[test]
fn a_whole_program_reads_the_exclusion_set_off_its_own_declarations() {
    let app = |destructor: &str| {
        format!(
            "class Counter {{\n\
             \x20   pub n: int\n\
             {destructor}\
             }}\n\
             fn run(): void {{\n\
             \x20   mut acc = Counter {{ n: 0 }}\n\
             \x20   acc = Counter {{ ...acc, n: 1 }}\n\
             \x20   echo acc.n\n\
             }}\n"
        )
    };
    let with = thread_reuse(
        &lower(&app("    destruct { echo \"drop\" }\n")),
        &ambient(&[]),
    );
    assert!(
        !object_reuse(&with, "Counter"),
        "a class with its own destructor must never self-update in place: reuse would skip the \
         destructor the displaced value is owed"
    );

    let without = thread_reuse(&lower(&app("")), &ambient(&[]));
    assert!(
        object_reuse(&without, "Counter"),
        "a destructor-free class is the reuse pass's whole reason to exist"
    );
}

/// **The hot-swap defect, at the pass.** A swap fragment carries the changed function and nothing
/// else — no `class Counter`, so nothing in the IR says `Counter` has a `destruct` block, and the
/// pass happily reused in place. The observable effect was a swapped body that stopped running a
/// destructor its own cold start runs (`gc/self_update_own_destructor_no_reuse`, and
/// `hotswap.rs::a_swapped_self_update_still_destroys_the_value_it_displaces`). The ambient set is
/// what closes it, and the two calls below differ **only** in that argument.
#[test]
fn a_fragment_takes_the_exclusion_set_from_the_ambient_program() {
    let fragment = lower(
        "fn run(): void {\n\
        \x20   mut acc = Counter { n: 0 }\n\
        \x20   acc = Counter { ...acc, n: 1 }\n\
        \x20   echo acc.n\n\
         }\n",
    );

    assert!(
        object_reuse(&thread_reuse(&fragment, &ambient(&[])), "Counter"),
        "sanity: with nothing ambient the fragment is indistinguishable from a destructor-free \
         program — which is exactly why the fact has to be supplied rather than inferred"
    );
    assert!(
        !object_reuse(&thread_reuse(&fragment, &ambient(&["Counter"])), "Counter"),
        "the enclosing program's own-destructor classes must gate a fragment's self-updates"
    );
}

/// The ambient set only ever *adds* exclusions: an unrelated ambient name leaves a fragment's
/// destructor-free self-update reusing. Without this, the safe repair ("exclude everything a
/// fragment cannot prove") would pass every correctness test while quietly turning a swapped
/// accumulator quadratic.
#[test]
fn an_unrelated_ambient_name_does_not_suppress_reuse() {
    let fragment = lower(
        "fn run(): void {\n\
        \x20   mut acc = Tally { n: 0 }\n\
        \x20   acc = Tally { ...acc, n: 1 }\n\
        \x20   echo acc.n\n\
         }\n",
    );
    assert!(object_reuse(
        &thread_reuse(&fragment, &ambient(&["Counter", "Handle"])),
        "Tally"
    ));
}
