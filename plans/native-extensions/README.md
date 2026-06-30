# Native-extension / module system — design

**Goal.** One uniform Rust API by which a crate registers **native modules** (and, later, **first-class
types**) into the language — so core's `json`/`math`/`fs`/`vec`/`quat`/… become the dogfooded **"std"
extension** registered *through* the registry, not hardcoded special cases.

**First real consumer.** `json.parse::<T>(s) -> T` — typed deserialization. The honest motivation for
module-function turbofish: a native function that builds a value of a type named only at the call site,
and something a user genuinely **cannot** write in-language. (We dropped `from_axis_angle` — its
*behaviour* is in-language-trivial with `math.sin/cos`, so it was a synthetic forcing function, not a
real need. A quaternion constructor, if wanted, is a user/library function.)

**Branch.** `packed-types` (unmerged). Status at design time: conformance 327, differential agrees,
leak 0, miri/clippy/fmt clean.

---

## What this dismantles

| # | Hardcoded today | Location | Becomes |
|---|-----------------|----------|---------|
| 1 | `NativeModule` enum + `from_name`/`name` | `lang-stdlib/src/lib.rs ~241` | a **registry** |
| 2 | Per-backend dispatch `call_vec`/`call_quat` (+ all `call_json`/`call_math`/…) | both backends | **one shared dispatch** behind a value-marshalling seam |
| 3 | Checker tables `STD_MODULES`/`module_params`/`module_return` | `lang-check/src/stdlib.rs` | registry queries |
| 4 | Prelude *types* (`register_prelude`) | `lang-check/src/lib.rs ~537` | registry (**deferred** — extension types) |

**Decision (with user): all 9 existing modules migrate through the registry** — `json`, `math`,
`random`, `fs`, `time`, `env`, `args`, `vec`, `quat`. Migrating the permanent core citizens proves the
registry is *the* mechanism, not a toy. `vec`/`quat` are **evictable tenants**: they remain in-process
for now (the package manager that would let them leave is deferred) and earn their place by exercising
the *object + packed-buffer* seam — the hard part the scalar modules don't touch. The design test is:
*could vec/quat be deleted from core and re-added as a third-party crate with no API change?* If yes,
the registry is right regardless of when they actually leave.

---

## The dispatch seam (the hard part)

The existing `Arg`/`Output` is a **neutral-marshalling** seam (each backend projects its `Value` onto a
shared enum, the stdlib computes, the backend lifts the result). We **widen the same idea** — no trait
object, no generics — to cover objects, packed buffers, and (Phase B) recursive structures.

```rust
// lang-stdlib
pub enum Scalar { Int(i64), Float(f64), F32(f32), Bool(bool) }      // Copy, no heap

pub enum NativeValue {                                              // argument view
    Scalar(Scalar),
    Str(String),
    Bytes(Vec<u8>),
    Object { fields: SmallScalars },          // object's primitive fields, slot order, stack-inline
    Packed { layout: PackedLayout, bytes: Vec<u8> },   // flat List<@packed>: the bulk fast path
    List(Vec<NativeValue>),                   // boxed list (scalar fallback)
    // Phase B: Object becomes recursive (Vec<NativeValue>), + Null/JSON-ish for json.parse
    Opaque(&'static str),                     // type name only, for errors
}

pub enum NativeOut { Scalar(Scalar), Str(String), Bytes(Vec<u8>), Unit,
                     Object(SmallScalars), Packed { layout, bytes }, List(Vec<NativeOut>) }
```

**The seam is two per-backend functions written once each** — `marshal_in(&Value) -> NativeValue` and
`materialize(NativeOut, ResultShape) -> Value` — *not* a per-function `read_vec3`/`build_vec3`. They
replace all the duplicated dispatch.

**Host is threaded through dispatch** (decision with user — so `fs`/`time`/`random`/`env`/`args`
migrate too):

```rust
pub type DispatchFn = fn(&mut dyn Host, &[NativeValue]) -> Result<NativeOut, StdError>;
```

Pure functions (`json`, `math`, `vec`, `quat`) ignore the host. The dispatch fn is **shared across both
backends**, so differential holds by construction — the property the scalar seam already has.

> **Perf.** Scalar `vec.add`/`dot` run in tight loops, so marshalling must not regress them. `Object`
> carries a stack-inline `SmallScalars` (fixed `[Scalar; N]` + len, no heap) → Vec3/Quat round-trip
> with zero allocation. Bulk ops ride the one-copy `Packed{bytes}` path, never marshalling per element.
> Gate the migration on `vm_vec_add_all` + scalar-vec benches showing no regression.

`PackedLayout` is the existing `lang_ast::reflect` type. Confirm `lang-stdlib` may depend on `lang-ast`
at S1; if not, re-home/re-export the layout vocab.

---

## The registration API

```rust
// lang-stdlib
pub trait Extension {
    fn name(&self) -> &'static str;
    fn modules(&self) -> &'static [ExtModule];
    fn types(&self) -> &'static [ExtType] { &[] }   // design slot; impl DEFERRED (extension types)
}

pub struct ExtModule { pub name: &'static str, pub functions: &'static [ExtFn] }
pub struct ExtFn { pub name: &'static str, pub params: &'static [SigType], pub ret: RetTy, pub dispatch: DispatchFn }

/// lang-stdlib's small signature vocabulary (it cannot see `lang_types::Type` — that is *why* the
/// tables live in lang-check). lang-check maps each `SigType` -> `Type`.
pub enum SigType { Int, Float, F32, Bool, String, Bytes, Unit, Dyn,
                   List(&'static SigType), Option(&'static SigType),
                   Map(&'static SigType, &'static SigType), Named(&'static str) }

pub enum RetTy {
    Concrete(SigType),     // `dot` -> F32
    SameAsArg(usize),      // `vec.add` -> typeof arg0 (result shape from an argument)
    TypeArg,               // turbofish: `json.parse::<T>` -> the call-site type (Phase B)
}
```

`SigType` must cover every existing module's signatures (e.g. `fs.open` → `Named("FileHandle")`,
`fs.read_lines` → `List(String)`, `env.get` → `String`, `json.parse` → `Dyn`). Core registers one
in-process `StdExtension`; the global registry is a `&'static` list of extensions. A package manager
wires third-party crates in later.

---

## Phase A — registry + seam + migration (no new language surface)

Internal unification only; every slice is differential-green by construction.

- **A1** — registry skeleton in `lang-stdlib` (`Extension`/`ExtModule`/`ExtFn`/`SigType`/`RetTy`/
  `NativeValue`/`NativeOut`/`SmallScalars`/`DispatchFn`) + `StdExtension` declaring all 9 modules with
  shared dispatch fns. `vec`/`quat` math moves out of the per-backend `call_vec`/`call_quat` into shared
  dispatch; `json`/`math`/… dispatch fns wrap the existing `lang_stdlib` inner fns. Pure addition.
- **A2** — backend seam: `marshal_in`/`materialize` per backend; `call_native_module` routes **all 9**
  through the registry dispatch (Host passed in); **delete** `NativeModule` enum + every per-backend
  `call_*`/`read_vec3`/`build_vec3`. **Bench** scalar `vec.add` + `vm_vec_add_all` for no regression.
- **A3** — checker registry-driven: replace `STD_MODULES`/`module_params`/`module_return` with registry
  queries (`SigType`→`Type`; `SameAsArg(i)`→`args[i]`).

## Phase B — turbofish + call-site-typed construction (lands `json.parse::<T>`)

- **B1** — **recursive type-recipe channel**: checker records `span -> TypeRecipe` (struct =
  name+`[(field, recipe)]`, list/option/map/primitive/dyn — `type_of` full-fidelity generalized into a
  build recipe). Recursive `NativeValue`/`NativeOut` (objects hold `Vec`, not just scalars) + recursive
  **materialization** in both backends (eval constructs by name; VM interns each struct shape in the
  recipe, like from_bytes).
- **B2** — grammar: postfix `. id ::< T > ( args )` → `Expr::TypedModuleCall` (mirrors the `.as<T>()`
  postfix). Checker: receiver is a module + fn `RetTy == TypeArg` → result `T`; record recipe at span.
  Lowering → `Rvalue::ExtCall { module, func, args, recipe }` → `Op::ExtCall`.
- **B3** — first consumer `json.parse::<T>(s)`: native parse → walk JSON value × recipe → `NativeOut`
  tree → typed `T`. **Flat structs first** (primitive/string/bool fields), then nesting/lists/option —
  each green. Decide the failure story (runtime error vs `Result<T, string>`) here. Conformance fixtures
  + differential.

## Deferred (separate milestones)

- **Extension types** — `ExtType` impl (native `Vec3`/`Quat`, replacing item 4); then a real native type
  like `Image`.
- **Package / dependency manager** — third-party crates, cross-package providers (ties to the deferred
  object-model package system). This is what finally lets `vec`/`quat` physically leave core.

## Gates (every slice)

`cargo run -p lang-conformance --quiet --` + `--differential` + `--check-leaks`, `cargo clippy
--workspace --all-targets`, `cargo fmt --all --check`, `cargo test --workspace`, `cargo +nightly miri
test -p lang-value` when lang-value is touched. Commit per green slice; **never push without
authorization**. Standard trailer.
