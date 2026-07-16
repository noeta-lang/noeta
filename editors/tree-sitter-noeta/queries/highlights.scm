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
(impl_block trait: (identifier) @type)
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

; --------------------------------------------------------------------- keywords
[
  "fn" "mut" "struct" "class" "enum" "impl" "trait" "destruct"
  "namespace" "use" "pub"
] @keyword

[
  "if" "then" "else" "for" "while" "break" "continue" "in" "match" "return" "yield"
] @keyword.control

[
  "async" "await" "spawn" "isolate" "concurrent"
] @keyword.coroutine

"echo" @keyword

["as" "is"] @keyword.operator

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
