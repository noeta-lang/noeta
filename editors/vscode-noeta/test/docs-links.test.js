// Regression tests for the docs page browser's cross-linking (media/docs.js).
//
// The wiki's prose links are bare page names — `[Dev Tiers](Dev-Tiers)`, sometimes with a
// `#heading` — because that is what GitHub and the website resolve. The webview used to render
// every one of them as plain text, so the docs' cross-references were dead ends inside the editor.
// They now become in-panel navigation, but ONLY for pages that exist (the panel is told the guide's
// slugs), and headings carry the same anchors the server's `github_anchor` produces so a
// `Page#section` link lands on the right heading.
//
// Run: npm test  (plain node; no test framework)

const assert = require("assert");
const path = require("path");

// media/docs.js is a webview IIFE: stub the two globals it grabs on load, then read its test hook.
global.acquireVsCodeApi = () => ({ postMessage() {} });
global.document = { getElementById: () => ({}) };
global.window = { addEventListener() {} };

const { renderMarkdown, renderInline, anchorSlug, setKnownPages } = require(
  path.join(__dirname, "..", "media", "docs.js"),
);

let failures = 0;
function test(name, fn) {
  try {
    fn();
    console.log(`  ok    ${name}`);
  } catch (err) {
    failures++;
    console.error(`  FAIL  ${name}\n        ${err.message}`);
  }
}

// The guide corpus the extension reports; anything outside it must stay plain text.
const KNOWN = ["Dev-Tiers", "Control-Flow-and-Pattern-Matching", "Std", "Derives"];
setKnownPages(KNOWN);

console.log("docs page-browser links");

test("a bare page link becomes an in-panel link", () => {
  const html = renderInline("see [Dev Tiers](Dev-Tiers) for the model");
  assert.match(html, /<a class="doc-link" data-page="Dev-Tiers" data-anchor="">Dev Tiers<\/a>/);
});

test("a page link keeps its #fragment", () => {
  const html = renderInline("[build targets](Dev-Tiers#build-targets--noetatoml)");
  assert.match(html, /data-page="Dev-Tiers"/);
  assert.match(html, /data-anchor="build-targets--noetatoml"/);
});

test("an unknown page stays plain text", () => {
  const html = renderInline("[Retired Page](Retired-Page) and [a file](AGENTS.md)");
  assert.ok(!html.includes("<a"), `expected no link, got: ${html}`);
  assert.ok(html.includes("Retired Page") && html.includes("a file"));
});

test("a repo-relative path stays plain text", () => {
  const html = renderInline("[the example](examples/liveview_counter.noe)");
  assert.ok(!html.includes("<a"), `expected no link, got: ${html}`);
});

test("a same-page fragment becomes a scroll link", () => {
  const html = renderInline("[below](#where-inference-stops)");
  assert.match(html, /<a class="doc-link" data-anchor="where-inference-stops">below<\/a>/);
  assert.ok(!html.includes("data-page"));
});

test("external links keep their href", () => {
  const html = renderInline("[spec](https://example.com/a?b=1)");
  assert.match(html, /<a href="https:\/\/example\.com\/a\?b=1">spec<\/a>/);
});

test("links inside inline code are left alone", () => {
  const html = renderInline("write `[label](Dev-Tiers)` verbatim");
  assert.ok(!html.includes("<a"), `expected no link inside code, got: ${html}`);
});

test("a target that could break out of an attribute never becomes a link", () => {
  // Even with such a page "known", the strict page-name charset rejects it, so it degrades to
  // text rather than emitting an attribute with a quote in it.
  try {
    setKnownPages(['Ev"il', "a b", "x/y"]);
    for (const src of ['[x](Ev"il)', "[x](a b)", "[x](x/y)"]) {
      const html = renderInline(src);
      assert.ok(!html.includes("<a"), `expected plain text for ${src}, got: ${html}`);
    }
  } finally {
    setKnownPages(KNOWN);
  }
});

console.log("heading anchors");

test("anchors match the server's github_anchor slugging", () => {
  // The case in noeta-ide/src/guide.rs's own unit test, plus the em-dash headings our pages use.
  assert.strictEqual(anchorSlug("The `@doc` Tier!"), "the-doc-tier");
  assert.strictEqual(anchorSlug("Build targets — `noeta.toml`"), "build-targets--noetatoml");
  assert.strictEqual(anchorSlug("Guards"), "guards");
});

test("rendered headings carry the anchor id", () => {
  const html = renderMarkdown("## Build targets — `noeta.toml`\n\nbody\n");
  assert.match(html, /<h2 id="build-targets--noetatoml">/);
});

test("a page link inside a table cell still renders", () => {
  const html = renderMarkdown("| Page | What |\n|---|---|\n| [Derives](Derives) | derive |\n");
  assert.match(html, /data-page="Derives"/);
});

if (failures) {
  console.error(`\n${failures} failed`);
  process.exit(1);
}
console.log("\nall passed");
