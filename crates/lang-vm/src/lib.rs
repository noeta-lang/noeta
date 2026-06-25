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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use lang_ast::{BinaryOp, Program};
use lang_backend::{Backend, RunResult};
use lang_bytecode::{BoolSide, Builtin, CaptureFrom, Const, Module, NarrowTarget, Op, ReuseCheck};
use lang_compiler::{Unsupported, compile};
use lang_diagnostics::{Diagnostic, DiagnosticCode};
use lang_gc::{release, retain};
use lang_object::{Shape, ShapeKind};
use lang_span::Span;
use lang_value::{Value, apply_binary, apply_unary, compare_primitive, structural_compare};

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
        Ok(execute(&module, Box::new(lang_stdlib::SandboxHost::new())))
    }

    /// Execute an already-compiled [`Module`]. This is the seam the salsa graph (`lang-db`)
    /// drives: it produces the `Module` via the memoized `bytecode` query, then hands it here.
    /// Splitting compilation from execution is what lets the VM "consume `chunk(db)`" (M1.1)
    /// without the VM crate depending on the database. Runs against a deterministic
    /// [`lang_stdlib::SandboxHost`] — the host the conformance differential always uses.
    pub fn run_module(&self, module: &Module) -> RunResult {
        execute(module, Box::new(lang_stdlib::SandboxHost::new()))
    }

    /// Execute a module against a caller-provided [`lang_stdlib::Host`] (M2.3). The CLI/REPL pass
    /// a real host here; the conformance harness keeps using the sandbox default via
    /// [`VmBackend::run_module`], so the differential stays deterministic.
    pub fn run_module_with_host(
        &self,
        module: &Module,
        host: Box<dyn lang_stdlib::Host>,
    ) -> RunResult {
        execute(module, host)
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
    /// The closure's captured upvalue cells, one owned reference each (released at frame
    /// teardown). Empty for top-level functions, methods, and operator-dispatch frames — only a
    /// closure built with captures carries any.
    upvalues: Vec<Value>,
}

/// A transform applied to a frame's return value as it flows into the caller's destination
/// register. Used by operator dispatch where the called trait method's raw result needs
/// post-processing: `!=` calls `Equatable::eq` and negates the resulting `bool`; `< <= > >=` call
/// `Comparable::compare` and map the resulting `Ordering` variant to a `bool`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RetTransform {
    /// Pass the value through unchanged (every ordinary call/return).
    None,
    /// Negate a `bool` result (for `!=` dispatched to `eq`); a non-bool passes through.
    Negate,
    /// Map a returned `Ordering` enum to this operator's `bool` (for `< <= > >=` dispatched to
    /// `compare`); a non-`Ordering` value passes through (an ill-typed `compare`).
    Ordering(BinaryOp),
    /// Wrap a by-name invocation's return value in `Result.Ok` (P2.6). The shape is the `Result.Ok`
    /// variant shape, baked into `Op::Invoke` and cloned in at frame setup; the raw return's
    /// reference transfers into the enum payload, so the original is *not* released afterward.
    WrapOk(Rc<Shape>),
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
            // `v`'s reference transfers into the enum payload, so it is *not* a replacement (the
            // returned `Ok` carries it onward); the caller must not release `v`.
            RetTransform::WrapOk(shape) => (Value::enum_value(shape, vec![v]), false),
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
    /// Type names whose value, when destroyed, can run *some* `destruct` block — its own or a
    /// transitively-owned field / variant-payload / collection element (the checker's
    /// destruct-reachability fixpoint, threaded through the module). The container-before-contained
    /// field-walk gate (Phase 4.3, spec §4): a value whose shape name is absent here owns no
    /// destructor in its subtree and frees on the plain-release fast path.
    destruct_reachable: HashSet<String>,
    /// Type names that `@derive(Comparable)` (without a hand-written `compare`): their instances
    /// get structural field-wise ordering for `< <= > >=`.
    comparable_derives: HashSet<String>,
    /// Type names that `@derive(Serialize<Json>)` (without a hand-written `to_json`): `o.to_json()` on
    /// their instances synthesizes a structural JSON serializer.
    tojson_derives: HashSet<String>,
    globals: HashMap<String, Value>,
    /// Top-level binding names in declaration order, so globals are destroyed at program end
    /// in reverse declaration order (the deterministic "program order" the spec requires).
    global_order: Vec<String>,
    /// The deterministic `next_id()` counter, seeded at 1 (matching the M0 `IdGen`).
    next_id: u64,
    /// All host-coupled effects (filesystem, seeded PRNG, logical clock) behind the M2.1
    /// [`lang_stdlib::Host`] seam. The conformance harness constructs a deterministic
    /// [`lang_stdlib::SandboxHost`]; a real host (later M2 slices) swaps in without touching
    /// this struct. See the eval backend's field of the same name.
    host: Box<dyn lang_stdlib::Host>,
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

/// Whether a value matches a narrowing target (`x.as<T>()`). Generics are erased, so only the
/// runtime **head constructor** is tested. The primitive/collection kinds compare against
/// [`Value::type_name`] — the same canonical strings the M0 tree-walker matches on, so both
/// backends decide a narrowing identically; `Named` (a user record/class/enum, or the built-in
/// `Option`/`Result`) matches by shape name; `Dyn` always matches (no-op narrowing).
fn narrow_matches(v: Value, target: &NarrowTarget) -> bool {
    let kind = match target {
        NarrowTarget::Int => "int",
        NarrowTarget::Float => "float",
        NarrowTarget::Bool => "bool",
        NarrowTarget::String => "string",
        NarrowTarget::Unit => "unit",
        NarrowTarget::List => "list",
        NarrowTarget::Map => "map",
        NarrowTarget::Set => "set",
        NarrowTarget::Fn => "function",
        NarrowTarget::Dyn => return true,
        NarrowTarget::Named(name) => return v.shape().is_some_and(|s| &s.name == name),
        NarrowTarget::AnyOf(members) => return members.iter().any(|m| narrow_matches(v, m)),
        // Abstract kind-types match any value of that declaration kind, by the value's shape kind.
        NarrowTarget::AnyEnum => {
            return v.shape().is_some_and(|s| s.kind == ShapeKind::Enum);
        }
        NarrowTarget::AnyRecord => {
            return v.shape().is_some_and(|s| s.kind == ShapeKind::Record);
        }
        NarrowTarget::AnyClass => {
            return v.shape().is_some_and(|s| s.kind == ShapeKind::Class);
        }
    };
    v.type_name() == kind
}

/// Execute a compiled module, capturing stdout, exit code, and diagnostics.
fn execute(module: &Module, host: Box<dyn lang_stdlib::Host>) -> RunResult {
    let methods = module
        .methods
        .iter()
        .map(|m| ((m.type_name.clone(), m.method.clone()), m.proto))
        .collect();
    let destructors = module.destructors.iter().cloned().collect();
    let destruct_reachable = module.destruct_reachable.iter().cloned().collect();
    let comparable_derives = module.comparable_derives.iter().cloned().collect();
    let tojson_derives = module.tojson_derives.iter().cloned().collect();
    let mut vm = Vm {
        module,
        shapes: module.shapes.iter().cloned().map(Rc::new).collect(),
        methods,
        destructors,
        destruct_reachable,
        comparable_derives,
        tojson_derives,
        globals: HashMap::new(),
        global_order: Vec::new(),
        next_id: 1,
        host,
        stdout: String::new(),
        diagnostics: Vec::new(),
    };
    let top = Frame {
        proto: 0,
        regs: vec![Value::unit(); module.main().num_registers as usize],
        pc: 0,
        ret_dst: 0,
        ret_transform: RetTransform::None,
        upvalues: Vec::new(),
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
    /// Materialize the `#[type_name(...)]` attributes from the module manifest into a
    /// `List<Attributed<T>>` — each a real `T` record (built from its stored args) paired with its
    /// target. Shapes are built fresh from the shared reflection info; because shape equality is
    /// structural (name + fields), they match the tree-walker's by construction.
    fn materialize_attributes(&self, type_name: &str) -> Value {
        let attributed_shape = Rc::new(Shape::object(
            ShapeKind::Record,
            "Attributed",
            vec!["target".to_string(), "value".to_string()],
        ));
        let info = self.module.reflection.type_named(type_name);
        let fields: Vec<String> = info.map(|t| t.fields.clone()).unwrap_or_default();
        let kind = match info.map(|t| t.kind) {
            Some(lang_ast::reflect::TypeKind::Class) => ShapeKind::Class,
            _ => ShapeKind::Record,
        };
        let items: Vec<Value> = self
            .module
            .reflection
            .manifest
            .iter()
            .filter(|a| a.name == type_name)
            .map(|a| {
                let values: Vec<Value> = lang_ast::reflect::materialize_args(a, &fields)
                    .iter()
                    .map(|v| attr_value_to_vm(v, &self.module.reflection))
                    .collect();
                let t_shape = Rc::new(Shape::object(kind, type_name, fields.clone()));
                let t_value = Value::object(t_shape, values);
                Value::object(
                    attributed_shape.clone(),
                    vec![Value::string(&a.target), t_value],
                )
            })
            .collect();
        Value::list(items)
    }

    /// Materialize the `(declaration, Role)` index from the module's reflection info into a
    /// `List<RoleBinding>` — each `{ target: string, role: Role }`. Shapes are built fresh; because
    /// shape equality is structural (name + variant + fields), the `Role` enum and `RoleBinding`
    /// record match the tree-walker's by construction. (P2.7.)
    fn materialize_roles(&self) -> Value {
        let binding_shape = Rc::new(Shape::object(
            ShapeKind::Record,
            "RoleBinding",
            vec!["target".to_string(), "role".to_string()],
        ));
        let items: Vec<Value> = self
            .module
            .reflection
            .roles
            .iter()
            .map(|r| {
                Value::object(
                    binding_shape.clone(),
                    vec![
                        Value::string(&r.target),
                        make_role(&r.enum_name, &r.variant),
                    ],
                )
            })
            .collect();
        Value::list(items)
    }

    /// Record a runtime diagnostic and produce the unwind token.
    fn error(&mut self, code: DiagnosticCode, span: Span, message: String) -> Abort {
        self.diagnostics
            .push(Diagnostic::error(code, span, message));
        Abort
    }

    /// A Ring 1 list method (`reverse`/`contains`/`join`). Mirrors the tree-walker's
    /// `call_list_method`; the result is a freshly-owned value (refcount 1). The receiver's
    /// elements shared from `list_items` are not retained, so any element placed into a *new*
    /// list must be retained first (the list then owns that reference).
    fn call_list_method(
        &mut self,
        list: Value,
        method: lang_stdlib::ListMethod,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        let items = list.list_items().expect("list receiver");
        match method {
            lang_stdlib::ListMethod::Reverse => {
                self.stdlib_arity(name, args, 0, span)?;
                let mut reversed = items;
                reversed.reverse();
                for &element in &reversed {
                    retain(element);
                }
                Ok(Value::list(reversed))
            }
            lang_stdlib::ListMethod::Contains => {
                self.stdlib_arity(name, args, 1, span)?;
                let target = args[0];
                let found = items.iter().any(|&item| {
                    apply_binary(BinaryOp::Eq, item, target)
                        .ok()
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                });
                Ok(Value::bool(found))
            }
            lang_stdlib::ListMethod::Join => {
                self.stdlib_arity(name, args, 1, span)?;
                let separator = self.stdlib_string(name, args[0], span)?;
                let joined = items
                    .iter()
                    .map(|v| v.display())
                    .collect::<Vec<_>>()
                    .join(&separator);
                Ok(Value::string(&joined))
            }
            lang_stdlib::ListMethod::Sorted => {
                self.stdlib_arity(name, args, 0, span)?;
                // Mutual orderability check against the first element (homogeneous numbers or
                // strings); a stable sort then matches the tree-walker element-for-element.
                if items
                    .iter()
                    .any(|&item| compare_primitive(items[0], item).is_none())
                {
                    let error = lang_stdlib::unorderable_error(name);
                    return Err(self.error(stdlib_error_code(error.kind), span, error.message));
                }
                let mut sorted = items;
                sorted
                    .sort_by(|&a, &b| compare_primitive(a, b).unwrap_or(std::cmp::Ordering::Equal));
                for &element in &sorted {
                    retain(element);
                }
                Ok(Value::list(sorted))
            }
            lang_stdlib::ListMethod::Slice => {
                self.stdlib_arity(name, args, 2, span)?;
                let start = self.stdlib_int(name, args[0], span)?;
                let end = self.stdlib_int(name, args[1], span)?;
                let len = items.len();
                if start < 0 || end < start || end as usize > len {
                    let error = lang_stdlib::slice_bounds_error(start, end, len);
                    return Err(self.error(stdlib_error_code(error.kind), span, error.message));
                }
                let slice: Vec<Value> = items[start as usize..end as usize].to_vec();
                for &element in &slice {
                    retain(element);
                }
                Ok(Value::list(slice))
            }
            lang_stdlib::ListMethod::First => {
                self.stdlib_arity(name, args, 0, span)?;
                Ok(match items.first() {
                    Some(&value) => {
                        retain(value);
                        make_some(value)
                    }
                    None => make_none(),
                })
            }
            lang_stdlib::ListMethod::Last => {
                self.stdlib_arity(name, args, 0, span)?;
                Ok(match items.last() {
                    Some(&value) => {
                        retain(value);
                        make_some(value)
                    }
                    None => make_none(),
                })
            }
            lang_stdlib::ListMethod::ToSet => {
                self.stdlib_arity(name, args, 0, span)?;
                match canonical_set(&items) {
                    Some(canonical) => {
                        for &element in &canonical {
                            retain(element);
                        }
                        Ok(Value::set(canonical))
                    }
                    None => {
                        let error = lang_stdlib::unorderable_error(name);
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            lang_stdlib::ListMethod::Set => {
                self.stdlib_arity(name, args, 2, span)?;
                let i = self.stdlib_int(name, args[0], span)?;
                if i < 0 || i as usize >= items.len() {
                    return Err(self.error(
                        DiagnosticCode::IndexOutOfBounds,
                        span,
                        format!("index {i} out of bounds for list of length {}", items.len()),
                    ));
                }
                // Replace the slot; the displaced old element is just dropped from the clone (it was
                // never retained by `list_items`). Every element the new list ends up holding is
                // retained once (the new list is a fresh owner).
                let mut new = items;
                new[i as usize] = args[1];
                for &element in &new {
                    retain(element);
                }
                Ok(Value::list(new))
            }
        }
    }

    /// A Ring 1 set method (`contains`/`union`/`intersection`). Mirrors the tree-walker's
    /// `call_set_method`. The receiver's elements (from `set_items`) are already canonical and
    /// shared (not retained); any element placed into a new set is retained first.
    fn call_set_method(
        &mut self,
        set: Value,
        method: lang_stdlib::SetMethod,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        let items = set.set_items().expect("set receiver");
        match method {
            lang_stdlib::SetMethod::Contains => {
                self.stdlib_arity(name, args, 1, span)?;
                let target = args[0];
                let found = items.iter().any(|&item| {
                    apply_binary(BinaryOp::Eq, item, target)
                        .ok()
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                });
                Ok(Value::bool(found))
            }
            lang_stdlib::SetMethod::Union => {
                self.stdlib_arity(name, args, 1, span)?;
                let other = self.stdlib_set(name, args[0], span)?;
                let mut combined = items;
                combined.extend(other);
                // Both operands are valid sets, so every element is orderable.
                let canonical = canonical_set(&combined).expect("set elements are orderable");
                for &element in &canonical {
                    retain(element);
                }
                Ok(Value::set(canonical))
            }
            lang_stdlib::SetMethod::Intersection => {
                self.stdlib_arity(name, args, 1, span)?;
                let other = self.stdlib_set(name, args[0], span)?;
                // `items` is already canonical, so filtering preserves sorted, de-duplicated order.
                let kept: Vec<Value> = items
                    .into_iter()
                    .filter(|&item| {
                        other.iter().any(|&o| {
                            apply_binary(BinaryOp::Eq, item, o)
                                .ok()
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false)
                        })
                    })
                    .collect();
                for &element in &kept {
                    retain(element);
                }
                Ok(Value::set(kept))
            }
            lang_stdlib::SetMethod::Add => {
                self.stdlib_arity(name, args, 1, span)?;
                let mut combined = items;
                combined.push(args[0]);
                match canonical_set(&combined) {
                    Some(canonical) => {
                        for &element in &canonical {
                            retain(element);
                        }
                        Ok(Value::set(canonical))
                    }
                    None => {
                        let error = lang_stdlib::unorderable_error(name);
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            lang_stdlib::SetMethod::Remove => {
                self.stdlib_arity(name, args, 1, span)?;
                let target = args[0];
                let kept: Vec<Value> = items
                    .into_iter()
                    .filter(|&item| {
                        !apply_binary(BinaryOp::Eq, item, target)
                            .ok()
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                    })
                    .collect();
                for &element in &kept {
                    retain(element);
                }
                Ok(Value::set(kept))
            }
        }
    }

    /// Read a set argument for a set method, raising the shared `lang-stdlib` type error. Returns
    /// the set's canonical elements (shared, not retained).
    fn stdlib_set(&mut self, name: &str, value: Value, span: Span) -> Result<Vec<Value>, Abort> {
        match value.set_items() {
            Some(items) => Ok(items),
            None => {
                let error = lang_stdlib::type_error(name, "set");
                Err(self.error(stdlib_error_code(error.kind), span, error.message))
            }
        }
    }

    /// Dispatch a Ring 2 native module function call (`json.parse(...)`). Mirrors the
    /// tree-walker's `call_native_module`.
    fn call_native_module(
        &mut self,
        module: &str,
        func: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        match lang_stdlib::NativeModule::from_name(module) {
            Some(lang_stdlib::NativeModule::Json) => self.call_json(func, args, span),
            Some(lang_stdlib::NativeModule::Math) => self.call_math(func, args, span),
            Some(lang_stdlib::NativeModule::Random) => self.call_random(func, args, span),
            Some(lang_stdlib::NativeModule::Fs) => self.call_fs(func, args, span),
            Some(lang_stdlib::NativeModule::Time) => self.call_time(func, args, span),
            Some(lang_stdlib::NativeModule::Env) => self.call_env(func, args, span),
            Some(lang_stdlib::NativeModule::Args) => self.call_args(func, args, span),
            // Only valid module names are ever bound, so this is unreachable in practice.
            None => {
                let error = lang_stdlib::no_function_error(module, func);
                Err(self.error(stdlib_error_code(error.kind), span, error.message))
            }
        }
    }

    /// The `math` module: pure scalar functions. Semantics live in `lang-stdlib::math`, so this
    /// is thin glue — project args, dispatch, lift `Output`. Mirrors the tree-walker's `call_math`.
    fn call_math(&mut self, func: &str, args: &[Value], span: Span) -> Result<Value, Abort> {
        let projected: Vec<lang_stdlib::Arg> = args.iter().map(|a| project_arg(*a)).collect();
        match lang_stdlib::math::call(func, &projected) {
            lang_stdlib::Dispatch::Done(output) => Ok(stdlib_output_to_value(output)),
            lang_stdlib::Dispatch::Err(error) => {
                Err(self.error(stdlib_error_code(error.kind), span, error.message))
            }
            lang_stdlib::Dispatch::Unknown => {
                let error = lang_stdlib::no_function_error("math", func);
                Err(self.error(stdlib_error_code(error.kind), span, error.message))
            }
        }
    }

    /// The `random` module: a seeded PRNG whose state lives in the host (`self.host`), threaded
    /// through the shared stepper so the stream matches the tree-walker for a given seed. Mirrors
    /// `call_random` there.
    fn call_random(&mut self, func: &str, args: &[Value], span: Span) -> Result<Value, Abort> {
        match func {
            "seed" => {
                self.stdlib_arity(func, args, 1, span)?;
                let n = self.stdlib_int(func, args[0], span)?;
                self.host.rng_seed(n);
                Ok(Value::unit())
            }
            "int" => {
                self.stdlib_arity(func, args, 2, span)?;
                let lo = self.stdlib_int(func, args[0], span)?;
                let hi = self.stdlib_int(func, args[1], span)?;
                match self.host.rng_int(lo, hi) {
                    Ok(value) => Ok(Value::int(value)),
                    Err(error) => {
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            "float" => {
                self.stdlib_arity(func, args, 0, span)?;
                Ok(Value::float(self.host.rng_float()))
            }
            _ => {
                let error = lang_stdlib::no_function_error("random", func);
                Err(self.error(stdlib_error_code(error.kind), span, error.message))
            }
        }
    }

    /// The `fs` module: file IO over the sandboxed in-memory [`lang_stdlib::fs::Vfs`]. Shared VFS
    /// semantics make this identical to the tree-walker's `call_fs` by construction.
    fn call_fs(&mut self, func: &str, args: &[Value], span: Span) -> Result<Value, Abort> {
        match func {
            "write" => {
                self.stdlib_arity(func, args, 2, span)?;
                let path = self.stdlib_string(func, args[0], span)?;
                let content = self.stdlib_string(func, args[1], span)?;
                match self.host.fs_write(&path, &content) {
                    Ok(()) => Ok(Value::unit()),
                    Err(error) => {
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            "append" => {
                self.stdlib_arity(func, args, 2, span)?;
                let path = self.stdlib_string(func, args[0], span)?;
                let content = self.stdlib_string(func, args[1], span)?;
                match self.host.fs_append(&path, &content) {
                    Ok(()) => Ok(Value::unit()),
                    Err(error) => {
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            "read" => {
                self.stdlib_arity(func, args, 1, span)?;
                let path = self.stdlib_string(func, args[0], span)?;
                match self.host.fs_read(&path) {
                    Ok(content) => Ok(Value::string(&content)),
                    Err(error) => {
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            "read_lines" => {
                self.stdlib_arity(func, args, 1, span)?;
                let path = self.stdlib_string(func, args[0], span)?;
                match self.host.fs_read(&path) {
                    Ok(content) => {
                        let lines = content.lines().map(Value::string).collect();
                        Ok(Value::list(lines))
                    }
                    Err(error) => {
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            "exists" => {
                self.stdlib_arity(func, args, 1, span)?;
                let path = self.stdlib_string(func, args[0], span)?;
                Ok(Value::bool(self.host.fs_exists(&path)))
            }
            "remove" => {
                self.stdlib_arity(func, args, 1, span)?;
                let path = self.stdlib_string(func, args[0], span)?;
                match self.host.fs_remove(&path) {
                    Ok(existed) => Ok(Value::bool(existed)),
                    Err(error) => {
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            // `list()` lists every file; `list(dir)` lists a directory's immediate children.
            "list" => {
                let result = match args.len() {
                    0 => self.host.fs_list(),
                    1 => {
                        let dir = self.stdlib_string(func, args[0], span)?;
                        self.host.fs_list_dir(&dir)
                    }
                    n => {
                        let error = lang_stdlib::arity_error(func, 1, n);
                        return Err(self.error(stdlib_error_code(error.kind), span, error.message));
                    }
                };
                match result {
                    Ok(paths) => {
                        let paths = paths.iter().map(|p| Value::string(p)).collect();
                        Ok(Value::list(paths))
                    }
                    Err(error) => {
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            "mkdir" => {
                self.stdlib_arity(func, args, 1, span)?;
                let path = self.stdlib_string(func, args[0], span)?;
                match self.host.fs_mkdir(&path) {
                    Ok(()) => Ok(Value::unit()),
                    Err(error) => {
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            "is_dir" => {
                self.stdlib_arity(func, args, 1, span)?;
                let path = self.stdlib_string(func, args[0], span)?;
                Ok(Value::bool(self.host.fs_is_dir(&path)))
            }
            // `open(path, mode)` → a cursor file handle. Read mode snapshots the file (a missing
            // file is the same E0021 as `fs.read`); write/append buffer until `close`.
            "open" => {
                self.stdlib_arity(func, args, 2, span)?;
                let path = self.stdlib_string(func, args[0], span)?;
                let mode_spec = self.stdlib_string(func, args[1], span)?;
                let Some(mode) = lang_stdlib::FileMode::parse(&mode_spec) else {
                    let error = lang_stdlib::handle::unknown_mode_error(&mode_spec);
                    return Err(self.error(stdlib_error_code(error.kind), span, error.message));
                };
                let handle = match mode {
                    lang_stdlib::FileMode::Read => match self.host.fs_read(&path) {
                        Ok(content) => lang_stdlib::FileHandle::open_read(&path, content),
                        Err(error) => {
                            return Err(self.error(
                                stdlib_error_code(error.kind),
                                span,
                                error.message,
                            ));
                        }
                    },
                    lang_stdlib::FileMode::Write => lang_stdlib::FileHandle::open_write(&path),
                    lang_stdlib::FileMode::Append => lang_stdlib::FileHandle::open_append(&path),
                };
                Ok(Value::file_handle(handle))
            }
            _ => {
                let error = lang_stdlib::no_function_error("fs", func);
                Err(self.error(stdlib_error_code(error.kind), span, error.message))
            }
        }
    }

    /// Dispatch a file-handle method. Mirrors the tree-walker's `call_file_handle_method`: the
    /// cursor logic lives in the shared `FileHandle`, so the two backends differ only in value glue
    /// (building `some`/`none`, routing the close flush through `self.host`).
    fn call_file_handle_method(
        &mut self,
        recv: Value,
        method: lang_stdlib::FileHandleMethod,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        use lang_stdlib::FileHandleMethod as M;
        match method {
            M::ReadLine => {
                self.stdlib_arity(name, args, 0, span)?;
                match recv.with_file_handle_mut(|handle| handle.read_line()) {
                    Ok(Some(line)) => Ok(make_some(Value::string(&line))),
                    Ok(None) => Ok(make_none()),
                    Err(error) => {
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            M::Read => {
                self.stdlib_arity(name, args, 1, span)?;
                let count = self.stdlib_int(name, args[0], span)?;
                match recv.with_file_handle_mut(|handle| handle.read(count)) {
                    Ok(Some(chunk)) => Ok(make_some(Value::string(&chunk))),
                    Ok(None) => Ok(make_none()),
                    Err(error) => {
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            M::Write => {
                self.stdlib_arity(name, args, 1, span)?;
                let chunk = self.stdlib_string(name, args[0], span)?;
                match recv.with_file_handle_mut(|handle| handle.write(&chunk)) {
                    Ok(()) => Ok(Value::unit()),
                    Err(error) => {
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            M::Close => {
                self.stdlib_arity(name, args, 0, span)?;
                // Take the flush instruction first (the handle borrow ends), then hit the host.
                let flush = recv.with_file_handle_mut(|handle| handle.close());
                let result = match flush {
                    None => Ok(()),
                    Some(lang_stdlib::Flush::Write { path, content }) => {
                        self.host.fs_write(&path, &content)
                    }
                    Some(lang_stdlib::Flush::Append { path, content }) => {
                        self.host.fs_append(&path, &content)
                    }
                };
                match result {
                    Ok(()) => Ok(Value::unit()),
                    Err(error) => {
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
        }
    }

    /// The `time` module: a deterministic logical clock. Mirrors the tree-walker's `call_time`.
    fn call_time(&mut self, func: &str, args: &[Value], span: Span) -> Result<Value, Abort> {
        match func {
            "monotonic" => {
                self.stdlib_arity(func, args, 0, span)?;
                let now = self.host.clock_monotonic();
                Ok(Value::int(now as i64))
            }
            "sleep" => {
                self.stdlib_arity(func, args, 1, span)?;
                let ms = self.stdlib_int(func, args[0], span)?;
                self.host.clock_sleep(ms);
                Ok(Value::unit())
            }
            _ => {
                let error = lang_stdlib::no_function_error("time", func);
                Err(self.error(stdlib_error_code(error.kind), span, error.message))
            }
        }
    }

    /// The `env` module: host environment introspection over the host's fixed sandbox fixture.
    /// `get(key)` returns the value or an `E0021` if absent (mirroring `fs.read`); `keys()` is
    /// sorted. Mirrors the tree-walker's `call_env`.
    fn call_env(&mut self, func: &str, args: &[Value], span: Span) -> Result<Value, Abort> {
        match func {
            "get" => {
                self.stdlib_arity(func, args, 1, span)?;
                let key = self.stdlib_string(func, args[0], span)?;
                match self.host.env_get(&key) {
                    Some(value) => Ok(Value::string(&value)),
                    None => {
                        let error = lang_stdlib::env::not_found_error(&key);
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            "keys" => {
                self.stdlib_arity(func, args, 0, span)?;
                let keys = self
                    .host
                    .env_keys()
                    .iter()
                    .map(|k| Value::string(k))
                    .collect();
                Ok(Value::list(keys))
            }
            _ => {
                let error = lang_stdlib::no_function_error("env", func);
                Err(self.error(stdlib_error_code(error.kind), span, error.message))
            }
        }
    }

    /// The `args` module: the program's argument vector. `all()` returns it as a list. Mirrors the
    /// tree-walker's `call_args`.
    fn call_args(&mut self, func: &str, args: &[Value], span: Span) -> Result<Value, Abort> {
        match func {
            "all" => {
                self.stdlib_arity(func, args, 0, span)?;
                let all = self.host.args().iter().map(|a| Value::string(a)).collect();
                Ok(Value::list(all))
            }
            _ => {
                let error = lang_stdlib::no_function_error("args", func);
                Err(self.error(stdlib_error_code(error.kind), span, error.message))
            }
        }
    }

    /// The `json` module: `parse(text) -> value` and `stringify(value) -> string`. Parsing goes
    /// through the shared `lang-stdlib` parser; stringifying reuses the structural `to_json`.
    fn call_json(&mut self, func: &str, args: &[Value], span: Span) -> Result<Value, Abort> {
        match func {
            "parse" => {
                self.stdlib_arity(func, args, 1, span)?;
                let text = self.stdlib_string(func, args[0], span)?;
                match lang_stdlib::json::parse(&text) {
                    Ok(json) => Ok(json_to_value(json)),
                    Err(detail) => {
                        let error = lang_stdlib::invalid_json_error(&detail);
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            "stringify" => {
                self.stdlib_arity(func, args, 1, span)?;
                Ok(Value::string(&args[0].to_json()))
            }
            _ => {
                let error = lang_stdlib::no_function_error("json", func);
                Err(self.error(stdlib_error_code(error.kind), span, error.message))
            }
        }
    }

    /// A Ring 1 map method (`keys`/`values`/`has`). Mirrors the tree-walker's `call_map_method`.
    fn call_map_method(
        &mut self,
        map: Value,
        method: lang_stdlib::MapMethod,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        match method {
            lang_stdlib::MapMethod::Keys => {
                self.stdlib_arity(name, args, 0, span)?;
                let keys = map.map_keys().expect("map receiver");
                Ok(Value::list(keys.iter().map(|k| Value::string(k)).collect()))
            }
            lang_stdlib::MapMethod::Values => {
                self.stdlib_arity(name, args, 0, span)?;
                let values = map.map_values().expect("map receiver");
                for &element in &values {
                    retain(element);
                }
                Ok(Value::list(values))
            }
            lang_stdlib::MapMethod::Has => {
                self.stdlib_arity(name, args, 1, span)?;
                let key = self.stdlib_string(name, args[0], span)?;
                Ok(Value::bool(map.map_get(&key).is_some()))
            }
            lang_stdlib::MapMethod::Set => {
                self.stdlib_arity(name, args, 2, span)?;
                let key = self.stdlib_string(name, args[0], span)?;
                let mut new = map.map_entries().expect("map receiver");
                new.insert(key, args[1]);
                // The receiver is borrowed (untouched); the new map is a fresh owner, so retain each
                // value it ends up holding exactly once. A displaced/absent value is simply not in
                // `new`, so it keeps only the receiver's reference — no leak, no double-free.
                for &value in new.values() {
                    retain(value);
                }
                Ok(Value::map(new))
            }
            lang_stdlib::MapMethod::Remove => {
                self.stdlib_arity(name, args, 1, span)?;
                let key = self.stdlib_string(name, args[0], span)?;
                let mut new = map.map_entries().expect("map receiver");
                new.remove(&key);
                for &value in new.values() {
                    retain(value);
                }
                Ok(Value::map(new))
            }
        }
    }

    /// Apply an in-place map update (`set`/`remove`) to a **consumed** map receiver (Phase 5.1c): the
    /// caller has already taken the receiver's single reference out of its register. When uniquely
    /// owned (`refcount == 1`) the backing buffer is mutated in place — O(1) — and the displaced value
    /// (if any) fires its destructor now via `release_value`, matching the copy-and-reassign baseline
    /// (which releases it when the old map dies at the reassignment). An aliased map copies (preserving
    /// the other owner's view), then drops the consumed reference. Run under miri to validate refcounts.
    /// Apply an in-place list `set(index, value)` to a **consumed** list receiver (the caller has
    /// taken its single reference out of the register). When uniquely owned (`refcount == 1`) the slot
    /// is overwritten in place — O(1), the displaced element released — otherwise the list copies
    /// (preserving an alias), then the consumed reference is dropped. An out-of-range index is E0016.
    fn list_set_in_place(
        &mut self,
        list: Value,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        let i = self.stdlib_int("set", args[0], span)?;
        let len = list.list_len().unwrap_or(0);
        if i < 0 || i as usize >= len {
            release(list);
            return Err(self.error(
                DiagnosticCode::IndexOutOfBounds,
                span,
                format!("index {i} out of bounds for list of length {len}"),
            ));
        }
        if list.refcount() == 1 {
            let value = args[1];
            retain(value);
            let old = list.list_replace_slot(i as usize, value);
            self.release_value(old);
            Ok(list)
        } else {
            // Aliased: copy via the ordinary method, then drop the consumed reference.
            let new =
                self.call_list_method(list, lang_stdlib::ListMethod::Set, "set", args, span)?;
            release(list);
            Ok(new)
        }
    }

    fn map_update_in_place(
        &mut self,
        map: Value,
        method: lang_stdlib::MapMethod,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        if map.refcount() != 1 {
            // Aliased: copy, then release the reference we consumed from the receiver register.
            let new = self.call_map_method(map, method, name, args, span)?;
            release(map);
            return Ok(new);
        }
        match method {
            lang_stdlib::MapMethod::Set => {
                self.stdlib_arity(name, args, 2, span)?;
                let key = self.stdlib_string(name, args[0], span)?;
                let value = args[1];
                // The map gains an owned reference to the new value.
                retain(value);
                if let Some(old) = map.map_insert(key, value) {
                    self.release_value(old);
                }
                Ok(map)
            }
            lang_stdlib::MapMethod::Remove => {
                self.stdlib_arity(name, args, 1, span)?;
                let key = self.stdlib_string(name, args[0], span)?;
                if let Some(old) = map.map_remove(&key) {
                    self.release_value(old);
                }
                Ok(map)
            }
            // Only `set`/`remove` are routed to the in-place path by the dispatch guard.
            _ => unreachable!("non-update map method on the in-place path"),
        }
    }

    /// In-place `add`/`remove` for a reuse-marked set self-update (`s = s.add(x)` / `s = s.remove(x)`).
    /// The receiver has been consumed from its register by the dispatch above. A uniquely-owned set
    /// mutates its canonical buffer in place via a binary search (the displaced element of a `remove`,
    /// or nothing for `add`, releases now — matching the copy baseline, which drops the old set); an
    /// aliased set copies through the ordinary method so the other owner's view is preserved.
    fn set_update_in_place(
        &mut self,
        set: Value,
        method: lang_stdlib::SetMethod,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        if set.refcount() != 1 {
            // Aliased: copy, then release the reference we consumed from the receiver register.
            let new = self.call_set_method(set, method, name, args, span)?;
            release(set);
            return Ok(new);
        }
        if let Err(err) = self.stdlib_arity(name, args, 1, span) {
            release(set);
            return Err(err);
        }
        let target = args[0];
        // A target not orderable against the set's class behaves exactly as the copy path: `add`
        // raises the unorderable error, `remove` finds nothing (a no-op). An empty set is orderable
        // with anything, so a first-element probe of `None` (empty) takes the in-place path.
        let orderable = set
            .set_first()
            .is_none_or(|first| compare_primitive(first, target).is_some());
        match method {
            lang_stdlib::SetMethod::Add => {
                if !orderable {
                    release(set);
                    let error = lang_stdlib::unorderable_error(name);
                    return Err(self.error(stdlib_error_code(error.kind), span, error.message));
                }
                // The set gains an owned reference only when the element is newly inserted.
                if set.set_insert_sorted(target) {
                    retain(target);
                }
                Ok(set)
            }
            lang_stdlib::SetMethod::Remove => {
                if orderable && let Some(old) = set.set_remove_sorted(target) {
                    self.release_value(old);
                }
                Ok(set)
            }
            // Only `add`/`remove` are routed to the in-place path by the dispatch guard.
            _ => unreachable!("non-update set method on the in-place path"),
        }
    }

    /// Enforce a collection method's arity, raising the shared `lang-stdlib` arity error.
    fn stdlib_arity(
        &mut self,
        name: &str,
        args: &[Value],
        expected: usize,
        span: Span,
    ) -> Result<(), Abort> {
        if args.len() == expected {
            Ok(())
        } else {
            let error = lang_stdlib::arity_error(name, expected, args.len());
            Err(self.error(stdlib_error_code(error.kind), span, error.message))
        }
    }

    /// Read a string argument for a collection method, raising the shared `lang-stdlib` type error.
    fn stdlib_string(&mut self, name: &str, value: Value, span: Span) -> Result<String, Abort> {
        match value.as_string() {
            Some(s) => Ok(s),
            None => {
                let error = lang_stdlib::type_error(name, "string");
                Err(self.error(stdlib_error_code(error.kind), span, error.message))
            }
        }
    }

    /// Read an int argument for a collection method, raising the shared `lang-stdlib` type error.
    /// `as_int` is `None` for a float, so `slice(1.0, 2)` is a type error — matching the
    /// tree-walker, which accepts only `Value::Int`.
    fn stdlib_int(&mut self, name: &str, value: Value, span: Span) -> Result<i64, Abort> {
        match value.as_int() {
            Some(i) => Ok(i),
            None => {
                let error = lang_stdlib::type_error(name, "int");
                Err(self.error(stdlib_error_code(error.kind), span, error.message))
            }
        }
    }

    /// Release a value that may be the *last* reference to a destructor-carrying object. If so,
    /// the `destruct` block runs synchronously (with the instance's fields in scope) before the
    /// object is freed — the deterministic destruction the spec requires. Used at every
    /// destructor-relevant drop point: reassignment, program end, and (Phase 4) a destructor-
    /// relevant `Op::Drop` at a local's last use. A non-relevant release uses the plain `release`.
    fn release_value(&mut self, value: Value) {
        // Immediates, and any reference that is not the last, never run a destructor here: an
        // immediate has none, and an alias survives (spec §2 — destruction defers to the final
        // reference). Both take the plain release (a decrement; a free only at the true last ref).
        if !value.is_pointer() || value.refcount() > 1 {
            release(value);
            return;
        }
        // The last reference. Take the slow container-before-contained path only if this subtree
        // owns a destructor — its own (an object/enum whose `destruct` is in the table) or a
        // contained one (`subtree_owns_destructor`). Otherwise the plain recursive free reclaims it
        // with no per-node destructor lookups — the Phase-4.3 fast path for non-RAII data.
        let own = value
            .shape()
            .and_then(|s| self.destructors.get(&s.name).copied());
        if own.is_none() && !self.subtree_owns_destructor(value) {
            release(value);
            return;
        }
        // Container before contained (spec §4): the container's own `destruct` runs first, while
        // its fields are still live; then each child is released in declared/iteration order — a
        // child reaching zero runs its own `destruct`, recursively — and finally the container's
        // own box is freed (children already released, so a shallow free that does not touch them).
        if let Some(proto) = own {
            self.run_destructor(proto, value);
        }
        for child in value.gc_children() {
            self.release_value(child);
        }
        value.gc_free_shallow();
    }

    /// Whether `value`'s subtree may contain a destructor — the container-before-contained
    /// field-walk gate (spec §4, Phase 4.3). An object/enum is decided by its type name against the
    /// checker's destruct-reachability set; a list/map/set is always walked because its element
    /// types are erased at runtime (a non-relevant element then takes the fast path on its own);
    /// any other value kind (string, closure, cell, handle, boxed int) is a leaf with no
    /// destructor-bearing children, so it frees plainly.
    fn subtree_owns_destructor(&self, value: Value) -> bool {
        match value.shape() {
            Some(shape) => self.destruct_reachable.contains(&shape.name),
            None => value.is_list() || value.is_map() || value.is_set(),
        }
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
            upvalues: Vec::new(),
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
            // Phase 4.2c-ii: a panic unwinds the live frames. Before reclaiming their memory, fire
            // the `destruct` of every live destructor-bearing frame local — innermost frame first,
            // reverse-construction within each (the `frame_locals` list reversed) — so an aborting
            // program destroys its abandoned values deterministically (spec §6). This matches the
            // tree-walker, which fires each aborted scope's `drain_reverse` as the abort climbs the
            // call stack. Each fired register is cleared to `unit`, so the plain release below (which
            // also reclaims temporaries, never destructor-fired in either backend) never double-frees.
            for fi in (0..frames.len()).rev() {
                let proto = frames[fi].proto as usize;
                let count = self.module.protos[proto].frame_locals.len();
                for idx in (0..count).rev() {
                    let reg = self.module.protos[proto].frame_locals[idx] as usize;
                    let v = std::mem::replace(&mut frames[fi].regs[reg], Value::unit());
                    self.release_value(v);
                }
            }
            for frame in &frames {
                for r in &frame.regs {
                    release(*r);
                }
                for u in &frame.upvalues {
                    release(*u);
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
        // Per-run inline caches, one slot per cacheable call site (`LoadField`/`CallMethod`),
        // indexed by the op's `cache` field. Each entry memoizes the last receiver shape and the
        // resolved field-slot / method prototype; a hit is a pointer compare against the cached
        // shape, skipping the field-name scan / `(type, method)` hashmap lookup. A local (not a
        // `self` field) so it neither borrows `self` in the loop nor leaks across runs; holding the
        // `Rc<Shape>` keeps the cached shape alive, so the pointer key can never alias a freed shape.
        let mut caches: Vec<Option<(Rc<Shape>, u32)>> = vec![None; module.cache_slots as usize];
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
                Op::TakeGlobal { dst, name, span } => {
                    // Move the global's value into `dst`, leaving `unit` — no retain, so the single
                    // owning reference transfers and a following `ConcatInPlace` can see uniqueness.
                    let v = match self.globals.get_mut(name) {
                        Some(slot) => std::mem::replace(slot, Value::unit()),
                        None => {
                            return Err(self.error(
                                DiagnosticCode::UnknownName,
                                *span,
                                format!("cannot find `{name}` in this scope"),
                            ));
                        }
                    };
                    set_reg(&mut frames[top].regs, *dst, v);
                    frames[top].pc += 1;
                }
                Op::Drop { reg, relevant } => {
                    // Release a dead binding/temporary at its last use and clear it to `unit` (so
                    // `set_reg`/teardown later release `unit`, never double-freeing). This frees the
                    // value promptly, restoring an accumulator's unique ownership. When the IR marked
                    // the drop destructor-relevant (Phase 4), route it through `release_value` so a
                    // `destruct` block fires here if this is the final owning reference; otherwise the
                    // value provably reaches no destructor and the plain `release` is used.
                    let v = std::mem::replace(&mut frames[top].regs[*reg as usize], Value::unit());
                    if *relevant {
                        self.release_value(v);
                    } else {
                        release(v);
                    }
                    frames[top].pc += 1;
                }
                Op::ConcatInPlace { dst, lhs, rhs, .. } => {
                    let l = frames[top].regs[*lhs as usize];
                    let r = frames[top].regs[*rhs as usize];
                    // `lhs` is consumed: clear its register *without* releasing (a direct overwrite,
                    // not `set_reg`), so the refcount below still counts the accumulator's reference
                    // and the single owner is transferred into the result. This also makes a
                    // `dst == lhs` store safe (the old occupant is now `unit`, not the live list).
                    frames[top].regs[*lhs as usize] = Value::unit();
                    let result = if l.is_list() && r.is_list() {
                        if l.refcount() == 1 {
                            // Sole owner: extend the backing buffer in place (O(1) amortized). The
                            // single reference moves from `lhs` into the result.
                            l.list_extend(r);
                            l
                        } else {
                            // Aliased (refcount > 1): copy, preserving immutable semantics. Retain
                            // each element into the new list, then drop the accumulator's reference.
                            let mut items = l.list_items().unwrap();
                            items.extend(r.list_items().unwrap());
                            for &item in &items {
                                item.inc_ref();
                            }
                            release(l);
                            Value::list(items)
                        }
                    } else {
                        // Non-list operand: display concatenation, identical to `Op::Binary`'s `~`.
                        let s = Value::string(&format!("{}{}", l.display(), r.display()));
                        release(l);
                        s
                    };
                    set_reg(&mut frames[top].regs, *dst, result);
                    frames[top].pc += 1;
                }
                Op::MakeClosure {
                    dst,
                    proto,
                    captures,
                } => {
                    // Gather one cell per capture (from a celled local register, or one of this
                    // frame's own upvalues — forwarding a capture down a level), retaining each
                    // into the new closure, which owns its upvalue cells.
                    let mut upvalues = Vec::with_capacity(captures.len());
                    for capture in captures.iter() {
                        let cell = match capture {
                            CaptureFrom::Local(reg) => frames[top].regs[*reg as usize],
                            CaptureFrom::Upvalue(index) => frames[top].upvalues[*index as usize],
                        };
                        retain(cell);
                        upvalues.push(cell);
                    }
                    let v = Value::closure(*proto, upvalues);
                    set_reg(&mut frames[top].regs, *dst, v);
                    frames[top].pc += 1;
                }
                Op::MakeCell { dst, src } => {
                    // Box the value into a fresh cell, which owns one reference to it.
                    let v = frames[top].regs[*src as usize];
                    retain(v);
                    set_reg(&mut frames[top].regs, *dst, Value::cell(v));
                    frames[top].pc += 1;
                }
                Op::CellGet { dst, cell } => {
                    let v = frames[top].regs[*cell as usize].cell_get();
                    retain(v);
                    set_reg(&mut frames[top].regs, *dst, v);
                    frames[top].pc += 1;
                }
                Op::CellSet { cell, src } => {
                    // `cell_set` retains the new occupant and releases the old internally.
                    let v = frames[top].regs[*src as usize];
                    frames[top].regs[*cell as usize].cell_set(v);
                    frames[top].pc += 1;
                }
                Op::UpvalueGet { dst, index } => {
                    let v = frames[top].upvalues[*index as usize].cell_get();
                    retain(v);
                    set_reg(&mut frames[top].regs, *dst, v);
                    frames[top].pc += 1;
                }
                Op::UpvalueSet { index, src } => {
                    let v = frames[top].regs[*src as usize];
                    frames[top].upvalues[*index as usize].cell_set(v);
                    frames[top].pc += 1;
                }
                Op::LoadNativeFn { dst, func } => {
                    set_reg(&mut frames[top].regs, *dst, Value::native_fn(*func));
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
                Op::MakeRange {
                    dst,
                    start,
                    end,
                    inclusive,
                    span,
                } => {
                    let lo = frames[top].regs[*start as usize];
                    let hi = frames[top].regs[*end as usize];
                    match (lo.as_int(), hi.as_int()) {
                        (Some(a), Some(b)) => {
                            // `..=` shifts the exclusive upper to `b + 1`; `saturating_add` keeps
                            // the unmaterializable `i64::MAX` edge from panicking. The elements are
                            // fresh int immediates (no refcount), so no retain is needed.
                            let upper = if *inclusive { b.saturating_add(1) } else { b };
                            let elements: Vec<Value> = (a..upper).map(Value::int).collect();
                            set_reg(&mut frames[top].regs, *dst, Value::list(elements));
                            frames[top].pc += 1;
                        }
                        _ => {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "range bounds must be ints, found {} and {}",
                                    lo.type_name(),
                                    hi.type_name()
                                ),
                            ));
                        }
                    }
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
                    // A user object lights up the `Iterable` trait: `for x in o` iterates the list
                    // its `iter` method returns. The method runs bytecode, so it is pushed as a
                    // call frame; its returned value becomes the snapshot (the following `ListLen`
                    // raises E0007 if it was not a list). Matches the tree-walker's `exec_for`.
                    if v.is_object() {
                        let type_name = v.shape().unwrap().name.clone();
                        if let Some(&proto) =
                            self.methods.get(&(type_name.clone(), "iter".to_string()))
                        {
                            let chunk = &module.protos[proto as usize];
                            if chunk.num_params != 1 {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!(
                                        "this method takes {} argument(s) but 0 were supplied",
                                        chunk.num_params - 1
                                    ),
                                ));
                            }
                            let mut new_regs = vec![Value::unit(); chunk.num_registers as usize];
                            retain(v);
                            new_regs[0] = v;
                            frames[top].pc += 1;
                            frames.push(Frame {
                                proto,
                                regs: new_regs,
                                pc: 0,
                                ret_dst: *dst,
                                ret_transform: RetTransform::None,
                                upvalues: Vec::new(),
                            });
                            continue;
                        }
                    }
                    // Snapshot the elements to iterate (a list's elements, a set's canonical
                    // elements, or a map's values in sorted-key order), each retained so the loop
                    // owns them independently.
                    let snapshot = match v
                        .list_items()
                        .or_else(|| v.set_items())
                        .or_else(|| v.map_values())
                    {
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
                Op::ListLen { dst, src, span } => {
                    // After `IterSnapshot`, `src` is a list for the list/map paths; the only way it
                    // is not is an `Iterable::iter` that returned a non-list, reported here (E0007),
                    // matching the tree-walker's `exec_for`.
                    let v = frames[top].regs[*src as usize];
                    match v.list_len() {
                        Some(n) => {
                            set_reg(&mut frames[top].regs, *dst, Value::int(n as i64));
                            frames[top].pc += 1;
                        }
                        None => {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("`iter` must return a list, found {}", v.type_name()),
                            ));
                        }
                    }
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
                    // A user object lights up the `Length` trait: `len(o)` dispatches to its `len`
                    // method, which runs bytecode, so it is pushed as a call frame rather than
                    // handled by the synchronous `call_builtin`. (Matches the tree-walker's
                    // `Builtin::Len` object case.)
                    if *builtin == Builtin::Len && args.len() == 1 {
                        let recv = frames[top].regs[args[0] as usize];
                        if recv.is_object() {
                            let type_name = recv.shape().unwrap().name.clone();
                            if let Some(&proto) =
                                self.methods.get(&(type_name.clone(), "len".to_string()))
                            {
                                let chunk = &module.protos[proto as usize];
                                if chunk.num_params != 1 {
                                    return Err(self.error(
                                        DiagnosticCode::TypeMismatch,
                                        *span,
                                        format!(
                                            "this method takes {} argument(s) but 0 were supplied",
                                            chunk.num_params - 1
                                        ),
                                    ));
                                }
                                let mut new_regs =
                                    vec![Value::unit(); chunk.num_registers as usize];
                                retain(recv);
                                new_regs[0] = recv;
                                frames[top].pc += 1;
                                frames.push(Frame {
                                    proto,
                                    regs: new_regs,
                                    pc: 0,
                                    ret_dst: *dst,
                                    ret_transform: RetTransform::None,
                                    upvalues: Vec::new(),
                                });
                                continue;
                            }
                        }
                    }
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
                    cache,
                    reuse,
                } => {
                    let v = frames[top].regs[*recv as usize];
                    // In-place map self-update (Phase 5.1c): a reuse-marked `m = m.set(k,v)` /
                    // `m = m.remove(k)` whose runtime receiver is actually a map consumes the receiver
                    // register and mutates the sole-owned backing buffer in place (an alias copies). A
                    // non-map receiver — a user method that happens to be named `set` — falls through to
                    // the ordinary dispatch below with the receiver intact.
                    if *reuse
                        && v.is_map()
                        && let Some(map_method) = lang_stdlib::MapMethod::from_name(method)
                        && matches!(
                            map_method,
                            lang_stdlib::MapMethod::Set | lang_stdlib::MapMethod::Remove
                        )
                    {
                        let arg_values: Vec<Value> =
                            args.iter().map(|r| frames[top].regs[*r as usize]).collect();
                        // Consume the receiver: take its single reference out of the register without
                        // releasing (a direct overwrite, like `ConcatInPlace`), so the refcount below
                        // still counts the accumulator's reference and a `dst == recv` store is safe.
                        frames[top].regs[*recv as usize] = Value::unit();
                        let result =
                            self.map_update_in_place(v, map_method, method, &arg_values, *span)?;
                        set_reg(&mut frames[top].regs, *dst, result);
                        frames[top].pc += 1;
                        continue;
                    }
                    // In-place list self-update (`xs[i] = v` ⟶ `xs = xs.set(i, v)`): a uniquely-owned
                    // list overwrites slot `i` in place (O(1)) instead of copying the whole list.
                    if *reuse && v.is_list() && method == "set" {
                        let arg_values: Vec<Value> =
                            args.iter().map(|r| frames[top].regs[*r as usize]).collect();
                        frames[top].regs[*recv as usize] = Value::unit();
                        let result = self.list_set_in_place(v, &arg_values, *span)?;
                        set_reg(&mut frames[top].regs, *dst, result);
                        frames[top].pc += 1;
                        continue;
                    }
                    // In-place set self-update (`s = s.add(x)` / `s = s.remove(x)`): a uniquely-owned,
                    // canonically-ordered set binary-search-inserts/removes one element in its existing
                    // buffer instead of cloning + re-sorting the whole set.
                    if *reuse
                        && v.is_set()
                        && let Some(set_method) = lang_stdlib::SetMethod::from_name(method)
                        && matches!(
                            set_method,
                            lang_stdlib::SetMethod::Add | lang_stdlib::SetMethod::Remove
                        )
                    {
                        let arg_values: Vec<Value> =
                            args.iter().map(|r| frames[top].regs[*r as usize]).collect();
                        frames[top].regs[*recv as usize] = Value::unit();
                        let result =
                            self.set_update_in_place(v, set_method, method, &arg_values, *span)?;
                        set_reg(&mut frames[top].regs, *dst, result);
                        frames[top].pc += 1;
                        continue;
                    }
                    // `json.parse(...)` — a Ring 2 native module function call, dispatched before
                    // the object/collection paths.
                    if let Some(module_name) = v.native_module_name() {
                        let arg_values: Vec<Value> =
                            args.iter().map(|r| frames[top].regs[*r as usize]).collect();
                        let value =
                            self.call_native_module(&module_name, method, &arg_values, *span)?;
                        set_reg(&mut frames[top].regs, *dst, value);
                        frames[top].pc += 1;
                        continue;
                    }
                    // An object dispatches to a user method through the type's method table;
                    // anything else falls to the built-in `count`/`enumerate` methods.
                    if v.is_object() {
                        // `o.to_json()` on a type that `@derive(Serialize<Json>)` (so has no hand-written
                        // `to_json`) synthesizes a structural JSON string — a pure value
                        // computation, so it is produced inline rather than via a call frame. Only a
                        // literal `to_json` site reaches here, so the shape clone stays off the common
                        // method-call path.
                        if method == "to_json" && args.is_empty() {
                            let type_name = v.shape().unwrap().name.clone();
                            if self.tojson_derives.contains(&type_name) {
                                let json = Value::string(&v.to_json());
                                set_reg(&mut frames[top].regs, *dst, json);
                                frames[top].pc += 1;
                                continue;
                            }
                        }
                        // Inline cache: a hit (the receiver's shape pointer matches the cached one)
                        // gives the resolved prototype directly, skipping the `(type, method)` hashmap
                        // lookup and its two `String` clones. The hit check avoids bumping the shape
                        // refcount (raw pointer compare); only a miss clones the shape into the cache.
                        let ci = *cache as usize;
                        let shape_ptr = v.object_shape_ptr();
                        let hit = match &caches[ci] {
                            Some((cs, p)) if Some(Rc::as_ptr(cs)) == shape_ptr => Some(*p),
                            _ => None,
                        };
                        let proto = match hit {
                            Some(proto) => proto,
                            None => {
                                let shape = v.shape().unwrap();
                                let Some(&proto) =
                                    self.methods.get(&(shape.name.clone(), method.clone()))
                                else {
                                    return Err(self.error(
                                        DiagnosticCode::UnknownName,
                                        *span,
                                        format!("type `{}` has no method `{method}`", shape.name),
                                    ));
                                };
                                caches[ci] = Some((shape, proto));
                                proto
                            }
                        };
                        let chunk = &module.protos[proto as usize];
                        // The prototype takes the receiver in register 0 and the user arguments
                        // after it, so its declared arity is one more than the supplied args. A
                        // method may have trailing defaulted parameters, so the supplied count is a
                        // range `[total - defaults, total]` (all less the receiver).
                        let total = chunk.num_params as usize - 1;
                        let required = total - chunk.defaults.len();
                        if args.len() < required || args.len() > total {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                arity_message("method", required, total, args.len()),
                            ));
                        }
                        let num_registers = chunk.num_registers as usize;
                        let defaults = chunk.defaults.clone();
                        let mut new_regs = vec![Value::unit(); num_registers];
                        retain(v);
                        new_regs[0] = v;
                        for (i, &arg_reg) in args.iter().enumerate() {
                            let a = frames[top].regs[arg_reg as usize];
                            retain(a);
                            new_regs[i + 1] = a;
                        }
                        // Fill any omitted trailing parameters from their default thunks. The
                        // receiver and supplied args occupy registers `0..=args.len()`, so a default
                        // register at or beyond that was not supplied.
                        // A method frame carries no upvalues (it is defined at module scope), so its
                        // default thunks resolve globals only.
                        let filled = args.len() + 1;
                        for (reg, proto) in &defaults {
                            if *reg as usize >= filled {
                                let value = self.run_thunk(*proto, &[])?;
                                new_regs[*reg as usize] = value;
                            }
                        }
                        frames[top].pc += 1;
                        frames.push(Frame {
                            proto,
                            regs: new_regs,
                            pc: 0,
                            ret_dst: *dst,
                            ret_transform: RetTransform::None,
                            upvalues: Vec::new(),
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
                    // Ring 1 string methods (`upper`/`split`/`replace`/...) — dispatched through
                    // the shared `lang-stdlib` surface so the tree-walker and the VM cannot drift.
                    // `Unknown` falls through to the collection methods below. `as_string` clones
                    // out of the heap, so the projected args own their strings for the call.
                    if let Some(recv_str) = v.as_string() {
                        let arg_values: Vec<Value> =
                            args.iter().map(|r| frames[top].regs[*r as usize]).collect();
                        let arg_strings: Vec<Option<String>> =
                            arg_values.iter().map(|a| a.as_string()).collect();
                        let projected: Vec<lang_stdlib::Arg> = arg_values
                            .iter()
                            .zip(&arg_strings)
                            .map(|(a, s)| {
                                if let Some(s) = s {
                                    lang_stdlib::Arg::Str(s)
                                } else if let Some(i) = a.as_int() {
                                    lang_stdlib::Arg::Int(i)
                                } else if let Some(f) = a.as_float() {
                                    lang_stdlib::Arg::Float(f)
                                } else if let Some(b) = a.as_bool() {
                                    lang_stdlib::Arg::Bool(b)
                                } else {
                                    lang_stdlib::Arg::Other
                                }
                            })
                            .collect();
                        match lang_stdlib::string_method(&recv_str, method, &projected) {
                            lang_stdlib::Dispatch::Done(output) => {
                                let value = stdlib_output_to_value(output);
                                set_reg(&mut frames[top].regs, *dst, value);
                                frames[top].pc += 1;
                                continue;
                            }
                            lang_stdlib::Dispatch::Err(error) => {
                                return Err(self.error(
                                    stdlib_error_code(error.kind),
                                    *span,
                                    error.message,
                                ));
                            }
                            lang_stdlib::Dispatch::Unknown => {}
                        }
                    }
                    // Ring 1 list methods (reverse/contains/join) — the shared `ListMethod` enum
                    // makes the helper's `match` exhaustive, so the tree-walker cannot offer a
                    // method this backend lacks.
                    if v.list_len().is_some()
                        && let Some(list_method) = lang_stdlib::ListMethod::from_name(method)
                    {
                        let arg_values: Vec<Value> =
                            args.iter().map(|r| frames[top].regs[*r as usize]).collect();
                        let value =
                            self.call_list_method(v, list_method, method, &arg_values, *span)?;
                        set_reg(&mut frames[top].regs, *dst, value);
                        frames[top].pc += 1;
                        continue;
                    }
                    // Ring 1 set methods (contains/union/intersection).
                    if v.is_set()
                        && let Some(set_method) = lang_stdlib::SetMethod::from_name(method)
                    {
                        let arg_values: Vec<Value> =
                            args.iter().map(|r| frames[top].regs[*r as usize]).collect();
                        let value =
                            self.call_set_method(v, set_method, method, &arg_values, *span)?;
                        set_reg(&mut frames[top].regs, *dst, value);
                        frames[top].pc += 1;
                        continue;
                    }
                    // File-handle methods (read_line/read/write/close) — the shared
                    // `FileHandleMethod` enum keeps the two backends in lockstep.
                    if v.is_file_handle()
                        && let Some(handle_method) =
                            lang_stdlib::FileHandleMethod::from_name(method)
                    {
                        let arg_values: Vec<Value> =
                            args.iter().map(|r| frames[top].regs[*r as usize]).collect();
                        let value = self.call_file_handle_method(
                            v,
                            handle_method,
                            method,
                            &arg_values,
                            *span,
                        )?;
                        set_reg(&mut frames[top].regs, *dst, value);
                        frames[top].pc += 1;
                        continue;
                    }
                    // Ring 1 map methods (keys/values/has).
                    if v.is_map()
                        && let Some(map_method) = lang_stdlib::MapMethod::from_name(method)
                    {
                        let arg_values: Vec<Value> =
                            args.iter().map(|r| frames[top].regs[*r as usize]).collect();
                        let value =
                            self.call_map_method(v, map_method, method, &arg_values, *span)?;
                        set_reg(&mut frames[top].regs, *dst, value);
                        frames[top].pc += 1;
                        continue;
                    }
                    // Built-in zero-argument methods on lists/maps/strings.
                    let result = if !args.is_empty() {
                        None
                    } else if method == "count" {
                        v.list_len()
                            .or_else(|| v.set_len())
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
                Op::Index {
                    dst,
                    recv,
                    index,
                    span,
                } => {
                    let v = frames[top].regs[*recv as usize];
                    let idx = frames[top].regs[*index as usize];
                    // `o[i]` on a user object lights up the `Index` trait: dispatch to `get`,
                    // pushing a call frame `[recv, index]` exactly like a method call. An object
                    // without an `Index` impl has no `get` method, so this reports the missing
                    // method — matching the tree-walker's `eval_index`.
                    if v.is_object() {
                        let type_name = v.shape().unwrap().name.clone();
                        let Some(&proto) =
                            self.methods.get(&(type_name.clone(), "get".to_string()))
                        else {
                            return Err(self.error(
                                DiagnosticCode::UnknownName,
                                *span,
                                format!("type `{type_name}` has no method `get`"),
                            ));
                        };
                        let chunk = &module.protos[proto as usize];
                        if chunk.num_params as usize != 2 {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "this method takes {} argument(s) but 1 were supplied",
                                    chunk.num_params - 1
                                ),
                            ));
                        }
                        let mut new_regs = vec![Value::unit(); chunk.num_registers as usize];
                        retain(v);
                        new_regs[0] = v;
                        retain(idx);
                        new_regs[1] = idx;
                        frames[top].pc += 1;
                        frames.push(Frame {
                            proto,
                            regs: new_regs,
                            pc: 0,
                            ret_dst: *dst,
                            ret_transform: RetTransform::None,
                            upvalues: Vec::new(),
                        });
                        continue;
                    }
                    // A built-in list addresses an element by integer position (bounds-checked).
                    if let Some(len) = v.list_len() {
                        let Some(i) = idx.as_int() else {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("list index must be an int, found {}", idx.type_name()),
                            ));
                        };
                        if i < 0 || i as usize >= len {
                            return Err(self.error(
                                DiagnosticCode::IndexOutOfBounds,
                                *span,
                                format!("index {i} out of bounds for list of length {len}"),
                            ));
                        }
                        let element = v.list_get(i as usize).expect("bounds checked above");
                        retain(element);
                        set_reg(&mut frames[top].regs, *dst, element);
                        frames[top].pc += 1;
                        continue;
                    }
                    // A map looks the value up by its string key; a missing key is `E0018`.
                    if v.is_map() {
                        let Some(key) = idx.as_string() else {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("map index must be a string, found {}", idx.type_name()),
                            ));
                        };
                        let Some(element) = v.map_get(&key) else {
                            return Err(self.error(
                                DiagnosticCode::KeyNotFound,
                                *span,
                                format!("map has no key {key:?}"),
                            ));
                        };
                        retain(element);
                        set_reg(&mut frames[top].regs, *dst, element);
                        frames[top].pc += 1;
                        continue;
                    }
                    // A string addresses a single character by position (bounds-checked),
                    // counting by Unicode scalar values to match `len`.
                    if let Some(s) = v.as_string() {
                        let Some(i) = idx.as_int() else {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("string index must be an int, found {}", idx.type_name()),
                            ));
                        };
                        let count = s.chars().count();
                        if i < 0 || i as usize >= count {
                            return Err(self.error(
                                DiagnosticCode::IndexOutOfBounds,
                                *span,
                                format!("index {i} out of bounds for string of length {count}"),
                            ));
                        }
                        let ch = s.chars().nth(i as usize).unwrap().to_string();
                        set_reg(&mut frames[top].regs, *dst, Value::string(&ch));
                        frames[top].pc += 1;
                        continue;
                    }
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!("cannot index a value of type {}", v.type_name()),
                    ));
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
                    // `...base` fills declared slots the base provides; named initializers then
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
                Op::MakeRecordInPlace {
                    dst,
                    shape,
                    named,
                    base,
                    check,
                    span,
                } => {
                    let shape = self.shapes[*shape as usize].clone();
                    // The base is consumed: take its single reference out of the register without
                    // releasing (a direct overwrite, mirroring `ConcatInPlace`), so the refcount
                    // below still counts the accumulator's reference and a `dst == base` store is
                    // safe (the old occupant is now `unit`).
                    let base_val = frames[top].regs[*base as usize];
                    frames[top].regs[*base as usize] = Value::unit();
                    let same_shape = base_val.object_shape_ptr() == Some(Rc::as_ptr(&shape));
                    let reuse = match check {
                        ReuseCheck::Static => {
                            // The linearity analysis proved sole ownership, so the **refcount** check
                            // is elided — this is the compile-time-hoisted uniqueness path. The debug
                            // assertion documents (and, in debug builds, guards) that invariant; a
                            // failure means the analysis is wrong. The shape is still guarded (a
                            // well-typed self-update always matches, but a mismatch must fall back to
                            // copy rather than corrupt the object at the wrong slot layout).
                            debug_assert!(
                                base_val.refcount() == 1,
                                "static record reuse requires a uniquely-owned base"
                            );
                            same_shape
                        }
                        ReuseCheck::Runtime => same_shape && base_val.refcount() == 1,
                    };
                    if reuse {
                        // Reuse the allocation: overwrite only the changed slots. Every unchanged
                        // field keeps base's reference, which transfers into the result — base *is*
                        // the result. The displaced old field value is routed through `release_value`
                        // (not a plain free) so its `destruct` fires at the right time — matching the
                        // copy-and-destroy baseline, which would destroy the old base and its fields
                        // (spec §4/§5). The reuse pass guarantees `base`'s own type has no destructor,
                        // so reuse never skips a container destructor.
                        for (slot, reg) in named.iter() {
                            let v = frames[top].regs[*reg as usize];
                            let old = base_val.replace_slot(*slot as usize, v);
                            self.release_value(old);
                        }
                        set_reg(&mut frames[top].regs, *dst, base_val);
                        frames[top].pc += 1;
                    } else {
                        // Aliased or a different shape: build a fresh object exactly like
                        // `MakeRecord` (spreading base's fields), then release the consumed base.
                        let mut slots: Vec<Option<Value>> = vec![None; shape.fields.len()];
                        for (i, field) in shape.fields.iter().enumerate() {
                            if let Some(value) = base_val.field(field) {
                                retain(value);
                                slots[i] = Some(value);
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
                            release(base_val);
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
                        release(base_val);
                        set_reg(&mut frames[top].regs, *dst, Value::object(shape, slots));
                        frames[top].pc += 1;
                    }
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
                Op::EnumFromStr {
                    dst,
                    arg,
                    enum_name,
                    cases,
                    some_shape,
                    none_shape,
                    panic,
                    span,
                } => {
                    let key = match frames[top].regs[*arg as usize].as_string() {
                        Some(s) => s,
                        None => {
                            let kind = if *panic { "from" } else { "try_from" };
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "`{enum_name}.{kind}` expects a string, found {}",
                                    frames[top].regs[*arg as usize].type_name()
                                ),
                            ));
                        }
                    };
                    let matched = cases.iter().find(|(name, _)| *name == key);
                    let result = match matched {
                        Some((_, shape_idx)) => {
                            // Build the payload-free case; its single reference transfers onward.
                            let shape = self.shapes[*shape_idx as usize].clone();
                            let case = Value::enum_value(shape, Vec::new());
                            if *panic {
                                case
                            } else {
                                let some = self.shapes[*some_shape as usize].clone();
                                Value::enum_value(some, vec![case])
                            }
                        }
                        None if *panic => {
                            return Err(self.error(
                                DiagnosticCode::Panic,
                                *span,
                                format!("panic: `{enum_name}` has no case `{key}`"),
                            ));
                        }
                        None => {
                            let none = self.shapes[*none_shape as usize].clone();
                            Value::enum_value(none, Vec::new())
                        }
                    };
                    set_reg(&mut frames[top].regs, *dst, result);
                    frames[top].pc += 1;
                }
                Op::LoadField {
                    dst,
                    obj,
                    field,
                    span,
                    cache,
                } => {
                    let v = frames[top].regs[*obj as usize];
                    // Inline cache: a hit (the receiver's shape pointer matches the cached one) reads
                    // the memoized slot directly; a miss resolves `slot_of` and refreshes the cache.
                    // The hit check returns an owned slot so the `&caches[ci]` borrow ends before the
                    // miss path mutates the same entry.
                    let ci = *cache as usize;
                    let hit = match &caches[ci] {
                        Some((cs, slot)) if v.object_shape_ptr() == Some(Rc::as_ptr(cs)) => {
                            Some(*slot as usize)
                        }
                        _ => None,
                    };
                    let cached_slot = match hit {
                        Some(slot) => Some(slot),
                        None => match v.shape() {
                            Some(sh) => sh.slot_of(field).inspect(|&s| {
                                caches[ci] = Some((sh.clone(), s as u32));
                            }),
                            None => None,
                        },
                    };
                    match cached_slot.and_then(|s| v.slot_at(s)) {
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
                Op::SetField {
                    dst,
                    obj,
                    field,
                    value,
                    reuse,
                    span,
                } => {
                    let v = frames[top].regs[*obj as usize];
                    let val = frames[top].regs[*value as usize];
                    let Some(slot) = v.shape().and_then(|sh| sh.slot_of(field)) else {
                        return Err(self.error(
                            DiagnosticCode::UnknownName,
                            *span,
                            if v.is_object() {
                                format!("type `{}` has no field `{field}`", v.shape().unwrap().name)
                            } else {
                                format!("cannot assign field `{field}` on {}", v.type_name())
                            },
                        ));
                    };
                    if *reuse {
                        // The receiver's sole reference moves into this op (its register cleared, like
                        // the map/record in-place paths), so the `refcount == 1` check below sees the
                        // accumulator's reference and a `dst == obj` store is safe.
                        frames[top].regs[*obj as usize] = Value::unit();
                        if v.refcount() == 1 {
                            // Unique: overwrite the slot in place (`replace_slot` retains the new
                            // value); the displaced old value's `destruct` fires now (spec §5).
                            let old = v.replace_slot(slot, val);
                            self.release_value(old);
                            set_reg(&mut frames[top].regs, *dst, v);
                        } else {
                            // Aliased: copy with the field replaced, preserving the alias's view, then
                            // release the consumed receiver reference.
                            let new = object_copy_with_slot(v, slot, val);
                            release(v);
                            set_reg(&mut frames[top].regs, *dst, new);
                        }
                    } else {
                        // Unmarked: a functional update — copy with the field replaced, the receiver
                        // register untouched (a temp receiver is dropped by the compiler-emitted Drop).
                        let new = object_copy_with_slot(v, slot, val);
                        set_reg(&mut frames[top].regs, *dst, new);
                    }
                    frames[top].pc += 1;
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
                Op::TryUnwrap {
                    dst,
                    src,
                    on_error,
                    span,
                } => {
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
                            // Drop the frame locals this `?` abandons before unwinding (Phase 4.2c) —
                            // destructor-relevant ones fire `destruct`, in the drop pass's order. Each
                            // is cleared to `unit`, so the teardown release below never double-frees.
                            for (reg, relevant) in on_error.iter() {
                                let dv = std::mem::replace(
                                    &mut frames[top].regs[*reg as usize],
                                    Value::unit(),
                                );
                                if *relevant {
                                    self.release_value(dv);
                                } else {
                                    release(dv);
                                }
                            }
                            let finished = frames.pop().unwrap();
                            for r in &finished.regs {
                                release(*r);
                            }
                            for u in &finished.upvalues {
                                release(*u);
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
                Op::Narrow {
                    dst,
                    src,
                    target,
                    some_shape,
                    none_shape,
                } => {
                    let v = frames[top].regs[*src as usize];
                    let result = if narrow_matches(v, target) {
                        retain(v);
                        let shape = self.shapes[*some_shape as usize].clone();
                        Value::enum_value(shape, vec![v])
                    } else {
                        let shape = self.shapes[*none_shape as usize].clone();
                        Value::enum_value(shape, Vec::new())
                    };
                    set_reg(&mut frames[top].regs, *dst, result);
                    frames[top].pc += 1;
                }
                Op::IsType { dst, src, target } => {
                    let v = frames[top].regs[*src as usize];
                    let result = Value::bool(narrow_matches(v, target));
                    set_reg(&mut frames[top].regs, *dst, result);
                    frames[top].pc += 1;
                }
                Op::AttributesOf { dst, type_name } => {
                    let result = self.materialize_attributes(type_name);
                    set_reg(&mut frames[top].regs, *dst, result);
                    frames[top].pc += 1;
                }
                Op::RolesOf { dst } => {
                    let result = self.materialize_roles();
                    set_reg(&mut frames[top].regs, *dst, result);
                    frames[top].pc += 1;
                }
                Op::TypeOf { dst, src } => {
                    let repr = vm_type_repr(&frames[top].regs[*src as usize]);
                    let result = build_type_value(&repr);
                    set_reg(&mut frames[top].regs, *dst, result);
                    frames[top].pc += 1;
                }
                Op::TypeOfStatic { dst, repr } => {
                    let result = build_type_value(repr);
                    set_reg(&mut frames[top].regs, *dst, result);
                    frames[top].pc += 1;
                }
                Op::TypeValue { dst, name } => {
                    // A bare type name used as a value (an `invoke` receiver) materializes as the
                    // reflection `Type` ADT — the one representation of "a type as a value", shared
                    // with `type_of` and stored type-refs. `Op::Invoke` resolves it back to the
                    // named type via `reflection_type_name`.
                    let value = build_type_value(&module.reflection.type_ref_repr(name));
                    set_reg(&mut frames[top].regs, *dst, value);
                    frames[top].pc += 1;
                }
                Op::Invoke {
                    dst,
                    recv,
                    name,
                    args,
                    ok_shape,
                    err_shape,
                    ..
                } => {
                    let recv_val = frames[top].regs[*recv as usize];
                    let name_val = frames[top].regs[*name as usize];
                    let args_val = frames[top].regs[*args as usize];
                    // Resolve the dispatch by name: either a prototype to call (`Ok`) or a reason it
                    // failed (`Err(msg)` → `Result.Err`). Every resolution failure — non-string name,
                    // non-list args, non-invokable receiver, unknown name, arity mismatch — is a
                    // runtime `Err`, never an abort (only a panic *inside* the called body aborts).
                    let outcome: Result<(u32, bool, Vec<Value>), String> = 'resolve: {
                        let Some(method) = name_val.as_string() else {
                            break 'resolve Err(format!(
                                "invoke name must be a string, found {}",
                                name_val.type_name()
                            ));
                        };
                        let Some(arg_items) = args_val.list_items() else {
                            break 'resolve Err(format!(
                                "invoke args must be a list, found {}",
                                args_val.type_name()
                            ));
                        };
                        // A type handle dispatches an associated function (no receiver); an object
                        // dispatches an instance method (receiver in register 0). A reflection `Type`
                        // value (a stored type-ref) names the type for an associated call too.
                        let (type_name, is_assoc) = if recv_val.is_object() {
                            (recv_val.shape().unwrap().name.clone(), false)
                        } else if let Some(tn) = reflection_type_name(recv_val) {
                            (tn, true)
                        } else {
                            break 'resolve Err(format!(
                                "cannot invoke on a value of type `{}`",
                                recv_val.type_name()
                            ));
                        };
                        let kind = if is_assoc {
                            "associated function"
                        } else {
                            "method"
                        };
                        let Some(&proto) = self.methods.get(&(type_name.clone(), method.clone()))
                        else {
                            break 'resolve Err(format!(
                                "type `{type_name}` has no {kind} `{method}`"
                            ));
                        };
                        // The prototype reserves register 0 for `self` (unit for an associated
                        // call), so its declared arity is one more than the supplied args; trailing
                        // defaults widen the accepted range, exactly as `Op::CallMethod`.
                        let chunk = &module.protos[proto as usize];
                        let total = chunk.num_params as usize - 1;
                        let required = total - chunk.defaults.len();
                        if arg_items.len() < required || arg_items.len() > total {
                            break 'resolve Err(arity_message(
                                kind,
                                required,
                                total,
                                arg_items.len(),
                            ));
                        }
                        Ok((proto, is_assoc, arg_items))
                    };
                    match outcome {
                        Err(message) => {
                            let shape = self.shapes[*err_shape as usize].clone();
                            let err = Value::enum_value(shape, vec![Value::string(&message)]);
                            set_reg(&mut frames[top].regs, *dst, err);
                            frames[top].pc += 1;
                        }
                        Ok((proto, is_assoc, arg_items)) => {
                            let chunk = &module.protos[proto as usize];
                            let num_registers = chunk.num_registers as usize;
                            let defaults = chunk.defaults.clone();
                            let mut new_regs = vec![Value::unit(); num_registers];
                            // An associated call leaves register 0 as unit (no receiver); an instance
                            // call places the retained receiver there.
                            if !is_assoc {
                                retain(recv_val);
                                new_regs[0] = recv_val;
                            }
                            for (i, &arg) in arg_items.iter().enumerate() {
                                retain(arg);
                                new_regs[i + 1] = arg;
                            }
                            // Fill any omitted trailing parameters from their default thunks (module
                            // scope only, like a method frame).
                            let filled = arg_items.len() + 1;
                            for (reg, proto) in &defaults {
                                if *reg as usize >= filled {
                                    let value = self.run_thunk(*proto, &[])?;
                                    new_regs[*reg as usize] = value;
                                }
                            }
                            // The result is wrapped in `Result.Ok` as it lands in the caller, so the
                            // invocation yields a `Result` whichever way the body returns.
                            let ok = self.shapes[*ok_shape as usize].clone();
                            frames[top].pc += 1;
                            frames.push(Frame {
                                proto,
                                regs: new_regs,
                                pc: 0,
                                ret_dst: *dst,
                                ret_transform: RetTransform::WrapOk(ok),
                                upvalues: Vec::new(),
                            });
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
                            // `..xs` (spread) returns the source value unchanged, so the result
                            // aliases a live heap reference — retain it before `set_reg` releases
                            // the old occupant of `dst` (which is `src`). A no-op for the fresh
                            // primitives `Neg`/`Not` produce; mirrors `Op::Move`.
                            retain(v);
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
                            upvalues: Vec::new(),
                        });
                        continue;
                    }
                    // Derived structural comparison: `< <= > >=` on an object whose type
                    // `@derive(Comparable)`s (and has no hand-written `compare`) — field-wise
                    // ordering, computed synchronously (no method to call).
                    if left.is_object()
                        && op.comparable_method().is_some()
                        && self
                            .comparable_derives
                            .contains(&left.shape().unwrap().name)
                    {
                        match structural_compare(left, right) {
                            Some(ordering) => {
                                let satisfied =
                                    op.ordering_satisfies(lang_ast::ordering_variant(ordering));
                                set_reg(&mut frames[top].regs, *dst, Value::bool(satisfied));
                                frames[top].pc += 1;
                            }
                            None => {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!(
                                        "cannot compare {} and {}",
                                        left.type_name(),
                                        right.type_name()
                                    ),
                                ));
                            }
                        }
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
                Op::Stringify { dst, src, span } => {
                    let v = frames[top].regs[*src as usize];
                    // A user object lights up the `Display` trait: render it via its `to_string`
                    // method (which runs bytecode, so it is pushed as a call frame). Matches the
                    // tree-walker's `display_value`.
                    if v.is_object() {
                        let type_name = v.shape().unwrap().name.clone();
                        if let Some(&proto) = self
                            .methods
                            .get(&(type_name.clone(), "to_string".to_string()))
                        {
                            let chunk = &module.protos[proto as usize];
                            if chunk.num_params != 1 {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!(
                                        "this method takes {} argument(s) but 0 were supplied",
                                        chunk.num_params - 1
                                    ),
                                ));
                            }
                            let mut new_regs = vec![Value::unit(); chunk.num_registers as usize];
                            retain(v);
                            new_regs[0] = v;
                            frames[top].pc += 1;
                            frames.push(Frame {
                                proto,
                                regs: new_regs,
                                pc: 0,
                                ret_dst: *dst,
                                ret_transform: RetTransform::None,
                                upvalues: Vec::new(),
                            });
                            continue;
                        }
                    }
                    // Identity for every other value: the consuming `Echo`/`Concat` stringifies
                    // it via `display`.
                    retain(v);
                    set_reg(&mut frames[top].regs, *dst, v);
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
                            let num_params = callee_chunk.num_params as usize;
                            let required = num_params - callee_chunk.defaults.len();
                            if args.len() < required || args.len() > num_params {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    arity_message("function", required, num_params, args.len()),
                                ));
                            }
                            let num_registers = callee_chunk.num_registers as usize;
                            let defaults = callee_chunk.defaults.clone();
                            // Move the arguments into the new frame's leading registers, each
                            // owning a fresh reference.
                            let mut new_regs = vec![Value::unit(); num_registers];
                            for (i, &arg_reg) in args.iter().enumerate() {
                                let v = frames[top].regs[arg_reg as usize];
                                retain(v);
                                new_regs[i] = v;
                            }
                            // The closure's captured upvalue cells, carried into the frame (one owned
                            // reference each, released at teardown) so the body can read/write
                            // through them — and handed to each default thunk, which shares the
                            // closure's upvalue layout, so a capture-referencing default reads the
                            // right cell.
                            let count = callee_val.closure_upvalue_count();
                            let cells: Vec<Value> =
                                (0..count).map(|i| callee_val.closure_upvalue(i)).collect();
                            // Fill any omitted trailing parameters from their default thunks: a
                            // default whose register is at or beyond the supplied count was not
                            // passed. (An associated function carries a synthetic unit receiver in
                            // register 0, already counted among the supplied args.)
                            let filled = args.len();
                            for (reg, proto) in &defaults {
                                if *reg as usize >= filled {
                                    let value = self.run_thunk(*proto, &cells)?;
                                    new_regs[*reg as usize] = value;
                                }
                            }
                            let mut upvalues = Vec::with_capacity(count);
                            for &cell in &cells {
                                retain(cell);
                                upvalues.push(cell);
                            }
                            // Resume after the call once the callee returns.
                            frames[top].pc += 1;
                            frames.push(Frame {
                                proto: proto_idx,
                                regs: new_regs,
                                pc: 0,
                                ret_dst: *dst,
                                ret_transform: RetTransform::None,
                                upvalues,
                            });
                        }
                        None => match callee_val.as_native_fn() {
                            // An indirect call of a first-class builtin (`f = len; f(xs)`). The
                            // arguments stay owned by their registers (the helper borrows them);
                            // the result is freshly owned.
                            Some(func) => {
                                let arg_vals: Vec<Value> =
                                    args.iter().map(|&r| frames[top].regs[r as usize]).collect();
                                let result = self.call_native_fn(func, &arg_vals, *span)?;
                                set_reg(&mut frames[top].regs, *dst, result);
                                frames[top].pc += 1;
                            }
                            None => {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!("{} is not callable", callee_val.type_name()),
                                ));
                            }
                        },
                    }
                }
                Op::Return { src } => {
                    let raw = frames[top].regs[*src as usize];
                    retain(raw); // keep alive across this frame's teardown
                    let finished = frames.pop().unwrap();
                    for r in &finished.regs {
                        release(*r);
                    }
                    for u in &finished.upvalues {
                        release(*u);
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
                    for u in &finished.upvalues {
                        release(*u);
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
                let num_params = chunk.num_params as usize;
                let num_registers = chunk.num_registers as usize;
                let required = num_params - chunk.defaults.len();
                let defaults = chunk.defaults.clone();
                if args.len() < required || args.len() > num_params {
                    let supplied = args.len();
                    for a in args {
                        release(a);
                    }
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        arity_message("function", required, num_params, supplied),
                    ));
                }
                let filled = args.len();
                let mut regs = vec![Value::unit(); num_registers];
                for (i, v) in args.into_iter().enumerate() {
                    regs[i] = v;
                }
                // A first-class closure may capture upvalues; carry its cells into the re-entrant
                // frame (one owned reference each) and hand them to each default thunk, which shares
                // the closure's upvalue layout so a capture-referencing default reads the right cell.
                let count = callee.closure_upvalue_count();
                let cells: Vec<Value> = (0..count).map(|i| callee.closure_upvalue(i)).collect();
                // Fill any omitted trailing parameters from their default thunks.
                for (reg, dproto) in &defaults {
                    if *reg as usize >= filled {
                        let value = self.run_thunk(*dproto, &cells)?;
                        regs[*reg as usize] = value;
                    }
                }
                let mut upvalues = Vec::with_capacity(count);
                for &cell in &cells {
                    retain(cell);
                    upvalues.push(cell);
                }
                self.run(vec![Frame {
                    proto,
                    regs,
                    pc: 0,
                    ret_dst: 0,
                    ret_transform: RetTransform::None,
                    upvalues,
                }])
            }
            None => match callee.as_native_fn() {
                // A first-class builtin passed as the callee (e.g. `map(xs, len)`). The args are
                // owned here, so release them after the borrowing helper returns.
                Some(func) => {
                    let result = self.call_native_fn(func, &args, span);
                    for a in &args {
                        release(*a);
                    }
                    result
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
            },
        }
    }

    /// Run a defaulted parameter's zero-argument thunk prototype to its value, on a fresh frame
    /// stack (the same re-entry `map`/`filter` callbacks use). `upvalues` are the calling closure's
    /// captured cells — the thunk is compiled with that same upvalue layout, so a default that
    /// references a captured variable reads the right cell; for a top-level function or method this
    /// is empty and the thunk resolves globals only. Each cell is retained for the thunk frame (and
    /// released at its teardown). The returned value owns one reference, transferred to its register.
    fn run_thunk(&mut self, proto: u32, upvalues: &[Value]) -> Result<Value, Abort> {
        let num_registers = self.module.protos[proto as usize].num_registers as usize;
        let mut ups = Vec::with_capacity(upvalues.len());
        for &cell in upvalues {
            retain(cell);
            ups.push(cell);
        }
        self.run(vec![Frame {
            proto,
            regs: vec![Value::unit(); num_registers],
            pc: 0,
            ret_dst: 0,
            ret_transform: RetTransform::None,
            upvalues: ups,
        }])
    }

    /// Dispatch a first-class prelude builtin called indirectly. Reuses `call_builtin` (so the
    /// arity/error text matches the direct `CallBuiltin` path exactly), except `len` on a user
    /// object, which re-enters that object's `Length` (`len`) method — mirroring the `CallBuiltin`
    /// object case. Arguments are borrowed; the result is freshly owned.
    fn call_native_fn(
        &mut self,
        func: Builtin,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        if func == Builtin::Len && args.len() == 1 && args[0].is_object() {
            let recv = args[0];
            let type_name = recv.shape().unwrap().name.clone();
            if let Some(&proto) = self.methods.get(&(type_name, "len".to_string())) {
                let chunk = &self.module.protos[proto as usize];
                if chunk.num_params != 1 {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "this method takes {} argument(s) but 0 were supplied",
                            chunk.num_params - 1
                        ),
                    ));
                }
                let mut regs = vec![Value::unit(); chunk.num_registers as usize];
                retain(recv);
                regs[0] = recv;
                return self.run(vec![Frame {
                    proto,
                    regs,
                    pc: 0,
                    ret_dst: 0,
                    ret_transform: RetTransform::None,
                    upvalues: Vec::new(),
                }]);
            }
        }
        self.call_builtin(func, args, span)
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
                    .or_else(|| v.set_len())
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

/// Lift a shared stdlib [`lang_stdlib::Output`] into a freshly-owned VM `Value` (refcount 1,
/// owned by the destination register). Mirrors the tree-walker's `output_to_value`.
/// Project a VM value onto the backend-agnostic [`lang_stdlib::Arg`] for the numeric native
/// modules (`math`/`random`). These never inspect string *content*, so a string collapses to
/// [`lang_stdlib::Arg::Other`] (same effect: it is "not a number") and no borrow is needed —
/// keeping the result `'static`. The string-method site projects strings itself, since it does
/// read their content.
fn project_arg(value: Value) -> lang_stdlib::Arg<'static> {
    if let Some(i) = value.as_int() {
        lang_stdlib::Arg::Int(i)
    } else if let Some(f) = value.as_float() {
        lang_stdlib::Arg::Float(f)
    } else if let Some(b) = value.as_bool() {
        lang_stdlib::Arg::Bool(b)
    } else {
        lang_stdlib::Arg::Other
    }
}

fn stdlib_output_to_value(output: lang_stdlib::Output) -> Value {
    match output {
        lang_stdlib::Output::Str(s) => Value::string(&s),
        lang_stdlib::Output::Bool(b) => Value::bool(b),
        lang_stdlib::Output::Int(i) => Value::int(i),
        lang_stdlib::Output::Float(f) => Value::float(f),
        lang_stdlib::Output::StrList(items) => {
            Value::list(items.iter().map(|s| Value::string(s)).collect())
        }
    }
}

/// Map a stdlib misuse kind onto a diagnostic code, matching the tree-walker: arity/argument-type
/// mistakes are a `TypeMismatch`; an out-of-range index/range is an `IndexOutOfBounds`.
fn stdlib_error_code(kind: lang_stdlib::ErrorKind) -> DiagnosticCode {
    match kind {
        lang_stdlib::ErrorKind::Arity | lang_stdlib::ErrorKind::ArgType => {
            DiagnosticCode::TypeMismatch
        }
        lang_stdlib::ErrorKind::Bounds => DiagnosticCode::IndexOutOfBounds,
        lang_stdlib::ErrorKind::UnknownName => DiagnosticCode::UnknownName,
        lang_stdlib::ErrorKind::Io => DiagnosticCode::IoError,
    }
}

/// Build a set's canonical form from `items`: every element must be mutually orderable (a single
/// orderable primitive — int, float, or string); the result is sorted and de-duplicated. Returns
/// `None` if any element is non-orderable or of a different kind. Mirrors the tree-walker's
/// `canonical_set` so both backends build identical sets. The returned values are still shared
/// (not retained) — the caller retains those it keeps.
/// A shallow copy of object `obj` with slot `slot` replaced by `value` — the copy-on-write path for
/// `x.f = v` on a shared (or unmarked) instance. Each slot of the new object (the unchanged ones and
/// `value`) is retained, since `Value::object` adopts one reference per slot; `obj` itself is left
/// untouched (the caller decides whether to release it). The caller must have checked `obj` is an
/// object and `slot` is in range.
fn object_copy_with_slot(obj: Value, slot: usize, value: Value) -> Value {
    let shape = obj.shape().expect("object_copy_with_slot on a non-object");
    let mut slots = obj.slots().expect("object_copy_with_slot on a non-object");
    slots[slot] = value;
    for &s in &slots {
        retain(s);
    }
    Value::object(shape, slots)
}

fn canonical_set(items: &[Value]) -> Option<Vec<Value>> {
    if items.is_empty() {
        return Some(Vec::new());
    }
    if items
        .iter()
        .any(|&item| compare_primitive(items[0], item).is_none())
    {
        return None;
    }
    let mut canonical = items.to_vec();
    canonical.sort_by(|&a, &b| compare_primitive(a, b).unwrap_or(std::cmp::Ordering::Equal));
    canonical.dedup_by(|&mut a, &mut b| compare_primitive(a, b) == Some(std::cmp::Ordering::Equal));
    Some(canonical)
}

/// Build a built-in `Ordering` enum value (`Ordering.Less`/`Equal`/`Greater`) with a fresh shape.
/// Shapes carry no identity for matching or equality (both compare by name + variant), so an
/// on-the-fly shape is interchangeable with any other `Ordering` shape — including the
/// tree-walker's, which is what keeps the differential identical.
fn make_ordering(variant: &str) -> Value {
    let shape = Rc::new(Shape::enum_variant("Ordering", variant, Vec::new(), false));
    Value::enum_value(shape, Vec::new())
}

/// Build a role enum value (`Semantic.EntryPoint`, `WebRole.Controller`, …) with a fresh shape —
/// the payload-free `roles_of()` counterpart to [`make_ordering`], for whichever `@semantic` enum a
/// `@role` tag named. Matches the tree-walker's by structural equality.
fn make_role(enum_name: &str, variant: &str) -> Value {
    let shape = Rc::new(Shape::enum_variant(enum_name, variant, Vec::new(), false));
    Value::enum_value(shape, Vec::new())
}

/// Classify a runtime value into its **head-constructor** [`TypeRepr`] (`type_of`, fidelity B).
/// Generics are erased at runtime, so a container's element/argument types collapse to `Dyn`.
/// Mirrors the tree-walker's `eval_type_repr` exactly so both backends reflect identical `Type`
/// values; the classification follows the same kind order as [`Value::type_name`].
fn vm_type_repr(value: &Value) -> lang_ast::reflect::TypeRepr {
    use lang_ast::reflect::TypeRepr;
    let v = *value;
    let dyn_ = || Box::new(TypeRepr::Dyn);
    let shape_name = || v.shape().map(|s| s.name.clone()).unwrap_or_default();
    match v.type_name() {
        "bool" => TypeRepr::Bool,
        "int" => TypeRepr::Int,
        "float" => TypeRepr::Float,
        "string" => TypeRepr::Str,
        "unit" => TypeRepr::Unit,
        "list" => TypeRepr::List(dyn_()),
        "set" => TypeRepr::Set(dyn_()),
        "map" => TypeRepr::Map(dyn_(), dyn_()),
        "function" => TypeRepr::Fn(Vec::new(), dyn_()),
        "object" => match v.shape().map(|s| s.kind) {
            Some(ShapeKind::Class) => TypeRepr::Class(shape_name(), Vec::new()),
            _ => TypeRepr::Record(shape_name(), Vec::new()),
        },
        "enum" => match shape_name().as_str() {
            "Option" => TypeRepr::Option(dyn_()),
            "Result" => TypeRepr::Result(dyn_(), dyn_()),
            other => TypeRepr::Enum(other.to_string(), Vec::new()),
        },
        // A module or file handle has no nameable lattice type: it reflects as the top.
        _ => TypeRepr::Dyn,
    }
}

/// Build the prelude `Type` enum value from a [`TypeRepr`], recursively. Each node is a freshly
/// constructed enum value (refcount 1) owned by its parent, with an on-the-fly shape — structurally
/// interchangeable with the tree-walker's, which keeps the differential identical.
fn build_type_value(repr: &lang_ast::reflect::TypeRepr) -> Value {
    use lang_ast::reflect::{TYPE_ENUM, TypeRepr};
    let data: Vec<Value> = match repr {
        TypeRepr::Int
        | TypeRepr::Float
        | TypeRepr::Bool
        | TypeRepr::Str
        | TypeRepr::Unit
        | TypeRepr::Dyn => Vec::new(),
        TypeRepr::List(t) | TypeRepr::Set(t) | TypeRepr::Option(t) => {
            vec![build_type_value(t)]
        }
        TypeRepr::Map(k, v) | TypeRepr::Result(k, v) => {
            vec![build_type_value(k), build_type_value(v)]
        }
        TypeRepr::Enum(name, args)
        | TypeRepr::Record(name, args)
        | TypeRepr::Class(name, args)
        | TypeRepr::Named(name, args) => vec![
            Value::string(name),
            Value::list(args.iter().map(build_type_value).collect()),
        ],
        TypeRepr::Fn(params, ret) => vec![
            Value::list(params.iter().map(build_type_value).collect()),
            build_type_value(ret),
        ],
        TypeRepr::Union(members) => {
            vec![Value::list(members.iter().map(build_type_value).collect())]
        }
    };
    let shape = Rc::new(Shape::enum_variant(
        TYPE_ENUM,
        repr.variant_name(),
        Vec::new(),
        false,
    ));
    Value::enum_value(shape, data)
}

/// Convert a manifest attribute-argument literal tree to a VM value (for materializing an attribute
/// record), recursing through the collection and nominal literals. A type reference materializes as
/// the reflection `Type` ADT classified by the named type's *kind* (via the shared
/// [`reflect::ReflectionInfo::type_ref_repr`]); a set is canonicalized exactly like the runtime
/// `to_set`. Mirrors the tree-walker's `attr_value_to_eval` element-for-element, so the materialized
/// attribute agrees across the differential by construction.
fn attr_value_to_vm(
    value: &lang_ast::AttrValue,
    reflection: &lang_ast::reflect::ReflectionInfo,
) -> Value {
    use lang_ast::AttrValue as A;
    let recur = |v: &A| attr_value_to_vm(v, reflection);
    match value {
        A::Str(s) => Value::string(s),
        A::Int(n) => Value::int(*n),
        A::Float(f) => Value::float(*f),
        A::Bool(b) => Value::bool(*b),
        A::List(items) => Value::list(items.iter().map(recur).collect()),
        A::Set(items) => {
            let vals: Vec<Value> = items.iter().map(recur).collect();
            Value::set(canonical_set(&vals).unwrap_or(vals))
        }
        A::Map(entries) => {
            let mut map = BTreeMap::new();
            for (k, v) in entries {
                map.insert(k.clone(), recur(v));
            }
            Value::map(map)
        }
        A::Enum {
            enum_name,
            variant,
            args,
        } => make_attr_enum(enum_name, variant, args.iter().map(recur).collect()),
        A::Record { type_name, fields } => {
            let names: Vec<String> = fields.iter().map(|(n, _)| n.clone()).collect();
            let shape = Rc::new(Shape::object(ShapeKind::Record, type_name, names));
            let values: Vec<Value> = fields.iter().map(|(_, v)| recur(v)).collect();
            Value::object(shape, values)
        }
        A::TypeRef(name) => build_type_value(&reflection.type_ref_repr(name)),
    }
}

/// If `value` is a reflection `Type` value naming a nominal type (`Type.Named`/`Record`/`Class`/
/// `Enum`, whose first payload is the type's name), return that name — so a stored type reference
/// can be used as an `invoke` receiver. Mirrors the tree-walker's `reflection_type_name`.
fn reflection_type_name(value: Value) -> Option<String> {
    let shape = value.shape()?;
    let is_nominal = shape.name == lang_ast::reflect::TYPE_ENUM
        && shape
            .variant
            .as_deref()
            .is_some_and(|v| matches!(v, "Named" | "Record" | "Class" | "Enum"));
    if is_nominal {
        return value
            .enum_data()?
            .into_iter()
            .next()
            .and_then(|v| v.as_string());
    }
    None
}

/// Build an enum value (`Color.Red`, `Ok(5)`, `Option.none`) for an attribute argument, with a fresh
/// payload-free or payload-carrying shape. Matches the tree-walker's `builtin_enum` by structural
/// shape equality.
fn make_attr_enum(enum_name: &str, variant: &str, data: Vec<Value>) -> Value {
    let shape = Rc::new(Shape::enum_variant(
        enum_name,
        variant,
        Vec::new(),
        !data.is_empty(),
    ));
    Value::enum_value(shape, data)
}

/// The arity-mismatch message, worded identically to the tree-walker's (so the differential
/// matches). `kind` is `"function"` or `"method"`; the range form appears only when some
/// parameters are defaulted (`required < total`).
fn arity_message(kind: &str, required: usize, total: usize, supplied: usize) -> String {
    if required == total {
        format!("this {kind} takes {total} argument(s) but {supplied} were supplied")
    } else {
        format!(
            "this {kind} takes between {required} and {total} argument(s) but {supplied} were supplied"
        )
    }
}

/// Build the built-in `Option::some(value)` with a fresh shape (the `builtin_result_option` flag
/// makes it render as `some(..)`, matching the tree-walker and the compiler-lowered `some(x)`).
/// The enum owns one reference to `value`, so the caller must have retained it first.
fn make_some(value: Value) -> Value {
    let shape = Rc::new(Shape::enum_variant("Option", "some", Vec::new(), true));
    Value::enum_value(shape, vec![value])
}

/// Build the built-in `Option::none` (no payload), matching the tree-walker / compiler `none`.
fn make_none() -> Value {
    let shape = Rc::new(Shape::enum_variant("Option", "none", Vec::new(), true));
    Value::enum_value(shape, Vec::new())
}

/// Convert a parsed JSON tree into a VM value: arrays become lists, objects become sorted-key
/// maps, `null` becomes unit. Each value is freshly built (refcount 1), so the containers own
/// their children without extra retains. Mirrors the tree-walker's `json_to_value`.
fn json_to_value(json: lang_stdlib::json::Json) -> Value {
    use lang_stdlib::json::Json;
    match json {
        Json::Null => Value::unit(),
        Json::Bool(b) => Value::bool(b),
        Json::Int(i) => Value::int(i),
        Json::Float(f) => Value::float(f),
        Json::Str(s) => Value::string(&s),
        Json::Array(items) => Value::list(items.into_iter().map(json_to_value).collect()),
        Json::Object(entries) => Value::map(
            entries
                .into_iter()
                .map(|(k, v)| (k, json_to_value(v)))
                .collect(),
        ),
    }
}

/// Turn a compile-time constant into a freshly-owned runtime value.
fn materialize(c: &Const) -> Value {
    match c {
        Const::Unit => Value::unit(),
        Const::Bool(b) => Value::bool(*b),
        Const::Int(i) => Value::int(*i),
        Const::Float(f) => Value::float(*f),
        Const::Str(s) => Value::string(s),
        Const::NativeModule(name) => Value::native_module(name),
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

    /// Peak heap residency for one program (architecture §0.3) — `reset_peak` before, `live_peak`
    /// after, so the high-water mark is measured in isolation.
    fn peak_residency(src: &str) -> usize {
        lang_value::reset_peak();
        let _ = run(src);
        lang_value::live_peak()
    }

    #[test]
    fn mm_peak_residency_baseline() {
        // The pre-migration peak-residency snapshot for `plans/memory-management/phase-0-benchmarks`.
        // Prints under `--nocapture`; asserts the meter reflects each program's footprint shape.

        // Allocation churn: each short-lived record dies before the next is built ⇒ a small,
        // n-independent peak (the reclaim-at-last-use shape we already have on a local temp).
        let churn = "class Pair { a: int b: int }\nmut total = 0;\nfor i in 0..4000 { p = Pair { a: i, b: i }; total = total + p.a; }\necho total;\n";
        let churn_peak = peak_residency(churn);

        // A monotonically-growing accumulator of **heap** elements (records — ints would be immediate
        // and never counted). Peak ≈ n live objects at the end: the genuinely-live structure prompt
        // reclamation cannot shrink, but whose transient cost reuse/COW keeps O(n) not O(n²).
        let accumulate = "class Pair { a: int b: int }\nmut acc = [];\nfor i in 0..4000 { acc ~= [Pair { a: i, b: i }]; }\necho acc.count();\n";
        let accumulate_peak = peak_residency(accumulate);

        // (Deep-nested teardown is benched separately on the optimized bench profile — its recursive
        // `free` overflows this 2 MiB debug test thread at shallow depth, the MM limitation recorded
        // in `phase-0-benchmarks.md`; it is not measured here.)

        eprintln!(
            "MM peak residency (objects): alloc_churn(n=4000)={churn_peak}  accumulate_records(n=4000)={accumulate_peak}"
        );

        // Shape assertions (not exact counts — those are the recorded baseline): churn stays small and
        // n-independent; the record accumulator's peak scales with n.
        assert!(churn_peak < 100, "alloc churn peak should be n-independent");
        assert!(
            accumulate_peak >= 4000,
            "record-accumulator peak should scale with n"
        );
    }

    /// A function with `n` **single-assignment** intermediate records chained `aᵢ = f(aᵢ₋₁)`, each
    /// dead once the next is built. Returns a scalar so nothing heap stays live past the chain.
    fn sequential_intermediates_src(n: usize) -> String {
        let mut body = String::from("  a0 = Pair { a: 1, b: 1 };\n");
        for i in 1..n {
            body.push_str(&format!(
                "  a{i} = Pair {{ a: a{prev}.a + 1, b: a{prev}.b }};\n",
                prev = i - 1
            ));
        }
        format!(
            "class Pair {{ a: int b: int }}\nfn chain(): int {{\n{body}  return a{last}.a;\n}}\necho chain();\n",
            last = n - 1
        )
    }

    #[test]
    fn mm_peak_residency_prompt_reclamation_is_n_independent() {
        // The headline Phase-3 metric (memory-management `phase-3-rc-passes` gate): precise last-use
        // drops reclaim a function-local the moment it dies, so a straight-line chain of n transient
        // intermediates holds only ~the current+previous record live at once — an O(1), n-INDEPENDENT
        // peak. Under the pre-migration reclaim-at-teardown model every aᵢ stayed live until `chain`
        // returned, an O(n) peak. We prove the win by its shape: the peak must not grow with n.
        let small = peak_residency(&sequential_intermediates_src(50));
        let large = peak_residency(&sequential_intermediates_src(400));
        eprintln!(
            "MM peak residency (objects): sequential_intermediates n=50={small}  n=400={large}"
        );
        // n-independence is the proof of prompt reclamation: 8× the chain length leaves the peak flat
        // (a tiny constant — the live window — not 8× larger). A generous bound absorbs allocator slack
        // while still failing hard if drops regressed to teardown reclamation (which would be ≈ n).
        assert!(
            small < 20 && large < 20,
            "prompt last-use reclamation should keep the intermediate-chain peak O(1); got n=50→{small}, n=400→{large}"
        );
    }

    #[test]
    fn invoke_by_name_wraps_ok_and_err() {
        // `invoke` dispatches by runtime name: a hit wraps the return in `Result.Ok` (via the
        // `WrapOk` frame transform); an unknown name / arity mismatch builds `Result.Err`. Exercises
        // the new type-handle value, the `Op::Invoke` dispatch, and the refcount handoff on return.
        let r = run(
            "class Box {\n  v: int\n  fn new(v: int): Box { return Box { v: v }; }\n  fn doubled(): int { return v * 2; }\n}\nhit = match invoke(Box.new(21), \"doubled\", []) { Ok(v) => \"${v}\", Err(e) => \"err ${e}\" };\necho hit;\nmade = match invoke(Box, \"new\", [7]) { Ok(b) => match invoke(b, \"doubled\", []) { Ok(d) => \"${d}\", Err(_) => \"x\" }, Err(_) => \"x\" };\necho made;\nmiss = match invoke(Box.new(1), \"nope\", []) { Ok(_) => \"ok\", Err(_) => \"miss\" };\necho miss;\n",
        );
        assert_eq!(r.stdout, "42\n14\nmiss\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn type_of_distinguishes_nominal_kinds() {
        // `type_of` classifies a value's shape kind into `Type.Enum`/`Type.Record`/`Type.Class`
        // (not a collapsed `Named`). Exercises `vm_type_repr` + `build_type_value`'s kind arms and
        // their refcount handoff.
        let r = run(
            "enum E { A; }\ntype R = { x: int };\nclass C {\n  v: int\n  fn new(): C { return C { v: 1 }; }\n}\nfn k(t: Type): string { return match t { Type.Enum(n, _) => \"e:${n}\", Type.Record(n, _) => \"r:${n}\", Type.Class(n, _) => \"c:${n}\", _ => \"?\" }; }\necho k(type_of(E.A));\necho k(type_of(R { x: 1 }));\necho k(type_of(C.new()));\n",
        );
        assert_eq!(r.stdout, "e:E\nr:R\nc:C\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn abstract_kind_is_tests() {
        // `is Enum`/`Record`/`Class` are runtime kind tests over a `dyn` value, keyed on the
        // value's shape kind. Exercises the new `narrow_matches` arms in the VM.
        let r = run(
            "enum E { A; }\ntype R = { x: int };\nclass C {\n  v: int\n  fn new(): C { return C { v: 1 }; }\n}\ne: dyn = E.A;\nrec: dyn = R { x: 1 };\nc: dyn = C.new();\necho e is Enum;\necho rec is Record;\necho c is Class;\necho e is Record;\n",
        );
        assert_eq!(r.stdout, "true\ntrue\ntrue\nfalse\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn roles_of_materializes_the_index() {
        // `roles_of()` materializes the `(declaration, role)` index into a `List<RoleBinding>`,
        // each carrying a fresh `string` target and the named enum value. Exercises `materialize_roles`
        // and `make_role` plus the refcount handoff of the freshly-built list/record/enum values.
        let r = run(
            "@attribute(Function)\n@role(Semantic.EntryPoint)\ntype Route = { path: string };\n#[Route(\"/x\")]\nfn handle(): int { return 1; }\nfor b in roles_of() {\n  echo match b.role { Semantic.EntryPoint => \"${b.target}=entry\", _ => \"other\" };\n}\n",
        );
        assert_eq!(r.stdout, "handle=entry\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn arithmetic_and_concat() {
        let r = run("echo 1 + 2 * 3;\necho \"users/\" ~ 42 ~ \"/profile\";\n");
        assert_eq!(r.stdout, "7\nusers/42/profile\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn cow_in_place_append_paths() {
        // VM-side copy-on-write self-append (`~=`). Covers: a GLOBAL accumulator (TakeGlobal +
        // ConcatInPlace) on the unique path (`g ~= ["b"]`) and the aliased path (`h = g; g ~= ["c"]`
        // — the alias must keep `h` at the pre-append value, so COW copies); and a LOCAL accumulator
        // inside a function (the register path, int elements). Heap elements (strings) exercise the
        // element-retain accounting; run under miri to validate refcounts (no UAF / double free).
        let r = run(
            "mut g = [\"a\"];\ng ~= [\"b\"];\nh = g;\ng ~= [\"c\"];\necho g;\necho h;\nfn build(): List<int> {\n    mut acc = [];\n    for i in 0..3 {\n        acc ~= [i];\n    }\n    return acc;\n}\necho build();\n",
        );
        assert_eq!(
            r.stdout,
            "[\"a\", \"b\", \"c\"]\n[\"a\", \"b\"]\n[0, 1, 2]\n"
        );
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn record_update_reuse_paths() {
        // VM-side record-update reuse (`acc = T { ...acc, … }`). Covers the RUNTIME-checked
        // `Op::MakeRecordInPlace` paths reached via a GLOBAL accumulator (`TakeGlobal` exposes the
        // taken-out value's uniqueness; Phase 5.1b): (1) the in-place hit — a global whose update
        // overwrites a field, with a HEAP field (`tag`) whose reference must transfer untouched across
        // the reuse; (2) the copy fallback — an aliased accumulator (`snap = acc`) must keep `snap` at
        // the pre-update value (the runtime refcount > 1 forces the copy). Heap fields exercise the
        // slot retain/release accounting; run under miri to validate refcounts (no UAF/double free).
        let r = run(
            "class Point {\n  x: int\n  tag: string\n  fn show(): string { return \"${x} ${tag}\"; }\n}\nmut acc = Point { x: -1, tag: \"k\" };\nfor i in 0..4 {\n  acc = Point { ...acc, x: i };\n}\necho acc.show();\nmut p = Point { x: 1, tag: \"a\" };\nsnap = p;\np = Point { ...p, x: 9 };\necho p.show();\necho snap.show();\n",
        );
        assert_eq!(r.stdout, "3 k\n9 a\n1 a\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn map_update_reuse_paths() {
        // VM-side in-place map update (`m[k] = v` ⟶ `m = m.set(k, v)`; Phase 5.1c). Covers the two
        // runtime paths of a reuse-marked local map self-update: (1) the in-place hit — a uniquely-owned
        // accumulator mutated in place, including overwriting a key (its displaced HEAP value released)
        // and removing one; (2) the copy fallback — an aliased accumulator (`snap = m`) must keep `snap`
        // at the pre-update value. String values exercise the slot retain/release accounting; run under
        // miri to validate refcounts (no UAF / double free).
        let r = run(
            "fn build(): string {\n  mut m = {};\n  for i in 0..3 { m[\"k${i}\"] = \"v${i}\"; }\n  m[\"k0\"] = \"x\";\n  m = m.remove(\"k1\");\n  return \"${m.values()} ${m.count()}\";\n}\necho build();\nmut acc = { \"a\": \"1\" };\nsnap = acc;\nacc[\"a\"] = \"9\";\nacc[\"b\"] = \"2\";\necho acc.values();\necho snap.values();\n",
        );
        assert_eq!(r.stdout, "[\"x\", \"v2\"] 2\n[\"9\", \"2\"]\n[\"1\"]\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn list_set_reuse_paths() {
        // VM-side in-place list `set` (`xs[i] = v` ⟶ `xs = xs.set(i, v)`). Covers the in-place hit — a
        // function-local accumulator overwrites each slot in place, its displaced HEAP element released
        // each step — and the copy fallback — an aliased accumulator (`snap = ys`) keeps its value.
        // String elements exercise the slot retain/release accounting; run under miri (no UAF / double
        // free).
        let r = run(
            "fn build(): string {\n  mut xs = [\"a\", \"b\", \"c\"];\n  for i in 0..3 { xs[i] = \"v${i}\"; }\n  return xs.join(\",\");\n}\necho build();\nmut ys = [\"x\", \"y\"];\nsnap = ys;\nys[0] = \"z\";\necho ys.join(\",\");\necho snap.join(\",\");\n",
        );
        assert_eq!(r.stdout, "v0,v1,v2\nz,y\nx,y\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn set_update_reuse_paths() {
        // VM-side in-place set update (`s = s.add(x)` / `s = s.remove(x)`). Covers the in-place hit —
        // a function-local accumulator binary-search-inserts/removes one element in its existing
        // canonical buffer, including a duplicate `add` (a no-op) and a `remove` — and the copy
        // fallback — an aliased accumulator (`snap = t`) keeps its value. String elements exercise the
        // element retain/release accounting; run under miri (no UAF / double free).
        let r = run(
            "fn build(): string {\n  mut s = #{};\n  for i in 0..3 { s = s.add(\"v${i}\"); }\n  s = s.add(\"v0\");\n  s = s.remove(\"v1\");\n  return \"${s.count()}\";\n}\necho build();\nmut t = #{\"a\", \"b\"};\nsnap = t;\nt = t.add(\"c\");\nt = t.remove(\"a\");\necho t;\necho snap;\n",
        );
        assert_eq!(r.stdout, "2\n{\"b\", \"c\"}\n{\"a\", \"b\"}\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn mut_field_set_reuse_paths() {
        // VM-side in-place `mut` field assignment (`x.f = v`, Phase 5.2). Covers the in-place hit — a
        // function-local accumulator overwrites its `mut` fields each iteration (its displaced HEAP
        // field, a string, released each step) — and the copy fallback — an aliased snapshot
        // (`snap = p`) keeps its value because the shared instance is copied before the write. The
        // string field exercises the slot retain/release accounting; run under miri (no UAF / double
        // free).
        let r = run(
            "class Box {\n  mut tag: string\n  mut n: int\n  fn new(): Box { return Box { tag: \"init\", n: 0 }; }\n}\nfn build(): string {\n  mut b = Box.new();\n  for i in 0..3 { b.n = b.n + i; b.tag = \"t${i}\"; }\n  return \"${b.tag} ${b.n}\";\n}\necho build();\nmut p = Box.new();\nsnap = p;\np.tag = \"changed\";\necho p.tag;\necho snap.tag;\n",
        );
        assert_eq!(r.stdout, "t2 3\nchanged\ninit\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn record_reassign_reuse_paths() {
        // VM-side whole-value record reassignment reuse (`p = P { … }`, no spread; Phase 5 general
        // reassignment). The reuse pass injects a `...p` spread (a record literal sets every field, so
        // it is value-identical), so this lowers to `MakeRecordInPlace` overwriting *all* slots — the
        // in-place hit reuses `p`'s cell across the loop (its displaced HEAP field `tag` released each
        // step), while an aliased reassignment (`snap = q`) copies to preserve `snap`. Run under miri to
        // validate the all-slot overwrite's retain/release accounting (no UAF / double free).
        let r = run(
            "class P {\n  n: int\n  tag: string\n  fn show(): string { return \"${n} ${tag}\"; }\n}\nfn build(): string {\n  mut p = P { n: 0, tag: \"a\" };\n  for i in 0..3 { p = P { n: i, tag: \"t${i}\" }; }\n  return p.show();\n}\necho build();\nmut q = P { n: 1, tag: \"x\" };\nsnap = q;\nq = P { n: 9, tag: \"y\" };\necho q.show();\necho snap.show();\n",
        );
        assert_eq!(r.stdout, "2 t2\n9 y\n1 x\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn record_update_reuse_with_self_read() {
        // Drop insertion (Step B): a self-update that *reads* the accumulator
        // (`acc = Point { ...acc, x: acc.x + 1 }`) reuses in place — the `Drop` after the `acc.x`
        // `LoadField` frees the receiver temporary, restoring unique ownership before the construct.
        // Covers a LOCAL accumulator (Step A: no declaration `Move`) inside a function with a HEAP
        // field carried across each in-place update. Run under miri to validate the `Drop` does not
        // double-free the receiver and the carried heap field's refcount stays balanced.
        let r = run(
            "class Point {\n  x: int\n  label: string\n  fn show(): string { return \"${x} ${label}\"; }\n}\nfn run(n: int): string {\n  mut acc = Point { x: 0, label: \"p\" };\n  for i in 0..n {\n    acc = Point { ...acc, x: acc.x + 2 };\n  }\n  return acc.show();\n}\necho run(5);\n",
        );
        assert_eq!(r.stdout, "10 p\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn in_place_reuse_fires_replaced_field_destructor() {
        // Phase 5.1a: a function-local self-update of a destructor-free `Box` reuses in place, but the
        // *replaced* field `r` (a destructor-bearing `Res`) must run its `destruct` at the update via
        // the in-place path's `replace_slot` + `release_value`. Run under miri to validate the
        // displaced field is released exactly once (no UAF / double-free) and the carried field `n`
        // stays balanced.
        let r = run(
            "class Res {\n  id: int\n  fn new(id: int): Res { return Res { id: id }; }\n  destruct { echo \"drop ${id}\"; }\n}\nclass Box {\n  r: Res\n  n: int\n}\nfn run(): void {\n  mut acc = Box { r: Res.new(0), n: 7 };\n  acc = Box { ...acc, r: Res.new(1) };\n  echo \"n=${acc.n}\";\n}\nrun();\n",
        );
        assert_eq!(r.stdout, "drop 0\nn=7\ndrop 1\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn heap_element_list_concat_refcounts() {
        // Probe: concatenating lists of HEAP elements (strings) must keep element refcounts
        // balanced (no UAF / double free at teardown). Run under miri to validate.
        let r = run(
            "mut acc = [\"a\", \"b\"];\nacc = acc ~ [\"c\"];\nacc ~= [\"d\"];\nb = acc;\nacc ~= [\"e\"];\necho acc;\necho b;\n",
        );
        assert_eq!(
            r.stdout,
            "[\"a\", \"b\", \"c\", \"d\", \"e\"]\n[\"a\", \"b\", \"c\", \"d\"]\n"
        );
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
            "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  destruct { echo \"close ${name}\"; }\n}\na = R.new(\"a\");\nb = R.new(\"b\");\necho \"body\";\n",
        );
        // Globals destroyed in reverse declaration order: b before a.
        assert_eq!(r.stdout, "body\nclose b\nclose a\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn destructor_fires_at_a_locals_last_use_not_at_program_end() {
        // Phase 4: a destructor-bearing function **local** runs its `destruct` at its last use —
        // here the `r.announce()` call — before the function returns, not deferred to program end.
        // The bare `compile` path marks every drop conservatively relevant, so the local's
        // `Op::Drop` routes through `release_value` and fires the destructor.
        let r = run(
            "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  fn announce(): void { echo \"here ${name}\"; }\n  destruct { echo \"close ${name}\"; }\n}\nfn scope(): void {\n  r = R.new(\"x\");\n  r.announce();\n  echo \"after\";\n}\necho \"start\";\nscope();\necho \"end\";\n",
        );
        // `r`'s last use is `r.announce()`; the destructor fires right after it returns, before
        // "after" — and definitely before program end ("end").
        assert_eq!(r.stdout, "start\nhere x\nclose x\nafter\nend\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn reassigning_a_binding_destroys_the_displaced_value() {
        let r = run(
            "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  destruct { echo \"close ${name}\"; }\n}\nmut x = R.new(\"first\");\nx = R.new(\"second\");\necho \"mid\";\n",
        );
        // "first" is destroyed at the reassignment; "second" at program end.
        assert_eq!(r.stdout, "close first\nmid\nclose second\n");
    }

    #[test]
    fn reassigning_a_local_destroys_displaced_then_survivor_at_scope_exit() {
        // Phase 4.2a: a reassigned **local** (not a global) destroys its displaced value at the
        // assignment via the `Op::Drop` the compiler emits before the overwriting `Op::Move`
        // (`set_reg`'s plain release would not fire the destructor), and its surviving value via the
        // function-body scope-exit drop. "first" closes between the two reads; "second" before return.
        let r = run(
            "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  fn use_it(): void { echo \"use ${name}\"; }\n  destruct { echo \"close ${name}\"; }\n}\nfn go(): void {\n  mut r = R.new(\"first\");\n  r.use_it();\n  r = R.new(\"second\");\n  r.use_it();\n}\necho \"start\";\ngo();\necho \"end\";\n",
        );
        assert_eq!(
            r.stdout,
            "start\nuse first\nclose first\nuse second\nclose second\nend\n"
        );
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn question_mark_propagation_destroys_abandoned_locals() {
        // Phase 4.2c: a `?` that early-returns an `Err` destroys the frame locals it abandons before
        // unwinding (the `on_error` drops the compiler attaches to `Op::TryUnwrap`). `r` is live past
        // the `?`, so `close r` fires on the error path, before the caller prints the propagated Err.
        let r = run(
            "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  destruct { echo \"close ${name}\"; }\n}\nfn check(c: bool): Result<int, string> {\n  if c { return Ok(1); }\n  return Err(\"bad\");\n}\nfn go(c: bool): Result<int, string> {\n  r = R.new(\"r\");\n  x = check(c)?;\n  return Ok(x);\n}\necho \"start\";\necho go(false);\necho \"end\";\n",
        );
        assert_eq!(r.stdout, "start\nclose r\nErr(bad)\nend\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn panic_destroys_live_frame_locals_in_reverse_construction_order() {
        // Phase 4.2c-ii: as a panic aborts, the VM's per-frame teardown fires the `destruct` of each
        // live destructor-bearing frame local (the `frame_locals` list reversed), so `a` and `b` are
        // destroyed — `b` before `a` — before the program exits 1. They are never read, so they live
        // undropped to the panic; the panic-aware `coalesce` pinning keeps them in distinct registers.
        let r = run(
            "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  destruct { echo \"close ${name}\"; }\n}\nfn go(): void {\n  a = R.new(\"a\");\n  b = R.new(\"b\");\n  echo \"made\";\n  panic(\"boom\");\n}\necho \"start\";\ngo();\n",
        );
        assert_eq!(r.stdout, "start\nmade\nclose b\nclose a\n");
        assert_eq!(r.exit_code, 1);
    }

    #[test]
    fn destroying_a_container_runs_its_destructor_then_its_fields_in_declared_order() {
        // Phase 4.3 (spec §4): destroying an object runs the container's own `destruct` first (its
        // fields still live), then releases its fields depth-first in declared order, each firing its
        // own `destruct`. `Outer`'s two destructor-bearing `Leaf` fields are built inline (so the
        // record holds the sole reference — the construction-temp release makes refcount 1 here), and
        // `o` is a dead-store dropped at scope exit: `outer`, then `a`, then `b` (declared order).
        let r = run(
            "class Leaf {\n  tag: string\n  fn new(tag: string): Leaf { return Leaf { tag: tag }; }\n  destruct { echo \"drop ${tag}\"; }\n}\nclass Outer {\n  label: string\n  a: Leaf\n  b: Leaf\n  fn new(): Outer { return Outer { label: \"o\", a: Leaf.new(\"a\"), b: Leaf.new(\"b\") }; }\n  destruct { echo \"drop outer ${label}\"; }\n}\nfn go(): void {\n  o = Outer.new();\n  echo \"built\";\n}\necho \"start\";\ngo();\necho \"end\";\n",
        );
        assert_eq!(
            r.stdout,
            "start\nbuilt\ndrop outer o\ndrop a\ndrop b\nend\n"
        );
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn destroying_a_list_runs_its_elements_destructors_in_order() {
        // Phase 4.3 (spec §4): a collection releases its elements in iteration order. The list has no
        // `destruct`; its contained `Leaf`s do, and fire a, b, c (index order) when the list dies. The
        // construction-temp releases make the list the sole owner, so each element is at refcount 1.
        let r = run(
            "class Leaf {\n  tag: string\n  fn new(tag: string): Leaf { return Leaf { tag: tag }; }\n  destruct { echo \"drop ${tag}\"; }\n}\nfn go(): void {\n  items = [Leaf.new(\"a\"), Leaf.new(\"b\"), Leaf.new(\"c\")];\n  echo \"built\";\n}\necho \"start\";\ngo();\necho \"end\";\n",
        );
        assert_eq!(r.stdout, "start\nbuilt\ndrop a\ndrop b\ndrop c\nend\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn a_temp_used_only_as_a_receiver_fires_its_destructor() {
        // Phase 4.4 (spec §2): a destructor-bearing value used only as a method receiver, or
        // discarded as a bare statement, still fires at last use — a temp is an owner. `R.new("a")`
        // is consumed by `.use_it()` (fires after the call); `R.new("b");` is discarded (fires at the
        // statement). The compiler emits a destructor-aware `Op::Drop` of the receiver / discarded
        // register where there was none before.
        let r = run(
            "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  fn use_it(): void { echo \"use ${name}\"; }\n  destruct { echo \"close ${name}\"; }\n}\necho \"start\";\nR.new(\"a\").use_it();\nR.new(\"b\");\necho \"end\";\n",
        );
        assert_eq!(r.stdout, "start\nuse a\nclose a\nclose b\nend\n");
        assert_eq!(r.exit_code, 0);
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
            "class M {\n  amount: int\n  currency: string\n  fn new(a: int, c: string): M { return M { amount: a, currency: c }; }\n}\na = M.new(500, \"USD\");\nb = M { amount: 300, ...a };\necho b.amount;\necho b.currency;\necho a.amount;\n",
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
    fn derive_comparable_orders_fields_lexicographically() {
        // `@derive(Comparable)` gives structural ordering via the Module's comparable set + the
        // VM's `structural_compare`; no method is called.
        let r = run(
            "@derive(Comparable)\nclass P {\n  x: int\n  y: int\n  fn new(x: int, y: int): P { return P { x: x, y: y }; }\n}\na = P.new(1, 2);\nb = P.new(1, 5);\nc = P.new(1, 2);\necho a < b;\necho a > b;\necho a <= c;\necho a >= c;\n",
        );
        assert_eq!(r.stdout, "true\nfalse\ntrue\ntrue\n");
    }

    #[test]
    fn comparison_on_non_comparable_object_errors() {
        let r = run(
            "class P {\n  x: int\n  fn new(x: int): P { return P { x: x }; }\n}\necho P.new(1) < P.new(2);\n",
        );
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::TypeMismatch
        );
    }

    #[test]
    fn index_list_by_position() {
        // List element access retains the element (refcount discipline checked under miri).
        let r = run("xs = [\"a\", \"b\", \"c\"];\necho xs[1];\necho [10, 20][0];\n");
        assert_eq!(r.stdout, "b\n10\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn index_out_of_bounds_is_e0016() {
        let r = run("xs = [1, 2];\necho xs[5];\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::IndexOutOfBounds
        );
    }

    #[test]
    fn index_dispatches_to_index_trait() {
        // `inv[i]` routes to the class's `Index::get`, pushing a call frame `[recv, index]`.
        let r = run(
            "class Inv {\n  items: list\n  fn new(items: list): Inv { return Inv { items: items }; }\n  impl Index {\n    fn get(i: int): int { return items[i]; }\n  }\n}\necho Inv.new([7, 8, 9])[2];\n",
        );
        assert_eq!(r.stdout, "9\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn indexing_a_non_indexable_is_type_error() {
        let r = run("echo 42[0];\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::TypeMismatch
        );
    }

    #[test]
    fn index_map_by_key() {
        // Map element access by string key retains the value (refcount discipline under miri).
        let r = run("m = {\"a\": \"x\", \"b\": \"y\"};\necho m[\"b\"];\n");
        assert_eq!(r.stdout, "y\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn index_map_missing_key_is_e0018() {
        let r = run("m = {\"a\": 1};\necho m[\"z\"];\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::KeyNotFound
        );
    }

    #[test]
    fn index_string_by_position() {
        let r = run("s = \"hello\";\necho s[0];\necho s[4];\n");
        assert_eq!(r.stdout, "h\no\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn index_string_out_of_bounds_is_e0016() {
        let r = run("s = \"hi\";\necho s[5];\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::IndexOutOfBounds
        );
    }

    #[test]
    fn len_dispatches_to_length_trait() {
        // `len(o)` routes to the class's `Length::len`, pushing a receiver-only call frame.
        let r = run(
            "class Stack {\n  items: list\n  fn new(items: list): Stack { return Stack { items: items }; }\n  impl Length {\n    fn len(): int { return len(items); }\n  }\n}\necho len(Stack.new([1, 2, 3]));\n",
        );
        assert_eq!(r.stdout, "3\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn echo_dispatches_to_display_trait() {
        // `echo o` and `"{o}"` route to the class's `Display::to_string` (the `Stringify` op).
        let r = run(
            "class P {\n  n: int\n  fn new(n: int): P { return P { n: n }; }\n  impl Display {\n    fn to_string(): string { return \"P#${n}\"; }\n  }\n}\np = P.new(7);\necho p;\necho \"it is ${p}\";\n",
        );
        assert_eq!(r.stdout, "P#7\nit is P#7\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn derived_to_json_serializes_structurally() {
        // `@derive(Serialize<Json>)` synthesizes `to_json`: fields in declared order, strings
        // escaped, nested objects recursed — computed inline (no call frame).
        let r = run(
            "@derive(Serialize<Json>)\nclass U {\n  name: string\n  id: int\n  fn new(name: string, id: int): U { return U { name: name, id: id }; }\n}\necho U.new(\"Ada\", 7).to_json();\n",
        );
        assert_eq!(r.stdout, "{\"name\":\"Ada\",\"id\":7}\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn for_dispatches_to_iterable_trait() {
        // `for x in o` routes to the class's `Iterable::iter`, iterating its returned list.
        let r = run(
            "class Bag {\n  items: list\n  fn new(items: list): Bag { return Bag { items: items }; }\n  impl Iterable {\n    fn iter(): list { return items; }\n  }\n}\nmut total = 0;\nfor x in Bag.new([1, 2, 3]) { total = total + x; }\necho total;\n",
        );
        assert_eq!(r.stdout, "6\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn iterable_returning_non_list_is_e0007() {
        let r = run(
            "class B {\n  x: int\n  fn new(): B { return B { x: 1 }; }\n  impl Iterable { fn iter(): int { return 5; } }\n}\nfor v in B.new() { echo v; }\n",
        );
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::TypeMismatch
        );
    }

    #[test]
    fn object_without_display_uses_structural_render() {
        // No `Display` impl ⇒ the `Stringify` op is identity and the structural form prints.
        let r = run(
            "class P {\n  n: int\n  fn new(n: int): P { return P { n: n }; }\n}\necho P.new(7);\n",
        );
        assert_eq!(r.stdout, "P {n: 7}\n");
        assert_eq!(r.exit_code, 0);
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
            "enum E { Empty; Code(n: int); }\nx = E.Code(42);\necho match x { E.Empty => \"empty\", E.Code(n) => \"code ${n}\" };\n",
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
        let r = run("name = \"Niro\";\necho \"Hello ${name}\";\necho \"sum is ${1 + 2 * 3}\";\n");
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
    fn attribute_manifest_records_decorations() {
        // `#[...]` data attributes (with literal args) are collected into the queryable
        // build manifest, in source order, keyed by the decorated type.
        let source = Source::new(
            SourceId::FIRST,
            "t.lang",
            "#[Entity]\n#[Route(login, post)]\nclass Account {\n  id: int\n  fn new(id: int): Account { return Account { id: id }; }\n}\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        let attrs: Vec<_> = module.attributes_for("Account").collect();
        assert_eq!(attrs.len(), 2);
        assert_eq!(attrs[0].name, "Entity");
        assert!(attrs[0].args.is_empty());
        assert_eq!(attrs[1].name, "Route");
        let arg_values: Vec<_> = attrs[1].args.iter().map(|a| a.value.clone()).collect();
        assert_eq!(
            arg_values,
            vec![
                lang_ast::AttrValue::TypeRef("login".to_string()),
                lang_ast::AttrValue::TypeRef("post".to_string()),
            ]
        );
        // A type with no attributes has no manifest entries.
        assert_eq!(module.attributes_for("Missing").count(), 0);
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
    fn local_self_update_lowers_to_in_place_record_reuse() {
        // Phase 5.1a: a self-update of a destructor-free type whose accumulator is a directly-held
        // **function-local** must lower to the in-place `MakeRecordInPlace` (the reuse pass marks it,
        // the compiler emits it) rather than a copying `MakeRecord` — the proof the reuse token reaches
        // the VM. (A top-level global accumulator is the `TakeGlobal` case — see
        // `global_self_update_lowers_to_take_global_plus_in_place_reuse`.)
        let source = Source::new(
            SourceId::FIRST,
            "t.lang",
            "class P { x: int }\nfn run(): int {\n  mut acc = P { x: 0 };\n  acc = P { ...acc, x: acc.x + 1 };\n  return acc.x;\n}\necho run();\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        let disasm = module.disassemble();
        assert!(
            disasm.contains("MakeRecIP"),
            "expected an in-place record-reuse op, got:\n{disasm}"
        );
    }

    #[test]
    fn global_self_update_lowers_to_take_global_plus_in_place_reuse() {
        // Phase 5.1b: a top-level (global) record accumulator's self-update must move the global out
        // with `TakeGlobal` and reuse it in place with `MakeRecordInPlace` — not the copying
        // `MakeRecord` the local-only 5.1a path fell back to for a global. Both ops together are the
        // proof the global path is wired.
        let source = Source::new(
            SourceId::FIRST,
            "t.lang",
            "class P { x: int }\nmut acc = P { x: 0 };\nacc = P { ...acc, x: 5 };\necho acc.x;\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        let disasm = module.disassemble();
        assert!(
            disasm.contains("TakeGlobal") && disasm.contains("MakeRecIP"),
            "expected TakeGlobal + in-place record reuse for a global accumulator, got:\n{disasm}"
        );
    }

    #[test]
    fn local_map_self_update_lowers_to_reuse_method_call() {
        // Phase 5.1c: a function-local map accumulator updated with `m[k] = v` (desugaring to
        // `m = m.set(k, v)`) must carry the in-place-reuse token to the VM — `CallMethod ... [reuse]` —
        // so the dispatch mutates the uniquely-owned backing map in place rather than copying it. A
        // top-level (global) map accumulator is the `TakeGlobal` case (a later slice; the IR
        // interpreter already reuses it, and reuse is invisible, so the backends still agree).
        let source = Source::new(
            SourceId::FIRST,
            "t.lang",
            "fn build(): Map<string, int> {\n  mut m = {};\n  for i in 0..3 { m[\"k${i}\"] = i; }\n  return m;\n}\necho build().count();\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        let disasm = module.disassemble();
        assert!(
            disasm.contains("[reuse]"),
            "expected a reuse-marked method call for a local map self-update, got:\n{disasm}"
        );
    }

    #[test]
    fn self_append_lowers_to_in_place_concat() {
        // Phase 5.1b: a list self-append `acc ~= rhs` must lower to `ConcatInPlace` — for a global
        // accumulator preceded by `TakeGlobal` (to expose unique ownership), and for a function-local
        // accumulator directly on its register. The proof the concat reuse token reaches the VM rather
        // than the copying `Op::Binary` (`~`).
        let source = Source::new(
            SourceId::FIRST,
            "t.lang",
            "mut g = [\"a\"];\ng ~= [\"b\"];\nfn build(): List<int> {\n  mut acc = [];\n  for i in 0..3 { acc ~= [i]; }\n  return acc;\n}\necho g;\necho build();\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        let disasm = module.disassemble();
        assert_eq!(
            disasm.matches("ConcatIP").count(),
            2,
            "expected two in-place concats (global + local), got:\n{disasm}"
        );
        assert!(
            disasm.contains("TakeGlobal"),
            "expected the global self-append to be preceded by TakeGlobal, got:\n{disasm}"
        );
    }

    #[test]
    fn disassembly_of_a_match_decision_tree_is_stable() {
        let source = Source::new(
            SourceId::FIRST,
            "t.lang",
            "enum E { Empty; Code(n: int); }\nfn describe(e): string {\n  return match e {\n    E.Empty => \"empty\",\n    E.Code(n) => \"code ${n}\",\n  };\n}\necho describe(E.Code(7));\n",
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

    #[test]
    fn disassembly_of_local_bindings_consumes_temporaries() {
        // Each local declaration's value is a single-use temporary, so the local *adopts* the
        // temporary's register (a consuming move, Phase 3.3b) instead of a retaining `Op::Move` into
        // a fresh slot: the body holds no `Move` between the producing `Add` and the binding, and
        // `registers` stays small. A borrowed source (`y = x`, an aliased live local) still copies.
        let source = Source::new(
            SourceId::FIRST,
            "t.lang",
            "fn build(): int {\n  a = 1 + 2;\n  b = a + 3;\n  return b;\n}\necho build();\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        insta::assert_snapshot!(module.disassemble());
    }

    #[test]
    fn closure_default_reads_a_captured_cell() {
        // A closure default that references a captured variable the body never otherwise names: the
        // default thunk shares the closure's upvalue layout and reads the captured cell. Exercises
        // the run_thunk upvalue-retain path (miri verifies no leak / double-free).
        let r = run(
            "fn make(tag: string): dyn {\n  return fn(s: string, label: string = tag) => label ~ \":\" ~ s;\n}\nt = make(\"X\");\necho t(\"a\");\necho t(\"a\", \"Y\");\n",
        );
        assert_eq!(r.stdout, "X:a\nY:a\n");
        assert_eq!(r.exit_code, 0);
    }
}
