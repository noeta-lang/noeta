/**
 * Tree-sitter grammar for Noeta (.noe).
 *
 * A pragmatic grammar aimed first at highlighting (Neovim / Zed / Helix), built from the
 * actual lexer/parser surface. Statements are newline-or-`;` terminated in Noeta; without an
 * external scanner we treat `;` as an optional separator and let the GLR parser find statement
 * boundaries structurally.
 */

const PREC = {
  pipe: 1,        // |>
  coalesce: 2,    // ?? ??=
  or: 3,          // ||
  and: 4,         // &&
  compare: 5,     // == != < <= > >= === !==
  bit_or: 6,      // |
  bit_xor: 7,     // ^
  bit_and: 8,     // &
  shift: 9,       // << >>
  range: 10,      // .. ..=
  add: 11,        // + - ~
  mul: 12,        // * / %
  unary: 13,      // - !
  as_is: 14,      // as is
  call: 15,       // f() x.m() x[i] x.f
};

// Verbatim text-tier names. `@<name> { … }` for a name in this set has its body captured as raw
// `text_body` prose (never lexed as code). The std default is `doc`; a per-project generated
// `project-tiers.json` (emitted by `noeta grammar tree-sitter`) widens it with the project's
// `@tier(<name>, text: "…")` declarations, so third-party text tiers parse verbatim too. Absent
// that file — a project with no declared tiers — the static `doc`-only set is the fallback. The
// file is validated here so a malformed overlay can never inject an unexpected token.
const TEXT_TIER_NAMES = (() => {
  const isIdent = (s) => typeof s === 'string' && /^[A-Za-z_][A-Za-z0-9_]*$/.test(s);
  try {
    const declared = require('./project-tiers.json').textTiers;
    if (Array.isArray(declared)) {
      const names = [...new Set(declared.filter(isIdent))];
      if (names.length > 0) return names;
    }
  } catch (_) {
    // No per-project overlay — fall through to the static default.
  }
  return ['doc'];
})();

// Expression-tier names. `@<name> { … }` for a name here has a **text-with-holes** body (expr-tiers
// arc): verbatim text interrupted by `${ … }` code holes (like a double-quoted string), rather than
// the fully-verbatim body of a text tier — a `${` in a text tier (`@sql`) is literal, in an expr
// tier it opens a hole. Sourced from the same generated `project-tiers.json` (`exprTiers`); empty
// without the overlay, so an undeclared `@name { … }` still parses as a code decorator.
const EXPR_TIER_NAMES = (() => {
  const isIdent = (s) => typeof s === 'string' && /^[A-Za-z_][A-Za-z0-9_]*$/.test(s);
  try {
    const declared = require('./project-tiers.json').exprTiers;
    if (Array.isArray(declared)) {
      return [...new Set(declared.filter(isIdent))];
    }
  } catch (_) {
    // No per-project overlay — no expr tiers are known statically.
  }
  return [];
})();

// The tier-name rule for `text_tier_block`: an `alias(<literal>, identifier)` per verbatim tier, so
// each name is a keyword that selects the text-tier form. A single name needs no `choice` wrapper.
function textTierName($) {
  const alts = TEXT_TIER_NAMES.map((n) => alias(n, $.identifier));
  return alts.length === 1 ? alts[0] : choice(...alts);
}

// The tier-name rule for `expr_tier_block` — one aliased literal per declared expression tier. With
// none declared (no overlay), a NUL sentinel that never occurs in source, so `expr_tier_block` stays
// a valid but unreachable rule instead of an empty `choice()` (which tree-sitter rejects) — every
// `@name { … }` then falls through to the code decorator, as before.
function exprTierName($) {
  const alts = EXPR_TIER_NAMES.map((n) => alias(n, $.identifier));
  if (alts.length === 0) return token('\0');
  return alts.length === 1 ? alts[0] : choice(...alts);
}

module.exports = grammar({
  name: 'noeta',

  word: $ => $.identifier,

  externals: $ => [$.block_comment, $._newline, $.text_body, $.text_segment],

  extras: $ => [/\s/, $.line_comment, $.block_comment],

  conflicts: $ => [
    // `X { ... }` is a struct literal in a value position but `X` + a body block after a
    // control-flow head. Both parses are kept; the mandatory flow-body makes the struct
    // interpretation error out in a head, and the positive dynamic precedence on struct_literal
    // makes it win in value positions (where both parses would otherwise be valid).
    [$.struct_literal, $._expression],
    // `(a, b)` leading a statement is a tuple expression or a destructuring-assignment target,
    // resolved only when the `=` (or its absence) appears.
    [$._expression, $._pattern],
    // `a.b` is a member expression in a value position but a module-qualified head when a struct
    // body (or `.Variant` in a pattern) follows — GLR keeps both until the `{`/args decide, and
    // struct_literal's dynamic precedence picks the qualified parse where both survive.
    [$._expression, $.qualified_identifier],
    // The chain length of a qualified head is decided by what follows (pattern variant vs head).
    [$.qualified_identifier],
    // `use foo.{bar}` (grouped import) vs `use foo` then a `.{bar}` dot-brace statement — the same
    // `.{` token now opens both. GLR keeps both until the terminator decides: no scanner `_newline`
    // between the path and `.{` (same line) forces the grouped-import parse; a newline splits them.
    [$._use_path],
  ],

  rules: {
    source_file: $ => seq(optional($.shebang), repeat($._item)),

    // A leading `#!…` interpreter line (PHP-style executable scripts). Only valid as the file's
    // first token — the lexer never produces it elsewhere (a mid-file `#!` is a `#` then an error,
    // matching the compiler). Distinct from an `#[attribute]` (which is `#` then `[`).
    shebang: _ => token(seq('#!', /[^\n]*/)),

    // A statement/member terminator: an explicit `;` or a scanner-emitted newline.
    _terminator: $ => choice(';', $._newline),

    _item: $ => choice(
      $._declaration,
      $._statement,
    ),

    // ---------------------------------------------------------------- comments
    // `block_comment` is produced by the external scanner (src/scanner.c) so it can nest.
    line_comment: _ => token(seq('//', /[^\n]*/)),

    // ------------------------------------------------------------ declarations
    _declaration: $ => choice(
      $.use_declaration,
      $.function_declaration,
      $.struct_declaration,
      $.class_declaration,
      $.enum_declaration,
      $.trait_declaration,
      $.impl_block,
      $.namespace_declaration,
      $.text_tier_block,
      $.decorator,
      $.attributed_declaration,
    ),

    use_declaration: $ => seq(
      'use',
      $._use_path,
      optional($._terminator),
    ),
    _use_path: $ => seq(
      $.identifier,
      repeat(seq('.', $.identifier)),
      // A grouped import `use a.b.{c, d}`. The `.{` is one fused token (the compiler lexer fuses it
      // too), so `.` as a path separator and `.{` as a group opener never compete at `a • .`.
      optional(seq('.{', commaSep1($.identifier), '}')),
    ),

    attribute: $ => seq(
      '#', '[',
      field('name', $.identifier),
      optional(seq('(', optional(commaSep($._decorator_arg)), ')')),
      ']',
    ),

    attributed_declaration: $ => seq(
      repeat1($.attribute),
      choice(
        $.function_declaration,
        $.struct_declaration,
        $.class_declaration,
        $.enum_declaration,
        $.field_declaration,
        $.enum_variant,
        $.decorator,
      ),
    ),

    // std's `doc` tier is a **text** tier (text-tiers arc — it mirrors the compiler's default
    // TextTiers set): its `@doc { … }` body is verbatim prose, captured raw by the external
    // scanner with the same balanced-brace + `\{`/`\}`/`\\` escape count as the compiler's lexer,
    // so editor and compiler always agree on where the body ends. The `queries/injections.scm`
    // rule overlays markdown on the body. Third-party declared text tiers (`@tier(x, text: "…")`)
    // are not modeled by the *static* grammar — a static grammar cannot read the declaration set, so
    // their bodies parse as code (or error-recover). A per-project overlay (`project-tiers.json`,
    // emitted by `noeta grammar tree-sitter`) widens `TEXT_TIER_NAMES` above so those names parse
    // verbatim too; `queries/injections.scm` then maps each to its language.
    text_tier_block: $ => seq(
      '@',
      field('name', textTierName($)),
      field('body', $.text_block),
    ),
    text_block: $ => seq('{', optional($.text_body), '}'),

    // An **expression tier** `@<name> { text ${hole} more }` (expr-tiers arc): a value-producing
    // block whose body is verbatim text interrupted by `${ … }` code holes. The `text_segment`
    // external captures each run of prose (stopping at `${` and at the block's own `}`), and the
    // shared `interpolation` rule parses each hole as a real expression — so a hole is highlighted
    // and navigable exactly like a `${…}` hole in a double-quoted string. The name set comes from
    // the generated overlay (`exprTiers`); with none declared this rule matches nothing and every
    // `@name { … }` stays a code decorator.
    expr_tier_block: $ => seq(
      '@',
      field('name', exprTierName($)),
      '{',
      repeat(choice($.text_segment, $.interpolation)),
      '}',
    ),

    // Covers both the fixed decorator directives (@derive/@role/@semantic/@attribute/@packed)
    // and the open tier set (@test/@bench/@debug/...). The distinction is name-based and
    // semantic, not syntactic, so one rule serves both — including the `@test fn` / `@fuzz {...}`
    // annotation and block forms. (`@doc` is carved out above as the text-tier form.)
    decorator: $ => prec.right(seq(
      '@',
      field('name', $.identifier),
      optional(seq('(', optional(commaSep($._decorator_arg)), ')')),
      optional(choice($._declaration, $.block)),
    )),
    _decorator_arg: $ => choice(
      seq(field('key', $.identifier), ':', $._expression),
      // A type argument, e.g. `@derive(Serialize<Json>)` — matched before the plain expression so
      // the `<…>` is read as generic arguments, not a comparison.
      $.generic_type,
      $._expression,
    ),

    function_declaration: $ => seq(
      optional('pub'),
      optional('async'),
      'fn',
      $._function_rest,
    ),
    _function_rest: $ => seq(
      field('name', $.identifier),
      optional($.type_parameters),
      field('parameters', $.parameters),
      // The sealed-fn capture clause: `fn f(params) use (a, b): Ret { … }` — the explicit
      // import of surrounding value bindings into a named function's body.
      optional(field('captures', $.capture_clause)),
      optional(seq(':', field('return_type', $._type))),
      field('body', $.block),
    ),
    capture_clause: $ => seq('use', '(', commaSep1($.identifier), ')'),

    parameters: $ => seq('(', optional(commaSep($.parameter)), ')'),
    parameter: $ => seq(
      field('name', $.identifier),
      optional(seq(':', field('type', $._type))),
      optional(seq('=', field('default', $._expression))),
    ),

    type_parameters: $ => seq('<', commaSep1($.type_parameter), '>'),
    type_parameter: $ => seq(
      field('name', $.identifier),
      optional(seq(':', $._type)),
    ),

    struct_declaration: $ => seq(optional('pub'), 'struct', $._type_body_decl),
    class_declaration: $ => seq(optional('pub'), 'class', $._type_body_decl),
    enum_declaration: $ => seq(optional('pub'), 'enum', $._type_body_decl),

    // A user-defined trait (L1): `pub? trait Name<T> { method-sigs }`. A trait method is a
    // signature whose body is OPTIONAL — bodiless is a required method, a `{ … }` body a default —
    // so it cannot reuse `function_declaration` (which requires a body).
    trait_declaration: $ => seq(
      optional('pub'),
      'trait',
      field('name', $.identifier),
      optional($.type_parameters),
      field('body', $.trait_body),
    ),
    trait_body: $ => seq('{', repeat(choice($.associated_type, $.trait_method)), '}'),
    // `type Name` / `type Name = T` — an associated type. DECLARED in a trait body (bodiless is a
    // required binding every impl must supply; `= T` gives a default an impl may omit) and BOUND in
    // an impl body (`type Item = int`). One rule for both: the two spellings differ only in whether
    // the `= T` is there, and the `type`-led form is tried before a method in each body for the same
    // reason the parser tries it first — a leading `type` opens an associated type, not a malformed
    // method. Without this rule `type` was not a token of the grammar at all, so an `impl` carrying
    // an associated binding failed to parse and every tree-sitter editor lost the whole block.
    associated_type: $ => seq(
      'type',
      field('name', $.identifier),
      optional(seq('=', field('value', $._type))),
      optional($._terminator),
    ),
    trait_method: $ => seq(
      optional('async'),
      'fn',
      field('name', $.identifier),
      field('parameters', $.parameters),
      optional(seq(':', field('return_type', $._type))),
      optional(field('body', $.block)),
      optional($._terminator),
    ),
    _type_body_decl: $ => seq(
      field('name', $.identifier),
      optional($.type_parameters),
      optional(seq(':', field('backing', $._type))),
      field('body', $.type_body),
    ),

    type_body: $ => seq(
      '{',
      repeat($._type_member),
      '}',
    ),
    _type_member: $ => choice(
      $.field_declaration,
      $.enum_variant,
      $.function_declaration,
      $.destructor,
      $.impl_block,
      $.decorator,
      $.attributed_declaration,
    ),

    destructor: $ => seq('destruct', field('body', $.block)),

    field_declaration: $ => prec.right(seq(
      optional('pub'),
      optional('mut'),
      field('name', $.identifier),
      ':',
      field('type', $._type),
      optional(seq('=', field('default', $._expression))),
      optional($._terminator),
    )),

    enum_variant: $ => prec.right(1, seq(
      field('name', $.identifier),
      optional(seq('(', commaSep1($._variant_field), ')')),
      optional(seq('=', field('value', $._expression))),
      optional($._terminator),
    )),
    // A variant payload field: a bare type, or a named field `name: type`.
    _variant_field: $ => choice(
      seq(field('name', $.identifier), ':', field('type', $._type)),
      $._type,
    ),

    impl_block: $ => seq(
      'impl',
      field('trait', $.trait_reference),
      optional(seq('for', field('type', $._type))),
      '{',
      repeat(choice($.associated_type, $.function_declaration)),
      '}',
    ),
    // The reference right after `impl`: the trait for a trait impl, or the type for an inherent impl.
    // It may be bare (`impl Add`), module-qualified (`impl vec.Kernels for T` — kernel-methods arc),
    // generic (`impl From<Low>`, `impl Keyed<string> for Tag`), or both (`impl a.B<C>`). Previously a
    // bare identifier only, which ERRORed on every qualified or generic impl (~54 in the corpus).
    trait_reference: $ => seq(
      field('name', choice($.qualified_identifier, $.identifier)),
      optional(seq('<', commaSep1($._type), '>')),
    ),

    namespace_declaration: $ => seq(
      'namespace',
      field('name', $._dotted_name),
      choice(';', seq('{', repeat($._item), '}')),
    ),
    _dotted_name: $ => seq($.identifier, repeat(seq('.', $.identifier))),

    // -------------------------------------------------------------- statements
    _statement: $ => choice(
      $.let_statement,
      $.typed_binding,
      $.echo_statement,
      $.return_statement,
      $.yield_statement,
      $.break_statement,
      $.continue_statement,
      $.if_statement,
      $.for_statement,
      $.while_statement,
      $.assignment_statement,
      $.expression_statement,
    ),

    let_statement: $ => seq(
      'mut',
      field('name', choice($.identifier, $.tuple_pattern)),
      optional(seq(':', field('type', $._type))),
      '=',
      field('value', $._expression),
      optional($._terminator),
    ),

    // A typed immutable binding: `x: T = value` (no `mut`).
    typed_binding: $ => seq(
      field('name', $.identifier),
      ':', field('type', $._type),
      '=', field('value', $._expression),
      optional($._terminator),
    ),

    echo_statement: $ => seq('echo', $._expression, optional($._terminator)),
    return_statement: $ => prec.right(seq('return', optional($._expression), optional($._terminator))),
    yield_statement: $ => prec.right(seq('yield', optional($._expression), optional($._terminator))),
    break_statement: $ => seq('break', optional($._terminator)),
    continue_statement: $ => seq('continue', optional($._terminator)),

    assignment_statement: $ => seq(
      field('left', $._assignable),
      field('operator', choice('=', '+=', '-=', '*=', '/=', '%=', '~=', '??=')),
      field('right', $._expression),
      optional($._terminator),
    ),
    _assignable: $ => choice(
      $.identifier,
      $.field_expression,
      $.index_expression,
      $.tuple_pattern,
    ),

    expression_statement: $ => prec(-1, seq($._expression, optional($._terminator))),

    if_statement: $ => prec.right(seq(
      'if',
      field('condition', $._expression),
      field('consequence', $.block),
      optional(seq('else', field('alternative', choice($.block, $.if_statement)))),
    )),

    for_statement: $ => seq(
      'for',
      field('pattern', choice($.identifier, $.tuple_pattern)),
      'in',
      field('iterable', $._expression),
      field('body', $.block),
    ),

    while_statement: $ => seq(
      'while',
      field('condition', $._expression),
      field('body', $.block),
    ),

    block: $ => seq('{', repeat($._item), '}'),

    // ------------------------------------------------------------- expressions
    _expression: $ => choice(
      $._literal,
      $.identifier,
      $.self,
      $.turbofish_call,
      $.instantiated_call_expression,
      $.field_expression,
      $.index_expression,
      $.call_expression,
      $.method_call_expression,
      $.narrow_expression,
      $.unary_expression,
      $.binary_expression,
      $.as_expression,
      $.is_expression,
      $.closure_expression,
      $.match_expression,
      $.if_expression,
      $.await_expression,
      $.try_expression,
      $.spawn_expression,
      $.isolate_expression,
      $.concurrent_expression,
      $.struct_literal,
      $.dot_brace_literal,
      $.expr_tier_block,
      $.list_literal,
      $.map_literal,
      $.set_literal,
      $.tuple_expression,
      $.spread_expression,
      $._parenthesized_expression,
    ),

    _parenthesized_expression: $ => seq('(', $._expression, ')'),

    self: _ => 'self',

    field_expression: $ => prec(PREC.call, seq(
      field('object', $._expression),
      '.',
      field('field', choice($.identifier, $.integer_literal)),
    )),

    index_expression: $ => prec(PREC.call, seq(
      field('object', $._expression),
      '[', field('index', $._expression), ']',
    )),

    call_expression: $ => prec(PREC.call, seq(
      field('function', $._expression),
      field('arguments', $.arguments),
    )),

    // Turbofish call: reflection intrinsics and typed generics — `attributes_of::<Route>()`,
    // `from_bytes::<T>(bytes)`, `type_of::<T>()`.
    turbofish_call: $ => prec(PREC.call, seq(
      field('function', $.identifier),
      '::', '<', commaSep1($._type), '>',
      field('arguments', $.arguments),
    )),

    // Call-site class instantiation: `Repo::<Todo>.new("todos")` — the turbofish applies to the
    // generic TYPE, and the associated call that consumes it is part of the rule (the compiler
    // requires the trailing `.member`, and including it keeps this disjoint from `turbofish_call`,
    // which takes `(` where this takes `.`).
    //
    // The member may carry a turbofish of its OWN — `Repo::<Todo>.blank::<int>()` — naming the
    // METHOD's type parameters where the leading one names the CLASS's. Optional here for the same
    // reason it is optional in `method_call_expression`, and spelled the same way, because it is
    // the same construct: only its receiver differs.
    instantiated_call_expression: $ => prec(PREC.call + 2, seq(
      field('type', $.identifier),
      '::', '<', commaSep1($._type), '>',
      '.',
      field('method', $.identifier),
      optional(seq('::', '<', commaSep1($._type), '>')),
      field('arguments', $.arguments),
    )),

    method_call_expression: $ => prec(PREC.call + 1, seq(
      field('object', $._expression),
      '.',
      field('method', $.identifier),
      optional(seq('::', '<', commaSep1($._type), '>')),
      field('arguments', $.arguments),
    )),

    narrow_expression: $ => prec(PREC.call + 1, seq(
      field('object', $._expression),
      '.', 'as',
      '<', field('type', $._type), '>',
      '(', ')',
    )),

    // A trailing comma is legal (the compiler's call_args list is allow_trailing), so a multi-line
    // call can end `3,\n)` — the shape the termination conformance corpus exercises.
    arguments: $ => seq('(', optional(commaSep($._argument)), optional(','), ')'),
    _argument: $ => choice(
      seq(field('name', $.identifier), ':', $._expression),
      $._expression,
    ),

    unary_expression: $ => prec(PREC.unary, seq(
      field('operator', choice('-', '!')),
      field('operand', $._expression),
    )),

    // Async / structured-concurrency forms.
    await_expression: $ => prec(PREC.call + 2, seq(field('value', $._expression), '.', 'await')),
    try_expression: $ => prec(PREC.call + 2, seq(field('value', $._expression), '?')),
    spawn_expression: $ => prec.right(PREC.unary, seq('spawn', $._expression)),
    isolate_expression: $ => prec.right(PREC.unary, seq('isolate', $._expression)),
    concurrent_expression: $ => seq('concurrent', field('body', $.block)),

    as_expression: $ => prec.left(PREC.as_is, seq(
      field('value', $._expression), 'as', field('type', $._type),
    )),
    is_expression: $ => prec.left(PREC.as_is, seq(
      field('value', $._expression), 'is', field('type', $._type),
    )),

    binary_expression: $ => binaryTable($._expression),

    closure_expression: $ => seq(
      'fn',
      field('parameters', $.parameters),
      optional(seq(':', field('return_type', $._type))),
      choice(
        seq('=>', field('body', $._expression)),
        field('body', $.block),
      ),
    ),

    match_expression: $ => seq(
      'match',
      field('value', $._expression),
      '{',
      optional(commaSep($.match_arm)),
      optional(','),
      '}',
    ),

    match_arm: $ => seq(
      field('pattern', $._pattern),
      '=>',
      field('value', $._expression),
    ),

    if_expression: $ => prec.right(seq(
      'if', field('condition', $._expression),
      'then', field('consequence', $._expression),
      'else', field('alternative', $._expression),
    )),

    struct_literal: $ => prec.dynamic(1, seq(
      // The head may be module-qualified (`vec.Vec2 { … }`, `geometry.vec.Vec2 { … }`) — the
      // qualified-references feature; a dotted path directly before `{` is always a type head
      // (a field path can never take a struct body).
      field('type', choice($.qualified_identifier, $.identifier)),
      '{',
      optional(commaSep($._struct_field_init)),
      optional(','),
      '}',
    )),
    // A dotted module-qualified name in a head position (`vec.Vec2`, `geometry.vec.Shape`). The
    // chain length is ambiguous mid-parse (`geometry.vec.Shape.Circle` in a pattern is a 3-segment
    // head + variant) — the self-conflict below keeps every length alive until the follower
    // decides.
    qualified_identifier: $ => seq($.identifier, repeat1(seq('.', $.identifier))),
    _struct_field_init: $ => choice(
      $.spread_expression,
      seq(field('field', $.identifier), ':', $._expression),
      field('field', $.identifier),
    ),

    // `.{ … }` — a target-typed struct literal (dot-brace-literals): the head type is inferred from
    // context (a call arg `f(.{ … })`, a `: T = .{ … }` binding, a `return .{ … }`), so only the
    // field-init body is written. Same body as `struct_literal`, headless. The `.` and `{` are
    // separate tokens (as in the `use a.{b, c}` grouped import), disambiguated by the leading `.`
    // sitting in expression-primary position with no receiver before it.
    dot_brace_literal: $ => seq(
      '.{',
      optional(commaSep($._struct_field_init)),
      optional(','),
      '}',
    ),

    list_literal: $ => seq('[', optional(commaSep($._expression)), optional(','), ']'),

    map_literal: $ => prec.dynamic(1, seq(
      '{',
      optional(commaSep1($._map_entry)),
      optional(','),
      '}',
    )),
    _map_entry: $ => choice(
      seq(field('key', $._expression), ':', field('value', $._expression)),
      field('key', $.identifier),
    ),

    set_literal: $ => seq('#', '{', optional(commaSep($._expression)), optional(','), '}'),

    tuple_expression: $ => seq('(', $._expression, ',', optional(commaSep($._expression)), optional(','), ')'),

    spread_expression: $ => prec.right(seq('...', $._expression)),

    // ---------------------------------------------------------------- patterns
    _pattern: $ => choice(
      $.identifier,
      $.self,
      $._literal,
      $.enum_pattern,
      $.tuple_pattern,
      $.type_pattern,
      $.wildcard_pattern,
    ),
    // A type-test match arm: `is int => …`, `is List<int> => …` (union / dyn narrowing).
    type_pattern: $ => seq('is', field('type', $._type)),
    enum_pattern: $ => prec(1, choice(
      // Qualified: `Type.Variant`, `Type.Variant(binds)` (variant is PascalCase or lowercase).
      // The type head may itself be module-qualified: `vec.Shape.Circle(r)`.
      seq(
        field('type', choice($.qualified_identifier, $.identifier)),
        '.',
        field('variant', choice($.identifier, $.identifier)),
        optional($._variant_args),
      ),
      // Unqualified variant carrying bindings: `some(n)`, `Circle(r)` — the args disambiguate it
      // from a bare identifier binding.
      seq(field('variant', choice($.identifier, $.identifier)), $._variant_args),
    )),
    _variant_args: $ => seq('(', commaSep($._pattern), ')'),
    tuple_pattern: $ => seq('(', commaSep1($._pattern), ')'),
    wildcard_pattern: _ => '_',

    // ------------------------------------------------------------------- types
    _type: $ => choice(
      $.primitive_type,
      $.trait_object_type,
      $.generic_type,
      $.optional_type,
      $.union_type,
      $.tuple_type,
      $.function_type,
      $.identifier,
    ),
    // `dyn Trait` (L1 UT4): the top type `dyn` followed by a trait name. Higher precedence than the
    // bare `dyn` primitive so `dyn Foo` binds as one trait-object type, not primitive + stray name.
    trait_object_type: $ => prec(1, seq('dyn', field('trait', $.identifier))),
    function_type: $ => prec.right(seq(
      '(', optional(commaSep($._type)), ')',
      '->', field('return', $._type),
    )),
    // The built-in type names. GENERATED from `noeta_ast::BuiltinTy` — do not hand-edit between
    // the markers; the generator is `crates/noeta-ide/tests/editor_vocabulary.rs`, which also
    // checks this region on every `cargo test -p noeta-ide`. The containers (`List`, `Map`, …) and
    // the kind-types (`Enum`, `Struct`, `Class`) are deliberately NOT here: they are ordinary
    // identifiers to the grammar, reached through `generic_type`.
    // --- BEGIN GENERATED VOCABULARY ---
    // `never` is a type NAME, not a keyword: an ordinary identifier spelled `never`
    // elsewhere still parses as one, since `$.identifier` is also a `$._type` and the
    // grammar declares `word: $.identifier`, so these literals are only recognised
    // where a type is expected. The same is true of `unit`, `number` and `Any`.
    primitive_type: _ => choice(
      'int', 'float', 'f32', 'f64', 'bool', 'string', 'bytes', 'void',
      'unit', 'dyn', 'Any', 'never', 'number', 'i8', 'i16', 'i32',
      'i64', 'u8', 'u16', 'u32', 'u64',
    ),
    // --- END GENERATED VOCABULARY ---

    generic_type: $ => prec(3, seq(
      field('name', choice($.identifier, $.primitive_type)),
      '<', commaSep1($._type), '>',
    )),
    optional_type: $ => prec(2, seq('?', $._type)),
    union_type: $ => prec.left(1, seq($._type, '|', $._type)),
    tuple_type: $ => seq('(', commaSep1($._type), ')'),

    // ---------------------------------------------------------------- literals
    _literal: $ => choice(
      $.integer_literal,
      $.float_literal,
      $.boolean_literal,
      $.string_literal,
    ),

    integer_literal: _ => token(choice(
      /[0-9][0-9_]*((i|u)(8|16|32|64))?/,
      /0[xX][0-9A-Fa-f][0-9A-Fa-f_]*((i|u)(8|16|32|64))?/,
      /0[oO][0-7][0-7_]*((i|u)(8|16|32|64))?/,
      /0[bB][01][01_]*((i|u)(8|16|32|64))?/,
    )),
    float_literal: _ => token(choice(
      /[0-9][0-9_]*\.[0-9][0-9_]*([eE][+-]?[0-9][0-9_]*)?(f32)?/,
      /[0-9][0-9_]*[eE][+-]?[0-9][0-9_]*(f32)?/,
      /[0-9][0-9_]*f32/,
    )),
    boolean_literal: _ => choice('true', 'false'),

    // A double-quoted or backtick string INTERPOLATES (`${…}` holes) and honors the full escape set.
    // A single-quoted string is RAW (compiler lexer `RawStr`): no interpolation — `${…}`, `{`, `$`
    // are literal — and its only escapes are `\'` and `\\` (every other backslash is literal too).
    string_literal: $ => choice(
      seq('"', repeat(choice($._string_content_dq, $.escape_sequence, $.interpolation)), '"'),
      seq("'", repeat(choice($._raw_string_content, $.raw_escape_sequence)), "'"),
      seq('`', repeat(choice($._string_content_bt, $.escape_sequence, $.interpolation)), '`'),
    ),
    _string_content_dq: _ => token.immediate(prec(1, /[^"\\$]+|\$[^{]/)),
    _string_content_bt: _ => token.immediate(prec(1, /[^`\\$]+|\$[^{]/)),
    // A run of raw single-quoted content: non-quote/non-backslash chars, or a backslash that does
    // NOT start one of the two raw escapes (so `\t`, `\n`, `\$`, `\u{…}` stay literal — a raw string
    // never expands them).
    _raw_string_content: _ => token.immediate(prec(1, /([^'\\]|\\[^'\\])+/)),
    raw_escape_sequence: _ => token.immediate(/\\['\\]/),
    escape_sequence: _ => token.immediate(/\\(u\{[0-9A-Fa-f]+\}|x[0-9A-Fa-f]{2}|\$\{|.)/),
    interpolation: $ => seq('${', $._expression, '}'),

    // ------------------------------------------------------------- identifiers
    identifier: _ => /[A-Za-z_][A-Za-z0-9_]*/,
  },
});

// The binary-operator precedence table, parameterized by the operand rule so the full expression
// and the restricted control-flow-head expression share one definition.
function binaryTable(operand) {
  const table = [
    [PREC.pipe, '|>'],
    [PREC.coalesce, '??'],
    [PREC.or, '||'],
    [PREC.and, '&&'],
    [PREC.compare, choice('==', '!=', '<', '<=', '>', '>=', '===', '!==')],
    [PREC.bit_or, '|'],
    [PREC.bit_xor, '^'],
    [PREC.bit_and, '&'],
    [PREC.shift, choice('<<', '>>')],
    [PREC.range, choice('..', '..=')],
    [PREC.add, choice('+', '-', '~')],
    [PREC.mul, choice('*', '/', '%')],
  ];
  return choice(...table.map(([p, op]) => prec.left(p, seq(
    field('left', operand),
    field('operator', op),
    field('right', operand),
  ))));
}

function commaSep(rule) {
  return optional(commaSep1(rule));
}
function commaSep1(rule) {
  return seq(rule, repeat(seq(',', rule)));
}
