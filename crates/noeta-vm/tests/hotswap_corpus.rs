//! The **corpus swap differential**: the conformance corpus, run as hot-swap oracle cases.
//!
//! `hotswap.rs` holds the hand-written differential — 40-odd programs someone thought to write.
//! Every hot-swap bug this month lived in the gap that corpus leaves: a swapped `@html` lowered to
//! a panic, `x is Uuid` silently answered `false`, a swapped `async` body touching a module global
//! panicked, `i8` `100 * 3` gave `300` where a cold start wraps to `44`. The shape is always the
//! same — the swap fragment is compiled **alone**, so whatever whole-program context the body
//! needed (a tier table, an import's aliases, the module globals, the checker's sites) was simply
//! not there, and the miss is silent.
//!
//! So instead of writing more cases by hand, derive them: `tests/conformance/**/*.noe` is a
//! thousand real programs that already carry tiers, imports, globals, generics, packed types,
//! reflection and destructors. For each one, mechanically produce a v2 and run it through the same
//! oracle the hand-written corpus uses —
//!
//! > swap(v1 → v2) then probe must be observationally identical to cold-starting v2 then probing.
//!
//! Four generators, because the differ has distinct code paths and one generator exercises one:
//!
//! * [`Generator::CloneAppend`] — clone an eligible top-level `fn` under a fresh name and append
//!   it. The differ reports it `added`; the fragment holds a body written against *this program's*
//!   types, imports, tier declarations and globals, which is exactly the context a fragment does
//!   not carry. Probe by calling the clone.
//! * [`Generator::CloneChanged`] — the same clone, but present in v1 as a one-line delegate and in
//!   v2 as the real body. The differ reports it `changed`: the *body-edit* path, which is what a
//!   developer's save actually is.
//! * [`Generator::RerunTopLevel`] — append a top-level statement, which makes the swap
//!   **re-running**: the whole top level re-executes in the live session with reactive anchors
//!   withheld. Compared against a cold start's own top-level output. This is the broad one — it
//!   needs nothing of a program but that it be green, so it reaches the ~two thirds of the corpus
//!   that declares no top-level `fn` at all.
//! * [`Generator::RerunWithBodyEdit`] — a re-running swap whose fragment *also* carries a changed
//!   function body. It exists because the clone generators can only probe a zero-arity function and
//!   most corpus functions take arguments: here the program's own re-running top level supplies the
//!   call, so a body of any arity gets fragment-compiled and then actually executed. Every
//!   divergence but one was found by this generator.
//!
//! **Coverage is reported, never capped silently.** Every program lands in exactly one bucket per
//! generator — exercised, or skipped with a machine-assigned reason — and the tally prints, against
//! two denominators: every corpus program, and the *green* ones (a corpus that is a third negative
//! cases would otherwise read as a harness that covers a third of nothing). A harness that quietly
//! covers 40% reads exactly like one that covers everything.
//!
//! **Nondeterminism is excluded by measurement, not by a denylist.** Every case runs its cold arm
//! twice; a program whose two cold runs disagree (clock, entropy, ports, address-shaped output) is
//! skipped as nondeterministic before its swap arm is ever compared. As it turns out the corpus is
//! deterministic throughout — every case pins an exact stdout, so it has to be — and the detector
//! excludes nothing. It stays in as the guard for corpus programs not yet written.
//!
//! Runtime: well under a minute for all 1098 programs, fanned out over [`WORKERS`] threads — the
//! sequential sweep is ~96s, the fanned-out one ~37s on 20 cores. That is ordinary-test
//! range, so it is an ordinary test — no `#[ignore]`, no flag to remember, no coverage that only
//! exists when someone opts in.
//!
//! What it found is in [`KNOWN_DIVERGENCES`], and that list is now **empty**. It reported nine real
//! hot-swap defects in three families when it landed — a fragment-compiled body skipping a
//! destructor, a swapped declaration losing the `@role` its unchanged attribute confers, and the
//! attribute manifest reordering. All nine are fixed. The list stays, and stays enforced in both
//! directions, because an empty list is the state worth defending: the next divergence fails the
//! build rather than joining a backlog.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use noeta_ast::{FnDecl, Program, Stmt};
use noeta_compiler::hotswap::SwapDiff;
use noeta_span::{Source, SourceId};
use noeta_vm::VmSession;

/// The corpus root, relative to this crate.
const CORPUS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/conformance");

/// How long one case (all generators) may take before it is skipped as a timeout. Generous:
/// the point is to survive a program that blocks on a socket, not to police slow ones.
const CASE_TIMEOUT: Duration = Duration::from_secs(25);

// ------------------------------------------------------------------ front end (mirrors the corpus runner)

/// Lex + parse `src` the way the conformance runner does — seeded with the installed extensions'
/// verbatim-body tiers and then re-lexed with the program's own `@tier(…, text/expr)` declarations,
/// so a corpus program with a text tier parses here as it does there.
fn parse(src: &str) -> (Program, bool) {
    let source = Source::new(SourceId::FIRST, "<corpus>", src);
    let mut tier_names: Vec<String> = noeta_stdlib::registry::ext_verbatim_tier_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    let lexed = noeta_lexer::lex_in(
        &source,
        noeta_lexer::Edition::DEFAULT,
        &noeta_lexer::TextTiers::with(tier_names.clone()),
    );
    tier_names.extend(lexed.text_tier_decls.iter().cloned());
    let tiers = noeta_lexer::TextTiers::with(tier_names);
    let lexed = noeta_lexer::lex_in(&source, noeta_lexer::Edition::DEFAULT, &tiers);
    let parsed = noeta_parser::parse_in(
        &source,
        &lexed.tokens,
        noeta_lexer::Edition::DEFAULT,
        &tiers,
    );
    let clean = !noeta_diagnostics::has_errors(&lexed.diagnostics)
        && !noeta_diagnostics::has_errors(&parsed.diagnostics);
    (parsed.program, clean)
}

fn factory() -> noeta_vm::HostFactory {
    Box::new(|| {
        (
            Box::new(noeta_stdlib::SandboxHost::new()),
            Box::new(noeta_stdlib::SandboxExecutor::new()),
        )
    })
}

/// What a session entry actually did — the unit the oracle compares. Stdout is the spine; stderr,
/// the abort traceback and the diagnostics are folded in because *the swap failing where the cold
/// start succeeds* is the exact shape three of the four known bugs took.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Observation {
    stdout: String,
    stderr: String,
    trace: Vec<String>,
    diagnostics: Vec<String>,
}

impl Observation {
    fn of(out: &noeta_vm::SessionOutput) -> Observation {
        Observation {
            stdout: out.stdout.clone(),
            stderr: out.stderr.clone(),
            // Frame **names** only. A frame's span is a byte offset into the version that compiled
            // it, and a generated v2 that inserts text shifts every later offset — comparing spans
            // would report a divergence for every program that legitimately aborts.
            trace: out
                .trace
                .iter()
                .map(|f| f.name.clone().unwrap_or_else(|| "<anon>".into()))
                .collect(),
            diagnostics: out.diagnostics.iter().map(|d| d.code.to_string()).collect(),
        }
    }

    fn render(&self) -> String {
        let mut s = format!("stdout {:?}", self.stdout);
        if !self.stderr.is_empty() {
            s.push_str(&format!("\n      stderr {:?}", self.stderr));
        }
        if !self.trace.is_empty() {
            s.push_str(&format!("\n      trace {:?}", self.trace));
        }
        if !self.diagnostics.is_empty() {
            s.push_str(&format!("\n      diagnostics {:?}", self.diagnostics));
        }
        s
    }
}

/// Boot `src` the way the CLI launch path does: checked compile, session adopted from it, entry 0
/// run to completion. `None` when the program does not check green — a negative corpus case, which
/// has nothing to swap.
fn boot(src: &str) -> Option<(VmSession, Observation)> {
    noeta_stdlib::registry::default_seeded();
    let (program, clean) = parse(src);
    if !clean {
        return None;
    }
    let checked = noeta_check::check_all(&program);
    if noeta_diagnostics::has_errors(&checked.diagnostics) {
        return None;
    }
    let (module, compiler) =
        noeta_compiler::compile_with_sites_session(&program, checked.sites, false, true).ok()?;
    let (session, out) = VmSession::adopted(&module, compiler, factory());
    let observed = Observation::of(&out);
    Some((session, observed))
}

/// The swap half: diff v1 → v2, gate on the new version's check exactly as the watcher does, apply
/// the plan with that check's whole-program sites. `Err` carries why no swap happened.
fn swap(session: &mut VmSession, v1: &str, v2: &str) -> Result<Observation, Skip> {
    noeta_stdlib::registry::default_seeded();
    let (old, old_clean) = parse(v1);
    let (new, new_clean) = parse(v2);
    if !old_clean || !new_clean {
        return Err(Skip::GeneratedVersionDoesNotParse);
    }
    let checked = noeta_check::check_all(&new);
    if noeta_diagnostics::has_errors(&checked.diagnostics) {
        return Err(Skip::GeneratedVersionDoesNotCheck);
    }
    match noeta_compiler::hotswap::diff_programs(&old, v1, &new, v2) {
        SwapDiff::Swap(plan) => {
            let out = session.hot_swap(&plan, Some(&checked.sites));
            Ok(Observation::of(&out))
        }
        SwapDiff::Unchanged => Err(Skip::DiffSaysUnchanged),
        SwapDiff::NeedsRestart(_) => Err(Skip::DiffSaysRestart),
    }
}

fn probe(session: &mut VmSession, src: &str) -> Observation {
    let (program, _) = parse(src);
    Observation::of(&session.eval(&program))
}

// ------------------------------------------------------------------ the generators

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Generator {
    /// v2 = v1 + a clone of an eligible top-level `fn` under a fresh name. Differ verdict: `added`.
    CloneAppend,
    /// v1 = program + a delegate stub; v2 = the same name carrying the real body. Verdict: `changed`.
    CloneChanged,
    /// v2 = v1 + one more top-level statement. Verdict: a **re-running** swap.
    RerunTopLevel,
    /// v1 = the program with one top-level `fn`'s body carrying an inert extra statement; v2 = the
    /// program itself, plus a marker statement. Verdict: a re-running swap **whose fragment also
    /// carries a changed function body** — so the real body is fragment-compiled and then called by
    /// the re-run top level. Works for a `fn` of any arity, which is why it exists: the clone
    /// generators can only probe a zero-arity function, and most corpus functions take arguments.
    RerunWithBodyEdit,
}

impl Generator {
    fn label(self) -> &'static str {
        match self {
            Generator::CloneAppend => "clone-append (added fn)",
            Generator::CloneChanged => "clone-changed (body edit)",
            Generator::RerunTopLevel => "rerun (top-level edit)",
            Generator::RerunWithBodyEdit => "rerun + changed fn body",
        }
    }

    /// Whether this generator's observation is the **re-run's own** output rather than a probe's.
    fn observes_the_rerun(self) -> bool {
        matches!(
            self,
            Generator::RerunTopLevel | Generator::RerunWithBodyEdit
        )
    }
}

/// A generated oracle case: the two versions and the probe that observes the difference.
struct Case {
    v1: String,
    v2: String,
    probe: String,
}

/// The fresh name every generated declaration takes. Deliberately unspellable-by-accident so a
/// clash with a corpus program's own names is not a thing that can happen.
const PROBE_FN: &str = "__swap_probe_0";

/// Whether a top-level `fn` can be cloned at all.
///
/// Non-generic (a clone of a generic fn would need a turbofish to call) and not a tier runner (a
/// second `@tier(name, …)` declaration would declare the tier twice). `zero_arity` additionally
/// requires no parameters — what the clone generators need, since they probe by *calling* the clone
/// and have no values to pass it.
fn eligible(decl: &FnDecl, zero_arity: bool) -> bool {
    decl.type_params.is_empty()
        && decl.tier.is_none()
        && !decl.name.as_str().starts_with("__swap")
        && (!zero_arity || decl.params.is_empty())
}

/// The fn to work on: the **last** eligible one in source order whose body could produce something
/// observable (an `echo` or a `return`), else the last eligible one at all.
///
/// Last rather than first because corpus programs put their interesting function after the types
/// and helpers it uses; a body with an `echo` or a `return` is preferred because a clone whose call
/// yields no output makes the oracle vacuous — it would compare `""` against `""` forever.
fn pick<'a>(program: &'a Program, src: &str, zero_arity: bool) -> Option<&'a FnDecl> {
    let fns: Vec<&FnDecl> = program
        .stmts
        .iter()
        .filter_map(|s| match s {
            Stmt::Fn(decl) if eligible(decl, zero_arity) => Some(decl),
            _ => None,
        })
        .collect();
    let observable = |decl: &FnDecl| {
        let text = &src[decl.span.start as usize..decl.span.end as usize];
        text.contains("echo") || text.contains("return")
    };
    fns.iter()
        .rev()
        .find(|decl| observable(decl))
        .or_else(|| fns.last())
        .copied()
}

/// A **top-level call** of `name`, as source text — `f(1, "x")` lifted verbatim out of whichever
/// top-level statement calls it.
///
/// This is what lets the clone generators reach a function that takes arguments, which is most of
/// them: rather than inventing values for its parameters (impossible in general — they may be
/// user structs, closures, generics), reuse a call the program already writes. Restricted to
/// **top-level** statements on purpose: their operands are by construction the names a session
/// entry can also see, so the lifted text stays evaluable where a call lifted out of some function
/// body would reference that body's locals.
///
/// Found textually — from `name(` to the paren that balances it — rather than by walking sixteen
/// `Expr` variants for a `Call` node. A `name(` inside a string literal would be mis-lifted, and the
/// lift is not verified here: the oracle's own cold arm is the check, and a probe that does not
/// compile is skipped as [`Skip::ProbeDoesNotCompile`] rather than passing vacuously.
fn top_level_call(program: &Program, src: &str, name: &str) -> Option<String> {
    let needle = format!("{name}(");
    for stmt in &program.stmts {
        if is_declaration(stmt) {
            continue;
        }
        let (from, to) = (stmt.span().start as usize, stmt.span().end as usize);
        let text = &src[from..to];
        let mut search = 0;
        while let Some(hit) = text[search..].find(&needle) {
            let at = search + hit;
            search = at + needle.len();
            // A call, not the tail of a longer identifier or a member access.
            let preceded_by = text[..at].chars().next_back();
            if preceded_by.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.') {
                continue;
            }
            let open = at + needle.len() - 1;
            if let Some(close) = balanced(&text[open..]) {
                return Some(format!("{PROBE_FN}{}", &text[open..=open + close]));
            }
        }
    }
    None
}

/// The offset, within `text` (which starts at an opening `(`), of the paren that balances it.
fn balanced(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in text.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether a top-level statement is a declaration rather than executable top level.
fn is_declaration(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::Fn(_)
            | Stmt::Struct(_)
            | Stmt::Class(_)
            | Stmt::Enum(_)
            | Stmt::Trait(_)
            | Stmt::Impl(_)
            | Stmt::Use { .. }
            | Stmt::Namespace { .. }
            | Stmt::TierBlock { .. }
    )
}

/// The clone's source text: the declaration verbatim, with its name replaced.
fn clone_text(decl: &FnDecl, src: &str) -> String {
    let start = decl.span.start as usize;
    let text = &src[start..decl.span.end as usize];
    let at = decl.name_span.start as usize - start;
    let to = decl.name_span.end as usize - start;
    format!("{}{PROBE_FN}{}", &text[..at], &text[to..])
}

/// The probe source: call the clone and echo what it answers. An `async` clone returns a future, so
/// the probe awaits it — the session entry is the scheduler tick that resolves it.
fn probe_call(decl: &FnDecl, call: &str) -> String {
    if decl.is_async {
        format!("echo {call}.await\n")
    } else {
        format!("echo {call}\n")
    }
}

/// A stub with the clone's exact signature and a body that just delegates to the original — so v1
/// holds a *declaration* the differ can then see change into the real body.
///
/// The stub does not have to mean the same thing as the body: the oracle compares two runs of **v2**
/// against each other, and v1 is only the state the session starts in.
fn delegate_text(decl: &FnDecl, src: &str, call: &str) -> String {
    let start = decl.span.start as usize;
    let head_end = body_brace(decl, src);
    let head = &src[start..head_end];
    let at = decl.name_span.start as usize - start;
    let to = decl.name_span.end as usize - start;
    let head = format!("{}{PROBE_FN}{}", &head[..at], &head[to..]);
    // Delegate through the *original*, with whatever arguments the probe will pass the clone —
    // the stub only has to check green, and forwarding its own parameters would need their names.
    let call = call.replacen(PROBE_FN, decl.name.as_str(), 1);
    let call = if decl.is_async {
        format!("{call}.await")
    } else {
        call
    };
    let body = if decl.ret.is_some() {
        format!("return {call}")
    } else {
        call
    };
    format!("{head} {{\n    {body}\n}}\n")
}

/// Where a declaration's body block opens.
///
/// Found from the **first body statement's span** backwards, not by scanning the header forwards: a
/// default parameter value can itself contain a brace (`fn f(p: Point = Point { x: 0 })`) and a
/// forward scan would stop there. An empty body has no statement to anchor on, so it falls back to
/// the last `{` in the declaration — which for `{ }` is the right one.
fn body_brace(decl: &FnDecl, src: &str) -> usize {
    let bytes = src.as_bytes();
    let from = match decl.body.first() {
        Some(stmt) => stmt.span().start as usize,
        None => decl.span.end as usize,
    };
    let floor = decl.name_span.end as usize;
    let mut i = from.min(src.len());
    while i > floor {
        i -= 1;
        if bytes[i] == b'{' {
            return i;
        }
    }
    from
}

/// The program with one `fn`'s body carrying an extra, inert statement as its first line: a fresh
/// binding of a constant, which binds a name nothing reads.
///
/// This is the mechanical "the developer edited this function" delta. It has to be *inert* — the
/// oracle compares two runs of v2, and v1 only has to check green — and it has to be a real
/// statement, because the differ normalizes formatting and would call a comment-only edit unchanged.
fn with_edited_body(decl: &FnDecl, src: &str) -> String {
    let at = body_brace(decl, src) + 1;
    format!("{}\n    __swap_touch = 0\n{}", &src[..at], &src[at..])
}

/// The fn the clone generators work on, together with the call the probe will make: a zero-arity
/// one calls trivially, otherwise the program must already call it from its top level so the
/// arguments can be lifted verbatim ([`top_level_call`]).
fn callable<'a>(program: &'a Program, src: &str) -> Result<(&'a FnDecl, String), Skip> {
    if let Some(decl) = pick(program, src, true) {
        return Ok((decl, format!("{PROBE_FN}()")));
    }
    let Some(decl) = pick(program, src, false) else {
        return Err(Skip::NoTopLevelFn);
    };
    match top_level_call(program, src, decl.name.as_str()) {
        Some(call) => Ok((decl, call)),
        None => Err(Skip::NoZeroArityFn),
    }
}

fn generate(gtor: Generator, src: &str, program: &Program) -> Result<Case, Skip> {
    match gtor {
        Generator::CloneAppend => {
            let (decl, call) = callable(program, src)?;
            Ok(Case {
                v1: src.to_string(),
                v2: format!("{src}\n{}\n", clone_text(decl, src)),
                probe: probe_call(decl, &call),
            })
        }
        Generator::CloneChanged => {
            let (decl, call) = callable(program, src)?;
            Ok(Case {
                v1: format!("{src}\n{}\n", delegate_text(decl, src, &call)),
                v2: format!("{src}\n{}\n", clone_text(decl, src)),
                probe: probe_call(decl, &call),
            })
        }
        Generator::RerunTopLevel => Ok(Case {
            v1: src.to_string(),
            v2: format!("{src}\necho \"__swap_rerun_marker\"\n"),
            // The re-run's own output *is* the observation; the probe only has to be a legal entry.
            probe: String::new(),
        }),
        Generator::RerunWithBodyEdit => {
            let decl = pick(program, src, false).ok_or(Skip::NoTopLevelFn)?;
            Ok(Case {
                v1: with_edited_body(decl, src),
                v2: format!("{src}\necho \"__swap_rerun_marker\"\n"),
                probe: String::new(),
            })
        }
    }
}

// ------------------------------------------------------------------ the oracle

/// Why a program contributed no comparison. Assigned by the harness, never hand-maintained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Skip {
    /// A negative corpus case: it does not lex/parse/check clean, so there is nothing to boot.
    NotGreen,
    /// The program checks but panics on its own top level (a case that pins an abort).
    BootPanics,
    /// The program declares no usable top-level `fn` at all (it is pure top-level statements, or
    /// every `fn` it has is generic or a tier runner). Nothing for a fn-shaped generator to touch.
    NoTopLevelFn,
    /// The program has a top-level `fn`, but every one of them takes arguments — so the clone
    /// generators, which probe by *calling* the clone, have no values to pass it. The
    /// `rerun + changed fn body` generator covers these instead: it reaches the body through the
    /// program's own top level rather than through a synthesized call.
    NoZeroArityFn,
    /// The mechanical v2 does not parse — the generator mangled something.
    GeneratedVersionDoesNotParse,
    /// The mechanical v2 parses but does not check. Cloning a body into a second declaration can
    /// legitimately fail to check (a name the checker treats as once-only, a `use`d capture the
    /// clone re-declares); not a swap bug.
    GeneratedVersionDoesNotCheck,
    /// The differ saw no difference — nothing to swap.
    DiffSaysUnchanged,
    /// The differ blocked the swap (a restart verdict). Correct behavior, no comparison to make.
    DiffSaysRestart,
    /// Two cold runs of v2 disagree: the program reads the clock, entropy, a port, the filesystem,
    /// or prints an address. Detected by measurement.
    Nondeterministic,
    /// The top level is not re-run stable — running it a second time in the same session
    /// legitimately differs (retained plain state, an external side effect). Re-run generator only.
    TopLevelNotRerunStable,
    /// The generated probe does not compile: an argument list lifted out of a top-level call
    /// referenced something a session entry cannot see, or the textual lift caught a `name(` that
    /// was not a call. Detected on the **cold** arm, so a bogus probe is a skip rather than a pair
    /// of identically-broken runs passing as agreement.
    ProbeDoesNotCompile,
    /// The case did not finish inside [`CASE_TIMEOUT`].
    Timeout,
}

impl Skip {
    fn label(self) -> &'static str {
        match self {
            Skip::NotGreen => "not green (negative case: expects a lex/parse/check error)",
            Skip::BootPanics => "boots to a panic (a case that pins an abort)",
            Skip::NoTopLevelFn => {
                "no usable top-level fn (pure top level, or generic/tier-runner only)"
            }
            Skip::NoZeroArityFn => "every top-level fn takes arguments (no clone to call)",
            Skip::GeneratedVersionDoesNotParse => "generated v2 does not parse",
            Skip::GeneratedVersionDoesNotCheck => "a generated version does not check",
            Skip::DiffSaysUnchanged => "differ: unchanged",
            Skip::DiffSaysRestart => "differ: needs restart",
            Skip::Nondeterministic => "nondeterministic (two cold runs disagree)",
            Skip::TopLevelNotRerunStable => "top level not re-run stable (retained state)",
            Skip::ProbeDoesNotCompile => "generated probe does not compile",
            Skip::Timeout => "timed out",
        }
    }
}

/// One program's divergence, with everything a reader needs to reproduce it.
#[derive(Debug, Clone)]
struct Divergence {
    case: String,
    generator: Generator,
    v1: String,
    v2: String,
    probe: String,
    via_swap: Observation,
    via_cold: Observation,
}

enum Verdict {
    /// Compared and agreed. `silent` records that the comparison saw no stdout at all — a real
    /// comparison (a swap that aborted would still show) but a weak one, so it is reported apart
    /// rather than counted as if it had pinned an answer.
    Exercised {
        silent: bool,
    },
    Skipped(Skip),
    Diverged(Box<Divergence>),
}

/// Run one generated case through the oracle.
///
/// Cold first and **twice**: a program whose two cold runs disagree is nondeterministic, and its
/// swap arm would be compared against a moving target. Only then the swap arm.
fn oracle(name: &str, gtor: Generator, case: &Case) -> Verdict {
    // --- cold arm, run twice.
    let cold = |()| -> Option<Observation> {
        let (mut session, boot_out) = boot(&case.v2)?;
        let observed = if gtor.observes_the_rerun() {
            // The re-running swap re-executes the top level, so a cold start's *boot* output is the
            // thing it must reproduce.
            boot_out
        } else {
            probe(&mut session, &case.probe)
        };
        session.teardown();
        Some(observed)
    };
    let Some(first) = cold(()) else {
        return Verdict::Skipped(Skip::GeneratedVersionDoesNotCheck);
    };
    let Some(second) = cold(()) else {
        return Verdict::Skipped(Skip::GeneratedVersionDoesNotCheck);
    };
    if first != second {
        return Verdict::Skipped(Skip::Nondeterministic);
    }
    if !first.diagnostics.is_empty() {
        return Verdict::Skipped(Skip::ProbeDoesNotCompile);
    }

    // --- swap arm. The program itself was gated green before any generator ran, so a boot that
    // fails here is the *generated* v1 — the clone-changed stub — not the corpus case.
    let Some((mut session, boot_out)) = boot(&case.v1) else {
        return Verdict::Skipped(Skip::GeneratedVersionDoesNotCheck);
    };
    if !boot_out.trace.is_empty() {
        session.teardown();
        return Verdict::Skipped(Skip::BootPanics);
    }
    let swapped = match swap(&mut session, &case.v1, &case.v2) {
        Ok(out) => out,
        Err(skip) => {
            session.teardown();
            return Verdict::Skipped(skip);
        }
    };
    let via_swap = if gtor.observes_the_rerun() {
        swapped
    } else {
        // A body swap re-evaluates declarations only: the fragment entry must be silent. Anything
        // it emits (a diagnostic, an abort) is folded into the observation, which is how a
        // fragment that fails to compile or run surfaces as a divergence rather than as a pass.
        let mut observed = probe(&mut session, &case.probe);
        observed.trace.extend(swapped.trace);
        observed.diagnostics.extend(swapped.diagnostics);
        observed.stdout = format!("{}{}", swapped.stdout, observed.stdout);
        observed.stderr = format!("{}{}", swapped.stderr, observed.stderr);
        observed
    };
    session.teardown();

    if via_swap == first {
        Verdict::Exercised {
            silent: first.stdout.is_empty(),
        }
    } else {
        Verdict::Diverged(Box::new(Divergence {
            case: name.to_string(),
            generator: gtor,
            v1: case.v1.clone(),
            v2: case.v2.clone(),
            probe: case.probe.clone(),
            via_swap,
            via_cold: first,
        }))
    }
}

/// The re-run generator's precondition, measured rather than assumed: swap in a v2 whose only
/// change is an **inert** top-level binding and require the re-run to reproduce the boot exactly.
/// A program that fails this has retained state or an external side effect — a second run of its
/// top level differs for reasons that are not swap bugs, so the observable re-run case would be
/// comparing noise.
fn rerun_is_stable(src: &str) -> Result<(), Skip> {
    let inert = format!("{src}\n__swap_rerun_inert = 1\n");
    let Some((mut session, boot_out)) = boot(src) else {
        return Err(Skip::NotGreen);
    };
    if !boot_out.trace.is_empty() {
        session.teardown();
        return Err(Skip::BootPanics);
    }
    let result = swap(&mut session, src, &inert);
    session.teardown();
    match result {
        Ok(out) if out.stdout == boot_out.stdout && out.trace.is_empty() => Ok(()),
        Ok(_) => Err(Skip::TopLevelNotRerunStable),
        Err(skip) => Err(skip),
    }
}

// ------------------------------------------------------------------ the sweep

/// Every single-file corpus case: a `.noe` file at `conformance/<category>/`.
///
/// Multi-file cases (a case *directory* with siblings, a manifest, or dependency subdirectories)
/// are deliberately out of scope here — they need the loader's linking, and the hot-swap differ
/// works on one program at a time. They are counted and reported, not silently dropped.
fn corpus() -> (Vec<(String, PathBuf)>, usize) {
    let root = Path::new(CORPUS);
    let mut single = Vec::new();
    let mut multi = 0usize;
    walk(root, root, &mut single, &mut multi);
    single.sort();
    (single, multi)
}

fn walk(root: &Path, dir: &Path, single: &mut Vec<(String, PathBuf)>, multi: &mut usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let depth = dir
        .strip_prefix(root)
        .map(|p| p.components().count())
        .unwrap_or(0);
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            walk(root, &path, single, multi);
        } else if path.extension().is_some_and(|e| e == "noe") {
            if depth <= 1 {
                let name = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                single.push((name, path));
            } else {
                *multi += 1;
            }
        }
    }
}

#[derive(Default)]
struct Tally {
    exercised: BTreeMap<Generator, usize>,
    /// Of the exercised cases, those whose comparison saw no stdout on either side.
    silent: BTreeMap<Generator, usize>,
    skipped: BTreeMap<(Generator, Skip), usize>,
    divergences: Vec<Divergence>,
}

impl Tally {
    fn record(&mut self, gtor: Generator, verdict: Verdict) {
        match verdict {
            Verdict::Exercised { silent } => {
                *self.exercised.entry(gtor).or_default() += 1;
                if silent {
                    *self.silent.entry(gtor).or_default() += 1;
                }
            }
            Verdict::Skipped(skip) => *self.skipped.entry((gtor, skip)).or_default() += 1,
            Verdict::Diverged(d) => self.divergences.push(*d),
        }
    }

    fn merge(&mut self, other: Tally) {
        for (k, v) in other.exercised {
            *self.exercised.entry(k).or_default() += v;
        }
        for (k, v) in other.silent {
            *self.silent.entry(k).or_default() += v;
        }
        for (k, v) in other.skipped {
            *self.skipped.entry(k).or_default() += v;
        }
        self.divergences.extend(other.divergences);
    }
}

/// Run every generator over one corpus program.
///
/// The two whole-program preconditions are settled **first and once**, so every generator reports
/// the same reason for the same program: a corpus case that is a negative test, or one that pins an
/// abort, has nothing to swap under any generator, and counting it as "no eligible fn" under two of
/// them and "not green" under the other two would make the coverage table lie about where the
/// harness actually stops.
fn sweep_one(name: &str, src: &str) -> Tally {
    let mut tally = Tally::default();
    let all = |tally: &mut Tally, skip: Skip| {
        for gtor in GENERATORS {
            tally.record(gtor, Verdict::Skipped(skip));
        }
    };
    let (program, clean) = parse(src);
    if !clean {
        all(&mut tally, Skip::NotGreen);
        return tally;
    }
    let Some((mut session, boot_out)) = boot(src) else {
        all(&mut tally, Skip::NotGreen);
        return tally;
    };
    let panicked = !boot_out.trace.is_empty();
    session.teardown();
    if panicked {
        all(&mut tally, Skip::BootPanics);
        return tally;
    }
    // Measured once and shared: both re-run generators need the same precondition, and it costs a
    // boot plus a swap.
    let mut stability: Option<Result<(), Skip>> = None;
    for gtor in GENERATORS {
        if gtor.observes_the_rerun() {
            let verdict = *stability.get_or_insert_with(|| rerun_is_stable(src));
            if let Err(skip) = verdict {
                tally.record(gtor, Verdict::Skipped(skip));
                continue;
            }
        }
        match generate(gtor, src, &program) {
            Ok(case) => {
                let verdict = oracle(name, gtor, &case);
                tally.record(gtor, verdict);
            }
            Err(skip) => tally.record(gtor, Verdict::Skipped(skip)),
        }
    }
    tally
}

const GENERATORS: [Generator; 4] = [
    Generator::CloneAppend,
    Generator::CloneChanged,
    Generator::RerunTopLevel,
    Generator::RerunWithBodyEdit,
];

/// Run one case on its own thread with a wall-clock budget.
///
/// A corpus program may block forever (the HTTP-server cases bind a real socket), and a blocked
/// case must cost one skip, not the whole sweep. The thread is abandoned on timeout — the process
/// is a test binary and exits when the sweep returns.
fn sweep_one_guarded(name: &str, src: &str) -> Tally {
    let (tx, rx) = mpsc::channel();
    let (name_owned, src_owned) = (name.to_string(), src.to_string());
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let tally = sweep_one(&name_owned, &src_owned);
            let _ = tx.send(tally);
        })
        .expect("spawning a sweep thread");
    match rx.recv_timeout(CASE_TIMEOUT) {
        Ok(tally) => tally,
        Err(_) => {
            let mut tally = Tally::default();
            for gtor in GENERATORS {
                tally.record(gtor, Verdict::Skipped(Skip::Timeout));
            }
            tally
        }
    }
}

/// How many corpus programs are swept concurrently.
///
/// Each case is independent — its own session, its own heap owner, no shared mutable state beyond
/// the process-wide extension registry, which is seeded once and then read-only. Sequentially the
/// full sweep is ~96s, which is exactly the length at which a test gets `#[ignore]`d and then never
/// run; fanned out it is well inside ordinary-test range, so it stays a test everyone runs.
static WORKERS: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(1, 8))
        .unwrap_or(4)
});

fn sweep(cases: &[(String, PathBuf)], multi: usize) -> Tally {
    let started = Instant::now();
    // Seed the shared registry once, on this thread, before any worker reads it.
    noeta_stdlib::registry::default_seeded();
    let mut tally = Tally::default();
    let chunk = cases.len().div_ceil(*WORKERS).max(1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = cases
            .chunks(chunk)
            .map(|slice| {
                scope.spawn(move || {
                    let mut local = Tally::default();
                    for (name, path) in slice {
                        let Ok(src) = std::fs::read_to_string(path) else {
                            continue;
                        };
                        local.merge(sweep_one_guarded(name, &src));
                    }
                    local
                })
            })
            .collect();
        for handle in handles {
            tally.merge(handle.join().expect("a sweep worker must not panic"));
        }
    });
    tally
        .divergences
        .sort_by(|a, b| (&a.case, a.generator).cmp(&(&b.case, b.generator)));
    report(&tally, cases.len(), multi, started.elapsed());
    tally
}

fn report(tally: &Tally, programs: usize, multi: usize, elapsed: Duration) {
    println!("\n=== corpus swap differential ===");
    println!(
        "{programs} single-file corpus programs x {} generators = {} candidate oracle cases  \
         ({multi} multi-file corpus files out of scope: they need the loader's linking)",
        GENERATORS.len(),
        programs * GENERATORS.len()
    );
    let not_green: usize = tally
        .skipped
        .iter()
        .filter(|((_, s), _)| matches!(s, Skip::NotGreen | Skip::BootPanics))
        .map(|(_, n)| *n)
        .sum::<usize>()
        / GENERATORS.len();
    let green = programs - not_green;
    println!(
        "{green} of them are green and run to completion ({not_green} are negative cases or pin \
         an abort, so there is nothing to swap)"
    );
    println!("swept in {:.1}s\n", elapsed.as_secs_f64());
    for gtor in GENERATORS {
        let exercised = tally.exercised.get(&gtor).copied().unwrap_or(0);
        let diverged = tally
            .divergences
            .iter()
            .filter(|d| d.generator == gtor)
            .count();
        let skipped: usize = tally
            .skipped
            .iter()
            .filter(|((g, _), _)| *g == gtor)
            .map(|(_, n)| *n)
            .sum();
        let silent = tally.silent.get(&gtor).copied().unwrap_or(0);
        println!(
            "  {:<26}  compared {:>4} (of which {:>3} silent)   diverged {:>3}   skipped {:>4}",
            gtor.label(),
            exercised + diverged,
            silent,
            diverged,
            skipped
        );
        let mut reasons: Vec<(Skip, usize)> = tally
            .skipped
            .iter()
            .filter(|((g, _), _)| *g == gtor)
            .map(|((_, s), n)| (*s, *n))
            .collect();
        reasons.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        for (skip, n) in reasons {
            println!("        {n:>4}  {}", skip.label());
        }
    }
    let compared: usize = tally.exercised.values().sum::<usize>() + tally.divergences.len();
    let silent: usize = tally.silent.values().sum();
    println!(
        "\n  TOTAL compared {compared} of {} candidate cases ({:.0}%) — or {:.0}% of the {} cases \
         a green program can offer. {silent} were silent (no stdout on either side). \
         Divergences: {}.",
        programs * GENERATORS.len(),
        100.0 * compared as f64 / (programs * GENERATORS.len()).max(1) as f64,
        100.0 * compared as f64 / (green * GENERATORS.len()).max(1) as f64,
        green * GENERATORS.len(),
        tally.divergences.len()
    );

    for d in &tally.divergences {
        println!("\n--- DIVERGENCE: {} [{}]", d.case, d.generator.label());
        println!("  probe: {:?}", d.probe);
        println!("  via swap:\n      {}", d.via_swap.render());
        println!("  via cold start:\n      {}", d.via_cold.render());
        // v1 only when it is not simply a prefix of v2 — for the append-shaped generators printing
        // it would repeat the whole program for no information.
        if !d.v2.starts_with(&d.v1) {
            println!("  v1 (swapped FROM):\n{}", indent(&d.v1));
        }
        println!("  v2 (swapped TO, and cold-started):\n{}", indent(&d.v2));
    }
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| format!("    | {l}\n"))
        .collect::<String>()
}

// ------------------------------------------------------------------ the open defects this found

/// The divergences the corpus differential reports **today** — and it reports none.
///
/// Enforced in BOTH directions: a divergence the sweep finds that is *not* listed here fails the
/// build, and an entry here that stops reproducing fails it too. The second half is what stops a
/// list like this from outliving its bugs, and it is why this one emptied out instead of settling
/// into a backlog — each fix had to delete its own rows to go green.
///
/// The nine it found are the arc's record: three from a fragment-compiled body skipping a
/// destructor (`thread_reuse` built its own-destructor exclusion set from the IR in hand, which a
/// fragment does not carry), three from `roles_of` losing a binding whose `@role` declaration is
/// ambient to the fragment, one where that same defect escalated to an E0016 abort because the
/// program indexed the list instead of scanning it, and two from the attribute manifest being
/// appended to rather than superseded in place.
///
/// 1. ~~**A fragment-compiled body skips a destructor.**~~ FIXED. `gc/self_update_own_destructor_no_reuse`
///    pins that `acc = Counter { ...acc, n: 1 }` must *not* reuse the allocation when the type has
///    its own `destruct`, so the displaced value runs its destructor. Swapped, the reuse fired and
///    `drop counter 0` never printed — reported independently by three of the four generators. The
///    reuse pass read its own-destructor exclusion set off the class declarations in the IR it was
///    handed, and a fragment carries none; the set now travels with the program
///    (`noeta_ir::ProgramFacts::own_destructors`). Pinned targetedly by
///    `hotswap.rs::a_swapped_self_update_still_destroys_the_value_it_displaces`.
///
/// Two further families the sweep found have since been **fixed**, both in
/// `ReflectionInfo::accumulate`, and are pinned in the hand-written oracle (`hotswap.rs`) rather
/// than only swept for: a swapped declaration losing the `@role` binding its unchanged attribute
/// conferred (`reflection/roles_of`, `roles_of_scoped`, `prelude_enums_constructible`, and the
/// E0016 abort in `prelude_structs_constructible`, which was the same defect escalating), and the
/// attribute manifest reordering when a re-registered declaration was appended instead of
/// superseded in place (`reflection/attributes_on_functions`, `attributes_on_params`).
const KNOWN_DIVERGENCES: &[(&str, Generator)] = &[];

/// Compare what the sweep found against [`KNOWN_DIVERGENCES`], in both directions.
fn assert_against_the_known_defects(tally: &Tally, exhaustive: bool) {
    let found: Vec<(&str, Generator)> = tally
        .divergences
        .iter()
        .map(|d| (d.case.as_str(), d.generator))
        .collect();
    let unexpected: Vec<_> = found
        .iter()
        .filter(|entry| !KNOWN_DIVERGENCES.contains(entry))
        .collect();
    assert!(
        unexpected.is_empty(),
        "NEW hot-swap divergences (details printed above): {unexpected:#?}"
    );
    if exhaustive {
        let fixed: Vec<_> = KNOWN_DIVERGENCES
            .iter()
            .filter(|entry| !found.contains(entry))
            .collect();
        assert!(
            fixed.is_empty(),
            "these divergences no longer reproduce — the defect was fixed, so drop it from \
             KNOWN_DIVERGENCES: {fixed:#?}"
        );
    }
}

// ------------------------------------------------------------------ entry points

/// The whole corpus, every generator. An ordinary test: fanned out over [`WORKERS`] it runs in well
/// under a minute, so nobody has to remember a flag to get the coverage.
#[test]
fn the_whole_corpus_swaps_like_a_cold_start() {
    let (cases, multi) = corpus();
    assert!(
        cases.len() > 500,
        "the corpus should have been discovered at {CORPUS}, found {} cases",
        cases.len()
    );
    let tally = sweep(&cases, multi);
    let compared: usize = tally.exercised.values().sum::<usize>() + tally.divergences.len();
    assert!(
        compared > 800,
        "the sweep compared only {compared} cases — the generators have stopped generating"
    );
    assert_against_the_known_defects(&tally, true);
}
