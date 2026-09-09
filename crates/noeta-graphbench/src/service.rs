//! The service under measurement: the real [`noeta_mcp::NoetaMcp`], driven over an in-process
//! `tokio::io::duplex` by a real MCP client.
//!
//! Going over the wire rather than calling the Rust functions is the point. An arm sees exactly
//! what an agent sees, including the shapes that carry no identity, so a tool whose JSON changes is
//! a tool the benchmark notices.
//!
//! Two accounting decisions. Responses are **cached per (tool, arguments)** within a run, because
//! every MCP call builds a fresh `LangDatabase` and re-links the project, and a sweep of `symbols`
//! over a twelve-file project would otherwise cost seconds per question. The arm's **logical** call
//! count is still recorded, so "how many tool calls does this composition make" stays the number an
//! agent would pay. And an **ablation** short-circuits one tool to its empty shape before the call,
//! which is how a category proves it was reaching the tool it names.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use serde_json::{Map, Value};

/// Every source of evidence an arm can cite.
///
/// An enum rather than a string, so the report's evidence column is an exhaustive match and a new
/// tool cannot be silently omitted from a strategy's accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    Symbols,
    Definition,
    References,
    Trace,
    ModuleGraph,
    Reflect,
    CodeSearch,
    ContextMap,
    Path,
    Impact,
    Architecture,
    Callers,
    /// Reading a corpus file directly, which is what an agent's file-read tool does.
    FileRead,
}

impl Tool {
    pub fn as_str(self) -> &'static str {
        match self {
            Tool::Symbols => "symbols",
            Tool::Definition => "definition",
            Tool::References => "references",
            Tool::Trace => "trace",
            Tool::ModuleGraph => "module_graph",
            Tool::Reflect => "reflect",
            Tool::CodeSearch => "code_search",
            Tool::ContextMap => "context_map",
            Tool::Path => "path",
            Tool::Impact => "impact",
            Tool::Architecture => "architecture",
            Tool::Callers => "callers",
            Tool::FileRead => "file_read",
        }
    }

    /// Every tool the harness can ablate.
    pub fn ablatable() -> &'static [Tool] {
        &[
            Tool::Symbols,
            Tool::Definition,
            Tool::References,
            Tool::Trace,
            Tool::ModuleGraph,
            Tool::Reflect,
            Tool::Impact,
            Tool::Callers,
            Tool::ContextMap,
            Tool::Path,
            Tool::Architecture,
            Tool::FileRead,
        ]
    }

    /// The answer a tool gives when it has nothing: what an ablation substitutes.
    pub fn empty_answer(self) -> Value {
        match self {
            Tool::Symbols => serde_json::json!({ "symbols": [] }),
            Tool::Definition => {
                serde_json::json!({ "found": false, "location": null, "snippet": null })
            }
            Tool::References => {
                serde_json::json!({ "found": false, "count": 0, "references": [] })
            }
            Tool::Trace => {
                serde_json::json!({ "found": false, "traces": [], "boundaries": [], "truncated": false })
            }
            Tool::ModuleGraph => serde_json::json!({ "modules": [] }),
            Tool::Reflect => serde_json::json!({ "roles": [], "attributes": [], "types": [] }),
            Tool::FileRead => serde_json::json!({ "text": "" }),
            Tool::CodeSearch => serde_json::json!({ "results": [] }),
            Tool::ContextMap => serde_json::json!({
                "found": false, "files": [], "seeds": [], "candidates": [], "missing_seeds": []
            }),
            Tool::Path => {
                serde_json::json!({ "found": false, "paths": [], "candidates": [], "ranked": false })
            }
            Tool::Callers => {
                serde_json::json!({ "found": false, "levels": [], "candidates": [] })
            }
            Tool::Impact => serde_json::json!({
                "attributed": false, "decls": [], "tier_functions": [], "candidates": []
            }),
            Tool::Architecture => {
                serde_json::json!({ "roles": [], "edges": [], "connections": [], "boundaries": [] })
            }
        }
    }
}

impl std::fmt::Display for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Tool {
    type Err = String;

    fn from_str(text: &str) -> Result<Tool, String> {
        let all = [
            Tool::Symbols,
            Tool::Definition,
            Tool::References,
            Tool::Trace,
            Tool::ModuleGraph,
            Tool::Reflect,
            Tool::CodeSearch,
            Tool::ContextMap,
            Tool::Path,
            Tool::Impact,
            Tool::Architecture,
            Tool::Callers,
            Tool::FileRead,
        ];
        all.into_iter()
            .find(|t| t.as_str() == text)
            .ok_or_else(|| format!("`{text}` names no tool"))
    }
}

/// What one arm spent answering one question.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Spend {
    /// Logical tool calls the composition issued, cache hits included.
    pub calls: usize,
    /// Calls the response cache served, so the wall-clock number can be read honestly.
    pub cache_hits: usize,
    /// Characters of tool output the composition took in.
    pub chars: usize,
    pub millis: u128,
}

impl Spend {
    /// The token estimate the report carries: four characters to a token.
    pub fn tokens(&self) -> usize {
        self.chars.div_ceil(4)
    }

    pub fn add(&mut self, other: &Spend) {
        self.calls += other.calls;
        self.cache_hits += other.cache_hits;
        self.chars += other.chars;
        self.millis += other.millis;
    }
}

/// A live MCP session plus the run's response cache and accounting.
#[derive(Debug)]
pub struct Service {
    client: RunningService<RoleClient, ()>,
    advertised: HashSet<String>,
    cache: HashMap<String, Value>,
    ablated: Option<Tool>,
    spend: Spend,
    /// Which ranking `context_map` is asked for, so a run can swap personalized PageRank for
    /// degree centrality or a seeded draw and the arms stay untouched.
    ranker: Option<String>,
    /// Which ranking `path` is asked for: the flow ranker, or hop count alone.
    path_ranker: Option<String>,
    /// The token budget the map arms spend, when a run names one instead of the arm's own.
    map_budget: Option<usize>,
}

/// The ranking knobs a run turns. Both default to the tool's own default, so a plain run measures
/// what ships.
#[derive(Debug, Clone, Default)]
pub struct Rankers {
    pub context_map: Option<String>,
    pub path: Option<String>,
    /// The budget the map arms spend. `None` leaves each arm at its own.
    pub map_budget: Option<usize>,
}

impl Service {
    /// Start `NoetaMcp` on one end of a duplex and a client on the other.
    pub async fn start() -> Result<Service, String> {
        let (client_io, server_io) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            if let Ok(service) = noeta_mcp::NoetaMcp::new().serve(server_io).await {
                let _ = service.waiting().await;
            }
        });
        let client = ()
            .serve(client_io)
            .await
            .map_err(|e| format!("the MCP client did not initialize: {e}"))?;
        let listed = client
            .list_tools(Default::default())
            .await
            .map_err(|e| format!("tools/list failed: {e}"))?;
        let advertised = listed
            .tools
            .iter()
            .map(|t| t.name.to_string())
            .collect::<HashSet<_>>();
        Ok(Service {
            client,
            advertised,
            cache: HashMap::new(),
            ablated: None,
            spend: Spend::default(),
            ranker: None,
            path_ranker: None,
            map_budget: None,
        })
    }

    /// Set the ranking each ranked tool is asked for. Clears the response cache, because a cached
    /// answer was ranked by whatever was set when it was made.
    pub fn set_rankers(&mut self, rankers: &Rankers) {
        self.ranker = rankers.context_map.clone();
        self.path_ranker = rankers.path.clone();
        self.map_budget = rankers.map_budget;
        self.cache.clear();
    }

    /// The token budget a map call carries: what the run asked for, else the arm's own.
    pub fn map_budget(&self, arm_budget: usize) -> usize {
        self.map_budget.unwrap_or(arm_budget)
    }

    /// The ranking `context_map` is asked for, when a run named one.
    pub fn ranker(&self) -> Option<&str> {
        self.ranker.as_deref()
    }

    /// The ranking `path` is asked for, when a run named one.
    pub fn path_ranker(&self) -> Option<&str> {
        self.path_ranker.as_deref()
    }

    /// Whether the service advertises a tool. An arm that needs one it does not have reports SKIP.
    pub fn advertises(&self, tool: Tool) -> bool {
        self.advertised.contains(tool.as_str())
    }

    /// Every tool name the service advertises, sorted.
    pub fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.advertised.iter().cloned().collect();
        names.sort();
        names
    }

    /// Stub one tool's answers to its empty shape, for the self-ablation.
    pub fn ablate(&mut self, tool: Option<Tool>) {
        self.ablated = tool;
        // A cached answer from before the ablation would hide it.
        self.cache.clear();
    }

    pub fn ablated(&self) -> Option<Tool> {
        self.ablated
    }

    /// Take and reset the accounting, so a caller can attribute spend to one question.
    pub fn take_spend(&mut self) -> Spend {
        std::mem::take(&mut self.spend)
    }

    /// Call one tool. `arguments` is the tool's own request shape.
    pub async fn call(&mut self, tool: Tool, arguments: Map<String, Value>) -> Value {
        self.spend.calls += 1;
        if self.ablated == Some(tool) {
            let answer = tool.empty_answer();
            self.spend.chars += answer.to_string().len();
            return answer;
        }
        let key = format!("{tool}|{}", Value::Object(arguments.clone()));
        if let Some(hit) = self.cache.get(&key) {
            self.spend.cache_hits += 1;
            self.spend.chars += hit.to_string().len();
            return hit.clone();
        }
        let started = Instant::now();
        let mut params = CallToolRequestParams::default();
        params.name = tool.as_str().to_string().into();
        params.arguments = Some(arguments);
        let answer = match self.client.call_tool(params).await {
            Ok(result) => result
                .structured_content
                .unwrap_or_else(|| serde_json::json!({ "error": "no structured content" })),
            Err(error) => serde_json::json!({ "error": error.to_string() }),
        };
        self.spend.millis += started.elapsed().as_millis();
        self.spend.chars += answer.to_string().len();
        self.cache.insert(key, answer.clone());
        answer
    }

    /// Read a corpus file, accounted like a tool call because an agent pays for it like one.
    pub fn read_file(&mut self, path: &std::path::Path) -> String {
        self.spend.calls += 1;
        if self.ablated == Some(Tool::FileRead) {
            return String::new();
        }
        let key = format!("{}|{}", Tool::FileRead, path.display());
        if let Some(hit) = self.cache.get(&key).and_then(Value::as_str) {
            self.spend.cache_hits += 1;
            self.spend.chars += hit.len();
            return hit.to_string();
        }
        let started = Instant::now();
        let text = std::fs::read_to_string(path).unwrap_or_default();
        self.spend.millis += started.elapsed().as_millis();
        self.spend.chars += text.len();
        self.cache.insert(key, Value::String(text.clone()));
        text
    }

    /// Close the session.
    pub async fn shutdown(self) {
        let _ = self.client.cancel().await;
    }
}

/// Build a tool request from `(name, value)` pairs.
pub fn args<const N: usize>(pairs: [(&str, Value); N]) -> Map<String, Value> {
    let mut map = Map::new();
    for (name, value) in pairs {
        map.insert(name.to_string(), value);
    }
    map
}
