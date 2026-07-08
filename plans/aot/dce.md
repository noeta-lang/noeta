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

**✅ DONE (A1 `6b11b79`, A2 `1447a76`) — but with a CORRECTED value story (measured 2026-07-08).**
The `aot` feature no longer pulls Cranelift: an isolated `cargo rustc -p noeta-aot-runtime` archive
has **0 Cranelift symbols** (was ~20 MB of compiler); noeta-vm builds clean under both `aot`-only and
`jit`; 147 jit tests + native AOT differential green. Implemented with an internal `jit-rt` feature
(enabled by both `jit` and `aot`) gating the runtime-support surface; `jit` adds the compiler on top.

**HONEST CORRECTION — A2 does NOT shrink the shipped binary.** The premise ("~20 MB of Cranelift sits
in every AOT binary") was extrapolated from measuring the *CLI* binary (which needs the compiler),
never an actual AOT binary — and it was wrong. Measured: linking a native binary against the
cranelift-*ful* archive (538 cranelift syms) vs the cranelift-*free* archive both produce a
**byte-identical 12,244,495-byte binary with 0 cranelift symbols**. Standard static-archive member
selection already drops the unreferenced Cranelift `.o` members — nothing in the AOT binary's
reachable graph (`main → run_module_aot → bind_aot_dispatch + helpers`) touches `Jit::compile`.
So A2's real, non-zero value is **build-time + dependency hygiene**: the isolated aot-runtime archive
no longer *compiles* ~20 MB of Cranelift, so the first `noeta build --native` (which builds that
archive) is faster and the pure-AOT dependency closure is clean. Worth keeping (right architecture,
real dev-time win), but not a binary-size lever.

**Where the binary weight actually is (measured on the 12.24 MB `jit_promo.noe` native binary — a
program that never imports `std.http`):** the http/network stack is linked anyway —
rustls 453 K + ring 407 K + hyper 250 K + h2 219 K + tokio 200 K + reqwest 171 K ≈ **1.7 MB+**
(undercount; 1411 rustls/reqwest/hyper symbols present) — because `STD_MODULES` roots every ring's
`*_dispatch` fn. **This is Axis B, and it is confirmed the real binary-size lever.**

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

### Axis B — per-ring stdlib elimination (HIGH value multi-MB) — the "strip stdlib" row

**✅ DELIVERED for the http ring (the ~5 MB one), automated & sound.**
- `ring-http-client` (default-on) gates reqwest + its TLS tree in `noeta-runtime`'s `RealHost`
  (`89a2c14`, renamed from `ring-http` in the client/server split). `net_fetch` is a hard-error stub
  without it — never reached, since a program that can't name a client fn never calls it.
- `noeta-aot-runtime` forwards the ring (default-features=false via a direct path dep); the `aot`
  runtime support is a hard dep feature so it survives `--no-default-features`.
- **Footprint scan** `aot_ring_features` (`b1065c9`) — `noeta build --native` derives the used rings
  and builds the archive with exactly them (`cargo rustc --no-default-features --features <rings>`),
  linker drops the rest. Reads all three lowerings (`NativeModule`/`ModuleFn` consts + `ExtCall`).
- **Client/server split** (this slice): a whole-module `use std.{http}` value is conservative
  (could call any fn → client ring), but a *precisely-named* reference (`use std.http.get`, or a
  turbofish `ExtCall`) selects the client ring only for the outbound-client fns (`get`/`post`/…/
  `_async`); `response`/`serve` select nothing. So `use std.http.{serve, response}` sheds reqwest.

Measured (release native binaries, auto-selected):

| program | binary | reqwest/rustls | note |
|---|---|---|---|
| non-http `jit_promo` | **7.24 MB** | 0 | ring dropped, −5.0 MB / −41% |
| selective server `use std.http.response` | **5.67 MB** | 0 | split payoff — server sheds reqwest, runs (`200`) |
| whole-import client `http_async` | 10.68 MB | 1074 | client keeps reqwest, real fetch path |

**Known limitation → package-manager milestone (below):** a *whole-module* `use std.{http}` in a
server program stays conservative (keeps reqwest), because the module value's member calls lower to
`CallMethod` on a receiver that isn't statically pinned, and the sync client fns (`get`/`post`) are
indistinguishable from `map.get`/user methods. The clean fix is splitting `std.http` into separate
client/server *modules* so module-level detection is precise — see the carry-over note.

Remaining rings (crypto ~60 K, id ~6 K) are deliberately **not** hand-gated — negligible size, and
they generalize for free under the extension-split. Original per-ring plan (still the shape):

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
surface is **much narrower than an earlier draft of this section claimed** (corrected 2026-07-08 by
reading the ops):
- **Statically-named edges** — `MakeClosure { proto }`, `CallMethod` (static method `NameId` at the
  site), **method-handle** materialization (`Type.method` names its `(type, method)` statically).
  All followable in a reachability pass; a method reachable only via a handle is *provably* reachable.
- **The reflection *metadata* queries are closed-world, not runtime strings.** `attributes_of::<T>()`
  → `Op::AttributesOf { type_name }` — a **turbofish**, the attribute type resolved at compile time
  ("closed-world" per the op doc), NOT a runtime target. `roles_of()` → `Op::RolesOf` — also
  closed-world, but reads the **whole** role index. `type_of(value)` → `Op::TypeOf` — builds a
  `TypeRepr` from the value's runtime *shape*, and doesn't read the attribute manifest at all. So
  `attributes_of` is **not** an escape hatch (an earlier version of this doc wrongly listed it as one).
- **The only genuine runtime-string dispatch is `Op::Invoke`** (`name: Reg`, `name_val.as_string()`) —
  and it resolves against the **method table**, not `ReflectionInfo`, so it keeps *methods* reachable,
  not attribute/role *metadata*.

Net: the reflection-metadata reachability is essentially **static and closed-world**. The manifest is
queried by attribute type (scan `Op::AttributesOf` names → keep only those `AttributeRecord`s);
`Op::TypeOf` needs only shapes. The one remaining all-or-nothing query is `Op::RolesOf` (present ⇒
keep all `RoleRecord`s). **Reflection refinement (tie to this axis): make `roles_of::<RoleEnum>()` a
turbofish** mirroring `attributes_of::<T>()`, so the role index is queried by role-enum and DCE keeps
only the queried enums' records — removing the last all-or-nothing case. Language-surface change, so
it rides with the Axis C / reflection decision, not the module split.

**Tier 0 (safe floor):** strip only unreachable free-function protos via the static edge set; keep
all methods + all reflection. Zero risk.
**Tier 1 (RECOMMENDED, sound):** additionally drop unreachable methods, plus `AttributeRecord`s whose
attribute type no `Op::AttributesOf` names, plus all `RoleRecord`s iff no `Op::RolesOf` is present
(or, with the `roles_of::<RoleEnum>()` refinement, the unqueried enums'). The soundness gate is just
`Op::Invoke` — a program that dispatches by runtime string keeps its methods conservatively. No
`@reflectable`, no language change; the reflection *metadata* strip needs no gate at all (closed-world).
**Tier 2 (`@reflectable`, DEFER — likely unnecessary):** would only add value for the residual
`Op::Invoke` case (keeping method *code* a dynamic name might reach). Given how narrow that is, this
may never be worth the new attribute + checker + migration + semantics change. Revisit only if a real
`invoke`-heavy program shows measurable metadata bloat.

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
6. **C1** reachability pass (Tier 1): strip unreachable protos + `AttributeRecord`s no `Op::AttributesOf` names + `RoleRecord`s if no `Op::RolesOf`; the only method-reachability gate is `Op::Invoke`. Renumber + fixup; differentials green; report byte delta. (Optional `roles_of::<RoleEnum>()` refinement is a separate language change.)
7. Docs + memory; Tier 2 left explicitly open.

Nothing pushed without authorization.

## Package-manager milestone — DCE carry-over (do not miss)

The footprint scan (`aot_ring_features` in `noeta-cli`) is the *interim* selector — a bytecode-derived
guess. The package-manager milestone builds the general "select which extensions/rings link in"
machinery (generate the `REGISTRY` slice from a manifest's native deps, compile the archive with
them). When that lands, fold in the following — each is either a precision gap the manifest closes or
a generalization the extension-split makes uniform:

1. **Split `std.http` into client vs server *modules*.** Today the whole-module `use std.{http}` case
   is conservative (keeps reqwest) because member calls lower to `CallMethod` on an unpinned receiver
   and sync client fns (`get`/`post`) collide with `map.get`/user methods — so a server program that
   imports the whole module can't be proven client-free. Splitting the surface (e.g. `std.http` =
   outbound client; `std.http.server` / `std.serve` = `serve` + `response` + `Request`) makes
   *module-level* detection precise, and lets a `ring-http-server` (inbound tokio — no reqwest)
   separate cleanly from `ring-http-client` (reqwest). Until then, server programs should use
   *selective* imports (`use std.http.{serve, response}`) to shed reqwest — the scan already handles
   that soundly.
2. **Add `ring-http-server`.** The inbound `RealHost` code (`net_listen`/`net_accept`/`net_reply`,
   `servers`/`conns`, `ServerState`, `RealAcceptIo`) is currently always-compiled (cheap — tokio is
   already linked for fs). Gate it for capability completeness once (1) makes the server signal
   precise; near-zero bytes, but it finishes the client/server capability separation.
3. **Gate the remaining rings** (`crypto` → sha*/bcrypt/hmac/subtle; `id` → uuid; and any future
   heavy ring). Deliberately not hand-gated now (~65 KB total). Under the extension-split each ring is
   an `Extension` unit you include/exclude wholesale, so gating becomes *uniform* (drop the extension
   crate + its deps) instead of per-site `#[cfg]` — do it there, not by hand here.
4. **Manifest drives selection; scan becomes a cross-check.** The package manager knows a program's
   native deps from its manifest — authoritative and precise (no CallMethod ambiguity). Make the
   manifest the source of truth for the archive feature set; keep `aot_ring_features` as a
   belt-and-suspenders fallback for source-only `noeta build --native` (no manifest), or retire it.
   The `module_ring`/`fn_ring` tables in `noeta-cli` are today's module→ring mapping — reconcile them
   with whatever the extension registry exposes (ideally each `Extension`/`ExtModule` *declares* its
   ring/feature, so the mapping isn't a hand-maintained table in the CLI).
5. **Archive cache keyed by ring-set.** `resolve_aot_runtime` rebuilds `libnoeta_aot.a` per feature
   set to the same path; a cache keyed by the sorted ring list avoids recompiling for repeat builds
   with the same footprint. Pure perf; deferred.
6. **`STD_MODULES` reshaping.** Per the higher-order-abi arc, `STD_MODULES` is std's private manifest
   and is slated to be reshaped/removed as third-party extensions land. The ring feature-gating must
   move with the entries — the gating property ("this ring's code + deps are behind a cfg / in a
   separately-selectable unit") is what carries over, not the current table shape.
