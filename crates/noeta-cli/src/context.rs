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

use noeta_runner::compile::Loaded;

use crate::output::emit_diagnostics_mapped;
use crate::{compose, run_declared_tier};

/// Print a [`noeta_runner::CompileFailure`] to stderr and yield its **`u8`** exit code — the tier
/// subsystem speaks the native-runner seam's `u8` exit codes (0 ok, 1 program error, 2 setup
/// failure) end to end, so `CompileFailure::report` (which returns a `std::process::ExitCode` the
/// seam cannot read) is unwrapped to the code here. `ExitCode` reappears only at the CLI's outer
/// clap boundary, where a verb wraps this crate's `u8` back into one.
pub(crate) fn report_u8(f: &noeta_runner::CompileFailure) -> u8 {
    f.report_u8()
}

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
) -> Option<u8> {
    match tier_active_in_target(entry, target, tier) {
        Ok(true) => None,
        Ok(false) => {
            println!(
                "tier `{tier}` is not active in target `{}`",
                target.as_deref().unwrap_or_default()
            );
            Some(0)
        }
        Err(err) => {
            eprintln!("noeta: {err}");
            Some(1)
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
) -> Result<noeta_loader::Linked, u8> {
    load_entry_with_tail(file, &[], Front::Resolve(resolved.map(Box::new)))
        .map(|entry| entry.linked)
        .map_err(|f| report_u8(&f))
}

/// Where the dependency graph a link runs under comes from (audit-10).
///
/// The resolved graph is boxed: it is an order of magnitude larger than an `Arc`, and this enum is
/// passed by value at every call.
pub(crate) enum Front {
    /// Resolve it — reusing the graph the caller's compose probe already resolved when it has one
    /// (audit-5 F2), else resolving here so the error renders on this path.
    Resolve(Option<Box<noeta_pm::graph::ResolvedGraph>>),
    /// **The facts a boot already resolved.** The hot re-link's source: every re-link of a running
    /// server must link against the graph the running program was built with, not against whatever
    /// a fresh resolve finds a hundred edits later. See [`load_entry_with_tail`].
    Given(std::sync::Arc<noeta_runner::compile::FrontFacts>),
}

/// A linked entry program, its rewrapped [`Loaded`] view, and the facts it linked under.
pub(crate) struct EntryProgram {
    pub(crate) linked: noeta_loader::Linked,
    /// The **entry file's own source**, kept past the [`loaded`] rewrap: the hot path's diff
    /// baseline is the entry unit of *this* program, the one about to be compiled and served.
    pub(crate) entry: noeta_span::Source,
    /// The front facts this link ran under — handed to the hot watcher so its re-links are the
    /// boot's ([`Front::Given`]).
    pub(crate) front: std::sync::Arc<noeta_runner::compile::FrontFacts>,
}

impl EntryProgram {
    /// The runner's [`Loaded`] view, for the type check ([`loaded`]). Consumes the linked program.
    pub(crate) fn into_loaded(
        self,
    ) -> (
        Loaded,
        noeta_span::Source,
        std::sync::Arc<noeta_runner::compile::FrontFacts>,
    ) {
        (loaded(self.linked), self.entry, self.front)
    }
}

/// **The one front half of an entry-call run** (audit-10): resolve the dependency graph → load and
/// link `file` with `tail` appended → keep the entry source → hand back the facts it linked under.
///
/// Three call sites assembled this by hand before it existed — `noeta serve`'s single-worker path,
/// its `--parallel` path, and the hot watcher's re-link in `watch.rs` — each with its own copy of
/// the graph resolution, its own loader argument list and its own error rendering. The three were
/// not identical: the two in `serve.rs` reused the compose probe's graph and the watcher always
/// re-resolved from scratch, which meant a hot server's re-links could link against a *different*
/// graph than the program they were swapping into (and, with `[trust].require_transparency` on,
/// re-resolved over the network on every keystroke-save — where a transient failure silently
/// discarded the edit). [`Front::Given`] is the fix: the watcher is handed the boot's facts.
///
/// The failure is a [`noeta_runner::CompileFailure`] rather than an exit code, because the two
/// consumers need different things from it — `serve` prints it and exits, the watcher renders it
/// into the browser's error overlay and keeps serving.
pub(crate) fn load_entry_with_tail(
    file: &std::path::Path,
    tail: &[noeta_ast::Stmt],
    front: Front,
) -> Result<EntryProgram, noeta_runner::CompileFailure> {
    // The shared front half (drift firewall): deps + edition resolve exactly as `noeta run`'s
    // pipeline resolves them; the verbs stage tier activation themselves.
    let front = match front {
        Front::Given(facts) => facts,
        Front::Resolve(resolved) => std::sync::Arc::new(noeta_runner::compile::resolve_front_with(
            file,
            &[],
            &None,
            resolved.map(|g| noeta_runner::compile::ResolvedFront {
                packages: g.packages,
                package_uses: g.package_uses,
            }),
        )?),
    };
    // The dep-aware loader carries the resolved `@name` tables through on `Linked::provenance`, so
    // there is nothing left to patch in afterwards — the field it used to be patched into is gone.
    let linked = noeta_runner::compile::load_linked_appending(file, &front, tail)?;
    let entry = linked.entry.clone();
    Ok(EntryProgram {
        linked,
        entry,
        front,
    })
}

/// Rewrap a linker result as the runner's [`Loaded`], so type-checking goes through
/// [`Loaded::check`] — the editions/provenance threading choke point — instead of a hand-paired
/// `check_all_with_editions(&linked.program, linked.provenance.editions.clone())`.
pub(crate) fn loaded(linked: noeta_loader::Linked) -> Loaded {
    Loaded {
        program: linked.program,
        sources: linked.sources,
        provenance: linked.provenance,
        // A raw link performs no tier activation, so there is nothing advisory to carry yet — the
        // caller's own check supplies whatever warnings there are.
        warnings: Vec::new(),
    }
}

/// Type-check a program **synthesized from** an already-linked one (a per-test case, a bench
/// measurement loop, a tier-dispatch program, a hot-swap candidate) under the parent workspace's
/// [`noeta_check::CheckOptions`] — its per-source editions *and* its package provenance. SourceIds
/// survive the synthesis (the new nodes carry existing or synthetic spans), so both maps stay valid.
/// The crate's single hand-written whole-`CheckOptions` call site — whole linked programs go through
/// [`Loaded::check`].
///
/// It takes the options **as one value** rather than a map per concern: the two used to be paired by
/// hand at every one of these call sites, which is precisely how a second per-source map would end
/// up threaded through half of them.
pub(crate) fn check_under(
    program: &noeta_ast::Program,
    opts: &noeta_check::CheckOptions,
) -> noeta_check::Checked {
    noeta_check::check_all_with(program, opts.clone())
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
) -> Result<noeta_check::Activated, u8> {
    match activated.registry.resolve_provider(tier, providers) {
        Ok(noeta_check::ResolvedProvider::Extension) => Ok(activated),
        Ok(noeta_check::ResolvedProvider::Declared(d)) => {
            let decl = d.clone();
            Err(run_declared_tier(tier, linked, activated, decl))
        }
        Err(err) => {
            eprintln!("noeta: {err}");
            Err(2)
        }
    }
}

/// A tier verb's program, ready to run natively: the activated program (with its collected tier
/// fns) plus the workspace editions its per-case checks run under. The prologue already gated on
/// activation and whole-program type-check diagnostics (rendered against the workspace sources).
pub(crate) struct TierRun {
    pub(crate) activated: noeta_check::Activated,
    /// The top-level statements that **do not return** (`noeta_check::Checked::diverging_stmts`),
    /// harvested from the whole-program check above. The shared-setup filter reads it to keep
    /// `conn.migrate(…)` while dropping `server.serve(…)` — the behavioural question that replaced
    /// a filter written in terms of statement syntax. It must come from the check of *this* program:
    /// an empty set would silently assert that nothing diverges.
    pub(crate) diverging: std::collections::HashSet<noeta_span::Span>,
    /// The workspace's sources, kept so a runner can render a span against the file it came from —
    /// the dropped-setup warning (`E0071`) points at a real line in a real file.
    pub(crate) sources: noeta_span::SourceMap,
    /// The workspace's check configuration — per-source editions and package provenance — so a
    /// per-case re-check ([`check_under`]) judges the same program the whole-program check did.
    pub(crate) opts: noeta_check::CheckOptions,
}

/// The outcome of [`tier_prologue`]: either the invocation was fully handled (delegated to a
/// composed toolchain, gated off by `--target`, dispatched to a declared provider, or failed —
/// return the exit code), or the program is loaded, activated, and checked clean under the
/// native runner.
pub(crate) enum Prologue {
    /// The invocation was fully handled; carries the native-runner-seam `u8` exit code (a verb
    /// wraps it back into a `std::process::ExitCode` at the clap boundary).
    Ran(u8),
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
        // Composition needed but failed (`maybe_delegate` yields a fixed exit-1 `ExitCode`); the
        // tier subsystem speaks `u8`, so surface it as code 1.
        Err(_) => return Prologue::Ran(1),
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
            return Prologue::Ran(2);
        }
    };
    // Activate the tier: inline its `@<tier>` blocks as ordinary top-level declarations and
    // collect the tier fns. An unknown-tier block is an E0036 (a typo must not silently vanish).
    // Each `@name` resolves per the package that wrote it (per-package naming arc).
    let activated = noeta_check::activate_tiers(&linked.program, &[tier], &linked.provenance);
    let mut activated = match provider_escape(tier, &linked, activated, &providers) {
        Ok(activated) => activated,
        Err(code) => return Prologue::Ran(code),
    };
    // Report what activation found, then gate on **errors** only: a warning is advisory, and a
    // `noeta test` that refuses to run because one line lints is a broken test runner.
    emit_diagnostics_mapped(&linked.sources, activated.diagnostics.iter());
    if noeta_diagnostics::has_errors(&activated.diagnostics) {
        return Prologue::Ran(1);
    }

    // Type-check the activated program once — through the runner's `Loaded`, so the per-source
    // editions ride structurally — so a broken tier fn is a compile error reported a single time
    // here rather than redundantly inside every per-case run.
    let noeta_loader::Linked {
        sources,
        provenance,
        ..
    } = linked;
    let checking = Loaded {
        program: activated.program,
        sources,
        provenance,
        // Activation's own diagnostics were reported above; nothing left to carry.
        warnings: Vec::new(),
    };
    let checked = checking.check();
    // Reported once, here — the per-case programs the verb synthesizes below re-check the *same*
    // source, so they deliberately stay silent rather than repeating every warning per test case.
    emit_diagnostics_mapped(&checking.sources, checked.diagnostics.iter());
    if noeta_diagnostics::has_errors(&checked.diagnostics) {
        return Prologue::Ran(1);
    }
    let diverging = checked.diverging_stmts;
    let Loaded {
        program,
        provenance,
        sources,
        // Empty by construction here (this `Loaded` was built above with none), and activation's own
        // diagnostics were already reported.
        warnings: _,
    } = checking;
    activated.program = program;
    Prologue::Ready(Box::new(TierRun {
        activated,
        diverging,
        sources,
        opts: noeta_check::CheckOptions::for_workspace(provenance),
    }))
}
