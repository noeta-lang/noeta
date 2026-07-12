// The playground page (P-WASM W2.2): editor glue + worker lifecycle. Deliberately
// dependency-free — no bundler, no CDN — so the page is a pair of static files next to the
// engine artifact, servable from anywhere (see README.md).

import { decodeShare, encodeShare } from './share.js';

const RUN_TIMEOUT_MS = 5000;

const editor = document.getElementById('editor');
const output = document.getElementById('output');
const diagnosticsPane = document.getElementById('diagnostics');
const statusLine = document.getElementById('status');
const examplePicker = document.getElementById('examples');

const EXAMPLES = {
  hello: `echo "hello from Noeta in your browser";\n`,
  fibonacci: `fn fib(n: int): int {
  if n < 2 { return n; }
  return fib(n - 1) + fib(n - 2);
}

echo fib(20);
`,
  'seeded random': `// The playground runs the deterministic sandbox: the same seed
// always produces the same stream — reload and see.
use std.random;

random.seed(42);
for i in [1, 2, 3] {
  echo random.int(0, 100);
}
`,
  'stack trace': `fn inner(): int {
  panic("something went wrong");
}

fn outer(): int {
  return inner();
}

echo outer();
`,
  'type error': `// The checker runs as you'd expect: \`mut\` is stably typed, so this
// reassignment is a compile-time error — hit Check or Run.
mut count = 1;
count = "not a number";
`,
  'http fetch': `// Tick "real host" above: the request leaves your browser (subject to CORS).
// In the default sandbox the same code gets the deterministic pure responder.
use std.http.client

r = client.get("https://api.github.com/zen")
echo r.status()
echo r.body()
`,
};

// --- Worker lifecycle: one engine worker, terminated and respawned on timeout (the runaway
// guard — the VM has no fuel counter by design). ---

let worker = null;
let nextId = 1;
const pending = new Map(); // id -> {resolve, timer}

function spawnWorker() {
  worker = new Worker('./worker.js');
  worker.onmessage = (event) => {
    if ('ready' in event.data) {
      setStatus(event.data.ready ? 'ready' : `engine failed to load: ${event.data.error}`);
      return;
    }
    const { id, ok, result, error } = event.data;
    const entry = pending.get(id);
    if (!entry) return;
    pending.delete(id);
    clearTimeout(entry.timer);
    entry.resolve(ok ? { ok, result } : { ok, error });
  };
}

function request(op, source) {
  return new Promise((resolve) => {
    const id = nextId++;
    const timer = setTimeout(() => {
      pending.delete(id);
      // Terminate the wedged engine and start fresh — the whole point of running it in a worker.
      worker.terminate();
      for (const [, other] of pending) {
        clearTimeout(other.timer);
        other.resolve({ ok: false, error: 'engine restarted' });
      }
      pending.clear();
      spawnWorker();
      resolve({ ok: false, error: `no result after ${RUN_TIMEOUT_MS / 1000}s — the program was stopped (infinite loop?)` });
    }, RUN_TIMEOUT_MS);
    pending.set(id, { resolve, timer });
    worker.postMessage({ id, op, source });
  });
}

// --- UI wiring ---

function setStatus(text) {
  statusLine.textContent = text;
}

function renderDiagnostics(diagnostics) {
  diagnosticsPane.replaceChildren();
  for (const d of diagnostics) {
    const line = document.createElement('div');
    line.className = `diagnostic ${d.severity}`;
    const where = document.createElement('button');
    where.className = 'loc';
    where.textContent = `${d.line}:${d.column}`;
    where.title = 'jump to location';
    where.addEventListener('click', () => jumpTo(d.byte_start, d.byte_end));
    line.append(where, ` [${d.code}] ${d.message}`);
    if (d.help) {
      const help = document.createElement('div');
      help.className = 'help';
      help.textContent = `help: ${d.help}`;
      line.append(help);
    }
    diagnosticsPane.append(line);
  }
}

function jumpTo(start, end) {
  editor.focus();
  editor.setSelectionRange(start, Math.max(end, start + 1));
}

async function doCheck() {
  setStatus('checking…');
  const reply = await request('check', editor.value);
  if (!reply.ok) { setStatus(reply.error); return; }
  renderDiagnostics(reply.result.diagnostics);
  output.textContent = '';
  setStatus(reply.result.diagnostics.length === 0 ? 'no diagnostics ✓' : `${reply.result.diagnostics.length} diagnostic(s)`);
}

async function doRun() {
  setStatus('running…');
  output.textContent = '';
  const op = document.getElementById('realhost').checked ? 'run-browser' : 'run';
  const reply = await request(op, editor.value);
  if (!reply.ok) { setStatus(reply.error); return; }
  const r = reply.result;
  renderDiagnostics(r.diagnostics);
  if (!r.compiled) {
    setStatus(r.error ?? 'did not compile');
    return;
  }
  output.textContent = r.stdout + (r.trace ? `\n${r.trace}` : '');
  setStatus(`exit ${r.exit_code}`);
}

async function doFmt() {
  setStatus('formatting…');
  const reply = await request('fmt', editor.value);
  if (!reply.ok) { setStatus(reply.error); return; }
  if (reply.result.ok) {
    editor.value = reply.result.formatted;
    setStatus('formatted ✓');
  } else {
    setStatus(reply.result.error);
  }
}

async function doShare() {
  const url = new URL(location.href);
  url.hash = encodeShare(editor.value);
  history.replaceState(null, '', url);
  try {
    await navigator.clipboard.writeText(url.href);
    setStatus('share link copied ✓');
  } catch {
    // Clipboard needs a secure context / permission; the address bar already holds the link.
    setStatus('share link is in the address bar');
  }
}

document.getElementById('run').addEventListener('click', doRun);
document.getElementById('check').addEventListener('click', doCheck);
document.getElementById('fmt').addEventListener('click', doFmt);
document.getElementById('share').addEventListener('click', doShare);
editor.addEventListener('keydown', (event) => {
  if ((event.ctrlKey || event.metaKey) && event.key === 'Enter') {
    event.preventDefault();
    doRun();
  }
});

for (const name of Object.keys(EXAMPLES)) {
  const option = document.createElement('option');
  option.value = name;
  option.textContent = name;
  examplePicker.append(option);
}
examplePicker.addEventListener('change', () => {
  editor.value = EXAMPLES[examplePicker.value];
  output.textContent = '';
  diagnosticsPane.replaceChildren();
});

// A shared link restores its buffer; otherwise start on the hello example.
editor.value = decodeShare(location.hash.slice(1)) ?? EXAMPLES.hello;
setStatus('loading engine…');
spawnWorker();
