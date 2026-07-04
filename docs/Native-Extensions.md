# Native Extensions

Native modules (like `math`, `json`, `fs`) are not hardcoded into the runtime — they are registered through one uniform seam, and the core `std` modules are the *dogfooded* extension registered *through* that seam rather than special-cased.

> [!NOTE]
> This is implementation/runtime plumbing. There is **no third-party package system yet**, so you cannot ship your own native extension crate today — the registry is the internal mechanism, and the API is being proven by having core use it. The `noeta.toml` provider grammar exists and is validated, but the only accepted provider is `"std"` (see [Documentation & Dev Tiers](Documentation-and-Tiers#build-profiles--noetatoml)).

## Why a registry

Hardcoding native modules created four parallel seams that could drift: a `NativeModule` enum, per-backend `call_vec`/`call_json`/… dispatch, and checker tables of known modules. The registry dismantles all four into one mechanism — and it makes differential agreement *structural*: one shared dispatch function per module, not two mirrored copies. The design test the work held itself to: *could `vec`/`quat` be deleted from core and re-added as a third-party crate with no API change?*

## The seam

The registry (`noeta-stdlib`, `registry.rs`) is built on a **neutral value-marshalling** layer:

- `NativeValue` — the argument view: `Scalar`, `Str`, `Bytes`, `Object { fields }`, `Packed { layout, bytes }`, `List`, and so on.
- `NativeOut` — the result view.

Two per-backend functions, written once each — `marshal_native_arg(&Value) -> NativeValue` and `materialize_native(NativeOut, …) -> Value` — replace all the duplicated dispatch. A module function is then just a `DispatchFn = fn(&mut dyn Host, &[NativeValue]) -> Result<NativeOut, StdError>`, **shared across both backends** so the differential holds by construction. The `Host` capability (see below) is threaded through so `fs`/`time`/`random`/`env`/`args` migrate too (pure modules ignore it).

Registration is declarative:

```
trait Extension {
    name;
    modules() -> &[ExtModule];   // each with ExtFn { name, params, ret, dispatch }
    types()   -> &[ExtType];
}
```

`params` and `ret` use `SigType`, a small signature vocabulary (noeta-stdlib cannot see the checker's `Type`); `noeta-check` maps each `SigType` to a real `Type`, so the registry is the single source of truth that *both* the checker and both backends read.

## The `Host` capability

All host-coupled effects — filesystem, clock, PRNG, `env`/`args` — go through one `Host` trait. Two implementations exist: `SandboxHost` (deterministic in-memory VFS, logical clock, seeded RNG — what the differential always runs) and `RealHost` (real disk, real env, per-isolate tokio — what `noeta run` uses, never differential-tested). This is the same "simulate deterministically, deploy real" split as the async executor and isolate scheduler.

## Case study: `json.parse::<T>`

The motivating consumer is a native function that builds a value of a type named *only at the call site* — something a user genuinely cannot express in-language. The grammar `module.func::<T>(args)` is an atom (`Expr::TypedModuleCall`). The checker resolves `T` into a neutral `TypeRecipe` (scalar / option / list / string-keyed map / declared-order struct), and a shared lowering bakes it into an `ExtCall` IR node the VM transcribes to `Op::ExtCall`. Both backends marshal the arguments, run the shared recursive `json::parse_typed(text, &recipe)`, and materialize the result — the reference interpreter through its real registered type, the VM through a fresh same-name shape (method dispatch is name-keyed) — so they agree by construction.

## Status

- **Shipped (Phases A + B):** the registry and neutral marshalling seam; `math`/`random`/`time`/`env`/`args`/`fs`/`vec`/`quat`/`json` all migrated onto it; the old `NativeModule` enum deleted; `json.parse::<T>` working end to end.
- **Deferred:** `ExtType` (native first-class *types*, like an `Image`), columnar kernels via extensions (blocked on a raw-buffer ABI capability), and the package/dependency manager that would let `vec`/`quat` physically leave core and third parties register their own crates. Extracting a stable `noeta-native` ABI crate is planned for the package-manager milestone.

## See also

- [Standard-Library Modules](Standard-Library-Modules) — the modules registered through this seam.
- [Concurrency Internals](Concurrency-Internals) — the `Host` capability's role in the deterministic/real split.
