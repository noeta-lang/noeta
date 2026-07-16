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

module.exports = grammar({
  name: 'noeta',

  word: $ => $.identifier,

  externals: $ => [$.block_comment, $._newline, $.text_body],

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
      optional(seq('.', '{', commaSep1($.identifier), '}')),
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
    // are not modeled statically — a static grammar cannot read the declaration set; their bodies
    // parse as code (or error-recover) until a per-project generated grammar exists.
    text_tier_block: $ => seq(
      '@',
      field('name', alias('doc', $.identifier)),
      field('body', $.text_block),
    ),
    text_block: $ => seq('{', optional($.text_body), '}'),

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
      optional(seq(':', field('return_type', $._type))),
      field('body', $.block),
    ),

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
    trait_body: $ => seq('{', repeat($.trait_method), '}'),
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
      field('trait', $.identifier),
      optional(seq('for', field('type', $._type))),
      '{',
      repeat($.function_declaration),
      '}',
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

    arguments: $ => seq('(', optional(commaSep($._argument)), ')'),
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
      field('type', $.identifier),
      '{',
      optional(commaSep($._struct_field_init)),
      optional(','),
      '}',
    )),
    _struct_field_init: $ => choice(
      $.spread_expression,
      seq(field('field', $.identifier), ':', $._expression),
      field('field', $.identifier),
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
      seq(
        field('type', $.identifier),
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
    primitive_type: _ => choice(
      'int', 'float', 'f32', 'bool', 'string', 'bytes', 'void', 'unit', 'dyn',
      'i8', 'i16', 'i32', 'i64', 'u8', 'u16', 'u32', 'u64',
    ),
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

    string_literal: $ => choice(
      seq('"', repeat(choice($._string_content_dq, $.escape_sequence, $.interpolation)), '"'),
      seq("'", repeat(choice($._string_content_sq, $.escape_sequence, $.interpolation)), "'"),
      seq('`', repeat(choice($._string_content_bt, $.escape_sequence, $.interpolation)), '`'),
    ),
    _string_content_dq: _ => token.immediate(prec(1, /[^"\\$]+|\$[^{]/)),
    _string_content_sq: _ => token.immediate(prec(1, /[^'\\$]+|\$[^{]/)),
    _string_content_bt: _ => token.immediate(prec(1, /[^`\\$]+|\$[^{]/)),
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
