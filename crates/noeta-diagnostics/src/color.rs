//! Whether rendered output carries ANSI colour — the one place that rule is written down.
//!
//! Colour is a property of the **destination**, not of the diagnostic. The same rendered report
//! goes to a terminal (where colour helps), to a pipe (where it is noise a `grep` has to strip), to
//! a DAP output event and an MCP tool result (where it is corruption in a JSON string), and into an
//! HTTP body. So nothing here is decided globally for "diagnostics": a renderer is told, and the
//! sink is what knows.
//!
//! What *is* shared is the rule itself, and there is exactly one of it: [`ColorChoice::resolve`].
//! The REPL's line editor asks the same question about the same environment variables and gets its
//! answer from here, so a change to how `NO_COLOR` is read cannot land in one surface and miss the
//! other.

use std::ffi::OsStr;
use std::io::IsTerminal;
use std::sync::OnceLock;

/// What the user asked for, before it meets a destination — the three states of a `--color` flag.
///
/// [`Always`](ColorChoice::Always) and [`Never`](ColorChoice::Never) are explicit instructions and
/// override the environment; [`Auto`](ColorChoice::Auto) is the default and consults it. That
/// ordering is the ecosystem convention (cargo, ripgrep): `NO_COLOR` states a preference, and
/// naming the flag on the command line is how you say you mean otherwise this once.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ColorChoice {
    /// Colour when the destination is a terminal that has not asked to go without.
    #[default]
    Auto,
    /// Colour regardless of destination — for a pager, a CI log that renders ANSI, or a test that
    /// needs the coloured form without allocating a pty.
    Always,
    /// Never colour.
    Never,
}

impl ColorChoice {
    /// Resolve against a destination, reading the environment for [`Auto`](ColorChoice::Auto).
    pub fn resolve(self, is_terminal: bool) -> bool {
        self.resolve_with(
            is_terminal,
            std::env::var_os("NO_COLOR").as_deref(),
            std::env::var_os("CLICOLOR_FORCE").as_deref(),
            std::env::var_os("TERM").as_deref(),
        )
    }

    /// [`resolve`](ColorChoice::resolve) with the environment passed in — the decision as a pure
    /// function, which is the only way it can be tested: the workspace forbids `unsafe`, and
    /// mutating the process environment to exercise a branch needs exactly that.
    ///
    /// Under [`Auto`](ColorChoice::Auto), in order:
    ///
    /// 1. `CLICOLOR_FORCE` set to anything non-empty other than `0` forces colour on even off a
    ///    terminal. This is how a build system says "my log renders ANSI" — and how this crate's
    ///    own CLI tests reach the coloured path without a pty.
    /// 2. `NO_COLOR` set to anything **non-empty** disables it. The informal standard is explicit
    ///    that presence-with-an-empty-value is not the signal, so `NO_COLOR=` does nothing.
    /// 3. `TERM=dumb` disables it — that terminal promises no escape-sequence support at all.
    /// 4. Otherwise the destination decides.
    pub fn resolve_with(
        self,
        is_terminal: bool,
        no_color: Option<&OsStr>,
        clicolor_force: Option<&OsStr>,
        term: Option<&OsStr>,
    ) -> bool {
        match self {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => {
                if clicolor_force.is_some_and(|v| !v.is_empty() && v != OsStr::new("0")) {
                    return true;
                }
                if no_color.is_some_and(|v| !v.is_empty()) {
                    return false;
                }
                if term == Some(OsStr::new("dumb")) {
                    return false;
                }
                is_terminal
            }
        }
    }
}

/// The SGR sequence for de-emphasised text, and its reset.
///
/// This is the 256-colour grey `ariadne` paints a report's gutter with, and it is shared because a
/// traceback prints *directly underneath* a diagnostic: two different greys an inch apart read as a
/// rendering bug rather than a distinction. Anything that dims text in the same output should use
/// these rather than pick its own.
pub const DIM: &str = "\x1b[38;5;246m";
/// The reset that closes [`DIM`].
pub const RESET: &str = "\x1b[0m";

/// The process-wide choice, set once from a `--color` flag.
static CHOICE: OnceLock<ColorChoice> = OnceLock::new();

/// Record what this process was asked for on the command line. Called once, before any diagnostic
/// is rendered; a second call is ignored rather than racing.
///
/// A process that never calls this — a test binary, a stapled AOT executable, `noeta-wasm-runner` —
/// gets [`ColorChoice::Auto`], which is the right answer for all of them.
pub fn set_choice(choice: ColorChoice) {
    let _ = CHOICE.set(choice);
}

/// Whether *this process's stderr* should carry colour: the recorded [`ColorChoice`] resolved
/// against stderr being a terminal.
///
/// Every diagnostic a user reads on a terminal goes to stderr, so this is the answer nearly every
/// sink wants. It is a function rather than a value because the destination is genuinely a property
/// of the process — threading a bool from `main` through every subcommand into every compile
/// epilogue would put the same fact in a hundred signatures to say the same thing.
///
/// The sinks that are **not** stderr — a DAP output event, an MCP tool result, a `wasi:http` body,
/// a conformance comparison — must not call this. They call the plain renderers, and their
/// rendering stays byte-for-byte what it was.
pub fn stderr_color() -> bool {
    CHOICE
        .get()
        .copied()
        .unwrap_or_default()
        .resolve(std::io::stderr().is_terminal())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMB: Option<&OsStr> = None;

    fn os(s: &str) -> Option<&OsStr> {
        Some(OsStr::new(s))
    }

    #[test]
    fn auto_follows_the_destination() {
        assert!(ColorChoice::Auto.resolve_with(true, DUMB, DUMB, os("xterm-256color")));
        assert!(!ColorChoice::Auto.resolve_with(false, DUMB, DUMB, os("xterm-256color")));
    }

    #[test]
    fn an_explicit_choice_ignores_both_the_destination_and_the_environment() {
        // The flag is how a user overrides a `NO_COLOR` they did not set themselves (a shell
        // profile, a CI image). If the environment could veto it, `--color always` would be a
        // suggestion.
        assert!(ColorChoice::Always.resolve_with(false, os("1"), DUMB, os("dumb")));
        assert!(!ColorChoice::Never.resolve_with(true, DUMB, os("1"), os("xterm-256color")));
    }

    #[test]
    fn no_color_disables_it_only_when_non_empty() {
        assert!(!ColorChoice::Auto.resolve_with(true, os("1"), DUMB, DUMB));
        // The standard is explicit that an empty value is not the signal — otherwise a shell that
        // exports every declared variable would silently strip colour for everyone.
        assert!(ColorChoice::Auto.resolve_with(true, os(""), DUMB, DUMB));
    }

    #[test]
    fn a_dumb_terminal_gets_no_escape_sequences() {
        assert!(!ColorChoice::Auto.resolve_with(true, DUMB, DUMB, os("dumb")));
    }

    #[test]
    fn clicolor_force_colours_a_pipe() {
        // The path a CI log — or this workspace's own CLI tests, which pipe — uses to reach the
        // coloured rendering without allocating a pty.
        assert!(ColorChoice::Auto.resolve_with(false, DUMB, os("1"), DUMB));
        // ...but it outranks `NO_COLOR` only because it is the more specific statement; `0` and
        // empty are both "not set" per the convention.
        assert!(!ColorChoice::Auto.resolve_with(false, DUMB, os("0"), DUMB));
        assert!(!ColorChoice::Auto.resolve_with(false, DUMB, os(""), DUMB));
    }
}
