# std.http — an HTTP client over the Network host capability

**Status: ARC COMPLETE (2026-07-07).** H0 `1940efb`, H1 `a29c77f`, H2 `d0169c1`, H3 `c681e8c`,
H4 `35f181d`, H5 `0b4f20d`, H6 docs+memory. Branch `http` (off local main `1c89bd1`). All four
design decisions shipped as recommended (sync+async, `Response` extern type, pure sandbox
responder, reqwest+rustls). Gates each slice: 73 suites + full conformance (differential + leak +
doc-samples) + fmt + clippy. Real-network paths covered by `#[ignore]` tests (sync + async).
Follow-ons in `plans/deferred.md` §HTTP.

The last major host capability. `std.crypto` proved the extern-type + registry seams carry a new
module with zero backend edits; this arc adds the seventh `Host` capability (**Network**), its
deterministic sandbox twin (the Vfs analog for the network), and a real async client — exercising
the `RealBody::Async` path the extern-types arc built but never drove with a real future.

## What's already in place (verified)

- **`ExternIo` seam** (`crates/noeta-stdlib/src/executor.rs`): a dispatch returns
  `NativeOut::Spawn(Box<dyn ExternIo>)`; `run_sync(host)` is the deterministic body (sandbox runs
  it at spawn), `run_real() -> Option<RealBody>` hands the real executor a `RealBody::Async(fut)`
  or `RealBody::Blocking(closure)`. `RealExecutor::spawn_ext` already spawns an `Async` future
  straight onto the per-isolate tokio runtime. **Nothing in the seam needs changing.**
- **`Host`** (`host.rs`): `FileSystem + Rng + Clock + Env + Entropy + Ids`, blanket-impl'd.
  Network becomes the 7th trait; the union and blanket impl grow by one bound.
- **`SandboxHost`** owns a `Vfs`; **`RealHost`** (`noeta-runtime`) does real disk on tokio and is
  CLI-only, never differential-tested. The network capability follows the identical split.

## The four decisions

### D1 — Surface: sync **and** async (recommended)
`fs` ships both `fs.read` (sync, `block_on`) and `fs.read_async` (Future). HTTP mirrors it:
`http.get(url)` / `http.post(url, body)` / … are sync and ergonomic (the 90% scripting case);
`http.get_async(url)` / … return `Future<Response>` for concurrent fan-out and are what finally
drives `RealBody::Async` with a real client. Alternative: sync-only first, async as a follow-on
— but async was the stated point of picking this arc up.

### D2 — `Response` as an extern type (recommended)
The natural next seam client (pure, host-free — like `Uuid`). Methods: `status() -> int`,
`ok() -> bool` (200–299), `body() -> string`, `body_bytes() -> bytes`, `header(name) -> string?`.
JSON decoding stays the existing `json.parse::<T>(resp.body())` — no new call-site-typed method
machinery. A `Request` type is **not** built: requests are plain arguments (method in the
function name, url + optional body + optional headers map), which covers essentially all calls.
Alternative: `Response` as a declared struct `{status, body, headers}` — more transparent, but
forces one body representation and needs native struct materialization.

### D3 — Deterministic sandbox network: a pure httpbin-style responder (recommended)
The Vfs analog, but a network has no program-visible "write" step, so a mutable store would need
a stub API leaking test machinery into the language. Instead **`SandboxNet` is a pure function of
the request** — deterministic by construction, no state, no fixture to maintain — honoring a small
documented control grammar so conformance can exercise every path and pin exact bytes:
- `…/status/{n}` → response with status `n`
- `…/echo` (or any path) → 200, body = a deterministic JSON of `{method, path, body, headers}`
- `…/headers` → 200, echoing request headers
Real URLs (`https://api.github.com/…`) under the sandbox hit this responder (which knows nothing
of them) — correct: the sandbox is a simulator; real data needs `noeta run`. Alternatives: a
fixed route fixture (like `env::sandbox_vars()`) or a Vfs-backed `http://file/<path>` (conflates
two capabilities).

### D4 — Real client: `reqwest` with `rustls-tls`, minimal features (recommended, flagging weight)
Async-first (drives `RealBody::Async` with a genuine future), ecosystem standard, and `rustls`
keeps it portable (no system OpenSSL). It is a **CLI-only** dependency (`noeta-runtime`), never in
the hot interpreter core — but the dep tree is real (hyper, http, tower). Alternative: `ureq`
(sync, light, rustls) — but then the real path is always `RealBody::Blocking` and the async future
seam never gets a real client, which defeats part of the point.

## Slice plan (pending sign-off — subject to the decisions above)

- **H0** — plan + workspace deps (`reqwest` in `noeta-runtime` only).
- **H1** — the `Network` capability **on both hosts at once** (forced: adding the 7th bound to the
  blanket-impl'd `Host` union means SandboxHost *and* RealHost must implement it in the same slice
  or nothing is a `Host`). Trait in `host.rs`; `SandboxNet` pure responder on `SandboxHost`; the
  real reqwest client on `RealHost` (its tokio runtime gains `enable_io`). A `NetRequest`/
  `NetResponse` plain-data type crosses the seam (like `ReadSource`). Unit-tested both sides, no
  language surface yet. **This is what keeps the REPL working** — the REPL runs on `RealHost`
  (repl-on-vm `bee8cd6`) via `VmSession`'s `Box<dyn Host>`, so a Network-capable RealHost gives the
  REPL real interactive HTTP for free; the session-differential's `SandboxHost` gets the
  deterministic responder.
- **H2** — `std.http` sync surface: `get`/`post`/`put`/`delete`/`head`/`request`, `Response`
  extern type + accessor methods, checker tables. Conformance pins the control-route responses in
  both backends (differential). Zero backend edits (registry + extern-type seam only).
- **H3** — async twins (`*_async -> Future<Response>`) via `HttpIo: ExternIo` with
  `RealBody::Async` (RealExecutor's runtime gains `enable_io`). Conformance: zero-edit async
  corpus; real-executor CLI test hits a real endpoint (network-gated / `#[ignore]` by default so
  CI stays hermetic).
- **H4** — **general optional-param support in the registry** (decided with the user: build it
  properly, not a per-function arity hack). Design:
  - A new `SigType::Optional(&SigType)` wrapper marks a **trailing-optional** param. Optionality
    lives *in the `params` array* (which already varies per function), NOT a new `ExtFn` field —
    so the 112 existing `ExtFn` literals are untouched; only functions that want optional params
    change. Convention (matching the language's own optional params + `check_args`'s
    leading-required model): once a param is `Optional`, every following one is too.
  - Checker: `sig_to_type(Optional(t)) = sig_to_type(t)` (the type used for assignability when the
    arg is present). A new `required` count = index of the first `Optional` param; the four
    `check_args` call sites (module + method, `module.func` and `x.method`) pass that instead of
    `params.len()`. `check_args` already accepts `required < params.len()` — no change there.
  - Backends: **none.** `call_native_module` marshals exactly the caller's args (verified — no
    padding), so a short call hands the dispatch fewer `NativeValue`s; the dispatch reads optional
    args with `args.get(i)` and supplies the default. Extern-type methods get optional params for
    free (same `ExtFn`).
  - Dispatch helper: `want_arity_range(func, args, min, max)` for functions with optional params.
  - Conformance: a focused test that a registry function with an optional param accepts both
    arities and rejects too-few/too-many (E0007), in both backends.
- **H5** — `std.http` request options *using* H4: an optional trailing `headers: Map<string,string>`
  on every verb (the `http` module gains `deep_marshal` to read the map). Plus the **`QUERY`**
  verb (RFC-draft HTTP QUERY — safe, idempotent, body-carrying): `http.query(url, body, headers?)`
  + `query_async`. Sandbox responder echoes the method as today, so QUERY and custom headers are
  differential-pinnable. Timeout deferred to a follow-on (recorded).
- **H6** — docs (Standard-Library-Modules `http` section, Native-Extensions network-capability +
  optional-param rows, Concurrency-Internals sandbox/real split), plan outcome, deferred entries,
  memory.

## Deliberate non-goals (recorded, not silently cut)
Server/listener (`http.serve`) — its own arc; WebSockets; HTTP/2 tuning; a cookie jar / redirect
policy beyond reqwest defaults; auth helpers (belong with the `crypto` HMAC-signature follow-on).

## Gates
Per slice: workspace tests + full conformance (differential + leak + doc-samples) green; commit
per green slice. The interpreter hot path is untouched (network is registry-dispatched, cold), so
no bench gate. The real-network CLI test is `#[ignore]` by default (hermetic CI); run explicitly.
