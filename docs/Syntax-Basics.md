# Syntax Basics

Comments, statement termination, bindings, primitive types, literals, strings, and operators. Everything here runs with `noeta run`.

## Comments

`//` starts a line comment, running to the end of the line, and `/* … */` is a block comment. Block comments **nest**, so a region that already contains a block comment can be commented out whole.

```noeta
// a line comment
echo 1        // trailing comment

/* a block comment
   spanning several lines */
x = /* inline */ 2 + /* they nest: /* inner */ still commented */ 3
echo x        // 5
```

A block comment may span a statement boundary. Like a line continuation, the enclosing statement continues across it.

## Statements and semicolons

A newline ends a statement, so **semicolons are optional**. A `;` is still valid, and it is required to put two statements on one line.

```noeta
echo "a"
echo "b"
echo "c"; echo "d"   // two statements, one line
```

A line **continues** onto the next, with no statement break inserted, when the break is clearly mid-expression. That covers a next line starting with an infix or postfix operator, `.`, `|>`, `??`, `..`, a comma, `=>`, `->`, a closing bracket, or a clause keyword (`else`, `then`, `in`, `as`, `is`), and any break sitting inside an open `(` or `[`, such as a multi-line call or list:

```noeta
total = 1 +          // trailing operator → continues
        2 + 3
scaled = [1, 2, 3]
    .map(fn(x) => x * 2)     // leading . → continues
    .sum()
echo total               // 6
```

A `{ … }` block opens a fresh statement context wherever it appears, closure bodies nested inside a call included, so newlines terminate statements there as usual:

```noeta check
ys = xs.map(fn(n) {
    d = n * 2        // newline terminates, no `;` needed
    return d + 1
})
```

Termination is also a **barrier**. After a line that can stand as a complete statement, a next line starting with `(` or `[` begins a new statement, never a call or index on the previous line's value. This holds at every nesting level, closure bodies included. To call across the break, keep the `(` on the same line.

Type, `struct`, and `class` bodies are newline-separated, so fields need no terminator.

### Parentheses around control-flow headers

The condition of `if` and `while`, and the iterable of `for`, may be parenthesized. Both styles mean the same thing, since a lone `(expr)` is just `expr`:

```noeta check
if x > 0 { echo "a" }
if (x > 0) { echo "a" }      // same thing — the parens are a readability choice

while (running) { tick() }
for x in (items) { echo x }
```

`noeta fmt` normalizes both choices, header parentheses and trailing semicolons, from the `[fmt]` table of `noeta.toml`. See [The `noeta` CLI](The-CLI#noeta-fmt).

## `echo`

`echo` prints one value, followed by a newline, using the value's `Display` form.

```noeta
echo "hello"    // hello
echo 1 + 2      // 3
echo [1, 2, 3]  // [1, 2, 3]
```

## Bindings and mutability

`name = expr` binds immutably and `mut name = expr` binds mutably. Reassigning an immutable binding is a **compile-time** error (E0006), caught statically even on a branch that never runs.

```noeta
x = 10               // immutable
mut total = 0        // mutable
total = total + 5    // ok
```

A `mut` binding has a **fixed type**, set when the binding is declared, from an annotation or inferred from the initializer. A reassignment must be assignable to that type, so the type you see is the type it keeps.

```noeta
mut x = 1
x = 2          // ok — still an int
echo x         // 2
```

Assigning an incompatible value is E0007:

```noeta error
mut x = 1
x = "hi"       // E0007: `string` is not assignable to `int`
```

For a binding that must hold more than one type, say so explicitly with a **union** or `dyn`:

```noeta
mut u: int | string = 1
u = "hi"       // ok — string is a member of the union
mut d: dyn = 1
d = "hi"       // ok — dyn opts out of a fixed type
echo u         // hi
```

A value outside the declared set is still rejected:

```noeta error
mut u: int | string = 1
u = true       // E0007: bool is not a member of `int | string`
```

A binding may carry a type annotation, which is a **checked boundary** (a mismatch is E0007) and is erased at runtime:

```noeta
xs: List<int> = [1, 2, 3]
count: int = 3
```

**There is no shadowing.** One name means one thing per scope stack. A binder, meaning a closure parameter, a `for` variable, or a match-pattern binding, may not reuse a name already bound in a scope it can see (E0059), and a binding may not reuse an imported name (E0020).

A plain `name = expr` never introduces a second binding. The first use in a scope declares it and a later one reassigns it, which is E0006 when the binding is immutable and E0007 when the type would change.

Named functions stay ergonomic under this rule because they are **sealed**. Their bodies do not see surrounding value bindings, so their parameters conflict with nothing (see [Functions & Closures](Functions-and-Closures#sealed-functions--the-use--capture-clause)).

### Reserved words

A name has to be one the language has not already taken. Three families are reserved: the keywords of the grammar (`fn`, `for`, `match`, `mut`, `struct`, `use`, `is`, `in`, …), the two boolean literals `true` and `false`, and the thirteen reflection primitives (`type_of`, `type_name`, `fields_of`, `field_specs_of`, `variants_of`, `construct`, `invoke`, `params_of`, `returns_of`, `roles_of`, `traits_of`, `attributes_of`, `from_bytes`). The six prelude names `Ok`, `Err`, `some`, `none`, `panic` and `assert` are held back the same way.

None of them can be bound, in any binder position: a parameter, a binding, a `for` variable, a closure parameter, a pattern binding, a field, a type, a generic parameter, or a function name.

Writing one where a name belongs is **E0046**, and the message names the word and what took it:

```noeta error
fn field_help(type_name: string, field: string): string {
    return type_name ~ "." ~ field
}
```

```text
[E0046] Error: `type_name` cannot be used as a name — it is one of the reflection primitives, reserved by the language so it means one thing everywhere it appears
   ╭─[ app.noe:1:15 ]
   │
 1 │ fn field_help(type_name: string, field: string): string {
   │               ────┬────  
   │                   ╰────── `type_name` is reserved
   │ 
   │ Help: rename it to `type_name_`
───╯
```

The reflection primitives are keywords because each has a call form, `type_name::<T>()` or `construct(name, fields)`, that a *user* function of the same name would be indistinguishable from at the call site. The name cannot be shared.

**Compound assignment** `name OP= expr` desugars to `name = name OP expr` for `+= -= *= /= %= ~=`:

```noeta
mut n = 10;  n += 5;  n *= 2;   echo n     // 30
mut acc = [];  acc ~= [1];  acc ~= [2];  echo acc   // [1, 2]  (list append)
mut s = "a";  s ~= "b";  echo s            // ab
```

> [!NOTE]
> An immutable, unannotated binding to a *context-free* literal (`[]`, `{}`, or an `Ok(x)` whose `Err` type is unknown) is E0023, "cannot infer". Fix it with a type annotation, or use a `mut` accumulator whose later writes supply the element type.

## Primitive types

| Type | Notes |
|---|---|
| `int` | 64-bit signed; **wraps** on overflow, never panics. |
| `float` | 64-bit IEEE-754. |
| `f32` | 32-bit strict fixed-width float; literal suffix `f32`. See below. |
| `f64` | 64-bit strict fixed-width float; literal suffix `f64`. Distinct from `float`. See below. |
| `bool` | `true` / `false`. |
| `string` | UTF-8 text. |
| `void` | The unit type (a function that returns nothing). |
| `dyn` | The dynamic top — any value. See [The Type System](Type-System). |

There are also [fixed-width integers](Fixed-Width-Integers) (`i8`…`u64`) and the abstract kind-types `Enum`/`Struct`/`Class`.

```noeta
echo 9223372036854775807 + 1   // -9223372036854775808  (int wraps)
```

## Number literals

Underscores may separate digits anywhere, `0x`/`0o`/`0b` are radix prefixes, and a `.` or `e` makes a literal a `float`.

```noeta
echo 1_000_000   // 1000000
echo 0xFF        // 255
echo 0b1010      // 10
echo 0o755       // 493
echo 0xDE_AD     // 57005
echo 1.5e3       // 1500.0
echo 2e-2        // 0.02
echo 3.141_592   // 3.141592
```

> [!NOTE]
> **Numeric conversions are explicit at a boundary.** An `int` is not implicitly a `float`, so a binding, argument, return, or element of type `float` rejects an `int`. Write the literal in the target type, `sqrt(4.0)` rather than `sqrt(4)`. Widening happens only *inside an expression*, where `int` and `float` combine in arithmetic and `x + 1` is a `float` when `x` is one. That result is then checked against its own boundary like any other value, so a widened `float` can never reach an `int` binding.

### `f32` / `f64` — strict fixed-width floats

Written with an `f32` or `f64` suffix. These are **strict fixed-width** types and stand outside the `int`/`float` widening above: both operands of an arithmetic operator must already be the same type. `f32 + float`, `f32 + int` and `f64 + float` are each E0044 and each need an explicit conversion. `f64` is a 64-bit float distinct from `float`, so assigning one where the other is expected is E0007. `f32` carries observably less precision than `float`:

```noeta
x = 1.5f32
echo x + 2.0f32        // 3.5
echo 0.1f32 + 0.2f32   // 0.3     (float would give 0.30000000000000004)
echo -1.5f32           // -1.5    (unary negation stays f32)
```

## The three string forms

**`"..."` — interpolated.** `${expr}` embeds any expression; bare `{` and `}` are literal.

```noeta
name = "Niro"
echo "Hello ${name}"         // Hello Niro
echo "sum is ${1 + 2 * 3}"   // sum is 7
echo "{not a hole}"          // {not a hole}
echo "say \"hi\""            // say "hi"
echo "esc \x1b[0m \u{1F600}" // an ASCII/control byte, and a Unicode scalar
```

The escapes are `\n`, `\t`, `\r`, `\"`, `\\`, `\$` (a literal `$`, so a literal `${` is `\${`), `\xHH` with exactly two hex digits naming an ASCII scalar `0x00`–`0x7F`, and `\u{H…H}` with 1 to 6 hex digits naming any non-surrogate Unicode scalar up to `0x10FFFF`. Any other escaped character is that character verbatim, so a stray `\q` is just `q`.

A malformed **numeric** escape is E0064, an invalid string escape, reported at the escape itself. That covers `\x` without two hex digits or naming a byte above `0x7F`, `\u` without braces, an empty `\u{}`, and a `\u{…}` that is a surrogate or above `0x10FFFF`.

**`'...'` — raw.** No interpolation; the only escapes are `\'` and `\\`.

```noeta
echo 'plain ${name} {braces} $dollar'   // literal, verbatim
echo 'tab\tnot-expanded'                // backslash-t literal
echo 'quote: it\'s'                     // quote: it's
```

**`` `...` `` — dedented template.** Multiline, interpolating like `"..."`, and stripping the common leading indentation along with the leading and trailing blank line. It suits SQL, HTML, and email bodies.

```noeta
name = "Ada"
echo `
    Dear ${name},
    Order #${1 + 6} shipped.
`
// Dear Ada,
// Order #7 shipped.
```

String methods (`.upper()`, `.split(",")`, …) are covered in [Built-ins](Standard-Library).

## Operators

| Category | Operators |
|---|---|
| Arithmetic | `+ - * / %` (unary `-`) |
| Concatenation | `~` |
| Comparison | `== != < <= > >=` |
| Identity (class only) | `=== !==` |
| Logical (short-circuit) | `&& \|\| !` |
| Bitwise | `& \| ^ << >> !` |
| Pipe | `\|>` |
| Try / coalesce | `? ?? ??=` |
| Range | `a..b` (exclusive), `a..=b` (inclusive) |
| Type test / narrow | `is T`, `.as<T>()` |

**`~` concatenation** joins two lists into a new list, or display-concatenates its operands into a string:

```noeta
echo [1, 2] ~ [3, 4]              // [1, 2, 3, 4]
echo "users/" ~ 42 ~ "/profile"  // users/42/profile
```

**`|>` pipe** threads the left value in as an *argument* of the right call, reading left to right. It fills the first parameter no [label](Functions-and-Closures#named-arguments) claimed, which by default is the first one:

```noeta
fn inc(x: int): int { return x + 1 }
fn add(a: int, b: int): int { return a + b }
fn div(a: int, b: int): int { return a / b }
echo 5 |> inc |> inc      // inc(inc(5))  -> 7
echo 5 |> add(10)         // add(5, 10)   -> 15
echo 5 |> div(a: 100)     // div(100, 5)  -> 20  (`a` is named, so the pipe fills `b`)
```

When the piped value is the *only* argument, the empty parentheses are optional, and `5 |> inc` and `5 |> inc()` are the same call.

**`??` coalesce** supplies a fallback for a `none` or absent value, and short-circuits, so the fallback runs only when needed. **`??=`** is `x = x ?? y`. [Error Handling](Error-Handling) covers these and the `?` try operator.

**Ranges** eagerly build a `List<int>`. `..` binds looser than `+` and `-`, so `0..n-1` means `0..(n-1)`. An empty range is `[]`:

```noeta
echo 0..5    // [0, 1, 2, 3, 4]
echo 0..=5   // [0, 1, 2, 3, 4, 5]
echo 5..2    // []
```

### Precedence

Tightest to loosest: postfix (call, `.`, `[i]`, try `?`) → unary `!`/`-` → `* / %` → `+ -` → shifts `<< >>` → `&` → `^` → `|` → `~` and ranges `..`/`..=` → comparison `< <= > >=` and `is T` → equality `== != === !==` → `&&` → `||` and `??` → `|>` (loosest). Parentheses override.

The consequences worth knowing:

- Bitwise binds *tighter* than comparison, so `5 & 3 == 1` is `(5 & 3) == 1`.
- `~` and `..` sit between bitwise and comparison, so `1 + 2 ~ "x"` is `(1 + 2) ~ "x"` (giving `3x`), `"a" ~ "b" == "ab"` is `("a" ~ "b") == "ab"` (giving `true`), and `0..n-1` is `0..(n-1)`.
- `is` is at the comparison tier, so `a + b is int` is `(a + b) is int` and `x is int == true` is `(x is int) == true`.
- `??` sits alongside `||`, tighter only than the pipeline, so `a ?? b |> f` is `(a ?? b) |> f`.

See also [Fixed-Width Integers & Bitwise](Fixed-Width-Integers) for the bitwise operators in depth.
