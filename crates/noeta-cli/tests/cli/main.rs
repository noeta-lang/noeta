//! End-to-end tests for the `lang` binary itself: the `run` and `repl` subcommands, driven through
//! a real process so the CLI glue, exit codes, stdout/stderr split, and the REPL's interactive
//! behaviour are all exercised (none of which the library-level tests can reach). The conformance
//! corpus runner moved to its own dev binary (`noeta-conformance`), with its CLI tests alongside it.
//!
//! One test binary, split into per-verb/area modules (audit-4 F12) so unrelated arcs no longer
//! share one 6,000-line file, while still linking the `noeta` dependency tree once. Shared
//! fixtures live in [`support`].

mod support;

mod automation;
mod bench;
mod build;
mod cache;
mod capture;
mod check;
mod derivation;
mod doc;
mod expand;
mod fmt;
mod grammar;
mod ide;
mod init;
mod isolates;
mod mcp;
mod namespace;
mod pm;
mod pm_native;
mod repl;
/// The interactive prompt, driven on a pty — unix-only, and only when it is compiled in.
#[cfg(all(unix, feature = "repl-tty"))]
mod repl_tty;
mod run;
mod run_tail;
mod targets;
mod test_runner;
mod upgrade;
