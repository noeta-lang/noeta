//! P-PAR S0c: measure the synchronous tier-1 compile pause (`plans/perf/parallel-seams/`).
//!
//! Generates a program with many int-only functions of mixed sizes, runs it under **ordinary
//! hot-counter promotion** (the production tiering), and reports how much mutator wall time the
//! in-line Cranelift compiles cost — the go/no-go input for the off-thread-JIT slice (S4).
//!
//! Run: `cargo run --release -p noeta-vm --features jit --example jit_pause`

use noeta_compiler::compile;
use noeta_lexer::lex;
use noeta_parser::parse;
use noeta_span::{Source, SourceId};
use noeta_vm::VmBackend;

/// One int-only function of `stmts` chained arithmetic statements — J1-eligible, so promotion
/// compiles a real native body whose size scales with `stmts`.
fn make_fn(name: &str, stmts: usize) -> String {
    let mut body = String::from("    mut a = x\n");
    for i in 0..stmts {
        body.push_str(&format!("    a = a * 3 + x % {} + 1\n", i + 2));
    }
    format!("fn {name}(x: int): int {{\n{body}    return a\n}}\n")
}

fn main() {
    // 30 functions, each called 60 times (> JIT_HOT_THRESHOLD = 50) so every one is promoted
    // mid-run. Sizes: mixed 5/40/160 statements by default; `JIT_PAUSE_STMTS=<n>` makes all 30
    // uniform at n statements (for per-size pause numbers).
    let uniform: Option<usize> = std::env::var("JIT_PAUSE_STMTS")
        .ok()
        .and_then(|v| v.parse().ok());
    let mut src = String::new();
    let mut names = Vec::new();
    for i in 0..30 {
        let stmts = uniform.unwrap_or(match i % 3 {
            0 => 5,
            1 => 40,
            _ => 160,
        });
        let name = format!("f{i}");
        src.push_str(&make_fn(&name, stmts));
        names.push(name);
    }
    // `JIT_PAUSE_CALLS=<n>` (default 60) sets how many times each function is called. 60 just
    // crosses the promotion threshold (pause measurement); large values make *runtime* dominate,
    // so wall − compile compares generated-code quality across `NOETA_JIT_OPT` levels.
    let calls: usize = std::env::var("JIT_PAUSE_CALLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    src.push_str(&format!("mut total = 0\nfor i in 0..{calls} {{\n"));
    for name in &names {
        src.push_str(&format!("    total = total + {name}(i)\n"));
    }
    src.push_str("}\necho total\n");

    let source = Source::new(SourceId::FIRST, "jit_pause.noe", &src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).expect("generated program is in the VM subset");

    let backend = VmBackend::new();
    let t0 = std::time::Instant::now();
    let (result, stats) = backend.run_module_jit_hot_with_stats(&module);
    let wall = t0.elapsed();

    assert_eq!(result.exit_code, 0, "run failed: {:?}", result.stdout);
    let avg_us = if stats.compiled > 0 {
        stats.compile_ns_total / stats.compiled as u64 / 1_000
    } else {
        0
    };
    println!("wall            {:>10.1} ms", wall.as_secs_f64() * 1e3);
    println!(
        "compiled        {:>10}    (native {})",
        stats.compiled, stats.native
    );
    println!(
        "compile total   {:>10.1} ms  ({:.1}% of wall)",
        stats.compile_ns_total as f64 / 1e6,
        stats.compile_ns_total as f64 / wall.as_nanos() as f64 * 100.0
    );
    println!(
        "compile max     {:>10} µs  (worst single mutator pause)",
        stats.compile_ns_max / 1_000
    );
    println!("compile avg     {:>10} µs", avg_us);
    println!(
        "runtime (wall − compile) {:>7.1} ms",
        (wall.as_nanos() as f64 - stats.compile_ns_total as f64) / 1e6
    );
    // P-JCT C0: where compile total goes. `build` (IR construction) is the remainder.
    let b = stats.breakdown;
    let build_ns = stats
        .compile_ns_total
        .saturating_sub(b.define_ns + b.finalize_ns);
    println!(
        "  build IR      {:>10.1} ms   define {:>8.1} ms   finalize {:>8.1} ms",
        build_ns as f64 / 1e6,
        b.define_ns as f64 / 1e6,
        b.finalize_ns as f64 / 1e6
    );
    println!(
        "  bodies        {:>10}    clif insts {}   code bytes {}",
        b.bodies, b.clif_insts, b.code_bytes
    );
    if b.define_ns > 0 {
        println!(
            "  define throughput {:>6.2} MB/s  ({:.1} µs/body, {:.0} insts/body)",
            b.code_bytes as f64 / (b.define_ns as f64 / 1e9) / 1e6,
            b.define_ns as f64 / b.bodies.max(1) as f64 / 1e3,
            b.clif_insts as f64 / b.bodies.max(1) as f64
        );
    }
}
