# Standard-Library Modules

The `std` namespace holds modules you import explicitly with `use std.{name}`. Unused modules tree-shake away. An unknown function on a module is E0005; misuse maps onto a diagnostic code as noted per module.

```noeta
use std.{math, json, fs}       // whole modules → qualified calls: math.sqrt(x)
use std.math.sqrt              // selective import → bare call: sqrt(x)
use std.math.{abs, max}        // selective, several members
```

A **selective import** (`use std.<module>.<member>`) binds the member under its bare name — braces are only a grouping delimiter for several names, never required for one. A selectively-imported function is a first-class value (`f = sqrt`). Both forms work for every module below.

The always-available Ring 1 surface (strings, collections, options) needs no import — see [Standard Library](Standard-Library).

## `math`

Pure scalar math. `sqrt`/`pow`/`sin`/`cos`/`tan`/`pi`/`e` always return `float`; `floor`/`ceil`/`round` return `int`; `abs`/`min`/`max` preserve kind (int in → int out; a mixed pair promotes to float).

| Function | Signature | Example → result |
|---|---|---|
| `pi` | `pi() -> float` | `math.pi()` → `3.14159…` |
| `e` | `e() -> float` | `math.e()` → `2.71828…` |
| `sqrt` | `sqrt(x: float) -> float` | `math.sqrt(16.0)` → `4.0` |
| `pow` | `pow(base: float, exp: float) -> float` | `math.pow(2, 10)` → `1024.0` |
| `abs` | `abs(x: number) -> number` | `math.abs(-7)` → `7` |
| `floor` | `floor(x: float) -> int` | `math.floor(2.9)` → `2` |
| `ceil` | `ceil(x: float) -> int` | `math.ceil(2.1)` → `3` |
| `round` | `round(x: float) -> int` | `math.round(2.5)` → `3` |
| `min` | `min(a: number, b: number) -> number` | `math.min(3, 8)` → `3` |
| `max` | `max(a: number, b: number) -> number` | `math.max(3, 3.5)` → `3.5` |
| `sin` / `cos` / `tan` | `(x: float) -> float` | radians; `math.cos(0)` → `1.0` |

## `random`

A deterministic seeded PRNG (SplitMix64). The default seed is fixed, so even un-seeded use is reproducible.

| Function | Signature | Notes |
|---|---|---|
| `seed` | `seed(n: int) -> void` | Re-seed (rewinds the stream). |
| `int` | `int(lo: int, hi: int) -> int` | Inclusive `[lo, hi]`; `lo > hi` is E0007. |
| `float` | `float() -> float` | In `[0, 1)`. |

```noeta
use std.{random}
random.seed(42)
echo random.int(1, 6)     // reproducible roll
```

## `time`

A logical monotonic clock — no wall-clock, so programs stay deterministic.

| Function | Signature | Notes |
|---|---|---|
| `monotonic` | `monotonic() -> int` | Reads then advances one tick; first call → `0`. |
| `sleep` | `sleep(ms: int) -> void` | Advances the logical clock by `ms` without blocking. |

(The async `sleep(ms).await` used in [Concurrency](Concurrency) is the `use std.task` future form.)

## `env` and `args`

Host introspection. Under the sandbox the fixture is `HOME=/home/sandbox`, `USER=noeta`, args `["noeta", "run"]`; `noeta run` uses the real process environment.

| Function | Signature | Notes |
|---|---|---|
| `env.get` | `get(key: string) -> string` | Missing key is E0021. |
| `env.keys` | `keys() -> List<string>` | Sorted. |
| `args.all` | `all() -> List<string>` | Process arguments. |

## `fs`

File IO. Under `noeta run` this is real disk; the conformance sandbox uses an in-memory VFS. A missing file, a bad mode, or a non-UTF-8 read is E0021. Listings are sorted.

| Function | Signature |
|---|---|
| `write` | `write(path: string, content: string) -> void` |
| `append` | `append(path: string, content: string) -> void` |
| `read` | `read(path: string) -> string` |
| `read_lines` | `read_lines(path: string) -> List<string>` |
| `write_bytes` / `read_bytes` | `(path, data: bytes) -> void` / `(path) -> bytes` |
| `exists` | `exists(path: string) -> bool` |
| `remove` | `remove(path: string) -> bool` (true if it existed) |
| `is_dir` | `is_dir(path: string) -> bool` |
| `mkdir` | `mkdir(path: string) -> void` (creates ancestors, like `mkdir -p`) |
| `list` | `list() -> List<string>` / `list(dir: string) -> List<string>` |
| `open` | `open(path: string, mode: string) -> FileHandle` |
| `read_async` / `write_async` / `append_async` | the `Future`-returning variants (see [Concurrency](Concurrency)) |

### File handles

`fs.open(path, mode)` returns a `FileHandle` — a **reference** value with a live cursor. Modes: `"r"`/`"read"`, `"w"`/`"write"`, `"a"`/`"append"`. Write and append buffer until `close`; read streams a cursor. Wrong-mode, closed, or unknown-mode use is E0021.

| Method | Signature | Notes |
|---|---|---|
| `read_line` | `read_line() -> ?string` | Line without `\n`; `none` at EOF. |
| `read` | `read(n: int) -> ?string` | Up to `n` characters; `none` at EOF. |
| `write` | `write(chunk: string) -> void` | Write/append handles only. |
| `close` | `close() -> void` | Flushes a write/append buffer — you must close to persist. |

```noeta
use std.{fs}
out = fs.open("log.txt", "w")
out.write("alpha\n")
out.close()

reader = fs.open("log.txt", "r")
echo reader.read_line() ?? "<eof>"   // alpha
```

## `json`

| Function | Signature | Notes |
|---|---|---|
| `stringify` | `stringify(value: dyn) -> string` | Sorted object keys; `none`/unit → `null`. |
| `parse` | `parse(text: string) -> dyn` | Objects → maps, arrays → lists, `null` → unit. Malformed is E0007. |
| `parse::<T>` | `parse::<T>(text: string) -> T` | **Typed** decode into a real value. |

```noeta ignore
use std.{json}
echo json.stringify({"b": 2, "a": 1})               // {"a":1,"b":2}

v = json.parse("{\"name\":\"Niro\",\"age\":3}")
echo v["name"]                                       // Niro

p = json.parse::<Point>("{\"x\":1,\"y\":2}")         // a real Point (methods callable)
```

The typed form supports nested structs, `List<T>`, `Map`, and optional fields (an absent field becomes `none`). A shape/type mismatch is E0007; a missing required field is E0009. Numeric widening follows `int <: f32 <: float` (a JSON integer satisfies `float`, a fractional number does not satisfy `int`).

## `reactive`

Server-side reactivity ([Reactivity](Reactivity)): `signal(v: T) -> Signal<T>` (a mutable cell), `computed(fn() -> T) -> Computed<T>` (a lazy memoized derivation), `effect(fn) -> Effect` (a side effect that reruns when a signal it read changes). These are interpreter builtins behind an import gate — `use std.reactive.{signal, computed, effect}` (or the qualified `reactive.signal(0)`).

## `task`

The concurrency combinators ([Concurrency](Concurrency)): `sleep(ms) -> Future<void>`, `all(List<Future<T>>) -> List<T>`, `race(List<Future<T>>) -> T` (losers cancelled), `map_bounded(items, n, f) -> List<B>` (≤ n in flight). Named `task` — `async` is a keyword and cannot appear in a `use` path.

## `id`

`next_id() -> int` — the deterministic seeded counter (1, 2, 3, …), reproducible by design so tests never flake on identity. (UUIDs are a planned addition through the deterministic host seam.)

## `vec` & `quat`

Scalar 3D vector and quaternion math over any struct with the right shape — a `Vec3` is any struct with three `f32` fields, a `Quat` any struct with four. Result-shape operations return the *same* struct type as the input.

```noeta ignore
use std.{vec}
a = V3 { x: 1.0f32, y: 2.0f32, z: 3.0f32 }
b = V3 { x: 4.0f32, y: 5.0f32, z: 6.0f32 }
echo vec.dot(a, b)          // 32.0
echo vec.cross(a, b)        // V3 { x: -3.0, y: 6.0, z: -3.0 }
```

`vec`: `add`, `sub`, `scale`, `dot`, `cross`, `length`, `normalize`, `distance`, `lerp`, `reflect`, `clamp`, `min`, `max`, `abs`. `quat`: `mul`, `conjugate`, `normalize`, `slerp`, `dot`, `length`, `rotate_vec3`.

For bulk work, a `List<Vec3>` is stored as a flat packed buffer, and the `vec.soa*` family (`soa`, `soa_dot`, `soa_length`, …) reduces columnar batches fast. The performance story — flat layout unlocking autovectorization — is on [Performance Techniques](Performance-Techniques).

## See also

- [Standard Library](Standard-Library) — the always-available Ring 1 surface.
- [Concurrency](Concurrency) — `sleep`, futures, channels.
- [Native Extensions](Native-Extensions) — how these modules are registered, and how you could add your own.
