# noeta-ext-abi

The native-extension ABI (P-NATIVE): the contract a crate implements to register native modules and first-class types into the language, plus the dep-free primitives both backends and the front end share.

- **Takes in:** nothing beyond `serde`/`compact_str`/`equivalent`/`hashbrown` — deliberately dependency-lean.
- **Emits:** the `Extension` trait and the registration vocabulary (`ExtModule`/`ExtType`/`ExtClass`/`ExtEnum`/`ExtStruct`/`ExtTrait`/`ExtFn`, `NativeValue`/`NativeOut` marshalling), the `Host` capability supertrait (`Clock`/`Console`/`Entropy`/`Env`/`FileSystem`/`Ids`/`Network`/`Os`/`P2p`/`Rng`), `NativeCtx`/`CtxDispatch` for host re-entry, and the `registry` module that is the single source of truth for what's registered.

Split out of `noeta-stdlib` so the contract does not drag core's batteries (crypto/UUID/JSON): a third-party extension — and internal mid-end crates like `noeta-ir` — depend on this lean crate, while `noeta-stdlib` re-exports it (`pub use noeta_ext_abi::*`) and adds the concrete `std` modules on top, mirroring Rust's `core`/`std` relationship. It also carries the `ABI_VERSION` constant (bumped on any change to the registration/dispatch contract), the command surface (`ExtCommand`, for extension-provided CLI verbs), and the `net`/`stream`/`os`/`p2p`/`telemetry` capability vocabularies. The `stream` module additionally carries real *behavior* rather than only vocabulary — the incremental `FrameDecoder` behind `std.http.client.stream`, which is dependency-free precisely so the deterministic sandbox and the real reqwest-backed host cut bytes into frames with one parser and cannot disagree. See `plans/native-abi/README.md`.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
