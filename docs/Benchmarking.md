# Benchmarking

Micro-benchmarks live in the file they measure, inside a `@bench` block, the sibling of the `@test` block. On a normal run they strip away; `noeta bench` activates and times them.

```console
$ noeta bench sort.noe
running 2 benchmarks
  sum_100                           412 ns/iter  (1000 iterations)
  sum_10                             38 ns/iter  (200 iterations)

2 ran, 0 failed, 2 total
```

## Writing benchmarks

A benchmark is a function inside a `@bench` block, or one carrying the `@bench fn` annotation. Like tests, it uses `assert` for correctness and *fails* if it aborts:

```noeta
fn sum_to(n: int): int {
    mut total = 0
    for i in 1..=n { total += i }
    return total
}

// sample:start
@bench(iterations: 1000) {
    fn sum_100(): void { assert(sum_to(100) == 5050) }
}

@bench fn sum_10(): void { assert(sum_to(10) == 55) }
```

### Iteration count

Fix a benchmark's iteration count on the block, as `@bench(iterations: 1000) { … }` above, or leave it off and let calibration pick one. Positional `@bench(1000)` and named `@bench(iterations: 1000)` are equivalent, and a single fn can carry its own `#[Bench(iterations: N)]` attribute to override the block's.

The count each fn runs with comes from the first of these that is set:

1. `--iterations N` on the command line, which overrides everything.
2. The fn's own `#[Bench(iterations: N)]` attribute.
3. The block's `@bench(iterations: N)` directive.
4. **Calibration**: a short probe grows the count until one measurement run takes about 50 ms, so fast and slow bodies alike get a statistically meaningful count.

The block directive is **distribution sugar**. `@bench(iterations: N) { … }` stamps the `std.bench.Bench` attribute (`#[Bench(iterations: N)]`) onto each contained fn: validated like any attribute construction, visible to reflection, and stamped already-qualified, so the block form needs no `use`. Writing the attribute yourself takes `use std.bench.Bench`, or the qualified form `#[std.bench.Bench(iterations: 50)]`.

## How timing works

Benchmarks run **sequentially**, unlike tests, which run in parallel. Concurrency would corrupt the timings.

Per-iteration cost is measured with a **two-point** method. The function runs in a counted loop of `N` and of `2N` trips, in fresh isolates, and per-iteration cost is `(t(2N) − t(N)) / N`. Subtracting cancels the fixed per-run overhead (runtime startup, setup evaluation, IR lowering, and the loop's JIT warm-up, which the loop form keeps identical at both points), isolating the body.

Each measurement is the **minimum of three runs**, a robust estimator that also discards the cold first run. IR lowering and bytecode generation happen before the clock starts, so only execution is timed.

A subtraction that comes out at or below zero is a **spoiled sample** rather than a result, and the whole two-point measurement runs up to **three attempts** in all. The method assumes the fixed overhead is the same at both points, and a busy machine can break that assumption by handing the two points different amounts of CPU, which inverts the difference however much work the body does. Only if all three attempts fail is the run reported as having measured nothing.

The reported unit adapts to the magnitude: `ns`, `µs`, `ms`, or `s`.

## Command reference

```text
noeta bench [OPTIONS] [PATH]
```

`PATH` (default `.`) is a file **or a directory**, exactly like [`noeta check`](The-CLI#noeta-check) and [`noeta test`](Testing#command-reference). A directory measures every `.noe` beneath it as its own entry into one report, labelling each result with the file it came from (`src/util.noe::parse_bench`). That is the only way a multi-module project's benchmarks all run, since linking merges a module's declarations without its `@bench` blocks. A file measures just that file.

Baselines stay keyed **per entry file**, so a directory run writes exactly the baselines a per-file run writes, and the two compare against each other.

| Flag | Effect |
|---|---|
| `--iterations <N>` | Override the iteration count for every benchmark, disabling calibration. |
| `--name <FN>` | Run only the named bench fn. Repeatable, exact match; the single-benchmark seam editors use. |
| `--json` | One machine-readable JSON object on stdout: `benches[].{name, iterations, perIterNs, unresolved, message, baselineDeltaPct, baselineNote}`, plus `ran`/`failed`/`regressed`/`ungated`/`total`. |
| `--save-baseline <NAME>` | Save this run's measurements as a named baseline, per entry file, in the noeta cache. Timings are machine-local rather than project artifacts. |
| `--baseline <NAME>` | Compare each result against the named baseline: the report gains `(+5.2% vs NAME)`, the JSON `baselineDeltaPct`. |
| `--max-regress <PCT>` | The CI gate. Exit `1` when any bench regresses more than `PCT`% against the baseline, naming the offenders on stderr, and exit `2` when it could not judge a bench at all (see [when the gate cannot measure](#when-the-gate-cannot-measure)). Without `--baseline` it exits `2`. |
| `--target <NAME>` | Only run when the `bench` tier is live in this `noeta.toml` build target; otherwise no-op with exit `0`. A target may also map `bench` to another **provider** (see [Extending Tiers](Extending-Tiers)). |

Save a baseline on your main branch, then gate pull requests with `noeta bench app.noe --baseline main --max-regress 10 --json`.

### Output and exit codes

```text
running <N> benchmarks
  <name>                    <value>/iter  (<N> iterations)
  <name>                    FAILED: <message>

<ran> ran, <failed> failed, <N> total
```

| Code | Meaning |
|---|---|
| `0` | Every benchmark ran, and under `--max-regress` every one of them was judged and passed. `no benchmarks found` also exits `0`. |
| `1` | A benchmark failed, or `--max-regress` found a regression past the limit. |
| `2` | The command could not do what was asked: an unknown `--baseline`, a baseline it refused to save, or a `--max-regress` gate it could not reach a verdict on. |

With `--baseline`, each line gains the delta against the named baseline:

```console
$ noeta bench sort.noe --baseline main
running 1 benchmark
  sum_100                          2.55 µs/iter  (1000 iterations)  (-5.2% vs main)

1 ran, 0 failed, 1 total
```

### When the gate cannot measure

A benchmark whose body costs less per iteration than the timer can resolve produces **no measurement**. Every attempt at the two-point subtraction lands at or below zero, and the report says so:

```console
$ noeta bench app.noe --baseline main --max-regress 10
running 1 benchmark
  b                                   0 ns/iter  (2000 iterations)  (no comparison vs main: this run measured nothing — no per-iteration cost resolved above the timer noise — raise `--iterations`, give the body more work, or measure on a less contended machine)

1 ran, 0 failed, 1 total
noeta: `b` was not compared, so `--max-regress` could not judge it: this run measured nothing — …
noeta: the regression gate is inconclusive: 1 of 1 benchmark could not be compared, so a pass here would prove nothing (exit 2)
```

No measurement means no delta, and no delta means nothing can have regressed. `--max-regress` therefore **exits `2`** rather than `0`: a gate that could not measure must not pass. Exit `0` would be indistinguishable from "measured, and fine" to the CI system reading it. The same verdict covers every other way a requested comparison does not happen: a benchmark the baseline has no entry for, added since it was saved, and a stored baseline entry of `0` that nothing can be compared against.

The fix is one of the three the note prints. Raise `--iterations` or give the body more work, since a benchmark has to be *measurable*, which is a different thing from slow. Or measure somewhere less contended: on an oversubscribed machine the two points can be handed different amounts of CPU, and no amount of extra work in the body fixes that.

Under `--json` the same fact is `ungated`, the count of benchmarks the gate could not judge, alongside each bench's `unresolved` and `baselineNote`.

A plain `--baseline` run without `--max-regress` is a **report**, and stays exit `0` in all of these cases. It prints why each comparison did not happen. The exit code changes only where the exit code is the product.

> [!NOTE]
> `noeta bench` measures *your program's* `@bench` blocks. It is unrelated to the `criterion` benches the compiler developers run against the VM itself (`cargo bench -p noeta-vm`), which are covered in the [Contributing guide](Contributing).

## See also

- [Testing](Testing) — the `@test` sibling.
- [Dev Tiers](Dev-Tiers) — the tier model and `noeta.toml` targets.
