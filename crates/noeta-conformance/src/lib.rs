//! The conformance harness: the executable specification.
//!
//! A conformance case is a `.noe` file with an expectation header:
//!
//! ```text
//! // expect: stdout "Order #1 awaiting payment"
//! // expect: exit 0
//! ```
//!
//! and negative cases:
//!
//! ```text
//! // expect: error E0003 at 12:5
//! // expect: exit 1
//! ```
//!
//! This corpus *is* the language spec in executable form — every feature lands with
//! cases here. The same runner powers the language's own suite and (later) user-facing
//! `lang test`. Output is available as human text or machine-readable JSON, and runs
//! can be narrowed by file or by pipeline stage so an agent's loop stays fast.

use std::path::{Path, PathBuf};

use noeta_diagnostics::Diagnostic;
use noeta_span::{Source, SourceId, SourceMap};

#[cfg(feature = "jit")]
mod aot;
mod bundle;
mod determinism;
mod differential;
mod expectation;
mod ir_corpus;
#[cfg(feature = "jit")]
mod jit_differential;
mod leaks;
// Public so the trace-parity integration test drives the oracle's traced entry through the same
// IR pipeline the differential uses.
pub mod reference;
mod report;
mod wasm;

#[cfg(feature = "jit")]
pub use aot::{AotDiffFailure, AotDiffReport, run_aot_differential};
pub use bundle::{BundleFailure, BundleReport, run_bundle_roundtrip};
pub use determinism::{DeterminismReport, digest_corpus};
pub use differential::{DiffReport, Mismatch, run_differential};
pub use expectation::{ErrorExpectation, Expectations};
pub use ir_corpus::{IrCorpusReport, run_ir_corpus};
#[cfg(feature = "jit")]
pub use jit_differential::{
    Arm as JitDiffArm, JitDiffReport, run_jit_differential, run_jit_differential_with,
};
pub use leaks::{Leak, LeakReport, run_leak_check};
pub use report::{CaseResult, CaseStatus, NotRun, Report};
pub use wasm::{WasmDiffFailure, WasmDiffReport, run_wasm_differential};

/// Which pipeline stages to run a case through. Narrowing the stage makes an agent's inner loop
/// fast, and narrows what a pass *means*: only [`Stage::Eval`] runs the program, so the front-end
/// stages check the `// expect: error` lines and nothing else. Every report says which stage it
/// ran, because "1265 passed" reads identically either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Load the case — lex, parse and link — and check its diagnostics. Nothing runs.
    Lexer,
    /// The same front end as [`Stage::Lexer`]: the loader lexes and parses in one step, so the two
    /// names select the same work and the same assertions.
    Parser,
    /// The whole pipeline: load, check, and execute the program on every selected [`Engine`].
    #[default]
    Eval,
}

impl Stage {
    pub fn parse(name: &str) -> Option<Stage> {
        match name {
            "lexer" => Some(Stage::Lexer),
            "parser" => Some(Stage::Parser),
            "eval" => Some(Stage::Eval),
            _ => None,
        }
    }

    /// The stage's name on the command line and in the report.
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Lexer => "lexer",
            Stage::Parser => "parser",
            Stage::Eval => "eval",
        }
    }

    /// Whether this stage executes the program (and can therefore check stdout, stderr and the
    /// exit code).
    pub fn runs_the_program(self) -> bool {
        self == Stage::Eval
    }
}

impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which executor ran a case's program when its `// expect:` header was checked.
///
/// The language is implemented twice, so "the expectations hold" is a claim about an *engine*: a
/// header validated against the reference interpreter alone says nothing about the bytecode VM,
/// and a regression living in one backend passes a run that never drove it. So an expectation run
/// drives **both** by default, every failure names the engine that produced it, and every report
/// names the engines it validated against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    /// The Core-IR interpreter (`noeta-eval`) — the reference semantics.
    Reference,
    /// The bytecode VM (`noeta-vm`) executing the module `noeta-compiler` produced — the pair
    /// `noeta run` uses.
    Vm,
}

impl Engine {
    /// The engine's name on the command line, in the report, and in the JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Engine::Reference => "reference",
            Engine::Vm => "vm",
        }
    }

    /// The engine in a sentence, for the report's summary line.
    pub fn description(self) -> &'static str {
        match self {
            Engine::Reference => "the reference interpreter",
            Engine::Vm => "the bytecode VM",
        }
    }
}

impl std::fmt::Display for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which engines an expectation run validates against — the `--engine` selection.
///
/// [`Engines::Both`] is the default because the alternative is a check that reads as a verdict on
/// the language and is a verdict on one half of it. The single-engine selections exist to *narrow*
/// a known failure ("does this reproduce on the VM alone?"), never as the cheap default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Engines {
    /// The reference interpreter and the bytecode VM, each checked against the same header.
    #[default]
    Both,
    /// The reference interpreter only.
    Reference,
    /// The bytecode VM only.
    Vm,
}

impl Engines {
    /// The accepted spellings, for an error message.
    pub const ACCEPTED: &'static str = "reference, vm, or both";

    pub fn parse(name: &str) -> Option<Engines> {
        match name {
            "reference" => Some(Engines::Reference),
            "vm" => Some(Engines::Vm),
            "both" => Some(Engines::Both),
            _ => None,
        }
    }

    /// The engines this selection runs, in the order they run.
    pub fn engines(self) -> &'static [Engine] {
        match self {
            Engines::Both => &[Engine::Reference, Engine::Vm],
            Engines::Reference => &[Engine::Reference],
            Engines::Vm => &[Engine::Vm],
        }
    }
}

/// What actually happened when a case ran: its captured stdout and stderr, exit code, and the
/// `(code, line, col)` of every diagnostic, in order.
struct Outcome {
    stdout: String,
    stderr: String,
    exit_code: i32,
    errors: Vec<ErrorExpectation>,
}

/// One engine's answer for a case — or the reason that engine could not produce one, which is
/// always a failure (the harness never counts "this backend declined to run it" as a pass).
struct EngineOutcome {
    /// The engine that produced it. `None` when no program ran at all — a front-end-only stage, a
    /// load failure, or a checker rejection — where the answer is the front end's and would be
    /// identical on every engine.
    engine: Option<Engine>,
    outcome: Result<Outcome, String>,
}

/// Whether any diagnostic is an **error** — the gate on running a program and on a failing exit.
///
/// A [`Severity::Warning`](noeta_diagnostics::Severity::Warning) is by definition compatible with
/// running: it says the program is well-formed and something in it is worth a second look. It is
/// still reported (a case asserts it with the same `// expect: error <CODE> at …` header — the
/// header names a diagnostic, whatever its severity), it simply does not stop the program.
pub(crate) fn has_error(diagnostics: &[Diagnostic]) -> bool {
    noeta_diagnostics::has_errors(diagnostics)
}

/// Map each diagnostic to its `(code, line, col)` expectation, resolved against `source`.
fn errors_of(source: &Source, diagnostics: &[Diagnostic]) -> Vec<ErrorExpectation> {
    diagnostics
        .iter()
        .map(|d| expectation(d, source.line_col(d.span.start)))
        .collect()
}

/// Map each diagnostic to its `(code, line, col)` expectation, resolving each span against the
/// source it belongs to (its `SourceId`) via the [`SourceMap`]. Used for the linked path, where a
/// diagnostic on a merged-in sibling declaration must render against that sibling, not the entry.
fn errors_of_mapped(sources: &SourceMap, diagnostics: &[Diagnostic]) -> Vec<ErrorExpectation> {
    diagnostics
        .iter()
        .map(|d| expectation(d, sources.line_col(d.span)))
        .collect()
}

fn expectation(d: &Diagnostic, at: noeta_span::LineCol) -> ErrorExpectation {
    ErrorExpectation {
        code: d.code.to_string(),
        line: at.line,
        col: at.col,
    }
}

/// Seed the process-default extension registry with the std units. The front-end
/// (loader/checker/compiler/ir/db) consumes the registry as data and no longer links the std
/// units (audit-6 F2), so every harness entry — and any test helper that calls
/// `noeta_check`/`noeta_compiler`/`noeta_loader` directly — must seed first. Idempotent.
pub fn ensure_std_registry() {
    noeta_stdlib::registry::default_seeded();
}

/// Run a single named case (already-loaded source text) and compare it to its header, on **every**
/// engine — see [`run_case_with`] to narrow that.
pub fn run_case(name: &str, text: &str, stage: Stage) -> CaseResult {
    run_case_with(name, text, stage, Engines::default())
}

/// As [`run_case`], against a chosen set of [`Engines`].
pub fn run_case_with(name: &str, text: &str, stage: Stage, engines: Engines) -> CaseResult {
    ensure_std_registry();
    let expectations = match Expectations::parse(text) {
        Ok(expectations) => expectations,
        Err(message) => return CaseResult::malformed(name, message),
    };
    let outcomes = run_linked_source(name, text, stage, engines.engines());
    compare(name, &expectations, &outcomes, stage)
}

/// Compare one engine's outcome against the header, returning every way it differs.
fn differences(expected: &Expectations, actual: &Outcome, stage: Stage) -> Vec<String> {
    let mut failures = Vec::new();

    // stdout and exit code only become meaningful once the program is evaluated, so
    // partial-stage runs (`--stage lexer`/`parser`) check error expectations only.
    if stage == Stage::Eval {
        if let Some(expected_exit) = expected.exit
            && expected_exit != actual.exit_code
        {
            failures.push(format!(
                "exit: expected {expected_exit}, got {}",
                actual.exit_code
            ));
        }

        if let Some(expected_lines) = &expected.stdout_lines {
            let actual_lines: Vec<&str> = actual.stdout.lines().collect();
            if actual_lines
                != expected_lines
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
            {
                failures.push(format!(
                    "stdout: expected {:?}, got {:?}",
                    expected_lines, actual_lines
                ));
            }
        }

        if let Some(expected_lines) = &expected.stderr_lines {
            let actual_lines: Vec<&str> = actual.stderr.lines().collect();
            if actual_lines
                != expected_lines
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
            {
                failures.push(format!(
                    "stderr: expected {:?}, got {:?}",
                    expected_lines, actual_lines
                ));
            }
        }
    }

    for expected_error in &expected.errors {
        if !actual.errors.contains(expected_error) {
            failures.push(format!(
                "error: expected {} at {}:{}, but it was not produced (got {:?})",
                expected_error.code, expected_error.line, expected_error.col, actual.errors
            ));
        }
    }

    failures
}

/// Compare every engine's outcome against the header and fold them into one [`CaseResult`].
///
/// Each failure is **attributed** to the engine that produced it, because the interesting shape is
/// one engine failing while the other passes: that is a backend divergence wearing a wrong-output
/// costume, and an unattributed line would leave a reader guessing which half is wrong.
fn compare(
    name: &str,
    expected: &Expectations,
    outcomes: &[EngineOutcome],
    stage: Stage,
) -> CaseResult {
    let mut failures = Vec::new();
    let mut executed_on = Vec::new();
    for produced in outcomes {
        // A failure from a named engine is labelled; the front end's own answer (no engine ran)
        // stands unlabelled, because there is nothing to distinguish it from.
        let attribute = |failure: String| match produced.engine {
            Some(engine) => format!("[{engine}] {failure}"),
            None => failure,
        };
        match &produced.outcome {
            Err(reason) => failures.push(attribute(reason.clone())),
            Ok(outcome) => {
                if let Some(engine) = produced.engine {
                    executed_on.push(engine);
                }
                failures.extend(
                    differences(expected, outcome, stage)
                        .into_iter()
                        .map(attribute),
                );
            }
        }
    }

    let mut result = if failures.is_empty() {
        CaseResult::pass(name)
    } else {
        CaseResult::fail(name, failures)
    };
    result.executed_on = executed_on;
    result
}

/// Run a multi-file case rooted at `entry` (the `main.noe` of a module fixture) on **every** engine:
/// sibling modules are loaded and linked and the merged program is checked and run like any other
/// case. The expectation header lives in the entry file.
pub fn run_case_path(entry: &Path, display: &str, stage: Stage) -> CaseResult {
    run_case_path_with(entry, display, stage, Engines::default())
}

/// As [`run_case_path`], against a chosen set of [`Engines`].
pub fn run_case_path_with(
    entry: &Path,
    display: &str,
    stage: Stage,
    engines: Engines,
) -> CaseResult {
    let text = match std::fs::read_to_string(entry) {
        Ok(text) => text,
        Err(err) => return CaseResult::malformed(display, format!("could not read: {err}")),
    };
    let expectations = match Expectations::parse(&text) {
        Ok(expectations) => expectations,
        Err(message) => return CaseResult::malformed(display, message),
    };
    let outcomes = run_linked(entry, stage, engines.engines());
    compare(display, &expectations, &outcomes, stage)
}

/// The **root package** a multi-file case declares, if it declares one: a case directory holding a
/// [`noeta_loader::MANIFEST_NAME`] is a package, and its `.noe` files derive their module paths under
/// the manifest's `[package] name` root segment (`local/App` → `App.…`).
///
/// This is what lets a case exercise **derivation** at all. Without a manifest there is no package,
/// nothing derives, and each file's own `namespace` declaration stands — which is exactly the
/// behavior a bare `noeta run` on a lone script keeps, and what every package-less case relies on.
///
/// Looked for in the case directory **only**, never up the tree: a corpus fixture must not change
/// meaning because of a `noeta.toml` sitting somewhere above the checkout.
pub(crate) fn case_root(entry: &Path) -> Option<noeta_loader::PackageRoot> {
    let dir = entry.parent()?;
    let text = std::fs::read_to_string(dir.join(noeta_loader::MANIFEST_NAME)).ok()?;
    let name = noeta_pm::manifest::Manifest::parse(&text)
        .ok()?
        .package()?
        .root()
        .to_string();
    Some(noeta_loader::PackageRoot::new(dir, vec![name]))
}

/// Read a discovered case's sources as a workspace, under whatever package root the case declares
/// ([`case_root`]) — so every runner that drives cases through the salsa module graph derives module
/// paths the same way the batch loader does. One helper, because six runners disagreeing about which
/// package a case belongs to would be silent.
pub(crate) fn read_case_workspace(entry: &Path) -> std::io::Result<noeta_loader::RawWorkspace> {
    noeta_loader::read_workspace(entry, case_root(entry).as_ref())
}

/// The **dependency packages** of a multi-file case: every *subdirectory* of the case directory is
/// one package, keyed (and rooted) by its directory name, holding that directory's `.noe` files as
/// its modules.
///
/// A conformance case is otherwise one package — entry plus siblings — which cannot express any rule
/// whose boundary is the package, the orphan rule (E0070) first among them. A subdirectory is the
/// smallest thing that can: the loader's `DepPackage` needs a root segment, a derived prefix, and
/// modules, and a directory name supplies the first two. A plain name gives `prefix == [name]` and
/// `root == name`, so nothing is re-rooted and a package's modules are addressed by exactly the path
/// their file names derive; every package is visible to every other, which models a graph where the
/// entry depends on all of them and they depend on each other (the checker sees one merged pool
/// either way).
///
/// **A dotted directory name is a scope-array member** — `para.db/` is the package `para/db` reached
/// through `para = [{ package = "para/db" }, … ]`, so its modules derive under the two-segment prefix
/// `para.db` while the package's own root segment stays `db` (what it derives under standalone, and
/// therefore what its intra-package `use`s lead with). That shape is the *only* one in which a
/// package's derived prefix is deeper than its own root segment, and it is where an intra-package
/// import has to be re-rooted to keep resolving — inexpressible while a directory name gave one
/// segment for both.
///
/// **Empty for every existing case**, and callers must keep the deps-free path when it is: linking
/// *with* a resolved dependency graph is deliberately stricter about foreign import roots than
/// sibling-only linking, so routing a package-less case through it would change its meaning.
/// The root segment a dependency fixture's own `noeta.toml` declares, if it has one.
///
/// `None` covers both "no manifest" (the common fixture shape, where the directory name is the
/// whole story) and "a manifest that does not parse" — a broken one is the case's own business to
/// fail on, not something to guess a root from.
fn dep_declared_root(package_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(package_dir.join(noeta_loader::MANIFEST_NAME)).ok()?;
    let manifest = noeta_pm::manifest::Manifest::parse(&text).ok()?;
    Some(manifest.package()?.root().to_string())
}

fn dep_packages(dir: &Path) -> Vec<noeta_loader::DepPackage> {
    // A case that is *itself* a package ([`case_root`]) owns its subdirectories — they are its own
    // module tree, walked recursively under its prefix. Treating them as separate packages too would
    // link every file twice and collide on every derived path.
    if dir.join(noeta_loader::MANIFEST_NAME).is_file() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .filter_map(|package_dir| {
            let name = package_dir.file_name()?.to_string_lossy().into_owned();
            // A case's package subdirectory IS a package root: walked recursively, every module
            // deriving its path under the prefix the directory name spells.
            let prefix: Vec<String> = name.split('.').map(str::to_string).collect();
            // The package's own root segment — what its intra-package `use`s lead with, and what
            // `reroot_path` rewrites to the prefix above. Its **own manifest** decides that when it
            // has one, exactly as a real dependency's does; a fixture without a manifest keeps the
            // directory-derived answer (the last prefix segment, which a single-segment name gives
            // too). Without reading it, a fixture could declare `root` and the harness would ignore
            // it — the case would test the directory name and quietly claim to test the manifest.
            let root = dep_declared_root(&package_dir)
                .unwrap_or_else(|| prefix.last().cloned().unwrap_or_default());
            let modules = noeta_loader::read_package_modules(&noeta_loader::PackageRoot::new(
                &package_dir,
                prefix.clone(),
            ));
            (!modules.is_empty()).then(|| noeta_loader::DepPackage {
                prefix,
                root,
                modules,
                dep_renames: std::collections::BTreeMap::new(),
                native: false,
                edition: noeta_lexer::Edition::DEFAULT,
                directives: Default::default(),
            })
        })
        .collect()
}

/// The same package layout as [`dep_packages`], expressed as the salsa module graph's
/// [`noeta_db::DepSources`] — for the differential, which drives multi-file cases through the query
/// graph rather than the batch loader. `next_id` continues the `SourceId` numbering past the entry
/// and its siblings, matching the loader's own assignment (entry = 0, siblings `1..=S`, dependency
/// modules after), so a span resolves to the same file on both paths.
pub(crate) fn dep_sources(entry: &Path, next_id: u32) -> Vec<noeta_db::DepSources> {
    let Some(dir) = entry.parent() else {
        return Vec::new();
    };
    let mut id = next_id;
    dep_packages(dir)
        .into_iter()
        .map(|pkg| {
            let modules = pkg
                .modules
                .iter()
                .map(|m| {
                    let source = Source::new(SourceId(id), m.name.as_str(), m.text.as_str());
                    id += 1;
                    source
                })
                .collect();
            let paths = pkg.modules.iter().map(|m| m.path.clone()).collect();
            noeta_db::DepSources {
                root: pkg.root,
                prefix: pkg.prefix,
                renames: Vec::new(),
                modules,
                paths,
                edition: noeta_lexer::Edition::DEFAULT,
            }
        })
        .collect()
}

/// The salsa [`Workspace`](noeta_db::Workspace) a multi-file case is linked, checked and run in —
/// **with** the dependency packages the case declares as subdirectories ([`dep_sources`]), and the
/// deps-free workspace when it declares none.
///
/// One helper because the alternative was measured: six oracles each built this themselves, three
/// carrying the dependency graph and three dropping it, so one case was a *different program*
/// depending on which oracle was looking. Concretely, dropping the deps left three package fixtures
/// checker-rejected (never leak-checked at all) and made five negative fixtures — the orphan rule,
/// a derived-path collision, an unknown module in a block `use` — link and run something that is
/// not what the case is about, while reporting it as measured. Both readings are green.
pub(crate) fn case_workspace(
    db: &noeta_db::LangDatabase,
    raw: &noeta_loader::RawWorkspace,
    entry: &Path,
) -> noeta_db::Workspace {
    // `next_id` continues the numbering past the entry and its siblings, matching the loader's own
    // assignment, so a diagnostic's span resolves to the same file on both paths.
    let deps = dep_sources(entry, (raw.modules.len() + 1) as u32);
    if deps.is_empty() {
        noeta_db::workspace(
            db,
            &raw.entry,
            &raw.modules,
            noeta_lexer::Edition::DEFAULT,
            &raw.paths,
        )
    } else {
        // No `@name` tables: the corpus's dependency graph is synthesized from the case's
        // subdirectories, not from a `noeta.toml`, so no package binds a `[directives]` local name
        // — an empty `PackageUses` is behavior-identical.
        noeta_db::workspace_with_deps(
            db,
            &raw.entry,
            &raw.modules,
            &deps,
            &noeta_span::PackageUses::new(),
            noeta_lexer::Edition::DEFAULT,
            &raw.paths,
        )
    }
}

/// Load + link `entry` and run the merged program to an [`Outcome`]. Lex/parse errors render
/// against the source they came from; check/runtime diagnostics against the entry source.
/// Run a case from source text alone, linked **with no siblings**.
///
/// This is the single-file path, and it goes through [`noeta_loader::link`] for one reason: the
/// linker is where a program's names are resolved against the extension registry. Its rewrite
/// tables — `add_native_type_aliases` and friends — are what make `use std.http` bind `http.Frame`,
/// so a harness that lexes, parses and checks *without* linking is asking a different question than
/// `noeta run` asks. Measured before this path existed: `variants_of::<http.Framing>()` answered
/// correctly under `noeta run` and `[]` in the harness, and a conformance case could pin the wrong
/// answer and pass.
///
/// **The sibling pool is deliberately empty.** A single-file case's directory holds dozens of
/// unrelated cases, several of them deliberately malformed, and the loader fails every entry in a
/// directory when one module there is broken — so reading siblings would make one negative case
/// poison its neighbours. Linking the entry alone runs the rewrite tables and merges nothing.
fn run_linked_source(
    name: &str,
    text: &str,
    stage: Stage,
    engines: &[Engine],
) -> Vec<EngineOutcome> {
    let linked = match noeta_loader::link(
        name,
        text,
        noeta_lexer::Edition::DEFAULT,
        &[],
        noeta_loader::ModulePath::Declared,
    ) {
        Ok(linked) => linked,
        Err(load_diagnostics) => return vec![front_end(load_failure(&load_diagnostics))],
    };
    outcomes_of_linked(linked, stage, engines)
}

/// An outcome no engine produced: the front end's own answer, identical whichever backend would
/// have run. A load failure, a checker rejection, and a front-end-only stage all end here.
fn front_end(outcome: Outcome) -> EngineOutcome {
    EngineOutcome {
        engine: None,
        outcome: Ok(outcome),
    }
}

/// The outcome of a load that never produced a program: its diagnostics, rendered against whichever
/// source each one belongs to.
fn load_failure(load_diagnostics: &[noeta_loader::LoadDiagnostic]) -> Outcome {
    Outcome {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 1,
        errors: load_diagnostics
            .iter()
            .flat_map(|ld| errors_of(&ld.source, std::slice::from_ref(&ld.diagnostic)))
            .collect(),
    }
}

fn run_linked(entry: &Path, stage: Stage, engines: &[Engine]) -> Vec<EngineOutcome> {
    // A case with package subdirectories links through the dependency-aware path so its sources
    // carry real package provenance; one without keeps the deps-free path byte-for-byte.
    let deps = entry.parent().map(dep_packages).unwrap_or_default();
    // A case declaring a manifest is a package: its files derive their module paths ([`case_root`]).
    // One without keeps `None` — nothing derives and each file's `namespace` declaration stands.
    let root = case_root(entry);
    let load = if deps.is_empty() {
        noeta_loader::load(entry, noeta_lexer::Edition::DEFAULT, root.as_ref())
    } else {
        // Conformance cases carry no manifest `[directives]`, so an empty `PackageUses`
        // suffices — a dependency's own `@tier(…, text)` still captures via the global scan.
        noeta_loader::load_with_deps(
            entry,
            noeta_lexer::Edition::DEFAULT,
            &deps,
            &noeta_span::PackageUses::new(),
            root.as_ref(),
        )
    };
    let linked = match load {
        Ok(Ok(linked)) => linked,
        Ok(Err(load_diagnostics)) => return vec![front_end(load_failure(&load_diagnostics))],
        Err(err) => {
            return vec![front_end(Outcome {
                stdout: format!("could not read: {err}"),
                stderr: String::new(),
                exit_code: 1,
                errors: Vec::new(),
            })];
        }
    };
    outcomes_of_linked(linked, stage, engines)
}

/// Check and run an already-linked program, once per requested [`Engine`]. Shared by both case
/// paths — the multi-file one that loads a directory and the single-file one that links an entry
/// alone — so the two cannot drift into disagreeing about what a case means.
///
/// The front end runs **once** and its result is shared: loading, linking and checking are the same
/// work whichever backend executes afterwards, and a case the checker rejects has no program to run
/// at all, so it yields one engine-independent outcome rather than one identical copy per engine.
fn outcomes_of_linked(
    linked: noeta_loader::Linked,
    stage: Stage,
    engines: &[Engine],
) -> Vec<EngineOutcome> {
    // The loader already lexed + parsed cleanly; the lexer/parser stages have nothing more to do.
    if stage != Stage::Eval {
        return vec![front_end(Outcome {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            errors: Vec::new(),
        })];
    }

    // Check/runtime diagnostics may land on a declaration merged in from a sibling module, so they
    // resolve through the source map (by each span's `SourceId`) rather than always against the
    // entry — that is what gives a cross-module error its real file/line/column.
    // Checked under the program's real per-source provenance, so a package-boundary rule (E0070)
    // sees the same graph a `noeta run` of the same layout does.
    let checked = noeta_check::check_all_with(
        &linked.program,
        // The link's own provenance, whole. Empty `uses` for every corpus case today (no manifest,
        // so nothing binds a `@name`) — but carried rather than defaulted, so a fixture that grows a
        // manifest binding is checked the way `noeta run` checks it.
        noeta_check::CheckOptions::for_workspace(linked.provenance.clone()),
    );
    if has_error(&checked.diagnostics) {
        return vec![front_end(Outcome {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 1,
            errors: errors_of_mapped(&linked.sources, &checked.diagnostics),
        })];
    }
    engines
        .iter()
        .map(|&engine| EngineOutcome {
            engine: Some(engine),
            outcome: run_on(engine, &linked, &checked),
        })
        .collect()
}

/// Execute an already-checked program on one engine.
///
/// The two arms are the same pair `noeta run` chooses between, driven the same way: the reference
/// lowers to the drop-annotated Core IR and interprets it; the VM compiles that same program to a
/// bytecode module and runs it on the sandbox host. A module the compiler refuses is an `Err` here
/// rather than a silent skip — a check-clean corpus program that stops compiling is exactly the
/// regression this arm exists to see, and the differential already pins that number at 100%.
fn run_on(
    engine: Engine,
    linked: &noeta_loader::Linked,
    checked: &noeta_check::Checked,
) -> Result<Outcome, String> {
    // A check that produced only warnings runs — and the warnings stay in the reported list, ahead
    // of anything the run itself reports, so a case can assert one and its output in one header.
    let mut diagnostics = checked.diagnostics.clone();
    let result = match engine {
        Engine::Reference => reference::reference_run(&linked.program, checked.sites.clone()),
        Engine::Vm => {
            let module = noeta_compiler::compile_with_sites(
                &linked.program,
                checked.sites.clone(),
                false,
                false,
            )
            .map_err(|unsupported| unsupported.to_string())?;
            noeta_vm::VmBackend::new().run_module(&module)
        }
    };
    diagnostics.extend(result.diagnostics);
    Ok(Outcome {
        errors: errors_of_mapped(&linked.sources, &diagnostics),
        stdout: result.stdout,
        stderr: result.stderr,
        exit_code: result.exit_code,
    })
}

/// Run `f` on a worker thread with a large stack, returning its result.
///
/// Executing a whole program through the recursive Core-IR interpreter (and checking it through the
/// recursive-descent checker) uses *runtime* stack proportional to the program's call depth. A
/// realistic case — e.g. the `@html` LiveView test composes an async server, a websocket session,
/// and reactive-graph propagation — can exceed a small caller stack (libtest gives each `#[test]` a
/// ~2 MiB thread) in an unoptimized debug build, aborting the whole process on overflow. Running a
/// corpus sweep inside this helper makes the interpreter's depth, not whatever stack the harness
/// happens to provide, the binding constraint — mirroring the parser's own deep-nesting worker
/// (`noeta_parser::parse_in`).
///
/// The sweep runs *entirely* within the one worker thread — including any thread-local instrumentation
/// the caller sets up around it (e.g. `noeta_eval::drop_audit`), which would be invisible if the
/// execution ran on a different thread than the `begin`/`end` calls. So callers wrap their whole
/// test body, not the individual runner. A scoped thread lets `f` borrow its inputs directly; only
/// the owned result crosses the join.
pub fn on_deep_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    // This is the *interpreter's* budget, not the parser's: it used to be written as "matches
    // `noeta_parser::DEEP_PARSE_STACK`", which stopped being true when that constant was sized
    // against a measured per-nesting-level cost and grew to 256 MiB. Nothing here needs to track it
    // — a corpus case's depth is its call depth, not its syntactic nesting — so the number stands on
    // its own: comfortably above any single case's needs.
    const DEEP_STACK: usize = 64 * 1024 * 1024;
    std::thread::scope(|scope| {
        match std::thread::Builder::new()
            .stack_size(DEEP_STACK)
            .spawn_scoped(scope, f)
            .expect("spawn conformance worker")
            .join()
        {
            Ok(value) => value,
            // Re-raise the worker's panic on the caller's thread with its original payload, so an
            // assertion failure inside a wrapped test surfaces its real message (not a generic one).
            Err(payload) => std::panic::resume_unwind(payload),
        }
    })
}

/// Whether `only` selects `entry` — the `--file` narrowing rule, in one place because every oracle
/// applies it and a filter that means different things per oracle is the "one rule spelled twice"
/// defect this repo already knows.
pub(crate) fn selects(entry: &Path, only: &Path) -> bool {
    entry == only || entry.ends_with(only)
}

/// How many corpus cases a `--file` argument names. Zero means the argument matches nothing — a
/// typo — which every oracle must refuse rather than report as an empty pass. Distinct from "the
/// run executed zero programs", which is legitimate for a narrowed differential whose one case the
/// checker rejects; this asks only whether the *file* exists in the corpus.
pub fn cases_selected(root: &Path, only: &Path) -> usize {
    let mut cases = Vec::new();
    collect_cases(root, &mut cases);
    cases.iter().filter(|c| selects(&c.entry, only)).count()
}

/// Discover and run every case under `root`, optionally narrowed to a single entry file, checking
/// each header against **every** engine. Returns a [`Report`] that can be rendered as text or JSON.
pub fn run_corpus(root: &Path, only: Option<&Path>, stage: Stage) -> Report {
    run_corpus_with(root, only, stage, Engines::default())
}

/// As [`run_corpus`], against a chosen set of [`Engines`].
pub fn run_corpus_with(root: &Path, only: Option<&Path>, stage: Stage, engines: Engines) -> Report {
    let mut cases = Vec::new();
    collect_cases(root, &mut cases);
    cases.sort_by(|a, b| a.entry.cmp(&b.entry));

    let mut report = Report {
        cases: Vec::new(),
        stage,
        // What the run *can* validate against: a front-end-only stage executes no program, so
        // claiming an engine there would be the same overstatement this field exists to prevent.
        engines: if stage.runs_the_program() {
            engines.engines().to_vec()
        } else {
            Vec::new()
        },
    };
    for case in cases {
        if let Some(only) = only
            && !selects(&case.entry, only)
        {
            continue;
        }
        let display = case
            .entry
            .strip_prefix(root)
            .unwrap_or(&case.entry)
            .to_string_lossy()
            .into_owned();
        if case.multi {
            report.push(run_case_path_with(&case.entry, &display, stage, engines));
        } else {
            match std::fs::read_to_string(&case.entry) {
                Ok(text) => report.push(run_case_with(&display, &text, stage, engines)),
                Err(err) => report.push(CaseResult::malformed(
                    &display,
                    format!("could not read: {err}"),
                )),
            }
        }
    }
    report
}

/// One discovered case: its entry `.noe` file and whether it is a multi-file module fixture.
pub(crate) struct Case {
    pub entry: PathBuf,
    pub multi: bool,
}

/// Discover cases under `dir`. A directory that directly contains a `main.noe` is a single
/// **multi-file** case — its other `.noe` files are that program's modules, not standalone
/// cases — so discovery does not descend into it. Every other `.noe` file is its own
/// single-file case.
pub(crate) fn collect_cases(dir: &Path, out: &mut Vec<Case>) {
    let main = dir.join("main.noe");
    if main.is_file() {
        out.push(Case {
            entry: main,
            multi: true,
        });
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_cases(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "noe") {
            out.push(Case {
                entry: path,
                multi: false,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pipeline the single-file path used to run: lex, parse, check, evaluate — no linking.
    ///
    /// It survives **only here**, as the control for
    /// [`a_module_qualified_native_type_resolves_with_or_without_linking`]. A claim that the two
    /// paths agree is worth nothing without the measurement that they do, and this is that
    /// measurement: the same source, both ways.
    fn run_unlinked_control(name: &str, text: &str) -> String {
        ensure_std_registry();
        let source = Source::new(SourceId::FIRST, name, text);
        let lexed = noeta_lexer::lex_in(
            &source,
            noeta_lexer::Edition::DEFAULT,
            &noeta_lexer::TextTiers::default(),
        );
        let parsed = noeta_parser::parse_in(
            &source,
            &lexed.tokens,
            noeta_lexer::Edition::DEFAULT,
            &noeta_lexer::TextTiers::default(),
        );
        let checked = noeta_check::check_all(&parsed.program);
        reference::reference_run(&parsed.program, checked.sites).stdout
    }

    /// The captured stdout of a single-engine run, which is what these two tests compare.
    fn stdout_of(outcomes: Vec<EngineOutcome>) -> String {
        let [produced] = <[EngineOutcome; 1]>::try_from(outcomes)
            .unwrap_or_else(|got| panic!("one engine ran, got {}", got.len()));
        produced
            .outcome
            .unwrap_or_else(|reason| panic!("the engine produced no outcome: {reason}"))
            .stdout
    }

    /// **A module-qualified native name resolves the same with or without linking.**
    /// `use std.http` then `http.Framing` names a native enum through the module it lives in, and
    /// lowering binds that spelling from the program's own `use` statements — so the name reaches
    /// the registry's `ExtEnum` whether or not the loader ran.
    ///
    /// Measured both ways because the failure it guards against is silent: `variants_of` answers an
    /// empty list rather than an error for a name it cannot resolve, so a half that stopped
    /// resolving would let a conformance case assert the empty answer and pass while `noeta run` on
    /// the same file printed three. Equality is the assertion, and either number moving alone is
    /// the finding.
    #[test]
    fn a_module_qualified_native_type_resolves_with_or_without_linking() {
        let source = "use std.http\necho variants_of::<http.Framing>().len()\n";

        // The control below is the reference interpreter without the loader, so the linked side is
        // the reference interpreter too: the question is what *linking* changes, not what the
        // backends do (that is the differential's).
        let linked = stdout_of(run_linked_source(
            "linked",
            source,
            Stage::Eval,
            &[Engine::Reference],
        ));
        assert_eq!(
            linked.trim(),
            "3",
            "linked, the module-qualified native enum resolves to its three variants"
        );

        let unlinked = run_unlinked_control("unlinked", source);
        assert_eq!(
            unlinked.trim(),
            linked.trim(),
            "unlinked, the same name resolves to the same enum — lowering binds it from this \
             program's own imports, so the two paths cannot answer differently"
        );
    }

    /// **A native verbatim tier lexes in the single-file salsa front end.**
    ///
    /// `@json { … }` is declared by an extension and by no `.noe` file, so nothing in the text says
    /// its body is verbatim: a front end that lexes without the installed extensions' tier names
    /// tokenizes the body as code and reports a parse error on source the loader accepts. That is
    /// not a cosmetic difference — six oracles drive single-file cases through this path, and a case
    /// that "fails to parse" there is silently excluded from all of them, which is how a corpus
    /// fixture came to claim in its own header that the differential proved its two backends agreed
    /// while no backend had ever run it.
    #[test]
    fn a_native_verbatim_tier_lexes_in_the_single_file_front_end() {
        ensure_std_registry();
        let db = noeta_db::LangDatabase::default();
        let text = "id = \"u-7\"\ndoc = @json { {\"id\": ${id}} }\necho doc\n";
        let source = Source::new(SourceId::FIRST, "tier", text);
        let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);

        let lex = &noeta_db::tokens(&db, src).0.diagnostics;
        let parse = &noeta_db::ast(&db, src).0.diagnostics;
        assert!(
            !noeta_diagnostics::has_errors(lex.iter().chain(parse.iter())),
            "the single-file front end could not read a native tier's verbatim body: {:?}",
            lex.iter().chain(parse.iter()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn passing_case_passes() {
        let case = "// expect: stdout \"hello\"\n// expect: exit 0\necho \"hello\";\n";
        assert_eq!(
            run_case("hello", case, Stage::Eval).status,
            CaseStatus::Pass
        );
    }

    #[test]
    fn wrong_stdout_fails() {
        let case = "// expect: stdout \"goodbye\"\necho \"hello\";\n";
        assert_eq!(run_case("x", case, Stage::Eval).status, CaseStatus::Fail);
    }

    /// **Both engines check the header, and each failure says which one produced it.**
    ///
    /// This is the gate on the harness's central claim. Checking a header against the reference
    /// interpreter alone leaves the bytecode VM — the thing `noeta run` actually executes —
    /// unexercised, so a regression that lives only in the VM passes a run that reads as "the
    /// corpus is green". Drop the VM arm and this case reports one failure instead of two.
    #[test]
    fn a_wrong_expectation_is_reported_by_every_engine_that_ran_it() {
        let case = "// expect: stdout \"goodbye\"\necho \"hello\";\n";
        let result = run_case("x", case, Stage::Eval);
        assert_eq!(result.status, CaseStatus::Fail);
        assert_eq!(
            result.executed_on,
            vec![Engine::Reference, Engine::Vm],
            "the default run executes the program on both engines"
        );
        for engine in [Engine::Reference, Engine::Vm] {
            assert!(
                result
                    .failures
                    .iter()
                    .any(|f| f.starts_with(&format!("[{engine}] stdout:"))),
                "{engine} did not report the wrong stdout: {:?}",
                result.failures
            );
        }
    }

    /// Narrowing to one engine narrows what ran — and the result says so, rather than reading like
    /// a verdict on both.
    #[test]
    fn narrowing_to_one_engine_runs_only_that_engine() {
        let case = "// expect: stdout \"hello\"\n// expect: exit 0\necho \"hello\";\n";
        for (selection, expected) in [
            (Engines::Reference, vec![Engine::Reference]),
            (Engines::Vm, vec![Engine::Vm]),
            (Engines::Both, vec![Engine::Reference, Engine::Vm]),
        ] {
            let result = run_case_with("hello", case, Stage::Eval, selection);
            assert_eq!(result.status, CaseStatus::Pass, "{:?}", result.failures);
            assert_eq!(result.executed_on, expected, "for {selection:?}");
        }
    }

    /// A case the front end rejects has no program to run, so no engine claims it — the count of
    /// executed cases stays a count of programs that really executed, on both engines.
    #[test]
    fn a_case_the_front_end_rejects_claims_no_engine() {
        let case = "// expect: error E0002 at 3:6\n// expect: exit 1\necho \"oops;\n";
        let result = run_case("bad", case, Stage::Eval);
        assert_eq!(result.status, CaseStatus::Pass, "{:?}", result.failures);
        assert!(result.executed_on.is_empty());
    }

    /// A front-end stage runs no program at all, on any engine — and a header's stdout goes
    /// unchecked there, which is why the report says so instead of printing a bare "1 passed".
    #[test]
    fn a_front_end_stage_runs_no_engine() {
        let case = "// expect: stdout \"never printed\"\necho \"hello\";\n";
        for stage in [Stage::Lexer, Stage::Parser] {
            let result = run_case("x", case, stage);
            assert_eq!(result.status, CaseStatus::Pass, "at {stage}");
            assert!(result.executed_on.is_empty(), "at {stage}");
        }
    }

    #[test]
    fn negative_case_matches_error_code_and_position() {
        // Positions are absolute in the file; the two header lines push the `echo`
        // to line 3, where the unterminated string opens at column 6.
        let case = "// expect: error E0002 at 3:6\n// expect: exit 1\necho \"oops;\n";
        let result = run_case("bad", case, Stage::Eval);
        assert_eq!(result.status, CaseStatus::Pass, "{:?}", result.failures);
    }

    #[test]
    fn malformed_header_is_reported() {
        let case = "// expect: nonsense\necho \"x\";\n";
        assert_eq!(
            run_case("m", case, Stage::Eval).status,
            CaseStatus::Malformed
        );
    }
}
