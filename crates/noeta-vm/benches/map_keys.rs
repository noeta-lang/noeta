//! The string-map hot paths. `MapStore` is a `HashMap<MapKey, _>` (hashbrown) so extern types can
//! key maps, and the claim under test is that the string paths pay nothing for it: content-only
//! hashing keeps the `&str` probe hash-identical and allocation-free, inserts keep the
//! short-string move/clone fast paths, and the only added cost is one predicted discriminant check
//! in `eq`. Gets and set-churn must not regress.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};

use noeta_bytecode::Module;
use noeta_lexer::lex;
use noeta_parser::parse;
use noeta_span::{Source, SourceId};
use noeta_vm::VmBackend;

fn compile(src: &str) -> Module {
    // The bench is its own assembling driver: the compiler resolves std names
    // against the process-default registry. Outside the measured loop; idempotent.
    noeta_stdlib::registry::default_seeded();
    let source = Source::new(SourceId::FIRST, "bench.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "bench program must parse without diagnostics: {:?}",
        parsed.diagnostics
    );
    noeta_compiler::compile(&parsed.program).expect("bench program must be in the VM subset")
}

/// Lookup-heavy: a small string-keyed map probed twice per iteration (`m[k]` + `get_or`) — the
/// borrowed-`&str`, zero-allocation read path.
fn map_get_src(n: usize) -> String {
    format!(
        "fn run(n: int): int {{\n    \
            mut m: Map<string, int> = {{}};\n    \
            m = m.set(\"alpha\", 1);\n    \
            m = m.set(\"beta\", 2);\n    \
            m = m.set(\"gamma-key-spills-sso\", 3);\n    \
            mut total = 0;\n    \
            for i in 0..n {{\n        \
                total = total + m[\"alpha\"] + m.get_or(\"beta\", 0) + m[\"gamma-key-spills-sso\"];\n    \
            }}\n    \
            return total;\n\
         }}\n\
         echo run({n});\n"
    )
}

/// Insert-churn: a uniquely-owned map self-update per iteration (`m = m.set(k, i)`) — the
/// P-REUSE in-place path with the P-SSO key move/clone.
fn map_set_src(n: usize) -> String {
    format!(
        "fn run(n: int): int {{\n    \
            mut m: Map<string, int> = {{}};\n    \
            for i in 0..n {{\n        \
                m = m.set(\"hot-key\", i);\n        \
                m = m.set(\"other\", i);\n    \
            }}\n    \
            return m[\"hot-key\"];\n\
         }}\n\
         echo run({n});\n"
    )
}

fn map_hot_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("vm_map_keys");
    for &n in &[10_000usize, 100_000] {
        let module = compile(&map_get_src(n));
        group.bench_with_input(BenchmarkId::new("get", n), &module, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
        let module = compile(&map_set_src(n));
        group.bench_with_input(BenchmarkId::new("set_churn", n), &module, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    group.finish();
}

criterion_group!(benches, map_hot_paths);
criterion_main!(benches);
