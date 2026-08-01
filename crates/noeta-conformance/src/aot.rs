//! The **AOT differential oracle** (parallel-path audit row 9): every corpus program, built into a
//! `noeta build --native` artifact — AOT object, `cc`-linked against the real AOT runtime archive,
//! bundle stapled on — and executed as a native binary, must produce byte-for-byte what `noeta run`
//! produces from the *same module*: stdout, the whole stderr stream (the program's own `std.io`
//! `err`/`errln`, then diagnostics, then the traceback), and the process exit code.
//!
//! ## Why this exists
//!
//! `--native` was the one execution surface with no differential oracle. What gated it was a single
//! hand-written program (`noeta-cli/tests/cli/build.rs`'s
//! `build_native_matches_a_source_run_byte_for_byte`): all-int `sq`/`fib`/loop, `echo` only, stdout
//! only, and a silent `return` when `cargo` or `cc` is missing. Two things lived in that blind spot
//! at once — an AOT-only soundness bug found late (`0f9752d4c`, the misaligned dispatch table), and
//! the AOT run tail quietly dropping `RunResult.stderr` and truncating the exit code while four
//! other tails grew those fixes.
//!
//! ## What the comparison holds fixed
//!
//! One module, two shipped surfaces. Both sides get the **same compiled module** (this harness
//! compiles it once, exactly as the bundle/wasm/JIT oracles do) and run it on the **real host**:
//!
//! * the artifact — the linked native binary, dispatching AOT-compiled bodies, with
//!   `noeta_aot_runtime`'s tail;
//! * the truth — `noeta run` over that module's `.noeb`, which is `noeta_runner::run_compiled_module`,
//!   the tail whose own doc says it exists so every surface "presents identical output".
//!
//! So a divergence is either the AOT codegen or the AOT tail, which are exactly the two things this
//! surface has no other gate for. The expectation is deliberately *not* composed from the AOT tail's
//! own rendering: an expectation copied from the implementation under test agrees with its own bugs,
//! which is how the wasm oracle shipped a vacuous stderr assertion for weeks.
//!
//! The bundle is written as a file literally named `<aot>`, and `noeta run` is invoked from that
//! directory: a source-free run renders diagnostics and tracebacks against a synthetic source named
//! after the file it was handed, and `<aot>` is the name the AOT runtime uses. Same name on both
//! sides, so the stderr comparison is byte-for-byte rather than approximate.
//!
//! ## Nondeterminism and hangs
//!
//! Both sides run on the real host, where a program can read the clock, mint a UUID, or wait on a
//! socket. Neither is handled with a hand-maintained exclusion list:
//!
//! * a case whose output differs is re-run on **both** sides; a side that does not reproduce *itself*
//!   makes the case `nondeterministic` — excluded, counted, named;
//! * a case where both sides are self-stable and printed the same lines in a different order is
//!   `reordered` — the real scheduler, not the codegen (see [`reordered`]);
//! * either side is capped at [`RUN_TIMEOUT`]; a case that outlives it is `timed_out` — excluded,
//!   counted, named. (The corpus's server cases block on a real socket under a real executor; under
//!   the sandbox executor the whole corpus is instant, which is why the other oracles never met
//!   this.)
//!
//! A **host-level abort** is checked before any of that, and asked a different question. A Rust panic
//! prints an ASLR-varying address, so a crashing artifact looks exactly like a program that does not
//! reproduce itself, and the first version of this oracle nearly excluded a live crash as
//! "nondeterministic" — which is why the abort check comes first.
//!
//! It is still *checked for reproducibility*, just not by comparing bytes. The aborting side is
//! re-run [`ABORT_REPEATS`] times and asked whether it aborts **again**; any repeat that aborts makes
//! the case a divergence, and only [`ABORT_REPEATS`] consecutive clean runs earn an exclusion (named
//! and counted, as `aborted_once`). Coming first used to mean "off one sample", so an abort was the
//! one outcome the oracle judged with *less* evidence than an ordinary output disagreement — and it
//! is the arm anything load-induced lands in, which made a release gate that could go red because a
//! build was running beside it. A gate that goes red under load gets re-run instead of read.
//!
//! A case cannot slip out of coverage silently — it has to fail to reproduce itself, hang, or be a
//! pure reordering, first, and every one of those is printed with the case's name.
//!
//! ## Cost
//!
//! One `cc` link plus two process launches per program: minutes, not seconds, so this is the
//! **gate** arm. The cheap 80% is [`crate::jit_differential`]'s [`Arm::AotBodies`] — the same
//! codegen, finalized to pages instead of an object file, over the same corpus, with no linker at
//! all — which runs per-commit in `cargo test -p noeta-conformance --features jit`.
//!
//! [`Arm::AotBodies`]: crate::JitDiffArm::AotBodies

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};

use crate::collect_cases;

/// The file name the module's bundle is written under, and therefore the synthetic source name
/// `noeta run` renders against — `noeta_aot_runtime`'s own `<aot>`, so both tails name the same
/// source and the stderr comparison stays byte-for-byte.
const AOT_SOURCE: &str = "<aot>";

/// How long either side may take before the case is excluded as `timed_out`. Generous enough for a
/// debug-profile artifact under a loaded machine, short enough that a corpus of server cases does not
/// turn the gate into a nap. `NOETA_AOT_TIMEOUT_SECS` overrides it.
const RUN_TIMEOUT: Duration = Duration::from_secs(20);

/// The outcome of an AOT differential run over a corpus.
#[derive(Debug)]
pub struct AotDiffReport {
    /// Programs whose linked native artifact was byte-identical to the interpreted run.
    pub matched: usize,
    /// Every case excluded before the comparison, by reason.
    pub not_run: crate::NotRun,
    /// Cases excluded because the truth run does not reproduce its own output (clock/RNG/env) —
    /// named, not silently dropped.
    pub nondeterministic: Vec<String>,
    /// Cases where a side fell over at the host level **once** and then ran clean [`ABORT_REPEATS`]
    /// times — named, with the signal or status the one abort ended on. Not a failure (an abort that
    /// will not reproduce in six runs is not evidence about the codegen), but the loudest exclusion
    /// this report has: a case that keeps landing here is a case to go and read.
    pub aborted_once: Vec<String>,
    /// Cases excluded because a side outlived [`RUN_TIMEOUT`] (a real socket, a real sleep) — named,
    /// with which side hung.
    pub timed_out: Vec<String>,
    /// Cases whose two sides printed the same lines in a different order — see [`reordered`].
    pub reordered: Vec<String>,
    /// Programs whose native artifact diverged from the interpreted run.
    pub failures: Vec<AotDiffFailure>,
    /// Wall-clock split, in milliseconds, over the whole corpus: AOT codegen, `cc` link, running the
    /// artifact, and the truth runs. Printed so the per-commit / gate-only call is a measurement.
    pub compile_ms: u128,
    pub link_ms: u128,
    pub run_ms: u128,
    pub truth_ms: u128,
    /// Prototypes the AOT codegen emitted as real native bodies across the corpus (the AOT twin of
    /// the JIT differential's `native_protos`) — coverage, so "it linked" can never mean "it linked
    /// an empty dispatch table".
    pub native_protos: usize,
    /// Whether this run was narrowed to one case (`--file`). A narrowed run cannot say anything about
    /// the [`KNOWN_DIVERGENCES`] rows it did not run, so the stale-row half of the ratchet is only
    /// asserted for a whole-corpus run.
    pub narrowed: bool,
    /// The expect-fail list this report is judged against — [`KNOWN_DIVERGENCES`] on every real run.
    ///
    /// It is a field rather than a direct read of the const so the ratchet's **own** tests can pose
    /// a list to it. They used to derive their fixtures from the const (`KNOWN_DIVERGENCES.skip(1)`),
    /// which meant the self-retiring half stopped testing anything the moment the last row was
    /// deleted — precisely the state the ratchet exists to reach, and the one where the rule must
    /// still hold.
    pub listed: &'static [(&'static str, &'static str)],
}

impl Default for AotDiffReport {
    fn default() -> Self {
        Self {
            matched: 0,
            not_run: crate::NotRun::default(),
            nondeterministic: Vec::new(),
            aborted_once: Vec::new(),
            timed_out: Vec::new(),
            reordered: Vec::new(),
            failures: Vec::new(),
            compile_ms: 0,
            link_ms: 0,
            run_ms: 0,
            truth_ms: 0,
            native_protos: 0,
            narrowed: false,
            listed: KNOWN_DIVERGENCES,
        }
    }
}

/// One program whose native artifact diverged.
#[derive(Debug)]
pub struct AotDiffFailure {
    pub name: String,
    /// Which stream disagreed — `stdout`, `stderr`, `exit`, or `build` — so a divergence can be
    /// matched against [`KNOWN_DIVERGENCES`] by identity rather than by message text.
    pub stream: &'static str,
    pub detail: String,
}

/// **Known divergences**: defects this oracle found or confirmed, which are not its to fix, listed
/// so it can land green while still failing on anything new.
///
/// **Four rows have already left this list, and the ratchet is why.**
///
/// `modules/derived_package_path/main.noe` was the one crash — and it was the row this list existed
/// for. The linked artifact aborted every time at `dispatch.rs`'s `&chunk.code[pc]` with a
/// pointer-shaped `pc`. The mechanism was the S4.1 direct-call convention tag: `jit_prepare_call`
/// marks a fast-convention callee by setting bit 0 of the entry pointer, and Cranelift's object
/// backend aligns function bodies to **1** on x86-64, so a body could land on an odd address —
/// `ff | 1` said nothing, the caller's `& !1` called `ff - 1`, that `ret` handed the address back as
/// the callee's outcome, and `jit_after_call` wrote it into the callee frame as a resume pc. Fixed
/// at the root by asking Cranelift for a real function alignment on both ISAs, with `jit_install`
/// refusing any entry the tag cannot describe. The in-process AOT-bodies arm never saw it because
/// the runtime JIT allocates each body through its own finalize, which hands out aligned memory.
///
/// `io/streams.noe` and `io/to_string_parity.noe` were here because the AOT tail dropped the
/// program's own `std.io` `err`/`errln` stream. `std/os_exit.noe` was here because `os.exit(3)`
/// exited **1** — the tail's `as u8` truncation was only half of it, and the `ExitCode` → `c_int`
/// collapse in `run_embedded_with_extensions` was the binding constraint, with `RunTail::status()`
/// holding the right byte and `emit()` throwing it away one call later.
///
/// The first two are instructive about *this list* rather than about the AOT path: the tail fix
/// (audit row 1) and this oracle (row 9) were built on separate branches, so each was correct about
/// the tree it could see and the rows were honest when written. The moment both merged, the defect
/// was gone and the rows were not — and the gate failed, naming them, and told whoever read it to
/// delete them. That is the whole design. A suppression list would have gone quiet instead, and the
/// next person would have inherited two entries describing a bug that no longer existed.
///
/// The list is an **expect-fail ratchet**, not a suppression: the oracle asserts the failure set is
/// *exactly* this list, so a new divergence fails **and a fixed one fails too**, with instructions to
/// delete the row. A stale entry cannot outlive the bug it describes. The list is currently
/// **empty**: every corpus program's linked artifact runs byte-identically to `noeta run`, and the
/// next entry added here should be as short-lived as the crash row was.
pub const KNOWN_DIVERGENCES: &[(&str, &str)] = &[];

impl AotDiffReport {
    /// Divergences that are not a known, tracked tail gap — the ones that fail this oracle.
    pub fn unexpected(&self) -> Vec<&AotDiffFailure> {
        self.failures
            .iter()
            .filter(|f| !self.listed.contains(&(f.name.as_str(), f.stream)))
            .collect()
    }

    /// Known gaps that did **not** reproduce: either fixed (delete the row) or no longer covered
    /// (which is worse — the case stopped running). Either way the list is now a lie.
    pub fn stale_gaps(&self) -> Vec<&(&'static str, &'static str)> {
        if self.narrowed {
            return Vec::new();
        }
        self.listed
            .iter()
            .filter(|(name, stream)| {
                !self
                    .failures
                    .iter()
                    .any(|f| f.name == *name && f.stream == *stream)
            })
            .collect()
    }

    /// Whether every compiled program ran identically as a linked native binary, modulo the known
    /// tail gaps — and that every one of those gaps still reproduces.
    pub fn ok(&self) -> bool {
        self.unexpected().is_empty() && self.stale_gaps().is_empty()
    }

    /// A human-readable summary.
    pub fn to_human(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "AOT differential: {} ran and agreed, {} not run ({}), {} nondeterministic, \
             {} aborted once, {} timed out; {} prototypes compiled to native bodies",
            self.matched,
            self.not_run.total(),
            self.not_run.to_human(),
            self.nondeterministic.len(),
            self.aborted_once.len(),
            self.timed_out.len(),
            self.native_protos,
        );
        let _ = writeln!(
            out,
            "  wall clock: {} ms AOT codegen, {} ms link, {} ms artifact runs, {} ms truth runs",
            self.compile_ms, self.link_ms, self.run_ms, self.truth_ms,
        );
        if !self.nondeterministic.is_empty() {
            let _ = writeln!(
                out,
                "{} case(s) EXCLUDED as nondeterministic (the truth run disagreed with itself, so \
                 no artifact comparison is meaningful):",
                self.nondeterministic.len()
            );
            for name in &self.nondeterministic {
                let _ = writeln!(out, "  {name}");
            }
        }
        if !self.aborted_once.is_empty() {
            let _ = writeln!(
                out,
                "{} case(s) EXCLUDED as a ONE-OFF ABORT — a side fell over at the host level and \
                 then ran clean {ABORT_REPEATS} times, so the abort is not evidence about the \
                 codegen. Read these anyway: an abort that reproduces is a DIVERGENCE, and this \
                 list is where a rare one would hide:",
                self.aborted_once.len(),
            );
            for name in &self.aborted_once {
                let _ = writeln!(out, "  {name}");
            }
        }
        if !self.reordered.is_empty() {
            let _ = writeln!(
                out,
                "{} case(s) EXCLUDED as REORDERED — same lines, same exit, different interleaving: \
                 concurrent programs whose ordering comes from a real clock (the corpus pins these \
                 orderings under the sandbox clock, where a 1ms and a 3ms sleep cannot tie):",
                self.reordered.len()
            );
            for name in &self.reordered {
                let _ = writeln!(out, "  {name}");
            }
        }
        if !self.timed_out.is_empty() {
            let _ = writeln!(
                out,
                "{} case(s) EXCLUDED on a {}s timeout — a real host means real sockets and real \
                 sleeps (raise it with NOETA_AOT_TIMEOUT_SECS):",
                self.timed_out.len(),
                RUN_TIMEOUT.as_secs(),
            );
            for name in &self.timed_out {
                let _ = writeln!(out, "  {name}");
            }
        }
        let unexpected = self.unexpected();
        let stale = self.stale_gaps();
        if !unexpected.is_empty() {
            let _ = writeln!(out, "{} AOT DIVERGENCE(s):", unexpected.len());
            for f in &unexpected {
                let _ = writeln!(out, "  {} — {}", f.name, f.detail);
            }
        }
        if !stale.is_empty() {
            let _ = writeln!(
                out,
                "{} KNOWN_DIVERGENCES row(s) did not reproduce — if the defect was fixed, DELETE \
                 the row (that is the ratchet); if the case stopped running, find out why:",
                stale.len()
            );
            for (name, stream) in &stale {
                let _ = writeln!(out, "  {name} ({stream})");
            }
        }
        let known = self.failures.len() - unexpected.len();
        if known > 0 {
            let _ = writeln!(
                out,
                "{known} known divergence(s) reproduced (tracked in KNOWN_DIVERGENCES, not this oracle's to fix):",
            );
            for f in &self.failures {
                if KNOWN_DIVERGENCES.contains(&(f.name.as_str(), f.stream)) {
                    let _ = writeln!(out, "  {} — {}", f.name, f.detail);
                }
            }
        }
        if self.ok() {
            out.push_str(
                "every compiled program runs byte-identically as a `--native` binary (modulo the \
                 known divergences above) ✓\n",
            );
        }
        out
    }
}

/// The external tooling the oracle drives, discovered once up front — never assumed, and a missing
/// one is a loud setup error naming the command that fixes it, not a skip.
struct AotTools {
    /// The C toolchain driver that links the artifact (`NOETA_CC`, else `cc`).
    cc: String,
    /// `libnoeta_aot.a` — the real archive `noeta build --native` links against.
    archive: PathBuf,
    /// The system libraries rustc says that archive needs (`native-static-libs`).
    libs: Vec<String>,
    /// The `noeta` binary whose `run` provides the truth side — the shipped tail, not a
    /// reimplementation of it.
    noeta: PathBuf,
    /// One scratch dir, with a per-worker subdirectory (each holds a bundle named `<aot>`, so the
    /// truth run's synthetic source name is the artifact's).
    workdir: PathBuf,
}

/// Locate `cc` and the AOT runtime archive, building the archive if it is not supplied.
///
/// `NOETA_AOT_RUNTIME_LIB` (+ `NOETA_AOT_LINK_LIBS`) short-circuits the build, exactly as the CLI's
/// `resolve_aot_runtime` honours them — so a gate that already built the archive hands it over
/// instead of paying for it twice.
fn discover_tools(root: &Path) -> Result<AotTools, String> {
    let cc = std::env::var("NOETA_CC").unwrap_or_else(|_| "cc".to_string());
    if which(&cc).is_none() {
        return Err(format!(
            "no C toolchain: `{cc}` is not on PATH — install one (e.g. `apt install build-essential`) \
             or set NOETA_CC=/path/to/cc"
        ));
    }
    let (archive, libs) = resolve_archive()?;
    if !archive.is_file() {
        return Err(format!(
            "the AOT runtime archive is not at {} — build it with:\n  \
             cargo rustc -p noeta-aot-runtime -- --print native-static-libs\n\
             (or set NOETA_AOT_RUNTIME_LIB=/path/to/libnoeta_aot.a)",
            archive.display()
        ));
    }
    let noeta = resolve_noeta()?;
    let _ = root;
    let workdir =
        std::env::temp_dir().join(format!("noeta-aot-differential-{}", std::process::id()));
    for slot in 0..link_width() {
        std::fs::create_dir_all(workdir.join(format!("w{slot}")))
            .map_err(|e| format!("cannot create workdir: {e}"))?;
    }
    Ok(AotTools {
        cc,
        archive,
        libs,
        noeta,
        workdir,
    })
}

/// The `noeta` binary the truth side runs. `NOETA_BIN`, else the one beside this harness in the
/// target directory — the harness and the CLI are built by the same `cargo` invocation in the gate.
fn resolve_noeta() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("NOETA_BIN") {
        let path = PathBuf::from(path);
        return path
            .is_file()
            .then_some(path.clone())
            .ok_or_else(|| format!("NOETA_BIN points at {} which is not a file", path.display()));
    }
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("noeta")));
    match beside {
        Some(path) if path.is_file() => Ok(path),
        _ => Err(
            "the `noeta` binary was not found beside this harness — build it first:\n  \
             cargo build -p noeta-cli\n(or set NOETA_BIN=/path/to/noeta)"
                .to_string(),
        ),
    }
}

/// The per-side wall-clock cap ([`RUN_TIMEOUT`], or `NOETA_AOT_TIMEOUT_SECS`).
fn run_timeout() -> Duration {
    std::env::var("NOETA_AOT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(RUN_TIMEOUT)
}

/// The archive + its native link line: the caller's, or one built from this workspace.
fn resolve_archive() -> Result<(PathBuf, Vec<String>), String> {
    if let Ok(path) = std::env::var("NOETA_AOT_RUNTIME_LIB") {
        let libs = std::env::var("NOETA_AOT_LINK_LIBS")
            .ok()
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_else(default_native_libs);
        return Ok((PathBuf::from(path), libs));
    }
    // The full-ring (default-features) archive: the corpus imports across every stdlib ring, so the
    // oracle links the fully capable runtime rather than re-deriving `--native`'s per-program ring
    // footprint. `cargo rustc … --print native-static-libs` both builds it and prints the exact
    // system-library line the link needs — the same two facts the CLI's `resolve_aot_runtime` reads.
    // Debug, not release: the gate has already built this workspace's debug artifacts, and the
    // profile does not change the generated AOT bodies (they come from the object, not the archive).
    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()))
        .env("CARGO_TERM_COLOR", "never")
        .args([
            "rustc",
            "-p",
            "noeta-aot-runtime",
            "--",
            "--print",
            "native-static-libs",
        ])
        .output()
        .map_err(|e| format!("cannot run cargo to build the AOT runtime archive: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "building the AOT runtime archive failed:\n{}",
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
        .filter(|v: &Vec<String>| !v.is_empty())
        .unwrap_or_else(default_native_libs);
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target"));
    Ok((target.join("debug/libnoeta_aot.a"), libs))
}

/// A conservative link set for a Rust staticlib on Linux, when the exact note is unavailable.
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

/// How many artifacts to link at once. Capped at 8 by default rather than taken from
/// `available_parallelism`: each link pulls objects out of a several-hundred-megabyte archive, so
/// twenty concurrent `cc`s is a memory-pressure problem, not a speedup — and this repo runs several
/// agents at once. `NOETA_AOT_JOBS` overrides it.
fn link_width() -> usize {
    if let Some(n) = std::env::var("NOETA_AOT_JOBS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
    {
        return n;
    }
    std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(4)
}

/// A minimal `$PATH` probe (the dev harness has no dependency budget for a `which` crate).
fn which(name: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(name);
    if direct.is_absolute() {
        return direct.is_file().then_some(direct);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// One case's compiled inputs, produced on the main thread and linked/run on a worker.
struct Job {
    name: String,
    /// The AOT object file bytes (the program's native bodies + its dispatch table).
    object: Vec<u8>,
    /// The module's bundle: stapled onto the linked binary, and handed to `noeta run` as the truth.
    bundle: Vec<u8>,
}

/// What one side of the comparison produced.
#[derive(PartialEq, Eq, Clone, Debug, Default)]
struct Output {
    stdout: String,
    stderr: String,
    exit: Option<i32>,
    /// The signal that killed the process, when one did — the other half of what `exit: None` means.
    ///
    /// Without this the report could only say `ABORTED (exit None)`, which names the *absence* of
    /// the answer rather than the answer: on Unix `exit` is `None` precisely because a signal ended
    /// the process, and which signal is the whole diagnosis. A `--native` artifact dying on `SIGPIPE`
    /// (a broken pipe the runtime failed to disarm) and one dying on `SIGSEGV` (a miscompile) are
    /// the same `exit: None` and nothing alike. Always `None` off Unix, where `code()` is total.
    signal: Option<i32>,
}

/// How one side's run ended.
enum Ran {
    Done(Output),
    /// Outlived the cap: which side, so the report can say.
    TimedOut(&'static str),
    /// A harness-level failure (could not launch, could not link) — a real failure, not an exclusion.
    Broke(String),
}

/// Run every corpus program under `root` (optionally narrowed to one file) as a linked `--native`
/// artifact and compare against the interpreted run. `Err` is a setup problem (missing tools),
/// distinct from per-program divergences in the report.
pub fn run_aot_differential(root: &Path, only: Option<&Path>) -> Result<AotDiffReport, String> {
    crate::ensure_std_registry();
    let tools = discover_tools(root)?;
    let mut cases = Vec::new();
    collect_cases(root, &mut cases);
    cases.sort_by(|a, b| a.entry.cmp(&b.entry));

    let mut report = AotDiffReport {
        narrowed: only.is_some(),
        ..AotDiffReport::default()
    };
    // Linking and both runs are subprocesses, so they parallelize freely; compiling the module does
    // not (one salsa database per case, on this thread). So the main thread produces jobs in chunks
    // and a scoped pool consumes each chunk — bounded memory (one chunk of object files), and the
    // linker still runs `n` wide.
    let width = link_width();
    let mut chunk: Vec<Job> = Vec::with_capacity(width);
    for case in cases {
        if let Some(only) = only
            && case.entry != only
            && !case.entry.ends_with(only)
        {
            continue;
        }
        let name = case
            .entry
            .strip_prefix(root)
            .unwrap_or(&case.entry)
            .to_string_lossy()
            .into_owned();
        let module = if case.multi {
            match crate::read_case_workspace(&case.entry) {
                Ok(raw) => workspace_module(&raw, &case.entry, &mut report),
                Err(_) => {
                    report.not_run.read_failed += 1;
                    None
                }
            }
        } else {
            match std::fs::read_to_string(&case.entry) {
                Ok(text) => single_module(&name, &text, &mut report),
                Err(_) => {
                    report.not_run.read_failed += 1;
                    None
                }
            }
        };
        let Some(module) = module else { continue };
        if let Some(job) = prepare(&name, &module, &mut report) {
            chunk.push(job);
        }
        if chunk.len() == width {
            drain(&mut chunk, &tools, &mut report);
        }
    }
    drain(&mut chunk, &tools, &mut report);
    if keep_artifacts() {
        eprintln!("kept artifacts under {}", tools.workdir.display());
    } else {
        std::fs::remove_dir_all(&tools.workdir).ok();
    }
    Ok(report)
}

/// Compile one single-file program to a module, gating exactly like the bundle and wasm oracles.
fn single_module(
    name: &str,
    text: &str,
    report: &mut AotDiffReport,
) -> Option<noeta_bytecode::Module> {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, name, text);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);

    if noeta_diagnostics::has_errors(
        noeta_db::tokens(&db, src)
            .0
            .diagnostics
            .iter()
            .chain(noeta_db::ast(&db, src).0.diagnostics.iter()),
    ) {
        report.not_run.parse_failed += 1;
        return None;
    }
    if crate::has_error(&noeta_db::checked(&db, src).diagnostics) {
        // A checker-rejected program produces no artifact, so nothing is ever linked — an exclusion,
        // not a match. Its diagnostics are compile-time and the corpus harness asserts them.
        report.not_run.checker_rejected += 1;
        return None;
    }
    match &noeta_db::bytecode(&db, src).0 {
        Err(_) => {
            report.not_run.unsupported += 1;
            None
        }
        Ok(module) => Some(module.clone()),
    }
}

/// The workspace analogue of [`single_module`] for a multi-file fixture.
fn workspace_module(
    raw: &noeta_loader::RawWorkspace,
    entry: &Path,
    report: &mut AotDiffReport,
) -> Option<noeta_bytecode::Module> {
    let db = LangDatabase::default();
    // A case with package subdirectories is a dependency graph (see the JIT differential's twin of
    // this function for why the deps have to be synthesized rather than dropped).
    let deps = crate::dep_sources(entry, (raw.modules.len() + 1) as u32);
    let ws = if deps.is_empty() {
        noeta_db::workspace(
            &db,
            &raw.entry,
            &raw.modules,
            noeta_lexer::Edition::DEFAULT,
            &raw.paths,
        )
    } else {
        noeta_db::workspace_with_deps(
            &db,
            &raw.entry,
            &raw.modules,
            &deps,
            &noeta_span::PackageUses::new(),
            noeta_lexer::Edition::DEFAULT,
            &raw.paths,
        )
    };
    if noeta_db::linked(&db, ws).program.is_err() {
        report.not_run.link_failed += 1;
        return None;
    }
    if crate::has_error(&noeta_db::linked_checked(&db, ws).diagnostics) {
        report.not_run.checker_rejected += 1;
        return None;
    }
    match &noeta_db::linked_bytecode(&db, ws).0 {
        Err(_) => {
            report.not_run.unsupported += 1;
            None
        }
        Ok(module) => Some(module.clone()),
    }
}

/// AOT-compile `module` into a job, or fold the reason it cannot be built into `report`.
fn prepare(name: &str, module: &noeta_bytecode::Module, report: &mut AotDiffReport) -> Option<Job> {
    let t0 = std::time::Instant::now();
    let (object, natives) = match noeta_vm::compile_module_aot(module) {
        Ok(pair) => pair,
        Err(err) => {
            // The AOT compiler refusing a module the VM compiled is itself a finding, so it is a
            // failure rather than an exclusion.
            report.failures.push(AotDiffFailure {
                name: name.to_string(),
                stream: "build",
                detail: format!("AOT compile failed: {err}"),
            });
            return None;
        }
    };
    report.compile_ms += t0.elapsed().as_millis();
    report.native_protos += natives;
    Some(Job {
        name: name.to_string(),
        object,
        bundle: noeta_bundle::write(module),
    })
}

/// What one case's worker decided, plus the time it spent linking / running.
struct Verdict {
    name: String,
    outcome: Outcome,
    link_ms: u128,
    run_ms: u128,
    truth_ms: u128,
}

/// What a case turned out to be.
enum Outcome {
    Agreed,
    /// Same lines on both sides, different order, same exit code — the real scheduler, not the
    /// codegen. See [`reordered`].
    Reordered,
    /// Which stream disagreed, and how.
    Diverged(&'static str, String),
    /// Which side failed to reproduce itself.
    Nondeterministic(&'static str),
    /// A side fell over at the host level **once** and then ran clean [`ABORT_REPEATS`] times: which
    /// side, and what the one abort was. Excluded, but named and counted — see [`reproduce_abort`].
    AbortedOnce(&'static str, String),
    TimedOut(&'static str),
}

/// Link and run one chunk of jobs in parallel, folding every outcome into `report`.
fn drain(chunk: &mut Vec<Job>, tools: &AotTools, report: &mut AotDiffReport) {
    if chunk.is_empty() {
        return;
    }
    let jobs = std::mem::take(chunk);
    let verdicts: Vec<Verdict> = std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .iter()
            .enumerate()
            .map(|(slot, job)| scope.spawn(move || judge(slot, job, tools)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("an AOT worker panicked"))
            .collect()
    });
    for v in verdicts {
        report.link_ms += v.link_ms;
        report.run_ms += v.run_ms;
        report.truth_ms += v.truth_ms;
        match v.outcome {
            Outcome::Agreed => report.matched += 1,
            Outcome::Reordered => report.reordered.push(v.name),
            Outcome::Diverged(stream, detail) => report.failures.push(AotDiffFailure {
                name: v.name,
                stream,
                detail,
            }),
            Outcome::Nondeterministic(side) => {
                report.nondeterministic.push(format!("{} ({side})", v.name))
            }
            Outcome::AbortedOnce(side, detail) => report
                .aborted_once
                .push(format!("{} ({side} side) — {detail}", v.name)),
            Outcome::TimedOut(side) => report.timed_out.push(format!("{} ({side} side)", v.name)),
        }
    }
}

/// Build one artifact the way `noeta build --native` does — link the AOT object against the runtime
/// archive, staple the bundle on, mark it executable — then run it and `noeta run` the same bundle,
/// and say which of the four things happened.
fn judge(slot: usize, job: &Job, tools: &AotTools) -> Verdict {
    let dir = tools.workdir.join(format!("w{slot}"));
    let obj = dir.join("program.o");
    let linked = dir.join("linked");
    let app = dir.join("app");
    // A copy of the bundle beside the artifact, under the name both sides run it as — kept only so
    // that `NOETA_AOT_KEEP` leaves behind everything needed to reproduce a divergence by hand. Each
    // run gets its own fresh copy in its own directory (see `run_truth`).
    let bundle = dir.join(AOT_SOURCE);

    let t0 = std::time::Instant::now();
    let built = build_artifact(job, tools, &obj, &linked, &app, &bundle);
    let link_ms = t0.elapsed().as_millis();
    if let Err(detail) = built {
        return Verdict {
            name: job.name.clone(),
            outcome: Outcome::Diverged("build", detail),
            link_ms,
            run_ms: 0,
            truth_ms: 0,
        };
    }

    let t1 = std::time::Instant::now();
    let native = run_native(&dir, &app);
    let run_ms = t1.elapsed().as_millis();
    let t2 = std::time::Instant::now();
    let truth = run_truth(&dir, tools, &job.bundle);
    let mut truth_ms = t2.elapsed().as_millis();

    let outcome = match (native, truth) {
        (Ran::TimedOut(side), _) | (_, Ran::TimedOut(side)) => Outcome::TimedOut(side),
        (Ran::Broke(detail), _) | (_, Ran::Broke(detail)) => Outcome::Diverged("build", detail),
        (Ran::Done(native), Ran::Done(truth)) => {
            if native == truth {
                Outcome::Agreed
            } else if let Some(side) = crashed(&native, &truth) {
                // A host-level abort still cannot go through the ORDINARY reproducibility dance: a
                // Rust panic prints an ASLR-varying address, so a crashing artifact looks exactly
                // like a program that does not reproduce itself, and asking "did it print the same
                // bytes twice" would file a live crash as an exclusion. That is why this arm exists
                // and why it is checked first.
                //
                // But "not that question" was read as "no question", and an abort was called a
                // divergence off a single sample while a mere output disagreement got four. That is
                // the wrong way round, and it made the gate load-sensitive: this is the arm a
                // load-induced abort lands in, so a machine under a parallel build could turn the
                // release gate red with nothing wrong. A gate that goes red under load is a gate
                // people re-run instead of read, which is the failure mode the whole discipline
                // exists to prevent.
                //
                // So the abort is held to the same reproducibility discipline, asking the question
                // an abort can actually answer — "does it abort AGAIN", not "does it print the same
                // bytes" — and it is asked more times, not fewer. A reproducible abort is a
                // divergence exactly as loudly as before.
                let fell_over = if side == "native" { &native } else { &truth };
                let verdict = if side == "native" {
                    reproduce_abort(native_probe(&dir, &app))
                } else {
                    reproduce_abort(truth_ms_probe(&dir, tools, &job.bundle))
                };
                match verdict {
                    Abort::Reproduced => Outcome::Diverged(
                        "crash",
                        format!(
                            "the {side} side ABORTED and did it again on re-run — {}",
                            abort_detail(fell_over)
                        ),
                    ),
                    Abort::NotReproduced => Outcome::AbortedOnce(
                        side,
                        format!(
                            "{} — clean on all {ABORT_REPEATS} re-runs",
                            abort_detail(fell_over)
                        ),
                    ),
                    Abort::TimedOut(side) => Outcome::TimedOut(side),
                    Abort::Broke(detail) => Outcome::Diverged("build", detail),
                }
            } else {
                // Only a disagreeing case pays for the reproducibility check, and it asks the
                // question of BOTH sides: a real host has a real clock, a real RNG and a real
                // scheduler, so "these two engines printed different things" is only evidence of an
                // AOT bug once each engine has been shown to reproduce itself. Repeats, not one
                // re-run: a race biased towards one outcome needs more than two samples to show
                // itself (`async/race.noe` sleeps 1/2/3 ms and picks a winner by whichever real
                // sleep lands first).
                let t3 = std::time::Instant::now();
                let steady_truth = repeats(truth_ms_probe(&dir, tools, &job.bundle), &truth);
                truth_ms += t3.elapsed().as_millis();
                let steady_native = repeats(native_probe(&dir, &app), &native);
                match (steady_truth, steady_native) {
                    (Steady::Yes, Steady::Yes) if reordered(&truth, &native) => Outcome::Reordered,
                    (Steady::Yes, Steady::Yes) => {
                        let (stream, detail) = describe(&truth, &native);
                        Outcome::Diverged(stream, detail)
                    }
                    (Steady::Broke(detail), _) | (_, Steady::Broke(detail)) => {
                        Outcome::Diverged("build", detail)
                    }
                    (Steady::TimedOut(side), _) | (_, Steady::TimedOut(side)) => {
                        Outcome::TimedOut(side)
                    }
                    (Steady::No, Steady::No) => Outcome::Nondeterministic("both sides"),
                    (Steady::No, _) => Outcome::Nondeterministic("truth side"),
                    (_, Steady::No) => Outcome::Nondeterministic("native side"),
                }
            }
        }
    };
    // A case that did not simply agree is a case someone will want to re-run by hand, and the
    // artifact is the expensive part to reproduce. `NOETA_AOT_KEEP=1` leaves it (and the object, and
    // the bundle) on disk, named after the case, instead of deleting it.
    if matches!(outcome, Outcome::Agreed) || !keep_artifacts() {
        for path in [&obj, &linked, &app, &bundle] {
            let _ = std::fs::remove_file(path);
        }
    } else {
        let slug = job.name.replace(['/', '.'], "_");
        let kept = dir.join(&slug);
        let _ = std::fs::create_dir_all(&kept);
        let _ = std::fs::rename(&app, kept.join("app"));
        let _ = std::fs::rename(&bundle, kept.join(AOT_SOURCE));
        let _ = std::fs::remove_file(&obj);
        let _ = std::fs::remove_file(&linked);
        eprintln!("kept the artifact for {}: {}", job.name, kept.display());
    }
    Verdict {
        name: job.name.clone(),
        outcome,
        link_ms,
        run_ms,
        truth_ms,
    }
}

/// The three steps `emit_native` takes after the AOT object exists: `cc`-link against the runtime
/// archive, staple the bundle onto the linked binary, mark it executable.
fn build_artifact(
    job: &Job,
    tools: &AotTools,
    obj: &Path,
    linked: &Path,
    app: &Path,
    bundle: &Path,
) -> Result<(), String> {
    std::fs::write(obj, &job.object).map_err(|e| format!("cannot write the AOT object: {e}"))?;
    std::fs::write(bundle, &job.bundle).map_err(|e| format!("cannot write the bundle: {e}"))?;
    let out = Command::new(&tools.cc)
        .arg("-s")
        .arg(obj)
        .arg(&tools.archive)
        .args(&tools.libs)
        .arg("-o")
        .arg(linked)
        .output()
        .map_err(|e| format!("cannot run the linker `{}`: {e}", tools.cc))?;
    if !out.status.success() {
        return Err(format!(
            "link failed ({}):\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let runtime =
        std::fs::read(linked).map_err(|e| format!("cannot read the linked binary: {e}"))?;
    let image = noeta_bundle::staple(&runtime, &job.bundle);
    std::fs::write(app, &image).map_err(|e| format!("cannot write the artifact: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(app, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("cannot mark the artifact executable: {e}"))?;
    }
    Ok(())
}

/// How many times a disagreeing case is re-run per side before its output is called stable.
const STABILITY_REPEATS: usize = 3;

/// What a repeated-run probe concluded.
enum Steady {
    /// Every repeat produced the first run's output.
    Yes,
    /// A repeat differed — the side does not reproduce itself.
    No,
    TimedOut(&'static str),
    Broke(String),
}

/// Re-run one side [`STABILITY_REPEATS`] times and report whether it always reproduced `first`.
fn repeats(mut probe: impl FnMut() -> Ran, first: &Output) -> Steady {
    for _ in 0..STABILITY_REPEATS {
        match probe() {
            Ran::Done(again) if &again == first => {}
            Ran::Done(_) => return Steady::No,
            Ran::TimedOut(side) => return Steady::TimedOut(side),
            Ran::Broke(detail) => return Steady::Broke(detail),
        }
    }
    Steady::Yes
}

/// A closure that runs the truth side once more, in a fresh directory each time.
fn truth_ms_probe<'a>(dir: &'a Path, tools: &'a AotTools, bundle: &'a [u8]) -> impl FnMut() -> Ran {
    move || run_truth(dir, tools, bundle)
}

/// A closure that runs the artifact once more, in a fresh directory each time.
fn native_probe<'a>(dir: &'a Path, app: &'a Path) -> impl FnMut() -> Ran {
    move || run_native(dir, app)
}

/// Run the artifact in a **fresh** working directory, with `argv[0]` set to the synthetic source
/// name.
///
/// Both details are the harness refusing to manufacture its own divergences. A program with
/// filesystem side effects (`async/dir_async.noe` creates a directory and then asks whether it
/// exists) would otherwise see whatever the other side left behind — the second run reads the first
/// run's world. And `std.os.args` includes the program path, which is the artifact's path on one
/// side and the bundle's on the other; `arg0` makes both `<aot>`, which is what they *mean*.
fn run_native(dir: &Path, app: &Path) -> Ran {
    let run = match fresh_dir(dir, "native") {
        Ok(run) => run,
        Err(e) => return Ran::Broke(e),
    };
    // The artifact runs from a hard link named `<aot>` inside its own run directory, mirroring the
    // truth side's bundle exactly: a program that LISTS its working directory (`std/fs.noe`,
    // `async/fs_metadata_async.noe`) must see the same entries on both sides, and it saw the truth
    // side's `<aot>` and nothing on the native side. A link, not a copy — the artifact is tens of
    // megabytes and this is per case.
    let linked = run.join(AOT_SOURCE);
    if std::fs::hard_link(app, &linked).is_err() && std::fs::copy(app, &linked).is_err() {
        return Ran::Broke("cannot place the artifact in its run directory".to_string());
    }
    let mut cmd = Command::new(&linked);
    cmd.current_dir(&run);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.arg0(AOT_SOURCE);
    }
    let out = run_capped(&mut cmd, "native");
    let _ = std::fs::remove_dir_all(&run);
    out
}

/// Run `noeta run <aot>` over the same module, in a fresh working directory holding a copy of the
/// bundle under that name — so the CLI's synthetic source is named `<aot>`, exactly as the AOT
/// runtime names its own.
fn run_truth(dir: &Path, tools: &AotTools, bundle: &[u8]) -> Ran {
    let run = match fresh_dir(dir, "truth") {
        Ok(run) => run,
        Err(e) => return Ran::Broke(e),
    };
    if let Err(e) = std::fs::write(run.join(AOT_SOURCE), bundle) {
        return Ran::Broke(format!("cannot write the bundle: {e}"));
    }
    let out = run_capped(
        Command::new(&tools.noeta)
            .arg("run")
            .arg(AOT_SOURCE)
            .current_dir(&run),
        "truth",
    );
    let _ = std::fs::remove_dir_all(&run);
    out
}

/// Whether one side fell over at the *host* level: killed by a signal (no exit code), an
/// abort-shaped status, or a Rust panic in its stderr. A Noeta `panic(...)` is none of these — it is
/// a rendered `panic: …` diagnostic and an ordinary exit code — so this catches only the artifact
/// itself failing, which is never a legitimate outcome.
fn aborted(o: &Output) -> bool {
    o.exit.is_none_or(|c| c >= 128) || o.stderr.contains("panicked at")
}

/// Which side, if either, aborted — the native side first, since it is the one under test.
fn crashed(native: &Output, truth: &Output) -> Option<&'static str> {
    if aborted(native) {
        Some("native")
    } else if aborted(truth) {
        Some("truth")
    } else {
        None
    }
}

/// How many times an aborting side is re-run before its abort is called a one-off.
///
/// Deliberately more than the [`STABILITY_REPEATS`] an output disagreement pays for. A disagreement
/// only has to show that two engines differ; an abort has to answer a question with a much worse
/// wrong answer available in each direction — call a real miscompile a fluke, or call the gate red
/// when nothing is wrong — so it buys more samples.
const ABORT_REPEATS: usize = 5;

/// What re-running an aborting side concluded.
#[derive(Debug, PartialEq, Eq)]
enum Abort {
    /// It aborted again. The artifact really does fall over: a divergence, loudly.
    Reproduced,
    /// Every re-run finished without aborting.
    NotReproduced,
    TimedOut(&'static str),
    Broke(String),
}

/// Re-run an aborting side and say whether the abort reproduces.
///
/// **Any** abort across the re-runs counts as reproduced — not "most", not "all". The asymmetry is
/// the whole point and it is the direction that cannot lose a bug: an abort that happens one run in
/// six is still a `--native` artifact falling over, and the only thing an intermittent miscompile
/// looks like is exactly this. What the re-runs can rule out is the *opposite* error — a single
/// abort caused by something that is not the codegen at all — and ruling that out is what this is
/// for. It takes [`ABORT_REPEATS`] consecutive clean runs to earn an exclusion, and the exclusion is
/// still named and counted in the report.
///
/// The stated limit, rather than a hidden one: an abort rarer than one in six runs can be excluded
/// here. Nothing has ever looked like that — the one real abort this oracle has caught (a misaligned
/// AOT dispatch table indexing out of bounds, `modules/derived_package_path`) was deterministic, and
/// so is every miscompile shape anyone has proposed for it. The exclusion is loud precisely so that
/// a case which keeps appearing in it gets read rather than accumulated.
fn reproduce_abort(mut probe: impl FnMut() -> Ran) -> Abort {
    for _ in 0..ABORT_REPEATS {
        match probe() {
            Ran::Done(again) if aborted(&again) => return Abort::Reproduced,
            Ran::Done(_) => {}
            Ran::TimedOut(side) => return Abort::TimedOut(side),
            Ran::Broke(detail) => return Abort::Broke(detail),
        }
    }
    Abort::NotReproduced
}

/// Say what actually happened to a side that fell over.
///
/// `exit: None` and an empty detail — which is all this used to print — names the absence of the
/// answer, not the answer. It cost a reader a whole investigation to work out that `exit None` meant
/// "killed by a signal" and then which signal it was, and the signal *was* the diagnosis. So: the
/// signal by number and name where there is one, the status where there is not, and an explicit "no
/// output captured" rather than a colon with nothing after it.
fn abort_detail(o: &Output) -> String {
    let how = match (o.signal, o.exit) {
        (Some(sig), _) => format!("killed by signal {sig} ({})", signal_name(sig)),
        (None, Some(code)) if code >= 128 => {
            format!("exit {code} (an abort-shaped status: {} + 128)", code - 128)
        }
        (None, Some(code)) => format!("exit {code}"),
        (None, None) => "no exit code and no signal".to_string(),
    };
    let said = first_lines(&o.stderr);
    if said.is_empty() {
        format!("{how}, no output captured on stderr")
    } else {
        format!("{how}: {said}")
    }
}

/// The POSIX name of a signal number — the ones a native artifact can plausibly die on, so a report
/// reads as a diagnosis rather than a number to go and look up.
fn signal_name(sig: i32) -> &'static str {
    match sig {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        6 => "SIGABRT",
        8 => "SIGFPE",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        24 => "SIGXCPU",
        25 => "SIGXFSZ",
        _ => "unknown",
    }
}

/// Whether the two sides printed the **same lines in a different order**, with the same exit code.
///
/// This is the real scheduler, not the codegen. The corpus pins interleavings that are deterministic
/// under the *sandbox* clock — `async/nested_concurrent.noe` spells out why a 1 ms sleeper finishes
/// before a 3 ms one — and both of these sides run on a real one, where those two sleeps can land in
/// either order on a loaded box. Both engines reproduce themselves; they just made different (equally
/// legal) scheduling choices, so calling it an AOT divergence would be a lie.
///
/// The known limit, stated rather than hidden: a real codegen bug that permutes output without
/// changing it would land here too. Nothing in the corpus has ever looked like that, and the
/// alternative — a hand-maintained list of "concurrent cases" — is the rot this audit is about.
fn reordered(truth: &Output, native: &Output) -> bool {
    let same_multiset = |a: &str, b: &str| {
        let mut a: Vec<&str> = a.lines().collect();
        let mut b: Vec<&str> = b.lines().collect();
        a.sort_unstable();
        b.sort_unstable();
        a == b
    };
    truth.exit == native.exit
        && truth.stdout != native.stdout
        && same_multiset(&truth.stdout, &native.stdout)
        && same_multiset(&truth.stderr, &native.stderr)
}

/// The first few lines of a stderr blob — enough to identify a panic without pasting a backtrace
/// into the report.
fn first_lines(stderr: &str) -> String {
    stderr
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Whether to keep a non-agreeing case's artifact on disk (`NOETA_AOT_KEEP`), and with it the
/// workdir — a divergence is only actionable if you can re-run the binary that produced it.
fn keep_artifacts() -> bool {
    std::env::var_os("NOETA_AOT_KEEP").is_some()
}

/// An empty directory for one run of one side, removed and recreated so nothing survives between
/// runs.
fn fresh_dir(dir: &Path, side: &str) -> Result<PathBuf, String> {
    let run = dir.join(side);
    let _ = std::fs::remove_dir_all(&run);
    std::fs::create_dir_all(&run)
        .map(|()| run)
        .map_err(|e| format!("cannot create the {side} run directory: {e}"))
}

/// Run `cmd` with its output captured and a wall-clock cap. The pipes are drained by two threads
/// while the parent polls, so a program that outruns the pipe buffer cannot deadlock the harness —
/// the failure mode a naive `wait_with_output()` + timeout has.
fn run_capped(cmd: &mut Command, side: &'static str) -> Ran {
    use std::io::Read as _;
    use std::process::Stdio;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // `ETXTBSY` is a multi-threaded fork/exec race, not a property of the program: another worker's
    // child, forked while this artifact's file descriptor was still open for writing, holds it open
    // until its own `exec`. The window is microseconds and the retry is the standard answer (cargo
    // carries the same one). Retrying is safe because nothing has run yet.
    let mut child = None;
    for attempt in 0..20 {
        match cmd.spawn() {
            Ok(spawned) => {
                child = Some(spawned);
                break;
            }
            Err(e) if e.raw_os_error() == Some(26) => {
                std::thread::sleep(Duration::from_millis(10 * (attempt + 1)));
            }
            Err(e) => return Ran::Broke(format!("the {side} side failed to launch: {e}")),
        }
    }
    let Some(mut child) = child else {
        return Ran::Broke(format!(
            "the {side} side failed to launch: ETXTBSY after 20 retries"
        ));
    };
    let mut out_pipe = child.stdout.take().expect("stdout was piped");
    let mut err_pipe = child.stderr.take().expect("stderr was piped");
    let deadline = std::time::Instant::now() + run_timeout();
    std::thread::scope(|scope| {
        let out = scope.spawn(move || {
            let mut buf = Vec::new();
            let _ = out_pipe.read_to_end(&mut buf);
            buf
        });
        let err = scope.spawn(move || {
            let mut buf = Vec::new();
            let _ = err_pipe.read_to_end(&mut buf);
            buf
        });
        let status = loop {
            match child.try_wait() {
                Err(e) => {
                    return Ran::Broke(format!("the {side} side could not be waited on: {e}"));
                }
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        // The reader threads end when the pipes close with the process.
                        let _ = out.join();
                        let _ = err.join();
                        return Ran::TimedOut(side);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        };
        let stdout = String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
        let stderr = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt as _;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;
        Ran::Done(Output {
            stdout,
            stderr,
            exit: status.code(),
            signal,
        })
    })
}

/// Name the first stream on which the two sides differ, and describe how.
fn describe(truth: &Output, native: &Output) -> (&'static str, String) {
    if truth.stdout != native.stdout {
        (
            "stdout",
            format!(
                "stdout: `noeta run` {:?}, --native {:?}",
                truth.stdout, native.stdout
            ),
        )
    } else if truth.stderr != native.stderr {
        (
            "stderr",
            format!(
                "stderr: `noeta run` {:?}, --native {:?}",
                truth.stderr, native.stderr
            ),
        )
    } else {
        (
            "exit",
            format!(
                "exit: `noeta run` {:?}, --native {:?}",
                truth.exit, native.exit
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-row expect-fail list to pose the ratchet's rules against. The rules are tested over
    /// *this*, never over [`KNOWN_DIVERGENCES`]: the list's whole purpose is to empty itself, and a
    /// test derived from it stops asserting anything the moment it does.
    const LISTED: &[(&str, &str)] = &[("some/known_a.noe", "stdout"), ("some/known_b.noe", "exit")];

    /// A report judged against [`LISTED`], carrying exactly the failures `failures` describes.
    fn report_with(failures: &[(&str, &'static str)]) -> AotDiffReport {
        AotDiffReport {
            failures: failures
                .iter()
                .map(|(name, stream)| AotDiffFailure {
                    name: (*name).to_string(),
                    stream,
                    detail: String::new(),
                })
                .collect(),
            listed: LISTED,
            ..AotDiffReport::default()
        }
    }

    /// A real run is judged against the real list — the wiring, so posing [`LISTED`] to the rules
    /// below can never mean the shipped oracle consults something else.
    #[test]
    fn a_default_report_is_judged_against_the_shipped_list() {
        assert!(std::ptr::eq(
            AotDiffReport::default().listed,
            KNOWN_DIVERGENCES
        ));
    }

    /// The ratchet's happy state: every known divergence reproduced, nothing else diverged.
    #[test]
    fn reproducing_exactly_the_known_divergences_is_green() {
        let report = report_with(LISTED);
        assert!(report.unexpected().is_empty());
        assert!(report.stale_gaps().is_empty());
        assert!(report.ok(), "{}", report.to_human());
    }

    /// A NEW divergence fails, and names itself.
    #[test]
    fn an_unlisted_divergence_fails() {
        let mut rows = LISTED.to_vec();
        rows.push(("some/new_case.noe", "stdout"));
        let report = report_with(&rows);
        assert_eq!(report.unexpected().len(), 1);
        assert!(!report.ok());
        assert!(report.to_human().contains("some/new_case.noe"));
    }

    /// And the half that makes the list self-retiring: a known divergence that stops reproducing —
    /// because someone fixed it, which is the point — fails too, so the row cannot be left behind.
    #[test]
    fn a_fixed_known_divergence_fails_until_its_row_is_deleted() {
        let rows: Vec<_> = LISTED.iter().skip(1).copied().collect();
        let report = report_with(&rows);
        assert!(report.unexpected().is_empty());
        assert_eq!(report.stale_gaps().len(), 1);
        assert!(!report.ok());
        assert!(report.to_human().contains("did not reproduce"));
    }

    /// A narrowed (`--file`) run cannot speak for the rows it did not run.
    #[test]
    fn a_narrowed_run_does_not_judge_rows_it_never_ran() {
        let report = AotDiffReport {
            narrowed: true,
            ..report_with(&[])
        };
        assert!(report.stale_gaps().is_empty());
        assert!(report.ok());
    }

    /// A permutation of the same lines is a scheduling difference; a different line is not.
    #[test]
    fn reordering_is_recognised_and_a_real_difference_is_not() {
        let out = |stdout: &str| Output {
            stdout: stdout.to_string(),
            exit: Some(0),
            ..Output::default()
        };
        assert!(reordered(&out("a\nb\nc\n"), &out("a\nc\nb\n")));
        assert!(!reordered(&out("a\nb\nc\n"), &out("a\nb\nd\n")));
        // Same order is not "reordered" — it is agreement, and callers check that first.
        assert!(!reordered(&out("a\nb\n"), &out("a\nb\n")));
        // A differing exit code is never just an interleaving.
        let mut fewer = out("b\na\n");
        fewer.exit = Some(1);
        assert!(!reordered(&out("a\nb\n"), &fewer));
    }

    /// A host-level abort is a crash, not a program result — including one that only shows up as a
    /// Rust panic on stderr with an otherwise ordinary exit code.
    #[test]
    fn a_rust_panic_or_a_signal_is_a_crash() {
        assert_eq!(crashed(&panicked(), &clean()), Some("native"));
        assert_eq!(crashed(&clean(), &signalled(13)), Some("truth"));
        assert_eq!(crashed(&clean(), &clean()), None);
        // A Noeta `panic(...)` is a rendered diagnostic and an ordinary exit — not a crash.
        let noeta_panic = Output {
            stdout: "before\n".to_string(),
            stderr: "panic: boom\n".to_string(),
            exit: Some(1),
            signal: None,
        };
        assert_eq!(crashed(&noeta_panic, &clean()), None);
    }

    /// A side that finished normally.
    fn clean() -> Output {
        Output {
            exit: Some(1),
            ..Output::default()
        }
    }

    /// A side killed by `sig`, which is what `exit: None` means on Unix.
    fn signalled(sig: i32) -> Output {
        Output {
            exit: None,
            signal: Some(sig),
            ..Output::default()
        }
    }

    /// A side that died with a Rust panic on stderr.
    fn panicked() -> Output {
        Output {
            stderr: "thread '<unnamed>' panicked at src/dispatch.rs:439:37".to_string(),
            exit: Some(101),
            ..Output::default()
        }
    }

    /// `n` completed runs that all produced `what()`.
    fn repeated(n: usize, what: fn() -> Output) -> Vec<Ran> {
        (0..n).map(|_| Ran::Done(what())).collect()
    }

    /// A probe that replays a scripted sequence of runs, so the abort re-run policy can be posed
    /// exact histories instead of a real process.
    fn scripted(runs: Vec<Ran>) -> impl FnMut() -> Ran {
        let mut runs = runs.into_iter();
        move || {
            runs.next()
                .expect("the policy asked for more runs than were scripted")
        }
    }

    /// The rule the `derived_package_path` miscompile was caught by, and the one that must survive
    /// every leniency added here: an abort that happens again is a divergence.
    #[test]
    fn an_abort_that_reproduces_is_still_a_divergence() {
        // Every re-run aborts — the deterministic miscompile shape.
        assert_eq!(
            reproduce_abort(scripted(repeated(ABORT_REPEATS, panicked))),
            Abort::Reproduced
        );
        // And so is one that aborts only on the LAST re-run: any repeat, not most of them. An
        // intermittent miscompile is still a miscompile, and this is the direction that cannot
        // lose one.
        let mut runs = repeated(ABORT_REPEATS - 1, clean);
        runs.push(Ran::Done(signalled(11)));
        assert_eq!(reproduce_abort(scripted(runs)), Abort::Reproduced);
    }

    /// The load-flake this arm used to report as a release-blocking divergence off ONE sample.
    #[test]
    fn an_abort_that_never_reproduces_is_excluded_not_failed() {
        assert_eq!(
            reproduce_abort(scripted(repeated(ABORT_REPEATS, clean))),
            Abort::NotReproduced
        );
    }

    /// It takes the full run of clean re-runs to earn the exclusion — not a first clean one.
    #[test]
    fn one_clean_re_run_does_not_clear_an_abort() {
        const { assert!(ABORT_REPEATS > 1, "a single re-run cannot clear an abort") };
        let mut runs = repeated(1, clean);
        runs.push(Ran::Done(panicked()));
        assert_eq!(reproduce_abort(scripted(runs)), Abort::Reproduced);
    }

    /// An abort is judged with MORE evidence than an output disagreement, not less — the inversion
    /// that made this the load-sensitive arm of the gate.
    ///
    /// A `const` block, so the inversion cannot even be *built*, let alone reach a test run. Clippy
    /// asked for this and it is the better shape: a guard over two constants has no business waiting
    /// for a test binary to execute.
    #[test]
    fn an_abort_buys_more_samples_than_a_disagreement() {
        const { assert!(ABORT_REPEATS >= STABILITY_REPEATS) };
    }

    /// `exit None` and an empty detail names the absence of the answer. The report says which
    /// signal, and says so when there was no output rather than trailing off after a colon.
    #[test]
    fn an_abort_reports_the_signal_and_admits_to_saying_nothing() {
        let detail = abort_detail(&signalled(13));
        assert!(detail.contains("signal 13"), "{detail}");
        assert!(detail.contains("SIGPIPE"), "{detail}");
        assert!(detail.contains("no output captured"), "{detail}");
        assert!(abort_detail(&signalled(11)).contains("SIGSEGV"));
        // A panic keeps its message, which is how the one real abort found so far identified itself.
        let said = abort_detail(&panicked());
        assert!(said.contains("panicked at"), "{said}");
        assert!(!said.contains("no output captured"), "{said}");
        // An abort-shaped status with no signal still says what the status was.
        let shaped = Output {
            exit: Some(134),
            ..Output::default()
        };
        assert!(abort_detail(&shaped).contains("134"));
    }
}
