# lang-gc

Garbage collection: the runtime-wide memory-management floor.

- **Takes in:** `Value`/`Color` (from `lang-value`).
- **Emits:** the GC policy — `retain`/`release` over the refcount primitives, plus `CycleCollector`, a Bacon–Rajan synchronous trial-deletion cycle collector.

M1's GC is refcount + a cycle collector (architecture §5). This crate owns the *policy* (when to free; the trial-deletion mark→scan→gather→free); the unsafe refcount/graph *mechanism* lives in `lang-value`'s heap. `__destruct` ordering is the VM's job (a destructor needs the interpreter). The cycle collector is correct and `miri`-tested but not yet wired into `release` — the current language can't form a cycle (objects are immutable after construction), so there is no cyclic garbage; it activates once field mutation lands. The `gc-arena` tracing path (a throughput optimization for destructor-free classes) is a documented deferral — see `plans/m1/slice-06-gc.md`.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
