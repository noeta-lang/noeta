# noeta-wasm-serve

The edge-serve component: unchanged `http.serve` programs running on `wasmtime serve`-class platforms as a `wasi:http/incoming-handler` component.

- **Takes in:** an inbound HTTP request, invoked per request by the host platform.
- **Emits:** an HTTP response, by running the embedded program on a `WasiHost` armed with a one-request inbound script.

The inversion that makes this a zero-VM-change slice: a `wasi:http` component is invoked per request, and the deterministic sandbox already models inbound serving as a finite request script that ends the serve loop. So each invocation runs the embedded program fresh: the program's `http.serve(port, handler)` accepts exactly this request, the handler replies through the ordinary inbound `Network` capability, the next accept yields `None`, the serve loop returns, and the captured reply becomes the component's response — per-request isolation is the platform's own model, so a fresh VM per request is the natural shape here, not an inefficiency. The program arrives by stapling (the same slot mechanism as `noeta-wasm-runner`): `noeta build --serve` patches the `.noeb` into a prebuilt generic component's data section. A handler that never replies (a non-serving program, an abort) answers 500 with the run's output as the body. Split on purpose: the core (request → run → response over neutral `NetRequest`/`NetResponse`) is target-agnostic and natively unit-tested; the `wasi:http` type glue lives in `component.rs`, compiled only for the wasi target. Built `cdylib` (the wasip2 component) plus `rlib` (native unit tests).

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
