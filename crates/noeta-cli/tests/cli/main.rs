//! End-to-end tests for the `lang` binary itself: the `run` and `repl` subcommands, driven through
//! a real process so the CLI glue, exit codes, stdout/stderr split, and the REPL's interactive
//! behaviour are all exercised (none of which the library-level tests can reach). The conformance
//! corpus runner moved to its own dev binary (`noeta-conformance`), with its CLI tests alongside it.
//!
//! One test binary, split into per-verb/area modules (audit-4 F12) so unrelated arcs no longer
//! share one 6,000-line file, while still linking the `noeta` dependency tree once. Shared
//! fixtures live in [`support`].

mod support;

mod bench;
mod build;
mod check;
mod doc;
mod expand;
mod fmt;
mod grammar;
mod init;
mod isolates;
mod keyed;
mod migrate;
mod namespace;
mod pm;
mod pm_native;
mod repl;
mod run;
mod targets;
mod test_runner;
