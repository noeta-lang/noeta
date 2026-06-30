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

## Phase A — registry + seam + migration (no new language surface) — ✅ DONE

Internal unification only; every slice differential-green by construction. (Re-sliced during
implementation — `json` deferred to Phase B since its parse→value / stringify←value bridge needs the
recursive value seam, not the flat one; `vec`/`quat` split into scalar-via-seam + bulk-per-backend per
the option-B decision below.)

- **A1** ✅ (`49b20e9`) — registry skeleton in `lang-stdlib` (`Extension`/`ExtModule`/`ExtFn`/`SigType`/
  `RetTy`/`NativeValue`/`NativeOut`/`Scalar`/`DispatchFn` + `find_module`/`find_function`/`dispatch`) +
  `StdExtension` declaring the scalar/host modules. Pure addition; unit-tested via `SandboxHost`.
- **A2** ✅ (`f8e5274`) — backend seam `marshal_native_arg`/`materialize_native` per backend;
  `call_native_module` routes `math`/`random`/`time`/`env`/`args` through registry dispatch (Host in);
  deleted their per-backend `call_*`.
- **A3** ✅ (`56670f3`) — `fs` migrated; seam widened with `NativeValue::Bytes`/`NativeOut::Bytes` +
  `NativeOut::FileHandle` (`fs.open`); deleted `call_fs`.
- **A4** ✅ (`8c482d9`) — `vec`/`quat` **scalar** ops via the **object seam** (`NativeValue::Object`/
  `NativeOut::Object`, result shape from `RetTy::SameAsArg`); routing now **per-function** so the bulk
  `*_all` kernels stay in a slimmed per-backend `call_vec` (option B — avoids re-homing packed-layout
  vocab into lang-stdlib; see plans/backlog.md). `quat` fully migrated; `call_quat` deleted.
- **A5** ✅ (`e9859a9`) — checker registry-driven: `module_params`/`module_return` query the registry
  (`SigType`→`Type`, `RetTy`→`Type`); `is_std_module` for binding. Only `json` + `vec` bulk remain on a
  small hardcoded fallback. The registry is now the single source of truth for a migrated module's
  surface (checker + both backends read it).

**Remaining un-migrated:** `json` (→ Phase B, the recursive seam) and the `vec` bulk `*_all` kernels
(deferred with vec/quat's eventual eviction to a package). The `NativeModule` enum survives only to
route those; it dies in Phase B.

## Phase B — turbofish + call-site-typed construction (lands `json.parse::<T>`) — ✅ DONE (B1+B2)

The headline feature — `json.parse::<T>(text)` — works end to end on both backends. (Re-sliced during
implementation: the plan's B1/B2/B3 collapsed into two green commits, B1 = the stdlib half, B2 = the
whole language wiring incl. nesting — the recursive materialization was uniform enough to land in one
go rather than flat-then-nested.)

- **B1** ✅ (`30c7250`) — **the recipe + shared decode, in lang-stdlib**: a neutral `TypeRecipe`
  (scalar / option / list / string-keyed map / declared-order struct) and a recursive
  `json::parse_typed(text, &TypeRecipe)` that walks a parsed JSON tree against the recipe into a
  `NativeOut` tree. `TypeRecipe` lives in **lang-stdlib** (a leaf) — *not* `lang_ast::reflect` where
  `TypeRepr`/`PackedLayout` live — because the walk lives here and lang-stdlib can't see the type
  system; the bytecode op carries a `lang_stdlib::TypeRecipe`, no cycle. `NativeOut` gained recursive
  recipe-result variants (`Struct`/`Map`/`None`/`Some`). Numeric widening matches the lattice
  (`int <: f32 <: float`); a missing optional field → `None`, a missing required field → error. Unit-
  tested via the decoder tests.
- **B2** ✅ (`db7a72b`) — **the language wiring + both executors**. Grammar: `module.func::<T>(args)`
  is an **atom** `Expr::TypedModuleCall` (`ident . ident ::< T > ( args )`) — the receiver is always a
  bare module name, so it's an atom rather than a postfix, keeping it off the arity-bounded pratt op
  table. Checker types it (result `T`), validates the call, resolves `T` into a `TypeRecipe` recorded
  in the new **`ext_call_sites`** channel (parallel to `type_of_sites`); only value structs / scalars /
  lists / options / string-keyed maps decode, else compile-time E0007. One shared lowering bakes the
  recipe into `Rvalue::ExtCall` (read from `ext_call_sites`, exactly like `FromBytes` reads
  packed-list sites); the VM compiler transcribes it to `Op::ExtCall`. Both backends marshal the args,
  run the shared `parse_typed`, and **materialize recursively** — the tree-walker through its real
  registered type (methods/defaults like a literal), the VM through a fresh same-name shape (method
  dispatch is name-keyed), so they agree by construction. Failure = runtime E0007 at the call site
  (decided here; a `Result<T,string>` variant could be added later). Fixtures cover flat / nested /
  lists / options / methods; differential-covered.

**Not done — the registry generalization (call it B4, deferred):** `json` is deliberately still on the
legacy `NativeModule` enum path (dynamic `json.parse(s)`/`stringify` unchanged); `Op::ExtCall` routes
`json.parse` by a name match, not through `registry::dispatch_typed`. Finishing the dogfood —
registering `json` as an `ExtModule` with a `RetTy::TypeArg` `parse` (+ a typed-dispatch fn-pointer),
migrating `stringify` (needs the recursive **arg-side** `NativeValue` marshalling, which the typed
parse did not), and removing the `NativeModule` enum — is the remaining cleanup. The arg-side recursion
is the one genuinely new piece left.

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
