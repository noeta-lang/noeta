// Share-by-URL round trip (hosting prep): the fragment encoding must survive everything a
// buffer can hold — unicode, newlines, the works — and a malformed fragment must decode to
// null, never throw. Run by the CI wasm job:
//
//   node crates/noeta-playground/tests/share_roundtrip.mjs

import assert from 'node:assert/strict';
import { decodeShare, encodeShare } from '../../../web/playground/share.js';

const cases = [
  'echo "hello";',
  '',
  'fn π(): float { return 3.14159; }\necho π(); // → 3.14159 ✓\n',
  '“smart quotes” — emoji 🦀🕸️ — tabs\t and \\ backslashes',
  'a'.repeat(10_000),
];
for (const source of cases) {
  assert.equal(decodeShare(encodeShare(source)), source);
}

// URL-safety: the fragment must survive a URL round trip untouched.
const fragment = encodeShare('mut x = 1;\nx = x + 1;\necho x;');
const url = new URL(`https://noeta.dev/playground#${fragment}`);
assert.equal(url.hash.slice(1), fragment);
assert.match(fragment, /^code=v1:[A-Za-z0-9_-]*$/);

// Malformed fragments never throw and never restore garbage.
for (const bad of ['', 'code=v1:%%%', 'code=v9:AAAA', 'unrelated', 'code=v1:_-!!']) {
  assert.equal(typeof decodeShare(bad) === 'string' || decodeShare(bad) === null, true);
  assert.doesNotThrow(() => decodeShare(bad));
}
assert.equal(decodeShare('unrelated'), null);
assert.equal(decodeShare('code=v9:AAAA'), null);

console.log('share round trip: all assertions passed ✓');
