# Phase 1 — Dynamic extension registry + manifest dependency-gating

*Parent: [`README.md`](README.md). Builds on Phase 0's root-qualified module identity. Still in-tree
(no network, no cross-package resolution — that's Phase 2). Design source: `plans/aot/dce.md`
carry-over §2–4.*

## What Phase 1 is

The AOT DCE arc gated exactly one ring by hand (`ring-http-client`) and parked the rest, leaving two
kinds of debt this phase pays:

1. **The module→ring map is duplicated and hand-maintained.** It lived in the CLI
   (`module_ring`/`fn_ring` match tables in `noeta-cli`) *disconnected* from the module it describes.
   Adding a native-backed module meant remembering to add a CLI row. dce.md §4: *"ideally each
   `Extension`/`ExtModule` declares its ring/feature, so the mapping isn't a hand-maintained table."*
2. **The registry is a hardwired 1-element static** (`static REGISTRY = &[&StdExtension]`). The
   "assemble the runtime from exactly the capabilities the program needs" principle wants the registry
   to be an *assembled* list of capability units, each includable/excludable wholesale (dce.md §3) —
   the seam Phase 2/3 populate with packages.

Phase 1 makes the **native-dependency ring a first-class property of the registry**, declared once on
the module, consumed by the footprint scan; gates the remaining heavy rings (`crypto`, `id`) so a
tailored AOT archive sheds them; and turns the registry into an assembled per-capability list. No
language-surface change; the differential oracle is untouched (rings only affect the native AOT
archive's link line, never observable behavior).

## Slices

Each commits green (`cargo test --workspace`, differential + conformance, fmt/clippy).

### 1.0 — `ExtModule` declares its ring; retire the CLI ring tables ✅ DONE

- Add `ring: Option<&'static str>` to `ExtModule` (`noeta-native/registry.rs`); `None` = always-on
  core, `Some("ring-http-client")` on `std.http.client`. The string equals the `noeta-aot-runtime`
  Cargo feature the ring turns on.
- Add `registry::ring_of(module) -> Option<&'static str>` (`= find_module(module).ring`) in
  `noeta-stdlib` — accepts a root-qualified path, a bare name, or a turbofish's bound local, so all
  three bytecode forms the footprint scan walks funnel through one lookup.
- `noeta-cli`'s `aot_ring_features` reads `ring_of` instead of the `module_ring`/`fn_ring` match
  tables; **delete** those tables. The registry is now the single source of truth for module→ring.
- Faithful refactor: `ring_of` returns exactly the mapping the tables did, so the AOT footprint
  selection is byte-identical. The `aot_ring_features_selects_http_client_but_not_server` test
  exercises it end-to-end (unchanged — it never named the deleted helpers).

### 1.1 — Gate `ring-http-server` (capability completeness)

The inbound `RealHost` server code (`net_listen`/`net_accept`/`net_reply`, `servers`/`conns`,
`ServerState`, `RealAcceptIo`) is currently always-compiled. It rides tokio (already linked for `fs`),
so this is near-zero bytes — but it finishes the client/server capability separation (dce.md §2): a
program using neither http ring links no http host code at all. `std.http.server`'s `ExtModule.ring`
stays `None` for *reqwest* but the RealHost server methods gate behind `ring-http-server`.
**Open question to resolve in-slice:** whether the ~0-byte payoff justifies the `#[cfg]` surface, or
whether http-server stays always-on and this slice is a documented no-op. Surfaced, not pre-decided.

### 1.2 — Gate `ring-crypto`

Make `sha1`/`sha2`/`md-5`/`hmac`/`bcrypt` **optional** in `noeta-stdlib` behind a `ring-crypto`
feature (default-on everywhere, so `noeta run` / the checker / the differential keep crypto); `#[cfg]`
the crypto module registration + its dispatch/type so a `--no-default-features` archive build without
`ring-crypto` compiles without those crates. Forward the feature `noeta-stdlib` → `noeta-runtime` →
`noeta-aot-runtime` (default set), and add it to the aot-runtime default so the plain archive stays
fully capable. `ring_of("std.crypto")` → `Some("ring-crypto")` closes the footprint loop. ~55 KB.

### 1.3 — Gate `ring-id`

Same shape for `uuid` (~6 KB). `id::Uuid` is used by crypto's `uuid_v5` — so `ring-crypto` depends on
`ring-id` (or the `uuid_v5` path is itself `#[cfg]`'d). Resolve the coupling in-slice.

### 1.4 — Registry assembly: kill the hardwired `&[&StdExtension]`

Split `StdExtension` into per-capability in-tree `Extension` units sharing the `"std"` root
(`CoreExtension` = always-on rings; `HttpExtension`, `CryptoExtension`, `IdExtension`, `P2pExtension`,
…), and build the registry as an **assembled, feature-gated list** — a capability whose ring is off at
build time drops out of the list *wholesale* (the uniform model dce.md §3 wants), instead of per-site
`#[cfg]`. `extensions()` returns the assembled slice; `find_module`/`find_type`/`commands` already
iterate it, so they're unchanged. This is the seam Phase 2/3 plug packages into.

### 1.5 — Docs + memory

Update `docs/Native-Extensions.md` (ring declaration + capability-unit model); update the arc memory.

## Deferrals surfaced (not silently narrowed)

- **`vec`/`quat` *physically* leaving core** (own crate/package) is **Phase 3** (out-of-tree native).
  Phase 1 can make them a distinct in-tree `Extension` *unit* (1.4), proving the multi-extension
  registry, but the true crate/package exit needs the out-of-tree native path. Flagged per
  *confirm-before-deferring-scope*.
- **p2p real p2panda transport** is the **p2p arc's own P3** — Phase 1 delivers only the *packaging*
  (p2p as a dependency-gated first-party extension unit + its ring). The transport wiring rides the
  p2p arc once this registry seam exists.

## Phase 1 gate

Each native-backed std ring (`http-client`, `crypto`, `id`) is a declared unit a tailored
`noeta build --native` sheds when unused; the module→ring map lives only on the registry (no CLI
table); the registry is an assembled per-capability list (no hardwired `&[&StdExtension]`); full
corpus + JIT differential + AOT differential green; clippy clean; no `unsafe` touched outside the
already-relaxed aot-runtime.
