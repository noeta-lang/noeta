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

(The async `sleep(ms).await` used in [Concurrency](Concurrency) is the `use std.task` future form. Wall time exists only where it belongs: `id.uuid_v7()` reads it through the host seam — real under `noeta run`, fixed-epoch deterministic in tests.)

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
| `exists_async` / `remove_async` / `list_async` | async metadata twins — same semantics as their sync forms, awaited |

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

Identity generation — sequential ids and UUIDs.

| Function | Signature | Notes |
|---|---|---|
| `next_id` | `next_id() -> int` | A deterministic counter: 1, 2, 3, …. |
| `uuid` | `uuid() -> Uuid` | A UUID (version 4, random) — the default choice. |
| `uuid_v7` | `uuid_v7() -> Uuid` | A time-ordered UUID (version 7, unix-ms + random) — for ids that should sort by creation time. |
| `parse` | `parse(s: string) -> Uuid?` | Any RFC form; `none` on malformed input. |
| `uuid_v5` | `uuid_v5(ns: Uuid, name: string) -> Uuid` | A name-based UUID (version 5, SHA-1) — **pure**: same namespace + name gives the same UUID on any host, forever. |
| `namespace_dns` … | `namespace_dns() -> Uuid` | The RFC 9562 well-known namespaces: `namespace_dns`, `namespace_url`, `namespace_oid`, `namespace_x500`. Any `Uuid` (including a v5 result) can be a namespace. |

`Uuid` is a first-class value type: it compares by value, orders by its bytes (so v7 ids sort by creation time), and displays in the canonical hyphenated lowercase form. Instance methods: `to_string() -> string`, `version() -> int`, and `timestamp_ms() -> int?` — `some(ms)` exactly when the version carries a timestamp (v7), `none` otherwise.

```noeta
use std.{id}
echo id.next_id()            // 1
key = id.uuid()              // e.g. 4396d60d-bd85-47af-a98f-f1a0396ff552
ordered = id.uuid_v7()       // sorts by creation time
echo key.version()           // 4
echo ordered.version()       // 7
echo key is Uuid             // true
echo id.parse(key.to_string()) == some(key)   // true — canonical round-trip

// v5: deterministic, hierarchical naming — no entropy involved.
user_ns = id.uuid_v5(id.namespace_dns(), "example.com")
echo id.uuid_v5(user_ns, "alice")   // the same UUID on every machine, every run
```

UUIDs flow through the host seam: in the deterministic sandbox (tests, the differential oracle) they are exactly reproducible — drawn from an entropy stream **independent of `random`** (generating an id never perturbs a seeded sequence, and `random.seed` never rewinds ids), with v7 timestamps built on a fixed epoch plus the logical clock (so `time.sleep` advances them). Under `noeta run`, `uuid()` uses real OS entropy and `uuid_v7()` real wall time.

## `crypto`

Everyday cryptographic primitives: content digests, keyed digests (HMAC), password hashing
(bcrypt), and crypto-grade random bytes.

| Function | Signature | Notes |
|---|---|---|
| `sha256`, `sha512` | `sha256(data: string\|bytes) -> bytes` | Content digests. A string hashes as its UTF-8 bytes. |
| `sha1`, `md5` | `sha1(data: string\|bytes) -> bytes` | **Interop only** (legacy checksums, UUID v5) — not collision-resistant; don't build integrity on them. |
| `hmac_sha256`, `hmac_sha512` | `hmac_sha256(key: string\|bytes, data: string\|bytes) -> bytes` | Keyed digests (RFC 2104) — message authentication, API signatures. |
| `hmac_sha256_verify`, `hmac_sha512_verify` | `hmac_sha256_verify(key: string\|bytes, data: string\|bytes, tag: bytes) -> bool` | **Constant-time** tag verification — always use this, never `tag == …` (which short-circuits and leaks timing). A tampered, truncated, or wrong-key tag is `false`, never an error. |
| `constant_time_eq` | `constant_time_eq(a: string\|bytes, b: string\|bytes) -> bool` | Constant-time equality for other secrets (session tokens, API keys, stored digests). Unequal lengths are `false`. |
| `sha256_hasher`, `sha512_hasher` | `sha256_hasher() -> Hasher` | An incremental hasher for streaming input. |
| `bcrypt_hash` | `bcrypt_hash(password: string, cost: int) -> string` | Password hashing; the salt comes from host entropy. Cost is bcrypt's 4..=31 (12 is a sensible production default). Returns the self-describing `$2b$…` string. |
| `bcrypt_verify` | `bcrypt_verify(password: string, hash: string) -> bool` | `false` on a wrong password; an **error** on a string that isn't a bcrypt hash. Accepts hashes from any bcrypt implementation. |
| `random_bytes` | `random_bytes(n: int) -> bytes` | Crypto-grade random bytes (tokens, keys) from host entropy — distinct from `random`'s seeded stream, like `id.uuid`. |

Digests are `bytes` — composable (hash a hash, key an HMAC with a digest) — and render with
`bytes.to_hex()`. Comparing **content** digests with `==` is fine; comparing anything secret
(auth tags, tokens) must go through the constant-time functions above. `Hasher` is a first-class value type: `update(data: string|bytes)` absorbs
input (mutating the hasher in place, with reference semantics like a file handle), and
`digest() -> bytes` reads the current digest *without* consuming the state, so interim digests
keep flowing.

```noeta
use std.{crypto}
echo crypto.sha256("abc").to_hex()   // ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
echo crypto.hmac_sha256("key", "message").to_hex()   // 6e9ef29b75fffc5b7abae527d58fdadb2fe42e7219011976917343065f58ed4a

h = crypto.sha256_hasher()           // streaming: feed input as it arrives
h.update("ab")
h.update("c")
echo h.digest() == crypto.sha256("abc")   // true — incremental matches one-shot

hash = crypto.bcrypt_hash("hunter2", 4)   // cost 4 for demo speed; use ~12 in production
echo crypto.bcrypt_verify("hunter2", hash)   // true
echo crypto.bcrypt_verify("wr0ng", hash)     // false
```

Like `id`, the effectful inputs ride the host seam: the bcrypt salt and `random_bytes` draw from
the host's entropy capability — exactly reproducible in the deterministic sandbox, real OS
entropy under `noeta run`. The digest functions are pure: the same input gives the same digest
everywhere (the conformance suite pins the published NIST/RFC vectors).

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
