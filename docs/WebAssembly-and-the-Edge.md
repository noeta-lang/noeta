# WebAssembly & the Edge

Noeta programs run as WebAssembly in three shapes, all from the same source with no code changes:

| Shape | Command | Runs on |
|---|---|---|
| **Standalone module** | `noeta build --wasm app.noe` | Any WASI runtime: `wasmtime run app.wasm`, CLI tools, CI sandboxes, compute platforms. |
| **HTTP serve component** | `noeta build --serve app.noe` | Any `wasi:http` host: `wasmtime serve`, [Spin](https://spinframework.dev), and Spin-class edge clouds. |
| **The browser playground** | — | The whole toolchain (checker, formatter, VM, and the LSP's language smarts) compiled to WebAssembly, running client-side in a Web Worker. |

Both build verbs are **binary surgery rather than compilation**: the toolchain patches your program's compiled bundle into a prebuilt engine, the same mechanism as `noeta build --exe` adapted to the WebAssembly binary format. No Rust toolchain, no `cargo`, no separate build step.

A program behaves the same because it compiled to wasm, and two oracles hold that. The **wasm differential** runs the entire conformance corpus through the shipped wasm engine under wasmtime for the standalone module shape, asserting stdout, exit code, and rendered diagnostics and tracebacks byte-identical to a native run. The **`wasi:http` serve e2e** builds a handler with `--serve`, runs the component under `wasmtime serve`, and asserts a live HTTP round trip, including an outbound proxy call and the failure the handler sees when its upstream goes away. It repeats that round trip under Spin when `spin` is on `PATH`.

## Standalone modules: `noeta build --wasm`

```sh
noeta build --wasm app.noe          # → app.wasm
wasmtime run app.wasm               # no flags needed
wasmtime run --dir . app.wasm data  # grant the working directory, pass args
```

The artifact is a single `wasm32-wasip1` module: the bytecode VM with your program's bundle stapled into its data section. What the program sees:

- **Real file IO** over the directories the host grants (`wasmtime run --dir .`). With no preopens, file operations return ordinary IO errors.
- **Real environment, arguments, wall clock, and entropy.** `env.get`, `args.all()`, `datetime.now().unix_ms()`, and `id.uuid()` behave as they do under `noeta run`.
- **The language's determinism rules.** `random.seed(n)` still makes `random.*` a pure function of `n`, and `monotonic` stays a logical ordering device. Real time and real entropy live in their own capabilities, as on every host.
- `os.platform()` reports `"wasi"` and `os.arch()` reports `"wasm32"`.

Two things it does not have are **networking**, which is the serve component's job, so `http.*` errors and points at `--serve`; and **subprocesses**, since `os.exec` and `os.spawn` error on a target WASI gives no process model. Isolates run cooperatively on the single-threaded scheduler: same semantics, no OS threads.

Execution is the tier-0 interpreter, and the wasm engine's own JIT compiles the interpreter loop. The shipped runner is built for speed over size, because edge compute wants the throughput. A size-critical deploy can build the runner with its own size-tuned profile and point `NOETA_WASM_RUNNER` at the result.

## HTTP at the edge: `noeta build --serve`

An unchanged `server.serve` program, the same one `noeta run` runs on your machine against a real socket, deploys as a **`wasi:http` component**:

```noeta check
use std.http.server
use std.http.{Request, Response}

fn handle(req: Request): Response {
    return server.response(200, "hello from the edge: ${req.path()}")
}

server.serve(8080, handle)
```

```sh
noeta build --serve app.noe                    # → app.serve.wasm
wasmtime serve -S cli=y app.serve.wasm         # serving on http://0.0.0.0:8080/
```

`-S cli=y` grants the standard-library imports beyond the bare proxy world; Spin grants them by default.

The execution model matches how these platforms work. The host instantiates the component **per request**, and each invocation runs your program against a one-request world: `server.serve` accepts exactly the incoming request, your handler replies, and the serve loop ends. The port number is the platform's concern and is ignored. A program that never replies, or that aborts first, answers a diagnostic `500` carrying its output and traceback, so you debug at the edge with the same information as locally.

### Calling upstream services

Handlers are full HTTP clients. `client.get(...)` and the rest of `std.http` go out through the platform's own connection-pooled client (`wasi:http/outgoing-handler`), so proxying and API composition work as they do anywhere else:

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

Edge platforms typically **allowlist outbound hosts**. Under Spin, list them in the manifest; `wasmtime serve` allows all outbound traffic by default.

### Running under Spin (and Spin-class clouds)

The same artifact runs under [Spin](https://spinframework.dev) unmodified, verified against Spin 4, and any host that speaks the `wasi:http` proxy world can serve the component. That is the point of targeting the standard interface rather than a vendor SDK. [Edge Deployment](Edge-Deployment) has the step-by-step walkthrough: the `spin.toml` manifest, `spin up` and `spin deploy`, routing and env and outbound limits, the full edge capability table, a troubleshooting matrix, and Fastly Compute as a secondary target. A complete buildable example is in `examples/edge-hello/`.

### What a handler world looks like

Per request, the program gets a fresh deterministic-by-default world plus the real-world capabilities a request handler needs: wall clock, entropy for uuids, an in-memory scratch filesystem, and outbound HTTP. There are no subprocesses and no cross-request state, so persistent state belongs in the upstream services the handler calls, which is the shape edge platforms enforce on every language.

## The browser playground

The playground, live at [play.noeta.dev](https://play.noeta.dev), is the **real toolchain compiled to WebAssembly**, running client-side in a Web Worker with no backend. Check, Run and Format go through the same pipeline as `noeta run` on the deterministic sandbox, so playground output is oracle-grade. Hover, completion, go-to-definition, and signature help come from the same engine `noeta lsp` adapts over. A "real host" mode backs entropy, wall clock, and `std.http` fetches with the browser's own APIs, and an infinite loop is stopped by terminating the worker, which keeps the page responsive.

## Limits, plainly

| Area | On wasm |
|---|---|
| Isolates | Cooperative on a single-threaded scheduler: same semantics, no parallelism. |
| JIT tier | Not applicable. A wasm module cannot generate code, so the wasm engine JIT-compiles the interpreter instead. |
| Subprocesses | None. `os.exec` and `os.spawn` error, which is a WASI and browser fact rather than a Noeta gap. |
| Inbound HTTP | Serve components only; the standalone module has no sockets. |
| Telemetry | Spans track for context propagation, and nothing exports, since OTLP needs an exporter host. |
| p2p / local-first | Unavailable on wasm targets. |
