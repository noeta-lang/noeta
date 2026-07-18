# Testing

Tests live *in the same file as the code they test*, inside a `@test` block. On a normal `noeta run` those blocks strip away — they never compile or execute. `noeta test` activates them and runs each one in isolation.

```console
$ noeta test math.noe
running 2 tests on 8 threads
  ok    adds
  ok    subtracts

2 passed, 0 failed, 2 total
```

## Writing tests

A test is an ordinary function inside a `@test` block. Assert with the prelude `assert`:

```noeta
fn add(a: int, b: int): int { return a + b }

@test {
    fn adds() { assert(add(1, 2) == 3) }
    fn more() { assert(add(2, 2) == 4, "two plus two") }
}
```

There is an equivalent **annotation form** — `@test fn …` is exactly a one-item block:

```noeta
@test fn adds(): void { assert(add(1, 2) == 3) }
```

A test's return type is optional; both `fn adds()` and `fn adds(): void` work.

- `assert(cond)` and `assert(cond, msg)` are built in. A test **fails** when its function aborts — a false `assert`, a `panic`, or any runtime error — and **passes** when it returns normally.
- Dev-tier functions get **white-box access to private fields** of the module (read, write, construct) — you can test a type's internals without making them `pub`. This access is scoped to dev-tier functions only; ordinary code still cannot touch a private field.

## Isolation and concurrency

- The activated program is type-checked **once**, so a broken test is one compile error, not one per run.
- Each test runs as `<shared setup> + <call the test fn>` in a **fresh real-host isolate**, so one test can never observe another's state. The shared setup is the file's declarations and globals with its top-level "main" effects removed — so a top-level `echo` in the file under test does *not* run during `noeta test`.
- Tests run **in parallel** across worker threads (default: the machine's parallelism, capped at the test count). Results are gathered by declaration index and reported in **declaration order**, deterministically, regardless of finish order.
- By default all tests run even after a failure. `--fail-fast` stops after the first failure and drains the workers.

## Metadata attributes

Lead a test with any of these prelude attributes to change how it runs or is reported:

| Attribute | Effect |
|---|---|
| `#[Skip]` / `#[Skip("reason")]` | Reported as skipped, never run; never fails the suite. |
| `#[Name("…")]` | Overrides the display name in the report. |
| `#[Group("…")]` | Tags the test for `--group` filtering. |
| `#[Data([…])]` | Parameterized — runs once per row, reported as `name[row]`. |

```noeta
@test {
    #[Skip("flaky until fixed")]
    fn not_ready(): void { assert(false) }

    #[Name("adds to seven")]
    #[Group("fast")]
    fn adds(): void { assert(add(3, 4) == 7) }

    #[Data([1, 2, 3])]
    fn positive(n: int): void { assert(n > 0) }
}
```

Notes on `#[Data]`:

- Each element of the list becomes one test case, passing that value as the function's single argument.
- Rows may be scalars (`int`/`float`/`string`/`bool`), including **negative literals** (`#[Data([-1, -2])]`), and nested lists.
- A row that cannot become a runtime value does not silently vanish — it becomes a case that *fails* with a clear message.

## Command reference

```text
noeta test [OPTIONS] <FILE>
```

| Flag | Effect |
|---|---|
| `--fail-fast` | Stop after the first failing test. |
| `-j, --jobs <N>` | Number of tests to run concurrently (default: machine parallelism). |
| `--group <NAME>` | Run only tests tagged `#[Group("<NAME>")]`. |
| `--name <NAME>` | Run only the test function(s) with this exact name. Repeatable; composes with `--group`. (This is the seam the editor test explorer uses.) |
| `--json` | Emit a single machine-readable JSON report on stdout instead of the human table (per-test stdout captured, not interleaved) — for CI and editors. |
| `--target <NAME>` | Only run when the `test` tier is live in this `noeta.toml` build target; otherwise no-op with exit `0`. |
| `--watch` | Rerun on every save, narrowed to the tests the edit actually affected. See [Watch mode](#watch-mode). |

### Report format

```text
running <N> tests on <J> threads[, <K> skipped]
  ok    <name>
  FAIL  <name>
        <failure message>
        | <captured stdout line>
  skip  <name> (reason)

<p> passed, <f> failed[, <s> skipped][, <n> not run (stopped early)], <total> total
```

### Exit codes

`0` only when nothing failed and nothing was left un-run (a `#[Skip]` never fails the suite); `1` otherwise. `no tests found` (or an empty `--group`) exits `0`; a file that cannot be read exits `2`.

## Watch mode

`noeta test --watch app.noe` keeps running and reruns on every save — and it does not blindly rerun everything. Each save is diffed against the sources the previous run observed, the changed definitions are walked backwards through the project's call graph, and **only the impacted tests** rerun (through the same `--name` filter above):

- Edit a leaf function and exactly the tests that transitively call it rerun; edit one test's body and only that test reruns.
- This works **across module boundaries**: edit a function in an imported module and the entry's tests that reach it rerun — and an edit to a module function nothing imports reruns nothing at all.
- An **inert** edit — reformatting between declarations, a comment — runs nothing.
- Edits the engine cannot attribute to specific declarations fall back to a full rerun, with the reason printed: a signature or layout change, a changed top-level statement (globals and fixtures live there), a new or deleted `.noe` file, a manifest/lockfile change, or code that does not type-check.

The reachability analysis is static, so tests reached only through dynamic dispatch are matched best-effort (a method call on an untyped receiver is over-approximated by name); rerun without `--watch` occasionally if you lean heavily on reflection-driven dispatch. `--watch` is not specific to `test` — it works on any command (`noeta run --watch`, `noeta bench --watch`, `noeta serve --watch`); see [The CLI](The-CLI) for the full watch story.

## See also

- [Benchmarking](Benchmarking) — the `@bench` sibling of `@test`.
- [Documentation & Dev Tiers](Documentation-and-Tiers) — the tier model these blocks belong to, and `noeta.toml` targets.
- [Attributes & Reflection](Attributes-and-Reflection) — how `#[…]` attributes work in general.
