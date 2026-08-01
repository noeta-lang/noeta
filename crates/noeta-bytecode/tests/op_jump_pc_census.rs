//! **The `Op` jump-destination census**: every `u32`-shaped field of [`Op`] is classified here as
//! either a *jump pc* (a branch destination in `Chunk::code`) or not, and the classification is
//! checked against the declaration.
//!
//! ## The bug class
//!
//! "Which op carries a jump target" used to be hand-written in four places — the compiler's
//! `patch_jump`, `regalloc`'s LICM target fix-up and `op_facts`, and the JIT's tier-0 successor
//! function. Two of them ended in `_ => {}`, so a branching op missing from the list was not a
//! compile error, it was a wrong answer:
//!
//! * missed in the LICM rebuild's fix-up → the op keeps a **pre-rebuild** index and jumps to the
//!   wrong instruction (a silent miscompile);
//! * missed in the JIT's successors → a **missing CFG edge**, so liveness under-approximates and
//!   native code omits a spill it needed (unsound native code).
//!
//! Two of those files documented, in prose, that they listed "the same set" as the others. Nothing
//! checked it.
//!
//! The structural fix is [`Op::for_each_jump_pc`] / [`Op::for_each_jump_pc_mut`] in
//! `noeta-bytecode`: one exhaustive arm list, **no `_` catch-all**, that all four sites call. A new
//! `Op` *variant* is therefore a compile error at that one place.
//!
//! ## What this gate adds
//!
//! The compiler cannot see the other half: a new `u32` **field** on an *existing* variant. The arms
//! bind with `..`, so adding `fail: u32` to, say, `MatchFail` compiles fine and is silently never
//! remapped. That is what this census covers, in three properties:
//!
//! - **Completeness** — every field of `Op` whose type mentions `u32`/`usize` must be classified:
//!   either declared as [`JumpPc`] (and then visited by the shared arms) or listed in
//!   [`NOT_A_JUMP_PC`] below. A new one fails this test until its author says which it is.
//! - **Agreement** — the set of `JumpPc` fields in the declaration is exactly the set of fields
//!   the shared arms bind and pass to `f`. A `JumpPc` field that no arm visits fails here.
//! - **Exhaustiveness** — the shared arms mention every `Op` variant and contain no `_` arm, so the
//!   compile-error guarantee above is real rather than assumed.
//!
//! The technique — a test that parses its own crate's source text — is the one used by
//! `noeta-compiler/tests/pipeline_tables.rs` and `noeta-ir/tests/lowerer_field_census.rs`.

use std::collections::BTreeSet;
use std::path::Path;

/// Every `Op` field whose type mentions `u32`/`usize` and is **not** a jump destination, as
/// `"Variant.field"`, with what it indexes instead. Adding a `u32` field to `Op` without either
/// typing it `JumpPc` or adding it here fails [`every_u32_field_is_classified`].
///
/// The distinction is the whole point of the census: all of these are indices, none of them index
/// `Chunk::code`, and remapping one as if it did would corrupt a shape, a prototype or a cache.
const NOT_A_JUMP_PC: &[(&str, &str)] = &[
    ("MakeClosure.proto", "index into `Module::protos`"),
    ("MakeList.reflect", "index into the reflect table"),
    ("PackedListNew.schema", "index into the packed-schema table"),
    ("TupleIndex.index", "element position within the tuple"),
    ("MakeMap.reflect", "index into the reflect table"),
    ("CallMethod.cache", "inline-cache slot"),
    ("MakeStruct.shape", "index into the shape table"),
    ("MakeStruct.reflect", "index into the reflect table"),
    ("MakeStructInPlace.shape", "index into the shape table"),
    ("MakeStructInPlace.reflect", "index into the reflect table"),
    ("MakeEnum.shape", "index into the shape table"),
    ("MakeEnum.reflect", "index into the reflect table"),
    ("EnumFromStr.cases", "per-case index into the shape table"),
    ("EnumFromStr.some_shape", "index into the shape table"),
    ("EnumFromStr.none_shape", "index into the shape table"),
    ("LoadField.cache", "inline-cache slot"),
    ("Narrow.some_shape", "index into the shape table"),
    ("Narrow.none_shape", "index into the shape table"),
    ("Construct.ok_shape", "index into the shape table"),
    ("Construct.err_shape", "index into the shape table"),
    ("Retag.repr", "index into the reflect table"),
    ("TypeArgName.index", "type-argument position"),
    ("FromBytes.schema", "index into the packed-schema table"),
    ("Invoke.ok_shape", "index into the shape table"),
    ("Invoke.err_shape", "index into the shape table"),
    ("DecodeTyped.ok_shape", "index into the shape table"),
    ("DecodeTyped.err_shape", "index into the shape table"),
];

/// The marker comment inside `for_each_jump_pc_arms!` that separates the arms which pass a
/// destination to `f` from the catch-everything-else arm.
const REST_MARKER: &str = "every remaining variant carries no jump pc";

fn source() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// The body of a braced item starting at the first `{` at or after `from`, brace-matched.
fn braced_body(src: &str, from: usize) -> &str {
    let start = src[from..].find('{').expect("a `{` after the item header") + from;
    let mut depth = 0usize;
    for (i, c) in src[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &src[start + 1..start + i];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces");
}

/// `(variant, field, type)` for every field of every `Op` variant, plus the full variant list, read
/// out of the `pub enum Op` declaration.
fn declared_op() -> (Vec<(String, String, String)>, BTreeSet<String>) {
    let src = source();
    let at = src
        .find("pub enum Op {")
        .expect("`pub enum Op` is declared in noeta-bytecode/src/lib.rs");
    let body = braced_body(&src, at).to_string();

    let mut fields = Vec::new();
    let mut variants = BTreeSet::new();
    let mut current: Option<String> = None;
    for line in body.lines() {
        let line = line.trim();
        match &current {
            None => {
                // `Variant {` opens a struct variant; `Variant,` is a unit variant.
                if let Some(name) = line.strip_suffix(" {").filter(|n| is_variant_name(n)) {
                    variants.insert(name.to_string());
                    current = Some(name.to_string());
                } else if let Some(name) = line.strip_suffix(',').filter(|n| is_variant_name(n)) {
                    variants.insert(name.to_string());
                }
            }
            Some(variant) => {
                if line == "}," {
                    current = None;
                } else if let Some((name, ty)) = line.split_once(": ")
                    && let Some(ty) = ty.strip_suffix(',')
                    && name.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                {
                    fields.push((variant.clone(), name.to_string(), ty.to_string()));
                }
            }
        }
    }
    assert!(
        variants.len() > 90,
        "the `Op` parse found only {} variants — the declaration's shape changed and this census \
         is no longer reading it",
        variants.len()
    );
    (fields, variants)
}

fn is_variant_name(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_uppercase()) && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// The macro body split into (arms that hand a destination to `f`, the rest arm).
fn shared_arms() -> (String, String) {
    let src = source();
    let at = src
        .find("macro_rules! for_each_jump_pc_arms")
        .expect("the shared arm list is declared in noeta-bytecode/src/lib.rs");
    let body = braced_body(&src, at).to_string();
    let split = body
        .find(REST_MARKER)
        .unwrap_or_else(|| panic!("the marker comment `{REST_MARKER}` is gone from the arm list"));
    let (carrying, rest) = body.split_at(split);
    (carrying.to_string(), rest.to_string())
}

/// `Op::Variant { a, b, .. }` occurrences in `text`, as `(variant, bound field names)`.
fn patterns(text: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("Op::") {
        rest = &rest[i + 4..];
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(rest.len());
        let variant = rest[..end].to_string();
        rest = &rest[end..];
        let mut bound = Vec::new();
        if rest.trim_start().starts_with('{') {
            let open = rest.find('{').unwrap();
            let close = rest[open..].find('}').expect("a closed pattern") + open;
            for part in rest[open + 1..close].split(',') {
                let part = part.trim();
                if !part.is_empty() && part != ".." {
                    bound.push(part.to_string());
                }
            }
            rest = &rest[close + 1..];
        }
        out.push((variant, bound));
    }
    out
}

/// Every `u32`-shaped field of `Op` is either declared `JumpPc` or listed in
/// [`NOT_A_JUMP_PC`]. A new index field on `Op` fails here until it is classified.
#[test]
fn every_u32_field_is_classified() {
    let (fields, _) = declared_op();
    let listed: BTreeSet<String> = NOT_A_JUMP_PC.iter().map(|(k, _)| k.to_string()).collect();
    assert_eq!(
        listed.len(),
        NOT_A_JUMP_PC.len(),
        "NOT_A_JUMP_PC has a duplicate entry"
    );

    let mut unclassified = Vec::new();
    let mut seen = BTreeSet::new();
    for (variant, name, ty) in &fields {
        let numeric = ty.contains("u32") || ty.contains("usize");
        if !numeric && !ty.contains("JumpPc") {
            continue;
        }
        let key = format!("{variant}.{name}");
        if ty == "JumpPc" {
            assert!(
                !listed.contains(&key),
                "{key} is declared `JumpPc` but also listed in NOT_A_JUMP_PC"
            );
            continue;
        }
        if listed.contains(&key) {
            seen.insert(key);
        } else {
            unclassified.push(format!("{key}: {ty}"));
        }
    }

    assert!(
        unclassified.is_empty(),
        "these `Op` fields index something and are unclassified:\n  {}\n\nEach is either a *code \
         index* — a destination in `Chunk::code`, which must be declared `JumpPc` and visited \
         by `for_each_jump_pc_arms!`, or it is not, and belongs in this test's \
         NOT_A_JUMP_PC list with a note on what it indexes.",
        unclassified.join("\n  ")
    );

    let stale: Vec<&String> = listed.difference(&seen).collect();
    assert!(
        stale.is_empty(),
        "NOT_A_JUMP_PC lists fields that are no longer declared that way: {stale:?}"
    );
}

/// The `JumpPc` fields in the declaration are exactly the fields the shared arms hand to `f`.
/// This is the count the audit asked for, by name rather than by number.
#[test]
fn declared_jump_pcs_match_the_shared_arms() {
    let (fields, _) = declared_op();
    let declared: BTreeSet<String> = fields
        .iter()
        .filter(|(_, _, ty)| ty == "JumpPc")
        .map(|(v, n, _)| format!("{v}.{n}"))
        .collect();

    let (carrying, _) = shared_arms();
    let visited: BTreeSet<String> = patterns(&carrying)
        .into_iter()
        .flat_map(|(variant, bound)| {
            bound
                .into_iter()
                .map(move |f| format!("{variant}.{f}"))
                .collect::<Vec<_>>()
        })
        .collect();

    assert_eq!(
        declared, visited,
        "the `JumpPc` fields declared on `Op` and the fields `for_each_jump_pc_arms!` hands \
         to `f` have diverged — a declared destination that no arm visits is never remapped (a \
         silent miscompile in the LICM rebuild) and never yields a CFG edge (an unsound spill \
         omission in the JIT)"
    );
    assert_eq!(
        declared.len(),
        10,
        "the jump-destination set changed size; that is allowed, but update this number deliberately so \
         the change is reviewed: {declared:?}"
    );
}

/// The shared arms name every `Op` variant and have no `_` catch-all — which is what makes a new
/// variant a compile error there rather than a silent omission at the four call sites.
#[test]
fn the_shared_arms_are_exhaustive_without_a_wildcard() {
    let (_, variants) = declared_op();
    let (carrying, rest) = shared_arms();

    for arm in [&carrying, &rest] {
        for line in arm.lines() {
            let line = line.trim();
            assert!(
                !line.starts_with("_ =>") && !line.starts_with("| _"),
                "`for_each_jump_pc_arms!` grew a catch-all arm (`{line}`) — that is exactly the \
                 shape this chokepoint exists to remove: a new `Op` variant would compile and \
                 silently carry no jump target"
            );
        }
    }

    let mut mentioned: BTreeSet<String> = BTreeSet::new();
    for (variant, _) in patterns(&carrying).into_iter().chain(patterns(&rest)) {
        assert!(
            mentioned.insert(variant.clone()),
            "`{variant}` appears twice in the shared arm list"
        );
    }

    let missing: Vec<&String> = variants.difference(&mentioned).collect();
    assert!(
        missing.is_empty(),
        "the shared arm list does not mention these `Op` variants: {missing:?} (the compiler would \
         normally catch this — if it did not, the census is mis-parsing the declaration)"
    );
    let unknown: Vec<&String> = mentioned.difference(&variants).collect();
    assert!(
        unknown.is_empty(),
        "the shared arm list mentions non-variants: {unknown:?}"
    );
}

/// The structural census reads *names*; this reads *values*. Each branching op is built with a
/// recognisable destination and must report exactly that one back, and the `_mut` form must
/// rewrite the very field the interpreter later branches on — the wiring the arm list's shape
/// cannot prove (an arm could bind the right field and hand `f` a different one).
#[test]
fn every_branching_op_reports_and_rewrites_its_own_destination() {
    use noeta_bytecode::{NameId, Op};
    use noeta_span::Span;

    let span = Span::new(0, 0);
    let name = NameId(0);
    // One op per branching variant, each with a distinct destination.
    let mut ops = vec![
        Op::Jump { target: 1 },
        Op::JumpIfTrue { reg: 0, target: 2 },
        Op::JumpIfFalse { reg: 0, target: 3 },
        Op::CondBranch {
            reg: 0,
            target: 4,
            span,
        },
        Op::Coalesce {
            dst: 0,
            src: 0,
            fallback: 5,
            span,
        },
        Op::MatchInt {
            src: 0,
            value: 0,
            fail: 6,
        },
        Op::MatchStr {
            src: 0,
            value: name,
            fail: 7,
        },
        Op::MatchBool {
            src: 0,
            value: false,
            fail: 8,
        },
        Op::MatchVariant {
            src: 0,
            type_name: None,
            variant: name,
            arity: 0,
            fail: 9,
        },
        Op::MatchTuple {
            src: 0,
            arity: 0,
            fail: 10,
        },
    ];
    assert_eq!(
        ops.len(),
        10,
        "one probe per branching variant; add one when the set grows"
    );

    for (i, op) in ops.iter().enumerate() {
        let mut seen = Vec::new();
        op.for_each_jump_pc(|t| seen.push(t));
        assert_eq!(
            seen,
            vec![i as u32 + 1],
            "op #{i} reported the wrong destination — its arm binds one field and hands `f` another"
        );
        assert!(op.has_jump_pc(), "op #{i} must report as branching");
    }

    // The `_mut` form must reach the same field: rewrite, then read back through the `&self` form.
    for op in &mut ops {
        op.for_each_jump_pc_mut(|t| *t += 100);
    }
    for (i, op) in ops.iter().enumerate() {
        let mut seen = Vec::new();
        op.for_each_jump_pc(|t| seen.push(t));
        assert_eq!(
            seen,
            vec![i as u32 + 101],
            "op #{i}'s `_mut` form missed it"
        );
    }

    // A representative non-branching op reports nothing (and would be a compile error if the arms
    // stopped covering it).
    let inert = Op::Halt;
    let mut seen = Vec::new();
    inert.for_each_jump_pc(|t| seen.push(t));
    assert!(seen.is_empty() && !inert.has_jump_pc());
}
