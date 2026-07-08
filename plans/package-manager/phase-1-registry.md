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

### 1.1–1.3 — Negligible native rings: deliberately NOT byte-gated (decided 2026-07-08)

`ring-http-server`, `ring-crypto`, and `ring-id` were scoped for byte-gating but the code shows they
don't separate cleanly and the payoff is negligible:

- **`ring-http-server`** — the inbound `RealHost` server (`net_listen`/`net_accept`/`net_reply`,
  `ServerState`, `RealAcceptIo`) rides tokio, which `fs` already links. Gating it sheds **~0 bytes**.
  Documented no-op; `std.http.server.ring = None` and the server host code stays always-on.
- **`ring-crypto` / `ring-id`** — entangled, and ~55 KB + ~6 KB (vs http-client's ~3 MB, the win
  P0.3b + P1.0 already capture): `id::v5` shares `crypto::sha1` (deliberately, to avoid a second SHA-1
  impl), and `uuid` is used by *both* `id` and the CRDT/p2p types — so neither maps to a clean
  per-capability ring. Byte-gating them would contort the Cargo feature graph for negligible size.
  Left as declared-but-always-on (`ring = None`), matching dce.md §3's own "deliberately not
  hand-gated — negligible size." **User-confirmed** (agreed to skip; make `vec`/`quat` a unit instead).

### 1.4 — Registry assembly: kill the hardwired `&[&StdExtension]` ✅ DONE

Split `StdExtension` into per-capability in-tree `Extension` units sharing the `"std"` root
(`CoreExtension` = always-on Ring-1/2; `HttpExtension`, `CryptoExtension`, `IdExtension`,
`VecExtension` = the vec/quat pair, `P2pExtension`), and build the registry as an **assembled** list.
`extensions()` returns the assembled slice; `find_module`/`find_type`/`commands` already iterate it,
so the registered surface is byte-identical (faithful partition, differential-green). This is the seam
Phase 2/3 plug packages into: a package registers as a new `Extension` unit; Phase 3's out-of-tree
native path is where a unit's *deps* drop wholesale (its crate is simply not compiled). In-tree today
the heavy weight (reqwest) is already gated at the runtime layer (`noeta-runtime/ring-http-client`),
not by dropping the tiny registry entry — so the entries stay unconditional here.

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

## What defers to Phase 2 (surfaced, not silently narrowed)

The plan's Phase 1 bullet said *"in-tree first-party extensions become dependency-gated: an app that
doesn't declare a capability never links it."* The **declaration** half needs the `[dependencies]`
table — a **Phase 2** deliverable. Phase 1 delivers the *mechanism*: the ring is registry-declared,
capabilities are per-capability units, and a tailored `noeta build --native` already gates the one
heavy ring (reqwest) by **usage** (the footprint scan reads `ring_of`). Making the *manifest* the
authoritative selector (dce.md §4, footprint scan → cross-check fallback) rides Phase 2, because
there's nothing to declare until the dependency table exists. This is a scoping correction: Phase 1 =
the registry/ring *mechanism* + the extension-unit seam; manifest-driven gating = Phase 2.

## Phase 1 gate ✅ MET

The module→ring map lives only on the registry (no CLI table, P1.0); the registry is an assembled
per-capability list of `Extension` units, no hardwired `&[&StdExtension]` (P1.4); `vec`/`quat` are
their own unit (extraction-prep); the heavy `ring-http-client` is shed by a tailored native build via
the registry-declared ring; negligible rings deliberately left always-on (user-confirmed). Full corpus
+ JIT differential green (0 failed), clippy clean, no `unsafe` touched. Next: Phase 2 (package +
dependency system).
