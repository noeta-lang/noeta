# lang-gc

Garbage collection: the runtime-wide memory-management floor.

- **Takes in:** `Value` (from `lang-value`).
- **Emits:** the GC policy `retain`/`release` over the refcount primitives.

M1's GC is refcount + (later) a cycle collector. This crate owns the *policy* (when to free, and — from slice M1.6 — `__destruct` ordering and cycle collection); the unsafe refcount *mechanism* lives in `lang-value`. M1.0 implements the acyclic floor only.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
