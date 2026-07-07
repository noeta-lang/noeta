# The extern-type seam — registrable native value types (and `Uuid`, its first client)

**Status: ARC COMPLETE (2026-07-07).** X1 `727b603`, X2 `f22349e`, X3 `1568b92`, X4 `af0ab27`+`7a17ae2`, X5 `cff9cb3`, X6 `9cd4c42`, X7 docs+memory. Gates: X1/X4 benches no-regression (X4 get/100k ~8-11% FASTER), X3 zero-edit fs corpus, X5 zero-edit async corpus + real-executor CLI tests, X6 zero backend edits. Follow-ons in `plans/deferred.md` §Extern types.

Original design (as approved): Branch `uuid-host-seam` (continues the id-entropy arc).
The `Extension` trait's "and, later, types" promissory note comes due: the registry grows a
`types()` seam so a Rust crate can contribute a first-class value type the way it already
contributes modules. `Uuid` is the first client (pure, ordered, key-capable); **FileHandle
migrates onto the seam in this arc too** (mutable, effectful — the other corner of the matrix),
proving the contract covers both before a third type arrives. The arc also builds the **async
seam** (`ExternIo` work descriptors): `fs.*_async` migrates off its hand-wired intercept onto
it, and new async fs functions prove the seam is open.

## Why now

`id.uuid()`/`id.uuid_v7()` (id-entropy arc, this branch) return `string`. A first-class `Uuid`
wants: type-checked signatures (`fn find(id: Uuid)`), `is Uuid` narrowing, value equality and
ordering (v7's time-sortability), and eventually map keys. We'll have plenty of such types
(decided with the user 2026-07-06) — so build the general seam, not a third hand-threaded type.

## Design constraints (non-negotiable)

1. **No hot-path regression.** The seam touches `Payload`, the `values_equal` predicate chain, and
   (slice X3) the map-key representation P-SSO just optimized. Every touched hot structure gets a
   before/after bench run (`cargo bench` suite + the PHP-comparison loops); the map-key slice gets
   a dedicated A/B.
2. **Differential lockstep.** Every seam behavior (equality, ordering, display, narrowing,
   map/set membership) lands in both backends in the same slice; conformance pins exact values.
3. **The Host seam stays the only effect boundary — and the executor stays backend-owned.**
   Extern-type methods that perform effects reach them ONLY through the `&mut dyn Host` the
   dispatch hands them (exactly like module functions); *construction* of effectful values
   likewise stays in module functions (`fs.open`, `id.uuid`). Async joins the seam by the same
   inversion (decided with the user 2026-07-06): a dispatch returns a **work descriptor**
   (`NativeOut::Spawn(Box<dyn ExternIo>)`), and the backend tickets it on its executor —
   extensions provide values and work, core owns time, scheduling, and determinism. See "The
   async seam" below. Extensions never see the executor.

## The seam

### Registry side (`noeta-stdlib/src/registry.rs`)

```rust
/// A value type contributed by an extension: a reserved type name, its method surface, and the
/// capability flag the checker + backends read. Everything the two backends need to host the
/// value uniformly lives behind `ExternValue` (below).
pub struct ExtType {
    pub name: &'static str,             // reserved: user declarations of this name are E0049
    pub methods: &'static [ExtFn],      // instance-method signatures (checker + arity/typing)
    pub dispatch: TypeDispatch,
    pub key_capable: bool,              // may key a Map / member a Set — see the contract below
}

/// One dispatch signature for the whole {pure, mutable} × {host-free, effectful} matrix:
/// the receiver comes in `&mut` (a pure method just doesn't mutate) and the Host is always
/// passed (a pure method just doesn't touch it). This is exactly the shape FileHandle's
/// shared method logic (`handle.rs`: `read_line(host)`, `write(&chunk)`, …) already has.
pub type TypeDispatch = fn(
    recv: &mut dyn ExternValue,
    method: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError>;

pub trait Extension: Sync {
    fn name(&self) -> &'static str;
    fn modules(&self) -> &'static [ExtModule];
    fn types(&self) -> &'static [ExtType] { &[] }   // default empty: existing extensions unchanged
}
```

**The `key_capable` contract**: a type declaring `key_capable: true` promises (a) no mutating
methods, (b) `cmp_value` is a total order over its kind, (c) `hash_value` is stable and
content-derived. `Uuid`: true. `FileHandle`: false — it mutates (cursor/buffer), so it must
never sit where a hash or sort order could go stale. The checker enforces the flag at the
`Map<K,_>`/`Set<T>` type-formation sites (slice X4); the promise itself is a documented impl
contract (debug-asserted where cheap), like every other registry invariant.

`find_type(name)` mirrors `find_module`. `SigType::Named(name)` already flows to
`Type::Named(name, [])` in the checker — an `ExtFn` returns/accepts an extern type by name today
with zero new plumbing.

### The value contract (`noeta-stdlib`, new `extern_value.rs`)

```rust
/// The uniform behavior contract an extern value implements once, hosted by BOTH backends.
/// Values are acyclic by design (a GC leaf: no child `Value`s). Mutation is allowed — the
/// backends host extern values in shared cells (see below), so a mutating method has
/// reference semantics, exactly like today's FileHandle ("first mutable heap value type").
pub trait ExternValue: std::fmt::Debug + Send {   // Send: extern results may cross the real
                                                  // executor's runtime (async seam below)
    fn type_name(&self) -> &'static str;            // must match the registered ExtType.name
    fn eq_value(&self, other: &dyn ExternValue) -> bool;      // downcast via as_any; false on kind mismatch
    fn cmp_value(&self, other: &dyn ExternValue) -> Option<std::cmp::Ordering>;  // None = unordered kind
    fn hash_value(&self) -> u64;                    // stable, content-only (meaningful iff key_capable)
    fn display(&self, out: &mut dyn std::fmt::Write);         // echo/interpolation form
    fn clone_box(&self) -> Box<dyn ExternValue>;    // GC promote (mirrors today's payload-clone semantics)
    fn as_any(&self) -> &dyn std::any::Any;         // dispatch downcasts to the concrete type
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;       // …and mutably, for mutating methods
}
```

Equality semantics are the impl's call, per kind: `Uuid` compares bytes; `FileHandle` keeps its
current full-shared-state comparison (`ops.rs:424` — path, mode, cursor, buffer, closed), which
its derived `PartialEq` already provides. Nothing in the seam forces content vs identity.

Resource lifecycle: freeing an extern value is a plain Rust drop (the GC cannot reach the Host
at free time). Self-contained RAII inside the value (an owned fd/socket closing on drop) works
by construction; **Host-coupled finalizers are out of scope** — types needing them (buffered
writers…) keep FileHandle's explicit-`close()` discipline.

Marshalling arms: `NativeOut::Extern(Box<dyn ExternValue>)` (results) and
`NativeValue::Extern(&dyn ExternValue)`-shaped arg projection (args into module fns and methods).

### Backend hosting — one variant each, forever

- **VM** (`noeta-value`): `Payload::Extern(Box<dyn ExternValue>)` + `HeapKind::Extern`. A GC leaf
  (free = drop, no children/trace arms beyond the empty pattern; promote = `clone_box`).
  `Box<dyn>` is 16 bytes — smaller than the existing 32-byte variants, so `Payload` does not grow.
  Mutating methods reach the payload through a `with_extern_mut` accessor — the generalization
  of today's `with_file_handle_mut` (`heap.rs:946`), which it replaces.
- **Tree-walker** (`noeta-eval`): `Value::Extern(Rc<RefCell<dyn ExternValue>>)` — the direct
  generalization of today's `Value::FileHandle(Rc<RefCell<FileHandle>>)` (`value.rs:92`); the
  unsize coercion from `Rc<RefCell<Concrete>>` keeps it one allocation. Shared-cell semantics in
  both backends ⇒ mutation is visible through every copy, identically to FileHandle today.
- **Equality**: ONE new rung at the END of each backend's `values_equal` chain
  (`is_extern && is_extern → eq_value`). Appended last ⇒ existing kinds' comparison paths are
  byte-identical to today.
- **Ordering** (set canonicalization, sorted map display): one arm in each backend's canonical
  value ordering → `cmp_value`.
- **Display/`repr`/`type_name`**: delegate to the trait; `type_name()` also drives
  `NarrowTarget::Named` matching (see below).

### Checker (`noeta-check`)

- `check_type_ref`: a `Named` name matching `registry::find_type` is admitted (alongside
  builtins/prelude/declared types) — `let u: Uuid` works.
- **E0049** (next free diag): a user `struct`/`class`/`enum` declaration whose name collides with
  a registered extern type. Reserved explicitly, not silently shadowed (this also retro-covers
  FileHandle/Iterator/etc. if we fold their names into the same reservation set — do it: one
  reserved-names source).
- `method_return`/`method_params`: one table-driven arm — `Type::Named(n, _)` with
  `find_type(n)` ⇒ look up the method in `ExtType.methods` (same `ExtFn`/`sig_to_type` machinery
  module functions use). No per-type hardcoded `uuid_method` fn — that's the point of the seam.
- Reflection: `Type::Named(extern) → TypeRepr::Named` (already the shape user types take).

### Narrowing (`is Uuid` / `.as<Uuid>()`)

`NarrowTarget::Named(name)` currently resolves via `v.shape().name` (user objects only). Add: if
the value `is_extern`, match `extern.type_name() == name`. Both backends, same slice, conformance
covers `dyn` laundering.

### Method dispatch (both backends)

`call_method` on an extern receiver: resolve `find_type(type_name)` → find the `ExtFn` → project
args to `NativeValue` → `ExtType.dispatch(recv, method, host, args)` → materialize `NativeOut`.
This replaces the per-type `call_file_handle_method` twins (`noeta-vm/src/methods.rs:657`,
`noeta-eval/src/lib.rs:2919`) and the `FileHandleMethod` name-enum. Costs, stated honestly:

- Method *lookup* goes from an enum match to a registry find (short linear scans over statics) —
  file-handle methods are IO-bound, Uuid methods are not loop arithmetic; no inline-cache
  integration in v1. Bench, and wire into P-IC later only if a real workload shows it.
- Arg *projection* means `handle.write(chunk)` passes an owned `NativeValue::Str` — one string
  clone per call. This is the SAME cost every registry module function already pays
  (`fs.append` etc.); handle methods merely join the same seam. If a write-loop bench ever
  objects, the fix is a borrowed arg projection for the whole registry, not a FileHandle
  special case.

## `Uuid` — first client (the `uuid` crate, not hand-rolled)

Workspace dep: `uuid = { version = "1", default-features = false }` — the `Uuid` type,
`Builder`, `parse_str`, hyphenated formatting, `get_version_num`/`get_timestamp`, and **no**
self-generating constructors (those live behind `v4`/`v7` features we deliberately do NOT
enable), so no second entropy source sneaks past the Host seam. `Builder::from_random_bytes`
and `Builder::from_unix_timestamp_millis` are `const fn` taking caller-provided bytes — the
Host's `entropy_u64()`/`clock_unix_ms()` feed them exactly as today. The hand-rolled byte
assembly in `id.rs` (v4/v7/format_uuid) is DELETED in favor of the crate's RFC-9562 builders;
the exact-value conformance pins in `std/id_uuid.noe` prove bit-identical output across the swap.

`impl ExternValue for uuid wrapper` (newtype in `noeta-stdlib/src/id.rs`): eq/cmp = byte order
(v7 sorts by time, by construction), hash = content, display = canonical lowercase hyphenated —
so every existing test that observes `echo` output stays green untouched.

### The surface: how versions differentiate

**One `Uuid` type; the version is data, not a type.** RFC 9562 encodes the version in the value's
own bits; every ecosystem (postgres, the `uuid` crate, python, java) uses one type, and splitting
`Uuid4`/`Uuid7` would fracture any generic code holding ids. Differentiation is therefore:

1. **Explicit constructor names, versioned at the source**: `id.uuid()` (v4 — the "just give me
   an id" default) and `id.uuid_v7()` (explicit opt-in to time-ordered). Future versions extend
   the family (`id.uuid_v5(ns, name)` when hashing arrives with std.crypto).
2. **Introspection on the value**: `.version() -> int` reads the bits back.
3. **Version-specific data is fallible accessors**: `.timestamp_ms() -> int?` — `some(ms)` on a
   v7 (and any time-carrying version), `none` otherwise. The Option IS the version distinction,
   surfaced where it matters instead of at the type level.

| Surface | Type | Notes |
|---|---|---|
| `id.uuid() -> Uuid` | module fn | v4, Host entropy (return type changes from `string`) |
| `id.uuid_v7() -> Uuid` | module fn | v7, Host clock + entropy (ditto) |
| `id.parse(s: string) -> Uuid?` | module fn | any RFC form; `none` on malformed |
| `u.to_string() -> string` | method | canonical lowercase hyphenated |
| `u.version() -> int` | method | 4, 7, … |
| `u.timestamp_ms() -> int?` | method | `some` iff the version carries time |
| `==`, ordering, `echo` | value rungs | byte order = v7 time order |

## Map keys (X4) — `Map<Uuid, T>` for real

Today: VM `MapStore = HashMap<CompactString, Value, Fx>` (bare-`&str` lookup via `Borrow<str>`),
eval `BTreeMap<String, Value>`; the checker asserts string keys in five places. Sets need
nothing (sorted `Vec<Value>` — the X1 ordering rung already unlocks `Set<Uuid>`).

Design — generalize the key, not the map:

- **VM**: `MapStore = hashbrown::HashMap<MapKey, Value, Fx>` with
  `enum MapKey { Str(CompactString), Extern(Box<dyn ExternValue>) }`.
  - `Hash` is **content-only** (Str hashes exactly the bytes it hashes today, no discriminant;
    Extern hashes `hash_value()`): the string path's hash cost is bit-identical.
  - Bare-`&str` lookup survives via hashbrown's `Equivalent<MapKey> for str` (std's `Borrow`
    can't express a heterogeneous enum lookup; hashbrown — std's own internal table — can).
    New direct dep: `hashbrown` (with the same Fx hasher).
  - Eq gains one discriminant check on the string path (same-kind, branch-predicted).
    Cost: +8 bytes per entry (tag). **A/B bench against the P-SSO map benches is the gate.**
- **Eval**: `BTreeMap<MapKey, Value>` with `Ord` = Str by string order, Extern by `cmp_value`
  (kinds never mix in a typed map). Deterministic iteration preserved; matches the VM's
  sorted-by-key display because extern ordering is the same `cmp_value` both sides.
- **The shared key contract** (must be identical in both backends, byte-for-byte observable):
  ordering is Str-by-content, Extern-by-`cmp_value`, and cross-kind Str < Extern (arbitrary but
  fixed; a typed map never mixes, `dyn` paths stay deterministic). Display: a Str key keeps its
  quoted `{k:?}` form; an extern key renders its display form UNQUOTED (it is not a string —
  `{019b76da-…: 1}`). `json.stringify` of an extern-keyed map uses the display form as the JSON
  object key (JSON keys are strings by definition).
- **Checker**: the string-key assertions become a key-capability rule: `K` is `string` OR an
  extern type with `key_capable: true` (enforced at `Map<K,_>`/`Set<T>` formation — a
  `Map<FileHandle,_>` is a type error). `keys()` returns `List<K>` (not hardcoded
  `List<string>`), `has`/`remove`/`set` take `K`. JSON decode keeps its string-keyed
  restriction (JSON object keys ARE strings) — the existing E-diag sites stand.
- **Out of scope, explicitly**: `int`/tuple/arbitrary-value keys. The `MapKey` enum is where
  they'd land later; this arc only adds the extern arm. (Noted in plans/deferred.md.)

## FileHandle migration — the effectful client (X3)

FileHandle exercises the corner Uuid can't: mutable receiver + Host-effectful methods +
unordered/non-keyable. Its method logic ALREADY lives shape-compatible in shared stdlib
(`handle.rs`: `read_line(&mut self, host)`, `read(&mut self, count, host)`, `write(&mut self,
&chunk)`, `close(&mut self)` — both backends are pure glue), so migrating is mostly deletion:

- `impl ExternValue for FileHandle` (eq = derived full-state PartialEq, as today; cmp = `None`;
  display = existing `display()`; `key_capable: false`) + an `ExtType` with `read_line`/`read`/
  `write`/`close` `ExtFn`s (rets: `Option<String>`, `Option<String>`, `Unit`, `Unit` — matching
  `file_handle_method` in the checker today) and a dispatch that downcasts and calls the
  existing methods. `fs.open`'s dispatch returns `NativeOut::Extern(Box::new(handle))`.
- **Deleted**: `Payload::FileHandle` + `HeapKind::FileHandle` + `with_file_handle{,_mut}`
  (heap.rs:935/946, lib.rs:780–790); eval `Value::FileHandle` + its display/type_name/equality
  arms; both `call_file_handle_method` twins; the `FileHandleMethod` enum + `from_name`;
  `NativeOut::FileHandle`; the VM `values_equal` file-handle rung (ops.rs:424 — subsumed by the
  extern rung); checker `FILE_HANDLE` const + `file_handle_method` + `file_handle_params`
  (subsumed by the table-driven extern arm).
- **Behavior-invariant by oracle**: the existing fs/handle conformance corpus pins read/write/
  cursor/close semantics and handle equality; the differential holds both backends to it
  through the swap. `SigType::Named("FileHandle")` in `FS_FNS` doesn't change at all.

The sibling *checker-only* native names (`Iterator`, `Sender`, `Signal`, …) are NOT migrated —
they are backend-builtin values coupled to the executor/reactive graph (the same reason
`reactive`/`task` stay virtual modules). Their names join the E0049 reservation set alongside
registered extern types, so reservation is uniform even where hosting is not.

## The async seam (X5) — extensions get async without touching the executor

Today `fs.*_async` bypasses the registry: each backend intercepts by name, marshals args into
the closed `IoRequest` enum, and tickets it (`executor.spawn_io(host, req)` →
`Value::make_async_io(id)`; `methods.rs:335`, eval twin). `IoRequest` already has exactly the
two-body shape an open seam needs — `run_io_sync(host)` (the sandbox body, run **at spawn**,
deterministic and in-oracle) and `run_io_real` (the real executor's self-contained tokio body,
host unused). Generalize the enum to a trait and the intercept disappears:

```rust
/// Async work a dispatch returns instead of a value (`NativeOut::Spawn(Box<dyn ExternIo>)`).
/// Plain Send data + two bodies, exactly the split `IoRequest` has today.
pub trait ExternIo: Send + std::fmt::Debug {
    /// The deterministic body: run synchronously against the Host. The sandbox executor runs
    /// this at spawn (ready on first poll — in-oracle, differential-identical), and it is the
    /// real executor's fallback when no real body is provided.
    fn run_sync(&mut self, host: &mut dyn Host) -> Result<NativeOut, StdError>;
    /// The real executor's concurrency body. Default = no real body (degrade to `run_sync`
    /// at spawn: correct, serial). Implementations override for true concurrency.
    fn run_real(self: Box<Self>) -> RealBody { RealBody::Sync(self) }
}

pub enum RealBody {
    Sync(Box<dyn ExternIo>),                                        // run_sync(host) at spawn
    Blocking(Box<dyn FnOnce() -> Result<NativeOut, StdError> + Send>),   // → blocking pool
    Async(Pin<Box<dyn Future<Output = Result<NativeOut, StdError>> + Send>>), // → runtime
}
```

- **Executor trait**: `spawn_io(host, IoRequest)`/`poll_io → IoOutcome` become
  `spawn_ext(host, Box<dyn ExternIo>)`/`poll_ext → NativeOut`. `IoRequest` + `IoOutcome` +
  `from_fs_async` are DELETED — the fs requests become an `FsIo` struct implementing
  `ExternIo` (`run_sync` = today's `run_io_sync`; `run_real` = `RealBody::Async` over today's
  `run_io_real`, verbatim). One ticket space, one future value kind (`make_async_io`
  unchanged).
- **Dispatch path**: `fs_dispatch` gains the `*_async` arms returning `NativeOut::Spawn` —
  ordinary registry functions with `ret: Future(T)` (`SigType::Future` already exists and the
  checker already maps it). Both backends' by-name intercepts (`vm_fs_async_request` + the
  eval twin) are DELETED; materialize handles `Spawn` by ticketing and wrapping. Await/resolve
  materializes the `NativeOut` exactly like a sync dispatch result.
- **Determinism**: in-oracle only `SandboxExecutor` runs, and it only ever runs `run_sync`
  at spawn — an extension's async function is deterministic under the differential *no matter
  what its real body does*. The real bodies are out-of-oracle by construction, same as today.
- **Why `Blocking` AND `Async`**: `Blocking` is the easy path (an HTTP extension over a
  blocking client, file IO — note `tokio::fs` is `spawn_blocking` underneath anyway);
  `Async` is the native path for genuinely async clients (hyper/reqwest). Both are two-line
  arms in the real executor; std-only types, no new dependency.

Async *methods* on extern types fall out for free: `TypeDispatch` returns `NativeOut`, so a
method arm may return `Spawn` like any function (none needed this arc — handle methods stay
sync). An HTTP library is the natural first out-of-tree client: it needs a network Host
capability + a deterministic virtual network (the Vfs analog) and is its OWN arc (decided with
the user — file functions are this arc's async client instead).

## New async file functions (X6) — proving the seam is open

The point of the open seam: adding an async function = adding a request type + a dispatch arm,
touching NO backend code. Prove it by rounding out the async fs surface with the async twins of
the remaining sync functions (`exists_async`, `list_async`, `remove_async` — exact set per
`FS_FNS` at implementation time): new `FsIo` variants + `FS_FNS` rows only. Conformance for
each (sandbox exact-value, concurrent-completion ordering under `concurrent`), plus a real-host
CLI test.

## Slices

- **X1 — the seam. ✅ DONE (`727b603`).** Bench gate: back-to-back A/B vs the parent commit
  (`fib`, `loop_sum`, `iter_pipeline`) — all flat within a ±15% machine-noise floor (the box
  runs concurrent builds; the noise floor was established by impossible "improvements" on
  untouched paths, and the one flagged size, eager/8000 "+16%", re-read 24% *below* baseline
  on the confirmation run). Mechanistically consistent: list receivers match their ladder rung
  before the extern rung; equality/ordering rungs are appended after all existing paths.
  `ExternValue` + `ExtType` + `Extension::types()` + E0049 reserved names
  (registered extern types + the checker-only native names); `NativeOut::Extern`/arg
  projection; `Payload::Extern` + `with_extern{,_mut}` + `Value::Extern(Rc<RefCell<…>>)` +
  equality/ordering/display/narrowing rungs in both backends; checker admit + table-driven
  methods; the shared method-dispatch path. Gate: full bench suite, no regression (equality
  chain appended-last + Payload untouched-size proof).
- **X2 — Uuid. ✅ DONE (`f22349e`)** (also fixed: eval's hand-written `Value: PartialEq` extern hole — `some(u) == some(u)` was silently false on one backend). `uuid` crate dep; `id.rs` rewritten over `uuid::Builder` (hand-rolled bytes
  deleted; exact-value pins prove identity); `ID_FNS` ret → `Named("Uuid")`; `id.parse`;
  methods `version`/`to_string`/`timestamp_ms`; narrowing + dyn conformance; real-host CLI
  test unchanged (observes canonical strings).
- **X3 — FileHandle migration. ✅ DONE (`1568b92`)** (bonus: every heap object's payload shrank 88→56 bytes — the inline FileHandle variant was the largest). `impl ExternValue for FileHandle` + `ExtType`; delete the
  hand-threaded hosting (payload/value variants, method twins, `FileHandleMethod`,
  `NativeOut::FileHandle`, checker tables — list above). Gate: existing fs/handle conformance
  corpus green through the swap (behavior pinned by oracle, zero expectation edits).
- **X4 — extern map keys. ✅ DONE (`af0ab27`, bench `7353dcc`).** Bench gate: no map bench
  existed, so `benches/map_keys.rs` (get-heavy + set-churn over string keys, 10k/100k) was
  added and A/B'd X3-vs-X4 twice (identical-code rerun as the noise probe): every delta inside
  the box's ±15% noise floor except one CONSISTENT signal — X4 is ~8-11% FASTER on get/100k
  (hashbrown taken directly + inline-more), in both runs. No regression; the string insert
  move/clone fast paths are structurally intact. `MapKey` shared in noeta-stdlib
  (ordering/display agreement by construction); checker key-capability rule; exact-value
  `Map<Uuid,T>`/`Set<Uuid>` conformance; `Map<FileHandle,_>` static-rejection test. NOTE
  (pre-existing, NOT this slice): the checker still accepts `Map<int,_>` and int-keyed literals
  statically while runtimes reject at runtime — recorded for the deferred sweep.
- **X5 — the async seam + fs migration. ✅ DONE (`cff9cb3`).** `ExternIo` + `RealBody` + `NativeOut::Spawn`;
  `Executor::spawn_ext`/`poll_ext` (deleting `IoRequest`/`IoOutcome`/`from_fs_async` — fs
  becomes an `FsIo: ExternIo` with today's two bodies verbatim); `fs_dispatch` `*_async` arms
  with `Future(T)` rets; DELETE both backends' by-name intercepts. Gate: existing async
  conformance corpus green with zero expectation edits; real-host CLI async test still shows
  genuine concurrency.
- **X6 — new async file functions. ✅ DONE (`9cd4c42`)** (metadata twins deliberately use the None-fallback — real semantics by construction + the degradation path exercised). The async twins of the remaining sync fs surface
  (`exists_async`/`list_async`/`remove_async` per `FS_FNS`) — new `FsIo` variants + signature
  rows ONLY (no backend edits — the proof the seam is open). Sandbox + real-host conformance.
- **X7 — docs + memory.** Wiki `id` section (+ a `Uuid` type section), fs async additions,
  native-extensions doc gains the type-registration + async story, plans/deferred.md entries
  (Host-coupled finalizers, non-extern key kinds, P-IC for extern methods if benches ever
  demand, HTTP library arc = network Host capability + virtual network), memory update.

Differential-green + leak-0 per slice, as always.
