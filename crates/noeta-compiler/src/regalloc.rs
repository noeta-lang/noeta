//! Reuse-aware register coalescing — a bytecode→bytecode post-pass over a compiled [`Chunk`]
//! (memory-management migration, Phase 3.3).
//!
//! The IR→bytecode lowering allocates registers **monotonically**: every temporary and local gets
//! a fresh register that lives until frame teardown. This pass reclaims that waste. It computes,
//! over the chunk's control-flow graph, each register's live range, then **merges register numbers
//! whose ranges never overlap** onto one physical slot. Two effects fall out:
//!
//! * `Chunk.num_registers` shrinks (smaller per-activation register arrays), retiring the monotonic
//!   allocator's bloat.
//! * Reclamation becomes *prompt*: when a later value reuses a slot, [`set_reg`]'s release-on-write
//!   frees the previous (now-dead) occupant at that point rather than at teardown — the headline
//!   peak-residency win — with **no new VM op** ([`set_reg`] already releases the old value).
//!
//! ## Safety
//!
//! Two register numbers share a physical slot only when they are **never simultaneously live**, so
//! at runtime a slot never holds two live values at once — merging is sound and the release on
//! overwrite always frees a genuinely dead value. Correctness rests on three invariants:
//!
//! 1. **Conservative liveness.** A standard backward dataflow to a fixpoint over the real CFG (jump
//!    targets + fall-through), so a value live across a loop back-edge stays live across the whole
//!    loop. Where flow is uncertain we *over*-approximate liveness, never under — the "never too
//!    early" invariant. Under-approximation would be the only way to wrongly merge.
//! 2. **Interference via the later definition.** If two values are simultaneously live, the later
//!    one's defining op sees the earlier in its `live_out`, so the standard "def vs live_out" edge
//!    captures every interference. Parameters have no defining op, so they are **pinned** to their
//!    own distinct colors (`0..num_params`) — both because they cannot otherwise be made to
//!    interfere and because the calling convention and default-thunks address them by absolute
//!    register index.
//! 3. **No intra-op aliasing.** We also add an edge between an op's def and each of its uses, so a
//!    physical slot is never simultaneously a source *and* the destination of one instruction —
//!    making the rewrite independent of each op's internal read-before-write ordering.
//!
//! [`set_reg`]: ../../noeta_vm/index.html

use noeta_bytecode::{CaptureFrom, Chunk, Const, Op, Reg, StrPart};

/// Coalesce `chunk`'s registers in place: rename register numbers onto the smallest set of physical
/// slots that respects liveness, and shrink `num_registers` to match. Behaviour-preserving — only
/// *which* slot holds each value changes (and dead values are released earlier).
/// A constant with no heap allocation and no destructor — safe to materialize **once** and read from
/// the same register across many loop iterations (P-VMT-LICM). `Str`/`NativeModule` are excluded:
/// they carry an `Rc`, so sharing one materialization across iterations would change refcount/drop
/// timing versus the per-iteration re-materialization the loop body expects.
fn is_primitive_const(c: &Const) -> bool {
    matches!(
        c,
        Const::Unit | Const::Bool(_) | Const::Int(_) | Const::Float(_) | Const::F32(_)
    )
}

/// Apply `f` to each jump-target field an op carries (the same set [`patch_jump`] handles), for the
/// LICM rebuild's target fix-up.
fn for_each_target_mut(op: &mut Op, mut f: impl FnMut(&mut u32)) {
    match op {
        Op::Jump { target }
        | Op::JumpIfFalse { target, .. }
        | Op::JumpIfTrue { target, .. }
        | Op::CondBranch { target, .. } => f(target),
        Op::Coalesce { fallback, .. } => f(fallback),
        Op::MatchInt { fail, .. }
        | Op::MatchStr { fail, .. }
        | Op::MatchBool { fail, .. }
        | Op::MatchVariant { fail, .. }
        | Op::MatchTuple { fail, .. } => f(fail),
        _ => {}
    }
}

/// Hoist loop-invariant primitive-constant loads out of loops (P-VMT-LICM). Runs on the **monotonic**
/// pre-coalesce code (every value in its own write-once register), *before* [`coalesce`].
///
/// A `LoadConst` of a primitive (int/float/bool/f32/unit — see [`is_primitive_const`]) inside a loop
/// re-materializes the constant every iteration. When its destination register is written only by that
/// one load and read only as a **borrowing** arithmetic/comparison operand (`Binary`/`WideInt`, which
/// never clear their inputs), the load is safely hoisted to a pre-header just before the loop: the
/// value is materialized once and every iteration reads the surviving register. Because the code uses
/// structured jump targets, the chunk is rebuilt with an old→new index remap and all jumps are fixed
/// up — no manual per-op arithmetic. Behaviour is identical (a primitive load has no side effect and
/// the value is invariant), so the VM's result is unchanged; the eval backend never runs this (it
/// interprets the IR), so the differential is untouched.
pub fn hoist_loop_invariant_consts(chunk: &mut Chunk) {
    let n = chunk.code.len();
    // Loop regions from backward unconditional jumps (`while`/`for` emit a back `Jump` to the top).
    let loops: Vec<(usize, usize)> = chunk
        .code
        .iter()
        .enumerate()
        .filter_map(|(j, op)| match op {
            Op::Jump { target } if *target as usize <= j => Some((*target as usize, j)),
            _ => None,
        })
        .collect();
    if loops.is_empty() {
        return;
    }

    // Per-register: how many ops define it, and whether any *non*-arithmetic op reads it. A register
    // read only by `Binary`/`WideInt` is never consumed, so hoisting its load can't strand a later
    // iteration on a cleared slot.
    let regs = chunk.num_registers as usize;
    let mut def_count = vec![0u32; regs];
    let mut nonarith_use = vec![false; regs];
    // Indices that are jump targets: a `LoadConst` sitting on one is a control-flow merge point (e.g.
    // the op after an `if`-expression, where `prev + 1` loads its `1`). Moving it would leave the
    // branches jumping into the pre-header, so such loads are never hoisted.
    let mut is_target = vec![false; n];
    for op in &chunk.code {
        let facts = op_facts(op);
        if let Some(d) = facts.def {
            def_count[d as usize] += 1;
        }
        if !matches!(op, Op::Binary { .. } | Op::WideInt { .. }) {
            for u in facts.uses {
                nonarith_use[u as usize] = true;
            }
        }
        for t in facts.targets {
            if (t as usize) < n {
                is_target[t as usize] = true;
            }
        }
    }

    // For each hoistable `LoadConst`, record it under the top of its innermost enclosing loop.
    let mut hoist_at: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut hoisted = vec![false; n];
    for p in 0..n {
        if is_target[p] {
            continue; // a control-flow merge point — moving it would strand its branches
        }
        let Op::LoadConst { dst, k } = chunk.code[p] else {
            continue;
        };
        if !is_primitive_const(&chunk.consts[k as usize]) {
            continue;
        }
        let d = dst as usize;
        if def_count[d] != 1 || nonarith_use[d] {
            continue;
        }
        // Innermost enclosing loop = the one with the greatest top that still contains `p`.
        let top = loops
            .iter()
            .filter(|&&(top, end)| top < p && p <= end)
            .map(|&(top, _)| top)
            .max();
        if let Some(top) = top {
            hoist_at[top].push(p);
            hoisted[p] = true;
        }
    }
    if !hoisted.iter().any(|&h| h) {
        return;
    }

    // Rebuild: at each loop top, emit its hoisted loads first (the pre-header), then the top's own op;
    // skip each hoisted load at its original site. `remap[old] = new` for every retained/moved op.
    let mut new_code: Vec<Op> = Vec::with_capacity(n);
    let mut remap = vec![0u32; n];
    for old in 0..n {
        for &src in &hoist_at[old] {
            remap[src] = new_code.len() as u32;
            new_code.push(chunk.code[src].clone());
        }
        if hoisted[old] {
            continue; // already emitted in its loop's pre-header above
        }
        remap[old] = new_code.len() as u32;
        new_code.push(chunk.code[old].clone());
    }
    for op in &mut new_code {
        for_each_target_mut(op, |t| *t = remap[*t as usize]);
    }
    // The debug line table is pc-keyed, so it moves with the code (empty in a non-debug compile, so
    // this is then a no-op). Every original pc is retained — a hoisted load keeps its entry, now at
    // its pre-header position — so remapping each entry through `remap` keeps the table accurate.
    for entry in &mut chunk.line_table {
        entry.pc = remap[entry.pc as usize];
    }
    chunk.code = new_code;
}

pub fn coalesce(chunk: &mut Chunk) {
    let n = chunk.num_registers as usize;
    if n == 0 {
        return;
    }
    let liveness = Liveness::analyze(&chunk.code, n);
    let mut interfere = build_interference(&chunk.code, &liveness, n);
    // Pin the panic-teardown registers (Phase 4.2c-ii): make each interfere with every other
    // register so colouring gives it a unique slot and never reuses it for another local. Without
    // this, a destructor-bearing local that dies only at an abort-skipped drop would have its slot
    // coalesced away and its value lost before the panic teardown fires. Parameters are already
    // pinned by `color`; temporaries (not in `frame_locals`) are untouched and still coalesce.
    for &reg in &chunk.frame_locals {
        let r = reg as usize;
        for other in 0..n {
            if other != r {
                interfere[r].insert(other);
                interfere[other].insert(r);
            }
        }
    }
    // Pin every debug-locals register the same way. `declare_local` bindings are already covered
    // (their registers sit in `frame_locals` on a debug compile), but loop/match bindings are
    // deliberately NOT teardown-listed (a debug compile must not change which destructors fire on
    // a panic) — this pin keeps their `reg → name` 1:1 and their value readable at any pause,
    // without touching the teardown list. Empty on non-debug compiles, so a no-op there.
    for local in &chunk.debug_locals {
        let r = local.reg as usize;
        for other in 0..n {
            if other != r {
                interfere[r].insert(other);
                interfere[other].insert(r);
            }
        }
    }
    let colors = color(&interfere, n, chunk.num_params as usize);

    let new_count = colors.iter().copied().max().map_or(0, |c| c + 1);
    for op in &mut chunk.code {
        remap_op(op, &colors);
    }
    // Remap the panic-teardown list through the same colouring (it is metadata, not code, so the
    // op-walk above misses it). Two locals coalesced to one register collapse to one entry —
    // harmless, since at any panic point only one of them is live and the VM fires a register once.
    let mut seen = vec![false; new_count];
    chunk.frame_locals.retain_mut(|reg| {
        let c = colors[*reg as usize];
        *reg = c as u16;
        if seen[c] {
            false
        } else {
            seen[c] = true;
            true
        }
    });
    // Remap the debugger's `reg → name` records through the same colouring (also metadata).
    // Every debug-locals register is pinned to its own colour above, so coalescing never
    // collapses two *distinct* locals onto one register; a match binding that deliberately
    // ALIASES its scrutinee's register shares a slot before and after — two names for one
    // register there is the truth, not a collapse. In a non-debug compile `debug_locals` is
    // empty and this is a no-op.
    for local in &mut chunk.debug_locals {
        local.reg = colors[local.reg as usize] as u16;
    }
    chunk.num_registers = new_count as u16;
}

/// The registers an op **reads** (`uses`) and the single register it fully **overwrites** (`def`),
/// plus its control-flow successors. ANF makes every non-parameter register write-once except for
/// reassigned `mut` locals, which simply contribute several defs to the one register's range.
struct OpFacts {
    def: Option<Reg>,
    uses: Vec<Reg>,
    /// Explicit jump targets (instruction indices).
    targets: Vec<u32>,
    /// Whether control can fall through to the next instruction.
    fallthrough: bool,
}

/// Enumerate an op's register reads/writes and control flow. Exhaustive by design — there is no
/// `_` arm, so adding an `Op` variant forces this to be revisited (a missed register would be a
/// silent use-after-free).
fn op_facts(op: &Op) -> OpFacts {
    // Most ops fall through to the next instruction; only jumps/terminators differ.
    let mut f = OpFacts {
        def: None,
        uses: Vec::new(),
        targets: Vec::new(),
        fallthrough: true,
    };
    match op {
        // ── pure value producers: def = dst, uses = the read registers ──
        Op::LoadConst { dst, .. } => f.def = Some(*dst),
        Op::Move { dst, src } => {
            f.def = Some(*dst);
            f.uses.push(*src);
        }
        Op::LoadGlobal { dst, .. } => f.def = Some(*dst),
        Op::StoreGlobal { src, .. } => f.uses.push(*src),
        Op::TakeGlobal { dst, .. } => f.def = Some(*dst),
        Op::Drop { reg, .. } => {
            // Reads then clears the register to unit: a use that kills the value.
            f.uses.push(*reg);
            f.def = Some(*reg);
        }
        Op::ConcatInPlace { dst, lhs, rhs, .. } => {
            f.def = Some(*dst);
            f.uses.push(*lhs);
            f.uses.push(*rhs);
        }
        Op::MakeClosure { dst, captures, .. } => {
            f.def = Some(*dst);
            for c in captures.iter() {
                if let CaptureFrom::Local(r) = c {
                    f.uses.push(*r);
                }
            }
        }
        Op::MakeCell { dst, src } => {
            f.def = Some(*dst);
            f.uses.push(*src);
        }
        Op::CellGet { dst, cell } => {
            f.def = Some(*dst);
            f.uses.push(*cell);
        }
        Op::CellSet { cell, src } => {
            // Mutates the cell object through `cell`; writes no register.
            f.uses.push(*cell);
            f.uses.push(*src);
        }
        Op::UpvalueGet { dst, .. } => f.def = Some(*dst),
        Op::UpvalueSet { src, .. } => f.uses.push(*src),
        Op::LoadNativeFn { dst, .. } => f.def = Some(*dst),
        Op::BindMethod { dst, recv, .. } => {
            f.def = Some(*dst);
            f.uses.push(*recv);
        }
        Op::MakeList { dst, items, .. } | Op::MakeTuple { dst, items } => {
            f.def = Some(*dst);
            f.uses.extend(items.iter().copied());
        }
        Op::PackedListNew { dst, .. } => f.def = Some(*dst),
        Op::PackedListPush {
            dst, list, value, ..
        } => {
            f.def = Some(*dst);
            f.uses.push(*list);
            f.uses.push(*value);
        }
        Op::TupleIndex { dst, receiver, .. } => {
            f.def = Some(*dst);
            f.uses.push(*receiver);
        }
        Op::MakeRange {
            dst, start, end, ..
        } => {
            f.def = Some(*dst);
            f.uses.push(*start);
            f.uses.push(*end);
        }
        Op::MakeMap { dst, entries, .. } => {
            f.def = Some(*dst);
            for (k, v) in entries.iter() {
                f.uses.push(*k);
                f.uses.push(*v);
            }
        }
        Op::RequireMapKey { reg, .. } => f.uses.push(*reg),
        Op::IterSnapshot { dst, src, .. } => {
            f.def = Some(*dst);
            f.uses.push(*src);
        }
        Op::ListLen { dst, src, .. } => {
            f.def = Some(*dst);
            f.uses.push(*src);
        }
        Op::ListGet { dst, list, index } => {
            f.def = Some(*dst);
            f.uses.push(*list);
            f.uses.push(*index);
        }
        // Defines two registers (`elem` here, `has` via `extra_defs`); reads the iterator.
        Op::IterForNext { iter, elem, .. } => {
            f.def = Some(*elem);
            f.uses.push(*iter);
        }
        Op::CallBuiltin { dst, args, .. } => {
            f.def = Some(*dst);
            f.uses.extend(args.iter().copied());
        }
        Op::CallMethod {
            dst, recv, args, ..
        } => {
            f.def = Some(*dst);
            f.uses.push(*recv);
            f.uses.extend(args.iter().copied());
        }
        Op::Index {
            dst, recv, index, ..
        }
        | Op::IndexField {
            dst, recv, index, ..
        } => {
            f.def = Some(*dst);
            f.uses.push(*recv);
            f.uses.push(*index);
        }
        Op::MakeStruct {
            dst, named, spread, ..
        } => {
            f.def = Some(*dst);
            for (_, r) in named.iter() {
                f.uses.push(*r);
            }
            if let Some(s) = spread {
                f.uses.push(*s);
            }
        }
        Op::MakeStructInPlace {
            dst, named, base, ..
        } => {
            f.def = Some(*dst);
            for (_, r) in named.iter() {
                f.uses.push(*r);
            }
            f.uses.push(*base);
        }
        Op::MakeOpaque {
            dst, keys, spread, ..
        } => {
            f.def = Some(*dst);
            for (_, r) in keys.iter() {
                f.uses.push(*r);
            }
            if let Some(s) = spread {
                f.uses.push(*s);
            }
        }
        Op::MakeEnum { dst, args, .. } => {
            f.def = Some(*dst);
            f.uses.extend(args.iter().copied());
        }
        Op::EnumFromStr { dst, arg, .. } => {
            f.def = Some(*dst);
            f.uses.push(*arg);
        }
        Op::LoadField { dst, obj, .. } => {
            f.def = Some(*dst);
            f.uses.push(*obj);
        }
        Op::SetField {
            dst, obj, value, ..
        } => {
            f.def = Some(*dst);
            f.uses.push(*obj);
            f.uses.push(*value);
        }
        Op::Panic { msg, .. } => {
            f.uses.push(*msg);
            f.fallthrough = false; // aborts the program
        }
        Op::TryUnwrap {
            dst, src, on_error, ..
        } => {
            // On the error path it early-returns from the frame; on success continues. Either way
            // `dst` is defined on the continue path and `src` is read. The `on_error` registers are
            // read (dropped) on the error path, so they are uses too — keeping them live to this op.
            f.def = Some(*dst);
            f.uses.push(*src);
            for (reg, _) in on_error.iter() {
                f.uses.push(*reg);
            }
        }
        Op::Coalesce {
            dst, src, fallback, ..
        } => {
            f.def = Some(*dst);
            f.uses.push(*src);
            f.targets.push(*fallback);
        }
        Op::Narrow { dst, src, .. } => {
            f.def = Some(*dst);
            f.uses.push(*src);
        }
        Op::IsType { dst, src, .. } => {
            f.def = Some(*dst);
            f.uses.push(*src);
        }
        Op::MakeGen { dst, src }
        | Op::MakeFuture { dst, src }
        | Op::RunFuture { dst, src, .. }
        | Op::PollFuture { dst, src, .. }
        | Op::ScopeReady { dst, src, .. }
        | Op::Spawn { dst, src, .. } => {
            f.def = Some(*dst);
            f.uses.push(*src);
        }
        Op::MakeChannel { dst, capacity, .. } => {
            f.def = Some(*dst);
            f.uses.push(*capacity);
        }
        Op::LoadPending { dst } | Op::ScopeBeginValue { dst, .. } => f.def = Some(*dst),
        Op::ScopeEndAt { src, .. } => f.uses.push(*src),
        Op::ScopeBegin | Op::ScopeEnd { .. } => {}
        Op::AttributesOf { dst, dynamic, .. } => {
            f.def = Some(*dst);
            f.uses.extend(dynamic.iter().copied());
        }
        Op::RolesOf { dst, .. } => f.def = Some(*dst),
        Op::ParamsOf { dst, src } | Op::ReturnsOf { dst, src } => {
            f.def = Some(*dst);
            f.uses.push(*src);
        }
        Op::TypeOf { dst, src }
        | Op::FieldsOf { dst, src }
        | Op::TraitsOf { dst, src }
        | Op::FieldSpecsOf { dst, src } => {
            f.def = Some(*dst);
            f.uses.push(*src);
        }
        Op::FromBytes { dst, src, .. } => {
            f.def = Some(*dst);
            f.uses.push(*src);
        }
        Op::TypeOfStatic { dst, .. } => f.def = Some(*dst),
        Op::TypeValue { dst, .. } => f.def = Some(*dst),
        Op::Invoke {
            dst,
            recv,
            name,
            args,
            ..
        } => {
            f.def = Some(*dst);
            // The free-fn form reads no receiver register — its callee comes from a global slot.
            if let Some(recv) = recv {
                f.uses.push(*recv);
            }
            f.uses.push(*name);
            f.uses.push(*args);
        }
        Op::Construct {
            dst, name, fields, ..
        } => {
            f.def = Some(*dst);
            f.uses.push(*name);
            f.uses.push(*fields);
        }
        Op::MatchInt { src, fail, .. } => {
            f.uses.push(*src);
            f.targets.push(*fail);
        }
        Op::MatchStr { src, fail, .. } => {
            f.uses.push(*src);
            f.targets.push(*fail);
        }
        Op::MatchBool { src, fail, .. } => {
            f.uses.push(*src);
            f.targets.push(*fail);
        }
        Op::MatchVariant { src, fail, .. } => {
            f.uses.push(*src);
            f.targets.push(*fail);
        }
        Op::MatchTuple { src, fail, .. } => {
            f.uses.push(*src);
            f.targets.push(*fail);
        }
        Op::ExtractField { dst, src, .. } => {
            f.def = Some(*dst);
            f.uses.push(*src);
        }
        Op::MatchFail { src, .. } => {
            f.uses.push(*src);
            f.fallthrough = false; // aborts the program
        }
        Op::Call {
            dst, callee, args, ..
        } => {
            f.def = Some(*dst);
            f.uses.push(*callee);
            f.uses.extend(args.iter().copied());
        }
        Op::CallGlobal { dst, args, .. } => {
            f.def = Some(*dst);
            f.uses.extend(args.iter().copied());
        }
        Op::SpawnIsolate {
            dst, callee, args, ..
        } => {
            f.def = Some(*dst);
            f.uses.push(*callee);
            f.uses.extend(args.iter().copied());
        }
        Op::TypedModuleCall {
            dst, args, dynamic, ..
        } => {
            f.def = Some(*dst);
            f.uses.extend(args.iter().copied());
            f.uses.extend(dynamic.iter().copied());
        }
        Op::TypedMethodCall {
            dst,
            recv,
            args,
            dynamic,
            ..
        } => {
            f.def = Some(*dst);
            f.uses.push(*recv);
            f.uses.extend(args.iter().copied());
            f.uses.extend(dynamic.iter().copied());
        }
        Op::DecodeTyped {
            dst, name, text, ..
        } => {
            f.def = Some(*dst);
            f.uses.push(*name);
            f.uses.push(*text);
        }
        Op::TraitMethod {
            dst, recv, args, ..
        } => {
            f.def = Some(*dst);
            f.uses.push(*recv);
            f.uses.extend(args.iter().copied());
        }
        Op::Return { src } => {
            f.uses.push(*src);
            f.fallthrough = false;
        }
        Op::Unary { dst, src, .. } => {
            f.def = Some(*dst);
            f.uses.push(*src);
        }
        Op::MaskWidth { dst, src, .. } => {
            f.def = Some(*dst);
            f.uses.push(*src);
        }
        Op::Binary { dst, a, b, .. } | Op::WideInt { dst, a, b, .. } => {
            f.def = Some(*dst);
            f.uses.push(*a);
            f.uses.push(*b);
        }
        Op::WidthIntMethod { dst, recv, arg, .. } => {
            f.def = Some(*dst);
            f.uses.push(*recv);
            if let Some(a) = arg {
                f.uses.push(*a);
            }
        }
        Op::RequireBool { reg, .. } => f.uses.push(*reg),
        Op::RequireCondBool { reg, .. } => f.uses.push(*reg),
        Op::Jump { target } => {
            f.targets.push(*target);
            f.fallthrough = false;
        }
        Op::JumpIfTrue { reg, target } => {
            f.uses.push(*reg);
            f.targets.push(*target);
        }
        Op::JumpIfFalse { reg, target } => {
            f.uses.push(*reg);
            f.targets.push(*target);
        }
        Op::CondBranch { reg, target, .. } => {
            f.uses.push(*reg);
            f.targets.push(*target);
        }
        Op::Echo { reg } => f.uses.push(*reg),
        Op::Stringify { dst, src, .. } => {
            f.def = Some(*dst);
            f.uses.push(*src);
        }
        Op::BuildString { dst, parts } => {
            f.def = Some(*dst);
            for part in parts.iter() {
                if let StrPart::Hole(r) = part {
                    f.uses.push(*r);
                }
            }
        }
        Op::Raise { .. } => f.fallthrough = false,
        Op::Halt => f.fallthrough = false,
    }
    f
}

/// Registers an op writes *in addition* to its primary `def`. No current op is multi-def (the only
/// one, `DestructurePair`, was retired with the tuple-destructure migration — object-model slice 4b),
/// but the mechanism is kept so a future multi-def op needs no liveness rework.
fn extra_defs(op: &Op) -> Option<Reg> {
    // `IterForNext` writes a second register — the bool continue flag (its element is the primary
    // `def`); both must be treated as defined by liveness (Track I.2).
    match op {
        Op::IterForNext { has, .. } => Some(*has),
        _ => None,
    }
}

/// Per-instruction live-register sets, as fixed-width bitsets (one `u64` word per 64 registers).
struct Liveness {
    /// `live_out[i]` — registers live on entry to *some* successor of instruction `i`.
    live_out: Vec<BitSet>,
}

impl Liveness {
    fn analyze(code: &[Op], n: usize) -> Liveness {
        let len = code.len();
        let mut live_in = vec![BitSet::new(n); len];
        let mut live_out = vec![BitSet::new(n); len];

        // Backward dataflow to a fixpoint. Iterating until no set changes handles loops (a back-edge
        // re-feeds a successor's `live_in` into the predecessor's `live_out`).
        let mut changed = true;
        while changed {
            changed = false;
            for i in (0..len).rev() {
                let facts = op_facts(&code[i]);
                // live_out[i] = ∪ live_in[successors]
                let mut out = BitSet::new(n);
                if facts.fallthrough && i + 1 < len {
                    out.union_with(&live_in[i + 1]);
                }
                for &t in &facts.targets {
                    if (t as usize) < len {
                        out.union_with(&live_in[t as usize]);
                    }
                }
                // live_in[i] = uses ∪ (live_out[i] − defs)
                let mut in_ = out.clone();
                if let Some(d) = facts.def {
                    in_.remove(d as usize);
                }
                if let Some(d) = extra_defs(&code[i]) {
                    in_.remove(d as usize);
                }
                for &u in &facts.uses {
                    in_.insert(u as usize);
                }

                if out != live_out[i] {
                    live_out[i] = out;
                    changed = true;
                }
                if in_ != live_in[i] {
                    live_in[i] = in_;
                    changed = true;
                }
            }
        }
        Liveness { live_out }
    }
}

/// An undirected interference graph as an adjacency bitset per register.
fn build_interference(code: &[Op], liveness: &Liveness, n: usize) -> Vec<BitSet> {
    let mut adj = vec![BitSet::new(n); n];
    let mut add = |a: usize, b: usize| {
        if a != b {
            adj[a].insert(b);
            adj[b].insert(a);
        }
    };
    for (i, op) in code.iter().enumerate() {
        let facts = op_facts(op);
        let out = &liveness.live_out[i];
        let defs: Vec<Reg> = facts.def.into_iter().chain(extra_defs(op)).collect();
        // A definition interferes with everything live after it (the later of two simultaneously
        // live values always reaches this edge), with this op's own uses (no intra-op source/dest
        // aliasing), and with the op's *other* defs (an op writing two registers must keep them
        // distinct, e.g. `DestructurePair`).
        for &d in &defs {
            for r in out.iter() {
                add(d as usize, r);
            }
            for &u in &facts.uses {
                add(d as usize, u as usize);
            }
            for &d2 in &defs {
                add(d as usize, d2 as usize);
            }
        }
    }
    adj
}

/// Greedy graph colouring with parameters pre-coloured to their own register numbers. Returns the
/// new physical register for each old register. Pinned parameters (`0..num_params`) map to
/// themselves; every other register takes the lowest colour no interfering neighbour already holds.
fn color(adj: &[BitSet], n: usize, num_params: usize) -> Vec<usize> {
    const UNASSIGNED: usize = usize::MAX;
    let mut colors = vec![UNASSIGNED; n];
    // Parameters keep their own slot — the entry sequence and default thunks address them by index.
    for (p, c) in colors.iter_mut().enumerate().take(num_params.min(n)) {
        *c = p;
    }
    // Colour the rest in register order (a stable, deterministic schedule).
    for r in num_params..n {
        let mut taken = vec![false; n];
        for nb in adj[r].iter() {
            if colors[nb] != UNASSIGNED {
                taken[colors[nb]] = true;
            }
        }
        let mut c = 0;
        while c < n && taken[c] {
            c += 1;
        }
        colors[r] = c;
    }
    colors
}

/// Rewrite every register field of `op` through `colors`.
fn remap_op(op: &mut Op, colors: &[usize]) {
    let m = |r: &mut Reg| *r = colors[*r as usize] as Reg;
    match op {
        Op::LoadConst { dst, .. } => m(dst),
        Op::Move { dst, src } => {
            m(dst);
            m(src);
        }
        Op::LoadGlobal { dst, .. } => m(dst),
        Op::StoreGlobal { src, .. } => m(src),
        Op::TakeGlobal { dst, .. } => m(dst),
        Op::Drop { reg, .. } => m(reg),
        Op::ConcatInPlace { dst, lhs, rhs, .. } => {
            m(dst);
            m(lhs);
            m(rhs);
        }
        Op::MakeClosure { dst, captures, .. } => {
            m(dst);
            for c in captures.iter_mut() {
                if let CaptureFrom::Local(r) = c {
                    m(r);
                }
            }
        }
        Op::MakeCell { dst, src } => {
            m(dst);
            m(src);
        }
        Op::CellGet { dst, cell } => {
            m(dst);
            m(cell);
        }
        Op::CellSet { cell, src } => {
            m(cell);
            m(src);
        }
        Op::UpvalueGet { dst, .. } => m(dst),
        Op::UpvalueSet { src, .. } => m(src),
        Op::LoadNativeFn { dst, .. } => m(dst),
        Op::BindMethod { dst, recv, .. } => {
            m(dst);
            m(recv);
        }
        Op::MakeList { dst, items, .. } | Op::MakeTuple { dst, items } => {
            m(dst);
            for r in items.iter_mut() {
                m(r);
            }
        }
        Op::PackedListNew { dst, .. } => m(dst),
        Op::PackedListPush {
            dst, list, value, ..
        } => {
            m(dst);
            m(list);
            m(value);
        }
        Op::TupleIndex { dst, receiver, .. } => {
            m(dst);
            m(receiver);
        }
        Op::MakeRange {
            dst, start, end, ..
        } => {
            m(dst);
            m(start);
            m(end);
        }
        Op::MakeMap { dst, entries, .. } => {
            m(dst);
            for (k, v) in entries.iter_mut() {
                m(k);
                m(v);
            }
        }
        Op::RequireMapKey { reg, .. } => m(reg),
        Op::IterSnapshot { dst, src, .. } => {
            m(dst);
            m(src);
        }
        Op::ListLen { dst, src, .. } => {
            m(dst);
            m(src);
        }
        Op::ListGet { dst, list, index } => {
            m(dst);
            m(list);
            m(index);
        }
        Op::IterForNext {
            iter, elem, has, ..
        } => {
            m(iter);
            m(elem);
            m(has);
        }
        Op::CallBuiltin { dst, args, .. } => {
            m(dst);
            for r in args.iter_mut() {
                m(r);
            }
        }
        Op::CallMethod {
            dst, recv, args, ..
        } => {
            m(dst);
            m(recv);
            for r in args.iter_mut() {
                m(r);
            }
        }
        Op::Index {
            dst, recv, index, ..
        }
        | Op::IndexField {
            dst, recv, index, ..
        } => {
            m(dst);
            m(recv);
            m(index);
        }
        Op::MakeStruct {
            dst, named, spread, ..
        } => {
            m(dst);
            for (_, r) in named.iter_mut() {
                m(r);
            }
            if let Some(s) = spread {
                m(s);
            }
        }
        Op::MakeStructInPlace {
            dst, named, base, ..
        } => {
            m(dst);
            for (_, r) in named.iter_mut() {
                m(r);
            }
            m(base);
        }
        Op::MakeOpaque {
            dst, keys, spread, ..
        } => {
            m(dst);
            for (_, r) in keys.iter_mut() {
                m(r);
            }
            if let Some(s) = spread {
                m(s);
            }
        }
        Op::MakeEnum { dst, args, .. } => {
            m(dst);
            for r in args.iter_mut() {
                m(r);
            }
        }
        Op::EnumFromStr { dst, arg, .. } => {
            m(dst);
            m(arg);
        }
        Op::LoadField { dst, obj, .. } => {
            m(dst);
            m(obj);
        }
        Op::SetField {
            dst, obj, value, ..
        } => {
            m(dst);
            m(obj);
            m(value);
        }
        Op::Panic { msg, .. } => m(msg),
        Op::TryUnwrap {
            dst, src, on_error, ..
        } => {
            m(dst);
            m(src);
            for (reg, _) in on_error.iter_mut() {
                m(reg);
            }
        }
        Op::Coalesce { dst, src, .. } => {
            m(dst);
            m(src);
        }
        Op::Narrow { dst, src, .. } => {
            m(dst);
            m(src);
        }
        Op::IsType { dst, src, .. } => {
            m(dst);
            m(src);
        }
        Op::MakeGen { dst, src }
        | Op::MakeFuture { dst, src }
        | Op::RunFuture { dst, src, .. }
        | Op::PollFuture { dst, src, .. }
        | Op::ScopeReady { dst, src, .. }
        | Op::Spawn { dst, src, .. } => {
            m(dst);
            m(src);
        }
        Op::MakeChannel { dst, capacity, .. } => {
            m(dst);
            m(capacity);
        }
        Op::LoadPending { dst } | Op::ScopeBeginValue { dst, .. } => m(dst),
        Op::ScopeEndAt { src, .. } => m(src),
        Op::ScopeBegin | Op::ScopeEnd { .. } => {}
        Op::AttributesOf { dst, dynamic, .. } => {
            m(dst);
            if let Some(slot) = dynamic {
                m(slot);
            }
        }
        Op::RolesOf { dst, .. } => m(dst),
        Op::ParamsOf { dst, src } | Op::ReturnsOf { dst, src } => {
            m(dst);
            m(src);
        }
        Op::TypeOf { dst, src }
        | Op::FieldsOf { dst, src }
        | Op::TraitsOf { dst, src }
        | Op::FieldSpecsOf { dst, src } => {
            m(dst);
            m(src);
        }
        Op::FromBytes { dst, src, .. } => {
            m(dst);
            m(src);
        }
        Op::TypeOfStatic { dst, .. } => m(dst),
        Op::TypeValue { dst, .. } => m(dst),
        Op::Invoke {
            dst,
            recv,
            name,
            args,
            ..
        } => {
            m(dst);
            if let Some(recv) = recv {
                m(recv);
            }
            m(name);
            m(args);
        }
        Op::Construct {
            dst, name, fields, ..
        } => {
            m(dst);
            m(name);
            m(fields);
        }
        Op::MatchInt { src, .. } => m(src),
        Op::MatchStr { src, .. } => m(src),
        Op::MatchBool { src, .. } => m(src),
        Op::MatchVariant { src, .. } => m(src),
        Op::MatchTuple { src, .. } => m(src),
        Op::ExtractField { dst, src, .. } => {
            m(dst);
            m(src);
        }
        Op::MatchFail { src, .. } => m(src),
        Op::Call {
            dst, callee, args, ..
        } => {
            m(dst);
            m(callee);
            for r in args.iter_mut() {
                m(r);
            }
        }
        Op::CallGlobal { dst, args, .. } => {
            m(dst);
            for r in args.iter_mut() {
                m(r);
            }
        }
        Op::SpawnIsolate {
            dst, callee, args, ..
        } => {
            m(dst);
            m(callee);
            for r in args.iter_mut() {
                m(r);
            }
        }
        Op::TypedModuleCall {
            dst, args, dynamic, ..
        } => {
            m(dst);
            for r in args.iter_mut() {
                m(r);
            }
            if let Some(slot) = dynamic {
                m(slot);
            }
        }
        Op::TypedMethodCall {
            dst,
            recv,
            args,
            dynamic,
            ..
        } => {
            m(dst);
            m(recv);
            for r in args.iter_mut() {
                m(r);
            }
            if let Some(slot) = dynamic {
                m(slot);
            }
        }
        Op::DecodeTyped {
            dst, name, text, ..
        } => {
            m(dst);
            m(name);
            m(text);
        }
        Op::TraitMethod {
            dst, recv, args, ..
        } => {
            m(dst);
            m(recv);
            for r in args.iter_mut() {
                m(r);
            }
        }
        Op::Return { src } => m(src),
        Op::Unary { dst, src, .. } => {
            m(dst);
            m(src);
        }
        Op::MaskWidth { dst, src, .. } => {
            m(dst);
            m(src);
        }
        Op::Binary { dst, a, b, .. } | Op::WideInt { dst, a, b, .. } => {
            m(dst);
            m(a);
            m(b);
        }
        Op::WidthIntMethod { dst, recv, arg, .. } => {
            m(dst);
            m(recv);
            if let Some(a) = arg {
                m(a);
            }
        }
        Op::RequireBool { reg, .. } => m(reg),
        Op::RequireCondBool { reg, .. } => m(reg),
        Op::CondBranch { reg, .. } => m(reg),
        Op::Jump { .. } => {}
        Op::JumpIfTrue { reg, .. } => m(reg),
        Op::JumpIfFalse { reg, .. } => m(reg),
        Op::Echo { reg } => m(reg),
        Op::Stringify { dst, src, .. } => {
            m(dst);
            m(src);
        }
        Op::BuildString { dst, parts } => {
            m(dst);
            for part in parts.iter_mut() {
                if let StrPart::Hole(r) = part {
                    m(r);
                }
            }
        }
        Op::Raise { .. } => {}
        Op::Halt => {}
    }
}

/// A small fixed-width bitset over register indices (`u64` words). Cheap clone/compare/union for the
/// dataflow fixpoint.
#[derive(Clone, PartialEq, Eq)]
struct BitSet {
    words: Vec<u64>,
}

impl BitSet {
    fn new(n: usize) -> BitSet {
        BitSet {
            words: vec![0; n.div_ceil(64)],
        }
    }
    fn insert(&mut self, i: usize) {
        self.words[i / 64] |= 1 << (i % 64);
    }
    fn remove(&mut self, i: usize) {
        self.words[i / 64] &= !(1 << (i % 64));
    }
    fn union_with(&mut self, other: &BitSet) {
        for (a, b) in self.words.iter_mut().zip(&other.words) {
            *a |= *b;
        }
    }
    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(w, &bits)| {
            (0..64)
                .filter(move |b| bits & (1 << b) != 0)
                .map(move |b| w * 64 + b)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_ast::BinaryOp;
    use noeta_span::Span;

    fn chunk(code: Vec<Op>, num_params: u16, num_registers: u16) -> Chunk {
        Chunk {
            code,
            consts: Vec::new(),
            diagnostics: Vec::new(),
            num_params,
            num_registers,
            defaults: Vec::new(),
            frame_locals: Vec::new(),
            name: None,
            def_span: None,
            debug_locals: Vec::new(),
            line_table: Vec::new(),
        }
    }

    fn load(dst: Reg) -> Op {
        Op::LoadConst { dst, k: 0 }
    }

    /// The core soundness invariant: no two registers that *interfere* in the original code share a
    /// colour after coalescing. (If this holds, a physical slot never holds two live values at once.)
    fn assert_sound(code: &[Op], num_params: u16, n: u16) {
        let n = n as usize;
        let liveness = Liveness::analyze(code, n);
        let interfere = build_interference(code, &liveness, n);
        let colors = color(&interfere, n, num_params as usize);
        for a in 0..n {
            for b in interfere[a].iter() {
                assert_ne!(
                    colors[a], colors[b],
                    "interfering registers r{a} and r{b} got the same colour {}",
                    colors[a]
                );
            }
        }
    }

    #[test]
    fn disjoint_temps_share_one_slot() {
        // r0 dies before r1 is defined — they should coalesce onto a single register.
        let code = vec![
            load(0),
            Op::Echo { reg: 0 },
            load(1),
            Op::Echo { reg: 1 },
            Op::Halt,
        ];
        let mut c = chunk(code, 0, 2);
        coalesce(&mut c);
        assert_eq!(c.num_registers, 1, "disjoint temps should fuse");
        assert!(matches!(c.code[2], Op::LoadConst { dst: 0, .. }));
    }

    #[test]
    fn simultaneously_live_registers_stay_distinct() {
        // r0 and r1 are both read by the `Binary`, so they cannot share a slot; the result r2 also
        // interferes with both of its sources (no intra-op aliasing).
        let code = vec![
            load(0),
            load(1),
            Op::Binary {
                op: BinaryOp::Add,
                dst: 2,
                a: 0,
                b: 1,
                span: Span::new(0, 0),
            },
            Op::Echo { reg: 2 },
            Op::Halt,
        ];
        assert_sound(&code, 0, 3);
        let mut c = chunk(code, 0, 3);
        coalesce(&mut c);
        // Three mutually-interfering live values → three distinct slots.
        assert_eq!(c.num_registers, 3);
    }

    #[test]
    fn parameters_keep_their_registers() {
        // Params 0 and 1 are pinned; a later dead temp may reuse a dead param's slot but the params
        // themselves never move (the calling convention addresses them by index).
        let code = vec![
            Op::Binary {
                op: BinaryOp::Add,
                dst: 2,
                a: 0,
                b: 1,
                span: Span::new(0, 0),
            },
            Op::Return { src: 2 },
        ];
        let mut c = chunk(code, 2, 3);
        coalesce(&mut c);
        // r0 and r1 are still read in place as the operands.
        assert!(matches!(
            c.code[0],
            Op::Binary { a: 0, b: 1, dst, .. } if dst != 0 && dst != 1
        ));
    }

    #[test]
    fn a_value_live_across_a_loop_is_not_clobbered() {
        // `limit` (r0) is defined before the loop and read inside it across the back-edge, so it must
        // stay live for the whole loop — a temp defined inside the loop must not reuse its slot.
        //   0: load r0            (limit)
        //   1: load r1            (loop-internal temp, defined each iteration)
        //   2: JumpIfFalse r1 -> 5
        //   3: Echo r0            (reads limit inside the loop)
        //   4: Jump -> 1          (back-edge)
        //   5: Halt
        let code = vec![
            load(0),
            load(1),
            Op::JumpIfFalse { reg: 1, target: 5 },
            Op::Echo { reg: 0 },
            Op::Jump { target: 1 },
            Op::Halt,
        ];
        assert_sound(&code, 0, 2);
        let liveness = Liveness::analyze(&code, 2);
        let interfere = build_interference(&code, &liveness, 2);
        // r0 is live across the loop body where r1 lives, so they must interfere.
        assert!(
            interfere[0].iter().any(|r| r == 1),
            "limit must interfere with the loop-internal temp"
        );
        let mut c = chunk(code, 0, 2);
        coalesce(&mut c);
        assert_eq!(c.num_registers, 2, "loop-carried value must not be fused");
    }
}
