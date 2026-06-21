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
use lang_object::Shape;
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

/// A prelude collection builtin, called directly by name (`len(x)`, `map(xs, f)`). These
/// are never first-class values in the M1.3 subset — a program that passes one around
/// (rather than calling it) is left unsupported — so they ride in a dedicated `CallBuiltin`
/// op rather than being materialized into a register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    Len,
    Map,
    Filter,
    Sum,
}

impl Builtin {
    /// The surface name, for diagnostics ("`map` expects a list, ...").
    pub fn name(self) -> &'static str {
        match self {
            Builtin::Len => "len",
            Builtin::Map => "map",
            Builtin::Filter => "filter",
            Builtin::Sum => "sum",
        }
    }

    /// The builtin a prelude name refers to, if it is one this slice implements.
    pub fn from_name(name: &str) -> Option<Builtin> {
        match name {
            "len" => Some(Builtin::Len),
            "map" => Some(Builtin::Map),
            "filter" => Some(Builtin::Filter),
            "sum" => Some(Builtin::Sum),
            _ => None,
        }
    }
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
    LoadConst {
        dst: Reg,
        k: u16,
    },
    /// `dst = src` (a refcounted copy: release old `dst`, retain `src`).
    Move {
        dst: Reg,
        src: Reg,
    },
    /// `dst = globals[name]`, or raise E0005 ("cannot find `name` in this scope") at `span`
    /// if the global is unbound. The single name-resolution path for everything that is not a
    /// frame-local register: top-level bindings, function names, and (at runtime) unknowns.
    LoadGlobal {
        dst: Reg,
        name: String,
        span: Span,
    },
    /// `globals[name] = src` (refcounted: release old binding, retain `src`). Only emitted at
    /// the top level — functions never assign globals in the M1.2 subset.
    StoreGlobal {
        name: String,
        src: Reg,
    },
    /// `dst = <closure over proto>` — materialize a function value referencing `proto`.
    MakeClosure {
        dst: Reg,
        proto: u32,
    },
    /// `dst = [items...]` — build a heap list, retaining each element into it.
    MakeList {
        dst: Reg,
        items: Box<[Reg]>,
    },
    /// `dst = {key: value, ...}` — build a heap map (sorted-key), retaining each value. Keys
    /// are validated by a preceding `RequireMapKey`, so they are known strings here.
    MakeMap {
        dst: Reg,
        entries: Box<[(Reg, Reg)]>,
    },
    /// Require `reg` to be a string (a map key), else raise E0007 ("map keys must be strings,
    /// found <type>") at `span`. Emitted between a map entry's key and value so the error
    /// timing matches the M0 tree-walker (key checked before the value is evaluated).
    RequireMapKey {
        reg: Reg,
        span: Span,
    },
    /// `dst = <elements of src to iterate>`. A list yields a retained shallow copy; a map
    /// yields a new list of its values in sorted-key order; anything else raises E0007
    /// ("cannot iterate over <type>") at `span`. Snapshots iteration, as the M0 tree-walker
    /// does, so `dst` is always a list the loop can index.
    IterSnapshot {
        dst: Reg,
        src: Reg,
        span: Span,
    },
    /// `dst = len(src)` where `src` is a list (an iteration snapshot). Never fails.
    ListLen {
        dst: Reg,
        src: Reg,
    },
    /// `dst = list[index]` (retained), where `list` is a list and `index` an in-bounds int.
    ListGet {
        dst: Reg,
        list: Reg,
        index: Reg,
    },
    /// Destructure a 2-element list `src` into `first`/`second` (each retained), else raise
    /// E0007 ("destructuring `(a, b)` expects a 2-element list, found <type>") at `span`.
    DestructurePair {
        first: Reg,
        second: Reg,
        src: Reg,
        span: Span,
    },
    /// `dst = builtin(args...)` — a prelude collection builtin (`len`/`map`/`filter`/`sum`).
    /// `map`/`filter` re-enter the VM to call their closure argument per element.
    CallBuiltin {
        dst: Reg,
        builtin: Builtin,
        args: Box<[Reg]>,
        span: Span,
    },
    /// `dst = recv.method(args...)` — a runtime-dispatched method call (mirroring the M0
    /// tree-walker's `call_method`). On an object, `method` resolves through the module's
    /// instance-method table by the receiver's type name and pushes a call frame `[recv,
    /// args...]`; on a list/map/string, `count`/`enumerate` are the built-in zero-arg methods
    /// computed inline. An unresolved method raises E0005, an arity/type misuse E0007, at
    /// `span`.
    CallMethod {
        dst: Reg,
        recv: Reg,
        method: String,
        args: Box<[Reg]>,
        span: Span,
    },
    /// `dst = recv[index]` — index access (the `Index` trait / list element access), mirroring
    /// the tree-walker's `eval_index`. On an object it dispatches to the `get` method (pushing a
    /// call frame `[recv, index]`); on a list it addresses an element by integer position,
    /// raising E0016 out of bounds. A non-int list index or a non-indexable receiver raises
    /// E0007, at `span`.
    Index {
        dst: Reg,
        recv: Reg,
        index: Reg,
        span: Span,
    },
    /// `dst = Type { named..., ..spread }` — construct a declared record/class instance whose
    /// layout is `shapes[shape]`. `named` gives each provided field's slot index and value
    /// register; `spread` (if present) is a base object every still-unset declared slot is
    /// copied from. A declared slot left unset by both raises E0009 ("missing field(s) ...")
    /// at `span`.
    MakeRecord {
        dst: Reg,
        shape: u32,
        named: Box<[(u16, Reg)]>,
        spread: Option<Reg>,
        span: Span,
    },
    /// `dst = Type { key: value, ..spread }` for an **opaque** `use`-imported type, whose
    /// real field set is unknown until the literal supplies it. The runtime object's shape is
    /// built from the (spread ∪ named) keys in sorted order — matching the M0 tree-walker's
    /// `BTreeMap`-ordered field bag — with no missing/unknown-field checks.
    MakeOpaque {
        dst: Reg,
        type_name: String,
        keys: Box<[(String, Reg)]>,
        spread: Option<Reg>,
    },
    /// `dst = <enum variant>` — construct the `(enum, variant)` value whose shape is
    /// `shapes[shape]`, carrying `args` as the variant's positional data (empty for a no-data
    /// variant). Each argument is retained into the value.
    MakeEnum {
        dst: Reg,
        shape: u32,
        args: Box<[Reg]>,
    },
    /// `dst = obj.field` — load an object field by name (resolved through the receiver's
    /// shape). A receiver that is not an object, or lacks the field, raises E0005 at `span`.
    LoadField {
        dst: Reg,
        obj: Reg,
        field: String,
        span: Span,
    },
    /// `dst = next_id()` — the deterministic seeded counter (1, 2, 3, …), reproducing the M0
    /// tree-walker's `IdGen` (seed 1).
    NextId {
        dst: Reg,
    },
    /// `panic(msg)` — record E0010 ("panic: <msg display>") at `span` and abort the program.
    Panic {
        msg: Reg,
        span: Span,
    },
    /// The `?` operator. If `src` is `Ok(x)`/`some(x)`, `dst = x` (unit for the void `Ok()`)
    /// and execution continues; if it is `Err(_)`/`none`, that value is early-returned from the
    /// current frame (the M0 `Unwind::Return`); anything else raises E0007 at `span`.
    TryUnwrap {
        dst: Reg,
        src: Reg,
        span: Span,
    },
    /// The `??` operator. If `src` is `Ok(x)`/`some(x)`, `dst = x` and execution continues; if
    /// it is `Err(_)`/`none`, jump to `fallback` (the right-hand expression); anything else
    /// raises E0007 at `span`.
    Coalesce {
        dst: Reg,
        src: Reg,
        fallback: u32,
        span: Span,
    },
    /// A `match` literal test: if `src` equals the literal, continue; else jump to `fail` (the
    /// next arm). Three variants for the three literal pattern kinds.
    MatchInt {
        src: Reg,
        value: i64,
        fail: u32,
    },
    MatchStr {
        src: Reg,
        value: String,
        fail: u32,
    },
    MatchBool {
        src: Reg,
        value: bool,
        fail: u32,
    },
    /// A `match` variant test: if `src` is an enum of the given variant (and, when
    /// `type_name` is set, that enum) with `arity` data fields, continue; else jump to `fail`.
    MatchVariant {
        src: Reg,
        type_name: Option<String>,
        variant: String,
        arity: u16,
        fail: u32,
    },
    /// `dst = src.data[index]` (retained) — extract an enum variant's positional field for a
    /// sub-pattern, after a `MatchVariant` has confirmed the shape.
    ExtractField {
        dst: Reg,
        src: Reg,
        index: u16,
    },
    /// No `match` arm matched `src`: raise E0007 ("no match arm matched the value <...>") at
    /// `span` (the M0 runtime non-exhaustive-match error).
    MatchFail {
        src: Reg,
        span: Span,
    },
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
    Return {
        src: Reg,
    },
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
    RequireCondBool {
        reg: Reg,
        span: Span,
    },
    /// Unconditional jump to `target`.
    Jump {
        target: u32,
    },
    /// Jump to `target` if `reg` (a bool) is true.
    JumpIfTrue {
        reg: Reg,
        target: u32,
    },
    /// Jump to `target` if `reg` (a bool) is false.
    JumpIfFalse {
        reg: Reg,
        target: u32,
    },
    /// Print `reg`'s display form followed by a newline.
    Echo {
        reg: Reg,
    },
    /// `dst = display_string(src)` — render `src` for `echo`/interpolation. A user object that
    /// implements the `Display` trait dispatches to its `to_string` method (pushing a call
    /// frame); every other value is copied unchanged, since the consuming `Echo`/`Concat`
    /// stringifies it via `display`. Emitted before each `Echo` and each interpolation hole.
    Stringify {
        dst: Reg,
        src: Reg,
        span: Span,
    },
    /// Push a precomputed diagnostic (`diagnostics[idx]`) and halt — the unknown-name (E0005)
    /// and immutable-assignment (E0006) errors, whose text the compiler knows statically.
    Raise {
        idx: u16,
    },
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

/// One entry in a module's instance-method dispatch table: a class's method, keyed by the
/// `(type_name, method)` pair the VM looks it up by, resolving to the method's prototype.
#[derive(Debug, Clone)]
pub struct MethodEntry {
    pub type_name: String,
    pub method: String,
    pub proto: u32,
}

/// A compiled module: the prototype table plus the object-model side tables. `protos[0]` is
/// the top-level program; the rest are functions, closures, and methods, referenced by
/// `MakeClosure`/`Call`/the method table via their index. `shapes` is the layout table
/// (referenced by index from `MakeRecord`/`MakeEnum`); `methods` is the instance-method
/// dispatch table.
#[derive(Debug, Clone)]
pub struct Module {
    pub protos: Vec<Chunk>,
    pub shapes: Vec<Shape>,
    pub methods: Vec<MethodEntry>,
    /// `(type_name, proto)` for each class with a `destruct` block — the runtime-invoked
    /// destructor, compiled like a parameterless method (receiver in register 0). The VM runs
    /// it when the last reference to an instance of that type drops.
    pub destructors: Vec<(String, u32)>,
    /// Type names that `@derive(Comparable)` without a hand-written `compare` method — the VM
    /// gives their instances structural field-wise ordering for `< <= > >=`.
    pub comparable_derives: Vec<String>,
}

impl Module {
    /// The top-level program prototype (always present).
    pub fn main(&self) -> &Chunk {
        &self.protos[0]
    }

    /// Render the whole module as stable disassembly: the shape and method tables (when
    /// present), then the top-level program followed by each numbered function prototype.
    pub fn disassemble(&self) -> String {
        let mut out = String::new();
        if !self.shapes.is_empty() {
            out.push_str("shapes:\n");
            for (i, shape) in self.shapes.iter().enumerate() {
                let _ = match &shape.variant {
                    Some(variant) => writeln!(
                        out,
                        "  s{i} = enum {}.{variant}({})",
                        shape.name,
                        shape.fields.join(", ")
                    ),
                    None => writeln!(
                        out,
                        "  s{i} = {:?} {}{{{}}}",
                        shape.kind,
                        shape.name,
                        shape.fields.join(", ")
                    ),
                };
            }
        }
        if !self.methods.is_empty() {
            out.push_str("methods:\n");
            for m in &self.methods {
                let _ = writeln!(out, "  {}.{} -> proto {}", m.type_name, m.method, m.proto);
            }
        }
        if !self.destructors.is_empty() {
            out.push_str("destructors:\n");
            for (type_name, proto) in &self.destructors {
                let _ = writeln!(out, "  {type_name} -> proto {proto}");
            }
        }
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
        Op::MakeList { dst, items } => {
            let items: Vec<String> = items.iter().map(|r| format!("r{r}")).collect();
            format!("MakeList    r{dst} <- [{}]", items.join(", "))
        }
        Op::MakeMap { dst, entries } => {
            let entries: Vec<String> = entries.iter().map(|(k, v)| format!("r{k}: r{v}")).collect();
            format!("MakeMap     r{dst} <- {{{}}}", entries.join(", "))
        }
        Op::RequireMapKey { reg, .. } => format!("RequireMapKey r{reg}"),
        Op::IterSnapshot { dst, src, .. } => format!("IterSnapshot r{dst} <- r{src}"),
        Op::ListLen { dst, src } => format!("ListLen     r{dst} <- len r{src}"),
        Op::ListGet { dst, list, index } => format!("ListGet     r{dst} <- r{list}[r{index}]"),
        Op::DestructurePair {
            first, second, src, ..
        } => format!("DestructurePair (r{first}, r{second}) <- r{src}"),
        Op::CallBuiltin {
            dst, builtin, args, ..
        } => {
            let args: Vec<String> = args.iter().map(|r| format!("r{r}")).collect();
            format!(
                "CallBuiltin r{dst} <- {}({})",
                builtin.name(),
                args.join(", ")
            )
        }
        Op::CallMethod {
            dst,
            recv,
            method,
            args,
            ..
        } => {
            let args: Vec<String> = args.iter().map(|r| format!("r{r}")).collect();
            format!(
                "CallMethod  r{dst} <- r{recv}.{method}({})",
                args.join(", ")
            )
        }
        Op::Index {
            dst, recv, index, ..
        } => format!("Index       r{dst} <- r{recv}[r{index}]"),
        Op::MakeRecord {
            dst,
            shape,
            named,
            spread,
            ..
        } => {
            let mut parts: Vec<String> = named
                .iter()
                .map(|(slot, r)| format!("s{slot}=r{r}"))
                .collect();
            if let Some(base) = spread {
                parts.push(format!("..r{base}"));
            }
            format!(
                "MakeRecord  r{dst} <- shape s{shape} {{{}}}",
                parts.join(", ")
            )
        }
        Op::MakeOpaque {
            dst,
            type_name,
            keys,
            spread,
        } => {
            let mut parts: Vec<String> = keys.iter().map(|(k, r)| format!("{k:?}=r{r}")).collect();
            if let Some(base) = spread {
                parts.push(format!("..r{base}"));
            }
            format!("MakeOpaque  r{dst} <- {type_name} {{{}}}", parts.join(", "))
        }
        Op::MakeEnum { dst, shape, args } => {
            let args: Vec<String> = args.iter().map(|r| format!("r{r}")).collect();
            format!("MakeEnum    r{dst} <- shape s{shape}({})", args.join(", "))
        }
        Op::LoadField {
            dst, obj, field, ..
        } => format!("LoadField   r{dst} <- r{obj}.{field}"),
        Op::NextId { dst } => format!("NextId      r{dst}"),
        Op::Panic { msg, .. } => format!("Panic       r{msg}"),
        Op::TryUnwrap { dst, src, .. } => format!("TryUnwrap   r{dst} <- r{src}?"),
        Op::Coalesce {
            dst, src, fallback, ..
        } => format!("Coalesce    r{dst} <- r{src} ?? -> {fallback}"),
        Op::MatchInt { src, value, fail } => {
            format!("MatchInt    r{src} == {value} else -> {fail}")
        }
        Op::MatchStr { src, value, fail } => {
            format!("MatchStr    r{src} == {value:?} else -> {fail}")
        }
        Op::MatchBool { src, value, fail } => {
            format!("MatchBool   r{src} == {value} else -> {fail}")
        }
        Op::MatchVariant {
            src,
            type_name,
            variant,
            arity,
            fail,
        } => {
            let qualifier = match type_name {
                Some(name) => format!("{name}."),
                None => String::new(),
            };
            format!("MatchVariant r{src} is {qualifier}{variant}/{arity} else -> {fail}")
        }
        Op::ExtractField { dst, src, index } => format!("ExtractField r{dst} <- r{src}.{index}"),
        Op::MatchFail { src, .. } => format!("MatchFail   r{src}"),
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
        Op::Stringify { dst, src, .. } => format!("Stringify   r{dst} <- display(r{src})"),
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
