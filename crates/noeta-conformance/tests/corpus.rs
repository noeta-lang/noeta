//! Runs the whole conformance corpus under `cargo test`, so the executable spec is a
//! CI gate and not only reachable via `lang test`. Mirrors what `lang test` does.

use std::path::PathBuf;

use noeta_conformance::{
    Stage, on_deep_stack, run_corpus, run_differential, run_ir_corpus, run_leak_check,
};

fn corpus_root() -> PathBuf {
    // crates/noeta-conformance → workspace root → tests/conformance
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/conformance")
}

#[test]
fn conformance_corpus_passes() {
    // Run on a large-stack worker: a realistic case (async server + reactive diff) can out-recurse
    // libtest's ~2 MiB test thread in debug. See `noeta_conformance::on_deep_stack`.
    on_deep_stack(|| {
        let root = corpus_root();
        assert!(
            root.is_dir(),
            "conformance corpus not found at {}",
            root.display()
        );

        let report = run_corpus(&root, None, Stage::Eval);
        assert!(
            !report.cases.is_empty(),
            "the conformance corpus is empty — at least the walking-skeleton case should exist"
        );
        assert!(
            report.all_passed(),
            "conformance failures:\n{}",
            report.to_human()
        );
        assert_engine_reach(&report);
    });
}

/// **The reach assertion on the expectation runner**: every corpus program is checked against its
/// header on *both* engines, so this gate is a verdict on the language rather than on the reference
/// interpreter's half of it.
///
/// It is asserted rather than reported for the reason every other reach floor here exists: an
/// all-green run over an engine that executed nothing is indistinguishable from an all-green run
/// that covered everything, and the pass is what gets cited. The two counts must be *equal* — a
/// program the reference ran and the VM did not is a program the compiler refused, which the run
/// already fails on — and the floor stops the whole set silently draining away.
fn assert_engine_reach(report: &noeta_conformance::Report) {
    use noeta_conformance::Engine;
    let on_reference = report.executed_on(Engine::Reference);
    let on_vm = report.executed_on(Engine::Vm);
    assert_eq!(
        on_reference,
        on_vm,
        "the two engines ran different programs; {}",
        report.coverage_line()
    );
    // Measured at 893 on 2026-08-19 — the corpus programs that survive the front end. If it drops,
    // find out which cases stopped running before touching this number.
    const MIN_EXECUTED: usize = 850;
    assert!(
        on_vm >= MIN_EXECUTED,
        "only {on_vm} case(s) executed a program (floor {MIN_EXECUTED}); {}",
        report.coverage_line()
    );
}

/// The leak oracle's **known debt**: `(backend, program)` pairs tolerated as leaking at clean exit
/// because of reference cycles no collector reaps. **This list is now empty** — the
/// memory-management migration closed the last cyclic debt in Phase 6, so residency is **0 on both
/// backends for every program** and any leak at all fails the gate. The list and the gate around it
/// stay so the guarantee is enforced, not merely achieved once: the oracle asserts the leak set
/// equals *exactly* this list, so a brand-new leak fails the gate AND any future debt entry can only
/// be removed by an actual fix.
///
/// How residency reaches 0 (for the record): the only cycles under value semantics are
/// closure/scope self-captures. The **VM** reaps its closure↔cell cycles with the backup mark-sweep
/// trace at clean exit (rooted at the live globals — Phase 6); the **eval** Core-IR interpreter
/// reaps its `Rc<Scope>` ↔ `Rc<Closure>` capture cycles by clearing the bindings of any captured
/// scope still live after global teardown, and its precise last-use drops also reclaim ordinary
/// captured bindings promptly (so e.g. `counter_nested_fn` never accumulates a cycle in the first
/// place).
const KNOWN_LEAKS: &[(&str, &str)] = &[];

#[test]
fn leak_oracle_residency_is_zero_except_known_cycles() {
    // The leak oracle (architecture §0): every program must reclaim all of its heap before it
    // returns — residency 0 at clean exit, on *both* backends. A cycle leak or missed release
    // shows up as a positive per-program residual. The only tolerated residuals are the cyclic
    // ones in `KNOWN_LEAKS` (Phase-6 debt); any other leak — or any change to the known set —
    // fails the gate.
    on_deep_stack(|| {
        let report = run_leak_check(&corpus_root(), None);
        eprintln!("{}", report.to_human());

        let mut found: Vec<(&str, &str)> = report
            .leaks
            .iter()
            .map(|l| (l.backend, l.name.as_str()))
            .collect();
        found.sort_unstable();
        let mut expected: Vec<(&str, &str)> = KNOWN_LEAKS.to_vec();
        expected.sort_unstable();

        assert_eq!(
            found,
            expected,
            "the leak-oracle set changed.\n  - a NEW pair ⇒ a real leak to fix or a regression\n  \
             - a MISSING pair ⇒ a debt was fixed; remove it from KNOWN_LEAKS\nfull report:\n{}",
            report.to_human()
        );
    });
}

#[test]
fn ir_lowering_is_total_over_the_corpus() {
    // The Core-IR interpreter is the reference (memory-management migration, Phase 4): every
    // parse+check-clean program lowers and runs through it. This asserts the lowering stays
    // **total** over the corpus (skipped == 0) — the floor the differential (IR interpreter vs VM)
    // relies on to compare every program. (The skip mechanism remains, ready for any new AST node
    // added ahead of its lowering.)
    //
    // Through Phase 3 this was the *faithfulness* oracle, which also asserted the IR interpreter
    // matched the AST tree-walker byte-for-byte. Phase 4 promoted the IR interpreter to the
    // reference; the AST walker can no longer reproduce its last-use destruction, so the equality
    // assertion was retired (the live cross-check is now the differential). See `ir_corpus.rs`.
    on_deep_stack(|| {
        let report = run_ir_corpus(&corpus_root(), None);
        eprintln!("{}", report.to_human());
        assert!(
            report.ran > 0,
            "the IR-corpus sweep ran no programs:\n{}",
            report.to_human()
        );
        assert_eq!(
            report.not_run.unsupported,
            0,
            "the Core-IR lowering must cover 100% of the comparable corpus; got:\n{}",
            report.to_human()
        );
    });
}

#[test]
fn no_local_is_read_after_its_drop() {
    // The static-≤-dynamic last-use property (memory-management Phase 3.x): a `DropVar` must never
    // fire before its binding's real last *dynamic* read. The drop-audit records every drop, rebind,
    // and read in the IR interpreter; we run the whole corpus through the IR-corpus sweep (which
    // executes every lowered program via the reference) with the audit active, and assert it
    // observed zero use-after-drop violations — the static drop placement is sound against
    // ground-truth execution, independent of the liveness reasoning that placed the drops.
    // The whole audit runs inside the worker: `drop_audit` state is thread-local, so `begin`/`end`
    // must be on the *same* thread that executes the programs — otherwise the audit would observe no
    // drops and pass vacuously (the `report.ran > 0` guard cannot catch a cross-thread disconnect).
    on_deep_stack(|| {
        noeta_eval::drop_audit::begin();
        let report = run_ir_corpus(&corpus_root(), None);
        let violations = noeta_eval::drop_audit::end();
        // Guard against a vacuous pass: the sweep must actually have run programs through the IR path.
        assert!(
            report.ran > 0,
            "the IR-corpus sweep ran no programs — the audit would be vacuous"
        );
        assert_eq!(
            violations, 0,
            "use-after-drop: a DropVar fired before its binding's last dynamic read in {} of {} programs",
            violations, report.ran
        );
    });
}

#[test]
fn differential_backends_agree() {
    // The differential oracle: every program the M1 VM can compile must produce a byte-for-
    // byte identical `RunResult` to the M0 tree-walker. Programs outside the VM's current
    // subset are skipped (not failed); this is the climbing coverage gate for Thrust A.
    on_deep_stack(|| {
        let report = run_differential(&corpus_root(), None);
        // Print coverage so the agent loop can watch it climb slice by slice.
        eprintln!("{}", report.to_human());
        assert!(
            report.ok(),
            "the VM diverged from the tree-walker:\n{}",
            report.to_human()
        );
        // M1.0 established the spine on the smallest subset; each slice raised this floor until
        // M1.5 reached the **Thrust-A gate**: the VM compiles 100% of the comparable corpus
        // (every parse-clean case) — match/`?`/`??`/constructors completing the feature surface.
        // The tree-walker is now frozen as the pure oracle; this floor must never regress.
        assert_eq!(
            report.not_run.unsupported,
            0,
            "the VM must compile 100% of the comparable corpus (Thrust-A gate); got:\n{}",
            report.to_human()
        );
    });
}

/// The bundle gate (P-AOT L1.0 + L1.3): every module the VM compiles must survive a
/// serialize→deserialize→serialize round-trip byte-for-byte (structural), *and* the decoded module
/// must run byte-identically to the source-compiled one on the sandbox (execution) — the two
/// preconditions for shipping a `.noeb` bundle instead of source.
#[test]
fn bundle_modules_round_trip_and_run_identically() {
    on_deep_stack(|| {
        let report = noeta_conformance::run_bundle_roundtrip(&corpus_root(), None);
        eprintln!("{}", report.to_human());
        assert!(
            report.ok(),
            "a compiled module did not survive bundling:\n{}",
            report.to_human()
        );
        assert_eq!(
            report.failures.len(),
            0,
            "every compiled module must serialize losslessly; got:\n{}",
            report.to_human()
        );
    });
}

/// The JIT differential gate (milestone P-JIT): every program the VM compiles must produce a
/// byte-for-byte identical `RunResult` on the forced tier-1 JIT as on the interpreter, and leave
/// zero heap residency under JIT. Only compiled in a `--features jit` build; the plain build's
/// `differential_backends_agree` above is unaffected.
#[cfg(feature = "jit")]
#[test]
fn jit_differential_tiers_agree() {
    on_deep_stack(|| {
        let report = noeta_conformance::run_jit_differential(&corpus_root(), None);
        eprintln!("{}", report.to_human());
        assert!(
            report.ok(),
            "tier 1 diverged from tier 0 (or leaked under JIT):\n{}",
            report.to_human()
        );
        // Every parse-clean program the VM supports must run through the JIT (J0 forces this); the
        // interpreter's own gate already fixes `skipped == 0`, so tier 1 covers the same 100%.
        assert_eq!(
            report.not_run.unsupported,
            0,
            "the JIT oracle must cover 100% of the comparable corpus; got:\n{}",
            report.to_human()
        );
        assert_native_proto_ratchet(report.native_protos, &report.to_human());
    });
}

/// The **native-prototype ratchet**. This was `native_protos > 0` — a floor so low that a regression
/// in `is_fast_op` turning 2600 prototypes into bail stubs passed it (parallel-path audit row 9): the
/// oracle would still compare tier 0 against tier 1 and still agree, because an all-bail tier 1 *is*
/// tier 0. The number below is a measurement of the corpus, and it only ever goes up.
///
/// Slack is deliberate but small: prototype counts move when the corpus gains or loses a case, and a
/// gate that fails on an unrelated fixture edit gets deleted. A real codegen regression sheds
/// hundreds at once, not tens.
///
/// **When this fails high** ("the corpus now compiles more"), raise it — that is the ratchet
/// tightening. **When it fails low**, do not lower it: find out which prototypes stopped compiling.
/// `cargo run -p noeta-conformance --features jit -- --jit-differential` prints the live number.
///
/// `jit`-gated with the three oracles that assert it: without Cranelift nothing compiles a prototype
/// to native code, so the number does not exist to ratchet.
#[cfg(feature = "jit")]
const NATIVE_PROTO_FLOOR: usize = 2550; // measured 2633 on 2026-08-01

#[cfg(feature = "jit")]
fn assert_native_proto_ratchet(native_protos: usize, human: &str) {
    assert!(
        native_protos >= NATIVE_PROTO_FLOOR,
        "the corpus compiled only {native_protos} prototypes to native code, below the ratchet of \
         {NATIVE_PROTO_FLOOR} — some prototypes that used to go native are now bail stubs (tier 1 \
         still AGREES with tier 0 in that case: an all-bail tier 1 is tier 0, which is why a bare \
         `> 0` floor could not see this). Do not lower the ratchet; find what stopped \
         compiling:\n{human}"
    );
}

/// The **AOT-bodies** JIT differential gate (parallel-path audit row 9): the same corpus, the same
/// two tiers, but the forced-JIT run emits the body shape `noeta build --native` links — inline
/// caches off, null call sites, no cancellation poll — and the result must still be byte-identical.
///
/// Three comments in the tree said this codegen was "proven corpus-wide by the `NOETA_JIT_AOT`
/// oracle". It was not: that knob is an environment variable read at `Jit` construction, it appeared
/// in no gate script and no CI workflow, and the only thing actually gating `--native` was one
/// hand-written all-int program comparing stdout, which skipped silently without a C toolchain. An
/// AOT-only soundness bug (`0f9752d4c`, a misaligned dispatch table) had already been found late in
/// exactly that gap. This test makes the claim true, per-commit, with no linker: the knob became
/// [`RunOptions::aot_bodies`] so an in-process arm can set it.
///
/// The linked half — the real `cc`-linked artifact, which also covers the AOT *run tail* and the
/// dispatch table itself — is `--aot-differential`, run from `scripts/gate.sh` because a link per
/// program is minutes rather than seconds.
///
/// [`RunOptions::aot_bodies`]: noeta_vm::RunOptions::aot_bodies
#[cfg(feature = "jit")]
#[test]
fn jit_differential_aot_bodies_agree() {
    on_deep_stack(|| {
        let report = noeta_conformance::run_jit_differential_with(
            &corpus_root(),
            None,
            noeta_conformance::JitDiffArm::AotBodies,
        );
        eprintln!("{}", report.to_human());
        assert!(
            report.ok(),
            "the ahead-of-time body shape changed a program's observable behaviour (or \
             leaked):\n{}",
            report.to_human()
        );
        assert_eq!(
            report.not_run.unsupported,
            0,
            "the AOT-bodies oracle must cover 100% of the comparable corpus; got:\n{}",
            report.to_human()
        );
        // Coverage, not just agreement: AOT-form bodies that all bailed would agree with tier 0
        // trivially and prove nothing about the codegen a `--native` artifact carries.
        assert_native_proto_ratchet(report.native_protos, &report.to_human());
    });
}

/// The **cancel-poll** JIT differential gate (isolate-cancel, JIT half): the same corpus, the same
/// two tiers, but the forced-JIT run carries a cancellation flag that is never set — so every
/// compiled loop header carries the cancellation poll, and the result must still be byte-identical.
///
/// This is a second pass rather than a replacement because the two arms cover *different generated
/// code*. Production runs are not cancellable and get no poll; only a bounded `@test` case (and a
/// worker isolate, were one ever tier-1) gets the poll-bearing bodies. Testing one arm would leave
/// the other unchecked, and the poll-bearing arm is precisely the one whose bail placement is new.
///
/// A never-set flag is the honest control: the interpreter's own safepoints read `false` all the
/// way through, so the *only* difference between the arms is the native code, which is exactly what
/// this oracle exists to compare.
#[cfg(feature = "jit")]
#[test]
fn jit_differential_cancel_poll_agrees() {
    on_deep_stack(|| {
        let report = noeta_conformance::run_jit_differential_with(
            &corpus_root(),
            None,
            noeta_conformance::JitDiffArm::CancelPoll,
        );
        eprintln!("{}", report.to_human());
        assert!(
            report.ok(),
            "the cancellation poll changed a program's observable behaviour (or leaked):\n{}",
            report.to_human()
        );
        assert_eq!(
            report.not_run.unsupported,
            0,
            "the cancel-poll oracle must cover 100% of the comparable corpus; got:\n{}",
            report.to_human()
        );
        assert_native_proto_ratchet(report.native_protos, &report.to_human());
    });
}

/// A **single-file** corpus case may not import a user module.
///
/// Single-file cases are checked from their raw text, with no linker: an unresolved `use App.X`
/// silently becomes an opaque stub instead of the `E0019` the real `noeta` binary reports. That is
/// a fidelity gap between this harness and the shipped pipeline, and it hid two obsolete M0
/// fixtures (`demo/orders.noe`, `modules/namespace_and_use.noe`) for months: both asserted stdout
/// they could no longer produce, passed here, and failed through the CLI. Both are now directory
/// cases carrying the modules they import.
///
/// So the rule this pins: a case that imports a user module is a **multi-file** case. Put it in its
/// own directory with `main.noe` plus the modules it needs, and the loader/linker path — the same
/// one the CLI uses — runs it. `use std.…` is unaffected; those resolve through the extension
/// registry with no linking involved.
#[test]
fn a_single_file_case_never_imports_a_user_module() {
    let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/conformance");
    let mut offenders = Vec::new();
    collect_single_file_cases(&corpus, &mut offenders);
    assert!(
        offenders.is_empty(),
        "these single-file cases import a user module, which the single-file harness path cannot \
         resolve (it never links) — make each one a directory case with `main.noe` and the \
         modules it imports:\n{}",
        offenders.join("\n")
    );
}

/// Walk the corpus the way `collect_cases` does — a directory holding `main.noe` is one multi-file
/// case and is not descended into — collecting single-file cases that carry a user import.
fn collect_single_file_cases(dir: &std::path::Path, out: &mut Vec<String>) {
    if dir.join("main.noe").is_file() {
        return; // a multi-file case: the linker resolves its imports, which is the point
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_single_file_cases(&path, out);
        } else if path.extension().is_some_and(|e| e == "noe") {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            // A user import is `use <Capitalised>…`; `use std.…` needs no linking.
            if text.lines().any(|line| {
                line.trim_start()
                    .strip_prefix("use ")
                    .and_then(|rest| rest.chars().next())
                    .is_some_and(char::is_uppercase)
            }) {
                out.push(format!("  {}", path.display()));
            }
        }
    }
}
