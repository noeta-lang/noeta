// The playground's engine worker (P-WASM W2.2 + W3.0): instantiates the noeta-playground cdylib
// and serves check/run/fmt requests over its hand-rolled (ptr, len) ABI — the same calls the
// node smoke test (crates/noeta-playground/tests/browser_smoke.mjs) proves.
//
// Runs in a Web Worker on purpose, twice over:
// - the VM has no fuel counter, so the main thread's terminate-on-timeout IS the runaway-loop
//   guard (state here is throwaway — a terminated worker is simply respawned);
// - the browser host's `net_fetch` leaf is a **synchronous** XMLHttpRequest, which is legal in a
//   worker (only the main thread bans it) — that is what lets the engine's synchronous Host
//   trait reach the real network with no VM changes.

let engine = null;

// The `noeta_host` import module (W3.0): the real-world leaves the BrowserHost draws through.
// The closures late-bind `engine` — they are only callable during an export call, after
// instantiation assigned it.
function hostImports() {
  return {
    noeta_host: {
      // Real entropy for uuids and span ids: an i64 import, so a BigInt from getRandomValues.
      js_entropy_u64() {
        const word = new BigUint64Array(1);
        crypto.getRandomValues(word);
        return word[0];
      },
      js_now_ms: () => Date.now(),
      // Synchronous fetch. Request arrives as JSON in wasm memory; the reply goes back as a
      // length-prefixed buffer allocated through the engine's own allocator (the export-surface
      // packing, mirrored).
      js_net_fetch(ptr, len) {
        const request = JSON.parse(
          new TextDecoder().decode(new Uint8Array(engine.memory.buffer, ptr, len)),
        );
        let reply;
        try {
          const xhr = new XMLHttpRequest();
          xhr.open(request.method, request.url, false); // synchronous — worker-legal
          for (const [name, value] of request.headers) xhr.setRequestHeader(name, value);
          xhr.send(request.body.length > 0 ? request.body : null);
          const headers = xhr
            .getAllResponseHeaders()
            .trim()
            .split(/\r?\n/)
            .filter(Boolean)
            .map((line) => {
              const at = line.indexOf(': ');
              return [line.slice(0, at), line.slice(at + 2)];
            });
          reply = { status: xhr.status, headers, body: xhr.responseText };
        } catch (error) {
          reply = { error: String(error?.message ?? error) };
        }
        return pack(JSON.stringify(reply));
      },
    },
  };
}

// Mirror of the Rust `pack()`: `[len: u32 LE][bytes]` in one engine allocation.
function pack(json) {
  const bytes = new TextEncoder().encode(json);
  const ptr = engine.noeta_alloc(4 + bytes.length);
  new DataView(engine.memory.buffer).setUint32(ptr, bytes.length, true);
  new Uint8Array(engine.memory.buffer, ptr + 4, bytes.length).set(bytes);
  return ptr;
}

async function instantiate() {
  const response = await fetch('./noeta_playground.wasm');
  const imports = hostImports();
  try {
    const { instance } = await WebAssembly.instantiateStreaming(response.clone(), imports);
    return instance.exports;
  } catch {
    // A host serving the artifact without `application/wasm` breaks streaming instantiation;
    // fall back to the buffered path rather than fail the whole playground.
    const { instance } = await WebAssembly.instantiate(await response.arrayBuffer(), imports);
    return instance.exports;
  }
}

function call(exports, entry, source) {
  const encoded = new TextEncoder().encode(source);
  const input = exports.noeta_alloc(encoded.length);
  new Uint8Array(exports.memory.buffer, input, encoded.length).set(encoded);
  const out = entry(input, encoded.length); // consumes the input buffer
  const len = new DataView(exports.memory.buffer).getUint32(out, true);
  const json = new TextDecoder().decode(new Uint8Array(exports.memory.buffer, out + 4, len));
  exports.noeta_free_result(out);
  return JSON.parse(json);
}

self.onmessage = async (event) => {
  const { id, op, source } = event.data;
  try {
    engine ??= await instantiate();
    const entry = {
      check: engine.noeta_check,
      run: engine.noeta_run,
      'run-browser': engine.noeta_run_browser,
      fmt: engine.noeta_fmt,
    }[op];
    if (!entry) throw new Error(`unknown op: ${op}`);
    self.postMessage({ id, ok: true, result: call(engine, entry, source) });
  } catch (error) {
    // A trap poisons the instance; drop it so the next request re-instantiates.
    engine = null;
    self.postMessage({ id, ok: false, error: String(error?.message ?? error) });
  }
};

// Tell the main thread the engine is warm (first paint can enable the buttons immediately).
instantiate().then(
  (exports) => { engine = exports; self.postMessage({ ready: true }); },
  (error) => self.postMessage({ ready: false, error: String(error?.message ?? error) }),
);
