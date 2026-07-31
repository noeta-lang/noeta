# Edge Deployment

This page is the deployment companion to [WebAssembly & the Edge](WebAssembly-and-the-Edge), which covers how Noeta compiles to WebAssembly. Here the focus is narrow and practical: taking one `http.serve` program and putting it on an edge platform. **Fermyon Spin** is the primary target and the one worked through end to end; **Fastly Compute** is covered as a secondary target with an honest note on what is and isn't verified.

A complete, buildable version of everything below lives in [`examples/edge-hello/`](https://github.com/noeta-lang/noeta/tree/main/examples/edge-hello) (`hello.noe` + `spin.toml` + `deploy.sh`).

## What the artifact actually is

Two build verbs emit WebAssembly, and they emit **different things** — deploying to the edge uses the second one:

| Verb | Emits | WASI level | Entry shape | Where it runs |
|---|---|---|---|---|
| `noeta build --wasm app.noe` | a **module** (`app.wasm`) | `wasm32-wasip1` (preview 1) | a **CLI `_start`** — run once, exit | `wasmtime run app.wasm` — CLI tools, CI sandboxes, batch compute. **No sockets, no inbound HTTP.** |
| `noeta build --serve app.noe` | a **component** (`app.serve.wasm`) | `wasm32-wasip2` (preview 2) | a **`wasi:http/incoming-handler`** — invoked per request | any host that speaks the `wasi:http` proxy world: `wasmtime serve`, **Fermyon Spin**, Spin-class clouds. |

The edge deployment story is entirely the second row. `--serve` produces a genuine `wasi:http/incoming-handler` component (it exports the `wasi:http/proxy` world, the exact interface Spin and `wasmtime serve` consume) — not a wrapped CLI module and not a vendor-specific format. That is why the same artifact runs across `wasi:http` hosts without re-targeting.

Both verbs are **binary surgery, not compilation**: your program's compiled bundle is stapled into a prebuilt engine (`noeta_bundle::staple_wasm`), so a build is milliseconds and needs no Rust toolchain at app-build time — provided the prebuilt engine is where the build can find it, covered under [What the deploy asks of you](#what-the-deploy-asks-of-you).

## The program

A handler is an ordinary `(Request) -> Response`. Nothing about it is edge-specific — this is the same program `noeta run` runs on your laptop, on a real socket:

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

The `port` argument to `server.serve` is inert at the edge — the platform owns the socket and invokes your component per request — but it is what `noeta run app.noe` binds locally, so the one program serves both ways. Don't confuse this with the similarly named CLI verb: `noeta serve` is a separate convenience that expects a top-level `fn fetch(req: Request): Response` and drives its own `--port`. A program that calls `server.serve` itself, like this one, is run directly with `noeta run`. Routing (`req.path()`), reading the request (`req.method()`, `req.header(name)`, `req.body()`), and building the reply (`server.response(status, body, headers?)`, `Response.with_header`) are all just code; there is no framework or runtime hook to learn. See [std.http](std-http) for the full API.

## Building the artifact

```sh
noeta build --serve hello.noe          # → hello.serve.wasm (~4.3 MiB today; the size tracks the embedded runtime)
noeta build --serve hello.noe -o out/app.wasm   # explicit output path
```

The output is the component you deploy. You can smoke-test it under raw wasmtime before involving a platform:

```sh
wasmtime serve -S cli=y hello.serve.wasm    # serving on http://0.0.0.0:8080/
curl localhost:8080/health                  # ok
```

(`-S cli=y` grants the standard-library imports the component uses beyond the bare proxy world; Spin grants them by default, so its manifest needs no equivalent.)

## Primary target: Fermyon Spin

[Spin](https://spinframework.dev) runs `wasi:http` components directly — it embeds wasmtime, so there is no separate build step and no SDK. Deployment is a manifest plus the component.

### The manifest (`spin.toml`)

```toml
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

- **Routing.** `route = "/..."` is a wildcard: every path reaches the component, which routes internally on `req.path()`. For path-scoped components use a prefix like `route = "/api/..."`; Spin strips nothing — your handler still sees the full path.
- **Environment.** Add `[component.edge-hello.environment]` with `KEY = "value"` pairs; the program reads them through `env.get("KEY")`. Writes (`env.set`) are a per-request overlay and never escape the invocation.
- **Outbound allowlist.** Edge platforms deny outbound traffic by default. Every host your handler calls with `client.get(...)` (or any `std.http` verb) must be listed in `allowed_outbound_hosts` (e.g. `["https://api.example.com"]`); an un-listed host errors. Outbound calls travel the platform's own `wasi:http/outgoing-handler`, so connection pooling and TLS are the platform's, not yours.
- **Limits.** Per-request instantiation means there is **no cross-request state** — no in-process cache, no connection kept between requests. Persistent state belongs in the upstream services the handler calls. Execution and memory limits are the platform's to set (Spin/Fermyon enforce a per-request time budget); a handler that never replies answers a diagnostic `500` carrying its stdout and traceback rather than hanging.

### Run it locally

```sh
spin up                     # serves on http://127.0.0.1:3000
curl localhost:3000/health  # ok
```

`spin up` needs no account and is the fastest way to confirm the component and manifest agree. This is the exact path verified for this guide (Spin 4.0.2): the stapled component serves `/`, `/health`, an echo route, and a `/whoami` route that mints a v7 UUID and reads the wall clock — proving real entropy and real time reach the handler unchanged.

### Deploy it

```sh
spin login       # one-time: authenticate to Fermyon Cloud (needs a free account)
spin deploy      # uploads the component and prints the public URL
```

`spin deploy` is the **one step that needs an account**. The [`deploy.sh`](https://github.com/noeta-lang/noeta/tree/main/examples/edge-hello/deploy.sh) in the example runs the Noeta build, then guards the deploy: a missing `spin` CLI or a logged-out session prints the exact command to fix it instead of a cryptic failure. Any other host that speaks the `wasi:http` proxy world can serve the same `hello.serve.wasm` — the component is not Fermyon-specific.

## What the standard library gives a handler at the edge

The handler runs on `WasiHost` (`crates/noeta-wasi-host`), the third `Host` implementation alongside the deterministic sandbox and the CLI's real host. It is **real-but-synchronous**: the capabilities WASI genuinely exposes are real, the ones it cannot are honest runtime errors — never silent stubs. Precisely what works:

| Capability | At the edge | Notes |
|---|---|---|
| **Inbound HTTP** | ✅ | The request that invoked the component; your handler's reply is the response. |
| **Outbound HTTP** (`std.http` client) | ✅ | Through `wasi:http/outgoing-handler` — **allowlisted** per component (see the manifest). |
| **Entropy / UUIDs** (`id.uuid`, `id.uuid_v7`, `random_bytes`) | ✅ | Real host entropy. |
| **Wall clock** (`datetime.now().unix_ms()`) | ✅ | Real time. |
| **Seeded PRNG** (`random.*`) | ✅ | Still a pure function of its seed, as on every host — determinism rules are unchanged. |
| **Monotonic clock** | ✅ (logical) | An ordering device, not wall time — same as the CLI's real host. |
| **Environment / args** (`env.get`, `args.all`) | ✅ | Env from the manifest; `env.set` is a per-request overlay. |
| **Filesystem** (`fs.*`) | ⚠️ scratch only | Reads/writes the preopened directories the platform grants (typically an ephemeral per-request scratch). No preopen ⇒ ordinary IO errors. Not durable across requests. |
| **Subprocesses** (`os.exec`, `os.spawn`) | ❌ | A permanent WASI fact, not a Noeta gap — WASI has no process model. |
| **Telemetry export** (OTLP) | ❌ | Spans still track for context propagation and parenting, but nothing exports (the exporter needs its own host). |
| **p2p / local-first** (`para.p2p`) | ❌ | Not available on wasm targets. |
| **Isolates** | ⚠️ cooperative | Single-threaded scheduler — same semantics, no OS-thread parallelism. |

The single most important operational consequence is the ⚠️ on the filesystem and the ❌ on cross-request state: **an edge handler is stateless by construction.** Anything that must persist lives in an upstream service you call over allowlisted outbound HTTP.

## Secondary target: Fastly Compute

Fastly Compute runs WebAssembly at the edge and has adopted `wasi:http`, so a `wasi:http/incoming-handler` component is the right shape to bring. In principle `hello.serve.wasm` deploys with Fastly's own CLI:

```sh
fastly compute publish        # packages fastly.toml + the component, uploads to your service
```

with a `fastly.toml` naming the prebuilt component as the artifact.

**Honest status.** This path is **not verified** in this guide — it needs a Fastly account, and Fastly's long-standing, best-documented path is its own SDK targeting a Fastly-specific world (`fastly:api`) rather than the vendor-neutral `wasi:http` proxy world Noeta emits. Whether a given Fastly Compute service accepts the plain proxy-world component depends on the platform's current `wasi:http` support level, which we could not exercise. Treat Fastly as **expected-to-work, pending confirmation**; Spin is the target proven end to end. If you deploy to Fastly successfully, the mechanics are otherwise identical to Spin: build with `noeta build --serve`, allowlist outbound hosts in the platform manifest, and route on `req.path()`.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Every route answers `500` with "no program stapled in" | The component is the generic `noeta-wasm-serve` shell, not your app. | Rebuild with `noeta build --serve app.noe`; deploy that output, not a hand-copied component. |
| `500` with "the program produced no HTTP response" + a traceback | Your program never called `server.serve`, or the handler aborted before replying. | The `500` body carries the stdout and diagnostics — debug it exactly as locally. Confirm the top level calls `server.serve(port, handler)`. |
| Outbound `client.get(...)` errors at the edge but works under `noeta run` | The target host isn't allowlisted. | Add it to `allowed_outbound_hosts` in `spin.toml` (Fastly: the equivalent backend/allowlist config). |
| `noeta build --serve` fails: "building the serve component failed (is the wasm32-wasip2 target installed?)" | No prebuilt component was found, so the build fell back to compiling it, and the target is missing. | `rustup target add wasm32-wasip2`, or point `NOETA_WASM_SERVE` at a prebuilt `noeta-wasm-serve.wasm`. See below. |
| `fs.*` calls error at the edge | No directory was preopened, or the write outlived the request. | Grant a preopen in the platform config for scratch IO; never rely on the filesystem for cross-request persistence. |
| `os.exec` / `os.spawn` errors | WASI has no subprocesses. | Not fixable on this target — restructure to call a service instead. |
| Telemetry emits nothing | No OTLP exporter on wasm. | Expected; spans still propagate context. Export from an upstream collector if you need it. |

## What the deploy asks of you

The emitted component is the correct shape and runs on Spin unmodified. Three things sit between `noeta build --serve` and a live URL:

- **`--serve` needs a serve component to staple into, and resolves one via a ladder:** the `NOETA_WASM_SERVE` environment variable → a `noeta-wasm-serve.wasm` sitting next to the `noeta` binary → **an on-demand `cargo build` of the `noeta-wasm-serve` crate** (`wasm32-wasip2`). A developer checkout takes the third rung transparently, needing only `rustup target add wasm32-wasip2`. A binary-only toolchain has neither of the first two in place, so give it one — set `NOETA_WASM_SERVE`, or drop a prebuilt `noeta-wasm-serve.wasm` beside the binary — if it must build offline.
- **Spin is the verified platform.** Fastly Compute is documented above from its published contract rather than from a deploy we have run; treat that half as a starting point, not a tested path.
- **The hosted deploy authenticates as you.** `spin deploy` (or `fastly compute publish`) uses *your* account, so no guide can run it for you. Everything up to that point — build, manifest, local smoke test — is covered above and scripted in the example's `deploy.sh`; the final push is one command.
