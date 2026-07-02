# Decompose the parser's giant closures (`lang-parser/src/lib.rs`, 3679 LOC)

Status: assessed — recommend leaving as-is (highest-risk, negative payoff)

chumsky combinators capture each other, so extracting sub-builders means
unnameable generic `impl Parser` return types and hand-threaded recursion, and
precedence / error-recovery is easy to perturb invisibly. A single cohesive
grammar function is a defensible (often preferable) shape for a combinator
parser. Revisit only if a specific sub-builder (`fn_decl`/`class_body`) becomes
independently useful; do not do the mechanical whole-file split.

The literal-parsing concern was already lifted to `literals.rs`. What remains is
the hardest part of the parser file: two enormous chumsky combinator functions —
`statement_parser` (~1164 lines, line 1859) and `expr_with` (~744 lines, line
1043) — each a single `recursive` closure holding the entire statement / pratt
grammar inline. This track breaks them into sub-builders **without changing the
grammar**.

## Goal

`statement_parser` and `expr_with` become thin assemblers that combine named
sub-builder functions (each `fn <name>_parser<'src, I>(ctx, stmt) -> impl
Parser<…>`), mirroring the already-extracted `type_parser` (773) and
`pattern_parser` (894). No behavior change: the same tokens parse to the same AST.

## Scope

- **In:**
  - Split `statement_parser` into sub-builders: `fn_decl_parser`,
    `class_body_parser`, `decorator_parser`, `assign_parser`, the `@tier`-block
    parsers — each taking `ctx` (+ the recursive `stmt`/`expr` parsers it needs).
  - Split `expr_with` into: the literal/keyword-atom builders (`attributes_of`/
    `type_of`/`from_bytes`/`channel`/`invoke`/…) and the pratt operator table,
    lifted out of the one closure.
  - Optionally then move the sub-builders (and `type_parser`/`pattern_parser`)
    into `types.rs`/`patterns.rs`/`expr.rs`/`stmt.rs`/`decorators.rs` modules — a
    file split once the functions are small enough to relocate cleanly.
- **Out:** any grammar change; the `Ctx`/`Extra`/`T` plumbing (unchanged).

## Design

The combinators are already free functions parameterized by `Ctx` and returning
`impl Parser<…> + Clone`, so extracting a sub-builder is mechanical *in principle*
— but chumsky's `recursive` closures capture each other (the statement parser
needs the expression parser and vice-versa), and the return types are verbose
generic `impl Parser` signatures. Each extracted sub-builder must take the
parsers it references as parameters (as `type_parser`/`pattern_parser` already
do), so the recursion is threaded explicitly rather than by closure capture.

This is why it was deferred: it is **higher-risk than the other splits** — a
mis-threaded recursive parser can change error-recovery or precedence subtly. The
parser snapshot tests + the differential are the safety net; make small
extractions and re-run both after each.

## Risks & constraints

- **Highest-risk of the four splits.** chumsky's generic `impl Parser` return
  types are unwieldy, and precedence / error-recovery behavior is easy to perturb.
  Extract one sub-builder at a time; run the 54 parser tests (insta snapshots) +
  the differential after each; never batch.
- If a sub-builder's return type becomes intractable to name, that piece can stay
  inline — partial decomposition is acceptable. Prioritize the clearly-separable
  builders (`fn_decl`, `class_body`, `decorator`) over deeply-entangled ones.

## Checklist

- [ ] `statement_parser` sub-builders extracted (fn/class/decorator/assign/tier)
- [ ] `expr_with` split (atoms + pratt table)
- [ ] (optional) sub-builders relocated to `types`/`patterns`/`expr`/`stmt`/`decorators` modules
- [ ] 54 parser tests (insta snapshots) green after each extraction
- [ ] differential 417/0, backends agree; clippy `--all-targets` + fmt clean

## Definition of done

`statement_parser` and `expr_with` are assemblers over named sub-builders (each a
few dozen lines), the parser snapshot tests and the differential are unchanged,
and all gates green. Full module relocation is optional; shrinking the two giant
functions is the core win.
