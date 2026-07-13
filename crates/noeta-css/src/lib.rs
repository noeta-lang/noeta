//! A first-party **CSS tier-body formatter** for `noeta fmt`, in its own namespace (not `std`).
//!
//! An HTML formatter (`noeta-html`) reflows structure but leaves `<style>` content verbatim, because
//! CSS is a *different language*. This crate closes that gap the extension way: a formatter-only
//! [`Extension`] that registers a `"css"` body formatter, backed by the pure-Rust
//! [`malva`](https://docs.rs/malva) CSS formatter. `noeta-html`'s `sub`-delegation then hands a
//! hole-free `<style>` body here; `fmt` places the result under the tag. Registering it is opt-in —
//! a toolchain that does not want the CSS dependency simply does not install this extension, and
//! `<style>` bodies stay verbatim.

use noeta_native::registry::{BodyFormatter, ExtModule, Extension};

/// The formatter-only CSS extension (namespace root `"css"`, distinct from `std`).
#[derive(Debug)]
pub struct CssExtension;

/// A process-static handle the toolchain assembles into its registry (`noeta_cli::run_cli`).
pub static CSS_EXTENSION: CssExtension = CssExtension;

const CSS_FORMATTERS: &[BodyFormatter] = &[("css", css_format)];

impl Extension for CssExtension {
    fn name(&self) -> &'static str {
        "css"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[]
    }
    fn body_formatters(&self) -> &'static [BodyFormatter] {
        CSS_FORMATTERS
    }
}

/// Format a `<style>` body — plain CSS (the HTML formatter only delegates hole-free content, so there
/// are no `${…}` holes to preserve) — with malva, then indent every line at `indent` so it nests
/// under the tag. Declines (`None` → the HTML formatter leaves the body verbatim) on a parse error.
fn css_format(
    body: &str,
    indent: &str,
    _sub: &dyn Fn(&str, &str, &str) -> Option<String>,
) -> Option<String> {
    let options = malva::config::FormatOptions::default();
    let formatted = malva::format_text(body, malva::Syntax::Css, &options).ok()?;
    // malva lays CSS out from column 0; place it under the tag by prefixing `indent` to each
    // non-empty line (blank lines stay empty so they carry no trailing indentation).
    let indented = formatted
        .lines()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("{indent}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(indented.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reindents_css_under_the_given_base() {
        let out = css_format("a{color:red;background:blue}", "  ", &|_, _, _| None).unwrap();
        // malva normalizes the rule; every non-blank line is indented at least two spaces.
        assert!(
            out.lines().all(|l| l.is_empty() || l.starts_with("  ")),
            "got:\n{out}"
        );
        assert!(out.contains("color: red"), "got:\n{out}");
    }

    #[test]
    fn declines_on_a_parse_error() {
        assert!(css_format("a { color: ", "", &|_, _, _| None).is_none());
    }
}
