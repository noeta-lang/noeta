//! The M0 prelude support: the small set of built-in capabilities programs rely on
//! without importing anything — today just the prelude name metadata.
//! (`IdGen`, the M0 id source, left with the id-entropy arc: `id.next_id()` is a registry
//! function over the Host's `Ids` capability now, one counter shared by both backends.)

/// The names reserved by the M0 prelude. Used by name resolution (Slice 8) and, later,
/// by the LSP to mark prelude identifiers. Grows as value-returning builtins land.
///
/// Keywords (`echo`, etc.) do NOT belong here: they are consumed by the parser as their own
/// statement forms and never reach identifier resolution, so listing them is dead weight and
/// misleadingly implies they are shadowable prelude bindings (they are not).
pub const PRELUDE_NAMES: &[&str] = &[
    "echo",
    // `len`/`map`/`filter`/`sum` left the prelude (prelude-redesign P1.2): they are collection
    // METHODS now (`xs.len()`, `xs.map(f)`), passable as values via method handles (`list.len`).
    "Ok",
    "Err",
    "some",
    "none",
    "panic",
    "assert",
    // `signal`/`computed`/`effect` left the prelude (P2a) for `use std.reactive`, and
    // `sleep`/`all`/`race`/`map_bounded` (P2b) for `use std.task` (`registry::VIRTUAL_MODULES`).
];
