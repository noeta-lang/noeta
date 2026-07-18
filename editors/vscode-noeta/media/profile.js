// The Noeta profile view, running inside the webview. Receives the parsed artifact from the
// extension host (`src/profile.js`) and renders it: a canvas flame graph + a sortable function
// table for a sampling profile (speedscope JSON), or the exact function table for an instrumenting
// profile. Everything is drawn with the editor's theme colors; the only styling not in
// `profile.css` is the flame cells' warm palette, which adapts to the active theme kind.

(function () {
  "use strict";

  const vscode = acquireVsCodeApi();
  const app = document.getElementById("app");

  /** @type {"sampling" | "instrument"} */
  let kind = "sampling";
  // Which profile of the artifact's `profiles` array is displayed: 0 = main; a program that
  // spawned worker isolates carries one additional profile per isolate.
  let profileIdx = 0;
  // What one stack weight means: sample counts (sampling) or exact nanoseconds (the instrumenting
  // call tree). Drives every weight rendering — flame tooltips, crumbs, and table cells.
  let weightUnit = "samples";
  function fmtBytes(n) {
    if (n < 1024) return fmtInt(n) + " B";
    if (n < 1024 * 1024) return (n / 1024).toFixed(1) + " KB";
    if (n < 1024 * 1024 * 1024) return (n / (1024 * 1024)).toFixed(1) + " MB";
    return (n / (1024 * 1024 * 1024)).toFixed(2) + " GB";
  }
  const fmtWeight = (n) =>
    weightUnit === "ns" ? fmtNs(n) : weightUnit === "bytes" ? fmtBytes(n) : fmtInt(n);
  const fmtWeightLong = (n) => (weightUnit === "samples" ? fmtInt(n) + " samples" : fmtWeight(n));
  let meta = { summary: "", program: "" };

  // ---- formatting --------------------------------------------------------------------------

  function fmtInt(n) {
    return n.toLocaleString();
  }

  function fmtPct(fraction) {
    return (fraction * 100).toFixed(1) + "%";
  }

  function fmtNs(ns) {
    if (ns < 1e3) return ns + "ns";
    if (ns < 1e6) return (ns / 1e3).toFixed(1) + "µs";
    if (ns < 1e9) return (ns / 1e6).toFixed(1) + "ms";
    return (ns / 1e9).toFixed(2) + "s";
  }

  function locText(file, line) {
    if (!file) return "";
    const base = file.split(/[\\/]/).pop();
    return line != null ? `${base}:${line}` : base;
  }

  function openSource(file, line, col) {
    if (file) {
      vscode.postMessage({ type: "openSource", file, line, col });
    }
  }

  // ---- theme ----------------------------------------------------------------------------------

  function themeKind() {
    const cls = document.body.className;
    if (cls.includes("vscode-high-contrast-light")) return "hc-light";
    if (cls.includes("vscode-high-contrast")) return "hc-dark";
    if (cls.includes("vscode-light")) return "light";
    return "dark";
  }

  /** A stable warm hue for a function name, so the same function keeps its color across runs. */
  function hueFor(name) {
    let h = 0;
    for (let i = 0; i < name.length; i++) {
      h = (h * 31 + name.charCodeAt(i)) >>> 0;
    }
    return 8 + (h % 44); // warm flame range: red-orange through amber
  }

  function cellStyle(name) {
    const hue = hueFor(name);
    switch (themeKind()) {
      case "light":
        return { fill: `hsl(${hue} 78% 74%)`, text: "rgba(0,0,0,0.85)" };
      case "hc-light":
        return { fill: "transparent", text: "var(--fg)" };
      case "hc-dark":
        return { fill: "transparent", text: "var(--fg)" };
      default:
        return { fill: `hsl(${hue} 58% 38%)`, text: "rgba(255,255,255,0.9)" };
    }
  }

  function cssVar(name, fallback) {
    return getComputedStyle(document.body).getPropertyValue(name).trim() || fallback;
  }

  // ---- skeleton -------------------------------------------------------------------------------

  function el(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text != null) node.textContent = text;
    return node;
  }

  function renderMessage(text) {
    app.replaceChildren(el("div", "message", text));
  }

  // ---- flame graph model ------------------------------------------------------------------

  /**
   * Merge the weighted stacks into a left-heavy call tree. Node: `{ frame, total, self,
   * children }` where `frame` indexes the shared frame table (-1 for the synthetic root).
   */
  function buildTree(samples, weights) {
    const root = { frame: -1, total: 0, self: 0, children: new Map() };
    for (let i = 0; i < samples.length; i++) {
      const chain = samples[i];
      const w = weights[i];
      root.total += w;
      let node = root;
      for (const frame of chain) {
        let child = node.children.get(frame);
        if (!child) {
          child = { frame, total: 0, self: 0, children: new Map() };
          node.children.set(frame, child);
        }
        child.total += w;
        node = child;
      }
      node.self += w;
    }
    // Freeze children into weight-sorted arrays — heaviest first, the "left heavy" layout.
    (function finish(node) {
      node.children = [...node.children.values()].sort((a, b) => b.total - a.total);
      for (const child of node.children) finish(child);
    })(root);
    return root;
  }

  // ---- flame graph view ---------------------------------------------------------------------

  function flameView(frames, tree) {
    const view = el("div", "view", null);
    const crumbs = el("div", "breadcrumbs");
    const wrap = el("div", "flame-wrap");
    const canvas = document.createElement("canvas");
    canvas.id = "flame-canvas";
    const tooltip = el("div", null);
    tooltip.id = "tooltip";
    wrap.append(canvas);
    view.append(crumbs, wrap, tooltip);

    const ROW = 22;
    /** The zoom path, root first; the last entry renders full-width with its subtree below. */
    let path = [tree];
    /** Hit-test rectangles from the last draw: {x, y, w, node, ancestor}. */
    let hits = [];

    const label = (node) =>
      node.frame < 0 ? `all — ${fmtWeightLong(tree.total)}` : frames[node.frame].name;

    function drawCell(ctx, node, x, y, w, width, dimmed) {
      const name = node.frame < 0 ? "all" : frames[node.frame].name;
      const style = cellStyle(name);
      const hc = themeKind().startsWith("hc");
      ctx.globalAlpha = dimmed ? 0.45 : 1;
      if (hc) {
        ctx.strokeStyle = cssVar("--vscode-contrastBorder", "#fff");
        ctx.strokeRect(x + 0.5, y + 1.5, Math.max(w - 1, 1), ROW - 3);
      } else {
        ctx.fillStyle = style.fill;
        ctx.fillRect(x, y + 1, Math.max(w - 0.7, 0.7), ROW - 2);
      }
      if (w > 24) {
        ctx.fillStyle = hc ? cssVar("--vscode-editor-foreground", "#fff") : style.text;
        ctx.font = `11px ${cssVar("--vscode-editor-font-family", "monospace")}`;
        ctx.textBaseline = "middle";
        const text = label(node);
        ctx.save();
        ctx.beginPath();
        ctx.rect(x + 4, y, w - 8, ROW);
        ctx.clip();
        ctx.fillText(text, x + 4, y + ROW / 2 + 1);
        ctx.restore();
      }
      ctx.globalAlpha = 1;
    }

    function depthOf(node) {
      let d = 0;
      for (const child of node.children) d = Math.max(d, depthOf(child));
      return d + 1;
    }

    function draw() {
      const width = wrap.clientWidth;
      if (width <= 0) return;
      const focus = path[path.length - 1];
      const rows = path.length + depthOf(focus) - 1;
      const height = rows * ROW;
      const dpr = window.devicePixelRatio || 1;
      canvas.width = Math.round(width * dpr);
      canvas.height = Math.round(height * dpr);
      canvas.style.height = height + "px";
      const ctx = canvas.getContext("2d");
      ctx.scale(dpr, dpr);
      ctx.clearRect(0, 0, width, height);
      hits = [];

      // Ancestors of the focus (including the root), full-width and dimmed — the zoom context.
      for (let i = 0; i < path.length - 1; i++) {
        drawCell(ctx, path[i], 0, i * ROW, width, width, true);
        hits.push({ x: 0, y: i * ROW, w: width, node: path[i], ancestor: true });
      }

      // The focus subtree, proportional under the focus row.
      const scale = width / focus.total;
      (function walk(node, x, depth) {
        const y = (path.length - 1 + depth) * ROW;
        const w = node.total * scale;
        if (w < 0.4) return;
        drawCell(ctx, node, x, y, w, width, false);
        hits.push({ x, y, w, node, ancestor: false });
        let cx = x;
        for (const child of node.children) {
          walk(child, cx, depth + 1);
          cx += child.total * scale;
        }
      })(focus, 0, 0);
    }

    function renderCrumbs() {
      crumbs.replaceChildren();
      path.forEach((node, i) => {
        if (i > 0) crumbs.append(el("span", "sep", "›"));
        const crumb = el("button", "crumb", node.frame < 0 ? "all" : frames[node.frame].name);
        crumb.title = label(node);
        crumb.addEventListener("click", () => zoomTo(path.slice(0, i + 1)));
        crumbs.append(crumb);
      });
    }

    function zoomTo(newPath) {
      path = newPath;
      renderCrumbs();
      draw();
    }

    function hitAt(event) {
      const rect = canvas.getBoundingClientRect();
      const x = event.clientX - rect.left;
      const y = event.clientY - rect.top;
      return hits.find((h) => x >= h.x && x < h.x + h.w && y >= h.y && y < h.y + ROW);
    }

    /** The root→node path inside the focused subtree, for zooming into a drawn cell. */
    function pathTo(target) {
      const focus = path[path.length - 1];
      const trail = [];
      (function search(node) {
        trail.push(node);
        if (node === target) return true;
        for (const child of node.children) {
          if (search(child)) return true;
        }
        trail.pop();
        return false;
      })(focus);
      return trail.length > 0 ? path.concat(trail.slice(1)) : path;
    }

    let clickTimer;
    canvas.addEventListener("click", (event) => {
      const hit = hitAt(event);
      if (!hit) return;
      const frame = hit.node.frame >= 0 ? frames[hit.node.frame] : null;
      if ((event.ctrlKey || event.metaKey) && frame) {
        openSource(frame.file, frame.line, frame.col);
        return;
      }
      // Delay the zoom one beat so a double-click (open source) doesn't zoom first.
      clearTimeout(clickTimer);
      clickTimer = setTimeout(() => {
        zoomTo(hit.ancestor ? path.slice(0, path.indexOf(hit.node) + 1) : pathTo(hit.node));
      }, 220);
    });
    canvas.addEventListener("dblclick", (event) => {
      clearTimeout(clickTimer);
      const hit = hitAt(event);
      const frame = hit && hit.node.frame >= 0 ? frames[hit.node.frame] : null;
      if (frame) openSource(frame.file, frame.line, frame.col);
    });
    window.addEventListener("keydown", (event) => {
      if (event.key === "Escape" && path.length > 1) zoomTo([tree]);
    });

    canvas.addEventListener("mousemove", (event) => {
      const hit = hitAt(event);
      if (!hit) {
        tooltip.style.display = "none";
        return;
      }
      const node = hit.node;
      tooltip.replaceChildren();
      tooltip.append(el("div", "tip-title", label(node)));
      if (node.frame >= 0) {
        const total = tree.total || 1;
        tooltip.append(
          el(
            "div",
            null,
            `total ${fmtWeightLong(node.total)} (${fmtPct(node.total / total)}) · ` +
              `self ${fmtWeight(node.self)} (${fmtPct(node.self / total)})`,
          ),
        );
        const frame = frames[node.frame];
        if (frame.file) {
          tooltip.append(el("div", "tip-loc", locText(frame.file, frame.line)));
        }
        tooltip.append(el("div", "tip-hint", "click: zoom · double / ctrl+click: open source · esc: reset"));
      }
      tooltip.style.display = "block";
      const pad = 12;
      const tw = tooltip.offsetWidth;
      const th = tooltip.offsetHeight;
      let tx = event.clientX + pad;
      let ty = event.clientY + pad;
      if (tx + tw > window.innerWidth - 4) tx = event.clientX - tw - pad;
      if (ty + th > window.innerHeight - 4) ty = event.clientY - th - pad;
      tooltip.style.left = tx + "px";
      tooltip.style.top = ty + "px";
    });
    canvas.addEventListener("mouseleave", () => {
      tooltip.style.display = "none";
    });

    new ResizeObserver(() => draw()).observe(wrap);
    // Redraw when the user switches color themes (VS Code swaps the body class).
    new MutationObserver(() => draw()).observe(document.body, {
      attributes: true,
      attributeFilter: ["class"],
    });

    renderCrumbs();
    // First draw happens via the ResizeObserver once the view is laid out.
    return view;
  }

  // ---- tables --------------------------------------------------------------------------------

  /**
   * A sortable table. `columns`: `{key, title, num, format}`; `rows`: objects with the column
   * keys plus `file`/`line`/`col` for navigation; `pctKey` draws the meter behind that column.
   */
  function tableView(columns, rows, defaultSort) {
    const view = el("div", "view");
    const wrap = el("div", "table-wrap");
    const table = document.createElement("table");
    const thead = document.createElement("thead");
    const tbody = document.createElement("tbody");
    table.append(thead, tbody);
    wrap.append(table);
    view.append(wrap);

    let sort = { key: defaultSort, desc: true };

    function renderHead() {
      const tr = document.createElement("tr");
      for (const col of columns) {
        const th = el("th", col.num ? "num" : null, col.title);
        if (sort.key === col.key) th.textContent += sort.desc ? " ↓" : " ↑";
        th.addEventListener("click", () => {
          sort = { key: col.key, desc: sort.key === col.key ? !sort.desc : true };
          render();
        });
        tr.append(th);
      }
      thead.replaceChildren(tr);
    }

    function renderBody() {
      const sorted = [...rows].sort((a, b) => {
        const va = a[sort.key];
        const vb = b[sort.key];
        const cmp = typeof va === "string" ? va.localeCompare(vb) : va - vb;
        return sort.desc ? -cmp : cmp;
      });
      tbody.replaceChildren(
        ...sorted.map((row) => {
          const tr = document.createElement("tr");
          for (const col of columns) {
            const td = el("td", col.className || (col.num ? "num" : null));
            if (col.pct) {
              const cell = el("span", "pct-cell");
              const bar = el("span", "pct-bar");
              bar.style.width = Math.min(row[col.key] * 100, 100).toFixed(1) + "%";
              cell.append(bar, el("span", "pct-num", fmtPct(row[col.key])));
              td.append(cell);
            } else {
              td.textContent = col.format ? col.format(row[col.key]) : String(row[col.key] ?? "");
              if (col.key === "loc") td.title = row.file ?? "";
            }
            tr.append(td);
          }
          tr.addEventListener("click", () => openSource(row.file, row.line, row.col));
          return tr;
        }),
      );
    }

    function render() {
      renderHead();
      renderBody();
    }
    render();
    return view;
  }

  /**
   * Aggregate the sampled stacks per *function* for the table: line-attributed leaf frames
   * (`fn:12`) fold back into their function, keeping the hottest line as the row's location.
   */
  function samplingRows(frames, samples, weights, total) {
    // A line-attributed leaf has no column and a label ending in its line number; strip that
    // suffix to recover the function. (Definition-site frames keep their column, so an anonymous
    // closure's `<anonymous>@file:3` identity is never stripped.)
    const fnName = (frame) => {
      const suffix = ":" + frame.line;
      return frame.col == null && frame.line != null && frame.name.endsWith(suffix)
        ? frame.name.slice(0, -suffix.length)
        : frame.name;
    };
    const rows = new Map();
    const rowFor = (frame) => {
      const fn = fnName(frame);
      const key = fn + " " + (frame.file ?? "");
      let row = rows.get(key);
      if (!row) {
        row = { fn, file: frame.file, line: frame.line, col: frame.col, loc: locText(frame.file, frame.line), self: 0, total: 0, hottest: -1 };
        rows.set(key, row);
      }
      return row;
    };
    for (let i = 0; i < samples.length; i++) {
      const chain = samples[i];
      const w = weights[i];
      const seen = new Set();
      for (const idx of chain) {
        const row = rowFor(frames[idx]);
        if (!seen.has(row)) {
          seen.add(row);
          row.total += w;
        }
      }
      if (chain.length > 0) {
        const leaf = frames[chain[chain.length - 1]];
        const row = rowFor(leaf);
        row.self += w;
        // The row navigates to the hottest sampled line of the function.
        if (row.self > row.hottest && leaf.line != null) {
          row.hottest = row.self;
          row.line = leaf.line;
          row.col = leaf.col;
          row.loc = locText(row.file, leaf.line);
        }
      }
    }
    return [...rows.values()].map((row) => ({ ...row, selfPct: row.self / (total || 1) }));
  }

  // ---- assembly -------------------------------------------------------------------------------

  function show(message) {
    app.replaceChildren();

    const header = el("div", "header");
    header.append(el("span", "title", message.meta.program));
    if (message.meta.summary) header.append(el("span", "meta", message.meta.summary));
    // A program that spawned isolates has one profile per worker thread — offer a picker.
    const allProfiles = message.profile.profiles ?? [];
    if (allProfiles.length > 1) {
      const picker = el("select", "profile-picker");
      allProfiles.forEach((p, i) => {
        const option = document.createElement("option");
        option.value = String(i);
        option.textContent = p.name || (i === 0 ? "main" : `profile ${i}`);
        if (i === profileIdx) option.selected = true;
        picker.append(option);
      });
      picker.addEventListener("change", () => {
        profileIdx = Number(picker.value);
        show(message);
      });
      header.append(picker);
    }
    app.append(header);

    if (message.kind === "instrument") {
      const functions = message.profile.functions ?? [];
      if (functions.length === 0) {
        app.append(el("div", "message", "The profile contains no functions."));
        return;
      }
      weightUnit = "ns"; // instrument weights are exact nanoseconds
      const totalSelf = functions.reduce((acc, f) => acc + f.self_ns, 0);
      const rows = functions.map((f) => ({
        fn: f.name,
        file: f.file,
        line: f.line,
        loc: locText(f.file, f.line),
        calls: f.calls,
        self: f.self_ns,
        total: f.total_ns,
        selfPct: f.self_ns / (totalSelf || 1),
      }));
      const view = tableView(
        [
          { key: "fn", title: "Function", className: "fn" },
          { key: "loc", title: "Location", className: "loc" },
          { key: "calls", title: "Calls", num: true, format: fmtInt },
          { key: "self", title: "Self", num: true, format: fmtNs },
          { key: "total", title: "Total", num: true, format: fmtNs },
          { key: "selfPct", title: "Self %", num: true, pct: true },
        ],
        rows,
        "self",
      );
      // The artifact now also carries the EXACT call tree (speedscope-shaped, ns-weighted): render
      // the flame graph beside the table, tabbed like the sampling view. The Functions tab is the
      // exact instrument table (true call counts + recursion-correct totals). An ISOLATE profile
      // (picker index > 0) has stacks but no row in the main function table — it renders the
      // generic flame + leaf-table pair instead.
      const frames = message.profile.shared?.frames ?? [];
      const chosen = allProfiles[profileIdx];
      if (profileIdx > 0 && frames.length && chosen?.samples?.length) {
        const body = el("div", "sampling-body");
        app.append(body);
        renderSampling(body, frames, chosen.samples, chosen.weights ?? [], null);
        return;
      }
      if (frames.length && chosen?.samples?.length) {
        const body = el("div", "sampling-body");
        app.append(body);
        const tree = buildTree(chosen.samples, chosen.weights ?? []);
        tabbed(body, flameView(frames, tree), view);
      } else {
        view.classList.add("active");
        app.append(view);
      }
      return;
    }

    // Sampling: speedscope JSON — sampled profiles over a shared frame table (main first, one
    // more per isolate).
    const frames = message.profile.shared?.frames ?? [];
    const profile = allProfiles[profileIdx];
    weightUnit =
      profile?.unit === "nanoseconds" ? "ns" : profile?.unit === "bytes" ? "bytes" : "samples";
    const allSamples = profile?.samples ?? [];
    const allWeights = profile?.weights ?? [];
    if (allSamples.length === 0) {
      app.append(el("div", "message", "The profile contains no samples (the program may have run too briefly to sample)."));
      return;
    }
    const body = el("div", "sampling-body");
    app.append(body);
    renderSampling(body, frames, allSamples, allWeights, meta.focus || null);
  }

  /** Whether a frame belongs to the function `name` (`--lines` leaf frames are `fn:line`). */
  function frameIs(frame, name) {
    return frame.name === name || frame.name.startsWith(name + ":");
  }

  /**
   * Render the sampling views into `container`, optionally **focused** (a profile slice, ide-ui
   * U3): with a focus function, every sample stack is re-rooted at its first occurrence of that
   * function and stacks that never pass through it are dropped — the flame graph and table show
   * only the part of the run the user asked about, with a bar reporting the slice's share and a
   * one-click way back to the whole run.
   */
  function renderSampling(container, frames, allSamples, allWeights, focus) {
    container.replaceChildren();
    let samples = allSamples;
    let weights = allWeights;
    if (focus) {
      samples = [];
      weights = [];
      for (let i = 0; i < allSamples.length; i++) {
        const at = allSamples[i].findIndex((idx) => frameIs(frames[idx], focus));
        if (at >= 0) {
          samples.push(allSamples[i].slice(at));
          weights.push(allWeights[i]);
        }
      }
      const kept = weights.reduce((a, b) => a + b, 0);
      const total = allWeights.reduce((a, b) => a + b, 0);
      const bar = el("div", "focus-bar");
      bar.append(
        el(
          "span",
          "focus-text",
          `⊙ focused on ${focus} — ${fmtInt(kept)} of ${fmtInt(total)} samples (${fmtPct(kept / (total || 1))})`,
        ),
      );
      const clear = el("button", "focus-clear", "Show whole run");
      clear.addEventListener("click", () =>
        renderSampling(container, frames, allSamples, allWeights, null),
      );
      bar.append(clear);
      container.append(bar);
      if (samples.length === 0) {
        container.append(
          el(
            "div",
            "message",
            `No samples pass through ${focus} — it may run too briefly to sample or be unreached on this input.`,
          ),
        );
        return;
      }
    }

    const tree = buildTree(samples, weights);
    const flame = flameView(frames, tree);
    const table = tableView(
      [
        { key: "fn", title: "Function", className: "fn" },
        { key: "loc", title: "Hottest Line", className: "loc" },
        { key: "self", title: "Self", num: true, format: fmtWeight },
        { key: "total", title: "Total", num: true, format: fmtWeight },
        { key: "selfPct", title: "Self %", num: true, pct: true },
      ],
      samplingRows(frames, samples, weights, tree.total),
      "self",
    );

    tabbed(container, flame, table);
  }

  /** Append the Flame Graph | Functions tab pair to `container`, flame active first. */
  function tabbed(container, flame, table) {
    const tabs = el("div", "tabs");
    const views = { flame, table };
    const buttons = {};
    for (const [name, title] of [
      ["flame", "Flame Graph"],
      ["table", "Functions"],
    ]) {
      const tab = el("button", "tab", title);
      tab.addEventListener("click", () => {
        for (const key of Object.keys(views)) {
          views[key].classList.toggle("active", key === name);
          buttons[key].classList.toggle("active", key === name);
        }
      });
      buttons[name] = tab;
      tabs.append(tab);
    }
    container.append(tabs, flame, table);
    buttons.flame.click();
  }

  window.addEventListener("message", (event) => {
    const message = event.data;
    if (message.type === "profile") {
      kind = message.kind;
      meta = message.meta;
      profileIdx = 0;
      show(message);
    } else if (message.type === "error") {
      renderMessage(message.message);
    }
  });

  renderMessage("Loading profile…");
  vscode.postMessage({ type: "ready" });
})();
