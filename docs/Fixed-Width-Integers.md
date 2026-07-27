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
```

- On a plain `int`, `!` is **bitwise complement**; on a `bool` it stays logical NOT.
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

Conversions are total (`to_i8`, `to_u8`, …, `to_i64`, `to_u64`, `to_int`), with Rust-`as` semantics — widening is lossless, narrowing truncates, and crossing signedness reinterprets:

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

Iteration/display order over packed keys is field-wise (declaration order, negatives before positives) — the same total order sets and `sorted()` use. Float/`f32` fields disqualify a struct as a key (`NaN != NaN` makes float keys a footgun). See [Built-ins: Map](Standard-Library#map) for the key-capability rules.

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

See [std.vec](std-vec) and [std.quat](std-quat) for the full `vec`/`quat` surface, and [Performance Techniques](Performance-Techniques) for *why* the column layout beats hand-written SIMD here (LLVM autovectorization).
