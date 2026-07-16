//! The shared tier-verb prologue (audit-4 finding 3) and the crate's editions-threading choke
//! points. `noeta test`/`noeta bench` used to repeat the same ~45-line sequence — compose
//! delegation → target gate → dep-aware load+link → provider resolution → tier activation →
//! declared-provider dispatch → activation diagnostics → editions-threaded type check — and the
//! three copies had to stay in lockstep by discipline. [`tier_prologue`] is that sequence once;
//! `noeta doc` (whose native path deliberately neither activates nor type-checks) shares the
//! same building blocks ([`target_gate`], [`load_linked`], [`provider_escape`]).
//!
//! Editions threading is structural here (audit-3 F8): a whole linked program is checked through
//! the runner's [`Loaded::check`] (via [`loaded`]), and a program *synthesized from* one (a
//! per-test case, a bench loop, a tier-dispatch call) through [`check_under`] — the crate's one
//! remaining hand-paired `check_all_with_editions` call.

use std::process::ExitCode;

use noeta_runner::compile::Loaded;

use crate::output::emit_diagnostics_mapped;
use crate::{compose, run_declared_tier};

/// For a tier runner: whether its `tier` is live under `--target`. `Ok(true)` when no target was
/// given (the runner always runs); `Ok(false)` when a target was given but does not make `tier`
/// live (the runner should no-op); `Err` on a target-resolution failure (a fatal error the caller
/// prints).
/// The active tier → provider map for `--target` (empty with no target — default resolution:
/// extension declarations first). The tier-execution layer dispatches on this: `"std"` runs the
/// native built-in runner, a dependency key runs that package's `@tier` runner.
fn tier_active_in_target(
    entry: &std::path::Path,
    target: &Option<String>,
    tier: &str,
) -> Result<bool, String> {
    match target {
        None => Ok(true),
        Some(name) => Ok(noeta_pm::manifest::resolve_active_tiers(entry, name)?
            .iter()
            .any(|t| t == tier)),
    }
}

/// Gate a tier runner on `--target`: if a target was given and does not make `tier` live, print a
/// note and return the success exit code (the runner no-ops); on a resolution failure, print it and
/// return the error code. `None` means "proceed" (no target gate). The caller runs its body only
/// when this returns `None`.
pub(crate) fn target_gate(
    entry: &std::path::Path,
    target: &Option<String>,
    tier: &str,
) -> Option<ExitCode> {
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
            eprintln!("noeta: {err}");
            Some(ExitCode::from(1))
        }
    }
}

/// Load and link `file` **with its dependency packages resolved** — the same dep-aware path
/// `run`/`check` use — rendering any failure. The tier runners (`test`/`bench`/`doc`) share this:
/// a program whose tier content imports from a dependency (a test exercising `use fuzzkit.…`, a
/// declared tier's runner) must link exactly as it does for a run. `resolved` is the graph the
/// caller's compose probe already resolved, reused here — the tier verbs always load under the
/// default selection, so it always matches (audit-5 F2).
pub(crate) fn load_linked(
    file: &std::path::Path,
    resolved: Option<noeta_pm::graph::ResolvedGraph>,
) -> Result<noeta_loader::Linked, ExitCode> {
    // The shared front half (drift firewall): deps + edition resolve exactly as `noeta run`'s
    // pipeline resolves them; the verbs stage tier activation themselves.
    let facts =
        noeta_runner::compile::resolve_front_with(file, &[], &None, resolved.map(|g| g.packages))
            .map_err(|f| f.report())?;
    noeta_runner::compile::load_linked(file, &facts).map_err(|f| f.report())
}

/// Rewrap a linker result as the runner's [`Loaded`], so type-checking goes through
/// [`Loaded::check`] — the editions-threading choke point — instead of a hand-paired
/// `check_all_with_editions(&linked.program, linked.editions.clone())`.
pub(crate) fn loaded(linked: noeta_loader::Linked) -> Loaded {
    Loaded {
        program: linked.program,
        sources: linked.sources,
        editions: linked.editions,
    }
}

/// Type-check a program **synthesized from** an already-linked one (a per-test case, a bench
/// measurement loop, a tier-dispatch program, a hot-swap candidate) under the parent workspace's
/// editions. SourceIds survive the synthesis (the new nodes carry existing or synthetic spans),
/// so the parent's edition map stays valid. The crate's single hand-written
/// `check_all_with_editions` call site — whole linked programs go through [`Loaded::check`].
pub(crate) fn check_under(
    program: &noeta_ast::Program,
    editions: &noeta_lexer::EditionMap,
) -> noeta_check::Checked {
    noeta_check::check_all_with_editions(program, editions.clone())
}

/// Resolve which provider owns `tier` under the target's map and, when a dependency package's
/// **declared** `@tier` runner owns it, dispatch there in-process (provider dispatch). Returns
/// the activation back (`Ok`) when the native/extension runner should proceed, or the finished
/// exit code (`Err`) when the invocation was fully handled — dispatched, or failed to resolve.
pub(crate) fn provider_escape(
    tier: &str,
    linked: &noeta_loader::Linked,
    activated: noeta_check::Activated,
    providers: &std::collections::BTreeMap<String, String>,
) -> Result<noeta_check::Activated, ExitCode> {
    match activated.registry.resolve_provider(tier, providers) {
        Ok(noeta_check::ResolvedProvider::Extension) => Ok(activated),
        Ok(noeta_check::ResolvedProvider::Declared(d)) => {
            let decl = d.clone();
            Err(run_declared_tier(tier, linked, activated, decl))
        }
        Err(err) => {
            eprintln!("noeta: {err}");
            Err(ExitCode::from(2))
        }
    }
}

/// A tier verb's program, ready to run natively: the activated program (with its collected tier
/// fns) plus the workspace editions its per-case checks run under. The prologue already gated on
/// activation and whole-program type-check diagnostics (rendered against the workspace sources).
pub(crate) struct TierRun {
    pub(crate) activated: noeta_check::Activated,
    pub(crate) editions: noeta_lexer::EditionMap,
}

/// The outcome of [`tier_prologue`]: either the invocation was fully handled (delegated to a
/// composed toolchain, gated off by `--target`, dispatched to a declared provider, or failed —
/// return the exit code), or the program is loaded, activated, and checked clean under the
/// native runner.
pub(crate) enum Prologue {
    Ran(ExitCode),
    /// Boxed: `TierRun` carries a whole activated program, and the enum crosses a return
    /// boundary per verb invocation (clippy::large_enum_variant).
    Ready(Box<TierRun>),
}

/// The shared prologue of the native tier runners (`noeta test`/`noeta bench`): one copy of the
/// policy-bearing sequence whose order must not drift between verbs. Only the tier name differs;
/// everything after `Ready` is the verb's own body.
pub(crate) fn tier_prologue(
    file: &std::path::Path,
    tier: &str,
    target: &Option<String>,
) -> Prologue {
    // The compose probe hands back the graph it resolved (default selection) for the load below
    // — the tier verbs always load under the default selection (audit-5 F2).
    let resolved = match compose::maybe_delegate(file) {
        Err(code) => return Prologue::Ran(code),
        Ok(resolved) => resolved,
    };
    if let Some(code) = target_gate(file, target, tier) {
        return Prologue::Ran(code);
    }
    let linked = match load_linked(file, resolved) {
        Ok(linked) => linked,
        Err(code) => return Prologue::Ran(code),
    };
    // The target's provider selection (provider dispatch): `<tier> = "<pkg>"` in the target's
    // tiers map hands this tier to that package's `@tier` runner instead of the native one.
    // Default (no target, or `"std"`) keeps the native path.
    let providers = match noeta_runner::resolve_providers(file, target) {
        Ok(map) => map,
        Err(err) => {
            eprintln!("noeta: {err}");
            return Prologue::Ran(ExitCode::from(2));
        }
    };
    // Activate the tier: inline its `@<tier>` blocks as ordinary top-level declarations and
    // collect the tier fns. An unknown-tier block is an E0036 (a typo must not silently vanish).
    let activated = noeta_check::activate_tiers_with(&linked.program, &[tier], &providers);
    let mut activated = match provider_escape(tier, &linked, activated, &providers) {
        Ok(activated) => activated,
        Err(code) => return Prologue::Ran(code),
    };
    if !activated.diagnostics.is_empty() {
        emit_diagnostics_mapped(&linked.sources, activated.diagnostics.iter());
        return Prologue::Ran(ExitCode::from(1));
    }

    // Type-check the activated program once — through the runner's `Loaded`, so the per-source
    // editions ride structurally — so a broken tier fn is a compile error reported a single time
    // here rather than redundantly inside every per-case run.
    let noeta_loader::Linked {
        sources, editions, ..
    } = linked;
    let checking = Loaded {
        program: activated.program,
        sources,
        editions,
    };
    let checked = checking.check();
    if !checked.diagnostics.is_empty() {
        emit_diagnostics_mapped(&checking.sources, checked.diagnostics.iter());
        return Prologue::Ran(ExitCode::from(1));
    }
    let Loaded {
        program, editions, ..
    } = checking;
    activated.program = program;
    Prologue::Ready(Box::new(TierRun {
        activated,
        editions,
    }))
}
