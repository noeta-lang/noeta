// The Noeta test explorer (ide-ui U3): VS Code's native Testing API over the compiler's own test
// discovery. Each open `.noe` document's `@test` fns are discovered through the language server's
// `noeta/tests` request (the same `activate_tiers` walk `noeta test` runs, so the explorer and
// the runner can never disagree), which puts run arrows in the gutter next to every test —
// inline triggering for free. Runs shell out to `noeta test <file> --json [--name <fn>…]` and
// map the machine-readable outcomes back onto the test items.

const { tests, workspace, Uri, Range, Position, TestRunProfileKind, TestMessage } = require("vscode");
const { spawn } = require("child_process");
const path = require("path");
const { noetaCommand } = require("./toolchain");

/** Run `noeta test` with `--json` and resolve the parsed outcome object. */
function runNoetaTests(file, names, token) {
  const args = ["test", file, "--json"];
  for (const name of names) {
    args.push("--name", name);
  }
  return new Promise((resolve, reject) => {
    const child = spawn(noetaCommand(), args, { cwd: path.dirname(file) });
    if (token) {
      token.onCancellationRequested(() => child.kill());
    }
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (d) => (stdout += d.toString()));
    child.stderr.on("data", (d) => (stderr += d.toString()));
    child.on("error", (err) => reject(new Error(`cannot launch ${noetaCommand()}: ${err.message}`)));
    child.on("close", () => {
      // `--json` puts exactly one JSON object on stdout; a compile error prints diagnostics on
      // stderr and produces no JSON.
      try {
        resolve(JSON.parse(stdout.trim()));
      } catch {
        reject(new Error(stderr.trim() || "noeta test produced no JSON report"));
      }
    });
  });
}

/** The report label `noeta test` uses for a test: its `#[Name]` if present, else the fn name. */
function labelOf(item) {
  return item.display || item.name;
}

function registerTests(context, getClient) {
  const controller = tests.createTestController("noeta", "Noeta Tests");

  /** Discover (or re-discover) the `@test` fns of one document into a per-file test item. */
  async function discover(document) {
    if (document.languageId !== "noeta" || document.uri.scheme !== "file") {
      return;
    }
    const client = getClient();
    if (!client) {
      return;
    }
    const uri = document.uri.toString();
    const reply = await client.sendRequest("noeta/tests", { uri }).then(
      (r) => r,
      () => undefined,
    );
    const found = (reply && reply.tests) || [];
    if (found.length === 0) {
      controller.items.delete(uri);
      return;
    }
    const fileItem =
      controller.items.get(uri) ||
      controller.createTestItem(uri, path.basename(document.uri.fsPath), document.uri);
    const children = found.map((t) => {
      const item = controller.createTestItem(`${uri}#${t.name}`, labelOf(t), document.uri);
      item.range = new Range(new Position(t.line, t.character), new Position(t.endLine, 0));
      // Stash what the run handler needs: the fn name for `--name`, the report label to match
      // outcomes, and whether the runner will skip it.
      item.noeta = { name: t.name, label: labelOf(t), skipped: t.skipped };
      return item;
    });
    fileItem.children.replace(children);
    controller.items.add(fileItem);
  }

  /** Apply one `--json` outcome object to the run, matching outcomes to items by report label
   *  (a `#[Data]` test reports one `label[row]` outcome per row — aggregated onto its item). */
  function applyOutcomes(run, fileItem, report) {
    fileItem.children.forEach((item) => {
      const { label, skipped } = item.noeta;
      if (skipped || report.skipped.includes(label)) {
        run.skipped(item);
        return;
      }
      const cases = report.tests.filter(
        (t) => t.name === label || t.name.startsWith(`${label}[`),
      );
      if (cases.length === 0) {
        return; // not selected in this run, or stopped early — leave its state untouched
      }
      const failures = cases.filter((t) => !t.passed);
      if (failures.length === 0) {
        run.passed(item);
        return;
      }
      const message = failures
        .map((f) => {
          const out = f.stdout ? `\n${f.stdout.trimEnd()}` : "";
          return `${f.name}: ${f.message || "failed"}${out}`;
        })
        .join("\n");
      run.failed(item, new TestMessage(message));
    });
  }

  async function runHandler(request, token) {
    const run = controller.createTestRun(request);
    // Group the requested items by file: a file item selects all its tests (no --name filter),
    // an individual test contributes its fn name.
    const byFile = new Map(); // uri -> { fileItem, names: Set | null (null = whole file) }
    const enqueue = (item) => {
      if (item.parent) {
        const uri = item.parent.id;
        const entry = byFile.get(uri) || { fileItem: item.parent, names: new Set() };
        if (entry.names) {
          entry.names.add(item.noeta.name);
        }
        byFile.set(uri, entry);
      } else {
        byFile.set(item.id, { fileItem: item, names: null });
      }
    };
    if (request.include) {
      request.include.forEach(enqueue);
    } else {
      controller.items.forEach((fileItem) => byFile.set(fileItem.id, { fileItem, names: null }));
    }

    for (const [uri, { fileItem, names }] of byFile) {
      if (token.isCancellationRequested) {
        break;
      }
      fileItem.children.forEach((item) => {
        if (!names || names.has(item.noeta.name)) {
          run.started(item);
        }
      });
      try {
        const report = await runNoetaTests(
          Uri.parse(uri).fsPath,
          names ? [...names] : [],
          token,
        );
        applyOutcomes(run, fileItem, report);
      } catch (err) {
        // A compile error fails every started test with the diagnostics — silent green is worse.
        fileItem.children.forEach((item) => {
          if (!names || names.has(item.noeta.name)) {
            run.failed(item, new TestMessage(String(err.message)));
          }
        });
      }
    }
    run.end();
  }

  controller.createRunProfile("Run", TestRunProfileKind.Run, runHandler, true);

  // Discover tests in newly-opened documents and re-discover on save (the server re-checks on
  // save anyway; per-keystroke discovery would churn the explorer). Already-open documents are
  // swept by `discoverAll`, which the activation calls once the language client is running.
  context.subscriptions.push(
    controller,
    workspace.onDidOpenTextDocument(discover),
    workspace.onDidSaveTextDocument(discover),
    workspace.onDidCloseTextDocument((document) => {
      controller.items.delete(document.uri.toString());
    }),
  );

  return {
    discoverAll() {
      for (const document of workspace.textDocuments) {
        discover(document);
      }
    },
  };
}

module.exports = { registerTests };
