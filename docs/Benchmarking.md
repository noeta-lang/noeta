# Benchmarking

Micro-benchmarks live in the file they measure, inside a `@bench` block — the sibling of the `@test` block. On a normal run they strip away; `noeta bench` activates and times them.

```console
$ noeta bench sort.noe
running 2 benchmarks
  sum_100                    412ns/iter  (1000 iterations)
  sum_10                      38ns/iter  (200 iterations)

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

The iteration count comes from the first of these that is set:

1. `--iterations N` on the command line (overrides everything).
2. A per-bench `@bench(iterations: N)` directive — positional `@bench(1000)` and named `@bench(iterations: 1000)` are equivalent.
3. The default (200).

## How timing works

- Benchmarks run **sequentially** — concurrency would corrupt the timings — unlike tests, which run in parallel.
- Per-iteration cost is measured with a **two-point** method: the function is invoked `N` and `2N` times (in fresh isolates), and per-iter cost is `(t(2N) − t(N)) / N`. Subtracting cancels the fixed per-run overhead (runtime startup, setup evaluation, IR lowering), isolating the loop body.
- Each measurement is the **minimum of three runs**, a robust estimator that also discards the cold first run.
- IR lowering and bytecode generation happen **before the clock starts** — only execution is timed.

The reported unit adapts to the magnitude: `ns`, `µs`, `ms`, or `s`.

## Command reference

```
noeta bench [OPTIONS] <FILE>
```

| Flag | Effect |
|---|---|
| `--iterations <N>` | Override the iteration count for every benchmark. |
| `--target <NAME>` | Only run when the `bench` tier is live in this `noeta.toml` build target; otherwise no-op with exit `0`. |

### Output and exit codes

```
running <N> benchmarks
  <name>                    <value>/iter  (<N> iterations)
  <name>                    FAILED: <message>

<ran> ran, <failed> failed, <N> total
```

Exit `0` when nothing failed, `1` otherwise. `no benchmarks found` exits `0`.

> [!NOTE]
> `noeta bench` measures *your program's* `@bench` blocks. It is unrelated to the `criterion` benches the compiler developers run against the VM itself (`cargo bench -p noeta-vm`) — those are covered in the [Contributing guide](Contributing).

## See also

- [Testing](Testing) — the `@test` sibling.
- [Documentation & Dev Tiers](Documentation-and-Tiers) — the tier model and `noeta.toml` targets.
