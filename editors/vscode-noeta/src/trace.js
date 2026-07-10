// The role-aware trace view (ide-ui U2): the `noeta.showTrace` command — invoked by the role
// CodeLenses the language server puts above `@role`-bearing declarations, or from the palette —
// asks the server's `noeta/trace` custom request for the rendered static trace and opens it as a
// read-only `noeta-trace:` virtual document. The server renders the same call-graph walk the
// `noeta mcp` trace tool serves, so what the editor shows always matches what agents see.
//
// Every `path:line` in the document is a clickable link (a DocumentLinkProvider over the trace
// scheme), resolved against the traced workspace's directory carried in the virtual document URI.

const {
  window,
  workspace,
  commands,
  languages,
  Uri,
  Range,
  DocumentLink,
  EventEmitter,
  ViewColumn,
} = require("vscode");
const path = require("path");

/** Rendered trace text per virtual-document URI (string form). The content provider reads it. */
const traceContents = new Map();
const traceChanged = new EventEmitter();

/** The virtual URI for a trace: the label becomes the tab title; the query carries the base
 *  directory `path:line` links resolve against. */
function traceUri(label, baseDir) {
  const query = encodeURIComponent(JSON.stringify({ dir: baseDir }));
  return Uri.parse(`noeta-trace:${encodeURIComponent(label)}.trace?${query}`);
}

function baseDirOf(uri) {
  try {
    return JSON.parse(decodeURIComponent(uri.query)).dir || "";
  } catch {
    return "";
  }
}

/** Run `noeta/trace` for (sourceUri, from) and open/refresh the virtual document. */
async function showTrace(getClient, sourceUri, from) {
  const client = getClient();
  if (!client) {
    window.showWarningMessage("Noeta language server is not running.");
    return;
  }
  const reply = await client.sendRequest("noeta/trace", {
    uri: sourceUri,
    from: from || null,
  });
  if (!reply || !reply.content) {
    window.showWarningMessage("Noeta: no workspace covers this file — open a .noe file first.");
    return;
  }
  const fsPath = Uri.parse(sourceUri).fsPath;
  const target = traceUri(from || "all-roles", path.dirname(fsPath));
  traceContents.set(target.toString(), reply.content);
  traceChanged.fire(target); // refresh if the document is already open
  const doc = await workspace.openTextDocument(target);
  await window.showTextDocument(doc, { viewColumn: ViewColumn.Beside, preview: true });
}

/** Turn every `path:line` in a trace document into a link to that source location. */
function traceLinks(document) {
  const base = baseDirOf(document.uri);
  const links = [];
  const pattern = /([\w./-]+\.noe):(\d+)/g;
  for (let line = 0; line < document.lineCount; line += 1) {
    const text = document.lineAt(line).text;
    for (const match of text.matchAll(pattern)) {
      const file = path.isAbsolute(match[1]) ? match[1] : path.join(base, match[1]);
      const range = new Range(line, match.index, line, match.index + match[0].length);
      // The `#L<line>` fragment makes VS Code reveal that line on open.
      links.push(new DocumentLink(range, Uri.file(file).with({ fragment: `L${match[2]}` })));
    }
  }
  return links;
}

/**
 * Wire the trace view: the content provider for the `noeta-trace:` scheme, the link provider that
 * makes `path:line` clickable, and the `noeta.showTrace` command. `getClient` returns the running
 * LanguageClient (the command needs it lazily — activation starts the client asynchronously).
 */
function registerTrace(context, getClient) {
  context.subscriptions.push(
    workspace.registerTextDocumentContentProvider("noeta-trace", {
      onDidChange: traceChanged.event,
      provideTextDocumentContent(uri) {
        return traceContents.get(uri.toString()) || "trace expired — run it again";
      },
    }),
    languages.registerDocumentLinkProvider(
      { scheme: "noeta-trace" },
      { provideDocumentLinks: traceLinks },
    ),
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
      await showTrace(getClient, source, fnName);
    }),
    traceChanged,
  );
}

module.exports = { registerTrace };
