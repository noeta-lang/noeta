// Browser-engine smoke test: instantiate the wasm32-unknown-unknown cdylib the way
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
// The JSPI trio (W3.1), with **controlled resolution** so overlap is provable: every started
// fetch settles on a macrotask, and the event log records starts vs settles.
const JSPI = typeof WebAssembly.Suspending === 'function' && typeof WebAssembly.promising === 'function';
const tickets = new Map();
let nextTicket = 1n;
const events = [];
let settledSinceWait = false;
let wakeResolve = () => {};
let wakePromise = new Promise((resolve) => { wakeResolve = resolve; });
// The debug pause seam (W2.4), scripted: node has no paused UI, so resume commands come from a
// queue (empty queue = terminate, the engine's own fail-stop default). Every pause payload is
// recorded for the assertions; in the real worker this import parks on Atomics.wait instead.
const debugPauses = [];
const debugCommands = [];
const imports = {
  noeta_host: {
    js_debug_pause(ptr, len) {
      const payload = JSON.parse(new TextDecoder().decode(new Uint8Array(engine.memory.buffer, ptr, len)));
      debugPauses.push(payload);
      const command = debugCommands.length > 0 ? debugCommands.shift() : { action: 'terminate' };
      return packReply(JSON.stringify(command));
    },
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
    js_fetch_start(ptr, len) {
      const request = JSON.parse(new TextDecoder().decode(new Uint8Array(engine.memory.buffer, ptr, len)));
      const ticket = nextTicket++;
      const entry = { done: false, resultJson: null };
      tickets.set(ticket, entry);
      events.push(`start ${request.url}`);
      setTimeout(() => {
        entry.resultJson = JSON.stringify({
          status: 200,
          headers: [],
          body: `${new URL(request.url).pathname}-pong`,
        });
        entry.done = true;
        events.push(`settle ${request.url}`);
        settledSinceWait = true;
        wakeResolve();
      }, 5);
      return ticket;
    },
    js_fetch_take(ticket) {
      const entry = tickets.get(ticket);
      if (!entry?.done) return 0;
      tickets.delete(ticket);
      return packReply(entry.resultJson);
    },
    js_wait: JSPI
      ? new WebAssembly.Suspending(async (timeoutMs) => {
          if (settledSinceWait) {
            settledSinceWait = false;
            wakePromise = new Promise((resolve) => { wakeResolve = resolve; });
            return;
          }
          const racers = [wakePromise];
          if (timeoutMs >= 0) racers.push(new Promise((r) => setTimeout(r, timeoutMs)));
          await Promise.race(racers);
          settledSinceWait = false;
          wakePromise = new Promise((resolve) => { wakeResolve = resolve; });
        })
      : () => { throw new Error('js_wait requires JSPI'); },
  },
};
const { instance } = await WebAssembly.instantiate(bytes, imports);
engine = instance.exports;
const {
  memory, noeta_alloc, noeta_check, noeta_run, noeta_run_browser, noeta_fmt, noeta_free_result,
  noeta_hover, noeta_definition, noeta_complete, noeta_signature, noeta_debug_run,
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

// An IDE request is legal as the engine's FIRST call — this must stay above every other call to
// keep meaning what it says. The store resolves `std` names against the process-default registry,
// which only `front_end` (check / run / debug) seeds; the IDE entries used to skip that, so a
// hover arriving before the page's first diagnostics pass aborted the whole instance on a wasm
// `unreachable`. That is exactly when a visitor's first hover lands, and only a cold instance can
// prove it — a native test shares one already-seeded process registry.
assert.equal(
  call(noeta_hover, 'x = 1;\necho x;', 1, 5).found,
  true,
  'hover must answer on a cold engine, before any check/run',
);

// A clean program checks clean and runs deterministically.
const clean = 'use std.random;\nrandom.seed(7);\necho "hello from the browser engine";\necho random.int(0, 100);';
assert.equal(call(noeta_check, clean).diagnostics.length, 0);
const run1 = call(noeta_run, clean);
const run2 = call(noeta_run, clean);
assert.equal(run1.compiled, true);
assert.equal(run1.exit_code, 0);
assert.match(run1.stdout, /^hello from the browser engine\n\d+\n$/);
assert.equal(run1.stdout, run2.stdout, 'the sandbox is deterministic');

// The parser's nesting limit, delivered on the browser's ONE stack. Natively a parse deeper than
// `INLINE_NESTING_DEPTH` is offloaded to a `DEEP_PARSE_STACK` worker; wasm has no threads, so
// there is nothing to offload to and the module's link-time stack (`WASM_STACK_SIZE`, passed to
// wasm-ld from `.cargo/config.toml`) is what has to hold it. The parser used to ask for the worker
// anyway — `stacker` cannot read a stack limit on wasm, so its "am I short on stack?" test said
// yes unconditionally — and `spawn` on a thread-free target is `Err`, so EVERY parse in this
// engine died on `.expect("spawn parse worker")`. These four are the worst shapes at exactly the
// depths E0032 promises to accept: they are the measurement `WASM_STACK_SIZE` is sized from, and
// they fail the only way a blown wasm stack can — by trapping the instance.
const atLimit = {
  // 128 nested delimiters — `MAX_NESTING_DEPTH`.
  'nested delimiters': `x = ${'['.repeat(128)}1${']'.repeat(128)};\n`,
  // The same depth in the most expensive shape measured (~3.2 KiB of wasm stack per level).
  'nested function values': `x = ${'fn() { return '.repeat(128)}1${' }'.repeat(128)};\n`,
  // 128 `if … then … else if` branches — `MAX_TERNARY_CHAIN_BRANCHES`. A chain is flat in
  // delimiters, so the depth test above cannot stand in for it.
  'conditional-expression chain':
    `c = true;\nx = ${'if c then 1 else '.repeat(128)}0;\n`,
  // 512 `else if` branches — `MAX_ELSE_CHAIN_BRANCHES`.
  'else-if chain':
    `c = true;\n${'if c { echo 1; } else '.repeat(512)}{ echo 0; }\n`,
};
for (const [shape, src] of Object.entries(atLimit)) {
  const deep = call(noeta_check, src);
  assert.deepEqual(deep.diagnostics, [], `${shape} at the limit must check clean, not E0032/trap`);
}
// One level past the limit is a diagnostic, not a trap — the pre-pass rejects before recursing.
const tooDeep = call(noeta_check, `x = ${'['.repeat(129)}1${']'.repeat(129)};\n`).diagnostics;
assert.equal(tooDeep[0]?.code, 'E0032', JSON.stringify(tooDeep));
// And the whole pipeline, not just the parse, has to fit that one stack: IR lowering and the VM
// recurse over the same shape and want MORE of it than the parse does. Measured at a deliberately
// starved 128 KiB, `noeta_check` still answered at depth 64 while `noeta_run` on the same source
// already trapped — which is why WASM_STACK_SIZE is sized on the pipeline (MIN_PIPELINE_STACK)
// and not on the parse alone.
const deepRun = call(noeta_run, `x = ${'['.repeat(128)}1${']'.repeat(128)};\necho "deep ok";\n`);
assert.equal(deepRun.compiled, true, JSON.stringify(deepRun));
assert.equal(deepRun.exit_code, 0, JSON.stringify(deepRun));
assert.equal(deepRun.stdout, 'deep ok\n');

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
// The engine composes the same rich Markdown the VS Code hover shows (a `value` field), not a
// bare `type` string — a hover over `a` in `a + b` carries its `int` type.
assert.ok(hover.value.includes('int'), `hover value: ${hover.value}`);
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
const fetching = 'use std.http.client\nr = client.get("https://svc.test/ping")?\necho r.status()\necho r.body()';
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

// The debug run (W2.4): a breakpoint inside `add` pauses with the captured stack + locals, a
// stepOver lands on the next line, and continue finishes the run — the browser debugger's whole
// command loop over the scripted embedder.
const debugSource = 'fn add(a: int, b: int): int {\n  c = a + b;\n  return c;\n}\n\necho add(1, 2);';
debugCommands.push({ action: 'stepOver' }, { action: 'continue' });
const debugRun = call(noeta_debug_run, JSON.stringify({
  source: debugSource,
  breakpoints: [2],
  stop_on_entry: false,
}));
assert.equal(debugRun.compiled, true, JSON.stringify(debugRun));
assert.equal(debugRun.exit_code, 0);
assert.equal(debugRun.stdout, '3\n');
assert.equal(debugRun.terminated, false);
assert.equal(debugPauses.length, 2, JSON.stringify(debugPauses));
assert.equal(debugPauses[0].reason, 'breakpoint');
assert.equal(debugPauses[0].frames[0].name, 'add');
assert.equal(debugPauses[0].frames[0].line, 2);
assert.ok(debugPauses[0].frames[0].locals.some((l) => l.name === 'a' && l.value === '1' && l.ty === 'int'),
  JSON.stringify(debugPauses[0].frames[0].locals));
assert.equal(debugPauses[0].frames.at(-1).name, 'main');
assert.equal(debugPauses[1].reason, 'step');
assert.equal(debugPauses[1].frames[0].line, 3);
// A terminate from a pause stops the run and marks it.
debugPauses.length = 0;
const stopped = call(noeta_debug_run, JSON.stringify({ source: debugSource, breakpoints: [2] }));
assert.equal(stopped.terminated, true);
assert.equal(stopped.stdout, '');

// The debug console (W2.5): an eval against the paused frame answers in the NEXT pause payload
// (the trampoline re-entry), full language — a call included — and the program stays paused.
debugPauses.length = 0;
debugCommands.push(
  { action: 'eval', expr: 'a + b', frame: 0 },
  { action: 'eval', expr: 'add(10, 20)', frame: 0 },
  { action: 'continue' },
);
const evalRun = call(noeta_debug_run, JSON.stringify({ source: debugSource, breakpoints: [2] }));
assert.equal(evalRun.exit_code, 0, JSON.stringify(evalRun));
assert.equal(evalRun.stdout, '3\n');
assert.equal(debugPauses.length, 3, JSON.stringify(debugPauses));
assert.equal(debugPauses[0].eval, null);
assert.deepEqual(debugPauses[1].eval, { ok: true, value: '3', ty: 'int' });
assert.deepEqual(debugPauses[2].eval, { ok: true, value: '30', ty: 'int' });
assert.equal(debugPauses[2].frames[0].name, 'add', 'still paused at the breakpoint');

// Scope precision: the breakpoint is on line 2 (`c = a + b`), which we stop *before* executing,
// so `c` is not yet stored — it must not appear as a local, and evaluating it is a clean
// undefined-name error, NOT "cannot apply `+` to int and unit" (the byte-offset scope bug).
debugPauses.length = 0;
debugCommands.push({ action: 'eval', expr: 'c', frame: 0 }, { action: 'continue' });
const scopeRun = call(noeta_debug_run, JSON.stringify({ source: debugSource, breakpoints: [2] }));
assert.equal(scopeRun.exit_code, 0);
const localNames = debugPauses[0].frames[0].locals.map((l) => l.name);
assert.ok(localNames.includes('a') && localNames.includes('b'), `params in scope: ${localNames}`);
assert.ok(!localNames.includes('c'), `c is not stored yet: ${localNames}`);
assert.equal(debugPauses[1].eval.ok, false, JSON.stringify(debugPauses[1].eval));
assert.match(debugPauses[1].eval.error, /cannot find `c`/);

// The JSPI pump (W3.1): two async fetches must genuinely OVERLAP — both start before either
// settles — and the run entry suspends/resumes through WebAssembly.promising.
if (JSPI) {
  const promisingRun = WebAssembly.promising(instance.exports.noeta_run_browser_async);
  const fanOut = [
    'use std.http.client',
    'use std.http.HttpError',
    'use std.task.{all}',
    '',
    'async fn run(): Result<void, HttpError> {',
    '    rs = all([',
    '        client.get_async("https://svc.test/a"),',
    '        client.get_async("https://svc.test/b"),',
    '    ])',
    '    echo "${rs[0]?.status()},${rs[1]?.status()}"',
    '    echo rs[0]?.body()',
    '    echo rs[1]?.body()',
    '    return Ok()',
    '}',
    'run().await?',
  ].join('\n');
  const encoded = new TextEncoder().encode(fanOut);
  const input = noeta_alloc(encoded.length);
  new Uint8Array(memory.buffer, input, encoded.length).set(encoded);
  const out = await promisingRun(input, encoded.length);
  const len = new DataView(memory.buffer).getUint32(out, true);
  const jspiRun = JSON.parse(new TextDecoder().decode(new Uint8Array(memory.buffer, out + 4, len)));
  noeta_free_result(out);

  assert.equal(jspiRun.compiled, true, JSON.stringify(jspiRun));
  assert.equal(jspiRun.exit_code, 0, JSON.stringify(jspiRun));
  assert.equal(jspiRun.stdout, '200,200\n/a-pong\n/b-pong\n');
  // The overlap proof: both requests were in flight before either settled.
  assert.deepEqual(events.slice(0, 2), ['start https://svc.test/a', 'start https://svc.test/b'], `events: ${events}`);
  assert.ok(events.slice(2).every((e) => e.startsWith('settle ')), `events: ${events}`);
  console.log('JSPI pump: two fetches overlapped (both started before either settled) ✓');
} else {
  console.log('JSPI pump: not supported by this runtime — serial fallback path is the one under test');
}

console.log('browser-engine smoke: all assertions passed ✓');
