//! Criterion benchmark for the reactivity milestone (architecture §9.4): the **large fan-out flush**,
//! the hot path a `signal.set` drives. One signal fans out to `N` effects; each `set` marks all `N`
//! dirty and the flush reruns every one in ascending-`NodeId` order. The timed work is therefore
//! `SETS × N` effect body runs plus the graph bookkeeping (queue drain + sort, dependency
//! resubscription) — the shape a server pushing one state change to many subscribers exercises.
//!
//! This is the reactivity baseline: there is no "before" (the feature is new), so it pins the
//! flush's cost per fan-out width so a later change (batching, value-equality suppression, the
//! transport diff layer) can prove it introduced no regression. The whole compiled module runs in the
//! timed closure; with `SETS` sets over `N` effects the flush dominates the one-time setup (creating
//! the `N` effects is `O(N)`, the sets are `O(SETS × N)`), so the measurement tracks flush width.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use noeta_bytecode::Module;
use noeta_lexer::lex;
use noeta_parser::parse;
use noeta_span::{Source, SourceId};
use noeta_vm::VmBackend;

/// Source → compiled `Module`. Panics if the program falls outside the VM subset, so a silently
/// near-empty module never benches nothing (mirrors the `vm` bench's `compile`).
fn compile(src: &str) -> Module {
    let source = Source::new(SourceId::FIRST, "reactivity_bench.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "bench program must parse without diagnostics: {:?}",
        parsed.diagnostics
    );
    noeta_compiler::compile(&parsed.program).expect("bench program must be in the VM subset")
}

/// How many sets each program performs. Fixed across fan-out widths so the benchmark isolates the
/// per-set flush cost as `N` (the number of subscribed effects) grows.
const SETS: usize = 16;

/// A fan-out program: one signal, `n` effects each reading it, then `SETS` sets — each set flushes
/// all `n` effects. The effects are created in a loop (so the source stays small for any `n`), each
/// with a trivial body that only reads the signal (subscribing it); the reruns are what the flush
/// measures, not the body work.
fn fanout_src(n: usize) -> String {
    format!(
        "use std.reactive.{{signal, effect}}\n\
         s = signal(0)\n\
         mut c = 0\n\
         while c < {n} {{\n    \
             effect(fn() {{ s.get() }})\n    \
             c = c + 1\n\
         }}\n\
         for i in 0..{SETS} {{\n    \
             s.set(i)\n\
         }}\n\
         echo \"done\"\n",
    )
}

/// The fan-out program with the change log switched on (a `view` exposes the signal, which is what
/// enables observation) — benched against the plain `fanout` to pin the L1 recording overhead on
/// the flush hot path at ~zero. No `diff()` in the loop: this isolates record+distribute.
fn fanout_observed_src(n: usize) -> String {
    format!(
        "use std.reactive.{{signal, effect, view}}\n\
         s = signal(0)\n\
         v = view()\n\
         v.expose(\"s\", s)\n\
         mut c = 0\n\
         while c < {n} {{\n    \
             effect(fn() {{ s.get() }})\n    \
             c = c + 1\n\
         }}\n\
         for i in 0..{SETS} {{\n    \
             s.set(i)\n\
         }}\n\
         echo \"done\"\n",
    )
}

/// The diff-push shape (server-hmr L1): one view exposing `n` cold bindings plus one hot signal;
/// each iteration sets the hot signal and renders `diff()`. The minimal-diff promise is that the
/// patch cost tracks the ONE dirty binding (serialize one value), not the `n` cold ones — the
/// change log narrows the candidate set before any serialization happens.
fn view_diff_src(n: usize) -> String {
    format!(
        "use std.reactive.{{signal, view}}\n\
         v = view()\n\
         hot = signal(0)\n\
         v.expose(\"hot\", hot)\n\
         mut c = 0\n\
         while c < {n} {{\n    \
             v.expose(\"cold${{c}}\", signal(0))\n    \
             c = c + 1\n\
         }}\n\
         for i in 0..{SETS} {{\n    \
             hot.set(i + 1)\n    \
             v.diff()\n\
         }}\n\
         echo \"done\"\n",
    )
}

fn reactivity_flush(c: &mut Criterion) {
    const FANOUT: &[usize] = &[64, 256, 1024];

    let mut group = c.benchmark_group("reactivity_flush");
    for &n in FANOUT {
        let module = compile(&fanout_src(n));
        group.bench_with_input(BenchmarkId::new("fanout", n), &module, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
        let observed = compile(&fanout_observed_src(n));
        group.bench_with_input(
            BenchmarkId::new("fanout_observed", n),
            &observed,
            |b, module| {
                b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
            },
        );
    }
    group.finish();
}

fn reactivity_view_diff(c: &mut Criterion) {
    const COLD: &[usize] = &[64, 256, 1024];

    let mut group = c.benchmark_group("view_diff_push");
    for &n in COLD {
        let module = compile(&view_diff_src(n));
        group.bench_with_input(
            BenchmarkId::new("cold_bindings", n),
            &module,
            |b, module| {
                b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
            },
        );
    }
    group.finish();
}

criterion_group!(benches, reactivity_flush, reactivity_view_diff);
criterion_main!(benches);
