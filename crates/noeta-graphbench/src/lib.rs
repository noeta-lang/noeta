//! `noeta-graphbench` — how well a composition of `noeta mcp` tool calls answers a graph question
//! about a Noeta codebase.
//!
//! The measurement has two sides that never touch. **Gold** comes from the compiler's own indices
//! (`noeta_ide::callgraph`, the reflection index, the linker's module paths), so every structural
//! answer is exact and needs no model. **Arms** are fixed compositions of MCP tool calls, driven
//! through the real [`noeta_mcp::NoetaMcp`] service over an in-process duplex, so a wire shape that
//! changes is a shape the benchmark sees. Scoring compares one against the other.
//!
//! The circularity is deliberate and bounded: gold derived from the graph tests **retrieval,
//! ranking and question-to-node mapping**, never graph correctness. A benchmark question cannot
//! fail for a reason the conformance corpus should have caught, because both sides read the same
//! graph. Graph correctness stays the conformance corpus's job.
//!
//! Everything here is deterministic. There is no model in the loop, no sampling, no clock in the
//! answer, so a run repeats exactly and its numbers belong in a checked-in baseline.

pub mod ablate;
pub mod adapter;
pub mod arms;
pub mod baseline;
pub mod corpus;
pub mod generate;
pub mod gold;
pub mod harness;
pub mod metrics;
pub mod question;
pub mod report;
pub mod service;

/// The repository-relative home of everything the benchmark reads and writes.
pub const DATA_DIR: &str = "tests/graphbench";

/// Locate `tests/graphbench` from the crate's own manifest directory, so the harness runs the same
/// from a worktree, a checkout, or `cargo run` in any directory.
pub fn data_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|crates| crates.parent())
        .map(|root| root.join(DATA_DIR))
        .expect("the crate sits two levels under the repository root")
}
