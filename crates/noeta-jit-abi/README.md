# noeta-jit-abi

The cranelift-free **ABI contract** between the JIT/AOT codegen (`noeta-jit`) and the runtime that executes native bodies (the `noeta_jit_*` helpers, the AOT dispatch binding, and the frame-entry gate — all in `noeta-vm`).

- **Takes in:** nothing beyond `noeta-value` (for `Value`).
- **Emits:** [`FrameLayout`] (the VM's call-frame layout, in byte offsets/sizes, baked into JIT-generated code), the [`CompiledFn`] entry-point type, the `noeta_jit_*` helper name contract, the `CallSiteCache` slot shape, the `OUTCOME_*` return sentinels, and the `AOT_DISPATCH_SYMBOL` contract.

An ahead-of-time binary runs pre-compiled native bodies through a static dispatch table and never JIT-compiles anything (`run_module_aot` binds the table with the JIT off). It needs the runtime *support* this crate defines, but not the ~20 MB Cranelift compiler that produced the bodies. Keeping this surface cranelift-free lets `noeta-vm`'s `aot` feature depend on it without pulling in `noeta-jit`/Cranelift at all. `noeta-jit` depends on and re-exports this crate, so every existing `noeta_jit::FrameLayout`/`CompiledFn`/`OUTCOME_*` path keeps resolving unchanged. `noeta-vm` fills `FrameLayout`'s fields from `offset_of!`/`size_of!` on its own `Frame` type, so a layout change can never desync from the JIT that consumes it (checked by a `noeta-vm` lock test).

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
