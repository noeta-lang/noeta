# noeta-fmt

`noeta fmt` — the canonical source formatter for Noeta.

- **Takes in:** `.noe` source text (plus an optional `FmtConfig` from `noeta.toml`'s `[fmt]` table).
- **Emits:** [`format_source`] — reformatted source, a pure function of the parsed program (plus comments and a small set of preserved author-choice trivia), not of the incoming whitespace.

A canonical reformatter in the gofmt/rustfmt/Prettier model: the same program always prints identically regardless of how it was originally laid out. This crate is a reusable library; the `noeta fmt` CLI verb and the LSP's `textDocument/formatting` provider are thin front ends over `format_source`. Pipeline: lex with trivia → parse to an AST → reattach comments → lower to a Wadler-style `Doc` IR → render → **safety gate** (re-parse the output and assert it's AST-equal-modulo-spans to the input, else abort untouched). Canonical style (v1): 4-space indent, K&R braces, one statement per line, configurable trailing-`;`/header-paren/match-arm-arrow policy, and off-by-default width-driven wrapping so an already-sane file is untouched. `// fmt: off`/`// fmt: on` fence a verbatim region. Correctness is property-tested over the `.noe` corpus for safety (never changes meaning), idempotency, and comment completeness.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
