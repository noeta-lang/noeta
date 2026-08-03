# noeta-project

The **project model**: which files are this program, and is it clean.

- **Takes in:** a directory (or an ordered `(uri, text)` member list), and the packages `noeta-pm` resolves for it.
- **Emits:** `project_check` — the one answer `noeta check`, the LSP's `workspace/diagnostic` and the MCP `check` tool all give — plus the entry/pool decomposition (`entry_pool`/`pool_modules`/`noe_files`) and the live salsa `Workspace` inputs underneath it.

Three surfaces answer "does this project compile", and they used to walk, activate and sweep in three places and disagree about what *clean* meant. There is one implementation here; the surfaces differ only in **which entries** they hand it. That sharing is the point and must be preserved.

This code grew inside `noeta-ide`, next to completion, inlay hints, semantic tokens and an embedded language-guide corpus. A batch checker should not depend on an editor crate to answer whether a project compiles, so it lives here now and `noeta-ide` depends on it (and re-exports it, so editor-side callers are unaffected): `noeta-loader` → `noeta-db` → **`noeta-project`** → `noeta-ide`.

**The standing rule: delegate, never re-derive.** Module paths, package roots, URI→path, dependency selection for a `--target`, the sibling pool an entry links against — every one of those is `noeta-loader`'s or `noeta-pm`'s answer, and asking them is not a style preference. Six `check`-vs-`run` divergences were fixed in a single day and every one was a local re-derivation that had drifted from its owner, in the worst available direction: `noeta check` exiting 0 on a tree `noeta run` refuses outright. A new question about which files are the program gets answered by calling into the loader or pm — and if the answer is not there yet, it goes there, not into a helper here.

Nothing that needs a cursor or a buffer belongs here; that is `noeta-ide`.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
