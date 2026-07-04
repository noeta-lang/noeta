//! P-JSSA S0 — the register plan: per-pc **liveness** and per-pc **SSA-residency permission**
//! (`plans/jit/ssa.md`). This module is pure analysis; codegen consumes it starting at S1.
//!
//! Two flattened `[pc * nreg + r]` maps over the tier-0 CFG:
//!
//! - **`live_in`** — may register `r`'s value be read (as an operand, a branch scrutinee, a
//!   `Return`/`Echo` source, or a `Drop` release) along some tier-0 path from the *start* of op
//!   `pc`, before being overwritten? This is the spill set: when native code bails (or calls a
//!   runtime helper) at `pc`, the SSA-resident registers it must materialize into their slots are
//!   exactly the dirty ∩ live-in ones — a dead register's slot is never read, so its staleness is
//!   unobservable.
//! - **`ssa_ok`** — may register `r` be **SSA-resident** at `pc`? Exactly the complement of the
//!   bare-store map ([`crate::heap_in_map`]): a register is promotable only where its value is
//!   provably an immediate. This is the v1 refcount dodge (ssa.md): an immediate carries no
//!   ownership, so eliding its intermediate slot stores changes no refcount, and any
//!   heap→immediate transition happens at a pc where `ssa_ok` is false — i.e. through a real,
//!   releasing store — so a slot left stale by SSA residency always holds an immediate and frame
//!   teardown's release loop stays a no-op over it.
//!
//! **Fail-closed posture.** Liveness fails closed **per op**: an op the model doesn't cover is
//! treated as reading every register and defining none, so everything is live across it (native
//! codegen treats those ops as spill-everything sync points anyway — runtime helpers read and
//! write the frame's slots). `ssa_ok` inherits `heap_in_map`'s whole-prototype fail-closed: any
//! op the heap analysis can't model makes every cell false (nothing promotes).
//!
//! **Successor soundness.** Unlike [`crate::analysis_succ`] (which only ever sees the
//! arithmetic-loop whitelist), liveness runs over *every* op, so [`succ_all`] must know every
//! jump-target-carrying op — the same set the compiler's `patch_jump`/`for_each_target_mut`
//! handle (`Jump`/`JumpIf*`/`CondBranch`, `Coalesce.fallback`, the five `Match*.fail`s). A missed
//! edge would under-approximate liveness (an unsound spill omission); an op that never falls
//! through but is treated as if it did (`MatchFail`, which raises) only over-approximates — safe.

use noeta_bytecode::{Chunk, Op, Reg};

/// The S0 register plan for a prototype. See the module docs for the two maps' contracts.
pub(crate) struct RegPlan {
    live_in: Vec<bool>,
    ssa_ok: Vec<bool>,
    nreg: usize,
}

// Consumed by codegen from S1 (straight-line promotion) on; until then only the contract tests
// read it.
#[allow(dead_code)]
impl RegPlan {
    pub(crate) fn compute(chunk: &Chunk, heap_aware: bool) -> RegPlan {
        let nreg = chunk.num_registers as usize;
        let heap_in = crate::heap_in_map(chunk, heap_aware);
        RegPlan {
            live_in: live_in_map(chunk),
            ssa_ok: heap_in.iter().map(|&h| !h).collect(),
            nreg,
        }
    }

    /// May `r`'s value be read along some tier-0 path from the start of op `pc`?
    pub(crate) fn live_at(&self, pc: usize, r: Reg) -> bool {
        self.live_in[pc * self.nreg + r as usize]
    }

    /// May `r` be SSA-resident at `pc` (provably immediate there)?
    pub(crate) fn ssa_ok_at(&self, pc: usize, r: Reg) -> bool {
        self.ssa_ok[pc * self.nreg + r as usize]
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

/// Up to two inline reads plus an op's argument list (`Call`/`CallGlobal`, borrowed from the op)
/// — avoids a heap allocation per op in the fixpoint's inner loop.
struct ReadSet<'a> {
    inline: [Option<Reg>; 2],
    list: &'a [Reg],
}

impl<'a> ReadSet<'a> {
    const EMPTY: ReadSet<'static> = ReadSet {
        inline: [None; 2],
        list: &[],
    };
    fn one(a: Reg) -> ReadSet<'static> {
        ReadSet {
            inline: [Some(a), None],
            list: &[],
        }
    }
    fn two(a: Reg, b: Reg) -> ReadSet<'static> {
        ReadSet {
            inline: [Some(a), Some(b)],
            list: &[],
        }
    }
    fn for_each(&self, mut f: impl FnMut(Reg)) {
        for r in self.inline.iter().flatten() {
            f(*r);
        }
        for r in self.list {
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
            dst, callee, args, ..
        } => Modeled {
            reads: ReadSet {
                inline: [Some(*callee), None],
                list: args,
            },
            def: Some(*dst),
        },
        Op::CallGlobal { dst, args, .. } => Modeled {
            reads: ReadSet {
                inline: [None; 2],
                list: args,
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

/// The tier-0 successors of `pc` for **every** op (cf. the module docs on soundness): each
/// jump-target field plus fallthrough, no fallthrough after `Jump`/`Return`/`Halt`. `MatchFail`
/// raises (no real successor) but keeps its conservative fallthrough — over-approximation only.
fn succ_all(op: &Op, pc: usize, n: usize, out: &mut Vec<usize>) {
    out.clear();
    let mut fallthrough = true;
    match op {
        Op::Jump { target } => {
            out.push(*target as usize);
            fallthrough = false;
        }
        Op::JumpIfTrue { target, .. }
        | Op::JumpIfFalse { target, .. }
        | Op::CondBranch { target, .. } => out.push(*target as usize),
        Op::Coalesce { fallback, .. } => out.push(*fallback as usize),
        Op::MatchInt { fail, .. }
        | Op::MatchStr { fail, .. }
        | Op::MatchBool { fail, .. }
        | Op::MatchVariant { fail, .. }
        | Op::MatchTuple { fail, .. } => out.push(*fail as usize),
        Op::Return { .. } | Op::Halt => fallthrough = false,
        _ => {}
    }
    if fallthrough && pc + 1 < n {
        out.push(pc + 1);
    }
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
            succ_all(&chunk.code[pc], pc, n, &mut succ);
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
    /// live across the match (the `analysis_succ`-whitelist hazard [`succ_all`] exists to avoid).
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

    /// `ssa_ok` is the complement of the bare-store map: a call-free non-OSR prototype promotes
    /// everywhere (params included — the pc-0 entry guard proved them immediate); a `heap_aware`
    /// prototype's parameter is not promotable while it may hold a heap value, but a natively
    /// stored arithmetic result is.
    #[test]
    fn ssa_ok_tracks_provable_immediacy() {
        // 0: r1 = r0 < r0   1: r2 = r0 + r0   2: halt
        let code = vec![
            Op::Binary {
                op: BinaryOp::Lt,
                dst: 1,
                a: 0,
                b: 0,
                span: sp(),
            },
            Op::Binary {
                op: BinaryOp::Add,
                dst: 2,
                a: 0,
                b: 0,
                span: sp(),
            },
            Op::Halt,
        ];
        let free = RegPlan::compute(&chunk(code.clone(), vec![], 1, 3), false);
        assert!(
            free.ssa_ok_at(0, 0) && free.ssa_ok_at(2, 2),
            "a non-heap-aware prototype promotes everywhere"
        );
        let aware = RegPlan::compute(&chunk(code, vec![], 1, 3), true);
        assert!(
            !aware.ssa_ok_at(0, 0),
            "a heap-aware prototype's parameter may be heap — not promotable"
        );
        assert!(
            aware.ssa_ok_at(2, 2),
            "a natively stored arithmetic result is an immediate — promotable"
        );
    }
}
