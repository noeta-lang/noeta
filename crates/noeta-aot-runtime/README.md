# noeta-aot-runtime

The **AOT runtime**: the static library a `noeta build --native` program links against to become a self-contained native executable.

- **Takes in:** a stapled bundle (a compiled `Module` appended to the linked binary, the L2 mechanism) plus a linker-resolved `noeta_aot_dispatch` table of native prototype bodies.
- **Emits:** a C-ABI `main` entry (`libnoeta_aot.a`) that decodes the bundle, binds the dispatch table into the VM's per-prototype entry tables, and runs — eligible prototypes dispatch straight to native code, the rest interpret.

A native artifact is laid out exactly like a Level-2 stapled exe (`[linked runtime | bundle | trailer]`); the difference is the runtime half. Where an L2 exe embeds the whole toolchain and interprets the bundle, an L3 native exe embeds this lean runtime plus the program's prototypes compiled to native code. It links `noeta-vm` (with default features off and only the `aot` feature on, dropping the whole compiler front end — no `noeta-compiler`/`noeta-check`/`noeta-ir`), `noeta-bundle`, `noeta-host-real`, and `noeta-stdlib`, each pulled to shed unused stdlib rings and the compiler surface a run-only artifact never needs. `noeta build --native`'s footprint scan builds this crate with only the rings a given program actually uses. Built `staticlib` (what `cc` links directly) plus `rlib` (so a *composed* AOT runtime — an app with native-dependency packages — can depend on it as an ordinary library and call [`run_embedded_with_extensions`] before bundling its own staticlib).

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
