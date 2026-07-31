# noeta-jit

The Tier-1 method JIT: a [Cranelift](https://cranelift.dev/) backend that compiles hot prototypes to native machine code, sitting on top of the Tier-0 register VM (`noeta-vm`).

- **Takes in:** a `noeta-bytecode` `Module` + a prototype index (the VM decides *what* gets hot via entry/back-edge counters); the VM's `FrameLayout` (baked byte offsets for the frame/`Vec` internals), its runtime-helper function pointers, and — for a run that can be cancelled — the run's cancellation flag.
- **Emits:** finalized native entry points the VM calls instead of its dispatch loop — the normal entry `(vm, regs, base, globals, frames, regs_vec, entry_pc) -> outcome` and, for eligible prototypes, a **fast-convention** entry that takes its arguments as machine arguments and returns `(outcome, value)`.

Everything lives behind the `jit` cargo feature: a `--no-default-features` build pulls in zero Cranelift crates, and the sandbox/differential baseline never runs native code.

## How it works, in five contracts

1. **Deopt is a pc-return.** Compiled code runs on the interpreter's own contiguous register stack. At any op it doesn't compile — or any failed guard — it makes the window tier-0-valid and *returns the bytecode pc*; the interpreter resumes there. Guards bail **before** mutating state, so re-execution is clean. Mid-frame entries (resume-after-call, OSR loop headers) re-enter through guarded init blocks.
2. **Registers live in SSA** (`plan.rs` + the analyses in `lib.rs`). Per-pc liveness, a bare-store heap map, a kind dataflow (`Int`/`Bool`/`Float` → a second *raw* unboxed variable per register), and a slot-hazard map together decide what each sync point must spill. Residency is universal in a modeled prototype — heap values included; overwrites release the old value straight from the variable.
3. **Analysis claims are verified, never trusted, at entries.** The dataflows describe *native-path* state; Tier 0 can differ exactly where native would have bailed (e.g. heap-boxing an overflow). Every mid-frame entry checks the claimed registers against the actual slots and bails on a violation.
4. **Calls use per-site inline caches + a fast convention.** A cache hit pushes the callee frame natively (uninitialized window, baked frame template) and calls the fast body with arguments in machine arguments; the return protocol is emitted inline. The frame stack stays honest — every call pushes a real `Frame` — which keeps deopt and abort-unwinding trivial. Cached closures are pinned by the VM until teardown so bits-equality proves identity.
5. **The oracle is the definition of correct.** `noeta-conformance -- --jit-differential` runs the whole corpus interpreter-vs-forced-JIT and asserts byte-identical output, zero heap residency, and zero refcount anomalies. Native code can't run under miri, so this gate — not miri — owns the JIT's `unsafe` and refcount contracts. `--cancel-poll` runs it a second time with a never-set cancellation flag armed, putting the poll-bearing bodies below under the same gate.

## Cancellation

A run that carries a cancellation flag (a `noeta test` case under its deadline; `RunOptions::cancel` generally) gets a **poll at every loop header** — an `atomic_load` of the flag, branching to that pc's ordinary bail block. It is the native counterpart of the interpreter's taken-back-edge safepoint, and it is what lets a bounded run tier up at all: without it, a cancellable run had to decline on-stack replacement to stay stoppable, at a measured 10× on a loop the JIT could sustain (6.53 s → 0.76 s on a 200M-iteration counting loop).

The poll **never unwinds** — it deopts, and the interpreter decides. That keeps every rule about when a cancellation may be honored (notably: a destructor is uninterruptible) in one place, and means the poll needs no deopt contract beyond the pc-return in contract 1. The flag's address is baked as an immediate, exactly like the frame template, so nothing in `noeta-jit-abi` changed; the engine holds a strong `Arc` clone declared after its `Module`, so the flag outlives the code pages that read it. A run with no flag emits nothing — same bytes as before the poll existed.

`NOETA_JIT_DISASM=1` dumps each compiled prototype's final machine code to stderr — the native analogue of `noeta dump`.

The milestone records (with measurements, dead ends, and the soundness episodes) are the JIT arc ledgers in `plans/` git history, especially the SSA milestone. Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
