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

Pure scalar math. The real-valued functions (`sqrt`/`pow`, trig and inverse trig, logarithms/`exp`, hyperbolics, `hypot`, `pi`/`e`) always return `float`; `floor`/`ceil`/`round` return `int`; `abs`/`min`/`max` preserve kind (int in → int out; a mixed pair promotes to float). Out-of-domain inputs (`ln(-1.0)`, `asin(2.0)`) yield `NaN`, like `sqrt(-1.0)`.

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
| `asin` / `acos` / `atan` | `(x: float) -> float` | result in radians; `math.acos(1.0)` → `0.0` |
| `atan2` | `atan2(y: float, x: float) -> float` | quadrant-aware angle; `math.atan2(1.0, 1.0)` → `0.785…` |
| `ln` | `ln(x: float) -> float` | natural log; `math.ln(math.e())` → `1.0` |
| `log` | `log(x: float, base: float) -> float` | `math.log(8.0, 2.0)` → `3.0` |
| `log2` / `log10` | `(x: float) -> float` | `math.log10(1000.0)` → `3.0` |
| `exp` | `exp(x: float) -> float` | `math.exp(1.0)` → `2.71828…` |
| `hypot` | `hypot(a: float, b: float) -> float` | `math.hypot(3.0, 4.0)` → `5.0` |
| `sinh` / `cosh` / `tanh` | `(x: float) -> float` | `math.cosh(0.0)` → `1.0` |

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

## `datetime`

Calendar and timezone datetime — DST-correct arithmetic, the IANA timezone database, and RFC-3339 + strftime formatting (backed by [jiff](https://docs.rs/jiff)). This is the heavy calendar layer, distinct from the always-on lightweight `time` clock above; it lives behind the default-on `ring-datetime` footprint ring, so an AOT binary that never imports `datetime` sheds it and the tzdb.

Three value types:

- **`Instant`** — an absolute moment, timezone-independent. `datetime.now()` reads the host clock (real under `noeta run`, the fixed sandbox epoch in tests, so deterministic). `datetime.from_unix_ms(n)`, `datetime.parse(s)` (RFC-3339 → `?Instant`).
- **`Zoned`** — an `Instant` resolved into a named timezone: DST-aware civil fields and calendar arithmetic.
- **`Duration`** — a span, built with `datetime.seconds/minutes/hours/days/weeks/months/years(n)` and fed to `add`/`sub`.

| Method (receiver) | Signature | Notes |
|---|---|---|
| `Instant.unix_ms` | `() -> int` | Milliseconds since the Unix epoch. |
| `Instant.format` | `(fmt: string) -> string` | strftime, in UTC. |
| `Instant.in_zone` | `(tz: string) -> Zoned` | Resolve into an IANA zone (`"America/New_York"`); unknown zone is E0021. |
| `Instant.add` / `sub` | `(d: Duration) -> Instant` | **Time units only** (seconds/minutes/hours) — a bare instant has no zone, so calendar units like days must go through `in_zone(...)`. |
| `Instant.diff` | `(other: Instant) -> Duration` | The span from `self` to `other` (positive when `other` is later). |
| `Instant.is_before` / `is_after` | `(other: Instant) -> bool` | Chronological comparison (`==` also works). |
| `Zoned.year`…`second` | `() -> int` | Civil fields in the zone. |
| `Zoned.weekday` | `() -> int` | ISO: 1 = Monday … 7 = Sunday. |
| `Zoned.zone` | `() -> string` | The IANA zone name. |
| `Zoned.format` | `(fmt: string) -> string` | strftime, in the zone. |
| `Zoned.to_instant` | `() -> Instant` | The underlying absolute moment. |
| `Zoned.add` / `sub` | `(d: Duration) -> Zoned` | DST-correct calendar arithmetic (all units). |
| `Zoned.is_before` / `is_after` | `(other: Zoned) -> bool` | Chronological comparison. |
| `Duration.to_string` | `() -> string` | ISO-8601 (`PT1H30M`, `P2D`). |

```noeta
use std.{datetime}
t = datetime.from_unix_ms(1720661640000)
ny = t.in_zone("America/New_York")
echo ny.format("%Y-%m-%d %H:%M %Z")        // 2024-07-10 21:34 EDT
echo ny.add(datetime.days(1)).weekday()    // 4  (Thursday)
```

## `env` and `args`

Host introspection. Under the sandbox the fixture is `HOME=/home/sandbox`, `USER=noeta`, args `["noeta", "run"]`; `noeta run` uses the real process environment.

| Function | Signature | Notes |
|---|---|---|
| `env.get` | `get(key: string) -> ?string` | `none` when unset — pair with `??` for a default: `env.get("PORT") ?? "8080"`. |
| `env.set` | `set(key: string, value: string) -> void` | Writes the **program's view** of the environment: reads observe it, `os.exec` children inherit it; the parent process is untouched. |
| `env.keys` | `keys() -> List<string>` | Sorted. |
| `env.parse` | `parse(s: string) -> Map<string, string>` | Parse `.env`-format text into a map (no environment mutation). |
| `env.load` | `load(path?: string) -> Map<string, string>` | Load a `.env` file (default `.env`), applying its entries as defaults under real-env-wins precedence. |
| `args.all` | `all() -> List<string>` | Process arguments. |

## `os`

Process execution and system introspection. Under the sandbox the introspection leaves are fixed fixtures (`platform`/`arch`/`hostname` = `"sandbox"`, 1 cpu, cwd `/`, pid 1) and `exec` interprets a tiny scripted command set (`echo` echoes its args; `status n msg` exits `n` with `msg` on stderr) so exec-driving programs stay deterministic; `noeta run` reports the real machine and runs real subprocesses (no shell — the command is executed directly with its argument vector, so there is no shell-injection surface and nothing to escape). If you deliberately want a shell (`os.exec("sh", ["-c", …])`), quote any interpolated input with `os.shell_quote` so it stays one literal token.

| Function | Signature | Notes |
|---|---|---|
| `platform` | `platform() -> string` | `"linux"`, `"macos"`, `"windows"`, … |
| `arch` | `arch() -> string` | `"x86_64"`, `"aarch64"`, … |
| `hostname` | `hostname() -> string` | |
| `cpus` | `cpus() -> int` | Logical CPUs, ≥ 1. |
| `cwd` | `cwd() -> string` | Current working directory. |
| `pid` | `pid() -> int` | Process id. |
| `exec` | `exec(command: string, args?: List<string>) -> ExecResult` | Runs and waits. A command that cannot start is E0021; one that runs and fails is an `ExecResult` with its non-zero status. |
| `exec_async` | `exec_async(command: string, args?: List<string>) -> Future<ExecResult>` | The async twin — the subprocess runs on the blocking pool. |
| `spawn` | `spawn(command: string, args?: List<string>) -> Process` | Starts a child **without waiting** and returns a controllable handle. A command that cannot start is E0021. |
| `exit` | `exit(code?: int) -> void` | Deliberate, clean termination: output so far is kept, nothing is reported, the run's exit code is `code` (default 0). |
| `shell_quote` | `shell_quote(s: string) -> string` | POSIX-shell-safe quoting for the explicit `sh -c` escape hatch (below). |

`ExecResult` (namespaced `std.os.ExecResult`) carries the captured outcome: `status() -> int`, `ok() -> bool` (status 0), `stdout() -> string`, `stderr() -> string`.

`Process` (namespaced `std.os.Process`) is a handle to a spawned child you control over its lifetime (unlike `exec`, which runs to completion). Its stdout/stderr are captured while it runs, so `wait` returns them in full.

| Method | Signature | Notes |
|---|---|---|
| `pid` | `pid() -> int` | The child's OS process id. |
| `wait` | `wait() -> ExecResult` | Blocks until the child exits; returns its status + captured output. Idempotent. |
| `wait_async` | `wait_async() -> Future<ExecResult>` | The awaitable twin of `wait`: an async context awaits the child's exit. In the sandbox it resolves deterministically; on the real host the wait runs on the blocking pool, genuinely overlapping the isolate's other tasks. |
| `try_wait` | `try_wait() -> ?ExecResult` | Non-blocking poll: `some(result)` if exited, `none` if still running. |
| `kill` | `kill() -> void` | Forcefully terminates the child (idempotent). A later `wait` sees the killed status. |
| `signal` | `signal(name: string) -> void` | Send a named OS signal to the child — the general form of `kill`. The name is case-insensitive and the `SIG` prefix is optional (`"TERM"`, `"sighup"`, `"KILL"`). Supported: `HUP`, `INT`, `QUIT`, `KILL`, `USR1`, `USR2`, `TERM`, `CONT`, `STOP`; an unknown name is E0021. Idempotent (signalling an exited child is a no-op). On non-Unix hosts only `KILL`/`TERM` are expressible. |
| `read_line` | `read_line() -> ?string` | Streams the child's stdout a line at a time **while it runs** (blocks until a line is ready), `none` at end of output. `wait` still returns the whole capture. |
| `read` | `read(n: int) -> ?string` | Up to `n` **characters** from stdout (POSIX-read shape: blocks only until at least one is ready), sharing the `read_line` cursor; `none` at EOF. |
| `read_err_line` | `read_err_line() -> ?string` | Streams **stderr** a line at a time on its own cursor. |
| `write` | `write(s: string) -> void` | Writes to the child's stdin. |
| `close_stdin` | `close_stdin() -> void` | Closes stdin, signalling EOF to the child (idempotent). |

```noeta
use std.{os}
r = os.exec("echo", ["hi"])
echo if r.ok() then r.stdout().trim() else r.stderr()

// A controllable child: start it, do other work, then collect its output.
p = os.spawn("echo", ["from the child"])
done = p.wait()
echo done.stdout().trim()
```

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
| `read_async` / `read_bytes_async` / `write_async` / `append_async` | the `Future`-returning variants (see [Concurrency](Concurrency)) |
| `exists_async` / `remove_async` / `list_async` / `is_dir_async` / `mkdir_async` | async metadata & directory twins — same semantics as their sync forms, awaited |

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
| `stringify` | `stringify(value: dyn) -> string` | Maps in sorted-key order, objects in declared field order; `none`/unit → `null`. The same engine as `@derive(Serialize<Json>)`'s `to_json()`. |
| `parse` | `parse(text: string) -> dyn` | Objects → maps, arrays → lists, `null` → unit. Malformed is E0007. |
| `parse::<T>` | `parse::<T>(text: string) -> T` | **Typed** decode into a real value; **aborts** on failure — the convenience form. |
| `try_parse::<T>` | `try_parse::<T>(text: string) -> Result<T, JsonError>` | The **recoverable** twin of `parse::<T>` — the exact same decode walk, failures as a catchable `JsonError`. |
| `decode_typed` | `decode_typed(name: string, text: string) -> Result<dyn, JsonError>` | Decode by **runtime** type name (router/DI-facing), backed by `@derive(Deserialize<Json>)` recipes. |

```noeta check
struct Point { x: int  y: int }

use std.{json}
echo json.stringify({"b": 2, "a": 1})               // {"a":1,"b":2}

v = json.parse("{\"name\":\"Niro\",\"age\":3}")
echo v["name"]                                       // Niro

p = json.parse::<Point>("{\"x\":1,\"y\":2}")         // a real Point (methods callable)

echo match json.try_parse::<Point>("{\"x\": 1}") {   // recoverable: a Result, not an abort
    Ok(q)  => "at ${q.x}",
    Err(e) => e.message(),                           // missing field `y` for `Point`
}
```

The typed forms support nested structs, `List<T>`, `Map`, and optional fields (an absent field becomes `none`). Numeric widening follows `int <: f32 <: float` (a JSON integer satisfies `float`, a fractional number does not satisfy `int`). `parse::<T>` and `try_parse::<T>` are a deliberate pairing over one decode walk: reach for `parse::<T>` when a bad document is a bug (config you ship), `try_parse::<T>` when it is input (a request body). With `parse::<T>` a shape/type mismatch is E0007 and a missing required field E0009 — messages carry the same path precision `JsonError` does.

**Decode-time validation.** If a decoded type (or any nested field type) implements [`Validate`](Validation), its `validate()` runs automatically on the freshly-built value, bottom-up — so a shape-correct document with a broken *invariant* (a negative price, a port out of range) is rejected at the boundary. `parse::<T>` aborts on a validation failure (E0007); `try_parse::<T>` and `decode_typed` thread it into `Result.Err(JsonError)` with the same `field[i]: <message>` path. See [Validation](Validation).

### `JsonError`

Every recoverable decode failure — from `try_parse::<T>` or `decode_typed` — is a `JsonError` (importable as `use std.json.JsonError`), the standard library's first [`Error`](Error-Handling#the-error-trait) implementor. It also implements `Display`, so `${e}` interpolates its message.

| Method | Returns | Notes |
|---|---|---|
| `message()` | `string` | The composed message — `items[2].price: expected float, found JSON string`. The `Error` trait's method; also its `Display`. |
| `kind()` | `string` | `"syntax"`, `"mismatch"`, `"missing_field"`, or `"unknown_type"`. |
| `path()` | `string` | The path from the document root (`items[2].price`); empty for document-level failures. |
| `line()` / `column()` | `?int` | 1-based source position for `"syntax"` failures; `none` otherwise. |

## `reactive`

Server-side reactivity ([Reactivity](Reactivity)): `signal(v: T) -> Signal<T>` (a mutable cell), `computed(fn() -> T) -> Computed<T>` (a lazy memoized derivation), `effect(fn) -> Effect` (a side effect that reruns when a signal it read changes). Registered through the native-extension registry like every other module — `use std.reactive.{signal, computed, effect}` (or the qualified `reactive.signal(0)`); see [Native Extensions](Native-Extensions) for the higher-order seam they dispatch through.

## `task`

The concurrency combinators ([Concurrency](Concurrency)): `sleep(ms) -> Future<void>`, `all(List<Future<T>>) -> List<T>`, `race(List<Future<T>>) -> T` (losers cancelled), `map_bounded(items, n, f) -> List<B>` (≤ n in flight). Named `task` — `async` is a keyword and cannot appear in a `use` path.

## `para.crdt`, `para.p2p`, `para.synced` — *(the non-default `para-p2p` package, not `std`)*

The local-first / peer-to-peer stack ([Local-First & P2P](Local-First-and-P2P)) is **not** part of `std` — it is the first-party but non-default **`para-p2p` package** under the `para` namespace (add it to `[dependencies]` and authorize it in `[trust] native`). `para.crdt` builds conflict-free replicated values (`gcounter`/`pncounter`/`gset`) that `.merge` to convergence; `para.p2p` publishes/receives messages over topics (`publish`, `receive(topic) -> Future<?bytes>`) and reports this node's stable identity (`identity() -> ?string`, the hex public key); `para.synced` fuses them with reactivity — `synced_signal(initial, topic)` where `initial: Mergeable` is a [reactive](Reactivity) signal holding a CRDT, converging over p2p (`.get`/`.merge`/`.sync`). Misuse maps onto E0007/E0025 as noted on that page.

## `cell`

A shared, mutable, identity-carrying box: `cell.new(v: T) -> Cell<T>` holds one value; `.get() -> T` reads it, `.set(v: T)` replaces it, `.update(fn(T) -> T) -> T` reads-modifies-writes and returns the new value. Copies of the handle alias the one box (reference semantics — the point of a cell), and equality is identity: two cells over equal values are still different cells.

```noeta
use std.{cell}
c = cell.new(1)
alias = c
c.set(2)
echo alias.get()                                     // 2
echo c.update(fn(n) => n * 10)                       // 20
```

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
use std.id.Uuid              // the type, to name it in `is Uuid` below
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

## `regex`

Regular expressions. Two value types: a compiled **`Pattern`**, and a **`Match`** describing one hit.

| Function | Signature | Notes |
|---|---|---|
| `compile` | `compile(pattern: string) -> Pattern` | Compile a pattern. **Errors** on an invalid pattern, carrying the engine's own caret-and-span diagnostic. |
| `escape` | `escape(text: string) -> string` | Escape every metacharacter, so the result matches `text` literally. |

`Pattern` methods:

| Method | Signature | Notes |
|---|---|---|
| `is_match` | `is_match(text: string) -> bool` | The cheapest question — no groups, no offsets, no allocation. |
| `find` | `find(text: string) -> Option<Match>` | The leftmost match, or `none`. No match is never an error. |
| `find_all` | `find_all(text: string) -> List<Match>` | Every non-overlapping match, left to right. |
| `replace`, `replace_all` | `replace(text: string, replacement: string) -> string` | `replacement` expands `$1` / `${name}`; write `$$` for a literal `$`. |
| `split` | `split(text: string) -> List<string>` | Adjacent and edge matches yield empty strings, matching `string.split`. |
| `source` | `source() -> string` | The pattern source this was compiled from. |

`Match` methods:

| Method | Signature | Notes |
|---|---|---|
| `text` | `text() -> string` | The matched text (group 0). |
| `start`, `end` | `start() -> int` | **Character** indices, not byte offsets — so they compose with `slice` and `char_at`. |
| `group` | `group(n: int) -> Option<string>` | Group `n`; 0 is the whole match. `none` if the group didn't participate, or `n` is out of range. |
| `named` | `named(name: string) -> Option<string>` | The group captured by `(?<name>…)`, or `none`. |
| `groups` | `groups() -> List<Option<string>>` | Groups 1..n in order. |

Write patterns as **raw single-quoted strings** (`'\d+'`): they don't interpolate, so `\d` and
`${…}` reach the engine untouched.

Compiling is deliberately explicit — there is no `regex.is_match(pattern, text)` free function.
Compile once, match many. For a genuine one-shot, chain it:

```noeta check
use std.{regex}
echo regex.compile('\d+').is_match("a1b")   // true
```

The chain costs eight characters over a free function, and buys a cost that stays visible: on 200k
matches of an email pattern, compiling once and matching in a loop takes 0.06s, while recompiling
per call takes 104s. A free function would make those two spellings identical. If you do want a
cache, build it yourself — a `Pattern` is immutable and equal-by-source, so it can key a `Map`.

```noeta
use std.{regex}

digits = regex.compile('\d+')
echo digits.is_match("a1b")            // true
echo digits.replace_all("a1b22", "#")  // a#b#
echo digits.split("a1b22c")            // ["a", "b", "c"]

date = regex.compile('(?<year>\d{4})-(?<month>\d{2})')
echo match date.find("shipped 2026-07") {
    some(m) => m.named("year"),
    none => none,
}                                      // some(2026)

// Offsets are character indices, so they slice correctly through multi-byte text.
subject = "héllo 42"
m = match digits.find(subject) { some(x) => x, none => panic("unreachable") }
echo subject.slice(m.start(), m.end()) == m.text()   // true
```

The engine is a finite-automata matcher with a **linear-time guarantee**: matching cannot blow up
on hostile input, so it is safe to point a pattern at a request body. The price is the two
constructs a backtracking engine gives you for free — **lookaround** (`(?=…)`, `(?<=…)`) and
**backreferences** (`\1`) — which this engine does not support and rejects at `compile` time. That
trade is deliberate: a pattern that fails to compile is a diagnostic you see immediately, whereas a
pattern that backtracks catastrophically is an outage you see in production. Most lookaround uses
rewrite as a capture group plus a `group(n)` read.

Everything here is pure — no host capability, so results are identical across backends, the
sandbox, and native builds. The engine rides the `ring-regex` ring: a program that never imports
`std.regex` links neither it nor its Unicode tables.

## `http.client` and `http.server`

HTTP is two modules: **`std.http.client`** (outbound requests) and **`std.http.server`** (an inbound
server). They are split so a program that only *serves* never links the client's reqwest/TLS stack,
and a program that only *calls out* never links the server — each `use` pulls exactly what it needs.

### Client: `http.client`

`use std.http.client` binds `client`. Each verb performs a request and returns a
`Result<Response, HttpError>`; the `*_async` twins return a `Future<Result<Response, HttpError>>`
for concurrent work.

| Function | Signature | Notes |
|---|---|---|
| `get` / `head` / `delete` | `get(url: string, headers?: Map<string, string>) -> Result<Response, HttpError>` | Bodyless verbs. |
| `post` / `put` | `post(url: string, body: string\|bytes, headers?: Map<string, string>) -> Result<Response, HttpError>` | Body-carrying. |
| `query` | `query(url: string, body: string\|bytes, headers?: Map<string, string>) -> Result<Response, HttpError>` | The HTTP QUERY method — a safe, idempotent request that carries a body (for complex reads a URL can't express). |
| `request` | `request(method: string, url: string, headers?: Map<string, string>) -> Result<Response, HttpError>` | Any other (bodyless) verb. |
| `*_async` | `get_async(url, headers?) -> Future<Result<Response, HttpError>>`, … | Async twin of every verb above; `.await?` yields the `Response`. |

Every verb takes an **optional** trailing `headers: Map<string, string>`.

#### Configured clients

The verbs above are the **one-shot** door. When you talk to the same API repeatedly, bind the
configuration once with `client.new(base_url?)` and spend it many times:

```noeta ignore
use std.http.client

gh = client.new("https://api.github.com")
    .header("accept", "application/vnd.github+json")
    .bearer(env.get("GITHUB_TOKEN") ?? "")
    .timeout(30_000)

repo = gh.get("/repos/nsrosenqvist/noeta")?
```

A `Client` carries a base URL, headers applied to every request, an auth scheme, and a per-request
deadline. Its verbs mirror the free functions exactly — same names, same optional trailing headers,
same `Result<Response, HttpError>` — differing only in that the first argument is a **path**
resolved against the base. An absolute target (one with a scheme) is used as-is, so following a
`next` link back through a based client works.

| Method | Returns | Notes |
|---|---|---|
| `header(name, value)` | `Client` | Applied to every request. |
| `bearer(token)` | `Client` | `Authorization: Bearer <token>`. |
| `basic(user, password)` | `Client` | HTTP Basic (RFC 7617). |
| `timeout(ms)` | `Client` | Per-request deadline; exceeding it is an `HttpError` with `kind() == "timeout"`. |
| `retry(max, base_ms?, on?)` | `Client` | Retry transient failures — see below. |
| `retry_non_idempotent()` | `Client` | Extend retries to POST. |
| `prepare(method, path, body?, headers?)` | `Request` | Build a request **without** performing it. |
| `send(req)` | `Result<Response, HttpError>` | Perform an already-built request. |
| `base_url()` | `string` | The configured base, or empty. |
| `get` / `head` / `delete` / `post` / `put` / `query` / `request` | `Result<Response, HttpError>` | As the free verbs, but path-relative. |

A `Client` is **immutable**: every configuration method returns a *new* client. So a derived client
can never disturb the one it came from, and sharing a configured client across a program is safe by
construction:

```noeta ignore
api = client.new("https://api.example.com").header("accept", "application/json")
tracing = api.header("x-trace", request_id)   // `api` is unchanged
```

Header precedence is **call over client** — a per-request header replaces the client's same-named
one (matched case-insensitively) rather than duplicating it, so a client-wide `accept` can be
overridden for exactly one call.

#### Retries

```noeta ignore
api = client.new("https://api.example.com").retry(3)              // defaults
api = client.new("https://api.example.com").retry(3, 500)          // 500ms first backoff
api = client.new("https://api.example.com").retry(3, 500, [429])   // only rate limits
```

`retry(max, base_ms?, on?)` retries two things: a **transient transport failure** (an `HttpError`
whose `retryable()` is true — `timeout`, `dns`, `connect`) and any **status** in `on`, which
defaults to `[429, 502, 503, 504]`. Backoff starts at `base_ms` (default 250) and doubles per
attempt, capped at 30s. A server's own `Retry-After` header wins over the computed backoff — it
knows its rate limit better than a curve does — but is capped too, so a broken `Retry-After:
86400` cannot park your program for a day.

Note what is *not* in the default status set: **500**. A generic server error is usually
deterministic, and hammering it helps nobody. Name it explicitly if your API means something
transient by it.

**POST is not retried by default.** Retrying a request that may already have been applied can
duplicate a side effect — a second charge, a second order — and a timeout is exactly the case where
the client cannot tell whether the server processed it. Everything RFC 7231 defines as idempotent
(GET, HEAD, PUT, DELETE, OPTIONS, TRACE) plus QUERY is retried freely; POST needs
`retry_non_idempotent()`, which you should reach for only when the endpoint is safe to repeat or
you send an idempotency key.

Retries sleep on the **Clock** capability, not the thread — so under the deterministic sandbox a
retrying program advances logical time instead of blocking, stays reproducible, and is covered by
the conformance differential like any other code.

#### Building and sending a request separately

`prepare` builds a `Request` without performing it; `send` performs one. Between the two you can
inspect or rewrite it with `Request`'s copy-modify builders:

```noeta ignore
req = api.prepare("get", "/users/1")
resp = api.send(req.with_header("x-trace", request_id))?
```

This pair exists because it is the seam a **middleware** layer bottoms out in. `std.http`
deliberately stops here: it never invokes user code. Middleware, mocking, and pagination live one
level up in the `para/api` package, where a chain is composed from ordinary Noeta closures under
ordinary garbage collection — rather than natively, which would mean holding user closures inside a
native value.

What std keeps is what needs the transport: configuration, retry, the error classification, and the
`Link` parsing primitive.

A worked example of the whole surface — configured client, `?` propagation, status-vs-error, typed
decoding, `Link` pagination by hand, and `prepare`/`send` — is `examples/http_client.noe`.

#### What is an error, and what isn't

The `Err` arm is a **transport** failure only — the request never produced a response. So `?` on a
request means exactly one thing: *the network broke*.

An HTTP error **status is not an error**. A `404` is an answer: it arrives as `Ok(Response)`, and
you check it with `ok()` or `status()`. This is deliberate — folding status into `Err` is the
mistake that makes `http_errors`-style flags necessary elsewhere. When you *do* want a non-2xx to
short-circuit, opt in per call with `error_for_status()`:

```noeta ignore
resp = client.get("https://api.example.com/users/1")?   // Err only if the network broke
strict = client.get("https://api.example.com/users/1")?.error_for_status()?   // Err on 4xx/5xx too
```

`Response` methods: `status() -> int`, `ok() -> bool` (2xx), `body() -> string`,
`body_bytes() -> bytes`, `header(name) -> string?` (case-insensitive), `url() -> string` (the final
URL after redirects), `links() -> Map<string, string>` (RFC 8288 `Link` relations),
`error_for_status() -> Result<Response, HttpError>`, and the typed decoder
`json::<T>() -> Result<T, JsonError>`.

#### Decoding the body

`resp.json::<T>()` decodes straight into your own type:

```noeta ignore
struct User { name: string  id: int }

user = gh.get("/users/1")?.json::<User>()?
echo user.name
```

It is **recoverable by construction** — a response body is remote input, so a server that changes
shape is a value you handle, not an abort. The error is the same path-precise `JsonError` the JSON
module produces (`method: expected int, found JSON string`). When you *do* want a malformed body to
be fatal, `json.parse::<T>(resp.body())` is the aborting spelling.

`HttpError` implements `Error` and `Display`, so it converts through `?` like any other error type.
Its methods: `message()`, `kind()`, `url()`, and `retryable()`. `kind()` is one of `"timeout"`,
`"dns"`, `"connect"`, `"tls"`, `"protocol"` (the response was unreadable), `"invalid_url"`, or
`"other"`; `retryable()` is true for the transient three (`timeout`/`dns`/`connect`) and false for
the rest — a TLS failure will not fix itself, and a `protocol` failure may already have been
applied server-side.

A request never yields `"status"`. That kind exists only for `error_for_status()`, and it is
deliberately distinct from `"protocol"`: a 404 is perfectly valid HTTP, so folding the two together
would make a "corrupt upstream" check fire on every opted-in 404.

```noeta check
use std.http.client
use std.json

struct User { name: string }
token = "s3cret"
payload = {"name": "Niro"}
filter = {"q": "cats"}

resp = client.get("https://api.example.com/users/1", {"authorization": "Bearer " ~ token})?
if resp.ok() {
    user = json.parse::<User>(resp.body())
    echo user.name
} else {
    echo "request failed: " ~ resp.status()
}

// POST a JSON body; QUERY for a body-carrying read.
client.post("https://api.example.com/users", json.stringify(payload), {"content-type": "application/json"})?
found = client.query("https://api.example.com/search", json.stringify(filter))?
```

Concurrent fan-out: `all` (from `std.task`) awaits a batch of futures together, returning their
results in input order (see [Concurrency](Concurrency)):

```noeta check
use std.http.client
use std.task.{all}
codes = all([
    client.get_async("https://a.example"),
    client.get_async("https://b.example"),
])
echo [codes[0]?.status(), codes[1]?.status()].join(",")
```

**Sandbox vs. real.** Under `noeta run` (and the REPL) requests hit the real network. Under the
deterministic sandbox (the conformance differential, tests) a built-in responder answers every
request purely from its shape — `…/status/{n}` returns status `n`, `…/echo` returns a JSON echo of
the request, `…/headers` echoes the request headers — so tests are reproducible without a live
server. A program that needs real data runs on the real host. (Examples above are `ignore`d in the
doc-test gate precisely because they would otherwise reach the network.)

### Server: `http.server`

`use std.http.server` binds `server`. `server.serve(port, handler)` binds a listener on `port` and
drives an inbound HTTP server: for each
connection it calls `handler(request)` and writes back the `Response` the handler returns. The
handler is a `(Request) -> Response`, **sync or async** — an `async` handler `await`s freely, and the
server dispatches connections **concurrently** (a slow handler yields while others make progress),
all on Noeta's own async runtime. A handler that errors becomes a `500`; the server keeps running.

| Type / function | Signature | Notes |
|---|---|---|
| `server.serve` | `serve(port: int, handler: (Request) -> Response, host?: string) -> void` | Binds `host:port` (`host` defaults to `0.0.0.0`); serves until the process stops (Ctrl-C drains gracefully). |
| `server.response` | `response(status: int, body?: string\|bytes, headers?: Map<string, string>) -> Response` | The reply builder a handler uses. |
| `Request` methods | `method() -> string`, `path() -> string`, `query(name) -> string?`, `header(name) -> string?` (case-insensitive), `body() -> string`, `body_bytes() -> bytes` | Read the inbound request. |
| `Response.with_header` | `with_header(name, value) -> Response` | Copy-modify — returns a new response (a `Response` is immutable). **Replaces** any existing header of that name. |
| `Response.headers_all` | `headers_all(name) -> List<string>` | Every value of a repeated header, in order. `header` sees only the first. |

#### Cookies

`server.cookie(name, value)` builds a `Cookie`; `Response.with_cookie` attaches it. The defaults are
the safe ones — `Path=/`, `HttpOnly`, `SameSite=Lax` — so the shortest spelling is also the one you
want for a session.

Two properties are worth knowing, because both are places cookie code usually goes wrong:

**Setting two cookies works.** `Set-Cookie` is the one header RFC 7230 exempts from comma-folding — a
cookie's `Expires` attribute contains a comma, so the fold would be ambiguous — which means two
cookies must be two headers. `with_header` could not express that, since it replaces.

`with_cookie` replaces per **cookie name**, not per header: two different cookies both survive, and
setting the same one twice does what you meant rather than emitting a duplicate the browser has to
break a tie on. So the one rule `with_X` sets `X` holds across the whole type, and the multi-header
shape stays an implementation detail.

There is deliberately **no generic multi-value write** to match `headers_all`'s multi-value read. The
asymmetry tracks who controls the bytes: a peer may repeat any header it likes and you must be able
to see all of them, whereas everything *you* emit can be comma-joined — except `Set-Cookie`, which
has its own door.

**An invalid cookie cannot be built.** `server.cookie` validates the name and value and *errors*
rather than escaping. A cookie value is derived from user input more reliably than any other header,
and a `\r\n` in an unchecked value lets the caller append headers of their choosing (response
splitting) while a stray `;` forges attributes the author never wrote. Because construction
validates, `to_header()` is total. Encode anything richer than an RFC 6265 token — arbitrary bytes,
UTF-8 text — before it goes in; base64url is the usual choice.

| Type / function | Signature | Notes |
|---|---|---|
| `server.cookie` | `cookie(name: string, value: string) -> Cookie` | Errors on a name or value outside RFC 6265. |
| `Response.with_cookie` | `with_cookie(cookie: Cookie) -> Response` | Sets a cookie. Replaces one of the same name; otherwise adds. |
| `Request.cookies` | `cookies() -> Map<string, string>` | Every cookie the client sent. Empty when the header is absent. |
| `Request.cookie` | `cookie(name) -> string?` | One cookie by name. Cookie names are case-**sensitive**, unlike header names. |
| `Cookie` builders | `with_value(v)`, `with_path(p)`, `with_domain(d)`, `with_max_age(secs: int)`, `with_http_only(b)`, `with_secure(b)`, `with_same_site("strict"\|"lax"\|"none")` | Copy-modify, like `Response.with_header`. |
| `Cookie.expired` | `expired() -> Cookie` | The deletion form: same name/path/domain, empty value, `Max-Age=0`. |
| `Cookie.to_header` | `to_header() -> string` | The raw header value. Prefer `Response.with_cookie`. |

`SameSite=None` implies `Secure` and sets it, and `with_secure(false)` on such a cookie is an error —
a browser discards the combination, which is the hardest cookie bug to diagnose because the response
looks correct on the wire.

Deleting a cookie means *overwriting* it with an expired one, and a browser only matches the
overwrite when `Path` and `Domain` match the original. That is why `expired()` is a method on the
cookie you set rather than a free `delete(name)` that could not know them:

```noeta ignore
sid = server.cookie("sid", token).with_path("/app").with_secure(true)
// …later, to log out — the path must match, so derive it from the original:
reply = server.response(303, "").with_cookie(sid.expired())
```

```noeta check
use std.http.server
use std.http.{Request, Response}

fn fetch(req: Request): Response {
    if req.path() == "/health" {
        return server.response(200, "ok")
    }
    return server.response(404, "not found")
}

server.serve(8080, fetch)
```

**`noeta serve`.** Rather than call `server.serve` yourself, run `noeta serve app.noe --port 8080`:
the file defines a top-level `fn fetch(req: Request): Response` (and `use std.http.server`), and the
command runs its top-level setup, then serves that handler until interrupted (Ctrl-C). It is the
ergonomic entry point over an explicit `server.serve(...)` call — the same mechanism underneath. `--host` sets the bind address and `--parallel N` serves across N worker isolates (multi-core); Ctrl-C drains in-flight requests gracefully.

**Routers and middleware are ordinary code.** Because a handler is a first-class `(Request) ->
Response`, a router is just a handler that dispatches on `req.path()`, and middleware is a function
`(Request) -> Response -> (Request) -> Response` that wraps one — no framework or runtime hook
needed; you compose them into the single handler you serve.

**Sandbox vs. real.** Under `noeta run` / `noeta serve` the server binds a real socket. Under the
deterministic sandbox (tests) `server.serve` instead drives a fixed, documented **request script**
through the handler and returns — so a served program is reproducible and terminates in-oracle,
the inbound mirror of the client's pure responder.

## `session`

`use std.session` binds `session`: signed, stateless sessions carried in a cookie. The state rides
on the request, not in the server.

That is not a stylistic choice. `noeta serve --parallel N` gives every worker its own host and its
own retained arena, so the obvious in-memory implementation — a `Cell<Map<…>>` the handler captures
— is correct at `--parallel 1` and **silently fragments** above it: a session written on worker 2 is
invisible to the others while requests bounce between them. The bug appears only under the flag you
reach for in production, and presents as random logouts. A signed cookie has no such failure mode,
and it needs no framework hook, so it composes with a bare `server.serve` handler and any router
built on it.

| Function | Signature | Notes |
|---|---|---|
| `session.keyring` | `keyring(secrets: List<string>) -> Keyring` | Signing keys, newest first. Each at least 16 bytes. |
| `session.open` | `open(req: Request, keys: Keyring) -> Session` | Never fails — absent, forged, and expired all give an empty session. |
| `session.attach` | `attach(resp: Response, s: Session, keys: Keyring, max_age: int, secure: bool) -> Response` | Writes the cookie back, but only if the session changed. |
| `session.encode` | `encode(data: Map<string,string>, keys: Keyring, max_age: int) -> string` | The raw codec, for a non-cookie carrier or a different name. |
| `session.decode` | `decode(token: string, keys: Keyring) -> Map<string,string>?` | Verify and decode, or none. |
| `Session` methods | `get(name) -> string?`, `set(name, value) -> Session`, `remove(name) -> Session`, `clear() -> Session`, `dirty() -> bool`, `data() -> Map<string,string>` | Copy-modify, like `Response` and `Cookie`. |

```noeta ignore
use std.http.server
use std.http.{Request, Response}
use std.{session, env}

keys = session.keyring([env.get("SESSION_SECRET")])

fn handle(req: Request) use (keys): Response {
    s = session.open(req, keys)
    if req.path() == "/login" {
        s = s.set("user", "42")
    }
    body = s.get("user") ?? "anonymous"
    return session.attach(server.response(200, body), s, keys, 86400, true)
}

server.serve(8080, handle)
```

**The token** is `base64url(payload) "." base64url(hmac_sha256(key, base64url(payload)))`, where the
payload is `{"d": {…}, "exp": <unix seconds>}`. Three properties are load-bearing:

- **The MAC is verified before the payload is parsed**, so attacker-controlled bytes never reach the
  JSON parser unauthenticated.
- **`exp` is mandatory.** With no store there is nothing to revoke against, so an unbounded token
  would be valid forever and a stolen one stolen for good. It is a parameter, not an option.
- **Signing uses the first key; verification accepts any.** Rotating a secret would otherwise log
  every user out at once — which is why, in practice, nobody rotates.

**A session is signed, not encrypted.** Anyone holding the cookie can read its contents; the
signature proves only that they did not change them. Never put a secret in a session — store an
identifier and look the rest up.

**`secure` has no default, deliberately.** Defaulting it on breaks every plain-http localhost server
with a cookie the browser silently refuses to store; defaulting it off ships session credentials over
cleartext. Both failures are quiet, so the choice is stated out loud at the call: `true` in
production, `false` only for local development.

**There is a 4096-byte ceiling**, and exceeding it is an error rather than a truncation — browsers
drop an oversized cookie silently. Hitting it is the signal to move to a server-side store keyed by
a small id, which is what `para/db` offers on top.

Removing a key that was not there, or clearing an already-empty session, leaves it **not** dirty — so
a speculative `remove` on every request does not re-emit the cookie and quietly extend its own
expiry. Clearing a non-empty session emits an *expired* cookie rather than a valid token for empty
data, which is the difference between logging out and appearing to.

### WebSockets

A handler upgrades a connection by returning `server.websocket(session)` — the signature stays
`(Request) -> Response` whether it serves bodies or sockets. The `session` is an
`async fn (Socket)` that becomes the connection's second life: it runs concurrently with the rest
of the server and the stream closes when it returns.

| Type / function | Signature | Notes |
|---|---|---|
| `server.websocket` | `websocket(session: (Socket) -> dyn) -> Response` | The connection-hijack response: 101 handshake, then `session(socket)` runs. |
| `Socket.send` | `send(text: string) -> void` | Write one text frame. |
| `Socket.recv` | `recv() -> Future<?string>` | Await the next text frame; `none` means the peer closed. |
| `Socket.close` | `close() -> void` | End the stream early. |

```noeta ignore
use std.http.server
use std.http.{Request, Response, Socket}

async fn session(sock: Socket): bool {
    mut going = true
    while going {
        msg = sock.recv().await
        if msg == none { going = false }
        else { sock.send("echo: ${msg ?? ""}") }
    }
    return true
}

fn fetch(req: Request): Response {
    if req.path() == "/ws" { return server.websocket(session) }
    return server.response(200, "ok")
}
```

Real hosts speak RFC 6455 (text frames, ping/pong, clean close). Under the sandbox an upgraded
session is driven by a fixed, documented client conversation and then a close — deterministic and
terminating, like the request script.

### LiveView — pushing reactive state to the browser

The server-side [view/diff protocol](Reactivity) composes with websockets into a LiveView: the
session exposes signals/computeds through a `view`, sends `snapshot()` on connect, applies client
events to the signals, and pushes `diff()` — a frame containing **only what changed**. The browser
half is a bundled ~50-line shim, available in-language:

| Function | Signature | Notes |
|---|---|---|
| `server.liveview_js` | `liveview_js() -> string` | The bundled browser client; serve it as `application/javascript`. |

The shim connects to `/ws` (override with `window.NOETA_LIVE_PATH`), renders every binding into
elements marked `data-live="name"` (strings verbatim, other values as JSON), sends
`{"type":"event","name":"…"}` when an element with `data-live-click="…"` is clicked, exposes
`window.noetaLive = { state, send }`, and reconnects on close (the server re-snapshots each
connect, so recovery is total-state). It is a patch-applier, not a component framework — the
server renders the page however it likes.

Under `noeta serve --watch`, hot-reload events ride the same socket: a landed swap pushes
`{"type":"reload"}` (the page reloads into the new code over the **preserved** signal state) and
a rejected edit pushes `{"type":"error",…}`, rendered as a full-screen diagnostics overlay —
see [The CLI](The-CLI) for the whole dev loop.

```noeta ignore
async fn session(sock: Socket): bool {
    v = view()
    v.expose("count", count)
    sock.send(v.snapshot())
    mut going = true
    while going {
        msg = sock.recv().await
        if msg == none { going = false }
        else {
            evt = json.parse(msg ?? "{}")
            if evt["name"] == "increment" { count.update(fn(n) { return n + 1 }) }
            patch = v.diff() ?? ""
            if patch != "" { sock.send(patch) }
        }
    }
    return true
}
```

The complete runnable app — page, `/live.js` route, event dispatch — is
`examples/liveview_counter.noe` (`noeta serve examples/liveview_counter.noe`, then open
`http://localhost:8080`). Run it with `--watch` and edits hot-swap while signal state survives.

## `tracing`

Production distributed tracing, emitted as OpenTelemetry ([Observability](Observability)). The
scoped `with_span(name, body)` is the primary API; `span(name) -> Span` (with `set_attribute` /
`add_event` / `record_error` / `end`) is the manual form; `current_context()` / `span_from(name,
traceparent)` bridge W3C context across boundaries Noeta doesn't own.

```noeta check
use std.{tracing}
tracing.with_span("checkout", fn(): void {
    span = tracing.span("charge")
    span.set_attribute("amount", 4200)
    // … work …
    span.end()
})
```

**Opt-in.** Nothing is emitted until `OTEL_EXPORTER_OTLP_ENDPOINT` is set — a program that never
configures a collector pays nothing. Once configured, server requests, async work, and channel /
isolate messages are **auto-instrumented** into connected traces with no code changes. `Span` is a
reserved type name (declaring your own is **E0049**); a non-scalar `set_attribute` value is a
compile error (**E0007**). Full surface, config, and design on [Observability](Observability).

## `log`

OpenTelemetry **log records** ([Observability](Observability)), auto-correlated to the active span
(trace + span id) — not `print`. `log.info` / `debug` / `warn` / `error` (and the generic
`log.log(severity, message)`); the `*_with(message, attrs)` forms attach a `Map<string,
string|int|float|bool>` of attributes.

```noeta check
use std.{log}
log.info("server started")
log.error_with("checkout failed", {"order": 42, "stage": "charge"})
```

Opt-in like tracing; a `log.info(...)` is free when no logs endpoint is configured. A log inside a
`with_span` (or a server request) carries that span's ids automatically.

## `metrics`

OpenTelemetry **metrics** ([Observability](Observability)) — aggregated host-side into time series.
`metrics.counter` / `up_down_counter` / `histogram` / `gauge` are get-or-create by name, returning a
`Counter` / `Histogram` / `Gauge` handle; counters record with `.add(n)`, histograms/gauges with
`.record(v)` (plus the `.add_with(n, attrs)` / `.record_with(v, attrs)` attributed forms).

```noeta check
use std.{metrics}
hits = metrics.counter("http.requests")
hits.add_with(1, {"route": "/orders", "status": 200})
```

`Counter`/`Histogram`/`Gauge` are namespaced extern types under `std.metrics` — `use`-imported (they
coexist with a user's own `Counter`), needed only when you annotate one. Server requests are
auto-instrumented with an `http.server.request.duration` histogram and an `http.server.active_requests`
counter. Keep attribute cardinality low — each distinct attribute set is a stored series.

## `vec` & `quat`

Scalar 3D vector and quaternion math over any struct with the right shape — a `Vec3` is any struct with three `f32` fields, a `Quat` any struct with four. Result-shape operations return the *same* struct type as the input.

```noeta check
use std.{vec}
a = V3 { x: 1.0f32, y: 2.0f32, z: 3.0f32 }
b = V3 { x: 4.0f32, y: 5.0f32, z: 6.0f32 }
echo vec.dot(a, b)          // 32.0
echo vec.cross(a, b)        // V3 { x: -3.0, y: 6.0, z: -3.0 }
```

`vec`: `add`, `sub`, `scale`, `dot`, `cross`, `length`, `normalize`, `distance`, `lerp`, `reflect`, `clamp`, `min`, `max`, `abs`. `quat`: `mul`, `conjugate`, `normalize`, `slerp`, `dot`, `length`, `rotate_vec3`.

For bulk work, a `List<Vec3>` is stored as a flat packed buffer, and the `*_all` family (`add_all`, `sub_all`, `scale_all`, `dot_all`, `length_all` — usable as `vec.dot_all(xs, ys)` or the method form `xs.dot_all(ys)`) reduces columnar batches fast. The performance story — flat layout unlocking autovectorization — is on [Performance Techniques](Performance-Techniques).

## See also

- [Standard Library](Standard-Library) — the always-available Ring 1 surface.
- [Concurrency](Concurrency) — `sleep`, futures, channels.
- [Reactivity](Reactivity) — `signal`/`computed`/`effect`.
- [Local-First & P2P](Local-First-and-P2P) — CRDTs, peer-to-peer messaging, synced signals.
- [Native Extensions](Native-Extensions) — how these modules are registered, and how you could add your own.
