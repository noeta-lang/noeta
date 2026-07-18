//! The **instrumenting** collector (P1): exact per-function call counts + self / total time, and —
//! since the instrument-flamegraph slice — the exact **call tree**, so the instrumenting profiler
//! renders a flamegraph too (weighted by measured self-nanoseconds rather than sample counts).
//!
//! It implements [`ProfileHook`], so the VM consults it before every interpreted op with a view of
//! the live call stack. Rather than hook the VM's ~13 scattered frame push/pop sites, it keeps a
//! **shadow stack** and reconciles it against the live frame stack each op: a frame that appeared is
//! a call *enter* (start its timer, bump its count), one that vanished is a call *exit* (bank its
//! time). The common case — same innermost frame as the previous op — is a single length+proto
//! compare and returns immediately, so the per-op cost is tiny; a real clock is read only at the
//! ~two ops that bracket each call.
//!
//! Timing bookkeeping is the textbook self/total split. Each shadow entry tracks the time charged to
//! its callees (`child_ns`); on exit, `self = elapsed − child_ns` and the parent's `child_ns` grows
//! by this frame's whole `elapsed`. **Total** (inclusive) time is counted only when a proto's
//! *outermost* activation exits (a per-proto active-depth counter), so recursion never double-counts
//! it. Counts and self-time are exact; total-time is exact for the outermost activation.
//!
//! The call tree is a trie over the same enter/exit events: a node is a distinct **path** of protos
//! root→frame; enter descends (creating the child on first visit) and bumps the node's call count,
//! exit banks the frame's self-time into the node it exits from. Per-path self-times sum exactly to
//! the per-function self-times — the tree is the same measurement, keyed finer.

use std::collections::HashMap;
use std::time::Instant;

use noeta_vm::{DebugView, ProfileHook};

/// The parent key of a root-level call-tree node (no parent).
const ROOT: u32 = u32::MAX;

/// One live activation on the shadow stack.
struct Active {
    proto: u32,
    /// The call-tree node this activation banks into (the path root→this frame).
    node: u32,
    start: Instant,
    /// Time already banked to this frame's callees — subtracted from `elapsed` to get self-time.
    child_ns: u64,
}

/// One node of the exact call tree: a distinct root→frame path, with the calls that took exactly
/// this path and the self-time banked while this path's leaf frame was executing.
pub struct RawTreeNode {
    /// The parent node's index, or `None` for a root-level frame.
    pub parent: Option<u32>,
    /// The prototype executing at this path's leaf.
    pub proto: u32,
    /// Activations that took exactly this path.
    pub calls: u64,
    /// Nanoseconds measured with this path's leaf as the executing frame (exclusive of callees).
    pub self_ns: u64,
}

/// Per-function accumulator, keyed by prototype index (into `Module::protos`).
pub struct InstrumentCollector {
    stack: Vec<Active>,
    calls: Vec<u64>,
    self_ns: Vec<u64>,
    total_ns: Vec<u64>,
    /// Per-proto count of live activations (recursion depth), so inclusive time is banked only when
    /// the outermost activation of a proto exits.
    active: Vec<u32>,
    /// The call-tree nodes, appended in first-visit order (a parent always precedes its children).
    nodes: Vec<RawTreeNode>,
    /// `(parent node | ROOT, proto)` → node index: the trie edges.
    edges: HashMap<(u32, u32), u32>,
}

/// One function's raw counters, as the collector produced them (proto index + counts/times). The
/// caller ([`crate`]) resolves `proto` → name @ file:line against the compiled module.
pub struct RawStat {
    pub proto: u32,
    pub calls: u64,
    pub self_ns: u64,
    pub total_ns: u64,
}

impl InstrumentCollector {
    /// A collector sized for a module with `protos` prototypes (its per-proto tables are dense).
    pub fn new(protos: usize) -> InstrumentCollector {
        InstrumentCollector {
            stack: Vec::new(),
            calls: vec![0; protos],
            self_ns: vec![0; protos],
            total_ns: vec![0; protos],
            active: vec![0; protos],
            nodes: Vec::new(),
            edges: HashMap::new(),
        }
    }

    fn enter(&mut self, proto: u32) {
        let i = proto as usize;
        self.calls[i] += 1;
        self.active[i] += 1;
        // Descend the call tree: the current node's child for this proto (created on first visit).
        let parent = self.stack.last().map(|a| a.node).unwrap_or(ROOT);
        let nodes = &mut self.nodes;
        let node = *self.edges.entry((parent, proto)).or_insert_with(|| {
            nodes.push(RawTreeNode {
                parent: (parent != ROOT).then_some(parent),
                proto,
                calls: 0,
                self_ns: 0,
            });
            (nodes.len() - 1) as u32
        });
        self.nodes[node as usize].calls += 1;
        self.stack.push(Active {
            proto,
            node,
            start: Instant::now(),
            child_ns: 0,
        });
    }

    fn exit_top(&mut self) {
        let Active {
            proto,
            node,
            start,
            child_ns,
        } = self.stack.pop().expect("exit_top on an empty shadow stack");
        let elapsed = start.elapsed().as_nanos() as u64;
        let this_self = elapsed.saturating_sub(child_ns);
        let i = proto as usize;
        self.self_ns[i] += this_self;
        self.nodes[node as usize].self_ns += this_self;
        self.active[i] -= 1;
        if self.active[i] == 0 {
            self.total_ns[i] += elapsed;
        }
        if let Some(parent) = self.stack.last_mut() {
            parent.child_ns += elapsed;
        }
    }

    /// Drain any activations still live at program end (the outermost `main`, and any leaf caught
    /// mid-call by an abort), then produce one [`RawStat`] per function that was actually called,
    /// plus the exact call tree.
    pub fn finish(mut self) -> (Vec<RawStat>, Vec<RawTreeNode>) {
        while !self.stack.is_empty() {
            self.exit_top();
        }
        let stats = (0..self.calls.len())
            .filter(|&i| self.calls[i] > 0)
            .map(|i| RawStat {
                proto: i as u32,
                calls: self.calls[i],
                self_ns: self.self_ns[i],
                total_ns: self.total_ns[i],
            })
            .collect();
        (stats, self.nodes)
    }
}

impl ProfileHook for InstrumentCollector {
    fn before_op(&mut self, view: &DebugView) {
        let depth = view.depth();
        // Fast path: still executing the same innermost frame as the previous op — the overwhelming
        // majority of ops. One length compare + one proto compare, no clock read.
        if depth == self.stack.len()
            && (depth == 0 || self.stack[depth - 1].proto == view.proto_at(depth - 1))
        {
            return;
        }
        // Reconcile the shadow stack with the live frames by proto, bottom-up. `i` is the first level
        // that differs (or the shorter length); everything above it in the shadow is stale and gets
        // banked, then the newly-present frames are entered. Normally the divergence is at the very
        // top, so this is O(1); a task switch that swaps many frames at once is handled correctly too.
        let mut i = 0;
        while i < self.stack.len() && i < depth && self.stack[i].proto == view.proto_at(i) {
            i += 1;
        }
        while self.stack.len() > i {
            self.exit_top();
        }
        while self.stack.len() < depth {
            let proto = view.proto_at(self.stack.len());
            self.enter(proto);
        }
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}
