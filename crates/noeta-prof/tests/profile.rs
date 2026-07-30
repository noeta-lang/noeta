//! P0 in-process fixtures for `noeta profile`: compile a program tier-0, run it, and assert on the
//! structured [`noeta_prof::Report`]. The profiler is outside the differential oracle (its signal is
//! time, not output), so it is tested this way rather than through the conformance corpus.

/// The counting allocator the `noeta` binary registers — the alloc-profile test needs the same
/// thread-local allocated-bytes counter present in THIS test binary, or the collector reads 0.
#[global_allocator]
static ALLOC: noeta_alloc_probe::TrackingAlloc =
    noeta_alloc_probe::TrackingAlloc(std::alloc::System);

/// Write a one-off program into its own private temp *directory* and return its path. Each program
/// gets its own directory because the loader treats the containing directory as the module directory
/// (M1.9), so sibling test files must not share one.
///
/// Private per *process* as well as per program: the directory used to be
/// `/tmp/noeta_prof_test_<name>`, one path for every checkout and every concurrent test binary on the
/// machine. Nothing here deleted it, so the sharing looked harmless — but `fs::write` truncates
/// before it writes, and a run that read the program inside another run's truncation window profiled
/// an *empty* file: `assertion failed: program stdout is forwarded verbatim, left: ""`. The returned
/// `TempPath` carries its directory's guard, so the tree lives exactly as long as the path does.
fn fixture(name: &str, src: &str) -> noeta_test_temp::TempPath {
    let dir = noeta_test_temp::TempDir::new(&format!("prof-{name}"));
    let file = format!("{name}.noe");
    std::fs::write(dir.join(&file), src).expect("write fixture");
    dir.into_child(file)
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
        jit: false,
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
            jit: false,
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
    assert_eq!(flame.labels(top).last(), Some("hot"), "leaf is `hot`");
    assert_eq!(
        flame.labels(top).next(),
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
            jit: false,
        },
    );
    let fine = noeta_prof::profile(
        &path,
        Mode::Sample {
            clock: SampleClock::Ops { every: 1000 },
            lines: false,
            jit: false,
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
            jit: false,
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
            jit: false,
        },
    );
    assert_eq!(report.exit_code, 0, "{}", report.stderr);
    let flame = report.flamegraph.as_ref().unwrap();
    assert!(flame.total > 0, "wall-clock sampling took samples");
    assert!(
        flame
            .stacks
            .iter()
            .any(|s| flame.labels(s).any(|f| f == "hot")),
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
            jit: false,
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
            jit: false,
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
    // Frames carry the structured source location (speedscope-schema `file`/`line`/`col`), so a
    // consumer can jump to source without parsing the label.
    let hot = json["shared"]["frames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "hot")
        .expect("the hot function is a frame");
    assert!(
        hot["file"].as_str().unwrap().ends_with(".noe"),
        "frame carries its source file: {hot}"
    );
    assert_eq!(hot["line"], 1, "…its 1-based definition line");
    assert_eq!(hot["col"], 1, "…and its 1-based definition column");
}

#[test]
fn frame_table_is_structured_and_line_attribution_overrides_the_leaf_line() {
    let path = fixture("frameinfo", HOT_SRC);
    let report = noeta_prof::profile(
        &path,
        Mode::Sample {
            clock: SampleClock::Ops { every: 1000 },
            lines: true,
            jit: false,
        },
    );
    let flame = report.flamegraph.as_ref().unwrap();

    // Interior `hot` frames don't exist here (hot is always the leaf), but `main` is interior:
    // its frame resolves to the definition site.
    let main = flame
        .frames
        .iter()
        .find(|f| f.label == "main")
        .expect("main frame in the table");
    assert_eq!(main.name, "main");

    // The line-attributed leaf: label `hot:<line>`, `line` = the sampled line (not the definition
    // line 1), bare `name`, a file, and no column (several pcs merge into one line).
    let leaf = flame
        .frames
        .iter()
        .find(|f| f.label.starts_with("hot:"))
        .expect("line-attributed hot leaf in the table");
    assert_eq!(leaf.name, "hot", "name stays bare");
    let line = leaf.line.expect("attributed line");
    assert_eq!(
        leaf.label,
        format!("hot:{line}"),
        "label and structured line agree"
    );
    assert!(line > 1, "the sampled line is inside the body: {line}");
    assert!(
        leaf.file.as_deref().unwrap().ends_with(".noe"),
        "leaf frame carries its file"
    );
    assert_eq!(leaf.col, None, "no single column for a merged line");
}

#[test]
fn speedscope_frame_table_is_deterministic_under_the_op_clock() {
    // The whole artifact — frame table order included — must be byte-identical across op-clock
    // runs, or profile diffs churn.
    let path = fixture("det_speedscope", HOT_SRC);
    let mode = Mode::Sample {
        clock: SampleClock::Ops { every: 1000 },
        lines: false,
        jit: false,
    };
    let a = noeta_prof::profile(&path, mode);
    let b = noeta_prof::profile(&path, mode);
    assert_eq!(
        noeta_prof::render(&a, Format::Speedscope).unwrap(),
        noeta_prof::render(&b, Format::Speedscope).unwrap(),
        "speedscope output is byte-identical across runs"
    );
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
            jit: false,
        },
    );
    let flame = report.flamegraph.as_ref().unwrap();
    // The hot leaf now carries its source line (`hot:<line>`), and the while-loop line dominates.
    let leaf = flame.labels(&flame.stacks[0]).last().unwrap();
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
            jit: false,
        },
    );
    let flame = report.flamegraph.as_ref().unwrap();
    // Without `--lines`, frame labels are bare function names (no `:line` suffix).
    assert!(
        flame.frames.iter().all(|f| !f.label.contains(':')),
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
            jit: false,
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

/// The one-pipeline slice: the profiler resolves dependency packages exactly as `noeta run` does.
/// Before the fix its loader saw siblings only, so a program with a path dependency profiled to an
/// unresolved-import panic while running fine under `noeta run`.
#[test]
fn profiles_a_program_with_a_path_dependency() {
    let base = noeta_test_temp::TempDir::new("prof-path-dep");
    let app = base.join("app");
    let lib = base.join("lib");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\nhi = { path = \"../lib\" }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("main.noe"),
        "use hi.api.answer;\necho \"got=\" ~ answer();\n",
    )
    .unwrap();
    std::fs::write(
        lib.join("noeta.toml"),
        "[package]\nname = \"acme/lib\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        lib.join("api.noe"),
        "pub fn answer(): int { return 42; }\n",
    )
    .unwrap();

    let report = noeta_prof::profile(&app.join("main.noe"), noeta_prof::Mode::Summary);
    assert_eq!(
        report.exit_code, 0,
        "dep resolves under the profiler: {}",
        report.stderr
    );
    assert_eq!(report.stdout, "got=42\n");
}

#[test]
fn instrument_carries_an_exact_call_tree_flamegraph() {
    // `root` fans into two callees with distinct call paths; the instrumenting run must produce a
    // nanosecond-weighted flamegraph whose stacks reflect the exact tree — including fib's
    // recursive self-path — alongside the usual function table.
    let src = "fn fib(n: int): int {\n\
               \x20   if n < 2 { return n; }\n\
               \x20   return fib(n - 1) + fib(n - 2);\n\
               }\n\
               fn spin(n: int): int {\n\
               \x20   mut acc = 0\n\
               \x20   mut i = 0\n\
               \x20   while i < n { acc = acc + i; i = i + 1; }\n\
               \x20   return acc;\n\
               }\n\
               fn root(): int { return fib(14) + spin(400000); }\n\
               echo root();\n";
    let path = fixture("tree", src);
    let report = noeta_prof::profile(&path, noeta_prof::Mode::Instrument);
    assert_eq!(report.exit_code, 0, "{}", report.stderr);

    let flame = report
        .flamegraph
        .as_ref()
        .expect("instrument mode now carries the exact call tree");
    assert_eq!(flame.unit, noeta_prof::FlameUnit::Nanoseconds);
    assert!(flame.total > 0, "nanosecond weights accumulated");

    // The folded chains include the fan-out paths and the recursive fib self-edge.
    let chains: Vec<String> = flame
        .stacks
        .iter()
        .map(|s| flame.labels(s).collect::<Vec<_>>().join(";"))
        .collect();
    let has = |needle: &str| chains.iter().any(|c| c.contains(needle));
    assert!(has("root;spin"), "chains: {chains:?}");
    assert!(has("root;fib"), "chains: {chains:?}");
    assert!(has("fib;fib"), "recursion is a real path: {chains:?}");

    // Exactness: per-path self weights sum to the per-function self times.
    let table_self: u64 = report
        .functions
        .as_ref()
        .unwrap()
        .iter()
        .map(|f| f.self_ns)
        .sum();
    let flame_self: u64 = flame.stacks.iter().map(|s| s.count).sum();
    assert_eq!(flame_self, table_self, "tree weights == table self-times");

    // Both stack-shaped renders work in instrument mode, labeled in ns.
    let folded = noeta_prof::render(&report, noeta_prof::Format::Folded).unwrap();
    assert!(!folded.is_empty());
    let json = noeta_prof::render(&report, noeta_prof::Format::Json).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert!(v["functions"].is_array(), "table rows present");
    assert_eq!(
        v["profiles"][0]["unit"], "nanoseconds",
        "speedscope stacks present, ns unit"
    );
}

#[test]
fn alloc_mode_attributes_bytes_to_the_allocating_paths() {
    // `build_lists` allocates a fresh list per iteration; `lean_math` is pure arithmetic. The
    // memory flamegraph must weight the allocating path FAR above the arithmetic one — that
    // discrimination is the whole point ("who allocates", which a wall-time graph hides).
    let src = "fn build_lists(rounds: int): int {\n\
               \x20   mut total = 0\n\
               \x20   for i in 0..rounds {\n\
               \x20       mut xs = [i, i + 1, i + 2, i + 3]\n\
               \x20       total = total + xs.len()\n\
               \x20   }\n\
               \x20   return total\n\
               }\n\
               fn lean_math(rounds: int): int {\n\
               \x20   mut acc = 0\n\
               \x20   mut i = 0\n\
               \x20   while i < rounds { acc = acc + i * 3; i = i + 1 }\n\
               \x20   return acc\n\
               }\n\
               fn root(): int { return build_lists(20000) + lean_math(20000) }\n\
               echo root()\n";
    let path = fixture("alloc", src);
    let report = noeta_prof::profile(&path, noeta_prof::Mode::Alloc);
    assert_eq!(report.exit_code, 0, "{}", report.stderr);

    let flame = report
        .flamegraph
        .as_ref()
        .expect("alloc mode carries a flamegraph");
    assert_eq!(flame.unit, noeta_prof::FlameUnit::Bytes);
    assert!(
        report.functions.is_none(),
        "alloc mode has no function table"
    );
    assert!(flame.total > 0, "bytes were counted (allocator registered)");

    let weight_of = |needle: &str| -> u64 {
        flame
            .stacks
            .iter()
            .filter(|s| {
                flame
                    .labels(s)
                    .collect::<Vec<_>>()
                    .join(";")
                    .contains(needle)
            })
            .map(|s| s.count)
            .sum()
    };
    let lists = weight_of("root;build_lists");
    let math = weight_of("root;lean_math");
    assert!(lists > 0, "the allocating path carries bytes");
    // 20k list allocations vs a pure-arithmetic loop: orders of magnitude apart. A 20x margin is
    // far below reality (measured ~15000x) but robust against interpreter-internal noise.
    assert!(
        lists > math.max(1) * 20,
        "allocating path dominates: build_lists={lists} lean_math={math}"
    );
}

#[test]
fn isolates_get_their_own_profiles() {
    // Two worker isolates crunch on their own OS threads; the profile must carry one named
    // flamegraph per isolate alongside main's — and the isolates' own work (`crunch` frames) must
    // live in THEIR profiles, not main's.
    let src = "async fn crunch(tx: Sender<int>, n: int): void {\n\
               \x20   mut acc = 0\n\
               \x20   mut i = 0\n\
               \x20   while i < 200000 { acc = acc + i * n; i = i + 1 }\n\
               \x20   tx.send(acc).await\n\
               }\n\
               async fn gather(rx: Receiver<int>): int {\n\
               \x20   mut total = 0\n\
               \x20   mut running = true\n\
               \x20   while running {\n\
               \x20       r = rx.recv().await\n\
               \x20       (v, keep) = match r { some(x) => (x, true), none => (0, false) }\n\
               \x20       total = total + v\n\
               \x20       running = keep\n\
               \x20   }\n\
               \x20   return total\n\
               }\n\
               async fn run(): int {\n\
               \x20   (tx, rx) = channel::<int>(4)\n\
               \x20   mut result = 0\n\
               \x20   concurrent {\n\
               \x20       h = spawn gather(rx)\n\
               \x20       concurrent {\n\
               \x20           isolate crunch(tx, 2)\n\
               \x20           isolate crunch(tx, 3)\n\
               \x20       }\n\
               \x20       tx.close()\n\
               \x20       result = h.await\n\
               \x20   }\n\
               \x20   return result\n\
               }\n\
               echo run().await\n";
    let path = fixture("isolates", src);
    let report = noeta_prof::profile(&path, noeta_prof::Mode::Instrument);
    assert_eq!(report.exit_code, 0, "{}", report.stderr);

    assert_eq!(report.isolates.len(), 2, "one profile per isolate");
    let names: Vec<&str> = report.isolates.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        names.contains(&"isolate crunch #1") && names.contains(&"isolate crunch #2"),
        "harvest-numbered names: {names:?}"
    );
    for (name, flame) in &report.isolates {
        assert_eq!(flame.unit, noeta_prof::FlameUnit::Nanoseconds);
        assert!(flame.total > 0, "{name} accumulated time");
        let chains: Vec<String> = flame
            .stacks
            .iter()
            .map(|s| flame.labels(s).collect::<Vec<_>>().join(";"))
            .collect();
        assert!(
            chains.iter().any(|c| c.contains("crunch")),
            "{name} runs crunch: {chains:?}"
        );
    }
    // The folded artifact roots each isolate's stacks at its display name.
    let folded = noeta_prof::render_folded(&report);
    assert!(
        folded.contains("isolate crunch #1;"),
        "folded has thread-rooted isolate sections:\n{folded}"
    );
}

// ---- Tier-1 (JIT-on) sampling ----------------------------------------------------------------------

/// A long-running, JIT-friendly program: a pure-arithmetic hot loop (`hot`) called many times from
/// the top level, so both the callee and the top-level driver cross the promotion threshold and run
/// native long enough for the wall sampler to land ticks inside the JIT trampoline. Only the
/// `jit`-feature test consumes it (the feature-less tests reuse the smaller `HOT_SRC`).
#[cfg(feature = "jit")]
const TIER1_SRC: &str = "fn hot(n: int): int {\n\
     \x20   mut acc = 0\n\
     \x20   mut i = 0\n\
     \x20   while i < n { acc = acc + i * 2 - (i / 3); i = i + 1; }\n\
     \x20   return acc;\n\
     }\n\
     mut total = 0\n\
     mut k = 0\n\
     while k < 4000 { total = total + hot(20000); k = k + 1; }\n\
     echo total;\n";

/// With the `jit` feature, `noeta profile --jit` arms the production tier-1 JIT: hot prototypes run
/// native and the sampler attributes their wall time at the trampoline, labeled ` [jit]`. We assert
/// on *presence* (the run is statistical), not counts.
#[cfg(feature = "jit")]
#[test]
fn tier1_sampling_labels_the_hot_jit_frame() {
    let path = fixture("tier1_hot", TIER1_SRC);
    let report = noeta_prof::profile(
        &path,
        Mode::Sample {
            clock: SampleClock::Wall { hz: 2000 },
            lines: false,
            jit: true,
        },
    );
    assert_eq!(report.exit_code, 0, "clean tier-1 run: {}", report.stderr);

    // The JIT promoted at least one prototype (the `--jit-stats`-style promotion signal).
    assert!(
        report.jit_compiled.unwrap_or(0) >= 1,
        "the JIT promoted a prototype: {:?}",
        report.jit_compiled
    );

    let flame = report
        .flamegraph
        .as_ref()
        .expect("a sampling run fills a flamegraph");
    let labels: Vec<&str> = flame.frames.iter().map(|f| f.label.as_str()).collect();

    // Some frame is labeled tier-1 (native code got sampled at the trampoline).
    assert!(
        flame
            .frames
            .iter()
            .any(|f| f.label.ends_with(noeta_prof::TIER1_MARKER)),
        "some frame carries the tier-1 marker: {labels:?}"
    );
    // And a tier-1-labeled frame names the hot function — its native time is attributed to `hot`,
    // not misattributed to whatever interpreter frame ran right after native code bailed back.
    assert!(
        flame
            .frames
            .iter()
            .any(|f| f.name == "hot" && f.label.ends_with(noeta_prof::TIER1_MARKER)),
        "the hot function has tier-1 (native) samples: {labels:?}"
    );
    // A tier-1 frame is function-level: its label is exactly `<name> [jit]` (no leaf `:line` suffix —
    // native code merges several source lines per segment, so line-level attribution is withheld).
    for f in &flame.frames {
        if f.label.ends_with(noeta_prof::TIER1_MARKER) {
            assert_eq!(
                f.label,
                format!("{}{}", f.name, noeta_prof::TIER1_MARKER),
                "tier-1 label is `<name> [jit]`, no leaf-line suffix"
            );
        }
    }
    // The tier-1 marker surfaces in the folded artifact too.
    let folded = noeta_prof::render_folded(&report);
    assert!(
        folded.contains(noeta_prof::TIER1_MARKER),
        "folded output carries the tier-1 marker:\n{folded}"
    );
}

/// A tier-0 (`jit: false`) sampling run is unchanged: no prototype is promoted, no frame is
/// tier-1-labeled — exactly the classic profile. Holds regardless of the `jit` feature.
#[test]
fn tier0_sampling_has_no_jit_frames() {
    // A smaller (single-call) hot loop — op-clock, JIT unarmed, so it stays a quick interpreter run.
    let path = fixture("tier0_plain", HOT_SRC);
    let report = noeta_prof::profile(
        &path,
        Mode::Sample {
            clock: SampleClock::Ops { every: 1000 },
            lines: false,
            jit: false,
        },
    );
    assert_eq!(report.exit_code, 0, "clean run: {}", report.stderr);
    assert_eq!(
        report.jit_compiled, None,
        "a tier-0 run promotes nothing (JIT unarmed)"
    );
    let flame = report
        .flamegraph
        .expect("a sampling run fills a flamegraph");
    assert!(
        !flame
            .frames
            .iter()
            .any(|f| f.label.ends_with(noeta_prof::TIER1_MARKER)),
        "no frame is tier-1-labeled on a tier-0 run"
    );
    // The hot function is still attributed (interpreter samples), bare-labeled.
    assert!(
        flame.frames.iter().any(|f| f.name == "hot"),
        "the hot function is still sampled tier-0"
    );
}

/// Without the `jit` feature, `jit: true` is honored but the JIT is a no-op: the run stays observably
/// tier-0 (nothing promoted, no tier-1 frames), and the profile is otherwise a normal sampling run.
#[cfg(not(feature = "jit"))]
#[test]
fn tier1_flag_without_the_feature_stays_tier0() {
    // A quick interpreter run (the JIT is a no-op in this build, so keep the loop small).
    let path = fixture("tier1_nofeat", HOT_SRC);
    let report = noeta_prof::profile(
        &path,
        Mode::Sample {
            clock: SampleClock::Ops { every: 1000 },
            lines: false,
            jit: true,
        },
    );
    assert_eq!(report.exit_code, 0, "clean run: {}", report.stderr);
    assert_eq!(report.jit_compiled, None, "no JIT in this build");
    let flame = report
        .flamegraph
        .expect("a sampling run fills a flamegraph");
    assert!(
        !flame
            .frames
            .iter()
            .any(|f| f.label.ends_with(noeta_prof::TIER1_MARKER)),
        "no tier-1 frames without the jit feature"
    );
}
