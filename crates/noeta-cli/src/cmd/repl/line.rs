//! The interactive `noeta repl` prompt: raw-mode line editing, persistent history, in-place syntax
//! colouring, and TAB completion — engaged only when the prompt is attached to a real terminal.
//!
//! The design rule here is that this module owns **terminal mechanics and nothing else**. Every
//! question that is about the *language* is forwarded to the component that already answers it for
//! the editor and the toolchain:
//!
//! - *What colour is this token?* → [`noeta_ide::highlight::highlight_code_bytes`], which classifies
//!   with the compiler's own lexer. The docs browser asks the same function.
//! - *What completes at the cursor?* → the IDE engine's [`DocumentStore`], the same one the LSP's
//!   `textDocument/completion` and the MCP `completions` tool drive.
//! - *Is this entry finished, or is the user mid-block?* → the prompt's own
//!   [`super::parse_entry`], so the editor and the evaluator cannot disagree about where an entry
//!   ends.
//!
//! That is deliberate: a second grammar here — a regex highlighter, a bracket counter, a keyword
//! list — would be a copy of a rule that already exists, and copies drift. The TextMate grammar and
//! the tree-sitter grammar stay where they belong (the editor); neither is linked into the CLI.
//!
//! A pipe never reaches this module. `noeta repl < script.noe`, a test driving stdin, and a
//! `--no-default-features` build all take the plain reader in the parent module, whose behaviour is
//! unchanged.

use std::borrow::Cow;
use std::cell::RefCell;
use std::ffi::OsStr;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;

use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::history::FileHistory;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{CompletionType, Config, Context, Editor, Helper};

use noeta_ide::highlight::{HlClass, highlight_code_bytes};
use noeta_ide::{DocumentStore, Encoding, LineIndex};

use super::{Feed, META_COMMANDS, ReplState, buffer_incomplete};

/// Whether the prompt should open the line editor rather than the plain reader.
///
/// Both ends must be a terminal: stdin because raw mode is meaningless on a pipe, and stderr
/// because that is where the prompt, the redrawn line and the completion list are written. A
/// process with a terminal on one and a file on the other is being scripted, and gets the reader.
pub(crate) fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Whether to emit colour — the same question, and the same answer, that a diagnostic printed to
/// this prompt's stderr gets.
///
/// [`noeta_diagnostics::stderr_color`] owns the rule (`--color`, `CLICOLOR_FORCE`, `NO_COLOR`,
/// `TERM=dumb`). This module used to carry its own copy of the environment half, which was one rule
/// spelled twice: a prompt that highlighted `1 + 1` in colour while the type error underneath it
/// came out plain is the drift that shape produces. The editor only opens when stderr is a
/// terminal, so the destination half agrees by construction.
fn colour_enabled() -> bool {
    noeta_diagnostics::stderr_color()
}

// The SGR codes for the highlighter's classes. Deliberately the basic 8/16-colour set rather than
// 256-colour or truecolor: those are the ones every terminal and every user theme remaps, so the
// prompt inherits the palette the user already chose instead of imposing one.
const RESET: &str = "\x1b[0m";

fn sgr(class: HlClass) -> &'static str {
    match class {
        HlClass::Keyword => "\x1b[35m",   // magenta
        HlClass::Type => "\x1b[33m",      // yellow
        HlClass::Function => "\x1b[34m",  // blue
        HlClass::String => "\x1b[32m",    // green
        HlClass::Number => "\x1b[36m",    // cyan
        HlClass::Comment => "\x1b[90m",   // bright black — muted, like prose
        HlClass::Decorator => "\x1b[95m", // bright magenta — a keyword's louder cousin
    }
}

/// What the helper needs to know about the live session. Refreshed after every entry.
///
/// The helper lives *inside* the editor, which `readline` borrows mutably for the whole time a line
/// is being typed — so it cannot reach into [`ReplState`] while the user is at the prompt. This is
/// the snapshot it reads instead.
#[derive(Default)]
struct PromptView {
    /// Every entry evaluated so far, concatenated (and newline-terminated). Completion resolves
    /// against this as one document, so a `fn`, `class` or `use` written at the prompt completes
    /// afterwards exactly like one written in a file.
    session_text: String,
    /// The live runtime bindings. The session is the ground truth for what is actually bound — an
    /// entry that ran with `:check off` left no typing environment behind, and `:drop`ping a name
    /// unbinds it without rewriting the text it was bound in.
    bindings: Vec<String>,
}

impl PromptView {
    fn refresh(&mut self, state: &ReplState) {
        self.session_text.clear();
        for source in &state.sources {
            self.session_text.push_str(source.text());
            if !self.session_text.ends_with('\n') {
                self.session_text.push('\n');
            }
        }
        self.bindings = state.session.binding_names();
    }
}

struct NoetaHelper {
    view: Rc<RefCell<PromptView>>,
    /// The IDE engine, kept across completions so salsa can reuse what it already computed for the
    /// unchanged prefix of the session rather than rebuilding it on every TAB.
    store: RefCell<DocumentStore>,
    /// The edition prompt entries parse under — the validator's, and the same one
    /// [`ReplState::feed`] evaluates with.
    edition: noeta_lexer::Edition,
    colour: bool,
}

/// The URI the prompt's virtual document is opened under. Not a real path: the IDE engine keys
/// documents by string, and nothing about completion requires the text to exist on disk.
const PROMPT_URI: &str = "noeta-repl:///prompt.noe";

impl NoetaHelper {
    /// The word being completed: the run of identifier characters ending at `pos`. A `.`, `@`, `(`
    /// or space ends it, which is what makes member completion (`x.ge|`) and directive completion
    /// (`@te|`) offer bare names — the sigil stays in the line and only the name is replaced.
    fn word_start(line: &str, pos: usize) -> usize {
        line[..pos]
            .char_indices()
            .rev()
            .find(|(_, c)| !c.is_alphanumeric() && *c != '_')
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0)
    }

    /// Completion for a `:`-meta line: the command names, or the argument vocabulary of the command
    /// already typed. Read off [`META_COMMANDS`], the same table `:help` prints.
    fn complete_meta(&self, line: &str, pos: usize) -> (usize, Vec<Pair>) {
        // The `:` is not necessarily at byte 0 — the line may be indented, and `repl_meta` trims
        // before dispatching, so an indented meta-command is a real one. Found rather than assumed:
        // a fixed `line[1..]` would misread every indented line, and would *panic* on one indented
        // with a multi-byte space (U+00A0 is whitespace, so `trim_start` accepts it, and byte 1
        // lands inside it).
        let Some(colon) = line.find(':') else {
            return (pos, Vec::new());
        };
        let name = colon + 1;
        if pos < name {
            return (pos, Vec::new());
        }
        let mut parts = line[name..pos].splitn(2, char::is_whitespace);
        let head = parts.next().unwrap_or("");
        let Some(rest) = parts.next() else {
            // Still typing the command name — offer the canonical spellings (not the aliases: they
            // exist to be typed quickly, and listing both halves of every pair is noise).
            let names = META_COMMANDS
                .iter()
                .filter(|c| c.name.starts_with(head))
                .map(|c| pair(c.name))
                .collect();
            return (name, names);
        };

        let Some(command) = META_COMMANDS
            .iter()
            .find(|c| c.name == head || c.aliases.contains(&head))
        else {
            return (pos, Vec::new());
        };
        let prefix = rest.rsplit(char::is_whitespace).next().unwrap_or("");
        let start = pos - prefix.len();
        let mut candidates: Vec<Pair> = command
            .arg_words
            .iter()
            .filter(|w| w.starts_with(prefix))
            .map(|w| pair(w))
            .collect();
        if command.completes_bindings {
            candidates.extend(
                self.view
                    .borrow()
                    .bindings
                    .iter()
                    .filter(|b| b.starts_with(prefix))
                    .map(|b| pair(b)),
            );
        }
        (start, candidates)
    }

    /// Completion for a Noeta entry: the IDE engine's answer at the cursor, over a virtual document
    /// of the whole session so far plus the line being typed.
    ///
    /// The session prefix is what makes this worth doing. Asking about the bare line would know
    /// nothing about a type declared three entries ago; asking about the session knows the same
    /// things an editor would know about a file containing all of it.
    fn complete_code(&self, line: &str, pos: usize) -> (usize, Vec<Pair>) {
        let start = Self::word_start(line, pos);
        let prefix = &line[start..pos];

        let document = {
            let view = self.view.borrow();
            format!("{}{}", view.session_text, line)
        };
        let offset = document.len() - line.len() + pos;
        let position = LineIndex::new(&document).position(offset as u32, Encoding::Utf8);

        let mut store = self.store.borrow_mut();
        store.open(PROMPT_URI, document);
        let candidates = store
            .completions(PROMPT_URI, position, Encoding::Utf8)
            .unwrap_or_default();

        // The engine answers for the *position*, leaving prefix filtering to the client — an LSP
        // client does it as you type. rustyline does not: whatever is returned is what gets
        // inserted, so the filtering has to happen here.
        let mut pairs: Vec<Pair> = candidates
            .into_iter()
            .filter(|c| c.label.starts_with(prefix))
            .map(|c| Pair {
                display: c.label.clone(),
                replacement: c.insert_text.unwrap_or(c.label),
            })
            .collect();

        // Live runtime bindings, which no reconstruction of the text can be trusted to cover (see
        // [`PromptView::bindings`]). Only in a plain identifier position — after a `.` or inside a
        // directive's parens a session binding is not what the user is reaching for, and the engine
        // deliberately returned a narrow set there.
        if !ends_with_member_access(line, start) {
            let view = self.view.borrow();
            for binding in view.bindings.iter().filter(|b| b.starts_with(prefix)) {
                if !pairs.iter().any(|p| p.replacement == *binding) {
                    pairs.push(pair(binding));
                }
            }
        }
        pairs.sort_by(|a, b| a.display.cmp(&b.display));
        (start, pairs)
    }
}

/// Whether the word starting at `start` is being written as a member (`recv.wor|`) or a directive
/// (`@wor|`) — positions where the session's value bindings are not candidates.
fn ends_with_member_access(line: &str, start: usize) -> bool {
    matches!(line[..start].chars().next_back(), Some('.') | Some('@'))
}

fn pair(text: &str) -> Pair {
    Pair {
        display: text.to_string(),
        replacement: text.to_string(),
    }
}

impl Highlighter for NoetaHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if !self.colour || line.is_empty() {
            return Cow::Borrowed(line);
        }
        // A `:`-meta line is not Noeta — lexing it would colour `:help` as a stray operator and an
        // identifier. Left plain, which reads correctly as "this is prompt tooling, not code".
        if line.trim_start().starts_with(':') {
            return Cow::Borrowed(line);
        }
        let spans = highlight_code_bytes(line);
        if spans.is_empty() {
            return Cow::Borrowed(line);
        }
        let mut out = String::with_capacity(line.len() + spans.len() * 9);
        let mut at = 0usize;
        for span in spans {
            let (start, end) = (span.start as usize, span.end as usize);
            // Defensive: the classifier's spans are sorted, non-overlapping, and on char
            // boundaries, but a malformed one must degrade to "no colour", never panic on a slice.
            if start < at || end > line.len() || !line.is_char_boundary(start) {
                continue;
            }
            out.push_str(&line[at..start]);
            out.push_str(sgr(span.class));
            out.push_str(&line[start..end]);
            out.push_str(RESET);
            at = end;
        }
        out.push_str(&line[at..]);
        Cow::Owned(out)
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        if self.colour {
            Cow::Owned(format!("\x1b[1;34m{prompt}{RESET}"))
        } else {
            Cow::Borrowed(prompt)
        }
    }

    /// Redraw on every keystroke. The default only redraws on a forced refresh, which would leave
    /// the colouring one character behind whatever was just typed.
    fn highlight_char(&self, _line: &str, _pos: usize, _kind: CmdKind) -> bool {
        self.colour
    }
}

impl Completer for NoetaHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        if line.trim_start().starts_with(':') && !line.trim_start().is_empty() {
            return Ok(self.complete_meta(line, pos));
        }
        Ok(self.complete_code(line, pos))
    }
}

impl Validator for NoetaHelper {
    fn validate(&self, ctx: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        let input = ctx.input();
        // A meta-command is not Noeta and is always one line — asking the parser about `:help`
        // would be asking the wrong question.
        if input.trim().is_empty() || input.trim_start().starts_with(':') {
            return Ok(ValidationResult::Valid(None));
        }
        // The *same* verdict the evaluator reaches, from the same function. Enter on an unfinished
        // `class`/`fn` body opens a new line inside the entry instead of submitting it.
        if buffer_incomplete(input, self.edition) {
            return Ok(ValidationResult::Incomplete);
        }
        Ok(ValidationResult::Valid(None))
    }
}

impl Hinter for NoetaHelper {
    type Hint = String;
}

impl Helper for NoetaHelper {}

/// Where the prompt's history is kept. `NOETA_REPL_HISTORY` overrides it outright (which is how the
/// tests keep a test run out of the developer's real history); otherwise it is the XDG state
/// directory, the spec's home for data that should survive a reboot but is not worth backing up.
///
/// `None` — no home directory to resolve — simply means the session has no persistent history.
fn history_path() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    history_path_from(
        std::env::var_os("NOETA_REPL_HISTORY").as_deref(),
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os(var).as_deref(),
    )
}

/// [`history_path`]'s decision, over the three variables it reads — split out for the same reason
/// [`colour_from`] is.
fn history_path_from(
    explicit: Option<&OsStr>,
    xdg_state: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<PathBuf> {
    if let Some(explicit) = explicit.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(explicit));
    }
    let state = match xdg_state.filter(|v| !v.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(home.filter(|v| !v.is_empty())?)
            .join(".local")
            .join("state"),
    };
    Some(state.join("noeta").join("repl-history"))
}

/// Drive the prompt through the line editor. `None` means the editor could not be started (no
/// termios, a terminal the backend cannot drive) and the caller should fall through to the plain
/// reader — a REPL that refuses to open because it could not colour itself would be a bad trade.
pub(crate) fn run(state: &mut ReplState) -> Option<ExitCode> {
    let view = Rc::new(RefCell::new(PromptView::default()));
    view.borrow_mut().refresh(state);

    let config = Config::builder()
        .auto_add_history(true)
        .history_ignore_dups(true)
        .ok()?
        // A list beats cycling at a prompt: the candidates for `x.` are the receiver's whole
        // surface, and reading them is the point.
        .completion_type(CompletionType::List)
        .build();
    let mut editor: Editor<NoetaHelper, FileHistory> = Editor::with_config(config).ok()?;
    editor.set_helper(Some(NoetaHelper {
        view: Rc::clone(&view),
        store: RefCell::new(DocumentStore::default()),
        edition: state.edition,
        colour: colour_enabled(),
    }));

    let history = history_path();
    if let Some(path) = &history {
        // A missing history file is the first run, not an error.
        let _ = editor.load_history(path);
    }

    eprintln!("noeta repl — type a statement, Ctrl-D to exit");
    eprintln!("type :help for commands, TAB to complete");

    let mut prompt = "» ";
    loop {
        match editor.readline(prompt) {
            Ok(line) => match state.feed(&line) {
                Feed::Quit => break,
                Feed::Ready => prompt = "» ",
                // The validator gathers continuation lines itself, so a complete entry is what
                // arrives here. This is the safety net for the case where it did not: keep the
                // partial buffer and ask for the rest, exactly as the piped reader does.
                Feed::Continue => prompt = "… ",
            },
            // Ctrl-C abandons the entry being typed, and only that — the session survives.
            Err(ReadlineError::Interrupted) => {
                state.buffer.clear();
                prompt = "» ";
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(err) => {
                eprintln!("noeta: {err}");
                break;
            }
        }
        view.borrow_mut().refresh(state);
    }
    eprintln!();

    if let Some(path) = &history {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Append so two concurrent prompts do not truncate each other's history; a first run has
        // no file to append to, so fall back to writing one.
        if editor.append_history(path).is_err() {
            let _ = editor.save_history(path);
        }
    }
    Some(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_start_finds_the_identifier_under_the_cursor() {
        // A bare identifier: the whole word.
        assert_eq!(NoetaHelper::word_start("greet", 5), 0);
        // A member access: only the member, so the receiver and the `.` stay in the line.
        assert_eq!(NoetaHelper::word_start("todo.ins", 8), 5);
        // A directive: only the name, so the `@` is not doubled when the candidate is inserted.
        assert_eq!(NoetaHelper::word_start("@te", 3), 1);
        // After an opening paren, and after a space.
        assert_eq!(NoetaHelper::word_start("f(ar", 4), 2);
        assert_eq!(NoetaHelper::word_start("mut x = va", 10), 8);
        // Nothing to complete yet — the cursor sits on a boundary.
        assert_eq!(NoetaHelper::word_start("todo.", 5), 5);
    }

    #[test]
    fn word_start_is_char_boundary_safe() {
        // A multi-byte identifier character must not split: `π` is alphanumeric and 2 bytes.
        let line = "mut π = 1";
        let pos = "mut π".len();
        let start = NoetaHelper::word_start(line, pos);
        assert_eq!(&line[start..pos], "π");
    }

    /// A helper with no session behind it — enough to exercise the meta-command completion, which
    /// reads only [`META_COMMANDS`] and the (here empty) binding list.
    fn bare_helper() -> NoetaHelper {
        NoetaHelper {
            view: Rc::new(RefCell::new(PromptView::default())),
            store: RefCell::new(DocumentStore::default()),
            edition: noeta_lexer::Edition::default(),
            colour: false,
        }
    }

    #[test]
    fn meta_completion_offers_the_command_names() {
        let helper = bare_helper();
        let (start, candidates) = helper.complete_meta(":", 1);
        assert_eq!(start, 1, "the name replaces everything after the colon");
        let names: Vec<&str> = candidates.iter().map(|p| p.replacement.as_str()).collect();
        assert_eq!(
            names,
            vec!["type", "drop", "bindings", "reset", "check", "help", "quit"]
        );
        // A partial name filters.
        let (_, candidates) = helper.complete_meta(":ch", 3);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].replacement, "check");
    }

    #[test]
    fn meta_completion_offers_a_commands_argument_words() {
        let helper = bare_helper();
        let (start, candidates) = helper.complete_meta(":check o", 8);
        assert_eq!(start, 7, "only the argument is replaced");
        let words: Vec<&str> = candidates.iter().map(|p| p.replacement.as_str()).collect();
        assert_eq!(words, vec!["on", "off"]);
        // An alias resolves to the same command.
        let (_, candidates) = helper.complete_meta(":t x", 4);
        assert!(
            candidates.is_empty(),
            "`:type` takes an expression, not a fixed word"
        );
    }

    #[test]
    fn meta_completion_survives_an_indented_line() {
        // `repl_meta` trims before dispatching, so an indented `:help` is a real meta-command and
        // completion has to read it as one. The multi-byte case is the one that used to panic:
        // U+00A0 is whitespace, so `trim_start` accepts it and the line reaches this function with
        // a two-byte character where a fixed `line[1..]` slice would split it.
        let helper = bare_helper();
        let (start, candidates) = helper.complete_meta("   :qu", 6);
        assert_eq!(start, 4);
        assert_eq!(candidates[0].replacement, "quit");
        let line = "\u{a0}:qu";
        let (start, candidates) = helper.complete_meta(line, line.len());
        assert_eq!(start, "\u{a0}:".len());
        assert_eq!(candidates[0].replacement, "quit");
    }

    #[test]
    fn meta_completion_of_an_unknown_command_offers_nothing() {
        let helper = bare_helper();
        let (_, candidates) = helper.complete_meta(":nonsense x", 11);
        assert!(candidates.is_empty());
    }

    #[test]
    fn member_and_directive_positions_are_recognised() {
        assert!(ends_with_member_access("todo.ins", 5));
        assert!(ends_with_member_access("@te", 1));
        assert!(!ends_with_member_access("greet", 0));
        assert!(!ends_with_member_access("mut x = va", 8));
    }

    fn os(value: &str) -> &OsStr {
        OsStr::new(value)
    }

    #[test]
    fn history_path_honours_the_explicit_override() {
        // The override is what keeps a test run out of the developer's own history file — it wins
        // over both of the others.
        assert_eq!(
            history_path_from(
                Some(os("/tmp/noeta-test-history")),
                Some(os("/ignored")),
                Some(os("/home/dev"))
            ),
            Some(PathBuf::from("/tmp/noeta-test-history"))
        );
    }

    #[test]
    fn history_path_falls_back_to_the_xdg_state_directory() {
        assert_eq!(
            history_path_from(
                None,
                Some(os("/home/dev/.local/state")),
                Some(os("/home/dev"))
            ),
            Some(PathBuf::from("/home/dev/.local/state/noeta/repl-history"))
        );
        // No `XDG_STATE_HOME` — the spec's own default location under the home directory.
        assert_eq!(
            history_path_from(None, None, Some(os("/home/dev"))),
            Some(PathBuf::from("/home/dev/.local/state/noeta/repl-history"))
        );
        // Nowhere to put it: the session simply has no persistent history, and must not guess.
        assert_eq!(history_path_from(None, None, None), None);
        // An empty variable is not a path.
        assert_eq!(history_path_from(Some(os("")), None, Some(os(""))), None);
    }

    // The colour rule itself is tested where it now lives, in `noeta_diagnostics::color` — this
    // module holds no copy of it to test.
}
