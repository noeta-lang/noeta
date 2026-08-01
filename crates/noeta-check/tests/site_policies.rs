//! **The site-census gate**: every field of the checker's [`Sites`] bundle must say whether it
//! carries an index a *live session* has to renumber.
//!
//! ## The bug class
//!
//! `noeta_compiler::TABLE_POLICIES` classifies every table the compile pipeline *keeps*, because
//! four shipped bugs lived in the delta between a cold whole-program compile and a session install
//! (a REPL entry, a hot swap). One of the four was "the type-argument table replaced where it had to
//! be merged": the session's `Module::type_args` is addressed **by index** from live runtime values,
//! a fresh whole-program check numbers its own table from zero in its own discovery order, and
//! adopting that numbering wholesale silently re-points every already-emitted hidden type argument
//! at a *different type*. Nothing crashes. The program answers with the wrong `T`.
//!
//! The fix — `SessionCompiler::absorb_type_args` — merges by content and rewrites the incoming
//! indices into session space. It is correct, and it covered exactly one field, because exactly one
//! field carried such an index. That fact was recorded in a doc comment:
//!
//! > `hidden_arg_sites` is the only `Sites` field that carries a type-arg TABLE index.
//!
//! A judgement about thirty-five fields, restated as prose, checked by nothing. And the thirty-sixth
//! field is not hypothetical: three fields already hold a `u32` that *looks* exactly like a table
//! index (`dynamic_construction_sites`, `forwarded_slot_sites`, `self_type_arg_sites` — a hidden
//! **slot** ordinal, a hidden slot ordinal, and a type parameter's declaration position), so
//! "`HashMap<Span, u32>` beside them" is the natural shape of any future call-site-typed feature. A
//! cold compile of one would be fine forever: the compiler is empty, the remap is the identity. Only
//! a REPL entry or a hot swap would be wrong, and only silently.
//!
//! ## What is enforced, in order of strength
//!
//! 1. **The build.** `Sites::remap_type_arg_indices` destructures the bundle with no `..`. A new
//!    field does not compile until its author puts it in one of the two arms. This is the half no
//!    test can be forgotten around, and it is why the remap lives beside `Sites` rather than in the
//!    compiler.
//! 2. **The type.** A table index is a [`noeta_ext_abi::TypeArgIndex`], not a `u32`. Any `Sites`
//!    field whose declared type mentions one — directly, or through an ABI type that carries one,
//!    which is how `hidden_arg_sites` does it via `HiddenArg::Table` — must be classified
//!    `TableIndexed`. A field that *says* it holds a table index cannot be classified as anything
//!    else.
//! 3. **The census.** `SITE_POLICIES` holds one row per field, and this file checks it against the
//!    struct (completeness, staleness, duplicates, a non-empty note) and against the *code*: the
//!    fields BOUND by the remap's destructure must be exactly the `TableIndexed` rows, and the
//!    fields `absorb_type_args` overwrites must be exactly the `TheTable` rows. The census cannot
//!    claim a remap the code does not perform, nor hide one it does.
//! 4. **Behaviour.** `noeta-vm/tests/hotswap.rs`'s
//!    `a_swap_that_renumbers_the_type_argument_table_still_resolves_every_T` forces a
//!    **non-identity** remap end to end and requires the swapped program to answer exactly as a cold
//!    start does. That is the only check here that watches meaning rather than shape.
//!
//! ## What this cannot catch
//!
//! A table index written as a **bare `u32`** and classified `Ordinal` or `SpanKeyed`. Nothing in the
//! type distinguishes it from the three genuine ordinals, and no text scan can. Rule 2 is what makes
//! that a deviation rather than the default; rule 4 is what catches it once the field reaches
//! lowering and some program exercises it. Stated here rather than left implied, because a census
//! that only counts names is weaker than one that checks a type, and a reader should know which
//! parts of this file are which.
//!
//! The parsing trick (read the type's own source and count) is borrowed from
//! `noeta-compiler/tests/pipeline_tables.rs` and `noeta-ir/tests/lowerer_field_census.rs`, the two
//! gates this one is the missing third of.
//!
//! [`Sites`]: noeta_check::Sites

use std::path::{Path, PathBuf};

const SITES: &str = "crates/noeta-check/src/sites.rs";
const COMPILER: &str = "crates/noeta-compiler/src/lib.rs";
const ABI: &str = "crates/noeta-ext-abi/src/registry.rs";
const HOTSWAP: &str = "crates/noeta-vm/tests/hotswap.rs";

/// The behavioural half — the one check in this family that watches *meaning*. Named here so
/// deleting it fails the gate rather than quietly leaving the census as the only guard.
const NON_IDENTITY_ORACLE: &str =
    "fn a_swap_that_renumbers_the_type_argument_table_still_resolves_every_t(";

fn workspace_root() -> PathBuf {
    // crates/noeta-check → crates → workspace root.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the workspace root is reachable from CARGO_MANIFEST_DIR");
    assert!(
        root.join("Cargo.toml").is_file() && root.join("crates").is_dir(),
        "expected a workspace root at {}; this gate reads the tree's sources",
        root.display()
    );
    root
}

fn read(rel: &str) -> String {
    let path = workspace_root().join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("gate input {} is unreadable: {e}", path.display()))
}

/// The `(field, declared type)` pairs of `struct <head>` in `src`, in declaration order.
///
/// Keyed off "a line at the struct's own indentation holding `name: Type,`", skipping doc comments
/// and attributes — and deliberately NOT brace-matched, because `Sites`'s doc comments contain
/// fenced code blocks with braces of their own. The struct's own closing brace is the first `}` at
/// column zero.
fn fields_with_types(src: &str, head: &str) -> Vec<(String, String)> {
    let start = src
        .find(head)
        .unwrap_or_else(|| panic!("`{head}` not found — did the type move or get renamed?"))
        + head.len();
    let body = &src[start..];
    let end = body
        .find("\n}")
        .unwrap_or_else(|| panic!("no closing brace for `{head}`"));
    body[..end]
        .lines()
        .filter(|l| l.starts_with("    ") && !l.starts_with("     "))
        .map(str::trim)
        .filter(|l| !l.starts_with("//") && !l.starts_with('#'))
        .map(|l| l.strip_prefix("pub ").unwrap_or(l))
        .filter_map(|l| l.split_once(':'))
        .map(|(name, ty)| {
            (
                name.trim().to_string(),
                ty.trim().trim_end_matches(',').to_string(),
            )
        })
        .filter(|(n, _)| {
            !n.is_empty()
                && n.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
        .collect()
}

/// The `(field, class)` rows of `SITE_POLICIES`, in order — with the note checked non-trivial here,
/// because a row whose note is `""` classifies nothing.
fn rows(src: &str) -> Vec<(String, String)> {
    let head = "pub(crate) const SITE_POLICIES: &[(&str, SiteClass, &str)] = &[";
    let start = src
        .find(head)
        .expect("`SITE_POLICIES` not found — the site census is the thing under gate")
        + head.len();
    let body = &src[start..];
    let end = body
        .find("\n];")
        .expect("no closing bracket for SITE_POLICIES");
    body[..end]
        .lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("(\""))
        .filter_map(|l| {
            let (name, rest) = l.split_once("\", ")?;
            let class = rest.strip_prefix("SiteClass::")?;
            let (class, note) = class.split_once(", ")?;
            assert!(
                note.trim_start_matches('"').len() > 8,
                "{name}: a census row must say what the field's integer payload (if any) actually \
                 indexes — \"the type-argument table\" being the one answer that means the row is \
                 misclassified"
            );
            Some((name.to_string(), class.to_string()))
        })
        .collect()
}

/// Every field of `SITE_POLICIES` with class `class`.
fn classified_as<'a>(rows: &'a [(String, String)], class: &str) -> Vec<&'a str> {
    rows.iter()
        .filter(|(_, c)| c == class)
        .map(|(f, _)| f.as_str())
        .collect()
}

/// The fields **bound** (rather than `_`-ignored) by the `let Sites { … } = self;` destructure in
/// `Sites::remap_type_arg_indices`. A binding there is a field the remap actually rewrites — Rust's
/// own unused-variable lint is what stops one from being bound and then ignored.
fn remapped_fields(src: &str) -> Vec<String> {
    let at = src
        .find("pub fn remap_type_arg_indices")
        .expect("`Sites::remap_type_arg_indices` not found — it IS the exhaustive census");
    let rest = &src[at..];
    let open = rest
        .find("let Sites {")
        .expect("the remap must destructure `Sites` exhaustively — that is the compile-time half");
    let body = &rest[open..];
    let end = body
        .find("} = self;")
        .expect("no end to the `let Sites { … } = self;` destructure");
    assert!(
        !body[..end].contains(".."),
        "`Sites::remap_type_arg_indices` destructures with `..` — that is exactly the hole this \
         gate exists to keep shut. A thirty-sixth field must fail to COMPILE until its author says \
         whether it carries a type-argument table index."
    );
    body[..end]
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("//") && !l.starts_with("let Sites"))
        .filter(|l| !l.ends_with(": _,"))
        .map(|l| l.trim_end_matches(',').to_string())
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        .collect()
}

/// The `Sites` fields `SessionCompiler::absorb_type_args` overwrites wholesale (`owned.<f> = …`) —
/// the tables it replaces with the session's merged superset rather than renumbering in place.
fn replaced_fields(src: &str) -> Vec<String> {
    let at = src
        .find("pub fn absorb_type_args")
        .expect("`SessionCompiler::absorb_type_args` not found — it is the obligation under gate");
    let body = body_after(&src[at..], "pub fn absorb_type_args");
    body.lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("owned."))
        .filter_map(|l| l.split_once(" = "))
        .map(|(f, _)| f.to_string())
        .filter(|n| n.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        .collect()
}

/// The `{ … }`-delimited body that follows `sig` in `src`, brace-matched.
fn body_after(src: &str, sig: &str) -> String {
    let at = src
        .find(sig)
        .unwrap_or_else(|| panic!("`{sig}` not found — did it move or get renamed?"));
    let rest = &src[at..];
    let open = rest.find('{').expect("a body follows the signature");
    let mut depth = 0usize;
    for (i, c) in rest[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return rest[open..=open + i].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces after `{sig}`");
}

/// The ABI types that **carry** a `TypeArgIndex` in a field or variant payload — `HiddenArg` today,
/// through `HiddenArg::Table`. Doc comments are stripped first, so a type that merely *mentions* the
/// newtype in prose is not counted; what is wanted is the set of type names whose appearance in a
/// `Sites` field's declared type means that field holds a table index.
fn type_arg_index_carriers(abi: &str) -> Vec<String> {
    let mut carriers = Vec::new();
    for (at, _) in abi
        .match_indices("pub enum ")
        .chain(abi.match_indices("pub struct "))
    {
        let rest = &abi[at..];
        // The header must open its body on its own line — a tuple struct (`pub struct X(u32);`)
        // has no field list to carry anything, and must not borrow the next item's brace.
        let header = rest.lines().next().unwrap_or_default();
        let Some(open) = header.find(" {") else {
            continue;
        };
        let name = header[..open]
            .rsplit(' ')
            .next()
            .unwrap_or_default()
            .split('<')
            .next()
            .unwrap_or_default()
            .to_string();
        let Some(end) = rest.find("\n}") else {
            continue;
        };
        let carries = rest[header.len()..end]
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//"))
            .any(|l| l.contains("TypeArgIndex"));
        if carries && !name.is_empty() {
            carriers.push(name);
        }
    }
    carriers
}

/// Adding a field to `Sites` must not be possible without saying what it carries.
#[test]
fn every_site_is_classified() {
    let src = read(SITES);
    let declared = fields_with_types(&src, "pub struct Sites {");
    let rows = rows(&src);

    let classified: Vec<&str> = rows.iter().map(|(f, _)| f.as_str()).collect();
    let missing: Vec<&String> = declared
        .iter()
        .map(|(f, _)| f)
        .filter(|f| !classified.contains(&f.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "these `Sites` fields are not classified in `SITE_POLICIES`: {missing:?}\n\
         Say what the field carries — the question is \"what does a LIVE session (a REPL entry, a \
         hot swap) have to renumber in it?\":\n  \
           TableIndexed — holds a `noeta_ext_abi::TypeArgIndex`. `absorb_type_args` MUST rewrite \
         it; add it to `Sites::remap_type_arg_indices`'s bound arm.\n  \
           TheTable     — the type-argument table itself, or a projection indexed in lockstep with \
         it. Replaced by the merged superset, not renumbered.\n  \
           Ordinal      — an integer that indexes something ELSE. NAME IT in the note: a hidden \
         SLOT ordinal (`$tyN`, resolved through the slot's runtime value), an argument position, a \
         type parameter's declaration index, a bit width, a count. If the honest answer is \"the \
         type-argument table\", the class is `TableIndexed` and the type should say `TypeArgIndex`.\n  \
           SpanKeyed    — span keys, and a payload with no integer index at all.\n  \
           Content      — not span-keyed: a `Vec` or a name-keyed table of pure payload."
    );

    let stale: Vec<&(String, String)> = rows
        .iter()
        .filter(|(f, _)| !declared.iter().any(|(d, _)| d == f))
        .collect();
    assert!(
        stale.is_empty(),
        "`SITE_POLICIES` classifies `Sites` fields that no longer exist (renamed or removed?): \
         {stale:?}"
    );

    let mut seen = classified.clone();
    seen.sort_unstable();
    let before = seen.len();
    seen.dedup();
    assert_eq!(
        before,
        seen.len(),
        "`SITE_POLICIES` classifies a field twice"
    );
}

/// The gate is only as good as its parsers: every one of them silently matching nothing would make
/// each assertion above pass vacuously. Pin a floor and the known members.
#[test]
fn the_parsers_actually_find_the_sites() {
    let src = read(SITES);
    let declared = fields_with_types(&src, "pub struct Sites {");
    assert!(
        declared.len() > 30,
        "expected the full `Sites` field list, parsed {}: {declared:?}",
        declared.len()
    );
    for expected in [
        "hidden_arg_sites",
        "type_arg_table",
        "forwarded_slot_sites",
        "destructor_relevance",
    ] {
        assert!(
            declared.iter().any(|(f, _)| f == expected),
            "the field scanner missed `{expected}` — a stale scanner passes everything"
        );
    }
    assert_eq!(
        rows(&src).len(),
        declared.len(),
        "one census row per declared field"
    );
    assert_eq!(
        remapped_fields(&src),
        ["hidden_arg_sites"],
        "the destructure scanner must find the bound field(s)"
    );
    assert_eq!(
        replaced_fields(&read(COMPILER)),
        ["type_arg_table", "type_arg_reprs"],
        "the absorb scanner must find the replaced tables"
    );
    assert_eq!(
        type_arg_index_carriers(&read(ABI)),
        ["HiddenArg"],
        "the carrier scanner must find the ABI types that hold a `TypeArgIndex`. A NEW name here \
         means some ABI type started carrying one — check whether a `Sites` field now does too, \
         and reclassify it if so."
    );
}

/// **The census against the code.** The fields the remap rewrites must be exactly the fields the
/// census says it rewrites — in both directions. A `TableIndexed` row nothing remaps is a promise
/// the pipeline does not keep; a remapped field the census calls `SpanKeyed` is the census lying
/// about the one thing it exists to record.
#[test]
fn the_remapped_fields_are_exactly_the_table_indexed_rows() {
    let src = read(SITES);
    let mut remapped = remapped_fields(&src);
    let mut declared = classified_as(&rows(&src), "TableIndexed")
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    remapped.sort();
    declared.sort();
    assert_eq!(
        remapped, declared,
        "`Sites::remap_type_arg_indices` rewrites {remapped:?} but `SITE_POLICIES` classifies \
         {declared:?} as `TableIndexed`.\n\
         These are one fact written twice and they have drifted. A field that carries a \
         `TypeArgIndex` must be bound by the destructure AND classified `TableIndexed`; a field \
         that does not must be `_`-ignored AND classified as what it really holds."
    );
}

/// The other half: `absorb_type_args` overwrites the tables themselves with the session's merged
/// superset, and those are exactly the `TheTable` rows. A table that stopped being replaced would
/// let a snapshot shrink out from under indices older code still holds.
#[test]
fn the_replaced_tables_are_exactly_the_the_table_rows() {
    let mut replaced = replaced_fields(&read(COMPILER));
    let mut declared = classified_as(&rows(&read(SITES)), "TheTable")
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    replaced.sort();
    declared.sort();
    assert_eq!(
        replaced, declared,
        "`SessionCompiler::absorb_type_args` overwrites {replaced:?} but `SITE_POLICIES` \
         classifies {declared:?} as `TheTable`.\n\
         Lowering embeds these verbatim into `Module::type_args` / `Module::type_arg_reprs`; they \
         must be the merged SUPERSET the session already holds, never the freshly-checked tables."
    );
}

/// **The type, not the comment.** A field whose declared type holds a `TypeArgIndex` — directly, or
/// through an ABI type that carries one — cannot be classified as anything but `TableIndexed`.
///
/// This is what the newtype bought. Before it, `HashMap<Span, u32>` was the spelling of a table
/// index *and* of three things that must never be remapped, and telling them apart was reading the
/// producer. Now writing the honest type is enough to make the classification checkable.
#[test]
fn a_field_carrying_a_type_arg_index_is_classified_table_indexed() {
    let src = read(SITES);
    let rows = rows(&src);
    let carriers = type_arg_index_carriers(&read(ABI));

    let offenders: Vec<String> = fields_with_types(&src, "pub struct Sites {")
        .into_iter()
        .filter(|(_, ty)| {
            ty.contains("TypeArgIndex") || carriers.iter().any(|c| ty.contains(c.as_str()))
        })
        .filter(|(f, _)| !rows.iter().any(|(n, c)| n == f && c == "TableIndexed"))
        .map(|(f, ty)| format!("{f}: {ty}"))
        .collect();

    assert!(
        offenders.is_empty(),
        "these `Sites` fields hold a type-argument TABLE index and are not classified \
         `TableIndexed`: {offenders:?}\n\
         The session's type-argument table is renumbered on every install that is not into an \
         empty compiler. An index that is not rewritten keeps pointing at whatever the OLD \
         numbering put there — the program runs on and answers with the wrong `T`. Classify it \
         `TableIndexed` and bind it in `Sites::remap_type_arg_indices`."
    );
}

/// The census is a *shape* check. The one check in this family that watches **meaning** is the
/// hot-swap oracle that forces a non-identity remap end to end, and it must not quietly disappear —
/// a shape gate outliving the behaviour gate is how a census comes to be mistaken for a proof.
#[test]
fn the_non_identity_remap_still_has_a_behavioural_oracle() {
    let src = read(HOTSWAP);
    assert!(
        src.contains(NON_IDENTITY_ORACLE),
        "the non-identity absorption oracle {NON_IDENTITY_ORACLE:?} is gone from {HOTSWAP}.\n\
         Everything else in this file checks that the census matches the code. That test is the \
         only one that checks the code is RIGHT: it swaps a program whose freshly-checked \
         type-argument table is numbered differently from the running session's, and requires the \
         swapped program to answer exactly as a cold start does. Without it, an unremapped table \
         index in a correctly-classified-looking field is invisible."
    );
}
