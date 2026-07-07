# Debugging (`noeta dap`)

Noeta ships a full source-level debugger: `noeta dap` speaks the [Debug Adapter Protocol](https://microsoft.github.io/debug-adapter-protocol/)
over stdio, so any DAP-capable editor — VS Code / VSCodium's built-in debug UI, Neovim
`nvim-dap`, Helix — can run a `.noe` program with breakpoints, stepping, variable inspection, and a
live debug console.

A design point worth knowing up front: **the debugger debugs the production VM.** There is no
separate "debug interpreter" — the same bytecode pipeline `noeta run` uses executes your program,
with the JIT unarmed so every frame is inspectable and a compile flag that keeps variable names.
What you observe under the debugger is what ships.

## Quick start (VS Code / VSCodium)

The bundled extension in [`editors/vscode-noeta/`](https://github.com/noeta-lang/noeta/tree/main/editors/vscode-noeta)
registers the `noeta` debugger type alongside highlighting and the language server. With it
installed, open a `.noe` file and press **F5** — the active file launches under the debugger. Or add
a `launch.json`:

```json
{
  "type": "noeta",
  "request": "launch",
  "name": "Debug Noeta file",
  "program": "${file}",
  "stopOnEntry": false
}
```

`program` is the entry file (its `use` imports resolve as in `noeta run`); `stopOnEntry: true`
pauses before the first instruction.

## What works

| Capability | Notes |
|---|---|
| **Line breakpoints** | Set on any executable line — including lines that compile to spanless instructions (a bare `return x`): a debug-only line table maps every statement. A line with no code (blank, comment) simply doesn't bind. |
| **Stop on entry** | Pause before the first instruction runs. |
| **Stepping** | Step over / into / out, **line-granular**: one press advances one visible source line, calls are skipped or descended into as the mode says. |
| **Call stack** | Every frame with its function name and source position — including frames from functions defined in other modules. |
| **Variables** | Each frame's named locals (and `self` in a method frame) with their current value and type — the same surface type spelling (`List<int>`, `Point`) the LSP hover and REPL `:type` show. Only locals *in scope at the pause* appear; a name declared further down the function isn't shown as bound yet. |
| **Hover** | Hovering an expression in the source while paused evaluates it **read-only** — names, `.field` chains, `[index]`, operators, literals. Anything that would run code is refused (see below). |
| **Watch & debug console** | Full expression evaluation — the console is effectively **a REPL over the paused program** (see next section). Repeated watch evaluations are memoized, so stepping with a full watch panel costs nothing extra. |
| **Edit variables** | Change a local's value from the Variables panel while paused; the replacement is any console expression (other locals visible), and the resumed program runs with the new value. `self` is not editable. |
| **Panic tracebacks** | An abort replays its diagnostic and call-chain traceback as console output, same rendering as `noeta run`. |

## The debug console is a REPL over the paused program

A watch or console entry is not interpreted by a side evaluator — it is **compiled by the same
compiler that built your program**, into the program's own id-spaces, and run against the paused
frame. That has three practical consequences:

**1. The full language works.** Calls, methods, closures, statements:

```text
> xs.filter(fn(x) => x > 15)
[20, 30]
> xs.map(fn(x) => twice(x))
[20, 40, 60]
> mut total = 0; for x in xs { total = total + x }; total
60
```

The fragment sees every local in scope in the selected stack frame (exactly the names the Variables
panel shows, `self` included in a method frame), plus every function, type, and global of the
program.

**2. Semantics are the program's semantics, by construction.** There is no "debugger dialect": a
console expression compiles through the production pipeline, so operator behavior, method dispatch,
arity rules, and error messages match a real run exactly.

**3. What you create can outlive the pause.** A closure built at the console and stored into
program state — rebinding a global callback, say — stays alive and callable after you `continue`:

```text
> cb = fn(n: int) => twice(n) + xs.len()
> cb(1)
5
```

…and when the program later calls `cb(7)` itself, it runs your console-built closure. The runtime
keeps every fragment's code resolvable for the rest of the run, so nothing dangles.

**4. Console bindings persist.** A top-level `mut total = …` (or a bare `label = …` introducing a
new name) at the console binds a **session global**, exactly like a REPL binding — visible in every
later console entry for the rest of the run:

```text
> mut total = xs.len() * 10
> total + 2
32
```

One rule: a console `mut` whose name collides with a **frame local** is refused ("pick another
name") — the language forbids shadowing, and silently diverging from what the Variables panel shows
would be worse. A `mut` nested *inside* the fragment (in a loop or closure body) stays
fragment-local, as it would in any function.

Console entries **type-check before running**, against everything the debugged program declared
and bound: a retype, a wrong-arity call to a program function, a missing signature — each answers
with its `E0xxx` diagnostics and never runs. Frame locals enter the check untyped (so expressions
touching only locals are under-constrained rather than over-rejected), and a failed entry leaves no
trace in the checker or the debugged program's diagnostics.

### One nuance about frame locals

Frame locals pass into a fragment **by value**: `i = 5` typed at the console changes the fragment's
copy, not the paused register. To actually change a live local, edit it in the **Variables panel**
(`setVariable`) — that writes the frame register, and the resumed program sees the new value.

## Hover stays side-effect-free

VS Code evaluates hovers on mouse-over, so a hover must never run code. Hover requests go through
the same evaluator, gated twice:

- statically, to the read-only surface — names, members, indexing, operators, literals; a call or
  construction is refused before it compiles;
- at run time, for the cases only the receiver's runtime type can decide: `b[0]` where `b`'s type
  has an `Index` implementation would *run* its `get` method, so hover refuses it (a watch runs it
  happily).

If a hover shows nothing where you expected a value, evaluate the same expression in a watch or the
console — those are allowed to run code.

## Under the hood (short version)

- **Tier-0 execution.** A debug session never arms the JIT; the interpreter exposes a real program
  counter to pause at. This is the standard `-O0`-style trade: identical semantics, full
  observability.
- **Debug info is a side table.** A debug compile records `register → name` for every named local
  (pinned through register coalescing so the mapping stays 1:1) plus per-statement line tables.
  Production compiles carry none of this; the hot path is untouched.
- **Types come from values.** A variable's displayed type is its value's reified runtime type tag —
  correct even through `dyn` — rendered with the same spelling the type checker uses.
- **Console fragments are session compiles.** The launch compile keeps its compiler alive as an
  incremental session (the same machinery as the REPL); each console entry appends new code with
  stable ids and the running VM atomically adopts the extended program. Details in
  `plans/tooling-unification`.

## Current limitations

- No conditional / hit-count / logpoint breakpoints, and no data or exception breakpoints.
- Column-precise breakpoints are not supported; breakpoints are line-granular.
- Debugging real OS-thread `isolate`s is out of scope for now: debug the main isolate; worker
  isolates run to completion undebugged.
- The debugger is launch-only (no attach): it always starts the program itself.
