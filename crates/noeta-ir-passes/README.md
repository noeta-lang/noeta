# noeta-ir-passes

Precise-reference-counting analyses and transforms over the Core IR.

- **Takes in:** the Core IR (`noeta-ir`).
- **Emits:** the same IR, annotated with reference-counting decisions — last-use/liveness facts, inserted `drop`s, and in-place-reuse tokens.

This crate hosts the passes that make memory management *compiled, not traced*. Because the annotated IR is the single program both backends execute, prompt reclamation lands in both at the same points by construction.

- **`liveness`** — a structured backward dataflow computing, for each named source variable, the point(s) of its last use. (ANF temporaries are single-use, so their last use is trivial; the dataflow concerns the named bindings, which may be read across branches and loops.)
- **`drops`** — drop insertion with three placement rules of increasing coverage: last use (a value dropped right after its final read), scope exit (owned locals still live at a scope's end, dropped in reverse-construction order), and early exit (values abandoned at `return`/`break`/`continue`).
- **`reuse`** — threads in-place-reuse tokens onto constructors whose input allocation is dead at the construction point (`acc ~= [x]`, `Type { ...acc, f: v }`, `m.set(k, v)`, `x.f = v`), so both backends reuse the storage instead of allocating afresh.

The load-bearing safety direction: every analysis here is **conservative in the "never too early" direction**. Where flow makes a last use uncertain, the value is treated as still live (its drop omitted, reclaimed later by scope teardown). A late drop costs only promptness; an early drop would be a use-after-free and must be impossible by construction. Static analysis is an optimization input — correctness always rests on the runtime refcount plus scope teardown — so a bug in any pass here can cost performance, never memory safety.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
