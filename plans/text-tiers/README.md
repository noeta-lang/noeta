# Text tiers — generic verbatim bodies + editor highlighting

**Goal.** "What's inside a tier block" becomes a *declared property of the tier*, not a
hardcoded name check. `@doc` stops being the one special text tier: any tier — std or
third-party — can declare that its body is verbatim text with a language tag
(`markdown`, `xml`, …), and every consumer (lexer, checker, `noeta doc`-style runners,
fmt, LSP, TextMate, tree-sitter) keys off the declaration. Escaping (`\{`/`\}`/`\\`)
makes brace-counting exact and identical across all three counters.

Builds directly on tier-providers T2–T4 (`@tier` declarations, open name set, runner
dispatch), merged to main in `4bf10d87`.

## Design

### Declaration surface

A tier declares a text body with a `text:` key on the `@tier` directive:

```noeta
@tier(spec, text: "xml")
fn run_specs(roots: List<TierRoot>): void { … }
```

- `text:` takes a **string literal language ID** (`"markdown"`, `"xml"`, `"sql"`, …).
  The ID is decoupled from the tier name — `doc` maps to `markdown`.
- Omitted `text:` = a code tier (today's default; `@test`/`@bench`/`@debug` unchanged).
- `config:` and `text:` are mutually exclusive: a text body carries no fns to stamp
  knobs onto (E0051 extension).
- std's `doc` becomes the dogfood declaration of this feature; the lexer's default set
  is `{doc}` so bare snippets/tests keep working without a manifest.

### Lexing — `TextTierSet`, two-pass self-use, escapes

- `lex(source)` stays and defaults to the std set (`{"doc"}`). New
  `lex_in(source, &TextTierSet)` (and trivia variant) is what pipeline paths upgrade
  to. ~80 test call sites remain untouched.
- `opens_doc_block` generalizes to `opens_text_block`: `@` + ident ∈ set + `{`.
  The captured token stays `DocText` (renamed `TextBody`).
- **Same-file self-use, decl after use included**: after a normal pass, scan the token
  stream for `@ tier ( <name> … text … )` sequences; if that discovers text-tier names
  not in the supplied set *and* the stream uses `@<name> {`, re-lex once with the
  augmented set. Only files declaring text tiers pay the second pass.
- **Escapes** (user directive): inside a text body, exactly three sequences are
  consumed — `\{` (literal `{`, not counted), `\}` (literal `}`, not counted),
  `\\` (literal `\`). Every other backslash passes through verbatim (markdown needs
  its own escapes untouched). `matching_brace` gains the skip; the **parser** unescapes
  when it slices the body into `TierBlock.doc_text` — the one materialization point, so
  extraction, hover, `#[Doc]` stamping and runners all see clean text. The three
  grammars (lexer, tree-sitter scanner, TextMate) implement the identical rule.

### Checker & runners

- `DeclaredTier` gains `text: Option<String>` (the language ID); `TierRegistry`
  exposes `text_tiers()` for the pipeline and E0036/E0051 validation.
- The `tier == "doc"` special case in tiers.rs generalizes: `resolve_docs` →
  `resolve_texts(program, registry)`; `DocBlock` → `TextBlock { tier, lang, … }`
  (`DocBlock`-compat alias for the doc consumers). Adjacency targeting is shared —
  a `@spec` block above a fn documents/specifies that fn the same way `@doc` does.
- Activation: text blocks of an *active* declared text tier surface as roots for the
  T4 runner dispatch (text content instead of fn handles); on a normal run they strip
  exactly like today's `@doc`.

### Pipeline plumbing

- Loader: dependency packages parse before consumers (verify; restructure if needed).
  Declared text tiers accumulate as packages parse and feed `lex_in` for subsequent
  files. Entry-package self-use is covered by the two-pass lexer even with no manifest.
- noeta-db (salsa): the text-tier set becomes an input the `Tokens` query depends on,
  so an edit to a `@tier(…, text:)` decl re-lexes dependents correctly (LSP path).
- fmt: text bodies are already preserved verbatim via `DocText`; extend to the set +
  keep escapes byte-exact (fmt must NOT unescape — it re-emits source).

### Editors

- **tree-sitter**: external scanner token for the raw body (same brace+escape count),
  `injections.scm`: `doc` → `markdown` alias + dynamic `injection.language` from the
  tier name for editors that support it. Corpus stays green.
- **TextMate**: one generic rule for `@<name> {` tier bodies scoped
  `meta.embedded.block.tier.$2.noeta` (capture substitution), body patterns =
  escape match (`\\[{}\\]`) first, then recursive brace-pair, so nesting and escapes
  track the lexer. std ships an injection grammar `doc` → `text.html.markdown`
  (`injectTo`); third-party tiers target their own scope the same way. Baseline: every
  tier body at minimum stops leaking string scopes into the rest of the file
  (today an apostrophe in `@doc` prose corrupts highlighting below it).

## Slices

- **S1** lexer: `TextTierSet` + `lex_in`, generalized capture, two-pass self-use,
  escapes in `matching_brace`, parser unescape. Conformance: escapes + custom-tier
  fixture (via default-set for doc; self-use test for custom).
- **S2** checker: `DeclaredTier.text`, E0051 exclusivity, `resolve_texts`,
  activation roots for text tiers.
- **S3** pipeline: loader accumulation, salsa input, fmt set-awareness, cross-package
  e2e (dep declares `@spec` XML tier; consumer's body captured verbatim; `noeta spec`
  dispatches).
- **S4** tree-sitter: scanner + injections + corpus.
- **S5** TextMate: generic scope + markdown injection + bleed-bug regression sample.
- **S6** conformance sweep + fmt idempotence over text bodies + /verify.

## Non-goals (this arc)

- Rendering markdown inside hover is already done (T5); no change.
- No auto-generation of VS Code injection grammars from manifests (third-party tiers
  ship their own `injectTo` grammar; revisit if it proves too high-friction).
