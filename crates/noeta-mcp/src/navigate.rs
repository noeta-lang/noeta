//! M5 Understand pillar, navigation half: `definition` / `references` / `completions` /
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

use crate::analyze::{LineIndex, symbol_offsets};

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
    /// The target's file path; `None` when the target is in the inline `source` entry.
    pub file: Option<String>,
    pub range: NavRange,
}

/// The `definition` result: where the symbol at the site is declared.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DefinitionOutput {
    pub found: bool,
    /// The declaration's location — possibly in a different file (a sibling module or a dependency
    /// package).
    pub location: Option<NavLocation>,
    /// The declaration's line of source, trimmed, when it could be read.
    pub snippet: Option<String>,
    /// When not found, why.
    pub note: Option<String>,
}

/// Answer `definition`: the site (symbol or 1-based position) resolves through the engine's
/// scope-aware value index, member table, and top-level name tables — cross-file over the merged
/// workspace program.
pub fn definition(
    opened: &Opened,
    symbol: Option<&str>,
    line: Option<u32>,
    column: Option<u32>,
) -> DefinitionOutput {
    let sites = match opened.sites(symbol, line, column) {
        Ok(sites) => sites,
        Err(note) => {
            return DefinitionOutput {
                found: false,
                location: None,
                snippet: None,
                note: Some(note),
            };
        }
    };
    match sites.into_iter().find_map(|position| {
        opened
            .store
            .definition(&opened.uri, position, Encoding::Utf8)
    }) {
        Some((target_uri, range)) => DefinitionOutput {
            found: true,
            snippet: opened.line_text(&target_uri, range.start.line),
            location: Some(opened.location(&target_uri, range)),
            note: None,
        },
        None => DefinitionOutput {
            found: false,
            location: None,
            snippet: None,
            note: Some("no resolvable symbol at that site".to_string()),
        },
    }
}

/// The `references` result: every use of the symbol at the site (declaration included unless opted
/// out).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ReferencesOutput {
    pub found: bool,
    pub count: usize,
    pub references: Vec<NavLocation>,
    pub note: Option<String>,
}

/// Answer `references`: value symbols (locals, parameters, functions) via the scope-aware def/use
/// index, member symbols (fields, variants, methods) matched by the receiver's type — across
/// modules.
pub fn references(
    opened: &Opened,
    symbol: Option<&str>,
    line: Option<u32>,
    column: Option<u32>,
    include_declaration: bool,
) -> ReferencesOutput {
    let sites = match opened.sites(symbol, line, column) {
        Ok(sites) => sites,
        Err(note) => {
            return ReferencesOutput {
                found: false,
                count: 0,
                references: Vec::new(),
                note: Some(note),
            };
        }
    };
    match sites.into_iter().find_map(|position| {
        opened
            .store
            .references(&opened.uri, position, Encoding::Utf8, include_declaration)
    }) {
        Some(locations) => {
            let references: Vec<NavLocation> = locations
                .into_iter()
                .map(|(target_uri, range)| opened.location(&target_uri, range))
                .collect();
            ReferencesOutput {
                found: true,
                count: references.len(),
                references,
                note: None,
            }
        }
        None => ReferencesOutput {
            found: false,
            count: 0,
            references: Vec::new(),
            note: Some("no resolvable symbol at that site".to_string()),
        },
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

    fn opened(source: &str) -> Opened {
        open(&Some(source.to_string()), &None).unwrap()
    }

    #[test]
    fn definition_resolves_a_call_to_its_fn() {
        let o = opened("fn greet(): int { return 1 }\ntotal = greet()");
        // Cursor on `greet` inside the call (line 2, column 11 — 1-based).
        let out = definition(&o, None, Some(2), Some(11));
        assert!(out.found, "note: {:?}", out.note);
        let loc = out.location.unwrap();
        assert_eq!(loc.range.start.line, 1);
        assert_eq!(loc.range.start.column, 4); // `fn greet` — name starts after "fn "
        assert!(loc.file.is_none(), "inline entry has no path");
        assert_eq!(out.snippet.as_deref(), Some("fn greet(): int { return 1 }"));
    }

    #[test]
    fn definition_resolves_by_symbol_name() {
        let o = opened("total = 1\necho total");
        let out = definition(&o, Some("total"), None, None);
        assert!(out.found, "note: {:?}", out.note);
        assert_eq!(out.location.unwrap().range.start.line, 1);
    }

    #[test]
    fn definition_reports_a_missing_symbol() {
        let o = opened("x = 1");
        let out = definition(&o, Some("ghost"), None, None);
        assert!(!out.found);
        assert!(out.note.unwrap().contains("ghost"));
    }

    #[test]
    fn references_lists_every_use_and_the_declaration() {
        let o = opened("total = 1\necho total\necho total");
        let out = references(&o, Some("total"), None, None, true);
        assert!(out.found);
        assert_eq!(out.count, 3);
        // Sorted by position: declaration first.
        assert_eq!(out.references[0].range.start.line, 1);
        assert_eq!(out.references[2].range.start.line, 3);
    }

    #[test]
    fn references_without_declaration_lists_only_uses() {
        let o = opened("total = 1\necho total\necho total");
        let out = references(&o, Some("total"), None, None, false);
        assert_eq!(out.count, 2);
    }

    #[test]
    fn completions_after_a_dot_offer_the_receiver_members() {
        let source = "\
struct Counter { n: int\n  fn get(): int { return self.n }\n}\nc = Counter { n: 1 }\nv = c.n";
        let o = opened(source);
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
        let o = opened("fn helper(): int { return 1 }\ntotal = 1\necho to");
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
        let o = opened("fn add(a: int, b: int): int { return a + b }\nx = add(1, ");
        // Cursor after the comma — the second parameter is active.
        let out = signature(&o, 2, 12);
        assert!(out.found, "note: {:?}", out.note);
        assert!(out.label.contains("add"), "label: {}", out.label);
        assert_eq!(out.parameters.len(), 2);
        assert_eq!(out.active_parameter, Some(1));
    }

    #[test]
    fn definition_jumps_into_a_sibling_module_file() {
        // A real directory: `file` entries get the engine's sibling discovery, so the imported
        // struct's definition resolves into the other file.
        let dir = std::env::temp_dir().join("noeta_mcp_nav_sibling");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
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

        let o = open(&None, &Some(dir.join("main.noe").display().to_string())).unwrap();
        // Cursor on `User` in the constructor on line 2.
        let out = definition(&o, None, Some(2), Some(6));
        assert!(out.found, "note: {:?}", out.note);
        let loc = out.location.unwrap();
        let file = loc.file.expect("target has a path");
        assert!(file.ends_with("models.noe"), "landed in {file}");
        assert_eq!(loc.range.start.line, 2);
        assert!(out.snippet.unwrap().contains("struct User"));
    }
}
