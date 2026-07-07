# http.serve + `noeta serve` — a bundled, concurrent HTTP server over the Network capability

**Status: PLANNED (decisions locked; building).** This is the arc the `std.http` *client*
explicitly deferred (`plans/http/README.md` non-goals; `plans/deferred.md` §HTTP → "`http.serve` …
its own arc"). It is the keystone that unblocks the reactivity transport layer (WS minimal-diff push
/ LiveView), HMR, and — further out — reactive persistence.

## The one idea that shapes everything: inversion of control

Every capability shipped so far is **program-initiated**. The program calls `http.get(url)` /
`fs.read(path)` / `sleep(ms)`; the Host answers — deterministically on `SandboxHost` (so the
differential holds), really on `RealHost`. The async model is the same shape: *the program spawns
work, the executor drives it, the program awaits the result.*

A **server inverts this**. The program registers a handler and then **cedes control**: the world (an
inbound TCP connection) *initiates*, and the program's handler *responds*. Three consequences:

1. **It blocks indefinitely** — a real accept loop never returns on its own; under the differential
   that would hang forever and never produce a `RunResult`.
2. **The handler is invoked from "outside"** — a function value the runtime calls once per request, a
   call *into* the interpreter driven *by* an inbound event, the reverse of every existing edge.
3. **It is long-lived**, not a request→response round trip that completes.

The client arc made determinism fall out of a **pure responder** (`SandboxNet` = a pure function of
the request). The server's determinism is the **exact inverse**: a **pure, finite, documented
sequence of synthetic inbound requests**. Under the sandbox, `http.serve` drives that scripted
sequence through the handler, emits a deterministic transcript of the responses (what the corpus
pins), and **terminates** — deterministic, in-oracle, no socket. Under the real host it binds a real
listener and blocks. *Determinism comes from making the input sequence pure, just as the client's
came from making the response pure.*

## Concurrency: our own two tiers, nothing new invented

Concurrent per-connection dispatch is **in scope**, and it maps exactly onto the primitives we
already ship — a server is close to the platonic workload the async + isolate model was built for.

- **Tier 1 — cooperative async within one isolate (the Node/Deno event-loop model).** The serve
  loop is `while let Some(conn) = accept().await { spawn handle(conn) }`. Each connection is a task
  the executor round-robins, so a slow handler *yields* at its `.await` points instead of blocking
  the accept loop or its siblings. This is our existing `async`/`await` + executor
  `spawn_ext`/poll machinery. Single-core, non-blocking. **Determinism holds for free**: the sandbox
  executor already schedules cooperatively in a fixed order and resolves work at spawn.
- **Tier 2 — multi-core via worker isolates (`noeta serve --parallel N`).** Multi-core, for us, *is*
  multi-isolate — we are shared-nothing (`isolate` + `Channel`), not shared-heap like Go's
  goroutines. `noeta serve --parallel N` spawns N worker isolates (each an OS thread with its own
  heap + `RealHost`/`RealExecutor`, each running the Tier-1 loop), all accepting the same port via
  **`SO_REUSEPORT`** (the kernel load-balances connections — what Deno/nginx do; no cross-isolate FD
  passing). This is the shipped `isolate` primitive, unchanged. Request handlers are naturally
  shared-nothing, so per-worker heaps fit rather than fight the model — this arc is the strongest
  validation of the isolate milestone. **Tier 2 is real-host-only, out-of-oracle** like
  `RealExecutor`, so it never touches the differential.

### The server-owned reaping scope (the one concurrency extension)

`spawn e` today registers into the current `concurrent { }` scope and **joins** — bounded. An accept
loop spawns handler tasks *unboundedly* for the life of the server, which no bounded join block
models. Rather than introduce raw detached spawning, the serve construct owns a **long-lived scope
that reaps**: it spawns a handler task per connection and **reaps completed tasks as it goes** (their
values/destructors released at completion, leak-free), staying inside the structured-concurrency
model. A handler task that **errors or panics** is caught by the scope → a **500 response**, and the
worker isolate **survives** (Go recovers per-goroutine; Deno isolates per-request). This
error-catching is also what lets framework-level error middleware work: it wraps and catches *above*
the loop's 500 fallback.

## The handler contract (must be right now — frameworks depend on it)

The contract is **`(Request) -> Response`, async-capable and error-catching** — deliberately minimal
and *composable*, so frameworks are pure library code over it (see "Frameworks" below):

- **Async**: the handler body may `.await` (yielding a `Future<Response>` the serve loop awaits).
  Middleware routinely awaits (auth hitting a DB, a proxy); sync-only would be a dead end. Cheap for
  us — it is the executor we already drive. A sync handler (`(Request) -> Response`) is accepted too
  (awaiting a ready value).
- **Error → 500**: a handler error/panic becomes a 500 via the reaping scope; the worker never tears
  down.
- **Rich enough `Request`/`Response`**: a framework must be able to read everything off a request
  (method/path/query/all headers/body) and freely build/modify a response — a first-class
  requirement, not a nicety (S2).

## What's already in place (verified — this arc reuses all of it)

- **`Network` Host capability** (`noeta-native/src/host.rs`): `net_fetch` + `net_spawn`,
  blanket-impl'd into `Host`. The server adds the **inbound** side to the same trait — a `net_listen`
  / `net_accept` (async leaf) / `net_reply` triple — so `SandboxHost` and `RealHost` each grow those
  methods, exactly as the outbound side was added in http H1.
- **`NetRequest` / `NetResponse` seam types** (`noeta-native/src/net.rs`): plain `Send` data already
  crossing the Network seam both ways. An inbound request *is* a `NetRequest`; a handler's reply *is*
  a `NetResponse` (already the `Response` extern type, `RESPONSE_TYPE_NAME`).
- **The async leaf + executor machinery** (`ExternIo` / `RealBody::Async` / `spawn_ext` / `poll_ext`):
  the accept future is just another `RealBody::Async` (a tokio `TcpListener::accept().await`),
  polled like `fs.read_async`.
- **The closure-call seam both backends expose** — the one **reactivity** drives (`effect`/`computed`
  bodies: eval `call`, VM `call_value` with `GcVal` RAII) and the **debugger** re-uses. Calling a
  handler per request is the same operation as running an effect body. No new call machinery.
- **Structured concurrency** (`concurrent`/`spawn`, cross-scope round-robin polling A.7) — the base
  the reaping scope extends. **Isolates** (`isolate f(args)`, `Rvalue::SpawnIsolate`, real OS threads)
  — the Tier-2 worker substrate.
- **`RealHost` + `RealExecutor`** (`noeta-runtime`): a per-isolate tokio `current_thread` runtime with
  `enable_all` (IO + time drivers already on). A `TcpListener` binds and accepts on it; `RealHost`
  already keeps per-connection state maps (`readers`) — the pattern for holding an open `TcpStream`.
- **Extern-type + registry seams**: `Request` (new, inbound) is another pure extern type like
  `Response`/`Uuid`; accessor methods dispatch through the registry with **zero backend edits**.
- **Function types** (`(Request) -> Response`, `Type::Fn`): the checker types a handler parameter and
  validates the call, so `http.serve(port, handler)` / the exported `fetch` type-check.

## Two entry points, one mechanism

- **`http.serve(port, handler)` — the mechanism.** Callable from any program; binds one listener in
  the calling isolate and runs the Tier-1 reaping loop (blocks until the listener closes). This is
  what the differential exercises via the sandbox request-script (in-oracle). A program wanting
  multi-core itself can spawn worker isolates around it, but that is what the command automates.
- **`noeta serve <file>` — the operational entry point.** Looks up a **conventional exported handler**
  (a top-level `fn fetch(req: Request) -> Response`, the web-standard `export default { fetch }`
  analog), then runs the mechanism in **N worker isolates** (Tier 2, `--parallel`, `SO_REUSEPORT`)
  with `--port`/`--host`, and graceful shutdown. Real-host-only, out-of-oracle. `noeta run` already
  builds `RealHost` + `RealExecutor`; `noeta serve` builds the worker pool on top.

## Frameworks compose at the value level — `noeta serve` has one seam

A framework needs **no runtime hook**: because the contract is a first-class composable function
value, routers and middleware are ordinary library code that *produce or wrap* a `(Request) ->
Response`.

- **A router *is* a handler** — a `(Request) -> Response` that branches on `req.method()`/`req.path()`
  and delegates to a registered sub-handler.
- **Middleware is a handler transformer** — `(Handler) -> Handler`, doing work before/after calling
  the next handler (the onion model).
- **The app is their composition**, collapsing to one handler the program exports:

```
use web.{Router, logging, cors}
let routes = Router{}.get("/users", list_users).post("/users", create_user)
fn fetch(req: Request) -> Response { logging(cors(routes.into_handler()))(req) }
```

The seam is **the single exported handler**. Everything sits relative to it:

- **Below the seam (the program): all domain composition** — routing, auth, request-context (a
  framework-defined `Context` wrapping the immutable `Request`), body parsing, error middleware — all
  in-language and **type-checked**. The command never sees a router; it invokes one handler.
- **Above the seam (the command wraps *ops* middleware):** cross-cutting operational concerns the
  user should not hand-wire — access logging, metrics, and **distributed tracing** — are injected by
  `noeta serve` *around* the exported handler. This is the natural home for the **native OTEL arc**
  (roadmap M2 observability split): `serve` wraps `fetch` in a tracing layer, tracing every request
  with zero user code. A good reason owning the command matters, and why the arcs sequence this way.

This arc **proves** composability with a conformance test (a tiny in-language router + one logging
middleware over the primitive, driven by the sandbox script) but **ships no framework** — a
first-party router/middleware library is a follow-on arc or a documented example.

## Slice plan

- **S0** — this plan; workspace wiring. No new *crate*, but the workspace `tokio` (pinned
  `["rt","fs","io-util","time"]`) gains the **`net`** feature so `RealHost` can use
  `tokio::net::TcpListener` (the runtime is already built with `enable_all`, so the IO driver is on;
  only the compile-time API feature was missing). Conformance directory
  `tests/conformance/http_server/`.
- **S1 — done (`<pending>`).** The **inbound `Network` methods on both hosts at once** (forced: the
  trait grows, so `SandboxHost` *and* `RealHost` must implement or nothing is a `Host`), with
  **accept as an async leaf**. `net_listen(addr)` → a listener id; `net_accept(listener)` → an
  `ExternIo` (default `AcceptIo` resolves through the sync `net_accept_next` at spawn; `RealHost`
  overrides it with a `TcpListener::accept().await` future); `net_reply(conn, NetResponse)` → an
  `ExternIo` (default `ReplyIo` via the sync `net_reply_now`; `RealHost` overrides with an async
  socket write). The accept outcome is `Option<Request>` — and the **`Request` extern *value type***
  (holding the `conn` id internally + the `NetRequest`) is defined here, since accept must produce
  it; its *language-surface accessor methods* are S2. Sandbox drives the fixed
  `net::sandbox_request_script()` then signals close, recording replies in a transcript; `RealHost`
  binds via `std::net` (runtime-free) and attaches the tokio listener lazily on the executor's
  runtime at first accept, parking accepted `TcpStream`s in a shared `conns` map keyed by `conn`
  (so all socket IO stays on the runtime that accepted it — the cross-runtime pitfall, resolved).
  Includes a minimal dependency-free HTTP/1.1 request parser + response writer on the real host.
  Unit-tested both sides (sandbox: the exact scripted sequence + transcript; real: a loopback round
  trip, `#[ignore]`). No language surface yet.
- **S2** — the **`Request` accessor methods** (`method`/`path`/`query`/`header`/`body`/
  `body_bytes`, registry, zero backend edits) and the **`http.response(status, body?, headers?)`
  builder** (+ a copy-modify `with_header` so middleware can augment a response). Conformance builds a
  `Request` from a fixture and a `Response`, reading both back, in both backends.
- **S3a — done (`<pending>`).** The **serve construct**, serial loop. `http.serve` is a `Builtin`
  (not a registry `ExtFn` — the handler is a closure the `NativeValue` seam can't carry, and the loop
  needs the executor + inbound Network capability), intercepted on the qualified `http.serve` call in
  each backend's `call_native_module` ahead of `http`'s registry functions. The checker types it as
  `-> Unit` in `module_return` (arguments not strictly validated against a signature, matching the
  `task`/`reactive` virtual-builtin precedent — strict `handler: (Request) -> Response` validation is
  a follow-on). Both backends run the identical `accept().await` → call handler → `net_reply` loop,
  serially (one connection to completion before the next), with a handler error caught → 500 (the
  server keeps running). Conformance `serve_routing.noe` drives the sandbox script through a routing
  handler; differential + leak (0) green in both backends. **S3b** adds concurrent in-flight dispatch.
- **S3b — done (`<pending>`).** The serve loop is now **concurrent**: a server-owned in-flight set
  the loop reaps. The accept future is polled *alongside* the handler futures each round (never
  drive-to-completion), so a slow async handler yields at its awaits while the next connection is
  accepted and other handlers advance — cooperative Tier-1 dispatch. Both backends poll in the
  identical order (accept, then in-flight by index), so the interleaving is deterministic and agrees.
  Conformance `serve_concurrent.noe`: an async handler that `sleep(5).await`s prints all five
  "handling" lines (every connection accepted + in flight) *before* any "done" — a serial server
  could not. Differential + leak 0 green in both backends.
- **S3 (original, superseded by S3a+S3b)** — the **serve construct** (the core green slice): grammar for `http.serve(port, handler)`
  (checker-recognized, validates `handler: (Request) -> Response`, sync or async), one shared IR
  lowering, `Op::Serve` in the VM. Both backends implement the **Tier-1 async reaping loop** —
  `accept().await` → spawn a handler task into the server-owned scope → marshal `NetRequest ↔ Request`
  / `Response ↔ NetResponse` via the closure-call seam → `net_reply` — reaping completed tasks and
  catching a failed handler → 500 (worker survives). Conformance: a handler (a small router branching
  on `path()`, echoing `body()`, setting a status) run against the sandbox script, differential-pinned
  by the response transcript, in both backends.
- **S4 — done (lean, `<pending>`).** The **`noeta serve <file> [--port N]`** command (CLI,
  real-host-only). Convention: the file defines a top-level `fn fetch(req: Request): Response` (sync
  or async) + `use std.{http}`; the command loads it, synthesizes a trailing `http.serve(<port>,
  fetch)` statement, and runs it on the real host — so the mechanism is the *exact same* `http.serve`
  a program can call directly (a program calling `http.serve(...)` under `noeta run` already serves;
  the command only supplies the entry convention + the port). Single worker, cooperatively concurrent.
  `#[ignore]` integration test (`crates/noeta-cli/tests/serve.rs`) spawns the CLI, drives a real
  loopback request, asserts the routed response. **Deliberately deferred (with the extension-command
  follow-on, `plans/deferred.md`):** `--host`, graceful drain-on-SIGINT (Ctrl-C hard-stops for now),
  and **multi-core**. Multi-core does **not** need `socket2`/`SO_REUSEPORT` — the isolate-native path
  is an **acceptor isolate + fd-over-`Channel<int>` to worker isolates** (intra-process fds are shared
  across threads, so a plain int crosses the existing copy-at-boundary channel; `SO_REUSEPORT` is only
  an alternative that *would* need the dep). `noeta serve` itself is slated to become an
  extension-provided command (the higher-order-ABI follow-on), so it is kept lean rather than
  gold-plated in core.
- **S5 — done (`<pending>`).** The **composability proof** (`serve_composition.noe`): a router and a
  logging middleware built as ordinary in-language code over `http.serve` — no framework, no runtime
  hook. Middleware is a handler-transformer (closes over `next`); the router is a handler that
  dispatches to sub-handlers; `app = logging(route)` composes to the one `(Request) -> Response` the
  server runs. Handlers are annotated closures typed `(Request) -> Response`. Differential + leak 0
  (the closure captures reclaim cleanly) green in both backends. **Finding (orthogonal, recorded in
  `plans/deferred.md`):** a *bare top-level `fn` used as a value* is typed `fn() -> Response` — the
  checker drops its parameters in the function-handle path — so an annotated closure is used instead;
  a pre-existing function-handle typing gap, not introduced here.
- **S6** — docs (Standard-Library-Modules `http.serve` + `noeta serve`; Concurrency/Concurrency-
  Internals the two-tier model + reaping scope + sandbox-script/real-listener split; Native-Extensions
  inbound-Network rows), plan outcome, deferred entries, memory.

## Deliberate non-goals (recorded, not silently cut)

- **WebSockets / the reactivity transport (WS minimal-diff push / LiveView).** The clean follow-on:
  once request/response serving exists, a **connection-hijack** path (a `Response` upgrade variant —
  the contract is deliberately left open to it) + the reactive core's flush hook
  (`ReactiveGraph::for_each_value`, and the "which nodes recomputed this flush" knowledge the core
  already has) streams diffs to a client. Its own slice, as the client and reactivity arcs both
  deferred the transport.
- **First-party routing / middleware library** — proven composable here, shipped later (follow-on or
  example).
- **Streaming bodies (SSE, chunked upload/download)** — v1 buffers request and response; the hijack
  path above is the opening. Would reuse the P-LAZY `ReadSource` streaming model.
- **TLS termination** (`https://` inbound, rustls `TlsAcceptor`) — bundle behind config once a use
  case needs it; plain HTTP first.
- **HTTP/2, keep-alive tuning, static-file serving, per-request timeouts** — later ergonomic/perf
  follow-ons.

## Gates
Per slice: workspace tests + full conformance (differential + leak + doc-samples) green; commit per
green slice. The interpreter hot path is untouched (the serve loop is cold IO; Tier-2 is CLI-only).
The real-listener round-trip test is `#[ignore]` by default (hermetic CI); run explicitly.
