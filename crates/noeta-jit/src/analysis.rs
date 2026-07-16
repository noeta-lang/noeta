//! The **pure bytecode analyses** the tier-1 codegen consults (audit-1 finding 11) — the same
//! kind of standalone, independently-testable analysis `plan.rs` already holds for register
//! residency. Everything here is a pure function of a `Chunk`: the bare-store may-hold-heap
//! dataflow (`heap_in_map` and its `RegEffect` op model), the move-then-drop `transfer_pairs`
//! scan, the slot-hazard and must-slot-written maps, the per-pc `Kind` lattice (`kind_in_map`),
//! and the `fast_ok` fast-body gate. Moved verbatim from `lib.rs` — no behavior change; codegen
//! output is byte-identical.

use noeta_ast::BinaryOp;
use noeta_bytecode::{Const, Op, Reg};
use noeta_value::Value;

use crate::{const_immediate_bits, supported_binary};

/// Whether a `Binary`'s result — as produced by *native code* — is always an immediate. Used by the
/// bare-store analysis to decide whether a `Binary`'s destination may hold a heap value afterwards.
///
/// A comparison (`==`/`<`/…) or short-circuit `&&`/`||` yields a bool. **Arithmetic yields an
/// immediate too, in the JIT:** the native `Binary` guards the 48-bit range and *bails to the
/// interpreter before storing* on an overflowing (would-be heap-boxed) integer result, and the float
/// path is NaN-boxed — so a register holding a *completed* native arithmetic result is provably
/// immediate at every point the interpreter can re-enter (the boxing case already left native code).
/// `~` (`Concat`) and other heap-building ops stay may-heap.
pub(crate) fn binary_result_is_immediate(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::Identity
            | BinaryOp::NotIdentity
            | BinaryOp::Lt
            | BinaryOp::Le
            | BinaryOp::Gt
            | BinaryOp::Ge
            | BinaryOp::And
            | BinaryOp::Or
            | BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::Mul
            | BinaryOp::Div
            | BinaryOp::Rem
            | BinaryOp::BitAnd
            | BinaryOp::BitOr
            | BinaryOp::BitXor
            | BinaryOp::Shl
            | BinaryOp::Shr
    )
}

/// One op's effect on the per-register "may hold a heap value" set (the bare-store analysis,
/// [`heap_in_map`]). `None` means the op is *not modeled* — its effect on registers is unknown to
/// this analysis — which opts the whole prototype out (the analysis fails **closed**: every store
/// keeps its refcount-correct release). Only the ops that appear in a pure-arithmetic loop are
/// modeled; a call, a leaf/heap op, a closure, an index — anything richer — returns `None`, so the
/// optimization is confined to exactly the prototypes where it was measured to help and cannot silently
/// mis-model a heap value.
pub(crate) enum RegEffect {
    /// Reads/writes no register the analysis tracks (a branch, `Echo`, `Return`, `Halt`).
    Inert,
    /// The op leaves `reg` holding `unit` (a `Drop`, or a `StoreGlobal` that moves its source out).
    Clear(Reg),
    /// `dst` is (re)defined; `heap` = whether its new value may be a heap pointer.
    Def { dst: Reg, heap: bool },
    /// `dst = src` — `dst` inherits `src`'s heap-ness (a `Move`).
    Copy { dst: Reg, src: Reg },
}

/// Model one op's effect on the may-hold-heap set, or `None` if the op is unmodeled (see
/// [`RegEffect`]). The whitelist is deliberately the pure-arithmetic-loop subset.
pub(crate) fn reg_effect(op: &Op, consts: &[Const]) -> Option<RegEffect> {
    Some(match op {
        Op::LoadConst { dst, k } => RegEffect::Def {
            dst: *dst,
            heap: const_immediate_bits(&consts[*k as usize]).is_none(),
        },
        Op::Move { dst, src } => RegEffect::Copy {
            dst: *dst,
            src: *src,
        },
        Op::Drop { reg, .. } => RegEffect::Clear(*reg),
        Op::StoreGlobal { src, .. } => RegEffect::Clear(*src),
        Op::LoadGlobal { dst, .. } | Op::TakeGlobal { dst, .. } => RegEffect::Def {
            dst: *dst,
            heap: true,
        },
        Op::Binary { op, dst, .. } => RegEffect::Def {
            dst: *dst,
            heap: !binary_result_is_immediate(*op),
        },
        // S1 (Tier W): same contract as `Binary` — a natively-stored result is immediate (every
        // bail — non-int operand, zero divisor, out-of-immediate-range result — precedes the
        // write); mid-frame re-entry verification covers the tier-0-boxed case, as for `Add`.
        Op::WideInt { dst, .. } | Op::MaskWidth { dst, .. } => RegEffect::Def {
            dst: *dst,
            heap: false,
        },
        Op::Unary { dst, .. } | Op::Stringify { dst, .. } => RegEffect::Def {
            dst: *dst,
            heap: true,
        },
        // A call redefines only its destination (the result, which may be a heap value) and reads its
        // args; from the *caller's* register view nothing else changes. Modeling it — rather than
        // failing the whole map closed — lets a call-bearing prototype (every recursive/helper fn)
        // bare-store its provably-immediate arithmetic temps. The interpreter's re-entry after a call
        // that bails resumes at `pc + 1` with exactly this state (result in `dst`, other caller
        // registers preserved), so the forward dataflow's out-set there over-approximates it.
        Op::Call { dst, .. } | Op::CallGlobal { dst, .. } => RegEffect::Def {
            dst: *dst,
            heap: true,
        },
        Op::Jump { .. }
        | Op::JumpIfTrue { .. }
        | Op::JumpIfFalse { .. }
        | Op::CondBranch { .. }
        | Op::RequireBool { .. }
        | Op::RequireCondBool { .. }
        | Op::Echo { .. }
        | Op::Return { .. }
        | Op::Halt => RegEffect::Inert,
        _ => return None,
    })
}

/// The analysis CFG successors of `pc` under *tier-0* (interpreter) semantics — the paths along which a
/// register's value flows. Every op falls through to `pc + 1` except a jump (to its target), a
/// conditional branch (both), and the terminators `Return`/`Halt` (none). Modeled only for the
/// whitelisted ops ([`reg_effect`]); the caller has already rejected any other op.
pub(crate) fn analysis_succ(op: &Op, pc: usize, n: usize, out: &mut Vec<usize>) {
    out.clear();
    match op {
        Op::Jump { target } => out.push(*target as usize),
        Op::JumpIfTrue { target, .. }
        | Op::JumpIfFalse { target, .. }
        | Op::CondBranch { target, .. } => {
            out.push(*target as usize);
            if pc + 1 < n {
                out.push(pc + 1);
            }
        }
        Op::Return { .. } | Op::Halt => {}
        _ => {
            if pc + 1 < n {
                out.push(pc + 1);
            }
        }
    }
}

/// The bare-store release map for a prototype: `map[pc * nreg + r]` is whether a store to register `r`
/// at `pc` must release the value it overwrites. A `false` cell means the old occupant is provably an
/// immediate, so the store can skip the load-old + `is_pointer` release (the bare-store optimization).
///
/// - A non-`heap_aware` prototype already stores bare everywhere (the immediate invariant) → all-false.
/// - A `heap_aware` prototype the forward analysis can model → the precise may-hold-heap set at each
///   store site (release iff the overwritten value may be a heap pointer).
/// - A `heap_aware` prototype with any unmodeled op → all-true (release everywhere, the prior behavior).
///
/// **Soundness.** The analysis is a monotone forward may-hold-heap dataflow over the tier-0 CFG
/// (join = union), seeded all-immediate at entry (`pc 0`): locals are `unit`-initialized and the
/// parameters' immediate claim is **runtime-verified** — the pc-0 entry guard bails on a heap
/// argument before the body runs, and every mid-frame entry re-verifies the claims
/// ([`Codegen::guard_entry_claims`]), the same contract that covers the natively-stored-result
/// claims. A `false` cell is thus a *guarantee* the register holds an immediate wherever native
/// code executes, so skipping the release is a no-op — the interpreter's `set_reg` would release
/// an immediate, which is itself a no-op. Every modeled op's [`RegEffect`] only clears a bit
/// (`Drop`/`StoreGlobal` leave `unit`), copies one (`Move`), or sets one to an over-approximation
/// of its result's heap-ness; an unmodeled op fails the whole map closed.
pub(crate) fn heap_in_map(chunk: &noeta_bytecode::Chunk, heap_aware: bool) -> Vec<bool> {
    let n = chunk.code.len();
    let nreg = chunk.num_registers as usize;
    if !heap_aware {
        return vec![false; n * nreg];
    }
    match heap_at_fixpoint(chunk, n, nreg) {
        Some(inset) => inset,
        None => vec![true; n * nreg],
    }
}

/// Whether the register-effect model covers this prototype (P-JSSA S5's residency permission):
/// either every op is modeled by [`reg_effect`], or the prototype is call-free/non-OSR
/// (`!heap_aware`), where the pc-0 entry guard's all-immediate invariant holds without any
/// modeling — every register write is then refcount-free by construction, and unmodeled ops are
/// plain bail points whose sync uses the (always-modeled) liveness.
pub(crate) fn proto_modeled(chunk: &noeta_bytecode::Chunk, heap_aware: bool) -> bool {
    !heap_aware
        || chunk
            .code
            .iter()
            .all(|op| reg_effect(op, &chunk.consts).is_some())
}

/// The forward **slot-hazard** fixpoint (P-JSSA S5): `map[pc * nreg + r]` = may register `r`'s
/// window slot be **out of sync with its variable in a heap-relevant way** at the start of `pc`?
/// With heap values SSA-resident, a def releases the old value *from the variable* and writes no
/// slot — so a slot can hold a released (dangling) pointer, or fail to hold the heap reference
/// the register owns. Either way the slot must be re-synced before anything that reads or
/// releases it (teardown, unwind, the interpreter). The spill set at every sync point is
/// therefore `live ∪ hazard`.
///
/// Transfer: a def/clear of `r` raises the hazard if the *old* value may be heap
/// (`heap_in[pc][r]` — its release/move leaves released bits in the slot) or the *new* value may
/// be heap (`heap_in[pc+1][r]` — the variable now owns a reference the slot doesn't hold). A
/// call clears everything (the pre-call sync spilled `live ∪ hazard`, and immediates are
/// stale-safe) and then raises its own destination (the fast path writes the result to the
/// variable only). Entries seed in-sync (variables are loaded from the slots). Join = OR.
pub(crate) fn slot_hazard_map(chunk: &noeta_bytecode::Chunk, heap_in: &[bool]) -> Vec<bool> {
    let n = chunk.code.len();
    let nreg = chunk.num_registers as usize;
    let all_immediate = heap_in.iter().all(|&h| !h);
    if all_immediate {
        return vec![false; n * nreg]; // stale slots are stale-immediates — always safe
    }
    let heap_at = |pc: usize, r: usize| -> bool {
        let i = pc * nreg + r;
        i < heap_in.len() && heap_in[i]
    };
    let mut hazard = vec![false; n * nreg];
    let mut succ = Vec::new();
    let mut changed = true;
    while changed {
        changed = false;
        for pc in 0..n {
            let mut out: Vec<bool> = hazard[pc * nreg..pc * nreg + nreg].to_vec();
            match reg_effect(&chunk.code[pc], &chunk.consts) {
                Some(RegEffect::Def { dst, .. }) | Some(RegEffect::Copy { dst, .. }) => {
                    let d = dst as usize;
                    if matches!(chunk.code[pc], Op::Call { .. } | Op::CallGlobal { .. }) {
                        out.fill(false); // the pre-call sync spilled live ∪ hazard
                        out[d] = true; // the result lands in the variable only (fast path)
                    } else {
                        out[d] = out[d] || heap_at(pc, d) || heap_at(pc + 1, d);
                    }
                }
                Some(RegEffect::Clear(r)) => {
                    let d = r as usize;
                    out[d] = out[d] || heap_at(pc, d);
                }
                Some(RegEffect::Inert) => {}
                None => return vec![true; n * nreg], // unmodeled → no residency anyway
            }
            analysis_succ(&chunk.code[pc], pc, n, &mut succ);
            for &su in &succ {
                let base = su * nreg;
                for (r, &o) in out.iter().enumerate() {
                    if o && !hazard[base + r] {
                        hazard[base + r] = true;
                        changed = true;
                    }
                }
            }
        }
    }
    hazard
}

/// The ownership-transfer peephole map for a prototype (see [`Codegen::transfer`]): pc `i` is marked
/// when a `Move dst <- src` at `i` is immediately followed by a `Drop src` at `i + 1` — an ownership
/// transfer whose retain/release pair cancels. Both `i` and `i + 1` are marked. The `Drop` must be
/// reachable *only* through the `Move` (no branch targets `i + 1`), so that on every path reaching the
/// `Drop` the `Move` ran first and the pairing holds; a `Drop` that is a jump target is left alone.
pub(crate) fn transfer_pairs(chunk: &noeta_bytecode::Chunk) -> Vec<bool> {
    let n = chunk.code.len();
    let mut is_target = vec![false; n + 1];
    for op in &chunk.code {
        match op {
            Op::Jump { target }
            | Op::JumpIfTrue { target, .. }
            | Op::JumpIfFalse { target, .. }
            | Op::CondBranch { target, .. } => is_target[*target as usize] = true,
            _ => {}
        }
    }
    let mut transfer = vec![false; n];
    for pc in 0..n {
        let Op::Move { src, .. } = chunk.code[pc] else {
            continue;
        };
        if pc + 1 < n
            && !is_target[pc + 1]
            && matches!(chunk.code[pc + 1], Op::Drop { reg, .. } if reg == src)
        {
            transfer[pc] = true;
            transfer[pc + 1] = true;
        }
    }
    transfer
}

/// The forward may-hold-heap fixpoint used by [`heap_in_map`], or `None` if any op is unmodeled.
/// Returns the in-set flattened as `[pc * nreg + r]`: whether register `r` may hold a heap value at the
/// *start* of op `pc` — which is exactly whether a store to `r` at `pc` overwrites a possibly-heap
/// value.
pub(crate) fn heap_at_fixpoint(
    chunk: &noeta_bytecode::Chunk,
    n: usize,
    nreg: usize,
) -> Option<Vec<bool>> {
    let effects: Vec<RegEffect> = chunk
        .code
        .iter()
        .map(|op| reg_effect(op, &chunk.consts))
        .collect::<Option<_>>()?;

    // in[pc * nreg + r]: may register `r` hold a heap value at the start of op `pc`. Seed:
    // all-immediate — locals are `unit`-initialized, and the parameters are **claimed**
    // immediate too (T1b): the pc-0 entry guard has always bailed on a heap argument before the
    // body runs, and every mid-frame entry verifies the claim (below), so a parameter register
    // is promotable — and typed-readable — like any other, until an op redefines it as may-heap.
    let mut inset = vec![false; n * nreg];
    // NOTE (soundness, P-JSSA): this forward model describes tier-0's *fall-through* state, but a
    // mid-frame native entry (a seam resume after an interpreted callee, or an OSR loop header)
    // begins with whatever tier 0 actually left in the slots — and tier 0 can put a heap value
    // where this model claims an immediate (a heap argument reaches the body when tier 0 runs the
    // frame; an overflowing arithmetic result heap-boxes to a big int, where native bails
    // *before* such a store — so the discrepancy arises exactly when tier 0 ran the segment). A
    // false immediate claim would skip a needed retain/release — a leak, or a double-release.
    // Rather than dropping the claims (which would forfeit post-call bare stores and the loop
    // promotion), every native entry **verifies** them at runtime and bails on a violation — the
    // pc-0 parameter guard for a fresh frame, `Codegen::guard_entry_claims` for every mid-frame
    // entry. Native→native direct calls never pass through a mid-frame entry (the callee provably
    // ran fully native), so the hot path pays nothing.

    let mut succ = Vec::new();
    let mut changed = true;
    while changed {
        changed = false;
        for pc in 0..n {
            // out = transfer(in[pc], effects[pc]).
            let mut out = inset[pc * nreg..pc * nreg + nreg].to_vec();
            // P-JCT C3 guard strengthening: a supported `Binary`'s native continuation proved
            // **both** operands immediate — every emitted path (typed, asymmetric-guarded,
            // generic dispatch) bails unless both operands are small ints or both floats — and a
            // `CondBranch`'s continuation proved its scrutinee a bool (a non-bool bails for
            // E0007). Tier-0 divergence is caught by the mid-frame entry verification, exactly
            // the contract that lets the entry row seed all-immediate. A slot left stale-heap by
            // an *earlier* def stays tracked: [`slot_hazard_map`] raises hazards at def sites
            // and never lowers them (only a call's full sync clears).
            match &chunk.code[pc] {
                Op::Binary { op, a, b, .. } if supported_binary(*op) => {
                    out[*a as usize] = false;
                    out[*b as usize] = false;
                }
                Op::CondBranch { reg, .. } => out[*reg as usize] = false,
                _ => {}
            }
            match effects[pc] {
                RegEffect::Inert => {}
                RegEffect::Clear(r) => out[r as usize] = false,
                RegEffect::Def { dst, heap } => out[dst as usize] = heap,
                RegEffect::Copy { dst, src } => out[dst as usize] = out[src as usize],
            }
            // Propagate out into each successor's in-set (join = OR).
            analysis_succ(&chunk.code[pc], pc, n, &mut succ);
            for &s in &succ {
                let base = s * nreg;
                for (r, &o) in out.iter().enumerate() {
                    if o && !inset[base + r] {
                        inset[base + r] = true;
                        changed = true;
                    }
                }
            }
        }
    }
    Some(inset)
}

/// A register's statically-known **immediate kind** (P-JSSA T1 typed promotion). Where the kind
/// analysis ([`kind_in_map`]) proves a register `Int`/`Bool`/`Float` at a pc, the codegen skips
/// the NaN-box tag checks and (for `Int`/`Bool`) works on a second, **raw** SSA variable holding
/// the unboxed value — the box/unbox chain the egraph cannot fold through loop-header block
/// params disappears from the loop body entirely.
///
/// The lattice: `Bot < {Int, Bool, Float} < Imm`, join = equality-or-`Imm`. A typed kind is a
/// *claim* with the same contract as the bare-store analysis's immediate claims: true along
/// native paths by construction (a native int `Binary` bails before storing a non-fitting
/// result), and **verified at every mid-frame entry** against tier-0's actual slots
/// ([`Codegen::guard_entry_claims`]) — tier 0 can heap-box an overflow where the claim says
/// `Int`, and the guard catches exactly that (subsuming the plain `is_pointer` check).
///
/// Invariant (locked by a test): a typed kind implies the bare-store analysis proves the
/// register immediate there (`kind ∈ {Int, Bool, Float}` ⇒ `!heap_in`) — both transfers mark
/// the same may-heap defs, so a kind claim never outlives its immediate claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// No analyzed path has defined the register yet (an unreached cell) — the join identity.
    Bot,
    /// A small int (48-bit immediate). Raw form: the sign-extended i64.
    Int,
    /// A bool. Raw form: 0 or 1 in an i64.
    Bool,
    /// An f64. No raw form — a NaN-boxed float's word *is* its bit pattern; the win is skipping
    /// the type dispatch, not the (identity) unboxing.
    Float,
    /// An immediate of statically-unknown kind (or a cell outside the promoted region).
    Imm,
}

impl Kind {
    fn join(self, other: Kind) -> Kind {
        match (self, other) {
            (Kind::Bot, k) | (k, Kind::Bot) => k,
            (a, b) if a == b => a,
            _ => Kind::Imm,
        }
    }
}

/// The kind a `LoadConst` of this constant leaves in its destination. Heap constants (strings,
/// big ints, native modules) are `Imm` — irrelevant, since their defs are also may-heap in the
/// bare-store analysis, so no typed claim survives past them.
pub(crate) fn const_kind(c: &Const) -> Kind {
    match const_immediate_bits(c) {
        None => Kind::Imm,
        Some(_) => match c {
            Const::Int(_) => Kind::Int,
            Const::Bool(_) => Kind::Bool,
            Const::Float(_) => Kind::Float,
            _ => Kind::Imm, // unit, f32
        },
    }
}

/// The kind of a statically-known immediate word (a [`plan::const_reg_bits`] constant register).
pub(crate) fn classify_immediate_bits(bits: u64) -> Kind {
    let l = Value::NANBOX;
    if bits & (l.sign_bit | l.qnan | l.int_tag) == l.qnan | l.int_tag {
        Kind::Int
    } else if bits == l.true_bits || bits == l.false_bits {
        Kind::Bool
    } else if bits & l.qnan != l.qnan {
        Kind::Float
    } else {
        Kind::Imm // unit, f32
    }
}

/// The forward kind fixpoint (T1 typed promotion): `map[pc * nreg + r]` = register `r`'s
/// statically-known kind at the *start* of op `pc`, over the same tier-0 CFG and the same
/// modeled-op whitelist as the bare-store analysis ([`reg_effect`] — any unmodeled op fails the
/// whole map closed to `Imm`, which emits exactly today's generic code). Transfer highlights:
/// a comparison defines `Bool`; arithmetic defines `Int`/`Float` only when *both* operands
/// already have that kind (else `Imm` — tier 0 may coerce or produce either); everything
/// may-heap (params at entry, globals, call results, heap constants) defines `Imm`, keeping the
/// typed⇒immediate invariant aligned with [`heap_at_fixpoint`] by construction.
pub(crate) fn kind_in_map(chunk: &noeta_bytecode::Chunk) -> Vec<Kind> {
    let n = chunk.code.len();
    let nreg = chunk.num_registers as usize;
    let all_imm = || vec![Kind::Imm; n * nreg];
    if chunk
        .code
        .iter()
        .any(|op| reg_effect(op, &chunk.consts).is_none())
    {
        return all_imm();
    }
    let mut inset = vec![Kind::Bot; n * nreg];
    // Seed pc 0: parameters hold caller values of unknown kind; locals hold `unit`. All `Imm`.
    let row0 = nreg.min(inset.len());
    inset[..row0].fill(Kind::Imm);
    let mut succ = Vec::new();
    let mut changed = true;
    while changed {
        changed = false;
        for pc in 0..n {
            let mut out = inset[pc * nreg..pc * nreg + nreg].to_vec();
            match &chunk.code[pc] {
                Op::LoadConst { dst, k } => {
                    out[*dst as usize] = const_kind(&chunk.consts[*k as usize]);
                }
                Op::Move { dst, src } => out[*dst as usize] = out[*src as usize],
                Op::Drop { reg, .. } => out[*reg as usize] = Kind::Imm, // leaves `unit`
                Op::StoreGlobal { src, .. } => out[*src as usize] = Kind::Imm,
                Op::LoadGlobal { dst, .. }
                | Op::TakeGlobal { dst, .. }
                | Op::Call { dst, .. }
                | Op::CallGlobal { dst, .. }
                | Op::Unary { dst, .. }
                | Op::Stringify { dst, .. } => out[*dst as usize] = Kind::Imm,
                Op::Binary { op, dst, a, b, .. } => {
                    let (ka, kb) = (out[*a as usize], out[*b as usize]);
                    // P-JCT C3 guard strengthening: the asymmetric emitter path (T1b) *guards*
                    // its statically-unknown side, so on every native continuation that operand
                    // held the typed side's kind — claim it downstream (the emitter's matching
                    // `def_raw` keeps the raw variable current; mid-frame entries re-verify).
                    // The generic (Imm, Imm) path proves "immediate", not a single kind — the
                    // heap transfer strengthens there, this map cannot.
                    if supported_binary(*op) {
                        match (ka, kb) {
                            (Kind::Int, Kind::Imm) => out[*b as usize] = Kind::Int,
                            (Kind::Imm, Kind::Int) => out[*a as usize] = Kind::Int,
                            (Kind::Float, Kind::Imm) => out[*b as usize] = Kind::Float,
                            (Kind::Imm, Kind::Float) => out[*a as usize] = Kind::Float,
                            _ => {}
                        }
                    }
                    out[*dst as usize] = match op {
                        BinaryOp::Eq
                        | BinaryOp::Ne
                        | BinaryOp::Lt
                        | BinaryOp::Le
                        | BinaryOp::Gt
                        | BinaryOp::Ge => Kind::Bool,
                        // Tier-B bitwise (S1): the emitter's dispatch is int-or-bail, so a
                        // natively-stored destination is always an int — claim it whenever
                        // neither operand is statically a non-int (where the op always bails
                        // and the claim would be inert anyway).
                        BinaryOp::BitAnd
                        | BinaryOp::BitOr
                        | BinaryOp::BitXor
                        | BinaryOp::Shl
                        | BinaryOp::Shr => match (ka, kb) {
                            (Kind::Float | Kind::Bool, _) | (_, Kind::Float | Kind::Bool) => {
                                Kind::Imm
                            }
                            _ => Kind::Int,
                        },
                        BinaryOp::Add
                        | BinaryOp::Sub
                        | BinaryOp::Mul
                        | BinaryOp::Div
                        | BinaryOp::Rem => match (ka, kb) {
                            (Kind::Int, Kind::Int) => Kind::Int,
                            (Kind::Float, Kind::Float) => Kind::Float,
                            // P-JCT C3: the asymmetric path bails before storing anything that
                            // isn't a fitting int/float — the same contract that makes the
                            // (Int, Int) claim sound. Without this, `a*3 + x%2`'s outer `+`
                            // compiled the full generic two-sided dispatch.
                            (Kind::Int, Kind::Imm) | (Kind::Imm, Kind::Int) => Kind::Int,
                            (Kind::Float, Kind::Imm) | (Kind::Imm, Kind::Float) => Kind::Float,
                            // S2 mixed lane: a statically int×float pairing computes at f64 and
                            // stores a float on every native continuation.
                            (Kind::Int, Kind::Float) | (Kind::Float, Kind::Int) => Kind::Float,
                            _ => Kind::Imm,
                        },
                        _ => Kind::Imm, // `~`, `&&`/`||`, identity — bail ops here; tier 0 decides
                    };
                }
                // S1 (Tier W): the emitter's dispatch is int-or-bail, so a natively-stored
                // destination is an int (comparisons: a bool) — the same claim shape as the
                // Tier-B bitwise arm above. The emitter def_raws on every storing path.
                Op::WideInt { op, dst, .. } => {
                    out[*dst as usize] = match op {
                        BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => Kind::Bool,
                        _ => Kind::Int,
                    }
                }
                Op::MaskWidth { dst, .. } => out[*dst as usize] = Kind::Int,
                // P-JCT C3: a `CondBranch`'s continuation (either successor) proved the
                // scrutinee a bool — a non-bool bails for E0007 before branching. The emitter
                // defines the raw 0/1 form before the branch, on both claimed and generic paths.
                Op::CondBranch { reg, .. } => out[*reg as usize] = Kind::Bool,
                // Inert for registers (the reg_effect whitelist guarantees nothing else appears).
                _ => {}
            }
            analysis_succ(&chunk.code[pc], pc, n, &mut succ);
            for &s in &succ {
                let base = s * nreg;
                for (r, &o) in out.iter().enumerate() {
                    let j = inset[base + r].join(o);
                    if j != inset[base + r] {
                        inset[base + r] = j;
                        changed = true;
                    }
                }
            }
        }
    }
    inset
}

/// The forward **must-slot-written** fixpoint (P-JSSA S4.1): `map[pc * nreg + r]` = has register
/// `r`'s window *slot* been written on **every** path from a fresh pc-0 entry to the start of op
/// `pc`? Under the fast call convention the callee's window is reserved without initialization,
/// so `normalize_frame` may keep a slot's contents only where this map proves a real store
/// happened. With S5's universal residency the only defs that write slots are the
/// known-constant registers' `LoadConst`s (everything else is a pure variable def); sync spills
/// also write slots, but path-dependently, so they are not counted. Meet = AND over
/// [`analysis_succ`] predecessors; pc 0 starts all-unwritten. Requires every op modeled by
/// [`reg_effect`] (the caller checks [`fast_ok`] first).
pub(crate) fn must_slot_written_map(
    chunk: &noeta_bytecode::Chunk,
    const_bits: &[Option<u64>],
) -> Vec<bool> {
    let n = chunk.code.len();
    let nreg = chunk.num_registers as usize;
    let slot_write = |r: usize| -> bool { const_bits.get(r).is_some_and(|c| c.is_some()) };
    // Must-analysis: cells start "written" (⊤) except the entry row, and intersect over
    // predecessors; iterate to the greatest fixpoint. `written[pc]` describes the start of `pc`.
    let mut written = vec![true; n * nreg];
    let row0 = nreg.min(written.len());
    written[..row0].fill(false);
    let mut succ = Vec::new();
    let mut changed = true;
    while changed {
        changed = false;
        for pc in 0..n {
            let mut out: Vec<bool> = written[pc * nreg..pc * nreg + nreg].to_vec();
            match reg_effect(&chunk.code[pc], &chunk.consts) {
                Some(RegEffect::Def { dst, .. }) | Some(RegEffect::Copy { dst, .. }) => {
                    let d = dst as usize;
                    out[d] = out[d] || slot_write(d);
                }
                Some(RegEffect::Clear(r)) => {
                    let d = r as usize;
                    out[d] = out[d] || slot_write(d);
                }
                Some(RegEffect::Inert) => {}
                None => unreachable!("fast_ok requires a fully modeled prototype"),
            }
            analysis_succ(&chunk.code[pc], pc, n, &mut succ);
            for &su in &succ {
                let base = su * nreg;
                for (r, &o) in out.iter().enumerate() {
                    if !o && written[base + r] {
                        written[base + r] = false;
                        changed = true;
                    }
                }
            }
        }
    }
    written
}

/// Whether a prototype is eligible for the **fast call convention** (P-JSSA S4.1/S5): entered
/// with its window reserved **uninitialized** and its arguments as machine arguments, so every
/// native exit must make the window fully tier-0-valid (`Codegen::normalize_frame`). With S5's
/// universal residency the requirements reduce to:
///
/// - every op modeled by the register-effect whitelist (unknown slot effects are unnormalizable);
/// - ≤ 64 registers (the teardown mask, and bounded normalize code).
///
/// Reads-before-write need no clause: reads go through the variables, which the fast entry
/// initializes to `unit` — exactly tier-0's fresh-local semantics. Slot validity needs no
/// per-return clause: the fast return releases from the variables, and `normalize_frame` spills
/// `live ∪ hazard` and unit-fills everything not must-written.
pub(crate) fn fast_ok(chunk: &noeta_bytecode::Chunk) -> bool {
    chunk.num_registers <= 64
        && chunk
            .code
            .iter()
            .all(|op| reg_effect(op, &chunk.consts).is_some())
}
