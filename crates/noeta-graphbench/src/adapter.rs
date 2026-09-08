//! The one module that reads MCP tool output.
//!
//! Every arm speaks [`NodeRef`] and [`Located`]; nothing outside this file touches a tool's JSON.
//! That boundary is the whole point: the graph tools' wire shapes are changing, and re-targeting
//! the benchmark to a new shape has to be an edit here rather than a sweep of the strategies.
//!
//! Each reader **prefers a node's `id` object when the tool emits one** — a qualified name, a kind,
//! a file, and a byte span, which is the identity the engine has always had and no wire shape used
//! to expose. Absent an `id`, it falls back to today's per-tool fields: bare names and 1-based
//! line/column from `symbols`, `definition` and `references`; qualified names with a line and no
//! column from `trace`; a target name and a `SpanLoc` from `reflect`. Byte spans are what scoring
//! matches on, so a line/column answer is resolved against the file's own text here.

use std::collections::HashMap;

use serde_json::Value;

/// A declaration an arm named, in whatever identity the tool it came from could give.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct NodeRef {
    /// The dotted name, when the tool spoke one.
    pub qualified: Option<String>,
    /// The last segment, which every tool speaks.
    pub leaf: String,
    pub file: Option<String>,
    /// The declaration name's byte span, when the tool carried one (or one was resolvable).
    pub span: Option<(u32, u32)>,
    pub kind: Option<String>,
}

impl NodeRef {
    pub fn named(name: &str) -> NodeRef {
        NodeRef {
            qualified: if name.contains('.') {
                Some(name.to_string())
            } else {
                None
            },
            leaf: name.rsplit('.').next().unwrap_or(name).to_string(),
            ..NodeRef::default()
        }
    }
}

/// A position an arm read out of a tool, resolved to a byte offset in its file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub file: Option<String>,
    pub offset: u32,
    pub end: u32,
}

/// Byte offsets for 1-based line/column positions, per file.
#[derive(Debug)]
pub struct LineIndexes {
    lines: HashMap<String, Vec<u32>>,
}

impl LineIndexes {
    pub fn new() -> LineIndexes {
        LineIndexes {
            lines: HashMap::new(),
        }
    }

    /// Register a file's text so positions in it resolve.
    pub fn add(&mut self, file: &str, text: &str) {
        let mut starts = vec![0u32];
        for (offset, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                starts.push(offset as u32 + 1);
            }
        }
        self.lines.insert(file.to_string(), starts);
    }

    /// A 1-based line and byte column in `file`, as a byte offset.
    pub fn offset(&self, file: &str, line: u32, column: u32) -> Option<u32> {
        let starts = self.lines.get(file)?;
        let start = *starts.get(line.saturating_sub(1) as usize)?;
        Some(start + column.saturating_sub(1))
    }
}

impl Default for LineIndexes {
    fn default() -> LineIndexes {
        LineIndexes::new()
    }
}

/// Read a node's `id` object, when the tool emits one.
///
/// Accepts the spellings a stable id can plausibly land under, because a reader that insists on one
/// of them silently degrades to the fallback the day the shape settles. Everything else in this
/// crate reads what this returns.
fn read_id(value: &Value) -> Option<NodeRef> {
    let id = value.get("id")?;
    if !id.is_object() {
        return None;
    }
    let name = id
        .get("qualified")
        .or_else(|| id.get("qualified_name"))
        .or_else(|| id.get("name"))
        .and_then(Value::as_str)?;
    let file = id.get("file").and_then(Value::as_str).map(str::to_string);
    let span = read_span(id.get("span")).or_else(|| read_span(Some(id)));
    Some(NodeRef {
        qualified: Some(name.to_string()),
        leaf: name.rsplit('.').next().unwrap_or(name).to_string(),
        file,
        span,
        kind: id.get("kind").and_then(Value::as_str).map(str::to_string),
    })
}

/// A byte span written as `{start, end}`, `{byte_start, byte_end}`, or `{start: {offset}, end: {offset}}`.
fn read_span(value: Option<&Value>) -> Option<(u32, u32)> {
    let span = value?;
    let field = |names: [&str; 2]| -> Option<u32> {
        for name in names {
            if let Some(number) = span.get(name).and_then(Value::as_u64) {
                return Some(number as u32);
            }
            if let Some(number) = span
                .get(name)
                .and_then(|v| v.get("offset"))
                .and_then(Value::as_u64)
            {
                return Some(number as u32);
            }
        }
        None
    };
    Some((field(["start", "byte_start"])?, field(["end", "byte_end"])?))
}

/// One entry of a `symbols` outline, flattened.
#[derive(Debug, Clone)]
pub struct Symbol {
    pub node: NodeRef,
    /// The whole declaration's byte range, which is what assigns a reference to its enclosing
    /// declaration.
    pub range: (u32, u32),
    pub detail: Option<String>,
    pub roles: Vec<String>,
    /// The type a method hangs off, when the outline nests it under one.
    pub owner: Option<String>,
}

/// Flatten a `symbols` response for one file.
pub fn symbols(value: &Value, file: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    if let Some(list) = value.get("symbols").and_then(Value::as_array) {
        for node in list {
            flatten_symbol(node, file, None, &mut out);
        }
    }
    out
}

fn flatten_symbol(value: &Value, file: &str, owner: Option<&str>, out: &mut Vec<Symbol>) {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .map(str::to_string);
    let start = value
        .get("location")
        .and_then(|l| l.get("start"))
        .and_then(|l| l.get("offset"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let end = value
        .get("location")
        .and_then(|l| l.get("end"))
        .and_then(|l| l.get("offset"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let node = read_id(value).unwrap_or(NodeRef {
        qualified: owner.map(|o| format!("{o}.{name}")),
        leaf: name.to_string(),
        file: Some(file.to_string()),
        // The outline's `location` is the whole declaration, so the name span is unknown; scoring
        // falls back to containment against the declaration range.
        span: None,
        kind: kind.clone(),
    });
    out.push(Symbol {
        node,
        range: (start, end),
        detail: value
            .get("detail")
            .and_then(Value::as_str)
            .map(str::to_string),
        roles: string_list(value.get("roles")),
        owner: owner.map(str::to_string),
    });
    if let Some(children) = value.get("children").and_then(Value::as_array) {
        let next_owner = if matches!(kind.as_deref(), Some("struct" | "class" | "enum" | "impl")) {
            Some(name)
        } else {
            owner
        };
        for child in children {
            flatten_symbol(child, file, next_owner, out);
        }
    }
}

/// The location `definition` resolved to.
pub fn definition(value: &Value, entry: &str, indexes: &LineIndexes) -> Option<Located> {
    if value.get("found").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    if let Some(node) = read_id(value)
        && let (Some(file), Some(span)) = (node.file.clone(), node.span)
    {
        return Some(Located {
            file: Some(file),
            offset: span.0,
            end: span.1,
        });
    }
    location(value.get("location")?, entry, indexes)
}

/// Every location `references` reported.
pub fn references(value: &Value, entry: &str, indexes: &LineIndexes) -> Vec<Located> {
    let Some(list) = value.get("references").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|hit| location(hit, entry, indexes))
        .collect()
}

fn location(value: &Value, entry: &str, indexes: &LineIndexes) -> Option<Located> {
    let file = value
        .get("file")
        .and_then(Value::as_str)
        .unwrap_or(entry)
        .to_string();
    let range = value.get("range")?;
    let read = |end: &str| -> Option<u32> {
        let at = range.get(end)?;
        let line = at.get("line").and_then(Value::as_u64)? as u32;
        let column = at.get("column").and_then(Value::as_u64)? as u32;
        indexes.offset(file.as_str(), line, column)
    };
    let start = read("start")?;
    let end = read("end").unwrap_or(start);
    Some(Located {
        file: Some(file),
        offset: start,
        end,
    })
}

/// One node of a `trace` tree, flattened with its depth and how it was reached.
#[derive(Debug, Clone)]
pub struct TraceHit {
    pub node: NodeRef,
    pub depth: usize,
    pub external: bool,
    pub dynamic: bool,
    pub roles: Vec<String>,
    /// The chain of names from the root to this node, inclusive.
    pub path: Vec<String>,
}

/// Whether the trace found its root at all, and every node it unfolded.
pub fn trace(value: &Value) -> (bool, Vec<TraceHit>) {
    let found = value.get("found").and_then(Value::as_bool) == Some(true);
    let mut out = Vec::new();
    if let Some(roots) = value.get("traces").and_then(Value::as_array) {
        for root in roots {
            flatten_trace(root, 0, &mut Vec::new(), &mut out);
        }
    }
    (found, out)
}

fn flatten_trace(value: &Value, depth: usize, path: &mut Vec<String>, out: &mut Vec<TraceHit>) {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    path.push(name.to_string());
    let node = read_id(value).unwrap_or(NodeRef {
        qualified: if name.contains('.') {
            Some(name.to_string())
        } else {
            None
        },
        leaf: name.rsplit('.').next().unwrap_or(name).to_string(),
        file: value
            .get("file")
            .and_then(Value::as_str)
            .map(str::to_string),
        span: None,
        kind: value
            .get("kind")
            .and_then(Value::as_str)
            .map(str::to_string),
    });
    out.push(TraceHit {
        node,
        depth,
        external: value.get("external").and_then(Value::as_bool) == Some(true),
        dynamic: value.get("dynamic").and_then(Value::as_bool) == Some(true),
        roles: string_list(value.get("roles")),
        path: path.clone(),
    });
    if let Some(children) = value.get("children").and_then(Value::as_array) {
        for child in children {
            flatten_trace(child, depth + 1, path, out);
        }
    }
    path.pop();
}

/// Every `(function, role)` a trace's `boundaries` summary reported.
pub fn boundaries(value: &Value) -> Vec<(NodeRef, String)> {
    let Some(list) = value.get("boundaries").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|hit| {
            let target = hit.get("target").and_then(Value::as_str)?;
            let role = hit.get("role").and_then(Value::as_str).unwrap_or_default();
            let mut node = read_id(hit).unwrap_or(NodeRef::named(target));
            if node.file.is_none() {
                node.file = hit.get("file").and_then(Value::as_str).map(str::to_string);
            }
            Some((node, role.to_string()))
        })
        .collect()
}

/// One file of a `module_graph` answer.
#[derive(Debug, Clone)]
pub struct ModuleNode {
    pub file: String,
    /// The module's own identity, when the tool carries one.
    pub namespace: String,
    pub imports: Vec<String>,
    pub roles: Vec<(String, String)>,
}

pub fn module_graph(value: &Value) -> Vec<ModuleNode> {
    let Some(list) = value.get("modules").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .map(|node| {
            let identity = read_id(node);
            ModuleNode {
                file: identity
                    .as_ref()
                    .and_then(|id| id.file.clone())
                    .or_else(|| node.get("file").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_default(),
                namespace: identity
                    .and_then(|id| id.qualified)
                    .or_else(|| {
                        node.get("namespace")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                    })
                    .unwrap_or_default(),
                imports: node
                    .get("imports")
                    .and_then(Value::as_array)
                    .map(|edges| {
                        edges
                            .iter()
                            .filter_map(|e| e.get("module").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                roles: node
                    .get("roles")
                    .and_then(Value::as_array)
                    .map(|bindings| {
                        bindings
                            .iter()
                            .filter_map(|b| {
                                Some((
                                    b.get("target").and_then(Value::as_str)?.to_string(),
                                    b.get("role").and_then(Value::as_str)?.to_string(),
                                ))
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        })
        .collect()
}

/// Every `(declaration, role)` the reflection index reported, with a location when it carried one.
pub fn reflect_roles(value: &Value) -> Vec<(NodeRef, String)> {
    let Some(list) = value.get("roles").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|entry| {
            let target = entry.get("target").and_then(Value::as_str)?;
            let role = entry
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut node = read_id(entry).unwrap_or(NodeRef::named(target));
            if node.file.is_none() {
                node.file = entry
                    .get("file")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            if node.span.is_none() {
                node.span = entry
                    .get("location")
                    .and_then(|l| read_span(Some(l)))
                    .or_else(|| {
                        let start = entry
                            .get("location")?
                            .get("start")?
                            .get("offset")?
                            .as_u64()? as u32;
                        let end =
                            entry.get("location")?.get("end")?.get("offset")?.as_u64()? as u32;
                        Some((start, end))
                    });
            }
            Some((node, role.to_string()))
        })
        .collect()
}

/// The declared types the reflection index reported.
pub fn reflect_types(value: &Value) -> Vec<NodeRef> {
    let Some(list) = value.get("types").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|entry| {
            let name = entry.get("name").and_then(Value::as_str)?;
            let mut node = read_id(entry).unwrap_or(NodeRef::named(name));
            if node.kind.is_none() {
                node.kind = entry
                    .get("kind")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            Some(node)
        })
        .collect()
}

/// The ranked hits a workspace search reported, whatever its result key is called.
///
/// The tool does not exist yet, so this reads the shape a ranked search has to have: a list under
/// `results`/`hits`/`symbols`, each entry naming a declaration. The arm that calls it reports SKIP
/// until the service advertises it, and this reader is what that arm will use on the day it does.
pub fn ranked_hits(value: &Value) -> Vec<NodeRef> {
    for key in ["results", "hits", "symbols", "matches", "nodes"] {
        if let Some(list) = value.get(key).and_then(Value::as_array) {
            return list
                .iter()
                .filter_map(|entry| {
                    let name = entry
                        .get("name")
                        .or_else(|| entry.get("target"))
                        .and_then(Value::as_str);
                    let mut node = match read_id(entry) {
                        Some(node) => node,
                        None => NodeRef::named(name?),
                    };
                    if node.file.is_none() {
                        node.file = entry
                            .get("file")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    Some(node)
                })
                .collect();
        }
    }
    Vec::new()
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_object_wins_over_the_legacy_fields() {
        let value = serde_json::json!({
            "name": "place_order",
            "file": "old.noe",
            "id": {
                "qualified": "Shop.handlers.orders.place_order",
                "kind": "function",
                "file": "handlers/orders.noe",
                "span": { "start": 120, "end": 131 }
            }
        });
        let node = read_id(&value).expect("an id object");
        assert_eq!(
            node.qualified.as_deref(),
            Some("Shop.handlers.orders.place_order")
        );
        assert_eq!(node.leaf, "place_order");
        assert_eq!(node.file.as_deref(), Some("handlers/orders.noe"));
        assert_eq!(node.span, Some((120, 131)));
    }

    #[test]
    fn without_an_id_a_trace_node_falls_back_to_its_own_fields() {
        let value = serde_json::json!({
            "found": true,
            "traces": [{
                "name": "Shop.store.save_order",
                "kind": "call",
                "file": "store.noe",
                "roles": ["Semantic.PersistenceBoundary"],
                "external": false,
                "dynamic": false,
                "children": []
            }]
        });
        let (found, hits) = trace(&value);
        assert!(found);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node.leaf, "save_order");
        assert_eq!(
            hits[0].node.qualified.as_deref(),
            Some("Shop.store.save_order")
        );
        assert_eq!(hits[0].node.span, None);
    }

    #[test]
    fn positions_resolve_to_byte_offsets_through_the_file_text() {
        let mut indexes = LineIndexes::new();
        indexes.add(
            "a.noe",
            "fn one(): int { return 1 }\nfn two(): int { return one() }\n",
        );
        let value = serde_json::json!({
            "found": true,
            "count": 1,
            "references": [{ "file": "a.noe", "range": {
                "start": { "line": 2, "column": 24 },
                "end": { "line": 2, "column": 27 }
            }}]
        });
        let hits = references(&value, "a.noe", &indexes);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].offset, 27 + 23);
    }
}
