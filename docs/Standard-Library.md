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
| `sorted` | `sorted() -> List<T>` | `[3,1,2].sorted()` → `[1, 2, 3]` |
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

Elements are a single **orderable** type: a primitive, a key-capable native type, a key-capable `@packed` struct (ordered by content — type name, then field-wise), or any other **value kind** — structs and enums order structurally (the same ordering `@derive(Comparable)` and `.sorted()` use), so `[P {x: 2}, P {x: 1}].to_set()` canonicalizes like any primitive set. A `class` element is rejected (statically at a `Set<T>` annotation): a set stores a sorted snapshot, and a reference could be mutated after insertion.

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
| `count` | `count() -> int` | drain and count |
| `sum` | `sum() -> int \| float` | drain and total |

```noeta
echo [1,2,3,4,5].iter().map(fn(n) => n * 10).take(3).collect()   // [10, 20, 30]
echo [1,2,3].iter().zip(["a","b","c"].iter()).collect()          // [(1, "a"), (2, "b"), (3, "c")]
```

Generators (`yield`) produce iterators too — see [Concurrency](Concurrency#generators--yield).

## Integer bit-methods

Every integer (and fixed-width integer) carries bit-manipulation methods and total conversions — `count_ones()`, `rotate_left(n)`, `to_u8()`, and more. The `to_*` conversions also bridge to the float domain (`to_float`/`to_f32`) and back (`float.to_int()`). See [Fixed-Width Ints & Packed Types](Fixed-Width-Integers#bit-intrinsics-and-conversions).

## Diagnostic codes you'll see

`E0007` type/arity mismatch · `E0016` index/slice out of bounds · `E0018` map key not found · `E0010` `panic` / deadlock · `E0021` IO error. The full catalog is in the [reference appendix](Syntax-Basics).
