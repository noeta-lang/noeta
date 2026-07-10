// The Noeta profiler UI: the `Noeta: Profile File` commands and the custom editor that renders a
// profile artifact (`*.noeprof.json`) as a flame graph + function table inside VS Code.
//
// The commands run the same `noeta` binary the language server and debugger use (`noeta profile`),
// write the machine artifact to a temp file, and open it with the `noeta.profileView` custom
// editor. The artifact stays a *standard* format — speedscope JSON for a sampling run (each frame
// carrying structured `file`/`line`/`col`), the instrumenting profiler's `{"functions": [...]}`
// JSON for an exact run — so the same file also opens in external tools (speedscope.app), and a
// profile produced by hand on the CLI opens in this view.
//
// All rendering happens in `media/profile.js` inside the webview, styled exclusively with the
// editor's own `--vscode-*` theme variables so the view tracks light/dark/high-contrast themes.

const {
  window,
  workspace,
  commands,
  Uri,
  Range,
  Selection,
  ViewColumn,
  ProgressLocation,
  TextEditorRevealType,
} = require("vscode");
const { spawn } = require("child_process");
const path = require("path");
const os = require("os");
const fs = require("fs");

/**
 * Per-artifact metadata the run commands remember for the viewer: the profiler's own one-line
 * summary (samples/stacks/wall-time — it goes to stderr, not into the artifact) and the directory
 * of the profiled program (for resolving relative source paths in frames). An artifact opened
 * standalone (produced on the CLI by hand) simply has no entry here and the view shows no summary.
 * Keyed by artifact fsPath.
 * @type {Map<string, {summary: string, sourceDir: string, program: string}>}
 */
const artifactMeta = new Map();

/** The configured path to the `noeta` executable — the same setting the LSP/DAP/MCP use. */
function noetaCommand() {
  return workspace.getConfiguration("noeta").get("server.path", "noeta");
}

/** One shared output channel for the profiled program's own stdout/stderr. */
let channel;
function profileChannel() {
  if (!channel) {
    channel = window.createOutputChannel("Noeta Profile");
  }
  return channel;
}

/**
 * Run `noeta profile` on the active `.noe` file and open the resulting artifact in the profile
 * view. `mode` is `"sampling"` (wall-clock flamegraph with line attribution) or `"instrument"`
 * (exact per-function counts).
 */
async function runProfile(mode) {
  const editor = window.activeTextEditor;
  if (!editor || editor.document.languageId !== "noeta") {
    window.showErrorMessage("Noeta: open a .noe file to profile.");
    return;
  }
  if (editor.document.isDirty) {
    await editor.document.save();
  }
  const program = editor.document.uri.fsPath;
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const artifact = path.join(
    os.tmpdir(),
    `${path.basename(program, ".noe")}-${stamp}.noeprof.json`,
  );

  // Sampling runs with --lines so the flamegraph leaves (and, later, editor line annotations)
  // resolve to the hot source line, not just the hot function.
  const hz = workspace.getConfiguration("noeta").get("profile.hz", 1000);
  const args =
    mode === "instrument"
      ? ["profile", "--instrument", "--format", "json", "-o", artifact, program]
      : ["profile", "--hz", String(hz), "--lines", "--format", "speedscope", "-o", artifact, program];

  const out = profileChannel();
  out.appendLine(`> ${noetaCommand()} ${args.join(" ")}`);

  const summary = await window.withProgress(
    {
      location: ProgressLocation.Notification,
      title: `Profiling ${path.basename(program)}…`,
      cancellable: true,
    },
    (_progress, token) =>
      new Promise((resolve, reject) => {
        const child = spawn(noetaCommand(), args, { cwd: path.dirname(program) });
        token.onCancellationRequested(() => child.kill());
        let summaryLine = "";
        child.stdout.on("data", (data) => out.append(data.toString()));
        child.stderr.on("data", (data) => {
          const text = data.toString();
          out.append(text);
          // The profiler's own report lines are prefixed; keep the one-line run summary for the
          // view's header.
          for (const line of text.split("\n")) {
            if (line.startsWith("noeta profile:") && !line.includes("wrote")) {
              summaryLine = line.replace("noeta profile:", "").trim();
            }
          }
        });
        child.on("error", (err) => reject(new Error(`cannot launch ${noetaCommand()}: ${err.message}`)));
        child.on("close", () => {
          if (token.isCancellationRequested) {
            reject(new Error("profiling cancelled"));
            return;
          }
          // A non-zero program exit still produces a profile (the run up to the abort); only a
          // missing artifact — compile error, bad flags — is a failure.
          if (!fs.existsSync(artifact)) {
            reject(new Error("no profile was produced — see the Noeta Profile output"));
            return;
          }
          resolve(summaryLine);
        });
      }),
  ).then(
    (s) => s,
    (err) => {
      if (String(err.message) !== "profiling cancelled") {
        out.show(true);
        window.showErrorMessage(`Noeta profile failed: ${err.message}`);
      }
      return undefined;
    },
  );
  if (summary === undefined) {
    return;
  }

  artifactMeta.set(artifact, {
    summary,
    sourceDir: path.dirname(program),
    program,
  });
  await commands.executeCommand(
    "vscode.openWith",
    Uri.file(artifact),
    "noeta.profileView",
    { viewColumn: ViewColumn.Active, preserveFocus: false },
  );
}

/**
 * Resolve a frame's `file` string to an on-disk file and reveal `line`/`col` in an editor. Frames
 * carry the path as the compiler saw it: absolute when the program was profiled by path (the
 * commands above), possibly relative for a hand-run CLI profile — then it resolves against the
 * profiled program's directory (if known) or the workspace folders.
 */
async function openSource(file, line, col, meta) {
  const candidates = [];
  if (path.isAbsolute(file)) {
    candidates.push(file);
  } else {
    if (meta) {
      candidates.push(path.join(meta.sourceDir, file));
    }
    for (const folder of workspace.workspaceFolders ?? []) {
      candidates.push(path.join(folder.uri.fsPath, file));
    }
  }
  let target = candidates.find((c) => fs.existsSync(c));
  if (!target) {
    // Last resort: find the file anywhere in the workspace by its basename.
    const found = await workspace.findFiles(`**/${path.basename(file)}`, "**/node_modules/**", 1);
    if (found.length > 0) {
      target = found[0].fsPath;
    }
  }
  if (!target) {
    window.showWarningMessage(`Noeta: cannot find source file ${file}`);
    return;
  }
  const doc = await workspace.openTextDocument(Uri.file(target));
  const editor = await window.showTextDocument(doc, { viewColumn: ViewColumn.One });
  const position = doc.lineAt(Math.min(Math.max((line ?? 1) - 1, 0), doc.lineCount - 1)).range.start.translate(0, Math.max((col ?? 1) - 1, 0));
  editor.selection = new Selection(position, position);
  editor.revealRange(new Range(position, position), TextEditorRevealType.InCenter);
}

/** A little HTML-attribute-safe nonce for the webview's CSP. */
function nonce() {
  let text = "";
  const chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  for (let i = 0; i < 32; i++) {
    text += chars.charAt(Math.floor(Math.random() * chars.length));
  }
  return text;
}

/**
 * The read-only custom editor for `*.noeprof.json`: loads the artifact, detects its kind
 * (speedscope sampling profile vs instrumenting function table), and hands the parsed JSON to the
 * webview, which owns all rendering. Source-navigation requests come back over the message channel.
 */
class ProfileViewProvider {
  constructor(context) {
    this.context = context;
  }

  async openCustomDocument(uri) {
    return { uri, dispose() {} };
  }

  async resolveCustomEditor(document, panel) {
    const media = Uri.joinPath(this.context.extensionUri, "media");
    panel.webview.options = {
      enableScripts: true,
      localResourceRoots: [media],
    };

    const scriptUri = panel.webview.asWebviewUri(Uri.joinPath(media, "profile.js"));
    const styleUri = panel.webview.asWebviewUri(Uri.joinPath(media, "profile.css"));
    const n = nonce();
    panel.webview.html = `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; style-src ${panel.webview.cspSource}; script-src 'nonce-${n}';">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<link href="${styleUri}" rel="stylesheet">
<title>Noeta Profile</title>
</head>
<body>
<div id="app" aria-live="polite"></div>
<script nonce="${n}" src="${scriptUri}"></script>
</body>
</html>`;

    const meta = artifactMeta.get(document.uri.fsPath);
    panel.webview.onDidReceiveMessage(async (msg) => {
      if (msg.type === "ready") {
        let profile;
        try {
          const bytes = await workspace.fs.readFile(document.uri);
          profile = JSON.parse(Buffer.from(bytes).toString("utf8"));
        } catch (err) {
          panel.webview.postMessage({ type: "error", message: `cannot read profile: ${err.message}` });
          return;
        }
        const kind = Array.isArray(profile.functions) ? "instrument" : "sampling";
        panel.webview.postMessage({
          type: "profile",
          kind,
          profile,
          meta: {
            summary: meta?.summary ?? "",
            program: meta?.program ? path.basename(meta.program) : path.basename(document.uri.fsPath),
          },
        });
      } else if (msg.type === "openSource") {
        await openSource(msg.file, msg.line, msg.col, meta);
      }
    });
  }
}

/** Wire the profiler UI: the two run commands and the profile custom editor. */
function registerProfiling(context) {
  context.subscriptions.push(
    commands.registerCommand("noeta.profileFile", () => runProfile("sampling")),
    commands.registerCommand("noeta.profileFileInstrumented", () => runProfile("instrument")),
    window.registerCustomEditorProvider("noeta.profileView", new ProfileViewProvider(context), {
      webviewOptions: { retainContextWhenHidden: true },
      supportsMultipleEditorsPerDocument: false,
    }),
  );
}

module.exports = { registerProfiling };
