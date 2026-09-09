//! The Understand pillar's navigation half: `definition` / `references` / `completions` /
//! `signature` over the shared IDE engine ([`noeta_ide::DocumentStore`]) — the exact resolver,
//! occurrence index, and completion logic the LSP serves, so an agent and an editor can never
//! disagree about where a symbol lives.
//!
//! Each call builds a fresh store (stateless, like every other tool), opens the entry as a
//! document, and queries it. A `file` entry gets the store's full workspace behavior — sibling
//! `.noe` modules and resolved dependency packages — so cross-file navigation works; an inline
//! `source` is a lone document. Positions are 1-based line/column (column in UTF-8 bytes, the
//! unit the whole server speaks); the engine's 0-based UTF-8 positions convert at this boundary.

use noeta_ide::{DocumentStore, Encoding, completion};
use rmcp::ErrorData;
use rmcp::schemars;
use serde::Serialize;

use crate::analyze::{LineIndex, NodeId, Prepared, symbol_offsets};
use crate::graph::{DeclIndex, Lookup};

/// Completion candidates past this count are dropped (with `truncated` set) — an agent wants the
/// shape of what's available, not an exhaustive identifier dump.
const MAX_CANDIDATES: usize = 100;

/// A fresh store with the request's entry opened, plus what the tools need alongside it: the entry
/// URI (store queries key on it), the entry text (position math), and the entry's on-disk path
/// when the request named a file (target-path reporting).
pub struct Opened {
    store: DocumentStore,
    uri: String,
    text: String,
    path: Option<String>,
}

/// Open a `check`-style request as an engine document. `file` opens the real path — sibling
/// modules and dependency packages resolve exactly as they do in the editor; `source` is a lone
/// in-memory document under a non-`file:` URI (no directory to scan).
pub fn open(source: &Option<String>, file: &Option<String>) -> Result<Opened, ErrorData> {
    let (uri, text, path) = match (source, file) {
        (Some(text), None) => ("untitled:entry.noe".to_string(), text.clone(), None),
        (None, Some(path)) => {
            let canonical = std::fs::canonicalize(path)
                .map_err(|e| ErrorData::invalid_params(format!("cannot open {path}: {e}"), None))?;
            let text = std::fs::read_to_string(&canonical)
                .map_err(|e| ErrorData::invalid_params(format!("cannot read {path}: {e}"), None))?;
            let uri = format!("file://{}", canonical.display());
            (uri, text, Some(canonical.display().to_string()))
        }
        (Some(_), Some(_)) => {
            return Err(ErrorData::invalid_params(
                "provide either `source` or `file`, not both",
                None,
            ));
        }
        (None, None) => {
            return Err(ErrorData::invalid_params(
                "provide `source` (inline code) or `file` (a path)",
                None,
            ));
        }
    };
    let mut store = DocumentStore::default();
    store.open(&uri, text.clone());
    Ok(Opened {
        store,
        uri,
        text,
        path,
    })
}

impl Opened {
    /// Resolve a `symbol`-or-`line`/`column` site to candidate engine positions (0-based), or
    /// explain how to ask. The symbol form yields **every** whole-word occurrence in the entry
    /// file: the first may be the declaration itself (not a "use" the resolver indexes), so the
    /// caller probes until one resolves.
    fn sites(
        &self,
        symbol: Option<&str>,
        line: Option<u32>,
        column: Option<u32>,
    ) -> Result<Vec<noeta_ide::Position>, String> {
        match (symbol, line, column) {
            (Some(name), _, _) => {
                let sites: Vec<_> = symbol_offsets(&self.text, name)
                    .into_iter()
                    .take(32) // bound the probing on a pathological file
                    .map(|offset| self.position_at(offset))
                    .collect();
                if sites.is_empty() {
                    Err(format!("no identifier `{name}` in the entry file"))
                } else {
                    Ok(sites)
                }
            }
            (None, Some(l), Some(c)) => Ok(vec![noeta_ide::Position {
                line: l.saturating_sub(1),
                character: c.saturating_sub(1),
            }]),
            (None, _, _) => {
                Err("provide `symbol` (a name) or both `line` and `column`".to_string())
            }
        }
    }

    /// A byte offset in the entry text as the engine's 0-based UTF-8 position.
    fn position_at(&self, offset: u32) -> noeta_ide::Position {
        let loc = LineIndex::new(&self.text).loc(offset);
        noeta_ide::Position {
            line: loc.line - 1,
            character: loc.column - 1,
        }
    }

    /// Map an engine `(uri, range)` target to the reported location: the target's file path (the
    /// entry's own path — `None` for an inline entry — or the sibling/dependency file the range
    /// landed in) and the 1-based range.
    fn location(&self, target_uri: &str, range: noeta_ide::Range) -> NavLocation {
        let file = if target_uri == self.uri {
            self.path.clone()
        } else {
            Some(
                target_uri
                    .strip_prefix("file://")
                    .unwrap_or(target_uri)
                    .to_string(),
            )
        };
        NavLocation {
            file,
            range: NavRange {
                start: Pos {
                    line: range.start.line + 1,
                    column: range.start.character + 1,
                },
                end: Pos {
                    line: range.end.line + 1,
                    column: range.end.character + 1,
                },
            },
        }
    }

    /// Open another file of the workspace in this store, so a declaration outside the entry can be
    /// addressed at its own name. Returns the document's URI and text.
    fn open_file(&mut self, path: &str) -> Option<(String, String)> {
        let canonical = std::fs::canonicalize(path).ok()?;
        let uri = format!("file://{}", canonical.display());
        if uri == self.uri {
            return Some((uri, self.text.clone()));
        }
        let text = std::fs::read_to_string(&canonical).ok()?;
        self.store.open(&uri, text.clone());
        Some((uri, text))
    }

    /// The text of the target's line, for a definition answer an agent can read without another
    /// tool call. Entry targets read the open buffer; cross-file targets read the file on disk.
    /// Best-effort (`None` when the file cannot be read).
    fn line_text(&self, target_uri: &str, line_zero_based: u32) -> Option<String> {
        let owned;
        let text = if target_uri == self.uri {
            self.text.as_str()
        } else {
            owned = std::fs::read_to_string(target_uri.strip_prefix("file://")?).ok()?;
            owned.as_str()
        };
        text.lines()
            .nth(line_zero_based as usize)
            .map(|l| l.trim_end().to_string())
    }
}

/// A 1-based source position (column in UTF-8 bytes).
#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema)]
pub struct Pos {
    pub line: u32,
    pub column: u32,
}

/// A 1-based source range.
#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema)]
pub struct NavRange {
    pub start: Pos,
    pub end: Pos,
}

/// A resolved navigation target: the file it lives in (`None` for the inline entry itself) and its
/// range there.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct NavLocation {
    /// The target's file, relative to the project root — the same spelling `id.file` uses.
    /// `None` when the target is in the inline `source` entry.
    pub file: Option<String>,
    pub range: NavRange,
}

impl NavLocation {
    /// The same location with its file spelled relative to the project root.
    fn relative_to(mut self, p: &Prepared) -> NavLocation {
        self.file = self.file.map(|file| p.relative(&file));
        self
    }
}

/// The `definition` result: where the symbol at the site is declared.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DefinitionOutput {
    pub found: bool,
    /// The declaration's stable identity — the same `id` `symbols`, `trace`, `impact` and
    /// `callers` report for it.
    pub id: Option<NodeId>,
    /// The declaration's location — possibly in a different file (a sibling module or a dependency
    /// package).
    pub location: Option<NavLocation>,
    /// The declaration's line of source, trimmed, when it could be read.
    pub snippet: Option<String>,
    /// Every declaration a `symbol` leaf matched, when it matched more than one. Pass one of their
    /// `name`s back to pick it.
    pub candidates: Vec<NodeId>,
    /// When not found, why.
    pub note: Option<String>,
}

/// The workspace declaration inventory a `symbol` request falls back to when the name is not in
/// the entry file, built over the linked program so its names carry the post-link qualification.
struct Workspace<'a> {
    p: &'a Prepared,
    decls: DeclIndex,
}

impl<'a> Workspace<'a> {
    fn new(p: &'a Prepared) -> Workspace<'a> {
        let linked = noeta_db::linked(&p.db, p.ws);
        let entry = noeta_db::ast(&p.db, crate::analyze::entry_program(p));
        let decls = match &linked.program {
            Ok(program) => DeclIndex::build(program),
            Err(_) => DeclIndex::build(&entry.0.program),
        };
        Workspace { p, decls }
    }

    /// The identity of the declaration at `offset` in the file named `file`, when that file is one
    /// of the workspace's sources. This is how a resolved navigation target — which lands on the
    /// declared name — becomes a node id.
    fn id_at(&self, file: Option<&str>, line: u32, column: u32) -> Option<NodeId> {
        let path = file?;
        let canonical = std::fs::canonicalize(path).ok();
        let index = self.p.sources.iter().position(|s| {
            s.name() == path
                || canonical
                    .as_deref()
                    .is_some_and(|c| std::path::Path::new(s.name()) == c)
        })?;
        let offset = LineIndex::new(self.p.sources[index].text()).offset(line, column);
        self.decls
            .at_offset(noeta_span::SourceId(index as u32), offset)
            .map(|decl| decl.id(self.p))
    }

    /// The identity of the entry file's declaration at a 0-based engine position.
    fn entry_id_at(&self, position: noeta_ide::Position) -> Option<NodeId> {
        let offset =
            LineIndex::new(self.p.entry_text()).offset(position.line + 1, position.character + 1);
        self.decls
            .at_offset(noeta_span::SourceId::FIRST, offset)
            .map(|decl| decl.id(self.p))
    }
}

/// Answer `definition`: the site (symbol or 1-based position) resolves through the engine's
/// scope-aware value index, member table, and top-level name tables — cross-file over the merged
/// workspace program. A `symbol` the entry file does not contain resolves against the whole
/// workspace by qualified name or unique leaf, so a project's declarations are addressable without
/// first knowing which file holds them.
pub fn definition(
    opened: &mut Opened,
    p: &Prepared,
    symbol: Option<&str>,
    line: Option<u32>,
    column: Option<u32>,
) -> DefinitionOutput {
    let ws = Workspace::new(p);
    if let Some(sites) = opened.sites(symbol, line, column).ok()
        && let Some((target_uri, range)) = sites.into_iter().find_map(|position| {
            opened
                .store
                .definition(&opened.uri, position, Encoding::Utf8)
        })
    {
        let mut location = opened.location(&target_uri, range);
        let absolute = location.file.clone();
        location.file = location.file.map(|file| p.relative(&file));
        return DefinitionOutput {
            found: true,
            id: ws.id_at(
                absolute.as_deref(),
                location.range.start.line,
                location.range.start.column,
            ),
            snippet: opened.line_text(&target_uri, range.start.line),
            location: Some(location),
            candidates: Vec::new(),
            note: None,
        };
    }
    let Some(name) = symbol else {
        return DefinitionOutput {
            found: false,
            id: None,
            location: None,
            snippet: None,
            candidates: Vec::new(),
            note: Some(match (line, column) {
                (Some(_), Some(_)) => "no resolvable symbol at that site".to_string(),
                _ => "provide `symbol` (a name) or both `line` and `column`".to_string(),
            }),
        };
    };
    match ws.decls.lookup(name) {
        Lookup::Found(decl) => {
            let id = decl.id(p);
            let (file, at) = match crate::analyze::locate_span(p, decl.name_span) {
                Some(found) => found,
                None => {
                    return not_found_definition(format!(
                        "`{name}` resolves to `{}`, whose file is outside this workspace",
                        decl.name
                    ));
                }
            };
            let index = decl.name_span.source.0 as usize;
            let snippet = p.sources.get(index).and_then(|src| {
                src.text()
                    .lines()
                    .nth(at.start.line.saturating_sub(1) as usize)
                    .map(|l| l.trim_end().to_string())
            });
            DefinitionOutput {
                found: true,
                id: Some(id),
                location: Some(NavLocation {
                    file: Some(p.relative(&file)),
                    range: NavRange {
                        start: Pos {
                            line: at.start.line,
                            column: at.start.column,
                        },
                        end: Pos {
                            line: at.end.line,
                            column: at.end.column,
                        },
                    },
                }),
                snippet,
                candidates: Vec::new(),
                note: None,
            }
        }
        Lookup::Ambiguous(all) => DefinitionOutput {
            found: false,
            id: None,
            location: None,
            snippet: None,
            candidates: all.iter().map(|d| d.id(p)).collect(),
            note: Some(format!(
                "`{name}` names {} declarations in this workspace — pass one of the candidate \
                 names",
                all.len()
            )),
        },
        Lookup::Missing => not_found_definition(format!(
            "no declaration named `{name}` in this workspace, and no identifier `{name}` in the \
             entry file"
        )),
    }
}

fn not_found_definition(note: String) -> DefinitionOutput {
    DefinitionOutput {
        found: false,
        id: None,
        location: None,
        snippet: None,
        candidates: Vec::new(),
        note: Some(note),
    }
}

/// The `references` result: every use of the symbol at the site (declaration included unless opted
/// out).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ReferencesOutput {
    pub found: bool,
    /// The declaration the references belong to, when the site resolved to one.
    pub id: Option<NodeId>,
    pub count: usize,
    pub references: Vec<NavLocation>,
    /// Every declaration a `symbol` leaf matched, when it matched more than one.
    pub candidates: Vec<NodeId>,
    pub note: Option<String>,
}

/// Answer `references`: value symbols (locals, parameters, functions) via the scope-aware def/use
/// index, member symbols (fields, variants, methods) matched by the receiver's type — across
/// modules. A `symbol` the entry file does not contain resolves against the whole workspace and is
/// queried at its own declaration.
pub fn references(
    opened: &mut Opened,
    p: &Prepared,
    symbol: Option<&str>,
    line: Option<u32>,
    column: Option<u32>,
    include_declaration: bool,
) -> ReferencesOutput {
    let ws = Workspace::new(p);
    if let Some(sites) = opened.sites(symbol, line, column).ok()
        && let Some((position, locations)) = sites.into_iter().find_map(|position| {
            opened
                .store
                .references(&opened.uri, position, Encoding::Utf8, include_declaration)
                .map(|found| (position, found))
        })
    {
        let references: Vec<NavLocation> = locations
            .into_iter()
            .map(|(target_uri, range)| opened.location(&target_uri, range).relative_to(p))
            .collect();
        return ReferencesOutput {
            found: true,
            id: ws.entry_id_at(position),
            count: references.len(),
            references,
            candidates: Vec::new(),
            note: None,
        };
    }
    let Some(name) = symbol else {
        return not_found_references(match (line, column) {
            (Some(_), Some(_)) => "no resolvable symbol at that site".to_string(),
            _ => "provide `symbol` (a name) or both `line` and `column`".to_string(),
        });
    };
    let (decl, id) = match ws.decls.lookup(name) {
        Lookup::Found(decl) => (decl, decl.id(p)),
        Lookup::Ambiguous(all) => {
            return ReferencesOutput {
                found: false,
                id: None,
                count: 0,
                references: Vec::new(),
                candidates: all.iter().map(|d| d.id(p)).collect(),
                note: Some(format!(
                    "`{name}` names {} declarations in this workspace — pass one of the candidate \
                     names",
                    all.len()
                )),
            };
        }
        Lookup::Missing => {
            return not_found_references(format!(
                "no declaration named `{name}` in this workspace, and no identifier `{name}` in \
                 the entry file"
            ));
        }
    };
    // Query the engine at the declaration's own file and name position: the store resolves
    // cross-file from wherever it is asked, so this reaches every module of the workspace.
    let Some((file, at)) = crate::analyze::locate_span(p, decl.name_span) else {
        return not_found_references(format!(
            "`{}` is declared outside this workspace's files",
            decl.name
        ));
    };
    // `file` is the source's own (absolute) name, which is what opens it; every path this
    // function *reports* goes out relative, the way `id.file` does.
    let Some((uri, _)) = opened.open_file(&file) else {
        return not_found_references(format!("cannot read {}", p.relative(&file)));
    };
    let position = noeta_ide::Position {
        line: at.start.line.saturating_sub(1),
        character: at.start.column.saturating_sub(1),
    };
    match opened
        .store
        .references(&uri, position, Encoding::Utf8, include_declaration)
    {
        Some(locations) => {
            let references: Vec<NavLocation> = locations
                .into_iter()
                .map(|(target_uri, range)| opened.location(&target_uri, range).relative_to(p))
                .collect();
            ReferencesOutput {
                found: true,
                id: Some(id),
                count: references.len(),
                references,
                candidates: Vec::new(),
                note: None,
            }
        }
        None => not_found_references(format!(
            "`{}` is declared at {}, and the engine found no occurrences of it",
            decl.name,
            p.relative(&file)
        )),
    }
}

fn not_found_references(note: String) -> ReferencesOutput {
    ReferencesOutput {
        found: false,
        id: None,
        count: 0,
        references: Vec::new(),
        candidates: Vec::new(),
        note: Some(note),
    }
}

/// One completion candidate.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CompletionCandidate {
    pub label: String,
    /// `keyword` / `function` / `struct` / `class` / `enum` / `variable` / `field` / `method` /
    /// `variant` / `type`.
    pub kind: String,
    /// A short signature-ish detail (a method's parameters, a field's type), when available.
    pub detail: Option<String>,
}

/// The `completions` result.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CompletionsOutput {
    pub count: usize,
    /// True when more candidates existed than were returned.
    pub truncated: bool,
    pub candidates: Vec<CompletionCandidate>,
    pub note: Option<String>,
}

/// Answer `completions` at a 1-based position: member completion after a `.` (the receiver's
/// fields/variants/methods, bundle methods included), type names in an annotation position, or the
/// identifiers in scope.
pub fn completions(opened: &Opened, line: u32, column: u32) -> CompletionsOutput {
    let position = noeta_ide::Position {
        line: line.saturating_sub(1),
        character: column.saturating_sub(1),
    };
    match opened
        .store
        .completions(&opened.uri, position, Encoding::Utf8)
    {
        Some(candidates) => {
            let total = candidates.len();
            let truncated = total > MAX_CANDIDATES;
            let candidates: Vec<CompletionCandidate> = candidates
                .into_iter()
                .take(MAX_CANDIDATES)
                .map(|candidate| CompletionCandidate {
                    label: candidate.label,
                    kind: candidate_kind(candidate.kind).to_string(),
                    detail: candidate.detail,
                })
                .collect();
            CompletionsOutput {
                count: candidates.len(),
                truncated,
                candidates,
                note: truncated
                    .then(|| format!("{total} candidates; first {MAX_CANDIDATES} shown")),
            }
        }
        None => CompletionsOutput {
            count: 0,
            truncated: false,
            candidates: Vec::new(),
            note: Some("document is not open in the engine (internal)".to_string()),
        },
    }
}

fn candidate_kind(kind: completion::CandidateKind) -> &'static str {
    use completion::CandidateKind;
    match kind {
        CandidateKind::Keyword => "keyword",
        CandidateKind::Function => "function",
        CandidateKind::Struct => "struct",
        CandidateKind::Class => "class",
        CandidateKind::Enum => "enum",
        CandidateKind::Variable => "variable",
        CandidateKind::Field => "field",
        CandidateKind::Method => "method",
        CandidateKind::EnumMember => "variant",
        CandidateKind::Type => "type",
        CandidateKind::Trait => "trait",
        CandidateKind::Module => "module",
        CandidateKind::Directive => "directive",
    }
}

/// The `signature` result: the signature of the call the position is inside.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SignatureOutput {
    pub found: bool,
    /// The full signature, e.g. `add(a: int, b: int) -> int`.
    pub label: String,
    pub parameters: Vec<String>,
    /// Which parameter the position sits at (0-based), when found.
    pub active_parameter: Option<u32>,
    pub note: Option<String>,
}

/// Answer `signature` at a 1-based position inside a call's parentheses. Token-based, so a
/// half-typed call with an unbalanced paren still resolves; method calls resolve the receiver's
/// type.
pub fn signature(opened: &Opened, line: u32, column: u32) -> SignatureOutput {
    let position = noeta_ide::Position {
        line: line.saturating_sub(1),
        character: column.saturating_sub(1),
    };
    match opened
        .store
        .signature_help(&opened.uri, position, Encoding::Utf8)
    {
        Some(data) => SignatureOutput {
            found: true,
            label: data.label,
            active_parameter: Some(data.active_param as u32),
            parameters: data.parameters,
            note: None,
        },
        None => SignatureOutput {
            found: false,
            label: String::new(),
            parameters: Vec::new(),
            active_parameter: None,
            note: Some("the position is not inside a resolvable call".to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opened(source: &str) -> (Opened, Prepared) {
        noeta_stdlib::registry::default_seeded();
        (
            open(&Some(source.to_string()), &None).unwrap(),
            crate::analyze::prepare(&Some(source.to_string()), &None).unwrap(),
        )
    }

    /// The same pair for an on-disk entry, so the store's sibling discovery and the salsa
    /// workspace both see the whole project.
    fn opened_file(path: &str) -> (Opened, Prepared) {
        noeta_stdlib::registry::default_seeded();
        (
            open(&None, &Some(path.to_string())).unwrap(),
            crate::analyze::prepare(&None, &Some(path.to_string())).unwrap(),
        )
    }

    #[test]
    fn definition_resolves_a_call_to_its_fn() {
        let (mut o, p) = opened("fn greet(): int { return 1 }\ntotal = greet()");
        // Cursor on `greet` inside the call (line 2, column 11 — 1-based).
        let out = definition(&mut o, &p, None, Some(2), Some(11));
        assert!(out.found, "note: {:?}", out.note);
        let loc = out.location.unwrap();
        assert_eq!(loc.range.start.line, 1);
        assert_eq!(loc.range.start.column, 4); // `fn greet` — name starts after "fn "
        assert!(loc.file.is_none(), "inline entry has no path");
        assert_eq!(out.snippet.as_deref(), Some("fn greet(): int { return 1 }"));
    }

    #[test]
    fn definition_resolves_by_symbol_name() {
        let (mut o, p) = opened("total = 1\necho total");
        let out = definition(&mut o, &p, Some("total"), None, None);
        assert!(out.found, "note: {:?}", out.note);
        assert_eq!(out.location.unwrap().range.start.line, 1);
    }

    #[test]
    fn definition_reports_a_missing_symbol() {
        let (mut o, p) = opened("x = 1");
        let out = definition(&mut o, &p, Some("ghost"), None, None);
        assert!(!out.found);
        assert!(out.note.unwrap().contains("ghost"));
    }

    #[test]
    fn references_lists_every_use_and_the_declaration() {
        let (mut o, p) = opened("total = 1\necho total\necho total");
        let out = references(&mut o, &p, Some("total"), None, None, true);
        assert!(out.found);
        assert_eq!(out.count, 3);
        // Sorted by position: declaration first.
        assert_eq!(out.references[0].range.start.line, 1);
        assert_eq!(out.references[2].range.start.line, 3);
    }

    #[test]
    fn references_without_declaration_lists_only_uses() {
        let (mut o, p) = opened("total = 1\necho total\necho total");
        let out = references(&mut o, &p, Some("total"), None, None, false);
        assert_eq!(out.count, 2);
    }

    #[test]
    fn completions_after_a_dot_offer_the_receiver_members() {
        let source = "\
struct Counter { n: int\n  fn get(): int { return self.n }\n}\nc = Counter { n: 1 }\nv = c.n";
        let (o, _p) = opened(source);
        // Cursor right after `c.n` on the last line (line 5, column 8 → on the member).
        let out = completions(&o, 5, 8);
        assert!(
            out.candidates
                .iter()
                .any(|c| c.label == "get" && c.kind == "method"),
            "got {:?}",
            out.candidates
        );
        assert!(
            !out.candidates.iter().any(|c| c.kind == "keyword"),
            "member completion offers members only"
        );
    }

    #[test]
    fn completions_in_scope_offer_functions_and_locals() {
        let (o, _p) = opened("fn helper(): int { return 1 }\ntotal = 1\necho to");
        let out = completions(&o, 3, 8);
        let has = |label: &str, kind: &str| {
            out.candidates
                .iter()
                .any(|c| c.label == label && c.kind == kind)
        };
        assert!(has("helper", "function"), "got {:?}", out.candidates);
        assert!(has("total", "variable"));
    }

    #[test]
    fn signature_reports_the_call_and_active_parameter() {
        let (o, _p) = opened("fn add(a: int, b: int): int { return a + b }\nx = add(1, ");
        // Cursor after the comma — the second parameter is active.
        let out = signature(&o, 2, 12);
        assert!(out.found, "note: {:?}", out.note);
        assert!(out.label.contains("add"), "label: {}", out.label);
        assert_eq!(out.parameters.len(), 2);
        assert_eq!(out.active_parameter, Some(1));
    }

    /// A three-module package: `main` calls into `alpha` and `beta`, each of which declares its
    /// own `shared`. Returns the entry path (the guard rides along).
    fn three_module_project(name: &str) -> noeta_test_temp::TempPath {
        let root = noeta_test_temp::TempDir::new(&format!("mcp-{name}"));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("noeta.toml"),
            "[package]\nname = \"local/joined\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("alpha.noe"),
            "pub fn shared(): int { return 1 }\npub fn alpha_only(): int { return shared() }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("beta.noe"),
            "pub fn shared(): int { return 2 }\npub fn beta_only(): int { return shared() }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("main.noe"),
            "use joined.alpha\nuse joined.beta\n\
             fn entry(): int { return alpha.alpha_only() + beta.beta_only() }\necho entry()\n",
        )
        .unwrap();
        root.into_child("src/main.noe")
    }

    /// **D13.** A `symbol` the entry file does not contain resolves across the workspace, by a
    /// unique leaf as well as by the post-link name. Addressing was the entry file's *text*, so a
    /// declaration in a sibling module could not be asked about by name at all.
    #[test]
    fn definition_resolves_a_symbol_declared_in_another_module() {
        let entry = three_module_project("nav_ws_definition");
        let (mut o, p) = opened_file(&entry.display().to_string());

        // A unique leaf: `alpha_only` appears nowhere in the entry's text as a declaration.
        let out = definition(&mut o, &p, Some("alpha_only"), None, None);
        assert!(out.found, "note: {:?}", out.note);
        let id = out.id.expect("the declaration carries an id");
        assert_eq!(id.name, "joined.alpha.alpha_only");
        assert_eq!(id.file.as_deref(), Some("src/alpha.noe"));
        assert_eq!(id.kind.as_str(), "function");
        assert!(
            out.snippet
                .as_deref()
                .is_some_and(|s| s.contains("alpha_only")),
            "snippet: {:?}",
            out.snippet
        );

        // The post-link name resolves to exactly the same node.
        let by_name = definition(&mut o, &p, Some("joined.beta.beta_only"), None, None);
        assert!(by_name.found, "note: {:?}", by_name.note);
        assert_eq!(
            by_name.id.expect("id").file.as_deref(),
            Some("src/beta.noe")
        );
    }

    /// A leaf that names two declarations comes back as a candidate list rather than a guess.
    #[test]
    fn an_ambiguous_leaf_reports_every_candidate() {
        let entry = three_module_project("nav_ws_ambiguous");
        let (mut o, p) = opened_file(&entry.display().to_string());
        let out = definition(&mut o, &p, Some("shared"), None, None);
        assert!(
            !out.found,
            "two `shared` declarations must not resolve to one"
        );
        let names: std::collections::BTreeSet<&str> =
            out.candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            ["joined.alpha.shared", "joined.beta.shared"]
                .into_iter()
                .collect()
        );
        assert!(
            out.note
                .as_deref()
                .is_some_and(|n| n.contains("2 declarations")),
            "note: {:?}",
            out.note
        );
        // Naming one of the candidates picks it.
        let picked = definition(&mut o, &p, Some("joined.alpha.shared"), None, None);
        assert!(picked.found);
        assert_eq!(
            picked.id.expect("id").file.as_deref(),
            Some("src/alpha.noe")
        );
    }

    /// `references` addressed by a workspace name is queried at the declaration's own file, so it
    /// reaches the module's uses and does not sweep in the same-named function next door.
    #[test]
    fn references_resolve_a_symbol_declared_in_another_module() {
        let entry = three_module_project("nav_ws_references");
        let (mut o, p) = opened_file(&entry.display().to_string());
        let out = references(&mut o, &p, Some("joined.alpha.shared"), None, None, true);
        assert!(out.found, "note: {:?}", out.note);
        assert_eq!(out.id.as_ref().expect("id").name, "joined.alpha.shared");
        assert!(out.count >= 2, "declaration plus its use: {out:?}");
        for reference in &out.references {
            assert!(
                reference
                    .file
                    .as_deref()
                    .is_some_and(|f| f.ends_with("alpha.noe")),
                "beta's same-named function must not be swept in: {reference:?}"
            );
        }
    }

    #[test]
    fn definition_jumps_into_a_sibling_module_file() {
        // A real directory: `file` entries get the engine's sibling discovery, so the imported
        // struct's definition resolves into the other file.
        let dir = noeta_test_temp::TempDir::new("mcp-nav-sibling");
        std::fs::write(
            dir.join("models.noe"),
            "namespace App.Models;\npub struct User { id: int }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("main.noe"),
            "use App.Models.User;\nu = User { id: 1 }\necho u.id\n",
        )
        .unwrap();

        let (mut o, p) = opened_file(&dir.join("main.noe").display().to_string());
        // Cursor on `User` in the constructor on line 2.
        let out = definition(&mut o, &p, None, Some(2), Some(6));
        assert!(out.found, "note: {:?}", out.note);
        let loc = out.location.unwrap();
        let file = loc.file.expect("target has a path");
        assert!(file.ends_with("models.noe"), "landed in {file}");
        assert_eq!(loc.range.start.line, 2);
        assert!(out.snippet.unwrap().contains("struct User"));
    }
}
