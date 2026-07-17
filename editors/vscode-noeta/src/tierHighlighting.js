// Declaration-driven highlighting for CUSTOM-named embedded-language tiers.
//
// The extension bundles a static injection grammar (`tier-languages.tmLanguage.json`) covering tiers
// NAMED after a well-known language (`@sql`, `@html`, …). A project can also declare a tier whose name
// differs from its language — `@tier(spec, text: "xml")` — which the static set cannot know. This
// module closes that gap: on activation, and whenever a `.noe` file changes, it scans the workspace's
// `.noe` files for `@tier(<name>, … text: "<lang>")` declarations and regenerates an injection grammar
// (`generated-tiers.tmLanguage.json`) with one rule per custom tier, injecting the declared language.
//
// VS Code loads TextMate grammars statically (there is no runtime-registration API), so a freshly
// discovered tier only takes effect after a window reload — the module writes the grammar and, when it
// changed, shows a toast offering to reload. The grammar persists, so subsequent sessions start correct.

const path = require("path");
const { workspace, window, commands, Uri } = require("vscode");

// The languages the STATIC bundled grammar (`tier-languages.tmLanguage.json`) already handles by tier
// name. A declared tier `@sql`/`@html`/… is covered there, so this module skips it.
const BUNDLED = new Set([
  "sql", "html", "css", "json", "yaml", "xml", "graphql",
  "markdown", "javascript", "python", "shell", "toml", "sparql",
]);

// Tiers the CORE language grammar already handles (built-in text tiers) — never regenerate a rule for
// them. `doc` is the built-in markdown tier baked into `noeta.tmLanguage.json`.
const CORE = new Set(["doc"]);

// A `text:` language → (TextMate scope to inject, VS Code language id). The scope is `source.<lang>`
// for most; the handful of exceptions (markup/text grammars) are listed. An unlisted language falls
// back to `source.<lang>`, which resolves if the user has that language's grammar installed.
const LANG = {
  html: ["text.html.basic", "html"],
  xml: ["text.xml", "xml"],
  markdown: ["text.html.markdown", "markdown"],
  javascript: ["source.js", "javascript"],
  shell: ["source.shell", "shellscript"],
};
function langInfo(lang) {
  return LANG[lang] || [`source.${lang}`, lang];
}

// Match `@tier(<name>, … text: "<lang>" …)` — the decorator, the tier name, and (anywhere in the same
// parenthesized argument list) a `text: "<lang>"`. Tolerant of argument order and whitespace.
const TIER_RE = /@tier\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)\b([^)]*)\)/g;
const TEXT_RE = /\btext\s*:\s*"([^"]+)"/;

// A valid TextMate/VS Code identifier fragment, so a malformed declaration can never inject markup.
function safe(id) {
  return /^[A-Za-z_][A-Za-z0-9_]*$/.test(id);
}

/** Scan the workspace's `.noe` files for custom-named tier → language declarations. */
async function scanCustomTiers() {
  const files = await workspace.findFiles("**/*.noe", "**/{node_modules,target,.git}/**", 2000);
  const tiers = new Map(); // name -> language
  for (const uri of files) {
    let text;
    try {
      text = Buffer.from(await workspace.fs.readFile(uri)).toString("utf8");
    } catch {
      continue;
    }
    for (const m of text.matchAll(TIER_RE)) {
      const name = m[1];
      const textMatch = TEXT_RE.exec(m[2]);
      if (!textMatch) continue; // not a text/embedded-language tier
      const lang = textMatch[1];
      // Skip a tier the STATIC grammar already covers (its name IS a bundled language), and any
      // malformed name/lang.
      if (!safe(name) || !safe(lang) || CORE.has(name) || (BUNDLED.has(name) && name === lang)) {
        continue;
      }
      tiers.set(name, lang);
    }
  }
  return tiers;
}

/** Build a `begin`/`end` injection rule + its brace-recursion, for one custom tier. */
function tierRules(name, lang) {
  const [scope] = langInfo(lang);
  const inner = [
    { include: "#tier-escape" },
    { include: "#tier-hole" },
    { include: `#${name}-brace` },
    { include: scope },
  ];
  return {
    [`${name}-tier`]: {
      begin: `(@)(${name})\\b\\s*(\\{)`,
      end: "\\}",
      beginCaptures: {
        1: { name: "punctuation.definition.decorator.noeta" },
        2: { name: "entity.name.function.decorator.noeta" },
        3: { name: "punctuation.section.tier.begin.noeta" },
      },
      endCaptures: { 0: { name: "punctuation.section.tier.end.noeta" } },
      contentName: `meta.embedded.block.${lang}`,
      patterns: inner,
    },
    [`${name}-brace`]: { begin: "\\{", end: "\\}", patterns: inner },
  };
}

/** Assemble the full generated grammar from the discovered custom tiers. */
function buildGrammar(tiers) {
  const repository = {
    "tier-escape": { name: "constant.character.escape.noeta", match: "\\\\[{}$\\\\]" },
    "tier-hole": {
      begin: "(\\$\\{)",
      end: "(\\})",
      beginCaptures: { 1: { name: "punctuation.section.embedded.begin.noeta" } },
      endCaptures: { 1: { name: "punctuation.section.embedded.end.noeta" } },
      contentName: "meta.embedded.line.noeta",
      patterns: [{ include: "source.noeta" }],
    },
  };
  const patterns = [];
  for (const [name, lang] of [...tiers].sort()) {
    Object.assign(repository, tierRules(name, lang));
    patterns.push({ include: `#${name}-tier` });
  }
  return {
    $comment:
      "GENERATED by the Noeta extension from the workspace's `@tier(name, text: \"lang\")` declarations. Do not edit — regenerated on activation and on .noe changes.",
    scopeName: "inline.noeta.generated-tiers",
    injectionSelector: "L:source.noeta",
    patterns,
    repository,
  };
}

/**
 * Regenerate the custom-tier grammar. Returns true if the file changed (so the caller can prompt for a
 * reload). Writes into the extension's own `syntaxes/` dir (the statically-contributed path).
 */
async function regenerate(context) {
  const target = Uri.file(
    path.join(context.extensionPath, "syntaxes", "generated-tiers.tmLanguage.json"),
  );
  const tiers = await scanCustomTiers();
  const next = JSON.stringify(buildGrammar(tiers), null, 2) + "\n";
  let current = "";
  try {
    current = Buffer.from(await workspace.fs.readFile(target)).toString("utf8");
  } catch {
    /* first run — no file yet */
  }
  if (next === current) return false;
  try {
    await workspace.fs.writeFile(target, Buffer.from(next, "utf8"));
  } catch {
    // A read-only install (e.g. some marketplace installs) — silently skip; the bundled static grammar
    // still covers well-known-named tiers.
    return false;
  }
  return true;
}

/**
 * Wire up custom-tier highlighting: regenerate on activation, on `.noe` changes (debounced), and via a
 * command; prompt for a reload when the generated grammar changed (VS Code only loads it on reload).
 */
function registerTierHighlighting(context) {
  let promptedThisSession = false;
  const maybePrompt = async (changed) => {
    if (!changed || promptedThisSession) return;
    promptedThisSession = true;
    const choice = await window.showInformationMessage(
      "Noeta: embedded-language tier highlighting was updated. Reload the window to apply it.",
      "Reload Window",
    );
    if (choice === "Reload Window") {
      commands.executeCommand("workbench.action.reloadWindow");
    }
  };

  // Regenerate on activation (persisted, so most sessions start already correct).
  regenerate(context).then(maybePrompt);

  // Regenerate when a `.noe` file changes (a new/edited `@tier` declaration), debounced so a burst of
  // saves triggers one scan.
  let timer;
  const watcher = workspace.createFileSystemWatcher("**/*.noe");
  const onChange = () => {
    clearTimeout(timer);
    timer = setTimeout(() => regenerate(context).then(maybePrompt), 400);
  };
  watcher.onDidChange(onChange);
  watcher.onDidCreate(onChange);
  watcher.onDidDelete(onChange);

  context.subscriptions.push(
    watcher,
    commands.registerCommand("noeta.refreshTierHighlighting", async () => {
      const changed = await regenerate(context);
      if (changed) {
        await maybePrompt(true);
      } else {
        window.showInformationMessage("Noeta: tier highlighting is already up to date.");
      }
    }),
  );
}

module.exports = { registerTierHighlighting };
