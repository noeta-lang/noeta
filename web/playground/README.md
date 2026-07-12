# Noeta playground (P-WASM W2.2)

The **real toolchain, client-side**: the same lexer → parser → checker → compiler → VM that
`noeta run` uses, compiled to `wasm32-unknown-unknown` and executing on the deterministic
sandbox. No backend to operate — three static files plus the engine artifact.

## Build & serve

```sh
# One command: engine + page + brotli side-cars → web/playground/dist/ (the deployable bundle —
# when hosting exists, upload this directory as-is).
bash web/playground/build.sh

# Any static file server works (the page needs http(s) for workers + streaming instantiation):
python -m http.server -d web/playground/dist 8080
# → http://localhost:8080
```

The CI `wasm` job builds the same bundle on every run and uploads it as the
`playground-dist` artifact — a deployable playground is always one download away.

## How it works

- `worker.js` instantiates the cdylib and speaks its hand-rolled `(ptr, len)` ABI —
  `noeta_check` / `noeta_run` / `noeta_fmt`, each returning a length-prefixed JSON buffer. The
  same calls are proven headlessly by `crates/noeta-playground/tests/browser_smoke.mjs` (node,
  in the CI `wasm` job), so the page is thin glue over a tested seam.
- `app.js` owns the worker lifecycle. The engine runs in a Web Worker and the main thread
  **terminates it after 5 s and respawns** — that is the runaway-loop guard; the VM deliberately
  has no fuel counter.
- Diagnostics arrive in the stable `JsonDiagnostic` shape (`noeta check --format json`); the
  pane links each one back to its byte span in the editor.
- **Share** encodes the buffer into the URL fragment (`share.js`, versioned base64url — pure
  functions, node-tested) — no backend, and the fragment never reaches the server.
- Everything is dependency-free (no bundler, no CDN): the playground works offline and its only
  build step is `cargo build`.
