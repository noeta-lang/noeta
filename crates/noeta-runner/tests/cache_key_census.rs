//! **A compilation input that reaches the cache key by nobody's decision is how a stale artifact
//! runs.**
//!
//! The startup cache serves compiled bytecode when every input to the compile is unchanged. The key
//! builder is careful — domain-tagged, length-prefixed, sorted. The *inputs* are a hand-enumerated
//! list, and twice now an input has been missing from it:
//!
//! * `2495394f4 fix(cache): key on the entry file, not just the source set` — `noeta run a.noe`
//!   then `noeta run b.noe` in one directory ran the same program.
//! * Before the editions arc's S2, only the root package's edition was keyed, so a dependency's
//!   edition change served a stale artifact.
//!
//! And a third, found by the parallel-path audit and fixed in its first pass: `package_uses` — the
//! per-package `[directives]` table that decides *which extension* a `@name` expands to —
//! was not folded at all, so editing `[directives] openapi = "para"` to `openapi = "other"` changed
//! no `.noe` byte, produced the same key, hit the cache, and ran the **old provider's generated
//! code**. Deleting the binding was worse: that should be `E0036`, but a hit skips the whole front
//! end, so the error was never reported and the stale expansion ran anyway.
//!
//! That fix made [`open_startup_cache`] and `key_deps` **destructure** their inputs with no
//! rest-pattern, so a new field on `FrontFacts` or `DepPackage` is a compile error rather than an
//! omission. That is the strong half and it needs no test.
//!
//! **This file closes the half a destructure cannot.** `let FrontFacts { thing: _, .. }` compiles
//! just as happily as folding it, and reads as a decision nobody made. So: every field bound in
//! either destructure must actually be *used* to build the key, unless it is named here with a
//! reason. Today exactly one field is exempt, and its reason is that it is folded under a different
//! spelling.
//!
//! The technique — read the crate's own source text — is the one `noeta-diagnostics`' `ALL` gate,
//! `noeta-compiler`'s `pipeline_tables` and `noeta-check`'s `site_policies` use, for the same reason:
//! the property is about how the code is written, and Rust has no way to state it as a type.

use std::path::{Path, PathBuf};

/// A field of a destructured cache-key input that is deliberately not used under its own name,
/// with the reason it is not.
const NOT_USED_UNDER_ITS_OWN_NAME: &[(&str, &str, &str)] = &[(
    "key_deps",
    "prefix",
    "folded into every one of this dep's key names (`dep.prefix.join(\".\")` on the next line) \
     rather than keyed as a value of its own",
)];

fn compile_rs() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/compile.rs")
}

/// The body of `fn name(...)`, from its opening brace to the matching close.
fn function_body(text: &str, name: &str) -> String {
    let at = text
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("`fn {name}(` — has it been renamed? this gate is now vacuous"));
    let open = text[at..]
        .find('{')
        .map(|i| at + i)
        .expect("a function has a body");
    let mut depth = 0usize;
    for (i, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return text[open..open + i + 1].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces in `{name}`");
}

/// The field names bound by the **first** `let <Type> { … } = …;` destructure in `body`, paired with
/// whether each was bound to `_`.
fn destructured_fields(body: &str, ty: &str) -> Vec<(String, bool)> {
    let at = body.find(&format!("{ty} {{")).unwrap_or_else(|| {
        panic!(
            "no `{ty} {{ … }}` destructure — the input is no longer taken apart by field, so a \
                new field is no longer a compile error and this gate is vacuous"
        )
    });
    // Past the brace, not past the type name — the two are separated by a space, and swallowing it
    // leaves `{` glued to the first field.
    let open = at + body[at..].find('{').expect("the destructure opens") + 1;
    let close = body[open..]
        .find('}')
        .map(|i| open + i)
        .expect("the destructure closes");
    assert!(
        !body[open..close].contains(".."),
        "`{ty}`'s destructure has a rest-pattern, which is exactly what lets a new compilation \
         input reach the cache key by nobody's decision — that is the bug this gate exists for"
    );
    body[open..close]
        .split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty() && !f.starts_with("//"))
        .map(|f| match f.split_once(':') {
            Some((name, bind)) => (name.trim().to_string(), bind.trim() == "_"),
            None => (f.to_string(), false),
        })
        .collect()
}

/// Every field the two cache-key builders take apart is either used to build the key, or listed
/// above with a reason.
#[test]
fn every_destructured_cache_key_input_reaches_the_key() {
    let text = std::fs::read_to_string(compile_rs()).expect("compile.rs is readable");

    let cases = [
        ("open_startup_cache", "FrontFacts"),
        ("key_deps", "DepPackage"),
    ];
    let mut checked = 0usize;
    let mut offenders = Vec::new();

    for (func, ty) in cases {
        let body = function_body(&text, func);
        let fields = destructured_fields(&body, ty);
        assert!(
            fields.len() >= 5,
            "`{func}` destructures only {} field(s) of `{ty}` — the scrape has stopped matching the \
             source, so this gate is passing vacuously",
            fields.len()
        );
        for (field, underscored) in fields {
            checked += 1;
            let excused = NOT_USED_UNDER_ITS_OWN_NAME
                .iter()
                .any(|(f, n, _)| *f == func && *n == field);
            if excused {
                assert!(
                    underscored,
                    "`{func}` binds `{field}` by name but it is listed as not used under its own \
                     name — one of the two is now wrong"
                );
                continue;
            }
            if underscored {
                offenders.push(format!(
                    "{func}: `{ty}.{field}` is bound to `_`, so it reaches no key entry. If it is \
                     genuinely not a compilation input, add it to NOT_USED_UNDER_ITS_OWN_NAME with \
                     the reason; if it is, fold it into the key."
                ));
            }
        }
    }

    assert!(
        checked >= 10,
        "only {checked} field(s) across both builders — the scrape has gone blind"
    );
    assert!(offenders.is_empty(), "\n  {}", offenders.join("\n  "));
}

/// The excuse list cannot outlive the thing it excuses: every entry must name a field that is still
/// destructured, in a function that still exists.
#[test]
fn the_excuse_list_still_describes_the_source() {
    let text = std::fs::read_to_string(compile_rs()).expect("compile.rs is readable");
    for (func, field, reason) in NOT_USED_UNDER_ITS_OWN_NAME {
        assert!(
            !reason.trim().is_empty(),
            "{func}.{field} is excused with no reason, which is the same as not being excused"
        );
        let body = function_body(&text, func);
        assert!(
            body.contains(field),
            "{func} no longer mentions `{field}` — drop the entry rather than leaving a stale excuse"
        );
    }
}
