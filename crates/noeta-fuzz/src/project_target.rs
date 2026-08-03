//! **The two front-ends, held against each other over generated *projects*.**
//!
//! `noeta check` and `noeta run` do not share a front-end. `check` goes through
//! [`noeta_ide::project_check`] — the one entry the LSP's `workspace/diagnostic` and the MCP
//! `check` tool also call — which walks the tree, groups files by module pool, and drives the
//! **salsa** workspace (`noeta-db`). `run` goes through [`noeta_runner::compile`], the loader's
//! directory reader. Two implementations of "which files are this program, what are their module
//! paths, and is it well-formed", and only one of them was ever fuzzed: every other target in this
//! crate — `run`, `typed`, `jit`, `leaks`, `bundle`, `fmt` — drives the run side.
//!
//! That gap is not hypothetical. Four defects were found and fixed by hand on the check path, all
//! invisible to 250,000+ generated *programs*, because no oracle went through it. Three of them are
//! literally "check says X, run says Y":
//!
//! 1. `noeta-ide` re-derived module paths and never learned the loader's migrations/seeds
//!    exception, so `check` reported E0074 on the filenames `noeta migrate new` generates.
//! 2. The salsa linker gated its derivation pass on `derived().is_some()`, and an *illegal* path is
//!    not a derived one — so a package whose only module was `src/my-utils.noe` was accepted by
//!    `check` and refused by `run`.
//! 3. `DirectiveCtx::source_dir` got a `file:` URI instead of a path on the salsa path, so
//!    file-opening directive hooks reported ENOENT on files that exist.
//!
//! Each is a *layout* fact, not a typing fact, which is why a generator that emits one buffer of
//! source could never have found them. The input here is therefore a **package on disk**: a
//! manifest, a source directory, a data directory, a subdirectory, and file names that are and are
//! not spellable as namespace segments.
//!
//! # The invariant
//!
//! For one generated project, with the **same entry set** on both sides:
//!
//! > `project_check` accepts the project **iff** the run-side front-end accepts every entry in it.
//!
//! One boolean, both directions, because both directions have produced defects: instance 2 was
//! `check` too lenient, instances 1 and 3 were `check` too strict. [`Violation`] names which way it
//! went, since that is what says where to look.
//!
//! ## What this deliberately does not assert
//!
//! * **Not diagnostic equality.** The two sides do not emit the same diagnostic *set* and are not
//!   supposed to: `check` deduplicates one module's fault across every entry that links it, and
//!   folds several code-tier shapes of one entry together, while the run side reports per entry, in
//!   one shape. Asserting set equality would fail on the design rather than on a defect. The
//!   accept/reject boolean is the part both sides really do claim to agree on, and it is the part
//!   every one of the three defects above broke.
//! * **Not the compile step.** The run side stops after link + tier activation + type-check, which
//!   is exactly what `noeta_db::entry_diagnostics(…, CheckFlavor::Compile)` computes on the other
//!   side. "A checked program compiles" is a real invariant, but it is *already* the run oracle's
//!   invariant 1 ([`crate::run_target`]) — folding it in here would report one defect from two
//!   places and blur which front-end was at fault.
//! * **Not execution.** Nothing is run. A project's *dynamic* behaviour is what the run oracle
//!   sweeps; the question here is static acceptance, and running would multiply the cost of the
//!   most expensive oracle in the crate for a property another one already holds.
//! * **Not code tiers.** `project_check` checks one shape per code tier an entry's blocks name;
//!   the run side compiles the shipping shape only. The generator emits no tier block, so the two
//!   entry-shape sets coincide — and that is a *precondition*, asserted by the suite
//!   ([`ProjectOutcome::tiers_checked`]) rather than assumed, for the same reason
//!   [`crate::run_target::uses_dynamic_typing`] is.
//!
//! # Why the entry sets are made to match
//!
//! `project_check(root)` checks **every** `.noe` file under `root` as its own entry — that is what
//! makes it see a library module no entry imports. So the run side is asked the same question
//! about the same files: [`entries`] walks the tree independently (a second implementation on
//! purpose — a divergence in the *walk* is exactly as much a defect as one in the linker) and each
//! file is loaded as its own entry. Without that the boolean would be comparing "is anything in
//! this tree wrong" against "is this one program wrong", and a fault in an unimported module would
//! read as a divergence forever.

use std::path::{Path, PathBuf};

/// The base seed this target's sweeps walk. A failure reports the nonce, and
/// [`materialize`] reproduces the exact project.
pub const BASE_SEED: u64 = 0x_C4EC_0DE5;

/// The shape a generated program is written to disk in.
///
/// These are not decorative. Each names a place the two front-ends derive something *separately* —
/// the package root, the source directory, the data directories, the derived module path — and the
/// three fixed check-vs-run defects lived in the last three of them. A layout that no sweep
/// produces is a seam no sweep covers, so the suite asserts every variant is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layout {
    /// No manifest: a bare script. The pool is its own directory, scanned flat, and nothing
    /// derives a module path.
    Script,
    /// No manifest, two files in one directory — the flat-pool case with a sibling to link.
    ScriptPair,
    /// A package with its module under the conventional `src/`.
    PackageSrc,
    /// A package with its module at the package root — `src/` is a layout choice, and both
    /// locations must derive the same path.
    PackageFlat,
    /// A package with a subdirectory: `src/deep/helper.noe` derives `<prefix>.deep.helper`.
    PackageNested,
    /// A package with a `migrations/` **data directory** beside a normal `src/`. A migration is a
    /// program the driver runs, not a module, and its timestamped name derives no path.
    DataDir,
    /// The same, with **no** `src/` — every member is a program. The shape defect 1 lived in: with
    /// no legally-derived sibling, the surface that re-derived paths on its own reported E0074 on
    /// a file whose name is exactly right for what it is.
    DataDirOnly,
    /// A package whose only module has a name no `use` can spell (`src/my-utils.noe`). The shape
    /// defect 2 lived in: no legally-derived sibling and no dependency, so the salsa linker skipped
    /// the derivation pass and never raised the refusal the pass exists to make.
    UnspellableOnly,
    /// The same unspellable file **with** a legally-derived sibling — the shape that hid defect 2,
    /// kept so a regression cannot pass by covering only the easy half.
    UnspellableSibling,
}

/// Every [`Layout`], in the order [`layout_for`] cycles them.
pub const LAYOUTS: &[Layout] = &[
    Layout::Script,
    Layout::ScriptPair,
    Layout::PackageSrc,
    Layout::PackageFlat,
    Layout::PackageNested,
    Layout::DataDir,
    Layout::DataDirOnly,
    Layout::UnspellableOnly,
    Layout::UnspellableSibling,
];

/// The layout nonce `nonce` is materialized in.
///
/// A plain cycle rather than a byte-derived choice: nine layouts against a sweep of a few hundred
/// projects, chosen from entropy, leaves the tail ones to chance, and a seam covered "usually" is a
/// seam that regresses on the run where it was not. Every layout is reached every 9 nonces, and the
/// suite asserts it rather than trusting this arithmetic.
pub fn layout_for(nonce: u32) -> Layout {
    LAYOUTS[nonce as usize % LAYOUTS.len()]
}

/// The manifest a generated package ships. No dependencies **on purpose**: every dependency module
/// derives a module path, and defect 2 was invisible in any workspace that had one.
const MANIFEST: &str = "[package]\nname = \"local/genpkg\"\nversion = \"0.1.0\"\n";

/// A file name `noeta migrate new` would produce: timestamped, so its stem is not a namespace
/// segment and the data-directory exception is what keeps it legal.
const MIGRATION_NAME: &str = "20260719000002_more_users.noe";

/// A module file name that cannot be a namespace segment. `-` is the character a package author
/// reaches for and the one no `use` can spell.
const UNSPELLABLE_NAME: &str = "my-utils.noe";

/// The sibling-module bodies. Fixed rather than generated: the variation this target is about is in
/// the *layout*, and a generated sibling would reject for ordinary typing reasons on nearly every
/// project, drowning the layout signal in noise the run oracle already sweeps for.
///
/// The three are the three ways a second file can relate to the first: silent, importing (a `use`
/// in one file is program-wide state in the linker — a fourth fixed defect had one file's
/// `use std.args` capturing `args` in every other), and **declaring** a namespace of its own, which
/// a package must hold against the path its location derives.
const SIBLINGS: &[&str] = &[
    "pub fn helper(): int { return 1; }\n",
    "use std.math\npub fn helper(x: int): int { return x + 1; }\n",
    "namespace shared;\npub fn helper(): int { return 2; }\n",
];

/// The generated program body for `(seed, nonce)`.
///
/// Two populations, alternating, because the invariant is a boolean and needs both of its values to
/// be reachable. The **syntax** generator ([`crate::generate`]) produces mostly ill-typed programs —
/// the rejected half, where a leniency divergence shows. The **type-directed** generator
/// ([`crate::typed`]) produces programs that are correct by construction — the accepted half, where
/// a false-positive divergence shows, and the only way to reach it: a rejection means nothing
/// unless you already know the program was good.
pub fn body(seed: u64, nonce: u32) -> String {
    let bytes = crate::seed_bytes(seed, nonce);
    if nonce.is_multiple_of(2) {
        crate::typed::program(&bytes)
    } else {
        crate::generate::program_with(&bytes, &crate::generate::GenOptions::terminating())
    }
}

/// Write the project `(seed, nonce)` denotes into the (existing, empty) directory `root`, and say
/// which [`Layout`] it took.
///
/// The caller owns `root` — a per-process fixture directory, never a fixed path under the system
/// temp dir, because this repository is routinely worked in several worktrees at once and a shared
/// fixture name is deleted out from under a concurrent run (see `noeta-test-temp`).
pub fn materialize(root: &Path, seed: u64, nonce: u32) -> Layout {
    let layout = layout_for(nonce);
    let program = body(seed, nonce);
    let sibling = SIBLINGS[(nonce as usize / LAYOUTS.len()) % SIBLINGS.len()];
    let write = |relative: &str, text: &str| {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create a fixture directory");
        }
        std::fs::write(&path, text).expect("write a fixture file");
    };
    match layout {
        Layout::Script => write("main.noe", &program),
        Layout::ScriptPair => {
            write("main.noe", &program);
            write("helper.noe", sibling);
        }
        Layout::PackageSrc => {
            write("noeta.toml", MANIFEST);
            write("src/main.noe", &program);
        }
        Layout::PackageFlat => {
            write("noeta.toml", MANIFEST);
            write("main.noe", &program);
        }
        Layout::PackageNested => {
            write("noeta.toml", MANIFEST);
            write("src/main.noe", &program);
            write("src/deep/helper.noe", sibling);
        }
        Layout::DataDir => {
            write("noeta.toml", MANIFEST);
            write("src/main.noe", sibling);
            write(&format!("migrations/{MIGRATION_NAME}"), &program);
        }
        Layout::DataDirOnly => {
            write("noeta.toml", MANIFEST);
            write(&format!("migrations/{MIGRATION_NAME}"), &program);
        }
        Layout::UnspellableOnly => {
            write("noeta.toml", MANIFEST);
            write(&format!("src/{UNSPELLABLE_NAME}"), &program);
        }
        Layout::UnspellableSibling => {
            write("noeta.toml", MANIFEST);
            write("src/main.noe", &program);
            write(&format!("src/{UNSPELLABLE_NAME}"), sibling);
        }
    }
    layout
}

/// Every `.noe` file under `root`, sorted — the entry set both sides are asked about.
///
/// A **second implementation** of `noeta_ide::project::noe_files`, deliberately: that walk is part
/// of the check side, and a check that quietly stops visiting a file would otherwise look like
/// agreement. Same rule (recursive, dotted directories skipped), independently written, so a drift
/// in either walk shows up as the run side being asked about a file the check side never saw.
pub fn entries(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            let hidden = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'));
            if hidden {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "noe") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// One front-end's static answer about a project.
#[derive(Debug, Clone)]
pub struct Verdict {
    /// Whether the front-end found nothing wrong. The whole comparison is this field; the rest is
    /// what a failure report needs in order to be actionable.
    pub accepted: bool,
    /// What it said, one rendered line per complaint. Empty when `accepted`.
    pub complaints: Vec<String>,
}

impl Verdict {
    /// A clean answer.
    fn clean() -> Verdict {
        Verdict {
            accepted: true,
            complaints: Vec::new(),
        }
    }

    /// A refusal, with what was said.
    fn refused(complaints: Vec<String>) -> Verdict {
        Verdict {
            accepted: false,
            complaints,
        }
    }
}

/// What the **check** side produced, plus the two facts the suite's preconditions are about.
#[derive(Debug, Clone)]
pub struct ProjectOutcome {
    pub verdict: Verdict,
    /// How many entries `project_check` reported checking — 0 means the sweep asked about nothing.
    pub files_checked: usize,
    /// The code tiers it swept beyond the shipping shape. Must stay empty: a non-empty set means
    /// `check` looked at shapes the run side never compiles, and the boolean stops being sharp.
    pub tiers_checked: Vec<String>,
}

/// **The check side**: [`noeta_ide::project_check`], the one entry `noeta check`, the LSP's
/// `workspace/diagnostic` and the MCP `check` tool share.
///
/// Operational `problems` — a file that could not be read, a dependency graph that would not
/// resolve — count as a refusal, not as a clean answer. They are not diagnostics (they are about
/// the check, not about the code), but a run that reports one has not checked what it was asked to,
/// and the CLI's exit code already treats them that way. Folding them in is what stops a check that
/// silently gave up from reading as agreement with a run that succeeded.
pub fn check_project(root: &Path) -> ProjectOutcome {
    noeta_conformance::ensure_std_registry();
    let options = noeta_ide::ProjectCheckOptions::new();
    let outcome = noeta_ide::project_check(root, &options);
    let mut complaints: Vec<String> = outcome.problems.clone();
    complaints.extend(
        outcome
            .diagnostics
            .iter()
            .filter(|d| d.diagnostic.severity == noeta_diagnostics::Severity::Error)
            .map(|d| {
                format!(
                    "{} {} @ {}",
                    d.diagnostic.code.code(),
                    d.diagnostic.message,
                    d.sources.source(d.diagnostic.span.source).name()
                )
            }),
    );
    // A native package this process does not carry would make every answer here an
    // unresolved-import cascade. Generated projects declare no dependencies, so this is empty; if
    // it ever is not, the comparison is meaningless and must not read as agreement.
    complaints.extend(
        outcome
            .uncomposed
            .iter()
            .map(|p| format!("uncomposed: {p}")),
    );
    ProjectOutcome {
        verdict: if complaints.is_empty() {
            Verdict::clean()
        } else {
            Verdict::refused(complaints)
        },
        files_checked: outcome.files_checked,
        tiers_checked: outcome.tiers_checked,
    }
}

/// **The run side**, for one entry: the loader front-end `noeta run` itself calls —
/// [`noeta_runner::compile::load_default_project`] (resolve front facts → load → link → activate
/// tiers) followed by the type-check it carries the provenance for.
///
/// Stops there rather than compiling to bytecode: see the module docs on why the compile step
/// belongs to [`crate::run_target`] and not here. Warnings do not refuse a program on either side,
/// so only error-severity diagnostics count.
///
/// `load_default_project` rather than `compile_whole_file` also means the **startup cache is
/// bypassed**, and that is the point rather than a shortcut: a cache hit returns a module without
/// running the front end at all, so it is not a front-end answer and cannot be compared with one.
/// This is `compile_whole_file`'s miss path — the same call the debugger, the profiler and the REPL
/// bootstrap make, for the same reason.
pub fn run_entry(entry: &Path) -> Verdict {
    match noeta_runner::compile::load_default_project(entry) {
        Err(failure) => Verdict::refused(vec![failure.to_text().0.trim_end().to_string()]),
        Ok(loaded) => {
            let checked = loaded.check();
            let errors: Vec<String> = checked
                .diagnostics
                .iter()
                .filter(|d| d.severity == noeta_diagnostics::Severity::Error)
                .map(|d| {
                    format!(
                        "{} {} @ {}",
                        d.code.code(),
                        d.message,
                        loaded.sources.source(d.span.source).name()
                    )
                })
                .collect();
            if errors.is_empty() {
                Verdict::clean()
            } else {
                Verdict::refused(errors)
            }
        }
    }
}

/// The run side over a whole project: every entry, and the first refusal wins.
///
/// "Every entry" is what makes the boolean comparable — `project_check` checks each `.noe` file as
/// its own entry, so a project is run-accepted only when each of them loads and checks clean.
pub fn run_project(root: &Path) -> (Verdict, Option<PathBuf>) {
    for entry in entries(root) {
        let verdict = run_entry(&entry);
        if !verdict.accepted {
            return (verdict, Some(entry));
        }
    }
    (Verdict::clean(), None)
}

/// How the two sides came out. The sweep asserts a floor on **both** of the first two, because a
/// boolean equality over a population that only ever takes one value has proved one implication and
/// skipped the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reach {
    /// Both front-ends accepted the project.
    Accepted,
    /// Both refused it.
    Rejected,
    /// They disagreed in the one way that is a **known open defect** rather than a finding — see
    /// [`is_open_divergence`]. Counted, reported, and asserted to still happen: an exception that
    /// outlives the bug it excuses is how an oracle goes quietly hollow.
    OpenDivergence,
}

/// **The one divergence this sweep tolerates, and the reason it is not a bug in the oracle.**
///
/// `noeta_ide::project::pool_sources` reads a package's members with
/// `noeta_loader::read_package_modules`, which **prunes the data directories** — a migration is a
/// program the driver runs, not a module of the package. A package whose only `.noe` files live in
/// `migrations/` (or `seeds/`) therefore has an empty member set, `workspace::sync` returns `None`,
/// and `sweep_pool` reports `cannot read <entry>` for every requested entry instead of falling
/// through to `check_lone`. Reproduced against the shipped v0.4.4 binary:
///
/// ```text
/// $ noeta check .                                   → exit 2, "cannot read ./migrations/…"
/// $ noeta run migrations/20260719000002_more_users.noe → exit 0, prints "hi"
/// ```
///
/// It is a real, open, check-too-strict divergence of exactly the class this target exists to find
/// — this sweep is what found it — and it is excused here only because the fix belongs in
/// `noeta-ide`, not in the oracle that reports it.
///
/// # Why this predicate and not a message match
///
/// The obvious exception is "the complaint starts with `cannot read`", and that would also swallow
/// a genuinely unreadable file — a permission fault, a race with a concurrent fixture, a real
/// refusal the sweep should shout about. So the test is stronger: **every** complaint must name a
/// path that this process can see is a readable file. "The check said it could not read a file that
/// is right there" is the defect and nothing else is.
///
/// The failure mode is benign in the direction that matters. If the message changes, or the defect
/// is fixed and the complaint stops appearing, the exception stops matching — it cannot go quietly
/// silent, only quietly loud. And the suite additionally asserts this outcome is still *reached*,
/// so the day it is fixed the sweep says so and asks for the exception to be deleted.
pub fn is_open_divergence(complaints: &[String]) -> bool {
    !complaints.is_empty()
        && complaints.iter().all(|complaint| {
            complaint
                .strip_prefix("cannot read ")
                .is_some_and(|path| Path::new(path.trim()).is_file())
        })
}

/// An agreement that did not hold, named by direction — which is what says where to look.
#[derive(Debug, Clone)]
pub enum Violation {
    /// `check` was clean and the run side refused an entry. The check-vs-run divergence class in
    /// its purest form: nothing underlines while you type, the editor is green, the agent's `check`
    /// tool reports success, and the failure appears only on Run.
    RunRefusedCheckedProject { entry: String, detail: Vec<String> },
    /// Every entry loaded and checked on the run side and `check` refused the project — a false
    /// positive in the surface the editor and the agent read. Defects 1 and 3 were this way round.
    CheckRefusedRunnableProject { detail: Vec<String> },
    /// A stage panicked. Both front-ends report an unacceptable project as a *value*, so a panic is
    /// a different thing: a shape nobody enumerated.
    Panicked { side: &'static str, where_: String },
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Violation::RunRefusedCheckedProject { entry, detail } => write!(
                f,
                "`check` accepted this project and the run front-end refused {entry}:\n  {}",
                detail.join("\n  ")
            ),
            Violation::CheckRefusedRunnableProject { detail } => write!(
                f,
                "the run front-end accepted every entry and `check` refused the project:\n  {}",
                detail.join("\n  ")
            ),
            Violation::Panicked { side, where_ } => {
                write!(f, "the {side} front-end panicked: {where_}")
            }
        }
    }
}

/// The class a violation belongs to, for deduplicating a sweep's findings down to distinct defects.
/// The direction plus the leading diagnostic code — messages carry names and paths that differ per
/// project, the code does not.
pub fn class(v: &Violation) -> String {
    let code = |detail: &[String]| {
        detail
            .first()
            .and_then(|line| line.split_whitespace().next())
            .unwrap_or("?")
            .to_string()
    };
    match v {
        Violation::RunRefusedCheckedProject { detail, .. } => {
            format!("run-refused-{}", code(detail))
        }
        Violation::CheckRefusedRunnableProject { detail } => {
            format!("check-refused-{}", code(detail))
        }
        Violation::Panicked { side, where_ } => format!(
            "panic-{side}-{}",
            where_.split_whitespace().next().unwrap_or("?")
        ),
    }
}

/// What one project's evaluation produced: how far it got, and the check side's own facts (which
/// the sweep's preconditions are about).
#[derive(Debug, Clone)]
pub struct Evaluated {
    pub reach: Reach,
    pub files_checked: usize,
    pub tiers_checked: Vec<String>,
}

/// **Run the oracle over one materialized project.**
///
/// `Ok` says the two front-ends agreed and which way; `Err` names the direction they did not.
pub fn evaluate(root: &Path) -> Result<Evaluated, Violation> {
    let checked = check_project(root);
    let (ran, entry) = run_project(root);
    compare(checked, ran, entry)
}

/// **The comparison itself** — one implementation, so the panic-catching wrapper and the
/// panic-transparent entry point cannot come to disagree about what agreement means.
fn compare(
    checked: ProjectOutcome,
    ran: Verdict,
    entry: Option<PathBuf>,
) -> Result<Evaluated, Violation> {
    let reach = match (checked.verdict.accepted, ran.accepted) {
        (true, true) => Reach::Accepted,
        (false, false) => Reach::Rejected,
        (true, false) => {
            return Err(Violation::RunRefusedCheckedProject {
                entry: entry
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<none>".to_string()),
                detail: ran.complaints,
            });
        }
        (false, true) if is_open_divergence(&checked.verdict.complaints) => Reach::OpenDivergence,
        (false, true) => {
            return Err(Violation::CheckRefusedRunnableProject {
                detail: checked.verdict.complaints,
            });
        }
    };
    Ok(Evaluated {
        reach,
        files_checked: checked.files_checked,
        tiers_checked: checked.tiers_checked,
    })
}

/// [`evaluate`], made total: a panic on either side becomes a [`Violation::Panicked`] rather than
/// taking the process with it.
///
/// The same shape [`crate::run_target::evaluate_total`] uses, and for the same reason — a sweep
/// that aborts at its first hit tells you about one project and nothing about the hundreds behind
/// it. The panic hook is silenced for the duration so a scan with many hits stays readable, and the
/// location is recovered from the payload, which is what [`class`] deduplicates on.
pub fn evaluate_total(root: &Path) -> Result<Evaluated, Violation> {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{Arc, Mutex};

    let site: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&site);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let at = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "?".to_string());
        *sink.lock().expect("panic-site sink") = format!("{at} — {}", panic_message(info));
    }));
    // The two sides are entered separately so the report can say WHICH front-end died: a panic in
    // salsa and a panic in the loader are different defects, and a single catch around both would
    // only be able to say "something".
    let checked = catch_unwind(AssertUnwindSafe(|| check_project(root)));
    let ran = checked
        .as_ref()
        .ok()
        .map(|_| catch_unwind(AssertUnwindSafe(|| run_project(root))));
    std::panic::set_hook(previous);

    let where_ = || site.lock().expect("panic-site sink").clone();
    let Ok(checked) = checked else {
        return Err(Violation::Panicked {
            side: "check",
            where_: where_(),
        });
    };
    let Some(Ok((ran, entry))) = ran else {
        return Err(Violation::Panicked {
            side: "run",
            where_: where_(),
        });
    };
    compare(checked, ran, entry)
}

/// The panic's own message, for the report.
fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_string())
}
