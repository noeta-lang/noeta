//! The register bytecode IR: opcodes, the constant pool, and a disassembler.
//!
//! Register-based (Lua/Dalvik style), not stack-based — fewer dispatches and a friendlier
//! base for the later specializing interpreter (architecture §6). This crate is pure data:
//! it knows nothing about runtime values. The compiler (`lang-compiler`) emits a [`Chunk`];
//! the VM (`lang-vm`) interprets one. The disassembler renders a [`Chunk`] to a stable text
//! form for snapshot tests (raw bytes are never asserted).

use std::fmt::Write as _;

use lang_ast::{BinaryOp, UnaryOp};
use lang_diagnostics::Diagnostic;
use lang_span::Span;

/// A register index. M1.0's allocator is monotonic (one register per value, no reuse); a
/// reusing allocator is a later optimization the disassembly snapshots will make visible.
pub type Reg = u16;

/// Which operand of a logical operator is being checked, for the "expects a bool on the
/// left/right" diagnostic (matching the M0 tree-walker's `eval_logical`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoolSide {
    Left,
    Right,
}

/// A compile-time constant, materialized into a runtime value on `LoadConst`.
#[derive(Debug, Clone, PartialEq)]
pub enum Const {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

/// One register-machine instruction.
#[derive(Debug, Clone)]
pub enum Op {
    /// `dst = consts[k]`
    LoadConst { dst: Reg, k: u16 },
    /// `dst = src` (a refcounted copy: release old `dst`, retain `src`).
    Move { dst: Reg, src: Reg },
    /// `dst = globals[name]`, or raise E0005 ("cannot find `name` in this scope") at `span`
    /// if the global is unbound. The single name-resolution path for everything that is not a
    /// frame-local register: top-level bindings, function names, and (at runtime) unknowns.
    LoadGlobal { dst: Reg, name: String, span: Span },
    /// `globals[name] = src` (refcounted: release old binding, retain `src`). Only emitted at
    /// the top level — functions never assign globals in the M1.2 subset.
    StoreGlobal { name: String, src: Reg },
    /// `dst = <closure over proto>` — materialize a function value referencing `proto`.
    MakeClosure { dst: Reg, proto: u32 },
    /// `dst = callee(args...)`. Pushes a new call frame; the callee must be a closure whose
    /// prototype's arity equals `args.len()` (else E0007 at `span`); a non-callable callee is
    /// E0007 ("<type> is not callable") at `span`.
    Call {
        dst: Reg,
        callee: Reg,
        args: Box<[Reg]>,
        span: Span,
    },
    /// Return `src` from the current frame to the caller's destination register (or end the
    /// program if returning from the top-level frame).
    Return { src: Reg },
    /// `dst = op src` — may raise (E0007) at `span`.
    Unary {
        op: UnaryOp,
        dst: Reg,
        src: Reg,
        span: Span,
    },
    /// `dst = a op b` — may raise (E0007/E0008) at `span`. Never `&&`/`||` (those lower to
    /// branches).
    Binary {
        op: BinaryOp,
        dst: Reg,
        a: Reg,
        b: Reg,
        span: Span,
    },
    /// Require `reg` to be a bool, else raise E0007 with the logical-operator message.
    RequireBool {
        reg: Reg,
        side: BoolSide,
        op: BinaryOp,
        span: Span,
    },
    /// Require `reg` to be a bool, else raise E0007 with the `if`-condition message
    /// ("`if` condition must be a bool, found <type>") at `span`.
    RequireCondBool { reg: Reg, span: Span },
    /// Unconditional jump to `target`.
    Jump { target: u32 },
    /// Jump to `target` if `reg` (a bool) is true.
    JumpIfTrue { reg: Reg, target: u32 },
    /// Jump to `target` if `reg` (a bool) is false.
    JumpIfFalse { reg: Reg, target: u32 },
    /// Print `reg`'s display form followed by a newline.
    Echo { reg: Reg },
    /// Push a precomputed diagnostic (`diagnostics[idx]`) and halt — the unknown-name (E0005)
    /// and immutable-assignment (E0006) errors, whose text the compiler knows statically.
    Raise { idx: u16 },
    /// Stop the program successfully.
    Halt,
}

/// A compiled function prototype: instructions, the constant pool, the precomputed raise
/// diagnostics, the parameter count, and the number of registers the frame needs. The
/// top-level program is just the prototype at index 0 (`num_params == 0`); every `fn` and
/// closure is another prototype.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub code: Vec<Op>,
    pub consts: Vec<Const>,
    pub diagnostics: Vec<Diagnostic>,
    /// Parameters occupy registers `0..num_params` on entry.
    pub num_params: u16,
    pub num_registers: u16,
}

impl Chunk {
    /// An empty placeholder prototype, used to reserve the top-level slot (index 0) while
    /// nested functions are compiled into later slots.
    pub fn placeholder() -> Chunk {
        Chunk {
            code: Vec::new(),
            consts: Vec::new(),
            diagnostics: Vec::new(),
            num_params: 0,
            num_registers: 0,
        }
    }

    /// Render the chunk as stable, human-readable disassembly for snapshot tests.
    pub fn disassemble(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "params: {}, registers: {}",
            self.num_params, self.num_registers
        );
        if !self.consts.is_empty() {
            out.push_str("constants:\n");
            for (i, c) in self.consts.iter().enumerate() {
                let _ = writeln!(out, "  k{i} = {}", const_repr(c));
            }
        }
        out.push_str("code:\n");
        for (i, op) in self.code.iter().enumerate() {
            let _ = writeln!(out, "  {i:>3}  {}", op_repr(op, &self.diagnostics));
        }
        out
    }
}

/// A compiled module: the prototype table. `protos[0]` is the top-level program; the rest
/// are functions and closures, referenced by `MakeClosure`/`Call` via their index.
#[derive(Debug, Clone)]
pub struct Module {
    pub protos: Vec<Chunk>,
}

impl Module {
    /// The top-level program prototype (always present).
    pub fn main(&self) -> &Chunk {
        &self.protos[0]
    }

    /// Render the whole module as stable disassembly: the top-level program followed by each
    /// numbered function prototype.
    pub fn disassemble(&self) -> String {
        let mut out = String::new();
        for (i, proto) in self.protos.iter().enumerate() {
            if i == 0 {
                out.push_str("=== main ===\n");
            } else {
                let _ = writeln!(out, "=== proto {i} ===");
            }
            out.push_str(&proto.disassemble());
        }
        out
    }
}

fn const_repr(c: &Const) -> String {
    match c {
        Const::Unit => "unit".to_string(),
        Const::Bool(b) => b.to_string(),
        Const::Int(i) => i.to_string(),
        Const::Float(f) => format!("{f:?}"),
        Const::Str(s) => format!("{s:?}"),
    }
}

fn op_repr(op: &Op, diagnostics: &[Diagnostic]) -> String {
    match op {
        Op::LoadConst { dst, k } => format!("LoadConst   r{dst} <- k{k}"),
        Op::Move { dst, src } => format!("Move        r{dst} <- r{src}"),
        Op::LoadGlobal { dst, name, .. } => format!("LoadGlobal  r{dst} <- {name:?}"),
        Op::StoreGlobal { name, src } => format!("StoreGlobal {name:?} <- r{src}"),
        Op::MakeClosure { dst, proto } => format!("MakeClosure r{dst} <- proto {proto}"),
        Op::Call {
            dst, callee, args, ..
        } => {
            let args: Vec<String> = args.iter().map(|r| format!("r{r}")).collect();
            format!("Call        r{dst} <- r{callee}({})", args.join(", "))
        }
        Op::Return { src } => format!("Return      r{src}"),
        Op::Unary { op, dst, src, .. } => format!("Unary       r{dst} <- {} r{src}", op.symbol()),
        Op::Binary { op, dst, a, b, .. } => {
            format!("Binary      r{dst} <- r{a} {} r{b}", op.symbol())
        }
        Op::RequireBool { reg, side, op, .. } => {
            format!("RequireBool r{reg} ({} {side:?})", op.symbol())
        }
        Op::RequireCondBool { reg, .. } => format!("RequireCondBool r{reg} (if)"),
        Op::Jump { target } => format!("Jump        -> {target}"),
        Op::JumpIfTrue { reg, target } => format!("JumpIfTrue  r{reg} -> {target}"),
        Op::JumpIfFalse { reg, target } => format!("JumpIfFalse r{reg} -> {target}"),
        Op::Echo { reg } => format!("Echo        r{reg}"),
        Op::Raise { idx } => {
            let code = diagnostics
                .get(*idx as usize)
                .map(|d| d.code.to_string())
                .unwrap_or_else(|| "?".to_string());
            format!("Raise       {code} (d{idx})")
        }
        Op::Halt => "Halt".to_string(),
    }
}
