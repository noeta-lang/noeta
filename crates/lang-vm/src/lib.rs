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

use lang_ast::Program;
use lang_backend::{Backend, RunResult};
use lang_bytecode::{BoolSide, Builtin, Const, Method, Module, Op};
use lang_compiler::{Unsupported, compile};
use lang_diagnostics::{Diagnostic, DiagnosticCode};
use lang_gc::{release, retain};
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
}

impl Backend for VmBackend {
    /// The [`Backend`] contract. The VM is only driven through [`VmBackend::try_run`] (the
    /// differential harness), so reaching this on an unsupported program is a caller bug.
    fn run(&self, program: &Program) -> RunResult {
        self.try_run(program)
            .expect("VmBackend::run on a program outside the VM subset; use try_run")
    }
}

/// One activation record: a prototype index, its register file, the program counter, and the
/// caller register the return value flows into (irrelevant for the bottom/top-level frame).
struct Frame {
    proto: u32,
    regs: Vec<Value>,
    pc: usize,
    ret_dst: u16,
}

/// Signals that a diagnostic has been recorded and execution must unwind. The diagnostic
/// itself lives on [`Vm::diagnostics`]; this is just the propagation token.
struct Abort;

/// One program's worth of execution state, shared across every (possibly re-entrant) frame
/// stack: the compiled module, the by-name global environment, captured stdout, and the
/// diagnostics recorded so far.
struct Vm<'m> {
    module: &'m Module,
    globals: HashMap<String, Value>,
    stdout: String,
    diagnostics: Vec<Diagnostic>,
}

/// Execute a compiled module, capturing stdout, exit code, and diagnostics.
fn execute(module: &Module) -> RunResult {
    let mut vm = Vm {
        module,
        globals: HashMap::new(),
        stdout: String::new(),
        diagnostics: Vec::new(),
    };
    let top = Frame {
        proto: 0,
        regs: vec![Value::unit(); module.main().num_registers as usize],
        pc: 0,
        ret_dst: 0,
    };
    // The top-level frame's `Return`/`Halt` yields the program's (discarded) value; release
    // it. On abort `run` has already released every frame register.
    if let Ok(v) = vm.run(vec![top]) {
        release(v);
    }
    for v in vm.globals.values() {
        release(*v);
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
                    let v = frames[top].regs[*src as usize];
                    retain(v);
                    if let Some(old) = self.globals.insert(name.clone(), v) {
                        release(old);
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
                    span,
                } => {
                    let v = frames[top].regs[*recv as usize];
                    let result = match method {
                        Method::Count => v
                            .list_len()
                            .or_else(|| v.map_len())
                            .or_else(|| v.as_string().map(|s| s.chars().count()))
                            .map(|n| Value::int(n as i64)),
                        Method::Enumerate => v.list_items().map(|items| {
                            let pairs = items
                                .iter()
                                .enumerate()
                                .map(|(i, &element)| {
                                    retain(element);
                                    Value::list(vec![Value::int(i as i64), element])
                                })
                                .collect();
                            Value::list(pairs)
                        }),
                    };
                    match result {
                        Some(value) => {
                            set_reg(&mut frames[top].regs, *dst, value);
                            frames[top].pc += 1;
                        }
                        None => {
                            return Err(self.error(
                                DiagnosticCode::UnknownName,
                                *span,
                                format!("no method `{}` on {}", method.name(), v.type_name()),
                            ));
                        }
                    }
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
                } => match apply_binary(
                    *op,
                    frames[top].regs[*a as usize],
                    frames[top].regs[*b as usize],
                ) {
                    Ok(v) => {
                        set_reg(&mut frames[top].regs, *dst, v);
                        frames[top].pc += 1;
                    }
                    Err(e) => return Err(self.error(e.code, *span, e.text)),
                },
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
                    let v = frames[top].regs[*src as usize];
                    retain(v); // keep alive across this frame's teardown
                    let finished = frames.pop().unwrap();
                    for r in &finished.regs {
                        release(*r);
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
