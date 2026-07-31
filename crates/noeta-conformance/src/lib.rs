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

mod bundle;
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

pub use bundle::{BundleFailure, BundleReport, run_bundle_roundtrip};
pub use differential::{DiffReport, Mismatch, run_differential};
pub use expectation::{ErrorExpectation, Expectations};
pub use ir_corpus::{IrCorpusReport, run_ir_corpus};
#[cfg(feature = "jit")]
pub use jit_differential::{JitDiffReport, run_jit_differential};
pub use leaks::{Leak, LeakReport, run_leak_check};
pub use report::{CaseResult, CaseStatus, NotRun, Report};
pub use wasm::{WasmDiffFailure, WasmDiffReport, run_wasm_differential};

/// Which pipeline stages to run a case through. Narrowing the stage makes an agent's
/// inner loop fast (`--stage parser` reruns only lexing+parsing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stage {
    Lexer,
    Parser,
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
}

/// What actually happened when a case ran: its captured stdout and stderr, exit code, and the
/// `(code, line, col)` of every diagnostic, in order.
struct Outcome {
    stdout: String,
    stderr: String,
    exit_code: i32,
    errors: Vec<ErrorExpectation>,
}

/// Run a source string through the pipeline up to `stage` and capture its outcome.
fn run_source(name: &str, text: &str, stage: Stage) -> Outcome {
    let source = Source::new(SourceId::FIRST, name, text);

    // Seed the lexer with the installed extensions' verbatim-body tiers (`doc`, native `@json`) so
    // a native tier's `@<name> { … }` body captures — the loader/pipeline does this too; the
    // conformance single-file path must match or the differential would lex a native tier's body
    // as code. The lexer's own two-pass covers a program's `@tier(…, text/expr)` declarations.
    let mut tier_names: Vec<String> = noeta_stdlib::registry::ext_verbatim_tier_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    let lexed = noeta_lexer::lex_in(
        &source,
        noeta_lexer::Edition::DEFAULT,
        &noeta_lexer::TextTiers::with(tier_names.clone()),
    );
    // Union the file's own `@tier(…, text/expr)` declarations (the lexer's two-pass discovered
    // them) so a `${…}` hole's re-lex knows *this* file's tiers too — an inline `@t { … }` loop
    // body inside a hole. Re-lexing is idempotent, so a second pass with the full set is safe.
    tier_names.extend(lexed.text_tier_decls.iter().cloned());
    let ext_tiers = noeta_lexer::TextTiers::with(tier_names);
    let lexed = noeta_lexer::lex_in(&source, noeta_lexer::Edition::DEFAULT, &ext_tiers);
    let mut diagnostics = lexed.diagnostics.clone();

    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut exit_code;

    if stage == Stage::Lexer {
        exit_code = if diagnostics.is_empty() { 0 } else { 1 };
    } else {
        let parsed = noeta_parser::parse_in(
            &source,
            &lexed.tokens,
            noeta_lexer::Edition::DEFAULT,
            &ext_tiers,
        );
        diagnostics.extend(parsed.diagnostics);

        // The type checker (M1.7) is the front-end gate for the eval stage: a program with type
        // errors is rejected before it runs, exactly as the bytecode pipeline gates it. Running
        // it here (not only on the VM path) is what lets a negative type-error case assert via
        // `// expect: error E00xx`, and keeps the tree-walker and VM observably identical.
        // One `check_all` yields both the gate diagnostics and the site bundle the eval backend
        // needs, so the checker runs once per case instead of again inside the backend.
        let mut sites = noeta_check::Sites::default();
        if stage == Stage::Eval && diagnostics.is_empty() {
            let checked = noeta_check::check_all(&parsed.program);
            diagnostics.extend(checked.diagnostics);
            sites = checked.sites;
        }

        // Only evaluate a program that checked cleanly and only when asked to. The reference is the
        // Core-IR interpreter (the migration's Phase-4 reference semantics), so conformance pins the
        // same last-use destruction the VM produces.
        //
        // "Cleanly" means **no errors**, not "no diagnostics": a warning says the program is
        // well-formed and compiles, so a case that trips one still runs and still exits 0. Gating on
        // emptiness made every warning a silent hard stop with no output at all, which is the one
        // thing a warning must not be.
        if stage == Stage::Eval && !has_error(&diagnostics) {
            let result = reference::reference_run(&parsed.program, sites);
            stdout = result.stdout;
            stderr = result.stderr;
            diagnostics.extend(result.diagnostics);
            exit_code = result.exit_code;
        } else {
            exit_code = if has_error(&diagnostics) { 1 } else { 0 };
        }
    }

    // A compile error always means a failing exit, even if a stage stopped early.
    if has_error(&diagnostics) && exit_code == 0 {
        exit_code = 1;
    }

    Outcome {
        stdout,
        stderr,
        exit_code,
        errors: errors_of(&source, &diagnostics),
    }
}

/// Whether any diagnostic is an **error** — the gate on running a program and on a failing exit.
///
/// A [`Severity::Warning`](noeta_diagnostics::Severity::Warning) is by definition compatible with
/// running: it says the program is well-formed and something in it is worth a second look. It is
/// still reported (a case asserts it with the same `// expect: error <CODE> at …` header — the
/// header names a diagnostic, whatever its severity), it simply does not stop the program.
pub(crate) fn has_error(diagnostics: &[Diagnostic]) -> bool {
    diagnostics
        .iter()
        .any(|d| d.severity == noeta_diagnostics::Severity::Error)
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

/// Run a single named case (already-loaded source text) and compare it to its header.
pub fn run_case(name: &str, text: &str, stage: Stage) -> CaseResult {
    ensure_std_registry();
    let expectations = match Expectations::parse(text) {
        Ok(expectations) => expectations,
        Err(message) => return CaseResult::malformed(name, message),
    };
    let outcome = run_source(name, text, stage);
    compare(name, &expectations, &outcome, stage)
}

fn compare(name: &str, expected: &Expectations, actual: &Outcome, stage: Stage) -> CaseResult {
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

    if failures.is_empty() {
        CaseResult::pass(name)
    } else {
        CaseResult::fail(name, failures)
    }
}

/// Run a multi-file case rooted at `entry` (the `main.noe` of a module fixture): sibling
/// modules are loaded and linked (M1.9) and the merged program is checked and run like any
/// other case. The expectation header lives in the entry file.
pub fn run_case_path(entry: &Path, display: &str, stage: Stage) -> CaseResult {
    let text = match std::fs::read_to_string(entry) {
        Ok(text) => text,
        Err(err) => return CaseResult::malformed(display, format!("could not read: {err}")),
    };
    let expectations = match Expectations::parse(&text) {
        Ok(expectations) => expectations,
        Err(message) => return CaseResult::malformed(display, message),
    };
    let outcome = run_linked(entry, stage);
    compare(display, &expectations, &outcome, stage)
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
        .name
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
            // The package's own root segment is the LAST prefix segment — the package half of the
            // identity, which is what a single-segment name gives too.
            let root = prefix.last().cloned().unwrap_or_default();
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

/// Load + link `entry` and run the merged program to an [`Outcome`]. Lex/parse errors render
/// against the source they came from; check/runtime diagnostics against the entry source.
fn run_linked(entry: &Path, stage: Stage) -> Outcome {
    // A case with package subdirectories links through the dependency-aware path so its sources
    // carry real package provenance; one without keeps the deps-free path byte-for-byte.
    let deps = entry.parent().map(dep_packages).unwrap_or_default();
    // A case declaring a manifest is a package: its files derive their module paths ([`case_root`]).
    // One without keeps `None` — nothing derives and each file's `namespace` declaration stands.
    let root = case_root(entry);
    let load = if deps.is_empty() {
        noeta_loader::load(entry, noeta_lexer::Edition::DEFAULT, root.as_ref())
    } else {
        // Conformance cases carry no manifest `[tiers]`/`[directives]`, so an empty `PackageUses`
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
        Ok(Err(load_diagnostics)) => {
            let errors = load_diagnostics
                .iter()
                .flat_map(|ld| errors_of(&ld.source, std::slice::from_ref(&ld.diagnostic)))
                .collect();
            return Outcome {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 1,
                errors,
            };
        }
        Err(err) => {
            return Outcome {
                stdout: format!("could not read: {err}"),
                stderr: String::new(),
                exit_code: 1,
                errors: Vec::new(),
            };
        }
    };

    // The loader already lexed + parsed cleanly; the lexer/parser stages have nothing more to do.
    if stage != Stage::Eval {
        return Outcome {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            errors: Vec::new(),
        };
    }

    // Check/runtime diagnostics may land on a declaration merged in from a sibling module, so they
    // resolve through the source map (by each span's `SourceId`) rather than always against the
    // entry — that is what gives a cross-module error its real file/line/column.
    // Checked under the program's real per-source provenance, so a package-boundary rule (E0070)
    // sees the same graph a `noeta run` of the same layout does.
    let checked = noeta_check::check_all_with(
        &linked.program,
        noeta_check::CheckOptions {
            editions: linked.editions.clone(),
            packages: linked.packages.clone(),
            ..noeta_check::CheckOptions::default()
        },
    );
    if has_error(&checked.diagnostics) {
        return Outcome {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 1,
            errors: errors_of_mapped(&linked.sources, &checked.diagnostics),
        };
    }
    // A check that produced only warnings runs — and the warnings stay in the reported list, ahead
    // of anything the run itself reports, so a case can assert one and its output in one header.
    let mut diagnostics = checked.diagnostics;
    let result = reference::reference_run(&linked.program, checked.sites);
    diagnostics.extend(result.diagnostics);
    Outcome {
        errors: errors_of_mapped(&linked.sources, &diagnostics),
        stdout: result.stdout,
        stderr: result.stderr,
        exit_code: result.exit_code,
    }
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

/// Discover and run every case under `root`, optionally narrowed to a single entry file.
/// Returns a [`Report`] that can be rendered as text or JSON.
pub fn run_corpus(root: &Path, only: Option<&Path>, stage: Stage) -> Report {
    let mut cases = Vec::new();
    collect_cases(root, &mut cases);
    cases.sort_by(|a, b| a.entry.cmp(&b.entry));

    let mut report = Report::default();
    for case in cases {
        if let Some(only) = only
            && case.entry != only
            && !case.entry.ends_with(only)
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
            report.push(run_case_path(&case.entry, &display, stage));
        } else {
            match std::fs::read_to_string(&case.entry) {
                Ok(text) => report.push(run_case(&display, &text, stage)),
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
