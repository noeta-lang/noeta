# WebAssembly & the Edge

Noeta programs run as WebAssembly in three shapes, all from the same source with no code changes:

| Shape | Command | Runs on |
|---|---|---|
| **Standalone module** | `noeta build --wasm app.noe` | Any WASI runtime: `wasmtime run app.wasm` — CLI tools, CI sandboxes, compute platforms. |
| **HTTP serve component** | `noeta build --serve app.noe` | Any `wasi:http` host: `wasmtime serve`, [Spin](https://spinframework.dev), and Spin-class edge clouds. |
| **The browser playground** | — | The whole toolchain (checker, formatter, VM, and the LSP's language smarts) compiled to WebAssembly, running client-side in a Web Worker. |

Both build verbs are **millisecond-fast binary surgery**, not compilations: the toolchain patches your program's compiled bundle into a prebuilt engine (the same mechanism as `noeta build --exe`, adapted to the WebAssembly binary format). No Rust toolchain, no `cargo`, no separate build step.

The safety story is the same as everywhere else in Noeta: the **wasm differential oracle** runs the entire conformance corpus through the shipped wasm engine under wasmtime — for the standalone module shape (`--wasm`) — and asserts the output byte-identical to a native run (stdout, exit code, and rendered diagnostics/tracebacks). The serve component (`--serve`) shares the same underlying engine but isn't yet in that automated corpus; it's checked by hand against Spin instead (see below). A program does not behave differently because it compiled to wasm.

## Standalone modules — `noeta build --wasm`

```sh
noeta build --wasm app.noe          # → app.wasm (~2 MiB)
wasmtime run app.wasm               # no flags needed
wasmtime run --dir . app.wasm data  # grant the working directory, pass args
```

The artifact is a single `wasm32-wasip1` module: the bytecode VM with your program's bundle stapled into its data section. What the program sees:

- **Real file IO** over the directories the host grants (`wasmtime run --dir .`); no preopens means file operations return ordinary IO errors.
- **Real environment, arguments, wall clock, and entropy** — `env.get`, `args.all()`, `datetime.now().unix_ms()`, and `id.uuid()` behave exactly as under `noeta run`.
- **The language's determinism rules hold**: `random.seed(n)` still makes `random.*` a pure function of `n`, and `monotonic` stays a logical ordering device — real time and real entropy live in their own capabilities, as on every host.
- `os.platform()` reports `"wasi"`, `os.arch()` reports `"wasm32"`.

What it honestly does not have: **networking** (that is the serve component's job — `http.*` errors with a pointer to `--serve`) and **subprocesses** (`os.exec`/`os.spawn` error; WASI has none). Isolates run cooperatively on the single-threaded scheduler — same semantics, no OS threads.

Execution is the tier-0 interpreter; the wasm engine's own JIT compiles the interpreter loop. The runner is built for speed over size (a size-optimized build was measured ~40 % smaller but ~60 % slower and rejected as the default).

## HTTP at the edge — `noeta build --serve`

An unchanged `server.serve` program — the same one `noeta run` runs on your machine, on a real socket — deploys as a **`wasi:http` component**:

```noeta check
use std.http.server
use std.http.{Request, Response}

fn handle(req: Request): Response {
    return server.response(200, "hello from the edge: ${req.path()}")
}

server.serve(8080, handle)
```

```sh
noeta build --serve app.noe                    # → app.serve.wasm (~4.3 MiB today; the size tracks the embedded runtime)
wasmtime serve -S cli=y app.serve.wasm         # serving on http://0.0.0.0:8080/
```

(The `-S cli=y` grants the standard-library imports beyond the bare proxy world; Spin grants them by default.)

The execution model matches how these platforms actually work: the host instantiates the component **per request**, and each invocation runs your program against a one-request world — `server.serve` accepts exactly the incoming request, your handler replies, and the serve loop ends. The port number is the platform's concern and is ignored. A program that never replies (or aborts first) answers a diagnostic `500` carrying its output and traceback — you debug at the edge with the same information as locally.

### Calling upstream services

Handlers are full HTTP clients: `client.get(...)` (and the rest of `std.http`) goes out through the platform's own connection-pooled client (`wasi:http/outgoing-handler`), so proxying and API composition work as they do anywhere else:

```noeta check
use std.http.server
use std.http.client
use std.http.{Request, Response}

fn handle(req: Request): Response {
    // A transport failure is the `Err` arm; degrade rather than take down the handler.
    return match client.get("https://api.example.com/data") {
        Ok(upstream) => server.response(200, "upstream said: ${upstream.body()}"),
        Err(e) => server.response(502, "upstream unreachable: ${e.kind()}"),
    }
}

server.serve(8080, handle)
```

Note that edge platforms typically **allowlist outbound hosts** — under Spin, list them in the manifest (below); `wasmtime serve` allows all outbound traffic by default.

### Running under Spin (and Spin-class clouds)

The same artifact runs under [Spin](https://spinframework.dev) unmodified — verified against Spin 4 — and any host that speaks the `wasi:http` proxy world can serve the component; that is the point of targeting the standard interface rather than a vendor SDK. The step-by-step walkthrough — the `spin.toml` manifest, `spin up`/`spin deploy`, routing/env/outbound limits, the full edge capability table, a troubleshooting matrix, and Fastly Compute as a secondary target — lives on [Edge Deployment](Edge-Deployment), with a complete buildable example in `examples/edge-hello/`.

### What a handler world looks like

Per request, the program gets a fresh deterministic-by-default world with the real-world capabilities a request handler needs: wall clock, entropy (uuids), an in-memory scratch filesystem, and outbound HTTP. There are no subprocesses and no cross-request state — persistent state belongs in the upstream services the handler calls, which is the shape edge platforms enforce on every language.

## The browser playground

The playground (the separate `noeta-playground` repo, live at play.noeta.dev) is not a transpiler or a service — it is the **real toolchain compiled to WebAssembly**, running client-side in a Web Worker with no backend. Check / Run / Format go through the same pipeline as `noeta run` on the deterministic sandbox (playground output is oracle-grade), and hover, completion, go-to-definition, and signature help come from the same engine `noeta lsp` adapts over. A "real host" mode backs entropy, wall clock, and `std.http` fetches with the browser's own APIs, and an infinite loop is stopped by terminating the worker — the page stays responsive.

## Limits, plainly

| Area | On wasm |
|---|---|
| Isolates | Cooperative (single-threaded scheduler) — same semantics, no parallelism. |
| JIT tier | Not applicable: wasm modules cannot generate code; the wasm engine JIT-compiles the interpreter instead. |
| Subprocesses | None (`os.exec`/`os.spawn` error) — a WASI/browser fact, not a Noeta gap. |
| Inbound HTTP | Serve components only; the standalone module has no sockets. |
| Telemetry | Spans track for context propagation but nothing exports (OTLP needs an exporter host). |
| p2p / local-first | Not available on wasm targets. |
