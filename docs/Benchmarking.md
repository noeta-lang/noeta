# Benchmarking

Micro-benchmarks live in the file they measure, inside a `@bench` block — the sibling of the `@test` block. On a normal run they strip away; `noeta bench` activates and times them.

```console
$ noeta bench sort.noe
running 2 benchmarks
  sum_100                    412 ns/iter  (1000 iterations)
  sum_10                      38 ns/iter  (200 iterations)

2 ran, 0 failed, 2 total
```

## Writing benchmarks

A benchmark is a function inside a `@bench` block (or the `@bench fn` annotation form). Like tests, it uses `assert` for correctness and *fails* if it aborts:

```noeta
fn sum_to(n: int): int {
    mut total = 0
    for i in 1..=n { total += i }
    return total
}

@bench(iterations: 1000) {
    fn sum_100(): void { assert(sum_to(100) == 5050) }
}

@bench fn sum_10(): void { assert(sum_to(10) == 55) }
```

### Iteration count

Fix a benchmark's iteration count on the block — `@bench(iterations: 1000) { … }`, as in the example above — or leave it off and let calibration pick one. Positional `@bench(1000)` and named `@bench(iterations: 1000)` are equivalent, and a single fn can carry its own `#[Bench(iterations: N)]` attribute to override the block's. The count each fn runs with comes from the first of these that is set:

1. `--iterations N` on the command line (overrides everything).
2. The fn's own `#[Bench(iterations: N)]` attribute.
3. The block's `@bench(iterations: N)` directive.
4. **Calibration**: a short probe grows the count until one measurement run takes ~50ms, so fast and slow bodies alike get a statistically meaningful count automatically.

Under the hood the block directive is **distribution sugar**: `@bench(iterations: N) { … }` stamps the `std.bench.Bench` attribute (`#[Bench(iterations: N)]`) onto each contained fn — validated like any attribute construction, visible to reflection, and stamped already-qualified so the block form needs no `use`. Writing the attribute yourself is what needs `use std.bench.Bench` (or the qualified form, `#[std.bench.Bench(iterations: 50)]`) — it is not prelude.

## How timing works

- Benchmarks run **sequentially** — concurrency would corrupt the timings — unlike tests, which run in parallel.
- Per-iteration cost is measured with a **two-point** method: the function is run in a counted loop of `N` and `2N` trips (in fresh isolates), and per-iter cost is `(t(2N) − t(N)) / N`. Subtracting cancels the fixed per-run overhead (runtime startup, setup evaluation, IR lowering — and the loop's JIT warm-up, which the loop form keeps identical at both points), isolating the body.
- Each measurement is the **minimum of three runs**, a robust estimator that also discards the cold first run.
- IR lowering and bytecode generation happen **before the clock starts** — only execution is timed.

The reported unit adapts to the magnitude: `ns`, `µs`, `ms`, or `s`.

## Command reference

```text
noeta bench [OPTIONS] <FILE>
```

| Flag | Effect |
|---|---|
| `--iterations <N>` | Override the iteration count for every benchmark (disables calibration). |
| `--name <FN>` | Run only the named bench fn (repeatable, exact match) — the single-benchmark seam editors use. |
| `--json` | One machine-readable JSON object on stdout (`benches[].{name, iterations, perIterNs, message, baselineDeltaPct}`, plus `ran`/`failed`/`total`). |
| `--save-baseline <NAME>` | Save this run's measurements as a named baseline (per entry file, in the noeta cache — timings are machine-local, not project artifacts). |
| `--baseline <NAME>` | Compare each result against the named baseline: the report gains `(+5.2% vs NAME)`, the JSON `baselineDeltaPct`. |
| `--max-regress <PCT>` | The CI gate (requires `--baseline`): exit `1` when any bench regresses more than `PCT`% against the baseline, naming the offenders on stderr. Save a baseline on your main branch, gate PRs with `noeta bench app.noe --baseline main --max-regress 10 --json`. |
| `--target <NAME>` | Only run when the `bench` tier is live in this `noeta.toml` build target; otherwise no-op with exit `0`. A target may also map `bench` to another **provider** (see [Documentation & Dev Tiers](Documentation-and-Tiers)). |

### Output and exit codes

```text
running <N> benchmarks
  <name>                    <value>/iter  (<N> iterations)
  <name>                    FAILED: <message>

<ran> ran, <failed> failed, <N> total
```

Exit `0` when nothing failed, `1` otherwise. `no benchmarks found` exits `0`.

With `--baseline`, each line gains the delta against the named baseline:

```console
$ noeta bench sort.noe --baseline main
running 1 benchmark
  sum_100                          2.55 µs/iter  (1000 iterations)  (-5.2% vs main)

1 ran, 0 failed, 1 total
```

> [!NOTE]
> `noeta bench` measures *your program's* `@bench` blocks. It is unrelated to the `criterion` benches the compiler developers run against the VM itself (`cargo bench -p noeta-vm`) — those are covered in the [Contributing guide](Contributing).

## See also

- [Testing](Testing) — the `@test` sibling.
- [Documentation & Dev Tiers](Documentation-and-Tiers) — the tier model and `noeta.toml` targets.
