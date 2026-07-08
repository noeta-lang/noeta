# L3.4 — DCE / tree-shaking (scoped plan)

Branch `aot-dce` off main (synced to `bca0595`). Last, optional slice of the P-AOT arc.

## Correcting the framing (2026-07-07)

An earlier pass here concluded "stdlib is native, not bundle bytecode → the linker handles its size
for free." **That was wrong**, and it dropped the axis the README's "strip … stdlib" row actually
meant: the different **rings** of stdlib (`std.id`, `std.crypto`, `std.http`, …). There is no reason
`std.http`'s reqwest/rustls/tokio stack should sit in a binary that never imports it.

The reason the linker does **not** strip unused rings for free: `noeta-stdlib`'s `STD_MODULES` is a
`const &[ExtModule]` whose every entry holds a `dispatch: fn` pointer (`http_dispatch`,
`crypto_dispatch`, `id_dispatch`, …). That static table is a **GC root** — `--gc-sections` keeps
every dispatch fn and, transitively, its whole dependency tree. So an AOT binary carries every ring
unconditionally. Measured (symbol-name attribution on release `noeta`, an undercount since
monomorphized generics don't carry the crate name):

| ring / dep | attributable code |
|---|---|
| `std.http` → rustls+tokio+hyper+h2+reqwest+ring | **~3.2 MB+** (rustls 516 K, h2 882 K, tokio 421 K, hyper 261 K, reqwest 190 K, ring 935 K) |
| `std.crypto` → sha2/bcrypt/hmac | ~55 K |
| `std.id` → uuid | ~6 K |
| the **JIT compiler** (Cranelift, never used at AOT runtime) | **~20 MB** (4445 symbols, 19.98 MB text) |

So the ~28 MB AOT binary is almost entirely **capabilities the program may not use**: ~20 MB of a
compiler an AOT binary never invokes, plus every stdlib ring's native dep tree.

## The unifying principle

**Assemble the AOT runtime from exactly the capabilities the program statically needs.** An AOT
binary needs the JIT *runtime helpers* + interpreter + AOT dispatch, but **not** the compiler; it
needs the stdlib rings the program **imports**, not all of them. The enabling mechanism is the same
at both granularities: make each capability an optional Cargo feature, have `noeta build --native`
select features from the program's static footprint, and let `--gc-sections` drop the unreferenced
code + dep trees once the roots are gone. The program's footprint is statically recoverable — every
native call is an `ExtCall { module: NameId, … }`, so the used-ring set is a scan of the bytecode;
the JIT compiler is simply the capability that is *never* needed at AOT runtime.

## Axes

### Axis A — dead **compiler** elimination (HIGH value ~20 MB, LOW risk)

An AOT binary binds a static dispatch table and runs with `self.jit == None` (L3.2b(2)); Cranelift is
dead weight. noeta-vm's references into `noeta_jit::` split cleanly:

| Runtime-support (no cranelift) | Compiler (needs cranelift) |
|---|---|
| `*_HELPER` consts, `AOT_DISPATCH_SYMBOL`, `FrameLayout`, `CompiledFn`, `CallSiteCache`, `OUTCOME_*`, `SITE_*` | `Jit`, `Jit::new`, `Jit::new_object`, `CompileBreakdown`, `worth_compiling`, `worth_osr` |

1. Extract **`noeta-jit-abi`** (left column; zero cranelift deps). `noeta-jit` depends on + `pub use`s
   it, so every `noeta_jit::Foo` path in noeta-vm keeps resolving unchanged.
2. Re-gate noeta-vm: cfg alias `jit-or-aot = any(feature="jit", feature="aot")`. The `noeta_jit_*`
   helper fns, `run_module_aot`, `bind_aot_dispatch`, AOT dispatch routing → `jit-or-aot`. The
   compiler pieces (`compile_module_aot`, `init_jit*`, the `jit:` field, `worth_*`) stay `jit`-only.
   `aot` becomes `["dep:noeta-jit-abi"]` (no longer `["jit"]`); `jit` stays full `noeta-jit`.
3. `noeta-aot-runtime` enables `noeta-vm/aot` **without** `noeta-vm/jit` → no cranelift.
4. `noeta build --native` (the CLI) keeps `jit` — it *is* the compiler (`compile_module_aot` runs
   there); only the linked *runtime archive* sheds cranelift.

### Axis B — per-ring stdlib elimination (HIGH value multi-MB, MODERATE work) — the "strip stdlib" row

1. **Feature-gate each ring** in `noeta-stdlib` (and the matching RealHost capability in
   `noeta-runtime`): each ring's deps become optional; the `STD_MODULES` entry + its `*_dispatch` fn
   compile under `#[cfg(feature = "ring-<name>")]`. A small always-on core (Ring-1 primitives, the
   marshalling, `find_module`) stays unconditional. `std.http`/network gates off tokio+reqwest+rustls
   in `noeta-runtime`; `std.crypto` gates off sha*/bcrypt/hmac; `std.id` gates off uuid; etc.
2. **Build-time footprint scan.** `noeta build --native` collects the used-ring set from the module —
   the distinct `ExtCall.module` names plus the handful of construct-backed capabilities
   (`http.serve`, reactive, task). That set → the `--features` list for the archive build.
3. **Tailored archive + linker gc.** Build `libnoeta_aot.a` with exactly those ring features; the
   unreferenced dispatch fns are gone from `STD_MODULES`, so `--gc-sections` drops them + their dep
   trees. Cache the archive keyed by the feature-set (repeat builds with the same imports reuse it) so
   per-program archive compilation isn't paid every time.
4. **Fallback.** If footprint detection is ever uncertain for a capability, default that ring **on**
   (conservative = larger binary, never broken).

Axis A is just Axis B's mechanism applied to the one capability (`jit`) that is *never* needed at AOT
runtime — same feature-select-then-gc move, so build A first and B reuses its `noeta build --native`
feature-selection plumbing.

### Axis C — bundle bytecode reachability + reflection DCE (LOW absolute value <2 KB)

Bundles are deflate-compressed to <2 KB (orders 1.6 K, hello 87 B), so this is polish, not headline —
but it's where the README's "aggressiveness / `@reflectable`" decision lives. The dynamic-dispatch
surface is narrower than it looks:
- **Statically-named edges** — `MakeClosure { proto }`, `CallMethod` (static method `NameId` at the
  site), **method-handle** materialization (`Type.method` names its `(type, method)` statically).
  All followable in a reachability pass; a method reachable only via a handle is *provably* reachable.
- **Runtime-string dispatch** — `Op::Invoke` (`name: Reg`, `name_val.as_string()`),
  `attributes_of(target)`, `roles_of()` key by string. Reflection's designed purpose; handles can't
  replace it. Only three constructs make reachability statically-unknowable: `invoke` with a
  non-literal name, `attributes_of` with a non-literal target, and `roles_of()` (reads the whole role
  index). These are **detectable** in the bytecode.

**Tier 0 (safe floor):** strip only unreachable free-function protos via the static edge set; keep
all methods + all reflection. Zero risk.
**Tier 1 (RECOMMENDED, sound):** additionally drop methods / type reflection records unreachable
through the static edges **when the bytecode contains none of the three escape hatches** — the scan
is the soundness gate; reflective programs keep their metadata automatically. No `@reflectable`, no
language change.
**Tier 2 (`@reflectable`, DEFER):** for programs that *do* dispatch reflectively but still want to
strip. Needs a new language attribute + checker + migration and **changes semantics** for un-annotated
types. A language-surface decision on its own — not folded in silently.

## Recommendation

Do **Axis A (compiler decoupling, ~20 MB)** then **Axis B (per-ring stdlib, multi-MB — this is the
"strip stdlib" the row meant)**, reusing A's feature-selection plumbing. Add **Axis C Tier 1** (sound,
cheap, no language change) as polish. **Defer Tier 2 `@reflectable`** to its own decision.

Ties to standing norms: "build it right, not easy" (assemble the runtime from needed capabilities at
the feature/crate seam, rather than ship 20 MB of dead compiler + every ring's dep tree); "confirm
before cutting/narrowing scope" (the earlier `stdlib`-is-free misread is corrected here, not silently
carried); bench rule (A and B are build-time/size only, no hot path; C Tier 1 build-time only).

## Sequencing (each step commits green; gate = `cargo test --workspace` incl. the two-`main` staticlib)

1. **A1** extract `noeta-jit-abi`; workspace builds, all `noeta_jit::` paths resolve via re-export.
2. **A2** re-gate noeta-vm `aot` vs `jit`; `aot`-without-`jit` compiles.
3. **A3** point `noeta-aot-runtime` at `aot`-only; rebuild archive; **measure size**; AOT differential green.
4. **B1** feature-gate stdlib rings + RealHost capabilities; default features = all (workspace unchanged).
5. **B2** `noeta build --native` footprint scan → per-program `--features`; archive cache; **measure size** on http-using vs http-free programs; AOT + base differential green over the full corpus.
6. **C1** reachability pass (Tier 1 + escape-hatch gate); renumber + fixup; differentials green; report byte delta.
7. Docs + memory; Tier 2 left explicitly open.

Nothing pushed without authorization.
