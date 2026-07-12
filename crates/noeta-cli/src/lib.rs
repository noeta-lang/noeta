//! `noeta` — the user-facing toolchain, as a **library** with a thin binary shim (`src/main.rs`
//! is `run_cli(&[])`). The library form exists for package-manager Phase 3: a composed toolchain
//! (an app with native extension dependencies) is a generated crate depending on this one, whose
//! `main` passes the extra extension units into [`run_cli`].
//!
//! Exposes `run` (execute a file), `test` (run a program's `@test` blocks), `dump` (disassemble to
//! VM bytecode — a debugging aid), and `repl` (interactive); all drive the same pipeline crates, so
//! the binary is thin glue. The binary is
//! named `noeta` (the Noeta toolchain binary). The conformance corpus / differential
//! / leak harness that tests the *implementation* is a separate dev binary (`noeta-conformance`), not
//! a subcommand here — which is what keeps the `noeta test` verb free for a user program's own
//! `@test {}` blocks (object-model slice 6).

// The runtime is allocation-heavy (every heap value — strings, lists, maps, objects — is a boxed
// `Obj`), so the toolchain binary uses mimalloc instead of the system allocator. Correctness is
// unaffected (the leak oracle counts live objects, not allocator behavior); it is a throughput win
// on allocation-bound programs.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Instant;

use clap::{Parser, Subcommand};
use noeta_ast::{AttrValue, Expr, Program, Stmt};
use noeta_check::TierFn;
use noeta_diagnostics::{Diagnostic, DiagnosticCode, render, render_mapped};
use noeta_lexer::{TokenKind, lex};
use noeta_parser::{parse, parse_fragment};
use noeta_span::{Source, SourceId, SourceMap, Span};
use noeta_vm::{SessionOutput, VmBackend, VmSession};

// The package manager (package-manager P2) now lives in the `noeta-pm` library so `noeta-lsp` and
// `noeta-db` resolve dependencies through the same code; the CLI names its modules unqualified.
use noeta_pm::{graph, lock, manifest, registry};

mod compose;
mod docgen;

#[derive(Parser)]
#[command(name = "noeta", version, about = "The Noeta toolchain")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// How `noeta check` renders its result: for a human terminal or as a machine-readable report.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum OutputFormat {
    Human,
    Json,
}

#[derive(Subcommand)]
enum Command {
    /// Run a program file.
    Run {
        /// Path to a `.noe` file.
        file: PathBuf,
        /// Activate a dev-tier for this run, e.g. `--tier debug` to compile in `@debug { … }`
        /// blocks (object-model slice 6). Repeatable. Without it, every tier block is stripped.
        /// (The interim active-set interface, complementary to `--target`.)
        #[arg(long)]
        tier: Vec<String>,
        /// Activate the tiers a build target makes live (from `noeta.toml`), e.g.
        /// `--target dev`. Unioned with any `--tier`.
        #[arg(long)]
        target: Option<String>,
        /// Bypass the transparent startup cache for this run: don't read a cached compile and don't
        /// write one. Equivalent to setting `NOETA_NO_CACHE`. Recompiles from source regardless.
        #[arg(long)]
        no_cache: bool,
        /// Report tier-1 JIT activity to stderr after the run: compile coverage, the **bail
        /// histogram** (which ops sent native code back to the interpreter, how often, where), and
        /// any loops declined native compilation with the ops responsible — the measurement behind
        /// "what should become JITable next". Recording costs one branch per bail event.
        #[arg(long)]
        jit_stats: bool,
        /// Arguments passed through to the program, after a `--` separator:
        /// `noeta run app.noe -- --verbose input.txt`. The program reads them with `args.all()`,
        /// which — matching a shipped `noeta build --exe` binary run directly — reports the program
        /// path as the first element followed by these arguments.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Discover and run a program's `@test` blocks (object-model slice 6).
    Test {
        /// Path to a `.noe` file.
        file: PathBuf,
        /// Stop after the first failing test instead of running them all.
        #[arg(long)]
        fail_fast: bool,
        /// Number of tests to run concurrently (default: the machine's parallelism).
        #[arg(long, short)]
        jobs: Option<usize>,
        /// Run only tests tagged `#[Group("<name>")]` with this group.
        #[arg(long)]
        group: Option<String>,
        /// Run only the test fn(s) with these names (repeatable; exact fn-name match). Composes
        /// with `--group`. Used by editor test explorers to run a single test.
        #[arg(long = "name")]
        names: Vec<String>,
        /// Report outcomes as one JSON object on stdout instead of the human report — the
        /// machine-readable seam editor integrations parse. The tests' own stdout is captured into
        /// the JSON (failures carry it), never interleaved.
        #[arg(long)]
        json: bool,
        /// Only run when the `test` tier is live in this `noeta.toml` build target; otherwise the
        /// runner does nothing.
        #[arg(long)]
        target: Option<String>,
    },
    /// Discover and run a program's `@bench` blocks, measuring each (object-model slice 6).
    Bench {
        /// Path to a `.noe` file.
        file: PathBuf,
        /// Override the iteration count for every benchmark, taking precedence over a per-bench
        /// `@bench(iterations: N)` directive. Without either, the count is **calibrated**: a
        /// short probe estimates per-iteration cost and the count is sized so one measurement
        /// takes roughly 50ms.
        #[arg(long)]
        iterations: Option<u64>,
        /// Run only the bench fn(s) with these names (repeatable; exact fn-name match). Used by
        /// editor integrations to run a single benchmark.
        #[arg(long = "name")]
        names: Vec<String>,
        /// Report results as one JSON object on stdout instead of the human report — the
        /// machine-readable seam editors and CI parse.
        #[arg(long)]
        json: bool,
        /// After measuring, save the results as the named baseline (in the noeta cache dir,
        /// per-entry-file — timings are machine-local).
        #[arg(long, value_name = "NAME")]
        save_baseline: Option<String>,
        /// Compare each result against the named baseline: the report gains a delta column
        /// (`+5.2% vs NAME`), the JSON a `baselineDeltaPct` field.
        #[arg(long, value_name = "NAME")]
        baseline: Option<String>,
        /// The CI regression gate: with `--baseline`, fail (exit 1) when any bench regresses more
        /// than this percentage against it (e.g. `10` allows up to +10%).
        #[arg(
            long,
            value_name = "PCT",
            requires = "baseline",
            allow_negative_numbers = true
        )]
        max_regress: Option<f64>,
        /// Only run when the `bench` tier is live in this `noeta.toml` build target; otherwise the
        /// runner does nothing.
        #[arg(long)]
        target: Option<String>,
    },
    /// Extract a program's `@doc { … }` text blocks to stdout, or — with `--out` — generate the
    /// package's documentation artifact (a registry-ready `docs.json` plus a Markdown tree).
    Doc {
        /// Path to a `.noe` file. Omit with `--package` to fetch a published package's docs.
        file: Option<PathBuf>,
        /// Fetch a **published** package's stored documentation from the registry instead of
        /// reading local source: `company/package` (highest published version) or
        /// `company/package@1.2.0`. Prints the `docs.json` to stdout; with `--out`, writes it and
        /// renders the Markdown tree.
        #[arg(long, value_name = "NAME[@VERSION]", conflicts_with = "file")]
        package: Option<String>,
        /// Generate the documentation artifact into this directory instead of extracting to
        /// stdout: `docs.json` (schema-versioned, keyed by the package's `[package]` identity —
        /// the canonical form a registry indexes) plus `index.md` and one Markdown page per
        /// module, woven from `@doc` prose and the public API's signatures.
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// Only extract when the `doc` tier is live in this `noeta.toml` build target; otherwise
        /// nothing is emitted.
        #[arg(long)]
        target: Option<String>,
    },
    /// Compile a program to a self-contained `.noeb` bundle (P-AOT L1): the versioned bytecode a
    /// `noeta run app.noeb` executes directly, so a program ships **without its `.noe` source**.
    /// Uses the same compile pipeline as `run`; dev-tier blocks are stripped unless made live by
    /// `--tier`/`--target`, so a production build never carries `@test`/`@debug`/`@doc` content.
    Build {
        /// Path to the entry `.noe` file.
        file: PathBuf,
        /// Output path (default: the input path with a `.noeb` extension, or — with `--exe` — with
        /// its extension stripped, e.g. `app.noe` → `app`).
        #[arg(long, short)]
        out: Option<PathBuf>,
        /// Emit a self-contained executable (P-AOT L2) instead of a `.noeb`: the bundle is stapled
        /// onto a copy of this runtime binary, so the artifact runs the program on its own with no
        /// separate `.noeb` or interpreter alongside it.
        #[arg(long)]
        exe: bool,
        /// Emit a **native** executable (P-AOT L3): every eligible prototype is compiled ahead of
        /// time to machine code and linked into the binary (the rest interpret), then the bundle is
        /// stapled on as with `--exe`. Requires a C toolchain (`cc`); the AOT runtime archive is
        /// located via `NOETA_AOT_RUNTIME_LIB`, else built from the workspace (interim).
        #[arg(long)]
        native: bool,
        /// Activate a dev-tier for this build, e.g. `--tier debug`. Repeatable.
        #[arg(long)]
        tier: Vec<String>,
        /// Activate the tiers a `noeta.toml` build target makes live. Unioned with any `--tier`.
        #[arg(long)]
        target: Option<String>,
    },
    /// Disassemble a program to its VM bytecode (a debugging aid: shows the exact opcodes,
    /// constants, shapes, and method tables `noeta run` executes). Compiled with the same pipeline
    /// as `run`, so the output reflects what actually runs.
    Dump {
        /// Path to a `.noe` file.
        file: PathBuf,
        /// Activate a dev-tier before disassembling (as `noeta run --tier …`). Repeatable.
        #[arg(long)]
        tier: Vec<String>,
        /// Activate the tiers a `noeta.toml` build target makes live. Unioned with any `--tier`.
        #[arg(long)]
        target: Option<String>,
    },
    /// Statically check a program without running or building it: parse every `.noe` file and verify
    /// it type-checks, reporting all diagnostics (the `cargo check` / `tsc --noEmit` primitive). Uses
    /// the same load → link → type-check pipeline as `run`, then stops before codegen. Exits non-zero
    /// if any error-severity diagnostic is found; warnings print but do not fail.
    Check {
        /// File or directory to check (default: the current directory, walked recursively for
        /// `.noe` files). A directory checks every file it contains; a file checks just that one
        /// (with its directory-sibling modules linked in, as `run` does).
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Activate a dev-tier before checking, e.g. `--tier debug` to check inside `@debug { … }`
        /// blocks (as `noeta run --tier …`). Repeatable.
        #[arg(long)]
        tier: Vec<String>,
        /// Activate the tiers a `noeta.toml` build target makes live. Unioned with any `--tier`.
        #[arg(long)]
        target: Option<String>,
        /// Output format. `human` (default) renders diagnostics for a terminal; `json` emits a
        /// single machine-readable report on stdout for tools (CI, editors, the MCP server).
        #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
        format: OutputFormat,
    },
    /// Start an interactive REPL. Entries type-check before running (an entry with a type error
    /// prints its `E0xxx` diagnostics and is skipped) — the default since session-checker C2/C5.
    Repl {
        /// Skip per-entry type checking (the pre-C2 behavior: every well-parsed entry runs, type
        /// errors surface at run time). Also toggleable at the prompt with `:check on` / `:check off`.
        #[arg(long)]
        no_check: bool,
        /// Run this program to completion first (fully checked, imports resolved), then open the
        /// prompt with everything it declared and bound live — a bootstrapped session ("tinker"):
        /// a framework's bootstrap script gives an app-context REPL. A bootstrap that fails to
        /// load, check, or run exits with its diagnostics instead of opening a broken prompt.
        #[arg(long, value_name = "FILE")]
        load: Option<PathBuf>,
    },
    /// Run the Noeta language server over stdio (LSP). Started by an editor client (e.g. the
    /// VS Code extension); speaks JSON-RPC on stdin/stdout. Provides live diagnostics, hover
    /// types, and navigation over the compiler's incremental query graph.
    Lsp,
    /// Run the Noeta debug adapter over stdio (DAP). Started by an editor's debug UI; speaks the
    /// Debug Adapter Protocol on stdin/stdout. Runs a program under the production VM (JIT unarmed
    /// for full introspection) with breakpoints, stepping, and variable inspection.
    Dap,
    /// Run the Noeta MCP server over stdio (Model Context Protocol). Started by an AI agent client
    /// (e.g. the VS Code extension or `claude mcp add`); speaks JSON-RPC on stdin/stdout. Exposes
    /// the compiler as agent tools — `check` first, then docs/examples, semantic queries, and more.
    Mcp,
    /// Profile a program and report where it spends its time. Runs the file under the production VM
    /// tier-0 (JIT unarmed, so every frame is observable) and prints a profile to stderr; the
    /// program's own stdout is forwarded verbatim.
    Profile {
        /// Path to a `.noe` file.
        file: PathBuf,
        /// Run the **instrumenting** profiler instead of sampling: exact per-function call counts +
        /// self/total time (a table on stderr). Takes precedence over the sampling flags.
        #[arg(long)]
        instrument: bool,
        /// Sampling rate in Hz for the wall-time flamegraph (default 1000). Ignored with
        /// `--instrument` or `--every`.
        #[arg(long)]
        hz: Option<u32>,
        /// Deterministic sampling: take one sample every N executed ops instead of on a wall clock —
        /// a reproducible, op-weighted flamegraph (for stable diffs / tests). Ignored with
        /// `--instrument`.
        #[arg(long, value_name = "N")]
        every: Option<u64>,
        /// Output format. Sampling: `folded` (default), `svg` (flamegraph), `speedscope` (JSON for
        /// speedscope.app). Instrumenting: `table` (default), `json`.
        #[arg(long, value_name = "FMT")]
        format: Option<String>,
        /// Write the profile artifact to this file instead of stderr (recommended for `svg` /
        /// `speedscope`), or to stdout with `-o -` (for piping; the artifact follows the
        /// program's own forwarded output). Without `-o` the program's stdout is never touched.
        #[arg(long, short)]
        out: Option<PathBuf>,
        /// Attribute each flamegraph leaf to its **source line** (`fn:line`), not just the function —
        /// so the hot *line* within a function is visible. Sampling only.
        #[arg(long)]
        lines: bool,
    },
    // (`Serve` was a variant here until higher-order-abi H6 — `noeta serve` is now an
    // extension-contributed command, `noeta-stdlib/src/serve.rs::SERVE_COMMAND`, wired
    // dynamically in `main`.)
    /// Inspect or clear the transparent startup cache (M3). `noeta run`/`dump`/`build` cache each
    /// compile under `~/.cache/noeta/` so a repeated run of unchanged sources skips the front-end;
    /// caching is on by default (disable per-run with `--no-cache`, or everywhere with
    /// `NOETA_NO_CACHE`).
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Format `.noe` source into the canonical style. By default rewrites each file in place
    /// (atomically); a directory argument formats every `.noe` beneath it. Style is read from the
    /// nearest `noeta.toml` `[fmt]` table (or built-in defaults). Files that do not parse are left
    /// untouched and reported; a formatted file is guaranteed to preserve the program's meaning.
    Fmt {
        /// Files or directories to format in place. Omit when using `--stdin`.
        files: Vec<PathBuf>,
        /// Do not write. List each file that is not already canonically formatted and exit non-zero
        /// if any exist (for CI).
        #[arg(long)]
        check: bool,
        /// Read source from stdin and write the formatted result to stdout (editor "format on save").
        #[arg(long)]
        stdin: bool,
        /// Override the `[fmt] parens` policy for control-flow headers: `remove` (strip) or `add`
        /// (wrap `if (x) {`). Defaults to the manifest value, then `remove`.
        #[arg(long, value_name = "remove|add")]
        parens: Option<noeta_fmt::ParenStyle>,
        /// Override the `[fmt] semicolons` policy: `remove` (strip redundant), `add` (terminate every
        /// simple statement), or `preserve` (keep as written). Defaults to the manifest, then `remove`.
        #[arg(long, value_name = "remove|add|preserve")]
        semicolons: Option<noeta_fmt::SemicolonStyle>,
    },
    /// Add a dependency to the nearest `noeta.toml` and refresh `noeta.lock` (package-manager P2.4).
    /// Exactly one source must be given: `--path`, `--git` (with `--tag`), or `--version` (registry).
    /// The manifest edit preserves your formatting and comments; the dependency is then resolved so
    /// the lockfile is updated (a git source is fetched now, so a typo'd URL/tag fails fast).
    Add {
        /// The import root you will `use` the dependency under (an identifier): `use <key>.…`.
        key: String,
        /// A local path dependency (relative to the manifest).
        #[arg(long)]
        path: Option<PathBuf>,
        /// A git repository URL (requires `--tag`).
        #[arg(long)]
        git: Option<String>,
        /// The git tag (a released version) to pin — used with `--git`.
        #[arg(long)]
        tag: Option<String>,
        /// A registry SemVer requirement, e.g. `^1.2` (registry resolution lands in P2.5).
        #[arg(long)]
        version: Option<String>,
    },
    /// Re-resolve dependencies and rewrite `noeta.lock` (package-manager P2.4): a git tag is
    /// re-fetched at the remote and re-pinned to its current commit SHA, so `update` picks up a
    /// moved tag that a locked build would otherwise reproduce from the old commit.
    Update,
    /// Publish this package to the registry index (package-manager P2.5). Records this package's
    /// `[package]` name + version → its git coordinates so others can depend on it by version. This
    /// is the **client stub**: it writes to the local/offline index (`NOETA_REGISTRY_DIR`); the
    /// hosted registry service is built and operated separately.
    Publish {
        /// The git repository URL the release's source lives at.
        #[arg(long)]
        git: String,
        /// The git tag for this release (defaults to `v<version>`).
        #[arg(long)]
        tag: Option<String>,
        /// Force **key-based** signing even when an ambient OIDC identity (CI) is present.
        /// Default: keyless (Sigstore) when the environment carries an identity, else the key
        /// file, else unsigned.
        #[arg(long, conflicts_with = "interactive")]
        key: bool,
        /// Sign keyless via an **interactive browser login** (Sigstore's OAuth: GitHub, Google,
        /// or Microsoft — the certified identity is your email). For publishing from a laptop,
        /// where no ambient CI identity exists.
        #[arg(long)]
        interactive: bool,
        /// With `--interactive`: skip opening a browser — print the sign-in URL and prompt for
        /// the code instead (SSH sessions, containers).
        #[arg(long, requires = "interactive")]
        oob: bool,
        /// Skip generating and uploading the release's documentation artifact (`docs.json`). By
        /// default docs ride along with the release; a docs failure never blocks the publish
        /// (it degrades to a warning).
        #[arg(long)]
        no_docs: bool,
    },
    /// Report the dependency tree's **trust footprint** (package-manager Phase 4): every resolved
    /// dependency, its source, and which ones run **native code** or contribute **CLI commands** —
    /// the elevated authority your `[trust]` grants make active. Informed-consent transparency:
    /// answer "what am I actually running?" before you build.
    Audit {
        /// A file or directory in the project to audit (default: the current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Manage the Ed25519 signing key used to attest published packages (package-manager Phase 4).
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
}

#[derive(Subcommand)]
enum KeyAction {
    /// Generate a fresh signing keypair. Writes the **private** key to a file (keep it secret) and
    /// prints the **public** key to register with your registry scope (`noeta publish` signs with the
    /// private key; consumers verify with the public one).
    New {
        /// Where to write the private key (default: `noeta-signing.key` in the current directory).
        #[arg(long, default_value = "noeta-signing.key")]
        out: PathBuf,
    },
}

#[derive(Subcommand)]
enum CacheAction {
    /// Print the cache directory path (whether or not it exists yet).
    Path,
    /// Show the cache location, entry count, total size on disk, and the size cap.
    Info,
    /// Remove all cached compilations.
    Clear,
}

/// The whole toolchain as a library entry (package-manager Phase 3, N3.0): the stock binary calls
/// this with no extras; a **composed** binary (an app whose dependency graph carries native
/// extension crates) calls it with the extra units, which register alongside the std units before
/// anything can look them up. Everything else — every verb, the LSP, the DAP, extension
/// commands — runs identically in both, which is the point: the composed artifact IS the
/// toolchain, not a runner.
pub fn run_cli(
    extra: &'static [&'static (dyn noeta_stdlib::Extension + Sync)],
    trusted_command_roots: &[&str],
) -> ExitCode {
    noeta_stdlib::registry::install_with_extras(extra);
    // Phase 4: a dependency's `ExtCommand`s reach the CLI only if the root app command-trusts its
    // package (`[trust].commands`, whose namespace roots the composer baked in here). std's own
    // commands (root `"std"`) are always available. The stock binary passes an empty list — it has
    // only std units, so nothing is filtered.
    let command_trusted = |root: &str| root == "std" || trusted_command_roots.contains(&root);
    let trusted_commands: Vec<_> = noeta_stdlib::registry::extensions()
        .iter()
        .filter(|ext| command_trusted(ext.root()))
        .flat_map(|ext| ext.commands().iter())
        .collect();
    // P-AOT L2: if this executable is a `noeta build --exe` artifact (a bundle stapled onto a copy
    // of the runtime), run the embedded program directly — the shipped app is not the toolchain, so
    // its CLI verbs are irrelevant. A plain `noeta` binary has no trailer and falls through to the
    // normal CLI. Detection reads only the tail of the file, not the whole binary.
    if let Some(code) = try_run_stapled() {
        return code;
    }
    // Extension-contributed subcommands (higher-order-abi H6): augment the derive-built CLI with
    // each registered command (so `noeta --help` lists them and each gets real clap parsing),
    // then dispatch a matched name to its extension `run` — the in-process `cargo clippy` model.
    let mut cli = <Cli as clap::CommandFactory>::command();
    for ext in &trusted_commands {
        cli = cli.subcommand(ext_command_clap(ext));
    }
    // An unknown subcommand may be a command contributed by a *native dependency* — visible only
    // inside the app's composed toolchain (Phase 3). Before rendering clap's error, try composing
    // from the current directory's manifest; if the app has no native deps (or we already are the
    // composed binary), fall through to the ordinary error.
    let matches = match cli.try_get_matches() {
        Ok(matches) => matches,
        Err(err) => {
            if err.kind() == clap::error::ErrorKind::InvalidSubcommand {
                // A command contributed by a *native dependency* exists only inside the app's
                // composed toolchain — compose from the cwd manifest first.
                if let Some(code) = compose::maybe_delegate_cwd() {
                    return code;
                }
                // Then a **declared tier** (tier-providers T4): `noeta <tier> <file>` where the
                // file's linked program declares `<tier>` with `@tier` dispatches to that tier's
                // runner in-process.
                if let Some(code) = try_tier_dispatch(&err) {
                    return code;
                }
                // Then the external-binary form (Phase 3, N3.7 — the `cargo-<name>` model): an
                // executable `noeta-<cmd>` on PATH serves `noeta <cmd> …`. Registered
                // (compiled-in) commands never reach here — clap knows them.
                if let Some(code) = external_command_fallback(&err) {
                    return code;
                }
            }
            err.exit()
        }
    };
    if let Some((name, sub)) = matches.subcommand()
        && let Some(ext) = trusted_commands.iter().find(|c| c.name == name)
    {
        return ext_command_dispatch(ext, sub);
    }
    let cli =
        <Cli as clap::FromArgMatches>::from_arg_matches(&matches).unwrap_or_else(|err| err.exit());
    match cli.command {
        Command::Run {
            file,
            tier,
            target,
            no_cache,
            jit_stats,
            args,
        } => cmd_run(&file, &tier, &target, no_cache, jit_stats, &args),
        Command::Test {
            file,
            fail_fast,
            jobs,
            group,
            names,
            json,
            target,
        } => cmd_test(&file, fail_fast, jobs, &group, &names, json, &target),
        Command::Bench {
            file,
            iterations,
            names,
            json,
            save_baseline,
            baseline,
            max_regress,
            target,
        } => cmd_bench(
            &file,
            iterations,
            &names,
            json,
            &save_baseline,
            &baseline,
            max_regress,
            &target,
        ),
        Command::Doc {
            file,
            package,
            out,
            target,
        } => cmd_doc(&file, &package, &out, &target),
        Command::Build {
            file,
            out,
            exe,
            native,
            tier,
            target,
        } => cmd_build(&file, out.as_deref(), exe, native, &tier, &target),
        Command::Dump { file, tier, target } => cmd_dump(&file, &tier, &target),
        Command::Check {
            path,
            tier,
            target,
            format,
        } => cmd_check(&path, &tier, &target, format),
        Command::Repl { no_check, load } => cmd_repl(!no_check, load),
        Command::Lsp => cmd_lsp(),
        Command::Dap => cmd_dap(),
        Command::Mcp => cmd_mcp(),
        Command::Profile {
            file,
            instrument,
            hz,
            every,
            format,
            out,
            lines,
        } => cmd_profile(&file, instrument, hz, every, format.as_deref(), out, lines),
        Command::Cache { action } => cmd_cache(&action),
        Command::Fmt {
            files,
            check,
            stdin,
            parens,
            semicolons,
        } => cmd_fmt(&files, check, stdin, parens, semicolons),
        Command::Add {
            key,
            path,
            git,
            tag,
            version,
        } => cmd_add(
            &key,
            path.as_deref(),
            git.as_deref(),
            tag.as_deref(),
            version.as_deref(),
        ),
        Command::Update => cmd_update(),
        Command::Publish {
            git,
            tag,
            key,
            interactive,
            oob,
            no_docs,
        } => cmd_publish(&git, tag.as_deref(), key, interactive, oob, no_docs),
        Command::Audit { path } => cmd_audit(&path),
        Command::Key { action } => cmd_key(&action),
    }
}

/// `noeta add <key> --path/--git+--tag/--version` — add a dependency to the nearest `noeta.toml`,
/// then resolve so `noeta.lock` reflects it (package-manager P2.4d).
fn cmd_add(
    key: &str,
    path: Option<&std::path::Path>,
    git: Option<&str>,
    tag: Option<&str>,
    version: Option<&str>,
) -> ExitCode {
    // Exactly one source form.
    let value_toml = match (path, git, version) {
        (Some(p), None, None) => format!("{{ path = {} }}", toml_string(&p.display().to_string())),
        (None, Some(url), None) => {
            let Some(tag) = tag else {
                eprintln!(
                    "lang: `--git` requires `--tag` (sources are git + tagged releases only)"
                );
                return ExitCode::from(2);
            };
            format!(
                "{{ git = {}, tag = {} }}",
                toml_string(url),
                toml_string(tag)
            )
        }
        (None, None, Some(req)) => toml_string(req),
        (None, None, None) => {
            eprintln!("lang: give a source — `--path`, `--git` (+ `--tag`), or `--version`");
            return ExitCode::from(2);
        }
        _ => {
            eprintln!("lang: give exactly one source — `--path`, `--git`, or `--version`");
            return ExitCode::from(2);
        }
    };
    if git.is_none() && tag.is_some() {
        eprintln!("lang: `--tag` only applies to a `--git` dependency");
        return ExitCode::from(2);
    }

    let manifest_path = match locate_manifest() {
        Ok(p) => p,
        Err(code) => return code,
    };
    if let Err(err) = manifest::add_dependency(&manifest_path, key, &value_toml) {
        eprintln!("lang: {err}");
        return ExitCode::from(1);
    }
    // Resolve so the new dependency is fetched and the lock is refreshed; a bad URL/tag/path fails
    // here (the manifest edit already succeeded — the entry stays so the user can fix it).
    match graph::resolve_graph(&manifest_path) {
        Ok(_) => {
            println!("added `{key}` to {}", manifest_path.display());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("lang: added `{key}`, but resolving it failed: {err}");
            ExitCode::from(1)
        }
    }
}

/// `noeta update` — discard the current pins and re-resolve, rewriting `noeta.lock` (P2.4d). Removing
/// the lock forces the graph walk to re-`ls-remote` each git tag and re-pin its current commit SHA.
fn cmd_update() -> ExitCode {
    let manifest_path = match locate_manifest() {
        Ok(p) => p,
        Err(code) => return code,
    };
    let dir = manifest_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let _ = std::fs::remove_file(dir.join(lock::LOCK_NAME));
    match graph::resolve_graph(&manifest_path) {
        Ok(graph) => {
            if graph.locked.is_empty() {
                println!("no dependencies to update");
            } else {
                println!(
                    "updated {} ({} package(s))",
                    lock::LOCK_NAME,
                    graph.locked.len()
                );
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("lang: {err}");
            ExitCode::from(1)
        }
    }
}

/// `noeta publish --git <url> [--tag <tag>]` — record this package's identity + version → git
/// coordinates in the registry index (package-manager P2.5, client stub). The tag defaults to
/// `v<version>`. Writes to the local/offline index; the hosted registry is operated separately.
fn cmd_publish(
    git: &str,
    tag: Option<&str>,
    force_key: bool,
    interactive: bool,
    oob: bool,
    no_docs: bool,
) -> ExitCode {
    let manifest_path = match locate_manifest() {
        Ok(p) => p,
        Err(code) => return code,
    };
    let manifest = match manifest::load(&manifest_path) {
        Ok(manifest) => manifest,
        Err(err) => {
            eprintln!("lang: {err}");
            return ExitCode::from(1);
        }
    };
    let Some(pkg) = manifest.package() else {
        eprintln!(
            "lang: `{}` has no `[package]` table — only a package (with a name + version) can be \
             published",
            manifest_path.display()
        );
        return ExitCode::from(1);
    };
    let name = format!("{}/{}", pkg.name.company, pkg.name.package);
    let version = pkg.version.clone();

    // A published package must depend **only via the registry** (Phase 4, follow-up #3): a path
    // dependency can't travel to a consumer, and a git dependency isn't expressible in the index's
    // (identity, req) shape — so a consumer resolving this release from the index would silently miss
    // it and fail to build. Reject up front (before touching git), naming the offending dependency.
    let mut deps: Vec<registry::Dep> = Vec::new();
    for (key, dep) in manifest.dependencies() {
        match dep {
            manifest::Dependency::Registry {
                package: Some(pkg),
                req,
            } => deps.push(registry::Dep {
                package: format!("{}/{}", pkg.company, pkg.package),
                req: req.clone(),
            }),
            manifest::Dependency::Registry { package: None, .. } => {
                eprintln!(
                    "lang: dependency `{key}` is a registry dependency but names no `package = \
                     \"company/pkg\"` — a published package's dependencies must each name their \
                     registry identity."
                );
                return ExitCode::from(1);
            }
            manifest::Dependency::Path { .. } | manifest::Dependency::Git { .. } => {
                eprintln!(
                    "lang: dependency `{key}` is a path/git dependency — a published package must \
                     depend only via the registry (`{key} = {{ version = \"…\", package = \
                     \"company/pkg\" }}`), so consumers can resolve it. Publish aborted."
                );
                return ExitCode::from(1);
            }
        }
    }

    let tag = tag
        .map(str::to_string)
        .unwrap_or_else(|| format!("v{version}"));
    let index = match registry::open_default() {
        Ok(index) => index,
        Err(err) => {
            eprintln!("lang: {err}");
            return ExitCode::from(1);
        }
    };
    // Pin the commit SHA at publish time (Phase 4, S2): the index — not just a consumer's lockfile —
    // records "this version = this commit", so a first registry resolve fetches the exact commit and
    // a later tag move is caught. A tag that doesn't resolve is a publish error (nothing to pin).
    let sha = match noeta_pm::resolve_tag_sha(git, &tag) {
        Ok(sha) => sha,
        Err(err) => {
            eprintln!("lang: cannot resolve `{git}`@`{tag}` to a commit to pin: {err}");
            return ExitCode::from(1);
        }
    };
    let coords = registry::GitCoords {
        url: git.to_string(),
        tag: tag.clone(),
        sha: sha.clone(),
    };
    // Attest the release (Phase 4 #2 / Phase 5) under one of two trust roots:
    //  - **keyless** (preferred when available): an ambient OIDC identity (CI) signs via
    //    Sigstore — ephemeral key, Fulcio cert, transparency log; nothing to steal afterwards.
    //  - **key**: the Ed25519 file from NOETA_SIGNING_KEY / `noeta-signing.key` (`--key` forces
    //    this even in CI).
    // Neither available → publish *unsigned* (a warning — the release resolves, unverified).
    let attestation = noeta_pm::provenance::Attestation {
        name: &name,
        version: &version,
        sha: &sha,
    };
    let ambient = if force_key {
        None
    } else if interactive {
        // Interactive browser login (K6): Sigstore's OAuth signs you in with GitHub/Google/
        // Microsoft and the certified identity is the account email. `--oob` prints the URL
        // and prompts for the code instead of opening a browser.
        match noeta_pm::keyless::interactive_identity(oob) {
            Ok(identity) => Some(identity),
            Err(err) => {
                eprintln!("lang: {err}");
                return ExitCode::from(1);
            }
        }
    } else {
        match noeta_pm::keyless::ambient_identity() {
            Ok(identity) => identity,
            Err(err) => {
                eprintln!("lang: {err}");
                return ExitCode::from(1);
            }
        }
    };
    let (signature, bundle, provenance_tag) = if let Some(identity) = ambient {
        let who = identity.identity().to_string();
        let statement = noeta_pm::keyless::publish_statement(&attestation, &coords);
        let bundle = match noeta_pm::keyless::publish_bundle(statement.as_bytes(), identity) {
            Ok(bundle) => bundle,
            Err(err) => {
                eprintln!("lang: {err}");
                return ExitCode::from(1);
            }
        };
        // Verify our own bundle before uploading it: a publisher should never ship provenance
        // consumers will reject (a broken CA response, a log that didn't include us, …).
        let digest = noeta_pm::keyless::attested_digest(&attestation);
        if let Err(err) = noeta_pm::keyless::verify_bundle(&bundle, &digest, None) {
            eprintln!("lang: the freshly signed bundle does not verify — not publishing: {err}");
            return ExitCode::from(1);
        }
        (None, Some(bundle), format!("keyless: {who}"))
    } else {
        match provenance_sign(&name, &version, &sha, index.as_ref()) {
            Ok(Some(sig)) => (Some(sig), None, "signed".to_string()),
            Ok(None) => (None, None, "UNSIGNED".to_string()),
            Err(err) => {
                eprintln!("lang: {err}");
                return ExitCode::from(1);
            }
        }
    };
    let release = registry::Release {
        version: version.clone(),
        coords,
        deps,
        signature,
        bundle,
    };
    match index.publish(&name, &release) {
        Ok(()) => {
            println!("published `{name}` {version} → {git}#{tag} ({sha}) [{provenance_tag}]");
            // Docs ride along (docs-ingestion follow-up): generate the artifact from the package
            // dir and store it with the release. Advisory metadata — a docs failure warns, never
            // blocks a publish that already succeeded.
            if !no_docs {
                let pkg_dir = manifest_path
                    .parent()
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."));
                match docgen::package_docs_json(&pkg_dir) {
                    Ok((docs_json, done)) => match index.put_docs(&name, &version, &docs_json) {
                        Ok(()) => println!(
                            "docs uploaded ({} module{}, {} declaration{})",
                            done.modules,
                            plural(done.modules),
                            done.decls,
                            plural(done.decls),
                        ),
                        Err(err) => eprintln!("lang: warning: docs not uploaded: {err}"),
                    },
                    Err(err) => eprintln!("lang: warning: docs not generated: {err}"),
                }
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("lang: {err}");
            ExitCode::from(1)
        }
    }
}

/// `noeta audit [path]` — report the dependency tree's trust footprint (package-manager Phase 4, S6):
/// every resolved dependency, its source, and the elevated authority (`native` / `commands`) the root
/// `[trust]` grants make active. Transparency/informed-consent: since an *unauthorized* native
/// dependency fails resolution, a successful audit lists exactly the elevated authority that is live.
fn cmd_audit(path: &std::path::Path) -> ExitCode {
    let start = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf()
    };
    let Some(manifest_path) = manifest::find(&start) else {
        eprintln!(
            "lang: no `{}` found at or above `{}`",
            manifest::MANIFEST_NAME,
            start.display()
        );
        return ExitCode::from(1);
    };
    let manifest_dir = manifest_path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();
    // A synthetic entry so `resolve_graph` discovers the manifest from its directory.
    let graph = match graph::resolve_graph(&manifest_dir.join("_.noe")) {
        Ok(graph) => graph,
        Err(err) => {
            eprintln!("lang: {err}");
            return ExitCode::from(1);
        }
    };
    let manifest = match manifest::load(&manifest_path) {
        Ok(manifest) => manifest,
        Err(err) => {
            eprintln!("lang: {err}");
            return ExitCode::from(1);
        }
    };
    let trust = manifest.trust();

    println!("Trust audit — {}\n", manifest_path.display());
    if graph.locked.is_empty() {
        println!("  No dependencies.");
        return ExitCode::SUCCESS;
    }

    let mut deps = graph.locked.clone();
    deps.sort_by(|a, b| a.identity.cmp(&b.identity));
    let mut native_count = 0usize;
    let mut command_count = 0usize;
    println!("  Dependencies ({}):", deps.len());
    for pkg in &deps {
        let source = match &pkg.source {
            noeta_pm::graph::ResolvedSource::Path { path } => format!("path {}", path.display()),
            noeta_pm::graph::ResolvedSource::Git { url, tag, sha } => {
                format!("git {url}#{tag} ({})", &sha[..sha.len().min(9)])
            }
        };
        let mut flags = Vec::new();
        if pkg.native.is_some() {
            native_count += 1;
            flags.push("native");
        }
        if trust.commands.contains(&pkg.identity) {
            command_count += 1;
            flags.push("commands");
        }
        let tail = if flags.is_empty() {
            String::new()
        } else {
            format!("  ⚠ {}", flags.join(" + "))
        };
        println!("    {} {}  [{}]{}", pkg.identity, pkg.version, source, tail);
    }

    println!("\n  Elevated authority (granted in [trust]):");
    println!("    native   : {}", render_trust_list(&trust.native));
    println!("    commands : {}", render_trust_list(&trust.commands));
    println!(
        "\n  {native_count} package(s) run native code, {command_count} may add CLI commands — all \
         authorized (an unauthorized native dependency would have failed resolution)."
    );

    // Provenance (Phase 4 #2 / Phase 5): each scope's pinned trust root. Resolution *enforces*
    // verification (a bad signature/bundle, a changed key/identity, or a downgraded root fails the
    // resolve), so a successful audit means every signed release verified against its pinned root.
    println!("\n  Provenance (pinned trust roots):");
    if graph.scope_trust.is_empty() {
        println!("    (none — no registry dependency carried provenance)");
    } else {
        for (scope, trust) in &graph.scope_trust {
            match trust {
                noeta_pm::lock::ScopeTrust::Key(key) => {
                    println!("    {scope}: key {}…", &key[..key.len().min(16)]);
                }
                noeta_pm::lock::ScopeTrust::Keyless { issuer, identity } => {
                    println!("    {scope}: keyless {identity} (via {issuer})");
                }
            }
        }
        println!(
            "    releases from these scopes verified during resolution; a changed key or identity, \
             a downgraded trust root, or a bad signature/bundle aborts the build."
        );
    }
    ExitCode::SUCCESS
}

/// Render a `[trust]` identity set for the audit report (`(none)` when empty).
fn render_trust_list(set: &std::collections::BTreeSet<String>) -> String {
    if set.is_empty() {
        "(none)".to_string()
    } else {
        set.iter().cloned().collect::<Vec<_>>().join(", ")
    }
}

/// Sign a release attestation for `noeta publish` (Phase 4, #2), returning the hex signature — or
/// `None` (with a warning) when no signing key is configured, so publishing stays possible while
/// provenance is adopted gradually. Reads the private key from `NOETA_SIGNING_KEY` (a path) or
/// `noeta-signing.key`. When it signs, it also registers the scope's public key with the index (a
/// no-op for the hosted registry, which registers keys via its admin endpoint).
fn provenance_sign(
    name: &str,
    version: &semver::Version,
    sha: &str,
    index: &dyn registry::Index,
) -> Result<Option<String>, String> {
    let key_path = std::env::var_os("NOETA_SIGNING_KEY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("noeta-signing.key"));
    if !key_path.is_file() {
        eprintln!(
            "lang: no signing key at `{}` — publishing UNSIGNED (consumers can't verify provenance). \
             Sign keyless with `noeta publish --interactive` (browser login), or run \
             `noeta key new` and set NOETA_SIGNING_KEY.",
            key_path.display()
        );
        return Ok(None);
    }
    let private_hex = std::fs::read_to_string(&key_path)
        .map_err(|err| format!("cannot read signing key `{}`: {err}", key_path.display()))?
        .trim()
        .to_string();

    let attestation = noeta_pm::provenance::Attestation { name, version, sha };
    let signature = noeta_pm::provenance::sign(&attestation, &private_hex)?;
    // Register this scope's public key so a consumer can verify (local index writes it; the hosted
    // registry no-ops — it registers keys via admin, enforcing scope ownership).
    let scope = name.split('/').next().unwrap_or(name);
    let public_hex = noeta_pm::provenance::public_key_hex(&private_hex)?;
    index.set_scope_key(scope, &public_hex)?;
    Ok(Some(signature))
}

/// `noeta key new` — generate an Ed25519 signing keypair (package-manager Phase 4, #2). The private
/// key is written to a file (kept secret); the public key is printed to register with the registry
/// scope. `noeta publish` signs releases with the private key; consumers verify with the public one.
fn cmd_key(action: &KeyAction) -> ExitCode {
    let KeyAction::New { out } = action;
    let keypair = match noeta_pm::provenance::generate_keypair() {
        Ok(keypair) => keypair,
        Err(err) => {
            eprintln!("lang: {err}");
            return ExitCode::from(1);
        }
    };
    if let Err(err) = write_private_key(out, &keypair.private_hex) {
        eprintln!("lang: cannot write `{}`: {err}", out.display());
        return ExitCode::from(1);
    }
    println!(
        "wrote private signing key to {} (keep it secret)",
        out.display()
    );
    println!("public key — register this with your registry scope:");
    println!("  {}", keypair.public_hex);
    println!(
        "`noeta publish` reads the private key from NOETA_SIGNING_KEY (a path) or `{}`.",
        out.display()
    );
    ExitCode::SUCCESS
}

/// Write a private key to `path`, `0600` on unix (a signing key must not be world-readable).
fn write_private_key(path: &std::path::Path, private_hex: &str) -> std::io::Result<()> {
    std::fs::write(path, format!("{private_hex}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Find the nearest `noeta.toml` from the current directory, or print a diagnostic and return a
/// non-zero exit code (shared by `noeta add`/`update`).
fn locate_manifest() -> Result<PathBuf, ExitCode> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    manifest::find(&cwd).ok_or_else(|| {
        eprintln!(
            "lang: no `{}` found at or above `{}`",
            manifest::MANIFEST_NAME,
            cwd.display()
        );
        ExitCode::from(1)
    })
}

/// Quote a string as a TOML basic string for a manifest value we write (`noeta add`).
fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `noeta cache <path|info|clear>` — inspect or clear the transparent startup cache.
fn cmd_cache(action: &CacheAction) -> ExitCode {
    let Some(dir) = noeta_cache::Cache::locate() else {
        eprintln!("lang: no cache directory could be resolved (set HOME or NOETA_CACHE_DIR)");
        return ExitCode::from(1);
    };
    match action {
        CacheAction::Path => {
            println!("{}", dir.display());
            ExitCode::SUCCESS
        }
        CacheAction::Info => {
            let cap = noeta_cache::max_bytes();
            let cap_str = if cap == 0 {
                "unbounded".to_string()
            } else {
                human_bytes(cap)
            };
            if !dir.exists() {
                println!("{}\n0 entries, 0 B on disk (cap {cap_str})", dir.display());
                return ExitCode::SUCCESS;
            }
            match noeta_cache::Cache::open_at(dir.clone()).and_then(|c| c.stats()) {
                Ok((count, bytes)) => {
                    println!("{}", dir.display());
                    println!(
                        "{count} {}, {} on disk (cap {cap_str})",
                        if count == 1 { "entry" } else { "entries" },
                        human_bytes(bytes),
                    );
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("lang: cannot read cache at {}: {err}", dir.display());
                    ExitCode::from(1)
                }
            }
        }
        CacheAction::Clear => {
            if !dir.exists() {
                println!("cache is already empty ({})", dir.display());
                return ExitCode::SUCCESS;
            }
            match noeta_cache::Cache::open_at(dir.clone()).and_then(|c| c.clear()) {
                Ok(n) => {
                    println!(
                        "removed {n} cached compilation{} from {}",
                        if n == 1 { "" } else { "s" },
                        dir.display()
                    );
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("lang: cannot clear cache at {}: {err}", dir.display());
                    ExitCode::from(1)
                }
            }
        }
    }
}

/// Format a byte count as a short human-readable string (B / KiB / MiB / GiB).
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Start the Noeta language server over stdio, blocking until the editor client disconnects.
fn cmd_lsp() -> ExitCode {
    noeta_lsp::run_stdio();
    ExitCode::SUCCESS
}

/// Start the Noeta debug adapter over stdio, blocking until the editor client disconnects.
fn cmd_dap() -> ExitCode {
    noeta_dap::run_stdio();
    ExitCode::SUCCESS
}

/// Start the Noeta MCP server over stdio, blocking until the agent client disconnects.
fn cmd_mcp() -> ExitCode {
    noeta_mcp::run_stdio();
    ExitCode::SUCCESS
}

/// Profile a program: run it tier-0 under the production VM and report where it spends its time.
/// Sampling (wall-time flamegraph) is the default; `--instrument` selects the exact per-function
/// profiler; `--every N` makes sampling deterministic (op-weighted).
#[allow(clippy::too_many_arguments)]
fn cmd_profile(
    file: &std::path::Path,
    instrument: bool,
    hz: Option<u32>,
    every: Option<u64>,
    format: Option<&str>,
    out: Option<PathBuf>,
    lines: bool,
) -> ExitCode {
    let mode = if instrument {
        noeta_prof::Mode::Instrument
    } else if let Some(every) = every {
        noeta_prof::Mode::Sample {
            clock: noeta_prof::SampleClock::Ops { every },
            lines,
        }
    } else {
        noeta_prof::Mode::Sample {
            clock: noeta_prof::SampleClock::Wall {
                hz: hz.unwrap_or(1000),
            },
            lines,
        }
    };
    let format = match format {
        Some(s) => match noeta_prof::Format::parse(s) {
            Some(f) => Some(f),
            None => {
                eprintln!(
                    "lang: unknown --format '{s}' (expected folded, svg, speedscope, table, or json)"
                );
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    noeta_prof::run(file, mode, format, out)
}

/// `noeta fmt` — format `.noe` source into the canonical style. Rewrites the given files (or every
/// `.noe` under the given directories) in place by default; `--check` writes nothing and lists any
/// file that is not already formatted (exit 1, for CI); `--stdin` reads stdin and writes the result
/// to stdout. Style comes from the nearest `noeta.toml` `[fmt]` table. Files that do not parse are
/// left untouched and reported; the safety gate inside `noeta_fmt::format_source` guarantees a
/// written file never changes meaning. In-place writes are atomic (temp file + rename) and skipped
/// when the content is already canonical.
/// Apply any `--parens` / `--semicolons` CLI overrides on top of a manifest-resolved `FmtConfig`.
fn apply_fmt_overrides(
    mut config: noeta_fmt::FmtConfig,
    parens: Option<noeta_fmt::ParenStyle>,
    semicolons: Option<noeta_fmt::SemicolonStyle>,
) -> noeta_fmt::FmtConfig {
    if let Some(p) = parens {
        config.parens = p;
    }
    if let Some(s) = semicolons {
        config.semicolons = s;
    }
    config
}

fn cmd_fmt(
    paths: &[PathBuf],
    check: bool,
    stdin: bool,
    parens: Option<noeta_fmt::ParenStyle>,
    semicolons: Option<noeta_fmt::SemicolonStyle>,
) -> ExitCode {
    if stdin {
        return cmd_fmt_stdin(parens, semicolons);
    }
    if paths.is_empty() {
        eprintln!("noeta fmt: no files given (use `--stdin` to format standard input)");
        return ExitCode::FAILURE;
    }

    // Expand any directory argument into the `.noe` files beneath it.
    let mut files = Vec::new();
    for path in paths {
        if path.is_dir() {
            collect_noe_files(path, &mut files);
        } else {
            files.push(path.clone());
        }
    }

    let mut failed = false; // a parse or IO error on any file
    let mut would_change = false; // `--check`: some file is not already formatted

    for file in &files {
        let dir = file.parent().unwrap_or_else(|| std::path::Path::new("."));
        let config = match manifest::resolve_fmt_config(dir) {
            Ok(config) => apply_fmt_overrides(config, parens, semicolons),
            Err(err) => {
                eprintln!("noeta fmt: {err}");
                return ExitCode::FAILURE;
            }
        };
        let original = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(err) => {
                eprintln!("noeta fmt: cannot read `{}`: {err}", file.display());
                failed = true;
                continue;
            }
        };

        match noeta_fmt::format_source(&file.to_string_lossy(), &original, &config) {
            Ok(formatted) => {
                if formatted == original {
                    continue; // already canonical — no write, no churn
                }
                if check {
                    would_change = true;
                    println!("{}", file.display());
                } else if let Err(err) = atomic_write(file, &formatted) {
                    eprintln!("noeta fmt: cannot write `{}`: {err}", file.display());
                    failed = true;
                }
            }
            Err(err) => {
                report_fmt_error(&file.display().to_string(), &err);
                failed = true;
            }
        }
    }

    if failed || (check && would_change) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Recursively collect every `.noe` file under `dir` (skipping dot-directories like `.git`).
fn collect_noe_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        eprintln!("noeta fmt: cannot read directory `{}`", dir.display());
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let hidden = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'));
            if !hidden {
                collect_noe_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "noe") {
            out.push(path);
        }
    }
}

/// Write `contents` to `path` atomically: write a sibling temp file, then rename over the target, so
/// a crash mid-write never leaves a truncated source file.
fn atomic_write(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("out");
    let tmp = dir.join(format!(".{name}.fmt-tmp"));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

/// Format stdin → stdout with the config discovered from the current directory.
fn cmd_fmt_stdin(
    parens: Option<noeta_fmt::ParenStyle>,
    semicolons: Option<noeta_fmt::SemicolonStyle>,
) -> ExitCode {
    let text = match io::read_to_string(io::stdin()) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("noeta fmt: cannot read stdin: {err}");
            return ExitCode::FAILURE;
        }
    };
    let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config = match manifest::resolve_fmt_config(&dir) {
        Ok(config) => apply_fmt_overrides(config, parens, semicolons),
        Err(err) => {
            eprintln!("noeta fmt: {err}");
            return ExitCode::FAILURE;
        }
    };
    match noeta_fmt::format_source("<stdin>", &text, &config) {
        Ok(formatted) => {
            print!("{formatted}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            report_fmt_error("<stdin>", &err);
            ExitCode::FAILURE
        }
    }
}

/// Print a one-line reason a file could not be formatted (leaving it untouched).
fn report_fmt_error(name: &str, err: &noeta_fmt::FmtError) {
    use noeta_fmt::FmtError;
    match err {
        FmtError::Parse(diags) => {
            eprintln!(
                "{name}: not formatted — source does not parse ({} diagnostic(s))",
                diags.len()
            );
        }
        FmtError::Safety(why) => {
            eprintln!("{name}: not formatted — internal safety check failed: {why}");
        }
    }
}

/// For a tier runner: whether its `tier` is live under `--target`. `Ok(true)` when no target was
/// given (the runner always runs); `Ok(false)` when a target was given but does not make `tier`
/// live (the runner should no-op); `Err` on a target-resolution failure (a fatal error the caller
/// prints).
/// The active tier → provider map for `--target` (empty with no target — default resolution:
/// extension declarations first). The tier-execution layer dispatches on this: `"std"` runs the
/// native built-in runner, a dependency key runs that package's `@tier` runner.
fn resolve_providers(
    entry: &std::path::Path,
    target: &Option<String>,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    match target {
        None => Ok(std::collections::BTreeMap::new()),
        Some(name) => manifest::resolve_active_tier_providers(entry, name),
    }
}

fn tier_active_in_target(
    entry: &std::path::Path,
    target: &Option<String>,
    tier: &str,
) -> Result<bool, String> {
    match target {
        None => Ok(true),
        Some(name) => Ok(manifest::resolve_active_tiers(entry, name)?
            .iter()
            .any(|t| t == tier)),
    }
}

/// Type-check and run a program, writing stdout to the real stdout and rendering any diagnostics to
/// stderr — each against the source its span belongs to (via the `SourceMap`). Returns the process
/// exit code. `program` is the loaded program, possibly after dev-tier activation (`cmd_run`).
fn run_program(program: &noeta_ast::Program, sources: &SourceMap, args: Vec<String>) -> i32 {
    // The loader already lexed + parsed (and reported any lex/parse errors); type-check then run.
    // One `check_all` produces both the gate diagnostics and the `type_of` site map the backend
    // needs, so the checker runs exactly once (it previously ran again inside the backend).
    let checked = noeta_check::check_all(program);
    if !checked.diagnostics.is_empty() {
        emit_diagnostics_mapped(sources, checked.diagnostics.iter());
        return 1;
    }

    match execute_real_host(program, &checked, args) {
        Ok((result, trace)) => {
            print!("{}", result.stdout);
            let _ = io::stdout().flush();
            emit_diagnostics_mapped(sources, result.diagnostics.iter());
            // An abort's stack trace, after the diagnostic it belongs to. Only when there is a call
            // chain to show — a single-frame trace repeats what the diagnostic's span already says.
            if trace.len() >= 2 {
                eprint!("{}", noeta_vm::render_trace(&trace, sources));
            }
            result.exit_code
        }
        Err(err) => {
            eprintln!("lang: {err}");
            1
        }
    }
}

/// Compile an already-typechecked program straight to a bytecode [`Module`] for the real (VM)
/// execution path (isolates I.4a). This runs the *same* Core-IR lowering + precise-RC drop + reuse
/// passes the differential and the eval reference use, then continues IR → bytecode (the extra stage
/// the VM needs); reusing `checked`'s site maps keeps the checker to a single run.
///
/// Every program that parses and type-checks compiles to bytecode — the differential holds the VM at
/// 100% coverage *by construction* (each language feature lands in both backends together). So an
/// `Err` here does not mean "ordinary unsupported user program"; it means that invariant broke, and
/// we surface it rather than silently downgrading to a different backend.
fn compile_real(
    program: &noeta_ast::Program,
    checked: &noeta_check::Checked,
) -> Result<noeta_bytecode::Module, String> {
    noeta_compiler::compile_with_sites(
        program,
        checked.sites.clone(),
        // Real execution runs isolates on OS threads (I.4b): lower `isolate f(args)` to `SpawnIsolate`.
        // The differential/salsa paths pass false (byte-identical cooperative sandbox).
        true,
        // `noeta run` is a production compile — no debug info (the debugger's `noeta dap` compiles
        // the same program with debug = true).
        false,
    )
    .map_err(|u| {
        format!(
            "internal error: the VM cannot compile this program: {}",
            u.reason
        )
    })
}

/// Execute an already-checked program against the **real host** (real `env`/`args`, real-disk IO)
/// on a per-isolate tokio runtime (M2.3), returning its [`RunResult`].
///
/// Real execution runs on the **VM** backend (isolates I.4a). The tree-walker's `Rc`-based values are
/// `!Send`, so it can never carry real inter-isolate parallelism (isolates I.4); the VM's NaN-boxed,
/// thread-local heap is the one the shared-region borrow-sharing (I.3) and the coming `RealScheduler`
/// (I.4b) build on. The conformance differential still runs *both* backends over the deterministic
/// sandbox, so this real-host path is never compared backend-to-backend. Shared by `noeta run` and the
/// `@test` runner so both execute a program identically.
fn execute_real_host(
    program: &noeta_ast::Program,
    checked: &noeta_check::Checked,
    args: Vec<String>,
) -> Result<(noeta_backend::RunResult, Vec<noeta_vm::TraceFrame>), String> {
    let (result, trace, _) = run_module_real_host(
        std::sync::Arc::new(compile_real(program, checked)?),
        args,
        false,
    );
    Ok((result, trace))
}

/// Run an already-compiled [`Module`] against the real host — the shared execution core of
/// [`execute_real_host`] (source path) and the `.noeb` bundle runner (P-AOT L1.2), which loads a
/// module directly with no source to compile.
/// The p2p application namespace for the running program (p2p P3.4): the package name of the
/// nearest `noeta.toml` (stable and unique per project) when in a package, else the entry script's
/// file stem — so two different Noeta apps never share one p2p identity/store dir. `None` ⇒ let the
/// runtime fall back to its own default (the executable's file stem). `args[0]` is the entry path.
fn p2p_app_namespace(args: &[String]) -> Option<String> {
    if let Ok(cwd) = std::env::current_dir()
        && let Some(path) = manifest::find(&cwd)
        && let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(parsed) = manifest::Manifest::parse(&text)
        && let Some(pkg) = parsed.package()
    {
        return Some(format!("{}/{}", pkg.name.company, pkg.name.package));
    }
    args.first()
        .map(std::path::Path::new)
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
}

fn run_module_real_host(
    module: std::sync::Arc<noeta_bytecode::Module>,
    args: Vec<String>,
    jit_report: bool,
) -> (
    noeta_backend::RunResult,
    Vec<noeta_vm::TraceFrame>,
    Option<noeta_vm::JitReport>,
) {
    // A factory that mints a fresh real host + wall-clock executor per isolate (isolates I.4b): the
    // main program gets one, and each real-thread `isolate f(args)` gets its own so a worker's disk /
    // clock / async state is independent. Injected here (not in `noeta-vm`) so the VM crate needs no
    // `noeta-runtime`/tokio dependency. A worker that cannot start its runtime panics the worker thread,
    // which surfaces as an isolate failure at the `.await`. The program argument vector (from
    // `noeta run … -- <args>`) is cloned into every isolate's host so a worker's `args.all()` agrees
    // with the main program's.
    // The p2p application namespace (p2p P3.4): keys each isolate's persistent p2p identity/store
    // dir on *this* program so two different Noeta apps never collide on one dir. Computed once and
    // cloned into every isolate's host.
    let app_id = p2p_app_namespace(&args);
    let factory: noeta_vm::IsolateFactory = std::sync::Arc::new(move || {
        let host: Box<dyn noeta_stdlib::Host> = Box::new(
            noeta_runtime::RealHost::new()
                .expect("cannot start an isolate's runtime")
                .with_args(args.clone())
                .with_p2p_app(app_id.clone()),
        );
        let executor: Box<dyn noeta_stdlib::Executor> = Box::new(
            noeta_runtime::RealExecutor::new().expect("cannot start an isolate's async executor"),
        );
        (host, executor)
    });
    let (host, executor) = factory();
    // Real isolates run on OS threads (out-of-oracle); channel-shipping isolates fall back to
    // cooperative tasks (cross-thread channels are I.4c). The differential keeps the sandbox pair.
    VmBackend::new()
        .run_module_with_host_and_executor_parallel(module, host, executor, factory, jit_report)
}

/// P-AOT L2: detect and run a bundle stapled onto this executable (a `noeta build --exe` artifact),
/// returning its exit code — or `None` when this is the plain toolchain binary (no trailer), so the
/// normal CLI runs. Reads only the trailer + embedded blob, seeking rather than slurping the whole
/// binary, so the toolchain's own startup is not taxed on the common (no-bundle) path. Any IO/format
/// hiccup is treated as "no bundle" (returns `None`): the toolchain must still start normally.
fn try_run_stapled() -> Option<ExitCode> {
    use std::io::{Read, Seek, SeekFrom};

    let exe_path = std::env::current_exe().ok()?;
    let mut file = std::fs::File::open(&exe_path).ok()?;
    let size = file.seek(SeekFrom::End(0)).ok()?;
    let trailer_len = noeta_bundle::TRAILER_LEN as u64;
    if size < trailer_len {
        return None;
    }
    // Read the fixed trailer at the tail; a missing sentinel means this is the plain binary.
    file.seek(SeekFrom::End(-(trailer_len as i64))).ok()?;
    let mut trailer = [0u8; noeta_bundle::TRAILER_LEN];
    file.read_exact(&mut trailer).ok()?;
    let blob_len = noeta_bundle::stapled_len(&trailer)?;
    let blob_start = size.checked_sub(trailer_len + blob_len as u64)?;
    file.seek(SeekFrom::Start(blob_start)).ok()?;
    let mut blob = vec![0u8; blob_len];
    file.read_exact(&mut blob).ok()?;
    // A shipped `--exe` artifact is invoked directly, so its real process argv (`[<binary>, <args…>]`)
    // is exactly the program's argument vector — pass it straight through to `args.all()`.
    // A stapled executable has no CLI of its own (its argv belongs to the program) — no report.
    Some(cmd_run_bundle(
        &exe_path,
        &blob,
        std::env::args().collect(),
        false,
    ))
}

fn emit_diagnostics<'a>(source: &Source, diagnostics: impl Iterator<Item = &'a Diagnostic>) {
    let mut stderr = io::stderr();
    for diagnostic in diagnostics {
        let _ = stderr.write_all(render(source, diagnostic).as_bytes());
    }
}

/// Print [`noeta_diagnostics::render_mapped`]'s cross-module rendering to stderr — each diagnostic
/// resolved against the source its span belongs to.
fn emit_diagnostics_mapped<'a>(
    sources: &SourceMap,
    diagnostics: impl Iterator<Item = &'a Diagnostic>,
) {
    let _ = io::stderr().write_all(render_mapped(sources, diagnostics).as_bytes());
}

/// The argument vector a `noeta run` presents to the program via `args.all()`: the entry path as the
/// program name (argv[0]) followed by any pass-through args given after `--`. This mirrors a shipped
/// `noeta build --exe` binary, whose `args.all()` is the real process argv (`[<binary>, <args…>]`)
/// when invoked directly — so a program observes the identical argv from source or as an executable.
fn program_args(entry: &std::path::Path, passthrough: &[String]) -> Vec<String> {
    let mut args = Vec::with_capacity(passthrough.len() + 1);
    args.push(entry.display().to_string());
    args.extend(passthrough.iter().cloned());
    args
}

fn cmd_run(
    file: &std::path::Path,
    tiers: &[String],
    target: &Option<String>,
    no_cache: bool,
    jit_stats: bool,
    args: &[String],
) -> ExitCode {
    if let Some(code) = compose::maybe_delegate(file) {
        return code;
    }
    // P-AOT L1.2: a `.noeb` bundle runs directly — no source, no compile. Sniff the magic (cheap,
    // and we need the bytes to load it anyway); anything else is source, handled below. Tiers are a
    // *build*-time concern (they are already baked into the bundle), so `--tier`/`--target` on a
    // bundle run are meaningless — reject them rather than silently ignore.
    if let Ok(bytes) = std::fs::read(file)
        && noeta_bundle::is_bundle(&bytes)
    {
        if !tiers.is_empty() || target.is_some() {
            eprintln!("lang: --tier/--target apply at build time; a .noeb bundle is already built");
            return ExitCode::from(2);
        }
        return cmd_run_bundle(file, &bytes, program_args(file, args), jit_stats);
    }

    // Everything else — resolve tiers, consult the startup cache, and (on a miss) load → check →
    // compile — is the shared whole-file pipeline. On success run the module with the program's
    // pass-through args; on failure report it.
    match compile_whole_file(file, tiers, target, no_cache) {
        Ok(compiled) => run_compiled_module(
            compiled.module,
            &compiled.sources,
            program_args(file, args),
            jit_stats,
        ),
        Err(failure) => failure.report(),
    }
}

/// The `--format json` report: the whole outcome of a `noeta check` run in one serializable object.
#[derive(serde::Serialize)]
struct CheckReport {
    /// The number of `.noe` files checked (entries walked).
    files_checked: usize,
    /// The number of unique error-severity diagnostics.
    errors: usize,
    /// The number of unique warning-severity diagnostics.
    warnings: usize,
    /// Every unique diagnostic, resolved to files + line/column (see [`noeta_diagnostics::to_json`]).
    diagnostics: Vec<noeta_diagnostics::JsonDiagnostic>,
}

/// `noeta check [PATH]` — statically validate source without running or building it: parse every
/// `.noe` file and verify it type-checks, printing all diagnostics and exiting non-zero if any is an
/// error. This is `cmd_run`'s front half — load → (activate tiers) → `check_all` — stopping before
/// `execute_real_host`, so it has no side effects.
///
/// `PATH` (default `.`) is a file or a directory; a directory is walked recursively and **every**
/// `.noe` file is checked as its own entry. The loader links only an entry's *directory-sibling*
/// modules (there is no cross-directory module graph), so checking each file as an entry is what
/// guarantees a library module no single entry imports is still parsed and type-checked. A module
/// shared by several entries is therefore linked (and its diagnostics produced) once per importer;
/// diagnostics are deduplicated globally by their source file + span + code so each is reported once.
///
/// With `--format json` the whole result is emitted as one machine-readable object on stdout (for
/// CI, editors, and the MCP server) instead of human-rendered diagnostics on stderr; the exit code
/// is identical in both modes.
fn cmd_check(
    path: &std::path::Path,
    tiers: &[String],
    target: &Option<String>,
    format: OutputFormat,
) -> ExitCode {
    if let Some(code) = compose::maybe_delegate(path) {
        return code;
    }
    use noeta_diagnostics::Severity;

    // The active tier set — resolved once and applied to every file — is the union of a `--target`'s
    // live tiers (from `noeta.toml`) and any explicit `--tier` flags, exactly as `cmd_run` resolves
    // it. A bad target fails fast before any file is read.
    let mut active: Vec<String> = match target {
        Some(name) => match manifest::resolve_active_tiers(path, name) {
            Ok(tiers) => tiers,
            Err(err) => {
                eprintln!("lang: {err}");
                return ExitCode::from(1);
            }
        },
        None => Vec::new(),
    };
    for tier in tiers {
        if !active.contains(tier) {
            active.push(tier.clone());
        }
    }
    // The target's tier → provider map steers which declaration's config attribute activation
    // stamps (provider dispatch); empty without a target.
    let providers = match resolve_providers(path, target) {
        Ok(map) => map,
        Err(err) => {
            eprintln!("lang: {err}");
            return ExitCode::from(1);
        }
    };
    let active_refs: Vec<&str> = active.iter().map(String::as_str).collect();

    // The set of entry files to check: the file itself, or every `.noe` file under the directory.
    let entries: Vec<PathBuf> = if path.is_dir() {
        noe_files(path)
    } else {
        vec![path.to_path_buf()]
    };
    if entries.is_empty() {
        eprintln!("lang: no `.noe` files found under `{}`", path.display());
        return ExitCode::from(2);
    }

    // Deduplicate diagnostics across every entry's workspace. `SourceId`s are workspace-local (each
    // load restarts them at 0), so the key is the *file name* the diagnostic renders against plus its
    // byte span and code — never the id. The map's key order (name, then offset, then code) is also
    // the render order, so output is deterministic. Each value keeps the workspace's `SourceMap`
    // (shared via `Rc` so a workspace's diagnostics don't each clone it) so a diagnostic — and any of
    // its cross-file labels — resolves against the right source in both the human and JSON paths.
    type MapDiag = (std::rc::Rc<SourceMap>, Diagnostic);
    let mut diags: std::collections::BTreeMap<(String, u32, u32, &'static str), MapDiag> =
        std::collections::BTreeMap::new();
    let mut fold = |sources: &std::rc::Rc<SourceMap>, diag: &Diagnostic| {
        let key = (
            sources.source(diag.span.source).name().to_string(),
            diag.span.start,
            diag.span.end,
            diag.code.code(),
        );
        diags
            .entry(key)
            .or_insert_with(|| (std::rc::Rc::clone(sources), diag.clone()));
    };

    let mut unreadable = false;
    for entry in &entries {
        // Resolve the entry's dependency packages so a cross-package `use <dep-key>.…` type-checks
        // accurately under `noeta check`, matching `run` (package-manager P2.1c). A resolution
        // failure is reported like an unreadable file — it doesn't abort the whole check.
        let deps = match graph::resolve_graph(entry) {
            Ok(graph) => graph.packages,
            Err(err) => {
                eprintln!("lang: {}: {err}", entry.display());
                unreadable = true;
                continue;
            }
        };
        match noeta_loader::load_with_deps(entry, &deps) {
            Err(err) => {
                // One unreadable file does not abort the whole run — record it and keep checking the
                // rest, so `check` reports as much as it can in a single pass.
                eprintln!("lang: cannot read {}: {err}", entry.display());
                unreadable = true;
            }
            Ok(Err(load_diagnostics)) => {
                // Lex/parse errors — each carries the single source it renders against. Wrap it in a
                // one-element `SourceMap` (any `SourceId` resolves back to that source), matching how
                // `cmd_run` renders load diagnostics single-source.
                for ld in &load_diagnostics {
                    let sources = std::rc::Rc::new(SourceMap::new(vec![ld.source.clone()]));
                    fold(&sources, &ld.diagnostic);
                }
            }
            Ok(Ok(linked)) => {
                // Activate the resolved dev-tiers before checking, as `run`/`build`/`dump` do; with no
                // active tiers the program is checked as-is. Tier-activation diagnostics resolve
                // against the same workspace sources.
                let sources = std::rc::Rc::new(linked.sources);
                let program_diags = if active_refs.is_empty() {
                    noeta_check::check_all(&linked.program).diagnostics
                } else {
                    let activated =
                        noeta_check::activate_tiers_with(&linked.program, &active_refs, &providers);
                    let mut ds = activated.diagnostics;
                    ds.extend(noeta_check::check_all(&activated.program).diagnostics);
                    ds
                };
                for d in &program_diags {
                    fold(&sources, d);
                }
            }
        }
    }

    // Count severities once (independent of output format): errors gate the exit code, warnings print
    // but pass.
    let mut errors = 0usize;
    let mut warnings = 0usize;
    for (_, diag) in diags.values() {
        match diag.severity {
            Severity::Error => errors += 1,
            Severity::Warning => warnings += 1,
            Severity::Note => {}
        }
    }
    let n = entries.len();

    match format {
        OutputFormat::Human => {
            // Render each unique diagnostic against its workspace sources (color disabled), then a
            // summary line — all to stderr, as the other commands do.
            let mut stderr = io::stderr();
            for (sources, diag) in diags.values() {
                let _ = stderr.write_all(render_mapped(sources, std::iter::once(diag)).as_bytes());
            }
            let files = if n == 1 { "file" } else { "files" };
            eprintln!("checked {n} {files}: {errors} error(s), {warnings} warning(s)");
        }
        OutputFormat::Json => {
            // A single machine-readable report on stdout, so a tool can pipe `noeta check --format
            // json` and parse it. Operational `cannot read` errors stay on stderr; the exit code
            // still reflects them.
            let report = CheckReport {
                files_checked: n,
                errors,
                warnings,
                diagnostics: diags
                    .values()
                    .map(|(sources, diag)| noeta_diagnostics::to_json(sources, diag))
                    .collect(),
            };
            match serde_json::to_string_pretty(&report) {
                Ok(json) => println!("{json}"),
                Err(err) => eprintln!("lang: cannot serialize check report: {err}"),
            }
        }
    }

    if errors > 0 {
        ExitCode::from(1)
    } else if unreadable {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}

/// Collect every `.noe` file under `root`, recursively, in sorted order (so discovery and thus the
/// check order are deterministic). Hand-rolled in the style of the loader's `read_siblings` — a
/// depth-first `read_dir` walk that silently skips directories it cannot read (a partial tree still
/// checks what it can). Symlinked directories are followed by `read_dir` as ordinary entries; cycles
/// are not guarded against, matching the loader's own assumptions about a normal source tree.
fn noe_files(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut dirs = Vec::new();
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().is_some_and(|ext| ext == "noe") {
                out.push(p);
            }
        }
        stack.extend(dirs);
    }
    out.sort();
    out
}

/// Build the clap subcommand for an extension-contributed command (higher-order-abi H6) from
/// its declared [`noeta_stdlib::ArgSpec`]s — real help text and validation, same as a core verb.
/// The external-binary command form (package-manager Phase 3, N3.7 — the `cargo-<name>` model):
/// `noeta <cmd>` with an unknown `<cmd>` looks for an executable `noeta-<cmd>` on `PATH` and runs
/// it with everything after the subcommand as its argv, forwarding the exit code. Returns `None`
/// (letting clap's error render) when the name can't be extracted or no such binary exists.
/// `noeta <tier> <file>` for a **declared tier** (tier-providers T4). When the unknown subcommand
/// names a tier the file's linked program declares with `@tier(name[, config: T])`, this owns the
/// command: activate exactly that tier, then invoke its runner **in-process** with the activated
/// roots — a synthesized `runner([TierRoot { name: "…", run: <fn> }, …])` fragment appended to the
/// activated program and run through the ordinary real-host pipeline (the in-process
/// reflected-handles protocol). Knob values are not passed here: the block-stamped config
/// attributes travel on the root fns, and the runner reads them with `attributes_of::<Config>()`.
/// Returns `None` when this is not a tier invocation — no file argument, an unloadable file, or a
/// name the program does not declare — so the caller falls through to the external-binary probe.
fn try_tier_dispatch(err: &clap::Error) -> Option<ExitCode> {
    let name = err
        .get(clap::error::ContextKind::InvalidSubcommand)
        .and_then(|v| match v {
            clap::error::ContextValue::String(s) => Some(s.clone()),
            _ => None,
        })?;
    // The subcommand's first argument is the entry file.
    let file: PathBuf = std::env::args_os()
        .skip(1)
        .skip_while(|a| *a != *name.as_str())
        .nth(1)
        .map(PathBuf::from)?;
    if !file.is_file() {
        return None;
    }
    // An optional `--target <NAME>` after the subcommand selects a build target whose tier →
    // provider map steers resolution (same-named tiers from different packages).
    let argv: Vec<String> = std::env::args()
        .skip(1)
        .skip_while(|a| *a != name)
        .collect();
    let target = argv
        .iter()
        .position(|a| a == "--target")
        .and_then(|i| argv.get(i + 1))
        .cloned();
    let providers = match resolve_providers(&file, &target) {
        Ok(map) => map,
        Err(err) => {
            eprintln!("lang: {err}");
            return Some(ExitCode::from(2));
        }
    };
    // A file that fails to load or link cannot tell us whether it declares the tier — fall
    // through (the external probe, then clap's error). Dependencies resolve first: a declared
    // tier typically lives in a dependency package (`use fuzzkit.tiers.run_fuzz`).
    let deps = graph::resolve_graph(&file).ok()?.packages;
    let linked = noeta_loader::load_with_deps(&file, &deps).ok()?.ok()?;
    let activated = noeta_check::activate_tiers_with(&linked.program, &[&name], &providers);
    let tier = match activated.registry.resolve_provider(&name, &providers) {
        Ok(noeta_check::ResolvedProvider::Declared(d)) => d.clone(),
        // A built-in name or an unknown one — not this fallback's command; clap's error (or the
        // external probe) is the right answer.
        Ok(noeta_check::ResolvedProvider::Extension) => return None,
        Err(err) => {
            // The name is tier-shaped (the target maps it) but the provider doesn't resolve —
            // that is a real user error, not fall-through material.
            if providers.contains_key(name.as_str()) {
                eprintln!("lang: {err}");
                return Some(ExitCode::from(2));
            }
            return None;
        }
    };
    Some(run_declared_tier(&name, &linked, activated, tier))
}

/// Run a declared tier's dispatch over an already-activated program: type-check, synthesize the
/// runner call over the collected roots, and execute on the real host. Owns all reporting; the
/// tier and its declaration are known to exist by the time this runs.
fn run_declared_tier(
    name: &str,
    linked: &noeta_loader::Linked,
    activated: noeta_check::Activated,
    tier: noeta_check::DeclaredTier,
) -> ExitCode {
    if !activated.diagnostics.is_empty() {
        emit_diagnostics_mapped(&linked.sources, activated.diagnostics.iter());
        return ExitCode::from(1);
    }
    // The activated roots for this tier — a built-in name's roots land in the dedicated sinks
    // (an overridden `bench = "criterion"` still collects under `benches`), a custom tier's in
    // `custom`.
    let roots = match name {
        "test" => activated.tests.clone(),
        "bench" => activated.benches.clone(),
        _ => activated.custom.get(name).cloned().unwrap_or_default(),
    };

    // The runner call — `<runner>([TierRoot { name: "<fn>", run: <fn> }, …])` — is built as AST
    // directly: the runner (and, in a namespaced entry, a root) carries its **link-qualified**
    // dotted name, which is an identifier to the resolved program but would parse as member
    // access from text. The `TierRoot` *declaration* is textual (no names to qualify) and only
    // synthesized when the program doesn't already declare one: the checker knows `TierRoot` as a
    // prelude type — that is what lets the runner's package name `List<TierRoot>` standalone —
    // but the backends build record literals from real declarations, and a declaration of the
    // same name shadows the prelude registration by design.
    let mut program = activated.program;
    let declares_tier_root = program
        .stmts
        .iter()
        .any(|s| matches!(s, Stmt::Struct(d) if d.name == noeta_ast::reflect::TIER_ROOT));
    if !declares_tier_root {
        let fragment = parse_fragment(
            SourceId(u32::MAX),
            "<tier-dispatch>",
            "struct TierRoot { name: string  run: () -> void }",
        );
        if !fragment.diagnostics.is_empty() {
            eprintln!("lang: internal error synthesizing the `{name}` dispatch");
            return ExitCode::from(2);
        }
        program.stmts.extend(fragment.program.stmts);
    }
    let span = program.span;
    let root_items: Vec<Expr> = roots
        .iter()
        .map(|root| {
            Expr::Object(noeta_ast::ObjectLit {
                type_name: noeta_ast::reflect::TIER_ROOT.to_string(),
                type_name_span: span,
                fields: vec![
                    noeta_ast::FieldInit {
                        name: "name".to_string(),
                        name_span: span,
                        value: Expr::Str {
                            value: root.name.clone(),
                            span,
                        },
                        span,
                    },
                    noeta_ast::FieldInit {
                        name: "run".to_string(),
                        name_span: span,
                        value: Expr::Ident {
                            name: root.name.clone(),
                            span,
                        },
                        span,
                    },
                ],
                spread: None,
                span,
            })
        })
        .collect();
    program.stmts.push(call_stmt(
        &tier.runner,
        vec![Expr::List {
            items: root_items,
            span,
        }],
        span,
    ));
    // One check over the whole dispatch program: the user's code, the stamped attributes, the
    // runner's signature, and the synthesized call all validate together.
    let checked = noeta_check::check_all(&program);
    if !checked.diagnostics.is_empty() {
        emit_diagnostics_mapped(&linked.sources, checked.diagnostics.iter());
        return ExitCode::from(1);
    }
    match execute_real_host(&program, &checked, std::env::args().collect()) {
        Ok((result, trace)) => {
            print!("{}", result.stdout);
            let _ = io::stdout().flush();
            emit_diagnostics_mapped(&linked.sources, result.diagnostics.iter());
            if trace.len() >= 2 {
                eprint!("{}", noeta_vm::render_trace(&trace, &linked.sources));
            }
            ExitCode::from(result.exit_code.clamp(0, 255) as u8)
        }
        Err(msg) => {
            eprintln!("lang: {msg}");
            ExitCode::from(1)
        }
    }
}

fn external_command_fallback(err: &clap::Error) -> Option<ExitCode> {
    let name = err
        .get(clap::error::ContextKind::InvalidSubcommand)
        .and_then(|v| match v {
            clap::error::ContextValue::String(s) => Some(s.clone()),
            _ => None,
        })?;
    let binary = format!("noeta-{name}");
    let path = std::env::var_os("PATH")?;
    let found = std::env::split_paths(&path)
        .map(|dir| dir.join(&binary))
        .find(|candidate| is_executable(candidate))?;
    // Forward everything after the subcommand token (global flags before it are noeta's own).
    let args: Vec<std::ffi::OsString> = std::env::args_os()
        .skip(1)
        .skip_while(|a| *a != *name.as_str())
        .skip(1)
        .collect();
    let status = std::process::Command::new(&found)
        .args(&args)
        .status()
        .map_err(|e| eprintln!("lang: running `{}` failed: {e}", found.display()))
        .ok()?;
    Some(ExitCode::from(status.code().unwrap_or(1) as u8))
}

/// Whether `path` is an existing executable file (the PATH-probe test for external commands).
fn is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn ext_command_clap(ext: &'static noeta_stdlib::ExtCommand) -> clap::Command {
    let mut cmd = clap::Command::new(ext.name).about(ext.about);
    for spec in ext.args {
        cmd = cmd.arg(match spec.kind {
            noeta_stdlib::ArgKind::Path => clap::Arg::new(spec.name)
                .help(spec.help)
                .required(true)
                .value_parser(clap::value_parser!(PathBuf)),
            // The default is applied at dispatch (clap's builder `default_value` wants an
            // owned-string feature we don't enable); the spec's help text names it.
            noeta_stdlib::ArgKind::Int { .. } => clap::Arg::new(spec.name)
                .long(spec.name)
                .help(spec.help)
                .value_parser(clap::value_parser!(i64)),
        });
    }
    cmd
}

/// Dispatch a matched extension command: collect its declared args from the clap matches and run
/// it against the CLI's [`noeta_stdlib::CommandCtx`] driver.
fn ext_command_dispatch(
    ext: &'static noeta_stdlib::ExtCommand,
    matches: &clap::ArgMatches,
) -> ExitCode {
    let mut parsed = noeta_stdlib::ParsedArgs::default();
    for spec in ext.args {
        match spec.kind {
            noeta_stdlib::ArgKind::Path => parsed.push_path(
                spec.name,
                matches
                    .get_one::<PathBuf>(spec.name)
                    .expect("a required path argument is always present")
                    .clone(),
            ),
            noeta_stdlib::ArgKind::Int { default } => parsed.push_int(
                spec.name,
                matches
                    .get_one::<i64>(spec.name)
                    .copied()
                    .unwrap_or(default),
            ),
        }
    }
    ExitCode::from((ext.run)(&mut CliCommandCtx, &parsed))
}

/// The CLI's [`noeta_stdlib::CommandCtx`] driver (higher-order-abi H6): load + check a program
/// file and run it on the real host, optionally appending a synthesized trailing entry call —
/// exactly what the hardcoded `cmd_serve` did, generalized over [`noeta_stdlib::EntryCall`].
/// Layering the entry on the loaded program means the mechanism is the exact same registered
/// function a program can call directly; the command only supplies the entry convention.
struct CliCommandCtx;

impl noeta_stdlib::CommandCtx for CliCommandCtx {
    fn run_file(
        &mut self,
        file: &std::path::Path,
        entry: Option<&noeta_stdlib::EntryCall>,
        banner: Option<&str>,
    ) -> u8 {
        use noeta_ast::{Expr, Stmt};
        use noeta_span::Span;

        // An extension command drives a program run; a native-dep app delegates to its composed
        // toolchain like every other verb (composition failure is fatal, never a stock fallback).
        if compose::maybe_delegate(file).is_some() {
            return 1;
        }

        // Resolve dependency packages so `noeta serve` (and any entry-call command) sees the same
        // cross-package `use <dep-key>.…` the plain `run` path does (package-manager P2.1c).
        let deps = match graph::resolve_graph(file) {
            Ok(graph) => graph.packages,
            Err(err) => {
                eprintln!("lang: {err}");
                return 2;
            }
        };
        let mut linked = match noeta_loader::load_with_deps(file, &deps) {
            Err(err) => {
                eprintln!("lang: cannot read {}: {err}", file.display());
                return 2;
            }
            Ok(Err(load_diagnostics)) => {
                let mut stderr = io::stderr();
                for ld in &load_diagnostics {
                    let _ = stderr.write_all(render(&ld.source, &ld.diagnostic).as_bytes());
                }
                return 1;
            }
            Ok(Ok(linked)) => linked,
        };

        // Synthesize `<module>.<func>(<args>)` as a trailing top-level statement. The program
        // supplies any identifiers the call names (`fetch`); a missing one surfaces as an
        // ordinary check error. A synthetic span (offset 0) is fine — this node is
        // compiler-generated, never the subject of a diagnostic the user needs to locate.
        if let Some(entry) = entry {
            let sp = Span::empty_at(0);
            let ident = |name: &str| Expr::Ident {
                name: name.to_string(),
                span: sp,
            };
            let callee = Expr::Member {
                receiver: Box::new(ident(entry.module)),
                name: entry.func.to_string(),
                name_span: sp,
                span: sp,
            };
            let args = entry
                .args
                .iter()
                .map(|arg| match arg {
                    noeta_stdlib::EntryArg::Int(value) => Expr::Int {
                        value: *value,
                        span: sp,
                    },
                    noeta_stdlib::EntryArg::Ident(name) => ident(name),
                })
                .collect();
            let call = Expr::Call {
                callee: Box::new(callee),
                args,
                span: sp,
            };
            linked.program.stmts.push(Stmt::Expr {
                expr: call,
                span: sp,
            });
        }

        if let Some(banner) = banner {
            eprintln!("{banner}");
        }
        // An entry call injected before compiling means the module differs from `run`'s for the
        // same source — a command run must never share the startup cache's `(source+tiers)` key,
        // and stays on the uncached `run_program` path (commands like `serve` are also
        // long-lived, so they would barely benefit). Commands take no program pass-through args;
        // the program sees the real process argv.
        u8::try_from(run_program(
            &linked.program,
            &linked.sources,
            std::env::args().collect(),
        ))
        .unwrap_or(1)
    }
}

/// Run a `.noeb` bundle (P-AOT L1.2): decode the module from the versioned container and execute it
/// on the real host, exactly as a source run does after compiling — but with no source to compile
/// or type-check (both happened at build time). A runtime abort's diagnostics/trace carry spans but
/// the bundle ships no source text, so they render against a synthetic empty source (message + code
/// + location show; no code snippet) — the honest cost of a source-free artifact.
fn cmd_run_bundle(
    file: &std::path::Path,
    bytes: &[u8],
    args: Vec<String>,
    jit_stats: bool,
) -> ExitCode {
    let module = match noeta_bundle::read(bytes) {
        Ok(module) => module,
        Err(err) => {
            eprintln!("lang: cannot load {}: {err}", file.display());
            return ExitCode::from(2);
        }
    };
    let sources = SourceMap::new(vec![Source::new(
        SourceId::FIRST,
        file.display().to_string(),
        "",
    )]);
    run_compiled_module(std::sync::Arc::new(module), &sources, args, jit_stats)
}

/// A resolved startup-cache slot: an open cache, the content key for this program, and the workspace
/// `SourceMap` (so a cache hit renders diagnostics against real source without re-parsing). Built by
/// [`open_startup_cache`], consumed by [`compile_whole_file`].
struct CacheSlot {
    cache: noeta_cache::Cache,
    key: noeta_cache::CacheKey,
    sources: SourceMap,
}

/// A compiled whole-file program: the runnable module plus the sources its spans resolve against.
struct Compiled {
    module: std::sync::Arc<noeta_bytecode::Module>,
    sources: SourceMap,
}

/// A whole-file compile failure, carrying what's needed to render it. [`report`](Self::report)
/// prints it and yields the process exit code, matching each command's prior behavior.
enum CompileFailure {
    /// A message rendered as `lang: {0}` with exit 1 (target resolution / compiler-internal error).
    Message(String),
    /// The entry file could not be read (exit 2).
    Unreadable(String),
    /// Load-time (lex/parse) diagnostics, each paired with its own source (exit 1).
    Load(Vec<noeta_loader::LoadDiagnostic>),
    /// Tier-activation or type-check diagnostics, rendered against `sources` (exit 1).
    Diagnostics {
        sources: SourceMap,
        diagnostics: Vec<Diagnostic>,
    },
}

impl CompileFailure {
    /// Print the failure to stderr and return the process exit code.
    fn report(&self) -> ExitCode {
        match self {
            CompileFailure::Message(msg) => {
                eprintln!("lang: {msg}");
                ExitCode::from(1)
            }
            CompileFailure::Unreadable(msg) => {
                eprintln!("lang: {msg}");
                ExitCode::from(2)
            }
            CompileFailure::Load(diagnostics) => {
                let mut stderr = io::stderr();
                for ld in diagnostics {
                    let _ = stderr.write_all(render(&ld.source, &ld.diagnostic).as_bytes());
                }
                ExitCode::from(1)
            }
            CompileFailure::Diagnostics {
                sources,
                diagnostics,
            } => {
                emit_diagnostics_mapped(sources, diagnostics.iter());
                ExitCode::from(1)
            }
        }
    }
}

/// The whole-file compile pipeline, shared by `run`/`dump`/`build` and cache-aware in one place: any
/// command that wants "a source file → its runnable [`Module`]" goes through here, so the startup
/// cache is applied exactly once rather than wired per command.
///
/// Resolves the active tier set (target ∪ `--tier`), then consults the startup cache: on a **hit**
/// the decoded module is returned directly (the whole front-end is skipped); on a **miss** it loads →
/// activates tiers → type-checks → compiles, populates the cache (best-effort), and returns. Because
/// `run`/`dump`/`build` all compile identically (same [`compile_real`]), they share cache entries —
/// a `noeta build` warms the exact entry `noeta run` reads, and vice versa.
///
/// `serve` does **not** use this (it injects an `http.serve(...)` call, so its module differs for the
/// same source and must never share the `(source+tiers)` key). `test`/`bench` do **not** either (they
/// compile a separate module per `@test`/`@bench` case — a different granularity).
fn compile_whole_file(
    file: &std::path::Path,
    tiers: &[String],
    target: &Option<String>,
    no_cache: bool,
) -> Result<Compiled, CompileFailure> {
    // The active tier set is the union of any `--target`'s live tiers (from `noeta.toml`) and any
    // explicit `--tier` flags, resolved before loading so a bad target fails fast.
    let mut active: Vec<String> = match target {
        Some(name) => {
            manifest::resolve_active_tiers(file, name).map_err(CompileFailure::Message)?
        }
        None => Vec::new(),
    };
    for tier in tiers {
        if !active.contains(tier) {
            active.push(tier.clone());
        }
    }

    // The target's tier → provider map (provider dispatch): decides which declaration's config
    // attribute activation stamps, so it is part of the compiled program — and of the cache key.
    let providers = resolve_providers(file, target).map_err(CompileFailure::Message)?;

    // The entry's dependency packages (package-manager P2.1): their sources feed both the cache key
    // (so a dep change never serves stale bytecode) and the loader (so `use <dep-key>.…` resolves).
    let deps = manifest::dependency_packages(file).map_err(CompileFailure::Message)?;

    // Startup cache (M3): on a hit, return the cached module — load/check/compile all skipped.
    let cache = open_startup_cache(file, &active, &providers, &deps, no_cache);
    if let Some(slot) = &cache
        && let Some(blob) = slot.cache.load(&slot.key)
        && let Ok(module) = noeta_bundle::read(&blob)
    {
        return Ok(Compiled {
            module: std::sync::Arc::new(module),
            sources: slot.sources.clone(),
        });
    }

    // Miss: load + link (sibling `.noe` modules the entry `use`s are resolved and merged; a lone file
    // links to itself; dependency packages re-rooted under their keys), activate any dev-tiers,
    // type-check, and compile to bytecode.
    let linked = match noeta_loader::load_with_deps(file, &deps) {
        Err(err) => {
            return Err(CompileFailure::Unreadable(format!(
                "cannot read {}: {err}",
                file.display()
            )));
        }
        Ok(Err(load_diagnostics)) => return Err(CompileFailure::Load(load_diagnostics)),
        Ok(Ok(linked)) => linked,
    };
    let sources = linked.sources;
    // Activation inlines each `@<tier> { … }` block; with no active tiers the program runs as-is and
    // every tier block is stripped at lowering (the default). Activation is only done when needed.
    let program = if active.is_empty() {
        linked.program
    } else {
        let active_refs: Vec<&str> = active.iter().map(String::as_str).collect();
        let activated = noeta_check::activate_tiers_with(&linked.program, &active_refs, &providers);
        if !activated.diagnostics.is_empty() {
            return Err(CompileFailure::Diagnostics {
                sources,
                diagnostics: activated.diagnostics,
            });
        }
        activated.program
    };
    let checked = noeta_check::check_all(&program);
    if !checked.diagnostics.is_empty() {
        return Err(CompileFailure::Diagnostics {
            sources,
            diagnostics: checked.diagnostics,
        });
    }
    let module = match compile_real(&program, &checked) {
        Ok(module) => std::sync::Arc::new(module),
        Err(err) => return Err(CompileFailure::Message(err)),
    };

    // Populate the cache, best-effort, then bound its size (oldest-first eviction) so it can't grow
    // without limit. Both run only on this already-slow miss path. Synchronous — encoding + writing
    // is trivial next to the compile we just paid — and the whole block is **panic-isolated**:
    // `store`/`prune` only ever return `Err` (already discarded), but `noeta_bundle::write`'s postcard
    // encode carries an `.expect` that could (practically never) panic, and a cache write must never
    // be able to abort an otherwise-successful run. A panic here still prints (surfacing the real bug)
    // but is caught and swallowed. `AssertUnwindSafe`: on unwind we observe none of the captured state
    // (`slot`/`module` are only read, and discarded), so there is no broken invariant to leak.
    if let Some(slot) = &cache {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = slot.cache.store(&slot.key, &noeta_bundle::write(&module));
            let _ = slot.cache.prune_to(noeta_cache::max_bytes());
        }));
    }
    Ok(Compiled { module, sources })
}

/// Run an already-compiled [`Module`] on the real host and render its output — the shared tail of the
/// `.noeb` bundle runner and the startup-cache *hit* path (both have a module with no source to
/// compile). `args` is the program's argv (see [`program_args`]). Diagnostics/trace render against
/// `sources` (real workspace sources on a cache hit; a synthetic empty source for a source-free bundle).
fn run_compiled_module(
    module: std::sync::Arc<noeta_bytecode::Module>,
    sources: &SourceMap,
    args: Vec<String>,
    jit_stats: bool,
) -> ExitCode {
    let (result, trace, report) =
        run_module_real_host(std::sync::Arc::clone(&module), args, jit_stats);
    print!("{}", result.stdout);
    let _ = io::stdout().flush();
    emit_diagnostics_mapped(sources, result.diagnostics.iter());
    if trace.len() >= 2 {
        eprint!("{}", noeta_vm::render_trace(&trace, sources));
    }
    // `--jit-stats`: the report renders after the program's own output, to stderr (it is
    // diagnostics, not program output). A JIT-less binary produces no report — say so rather
    // than print nothing.
    match report {
        Some(report) => eprint!("{}", render_jit_report(&report, &module, sources)),
        None if jit_stats => {
            eprintln!("lang: --jit-stats: this binary was built without the JIT (no report)");
        }
        None => {}
    }
    exit_code(result.exit_code)
}

/// Render the `--jit-stats` report (`noeta run --jit-stats`, stderr): tier-1 compile coverage, the
/// bail histogram, and OSR-declined loops — each site resolved to its function, source line, and
/// disassembled op. The histogram counts **native entries** that fell back (a frame stays tier-0
/// after a bail until its next entry), so read it as "how often the fallback happened", not a
/// per-iteration figure; a declined loop never enters native at all, which is why it is its own
/// section.
fn render_jit_report(
    report: &noeta_vm::JitReport,
    module: &noeta_bytecode::Module,
    sources: &SourceMap,
) -> String {
    use std::fmt::Write as _;
    // "app.noe:12" for an op site, resolved through the line table (always emitted, even in
    // production compiles); "?" for a site before the first line entry (a prototype's prologue).
    let site = |proto: u32, pc: u32| -> String {
        module.protos[proto as usize]
            .line_span(pc as usize)
            .map(|span| {
                let lc = sources.line_col(span);
                format!("{}:{}", sources.source(span.source).name(), lc.line)
            })
            .unwrap_or_else(|| "?".to_string())
    };
    let fn_name = |proto: u32| -> String {
        module.protos[proto as usize]
            .name
            .clone()
            .unwrap_or_else(|| format!("proto {proto}"))
    };
    let op_text = |proto: u32, pc: u32| -> String {
        module.protos[proto as usize].op_repr_at(pc as usize, &module.names, &module.global_names)
    };

    let mut out = String::new();
    let stubs = report.compiled.saturating_sub(report.native);
    let _ = writeln!(
        out,
        "── JIT report ──\ntier 1: {} of {} compiled prototypes native ({} bail stubs), compile time {:.1} ms",
        report.native,
        report.compiled,
        stubs,
        report.compile_ns_total as f64 / 1e6,
    );

    if report.bails.is_empty() {
        let _ = writeln!(
            out,
            "\nno bail events — native code never fell back mid-frame"
        );
    } else {
        let _ = writeln!(
            out,
            "\nbail sites (native code fell back to the interpreter; counts are native entries, not iterations):"
        );
        for b in &report.bails {
            let _ = writeln!(
                out,
                "  {:>8}\u{00d7}  {}  {}  {}",
                b.count,
                site(b.proto, b.pc),
                fn_name(b.proto),
                op_text(b.proto, b.pc),
            );
        }
        if report.bails.iter().any(|b| b.pc == 0) {
            let _ = writeln!(
                out,
                "  (a pc-0 site can also be the entry parameter guard: a heap argument bails before the first op)"
            );
        }
    }

    if !report.declined.is_empty() {
        let _ = writeln!(
            out,
            "\nloops declined tier 1 (every loop contains a non-native op; the prototype ran interpreted):"
        );
        for d in &report.declined {
            let _ = writeln!(out, "  {} — blocked by:", fn_name(d.proto));
            for &pc in &d.bail_pcs {
                let _ = writeln!(out, "    {}  {}", site(d.proto, pc), op_text(d.proto, pc),);
            }
        }
    }
    out
}

/// Build the startup-cache slot for a source run: open the cache and compute the content key from
/// the raw workspace (entry + sibling module sources) + runtime version + binary identity + the
/// active tier set. Returns `None` — meaning "run uncached" — when caching is disabled
/// (`--no-cache` or `NOETA_NO_CACHE`), the running binary can't be identified (so freshness can't be
/// guaranteed), the entry can't be read, or the cache directory can't be opened.
fn open_startup_cache(
    file: &std::path::Path,
    active: &[String],
    providers: &std::collections::BTreeMap<String, String>,
    deps: &[noeta_loader::DepPackage],
    no_cache: bool,
) -> Option<CacheSlot> {
    if no_cache || std::env::var_os("NOETA_NO_CACHE").is_some() {
        return None;
    }
    // The binary's build identity is mandatory: without it a same-version local toolchain rebuild
    // would reuse stale bytecode. If we can't obtain it, we must not cache.
    let binary = noeta_cache::binary_identity()?;
    // Read the entry + sibling sources (no lex/parse) — both the key material and, on a hit, the
    // SourceMap for rendering. SourceIds here match `noeta_loader::load_with_deps`'s assignment
    // (entry = 0, sorted siblings 1.., then each dependency's modules in order), so a cached module's
    // spans — including those from merged dependency declarations — resolve against this map.
    let workspace = noeta_loader::read_workspace(file).ok()?;
    let mut key = noeta_cache::KeyBuilder::new();
    // Which file is the *entry* is part of the key, not just the source set: a directory of
    // dir-flat modules compiles to a different program per entry, so `noeta run a.noe` and
    // `noeta run b.noe` in one directory must not collide (they would otherwise share the same
    // sorted source set and the second would run the first's cached bytecode).
    key.entry(source_key_name(&workspace.entry));
    key.source(
        source_key_name(&workspace.entry),
        workspace.entry.text().as_bytes(),
    );
    for module in &workspace.modules {
        key.source(source_key_name(module), module.text().as_bytes());
    }
    // Dependency packages are part of the compiled program: fold each dependency's key→root binding
    // (re-rooting changes the linked program even when the sources are byte-identical) and every
    // module's source text into the key, so any dependency change invalidates the cache.
    for dep in deps {
        key.source(format!("<dep {}>", dep.key), dep.root.as_bytes());
        // The transitive key-rewrite map is part of what re-rooting produces: two builds with
        // byte-identical dep sources but different transitive wiring link to different programs.
        for (local, global) in &dep.dep_renames {
            key.source(format!("<rename {} {local}>", dep.key), global.as_bytes());
        }
        for module in &dep.modules {
            key.source(&module.name, module.text.as_bytes());
        }
    }
    key.runtime_version(noeta_bundle::RUNTIME_VERSION)
        .binary_identity(binary);
    for tier in active {
        key.tier(tier);
    }
    // The provider selection changes what activation stamps (`bench = "criterion"` stamps
    // criterion's config attribute), so it is key material — encoded distinctly from bare tier
    // names; `BTreeMap` iteration keeps it deterministic.
    for (tier, provider) in providers {
        key.tier(format!("{tier}={provider}"));
    }
    let key = key.finish();

    let cache = noeta_cache::Cache::open()?;
    // Rebuild the exact Source sequence `link_with_deps` assigns SourceIds to, so a cached module's
    // spans resolve. `read_workspace` already gave entry (id 0) + siblings; the dependency modules
    // continue the ids in the same order the loader parses them.
    let mut sources = Vec::with_capacity(1 + workspace.modules.len());
    sources.push(workspace.entry);
    sources.extend(workspace.modules);
    let mut next_id = sources.len() as u32;
    for dep in deps {
        for module in &dep.modules {
            sources.push(Source::new(SourceId(next_id), &module.name, &module.text));
            next_id += 1;
        }
    }
    Some(CacheSlot {
        cache,
        key,
        sources: SourceMap::new(sources),
    })
}

/// The cache-key name for a source: its file name, so the key is independent of the path the program
/// was invoked through (`./app.noe` and `app.noe` share an entry). Two distinct programs can only
/// share a name here if their whole hashed content also matches — in which case they are identical.
fn source_key_name(source: &Source) -> &str {
    std::path::Path::new(source.name())
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_else(|| source.name())
}

/// `noeta dump <FILE>` — disassemble the program to its VM bytecode and print it to stdout. Loads,
/// activates any `--tier`/`--target`, type-checks, and compiles through the **same** pipeline as
/// `noeta run` (`compile_real`), so the disassembly is exactly what the VM executes — the tool for
/// inspecting codegen (which ops a construct lowers to, whether a reuse/in-place fast path fired,
/// how names/constants are laid out). A type error prints diagnostics and exits non-zero, like `run`.
fn cmd_dump(file: &std::path::Path, tiers: &[String], target: &Option<String>) -> ExitCode {
    if let Some(code) = compose::maybe_delegate(file) {
        return code;
    }
    // Same whole-file compile as `run` (so the disassembly is exactly what the VM runs), and a cache
    // participant — a cached module is byte-identical to a fresh compile, so the disassembly matches.
    match compile_whole_file(file, tiers, target, false) {
        Ok(compiled) => {
            print!("{}", compiled.module.disassemble());
            let _ = io::stdout().flush();
            ExitCode::SUCCESS
        }
        Err(failure) => failure.report(),
    }
}

/// `noeta build <FILE>` — compile a program to a self-contained artifact (P-AOT L1/L2). Loads +
/// links, activates any `--tier`/`--target`, type-checks, and compiles through the **same**
/// `compile_real` pipeline as `run`/`dump`. The result is emitted either as a `.noeb` bundle
/// (`noeta_bundle::write`, run by `noeta run app.noeb`) or — with `--exe` — as a self-contained
/// executable that runs the program on its own (`emit_exe`). Either artifact carries no `.noe`
/// source. A type error prints diagnostics and exits non-zero, like `run`.
fn cmd_build(
    file: &std::path::Path,
    out: Option<&std::path::Path>,
    exe: bool,
    native: bool,
    tiers: &[String],
    target: &Option<String>,
) -> ExitCode {
    if let Some(code) = compose::maybe_delegate(file) {
        return code;
    }
    if exe && native {
        eprintln!("lang: --exe and --native are mutually exclusive");
        return ExitCode::from(2);
    }
    // Same whole-file compile + startup cache as `run`/`dump`. The emit format doesn't affect the
    // module, so a `build` shares cache entries with a `run` of the same source — each warms the other.
    let module = match compile_whole_file(file, tiers, target, false) {
        Ok(compiled) => compiled.module,
        Err(failure) => return failure.report(),
    };
    let module = module.as_ref();
    if native {
        emit_native(file, out, module)
    } else if exe {
        emit_exe(file, out, module)
    } else {
        emit_bundle(file, out, module)
    }
}

/// Emit `module` as a standalone `.noeb` bundle (P-AOT L1.2) — the default `noeta build` output.
/// Writes to `out` if given, else the input path with a `.noeb` extension.
fn emit_bundle(
    file: &std::path::Path,
    out: Option<&std::path::Path>,
    module: &noeta_bytecode::Module,
) -> ExitCode {
    let blob = noeta_bundle::write(module);
    let out_path = out
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| file.with_extension("noeb"));
    match std::fs::write(&out_path, &blob) {
        Ok(()) => {
            eprintln!("wrote {} ({} bytes)", out_path.display(), blob.len());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("lang: cannot write {}: {err}", out_path.display());
            ExitCode::from(2)
        }
    }
}

/// Emit `module` as a self-contained executable (P-AOT L2.1): staple its bundle onto a copy of this
/// runtime binary, so the artifact runs the program with no separate `.noeb` or interpreter. Writes
/// to `out` if given, else the input path with its extension stripped (`app.noe` → `app`, or `.exe`
/// on Windows). The artifact is marked executable on Unix.
fn emit_exe(
    file: &std::path::Path,
    out: Option<&std::path::Path>,
    module: &noeta_bytecode::Module,
) -> ExitCode {
    // The runtime image to embed is *this* binary — the toolchain the user invoked. (A stapled exe
    // never reaches `build`; it runs its bundle, so `current_exe` here is always the plain toolchain.)
    let runtime = match std::env::current_exe().and_then(std::fs::read) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("lang: cannot read the runtime binary to embed: {err}");
            return ExitCode::from(2);
        }
    };
    let blob = noeta_bundle::write(module);
    let image = noeta_bundle::staple(&runtime, &blob);

    let default_out = if cfg!(windows) {
        file.with_extension("exe")
    } else {
        file.with_extension("")
    };
    let out_path = out.map(std::path::Path::to_path_buf).unwrap_or(default_out);
    // Never clobber the source with the artifact (e.g. an extension-less entry building to itself).
    if out_path == file {
        eprintln!(
            "lang: refusing to overwrite the source file {}; pass -o <path>",
            file.display()
        );
        return ExitCode::from(2);
    }
    if let Err(err) = std::fs::write(&out_path, &image) {
        eprintln!("lang: cannot write {}: {err}", out_path.display());
        return ExitCode::from(2);
    }
    // Make the artifact runnable (Unix): rwxr-xr-x.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) =
            std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(0o755))
        {
            eprintln!("lang: cannot mark {} executable: {err}", out_path.display());
            return ExitCode::from(2);
        }
    }
    eprintln!(
        "wrote {} ({} bytes, self-contained)",
        out_path.display(),
        image.len()
    );
    ExitCode::SUCCESS
}

/// Emit `module` as a **native** executable (P-AOT L3.2b(3)) — the final level. Steps:
///   1. AOT-compile every eligible prototype to a relocatable object (`compile_module_aot`), which
///      also defines the `noeta_aot_dispatch` table.
///   2. Link that object against the AOT runtime staticlib (`libnoeta_aot.a`) with a C toolchain
///      (`cc`) into a native binary: the runtime provides `main` + the `noeta_jit_*` helpers, the
///      object provides the native bodies + the dispatch table, and the linker resolves it all.
///   3. Staple the program's bundle onto that binary (the L2 mechanism), so at startup the runtime
///      recovers the module and binds the linked-in native bodies through the dispatch table.
///
/// The eligible prototypes run as machine code; ineligible ones interpret the same bytecode from the
/// stapled bundle — the identical hybrid the runtime JIT uses, just resolved at build time.
#[cfg(feature = "jit")]
fn emit_native(
    file: &std::path::Path,
    out: Option<&std::path::Path>,
    module: &noeta_bytecode::Module,
) -> ExitCode {
    let default_out = if cfg!(windows) {
        file.with_extension("exe")
    } else {
        file.with_extension("")
    };
    let out_path = out.map(std::path::Path::to_path_buf).unwrap_or(default_out);
    if out_path == file {
        eprintln!(
            "lang: refusing to overwrite the source file {}; pass -o <path>",
            file.display()
        );
        return ExitCode::from(2);
    }

    // 1. AOT object.
    let object = match noeta_vm::compile_module_aot(module) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("lang: AOT compile failed: {err}");
            return ExitCode::from(1);
        }
    };

    // A per-invocation scratch dir for the object + the pre-staple linked binary.
    let work = std::env::temp_dir().join(format!("noeta-aot-{}", std::process::id()));
    if let Err(err) = std::fs::create_dir_all(&work) {
        eprintln!("lang: cannot create a build directory: {err}");
        return ExitCode::from(2);
    }
    let cleanup = |code: ExitCode| -> ExitCode {
        let _ = std::fs::remove_dir_all(&work);
        code
    };
    let obj_path = work.join("program.o");
    if let Err(err) = std::fs::write(&obj_path, &object) {
        eprintln!("lang: cannot write the AOT object: {err}");
        return cleanup(ExitCode::from(2));
    }

    // 2. Locate the runtime archive + the system libs it must link with, then `cc`-link. The archive
    // is built with only the stdlib rings this program uses, so unused rings' native deps are dropped.
    let rings = aot_ring_features(module);
    let (archive, libs) = match resolve_aot_runtime(&rings) {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("lang: {err}");
            return cleanup(ExitCode::from(1));
        }
    };
    let linked = work.join("linked");
    if let Err(err) = link_native(&obj_path, &archive, &libs, &linked) {
        eprintln!("lang: link failed: {err}");
        return cleanup(ExitCode::from(1));
    }

    // 3. Staple the bundle onto the linked binary (L2), so the runtime recovers the module.
    let runtime = match std::fs::read(&linked) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("lang: cannot read the linked binary: {err}");
            return cleanup(ExitCode::from(2));
        }
    };
    let blob = noeta_bundle::write(module);
    let image = noeta_bundle::staple(&runtime, &blob);
    if let Err(err) = std::fs::write(&out_path, &image) {
        eprintln!("lang: cannot write {}: {err}", out_path.display());
        return cleanup(ExitCode::from(2));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) =
            std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(0o755))
        {
            eprintln!("lang: cannot mark {} executable: {err}", out_path.display());
            return cleanup(ExitCode::from(2));
        }
    }
    eprintln!(
        "wrote {} ({} bytes, native AOT)",
        out_path.display(),
        image.len()
    );
    cleanup(ExitCode::SUCCESS)
}

/// Native AOT (`noeta build --native`) needs the JIT codegen; an interpreter-only build
/// (`--no-default-features`) has no AOT compiler, so it reports that rather than emitting a binary.
#[cfg(not(feature = "jit"))]
fn emit_native(
    _file: &std::path::Path,
    _out: Option<&std::path::Path>,
    _module: &noeta_bytecode::Module,
) -> ExitCode {
    eprintln!("lang: native AOT (`--native`) requires the JIT-enabled build (default features)");
    ExitCode::from(2)
}

/// The stdlib rings the program actually needs, as `noeta-aot-runtime` cargo features — derived from
/// the native modules it references. `noeta build --native` builds the runtime archive with exactly
/// these features, so the linker drops every unused ring's native dependency tree (DCE Axis B — e.g.
/// an http-free program sheds reqwest + its ~5 MB TLS stack).
///
/// A native module surfaces in the bytecode three ways, all covered here (missing one would build a
/// binary that stubs out a ring the program actually calls):
/// - `Const::NativeModule(name)` — `use std.{http}` loads the whole module as a value, then member
///   calls (`http.get_async(...)`) dispatch on it via `CallMethod` (no `TypedModuleCall`).
/// - `Const::ModuleFn { module, .. }` — a selective import (`use std.http.get`).
/// - `Op::TypedModuleCall { module, .. }` — the fully-qualified / turbofish call form (`json.parse::<T>`).
///
/// Conservative by construction: a module with **no** ring row here is always-on core (Ring-1
/// string/list/map/set, the pure builders) or a ring not yet gated — either way it stays in the
/// default feature set and is never stripped. Only a module with an explicit row can turn its ring
/// off, so an unrecognized or newly-added module can never be silently dropped.
#[cfg(feature = "jit")]
fn aot_ring_features(module: &noeta_bytecode::Module) -> Vec<String> {
    use noeta_bytecode::{Const, Op};
    use std::collections::BTreeSet;

    let mut rings: BTreeSet<String> = BTreeSet::new();

    // A whole-module value (`use std.{http}`): the program holds the module and can call *any* of its
    // functions dynamically (member calls lower to `CallMethod` on the value, whose receiver isn't
    // statically pinned), so a module that owns a native-dep ring is selected conservatively.
    // Module identities are root-qualified (`std.http`); the ring tables key on the module name, so
    // strip the root before looking up. A turbofish's module is the source receiver (a local name,
    // unrooted) and passes through `module_name` unchanged.
    // The module→ring map is the registry's now (package-manager P1.0): each `ExtModule` declares its
    // `ring`, so both the whole-module and precisely-named forms funnel through one registry lookup —
    // no CLI-side table to keep in sync with the stdlib. `ring_of` accepts a root-qualified path, a
    // bare name, or a turbofish's bound local alike.
    let note_module = |name: &str, rings: &mut BTreeSet<String>| {
        if let Some(ring) = noeta_stdlib::registry::ring_of(name) {
            rings.insert(ring.to_string());
        }
    };
    // A precisely-named function (`use std.http.client.get`, or a turbofish `TypedModuleCall`): post
    // the `std.http` client/server split the client/server distinction lives in the *module* identity,
    // so a named function selects exactly its module's ring — the same `ring_of` lookup as a whole
    // module. This is what lets the split pay off: a program naming only `http.server` functions
    // selects no client ring and sheds reqwest, while any `http.client` reference selects it.
    let note_fn = |m: &str, _func: &str, rings: &mut BTreeSet<String>| {
        if let Some(ring) = noeta_stdlib::registry::ring_of(m) {
            rings.insert(ring.to_string());
        }
    };

    for chunk in &module.protos {
        for c in &chunk.consts {
            match c {
                Const::NativeModule(name) => note_module(name, &mut rings),
                Const::ModuleFn { module: m, func } => note_fn(m, func, &mut rings),
                _ => {}
            }
        }
        for op in &chunk.code {
            if let Op::TypedModuleCall {
                module: m, func, ..
            } = op
            {
                note_fn(module.name(*m), module.name(*func), &mut rings);
            }
        }
    }
    rings.into_iter().collect()
}

/// Locate the AOT runtime staticlib (`libnoeta_aot.a`) and the native system libraries it must be
/// linked against, built with exactly the stdlib `rings` the program needs (DCE Axis B).
/// Priority: an explicit `NOETA_AOT_RUNTIME_LIB` (paired with `NOETA_AOT_LINK_LIBS`,
/// space-separated) — the packaged/hermetic path, which supplies its own ring set so `rings` is
/// ignored — else build it from the workspace with `cargo rustc --no-default-features --features
/// <rings> … --print native-static-libs` (interim: needs cargo + the source tree), which both
/// produces the archive and prints the exact link line. Packaging the archive for a shipped toolchain
/// (so `--native` works outside the workspace) is a later distribution decision.
#[cfg(feature = "jit")]
fn resolve_aot_runtime(rings: &[String]) -> Result<(std::path::PathBuf, Vec<String>), String> {
    if let Ok(path) = std::env::var("NOETA_AOT_RUNTIME_LIB") {
        let libs = std::env::var("NOETA_AOT_LINK_LIBS")
            .ok()
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_else(default_native_libs);
        return Ok((std::path::PathBuf::from(path), libs));
    }

    // Interim workspace build: one `cargo rustc` compiles the staticlib and prints its
    // native-static-libs note. `--no-default-features --features <rings>` links only the rings the
    // program uses; the `aot` runtime support is a hard dep feature, so it survives regardless.
    let mut args = vec![
        "rustc",
        "-p",
        "noeta-aot-runtime",
        "--release",
        "--no-default-features",
    ];
    let joined = rings.join(",");
    if !joined.is_empty() {
        args.push("--features");
        args.push(&joined);
    }
    args.extend(["--", "--print", "native-static-libs"]);
    let output = std::process::Command::new("cargo")
        .args(&args)
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("cannot run cargo to build the AOT runtime: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "building the AOT runtime failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let notes = String::from_utf8_lossy(&output.stderr);
    let libs = notes
        .lines()
        .find_map(|l| l.split_once("native-static-libs:"))
        .map(|(_, libs)| {
            libs.split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(default_native_libs);
    let archive = workspace_target_dir()?
        .join("release")
        .join("libnoeta_aot.a");
    if !archive.exists() {
        return Err(format!(
            "the AOT runtime archive was not found at {} after building",
            archive.display()
        ));
    }
    Ok((archive, libs))
}

/// A conservative default native-link set for a Rust staticlib on Linux, used when the exact
/// `native-static-libs` note is unavailable (an explicit archive with no `NOETA_AOT_LINK_LIBS`).
#[cfg(feature = "jit")]
fn default_native_libs() -> Vec<String> {
    [
        "-lgcc_s",
        "-lutil",
        "-lrt",
        "-lpthread",
        "-lm",
        "-ldl",
        "-lc",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// The workspace's Cargo target directory: `CARGO_TARGET_DIR` if set, else `<workspace root>/target`
/// found by walking up from the current directory for the `Cargo.toml` that declares `[workspace]`.
#[cfg(feature = "jit")]
fn workspace_target_dir() -> Result<std::path::PathBuf, String> {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        return Ok(std::path::PathBuf::from(dir));
    }
    let mut dir = std::env::current_dir().map_err(|e| e.to_string())?;
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.exists()
            && std::fs::read_to_string(&manifest)
                .map(|s| s.contains("[workspace]"))
                .unwrap_or(false)
        {
            return Ok(dir.join("target"));
        }
        if !dir.pop() {
            return Err(
                "cannot find the workspace root; run `noeta build --native` from inside the \
                 workspace, or set NOETA_AOT_RUNTIME_LIB to a prebuilt archive"
                    .to_string(),
            );
        }
    }
}

/// Link the AOT `object` against the runtime `archive` (+ its native `libs`) into `out` with a C
/// toolchain. Everything — the program's native bodies, the runtime, and Rust std — lives in the one
/// archive, so a single archive mention resolves the object↔runtime mutual references (the object
/// defines `noeta_aot_dispatch`, the archive defines `main` + the `noeta_jit_*` helpers). `cc` adds
/// the C runtime that calls `main`. The linker (`cc`) is overridable via `NOETA_CC`.
#[cfg(feature = "jit")]
fn link_native(
    object: &std::path::Path,
    archive: &std::path::Path,
    libs: &[String],
    out: &std::path::Path,
) -> Result<(), String> {
    let cc = std::env::var("NOETA_CC").unwrap_or_else(|_| "cc".to_string());
    let mut cmd = std::process::Command::new(&cc);
    cmd.arg(object).arg(archive).args(libs).arg("-o").arg(out);
    let output = cmd
        .output()
        .map_err(|e| format!("cannot run the linker `{cc}` (override with NOETA_CC): {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "`{cc}` exited with {}:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

/// Gate a tier runner on `--target`: if a target was given and does not make `tier` live, print a
/// note and return the success exit code (the runner no-ops); on a resolution failure, print it and
/// return the error code. `None` means "proceed" (no target gate). The caller runs its body only
/// when this returns `None`.
fn target_gate(entry: &std::path::Path, target: &Option<String>, tier: &str) -> Option<ExitCode> {
    match tier_active_in_target(entry, target, tier) {
        Ok(true) => None,
        Ok(false) => {
            println!(
                "tier `{tier}` is not active in target `{}`",
                target.as_deref().unwrap_or_default()
            );
            Some(ExitCode::SUCCESS)
        }
        Err(err) => {
            eprintln!("lang: {err}");
            Some(ExitCode::from(1))
        }
    }
}

/// Load and link `file` **with its dependency packages resolved** — the same dep-aware path
/// `run`/`check` use — rendering any failure. The tier runners (`test`/`bench`/`doc`) share this:
/// a program whose tier content imports from a dependency (a test exercising `use fuzzkit.…`, a
/// declared tier's runner) must link exactly as it does for a run.
fn load_linked(file: &std::path::Path) -> Result<noeta_loader::Linked, ExitCode> {
    let deps = match graph::resolve_graph(file) {
        Ok(graph) => graph.packages,
        Err(err) => {
            eprintln!("lang: {err}");
            return Err(ExitCode::from(2));
        }
    };
    match noeta_loader::load_with_deps(file, &deps) {
        Err(err) => {
            eprintln!("lang: cannot read {}: {err}", file.display());
            Err(ExitCode::from(2))
        }
        Ok(Ok(linked)) => Ok(linked),
        Ok(Err(load_diagnostics)) => {
            let mut stderr = io::stderr();
            for ld in &load_diagnostics {
                let _ = stderr.write_all(render(&ld.source, &ld.diagnostic).as_bytes());
            }
            Err(ExitCode::from(1))
        }
    }
}

/// The outcome of running one `@test` fn: whether it passed, the failure message (the first
/// diagnostic, typically the assertion/panic), and anything it wrote to stdout (shown on failure).
struct TestOutcome {
    name: String,
    passed: bool,
    message: Option<String>,
    stdout: String,
}

/// `noeta test <FILE>` — discover the program's `@test` blocks (object-model slice 6) and run each
/// as an isolated test. Tests run concurrently (one fresh isolate per test) and, by default, **all**
/// of them run even after a failure; `--fail-fast` stops at the first failure. A test fails when its
/// fn aborts — a false `assert`/`panic` (or any runtime error) — and passes when it returns normally.
/// The program's own top-level "main" effects are not run: `noeta test` runs the tests, not the file.
fn cmd_test(
    file: &std::path::Path,
    fail_fast: bool,
    jobs: Option<usize>,
    group: &Option<String>,
    names: &[String],
    json: bool,
    target: &Option<String>,
) -> ExitCode {
    if let Some(code) = compose::maybe_delegate(file) {
        return code;
    }
    if let Some(code) = target_gate(file, target, "test") {
        return code;
    }
    let linked = match load_linked(file) {
        Ok(linked) => linked,
        Err(code) => return code,
    };

    // Activate the `test` tier: inline its `@test` blocks as ordinary top-level declarations and
    // collect the test fns. An unknown-tier block is an E0036 (a typo must not silently vanish).
    // The target's provider selection (provider dispatch): `test = "<pkg>"` in the target's
    // tiers map hands this tier to that package's `@tier(test)` runner instead of the native
    // one. Default (no target, or `"std"`) keeps the native path below.
    let providers = match resolve_providers(file, target) {
        Ok(map) => map,
        Err(err) => {
            eprintln!("lang: {err}");
            return ExitCode::from(2);
        }
    };
    let activated = noeta_check::activate_tiers_with(&linked.program, &["test"], &providers);
    match activated.registry.resolve_provider("test", &providers) {
        Ok(noeta_check::ResolvedProvider::Extension) => {}
        Ok(noeta_check::ResolvedProvider::Declared(d)) => {
            let tier = d.clone();
            return run_declared_tier("test", &linked, activated, tier);
        }
        Err(err) => {
            eprintln!("lang: {err}");
            return ExitCode::from(2);
        }
    }
    if !activated.diagnostics.is_empty() {
        emit_diagnostics_mapped(&linked.sources, activated.diagnostics.iter());
        return ExitCode::from(1);
    }

    // Type-check the activated program once, so a broken test is a compile error reported a single
    // time here rather than redundantly inside every per-test run.
    let checked = noeta_check::check_all(&activated.program);
    if !checked.diagnostics.is_empty() {
        emit_diagnostics_mapped(&linked.sources, checked.diagnostics.iter());
        return ExitCode::from(1);
    }

    if activated.tests.is_empty() {
        if json {
            return report_json(&[], &[], 0);
        }
        println!("no tests found");
        return ExitCode::SUCCESS;
    }

    // The setup every test shares: the program's declarations (and top-level bindings/globals),
    // with its own "main" effect statements removed. Each test then runs as `setup + <call the test
    // fn>` in a fresh isolate, so the program's `echo`s don't run and one test cannot observe
    // another's state.
    let setup: Vec<Stmt> = activated
        .program
        .stmts
        .iter()
        .filter(|s| is_tier_setup(s))
        .cloned()
        .collect();

    // The `--group` filter (object-model slice 6h): keep only tests tagged `#[Group("<g>")]`.
    // `--name` (ide-ui U3) then narrows to the named fn(s) — the editor's run-one-test seam.
    let selected: Vec<&TierFn> = match group {
        Some(g) => activated
            .tests
            .iter()
            .filter(|t| test_group(t).as_deref() == Some(g.as_str()))
            .collect(),
        None => activated.tests.iter().collect(),
    };
    let selected: Vec<&TierFn> = if names.is_empty() {
        selected
    } else {
        selected
            .into_iter()
            .filter(|t| names.contains(&t.name))
            .collect()
    };
    if selected.is_empty() {
        if json {
            return report_json(&[], &[], 0);
        }
        match (group, names.is_empty()) {
            (_, false) => println!("no tests matching --name"),
            (Some(g), _) => println!("no tests in group `{g}`"),
            (None, _) => println!("no tests found"),
        }
        return ExitCode::SUCCESS;
    }

    // Partition into skipped (`#[Skip]`) and runnable. A skipped test is reported but never run, and
    // never fails the suite (a skipped `#[Data]` test counts as one skip, not one per row).
    let (skipped_refs, runnable): (Vec<&TierFn>, Vec<&TierFn>) =
        selected.into_iter().partition(|t| test_is_skipped(t));
    let skipped: Vec<String> = skipped_refs.iter().map(|t| skip_label(t)).collect();

    // Expand each runnable test into its case(s): a `#[Data([…])]` test runs once per row (reported
    // `name[row]`); an ordinary test is a single zero-arg case.
    let cases: Vec<TestCase> = runnable.iter().flat_map(|t| test_cases(t)).collect();
    let total = cases.len() + skipped.len();
    let run_count = cases.len();
    let jobs = jobs
        .filter(|n| *n > 0)
        .unwrap_or_else(default_jobs)
        .min(run_count.max(1));
    let skipped_note = if skipped.is_empty() {
        String::new()
    } else {
        format!(", {} skipped", skipped.len())
    };
    if !json {
        println!(
            "running {run_count} test{} on {jobs} thread{}{skipped_note}",
            plural(run_count),
            plural(jobs),
        );
    }

    let outcomes = run_tests(&setup, &cases, activated.program.span, jobs, fail_fast);
    if json {
        report_json(&outcomes, &skipped, total)
    } else {
        report(&outcomes, &skipped, total)
    }
}

/// One runnable test invocation: which fn to call, the report label, and an optional argument (a
/// `#[Data]` row — `None` for an ordinary zero-arg test). A `#[Data([a, b])]` test expands to one
/// `TestCase` per row.
struct TestCase {
    /// The fn to invoke.
    fn_name: String,
    /// The report label (`#[Name]`/fn name, suffixed `[row]` for a data case).
    display: String,
    /// The argument to pass.
    arg: CaseArg,
    /// Where the fn is declared (for the synthesized call's span).
    span: Span,
}

/// A test case's argument: none (an ordinary zero-arg test), a `#[Data]` row value, or an invalid
/// row whose literal cannot become a runtime value (the case fails with this message).
enum CaseArg {
    None,
    Value(Expr),
    Invalid(String),
}

/// Expand a runnable test into its cases: one zero-arg case normally, or one per row when the test
/// carries `#[Data([…])]`. A row literal that cannot be a runtime value (e.g. a bare type name)
/// becomes a case that fails with a clear message rather than being silently dropped.
fn test_cases(test: &TierFn) -> Vec<TestCase> {
    let base = test_display_name(test);
    let Some(rows) = data_rows(test) else {
        return vec![TestCase {
            fn_name: test.name.clone(),
            display: base,
            arg: CaseArg::None,
            span: test.span,
        }];
    };
    rows.iter()
        .map(|row| {
            let arg = match attr_value_to_expr(row, test.span) {
                Some(expr) => CaseArg::Value(expr),
                None => CaseArg::Invalid(format!(
                    "`#[Data]` row `{}` is not a runtime value",
                    attr_value_label(row)
                )),
            };
            TestCase {
                fn_name: test.name.clone(),
                display: format!("{base}[{}]", attr_value_label(row)),
                arg,
                span: test.span,
            }
        })
        .collect()
}

/// Convert a `#[Data]` row literal to an expression to pass as the test argument. Scalars and lists
/// (recursively) are supported; other literal forms (map/set/enum/struct/type-ref) return `None` and
/// surface as a failing case.
fn attr_value_to_expr(value: &AttrValue, span: Span) -> Option<Expr> {
    Some(match value {
        AttrValue::Str(s) => Expr::Str {
            value: s.clone(),
            span,
        },
        AttrValue::Int(n) => Expr::Int { value: *n, span },
        AttrValue::Float(f) => Expr::Float { value: *f, span },
        AttrValue::Bool(b) => Expr::Bool { value: *b, span },
        AttrValue::List(items) => Expr::List {
            items: items
                .iter()
                .map(|item| attr_value_to_expr(item, span))
                .collect::<Option<Vec<_>>>()?,
            span,
        },
        _ => return None,
    })
}

/// A short label for a `#[Data]` row, used in the `name[row]` case display.
fn attr_value_label(value: &AttrValue) -> String {
    match value {
        AttrValue::Str(s) => format!("{s:?}"),
        AttrValue::Int(n) => n.to_string(),
        AttrValue::Float(f) => f.to_string(),
        AttrValue::Bool(b) => b.to_string(),
        AttrValue::List(items) => format!(
            "[{}]",
            items
                .iter()
                .map(attr_value_label)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => "?".to_string(),
    }
}

/// The rows of a `#[Data([…])]` attribute on `test`, if present — the elements of its list argument.
fn data_rows(test: &TierFn) -> Option<Vec<AttrValue>> {
    let attr = test
        .attrs
        .iter()
        .find(|a| a.name == noeta_ast::reflect::TEST_ATTR_DATA)?;
    attr.args.iter().find_map(|arg| match &arg.value {
        AttrValue::List(items) => Some(items.clone()),
        _ => None,
    })
}

/// Whether a test fn is marked `#[Skip]` — the runner reports it skipped and does not run it.
fn test_is_skipped(test: &TierFn) -> bool {
    test.attrs
        .iter()
        .any(|a| a.name == noeta_ast::reflect::TEST_ATTR_SKIP)
}

/// The report label for a skipped test: its display name, plus a `(reason)` when `#[Skip("reason")]`
/// gave one.
fn skip_label(test: &TierFn) -> String {
    let name = test_display_name(test);
    match string_attr(test, noeta_ast::reflect::TEST_ATTR_SKIP) {
        Some(reason) if !reason.is_empty() => format!("{name} ({reason})"),
        _ => name,
    }
}

/// A test's display name — the string in `#[Name("…")]` if present, else the fn's own name.
fn test_display_name(test: &TierFn) -> String {
    string_attr(test, noeta_ast::reflect::TEST_ATTR_NAME).unwrap_or_else(|| test.name.clone())
}

/// A test's group — the string in `#[Group("…")]` if present, for `--group` filtering.
fn test_group(test: &TierFn) -> Option<String> {
    string_attr(test, noeta_ast::reflect::TEST_ATTR_GROUP)
}

/// The first string-valued argument of the attribute named `name` on `test`, if any.
fn string_attr(test: &TierFn, name: &str) -> Option<String> {
    let attr = test.attrs.iter().find(|a| a.name == name)?;
    attr.args.iter().find_map(|arg| match &arg.value {
        AttrValue::Str(s) => Some(s.clone()),
        _ => None,
    })
}

/// Whether a top-level statement is tier-runner *setup* — a declaration or a global binding the
/// tests/benches may depend on — as opposed to the program's own "main" effects (which the
/// `noeta test`/`noeta bench` runners do not run; they run the tier fns, not the file).
fn is_tier_setup(stmt: &Stmt) -> bool {
    !matches!(
        stmt,
        Stmt::Echo { .. }
            | Stmt::Return { .. }
            | Stmt::If { .. }
            | Stmt::For { .. }
            | Stmt::While { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. }
            | Stmt::Expr { .. }
    )
}

/// A statement that calls fn `name` with `args`: `name(args…);`. Zero `args` is the ordinary
/// test/bench call; a single arg is a `#[Data]` row.
fn call_stmt(name: &str, args: Vec<Expr>, span: Span) -> Stmt {
    Stmt::Expr {
        expr: Expr::Call {
            callee: Box::new(Expr::Ident {
                name: name.to_string(),
                span,
            }),
            args,
            span,
        },
        span,
    }
}

/// The default test concurrency — the machine's available parallelism (1 if it cannot be queried).
fn default_jobs() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Run `cases` concurrently across `jobs` worker threads, each grabbing the next case by an atomic
/// index. By default every case runs; with `fail_fast` a failure sets a shared stop flag and the
/// workers drain out. Results are gathered with their original index and returned in declaration
/// order, so the report is deterministic regardless of completion order.
fn run_tests(
    setup: &[Stmt],
    cases: &[TestCase],
    span: Span,
    jobs: usize,
    fail_fast: bool,
) -> Vec<TestOutcome> {
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let results: Mutex<Vec<(usize, TestOutcome)>> = Mutex::new(Vec::with_capacity(cases.len()));

    thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    if fail_fast && stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= cases.len() {
                        break;
                    }
                    let outcome = run_one_test(setup, &cases[idx], span);
                    let failed = !outcome.passed;
                    results.lock().unwrap().push((idx, outcome));
                    if fail_fast && failed {
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            });
        }
    });

    let mut collected = results.into_inner().unwrap();
    collected.sort_by_key(|(idx, _)| *idx);
    collected.into_iter().map(|(_, outcome)| outcome).collect()
}

/// Run a single test case: synthesize `setup + <call the fn (with its data arg, if any)>`, run it in
/// a fresh real-host isolate, and read a nonzero exit / any diagnostic as a failure (the first
/// diagnostic — the assertion or panic — is the reported message). An invalid `#[Data]` row fails
/// without running. The synthesized program is a subset of the already-checked activated program
/// plus one call, so it cannot introduce new type errors; one is surfaced as a failure rather than
/// panicking the worker.
fn run_one_test(setup: &[Stmt], case: &TestCase, span: Span) -> TestOutcome {
    let args = match &case.arg {
        CaseArg::None => Vec::new(),
        CaseArg::Value(expr) => vec![expr.clone()],
        CaseArg::Invalid(message) => {
            return TestOutcome {
                name: case.display.clone(),
                passed: false,
                message: Some(message.clone()),
                stdout: String::new(),
            };
        }
    };
    let display = case.display.clone();
    let mut stmts = setup.to_vec();
    stmts.push(call_stmt(&case.fn_name, args, case.span));
    let program = Program { stmts, span };

    let checked = noeta_check::check_all(&program);
    if !checked.diagnostics.is_empty() {
        return TestOutcome {
            name: display,
            passed: false,
            message: Some(checked.diagnostics[0].message.clone()),
            stdout: String::new(),
        };
    }

    // `@test`/`@bench` compile a *separate* module per case (a different granularity than the
    // whole-file startup cache), so they don't participate in it — see `plans/startup-cache`. They
    // have no program pass-through args; a test sees the real process argv.
    match execute_real_host(&program, &checked, std::env::args().collect()) {
        // The `@test` runner reports the failing diagnostic; the trace is a `noeta run` affordance.
        Ok((result, _trace)) => {
            let passed = result.exit_code == 0 && result.diagnostics.is_empty();
            let message = (!passed).then(|| {
                result
                    .diagnostics
                    .first()
                    .map(|d| d.message.clone())
                    .unwrap_or_else(|| format!("exited with code {}", result.exit_code))
            });
            TestOutcome {
                name: display,
                passed,
                message,
                stdout: result.stdout,
            }
        }
        Err(err) => TestOutcome {
            name: display,
            passed: false,
            message: Some(err),
            stdout: String::new(),
        },
    }
}

/// Print the per-test report and the summary, returning the process exit code (success only when
/// every selected test ran and passed — a `#[Skip]`ped test does not fail the suite). Failing tests
/// show their message and any captured stdout; skipped tests are listed after. `total` counts every
/// selected test (run + skipped); `outcomes` are the runnable ones (fewer than run on `--fail-fast`).
/// `noeta test --json`: the machine-readable report — one JSON object on stdout with per-test
/// outcomes (name = the report label: fn name / `#[Name]`, `[row]`-suffixed for data cases), the
/// skipped labels, and the totals. The seam the editor's test explorer parses; same exit-code
/// semantics as the human report.
fn report_json(outcomes: &[TestOutcome], skipped: &[String], total: usize) -> ExitCode {
    let passed = outcomes.iter().filter(|o| o.passed).count();
    let failed = outcomes.len() - passed;
    let not_run = total.saturating_sub(skipped.len() + outcomes.len());
    let json = serde_json::json!({
        "tests": outcomes.iter().map(|o| serde_json::json!({
            "name": o.name,
            "passed": o.passed,
            "message": o.message,
            "stdout": o.stdout,
        })).collect::<Vec<_>>(),
        "skipped": skipped,
        "passed": passed,
        "failed": failed,
        "notRun": not_run,
        "total": total,
    });
    println!("{json}");
    let _ = io::stdout().flush();
    if failed == 0 && not_run == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn report(outcomes: &[TestOutcome], skipped: &[String], total: usize) -> ExitCode {
    let mut passed = 0usize;
    for outcome in outcomes {
        if outcome.passed {
            passed += 1;
            println!("  ok    {}", outcome.name);
        } else {
            println!("  FAIL  {}", outcome.name);
            if let Some(message) = &outcome.message {
                println!("        {message}");
            }
            for line in outcome.stdout.lines() {
                println!("        | {line}");
            }
        }
    }
    for name in skipped {
        println!("  skip  {name}");
    }

    let failed = outcomes.len() - passed;
    let not_run = total - skipped.len() - outcomes.len();
    println!();
    let mut parts = vec![format!("{passed} passed"), format!("{failed} failed")];
    if !skipped.is_empty() {
        parts.push(format!("{} skipped", skipped.len()));
    }
    if not_run > 0 {
        parts.push(format!("{not_run} not run (stopped early)"));
    }
    parts.push(format!("{total} total"));
    println!("{}", parts.join(", "));
    let _ = io::stdout().flush();

    if failed == 0 && not_run == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// The fallback iteration count when calibration cannot estimate per-iteration cost (a probe too
/// noisy to subtract). Small, because the runner executes *interpreted* code and measures at both
/// N and 2N (see [`cmd_bench`]).
const DEFAULT_BENCH_ITERATIONS: u64 = 200;

/// Calibration's target wall time for one measurement point, in nanoseconds (~50ms): long enough
/// to dwarf timer noise, short enough that the six runs a two-point/min-of-three measurement
/// takes stay comfortable.
const BENCH_TARGET_POINT_NS: f64 = 50_000_000.0;

/// Calibration's iteration-count ceiling — a nanosecond-scale body would otherwise be sized into
/// millions of synthesized call statements (the harness materializes one `Stmt` per iteration).
const BENCH_MAX_ITERATIONS: u64 = 100_000;

/// One benchmark's result — the human report, the `--json` seam, and the baseline file all read
/// from this.
struct BenchOutcome {
    name: String,
    iterations: u64,
    /// `Some` on success; `None` when the bench failed (see `message`).
    per_iter_ns: Option<f64>,
    message: Option<String>,
    /// Percent change vs the `--baseline` entry, when one exists for this bench.
    baseline_delta_pct: Option<f64>,
}

/// `noeta bench <FILE>` — discover the program's `@bench` blocks (object-model slice 6) and measure
/// each. Unlike `noeta test`, benchmarks run **sequentially** (concurrency would corrupt timings).
/// Each bench's per-iteration cost is estimated by a **two-point** measurement: the fn is invoked N
/// and 2N times in fresh isolates and the per-iteration time is `(t(2N) − t(N)) / N`, which cancels
/// the fixed per-run overhead (runtime startup, global/setup evaluation, IR lowering — all identical
/// between the two runs). N comes from `--iterations`, else the per-bench `@bench(iterations: N)`
/// directive, else **calibration** (a two-run probe sizes N so one point takes
/// [`BENCH_TARGET_POINT_NS`]). `--name` filters (exact fn-name match, repeatable); `--json`
/// reports one machine-readable object; `--save-baseline`/`--baseline` persist and diff runs
/// (per entry file, in the noeta cache — timings are machine-local).
#[allow(clippy::too_many_arguments)]
fn cmd_bench(
    file: &std::path::Path,
    iterations_override: Option<u64>,
    names: &[String],
    json: bool,
    save_baseline: &Option<String>,
    baseline: &Option<String>,
    max_regress: Option<f64>,
    target: &Option<String>,
) -> ExitCode {
    if let Some(code) = compose::maybe_delegate(file) {
        return code;
    }
    if let Some(code) = target_gate(file, target, "bench") {
        return code;
    }
    let linked = match load_linked(file) {
        Ok(linked) => linked,
        Err(code) => return code,
    };

    // Activate the `bench` tier: inline its `@bench` blocks as ordinary top-level declarations and
    // collect the bench fns. An unknown-tier block is an E0036.
    // The target's provider selection (provider dispatch): `bench = "<pkg>"` in the target's
    // tiers map hands this tier to that package's `@tier(bench)` runner instead of the native
    // one. Default (no target, or `"std"`) keeps the native path below.
    let providers = match resolve_providers(file, target) {
        Ok(map) => map,
        Err(err) => {
            eprintln!("lang: {err}");
            return ExitCode::from(2);
        }
    };
    let activated = noeta_check::activate_tiers_with(&linked.program, &["bench"], &providers);
    match activated.registry.resolve_provider("bench", &providers) {
        Ok(noeta_check::ResolvedProvider::Extension) => {}
        Ok(noeta_check::ResolvedProvider::Declared(d)) => {
            let tier = d.clone();
            return run_declared_tier("bench", &linked, activated, tier);
        }
        Err(err) => {
            eprintln!("lang: {err}");
            return ExitCode::from(2);
        }
    }
    if !activated.diagnostics.is_empty() {
        emit_diagnostics_mapped(&linked.sources, activated.diagnostics.iter());
        return ExitCode::from(1);
    }

    // Type-check once, so a broken benchmark is a compile error reported here rather than inside
    // every per-bench run.
    let checked = noeta_check::check_all(&activated.program);
    if !checked.diagnostics.is_empty() {
        emit_diagnostics_mapped(&linked.sources, checked.diagnostics.iter());
        return ExitCode::from(1);
    }

    // `--name` keeps only exact fn-name matches — the single-benchmark seam editors use.
    let selected: Vec<&TierFn> = if names.is_empty() {
        activated.benches.iter().collect()
    } else {
        activated
            .benches
            .iter()
            .filter(|b| names.iter().any(|n| n == &b.name))
            .collect()
    };
    if selected.is_empty() {
        println!("no benchmarks found");
        return ExitCode::SUCCESS;
    }

    let setup: Vec<Stmt> = activated
        .program
        .stmts
        .iter()
        .filter(|s| is_tier_setup(s))
        .cloned()
        .collect();

    let base = match baseline {
        Some(name) => match load_bench_baseline(file, name) {
            Ok(map) => Some(map),
            Err(err) => {
                eprintln!("lang: {err}");
                return ExitCode::from(2);
            }
        },
        None => None,
    };

    let total = selected.len();
    if !json {
        println!("running {total} benchmark{}", plural(total));
    }

    let mut outcomes: Vec<BenchOutcome> = Vec::with_capacity(total);
    for bench in &selected {
        let n = iterations_override
            .or_else(|| iterations_arg(bench))
            .map(|n| n.max(1))
            .unwrap_or_else(|| calibrate_iterations(&setup, bench));
        let mut outcome = match (
            measure_iterations(&setup, bench, n, 3),
            measure_iterations(&setup, bench, n.saturating_mul(2), 3),
        ) {
            (Ok(t1), Ok(t2)) => BenchOutcome {
                name: bench.name.clone(),
                iterations: n,
                per_iter_ns: Some(
                    ((t2.as_nanos() as f64 - t1.as_nanos() as f64) / n as f64).max(0.0),
                ),
                message: None,
                baseline_delta_pct: None,
            },
            (Err(msg), _) | (_, Err(msg)) => BenchOutcome {
                name: bench.name.clone(),
                iterations: n,
                per_iter_ns: None,
                message: Some(msg),
                baseline_delta_pct: None,
            },
        };
        if let (Some(base), Some(cur)) = (&base, outcome.per_iter_ns)
            && let Some(prev) = base.get(&outcome.name).copied()
            && prev > 0.0
        {
            outcome.baseline_delta_pct = Some((cur - prev) / prev * 100.0);
        }
        if !json {
            print_bench_outcome(&outcome, baseline.as_deref());
        }
        outcomes.push(outcome);
    }

    if let Some(name) = save_baseline
        && let Err(err) = save_bench_baseline(file, name, &outcomes)
    {
        eprintln!("lang: cannot save baseline `{name}`: {err}");
        return ExitCode::from(2);
    }

    let failed = outcomes.iter().filter(|o| o.per_iter_ns.is_none()).count();
    // The CI gate: any bench past the allowed regression fails the run (their names on stderr —
    // the JSON stays a pure result object either way).
    let regressed: Vec<&BenchOutcome> = match max_regress {
        Some(limit) => outcomes
            .iter()
            .filter(|o| o.baseline_delta_pct.is_some_and(|pct| pct > limit))
            .collect(),
        None => Vec::new(),
    };
    if json {
        let out = serde_json::json!({
            "benches": outcomes.iter().map(|o| serde_json::json!({
                "name": o.name,
                "iterations": o.iterations,
                "perIterNs": o.per_iter_ns,
                "message": o.message,
                "baselineDeltaPct": o.baseline_delta_pct,
            })).collect::<Vec<_>>(),
            "ran": total - failed,
            "failed": failed,
            "regressed": regressed.len(),
            "total": total,
        });
        println!("{out}");
    } else {
        println!();
        println!("{} ran, {failed} failed, {total} total", total - failed,);
    }
    for o in &regressed {
        eprintln!(
            "lang: `{}` regressed {:+.1}% (limit {:+.1}%)",
            o.name,
            o.baseline_delta_pct.unwrap_or_default(),
            max_regress.unwrap_or_default(),
        );
    }
    let _ = io::stdout().flush();
    if failed == 0 && regressed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// One line of the human bench report: per-iteration time, iteration count, and — under
/// `--baseline` — the percent delta against the named baseline.
fn print_bench_outcome(outcome: &BenchOutcome, baseline: Option<&str>) {
    match (outcome.per_iter_ns, &outcome.message) {
        (Some(per_ns), _) => {
            let delta = match (outcome.baseline_delta_pct, baseline) {
                (Some(pct), Some(name)) => format!("  ({pct:+.1}% vs {name})"),
                _ => String::new(),
            };
            println!(
                "  {:<28} {:>11}/iter  ({} iterations){delta}",
                outcome.name,
                fmt_per_iter(per_ns),
                outcome.iterations,
            );
        }
        (None, msg) => {
            println!(
                "  {:<28} FAILED: {}",
                outcome.name,
                msg.as_deref().unwrap_or("unknown")
            );
        }
    }
}

/// Size the iteration count so **one measurement run** takes at least
/// [`BENCH_TARGET_POINT_NS`] (the criterion/go-bench growth loop): run once at a small count and
/// scale up toward the target until a run crosses it (or the count hits the ceiling). Sizing on
/// the *total* run time is deliberately simple — startup rides along, but once a run is ≥ 50ms
/// the two-point subtraction in the real measurement operates far above scheduler/startup noise,
/// which is the property that makes the reported per-iteration number stable. A failing probe
/// falls back to [`DEFAULT_BENCH_ITERATIONS`] (the bench fails identically in the real
/// measurement, where it is reported).
fn calibrate_iterations(setup: &[Stmt], bench: &TierFn) -> u64 {
    let mut n: u64 = 64;
    loop {
        let Ok(t) = measure_iterations(setup, bench, n, 1) else {
            return DEFAULT_BENCH_ITERATIONS;
        };
        let elapsed = t.as_nanos().max(1) as f64;
        if elapsed >= BENCH_TARGET_POINT_NS || n >= BENCH_MAX_ITERATIONS {
            return n;
        }
        // Jump toward the target in one step, but at most 16× (a first run dominated by startup
        // under-reports per-iteration cost, so an unbounded jump could overshoot the target run
        // time badly); at least 2× so the loop always terminates.
        let factor = (BENCH_TARGET_POINT_NS / elapsed).ceil().clamp(2.0, 16.0) as u64;
        n = n.saturating_mul(factor).min(BENCH_MAX_ITERATIONS);
    }
}

/// The baseline file for `entry` + `name`: per-entry (hashed canonical path) under the noeta
/// cache directory — baselines are machine-local timings, not project artifacts.
fn bench_baseline_path(entry: &std::path::Path, name: &str) -> Result<PathBuf, String> {
    use std::hash::{Hash, Hasher};
    let canonical = entry
        .canonicalize()
        .map_err(|e| format!("cannot resolve `{}`: {e}", entry.display()))?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    let dir = noeta_cache::Cache::locate()
        .ok_or_else(|| "no cache directory available for baselines".to_string())?
        .join("bench-baselines");
    Ok(dir.join(format!("{:016x}-{name}.json", hasher.finish())))
}

/// Persist a run's successful measurements as the named baseline (`name → per-iteration ns`).
fn save_bench_baseline(
    entry: &std::path::Path,
    name: &str,
    outcomes: &[BenchOutcome],
) -> Result<(), String> {
    let path = bench_baseline_path(entry, name)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let map: serde_json::Map<String, serde_json::Value> = outcomes
        .iter()
        .filter_map(|o| {
            o.per_iter_ns
                .map(|ns| (o.name.clone(), serde_json::json!(ns)))
        })
        .collect();
    let doc = serde_json::json!({ "benches": map });
    std::fs::write(&path, doc.to_string()).map_err(|e| e.to_string())
}

/// Load the named baseline for `entry` (`name → per-iteration ns`).
fn load_bench_baseline(
    entry: &std::path::Path,
    name: &str,
) -> Result<std::collections::HashMap<String, f64>, String> {
    let path = bench_baseline_path(entry, name)?;
    let text = std::fs::read_to_string(&path).map_err(|_| {
        format!(
            "no baseline `{name}` saved for this file (expected `{}`)",
            path.display()
        )
    })?;
    let doc: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("corrupt baseline `{name}`: {e}"))?;
    Ok(doc["benches"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f)))
                .collect()
        })
        .unwrap_or_default())
}

/// The bench's `#[Bench(iterations: N)]` knob, if present and positive — the per-bench override of
/// the default iteration count. The attribute is either written on the fn directly or stamped from
/// the block's `@bench(…)` directive args (activation's desugar), and the construction gate has
/// already validated it, so this only reads: `iterations` is `Bench`'s sole field, bound
/// positionally (`#[Bench(1000)]` / `@bench(1000)`) or by name.
fn iterations_arg(bench: &TierFn) -> Option<u64> {
    let attr = bench
        .attrs
        .iter()
        .find(|a| a.name == noeta_ast::reflect::TIER_ATTR_BENCH)?;
    let value = attr
        .args
        .iter()
        .find(|arg| matches!(&arg.name, Some(n) if n == "iterations") || arg.name.is_none())?;
    match value.value {
        AttrValue::Int(n) if n > 0 => Some(n as u64),
        _ => None,
    }
}

/// Measure executing `bench` `n` times: synthesize `setup + n×<call the bench fn>`, then run it in a
/// fresh real-host isolate, timing **only execution** (IR lowering is done untimed, before the
/// clock starts). A discarded warm-up run plus the minimum of three measured runs damps noise. A
/// nonzero exit / any diagnostic (a panic in the bench body) is a failure, surfaced as `Err`.
fn measure_iterations(
    setup: &[Stmt],
    bench: &TierFn,
    n: u64,
    runs: u32,
) -> Result<std::time::Duration, String> {
    // The measured program is `setup` + one counted loop over the bench call — a *loop*, not `n`
    // synthesized call statements, so the VM treats both measurement points identically (the same
    // loop body crosses the JIT threshold the same way at N and 2N; only the trip count differs,
    // and the warm-up cost cancels in the two-point subtraction exactly like startup does). The
    // range list materializes once, before the clock-relevant body, and scales linearly with `n`,
    // so its cost lands as a tiny constant inside the per-iteration figure rather than skewing
    // the subtraction.
    let mut stmts = setup.to_vec();
    let span = bench.span;
    stmts.push(Stmt::For {
        pattern: noeta_ast::ForPattern::Single {
            name: "__bench_iter".to_string(),
            name_span: span,
        },
        iterable: Expr::Range {
            start: Box::new(Expr::Int { value: 0, span }),
            end: Box::new(Expr::Int {
                value: n as i64,
                span,
            }),
            inclusive: false,
            span,
        },
        body: vec![call_stmt(&bench.name, Vec::new(), span)],
        span,
    });
    let program = Program {
        stmts,
        span: bench.span,
    };

    let checked = noeta_check::check_all(&program);
    if !checked.diagnostics.is_empty() {
        return Err(checked.diagnostics[0].message.clone());
    }

    // Take the minimum of three runs: `min` is the standard robust estimator (the fastest run is
    // the one least perturbed by scheduler/GC/OS noise) and inherently discards the cold first run,
    // so no separate warm-up is needed.
    let mut best: Option<std::time::Duration> = None;
    for _ in 0..runs.max(1) {
        let (result, elapsed) = bench_execute(&program, &checked)?;
        if result.exit_code != 0 || !result.diagnostics.is_empty() {
            return Err(result
                .diagnostics
                .first()
                .map(|d| d.message.clone())
                .unwrap_or_else(|| format!("exited with code {}", result.exit_code)));
        }
        best = Some(best.map_or(elapsed, |b| b.min(elapsed)));
    }
    Ok(best.expect("at least one measured run"))
}

/// Lower a program for the real host (untimed) and execute it, returning the result and the
/// **execution-only** wall-clock duration (lowering excluded). Mirrors [`execute_real_host`]'s
/// pipeline so a benchmark runs the same Core-IR path a normal `noeta run` does.
fn bench_execute(
    program: &Program,
    checked: &noeta_check::Checked,
) -> Result<(noeta_backend::RunResult, std::time::Duration), String> {
    let host =
        noeta_runtime::RealHost::new().map_err(|err| format!("cannot start the runtime: {err}"))?;
    // Compile to bytecode untimed (isolates I.4a — the real path is the VM), then time execution
    // alone, so the measurement excludes both lowering and bytecode generation.
    let module = compile_real(program, checked)?;
    let start = Instant::now();
    let result = VmBackend::new().run_module_with_host(&module, Box::new(host));
    Ok((result, start.elapsed()))
}

/// Format a per-iteration duration (in nanoseconds) with an adaptive unit, so a fast op reads in
/// `ns` and a slow one in `ms`/`s`.
fn fmt_per_iter(ns: f64) -> String {
    if ns < 1_000.0 {
        format!("{ns:.0} ns")
    } else if ns < 1_000_000.0 {
        format!("{:.2} µs", ns / 1_000.0)
    } else if ns < 1_000_000_000.0 {
        format!("{:.2} ms", ns / 1_000_000.0)
    } else {
        format!("{:.2} s", ns / 1_000_000_000.0)
    }
}

/// `noeta doc <FILE>` — extract the program's `@doc { … }` text blocks (object-model slice 6f) to
/// stdout, in source order. Each block's verbatim body is dedented (the common leading indentation
/// and the surrounding blank lines from sitting inside `@doc { … }` are stripped) and preceded by an
/// HTML-comment header noting its source location — valid markdown that renders to nothing. The
/// program is not type-checked or run; doc extraction works on a parse alone, so docs can be pulled
/// from work-in-progress code.
/// Fetch a published package's stored documentation artifact from the registry: `name` picks the
/// highest published version, `name@1.2.0` an exact one. Prints the `docs.json` to stdout, or —
/// with `--out` — writes it and renders the Markdown tree from it (no source needed).
fn cmd_doc_package(spec: &str, out: &Option<PathBuf>) -> ExitCode {
    let (name, version) = match spec.split_once('@') {
        Some((n, v)) => match semver::Version::parse(v) {
            Ok(version) => (n.to_string(), Some(version)),
            Err(err) => {
                eprintln!("lang: `{v}` is not a version: {err}");
                return ExitCode::from(2);
            }
        },
        None => (spec.to_string(), None),
    };
    let index = match registry::open_default() {
        Ok(index) => index,
        Err(err) => {
            eprintln!("lang: {err}");
            return ExitCode::from(1);
        }
    };
    let version = match version {
        Some(v) => v,
        None => {
            // Highest published version, matching how registry resolution selects.
            let mut releases = match index.releases(&name) {
                Ok(r) => r,
                Err(err) => {
                    eprintln!("lang: {err}");
                    return ExitCode::from(1);
                }
            };
            releases.sort_by(|a, b| b.version.cmp(&a.version));
            match releases.first() {
                Some(r) => r.version.clone(),
                None => {
                    eprintln!("lang: registry has no package `{name}`");
                    return ExitCode::from(1);
                }
            }
        }
    };
    let docs = match index.docs(&name, &version) {
        Ok(Some(docs)) => docs,
        Ok(None) => {
            eprintln!("lang: no docs stored for `{name}@{version}`");
            return ExitCode::from(1);
        }
        Err(err) => {
            eprintln!("lang: {err}");
            return ExitCode::from(1);
        }
    };
    // The hosted registry renders these same docs in a browser; point there when one is
    // configured. Always on **stderr** so it never pollutes the `docs.json` piped to stdout.
    if let Some(web) = registry_web_docs_url(&name, &version) {
        eprintln!("view online: {web}");
    }
    match out {
        Some(dir) => match docgen::render_json_to(dir, &docs) {
            Ok(done) => {
                println!(
                    "rendered `{name}@{version}` docs ({} module{}, {} declaration{}) → {}",
                    done.modules,
                    plural(done.modules),
                    done.decls,
                    plural(done.decls),
                    dir.display(),
                );
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("lang: {err}");
                ExitCode::from(1)
            }
        },
        None => {
            print!("{docs}");
            let _ = io::stdout().flush();
            ExitCode::SUCCESS
        }
    }
}

/// The hosted registry's browser URL for a release's rendered docs, when a hosted registry is
/// configured (`NOETA_REGISTRY_URL`). `None` for the file-backed local index (no web surface).
fn registry_web_docs_url(name: &str, version: &semver::Version) -> Option<String> {
    let base = std::env::var("NOETA_REGISTRY_URL").ok()?;
    Some(web_docs_url(&base, name, &version.to_string()))
}

/// Format the registry's browser docs URL: the web UI lives at the registry root (the JSON API is
/// under `/v1`), and `name` already carries the `company/package` slash, so the path is
/// `{base}/{name}/{version}/docs` with any trailing slash on `base` trimmed.
fn web_docs_url(base: &str, name: &str, version: &str) -> String {
    format!("{}/{name}/{version}/docs", base.trim_end_matches('/'))
}

fn cmd_doc(
    file: &Option<PathBuf>,
    package: &Option<String>,
    out: &Option<PathBuf>,
    target: &Option<String>,
) -> ExitCode {
    // `--package`: the registry-fetch path — a published release's stored artifact, no local
    // source involved.
    if let Some(spec) = package {
        return cmd_doc_package(spec, out);
    }
    let Some(file) = file else {
        eprintln!(
            "lang: `noeta doc` needs a `.noe` file (or `--package <NAME>` for published docs)"
        );
        return ExitCode::from(2);
    };
    let file = file.as_path();
    if let Some(code) = compose::maybe_delegate(file) {
        return code;
    }
    if let Some(code) = target_gate(file, target, "doc") {
        return code;
    }
    // `--out`: the generator path — a registry-ready artifact from a bare parse, before (and
    // independent of) the extraction/provider machinery below.
    if let Some(out_dir) = out {
        return match docgen::generate(file, out_dir) {
            Ok(done) => {
                for skipped in &done.skipped {
                    eprintln!("lang: skipped `{skipped}` (does not parse)");
                }
                println!(
                    "documented {} module{} ({} declaration{}) → {}",
                    done.modules,
                    plural(done.modules),
                    done.decls,
                    plural(done.decls),
                    out_dir.display(),
                );
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("lang: {err}");
                ExitCode::from(2)
            }
        };
    }
    let linked = match load_linked(file) {
        Ok(linked) => linked,
        Err(code) => return code,
    };

    // The target's provider selection (provider dispatch): `doc = "<pkg>"` hands documentation
    // to that package's `@tier(doc)` runner — activation stamps every adjacency-attached block as
    // `#[Doc]`, so the runner reads the documented symbols through `attributes_of::<Doc>()` (the
    // doc-site-generator seam). Default keeps the native extractor below.
    let providers = match resolve_providers(file, target) {
        Ok(map) => map,
        Err(err) => {
            eprintln!("lang: {err}");
            return ExitCode::from(2);
        }
    };
    if providers.get("doc").is_some_and(|p| p != "std") {
        let activated = noeta_check::activate_tiers_with(&linked.program, &["doc"], &providers);
        match activated.registry.resolve_provider("doc", &providers) {
            Ok(noeta_check::ResolvedProvider::Declared(d)) => {
                let tier = d.clone();
                return run_declared_tier("doc", &linked, activated, tier);
            }
            Ok(noeta_check::ResolvedProvider::Extension) => {}
            Err(err) => {
                eprintln!("lang: {err}");
                return ExitCode::from(2);
            }
        }
    }

    let docs = noeta_check::resolve_docs(&linked.program);
    if docs.is_empty() {
        eprintln!("lang: no `@doc` blocks found");
        return ExitCode::SUCCESS;
    }

    let mut out = String::new();
    for (i, doc) in docs.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let source = linked.sources.source(doc.span.source);
        let line = source.line_col(doc.span.start).line;
        // A declaration-attached block's header carries the documented symbol (adjacency-resolved);
        // an unattached block's header is the bare location, exactly as before.
        let header = match &doc.target {
            noeta_check::DocTarget::Decl { name, .. } => {
                format!("<!-- {}:{} · {} -->\n", source.name(), line, name)
            }
            _ => format!("<!-- {}:{} -->\n", source.name(), line),
        };
        out.push_str(&header);
        out.push_str(&noeta_check::dedent_doc(&doc.text));
        out.push('\n');
    }
    print!("{out}");
    let _ = io::stdout().flush();
    ExitCode::SUCCESS
}

/// Whether an entry was consumed (evaluated or reported) or is still incomplete and needs
/// more input (multiline continuation).
enum ReplStep {
    Consumed,
    Incomplete,
}

/// The environment the REPL runs against: a **real** host + wall-clock executor, so `fs`, `time`,
/// `env`, and `uuid()` work at the prompt against the real machine — exactly as `noeta run` does. Built
/// fresh on `:reset`. (The deterministic sandbox exists only to make the differential oracle
/// reproducible; it is not what an interactive prompt wants.) A real-thread `isolate f(args)` falls
/// back to a cooperative task here, since the session does not arm the parallel-isolate path.
fn real_repl_env() -> noeta_vm::HostFactory {
    Box::new(|| {
        let host: Box<dyn noeta_stdlib::Host> =
            Box::new(noeta_runtime::RealHost::new().expect("cannot start the REPL's runtime"));
        let executor: Box<dyn noeta_stdlib::Executor> = Box::new(
            noeta_runtime::RealExecutor::new().expect("cannot start the REPL's async executor"),
        );
        (host, executor)
    })
}

/// Load, check, compile, and RUN the `--load` bootstrap, returning the adopted session and the
/// bootstrap's sources (the REPL's entry ids continue after them). The bootstrap is a *file*, so
/// it is always fully checked — as it would be under `noeta run` — regardless of `--no-check`
/// (which governs prompt entries); with checking on, the bootstrap's own checker session carries
/// forward, so entries check against everything it declared and bound. Isolates in a bootstrap run
/// cooperatively (the session's execution model). Any failure — unreadable file, load/check
/// diagnostics, a runtime abort — exits with diagnostics instead of opening a broken prompt.
fn repl_bootstrap(
    path: &std::path::Path,
    checker: &mut Option<noeta_check::SessionChecker>,
) -> Result<(VmSession, Vec<Source>), ExitCode> {
    let linked = match noeta_loader::load(path) {
        Err(err) => {
            eprintln!("noeta: cannot read {}: {err}", path.display());
            return Err(ExitCode::from(2));
        }
        Ok(Err(load_diagnostics)) => {
            for ld in &load_diagnostics {
                eprint!("{}", render(&ld.source, &ld.diagnostic));
            }
            return Err(ExitCode::FAILURE);
        }
        Ok(Ok(linked)) => linked,
    };

    // Always checked (it is a file); the session flavor keeps the checker when the prompt wants it.
    let (checked, session_checker) = noeta_check::check_all_session(&linked.program);
    if !checked.diagnostics.is_empty() {
        emit_diagnostics_mapped(&linked.sources, checked.diagnostics.iter());
        return Err(ExitCode::FAILURE);
    }
    if checker.is_some() {
        *checker = Some(session_checker);
    }

    // Cooperative isolates + no debug info: the prompt's own execution model.
    let (module, compiler) = match noeta_compiler::compile_with_sites_session(
        &linked.program,
        checked.sites,
        false,
        false,
    ) {
        Ok(pair) => pair,
        Err(u) => {
            eprintln!(
                "noeta: internal error: the VM cannot compile this program: {}",
                u.reason
            );
            return Err(ExitCode::FAILURE);
        }
    };
    let (session, out) = VmSession::adopted(&module, compiler, real_repl_env());
    print!("{}", out.stdout);
    let _ = io::stdout().flush();
    if !out.diagnostics.is_empty() {
        // A bootstrap that aborts is a broken app context — fail fast, exactly like `noeta run`.
        emit_diagnostics_mapped(&linked.sources, out.diagnostics.iter());
        emit_trace(&out.trace, &linked.sources);
        return Err(ExitCode::FAILURE);
    }
    eprintln!("(loaded {})", path.display());
    Ok((session, linked.sources.into_sources()))
}

fn cmd_repl(check: bool, load: Option<PathBuf>) -> ExitCode {
    // A `--load` bootstrap is a program run — a native-dep app's REPL must be the composed one.
    if let Some(file) = &load
        && let Some(code) = compose::maybe_delegate(file)
    {
        return code;
    }
    let stdin = io::stdin();
    // The optional per-entry type checker (session-checker C2): `Some` = every entry is checked
    // against the accumulated session before it runs; an erroring entry prints diagnostics and is
    // skipped (and commits nothing — `check_entry` is transactional). Toggleable at the prompt.
    // A `--load` bootstrap replaces the fresh checker with one seeded from the bootstrap's own
    // whole-program check, so entries check against everything the bootstrap declared.
    let mut checker: Option<noeta_check::SessionChecker> =
        check.then(noeta_check::SessionChecker::new);
    // A bootstrapped session ("tinker"): the file runs to completion as entry 0 — checked,
    // imports resolved — and the prompt opens over its final state. Its sources seed the entry
    // list so later `SourceId`s continue past them (a trace into a bootstrap function renders
    // against its real text).
    let (mut session, preloaded_sources) = match &load {
        None => (VmSession::new(real_repl_env()), Vec::new()),
        Some(path) => match repl_bootstrap(path, &mut checker) {
            Ok(booted) => booted,
            Err(code) => return code,
        },
    };
    // Whether SITE-DRIVEN codegen is still sound (session-checker C5): true only while the checker
    // has seen every entry of the session. `:check off` clears it PERMANENTLY — precise destructor
    // relevance derived from a registry that missed an unchecked entry's `destruct` class could
    // skip a destructor, so once any entry runs unchecked the session stays on conservative
    // codegen even if checking is turned back on (diagnostics return; the codegen upgrade doesn't).
    let mut precise_codegen = check;
    let mut buffer = String::new();
    // Each evaluated entry is parsed with a **distinct** `SourceId` (its index here) and kept, so a
    // stack trace into a function defined in an *earlier* entry renders against that entry's real text
    // and line — rather than degrading to name-only, as it did when every entry reused
    // `SourceId::FIRST` (REPL-on-VM follow-on). Only entries that actually run are kept; a syntax-error
    // entry compiles nothing, so no future trace can reference it.
    let mut sources: Vec<Source> = preloaded_sources;
    eprint!("lang repl — type a statement, Ctrl-D to exit\n» ");
    let _ = io::stderr().flush();

    eprintln!("type :help for commands");
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        // Skip blank lines when nothing is pending.
        if buffer.is_empty() && line.trim().is_empty() {
            eprint!("» ");
            let _ = io::stderr().flush();
            continue;
        }
        // A `:`-prefixed line (when nothing is pending) is a REPL meta-command — tooling that lives
        // outside the language grammar (`:type`, `:drop`, `:bindings`, `:reset`, `:help`, `:quit`).
        if buffer.is_empty() && line.trim_start().starts_with(':') {
            if repl_meta(
                &mut session,
                &mut checker,
                &mut precise_codegen,
                line.trim(),
                &sources,
            ) == MetaOutcome::Quit
            {
                break;
            }
            eprint!("» ");
            let _ = io::stderr().flush();
            continue;
        }
        if !buffer.is_empty() {
            buffer.push('\n');
        }
        buffer.push_str(&line);

        match repl_step(
            &mut session,
            &mut checker,
            precise_codegen,
            &buffer,
            &mut sources,
        ) {
            ReplStep::Consumed => {
                buffer.clear();
                eprint!("» ");
            }
            // Keep the buffer and read another line; show a continuation prompt.
            ReplStep::Incomplete => eprint!("… "),
        }
        let _ = io::stderr().flush();
    }
    eprintln!();
    ExitCode::SUCCESS
}

/// Whether a meta-command asked to leave the REPL.
#[derive(PartialEq)]
enum MetaOutcome {
    Continue,
    Quit,
}

/// Handle a `:`-prefixed REPL meta-command. These are REPL *tooling*, deliberately outside the
/// language grammar (the language itself has no manual `drop`/`type` keyword): the REPL keeps
/// top-level bindings alive across entries — extended lifetime, unlike compiled code's last-use
/// destruction — so `:drop` is how a destructor is observed or an object reclaimed interactively,
/// and `:type` reports a value's runtime type in a session that runs no checker.
fn repl_meta(
    session: &mut VmSession,
    checker: &mut Option<noeta_check::SessionChecker>,
    precise_codegen: &mut bool,
    line: &str,
    sources: &[Source],
) -> MetaOutcome {
    let body = line.strip_prefix(':').unwrap_or(line);
    let mut parts = body.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    match cmd {
        "quit" | "q" => return MetaOutcome::Quit,
        "help" | "h" | "?" => print_repl_help(),
        "reset" => {
            session.reset();
            // The checker's session must reset with the runtime's — its registries describe
            // bindings/types that no longer exist. A reset session with checking on is fully
            // checked again, so precise codegen is earned back.
            if checker.is_some() {
                *checker = Some(noeta_check::SessionChecker::new());
                *precise_codegen = true;
            }
            eprintln!("(session reset)");
        }
        // Toggle per-entry type checking (session-checker C2). Turning it on mid-session starts
        // from an EMPTY typing environment: earlier (unchecked) bindings are simply unknown to it,
        // which the checker's unknown-ident tolerance treats as runtime-deferred — degraded
        // precision, never false errors.
        "check" => match arg {
            "on" => {
                if checker.is_none() {
                    *checker = Some(noeta_check::SessionChecker::new());
                }
                // Diagnostics return; the codegen upgrade does not — see `precise_codegen`.
                eprintln!("(type checking on — entries are checked before running)");
            }
            "off" => {
                *checker = None;
                *precise_codegen = false;
                eprintln!("(type checking off)");
            }
            _ => eprintln!(
                "type checking is {} — usage: :check on|off",
                if checker.is_some() { "on" } else { "off" }
            ),
        },
        "bindings" | "b" => {
            let names = session.binding_names();
            if names.is_empty() {
                eprintln!("(no bindings)");
            } else {
                println!("{}", names.join(", "));
                let _ = io::stdout().flush();
            }
        }
        "drop" | "free" => {
            if arg.is_empty() {
                eprintln!("usage: :drop <name>");
            } else {
                let (found, out) = session.drop_binding(arg);
                print!("{}", out.stdout);
                let _ = io::stdout().flush();
                if found {
                    eprintln!("(dropped `{arg}`)");
                } else {
                    eprintln!("no binding named `{arg}`");
                }
            }
        }
        "type" | "t" => {
            if arg.is_empty() {
                eprintln!("usage: :type <expr>");
            } else {
                repl_type(session, arg, sources);
            }
        }
        other => eprintln!("unknown command `:{other}` — try :help"),
    }
    MetaOutcome::Continue
}

/// `:type <expr>` — parse `expr`, evaluate it in the session, and print its runtime type. Evaluating
/// the expression may abort (`:type boom()`); the trace then resolves against every entry's source, so
/// a `:type` id (the next index) is added to the render map without being *persisted* — a `:type`
/// query defines nothing, so no later trace can reference it.
fn repl_type(session: &mut VmSession, expr: &str, sources: &[Source]) {
    let id = SourceId(sources.len() as u32);
    let fragment = parse_fragment(id, "<repl-type>", expr);
    if !fragment.diagnostics.is_empty() {
        emit_diagnostics(&fragment.source, fragment.diagnostics.iter());
        return;
    }
    let out = session.type_of(&fragment.program);
    print!("{}", out.stdout);
    let _ = io::stdout().flush();
    // Render diagnostics / any abort trace against all entries plus this `:type` source.
    let mut map_sources = sources.to_vec();
    map_sources.push(fragment.source);
    let map = SourceMap::new(map_sources);
    if !out.diagnostics.is_empty() {
        emit_diagnostics_mapped(&map, out.diagnostics.iter());
        emit_trace(&out.trace, &map);
    } else if let Some(ty) = out.value {
        println!("{ty}");
        let _ = io::stdout().flush();
    }
}

fn print_repl_help() {
    eprintln!("REPL commands:");
    eprintln!("  :type <expr>   show the runtime type of an expression (evaluates it)");
    eprintln!("  :drop <name>   run a binding's destructor now and unbind it (alias :free)");
    eprintln!("  :bindings      list the live bindings");
    eprintln!("  :reset         clear all bindings and start fresh");
    eprintln!("  :check on|off  type-check entries before running them (skip on error)");
    eprintln!("  :help          show this help");
    eprintln!("  :quit          exit the REPL (or Ctrl-D)");
}

/// Evaluate one checked-clean entry: with the checker on AND every prior entry checked
/// (`precise_codegen`), the entry compiles with the checker's accumulated site bundle — the same
/// site-driven codegen the file pipeline runs (session-checker C5); otherwise the conservative
/// checkerless codegen, which is always sound.
fn eval_entry(
    session: &mut VmSession,
    checker: &Option<noeta_check::SessionChecker>,
    precise_codegen: bool,
    program: &noeta_ast::Program,
) -> noeta_vm::SessionOutput {
    match checker {
        Some(checker) if precise_codegen => {
            session.eval_checked(program, &checker.sites_snapshot())
        }
        _ => session.eval(program),
    }
}

/// The `--check` gate (session-checker C2): type-check one parsed entry against the accumulated
/// session. Returns whether the entry should RUN — `true` with no checker, or when the entry has
/// no error-severity diagnostics (warnings print and the entry still runs). Errors render against
/// the entry's own source and the entry is skipped; `check_entry`'s transactionality means a
/// skipped entry left no trace in the checker either.
fn check_entry_gate(
    checker: &mut Option<noeta_check::SessionChecker>,
    program: &noeta_ast::Program,
    source: &Source,
) -> bool {
    let Some(checker) = checker.as_mut() else {
        return true;
    };
    let diagnostics = checker.check_entry(program);
    if diagnostics.is_empty() {
        return true;
    }
    emit_diagnostics(source, diagnostics.iter());
    !diagnostics
        .iter()
        .any(|d| d.severity == noeta_diagnostics::Severity::Error)
}

/// Try to evaluate the accumulated REPL buffer. Statements ending in `;`/`}` evaluate as-is;
/// a bare expression (no trailing `;`) is retried with a `;` appended so its value can be
/// printed. If the only parse problem is hitting end-of-input, the entry is treated as
/// incomplete and more input is requested (multiline). Any other error is reported, and the
/// buffer is reset so one bad entry cannot wedge the session.
///
/// With a `checker` present (`--check` / `:check on`, session-checker C2), a parsed entry is
/// type-checked against the accumulated session first: an entry with errors prints its `E0xxx`
/// diagnostics (rendered against the entry's own source) and is **skipped** — and `check_entry`'s
/// transactionality means it commits nothing, so the checker stays aligned with what actually ran.
/// Warning-only entries print the warnings and run.
fn repl_step(
    session: &mut VmSession,
    checker: &mut Option<noeta_check::SessionChecker>,
    precise_codegen: bool,
    buffer: &str,
    sources: &mut Vec<Source>,
) -> ReplStep {
    // The next evaluated entry's `SourceId` is its index in the persistent `sources` vector.
    let id = SourceId(sources.len() as u32);
    let source = Source::new(id, format!("<repl:{}>", sources.len()), buffer.to_string());
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let diags: Vec<Diagnostic> = lexed
        .diagnostics
        .iter()
        .chain(parsed.diagnostics.iter())
        .cloned()
        .collect();

    if diags.is_empty() {
        if !check_entry_gate(checker, &parsed.program, &source) {
            return ReplStep::Consumed;
        }
        sources.push(source);
        let out = eval_entry(session, checker, precise_codegen, &parsed.program);
        emit_session(sources, out);
        return ReplStep::Consumed;
    }

    // A bare expression needs a terminating `;`; retry with one appended (same id — only one of the
    // two sources is ever kept, whichever compiled).
    let psource = Source::new(
        id,
        format!("<repl:{}>", sources.len()),
        format!("{buffer};"),
    );
    let plexed = lex(&psource);
    let pparsed = parse(&psource, &plexed.tokens);
    if plexed.diagnostics.is_empty() && pparsed.diagnostics.is_empty() {
        if !check_entry_gate(checker, &pparsed.program, &psource) {
            return ReplStep::Consumed;
        }
        sources.push(psource);
        let out = eval_entry(session, checker, precise_codegen, &pparsed.program);
        emit_session(sources, out);
        return ReplStep::Consumed;
    }

    // An entry with unclosed `(`/`{`/`[` is a multi-line definition still being typed (a `class`,
    // a `fn` body, a multi-line list/object literal). The parser may report a *non*-end-of-input
    // error inside such a buffer rather than cleanly running out of tokens, so the end-of-input
    // check below is not enough on its own — gather more lines until the delimiters balance. The
    // count is over lexer tokens, so braces inside string/template literals (a single token) and
    // `${…}` interpolation never miscount.
    if unclosed_delimiters(&lexed.tokens) {
        return ReplStep::Incomplete;
    }

    // Only end-of-input errors → the entry is unfinished; gather more lines.
    if diags
        .iter()
        .all(|d| d.code == DiagnosticCode::UnexpectedEndOfInput)
    {
        return ReplStep::Incomplete;
    }

    // A genuine syntax error: report it against the original buffer and reset. The entry compiled
    // nothing, so its source is *not* kept — its id is reused by the next entry.
    emit_diagnostics(&source, diags.iter());
    ReplStep::Consumed
}

/// Whether `tokens` has more opening than closing delimiters — i.e. a `(`/`{`/`[` left unclosed, the
/// signature of a multi-line REPL entry still being typed. A single net depth across all three kinds
/// is enough to decide *incompleteness* (the parser validates correct nesting once the buffer is
/// balanced); a buffer that closes more than it opens (net ≤ 0) is left to the parser to report.
fn unclosed_delimiters(tokens: &[noeta_lexer::Token]) -> bool {
    let mut depth: i32 = 0;
    for token in tokens {
        match token.kind {
            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => depth -= 1,
            _ => {}
        }
    }
    depth > 0
}

/// Print a session evaluation's stdout, the value of a trailing bare expression (if any), then any
/// diagnostics and abort trace. `sources` holds every evaluated entry keyed by `SourceId`, so a
/// diagnostic or trace frame from a function defined in an earlier entry renders against that entry's
/// real file and line.
fn emit_session(sources: &[Source], out: SessionOutput) {
    print!("{}", out.stdout);
    if let Some(value) = out.value {
        println!("{value}");
    }
    let _ = io::stdout().flush();
    let map = SourceMap::new(sources.to_vec());
    emit_diagnostics_mapped(&map, out.diagnostics.iter());
    emit_trace(&out.trace, &map);
}

/// Render a session entry's abort trace to stderr, resolving each frame against `map` — the same
/// rendering and "only when there is a real call chain (≥2 frames)" rule `noeta run` uses (a single
/// frame just repeats the diagnostic's own location). With per-entry sources in `map`, a frame from a
/// function defined in an earlier entry now shows that entry's real file and line.
fn emit_trace(trace: &[noeta_vm::TraceFrame], map: &SourceMap) {
    if trace.len() >= 2 {
        eprint!("{}", noeta_vm::render_trace(trace, map));
    }
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_docs_url_joins_base_name_version() {
        // `name` keeps its `company/package` slash; a trailing slash on the base is trimmed.
        assert_eq!(
            web_docs_url("https://reg.noeta.dev", "acme/greeter", "0.3.0"),
            "https://reg.noeta.dev/acme/greeter/0.3.0/docs"
        );
        assert_eq!(
            web_docs_url("https://reg.noeta.dev/", "acme/greeter", "0.3.0"),
            "https://reg.noeta.dev/acme/greeter/0.3.0/docs"
        );
    }

    fn toks(src: &str) -> Vec<noeta_lexer::Token> {
        lex(&Source::new(SourceId::FIRST, "<t>", src.to_string())).tokens
    }

    #[test]
    fn unclosed_delimiters_detects_in_progress_multiline_entries() {
        // Open `{`/`(`/`[` with no match → still being typed (a `class`, `fn` body, or literal).
        assert!(unclosed_delimiters(&toks("class Res {")));
        assert!(unclosed_delimiters(&toks(
            "fn run(): void {\n  mut r = Res.new(3);"
        )));
        assert!(unclosed_delimiters(&toks("[1,\n 2,")));
        assert!(unclosed_delimiters(&toks("f(")));
        // Balanced (or over-closed) → let the parser decide, not "incomplete".
        assert!(!unclosed_delimiters(&toks("class Res { id: int }")));
        assert!(!unclosed_delimiters(&toks("[1, 2, 3]")));
        assert!(!unclosed_delimiters(&toks("x = 5;")));
        assert!(!unclosed_delimiters(&toks("}")));
        // Braces inside a template string are one token — they never miscount.
        assert!(!unclosed_delimiters(&toks("echo \"drop ${id}\";")));
    }

    /// A one-proto module whose const pool + code carry the given entries, with `names` as the op
    /// name table — enough to exercise the ring scan.
    #[cfg(feature = "jit")]
    fn module_with(
        consts: Vec<noeta_bytecode::Const>,
        code: Vec<noeta_bytecode::Op>,
        names: Vec<String>,
    ) -> noeta_bytecode::Module {
        let mut chunk = noeta_bytecode::Chunk::placeholder();
        chunk.consts = consts;
        chunk.code = code;
        noeta_bytecode::Module {
            protos: vec![chunk],
            shapes: Vec::new(),
            packed_schemas: Vec::new(),
            map_packed_sites: Vec::new(),
            methods: Vec::new(),
            destructors: Vec::new(),
            field_defaults: Vec::new(),
            comparable_derives: Vec::new(),
            tojson_derives: Vec::new(),
            destruct_reachable: Vec::new(),
            cache_slots: 0,
            reflection: Default::default(),
            type_reprs: Vec::new(),
            names,
            global_names: Vec::new(),
        }
    }

    #[cfg(feature = "jit")]
    #[test]
    fn aot_ring_features_selects_http_client_but_not_server() {
        use noeta_bytecode::Const;
        let client = vec!["ring-http-client".to_string()];

        // `use std.http.client` → a whole-module value. Post-split (P0.3b) this is *precise*: the
        // client module owns reqwest, so the whole-module reference selects the client ring. Module
        // identities are root-qualified (`std.http.client`), as the compiler now emits them.
        let via_module = module_with(
            vec![Const::NativeModule("std.http.client".into())],
            vec![],
            vec![],
        );
        assert_eq!(aot_ring_features(&via_module), client);

        // `use std.http.client.get` → a selective import of a client function → client ring.
        let client_fn = module_with(
            vec![Const::ModuleFn {
                module: "std.http.client".into(),
                func: "get".into(),
            }],
            vec![],
            vec![],
        );
        assert_eq!(aot_ring_features(&client_fn), client);

        // The split payoff: a `use std.http.server` program — whole module *or* its `response`/`serve`
        // functions — selects NO client ring, so reqwest is shed from the archive.
        let server_module = module_with(
            vec![Const::NativeModule("std.http.server".into())],
            vec![],
            vec![],
        );
        assert!(aot_ring_features(&server_module).is_empty());
        let server_fn = module_with(
            vec![Const::ModuleFn {
                module: "std.http.server".into(),
                func: "response".into(),
            }],
            vec![],
            vec![],
        );
        assert!(aot_ring_features(&server_fn).is_empty());

        // A program that only touches an always-on core / not-yet-gated module selects no ring.
        let non_http = module_with(vec![Const::NativeModule("std.math".into())], vec![], vec![]);
        assert!(aot_ring_features(&non_http).is_empty());
    }
}
