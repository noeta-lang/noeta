//! `noeta repl` — the interactive session: per-entry lex/parse/check/eval over the VM
//! session, meta-commands, and the `--load` bootstrap ("tinker") path.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

/// The interactive line editor, engaged when the prompt has a terminal on both ends. Behind a
/// feature so a `--no-default-features` build stays free of the terminal stack; without it every
/// session takes the plain reader, which is what a pipe gets in either build.
#[cfg(feature = "repl-tty")]
mod line;

use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_lexer::TokenKind;
use noeta_parser::parse_fragment;
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
        resolved.map(|g| noeta_runner::compile::ResolvedFront {
            packages: g.packages,
            package_uses: g.package_uses,
        }),
    ) {
        Ok(loaded) => loaded,
        Err(failure) => return Err(failure.report()),
    };

    // Always checked (it is a file); the session flavor keeps the checker when the prompt wants it.
    let (checked, session_checker) = loaded.check_session();
    // Report, then gate on errors only — the same rule the prompt's own `check_entry_gate` already
    // applies to each typed entry, now applied to the file it bootstraps from.
    emit_diagnostics_mapped(
        &loaded.sources,
        loaded.warnings.iter().chain(checked.diagnostics.iter()),
    );
    if noeta_diagnostics::has_errors(&checked.diagnostics) {
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
        emit_diagnostics_mapped(&loaded.sources, out.diagnostics.iter());
    }
    if noeta_diagnostics::has_errors(&out.diagnostics) {
        // A bootstrap that aborts is a broken app context — fail fast, exactly like `noeta run`.
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
    let (session, preloaded_sources) = match &load {
        None => (VmSession::new(real_repl_env()), Vec::new()),
        Some(path) => match repl_bootstrap(path, &mut checker, resolved) {
            Ok(booted) => booted,
            Err(code) => return code,
        },
    };
    // `precise_codegen`: whether SITE-DRIVEN codegen is still sound (session-checker C5) — true only
    // while the checker has seen every entry of the session. `:check off` clears it PERMANENTLY —
    // precise destructor relevance derived from a registry that missed an unchecked entry's
    // `destruct` class could skip a destructor, so once any entry runs unchecked the session stays
    // on conservative codegen even if checking is turned back on (diagnostics return; the codegen
    // upgrade doesn't).
    //
    // `sources`: each evaluated entry is parsed with a **distinct** `SourceId` (its index here) and
    // kept, so a stack trace into a function defined in an *earlier* entry renders against that
    // entry's real text and line — rather than degrading to name-only, as it did when every entry
    // reused `SourceId::FIRST` (REPL-on-VM follow-on). Only entries that actually run are kept; a
    // syntax-error entry compiles nothing, so no future trace can reference it.
    let mut state = ReplState {
        session,
        checker,
        precise_codegen: check,
        buffer: String::new(),
        sources: preloaded_sources,
        edition,
    };

    // An interactive terminal gets the line editor — history, in-place syntax colouring, and TAB
    // completion off the IDE engine. A pipe gets the plain reader below, unchanged: raw mode is
    // meaningless without a terminal, and a script piping entries in wants exactly the old
    // behaviour. Everything downstream of the read is the same `ReplState` either way.
    #[cfg(feature = "repl-tty")]
    if line::interactive()
        && let Some(code) = line::run(&mut state)
    {
        return code;
    }

    eprintln!("noeta repl — type a statement, Ctrl-D to exit");
    eprintln!("type :help for commands");
    eprint!("» ");
    let _ = io::stderr().flush();
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        match state.feed(&line) {
            Feed::Quit => break,
            Feed::Ready => eprint!("» "),
            // Keep the buffer and read another line; show a continuation prompt.
            Feed::Continue => eprint!("… "),
        }
        let _ = io::stderr().flush();
    }
    eprintln!();
    ExitCode::SUCCESS
}

/// The live prompt: the VM session, the optional per-entry checker, and the accumulated entry
/// sources every diagnostic and stack trace renders against.
///
/// Both input loops — the piped reader and the interactive line editor — drive this one state
/// through [`ReplState::feed`]. Nothing about what an entry *means* lives in either loop, so the
/// two cannot answer differently; they differ only in how a line is read.
pub(crate) struct ReplState {
    pub(crate) session: VmSession,
    pub(crate) checker: Option<noeta_check::SessionChecker>,
    /// Whether SITE-DRIVEN codegen is still sound (session-checker C5) — see [`cmd_repl`].
    pub(crate) precise_codegen: bool,
    /// The multi-line entry still being gathered. Only the piped reader accumulates here: the
    /// interactive editor gathers continuation lines itself (its validator asks
    /// [`buffer_incomplete`]) and feeds whole entries, so this stays empty there.
    pub(crate) buffer: String,
    pub(crate) sources: Vec<Source>,
    pub(crate) edition: noeta_lexer::Edition,
}

/// What the prompt should do after a line has been fed.
#[derive(PartialEq, Eq, Clone, Copy)]
pub(crate) enum Feed {
    /// Ready for a new entry (`» `).
    Ready,
    /// The entry is unfinished; the next line continues it (`… `).
    Continue,
    /// `:quit` — leave the prompt.
    Quit,
}

impl ReplState {
    /// Feed one line (or, from the interactive editor, one whole multi-line entry) to the session.
    pub(crate) fn feed(&mut self, line: &str) -> Feed {
        // Skip blank lines when nothing is pending.
        if self.buffer.is_empty() && line.trim().is_empty() {
            return Feed::Ready;
        }
        // A `:`-prefixed line (when nothing is pending) is a REPL meta-command — tooling that lives
        // outside the language grammar (`:type`, `:drop`, `:bindings`, `:reset`, `:help`, `:quit`).
        if self.buffer.is_empty() && line.trim_start().starts_with(':') {
            let outcome = repl_meta(
                &mut self.session,
                &mut self.checker,
                &mut self.precise_codegen,
                line.trim(),
                &self.sources,
            );
            return if outcome == MetaOutcome::Quit {
                Feed::Quit
            } else {
                Feed::Ready
            };
        }
        if !self.buffer.is_empty() {
            self.buffer.push('\n');
        }
        self.buffer.push_str(line);

        match repl_step(
            &mut self.session,
            &mut self.checker,
            self.precise_codegen,
            &self.buffer,
            &mut self.sources,
            self.edition,
        ) {
            ReplStep::Consumed => {
                self.buffer.clear();
                Feed::Ready
            }
            ReplStep::Incomplete => Feed::Continue,
        }
    }
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
    }
    // The type still prints when the expression merely warned — only an abort leaves nothing to
    // report a type for.
    if noeta_diagnostics::has_errors(&out.diagnostics) {
        emit_trace(&out.trace, &map);
    } else if let Some(ty) = out.value {
        println!("{ty}");
        let _ = io::stdout().flush();
    }
}

/// One `:`-meta command, as both the help screen and the prompt's TAB completion see it.
///
/// The completion-only fields go unread in a build without the interactive prompt; the table is
/// still the single description of the command set, which is the point of it.
#[cfg_attr(not(feature = "repl-tty"), allow(dead_code))]
pub(crate) struct MetaCommand {
    pub(crate) name: &'static str,
    /// Alternative spellings [`repl_meta`] also accepts.
    pub(crate) aliases: &'static [&'static str],
    /// The argument placeholder shown in help, `""` for a command that takes none.
    pub(crate) args: &'static str,
    pub(crate) help: &'static str,
    /// The fixed words this command's argument can be, for completion. Empty when the argument is
    /// open-ended (an expression) or comes from the session (a binding name — see
    /// [`MetaCommand::completes_bindings`]).
    pub(crate) arg_words: &'static [&'static str],
    /// Whether the argument is a live binding name, so completion offers `:bindings`.
    pub(crate) completes_bindings: bool,
}

/// The `:`-meta commands. The help screen and the interactive prompt's completion both read this
/// one table, so a command cannot be offered but undocumented, or documented but unofferable.
pub(crate) const META_COMMANDS: &[MetaCommand] = &[
    MetaCommand {
        name: "type",
        aliases: &["t"],
        args: "<expr>",
        help: "show the runtime type of an expression (evaluates it)",
        arg_words: &[],
        completes_bindings: true,
    },
    MetaCommand {
        name: "drop",
        aliases: &["free"],
        args: "<name>",
        help: "run a binding's destructor now and unbind it (alias :free)",
        arg_words: &[],
        completes_bindings: true,
    },
    MetaCommand {
        name: "bindings",
        aliases: &["b"],
        args: "",
        help: "list the live bindings",
        arg_words: &[],
        completes_bindings: false,
    },
    MetaCommand {
        name: "reset",
        aliases: &[],
        args: "",
        help: "clear all bindings and start fresh",
        arg_words: &[],
        completes_bindings: false,
    },
    MetaCommand {
        name: "check",
        aliases: &[],
        args: "on|off",
        help: "type-check entries before running them (skip on error)",
        arg_words: &["on", "off"],
        completes_bindings: false,
    },
    MetaCommand {
        name: "help",
        aliases: &["h", "?"],
        args: "",
        help: "show this help",
        arg_words: &[],
        completes_bindings: false,
    },
    MetaCommand {
        name: "quit",
        aliases: &["q"],
        args: "",
        help: "exit the REPL (or Ctrl-D)",
        arg_words: &[],
        completes_bindings: false,
    },
];

pub(crate) fn print_repl_help() {
    eprintln!("REPL commands:");
    // Width of the widest `:name <args>` so the help column lines up without a hand-counted table.
    let width = META_COMMANDS
        .iter()
        .map(|c| c.name.len() + c.args.len() + if c.args.is_empty() { 1 } else { 2 })
        .max()
        .unwrap_or(0);
    for command in META_COMMANDS {
        let spelling = format!(":{} {}", command.name, command.args);
        eprintln!("  {:width$} {}", spelling.trim_end(), command.help);
    }
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

/// What one accumulated prompt buffer turned out to be, before anything is checked or run.
///
/// This is the *whole* syntactic verdict on an entry, and deliberately the only place that verdict
/// is reached: [`repl_step`] evaluates on it, and the interactive line editor's validator asks it
/// whether to keep reading lines. A second implementation of "is this finished?" living beside the
/// editor is exactly how the two would drift — the editor would hand `repl_step` a buffer it thinks
/// is complete and `repl_step` would sit waiting for more.
pub(crate) enum Entry {
    /// Parsed. Carries the source that actually parsed — the buffer as typed, or the
    /// bare-expression retry with its terminating `;` appended.
    Parsed {
        source: Source,
        program: noeta_ast::Program,
    },
    /// Still being typed: unclosed delimiters, or nothing wrong but running out of input.
    Incomplete,
    /// A genuine syntax error, against the buffer as typed.
    Failed {
        source: Source,
        diags: Vec<Diagnostic>,
    },
}

/// Parse one accumulated prompt buffer. Statements ending in `;`/`}` parse as-is; a bare expression
/// (no trailing `;`) is retried with a `;` appended so its value can be printed. If the only parse
/// problem is hitting end-of-input — or the buffer has unclosed delimiters — the entry is
/// [`Entry::Incomplete`] and more input is wanted. Any other error is [`Entry::Failed`].
///
/// `id`/`name` identify the entry for diagnostics and stack traces; only one of the two parse
/// attempts is ever kept, so both are built with the same id.
pub(crate) fn parse_entry(
    id: SourceId,
    name: String,
    buffer: &str,
    edition: noeta_lexer::Edition,
) -> Entry {
    let source = Source::new(id, name.clone(), buffer.to_string());
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
        return Entry::Parsed {
            source,
            program: parsed.program,
        };
    }

    // A bare expression needs a terminating `;`; retry with one appended (same id — only one of the
    // two sources is ever kept, whichever parsed). The retry runs under the entry's edition too:
    // a dialect that changes how an *expression* lexes must not be silently dropped just because
    // the user left the semicolon off.
    let psource = Source::new(id, name, format!("{buffer};"));
    let plexed = noeta_lexer::lex_in(&psource, edition, &noeta_lexer::TextTiers::default());
    let pparsed = noeta_parser::parse_in(
        &psource,
        &plexed.tokens,
        edition,
        &noeta_lexer::TextTiers::default(),
    );
    if plexed.diagnostics.is_empty() && pparsed.diagnostics.is_empty() {
        return Entry::Parsed {
            source: psource,
            program: pparsed.program,
        };
    }

    // An entry with unclosed `(`/`{`/`[` is a multi-line definition still being typed (a `class`,
    // a `fn` body, a multi-line list/object literal). The parser may report a *non*-end-of-input
    // error inside such a buffer rather than cleanly running out of tokens, so the end-of-input
    // check below is not enough on its own — gather more lines until the delimiters balance. The
    // count is over lexer tokens, so braces inside string/template literals (a single token) and
    // `${…}` interpolation never miscount.
    if unclosed_delimiters(&lexed.tokens) {
        return Entry::Incomplete;
    }

    // Only end-of-input errors → the entry is unfinished; gather more lines.
    if diags
        .iter()
        .all(|d| d.code == DiagnosticCode::UnexpectedEndOfInput)
    {
        return Entry::Incomplete;
    }

    Entry::Failed { source, diags }
}

/// Whether `buffer` is an entry still being typed — the [`parse_entry`] verdict, for a caller that
/// only wants the yes/no. This is what the interactive editor's multi-line validator asks; without
/// that editor compiled in, nothing asks, because the piped reader reads [`parse_entry`] directly.
#[cfg_attr(not(feature = "repl-tty"), allow(dead_code))]
pub(crate) fn buffer_incomplete(buffer: &str, edition: noeta_lexer::Edition) -> bool {
    matches!(
        parse_entry(SourceId::FIRST, "<repl-validate>".into(), buffer, edition),
        Entry::Incomplete
    )
}

/// Try to evaluate the accumulated REPL buffer. The syntactic verdict is [`parse_entry`]'s; this is
/// the half that checks and runs. A genuine syntax error is reported and the buffer reset, so one
/// bad entry cannot wedge the session.
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
    match parse_entry(id, format!("<repl:{}>", sources.len()), buffer, edition) {
        Entry::Incomplete => ReplStep::Incomplete,
        // The entry compiled nothing, so its source is *not* kept — its id is reused by the next.
        Entry::Failed { source, diags } => {
            emit_diagnostics(&source, diags.iter());
            ReplStep::Consumed
        }
        Entry::Parsed { source, program } => {
            if !check_entry_gate(checker, &program, &source) {
                return ReplStep::Consumed;
            }
            sources.push(source);
            let out = eval_entry(session, checker, precise_codegen, &program);
            emit_session(sources, out);
            ReplStep::Consumed
        }
    }
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
    use noeta_lexer::lex;

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
