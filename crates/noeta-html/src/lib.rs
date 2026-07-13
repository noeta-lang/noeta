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
//! collapsed to a single NUL (`\0`) placeholder plus the `indent` to lay the top level at, and takes
//! back reflowed HTML with the NULs in the same order — `fmt` substitutes the (inline-formatted)
//! holes and re-applies tier-body escaping. So this file never sees Noeta syntax; it only
//! pretty-prints HTML.

use noeta_native::registry::{BodyFormatter, ExtModule, Extension};

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
/// own line, and its children are indented. Everything else (`span`, `b`, `a`, …) is treated as
/// **inline** and flows on the current line, so `<b>${x}</b>` and `[x] ${title}` stay together. (A
/// pragmatic, not exhaustive, list — the common structural elements. `button` is included: templates
/// use it as a standalone control.)
const BLOCK: &[&str] = &[
    "html", "head", "body", "div", "section", "article", "header", "footer", "nav", "main", "aside",
    "p", "ul", "ol", "li", "dl", "dt", "dd", "table", "thead", "tbody", "tfoot", "tr", "td", "th",
    "form", "fieldset", "figure", "blockquote", "hr", "h1", "h2", "h3", "h4", "h5", "h6", "button",
];

/// Void elements — no closing tag, no children — emitted inline as atoms.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// **Raw-text** / whitespace-significant elements: their content is preserved **byte-for-byte** —
/// no HTML tokenizing, no whitespace collapse, no re-indentation. `pre`/`textarea` render whitespace
/// literally; `script`/`style` carry foreign code HTML must not touch. The open tag is laid out like
/// a block, but everything up to and including the matching close tag is emitted verbatim.
const RAW: &[&str] = &["pre", "textarea", "script", "style"];

fn is_block(name: &str) -> bool {
    BLOCK.contains(&name)
}
fn is_void(name: &str) -> bool {
    VOID.contains(&name)
}
fn is_raw(name: &str) -> bool {
    RAW.contains(&name)
}

/// The NUL-free control markers that bracket a **raw region** inside the intermediate (pre-indent)
/// layout. They never reach `fmt`: the final indentation pass emits the region verbatim and strips
/// them. (`\0` is reserved for holes.)
const RAW_OPEN: char = '\u{11}';
const RAW_CLOSE: char = '\u{12}';

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
    /// A raw-text element captured whole: its open tag, verbatim inner content (holes still `\0`), and
    /// its close tag — none of which is reflowed.
    Raw {
        open: String,
        content: String,
        close: String,
    },
    /// A run of text between tags/holes.
    Text(String),
    /// A `${…}` hole, carried as a single NUL by `fmt`.
    Hole,
}

/// The end index of a `<…>` tag starting at `open` (the position of `<`), skipping any `>` inside a
/// quoted attribute value. Returns the index of the closing `>`, or `None` if unterminated.
fn tag_end(chars: &[char], open: usize) -> Option<usize> {
    let mut i = open + 1;
    let mut quote: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '>' => return Some(i),
            None => {}
        }
        i += 1;
    }
    None
}

/// The start index of the matching `</name …>` close tag at or after `from` (case-insensitive tag
/// name), for capturing a raw element's content. `None` if the element is never closed.
fn find_close(chars: &[char], from: usize, name: &str) -> Option<usize> {
    let needle: Vec<char> = format!("</{name}").chars().collect();
    let mut i = from;
    while i + needle.len() <= chars.len() {
        if chars[i..i + needle.len()]
            .iter()
            .zip(&needle)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            // The name must end here (next char is `>`, whitespace, or `/`), not be a longer name.
            if matches!(chars.get(i + needle.len()), Some('>' | ' ' | '\t' | '\n' | '\r' | '/')) {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn tag_name(inner: &str) -> String {
    inner
        .trim_start_matches('/')
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Tokenize HTML (with `\0` holes) into tags / text / holes / raw elements. `None` on an unterminated
/// tag or an unclosed raw element — the signal for the formatter to decline and leave the body
/// verbatim rather than emit broken markup.
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
                let end = tag_end(&chars, i)?;
                let raw: String = chars[i..=end].iter().collect(); // `<` … `>`
                let inner = &raw[1..raw.len() - 1];
                let is_close = inner.starts_with('/');
                let self_closing = inner.ends_with('/');
                let name = tag_name(inner);
                i = end + 1;
                if is_close {
                    toks.push(Tok::Close { name, raw });
                } else if !self_closing && is_raw(&name) {
                    // Capture the element whole — content byte-for-byte to the matching close tag.
                    let close_start = find_close(&chars, i, &name)?;
                    let content: String = chars[i..close_start].iter().collect();
                    let close_end = tag_end(&chars, close_start)?;
                    let close: String = chars[close_start..=close_end].iter().collect();
                    i = close_end + 1;
                    toks.push(Tok::Raw {
                        open: raw,
                        content,
                        close,
                    });
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
/// structural gap collapses to a lone space, trimmed off at the next break.
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
    if in_ws {
        out.push(' ');
    }
    out
}

/// Re-indent HTML by block-element nesting, laying the top level at `base`. A block element opens
/// structure (children indent); an element with only inline content stays on one line
/// (`<li class="x">[x] \0</li>`), one with block children breaks its close tag onto its own line.
/// Inline elements, text, and `\0` holes flow inline. Raw-text elements (`<pre>`, `<textarea>`,
/// `<script>`, `<style>`) keep their content byte-for-byte, unindented and uncollapsed. Idempotent;
/// declines (`None` → verbatim) on unterminated or unclosed markup.
pub fn html_reindent(body: &str, base: &str) -> Option<String> {
    let toks = tokenize(body)?;
    // Pass 1: a relative layout (2-space nesting, column 0), with raw regions bracketed by control
    // markers. Trailing spaces are trimmed at each break, so only raw content can hold them.
    let mut buf = String::new();
    let mut depth = 0usize;
    let mut had_block_child: Vec<bool> = Vec::new();
    let br = |buf: &mut String, depth: usize| {
        while buf.ends_with(' ') {
            buf.pop();
        }
        buf.push('\n');
        for _ in 0..depth {
            buf.push_str("  ");
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
                    buf.push_str(&raw);
                } else {
                    if let Some(top) = had_block_child.last_mut() {
                        *top = true;
                    }
                    if !buf.is_empty() {
                        br(&mut buf, depth);
                    }
                    buf.push_str(&raw);
                    had_block_child.push(false);
                    depth += 1;
                }
            }
            Tok::Close { name, raw } => {
                if is_block(&name) {
                    depth = depth.saturating_sub(1);
                    if had_block_child.pop().unwrap_or(false) {
                        br(&mut buf, depth);
                    }
                    buf.push_str(&raw);
                } else {
                    buf.push_str(&raw);
                }
            }
            Tok::Raw {
                open,
                content,
                close,
            } => {
                if let Some(top) = had_block_child.last_mut() {
                    *top = true;
                }
                if !buf.is_empty() {
                    br(&mut buf, depth);
                }
                buf.push_str(&open);
                buf.push(RAW_OPEN);
                buf.push_str(&content); // byte-for-byte
                buf.push_str(&close);
                buf.push(RAW_CLOSE);
            }
            Tok::Text(t) => buf.push_str(&collapse_ws(&t)),
            Tok::Hole => buf.push('\u{0}'),
        }
    }
    // Pass 2: prepend `base` to each line — except lines inside a raw region, which are emitted
    // verbatim — and strip the raw markers. Leading whitespace (the body's own indentation before
    // the first element) is dropped so the body has no blank first line under `@<tier> {`.
    let mut out = String::new();
    let mut in_raw = false;
    let mut at_line_start = true;
    for c in buf.trim_start().chars() {
        match c {
            RAW_OPEN => in_raw = true,
            RAW_CLOSE => in_raw = false,
            '\n' => {
                out.push('\n');
                at_line_start = true;
            }
            _ => {
                if at_line_start {
                    if !in_raw {
                        out.push_str(base);
                    }
                    at_line_start = false;
                }
                out.push(c);
            }
        }
    }
    Some(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(html: &str) -> String {
        html_reindent(html, "").expect("well-formed")
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
    fn base_indent_is_applied_to_every_structural_line() {
        let out = html_reindent("<ul><li>a</li></ul>", "    ").expect("ok");
        assert_eq!(out, "    <ul>\n      <li>a</li>\n    </ul>");
    }

    #[test]
    fn pre_content_is_verbatim_uncollapsed_and_unindented() {
        // The whitespace inside <pre> is significant: it survives byte-for-byte, gets no base indent,
        // and is not collapsed — even though the <pre> tag itself is indented as a block.
        let out = html_reindent("<div><pre>  keep\n    these   spaces\n</pre></div>", "").expect("ok");
        assert_eq!(out, "<div>\n  <pre>  keep\n    these   spaces\n</pre>\n</div>");
    }

    #[test]
    fn holes_inside_pre_are_preserved() {
        let out = fmt("<pre>x = \u{0}\n</pre>");
        assert_eq!(out, "<pre>x = \u{0}\n</pre>");
    }

    #[test]
    fn is_idempotent() {
        let once = fmt("<div><p>hi <b>\u{0}</b></p><ul><li>a</li></ul><pre>  raw\n  text\n</pre></div>");
        assert_eq!(fmt(&once), once, "html reindent is not idempotent");
    }

    #[test]
    fn holes_in_attributes_are_preserved_in_order() {
        let out = fmt("<a href=\"\u{0}\">click \u{0}</a>");
        assert_eq!(out.matches('\u{0}').count(), 2);
        assert!(out.starts_with("<a href=\"\u{0}\">"));
    }

    #[test]
    fn unterminated_tag_declines() {
        assert!(html_reindent("<div class=\"x", "").is_none());
        assert!(html_reindent("<pre>never closed", "").is_none());
        assert!(html_reindent("<div>oops", "").is_some());
    }
}
