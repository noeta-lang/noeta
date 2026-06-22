# lang-stdlib

The layered standard library (milestone M1.10).

## What this crate is for

`lang-stdlib` is the home of the always-present standard library: the rich **Ring 1** core surface bound to the language's primitive types, and (as it lands) the thin **Ring 2** always-shipped modules. Ring 3 (regex, crypto, HTTP, timezone date/time, …) is out of scope — it arrives post-M1 through the extension mechanism, not here.

## The load-bearing idea: shared semantics, differential by construction

The project's spine is the **differential oracle**: the M0 tree-walker (`lang-eval`, frozen as the reference) and the M1 VM (`lang-vm`) must produce identical `RunResult`s for every program. Duplicating stdlib logic across the two backends would put that guarantee at the mercy of two hand-kept-in-sync copies.

Instead: **where a Ring 1 operation is expressible over data that is represented *identically* in both runtimes, its semantics live here once and both backends call into it.** Then the two backends agree not because a test caught a divergence, but because there is only one implementation.

Strings are the canonical case. Both backends store a string as a Rust `String` (the tree-walker's `Value::Str(String)` and the VM's `Payload::Str(String)`), so the entire string-method surface lives here:

- [`string_method`]`(recv: &str, method: &str, args: &[Arg]) -> Dispatch` is the single dispatcher.
- [`Arg`] is the backend-agnostic projection of an argument value (only the primitive shapes the stdlib introspects; everything else is `Arg::Other`).
- [`Output`] is the backend-agnostic result each backend lifts back into its own value.
- [`Dispatch`] is `Done(Output)` | `Unknown` (not part of this surface — caller falls through) | `Err(StdError)` (misused).

Each backend is reduced to thin glue: project args onto `Arg`, call `string_method`, lift `Output`. No compiler or bytecode change is involved — a method call already lowers generically and is resolved at runtime (the same dispatch site that handles `count`/`enumerate`).

Collection methods (list/map/set) manipulate backend-specific value representations and so cannot live here wholesale; they are implemented per backend with the differential as the guard, sharing value-agnostic helpers where possible.

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
- **M1.10.2 — todo:** Ring 1 list/map/set methods.
- **M1.10.3 — todo:** Ring 2 modules (json/math/seeded-random first; file IO and time pending a differential-oracle design decision).

See `plans/m1/slice-10-stdlib.md`.
