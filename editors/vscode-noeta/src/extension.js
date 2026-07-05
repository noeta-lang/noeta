// The Noeta VS Code language client.
//
// Grammar-based highlighting works with no compiler (the TextMate grammar in `syntaxes/`); this file
// adds the *semantic* half by launching the `noeta lsp` language server and connecting to it over
// stdio. Once connected, the server provides live diagnostics, hover types, go-to-definition, the
// document outline, and completion — every feature is served by the compiler's own salsa query graph,
// so what the editor shows always matches what the compiler sees.
//
// Plain JavaScript on purpose: the extension runs directly after `npm install`, with no build step to
// get out of sync with the source.

const { workspace, window, commands } = require("vscode");
const { LanguageClient, TransportKind } = require("vscode-languageclient/node");

/** @type {import("vscode-languageclient/node").LanguageClient | undefined} */
let client;

/**
 * How to launch the server: run the configured `noeta` executable (on `PATH` by default) with the
 * `lsp` subcommand, talking JSON-RPC over stdio. The same invocation is used for normal and debug
 * runs — the server has no separate debug mode.
 */
function buildServerOptions() {
  const command = workspace.getConfiguration("noeta").get("server.path", "noeta");
  const run = { command, args: ["lsp"], transport: TransportKind.stdio };
  return { run, debug: run };
}

function activate(context) {
  /** @type {import("vscode-languageclient/node").LanguageClientOptions} */
  const clientOptions = {
    // Only drive the server for on-disk `.noe` documents in the `noeta` language mode.
    documentSelector: [{ scheme: "file", language: "noeta" }],
    synchronize: {
      // Notify the server when `.noe` files change on disk (e.g. a sibling module edited outside the
      // editor), so its workspace view stays current.
      fileEvents: workspace.createFileSystemWatcher("**/*.noe"),
    },
  };

  client = new LanguageClient(
    "noeta",
    "Noeta Language Server",
    buildServerOptions(),
    clientOptions,
  );

  // A manual restart, handy while developing the server itself (rebuild, then restart the client).
  context.subscriptions.push(
    commands.registerCommand("noeta.restartServer", async () => {
      if (!client) {
        return;
      }
      await client.restart();
      window.showInformationMessage("Noeta language server restarted.");
    }),
  );

  // Starting the client spawns the server; a failure to launch (e.g. `noeta` not on `PATH`) surfaces
  // in the "Noeta Language Server" output channel.
  client.start();
}

function deactivate() {
  return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
