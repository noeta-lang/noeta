//! `noeta repl` driven through a real **pseudo-terminal**, which is the only way to reach the
//! interactive prompt at all: it opens exactly when stdin and stderr are both a terminal, so the
//! piped-stdin tests in [`super::repl`] — every other test of this command — take the plain reader
//! and cannot touch this path.
//!
//! What is being tested is the *seam*, not the line editor. The editor's own key handling is
//! rustyline's business; these tests check that the questions it asks are answered by the
//! toolchain's real engines — that Enter inside a `fn` body consults the same completeness rule the
//! evaluator does, that TAB reaches the IDE engine and sees the whole accumulated session, and that
//! the highlighter emits the colours the compiler's lexer classified.
//!
//! Unix only: a pty is a unix object, and so is the prompt's raw mode.

use crate::support::*;
use rexpect::reader::Options;
use rexpect::session::{PtySession, spawn_with_options};

/// Generous enough that a loaded CI box does not fail on timing, short enough that a genuinely
/// wedged prompt still reports rather than hanging the suite.
const TIMEOUT_MS: u64 = 30_000;

/// Spawn `noeta repl` on a pty. `strip_ansi` removes the escape sequences the redraw is made of, so
/// an assertion can read the text the user would see; the one colour test turns it off.
///
/// The history file is redirected into the per-target temp directory. That is not incidental
/// tidiness: without the override, running this suite would append every test's keystrokes to the
/// developer's own `~/.local/state/noeta/repl-history`.
fn repl_pty(name: &str, strip_ansi: bool) -> PtySession {
    let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("noeta"));
    command.arg("repl");
    command.env(
        "NOETA_CACHE_DIR",
        concat!(env!("CARGO_TARGET_TMPDIR"), "/noeta-cache"),
    );
    let history = history_path(name);
    // A leftover file from an earlier run would make a history assertion pass on stale content.
    let _ = std::fs::remove_file(&history);
    command.env("NOETA_REPL_HISTORY", history);
    // A known-capable terminal, and colour left on, so the prompt's own `NO_COLOR`/`TERM=dumb`
    // gates do not silently decide the outcome of the colour test.
    command.env("TERM", "xterm-256color");
    command.env_remove("NO_COLOR");
    spawn_with_options(
        command,
        Options::new()
            .timeout_ms(Some(TIMEOUT_MS))
            .strip_ansi_escape_codes(strip_ansi),
    )
    .expect("spawn the prompt on a pty")
}

fn history_path(name: &str) -> PathBuf {
    let dir = temp_root().join("noeta_repl_tty");
    std::fs::create_dir_all(&dir).expect("create the history fixture directory");
    dir.join(format!("history-{name}"))
}

/// Let the prompt re-arm before typing at it again.
///
/// This is not politeness, and it is not covering for a race in the prompt. `readline` reads
/// through a buffer it owns for the duration of one call, and whatever it has read past the Enter
/// goes with that buffer when the call returns — so a driver that writes every keystroke of a
/// session in one burst loses everything after the first Enter. A person never reaches that window:
/// they type slower than the turnaround, and a real multi-line paste arrives bracketed, which the
/// editor takes as a single entry (`\x1b[200~ … \x1b[201~`, one `readline`). A test writing at full
/// speed does, so it waits.
///
/// Only the *re-arm* is being waited for here — a few syscalls. Where an entry produces output, the
/// test waits for that output too, and that is what covers a slow box's evaluation time.
fn settle() {
    std::thread::sleep(std::time::Duration::from_millis(400));
}

/// Type `keys` and press Enter. Deliberately not `send_line`: that sends `\n`, and the key a
/// terminal actually delivers for the Enter key is `\r`.
fn enter(session: &mut PtySession, keys: &str) {
    settle();
    session.send(&format!("{keys}\r")).expect("send an entry");
    session.flush().expect("flush");
}

/// Type `keys` without pressing Enter — for the TAB tests, where the point is what the prompt does
/// before the line is submitted. Settles for the same reason [`enter`] does: these keys open a new
/// entry, so they land in the next `readline`'s buffer or nowhere.
fn typed(session: &mut PtySession, keys: &str) {
    settle();
    session.send(keys).expect("send keys");
    session.flush().expect("flush");
}

/// Leave the prompt and reap the process, so a test never leaves an orphan holding a pty.
fn quit(mut session: PtySession) {
    enter(&mut session, ":quit");
    let _ = session.exp_eof();
}

#[test]
fn a_terminal_opens_the_line_editor() {
    // The banner is the observable difference between the two input paths: the reader's does not
    // mention TAB, because without the editor there is no completion to advertise.
    let mut session = repl_pty("banner", true);
    session
        .exp_string("TAB to complete")
        .expect("the interactive banner");
    quit(session);
}

#[test]
fn an_unfinished_block_keeps_editing_instead_of_submitting() {
    // Enter on `fn dbl(…): int {` must NOT submit — the validator asks `parse_entry`, which reports
    // the entry as still being typed, exactly as it does for the piped reader.
    //
    // Asserting on the *result* would not test this: with no validator at all the three lines would
    // arrive as three entries and `feed` would reassemble them into the same block, because that is
    // exactly what it does for a pipe. The discriminator is the history, which records what the
    // editor treated as ONE entry — a multi-line entry lands on one line with its newlines escaped.
    let path = history_path("multiline");
    let mut session = repl_pty("multiline", true);
    session.exp_string("TAB to complete").expect("the banner");
    enter(&mut session, "fn dbl(n: int): int {");
    enter(&mut session, "  return n * 2;");
    enter(&mut session, "}");
    enter(&mut session, "dbl(21)");
    session.exp_string("42").expect("the block evaluated");
    quit(session);

    let written = std::fs::read_to_string(&path).expect("the history file was written");
    assert!(
        written
            .lines()
            .any(|entry| entry.contains("fn dbl") && entry.contains("return n * 2")),
        "the block is one history entry, so Enter kept editing it; got {written:?}"
    );
}

#[test]
fn tab_completes_a_declaration_from_an_earlier_entry() {
    // The completion document is the whole accumulated session, not the line being typed — so a
    // type declared in entry 0 completes in entry 1.
    //
    // A `class` specifically, and not a `fn`: a top-level `fn` is also a live *binding*, so the
    // prompt would complete its name from the session's binding list even with the IDE engine
    // returning nothing, and the test would pass without proving the engine was ever consulted. A
    // class name exists only in the reconstructed document.
    //
    // The assertion is likewise on a value that appears NOWHERE in the text typed: the pty echoes
    // every keystroke back, so expecting anything the test itself wrote would pass without the
    // prompt doing a thing. `6 * 7` is typed; `42` can only come from the field being read.
    let mut session = repl_pty("ident", true);
    session.exp_string("TAB to complete").expect("the banner");
    enter(&mut session, "class Widget { pub k: int }");
    typed(&mut session, "w = Wid\t");
    enter(&mut session, " { k: 6 * 7 }");
    enter(&mut session, "w.k");
    session
        .exp_string("42")
        .expect("`Wid` completed to `Widget` and the value constructed");
    quit(session);
}

#[test]
fn tab_after_a_dot_completes_the_receivers_member() {
    // Member completion resolves the receiver's type through the IDE engine — the same answer the
    // LSP gives an editor. Completing `bu` and then *calling* it is what makes this a real
    // assertion: `42` is nowhere in the typed text, so it can only come from `c.bump()` having been
    // spelled correctly by the prompt.
    let mut session = repl_pty("member", true);
    session.exp_string("TAB to complete").expect("the banner");
    enter(&mut session, "class Counter { pub n: int");
    enter(&mut session, "  pub fn bump(): int { return self.n + 1 }");
    enter(&mut session, "}");
    enter(&mut session, "c = Counter { n: 41 }");
    typed(&mut session, "c.bu\t");
    enter(&mut session, "()");
    session
        .exp_string("42")
        .expect("`c.bu` completed to `c.bump` and the call ran");
    quit(session);
}

#[test]
fn tab_completes_a_meta_commands_binding_argument() {
    // `:drop <name>` takes a live binding, so completion offers what the *session* is holding —
    // which is the only place that answer exists.
    let mut session = repl_pty("meta", true);
    session.exp_string("TAB to complete").expect("the banner");
    enter(&mut session, "counter = 7");
    typed(&mut session, ":drop coun\t");
    enter(&mut session, "");
    session
        .exp_string("dropped")
        .expect("`coun` completed to the live binding and it was dropped");
    quit(session);
}

#[test]
fn ctrl_c_abandons_the_entry_and_keeps_the_session() {
    // The half-typed entry goes; the bindings stay. A Ctrl-C that killed the session would lose
    // work that took the whole session to build up.
    let mut session = repl_pty("ctrlc", true);
    session.exp_string("TAB to complete").expect("the banner");
    enter(&mut session, "x = 41");
    typed(&mut session, "garbage(((");
    session.send_control('c').expect("abandon the entry");
    enter(&mut session, "x + 1");
    session
        .exp_string("42")
        .expect("the binding survived the interrupt");
    quit(session);
}

#[test]
fn the_line_is_syntax_coloured_as_it_is_typed() {
    // Escape codes left in: the assertion is precisely that they are emitted. `5` is a number, and
    // the classifier that says so is the compiler's own lexer, through
    // `noeta_ide::highlight::highlight_code_bytes`.
    let mut session = repl_pty("colour", false);
    session.exp_string("TAB to complete").expect("the banner");
    typed(&mut session, "x = 5");
    session
        .exp_string("\x1b[36m5")
        .expect("the literal is coloured as a number");
    session.send_control('c').expect("abandon the entry");
    quit(session);
}

#[test]
fn history_persists_to_the_configured_path() {
    let path = history_path("persist");
    let mut session = repl_pty("persist", true);
    session.exp_string("TAB to complete").expect("the banner");
    enter(&mut session, "remembered = 1");
    quit(session);
    let written = std::fs::read_to_string(&path).expect("the history file was written");
    assert!(
        written.contains("remembered = 1"),
        "the entry is in the history file; got {written:?}"
    );
}
