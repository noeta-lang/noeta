//! A Wadler/Leijen pretty-printing algebra — the intermediate form the AST lowers to.
//!
//! Instead of emitting strings directly, the printer builds a [`Doc`] tree and hands it to
//! [`render`], which lays it out within a column budget using the classic best-fit algorithm
//! (Wadler's *A prettier printer*, in Lindig's strict/iterative formulation). The payoff is that the
//! *same* tree serves both formatter policies (see `FmtConfig::wrap`): the lowering chooses
//! whether a break point is a hard break (author-directed, `wrap = false`) or a [`Doc::group`]-gated
//! soft break (width-driven, `wrap = true`) — the renderer below is identical either way.
//!
//! The vocabulary:
//! - [`Doc::text`] — literal text; must not contain newlines (use the line docs).
//! - [`Doc::line`] — a space when its group is flat, a newline (+ indent) when broken.
//! - [`Doc::softline`] — nothing when flat, a newline when broken.
//! - [`Doc::hardline`] — always a newline; forces every enclosing group to break.
//! - [`Doc::nest`] — indent the sub-doc's line breaks by N columns.
//! - [`Doc::group`] — lay the sub-doc out flat if it fits the remaining width, else broken.

/// A pretty-printing document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Doc {
    /// Empty.
    Nil,
    /// Literal text (newline-free).
    Text(String),
    /// Verbatim text that may contain newlines, emitted exactly as-is with **no** indentation
    /// applied to its internal line breaks — for content whose interior whitespace is significant
    /// (a `@doc { … }` body, a multiline string literal).
    RawText(String),
    /// A break that flattens to a single space.
    Line,
    /// A break that flattens to nothing.
    SoftLine,
    /// A break that never flattens; forces enclosing groups to break.
    HardLine,
    /// A sequence of docs, laid out in order.
    Concat(Vec<Doc>),
    /// Indent the contained doc's line breaks by `+n` columns.
    Nest(isize, Box<Doc>),
    /// Lay out flat if it fits the remaining width, otherwise broken.
    Group(Box<Doc>),
    /// Content emitted **only when the nearest enclosing group breaks** (nothing when it is flat) —
    /// the standard "if-break" combinator, used for a trailing comma that appears on a wrapped list
    /// but not an inline one.
    IfBreak(Box<Doc>),
}

impl Doc {
    /// Literal text. Debug-asserts the text is newline-free (newlines must be `line`/`hardline` so
    /// indentation tracks correctly).
    pub fn text(s: impl Into<String>) -> Doc {
        let s = s.into();
        debug_assert!(
            !s.contains('\n'),
            "Doc::text must not contain newlines: {s:?}"
        );
        Doc::Text(s)
    }

    /// Verbatim, possibly-multiline text (see [`Doc::RawText`]).
    pub fn raw_text(s: impl Into<String>) -> Doc {
        Doc::RawText(s.into())
    }

    pub fn line() -> Doc {
        Doc::Line
    }

    pub fn softline() -> Doc {
        Doc::SoftLine
    }

    pub fn hardline() -> Doc {
        Doc::HardLine
    }

    pub fn nil() -> Doc {
        Doc::Nil
    }

    /// Concatenate a sequence of docs.
    pub fn concat(docs: impl IntoIterator<Item = Doc>) -> Doc {
        Doc::Concat(docs.into_iter().collect())
    }

    /// Interleave `docs` with `sep` (like `join`), producing a single concatenation.
    pub fn join(docs: impl IntoIterator<Item = Doc>, sep: Doc) -> Doc {
        let mut out = Vec::new();
        for (i, d) in docs.into_iter().enumerate() {
            if i > 0 {
                out.push(sep.clone());
            }
            out.push(d);
        }
        Doc::Concat(out)
    }

    /// Indent this doc's line breaks by `n` columns.
    pub fn nest(self, n: isize) -> Doc {
        Doc::Nest(n, Box::new(self))
    }

    /// Group this doc: flat if it fits, else broken.
    pub fn group(self) -> Doc {
        Doc::Group(Box::new(self))
    }

    /// Emit this doc only when the enclosing group breaks (see [`Doc::IfBreak`]).
    pub fn if_break(self) -> Doc {
        Doc::IfBreak(Box::new(self))
    }

    /// Append `other` after `self`. A test-only builder convenience (the printer uses
    /// [`Doc::concat`]); `#[cfg(test)]` so it does not read as dead code in release builds.
    #[cfg(test)]
    pub fn append(self, other: Doc) -> Doc {
        match self {
            Doc::Concat(mut v) => {
                v.push(other);
                Doc::Concat(v)
            }
            first => Doc::Concat(vec![first, other]),
        }
    }

    /// Whether this doc renders to a **single line at every width** — it contains no break of any
    /// kind and no multi-line [`Doc::RawText`].
    ///
    /// A layout rule that wants to keep a construct on one line has to know that its *contents*
    /// will stay there too, and the contents are built by a recursive descent that may have emitted
    /// a [`Doc::hardline`] of its own (a block-bodied `match` arm, a `fn() { … }` closure, a
    /// multiline string). Choosing the flat form regardless would print a newline inside it — and
    /// then the *next* format run would see the author's construct broken and explode it, so the
    /// rule would not be a fixed point. This is the guard that makes "flat" mean flat.
    ///
    /// [`Doc::Line`]/[`Doc::SoftLine`]/[`Doc::IfBreak`] count as breakable: whether they break is
    /// the renderer's decision, not one this predicate can make.
    pub fn never_breaks(&self) -> bool {
        match self {
            Doc::Nil | Doc::Text(_) => true,
            Doc::RawText(s) => !s.contains('\n'),
            Doc::Line | Doc::SoftLine | Doc::HardLine => false,
            Doc::Concat(docs) => docs.iter().all(Doc::never_breaks),
            Doc::Nest(_, d) | Doc::Group(d) => d.never_breaks(),
            Doc::IfBreak(_) => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Flat,
    Break,
}

/// The display width of a text fragment (char count — adequate for layout decisions; identifiers
/// and punctuation are ASCII, and a slightly-off width on an exotic string literal only affects
/// wrapping, never correctness).
fn width_of(s: &str) -> isize {
    s.chars().count() as isize
}

/// Render `doc` within a `width`-column budget.
/// How a level of indentation is written: `width` columns as spaces, or one tab per level.
#[derive(Clone, Copy)]
pub struct IndentStyle {
    pub width: usize,
    pub tabs: bool,
}

impl Default for IndentStyle {
    fn default() -> Self {
        IndentStyle {
            width: 4,
            tabs: false,
        }
    }
}

pub fn render(doc: &Doc, width: usize) -> String {
    render_protected(doc, width, IndentStyle::default()).0
}

/// Like [`render`], but also returns the byte ranges of the output that came from [`Doc::RawText`] —
/// verbatim, possibly whitespace-significant content (a tier body, a multiline template/comment).
/// The whole-file trailing-whitespace trim uses these to leave such content untouched: a trailing
/// space produced by *layout* (an indented blank line) is safe to strip, but one inside `RawText` —
/// a Markdown line break in `@doc`, significant space in an `@html` `<pre>` — is content and is kept.
pub fn render_protected(
    doc: &Doc,
    width: usize,
    indent_style: IndentStyle,
) -> (String, Vec<std::ops::Range<usize>>) {
    let width = width as isize;
    let mut out = String::new();
    let mut protected: Vec<std::ops::Range<usize>> = Vec::new();
    let mut col: isize = 0;
    // The work stack of (indent, mode, doc), processed top-down.
    let mut stack: Vec<(isize, Mode, &Doc)> = vec![(0, Mode::Break, doc)];

    while let Some((indent, mode, doc)) = stack.pop() {
        match doc {
            Doc::Nil => {}
            Doc::Text(s) => {
                out.push_str(s);
                col += width_of(s);
            }
            Doc::RawText(s) => {
                let start = out.len();
                out.push_str(s);
                protected.push(start..out.len());
                // Column tracks the tail after the last newline (interior lines are not re-indented).
                col = match s.rfind('\n') {
                    Some(nl) => width_of(&s[nl + 1..]),
                    None => col + width_of(s),
                };
            }
            Doc::Concat(docs) => {
                for d in docs.iter().rev() {
                    stack.push((indent, mode, d));
                }
            }
            Doc::Nest(n, d) => stack.push((indent + n, mode, d)),
            Doc::Line => match mode {
                Mode::Flat => {
                    out.push(' ');
                    col += 1;
                }
                Mode::Break => col = new_line(&mut out, indent, indent_style),
            },
            Doc::SoftLine => match mode {
                Mode::Flat => {}
                Mode::Break => col = new_line(&mut out, indent, indent_style),
            },
            Doc::HardLine => col = new_line(&mut out, indent, indent_style),
            Doc::Group(d) => {
                // Flat if the flattened group (plus the rest of this line) fits; else broken.
                let mode = if fits(width - col, indent, d, &stack) {
                    Mode::Flat
                } else {
                    Mode::Break
                };
                stack.push((indent, mode, d));
            }
            Doc::IfBreak(d) => {
                if mode == Mode::Break {
                    stack.push((indent, mode, d));
                }
            }
        }
    }
    (out, protected)
}

/// Emit a newline followed by `indent` columns of indentation — as spaces, or one tab per level when
/// `style.tabs`. Returns the new column. (A trailing `\r` for CRLF, if any, is applied once at the
/// end of formatting, not here.)
fn new_line(out: &mut String, indent: isize, style: IndentStyle) -> isize {
    out.push('\n');
    if style.tabs && style.width > 0 {
        for _ in 0..(indent as usize / style.width) {
            out.push('\t');
        }
    } else {
        for _ in 0..indent {
            out.push(' ');
        }
    }
    indent
}

/// Whether laying out `group_doc` flat (followed by the already-queued `rest`, in their own modes)
/// fits in `remaining` columns before the next line break. A [`Doc::HardLine`] inside the flattened
/// group makes it not fit (forcing the group to break); a break in the trailing `rest` ends the line.
fn fits(
    mut remaining: isize,
    group_indent: isize,
    group_doc: &Doc,
    rest: &[(isize, Mode, &Doc)],
) -> bool {
    let mut local: Vec<(isize, Mode, &Doc)> = vec![(group_indent, Mode::Flat, group_doc)];
    let mut rest_idx = rest.len();

    loop {
        if remaining < 0 {
            return false;
        }
        let item = local.pop().or_else(|| {
            rest_idx.checked_sub(1).map(|i| {
                rest_idx = i;
                rest[i]
            })
        });
        let Some((indent, mode, doc)) = item else {
            return true; // reached the end without overflowing
        };
        match doc {
            Doc::Nil => {}
            Doc::Text(s) => remaining -= width_of(s),
            // A multiline raw text can never lie flat on the current line.
            Doc::RawText(s) if s.contains('\n') => return matches!(mode, Mode::Break),
            Doc::RawText(s) => remaining -= width_of(s),
            Doc::Concat(docs) => {
                for d in docs.iter().rev() {
                    local.push((indent, mode, d));
                }
            }
            Doc::Nest(n, d) => local.push((indent + n, mode, d)),
            Doc::Line => match mode {
                Mode::Flat => remaining -= 1,
                Mode::Break => return true,
            },
            Doc::SoftLine => match mode {
                Mode::Flat => {}
                Mode::Break => return true,
            },
            Doc::HardLine => match mode {
                Mode::Flat => return false, // can't flatten a forced break
                Mode::Break => return true, // the line ends here
            },
            Doc::Group(d) => local.push((indent, Mode::Flat, d)),
            // In `fits` the group under test is Flat, so an if-break contributes nothing; a
            // Break-mode item in the trailing continuation contributes its content.
            Doc::IfBreak(d) => {
                if mode == Mode::Break {
                    local.push((indent, mode, d));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `[a, b, c]`-style group that fits stays on one line; when it doesn't, it breaks.
    fn list(items: &[&str]) -> Doc {
        let inner = Doc::concat([
            Doc::softline(),
            Doc::join(
                items.iter().map(|s| Doc::text(*s)),
                Doc::concat([Doc::text(","), Doc::line()]),
            ),
        ])
        .nest(4)
        .append(Doc::softline());
        Doc::text("[").append(inner).append(Doc::text("]")).group()
    }

    #[test]
    fn group_stays_flat_when_it_fits() {
        assert_eq!(render(&list(&["1", "2", "3"]), 80), "[1, 2, 3]");
    }

    #[test]
    fn group_breaks_when_too_wide() {
        // Narrow budget forces each element onto its own indented line.
        assert_eq!(
            render(&list(&["one", "two", "three"]), 8),
            "[\n    one,\n    two,\n    three\n]"
        );
    }

    #[test]
    fn line_is_space_when_flat_newline_when_broken() {
        let d = Doc::text("a")
            .append(Doc::line())
            .append(Doc::text("b"))
            .group();
        assert_eq!(render(&d, 80), "a b");
        assert_eq!(render(&d, 1), "a\nb");
    }

    #[test]
    fn hardline_always_breaks_and_forces_the_group() {
        let d = Doc::text("a")
            .append(Doc::hardline())
            .append(Doc::text("b"))
            .group();
        // Even with an enormous budget, the hardline breaks (and its group cannot be flat).
        assert_eq!(render(&d, 80), "a\nb");
    }

    #[test]
    fn nest_indents_broken_lines_only() {
        let d = Doc::text("fn {")
            .append(Doc::concat([Doc::hardline(), Doc::text("body")]).nest(4))
            .append(Doc::hardline())
            .append(Doc::text("}"));
        assert_eq!(render(&d, 80), "fn {\n    body\n}");
    }

    #[test]
    fn if_break_emits_only_when_the_group_breaks() {
        // `[ a, b <ifbreak ,> ]` — the trailing comma appears broken, vanishes flat.
        let d = Doc::text("[")
            .append(
                Doc::concat([
                    Doc::softline(),
                    Doc::join(
                        [Doc::text("a"), Doc::text("b")],
                        Doc::concat([Doc::text(","), Doc::line()]),
                    ),
                    Doc::text(",").if_break(),
                ])
                .nest(4),
            )
            .append(Doc::softline())
            .append(Doc::text("]"))
            .group();
        assert_eq!(render(&d, 80), "[a, b]"); // flat: no trailing comma
        assert_eq!(render(&d, 3), "[\n    a,\n    b,\n]"); // broken: trailing comma
    }

    #[test]
    fn nested_groups_break_independently() {
        // Outer breaks, inner still fits → inner stays flat on its own line.
        let inner = Doc::text("(")
            .append(Doc::join([Doc::text("x"), Doc::text("y")], Doc::text(", ")))
            .append(Doc::text(")"))
            .group();
        let d = Doc::text("f")
            .append(Doc::concat([Doc::hardline(), inner]).nest(2))
            .group();
        assert_eq!(render(&d, 80), "f\n  (x, y)");
    }
}
