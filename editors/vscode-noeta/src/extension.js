// The Noeta VS Code language client + debug client.
//
// Grammar-based highlighting works with no compiler (the TextMate grammar in `syntaxes/`); this file
// adds the *semantic* half by launching the `noeta lsp` language server and connecting to it over
// stdio. Once connected, the server provides live diagnostics, hover types, go-to-definition, the
// document outline, and completion — every feature is served by the compiler's own salsa query graph,
// so what the editor shows always matches what the compiler sees.
//
// It also wires the **debugger**: pressing F5 on a `.noe` file launches `noeta dap` (the Debug Adapter
// Protocol server) and drives it through VS Code's generic debug UI — breakpoints, stepping, the call
// stack, and variables. And it registers the **MCP server** (`noeta mcp`) with the editor's language-
// model API, so AI agents running in the editor discover the compiler's tools automatically. The same
// `noeta` binary serves all three roles, so one `noeta.server.path` setting points at all of them.
//
// Plain JavaScript on purpose: the extension runs directly after `npm install`, with no build step to
// get out of sync with the source.

const {
  workspace,
  window,
  commands,
  debug,
  lm,
  DebugAdapterExecutable,
  EventEmitter,
  McpStdioServerDefinition,
} = require("vscode");
const { LanguageClient, TransportKind } = require("vscode-languageclient/node");
const { registerProfiling } = require("./profile");

/** The configured path to the `noeta` executable (on `PATH` by default). Shared by the LSP and DAP. */
function noetaCommand() {
  return workspace.getConfiguration("noeta").get("server.path", "noeta");
}

/** @type {import("vscode-languageclient/node").LanguageClient | undefined} */
let client;

/**
 * How to launch the server: run the configured `noeta` executable (on `PATH` by default) with the
 * `lsp` subcommand, talking JSON-RPC over stdio. The same invocation is used for normal and debug
 * runs — the server has no separate debug mode.
 */
function buildServerOptions() {
  const run = { command: noetaCommand(), args: ["lsp"], transport: TransportKind.stdio };
  return { run, debug: run };
}

/**
 * Register the debugger: a descriptor factory that launches `noeta dap` as the debug adapter, and a
 * configuration provider that lets F5 debug the active `.noe` file with no `launch.json` present.
 */
function registerDebugging(context) {
  // Spawn `noeta dap` (stdio DAP) as the adapter for every `noeta` debug session.
  const factory = {
    createDebugAdapterDescriptor() {
      return new DebugAdapterExecutable(noetaCommand(), ["dap"]);
    },
  };
  context.subscriptions.push(
    debug.registerDebugAdapterDescriptorFactory("noeta", factory),
  );

  // Fill in a runnable config: F5 with no `launch.json` synthesizes one for the active editor, and any
  // config missing `program` defaults to the file being edited.
  const provider = {
    resolveDebugConfiguration(_folder, config) {
      if (!config.type && !config.request && !config.name) {
        const editor = window.activeTextEditor;
        if (editor && editor.document.languageId === "noeta") {
          config.type = "noeta";
          config.request = "launch";
          config.name = "Debug Noeta file";
          config.program = editor.document.uri.fsPath;
        }
      }
      if (config.type === "noeta" && !config.program) {
        config.program = "${file}";
      }
      return config;
    },
  };
  context.subscriptions.push(
    debug.registerDebugConfigurationProvider("noeta", provider),
  );
}

/**
 * Register the Noeta MCP server with the editor's language-model API (VS Code 1.101+), so an AI
 * agent running in the editor (Copilot agent mode and friends) discovers `noeta mcp` without any
 * manual configuration — the compiler's own docs/check/navigate/run/debug tools, over stdio.
 */
function registerMcp(context) {
  // A host without the MCP API (an older VS Code, or a fork that doesn't ship `vscode.lm`): skip
  // quietly — highlighting, the language server, and the debugger all still work.
  if (
    !lm ||
    typeof lm.registerMcpServerDefinitionProvider !== "function" ||
    typeof McpStdioServerDefinition !== "function"
  ) {
    return;
  }
  // Re-announce the definition when the executable path setting changes, so the editor picks up
  // the new binary without a reload.
  const changed = new EventEmitter();
  context.subscriptions.push(
    workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration("noeta.server.path")) {
        changed.fire();
      }
    }),
  );
  const provider = {
    onDidChangeMcpServerDefinitions: changed.event,
    provideMcpServerDefinitions() {
      return [new McpStdioServerDefinition("noeta", noetaCommand(), ["mcp"])];
    },
    resolveMcpServerDefinition(server) {
      return server;
    },
  };
  context.subscriptions.push(
    lm.registerMcpServerDefinitionProvider("noeta.mcp", provider),
    changed,
  );
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

  // Wire the debugger (independent of the language client — breakpoints work even if the server fails
  // to launch).
  registerDebugging(context);

  // Offer `noeta mcp` to the editor's AI agents (no-op on hosts without the MCP API).
  registerMcp(context);

  // The profiler UI: `Noeta: Profile File` commands + the flame-graph view for `*.noeprof.json`.
  registerProfiling(context);

  // Starting the client spawns the server; a failure to launch (e.g. `noeta` not on `PATH`) surfaces
  // in the "Noeta Language Server" output channel.
  client.start();
}

function deactivate() {
  return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
