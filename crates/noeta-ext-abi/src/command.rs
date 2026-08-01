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
    /// An optional string flag with a default (`--host 0.0.0.0`, server-hmr S0).
    Str { default: &'static str },
    /// A boolean `--flag` (para-extraction: `noeta migrate --status`). Always parsed: present is
    /// `true`, absent `false` — read it with [`ParsedArgs::bool`].
    Bool,
    /// An optional string flag with **no** default (`--db <dsn>`): when absent, nothing is
    /// recorded and [`ParsedArgs::get_str`] returns `None` — the command supplies its own
    /// fallback chain (env var, manifest, …).
    OptStr,
    /// An optional path flag with **no** default (`--dir <path>`); absent means
    /// [`ParsedArgs::get_path`] returns `None` — see [`ArgKind::OptStr`].
    OptPath,
    /// An **optional positional word**, filled left-to-right in declaration order
    /// (`noeta migrate [new] [<name>]`). Absent means [`ParsedArgs::get_str`] returns `None`;
    /// the command body validates the combination (which words go together is grammar the
    /// command owns, not the parser).
    Word,
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
    strs: Vec<(&'static str, String)>,
    bools: Vec<(&'static str, bool)>,
}

impl ParsedArgs {
    pub fn push_path(&mut self, name: &'static str, value: PathBuf) {
        self.paths.push((name, value));
    }
    pub fn push_int(&mut self, name: &'static str, value: i64) {
        self.ints.push((name, value));
    }
    pub fn push_str(&mut self, name: &'static str, value: String) {
        self.strs.push((name, value));
    }
    pub fn push_bool(&mut self, name: &'static str, value: bool) {
        self.bools.push((name, value));
    }
    /// The parsed [`ArgKind::Str`] argument `name`, or `None` when no string argument of that
    /// name was declared/parsed — the honest probe behind [`ParsedArgs::str`].
    pub fn get_str(&self, name: &str) -> Option<&str> {
        self.strs
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }
    /// The parsed [`ArgKind::Path`] argument `name`, or `None` — see [`ParsedArgs::get_str`].
    pub fn get_path(&self, name: &str) -> Option<&Path> {
        self.paths
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_path())
    }
    /// The parsed [`ArgKind::Int`] argument `name`, or `None` — see [`ParsedArgs::get_str`].
    pub fn get_int(&self, name: &str) -> Option<i64> {
        self.ints.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
    }
    /// The parsed [`ArgKind::Bool`] argument `name`, or `None` when no boolean flag of that name
    /// was declared/parsed — see [`ParsedArgs::get_str`].
    pub fn get_bool(&self, name: &str) -> Option<bool> {
        self.bools.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
    }

    // The panicking accessors below are the ergonomic form for a command body reading its OWN
    // declared args (the CLI parses every declared arg, defaults included, before `run`). Asking
    // for an undeclared name is an author bug, not user input — `#[track_caller]` points the
    // panic at the command body's line and the message names the missing declaration, instead of
    // the bare `Option::expect` that used to abort the CLI opaquely (audit-2 F4).

    /// The declared [`ArgKind::Str`] argument `name` (defaulted by the CLI when absent).
    /// Panics when `name` was never declared as a string arg — declare it in the command's
    /// [`ArgSpec`]s, or probe with [`ParsedArgs::get_str`].
    #[track_caller]
    pub fn str(&self, name: &str) -> &str {
        self.get_str(name).unwrap_or_else(|| {
            panic!(
                "command argument `{name}` was not declared as a string arg (ArgKind::Str) — \
                 add it to the ExtCommand's ArgSpecs, or use ParsedArgs::get_str"
            )
        })
    }
    /// The declared [`ArgKind::Path`] argument `name` (the CLI guarantees presence). Panics when
    /// `name` was never declared as a path arg — see [`ParsedArgs::str`].
    #[track_caller]
    pub fn path(&self, name: &str) -> &Path {
        self.get_path(name).unwrap_or_else(|| {
            panic!(
                "command argument `{name}` was not declared as a path arg (ArgKind::Path) — \
                 add it to the ExtCommand's ArgSpecs, or use ParsedArgs::get_path"
            )
        })
    }
    /// The declared [`ArgKind::Int`] argument `name` (defaulted by the CLI when absent). Panics
    /// when `name` was never declared as an int arg — see [`ParsedArgs::str`].
    #[track_caller]
    pub fn int(&self, name: &str) -> i64 {
        self.get_int(name).unwrap_or_else(|| {
            panic!(
                "command argument `{name}` was not declared as an int arg (ArgKind::Int) — \
                 add it to the ExtCommand's ArgSpecs, or use ParsedArgs::get_int"
            )
        })
    }
    /// The declared [`ArgKind::Bool`] argument `name` (the CLI always parses a declared flag —
    /// present is `true`, absent `false`). Panics when `name` was never declared as a boolean
    /// flag — see [`ParsedArgs::str`].
    #[track_caller]
    pub fn bool(&self, name: &str) -> bool {
        self.get_bool(name).unwrap_or_else(|| {
            panic!(
                "command argument `{name}` was not declared as a boolean flag (ArgKind::Bool) — \
                 add it to the ExtCommand's ArgSpecs, or use ParsedArgs::get_bool"
            )
        })
    }
}

/// An argument of a synthesized [`EntryCall`].
#[derive(Debug, Clone)]
pub enum EntryArg {
    /// An integer literal (`http.serve(8080, …)`).
    Int(i64),
    /// A string literal (`http.serve(8080, fetch, "127.0.0.1")`, server-hmr S0).
    Str(String),
    /// A top-level identifier the loaded program defines (`fetch`) — a missing one surfaces as
    /// an ordinary check error against the program, exactly as if the user wrote the call.
    Ident(&'static str),
}

/// A trailing entry call the driver appends to the program — `<module>.<func>(<args>)`. This is the
/// whole trick behind `noeta serve`: the command supplies only the entry convention
/// (`http.serve(<port>, fetch)`); the mechanism is the exact same registered function a program can
/// call directly.
///
/// The driver appends it (with its `use`) to the entry **before the program links**, so it resolves
/// exactly as the same line written into the file would. Appending to the *linked* program instead
/// left it outside every decision linking makes — its import resolved nothing, and its names went
/// unqualified.
#[derive(Debug, Clone)]
pub struct EntryCall {
    /// The module the call names. Spell it **qualified** (`std.http.server`, `para.db`,
    /// `para.db.migrations`): the driver calls its last segment (`server.serve(…)`) and binds that
    /// segment with a synthetic `use` of the rest, so the entry call resolves whatever the program
    /// itself imports. A bare, single segment binds nothing and resolves through the program's own
    /// imports — which means a program that does not import the module gets "cannot find `server` in
    /// this scope" pointing at a line it never wrote, since a qualified reference requires a `use`.
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

    /// A string value from the consumer project's manifest — `manifest_str("db", "url")` reads
    /// `[db] url` from the nearest `noeta.toml` (para-extraction). Deliberately a **generic
    /// string-valued lookup**, not a typed per-table surface: a command reads its own convention
    /// keys without this ABI learning any package's schema. `None` when there is no manifest,
    /// the table/key is absent, or the value is not a string. Default: no manifest (a bare test
    /// driver need not implement it).
    fn manifest_str(&self, table: &str, key: &str) -> Option<String> {
        let _ = (table, key);
        None
    }

    /// Serve `file` across `workers` worker isolates on `host:port` (server-hmr S1 multi-core).
    /// The driver binds the listener once and gives each worker a cloned fd; the kernel
    /// load-balances connections. Default: fall back to a single-worker
    /// [`run_file`](CommandCtx::run_file) — a driver that has not implemented multi-core still
    /// serves, just on one core. Returns the process exit code.
    ///
    /// `entry` is **the command's own call**, the same [`EntryCall`] value it would hand
    /// [`run_file`](CommandCtx::run_file) for one worker (audit-10). It is passed rather than
    /// rebuilt because it used to be rebuilt: `std`'s `serve` declared the call, the ABI default
    /// here declared a second copy of it, and the CLI's multi-core path a third — under a comment
    /// asserting all of them were "built the same way". They were, until a signature change reached
    /// one and not the others. Now the declaration is one expression and every path runs *that*.
    ///
    /// `host`/`port` stay separate because they are not the call: they are the address the **driver**
    /// binds once, before any worker exists.
    fn serve_parallel(
        &mut self,
        file: &Path,
        entry: &EntryCall,
        host: &str,
        port: i64,
        workers: usize,
    ) -> u8 {
        let _ = (host, port, workers);
        self.run_file(file, Some(entry), None)
    }
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

impl ExtCommand {
    /// Field defaults for additive evolution (N3.6), mirroring `ExtModule::DEFAULTS`: write
    /// `ExtCommand { name, about, run, ..ExtCommand::DEFAULTS }` and a future optional field
    /// lands here once instead of in every registration. (`run` has no meaningful default — the
    /// placeholder exits with an error — so always name it explicitly.)
    pub const DEFAULTS: ExtCommand = ExtCommand {
        name: "",
        about: "",
        args: &[],
        run: |_, _| {
            eprintln!("internal: an ExtCommand registered without a body");
            2
        },
    };
}
