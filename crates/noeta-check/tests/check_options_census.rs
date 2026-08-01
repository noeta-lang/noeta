//! **`..Default::default()` is what made a forgotten check option silent, so the literal is gone.**
//!
//! [`noeta_check::CheckOptions`] used to be assembled by hand at a dozen call sites, every one of
//! them ending in `..Default::default()`. That is what makes a forgotten field *silent*: Rust forces
//! a struct literal to consider every field only until someone writes the rest-pattern, after which
//! "I did not think about this" and "I chose the default" are the same source text and the compiler
//! cannot tell them apart or ask.
//!
//! It had already happened five times when this was written. Four were found by an audit —
//! `noeta-ide`'s impact session, `noeta-mcp`'s `test` tool, the CLI's declared-tier run, and the
//! conformance runner, each omitting `package_uses` while supplying its inseparable neighbour
//! `packages`. The fifth was inside this crate: `check_all_session_opts` spelled the fields out and
//! stopped after `editions`, so every REPL and debug-console session silently ran with no package
//! map and no `@name` bindings.
//!
//! The fix is structural, and this file only guards it:
//!
//! - the three provenance fields are one value, [`noeta_check::Provenance`], so "supply two of
//!   three" is not a sentence you can write;
//! - both `Provenance` and `CheckOptions` are `#[non_exhaustive]` with no `Default`, so outside
//!   their home crates the only way to make one is a constructor that takes what it needs;
//! - inside `noeta-check`, `Config::of` destructures `CheckOptions` with no rest-pattern, so a new
//!   option is a compile error at one site rather than a default at three.
//!
//! What remains for a test is the meta-property the compiler cannot state: that those attributes are
//! *still there*. Deleting `#[non_exhaustive]` re-opens every call site at once, silently, and
//! nothing else in the tree would notice.

use std::path::{Path, PathBuf};

/// Every `.rs` file under `crates/`.
fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            // `target/` dirs (a per-crate override, or a stray one) hold generated code.
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .to_path_buf()
}

/// The `Name { … }` literals in one file, each as its own text block (from the brace to the matching
/// close), paired with the 1-based line the literal starts on.
///
/// Three things that look like a literal and are not, each of which this gate reported on its first
/// run: a declaration (`pub struct Name {`), an impl header, and — the one worth naming — a *return
/// type* (`fn f() -> Name {`), which is the most common way the name appears next to a brace in this
/// tree. A suffix match is the fourth: `Provenance {` also matches `RequireProvenance {`, an
/// unrelated `noeta-pm` enum, so the character before the name must not be part of an identifier.
fn struct_literals(text: &str, name: &str) -> Vec<(usize, String)> {
    let needle = format!("{name} {{");
    let mut out = Vec::new();
    let mut search = 0usize;
    while let Some(rel) = text[search..].find(&needle) {
        let at = search + rel;
        let open = at + needle.len() - 1;
        let head = &text[..at];
        let prefix = head.rsplit('\n').next().unwrap_or("");
        search = open + 1;
        // `RequireProvenance {` is not a `Provenance` literal.
        if head
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            continue;
        }
        let trimmed = prefix.trim_start();
        if prefix.contains("struct")
            || prefix.contains("impl")
            || prefix.contains("enum")
            // `fn f(…) -> Name {` opens a body, not a literal.
            || prefix.contains("->")
            || trimmed.starts_with("//")
            || trimmed.starts_with('*')
        {
            continue;
        }
        let mut depth = 0usize;
        let mut end = None;
        for (i, c) in text[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(open + i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else { continue };
        let line = text[..open].lines().count();
        out.push((line, text[open..end].to_string()));
    }
    out
}

/// `CheckOptions` and `Provenance` are `#[non_exhaustive]`, and neither can be `Default`-constructed.
///
/// This is the whole guard. Both properties are enforced by `rustc` at every call site *while they
/// hold*; deleting either attribute silently restores the world in which a dozen sites can each
/// forget a different field.
#[test]
fn the_two_option_structs_stay_closed_to_literals_and_defaults() {
    let cases = [
        (
            crates_dir().join("noeta-check/src/lib.rs"),
            "CheckOptions",
            "a check option",
        ),
        (
            crates_dir().join("noeta-edition/src/lib.rs"),
            "Provenance",
            "a provenance fact",
        ),
    ];
    for (file, name, what) in cases {
        let text = std::fs::read_to_string(&file).expect("the declaring source is readable");
        let decl = format!("pub struct {name} {{");
        let at = text
            .find(&decl)
            .unwrap_or_else(|| panic!("`{decl}` — has {name} moved? this gate is now vacuous"));
        // The attribute block immediately above the declaration — *attributes only*: the doc comment
        // above `Provenance` explains why it has no `Default`, and a window that swept prose read
        // its own justification as the violation.
        let head = &text[..at];
        let attrs: String = head
            .lines()
            .rev()
            .take_while(|l| l.trim_start().starts_with("#["))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            attrs.contains("#[non_exhaustive]"),
            "{name} is no longer `#[non_exhaustive]`, so any crate can write `{name} {{ … \
             ..Default::default() }}` again and forget {what} silently — which is how five \
             surfaces each shipped a wrong answer. Add the attribute back, or delete this gate \
             deliberately with a reason."
        );
        assert!(
            !attrs.contains("Default"),
            "{name} derives `Default`, which re-arms `..Default::default()` — the exact construct \
             that turned \"I did not consider this field\" into \"I chose the default\". Provide a \
             named constructor instead; an empty provenance is a decision, not a fallback."
        );
        assert!(
            !text.contains(&format!("impl Default for {name}")),
            "{name} has a hand-written `Default` impl — see the derive message above; the \
             objection is to the construct, not to how it is spelled"
        );
    }
}

/// No `CheckOptions` or `Provenance` literal is written outside the crate that declares it.
///
/// `#[non_exhaustive]` already makes this a compile error, so a failure here means either the
/// attribute went away (the test above says so first) or someone moved a type. It is cheap, and it
/// keeps the *claim* visible in a file whose whole subject is that claim.
#[test]
fn no_foreign_crate_writes_one_of_these_literals() {
    let crates_dir = crates_dir();
    let mut files = Vec::new();
    rust_sources(&crates_dir, &mut files);
    assert!(
        files.len() > 100,
        "the source walk found only {} files — it has stopped seeing the tree, so this gate is \
         passing vacuously",
        files.len()
    );

    let mut offenders = Vec::new();
    for file in &files {
        if file.ends_with("check_options_census.rs") {
            continue;
        }
        let rel = file.strip_prefix(&crates_dir).unwrap_or(file);
        let home = if rel.starts_with("noeta-check") {
            Some("CheckOptions")
        } else if rel.starts_with("noeta-edition") {
            Some("Provenance")
        } else {
            None
        };
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for name in ["CheckOptions", "Provenance"] {
            if home == Some(name) {
                continue;
            }
            for (line, _) in struct_literals(&text, name) {
                offenders.push(format!("{}:{line} — {name}", rel.display()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these sites write a literal of a type that is supposed to be constructor-only, so they can \
         supply a subset of its fields and let a rest-pattern answer for the others:\n  {}",
        offenders.join("\n  ")
    );
}

/// The scrape itself, against text this test owns — so a rewrite of it that silently matches nothing
/// fails here rather than turning the gate above into a no-op.
#[test]
fn the_literal_scrape_finds_literals_and_skips_declarations() {
    let sample = "\
pub struct CheckOptions {
    pub packages: PackageMap,
}
impl Default for CheckOptions {
    fn default() -> Self { todo!() }
}
fn a() {
    let x = CheckOptions {
        editions: e,
        ..CheckOptions::default()
    };
    let y = noeta_check::CheckOptions {
        provenance: p,
    };
}
";
    let found = struct_literals(sample, "CheckOptions");
    assert_eq!(
        found.len(),
        2,
        "the struct declaration and the impl header are not literals"
    );
    assert!(found[0].1.contains("editions:"));
    assert!(found[1].1.contains("provenance:"));
}
