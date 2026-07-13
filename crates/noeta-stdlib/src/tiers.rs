//! std's **dev-tier declarations** (tier-extensions port): the built-in four tiers and the
//! prelude attributes they own, declared through the extension ABI (`ExtTier`/`ExtAttribute`)
//! instead of hardcoded checker registration — the same surface a third-party tier package uses,
//! dogfooded. The checker registers these at prelude time (attribute construction gate,
//! reflection materialization, shadowable by a user declaration) and resolves tier names against
//! them; the **runners stay native** (`noeta test`'s parallel executor, `noeta bench`'s two-point
//! measurement, `noeta doc`'s extractor, `--tier debug`'s inline activation) — only the
//! declarations live here.
//!
//! Names are literals rather than `noeta_ast::reflect` constants because the ABI sits beneath the
//! syntax crates (neither `noeta-native` nor this crate sees `noeta-ast`); a checker test pins
//! the two spellings together so they cannot drift.

use noeta_native::registry::{
    AttrFieldDefault, AttrFieldType, BodyFormatter, ExtAttrField, ExtAttribute, ExtTier,
};

/// std's **tier-body formatters** keyed by body language (extension-driven tier-body formatting).
/// std ships one: a JSON re-indenter for the `"json"` language, which its `@json` tier declares —
/// so `noeta fmt` reflows `@json { … }` bodies. Keyed by language, so any other tier declaring
/// `text: "json"` would get it too.
pub const BODY_FORMATTERS: &[BodyFormatter] = &[("json", json_reindent)];

/// The built-in dev-tiers. `bench` carries its knob attribute; the rest are knob-less (`test`'s
/// metadata attributes attach per-fn, not through directive args, so they are not a `config`).
pub const TIERS: &[ExtTier] = &[
    ExtTier {
        name: "test",
        config: None,
        text: None,
        expr: None,
        handler: None,
    },
    ExtTier {
        name: "bench",
        config: Some("Bench"),
        text: None,
        expr: None,
        handler: None,
    },
    // `doc` is a **text** tier: its `@doc { … }` bodies are verbatim markdown. Declaring the
    // language here (rather than a hardcoded `text_lang` special-case) is the dogfood that the
    // extension text/expression-tier surface drives every consumer — lexer capture, editor
    // injection, LSP hover.
    ExtTier {
        name: "doc",
        config: None,
        text: Some("markdown"),
        expr: None,
        handler: None,
    },
    ExtTier {
        name: "debug",
        config: None,
        text: None,
        expr: None,
        handler: None,
    },
    // `@json { … ${s} … }` — a native **expression** tier (expr-tiers arc): its blocks are `string`
    // values (JSON text with safely-quoted holes), desugared to `std.template.render`. The dogfood
    // that a native package declares an expression tier — body language, value type, and a native
    // handler — through the same `ExtTier` surface a program `@tier(…, text/expr)` uses.
    ExtTier {
        name: "json",
        config: None,
        text: Some("json"),
        expr: Some("string"),
        handler: Some("std.template.render"),
    },
];

/// A minimal JSON re-indenter — the body formatter for the `@json` tier (extension-driven tier-body
/// formatting). `body` is the tier's foreign JSON text with each `${…}` hole already collapsed to a
/// single NUL (`\0`) placeholder by `noeta fmt`; this returns the same JSON laid out canonically
/// (two-space indent, one element per line, `key: value` spacing) with the NULs preserved in order.
/// It is a depth-driven reflow, not a validating parser: it tracks string state so braces/commas
/// inside strings are literal, treats a `\0` hole as an atom, and declines (`None`, → verbatim) only
/// if the delimiters are unbalanced. Idempotent — its own output re-indents to itself.
fn json_reindent(body: &str) -> Option<String> {
    let mut out = String::with_capacity(body.len());
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;
    let indent = |out: &mut String, depth: usize| {
        out.push('\n');
        for _ in 0..depth {
            out.push_str("  ");
        }
    };
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            c if c.is_whitespace() => {} // collapse insignificant whitespace
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                // Peek past whitespace for an immediate close → keep `{}`/`[]` compact.
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                let empty = chars.get(j) == Some(&'}') || chars.get(j) == Some(&']');
                if !empty {
                    depth += 1;
                    indent(&mut out, depth);
                }
            }
            '}' | ']' => {
                // Balance: an unexpected close means this is not JSON we understand — decline.
                let was_empty = out.ends_with('{') || out.ends_with('[');
                if !was_empty {
                    depth = depth.checked_sub(1)?;
                    indent(&mut out, depth);
                }
                out.push(c);
            }
            ',' => {
                out.push(c);
                indent(&mut out, depth);
            }
            ':' => out.push_str(": "),
            _ => out.push(c), // ordinary token char (incl. the `\0` hole atom)
        }
        i += 1;
    }
    if depth != 0 || in_string {
        return None;
    }
    Some(out.trim().to_string())
}

/// The prelude attributes the built-in tiers own: the test runner's metadata quartet
/// (`Skip`/`Name`/`Group`/`Data`), `bench`'s knob (`Bench { iterations }`), and the doc tier's
/// stamped text carrier (`Doc { text }` — written by activation from an adjacency-attached
/// `@doc { … }` block, never by hand).
pub const ATTRIBUTES: &[ExtAttribute] = &[
    ExtAttribute {
        name: "Skip",
        fields: &[ExtAttrField {
            name: "reason",
            ty: AttrFieldType::Str,
            // Optional: both `#[Skip]` and `#[Skip("flaky")]` construct it.
            default: Some(AttrFieldDefault::Str("")),
        }],
    },
    ExtAttribute {
        name: "Name",
        fields: &[ExtAttrField {
            name: "value",
            ty: AttrFieldType::Str,
            default: None,
        }],
    },
    ExtAttribute {
        name: "Group",
        fields: &[ExtAttrField {
            name: "value",
            ty: AttrFieldType::Str,
            default: None,
        }],
    },
    ExtAttribute {
        name: "Data",
        fields: &[ExtAttrField {
            name: "rows",
            ty: AttrFieldType::Dyn,
            default: None,
        }],
    },
    ExtAttribute {
        name: "Bench",
        fields: &[ExtAttrField {
            name: "iterations",
            ty: AttrFieldType::Int,
            default: None,
        }],
    },
    ExtAttribute {
        name: "Doc",
        fields: &[ExtAttrField {
            name: "text",
            ty: AttrFieldType::Str,
            default: None,
        }],
    },
];
