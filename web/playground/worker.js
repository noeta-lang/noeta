// The playground's engine worker (P-WASM W2.2): instantiates the noeta-playground cdylib and
// serves check/run/fmt requests over its hand-rolled (ptr, len) ABI — the same calls the node
// smoke test (crates/noeta-playground/tests/browser_smoke.mjs) proves.
//
// Runs in a Web Worker on purpose: the VM has no fuel counter, so the main thread's
// terminate-on-timeout IS the runaway-loop guard. State here is throwaway — a terminated worker
// is simply respawned.

let engine = null;

async function instantiate() {
  const response = await fetch('./noeta_playground.wasm');
  try {
    const { instance } = await WebAssembly.instantiateStreaming(response.clone(), {});
    return instance.exports;
  } catch {
    // A host serving the artifact without `application/wasm` breaks streaming instantiation;
    // fall back to the buffered path rather than fail the whole playground.
    const { instance } = await WebAssembly.instantiate(await response.arrayBuffer(), {});
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
    const entry = { check: engine.noeta_check, run: engine.noeta_run, fmt: engine.noeta_fmt }[op];
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
