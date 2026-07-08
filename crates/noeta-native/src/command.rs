//! Extension **CLI commands** (higher-order-abi H6): the seam that lets an extension contribute
//! a `noeta <name>` subcommand — the `cargo clippy` model, in-process for compiled-in extensions
//! (a PATH/binary model can join at the package-manager milestone). `noeta serve` is the proving
//! client: it was a hardcoded variant of the CLI's closed `Command` enum purely because no such
//! seam existed.
//!
//! The capability is deliberately **narrow**: a command drives a program run ([`CommandCtx`] =
//! load + check + run a file on the real host, optionally with a synthesized trailing entry
//! call). It is not a general scripting hook — everything effectful still happens inside the
//! language program the command runs.

use std::path::{Path, PathBuf};

/// What kind of value a command argument accepts, and how the CLI wires it.
#[derive(Debug, Clone, Copy)]
pub enum ArgKind {
    /// A required positional file path (`noeta serve app.noe`).
    Path,
    /// An optional integer flag with a default (`--port 8080`).
    Int { default: i64 },
}

/// One argument a command declares; the CLI builds the real parser (help text, validation)
/// from these specs.
#[derive(Debug, Clone, Copy)]
pub struct ArgSpec {
    pub name: &'static str,
    pub help: &'static str,
    pub kind: ArgKind,
}

/// The parsed argument values, by declared name — what a command's `run` receives.
#[derive(Debug, Default)]
pub struct ParsedArgs {
    paths: Vec<(&'static str, PathBuf)>,
    ints: Vec<(&'static str, i64)>,
}

impl ParsedArgs {
    pub fn push_path(&mut self, name: &'static str, value: PathBuf) {
        self.paths.push((name, value));
    }
    pub fn push_int(&mut self, name: &'static str, value: i64) {
        self.ints.push((name, value));
    }
    /// The declared [`ArgKind::Path`] argument `name` (the CLI guarantees presence).
    pub fn path(&self, name: &str) -> &Path {
        &self
            .paths
            .iter()
            .find(|(n, _)| *n == name)
            .expect("a declared path argument is always parsed")
            .1
    }
    /// The declared [`ArgKind::Int`] argument `name` (defaulted by the CLI when absent).
    pub fn int(&self, name: &str) -> i64 {
        self.ints
            .iter()
            .find(|(n, _)| *n == name)
            .expect("a declared int argument is always parsed")
            .1
    }
}

/// An argument of a synthesized [`EntryCall`].
#[derive(Debug, Clone)]
pub enum EntryArg {
    /// An integer literal (`http.serve(8080, …)`).
    Int(i64),
    /// A top-level identifier the loaded program defines (`fetch`) — a missing one surfaces as
    /// an ordinary check error against the program, exactly as if the user wrote the call.
    Ident(&'static str),
}

/// A trailing entry call the driver appends to the loaded program —
/// `<module>.<func>(<args>)`. This is the whole trick behind `noeta serve`: the command supplies
/// only the entry convention (`http.serve(<port>, fetch)`); the mechanism is the exact same
/// registered function a program can call directly.
#[derive(Debug, Clone)]
pub struct EntryCall {
    pub module: &'static str,
    pub func: &'static str,
    pub args: Vec<EntryArg>,
}

/// The narrow driver capability a command runs against, implemented by the CLI: load + check a
/// program file and run it **on the real host**, optionally appending `entry` as the trailing
/// top-level statement. `banner` (a status line, e.g. "listening on …") prints to stderr after a
/// successful load, before the run — so a load/check failure exits without it. Returns the
/// process exit code (0 ok, 1 program error, 2 unreadable file).
pub trait CommandCtx {
    fn run_file(&mut self, file: &Path, entry: Option<&EntryCall>, banner: Option<&str>) -> u8;
}

/// A CLI subcommand contributed by an extension.
#[derive(Debug, Clone, Copy)]
pub struct ExtCommand {
    /// The subcommand name (`noeta <name>`). Must not collide with a core command.
    pub name: &'static str,
    /// One-line help shown in `noeta --help`.
    pub about: &'static str,
    pub args: &'static [ArgSpec],
    /// The command body: inspect the parsed args, drive the ctx, return the exit code.
    pub run: fn(&mut dyn CommandCtx, &ParsedArgs) -> u8,
}
