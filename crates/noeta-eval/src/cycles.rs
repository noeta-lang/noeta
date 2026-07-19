//! **In-run safepoint cycle collection** for the `Rc`-based reference interpreter — the eval
//! mirror of the VM's mid-run collector (memory-management 6.x).
//!
//! The exit reapers (`reap_captured_scope_cycles` / `reap_object_cycles`) reclaim `Rc` cycles only
//! at clean exit, so a cycle-building loop grew residency without bound until then. This module
//! reclaims destructor-free cycle garbage *during* execution, at the interpreter's loop/call
//! safepoints, under the same semantic rule as the VM (`noeta-gc`): **a safepoint collection never
//! runs a destructor**. Dead components containing a destructor-bearing object are deferred intact
//! to the exit reapers (same members, same firing order and output as before); destructor-free
//! reclamation is unobservable (destructor spec §1), so the two backends need no synchronized
//! collection points and the differential holds by construction.
//!
//! **Algorithm** — trial deletion over the `Rc` graph, seeded from the existing weak candidate
//! registries (captured scopes + mutated objects, the proven complete set of possible cycle
//! anchors: every eval cycle passes through a scope's bindings or an object's slots, the only
//! mutable heap links):
//!
//! 1. Build the subgraph reachable from the candidates, one node per distinct `Rc` allocation
//!    (scopes, closures, objects, enum values, type/enum defs, list/tuple/set vectors, maps,
//!    iterators, boxed future/message values), holding exactly one analysis handle per node.
//! 2. Count each node's **internal in-edges** (each edge enumerated once per `Rc` clone the
//!    parent actually owns). A node whose `Rc::strong_count` exceeds `internal + 1` (our handle)
//!    has an owner outside the subgraph — a Rust local, an interpreter field, a register of the
//!    other backend's world doesn't exist here: *every* owner is a counted `Rc`, which is what
//!    makes this collection sound at any interpreter safepoint. Externally-owned nodes seed a
//!    liveness propagation over their children; the residue is dead.
//! 3. **Verify** the dead set exactly: each dead node's strong count must equal its in-edges from
//!    the dead set plus our handle. Any mismatch aborts the whole collection (reclaiming nothing)
//!    — an edge-enumeration gap can then only cost liveness until exit, never a wrong free.
//! 4. Partition the dead set into weakly-connected components; defer any component containing an
//!    object whose type has a `destruct` block (its registry entries stay for the exit reapers).
//! 5. Reclaim the rest by draining every dead scope's bindings and dead object's slots — each
//!    cycle contains at least one such mutable link, so the drained graph cascade-frees through
//!    plain `Rc` drops, firing no destructor (the set is destructor-free by construction).

use std::collections::HashMap;
use std::rc::Rc;

use crate::value::{IterState, ListRepr, Value};
use crate::{Closure, EnumDef, EnumValue, ObjectValue, Scope, TypeDef, leak};

/// One analysis handle per distinct `Rc` allocation in the candidate subgraph. Holding the
/// variant's `Rc` keeps the node alive for the collection's duration (exactly one handle per
/// node — the `+1` the external-owner arithmetic accounts for).
enum Node {
    Scope(Rc<Scope>),
    Object(Rc<ObjectValue>),
    Closure(Rc<Closure>),
    EnumValue(Rc<EnumValue>),
    TypeDef(Rc<TypeDef>),
    EnumDef(Rc<EnumDef>),
    /// A list/tuple/set element vector.
    Values(Rc<Vec<Value>>),
    Map(Rc<std::collections::BTreeMap<noeta_stdlib::MapKey, Value>>),
    Iter(Rc<std::cell::RefCell<IterState>>),
    /// A boxed inner value: a `Future`'s payload or a `ChannelSend`'s queued message.
    Boxed(Rc<Value>),
}

/// A node's identity: the `Rc` data pointer (unique among live allocations). The variant tag is
/// folded in defensively so two allocations of different types can never collide even in exotic
/// allocator reuse scenarios (`Rc` data pointers of live allocations are already unique).
type NodeKey = usize;

impl Node {
    fn key(&self) -> NodeKey {
        match self {
            Node::Scope(rc) => Rc::as_ptr(rc) as usize,
            Node::Object(rc) => Rc::as_ptr(rc) as usize,
            Node::Closure(rc) => Rc::as_ptr(rc) as usize,
            Node::EnumValue(rc) => Rc::as_ptr(rc) as usize,
            Node::TypeDef(rc) => Rc::as_ptr(rc) as usize,
            Node::EnumDef(rc) => Rc::as_ptr(rc) as usize,
            Node::Values(rc) => Rc::as_ptr(rc) as usize,
            Node::Map(rc) => Rc::as_ptr(rc) as usize,
            Node::Iter(rc) => Rc::as_ptr(rc) as usize,
            Node::Boxed(rc) => Rc::as_ptr(rc) as usize,
        }
    }

    fn strong_count(&self) -> usize {
        match self {
            Node::Scope(rc) => Rc::strong_count(rc),
            Node::Object(rc) => Rc::strong_count(rc),
            Node::Closure(rc) => Rc::strong_count(rc),
            Node::EnumValue(rc) => Rc::strong_count(rc),
            Node::TypeDef(rc) => Rc::strong_count(rc),
            Node::EnumDef(rc) => Rc::strong_count(rc),
            Node::Values(rc) => Rc::strong_count(rc),
            Node::Map(rc) => Rc::strong_count(rc),
            Node::Iter(rc) => Rc::strong_count(rc),
            Node::Boxed(rc) => Rc::strong_count(rc),
        }
    }

    /// The nodes this one owns an `Rc` clone of — **exactly one entry per owned clone** (the
    /// in-edge arithmetic depends on it: a missed edge only makes a node look externally owned,
    /// i.e. survive; a double-counted edge would be caught by the dead-set verification).
    fn children(&self) -> Vec<Node> {
        let mut out = Vec::new();
        match self {
            Node::Scope(scope) => {
                if let Some(parent) = &scope.parent {
                    out.push(Node::Scope(Rc::clone(parent)));
                }
                for binding in scope.vars.borrow().values() {
                    push_value_edge(&binding.value, &mut out);
                }
            }
            Node::Object(object) => {
                out.push(Node::TypeDef(Rc::clone(&object.def)));
                for value in object.slots.borrow().iter() {
                    push_value_edge(value, &mut out);
                }
            }
            Node::Closure(closure) => {
                out.push(Node::Scope(Rc::clone(&closure.captured)));
            }
            Node::EnumValue(e) => {
                for value in &e.data {
                    push_value_edge(value, &mut out);
                }
            }
            Node::TypeDef(def) => {
                for method in def.methods.values() {
                    out.push(Node::Closure(Rc::clone(method)));
                }
            }
            Node::EnumDef(def) => {
                for method in def.methods.values() {
                    out.push(Node::Closure(Rc::clone(method)));
                }
            }
            Node::Values(items) => {
                for value in items.iter() {
                    push_value_edge(value, &mut out);
                }
            }
            Node::Map(entries) => {
                for value in entries.values() {
                    push_value_edge(value, &mut out);
                }
            }
            Node::Iter(state) => {
                for value in iter_state_values(&state.borrow()) {
                    push_value_edge(&value, &mut out);
                }
            }
            Node::Boxed(inner) => {
                push_value_edge(inner, &mut out);
            }
        }
        out
    }
}

/// The node a value owns, if any. Leaves (scalars, strings, bytes, packed buffers, extern boxes —
/// acyclic by ABI contract — handles, endpoints, native/builtin references) own no traversable
/// node. A `BoundMethod`'s receiver lives in an owned `Box`, so its edge belongs to the enclosing
/// node (flattened here).
fn push_value_edge(value: &Value, out: &mut Vec<Node>) {
    match value {
        Value::List(ListRepr::Boxed { items, .. }) => out.push(Node::Values(Rc::clone(items))),
        Value::Tuple(items) | Value::Set(items, _) => out.push(Node::Values(Rc::clone(items))),
        Value::Map(entries, _) => out.push(Node::Map(Rc::clone(entries))),
        Value::Function(c) => out.push(Node::Closure(Rc::clone(c))),
        Value::EnumType(d) => out.push(Node::EnumDef(Rc::clone(d))),
        Value::Enum(e) => out.push(Node::EnumValue(Rc::clone(e))),
        Value::Type(t) => out.push(Node::TypeDef(Rc::clone(t))),
        Value::Object(o) => out.push(Node::Object(Rc::clone(o))),
        Value::Iter(i) => out.push(Node::Iter(Rc::clone(i))),
        Value::Future(f) => out.push(Node::Boxed(Rc::clone(f))),
        Value::ChannelSend(_, m) => out.push(Node::Boxed(Rc::clone(m))),
        Value::BoundMethod(inner, _) => push_value_edge(inner, out),
        _ => {}
    }
}

/// The child values an iterator state owns (cloned out under a short borrow — no user code runs
/// during collection, so the borrow cannot conflict).
fn iter_state_values(state: &IterState) -> Vec<Value> {
    match state {
        IterState::List { list, .. } => vec![list.clone()],
        IterState::Take { source, .. }
        | IterState::Drop { source, .. }
        | IterState::Enumerate { source, .. } => vec![source.clone()],
        IterState::Chain { first, second } => vec![first.clone(), second.clone()],
        IterState::Zip { a, b } => vec![a.clone(), b.clone()],
        IterState::Map { source, func } => vec![source.clone(), func.clone()],
        IterState::Filter { source, pred } => vec![source.clone(), pred.clone()],
        IterState::Gen { step } => vec![step.clone()],
    }
}

/// The interpreter's safepoint poll: one thread-local bool read when idle; a due collection runs
/// [`safepoint_collect`] and re-arms the trigger. Sound at any point where no `RefCell` borrow is
/// held across it (the loop-iteration and call-entry sites) — `Rc` counts every Rust-held value,
/// so unlike the VM there is no root-enumeration constraint.
#[inline]
pub(crate) fn poll_safepoint() {
    if leak::safepoint_pending() {
        safepoint_collect();
        leak::safepoint_rearm();
    }
}

/// Run one safepoint collection: reclaim every destructor-free dead component reachable from the
/// candidate registries, defer the rest to the exit reapers. Called from the interpreter's
/// loop/call safepoints when [`leak::safepoint_pending`]; the caller re-arms the trigger.
pub(crate) fn safepoint_collect() {
    let scope_candidates = crate::captured_scope_candidates();
    let object_candidates = crate::mutated_object_candidates();
    if scope_candidates.is_empty() && object_candidates.is_empty() {
        return;
    }

    // 1. Build the reachable subgraph: one handle per node, children recorded by key.
    let mut nodes: HashMap<NodeKey, Node> = HashMap::new();
    let mut children: HashMap<NodeKey, Vec<NodeKey>> = HashMap::new();
    let mut worklist: Vec<Node> = Vec::new();
    for scope in &scope_candidates {
        worklist.push(Node::Scope(Rc::clone(scope)));
    }
    for object in &object_candidates {
        worklist.push(Node::Object(Rc::clone(object)));
    }
    while let Some(node) = worklist.pop() {
        let key = node.key();
        if nodes.contains_key(&key) {
            continue;
        }
        let kids = node.children();
        let kid_keys: Vec<NodeKey> = kids.iter().map(Node::key).collect();
        children.insert(key, kid_keys);
        nodes.insert(key, node);
        worklist.extend(kids);
    }
    // Drop the raw candidate handles before reading strong counts, so each node's analysis
    // ownership is exactly the one handle in `nodes`.
    drop(scope_candidates);
    drop(object_candidates);

    // 2. Internal in-edges, then liveness from externally-owned seeds.
    let mut in_edges: HashMap<NodeKey, usize> = HashMap::with_capacity(nodes.len());
    for kids in children.values() {
        for &kid in kids {
            *in_edges.entry(kid).or_insert(0) += 1;
        }
    }
    let mut live: HashMap<NodeKey, bool> = HashMap::with_capacity(nodes.len());
    let mut seeds: Vec<NodeKey> = Vec::new();
    for (&key, node) in &nodes {
        let internal = in_edges.get(&key).copied().unwrap_or(0);
        let externally_owned = node.strong_count() > internal + 1;
        live.insert(key, externally_owned);
        if externally_owned {
            seeds.push(key);
        }
    }
    while let Some(key) = seeds.pop() {
        for &kid in &children[&key] {
            if let Some(flag) = live.get_mut(&kid)
                && !*flag
            {
                *flag = true;
                seeds.push(kid);
            }
        }
    }
    let dead: Vec<NodeKey> = nodes.keys().copied().filter(|k| !live[k]).collect();
    if dead.is_empty() {
        return;
    }

    // 3. Exact verification over the dead set: every owner of a dead node must be another dead
    // node (plus our handle). Abort wholesale on any mismatch.
    let dead_set: std::collections::HashSet<NodeKey> = dead.iter().copied().collect();
    let mut dead_in: HashMap<NodeKey, usize> = HashMap::with_capacity(dead.len());
    for &key in &dead {
        for &kid in &children[&key] {
            if dead_set.contains(&kid) {
                *dead_in.entry(kid).or_insert(0) += 1;
            }
        }
    }
    for &key in &dead {
        let expected = dead_in.get(&key).copied().unwrap_or(0) + 1;
        if nodes[&key].strong_count() != expected {
            return;
        }
    }

    // 4. Weakly-connected components over the dead set; defer destructor-bearing ones.
    let mut adjacency: HashMap<NodeKey, Vec<NodeKey>> = HashMap::with_capacity(dead.len());
    for &key in &dead {
        for &kid in &children[&key] {
            if dead_set.contains(&kid) && kid != key {
                adjacency.entry(key).or_default().push(kid);
                adjacency.entry(kid).or_default().push(key);
            }
        }
    }
    let mut component_of: HashMap<NodeKey, usize> = HashMap::with_capacity(dead.len());
    let mut components: Vec<Vec<NodeKey>> = Vec::new();
    for &start in &dead {
        if component_of.contains_key(&start) {
            continue;
        }
        let id = components.len();
        let mut members = Vec::new();
        let mut queue = vec![start];
        component_of.insert(start, id);
        while let Some(key) = queue.pop() {
            members.push(key);
            for &next in adjacency.get(&key).map(Vec::as_slice).unwrap_or(&[]) {
                if let std::collections::hash_map::Entry::Vacant(slot) = component_of.entry(next) {
                    slot.insert(id);
                    queue.push(next);
                }
            }
        }
        components.push(members);
    }

    // 5. Drain the mutable links of every reclaimable component, then let `Rc` cascade.
    let mut drained: Vec<Value> = Vec::new();
    for component in components {
        let has_destructor = component
            .iter()
            .any(|key| matches!(&nodes[key], Node::Object(o) if o.def.destructor.is_some()));
        if has_destructor {
            continue;
        }
        for key in component {
            match &nodes[&key] {
                Node::Scope(scope) => {
                    drained.extend(
                        scope
                            .vars
                            .borrow_mut()
                            .drain()
                            .map(|(_, binding)| binding.value),
                    );
                    scope.order.borrow_mut().clear();
                }
                Node::Object(object) => {
                    drained.extend(object.slots.borrow_mut().drain(..));
                }
                // Every cycle passes through a scope or object (the only mutable links); the
                // other node kinds cascade once those are drained.
                _ => {}
            }
        }
    }
    // Release the analysis handles first, then the drained values: the dead subgraph's counts
    // fall to zero and plain `Rc` drops reclaim it — no destructor fires (the reclaimed set is
    // destructor-free by construction; `leak::dec` runs in the aggregates' `Drop` impls).
    drop(nodes);
    drop(drained);
    // Registry hygiene: entries whose target was just freed can no longer upgrade.
    crate::prune_cycle_registries();
}
