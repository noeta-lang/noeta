# Edge Deployment

This page takes one `http.serve` program and puts it on an edge platform. [WebAssembly & the Edge](WebAssembly-and-the-Edge) covers how Noeta compiles to WebAssembly; **Fermyon Spin** is the target worked through here end to end, with **Fastly Compute** as a secondary target.

A complete, buildable version of everything below lives in [`examples/edge-hello/`](https://github.com/noeta-lang/noeta/tree/main/examples/edge-hello), as `hello.noe` plus `spin.toml` and `deploy.sh`.

## What the artifact is

Two build verbs emit WebAssembly, and they emit different things. Edge deployment uses the second one.

| Verb | Emits | WASI level | Entry shape | Where it runs |
|---|---|---|---|---|
| `noeta build --wasm app.noe` | a **module** (`app.wasm`) | `wasm32-wasip1` (preview 1) | a **CLI `_start`**, run once, exit | `wasmtime run app.wasm`: CLI tools, CI sandboxes, batch compute. It has no sockets and no inbound HTTP. |
| `noeta build --serve app.noe` | a **component** (`app.serve.wasm`) | `wasm32-wasip2` (preview 2) | a **`wasi:http/incoming-handler`**, invoked per request | any host that speaks the `wasi:http` proxy world: `wasmtime serve`, **Fermyon Spin**, Spin-class clouds. |

`--serve` produces a `wasi:http/incoming-handler` component that exports the `wasi:http/proxy` world, the interface Spin and `wasmtime serve` consume. It is neither a wrapped CLI module nor a vendor-specific format, which is why the same artifact runs across `wasi:http` hosts without re-targeting.

Both verbs are **binary surgery rather than compilation**: your program's compiled bundle is stapled into a prebuilt engine (`noeta_bundle::staple_wasm`), so a build takes milliseconds and needs no Rust toolchain, provided the prebuilt engine is where the build can find it. [What the deploy asks of you](#what-the-deploy-asks-of-you) covers that.

## The program

A handler is a `(Request) -> Response`. Nothing about it is edge-specific, and this is the same program `noeta run` runs on your laptop against a real socket:

```noeta check
use std.http.server
use std.http.{Request, Response}

fn handle(req: Request): Response {
    if req.path() == "/health" {
        return server.response(200, "ok")
    }
    return server.response(200, "hello from the edge: ${req.path()}")
}

server.serve(8080, handle)
```

The `port` argument to `server.serve` is inert at the edge, since the platform owns the socket and invokes your component per request. It is what `noeta run app.noe` binds locally, so the one program serves both ways.

`noeta serve` is a different verb from the `server.serve` call above. It is a convenience that expects a top-level `fn fetch(req: Request): Response` and drives its own `--port`. A program that calls `server.serve` itself, like this one, runs under `noeta run`.

Routing (`req.path()`), reading the request (`req.method()`, `req.header(name)`, `req.body()`), and building the reply (`server.response(status, body, headers?)`, `Response.with_header`) are ordinary code, with no framework or runtime hook to learn. See [std.http](std-http) for the full API.

## Building the artifact

```sh
noeta build --serve hello.noe                   # → hello.serve.wasm
noeta build --serve hello.noe -o out/app.wasm   # explicit output path
```

The output is the component you deploy, and the build reports its size. Most of that size is the embedded runtime rather than your program.

Smoke-test it under raw wasmtime before involving a platform:

```sh
wasmtime serve -S cli=y hello.serve.wasm    # serving on http://0.0.0.0:8080/
curl localhost:8080/health                  # ok
```

`-S cli=y` grants the standard-library imports the component uses beyond the bare proxy world. Spin grants them by default, so its manifest needs no equivalent.

## Primary target: Fermyon Spin

[Spin](https://spinframework.dev) runs `wasi:http` components directly. It embeds wasmtime, so there is no separate build step and no SDK. Deployment is a manifest plus the component.

### The manifest (`spin.toml`)

```toml ignore
spin_manifest_version = 2

[application]
name = "edge-hello"
version = "0.1.0"

[[trigger.http]]
route = "/..."
component = "edge-hello"

[component.edge-hello]
source = "hello.serve.wasm"
allowed_outbound_hosts = []
```

- **Routing.** `route = "/..."` is a wildcard: every path reaches the component, which routes internally on `req.path()`. For path-scoped components use a prefix like `route = "/api/..."`. Spin strips nothing, so your handler still sees the full path.
- **Environment.** Add `[component.edge-hello.environment]` with `KEY = "value"` pairs; the program reads them through `env.get("KEY")`. Writes (`env.set`) are a per-request overlay and never escape the invocation.
- **Outbound allowlist.** Edge platforms deny outbound traffic by default. Every host your handler calls with `client.get(...)`, or any other `std.http` verb, must be listed in `allowed_outbound_hosts` such as `["https://api.example.com"]`; an unlisted host errors. Outbound calls travel the platform's own `wasi:http/outgoing-handler`, so connection pooling and TLS belong to the platform. **Redirect targets count**: a request that follows a `301` to another host is a call to that host, so list it too, or set `client.redirect(0)` and handle the `Location` yourself.
- **Limits.** Per-request instantiation means there is **no cross-request state**: no in-process cache, no connection kept between requests. Persistent state belongs in the upstream services the handler calls. Execution and memory limits are the platform's to set, and Spin enforces a per-request time budget. A handler that never replies answers a diagnostic `500` carrying its stdout and traceback rather than hanging.

### Run it locally

```sh
spin up                     # serves on http://127.0.0.1:3000
curl localhost:3000/health  # ok
```

`spin up` needs no account and is the fastest way to confirm the component and manifest agree. Under Spin 4 the stapled `examples/edge-hello` component serves `/`, `/health`, an echo route, and a `/whoami` route that mints a v7 UUID and reads the wall clock, so real entropy and real time reach the handler unchanged.

### Deploy it

```sh
spin login       # one-time: authenticate to Fermyon Cloud (needs a free account)
spin deploy      # uploads the component and prints the public URL
```

`spin deploy` is the one step that needs an account, and it authenticates as you, so it stays a manual action. The [`deploy.sh`](https://github.com/noeta-lang/noeta/tree/main/examples/edge-hello/deploy.sh) in the example runs the Noeta build and then guards the deploy: a missing `spin` CLI or a logged-out session prints the exact command to fix it. Any other host that speaks the `wasi:http` proxy world can serve the same `hello.serve.wasm`, since the component is not Fermyon-specific.

## What the standard library gives a handler at the edge

The handler runs on `WasiHost` (`crates/noeta-wasi-host`), a `Host` implementation of its own alongside the deterministic sandbox and the CLI's real host. It is **real but synchronous**: the capabilities WASI exposes are real, and the ones it cannot expose are runtime errors rather than silent stubs.

| Capability | At the edge | Notes |
|---|---|---|
| **Inbound HTTP** | ✅ | The request that invoked the component; your handler's reply is the response. |
| **Outbound HTTP** (`std.http` client) | ✅ | Through `wasi:http/outgoing-handler`, **allowlisted** per component (see the manifest). |
| **Entropy / UUIDs** (`id.uuid`, `id.uuid_v7`, `random_bytes`) | ✅ | Real host entropy. |
| **Wall clock** (`datetime.now().unix_ms()`) | ✅ | Real time. |
| **Seeded PRNG** (`random.*`) | ✅ | A pure function of its seed, as on every host. Determinism rules are unchanged. |
| **Monotonic clock** | ✅ (logical) | An ordering device rather than wall time, as on the CLI's real host. |
| **Environment / args** (`env.get`, `args.all`) | ✅ | Env from the manifest; `env.set` is a per-request overlay. |
| **Filesystem** (`fs.*`) | ⚠️ scratch only | Reads and writes the preopened directories the platform grants, typically an ephemeral per-request scratch. No preopen gives ordinary IO errors. Not durable across requests. |
| **Subprocesses** (`os.exec`, `os.spawn`) | ❌ | A permanent WASI fact: WASI has no process model. |
| **Telemetry export** (OTLP) | ❌ | Spans still track for context propagation and parenting; the exporter needs its own host. |
| **p2p / local-first** (`para.p2p`) | ❌ | Unavailable on wasm targets. |
| **Isolates** | ⚠️ cooperative | Single-threaded scheduler: same semantics, no OS-thread parallelism. |

The operational consequence of the filesystem and cross-request rows is that **an edge handler is stateless by construction.** Anything that must persist lives in an upstream service you call over allowlisted outbound HTTP.

## Secondary target: Fastly Compute

Fastly Compute runs WebAssembly at the edge and has adopted `wasi:http`, so a `wasi:http/incoming-handler` component is the right shape to bring. `hello.serve.wasm` deploys with Fastly's own CLI:

```sh
fastly compute publish        # packages fastly.toml + the component, uploads to your service
```

with a `fastly.toml` naming the prebuilt component as the artifact.

Treat this path as **expected to work, pending confirmation**. Fastly's best-documented path is its own SDK targeting a Fastly-specific world (`fastly:api`) rather than the vendor-neutral `wasi:http` proxy world Noeta emits, and whether a given Fastly Compute service accepts the plain proxy-world component depends on the platform's current `wasi:http` support level. Spin is the target proven end to end. The mechanics are otherwise identical to Spin: build with `noeta build --serve`, allowlist outbound hosts in the platform manifest, and route on `req.path()`.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Every route answers `500` with "no program stapled in" | The component is the generic `noeta-wasm-serve` shell, not your app. | Rebuild with `noeta build --serve app.noe` and deploy that output, not a hand-copied component. |
| `500` with "the program produced no HTTP response" plus a traceback | Your program never called `server.serve`, or the handler aborted before replying. | The `500` body carries the stdout and diagnostics, so debug it exactly as locally. Confirm the top level calls `server.serve(port, handler)`. |
| Outbound `client.get(...)` errors at the edge but works under `noeta run` | The target host is not allowlisted. | Add it to `allowed_outbound_hosts` in `spin.toml`, or to Fastly's equivalent backend config. |
| `noeta build --serve` fails: "building the serve component failed (is the wasm32-wasip2 target installed?)" | No prebuilt component was found, so the build fell back to compiling it, and the target is missing. | `rustup target add wasm32-wasip2`, or point `NOETA_WASM_SERVE` at a prebuilt `noeta-wasm-serve.wasm`. See below. |
| `fs.*` calls error at the edge | No directory was preopened, or the write outlived the request. | Grant a preopen in the platform config for scratch IO, and never rely on the filesystem for cross-request persistence. |
| `os.exec` / `os.spawn` errors | WASI has no subprocesses. | Restructure to call a service instead. |
| Telemetry emits nothing | There is no OTLP exporter on wasm. | Spans still propagate context. Export from an upstream collector if you need it. |

## What the deploy asks of you

`--serve` needs a serve component to staple into, and resolves one through a ladder: the `NOETA_WASM_SERVE` environment variable, then a `noeta-wasm-serve.wasm` sitting next to the `noeta` binary, then an on-demand `cargo build` of the `noeta-wasm-serve` crate for `wasm32-wasip2`.

A developer checkout takes the third rung transparently and needs only `rustup target add wasm32-wasip2`. A binary-only toolchain has neither of the first two rungs in place, so give it one to build offline: set `NOETA_WASM_SERVE`, or drop a prebuilt `noeta-wasm-serve.wasm` beside the binary.
