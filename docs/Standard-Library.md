# Built-ins (Ring 1)

The **no-import surface**: strings, lists, maps, sets, options/results, iterators, `bytes`, and integer bit-methods are available in every program without a single `use`. For the importable `use std.{…}` modules (`math`, `json`, `fs`, …), see the [standard library reference](Std).

> [!NOTE]
> **Collections are value-semantic (copy-on-write).** A method like `set`/`add`/`remove` returns a **new** value; the receiver is unchanged. The exceptions — file handles, iterators, and channel endpoints — are *reference* values with a shared mutable cursor. Display forms are deterministic: lists `[1, 2, 3]`, maps `{"a": 1}`, sets `{1, 2, 3}` (sorted, de-duplicated), `some(x)`/`none`, `Ok(x)`/`Err(e)`; whole floats print with one decimal (`2.0`).

## Strings

Semantics are Unicode-scalar-based; a wrong arity or argument type is E0007.

| Method | Signature | Example → result |
|---|---|---|
| `upper` | `upper() -> string` | `"hi".upper()` → `HI` |
| `lower` | `lower() -> string` | `"HI".lower()` → `hi` |
| `trim` | `trim() -> string` | `"  x  ".trim()` → `x` |
| `trim_start` / `trim_end` | `() -> string` | `"  x  ".trim_start()` → `x  ` |
| `contains` | `contains(needle: string) -> bool` | `"hello".contains("ell")` → `true` |
| `starts_with` | `starts_with(prefix: string) -> bool` | `"hello".starts_with("he")` → `true` |
| `ends_with` | `ends_with(suffix: string) -> bool` | `"hello".ends_with("lo")` → `true` |
| `split` | `split(sep: string, limit?: int) -> List<string>` | `"a,b,c".split(",")` → `["a", "b", "c"]`; `"a,b,c".split(",", 2)` → `["a", "b,c"]` |
| `chars` | `chars() -> List<string>` | `"héy".chars()` → `["h", "é", "y"]` |
| `lines` | `lines() -> List<string>` | `"a\nb\n".lines()` → `["a", "b"]` (handles `\r\n`) |
| `replace` | `replace(from: string, to: string) -> string` | `"a.b".replace(".", "/")` → `a/b` |
| `repeat` | `repeat(n: int) -> string` | `"ab".repeat(3)` → `ababab` |
| `slice` | `slice(start: int, end?: int) -> string` | `"héllo".slice(1, 3)` → `él`; `"héllo".slice(1)` → `éllo` (end defaults to the length; chars, half-open; out of bounds is E0016) |
| `char_at` | `char_at(i: int) -> ?string` | `"héy".char_at(1)` → `some(é)`; out of range → `none` |
| `index_of` | `index_of(sub: string, from?: int) -> ?int` | `"héllo".index_of("llo")` → `some(2)` (char index); `from` starts the search; absent → `none` |
| `pad_start` / `pad_end` | `(width: int, fill?: string) -> string` | `"7".pad_start(3, "0")` → `007`; `"7".pad_start(3)` → `··7` (fill defaults to a space) |
| `is_empty` | `is_empty() -> bool` | `"".is_empty()` → `true` |
| `to_int` | `to_int() -> ?int` | `"42".to_int()` → `some(42)`; `"4.2".to_int()` → `none` |
| `to_float` | `to_float() -> ?float` | `"4.2".to_float()` → `some(4.2)` |
| `to_bytes` | `to_bytes() -> bytes` | `"hé".to_bytes().len()` → `3` (UTF-8; `bytes.decode()` is the inverse) |
| `len` | `len() -> int` | `"héllo".len()` → `5` |

A `?` on a parameter marks it **optional** — a built-in method with a trailing-optional parameter accepts either arity (`"a,b".split(",")` and `"a,b,c".split(",", 2)` both type-check), exactly like a `use std.{…}` module function. Supplying too many arguments is still a static error.

Splitting on `""` yields characters (same as `chars()`). Parsing is strict — `to_int`/`to_float` return `none` on surrounding whitespace or malformed input; compose with `trim()`. Also: index `s[i]` returns the i-th character (out of bounds is E0016), `s.len()` counts scalars, `"…${e}…"` interpolates, and `a ~ b` concatenates (display-concatenating non-strings).

## `List<T>`

Construct with `[a, b, c]`; an empty list in an ambiguous position needs a type (`xs: List<int> = []`).

```noeta
mut xs = [1, 2, 3]
echo xs[0]              // 1  (index; out of bounds is E0016)
echo [...xs, 4]        // [1, 2, 3, 4]  (spread)
echo xs ~ [4, 5]       // [1, 2, 3, 4, 5]  (concat)
xs[1] = 20             // sugar for  xs = xs.set(1, 20)  (needs a mut binding)
```

| Method | Signature | Example → result |
|---|---|---|
| `reverse` | `reverse() -> List<T>` | `[3,1,2].reverse()` → `[2, 1, 3]` |
| `contains` | `contains(x: T) -> bool` | `[1,2,3].contains(2)` → `true` |
| `join` | `join(sep?: string) -> string` | `["a","b"].join("-")` → `a-b`; `[1,2,3].join()` → `123` (sep defaults to empty) |
| `sorted` | `sorted() -> List<T>` | needs an ordered `T` (see [Reductions](#reductions)); `[3,1,2].sorted()` → `[1, 2, 3]` |
| `slice` | `slice(start: int, end?: int) -> List<T>` | `[1,2,3,4].slice(1,3)` → `[2, 3]`; `[1,2,3,4].slice(1)` → `[2, 3, 4]` (end defaults to the length) |
| `first` | `first() -> ?T` | `[1,2].first()` → `some(1)`; `[].first()` → `none` |
| `last` | `last() -> ?T` | `[1,2].last()` → `some(2)` |
| `to_set` | `to_set() -> Set<T>` | `[3,1,2,1].to_set()` → `{1, 2, 3}` |
| `to_bytes` | `to_bytes() -> bytes` | on a `List<@packed>`, its flat backing buffer (see [packed types](Fixed-Width-Integers)) |
| `set` | `set(i: int, v: T) -> List<T>` | `[1,2,3].set(2, 30)` → `[1, 2, 30]` |
| `len` | `len() -> int` | `[1,2,3].len()` → `3` |
| `enumerate` | `enumerate() -> List<(int, T)>` | `["a","b"].enumerate()` → `[(0, "a"), (1, "b")]` |
| `iter` | `iter() -> Iterator<T>` | see [Iterators](#iterators) |

**Eager collection methods** chain directly (each returns a plain value, unlike the lazy `iter()` adapters):

```noeta
echo [1, 2, 3].len()                                // 3
echo [1,2,3,4].filter(fn(n) => n % 2 == 0)
              .map(fn(n) => n * 10)
              .sum()                                // 60
```

- `xs.len()`, `xs.map(f)`, `xs.filter(pred)`, `xs.sum()` (int for `List<int>`, else float). To pass one as a value, take an unbound method handle: `f = list.len`, `xss.map(list.len)`.

### Reductions

A reduction folds the whole list to one value, and each one asks something of the **element type**.

| Method | Signature | Needs | Example → result |
|---|---|---|---|
| `min` / `max` | `min() -> ?T` | an ordered `T` | `["b","a"].max()` → `some(b)`; `[].max()` → `none` |
| `sum` / `product` | `sum() -> T` | a numeric `T` | `[1,2,3].sum()` → `6` |
| `checked_sum` | `checked_sum() -> ?T` | a numeric `T` | `none` instead of wrapping on integer overflow |
| `scale` / `abs` / `neg` / `clamp` | `scale(s: T) -> List<T>` | a numeric `T` | `[1,2].scale(3)` → `[3, 6]` |
| `any` / `all` | `any() -> bool` | `T = bool` | `[false,true].any()` → `true` |
| `count_true` | `count_true() -> int` | `T = bool` | `[true,false,true].count_true()` → `2`; `len()` is the size, and an iterator's `count()` is its element count |

`min`/`max` order by the **same order** `sorted()` sorts by, so `xs.min()` and `xs.sorted().first()` are always the same value. `sum`/`product` fold at the element's numeric width and wrap there, exactly as repeated `+`/`*` would. `checked_sum` reports at that same width instead of wrapping, so it answers by the element type rather than by the widest integer: a `List<u64>` summing past the top of `u64` is `none`, while one whose total merely passes the top of `i64` is `some` of the whole value.

The element-wise four answer by the element type too. `scale` and `neg` wrap at its width the way `*` and unary `-` do; `abs` and `clamp` **compare** at it, so an unsigned element is already non-negative and `abs` hands it straight back — `[18446744073709551615u64].abs()` is itself, not `1` — and a value near the top of `u64` clamps down to the high bound rather than up to the low one.

### What "an ordered `T`" means

`sorted()`, `min()` and `max()` hand the program an order, so they ask the element type for the same thing `<` asks for: an ordering it **declares**. Numbers, strings and bools have one built in; `?T` and `Result<T, E>` have one when their payloads do (variant first — `none` sorts below every `some(x)`); a native type declares one when it has one (`Uuid` orders by its bytes, which for `uuid_v7` is creation time; `Instant` and `Zoned` by their timestamp; a regex `Match` by position, so a list of them sorts) — and one without an order says so by staying silent, which is why a `Duration` cannot be sorted: a calendar span has no order until you name the date it is measured from; and a struct, class or enum you write declares one with [`@derive(Comparable)`](Derives), which orders it field-wise, or with an `impl Comparable` of its own ([Ordering your own type](Generics-and-Traits#ordering-your-own-type)) when field-wise is not the order it wants. Without either the call is E0007 naming the element type.

The declaration is the point rather than a formality: a value kind orders field-wise, in declaration order, so *swapping two fields* would silently re-sort every list of that type. `@derive(Comparable)` is where you say that the field order is the sort order — and the declared order is the one these three hand back, whichever way it was declared.

Inside a generic body the element type is whatever the caller chose, so the requirement becomes a **bound**: `fn top<T: Comparable>(xs: List<T>): ?T { return xs.max() }` compiles and instantiates at every ordered type. Without the bound the ordering is not promised and the call is E0025, naming the bound to add. The arithmetic and boolean reductions have no bound that promises a number or a `bool`, so they stay available only where the element type says so directly.

A **set** asks for less, and `to_set()` accordingly takes any value kind: a set sorts to get membership and de-duplication, not to answer a question about order, so no declaration is required to put a value in one.

## Map

String-keyed; keys iterate and print in sorted order. Empty is `{}`.

```noeta
host = "h"; scheme = "https"
mut m = {"a": 1, "b": 2}
echo m["a"]            // 1  (missing key is E0018)
m["c"] = 3            // sugar for  m = m.set("c", 3)
echo { host, scheme } // shorthand: { "host": host, "scheme": scheme }
```

| Method | Signature | Example → result |
|---|---|---|
| `keys` | `keys() -> List<K>` | `{"b":2,"a":1}.keys()` → `["a", "b"]` |
| `values` | `values() -> List<V>` | `{"b":2,"a":1}.values()` → `[1, 2]` |
| `has` | `has(key: K) -> bool` | `{"a":1}.has("a")` → `true` |
| `get` | `get(key: K) -> ?V` | `{"a":1}.get("a")` → `some(1)`, `.get("z")` → `none` — the Option read, for chaining with `??`/`match` |
| `get_or` | `get_or(key: K, default: V) -> V` | `{"a":1}.get_or("z", 0)` → `0` — one probe where `if m.has(k) then m[k] else d` costs two |
| `set` | `set(key: K, v: V) -> Map<K, V>` | new map with the entry added/updated |
| `remove` | `remove(key: K) -> Map<K, V>` | new map without the key |
| `len` | `len() -> int` | number of entries |
| `iter` | `iter() -> Iterator<V>` | iterates the values |

Iterating a map (`for v in m`) yields values in key order; equality is structural (order-independent).

A map's key type `K` must be immutable, totally ordered, and stably hashed. The rules:

- **Allowed**: `string`; `int` and any fixed-width integer (`Map<u8, V>` works) — the leanest kind (zero-allocation, one-word hash), iterating in numeric order; a **key-capable native type** (`Uuid`: `Map<Uuid, Order>` works end to end); a **key-capable `@packed` struct** — all fields integers/`bool` (or nested such structs), keyed by content.
- **Rejected statically**: mutable native types (`FileHandle`), `float`/`f32` (NaN makes float keys a footgun), `bool`, and any float-fielded struct.

Packed-struct keys are the spatial-hash idiom — see [Fixed-Width Ints & Packed Types](Fixed-Width-Integers#packed-value-types--packed) for the worked example and ordering details.

## Set

Sorted and de-duplicated; not indexable. Display form `{1, 2, 3}` (an empty set displays as `{}`); the empty-set literal is written `#{}`, since a bare `{}` is a map.

Elements are a single **orderable** type: a primitive, a key-capable native type, a key-capable `@packed` struct (ordered by content — type name, then field-wise), or any other **value kind** — structs and enums order structurally, so `[P {x: 2}, P {x: 1}].to_set()` canonicalizes like any primitive set whether or not `P` declares an ordering. That is the difference between a set and `sorted()`: the buffer a set keeps is how it gets membership and de-duplication, not an order the program asked for, so building one takes no `Comparable` — and it stays structural even for a type that declares its own `compare`, because a set places a value at one moment and looks for it at another ([Ordering your own type](Generics-and-Traits#ordering-your-own-type)). A `class` element is rejected (statically at a `Set<T>` annotation): a set stores a sorted snapshot, and a reference could be mutated after insertion.

```noeta
s = #{3, 1, 2, 1}     // set literal (sugar for [...].to_set())
echo s                // {1, 2, 3}
echo s.contains(2)    // true
```

| Method | Signature | Example → result |
|---|---|---|
| `contains` | `contains(x: T) -> bool` | `#{1,2,3}.contains(2)` → `true` |
| `union` | `union(other: Set<T>) -> Set<T>` | `#{1,2}.union(#{3})` → `{1, 2, 3}` |
| `intersection` | `intersection(other: Set<T>) -> Set<T>` | `#{1,2,3}.intersection(#{2,3,4})` → `{2, 3}` |
| `add` | `add(x: T) -> Set<T>` | `#{1,2}.add(5)` → `{1, 2, 5}` |
| `remove` | `remove(x: T) -> Set<T>` | new set without the element |
| `len` | `len() -> int` | `#{7,7,9}.len()` → `2` |
| `iter` | `iter() -> Iterator<T>` | sorted iteration |

## `bytes`

A binary buffer — what `string.to_bytes()`, a packed list's `.to_bytes()`, and a `crypto` digest return. It compares by content, and `type_of(b)` is `Type.Bytes`.

```noeta
b = "hé".to_bytes()
echo b[0]                   // 104  (one byte, as an int in 0..=255)
echo b[1]                   // 195  (a high byte reads UNSIGNED, never negative)
echo b.len()                // 3    (UTF-8 bytes, not characters)
echo b.slice(1).to_hex()    // c3a9 (half-open; end defaults to the length)
```

| Method | Signature | Example → result |
|---|---|---|
| `len` | `len() -> int` | `"hé".to_bytes().len()` → `3` (UTF-8 bytes, not chars) |
| `to_hex` | `to_hex() -> string` | `"hé".to_bytes().to_hex()` → `68c3a9` (lowercase — the usual way to display a digest) |
| `decode` | `decode() -> ?string` | `"hé".to_bytes().decode()` → `some(hé)`; `none` when the bytes are not valid UTF-8 (the inverse of `string.to_bytes()`) |
| `slice` | `slice(start: int, end?: int) -> bytes` | `"hé".to_bytes().slice(1)` → the last two bytes (half-open; end defaults to the length; out of bounds is E0016) |

Index `b[i]` returns the i-th byte as an `int` in `0..=255` — bytes are **unsigned**, so `0xff` reads as `255` and never as `-1` — and an out-of-range index is E0016, exactly as for a string or a list. `bytes` is a value like every other collection: there is no `b[i] = x`, so build a buffer with `List<u8>.to_bytes()` (or `string.to_bytes()`) rather than mutating one.

For round-tripping packed numeric data through `bytes` (`xs.to_bytes()` / `from_bytes::<T>(...)`), see [Fixed-Width Ints & Packed Types](Fixed-Width-Integers#bytes--serialize-a-packed-list). For the base64 envelope over `bytes`, see [`std.base64`](std-base64).

## Option and Result

Full treatment in [Error Handling](Error-Handling). In brief:

- `Option` (`?T`): `some(x)` / `none`; unwrap-or-default with `??`; `.first()`/`.last()`/`.next()`/`.recv()` return options.
- `Result<T, E>`: `Ok(x)` / `Err(e)` (and `Ok()` for `Result<void, E>`); propagate with `?`.

The constructors — and `panic` — are ordinary values you can pass around: `results.map(Ok)`, `xs.map(some)`, and `handler = panic` all work, and behave exactly as a direct call would (same arity rules, same error text). Where the context pins a type, they take it precisely — `ints.map(Ok)` is `List<Result<int, ?>>`; bound bare, they stay dynamic until used. `assert` is the exception: a special form, not a value.

## Iterators

`iter()` on any list, set, or map produces a lazy `Iterator<T>` (a reference value — aliases share the cursor). Adapters fuse; a `for` loop drives an iterator directly.

| Method | Signature | Example → result |
|---|---|---|
| `next` | `next() -> ?T` | drains one element; `none` at the end |
| `collect` | `collect() -> List<T>` | `[10,20].iter().collect()` → `[10, 20]` |
| `map` | `map(f: Fn(T) -> R) -> Iterator<R>` | lazy transform |
| `filter` | `filter(f: Fn(T) -> bool) -> Iterator<T>` | lazy keep-if |
| `take` | `take(n: int) -> Iterator<T>` | first `n` (clamps) |
| `drop` | `drop(n: int) -> Iterator<T>` | skip `n` |
| `chain` | `chain(other: Iterator<T>) -> Iterator<T>` | concatenate |
| `enumerate` | `enumerate() -> Iterator<(int, T)>` | index each element |
| `zip` | `zip(other: Iterator<B>) -> Iterator<(T, B)>` | pair up (stops at the shorter) |
| `count` | `count() -> int` | drain and count the elements |
| `sum` / `product` | `sum() -> int \| float` | drain and fold |
| `checked_sum` | `checked_sum() -> ?int` | `none` on overflow |
| `min` / `max` | `min() -> ?T` | drain and take the extremum; `none` if already drained |
| `last` | `last() -> ?T` | drain and take the final element |
| `to_set` | `to_set() -> Set<T>` | drain into a set |
| `join` | `join(sep?: string) -> string` | drain and join the display forms |
| `any` / `all` | `any() -> bool` | `T = bool`; **stops** at the first `true` / first `false` |
| `count_true` | `count_true() -> int` | `T = bool`; the `true`s, as on a list |
| `contains` | `contains(x: T) -> bool` | **stops** at the first match |

```noeta
echo [1,2,3,4,5].iter().map(fn(n) => n * 10).take(3).collect()   // [10, 20, 30]
echo [1,2,3].iter().zip(["a","b","c"].iter()).collect()          // [(1, "a"), (2, "b"), (3, "c")]
echo [3,1,5,2].iter().take(2).min()                              // some(1)
```

Everything from `count` down is a **terminal**: it drains the iterator and hands back a plain value. Each answers exactly what its eager `List` twin answers over the same elements — `xs.iter().take(k).min()` and `xs.iter().take(k).collect().min()` are the same value — and `min`/`max` ask the element type for the same declared ordering the list's do.

`any`, `all` and `contains` **short-circuit**: each is settled by a single element and stops there, leaving the iterator on the element after the one that decided it. That is the reason to reach for them over `.collect()` plus the eager method, which builds a tail nobody reads.

`count()` is the number of elements left — the question a list answers with `len()`. The `true`s are `count_true()` on both surfaces.

Generators (`yield`) produce iterators too — see [Concurrency](Concurrency#generators--yield).

## Integer bit-methods

Every integer (and fixed-width integer) carries bit-manipulation methods and total conversions — `count_ones()`, `rotate_left(n)`, `to_u8()`, and more. The `to_*` conversions also bridge to the float domain (`to_float`/`to_f32`) and back (`float.to_int()`), and each integer destination has a range-checked twin (`checked_to_u8(): ?u8`, `none` when the value does not fit). See [Fixed-Width Ints & Packed Types](Fixed-Width-Integers#bit-intrinsics-and-conversions).

## Diagnostic codes you'll see

`E0007` type/arity mismatch · `E0016` index/slice out of bounds · `E0018` map key not found · `E0010` `panic` / deadlock · `E0021` IO error. The full catalog is [Diagnostics](Diagnostics).
