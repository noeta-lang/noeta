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

use noeta_bytecode::{CaptureFrom, Chunk, Const, JumpPc, Op, Reg, StrPart};

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
    // Every branch destination moves with the code. `for_each_jump_pc_mut` is the one place
    // that knows which ops carry one, so a new branching op cannot silently keep a stale index.
    for op in &mut new_code {
        op.for_each_jump_pc_mut(|t| *t = remap[*t as usize]);
    }
    // The debug line table is pc-keyed, so it moves with the code (empty in a non-debug compile, so
    // this is then a no-op). A *hoisted* load is the one case where an entry must not follow its op:
    // the load moves backward, out of the loop, and `Chunk::line_span` resolves a pc to the last
    // entry at or before it — so an entry dragged into the pre-header would own every pc from there
    // to the statement's real position, and each of them would report that statement's line. This is
    // the forward twin of the backward line a synthetic `Drop` would inject, which lowering already
    // refuses to record for the same reason.
    //
    // It only bites when a statement's *first* op is the hoistable constant, which is why it went
    // unseen while every top-level statement began with a `LoadGlobal`: promoting a top-level
    // binding into a register removes that load, and `i = i + 1` then starts with `LoadConst 1`.
    // So: attach such an entry to the statement's first surviving op instead.
    for entry in &mut chunk.line_table {
        let mut old = entry.pc as usize;
        while old < n && hoisted[old] {
            old += 1;
        }
        entry.pc = if old < n {
            remap[old]
        } else {
            new_code.len() as u32
        };
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

/// Release a **stale global read** before the in-place update that moves the same global out of its
/// slot — a last-use release for the one register class the IR's drop insertion cannot see.
///
/// A top-level binding lives in a global slot, so every read of it materializes a *retained copy*
/// into a fresh register ([`Op::LoadGlobal`]). A function-scope local has no such copy: its register
/// **is** the binding, and an operand reads it in place. That asymmetry is the whole defect this
/// pass fixes. In a self-update statement — `m[k] = m.get_or(k, 0) + 1`, `xs = xs ~ e`, `p.x = p.x +
/// 1` — the lowering emits `TakeGlobal` so the in-place path sees a sole-owned receiver, but the
/// *earlier* read in the same statement has already left a second reference sitting in a register
/// nothing reads again. The refcount is 2, the in-place guard declines, and the update copies the
/// whole collection. Whether that happens is decided by [`coalesce`]: when the two registers happen
/// to be given the same physical slot, the load's release-on-overwrite kills the stale reference and
/// the update stays in place — otherwise it does not, and an O(n) copy runs once per iteration.
/// Register-allocation luck is not a performance model, so this pass makes the release explicit.
///
/// For each `TakeGlobal` of global `g`, it scans **backward through the same basic block** for a
/// `LoadGlobal` of that same `g` whose destination register is
///
/// 1. not redefined between the load and the take (so the slot still holds that read),
/// 2. **dead** at the take (`live_out` does not contain it, and it is not the take's own
///    destination — `TakeGlobal` reads nothing, so those two together are exactly "dead here"), and
/// 3. not a `frame_locals` / `debug_locals` register, whose panic-teardown and debugger contracts
///    own the slot's lifetime,
///
/// and emits `Op::Drop { relevant: false }` for it immediately before the take. The scan stops at
/// any intervening `StoreGlobal`/`TakeGlobal` of the same global, so the stale register is never
/// confused with a re-read of a *different* value that happens to live in the same slot.
///
/// **Why this cannot break genuine aliasing.** The pass releases one reference held by a register
/// that liveness proves dead, and touches nothing else. A real alias — `t = m` — is a `Move` into
/// its own live register, and its reference is untouched, so the receiver's count stays above 1 and
/// the update still copies. `relevant: false` is the same release strength that reclaims this exact
/// register today, when the next iteration's `LoadGlobal` overwrites it (`set_reg` releases with the
/// plain `release`): the pass moves that release earlier within one basic block and changes nothing
/// about which destructors run. Clearing the register to `unit` is idempotent with both
/// release-on-overwrite and frame teardown, so no value is released twice. It is also never the
/// *last* reference: only top-level code emits `StoreGlobal`, so nothing a call in between can do
/// vacates the slot the read came from, and the scan stops at any store this block does itself.
///
/// Runs **after** [`coalesce`], on the physical registers: where coalescing already merged the two,
/// there is no dead register left to find and the pass emits nothing.
pub fn release_stale_global_loads(chunk: &mut Chunk) {
    let n = chunk.code.len();
    let regs = chunk.num_registers as usize;
    if n == 0 || regs == 0 {
        return;
    }
    // Basic-block entries: index 0, any jump target, and the instruction after a non-fall-through
    // op. The backward scan stops at one, which also guarantees a `Drop` is never inserted in front
    // of a branch destination (a jump would otherwise skip it).
    let mut block_start = vec![false; n];
    block_start[0] = true;
    for (i, op) in chunk.code.iter().enumerate() {
        let facts = op_facts(op);
        if !facts.fallthrough && i + 1 < n {
            block_start[i + 1] = true;
        }
        for t in facts.targets {
            if (t as usize) < n {
                block_start[t as usize] = true;
            }
        }
    }
    // Registers whose lifetime belongs to the panic teardown or the debugger — never touched.
    let mut pinned = BitSet::new(regs);
    for &reg in &chunk.frame_locals {
        pinned.insert(reg as usize);
    }
    for local in &chunk.debug_locals {
        pinned.insert(local.reg as usize);
    }

    let liveness = Liveness::analyze(&chunk.code, regs);
    let mut drops: Vec<Vec<Reg>> = vec![Vec::new(); n];
    let mut any = false;
    for (p, at_take) in drops.iter_mut().enumerate() {
        let Op::TakeGlobal { dst, global, .. } = chunk.code[p] else {
            continue;
        };
        let out = &liveness.live_out[p];
        let mut redefined = BitSet::new(regs);
        let mut i = p;
        while i > 0 && !block_start[i] {
            i -= 1;
            match chunk.code[i] {
                // A write to the same slot between the read and the take: everything before it
                // refers to a different value.
                Op::StoreGlobal { global: g, .. } | Op::TakeGlobal { global: g, .. }
                    if g == global =>
                {
                    break;
                }
                Op::LoadGlobal {
                    dst: r, global: g, ..
                } if g == global
                    && r != dst
                    && !redefined.contains(r as usize)
                    && !out.contains(r as usize)
                    && !pinned.contains(r as usize) =>
                {
                    at_take.push(r);
                    any = true;
                }
                _ => {}
            }
            let facts = op_facts(&chunk.code[i]);
            if let Some(d) = facts.def {
                redefined.insert(d as usize);
            }
            if let Some(d) = extra_defs(&chunk.code[i]) {
                redefined.insert(d as usize);
            }
        }
    }
    if !any {
        return;
    }

    // Rebuild with the drops spliced in, remapping every branch destination and line-table pc. A
    // jump target keeps landing on its original op: drops only ever precede an op inside a block.
    let mut new_code: Vec<Op> = Vec::with_capacity(n);
    let mut remap = vec![0u32; n];
    for (i, op) in chunk.code.iter().enumerate() {
        for &reg in &drops[i] {
            new_code.push(Op::Drop {
                reg,
                relevant: false,
            });
        }
        remap[i] = new_code.len() as u32;
        new_code.push(op.clone());
    }
    for op in &mut new_code {
        op.for_each_jump_pc_mut(|t| *t = remap[*t as usize]);
    }
    for entry in &mut chunk.line_table {
        entry.pc = remap[entry.pc as usize];
    }
    chunk.code = new_code;
}

/// The registers an op **reads** (`uses`) and the single register it fully **overwrites** (`def`),
/// plus its control-flow successors. ANF makes every non-parameter register write-once except for
/// reassigned `mut` locals, which simply contribute several defs to the one register's range.
struct OpFacts {
    def: Option<Reg>,
    uses: Vec<Reg>,
    /// Explicit jump targets (instruction indices), filled from
    /// [`Op::for_each_jump_pc`] rather than per-arm.
    targets: Vec<JumpPc>,
    /// Whether control can fall through to the next instruction.
    fallthrough: bool,
}

/// Enumerate an op's register reads/writes and control flow. Exhaustive by design — there is no
/// `_` arm, so adding an `Op` variant forces this to be revisited (a missed register would be a
/// silent use-after-free). The arms model registers and fall-through only; `targets` is filled
/// afterwards from [`Op::for_each_jump_pc`], the one answer to which op branches where.
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
        // Writes the value's reflected type tag beside the payload — the register keeps holding the
        // very same value, so this is a pure USE (no `def`): the object must still be live here, and
        // it stays live for whoever consumes it afterwards.
        Op::Retag { reg, .. } => f.uses.push(*reg),
        // The dynamic twin (generic-in-generic construction) additionally READS the hidden
        // type-argument slot register that names the tag. Both are pure uses: the object keeps
        // holding the same value, and the slot local is the enclosing member's parameter, live for
        // its whole body — but it must be recorded, or the slot's register could be reused for
        // something else between the call and this stamp.
        Op::RetagDynamic { reg, slot } => {
            f.uses.push(*reg);
            f.uses.push(*slot);
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
            dst,
            recv,
            args,
            type_args,
            ..
        } => {
            f.def = Some(*dst);
            f.uses.push(*recv);
            f.uses.extend(args.iter().copied());
            f.uses.extend(type_args.regs().iter().copied());
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
        Op::Coalesce { dst, src, .. } => {
            f.def = Some(*dst);
            f.uses.push(*src);
        }
        // A narrow's `dynamic` head-name register is a use like any other — it holds the
        // instantiation's name, produced by a preceding `TypeArgName`/`TypeSlotName`. Missing it
        // here let coalescing reuse that register's slot and shrink the frame under it.
        Op::Narrow {
            dst, src, dynamic, ..
        }
        | Op::IsType {
            dst, src, dynamic, ..
        } => {
            f.def = Some(*dst);
            f.uses.push(*src);
            f.uses.extend(dynamic.iter().copied());
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
        // `roles_of()`'s scope is optional; `attributes_of`'s name register never is.
        Op::RolesOf { dst, src } => {
            f.def = Some(*dst);
            f.uses.extend(src.iter().copied());
        }
        Op::ParamsOf { dst, src } | Op::ReturnsOf { dst, src } => {
            f.def = Some(*dst);
            f.uses.push(*src);
        }
        Op::AttributesOf { dst, src }
        | Op::TypeOf { dst, src }
        | Op::FieldsOf { dst, src, .. }
        | Op::TraitsOf { dst, src }
        | Op::FieldSpecsOf { dst, src }
        | Op::VariantsOf { dst, src } => {
            f.def = Some(*dst);
            f.uses.push(*src);
        }
        Op::FromBytes { dst, src, .. }
        | Op::TypeArgName { dst, src, .. }
        | Op::TypeSlotName { dst, src, .. }
        | Op::SelfRenderSlot { dst, src, .. } => {
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
        Op::MatchInt { src, .. } => f.uses.push(*src),
        Op::MatchStr { src, .. } => f.uses.push(*src),
        Op::MatchBool { src, .. } => f.uses.push(*src),
        Op::MatchVariant { src, .. } => f.uses.push(*src),
        Op::MatchTuple { src, .. } => f.uses.push(*src),
        Op::ExtractField { dst, src, .. } => {
            f.def = Some(*dst);
            f.uses.push(*src);
        }
        Op::MatchFail { src, .. } => {
            f.uses.push(*src);
            f.fallthrough = false; // aborts the program
        }
        // A forwarding call's `type_args` are operand registers exactly like `args` — read at the
        // call, live until it. Omitting them here would let a type-argument register be coloured
        // over a live value and the callee would read a type-table index that is no longer there.
        Op::Call {
            dst,
            callee,
            args,
            type_args,
            ..
        } => {
            f.def = Some(*dst);
            f.uses.push(*callee);
            f.uses.extend(args.iter().copied());
            f.uses.extend(type_args.regs().iter().copied());
        }
        Op::CallGlobal {
            dst,
            args,
            type_args,
            ..
        } => {
            f.def = Some(*dst);
            f.uses.extend(args.iter().copied());
            f.uses.extend(type_args.regs().iter().copied());
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
        Op::Jump { .. } => f.fallthrough = false,
        Op::JumpIfTrue { reg, .. } | Op::JumpIfFalse { reg, .. } | Op::CondBranch { reg, .. } => {
            f.uses.push(*reg)
        }
        Op::Echo { reg } => f.uses.push(*reg),
        // A hinted display/JSON door in a generic body also READS the frame's render slots — the
        // registers its hint operand names. Missing them here would let the liveness walk call a
        // slot dead at the door and hand its register to another local.
        Op::Stringify { dst, src, hint, .. } => {
            f.def = Some(*dst);
            f.uses.push(*src);
            if let Some(hint) = hint {
                f.uses.extend(hint.slots.iter().copied());
            }
        }
        Op::JsonStringify { dst, src, hint } => {
            f.def = Some(*dst);
            f.uses.push(*src);
            f.uses.extend(hint.slots.iter().copied());
        }
        // The ordering door's twin: it reads the render slots and defines nothing.
        Op::ResolveHint { slots, .. } => f.uses.extend(slots.iter().copied()),
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
    // Explicit successors come from the one place that knows which ops carry a branch destination,
    // so the arms above only have to model *registers*. A target missed here would under-approximate
    // liveness and let two simultaneously-live values coalesce onto one slot.
    op.for_each_jump_pc(|t| f.targets.push(t));
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
        Op::Retag { reg, .. } => m(reg),
        Op::RetagDynamic { reg, slot } => {
            m(reg);
            m(slot);
        }
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
            dst,
            recv,
            args,
            type_args,
            ..
        } => {
            m(dst);
            m(recv);
            for r in args.iter_mut() {
                m(r);
            }
            for r in type_args.regs_mut().iter_mut() {
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
        // The `dynamic` head-name register is remapped with the rest — see the use-set arm above.
        Op::Narrow {
            dst, src, dynamic, ..
        }
        | Op::IsType {
            dst, src, dynamic, ..
        } => {
            m(dst);
            m(src);
            if let Some(reg) = dynamic {
                m(reg);
            }
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
        Op::RolesOf { dst, src } => {
            m(dst);
            if let Some(src) = src {
                m(src);
            }
        }
        Op::ParamsOf { dst, src } | Op::ReturnsOf { dst, src } => {
            m(dst);
            m(src);
        }
        Op::AttributesOf { dst, src }
        | Op::TypeOf { dst, src }
        | Op::FieldsOf { dst, src, .. }
        | Op::TraitsOf { dst, src }
        | Op::FieldSpecsOf { dst, src }
        | Op::VariantsOf { dst, src } => {
            m(dst);
            m(src);
        }
        Op::FromBytes { dst, src, .. }
        | Op::TypeArgName { dst, src, .. }
        | Op::TypeSlotName { dst, src, .. }
        | Op::SelfRenderSlot { dst, src, .. } => {
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
            dst,
            callee,
            args,
            type_args,
            ..
        } => {
            m(dst);
            m(callee);
            for r in args.iter_mut() {
                m(r);
            }
            for r in type_args.regs_mut().iter_mut() {
                m(r);
            }
        }
        Op::CallGlobal {
            dst,
            args,
            type_args,
            ..
        } => {
            m(dst);
            for r in args.iter_mut() {
                m(r);
            }
            for r in type_args.regs_mut().iter_mut() {
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
        Op::Stringify { dst, src, hint, .. } => {
            m(dst);
            m(src);
            if let Some(hint) = hint {
                for r in hint.slots.iter_mut() {
                    m(r);
                }
            }
        }
        Op::JsonStringify { dst, src, hint } => {
            m(dst);
            m(src);
            for r in hint.slots.iter_mut() {
                m(r);
            }
        }
        Op::ResolveHint { slots, .. } => {
            for r in slots.iter_mut() {
                m(r);
            }
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
    fn contains(&self, i: usize) -> bool {
        self.words[i / 64] & (1 << (i % 64)) != 0
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
            hidden: 0,
            hidden_base: 0,
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

    // ── release_stale_global_loads ─────────────────────────────────────────────────────────────

    fn gload(dst: Reg, g: u32) -> Op {
        Op::LoadGlobal {
            dst,
            global: noeta_bytecode::GlobalId(g),
            span: Span::new(0, 0),
        }
    }

    fn gtake(dst: Reg, g: u32) -> Op {
        Op::TakeGlobal {
            dst,
            global: noeta_bytecode::GlobalId(g),
            span: Span::new(0, 0),
        }
    }

    /// The pass declined: the code kept its original length and no `Drop` was spliced in.
    fn assert_untouched(c: &Chunk, len: usize) {
        assert_eq!(c.code.len(), len, "no op should have been inserted");
        assert!(
            !c.code.iter().any(|op| matches!(op, Op::Drop { .. })),
            "no drop should have been inserted"
        );
    }

    fn gstore(src: Reg, g: u32) -> Op {
        Op::StoreGlobal {
            global: noeta_bytecode::GlobalId(g),
            src,
        }
    }

    /// The defect's exact shape: a read of the global sits dead in r1 while `TakeGlobal` moves the
    /// same global into r2, so the in-place update would see refcount 2 and copy. The pass releases
    /// r1 immediately before the take.
    #[test]
    fn stale_global_read_is_released_before_the_take() {
        let code = vec![
            gload(1, 0),         // 0: the read whose reference goes stale
            Op::Echo { reg: 1 }, // 1: its last use
            gtake(2, 0),         // 2: the in-place update's receiver
            gstore(2, 0),        // 3
            Op::Halt,
        ];
        let mut c = chunk(code, 0, 3);
        release_stale_global_loads(&mut c);
        assert!(
            matches!(
                c.code[2],
                Op::Drop {
                    reg: 1,
                    relevant: false
                }
            ),
            "expected a drop of r1 before the take, got {:?}",
            c.code
        );
        assert!(matches!(c.code[3], Op::TakeGlobal { dst: 2, .. }));
    }

    /// Coalescing already gave the read and the take one slot (the "lucky" spelling): the read's
    /// reference dies on the overwrite, there is nothing dead to release, and the pass adds nothing.
    #[test]
    fn a_coalesced_read_needs_no_drop() {
        let code = vec![
            gload(1, 0),
            Op::Echo { reg: 1 },
            gtake(1, 0),
            gstore(1, 0),
            Op::Halt,
        ];
        let mut c = chunk(code, 0, 2);
        release_stale_global_loads(&mut c);
        assert_untouched(&c, 5);
    }

    /// A genuine alias: the read is still live after the take (something reads it later), so its
    /// reference is real and the receiver must keep its refcount above 1 — no drop.
    #[test]
    fn a_live_read_is_never_released() {
        let code = vec![
            gload(1, 0),
            gtake(2, 0),
            gstore(2, 0),
            Op::Echo { reg: 1 }, // r1 is read *after* the update: a real second reference
            Op::Halt,
        ];
        let mut c = chunk(code, 0, 3);
        release_stale_global_loads(&mut c);
        assert_untouched(&c, 5);
    }

    /// A read of a *different* global holds a reference to a different value — releasing it would
    /// not help the receiver and could free something still owned elsewhere.
    #[test]
    fn a_read_of_another_global_is_left_alone() {
        let code = vec![
            gload(1, 1),
            Op::Echo { reg: 1 },
            gtake(2, 0),
            gstore(2, 0),
            Op::Halt,
        ];
        let mut c = chunk(code, 0, 3);
        release_stale_global_loads(&mut c);
        assert_untouched(&c, 5);
    }

    /// A store to the same global between the read and the take rebinds the slot: the register now
    /// holds a *different* value from the one the take moves out, so the scan stops there.
    #[test]
    fn an_intervening_store_stops_the_scan() {
        let code = vec![
            gload(1, 0),
            Op::Echo { reg: 1 },
            gstore(2, 0),
            gtake(3, 0),
            gstore(3, 0),
            Op::Halt,
        ];
        let mut c = chunk(code, 0, 4);
        release_stale_global_loads(&mut c);
        assert_untouched(&c, 6);
    }

    /// The register was rewritten after the read, so it no longer holds that read's value.
    #[test]
    fn a_redefined_register_is_not_the_stale_read() {
        let code = vec![
            gload(1, 0),
            Op::Echo { reg: 1 },
            load(1), // r1 now holds a constant, not the global
            gtake(2, 0),
            gstore(2, 0),
            Op::Halt,
        ];
        let mut c = chunk(code, 0, 3);
        release_stale_global_loads(&mut c);
        assert_untouched(&c, 6);
    }

    /// The scan is block-local: a read in a *predecessor* block may or may not have executed on the
    /// path that reaches the take, so it is never released.
    #[test]
    fn the_scan_does_not_cross_a_block_boundary() {
        let code = vec![
            gload(1, 0),
            Op::JumpIfFalse { reg: 1, target: 3 },
            Op::Halt,
            gtake(2, 0), // a jump target: its block starts here
            gstore(2, 0),
            Op::Halt,
        ];
        let mut c = chunk(code, 0, 3);
        release_stale_global_loads(&mut c);
        assert_untouched(&c, 6);
    }

    /// A register on the panic-teardown list owns its slot's lifetime — the teardown fires it, so the
    /// pass leaves it to do that.
    #[test]
    fn a_teardown_register_is_left_alone() {
        let code = vec![
            gload(1, 0),
            Op::Echo { reg: 1 },
            gtake(2, 0),
            gstore(2, 0),
            Op::Halt,
        ];
        let mut c = chunk(code, 0, 3);
        c.frame_locals = vec![1];
        release_stale_global_loads(&mut c);
        assert_untouched(&c, 5);
    }

    /// Splicing a drop in shifts every later instruction, so branch destinations and the line table
    /// have to move with them — a stale target would jump into the middle of the statement.
    #[test]
    fn inserting_a_drop_remaps_jump_targets() {
        // 0: JumpIfFalse r0 -> 5 ; 1..4 the loop body with the stale read ; 5: Halt
        let code = vec![
            Op::JumpIfFalse { reg: 0, target: 5 },
            gload(1, 0),
            Op::Echo { reg: 1 },
            gtake(2, 0),
            gstore(2, 0),
            Op::Halt,
        ];
        let mut c = chunk(code, 0, 3);
        c.line_table = vec![noeta_bytecode::LineEntry {
            pc: 5,
            span: Span::new(0, 0),
        }];
        release_stale_global_loads(&mut c);
        assert_eq!(c.code.len(), 7, "one drop spliced in");
        assert!(matches!(c.code[0], Op::JumpIfFalse { target: 6, .. }));
        assert!(matches!(c.code[6], Op::Halt));
        assert_eq!(c.line_table[0].pc, 6, "the line table moves with the code");
    }

    /// A line entry is the one thing that must **not** follow a hoisted load out of its loop.
    /// `Chunk::line_span` resolves a pc to the last entry at or before it, so an entry parked in
    /// the pre-header owns every pc from there to its statement's real position — every op in
    /// between then reports that statement's line.
    ///
    /// The shape below is `while … { <stmt A>; <stmt B> }` where B's first op is the hoistable
    /// constant: exactly what a top-level `i = i + 1` lowers to once its binding lives in a
    /// register rather than the global table.
    #[test]
    fn a_hoisted_load_does_not_drag_its_line_entry_out_of_the_loop() {
        // 0: LoadConst (loop bound) ; 1: Binary (cond) ; 2: JumpIfFalse -> 7 ; 3: stmt A's op
        // 4: stmt B's LoadConst (hoistable) ; 5: stmt B's Binary ; 6: Jump -> 1 ; 7: Halt
        let code = vec![
            Op::LoadConst { dst: 0, k: 0 },
            Op::Binary {
                dst: 1,
                op: BinaryOp::Lt,
                a: 0,
                b: 0,
                span: Span::new(0, 0),
            },
            Op::JumpIfFalse { reg: 1, target: 7 },
            Op::Echo { reg: 0 },
            Op::LoadConst { dst: 2, k: 0 },
            Op::Binary {
                dst: 3,
                op: BinaryOp::Add,
                a: 2,
                b: 2,
                span: Span::new(1, 1),
            },
            Op::Jump { target: 1 },
            Op::Halt,
        ];
        let mut c = chunk(code, 0, 4);
        c.consts = vec![Const::Int(1)];
        // Entry for statement A at pc 3, and for statement B at pc 4 — B's first op is the
        // constant the hoister will lift into the pre-header.
        c.line_table = vec![
            noeta_bytecode::LineEntry {
                pc: 3,
                span: Span::new(0, 0),
            },
            noeta_bytecode::LineEntry {
                pc: 4,
                span: Span::new(1, 1),
            },
        ];
        hoist_loop_invariant_consts(&mut c);
        let a = c.line_table[0].pc;
        let b = c.line_table[1].pc;
        assert!(
            b > a,
            "statement B's entry must stay after statement A's, not move ahead of it into the \
             pre-header (A at {a}, B at {b})"
        );
        assert!(
            matches!(c.code[b as usize], Op::Binary { .. }),
            "B's entry lands on its first surviving op, not on the hoisted load"
        );
    }
}
