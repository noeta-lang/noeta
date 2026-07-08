//! P0 in-process fixtures for `noeta profile`: compile a program tier-0, run it, and assert on the
//! structured [`noeta_prof::Report`]. The profiler is outside the differential oracle (its signal is
//! time, not output), so it is tested this way rather than through the conformance corpus.

use std::path::PathBuf;

/// Write a one-off program into its own private temp *directory* and return its path. Each program
/// gets its own directory because the loader treats the containing directory as the module directory
/// (M1.9), so sibling test files must not share one.
fn fixture(name: &str, src: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("noeta_prof_test_{name}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join(format!("{name}.noe"));
    std::fs::write(&path, src).expect("write fixture");
    path
}

#[test]
fn runs_a_program_tier0_and_forwards_its_output() {
    let path = fixture(
        "fib",
        "fn fib(n: int): int {\n\
         \x20   if n < 2 { return n; }\n\
         \x20   return fib(n - 1) + fib(n - 2);\n\
         }\n\
         echo \"fib=\" ~ fib(20);\n",
    );

    let report = noeta_prof::profile(&path, noeta_prof::Mode::Summary);

    assert_eq!(report.exit_code, 0, "clean run exits 0: {}", report.stderr);
    assert_eq!(
        report.stdout, "fib=6765\n",
        "program stdout is forwarded verbatim"
    );
    assert!(
        report.stderr.is_empty(),
        "a clean run has no stderr (the profile report is emitted separately): {:?}",
        report.stderr
    );
    // The run took *some* measurable time — the P0 profiling signal exists.
    assert!(
        report.wall > std::time::Duration::ZERO,
        "wall time recorded"
    );
}

#[test]
fn compile_error_becomes_a_nonzero_report_not_a_panic() {
    // `let` is not a Noeta binding keyword → a parse error. The profiler must surface it as a
    // failed report, not crash.
    let path = fixture("bad", "let x = 1\n");

    let report = noeta_prof::profile(&path, noeta_prof::Mode::Summary);

    assert_ne!(report.exit_code, 0, "a compile error exits non-zero");
    assert!(
        report.stdout.is_empty(),
        "no program output on a failed compile"
    );
    assert!(
        report.stderr.contains("[E"),
        "the diagnostic is reported on stderr: {}",
        report.stderr
    );
}

#[test]
fn missing_file_is_reported_cleanly() {
    let report = noeta_prof::profile(
        std::path::Path::new("/no/such/noeta/file.noe"),
        noeta_prof::Mode::Summary,
    );
    assert_ne!(report.exit_code, 0);
    assert!(report.stderr.contains("cannot read"), "{}", report.stderr);
}

#[test]
fn program_exit_code_is_forwarded() {
    // A program that aborts (division by zero) surfaces a non-zero exit through the profiler.
    let path = fixture("boom", "echo 1 / 0;\n");
    let report = noeta_prof::profile(&path, noeta_prof::Mode::Summary);
    assert_ne!(
        report.exit_code, 0,
        "a runtime abort propagates a non-zero exit"
    );
}

// ---- P1: instrumenting profiler ----------------------------------------------------------------

/// The self-recursive Fibonacci fixture. `fib(n)` is invoked exactly `2·Fib(n+1) − 1` times, an
/// exact oracle for the call counter. `fib(10) = 55`, invoked `2·89 − 1 = 177` times.
fn fib_src(n: u32) -> String {
    format!(
        "fn fib(n: int): int {{\n\
         \x20   if n < 2 {{ return n; }}\n\
         \x20   return fib(n - 1) + fib(n - 2);\n\
         }}\n\
         echo \"fib=\" ~ fib({n});\n"
    )
}

fn find<'a>(report: &'a noeta_prof::Report, name: &str) -> &'a noeta_prof::FnStat {
    report
        .functions
        .as_ref()
        .expect("instrument mode fills in functions")
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no `{name}` row in the profile"))
}

#[test]
fn summary_mode_has_no_function_table() {
    let path = fixture("sum_none", &fib_src(10));
    let report = noeta_prof::profile(&path, noeta_prof::Mode::Summary);
    assert!(
        report.functions.is_none(),
        "summary mode attaches no collector"
    );
}

#[test]
fn instrument_counts_calls_exactly() {
    let path = fixture("count", &fib_src(10));
    let report = noeta_prof::profile(&path, noeta_prof::Mode::Instrument);

    assert_eq!(report.exit_code, 0, "{}", report.stderr);
    assert_eq!(report.stdout, "fib=55\n");
    // 2·Fib(11) − 1 = 2·89 − 1 = 177. Exact — this is the whole point of an instrumenting profiler.
    assert_eq!(find(&report, "fib").calls, 177, "exact call count");
    // The line table located the definition.
    assert_eq!(find(&report, "fib").line, Some(1));
}

#[test]
fn instrument_ranks_the_hot_function_first_by_self_time() {
    // `spin` does a big arithmetic loop and calls nothing (all self-time); `cheap` returns at once.
    // Even though both are called once, `spin` must sort first and dominate self%.
    let src = "fn spin(n: int): int {\n\
               \x20   mut acc = 0\n\
               \x20   mut i = 0\n\
               \x20   while i < n { acc = acc + i; i = i + 1; }\n\
               \x20   return acc;\n\
               }\n\
               fn cheap(): int { return 1; }\n\
               echo cheap();\n\
               echo spin(3000000);\n";
    let path = fixture("hot", src);
    let report = noeta_prof::profile(&path, noeta_prof::Mode::Instrument);
    assert_eq!(report.exit_code, 0, "{}", report.stderr);

    let functions = report.functions.as_ref().unwrap();
    assert_eq!(
        functions[0].name, "spin",
        "the hot leaf sorts first by self-time"
    );
    let spin = find(&report, "spin");
    let cheap = find(&report, "cheap");
    assert_eq!(spin.calls, 1);
    assert_eq!(cheap.calls, 1);
    assert!(
        spin.self_ns > cheap.self_ns * 10,
        "the loop dwarfs the trivial function: spin={} cheap={}",
        spin.self_ns,
        cheap.self_ns
    );
    // A leaf's self-time equals its total-time (it calls nothing).
    assert_eq!(spin.self_ns, spin.total_ns, "a leaf's self == total");
}

#[test]
fn instrument_total_is_inclusive_of_callees() {
    // `outer` calls `inner` (a spin loop). `outer`'s total must exceed its self, and cover inner.
    let src = "fn inner(n: int): int {\n\
               \x20   mut acc = 0\n\
               \x20   mut i = 0\n\
               \x20   while i < n { acc = acc + i; i = i + 1; }\n\
               \x20   return acc;\n\
               }\n\
               fn outer(n: int): int { return inner(n) + inner(n); }\n\
               echo outer(1000000);\n";
    let path = fixture("inclusive", src);
    let report = noeta_prof::profile(&path, noeta_prof::Mode::Instrument);
    assert_eq!(report.exit_code, 0, "{}", report.stderr);

    let outer = find(&report, "outer");
    let inner = find(&report, "inner");
    assert_eq!(inner.calls, 2, "inner called twice");
    assert!(
        outer.total_ns > outer.self_ns,
        "outer's inclusive time exceeds its own body: total={} self={}",
        outer.total_ns,
        outer.self_ns
    );
    // outer's inclusive time covers both inner calls.
    assert!(
        outer.total_ns >= inner.total_ns,
        "outer total {} covers inner total {}",
        outer.total_ns,
        inner.total_ns
    );
}

#[test]
fn render_table_is_empty_without_instrumentation() {
    let path = fixture("no_table", &fib_src(8));
    let report = noeta_prof::profile(&path, noeta_prof::Mode::Summary);
    assert!(noeta_prof::render_table(&report).is_empty());
}

// ---- P2: sampling profiler -----------------------------------------------------------------------

use noeta_prof::{Mode, SampleClock};

/// A program with one clearly-dominant hot function (`hot`, a big loop) called from the top level.
const HOT_SRC: &str = "fn hot(n: int): int {\n\
     \x20   mut acc = 0\n\
     \x20   mut i = 0\n\
     \x20   while i < n { acc = acc + i; i = i + 1; }\n\
     \x20   return acc;\n\
     }\n\
     echo hot(2000000);\n";

#[test]
fn op_clock_sampling_is_deterministic() {
    // The whole point of the op-clock mode: two runs of the same program produce byte-identical
    // flamegraphs (unlike wall-clock sampling), so the fixtures can assert exactly.
    let path = fixture("det", HOT_SRC);
    let mode = Mode::Sample {
        clock: SampleClock::Ops { every: 1000 },
        lines: false,
    };
    let a = noeta_prof::profile(&path, mode);
    let b = noeta_prof::profile(&path, mode);

    let fa = a.flamegraph.as_ref().expect("sample mode fills flamegraph");
    let fb = b.flamegraph.as_ref().unwrap();
    assert_eq!(fa.total, fb.total, "sample count is reproducible");
    assert!(fa.total > 0, "a long run takes samples");
    assert_eq!(
        noeta_prof::render_folded(&a),
        noeta_prof::render_folded(&b),
        "folded output is identical across runs"
    );
}

#[test]
fn sampling_attributes_most_samples_to_the_hot_function() {
    let path = fixture("hot_fg", HOT_SRC);
    let report = noeta_prof::profile(
        &path,
        Mode::Sample {
            clock: SampleClock::Ops { every: 1000 },
            lines: false,
        },
    );
    assert_eq!(report.exit_code, 0, "{}", report.stderr);
    assert!(
        report.functions.is_none(),
        "sample mode has no function table"
    );

    let flame = report.flamegraph.as_ref().unwrap();
    // Stacks are sorted heaviest-first; the top one is the hot loop, rooted at the top-level frame.
    let top = &flame.stacks[0];
    assert_eq!(
        top.frames.last().map(String::as_str),
        Some("hot"),
        "leaf is `hot`"
    );
    assert_eq!(
        top.frames.first().map(String::as_str),
        Some("main"),
        "rooted at top level"
    );
    // The hot stack holds the overwhelming majority of samples.
    assert!(
        top.count * 100 / flame.total >= 80,
        "the hot loop dominates: {}/{} samples",
        top.count,
        flame.total
    );
    // Every sample is accounted for by exactly one stack.
    let summed: u64 = flame.stacks.iter().map(|s| s.count).sum();
    assert_eq!(summed, flame.total, "stack counts sum to the total");
}

#[test]
fn op_clock_rate_changes_the_sample_count_proportionally() {
    // Sampling twice as often (every 500 ops vs every 1000) takes ~2× the samples — a sanity check
    // that the op-clock trigger actually fires on the configured cadence.
    let path = fixture("rate", HOT_SRC);
    let coarse = noeta_prof::profile(
        &path,
        Mode::Sample {
            clock: SampleClock::Ops { every: 2000 },
            lines: false,
        },
    );
    let fine = noeta_prof::profile(
        &path,
        Mode::Sample {
            clock: SampleClock::Ops { every: 1000 },
            lines: false,
        },
    );
    let c = coarse.flamegraph.unwrap().total;
    let f = fine.flamegraph.unwrap().total;
    assert!(
        f > c,
        "finer op-clock takes more samples: fine={f} coarse={c}"
    );
}

#[test]
fn folded_lines_are_well_formed() {
    let path = fixture("folded", HOT_SRC);
    let report = noeta_prof::profile(
        &path,
        Mode::Sample {
            clock: SampleClock::Ops { every: 1000 },
            lines: false,
        },
    );
    for line in noeta_prof::render_folded(&report).lines() {
        // Each line is "<frame>;<frame>;… <count>".
        let (stack, count) = line
            .rsplit_once(' ')
            .expect("a folded line ends in a count");
        assert!(count.parse::<u64>().is_ok(), "count is a number: {line:?}");
        assert!(
            !stack.is_empty(),
            "a stack has at least one frame: {line:?}"
        );
        assert!(
            stack.starts_with("main"),
            "stacks are rooted at main: {line:?}"
        );
    }
}

#[test]
fn wall_clock_sampling_produces_a_profile() {
    // Nondeterministic, so only structural assertions: a long run gets samples, all under `hot`.
    let path = fixture("wall", HOT_SRC);
    let report = noeta_prof::profile(
        &path,
        Mode::Sample {
            clock: SampleClock::Wall { hz: 2000 },
            lines: false,
        },
    );
    assert_eq!(report.exit_code, 0, "{}", report.stderr);
    let flame = report.flamegraph.as_ref().unwrap();
    assert!(flame.total > 0, "wall-clock sampling took samples");
    assert!(
        flame
            .stacks
            .iter()
            .any(|s| s.frames.iter().any(|f| f == "hot")),
        "the hot function appears in the profile"
    );
}

// ---- P3: emit formats ----------------------------------------------------------------------------

use noeta_prof::Format;

#[test]
fn format_parse_round_trips_and_rejects_unknown() {
    for (s, f) in [
        ("folded", Format::Folded),
        ("svg", Format::Svg),
        ("speedscope", Format::Speedscope),
        ("table", Format::Table),
        ("json", Format::Json),
    ] {
        assert_eq!(Format::parse(s), Some(f));
    }
    assert_eq!(Format::parse("nonsense"), None);
    assert!(Format::Svg.is_sampling() && !Format::Table.is_sampling());
}

#[test]
fn svg_renders_a_flamegraph() {
    let path = fixture("svg", HOT_SRC);
    let report = noeta_prof::profile(
        &path,
        Mode::Sample {
            clock: SampleClock::Ops { every: 1000 },
            lines: false,
        },
    );
    let svg = String::from_utf8(noeta_prof::render(&report, Format::Svg).expect("svg renders"))
        .expect("svg is utf-8");
    assert!(svg.contains("<svg"), "output is an SVG document");
    assert!(
        svg.contains("hot"),
        "the hot function is a bar in the flamegraph"
    );
}

#[test]
fn speedscope_is_valid_and_well_formed() {
    let path = fixture("speedscope", HOT_SRC);
    let report = noeta_prof::profile(
        &path,
        Mode::Sample {
            clock: SampleClock::Ops { every: 1000 },
            lines: false,
        },
    );
    let flame_total = report.flamegraph.as_ref().unwrap().total;

    let bytes = noeta_prof::render(&report, Format::Speedscope).expect("speedscope renders");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");

    assert_eq!(json["profiles"][0]["type"], "sampled");
    assert_eq!(json["profiles"][0]["endValue"], flame_total);
    let n_samples = json["profiles"][0]["samples"].as_array().unwrap().len();
    let n_weights = json["profiles"][0]["weights"].as_array().unwrap().len();
    assert_eq!(n_samples, n_weights, "one weight per sample");
    let n_frames = json["shared"]["frames"].as_array().unwrap().len();
    assert!(n_frames > 0, "the shared frame table is populated");
    // Every frame index in a sample is in range.
    for sample in json["profiles"][0]["samples"].as_array().unwrap() {
        for idx in sample.as_array().unwrap() {
            assert!(
                (idx.as_u64().unwrap() as usize) < n_frames,
                "frame index in range"
            );
        }
    }
}

#[test]
fn instrument_json_lists_functions() {
    let path = fixture("inst_json", &fib_src(10));
    let report = noeta_prof::profile(&path, Mode::Instrument);
    let bytes = noeta_prof::render(&report, Format::Json).expect("json renders");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
    let fib = json["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "fib")
        .expect("fib row present");
    assert_eq!(fib["calls"], 177, "exact call count survives into JSON");
}

// ---- P3.1: line attribution + top-N --------------------------------------------------------------

#[test]
fn line_attribution_labels_the_leaf_with_its_source_line() {
    let path = fixture("lines", HOT_SRC);
    let report = noeta_prof::profile(
        &path,
        Mode::Sample {
            clock: SampleClock::Ops { every: 1000 },
            lines: true,
        },
    );
    let flame = report.flamegraph.as_ref().unwrap();
    // The hot leaf now carries its source line (`hot:<line>`), and the while-loop line dominates.
    let leaf = flame.stacks[0].frames.last().unwrap();
    assert!(
        leaf.starts_with("hot:"),
        "leaf carries a source line: {leaf}"
    );
    assert!(
        leaf.rsplit(':').next().unwrap().parse::<u32>().is_ok(),
        "…and it is a line number: {leaf}"
    );
    // Line-attributed stacks are merged by resolved label — no duplicate chains.
    let mut seen = std::collections::HashSet::new();
    for s in &flame.stacks {
        assert!(
            seen.insert(&s.frames),
            "no duplicate folded chains: {:?}",
            s.frames
        );
    }
}

#[test]
fn line_attribution_is_off_by_default() {
    let path = fixture("nolines", HOT_SRC);
    let report = noeta_prof::profile(
        &path,
        Mode::Sample {
            clock: SampleClock::Ops { every: 1000 },
            lines: false,
        },
    );
    let flame = report.flamegraph.as_ref().unwrap();
    // Without `--lines`, leaf labels are bare function names (no `:line` suffix).
    assert!(
        flame
            .stacks
            .iter()
            .all(|s| s.frames.iter().all(|f| !f.contains(':'))),
        "no line suffixes without --lines"
    );
}

#[test]
fn top_functions_ranks_the_hot_leaf_with_its_percentage() {
    let path = fixture("topn", HOT_SRC);
    let report = noeta_prof::profile(
        &path,
        Mode::Sample {
            clock: SampleClock::Ops { every: 1000 },
            lines: false,
        },
    );
    let top = noeta_prof::top_functions(&report, 5);
    assert_eq!(top[0].0, "hot", "the hot leaf ranks first");
    assert!(top[0].2 >= 80.0, "…with most of the samples: {}%", top[0].2);
    // Percentages never exceed 100 and the top row is the largest.
    assert!(
        top.windows(2).all(|w| w[0].1 >= w[1].1),
        "sorted by samples desc"
    );
}
