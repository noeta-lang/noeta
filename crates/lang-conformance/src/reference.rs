//! The canonical reference runner: execute a program through the **Core-IR interpreter**.
//!
//! At Phase 4 of the memory-management migration the Core-IR interpreter became the language's
//! reference semantics. It is the tree-walk backend that executes the *same* drop-annotated Core
//! IR the bytecode VM compiles, so the two agree on last-use destruction by construction —
//! conformance pins output against this runner, the differential cross-checks it against the VM,
//! and the leak oracle measures its residency.
//!
//! **Phase 7 retired the AST tree-walker as an oracle.** The walker it superseded fires destructors
//! only at global teardown, so it cannot reproduce the reference's last-use destruction once that is
//! observable in any scope — it was kept only as the *total* fallback for programs outside the
//! lowering's subset. The lowering is now total over the parsed language (gate:
//! `ir_lowering_is_total_over_the_corpus`), so that fallback is dead and removed: this runner lowers
//! unconditionally. The `Interpreter` machinery survives as the shared executor of destructor bodies
//! and leaf semantics the IR interpreter reuses, and as the AST-walk baseline the perf benches and
//! property tests drive — neither of which is a reference oracle. The retained cross-checks (the
//! IR-interpreter↔VM differential, the leak oracle on both backends, and the static-≤-dynamic
//! property test) cover its former role; see `plans/memory-management/phase-7-finalize.md` for the
//! decision and rationale.

use std::collections::HashMap;

use lang_ast::Program;
use lang_ast::reflect::TypeRepr;
use lang_backend::RunResult;
use lang_eval::TreeWalkBackend;
use lang_span::Span;

/// Run `program` through the Core-IR interpreter on the given `type_of` sites, lowering it and
/// inserting the precise-RC drops (with destructor relevance) exactly as the bytecode pipeline
/// does — so the reference and the VM consume identical IR. The lowering is total over the parsed
/// language, so every parse+check-clean program reaches the IR path (no AST-walk fallback — Phase 7).
pub fn reference_run(
    program: &Program,
    sites: HashMap<Span, TypeRepr>,
    relevance: &lang_check::DestructorRelevance,
) -> RunResult {
    let ir = lang_ir::lower(program).expect(
        "Core-IR lowering is total over the parsed language \
         (gate: ir_lowering_is_total_over_the_corpus)",
    );
    let ir = lang_ir_passes::insert_drops(&ir, Some(&to_relevance(relevance)));
    // Thread reuse tokens identically to the bytecode pipeline so the reference and the VM consume
    // the same annotated IR (Phase 5).
    let ir = lang_ir_passes::thread_reuse(&ir);
    TreeWalkBackend::new().run_ir(program, &ir, sites)
}

/// The drop pass's relevance form, copied from the checker's (identical sets). Mirrors the
/// compiler's `passes_relevance`, so the IR interpreter and the VM annotate drops identically.
pub fn to_relevance(r: &lang_check::DestructorRelevance) -> lang_ir_passes::Relevance {
    lang_ir_passes::Relevance {
        locals: r.locals.clone(),
        params: r.params.clone(),
    }
}
