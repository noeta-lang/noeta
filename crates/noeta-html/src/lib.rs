//! A first-party **HTML tier-body formatter** for `noeta fmt`, in its own namespace (not `std`).
//!
//! This is the extension-driven tier-body-formatting story taken the rest of the way: `@html` is a
//! *program* tier (declared in the in-language liveview package, with a reactive Noeta handler), and
//! this crate is a formatter-only [`Extension`] that registers a native HTML re-indenter for the
//! `"html"` **language**. Any tier — program or native — that declares `text: "html"` gets it; `fmt`
//! resolves by language and delegates. Core stays HTML-ignorant, the `@html` handler stays idiomatic
//! Noeta, and the formatter lives where the language knowledge does: here, in the extension.
//!
//! The formatter is a *pure foreign reflow*: `fmt` hands it the body's HTML with each `${…}` hole
//! collapsed to a single NUL (`\0`) placeholder, and takes back reflowed HTML with the NULs in the
//! same order — `fmt` substitutes the (inline-formatted) holes and re-applies tier-body escaping. So
//! this file never sees Noeta syntax; it only pretty-prints HTML.

use noeta_native::registry::{BodyFormatter, Extension, ExtModule};

/// The formatter-only extension. It contributes no modules or types — its whole purpose is to
/// register the `"html"` body formatter so `@html` (and any `text: "html"` tier) reflows under
/// `noeta fmt`. Its own namespace root is `"html"`, distinct from `std`.
#[derive(Debug)]
pub struct HtmlExtension;

/// A process-static handle the toolchain assembles into its registry (`noeta_cli::run_cli`).
pub static HTML_EXTENSION: HtmlExtension = HtmlExtension;

const HTML_FORMATTERS: &[BodyFormatter] = &[("html", html_reindent)];

impl Extension for HtmlExtension {
    fn name(&self) -> &'static str {
        "html"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[]
    }
    fn body_formatters(&self) -> &'static [BodyFormatter] {
        HTML_FORMATTERS
    }
}

/// HTML elements whose content is laid out as **block** structure — each such open/close tag gets its
/// own line, and its children are indented. Everything else (`span`, `b`, `a`, `button`, …) is
/// treated as **inline** and flows on the current line, so `<b>${x}</b>` and `[x] ${title}` stay
/// together. (A pragmatic, not exhaustive, list — the common structural elements.)
const BLOCK: &[&str] = &[
    "html", "head", "body", "div", "section", "article", "header", "footer", "nav", "main", "aside",
    "p", "ul", "ol", "li", "dl", "dt", "dd", "table", "thead", "tbody", "tfoot", "tr", "td", "th",
    "form", "fieldset", "figure", "blockquote", "pre", "hr", "h1", "h2", "h3", "h4", "h5", "h6",
    "title", "script", "style", "template", "button",
];

/// Void elements — no closing tag, no children — emitted inline as atoms.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

fn is_block(name: &str) -> bool {
    BLOCK.contains(&name)
}

fn is_void(name: &str) -> bool {
    VOID.contains(&name)
}

enum Tok {
    /// An opening (or self-closing) tag: its verbatim `<…>` text, lowercased name, and whether it is
    /// self-closing or a void element.
    Open {
        name: String,
        raw: String,
        self_closing: bool,
        void: bool,
    },
    /// A closing `</…>` tag.
    Close { name: String, raw: String },
    /// A run of text between tags/holes.
    Text(String),
    /// A `${…}` hole, carried as a single NUL by `fmt`.
    Hole,
}

/// Tokenize HTML (with `\0` holes) into tags / text / holes. `None` on an unterminated tag — the
/// signal for the formatter to decline and leave the body verbatim rather than emit broken markup.
fn tokenize(body: &str) -> Option<Vec<Tok>> {
    let chars: Vec<char> = body.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    let mut text = String::new();
    let flush = |text: &mut String, toks: &mut Vec<Tok>| {
        if !text.is_empty() {
            toks.push(Tok::Text(std::mem::take(text)));
        }
    };
    while i < chars.len() {
        match chars[i] {
            '\u{0}' => {
                flush(&mut text, &mut toks);
                toks.push(Tok::Hole);
                i += 1;
            }
            '<' => {
                flush(&mut text, &mut toks);
                // Read to the matching `>`, skipping any inside a quoted attribute value.
                let start = i;
                i += 1;
                let mut quote: Option<char> = None;
                while i < chars.len() {
                    let c = chars[i];
                    match quote {
                        Some(q) if c == q => quote = None,
                        Some(_) => {}
                        None if c == '"' || c == '\'' => quote = Some(c),
                        None if c == '>' => break,
                        None => {}
                    }
                    i += 1;
                }
                if i >= chars.len() {
                    return None; // unterminated tag
                }
                let raw: String = chars[start..=i].iter().collect(); // includes `<` … `>`
                i += 1;
                let inner = &raw[1..raw.len() - 1]; // strip `<` `>`
                let is_close = inner.starts_with('/');
                let self_closing = inner.ends_with('/');
                let name: String = inner
                    .trim_start_matches('/')
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .collect::<String>()
                    .to_ascii_lowercase();
                if is_close {
                    toks.push(Tok::Close { name, raw });
                } else {
                    let void = is_void(&name);
                    toks.push(Tok::Open {
                        name,
                        raw,
                        self_closing,
                        void,
                    });
                }
            }
            _ => {
                text.push(chars[i]);
                i += 1;
            }
        }
    }
    flush(&mut text, &mut toks);
    Some(toks)
}

/// Collapse every run of ASCII whitespace to a single space (HTML's own whitespace model). Leading
/// and trailing spaces are kept — they carry inline spacing like `${box} ${title}` — but a purely
/// structural gap collapses to a lone space that ends up trailing a line, which `fmt` trims away.
fn collapse_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_ws = false;
    for c in text.chars() {
        if c.is_whitespace() {
            in_ws = true;
        } else {
            if in_ws {
                out.push(' ');
            }
            in_ws = false;
            out.push(c);
        }
    }
    // A trailing (or all-) whitespace run collapses to a single space — significant between inline
    // content (`${box} ${title}`); merely trailing a structural line, where `fmt` trims it away.
    if in_ws {
        out.push(' ');
    }
    out
}

/// Re-indent HTML by block-element nesting. A block element opens structure (its children indent);
/// an element with only inline content stays on one line (`<li class="x">[x] \0</li>`), while one
/// with block children breaks its close tag onto its own line. Inline elements, text, and `\0` holes
/// flow inline. Idempotent; declines (`None` → verbatim) on unterminated markup.
pub fn html_reindent(body: &str) -> Option<String> {
    let toks = tokenize(body)?;
    let mut out = String::new();
    let mut depth = 0usize;
    // One flag per open block element: did it contain a block child? Decides whether its close tag
    // breaks onto its own line.
    let mut had_block_child: Vec<bool> = Vec::new();
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        for _ in 0..depth {
            out.push_str("  ");
        }
    };
    for tok in toks {
        match tok {
            Tok::Open {
                name,
                raw,
                self_closing,
                void,
            } => {
                if void || self_closing || !is_block(&name) {
                    out.push_str(&raw); // inline / void / self-closing atom
                } else {
                    if let Some(top) = had_block_child.last_mut() {
                        *top = true;
                    }
                    if !out.is_empty() {
                        newline(&mut out, depth);
                    }
                    out.push_str(&raw);
                    had_block_child.push(false);
                    depth += 1;
                }
            }
            Tok::Close { name, raw } => {
                if is_block(&name) {
                    depth = depth.saturating_sub(1);
                    if had_block_child.pop().unwrap_or(false) {
                        newline(&mut out, depth);
                    }
                    out.push_str(&raw);
                } else {
                    out.push_str(&raw);
                }
            }
            Tok::Text(t) => out.push_str(&collapse_ws(&t)),
            Tok::Hole => out.push('\u{0}'),
        }
    }
    // Strip trailing whitespace per line (a collapsed structural space ends up trailing a block tag)
    // so the result is self-idempotent — independent of fmt's own line-trim.
    let out: String = out.lines().map(str::trim_end).collect::<Vec<_>>().join("\n");
    Some(out.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(html: &str) -> String {
        html_reindent(html).expect("well-formed")
    }

    #[test]
    fn block_children_indent_inline_content_stays() {
        let out = fmt("<ul><li class=\"todo\">[x] \u{0}</li><li>\u{0}</li></ul>");
        assert_eq!(
            out,
            "<ul>\n  <li class=\"todo\">[x] \u{0}</li>\n  <li>\u{0}</li>\n</ul>"
        );
    }

    #[test]
    fn is_idempotent() {
        let once = fmt("<div><p>hi <b>\u{0}</b></p><ul><li>a</li></ul></div>");
        assert_eq!(fmt(&once), once, "html reindent is not idempotent");
    }

    #[test]
    fn holes_in_attributes_are_preserved_in_order() {
        // Two holes: one in an attribute, one in text — both survive as `\0`, in order.
        let out = fmt("<a href=\"\u{0}\">click \u{0}</a>");
        assert_eq!(out.matches('\u{0}').count(), 2);
        assert!(out.starts_with("<a href=\"\u{0}\">"));
    }

    #[test]
    fn unterminated_tag_declines() {
        // A tag with no closing `>` — the formatter declines so fmt leaves the body verbatim rather
        // than emit broken markup. (An unclosed *element*, `<div>oops`, still reflows fine.)
        assert!(html_reindent("<div class=\"x").is_none());
        assert!(html_reindent("<div>oops").is_some());
    }
}
