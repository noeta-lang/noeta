//! The Tier-0 register VM: executes a [`Module`] into a [`RunResult`].
//!
//! `VmBackend` is the second [`Backend`] (the M0 tree-walker is the first). The conformance
//! harness runs both over the corpus and asserts identical `RunResult`s — the differential
//! oracle. The VM compiles only a subset of the language, so [`VmBackend::try_run`] returns
//! [`Unsupported`] for programs it can't lower yet; the harness skips those and tracks a
//! climbing coverage percentage.
//!
//! ## Call frames and globals
//!
//! Each prototype runs in its own [`Frame`]: a register file, a program counter, and the
//! caller register its return value flows back into. `Call` pushes a frame; `Return` (or
//! falling off the end, an implicit unit return) pops one and threads the value into the
//! caller. The top-level program is the bottom frame; its `Halt`/`Return` ends the program.
//! Top-level bindings and function names live in a by-name `globals` table that every frame
//! shares — the runtime half of the compiler's two-level scope model.
//!
//! Memory is refcounted (`lang-gc`): every register and every global owns one reference to
//! its value. The invariants are local — overwriting a slot releases the old occupant, a
//! `Move`/`LoadGlobal`/`Call`-argument retains the source, a returned value is retained
//! across its frame's teardown, and on exit every frame register and global is released — so
//! no value leaks and none is freed twice. A heap collection owns one reference to each of
//! its elements (the `MakeList`/`MakeMap`/iteration ops retain into it); freeing it releases
//! them. `miri` checks all of this over the unit tests.
//!
//! ## Re-entrant builtins
//!
//! `map`/`filter` are native, yet must call a *user* closure once per element. The dispatch
//! loop runs over an explicit frame stack ([`Vm::run`]); a native builtin re-enters the VM
//! by running a fresh single-frame stack to completion ([`Vm::call_value`]). The frame stack
//! is a local of `run`, never a field of [`Vm`], so this nesting is just ordinary Rust
//! recursion over the shared `globals`/`stdout`/`diagnostics`.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use lang_ast::{BinaryOp, Program};
use lang_backend::{Backend, RunResult};
use lang_bytecode::{BoolSide, Builtin, Const, Module, Op};
use lang_compiler::{Unsupported, compile};
use lang_diagnostics::{Diagnostic, DiagnosticCode};
use lang_gc::{release, retain};
use lang_object::{Shape, ShapeKind};
use lang_span::Span;
use lang_value::{Value, apply_binary, apply_unary};

/// The bytecode-VM backend.
#[derive(Debug, Clone, Default)]
pub struct VmBackend;

impl VmBackend {
    pub fn new() -> VmBackend {
        VmBackend
    }

    /// Compile and run a program, or report that it falls outside the supported subset.
    pub fn try_run(&self, program: &Program) -> Result<RunResult, Unsupported> {
        let module = compile(program)?;
        Ok(execute(&module))
    }

    /// Execute an already-compiled [`Module`]. This is the seam the salsa graph (`lang-db`)
    /// drives: it produces the `Module` via the memoized `bytecode` query, then hands it here.
    /// Splitting compilation from execution is what lets the VM "consume `chunk(db)`" (M1.1)
    /// without the VM crate depending on the database.
    pub fn run_module(&self, module: &Module) -> RunResult {
        execute(module)
    }
}

impl Backend for VmBackend {
    /// The [`Backend`] contract. The VM is only driven through [`VmBackend::try_run`] (the
    /// differential harness), so reaching this on an unsupported program is a caller bug.
    fn run(&self, program: &Program) -> RunResult {
        self.try_run(program)
            .expect("VmBackend::run on a program outside the VM subset; use try_run")
    }
}

/// One activation record: a prototype index, its register file, the program counter, the caller
/// register the return value flows into (irrelevant for the bottom/top-level frame), and an
/// optional transform applied to the return value as it lands in the caller.
struct Frame {
    proto: u32,
    regs: Vec<Value>,
    pc: usize,
    ret_dst: u16,
    ret_transform: RetTransform,
}

/// A transform applied to a frame's return value as it flows into the caller's destination
/// register. Used by operator dispatch where the called trait method's raw result needs
/// post-processing: `!=` calls `Equatable::eq` and negates the resulting `bool`; `< <= > >=` call
/// `Comparable::compare` and map the resulting `Ordering` variant to a `bool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetTransform {
    /// Pass the value through unchanged (every ordinary call/return).
    None,
    /// Negate a `bool` result (for `!=` dispatched to `eq`); a non-bool passes through.
    Negate,
    /// Map a returned `Ordering` enum to this operator's `bool` (for `< <= > >=` dispatched to
    /// `compare`); a non-`Ordering` value passes through (an ill-typed `compare`).
    Ordering(BinaryOp),
}

impl RetTransform {
    /// Map the frame's raw return value. Returns the transformed value and whether the original
    /// `v` was *replaced* (so the caller must release `v`'s keep-alive reference — the transformed
    /// result is always a fresh immediate `bool`, holding no heap reference of its own). A
    /// pass-through (`None`, or an ill-typed value the transform doesn't recognize) returns `v`
    /// unchanged with `false`, so the caller transfers `v`'s reference onward as usual.
    fn apply(self, v: Value) -> (Value, bool) {
        match self {
            RetTransform::None => (v, false),
            RetTransform::Negate => match v.as_bool() {
                Some(b) => (Value::bool(!b), true),
                None => (v, false),
            },
            RetTransform::Ordering(op) => match v.shape() {
                Some(shape) if shape.kind == ShapeKind::Enum && shape.name == "Ordering" => {
                    let variant = shape.variant.as_deref().unwrap_or("");
                    (Value::bool(op.ordering_satisfies(variant)), true)
                }
                _ => (v, false),
            },
        }
    }
}

/// Signals that a diagnostic has been recorded and execution must unwind. The diagnostic
/// itself lives on [`Vm::diagnostics`]; this is just the propagation token.
struct Abort;

/// One program's worth of execution state, shared across every (possibly re-entrant) frame
/// stack: the compiled module, the shared shape handles and instance-method table, the by-name
/// global environment, captured stdout, and the diagnostics recorded so far.
struct Vm<'m> {
    module: &'m Module,
    /// One shared `Rc<Shape>` per shape-table entry — cloned into every value of that shape,
    /// so equal-built aggregates point at one shape (identity is a pointer comparison).
    shapes: Vec<Rc<Shape>>,
    /// Instance-method dispatch: `(type_name, method)` to the method's prototype index.
    methods: HashMap<(String, String), u32>,
    /// `type_name` to its `destruct` prototype, for classes with a destructor.
    destructors: HashMap<String, u32>,
    globals: HashMap<String, Value>,
    /// Top-level binding names in declaration order, so globals are destroyed at program end
    /// in reverse declaration order (the deterministic "program order" the spec requires).
    global_order: Vec<String>,
    /// The deterministic `next_id()` counter, seeded at 1 (matching the M0 `IdGen`).
    next_id: u64,
    stdout: String,
    diagnostics: Vec<Diagnostic>,
}

/// How a value behaves under `?`/`??`: the unwrapped success payload, or the empty case.
enum TryOutcome {
    Success(Value),
    Empty,
}

/// Classify a value for `?`/`??`. Only the built-in `Result`/`Option` enums qualify; the
/// success payload is shared (not retained). Mirrors the M0 tree-walker's `try_branch`.
fn try_classify(v: Value) -> Option<TryOutcome> {
    if !v.is_enum() {
        return None;
    }
    let shape = v.shape()?;
    match (shape.name.as_str(), shape.variant.as_deref()) {
        ("Result", Some("Ok")) | ("Option", Some("some")) => {
            let inner = v
                .enum_data()
                .and_then(|d| d.into_iter().next())
                .unwrap_or_else(Value::unit);
            Some(TryOutcome::Success(inner))
        }
        ("Result", Some("Err")) | ("Option", Some("none")) => Some(TryOutcome::Empty),
        _ => None,
    }
}

/// Execute a compiled module, capturing stdout, exit code, and diagnostics.
fn execute(module: &Module) -> RunResult {
    let methods = module
        .methods
        .iter()
        .map(|m| ((m.type_name.clone(), m.method.clone()), m.proto))
        .collect();
    let destructors = module.destructors.iter().cloned().collect();
    let mut vm = Vm {
        module,
        shapes: module.shapes.iter().cloned().map(Rc::new).collect(),
        methods,
        destructors,
        globals: HashMap::new(),
        global_order: Vec::new(),
        next_id: 1,
        stdout: String::new(),
        diagnostics: Vec::new(),
    };
    let top = Frame {
        proto: 0,
        regs: vec![Value::unit(); module.main().num_registers as usize],
        pc: 0,
        ret_dst: 0,
        ret_transform: RetTransform::None,
    };
    // The top-level frame's `Return`/`Halt` yields the program's (discarded) value; release
    // it. On abort `run` has already released every frame register.
    if let Ok(v) = vm.run(vec![top]) {
        release(v);
    }
    // Destroy the globals at program end in reverse declaration order, running each
    // destructor on its last reference — the deterministic destruction the spec requires.
    for name in vm.global_order.clone().into_iter().rev() {
        if let Some(v) = vm.globals.get(&name).copied() {
            vm.release_value(v);
        }
    }

    let exit_code = if vm.diagnostics.is_empty() { 0 } else { 1 };
    RunResult {
        stdout: vm.stdout,
        exit_code,
        diagnostics: vm.diagnostics,
    }
}

impl<'m> Vm<'m> {
    /// Record a runtime diagnostic and produce the unwind token.
    fn error(&mut self, code: DiagnosticCode, span: Span, message: String) -> Abort {
        self.diagnostics
            .push(Diagnostic::error(code, span, message));
        Abort
    }

    /// Release a value that may be the *last* reference to a destructor-carrying object. If so,
    /// the `destruct` block runs synchronously (with the instance's fields in scope) before the
    /// object is freed — the deterministic destruction the spec requires. Used at the global
    /// drop points (reassignment and program end); ordinary register releases use the plain
    /// `release`, since a corpus destructible only ever lives in a global (its count never
    /// reaches zero elsewhere).
    fn release_value(&mut self, value: Value) {
        if value.is_object()
            && value.refcount() == 1
            && let Some(shape) = value.shape()
            && let Some(&proto) = self.destructors.get(&shape.name)
        {
            self.run_destructor(proto, value);
        }
        release(value);
    }

    /// Run an instance's `destruct` block on a fresh frame stack, with the instance in
    /// register 0 (so its fields resolve like a method's). The instance is retained for the
    /// duration, so the block sees a live object and the net reference count is unchanged —
    /// the caller's subsequent `release` performs the actual free.
    fn run_destructor(&mut self, proto: u32, instance: Value) {
        let chunk = &self.module.protos[proto as usize];
        let mut regs = vec![Value::unit(); chunk.num_registers as usize];
        retain(instance);
        regs[0] = instance;
        let frame = Frame {
            proto,
            regs,
            pc: 0,
            ret_dst: 0,
            ret_transform: RetTransform::None,
        };
        // A destructor returns unit (its body is run for its effects); discard it. An abort
        // inside a destructor has already recorded its diagnostic.
        if let Ok(v) = self.run(vec![frame]) {
            release(v);
        }
    }

    /// Run a frame stack until its bottom frame returns (`Return`) or the program/function
    /// halts (an implicit unit return). Returns the produced value, which the caller owns.
    /// On abort, every register still owned by a frame left on the stack is released here.
    fn run(&mut self, mut frames: Vec<Frame>) -> Result<Value, Abort> {
        let result = self.dispatch(&mut frames);
        if result.is_err() {
            for frame in &frames {
                for r in &frame.regs {
                    release(*r);
                }
            }
        }
        result
    }

    /// The dispatch loop. Returns `Ok(value)` once the bottom frame returns (the stack is
    /// then empty), or `Err(Abort)` with the stack left intact for [`Vm::run`] to release.
    fn dispatch(&mut self, frames: &mut Vec<Frame>) -> Result<Value, Abort> {
        // Copy the shared module reference out so the loop can index prototypes without
        // borrowing `self` — leaving `self.stdout`/`globals`/`diagnostics` free to mutate.
        let module = self.module;
        loop {
            let top = frames.len() - 1;
            let chunk = &module.protos[frames[top].proto as usize];
            let pc = frames[top].pc;
            // Every prototype ends with `Halt`, so the pc never runs off the end; guard anyway.
            let Some(op) = chunk.code.get(pc) else {
                return Ok(Value::unit());
            };
            match op {
                Op::LoadConst { dst, k } => {
                    let v = materialize(&chunk.consts[*k as usize]);
                    set_reg(&mut frames[top].regs, *dst, v);
                    frames[top].pc += 1;
                }
                Op::Move { dst, src } => {
                    let v = frames[top].regs[*src as usize];
                    retain(v);
                    set_reg(&mut frames[top].regs, *dst, v);
                    frames[top].pc += 1;
                }
                Op::LoadGlobal { dst, name, span } => match self.globals.get(name) {
                    Some(&v) => {
                        retain(v);
                        set_reg(&mut frames[top].regs, *dst, v);
                        frames[top].pc += 1;
                    }
                    None => {
                        return Err(self.error(
                            DiagnosticCode::UnknownName,
                            *span,
                            format!("cannot find `{name}` in this scope"),
                        ));
                    }
                },
                Op::StoreGlobal { name, src } => {
                    // Transfer ownership from the (dead) source temporary into the global,
                    // rather than retaining a duplicate. This keeps the reference count equal
                    // to the tree-walker's direct-binding model — a lingering temporary would
                    // otherwise inflate the count and hide a reassigned value's last reference,
                    // suppressing its destructor.
                    let v = std::mem::replace(&mut frames[top].regs[*src as usize], Value::unit());
                    match self.globals.insert(name.clone(), v) {
                        // Reassigning a global: the previous value is dropped here, running its
                        // destructor if this was its last reference.
                        Some(old) => self.release_value(old),
                        // First binding of this name: record it for reverse-order destruction.
                        None => self.global_order.push(name.clone()),
                    }
                    frames[top].pc += 1;
                }
                Op::MakeClosure { dst, proto } => {
                    let v = Value::closure(*proto);
                    set_reg(&mut frames[top].regs, *dst, v);
                    frames[top].pc += 1;
                }
                Op::MakeList { dst, items } => {
                    let mut elements = Vec::with_capacity(items.len());
                    for &r in items.iter() {
                        let v = frames[top].regs[r as usize];
                        retain(v);
                        elements.push(v);
                    }
                    set_reg(&mut frames[top].regs, *dst, Value::list(elements));
                    frames[top].pc += 1;
                }
                Op::MakeMap { dst, entries } => {
                    let mut map = BTreeMap::new();
                    for (key_reg, value_reg) in entries.iter() {
                        let key = frames[top].regs[*key_reg as usize]
                            .as_string()
                            .expect("map keys are validated by RequireMapKey");
                        let value = frames[top].regs[*value_reg as usize];
                        retain(value);
                        // A duplicate key keeps the later value (M0 `BTreeMap` semantics); the
                        // displaced value loses its owner, so release it.
                        if let Some(old) = map.insert(key, value) {
                            release(old);
                        }
                    }
                    set_reg(&mut frames[top].regs, *dst, Value::map(map));
                    frames[top].pc += 1;
                }
                Op::RequireMapKey { reg, span } => {
                    let v = frames[top].regs[*reg as usize];
                    if v.as_string().is_none() {
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!("map keys must be strings, found {}", v.type_name()),
                        ));
                    }
                    frames[top].pc += 1;
                }
                Op::IterSnapshot { dst, src, span } => {
                    let v = frames[top].regs[*src as usize];
                    // Snapshot the elements to iterate (a list's elements, or a map's values
                    // in sorted-key order), each retained so the loop owns them independently.
                    let snapshot = match v.list_items().or_else(|| v.map_values()) {
                        Some(elements) => {
                            for &e in &elements {
                                retain(e);
                            }
                            Value::list(elements)
                        }
                        None => {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("cannot iterate over {}", v.type_name()),
                            ));
                        }
                    };
                    set_reg(&mut frames[top].regs, *dst, snapshot);
                    frames[top].pc += 1;
                }
                Op::ListLen { dst, src } => {
                    let n = frames[top].regs[*src as usize]
                        .list_len()
                        .expect("ListLen operates on an iteration snapshot");
                    set_reg(&mut frames[top].regs, *dst, Value::int(n as i64));
                    frames[top].pc += 1;
                }
                Op::ListGet { dst, list, index } => {
                    let idx = frames[top].regs[*index as usize]
                        .as_int()
                        .expect("a loop index is an int") as usize;
                    let element = frames[top].regs[*list as usize]
                        .list_get(idx)
                        .expect("the loop keeps the index in bounds");
                    retain(element);
                    set_reg(&mut frames[top].regs, *dst, element);
                    frames[top].pc += 1;
                }
                Op::DestructurePair {
                    first,
                    second,
                    src,
                    span,
                } => {
                    let v = frames[top].regs[*src as usize];
                    match v.list_items() {
                        Some(items) if items.len() == 2 => {
                            let (a, b) = (items[0], items[1]);
                            retain(a);
                            retain(b);
                            set_reg(&mut frames[top].regs, *first, a);
                            set_reg(&mut frames[top].regs, *second, b);
                            frames[top].pc += 1;
                        }
                        _ => {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "destructuring `(a, b)` expects a 2-element list, found {}",
                                    v.type_name()
                                ),
                            ));
                        }
                    }
                }
                Op::CallBuiltin {
                    dst,
                    builtin,
                    args,
                    span,
                } => {
                    // Builtins borrow their arguments (the registers keep ownership); the
                    // result is a fresh owned value.
                    let arg_vals: Vec<Value> =
                        args.iter().map(|r| frames[top].regs[*r as usize]).collect();
                    let (dst, builtin, span) = (*dst, *builtin, *span);
                    let v = self.call_builtin(builtin, &arg_vals, span)?;
                    set_reg(&mut frames[top].regs, dst, v);
                    frames[top].pc += 1;
                }
                Op::CallMethod {
                    dst,
                    recv,
                    method,
                    args,
                    span,
                } => {
                    let v = frames[top].regs[*recv as usize];
                    // An object dispatches to a user method through the type's method table;
                    // anything else falls to the built-in `count`/`enumerate` methods.
                    if v.is_object() {
                        let type_name = v.shape().unwrap().name.clone();
                        let Some(&proto) = self.methods.get(&(type_name.clone(), method.clone()))
                        else {
                            return Err(self.error(
                                DiagnosticCode::UnknownName,
                                *span,
                                format!("type `{type_name}` has no method `{method}`"),
                            ));
                        };
                        let chunk = &module.protos[proto as usize];
                        // The prototype takes the receiver in register 0 and the user arguments
                        // after it, so its declared arity is one more than the supplied args.
                        if args.len() + 1 != chunk.num_params as usize {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "this method takes {} argument(s) but {} were supplied",
                                    chunk.num_params - 1,
                                    args.len()
                                ),
                            ));
                        }
                        let mut new_regs = vec![Value::unit(); chunk.num_registers as usize];
                        retain(v);
                        new_regs[0] = v;
                        for (i, &arg_reg) in args.iter().enumerate() {
                            let a = frames[top].regs[arg_reg as usize];
                            retain(a);
                            new_regs[i + 1] = a;
                        }
                        frames[top].pc += 1;
                        frames.push(Frame {
                            proto,
                            regs: new_regs,
                            pc: 0,
                            ret_dst: *dst,
                            ret_transform: RetTransform::None,
                        });
                        continue;
                    }
                    // `x.compare(y)` — the `Ordering` of two primitives (the value a `Comparable`
                    // impl returns). One argument, on any non-object receiver.
                    if method == "compare" {
                        if args.len() != 1 {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "method `compare` takes 1 argument but {} were supplied",
                                    args.len()
                                ),
                            ));
                        }
                        let other = frames[top].regs[args[0] as usize];
                        match compare_primitive(v, other) {
                            Some(ordering) => {
                                let value = make_ordering(lang_ast::ordering_variant(ordering));
                                set_reg(&mut frames[top].regs, *dst, value);
                                frames[top].pc += 1;
                            }
                            None => {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!(
                                        "cannot compare {} and {}",
                                        v.type_name(),
                                        other.type_name()
                                    ),
                                ));
                            }
                        }
                        continue;
                    }
                    // Built-in zero-argument methods on lists/maps/strings.
                    let result = if !args.is_empty() {
                        None
                    } else if method == "count" {
                        v.list_len()
                            .or_else(|| v.map_len())
                            .or_else(|| v.as_string().map(|s| s.chars().count()))
                            .map(|n| Value::int(n as i64))
                    } else if method == "enumerate" {
                        v.list_items().map(|items| {
                            let pairs = items
                                .iter()
                                .enumerate()
                                .map(|(i, &element)| {
                                    retain(element);
                                    Value::list(vec![Value::int(i as i64), element])
                                })
                                .collect();
                            Value::list(pairs)
                        })
                    } else {
                        None
                    };
                    match result {
                        Some(value) => {
                            set_reg(&mut frames[top].regs, *dst, value);
                            frames[top].pc += 1;
                        }
                        None if !args.is_empty()
                            && (method == "count" || method == "enumerate") =>
                        {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("method `{method}` takes no arguments"),
                            ));
                        }
                        None => {
                            return Err(self.error(
                                DiagnosticCode::UnknownName,
                                *span,
                                format!("no method `{method}` on {}", v.type_name()),
                            ));
                        }
                    }
                }
                Op::MakeRecord {
                    dst,
                    shape,
                    named,
                    spread,
                    span,
                } => {
                    let shape = self.shapes[*shape as usize].clone();
                    let mut slots: Vec<Option<Value>> = vec![None; shape.fields.len()];
                    // `..base` fills declared slots the base provides; named initializers then
                    // override. A slot left unset by both is a missing-field error (E0009).
                    if let Some(base_reg) = spread {
                        let base = frames[top].regs[*base_reg as usize];
                        for (i, field) in shape.fields.iter().enumerate() {
                            if let Some(value) = base.field(field) {
                                retain(value);
                                slots[i] = Some(value);
                            }
                        }
                    }
                    for (slot, reg) in named.iter() {
                        let value = frames[top].regs[*reg as usize];
                        retain(value);
                        if let Some(old) = slots[*slot as usize].replace(value) {
                            release(old);
                        }
                    }
                    let missing: Vec<&str> = shape
                        .fields
                        .iter()
                        .zip(&slots)
                        .filter(|(_, slot)| slot.is_none())
                        .map(|(name, _)| name.as_str())
                        .collect();
                    if !missing.is_empty() {
                        for slot in slots.into_iter().flatten() {
                            release(slot);
                        }
                        let list = missing
                            .iter()
                            .map(|name| format!("`{name}`"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        return Err(self.error(
                            DiagnosticCode::MissingField,
                            *span,
                            format!(
                                "missing field(s) {list} in `{}` literal — every field must be set",
                                shape.name
                            ),
                        ));
                    }
                    let slots = slots.into_iter().map(Option::unwrap).collect();
                    set_reg(&mut frames[top].regs, *dst, Value::object(shape, slots));
                    frames[top].pc += 1;
                }
                Op::MakeOpaque {
                    dst,
                    type_name,
                    keys,
                    spread,
                } => {
                    // An opaque object's shape is built from its (spread ∪ named) keys in sorted
                    // order, so its display matches the tree-walker's `BTreeMap` field bag.
                    let mut bag: BTreeMap<String, Value> = BTreeMap::new();
                    if let Some(base_reg) = spread
                        && let Some(base) = frames[top].regs[*base_reg as usize].shape()
                    {
                        let base_val = frames[top].regs[*base_reg as usize];
                        for (i, field) in base.fields.iter().enumerate() {
                            let value = base_val.slots().unwrap()[i];
                            retain(value);
                            if let Some(old) = bag.insert(field.clone(), value) {
                                release(old);
                            }
                        }
                    }
                    for (key, reg) in keys.iter() {
                        let value = frames[top].regs[*reg as usize];
                        retain(value);
                        if let Some(old) = bag.insert(key.clone(), value) {
                            release(old);
                        }
                    }
                    let fields: Vec<String> = bag.keys().cloned().collect();
                    let slots: Vec<Value> = bag.into_values().collect();
                    let shape =
                        Rc::new(Shape::object(ShapeKind::Opaque, type_name.clone(), fields));
                    set_reg(&mut frames[top].regs, *dst, Value::object(shape, slots));
                    frames[top].pc += 1;
                }
                Op::MakeEnum { dst, shape, args } => {
                    let shape = self.shapes[*shape as usize].clone();
                    let mut data = Vec::with_capacity(args.len());
                    for &r in args.iter() {
                        let v = frames[top].regs[r as usize];
                        retain(v);
                        data.push(v);
                    }
                    set_reg(&mut frames[top].regs, *dst, Value::enum_value(shape, data));
                    frames[top].pc += 1;
                }
                Op::LoadField {
                    dst,
                    obj,
                    field,
                    span,
                } => {
                    let v = frames[top].regs[*obj as usize];
                    match v.field(field) {
                        Some(value) => {
                            retain(value);
                            set_reg(&mut frames[top].regs, *dst, value);
                            frames[top].pc += 1;
                        }
                        None if v.is_object() => {
                            return Err(self.error(
                                DiagnosticCode::UnknownName,
                                *span,
                                format!(
                                    "type `{}` has no field `{field}`",
                                    v.shape().unwrap().name
                                ),
                            ));
                        }
                        None => {
                            return Err(self.error(
                                DiagnosticCode::UnknownName,
                                *span,
                                format!("no field `{field}` on {}", v.type_name()),
                            ));
                        }
                    }
                }
                Op::NextId { dst } => {
                    let id = self.next_id;
                    self.next_id += 1;
                    set_reg(&mut frames[top].regs, *dst, Value::int(id as i64));
                    frames[top].pc += 1;
                }
                Op::Panic { msg, span } => {
                    let message = frames[top].regs[*msg as usize].display();
                    return Err(self.error(
                        DiagnosticCode::Panic,
                        *span,
                        format!("panic: {message}"),
                    ));
                }
                Op::TryUnwrap { dst, src, span } => {
                    let v = frames[top].regs[*src as usize];
                    match try_classify(v) {
                        Some(TryOutcome::Success(inner)) => {
                            retain(inner);
                            set_reg(&mut frames[top].regs, *dst, inner);
                            frames[top].pc += 1;
                        }
                        // `Err(_)`/`none`: early-return the whole value from this frame, exactly
                        // as `Op::Return` does (the M0 `Unwind::Return`).
                        Some(TryOutcome::Empty) => {
                            retain(v);
                            let finished = frames.pop().unwrap();
                            for r in &finished.regs {
                                release(*r);
                            }
                            // Apply the frame's return transform on every exit path, for the same
                            // reason `Op::Return` does (a short-circuiting `?` is an early return);
                            // release the original if the transform replaced it.
                            let (out, replaced) = finished.ret_transform.apply(v);
                            if replaced {
                                release(v);
                            }
                            match frames.last_mut() {
                                Some(caller) => {
                                    let dst = finished.ret_dst as usize;
                                    let old = caller.regs[dst];
                                    caller.regs[dst] = out;
                                    release(old);
                                }
                                None => return Ok(out),
                            }
                        }
                        None => {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "`?` expects a `Result` or `Option`, found {}",
                                    v.type_name()
                                ),
                            ));
                        }
                    }
                }
                Op::Coalesce {
                    dst,
                    src,
                    fallback,
                    span,
                } => {
                    let v = frames[top].regs[*src as usize];
                    match try_classify(v) {
                        Some(TryOutcome::Success(inner)) => {
                            retain(inner);
                            set_reg(&mut frames[top].regs, *dst, inner);
                            frames[top].pc += 1;
                        }
                        // Empty: jump to the fallback expression (which writes `dst`).
                        Some(TryOutcome::Empty) => frames[top].pc = *fallback as usize,
                        None => {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "`??` expects a `Result` or `Option` on the left, found {}",
                                    v.type_name()
                                ),
                            ));
                        }
                    }
                }
                Op::MatchInt { src, value, fail } => {
                    if frames[top].regs[*src as usize].as_int() == Some(*value) {
                        frames[top].pc += 1;
                    } else {
                        frames[top].pc = *fail as usize;
                    }
                }
                Op::MatchStr { src, value, fail } => {
                    if frames[top].regs[*src as usize].as_string().as_deref() == Some(value) {
                        frames[top].pc += 1;
                    } else {
                        frames[top].pc = *fail as usize;
                    }
                }
                Op::MatchBool { src, value, fail } => {
                    if frames[top].regs[*src as usize].as_bool() == Some(*value) {
                        frames[top].pc += 1;
                    } else {
                        frames[top].pc = *fail as usize;
                    }
                }
                Op::MatchVariant {
                    src,
                    type_name,
                    variant,
                    arity,
                    fail,
                } => {
                    let v = frames[top].regs[*src as usize];
                    let matches = v.is_enum()
                        && v.shape().is_some_and(|shape| {
                            shape.variant.as_deref() == Some(variant)
                                && type_name.as_ref().is_none_or(|t| &shape.name == t)
                        })
                        && v.enum_data().is_some_and(|d| d.len() == *arity as usize);
                    if matches {
                        frames[top].pc += 1;
                    } else {
                        frames[top].pc = *fail as usize;
                    }
                }
                Op::ExtractField { dst, src, index } => {
                    let element =
                        frames[top].regs[*src as usize].enum_data().unwrap()[*index as usize];
                    retain(element);
                    set_reg(&mut frames[top].regs, *dst, element);
                    frames[top].pc += 1;
                }
                Op::MatchFail { src, span } => {
                    let shown = frames[top].regs[*src as usize].display();
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!("no match arm matched the value {shown}"),
                    ));
                }
                Op::Unary { op, dst, src, span } => {
                    match apply_unary(*op, frames[top].regs[*src as usize]) {
                        Ok(v) => {
                            set_reg(&mut frames[top].regs, *dst, v);
                            frames[top].pc += 1;
                        }
                        Err(e) => return Err(self.error(e.code, *span, e.text)),
                    }
                }
                Op::Binary {
                    op,
                    dst,
                    a,
                    b,
                    span,
                } => {
                    let left = frames[top].regs[*a as usize];
                    let right = frames[top].regs[*b as usize];
                    // Operator-trait dispatch on a user object: an arithmetic/concat operator
                    // routes to its trait method and uses the result directly; `==`/`!=` route to
                    // `Equatable::eq` (`!=` negating the bool via the frame's return transform).
                    // Built-in semantics apply otherwise. The checker guarantees a dispatched
                    // method's arity (receiver + 1); a mismatch falls through to the built-in path.
                    let dispatch = if left.is_object() {
                        let type_name = left.shape().unwrap().name.clone();
                        if let Some(method_name) = op.overload_method() {
                            self.methods
                                .get(&(type_name, method_name.to_string()))
                                .map(|&proto| (proto, RetTransform::None))
                        } else if let Some(negate) = op.equatable_negation() {
                            let transform = if negate {
                                RetTransform::Negate
                            } else {
                                RetTransform::None
                            };
                            self.methods
                                .get(&(type_name, "eq".to_string()))
                                .map(|&proto| (proto, transform))
                        } else if let Some(method_name) = op.comparable_method() {
                            self.methods
                                .get(&(type_name, method_name.to_string()))
                                .map(|&proto| (proto, RetTransform::Ordering(*op)))
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    if let Some((proto, transform)) = dispatch
                        && module.protos[proto as usize].num_params == 2
                    {
                        let chunk = &module.protos[proto as usize];
                        let mut new_regs = vec![Value::unit(); chunk.num_registers as usize];
                        retain(left);
                        new_regs[0] = left;
                        retain(right);
                        new_regs[1] = right;
                        frames[top].pc += 1;
                        frames.push(Frame {
                            proto,
                            regs: new_regs,
                            pc: 0,
                            ret_dst: *dst,
                            ret_transform: transform,
                        });
                        continue;
                    }
                    match apply_binary(*op, left, right) {
                        Ok(v) => {
                            set_reg(&mut frames[top].regs, *dst, v);
                            frames[top].pc += 1;
                        }
                        Err(e) => return Err(self.error(e.code, *span, e.text)),
                    }
                }
                Op::RequireBool {
                    reg,
                    side,
                    op,
                    span,
                } => {
                    let v = frames[top].regs[*reg as usize];
                    if v.as_bool().is_none() {
                        let where_ = match side {
                            BoolSide::Left => "left",
                            BoolSide::Right => "right",
                        };
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!(
                                "`{}` expects a bool on the {where_}, found {}",
                                op.symbol(),
                                v.type_name()
                            ),
                        ));
                    }
                    frames[top].pc += 1;
                }
                Op::RequireCondBool { reg, span } => {
                    let v = frames[top].regs[*reg as usize];
                    if v.as_bool().is_none() {
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!("`if` condition must be a bool, found {}", v.type_name()),
                        ));
                    }
                    frames[top].pc += 1;
                }
                Op::Jump { target } => {
                    frames[top].pc = *target as usize;
                }
                Op::JumpIfTrue { reg, target } => {
                    if frames[top].regs[*reg as usize].as_bool() == Some(true) {
                        frames[top].pc = *target as usize;
                    } else {
                        frames[top].pc += 1;
                    }
                }
                Op::JumpIfFalse { reg, target } => {
                    if frames[top].regs[*reg as usize].as_bool() == Some(false) {
                        frames[top].pc = *target as usize;
                    } else {
                        frames[top].pc += 1;
                    }
                }
                Op::Echo { reg } => {
                    let text = frames[top].regs[*reg as usize].display();
                    self.stdout.push_str(&text);
                    self.stdout.push('\n');
                    frames[top].pc += 1;
                }
                Op::Raise { idx } => {
                    self.diagnostics
                        .push(chunk.diagnostics[*idx as usize].clone());
                    return Err(Abort);
                }
                Op::Call {
                    dst,
                    callee,
                    args,
                    span,
                } => {
                    let callee_val = frames[top].regs[*callee as usize];
                    match callee_val.as_closure() {
                        Some(proto_idx) => {
                            let callee_chunk = &module.protos[proto_idx as usize];
                            if args.len() != callee_chunk.num_params as usize {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!(
                                        "this function takes {} argument(s) but {} were supplied",
                                        callee_chunk.num_params,
                                        args.len()
                                    ),
                                ));
                            }
                            // Move the arguments into the new frame's leading registers, each
                            // owning a fresh reference.
                            let mut new_regs =
                                vec![Value::unit(); callee_chunk.num_registers as usize];
                            for (i, &arg_reg) in args.iter().enumerate() {
                                let v = frames[top].regs[arg_reg as usize];
                                retain(v);
                                new_regs[i] = v;
                            }
                            // Resume after the call once the callee returns.
                            frames[top].pc += 1;
                            frames.push(Frame {
                                proto: proto_idx,
                                regs: new_regs,
                                pc: 0,
                                ret_dst: *dst,
                                ret_transform: RetTransform::None,
                            });
                        }
                        None => {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("{} is not callable", callee_val.type_name()),
                            ));
                        }
                    }
                }
                Op::Return { src } => {
                    let raw = frames[top].regs[*src as usize];
                    retain(raw); // keep alive across this frame's teardown
                    let finished = frames.pop().unwrap();
                    for r in &finished.regs {
                        release(*r);
                    }
                    // An operator-dispatch frame may post-process its result (`!=` negates `eq`'s
                    // bool; `< <= > >=` map `compare`'s `Ordering`). When the transform replaces a
                    // heap value (an `Ordering`) with a fresh `bool`, release the original's
                    // keep-alive reference so it is not leaked.
                    let (v, replaced) = finished.ret_transform.apply(raw);
                    if replaced {
                        release(raw);
                    }
                    match frames.last_mut() {
                        Some(caller) => {
                            // Transfer the retained reference into the caller's destination.
                            let dst = finished.ret_dst as usize;
                            let old = caller.regs[dst];
                            caller.regs[dst] = v;
                            release(old);
                        }
                        // The bottom frame returned: hand the value to `run`'s caller.
                        None => return Ok(v),
                    }
                }
                Op::Halt => {
                    let finished = frames.pop().unwrap();
                    for r in &finished.regs {
                        release(*r);
                    }
                    match frames.last_mut() {
                        // A non-bottom frame falling off the end implicitly returns unit.
                        Some(caller) => set_reg(&mut caller.regs, finished.ret_dst, Value::unit()),
                        // The bottom frame halted: the program (or re-entrant call) ends.
                        None => return Ok(Value::unit()),
                    }
                }
            }
        }
    }

    /// Call a value with already-owned arguments (each carrying one reference transferred to
    /// the callee), re-entering the VM on a fresh frame stack. Only closures are callable in
    /// this slice — builtins are never first-class values. Used by `map`/`filter`.
    fn call_value(&mut self, callee: Value, args: Vec<Value>, span: Span) -> Result<Value, Abort> {
        match callee.as_closure() {
            Some(proto) => {
                let chunk = &self.module.protos[proto as usize];
                let (num_params, num_registers) = (chunk.num_params, chunk.num_registers);
                if args.len() != num_params as usize {
                    let supplied = args.len();
                    for a in args {
                        release(a);
                    }
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "this function takes {num_params} argument(s) but {supplied} were supplied"
                        ),
                    ));
                }
                let mut regs = vec![Value::unit(); num_registers as usize];
                for (i, v) in args.into_iter().enumerate() {
                    regs[i] = v;
                }
                self.run(vec![Frame {
                    proto,
                    regs,
                    pc: 0,
                    ret_dst: 0,
                    ret_transform: RetTransform::None,
                }])
            }
            None => {
                let type_name = callee.type_name();
                for a in args {
                    release(a);
                }
                Err(self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!("{type_name} is not callable"),
                ))
            }
        }
    }

    /// Dispatch a prelude collection builtin. Arguments are borrowed (their registers retain
    /// ownership); the returned value is freshly owned.
    fn call_builtin(
        &mut self,
        builtin: Builtin,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        match builtin {
            Builtin::Len => {
                self.check_arity(builtin, args, 1, span)?;
                let v = args[0];
                match v
                    .list_len()
                    .or_else(|| v.map_len())
                    .or_else(|| v.as_string().map(|s| s.chars().count()))
                {
                    Some(n) => Ok(Value::int(n as i64)),
                    None => Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "`len` expects a list, map, or string, found {}",
                            v.type_name()
                        ),
                    )),
                }
            }
            Builtin::Map => {
                self.check_arity(builtin, args, 2, span)?;
                let Some(items) = args[0].list_items() else {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`map` expects a list, found {}", args[0].type_name()),
                    ));
                };
                let func = args[1];
                let mut result = Vec::with_capacity(items.len());
                for element in items {
                    retain(element); // transferred into the call
                    match self.call_value(func, vec![element], span) {
                        Ok(v) => result.push(v),
                        Err(abort) => {
                            for r in &result {
                                release(*r);
                            }
                            return Err(abort);
                        }
                    }
                }
                Ok(Value::list(result))
            }
            Builtin::Filter => {
                self.check_arity(builtin, args, 2, span)?;
                let Some(items) = args[0].list_items() else {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`filter` expects a list, found {}", args[0].type_name()),
                    ));
                };
                let func = args[1];
                let mut result = Vec::new();
                for element in items {
                    retain(element); // transferred into the call
                    let verdict = match self.call_value(func, vec![element], span) {
                        Ok(v) => v,
                        Err(abort) => {
                            for r in &result {
                                release(*r);
                            }
                            return Err(abort);
                        }
                    };
                    match verdict.as_bool() {
                        Some(true) => {
                            retain(element); // the result list now owns it too
                            result.push(element);
                        }
                        Some(false) => {}
                        None => {
                            let type_name = verdict.type_name();
                            release(verdict);
                            for r in &result {
                                release(*r);
                            }
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                span,
                                format!("`filter` predicate must return a bool, found {type_name}"),
                            ));
                        }
                    }
                    release(verdict); // the bool verdict (an immediate) is no longer needed
                }
                Ok(Value::list(result))
            }
            Builtin::Sum => {
                self.check_arity(builtin, args, 1, span)?;
                let Some(items) = args[0].list_items() else {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`sum` expects a list, found {}", args[0].type_name()),
                    ));
                };
                let mut int_total: i64 = 0;
                let mut float_total: f64 = 0.0;
                let mut any_float = false;
                for element in items {
                    // Floats take the float path; every other numeric is an int (matching the
                    // M0 tree-walker, which distinguishes `3` from `3.0`).
                    if let Some(f) = element.as_float() {
                        any_float = true;
                        float_total += f;
                    } else if let Some(i) = element.as_int() {
                        int_total = int_total.wrapping_add(i);
                    } else {
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            span,
                            format!(
                                "`sum` expects numeric elements, found {}",
                                element.type_name()
                            ),
                        ));
                    }
                }
                Ok(if any_float {
                    Value::float(float_total + int_total as f64)
                } else {
                    Value::int(int_total)
                })
            }
        }
    }

    fn check_arity(
        &mut self,
        builtin: Builtin,
        args: &[Value],
        expected: usize,
        span: Span,
    ) -> Result<(), Abort> {
        if args.len() == expected {
            Ok(())
        } else {
            Err(self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`{}` takes {expected} argument(s) but {} were supplied",
                    builtin.name(),
                    args.len()
                ),
            ))
        }
    }
}

/// Overwrite a register, releasing the value it held.
fn set_reg(regs: &mut [Value], dst: u16, value: Value) {
    let old = regs[dst as usize];
    regs[dst as usize] = value;
    release(old);
}

/// Build a built-in `Ordering` enum value (`Ordering.Less`/`Equal`/`Greater`) with a fresh shape.
/// Shapes carry no identity for matching or equality (both compare by name + variant), so an
/// on-the-fly shape is interchangeable with any other `Ordering` shape — including the
/// tree-walker's, which is what keeps the differential identical.
fn make_ordering(variant: &str) -> Value {
    let shape = Rc::new(Shape::enum_variant("Ordering", variant, Vec::new(), false));
    Value::enum_value(shape, Vec::new())
}

/// The total order of two primitives for `x.compare(y)`: integers compare exactly, strings
/// lexically, and any other numeric pairing as `f64`. `None` when the operands are not comparable
/// (different non-numeric kinds, or a `NaN` float). Mirrors the tree-walker's `compare_primitive`.
fn compare_primitive(left: Value, right: Value) -> Option<std::cmp::Ordering> {
    let int_operand = |v: Value| {
        if v.as_float().is_some() {
            None
        } else {
            v.as_int()
        }
    };
    if let (Some(a), Some(b)) = (int_operand(left), int_operand(right)) {
        return Some(a.cmp(&b));
    }
    if let (Some(a), Some(b)) = (left.as_string(), right.as_string()) {
        return Some(a.cmp(&b));
    }
    let num = |v: Value| v.as_float().or_else(|| v.as_int().map(|i| i as f64));
    num(left)?.partial_cmp(&num(right)?)
}

/// Turn a compile-time constant into a freshly-owned runtime value.
fn materialize(c: &Const) -> Value {
    match c {
        Const::Unit => Value::unit(),
        Const::Bool(b) => Value::bool(*b),
        Const::Int(i) => Value::int(*i),
        Const::Float(f) => Value::float(*f),
        Const::Str(s) => Value::string(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lang_lexer::lex;
    use lang_parser::parse;
    use lang_span::{Source, SourceId};

    fn run(src: &str) -> RunResult {
        let source = Source::new(SourceId::FIRST, "test.lang", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        VmBackend::new()
            .try_run(&parsed.program)
            .expect("program should be in the M1.0 subset")
    }

    #[test]
    fn arithmetic_and_concat() {
        let r = run("echo 1 + 2 * 3;\necho \"users/\" ~ 42 ~ \"/profile\";\n");
        assert_eq!(r.stdout, "7\nusers/42/profile\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn integer_wrapping_matches_i64() {
        let r = run("echo 9223372036854775807 + 1;\necho 9223372036854775807 * 2;\n");
        assert_eq!(r.stdout, "-9223372036854775808\n-2\n");
    }

    #[test]
    fn mutable_reassignment() {
        let r = run("mut total = 0;\ntotal = total + 5;\necho total;\n");
        assert_eq!(r.stdout, "5\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn immutable_reassignment_is_e0006() {
        let r = run("name = \"a\";\nname = \"b\";\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(r.diagnostics.len(), 1);
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::ImmutableAssignment
        );
    }

    #[test]
    fn functions_calls_and_nested_calls() {
        let r = run(
            "fn add(a, b) { return a + b; }\nfn dbl(n) { return n * 2; }\nfn quad(n) { return dbl(dbl(n)); }\necho add(2, 3);\necho quad(3);\n",
        );
        assert_eq!(r.stdout, "5\n12\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn recursion_through_globals() {
        let r = run(
            "fn fib(n) {\n  if n < 2 { return n; }\n  return fib(n - 1) + fib(n - 2);\n}\necho fib(10);\n",
        );
        assert_eq!(r.stdout, "55\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn closure_captures_global() {
        let r = run("base = 100;\nadd_base = fn(x) => x + base;\necho add_base(5);\n");
        assert_eq!(r.stdout, "105\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn pipeline_threads_first_argument() {
        let r = run(
            "fn inc(n) { return n + 1; }\nfn add(a, b) { return a + b; }\necho 5 |> inc |> inc;\necho 5 |> add(10);\n",
        );
        assert_eq!(r.stdout, "7\n15\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn parameter_shadows_global() {
        let r = run("base = 100;\nfn f(base) { return base; }\necho f(5);\necho base;\n");
        assert_eq!(r.stdout, "5\n100\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn arity_mismatch_is_type_error() {
        let r = run("fn add(a, b) { return a + b; }\necho add(1);\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::TypeMismatch
        );
    }

    #[test]
    fn implicit_unit_return_displays_empty() {
        // A function with no `return` yields unit, which echoes as an empty line (M0 parity).
        let r = run("fn noop(x) { x + 1; }\necho noop(5);\n");
        assert_eq!(r.stdout, "\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn short_circuit_logic() {
        // `false && <error>` short-circuits to false without evaluating the right side.
        assert_eq!(run("echo false && 1 < 2;\n").stdout, "false\n");
        assert_eq!(run("echo true || 1 < 2;\n").stdout, "true\n");
        assert_eq!(run("echo 1 < 2 && 3 >= 3;\n").stdout, "true\n");
    }

    #[test]
    fn division_by_zero_is_e0008() {
        let r = run("echo 1 / 0;\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::DivisionByZero
        );
    }

    #[test]
    fn unknown_name_is_e0005() {
        let r = run("echo missing;\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::UnknownName
        );
    }

    #[test]
    fn destructors_run_at_program_end_in_reverse_declaration_order() {
        let r = run(
            "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  destruct { echo \"close {name}\"; }\n}\na = R.new(\"a\");\nb = R.new(\"b\");\necho \"body\";\n",
        );
        // Globals destroyed in reverse declaration order: b before a.
        assert_eq!(r.stdout, "body\nclose b\nclose a\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn reassigning_a_binding_destroys_the_displaced_value() {
        let r = run(
            "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  destruct { echo \"close {name}\"; }\n}\nmut x = R.new(\"first\");\nx = R.new(\"second\");\necho \"mid\";\n",
        );
        // "first" is destroyed at the reassignment; "second" at program end.
        assert_eq!(r.stdout, "close first\nmid\nclose second\n");
    }

    #[test]
    fn a_class_without_a_destructor_runs_nothing() {
        let r = run(
            "class R {\n  v: int\n  fn new(v: int): R { return R { v: v }; }\n}\nx = R.new(1);\necho \"done\";\n",
        );
        assert_eq!(r.stdout, "done\n");
    }

    #[test]
    fn record_literal_field_access_and_structural_equality() {
        let r = run(
            "type Item = { price: float, qty: int };\na = Item { price: 2.5, qty: 4 };\necho a.price;\necho a.price * a.qty;\nb = Item { price: 2.5, qty: 4 };\necho a == b;\n",
        );
        assert_eq!(r.stdout, "2.5\n10.0\ntrue\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn object_displays_as_a_literal() {
        let r = run("type Pt = { x: int, y: int };\necho Pt { x: 1, y: 2 };\n");
        assert_eq!(r.stdout, "Pt {x: 1, y: 2}\n");
    }

    #[test]
    fn missing_field_is_e0009() {
        let r = run("type P = { x: int, y: int };\np = P { x: 1 };\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::MissingField
        );
    }

    #[test]
    fn class_constructor_method_and_field_access() {
        let r = run(
            "class Box {\n  v: int\n  fn new(v: int): Box { return Box { v: v }; }\n  fn doubled(): int { return v * 2; }\n}\nb = Box.new(21);\necho b.doubled();\necho b.v;\n",
        );
        assert_eq!(r.stdout, "42\n21\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn method_takes_arguments_alongside_fields() {
        let r = run(
            "class Counter {\n  base: int\n  fn new(base: int): Counter { return Counter { base: base }; }\n  fn plus(n: int): int { return base + n; }\n}\nc = Counter.new(10);\necho c.plus(5);\n",
        );
        assert_eq!(r.stdout, "15\n");
    }

    #[test]
    fn structural_update_overrides_one_field() {
        let r = run(
            "class M {\n  amount: int\n  currency: string\n  fn new(a: int, c: string): M { return M { amount: a, currency: c }; }\n}\na = M.new(500, \"USD\");\nb = M { amount: 300, ..a };\necho b.amount;\necho b.currency;\necho a.amount;\n",
        );
        assert_eq!(r.stdout, "300\nUSD\n500\n");
    }

    #[test]
    fn operator_trait_overloads_plus() {
        // `a + b` on a class implementing `Add` dispatches to its `add` method (M1.8).
        let r = run(
            "class Money {\n  amount: int\n  currency: string\n  fn new(a: int, c: string): Money { return Money { amount: a, currency: c }; }\n  impl Add {\n    fn add(other: Money): Money { return Money { amount: amount + other.amount, currency: currency }; }\n  }\n}\na = Money.new(5, \"USD\");\nb = Money.new(3, \"USD\");\nt = a + b;\necho t.amount;\necho t.currency;\n",
        );
        assert_eq!(r.stdout, "8\nUSD\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn operators_on_builtins_are_unaffected_by_overloads() {
        // A class without the relevant trait method leaves built-in `+` semantics untouched.
        let r = run("echo 2 + 3;\necho \"a\" ~ \"b\";\n");
        assert_eq!(r.stdout, "5\nab\n");
    }

    #[test]
    fn equatable_overrides_equality_and_negates_for_ne() {
        // `impl Equatable` routes `==`/`!=` to `eq`; `eq` here ignores `tag`, and `!=` negates the
        // returned bool through the frame's return transform.
        let r = run(
            "class M {\n  amount: int\n  tag: int\n  fn new(a: int, t: int): M { return M { amount: a, tag: t }; }\n  impl Equatable {\n    fn eq(other: M): bool { return amount == other.amount; }\n  }\n}\na = M.new(5, 1);\nb = M.new(5, 2);\necho a == b;\necho a != b;\necho a == M.new(9, 1);\n",
        );
        assert_eq!(r.stdout, "true\nfalse\nfalse\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn comparable_overloads_ordering_operators() {
        // `impl Comparable` routes `< <= > >=` to `compare`; the returned `Ordering` is mapped to
        // each operator's bool via the frame's return transform.
        let r = run(
            "class M {\n  amount: int\n  fn new(a: int): M { return M { amount: a }; }\n  impl Comparable {\n    fn compare(other: M): Ordering { return amount.compare(other.amount); }\n  }\n}\na = M.new(5);\nb = M.new(8);\necho a < b;\necho a > b;\necho a <= b;\necho a >= b;\n",
        );
        assert_eq!(r.stdout, "true\nfalse\ntrue\nfalse\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn primitive_compare_yields_ordering() {
        let r = run("echo 1.compare(2);\necho 5.compare(5);\necho 9.compare(2);\n");
        assert_eq!(
            r.stdout,
            "Ordering.Less\nOrdering.Equal\nOrdering.Greater\n"
        );
    }

    #[test]
    fn plain_enum_construction_and_equality() {
        let r = run("enum S { A; B; }\necho S.A == S.A;\necho S.A == S.B;\n");
        assert_eq!(r.stdout, "true\nfalse\n");
    }

    #[test]
    fn opaque_use_stub_constructs_and_reads_fields() {
        let r = run(
            "use App.Models.User;\nu = User { name: \"Ada\", id: 7 };\necho u.name;\necho u.id;\necho u;\n",
        );
        // Opaque objects display their fields in sorted-key order (M0 `BTreeMap` parity).
        assert_eq!(r.stdout, "Ada\n7\nUser {id: 7, name: \"Ada\"}\n");
    }

    #[test]
    fn match_over_enums_binds_variant_data() {
        let r = run(
            "enum E { Empty; Code(n: int); }\nx = E.Code(42);\necho match x { E.Empty => \"empty\", E.Code(n) => \"code {n}\" };\n",
        );
        assert_eq!(r.stdout, "code 42\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn match_literals_and_wildcard() {
        let r = run(
            "fn name(n) { return match n { 0 => \"zero\", 1 => \"one\", _ => \"many\" }; }\necho name(0);\necho name(5);\n",
        );
        assert_eq!(r.stdout, "zero\nmany\n");
    }

    #[test]
    fn unmatched_value_is_a_runtime_error() {
        let r = run("enum E { A; B; C; }\necho match E.C { E.A => 1, E.B => 2 };\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::TypeMismatch
        );
    }

    #[test]
    fn result_constructors_display_bare() {
        let r = run("echo Ok(5);\necho Err(\"boom\");\necho some(3);\necho none;\necho Ok();\n");
        assert_eq!(r.stdout, "Ok(5)\nErr(boom)\nsome(3)\nnone\nOk\n");
    }

    #[test]
    fn question_propagates_err_and_unwraps_ok() {
        assert_eq!(
            run("fn validate(): int { return Err(\"empty\"); }\nfn run_it(): int { validate()?; return Ok(\"done\"); }\necho run_it();\n").stdout,
            "Err(empty)\n"
        );
        assert_eq!(
            run("fn ok_val(): int { return Ok(41); }\nfn use_it(): int { return Ok(ok_val()? + 1); }\necho use_it();\n").stdout,
            "Ok(42)\n"
        );
    }

    #[test]
    fn coalesce_supplies_a_default() {
        let r =
            run("echo none ?? 99;\necho some(7) ?? 99;\necho Err(\"x\") ?? 0;\necho Ok(5) ?? 0;\n");
        assert_eq!(r.stdout, "99\n7\n0\n5\n");
    }

    #[test]
    fn panic_aborts_with_e0010_keeping_prior_output() {
        let r = run("echo \"before\";\npanic(\"boom\");\necho \"after\";\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(r.stdout, "before\n");
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::Panic
        );
    }

    #[test]
    fn next_id_is_a_deterministic_counter() {
        let r = run("echo next_id();\necho next_id();\necho next_id();\n");
        assert_eq!(r.stdout, "1\n2\n3\n");
    }

    #[test]
    fn capture_free_closure_inside_a_method_is_supported() {
        // The `fn(it) => it.price * it.qty` closure captures nothing enclosing, so it compiles
        // even though it is defined inside a method (true upvalue capture stays unsupported).
        let r = run(
            "type Item = { price: float, qty: int };\nclass Cart {\n  items: List<Item>\n  fn new(items: List<Item>): Cart { return Cart { items: items }; }\n  fn total(): float { return items |> map(fn(it) => it.price * it.qty) |> sum(); }\n}\nc = Cart.new([Item { price: 2.5, qty: 4 }, Item { price: 1.0, qty: 3 }]);\necho c.total();\n",
        );
        assert_eq!(r.stdout, "13.0\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn string_interpolation_concatenates_display_forms() {
        let r = run("name = \"Niro\";\necho \"Hello {name}\";\necho \"sum is {1 + 2 * 3}\";\n");
        assert_eq!(r.stdout, "Hello Niro\nsum is 7\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn list_literals_display_with_repr() {
        let r = run("echo [1, 2, 3];\necho [\"a\", \"b\"];\necho [];\n");
        assert_eq!(r.stdout, "[1, 2, 3]\n[\"a\", \"b\"]\n[]\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn maps_display_in_sorted_key_order() {
        let r = run("echo {\"b\": 2, \"a\": 1};\necho {\"a\": 1, \"b\": 2}.count();\n");
        assert_eq!(r.stdout, "{\"a\": 1, \"b\": 2}\n2\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn len_over_list_map_and_string() {
        let r = run(
            "echo len([1, 2, 3]);\necho len({\"a\": 1});\necho len(\"héllo\");\necho len([]);\n",
        );
        assert_eq!(r.stdout, "3\n1\n5\n0\n");
    }

    #[test]
    fn filter_map_sum_pipeline() {
        let r = run(
            "echo [1, 2, 3, 4] |> filter(fn(n) => n % 2 == 0) |> map(fn(n) => n * 10) |> sum();\n",
        );
        assert_eq!(r.stdout, "60\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn sum_promotes_to_float_when_any_element_is_float() {
        assert_eq!(run("echo sum([1, 2, 3]);\n").stdout, "6\n");
        assert_eq!(run("echo sum([1, 2.5, 3]);\n").stdout, "6.5\n");
        assert_eq!(run("echo sum([]);\n").stdout, "0\n");
    }

    #[test]
    fn for_over_list_accumulates_into_a_global() {
        let r =
            run("mut total = 0;\nfor n in [1, 2, 3, 4] {\n  total = total + n;\n}\necho total;\n");
        assert_eq!(r.stdout, "10\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn for_over_empty_list_runs_no_iterations() {
        let r = run("for x in [] { echo \"never\"; }\necho \"done\";\n");
        assert_eq!(r.stdout, "done\n");
    }

    #[test]
    fn for_pair_destructures_enumerate() {
        let r = run("for (i, x) in [\"a\", \"b\"].enumerate() {\n  echo i ~ \":\" ~ x;\n}\n");
        assert_eq!(r.stdout, "0:a\n1:b\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn for_over_map_iterates_values_in_key_order() {
        let r = run(
            "mut total = 0;\nfor v in {\"b\": 20, \"a\": 1} {\n  total = total + v;\n}\necho total;\n",
        );
        assert_eq!(r.stdout, "21\n");
    }

    #[test]
    fn iterating_a_non_collection_is_a_type_error() {
        let r = run("for x in 42 { echo x; }\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::TypeMismatch
        );
    }

    #[test]
    fn len_of_an_int_is_a_type_error() {
        let r = run("echo len(42);\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::TypeMismatch
        );
    }

    #[test]
    fn map_closure_error_propagates_and_frees() {
        // The closure divides by zero on the second element: the error must surface and the
        // partially-built result list must be freed (miri verifies no leak).
        let r = run("echo [1, 0, 2] |> map(fn(n) => 10 / n);\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::DivisionByZero
        );
    }

    #[test]
    fn nested_list_of_lists_round_trips() {
        // Exercises recursive collection freeing through the register/global machinery.
        let r = run("xs = [[1, 2], [3, 4]];\necho xs;\necho len(xs);\n");
        assert_eq!(r.stdout, "[[1, 2], [3, 4]]\n2\n");
    }

    #[test]
    fn disassembly_is_stable() {
        let source = Source::new(SourceId::FIRST, "t.lang", "mut x = 1;\necho x + 2;\n");
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        insta::assert_snapshot!(module.disassemble());
    }

    #[test]
    fn disassembly_of_a_recursive_function_is_stable() {
        let source = Source::new(
            SourceId::FIRST,
            "t.lang",
            "fn fib(n) {\n  if n < 2 { return n; }\n  return fib(n - 1) + fib(n - 2);\n}\necho fib(6);\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        insta::assert_snapshot!(module.disassemble());
    }

    #[test]
    fn disassembly_of_a_for_loop_is_stable() {
        let source = Source::new(
            SourceId::FIRST,
            "t.lang",
            "mut total = 0;\nfor n in [1, 2, 3] {\n  total = total + n;\n}\necho total;\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        insta::assert_snapshot!(module.disassemble());
    }

    #[test]
    fn disassembly_of_the_object_model_is_stable() {
        // A record literal, a class with a constructor + an instance method (showing the
        // shape and method tables, field loads, and enum construction).
        let source = Source::new(
            SourceId::FIRST,
            "t.lang",
            "enum Status { Pending; Paid; }\nclass Order {\n  id: int\n  mut status: Status\n  fn new(id: int): Order { return Order { id: id, status: Status.Pending }; }\n  fn tag(): int { return id; }\n}\no = Order.new(7);\necho o.tag();\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        insta::assert_snapshot!(module.disassemble());
    }

    #[test]
    fn disassembly_of_a_match_decision_tree_is_stable() {
        let source = Source::new(
            SourceId::FIRST,
            "t.lang",
            "enum E { Empty; Code(n: int); }\nfn describe(e): string {\n  return match e {\n    E.Empty => \"empty\",\n    E.Code(n) => \"code {n}\",\n  };\n}\necho describe(E.Code(7));\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        insta::assert_snapshot!(module.disassemble());
    }

    #[test]
    fn disassembly_of_a_question_propagating_function_is_stable() {
        let source = Source::new(
            SourceId::FIRST,
            "t.lang",
            "fn validate(): int { return Err(\"bad\"); }\nfn place(): int { validate()?; return Ok(\"ok\"); }\necho place();\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        insta::assert_snapshot!(module.disassemble());
    }

    #[test]
    fn disassembly_of_a_map_filter_chain_is_stable() {
        let source = Source::new(
            SourceId::FIRST,
            "t.lang",
            "echo [1, 2, 3, 4] |> filter(fn(n) => n % 2 == 0) |> map(fn(n) => n * 10) |> sum();\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        insta::assert_snapshot!(module.disassemble());
    }
}
