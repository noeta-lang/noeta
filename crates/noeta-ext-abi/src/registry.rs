//! The native-extension registry — the ABI type & trait vocabulary (P-NATIVE).
//!
//! An [`Extension`] declares its [`ExtModule`]s — each a set of [`ExtFn`] signatures plus one
//! backend-agnostic `dispatch` function — and its [`ExtType`]s (first-class value types). Both
//! backends route every module call and method through the same shared dispatch, so the
//! differential oracle (`TreeWalkBackend` ≡ `VmBackend`) holds by construction.
//!
//! ## The value-marshalling seam
//!
//! A dispatch function never sees a backend `Value`. Each backend projects its values onto
//! [`NativeValue`] (the argument view) and lifts the [`NativeOut`] result back — two functions
//! written once per backend. This crate holds only the neutral vocabulary; the concrete `std`
//! registration (`StdExtension`, the module/type tables, every `*_dispatch` fn, and the lookup
//! router `find_module`/`dispatch`/…) lives in `noeta-stdlib`, which reads these types.

use crate::{Host, StdError};
use serde::{Deserialize, Serialize};

/// A primitive scalar, backend-agnostic and `Copy`. The hot path (a scalar argument) marshals
/// with no allocation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scalar {
    Int(i64),
    Float(f64),
    F32(f32),
    Bool(bool),
}

/// A backend-agnostic view of a call **argument**. Each backend cheaply projects its own `Value`
/// onto this. Richer shapes (objects, packed buffers, bytes) are added as the modules that need
/// them migrate; the scalar/host modules use only [`NativeValue::Scalar`] and [`NativeValue::Str`].
#[derive(Debug, Clone, PartialEq)]
pub enum NativeValue {
    Scalar(Scalar),
    Str(String),
    /// A `bytes` buffer (e.g. `fs.write_bytes`). Marshalled by value — IO is never a hot path.
    Bytes(Vec<u8>),
    /// An object's primitive fields in slot order (e.g. a `Vec3`'s three `f32`s). `type_name` is the
    /// shape's name, kept for error messages. The shared dispatch reads the scalars; the backend
    /// supplies the *result* shape (via [`RetTy::SameAsArg`]) when materializing.
    Object {
        type_name: &'static str,
        fields: Vec<Scalar>,
    },
    /// The unit value (`json.stringify(unit)` → `null`). Part of the recursive "deep" arg view the
    /// reflective `json` module uses.
    Unit,
    /// A list/tuple/set, each element deeply marshalled — the recursive arg view `json.stringify`
    /// needs. (The shallow [`NativeValue::Object`] path `vec`/`quat` use is left untouched, so their
    /// hot path keeps its flat scalar projection.)
    List(Vec<NativeValue>),
    /// A keyed aggregate — a map (key order) or an object/record (declared field order), each value
    /// deeply marshalled. Both serialize to a JSON object, so one variant covers them.
    Map(Vec<(String, NativeValue)>),
    /// Any value a dispatch function never inspects — carries the type name for error messages.
    Opaque(&'static str),
    /// A registered extern-type value (extern-types X1), cloned into the seam via
    /// [`crate::ExternValue::clone_box`]. Extern arguments are never a hot path (their producers
    /// are host/IO-shaped), so by-value marshalling matches the rest of this view.
    Extern(crate::ExternBox),
}

/// A backend-agnostic **result** the backend materializes into its own `Value`.
#[derive(Debug, Clone, PartialEq)]
pub enum NativeOut {
    Scalar(Scalar),
    Str(String),
    Bytes(Vec<u8>),
    Unit,
    /// An object result as its field scalars in slot order (e.g. `vec.add` → a `Vec3`). The backend
    /// supplies the shape from the function's [`RetTy::SameAsArg`], so the dispatch never names a type.
    Object(Vec<Scalar>),
    /// A homogeneous list (e.g. `env.keys()` → list of strings). The backend builds its native
    /// list; nested `NativeOut` keeps it general for later recursive modules.
    List(Vec<NativeOut>),
    /// A homogeneous list of primitives as one **typed vector** (package-manager N3.4) — the
    /// result shape of a bulk *reduction* over a packed buffer (`vec.dot_all`/`length_all`: one
    /// `f32` per element). The backend converts the vector straight into its list in one pass;
    /// the boxed [`NativeOut::List`] form builds an enum per element first, which measured +80%
    /// on a 2k-element reduction. The bulk twin of [`NativeOut::Bytes`].
    Scalars(ScalarVec),
    /// A value-struct instance built by a call-site type recipe (`json.parse::<T>`): the type name
    /// and its `(field, value)` pairs **in the type's declared order**. Unlike [`NativeOut::Object`]
    /// — whose shape is supplied from an argument via [`RetTy::SameAsArg`] — a `Struct` names its own
    /// type, so the backend builds the instance by name (the tree-walker through its real registered
    /// definition, so methods/defaults match a normal literal; the VM through a fresh same-name shape,
    /// as reflection already does). Field values are themselves `NativeOut`, so nesting recurses.
    Struct {
        name: String,
        fields: Vec<(String, NativeOut)>,
        /// Propagated from [`TypeRecipe::Struct::has_validator`] (validation arc): when set, the
        /// backend re-enters the VM to run this type's `Validate::validate` on the built value,
        /// bottom-up. `false` short-circuits any re-entry (zero cost for a non-validated type).
        has_validator: bool,
    },
    /// A string-keyed map (a JSON object decoded under a `Map` recipe), entries in key order.
    Map(Vec<(String, NativeOut)>),
    /// `Option::None` — an absent optional field, or a JSON `null` decoded under an `Option` recipe.
    None,
    /// `Option::Some(x)` — a present optional value.
    Some(Box<NativeOut>),
    /// `Result::Ok(x)` — the success arm of a **call-site-typed** function whose declared return is
    /// `Result<T, E>` (`json.try_parse::<T>`). A recoverable typed dispatch builds the whole `Result`
    /// itself — success as `Ok`, failure as [`NativeOut::Err`] carrying the error value — so the
    /// backend materializes one tree with no per-function wrapping logic (the twin of the
    /// [`NativeOut::Some`]/[`NativeOut::None`] pair the `Option` wrap already uses).
    Ok(Box<NativeOut>),
    /// `Result::Err(e)` — the failure arm of a `Result<T, E>`-shaped call-site-typed function. The
    /// boxed value is the error (typically a [`NativeOut::Extern`] carrying a path-rich error type).
    Err(Box<NativeOut>),
    /// A registered extern-type value (extern-types X1) — `Uuid`, a `FileHandle`, … Each
    /// backend wraps it in its single extern hosting variant.
    Extern(crate::ExternBox),
    /// Async WORK instead of a value (extern-types X5): the backend tickets the descriptor on
    /// its executor (`spawn_ext`) and hands back a future — intercepted at the dispatch return,
    /// never reaching `materialize`. This is how an extension implements an async function
    /// without ever seeing the executor.
    Spawn(SpawnBox),
}

/// The typed bulk-primitive vector inside [`NativeOut::Scalars`]: one variant per [`Scalar`]
/// kind, so a reduction kernel's output vector crosses the seam without per-element boxing.
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarVec {
    Int(Vec<i64>),
    Float(Vec<f64>),
    F32(Vec<f32>),
    Bool(Vec<bool>),
}

/// A one-shot [`crate::ExternIo`] carrier inside [`NativeOut`] (which derives `Clone` +
/// `PartialEq` for its value variants — meaningless for work): cloning panics (a descriptor is
/// ticketed exactly once, on the dispatch return path), equality is always `false`.
#[derive(Debug)]
pub struct SpawnBox(pub Box<dyn crate::ExternIo>);

impl Clone for SpawnBox {
    fn clone(&self) -> SpawnBox {
        // Reaching this is a dispatch-author bug, not a user error: `NativeOut` derives `Clone`
        // for its VALUE variants, but a `Spawn` is one-shot WORK the backend tickets on the
        // executor at the dispatch return. Name the fix rather than aborting opaquely (F4).
        unreachable!(
            "a NativeOut::Spawn is one-shot async work, not a value — it is ticketed on the \
             executor at the dispatch return and must never be cloned (return it directly from \
             the dispatch; don't store or duplicate a Spawn result)"
        )
    }
}

impl PartialEq for SpawnBox {
    fn eq(&self, _other: &SpawnBox) -> bool {
        false
    }
}

/// noeta-stdlib's small signature vocabulary. noeta-stdlib cannot depend on `noeta_types::Type` (that
/// is exactly why the checker's tables live in `noeta-check`), so signatures are declared in this
/// neutral vocabulary and `noeta-check` maps each `SigType` onto a `Type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigType {
    Int,
    Float,
    F32,
    Bool,
    String,
    Bytes,
    Unit,
    /// Accepts any value (numeric-polymorphic positions, `json.stringify`, …).
    Dyn,
    List(&'static SigType),
    Option(&'static SigType),
    Map(&'static SigType, &'static SigType),
    /// A fallible result (http arc H6) — `http.client.get(url): Result<Response, HttpError>`. The
    /// checker maps it onto the language's first-class `Type::Result`, so `?` propagation and
    /// `From`-based error conversion work on it exactly as they do for a user-declared `Result`.
    ///
    /// This is the **non-turbofish** door. A call-site-typed one (`json.try_parse::<T>`) names its
    /// error through [`TypeArgWrap::Result`] instead, because its ok-type is only known at the call.
    /// The dispatch returns [`NativeOut::Ok`] / [`NativeOut::Err`] — never a `StdError` abort.
    Result(&'static SigType, &'static SigType),
    /// An async future (Track A.4c) — `fs.read_async(path): Future<string>`. The checker maps it onto
    /// `Type::Named("Future", [inner])` and `.await` unwraps it.
    Future(&'static SigType),
    /// A named type — an extension type or a user-declared type.
    Named(&'static str),
    /// A union of accepted types (crypto arc C1) — `crypto.sha256(data: string|bytes)`. The
    /// checker maps it onto the language's declared-union `Type::union`, so a mismatched
    /// argument is a static error; the dispatch still validates the concrete kind it received.
    Union(&'static [SigType]),
    /// A **trailing-optional** parameter (http arc H4) — `http.get(url, headers?)`. The wrapped
    /// type is what the argument must be *when present*; a call may omit it (and every parameter
    /// after it). The checker derives the required-argument count from the first `Optional`; the
    /// dispatch reads the slot with `args.get(i)` and supplies its own default when absent, so no
    /// backend change and no default-value machinery is needed. Convention: once a parameter is
    /// `Optional`, every following parameter is too.
    Optional(&'static SigType),
    /// A function/closure parameter (higher-order-abi H1) — `task.map_bounded(items, n,
    /// f: Fn([A]) -> Future<B>)`. The checker maps it onto the language's structural `Type::Fn`;
    /// the dispatch receives the closure as an opaque ctx slot and invokes it via
    /// [`crate::NativeCtx::call`], so `NativeValue` never grows a closure variant.
    Fn(&'static [SigType], &'static SigType),
    /// A signature-level type variable (higher-order-abi H1) — `task.all(fs: List<Future<Var(0)>>)
    /// -> List<Var(0)>`. The checker binds each variable at its first structural occurrence in the
    /// call's argument types and substitutes the bindings into the remaining parameters and the
    /// return, replacing the hand-written per-function checker arms the `Builtin` family needed.
    /// For an extern-type **method**, the receiver's type arguments seed the variables first
    /// (`Cell<T>.get() -> Var(0)` recovers `T`), then the call's arguments bind the rest.
    /// An unbound variable is a gradual hole (`Unknown`), never a wrong concrete type.
    Var(u8),
    /// A **trait-bounded** type variable (p2p P2) — like [`SigType::Var`] for binding and
    /// substitution, but the type bound to it must satisfy the named built-in trait or the call is
    /// a static error (E0025). `synced_signal(initial: BoundedVar(0, "Mergeable"), …)` is the first
    /// use: only a CRDT may be synced, enforced at compile time. The checker maps the trait name
    /// through `BuiltinTrait::from_name` and reuses its ordinary bound-satisfaction check.
    BoundedVar(u8, &'static str),
    /// A **generic nominal instantiation** (higher-order-abi H4) — a generic extern type in a
    /// signature position: `cell.new(v: Var(0)) -> Generic("Cell", &[Var(0)])` types as
    /// `Cell<T>` with `T` bound from the argument. The plain [`SigType::Named`] stays the
    /// monomorphic form. (Type arguments are a static-checker artifact — at runtime an extern
    /// value reflects as its bare nominal name, exactly as the reactive handles it generalizes
    /// reflected as `dyn`.)
    Generic(&'static str, &'static [SigType]),
}

impl SigType {
    /// The count of leading **required** parameters in `params` — everything up to the first
    /// [`SigType::Optional`] (http arc H4). All-required signatures return `params.len()`.
    pub fn required_count(params: &[SigType]) -> usize {
        params
            .iter()
            .take_while(|p| !matches!(p, SigType::Optional(_)))
            .count()
    }

    /// Render this signature type in **surface Noeta syntax**, matching the checker's own `Type`
    /// display (`void`, `Option<T>`, `A | B`, `fn(..) -> ..`) — so a signature read from the
    /// registry and a type printed in a diagnostic are the same text. This is the one canonical
    /// registry-signature renderer; every tooling surface that shows a registry signature (LSP
    /// completion detail, the MCP `stdlib_api` tool, future doc generation) formats through it
    /// rather than keeping its own lossy copy.
    ///
    /// The type-variable spelling is positional (`Var(0)` → `T`, `Var(1)` → `U`, …), the informal
    /// convention the docs use for generic positions. A trailing-[`SigType::Optional`] *parameter*
    /// renders as `T?` — an arity marker, distinct from the value type [`SigType::Option`].
    pub fn render(&self) -> String {
        match self {
            SigType::Int => "int".to_string(),
            SigType::Float => "float".to_string(),
            SigType::F32 => "f32".to_string(),
            SigType::Bool => "bool".to_string(),
            SigType::String => "string".to_string(),
            SigType::Bytes => "bytes".to_string(),
            SigType::Unit => "void".to_string(),
            SigType::Dyn => "dyn".to_string(),
            SigType::List(t) => format!("List<{}>", t.render()),
            SigType::Option(t) => format!("Option<{}>", t.render()),
            SigType::Map(k, v) => format!("Map<{}, {}>", k.render(), v.render()),
            SigType::Result(ok, err) => format!("Result<{}, {}>", ok.render(), err.render()),
            SigType::Future(t) => format!("Future<{}>", t.render()),
            SigType::Named(n) => (*n).to_string(),
            SigType::Union(ts) => ts
                .iter()
                .map(SigType::render)
                .collect::<Vec<_>>()
                .join(" | "),
            SigType::Optional(t) => format!("{}?", t.render()),
            SigType::Fn(params, ret) => format!(
                "fn({}) -> {}",
                params
                    .iter()
                    .map(SigType::render)
                    .collect::<Vec<_>>()
                    .join(", "),
                ret.render()
            ),
            SigType::Var(n) => type_var_name(*n),
            SigType::BoundedVar(n, bound) => format!("{}: {}", type_var_name(*n), bound),
            SigType::Generic(name, args) => format!(
                "{}<{}>",
                name,
                args.iter()
                    .map(SigType::render)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// Signature-level type-variable names — `Var(0)` → `T`, `Var(1)` → `U`, … then `T2`, `T3`, … past
/// the single-letter run.
fn type_var_name(n: u8) -> String {
    const LETTERS: &[u8] = b"TUVWXYZ";
    let i = n as usize;
    if i < LETTERS.len() {
        (LETTERS[i] as char).to_string()
    } else {
        format!("T{}", i - LETTERS.len() + 2)
    }
}

/// How a function's **return type** is determined. Most are [`RetTy::Concrete`]; the rest capture
/// the kind-polymorphic patterns the existing stdlib already has, plus the turbofish slot used by
/// the later call-site-typed construction (`json.parse::<T>`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RetTy {
    Concrete(SigType),
    /// The result has the same type as argument `n` (`vec.add(v, w): typeof v`).
    SameAsArg(usize),
    /// `int` if every argument is concretely `int`, else `float` (`math.abs`/`min`/`max`).
    NumericPreserving,
    /// The result type is named at the call site by a turbofish (`json.parse::<T>(): T`). The
    /// concrete `T` arrives as a [`TypeRecipe`] the checker records at the call site and the backend
    /// threads into the [`ExtModule::typed_dispatch`] (call-site-typed construction). The
    /// [`TypeArgWrap`] says how `T` is wrapped in the declared result — `T` itself, `Option<T>`, or
    /// `Result<T, E>` — which is exactly what the checker needs to type the call and (by the
    /// author-contract the dispatch mirrors) what shape of [`NativeOut`] tree the dispatch returns.
    TypeArg(TypeArgWrap),
}

/// How a [`RetTy::TypeArg`] function's turbofish `T` is wrapped in the declared result type — the
/// three shapes a call-site-typed native function may return. The checker maps the wrap onto the
/// call's static type; the dispatch produces the matching [`NativeOut`] tree (a plain value tree,
/// a [`NativeOut::Some`]/[`NativeOut::None`], or a [`NativeOut::Ok`]/[`NativeOut::Err`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeArgWrap {
    /// The result is `T` itself (`json.parse::<T>(): T`, the aborting convenience door — a decode
    /// failure is a runtime abort, so the dispatch returns the plain value tree or a `StdError`).
    Plain,
    /// The result is `Option<T>` — a present decode is [`NativeOut::Some`], an absent one
    /// [`NativeOut::None`].
    Option,
    /// The result is `Result<T, E>` where `E` is the named error type (`json.try_parse::<T>():
    /// Result<T, JsonError>`, the recoverable door). Success is [`NativeOut::Ok`], a decode failure
    /// [`NativeOut::Err`] carrying the error value — never a `StdError` abort.
    Result(SigType),
}

impl RetTy {
    /// Render the return type in surface syntax, resolving the polymorphic forms against the
    /// signature's `params` where they reference them. Companion to [`SigType::render`].
    pub fn render(&self, params: &[SigType]) -> String {
        match self {
            RetTy::Concrete(s) => s.render(),
            // Same type as a positional argument (`vec.add(v, w): typeof v`).
            RetTy::SameAsArg(n) => params
                .get(*n)
                .map(SigType::render)
                .unwrap_or_else(|| "dyn".to_string()),
            // `int` when every argument is concretely `int`, else `float`.
            RetTy::NumericPreserving => "int | float".to_string(),
            // Named at the call site by a turbofish, in its declared wrapper.
            RetTy::TypeArg(TypeArgWrap::Plain) => {
                "T /* call-site type: name it with ::<T> */".to_string()
            }
            RetTy::TypeArg(TypeArgWrap::Option) => {
                "Option<T> /* call-site type: ::<T> */".to_string()
            }
            RetTy::TypeArg(TypeArgWrap::Result(e)) => {
                format!("Result<T, {}> /* call-site type: ::<T> */", e.render())
            }
        }
    }
}

/// A recursive build recipe for a call-site type argument (`json.parse::<T>`). The checker resolves
/// the turbofish `T` into a `TypeRecipe`; the dispatch walks an input (a JSON tree) against it to
/// produce a [`NativeOut`] tree the backend materializes into a value of `T`.
///
/// noeta-stdlib cannot see `noeta_types::Type` (the very reason the checker's type tables live in
/// `noeta-check`), so the recipe is this neutral, self-contained vocabulary — a leaf type the
/// bytecode op can carry and the dispatch can walk without any type-system dependency. A struct
/// records its fields **in declared order**, with field names, so the decoder both matches input
/// keys and emits fields in the order the backend's registered type expects.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum TypeRecipe {
    Int,
    Float,
    F32,
    Bool,
    Str,
    /// The unit value (a JSON `null`).
    Unit,
    Option(Box<TypeRecipe>),
    List(Box<TypeRecipe>),
    /// A string-keyed map; the boxed recipe is the value type (JSON object keys are always strings).
    Map(Box<TypeRecipe>),
    /// A struct/record type: its name and `(field, recipe)` pairs in the type's declared order.
    Struct {
        name: String,
        fields: Vec<(String, TypeRecipe)>,
        /// Whether this type implements the `Validate` built-in trait (validation arc). When set,
        /// a recipe door re-enters the backend to run `validate()` on the freshly-built value
        /// (bottom-up: after all fields are materialized and validated). Resolved by the checker's
        /// `type_to_recipe`; `false` means the materialize walk never re-enters for this node
        /// (zero cost for a non-validated type).
        has_validator: bool,
    },
}

/// One resolved **type-argument bundle** for a forwarded generic instantiation (poly-values F2b):
/// what a generic function whose type parameter flows into a call-site-typed position needs at
/// runtime about one concrete instantiation. Generics are erased, so a single compiled body serves
/// every instantiation — the checker interns these bundles into a program-wide table, each
/// instantiating call passes its entry's INDEX as a hidden argument, and the forwarded sites
/// (`json.try_parse::<T>`, `attributes_of::<T>`) resolve their data through it at runtime. A pure
/// function of the program, identical for both backends by construction.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TypeArgInfo {
    /// The instantiation's display name (`"Order"`) — what a name-keyed consumer
    /// (`attributes_of`'s manifest) resolves with.
    pub name: String,
    /// The instantiation's build recipe, when the type has one — what a recipe-consuming door
    /// (`json.try_parse::<T>`) decodes with. `None` for an un-recipeable type: statically
    /// reachable only when no forwarded site of the callee needs a recipe (the checker rejects a
    /// recipe-needing instantiation without one at the call site).
    pub recipe: Option<TypeRecipe>,
}

/// How an instantiating call supplies one **hidden type-argument slot** of a forwarding generic
/// function (poly-values F2b). Checker → lowering vocabulary only (lowering turns it into an
/// ordinary prepended call argument), so it is not serialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HiddenArg {
    /// A concrete instantiation: pass this index into the program's [`TypeArgInfo`] table.
    Table(u32),
    /// A pass-through: forward the ENCLOSING function's own hidden slot `i` (the caller is itself
    /// generic and forwards its `T` onward), i.e. the local `$ty<i>`.
    Forward(u32),
}

/// One native function's static signature (for the checker and tooling). Dispatch is per-module
/// (matching on the function name), so an `ExtFn` carries no dispatch pointer of its own.
#[derive(Debug, Clone, Copy)]
pub struct ExtFn {
    pub name: &'static str,
    pub params: &'static [SigType],
    pub ret: RetTy,
}

impl ExtFn {
    /// Field defaults for additive evolution (N3.6), mirroring [`ExtModule::DEFAULTS`]: an
    /// out-of-tree table written as `ExtFn { name, params, ret, ..ExtFn::DEFAULTS }` keeps
    /// compiling when a future optional field (a doc string, a deprecation note, …) lands here.
    pub const DEFAULTS: ExtFn = ExtFn {
        name: "",
        params: &[],
        ret: RetTy::Concrete(SigType::Unit),
    };

    /// The whole signature in surface syntax — `fn split(string, string): List<string>`. Native
    /// parameters carry no names in the registry, so parameters render as their types positionally.
    pub fn render(&self) -> String {
        format!(
            "fn {}({}): {}",
            self.name,
            self.params
                .iter()
                .map(SigType::render)
                .collect::<Vec<_>>()
                .join(", "),
            self.ret.render(self.params)
        )
    }
}

/// A module's dispatch: given the function name, the host seam, and the projected arguments, run
/// the function and return a neutral result (or a misuse error). One per module, mirroring the
/// existing `call(func, args)` shape.
pub type ModuleDispatch =
    fn(func: &str, host: &mut dyn Host, args: &[NativeValue]) -> Result<NativeOut, StdError>;

/// A module's **call-site-typed** dispatch (`json.parse::<T>`): like [`ModuleDispatch`], but the
/// checker-resolved [`TypeRecipe`] for the turbofish `T` is threaded in, so the function builds a
/// value of the caller-named type. Reached only for a function in [`ExtModule::typed_functions`]
/// (each declaring [`RetTy::TypeArg`]); the returned [`NativeOut`] tree already carries the declared
/// wrapper ([`NativeOut::Ok`]/[`NativeOut::Err`] for a `Result` shape, [`NativeOut::Some`]/`None`
/// for an `Option`), so the backend materializes it with no per-function wrapping. A `Plain` door
/// signals an unrecoverable failure with `Err(StdError)` (a runtime abort); a recoverable door
/// never uses the `Err` channel — it returns the `Err` arm inside the `NativeOut`.
pub type TypedDispatch = fn(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
    recipe: &TypeRecipe,
) -> Result<NativeOut, StdError>;

/// A native module: its surface name, its function signatures, and its shared dispatch.
#[derive(Debug, Clone, Copy)]
pub struct ExtModule {
    pub name: &'static str,
    pub functions: &'static [ExtFn],
    pub dispatch: ModuleDispatch,
    /// Whether the backend should marshal this module's call arguments **deeply** — the recursive
    /// `Unit`/`List`/`Map` [`NativeValue`] view — rather than the default shallow scalar projection.
    /// Only the reflective `json` module needs it (`json.stringify` introspects an arbitrary value);
    /// the scalar/`vec`/`quat` modules keep the cheap flat marshalling, so their hot path is
    /// untouched. The module declares its own need here so the backends stay data-driven.
    pub deep_marshal: bool,
    /// The module's **higher-order** functions (higher-order-abi H0): signatures whose calls route
    /// to [`ExtModule::ctx_dispatch`] with opaque slot arguments instead of marshalled values —
    /// for functions that take closures, drive the executor, or orchestrate futures. Same
    /// signature vocabulary as [`ExtModule::functions`]; a name appears in exactly one table.
    pub ctx_functions: &'static [ExtFn],
    /// The shared dispatch for [`ExtModule::ctx_functions`] (`None` when the table is empty).
    pub ctx_dispatch: Option<crate::ctx::CtxDispatch>,
    /// The **native-dependency ring** this module's implementation lives behind (package-manager
    /// P1.0): the name of the optional Cargo feature gating its heavy native deps in the AOT runtime
    /// archive (`std.http.client` → `Some("ring-http-client")`). `None` = always-on core (no
    /// separable native dep tree). This is the **single source of truth** for the module→ring map:
    /// `noeta build --native`'s footprint scan reads it off the registry to select the archive's
    /// features (DCE Axis B), retiring the hand-maintained `module_ring`/`fn_ring` tables the CLI
    /// carried. The string must equal the `noeta-aot-runtime` Cargo feature that turns the ring on.
    pub ring: Option<&'static str>,
    /// The module's **method bundles** (kernel-methods K0): named method sets a user's `@packed`
    /// type acquires by explicit `impl <module>.<Bundle> for T {}`. Default empty.
    pub bundles: &'static [ExtBundle],
    /// Per-function **documentation prose** (docs-browser arc, Arc 2): `(function_name, markdown)`
    /// pairs surfaced by the API-reference docs generator (`noeta doc`, the editor's docs browser,
    /// the MCP docs tools). Co-located with the module's registration; opt-in and sparse — a
    /// function absent from this table renders signature-only, like docs.rs. Keyed by name so one
    /// table covers both [`ExtModule::functions`] and [`ExtModule::ctx_functions`]; third-party
    /// extensions get the same field for free (their literals use `..ExtModule::DEFAULTS`).
    pub docs: &'static [(&'static str, &'static str)],
    /// The module's **call-site-typed** functions (`json.parse::<T>` / `try_parse::<T>`): signatures
    /// whose result type is named at the call site by a turbofish. Each declares [`RetTy::TypeArg`]
    /// (the wrapper shape) and routes to [`ExtModule::typed_dispatch`] with the checker-resolved
    /// [`TypeRecipe`]. A **separate** table from [`ExtModule::functions`] because the turbofish form
    /// (`f::<T>(x)`) is a distinct call surface from a plain call (`f(x)`) — the two may legitimately
    /// share a name (`json.parse` is both a dynamic `parse(text): dyn` and a typed `parse::<T>: T`),
    /// so this table's names live in their own space. Default empty; a name is unique within it.
    pub typed_functions: &'static [ExtFn],
    /// The shared dispatch for [`ExtModule::typed_functions`] (`None` when the table is empty).
    pub typed_dispatch: Option<TypedDispatch>,
}

impl ExtModule {
    /// Field defaults for the optional capabilities, so a module literal only names what it uses:
    /// `ExtModule { name, functions, dispatch, deep_marshal, ..ExtModule::DEFAULTS }`. A future
    /// capability field lands here once instead of in every registration.
    pub const DEFAULTS: ExtModule = ExtModule {
        name: "",
        functions: &[],
        dispatch: no_dispatch,
        deep_marshal: false,
        ctx_functions: &[],
        ctx_dispatch: None,
        ring: None,
        bundles: &[],
        docs: &[],
        typed_functions: &[],
        typed_dispatch: None,
    };
}

/// The [`ExtModule::DEFAULTS`] dispatch placeholder — reached only by a module that registers no
/// plain functions (e.g. a ctx-only module), where any name is unknown by definition.
fn no_dispatch(
    func: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    Err(StdError {
        kind: crate::ErrorKind::UnknownName,
        message: format!("no function `{func}`"),
    })
}

/// A type's method dispatch (extern-types X1): given the receiver, the method name, the host
/// seam, and the projected arguments, run the method and return a neutral result. ONE signature
/// covers the whole {pure, mutable} × {host-free, effectful} matrix — a pure method simply does
/// not mutate `recv` or touch `host` (`Uuid.version()`), an effectful one does both
/// (`FileHandle.read_line(host)`).
pub type TypeDispatch = fn(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError>;

/// A type's **higher-order** method dispatch (higher-order-abi H4): like [`TypeDispatch`], but
/// the receiver and arguments arrive as opaque ctx slots and the body may re-enter the backend —
/// call closures, reach per-run state, read/write the retained arena. What `Cell.update(f)` and
/// the reactive handle methods need. The receiver slot is not consumed; downcast its plain data
/// via [`crate::NativeCtx::with_extern`].
pub type CtxTypeDispatch = fn(
    method: &str,
    ctx: &mut dyn crate::NativeCtx,
    recv: crate::Slot,
    args: &[crate::Slot],
) -> Result<crate::CtxOut, crate::CtxError>;

/// An [`ExtType::arena_getter`] declaration: the method name plus the projection reading the
/// [`crate::Retained`] id off the extern box.
pub type ArenaGetter = (&'static str, fn(&dyn crate::ExternValue) -> crate::Retained);

/// A first-class value type contributed by an extension (extern-types X1): a reserved type name,
/// its instance-method signatures, their shared dispatch, and the key capability the checker
/// reads. The value behavior itself (equality, ordering, hash, display) lives on the
/// [`crate::ExternValue`] impl the type's constructors box up.
///
/// A **generic** extern type (`Cell<T>`, higher-order-abi H4) declares nothing extra here: its
/// constructor's return names the instantiation ([`SigType::Generic`]) and its method signatures
/// reference the receiver's type arguments as [`SigType::Var`] (`Var(0)` = first argument) — the
/// checker seeds the variables from the receiver's static type. At runtime the value is tagged
/// with the nominal identity only (its qualified name, no type arguments).
#[derive(Debug, Clone, Copy)]
pub struct ExtType {
    /// The **short display name** (`Uuid`) — what humans see in errors / `type_of` stringification.
    /// The type's *identity* (for lookup, equality, dispatch, `is`/`as`) is the **qualified** name
    /// [`ExtType::qualified`] = `"{namespace}.{name}"` (`std.id.Uuid`); two types with the same short
    /// name under distinct namespaces are distinct identities. A user declaration of this short name
    /// is no longer globally reserved — extern types are `use`-imported like user types.
    pub name: &'static str,
    /// The namespace this type lives under (`std.id`) — its qualified identity is `namespace.name`.
    /// Mirrors [`Extension::root`] for modules; the seam that lets a native `std.metrics.Counter`
    /// coexist with a user's own `myapp.Counter`.
    pub namespace: &'static str,
    /// Instance-method signatures — same vocabulary as module functions.
    pub methods: &'static [ExtFn],
    pub dispatch: TypeDispatch,
    /// Whether values may key a `Map` / member a `Set`. Declaring `true` promises: no mutating
    /// methods, [`crate::ExternValue::cmp_value`] is a total order over the kind, and
    /// [`crate::ExternValue::hash_value`] is stable and content-derived.
    pub key_capable: bool,
    /// The type's **higher-order** method signatures (H4) — calls route to
    /// [`ExtType::ctx_dispatch`] with slot arguments. Disjoint from `methods` by name.
    pub ctx_methods: &'static [ExtFn],
    pub ctx_dispatch: Option<CtxTypeDispatch>,
    /// Hot-path declaration (H5 perf): `Some((method, project))` marks `method` — one of the
    /// `ctx_methods` — as a **gated arena read**: its entire observable behavior is "return the
    /// receiver's retained arena entry", where `project` reads the [`crate::Retained`] id off
    /// the extern box. While the type's **read gate** is open (the default), the backend inlines
    /// the read at the call site — arena load + retain, no ctx dispatch — which is what keeps a
    /// `signal.get()`/`cell.get()` hot loop at intercept speed. The extension closes the gate
    /// ([`crate::NativeCtx::set_read_gate`]) for exactly the windows where the full dispatch
    /// does *more* than the plain read (dependency tracking while an effect body runs; a dirty
    /// memo), and calls fall back to the ordinary ctx dispatch — which must behave identically
    /// to the fast path whenever the gate is open. The declaration is semantic, not an
    /// optimization hint: every tier (interpreter now, JIT later) may compile it.
    pub arena_getter: Option<ArenaGetter>,
    /// The **built-in traits this type declares** (p2p P2) — the extern-type analogue of a user
    /// type's `@derive`/`impl`. The checker seeds these into its trait-impl table so a
    /// `T: Mergeable` bound (or any built-in-trait bound) is satisfied by this type. The CRDT types
    /// declare `["Mergeable"]`; a non-built-in name is ignored. Default empty.
    pub traits: &'static [&'static str],
    /// Whether plain-`methods` arguments are **deep-marshalled** (a `Map`/`List`/object argument
    /// projects to a full [`crate::NativeValue`] tree) rather than the cheap shallow projection
    /// (containers → `Opaque`). The extern-type analogue of [`ExtModule::deep_marshal`]; set it for a
    /// type whose methods take a container argument (the metrics instruments' `*_with(_, attrs)`).
    /// Default `false` — most extern methods take scalars/handles.
    pub deep_marshal: bool,
    /// Per-method **documentation prose** (docs-browser Arc 2): `(method_name, markdown)` pairs, the
    /// extern-type analogue of [`ExtModule::docs`]. Opt-in and sparse; keyed by name so it covers
    /// both [`ExtType::methods`] and [`ExtType::ctx_methods`].
    pub docs: &'static [(&'static str, &'static str)],
}

impl ExtType {
    /// Literal-shortening defaults (`..ExtType::DEFAULTS`), mirroring [`ExtModule::DEFAULTS`]:
    /// a plain-data extern type declares no higher-order surface.
    pub const DEFAULTS: ExtType = ExtType {
        name: "",
        namespace: "std",
        methods: &[],
        dispatch: |_, method, _, _| {
            Err(StdError {
                kind: crate::ErrorKind::UnknownName,
                message: format!("internal: no method dispatch registered (method `{method}`)"),
            })
        },
        key_capable: false,
        ctx_methods: &[],
        ctx_dispatch: None,
        arena_getter: None,
        traits: &[],
        deep_marshal: false,
        docs: &[],
    };

    /// The type's **qualified identity** (`std.id.Uuid`) — `namespace.name`. This is the string the
    /// checker keys `Type::Named` on and the runtime keys dispatch/`is`/`as` on; [`ExtType::name`]
    /// is only the human-facing short form.
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.namespace, self.name)
    }

    /// Whether `q` **is** this type's qualified identity — [`ExtType::qualified`] equality without
    /// building the `String`. Registry lookups run this per candidate type per probe, and the
    /// checker probes per imported-type annotation/member on the per-keystroke LSP path, so the
    /// comparison must not allocate (audit-3 Finding 12).
    pub fn is_qualified(&self, q: &str) -> bool {
        q.len() == self.namespace.len() + 1 + self.name.len()
            && q.as_bytes()[self.namespace.len()] == b'.'
            && q.starts_with(self.namespace)
            && q.ends_with(self.name)
    }
}

// --- Method bundles (kernel-methods K0) ----------------------------------------------------------
//
// A **method bundle** is the nominal-binding half of the raw-buffer kernel story: N3.4 gave a
// native function the *capability* to run over a packed list's contiguous bytes, but the surface
// was free module functions, structurally connected to the user's `@packed` type by nothing but
// memory layout — invisible to the checker and the LSP. A bundle is a named set of native methods
// a user type acquires by **explicit opt-in** (`impl vec.Kernels for Px {}`): the checker
// validates the bundle's structural constraint against the type at the impl site (the shape check
// moves from runtime dispatch to a compile-time diagnostic), and from the binding on, the type
// (and `List<T>` for the bulk forms) carries the methods everywhere — typing, dispatch,
// completion. See `plans/kernel-methods/README.md`.

/// The static twin of the runtime [`crate::PackedView`] check a raw-buffer kernel performs: what
/// a type binding to the bundle must look like, validated **at the impl site, at compile time**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackedConstraint {
    /// Required field kinds, in slot (declared) order — exact arity and kinds.
    pub fields: &'static [ConstraintField],
    /// Required storage layout (`Any` for layout-agnostic kernels that branch on
    /// `PackedView::column` themselves).
    pub layout: ConstraintLayout,
}

/// One required field kind in a [`PackedConstraint`] (primitives only — a bundle over nested
/// packed structs is a later, additive extension).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintField {
    Int,
    Float,
    F32,
    Bool,
}

/// The storage layout a [`PackedConstraint`] requires of the bound type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintLayout {
    Any,
    Row,
    Column,
}

/// Which receiver carries a [`BundleFn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleReceiver {
    /// A method on a value of the bound type itself (`v.dot(w)`).
    Element,
    /// A method on a `List<T>` of the bound type (`xs.dot_all(ys)`, `xs.sum()`).
    Bulk,
}

/// One bundle method: an ordinary [`ExtFn`] signature (the receiver is *not* in `params` — it
/// rides as ctx slot 0, the extern-type ctx-method convention, so `RetTy::SameAsArg(0)` means
/// "same type as the receiver") plus which receiver carries it.
#[derive(Debug, Clone, Copy)]
pub struct BundleFn {
    pub sig: ExtFn,
    pub receiver: BundleReceiver,
}

/// A named method bundle contributed by a module (kernel-methods K0). Referenced at the impl site
/// through the owning module's binding — `use std.{vec}` then `impl vec.Kernels for Px {}` — so
/// provenance is explicit in the source.
#[derive(Debug, Clone, Copy)]
pub struct ExtBundle {
    /// The bundle's surface name (`Kernels`). Unique within its module.
    pub name: &'static str,
    /// What a binding type must look like.
    pub constraint: PackedConstraint,
    /// The methods a bound type acquires. Method names are unique across the whole bundle
    /// (regardless of receiver kind — one name meaning different things on `T` vs `List<T>`
    /// would be a comprehension hazard; install-time validated).
    pub methods: &'static [BundleFn],
    /// The one shared higher-order dispatch (both backends): the bound receiver rides as slot 0.
    /// Same shape as [`ExtType::ctx_dispatch`] — a `Bulk` method's slot 0 is the list.
    pub ctx_dispatch: CtxTypeDispatch,
}

impl ExtBundle {
    /// The bundle's method named `method`, if any.
    pub fn method(&self, method: &str) -> Option<&'static BundleFn> {
        // `methods` is a `&'static` slice, so the reference is `'static` too.
        self.methods.iter().find(|m| m.sig.name == method)
    }
}

/// The literal type of an extension-declared attribute field — the subset attribute
/// construction accepts (tier-extensions port). Mirrors the checker's field typing for a prelude
/// `@attribute` struct; grow variants as std's declarations demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrFieldType {
    Int,
    Str,
    /// An open payload (`Data.rows` — heterogeneous, element type left to the runtime).
    Dyn,
}

/// A field's literal default. Present ⇒ the field is optional at construction and materialization
/// fills the default (`Skip.reason` = `""`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AttrFieldDefault {
    Str(&'static str),
    Int(i64),
}

/// One field of an extension-declared attribute.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtAttrField {
    pub name: &'static str,
    pub ty: AttrFieldType,
    /// `Some` makes the field optional; `None` is mandatory.
    pub default: Option<AttrFieldDefault>,
}

/// An extension-declared prelude **attribute** — the extension counterpart of an `@attribute`
/// struct (tier-extensions port). The checker registers each installed extension's attributes
/// exactly as it registers a program-declared one (construction gate, reflection, shadowable by a
/// user declaration); std ships the tier knob/metadata attributes (`Bench`, `Doc`, `Skip`, `Name`,
/// `Group`, `Data`) this way.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtAttribute {
    pub name: &'static str,
    pub fields: &'static [ExtAttrField],
}

/// Where a dev-tier directive may **attach** (the directive attachment-site model). A tier declares
/// its allowed sites when it is registered; the checker rejects the directive at any site not listed
/// (E0054). This is the tier counterpart of an `@attribute(Method, Function, …)` placement list, and
/// applies only to the **annotation / adjacency** forms that decorate a declaration (`@test fn`,
/// `@doc { … } struct`) — the statement-position **block** form (`@debug { … }`, `@json { … }`) is
/// not an attachment and is never site-checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierSite {
    /// A top-level function (`@test fn foo()`).
    Function,
    /// A method inside a `struct`/`class`/`enum` body (`@test fn method()`).
    Method,
    /// A type declaration — `struct`, `class`, or `enum` (`@doc { … } struct Point`).
    Type,
}

/// An extension-declared **dev-tier** — the extension counterpart of a program's `@tier`
/// declaration. std ships the built-in four (`test`/`bench`/`doc`/`debug`); the tier name-space
/// the checker validates against is the installed extensions' tiers ∪ the program's own `@tier`
/// declarations. The built-ins' runners stay native (`noeta test`/`bench`/`doc` and `--tier
/// debug`); only the declaration lives here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtTier {
    pub name: &'static str,
    /// Which declaration sites this tier's annotation/adjacency form may attach to (directive
    /// attachment-site model). **Empty ⇒ unrestricted** — the directive attaches anywhere the
    /// grammar admits it, which keeps a tier that predates the model (and every statement/expression
    /// block tier, which is never attachment-checked) working unchanged. `test`/`bench` list
    /// `Function`+`Method`; `doc` adds `Type`.
    pub sites: &'static [TierSite],
    /// The knob attribute a `@<tier>(args)` block stamps onto its fns — one of the extension's
    /// [`Extension::attributes`] — or `None` for a knob-less tier (whose directive rejects args).
    pub config: Option<&'static str>,
    /// The body language ID for a **text** or **expression** tier (text-tiers / expr-tiers arcs):
    /// its `@<name> { … }` bodies are captured verbatim by the lexer and tagged with this language
    /// for editor injection and LSP hover (`doc` → `"markdown"`, a native `@json` → `"json"`).
    /// `None` for a code tier. Decoupled from the tier name.
    pub text: Option<&'static str>,
    /// The value type an **expression** tier's blocks evaluate to (expr-tiers arc) — the extension
    /// counterpart of a program `@tier(…, expr: T)`. When set, `@<name> { … }` is an *expression*
    /// (verbatim text with `${…}` holes) that desugars to a call of [`Self::handler`]; `None` for a
    /// code or text tier. Mutually exclusive with `config`.
    pub expr: Option<&'static str>,
    /// The **native handler** an expression tier's blocks desugar to (expr-tiers arc): the
    /// qualified module-function name called with `(statics: List<string>, holes: List<() -> dyn>)`
    /// yielding [`Self::expr`]. `None` unless `expr` is set (a program-declared expr tier names its
    /// handler on the `@tier` fn instead).
    pub handler: Option<&'static str>,
}

/// An extension-declared **derive recipe** (derive layer 4) — the native counterpart of deriving
/// a fully-defaulted user trait: `@derive(<Name>)` on a type synthesizes, for each declared
/// method, a forward into the extension's registered module function —
/// `fn <name>(a1: dyn, …): dyn { return <handler>(self, a1, …) }` — resolved like an expression
/// tier's native handler (an `Expr::NativeFnRef`, no user import needed). The handler's own
/// registered signature is the typing authority at the call; the recipe does its real work
/// natively (typically via reflection over the value), so this is the proc-macro power tier
/// without codegen opacity: what the derive adds is a visible, checkable forward.
#[derive(Debug, Clone, Copy)]
pub struct ExtDerive {
    /// The name programs write in `@derive(...)`. Resolved after built-in traits and the
    /// program's user traits, so it can never shadow either.
    pub name: &'static str,
    /// The methods the derive synthesizes onto the deriving type.
    pub methods: &'static [ExtDeriveMethod],
    /// Optional compile-time shape validation: given the deriving type's name and its
    /// `(field name, field type spelling)` pairs, return `Some(message)` to reject the derive at
    /// the declaration (E0050). `None` (the field or the result) accepts.
    #[allow(clippy::type_complexity)]
    pub validate: Option<fn(&str, &[(String, String)]) -> Option<String>>,
}

/// One method an [`ExtDerive`] synthesizes.
#[derive(Debug, Clone, Copy)]
pub struct ExtDeriveMethod {
    /// The synthesized method's name on the deriving type.
    pub name: &'static str,
    /// Its parameter count EXCLUDING the receiver (each parameter is `dyn` at the surface).
    pub arity: usize,
    /// The qualified native handler, `"module.func"`, called with `(self, args…)`.
    pub handler: &'static str,
}

/// A native **tier-body formatter** for `noeta fmt`, keyed by body **language** (extension-driven
/// tier-body formatting). It is `(body, indent, sub) -> Option<reflowed>`:
/// - `body` is the foreign text with each `${…}` hole represented as a single NUL (`\0`) placeholder;
/// - `indent` is the whitespace to lay the body's top level at (its column in the formatted file), so
///   the formatter owns its own indentation — which lets it indent structure while leaving
///   whitespace-significant content (`<pre>`, `<textarea>`) byte-for-byte untouched;
/// - `sub` is a delegation callback `(language, body, indent) -> Option<reflowed>`: a formatter uses
///   it to format an **embedded sub-language** with that language's registered formatter — an HTML
///   formatter hands `<style>`/`<script>` bodies to `sub("css", …)`/`sub("javascript", …)`, getting
///   `None` when none is registered (→ leave that region verbatim). A plain `&dyn Fn` (not a bespoke
///   trait) so this ABI stays decoupled from the formatter crate;
/// - it returns the reflowed foreign text (holes still `\0`, in order), or `None` to decline.
///
/// fmt owns everything Noeta — it substitutes the (separately, inline-formatted) holes back for the
/// `\0`s and re-applies tier-body escaping — so a formatter is pure foreign-language reflow and never
/// needs to know Noeta's syntax. Keyed by *language*, not tier, so any tier (native `ExtTier` or a
/// program `@tier(…, text: "…")`) declaring the language gets it; registering one is the extension's
/// assertion that reflowing that language preserves the value (the relaxation `noeta fmt` cannot
/// prove). A language with no formatter stays byte-for-byte verbatim.
pub type SubFormat<'a> = dyn Fn(&str, &str, &str) -> Option<String> + 'a;
pub type BodyFormatter = (&'static str, fn(&str, &str, &SubFormat) -> Option<String>);

/// A bundle of native modules and types registered into the language. Core implements this once
/// as `StdExtension` (in `noeta-stdlib`); a third-party crate implements it to contribute its own
/// modules/types.
pub trait Extension: Sync {
    fn name(&self) -> &'static str;
    /// The namespace **root** this extension's modules live under — the first segment of every
    /// qualified module path it owns (`std.math`, `std.http.client`). Defaults to [`Extension::name`]
    /// (core's `"std"`); a package whose import namespace diverges from its package name overrides it.
    /// Module identity is the full path, so two extensions with distinct roots never collide
    /// (`std.http` vs `guzzle.http`).
    fn root(&self) -> &'static str {
        self.name()
    }
    fn modules(&self) -> &'static [ExtModule];
    /// The extension's first-class value types. Default empty — a modules-only extension does
    /// not change.
    fn types(&self) -> &'static [ExtType] {
        &[]
    }
    /// The extension's CLI subcommands (higher-order-abi H6). Default empty.
    fn commands(&self) -> &'static [crate::ExtCommand] {
        &[]
    }
    /// The extension's declared dev-tiers (tier-extensions port). Default empty.
    fn tiers(&self) -> &'static [ExtTier] {
        &[]
    }
    /// The extension's declared prelude attributes (tier knobs and metadata). Default empty.
    fn attributes(&self) -> &'static [ExtAttribute] {
        &[]
    }
    /// The extension's declared **derive recipes** (derive layer 4 — see [`ExtDerive`]). Default
    /// empty; a defaulted trait method keeps every existing extension source-compatible.
    fn derives(&self) -> &'static [ExtDerive] {
        &[]
    }
    /// The extension's **tier-body formatters** for `noeta fmt`, keyed by body language (extension-
    /// driven tier-body formatting — see [`BodyFormatter`]). Default empty: an extension that ships a
    /// tier does not have to make its body reflowable, and one may exist purely to format a language
    /// used by *another* extension's tier (e.g. a first-party HTML formatter for a program `@html`).
    fn body_formatters(&self) -> &'static [BodyFormatter] {
        &[]
    }
    /// The **per-run capabilities** this extension provides to *other* extensions (the
    /// capability-broker seam). Default empty — most extensions provide none.
    ///
    /// A capability is a service one extension exposes to another as a **trait object**, reached at
    /// run time by trait type via [`NativeCtx::capability`] / [`capability`]. It generalizes the
    /// hardcoded cross-extension seams (`std.tracing`'s context stack, the reactive graph) into one
    /// mechanism, so a new collaboration between extensions — including an out-of-tree package and
    /// core — needs neither a new `NativeCtx` method nor either side naming the other's types. See
    /// [`ExtCapability`].
    fn capabilities(&self) -> &'static [ExtCapability] {
        &[]
    }
}

/// One **per-run capability** an extension provides (the capability-broker seam): a service exposed
/// to other extensions as a trait object, discovered by trait type.
///
/// The provider declares this on its [`Extension::capabilities`]; a consumer asks for it with
/// `capability::<dyn Trait>(ctx)`. The broker matches on [`ExtCapability::id`], ensures the backing
/// [`ExtState`](crate::ExtState) exists, and calls [`ExtCapability::build`] to mint the erased
/// trait-object handle. `noeta-ext-abi` never names any concrete capability trait — only stores and
/// vends these erased thunks.
pub struct ExtCapability {
    /// The capability trait's `TypeId`, e.g. `|| TypeId::of::<dyn ReactiveSource>()`. A thunk
    /// because `TypeId::of` over a `dyn Trait` is not callable in a `&'static` slice initializer.
    pub id: fn() -> std::any::TypeId,
    /// The [`ExtState`](crate::ExtState) slot that backs this capability (the provider's own per-run
    /// state key — e.g. `"std.reactive"`). Reused, so a capability and its owning module share one
    /// cell.
    pub state_key: &'static str,
    /// Initializer for that state on first access — the same `init` the provider passes to
    /// [`NativeCtx::state`], so reaching the engine via a module dispatch or via a capability yields
    /// the *same* cell regardless of which happened first.
    pub init: fn() -> Box<dyn std::any::Any>,
    /// Build the erased trait-object handle from the backing state: returns a boxed
    /// `Box<dyn Trait>` (a concrete, sized fat pointer) type-erased as `Box<dyn Any>`, which
    /// [`capability`] recovers by a safe `downcast`. The handle typically holds a clone of `state`
    /// so it can borrow the engine per-call and release before re-entry.
    pub build: fn(crate::ExtState) -> Box<dyn std::any::Any>,
}

impl std::fmt::Debug for ExtCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtCapability")
            .field("id", &(self.id)())
            .field("state_key", &self.state_key)
            .finish_non_exhaustive()
    }
}

// --- the runtime registry (package-manager Phase 3, N3.0) ---------------------------------------
//
// The binary's **assembled** extension-unit list and the generic lookup layer over it. This
// machinery grew up in `noeta-stdlib` around the dogfooded `std` units, but nothing in it is
// std-specific — and Phase 3's assembly point (the composed-toolchain shim) must not reach through
// the dogfood crate to register its peers. So the mechanism lives here, in the ABI crate: the shim
// (or any host binary) calls [`install`] with the full unit list; `noeta-stdlib::registry` remains
// a facade that lazily installs the std units so the many existing call sites never observe an
// unseeded registry.

use std::sync::OnceLock;

/// A binary's assembled extension units. `OnceLock` because assembly happens exactly once, at
/// process start, before any lookup — and because a `static` slice can't be extended at runtime
/// (the pre-N3.0 registry was a hardwired `static REGISTRY: &[&StdExtension-family]`).
/// An **extension registry** — an assembled set of `&'static` extension units and the lookup
/// surface over them (module/function/type/tier/attribute resolution and dispatch). Every unit is
/// `'static`, so a registry is a cheap selector over static data: its methods return `&'static`
/// references that outlive the registry itself, which is what lets an instance-scoped registry
/// (an embed session's own extension set) hand out the same `'static` references the whole type
/// system already assumes (interned shapes, extern-type tables, function signatures).
///
/// The process-global [`install`]/[`install_default`] seed a single **default** registry that the
/// free-function facade below reads — the path every existing call site (backends, checker, LSP)
/// uses. A host that wants a *different* extension set per session builds its own `Registry` and
/// threads it explicitly (server-hmr F2 / the embed API).
pub struct Registry {
    units: Vec<&'static (dyn Extension + Sync)>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The units are trait objects (no `Debug`); summarize by name, which is what matters.
        f.debug_struct("Registry")
            .field(
                "units",
                &self.units.iter().map(|e| e.name()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// One resolution hop from a namespace group into a member (`http` → `.client`): what the next
/// segment names under a root-qualified prefix. The membership question the compiler/checker ask
/// when lowering `http.client` / `http.Response` / a deeper `a.b.c` chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NsChild {
    /// A concrete native module — its **root-qualified identity** (`std.http.client`), the same
    /// string a `Const::NativeModule` carries and `find_module` keys on.
    Module(String),
    /// A deeper namespace group (`std.http` under a hypothetical `std.http.v2`), root-qualified.
    Namespace(String),
    /// A registered extension type — its qualified identity (`std.http.Response`).
    Type(String),
    /// The member names nothing under this prefix.
    None,
}

/// What a `use <path>.{name}` import binds — the single classification every `use`-collection site
/// (checker, compiler pre-pass + lowering, eval) consults, so the four never drift. `path` is the
/// segments before the imported leaf; `name` is the leaf (`use std.http.client` → path `["std",
/// "http"]`, name `"client"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UseKind {
    /// A concrete native module value (`use std.json`, `use std.http.client`) — root-qualified.
    Module(String),
    /// A navigable **namespace group** (`use std.http`) — a compile-time handle you dot into,
    /// root-qualified. Binds no runtime value on its own; `http.client` resolves at each use site.
    Namespace(String),
    /// A selectively-imported member function (`use std.math.sqrt`) — `(module_identity, func)`.
    MemberFn { module: String, func: String },
    /// A registered extension type (`use std.id.Uuid`) — qualified identity.
    ExternType(String),
    /// Under a known extension root but resolving to no module / namespace / member / type: a
    /// genuine error (a typo'd or nonexistent std target). An extension root is fully enumerable,
    /// so an unknown member cannot be a forward reference — unlike [`UseKind::UserImport`].
    UnknownUnderRoot,
    /// Not under any extension root — a sibling/dependency module import the linker resolves later
    /// (`use App.Models.User`). Opaque to the registry; never an error here.
    UserImport,
}

impl Registry {
    /// Assemble a registry from its complete unit list, validating uniqueness (a violation is a
    /// `panic` — a mis-assembled binary must not start; see [`Registry::validate`]).
    pub fn new(units: Vec<&'static (dyn Extension + Sync)>) -> Registry {
        match Registry::try_new(units) {
            Ok(registry) => registry,
            Err(msg) => panic!("{msg}"),
        }
    }

    /// The fallible twin of [`Registry::new`] — for library entry points (`noeta-embed`) that
    /// promised a `Result` and must not abort the host process over a mis-assembled unit set.
    pub fn try_new(units: Vec<&'static (dyn Extension + Sync)>) -> Result<Registry, String> {
        validate(&units)?;
        Ok(Registry { units })
    }

    /// All units in this registry.
    pub fn extensions(&self) -> &[&'static (dyn Extension + Sync)] {
        &self.units
    }

    /// Find the capability provider for a trait type id, across every registered unit (the
    /// capability-broker seam). Cold path — capabilities are resolved on orchestration ops, never in
    /// hot loops — so a linear scan is right. First declaration wins; a consumer that must see
    /// *every* provider (a plural recognition capability like `ViewSourceExtract`, provided by both
    /// `para.synced` and `para.db`'s `Watch`) walks [`Registry::find_capabilities`] instead.
    pub fn find_capability(&self, id: std::any::TypeId) -> Option<&'static ExtCapability> {
        self.units
            .iter()
            .flat_map(|e| e.capabilities())
            .find(|c| (c.id)() == id)
    }

    /// All providers of a capability trait, in unit-registration order — the **plural** lookup of
    /// the capability-broker seam. Most capability traits have one provider by nature (there is one
    /// reactive engine); a *recognition* capability like `ViewSourceExtract` is legitimately
    /// provided by every foreign reactive-node extension in the registry, and its consumer tries
    /// each in turn. This is the broker-native growth path the seam documented for a second foreign
    /// reactive extension — registry-scoped and declaration-driven, never a process-global list.
    pub fn find_capabilities(
        &self,
        id: std::any::TypeId,
    ) -> impl Iterator<Item = &'static ExtCapability> + '_ {
        self.units
            .iter()
            .flat_map(|e| e.capabilities())
            .filter(move |c| (c.id)() == id)
    }

    /// Find a registered module by its identity string — a **root-qualified path** (`"std.math"`,
    /// nested `"std.http.client"`) or a bare module name (`"math"`).
    pub fn find_module(&self, name: &str) -> Option<&'static ExtModule> {
        if let Some((root, module)) = name.split_once('.')
            && self.is_extension_root(root)
        {
            return self
                .units
                .iter()
                .filter(|e| e.root() == root)
                .flat_map(|e| e.modules())
                .find(|m| m.name == module);
        }
        self.units
            .iter()
            .flat_map(|e| e.modules())
            .find(|m| m.name == name)
    }

    /// The native-dependency **ring** a module identity resolves to, or `None` for always-on core.
    pub fn ring_of(&self, module: &str) -> Option<&'static str> {
        self.find_module(module).and_then(|m| m.ring)
    }

    /// Whether `root` is the namespace root of some registered extension.
    pub fn is_extension_root(&self, root: &str) -> bool {
        self.units.iter().any(|e| e.root() == root)
    }

    /// Find a registered module by its **fully qualified path** — `["std", "math"]`.
    pub fn find_module_qualified(&self, path: &[String]) -> Option<&'static ExtModule> {
        let (root, rest) = path.split_first()?;
        if rest.is_empty() {
            return None;
        }
        let module_name = rest.join(".");
        self.units
            .iter()
            .filter(|e| e.root() == root.as_str())
            .flat_map(|e| e.modules())
            .find(|m| m.name == module_name.as_str())
    }

    /// Whether `path` (a **root-qualified** dotted path like `std.http`) is a navigable **namespace
    /// group**: a strict prefix of ≥1 registered module or extension type, and not itself a concrete
    /// module. `std.http` is a namespace (parent of `std.http.client`/`std.http.server`);
    /// `std.http.client` is a module, not a namespace; `std.json` is a module, not a namespace.
    pub fn is_namespace(&self, path: &str) -> bool {
        let Some((root, _)) = path.split_once('.') else {
            return false;
        };
        if !self.is_extension_root(root) || self.find_module(path).is_some() {
            return false;
        }
        let dotted = format!("{path}.");
        self.units.iter().filter(|e| e.root() == root).any(|e| {
            e.modules()
                .iter()
                .any(|m| format!("{root}.{}", m.name).starts_with(&dotted))
                || e.types().iter().any(|t| t.qualified().starts_with(&dotted))
        })
    }

    /// The **immediate** child segment names under a namespace prefix (`std.http` → `["client",
    /// "server", "Response"]`) — submodules and types one hop down, de-duplicated in registration
    /// order. Empty when `prefix` is not a namespace. Backs member completion and "did you mean".
    pub fn namespace_children(&self, prefix: &str) -> Vec<String> {
        let Some((root, _)) = prefix.split_once('.') else {
            return Vec::new();
        };
        let dotted = format!("{prefix}.");
        let mut out: Vec<String> = Vec::new();
        let push_seg = |rest: &str, out: &mut Vec<String>| {
            let seg = rest.split('.').next().unwrap_or(rest).to_string();
            if !out.contains(&seg) {
                out.push(seg);
            }
        };
        for e in self.units.iter().filter(|e| e.root() == root) {
            for m in e.modules() {
                let rq = format!("{root}.{}", m.name);
                if let Some(rest) = rq.strip_prefix(&dotted) {
                    push_seg(rest, &mut out);
                }
            }
            for t in e.types() {
                if let Some(rest) = t.qualified().strip_prefix(&dotted) {
                    push_seg(rest, &mut out);
                }
            }
        }
        out
    }

    /// The valid next-segment names for a `use <path>.<?>` — every module, namespace, and extension
    /// type reachable one segment past `path`, for "did you mean" on a mistyped import. `path` may be
    /// a bare extension root (`["std"]` → `["http", "math", "json", …]`) or a namespace prefix
    /// (`["std", "http"]` → `["client", "server", "Response"]`). Empty when `path`'s root is not an
    /// extension root. De-duplicated in registration order.
    pub fn import_candidates(&self, path: &[String]) -> Vec<String> {
        let Some(root) = path.first() else {
            return Vec::new();
        };
        if !self.is_extension_root(root) {
            return Vec::new();
        }
        // `prefix.` is the boundary; the next dotted segment after it is one candidate. For a bare
        // root the prefix is just the root, so `std.http.client` contributes `http`; for `std.http`
        // it contributes `client`.
        let dotted = format!("{}.", path.join("."));
        let mut out: Vec<String> = Vec::new();
        let push = |rest: &str, out: &mut Vec<String>| {
            let seg = rest.split('.').next().unwrap_or(rest).to_string();
            if !out.contains(&seg) {
                out.push(seg);
            }
        };
        for e in self.units.iter().filter(|e| e.root() == root) {
            for m in e.modules() {
                let rq = format!("{root}.{}", m.name);
                if let Some(rest) = rq.strip_prefix(&dotted) {
                    push(rest, &mut out);
                }
            }
            for t in e.types() {
                if let Some(rest) = t.qualified().strip_prefix(&dotted) {
                    push(rest, &mut out);
                }
            }
        }
        out
    }

    /// The extension **types** reachable under a namespace prefix, as `(relative path, qualified
    /// identity)` — `std.http` → `[("Response", "std.http.Response")]`. A type under a sub-namespace
    /// keeps the dotted remainder (`("client.Handle", "std.http.client.Handle")`). Lets a `use
    /// std.http` group expose its types for a dotted annotation (`http.Response`) the way it exposes
    /// its modules for a call (`http.client.get`).
    pub fn namespace_types(&self, prefix: &str) -> Vec<(String, String)> {
        let Some((root, _)) = prefix.split_once('.') else {
            return Vec::new();
        };
        let dotted = format!("{prefix}.");
        let mut out = Vec::new();
        for e in self.units.iter().filter(|e| e.root() == root) {
            for t in e.types() {
                let q = t.qualified();
                if let Some(rest) = q.strip_prefix(&dotted) {
                    out.push((rest.to_string(), q.to_string()));
                }
            }
        }
        out
    }

    /// Resolve one namespace hop: what `<prefix>.<member>` names (`std.http` + `client` →
    /// [`NsChild::Module`]`("std.http.client")`). A module wins over a same-named deeper namespace
    /// (a concrete leaf is more specific); a type is checked before the namespace fallback.
    pub fn resolve_namespace_child(&self, prefix: &str, member: &str) -> NsChild {
        let qualified = format!("{prefix}.{member}");
        if self.find_module(&qualified).is_some() {
            NsChild::Module(qualified)
        } else if self.find_type_qualified(&qualified).is_some() {
            NsChild::Type(qualified)
        } else if self.is_namespace(&qualified) {
            NsChild::Namespace(qualified)
        } else {
            NsChild::None
        }
    }

    /// Classify what a `use <path>.{name}` import binds — the single source of truth for
    /// `use`-target resolution, shared by the checker, the compiler (pre-pass + lowering), and the
    /// eval reference so the four never diverge (the check/run divergence this replaces). See
    /// [`UseKind`] for the cases; an extension root that resolves to nothing is
    /// [`UseKind::UnknownUnderRoot`] (a hard error), a non-extension root is [`UseKind::UserImport`]
    /// (the linker's job).
    pub fn classify_use(&self, path: &[String], name: &str) -> UseKind {
        let Some(root) = path.first() else {
            return UseKind::UserImport;
        };
        if !self.is_extension_root(root) {
            return UseKind::UserImport;
        }
        let qualified = format!("{}.{}", path.join("."), name);
        if self.find_module(&qualified).is_some() {
            return UseKind::Module(qualified);
        }
        if self.find_type_qualified(&qualified).is_some() {
            return UseKind::ExternType(qualified);
        }
        if path.len() >= 2 {
            let module = path.join(".");
            if self.find_module(&module).is_some() && self.is_module_function(&module, name) {
                return UseKind::MemberFn {
                    module,
                    func: name.to_string(),
                };
            }
        }
        if self.is_namespace(&qualified) {
            return UseKind::Namespace(qualified);
        }
        UseKind::UnknownUnderRoot
    }

    /// Find a registered function's signature.
    pub fn find_function(&self, module: &str, func: &str) -> Option<&'static ExtFn> {
        self.find_module(module)?
            .functions
            .iter()
            .find(|f| f.name == func)
    }

    /// Find a registered **call-site-typed** function's signature (`json.parse::<T>`) — the
    /// turbofish surface, resolved out of [`ExtModule::typed_functions`]. The single predicate the
    /// checker's `Expr::TypedModuleCall` arm and both backends' typed dispatch consult, so all three
    /// agree on which `module.func::<T>` is call-site-typed.
    pub fn find_typed_function(&self, module: &str, func: &str) -> Option<&'static ExtFn> {
        self.find_module(module)?
            .typed_functions
            .iter()
            .find(|f| f.name == func)
    }

    /// Find a registered **higher-order** function's signature (higher-order-abi H0).
    pub fn find_ctx_function(&self, module: &str, func: &str) -> Option<&'static ExtFn> {
        self.find_module(module)?
            .ctx_functions
            .iter()
            .find(|f| f.name == func)
    }

    /// A function's signature from **either** table — what the checker and name resolution consult.
    pub fn find_function_sig(&self, module: &str, func: &str) -> Option<&'static ExtFn> {
        self.find_function(module, func)
            .or_else(|| self.find_ctx_function(module, func))
    }

    /// Whether `<module>.<func>` names a callable module function — the single predicate the checker
    /// and both backends share to decide what a selective member import (`use std.<mod>.<fn>`)
    /// binds, so all three agree by construction.
    pub fn is_module_function(&self, module: &str, func: &str) -> bool {
        self.find_function_sig(module, func).is_some()
    }

    /// Dispatch a registered higher-order function through the module's [`crate::CtxDispatch`].
    pub fn dispatch_ctx(
        &self,
        module: &str,
        func: &str,
        ctx: &mut dyn crate::NativeCtx,
        args: &[crate::Slot],
    ) -> Result<crate::CtxOut, crate::CtxError> {
        match self.find_module(module).and_then(|m| m.ctx_dispatch) {
            Some(d) => {
                let result = d(func, ctx, args);
                #[cfg(debug_assertions)]
                if let Ok(crate::CtxOut::Out(out)) = &result {
                    self.debug_verify_out(module, func, out);
                }
                result
            }
            None => Err(crate::no_function_error(module, func).into()),
        }
    }

    /// Every extension-contributed CLI subcommand (higher-order-abi H6).
    pub fn commands(&self) -> impl Iterator<Item = &'static crate::ExtCommand> + '_ {
        self.units.iter().flat_map(|e| e.commands())
    }

    /// Find a registered method bundle by its owning module and surface name (kernel-methods K0).
    pub fn find_bundle(&self, module: &str, bundle: &str) -> Option<&'static ExtBundle> {
        self.find_module(module)?
            .bundles
            .iter()
            .find(|b| b.name == bundle)
    }

    /// Route a bound bundle-method call to its bundle's shared ctx dispatch (kernel-methods K0).
    pub fn dispatch_bundle_method(
        &self,
        module: &str,
        bundle: &str,
        method: &str,
        ctx: &mut dyn crate::NativeCtx,
        recv: crate::Slot,
        args: &[crate::Slot],
    ) -> Result<crate::CtxOut, crate::CtxError> {
        match self.find_bundle(module, bundle) {
            Some(b) => (b.ctx_dispatch)(method, ctx, recv, args),
            None => Err(StdError {
                kind: crate::ErrorKind::UnknownName,
                message: format!("no bundle `{bundle}` in module `{module}`"),
            }
            .into()),
        }
    }

    /// Every installed extension's declared dev-tiers, in install order.
    pub fn ext_tiers(&self) -> impl Iterator<Item = &'static ExtTier> + '_ {
        self.units.iter().flat_map(|e| e.tiers().iter())
    }

    /// The installed extension tier named `name`, if any.
    pub fn find_ext_tier(&self, name: &str) -> Option<&'static ExtTier> {
        self.ext_tiers().find(|t| t.name == name)
    }

    /// Every installed extension's **verbatim-body** tier names — the text tiers (`doc` →
    /// markdown) and expression tiers whose `@<name> { … }` bodies the lexer must capture
    /// un-parsed. The front-end (loader, salsa db) seeds the lexer's `TextTiers` with these so a
    /// native tier's bodies capture even though no `.noe` file declares them (a program
    /// `@tier(…, text/expr)` is discovered by the lexer's own token scan instead).
    pub fn ext_verbatim_tier_names(&self) -> Vec<&'static str> {
        self.ext_tiers()
            .filter(|t| t.text.is_some() || t.expr.is_some())
            .map(|t| t.name)
            .collect()
    }

    /// Every installed extension's declared prelude attributes, in install order.
    pub fn ext_attributes(&self) -> impl Iterator<Item = &'static ExtAttribute> + '_ {
        self.units.iter().flat_map(|e| e.attributes().iter())
    }

    /// Every installed extension's derive recipes (derive layer 4), in install order.
    pub fn ext_derives(&self) -> impl Iterator<Item = &'static ExtDerive> + '_ {
        self.units.iter().flat_map(|e| e.derives().iter())
    }

    /// The installed derive recipe named `name`, if any.
    pub fn find_ext_derive(&self, name: &str) -> Option<&'static ExtDerive> {
        self.ext_derives().find(|d| d.name == name)
    }

    /// Every installed extension's tier-body formatters `(language, fn)`, in install order.
    pub fn ext_body_formatters(&self) -> impl Iterator<Item = &'static BodyFormatter> + '_ {
        self.units.iter().flat_map(|e| e.body_formatters().iter())
    }

    /// The installed extension attribute named `name`, if any.
    pub fn find_ext_attribute(&self, name: &str) -> Option<&'static ExtAttribute> {
        self.ext_attributes().find(|a| a.name == name)
    }

    /// Find a registered extern type by its short display name (extern-types X1) — first match in
    /// registration order. This is the *checker-side* bridge from a signature's bare
    /// [`SigType::Named`] name and the E0049 reservation set; **runtime identity paths must use**
    /// [`Registry::find_type_qualified`] with the value's
    /// [`crate::ExternValue::type_identity`], which stays unambiguous when two namespaces share a
    /// short name.
    pub fn find_type(&self, name: &str) -> Option<&'static ExtType> {
        self.units
            .iter()
            .flat_map(|e| e.types())
            .find(|t| t.name == name)
    }

    /// Find a registered extern type by its **qualified identity** (`std.id.Uuid`). Probes with
    /// the allocation-free [`ExtType::is_qualified`] — this runs per candidate type, and the
    /// checker calls it per imported-type resolution (hot under the LSP).
    pub fn find_type_qualified(&self, qualified: &str) -> Option<&'static ExtType> {
        self.units
            .iter()
            .flat_map(|e| e.types())
            .find(|t| t.is_qualified(qualified))
    }

    /// Resolve an extern type from **either** a qualified identity or a bare short name.
    pub fn resolve_type(&self, name: &str) -> Option<&'static ExtType> {
        self.find_type_qualified(name)
            .or_else(|| self.find_type(name))
    }

    /// Find a registered extern type's method signature.
    pub fn find_type_method(&self, type_name: &str, method: &str) -> Option<&'static ExtFn> {
        self.resolve_type(type_name)?
            .methods
            .iter()
            .find(|m| m.name == method)
    }

    /// Find a registered extern type's **higher-order** method signature (higher-order-abi H4).
    pub fn find_type_ctx_method(&self, type_name: &str, method: &str) -> Option<&'static ExtFn> {
        self.resolve_type(type_name)?
            .ctx_methods
            .iter()
            .find(|m| m.name == method)
    }

    /// A type method's signature from **either** table — what the checker consults.
    pub fn find_type_method_sig(&self, type_name: &str, method: &str) -> Option<&'static ExtFn> {
        self.find_type_method(type_name, method)
            .or_else(|| self.find_type_ctx_method(type_name, method))
    }

    /// Route a **higher-order** method call to its type's ctx dispatch (higher-order-abi H4).
    pub fn dispatch_ctx_method(
        &self,
        type_name: &str,
        method: &str,
        ctx: &mut dyn crate::NativeCtx,
        recv: crate::Slot,
        args: &[crate::Slot],
    ) -> Result<crate::CtxOut, crate::CtxError> {
        match self.resolve_type(type_name).and_then(|t| t.ctx_dispatch) {
            Some(d) => {
                let result = d(method, ctx, recv, args);
                #[cfg(debug_assertions)]
                if let Ok(crate::CtxOut::Out(out)) = &result {
                    self.debug_verify_out(type_name, method, out);
                }
                result
            }
            None => Err(crate::no_method_error(type_name, method).into()),
        }
    }

    /// Dispatch a method on an extern receiver through its registered [`ExtType`], resolved by
    /// the value's **qualified identity** ([`crate::ExternValue::type_identity`]) — so two types
    /// sharing a short name under distinct namespaces dispatch to their own tables.
    pub fn dispatch_method(
        &self,
        recv: &mut dyn crate::ExternValue,
        method: &str,
        host: &mut dyn Host,
        args: &[crate::NativeValue],
    ) -> Result<crate::NativeOut, StdError> {
        let identity = recv.type_identity();
        let Some(ext) = self.find_type_qualified(identity) else {
            return Err(StdError {
                kind: crate::ErrorKind::UnknownName,
                message: format!("`{identity}` is not a registered type"),
            });
        };
        let result = (ext.dispatch)(recv, method, host, args);
        #[cfg(debug_assertions)]
        if let Ok(out) = &result {
            self.debug_verify_out(identity, method, out);
        }
        result
    }

    /// Dispatch a registered module function.
    pub fn dispatch(
        &self,
        module: &str,
        func: &str,
        host: &mut dyn Host,
        args: &[crate::NativeValue],
    ) -> Result<crate::NativeOut, StdError> {
        match self.find_module(module) {
            Some(m) => {
                let result = (m.dispatch)(func, host, args);
                #[cfg(debug_assertions)]
                if let Ok(out) = &result {
                    self.debug_verify_out(module, func, out);
                }
                result
            }
            None => Err(crate::no_function_error(module, func)),
        }
    }

    // ----- debug-mode author-contract verification (audit-2 F4) ---------------------------------
    //
    // Contracts the ABI documents but the types cannot enforce, checked where a wrong extension
    // would otherwise corrupt quietly: at the dispatch return, the one seam every produced value
    // crosses. `debug_assertions`-gated — release dispatch is byte-identical, and the checks are
    // per-value walks over already-cold IO/orchestration paths in dev builds only.

    /// Walk a dispatch result for extern values and verify each against its registration —
    /// `type_identity()` must resolve as a qualified identity (a typo'd or short name otherwise
    /// errors at *first method call*, per value), and a `key_capable` type gets a one-shot
    /// equality/order/hash spot check.
    #[cfg(debug_assertions)]
    fn debug_verify_out(&self, owner: &str, func: &str, out: &crate::NativeOut) {
        use crate::NativeOut as O;
        match out {
            O::Extern(e) => self.debug_verify_extern(owner, func, &**e),
            O::Some(inner) | O::Ok(inner) | O::Err(inner) => {
                self.debug_verify_out(owner, func, inner)
            }
            O::List(items) => {
                for item in items {
                    self.debug_verify_out(owner, func, item);
                }
            }
            O::Struct { fields, .. } => {
                for (_, value) in fields {
                    self.debug_verify_out(owner, func, value);
                }
            }
            O::Map(entries) => {
                for (_, value) in entries {
                    self.debug_verify_out(owner, func, value);
                }
            }
            O::Scalar(_)
            | O::Str(_)
            | O::Bytes(_)
            | O::Unit
            | O::Object(_)
            | O::Scalars(_)
            | O::None
            | O::Spawn(_) => {}
        }
    }

    /// The per-extern half of [`Registry::debug_verify_out`].
    #[cfg(debug_assertions)]
    fn debug_verify_extern(&self, owner: &str, func: &str, value: &dyn crate::ExternValue) {
        let name = value.type_identity();
        let Some(ext) = self.find_type_qualified(name) else {
            panic!(
                "extension author contract violated: `{owner}.{func}` returned an extern value \
                 whose type_identity() is `{name}`, which is not a registered qualified type \
                 identity in this registry — ExternValue::type_identity must equal the \
                 `{{namespace}}.{{name}}` of the ExtType the value belongs to"
            );
        };
        if !ext.key_capable {
            return;
        }
        // One spot check per key-capable type per process (debug builds): `key_capable` promises
        // a total order and a stable, content-derived hash. A broken promise corrupts BTreeMap
        // invariants / set canonicalization silently — wrong answers, never an error.
        use std::sync::{Mutex, OnceLock};
        static CHECKED: OnceLock<Mutex<std::collections::HashSet<&'static str>>> = OnceLock::new();
        let mut checked = CHECKED
            .get_or_init(|| Mutex::new(std::collections::HashSet::new()))
            .lock()
            .expect("key-contract check set poisoned");
        if !checked.insert(name) {
            return;
        }
        drop(checked);
        let clone = value.clone_box();
        let contract = |what: &str| {
            format!(
                "extension author contract violated for key_capable type `{name}` (returned by \
                 `{owner}.{func}`): {what} — key_capable promises content-derived equality, a \
                 total order, and a stable hash (see ExtType::key_capable)"
            )
        };
        assert!(
            value.eq_value(&*clone) && clone.eq_value(value),
            "{}",
            contract("a value must equal its own clone under eq_value, both ways")
        );
        assert!(
            value.cmp_value(&*clone) == Some(std::cmp::Ordering::Equal)
                && clone.cmp_value(value) == Some(std::cmp::Ordering::Equal),
            "{}",
            contract("cmp_value over a value and its clone must be Some(Equal), both ways")
        );
        assert!(
            value.hash_value() == clone.hash_value(),
            "{}",
            contract("hash_value must be content-derived (a clone must hash identically)")
        );
    }
}

/// The process-global **default** registry — what the free-function facade below reads, seeded
/// once by [`install`]/[`install_default`]. Instance-scoped callers hold their own [`Registry`].
static DEFAULT: OnceLock<Registry> = OnceLock::new();

/// The default registry, or `None` before it is seeded (callers outside the std facade own their
/// seeding).
pub fn default_registry() -> Option<&'static Registry> {
    DEFAULT.get()
}

/// The process-global default [`Registry`], named for what calling it MEANS: **this call site
/// assumes a single-registry process** (cross-cutting audit finding 5). The front-end crates
/// (checker, loader, IR lowering, bytecode compiler, salsa db) fall back to this when no
/// per-session registry was threaded in — they consume the registry as *data* and deliberately do
/// not link the crate that declares the units (audit-6 finding 2), so the **assembling binary owns
/// seeding**: `noeta_cli::run_cli`, `noeta-runner`, and `noeta-embed` install at entry, and any
/// other driver (a test suite, a bench, a new binary) must call
/// `noeta_stdlib::registry::default_seeded()` (or `install`/`install_with_extras`) before its
/// first front-end lookup.
///
/// Panics if nothing is installed — loudly, because the silent alternative is a checker that
/// reports every `std.*` name as unknown.
pub fn single_registry_process() -> &'static Registry {
    default_registry().unwrap_or_else(|| {
        panic!(
            "no extension registry installed in this process — the assembling binary must seed \
             the default registry before the first front-end lookup (call \
             `noeta_stdlib::registry::default_seeded()` for the std units, or \
             `install`/`install_with_extras` for a composed set), or thread a per-session \
             registry through the options/`_with_registry` seams"
        )
    })
}

/// Install the binary's complete extension-unit list into the **default** registry — callable
/// **once**, before any lookup.
///
/// Uniqueness rules (a violation is a `panic`): extension **names** are unique, and **qualified
/// module identities** are unique across units. Roots are deliberately shared.
///
/// Panics if something was already installed (including the lazy std default).
pub fn install(units: Vec<&'static (dyn Extension + Sync)>) {
    let registry = Registry::new(units);
    if DEFAULT.set(registry).is_err() {
        panic!(
            "extension registry already installed — `install` must run once, before any lookup \
             (a lookup through the std facade lazily installs the default units)"
        );
    }
}

/// Install `provider()`'s units into the default registry only if nothing is installed yet — the
/// lazy-default seam the `noeta-stdlib::registry` facade uses so existing call sites never observe
/// an empty registry, while an explicit earlier [`install`] wins.
pub fn install_default(provider: fn() -> Vec<&'static (dyn Extension + Sync)>) {
    DEFAULT.get_or_init(|| Registry::new(provider()));
}

/// The uniqueness sweep behind [`Registry::new`] — O(n²) over a handful of units.
fn validate(units: &[&'static (dyn Extension + Sync)]) -> Result<(), String> {
    for (i, unit) in units.iter().enumerate() {
        for other in &units[i + 1..] {
            if unit.name() == other.name() {
                return Err(format!(
                    "duplicate extension unit name `{}` in the assembled registry",
                    unit.name()
                ));
            }
        }
    }
    let mut modules: Vec<String> = units
        .iter()
        .flat_map(|e| {
            e.modules()
                .iter()
                .map(|m| format!("{}.{}", e.root(), m.name))
        })
        .collect();
    modules.sort();
    for pair in modules.windows(2) {
        if pair[0] == pair[1] {
            return Err(format!(
                "duplicate qualified module `{}` in the assembled registry",
                pair[0]
            ));
        }
    }
    // Extern-type identities. Runtime dispatch, `is`/`.as<T>()`, and the checker all key on the
    // QUALIFIED identity (`namespace.name` — `ExternValue::type_identity`), so two types sharing
    // a short name under distinct namespaces are distinct and coexist. Two declarations of the
    // same qualified identity, however, would be first-wins at every lookup — refuse to start
    // rather than silently shadow (this also covers a duplicate within one unit).
    let mut types: Vec<((&str, &str), &str)> = units
        .iter()
        .flat_map(|e| {
            e.types()
                .iter()
                .map(move |t| ((t.namespace, t.name), e.name()))
        })
        .collect();
    types.sort_unstable();
    for pair in types.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(format!(
                "duplicate extern type `{}.{}` in the assembled registry (units `{}` and `{}`): \
                 a qualified type identity must be declared exactly once — distinct namespaces \
                 may share a short name, one namespace may not",
                pair[0].0.0, pair[0].0.1, pair[0].1, pair[1].1
            ));
        }
    }
    // A type's namespace must live under its own unit's root — the assembly-time form of the
    // publish lint (`compose --lint`), moved here so EVERY path hits it: `ExtType::DEFAULTS`
    // fills `namespace: "std"`, so a third-party type that forgets `namespace:` would otherwise
    // silently squat the reserved std namespace (winning `use std.X` resolution) until publish.
    for unit in units {
        let root = unit.root();
        for t in unit.types() {
            if t.namespace != root && !t.namespace.starts_with(&format!("{root}.")) {
                return Err(format!(
                    "extern type `{}` of unit `{}` declares namespace `{}`, outside the unit's \
                     root `{root}` — a missing `namespace:` defaults to `std`, which only std \
                     may claim",
                    t.name,
                    unit.name(),
                    t.namespace
                ));
            }
        }
    }
    // The remaining registration axes are first-wins at lookup, so a collision would silently
    // shadow: refuse each at assembly instead (the registry's philosophy — a mis-assembled binary
    // must not start). One loop each: tier names, attribute names, body-formatter languages,
    // command names, capability ids.
    let dup_of = |mut names: Vec<(&str, &str)>| -> Option<((String, String), String)> {
        names.sort_unstable();
        names
            .windows(2)
            .find(|w| w[0].0 == w[1].0)
            .map(|w| ((w[0].1.to_string(), w[1].1.to_string()), w[0].0.to_string()))
    };
    let collect =
        |f: &dyn Fn(&'static (dyn Extension + Sync)) -> Vec<(&'static str, &'static str)>| {
            units.iter().flat_map(|e| f(*e)).collect::<Vec<_>>()
        };
    for (axis, names) in [
        (
            "tier",
            collect(&|e| e.tiers().iter().map(|t| (t.name, e.name())).collect()),
        ),
        (
            "attribute",
            collect(&|e| e.attributes().iter().map(|a| (a.name, e.name())).collect()),
        ),
        (
            "body-formatter language",
            collect(&|e| {
                e.body_formatters()
                    .iter()
                    .map(|(lang, _)| (*lang, e.name()))
                    .collect()
            }),
        ),
        (
            "command",
            collect(&|e| e.commands().iter().map(|c| (c.name, e.name())).collect()),
        ),
    ] {
        if let Some(((a, b), name)) = dup_of(names) {
            return Err(format!(
                "duplicate {axis} `{name}` in the assembled registry (units `{a}` and `{b}`)"
            ));
        }
    }
    // Capability providers: DISTINCT units may legitimately provide the same capability trait —
    // a plural *recognition* capability like `ViewSourceExtract` has one provider per foreign
    // reactive-node extension (`para.synced`, `para.db`'s `Watch`), and its consumer walks them
    // all via `find_capabilities`. What is still a configuration error is one unit declaring the
    // same trait twice (a copy-paste bug — the second declaration is unreachable through the
    // singular lookup and indistinguishable through the plural one).
    for e in units {
        let mut ids: Vec<(std::any::TypeId, &str)> = e
            .capabilities()
            .iter()
            .map(|c| ((c.id)(), c.state_key))
            .collect();
        ids.sort_unstable_by_key(|(id, _)| *id);
        for w in ids.windows(2) {
            if w[0].0 == w[1].0 {
                return Err(format!(
                    "duplicate capability provider inside unit `{}` (state keys `{}` and `{}`): \
                     one extension declares the same capability trait twice",
                    e.name(),
                    w[0].1,
                    w[1].1
                ));
            }
        }
    }
    for module in units.iter().flat_map(|e| e.modules()) {
        for (i, bundle) in module.bundles.iter().enumerate() {
            for other in &module.bundles[i + 1..] {
                if bundle.name == other.name {
                    return Err(format!(
                        "duplicate bundle `{}` in module `{}`",
                        bundle.name, module.name
                    ));
                }
            }
            for (j, method) in bundle.methods.iter().enumerate() {
                for other in &bundle.methods[j + 1..] {
                    if method.sig.name == other.sig.name {
                        return Err(format!(
                            "duplicate method `{}` in bundle `{}.{}`",
                            method.sig.name, module.name, bundle.name
                        ));
                    }
                }
            }
        }
    }
    // Author contracts the ABI states in prose but the types cannot enforce (audit-2 F4). Each of
    // these misuses used to compile clean and fail deep at first call — a runtime "unknown name",
    // a routing miss, or silent checker/dispatch disagreement. Assembly is the one point every
    // path (CLI, composed shim, embed session, lazy default) passes through, so they refuse here,
    // naming the offending declaration.
    for unit in units {
        for module in unit.modules() {
            // A name must live in exactly one dispatch table — routing checks the plain table
            // first, so a doubly-declared name would silently never reach its ctx dispatch.
            for f in module.functions {
                if module.ctx_functions.iter().any(|c| c.name == f.name) {
                    return Err(format!(
                        "function `{}` of module `{}` (unit `{}`) is declared in both `functions` \
                         and `ctx_functions` — a name must live in exactly one dispatch table",
                        f.name,
                        module.name,
                        unit.name()
                    ));
                }
            }
            // A declared higher-order surface with no dispatch to route it to would type-check
            // calls that then fail as "no function" at runtime.
            if !module.ctx_functions.is_empty() && module.ctx_dispatch.is_none() {
                return Err(format!(
                    "module `{}` (unit `{}`) declares ctx_functions but no ctx_dispatch",
                    module.name,
                    unit.name()
                ));
            }
            // A declared call-site-typed surface with no dispatch to route it to would type-check
            // `f::<T>(...)` calls that then fail as "no function" at runtime.
            if !module.typed_functions.is_empty() && module.typed_dispatch.is_none() {
                return Err(format!(
                    "module `{}` (unit `{}`) declares typed_functions but no typed_dispatch",
                    module.name,
                    unit.name()
                ));
            }
            // Every call-site-typed function must declare `RetTy::TypeArg` — the turbofish is what
            // names its result; a `Concrete`/`SameAsArg`/… return in this table would leave the
            // checker with no way to type the call and the recipe unthreaded.
            for f in module.typed_functions {
                if !matches!(f.ret, RetTy::TypeArg(_)) {
                    return Err(format!(
                        "call-site-typed function `{}` of module `{}` (unit `{}`) must declare a \
                         `RetTy::TypeArg` return (its result is named by the turbofish `::<T>`)",
                        f.name,
                        module.name,
                        unit.name()
                    ));
                }
            }
            for f in module
                .functions
                .iter()
                .chain(module.ctx_functions)
                .chain(module.typed_functions)
            {
                validate_optional_tail(f, &format!("module `{}`", module.name), unit.name())?;
            }
        }
        for t in unit.types() {
            for m in t.methods {
                if t.ctx_methods.iter().any(|c| c.name == m.name) {
                    return Err(format!(
                        "method `{}` of type `{}` (unit `{}`) is declared in both `methods` and \
                         `ctx_methods` — a name must live in exactly one dispatch table",
                        m.name,
                        t.name,
                        unit.name()
                    ));
                }
            }
            if !t.ctx_methods.is_empty() && t.ctx_dispatch.is_none() {
                return Err(format!(
                    "type `{}` (unit `{}`) declares ctx_methods but no ctx_dispatch",
                    t.name,
                    unit.name()
                ));
            }
            // `arena_getter` marks one of the *ctx* methods as an inlineable arena read; a name
            // outside that table would make the backend's fast path and the declared surface
            // disagree (the read would inline for a method the type never dispatches).
            if let Some((getter, _)) = t.arena_getter
                && !t.ctx_methods.iter().any(|m| m.name == getter)
            {
                return Err(format!(
                    "type `{}` (unit `{}`) declares arena_getter `{getter}`, which is not one of \
                     its ctx_methods",
                    t.name,
                    unit.name()
                ));
            }
            for m in t.methods.iter().chain(t.ctx_methods) {
                validate_optional_tail(m, &format!("type `{}`", t.name), unit.name())?;
            }
        }
    }
    Ok(())
}

/// Enforce the documented [`SigType::Optional`] convention — "once a parameter is `Optional`,
/// every following parameter is too". The checker derives the required-argument count from the
/// *first* optional, so a required parameter after an optional one would be silently uncheckable.
fn validate_optional_tail(f: &ExtFn, owner: &str, unit: &str) -> Result<(), String> {
    let mut seen_optional = false;
    for p in f.params {
        match p {
            SigType::Optional(_) => seen_optional = true,
            _ if seen_optional => {
                return Err(format!(
                    "`{}` of {owner} (unit `{unit}`) declares a required parameter after an \
                     Optional one — optional parameters must form the trailing tail",
                    f.name
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

// ----- the free-function facade over the default registry (every existing call site) -----
// Each delegates to `default_registry()`; an empty/unseeded default yields the same "not found"
// answers the pre-F2 global did. These stay so the ~60 call sites across the checker, backends,
// LSP, and CLI are untouched until each is threaded to an explicit registry.

/// All installed extension units in the default registry (empty before install).
pub fn extensions() -> &'static [&'static (dyn Extension + Sync)] {
    default_registry().map_or(&[], |r| r.extensions())
}

pub fn find_module(name: &str) -> Option<&'static ExtModule> {
    default_registry().and_then(|r| r.find_module(name))
}

/// The registered module name of a (possibly root-qualified) module identity.
pub fn module_name(module: &str) -> &str {
    module.split_once('.').map_or(module, |(_root, name)| name)
}

pub fn ring_of(module: &str) -> Option<&'static str> {
    default_registry().and_then(|r| r.ring_of(module))
}

pub fn is_extension_root(root: &str) -> bool {
    default_registry().is_some_and(|r| r.is_extension_root(root))
}

pub fn find_module_qualified(path: &[String]) -> Option<&'static ExtModule> {
    default_registry().and_then(|r| r.find_module_qualified(path))
}

pub fn find_function(module: &str, func: &str) -> Option<&'static ExtFn> {
    default_registry().and_then(|r| r.find_function(module, func))
}

pub fn find_ctx_function(module: &str, func: &str) -> Option<&'static ExtFn> {
    default_registry().and_then(|r| r.find_ctx_function(module, func))
}

pub fn find_function_sig(module: &str, func: &str) -> Option<&'static ExtFn> {
    default_registry().and_then(|r| r.find_function_sig(module, func))
}

pub fn dispatch_ctx(
    module: &str,
    func: &str,
    ctx: &mut dyn crate::NativeCtx,
    args: &[crate::Slot],
) -> Result<crate::CtxOut, crate::CtxError> {
    match default_registry() {
        Some(r) => r.dispatch_ctx(module, func, ctx, args),
        None => Err(crate::no_function_error(module, func).into()),
    }
}

pub fn commands() -> impl Iterator<Item = &'static crate::ExtCommand> {
    default_registry().into_iter().flat_map(|r| r.commands())
}

pub fn find_bundle(module: &str, bundle: &str) -> Option<&'static ExtBundle> {
    default_registry().and_then(|r| r.find_bundle(module, bundle))
}

pub fn dispatch_bundle_method(
    module: &str,
    bundle: &str,
    method: &str,
    ctx: &mut dyn crate::NativeCtx,
    recv: crate::Slot,
    args: &[crate::Slot],
) -> Result<crate::CtxOut, crate::CtxError> {
    match default_registry() {
        Some(r) => r.dispatch_bundle_method(module, bundle, method, ctx, recv, args),
        None => Err(StdError {
            kind: crate::ErrorKind::UnknownName,
            message: format!("no bundle `{bundle}` in module `{module}`"),
        }
        .into()),
    }
}

pub fn ext_tiers() -> impl Iterator<Item = &'static ExtTier> {
    default_registry().into_iter().flat_map(|r| r.ext_tiers())
}

pub fn find_ext_tier(name: &str) -> Option<&'static ExtTier> {
    default_registry().and_then(|r| r.find_ext_tier(name))
}

pub fn ext_body_formatters() -> impl Iterator<Item = &'static BodyFormatter> {
    default_registry()
        .into_iter()
        .flat_map(|r| r.ext_body_formatters())
}

pub fn ext_attributes() -> impl Iterator<Item = &'static ExtAttribute> {
    default_registry()
        .into_iter()
        .flat_map(|r| r.ext_attributes())
}

pub fn find_ext_attribute(name: &str) -> Option<&'static ExtAttribute> {
    default_registry().and_then(|r| r.find_ext_attribute(name))
}

pub fn find_type(name: &str) -> Option<&'static ExtType> {
    default_registry().and_then(|r| r.find_type(name))
}

pub fn find_type_qualified(qualified: &str) -> Option<&'static ExtType> {
    default_registry().and_then(|r| r.find_type_qualified(qualified))
}

pub fn resolve_type(name: &str) -> Option<&'static ExtType> {
    default_registry().and_then(|r| r.resolve_type(name))
}

pub fn find_type_method(type_name: &str, method: &str) -> Option<&'static ExtFn> {
    default_registry().and_then(|r| r.find_type_method(type_name, method))
}

pub fn find_type_ctx_method(type_name: &str, method: &str) -> Option<&'static ExtFn> {
    default_registry().and_then(|r| r.find_type_ctx_method(type_name, method))
}

pub fn find_type_method_sig(type_name: &str, method: &str) -> Option<&'static ExtFn> {
    default_registry().and_then(|r| r.find_type_method_sig(type_name, method))
}

pub fn dispatch_ctx_method(
    type_name: &str,
    method: &str,
    ctx: &mut dyn crate::NativeCtx,
    recv: crate::Slot,
    args: &[crate::Slot],
) -> Result<crate::CtxOut, crate::CtxError> {
    match default_registry() {
        Some(r) => r.dispatch_ctx_method(type_name, method, ctx, recv, args),
        None => Err(crate::no_method_error(type_name, method).into()),
    }
}

pub fn dispatch_method(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    host: &mut dyn Host,
    args: &[crate::NativeValue],
) -> Result<crate::NativeOut, StdError> {
    let identity = recv.type_identity();
    match default_registry() {
        Some(r) => r.dispatch_method(recv, method, host, args),
        None => Err(StdError {
            kind: crate::ErrorKind::UnknownName,
            message: format!("`{identity}` is not a registered type"),
        }),
    }
}

pub fn dispatch(
    module: &str,
    func: &str,
    host: &mut dyn Host,
    args: &[crate::NativeValue],
) -> Result<crate::NativeOut, StdError> {
    match default_registry() {
        Some(r) => r.dispatch(module, func, host, args),
        None => Err(crate::no_function_error(module, func)),
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    #[test]
    fn sig_render_matches_checker_display_conventions() {
        // The canonical spellings agree with `noeta_types::Type`'s Display: `void`, `Option<T>`,
        // `A | B`, `fn(..) -> ..` — a registry signature and a diagnostic show the same text.
        assert_eq!(SigType::Unit.render(), "void");
        assert_eq!(SigType::Option(&SigType::Int).render(), "Option<int>");
        assert_eq!(
            SigType::Union(&[SigType::String, SigType::Bytes]).render(),
            "string | bytes"
        );
        assert_eq!(
            SigType::Fn(&[SigType::Named("Request")], &SigType::Dyn).render(),
            "fn(Request) -> dyn"
        );
        assert_eq!(
            SigType::Map(&SigType::String, &SigType::String).render(),
            "Map<string, string>"
        );
        assert_eq!(
            SigType::Future(&SigType::Named("Response")).render(),
            "Future<Response>"
        );
        // Trailing-optional parameter: an arity marker, not the Option value type.
        assert_eq!(SigType::Optional(&SigType::Int).render(), "int?");
        // Type variables: positional letters, then numbered.
        assert_eq!(SigType::Var(0).render(), "T");
        assert_eq!(SigType::Var(1).render(), "U");
        assert_eq!(SigType::Var(7).render(), "T2");
        assert_eq!(SigType::BoundedVar(0, "Mergeable").render(), "T: Mergeable");
        assert_eq!(
            SigType::Generic("Cell", &[SigType::Var(0)]).render(),
            "Cell<T>"
        );
    }

    #[test]
    fn ext_fn_render_is_the_full_surface_signature() {
        let f = ExtFn {
            name: "get",
            params: &[
                SigType::String,
                SigType::Optional(&SigType::Map(&SigType::String, &SigType::String)),
            ],
            ret: RetTy::Concrete(SigType::Named("Response")),
        };
        assert_eq!(f.render(), "fn get(string, Map<string, string>?): Response");

        let same_as = ExtFn {
            name: "add",
            params: &[SigType::Named("vec3"), SigType::Named("vec3")],
            ret: RetTy::SameAsArg(0),
        };
        assert_eq!(same_as.render(), "fn add(vec3, vec3): vec3");
    }
}

#[cfg(test)]
mod runtime_registry_tests {
    use super::*;

    struct Unit(&'static str, &'static str, &'static [ExtModule]);
    impl Extension for Unit {
        fn name(&self) -> &'static str {
            self.0
        }
        fn root(&self) -> &'static str {
            self.1
        }
        fn modules(&self) -> &'static [ExtModule] {
            self.2
        }
    }

    const M_MATH: ExtModule = ExtModule {
        name: "math",
        ..ExtModule::DEFAULTS
    };
    static A: Unit = Unit("a.core", "a", &[M_MATH]);
    static A2: Unit = Unit("a.extra", "a", &[]);
    static B_DUP_NAME: Unit = Unit("a.core", "b", &[]);
    static B_DUP_MODULE: Unit = Unit("b.core", "a", &[M_MATH]);

    // --- method bundles (kernel-methods K0) ---
    const VEC3_CONSTRAINT: PackedConstraint = PackedConstraint {
        fields: &[
            ConstraintField::F32,
            ConstraintField::F32,
            ConstraintField::F32,
        ],
        layout: ConstraintLayout::Any,
    };
    const KERNELS: ExtBundle = ExtBundle {
        name: "Kernels",
        constraint: VEC3_CONSTRAINT,
        methods: &[
            BundleFn {
                sig: ExtFn {
                    name: "dot",
                    params: &[SigType::Dyn],
                    ret: RetTy::Concrete(SigType::F32),
                },
                receiver: BundleReceiver::Element,
            },
            BundleFn {
                sig: ExtFn {
                    name: "scale_all",
                    params: &[SigType::F32],
                    ret: RetTy::SameAsArg(0),
                },
                receiver: BundleReceiver::Bulk,
            },
        ],
        ctx_dispatch: |method, _, _, _| {
            Err(StdError {
                kind: crate::ErrorKind::UnknownName,
                message: format!("test bundle never dispatches `{method}`"),
            }
            .into())
        },
    };
    const M_VEC: ExtModule = ExtModule {
        name: "vec",
        bundles: &[KERNELS],
        ..ExtModule::DEFAULTS
    };
    static G: Unit = Unit("g.core", "g", &[M_VEC]);

    #[test]
    fn bundle_lookup_and_method_table() {
        // `find_bundle` needs an installed registry; ride the same process-global default the
        // lifecycle test seeds (Unit A) by validating structure directly instead.
        let bundle = M_VEC.bundles.iter().find(|b| b.name == "Kernels").unwrap();
        assert!(bundle.method("dot").is_some());
        assert_eq!(
            bundle.method("scale_all").unwrap().receiver,
            BundleReceiver::Bulk
        );
        assert!(bundle.method("nope").is_none());
        validate(&[&G]).expect("well-formed: unique bundle + method names");
    }

    // --- namespace groups (module-namespaces) ---
    struct NsUnit(&'static str, &'static [ExtModule], &'static [ExtType]);
    impl Extension for NsUnit {
        fn name(&self) -> &'static str {
            "std.core"
        }
        fn root(&self) -> &'static str {
            self.0
        }
        fn modules(&self) -> &'static [ExtModule] {
            self.1
        }
        fn types(&self) -> &'static [ExtType] {
            self.2
        }
    }

    const M_HTTP_CLIENT: ExtModule = ExtModule {
        name: "http.client",
        ..ExtModule::DEFAULTS
    };
    const M_HTTP_SERVER: ExtModule = ExtModule {
        name: "http.server",
        ..ExtModule::DEFAULTS
    };
    const M_JSON: ExtModule = ExtModule {
        name: "json",
        ..ExtModule::DEFAULTS
    };
    const T_RESPONSE: ExtType = ExtType {
        name: "Response",
        namespace: "std.http",
        ..ExtType::DEFAULTS
    };

    fn ns_registry() -> Registry {
        static U: NsUnit = NsUnit(
            "std",
            &[M_HTTP_CLIENT, M_HTTP_SERVER, M_JSON],
            &[T_RESPONSE],
        );
        Registry::new(vec![&U])
    }

    #[test]
    fn namespace_prefix_detection() {
        let reg = ns_registry();
        // A shared prefix of ≥1 module/type is a namespace; a concrete module is not.
        assert!(reg.is_namespace("std.http"), "parent of http.client/server");
        assert!(!reg.is_namespace("std.http.client"), "a concrete module");
        assert!(!reg.is_namespace("std.json"), "a leaf module, not a group");
        assert!(!reg.is_namespace("std.bogus"), "no such prefix");
        assert!(!reg.is_namespace("other.http"), "not an extension root");
    }

    #[test]
    fn namespace_children_lists_submodules_and_types() {
        let reg = ns_registry();
        let mut kids = reg.namespace_children("std.http");
        kids.sort();
        assert_eq!(kids, vec!["Response", "client", "server"]);
        assert!(reg.namespace_children("std.json").is_empty(), "a leaf");
    }

    #[test]
    fn resolve_namespace_child_hops() {
        let reg = ns_registry();
        assert_eq!(
            reg.resolve_namespace_child("std.http", "client"),
            NsChild::Module("std.http.client".into())
        );
        assert_eq!(
            reg.resolve_namespace_child("std.http", "Response"),
            NsChild::Type("std.http.Response".into())
        );
        assert_eq!(
            reg.resolve_namespace_child("std.http", "bogus"),
            NsChild::None
        );
    }

    #[test]
    fn classify_use_covers_every_case() {
        let reg = ns_registry();
        let s = |x: &str| x.to_string();
        // A namespace group (`use std.http`).
        assert_eq!(
            reg.classify_use(&[s("std")], "http"),
            UseKind::Namespace("std.http".into())
        );
        // A concrete nested module (`use std.http.client`) and a flat one (`use std.json`).
        assert_eq!(
            reg.classify_use(&[s("std"), s("http")], "client"),
            UseKind::Module("std.http.client".into())
        );
        assert_eq!(
            reg.classify_use(&[s("std")], "json"),
            UseKind::Module("std.json".into())
        );
        // An extension type (`use std.http.Response`).
        assert_eq!(
            reg.classify_use(&[s("std"), s("http")], "Response"),
            UseKind::ExternType("std.http.Response".into())
        );
        // Under a known root but nothing resolves → a hard error (typo'd std target).
        assert_eq!(
            reg.classify_use(&[s("std")], "bogus"),
            UseKind::UnknownUnderRoot
        );
        assert_eq!(
            reg.classify_use(&[s("std"), s("http")], "bogus"),
            UseKind::UnknownUnderRoot
        );
        // Not an extension root → a user/sibling import the linker resolves later.
        assert_eq!(
            reg.classify_use(&[s("App"), s("Models")], "User"),
            UseKind::UserImport
        );
    }

    #[test]
    fn duplicate_bundle_name_in_a_module_is_rejected() {
        const M_DUP: ExtModule = ExtModule {
            name: "vec2",
            bundles: &[KERNELS, KERNELS],
            ..ExtModule::DEFAULTS
        };
        static H: Unit = Unit("h.core", "h", &[M_DUP]);
        assert!(validate(&[&H]).is_err(), "duplicate bundle name must panic");
    }

    #[test]
    fn duplicate_method_name_in_a_bundle_is_rejected() {
        const DUP_METHODS: ExtBundle = ExtBundle {
            methods: &[
                BundleFn {
                    sig: ExtFn {
                        name: "dot",
                        ..ExtFn::DEFAULTS
                    },
                    receiver: BundleReceiver::Element,
                },
                // Same name on the other receiver kind is still a conflict (one name, one meaning).
                BundleFn {
                    sig: ExtFn {
                        name: "dot",
                        ..ExtFn::DEFAULTS
                    },
                    receiver: BundleReceiver::Bulk,
                },
            ],
            ..KERNELS
        };
        const M_DUP: ExtModule = ExtModule {
            name: "vec3",
            bundles: &[DUP_METHODS],
            ..ExtModule::DEFAULTS
        };
        static I: Unit = Unit("i.core", "i", &[M_DUP]);
        assert!(
            validate(&[&I]).is_err(),
            "duplicate method name in a bundle must panic"
        );
    }

    #[test]
    fn duplicate_unit_name_is_rejected() {
        assert!(
            validate(&[&A, &B_DUP_NAME]).is_err(),
            "duplicate unit name must panic"
        );
    }

    #[test]
    fn duplicate_qualified_module_is_rejected() {
        // Same root (`a`) + same module name (`math`) across two differently-named units.
        assert!(
            validate(&[&A, &B_DUP_MODULE]).is_err(),
            "duplicate qualified module must panic"
        );
    }

    #[test]
    fn same_short_name_across_namespaces_coexists() {
        // Two units registering the same SHORT type name under DISTINCT namespaces assemble
        // fine: runtime dispatch and `is`/`.as<T>()` key on the qualified identity a value
        // carries (`ExternValue::type_identity`), so `std.metrics.Counter` and
        // `acme.metrics.Counter` are distinct types — the coexistence the qualified-identity
        // model exists to enable. Both stay individually resolvable by their qualified name;
        // the ambiguous short-name lookup answers the first registration (checker-side only).
        const T_STD_COUNTER: ExtType = ExtType {
            name: "Counter",
            namespace: "std.metrics",
            ..ExtType::DEFAULTS
        };
        const T_ACME_COUNTER: ExtType = ExtType {
            name: "Counter",
            namespace: "acme.metrics",
            ..ExtType::DEFAULTS
        };
        static SM: NsUnit = NsUnit("std", &[], &[T_STD_COUNTER]);
        static AM: TypedUnit = TypedUnit("acme.metrics", "acme", &[T_ACME_COUNTER]);
        assert!(
            validate(&[&SM, &AM]).is_ok(),
            "same short name under distinct namespaces must assemble"
        );
        let reg = Registry::new(vec![&SM, &AM]);
        assert_eq!(
            reg.find_type_qualified("std.metrics.Counter")
                .map(|t| t.namespace),
            Some("std.metrics")
        );
        assert_eq!(
            reg.find_type_qualified("acme.metrics.Counter")
                .map(|t| t.namespace),
            Some("acme.metrics")
        );
    }

    #[test]
    fn duplicate_qualified_extern_type_is_rejected() {
        // The SAME qualified identity twice — whether across units or within one — is first-wins
        // at every lookup and must refuse to assemble.
        const T_COUNTER_A: ExtType = ExtType {
            name: "Counter",
            namespace: "acme.metrics",
            ..ExtType::DEFAULTS
        };
        const T_COUNTER_B: ExtType = ExtType {
            name: "Counter",
            namespace: "acme.metrics",
            ..ExtType::DEFAULTS
        };
        static AM1: TypedUnit = TypedUnit("acme.metrics", "acme", &[T_COUNTER_A]);
        static AM2: TypedUnit = TypedUnit("acme.metrics2", "acme", &[T_COUNTER_B]);
        assert!(
            validate(&[&AM1, &AM2]).is_err(),
            "a duplicate qualified extern-type identity must refuse to assemble"
        );
        static AM_TWICE: TypedUnit = TypedUnit("acme.metrics", "acme", &[T_COUNTER_A, T_COUNTER_B]);
        assert!(
            validate(&[&AM_TWICE]).is_err(),
            "a duplicate qualified identity within one unit must refuse to assemble"
        );
    }

    #[test]
    fn a_type_namespace_outside_the_units_root_is_rejected() {
        // `ExtType::DEFAULTS` fills `namespace: "std"`; a third-party unit that forgets the field
        // would silently squat std. Assembly refuses it (the publish lint's rule, moved to where
        // every path hits it).
        const T_SQUATTER: ExtType = ExtType {
            name: "Widget",
            // The DEFAULTS value a forgotten `namespace:` leaves behind.
            ..ExtType::DEFAULTS
        };
        static SQ: TypedUnit = TypedUnit("acme.widgets", "acme", &[T_SQUATTER]);
        assert!(
            validate(&[&SQ]).is_err(),
            "a type namespaced outside its unit's root must refuse to assemble"
        );
    }

    #[test]
    fn a_duplicate_tier_name_across_units_is_rejected() {
        // Tier lookup is first-wins; a collision must refuse at assembly instead of shadowing.
        struct TierUnit(&'static str, &'static str);
        impl Extension for TierUnit {
            fn name(&self) -> &'static str {
                self.0
            }
            fn root(&self) -> &'static str {
                self.1
            }
            fn modules(&self) -> &'static [ExtModule] {
                &[]
            }
            fn tiers(&self) -> &'static [ExtTier] {
                &[ExtTier {
                    name: "audit",
                    sites: &[],
                    config: None,
                    text: None,
                    expr: None,
                    handler: None,
                }]
            }
        }
        static A_TIER: TierUnit = TierUnit("a.tools", "a");
        static B_TIER: TierUnit = TierUnit("b.tools", "b");
        assert!(
            validate(&[&A_TIER, &B_TIER]).is_err(),
            "a duplicate tier name across units must refuse to assemble"
        );
    }

    /// A unit with only types, under its own name and root (the `NsUnit` helper hardcodes
    /// `std.core` as its unit name, so a second distinctly-named unit needs its own shape).
    struct TypedUnit(&'static str, &'static str, &'static [ExtType]);
    impl Extension for TypedUnit {
        fn name(&self) -> &'static str {
            self.0
        }
        fn root(&self) -> &'static str {
            self.1
        }
        fn modules(&self) -> &'static [ExtModule] {
            &[]
        }
        fn types(&self) -> &'static [ExtType] {
            self.2
        }
    }

    #[test]
    fn shared_root_across_units_is_fine() {
        // The std pattern: six units all rooted `std`. Distinct names, distinct modules.
        validate(&[&A, &A2]).expect("shared roots across distinctly-named units are valid");
    }

    // --- author-contract checks (audit-2 F4) ---

    #[test]
    fn an_arena_getter_outside_the_ctx_methods_is_rejected() {
        // `arena_getter` marks one of the CTX methods as an inlineable read; a name outside that
        // table would make the backend's fast route and the dispatch surface disagree.
        const T_BAD_GETTER: ExtType = ExtType {
            name: "Cellish",
            namespace: "k",
            ctx_methods: &[ExtFn {
                name: "get",
                ..ExtFn::DEFAULTS
            }],
            ctx_dispatch: Some(|_, _, _, _| Err(crate::panic_error("unused").into())),
            arena_getter: Some(("peek", |_| 0)),
            ..ExtType::DEFAULTS
        };
        static K: TypedUnit = TypedUnit("k.core", "k", &[T_BAD_GETTER]);
        assert!(
            validate(&[&K]).is_err(),
            "an arena_getter naming a non-ctx method must refuse to assemble"
        );
    }

    #[test]
    fn a_ctx_table_without_a_ctx_dispatch_is_rejected() {
        const M_NO_DISPATCH: ExtModule = ExtModule {
            name: "orphan",
            ctx_functions: &[ExtFn {
                name: "go",
                ..ExtFn::DEFAULTS
            }],
            // ctx_dispatch stays the DEFAULTS `None` — declared surface, nothing to route to.
            ..ExtModule::DEFAULTS
        };
        static L: Unit = Unit("l.core", "l", &[M_NO_DISPATCH]);
        assert!(
            validate(&[&L]).is_err(),
            "ctx_functions with no ctx_dispatch must refuse to assemble"
        );
    }

    #[test]
    fn a_name_in_both_dispatch_tables_is_rejected() {
        // Routing consults the plain table first, so a doubly-declared name would silently never
        // reach its ctx dispatch.
        const F_GO: ExtFn = ExtFn {
            name: "go",
            ..ExtFn::DEFAULTS
        };
        const M_DOUBLE: ExtModule = ExtModule {
            name: "both",
            functions: &[F_GO],
            ctx_functions: &[F_GO],
            ctx_dispatch: Some(|_, _, _| Err(crate::panic_error("unused").into())),
            ..ExtModule::DEFAULTS
        };
        static M: Unit = Unit("m.core", "m", &[M_DOUBLE]);
        assert!(
            validate(&[&M]).is_err(),
            "a name in both functions and ctx_functions must refuse to assemble"
        );
    }

    #[test]
    fn a_required_param_after_an_optional_one_is_rejected() {
        // The checker derives the required-arg count from the FIRST Optional, so a required
        // parameter after it would be silently uncheckable.
        const M_BAD_TAIL: ExtModule = ExtModule {
            name: "tail",
            functions: &[ExtFn {
                name: "f",
                params: &[SigType::Optional(&SigType::Int), SigType::String],
                ..ExtFn::DEFAULTS
            }],
            ..ExtModule::DEFAULTS
        };
        static N: Unit = Unit("n.core", "n", &[M_BAD_TAIL]);
        assert!(
            validate(&[&N]).is_err(),
            "a required parameter after an Optional must refuse to assemble"
        );
    }

    /// A minimal extern value whose key contract is deliberately broken: `cmp_value` answers
    /// `None` (no total order) even though its type registers `key_capable`.
    #[cfg(debug_assertions)]
    #[derive(Debug, Clone)]
    struct BadKey;

    #[cfg(debug_assertions)]
    impl crate::ExternValue for BadKey {
        fn type_identity(&self) -> &'static str {
            "q.BadKey"
        }
        fn eq_value(&self, other: &dyn crate::ExternValue) -> bool {
            other.as_any().downcast_ref::<BadKey>().is_some()
        }
        fn cmp_value(&self, _other: &dyn crate::ExternValue) -> Option<std::cmp::Ordering> {
            None // the broken promise
        }
        fn hash_value(&self) -> u64 {
            0
        }
        fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
            write!(out, "badkey")
        }
        fn clone_box(&self) -> Box<dyn crate::ExternValue> {
            Box::new(self.clone())
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
    }

    // Debug builds only — the verifier is compiled out of release dispatch entirely.
    #[cfg(debug_assertions)]
    #[test]
    fn debug_verifier_catches_the_broken_author_contracts() {
        const T_BADKEY: ExtType = ExtType {
            name: "BadKey",
            namespace: "q",
            key_capable: true,
            ..ExtType::DEFAULTS
        };
        static Q: TypedUnit = TypedUnit("q.core", "q", &[T_BADKEY]);
        let reg = Registry::new(vec![&Q]);

        // A dispatch result whose extern type_identity resolves nowhere: the typo'd-identity
        // contract (`ExternValue::type_identity` must equal the ExtType's qualified identity)
        // fails at the dispatch return with a message naming the origin, not at first method
        // call per value.
        /// The typo case: `type_identity()` answers an identity no ExtType registers.
        #[derive(Debug, Clone)]
        struct Typo;
        impl crate::ExternValue for Typo {
            fn type_identity(&self) -> &'static str {
                "q.BadKye" // sic
            }
            fn eq_value(&self, other: &dyn crate::ExternValue) -> bool {
                other.as_any().downcast_ref::<Typo>().is_some()
            }
            fn cmp_value(&self, _other: &dyn crate::ExternValue) -> Option<std::cmp::Ordering> {
                None
            }
            fn hash_value(&self) -> u64 {
                0
            }
            fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
                write!(out, "typo")
            }
            fn clone_box(&self) -> Box<dyn crate::ExternValue> {
                Box::new(self.clone())
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
            fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
                self
            }
        }
        // `catch_unwind` requires unwind-safe captures; the registry and outs hold trait objects,
        // so move each into its closure via AssertUnwindSafe (the closures only read them, and
        // nothing observes them after the panic).
        use std::panic::AssertUnwindSafe;
        let unregistered = crate::NativeOut::Extern(crate::ExternBox::new(Typo));
        let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
            reg.debug_verify_out("q.mod", "make", &unregistered)
        }));
        assert!(
            caught.is_err(),
            "an unregistered type_identity must panic in debug"
        );

        // A key_capable type whose cmp_value is not a total order fails the one-shot spot check —
        // wrapped in Some/List to prove the walk recurses into containers.
        let nested = crate::NativeOut::Some(Box::new(crate::NativeOut::List(vec![
            crate::NativeOut::Extern(crate::ExternBox::new(BadKey)),
        ])));
        let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
            reg.debug_verify_out("q.mod", "key", &nested)
        }));
        assert!(
            caught.is_err(),
            "a broken key_capable contract must panic in debug"
        );
    }

    // One test drives the whole process-global lifecycle (the `OnceLock` is per-process, so
    // ordering across #[test] threads would race if split up).
    #[test]
    fn install_lifecycle() {
        assert!(extensions().is_empty(), "nothing installed at startup");
        install_default(|| vec![&A, &G]);
        assert_eq!(extensions().len(), 2);
        assert!(find_module("a.math").is_some());
        assert!(find_module("math").is_some(), "bare-name lookup");
        // Bundle resolution (kernel-methods K0): qualified and bare module forms.
        assert!(find_bundle("g.vec", "Kernels").is_some());
        assert!(find_bundle("vec", "Kernels").is_some(), "bare-name lookup");
        assert!(find_bundle("g.vec", "Nope").is_none());
        // A second default is a no-op — the first install wins.
        install_default(|| vec![&A]);
        assert_eq!(extensions().len(), 2);
        // An explicit install after anything is installed is a hard error.
        let result = std::panic::catch_unwind(|| install(vec![&A2]));
        assert!(result.is_err(), "install after install_default must panic");
    }
}
