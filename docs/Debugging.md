# Debugging (`noeta dap`)

Noeta ships a source-level debugger. `noeta dap` speaks the [Debug Adapter Protocol](https://microsoft.github.io/debug-adapter-protocol/) over stdio, so any DAP-capable editor runs a `.noe` program with breakpoints, stepping, variable inspection, and a live debug console. VS Code and VSCodium's built-in debug UI, Neovim's `nvim-dap`, and Helix all speak it.

**The debugger debugs the production VM.** The same bytecode pipeline `noeta run` uses executes your program, with the JIT unarmed so every frame is inspectable and a compile flag that keeps variable names. What you observe under the debugger is what ships.

The debugger is **launch-only**: it always starts the program itself, and there is no attaching to an already-running process. [Current limitations](#current-limitations) lists the rest.

## Quick start (VS Code / VSCodium)

The bundled extension in [`editors/vscode-noeta/`](https://github.com/noeta-lang/noeta/tree/main/editors/vscode-noeta) registers the `noeta` debugger type alongside highlighting and the language server. With it installed, open a `.noe` file and press **F5** to launch the active file under the debugger. Or add a `launch.json`:

```json
{
  "type": "noeta",
  "request": "launch",
  "name": "Debug Noeta file",
  "program": "${file}",
  "stopOnEntry": false
}
```

`program` is the entry file, and its `use` imports resolve as in `noeta run`. `stopOnEntry: true` pauses before the first instruction.

## Neovim (`nvim-dap`)

The adapter is the one VS Code spawns: the `noeta` binary run as `noeta dap`, speaking DAP on stdio, with a launch request carrying `program` and optionally `stopOnEntry`. The [nvim-dap](https://github.com/mfussenegger/nvim-dap) wiring is:

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

`dap.configurations` is keyed by *filetype*, so give `.noe` files one, with `vim.filetype.add({ extension = { noe = "noeta" } })` for instance, and match the key above to it. The adapter invocation and launch arguments come from the adapter's own source and the VS Code extension's wiring; the snippet has not been exercised end to end in Neovim, so please open an issue if it misbehaves.

## What works

| Capability | Notes |
|---|---|
| **Line breakpoints** | Set on any executable line, including lines that compile to spanless instructions such as a bare `return x`: a debug-only line table maps every statement. A line with no code, blank or comment, does not bind. |
| **Conditional breakpoints** | A breakpoint may carry a **condition**, any boolean expression, evaluated in the paused frame's context on each arrival, so it reads frame locals and calls program functions the way a watch does. The breakpoint pauses only when the condition is true. A condition that fails to evaluate **stops anyway** and surfaces the error, per DAP convention. |
| **Hit-count breakpoints** | A breakpoint may carry a **hit count**: `N` or `>=N` (from the Nth hit), `>N`, `=N`/`==N`, `<N`, `<=N`, or `%N` (every Nth). The count advances each time the location is reached *with its condition true*, so conditions and hit counts compose. An unparseable hit count is reported on the breakpoint, which then degrades to every-hit. |
| **Worker-isolate threads** | Each live worker `isolate` appears as its own debug **thread**, named after the spawned function. A breakpoint inside worker code stops on that worker's thread, where its stack, locals, watches, and stepping all work. See [Worker isolates and the all-stop model](#worker-isolates-and-the-all-stop-model). |
| **Stop on entry** | Pause before the first instruction runs. |
| **Stepping** | Step over, into and out, **line-granular**: one press advances one visible source line, and calls are skipped or descended into as the mode says. |
| **Call stack** | Every frame with its function name and source position, including frames from functions defined in other modules. |
| **Variables** | Each frame's named locals, plus `self` in a method frame, with their current value and type, in the same surface type spelling (`List<int>`, `Point`) LSP hover and the REPL's `:type` show. Only locals *in scope at the pause* appear, so a name declared further down the function is not yet shown as bound. |
| **Hover** | Hovering an expression in the source while paused evaluates it **read-only**: names, `.field` chains, `[index]`, operators, literals. Anything that would run code is refused; see [Hover stays side-effect-free](#hover-stays-side-effect-free). |
| **Watch & debug console** | Full expression evaluation, with the console acting as a REPL over the paused program. A watch panel is evaluated **once per stop**: re-rendering the same watch at the same pause replays its cached result, so a full watch panel costs nothing extra between renders. See [Watches are evaluated once per stop](#watches-are-evaluated-once-per-stop). |
| **Edit variables** | Change a local's value from the Variables panel while paused. The replacement is any console expression, with other locals visible, and the resumed program runs with the new value. `self` is not editable. |
| **Panic tracebacks** | An abort replays its diagnostic and call-chain traceback as console output, rendered as `noeta run` renders it. |

## The debug console is a REPL over the paused program

A watch or console entry is **compiled by the same compiler that built your program**, into the program's own id-spaces, and run against the paused frame. Four consequences follow.

**1. The full language works.** Calls, methods, closures, statements:

```text
> xs.filter(fn(x) => x > 15)
[20, 30]
> xs.map(fn(x) => twice(x))
[20, 40, 60]
> mut total = 0; for x in xs { total = total + x }; total
60
```

The fragment sees every local in scope in the selected stack frame, exactly the names the Variables panel shows and `self` included in a method frame, plus every function, type, and global of the program.

**2. Semantics are the program's semantics, by construction.** A console expression compiles through the production pipeline, so operator behavior, method dispatch, arity rules, and error messages match a real run.

**3. What you create can outlive the pause.** A closure built at the console and stored into program state, by rebinding a global callback say, stays alive and callable after you `continue`:

```text
> cb = fn(n: int) => twice(n) + xs.len()
> cb(1)
5
```

When the program later calls `cb(7)` itself, it runs your console-built closure. The runtime keeps every fragment's code resolvable for the rest of the run, so nothing dangles.

**4. Console bindings persist.** A top-level `mut total = …`, or a bare `label = …` introducing a new name, binds a **session global** exactly as a REPL binding does, visible in every later console entry for the rest of the run:

```text
> mut total = xs.len() * 10
> total + 2
32
```

One rule: a console `mut` whose name collides with a **frame local** is refused with "pick another name", since the language forbids shadowing and a silent divergence from what the Variables panel shows would be worse. A `mut` nested *inside* the fragment, in a loop or closure body, stays fragment-local as it would in any function.

Console entries **type-check before running**, against everything the debugged program declared and bound. A retype, a wrong-arity call to a program function, or a missing signature each answers with its `E0xxx` diagnostics and never runs. Frame locals enter the check untyped, so expressions touching only locals are under-constrained rather than over-rejected, and a failed entry leaves no trace in the checker or in the debugged program's diagnostics.

### One nuance about frame locals

Frame locals pass into a fragment **by value**, so `i = 5` typed at the console changes the fragment's copy rather than the paused register. To change a live local, edit it in the **Variables panel** (`setVariable`), which writes the frame register so the resumed program sees the new value.

## Watches are evaluated once per stop

A watch panel re-renders its expressions constantly, on every step and whenever the editor refreshes the view. An **observational** watch, one whose every top-level statement is an expression that only reads, has its rendered result **memoized within a stop**: the first render runs it and any repeat render at the same pause replays the cached value. A watch that calls a function with a side effect therefore runs that side effect once per stop rather than once per render, which is the standard debugger contract that a watch is an observation.

The memo is keyed by the watch's text and the frame it is evaluated against, and it is invalidated the moment the observed state can change:

- **resuming or stepping**, so the next stop is a fresh evaluation;
- **a debug-console (`repl`) entry**, which may mutate program or session state;
- **editing a variable** from the Variables panel (`setVariable`).

After any of those the next render re-evaluates against the changed state, so a watch never shows a stale value. Debug-console entries and hovers are never memoized: a console entry is an explicit action you asked to run, and a hover is already read-only and cheap.

## Hover stays side-effect-free

VS Code evaluates hovers on mouse-over, so a hover must never run code. Hover requests go through the same evaluator, gated twice:

- statically, to the read-only surface of names, members, indexing, operators and literals, so a call or construction is refused before it compiles;
- at run time, for the cases only the receiver's runtime type can decide. `b[0]` where `b`'s type has an `Index` implementation would *run* its `get` method, so hover refuses it while a watch runs it happily.

If a hover shows nothing where you expected a value, evaluate the same expression in a watch or the console, which are allowed to run code.

## Worker isolates and the all-stop model

A program that spawns worker isolates (`isolate f(args)`) is fully debuggable. Each live worker shows up as its own thread in the Threads view, named after the function it runs. You can set breakpoints inside worker code, and when one hits, the debugger pauses on **that worker's thread**, where its call stack, locals, watches, and stepping work as they do on the main program.

The pause model is **all-stop**: pausing any thread pauses the whole program, and one *Continue* resumes it, as in gdb's default and many embedded debuggers. Three facts make that the sound model here:

- Under the debugger, worker isolates run **cooperatively on the debug thread**, one VM at tier-0 with the JIT unarmed, rather than as the separate OS threads a production `noeta run` uses. Isolate semantics are identical either way, since the language defines them by the cooperative model the differential oracle runs, so you debug the canonical behavior.
- All-stop then falls out for free. With one thread there is no cross-thread state to capture from a running sibling, and no risk of a paused worker deadlocking a sibling waiting on its channel.
- Every strand shares the one debug session, so conditional breakpoints, watches, the debug console, and `setVariable` all work inside worker frames.

Threads come and go as isolates spawn and finish, and the Threads view updates with `thread` started and exited events. A thread that is *suspended*, an isolate awaiting a channel or the main program awaiting a worker, has no live frames to show while another thread is the one stopped. That is the nature of cooperative strands, and it is why the stopped thread is the one whose stack you inspect.

## Under the hood

- **Tier-0 execution.** A debug session never arms the JIT, and the interpreter exposes a real program counter to pause at. This is the standard `-O0`-style trade: identical semantics, full observability.
- **Debug info is a side table.** A debug compile records `register → name` for every named local, pinned through register coalescing so the mapping stays 1:1, plus per-statement line tables. Production compiles carry none of this, so the hot path is untouched.
- **Types come from values.** A variable's displayed type is its value's reified runtime type tag, correct even through `dyn`, rendered with the spelling the type checker uses.
- **Console fragments are session compiles.** The launch compile keeps its compiler alive as an incremental session, the same machinery as the REPL. Each console entry appends new code with stable ids, and the running VM atomically adopts the extended program.

## Current limitations

- Breakpoints are line-granular, with no column precision.
- Logpoint breakpoints, data breakpoints and exception breakpoints are absent. Conditional and hit-count breakpoints **are** supported; see [What works](#what-works).
- Under the debugger, worker isolates run cooperatively on the debug thread rather than as separate OS threads, and pausing is **all-stop**; see [Worker isolates and the all-stop model](#worker-isolates-and-the-all-stop-model). A race that only manifests under true OS-thread parallelism is therefore not reproducible under the debugger, as with any single-stepping debugger.
- The debugger is launch-only, with no attach: it always starts the program itself.
- There is no reverse debugging, meaning no stepping backward or replay.
