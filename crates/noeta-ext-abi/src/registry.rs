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
    /// A native-declared language **enum value** (native-extensibility S1) — the argument view of a
    /// real [`Value::Enum`], so a dispatch may receive a variant a user matched or a native call
    /// produced (`describe(color: Color)`). `enum_name` is the enum's **short** name (the runtime
    /// identity, matching how a value carries it), `variant` the case, and `fields` its positional
    /// payload deeply marshalled (empty for a fieldless/backed variant). The twin of
    /// [`NativeOut::Variant`] on the return path.
    Variant {
        enum_name: String,
        variant: String,
        variant_index: u32,
        fields: Vec<NativeValue>,
    },
    /// A native-declared language **class** instance (native-extensibility S2) — the argument view
    /// of a real class `Object`, so a dispatch may receive a native class value a program
    /// constructed or a native call produced (`relabel(h: Handle, s)`). `class` is the class's
    /// **short** name (its runtime shape name); `fields` are its `(name, value)` pairs in slot
    /// order, each deeply marshalled (an extern-handle field crosses as [`NativeValue::Extern`]).
    /// The twin of [`NativeOut::Instance`] on the return path.
    Instance {
        class: String,
        fields: Vec<(String, NativeValue)>,
    },
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
    /// A native-declared language **enum value** (native-extensibility S1) — a REAL enum the
    /// backend materializes (a `Value::Enum` / interned enum shape), NOT a string shortcut, so a
    /// user `match` over it is exhaustive (E0011). `enum_name` is the enum's **short** name (the
    /// runtime identity a pattern's `type_name` compares against and both backends stamp on the
    /// value), `variant` the case, `variant_index` the declaration index (what a derived
    /// `Comparable` orders by, and what keeps the two backends' shapes identical), and `fields` the
    /// positional payload, each itself a [`NativeOut`] so a payload-carrying variant nests. A
    /// fieldless or backed variant carries an empty `fields`. The dispatch names the enum by its
    /// short name; the checker's qualified identity is a separate, compile-time concern.
    Variant {
        enum_name: String,
        variant: String,
        variant_index: u32,
        fields: Vec<NativeOut>,
    },
    /// A native-declared **fielded-type** instance (native-extensibility S2, unified) — a REAL
    /// language `Object` the backend materializes with named fields, distinct from an anonymous
    /// value struct built by a call-site recipe ([`NativeOut::Struct`]). `kind` selects the shape:
    /// [`FieldedKind::Class`] → a **class-kind** shape (reference identity, RC + cycle participation,
    /// its extern-handle field's `Drop` as destructor); [`FieldedKind::Struct`] → a **struct-kind**
    /// shape (structural equality, value/copy semantics — the object model derives it). `class` is
    /// the type's **short** name (the runtime shape name, matching a source-constructed instance so
    /// the two interchange); `fields` are its `(name, value)` pairs **in the type's declared slot
    /// order**, each itself a [`NativeOut`] so a field nests (a class's native-state field is a
    /// [`NativeOut::Extern`] carrying the handle). Carrying `kind` here keeps materialization
    /// registry-free and lets both backends pick the identical shape kind from the value itself. The
    /// dispatch names the type by its short name; the checker's qualified identity is a separate,
    /// compile-time concern (the twin of [`NativeOut::Variant`]).
    Instance {
        class: String,
        fields: Vec<(String, NativeOut)>,
        kind: FieldedKind,
    },
    /// A native class **instance method's in-place mutation** (native-extensibility S3 / boundary 1):
    /// the method returns an explicit **write-set** applied to its LIVE receiver, plus the value the
    /// method itself returns. Only meaningful returned from a [`ClassDispatch`]; the backend, at the
    /// class-method call site, applies each `(field, value)` in `writes` in place to the receiver's
    /// slot — the same primitive a source-level `self.x = v` uses — so the mutation is visible through
    /// every alias and identity is preserved, then materializes `ret` as the method's result.
    ///
    /// **Why an explicit write-set, not a mutable receiver snapshot:** the receiver crosses as a value
    /// snapshot ([`NativeValue::Instance`]), so diffing a mutated snapshot back would (a) silently drop
    /// a mutation of a reference-typed field and (b) re-marshal the native-state extern-handle field,
    /// `clone_box`ing it into a second box (double-free). Naming the writes explicitly avoids both: the
    /// backend never re-marshals a field it wasn't told to write. A write targets a **`is_mut`** field
    /// (a non-`mut` or unknown field is a runtime error — the ABI mirrors source-level E0022 rules).
    /// The old slot value is released (so swapping a native-state handle fires the displaced one's
    /// destructor). `ret` is the method's ordinary result (`NativeOut::Unit` for a `void` mutator).
    InstanceUpdate {
        writes: Vec<(String, NativeOut)>,
        ret: Box<NativeOut>,
    },
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
    /// The constrained shape's **element scalar type** itself — resolved by the checker at the
    /// `impl <module>.<Bundle> for T {}` site from `T`'s uniform numeric field kind (`scale(s:
    /// Elem)`, an element-returning op). Only meaningful on a [`BundleFn`] whose bundle constraint
    /// binds a uniform numeric field ([`ConstraintField::AnyNumeric`]); the checker resolves it
    /// against the bound struct's concrete field type, so ONE bundle serves every element width.
    Elem,
    /// The element's **widened accumulator** (`dot() -> ElemWide`): an integer element (`int`, any
    /// `iN`/`uN`) widens to `int` (i64), `f32` stays `f32`, `f64` stays `f64`, `float` stays
    /// `float`. Matches the `Scalar::Wide` associated type — the accumulator a cross-lane reduction
    /// cannot let silently wrap. Element-relative, like [`RetTy::Elem`].
    ElemWide,
    /// The element's **float promotion** (`length() -> ElemFloat`): an integer element (`int`, any
    /// `iN`/`uN`) promotes to `float` (f64), `f32` stays `f32`, `f64` stays `f64`, `float` stays
    /// `float`. Matches the `Scalar::Float` associated type — the result of a magnitude/`sqrt` op.
    /// Element-relative, like [`RetTy::Elem`].
    ElemFloat,
    /// A **`List` of the element's widened accumulator** (`dot_all() -> List<ElemWide>`) — the bulk
    /// twin of [`RetTy::ElemWide`]: one widened reduction value per element of a packed `List<T>`.
    /// Resolved by the checker against the bound shape's element kind, exactly as [`RetTy::ElemWide`],
    /// then wrapped in `List<_>` (the scalar-unification collapse of the per-type `dot_all` returns).
    ListElemWide,
    /// A **`List` of the element's float promotion** (`length_all() -> List<ElemFloat>`) — the bulk
    /// twin of [`RetTy::ElemFloat`].
    ListElemFloat,
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
            // Element-relative: the concrete element type is only known at the bundle's `impl`
            // site, so a bare signature renders the symbolic element form (like `Var` → `T`).
            RetTy::Elem => "Elem".to_string(),
            RetTy::ElemWide => "ElemWide".to_string(),
            RetTy::ElemFloat => "ElemFloat".to_string(),
            RetTy::ListElemWide => "List<ElemWide>".to_string(),
            RetTy::ListElemFloat => "List<ElemFloat>".to_string(),
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

/// A native **class**'s instance-method dispatch (native-extensibility S3 / Pass 2a) — the
/// [`ExtClass`] analogue of [`TypeDispatch`]. A native class value is a real language `Object` (a
/// class-kind shape), **not** an [`crate::ExternValue`], so its receiver crosses as the whole
/// instance marshalled to a [`NativeValue::Instance`] (class name + `(field, value)` pairs in slot
/// order) — the same shape a class value takes when it crosses arg-IN. The method reads its fields
/// by name off `recv` (a language-value field like `label`, or its native-state extern-handle field
/// as a [`NativeValue::Extern`]); it gets the same `&mut dyn Host` seam an [`ExtType`] method does.
/// The receiver crosses as a value snapshot; a method that **mutates the instance in place** returns
/// a [`NativeOut::InstanceUpdate`] write-set (boundary 1) — the backend applies it to the live
/// receiver's slots. Native-state mutation through an extern-handle field's interior mutability
/// (Rc/Arc-shared) is also visible without a write-set. Reading a field is served directly.
pub type FieldedDispatch = fn(
    recv: &NativeValue,
    method: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError>;

/// The pre-unification name for [`FieldedDispatch`]. A native **class** and a native **struct** now
/// share one fielded-type declaration ([`ExtFielded`]) and therefore one instance-method dispatch;
/// this alias keeps every `ClassDispatch` reference compiling unchanged.
pub type ClassDispatch = FieldedDispatch;

/// The **neutral** spelling of the shared native instance-method dispatch signature, used by BOTH
/// fielded types ([`ExtFielded`]) and enums ([`ExtEnum`], native-extensibility S1 / Slice B). It is
/// identical to [`FieldedDispatch`] — the whole point is **one** dispatch shape across kinds, so a
/// native enum method routes through the exact same seam a native class/struct method does. The one
/// representational difference is arg-IN: a fielded receiver crosses as a [`NativeValue::Instance`]
/// (fields by name), an enum receiver as a [`NativeValue::Variant`] (case + declaration index +
/// positional payload); the method reads whichever off `recv`. An enum is an **immutable value
/// type**, so a dispatch returning [`NativeOut::InstanceUpdate`] is a runtime error for an enum,
/// exactly as it is for a [`FieldedKind::Struct`].
pub type NativeMethodDispatch = FieldedDispatch;

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

/// A type's **call-site-typed** method dispatch (http arc H8) — the [`TypedDispatch`] twin for
/// extern-type methods: `resp.json::<User>()`. Like [`TypeDispatch`] plus the `recipe` the
/// checker resolved from the turbofish, so the method can build a value of the caller-named type.
///
/// The same contract as [`TypedDispatch`]: the returned [`NativeOut`] already carries its declared
/// wrapper (`Ok`/`Err`, `Some`/`None`); a `Plain` door signals unrecoverable failure through
/// `Err(StdError)`, a recoverable one never uses that channel.
pub type TypedTypeDispatch = fn(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
    recipe: &TypeRecipe,
) -> Result<NativeOut, StdError>;

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
    /// The type's **call-site-typed** method signatures (http arc H8) — the turbofish surface
    /// `resp.json::<User>()`, the extern-type analogue of [`ExtModule::typed_functions`]. Each
    /// must declare a [`RetTy::TypeArg`] return (its result is named by the `::<T>`), and calls
    /// route to [`ExtType::typed_dispatch`] with the checker-resolved [`TypeRecipe`].
    ///
    /// As on the module side, this table's names live in their **own space**: a name may appear in
    /// both `methods` and `typed_methods` (a dynamic `json(): dyn` alongside a typed
    /// `json::<T>(): T`), and is unique only within each table.
    pub typed_methods: &'static [ExtFn],
    /// The shared dispatch for [`ExtType::typed_methods`] (`None` when the table is empty).
    pub typed_dispatch: Option<TypedTypeDispatch>,
    /// Per-method **documentation prose** (docs-browser Arc 2): `(method_name, markdown)` pairs, the
    /// extern-type analogue of [`ExtModule::docs`]. Opt-in and sparse; keyed by name so it covers
    /// [`ExtType::methods`], [`ExtType::ctx_methods`], and [`ExtType::typed_methods`].
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
        typed_methods: &[],
        typed_dispatch: None,
        docs: &[],
    };
}

/// The **identity quartet** every native declaration shares (audit: it was copy-pasted across
/// [`ExtType`] / [`ExtEnum`] / [`ExtFielded`] / [`ExtTrait`]). A nominal type's identity is its
/// qualified `namespace.name`: the string the checker keys `Type::Named` / `symbols.*` on and the
/// runtime keys dispatch / `is` / `as` / `use`-projection on. [`NominalType::name`] is only the
/// short human-facing form. Implementing `name()` + `namespace()` yields both projections once.
pub trait NominalType {
    /// The **short display name** (`Uuid`, `SameSite`, `Handle`, `Widget`).
    fn name(&self) -> &str;
    /// The namespace this declaration lives under (`std.id`, `std.http`, `res`, `fx`).
    fn namespace(&self) -> &str;
    /// The **qualified identity** (`std.id.Uuid`) — `namespace.name`.
    fn qualified(&self) -> String {
        format!("{}.{}", self.namespace(), self.name())
    }
    /// Whether `q` **is** this declaration's qualified identity — [`NominalType::qualified`]
    /// equality without building the `String`. Registry lookups run this per candidate per probe,
    /// and the checker probes per imported-type annotation/member on the per-keystroke LSP path, so
    /// the comparison must not allocate (audit-3 Finding 12).
    fn is_qualified(&self, q: &str) -> bool {
        qualified_matches(self.namespace(), self.name(), q)
    }
}

/// The allocation-free `namespace.name == q` test, the single body every [`NominalType`] and the
/// projected [`Nominal`] share.
pub fn qualified_matches(namespace: &str, name: &str, q: &str) -> bool {
    q.len() == namespace.len() + 1 + name.len()
        && q.as_bytes()[namespace.len()] == b'.'
        && q.starts_with(namespace)
        && q.ends_with(name)
}

/// Which native declaration a projected [`Nominal`] came from — the lightweight discriminant the
/// `use`-projection paths carry instead of the concrete `&ExtType`/`&ExtEnum`/… . A fielded type
/// projects as [`NominalKind::Class`] or [`NominalKind::Struct`] off its [`ExtFielded::kind`], so
/// `classify_use` maps it to the right [`UseKind`] without a second lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NominalKind {
    Type,
    Enum,
    Class,
    Struct,
    Trait,
}

/// A **lightweight projection** of one native declaration's identity — its short `name` +
/// `namespace` (both `&'static`, borrowed from the declaration, so the stream allocates nothing)
/// plus which [`NominalKind`] it is. The single item type [`Registry::nominal_types`] yields, so
/// `namespace_types` / `classify_use` / `resolve_namespace_child` walk one stream instead of four
/// parallel per-kind loops. Identity probing reuses the shared allocation-free
/// [`Nominal::is_qualified`].
#[derive(Debug, Clone, Copy)]
pub struct Nominal {
    pub name: &'static str,
    pub namespace: &'static str,
    pub kind: NominalKind,
}

impl Nominal {
    /// The **qualified identity** (`namespace.name`) — built on demand (the projection stream never
    /// allocates; only a materialized output tuple does).
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.namespace, self.name)
    }
    /// Whether `q` is this projection's qualified identity — allocation-free, so the checker's
    /// per-keystroke `use` resolution stays alloc-free across the whole candidate stream.
    pub fn is_qualified(&self, q: &str) -> bool {
        qualified_matches(self.namespace, self.name, q)
    }
}

impl NominalType for ExtType {
    fn name(&self) -> &str {
        self.name
    }
    fn namespace(&self) -> &str {
        self.namespace
    }
}

// --- Native-declared enums (native-extensibility S1) ---------------------------------------------

/// A first-class language **enum** contributed by an extension (native-extensibility S1): a real
/// enum whose variants a user `match`es exhaustively (E0011), whose values a native fn/method
/// returns and receives as REAL enum values — not opaque handles ([`ExtType`]) and not string
/// shortcuts. Seeded **eagerly** at prelude time (unlike the lazily-resolved [`ExtType`]) because
/// exhaustiveness, member access, and construction all read the checker's symbol tables directly.
///
/// Its **identity** is the qualified `namespace.name` ([`ExtEnum::qualified`], like [`ExtType`]) —
/// what the checker keys `symbols.enums`/`Type::Named` on, so a native `std.http.SameSite` never
/// collides with a user's own `SameSite`. Its **runtime** name (what a materialized value and a
/// pattern's `type_name` carry) is the short [`ExtEnum::name`], which is the display form.
#[derive(Debug, Clone, Copy)]
pub struct ExtEnum {
    /// The **short display name** (`SameSite`). Identity is the qualified [`ExtEnum::qualified`].
    pub name: &'static str,
    /// The namespace this enum lives under (`std.http`) — its qualified identity is `namespace.name`,
    /// mirroring [`ExtType::namespace`].
    pub namespace: &'static str,
    /// The variants in **declaration order** — the order a derived `Comparable` uses and the index
    /// each variant's shape carries. A backed enum's variants are fieldless with a
    /// [`ExtVariant::value`]; an algebraic enum's carry [`ExtVariant::fields`].
    pub variants: &'static [ExtVariant],
    /// The scalar kind each variant's `.value()` yields for a **backed** enum (`enum SameSite:
    /// string`), or [`EnumBacking::None`] for a plain/algebraic enum. This states the RULE the
    /// checker enforces on the `.value()` accessor's type: a `String`-backed enum's `.value()` is
    /// `string`, an `Int`-backed one's is `int`, and a non-backed enum has no `.value()` at all.
    pub backing: EnumBacking,
    /// Instance-method signatures (native-extensibility S1 / Slice B) — the [`ExtEnum`] twin of
    /// [`ExtFielded::methods`], same vocabulary as an [`ExtType`]'s `methods`. A call `color.name()`
    /// on a native enum value routes to [`ExtEnum::dispatch`]; the checker types the call off these
    /// signatures. Default empty (a data-only enum, the S1 shape). A method name is disjoint from the
    /// built-in `value()`/`to_json` accessors and from a variant's case name.
    pub methods: &'static [ExtFn],
    /// The one shared instance-method dispatch (native-extensibility S1 / Slice B) — the [`ExtEnum`]
    /// twin of [`ExtFielded::dispatch`], reusing the neutral [`NativeMethodDispatch`] signature so a
    /// native enum method dispatches through the **same** seam a fielded method does. Receives the
    /// enum value marshalled to a [`NativeValue::Variant`] (its case + payload) plus the host seam;
    /// both backends route a native enum method call here after the user-proto and built-in
    /// `value()`/`to_json` paths miss. A data-only enum never reaches it, so the default reports an
    /// unregistered-method misuse. An enum is an immutable value type: a dispatch returning
    /// [`NativeOut::InstanceUpdate`] is a runtime error (mirrors the [`FieldedKind::Struct`] guard).
    pub dispatch: NativeMethodDispatch,
    /// The **traits this enum declares** (native-extensibility S3 / Slice C) — the [`ExtEnum`] twin
    /// of [`ExtFielded::traits`] and [`ExtType::traits`]. A name matching a native [`ExtTrait`] makes
    /// the enum satisfy that trait: `seed_ext_traits` records it into
    /// `user_trait_impls[qualified][trait]`, so a native enum value coerces to `dyn Trait` and its
    /// trait-method call dispatches to the enum's native method (via `call_native_enum_method`). A
    /// built-in name (e.g. `"Comparable"`) is picked up by `seed_native_builtin_traits`
    /// (`record_trait_impls` filters to built-in names). Default empty.
    pub traits: &'static [&'static str],
    /// The **built-in directives** this enum carries (native type-declaration unification, Slice D) —
    /// the [`ExtEnum`] twin of [`ExtFielded::directives`], the same crosscutting channel. The only
    /// directive legal on an enum is [`ExtTypeDirective::Semantic`] (marking its fieldless variants as
    /// role names → `semantic_enums`); [`Registry::validate`] refuses a struct/class-only directive
    /// here. Default empty.
    pub directives: &'static [ExtTypeDirective],
}

/// One variant of an [`ExtEnum`]: its case name plus **either** a positional payload (an algebraic
/// variant, `Tagged(name: string)`) **or** a backing constant (a backed variant, `Pending =
/// "pending"`) — never both, mirroring a `.noe` enum.
#[derive(Debug, Clone, Copy)]
pub struct ExtVariant {
    /// The variant's case name (`Lax`, `Tagged`) — matched by a pattern and stamped on the value.
    pub name: &'static str,
    /// The variant's positional payload types (empty for a fieldless or backed variant), same
    /// signature vocabulary as an [`ExtFn`]'s parameters. Read when the checker binds a variant
    /// pattern's payloads and when a backend materializes/marshals the payload values.
    pub fields: &'static [SigType],
    /// The variant's **backing constant** for a backed enum (`= "pending"`), or
    /// [`VariantValue::None`] for a fieldless/algebraic variant. What `.value()` returns at runtime;
    /// its scalar kind must agree with the enum's [`ExtEnum::backing`].
    pub value: VariantValue,
}

impl ExtEnum {
    /// Literal-shortening defaults (`..ExtEnum::DEFAULTS`), mirroring [`ExtType::DEFAULTS`]: a plain
    /// (non-backed) enum names only its variants.
    pub const DEFAULTS: ExtEnum = ExtEnum {
        name: "",
        namespace: "std",
        variants: &[],
        backing: EnumBacking::None,
        methods: &[],
        dispatch: |_, method, _, _| {
            Err(StdError {
                kind: crate::ErrorKind::UnknownName,
                message: format!(
                    "internal: no enum-method dispatch registered (method `{method}`)"
                ),
            })
        },
        traits: &[],
        directives: &[],
    };

    /// The variant named `variant`, with its declaration index, if any.
    pub fn variant(&self, variant: &str) -> Option<(u32, &'static ExtVariant)> {
        self.variants
            .iter()
            .enumerate()
            .find(|(_, v)| v.name == variant)
            .map(|(i, v)| (i as u32, v))
    }
}

impl NominalType for ExtEnum {
    fn name(&self) -> &str {
        self.name
    }
    fn namespace(&self) -> &str {
        self.namespace
    }
}

/// The scalar kind a **backed** [`ExtEnum`]'s variants are backed by — what `.value()` yields.
/// [`EnumBacking::None`] is a plain/algebraic enum (no `.value()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnumBacking {
    None,
    Str,
    Int,
}

/// One backed variant's constant (`= "pending"` / `= 3`), or [`VariantValue::None`] for a
/// fieldless/algebraic variant. Read by both backends' `.value()` materialization.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VariantValue {
    None,
    Str(&'static str),
    Int(i64),
}

// --- Native-declared fielded types: classes + structs (native-extensibility S2, unified) --------

/// Whether a native [`ExtFielded`] type is a **class** (reference type) or a **struct** (value
/// type) — the one load-bearing bit that distinguishes the two. A class and a struct are the same
/// shape (named fields, methods, one dispatch); they differ only in semantics, and that difference
/// is derived from this discriminant everywhere:
///
/// - [`FieldedKind::Class`] — reference identity (two bindings alias, `==` is identity), full RC +
///   cycle participation, native state + a **destructor** (an extern-handle field's `Drop`), and
///   in-place mutation ([`NativeOut::InstanceUpdate`]). Seeded as `TypeKind::Class`; materialized
///   with a `class`-kind shape (`structural_eq = false`).
/// - [`FieldedKind::Struct`] — a **value** type: structural equality (`==` compares fields),
///   copy-on-assign, no identity/destructor/cycle, and **no in-place mutation** (a method that
///   "mutates" returns a new value; a dispatch returning `InstanceUpdate` is a runtime error).
///   Seeded as `TypeKind::Struct`; materialized with a `struct`-kind shape (`structural_eq = true`).
///
/// `Class` is the default ([`ExtFielded::DEFAULTS`]) so every pre-unification `ExtClass` fixture
/// keeps its meaning unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldedKind {
    Class,
    Struct,
}

/// A **built-in directive** a native-declared fielded or enum type carries — the ABI twin of the
/// closed [`noeta_ast::BuiltinDirective`](https://docs.rs/noeta-ast) set that a `.noe` type gets from
/// its `Decorators`. Unlike an [`ExtDirective`] (the `@openapi`-style codegen-*expansion* hook, whose
/// meaning is the source it produces), these directives have **checker/backend semantics**: each
/// reduces to a single `Symbols` membership insert keyed by the declaring type, exactly mirroring the
/// `.noe` translations the checker's collect pass performs from a `Decorators`.
///
/// Carried on the crosscutting [`ExtFielded::directives`] / [`ExtEnum::directives`] channel — one
/// field per kind, never four parallel fields. The checker's `seed_ext_directives` pass translates
/// each variant into its table write, and [`Registry::validate`] enforces which (kind, directive)
/// pairs are legal at assembly (the native analogue of the AST placement gate E0054), since a native
/// type bypasses the source-level site check.
///
/// (`@role` shipped in Slice D3 as [`ExtTypeDirective::Role`]; `@packed` in Slice E1 as
/// [`ExtTypeDirective::Packed`].)
#[derive(Debug, Clone, Copy)]
pub enum ExtTypeDirective {
    /// `@validated` — bar bare literal / record-update construction of this type outside its own
    /// `impl` (the checker's **E0060** construction gate, keyed purely on membership in
    /// `validated_types`). Legal on a **struct or class**. Note this only installs the static
    /// construction ban: validation actually *runs* iff the type additionally advertises
    /// [`ExtFielded::traits`] `["Validate"]` (making it `satisfies(Validate)`, so a recipe door's
    /// materialization gains a validator) and carries a reachable `validate` method its dispatch
    /// answers — both of which ride the type's existing trait + method channels, not this directive.
    Validated,
    /// `@semantic` — mark this **enum**'s fieldless variants as role names (the checker's
    /// `semantic_enums` membership). Enum-only. A `@semantic` enum is the vocabulary a `@role` tag
    /// draws its variants from.
    Semantic,
    /// `@attribute` — mark this **fielded struct** usable as a `#[...]` data attribute, the native
    /// analogue of a `.noe` `@attribute struct`. Struct-only. Seeds the checker's `attributes` opt-in
    /// (E0029) keyed on the type's qualified identity, and — when the placement list is non-empty —
    /// its `attachable` restriction (E0030), exactly as a `.noe` `@attribute(Kind, …)` does. An empty
    /// slice is a bare `@attribute` (attachable anywhere). The struct's fields (already seeded by the
    /// fielded seeder) are its construction contract, so a native `@attribute` and a `.noe` one behave
    /// identically to every consumer, including reflection.
    Attribute(&'static [AttrTarget]),
    /// `@role(Enum.Variant)` — tag this **`@attribute` struct** with one or more semantic roles, the
    /// native analogue of a `.noe` `@role(Enum.Variant)` on an `@attribute` record. A role is a
    /// *facet* of an attribute: applying the role-bearing attribute to a declaration confers each
    /// tagged role on that declaration, surfaced by `roles_of()` as a `RoleBinding { target, role }`.
    /// Struct-only, and **only** on a type that also carries [`ExtTypeDirective::Attribute`] — the
    /// role has nothing to attach to otherwise. [`Registry::validate`] enforces the coupling at
    /// assembly (the native analogue of the checker's E0031 role rules): each [`ExtRoleTag::enum_name`]
    /// must resolve to a `@semantic` enum (a native enum carrying [`ExtTypeDirective::Semantic`], or
    /// the built-in `Semantic` prelude enum), and its named variant must exist and be **fieldless**.
    /// Unlike the other directives this seeds **no** `Symbols` table: a role is surfaced purely by
    /// `reflect::build` joining the tags against the in-program attribute applications, so
    /// [`Registry::native_roles`] projects the tags into the plain-data table that builder now accepts.
    Role(&'static [ExtRoleTag]),
    /// `@packed` — lay a `List` of this **value struct** out as one flat, contiguous raw-primitive
    /// buffer (native-extensibility Slice E1, the native twin of a `.noe` `@packed struct`). Struct-only.
    /// Seeds the checker's `packed_structs` membership (and `column_structs` for
    /// [`PackedLayoutKind::Column`]) keyed on the type's **qualified** identity, so a source `List<Pt>`
    /// literal (`Pt` native) hits `note_packed_list` → `packed_layout` and packs flat on both backends,
    /// exactly like a `.noe` `@packed` struct's list. A *single* `@packed` value is always boxed (flat
    /// storage is a property of the *list*, keyed by construction-site span), so a native constructor
    /// returning a `NativeOut::Instance` yields the same boxed `Object` a source `Pt{..}` literal does.
    /// [`Registry::validate`] enforces the native analogue of the checker's **E0038** all-packable-field
    /// rule: every [`ExtField::ty`] must be [`SigType::Int`]/[`SigType::Float`]/[`SigType::F32`]/
    /// [`SigType::Bool`], or a [`SigType::Named`] resolving to another `@packed` struct in the same unit
    /// set — anything heap-shaped (a `string`/`List`/class/enum/`dyn`) refuses to assemble.
    Packed(PackedLayoutKind),
}

/// The storage **axis** a native [`ExtTypeDirective::Packed`] struct's list takes — the ABI twin of a
/// `.noe` `@packed(Layout.Row|Column)`. A local [`Copy`] enum (mirroring [`FieldedKind`]), *not* the
/// [`ConstraintLayout`] a [`PackedConstraint`] carries: that has a meaningless-here `Any` arm (a bundle
/// accepts either layout), whereas a declaration commits to exactly one. The per-field packed *kinds*
/// are derived from the struct's [`ExtField`] primitive types at seed time; this payload carries only
/// the row-vs-column axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackedLayoutKind {
    /// Row-major (array-of-structs): each element's fields are contiguous, elements back to back — the
    /// default, matching a bare `.noe` `@packed`.
    Row,
    /// Column-major (struct-of-arrays): each field's values are contiguous across all elements — the
    /// `.noe` `@packed(Layout.Column)` layout, seeding `column_structs` in addition to `packed_structs`.
    Column,
}

/// One `@role(Enum.Variant)` tag on a native `@attribute` struct — the ABI mirror of the AST
/// `RoleTag { enum_name, variant }`. Both name a fieldless variant of a `@semantic` enum by the
/// identity a role query and a materialized `role` value carry: the enum's qualified identity for a
/// native `@semantic` enum, or the bare `"Semantic"` for the built-in prelude enum. Carried inside
/// [`ExtTypeDirective::Role`]; [`Registry::validate`] resolves and checks each at assembly, and
/// [`Registry::native_roles`] projects it into `reflect::build`'s native-role table.
#[derive(Debug, Clone, Copy)]
pub struct ExtRoleTag {
    /// The role's `@semantic` enum, by the identity a `roles_of::<Enum>()` filter and the
    /// materialized `RoleBinding.role` value carry — a native `@semantic` enum's **qualified**
    /// identity (`cfg.Stage`), or the bare `"Semantic"` for the built-in prelude enum.
    pub enum_name: &'static str,
    /// The role's variant (`EntryPoint`) — must exist on `enum_name` and be fieldless.
    pub variant: &'static str,
}

/// Where a native `@attribute` fielded struct may be **applied** — the ABI mirror of the checker's
/// `TargetKind`, carried in [`ExtTypeDirective::Attribute`]. A `.noe` `@attribute(Method, Function)`
/// lists these by name; a native declaration lists them as this closed enum. The checker maps each to
/// its `TargetKind` when seeding the placement gate (E0030).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrTarget {
    Struct,
    Class,
    Enum,
    Function,
    Method,
    Field,
    Variant,
    Param,
}

/// A first-class **fielded type** contributed by an extension (native-extensibility S2, unified): a
/// real language type with language-visible named fields the language reads, mutates (class only),
/// and constructs. One struct with a [`FieldedKind`] discriminant covers both a reference **class**
/// and a value **struct** — see [`FieldedKind`] for the semantic split. Unlike an [`ExtType`] (an
/// opaque handle), a fielded type exposes named fields.
///
/// **Representation:** a native fielded value is a real language `Object`. A `Class`-kind value gets
/// a class-kind shape — identity, reference semantics, RC, and cycle participation from the object
/// model unchanged; its native state + destructor ride on a **field typed as an extern handle** (an
/// [`ExtType`] whose Rust `Drop` is the cleanup). A `Struct`-kind value gets a struct-kind shape, so
/// the object model derives `structural_eq` and value semantics automatically.
///
/// Its **identity** is the qualified `namespace.name` ([`NominalType::qualified`], like [`ExtType`]
/// / [`ExtEnum`]) — what the checker keys `symbols.records`/`Type::Named` on, so a native
/// `res.Handle` never collides with a user's own `Handle`. Its **runtime** shape carries the short
/// [`ExtFielded::name`] (the display form and what a constructed/materialized value stamps).
///
/// Authors declare a class through [`Extension::classes`] and a struct through
/// [`Extension::structs`]; both produce this shared type, distinguished by [`ExtFielded::kind`].
/// The convenience aliases [`ExtClass`] (defaults to `Class`) and [`ExtStruct`] name it at each
/// hook, and [`ExtFielded::DEFAULTS`] / [`ExtFielded::STRUCT_DEFAULTS`] fill the respective kind.
#[derive(Debug, Clone, Copy)]
pub struct ExtFielded {
    /// The **short display name** (`Handle`, `Point`). Identity is [`NominalType::qualified`].
    pub name: &'static str,
    /// The namespace this type lives under (`res`) — its qualified identity is `namespace.name`,
    /// mirroring [`ExtType::namespace`] / [`ExtEnum::namespace`].
    pub namespace: &'static str,
    /// The type's fields in **declaration (slot) order** — the order a native constructor supplies
    /// values in ([`NativeOut::Instance`]) and the order the object's slots take. Each states its
    /// name, type, visibility, and mutability; the checker seeds them into `symbols.records`
    /// (types), `symbols.private_fields` (visibility), and `symbols.mut_fields` (mutability).
    pub fields: &'static [ExtField],
    /// Instance-method signatures (native-extensibility S3 / Pass 2a) — same vocabulary as an
    /// [`ExtType`]'s `methods`. A call `h.describe()` on a native fielded instance routes to
    /// [`ExtFielded::dispatch`]; the checker types the call off these signatures. Default empty (a
    /// fields-only type). Names are disjoint from the type's field names (a method wins over a
    /// field, the checker's rule).
    pub methods: &'static [ExtFn],
    /// The one shared instance-method dispatch (native-extensibility S3 / Pass 2a) — the
    /// [`ExtFielded`] twin of [`ExtType::dispatch`]. Receives the instance marshalled to a
    /// [`NativeValue::Instance`] plus the host seam; both backends route a fielded object's method
    /// call here (their `CallMethod` Object arm). A fields-only type never reaches it, so the
    /// default reports an unregistered-method misuse. A `Struct`-kind dispatch that returns
    /// [`NativeOut::InstanceUpdate`] is a runtime error (value types have no in-place mutation).
    pub dispatch: FieldedDispatch,
    /// The **traits this type declares** (native-extensibility S3 / Pass 2b) — the [`ExtFielded`]
    /// twin of [`ExtType::traits`]. A name matching a native [`ExtTrait`] makes the type satisfy
    /// that trait: `seed_ext_traits` records it into `user_trait_impls[qualified][trait]`, so a
    /// native fielded value coerces to `dyn Trait` and its trait-method call dispatches to the
    /// type's native method (the Pass-2a Object-arm branch). Default empty.
    pub traits: &'static [&'static str],
    /// Whether this is a reference **class** or a value **struct** — the semantic discriminant. See
    /// [`FieldedKind`]. Defaults to [`FieldedKind::Class`] so pre-unification `ExtClass` fixtures
    /// keep their meaning; a struct sets it via [`ExtFielded::STRUCT_DEFAULTS`].
    pub kind: FieldedKind,
    /// The **built-in directives** this type carries (native type-declaration unification, Slice D) —
    /// the `.noe` `Decorators` twin, uniform across native fielded + enum kinds. `seed_ext_directives`
    /// translates each into its `Symbols` insert (e.g. [`ExtTypeDirective::Validated`] →
    /// `validated_types`); [`Registry::validate`] rejects a directive illegal for this type's
    /// [`FieldedKind`] (`@semantic` is enum-only, so it is refused here). Default empty.
    pub directives: &'static [ExtTypeDirective],
}

/// The pre-unification name for [`ExtFielded`], defaulting (via [`ExtFielded::DEFAULTS`]) to a
/// [`FieldedKind::Class`]. Keeps every `ExtClass { .. }` / `ExtClass::DEFAULTS` fixture compiling
/// unchanged, and reads correctly at the [`Extension::classes`] hook.
pub type ExtClass = ExtFielded;

/// A [`FieldedKind::Struct`]-flavoured spelling of [`ExtFielded`] for the [`Extension::structs`]
/// hook. It is the same type; a struct fixture fills its discriminant with
/// `..ExtStruct::STRUCT_DEFAULTS`.
pub type ExtStruct = ExtFielded;

/// One field of an [`ExtFielded`]: its name, type, and the two access rules (visibility, mutability)
/// the checker enforces. The native-state/destructor field is an ordinary field whose `ty` names an
/// extern handle ([`SigType::Named`] of an [`ExtType`]).
#[derive(Debug, Clone, Copy)]
pub struct ExtField {
    /// The field's name (`label`, `guard`) — how the language reads it (`h.label`) and how a native
    /// constructor keys its value in [`NativeOut::Instance`].
    pub name: &'static str,
    /// The field's declared type, same signature vocabulary as an [`ExtFn`]'s parameters. Seeded
    /// into `symbols.records` (via `sig_to_type`) so `h.field` types correctly.
    pub ty: SigType,
    /// Whether the field is **public** — readable/writable from outside the class. A `false` field
    /// is private: an access from outside is E0035, exactly like a `.noe` class's default-private
    /// field. Seeded into `symbols.private_fields` (the enforcer).
    pub is_public: bool,
    /// Whether the field is **mutable** — assignable after construction (`h.field = v`). A `false`
    /// field is read-only: an assignment is E0033. Seeded into `symbols.mut_fields` (the enforcer).
    pub is_mut: bool,
}

impl ExtFielded {
    /// Literal-shortening defaults (`..ExtClass::DEFAULTS`), mirroring [`ExtEnum::DEFAULTS`]: a
    /// fieldless, method-less **class** under `std`. `Class` is the default kind, so a pre-unification
    /// `ExtClass { .. ..ExtClass::DEFAULTS }` fixture keeps its exact meaning.
    pub const DEFAULTS: ExtFielded = ExtFielded {
        name: "",
        namespace: "std",
        fields: &[],
        methods: &[],
        dispatch: |_, method, _, _| {
            Err(StdError {
                kind: crate::ErrorKind::UnknownName,
                message: format!(
                    "internal: no fielded-method dispatch registered (method `{method}`)"
                ),
            })
        },
        traits: &[],
        kind: FieldedKind::Class,
        directives: &[],
    };

    /// Literal-shortening defaults for a value **struct** (`..ExtStruct::STRUCT_DEFAULTS`) — the
    /// [`ExtFielded::DEFAULTS`] shape with [`FieldedKind::Struct`], the one bit that flips reference
    /// semantics to value semantics (structural equality, copy-on-assign, no in-place mutation).
    pub const STRUCT_DEFAULTS: ExtFielded = ExtFielded {
        kind: FieldedKind::Struct,
        ..ExtFielded::DEFAULTS
    };
}

impl NominalType for ExtFielded {
    fn name(&self) -> &str {
        self.name
    }
    fn namespace(&self) -> &str {
        self.namespace
    }
}

// --- Native-declared traits (native-extensibility S3) --------------------------------------------

/// A first-class language **trait** contributed by an extension (native-extensibility S3). Two
/// capabilities in one declaration:
///
/// - **A contract for user types (3a):** a program writes `impl NativeTrait for MyType { ... }`,
///   binds on it (`fn f<T: NativeTrait>(x: T)`), and an incomplete impl is **E0015** — exactly as
///   for a `.noe` `trait`. The checker seeds this declaration into its **user-trait** machinery
///   (`symbols.user_traits` / `user_trait_impls`; `satisfies_user_trait`,
///   `enforce_type_param_bounds`, `check_user_trait_impl`), NOT the closed `BuiltinTrait` enum — a
///   native trait is indistinguishable from a `.noe` one to every downstream consumer.
///
/// - **Dynamic dispatch over native values (3b):** a native value (an [`ExtType`] instance)
///   laundered through `dyn NativeTrait`, calling a trait method, dispatches to the **native**
///   method — the same extern-method seam a directly-typed extern value uses (`resp.json()`), so no
///   new runtime plumbing. A native type advertises that it implements the trait through its
///   existing [`ExtType::traits`] list (a non-built-in name there is matched against a native trait
///   and seeded into `user_trait_impls`); the trait methods are declared as the type's ordinary
///   [`ExtType::methods`] and answered by its `dispatch`.
///
/// Its **identity** is the qualified `namespace.name` ([`ExtTrait::qualified`], like [`ExtType`] /
/// [`ExtEnum`] / [`ExtClass`]) for the `use`-projection + namespace re-rooting; the user-trait
/// tables are keyed by the imported **short** name (the vocabulary `impl`/bound sites are written
/// in, exactly like a `.noe` trait and the built-in traits), resolved through the `use`-import
/// alias.
#[derive(Debug, Clone, Copy)]
pub struct ExtTrait {
    /// The **short display name** (`Widget`). Identity is the qualified [`ExtTrait::qualified`].
    pub name: &'static str,
    /// The namespace this trait lives under (`fx`) — its qualified identity is `namespace.name`,
    /// mirroring [`ExtType::namespace`] / [`ExtEnum::namespace`] / [`ExtClass::namespace`].
    pub namespace: &'static str,
    /// The trait's method contract, in declaration order. A **required** method
    /// ([`ExtTraitMethod::has_default`] `false`) must be present in an `impl` or it is E0015; a
    /// default-carrying one is optional for an implementor.
    pub methods: &'static [ExtTraitMethod],
}

/// One method in an [`ExtTrait`]: an ordinary [`ExtFn`] signature (the receiver is `self`, not in
/// `params`) plus whether it carries a **default** — the ABI twin of the AST `TraitMethod { sig,
/// has_default }`. A required method's implementor must provide it (E0015); a defaulted one is
/// optional. (Default *bodies* are a later slice: `has_default` marks the method optional for the
/// contract check; a native trait's defaults are answered by the implementing native type's
/// dispatch, not a hoisted `.noe` body.)
#[derive(Debug, Clone, Copy)]
pub struct ExtTraitMethod {
    /// The method's signature (name, parameter types, return) — same vocabulary as any [`ExtFn`].
    pub sig: ExtFn,
    /// Whether the method carries a default (optional for an implementor); `false` for a required
    /// method whose absence in an `impl` is E0015.
    pub has_default: bool,
}

impl ExtTrait {
    /// Literal-shortening defaults (`..ExtTrait::DEFAULTS`), mirroring [`ExtClass::DEFAULTS`]: a
    /// method-less trait under `std`.
    pub const DEFAULTS: ExtTrait = ExtTrait {
        name: "",
        namespace: "std",
        methods: &[],
    };
}

impl NominalType for ExtTrait {
    fn name(&self) -> &str {
        self.name
    }
    fn namespace(&self) -> &str {
        self.namespace
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
    /// Required field kinds. Interpreted per [`arity`](Self::arity): under [`ConstraintArity::Exact`]
    /// these are the fields in slot (declared) order, exactly; under [`ConstraintArity::Uniform`]
    /// only `fields[0]` matters — every field of the bound type must equal it.
    pub fields: &'static [ConstraintField],
    /// Required storage layout (`Any` for layout-agnostic kernels that branch on
    /// `PackedView::column` themselves).
    pub layout: ConstraintLayout,
    /// How [`fields`](Self::fields) is matched against the bound type's field list.
    pub arity: ConstraintArity,
}

/// How a [`PackedConstraint`]'s field list is matched against a bound type (kernel-methods; extended
/// by the array-ops integer/u8 vector shapes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintArity {
    /// The bound type's fields must equal `fields` exactly — same count, same kinds, in order (a
    /// fixed-shape kernel: an f32 `Vec3`, a 4×`u8` `Color`).
    Exact,
    /// The bound type has **at least `min`** fields, **all** of the single kind in `fields[0]` — a
    /// uniform primitive vector of flexible width (`IVec2`/`IVec3`/… all bind one integer bundle).
    /// `fields` must hold exactly one kind.
    Uniform { min: usize },
}

/// One required field kind in a [`PackedConstraint`] (primitives only — a bundle over nested
/// packed structs is a later, additive extension).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintField {
    Int,
    Float,
    F32,
    Bool,
    /// A fixed-width integer field `i8..i64`/`u8..u64` (packed-widths arc) — the array-ops integer
    /// (`IVec2`/`IVec3` at `i32`) and `Color` (`u8`) vector shapes. Mirrors
    /// [`crate::PackedField::IntN`] / `noeta_ast::reflect::PackedKind::IntN`.
    IntN {
        bits: u8,
        signed: bool,
    },
    /// A **uniform numeric field of any (kind, width, signedness)** — `int`, `float`, `f32`, `f64`,
    /// and every `i8..i64`/`u8..u64`, but never `bool` or a nested packed struct. The generalized
    /// form that lets ONE bundle bind every numeric vector width at once (the three fixed
    /// `vec.Kernels`/`IntKernels`/`ColorKernels` copies collapse to one). Paired with a
    /// [`ConstraintArity::Uniform`], the checker captures the bound shape's concrete element type so
    /// it can resolve the element-relative returns [`RetTy::Elem`]/[`RetTy::ElemWide`]/
    /// [`RetTy::ElemFloat`]. The specific forms above stay exact — this is additive.
    AnyNumeric,
    /// A **uniform *integer* field of any width/signedness** — `int` and every `i8..i64`/`u8..u64`,
    /// but never `float`/`f32`/`f64`/`bool`. The [`AnyNumeric`](Self::AnyNumeric) sibling restricted
    /// to integers: the constraint of the **saturating** bundle (`vec.SatKernels`), where clamping to
    /// the type's bounds is only meaningful for integers (a float "saturates" in the IEEE sense — it
    /// is just plain arithmetic — so a float vector is rejected at the impl site). Like `AnyNumeric`
    /// it captures the bound shape's concrete element type for the element-relative returns.
    AnyInteger,
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

/// An extension-declared **attribute** — the extension counterpart of an `@attribute` struct
/// (tier-extensions port). An attribute *is* a struct, so it carries a `namespace` and projects
/// through the one [`Registry::nominal_types`] stream as a [`NominalKind::Struct`] nominal exactly
/// like any native fielded type: a consumer resolves `use std.test.{Skip}` / `use std.test` /
/// `#[std.test.Skip]` through the same `classify_use`/`namespace_types` machinery, and the checker
/// keys `symbols.attributes` on the [`ExtAttribute::qualified`] identity (D2). There is no global
/// attribute namespace — std's tier attributes live under `std.test` (`Skip`/`Name`/`Group`/`Data`),
/// `std.bench` (`Bench`), and `std.doc` (`Doc`), imported like any attribute.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtAttribute {
    pub name: &'static str,
    /// The namespace this attribute lives under (`std.test`) — its qualified identity is
    /// `namespace.name`, mirroring [`ExtType::namespace`] / [`ExtFielded::namespace`].
    pub namespace: &'static str,
    pub fields: &'static [ExtAttrField],
}

impl ExtAttribute {
    /// The **qualified identity** (`std.test.Skip`) — `namespace.name`, the string the checker keys
    /// `symbols.attributes`/`records` on and reflection carries. Mirrors [`ExtType::qualified`].
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.namespace, self.name)
    }
    /// Whether `q` is this attribute's qualified identity — allocation-free, mirroring
    /// [`Nominal::is_qualified`].
    pub fn is_qualified(&self, q: &str) -> bool {
        qualified_matches(self.namespace, self.name, q)
    }
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
    /// A data type declaration — `struct`, `class`, or `enum` (`@doc { … } struct Point`).
    Type,
    /// A `trait` declaration (`@doc { … } trait Shape`).
    ///
    /// Separate from [`Type`](Self::Type) because the language draws the line: a trait is a
    /// contract, not a data type, and the type directives are all rejected on one. Documentation,
    /// though, attaches to a trait perfectly well — and could not say so until this variant
    /// existed, which is why a `@doc` above a trait silently became the *module* doc.
    Trait,
}

/// An extension-declared **dev-tier** — the extension counterpart of a program's `@tier`
/// declaration. std ships the built-in four (`test`/`bench`/`doc`/`debug`); the tier name-space
/// the checker validates against is the installed extensions' tiers ∪ the program's own `@tier`
/// declarations. The built-ins' runners stay native (`noeta test`/`bench`/`doc` and `--tier
/// debug`); only the declaration lives here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtTier {
    pub name: &'static str,
    /// Which declarations this tier may **attach** to — as an annotation (`@test fn foo()`) or by
    /// adjacency (`@doc { … } struct Point`).
    ///
    /// **Empty ⇒ attaches to nothing**: a pure block tier, whose `@<name> { … }` stands alone in
    /// statement position (`debug`) or expression position (`json`) and decorates no declaration.
    /// It does *not* mean "unrestricted" — that reading made the field unenforceable, because the
    /// two tiers that attach to nothing and the tiers that attach to everything were spelled the
    /// same way, so no gate could act on either without breaking the other.
    ///
    /// A tier may still have a block form *and* attach: `test` is both `@test { … }` and
    /// `@test fn foo()`. The set constrains the attaching form only.
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

/// An extension-declared **`@`-directive** — a name an extension adds to the decorator
/// name-space, so `@openapi("petstore.yaml")` on a declaration is a directive the compiler knows
/// rather than a syntax error.
///
/// The built-in directives stay a closed enum: they have *semantics* in the checker and the
/// backends, which an extension cannot contribute. What an extension contributes is a **codegen
/// directive** — the name, where it may sit, what arguments it takes, the prose the editor shows,
/// and optionally an [`expand`](Self::expand) hook that synthesizes members of the declaration it
/// is attached to.
///
/// `@` is the codegen half of the language's decorator split (see the Attributes and Reflection
/// reference): `@…` runs at **compile time** and its meaning is the code it produces, while
/// `#[…]` data attributes are the runtime-readable half, reached through `attributes_of::<T>()`.
/// A directive is therefore *not* readable at runtime, by design — an extension that wants
/// runtime-visible metadata declares an attribute instead, and one that wants to consume an
/// external resource dynamically returns an invocable value rather than reaching for a directive.
///
/// Sites use [`TierSite`], the vocabulary this ABI already owns; the checker widens it into its
/// own finer-grained site model (which distinguishes `struct` from `class` from `enum` in ways a
/// three-variant enum cannot). **Empty ⇒ attaches to nothing**, matching [`ExtTier::sites`] — the
/// same polarity, for the same reason: a directive that genuinely goes everywhere can list the
/// sites it goes to, but one that decorates nothing has no other way to say so.
#[derive(Debug, Clone, Copy)]
pub struct ExtDirective {
    /// The name programs write after `@`. Resolved *after* the built-in directives and the tier
    /// name-space, so an extension can never shadow either.
    pub name: &'static str,
    /// Which declarations it may attach to. **Empty ⇒ attaches to nothing** — the same meaning as
    /// [`ExtTier::sites`], so one gate serves both. A directive that genuinely goes anywhere lists
    /// the sites it goes to.
    pub sites: &'static [TierSite],
    /// Maximum positional arguments; `None` is variadic, `Some(0)` takes none.
    pub max_args: Option<usize>,
    /// Named-argument keys it understands (`version:`). Empty ⇒ named arguments are rejected.
    pub named_keys: &'static [&'static str],
    /// The one-line usage shown beside the name in completion.
    pub detail: &'static str,
    /// Prose shown on hover.
    pub doc: &'static str,
    /// Signature-help parameter names, in order.
    pub params: &'static [&'static str],
    /// **Compile-time expansion**: synthesize members of the declaration this directive is attached
    /// to. `None` for a directive that only marks and validates.
    ///
    /// Returns **Noeta source** for the members — not AST. That keeps this ABI free of a
    /// `noeta-ast` dependency, routes generated code through the real parser so it earns the same
    /// diagnostics as hand-written code, and leaves the output inspectable (`noeta expand` prints
    /// it). An `Err(message)` is reported as a diagnostic at the directive's span.
    ///
    /// What it may emit follows from *where it attached* — members of the decorated declaration,
    /// exactly as `@derive` synthesizes methods onto a type. There is no separate notion of output
    /// scope: [`Self::sites`] already answers it.
    ///
    /// Runs only after the directive has passed the shared placement gate and its declared
    /// argument contract ([`Self::max_args`] / [`Self::named_keys`]), so a hook never sees a
    /// misplaced or malformed invocation.
    ///
    /// It may read the filesystem — the package's `[trust]` grant is the authorization — but must
    /// be a **pure function of [`DirectiveCtx`] plus the files it reports in [`Expansion::reads`]**.
    /// The compiler memoizes the result and re-runs it only when one of those inputs changes, so a
    /// hook that consults anything it does not report (the clock, the environment, a file it kept
    /// quiet about) will serve stale members until something unrelated invalidates it.
    pub expand: Option<DirectiveExpand>,
}

/// An [`ExtDirective::expand`] hook: the decorated declaration in, an [`Expansion`] out, or an
/// [`ExpansionError`] to report at the directive's span.
pub type DirectiveExpand = fn(&DirectiveCtx) -> Result<Expansion, ExpansionError>;

/// What an expansion produced: the members, and the files it took them from.
#[derive(Debug, Clone, Default)]
pub struct Expansion {
    /// **Noeta source** for the members of the decorated declaration.
    pub source: String,
    /// Every file the hook read, absolute or relative to [`DirectiveCtx::source_dir`].
    ///
    /// This is the hook's **incrementality contract**, and it is why the compiler does not simply
    /// hand over the contents of the path in the directive's arguments: a spec routinely pulls in
    /// further files (an OpenAPI `$ref` into a sibling document), and only the hook knows which.
    /// The compiler tracks each path it is given, so editing any of them re-runs the expansion and
    /// everything downstream of it.
    ///
    /// A hook that under-reports reads a file whose edits will not be noticed — the expansion goes
    /// stale until something else invalidates it. Report every file opened, including ones that
    /// turned out to be missing or empty (their *appearing* later is a change too).
    pub reads: Vec<String>,
}

/// Why an expansion failed — **and what it read on the way to failing.**
///
/// The reads matter on the error path more than on the success path, not less: the commonest
/// failure is a spec that is not there *yet*, and the incrementality contract only closes if the
/// compiler is told which path was consulted, so that *creating* the file re-runs the hook. A
/// `Result<Expansion, String>` could not carry that — the reads lived only in the `Ok` — so this
/// type gives the error the same `reads` channel the success has.
///
/// A hook with nothing to report converts a bare message: `return Err("no paths".into())` leaves
/// `reads` empty. A hook that opened a file before failing builds the struct so the file is still
/// watched.
#[derive(Debug, Clone, Default)]
pub struct ExpansionError {
    /// The message reported at the directive's span (as E0062).
    pub message: String,
    /// Every file the hook read before it failed — the same contract as [`Expansion::reads`], and
    /// the reason a missing file that later appears re-triggers the expansion instead of staying
    /// stale.
    pub reads: Vec<String>,
}

impl From<String> for ExpansionError {
    fn from(message: String) -> Self {
        Self {
            message,
            reads: Vec::new(),
        }
    }
}

impl From<&str> for ExpansionError {
    fn from(message: &str) -> Self {
        Self {
            message: message.to_string(),
            reads: Vec::new(),
        }
    }
}

/// Two directives are equal when they **declare** the same thing. `expand` is excluded: a function
/// pointer's address is not a meaningful identity (two identical hooks may compare unequal, and
/// distinct ones equal), so comparing it would make equality unreliable rather than more precise.
/// A directive is identified by what it says it is.
impl PartialEq for ExtDirective {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.sites == other.sites
            && self.max_args == other.max_args
            && self.named_keys == other.named_keys
            && self.detail == other.detail
            && self.doc == other.doc
            && self.params == other.params
            && self.expand.is_some() == other.expand.is_some()
    }
}

/// What an [`ExtDirective::expand`] hook is given: the invocation, and the declaration it decorates.
///
/// Deliberately narrow. A hook receives what the directive was written with and what it was written
/// on — not the surrounding program — so its output depends only on inputs the compiler can key a
/// memoized result on.
#[derive(Debug, Clone)]
pub struct DirectiveCtx {
    /// Positional arguments, already checked against [`ExtDirective::max_args`], rendered as source
    /// text (a string literal arrives without its quotes).
    pub args: Vec<String>,
    /// Named arguments, already checked against [`ExtDirective::named_keys`], in written order.
    pub named: Vec<(String, String)>,
    /// The decorated declaration's name — the type or function the synthesized members join.
    pub target: String,
    /// Which kind of declaration that is, so one hook can serve several sites.
    pub site: TierSite,
    /// The directory of the source file the directive was written in, so a relative path argument
    /// (`"petstore.yaml"`) resolves against the file rather than the process's working directory.
    pub source_dir: String,
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
    /// The extension's first-class **enums** (native-extensibility S1) — real language enums
    /// (exhaustively matchable, returned/received as values). Default empty; seeded eagerly into
    /// the checker's symbol tables at prelude time by qualified identity.
    fn enums(&self) -> &'static [ExtEnum] {
        &[]
    }
    /// The extension's first-class **classes** (native-extensibility S2) — real reference-type
    /// language classes (identity, destructor, fields, cycle participation). Default empty; seeded
    /// eagerly into the checker's symbol tables at prelude time by qualified identity. Every entry
    /// must be [`FieldedKind::Class`] (checked by [`Registry::validate`]).
    fn classes(&self) -> &'static [ExtClass] {
        &[]
    }
    /// The extension's first-class **structs** (native-extensibility, fielded unification) — real
    /// value-type language structs (structural equality, copy-on-assign, source-constructible,
    /// no identity/destructor). The value-semantics twin of [`Extension::classes`]; both hooks
    /// produce the shared [`ExtFielded`] type, distinguished by [`ExtFielded::kind`]. Default empty;
    /// seeded eagerly alongside classes at prelude time. Every entry must be [`FieldedKind::Struct`]
    /// (checked by [`Registry::validate`]).
    fn structs(&self) -> &'static [ExtStruct] {
        &[]
    }
    /// The extension's first-class **traits** (native-extensibility S3) — real language traits that
    /// user types `impl`/bound on (3a) and that dispatch dynamically over native values (3b).
    /// Default empty; seeded eagerly into the checker's user-trait tables at prelude time.
    fn traits(&self) -> &'static [ExtTrait] {
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
    /// The extension's declared **`@`-directives** (see [`ExtDirective`]). Default empty.
    fn directives(&self) -> &'static [ExtDirective] {
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
    /// A registered native **enum** (`use std.http.SameSite`, native-extensibility S1) — qualified
    /// identity. Bound like an [`UseKind::ExternType`]: the checker maps the local name to the
    /// qualified identity (so annotations/patterns resolve); the backends bind no runtime value
    /// (a native enum's values arrive from native calls, not a source-level type handle).
    ExtEnum(String),
    /// A registered native **class** (`use res.Handle`, native-extensibility S2) — qualified
    /// identity. Bound like an [`UseKind::ExternType`] in the checker (the local name maps to the
    /// qualified identity so annotations/construction resolve); the backends bind a **constructible**
    /// class-kind type handle under the imported short name so `Handle { ... }` builds a real class.
    ExtClass(String),
    /// A registered native **struct** (`use pkg.Point`, fielded unification) — qualified identity.
    /// The value-type twin of [`UseKind::ExtClass`]: the checker maps the local name to the qualified
    /// identity (so annotations/construction resolve); the backends bind a **constructible**
    /// struct-kind type handle so `Point { .. }` builds a real value struct (structural equality,
    /// copy-on-assign) rather than a reference class.
    ExtStruct(String),
    /// A registered native **trait** (`use fx.Widget`, native-extensibility S3) — qualified
    /// identity. The checker maps the local (imported short) name to the qualified identity and
    /// seeds the user-trait tables under the short name, so `impl Widget for T`, `T: Widget`
    /// bounds, and `dyn Widget` resolve; the backends bind no runtime value (a native trait is a
    /// contract + a dynamic-dispatch surface, not a source-level value handle).
    ExtTrait(String),
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
                || e.enums().iter().any(|t| t.qualified().starts_with(&dotted))
                // A namespace may contain *only* attributes (`std.test`, `std.bench`, `std.doc`),
                // so they count toward recognizing it as navigable — else `use std.test` would be
                // rejected and the F5 Module-arm projection never runs.
                || e.attributes().iter().any(|a| a.qualified().starts_with(&dotted))
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
            for t in e.enums() {
                if let Some(rest) = t.qualified().strip_prefix(&dotted) {
                    push_seg(rest, &mut out);
                }
            }
            for a in e.attributes() {
                if let Some(rest) = a.qualified().strip_prefix(&dotted) {
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
            for t in e.enums() {
                if let Some(rest) = t.qualified().strip_prefix(&dotted) {
                    push(rest, &mut out);
                }
            }
        }
        out
    }

    /// Every native declaration's lightweight identity projection — extern types, enums, fielded
    /// types (classes + structs), and traits — as one allocation-free [`Nominal`] stream. The single
    /// source `namespace_types` / `classify_use` / `resolve_namespace_child` walk, replacing the four
    /// structurally identical per-kind loops each used to run. A fielded type projects as
    /// [`NominalKind::Class`] or [`NominalKind::Struct`] off its [`ExtFielded::kind`].
    pub fn nominal_types(&self) -> impl Iterator<Item = Nominal> + '_ {
        self.units.iter().flat_map(|e| {
            let types = e.types().iter().map(|t| Nominal {
                name: t.name,
                namespace: t.namespace,
                kind: NominalKind::Type,
            });
            let enums = e.enums().iter().map(|t| Nominal {
                name: t.name,
                namespace: t.namespace,
                kind: NominalKind::Enum,
            });
            let fielded = e
                .classes()
                .iter()
                .chain(e.structs().iter())
                .map(|t| Nominal {
                    name: t.name,
                    namespace: t.namespace,
                    kind: match t.kind {
                        FieldedKind::Class => NominalKind::Class,
                        FieldedKind::Struct => NominalKind::Struct,
                    },
                });
            let traits = e.traits().iter().map(|t| Nominal {
                name: t.name,
                namespace: t.namespace,
                kind: NominalKind::Trait,
            });
            // An attribute is a struct, so it projects as a [`NominalKind::Struct`] nominal — this is
            // the whole of its consumer-side resolution: `namespace_types`/`classify_use`/
            // `resolve_namespace_child` walk this one stream, so `use std.test.{Skip}` binds
            // `Skip → std.test.Skip` and a `use std.test` group surfaces it, identical to a type.
            let attributes = e.attributes().iter().map(|a| Nominal {
                name: a.name,
                namespace: a.namespace,
                kind: NominalKind::Struct,
            });
            types
                .chain(enums)
                .chain(fielded)
                .chain(traits)
                .chain(attributes)
        })
    }

    /// The extension **types** reachable under a namespace prefix, as `(relative path, qualified
    /// identity)` — `std.http` → `[("Response", "std.http.Response")]`. A type under a sub-namespace
    /// keeps the dotted remainder (`("client.Handle", "std.http.client.Handle")`). Lets a `use
    /// std.http` group expose its types for a dotted annotation (`http.Response`) the way it exposes
    /// its modules for a call (`http.client.get`). Projects every nominal kind — extern types,
    /// enums, classes, structs, traits — through the one [`Registry::nominal_types`] stream.
    pub fn namespace_types(&self, prefix: &str) -> Vec<(String, String)> {
        if prefix.split_once('.').is_none() {
            return Vec::new();
        }
        let dotted = format!("{prefix}.");
        self.nominal_types()
            .filter_map(|n| {
                let q = n.qualified();
                match q.strip_prefix(&dotted) {
                    Some(rest) => {
                        let rest = rest.to_string();
                        Some((rest, q))
                    }
                    None => None,
                }
            })
            .collect()
    }

    /// Resolve one namespace hop: what `<prefix>.<member>` names (`std.http` + `client` →
    /// [`NsChild::Module`]`("std.http.client")`). A module wins over a same-named deeper namespace
    /// (a concrete leaf is more specific); a type is checked before the namespace fallback.
    pub fn resolve_namespace_child(&self, prefix: &str, member: &str) -> NsChild {
        let qualified = format!("{prefix}.{member}");
        if self.find_module(&qualified).is_some() {
            NsChild::Module(qualified)
        } else if self.nominal_types().any(|n| n.is_qualified(&qualified)) {
            // A native enum, class, struct, or trait is a type-like member for namespace navigation
            // (`pkg.SameSite` / `pkg.Handle` / `pkg.Point` / `pkg.Widget` in a dotted annotation or
            // bound), resolved by identity like an extern type.
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
        // One projected-stream probe classifies every nominal kind (extern type / enum / class /
        // struct / trait) — the four structurally identical `find_*_qualified` cascades collapse to
        // the discriminant carried on the matched [`Nominal`].
        if let Some(n) = self.nominal_types().find(|n| n.is_qualified(&qualified)) {
            return match n.kind {
                NominalKind::Type => UseKind::ExternType(qualified),
                NominalKind::Enum => UseKind::ExtEnum(qualified),
                NominalKind::Class => UseKind::ExtClass(qualified),
                NominalKind::Struct => UseKind::ExtStruct(qualified),
                NominalKind::Trait => UseKind::ExtTrait(qualified),
            };
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

    /// Every installed extension's declared `@`-directives, in install order.
    pub fn ext_directives(&self) -> impl Iterator<Item = &'static ExtDirective> + '_ {
        self.units.iter().flat_map(|e| e.directives().iter())
    }

    /// The installed extension directive named `name`, if any.
    pub fn find_ext_directive(&self, name: &str) -> Option<&'static ExtDirective> {
        self.ext_directives().find(|d| d.name == name)
    }

    /// Every installed extension's tier-body formatters `(language, fn)`, in install order.
    pub fn ext_body_formatters(&self) -> impl Iterator<Item = &'static BodyFormatter> + '_ {
        self.units.iter().flat_map(|e| e.body_formatters().iter())
    }

    /// The installed extension attribute named `name` — its bare name (`Bench`) or its **qualified
    /// identity** (`std.bench.Bench`). Attributes are namespace-scoped (D2 — no global attribute
    /// namespace), and their identity-carrying references (a tier's `config`, an
    /// `attributes_of::<…>()` key) name them by the qualified form; matching only the bare `name`
    /// silently missed every such caller.
    pub fn find_ext_attribute(&self, name: &str) -> Option<&'static ExtAttribute> {
        self.ext_attributes()
            .find(|a| a.name == name || a.is_qualified(name))
    }

    /// Whether `qualified` (`std.test.Skip`, `cfg.Route`) is a registered native **attribute**'s
    /// identity — either a standalone [`ExtAttribute`] hook OR a fielded **struct** carrying the
    /// [`ExtTypeDirective::Attribute`] directive (D2b: a native `@attribute` struct is an attribute to
    /// every consumer). The linker consults this to fold **only attribute** imports into a module's
    /// rewrite map (so a `#[Skip]`/`#[Route]`/`attributes_of::<Skip>()` resolves to its FQN like the
    /// checker's gate does — one identity everywhere), leaving every other native import to the
    /// checker's `extern_types`. Covering the fielded form is what lets a native `@attribute` *struct*
    /// application qualify in a linked program (and, riding on it, a native `@role` surface — D3).
    pub fn is_ext_attribute(&self, qualified: &str) -> bool {
        self.ext_attributes().any(|a| a.is_qualified(qualified))
            || self.fielded().any(|f| {
                f.is_qualified(qualified)
                    && f.directives
                        .iter()
                        .any(|d| matches!(d, ExtTypeDirective::Attribute(_)))
            })
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

    /// Every registered native enum (native-extensibility S1), across all units — what
    /// `Checker::seed_ext_enums` walks to pre-populate the enum symbol tables at prelude time.
    pub fn enums(&self) -> impl Iterator<Item = &'static ExtEnum> + '_ {
        self.units.iter().flat_map(|e| e.enums())
    }

    /// Find a native enum by its **short** name (the first match wins, mirroring
    /// [`Registry::find_type`]). The unambiguous spelling within one extension's signature
    /// vocabulary; runtime identity paths prefer [`Registry::find_enum_qualified`].
    pub fn find_enum(&self, name: &str) -> Option<&'static ExtEnum> {
        self.units
            .iter()
            .flat_map(|e| e.enums())
            .find(|t| t.name == name)
    }

    /// Find a native enum by its **qualified identity** (`std.http.SameSite`) — allocation-free
    /// probing, mirroring [`Registry::find_type_qualified`].
    pub fn find_enum_qualified(&self, qualified: &str) -> Option<&'static ExtEnum> {
        self.units
            .iter()
            .flat_map(|e| e.enums())
            .find(|t| t.is_qualified(qualified))
    }

    /// Resolve a native enum from **either** a qualified identity or a bare short name — the
    /// enum twin of [`Registry::resolve_type`], read by `qualified_extern` and the `.value()`
    /// runtime accessor.
    pub fn resolve_enum(&self, name: &str) -> Option<&'static ExtEnum> {
        self.find_enum_qualified(name)
            .or_else(|| self.find_enum(name))
    }

    /// Every registered native class (native-extensibility S2), across all units — what
    /// `Checker::seed_ext_classes` walks to pre-populate the record symbol tables at prelude time.
    pub fn classes(&self) -> impl Iterator<Item = &'static ExtClass> + '_ {
        self.units.iter().flat_map(|e| e.classes())
    }

    /// Find a native class by its **short** name (first match wins, mirroring [`Registry::find_type`]).
    pub fn find_class(&self, name: &str) -> Option<&'static ExtClass> {
        self.units
            .iter()
            .flat_map(|e| e.classes())
            .find(|t| t.name == name)
    }

    /// Find a native class by its **qualified identity** (`res.Handle`) — allocation-free probing,
    /// mirroring [`Registry::find_type_qualified`].
    pub fn find_class_qualified(&self, qualified: &str) -> Option<&'static ExtClass> {
        self.units
            .iter()
            .flat_map(|e| e.classes())
            .find(|t| t.is_qualified(qualified))
    }

    /// Resolve a native class from **either** a qualified identity or a bare short name — the class
    /// twin of [`Registry::resolve_type`], read by `qualified_extern`. Class-kind only; a path that
    /// accepts a value struct too uses [`Registry::resolve_fielded`].
    pub fn resolve_class(&self, name: &str) -> Option<&'static ExtClass> {
        self.find_class_qualified(name)
            .or_else(|| self.find_class(name))
    }

    /// Every registered native **struct** (fielded unification), across all units — the value-type
    /// twin of [`Registry::classes`], walked by `seed_ext_fielded` alongside classes.
    pub fn structs(&self) -> impl Iterator<Item = &'static ExtStruct> + '_ {
        self.units.iter().flat_map(|e| e.structs())
    }

    /// Every registered native **fielded type** — classes and structs — across all units. The
    /// single stream the seeder and [`Registry::resolve_fielded`] read; each carries its own
    /// [`ExtFielded::kind`].
    pub fn fielded(&self) -> impl Iterator<Item = &'static ExtFielded> + '_ {
        self.units
            .iter()
            .flat_map(|e| e.classes().iter().chain(e.structs().iter()))
    }

    /// The **native `@role` table** (native type-declaration unification, Slice D3) — the plain-data
    /// projection `noeta_ast::reflect::build` merges to surface native roles. Each entry is a
    /// role-bearing native `@attribute` struct's **qualified identity** (`cfg.Route`, the identity a
    /// linked attribute application is qualified to) paired with its `(enum_name, variant)` role tags.
    /// `reflect::build` joins these against the in-program attribute applications exactly as it joins a
    /// `.noe` struct's own `@role` tags, so applying a native role-bearing attribute confers the role.
    /// A registry-free `Vec` (owned `String`s) because `noeta-ast` cannot see this crate; the callers
    /// that have this compile's registry pass `&reg.native_roles()` and the pure `.noe` path passes
    /// `&[]`. Empty when no native fielded type carries [`ExtTypeDirective::Role`].
    pub fn native_roles(&self) -> Vec<(String, Vec<(String, String)>)> {
        self.fielded()
            .filter_map(|f| {
                let tags: Vec<(String, String)> = f
                    .directives
                    .iter()
                    .flat_map(|d| match d {
                        ExtTypeDirective::Role(tags) => tags
                            .iter()
                            .map(|t| (t.enum_name.to_string(), t.variant.to_string()))
                            .collect::<Vec<_>>(),
                        _ => Vec::new(),
                    })
                    .collect();
                (!tags.is_empty()).then(|| (f.qualified(), tags))
            })
            .collect()
    }

    /// Find a native struct by its **qualified identity** — allocation-free probing, mirroring
    /// [`Registry::find_class_qualified`].
    pub fn find_struct_qualified(&self, qualified: &str) -> Option<&'static ExtStruct> {
        self.structs().find(|t| t.is_qualified(qualified))
    }

    /// Resolve a native **fielded type** (class OR struct) from either a qualified identity or a
    /// bare short name. What both backends consult to materialize a [`NativeOut::Instance`] with the
    /// right shape kind (via [`ExtFielded::kind`]) and to marshal a native fielded receiver/arg.
    pub fn resolve_fielded(&self, name: &str) -> Option<&'static ExtFielded> {
        self.fielded()
            .find(|t| t.is_qualified(name))
            .or_else(|| self.fielded().find(|t| t.name == name))
    }

    /// Find a native fielded type's instance-method signature (native-extensibility S3 / Pass 2a) —
    /// the [`ExtFielded`] twin of [`Registry::find_type_method`]. `name` is the runtime shape name
    /// (the **short** name a fielded object carries) or a qualified identity. What both backends'
    /// `CallMethod` Object arm consults to decide a native fielded method call routes to
    /// [`ExtFielded::dispatch`]. Resolves over both classes and structs.
    pub fn find_class_method(&self, name: &str, method: &str) -> Option<&'static ExtFn> {
        self.resolve_fielded(name)?
            .methods
            .iter()
            .find(|m| m.name == method)
    }

    /// Find a native **enum**'s instance-method signature (native-extensibility S1 / Slice B) — the
    /// [`ExtEnum`] twin of [`Registry::find_class_method`]. `name` is the runtime shape name (the
    /// **short** name a native enum value carries) or a qualified identity. What both backends' enum
    /// method-call arm consults to decide a native enum method call routes to [`ExtEnum::dispatch`],
    /// and what the checker types the call off — the enum mirror of `find_class_method` →
    /// `call_native_class_method`.
    pub fn find_enum_method(&self, name: &str, method: &str) -> Option<&'static ExtFn> {
        self.resolve_enum(name)?
            .methods
            .iter()
            .find(|m| m.name == method)
    }

    /// Every registered native trait (native-extensibility S3), across all units — what
    /// `Checker::seed_ext_traits` walks to pre-populate the user-trait tables at prelude time, and
    /// what `seed_native_builtin_traits` matches an [`ExtType::traits`] / [`ExtFielded::traits`] /
    /// [`ExtEnum::traits`] name against.
    pub fn traits(&self) -> impl Iterator<Item = &'static ExtTrait> + '_ {
        self.units.iter().flat_map(|e| e.traits())
    }

    /// Find a native trait by its **short** name (first match wins, mirroring [`Registry::find_type`]).
    pub fn find_trait(&self, name: &str) -> Option<&'static ExtTrait> {
        self.units
            .iter()
            .flat_map(|e| e.traits())
            .find(|t| t.name == name)
    }

    /// Find a native trait by its **qualified identity** (`fx.Widget`) — allocation-free probing,
    /// mirroring [`Registry::find_type_qualified`].
    pub fn find_trait_qualified(&self, qualified: &str) -> Option<&'static ExtTrait> {
        self.units
            .iter()
            .flat_map(|e| e.traits())
            .find(|t| t.is_qualified(qualified))
    }

    /// Resolve a native trait from **either** a qualified identity or a bare short name — the trait
    /// twin of [`Registry::resolve_type`], read by `qualified_extern`.
    pub fn resolve_trait(&self, name: &str) -> Option<&'static ExtTrait> {
        self.find_trait_qualified(name)
            .or_else(|| self.find_trait(name))
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

    /// A type method's signature from **either** plain table — what the checker consults for an
    /// ordinary (non-turbofish) call. Deliberately excludes [`ExtType::typed_methods`], whose names
    /// live in their own space and are only reachable through a `::<T>` call site.
    pub fn find_type_method_sig(&self, type_name: &str, method: &str) -> Option<&'static ExtFn> {
        self.find_type_method(type_name, method)
            .or_else(|| self.find_type_ctx_method(type_name, method))
    }

    /// Find a registered extern type's **call-site-typed** method signature (http arc H8) — the
    /// `resp.json::<T>()` turbofish surface, the [`Registry::find_typed_function`] twin. The single
    /// predicate that decides whether a turbofish method call is a native typed call or an
    /// ordinary (erased) generic-method instantiation.
    pub fn find_typed_method(&self, type_name: &str, method: &str) -> Option<&'static ExtFn> {
        self.resolve_type(type_name)?
            .typed_methods
            .iter()
            .find(|m| m.name == method)
    }

    /// Route a **call-site-typed** method call to its type's typed dispatch (http arc H8).
    pub fn dispatch_typed_method(
        &self,
        recv: &mut dyn crate::ExternValue,
        method: &str,
        host: &mut dyn Host,
        args: &[NativeValue],
        recipe: &TypeRecipe,
    ) -> Result<NativeOut, StdError> {
        let identity = recv.type_identity();
        let Some(ext) = self.find_type_qualified(identity) else {
            return Err(StdError {
                kind: crate::ErrorKind::UnknownName,
                message: format!("no registered type `{identity}`"),
            });
        };
        let Some(dispatch) = ext.typed_dispatch else {
            return Err(StdError {
                kind: crate::ErrorKind::UnknownName,
                message: format!("type `{identity}` has no call-site-typed method `{method}`"),
            });
        };
        let result = dispatch(recv, method, host, args, recipe);
        #[cfg(debug_assertions)]
        if let Ok(out) = &result {
            self.debug_verify_out(identity, method, out);
        }
        result
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
            // A native enum result (native-extensibility S1): recurse into its payload for any
            // nested extern values, exactly like the `Struct`/`List` arms.
            O::Variant { fields, .. } => {
                for value in fields {
                    self.debug_verify_out(owner, func, value);
                }
            }
            // A native class instance (native-extensibility S2): recurse into each field for any
            // nested extern values (the native-state handle field is one), like the `Struct` arm.
            O::Instance { fields, .. } => {
                for (_, value) in fields {
                    self.debug_verify_out(owner, func, value);
                }
            }
            // An in-place instance mutation (boundary 1): recurse into each write's value and the
            // method's own return, so a nested extern in either is still verified.
            O::InstanceUpdate { writes, ret } => {
                for (_, value) in writes {
                    self.debug_verify_out(owner, func, value);
                }
                self.debug_verify_out(owner, func, ret);
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

/// The **fallback provider**: the unit list [`single_registry_process`] installs when a lookup finds
/// nothing installed. Registered at link time by whichever crate declares the units — `noeta-stdlib`
/// does it for the std set — so a binary that uses that crate has a working default without any call
/// site having to remember to seed, and without any ordering requirement.
///
/// Scope, precisely: registration rides in the declaring crate's object, and a linker pulls that
/// object only into a binary that references *something* from the crate. A binary that names no item
/// of it at all gets no registration and still panics on first lookup (no worse than before this
/// seam, but it is not literally "linking is enough"). Every real front-end test binary carries such
/// a reference already; `noeta-stdlib`'s `tests/fallback_provider.rs` pins the behaviour with a
/// deliberately inert one.
///
/// Registering is deliberately **not** installing. The pointer sits here unused until the first
/// lookup that finds [`DEFAULT`] empty, which is what keeps an assembling binary's explicit
/// [`install`]/`install_with_extras` authoritative: it runs before that binary's first lookup, wins
/// the `OnceLock`, and the provider is never consulted. Installing at registration time instead
/// would seed std-only into every composed binary and make its `install` panic.
static DEFAULT_PROVIDER: OnceLock<fn() -> Vec<&'static (dyn Extension + Sync)>> = OnceLock::new();

/// Register the [`DEFAULT_PROVIDER`] — the units to fall back to when nothing was explicitly
/// installed. Idempotent and first-registration-wins; it does not install anything (see
/// [`DEFAULT_PROVIDER`]), so it can never race an assembling binary's explicit [`install`].
///
/// Called from a link-time initializer in the unit-declaring crate, not from application code.
pub fn set_default_provider(provider: fn() -> Vec<&'static (dyn Extension + Sync)>) {
    let _ = DEFAULT_PROVIDER.set(provider);
}

/// The process-global default [`Registry`], named for what calling it MEANS: **this call site
/// assumes a single-registry process** (cross-cutting audit finding 5). The front-end crates
/// (checker, loader, IR lowering, bytecode compiler, salsa db) fall back to this when no
/// per-session registry was threaded in — they consume the registry as *data* and deliberately do
/// not link the crate that declares the units (audit-6 finding 2), so the units reach this registry
/// from outside: an assembling binary installs its exact set at entry
/// (`noeta_cli::run_cli`, `noeta-runner`, `noeta-embed`), and otherwise the
/// [`DEFAULT_PROVIDER`] registered by the unit-declaring crate is installed lazily, here.
///
/// That fallback is what makes seeding **structural rather than remembered** (within the linkage
/// scope noted on [`DEFAULT_PROVIDER`]). It used to be neither:
/// this function panicked unless something had already seeded, which an assembling binary does but a
/// *test* binary does not — so a crate's tests passed only when some sibling test happened to run
/// first through the lazily-seeding `noeta-stdlib` facade. Different scheduling, different set of
/// panics; CI eventually drew the short straw across four crates at once. Linking the unit-declaring
/// crate now suffices, which every such test binary already does.
///
/// Panics only if nothing installed *and* no provider was registered — i.e. no unit-declaring crate
/// is linked at all. That is the genuine assembly error the panic was written for, and it stays
/// loud, because the silent alternative is a checker that reports every `std.*` name as unknown.
pub fn single_registry_process() -> &'static Registry {
    if let Some(registry) = DEFAULT.get() {
        return registry;
    }
    if let Some(provider) = DEFAULT_PROVIDER.get() {
        // `get_or_init`, so a racing explicit `install` on another thread still wins cleanly.
        install_default(*provider);
        return DEFAULT
            .get()
            .expect("`install_default` seeds the default registry immediately above");
    }
    panic!(
        "no extension registry installed in this process and no default provider registered — \
         link a crate that declares extension units (`noeta-stdlib` registers the std set at load \
         time), install a composed set with `install`/`install_with_extras` before the first \
         front-end lookup, or thread a per-session registry through the options/`_with_registry` \
         seams"
    )
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
/// The built-in `Semantic` prelude enum's variant names (native type-declaration unification, Slice
/// D3). Mirrors `noeta_ast::reflect::SEMANTIC_VARIANTS` — `noeta-ext-abi` is dep-free of `noeta-ast`,
/// so [`Registry::validate`]'s native `@role` check keeps its own copy to resolve a tag naming the
/// built-in `Semantic` vocabulary (every variant fieldless, so a named one is valid iff it appears
/// here). If the prelude list changes, update this mirror.
const BUILTIN_SEMANTIC_VARIANTS: &[&str] = &[
    "EntryPoint",
    "PersistenceBoundary",
    "TrustBoundary",
    "Sink",
    "Layer",
];

/// Assembly-time check for one native `@role` tag (Slice D3): its `enum_name` must resolve to a
/// `@semantic` enum — the built-in `Semantic` prelude enum, or a native enum (across every unit)
/// whose **qualified identity** matches and that carries [`ExtTypeDirective::Semantic`] — and its
/// named `variant` must exist on that enum and be **fieldless**. The native analogue of the checker's
/// E0031 role-tag rules; `t`/`unit` name the tagged struct for the diagnostic.
fn check_role_tag(
    units: &[&'static (dyn Extension + Sync)],
    tag: &ExtRoleTag,
    t: &ExtFielded,
    unit: &str,
) -> Result<(), String> {
    // The built-in `Semantic` vocabulary is always `@semantic`; its variants are all fieldless.
    if tag.enum_name == "Semantic" {
        if !BUILTIN_SEMANTIC_VARIANTS.contains(&tag.variant) {
            return Err(format!(
                "native struct `{}.{}` of unit `{unit}` carries `@role(Semantic.{})`, but the \
                 built-in `Semantic` enum has no variant `{}`",
                t.namespace, t.name, tag.variant, tag.variant
            ));
        }
        return Ok(());
    }
    // Otherwise the enum must be a native `@semantic` enum, resolved by qualified identity.
    let Some(en) = units
        .iter()
        .flat_map(|u| u.enums())
        .find(|en| en.is_qualified(tag.enum_name))
    else {
        return Err(format!(
            "native struct `{}.{}` of unit `{unit}` carries `@role({}.{})`, but `{}` is not a \
             registered native enum (name it by its qualified identity, or use the built-in \
             `Semantic`)",
            t.namespace, t.name, tag.enum_name, tag.variant, tag.enum_name
        ));
    };
    if !en
        .directives
        .iter()
        .any(|d| matches!(d, ExtTypeDirective::Semantic))
    {
        return Err(format!(
            "native struct `{}.{}` of unit `{unit}` carries `@role({}.{})`, but `{}` is not a \
             `@semantic` enum — only a `@semantic` enum's variants are roles",
            t.namespace, t.name, tag.enum_name, tag.variant, tag.enum_name
        ));
    }
    match en.variant(tag.variant) {
        None => Err(format!(
            "native struct `{}.{}` of unit `{unit}` carries `@role({}.{})`, but `{}` has no \
             variant `{}`",
            t.namespace, t.name, tag.enum_name, tag.variant, tag.enum_name, tag.variant
        )),
        Some((_, v)) if !v.fields.is_empty() => Err(format!(
            "native struct `{}.{}` of unit `{unit}` carries `@role({}.{})`, but variant `{}` is \
             not fieldless — a role variant carries no payload",
            t.namespace, t.name, tag.enum_name, tag.variant, tag.variant
        )),
        Some(_) => Ok(()),
    }
}

/// Whether `ty` can be a field of a native `@packed` struct (Slice E1, the native twin of the
/// checker's [`is_packable_type`](https://docs.rs/noeta-check) / E0038). The packable set is the
/// primitives [`SigType`] can spell — `Int`/`Float`/`F32`/`Bool` — plus a [`SigType::Named`] that
/// resolves to another `@packed` struct in the assembled `units` (so a nested packed field flattens
/// inline). Everything else — `String`/`Bytes`/`List`/`Map`/`Option`/`Result`/`Dyn`/a class or enum
/// or non-packed struct — is heap-shaped and cannot lay out flat. Note [`SigType`] has no `IntN`/`F64`
/// variant, so a native `@packed` struct's fixed-width fields are limited to `F32`; the wider-primitive
/// set a `.noe` `@packed` admits (`i8..i64`, `f64`) is simply unspellable natively (a scope bound, not
/// a soundness gap).
fn packable_field(units: &[&'static (dyn Extension + Sync)], ty: &SigType) -> bool {
    match ty {
        SigType::Int | SigType::Float | SigType::F32 | SigType::Bool => true,
        SigType::Named(n) => units
            .iter()
            .flat_map(|u| u.structs())
            .filter(|s| s.name == *n || s.is_qualified(n))
            .any(|s| {
                s.directives
                    .iter()
                    .any(|d| matches!(d, ExtTypeDirective::Packed(_)))
            }),
        _ => false,
    }
}

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
    // Native-enum identities (native-extensibility S1) — the same qualified-identity uniqueness and
    // namespace-under-root rules extern types get, since native enums are keyed identically.
    let mut enums: Vec<((&str, &str), &str)> = units
        .iter()
        .flat_map(|e| {
            e.enums()
                .iter()
                .map(move |t| ((t.namespace, t.name), e.name()))
        })
        .collect();
    enums.sort_unstable();
    for pair in enums.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(format!(
                "duplicate native enum `{}.{}` in the assembled registry (units `{}` and `{}`): \
                 a qualified enum identity must be declared exactly once",
                pair[0].0.0, pair[0].0.1, pair[0].1, pair[1].1
            ));
        }
    }
    for unit in units {
        let root = unit.root();
        for t in unit.enums() {
            if t.namespace != root && !t.namespace.starts_with(&format!("{root}.")) {
                return Err(format!(
                    "native enum `{}` of unit `{}` declares namespace `{}`, outside the unit's \
                     root `{root}`",
                    t.name,
                    unit.name(),
                    t.namespace
                ));
            }
        }
    }
    // Native fielded types (classes + structs, fielded unification). Same qualified-identity
    // uniqueness and namespace-under-root rules; classes and structs share one identity space (a
    // struct and a class may not share a qualified name), so the two hooks are checked together.
    let mut fielded: Vec<((&str, &str), &str)> = units
        .iter()
        .flat_map(|e| {
            e.classes()
                .iter()
                .chain(e.structs().iter())
                .map(move |t| ((t.namespace, t.name), e.name()))
        })
        .collect();
    fielded.sort_unstable();
    for pair in fielded.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(format!(
                "duplicate native fielded type `{}.{}` in the assembled registry (units `{}` and \
                 `{}`): a qualified class/struct identity must be declared exactly once",
                pair[0].0.0, pair[0].0.1, pair[0].1, pair[1].1
            ));
        }
    }
    for unit in units {
        let root = unit.root();
        for t in unit.classes().iter().chain(unit.structs().iter()) {
            if t.namespace != root && !t.namespace.starts_with(&format!("{root}.")) {
                return Err(format!(
                    "native fielded type `{}` of unit `{}` declares namespace `{}`, outside the \
                     unit's root `{root}`",
                    t.name,
                    unit.name(),
                    t.namespace
                ));
            }
        }
        // An attribute is a namespaced nominal too (D2b) — same namespace-under-root rule, so a
        // forgotten `namespace:` (defaulting to nothing) or a cross-root claim is caught at assembly
        // rather than silently squatting a reserved namespace.
        for a in unit.attributes() {
            if a.namespace != root && !a.namespace.starts_with(&format!("{root}.")) {
                return Err(format!(
                    "native attribute `{}` of unit `{}` declares namespace `{}`, outside the \
                     unit's root `{root}`",
                    a.name,
                    unit.name(),
                    a.namespace
                ));
            }
        }
        // The two author-facing hooks are the same underlying type distinguished only by
        // `ExtFielded::kind`; a mismatched entry (a `Struct` in `classes()` or a `Class` in
        // `structs()`) would silently seed the wrong semantics. Catch it loudly at assembly.
        for t in unit.classes() {
            if t.kind != FieldedKind::Class {
                return Err(format!(
                    "native type `{}.{}` of unit `{}` is declared through `classes()` but its \
                     `kind` is `Struct` — a class hook must carry `FieldedKind::Class` (use \
                     `structs()` for a value struct)",
                    t.namespace,
                    t.name,
                    unit.name()
                ));
            }
        }
        for t in unit.structs() {
            if t.kind != FieldedKind::Struct {
                return Err(format!(
                    "native type `{}.{}` of unit `{}` is declared through `structs()` but its \
                     `kind` is `Class` — a struct hook must carry `FieldedKind::Struct` (use \
                     `..ExtStruct::STRUCT_DEFAULTS`)",
                    t.namespace,
                    t.name,
                    unit.name()
                ));
            }
        }
        // Built-in-directive site validity (native type-declaration unification, Slice D): a native
        // type seeds its directives straight into `Symbols`, bypassing the AST placement gate (E0054),
        // so the (kind, directive) legality the source-level gate enforces is enforced here instead.
        // `@semantic` is enum-only; `@validated` is struct-or-class-only. A mis-placed directive would
        // silently seed the wrong table — refuse it loudly at assembly.
        for t in unit.classes().iter().chain(unit.structs().iter()) {
            for d in t.directives {
                if let ExtTypeDirective::Semantic = d {
                    return Err(format!(
                        "native fielded type `{}.{}` of unit `{}` carries `@semantic`, which \
                         applies only to an enum",
                        t.namespace,
                        t.name,
                        unit.name()
                    ));
                }
                // `@attribute` is struct-only — an attribute is a struct (one canonical all-fields
                // construction), never a reference class.
                if matches!(d, ExtTypeDirective::Attribute(_)) && t.kind == FieldedKind::Class {
                    return Err(format!(
                        "native class `{}.{}` of unit `{}` carries `@attribute`, which applies \
                         only to a struct (an attribute is a value struct, not a class)",
                        t.namespace,
                        t.name,
                        unit.name()
                    ));
                }
                // `@role` (Slice D3): struct-only, and a *facet* of `@attribute` — the native analogue
                // of the checker's E0031 role rules. A role rides on what the attribute attaches to, so
                // the same type must carry `@attribute`; each tag must name a fieldless variant of a
                // `@semantic` enum (a native one, or the built-in `Semantic`). A native type bypasses
                // the source-level gate, so these couplings are enforced here at assembly.
                if let ExtTypeDirective::Role(tags) = d {
                    if t.kind == FieldedKind::Class {
                        return Err(format!(
                            "native class `{}.{}` of unit `{}` carries `@role`, which applies only \
                             to a struct (a role tags an `@attribute` struct)",
                            t.namespace,
                            t.name,
                            unit.name()
                        ));
                    }
                    if !t
                        .directives
                        .iter()
                        .any(|x| matches!(x, ExtTypeDirective::Attribute(_)))
                    {
                        return Err(format!(
                            "native struct `{}.{}` of unit `{}` carries `@role` without \
                             `@attribute` — a role is a facet of an attribute and has nothing to \
                             attach to on a plain struct; also declare it `@attribute`",
                            t.namespace,
                            t.name,
                            unit.name()
                        ));
                    }
                    for tag in *tags {
                        check_role_tag(units, tag, t, unit.name())?;
                    }
                }
                // `@packed` (Slice E1): struct-only, and every field must be packable — the native
                // analogue of the checker's E0038. A class is a reference type (heap identity, no flat
                // list layout), so `@packed` on it refuses to assemble; a field that cannot lay out flat
                // (`string`/`List`/class/enum/`dyn`, or a `Named` that is not itself a `@packed` struct)
                // does too. A native type bypasses the source-level gate, so this is enforced here.
                if let ExtTypeDirective::Packed(_) = d {
                    if t.kind == FieldedKind::Class {
                        return Err(format!(
                            "native class `{}.{}` of unit `{}` carries `@packed`, which applies only \
                             to a struct (a flat packed list is a value-type layout, not a reference \
                             class)",
                            t.namespace,
                            t.name,
                            unit.name()
                        ));
                    }
                    for f in t.fields {
                        if !packable_field(units, &f.ty) {
                            return Err(format!(
                                "native `@packed` struct `{}.{}` of unit `{}` has field `{}` whose \
                                 type is not packable — a `@packed` struct's fields must be `int`, \
                                 `float`, `f32`, `bool`, or another `@packed` struct (a \
                                 string/list/map/class/enum/dyn cannot lay out flat)",
                                t.namespace,
                                t.name,
                                unit.name(),
                                f.name
                            ));
                        }
                    }
                }
            }
        }
        for t in unit.enums() {
            for d in t.directives {
                if let ExtTypeDirective::Validated = d {
                    return Err(format!(
                        "native enum `{}.{}` of unit `{}` carries `@validated`, which applies only \
                         to a struct or a class",
                        t.namespace,
                        t.name,
                        unit.name()
                    ));
                }
                if matches!(d, ExtTypeDirective::Attribute(_)) {
                    return Err(format!(
                        "native enum `{}.{}` of unit `{}` carries `@attribute`, which applies only \
                         to a struct",
                        t.namespace,
                        t.name,
                        unit.name()
                    ));
                }
                // `@role` tags an `@attribute` struct — never an enum (an enum is a role *vocabulary*
                // via `@semantic`, not a role bearer).
                if matches!(d, ExtTypeDirective::Role(_)) {
                    return Err(format!(
                        "native enum `{}.{}` of unit `{}` carries `@role`, which applies only to a \
                         struct (an enum is a role vocabulary via `@semantic`, not a role bearer)",
                        t.namespace,
                        t.name,
                        unit.name()
                    ));
                }
                // `@packed` is a flat value-struct layout — never an enum (a sum has no single flat
                // shape). Struct-only, like `@attribute`/`@role`.
                if matches!(d, ExtTypeDirective::Packed(_)) {
                    return Err(format!(
                        "native enum `{}.{}` of unit `{}` carries `@packed`, which applies only to a \
                         struct (an enum is a sum type with no single flat layout)",
                        t.namespace,
                        t.name,
                        unit.name()
                    ));
                }
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
            // A `Uniform` constraint reads only `fields[0]` (every field of the bound type must
            // equal it) — declaring more than one kind is an author bug that would silently ignore
            // the rest, so refuse it here where every assembly path passes.
            if let ConstraintArity::Uniform { .. } = bundle.constraint.arity
                && bundle.constraint.fields.len() != 1
            {
                return Err(format!(
                    "bundle `{}.{}` has a `Uniform` arity constraint but declares {} field kinds — \
                     a uniform vector constraint names exactly one kind (`fields[0]`)",
                    module.name,
                    bundle.name,
                    bundle.constraint.fields.len()
                ));
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
            // The call-site-typed table mirrors the module side's rules (http arc H8): a dispatch
            // is required, and every entry must name its result through the turbofish — a
            // `Concrete` return would make the `::<T>` meaningless.
            if !t.typed_methods.is_empty() && t.typed_dispatch.is_none() {
                return Err(format!(
                    "type `{}` (unit `{}`) declares typed_methods but no typed_dispatch",
                    t.name,
                    unit.name()
                ));
            }
            for m in t.typed_methods {
                if !matches!(m.ret, RetTy::TypeArg(_)) {
                    return Err(format!(
                        "call-site-typed method `{}` of type `{}` (unit `{}`) must declare a \
                         `RetTy::TypeArg` return (its result is named by the turbofish `::<T>`)",
                        m.name,
                        t.name,
                        unit.name()
                    ));
                }
            }
            for m in t.methods.iter().chain(t.ctx_methods).chain(t.typed_methods) {
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
        arity: ConstraintArity::Exact,
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

    // --- native built-in-directive site validity (Slice D) ---
    //
    // A native type seeds its `@semantic`/`@validated` directive straight into `Symbols`, bypassing
    // the source-level placement gate (E0054), so the (kind, directive) legality is enforced at
    // assembly here: `@semantic` is enum-only, `@validated` is struct-or-class-only.

    /// A unit carrying directive-bearing native types under root `cfg`.
    struct DirUnit(
        &'static str,
        &'static [ExtEnum],
        &'static [ExtStruct],
        &'static [ExtClass],
    );
    impl Extension for DirUnit {
        fn name(&self) -> &'static str {
            self.0
        }
        fn root(&self) -> &'static str {
            "cfg"
        }
        fn modules(&self) -> &'static [ExtModule] {
            &[]
        }
        fn enums(&self) -> &'static [ExtEnum] {
            self.1
        }
        fn structs(&self) -> &'static [ExtStruct] {
            self.2
        }
        fn classes(&self) -> &'static [ExtClass] {
            self.3
        }
    }

    const STAGE: ExtEnum = ExtEnum {
        name: "Stage",
        namespace: "cfg",
        variants: &[ExtVariant {
            name: "Alpha",
            fields: &[],
            value: VariantValue::None,
        }],
        directives: &[ExtTypeDirective::Semantic],
        ..ExtEnum::DEFAULTS
    };
    const VALIDATED_STRUCT: ExtStruct = ExtStruct {
        name: "Conf",
        namespace: "cfg",
        directives: &[ExtTypeDirective::Validated],
        ..ExtStruct::STRUCT_DEFAULTS
    };
    const VALIDATED_CLASS: ExtClass = ExtClass {
        name: "Handle",
        namespace: "cfg",
        directives: &[ExtTypeDirective::Validated],
        ..ExtClass::DEFAULTS
    };

    #[test]
    fn native_directive_legal_placements_assemble() {
        // `@semantic` on an enum, `@validated` on a struct AND on a class — every legal pair.
        static U: DirUnit = DirUnit(
            "cfg.core",
            &[STAGE],
            &[VALIDATED_STRUCT],
            &[VALIDATED_CLASS],
        );
        validate(&[&U]).expect("@semantic on an enum and @validated on a struct/class are legal");
    }

    #[test]
    fn native_semantic_on_a_struct_is_rejected() {
        const BAD: ExtStruct = ExtStruct {
            name: "Nope",
            namespace: "cfg",
            directives: &[ExtTypeDirective::Semantic],
            ..ExtStruct::STRUCT_DEFAULTS
        };
        static U: DirUnit = DirUnit("cfg.core", &[], &[BAD], &[]);
        assert!(
            validate(&[&U]).is_err(),
            "`@semantic` applies only to an enum — a struct carrying it must refuse to assemble"
        );
    }

    #[test]
    fn native_validated_on_an_enum_is_rejected() {
        const BAD: ExtEnum = ExtEnum {
            name: "Nope",
            namespace: "cfg",
            variants: &[ExtVariant {
                name: "A",
                fields: &[],
                value: VariantValue::None,
            }],
            directives: &[ExtTypeDirective::Validated],
            ..ExtEnum::DEFAULTS
        };
        static U: DirUnit = DirUnit("cfg.core", &[BAD], &[], &[]);
        assert!(
            validate(&[&U]).is_err(),
            "`@validated` applies only to a struct/class — an enum carrying it must refuse to assemble"
        );
    }

    // --- native @packed site validity + packable-field rule (Slice E1) ---
    //
    // `@packed` is struct-only, and every field must be packable — the native analogue of the checker's
    // E0038. A native type bypasses the source gate, so `validate` enforces both at assembly.

    /// A well-formed `@packed(Row)` value struct: all-primitive fields.
    const PACKED_PT: ExtStruct = ExtStruct {
        name: "Pt",
        namespace: "cfg",
        fields: &[
            ExtField {
                name: "x",
                ty: SigType::Int,
                is_public: true,
                is_mut: false,
            },
            ExtField {
                name: "y",
                ty: SigType::Float,
                is_public: true,
                is_mut: false,
            },
        ],
        directives: &[ExtTypeDirective::Packed(PackedLayoutKind::Row)],
        ..ExtStruct::STRUCT_DEFAULTS
    };

    #[test]
    fn native_packed_struct_with_primitive_fields_assembles() {
        static U: DirUnit = DirUnit("cfg.core", &[], &[PACKED_PT], &[]);
        validate(&[&U])
            .expect("a `@packed` struct with all-primitive (int/float) fields is well-formed");
    }

    #[test]
    fn native_packed_struct_with_a_nested_packed_field_assembles() {
        // A `Named` field resolving to another `@packed` struct in the same unit is packable (it
        // flattens inline), exactly like a `.noe` nested packed field.
        const SEGMENT: ExtStruct = ExtStruct {
            name: "Segment",
            namespace: "cfg",
            fields: &[
                ExtField {
                    name: "start",
                    ty: SigType::Named("Pt"),
                    is_public: true,
                    is_mut: false,
                },
                ExtField {
                    name: "end",
                    ty: SigType::Named("Pt"),
                    is_public: true,
                    is_mut: false,
                },
            ],
            directives: &[ExtTypeDirective::Packed(PackedLayoutKind::Row)],
            ..ExtStruct::STRUCT_DEFAULTS
        };
        static U: DirUnit = DirUnit("cfg.core", &[], &[PACKED_PT, SEGMENT], &[]);
        validate(&[&U]).expect(
            "a `@packed` field naming another `@packed` struct is packable (nested flatten)",
        );
    }

    #[test]
    fn native_packed_struct_with_a_non_packable_field_is_rejected() {
        // A `string` field cannot lay out flat — the native E0038 analogue must refuse assembly.
        const BAD: ExtStruct = ExtStruct {
            name: "Nope",
            namespace: "cfg",
            fields: &[ExtField {
                name: "label",
                ty: SigType::String,
                is_public: true,
                is_mut: false,
            }],
            directives: &[ExtTypeDirective::Packed(PackedLayoutKind::Row)],
            ..ExtStruct::STRUCT_DEFAULTS
        };
        static U: DirUnit = DirUnit("cfg.core", &[], &[BAD], &[]);
        assert!(
            validate(&[&U]).is_err(),
            "a `@packed` struct with a `string` field is not flat-layoutable — it must refuse to assemble"
        );
    }

    #[test]
    fn native_packed_struct_naming_a_non_packed_struct_is_rejected() {
        // A `Named` field resolving to a NON-`@packed` struct is heap-shaped — reject it.
        const PLAIN: ExtStruct = ExtStruct {
            name: "Plain",
            namespace: "cfg",
            fields: &[ExtField {
                name: "n",
                ty: SigType::Int,
                is_public: true,
                is_mut: false,
            }],
            ..ExtStruct::STRUCT_DEFAULTS
        };
        const BAD: ExtStruct = ExtStruct {
            name: "Holder",
            namespace: "cfg",
            fields: &[ExtField {
                name: "inner",
                ty: SigType::Named("Plain"),
                is_public: true,
                is_mut: false,
            }],
            directives: &[ExtTypeDirective::Packed(PackedLayoutKind::Row)],
            ..ExtStruct::STRUCT_DEFAULTS
        };
        static U: DirUnit = DirUnit("cfg.core", &[], &[PLAIN, BAD], &[]);
        assert!(
            validate(&[&U]).is_err(),
            "a `@packed` field naming a NON-`@packed` struct is heap-shaped — it must refuse to assemble"
        );
    }

    #[test]
    fn native_packed_on_a_class_is_rejected() {
        const BAD: ExtClass = ExtClass {
            name: "Nope",
            namespace: "cfg",
            directives: &[ExtTypeDirective::Packed(PackedLayoutKind::Row)],
            ..ExtClass::DEFAULTS
        };
        static U: DirUnit = DirUnit("cfg.core", &[], &[], &[BAD]);
        assert!(
            validate(&[&U]).is_err(),
            "`@packed` applies only to a value struct — a reference class carrying it must refuse to assemble"
        );
    }

    #[test]
    fn native_packed_on_an_enum_is_rejected() {
        const BAD: ExtEnum = ExtEnum {
            name: "Nope",
            namespace: "cfg",
            variants: &[ExtVariant {
                name: "A",
                fields: &[],
                value: VariantValue::None,
            }],
            directives: &[ExtTypeDirective::Packed(PackedLayoutKind::Row)],
            ..ExtEnum::DEFAULTS
        };
        static U: DirUnit = DirUnit("cfg.core", &[BAD], &[], &[]);
        assert!(
            validate(&[&U]).is_err(),
            "`@packed` applies only to a struct — an enum carrying it must refuse to assemble"
        );
    }

    // --- native @role coupling (Slice D3) ---
    //
    // `@role` is a facet of `@attribute`: struct-only, on a type that also carries `@attribute`, each
    // tag naming a fieldless variant of a `@semantic` enum. A native type bypasses the source gate, so
    // `validate` enforces every coupling at assembly — the native analogue of the checker's E0031.

    /// A `@semantic` enum with one fieldless and one FIELDED variant — a fielded-variant role must be
    /// rejected, a fieldless one accepted.
    const KIND: ExtEnum = ExtEnum {
        name: "Kind",
        namespace: "cfg",
        variants: &[
            ExtVariant {
                name: "Simple",
                fields: &[],
                value: VariantValue::None,
            },
            ExtVariant {
                name: "Tagged",
                fields: &[SigType::Int],
                value: VariantValue::None,
            },
        ],
        directives: &[ExtTypeDirective::Semantic],
        ..ExtEnum::DEFAULTS
    };
    /// A plain (non-`@semantic`) enum — naming it in a `@role` must be rejected.
    const PLAIN: ExtEnum = ExtEnum {
        name: "Plain",
        namespace: "cfg",
        variants: &[ExtVariant {
            name: "A",
            fields: &[],
            value: VariantValue::None,
        }],
        ..ExtEnum::DEFAULTS
    };

    #[test]
    fn native_role_with_attribute_and_a_semantic_variant_assembles() {
        // The legal shape: an `@attribute` struct also carrying `@role`, over the built-in `Semantic`
        // vocabulary AND a native `@semantic` enum's fieldless variant.
        const OK: ExtStruct = ExtStruct {
            name: "Route",
            namespace: "cfg",
            directives: &[
                ExtTypeDirective::Attribute(&[]),
                ExtTypeDirective::Role(&[
                    ExtRoleTag {
                        enum_name: "Semantic",
                        variant: "EntryPoint",
                    },
                    ExtRoleTag {
                        enum_name: "cfg.Kind",
                        variant: "Simple",
                    },
                ]),
            ],
            ..ExtStruct::STRUCT_DEFAULTS
        };
        static U: DirUnit = DirUnit("cfg.core", &[KIND], &[OK], &[]);
        validate(&[&U])
            .expect("an `@attribute` + `@role` struct over a `@semantic` variant is legal");
    }

    #[test]
    fn native_role_without_attribute_is_rejected() {
        // `@role` over the always-valid built-in `Semantic`, but the struct is not `@attribute` — the
        // role has nothing to attach to.
        const BAD: ExtStruct = ExtStruct {
            name: "Lonely",
            namespace: "cfg",
            directives: &[ExtTypeDirective::Role(&[ExtRoleTag {
                enum_name: "Semantic",
                variant: "EntryPoint",
            }])],
            ..ExtStruct::STRUCT_DEFAULTS
        };
        static U: DirUnit = DirUnit("cfg.core", &[], &[BAD], &[]);
        assert!(
            validate(&[&U]).is_err(),
            "`@role` without `@attribute` on the same struct must refuse to assemble"
        );
    }

    #[test]
    fn native_role_naming_a_non_semantic_enum_is_rejected() {
        const BAD: ExtStruct = ExtStruct {
            name: "Route",
            namespace: "cfg",
            directives: &[
                ExtTypeDirective::Attribute(&[]),
                ExtTypeDirective::Role(&[ExtRoleTag {
                    enum_name: "cfg.Plain",
                    variant: "A",
                }]),
            ],
            ..ExtStruct::STRUCT_DEFAULTS
        };
        static U: DirUnit = DirUnit("cfg.core", &[PLAIN], &[BAD], &[]);
        assert!(
            validate(&[&U]).is_err(),
            "a `@role` naming a non-`@semantic` enum must refuse to assemble"
        );
    }

    #[test]
    fn native_role_naming_a_fielded_variant_is_rejected() {
        const BAD: ExtStruct = ExtStruct {
            name: "Route",
            namespace: "cfg",
            directives: &[
                ExtTypeDirective::Attribute(&[]),
                ExtTypeDirective::Role(&[ExtRoleTag {
                    enum_name: "cfg.Kind",
                    variant: "Tagged",
                }]),
            ],
            ..ExtStruct::STRUCT_DEFAULTS
        };
        static U: DirUnit = DirUnit("cfg.core", &[KIND], &[BAD], &[]);
        assert!(
            validate(&[&U]).is_err(),
            "a `@role` naming a fielded (non-fieldless) variant must refuse to assemble"
        );
    }

    #[test]
    fn native_role_on_a_class_is_rejected() {
        const BAD: ExtClass = ExtClass {
            name: "Handle",
            namespace: "cfg",
            directives: &[ExtTypeDirective::Role(&[ExtRoleTag {
                enum_name: "Semantic",
                variant: "EntryPoint",
            }])],
            ..ExtClass::DEFAULTS
        };
        static U: DirUnit = DirUnit("cfg.core", &[], &[], &[BAD]);
        assert!(
            validate(&[&U]).is_err(),
            "`@role` is struct-only — a class carrying it must refuse to assemble"
        );
    }

    #[test]
    fn native_role_on_an_enum_is_rejected() {
        const BAD: ExtEnum = ExtEnum {
            name: "Nope",
            namespace: "cfg",
            variants: &[ExtVariant {
                name: "A",
                fields: &[],
                value: VariantValue::None,
            }],
            directives: &[ExtTypeDirective::Role(&[ExtRoleTag {
                enum_name: "Semantic",
                variant: "EntryPoint",
            }])],
            ..ExtEnum::DEFAULTS
        };
        static U: DirUnit = DirUnit("cfg.core", &[BAD], &[], &[]);
        assert!(
            validate(&[&U]).is_err(),
            "`@role` applies only to a struct — an enum carrying it must refuse to assemble"
        );
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

#[cfg(test)]
mod elem_retty {
    //! The scalar-unification ABI additions (element-relative return types + the `AnyNumeric`
    //! constraint). The *resolution* of `Elem`/`ElemWide`/`ElemFloat` against a concrete element
    //! type is the checker's job (exercised end-to-end in
    //! `noeta-embed/tests/ext_constraint_enforcement.rs`); these pin the ABI-level contract this
    //! crate owns — that the variants exist, render symbolically, and stay distinct.

    use super::*;

    #[test]
    fn element_relative_returns_render_symbolically() {
        // No params are referenced — the element type is only known at the bundle's impl site, so a
        // bare signature renders the symbolic element form (like `Var(0)` → `T`).
        assert_eq!(RetTy::Elem.render(&[]), "Elem");
        assert_eq!(RetTy::ElemWide.render(&[]), "ElemWide");
        assert_eq!(RetTy::ElemFloat.render(&[]), "ElemFloat");
    }

    #[test]
    fn any_numeric_is_distinct_from_the_specific_forms() {
        // `AnyNumeric` is additive: it is a new, distinct constraint field, not an alias of any
        // pinned form — the specific `IntN`/`F32`/… keep their exact identity.
        assert_ne!(ConstraintField::AnyNumeric, ConstraintField::F32);
        assert_ne!(ConstraintField::AnyNumeric, ConstraintField::Int);
        assert_ne!(
            ConstraintField::AnyNumeric,
            ConstraintField::IntN {
                bits: 16,
                signed: true
            }
        );
    }

    #[test]
    fn an_element_relative_bundle_signature_renders() {
        // A whole `dot(dyn): ElemWide` bundle method renders end-to-end — the signature surface a
        // unified `vec.Kernels` will expose.
        let f = ExtFn {
            name: "dot",
            params: &[SigType::Dyn],
            ret: RetTy::ElemWide,
        };
        assert_eq!(f.render(), "fn dot(dyn): ElemWide");
    }
}
