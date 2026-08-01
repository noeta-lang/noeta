//! **The `RunOptions` census**: every option a run can vary is consumed by the one core runner, and
//! every `run_module_*` entry point reaches that core.
//!
//! ## The bug class
//!
//! [`VmBackend::run_module_with`] is a real core, not a dispatcher: nine-ish mandatory init steps in
//! a fixed order — collector mode, the safepoint-GC arm, the debug-session arena, the hot-mailbox
//! consumer cursor (claimed *before* the run can drain, which is what stops a worker losing a swap),
//! the isolate module handle, the cancel flag, the profiler seam, the tiering arm, the AOT dispatch
//! bind. Twelve `run_module_*` methods are thin `RunOptions` presets over it, so a tenth mandatory
//! step lands once and reaches all twelve.
//!
//! Eleven of them were presets. The twelfth, `run_module_aot`, was a parallel body, and its own doc
//! stated the reason: *"Stays off the `RunOptions` core: the dispatch bind is an unsafe pre-run step
//! no other mode has."* That was true about the `unsafe`, and it was also the entire mechanism by
//! which the `--native` path stopped receiving `cancel`, `hot_mailbox` and the profiler seam as the
//! core grew them. All three were N/A for a `--native` binary — the point is that nothing made the
//! author of the *next* init step reconsider (parallel-path audit row 13).
//!
//! The fix was structural: the dispatch table became [`RunOptions::aot_dispatch`], an
//! [`AotDispatch`] newtype carrying the safety contract, so the `unsafe` sits where a caller
//! discharges it rather than justifying a second copy of the protocol. This file is what keeps that
//! from silently coming apart again.
//!
//! ## What the gate checks
//!
//! - **Completeness, twice.** The census [`FIELDS`] list is checked against the field names parsed
//!   out of `backend.rs` (text), *and* against an exhaustive destructuring of a real `RunOptions`
//!   (the compiler). Text alone can miss a field written in a shape the scanner does not recognize;
//!   a destructure alone cannot see a field the census forgot to mention. Together, a new field
//!   fails both.
//! - **Consumption.** Every field must appear as `opts.<field>` inside `run_module_with`'s body. A
//!   field nothing reads is either dead or — the case that has actually happened — read somewhere
//!   *else*, which is a second setup path by another name.
//! - **Reachability.** Every `pub fn run_module_*` must reach `run_module_with`, directly or through
//!   another preset. This is the half that fires on the precise mistake: a new run mode written as
//!   its own `Vm::load` + init sequence, because the core "does not fit".
//!
//! ## Why source text
//!
//! Same answer as `noeta-ir/tests/lowerer_field_census.rs`, which this borrows wholesale: Rust has
//! no stable way to ask which function reads which field, and a proc-macro crate to answer it would
//! be a new build dependency for a question one `grep` answers. Reading text has one failure mode —
//! a shape the scanner does not recognize is invisible — so the completeness half is *also* a
//! compile error, and the scanner refuses to run at all if it cannot find its anchors.
//!
//! [`VmBackend::run_module_with`]: noeta_vm::VmBackend::run_module_with
//! [`RunOptions::aot_dispatch`]: noeta_vm::RunOptions::aot_dispatch
//! [`AotDispatch`]: noeta_vm::AotDispatch

use std::collections::BTreeSet;

/// Every field of `RunOptions`, and whether this build has it. The `cfg`s mirror the struct's own,
/// so the census is exact in each feature shape rather than approximate in all of them.
const FIELDS: &[&str] = &[
    "host",
    "executor",
    "collector",
    "gc_threshold",
    "tiering",
    "debugger",
    "profiler",
    #[cfg(feature = "compile")]
    "session",
    "hot_mailbox",
    "isolates",
    "cancel",
    "isolate_profiler",
    #[cfg(feature = "jit")]
    "bail_histogram",
    #[cfg(feature = "jit")]
    "drain_at_exit",
    #[cfg(feature = "jit")]
    "aot_bodies",
    #[cfg(feature = "jit-rt")]
    "aot_dispatch",
];

/// The compile-time half of completeness: a field added to `RunOptions` breaks this destructuring
/// until it is named, and naming it here without adding it to [`FIELDS`] fails the text half below.
/// No `..` — that is the whole point (see the `..Default::default()` note in the parallel-path
/// audit: "I did not consider this field" and "I chose the default" must not look alike).
#[test]
fn every_run_options_field_is_named_by_the_census() {
    let noeta_vm::RunOptions {
        host: _,
        executor: _,
        collector: _,
        gc_threshold: _,
        tiering: _,
        debugger: _,
        profiler: _,
        #[cfg(feature = "compile")]
            session: _,
        hot_mailbox: _,
        isolates: _,
        cancel: _,
        isolate_profiler: _,
        #[cfg(feature = "jit")]
            bail_histogram: _,
        #[cfg(feature = "jit")]
            drain_at_exit: _,
        #[cfg(feature = "jit")]
            aot_bodies: _,
        #[cfg(feature = "jit-rt")]
            aot_dispatch: _,
    } = noeta_vm::RunOptions::default();
}

/// `backend.rs`, read as text — the source of truth for the two structural properties below.
fn backend_source() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/backend.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// The body of the item introduced by `header`, brace-matched. `None` if the header is absent — the
/// callers treat that as a failure rather than a pass, because a renamed anchor must not silently
/// disarm the check.
fn body_after(source: &str, header: &str) -> Option<String> {
    let start = source.find(header)? + header.len();
    let rest = &source[start..];
    let open = rest.find('{')?;
    let mut depth = 0usize;
    for (i, c) in rest[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(rest[open..open + i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// The field names declared by `pub struct RunOptions { … }`, as the source spells them.
fn declared_fields(source: &str) -> BTreeSet<String> {
    let body = body_after(source, "pub struct RunOptions")
        .expect("`pub struct RunOptions` must exist in backend.rs");
    let mut fields = BTreeSet::new();
    for line in body.lines() {
        let line = line.trim();
        // `pub <name>: <type>,` — the only shape this struct uses. A field written any other way is
        // invisible here, which is why the destructuring test above exists.
        let Some(rest) = line.strip_prefix("pub ") else {
            continue;
        };
        let Some((name, _)) = rest.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            fields.insert(name.to_string());
        }
    }
    assert!(
        !fields.is_empty(),
        "the RunOptions field scanner found nothing — the struct's shape changed and this census \
         is now blind; fix the scanner before trusting a green run"
    );
    fields
}

/// **Completeness (text).** Every field `backend.rs` declares in this build's feature shape is in
/// [`FIELDS`], and nothing in [`FIELDS`] is stale.
#[test]
fn the_census_lists_exactly_the_declared_fields() {
    let source = backend_source();
    let declared = declared_fields(&source);
    let census: BTreeSet<String> = FIELDS.iter().map(|f| f.to_string()).collect();
    // A field the source declares under a `cfg` this build does not have is not comparable here, so
    // only the *presence* of a censused field is asserted in both directions for the active shape.
    let missing: Vec<_> = declared.difference(&census).cloned().collect();
    let stale: Vec<_> = census.difference(&declared).cloned().collect();
    assert!(
        stale.is_empty(),
        "the census names RunOptions field(s) that no longer exist: {stale:?}"
    );
    // `missing` may legitimately contain fields gated off in this build shape, so it is only a
    // failure when this build actually has them — which the destructuring test decides. Report them
    // as a hint rather than a hard failure, EXCEPT in the full-feature shape where nothing is gated
    // off and the two sets must be equal.
    #[cfg(all(feature = "jit", feature = "compile"))]
    assert!(
        missing.is_empty(),
        "RunOptions grew field(s) the census does not classify: {missing:?} — add them to FIELDS \
         and to the destructuring test, then make sure `run_module_with` actually consumes them"
    );
    #[cfg(not(all(feature = "jit", feature = "compile")))]
    let _ = missing;
}

/// **Consumption.** Every censused field is read by the core runner. A field the core never touches
/// is either dead or read by a second, parallel setup path — the shape this census exists to catch.
#[test]
fn every_run_options_field_is_consumed_by_the_core_runner() {
    let source = backend_source();
    let body = body_after(
        &source,
        "pub fn run_module_with(&self, module: &Module, opts: RunOptions) -> RunOutcome",
    )
    .expect("`run_module_with` must exist in backend.rs — the census anchors on it");
    let unconsumed: Vec<&str> = FIELDS
        .iter()
        .copied()
        .filter(|field| !body.contains(&format!("opts.{field}")))
        .collect();
    assert!(
        unconsumed.is_empty(),
        "these RunOptions fields are never read by `run_module_with`: {unconsumed:?}\n\
         A run option the core does not consume is a promise to the caller that nothing keeps. If \
         the option needs a step the core cannot host, that is the row-13 finding repeating itself \
         — put the step in the core (a newtype can carry an `unsafe` contract, see AotDispatch) \
         rather than writing a parallel `run_module_*` body."
    );
}

/// **Reachability.** Every `run_module_*` entry point reaches the core, directly or through another
/// preset. A new mode that hand-rolls `Vm::load` + its own init sequence fails here.
#[test]
fn every_run_module_entry_point_reaches_the_core() {
    let source = backend_source();
    // Collect `fn run_module_*` (the `pub`/`unsafe`/`async` prefixes vary) with its body.
    let mut methods: Vec<(String, String)> = Vec::new();
    let mut cursor = 0usize;
    while let Some(at) = source[cursor..].find("fn run_module") {
        let start = cursor + at;
        let name_start = start + "fn ".len();
        let name_end = source[name_start..]
            .find(['(', '<'])
            .map(|i| name_start + i)
            .expect("a function name is followed by `(` or `<`");
        let name = source[name_start..name_end].to_string();
        let body = body_after(&source[start..], &source[start..name_end])
            .unwrap_or_else(|| panic!("cannot brace-match the body of `{name}`"));
        methods.push((name, body));
        cursor = name_end;
    }
    assert!(
        methods.len() >= 10,
        "the run_module_* scanner found only {} method(s) — it has gone blind",
        methods.len()
    );

    // A method reaches the core if it calls `run_module_with`, or calls another method that does.
    // Iterated to a fixpoint so a two-hop preset (`..._no_jit` → `run_module_debug` → core) passes.
    let mut reaching: BTreeSet<String> = BTreeSet::from(["run_module_with".to_string()]);
    loop {
        let before = reaching.len();
        for (name, body) in &methods {
            if reaching.contains(name) {
                continue;
            }
            if reaching
                .iter()
                .any(|target| body.contains(&format!("{target}(")))
            {
                reaching.insert(name.clone());
            }
        }
        if reaching.len() == before {
            break;
        }
    }
    let orphans: Vec<&str> = methods
        .iter()
        .map(|(name, _)| name.as_str())
        .filter(|name| !reaching.contains(*name))
        .collect();
    assert!(
        orphans.is_empty(),
        "these run_module_* entry points never reach `run_module_with`: {orphans:?}\n\
         Each one is a second copy of the load→attach→arm→run→collect protocol, and the next \
         mandatory init step added to the core will not reach it. `run_module_aot` was exactly \
         this, and it cost `cancel`, `hot_mailbox` and the profiler seam (parallel-path audit row \
         13)."
    );
}
