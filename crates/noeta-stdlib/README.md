# noeta-stdlib

The shared standard-library and host-capability layer.

## What this crate is for

`noeta-stdlib` is the home of the always-present standard library: the rich **Ring 1** core surface bound to the language's primitive types, the thin **Ring 2** always-shipped modules, and the `Host`/`Executor` capability seam both backends route their side effects through. Ring 3 (regex, crypto, HTTP, timezone date/time, …) is out of scope — it arrives through the native-extension mechanism, not here.

## The load-bearing idea: shared semantics, differential by construction

The project's spine is the **differential oracle**: the reference backend (`noeta-eval`, a Core-IR interpreter frozen as the oracle) and the M1 VM (`noeta-vm`) must produce identical `RunResult`s for every program. Duplicating stdlib logic across the two backends would put that guarantee at the mercy of two hand-kept-in-sync copies.

Instead: **where a Ring 1 operation is expressible over data that is represented *identically* in both runtimes, its semantics live here once and both backends call into it.** Then the two backends agree not because a test caught a divergence, but because there is only one implementation.

Strings are the canonical case. Both backends store a string as a Rust `String` (`noeta-eval`'s `Value::Str(String)` and the VM's `Payload::Str(String)`), so the entire string-method surface lives here:

- [`string_method`]`(recv: &str, method: &str, args: &[Arg]) -> Dispatch` is the single dispatcher.
- [`Arg`] is the backend-agnostic projection of an argument value (only the primitive shapes the stdlib introspects; everything else is `Arg::Other`).
- [`Output`] is the backend-agnostic result each backend lifts back into its own value.
- [`Dispatch`] is `Done(Output)` | `Unknown` (not part of this surface — caller falls through) | `Err(StdError)` (misused).

Each backend is reduced to thin glue: project args onto `Arg`, call `string_method`, lift `Output`. No compiler or bytecode change is involved — a method call already lowers generically and is resolved at runtime (the same dispatch site that handles `count`/`enumerate`).

Collection methods (list/map/set) manipulate backend-specific value representations and so cannot live here wholesale; they are implemented per backend with the differential as the guard. What *does* live here for them is the method **set** — the [`ListMethod`] and [`MapMethod`] enums — so each backend's dispatch `match` is exhaustive: a method one backend offers, the other must handle or fail to compile. Their misuse also routes through the shared [`arity_error`]/[`type_error`] builders, so the two backends' diagnostics stay identical.

The **algorithm** can be shared even where the elements cannot. [`ordering::stable_order_by`] is the merge sort `.sorted()` runs when the element type supplies its own `compare`: it answers with a permutation of the indices, which both backends apply to their own element representation. Sharing it is not tidiness — a user comparator need not be a total order, `slice::sort_by` is documented as permitted to *panic* on one that is not, and two independently written sorts would put two different permutations of an inconsistent comparison in front of the differential.

## The Ring 1 string surface

| Method | Arity | Result | Notes |
|---|---|---|---|
| `upper` / `lower` / `trim` | 0 | string | Unicode-correct (`to_uppercase`/`to_lowercase`). |
| `contains` / `starts_with` / `ends_with` | 1 (string) | bool | |
| `split` | 1 (string) | list of strings | An empty separator splits into Unicode scalar characters. |
| `replace` | 2 (string, string) | string | |
| `repeat` | 1 (int) | string | A negative count clamps to the empty string. |

Misuse (wrong arity or wrong argument type) is an [`StdError`] whose `kind` each backend maps onto a diagnostic code — currently `E0007` (type mismatch) — raised at the call span, identically in both runtimes.

## Determinism

Determinism is a hard requirement across the whole stdlib: no wall-clock, no hash-order-dependent output, seeded PRNG only. It is what lets an agent tell a real regression from a flake. The Ring 2 modules that touch time/IO/randomness must honor it and be conformance-enforced.

## Status

- **M1.10.1 — done:** this crate + the Ring 1 string surface, shared by both backends.
- **M1.10.2 — done:** the full Ring 1 collection surface — list `reverse`/`contains`/`join`/`sorted`/`slice`/`first`/`last`/`to_set`, map `keys`/`values`/`has`, and a `Set` type (`contains`/`union`/`intersection`) — implemented per backend and gated by the shared `ListMethod`/`MapMethod`/`SetMethod` enums. `first`/`last` return a built-in `Option`; a `Set` is a canonical sorted+deduped heap value type rendering `{1, 2, 3}`, constructed via `[..].to_set()`. All differential-identical.
- **M1.10.3 — done:** Ring 2 modules via `use std.{...}` (explicit imports, so unused modules tree-shake). The module value is `Value::NativeModule`/`Const::NativeModule`, dispatched through `call_method`. All five:
  - `json` — `parse`/`stringify`; parsing lives here as the shared `json::Json` tree via `serde_json`. A fixed-width integer decodes through `TypeRecipe::IntN`, which accepts a number that fits the declared width and reports one that does not; `Json::Uint` keeps the range past `i64::MAX` exact on the way in, so a `u64` survives the round trip.
  - `math` — `sqrt`/`pow`/`abs`/`floor`/`ceil`/`round`/`min`/`max`/`pi`/`e`; pure scalar functions, so their semantics live here once in [`math::call`] and both backends are project/lift glue, bit-identical.
  - `random` — `seed`/`int`/`float`; a seeded SplitMix64 **pure stepper** in [`random`], so a given seed yields the identical stream in both runtimes (each backend holds the `u64` state and threads it through; it defaults to a fixed seed so even un-seeded use is reproducible).
  - `fs` — file IO over a sandboxed **in-memory** [`fs::Vfs`] each interpreter owns, fresh per run. In-memory rather than a real temp dir so isolation and cross-backend identity are *structural* (no disk flakiness, no cleanup); reading a missing path is `E0021`. (Real-disk and streaming IO now ship too — see the M2 additions below.)
  - `time` — `monotonic`/`sleep`; a **logical** monotonic clock (a per-backend counter, not wall-clock) so output is reproducible and identical across backends. `time.sleep(ms)` therefore advances that counter and returns — it does not block, on any host, including a shipped binary. To actually wait, `await std.task.sleep(ms)`, which parks on the executor's real timer and is cancellable. The distinction is worth stating out loud because the names do not carry it: a synchronous `sleep` that returns immediately is exactly what a reader would not predict.

## Since M1.10 (host IO, concurrency, extensions)

Later milestones grew this crate well past the M1.10 baseline above; all of the following ship:

- **The `Host` capability seam** (`host.rs`) — every host-coupled effect (fs, clock, PRNG, `env`/`args`) goes through one `Host` trait with a deterministic `SandboxHost` (what the differential always runs); the real-disk `RealHost` lives in `noeta-host-real`.
- **`env`/`args`** — process introspection over the host seam (a fixed sandbox fixture; the real environment under `noeta run`). No longer deferred.
- **`fs` streaming + directories + handles** — `read_lines`/`append`, `mkdir`/`is_dir`/`list(dir)`, and the `fs.open` cursor `FileHandle` (`handle.rs`, the first mutable heap value type, shared by both backends), plus `read_bytes`/`write_bytes` and the `*_async` variants.
- **The async `Executor` seam** (`executor.rs`) — a deterministic `SandboxExecutor` (logical time, in-oracle) behind the `async`/`await`, generator, and iterator surfaces; the real tokio `RealExecutor` lives in `noeta-host-real`.
- **`vec`/`quat`** — scalar 3D math plus the autovectorized `soa_*`/`*_all` bulk kernels over packed buffers. The `vec.Kernels`/`vec.SatKernels` method bundles (`vec_kernels.rs`) carry the same operations at every numeric width, as one generic body per op over the `Scalar` element trait (`scalar.rs`).
- **The native-extension registry** (`registry.rs`) — the neutral `NativeValue` marshalling seam through which `math`/`random`/`time`/`env`/`args`/`fs`/`vec`/`quat`/`json` are registered as the dogfooded "std" extension, with one shared dispatch function per module so the differential holds by construction. `json.parse::<T>` decodes into a call-site-named type.

See the wiki's [Standard-Library Modules](../../docs/Standard-Library-Modules.md) and [Native Extensions](../../docs/Native-Extensions.md) pages; the original arc ledgers live in `plans/` git history.
