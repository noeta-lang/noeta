//! P-JSSA S0/S5 — the register plan: per-pc **liveness** and the prototype-wide **residency
//! permission** (`plans/jit/ssa.md`). This module is pure analysis; codegen consumes it.
//!
//! - **`live_in`** (flattened `[pc * nreg + r]`, over the tier-0 CFG) — may register `r`'s value
//!   be read (as an operand, a branch scrutinee, a `Return`/`Echo` source, or a `Drop` release)
//!   along some tier-0 path from the *start* of op `pc`, before being overwritten? This is the
//!   core of the spill set: when native code bails (or calls a runtime helper) at `pc`, the
//!   SSA-resident registers it must materialize into their slots are the live-in ones plus the
//!   heap-desynced ones ([`crate::slot_hazard_map`], S5) — a dead, non-hazardous register's slot
//!   is never read and holds nothing that teardown could misrelease.
//! - **`modeled`** (S5) — residency is universal: in a modeled prototype **every** register
//!   (heap values included) lives in its SSA variable; `heap_in_map` decides release-on-overwrite
//!   and sync obligations instead of gating residency. An unmodeled prototype promotes nothing.
//!
//! **Fail-closed posture.** Liveness fails closed **per op**: an op the model doesn't cover is
//! treated as reading every register and defining none, so everything is live across it (native
//! codegen treats those ops as spill-everything sync points anyway — runtime helpers read and
//! write the frame's slots). Residency fails closed **per prototype**: any op the register-effect
//! model can't cover makes the whole prototype unmodeled (nothing promotes).
//!
//! **Successor soundness.** Liveness runs over *every* op, so its successor function must know
//! every jump-target-carrying op. It no longer keeps a list: [`crate::tier0_succ`] asks
//! [`Op::for_each_jump_pc`], the single exhaustive answer in `noeta-bytecode`, which the
//! compiler's `patch_jump`, `op_facts` and LICM target fix-up ask too. A missed edge would
//! under-approximate liveness (an unsound spill omission); an op that never falls through but is
//! treated as if it did (`MatchFail`, which raises) only over-approximates — safe.

use noeta_bytecode::{Chunk, Op, Reg};

use crate::analysis::tier0_succ;

/// The S0/S5 register plan for a prototype. See the module docs for the maps' contracts.
pub(crate) struct RegPlan {
    live_in: Vec<bool>,
    /// P-JSSA S5: residency is **universal** in a modeled prototype — every register (heap
    /// values included) lives in its SSA variable, and `heap_in` decides release-on-overwrite
    /// and sync obligations instead of gating residency. An unmodeled prototype promotes
    /// nothing (no variable is ever defined or used) — byte-identical to the slot backend.
    modeled: bool,
    nreg: usize,
}

impl RegPlan {
    /// Build the plan. `modeled` says whether every op's register effect is known to the heap
    /// analysis (or the prototype is call-free/non-OSR, where the all-immediate invariant holds
    /// without modeling) — the S5 residency permission.
    pub(crate) fn with_heap_in(chunk: &Chunk, _heap_in: &[bool], modeled: bool) -> RegPlan {
        RegPlan {
            live_in: live_in_map(chunk),
            modeled,
            nreg: chunk.num_registers as usize,
        }
    }

    #[cfg(test)]
    pub(crate) fn compute(chunk: &Chunk, heap_aware: bool) -> RegPlan {
        let heap_in = crate::heap_in_map(chunk, heap_aware);
        let modeled = crate::proto_modeled(chunk, heap_aware);
        RegPlan::with_heap_in(chunk, &heap_in, modeled)
    }

    /// May `r`'s value be read along some tier-0 path from the start of op `pc`?
    pub(crate) fn live_at(&self, pc: usize, r: Reg) -> bool {
        self.live_in[pc * self.nreg + r as usize]
    }

    /// Is `r` promotable (carries an SSA variable)? S5: all registers of a modeled prototype.
    pub(crate) fn promotable(&self, _r: Reg) -> bool {
        self.modeled
    }
}

/// One op's liveness effect: the registers it reads and the single register it unconditionally
/// (re)defines on **every** successor path, or `Unmodeled`.
///
/// Modeling notes:
/// - `Drop` both reads (the release observes the value — a real effect for a heap register) and
///   redefines (the slot is left `unit`) its register.
/// - `StoreGlobal` moves its source out (reads it, leaves `unit`).
/// - `Coalesce` defines its destination only on the success path (the `Empty` path jumps to the
///   fallback expression, which writes it there), so it must not kill — `def: None`.
/// - A def must hold on *all* paths to be a kill; reads may be over-approximated freely.
enum LiveEffect<'a> {
    Modeled {
        reads: ReadSet<'a>,
        def: Option<Reg>,
    },
    Unmodeled,
}

/// Up to two inline reads plus an op's borrowed register lists — a call's value arguments and, on
/// a forwarding call, its type arguments (both borrowed from the op, so the fixpoint's inner loop
/// allocates nothing). `list2` is empty for every op but a forwarding `Call`/`CallGlobal`; leaving
/// those registers out would under-approximate liveness, which is the one direction that is
/// unsound (a live type-argument register whose spill was omitted).
struct ReadSet<'a> {
    inline: [Option<Reg>; 2],
    list: &'a [Reg],
    list2: &'a [Reg],
}

impl<'a> ReadSet<'a> {
    const EMPTY: ReadSet<'static> = ReadSet {
        inline: [None; 2],
        list: &[],
        list2: &[],
    };
    fn one(a: Reg) -> ReadSet<'static> {
        ReadSet {
            inline: [Some(a), None],
            list: &[],
            list2: &[],
        }
    }
    fn two(a: Reg, b: Reg) -> ReadSet<'static> {
        ReadSet {
            inline: [Some(a), Some(b)],
            list: &[],
            list2: &[],
        }
    }
    fn for_each(&self, mut f: impl FnMut(Reg)) {
        for r in self.inline.iter().flatten() {
            f(*r);
        }
        for r in self.list {
            f(*r);
        }
        for r in self.list2 {
            f(*r);
        }
    }
}

fn live_effect(op: &Op) -> LiveEffect<'_> {
    use LiveEffect::Modeled;
    match op {
        Op::LoadConst { dst, .. } | Op::LoadGlobal { dst, .. } | Op::TakeGlobal { dst, .. } => {
            Modeled {
                reads: ReadSet::EMPTY,
                def: Some(*dst),
            }
        }
        Op::Move { dst, src } => Modeled {
            reads: ReadSet::one(*src),
            def: Some(*dst),
        },
        Op::Drop { reg, .. } => Modeled {
            reads: ReadSet::one(*reg),
            def: Some(*reg),
        },
        Op::StoreGlobal { src, .. } => Modeled {
            reads: ReadSet::one(*src),
            def: Some(*src),
        },
        Op::Binary { dst, a, b, .. } | Op::WideInt { dst, a, b, .. } => Modeled {
            reads: ReadSet::two(*a, *b),
            def: Some(*dst),
        },
        Op::Unary { dst, src, .. }
        | Op::Stringify { dst, src, .. }
        | Op::MaskWidth { dst, src, .. }
        | Op::ExtractField { dst, src, .. } => Modeled {
            reads: ReadSet::one(*src),
            def: Some(*dst),
        },
        Op::WidthIntMethod { dst, recv, arg, .. } => Modeled {
            reads: match arg {
                Some(a) => ReadSet::two(*recv, *a),
                None => ReadSet::one(*recv),
            },
            def: Some(*dst),
        },
        Op::Call {
            dst,
            callee,
            args,
            type_args,
            ..
        } => Modeled {
            reads: ReadSet {
                inline: [Some(*callee), None],
                list: args,
                list2: type_args.regs(),
            },
            def: Some(*dst),
        },
        Op::CallGlobal {
            dst,
            args,
            type_args,
            ..
        } => Modeled {
            reads: ReadSet {
                inline: [None; 2],
                list: args,
                list2: type_args.regs(),
            },
            def: Some(*dst),
        },
        Op::Coalesce { src, .. } => Modeled {
            reads: ReadSet::one(*src),
            def: None, // written on the success path only — not a kill
        },
        Op::JumpIfTrue { reg, .. }
        | Op::JumpIfFalse { reg, .. }
        | Op::CondBranch { reg, .. }
        | Op::RequireBool { reg, .. }
        | Op::RequireCondBool { reg, .. }
        | Op::Echo { reg } => Modeled {
            reads: ReadSet::one(*reg),
            def: None,
        },
        Op::MatchInt { src, .. }
        | Op::MatchStr { src, .. }
        | Op::MatchBool { src, .. }
        | Op::MatchVariant { src, .. }
        | Op::MatchTuple { src, .. }
        | Op::MatchFail { src, .. }
        | Op::Return { src } => Modeled {
            reads: ReadSet::one(*src),
            def: None,
        },
        Op::Jump { .. } | Op::Halt => Modeled {
            reads: ReadSet::EMPTY,
            def: None,
        },
        _ => LiveEffect::Unmodeled,
    }
}

/// Registers that provably hold one statically-known immediate constant at every read the frame
/// can execute: written exactly once in the whole chunk, by a `LoadConst` of an immediate, with
/// no read reachable from the frame's start (pc 0) without passing through that def. Reads of
/// such a register can be **inlined as the constant** — it then needs no SSA variable at all (no
/// entry load, no loop block param, no register pressure) and its slot, written once at the def,
/// is always current (no spills). This is exactly the shape LICM's hoisted-constant registers
/// have (defined once in a loop pre-header, read in the loop). Mid-frame native entries (OSR /
/// resume-after-call) don't weaken the reachability argument: they resume a frame in which
/// tier 0 already executed everything up to that pc — including the def.
///
/// Fails safe: any op the liveness model doesn't cover disqualifies every register (an unmodeled
/// op could write anything), a parameter register is never constant (the caller writes it
/// invisibly at frame setup), and a second write of any kind (including a `Drop`'s clearing)
/// disqualifies.
pub(crate) fn const_reg_bits(chunk: &Chunk) -> Vec<Option<u64>> {
    let n = chunk.code.len();
    let nreg = chunk.num_registers as usize;
    let mut writes: Vec<Vec<usize>> = vec![Vec::new(); nreg];
    let mut reads: Vec<Vec<usize>> = vec![Vec::new(); nreg];
    for (pc, op) in chunk.code.iter().enumerate() {
        match live_effect(op) {
            LiveEffect::Modeled { reads: rs, def } => {
                rs.for_each(|r| reads[r as usize].push(pc));
                if let Some(d) = def {
                    writes[d as usize].push(pc);
                }
            }
            LiveEffect::Unmodeled => return vec![None; nreg],
        }
    }
    let mut out = vec![None; nreg];
    for r in chunk.num_params as usize..nreg {
        let [def_pc] = writes[r][..] else { continue };
        let Op::LoadConst { k, .. } = &chunk.code[def_pc] else {
            continue;
        };
        let Some(bits) = crate::const_immediate_bits(&chunk.consts[*k as usize]) else {
            continue;
        };
        if !any_read_bypasses_def(chunk, n, def_pc, &reads[r]) {
            out[r] = Some(bits);
        }
    }
    out
}

/// Is any of `read_pcs` reachable from pc 0 along a path that never executes `def_pc`? (BFS over
/// [`crate::tier0_succ`], refusing to expand `def_pc`.) If not, every executed read observes the def.
fn any_read_bypasses_def(chunk: &Chunk, n: usize, def_pc: usize, read_pcs: &[usize]) -> bool {
    let mut seen = vec![false; n];
    let mut stack = vec![0usize];
    let mut succ = Vec::new();
    while let Some(pc) = stack.pop() {
        if pc >= n || seen[pc] {
            continue;
        }
        seen[pc] = true;
        if read_pcs.contains(&pc) {
            return true;
        }
        if pc == def_pc {
            continue; // paths through the def are fine — don't expand it
        }
        tier0_succ(&chunk.code[pc], pc, n, &mut succ);
        stack.extend_from_slice(&succ);
    }
    false
}

/// The backward may-be-read fixpoint: `map[pc * nreg + r]` = may `r`'s value be read before being
/// overwritten, along some tier-0 path from the start of op `pc`. `live_in = reads ∪ (live_out −
/// def)`, `live_out = ⋃ successors' live_in`, join = union. An unmodeled op reads everything and
/// kills nothing (fail closed, per op).
fn live_in_map(chunk: &Chunk) -> Vec<bool> {
    let n = chunk.code.len();
    let nreg = chunk.num_registers as usize;
    let mut live = vec![false; n * nreg];
    let mut succ = Vec::new();
    let mut changed = true;
    while changed {
        changed = false;
        for pc in (0..n).rev() {
            // live_out = union of successors' live_in.
            let mut out = vec![false; nreg];
            tier0_succ(&chunk.code[pc], pc, n, &mut succ);
            for &s in &succ {
                for (r, o) in out.iter_mut().enumerate() {
                    *o |= live[s * nreg + r];
                }
            }
            // live_in = reads ∪ (live_out − def).
            match live_effect(&chunk.code[pc]) {
                LiveEffect::Modeled { reads, def } => {
                    if let Some(d) = def {
                        out[d as usize] = false;
                    }
                    reads.for_each(|r| out[r as usize] = true);
                }
                LiveEffect::Unmodeled => out.fill(true),
            }
            let base = pc * nreg;
            for (r, &o) in out.iter().enumerate() {
                if o && !live[base + r] {
                    live[base + r] = true;
                    changed = true;
                }
            }
        }
    }
    live
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_ast::BinaryOp;
    use noeta_bytecode::{Chunk, Const};
    use noeta_span::Span;

    fn chunk(code: Vec<Op>, consts: Vec<Const>, num_params: u16, num_registers: u16) -> Chunk {
        let mut c = Chunk::placeholder();
        c.code = code;
        c.consts = consts;
        c.num_params = num_params;
        c.num_registers = num_registers;
        c
    }

    fn sp() -> Span {
        Span::new(0, 0)
    }

    /// Straight line: a value is live from its definition to its last read and dead after.
    #[test]
    fn live_spans_def_to_last_read() {
        // 0: r1 = k0        1: r2 = r1 + r1        2: return r2
        let c = chunk(
            vec![
                Op::LoadConst { dst: 1, k: 0 },
                Op::Binary {
                    op: BinaryOp::Add,
                    dst: 2,
                    a: 1,
                    b: 1,
                    span: sp(),
                },
                Op::Return { src: 2 },
            ],
            vec![Const::Int(1)],
            1,
            3,
        );
        let p = RegPlan::compute(&c, false);
        assert!(!p.live_at(0, 0), "the unread parameter is never live");
        assert!(!p.live_at(0, 1), "r1 is dead before its definition's read");
        assert!(p.live_at(1, 1), "r1 is live at its read");
        assert!(!p.live_at(2, 1), "r1 is dead after its last read");
        assert!(p.live_at(2, 2), "r2 is live at the return that reads it");
    }

    /// A loop-carried register is live at the header via the back edge, even though the only read
    /// is below the header.
    #[test]
    fn back_edge_keeps_loop_carried_register_live() {
        // 0: r1 = r1 + r1   1: jump 0
        let c = chunk(
            vec![
                Op::Binary {
                    op: BinaryOp::Add,
                    dst: 1,
                    a: 1,
                    b: 1,
                    span: sp(),
                },
                Op::Jump { target: 0 },
                Op::Halt,
            ],
            vec![],
            0,
            2,
        );
        let p = RegPlan::compute(&c, false);
        assert!(p.live_at(0, 1), "loop-carried r1 is live at the header");
        assert!(p.live_at(1, 1), "…and around the back edge");
    }

    /// The `Match*.fail` edge is a real successor: a register read only on the fail path stays
    /// live across the match (the whitelist hazard [`crate::tier0_succ`] exists to avoid).
    #[test]
    fn match_fail_edge_keeps_fail_path_read_live() {
        // 0: match-bool r0 else -> 2   1: halt   2: echo r1   3: halt
        let c = chunk(
            vec![
                Op::MatchBool {
                    src: 0,
                    value: true,
                    fail: 2,
                },
                Op::Halt,
                Op::Echo { reg: 1 },
                Op::Halt,
            ],
            vec![],
            0,
            2,
        );
        let p = RegPlan::compute(&c, false);
        assert!(
            p.live_at(0, 1),
            "r1 is read on the fail path, so it is live at the match"
        );
        assert!(p.live_at(0, 0), "the scrutinee itself is read");
    }

    /// An unmodeled op fails closed per op: everything is live at it (and flows backward), but a
    /// register dead *after* it is still recognized as dead — the failure is local, not
    /// whole-prototype.
    #[test]
    fn unmodeled_op_is_all_live_locally() {
        // 0: make-tuple r2 <- (r0, r1)   1: halt
        let c = chunk(
            vec![
                Op::MakeTuple {
                    dst: 2,
                    items: vec![0, 1].into_boxed_slice(),
                },
                Op::Echo { reg: 2 },
                Op::Halt,
            ],
            vec![],
            0,
            3,
        );
        let p = RegPlan::compute(&c, false);
        assert!(
            p.live_at(0, 0) && p.live_at(0, 1),
            "all live at the unmodeled op"
        );
        assert!(
            p.live_at(0, 2),
            "…including its own destination (no kill modeled)"
        );
        assert!(!p.live_at(1, 0), "but liveness below it is still precise");
    }

    /// `Drop` reads (the release) and redefines (leaves `unit`): the register is live at the
    /// `Drop`, dead above it once nothing else reads it, and dead below it.
    #[test]
    fn drop_reads_then_kills() {
        // 0: r0 = k0   1: drop r0   2: halt
        let c = chunk(
            vec![
                Op::LoadConst { dst: 0, k: 0 },
                Op::Drop {
                    reg: 0,
                    relevant: false,
                },
                Op::Halt,
            ],
            vec![Const::Int(1)],
            0,
            1,
        );
        let p = RegPlan::compute(&c, false);
        assert!(p.live_at(1, 0), "live at the Drop that releases it");
        assert!(!p.live_at(2, 0), "dead below the Drop");
    }

    /// S5 residency: a modeled prototype promotes **every** register (heap values included — a
    /// call's result too); an unmodeled one (an op the heap analysis can't track) promotes
    /// nothing.
    #[test]
    fn residency_is_universal_iff_modeled() {
        let code = vec![
            Op::Binary {
                op: BinaryOp::Lt,
                dst: 1,
                a: 0,
                b: 0,
                span: sp(),
            },
            Op::Call {
                dst: 1,
                callee: 2,
                args: Box::new([]),
                type_args: noeta_bytecode::TypeArgs::NONE,
                span: sp(),
                supplied: None,
            },
            Op::Halt,
        ];
        let aware = RegPlan::compute(&chunk(code, vec![], 1, 3), true);
        assert!(
            aware.promotable(0) && aware.promotable(1) && aware.promotable(2),
            "a modeled prototype promotes everything — call results included (S5)"
        );
        let unmodeled = RegPlan::compute(
            &chunk(
                vec![
                    Op::MakeTuple {
                        dst: 0,
                        items: Box::new([]),
                    },
                    Op::Halt,
                ],
                vec![],
                0,
                1,
            ),
            true,
        );
        assert!(
            !unmodeled.promotable(0),
            "an unmodeled prototype promotes nothing"
        );
    }
}
