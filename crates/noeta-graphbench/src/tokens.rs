//! What the node identity costs on the wire, and what is paid for twice.
//!
//! Every graph tool now carries an `id` object on each node it reports, and most of them still
//! carry the pre-id fields beside it: an absolute `file` next to the id's root-relative one, a
//! `location` whose offsets are the id's span, a `name` and a `kind` the id already spells. Both
//! halves are billed to the agent's context window, so this measures them separately:
//!
//! - **id** — the bytes of the `id` objects themselves.
//! - **duplicated** — the bytes of parent fields that state a fact the sibling `id` already states.
//!   A field counts only when its value *matches* the id's: `trace`'s `kind` is `call`/`root`
//!   rather than a node kind, so it is a different fact and is not counted.
//!
//! Nothing here changes the wire. It reports what a trimming pass would be worth.

use std::collections::BTreeMap;

use serde_json::Value;

/// One tool's answer, measured.
#[derive(Debug, Clone, Default)]
pub struct Cost {
    /// Bytes of the whole response.
    pub total: usize,
    /// Bytes of the `id` objects.
    pub id: usize,
    /// Bytes of fields a sibling `id` already carries.
    pub duplicated: usize,
    /// How many `id` objects the answer carries.
    pub nodes: usize,
}

impl Cost {
    pub fn tokens(bytes: usize) -> usize {
        bytes.div_ceil(4)
    }

    pub fn add(&mut self, other: &Cost) {
        self.total += other.total;
        self.id += other.id;
        self.duplicated += other.duplicated;
        self.nodes += other.nodes;
    }

    /// Bytes of id per node reported.
    pub fn id_per_node(&self) -> usize {
        if self.nodes == 0 {
            return 0;
        }
        self.id / self.nodes
    }
}

/// Measure one tool response.
pub fn measure(value: &Value) -> Cost {
    let mut cost = Cost {
        total: value.to_string().len(),
        ..Cost::default()
    };
    walk(value, &mut cost);
    cost
}

fn walk(value: &Value, cost: &mut Cost) {
    match value {
        Value::Object(map) => {
            if let Some(id) = map.get("id").filter(|id| id.is_object()) {
                cost.nodes += 1;
                cost.id += field_len("id", id);
                for (key, own) in map {
                    if key != "id" && states_the_same_fact(key, own, id) {
                        cost.duplicated += field_len(key, own);
                    }
                }
            }
            for (_, child) in map {
                walk(child, cost);
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, cost);
            }
        }
        _ => {}
    }
}

/// The bytes one `"key": value` pair costs, comma included.
fn field_len(key: &str, value: &Value) -> usize {
    key.len() + 3 + value.to_string().len()
}

/// Whether a field beside an `id` states something the `id` already states.
fn states_the_same_fact(key: &str, own: &Value, id: &Value) -> bool {
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let number = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64);
    let span = id.get("span");
    match key {
        "name" => {
            let Some(name) = own.as_str() else {
                return false;
            };
            let Some(full) = text(id, "name") else {
                return false;
            };
            name == full || full.rsplit('.').next() == Some(name)
        }
        "kind" => own
            .as_str()
            .is_some_and(|k| text(id, "kind").as_deref() == Some(k)),
        "file" => match (own.as_str(), text(id, "file")) {
            (Some(own), Some(relative)) => own.ends_with(&relative),
            _ => false,
        },
        "line" => own
            .as_u64()
            .is_some_and(|line| span.and_then(|s| number(s, "line")) == Some(line)),
        "location" => {
            let start = own.get("start").and_then(|s| number(s, "offset"));
            let end = own.get("end").and_then(|s| number(s, "offset"));
            start.is_some()
                && start == span.and_then(|s| number(s, "start"))
                && end == span.and_then(|s| number(s, "end"))
        }
        _ => false,
    }
}

/// A whole run's per-tool measurement.
#[derive(Debug, Default)]
pub struct Ledger {
    pub by_tool: BTreeMap<String, Cost>,
}

impl Ledger {
    pub fn record(&mut self, tool: &str, value: &Value) {
        let cost = measure(value);
        self.by_tool.entry(tool.to_string()).or_default().add(&cost);
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(
            "graphbench: what the node identity costs on the wire (tokens, four bytes to one)\n\n",
        );
        out.push_str(&format!(
            "{:<14} {:>6} {:>9} {:>9} {:>11} {:>9} {:>11}\n",
            "tool", "nodes", "total", "id", "id/node", "duplicate", "duplicate%"
        ));
        let mut totals = Cost::default();
        for (tool, cost) in &self.by_tool {
            totals.add(cost);
            out.push_str(&format!(
                "{:<14} {:>6} {:>9} {:>9} {:>11} {:>9} {:>10.1}%\n",
                tool,
                cost.nodes,
                Cost::tokens(cost.total),
                Cost::tokens(cost.id),
                Cost::tokens(cost.id_per_node()),
                Cost::tokens(cost.duplicated),
                100.0 * cost.duplicated as f64 / cost.total.max(1) as f64,
            ));
        }
        out.push_str(&format!(
            "{:<14} {:>6} {:>9} {:>9} {:>11} {:>9} {:>10.1}%\n",
            "all",
            totals.nodes,
            Cost::tokens(totals.total),
            Cost::tokens(totals.id),
            Cost::tokens(totals.id_per_node()),
            Cost::tokens(totals.duplicated),
            100.0 * totals.duplicated as f64 / totals.total.max(1) as f64,
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_repeating_its_id_is_counted_and_a_different_fact_is_not() {
        let node = serde_json::json!({
            "name": "authorize",
            "kind": "call",
            "file": "/abs/corpus/orders_service/auth.noe",
            "line": 17,
            "id": {
                "name": "Shop.auth.authorize",
                "kind": "function",
                "file": "auth.noe",
                "span": { "start": 446, "end": 455, "line": 17, "column": 8 }
            }
        });
        let cost = measure(&node);
        assert_eq!(cost.nodes, 1);
        // `name`, `file` and `line` repeat the id; `kind` here is how the edge was reached
        // (`call`), which the id's `function` does not state.
        assert!(cost.duplicated > 0);
        assert!(!states_the_same_fact(
            "kind",
            &serde_json::json!("call"),
            node.get("id").unwrap()
        ));
        assert!(states_the_same_fact(
            "line",
            &serde_json::json!(17),
            node.get("id").unwrap()
        ));
    }
}
