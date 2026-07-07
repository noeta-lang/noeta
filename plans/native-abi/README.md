# P-NATIVE — extract the extension ABI into a `noeta-native` crate

**Branch:** `noeta-native` (off `main` @ `1b8543e`). **Origin:** the registry ABI (and the
neutral marshalling both backends use) lives in `noeta-stdlib` today, so **every** consumer of the
ABI — including third-party extensions, once the package manager lands — must depend on the whole
batteries crate (uuid, sha1/2, md-5, hmac, subtle, bcrypt, serde_json, bytemuck). That is backwards:
the contract a third party implements should not drag core's crypto/UUID tree. The user flagged this
directly ("shouldn't we have a `noeta-registry` crate?"). It also bites internally today —
`cargo build -p noeta-ir` (a mid-end crate that only uses `IntMethod`/`mask_to_width`/`TypeRecipe`)
compiles bcrypt and the SHA family.

This was already on the roadmap: `docs/Native-Extensions.md` and the native-extensions memory both
note "extract a stable `noeta-native` ABI crate at the package-manager milestone." We pull the crate
boundary forward now (it's internal hygiene; the *published/frozen* ABI is still a
package-manager-milestone concern — extracting the crate does not freeze anything).

## Target architecture

Two crates, `core`/`std`-style — **`noeta-stdlib` re-exports `noeta-native`** (`pub use
noeta_native::*`), so `stdlib = the ABI + the batteries`. Every existing `noeta_stdlib::X` path keeps
working unchanged (zero churn for `vm`/`eval`/`value`/`check`/`compiler`/`runtime`/`cli`/`dap`/
`conformance`); only the crates that can *shed* stdlib actively switch.

### `noeta-native` (new) — the contract + dep-free primitives. Deps: `compact_str`, `equivalent`, `hashbrown` only.
- **Primitives** (all of today's `lib.rs`, dep-free): `Arg`/`Output`/`Dispatch`/`ErrorKind`/
  `StdError` + error builders; `string_method` + `STRING_METHODS`; `int_method`/`int_method_width`/
  `mask_to_width`/`num_convert` + `IntMethod`/`NumScalar`/`NumConvert`; `ListMethod`/`MapMethod`/
  `SetMethod`; `format_float`/`format_f32`/`bytes_to_hex`; `VEC_SCALAR_FUNCTIONS`.
- **ABI types** (from `registry.rs`): `Scalar`, `NativeValue`, `NativeOut`, `SpawnBox`, `SigType`,
  `RetTy`, `TypeRecipe`, `ExtFn`, `ExtModule`, `ExtType`, `Extension`, `ModuleDispatch`,
  `TypeDispatch`. (Generic lookups `find_module`/`dispatch`/… stay in stdlib — they reference
  `StdExtension`.)
- **`extern_value.rs`** (`ExternValue`, `ExternBox`), **`map_key.rs`** (`MapKey`, `ExternKeyRef`).
- **`executor.rs`** (`Executor`, `ExternIo`, `RealBody`, `FsIo`, `SandboxExecutor`).
- **`host.rs`** — the capability *traits* (`FileReader`/`FileSystem`/`Rng`/`Clock`/`Env`/`Entropy`/
  `Ids`/`Network`) + `Host` + blanket impl. (`Network::net_spawn`'s default builds a
  `net::NetFetchIo`, which is also native — no cycle.)
- **`net.rs`** — data types only: `NetRequest`, `NetResponse` (+ `header_value`/`path_of`),
  `NetFetchIo`, `impl ExternValue for NetResponse`, `RESPONSE_TYPE_NAME`.

### `noeta-stdlib` (keeps) — the batteries. Depends on `noeta-native`; `pub use noeta_native::*`.
- Concrete modules with heavy deps: `crypto` (sha/hmac/bcrypt/subtle), `id` (uuid), `json`
  (serde_json), `vec3`/`quat` (bytemuck), plus the dep-free `math`/`random`/`env`/`fs`/`handle`/`iter`.
- `SandboxHost` + `SANDBOX_EPOCH_MS`/`SANDBOX_ENTROPY_SEED` (only `vm`/`eval`/`conformance` use it —
  all keep stdlib). `net::sandbox_respond` (the serde_json httpbin responder — content, not ABI).
- The **registry std half**: `StdExtension`, `REGISTRY`, `STD_MODULES`/`STD_TYPES`, every `*_dispatch`
  fn, and the router entry points (`find_module`/`find_function`/`find_type`/`find_type_method`/
  `dispatch`/`dispatch_method`/`is_module_function`/`is_virtual_module`/`virtual_module_function`/
  `extensions`/`VIRTUAL_MODULES`).

**Acyclic:** `native` → {compact_str, equivalent, hashbrown}. `stdlib` → native + heavy deps. No
crate depends back on native's consumers. `noeta-value` → native + stdlib (it calls
`json::stringify`, so it legitimately keeps stdlib; it does not shed the tree — that's honest).

## Who sheds `noeta-stdlib` (the concrete internal win)
`noeta-ir`, `noeta-bytecode`, `noeta-db` use only `{IntMethod, int_method_width, mask_to_width,
TypeRecipe}`, all native — so all three switch their Cargo dep `noeta-stdlib → noeta-native` (the
correct home for those ABI types).

Tree-shedding actually lands for **`noeta-ir` and `noeta-bytecode`**: `cargo tree` confirms 0
heavy crates (no bcrypt/sha2/uuid/serde_json/bytemuck) in their transitive graphs after the swap.
**`noeta-db` does NOT shed** the tree — it transitively depends on `noeta-check`/`noeta-compiler`,
which legitimately need `noeta-stdlib` for the dispatch router — but sourcing `TypeRecipe` from its
true home (native) rather than a stdlib re-export is still the right import.

Everyone else keeps stdlib (they run or inspect the full std surface — legitimate) and rides the
re-exports.

## Slices (commit per green slice; full gate = 73 suites + differential + leak + doc-samples + fmt + clippy)
- **N0** — scaffold `noeta-native` (Cargo.toml + empty lib.rs). Workspace builds.
- **N1** — move the leaf files (`extern_value`, `map_key`, `executor`) + all `lib.rs` primitives into
  native; `stdlib` re-exports. Green, zero consumer churn.
- **N2** — split `host.rs` (traits→native, SandboxHost→stdlib), `net.rs` (data→native,
  responder→stdlib), `registry.rs` (ABI types→native, std half→stdlib). `stdlib` re-exports. Green.
- **N3** — migrate `ir`/`bytecode`/`db` to depend on `noeta-native` directly; drop their stdlib dep.
  Verify `cargo tree -p noeta-ir` no longer shows bcrypt/sha2/uuid.
- **N4** — docs (`Native-Extensions.md` status + crate-layout note; `deferred.md`) + memory.

## Guardrails
The differential/leak/conformance oracles make this tedious-not-risky: behavior cannot drift because
the shared dispatch bodies move verbatim. Bench is not required (no hot-path logic changes — only
crate boundaries; the interned-shape/value hot paths are untouched).
