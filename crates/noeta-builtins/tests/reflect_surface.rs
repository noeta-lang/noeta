//! **The reflection-surface census**: every reflection intrinsic is one [`ReflectKind`], carries one
//! declared operand shape, and is answered at every layer that has to answer it — with no wildcard
//! anywhere in between.
//!
//! ## The bug class
//!
//! The thirteen intrinsics used to be thirteen `Expr` variants, twelve `Rvalue` variants and a
//! scatter of opcodes, and by the time anyone counted they had reached **five different operand
//! contracts** between them: five took a name string, four took a value, two took a compile-time
//! `NameId` with no register at all, one took an *index* into `Module::type_args`, and one had two
//! bespoke opcodes written for it alone.
//!
//! The four with open bugs were exactly the four whose contract was its own. That is not a
//! coincidence — it is the mechanism. When the turbofish-forwarding capability landed, the five that
//! shared a contract came along for free and `attributes_of` structurally could not, because
//! `Op::TypeArgName` produces a *string* and `Op::AttributesOf` consumed an *int index*. A
//! capability added to one form could not propagate to the others, so each gap had to be found, and
//! diagnosed, and fixed, separately. Two of them were reported to users as the wrong reason
//! entirely: `roles_of::<E>()` on a forwarded parameter said *"requires a `@semantic` enum, but `E`
//! is not one"*, when `E` may well have been one at every call site and the real cause was that no
//! channel carried it.
//!
//! ## What is enforced, in order of strength
//!
//! 1. **The build.** There is one [`noeta_ast::Expr::Reflect`] node with a *fieldless*
//!    [`ReflectKind`], so every dispatch over the surface is an exhaustive `match` and a fourteenth
//!    intrinsic does not compile until each layer says what it means. This is the half no test can
//!    be forgotten around, and it is why the collapse was worth its edits.
//! 2. **The absence of a wildcard.** Exhaustiveness is only worth what the matches give up: a single
//!    `_ =>` arm turns "does not compile" back into "silently does the wrong thing". So this gate
//!    reads the four dispatches' own source and fails on a catch-all at the kind level. Rule 1 is
//!    the guarantee; this is the guarantee that rule 1 keeps holding.
//! 3. **The operand contract.** [`ReflectKind::shape`] declares which [`ReflectOperand`] arms each
//!    kind may carry, and [`ReflectShape::admits`] is checked over the entire (shape × arm) grid —
//!    every shape admits at least one arm, every arm is admitted by some shape, and only
//!    `OptionalType` admits two. A fourteenth kind cannot invent an eighth contract in place; it has
//!    to add one here, in front of this file.
//! 4. **Completeness against the lexer.** `ReflectKind::ALL` and the `ReservedRole::Reflection`
//!    words are the same set, spelled the same way. The lexer reserves the words; this is what stops
//!    a reserved word from having no kind, or a kind from having no word.
//! 5. **The named behavioural oracles.** The parser grid and the checker's live-buffer result-type
//!    tie live in `noeta-ide/tests/reflection_intrinsics.rs`; deleting either fails here rather than
//!    quietly leaving this census as the only guard.
//!
//! ## What this cannot catch
//!
//! A layer that handles a kind *wrongly* — an arm that compiles, matches, and answers the wrong
//! type. Nothing structural distinguishes it, and no text scan can. That is what rule 5's oracles
//! and the differential conformance runs are for; this file's job is to make sure a kind is
//! **reached** at every layer, which is the failure the four gaps actually were.
//!
//! The comment stripping below is line-oriented, so a `/* … */` block comment containing `_ =>`
//! inside one of the four dispatches would be a false positive. Said out loud rather than left
//! implied: this repo writes `//` comments, and a gate whose blind spots are undocumented is the
//! kind of thing this gate exists to prevent.
//!
//! The parsing trick (read the code's own source and count) is borrowed from
//! `noeta-check/tests/site_policies.rs` and `noeta-compiler/tests/pipeline_tables.rs`.

use noeta_ast::{Expr, ReflectKind, ReflectOperand, ReflectShape, TypeOperand, TypeRef};
use noeta_span::Span;
use std::path::{Path, PathBuf};

/// **The four dispatches** — every layer that must decide something per reflection kind, named by
/// the function that decides it.
///
/// One row per layer, and the rows are the argument: a kind is not "handled" because it type-checks,
/// it is handled because the checker gives it a type, lowering gives it operands, and *both*
/// backends evaluate it. A layer missing from this list is a layer nothing holds to the surface.
const DISPATCHES: &[(&str, &str, &str)] = &[
    (
        "the checker",
        "crates/noeta-check/src/expr/reflect.rs",
        "fn synth_reflect",
    ),
    (
        "lowering",
        "crates/noeta-ir/src/lower.rs",
        "fn lower_reflect",
    ),
    (
        "the bytecode backend",
        "crates/noeta-compiler/src/lib.rs",
        "fn compile_reflect",
    ),
    (
        "the reference interpreter",
        "crates/noeta-eval/src/ir.rs",
        "fn eval_reflect",
    ),
];

/// The behavioural oracles this census is not a substitute for. Named so deleting one fails here.
const ORACLES: &[(&str, &str)] = &[
    (
        "crates/noeta-ide/tests/reflection_intrinsics.rs",
        "fn the_call_surfaces_are_the_parsers_own",
    ),
    (
        "crates/noeta-ide/tests/reflection_intrinsics.rs",
        "fn the_result_types_are_the_checkers_own_answers",
    ),
    (
        "crates/noeta-ide/tests/reflection_intrinsics.rs",
        "fn every_intrinsic_completes_and_signature_helps",
    ),
];

fn workspace_root() -> PathBuf {
    // crates/noeta-builtins → crates → workspace root.
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

/// `src` with `//` line comments removed, so a comment mentioning a match arm cannot be read as one.
fn without_line_comments(src: &str) -> String {
    src.lines()
        .map(|line| match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The body of `match which {` inside the function introduced by `head`, brace-matched.
fn kind_dispatch_body(rel: &str, head: &str) -> String {
    let src = without_line_comments(&read(rel));
    let fn_at = src.find(head).unwrap_or_else(|| {
        panic!(
            "{rel} no longer declares `{head}` — the census names the four \
                                   dispatches that must answer for every reflection kind, so a \
                                   rename belongs in DISPATCHES"
        )
    });
    let rel_at = src[fn_at..].find("match which {").unwrap_or_else(|| {
        panic!(
            "`{head}` in {rel} no longer dispatches on `match which` — the \
                                   surface's exhaustiveness is what makes a fourteenth intrinsic a \
                                   compile error, and it is that match"
        )
    });
    let open = fn_at + rel_at + "match which {".len();
    let mut depth = 1usize;
    for (i, c) in src[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return src[open..open + i].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("`{head}`'s `match which` in {rel} is unbalanced");
}

/// Whether `body` has an arm head naming `kind` at the match's own level.
///
/// An *arm head*, not a mention: the variant must open a line (optionally after a `|`), which is
/// what distinguishes `ReflectKind::TypeOf =>` from the `Op::TypeOf { dst, src }` the backend
/// pushes inside it.
fn has_arm_for(body: &str, kind: ReflectKind) -> bool {
    let variant = format!("{kind:?}");
    body.lines().any(|line| {
        // The arm *head* — everything before `=>`. That is what keeps the `Op::TypeOf` a backend
        // arm pushes from counting as a `ReflectKind::TypeOf` arm, and it lets several kinds share
        // one head with `|`, which five of them do.
        let head = line.split("=>").next().unwrap_or("");
        // `K` is the conventional local alias in the three dispatches that also name `Op`/`Rvalue`.
        ["ReflectKind::", "K::"].iter().any(|prefix| {
            head.match_indices(*prefix).any(|(i, _)| {
                let before_ok = head[..i].chars().all(|c| {
                    c.is_whitespace() || c == '|' || c == ':' || c.is_alphanumeric() || c == '_'
                });
                let after = &head[i + prefix.len()..];
                before_ok
                    && after.strip_prefix(variant.as_str()).is_some_and(|rest| {
                        !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_')
                    })
            })
        })
    })
}

/// The `_` arm heads at `body`'s own brace depth — the catch-alls that would defeat exhaustiveness.
///
/// Depth-tracked rather than grepped, because the operand destructures *inside* the arms legitimately
/// end in `_ => mismatch()`: those match on [`ReflectOperand`], where a wildcard costs nothing
/// (the parser is the only constructor and rule 3 checks the contract). It is a wildcard over the
/// **kind** that turns a missing intrinsic from a compile error back into silence.
fn kind_level_wildcards(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut line_start = true;
    let mut current = String::new();
    for c in body.chars() {
        match c {
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => depth = depth.saturating_sub(1),
            '\n' => {
                if depth == 0 {
                    let t = current.trim();
                    if t.starts_with('_')
                        && !t.starts_with("_ =>")
                        && !t.starts_with("_=>")
                        && !t.starts_with("_ if")
                    {
                        // A binding named `_something` is not a wildcard.
                    } else if depth == 0 && (t.starts_with("_ =>") || t.starts_with("_ if")) {
                        out.push(t.to_string());
                    }
                }
                current.clear();
                line_start = true;
                continue;
            }
            _ => {}
        }
        if depth == 0 {
            current.push(c);
        }
        let _ = line_start;
        line_start = false;
    }
    out
}

/// One probe operand per [`ReflectOperand`] arm, for the (shape × arm) grid.
fn operand_arms() -> Vec<(&'static str, ReflectOperand)> {
    let ty = || TypeRef::Named {
        name: noeta_ast::Name::canonical("T".to_string()),
        args: Vec::new(),
        span: Span::new(0, 0),
    };
    let e = || {
        Box::new(Expr::Int {
            value: 0,
            span: Span::new(0, 0),
        })
    };
    vec![
        ("Nothing", ReflectOperand::Nothing),
        ("Type", ReflectOperand::Type(TypeOperand::Static(ty()))),
        ("Value", ReflectOperand::Value(e())),
        ("StaticType", ReflectOperand::StaticType(ty())),
        (
            "TypeWith",
            ReflectOperand::TypeWith {
                ty: TypeOperand::Static(ty()),
                arg: e(),
            },
        ),
        (
            "StaticTypeWith",
            ReflectOperand::StaticTypeWith { ty: ty(), arg: e() },
        ),
        (
            "Dispatch",
            ReflectOperand::Dispatch {
                recv: None,
                name: e(),
                args: e(),
            },
        ),
    ]
}

/// Rule 4 — the kinds and the lexer's reserved reflection words are the same set, spelled the same.
#[test]
fn every_reflection_reserved_word_is_exactly_one_kind() {
    let reserved: Vec<&'static str> = noeta_lexer::ReservedWord::all()
        .iter()
        .filter(|w| w.role == noeta_lexer::ReservedRole::Reflection)
        .map(|w| w.word)
        .collect();
    let kinds: Vec<&'static str> = ReflectKind::ALL.iter().map(|k| k.keyword()).collect();

    for word in &reserved {
        assert!(
            kinds.contains(word),
            "the lexer reserves `{word}` as a reflection primitive and no `ReflectKind` spells it — \
             a reserved word with no kind is a query the parser can accept and nothing downstream \
             can name"
        );
    }
    for kw in &kinds {
        assert!(
            reserved.contains(kw),
            "`ReflectKind` spells `{kw}` and the lexer does not reserve it as a reflection \
             primitive — either reserve the word or drop the kind"
        );
    }
    assert_eq!(
        reserved.len(),
        ReflectKind::ALL.len(),
        "the reflection words and the kinds must be the same set, not merely overlap"
    );
    // The declaration order is load-bearing for readers diffing the two lists side by side.
    assert_eq!(
        reserved, kinds,
        "`ReflectKind::ALL` is kept in the lexer's token-table order so the two lists read side by \
         side; reorder the kinds to match"
    );
}

/// Rule 3 — the operand contract is total, closed, and no two shapes overlap except where declared.
#[test]
fn every_kind_declares_one_operand_contract() {
    let arms = operand_arms();

    // Every shape admits at least one arm: a shape nothing satisfies is a contract no node can meet.
    for kind in ReflectKind::ALL {
        let shape = kind.shape();
        let admitted: Vec<&str> = arms
            .iter()
            .filter(|(_, op)| shape.admits(op))
            .map(|(name, _)| *name)
            .collect();
        assert!(
            !admitted.is_empty(),
            "`{}`'s {:?} shape admits no `ReflectOperand` arm — no parser production could build a \
             node for it",
            kind.keyword(),
            shape
        );
        // `OptionalType` is the one shape with two arms, and it earns them: `roles_of()`'s operand
        // really is optional in the grammar. Every other shape admits exactly one, so "which arm"
        // is a fact about the kind rather than a runtime guess.
        let expected = match shape {
            ReflectShape::OptionalType => 2,
            _ => 1,
        };
        assert_eq!(
            admitted.len(),
            expected,
            "`{}`'s {:?} shape admits {admitted:?}; only `OptionalType` may admit more than one arm",
            kind.keyword(),
            shape
        );
    }

    // Every arm is admitted by some kind: a `ReflectOperand` arm no kind can carry is dead weight
    // that a future intrinsic would find and reuse for something it was not designed for.
    for (name, op) in &arms {
        assert!(
            ReflectKind::ALL.iter().any(|k| k.shape().admits(op)),
            "`ReflectOperand::{name}` is admitted by no kind's shape — either give a kind that \
             contract or delete the arm"
        );
    }
}

/// Rule 2, the centrepiece — every kind reaches an arm at every layer, and no layer has a catch-all.
#[test]
fn every_kind_is_answered_at_every_layer_without_a_wildcard() {
    for (layer, file, head) in DISPATCHES {
        let body = kind_dispatch_body(file, head);

        let wildcards = kind_level_wildcards(&body);
        assert!(
            wildcards.is_empty(),
            "{layer}'s `{head}` ({file}) has a catch-all arm over `ReflectKind`: {wildcards:?}\n\
             \n\
             The whole point of collapsing thirteen variants into one node with a fieldless kind is \
             that a fourteenth intrinsic does not compile until every layer says what it means. A \
             `_ =>` here restores exactly the silence the collapse removed: the surface would \
             compile, and the new query would quietly do whatever this arm does.\n\
             \n\
             Write the arm out. If several kinds share behaviour, list them with `|`."
        );

        for kind in ReflectKind::ALL {
            assert!(
                has_arm_for(&body, kind),
                "{layer}'s `{head}` ({file}) has no arm naming `{}` — every reflection kind must be \
                 reached at every layer that decides something about it, and this file names the \
                 four that do",
                kind.keyword()
            );
        }
    }
}

/// Rule 5 — the behavioural oracles this census stands on still exist.
#[test]
fn the_behavioural_oracles_still_exist() {
    for (file, head) in ORACLES {
        let src = read(file);
        assert!(
            src.contains(head),
            "{file} no longer declares `{head}`.\n\
             \n\
             This census checks *shape*: that every kind is reached, and that no wildcard hides one. \
             It cannot check that an arm answers correctly. The oracles named here are what do — the \
             parser grid drives all thirteen keywords through the whole (turbofish? × arity) space, \
             and the completion/signature tie reads the checker's own answers over live buffers.\n\
             \n\
             If the oracle moved, update ORACLES. If it was deleted, this gate is now the only guard \
             on a surface that has already drifted four times."
        );
    }
}

/// The one property a reader would otherwise have to take on trust: `from_bytes` is the *only* kind
/// whose type operand a per-instantiation channel cannot answer, and the reason is its shape.
///
/// This is the fact the type-parameter forwarding walk keys on. It was previously re-derived at each
/// consumer, differently, which is how `roles_of::<E>()` and `from_bytes::<T>()` came to report two
/// different wrong reasons for the same missing channel.
#[test]
fn from_bytes_is_the_only_kind_a_channel_cannot_answer() {
    let by_name: Vec<&'static str> = ReflectKind::ALL
        .iter()
        .filter(|k| !k.shape().resolves_type_by_name())
        .map(|k| k.keyword())
        .collect();
    assert_eq!(
        by_name,
        vec!["from_bytes"],
        "exactly one reflection kind resolves its type operand by something other than a NAME, and \
         it is `from_bytes`: decoding an opaque buffer needs the element's packed *layout*, and \
         neither per-instantiation channel carries one (both carry names).\n\
         \n\
         If a new kind belongs on this list, it needs its own `E0058` message saying which channel \
         it wanted and why nothing carries it — the diagnostic `from_bytes` has. Sharing the \
         name-keyed one would tell the author to fix the wrong thing."
    );
}
