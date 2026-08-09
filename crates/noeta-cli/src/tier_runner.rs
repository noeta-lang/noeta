//! The CLI-side registration of std's native dev-tier runners — Part B of registry-dispatched tier
//! runners.
//!
//! `test`/`bench`/`doc` are ordinary `std` [`ExtTier`](noeta_stdlib::ExtTier)s (declared in
//! `noeta-stdlib`), but their *runners* — `cmd_test`'s parallel isolate executor, `cmd_bench`'s
//! two-point measurement, `cmd_doc`'s extractor — are native and live **above** stdlib, in this
//! crate. std therefore cannot register them from its own
//! [`Extension::tier_runners`](noeta_stdlib::Extension::tier_runners); the CLI registers them here,
//! through a unit whose [`root`](noeta_stdlib::Extension::root) is `"std"`. That is what makes
//! `find_tier_runner_scoped(&["std"], "test")` resolve to std's native driver, so the clap verbs and
//! the generic `noeta <tier> <file>` path both dispatch a tier **by identity** — exactly the way a
//! program-declared `@tier` runner and an expression tier already resolve — instead of calling the
//! executor directly.
//!
//! The seam ([`ExtTierRunner::run`](noeta_stdlib::ExtTierRunner)) is a bare `fn` pointer whose
//! payload is a file-plus-roots [`TierRun`](noeta_stdlib::TierRun); it cannot close over the flags a
//! clap verb parsed (`--fail-fast`, `--jobs`, `--baseline`, …). So a verb stashes its parsed flags
//! in a thread-local right before it resolves-and-invokes, and the wrapper reads them back. std's
//! native runners are **file-driven** (Part A's design — they re-load the entry through the
//! [`CommandCtx`](noeta_stdlib::CommandCtx) and re-collect their own roots, as an
//! [`ExtCommand`](noeta_stdlib::ExtCommand) drives its file), so the seam's roots ride empty here; a
//! program `@tier` runner still receives its collected roots through `run_declared_tier`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use noeta_stdlib::{
    ArgKind, ArgSpec, CommandCtx, ExtCommand, ExtModule, ExtTierRunner, Extension, TierRoots,
    TierRun,
};

use crate::cmd::bench::cmd_bench;
use crate::cmd::doc::cmd_doc;
use crate::cmd::test::cmd_test;

/// The flags `noeta test` parsed, stashed for the native runner (the seam carries no flags).
#[derive(Clone, Default)]
pub(crate) struct TestOpts {
    pub fail_fast: bool,
    pub jobs: Option<usize>,
    pub group: Option<String>,
    pub names: Vec<String>,
    pub json: bool,
    pub target: Option<String>,
    /// `--timeout <SECONDS>`; `None` leaves the runner's own default in place.
    pub timeout: Option<u64>,
}

/// The flags `noeta bench` parsed, stashed for the native runner.
#[derive(Clone, Default)]
pub(crate) struct BenchOpts {
    pub iterations: Option<u64>,
    pub names: Vec<String>,
    pub json: bool,
    pub save_baseline: Option<String>,
    pub baseline: Option<String>,
    pub max_regress: Option<f64>,
    pub target: Option<String>,
}

/// The flags `noeta doc` parsed, stashed for the native runner. Carries the (optional) entry file
/// itself: `doc`'s file argument is optional and its `--api`/`--package`/directory variants are not
/// file-driven tier runs, so the wrapper reads the file from here rather than the seam's `TierRun`.
#[derive(Clone, Default)]
pub(crate) struct DocOpts {
    pub file: Option<PathBuf>,
    pub package: Option<String>,
    pub out: Option<PathBuf>,
    pub target: Option<String>,
    pub api: bool,
    pub root: Option<String>,
    pub non_builtin: bool,
    pub lint: bool,
}

thread_local! {
    static TEST_OPTS: RefCell<TestOpts> = RefCell::new(TestOpts::default());
    static BENCH_OPTS: RefCell<BenchOpts> = RefCell::new(BenchOpts::default());
    static DOC_OPTS: RefCell<DocOpts> = RefCell::new(DocOpts::default());
}

/// Stash the flags `noeta test` parsed, for the native `test` runner to read back.
pub(crate) fn set_test_opts(opts: TestOpts) {
    TEST_OPTS.with(|c| *c.borrow_mut() = opts);
}

/// Stash the flags `noeta bench` parsed, for the native `bench` runner to read back.
pub(crate) fn set_bench_opts(opts: BenchOpts) {
    BENCH_OPTS.with(|c| *c.borrow_mut() = opts);
}

/// Stash the flags `noeta doc` parsed, for the native `doc` runner to read back.
pub(crate) fn set_doc_opts(opts: DocOpts) {
    DOC_OPTS.with(|c| *c.borrow_mut() = opts);
}

/// The native `test` runner: drive `cmd_test` over the entry the seam names, with the clap flags the
/// verb stashed (defaults on the generic `noeta test`-shaped path, which parses none).
fn test_runner(_ctx: &mut dyn CommandCtx, run: &TierRun<'_>) -> u8 {
    let o = TEST_OPTS.with(|c| std::mem::take(&mut *c.borrow_mut()));
    cmd_test(run.file, &o)
}

/// The native `bench` runner: drive `cmd_bench` over the entry the seam names, with the stashed flags.
fn bench_runner(_ctx: &mut dyn CommandCtx, run: &TierRun<'_>) -> u8 {
    let o = BENCH_OPTS.with(|c| std::mem::take(&mut *c.borrow_mut()));
    cmd_bench(
        run.file,
        o.iterations,
        &o.names,
        o.json,
        &o.save_baseline,
        &o.baseline,
        o.max_regress,
        &o.target,
    )
}

/// The native `doc` runner: drive `cmd_doc` with the stashed flags (including its own optional file —
/// `doc` is not purely file-driven, so it ignores the seam's `TierRun::file`).
fn doc_runner(_ctx: &mut dyn CommandCtx, _run: &TierRun<'_>) -> u8 {
    let o = DOC_OPTS.with(|c| std::mem::take(&mut *c.borrow_mut()));
    cmd_doc(
        &o.file,
        &o.package,
        &o.out,
        &o.target,
        o.api,
        o.root.as_deref(),
        o.non_builtin,
        o.lint,
    )
}

/// std's native tier runners, keyed to the `std` `ExtTier`s of the same name.
const STD_TIER_RUNNERS: &[ExtTierRunner] = &[
    ExtTierRunner {
        tier: "test",
        run: test_runner,
    },
    ExtTierRunner {
        tier: "bench",
        run: bench_runner,
    },
    ExtTierRunner {
        tier: "doc",
        run: doc_runner,
    },
];

/// std's three dev-tier **commands**, declared exactly as any package declares one. `noeta test`
/// is not a core verb the binary hardcodes: it is an [`ExtCommand`] std contributes, registered by
/// default because std ships with the toolchain — which is what makes it *replaceable*. A
/// `[trust.commands]` binding under the same local name takes the name over, so a project can put
/// a third-party test runner behind `noeta test` and get that runner's own flags and help, not a
/// third-party body wearing std's argument list.
static STD_COMMANDS: &[ExtCommand] = &[TEST_COMMAND, BENCH_COMMAND, DOC_COMMAND];

/// `noeta test` — discover and run a program's `@test` blocks.
const TEST_COMMAND: ExtCommand = ExtCommand {
    name: "test",
    about: "Discover and run a program's `@test` blocks",
    args: &[
        ArgSpec {
            name: "file",
            help: "File or directory to test (default: the current directory, walked recursively \
                   for `.noe` files). A directory runs every file's `@test` blocks as its own entry \
                   and aggregates one report; a file tests just that one",
            kind: ArgKind::PathDefault { default: "." },
            ..ArgSpec::DEFAULTS
        },
        ArgSpec {
            name: "fail-fast",
            help: "Stop after the first failing test instead of running them all",
            kind: ArgKind::Bool,
            ..ArgSpec::DEFAULTS
        },
        ArgSpec {
            name: "jobs",
            help: "Number of tests to run concurrently (default: the machine's parallelism)",
            kind: ArgKind::OptInt,
            short: Some('j'),
        },
        ArgSpec {
            name: "group",
            help: "Run only tests tagged `#[Group(\"<name>\")]` with this group",
            kind: ArgKind::OptStr,
            ..ArgSpec::DEFAULTS
        },
        ArgSpec {
            name: "name",
            help: "Run only the test fn(s) with these names (repeatable; exact fn-name match). \
                   Composes with --group",
            kind: ArgKind::Strings,
            ..ArgSpec::DEFAULTS
        },
        ArgSpec {
            name: "json",
            help: "Report outcomes as one JSON object on stdout instead of the human report",
            kind: ArgKind::Bool,
            ..ArgSpec::DEFAULTS
        },
        ArgSpec {
            name: "target",
            help: "Only run when the `test` tier is live in this `noeta.toml` build target; \
                   otherwise the runner does nothing",
            kind: ArgKind::OptStr,
            ..ArgSpec::DEFAULTS
        },
        ArgSpec {
            name: "timeout",
            help: "Per-test deadline in seconds (default: 60). A test that overruns is reported \
                   `TIME`; `--timeout 0` removes the bound for the whole run",
            kind: ArgKind::OptInt,
            ..ArgSpec::DEFAULTS
        },
    ],
    run: |_ctx, args| {
        set_test_opts(TestOpts {
            fail_fast: args.bool("fail-fast"),
            jobs: args
                .get_int("jobs")
                .and_then(non_negative)
                .map(|n| n as usize),
            group: args.get_str("group").map(str::to_string),
            names: args.strs("name").to_vec(),
            json: args.bool("json"),
            target: args.get_str("target").map(str::to_string),
            timeout: args.get_int("timeout").and_then(non_negative),
        });
        dispatch_std_tier("test", args.path("file"))
    },
};

/// `noeta bench` — discover and measure a program's `@bench` blocks.
const BENCH_COMMAND: ExtCommand = ExtCommand {
    name: "bench",
    about: "Discover and run a program's `@bench` blocks, measuring each",
    args: &[
        ArgSpec {
            name: "file",
            help: "File or directory to benchmark (default: the current directory, walked \
                   recursively for `.noe` files). Baselines stay keyed per entry file, so a \
                   directory run compares like with like",
            kind: ArgKind::PathDefault { default: "." },
            ..ArgSpec::DEFAULTS
        },
        ArgSpec {
            name: "iterations",
            help: "Override the iteration count for every benchmark, taking precedence over a \
                   per-bench `@bench(iterations: N)`. Without either, the count is calibrated",
            kind: ArgKind::OptInt,
            ..ArgSpec::DEFAULTS
        },
        ArgSpec {
            name: "name",
            help: "Run only the bench fn(s) with these names (repeatable; exact fn-name match)",
            kind: ArgKind::Strings,
            ..ArgSpec::DEFAULTS
        },
        ArgSpec {
            name: "json",
            help: "Report results as one JSON object on stdout instead of the human report",
            kind: ArgKind::Bool,
            ..ArgSpec::DEFAULTS
        },
        ArgSpec {
            name: "save-baseline",
            help: "After measuring, save the results as the named baseline (in the noeta cache \
                   dir, per-entry-file — timings are machine-local)",
            kind: ArgKind::OptStr,
            ..ArgSpec::DEFAULTS
        },
        ArgSpec {
            name: "baseline",
            help: "Compare each result against the named baseline: the report gains a delta \
                   column, the JSON a `baselineDeltaPct` field",
            kind: ArgKind::OptStr,
            ..ArgSpec::DEFAULTS
        },
        ArgSpec {
            name: "max-regress",
            help: "The CI regression gate: with --baseline, fail (exit 1) when any bench regresses \
                   more than this percentage against it. Exits 2 when it could not judge a bench",
            kind: ArgKind::OptFloat,
            ..ArgSpec::DEFAULTS
        },
        ArgSpec {
            name: "target",
            help: "Only run when the `bench` tier is live in this `noeta.toml` build target; \
                   otherwise the runner does nothing",
            kind: ArgKind::OptStr,
            ..ArgSpec::DEFAULTS
        },
    ],
    run: |_ctx, args| {
        // `--max-regress` gates against a baseline, so it means nothing without one. The clap
        // derive spelled this `requires = "baseline"`; a declared command owns its own grammar
        // (see `ArgKind::Word`), so the check lives here — and says what to do about it.
        if args.get_float("max-regress").is_some() && args.get_str("baseline").is_none() {
            eprintln!(
                "noeta: `--max-regress` is the gate against a baseline — pass `--baseline <NAME>` \
                 too, or drop it"
            );
            return 2;
        }
        set_bench_opts(BenchOpts {
            iterations: args.get_int("iterations").and_then(non_negative),
            names: args.strs("name").to_vec(),
            json: args.bool("json"),
            save_baseline: args.get_str("save-baseline").map(str::to_string),
            baseline: args.get_str("baseline").map(str::to_string),
            max_regress: args.get_float("max-regress"),
            target: args.get_str("target").map(str::to_string),
        });
        dispatch_std_tier("bench", args.path("file"))
    },
};

/// `noeta doc` — extract `@doc` prose, or generate the package's documentation artifact.
const DOC_COMMAND: ExtCommand = ExtCommand {
    name: "doc",
    about: "Extract a program's `@doc { … }` text blocks to stdout, or — with `--out` — generate \
            the package's documentation artifact",
    args: &[
        ArgSpec {
            name: "file",
            help: "File or directory to document (default: the current directory when no \
                   --package is given). A file extracts that file and its sibling modules",
            kind: ArgKind::Word,
            ..ArgSpec::DEFAULTS
        },
        ArgSpec {
            name: "package",
            help: "Fetch a published package's stored documentation from the registry instead of \
                   reading local source: `company/package[@1.2.0]`",
            kind: ArgKind::OptStr,
            ..ArgSpec::DEFAULTS
        },
        ArgSpec {
            name: "out",
            help: "Generate the documentation artifact into this directory instead of extracting \
                   to stdout: `docs.json` plus `index.md` and one Markdown page per module",
            kind: ArgKind::OptPath,
            short: Some('o'),
        },
        ArgSpec {
            name: "target",
            help: "Only extract when the `doc` tier is live in this `noeta.toml` build target",
            kind: ArgKind::OptStr,
            ..ArgSpec::DEFAULTS
        },
        ArgSpec {
            name: "api",
            help: "Generate the API reference from the intrinsic registry (the stdlib and any \
                   composed native modules) instead of from `.noe` source",
            kind: ArgKind::Bool,
            ..ArgSpec::DEFAULTS
        },
        ArgSpec {
            name: "root",
            help: "With --api, document only the extensions whose namespace root is this",
            kind: ArgKind::OptStr,
            ..ArgSpec::DEFAULTS
        },
        ArgSpec {
            name: "non-builtin",
            help: "With --api, document every registered NON-BUILTIN extension — in a package's \
                   composed toolchain, exactly the package's own surface",
            kind: ArgKind::Bool,
            ..ArgSpec::DEFAULTS
        },
        ArgSpec {
            name: "lint",
            help: "With --api --root or --api --non-builtin, fail (before emitting docs) if the \
                   scoped extensions register any surface outside their own namespace root(s)",
            kind: ArgKind::Bool,
            ..ArgSpec::DEFAULTS
        },
    ],
    run: |_ctx, args| {
        let file = args.get_str("file").map(PathBuf::from);
        let package = args.get_str("package").map(str::to_string);
        let (api, root, non_builtin, lint) = (
            args.bool("api"),
            args.get_str("root").map(str::to_string),
            args.bool("non-builtin"),
            args.bool("lint"),
        );
        // The four source-selection relations the clap derive expressed as `conflicts_with` /
        // `requires` / a shared `group`. Each names both sides, because "invalid combination" is
        // only useful if it says which two.
        let scoped = root.is_some() || non_builtin;
        if let Some(bad) = doc_arg_conflict(file.is_some(), package.is_some(), api, scoped, lint) {
            eprintln!("noeta: {bad}");
            return 2;
        }
        set_doc_opts(DocOpts {
            // The runner reads the entry back from here (`doc`'s file argument is optional and its
            // --api/--package variants are not file-driven), so the seam's `TierRun` carries `.`
            // only for contract fidelity.
            file: file.clone(),
            package,
            out: args.get_path("out").map(Path::to_path_buf),
            target: args.get_str("target").map(str::to_string),
            api,
            root,
            non_builtin,
            lint,
        });
        dispatch_std_tier("doc", &file.unwrap_or_else(|| PathBuf::from(".")))
    },
};

/// Which of `noeta doc`'s mutually exclusive source selections were combined, as the message to
/// print. `None` when the combination is valid.
fn doc_arg_conflict(
    file: bool,
    package: bool,
    api: bool,
    scoped: bool,
    lint: bool,
) -> Option<&'static str> {
    match () {
        _ if file && package => Some(
            "`--package` documents a published package and a path documents local source — pass one",
        ),
        _ if api && (file || package) => Some(
            "`--api` documents the registry, not source — drop the path/`--package`, or drop `--api`",
        ),
        _ if scoped && !api => {
            Some("`--root`/`--non-builtin` scope the `--api` reference — pass `--api` too")
        }
        _ if lint && !scoped => Some(
            "`--lint` checks a scoped API reference — pass `--api` with `--root <NS>` or \
             `--non-builtin`",
        ),
        _ => None,
    }
}

/// A non-negative CLI integer as a `u64` — the seam parses every integer as `i64`, and the three
/// counts these commands take (`--jobs`, `--timeout`, `--iterations`) have no meaning below zero.
/// A negative value reads as "not given" rather than wrapping into an enormous count.
fn non_negative(value: i64) -> Option<u64> {
    u64::try_from(value).ok()
}

/// The CLI-layer extension unit that registers std's native dev-tier runners **and the three
/// commands that drive them**. It declares no modules or types: it exists to attach both to the
/// `std` root, so a scoped lookup (`find_tier_runner_scoped(&["std"], …)`) resolves the runners the
/// way it resolves any provider's, and `[trust.commands]` can rebind the commands the way it binds
/// any package's.
pub(crate) struct StdTierRunners;

impl Extension for StdTierRunners {
    fn name(&self) -> &'static str {
        "std.tier-runners"
    }
    fn root(&self) -> &'static str {
        "std"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[]
    }
    fn tier_runners(&self) -> &'static [ExtTierRunner] {
        STD_TIER_RUNNERS
    }
    fn commands(&self) -> &'static [ExtCommand] {
        STD_COMMANDS
    }
}

/// The singleton unit `run_cli` installs alongside the other first-party CLI extensions.
pub(crate) static STD_TIER_RUNNERS_UNIT: StdTierRunners = StdTierRunners;

/// Invoke a native tier runner over `file` with the seam's roots riding empty (std's native runners
/// are file-driven — they re-collect their own roots), shaped `Text` vs `Code` per `is_text` so a
/// runner that reads them sees the right arm. Stays in the seam's own `u8`: a command body returns
/// one, and re-wrapping it as an `ExitCode` here only to unwrap it there loses any code above 2.
fn run_native(runner: &ExtTierRunner, is_text: bool, file: &Path) -> u8 {
    let roots = if is_text {
        TierRoots::Text(&[])
    } else {
        TierRoots::Code(&[])
    };
    let run = TierRun { file, roots };
    let mut ctx = crate::cmd::serve::CliCommandCtx;
    (runner.run)(&mut ctx, &run)
}

/// Invoke a resolved native tier runner over `file`, resolving its `Code`/`Text` arm from the tier's
/// bare-name declaration.
fn invoke_tier_runner(runner: &ExtTierRunner, name: &str, file: &Path) -> u8 {
    let is_text = noeta_stdlib::registry::single_registry_process()
        .find_ext_tier(name)
        .is_some_and(|t| t.text.is_some());
    run_native(runner, is_text, file)
}

/// Dispatch a **std** dev-tier (`test`/`bench`/`doc`) through the registry seam: resolve std's
/// native runner by identity (`find_tier_runner_scoped(&["std"], name)`) and invoke it — the single
/// path std's three commands enter, so `noeta test` reaches its executor exactly as a program
/// `@tier` and a third-party tier reach theirs. A provider override (`--target`, or a `[directives]`
/// binding, mapping the tier to a package) is still honored: `cmd_test`/`cmd_bench`/`cmd_doc`
/// re-resolve it in their own tier prologue and redirect to that package's `@tier` runner.
fn dispatch_std_tier(name: &str, file: &Path) -> u8 {
    match noeta_stdlib::registry::single_registry_process()
        .find_tier_runner_scoped(&["std".to_string()], name)
    {
        Some(runner) => invoke_tier_runner(runner, name, file),
        None => {
            // The three std runners register at startup; a missing one means a mis-assembled binary.
            eprintln!("noeta: internal error: no native runner registered for tier `{name}`");
            2
        }
    }
}

/// Dispatch a **generic** `noeta <tier> <file>` whose name resolves to an extension tier that ships
/// a native runner — the unification for the fall-through path (lib's `try_tier_dispatch`): a
/// third-party package that declares a tier *and* registers its runner dispatches here, on the same
/// resolve-then-invoke path std's verbs use. `None` when no runner is registered for `name` (an
/// inline-only or expression tier), so the caller falls through to the external-binary probe.
pub(crate) fn dispatch_generic_tier(name: &str, file: &Path) -> Option<ExitCode> {
    noeta_stdlib::registry::single_registry_process()
        .find_tier_runner(name)
        .map(|runner| ExitCode::from(invoke_tier_runner(runner, name, file)))
}

/// Dispatch a tier the root **renamed** in `[directives]`, resolved by identity to a
/// `(provider_root, exported)` pair — look up the native runner **scoped** to that provider (never
/// the literal local name), so a renamed std tier (`mytest = "std:test"`) or a collision the root
/// disambiguated (`crit = "depB:fuzz"`, where two dependencies each export a `fuzz` runner) reaches
/// exactly the provider it named. `None` when that provider ships no native runner for the tier — an
/// inline-only/expression extension tier, or a **program-declared** `@tier` runner (which the caller
/// dispatches through `run_declared_tier` instead).
pub(crate) fn dispatch_scoped_tier(
    provider_root: &str,
    exported: &str,
    file: &Path,
) -> Option<ExitCode> {
    let reg = noeta_stdlib::registry::single_registry_process();
    let root = [provider_root.to_string()];
    reg.find_tier_runner_scoped(&root, exported).map(|runner| {
        let is_text = reg
            .find_ext_tier_scoped(&root, exported)
            .is_some_and(|t| t.text.is_some());
        ExitCode::from(run_native(runner, is_text, file))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// std's three dev-tier verbs are declared commands, not core verbs — the property that makes a
    /// `[trust.commands]` binding able to take their names over. Asserted on the declaration itself
    /// so a verb quietly moving back into the clap derive fails here rather than in the one
    /// composed-toolchain e2e that can observe the override.
    #[test]
    fn std_contributes_its_dev_tier_verbs_as_commands() {
        let declared: Vec<&str> = STD_COMMANDS.iter().map(|c| c.name).collect();
        assert_eq!(declared, ["test", "bench", "doc"]);
        assert_eq!(
            StdTierRunners.commands().len(),
            3,
            "the `std`-rooted unit registers them, so `run_cli` finds them by root"
        );
        // Every flag the derive spelled still has a declaration: a silently dropped one would only
        // surface as a user's script failing on an unknown argument.
        let test_args: Vec<&str> = TEST_COMMAND.args.iter().map(|a| a.name).collect();
        assert_eq!(
            test_args,
            [
                "file",
                "fail-fast",
                "jobs",
                "group",
                "name",
                "json",
                "target",
                "timeout"
            ]
        );
        let bench_args: Vec<&str> = BENCH_COMMAND.args.iter().map(|a| a.name).collect();
        assert_eq!(
            bench_args,
            [
                "file",
                "iterations",
                "name",
                "json",
                "save-baseline",
                "baseline",
                "max-regress",
                "target"
            ]
        );
        let doc_args: Vec<&str> = DOC_COMMAND.args.iter().map(|a| a.name).collect();
        assert_eq!(
            doc_args,
            [
                "file",
                "package",
                "out",
                "target",
                "api",
                "root",
                "non-builtin",
                "lint"
            ]
        );
    }

    /// `noeta doc`'s source-selection relations, which the clap derive expressed as
    /// `conflicts_with`/`requires`/a shared group and a declared command owns itself. Each invalid
    /// combination is rejected, and every valid one — including the bare invocation — passes.
    #[test]
    fn doc_rejects_only_the_invalid_source_combinations() {
        // (file, package, api, scoped, lint)
        assert!(doc_arg_conflict(true, true, false, false, false).is_some());
        assert!(doc_arg_conflict(true, false, true, false, false).is_some());
        assert!(doc_arg_conflict(false, true, true, false, false).is_some());
        assert!(doc_arg_conflict(false, false, false, true, false).is_some());
        assert!(doc_arg_conflict(false, false, true, false, true).is_some());
        assert!(doc_arg_conflict(false, false, false, false, false).is_none());
        assert!(doc_arg_conflict(true, false, false, false, false).is_none());
        assert!(doc_arg_conflict(false, true, false, false, false).is_none());
        assert!(doc_arg_conflict(false, false, true, false, false).is_none());
        assert!(doc_arg_conflict(false, false, true, true, true).is_none());
    }

    /// A negative count is not a count: `--jobs -1` reads as unset rather than wrapping into
    /// 18 quintillion threads, which is what a bare `as u64` cast would have done.
    #[test]
    fn negative_counts_read_as_unset() {
        assert_eq!(non_negative(4), Some(4));
        assert_eq!(non_negative(0), Some(0));
        assert_eq!(non_negative(-1), None);
    }

    /// Part B's core: std's `test`/`bench`/`doc` runners register from the CLI layer and resolve
    /// through the registry seam by identity, exactly as a program `@tier` runner and a third-party
    /// tier runner do — so `noeta test` (and its siblings) reach their native executor through
    /// `find_tier_runner_scoped`, not a hardcoded native call. Assembled explicitly (the process
    /// default is seeded by `run_cli`, which a lib test does not call).
    #[test]
    fn std_native_runners_resolve_through_the_registry_seam() {
        let reg = noeta_stdlib::registry::assemble_with_extras(&[&STD_TIER_RUNNERS_UNIT]);
        for tier in ["test", "bench", "doc"] {
            // Scoped to the `std` provider root — the identity the std tiers carry — so a same-named
            // tier from another provider never shadows std's native runner.
            assert!(
                reg.find_tier_runner_scoped(&["std".to_string()], tier)
                    .is_some(),
                "std native `{tier}` runner resolves scoped to the `std` root"
            );
            // And by bare name, the generic `noeta <tier>` fall-through path.
            assert_eq!(reg.find_tier_runner(tier).map(|r| r.tier), Some(tier));
        }
        // `debug` is an inline-only std tier with no native runner — it resolves to none, so the
        // generic path falls through rather than dispatching an empty runner.
        assert!(reg.find_tier_runner("debug").is_none());
        // A foreign provider root does not match std's runners (the whole point of the scoped twin).
        assert!(
            reg.find_tier_runner_scoped(&["other".to_string()], "test")
                .is_none(),
            "a foreign provider root does not resolve std's runner"
        );
    }
}
