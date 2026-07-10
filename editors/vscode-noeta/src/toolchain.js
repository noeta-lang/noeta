// The one place the extension resolves the `noeta` executable. Every surface — the LSP client,
// the debug adapter, the build/run tasks, MCP registration, and the profiler — launches the
// toolchain through this helper, so the single `noeta.server.path` setting points the whole
// package at one binary and no surface can drift onto its own resolution rule.

const { workspace } = require("vscode");

/** The configured path to the `noeta` executable (on `PATH` by default). */
function noetaCommand() {
  return workspace.getConfiguration("noeta").get("server.path", "noeta");
}

module.exports = { noetaCommand };
