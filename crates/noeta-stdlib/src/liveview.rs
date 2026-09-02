//! The bundled LiveView client shim — the browser half of the view/diff push
//! protocol ([`crate::reactive::view_ctx_method_dispatch`]).
//!
//! Deliberately **not** a component framework: a ~50-line patch-applier. The server renders the
//! page however it likes; the shim only keeps the marked leaves current. It is shipped *in the
//! language* — `server.liveview_js()` returns this source — so the client is versioned with the
//! runtime that speaks the protocol, needs no asset pipeline, and stays deterministic under the
//! sandbox (it is a pure string).
//!
//! # Conventions
//!
//! - `<span data-live="count">` — rendered with the current value of the `count` binding on every
//!   snapshot/patch (strings verbatim, everything else `JSON.stringify`ed).
//! - `<button data-live-click="increment">` — a click sends `{"type":"event","name":"increment"}`
//!   over the socket; the server session decides what an event means (the client→server half of
//!   the protocol is app-defined — this envelope is just the shim's convention).
//! - `window.noetaLive` — `{ state, send }` for programmatic use.
//! - The socket path defaults to `/ws`; set `window.NOETA_LIVE_PATH` before the script to change.
//! - On close it reconnects (1s backoff); the server sends a fresh snapshot on every connect, so
//!   recovery is total-state, never replayed patches.
//! - HMR events ride the same socket: `reload` (a hot swap landed — the server
//!   pushes it and closes; the page reloads into fresh markup over the preserved signal state)
//!   and `error` (a rejected edit under `--watch` — rendered diagnostics in a full-screen
//!   overlay, cleared by the next good frame). Unknown frame types are ignored, so an older
//!   shim survives a newer server.
pub const LIVEVIEW_JS: &str = r#"// noeta LiveView client (bundled; see std.http.server docs)
(() => {
  "use strict";
  const path = window.NOETA_LIVE_PATH || "/ws";
  const state = {};
  let ws = null;
  const render = (name) => {
    const value = state[name];
    const text = typeof value === "string" ? value : JSON.stringify(value);
    document.querySelectorAll('[data-live="' + name + '"]').forEach((el) => {
      el.textContent = text;
    });
  };
  const apply = (values) => {
    for (const name of Object.keys(values)) {
      state[name] = values[name];
      render(name);
    }
  };
  const overlay = (message) => {
    let el = document.getElementById("noeta-live-overlay");
    if (message === null) {
      if (el) el.remove();
      return;
    }
    if (!el) {
      el = document.createElement("pre");
      el.id = "noeta-live-overlay";
      el.style.cssText =
        "position:fixed;inset:0;margin:0;padding:2rem;overflow:auto;z-index:2147483647;" +
        "background:rgba(24,4,4,.94);color:#ffb4b4;font:14px/1.5 monospace;white-space:pre-wrap";
      document.body.appendChild(el);
    }
    el.textContent = message;
  };
  const handle = (frame) => {
    if (frame.type === "snapshot") { overlay(null); apply(frame.values); }
    else if (frame.type === "patch") { overlay(null); apply(frame.changes); }
    else if (frame.type === "reload") location.reload();
    else if (frame.type === "error") { overlay(frame.message); console.error("noeta live:", frame.message); }
  };
  const connect = () => {
    const scheme = location.protocol === "https:" ? "wss://" : "ws://";
    ws = new WebSocket(scheme + location.host + path);
    ws.onmessage = (e) => handle(JSON.parse(e.data));
    ws.onclose = () => setTimeout(connect, 1000);
  };
  const send = (name) => {
    if (ws && ws.readyState === 1) ws.send(JSON.stringify({ type: "event", name }));
  };
  document.addEventListener("click", (e) => {
    const el = e.target.closest("[data-live-click]");
    if (el) send(el.dataset.liveClick);
  });
  window.noetaLive = { state, send };
  connect();
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shim_speaks_the_l1_protocol_and_conventions() {
        // The frame types the view emits and the DOM conventions the docs promise — if one of
        // these markers drifts, the docs/protocol changed and this pin makes it deliberate.
        for marker in [
            "\"snapshot\"",
            "\"patch\"",
            "data-live",
            "data-live-click",
            "noetaLive",
            "NOETA_LIVE_PATH",
            "\"reload\"",         // the HMR swap event (L3)
            "\"error\"",          // the rejected-edit event (L3)
            "noeta-live-overlay", // the error overlay element
        ] {
            assert!(
                LIVEVIEW_JS.contains(marker),
                "shim lost its `{marker}` marker"
            );
        }
    }
}
