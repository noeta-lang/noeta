//! The **allocation** collector (`noeta profile --alloc`): exact allocated-bytes attribution over
//! the call tree — the memory flamegraph.
//!
//! It rides the same per-op [`ProfileHook`] seam and the same shadow-stack + trie machinery as the
//! instrumenting collector, but banks a different weight: before each op it reads the thread's
//! **cumulative allocated-bytes counter** ([`noeta_alloc_probe::thread_allocated`], maintained by
//! the `noeta` binary's counting global allocator) and attributes the delta since the previous op
//! to the call path that was executing — the bytes an op allocated land on the stack that ran it
//! (off by at most one op, the same skew class as a sampling tick). No VM allocation site is
//! hooked; the allocator itself is the single choke point, so *everything* the interpreted code
//! allocates is counted, native builtins included.
//!
//! Frees are deliberately ignored (the counter is monotonic): the profile answers "who allocates",
//! the churn/pressure question, not "who retains" (a heap snapshot's question). Only the
//! interpreter thread's allocations are counted — host/runtime threads have their own counters —
//! and an isolate's OS thread is likewise not attributed to the main program's stacks.

use noeta_vm::{DebugView, ProfileHook};

use crate::instrument::RawTreeNode;
use std::collections::HashMap;

/// The parent key of a root-level call-tree node (no parent).
const ROOT: u32 = u32::MAX;

/// One live activation on the shadow stack (no clocks — only the tree cursor).
struct Active {
    proto: u32,
    /// The call-tree node this activation banks into (the path root→this frame).
    node: u32,
}

/// The allocation collector: a shadow stack mirroring the live frames (reconciled per op, exactly
/// as the instrumenting collector does) plus the byte-weighted call-tree trie.
pub struct AllocCollector {
    stack: Vec<Active>,
    /// The call-tree nodes, appended in first-visit order (a parent always precedes its children).
    nodes: Vec<RawTreeNode>,
    /// `(parent node | ROOT, proto)` → node index: the trie edges.
    edges: HashMap<(u32, u32), u32>,
    /// The thread-allocated reading at the previous op — `None` until the first op, so bytes
    /// allocated before the program starts (compilation, host setup) are never attributed.
    last: Option<u64>,
}

impl AllocCollector {
    pub fn new() -> AllocCollector {
        AllocCollector {
            stack: Vec::new(),
            nodes: Vec::new(),
            edges: HashMap::new(),
            last: None,
        }
    }

    fn enter(&mut self, proto: u32) {
        let parent = self.stack.last().map(|a| a.node).unwrap_or(ROOT);
        let nodes = &mut self.nodes;
        let node = *self.edges.entry((parent, proto)).or_insert_with(|| {
            nodes.push(RawTreeNode {
                parent: (parent != ROOT).then_some(parent),
                proto,
                calls: 0,
                weight: 0,
            });
            (nodes.len() - 1) as u32
        });
        self.nodes[node as usize].calls += 1;
        self.stack.push(Active { proto, node });
    }

    /// The exact byte-weighted call tree. Node weights sum to every byte allocated on the
    /// interpreter thread while interpreted code was executing.
    pub fn finish(self) -> Vec<RawTreeNode> {
        self.nodes
    }
}

impl Default for AllocCollector {
    fn default() -> Self {
        AllocCollector::new()
    }
}

impl ProfileHook for AllocCollector {
    fn before_op(&mut self, view: &DebugView) {
        // Bank the bytes allocated since the previous op to the path that was executing then —
        // BEFORE reconciling, so a delta straddling a call/return lands on the stack that did the
        // allocating.
        let now = noeta_alloc_probe::thread_allocated();
        if let Some(last) = self.last
            && let Some(top) = self.stack.last()
        {
            let delta = now.saturating_sub(last);
            if delta > 0 {
                self.nodes[top.node as usize].weight += delta;
            }
        }
        self.last = Some(now);

        // Reconcile the shadow stack with the live frames by proto, bottom-up — the instrumenting
        // collector's algorithm, minus the clocks (see `instrument.rs` for the reasoning).
        let depth = view.depth();
        if depth == self.stack.len()
            && (depth == 0 || self.stack[depth - 1].proto == view.proto_at(depth - 1))
        {
            return;
        }
        let mut i = 0;
        while i < self.stack.len() && i < depth && self.stack[i].proto == view.proto_at(i) {
            i += 1;
        }
        self.stack.truncate(i);
        while self.stack.len() < depth {
            let proto = view.proto_at(self.stack.len());
            self.enter(proto);
        }
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}
