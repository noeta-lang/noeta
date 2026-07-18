// The Docs browser (docs-browser arc; docs-browser-ui arc). Two surfaces:
//
//   * The **Docs view** (`noetaDocs`) — a single webview holding the filter input AND the navigable
//     tree in one pane (a native TreeView can't host an input; and driving a native tree's expansion
//     from a filter left matches hidden under collapsed roots). Empty filter → a lazy tree; typing →
//     a flat ranked match list. Fed by the language server's `noeta/docs[Children]` / `noeta/docsSearch`
//     over the unified doc model (the same model `noeta mcp` serves — editor and agents see one tree).
//   * The **page browser** (`noeta.docsPage`) — a reused webview panel that renders a `DocPage` as a
//     styled document (replacing the old markdown-preview tab), with clickable "see also" xrefs and a
//     go-to-source footer.
//
// The view follows the active editor's workspace (Noeta workspaces are per-entry-file) and refreshes
// on save.

const {
  window,
  workspace,
  commands,
  Uri,
  Range,
  Position,
  ViewColumn,
} = require("vscode");

/** A little HTML-attribute-safe nonce for the webviews' CSP. */
function nonce() {
  let text = "";
  const chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  for (let i = 0; i < 32; i++) {
    text += chars.charAt(Math.floor(Math.random() * chars.length));
  }
  return text;
}

/**
 * The Docs sidebar view: filter + tree in one webview. Holds the active `.noe` file's URI (the
 * workspace the project/deps corpora resolve against) and answers the webview's data requests over
 * the language client.
 */
class DocsViewProvider {
  constructor(context, getClient, openPage) {
    this.context = context;
    this.getClient = getClient;
    this.openPage = openPage; // (id) => Promise<void>
    this.view = undefined;
    this.sourceUri = undefined;
  }

  resolveWebviewView(view) {
    this.view = view;
    const media = Uri.joinPath(this.context.extensionUri, "media");
    view.webview.options = { enableScripts: true, localResourceRoots: [media] };
    const scriptUri = view.webview.asWebviewUri(Uri.joinPath(media, "docsView.js"));
    const styleUri = view.webview.asWebviewUri(Uri.joinPath(media, "docsView.css"));
    const n = nonce();
    view.webview.html = `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; style-src ${view.webview.cspSource}; script-src 'nonce-${n}'; img-src ${view.webview.cspSource};">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<link href="${styleUri}" rel="stylesheet">
<title>Noeta Docs</title>
</head>
<body>
<div id="filter-row">
  <input id="filter" type="text" placeholder="Filter docs…" aria-label="Filter docs" />
  <button id="clear" class="icon-btn" title="Clear filter" aria-label="Clear filter"></button>
  <button id="collapse" class="icon-btn" title="Collapse all" aria-label="Collapse all"></button>
</div>
<div id="count" aria-live="polite"></div>
<div id="tree"></div>
<script nonce="${n}" src="${scriptUri}"></script>
</body>
</html>`;
    // The clear/collapse glyphs are drawn by the script; set them here so the buttons aren't empty.
    view.webview.onDidReceiveMessage((msg) => this.handle(msg));
  }

  /** The URI the corpora resolve against — empty when no `.noe` file is active (guide/api still work). */
  uri() {
    return this.sourceUri || "";
  }

  setSource(uri) {
    if (uri !== this.sourceUri) {
      this.sourceUri = uri;
      this.sendRoots();
    }
  }

  /** Re-send the corpus roots (also the refresh action) — resets the tree to its top level. */
  refresh() {
    this.sendRoots();
  }

  async sendRoots() {
    if (!this.view) return;
    const client = this.getClient();
    if (!client) {
      this.post({ type: "message", text: "Starting the Noeta language server…" });
      return;
    }
    const reply = await client.sendRequest("noeta/docs", { uri: this.uri() });
    this.post({ type: "roots", nodes: (reply && reply.nodes) || [] });
  }

  post(message) {
    if (this.view) this.view.webview.postMessage(message);
  }

  async handle(msg) {
    const client = this.getClient();
    switch (msg.type) {
      case "ready":
        this.sendRoots();
        break;
      case "roots":
        this.sendRoots();
        break;
      case "children": {
        if (!client) return this.post({ type: "children", id: msg.id, nodes: [] });
        const reply = await client.sendRequest("noeta/docsChildren", { uri: this.uri(), id: msg.id });
        this.post({ type: "children", id: msg.id, nodes: (reply && reply.nodes) || [] });
        break;
      }
      case "search": {
        if (!client) return this.post({ type: "results", query: msg.query, hits: [] });
        const reply = await client.sendRequest("noeta/docsSearch", { uri: this.uri(), query: msg.query });
        this.post({ type: "results", query: msg.query, hits: (reply && reply.hits) || [] });
        break;
      }
      case "open":
        await this.openPage(msg.id, this.uri());
        break;
      case "source":
        await commands.executeCommand("vscode.open", Uri.parse(msg.uri), {
          selection: new Range(
            new Position(msg.line, msg.character || 0),
            new Position(msg.line, msg.character || 0),
          ),
        });
        break;
    }
  }
}

/**
 * The page browser: a single reused webview panel that renders a `DocPage` as a styled document. The
 * markdown-to-HTML rendering, clickable cross-references, and heading handling live in `media/docs.js`.
 */
class DocsPagePanel {
  constructor(context, getClient) {
    this.context = context;
    this.getClient = getClient;
    this.panel = undefined;
    this.ready = false;
    this.pending = undefined;
    /** Called when a cross-reference is opened, so the tree can reveal it: (id, sourceUri) => void. */
    this.onNavigate = undefined;
  }

  ensurePanel() {
    if (this.panel) return this.panel;
    this.ready = false;
    const media = Uri.joinPath(this.context.extensionUri, "media");
    const panel = window.createWebviewPanel(
      "noeta.docsPage",
      "Noeta Docs",
      { viewColumn: ViewColumn.Beside, preserveFocus: true },
      { enableScripts: true, localResourceRoots: [media], retainContextWhenHidden: true },
    );
    const scriptUri = panel.webview.asWebviewUri(Uri.joinPath(media, "docs.js"));
    const styleUri = panel.webview.asWebviewUri(Uri.joinPath(media, "docs.css"));
    const n = nonce();
    panel.webview.html = `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; style-src ${panel.webview.cspSource}; script-src 'nonce-${n}';">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<link href="${styleUri}" rel="stylesheet">
<title>Noeta Docs</title>
</head>
<body>
<div id="app" aria-live="polite"></div>
<script nonce="${n}" src="${scriptUri}"></script>
</body>
</html>`;
    panel.webview.onDidReceiveMessage((msg) => this.handleMessage(msg));
    panel.onDidDispose(() => {
      this.panel = undefined;
      this.pending = undefined;
      this.ready = false;
    });
    this.panel = panel;
    return panel;
  }

  async handleMessage(msg) {
    if (msg.type === "ready") {
      this.ready = true;
      if (this.pending) {
        this.panel.webview.postMessage(this.pending);
        this.pending = undefined;
      } else {
        this.panel.webview.postMessage({ type: "placeholder", text: "Select a doc to read it here." });
      }
    } else if (msg.type === "navigate") {
      await this.openById(msg.id, msg.sourceUri);
      if (this.onNavigate) this.onNavigate(msg.id, msg.sourceUri);
    } else if (msg.type === "source") {
      const pos = new Position(msg.line, msg.character || 0);
      await commands.executeCommand("vscode.open", Uri.parse(msg.uri), {
        selection: new Range(pos, pos),
        viewColumn: ViewColumn.One,
      });
    }
  }

  /** Fetch and show the page for a doc id, revealing the panel. */
  async openById(id, sourceUri, fallbackTitle) {
    const client = this.getClient();
    if (!client) return;
    const panel = this.ensurePanel();
    panel.reveal(ViewColumn.Beside, true);
    const reply = await client.sendRequest("noeta/docsPage", { uri: sourceUri || "", id });
    const page =
      (reply && reply.page) ||
      { id, title: fallbackTitle || id.split("/").pop(), kind: "section", markdown: "", xrefs: [] };
    const highlights = await fetchHighlights(client, page);
    const message = { type: "page", page, sourceUri, highlights };
    if (this.ready) {
      panel.webview.postMessage(message);
    } else {
      this.pending = message;
    }
  }
}

/** Fence languages highlighted as Noeta: explicit tags plus untagged (the guide's convention). */
const NOETA_FENCES = new Set(["", "noeta", "noe"]);

/**
 * Scan `markdown` for fenced code blocks, mirroring the webview renderer's fence parsing exactly
 * (`media/docs.js` `renderMarkdown`): any line starting ``` opens a fence, the body runs until the
 * next ```-starting line. Returns every fence in order with its ordinal (the webview counts all
 * fences the same way, so ordinals line up).
 */
function scanFences(markdown) {
  const lines = (markdown || "").replace(/\r\n/g, "\n").split("\n");
  const fences = [];
  let i = 0;
  while (i < lines.length) {
    const open = lines[i].match(/^```(\w*)/);
    if (open) {
      const body = [];
      i++;
      while (i < lines.length && !lines[i].startsWith("```")) {
        body.push(lines[i]);
        i++;
      }
      i++; // closing fence
      fences.push({ index: fences.length, lang: open[1], body: body.join("\n") });
      continue;
    }
    i++;
  }
  return fences;
}

/**
 * Ask the server to lexer-highlight a page's Noeta code (the signature + noeta/untagged fences) in
 * one batched `noeta/docsHighlight` request. Returns `{ signature, blocks }` (blocks keyed by the
 * fence's ordinal among ALL fences), or null — the page then renders plain, never broken.
 */
async function fetchHighlights(client, page) {
  try {
    const fences = scanFences(page.markdown).filter((f) => NOETA_FENCES.has(f.lang));
    const snippets = [];
    if (page.signature) snippets.push(page.signature);
    for (const f of fences) snippets.push(f.body);
    if (!snippets.length) return null;
    const reply = await client.sendRequest("noeta/docsHighlight", { snippets });
    const all = (reply && reply.spans) || [];
    let k = 0;
    const out = { signature: null, blocks: {} };
    if (page.signature) out.signature = all[k++] || null;
    for (const f of fences) out.blocks[f.index] = all[k++] || null;
    return out;
  } catch {
    return null;
  }
}

/**
 * Wire the Docs browser: the sidebar webview view, the page-browser panel, active-editor tracking,
 * and the open-page / docs-for-symbol / search commands. `getClient` returns the running
 * LanguageClient (lazy — activation starts it asynchronously).
 */
function registerDocs(context, getClient) {
  const pagePanel = new DocsPagePanel(context, getClient);
  const provider = new DocsViewProvider(context, getClient, (id, uri) =>
    pagePanel.openById(id, uri),
  );

  function track(editor) {
    if (
      editor &&
      editor.document.languageId === "noeta" &&
      editor.document.uri.scheme === "file"
    ) {
      provider.setSource(editor.document.uri.toString());
    }
  }
  track(window.activeTextEditor);

  context.subscriptions.push(
    window.registerWebviewViewProvider("noetaDocs", provider, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
    window.onDidChangeActiveTextEditor(track),
    workspace.onDidSaveTextDocument((doc) => {
      if (doc.languageId === "noeta") provider.refresh();
    }),
    commands.registerCommand("noeta.docsRefresh", () => provider.refresh()),
    // The QuickPick search: a jump-to over the whole corpus (complements the inline filter, which
    // narrows the tree in place). Accepting a result opens its page.
    commands.registerCommand("noeta.docsSearch", () => {
      const sourceUri = provider.uri();
      const qp = window.createQuickPick();
      qp.placeholder = "Search Noeta docs — project, dependencies, language guide, and API reference…";
      let seq = 0;
      qp.onDidChangeValue(async (value) => {
        const query = value.trim();
        if (!query) {
          qp.items = [];
          return;
        }
        const client = getClient();
        if (!client) return;
        const mine = (seq += 1);
        qp.busy = true;
        const reply = await client.sendRequest("noeta/docsSearch", { uri: sourceUri, query });
        if (mine !== seq) return;
        qp.busy = false;
        qp.items = ((reply && reply.hits) || []).map((hit) => ({
          label: hit.title,
          description: hit.kind,
          detail: hit.snippet || undefined,
          alwaysShow: true,
          hit,
        }));
      });
      qp.onDidAccept(async () => {
        const sel = qp.selectedItems[0];
        qp.hide();
        if (sel) await pagePanel.openById(sel.hit.id, sourceUri, sel.hit.title);
      });
      qp.onDidHide(() => qp.dispose());
      qp.show();
    }),
    // Palette / editor context: show the docs for the symbol under the cursor.
    commands.registerCommand("noeta.docsForSymbol", async () => {
      const client = getClient();
      const editor = window.activeTextEditor;
      if (!client || !editor || editor.document.languageId !== "noeta") {
        window.showWarningMessage("Noeta: put the cursor on a symbol in a .noe file.");
        return;
      }
      const uri = editor.document.uri.toString();
      const pos = editor.selection.active;
      const reply = await client.sendRequest("noeta/docsForSymbol", {
        uri,
        line: pos.line,
        character: pos.character,
      });
      const id = reply && reply.id;
      if (!id) {
        window.showInformationMessage("Noeta: no documentation node for this symbol.");
        return;
      }
      await pagePanel.openById(id, uri, id.split("/").pop());
    }),
  );

  return {
    refresh: () => provider.refresh(),
    provider,
  };
}

module.exports = { registerDocs };
