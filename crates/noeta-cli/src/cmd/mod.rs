//! The subcommand families, one module per verb family (audit-4 finding 2). `lib.rs`
//! keeps the clap types, `run_cli` dispatch, and the unknown-subcommand recovery chain.

pub(crate) mod bench;
pub(crate) mod build;
pub(crate) mod cache;
pub(crate) mod check;
pub(crate) mod doc;
pub(crate) mod docs;
pub(crate) mod expand;
pub(crate) mod explain;
pub(crate) mod fmt;
pub(crate) mod grammar;
pub(crate) mod ide;
pub(crate) mod init;
pub(crate) mod native;
pub(crate) mod pm;
pub(crate) mod repl;
pub(crate) mod run;
pub(crate) mod serve;
pub(crate) mod servers;
pub(crate) mod test;
pub(crate) mod upgrade;
