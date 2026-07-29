# Testing

Tests live *in the same file as the code they test*, inside a `@test` block. On a normal `noeta run` those blocks strip away — they never compile or execute. `noeta test` activates them and runs each one in isolation.

```console
$ noeta test math.noe
running 2 tests on 8 threads
  ok    adds
  ok    subtracts

2 passed, 0 failed, 2 total
```

Bare `noeta test` runs **every** `.noe` file under the current directory — the command to reach for
in a project, since an entry file does not carry its modules' tests. See the
[command reference](#command-reference).

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
- Each test runs as `<shared setup> + <call the test fn>` in a **fresh real-host isolate**, so one test can never observe another's state. The shared setup is the file's declarations, globals, and every top-level effect that *finishes* — see [Shared setup](#shared-setup) below.
- Tests run **in parallel** across worker threads (default: the machine's parallelism, capped at the test count). Results are gathered by declaration index and reported in **declaration order**, deterministically, regardless of finish order.
- By default all tests run even after a failure. `--fail-fast` stops after the first failure and drains the workers.

## Shared setup

There is no fixture attribute — **shared setup is the file's top-level declarations**. A top-level
binding is part of the setup every test runs on, and because each test gets a fresh isolate, the
fixture is rebuilt per test: one test mutating it can never leak into another. Named functions do
not see a top-level binding implicitly — capture it with a `use (…)` clause in the signature (see
[Functions & Closures](Functions-and-Closures)), in test fns and helpers alike:

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

The top-level `echo` above prints nothing under `noeta test`; everything that declares, binds, or
does work that finishes stays live.

### What runs, and what does not

A top-level statement is shared setup if it declares something, binds something, or performs an
effect that **finishes**. The split is by what the statement *does*, not by its shape:

| Kept — shared setup | Dropped |
|---|---|
| `use` imports, `namespace` | `echo …` |
| every declaration (`fn`, `class`, `struct`, `enum`, `trait`, `impl`, `@attribute`) | a call that **does not return**: `os.exit(…)`, `server.serve(…)`, `panic(…)` |
| a binding or destructure: `x = …`, `mut x = …`, `(a, b) = …` | `while true { … }` with no `break` |
| a statement-expression that returns: `conn.migrate(…)`, `log.push(…)` | `return` / `break` / `continue` |
| `if` / `for` / `while`, and `concurrent { … }` | any of the above nested in an `if`/`for`/`while` body |

Dropping the second column is what makes a file with a `main` runnable as a test suite at all: a CLI
entry's top-level `os.exit(run())` and a server's `server.serve(…)` would otherwise exit the runner
or block it forever.

The runner knows which calls those are because the language says so. A function that does not return
declares its return type as [`never`](Type-System#never--the-bottom) — the bottom type — and `os.exit`, `server.serve`
and `panic` all do. Nothing is inferred from a name or a statement shape:

```noeta check
conn = db.connect("sqlite::memory:")   // a binding — every test gets a live connection
conn.migrate("migrations")             // returns, so it RUNS — every test gets the schema too

server.serve(8080, fetch)              // declared `never` — dropped, or the runner would block
```

> **When something is dropped that a test needs.** The runner does not stay quiet about it. If a
> dropped statement writes to a top-level binding that a selected test `use (…)`-captures, the run
> reports `E0071` naming the statement, the binding, and the test:
>
> ```console
> [E0071] Warning: this statement is not part of the shared setup (`while true` with no `break`
> never exits), but it writes to `tick`, which `sees_the_loop` captures — so that test will see it
> unwritten
> ```
>
> The fix is to do the work **inside a binding** (`applied = conn.migrate("migrations")`) or in a
> helper the tests call themselves. Note that a binding runs once **per test**, not once per file,
> so it must be idempotent against any state that outlives the isolate (a file, a file-backed
> database) — that is also why `sqlite::memory:` is the well-behaved choice here: each test gets its
> own connection and therefore its own empty database to migrate.

**What the runner still cannot see.** Divergence is *declared*, not inferred, so a call that reaches
a non-returning function indirectly is not recognised — `fn boot(): void { os.exit(0) }` with a
top-level `boot()` joins the setup and ends the run. Declare `fn boot(): never` and it is dropped
like any other. Likewise a `for` over an endless generator, or a `while` whose condition never
becomes false, are kept and would not finish.

## Metadata attributes

Lead a test with any of these `std.test` attributes to change how it runs or is reported. They are
not prelude — bring them in with `use std.test.{Skip, Name, Group, Data}` (or qualify one inline,
`#[std.test.Skip]`). The `use` may sit at the top of the file or at the top of the `@test` block
itself; both spellings resolve the same. A block-scoped one binds *inside that block only*, and is
dropped with the block on an ordinary `noeta run`/`noeta build`:

| Attribute | Effect |
|---|---|
| `#[Skip]` / `#[Skip("reason")]` | Reported as skipped, never run; never fails the suite. |
| `#[Name("…")]` | Overrides the display name in the report. |
| `#[Group("…")]` | Tags the test for `--group` filtering. |
| `#[Data([…])]` | Parameterized — runs once per row, reported as `name[row]`. |

```noeta
@test {
    use std.test.{Skip, Name, Group, Data}

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
noeta test [OPTIONS] [PATH]
```

`PATH` (default `.`) is a file **or a directory**, exactly like [`noeta check`](The-CLI#noeta-check).

- A **directory** is walked recursively and every `.noe` file runs as its own entry, with the
  outcomes aggregated into one report and one exit code. Each test is labelled with the file it
  came from (`src/util.noe::doubles`), so the same name in two modules stays distinguishable.
- A **file** runs only that file's `@test` blocks.

Run the whole project by default. Naming a single entry file tests *only that file*: linking merges
a module's reachable declarations into its importer, never its `@test` blocks, so
`noeta test src/main.noe` on a two-module project reports the entry's tests alone and the module's
never run — silently, and looking like a green suite.

A file that fails to type-check renders its own diagnostic and fails the run, but does not stop the
remaining files; the summary names how many were skipped that way.

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

A failure prints its message plus any stdout the test produced (prefixed `| `), and the run exits
`1`:

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

`0` only when nothing failed and nothing was left un-run (a `#[Skip]` never fails the suite); `1` otherwise. `no tests found` (or an empty `--group`) exits `0`; a file that cannot be read exits `2`.

## Watch mode

`noeta test --watch app.noe` keeps running and reruns on every save — narrowed to **only the
impacted tests**, not everything:

- Edit a leaf function and exactly the tests that transitively call it rerun — across module boundaries; edit one test's body and only that test reruns.
- An **inert** edit — reformatting between declarations, a comment — runs nothing.
- Edits the engine cannot attribute (a signature change, a changed top-level statement, a new or deleted file, a manifest change, red code) fall back to a full rerun, with the reason printed.

`--watch` works on any command (`noeta run --watch`, `noeta bench --watch`, `noeta serve --watch`); the full watch story — how the impact filter works and its dynamic-dispatch caveat — is at [The CLI](The-CLI#noeta-serve-and---watch).

## See also

- [Benchmarking](Benchmarking) — the `@bench` sibling of `@test`.
- [Dev Tiers](Dev-Tiers) — the tier model these blocks belong to, and `noeta.toml` targets.
- [Attributes & Reflection](Attributes-and-Reflection) — how `#[…]` attributes work in general.
