# noeta-cache

A transparent, content-addressed startup cache for compiled bytecode.

- **Takes in:** source content (entry + sibling modules), the runtime version, the running binary's build identity, and the active tier set, via [`KeyBuilder`].
- **Emits:** a [`CacheKey`] (hex SHA-256 of all of the above) used to store/retrieve an opaque compiled `.noeb` blob (`Vec<u8>`).

`noeta run app.noe` re-lexes/parses/checks/compiles on every invocation; for a large program that front-end cost dominates startup (~118 ms measured on a 6000-line program, ~95% of wall time). This crate lets the CLI skip it: after a successful compile it stores the serialized blob keyed by everything that could change the output, and on the next run of unchanged sources it hands the blob straight back — the front end never runs. The crate is deliberately blob-shaped: it knows nothing about `Module`, the bytecode format, or `noeta-bundle` (the CLI produces/consumes the blob via `noeta_bundle::write`/`read`; this is just content-addressed storage), keeping it off the compile DAG (its only dependency is `sha2`).

Two invariants guard a default-on cache: **never a stale hit** (the key folds in every source, runtime version, binary identity, and tier set — any change misses) and **never poisoned** (the store lives under the user's private XDG cache dir, created mode `0700`, never a world-writable shared location).

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
