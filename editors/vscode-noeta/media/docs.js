// The Noeta Docs page browser webview (docs-browser-ui arc). Renders a `DocPage` sent by the
// extension into a styled, theme-aware document — a real rendering surface in place of the ephemeral
// markdown-preview tab. It owns markdown→HTML rendering (self-contained, no external library, so the
// CSP stays `script-src 'nonce-…'` only), makes "see also" cross-references and the source footer
// clickable (posting navigation back to the extension, which the markdown preview could never do),
// and — crucially — does NOT inject a title heading for language-guide pages, whose markdown already
// carries its own `# Title` (the old renderer prepended one, doubling the heading).

(function () {
  const vscode = acquireVsCodeApi();
  const app = document.getElementById("app");

  // ---- A compact, defensive markdown renderer for our own docs (headings, lists, code fences,
  // blockquotes, rules, tables, and inline emphasis/code/links). Everything is HTML-escaped before
  // structure is applied, so page content can never inject markup. ----

  function escapeHtml(s) {
    return s
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;");
  }

  // Inline spans: `code` first (its content is escaped and left literal), then links, then emphasis.
  function renderInline(text) {
    // Split on inline code so emphasis/link rules never touch code content.
    const parts = text.split(/(`[^`]+`)/g);
    return parts
      .map((part) => {
        if (part.startsWith("`") && part.endsWith("`") && part.length >= 2) {
          return "<code>" + escapeHtml(part.slice(1, -1)) + "</code>";
        }
        let out = escapeHtml(part);
        // [label](url) — only http(s) and mailto get an href; anything else renders as plain text.
        out = out.replace(/\[([^\]]+)\]\(([^)]+)\)/g, (m, label, url) => {
          if (/^(https?:|mailto:)/i.test(url)) {
            const safe = url.replace(/"/g, "&quot;");
            return '<a href="' + safe + '">' + label + "</a>";
          }
          return label;
        });
        out = out.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
        out = out.replace(/(^|[^*])\*([^*]+)\*/g, "$1<em>$2</em>");
        out = out.replace(/_([^_]+)_/g, "<em>$1</em>");
        return out;
      })
      .join("");
  }

  /** Render lexer-highlighted code: escape between spans, wrap span text in `tok-*` classes.
   *  Spans are server-supplied (noeta/docsHighlight), sorted, non-overlapping, UTF-16 offsets —
   *  i.e. exactly JavaScript string offsets. Defensive against malformed spans: skipped, never
   *  thrown, so a bad span degrades to plain text. */
  function renderHighlighted(text, spans) {
    let out = "";
    let pos = 0;
    for (const s of spans) {
      if (!s || s.start < pos || s.end > text.length || s.end <= s.start) continue;
      out += escapeHtml(text.slice(pos, s.start));
      out += '<span class="tok-' + s.class + '">' + escapeHtml(text.slice(s.start, s.end)) + "</span>";
      pos = s.end;
    }
    return out + escapeHtml(text.slice(pos));
  }

  function renderMarkdown(src, hl) {
    const lines = (src || "").replace(/\r\n/g, "\n").split("\n");
    const html = [];
    let i = 0;
    let fenceIdx = 0;

    const flushList = (items, ordered) => {
      const tag = ordered ? "ol" : "ul";
      html.push(
        "<" + tag + ">" + items.map((it) => "<li>" + renderInline(it) + "</li>").join("") + "</" + tag + ">",
      );
    };

    while (i < lines.length) {
      const line = lines[i];

      // Fenced code block.
      const fence = line.match(/^```(\w*)/);
      if (fence) {
        const body = [];
        i++;
        while (i < lines.length && !lines[i].startsWith("```")) {
          body.push(lines[i]);
          i++;
        }
        i++; // closing fence
        const codeText = body.join("\n");
        const spans = hl && hl.blocks ? hl.blocks[fenceIdx] : null;
        fenceIdx++;
        html.push(
          "<pre><code>" +
            (spans ? renderHighlighted(codeText, spans) : escapeHtml(codeText)) +
            "</code></pre>",
        );
        continue;
      }

      // Horizontal rule.
      if (/^(-{3,}|\*{3,}|_{3,})\s*$/.test(line)) {
        html.push("<hr>");
        i++;
        continue;
      }

      // ATX heading.
      const heading = line.match(/^(#{1,6})\s+(.*)$/);
      if (heading) {
        const level = Math.min(heading[1].length, 4);
        html.push("<h" + level + ">" + renderInline(heading[2].trim()) + "</h" + level + ">");
        i++;
        continue;
      }

      // Blockquote (consecutive `>` lines).
      if (/^>\s?/.test(line)) {
        const quote = [];
        while (i < lines.length && /^>\s?/.test(lines[i])) {
          quote.push(lines[i].replace(/^>\s?/, ""));
          i++;
        }
        html.push("<blockquote>" + renderInline(quote.join(" ")) + "</blockquote>");
        continue;
      }

      // Simple pipe table: a header row followed by a `---|---` separator.
      if (line.includes("|") && i + 1 < lines.length && /^\s*\|?[\s:|-]+\|[\s:|-]+$/.test(lines[i + 1])) {
        const cells = (row) =>
          row
            .replace(/^\s*\|/, "")
            .replace(/\|\s*$/, "")
            .split("|")
            .map((c) => c.trim());
        const header = cells(line);
        i += 2; // header + separator
        const rows = [];
        while (i < lines.length && lines[i].includes("|")) {
          rows.push(cells(lines[i]));
          i++;
        }
        const th = header.map((c) => "<th>" + renderInline(c) + "</th>").join("");
        const trs = rows
          .map((r) => "<tr>" + r.map((c) => "<td>" + renderInline(c) + "</td>").join("") + "</tr>")
          .join("");
        html.push("<table><thead><tr>" + th + "</tr></thead><tbody>" + trs + "</tbody></table>");
        continue;
      }

      // Unordered list.
      if (/^\s*[-*+]\s+/.test(line)) {
        const items = [];
        while (i < lines.length && /^\s*[-*+]\s+/.test(lines[i])) {
          items.push(lines[i].replace(/^\s*[-*+]\s+/, ""));
          i++;
        }
        flushList(items, false);
        continue;
      }

      // Ordered list.
      if (/^\s*\d+[.)]\s+/.test(line)) {
        const items = [];
        while (i < lines.length && /^\s*\d+[.)]\s+/.test(lines[i])) {
          items.push(lines[i].replace(/^\s*\d+[.)]\s+/, ""));
          i++;
        }
        flushList(items, true);
        continue;
      }

      // Blank line.
      if (line.trim() === "") {
        i++;
        continue;
      }

      // Paragraph: gather until a blank line or a block starter.
      const para = [];
      while (
        i < lines.length &&
        lines[i].trim() !== "" &&
        !/^(#{1,6}\s|>\s?|```|\s*[-*+]\s+|\s*\d+[.)]\s+)/.test(lines[i]) &&
        !/^(-{3,}|\*{3,}|_{3,})\s*$/.test(lines[i])
      ) {
        para.push(lines[i]);
        i++;
      }
      html.push("<p>" + renderInline(para.join(" ")) + "</p>");
    }

    return html.join("\n");
  }

  // ---- Page assembly ------------------------------------------------------------------------

  const KIND_LABEL = {
    root: "Docs",
    module: "Module",
    function: "Function",
    method: "Method",
    struct: "Struct",
    class: "Class",
    enum: "Enum",
    variant: "Variant",
    field: "Field",
    interface: "Interface",
    trait: "Trait",
    section: "Section",
    guide: "Guide",
  };

  function el(tag, className, html) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (html != null) node.innerHTML = html;
    return node;
  }

  function renderPage(page, sourceUri, highlights) {
    app.textContent = "";

    // Language-guide pages carry their own `# Title` H1 in the markdown; rendering an injected
    // header too is exactly the "heading repeated" bug. So for guide pages we render the markdown
    // verbatim and skip the synthesized header/signature.
    const isGuide = page.kind === "guide";

    if (!isGuide) {
      const header = el("div", "doc-header");
      header.appendChild(el("span", "doc-kind", escapeHtml(KIND_LABEL[page.kind] || page.kind)));
      header.appendChild(el("h1", "doc-title", escapeHtml(page.title || page.id)));
      app.appendChild(header);

      if (page.signature) {
        const sig = el("div", "doc-signature");
        sig.appendChild(
          el(
            "code",
            null,
            highlights && highlights.signature
              ? renderHighlighted(page.signature, highlights.signature)
              : escapeHtml(page.signature),
          ),
        );
        app.appendChild(sig);
      }
    }

    const hasProse = page.markdown && page.markdown.trim();
    const body = el("div", "doc-body");
    if (hasProse) {
      body.innerHTML = renderMarkdown(page.markdown, highlights);
    } else if (!isGuide && !page.signature) {
      body.appendChild(el("p", "doc-empty-note", "No documentation yet."));
    } else if (!isGuide) {
      body.appendChild(el("p", "doc-empty-note", "No prose documentation for this declaration."));
    }
    app.appendChild(body);

    // Cross-references ("See also"): clickable, unlike the old markdown preview.
    if (page.xrefs && page.xrefs.length) {
      const section = el("div", "doc-section");
      section.appendChild(el("div", "doc-section-label", "See also"));
      const list = el("div", "doc-xrefs");
      for (const xref of page.xrefs) {
        const btn = el("button", "doc-xref", escapeHtml(xref.title));
        btn.addEventListener("click", () =>
          vscode.postMessage({ type: "navigate", id: xref.id, sourceUri }),
        );
        list.appendChild(btn);
      }
      section.appendChild(list);
      app.appendChild(section);
    }

    // Source footer: jump to the declaration in the editor.
    if (page.uri && page.line != null) {
      const section = el("div", "doc-section");
      const file = page.uri.split("/").pop();
      const btn = el("button", "doc-source", escapeHtml(file + ":" + (page.line + 1)));
      btn.title = "Go to source";
      btn.addEventListener("click", () =>
        vscode.postMessage({
          type: "source",
          uri: page.uri,
          line: page.line,
          character: page.character || 0,
        }),
      );
      section.appendChild(btn);
      app.appendChild(section);
    }
  }

  function renderPlaceholder(text) {
    app.textContent = "";
    app.appendChild(el("div", "placeholder", escapeHtml(text)));
  }

  window.addEventListener("message", (event) => {
    const msg = event.data;
    if (msg.type === "page") {
      renderPage(msg.page, msg.sourceUri, msg.highlights);
    } else if (msg.type === "placeholder") {
      renderPlaceholder(msg.text || "");
    }
  });

  vscode.postMessage({ type: "ready" });
})();
