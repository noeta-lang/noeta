# Debugging (`noeta dap`)

Noeta ships a full source-level debugger: `noeta dap` speaks the [Debug Adapter Protocol](https://microsoft.github.io/debug-adapter-protocol/)
over stdio, so any DAP-capable editor — VS Code / VSCodium's built-in debug UI, Neovim
`nvim-dap`, Helix — can run a `.noe` program with breakpoints, stepping, variable inspection, and a
live debug console.

A design point worth knowing up front: **the debugger debugs the production VM.** There is no
separate "debug interpreter" — the same bytecode pipeline `noeta run` uses executes your program,
with the JIT unarmed so every frame is inspectable and a compile flag that keeps variable names.
What you observe under the debugger is what ships.

One scope note, also worth knowing up front: the debugger is **launch-only** — it always starts the
program itself. There is no attaching to an already-running process (see
[Current limitations](#current-limitations)).

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

## Neovim (`nvim-dap`)

The adapter is the same one VS Code spawns: the `noeta` binary run as `noeta dap`, speaking DAP on
stdio, with a launch request carrying `program` (and optionally `stopOnEntry`) — so the
[nvim-dap](https://github.com/mfussenegger/nvim-dap) wiring is:

```lua
local dap = require("dap")

dap.adapters.noeta = {
  type = "executable",
  command = "noeta",          -- or an absolute path to the binary
  args = { "dap" },
}

dap.configurations.noeta = {
  {
    type = "noeta",
    request = "launch",
    name = "Debug Noeta file",
    program = "${file}",
    stopOnEntry = false,
  },
}
```

`dap.configurations` is keyed by *filetype*, so make sure `.noe` files carry one — e.g.
`vim.filetype.add({ extension = { noe = "noeta" } })` — and the key above matches it. The adapter
invocation and launch arguments are taken from the adapter's own source and the VS Code extension's
wiring; the snippet itself has not been exercised end-to-end in Neovim — if it misbehaves for you,
please open an issue.

## What works

| Capability | Notes |
|---|---|
| **Line breakpoints** | Set on any executable line — including lines that compile to spanless instructions (a bare `return x`): a debug-only line table maps every statement. A line with no code (blank, comment) simply doesn't bind. |
| **Conditional breakpoints** | A breakpoint may carry a **condition** — any boolean expression, evaluated in the paused frame's context on each arrival (the same engine watches use, so it reads frame locals and calls program functions). The breakpoint pauses only when the condition is true. A condition that fails to evaluate **stops anyway** and surfaces the error (DAP convention — never silently skip). |
| **Hit-count breakpoints** | A breakpoint may carry a **hit count** — `N` / `>=N` (from the Nth hit), `>N`, `=N`/`==N`, `<N`, `<=N`, or `%N` (every Nth). The count advances each time the location is reached *with its condition true*, so conditions and hit counts compose. An unparseable hit count is reported on the breakpoint and it degrades to every-hit. |
| **Worker-isolate threads** | Each live worker `isolate` appears as its own debug **thread** (named after the spawned function). A breakpoint inside worker code stops on that worker's thread; its stack, locals, watches, and stepping all work there. See [Worker isolates and the all-stop model](#worker-isolates-and-the-all-stop-model). |
| **Stop on entry** | Pause before the first instruction runs. |
| **Stepping** | Step over / into / out, **line-granular**: one press advances one visible source line, calls are skipped or descended into as the mode says. |
| **Call stack** | Every frame with its function name and source position — including frames from functions defined in other modules. |
| **Variables** | Each frame's named locals (and `self` in a method frame) with their current value and type — the same surface type spelling (`List<int>`, `Point`) the LSP hover and REPL `:type` show. Only locals *in scope at the pause* appear; a name declared further down the function isn't shown as bound yet. |
| **Hover** | Hovering an expression in the source while paused evaluates it **read-only** — names, `.field` chains, `[index]`, operators, literals. Anything that would run code is refused (see below). |
| **Watch & debug console** | Full expression evaluation — the console is effectively **a REPL over the paused program** (see next section). A watch panel is evaluated **once per stop**: re-rendering the same watch at the same pause replays its cached result without re-running it, so a full watch panel costs nothing extra between renders (see [Watches are evaluated once per stop](#watches-are-evaluated-once-per-stop)). |
| **Edit variables** | Change a local's value from the Variables panel while paused; the replacement is any console expression (other locals visible), and the resumed program runs with the new value. `self` is not editable. |
| **Panic tracebacks** | An abort replays its diagnostic and call-chain traceback as console output, same rendering as `noeta run`. |

## The debug console is a REPL over the paused program

A watch or console entry is not interpreted by a side evaluator — it is **compiled by the same
compiler that built your program**, into the program's own id-spaces, and run against the paused
frame. That has four practical consequences:

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

## Watches are evaluated once per stop

A watch panel re-renders its expressions constantly — on every step, and whenever the editor
refreshes the view. To keep that free, an **observational** watch (one that only reads — every
top-level statement is an expression) has its rendered result **memoized within a stop**: the first
render runs it, and any repeat render at the same pause replays the cached value without re-running
the fragment. A watch that calls a function with a side effect therefore runs that side effect once
per stop, not once per render — the standard debugger contract that a watch is an observation.

The memo is keyed by the watch's text and the frame it is evaluated against, and it is invalidated
the moment the observed state can change:

- **resuming or stepping** — the next stop is a fresh evaluation;
- **a debug-console (`repl`) entry**, which may mutate program or session state;
- **editing a variable** from the Variables panel (`setVariable`).

After any of these the next render re-evaluates against the changed state, so a watch never shows a
stale value. Debug-console entries and hovers are never memoized: a console entry is an explicit
action you asked to run, and a hover is already read-only and cheap.

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

## Worker isolates and the all-stop model

A program that spawns worker isolates (`isolate f(args)`) is fully debuggable: each live worker
shows up as its own thread in the debugger's Threads view (named after the function it runs), you can
set breakpoints inside worker code, and when one hits, the debugger pauses on **that worker's
thread** — its call stack, locals, watches, and stepping all work exactly as they do on the main
program.

The pause model is **all-stop**: pausing any thread pauses the whole program, and one *Continue*
resumes it — like the default in gdb and many embedded debuggers. This is a deliberate,
sound choice for the debugger, not a limitation of the language:

- Under the debugger, worker isolates run **cooperatively on the debug thread** (a single VM, tier-0,
  JIT unarmed) rather than as the separate OS threads a production `noeta run` uses — the same
  "trade real-runtime detail for full observability" bargain the debugger already makes by disabling
  the JIT. Isolates are semantically identical either way (the language's isolate semantics are
  defined by the cooperative model the differential oracle runs), so you debug the canonical
  behavior.
- All-stop then falls out for free and is sound: with one thread there is no cross-thread state to
  capture from a running sibling, and no risk of a paused worker deadlocking a sibling that is
  waiting on its channel. Pausing one real OS thread while others run would court exactly those
  hazards with a debugger attached.
- Every strand shares the one debug session, so conditional breakpoints, watches, the debug console,
  and `setVariable` all work inside worker frames, not just on the main program.

Threads come and go as isolates spawn and finish; the Threads view updates with `thread` started /
exited events. A thread that is *suspended* (an isolate awaiting a channel, or the main program
awaiting a worker) has no live frames to show while another thread is the one stopped — that is the
nature of cooperative strands, and it is why the stopped thread is the one whose stack you inspect.

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
  stable ids and the running VM atomically adopts the extended program. (Detailed design record:
  the tooling-unification arc, in `plans/` git history.)

## Current limitations

- No logpoint breakpoints, and no data or exception breakpoints. (Conditional and hit-count
  breakpoints **are** supported — see [What works](#what-works).)
- Column-precise breakpoints are not supported; breakpoints are line-granular.
- Under the debugger, worker isolates run cooperatively on the debug thread rather than as separate
  OS threads, and pausing is **all-stop** — see [Worker isolates and the all-stop
  model](#worker-isolates-and-the-all-stop-model). A race that only manifests under true OS-thread
  parallelism is therefore not reproducible under the debugger (as with any single-stepping
  debugger).
- The debugger is launch-only (no attach): it always starts the program itself.
- No reverse debugging (stepping backward / replay).
