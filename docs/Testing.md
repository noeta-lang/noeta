# Testing

Tests live *in the same file as the code they test*, inside a `@test` block. On a normal `noeta run` those blocks strip away — they never compile or execute. `noeta test` activates them and runs each one in isolation.

```console
$ noeta test math.noe
running 2 tests on 8 threads
  ok    adds
  ok    subtracts

2 passed, 0 failed, 2 total
```

Bare `noeta test` runs **every** `.noe` file under the current directory — the command to reach for in a project, since an entry file does not carry its modules' tests. See the [command reference](#command-reference).

## Writing tests

A test is an ordinary function inside a `@test` block. Assert with the prelude `assert`:

```noeta
fn add(a: int, b: int): int { return a + b }

@test {
    fn adds(): void { assert(add(1, 2) == 3) }
    fn more(): void { assert(add(2, 2) == 4, "two plus two") }
}
```

There is an equivalent **annotation form** — `@test fn …` is exactly a one-item block:

```noeta
@test fn adds(): void { assert(add(1, 2) == 3) }
```

A test is a named function like any other, so it needs its return type — `fn adds(): void`. `noeta check` reads inside tier blocks, so a missing annotation is an error there rather than a surprise at `noeta test`.

- `assert(cond)` and `assert(cond, msg)` are built in. A test **fails** when its function aborts — a false `assert`, a `panic`, or any runtime error — and **passes** when it returns normally.
- Dev-tier functions get **white-box access to private fields** of the module (read, write, construct) — you can test a type's internals without making them `pub`. This access is scoped to dev-tier functions only; ordinary code still cannot touch a private field.

## A test that does not compile is a `noeta check` error

A `@test` block is stripped from a normal build, but it is not invisible to the compiler: [`noeta check`](The-CLI#noeta-check) checks every file once as it ships *and* once with its own `@test` blocks activated, so a type error inside a test is reported by the ordinary compile feedback loop, with no `--target` and no test run.

```console
$ noeta check .
checked 1 file (tiers: test): 1 error(s), 0 warning(s)
```

The summary names the tiers it looked inside, so a green `noeta check` means the tests compile too. What it does *not* mean is that they pass — that is still `noeta test`, and the two questions stay separate.

## Isolation and concurrency

- The activated program is type-checked **once**, so a broken test is one compile error, not one per run.
- Each test runs as `<shared setup> + <call the test fn>` in a **fresh real-host isolate**, so one test can never observe another's state. The shared setup is the file's declarations, globals, and every top-level effect that *finishes* — see [Shared setup](#shared-setup) below.
- Tests run **in parallel** across worker threads (default: the machine's parallelism, capped at the test count). Results are gathered by declaration index and reported in **declaration order**, deterministically, regardless of finish order.
- A test's stdout is **captured**: hidden when it passes, printed under the failure when it does not. That includes anything an [isolate](Concurrency#what-an-isolate-prints) the test spawned wrote — a worker's `echo` follows the same rule as the test's own, and lands where the test joined the worker.
- By default all tests run even after a failure. `--fail-fast` stops after the first failure — or the first timeout — and drains the workers.
- Every test is bounded by a **per-test deadline** (default 60 s). One test that never returns can no longer swallow the whole run: it is reported `TIME`, every other test still reports, and the run exits `1`. See [Timeouts](#timeouts).

## Shared setup

There is no fixture attribute — **shared setup is the file's top-level declarations**. A top-level binding is part of the setup every test runs on, and because each test gets a fresh isolate, the fixture is rebuilt per test: one test mutating it can never leak into another. Named functions do not see a top-level binding implicitly — capture it with a `use (…)` clause in the signature (see [Functions & Closures](Functions-and-Closures)), in test fns and helpers alike:

```noeta
users = ["ada", "grace"]                 // shared setup — rebuilt for every test

fn count_users() use (users): int { return users.len() }

echo "starting up"                       // a main effect — does NOT run under `noeta test`

@test {
    fn sees_fixture(): void { assert(count_users() == 2) }
    fn sees_fixture_directly() use (users): void { assert(users[0] == "ada") }
}
```

```console
$ noeta test fixtures.noe
running 2 tests on 2 threads
  ok    sees_fixture
  ok    sees_fixture_directly

2 passed, 0 failed, 2 total
```

The top-level `echo` above prints nothing under `noeta test`; everything that declares, binds, or does work that finishes stays live.

### What runs, and what does not

A top-level statement is shared setup if it declares something, binds something, or performs an effect that **finishes**. The split is by what the statement *does*, not by its shape:

| Kept — shared setup | Dropped |
|---|---|
| `use` imports | `echo …` |
| every declaration (`fn`, `class`, `struct`, `enum`, `trait`, `impl`, `@attribute`) | a call that **does not return**: `os.exit(…)`, `server.serve(…)`, `panic(…)` |
| a binding or destructure: `x = …`, `mut x = …`, `(a, b) = …` | `while true { … }` with no `break` |
| a statement-expression that returns: `conn.migrate(…)`, `log.push(…)` | `return` / `break` / `continue` |
| `if` / `for` / `while`, and `concurrent { … }` | an `if` / `for` / `while` / `concurrent` whose body holds either of the two rows above — it could not finish either |

Dropping the second column is what makes a file with a `main` runnable as a test suite at all: a CLI entry's top-level `os.exit(run())` and a server's `server.serve(…)` would otherwise exit the runner or block it forever.

The runner knows which calls those are because the language says so. A function that does not return declares its return type as [`never`](Type-System#never--the-bottom) — the bottom type — and `os.exit`, `server.serve` and `panic` all do. Nothing is inferred from a name or a statement shape:

```noeta check
conn = db.connect("sqlite::memory:")   // a binding — every test gets a live connection
conn.migrate("migrations")             // returns, so it RUNS — every test gets the schema too

server.serve(8080, fetch)              // declared `never` — dropped, or the runner would block
```

> **When something is dropped that a test needs.** The runner does not stay quiet about it. If a dropped statement writes to a top-level binding that a selected test `use (…)`-captures, the run reports `E0071` naming the statement, the binding, and the test:
>
> ```console [E0071] Warning: this statement is not part of the shared setup (`while true` with no `break` never exits), but it writes to `tick`, which `sees_the_loop` captures — so that test will see it unwritten
> ```
>The fix is to do the work **inside a binding** (`applied = conn.migrate("migrations")`) or in a helper the tests call themselves. Note that a binding runs once **per test**, not once per file, so it must be idempotent against any state that outlives the isolate (a file, a file-backed database) — that is also why `sqlite::memory:` is the well-behaved choice here: each test gets its own connection and therefore its own empty database to migrate.

**What the runner still cannot see.** Divergence is *declared*, not inferred, so a call that reaches a non-returning function indirectly is not recognised — `fn boot(): void { os.exit(0) }` with a top-level `boot()` joins the setup and ends the run. Declare `fn boot(): never` and it is dropped like any other. Likewise a `for` over an endless generator, or a `while` whose condition never becomes false, are kept and would not finish.

## Metadata attributes

Lead a test with any of these `std.test` attributes to change how it runs or is reported. They are not prelude — bring them in with `use std.test.{Skip, Name, Group, Data, Timeout}` (or qualify one inline, `#[std.test.Skip]`). The `use` may sit at the top of the file or at the top of the `@test` block itself; both spellings resolve the same. A block-scoped one binds *inside that block only*, and is dropped with the block on an ordinary `noeta run`/`noeta build`:

| Attribute | Effect |
|---|---|
| `#[Skip]` / `#[Skip("reason")]` | Reported as skipped, never run; never fails the suite. |
| `#[Name("…")]` | Overrides the display name in the report. |
| `#[Group("…")]` | Tags the test for `--group` filtering. |
| `#[Data([…])]` | Parameterized — runs once per row, reported as `name[row]`. |
| `#[Timeout(<seconds>)]` | This test's own deadline, replacing the suite default in both directions. `#[Timeout(0)]` removes it. See [Timeouts](#timeouts). |

```noeta
@test {
    use std.test.{Skip, Name, Group, Data, Timeout}

    #[Skip("flaky until fixed")]
    fn not_ready(): void { assert(false) }

    #[Name("adds to seven")]
    #[Group("fast")]
    fn adds(): void { assert(add(3, 4) == 7) }

    #[Data([1, 2, 3])]
    fn positive(n: int): void { assert(n > 0) }

    #[Timeout(600)]
    fn reindexes_the_whole_corpus(): void { assert(reindex() > 0) }
}
```

Notes on `#[Data]`:

- Each element of the list becomes one test case, passing that value as the function's single argument.
- Rows may be scalars (`int`/`float`/`string`/`bool`), including **negative literals** (`#[Data([-1, -2])]`), and nested lists.
- A row that cannot become a runtime value does not silently vanish — it becomes a case that *fails* with a clear message.

## Timeouts

**Every test is bounded. The default is 60 seconds.** A test that does not finish within its bound is reported `TIME`, the rest of the suite runs and reports as normal, and the run exits `1`.

```console
$ noeta test api.noe
running 3 tests on 3 threads
  ok    parses_the_response
  TIME  streams_a_large_body
        timed out: did not finish within 60s (the suite deadline). Raise it for this test with `#[std.test.Timeout(<seconds>)]` on `streams_a_large_body`, for the whole run with `noeta test --timeout <seconds>`, or remove the bound with `--timeout 0`
  ok    retries_on_503

2 passed, 0 failed, 1 timed out, 3 total
```

A timeout is its own outcome, not a kind of failure. A failing test ran and disagreed with an assertion; a timed-out test *did not finish*, so nothing is known about it either way. They are counted separately in the summary and in `--json` (`"outcome": "timedOut"`, and a top-level `timedOut` count beside `failed`), because they ask for different reactions: fix the code, versus raise the bound or go find the deadlock.

**Why 60 seconds.** It has to be a number that never fires on a legitimate test and always fires on a wedged one. From below: anything a test does that is slow rather than stuck is waiting on an I/O client that already carries its own bound (an HTTP request timeout, a database connect timeout, a subprocess wait), and those are conventionally 30 seconds or less — so a test merely waiting on the world resolves well inside a minute, or fails on its own terms first. From above: 60 seconds turns a wedged suite into a named failure a minute later instead of the 25+ minutes of silence that motivated the rail.

### Raising it

A long-running test is a legitimate thing to have, so put the bound on the test that needs it:

```noeta
@test {
    use std.test.{Timeout}

    #[Timeout(600)]                       // ten minutes, for this test only
    fn reindexes_the_whole_corpus(): void { assert(reindex() > 0) }

    #[Timeout(0)]                         // no bound at all, for this test only
    fn runs_until_the_operator_stops_it(): void { assert(drain() == 0) }
}
```

`#[Timeout(N)]` replaces the suite default for that test in **both** directions — `#[Timeout(5)]` is a real 5-second bound even under `--timeout 600`, so the number you wrote is the number that applies. A `#[Data]` test's rows each get the fn's bound, since every row is a separate run of the same test.

For a whole run — a slow CI box, or a deliberately unbounded debugging session — use the flag: `noeta test --timeout 300`, or `noeta test --timeout 0` to switch the rail off entirely.

The deadline is **wall clock**, and it counts the time a test spends waiting for a core as readily as the time it spends using one. A compute-heavy test that finishes in two seconds alone can exceed a four-second bound when the suite runs it beside a dozen siblings on a loaded machine — so size a bound against the loaded case, not the solo one. That is part of why the default is generous.

### What the runner can and cannot do to a test that overruns

When the deadline expires the runner **asks the test to stop**, waits a one-second grace for it to actually stop, and abandons it only if that grace expires. The report is written either way, and the rest of the suite runs either way — that half never depended on the test cooperating.

Almost every overrunning test stops. A test that is *running* — spinning in a loop, recursing, grinding through work — reaches a safepoint within an iteration, unwinds from there, and tears its isolate down exactly as a finished test does: its destructors run, its heap goes back to zero, and anything it spawned is cancelled and joined with it. Its thread is then joined, so nothing is left behind at all. Two things follow that are worth knowing:

- **A stopped test's cleanup really runs.** If your test holds something with a `destruct`, that `destruct` fires on the way out. A test that overruns is not a test whose cleanup is skipped.
- **It costs the run nothing.** No leaked thread, no core burning for the rest of the suite, no held files or sockets.

One class cannot be stopped: **a test blocked inside a native call** — a socket read, a pipe read with no writer, a subprocess wait. That thread is not executing Noeta, so no safepoint comes around and the request cannot land. Its thread is abandoned, it keeps running until the process exits, and everything its isolate owns is held until then. The report says so explicitly rather than leaving you to guess, because it changes what you do about it: the fix is a deadline on the *operation* — the read's own timeout — not a bigger bound on the test around it.

```console
  TIME  streams_a_large_body
        timed out: did not finish within 60s (the suite deadline). … It was asked to stop and did not, so its thread was abandoned: it keeps running — holding its isolate, its heap and any open files or sockets — until this run exits. A test that will not stop is blocked inside a native call (a socket or pipe read, a subprocess wait) that no safepoint can reach; put the deadline on that operation rather than on the test around it
```

Abandoning is still the right trade for that class by a wide margin: a leaked thread that lets the suite finish and *name* the culprit beats a tidy suite that never returns. And it is safe to leave behind, because a `@test` case is a whole program on its own thread with its own heap — nothing in the runner ever frees what it is still using, so it is leaked rather than freed-out-from-under, and the process exit does not touch it.

Two more consequences to know about:

- **A stopped test reports what it managed to print; an abandoned one reports nothing.** A test that stops when asked unwinds and runs its ordinary teardown, so its captured output is complete as far as it got — and that is printed under the `TIME` line, because "it got this far and then wedged" is the most useful thing a timed-out test can tell you. An abandoned test is still running and has produced no result to read, so its output is empty rather than partial.
- **The process still exits promptly.** Abandoned threads are detached, so neither the worker pool nor the runner's own teardown waits on them. The grace is waited out per worker rather than per test, so a suite with many wedged tests pays it once, in parallel.

**A bounded test gives up nothing in speed.** Being stoppable means staying somewhere with safepoints, and for a while that ruled out the JIT: native code carried no cancellation check, so a run that could be cancelled had to decline on-stack replacement and leave its hot loops in tier 0 — measured, a 200-million-iteration counting loop took **6.53 s** under a bound against **0.64 s** without one, a 10× tax. That is gone. The JIT now emits a cancellation check at every loop header, in bodies compiled for a cancellable run only, and the same loop runs at **0.76 s bounded against 0.66 s unbounded** — the check costs about half a nanosecond per iteration, against the ten-fold penalty it replaced. A bounded test tiers up exactly like an unbounded one.

The check costs an ordinary program nothing at all, and not merely nothing measurable: whether to emit it is decided when the compiler is built, from whether the run carries a cancellation flag, so a program that cannot be cancelled produces the same machine code it did before any of this existed. `noeta bench` is unaffected for the same reason — it does not run under the rail.

## Command reference

```text
noeta test [OPTIONS] [PATH]
```

`PATH` (default `.`) is a file **or a directory**, exactly like [`noeta check`](The-CLI#noeta-check).

- A **directory** is walked recursively and every `.noe` file runs as its own entry, with the outcomes aggregated into one report and one exit code. Each test is labelled with the file it came from (`src/util.noe::doubles`), so the same name in two modules stays distinguishable.
- A **file** runs only that file's `@test` blocks.

Run the whole project by default. Naming a single entry file tests *only that file*: linking merges a module's reachable declarations into its importer, never its `@test` blocks, so `noeta test src/main.noe` on a two-module project reports the entry's tests alone and the module's never run — silently, and looking like a green suite.

A file that fails to type-check renders its own diagnostic and fails the run, but does not stop the remaining files; the summary names how many were skipped that way.

| Flag | Effect |
|---|---|
| `--fail-fast` | Stop after the first failing test. |
| `-j, --jobs <N>` | Number of tests to run concurrently (default: machine parallelism). |
| `--group <NAME>` | Run only tests tagged `#[Group("<NAME>")]`. |
| `--name <NAME>` | Run only the test function(s) with this exact name. Repeatable; composes with `--group`. (This is the seam the editor test explorer uses.) |
| `--json` | Emit a single machine-readable JSON report on stdout instead of the human table (per-test stdout captured, not interleaved) — for CI and editors. Each test carries `outcome` (`"passed"` / `"failed"` / `"timedOut"`) alongside the older `passed` boolean, and the totals carry `timedOut` beside `failed`. |
| `--target <NAME>` | Only run when the `test` tier is live in this `noeta.toml` build target; otherwise no-op with exit `0`. |
| `--timeout <SECONDS>` | The per-test deadline (default `60`). `0` disables it. A per-test `#[Timeout(N)]` wins over this. See [Timeouts](#timeouts). |
| `--watch` | Rerun on every save, narrowed to the tests the edit actually affected. See [Watch mode](#watch-mode). |

### Report format

```text
running <N> tests on <J> threads[, <K> skipped]
  ok    <name>
  FAIL  <name>
        <failure message>
        | <captured stdout line>
  TIME  <name>
        <timeout message: the bound it exceeded, and how to raise it>
  skip  <name> (reason)

<p> passed, <f> failed[, <t> timed out][, <s> skipped][, <n> not run (stopped early)], <total> total
```

A failure prints its message plus any stdout the test produced (prefixed `| `), and the run exits `1`:

```console
$ noeta test math.noe
running 2 tests on 2 threads
  ok    adds
  FAIL  subtracts
        assertion failed: two plus two
        | checking subtraction

1 passed, 1 failed, 2 total
```

### Exit codes

`0` only when nothing failed, nothing timed out, and nothing was left un-run (a `#[Skip]` never fails the suite); `1` otherwise. `no tests found` (or an empty `--group`) exits `0`; a file that cannot be read exits `2`.

## Watch mode

`noeta test --watch app.noe` keeps running and reruns on every save — narrowed to **only the impacted tests**, not everything:

- Edit a leaf function and exactly the tests that transitively call it rerun — across module boundaries; edit one test's body and only that test reruns.
- An **inert** edit — reformatting between declarations, a comment — runs nothing.
- Edits the engine cannot attribute (a signature change, a changed top-level statement, a new or deleted file, a manifest change, red code) fall back to a full rerun, with the reason printed.

`--watch` works on any command (`noeta run --watch`, `noeta bench --watch`, `noeta serve --watch`); the full watch story — how the impact filter works and its dynamic-dispatch caveat — is at [The CLI](The-CLI#noeta-serve-and---watch).

## See also

- [Benchmarking](Benchmarking) — the `@bench` sibling of `@test`.
- [Dev Tiers](Dev-Tiers) — the tier model these blocks belong to, and `noeta.toml` targets.
- [Attributes & Reflection](Attributes-and-Reflection) — how `#[…]` attributes work in general.
