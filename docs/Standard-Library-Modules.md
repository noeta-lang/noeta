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
| `env.get` | `get(key: string) -> string` | Missing key is E0021. |
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
| `try_wait` | `try_wait() -> ?ExecResult` | Non-blocking poll: `some(result)` if exited, `none` if still running. |
| `kill` | `kill() -> void` | Forcefully terminates the child (idempotent). A later `wait` sees the killed status. |
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

Server-side reactivity ([Reactivity](Reactivity)): `signal(v: T) -> Signal<T>` (a mutable cell), `computed(fn() -> T) -> Computed<T>` (a lazy memoized derivation), `effect(fn) -> Effect` (a side effect that reruns when a signal it read changes). Registered through the native-extension registry like every other module — `use std.reactive.{signal, computed, effect}` (or the qualified `reactive.signal(0)`); see [Native Extensions](Native-Extensions) for the higher-order seam they dispatch through.

## `task`

The concurrency combinators ([Concurrency](Concurrency)): `sleep(ms) -> Future<void>`, `all(List<Future<T>>) -> List<T>`, `race(List<Future<T>>) -> T` (losers cancelled), `map_bounded(items, n, f) -> List<B>` (≤ n in flight). Named `task` — `async` is a keyword and cannot appear in a `use` path.

## `crdt`, `p2p`, `synced`

The local-first / peer-to-peer stack ([Local-First & P2P](Local-First-and-P2P)): `crdt` builds conflict-free replicated values (`gcounter`/`pncounter`/`gset`) that `.merge` to convergence; `p2p` publishes/receives messages over topics (`publish`, `receive(topic) -> Future<?bytes>`) and reports this node's stable identity (`identity() -> ?string`, the hex public key); `synced` fuses them with reactivity — `synced_signal(initial, topic)` where `initial: Mergeable` is a [reactive](Reactivity) signal holding a CRDT, converging over p2p (`.get`/`.merge`/`.sync`). Misuse maps onto E0007/E0025 as noted on that page.

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

## `http.client` and `http.server`

HTTP is two modules: **`std.http.client`** (outbound requests) and **`std.http.server`** (an inbound
server). They are split so a program that only *serves* never links the client's reqwest/TLS stack,
and a program that only *calls out* never links the server — each `use` pulls exactly what it needs.

### Client: `http.client`

`use std.http.client` binds `client`. Each verb performs a request and returns a `Response`; the
`*_async` twins return a `Future<Response>` for concurrent work.

| Function | Signature | Notes |
|---|---|---|
| `get` / `head` / `delete` | `get(url: string, headers?: Map<string, string>) -> Response` | Bodyless verbs. |
| `post` / `put` | `post(url: string, body: string\|bytes, headers?: Map<string, string>) -> Response` | Body-carrying. |
| `query` | `query(url: string, body: string\|bytes, headers?: Map<string, string>) -> Response` | The HTTP QUERY method — a safe, idempotent request that carries a body (for complex reads a URL can't express). |
| `request` | `request(method: string, url: string, headers?: Map<string, string>) -> Response` | Any other (bodyless) verb. |
| `*_async` | `get_async(url, headers?) -> Future<Response>`, … | Async twin of every verb above; `.await` yields the `Response`. |

Every verb takes an **optional** trailing `headers: Map<string, string>`. `Response` methods:
`status() -> int`, `ok() -> bool` (2xx), `body() -> string`, `body_bytes() -> bytes`, and
`header(name) -> string?` (case-insensitive).

```noeta ignore
use std.http.client
use std.json

resp = client.get("https://api.example.com/users/1", {"authorization": "Bearer " ~ token})
if resp.ok() {
    user = json.parse::<User>(resp.body())
    echo user.name
} else {
    echo "request failed: " ~ resp.status()
}

// POST a JSON body; QUERY for a body-carrying read.
client.post("https://api.example.com/users", json.stringify(payload), {"content-type": "application/json"})
found = client.query("https://api.example.com/search", json.stringify(filter))
```

Concurrent fan-out: `all` (from `std.task`) awaits a batch of futures together, returning their
results in input order (see [Concurrency](Concurrency)):

```noeta ignore
use std.http.client
use std.task.{all}
codes = all([
    client.get_async("https://a.example"),
    client.get_async("https://b.example"),
])
echo [codes[0].status(), codes[1].status()].join(",")
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
| `server.serve` | `serve(port: int, handler: (Request) -> Response) -> void` | Binds `0.0.0.0:port`; serves until the process stops. |
| `server.response` | `response(status: int, body?: string\|bytes, headers?: Map<string, string>) -> Response` | The reply builder a handler uses. |
| `Request` methods | `method() -> string`, `path() -> string`, `query(name) -> string?`, `header(name) -> string?` (case-insensitive), `body() -> string`, `body_bytes() -> bytes` | Read the inbound request. |
| `Response.with_header` | `with_header(name, value) -> Response` | Copy-modify — returns a new response (a `Response` is immutable). |

```noeta ignore
use std.http.server

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
ergonomic entry point over an explicit `server.serve(...)` call — the same mechanism underneath.

**Routers and middleware are ordinary code.** Because a handler is a first-class `(Request) ->
Response`, a router is just a handler that dispatches on `req.path()`, and middleware is a function
`(Request) -> Response -> (Request) -> Response` that wraps one — no framework or runtime hook
needed; you compose them into the single handler you serve.

**Sandbox vs. real.** Under `noeta run` / `noeta serve` the server binds a real socket. Under the
deterministic sandbox (tests) `server.serve` instead drives a fixed, documented **request script**
through the handler and returns — so a served program is reproducible and terminates in-oracle,
the inbound mirror of the client's pure responder.

## `tracing`

Production distributed tracing, emitted as OpenTelemetry ([Observability](Observability)). The
scoped `with_span(name, body)` is the primary API; `span(name) -> Span` (with `set_attribute` /
`add_event` / `record_error` / `end`) is the manual form; `current_context()` / `span_from(name,
traceparent)` bridge W3C context across boundaries Noeta doesn't own.

```noeta ignore
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

```noeta ignore
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

```noeta ignore
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

```noeta ignore
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
