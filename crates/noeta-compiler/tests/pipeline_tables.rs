//! **The pipeline-table gate**: every table the compile pipeline keeps must state what a *second*
//! install does to it.
//!
//! ## The bug class
//!
//! `compile_to_mc` (a cold whole-program compile) and `SessionCompiler::extend_impl` (a REPL entry
//! or a hot-swap install) used to be two implementations of one sequence — hoist, lower, drops,
//! reuse, the tables, the entry chunk — differing only in "build a fresh table" versus "accumulate
//! into the live one". Four shipped bugs lived in that delta, every one of them a table the author
//! of the cold path handled and the author of the hot path did not (or handled differently):
//!
//! 1. lowering facts (`ProgramFacts`) — the cold path had them for free, so a swapped `@html`
//!    lowered to a panic and `x is Uuid` answered `false`;
//! 2. the checker's sites — optional on the hot path, so swapped code silently compiled
//!    conservative and computed different answers;
//! 3. the type-argument table — built on one side, wholesale *replaced* on the other, re-pointing
//!    indices that live values hold;
//! 4. packed schemas — interned on the cold path only.
//!
//! The two functions are now one (`SessionCompiler::install`, run against an empty compiler for a
//! cold compile and a live one for a session install), which removes the *place* a fifth divergence
//! could live. This gate removes the place a fifth **omission** could live: `TABLE_POLICIES` in
//! `lib.rs` classifies every field of `ModuleCompiler` and `SessionCompiler`, and adding a field
//! without classifying it fails here — at the commit that adds it, not whenever someone next reads
//! the file.
//!
//! The trick (parse the type's own source and count) is borrowed from `noeta-ext-abi`'s
//! declared-constraint gate and `noeta_diagnostics`'s `all_list_guard`.

use std::path::Path;

fn compiler_source() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} is unreadable: {e}", path.display()))
}

/// The field names of `struct <ty>` in `src`, in declaration order. Fields here are private, so —
/// unlike the ABI gate's `pub_fields` — this takes every `name: Type,` line at the struct's own
/// indentation, skipping doc comments and attributes.
fn fields(src: &str, ty: &str) -> Vec<String> {
    let head = format!("struct {ty} {{");
    let start = src
        .find(&head)
        .unwrap_or_else(|| panic!("`{head}` not found — did the type move or get renamed?"))
        + head.len();
    let body = &src[start..];
    let end = body
        .find("\n}")
        .unwrap_or_else(|| panic!("no closing brace for `{ty}`"));
    body[..end]
        .lines()
        .filter(|l| l.starts_with("    ") && !l.starts_with("     "))
        .map(str::trim)
        .filter(|l| !l.starts_with("//") && !l.starts_with('#'))
        .filter_map(|l| l.split_once(':').map(|(name, _)| name.trim().to_string()))
        .filter(|n| {
            !n.is_empty()
                && n.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
        .collect()
}

/// The `(field, policy)` rows of `TABLE_POLICIES`, in order.
fn rows(src: &str) -> Vec<(String, String)> {
    let head = "const TABLE_POLICIES: &[(&str, Policy, &str)] = &[";
    let start = src
        .find(head)
        .expect("`TABLE_POLICIES` not found — the pipeline's policy table is the thing under gate")
        + head.len();
    let body = &src[start..];
    let end = body
        .find("\n];")
        .expect("no closing bracket for TABLE_POLICIES");
    body[..end]
        .lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("(\""))
        .filter_map(|l| {
            let (name, rest) = l.split_once("\", ")?;
            let policy = rest.strip_prefix("Policy::")?;
            let (policy, note) = policy.split_once(", ")?;
            assert!(
                note.trim_start_matches('"').len() > 2,
                "{name}: a policy row must say WHERE the table is installed"
            );
            Some((name.to_string(), policy.to_string()))
        })
        .collect()
}

#[test]
fn every_table_states_its_policy() {
    let src = compiler_source();
    let mut declared = fields(&src, "ModuleCompiler");
    declared.extend(fields(&src, "SessionCompiler"));
    let rows = rows(&src);

    let classified: Vec<&String> = rows.iter().map(|(name, _)| name).collect();
    let missing: Vec<&String> = declared
        .iter()
        .filter(|f| !classified.contains(f))
        .collect();
    assert!(
        missing.is_empty(),
        "these compiler tables are not classified in `TABLE_POLICIES`: {missing:?}\n\
         A table the pipeline keeps must say what a SECOND install does to it — the whole bug \
         class is a table handled on the cold path and forgotten on the hot one. Add a row:\n  \
           Replace        — overwritten wholesale (only where the incoming value is itself \
         cumulative and indices are append-only)\n  \
           MergeByKey     — keyed insert, latest-wins\n  \
           MergeByContent — interned, an existing entry keeps its index\n  \
           Append         — new ids at the end, never renumbering\n  \
           Recomputed     — rebuilt by its own pass from accumulated inputs\n  \
           Fixed          — install-invariant configuration\n  \
           Derived        — not stored: computed per install"
    );

    let stale: Vec<&(String, String)> = rows
        .iter()
        .filter(|(name, policy)| policy != "Derived" && !declared.contains(name))
        .collect();
    assert!(
        stale.is_empty(),
        "`TABLE_POLICIES` classifies tables that no longer exist (renamed or removed?): {stale:?}"
    );

    let mut seen: Vec<&String> = classified.clone();
    seen.sort();
    let before = seen.len();
    seen.dedup();
    assert_eq!(
        before,
        seen.len(),
        "`TABLE_POLICIES` classifies a table twice"
    );
}

/// The gate is only as good as its parser: if `fields` silently matched nothing, everything above
/// would pass vacuously. Pin a floor and a few known members.
#[test]
fn the_parser_actually_finds_the_tables() {
    let src = compiler_source();
    let mc = fields(&src, "ModuleCompiler");
    assert!(
        mc.len() > 20,
        "expected the full ModuleCompiler field list, parsed {}: {mc:?}",
        mc.len()
    );
    for expected in ["protos", "shapes", "type_args", "registry"] {
        assert!(mc.contains(&expected.to_string()), "missing {expected}");
    }
    let session = fields(&src, "SessionCompiler");
    assert_eq!(session, ["mc", "map_packed", "reflection", "facts"]);
    assert!(rows(&src).len() >= mc.len() + session.len());
}
