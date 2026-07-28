//! `noeta repl` — the interactive session: per-entry lex/parse/check/eval over the VM
//! session, meta-commands, and the `--load` bootstrap ("tinker") path.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_lexer::{TokenKind, lex};
use noeta_parser::{parse, parse_fragment};
use noeta_pm::manifest;
use noeta_span::{Source, SourceId, SourceMap};
use noeta_vm::{SessionOutput, VmSession};

use crate::compose;
use crate::output::{emit_diagnostics, emit_diagnostics_mapped, emit_trace};

/// Whether an entry was consumed (evaluated or reported) or is still incomplete and needs
/// more input (multiline continuation).
pub(crate) enum ReplStep {
    Consumed,
    Incomplete,
}

/// The environment the REPL runs against: a **real** host + wall-clock executor, so `fs`, `time`,
/// `env`, and `uuid()` work at the prompt against the real machine — exactly as `noeta run` does. Built
/// fresh on `:reset`. (The deterministic sandbox exists only to make the differential oracle
/// reproducible; it is not what an interactive prompt wants.) A real-thread `isolate f(args)` falls
/// back to a cooperative task here, since the session does not arm the parallel-isolate path.
pub(crate) fn real_repl_env() -> noeta_vm::HostFactory {
    Box::new(|| {
        let host: Box<dyn noeta_stdlib::Host> =
            Box::new(noeta_host_real::RealHost::new().expect("cannot start the REPL's runtime"));
        let executor: Box<dyn noeta_stdlib::Executor> = Box::new(
            noeta_host_real::RealExecutor::new().expect("cannot start the REPL's async executor"),
        );
        (host, executor)
    })
}

/// Load, check, compile, and RUN the `--load` bootstrap, returning the adopted session and the
/// bootstrap's sources (the REPL's entry ids continue after them). The bootstrap is a *file*, so
/// it is always fully checked — as it would be under `noeta run` — regardless of `--no-check`
/// (which governs prompt entries); with checking on, the bootstrap's own checker session carries
/// forward, so entries check against everything it declared and bound. Isolates in a bootstrap run
/// cooperatively (the session's execution model). Any failure — unreadable file, load/check
/// diagnostics, a runtime abort — exits with diagnostics instead of opening a broken prompt.
pub(crate) fn repl_bootstrap(
    path: &std::path::Path,
    checker: &mut Option<noeta_check::SessionChecker>,
    resolved: Option<noeta_pm::graph::ResolvedGraph>,
) -> Result<(VmSession, Vec<Source>), ExitCode> {
    // The shared front half (drift firewall): `repl --load` sees the same dependency packages and
    // editions `noeta run` resolves — a program that runs must also load at the prompt. The
    // compose probe's graph (default selection) is reused rather than resolved again (audit-5 F2).
    let loaded = match noeta_runner::compile::load_default_project_with(
        path,
        resolved.map(|g| g.packages),
    ) {
        Ok(loaded) => loaded,
        Err(failure) => return Err(failure.report()),
    };

    // Always checked (it is a file); the session flavor keeps the checker when the prompt wants it.
    let (checked, session_checker) = loaded.check_session();
    if !checked.diagnostics.is_empty() {
        emit_diagnostics_mapped(&loaded.sources, checked.diagnostics.iter());
        return Err(ExitCode::FAILURE);
    }
    if checker.is_some() {
        *checker = Some(session_checker);
    }

    // Cooperative isolates + no debug info: the prompt's own execution model.
    let (module, compiler) = match noeta_compiler::compile_with_sites_session(
        &loaded.program,
        checked.sites,
        false,
        false,
    ) {
        Ok(pair) => pair,
        Err(u) => {
            // A located internal failure renders like any other diagnostic; a span-less one keeps
            // the one-line form (`Unsupported`'s `Display`).
            match u.diagnostic() {
                Some(diagnostic) => {
                    emit_diagnostics_mapped(&loaded.sources, std::iter::once(&diagnostic))
                }
                None => eprintln!("noeta: {u}"),
            }
            return Err(ExitCode::FAILURE);
        }
    };
    let (session, out) = VmSession::adopted(&module, compiler, real_repl_env());
    print!("{}", out.stdout);
    let _ = io::stdout().flush();
    eprint!("{}", out.stderr);
    let _ = io::stderr().flush();
    if !out.diagnostics.is_empty() {
        // A bootstrap that aborts is a broken app context — fail fast, exactly like `noeta run`.
        emit_diagnostics_mapped(&loaded.sources, out.diagnostics.iter());
        emit_trace(&out.trace, &loaded.sources);
        return Err(ExitCode::FAILURE);
    }
    eprintln!("(loaded {})", path.display());
    Ok((session, loaded.sources.into_sources()))
}

pub(crate) fn cmd_repl(check: bool, load: Option<PathBuf>) -> ExitCode {
    // A `--load` bootstrap is a program run — a native-dep app's REPL must be the composed one.
    // The probe's resolved graph (default selection) feeds the bootstrap load (audit-5 F2).
    let mut resolved = None;
    if let Some(file) = &load {
        match compose::maybe_delegate(file) {
            Err(code) => return code,
            Ok(graph) => resolved = graph,
        }
    }
    let stdin = io::stdin();
    // The edition prompt entries lex/parse under (editions arc): the `--load` file's package
    // edition when bootstrapped, else the enclosing project's (a bare prompt in a package dir
    // tinkers in that package's dialect); a manifest-less cwd is the default edition.
    let edition = load
        .as_deref()
        .map(manifest::root_edition)
        .unwrap_or_else(|| {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            manifest::root_edition(&cwd.join("_.noe"))
        });
    // The optional per-entry type checker (session-checker C2): `Some` = every entry is checked
    // against the accumulated session before it runs; an erroring entry prints diagnostics and is
    // skipped (and commits nothing — `check_entry` is transactional). Toggleable at the prompt.
    // A `--load` bootstrap replaces the fresh checker with one seeded from the bootstrap's own
    // whole-program check, so entries check against everything the bootstrap declared.
    let mut checker: Option<noeta_check::SessionChecker> =
        check.then(noeta_check::SessionChecker::new);
    // A bootstrapped session ("tinker"): the file runs to completion as entry 0 — checked,
    // imports resolved — and the prompt opens over its final state. Its sources seed the entry
    // list so later `SourceId`s continue past them (a trace into a bootstrap function renders
    // against its real text).
    let (mut session, preloaded_sources) = match &load {
        None => (VmSession::new(real_repl_env()), Vec::new()),
        Some(path) => match repl_bootstrap(path, &mut checker, resolved) {
            Ok(booted) => booted,
            Err(code) => return code,
        },
    };
    // Whether SITE-DRIVEN codegen is still sound (session-checker C5): true only while the checker
    // has seen every entry of the session. `:check off` clears it PERMANENTLY — precise destructor
    // relevance derived from a registry that missed an unchecked entry's `destruct` class could
    // skip a destructor, so once any entry runs unchecked the session stays on conservative
    // codegen even if checking is turned back on (diagnostics return; the codegen upgrade doesn't).
    let mut precise_codegen = check;
    let mut buffer = String::new();
    // Each evaluated entry is parsed with a **distinct** `SourceId` (its index here) and kept, so a
    // stack trace into a function defined in an *earlier* entry renders against that entry's real text
    // and line — rather than degrading to name-only, as it did when every entry reused
    // `SourceId::FIRST` (REPL-on-VM follow-on). Only entries that actually run are kept; a syntax-error
    // entry compiles nothing, so no future trace can reference it.
    let mut sources: Vec<Source> = preloaded_sources;
    eprint!("noeta repl — type a statement, Ctrl-D to exit\n» ");
    let _ = io::stderr().flush();

    eprintln!("type :help for commands");
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        // Skip blank lines when nothing is pending.
        if buffer.is_empty() && line.trim().is_empty() {
            eprint!("» ");
            let _ = io::stderr().flush();
            continue;
        }
        // A `:`-prefixed line (when nothing is pending) is a REPL meta-command — tooling that lives
        // outside the language grammar (`:type`, `:drop`, `:bindings`, `:reset`, `:help`, `:quit`).
        if buffer.is_empty() && line.trim_start().starts_with(':') {
            if repl_meta(
                &mut session,
                &mut checker,
                &mut precise_codegen,
                line.trim(),
                &sources,
            ) == MetaOutcome::Quit
            {
                break;
            }
            eprint!("» ");
            let _ = io::stderr().flush();
            continue;
        }
        if !buffer.is_empty() {
            buffer.push('\n');
        }
        buffer.push_str(&line);

        match repl_step(
            &mut session,
            &mut checker,
            precise_codegen,
            &buffer,
            &mut sources,
            edition,
        ) {
            ReplStep::Consumed => {
                buffer.clear();
                eprint!("» ");
            }
            // Keep the buffer and read another line; show a continuation prompt.
            ReplStep::Incomplete => eprint!("… "),
        }
        let _ = io::stderr().flush();
    }
    eprintln!();
    ExitCode::SUCCESS
}

/// Whether a meta-command asked to leave the REPL.
#[derive(PartialEq)]
pub(crate) enum MetaOutcome {
    Continue,
    Quit,
}

/// Handle a `:`-prefixed REPL meta-command. These are REPL *tooling*, deliberately outside the
/// language grammar (the language itself has no manual `drop`/`type` keyword): the REPL keeps
/// top-level bindings alive across entries — extended lifetime, unlike compiled code's last-use
/// destruction — so `:drop` is how a destructor is observed or an object reclaimed interactively,
/// and `:type` reports a value's runtime type in a session that runs no checker.
pub(crate) fn repl_meta(
    session: &mut VmSession,
    checker: &mut Option<noeta_check::SessionChecker>,
    precise_codegen: &mut bool,
    line: &str,
    sources: &[Source],
) -> MetaOutcome {
    let body = line.strip_prefix(':').unwrap_or(line);
    let mut parts = body.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    match cmd {
        "quit" | "q" => return MetaOutcome::Quit,
        "help" | "h" | "?" => print_repl_help(),
        "reset" => {
            session.reset();
            // The checker's session must reset with the runtime's — its registries describe
            // bindings/types that no longer exist. A reset session with checking on is fully
            // checked again, so precise codegen is earned back.
            if checker.is_some() {
                *checker = Some(noeta_check::SessionChecker::new());
                *precise_codegen = true;
            }
            eprintln!("(session reset)");
        }
        // Toggle per-entry type checking (session-checker C2). Turning it on mid-session starts
        // from an EMPTY typing environment: earlier (unchecked) bindings are simply unknown to it,
        // which the checker's unknown-ident tolerance treats as runtime-deferred — degraded
        // precision, never false errors.
        "check" => match arg {
            "on" => {
                if checker.is_none() {
                    *checker = Some(noeta_check::SessionChecker::new());
                }
                // Diagnostics return; the codegen upgrade does not — see `precise_codegen`.
                eprintln!("(type checking on — entries are checked before running)");
            }
            "off" => {
                *checker = None;
                *precise_codegen = false;
                eprintln!("(type checking off)");
            }
            _ => eprintln!(
                "type checking is {} — usage: :check on|off",
                if checker.is_some() { "on" } else { "off" }
            ),
        },
        "bindings" | "b" => {
            let names = session.binding_names();
            if names.is_empty() {
                eprintln!("(no bindings)");
            } else {
                println!("{}", names.join(", "));
                let _ = io::stdout().flush();
            }
        }
        "drop" | "free" => {
            if arg.is_empty() {
                eprintln!("usage: :drop <name>");
            } else {
                let (found, out) = session.drop_binding(arg);
                print!("{}", out.stdout);
                let _ = io::stdout().flush();
                eprint!("{}", out.stderr);
                let _ = io::stderr().flush();
                if found {
                    eprintln!("(dropped `{arg}`)");
                } else {
                    eprintln!("no binding named `{arg}`");
                }
            }
        }
        "type" | "t" => {
            if arg.is_empty() {
                eprintln!("usage: :type <expr>");
            } else {
                repl_type(session, arg, sources);
            }
        }
        other => eprintln!("unknown command `:{other}` — try :help"),
    }
    MetaOutcome::Continue
}

/// `:type <expr>` — parse `expr`, evaluate it in the session, and print its runtime type. Evaluating
/// the expression may abort (`:type boom()`); the trace then resolves against every entry's source, so
/// a `:type` id (the next index) is added to the render map without being *persisted* — a `:type`
/// query defines nothing, so no later trace can reference it.
pub(crate) fn repl_type(session: &mut VmSession, expr: &str, sources: &[Source]) {
    let id = SourceId(sources.len() as u32);
    let fragment = parse_fragment(id, "<repl-type>", expr);
    if !fragment.diagnostics.is_empty() {
        emit_diagnostics(&fragment.source, fragment.diagnostics.iter());
        return;
    }
    let out = session.type_of(&fragment.program);
    print!("{}", out.stdout);
    let _ = io::stdout().flush();
    eprint!("{}", out.stderr);
    let _ = io::stderr().flush();
    // Render diagnostics / any abort trace against all entries plus this `:type` source.
    let mut map_sources = sources.to_vec();
    map_sources.push(fragment.source);
    let map = SourceMap::new(map_sources);
    if !out.diagnostics.is_empty() {
        emit_diagnostics_mapped(&map, out.diagnostics.iter());
        emit_trace(&out.trace, &map);
    } else if let Some(ty) = out.value {
        println!("{ty}");
        let _ = io::stdout().flush();
    }
}

pub(crate) fn print_repl_help() {
    eprintln!("REPL commands:");
    eprintln!("  :type <expr>   show the runtime type of an expression (evaluates it)");
    eprintln!("  :drop <name>   run a binding's destructor now and unbind it (alias :free)");
    eprintln!("  :bindings      list the live bindings");
    eprintln!("  :reset         clear all bindings and start fresh");
    eprintln!("  :check on|off  type-check entries before running them (skip on error)");
    eprintln!("  :help          show this help");
    eprintln!("  :quit          exit the REPL (or Ctrl-D)");
}

/// Evaluate one checked-clean entry: with the checker on AND every prior entry checked
/// (`precise_codegen`), the entry compiles with the checker's accumulated site bundle — the same
/// site-driven codegen the file pipeline runs (session-checker C5); otherwise the conservative
/// checkerless codegen, which is always sound.
pub(crate) fn eval_entry(
    session: &mut VmSession,
    checker: &Option<noeta_check::SessionChecker>,
    precise_codegen: bool,
    program: &noeta_ast::Program,
) -> noeta_vm::SessionOutput {
    match checker {
        Some(checker) if precise_codegen => {
            session.eval_checked(program, &checker.sites_snapshot())
        }
        _ => session.eval(program),
    }
}

/// The `--check` gate (session-checker C2): type-check one parsed entry against the accumulated
/// session. Returns whether the entry should RUN — `true` with no checker, or when the entry has
/// no error-severity diagnostics (warnings print and the entry still runs). Errors render against
/// the entry's own source and the entry is skipped; `check_entry`'s transactionality means a
/// skipped entry left no trace in the checker either.
pub(crate) fn check_entry_gate(
    checker: &mut Option<noeta_check::SessionChecker>,
    program: &noeta_ast::Program,
    source: &Source,
) -> bool {
    let Some(checker) = checker.as_mut() else {
        return true;
    };
    let diagnostics = checker.check_entry(program);
    if diagnostics.is_empty() {
        return true;
    }
    emit_diagnostics(source, diagnostics.iter());
    !diagnostics
        .iter()
        .any(|d| d.severity == noeta_diagnostics::Severity::Error)
}

/// Try to evaluate the accumulated REPL buffer. Statements ending in `;`/`}` evaluate as-is;
/// a bare expression (no trailing `;`) is retried with a `;` appended so its value can be
/// printed. If the only parse problem is hitting end-of-input, the entry is treated as
/// incomplete and more input is requested (multiline). Any other error is reported, and the
/// buffer is reset so one bad entry cannot wedge the session.
///
/// With a `checker` present (`--check` / `:check on`, session-checker C2), a parsed entry is
/// type-checked against the accumulated session first: an entry with errors prints its `E0xxx`
/// diagnostics (rendered against the entry's own source) and is **skipped** — and `check_entry`'s
/// transactionality means it commits nothing, so the checker stays aligned with what actually ran.
/// Warning-only entries print the warnings and run.
pub(crate) fn repl_step(
    session: &mut VmSession,
    checker: &mut Option<noeta_check::SessionChecker>,
    precise_codegen: bool,
    buffer: &str,
    sources: &mut Vec<Source>,
    edition: noeta_lexer::Edition,
) -> ReplStep {
    // The next evaluated entry's `SourceId` is its index in the persistent `sources` vector.
    let id = SourceId(sources.len() as u32);
    let source = Source::new(id, format!("<repl:{}>", sources.len()), buffer.to_string());
    // Prompt entries lex/parse under the enclosing package's edition (editions arc) — the same
    // edition a `--load` bootstrap checked and compiled under, so an entry can't parse under
    // different rules than the program it extends.
    let lexed = noeta_lexer::lex_in(&source, edition, &noeta_lexer::TextTiers::default());
    let parsed = noeta_parser::parse_in(
        &source,
        &lexed.tokens,
        edition,
        &noeta_lexer::TextTiers::default(),
    );
    let diags: Vec<Diagnostic> = lexed
        .diagnostics
        .iter()
        .chain(parsed.diagnostics.iter())
        .cloned()
        .collect();

    if diags.is_empty() {
        if !check_entry_gate(checker, &parsed.program, &source) {
            return ReplStep::Consumed;
        }
        sources.push(source);
        let out = eval_entry(session, checker, precise_codegen, &parsed.program);
        emit_session(sources, out);
        return ReplStep::Consumed;
    }

    // A bare expression needs a terminating `;`; retry with one appended (same id — only one of the
    // two sources is ever kept, whichever compiled).
    let psource = Source::new(
        id,
        format!("<repl:{}>", sources.len()),
        format!("{buffer};"),
    );
    let plexed = lex(&psource);
    let pparsed = parse(&psource, &plexed.tokens);
    if plexed.diagnostics.is_empty() && pparsed.diagnostics.is_empty() {
        if !check_entry_gate(checker, &pparsed.program, &psource) {
            return ReplStep::Consumed;
        }
        sources.push(psource);
        let out = eval_entry(session, checker, precise_codegen, &pparsed.program);
        emit_session(sources, out);
        return ReplStep::Consumed;
    }

    // An entry with unclosed `(`/`{`/`[` is a multi-line definition still being typed (a `class`,
    // a `fn` body, a multi-line list/object literal). The parser may report a *non*-end-of-input
    // error inside such a buffer rather than cleanly running out of tokens, so the end-of-input
    // check below is not enough on its own — gather more lines until the delimiters balance. The
    // count is over lexer tokens, so braces inside string/template literals (a single token) and
    // `${…}` interpolation never miscount.
    if unclosed_delimiters(&lexed.tokens) {
        return ReplStep::Incomplete;
    }

    // Only end-of-input errors → the entry is unfinished; gather more lines.
    if diags
        .iter()
        .all(|d| d.code == DiagnosticCode::UnexpectedEndOfInput)
    {
        return ReplStep::Incomplete;
    }

    // A genuine syntax error: report it against the original buffer and reset. The entry compiled
    // nothing, so its source is *not* kept — its id is reused by the next entry.
    emit_diagnostics(&source, diags.iter());
    ReplStep::Consumed
}

/// Whether `tokens` has more opening than closing delimiters — i.e. a `(`/`{`/`[` left unclosed, the
/// signature of a multi-line REPL entry still being typed. A single net depth across all three kinds
/// is enough to decide *incompleteness* (the parser validates correct nesting once the buffer is
/// balanced); a buffer that closes more than it opens (net ≤ 0) is left to the parser to report.
pub(crate) fn unclosed_delimiters(tokens: &[noeta_lexer::Token]) -> bool {
    let mut depth: i32 = 0;
    for token in tokens {
        match token.kind {
            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => depth -= 1,
            _ => {}
        }
    }
    depth > 0
}

/// Print a session evaluation's stdout, the value of a trailing bare expression (if any), then any
/// diagnostics and abort trace. `sources` holds every evaluated entry keyed by `SourceId`, so a
/// diagnostic or trace frame from a function defined in an earlier entry renders against that entry's
/// real file and line.
pub(crate) fn emit_session(sources: &[Source], out: SessionOutput) {
    print!("{}", out.stdout);
    if let Some(value) = out.value {
        println!("{value}");
    }
    let _ = io::stdout().flush();
    // The entry's stderr stream (`std.io`'s `err`/`errln`) to real stderr, after stdout flushes.
    eprint!("{}", out.stderr);
    let _ = io::stderr().flush();
    let map = SourceMap::new(sources.to_vec());
    emit_diagnostics_mapped(&map, out.diagnostics.iter());
    emit_trace(&out.trace, &map);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<noeta_lexer::Token> {
        lex(&Source::new(SourceId::FIRST, "<t>", src.to_string())).tokens
    }

    #[test]
    fn unclosed_delimiters_detects_in_progress_multiline_entries() {
        // Open `{`/`(`/`[` with no match → still being typed (a `class`, `fn` body, or literal).
        assert!(unclosed_delimiters(&toks("class Res {")));
        assert!(unclosed_delimiters(&toks(
            "fn run(): void {\n  mut r = Res.new(3);"
        )));
        assert!(unclosed_delimiters(&toks("[1,\n 2,")));
        assert!(unclosed_delimiters(&toks("f(")));
        // Balanced (or over-closed) → let the parser decide, not "incomplete".
        assert!(!unclosed_delimiters(&toks("class Res { id: int }")));
        assert!(!unclosed_delimiters(&toks("[1, 2, 3]")));
        assert!(!unclosed_delimiters(&toks("x = 5;")));
        assert!(!unclosed_delimiters(&toks("}")));
        // Braces inside a template string are one token — they never miscount.
        assert!(!unclosed_delimiters(&toks("echo \"drop ${id}\";")));
    }
}
