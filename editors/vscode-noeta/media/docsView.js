// The Docs sidebar view (docs-browser-ui arc, round 2). A single webview holding the filter input
// AND the tree, so the filter lives in the same pane as what it filters. Talks to the extension
// over postMessage: it asks for a node's children (lazily, on expand) and for search hits (while
// filtering), and reports clicks back (open a page, go to source).
//
// While the filter box is empty it shows the lazy tree; with text it shows a FLAT ranked list of
// matches — so a match is never hidden inside a collapsed ancestor (the flaw of trying to drive a
// native tree's expansion state from a filter).

(function () {
  const vscode = acquireVsCodeApi();
  const tree = document.getElementById("tree");
  const input = document.getElementById("filter");
  const count = document.getElementById("count");
  const clearBtn = document.getElementById("clear");
  const collapseBtn = document.getElementById("collapse");

  // ---- Inline SVG icons (16px, currentColor) so the view is self-contained (no font/CDN). ----
  const I = {
    chevron: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><path d="M6 4l4 4-4 4"/></svg>',
    book: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><path d="M2.5 3.5A1 1 0 013.5 3H7v10H3.5a1 1 0 01-1-1zM13.5 3.5A1 1 0 0012.5 3H9v10h3.5a1 1 0 001-1z"/></svg>',
    file: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><path d="M4 2h5l3 3v9H4z"/><path d="M9 2v3h3M6 8h4M6 10.5h4"/></svg>',
    box: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><path d="M8 2l5 2.5v7L8 14 3 11.5v-7z"/><path d="M3 4.5L8 7l5-2.5M8 7v7"/></svg>',
    func: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3"><path d="M10 3.5C8.5 3.5 8 4.5 7.8 6L7 12c-.2 1.2-.7 2-1.8 2M5 7.5h4.5"/></svg>',
    braces: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><path d="M6 3c-1.5 0-2 .8-2 2v1c0 1-.5 1.5-1.5 2 1 .5 1.5 1 1.5 2v1c0 1.2.5 2 2 2M10 3c1.5 0 2 .8 2 2v1c0 1 .5 1.5 1.5 2-1 .5-1.5 1-1.5 2v1c0 1.2-.5 2-2 2"/></svg>',
    layers: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><path d="M8 2l6 3-6 3-6-3z"/><path d="M2 8l6 3 6-3M2 11l6 3 6-3"/></svg>',
    dot: '<svg viewBox="0 0 16 16" fill="currentColor"><circle cx="8" cy="8" r="2.4"/></svg>',
    tag: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><path d="M3 3h5l5 5-5 5-5-5z"/><circle cx="6" cy="6" r="1" fill="currentColor" stroke="none"/></svg>',
    plug: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><circle cx="8" cy="8" r="5.5"/><path d="M8 4.5v7M4.5 8h7"/></svg>',
    note: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><path d="M3 3h10v10H3z"/><path d="M5.5 6h5M5.5 8.5h5M5.5 11h3"/></svg>',
    close: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3"><path d="M4 4l8 8M12 4l-8 8"/></svg>',
    collapse: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3"><path d="M4 6l4-3 4 3M4 10l4 3 4-3"/></svg>',
  };
  function iconFor(kind) {
    switch (kind) {
      case "root": case "guide": return I.book;
      case "module": return I.file;
      case "package": return I.box;
      case "function": case "method": return I.func;
      case "struct": case "class": return I.braces;
      case "enum": return I.layers;
      case "variant": return I.dot;
      case "field": return I.tag;
      case "interface": case "trait": return I.plug;
      case "section": return I.note;
      default: return I.dot;
    }
  }

  function el(tag, cls, html) {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (html != null) n.innerHTML = html;
    return n;
  }
  function esc(s) {
    return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  }

  // Pending children requests: id → the child-container element to fill when the reply arrives.
  const pending = new Map();

  // ---- Hover summary (linger on a row → its page's first @doc paragraph, native-hover style) ----
  const HOVER_DELAY = 500; // ms — the native tree/hover linger
  const summaries = new Map(); // node id → summary text ("" = the page has no prose)
  let tip = null; // the visible tooltip element
  let hoverTimer = 0;
  let hoverId = null; // the node id whose summary fetch is in flight for the hovered row

  function hideTip() {
    clearTimeout(hoverTimer);
    hoverId = null;
    if (tip) {
      tip.remove();
      tip = null;
    }
  }

  /** Show `text` in a hover widget anchored under `row` (above when there is no room below). */
  function showTip(row, text) {
    if (tip) {
      tip.remove();
      tip = null;
    }
    if (!text || !row.isConnected) return;
    tip = el("div", "doc-tooltip", esc(text));
    document.body.append(tip);
    const r = row.getBoundingClientRect();
    const pad = 4;
    const left = Math.max(pad, Math.min(r.left + 20, window.innerWidth - tip.offsetWidth - pad));
    let top = r.bottom + 3;
    if (top + tip.offsetHeight > window.innerHeight - pad) top = r.top - tip.offsetHeight - 3;
    tip.style.left = `${left}px`;
    tip.style.top = `${top}px`;
  }

  /** Arm the linger timer for `row`; fires a cached summary immediately or asks the host once. */
  function armHover(row, node) {
    hideTip();
    if (!node.hasPage) return; // nothing to summarize
    hoverTimer = setTimeout(() => {
      if (summaries.has(node.id)) {
        showTip(row, summaries.get(node.id));
      } else {
        hoverId = node.id;
        vscode.postMessage({ type: "summary", id: node.id });
      }
    }, HOVER_DELAY);
  }

  // Any scroll invalidates the tooltip's anchor.
  window.addEventListener("scroll", hideTip, true);

  /** A tree row for `node`, plus its (initially empty, structurally-indented) child container. */
  function makeRow(node) {
    const wrap = el("div", "node");
    const row = el("div", `row kind-${node.kind}`);
    row.dataset.id = node.id;

    const twisty = el("span", node.expandable ? "twisty" : "twisty leaf", I.chevron);
    const icon = el("span", "kind-icon", iconFor(node.kind));
    const label = el("span", "label", esc(node.title));
    row.append(twisty, icon, label);
    if (node.detail) {
      row.append(el("span", "detail", esc(node.detail)));
    }

    const kids = el("div", "children");
    wrap.append(row, kids);

    row.addEventListener("mouseenter", () => armHover(row, node));
    row.addEventListener("mouseleave", hideTip);
    row.addEventListener("click", (e) => {
      hideTip();
      select(row);
      // A click on the twisty (or anywhere on an expandable non-page row) toggles; otherwise open.
      const onTwisty = e.target.closest(".twisty");
      if (node.expandable && (onTwisty || !node.hasPage)) {
        toggle(row, kids, node);
      } else if (node.hasPage) {
        vscode.postMessage({ type: "open", id: node.id });
      }
    });
    return wrap;
  }

  function toggle(row, kids, node) {
    const open = row.classList.toggle("expanded");
    kids.classList.toggle("open", open);
    if (open && !kids.dataset.loaded) {
      kids.dataset.loaded = "1";
      kids.append(el("div", "hint", "Loading…"));
      pending.set(node.id, kids);
      vscode.postMessage({ type: "children", id: node.id });
    }
  }

  let selected = null;
  function select(row) {
    if (selected) selected.classList.remove("selected");
    selected = row;
    row.classList.add("selected");
    // Hold focus on the tree container so the selection shows the ACTIVE native styling
    // (:focus-within in the stylesheet); it dims to the inactive state when focus moves to
    // the filter or leaves the view — exactly the native list behavior.
    tree.focus({ preventScroll: true });
  }

  function renderRoots(nodes) {
    hideTip(); // the hovered row may be replaced under the cursor
    tree.textContent = "";
    for (const n of nodes) tree.append(makeRow(n));
  }

  function fillChildren(id, nodes) {
    const kids = pending.get(id);
    if (!kids) return;
    pending.delete(id);
    kids.textContent = "";
    if (!nodes.length) {
      kids.append(el("div", "hint", "(empty)"));
      return;
    }
    for (const n of nodes) kids.append(makeRow(n));
  }

  // ---- Filter: a flat, ranked results list -------------------------------------------------- */
  function renderResults(query, hits) {
    hideTip(); // the hovered row may be replaced under the cursor
    tree.textContent = "";
    count.textContent = `${hits.length} match${hits.length === 1 ? "" : "es"}`;
    if (!hits.length) {
      tree.append(el("div", "hint", `No matches for “${esc(query)}”.`));
      return;
    }
    for (const hit of hits) {
      const row = el("div", `row result kind-${hit.kind}`);
      row.append(el("span", "kind-icon", iconFor(hit.kind)));
      const body = el("div", "result-body");
      const title = el("div", "result-title");
      title.append(el("span", "label", highlight(hit.title, query)));
      const crumb = crumbFor(hit.id);
      if (crumb) title.append(el("span", "crumb", esc(crumb)));
      body.append(title);
      if (hit.snippet) body.append(el("div", "snippet", esc(hit.snippet)));
      row.append(body);
      row.addEventListener("click", () => {
        select(row);
        vscode.postMessage({ type: "open", id: hit.id });
      });
      tree.append(row);
    }
  }

  // The corpus a hit lives in, from its id root — a light breadcrumb next to the title.
  function crumbFor(id) {
    const root = id.split("/")[0];
    return { project: "Project", deps: "Dependencies", guide: "Guide", api: "API" }[root] || "";
  }

  function highlight(title, query) {
    const q = query.trim().toLowerCase();
    if (!q) return esc(title);
    const lower = title.toLowerCase();
    let out = "", from = 0, at;
    while ((at = lower.indexOf(q, from)) !== -1) {
      out += esc(title.slice(from, at)) + '<span class="hl">' + esc(title.slice(at, at + q.length)) + "</span>";
      from = at + q.length;
    }
    return out + esc(title.slice(from));
  }

  function showMessage(text) {
    tree.textContent = "";
    tree.append(el("div", "hint", text));
  }

  // ---- Filter input wiring ------------------------------------------------------------------ */
  let debounce;
  function onFilter() {
    clearTimeout(debounce);
    const q = input.value.trim();
    vscode.setState({ query: input.value });
    if (!q) {
      count.textContent = "";
      vscode.postMessage({ type: "roots" }); // back to the tree
      return;
    }
    debounce = setTimeout(() => vscode.postMessage({ type: "search", query: q }), 150);
  }
  input.addEventListener("input", onFilter);
  input.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && input.value) {
      input.value = "";
      onFilter();
    }
  });
  clearBtn.innerHTML = I.close;
  collapseBtn.innerHTML = I.collapse;
  clearBtn.addEventListener("click", () => {
    input.value = "";
    input.focus();
    onFilter();
  });
  collapseBtn.addEventListener("click", () => {
    input.value = "";
    count.textContent = "";
    vscode.postMessage({ type: "roots" });
  });

  window.addEventListener("message", (event) => {
    const msg = event.data;
    switch (msg.type) {
      case "roots":
        count.textContent = "";
        renderRoots(msg.nodes || []);
        break;
      case "children":
        fillChildren(msg.id, msg.nodes || []);
        break;
      case "results":
        // Ignore a stale reply if the box changed since.
        if (input.value.trim() === msg.query) renderResults(msg.query, msg.hits || []);
        break;
      case "message":
        count.textContent = "";
        showMessage(msg.text || "");
        break;
      case "summary": {
        summaries.set(msg.id, msg.text || "");
        // Show only if the cursor still rests on the row that asked.
        if (hoverId === msg.id) {
          hoverId = null;
          const row = tree.querySelector(`.row[data-id="${CSS.escape(msg.id)}"]`);
          if (row && row.matches(":hover")) showTip(row, msg.text || "");
        }
        break;
      }
    }
  });

  // Restore the filter text across a hide/show, then announce readiness.
  const prev = vscode.getState();
  if (prev && prev.query) input.value = prev.query;
  vscode.postMessage({ type: "ready" });
})();
