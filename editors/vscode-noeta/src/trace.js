// The role-aware trace view (ide-ui U2; trace-view arc): the `noeta.showTrace` command — invoked
// by the role CodeLens, the Architecture view's context menu, or the palette — fetches the
// **structured** trace (`noeta/traceTree`, the same call-graph walk `noeta trace` renders as text
// for terminals and agents) and shows it in a dedicated webview panel: a role-colored boundary
// rail, interactive call trees with role-tinted indent rails, click-to-source everywhere, and the
// walk's honesty markers (dynamic / external / passed-as-value / recursion / truncation) made
// visible. One panel, reused across traces.

const { window, commands, Uri, Range, Position, ViewColumn } = require("vscode");

/** A little HTML-attribute-safe nonce for the webview's CSP. */
function nonce() {
  let text = "";
  const chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  for (let i = 0; i < 32; i++) {
    text += chars.charAt(Math.floor(Math.random() * chars.length));
  }
  return text;
}

/** The reused trace panel: fetches `noeta/traceTree` and hands the structure to the webview. */
class TracePanel {
  constructor(context, getClient) {
    this.context = context;
    this.getClient = getClient;
    this.panel = undefined;
    this.ready = false;
    this.pending = undefined;
  }

  ensurePanel() {
    if (this.panel) return this.panel;
    this.ready = false;
    const media = Uri.joinPath(this.context.extensionUri, "media");
    const panel = window.createWebviewPanel(
      "noeta.traceView",
      "Noeta Trace",
      { viewColumn: ViewColumn.Beside, preserveFocus: true },
      { enableScripts: true, localResourceRoots: [media], retainContextWhenHidden: true },
    );
    const scriptUri = panel.webview.asWebviewUri(Uri.joinPath(media, "trace.js"));
    const styleUri = panel.webview.asWebviewUri(Uri.joinPath(media, "trace.css"));
    const n = nonce();
    panel.webview.html = `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; style-src ${panel.webview.cspSource}; script-src 'nonce-${n}';">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<link href="${styleUri}" rel="stylesheet">
<title>Noeta Trace</title>
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
      }
    } else if (msg.type === "source") {
      const pos = new Position(msg.line, msg.character || 0);
      await commands.executeCommand("vscode.open", Uri.parse(msg.uri), {
        selection: new Range(pos, pos),
        viewColumn: ViewColumn.One,
      });
    }
  }

  /** Fetch the trace for `sourceUri` (from `fnName`, or the whole role surface) and show it. */
  async show(sourceUri, fnName) {
    const client = this.getClient();
    if (!client) {
      window.showWarningMessage("Noeta: the language server is not running yet.");
      return;
    }
    const panel = this.ensurePanel();
    panel.title = fnName ? `Trace: ${fnName}` : "Noeta Trace";
    panel.reveal(ViewColumn.Beside, true);
    const trace = await client.sendRequest("noeta/traceTree", {
      uri: sourceUri,
      from: fnName || null,
    });
    const message = { type: "trace", trace };
    if (this.ready) {
      panel.webview.postMessage(message);
    } else {
      this.pending = message;
    }
  }
}

/**
 * Wire the trace view: the panel + the `noeta.showTrace` command. `getClient` returns the running
 * LanguageClient (the command needs it lazily — activation starts the client asynchronously).
 */
function registerTrace(context, getClient) {
  const panel = new TracePanel(context, getClient);
  context.subscriptions.push(
    // From a CodeLens: (uri, function). From the palette: no args — trace the active `.noe`
    // file's whole architectural surface (every role-bearing function).
    commands.registerCommand("noeta.showTrace", async (uriStr, fnName) => {
      const source =
        uriStr ||
        (window.activeTextEditor && window.activeTextEditor.document.languageId === "noeta"
          ? window.activeTextEditor.document.uri.toString()
          : undefined);
      if (!source) {
        window.showWarningMessage("Noeta: open a .noe file to trace.");
        return;
      }
      await panel.show(source, fnName);
    }),
  );
}

module.exports = { registerTrace };
