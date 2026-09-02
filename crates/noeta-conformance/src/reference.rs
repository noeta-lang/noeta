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
//! property test) cover its former role.

use noeta_ast::Program;
use noeta_backend::RunResult;
use noeta_eval::IrRefBackend;

/// Run `program` through the Core-IR interpreter on the checker's [`Sites`] bundle, lowering it
/// and inserting the precise-RC drops (with destructor relevance) exactly as the bytecode pipeline
/// does — so the reference and the VM consume identical IR. The lowering is total over the parsed
/// language, so every parse+check-clean program reaches the IR path (no AST-walk fallback — Phase 7).
///
/// One bundle field is **deliberately ignored**: [`Sites::map_packed_sites`]. The flat `map`-result
/// layout is a VM representation choice invisible to `RunResult`, so the reference stays boxed there
/// (the field's own doc says so). Every other field is consumed identically to the VM pipeline.
///
/// [`Sites`]: noeta_check::Sites
/// [`Sites::map_packed_sites`]: noeta_check::Sites::map_packed_sites
pub fn reference_run(program: &Program, sites: noeta_check::Sites) -> RunResult {
    crate::ensure_std_registry();
    reference_run_traced(program, sites).0
}

/// As [`reference_run`], plus the abort traceback (empty for a clean run) — the oracle side of the
/// backend trace-parity check.
pub fn reference_run_traced(
    program: &Program,
    sites: noeta_check::Sites,
) -> (RunResult, Vec<noeta_backend::TraceFrame>) {
    crate::ensure_std_registry();
    // Lower with the checker's site maps: packed-list literals stream into a flat buffer
    // and `list[i].field` reads fuse to `Rvalue::IndexField`. Both ride on the IR, so
    // `run_ir` needs no map (the VM compiles the same).
    let ir = noeta_ir::lower_with_sites(program, noeta_ir::lowering_sites!(sites)).expect(
        "Core-IR lowering is total over the parsed language \
         (gate: ir_lowering_is_total_over_the_corpus)",
    );
    let ir = noeta_ir_passes::insert_drops(&ir, Some(&to_relevance(&sites.destructor_relevance)));
    // Thread reuse tokens identically to the bytecode pipeline so the reference and the VM consume
    // the same annotated IR (Phase 5). A whole program, so the pass derives the own-destructor gate
    // from the class declarations the IR carries and nothing is ambient to it.
    let ir = noeta_ir_passes::thread_reuse(&ir, &std::collections::HashSet::new());
    let deserialize_recipes = sites.deserialize_recipes.iter().cloned().collect();
    let packed_type_layouts = packed_layouts_by_name(&sites);
    IrRefBackend::new().run_ir_traced(
        program,
        &ir,
        sites.type_of_sites,
        deserialize_recipes,
        packed_type_layouts,
    )
}

/// The checker's `@packed` layouts keyed by type name — the tree-walker's `make_packed` resolves a
/// produced list's element schema through this map (Slice E2), the twin of the VM interning the same
/// layouts into `packed_schemas`. Built from the same [`Sites::packed_type_layouts`] the compiler
/// interns, so both backends see identical schemas by construction.
fn packed_layouts_by_name(
    sites: &noeta_check::Sites,
) -> std::collections::HashMap<String, noeta_ast::reflect::PackedLayout> {
    sites
        .packed_type_layouts
        .iter()
        .map(|l| (l.type_name.clone(), l.clone()))
        .collect()
}

/// As [`reference_run`], but against a caller-provided [`noeta_stdlib::Host`] — the telemetry
/// parity oracle's entry: spans are write-only (invisible to `RunResult`), so proving both backends
/// *parent* spans identically needs a host whose recorder the test can observe (a `SandboxHost`
/// with a span sink installed). Same lower→drops→reuse recipe as the oracle run.
pub fn reference_run_with_host(
    program: &Program,
    sites: noeta_check::Sites,
    host: Box<dyn noeta_stdlib::Host>,
) -> RunResult {
    crate::ensure_std_registry();
    let ir = noeta_ir::lower_with_sites(program, noeta_ir::lowering_sites!(sites)).expect(
        "Core-IR lowering is total over the parsed language \
         (gate: ir_lowering_is_total_over_the_corpus)",
    );
    let ir = noeta_ir_passes::insert_drops(&ir, Some(&to_relevance(&sites.destructor_relevance)));
    let ir = noeta_ir_passes::thread_reuse(&ir, &std::collections::HashSet::new());
    let deserialize_recipes = sites.deserialize_recipes.iter().cloned().collect();
    let packed_type_layouts = packed_layouts_by_name(&sites);
    IrRefBackend::new().run_ir_with_host(
        program,
        &ir,
        host,
        sites.type_of_sites,
        deserialize_recipes,
        packed_type_layouts,
    )
}

/// The drop pass's relevance form, copied from the checker's (identical sets). Mirrors the
/// compiler's `passes_relevance`, so the IR interpreter and the VM annotate drops identically.
pub fn to_relevance(r: &noeta_check::DestructorRelevance) -> noeta_ir_passes::Relevance {
    noeta_ir_passes::Relevance {
        locals: r.locals.clone(),
        params: r.params.clone(),
    }
}
