# Fixed-Width Ints & Packed Types

Beyond `int` (a 64-bit signed integer), the language has eight explicit fixed-width integer types with a full set of bitwise operators, and `@packed` value types — structs stored as flat numeric buffers instead of heap objects. The first half of this page covers the widths and bit operations (binary formats, hashing, protocol code); the second half covers packed and column layouts (bulk numeric data).

## The fixed-width types

`i8 i16 i32 i64 u8 u16 u32 u64` — written with a suffix on an integer literal (any radix, `_` separators allowed):

```noeta
a = 255u8
b = 0xFFi32
c = 0b1010u16
d = 1_000u32
```

- There is **no implicit widening** — moving a value between widths is explicit (via the conversion methods below).
- An untyped literal **coerces** into a fixed-width annotation when it is in range: `x: u16 = 1000`.
- At runtime a fixed-width value is erased to a 64-bit word, so `type_of(255u8)` reports `Type.Int`. Declared-type reflection agrees by construction: a top-level `i32`/`u8` parameter reflects `Type.Int` through `params_of` too (and a top-level `f64` reflects `Type.Float`; `f32` is reified everywhere and reports `Type.F32`). In **container-element position** a width is a real storage slot and is preserved: a `List<i32>` annotation reflects `Type.List(Type.IntN(32, true))`, at any depth. See [Attributes & Reflection](Attributes-and-Reflection#params_ofname-listparaminfo).
- **Serializing** a value to JSON reads its static type the same way, so a `u64` above `i64::MAX` is written as its unsigned value — through `json.stringify`, a derived `to_json()` or `inspect()`, and inside a list, map value, map key, tuple, struct field or enum payload. A value **bound** for later serialization is written the same way, from the type it was bound under: a LiveView binding (`view.expose("count", count)` over a `Signal<u64>`) pushes its unsigned value in every snapshot and patch frame, on ticks long after the binding call. Decoding is the mirror: a fixed-width field accepts a JSON number that fits its width and reports one that does not (`expected u8, found 300 — out of range for u8`), so what `json.stringify` writes `json.parse::<T>` reads back. The **dynamic** `json.parse(text)` has no target type to read a width from, so a number beyond `int` arrives as a `float` like any other oversized number; name the type (`json.parse::<u64>(text)`) to recover it exactly.
- **Displaying** a value reads its static type, so a `u64` above `i64::MAX` prints its unsigned value — at `echo`, in an interpolation hole, in a `~` concatenation, at the REPL's echo of a trailing bare expression, in the string `.join(sep)` builds from a list's or an iterator's elements, and inside a list, set, map, tuple, object field or enum payload rendered from a `u64`-typed position. Once a value is laundered through `dyn` the width is gone (exactly as it is for a type test), and the erased 64-bit word prints as a signed `int` — laundering a `u64` warns (E0078) at the point the width is discarded, since that is the last place a diagnostic can name it — as it does at a `noeta repl --no-check` prompt, which runs no checker and so has no static type to read.
- **Ordering** a value reads its static type too, so a `u64` above `i64::MAX` sorts above every smaller value: `.sorted()`, `.min()`, `.max()` (on a list or an iterator), a rendered set or map, `.keys()`, `.values()`, and a `for` over a set or map all place it by its value. Inside a `dyn` position the width is gone, and those doors fall back to the erased word's order.
- **Computing** with a value reads its static type as well, and that is a different question from the three above: those ask how a value *reads*, this asks where the arithmetic *wraps*. Every collection door that turns elements into a number answers at the element's width, whatever the values happen to be and however the list was built — `[200u8, 100u8].sum()` is `44` and its `.product()` is `32`, whether that list is the literal or the result of `.map(fn(x) => x)`, `iter().collect()`, or any other route. `checked_sum` reports at the same width, so the same list is `none`. `scale`, `neg` and the element-wise `+`/`-`/`*` wrap there; `abs` folds around it, so an unsigned element is handed straight back and `[-128i8].abs()` stays `[-128]` because 128 is not an `i8`. Inside a `dyn` position, or a generic body whose element type is a type parameter, there is no width to read and the erased 64-bit word is what the fold wraps at.
- A **generic function** reaches all three doors at the width its caller chose: `fn wrap<T>(v: T): string { return json.stringify(v); }` called at `T = u64` writes the unsigned value, and called at `T = i64` writes the negative one. The type parameter names no width itself, so the answer comes from the call — through the parameter directly (`v: T`), through a container of it (`List<T>`, `Map<string, T>`, `?T`, `Set<T>`), through a generic struct's field, through a container literal written inline at the call (`wrap_list([big])` reads exactly as `xs: List<u64> = [big]; wrap_list(xs)` does), and through a further generic the body calls. That last one includes a call instantiated at a **composite** of the body's own parameter: `fn outer<T>(xs: List<T>): string { return wrap(xs); }` calls `wrap` at `List<T>`, and the width still arrives, however many generic frames the value crosses on the way down.
- A **generic type's method** reaches them at the width its *receiver* was built with: inside `class Holder<T>`, `"${self.v}"`, `json.stringify(self.v)` and `self.xs.sorted()` all read `T` as whatever the instance was constructed at. The instance records that when it is built — a written literal (`h: Holder<u64> = Holder { v: big }`) or a constructor called on the type (`Holder.new(big)`) — so a value whose instantiation was not known where it was constructed reads the erased word instead, as a `dyn` does. A binding made from inside either kind of generic body keeps the width it was bound at, so a `view.expose` written in a helper still pushes unsigned values on every later tick.
- A composite a generic body **builds** carries the width too, not only the ones its parameter types name: `fn f<T>(v: T): string { return wrap([v]); }` calls `wrap` at `List<T>`, an instantiation no caller of `f` ever wrote, and the `u64` still arrives. It works at any depth (`wrap([[v]])`), for every container shape (`wrap({"k": v})`, a generic struct built from the parameter), inside a generic type's method, and again a frame further down when the body it was handed to builds on top of it. Two positions do read the erased word, for the same reason a `dyn` does: a `dyn` local inside a generic body, and a generic function taken as a value with nothing to pin its instantiation. Nothing there is refused. Putting a `u64` into a `dyn` position **warns** (E0078), because that is the one width whose digits change when the type stops naming it — every narrower width, and every `iN`, fits the signed word and renders identically through it. The generic-value position gets no warning: nothing at that site knows whether a `u64` will ever pass through it, and a warning naming a value that may not exist is worse than none.
- A **declared `u64` field** is different: its width is part of the *type*, so `@derive(Comparable)` orders it unsigned wherever the value goes — `<`/`>`, `.sorted()`, a set's canonical order, a nested field — including after a `dyn` launder.
- A `u64` **map key** or **set member** is *stored* in the erased word's order, which is what makes a lookup total: a map keyed past bit 63 finds its keys through `m[k]`, `has`, `get` and `remove` no matter which position the map is read from. What a program observes — the rendered order, `keys()`, `values()`, iteration — is the type's order, so those agree with each other.
- A **type test against a width** (`x is i32`, `x is f64`) is answered by the checker, not at runtime: `a: i32` makes `a is i32` `true` and `a is i64` `false`, decided where the width actually lives — in the static type. Once a value is laundered through `dyn` the width is gone and nothing can recover it, so a test there is **E0063** (a warning) and you should test the base type instead (`x is int`, `x is float`). `f32` is exempt — it is reified, so the runtime answers it — and so is a container target like `List<i32>`.
- **Narrowing** to a width (`x.as<u64>()`, `x.as<i32>()`, `x.as<f64>()`) is **E0028**, because it could never succeed: `.as<T>()` needs an open source, and an open source is exactly where the width is gone, so the narrow would answer `none` for every value including one written as that width. Narrow to the base type and annotate the result — `n: u64 = (v.as<int>() ?? 0).to_u64()` — which is where a width belongs anyway. `f32` narrows normally (it is reified) and so does a container target like `List<i32>`.

Arithmetic **wraps** at the type's width, sign-appropriately:

```noeta
echo 255u8 + 1u8      // 0     (unsigned wrap mod 256)
echo 127i8 + 1i8      // -128  (signed wrap)
echo -128i8           // the min signed i8 (bare 128i8 would overflow)
echo 7i32 / 2i32      // 3     (division/remainder/order are sign-dependent)
```

Mixing two different fixed widths in one arithmetic expression is E0044 — convert first.

## Bitwise and shift operators

The bitwise operators work on both `int` and the fixed-width types:

| Operator | Meaning |
|---|---|
| `&` | AND |
| `\|` | OR |
| `^` | XOR |
| `<<` | left shift |
| `>>` | right shift |
| `!` | complement (on `int`/fixed-width); logical NOT on `bool` |

```noeta
echo 5 & 3            // 1
echo 0xF0 | 0x0F      // 255
echo 0b1100 ^ 0b1010  // 6
echo 1 << 4           // 16
echo 256 >> 2         // 64
echo !0               // -1     (bitwise complement:  !x == -(x + 1))
echo !1u8             // 254    (a fixed width complements within its width)
```

- On a plain `int`, `!` is **bitwise complement**; on a `bool` it stays logical NOT.
- On a fixed width, `!` complements **within the width**, so `!1u8` is `254` and `!255u8` is `0` — the same wrap `+ - *` take. Every door reads that one value: `!1u8 == 254u8`, `echo !1u8` prints `254`, and `[!1u8, 5u8].sorted()` is `[5, 254]`.
- Right shift `>>` is arithmetic on signed types and logical on unsigned types.
- **Precedence is Rust-style**: bitwise operators bind *tighter than* comparison, and shifts bind just below `+`/`-`. So `5 & 3 == 1` parses as `(5 & 3) == 1` → `true`.

## Bit intrinsics and conversions

Every integer carries bit-manipulation methods (width-relative on a fixed-width receiver):

```noeta
echo (0b1011).count_ones()     // 3
echo (1).rotate_left(4)        // 16
```

| Method | Result |
|---|---|
| `count_ones()` / `count_zeros()` | Population counts. |
| `leading_zeros()` / `trailing_zeros()` | Leading/trailing zero bits. |
| `rotate_left(n)` / `rotate_right(n)` | Cyclic rotation (amount mod width). |
| `reverse_bits()` | Reverse bit order. |
| `swap_bytes()` | Reverse byte order. |

The `to_*` conversions are total (`to_i8`, `to_u8`, …, `to_i64`, `to_u64`, `to_int`), with Rust-`as` semantics — widening is lossless, narrowing truncates, and crossing signedness reinterprets:

```noeta
echo (300).to_u8()    // 44   (300 mod 256)
```

The same `to_*` family bridges the **integer and float domains** in both directions — `to_float` (an alias `to_f64`), `to_f32`, and, on a `float`/`f32`, `to_int` / `to_i8` … / `to_u64`:

```noeta
echo (5).to_f32()        // 5.0   (int -> f32; build f32 data from a computed int)
echo (2.5).to_f32()      // 2.5   (float -> f32, rounds to nearest)
echo (2.5f32).to_float() // 2.5   (f32 -> float, exact widening)
echo (3.9).to_int()      // 3     (float -> int, truncates toward zero)
echo (1000.0).to_u8()    // 255   (float -> int SATURATES to the width; negatives clamp to 0)
```

Int→float is value-preserving (rounding to nearest on `f32`); float→int truncates toward zero and **saturates** to the destination range, with `NaN` → 0.

### Range-checked conversions

Every integer destination also has a **range-checked** spelling — `checked_to_i8`, `checked_to_u8`, …, `checked_to_u64`, `checked_to_int` — which returns `?T` instead of `T` and answers `none` when the value does not fit:

```noeta
echo 200u16.checked_to_u8()   // some(200)
echo 300u16.checked_to_u8()   // none    (300 does not fit a u8; `to_u8()` would give 44)
echo 200u8.checked_to_i8()    // none    (crossing signedness is a range question too)
echo (-1).checked_to_u64()    // none
```

The two families answer the same question from opposite sides: `x.checked_to_T()` is `some(x.to_T())` at every input where `to_T()` is exact, and `none` at exactly the inputs where `to_T()` has to wrap (an integer receiver) or saturate (a float receiver). So reaching for the checked form never changes a value — it only adds the case where there is no value to give.

A float receiver checks the range of the value it truncates toward zero, and `NaN` and the infinities are in no destination's range:

```noeta
echo (3.9).checked_to_int()    // some(3)   (truncation is not a range failure)
echo (1000.0).checked_to_u8()  // none      (`to_u8()` would saturate to 255)
```

Float *destinations* have no checked spelling: `to_f32` narrows by rounding, which loses precision rather than range, and there is no value it could refuse.

Use `??` to supply a fallback, or match the option when the out-of-range case needs handling of its own:

```noeta
echo 300u16.checked_to_u8() ?? 0u8   // 0
```

## Packed value types — `@packed`

The `@packed` directive marks a **struct** as a *packed value type*: a `List` of it is stored as a flat, unboxed, contiguous numeric buffer rather than an array of heap-object pointers. This is a pure *representation* change — the flat layout is invisible to program behavior (a packed list `==`, displays, and iterates exactly like a boxed one), but it is dramatically more cache-friendly and unlocks vectorized bulk math. For *why* that pays off — cache behavior, autovectorization, and the benchmarks — see [Performance Techniques](Performance-Techniques).

```noeta
@packed struct V { x: int  y: int }

mut acc = [V { x: 0, y: 0 }, V { x: 1, y: 1 }, V { x: 2, y: 2 }]
acc = acc.set(1, V { x: 9, y: 9 })     // in-place flat-slot overwrite when uniquely owned
echo acc                                // [V {x: 0, y: 0}, V {x: 9, y: 9}, V {x: 2, y: 2}]
echo acc.len()                          // 3
```

Rules:

- `@packed` marks a **struct** only. On a class it is E0054, a misplaced directive (a class has identity; a packed value type is a value).
- Fields must be packable — fixed-width ints, `int`, `f32`, `bool`, or a nested `@packed` struct. A non-primitive field is rejected.
- Every list operation (index, field read, iteration, `set`, `~`/concat, `slice`/`reverse`/`filter`/`map`) yields exactly what the boxed layout would.

A packed struct whose fields are all integers/`bool` (or nested such structs) is also **key-capable**: it can key a `Map` (or element a `Set`) **by content** — the spatial-hash idiom:

```noeta
@packed struct Cell { x: int; y: int }

mut grid: Map<Cell, int> = {}
grid[Cell { x: 3, y: 4 }] = 42          // keyed by value, not identity
echo grid[Cell { x: 3, y: 4 }]          // 42 — a fresh equal value finds it
echo grid.keys()                        // [Cell {x: 3, y: 4}] — full struct values again
```

Iteration/display order over packed keys is field-wise (declaration order, negatives before positives, a `u64` field by its unsigned value) — the same total order sets and `sorted()` use. A `u64` field's *digits* read unsigned too, at whatever depth it sits: a rendered map's keys, `keys()` and iteration all print `Tick {at: 18446744073709551615}` for a key whose field is a nested packed struct's as readily as for a flat one. Float/`f32` fields disqualify a struct as a key (`NaN != NaN` makes float keys a footgun). See [Built-ins: Map](Standard-Library#map) for the key-capability rules.

## Column (SoA) layout — `@packed(Layout.Column)`

By default a packed list is *row-major* (array-of-structs). `@packed(Layout.Column)` stores it **column-major** (struct-of-arrays) — each field in its own contiguous column. The argument is the built-in `Layout` enum — `Layout.Row` (the bare-`@packed` default) or `Layout.Column` — the same `Enum.Variant` shape `@role` takes. This is, again, a pure performance attribute invisible to results, but it is the layout the autovectorized bulk kernels reduce fastest.

```noeta
@packed(Layout.Column) struct P { r: int  g: int  b: int  opaque: bool }

ps = [P { r: 255, g: 0, b: 128, opaque: true }, P { r: 1, g: 2, b: 3, opaque: false }]
echo ps[1]              // P {r: 1, g: 2, b: 3, opaque: false}  (gather one element from its columns)
echo ps[0].r           // 255  (a fused single-column field read)
echo ps.reverse()      // [P {r: 1, ...}, P {r: 255, ...}]
```

## `bytes` — serialize a packed list

A `List` of a packed type round-trips through an opaque `bytes` buffer with `.to_bytes()` and `from_bytes::<T>(...)`:

```noeta
@packed struct V3 { x: f32  y: f32  z: f32 }

xs = [V3 { x: 1.0f32, y: 2.0f32, z: 3.0f32 }, V3 { x: 4.0f32, y: 5.0f32, z: 6.0f32 }]
blob = xs.to_bytes()                    // -> bytes
ys = from_bytes::<V3>(blob)             // -> List<V3>
echo ys == xs                           // true
echo blob.len()                         // 24  (2 elements × 3 × 4-byte f32)
```

`bytes` is an opaque binary buffer — for its general API (`len`, `to_hex`, `decode`, content comparison), see [Built-ins: bytes](Standard-Library#bytes).

## Bulk vector kernels

The flat/column layout is what makes the `vec.*_all` bulk kernels fast — they take the autovectorized struct-of-arrays path over a column buffer:

```noeta
use std.{vec}
@packed(Layout.Column) struct V3 { x: f32  y: f32  z: f32 }

ps = [V3 { x: 3.0f32, y: 4.0f32, z: 0.0f32 }, V3 { x: 1.0f32, y: 2.0f32, z: 2.0f32 }]
echo vec.length_all(ps)                 // [5.0, 3.0]     (reduction → f32 list)
echo vec.add_all(ps, ps)                // a column V3 list, element-wise doubled
```

The free `vec.*` functions are `f32` three-vector math. For the same operations at any width — an `i32` or `f64` vector, two components or four — bind the `vec.Kernels` [method bundle](Native-Extensions#method-bundles-impl-veckernels-for-px-) on your own `@packed` struct and call them as methods.

See [std.vec](std-vec) and [std.quat](std-quat) for the full `vec`/`quat` surface, and [Performance Techniques](Performance-Techniques) for *why* the column layout beats hand-written SIMD here (LLVM autovectorization).
