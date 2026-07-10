// The Architecture view (ide-ui U3): the project's `@role` surface as a tree — roles as groups,
// their bearers beneath, and each function's outgoing calls unfolding lazily one level per
// expansion (served by the language server's `noeta/architecture[Children]` requests over the
// same static call graph the trace document and `noeta mcp` read). Click = jump to source;
// context menu: the full trace document, the native call hierarchy, a focused profile run.
//
// The view follows the **active editor's** workspace (Noeta workspaces are per-entry-file, like
// the language server's own view of the world) and refreshes on save.

const {
  window,
  workspace,
  commands,
  Uri,
  Range,
  Position,
  EventEmitter,
  TreeItem,
  TreeItemCollapsibleState,
  ThemeIcon,
} = require("vscode");
const { runProfile } = require("./profile");

/** The tree node model: either a role group or an (arch) function/leaf node. */
class ArchItem extends TreeItem {
  constructor(node, sourceUri) {
    const expandable = node.kind === "role" || node.expandable;
    super(
      node.kind === "role" ? node.role : node.name,
      expandable ? TreeItemCollapsibleState.Collapsed : TreeItemCollapsibleState.None,
    );
    this.node = node;
    this.sourceUri = sourceUri;
    if (node.kind === "role") {
      this.iconPath = new ThemeIcon("tag");
      this.contextValue = "noetaArchRole";
      this.description = `${node.bearers.length}`;
      return;
    }
    // A function or leaf node.
    this.description = describe(node);
    this.tooltip = [node.name, ...(node.roles || [])].join("\n");
    this.iconPath = icon(node);
    if (node.uri != null && node.line != null) {
      this.command = {
        title: "Open",
        command: "vscode.open",
        arguments: [
          Uri.parse(node.uri),
          { selection: new Range(new Position(node.line, node.character || 0), new Position(node.line, node.character || 0)) },
        ],
      };
    }
    // Only located functions get the trace/hierarchy/profile menu.
    if (!node.external && !node.dynamic && node.uri != null) {
      this.contextValue = "noetaArchFn";
    }
  }
}

function describe(node) {
  const parts = [];
  if (node.roles && node.roles.length) {
    parts.push(`⚑ ${node.roles.join(", ")}`);
  }
  if (node.uri != null && node.line != null) {
    parts.push(`${node.uri.split("/").pop()}:${node.line + 1}`);
  }
  if (node.reference) parts.push("reference");
  if (node.external) parts.push("external");
  if (node.dynamic) parts.push("dynamic");
  if (node.cycle) parts.push("cycle");
  return parts.join(" · ");
}

function icon(node) {
  if (node.external) return new ThemeIcon("globe");
  if (node.dynamic) return new ThemeIcon("question");
  if (node.cycle) return new ThemeIcon("sync");
  if (node.uri == null) return new ThemeIcon("symbol-misc");
  return new ThemeIcon(node.name.includes(".") ? "symbol-method" : "symbol-function");
}

class ArchitectureProvider {
  constructor(getClient) {
    this.getClient = getClient;
    this.changed = new EventEmitter();
    this.onDidChangeTreeData = this.changed.event;
    /** The `.noe` document the view reflects (the active editor's), as a URI string. */
    this.sourceUri = undefined;
  }

  setSource(uri) {
    if (uri !== this.sourceUri) {
      this.sourceUri = uri;
      this.changed.fire();
    }
  }

  refresh() {
    this.changed.fire();
  }

  getTreeItem(item) {
    return item;
  }

  async getChildren(item) {
    const client = this.getClient();
    if (!client || !this.sourceUri) {
      return [];
    }
    if (!item) {
      const reply = await client.sendRequest("noeta/architecture", { uri: this.sourceUri });
      const roles = (reply && reply.roles) || [];
      return roles.map((g) => new ArchItem({ kind: "role", ...g }, this.sourceUri));
    }
    if (item.node.kind === "role") {
      return item.node.bearers.map((b) => new ArchItem(b, this.sourceUri));
    }
    const reply = await client.sendRequest("noeta/architectureChildren", {
      uri: this.sourceUri,
      function: item.node.name,
    });
    return ((reply && reply.children) || []).map((c) => new ArchItem(c, this.sourceUri));
  }
}

/** Wire the Architecture view: the tree, active-editor tracking, and its context-menu commands. */
function registerArchitecture(context, getClient) {
  const provider = new ArchitectureProvider(getClient);
  const view = window.createTreeView("noetaArchitecture", {
    treeDataProvider: provider,
    showCollapseAll: true,
  });

  function track(editor) {
    if (editor && editor.document.languageId === "noeta" && editor.document.uri.scheme === "file") {
      provider.setSource(editor.document.uri.toString());
      view.message = undefined;
    } else if (!provider.sourceUri) {
      view.message = "Open a .noe file to see its architecture.";
    }
  }
  track(window.activeTextEditor);

  context.subscriptions.push(
    view,
    window.onDidChangeActiveTextEditor(track),
    workspace.onDidSaveTextDocument((doc) => {
      if (doc.languageId === "noeta") {
        provider.refresh();
      }
    }),
    commands.registerCommand("noeta.architectureRefresh", () => provider.refresh()),
    // Context menu on a function node: the full trace document (U2's command takes uri+fn)…
    commands.registerCommand("noeta.archTrace", (item) =>
      commands.executeCommand("noeta.showTrace", item.sourceUri, item.node.name),
    ),
    // …the native call hierarchy (opens the declaration, then invokes the built-in peek)…
    commands.registerCommand("noeta.archCallHierarchy", async (item) => {
      const { node } = item;
      await commands.executeCommand("vscode.open", Uri.parse(node.uri), {
        selection: new Range(
          new Position(node.line, node.character || 0),
          new Position(node.line, node.character || 0),
        ),
      });
      await commands.executeCommand("editor.showCallHierarchy");
    }),
    // …and a profile run focused on this function (the flame view re-roots at it).
    commands.registerCommand("noeta.archProfileFocused", (item) =>
      runProfile("sampling", item.node.name),
    ),
  );

  return {
    // Called by the activation once the language client is running (an initial getChildren
    // before that returned nothing).
    refresh: () => provider.refresh(),
  };
}

module.exports = { registerArchitecture };
