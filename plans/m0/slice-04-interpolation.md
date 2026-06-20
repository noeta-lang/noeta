# Slice 4 — String interpolation

Status: todo

## Goal
`"Hello {name}"` brace interpolation, with nested expressions inside `{ }`.

## Scope
- In: double-quoted interpolation `"... {expr} ..."`; `{{`/`}}` (or chosen) escape for literal braces; expressions inside holes are full expressions. Optionally backtick multiline `` `...` `` (may defer within M0).
- Out: format specifiers / alignment.

## Checklist (vertical slice)
- [ ] Grammar / AST: lexer emits string-fragment + hole-marker tokens (logos can't recurse); parser stitches fragments and hole expressions into an interpolation AST node.
- [ ] Checker rule: n/a.
- [ ] Bytecode: n/a.
- [ ] Eval op: evaluate each hole, stringify (M0 built-in stringification; the `Display` trait is M1), concatenate.
- [ ] Conformance cases: interpolation with a variable and with a nested expression.
- [ ] Snapshots: **token stream** for an interpolated string (this is a known regression magnet — snapshot deliberately).

## Notes / traps
- Do not attempt to fully lex interpolation in one token; fragment + hole tokens stitched in the parser is the supported approach.
- Revisit Slice 3's enumerate conformance case to use real interpolation now.

## Definition of done
- Conformance cases pass for interpolation; token-stream snapshot reviewed.
- fmt/clippy clean; zero `unsafe`.
