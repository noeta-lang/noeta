//! **`packages` and `package_uses` are two halves of one answer, and `..Default::default()` is what
//! lets a call site supply one of them.**
//!
//! [`noeta_check::CheckOptions`] is assembled by hand at a dozen call sites, every one of them
//! ending in `..Default::default()`. That is what makes a forgotten field *silent*: Rust forces a
//! struct literal to consider every field only until someone writes the rest-pattern, after which
//! "I did not think about this" and "I chose the default" are the same source text and the compiler
//! cannot tell them apart or ask.
//!
//! For most fields the default is genuinely right. For `package_uses` it is right **only** when
//! `packages` is also absent. Both are per-source provenance the loader/pm resolve together:
//! `packages` says which package wrote each source, and `package_uses` is that package's
//! `[directives]`/`[tiers]` table — which local `@name` means which extension. Empty `packages`
//! means "provenance unknown", and every provenance rule stands down, which is the correct answer
//! for a single-file check. Empty `package_uses` does **not** mean unknown: it means *no package
//! binds any `@name`*, so a renamed directive or tier resolves to nothing and the checker reports
//! E0036 on a project the compiler accepts. A site that knows enough to fill in `packages` has the
//! `@name` tables in hand from the very same resolve, so supplying one and not the other is never a
//! deliberate choice.
//!
//! It had already happened four times when this gate was written — `noeta-ide`'s impact session,
//! `noeta-mcp`'s `test` tool, the CLI's declared-tier run, and the conformance runner — which is
//! four surfaces reporting an error the compiler does not. This reads the source text the way
//! `noeta-diagnostics`' `ALL` gate and `noeta-compiler`'s `pipeline_tables` do, because the language
//! has no way to state the pairing as a type.
//!
//! The proper fix is a constructor — `CheckOptions::for_workspace(editions, packages, package_uses)`
//! plus `#[non_exhaustive]`, so provenance arrives as an argument and cannot be half-supplied. This
//! gate holds the line until then, and stays useful after it as the check that no literal has crept
//! back.

use std::path::{Path, PathBuf};

/// Every `.rs` file under `crates/`, excluding this test's own source.
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

/// The `CheckOptions { … }` literals in one file, each as its own text block (from the brace to the
/// matching close), paired with the 1-based line the literal starts on.
fn check_options_literals(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut search = 0usize;
    while let Some(rel) = text[search..].find("CheckOptions {") {
        let open = search + rel + "CheckOptions ".len();
        // `pub struct CheckOptions {` / `impl … for CheckOptions {` are declarations, not literals.
        let head = &text[..search + rel];
        let prefix = head.rsplit('\n').next().unwrap_or("");
        search = open + 1;
        if prefix.contains("struct") || prefix.contains("impl") {
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

/// Every `CheckOptions` literal that supplies `packages` supplies `package_uses` too.
#[test]
fn a_check_options_literal_never_supplies_packages_without_package_uses() {
    let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .to_path_buf();
    let mut files = Vec::new();
    rust_sources(&crates_dir, &mut files);
    assert!(
        files.len() > 100,
        "the source walk found only {} files — it has stopped seeing the tree, so this gate is \
         passing vacuously",
        files.len()
    );

    let mut seen = 0usize;
    let mut offenders = Vec::new();
    for file in &files {
        if file.ends_with("check_options_census.rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for (line, literal) in check_options_literals(&text) {
            seen += 1;
            // `packages:` and not `package_uses:` — the substring order matters, so match the
            // field name with its colon.
            let has_packages = literal.contains("packages:");
            let has_uses = literal.contains("package_uses:");
            if has_packages && !has_uses {
                offenders.push(format!(
                    "{}:{line}",
                    file.strip_prefix(&crates_dir).unwrap_or(file).display()
                ));
            }
        }
    }

    assert!(
        seen >= 10,
        "the literal scan found only {seen} `CheckOptions {{ … }}` sites — the scrape has stopped \
         matching the source, so this gate is passing vacuously"
    );
    assert!(
        offenders.is_empty(),
        "these `CheckOptions` literals set `packages` and let `..Default::default()` answer for \
         `package_uses`, which is not the same claim — an empty `package_uses` means no package \
         binds any `@name`, so a renamed directive or tier reports a spurious E0036 there:\n  {}",
        offenders.join("\n  ")
    );
}

/// The scrape itself, against text this test owns — so a rewrite of it that silently matches
/// nothing fails here rather than turning the gate above into a no-op.
#[test]
fn the_literal_scrape_finds_literals_and_skips_declarations() {
    let sample = "\
pub struct CheckOptions {
    pub packages: PackageMap,
}
fn a() {
    let x = CheckOptions {
        editions: e,
        packages: p,
        ..CheckOptions::default()
    };
    let y = noeta_check::CheckOptions {
        packages: p,
        package_uses: u,
        ..Default::default()
    };
}
";
    let found = check_options_literals(sample);
    assert_eq!(found.len(), 2, "the struct declaration is not a literal");
    assert!(found[0].1.contains("packages:") && !found[0].1.contains("package_uses:"));
    assert!(found[1].1.contains("package_uses:"));
}
