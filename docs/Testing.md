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
- Each test runs as `<shared setup> + <call the test fn>` in a **fresh real-host isolate**, so one test can never observe another's state. The shared setup is the file's declarations and globals with its top-level "main" effects removed — see [Shared setup](#shared-setup) below.
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

Only the file's *main effects* are removed from the shared setup — the top-level `echo` above
prints nothing under `noeta test` — while the declarations and bindings around them stay live.

### What counts as a main effect

The split is by **statement form**, not by what the statement does. A top-level statement is shared
setup if it *binds* something, and a main effect otherwise:

| Kept — shared setup | Dropped — main effect |
|---|---|
| `use` imports, `namespace` | `echo …` |
| every declaration (`fn`, `class`, `struct`, `enum`, `trait`, `impl`, `@attribute`) | a bare statement-expression: `f(x)`, `obj.method(…)` |
| a binding or destructure: `x = …`, `mut x = …`, `(a, b) = …` | `if` / `for` / `while` statements |
| `concurrent { … }` | `return` / `break` / `continue` |

Dropping them is what makes a file with a `main` runnable as a test suite at all: a CLI entry's
top-level `os.exit(run())` and a server's `server.serve()` are both statement-expressions, and
running either under `noeta test` would exit the runner or block it forever.

> **The sharp edge.** A binding is kept, but a *statement* that mutates what it holds is not — so
> the tests see that value in its **unmutated** state, with no diagnostic. This bites hardest on a
> native resource with per-instance state, where the object is real and working but empty:
>
> ```noeta ignore
> conn = db.connect("sqlite::memory:")   // kept — every test gets a live connection
> conn.migrate("migrations")             // DROPPED — a bare statement-expression
>
> @test
> fn counts_users() use (conn): void {
>     // Fails with the database's own `no such table: users`, not a language error:
>     // the connection is fine, the schema was never created.
>     assert(conn.query("SELECT * FROM users", []).len() == 2)
> }
> ```
>
> The same applies to a fixture seeded by a top-level `for` loop — the whole loop is dropped, so
> the binding it seeds is kept but empty.
>
> Do setup work that tests depend on **inside a binding** (`applied = conn.migrate("migrations")`),
> or in a helper the tests call themselves. Note that a binding runs once **per test**, not once
> per file, so it must be idempotent against any state that outlives the isolate (a file, a
> file-backed database) — that is also why `sqlite::memory:` is the well-behaved choice here: each
> test gets its own connection and therefore its own empty database to migrate.

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
