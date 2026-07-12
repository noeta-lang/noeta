// Share-by-URL (hosting prep): the buffer travels in the URL fragment — no backend, works on
// any static host, and the fragment never reaches the server. Pure functions, unit-tested in
// node (tests/share_roundtrip.mjs); `app.js` wires them to the Share button and page load.
//
// Encoding: UTF-8 bytes → base64url (RFC 4648 §5, no padding), prefixed with a version so a
// future compressed encoding can coexist (`#code=v1:...`). A few-hundred-line program makes a
// URL of a few KB — within every browser's limits, if not a thing of beauty.

const PREFIX = 'code=v1:';

/// The URL fragment (without `#`) carrying `source`.
export function encodeShare(source) {
  const bytes = new TextEncoder().encode(source);
  let binary = '';
  for (const byte of bytes) binary += String.fromCharCode(byte);
  const base64url = btoa(binary).replaceAll('+', '-').replaceAll('/', '_').replace(/=+$/, '');
  return PREFIX + base64url;
}

/// The source carried by a URL fragment (without `#`), or `null` if it carries none / is not a
/// recognized share encoding (a malformed fragment must never break page load).
export function decodeShare(fragment) {
  if (!fragment.startsWith(PREFIX)) return null;
  const base64url = fragment.slice(PREFIX.length);
  try {
    const binary = atob(base64url.replaceAll('-', '+').replaceAll('_', '/'));
    const bytes = Uint8Array.from(binary, (ch) => ch.charCodeAt(0));
    return new TextDecoder('utf-8', { fatal: true }).decode(bytes);
  } catch {
    return null;
  }
}
