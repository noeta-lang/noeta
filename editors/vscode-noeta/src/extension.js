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
// stack, and variables. It contributes **build/run tasks** (a `noeta` task type plus the Run/Build
// buttons on the editor title bar) that shell out to `noeta run` and `noeta build [--native]`. And it
// registers the **MCP server** (`noeta mcp`) with the editor's language-model API, so AI agents running
// in the editor discover the compiler's tools automatically. The same `noeta` binary serves every role,
// so one `noeta.server.path` setting points at all of them.
//
// Plain JavaScript on purpose: the extension runs directly after `npm install`, with no build step to
// get out of sync with the source.

const path = require("path");
const {
  workspace,
  window,
  commands,
  debug,
  lm,
  tasks,
  Task,
  TaskScope,
  TaskGroup,
  ProcessExecution,
  Uri,
  DebugAdapterExecutable,
  EventEmitter,
  McpStdioServerDefinition,
} = require("vscode");
const { LanguageClient, TransportKind } = require("vscode-languageclient/node");
const { registerProfiling } = require("./profile");
const { registerTrace } = require("./trace");
const { registerArchitecture } = require("./architecture");
const { registerDocs } = require("./docs");
const { registerTests } = require("./tests");
const { registerTierHighlighting } = require("./tierHighlighting");
const { noetaCommand } = require("./toolchain");

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

/** The absolute path of the `.noe` file in the active editor, or undefined if none is focused. */
function activeNoeFile() {
  const editor = window.activeTextEditor;
  if (editor && editor.document.languageId === "noeta") {
    return editor.document.uri.fsPath;
  }
  return undefined;
}

/**
 * Resolve a task definition's `file` to a concrete path. A concrete path is used as-is; an empty or
 * still-unsubstituted (`${…}`) value falls back to the active `.noe` editor.
 */
function resolveTaskFile(definitionFile) {
  if (definitionFile && !definitionFile.includes("${")) {
    return definitionFile;
  }
  return activeNoeFile();
}

/** The `noeta` argv for a task definition: `run <file> [-- args…]` or `build <file> [--native|--exe]`. */
function taskArgs(definition, file) {
  if (definition.command === "run") {
    const extra =
      Array.isArray(definition.args) && definition.args.length
        ? ["--", ...definition.args]
        : [];
    return ["run", file, ...extra];
  }
  // build: --native (machine code) or --exe (self-contained bytecode); neither ⇒ a plain `.noeb`.
  const flags = definition.native ? ["--native"] : definition.exe ? ["--exe"] : [];
  return ["build", file, ...flags];
}

/** A short, file-scoped label for a provided task, e.g. `run app.noe` or `build native app.noe`. */
function taskLabel(definition, file) {
  const base = path.basename(file);
  if (definition.command === "run") {
    return `run ${base}`;
  }
  const kind = definition.native ? "build native" : definition.exe ? "build exe" : "build";
  return `${kind} ${base}`;
}

/** The workspace folder owning `file`, or the workspace as a whole when the file is outside any folder. */
function taskScope(file) {
  const folder = workspace.getWorkspaceFolder(Uri.file(file));
  return folder || TaskScope.Workspace;
}

/** Build a VS Code Task that shells out to the `noeta` binary for the given definition + concrete file. */
function noetaTask(definition, file) {
  const task = new Task(
    definition,
    taskScope(file),
    taskLabel(definition, file),
    "noeta",
    new ProcessExecution(noetaCommand(), taskArgs(definition, file)),
    [],
  );
  // Make the native build the workspace's default build task (Ctrl+Shift+B): it's the only task we
  // put in the Build group, so the shortcut runs it without prompting.
  if (definition.command === "build" && definition.native) {
    task.group = TaskGroup.Build;
  }
  return task;
}

/**
 * Register the `noeta` task type: `provideTasks` offers run/build tasks for the active file (so they
 * appear in "Run Task" and the native build binds to Ctrl+Shift+B), and `resolveTask` completes tasks
 * a user authored in `tasks.json`.
 */
function registerTasks(context) {
  const provider = {
    provideTasks() {
      const file = activeNoeFile();
      if (!file) {
        return [];
      }
      return [
        noetaTask({ type: "noeta", command: "run", file }, file),
        noetaTask({ type: "noeta", command: "build", native: true, file }, file),
        noetaTask({ type: "noeta", command: "build", file }, file),
      ];
    },
    resolveTask(task) {
      const definition = task.definition;
      if (definition.type !== "noeta" || !definition.command) {
        return undefined;
      }
      const file = resolveTaskFile(definition.file);
      if (!file) {
        return undefined;
      }
      // Keep the user's authored name/scope; supply the concrete execution VS Code requires.
      const resolved = new Task(
        definition,
        task.scope || taskScope(file),
        task.name,
        "noeta",
        new ProcessExecution(noetaCommand(), taskArgs(definition, file)),
        task.problemMatchers,
      );
      if (definition.command === "build" && definition.native) {
        resolved.group = TaskGroup.Build;
      }
      return resolved;
    },
  };
  context.subscriptions.push(tasks.registerTaskProvider("noeta", provider));
}

/** Run a one-off `noeta` task for the active `.noe` file (backs the Run/Build editor-title buttons). */
function runActiveFileTask(definition) {
  const file = activeNoeFile();
  if (!file) {
    window.showErrorMessage("Open a .noe file to run this Noeta command.");
    return;
  }
  tasks.executeTask(noetaTask({ ...definition, file }, file));
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
    // One-click Run / Build-native for the active file (also on the editor title bar's run menu).
    commands.registerCommand("noeta.run", () =>
      runActiveFileTask({ type: "noeta", command: "run" }),
    ),
    commands.registerCommand("noeta.buildNative", () =>
      runActiveFileTask({ type: "noeta", command: "build", native: true }),
    ),
  );

  // Wire the debugger (independent of the language client — breakpoints work even if the server fails
  // to launch).
  registerDebugging(context);

  // Contribute the `noeta` build/run task type.
  registerTasks(context);

  // Offer `noeta mcp` to the editor's AI agents (no-op on hosts without the MCP API).
  registerMcp(context);

  // The profiler UI: `Noeta: Profile File` commands + the flame-graph view for `*.noeprof.json`.
  registerProfiling(context);

  // Declaration-driven highlighting for CUSTOM-named embedded-language tiers (`@tier(spec, text:
  // "xml")`): regenerates an injection grammar from the workspace's `@tier(…, text: "…")` declarations
  // and prompts for a reload when it changes. Well-known-named tiers (`@sql`/`@html`/…) are already
  // covered by the statically-bundled `tier-languages` grammar.
  registerTierHighlighting(context);

  // The role-trace view (ide-ui U2 / trace-view): the CodeLens-invoked `noeta.showTrace` command +
  // the dedicated trace panel it opens (served by the language server's `noeta/traceTree`).
  registerTrace(context, () => client);

  // The Architecture sidebar + the native test explorer (ide-ui U3), both fed by the server's
  // custom requests (`noeta/architecture[Children]`, `noeta/tests`).
  const architecture = registerArchitecture(context, () => client);
  const testExplorer = registerTests(context, () => client);

  // The Docs browser (docs-browser slice 3): the project + language-guide documentation tree,
  // fed by the server's `noeta/docs[Children]`/`noeta/docsPage` over the unified doc model.
  const docs = registerDocs(context, () => client);

  // Starting the client spawns the server; a failure to launch (e.g. `noeta` not on `PATH`) surfaces
  // in the "Noeta Language Server" output channel. The U3 surfaces populate once it is running
  // (their first queries before that would have returned nothing).
  client.start().then(
    () => {
      architecture.refresh();
      docs.refresh();
      testExplorer.discoverAll();
    },
    () => {},
  );
}

function deactivate() {
  return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
