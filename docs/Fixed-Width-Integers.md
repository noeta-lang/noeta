# Fixed-Width Integers & Bitwise

Beyond `int` (a 64-bit signed integer), the language has eight explicit fixed-width integer types and a full set of bitwise operators. These matter for binary formats, hashing, protocol code, and packed numeric data.

## The fixed-width types

`i8 i16 i32 i64 u8 u16 u32 u64` — written with a suffix on an integer literal (any radix, `_` separators allowed):

```lang
a = 255u8
b = 0xFFi32
c = 0b1010u16
d = 1_000u32
```

- There is **no implicit widening** — moving a value between widths is explicit (via the conversion methods below).
- An untyped literal **coerces** into a fixed-width annotation when it is in range: `x: u16 = 1000`.
- At runtime a fixed-width value is erased to a 64-bit word, so `type_of(255u8)` reports `Type.Int`.

Arithmetic **wraps** at the type's width, sign-appropriately:

```lang
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

```lang
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

```lang
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

```lang
echo (300).to_u8()    // 44   (300 mod 256)
```

## Packed value types and `bytes`

Fixed-width fields let you define **packed value types** — structs stored as a flat, unboxed, cache-friendly numeric buffer rather than an array of pointers. A `List` of a packed type serializes to and from an opaque `bytes` buffer:

```lang
blob = packed_list.to_bytes()          // -> bytes
back = from_bytes::<Vec3>(blob)         // -> List<Vec3>
```

`bytes` is an opaque binary buffer (`b.count()` gives its length, it compares by content, and `type_of(b)` is `Type.Bytes`). This flat layout is also what makes the vector-math kernels fast — see [Standard-Library Modules](Standard-Library-Modules#vec--quat) for `vec`/`quat`, and [Performance Techniques](Performance-Techniques) for how the layout unlocks autovectorization.
