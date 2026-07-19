; Language injections for Noeta text-tier bodies (text-tiers arc).
;
; A text tier's `@<name> { … }` body is verbatim prose the compiler never lexes as code; the
; language ID comes from the tier. std's `doc` is markdown. A third-party declared text tier
; (`@tier(x, text: "sql")`) is not statically modeled by this grammar — run `noeta grammar
; tree-sitter --out <dir>` to regenerate this file (plus `project-tiers.json`) with one injection
; rule per project tier mapping its name to its declared language.
((text_tier_block
   name: (identifier) @_tier
   body: (text_block (text_body) @injection.content))
 (#eq? @_tier "doc")
 (#set! injection.language "markdown"))
