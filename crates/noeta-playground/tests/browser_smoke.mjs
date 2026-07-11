// Browser-engine smoke test (P-WASM W2.1): instantiate the wasm32-unknown-unknown cdylib the way
// the playground's Web Worker does — the plain WebAssembly API, no bundler, no wasm-bindgen — and
// drive check/run/fmt through the hand-rolled ABI. Run by the CI wasm job (and by hand):
//
//   cargo build -p noeta-playground --target wasm32-unknown-unknown --profile wasm-release
//   node crates/noeta-playground/tests/browser_smoke.mjs \
//     target/wasm32-unknown-unknown/wasm-release/noeta_playground.wasm
//
// This is the runtime gate the native unit tests cannot provide: it proves the engine (salsa
// graph included) actually executes in a JS embedding, and that the ABI's memory discipline
// holds from the caller's side.

import { readFile } from 'node:fs/promises';
import assert from 'node:assert/strict';

const wasmPath = process.argv[2] ?? 'target/wasm32-unknown-unknown/wasm-release/noeta_playground.wasm';
const bytes = await readFile(wasmPath);

// The `noeta_host` imports (W3.0), supplied exactly like the worker does — entropy and wall
// clock real, fetch canned (node has no sync XHR; the worker's is the same JSON contract).
let engine = null; // late-bound: imports are only called during an export call
const fetched = [];
function packReply(json) {
  const replyBytes = new TextEncoder().encode(json);
  const ptr = engine.noeta_alloc(4 + replyBytes.length);
  new DataView(engine.memory.buffer).setUint32(ptr, replyBytes.length, true);
  new Uint8Array(engine.memory.buffer, ptr + 4, replyBytes.length).set(replyBytes);
  return ptr;
}
const imports = {
  noeta_host: {
    js_entropy_u64() {
      const word = new BigUint64Array(1);
      crypto.getRandomValues(word);
      return word[0];
    },
    js_now_ms: () => Date.now(),
    js_net_fetch(ptr, len) {
      const request = JSON.parse(new TextDecoder().decode(new Uint8Array(engine.memory.buffer, ptr, len)));
      fetched.push(request);
      return packReply(JSON.stringify({ status: 200, headers: [['x-test', 'yes']], body: 'pong' }));
    },
  },
};
const { instance } = await WebAssembly.instantiate(bytes, imports);
engine = instance.exports;
const {
  memory, noeta_alloc, noeta_check, noeta_run, noeta_run_browser, noeta_fmt, noeta_free_result,
  noeta_hover, noeta_definition, noeta_complete, noeta_signature,
} = instance.exports;

function call(entry, source, ...extra) {
  const encoded = new TextEncoder().encode(source);
  const input = noeta_alloc(encoded.length);
  new Uint8Array(memory.buffer, input, encoded.length).set(encoded);
  const out = entry(input, encoded.length, ...extra); // consumes the input buffer
  const len = new DataView(memory.buffer).getUint32(out, true);
  const json = new TextDecoder().decode(new Uint8Array(memory.buffer, out + 4, len));
  noeta_free_result(out);
  return JSON.parse(json);
}

// A clean program checks clean and runs deterministically.
const clean = 'use std.random;\nrandom.seed(7);\necho "hello from the browser engine";\necho random.int(0, 100);';
assert.equal(call(noeta_check, clean).diagnostics.length, 0);
const run1 = call(noeta_run, clean);
const run2 = call(noeta_run, clean);
assert.equal(run1.compiled, true);
assert.equal(run1.exit_code, 0);
assert.match(run1.stdout, /^hello from the browser engine\n\d+\n$/);
assert.equal(run1.stdout, run2.stdout, 'the sandbox is deterministic');

// A type error surfaces as a stable JSON diagnostic with a real location.
const typo = 'mut x = 1;\nx = "s";';
const diags = call(noeta_check, typo).diagnostics;
assert.ok(diags.length > 0);
assert.match(diags[0].code, /^E\d{4}$/);
assert.equal(diags[0].file, 'playground.noe');
assert.equal(diags[0].line, 2);

// An abort renders its traceback.
const aborting = 'fn boom(): int {\n  panic("kaboom");\n}\necho boom();';
const abortRun = call(noeta_run, aborting);
assert.equal(abortRun.compiled, true);
assert.notEqual(abortRun.exit_code, 0);
assert.match(abortRun.trace, /boom/);

// The formatter round-trips.
const fmt = call(noeta_fmt, 'echo   "hello"  ;');
assert.equal(fmt.ok, true);
assert.match(fmt.formatted, /echo "hello"/);

// The IDE smarts (W2.3 engine half) answer over the persistent DocumentStore — proving the
// whole noeta-ide engine (salsa incrementality included) runs in a JS embedding. Positions are
// zero-based (line, UTF-16 character), the LSP convention.
const ideText = 'fn add(a: int, b: int): int {\n  return a + b;\n}\n\necho add(1, 2);';
const hover = call(noeta_hover, ideText, 1, 9);
assert.equal(hover.found, true);
assert.equal(hover.type, 'int');
const def = call(noeta_definition, ideText, 4, 5);
assert.equal(def.found, true);
assert.equal(def.range.start.line, 0);
const sig = call(noeta_signature, ideText, 4, 12);
assert.equal(sig.found, true);
assert.equal(sig.active, 1);
const completions = call(noeta_complete, ideText, 4, 0);
assert.ok(completions.items.some((item) => item.label === 'add' && item.kind === 'function'));

// The browser host (W3.0): `noeta_run_browser` reaches the real-world leaves through the
// imports — a full std.http round trip over the JSON contract, plus real entropy for uuids.
const fetching = 'use std.http.client\nr = client.get("https://svc.test/ping")\necho r.status()\necho r.body()';
const browserRun = call(noeta_run_browser, fetching);
assert.equal(browserRun.compiled, true, JSON.stringify(browserRun));
assert.equal(browserRun.exit_code, 0, JSON.stringify(browserRun));
assert.equal(browserRun.stdout, '200\npong\n');
assert.equal(fetched.length, 1);
assert.equal(fetched[0].method, 'GET');
assert.equal(fetched[0].url, 'https://svc.test/ping');
// Real entropy: two uuids from one browser-host run differ (the sandbox's would be fixed).
const uuids = call(noeta_run_browser, 'use std.id;\necho id.uuid();\necho id.uuid();');
const [a, b] = uuids.stdout.trim().split('\n');
assert.notEqual(a, b);
// The sandbox stays the default and deterministic — the same program, fixed stream.
const sandboxUuids = call(noeta_run, 'use std.id;\necho id.uuid();\necho id.uuid();');
const again = call(noeta_run, 'use std.id;\necho id.uuid();\necho id.uuid();');
assert.equal(sandboxUuids.stdout, again.stdout);

console.log('browser-engine smoke: all assertions passed ✓');
