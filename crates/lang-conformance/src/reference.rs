//! The canonical reference runner: execute a program through the **Core-IR interpreter**.
//!
//! At Phase 4 of the memory-management migration the Core-IR interpreter became the language's
//! reference semantics. It is the tree-walk backend that executes the *same* drop-annotated Core
//! IR the bytecode VM compiles, so the two agree on last-use destruction by construction —
//! conformance pins output against this runner, the differential cross-checks it against the VM,
//! and the leak oracle measures its residency.
//!
//! The AST tree-walker it superseded (`run_with_sites`) lacks the liveness needed for last-use
//! destruction (it fires destructors only at global teardown), so it cannot be the reference once
//! destruction is observable in any scope. It survives only as a shared executor of destructor
//! bodies and leaf semantics that the IR interpreter reuses, and as the **total** fallback below
//! for any program outside the lowering's subset (none in the corpus today). When the lowering is
//! made total the walker is retired entirely (migration Phase 7).

use std::collections::HashMap;

use lang_ast::Program;
use lang_ast::reflect::TypeRepr;
use lang_backend::RunResult;
use lang_eval::TreeWalkBackend;
use lang_span::Span;

/// Run `program` through the Core-IR interpreter on the given `type_of` sites, lowering it and
/// inserting the precise-RC drops (with destructor relevance) exactly as the bytecode pipeline
/// does — so the reference and the VM consume identical IR. A program outside the lowering's
/// subset falls back to the AST tree-walker (the total reference), preserving totality until the
/// lowering is total and the walker retired.
pub fn reference_run(
    program: &Program,
    sites: HashMap<Span, TypeRepr>,
    relevance: &lang_check::DestructorRelevance,
) -> RunResult {
    match lang_ir::lower(program) {
        Ok(ir) => {
            let ir = lang_ir_passes::insert_drops(&ir, Some(&to_relevance(relevance)));
            TreeWalkBackend::new().run_ir(program, &ir, sites)
        }
        Err(_) => TreeWalkBackend::new().run_with_sites(program, sites),
    }
}

/// The drop pass's relevance form, copied from the checker's (identical sets). Mirrors the
/// compiler's `passes_relevance`, so the IR interpreter and the VM annotate drops identically.
pub fn to_relevance(r: &lang_check::DestructorRelevance) -> lang_ir_passes::Relevance {
    lang_ir_passes::Relevance {
        locals: r.locals.clone(),
        params: r.params.clone(),
    }
}
