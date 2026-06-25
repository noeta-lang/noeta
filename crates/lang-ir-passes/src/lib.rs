//! Precise-reference-counting **passes over the Core IR** (memory-management migration, Phase 3).
//!
//! This crate hosts the analyses and IR→IR transforms that place reference-counting decisions —
//! last-use/liveness, `dup`/`drop` insertion, reuse-token threading — on the shared
//! [`lang_ir`] both backends execute. Because the annotated IR is the single program both tiers
//! run, prompt reclamation lands in both at the same points by construction (architecture §2).
//!
//! # What lives here so far
//!
//! [`liveness`] — a structured **backward dataflow** computing, for every **source variable**,
//! the point(s) at which its last use occurs. (ANF temporaries are single-use by construction —
//! each `let` has exactly one consumer — so their last use is trivial and handled directly by the
//! backends; the dataflow concerns the named bindings, which may be read many times across
//! branches and loops.) The result feeds the drop-insertion pass; it changes no behavior on its
//! own.
//!
//! # The load-bearing safety direction
//!
//! Every analysis here is **conservative in the "never too early" direction** (README §2): where
//! flow makes a last use uncertain, the value is treated as *still live* (its drop is omitted, so
//! it is reclaimed later by scope/teardown). A late drop costs only promptness; an early drop
//! would be a use-after-free, so it must be impossible by construction. Over-approximating *uses*
//! (e.g. a closure is taken to use every variable named in its body) is therefore always sound.

pub mod drops;
pub mod liveness;

pub use drops::{Relevance, insert_drops};
pub use liveness::{BlockLiveness, ProgramLiveness, StmtLiveness, VarSet, analyze};
