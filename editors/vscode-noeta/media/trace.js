// The dedicated trace view (trace-view arc). Renders the structured `noeta/traceTree` answer —
// the role-aware static call-graph walk — as:
//   * a **boundary rail**: one chip per reached (role, function), hue-hashed per role; clicking a
//     chip highlights every path leading to that boundary (click again to clear), double-click
//     jumps to its source;
//   * interactive, collapsible **call trees** whose indent rails are tinted by the parent's role,
//     so the architectural layers read as colored bands; every row click jumps to source;
//   * the walk's honesty markers made visible: `[dynamic]` / `[external]` leaves italic-dim,
//     passed-as-value references badged, recursion marked ↻, truncation an explicit note;
//   * a "low-level calls" toggle — trivial dynamic/external leaves (no roles, no children) are
//     hidden by default so the architectural shape stays foregrounded;
//   * a **Lanes** view (the header switcher): the call trees collapsed to the role graph — one
//     swimlane column per role, cards for role-bearing functions, SVG edges between the nearest
//     bearers (non-role intermediates collapsed; passed-as-value chains dashed) — the layered
//     architecture diagram, derived, never hand-drawn.

(function () {
  const vscode = acquireVsCodeApi();
  const app = document.getElementById("app");

  let trace = null;
  let showLowLevel = false;
  /** The active boundary filter (`role target`), or null. */
  let activeBoundary = null;
  /** "tree" (call trees) or "lanes" (the role swimlane graph). */
  let viewMode = "tree";
  /** Redraw hook for the lanes' SVG edges (layout-dependent), re-run on window resize. */
  let redrawEdges = null;

  function el(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text != null) node.textContent = text;
    return node;
  }

  /** A stable, theme-agnostic accent per role name: hash → hue, fixed sat/lightness. */
  function roleColor(role) {
    let h = 0;
    for (let i = 0; i < role.length; i++) h = (h * 31 + role.charCodeAt(i)) >>> 0;
    const hue = h % 360;
    const dark = !document.body.classList.contains("vscode-light");
    return `hsl(${hue} ${dark ? 55 : 65}% ${dark ? 65 : 38}%)`;
  }

  const boundaryKey = (b) => b.role + " " + b.target;

  /** Whether a node is a trivial low-level leaf (hidden unless the toggle is on). */
  function isLowLevel(node) {
    return (
      (node.dynamic || node.external) &&
      node.children.length === 0 &&
      node.roles.length === 0 &&
      node.kind !== "root"
    );
  }

  /** Whether `node` (or any descendant) matches the active boundary. */
  function matchesBoundary(node, role, target) {
    return node.name === target && node.roles.includes(role);
  }

  function basename(uri) {
    return uri.split("/").pop();
  }

  function openLoc(loc) {
    if (loc) vscode.postMessage({ type: "source", uri: loc.uri, line: loc.line, character: loc.character });
  }

  // ---- Tree rendering -------------------------------------------------------------------------

  /** Render one node; returns { el, onPath } where onPath = this subtree reaches the boundary. */
  function renderNode(node, railColor, boundary) {
    const wrap = el("div", "node");
    const row = el("div", "row");
    if (node.dynamic) row.classList.add("dynamic");
    if (node.external) row.classList.add("external");

    const kids = node.children.filter((c) => showLowLevel || !isLowLevel(c));
    const hiddenCount = node.children.length - kids.length;

    const twisty = el("span", kids.length || hiddenCount ? "twisty" : "twisty leaf", "▾");
    row.append(twisty);
    row.append(el("span", "fn-name", node.name));

    // The node's own role — also becomes the rail color for its children.
    let ownColor = null;
    for (const role of node.roles) {
      const chip = el("span", "tag role", "⚑ " + role);
      chip.style.setProperty("--role-c", roleColor(role));
      row.append(chip);
      if (!ownColor) ownColor = roleColor(role);
    }
    if (node.kind === "reference") row.append(el("span", "tag ref", "passed as value"));
    if (node.cycle) row.append(el("span", "tag", "↻ recursion"));
    if (node.dynamic) row.append(el("span", "tag", "dynamic"));
    if (node.external) row.append(el("span", "tag", "external"));
    if (node.truncated) row.append(el("span", "tag", "…truncated"));

    if (node.loc) {
      row.append(el("span", "loc", `${basename(node.loc.uri)}:${node.loc.line + 1}`));
      row.title = node.loc.uri;
    } else {
      row.append(el("span", "loc", ""));
    }

    const children = el("div", "children");
    children.style.setProperty("--rail-c", ownColor || railColor || "");
    let onPath = false;
    const isHit = boundary && matchesBoundary(node, boundary.role, boundary.target);
    for (const child of kids) {
      const sub = renderNode(child, ownColor || railColor, boundary);
      children.append(sub.el);
      if (sub.onPath) onPath = true;
    }
    if (hiddenCount > 0) {
      children.append(
        el("div", "hidden-note", `${hiddenCount} low-level call${hiddenCount === 1 ? "" : "s"} hidden`),
      );
    }
    onPath = onPath || isHit;
    if (boundary && onPath) row.classList.add("onpath");
    if (isHit) {
      row.classList.add("hit");
      row.style.setProperty("--role-c", roleColor(boundary.role));
    }

    row.addEventListener("click", (e) => {
      if (e.target.closest(".twisty") && (kids.length || hiddenCount)) {
        wrap.classList.toggle("collapsed");
        return;
      }
      openLoc(node.loc);
    });

    wrap.append(row, children);
    return { el: wrap, onPath };
  }

  // ---- The role swimlane graph ----------------------------------------------------------------

  /**
   * Collapse the call trees to the **role graph**: nodes are role-bearing functions (cards,
   * deduped by name, placed in their first role's lane), edges connect a bearer to the nearest
   * bearers reachable through any chain of non-role calls. An edge reached only via a
   * passed-as-value reference stays dashed; one real call anywhere makes it solid.
   */
  function buildRoleGraph(roots) {
    const cards = new Map();
    const edges = new Map();
    const lanes = [];
    function visit(node, src, viaRef) {
      const isBearer = node.roles.length > 0;
      let next = src;
      let nextRef = viaRef || node.kind === "reference";
      if (isBearer) {
        const lane = node.roles[0];
        if (!lanes.includes(lane)) lanes.push(lane);
        if (!cards.has(node.name)) {
          cards.set(node.name, { name: node.name, roles: node.roles, loc: node.loc, lane });
        }
        if (src && src !== node.name) {
          const key = src + "\u2192" + node.name;
          const prev = edges.get(key);
          if (!prev) edges.set(key, { src, dst: node.name, ref: nextRef });
          else prev.ref = prev.ref && nextRef;
        }
        next = node.name;
        nextRef = false;
      }
      for (const child of node.children) visit(child, next, nextRef);
    }
    roots.forEach((r) => visit(r, null, false));
    return { cards: [...cards.values()], edges: [...edges.values()], lanes };
  }

  /** The card names on any path INTO `target` (reverse reachability), plus the target itself. */
  function upstreamOf(edges, target) {
    const set = new Set([target]);
    let grew = true;
    while (grew) {
      grew = false;
      for (const e of edges) {
        if (set.has(e.dst) && !set.has(e.src)) {
          set.add(e.src);
          grew = true;
        }
      }
    }
    return set;
  }

  const SVG = "http://www.w3.org/2000/svg";

  function renderLanes(boundary) {
    const graph = buildRoleGraph(trace.roots);
    const laneOf = new Map(graph.cards.map((c) => [c.name, c.lane]));
    const wrap = el("div", "lanes-wrap");
    if (boundary) wrap.classList.add("highlighting");
    const onpath = boundary ? upstreamOf(graph.edges, boundary.target) : null;

    const lanesEl = el("div", "lanes");
    const cardEls = new Map();
    for (const lane of graph.lanes) {
      const laneEl = el("div", "lane");
      laneEl.style.setProperty("--role-c", roleColor(lane));
      laneEl.append(el("div", "lane-title", "⚑ " + lane));
      const cardsEl = el("div", "lane-cards");
      for (const card of graph.cards.filter((c) => c.lane === lane)) {
        const cardEl = el("div", "card");
        cardEl.style.setProperty("--role-c", roleColor(lane));
        cardEl.append(document.createTextNode(card.name));
        if (card.loc) {
          cardEl.append(el("span", "card-loc", `${basename(card.loc.uri)}:${card.loc.line + 1}`));
          cardEl.title = card.loc.uri;
        }
        if (onpath && onpath.has(card.name)) cardEl.classList.add("onpath");
        if (boundary && card.name === boundary.target && card.roles.includes(boundary.role)) {
          cardEl.classList.add("hit");
        }
        cardEl.addEventListener("click", () => openLoc(card.loc));
        cardEls.set(card.name, cardEl);
        cardsEl.append(cardEl);
      }
      laneEl.append(cardsEl);
      lanesEl.append(laneEl);
    }
    wrap.append(lanesEl);

    // Edges: an SVG overlay drawn once the cards have laid out (positions need real geometry —
    // guarded, so a headless environment simply skips the drawing).
    if (document.createElementNS) {
      const svg = document.createElementNS(SVG, "svg");
      svg.setAttribute("class", "lane-edges");
      wrap.append(svg);
      redrawEdges = () => {
        if (!svg.getBoundingClientRect) return;
        const base = wrap.getBoundingClientRect ? wrap.getBoundingClientRect() : null;
        if (!base || !base.width) return;
        svg.setAttribute("width", wrap.scrollWidth);
        svg.setAttribute("height", wrap.scrollHeight);
        svg.replaceChildren();
        for (const edge of graph.edges) {
          const a = cardEls.get(edge.src);
          const b = cardEls.get(edge.dst);
          if (!a || !b || !a.getBoundingClientRect) continue;
          const ra = a.getBoundingClientRect();
          const rb = b.getBoundingClientRect();
          const sameLane = Math.abs(ra.left - rb.left) < 4;
          // Anchors: right-middle → left-middle across lanes; a right-side arc within a lane.
          const x1 = ra.right - base.left + wrap.scrollLeft;
          const y1 = ra.top + ra.height / 2 - base.top;
          const x2 = (sameLane ? rb.right : rb.left) - base.left + wrap.scrollLeft;
          const y2 = rb.top + rb.height / 2 - base.top;
          const path = document.createElementNS(SVG, "path");
          const bend = sameLane ? Math.max(28, Math.abs(y2 - y1) / 2) : (x2 - x1) / 2;
          const d = sameLane
            ? `M ${x1} ${y1} C ${x1 + bend} ${y1}, ${x2 + bend} ${y2}, ${x2 + 4} ${y2}`
            : `M ${x1} ${y1} C ${x1 + bend} ${y1}, ${x2 - bend} ${y2}, ${x2 - 4} ${y2}`;
          // A small V arrowhead at the target end, stroke-colored with the edge.
          const dir = sameLane ? 1 : -1;
          const ax = sameLane ? x2 + 4 : x2 - 4;
          const arrow = ` M ${ax + 5 * dir} ${y2 - 4} L ${ax} ${y2} L ${ax + 5 * dir} ${y2 + 4}`;
          path.setAttribute("d", d + arrow);
          path.setAttribute("stroke", roleColor(laneOf.get(edge.src) ?? ""));
          let cls = "edge";
          if (edge.ref) cls += " ref";
          if (onpath && onpath.has(edge.src) && onpath.has(edge.dst)) cls += " onpath";
          path.setAttribute("class", cls);
          const title = document.createElementNS(SVG, "title");
          title.textContent = `${edge.src} → ${edge.dst}${edge.ref ? " (passed as value)" : ""}`;
          path.append(title);
          svg.append(path);
        }
      };
      if (typeof requestAnimationFrame === "function") requestAnimationFrame(redrawEdges);
      else redrawEdges();
    }
    return wrap;
  }

  // ---- Assembly -------------------------------------------------------------------------------

  function render() {
    app.replaceChildren();
    if (!trace) {
      app.append(el("div", "message", "Run a trace to see it here."));
      return;
    }
    if (trace.status === "noRoles") {
      app.append(el("div", "message", "No @role bindings on any function — nothing to trace. Bind roles via a @role(...) attribute to map the architecture."));
      return;
    }
    if (trace.status === "notFound") {
      app.append(el("div", "message", `“${trace.from ?? ""}” matches no role binding and no function.`));
      return;
    }

    const header = el("div", "trace-header");
    header.append(el("span", "trace-title", trace.from ? `trace — from ${trace.from}` : "trace — every role-bearing function"));
    if (trace.truncated) header.append(el("span", "trace-note", "(truncated — node budget reached)"));
    const switcher = el("div", "view-switch");
    for (const [mode, label] of [["tree", "Tree"], ["lanes", "Lanes"]]) {
      const btn = el("button", viewMode === mode ? "active" : "", label);
      btn.addEventListener("click", () => {
        if (viewMode !== mode) {
          viewMode = mode;
          render();
        }
      });
      switcher.append(btn);
    }
    header.append(switcher);
    if (viewMode === "tree") {
      const controls = el("label", "trace-controls");
      const toggle = document.createElement("input");
      toggle.type = "checkbox";
      toggle.checked = showLowLevel;
      toggle.addEventListener("change", () => {
        showLowLevel = toggle.checked;
        render();
      });
      controls.append(toggle, document.createTextNode("show low-level calls"));
      header.append(controls);
    }
    app.append(header);

    if (trace.boundaries.length) {
      const rail = el("div", "boundaries");
      // With a boundary toggled on, mute its siblings — the active/filter state reads at a glance
      // (and signals that the pills are toggles at all).
      if (activeBoundary) rail.classList.add("filtering");
      for (const b of trace.boundaries) {
        const chip = el("button", "boundary");
        chip.style.setProperty("--role-c", roleColor(b.role));
        chip.append(el("span", "role", "⚑ " + b.role), el("span", null, b.target));
        if (activeBoundary === boundaryKey(b)) chip.classList.add("active");
        chip.addEventListener("click", () => {
          activeBoundary = activeBoundary === boundaryKey(b) ? null : boundaryKey(b);
          render();
        });
        chip.addEventListener("dblclick", () => openLoc(b.loc));
        rail.append(chip);
      }
      app.append(rail);
    }

    const boundary = activeBoundary
      ? trace.boundaries.find((b) => boundaryKey(b) === activeBoundary) ?? null
      : null;
    if (!trace.roots.length) {
      app.append(el("div", "message", "The trace reached nothing."));
      return;
    }
    redrawEdges = null;
    if (viewMode === "lanes") {
      app.append(renderLanes(boundary));
      return;
    }
    const tree = el("div", "tree");
    if (boundary) tree.classList.add("highlighting");
    for (const root of trace.roots) {
      tree.append(renderNode(root, null, boundary).el);
    }
    app.append(tree);
  }

  window.addEventListener("resize", () => {
    if (redrawEdges) redrawEdges();
  });

  window.addEventListener("message", (event) => {
    const msg = event.data;
    if (msg.type === "trace") {
      trace = msg.trace;
      activeBoundary = null;
      render();
    }
  });

  render();
  vscode.postMessage({ type: "ready" });
})();
