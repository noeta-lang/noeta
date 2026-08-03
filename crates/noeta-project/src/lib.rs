//! **The project model** — which files are this program, and is it clean.
//!
//! One question, three surfaces. `noeta check [PATH]`, the LSP's `workspace/diagnostic` and the MCP
//! `check` tool all answer "does this project compile", and they used to walk, activate and sweep in
//! three places and disagree about what *clean* meant. [`project_check`] is the one implementation;
//! the surfaces differ only in **which entries** they hand it. That sharing is the point of this
//! crate and must be preserved.
//!
//! What changed is only *where it lives*. This code grew inside `noeta-ide`, beside completion,
//! inlay hints, semantic tokens and an embedded language-guide corpus — 18k lines of editor. A batch
//! checker should not depend on an editor crate to answer whether a project compiles, so the project
//! model moved out and `noeta-ide` now depends on it rather than containing it:
//!
//! ```text
//! noeta-loader → noeta-db → noeta-project → noeta-ide
//!                               ↑               ↑
//!                          noeta-cli       lsp / mcp / playground
//! ```
//!
//! # The rule this crate exists to enforce
//!
//! **Delegate project-model questions to [`noeta_loader`] and [`noeta_pm`]; never re-derive them.**
//!
//! This is not style. Six separate `check`-vs-`run` divergences were found and fixed in a single
//! day, and every one of them was this code re-deriving something the loader or the package manager
//! already answers — module paths, package roots, URI-to-path, dependency selection for a
//! `--target`, and the sibling module pool an entry links against. Each re-derivation started
//! agreeing with its owner and drifted, and the failure mode is the worst one available: `noeta
//! check` exiting **0** on a tree `noeta run` refuses outright. Quietly clean.
//!
//! The reason it drifted is that it lived in a crate nobody thought of as owning the project model.
//! Naming the crate is half the fix; the other half is that a new question about *which files are
//! the program* gets answered by calling into the loader or pm from here, and if the answer is not
//! there yet it goes there — not into a local helper.
//!
//! The agreement is guarded, not just asserted: `noeta-ide/tests/agreement.rs` puts the salsa
//! front end and the batch loader behind one function and pins every divergence that has been
//! found, and `noeta-fuzz`'s `--test project` oracle checks the two against each other over
//! generated project layouts.
//!
//! # What is here
//!
//! - [`project`] — [`project_check`], the sweep, the entry/pool decomposition ([`entry_pool`],
//!   [`pool_modules`], [`noe_files`]) and the tier activation.
//! - [`workspace`] — the shared disk-backed salsa [`Workspace`](noeta_db::Workspace) construction
//!   underneath it: members, dependency modules and per-package editions built from an ordered
//!   `(uri, text)` list, with live inputs reused in place across refreshes.
//!
//! # The public surface is the consumed surface
//!
//! [`workspace`] is public because the editor genuinely builds on it: [`sync`](workspace::sync)
//! and the [`WorkspaceCache`](workspace::WorkspaceCache) it returns are the construction
//! `noeta-ide` overlays unsaved buffers onto — that sharing is *why* check and run agree — plus
//! the [`SourceRef`](workspace::SourceRef)/[`SourceKind`](workspace::SourceKind) view over it and
//! the URI helpers ([`uri_to_path`](workspace::uri_to_path),
//! [`path_to_uri`](workspace::path_to_uri), [`project_root`](workspace::project_root),
//! [`workspace_key`](workspace::workspace_key), [`edition_of_uri`](workspace::edition_of_uri),
//! [`disk_noe_uris`](workspace::disk_noe_uris)), which exist so no consumer decodes a `file:` URI
//! twice.
//!
//! Everything else is `pub(crate)` and belongs there: dependency resolution's `ResolvedDeps`, and
//! the tombstone/target bookkeeping `sync` keeps between refreshes. Those are how the construction
//! works, not what it offers, and a consumer that reaches one has pinned an internal invariant
//! rather than an interface — which is exactly the shape the six divergences had. Widen this
//! surface only for something a consumer cannot express through the accessors, and prefer adding
//! the accessor.
//!
//! One thing on that line is already leaning the wrong way and is recorded rather than blessed:
//! `WorkspaceCache`'s `source_uris`/`programs`/`dep_*` vectors are read *directly*, index by index,
//! at about thirty sites in `noeta-ide` — including one that indexes `programs` by a `SourceId`'s
//! raw `.0`. That is the range convention [`SourceKind`](workspace::SourceKind) was introduced to
//! retire, in the crate that owns the invariant's other half. The fields stay `pub` because those
//! call sites exist; the direction of travel is `source`/`find_member`/`sources_with`, and a new
//! call site should use them.
//!
//! # What is deliberately NOT here
//!
//! Anything that needs a cursor. Completion, hover, signature help, inlay hints, semantic tokens,
//! symbol outlines, the call graph and the unsaved-buffer store are `noeta-ide`'s, and so is the
//! watch-mode impact engine (`noeta_ide::impact`) — it is change *attribution* over the static call
//! graph, an editor index, and it consumes this crate's workspace construction rather than defining
//! it.

pub mod project;
pub mod workspace;

pub use project::{
    ProjectCheck, ProjectCheckOptions, ProjectDiagnostic, check_sources, entry_pool, noe_files,
    pool_modules, project_check,
};
