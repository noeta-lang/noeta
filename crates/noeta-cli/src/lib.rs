//! `noeta` — the user-facing toolchain, as a **library** with a thin binary shim (`src/main.rs`
//! is `run_cli(&[], &[])`). The library form exists for package-manager Phase 3: a composed
//! toolchain (an app with native extension dependencies) is a generated crate depending on this
//! one, whose `main` passes the extra extension units (and the command-trusted subset of them)
//! into [`run_cli`].
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
// Wrapped in the counting tracker so `noeta profile --alloc` can attribute allocated bytes to the
// interpreter's call stacks (a thread-local cumulative counter; two relaxed atomics + one TLS add
// per allocation — mimalloc still does all the real work).
#[global_allocator]
static GLOBAL: noeta_alloc_probe::TrackingAlloc<mimalloc::MiMalloc> =
    noeta_alloc_probe::TrackingAlloc(mimalloc::MiMalloc);

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use noeta_ast::{Expr, Stmt};
// `render` is re-exported here for `watch`'s diagnostic rendering (`crate::render`).
use noeta_diagnostics::render;
use noeta_parser::parse_fragment;
use noeta_span::SourceId;

// The package manager (package-manager P2) now lives in the `noeta-pm` library so `noeta-lsp` and
// `noeta-db` resolve dependencies through the same code; the CLI names its modules unqualified.
use noeta_pm::{graph, manifest};
// The L2 compile pipeline (source → runnable module) and the execution core live in `noeta-runner`
// so the CLI and the standalone lean runtime share one implementation (dev-deps D3c). The CLI's
// `run`/`dump`/`build`/`test` paths call these by the same names they used when defined here.
use noeta_runner::resolve_providers;

mod cmd;
mod compose;
mod context;
mod docgen;
mod output;
mod watch;

use cmd::bench::cmd_bench;
use cmd::build::{cmd_build, cmd_dump};
use cmd::cache::cmd_cache;
use cmd::check::cmd_check;
use cmd::doc::cmd_doc;
use cmd::expand::cmd_expand;
use cmd::fmt::cmd_fmt;
use cmd::grammar::cmd_grammar_treesitter;
use cmd::init::cmd_init;
use cmd::pm::{
    cmd_add, cmd_advisory_promote, cmd_advisory_publish, cmd_advisory_report, cmd_advisory_reports,
    cmd_audit, cmd_claim, cmd_key, cmd_publish, cmd_scope, cmd_update, cmd_watch_scope,
};
use cmd::repl::cmd_repl;
use cmd::run::{cmd_run, execute_real_host, try_run_stapled};
use cmd::serve::{ext_command_clap, ext_command_dispatch};
use cmd::servers::{cmd_dap, cmd_lsp, cmd_mcp, cmd_profile};
use cmd::test::{call_stmt, cmd_test};
use cmd::upgrade::cmd_upgrade;
use output::emit_diagnostics_mapped;

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
    /// Scaffold a new Noeta project: a `noeta.toml` wiring the std dev tiers (`@test`,
    /// `@bench`, `@doc`, `@debug`) into a `development` target beside an explicit
    /// `production` baseline, a `src/main.noe` exercising each tier, `.gitignore`, the
    /// `.vscode/` run profiles the Noeta extension understands, and the agent surface —
    /// `AGENTS.md` (how to drive the toolchain, CLI and MCP) plus `SYNTAX.md`, the full
    /// language reference generated from the embedded guide. Never overwrites an existing
    /// file, so it is safe in a non-empty directory.
    Init {
        /// Directory to initialize (default: the current directory; created if missing).
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Package name as `company/package` (default: `local/<directory-name>` — change
        /// `local` to your registry scope before publishing).
        #[arg(long)]
        name: Option<String>,
        /// Skip `git init` when the directory is not already inside a git repository.
        #[arg(long)]
        no_git: bool,
    },
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
        /// editor integrations to run a single benchmark, and the impact-filtered `--watch` seam
        /// (server-hmr W3), symmetric with `noeta test --name`.
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
        /// Generate the **API reference** from the intrinsic registry (the stdlib and any composed
        /// native modules) instead of from `.noe` source — a registry-ready `docs.json` (schema 1)
        /// organized by module, with signatures and any registered doc prose. Prints to stdout, or
        /// writes the artifact + Markdown tree with `--out`.
        #[arg(long, conflicts_with_all = ["file", "package"])]
        api: bool,
        /// With `--api`, document only the extension whose namespace root is this (a package's own
        /// name segment) — its own modules/types, excluding `std`. Omit to document the whole
        /// registry.
        #[arg(long, requires = "api", value_name = "NAMESPACE")]
        root: Option<String>,
        /// With `--api --root`, fail (before emitting docs) if the package registers any module or
        /// extern type outside its root namespace — the publish quality gate against a type that
        /// leaked into `std` (a missing `namespace:`). Exit 2 lists the offenders.
        #[arg(long, requires = "root")]
        lint: bool,
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
        /// Emit a single **wasm** artifact (P-WASM W1.2): the bundle is injected into the
        /// `noeta-wasm-runner` wasm32-wasip1 binary's data section, producing one `.wasm` that
        /// runs the program under any WASI runtime (`wasmtime run app.wasm`). The runner is
        /// located via `NOETA_WASM_RUNNER`, next to this binary, else built from the workspace
        /// (interim; needs cargo + the `wasm32-wasip1` target).
        #[arg(long)]
        wasm: bool,
        /// Emit a **wasi:http serve component** (P-WASM W4): the program is baked into the
        /// `noeta-wasm-serve` component (wasm32-wasip2), whose `wasi:http/incoming-handler`
        /// export runs the program's `http.serve` handler once per request. Deploy on any
        /// component host: `wasmtime serve -S cli=y app.serve.wasm`. The generic component is
        /// located via `NOETA_WASM_SERVE`, next to this binary, else built from the workspace
        /// (interim) — then the bundle is stapled in (~1 ms, no per-app cargo build).
        #[arg(long)]
        serve: bool,
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
    /// Print the Noeta source that compile-time `@`-directive expansions produced — the code an
    /// extension's `expand` hook generated and spliced into the declaration it decorates. Links
    /// through the same pipeline as `check`, so what prints is exactly what the compiler saw. For
    /// debugging a hook, and for diffing in CI so a spec change surfaces as a reviewable delta.
    /// Exits non-zero if any expansion failed (the same 0/1/2 codes as `check`).
    Expand {
        /// File or directory to expand (default: the current directory, walked recursively for
        /// `.noe` files) — the same resolution `noeta check` uses.
        #[arg(default_value = ".")]
        path: PathBuf,
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
        /// Run the **allocation** profiler: attribute every byte the program allocates to the call
        /// path that allocated it — an exact, bytes-weighted memory flamegraph ("who allocates";
        /// frees are ignored). Takes precedence over the sampling flags; ignored with
        /// `--instrument`.
        #[arg(long)]
        alloc: bool,
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
        /// speedscope.app). Instrumenting: `table` (default), `json` (rows + the exact ns-weighted
        /// call-tree stacks) — the stack formats (`folded`/`svg`/`speedscope`) also work, rendered
        /// from the exact call tree.
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
        /// Arm the **tier-1 JIT** while sampling (default: tier-0 pinned). Hot prototypes run native
        /// and their wall time is sampled at the JIT trampoline, so the profile reflects what
        /// actually ships; tier-1 frames are labeled ` [jit]` in the flamegraph. Wall-clock sampling
        /// only (the op-clock cannot see native code) and ignored with `--instrument`/`--alloc`.
        /// Function-level attribution inside JIT frames — not line-level.
        #[arg(long)]
        jit: bool,
    },
    // (`Serve` was a variant here until higher-order-abi H6 — `noeta serve` is now an
    // extension-contributed command, `noeta-stdlib/src/serve.rs::SERVE_COMMAND`, wired
    // dynamically in `main`.)
    /// Inspect or clean the per-user cache (`~/.cache/noeta/`). It holds three kinds of derived
    /// state: cached compilations (`*.noeb` — the transparent startup cache, M3), composed
    /// toolchains (`compose/` — a full build per native-dependency set, easily 1–2 GiB each),
    /// and fetched package sources (`pkg/`). Everything in it is re-derivable, so deleting any
    /// of it is always safe — the next run recompiles, recomposes, or refetches what it needs.
    /// Without a subcommand, `ls`.
    Cache {
        #[command(subcommand)]
        action: Option<CacheAction>,
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
        /// Do not write. Print a unified diff of the pending reformat for each file (empty output +
        /// exit 0 when everything is already formatted; exit non-zero when any diff is shown).
        #[arg(long)]
        diff: bool,
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
    /// Generate editor grammar artifacts for this project's declared text tiers (text-tiers arc).
    /// A project's `@tier(<name>, text: "<lang>")` declarations open verbatim `@<name> { … }` bodies
    /// a *static* editor grammar cannot know about; this emits a per-project overlay so those bodies
    /// parse (and highlight) as their language. Mirrors the VS Code extension's TextMate generator.
    Grammar {
        #[command(subcommand)]
        target: GrammarTarget,
    },
    /// Add a dependency to the nearest `noeta.toml` and refresh `noeta.lock` (package-manager P2.4).
    /// Exactly one source must be given: `--path`, `--git` (with `--tag`), or `--version` (registry).
    /// The manifest edit preserves your formatting and comments; the dependency is then resolved so
    /// the lockfile is updated (a git source is fetched now, so a typo'd URL/tag fails fast).
    Add {
        /// The import root you will `use` the dependency under (an identifier): `use <key>.…`.
        /// Optional: when omitted it is derived from the package's own root segment — the `package`
        /// half of a `--package company/pkg`, or the `[package]` name of a `--path` dependency.
        /// (A `--git` source with no derivable identity still needs an explicit key.)
        key: Option<String>,
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
        /// The registry package identity `company/package` for a `--version` dependency, decoupled
        /// from the import-root key (like Cargo's `foo = { package = "real" }`). Required for a
        /// registry dependency to resolve; also the source of a derived key when `key` is omitted.
        #[arg(long)]
        package: Option<String>,
    },
    /// Re-resolve dependencies and rewrite `noeta.lock` (package-manager P2.4): a git tag is
    /// re-fetched at the remote and re-pinned to its current commit SHA, so `update` picks up a
    /// moved tag that a locked build would otherwise reproduce from the old commit.
    Update,
    /// Self-update the **toolchain binary** from GitHub releases — the counterpart of `update`,
    /// which re-resolves a *project's dependencies*. Resolves the latest release, verifies the
    /// artifact's SHA-256 checksum, and atomically replaces the running executable. Prereleases
    /// are never installed. A cargo-installed `noeta` is refused (upgrade that through cargo);
    /// unsupported platforms are pointed at building from source.
    Upgrade {
        /// Install this exact release tag (e.g. `v0.2.0`) instead of the latest. Downgrades are
        /// allowed and labeled as such; prerelease tags (any `-` suffix) are refused.
        #[arg(long, value_name = "vX.Y.Z")]
        version: Option<String>,
        /// Only report whether an upgrade is available, changing nothing. Exits 0 when this
        /// binary is current and 1 when a newer release exists, so scripts can gate on it.
        #[arg(long, conflicts_with = "version")]
        check: bool,
    },
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
        /// Skip uploading the package's `README.md` (rendered on the registry's package page).
        /// By default the README rides along with the release when the file exists; an upload
        /// failure never blocks the publish (it degrades to a warning).
        #[arg(long)]
        no_readme: bool,
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
    /// Claim a registry **scope** for yourself, proving you control the GitHub org/user of the same
    /// name via a GitHub Actions OIDC token (namespace-protection #1). Self-service — no admin — and
    /// squat-proof: you can only claim the scope that matches your GitHub identity. Run it from a
    /// GitHub Actions workflow that grants `id-token: write`. Binds a publish token to the scope
    /// (generated and printed unless you pass `--token`), which `noeta publish` then presents.
    Claim {
        /// The scope (your GitHub org/user name) to claim.
        scope: String,
        /// The publish token to bind (default: a fresh random token, printed on success).
        #[arg(long)]
        token: Option<String>,
        /// The OIDC audience the registry expects (default: `NOETA_REGISTRY_AUDIENCE` or
        /// `noeta-registry`). Must match the registry's configured audience.
        #[arg(long)]
        audience: Option<String>,
        /// Claim by **domain** instead of GitHub: prove you control this domain (whose first label must
        /// be the scope, e.g. `acme.dev` for `acme`) by serving `/.well-known/noeta-registry.txt`
        /// containing `noeta-scope=<scope>`. Skips the GitHub OIDC/device-flow path.
        #[arg(long)]
        domain: Option<String>,
    },
    /// Manage a registry scope you own — its publishing policy (namespace-protection #1). Authenticated
    /// with the scope's publish token (`NOETA_REGISTRY_TOKEN`) against `NOETA_REGISTRY_URL`.
    Scope {
        #[command(subcommand)]
        action: ScopeAction,
    },
    /// Issue or file security advisories (advisory-intake arc). `publish` issues a **publisher**-tier
    /// advisory for a package in a scope you own — keyless-signed with your OIDC identity, so consumers
    /// verify it offline. `report` files a **public report** against any package (unauthenticated,
    /// rate-limited) for an operator or the scope owner to triage.
    Advisory {
        #[command(subcommand)]
        action: AdvisoryCommand,
    },
    /// Monitor a scope's advisory **transparency log** over time (advisory-intake arc, tier 6): verify
    /// the log is an append-only extension of the last run's checkpoint and that no advisory previously
    /// seen for the scope has silently disappeared — so a registry can't quietly suppress or rewrite a
    /// scope's advisories after first use. State is pinned in a small file between runs (ideal for a CI
    /// cron); a detected suppression or rewrite exits non-zero.
    WatchScope {
        /// The scope (`company`) to monitor.
        scope: String,
        /// Where to keep the pinned watch state between runs (default: under the noeta cache).
        #[arg(long)]
        state: Option<PathBuf>,
    },
    // `noeta migrate` is NOT a core verb: it is an `ExtCommand` the `para/db` package contributes
    // (para-extraction) — a consumer that depends on `para/db` and trusts its commands
    // (`[trust] commands = ["para/db"]`) gets it through the composed toolchain, same as any
    // extension-contributed subcommand.
}

#[derive(Subcommand)]
enum AdvisoryCommand {
    /// Publish (or update) a **publisher**-tier advisory for a package in a scope you own. The advisory
    /// is keyless-signed with your OIDC identity (ambient CI identity, or `--interactive` browser
    /// login) and sent authenticated with the scope's publish token (`NOETA_REGISTRY_TOKEN`).
    Publish {
        /// The advisory id (e.g. `ACME-2026-0001`).
        id: String,
        /// The affected package (`company/package`) — its scope must be one you own.
        package: String,
        /// The affected version range, a SemVer requirement (e.g. `">=1.0.0, <1.2.0"`).
        ranges: String,
        /// Severity: `low`, `medium`, `high`, or `critical`.
        severity: String,
        /// A one-line summary headline.
        summary: String,
        /// A longer description (may be multi-line).
        #[arg(long)]
        details: Option<String>,
        /// A link to the full advisory.
        #[arg(long)]
        url: Option<String>,
        /// The first fixed version (informational).
        #[arg(long)]
        patched: Option<String>,
        /// Withdraw (retract) this advisory — a false alarm. Re-issues the same id in the withdrawn
        /// state (kept in the log, never deleted).
        #[arg(long)]
        withdraw: bool,
        /// Sign keyless via an **interactive browser login** (Sigstore OAuth). For a laptop, where no
        /// ambient CI identity exists. Without it, the ambient CI identity is used.
        #[arg(long)]
        interactive: bool,
        /// With `--interactive`: print the sign-in URL and prompt for the code instead of opening a
        /// browser (SSH sessions, containers).
        #[arg(long, requires = "interactive")]
        oob: bool,
    },
    /// File a **public report** against a package (unauthenticated, rate-limited). A report is not an
    /// advisory — it is queued for an operator or the scope owner to triage and possibly promote.
    Report {
        /// The package (`company/package`) the report is against.
        package: String,
        /// A one-line summary of the issue.
        summary: String,
        /// The affected version range you believe (a SemVer requirement), if known.
        #[arg(long)]
        ranges: Option<String>,
        /// A longer description (repro steps, impact).
        #[arg(long)]
        details: Option<String>,
        /// A link to more detail.
        #[arg(long)]
        url: Option<String>,
        /// How to identify you (optional — intake is anonymous by default).
        #[arg(long)]
        reporter: Option<String>,
    },
    /// List the public reports queued for triage — what's **promotable** (advisory-intake residual a).
    /// Without `--scope`, the operator triage queue (needs `NOETA_REGISTRY_ADMIN_TOKEN`); with it, the
    /// scope owner's own queue (only their packages' reports; needs the scope's `NOETA_REGISTRY_TOKEN`).
    /// Defaults to the `pending` (promotable) reports.
    Reports {
        /// Show a scope owner's own queue (their packages' reports), authenticated with the scope token.
        /// Without it, the operator queue (admin token).
        #[arg(long)]
        scope: Option<String>,
        /// Filter by status (`pending` | `promoted` | `dismissed`). Defaults to `pending`; pass `--all`
        /// to list every status.
        #[arg(long)]
        status: Option<String>,
        /// List reports of every status (overrides the default `pending` filter).
        #[arg(long)]
        all: bool,
    },
    /// **Promote** a queued report into a signed advisory (advisory-intake residual a). The advisory is
    /// prefilled from the report (package, ranges, summary, details, url) — you supply the triaged `--id`
    /// and `--severity`. As an **operator** (`--operator`, admin token) it becomes an `operator`-tier
    /// advisory; otherwise the report package's **scope owner** promotes it into a keyless-signed
    /// `publisher`-tier advisory (exactly like `advisory publish`), authenticated with the scope token.
    Promote {
        /// The report id to promote (from `noeta advisory reports`).
        report: String,
        /// The advisory id to issue (e.g. `NOETA-2026-0001` or `ACME-2026-0001`).
        #[arg(long)]
        id: String,
        /// Severity: `low`, `medium`, `high`, or `critical`.
        #[arg(long)]
        severity: String,
        /// Override the affected range (default: the report's `ranges`, if any).
        #[arg(long)]
        ranges: Option<String>,
        /// Override the summary (default: the report's summary).
        #[arg(long)]
        summary: Option<String>,
        /// Override the details (default: the report's details).
        #[arg(long)]
        details: Option<String>,
        /// Override the link (default: the report's url).
        #[arg(long)]
        url: Option<String>,
        /// The first fixed version (informational).
        #[arg(long)]
        patched: Option<String>,
        /// Promote as the **operator** (admin token → an `operator`-tier advisory, no keyless bundle).
        /// Without it, the report package's scope owner promotes into a `publisher`-tier advisory.
        #[arg(long)]
        operator: bool,
        /// (Scope-owner path) Sign keyless via an **interactive browser login** instead of the ambient
        /// CI identity.
        #[arg(long)]
        interactive: bool,
        /// With `--interactive`: print the sign-in URL and prompt for the code (SSH sessions, containers).
        #[arg(long, requires = "interactive")]
        oob: bool,
    },
}

/// Which editor grammar `noeta grammar` targets. Only tree-sitter needs a *generated* per-project
/// grammar (its parser is compiled, so a project's custom tiers cannot be discovered at load time);
/// the TextMate side is regenerated live by the VS Code extension itself.
#[derive(Subcommand)]
enum GrammarTarget {
    /// Emit the per-project tree-sitter overlay: `project-tiers.json` (the verbatim-body tier-name
    /// token list `grammar.js` reads) and `queries/injections.scm` (one language-injection rule per
    /// tier). Drop these into a `tree-sitter-noeta` grammar checkout and run `tree-sitter generate`
    /// (or pass `--generate`) to rebuild the parser. Without an output directory the token list is
    /// printed to stdout.
    #[command(name = "tree-sitter")]
    TreeSitter {
        /// The project to scan for `@tier(name, text: "lang")` declarations (default: the current
        /// directory). Every `.noe` file beneath it is scanned, plus installed native extensions'
        /// tiers.
        #[arg(default_value = ".")]
        project: PathBuf,
        /// The `tree-sitter-noeta` grammar directory to write the overlay into. Omit to print the
        /// `project-tiers.json` token list to stdout instead.
        #[arg(long, short)]
        out: Option<PathBuf>,
        /// After writing, run `tree-sitter generate` in the output directory to rebuild the parser
        /// (requires the tree-sitter CLI on PATH).
        #[arg(long, requires = "out")]
        generate: bool,
    },
}

#[derive(Subcommand)]
enum ScopeAction {
    /// Require that every release under a scope carry verified provenance, so a leaked publish token
    /// alone can't push a release. `--off` lifts the requirement.
    RequireProvenance {
        /// The scope (`company`) to set the policy on — you must own its publish token.
        scope: String,
        /// Narrow which trust root is required: `key` (Ed25519 signature) or `keyless` (Sigstore
        /// bundle). Omitted, either satisfies it.
        #[arg(long, value_name = "key|keyless")]
        root: Option<String>,
        /// Turn the requirement **off** for this scope instead of on.
        #[arg(long)]
        off: bool,
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
    /// Summarize the cache per category — cached compilations (`*.noeb`), composed toolchains
    /// (`compose/`), and fetched package sources (`pkg/`) — with entry counts and sizes; compose
    /// entries (the multi-GiB ones) are listed individually with size and last-used time. The
    /// default when no subcommand is given.
    Ls,
    /// Print the cache directory path (whether or not it exists yet).
    Path,
    /// Show the startup-cache location, entry count, total size on disk, and the size cap.
    Info,
    /// Remove the composed toolchains that other toolchain builds left behind (stale versions
    /// this binary can never reuse), reporting the bytes reclaimed; this binary's own
    /// compositions are kept. Always safe: everything in the cache is re-derivable — a later
    /// run rebuilds what it needs.
    Clean {
        /// Wipe the whole cache instead: all composed toolchains, fetched package sources, and
        /// cached compilations. Equally safe — costs at most a recompile/recompose/refetch.
        #[arg(long)]
        all: bool,
    },
    /// Remove all cached compilations (the `*.noeb` startup-cache entries only).
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
    command_extras: &'static [&'static (dyn noeta_stdlib::Extension + Sync)],
) -> ExitCode {
    // First-party toolchain extensions that ship with every binary (stock or composed), in their own
    // namespaces — the HTML body formatter (`noeta-html`) which reflows `@html` bodies under
    // `noeta fmt`, and the CSS formatter (`noeta-css`) it delegates `<style>` blocks to. Prepended to
    // the caller's `extra` (a composed app's dependency units) so all are installed before any lookup.
    let mut units: Vec<&'static (dyn noeta_stdlib::Extension + Sync)> =
        vec![&noeta_html::HTML_EXTENSION, &noeta_css::CSS_EXTENSION];
    units.extend_from_slice(extra);
    noeta_stdlib::registry::install_with_extras(&units);
    // Phase 4: a dependency's `ExtCommand`s reach the CLI only if the root app command-trusts its
    // PACKAGE (`[trust].commands`). The composer passes the trusted packages' extension units here
    // (`command_extras` ⊆ `extra`), so trust is keyed by the providing package's identity — never
    // by matching root-name strings, which would over-trust every package sharing a scope root
    // (trusting `para/db`'s commands must not trust all of `para/*`) and, for a scope-keyed
    // dependency, didn't even match its extensions' actual root. std's own commands (root `"std"`)
    // are always available. The stock binary passes an empty list — it has only std units.
    let trusted_commands: Vec<_> = noeta_stdlib::registry::extensions()
        .iter()
        .filter(|ext| ext.root() == "std")
        .flat_map(|ext| ext.commands().iter())
        .chain(command_extras.iter().flat_map(|ext| ext.commands().iter()))
        .collect();
    // P-AOT L2: if this executable is a `noeta build --exe` artifact (a bundle stapled onto a copy
    // of the runtime), run the embedded program directly — the shipped app is not the toolchain, so
    // its CLI verbs are irrelevant. A plain `noeta` binary has no trailer and falls through to the
    // normal CLI. Detection reads only the tail of the file, not the whole binary.
    if let Some(code) = try_run_stapled() {
        return code;
    }
    // `--watch` (server-hmr W0): wrap ANY invocation in the restart-on-change dev loop. Stripped
    // from argv before clap so it works uniformly for derive-built and extension-contributed
    // commands (`noeta serve --watch`); the clap arg added below exists purely so `--help` and
    // `--watch`'s error messages know the flag.
    if let Some(code) = watch::maybe_watch() {
        return code;
    }
    // Extension-contributed subcommands (higher-order-abi H6): augment the derive-built CLI with
    // each registered command (so `noeta --help` lists them and each gets real clap parsing),
    // then dispatch a matched name to its extension `run` — the in-process `cargo clippy` model.
    let mut cli = <Cli as clap::CommandFactory>::command()
        .arg(
            clap::Arg::new("watch")
                .long("watch")
                .global(true)
                .action(clap::ArgAction::SetTrue)
                .help(
                    "Restart the command whenever project source files change (*.noe, noeta.toml)",
                ),
        )
        // Accepted and ignored on every subcommand: LSP clients (VS Code's vscode-languageclient
        // with `TransportKind.stdio`, and others) append `--stdio` to the server argv to select the
        // stdio transport — the only transport `noeta lsp`/`dap`/`mcp` speak. Nothing reads it; the
        // global arg exists purely so clap accepts the flag instead of erroring out the server.
        .arg(
            clap::Arg::new("stdio")
                .long("stdio")
                .global(true)
                .hide(true)
                .action(clap::ArgAction::SetTrue),
        );
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
                // A bare file path as the "subcommand" — `noeta script.noe`, or a `.noe` file with a
                // `#!/usr/bin/env noeta` shebang run as `./script.noe` — is a run shortcut: execute
                // the file, forwarding any trailing arguments to the program.
                if let Some(code) = try_bare_file_run(&err) {
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
        Command::Init { path, name, no_git } => cmd_init(&path, &name, no_git),
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
            api,
            root,
            lint,
        } => cmd_doc(&file, &package, &out, &target, api, root.as_deref(), lint),
        Command::Build {
            file,
            out,
            exe,
            native,
            wasm,
            serve,
            tier,
            target,
        } => cmd_build(
            &file,
            out.as_deref(),
            exe,
            native,
            wasm,
            serve,
            &tier,
            &target,
        ),
        Command::Dump { file, tier, target } => cmd_dump(&file, &tier, &target),
        Command::Check {
            path,
            tier,
            target,
            format,
        } => cmd_check(&path, &tier, &target, format),
        Command::Expand { path } => cmd_expand(&path),
        Command::Repl { no_check, load } => cmd_repl(!no_check, load),
        Command::Lsp => cmd_lsp(),
        Command::Dap => cmd_dap(),
        Command::Mcp => cmd_mcp(),
        Command::Profile {
            file,
            instrument,
            alloc,
            hz,
            every,
            format,
            out,
            lines,
            jit,
        } => cmd_profile(
            &file,
            instrument,
            alloc,
            hz,
            every,
            format.as_deref(),
            out,
            lines,
            jit,
        ),
        Command::Cache { action } => cmd_cache(action.as_ref()),
        Command::Fmt {
            files,
            check,
            diff,
            stdin,
            parens,
            semicolons,
        } => cmd_fmt(&files, check, diff, stdin, parens, semicolons),
        Command::Grammar { target } => match target {
            GrammarTarget::TreeSitter {
                project,
                out,
                generate,
            } => cmd_grammar_treesitter(&project, out.as_deref(), generate),
        },
        Command::Add {
            key,
            path,
            git,
            tag,
            version,
            package,
        } => cmd_add(
            key.as_deref(),
            path.as_deref(),
            git.as_deref(),
            tag.as_deref(),
            version.as_deref(),
            package.as_deref(),
        ),
        Command::Update => cmd_update(),
        Command::Upgrade { version, check } => cmd_upgrade(version.as_deref(), check),
        Command::Publish {
            git,
            tag,
            key,
            interactive,
            oob,
            no_docs,
            no_readme,
        } => cmd_publish(
            &git,
            tag.as_deref(),
            key,
            interactive,
            oob,
            no_docs,
            no_readme,
        ),
        Command::Audit { path } => cmd_audit(&path),
        Command::Key { action } => cmd_key(&action),
        Command::Claim {
            scope,
            token,
            audience,
            domain,
        } => cmd_claim(
            &scope,
            token.as_deref(),
            audience.as_deref(),
            domain.as_deref(),
        ),
        Command::Scope { action } => cmd_scope(&action),
        Command::Advisory { action } => match action {
            AdvisoryCommand::Publish {
                id,
                package,
                ranges,
                severity,
                summary,
                details,
                url,
                patched,
                withdraw,
                interactive,
                oob,
            } => cmd_advisory_publish(
                &id,
                &package,
                &ranges,
                &severity,
                &summary,
                details.as_deref(),
                url.as_deref(),
                patched.as_deref(),
                withdraw,
                interactive,
                oob,
            ),
            AdvisoryCommand::Report {
                package,
                summary,
                ranges,
                details,
                url,
                reporter,
            } => cmd_advisory_report(
                &package,
                &summary,
                ranges.as_deref(),
                details.as_deref(),
                url.as_deref(),
                reporter.as_deref(),
            ),
            AdvisoryCommand::Reports { scope, status, all } => {
                let status = if all {
                    None
                } else {
                    Some(status.as_deref().unwrap_or("pending").to_string())
                };
                cmd_advisory_reports(scope.as_deref(), status.as_deref())
            }
            AdvisoryCommand::Promote {
                report,
                id,
                severity,
                ranges,
                summary,
                details,
                url,
                patched,
                operator,
                interactive,
                oob,
            } => cmd_advisory_promote(
                &report,
                &id,
                &severity,
                ranges.as_deref(),
                summary.as_deref(),
                details.as_deref(),
                url.as_deref(),
                patched.as_deref(),
                operator,
                interactive,
                oob,
            ),
        },
        Command::WatchScope { scope, state } => cmd_watch_scope(&scope, state.as_deref()),
    }
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
/// The **bare-file run** shortcut: `noeta <path>` where `<path>` is an existing `.noe`/`.noeb` file
/// runs it — the same as `noeta run <path>` — forwarding any trailing arguments to the program.
/// This is what makes a `#!/usr/bin/env noeta` shebang work: the kernel invokes `noeta <script> …`,
/// which clap sees as an unknown subcommand, and this recovers it. Returns `None` when the "command"
/// is not an existing Noeta file, so a genuine typo still gets clap's error. `.noeb` is handled by
/// [`cmd_run`] itself (it sniffs the bundle magic).
fn try_bare_file_run(err: &clap::Error) -> Option<ExitCode> {
    let name = err
        .get(clap::error::ContextKind::InvalidSubcommand)
        .and_then(|v| match v {
            clap::error::ContextValue::String(s) => Some(s.clone()),
            _ => None,
        })?;
    let file = PathBuf::from(&name);
    let runnable = file.is_file()
        && matches!(
            file.extension().and_then(|e| e.to_str()),
            Some("noe" | "noeb")
        );
    if !runnable {
        return None;
    }
    // Program arguments are everything after the file path: `./script.noe a b` → the program reads
    // `a`, `b` via `args.all()`.
    let prog_args: Vec<String> = std::env::args()
        .skip(1)
        .skip_while(|a| *a != name)
        .skip(1)
        .collect();
    Some(cmd_run(&file, &[], &None, false, false, &prog_args))
}

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
            eprintln!("noeta: {err}");
            return Some(ExitCode::from(2));
        }
    };
    // A file that fails to load or link cannot tell us whether it declares the tier — fall
    // through (the external probe, then clap's error). Dependencies resolve first: a declared
    // tier typically lives in a dependency package (`use fuzzkit.tiers.run_fuzz`).
    let deps = graph::resolve_graph(&file).ok()?.packages;
    let linked = noeta_loader::load_with_deps(&file, manifest::root_edition(&file), &deps)
        .ok()?
        .ok()?;
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
                eprintln!("noeta: {err}");
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
pub(crate) fn run_declared_tier(
    name: &str,
    linked: &noeta_loader::Linked,
    activated: noeta_check::Activated,
    tier: noeta_check::DeclaredTier,
) -> ExitCode {
    if !activated.diagnostics.is_empty() {
        emit_diagnostics_mapped(&linked.sources, activated.diagnostics.iter());
        return ExitCode::from(1);
    }
    // An expression tier has no runner semantics — its blocks are expressions in ordinary code,
    // evaluated wherever they appear; there is nothing for a subcommand to run.
    if tier.expr.is_some() {
        eprintln!(
            "`{name}` is an expression tier — its `@{name} {{ … }}` blocks are values in \
             ordinary code (run the program instead: `noeta run <file>`)"
        );
        return ExitCode::from(2);
    }
    // The activated code roots for this tier — a built-in name's roots land in the dedicated sinks
    // (an overridden `bench = "criterion"` still collects under `benches`), a custom tier's in
    // `custom`. A text tier has no code roots (its bodies come from `activated.texts`).
    let roots = match name {
        "test" => activated.tests.clone(),
        "bench" => activated.benches.clone(),
        _ => activated.custom.get(name).cloned().unwrap_or_default(),
    };

    // The runner call — `<runner>([TierRoot { name: "<fn>", run: <fn> }, …])`, or for a text
    // tier `<runner>([TierText { target: "<decl>", text: "<body>" }, …])` — is built as AST
    // directly: the runner (and, in a namespaced entry, a root) carries its **link-qualified**
    // dotted name, which is an identifier to the resolved program but would parse as member
    // access from text. The root *declaration* is textual (no names to qualify) and only
    // synthesized when the program doesn't already declare one: the checker knows it as a
    // prelude type — that is what lets the runner's package name `List<TierRoot>` standalone —
    // but the backends build record literals from real declarations, and a declaration of the
    // same name shadows the prelude registration by design.
    let is_text = tier.text.is_some();
    let (root_ty, root_decl) = if is_text {
        (
            noeta_ast::reflect::TIER_TEXT,
            "struct TierText { target: string  text: string }",
        )
    } else {
        (
            noeta_ast::reflect::TIER_ROOT,
            "struct TierRoot { name: string  run: () -> void }",
        )
    };
    let mut program = activated.program;
    let declares_root_ty = program
        .stmts
        .iter()
        .any(|s| matches!(s, Stmt::Struct(d) if d.name == root_ty));
    if !declares_root_ty {
        let fragment = parse_fragment(SourceId(u32::MAX), "<tier-dispatch>", root_decl);
        if !fragment.diagnostics.is_empty() {
            eprintln!("noeta: internal error synthesizing the `{name}` dispatch");
            return ExitCode::from(2);
        }
        program.stmts.extend(fragment.program.stmts);
    }
    let span = program.span;
    let field = |name: &str, value: Expr| noeta_ast::FieldInit {
        name: name.to_string(),
        name_span: span,
        value,
        span,
    };
    let str_expr = |value: String| Expr::Str { value, span };
    let object = |fields: Vec<noeta_ast::FieldInit>| {
        Expr::Object(noeta_ast::ObjectLit {
            type_name: Some(root_ty.to_string()),
            type_name_span: span,
            fields,
            spread: None,
            span,
        })
    };
    let root_items: Vec<Expr> = if is_text {
        activated
            .texts
            .get(name)
            .map(|blocks| blocks.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|block| {
                let target = match &block.target {
                    noeta_check::DocTarget::Decl { name, .. } => name.clone(),
                    _ => String::new(),
                };
                object(vec![
                    field("target", str_expr(target)),
                    field("text", str_expr(block.text.clone())),
                ])
            })
            .collect()
    } else {
        // `roots` already handles the built-in sinks, so a provider-overridden `test`/`bench`
        // dispatches its collected roots correctly (they never land in `custom`).
        roots
            .iter()
            .map(|root| {
                object(vec![
                    field("name", str_expr(root.name.clone())),
                    // A method root (`Type.method`) is referenced as an associated function, not a
                    // bare (dotted) identifier — so a provider-overridden `test`/`bench` can invoke a
                    // method root's `run`.
                    field("run", cmd::test::root_ref(&root.name, span)),
                ])
            })
            .collect()
    };
    program.stmts.push(call_stmt(
        &tier.runner,
        vec![Expr::List {
            items: root_items,
            span,
        }],
        span,
    ));
    // One check over the whole dispatch program: the user's code, the stamped attributes, the
    // runner's signature, and the synthesized call all validate together — under the project's
    // per-package editions (the user code keeps its source ids; the synthesized nodes are default).
    let checked = context::check_under(&program, &linked.editions);
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
            eprintln!("noeta: {msg}");
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
        .map_err(|e| eprintln!("noeta: running `{}` failed: {e}", found.display()))
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
