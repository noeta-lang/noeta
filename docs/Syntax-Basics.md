# Syntax Basics

The lexical foundation: comments, statement termination, bindings, primitive types, literals, strings, and operators. Everything here runs with `noeta run`.

## Comments

`//` starts a line comment (to end of line); `/* … */` is a block comment. Block comments **nest**, so you can comment out a region that already contains a block comment.

```noeta
// a line comment
echo 1        // trailing comment

/* a block comment
   spanning several lines */
x = /* inline */ 2 + /* they nest: /* inner */ still commented */ 3
echo x        // 5
```

A block comment may span a statement boundary — like a line continuation, the enclosing statement continues across it.

## Statements and semicolons

**Semicolons are optional** — a newline ends a statement. A `;` is still valid, and required to put two statements on one line.

```noeta
echo "a"
echo "b"
echo "c"; echo "d"   // two statements, one line
```

A line **continues** onto the next (no statement break inserted) when the break is clearly mid-expression — the next line starts with an infix/postfix operator, `.`, `|>`, `??`, `..`, a comma, `=>`, `->`, a closing bracket, or a clause keyword (`else`, `then`, `in`, `as`, `is`), or the current line ends with an open `(`/`[`:

```noeta
total = 1 +          // trailing operator → continues
        2 + 3
scaled = [1, 2, 3]
    |> map(fn(x) => x * 2)   // leading |> → continues
    |> sum()
echo total               // 6
```

Type, `struct`, and `class` bodies are newline-separated — fields need no terminator.

## `echo`

`echo` prints one value, followed by a newline, using the value's `Display` form.

```noeta
echo "hello"    // hello
echo 1 + 2      // 3
echo [1, 2, 3]  // [1, 2, 3]
```

## Bindings and mutability

`name = expr` binds immutably; `mut name = expr` binds mutably. Reassigning an immutable binding is an error (E0006).

```noeta
x = 10               // immutable
mut total = 0        // mutable
total = total + 5    // ok
```

A binding may carry a type annotation, which is a **checked boundary** (mismatch is E0007), erased at runtime:

```noeta
xs: List<int> = [1, 2, 3]
count: int = 3
```

**Shadowing** is lexical — a new binding (or a parameter) with an existing name shadows it in that scope.

**Compound assignment** `name OP= expr` desugars to `name = name OP expr` for `+= -= *= /= %= ~=`:

```noeta
mut n = 10;  n += 5;  n *= 2;   echo n     // 30
mut acc = [];  acc ~= [1];  acc ~= [2];  echo acc   // [1, 2]  (list append)
mut s = "a";  s ~= "b";  echo s            // ab
```

> [!NOTE]
> An immutable, unannotated binding to a *context-free* literal — `[]`, `{}`, or an `Ok(x)` whose `Err` type is unknown — is E0023 ("cannot infer"). Fix it with a type annotation, or use a `mut` accumulator whose later writes supply the element type.

## Primitive types

| Type | Notes |
|---|---|
| `int` | 64-bit signed; **wraps** on overflow, never panics. |
| `float` | 64-bit IEEE-754. |
| `f32` | 32-bit float; literal suffix `f32`. See below. |
| `bool` | `true` / `false`. |
| `string` | UTF-8 text. |
| `void` | The unit type (a function that returns nothing). |
| `dyn` | The dynamic top — any value. See [The Type System](Type-System). |

There are also [fixed-width integers](Fixed-Width-Integers) (`i8`…`u64`) and the abstract kind-types `Enum`/`Struct`/`Class`.

```noeta
echo 9223372036854775807 + 1   // -9223372036854775808  (int wraps)
```

## Number literals

Underscores may separate digits anywhere; `0x`/`0o`/`0b` are radix prefixes; a `.` or `e` makes a literal a `float`.

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

### `f32` — 32-bit float

Written with an `f32` suffix. The widening lattice is `int < f32 < float`: `f32 op int → f32`, `f32 op float → float`. It is observably lower-precision than `float`:

```noeta
x = 1.5f32
echo x + 2.0f32        // 3.5
echo 0.1f32 + 0.2f32   // 0.3     (float would give 0.30000000000000004)
echo -1.5f32           // -1.5    (unary negation stays f32)
```

## The three string forms

**`"..."` — interpolated.** `${expr}` embeds any expression; bare `{`/`}` are literal.

```noeta
name = "Niro"
echo "Hello ${name}"         // Hello Niro
echo "sum is ${1 + 2 * 3}"   // sum is 7
echo "{not a hole}"          // {not a hole}
echo "say \"hi\""            // say "hi"
```

**`'...'` — raw.** No interpolation; the only escapes are `\'` and `\\`.

```noeta
echo 'plain ${name} {braces} $dollar'   // literal, verbatim
echo 'tab\tnot-expanded'                // backslash-t literal
echo 'quote: it\'s'                     // quote: it's
```

**`` `...` `` — dedented template.** Multiline, interpolates like `"..."`, but strips the common leading indentation and the leading/trailing blank line — ideal for SQL, HTML, or email bodies.

```noeta
name = "Ada"
echo `
    Dear ${name},
    Order #${1 + 6} shipped.
`
// Dear Ada,
// Order #7 shipped.
```

String methods (`.upper()`, `.split(",")`, …) are covered in the [Standard Library](Standard-Library).

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

**`~` concatenation** — joins two lists into a new list, or display-concatenates operands into a string:

```noeta
echo [1, 2] ~ [3, 4]              // [1, 2, 3, 4]
echo "users/" ~ 42 ~ "/profile"  // users/42/profile
```

**`|>` pipe** — threads the left value as the *first argument* of the right call, reading left-to-right:

```noeta
fn inc(x: int): int { return x + 1 }
fn add(a: int, b: int): int { return a + b }
echo 5 |> inc |> inc      // inc(inc(5))  -> 7
echo 5 |> add(10)         // add(5, 10)   -> 15
```

**`??` coalesce** supplies a fallback for `none`/absent (short-circuiting — the fallback runs only when needed); **`??=`** is `x = x ?? y`. The `?` try operator and these are covered in [Error Handling](Error-Handling).

**Ranges** eagerly build a `List<int>`. `..` binds looser than `+`/`-`, so `0..n-1` means `0..(n-1)`; an empty range is `[]`:

```noeta
echo 0..5    // [0, 1, 2, 3, 4]
echo 0..=5   // [0, 1, 2, 3, 4, 5]
echo 5..2    // []
```

### Precedence

Tightest to loosest: unary `!`/`-` → `* / %` → `+ -` → shifts `<< >>` → bitwise `& ^ |` → comparison/equality → `&&` → `||`. Parentheses override. Bitwise binds *tighter* than comparison, so `5 & 3 == 1` is `(5 & 3) == 1`.

See also [Fixed-Width Integers & Bitwise](Fixed-Width-Integers) for the bitwise operators in depth.
