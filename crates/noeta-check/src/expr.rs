//! **Expression typing** — the `impl Checker` split of the bidirectional engine, one file per
//! rule cluster (audit-3 decomposition): the check/synth core dispatch stays whole in [`core`];
//! operators, calls, member access, and patterns are its sibling rule modules. All methods moved
//! verbatim out of the crate root; one `struct Checker`, no signature changes.

pub(crate) mod calls;
pub(crate) mod core;
pub(crate) mod member;
pub(crate) mod ops;
pub(crate) mod patterns;
