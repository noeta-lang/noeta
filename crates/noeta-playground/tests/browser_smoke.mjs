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
const { instance } = await WebAssembly.instantiate(bytes, {});
const { memory, noeta_alloc, noeta_check, noeta_run, noeta_fmt, noeta_free_result } = instance.exports;

function call(entry, source) {
  const encoded = new TextEncoder().encode(source);
  const input = noeta_alloc(encoded.length);
  new Uint8Array(memory.buffer, input, encoded.length).set(encoded);
  const out = entry(input, encoded.length); // consumes the input buffer
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

console.log('browser-engine smoke: all assertions passed ✓');
