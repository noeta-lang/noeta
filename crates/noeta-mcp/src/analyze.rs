//! Shared plumbing for the Understand + Introspect tools: build a salsa workspace from a
//! `source`/`file` request (the same shape `check` takes), and convert byte spans ↔ line/column so
//! a tool can report *where* in the source an answer sits and an agent can point at a position.
//!
//! Every Understand and Introspect tool is a pure read over the public salsa graph (`noeta-db`) or
//! the parsed AST (`noeta-ast`) — no VM, no host. Each builds a fresh `LangDatabase` per call,
//! exactly as `check` does.

use noeta_db::{LangDatabase, Workspace};
use noeta_span::{Source, SourceId};
use rmcp::schemars;
use serde::Serialize;

/// A prepared analysis context: the database, the workspace handle, and the ordered sources (entry
/// at index 0). Held together because the salsa queries borrow the database.
pub struct Prepared {
    pub db: LangDatabase,
    pub ws: Workspace,
    pub sources: Vec<Source>,
    /// One entry per [`Self::sources`] index: the module identity that source's *location* derives
    /// and the dependency package it belongs to. The graph tools' node identity reads from here, so
    /// a module's namespace is the linker's answer rather than a re-derivation.
    pub modules: Vec<ModuleIdentity>,
    /// The root package's directory, when the request named a file inside one — what a node id's
    /// `file` is reported relative to.
    pub root: Option<std::path::PathBuf>,
}

/// What a source's location says about it: the dotted module path it derives, and the dependency
/// package it belongs to (`None` for a workspace member).
#[derive(Debug, Clone, Default)]
pub struct ModuleIdentity {
    /// The dotted module path the file's location derives, empty when its location derives none
    /// (an inline source, or a file whose name cannot spell a path).
    pub namespace: String,
    /// The dependency package's module prefix (`toolkit`, `para.db`), for a module outside the
    /// workspace members.
    pub package: Option<String>,
}

impl Prepared {
    /// The entry file's source text (`SourceId::FIRST`) — what positions and the line index resolve
    /// against.
    pub fn entry_text(&self) -> &str {
        self.sources[0].text()
    }

    /// Whether `target` (a dotted callee label like `alpha.alpha_only`) leads with a module this
    /// **project** declares. A call graph built over an unlinked program classifies such a target
    /// as external, which is a lie an agent has no way to catch: it reads as "this leaves your
    /// code" when the declaration is two files away.
    pub fn names_a_project_module(&self, target: &str) -> bool {
        let head = target.split('.').next().unwrap_or(target);
        self.modules.iter().any(|m| {
            m.package.is_none()
                && !m.namespace.is_empty()
                && (m.namespace == head || m.namespace.ends_with(&format!(".{head}")))
        })
    }

    /// A source's reported file name: relative to the root package's directory when it sits inside
    /// one, so two tools name the same file identically regardless of how the request spelled it.
    pub fn file_name(&self, index: usize) -> Option<String> {
        let source = self.sources.get(index)?;
        Some(self.relative(source.name()))
    }

    /// `name` relative to the root package directory, or `name` unchanged when it sits outside it.
    fn relative(&self, name: &str) -> String {
        let Some(root) = &self.root else {
            return name.to_string();
        };
        std::path::Path::new(name)
            .strip_prefix(root)
            .map(|rel| rel.display().to_string())
            .unwrap_or_else(|_| name.to_string())
    }

    /// The stable identity of a declaration: its post-link name, its kind, the file it is declared
    /// in, and its declared name's span. Two tools that report the same declaration report an equal
    /// [`NodeId`], which is what lets an agent join their answers.
    pub fn node_id(&self, name: &str, kind: NodeKind, span: noeta_span::Span) -> NodeId {
        let index = span.source.0 as usize;
        let (file, at) = match self.sources.get(index) {
            Some(source) => {
                let loc = LineIndex::new(source.text()).loc(span.start);
                (
                    Some(self.relative(source.name())),
                    Some(NodeSpan {
                        start: span.start,
                        end: span.end,
                        line: loc.line,
                        column: loc.column,
                    }),
                )
            }
            None => (None, None),
        };
        NodeId {
            name: name.to_string(),
            kind,
            file,
            span: at,
        }
    }

    /// The identity of a node with no declaration in this program — an external module target or a
    /// dynamic callee. It carries a name and a kind and nothing to open.
    pub fn unlocated_id(&self, name: &str, kind: NodeKind) -> NodeId {
        NodeId {
            name: name.to_string(),
            kind,
            file: None,
            span: None,
        }
    }
}

/// What a graph node **is**. One vocabulary across every tool, so a node's kind means the same
/// thing whether `symbols`, `trace`, `reflect`, `module_graph`, `impact` or `callers` reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// A top-level `fn`.
    Function,
    /// A method declared inside a type or an `impl` block.
    Method,
    Struct,
    Class,
    Enum,
    /// One variant of an enum.
    Variant,
    /// One field of a struct or class.
    Field,
    Trait,
    /// A standalone `impl Trait for Type` block.
    Impl,
    /// A source file's module.
    Module,
    /// A native/module target outside the program (`math.sqrt`).
    External,
    /// A callee reached through a value — statically unresolvable.
    Dynamic,
    /// A target this project declares that the analysis could not resolve — what an external
    /// classification degrades to when the workspace did not link.
    Unresolved,
}

impl NodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::Function => "function",
            NodeKind::Method => "method",
            NodeKind::Struct => "struct",
            NodeKind::Class => "class",
            NodeKind::Enum => "enum",
            NodeKind::Variant => "variant",
            NodeKind::Field => "field",
            NodeKind::Trait => "trait",
            NodeKind::Impl => "impl",
            NodeKind::Module => "module",
            NodeKind::External => "external",
            NodeKind::Dynamic => "dynamic",
            NodeKind::Unresolved => "unresolved",
        }
    }
}

impl std::fmt::Display for NodeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A declared name's span: the byte range plus the 1-based line and (UTF-8 byte) column of its
/// start. Byte offsets are what the engine keys declarations by; the line/column is what an agent
/// opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, schemars::JsonSchema)]
pub struct NodeSpan {
    pub start: u32,
    pub end: u32,
    pub line: u32,
    pub column: u32,
}

/// The identity every graph tool emits alongside its own fields. `(file, span.start, span.end)` is
/// the join key — the declaration's name span, the one thing the engine itself keys nodes by —
/// while `name` is how `trace`, `impact` and `callers` are addressed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, schemars::JsonSchema)]
pub struct NodeId {
    /// The post-link name: namespace-qualified for a declaration in a package (`app.main.handle`),
    /// `Type.method` for a method.
    pub name: String,
    pub kind: NodeKind,
    /// The declaring file, relative to the project root. `null` for a node with no declaration in
    /// this program (an external or dynamic callee).
    pub file: Option<String>,
    /// The declared name's span. `null` alongside a `null` file.
    pub span: Option<NodeSpan>,
}

/// Build a workspace from a `check`-style request. `source` is a lone inline entry; `file` pulls in
/// its sibling `.noe` modules **and its `noeta.toml` dependency packages**, so every import
/// resolves and every tool sees the same program the compiler does. Exactly one must be present.
///
/// The dependency half is not a nicety: the role a package's `@role`-bearing attribute confers is
/// declared *in that package*, so with the package unlinked `reflect` listed the attribute and
/// reported no role at all. Each source is analyzed under its own package's edition (the root's for
/// the entry and its siblings) — [`crate::ResolvedWorkspace::workspace`] is the single place that
/// decision is made, so no tool can analyze a dependency-less slice of a program by accident.
pub fn prepare(
    source: &Option<String>,
    file: &Option<String>,
) -> Result<Prepared, rmcp::ErrorData> {
    let resolved = crate::resolve_workspace(source, file)?;
    let db = LangDatabase::default();
    let ws = resolved.workspace(&db);
    let modules = module_identities(&resolved);
    let root = file
        .as_deref()
        .map(std::path::Path::new)
        .and_then(noeta_pm::sources::package_root)
        .map(|root| root.dir);
    Ok(Prepared {
        db,
        ws,
        sources: resolved.sources,
        modules,
        root,
    })
}

/// The per-source module identity table, in [`Prepared::sources`] order: each member's derived
/// module path, then each dependency package's modules tagged with the package's prefix.
fn module_identities(resolved: &crate::ResolvedWorkspace) -> Vec<ModuleIdentity> {
    let dotted = |path: &noeta_loader::ModulePath| {
        path.derived()
            .map(|segments| segments.join("."))
            .unwrap_or_default()
    };
    let members = resolved.members().len();
    let mut identities: Vec<ModuleIdentity> = (0..members)
        .map(|i| ModuleIdentity {
            namespace: resolved.paths.get(i).map(dotted).unwrap_or_default(),
            package: None,
        })
        .collect();
    for dep in &resolved.deps {
        let package = dep.prefix.join(".");
        for (i, _) in dep.modules.iter().enumerate() {
            identities.push(ModuleIdentity {
                namespace: dep.paths.get(i).map(dotted).unwrap_or_default(),
                package: Some(package.clone()),
            });
        }
    }
    identities
}

/// Whether the workspace linked, and what stopped it when it did not.
///
/// Every graph tool falls back to the entry file's own parse when the link fails, because half an
/// answer beats none. The fallback changes what the answer *means*: names lose their qualification,
/// and a call into a sibling module resolves to nothing, which the call graph then labels
/// `external`. So the status rides on the answer, and the tools mark their nodes with it.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct LinkStatus {
    /// True when the answer was computed over the merged workspace program.
    pub linked: bool,
    /// The diagnostics that stopped the link, in the same JSON shape `check` reports. Empty when
    /// `linked`.
    pub link_diagnostics: Vec<noeta_diagnostics::JsonDiagnostic>,
}

impl LinkStatus {
    /// The workspace's link status, with the diagnostics resolved against its sources.
    pub fn of(p: &Prepared) -> LinkStatus {
        match &noeta_db::linked(&p.db, p.ws).program {
            Ok(_) => LinkStatus {
                linked: true,
                link_diagnostics: Vec::new(),
            },
            Err(diagnostics) => {
                let sources = noeta_span::SourceMap::new(p.sources.clone());
                LinkStatus {
                    linked: false,
                    link_diagnostics: diagnostics
                        .iter()
                        .map(|d| noeta_diagnostics::to_json(&sources, d))
                        .collect(),
                }
            }
        }
    }

    /// The sentence a tool puts in its `note` when it answered from the fallback, naming the first
    /// diagnostic. `None` when the workspace linked.
    pub fn note(&self) -> Option<String> {
        if self.linked {
            return None;
        }
        let first = self
            .link_diagnostics
            .first()
            .map(|d| format!("{}: {}", d.code, d.message))
            .unwrap_or_else(|| "the modules could not be merged".to_string());
        Some(format!(
            "the workspace did not link ({first}) — this answer comes from the entry file's own \
             parse, so names are unqualified and a call into a sibling module can be reported as \
             an external target"
        ))
    }
}

/// A resolved source location: 1-based line and column plus the raw byte offset. The column counts
/// **UTF-8 bytes** within the line (1-based), so `offset == line_start + column - 1` exactly — the
/// unit agents and byte-span math agree on.
#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema)]
pub struct Loc {
    pub line: u32,
    pub column: u32,
    pub offset: u32,
}

/// A span resolved to its start/end [`Loc`]s — how every analysis tool reports "where".
#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema)]
pub struct SpanLoc {
    pub start: Loc,
    pub end: Loc,
}

/// A byte-offset ↔ 1-based line/column index over one source text: the MCP wire's convention (a
/// 1-based UTF-8 **byte** column) as a thin adapter over the shared [`noeta_ide::LineIndex`] — the
/// same conversion engine the LSP serves, so the two surfaces cannot drift on position math
/// (audit-4 finding 8). Under `Encoding::Utf8` the ide character *is* the byte column, so the
/// adapter is exactly a ±1 shift on both axes.
pub struct LineIndex<'a> {
    inner: noeta_ide::LineIndex<'a>,
    /// The text length, for the clamp [`LineIndex::loc`] reports back in [`Loc::offset`] (the
    /// shared index clamps internally but does not echo the clamped offset).
    len: u32,
}

impl<'a> LineIndex<'a> {
    pub fn new(text: &'a str) -> Self {
        LineIndex {
            inner: noeta_ide::LineIndex::new(text),
            len: text.len() as u32,
        }
    }

    /// Resolve a byte offset to its 1-based line/column (offsets past the end clamp to it).
    pub fn loc(&self, offset: u32) -> Loc {
        let offset = offset.min(self.len);
        let pos = self.inner.position(offset, noeta_ide::Encoding::Utf8);
        Loc {
            line: pos.line + 1,
            column: pos.character + 1,
            offset,
        }
    }

    /// Resolve a 1-based line/column to a byte offset (clamped into the text — a column past its
    /// line's end lands on the line's last content byte, a line past the text on its end). The
    /// inverse of [`LineIndex::loc`] for a `check`-style caller that only has a position.
    pub fn offset(&self, line: u32, column: u32) -> u32 {
        self.inner.offset(
            noeta_ide::Position::new(line.saturating_sub(1), column.saturating_sub(1)),
            noeta_ide::Encoding::Utf8,
        )
    }

    pub fn span_loc(&self, span: noeta_span::Span) -> SpanLoc {
        SpanLoc {
            start: self.loc(span.start),
            end: self.loc(span.end),
        }
    }
}

/// Locate the byte offset of a **symbol** in the entry text: the first whole-word occurrence of
/// `name` (identifier boundaries on both sides), so an agent can ask "what's the type of `total`"
/// without computing a position. `None` if the name never appears as a standalone identifier.
pub fn symbol_offset(text: &str, name: &str) -> Option<u32> {
    symbol_offsets(text, name).first().copied()
}

/// Every whole-word occurrence of `name` in the entry text, in order. The navigation tools probe
/// occurrences until one resolves (the first may be the declaration itself, which is not a "use"
/// the resolver indexes).
pub fn symbol_offsets(text: &str, name: &str) -> Vec<u32> {
    if name.is_empty() {
        return Vec::new();
    }
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let bytes = text.as_bytes();
    let mut offsets = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = text[from..].find(name) {
        let at = from + rel;
        let before_ok = at == 0 || !is_ident(bytes[at - 1] as char);
        let after = at + name.len();
        let after_ok = after >= bytes.len() || !is_ident(bytes[after] as char);
        if before_ok && after_ok {
            offsets.push(at as u32);
        }
        from = at + name.len();
    }
    offsets
}

/// Resolve a span to its owning file's name and line/column location — spans in the merged
/// workspace program keep their per-file [`SourceId`]s, so a role or attribute target in a sibling
/// module locates correctly. `None` for a span outside the prepared sources.
pub fn locate_span(p: &Prepared, span: noeta_span::Span) -> Option<(String, SpanLoc)> {
    let source = p.sources.get(span.source.0 as usize)?;
    let index = LineIndex::new(source.text());
    Some((source.name().to_string(), index.span_loc(span)))
}

/// The entry file's [`SourceProgram`] — the workspace's own first-member input (ide-workspaces:
/// `prepare` builds the entry at index 0), so the per-file `ast`/`tokens` queries memoize against
/// the same input the workspace query family reads. Previously this minted a fresh input per
/// call, duplicating the entry in the database and defeating per-file memoization.
pub fn entry_program(p: &Prepared) -> noeta_db::SourceProgram {
    noeta_db::workspace_entry(&p.db, p.ws)
}

/// Whether a span belongs to the entry file (spans from imported siblings are filtered out of
/// position-addressed answers).
pub fn in_entry(span: noeta_span::Span) -> bool {
    span.source == SourceId::FIRST
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_index_round_trips() {
        let text = "ab\ncde\n\nfg";
        let idx = LineIndex::new(text);
        // Offsets → line/column (1-based; column is a 1-based byte column).
        assert_eq!((idx.loc(0).line, idx.loc(0).column), (1, 1)); // 'a'
        assert_eq!((idx.loc(3).line, idx.loc(3).column), (2, 1)); // 'c'
        assert_eq!((idx.loc(7).line, idx.loc(7).column), (3, 1)); // empty line
        assert_eq!((idx.loc(8).line, idx.loc(8).column), (4, 1)); // 'f'
        // Position → offset is the inverse.
        assert_eq!(idx.offset(2, 1), 3);
        assert_eq!(idx.offset(4, 2), 9);
        // Out-of-range column clamps into the line rather than overshooting.
        assert!(idx.offset(1, 999) <= text.len() as u32);
    }

    #[test]
    fn symbol_offset_matches_whole_words_only() {
        let text = "total = subtotal + total_x + total";
        // The first *standalone* `total`, not the one inside `subtotal` or `total_x`.
        assert_eq!(symbol_offset(text, "total"), Some(0));
        assert_eq!(symbol_offset(text, "subtotal"), Some(8));
        assert_eq!(symbol_offset(text, "missing"), None);
    }
}
