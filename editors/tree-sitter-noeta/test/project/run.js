#!/usr/bin/env node
// Corpus test for the PER-PROJECT generated grammar (text-tiers arc).
//
// The static grammar recognizes only the std `doc` text tier; a project's `@tier(<name>, text: …)`
// declarations are widened in via a generated `project-tiers.json` overlay (emitted by
// `noeta grammar tree-sitter`). This harness reproduces that overlay in a throwaway build: it copies
// the grammar sources into a temp dir, drops in a `project-tiers.json` naming custom tiers
// (`spec`, `query`), runs `tree-sitter generate`, and runs the project corpus under `tree-sitter
// test` — proving those custom tiers now parse as verbatim `text_body` bodies (which the static
// grammar refuses, pinned by test/corpus/basics.txt). It also asserts the generation is
// deterministic: generating twice yields a byte-identical `grammar.json`.

const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");

const pkgDir = path.resolve(__dirname, "..", "..");
const treeSitter = path.join(pkgDir, "node_modules", ".bin", "tree-sitter");

if (!fs.existsSync(treeSitter)) {
  console.error(
    "test:project: the tree-sitter CLI is not installed — run `npm install` in the grammar package first.",
  );
  process.exit(1);
}

// The overlay a project with `@tier(spec, text: "xml")` + `@tier(query, text: "sql")` would generate.
const OVERLAY = {
  $comment: "test overlay",
  textTiers: ["doc", "query", "spec"],
};

const work = fs.mkdtempSync(path.join(os.tmpdir(), "noeta-ts-project-"));
function cleanup() {
  fs.rmSync(work, { recursive: true, force: true });
}
process.on("exit", cleanup);

try {
  // Copy the grammar sources the generated build needs.
  fs.mkdirSync(path.join(work, "src"), { recursive: true });
  for (const rel of ["grammar.js", "tree-sitter.json", "package.json"]) {
    fs.copyFileSync(path.join(pkgDir, rel), path.join(work, rel));
  }
  fs.copyFileSync(
    path.join(pkgDir, "src", "scanner.c"),
    path.join(work, "src", "scanner.c"),
  );

  // The per-project overlay that widens the verbatim text-tier set.
  fs.writeFileSync(
    path.join(work, "project-tiers.json"),
    JSON.stringify(OVERLAY, null, 2) + "\n",
  );

  // The project corpus (custom tiers must now be verbatim).
  fs.mkdirSync(path.join(work, "test", "corpus"), { recursive: true });
  fs.copyFileSync(
    path.join(__dirname, "corpus", "generated.txt"),
    path.join(work, "test", "corpus", "generated.txt"),
  );

  const run = (args) =>
    execFileSync(treeSitter, args, { cwd: work, stdio: "inherit" });

  console.log("test:project: generating the overlaid grammar…");
  run(["generate"]);
  const firstGrammar = fs.readFileSync(path.join(work, "src", "grammar.json"));

  console.log("test:project: running the project corpus…");
  run(["test"]);

  // Determinism: a second generation from the same overlay must be byte-identical.
  run(["generate"]);
  const secondGrammar = fs.readFileSync(path.join(work, "src", "grammar.json"));
  if (!firstGrammar.equals(secondGrammar)) {
    console.error("test:project: FAILED — grammar generation is not deterministic.");
    process.exit(1);
  }
  console.log("test:project: OK (custom tiers parse verbatim; generation is deterministic).");
} catch (err) {
  console.error("test:project: FAILED");
  if (err && err.status !== undefined) process.exit(err.status || 1);
  console.error(err);
  process.exit(1);
}
