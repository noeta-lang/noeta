; Tree-sitter highlight query for Noeta.
; Type-ness is driven by grammatical position (Noeta identifiers are case-insensitive), with a
; PascalCase heuristic only for bare identifiers whose position is ambiguous.

; ---------------------------------------------------------------- comments
(line_comment) @comment
(block_comment) @comment
(shebang) @comment

; ------------------------------------------------------------------ literals
(integer_literal) @number
(float_literal) @number.float
(boolean_literal) @boolean
(string_literal) @string
(escape_sequence) @string.escape
(raw_escape_sequence) @string.escape
(self) @variable.builtin

; ---------------------------------------------------------- string interpolation
(interpolation
  "${" @punctuation.special
  "}" @punctuation.special)

; --------------------------------------------------------------------- types
(primitive_type) @type.builtin

; A type appears wherever the grammar expects a type — highlight the identifier there as a type.
(generic_type name: (identifier) @type)
(struct_declaration name: (identifier) @type)
(class_declaration name: (identifier) @type)
(enum_declaration name: (identifier) @type)
(trait_declaration name: (identifier) @type)
(trait_object_type trait: (identifier) @type)
(type_parameter name: (identifier) @type)
(impl_block trait: (trait_reference name: (identifier) @type))
(enum_variant name: (identifier) @constructor)
(type_pattern type: (identifier) @type)
(parameter type: (identifier) @type)
(field_declaration type: (identifier) @type)
(_ return_type: (identifier) @type)

; Bare `_type` identifiers (named type references not otherwise captured).
((identifier) @type
  (#match? @type "^[A-Z]"))

; ---------------------------------------------------------------- declarations
(function_declaration (identifier) @function)
(destructor "destruct" @keyword.function)

; ------------------------------------------------------------------ callables
(call_expression function: (identifier) @function.call)
(turbofish_call function: (identifier) @function.call)
(method_call_expression method: (identifier) @function.method.call)

; -------------------------------------------------------------------- members
(field_declaration name: (identifier) @property)
(field_expression field: (identifier) @property)
(parameter name: (identifier) @variable.parameter)

; --------------------------------------------------------------- decorators / attributes
(decorator "@" @attribute (identifier) @attribute)
(attribute (identifier) @attribute)
"#" @attribute

; An expression tier `@name { text ${hole} }`: the `@name` reads like a decorator, its prose runs
; are string-like, and each `${…}` hole is highlighted as code by the string-interpolation rules
; above (the `interpolation` node is shared). The name is `_`, not `(identifier)`: without a
; per-project `project-tiers.json` overlay the `name` field holds only the unreachable NUL
; sentinel token (an anonymous node), so `name: (identifier)` is a statically impossible pattern;
; with an overlay each tier name is aliased to `identifier` and `_` still matches it.
(expr_tier_block "@" @attribute name: _ @attribute)
(expr_tier_block (text_segment) @string)

; --------------------------------------------------------------------- keywords
; The keyword captures below are GENERATED from the lexer's own `#[token("…")]` declarations —
; every word the language reserves, filed under the colour family it was assigned once, for this
; grammar and the VS Code TextMate grammar together. Do not hand-edit between the markers; the
; generator is `crates/noeta-ide/tests/editor_vocabulary.rs`, which also checks this region on
; every `cargo test -p noeta-ide`.
;
; The boolean literals are deliberately absent: `(boolean_literal) @boolean` above captures the
; whole literal node, which is more precise than matching its two spellings.
; --- BEGIN GENERATED VOCABULARY ---
[
  "mut" "fn" "static" "enum" "struct" "type" "class" "destruct"
  "impl" "trait" "namespace" "use" "pub" "echo"
] @keyword

[
  "return" "yield" "if" "then" "else" "for" "while" "break"
  "continue" "in" "match"
] @keyword.control

[
  "async" "await" "concurrent" "spawn" "isolate"
] @keyword.coroutine

[
  "as" "is"
] @keyword.operator

; The reflection primitives are identifiers to the grammar (`turbofish_call` takes an
; `identifier`), so they are captured by spelling rather than as anonymous keyword nodes.
((identifier) @function.builtin
  (#any-of? @function.builtin
    "attributes_of" "type_of" "type_name" "fields_of"
    "traits_of" "from_bytes" "roles_of" "params_of"
    "returns_of" "invoke" "field_specs_of" "variants_of"
    "construct"
  ))
; --- END GENERATED VOCABULARY ---

; --------------------------------------------------------------------- operators
[
  "+" "-" "*" "/" "%" "~"
  "==" "!=" "<" "<=" ">" ">=" "===" "!=="
  "&&" "||" "!"
  "&" "|" "^" "<<" ">>"
  "=" "+=" "-=" "*=" "/=" "%=" "~=" "??=" "??" "?"
  "|>" "->" "=>" ".." "..=" "..."
] @operator

; ------------------------------------------------------------------ punctuation
["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["," ";" ":" "::" "."] @punctuation.delimiter

; ------------------------------------------------------------- text tiers
; The `@doc` head highlights like every other tier directive; the body is prose (injected as
; markdown via queries/injections.scm — editors without injection support leave it unstyled,
; which is already a win: prose can no longer bleed string scopes into the code below).
(text_tier_block "@" @attribute name: (identifier) @attribute)
