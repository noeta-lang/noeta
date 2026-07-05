# Changelog

## 0.1.0

First release. Static TextMate grammar and editor configuration for `.noe`:

- Syntax highlighting for the full Noeta surface — keywords, the three string
  forms with `${…}` interpolation, every numeric literal form (decimal, hex,
  octal, binary, floats, `f32` and `i8`…`u64` suffixes), primitive and container
  types, PascalCase user types, `@directive`/tier blocks, `#[attribute]`s, and
  the full operator set (`|>`, `..`/`..=`, `...`, `??`/`??=`).
- Comment toggling, bracket matching, auto-closing pairs, and indentation rules.

No language server yet — semantic features arrive with `noeta lsp`.
