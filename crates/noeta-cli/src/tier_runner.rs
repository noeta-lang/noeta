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

use noeta_stdlib::{CommandCtx, ExtModule, ExtTierRunner, Extension, TierRoots, TierRun};

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
    cmd_test(
        run.file,
        o.fail_fast,
        o.jobs,
        &o.group,
        &o.names,
        o.json,
        &o.target,
    )
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

/// The CLI-layer extension unit that registers std's native dev-tier runners. It declares **no**
/// modules or types — it exists only to attach the three runners to the `std` root, so a scoped
/// lookup (`find_tier_runner_scoped(&["std"], …)`) resolves them the way it resolves any provider's.
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
}

/// The singleton unit `run_cli` installs alongside the other first-party CLI extensions.
pub(crate) static STD_TIER_RUNNERS_UNIT: StdTierRunners = StdTierRunners;

/// Invoke a resolved native tier runner over `file`, adapting the seam's `u8` back to the CLI's
/// `ExitCode`. std's native runners are file-driven, so the seam's roots ride empty; their `Code`
/// vs `Text` shape still tracks the tier's declaration so a runner that reads them sees the right
/// arm.
fn invoke_tier_runner(runner: &ExtTierRunner, name: &str, file: &Path) -> ExitCode {
    let is_text = noeta_stdlib::registry::single_registry_process()
        .find_ext_tier(name)
        .is_some_and(|t| t.text.is_some());
    let roots = if is_text {
        TierRoots::Text(&[])
    } else {
        TierRoots::Code(&[])
    };
    let run = TierRun { file, roots };
    let mut ctx = crate::cmd::serve::CliCommandCtx;
    ExitCode::from((runner.run)(&mut ctx, &run))
}

/// Dispatch a **std** dev-tier (`test`/`bench`/`doc`) through the registry seam: resolve std's
/// native runner by identity (`find_tier_runner_scoped(&["std"], name)`) and invoke it — the single
/// path the clap verbs enter, so `noeta test` reaches its executor exactly as a program `@tier` and
/// a third-party tier reach theirs. A provider override (`--target` mapping the tier to a package)
/// is still honored: `cmd_test`/`cmd_bench`/`cmd_doc` re-resolve it in their own tier prologue and
/// redirect to that package's `@tier` runner.
pub(crate) fn dispatch_std_tier(name: &str, file: &Path) -> ExitCode {
    match noeta_stdlib::registry::single_registry_process()
        .find_tier_runner_scoped(&["std".to_string()], name)
    {
        Some(runner) => invoke_tier_runner(runner, name, file),
        None => {
            // The three std runners register at startup; a missing one means a mis-assembled binary.
            eprintln!("noeta: internal error: no native runner registered for tier `{name}`");
            ExitCode::from(2)
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
        .map(|runner| invoke_tier_runner(runner, name, file))
}

#[cfg(test)]
mod tests {
    use super::*;

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
