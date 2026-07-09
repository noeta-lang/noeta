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
    /// A value-struct instance built by a call-site type recipe (`json.parse::<T>`): the type name
    /// and its `(field, value)` pairs **in the type's declared order**. Unlike [`NativeOut::Object`]
    /// — whose shape is supplied from an argument via [`RetTy::SameAsArg`] — a `Struct` names its own
    /// type, so the backend builds the instance by name (the tree-walker through its real registered
    /// definition, so methods/defaults match a normal literal; the VM through a fresh same-name shape,
    /// as reflection already does). Field values are themselves `NativeOut`, so nesting recurses.
    Struct {
        name: String,
        fields: Vec<(String, NativeOut)>,
    },
    /// A string-keyed map (a JSON object decoded under a `Map` recipe), entries in key order.
    Map(Vec<(String, NativeOut)>),
    /// `Option::None` — an absent optional field, or a JSON `null` decoded under an `Option` recipe.
    None,
    /// `Option::Some(x)` — a present optional value.
    Some(Box<NativeOut>),
    /// A registered extern-type value (extern-types X1) — `Uuid`, a `FileHandle`, … Each
    /// backend wraps it in its single extern hosting variant.
    Extern(crate::ExternBox),
    /// Async WORK instead of a value (extern-types X5): the backend tickets the descriptor on
    /// its executor (`spawn_ext`) and hands back a future — intercepted at the dispatch return,
    /// never reaching `materialize`. This is how an extension implements an async function
    /// without ever seeing the executor.
    Spawn(SpawnBox),
}

/// A one-shot [`crate::ExternIo`] carrier inside [`NativeOut`] (which derives `Clone` +
/// `PartialEq` for its value variants — meaningless for work): cloning panics (a descriptor is
/// ticketed exactly once, on the dispatch return path), equality is always `false`.
#[derive(Debug)]
pub struct SpawnBox(pub Box<dyn crate::ExternIo>);

impl Clone for SpawnBox {
    fn clone(&self) -> SpawnBox {
        unreachable!("a Spawn result is one-shot — ticketed at the dispatch return, never cloned")
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
    /// threads into the dispatch (call-site-typed construction).
    TypeArg,
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
    },
}

/// One native function's static signature (for the checker and tooling). Dispatch is per-module
/// (matching on the function name), so an `ExtFn` carries no dispatch pointer of its own.
#[derive(Debug, Clone, Copy)]
pub struct ExtFn {
    pub name: &'static str,
    pub params: &'static [SigType],
    pub ret: RetTy,
}

/// A module's dispatch: given the function name, the host seam, and the projected arguments, run
/// the function and return a neutral result (or a misuse error). One per module, mirroring the
/// existing `call(func, args)` shape.
pub type ModuleDispatch =
    fn(func: &str, host: &mut dyn Host, args: &[NativeValue]) -> Result<NativeOut, StdError>;

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
/// with the bare nominal name only.
#[derive(Debug, Clone, Copy)]
pub struct ExtType {
    /// The surface type name (`Uuid`). Reserved: a user declaration of this name is E0049.
    pub name: &'static str,
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
}

impl ExtType {
    /// Literal-shortening defaults (`..ExtType::DEFAULTS`), mirroring [`ExtModule::DEFAULTS`]:
    /// a plain-data extern type declares no higher-order surface.
    pub const DEFAULTS: ExtType = ExtType {
        name: "",
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
    };
}

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
static INSTALLED: OnceLock<Vec<&'static (dyn Extension + Sync)>> = OnceLock::new();

/// Install the binary's complete extension-unit list — callable **once**, before any lookup.
///
/// Uniqueness rules (a violation is a `panic`, not an `Err` — a mis-assembled binary must not
/// start): extension **names** are unique (`"std.http"`), and **qualified module identities**
/// (`root() + "." + module.name`) are unique across units. Roots are deliberately shared — the six
/// std units all root `"std"`.
///
/// Panics if something was already installed (including the lazy std default — install before the
/// first lookup, or the assembly raced a lookup and the binary is misbuilt).
pub fn install(units: Vec<&'static (dyn Extension + Sync)>) {
    validate(&units);
    if INSTALLED.set(units).is_err() {
        panic!(
            "extension registry already installed — `install` must run once, before any lookup \
             (a lookup through the std facade lazily installs the default units)"
        );
    }
}

/// Install `provider()`'s units only if nothing is installed yet — the lazy-default seam the
/// `noeta-stdlib::registry` facade uses so existing call sites (backends, checker, tests) never
/// observe an empty registry, while an explicit earlier [`install`] (the composed shim) wins.
pub fn install_default(provider: fn() -> Vec<&'static (dyn Extension + Sync)>) {
    INSTALLED.get_or_init(|| {
        let units = provider();
        validate(&units);
        units
    });
}

/// The uniqueness sweep behind [`install`]/[`install_default`] — O(n²) over a handful of units.
fn validate(units: &[&'static (dyn Extension + Sync)]) {
    for (i, unit) in units.iter().enumerate() {
        for other in &units[i + 1..] {
            assert!(
                unit.name() != other.name(),
                "duplicate extension unit name `{}` in the assembled registry",
                unit.name()
            );
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
        assert!(
            pair[0] != pair[1],
            "duplicate qualified module `{}` in the assembled registry",
            pair[0]
        );
    }
}

/// All installed extension units (empty before [`install`]/[`install_default`] — callers outside
/// the std facade own their seeding).
pub fn extensions() -> &'static [&'static (dyn Extension + Sync)] {
    INSTALLED.get().map_or(&[], |v| v.as_slice())
}

/// Find a registered module by its identity string — a **root-qualified path** (`"std.math"`,
/// nested `"std.http.client"`) or a bare module name (`"math"`, from tests / legacy literal calls).
/// A leading segment that names a registered extension root selects that root and matches the
/// remainder against the module name; otherwise the whole string is matched as a bare name.
pub fn find_module(name: &str) -> Option<&'static ExtModule> {
    if let Some((root, module)) = name.split_once('.')
        && is_extension_root(root)
    {
        return extensions()
            .iter()
            .filter(|e| e.root() == root)
            .flat_map(|e| e.modules())
            .find(|m| m.name == module);
    }
    extensions()
        .iter()
        .flat_map(|e| e.modules())
        .find(|m| m.name == name)
}

/// The registered module name of a (possibly root-qualified) module identity — the identity with
/// its extension root stripped: `"std.vec"` → `"vec"`, `"std.http.client"` → `"http.client"`, bare
/// `"vec"` → `"vec"`. This is the `ExtModule::name` the identity resolves to.
pub fn module_name(module: &str) -> &str {
    module.split_once('.').map_or(module, |(_root, name)| name)
}

/// The native-dependency **ring** a module identity resolves to, or `None` for always-on core
/// (package-manager P1.0). The registry-backed source of truth for the AOT footprint scan's
/// Cargo-feature selection. An unrecognized identity is `None` (conservative: never strips a ring
/// for a module the registry doesn't own).
pub fn ring_of(module: &str) -> Option<&'static str> {
    find_module(module).and_then(|m| m.ring)
}

/// Whether `root` is the namespace root of some registered extension (`"std"`, a composed
/// package's root). A `use <root>.…` import binds a native module iff this holds.
pub fn is_extension_root(root: &str) -> bool {
    extensions().iter().any(|e| e.root() == root)
}

/// Find a registered module by its **fully qualified path** — `["std", "math"]`, or nested
/// `["std", "http", "client"]`. The first segment selects the extension by [root]; the remainder,
/// dot-joined, matches the module's registered name. Two extensions with distinct roots never
/// collide (`std.http` ≠ `guzzle.http`).
///
/// [root]: Extension::root
pub fn find_module_qualified(path: &[String]) -> Option<&'static ExtModule> {
    let (root, rest) = path.split_first()?;
    if rest.is_empty() {
        return None;
    }
    let module_name = rest.join(".");
    extensions()
        .iter()
        .filter(|e| e.root() == root.as_str())
        .flat_map(|e| e.modules())
        .find(|m| m.name == module_name.as_str())
}

/// Find a registered function's signature.
pub fn find_function(module: &str, func: &str) -> Option<&'static ExtFn> {
    find_module(module)?
        .functions
        .iter()
        .find(|f| f.name == func)
}

/// Find a registered **higher-order** function's signature (higher-order-abi H0) — the ctx-table
/// twin of [`find_function`]. The backends route a matched name through the `NativeCtx` seam.
pub fn find_ctx_function(module: &str, func: &str) -> Option<&'static ExtFn> {
    find_module(module)?
        .ctx_functions
        .iter()
        .find(|f| f.name == func)
}

/// A function's signature from **either** table — what the checker and name resolution consult
/// (they don't care how a call dispatches, only that the name exists and what it types as).
pub fn find_function_sig(module: &str, func: &str) -> Option<&'static ExtFn> {
    find_function(module, func).or_else(|| find_ctx_function(module, func))
}

/// Dispatch a registered higher-order function through the module's [`crate::CtxDispatch`]
/// (higher-order-abi H0). Mirrors [`dispatch`] for the ctx table.
pub fn dispatch_ctx(
    module: &str,
    func: &str,
    ctx: &mut dyn crate::NativeCtx,
    args: &[crate::Slot],
) -> Result<crate::CtxOut, crate::CtxError> {
    match find_module(module).and_then(|m| m.ctx_dispatch) {
        Some(d) => d(func, ctx, args),
        None => Err(crate::no_function_error(module, func).into()),
    }
}

/// Every extension-contributed CLI subcommand (higher-order-abi H6), for the CLI's dynamic
/// wiring and its unmatched-name dispatch.
pub fn commands() -> impl Iterator<Item = &'static crate::ExtCommand> {
    extensions().iter().flat_map(|e| e.commands())
}

/// Find a registered extern type by name (extern-types X1).
pub fn find_type(name: &str) -> Option<&'static ExtType> {
    extensions()
        .iter()
        .flat_map(|e| e.types())
        .find(|t| t.name == name)
}

/// Find a registered extern type's method signature.
pub fn find_type_method(type_name: &str, method: &str) -> Option<&'static ExtFn> {
    find_type(type_name)?
        .methods
        .iter()
        .find(|m| m.name == method)
}

/// Find a registered extern type's **higher-order** method signature (higher-order-abi H4) —
/// methods that dispatch through the ctx seam ([`ExtType::ctx_dispatch`]).
pub fn find_type_ctx_method(type_name: &str, method: &str) -> Option<&'static ExtFn> {
    find_type(type_name)?
        .ctx_methods
        .iter()
        .find(|m| m.name == method)
}

/// A type method's signature from **either** table — what the checker consults (it doesn't care
/// how a call dispatches). The type-method twin of [`find_function_sig`].
pub fn find_type_method_sig(type_name: &str, method: &str) -> Option<&'static ExtFn> {
    find_type_method(type_name, method).or_else(|| find_type_ctx_method(type_name, method))
}

/// Route a **higher-order** method call to its type's ctx dispatch (higher-order-abi H4) — the
/// type-method twin of [`dispatch_ctx`].
pub fn dispatch_ctx_method(
    type_name: &str,
    method: &str,
    ctx: &mut dyn crate::NativeCtx,
    recv: crate::Slot,
    args: &[crate::Slot],
) -> Result<crate::CtxOut, crate::CtxError> {
    match find_type(type_name).and_then(|t| t.ctx_dispatch) {
        Some(d) => d(method, ctx, recv, args),
        None => Err(crate::no_method_error(type_name, method).into()),
    }
}

/// Dispatch a method on an extern receiver through its registered [`ExtType`]. Returns the
/// canonical "no such method" error for an unknown method, mirroring [`dispatch`] for modules.
pub fn dispatch_method(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    host: &mut dyn Host,
    args: &[crate::NativeValue],
) -> Result<crate::NativeOut, StdError> {
    let type_name = recv.type_name();
    let Some(ext) = find_type(type_name) else {
        return Err(StdError {
            kind: crate::ErrorKind::UnknownName,
            message: format!("`{type_name}` is not a registered type"),
        });
    };
    (ext.dispatch)(recv, method, host, args)
}

/// Dispatch a registered module function. Returns the canonical "no such function" error if the
/// module is unknown (the backends only ever dispatch a name they bound, so that is unreachable
/// in practice).
pub fn dispatch(
    module: &str,
    func: &str,
    host: &mut dyn Host,
    args: &[crate::NativeValue],
) -> Result<crate::NativeOut, StdError> {
    match find_module(module) {
        Some(m) => (m.dispatch)(func, host, args),
        None => Err(crate::no_function_error(module, func)),
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

    #[test]
    fn duplicate_unit_name_is_rejected() {
        let result = std::panic::catch_unwind(|| validate(&[&A, &B_DUP_NAME]));
        assert!(result.is_err(), "duplicate unit name must panic");
    }

    #[test]
    fn duplicate_qualified_module_is_rejected() {
        // Same root (`a`) + same module name (`math`) across two differently-named units.
        let result = std::panic::catch_unwind(|| validate(&[&A, &B_DUP_MODULE]));
        assert!(result.is_err(), "duplicate qualified module must panic");
    }

    #[test]
    fn shared_root_across_units_is_fine() {
        // The std pattern: six units all rooted `std`. Distinct names, distinct modules.
        validate(&[&A, &A2]);
    }

    // One test drives the whole process-global lifecycle (the `OnceLock` is per-process, so
    // ordering across #[test] threads would race if split up).
    #[test]
    fn install_lifecycle() {
        assert!(extensions().is_empty(), "nothing installed at startup");
        install_default(|| vec![&A]);
        assert_eq!(extensions().len(), 1);
        assert!(find_module("a.math").is_some());
        assert!(find_module("math").is_some(), "bare-name lookup");
        // A second default is a no-op — the first install wins.
        install_default(|| vec![&A, &A2]);
        assert_eq!(extensions().len(), 1);
        // An explicit install after anything is installed is a hard error.
        let result = std::panic::catch_unwind(|| install(vec![&A2]));
        assert!(result.is_err(), "install after install_default must panic");
    }
}
