//! The bytecode compiler: **Core IR → [`Module`]**.
//!
//! Since the memory-management migration's Phase 2 the compiler lowers the shared
//! [A-normal-form Core IR][noeta_ir] both backends run, not the surface AST: [`compile`] first
//! lowers the parsed program to `noeta_ir` (the same lowering the IR interpreter consumes), then
//! emits bytecode from it. Because the IR has already named every intermediate value and fixed
//! evaluation order, the compiler is a near-1:1 structural lowering — an IR `let v = a + b`
//! becomes an `Op::Binary`, a constructor an `Op::MakeStruct`/`MakeEnum`, an IR `if` a branch —
//! rather than re-deriving order by recursively flattening nested expressions. Type
//! registration (shapes, the method/destructor proto table) still reads the surface declarations
//! the IR carries verbatim.
//!
//! The compiler covers the **whole** language: every program that parses and type-checks
//! compiles to bytecode — the conformance differential (`--differential`, 0 skipped) holds the
//! VM at 100% coverage against the Core-IR reference interpreter by construction, and an
//! [`Unsupported`] from this crate is an internal invariant break the callers surface loudly,
//! never a silently-skipped construct. Closures capture via **upvalues** (see [`freevars`]);
//! string interpolation, `match`/`?`/`??`, the object model on shapes, and `...spread` all
//! lower here.
//!
//! ## The scope model
//!
//! The reference interpreter resolves names through a chain of reference-counted lexical
//! scopes. The VM splits that into three tiers:
//!
//! - **Globals.** Every top-level binding and `fn` name lives in a runtime global table,
//!   read/written by name (`LoadGlobal`/`StoreGlobal`). A top-level function's free variables
//!   resolve here at call time — faithful, because the reference interpreter's captured scope
//!   for a top-level function *is* the (shared, mutable) global scope, so reads see live values.
//! - **Frame-locals.** Parameters and locals live in registers, one register file per call
//!   frame. Block scopes (`if`/`else` bodies) nest within the same register file.
//! - **Upvalues.** A local captured by an inner closure is boxed into a heap *cell* shared
//!   between the defining frame and every capturing closure, so a closure reads (and mutates)
//!   the live binding — matching the reference interpreter's `Rc`-captured scope chain. The
//!   free-variable analysis in [`freevars`] decides which locals are celled and lays out each
//!   closure's ordered upvalues; the closure carries the cells (`MakeClosure` captures), and the
//!   body reaches them with `UpvalueGet`/`UpvalueSet`.
//!
//! The compiler stays faithful to the reference interpreter's evaluation order and exact
//! diagnostic text/spans, because the differential oracle compares full `RunResult`s.
//! Registers are emitted against a virtual (monotonic) numbering, then compacted by the
//! graph-coloring allocator in [`regalloc`] — see that module's header for its three safety
//! invariants.

use std::collections::{HashMap, HashSet};

use noeta_ast::{BinaryOp, Program, TypeRef};
use noeta_builtins::PRELUDE_NAMES;
use noeta_bytecode::{
    BoolSide, Builtin, CaptureFrom, Chunk, Const, GlobalId, LineEntry, LocalDebug, MethodEntry,
    Module, NameId, NarrowTarget, Op, Reg, ReuseCheck, StrPart,
};
use noeta_ir::{
    Atom, Block, Const as IrConst, Decl, ForPattern, Func, InterpPart, Pattern, Rvalue, Stmt, Temp,
    Thunk,
};

mod freevars;
pub mod hotswap;
mod regalloc;
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
// Re-exported so the VM session API can name the checker's compile-input bundle through its
// existing compiler dependency (noeta-vm deliberately has no direct noeta-check dependency).
pub use noeta_check::Sites;
use noeta_object::{Shape, ShapeKind};
use noeta_span::Span;

/// Why a program could not be lowered to bytecode yet — a node outside the current subset.
/// The differential harness treats this as "skip", not "fail".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    pub reason: String,
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unsupported by the VM: {}", self.reason)
    }
}

fn unsupported<T>(reason: impl Into<String>) -> Result<T, Unsupported> {
    Err(Unsupported {
        reason: reason.into(),
    })
}

/// Whether `use <path>.{name}` binds a native value (a module — `use std.{json}`, nested
/// `use std.http.client` — or a selectively-imported member fn — `use std.math.sqrt`) rather than
/// a sibling-module declaration. Classification is `Registry::classify_use` — the ONE source of
/// truth the checker and IDE already consume (this crate used to re-derive it with private
/// helpers, the third copy of the rules; a new import shape then had to be taught to three
/// matchers, and a miss diverged checker-accepted programs from backend binding).
fn binds_native_value(
    reg: &noeta_ext_abi::registry::Registry,
    path: &[String],
    name: &str,
) -> bool {
    matches!(
        reg.classify_use(path, name),
        noeta_ext_abi::registry::UseKind::Module(_)
            | noeta_ext_abi::registry::UseKind::MemberFn { .. }
    )
}

/// Build the compiler's constructible-type record for a native-declared enum (native-extensibility
/// S1b), so `Hue.Red` / `Hue.Labeled(x)` lower to `MakeEnum` exactly like a `.noe` enum. Variant
/// declaration order — hence each variant's index — comes straight from the [`ExtEnum`], matching
/// the index a native-returned variant carries and the tree-walker's `EnumDef`. A payload variant's
/// field names are synthesized positionally (`_0`, `_1`, …): only their **count** is load-bearing
/// (it gates the payload-vs-fieldless distinction in `lower_field` and the `MakeEnum` arg count),
/// and enum equality/matching compare by name + variant + arity, never by field name — so this
/// stays identical to the native-return path's empty-name shape.
fn ext_enum_type_info(en: &noeta_ext_abi::registry::ExtEnum) -> TypeInfo {
    let variants = en
        .variants
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                v.name.to_string(),
                VariantSlots {
                    index: i as u32,
                    fields: (0..v.fields.len()).map(|n| format!("_{n}")).collect(),
                },
            )
        })
        .collect();
    TypeInfo::Enum {
        variants,
        fns: HashMap::new(),
    }
}

/// Build the compiler's constructible-type record for a native-declared **fielded type** (a class
/// or a value struct), so `Point { x: 1, y: 2 }` lowers to `MakeStruct` with the right-kind shape
/// exactly like a `.noe` class/struct. Field order comes straight from the [`ExtFielded`] (the same
/// order the checker seeds and a native `NativeOut::Instance` supplies), so a source-constructed
/// instance and a native-constructed one share the layout and interchange. The [`FieldedKind`]
/// discriminant selects the shape kind: a **class** gets a class-kind shape (reference identity,
/// destructor via its extern-handle field's Rust `Drop`); a **struct** gets a struct-kind shape, so
/// the object model derives structural equality and value semantics automatically. A native fielded
/// type declares no methods here and no `.noe` `destruct` block.
fn ext_fielded_type_info(cl: &noeta_ext_abi::registry::ExtFielded) -> TypeInfo {
    let fields = cl.fields.iter().map(|f| f.name.to_string()).collect();
    match cl.kind {
        noeta_ext_abi::FieldedKind::Class => TypeInfo::Class {
            fields,
            fns: HashMap::new(),
        },
        noeta_ext_abi::FieldedKind::Struct => TypeInfo::Struct {
            fields,
            fns: HashMap::new(),
        },
    }
}

/// Everything that varies a checked compile, so callers configure one entry point
/// ([`compile_with`] / [`compile_session_with`]) instead of this family growing a
/// `_with_isolates_and_debug_and_registry` combinatorial tail (audit-3 finding 9 — the
/// checker's own [`CheckOptions`] lesson, propagated). `Default` is the CLI/salsa/differential
/// path: cooperative isolates, no debug info, process-global registry.
///
/// [`CheckOptions`]: noeta_check::CheckOptions
pub struct CompileOptions {
    /// Whether `isolate f(args)` lowers to `Rvalue::SpawnIsolate` (real OS-thread path, I.4b).
    /// Only the CLI's real (VM) execution passes true; the differential/salsa keep false
    /// (byte-identical sandbox).
    pub real_isolates: bool,
    /// Emit per-prototype debug info (reg→name locals, function names, defining spans) and pin
    /// named locals through coalescing so the map stays 1:1. Only `noeta dap` passes true; the
    /// CLI, salsa, and the differential pass false (no debug info, unconstrained coalescing —
    /// goldens unchanged).
    pub debug: bool,
    /// The extension registry `use`-import lowering and native-type narrowing resolve against
    /// (instance-registry IR5). The production/CLI path keeps the process-global default; an
    /// embed session threads its own assembled set.
    pub registry: &'static noeta_ext_abi::registry::Registry,
}

impl Default for CompileOptions {
    fn default() -> Self {
        CompileOptions {
            real_isolates: false,
            debug: false,
            registry: noeta_ext_abi::registry::single_registry_process(),
        }
    }
}

impl std::fmt::Debug for CompileOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompileOptions")
            .field("real_isolates", &self.real_isolates)
            .field("debug", &self.debug)
            // The registry is a `&'static` handle whose contents aren't `Debug`.
            .finish_non_exhaustive()
    }
}

/// Compile a whole program to a [`Module`], or report the first unsupported construct.
///
/// Three passes: (1) register every top-level type so forward references resolve and shapes
/// exist before any body is compiled; (2) compile each class method/associated function into
/// its reserved prototype; (3) compile the top-level program. Splitting (1) from (3) mirrors
/// the tree-walker, whose type declarations are all evaluated before the driver code runs.
pub fn compile(program: &Program) -> Result<Module, Unsupported> {
    let checked = noeta_check::check_all(program);
    compile_with(program, checked.sites, CompileOptions::default())
}

/// Convert the checker's destructor-relevance into the drop pass's form (identical sets; the two
/// crates keep separate types so `noeta-ir-passes` needs no checker dependency).
fn passes_relevance(r: &noeta_check::DestructorRelevance) -> noeta_ir_passes::Relevance {
    noeta_ir_passes::Relevance {
        locals: r.locals.clone(),
        params: r.params.clone(),
    }
}

/// Compile a program using a **precomputed** checker [`Sites`] bundle instead of re-deriving it.
///
/// [`compile`] re-runs the checker to obtain the bundle, which on a path that already type-checked
/// the program — the CLI, the `noeta-db` `bytecode` query, the differential harness — means a
/// redundant checker run. An orchestrator that already holds a [`noeta_check::Checked`] threads
/// `checked.sites` here so the checker runs only once. The bundle is a pure function of the
/// program, so this is behavior-identical to [`compile`].
///
/// [`Sites`]: noeta_check::Sites
pub fn compile_with_sites(
    program: &Program,
    sites: noeta_check::Sites,
    real_isolates: bool,
    debug: bool,
) -> Result<Module, Unsupported> {
    // The CLI/salsa/differential compile path is single-registry — the default registry.
    compile_with(
        program,
        sites,
        CompileOptions {
            real_isolates,
            debug,
            ..CompileOptions::default()
        },
    )
}

/// As [`compile_with_sites`], but against explicit [`CompileOptions`] — the single configurable
/// batch-compile entry the positional-flag functions are thin presets of.
pub fn compile_with(
    program: &Program,
    sites: noeta_check::Sites,
    opts: CompileOptions,
) -> Result<Module, Unsupported> {
    let relevance = Some(passes_relevance(&sites.destructor_relevance));
    let destruct_reachable = sites
        .destructor_relevance
        .reachable_types
        .iter()
        .cloned()
        .collect();
    compile_inner(program, sites, relevance, destruct_reachable, opts)
}

// Threads the checker's site bundle plus the pre-converted relevance through to the IR lowering,
// then MOVES the compiler's tables into the `Module` (the production finisher — no clone tax on
// `noeta run`). The session path (`compile_with_sites_session`) shares `compile_to_mc` and keeps
// the compiler alive instead.
fn compile_inner(
    program: &Program,
    sites: noeta_check::Sites,
    relevance: Option<noeta_ir_passes::Relevance>,
    destruct_reachable: Vec<String>,
    opts: CompileOptions,
) -> Result<Module, Unsupported> {
    let native_roles = opts.registry.native_roles();
    let native_traits = noeta_ir::native_trait_impls(opts.registry);
    let mut reflection = noeta_ast::reflect::build(program, &native_roles, &native_traits);
    // Embed the installed extensions' attribute shapes (tier-extensions port): `attributes_of`
    // materializes `#[Skip]`/`#[Bench]`/… from the artifact, and their declarations live in the
    // registry now, not the AST.
    noeta_check::extend_reflection(&mut reflection);
    let (module, map_packed_sites) = compile_to_mc(program, sites, relevance, opts)?;
    Ok(Module {
        protos: module.protos,
        shapes: module.shapes,
        packed_schemas: module.packed_schemas,
        map_packed_sites,
        methods: module.methods,
        destructors: module.destructors,
        field_defaults: module.field_defaults,
        comparable_derives: module.comparable_derives,
        tojson_derives: module.tojson_derives,
        deserialize_recipes: module.deserialize_recipes,
        type_args: module.type_args,
        destruct_reachable,
        cache_slots: module.cache_slots,
        // The attribute manifest + type registry, built from the AST by the *same* pure builder the
        // tree-walker uses — so reflection is identical across backends by construction.
        reflection,
        type_reprs: module.type_reprs,
        names: module.names,
        // Map each top-level value binding to its slot — debug compiles only, so a release module
        // carries none (goldens/bundles unchanged). Its slot exists because the binding emits a
        // `StoreGlobal`; a binding the checker proved dead and elided simply has no slot and drops.
        global_bindings: if module.debug {
            module
                .module_binding_names
                .iter()
                .filter_map(|name| module.global_slots.get(name).copied().map(GlobalId))
                .collect()
        } else {
            Vec::new()
        },
        global_names: module.global_names,
    })
}

/// Compile a whole **checked** program and keep the compiler alive as a [`SessionCompiler`] — the
/// debug console's seed (tooling-unification T3). The returned session's tables *are* the module's
/// own id-spaces (proto indices, global slots, shapes, interned names), so a later
/// [`SessionCompiler::extend`] appends a fragment onto the running program exactly as a REPL entry
/// appends onto a session: stable prefix, new ids at the end. The initial compile is fully checked
/// (identical to [`compile_with_sites`] — same lowering, drops, debug info); only the *fragments*
/// compiled through `extend` are checkerless, matching the REPL's stance.
pub fn compile_with_sites_session(
    program: &Program,
    sites: noeta_check::Sites,
    real_isolates: bool,
    debug: bool,
) -> Result<(Module, SessionCompiler), Unsupported> {
    // The default (CLI/REPL/dap) session compile is single-registry — the default registry.
    compile_session_with(
        program,
        sites,
        CompileOptions {
            real_isolates,
            debug,
            ..CompileOptions::default()
        },
    )
}

/// As [`compile_with_sites_session`], but resolving native names against an explicit `registry`
/// (instance-registry IR5) — the seam an embedding host with its own assembled extension set uses so
/// its session's compile binds `use` imports and narrowing targets against *its* extensions, not the
/// process-global default. The default entry point above passes `default_seeded()`, so every CLI /
/// REPL / debug-adapter caller is unchanged.
pub fn compile_with_sites_session_with_registry(
    program: &Program,
    sites: noeta_check::Sites,
    real_isolates: bool,
    debug: bool,
    registry: &'static noeta_ext_abi::registry::Registry,
) -> Result<(Module, SessionCompiler), Unsupported> {
    compile_session_with(
        program,
        sites,
        CompileOptions {
            real_isolates,
            debug,
            registry,
        },
    )
}

/// As [`compile_with_sites_session`], but against explicit [`CompileOptions`] — the single
/// configurable session-compile entry the positional-flag functions are thin presets of.
pub fn compile_session_with(
    program: &Program,
    sites: noeta_check::Sites,
    opts: CompileOptions,
) -> Result<(Module, SessionCompiler), Unsupported> {
    let relevance = Some(passes_relevance(&sites.destructor_relevance));
    let destruct_reachable: Vec<String> = sites
        .destructor_relevance
        .reachable_types
        .iter()
        .cloned()
        .collect();
    let native_roles = opts.registry.native_roles();
    let native_traits = noeta_ir::native_trait_impls(opts.registry);
    let mut reflection = noeta_ast::reflect::build(program, &native_roles, &native_traits);
    // Embed the installed extensions' attribute shapes (tier-extensions port): `attributes_of`
    // materializes `#[Skip]`/`#[Bench]`/… from the artifact, and their declarations live in the
    // registry now, not the AST.
    noeta_check::extend_reflection(&mut reflection);
    let (mc, map_packed_sites) = compile_to_mc(program, sites, relevance, opts)?;
    let session = SessionCompiler {
        mc,
        // The launch compile's own map-packed pairs join the session accumulation: a program
        // function's `map(...)` resolves its span at run time, so every later snapshot must
        // carry them (session-checker C5).
        map_packed: map_packed_sites.clone(),
        reflection,
    };
    let module = session.snapshot(map_packed_sites, destruct_reachable);
    Ok((module, session))
}

// The core of the checked compile: lower the program (with the checker's site maps) and compile it
// into a live [`ModuleCompiler`], returning the compiler plus the interned `map(...)`-result packed
// pairs. Shared by [`compile_inner`] (which moves the tables into a `Module`) and
// [`compile_with_sites_session`] (which keeps the compiler alive and snapshots).
fn compile_to_mc(
    program: &Program,
    sites: noeta_check::Sites,
    relevance: Option<noeta_ir_passes::Relevance>,
    // What varies the compile (isolate lowering, debug info, extension registry) — see the field
    // docs on [`CompileOptions`]. `debug` is threaded onto `ModuleCompiler` and read at
    // `into_chunk`/`declare_local`; the registry is stored there too and passed to lowering.
    opts: CompileOptions,
) -> Result<(ModuleCompiler, Vec<(Span, u32)>), Unsupported> {
    let CompileOptions {
        real_isolates,
        debug,
        registry,
    } = opts;
    // `sites` stays whole through lowering (`lowering_sites!` is THE one projection); the owned
    // maps the compiler keeps (`type_of`, `map_packed`, `deserialize_recipes`) move out afterwards.

    // Hoist standalone-`impl` methods onto their target type (L1 user traits, UT2) so the surface
    // pass-1 (`register_types`) and the IR pass-2 (`compile_methods`) agree on the method set. The
    // helper is idempotent, so the lowering below re-hoisting is a no-op; rebinds only when such an
    // impl exists.
    //
    // Against **this compile's** registry, not the process default: a native derive recipe
    // (`ExtDerive`, derive layer 4) is what the hoist materializes, and an embed session's own
    // extensions live only in its assembled registry. Resolving against the default instead meant
    // the checker accepted `@derive(<session recipe>)` — it reads the session registry — while the
    // hoist synthesized nothing, and `compile_methods` then panicked on the missing prototype
    // ("no entry found for key"). The registry-threaded entry point existed for exactly this and
    // had no caller.
    let hoisted = noeta_ir::hoist_impl_methods_with_registry(program, Some(registry));
    let program: &Program = hoisted.as_ref().unwrap_or(program);
    // Lower the surface program to the shared Core IR, then compile *that* to bytecode. The same
    // lowering the IR interpreter consumes, so both backends execute one program (Phase 2). The
    // precise-RC drop-insertion pass (Phase 3) annotates the IR with `DropVar`s at last-use death
    // points; they lower to plain releases (prompt reclamation, no destructor) so this is
    // behavior-neutral, reclaiming a local's value at its last use instead of at frame teardown.
    // Lower with the checker's site maps: the `List<packed>` map streams packed-list literals into a
    // flat buffer (P-PACK 2.5; the resolved layout rides on the IR rvalue, so the bytecode compiler
    // reads it from there at `PackedListNew` and needs no separate span map of its own), and the
    // index-field set fuses `list[i].field` reads into `Rvalue::IndexField` (P-PACK 2.5+).
    let ir = noeta_ir::lower_with_sites_opts(
        program,
        noeta_ir::lowering_sites!(sites),
        noeta_ir::LowerOptions {
            real_isolates,
            registry,
        },
    )
    .map_err(|u| Unsupported {
        reason: format!("not yet lowered to the Core IR: {}", u.feature),
    })?;
    let ir = noeta_ir_passes::insert_drops(&ir, relevance.as_ref());
    // Thread in-place-reuse tokens (Phase 5) onto self-update constructors. A pure function of the
    // drop-annotated IR, run identically by the IR interpreter (`reference_run`), so both backends
    // reuse at the same points by construction.
    let ir = noeta_ir_passes::thread_reuse(&ir);
    let mut module = ModuleCompiler {
        protos: vec![Chunk::placeholder()],
        shapes: Vec::new(),
        packed_schemas: Vec::new(),
        methods: Vec::new(),
        destructors: Vec::new(),
        field_defaults: Vec::new(),
        comparable_derives: Vec::new(),
        tojson_derives: Vec::new(),
        deserialize_recipes: sites.deserialize_recipes,
        type_args: ir.type_args.clone(),
        structural_eq_types: HashSet::new(),
        packed_fields: HashMap::new(),
        key_capable_types: HashSet::new(),
        types: HashMap::new(),
        module_globals: HashMap::new(),
        module_fns: HashSet::new(),
        module_binding_names: Vec::new(),
        type_of_sites: sites.type_of_sites,
        cache_slots: 0,
        type_reprs: Vec::new(),
        names: Vec::new(),
        name_ids: HashMap::new(),
        global_names: Vec::new(),
        global_slots: HashMap::new(),
        debug,
        registry,
    };
    // Type registration reads the surface declarations (shapes, derives, the method/destructor
    // proto table) the IR carries verbatim; bodies are lowered from the IR.
    module.register_globals(program);
    module.register_types(program);
    module.compile_methods(&ir)?;
    let main = {
        let mut fc = FnCompiler::new(&mut module, true, None, Vec::new(), Vec::new());
        fc.init_temps(ir.temp_count);
        fc.setup_main_scopes(&ir.top);
        for stmt in &ir.top.stmts {
            fc.stmt(stmt)?;
        }
        fc.code.push(Op::Halt);
        fc.into_chunk(0, Vec::new(), Some("main".to_string()), Some(ir.span))
    };
    module.protos[0] = main;
    // Intern each packed `map(...)` result layout (P-PACK 2.6 category B) and pair it with the call
    // span the VM's `map` builtin keys on. Sorted by span first so schema interning order — and thus
    // the `packed_schemas` table — is deterministic regardless of the `HashMap`'s iteration order.
    let map_packed_sites = {
        let mut entries: Vec<(Span, &noeta_ast::reflect::PackedLayout)> = sites
            .map_packed_sites
            .iter()
            .map(|(s, l)| (*s, l))
            .collect();
        entries.sort_by_key(|(s, _)| (s.source, s.start, s.end));
        entries
            .into_iter()
            .map(|(span, layout)| (span, module.intern_packed_schema(layout)))
            .collect()
    };
    // Intern every `vec`-bundle-bound type's schema (scalar-unification slice 3) so the VM has the
    // element width even for a type that never appears in a `List<T>`; deduplicated by
    // `intern_packed_schema`, so a type already used in a packed list adds nothing.
    for layout in &sites.bundle_schema_layouts {
        module.intern_packed_schema(layout);
    }
    // Intern EVERY `@packed` struct's layout unconditionally (native type-declaration unification,
    // Slice E2) — the from-scratch producer's schema-availability channel. A native fn's
    // `NativeCtx::make_packed(type_name, …)` resolves the produced `List<packed>`'s element schema by
    // matching the interned schema's shape name, so a native `@packed` struct must be present even
    // when it never appears in a source `List<T>` literal (the `like`-less case). Deduplicated by
    // `intern_packed_schema` (a type already interned by a list literal or a bundle binding adds
    // nothing) and pre-sorted by name upstream, so the table stays deterministic.
    for layout in &sites.packed_type_layouts {
        module.intern_packed_schema(layout);
    }
    Ok((module, map_packed_sites))
}

/// A persistent, incremental compiler for a REPL session (REPL-on-VM). Where [`compile`] builds a
/// fresh [`Module`] from a whole program and consumes its tables, this keeps the compile tables
/// **alive across entries** so the proto indices, global slots, shapes, and method-table entries an
/// entry assigns stay valid in the next — the *stable-id accumulation* the session's cross-entry
/// object identity depends on (a closure holds a raw `proto`, an aggregate an `Rc<Shape>`; both would
/// be corrupted by recompiling from scratch each entry).
///
/// **Checkerless**, matching the tree-walker REPL it replaces: no type errors surface at the prompt,
/// drops are conservatively destructor-relevant (`insert_drops(_, None)`), and every declared type is
/// treated as possibly destructor-bearing. Lowering is total over parsed programs, so a
/// successfully-parsed entry always compiles.
pub struct SessionCompiler {
    mc: ModuleCompiler,
    /// Accumulated `map(...)`-result packed pairs (span → interned schema index), from the checked
    /// launch compile and every checked entry (session-checker C5). Every snapshot carries the FULL
    /// accumulation: an earlier entry's still-live function looks its call span up at run time, so
    /// the pairs must survive into every later module.
    map_packed: Vec<(Span, u32)>,
    /// Reflection accumulated across entries (REPL-on-VM follow-on): each entry's
    /// [`noeta_ast::reflect::build`] is merged in latest-wins, so `attributes_of` / `type_of` /
    /// `roles_of` on a type declared in an *earlier* entry resolve — where a per-entry rebuild would
    /// only see the current entry's declarations. The tree-walker `Session` accumulates identically,
    /// so the session differential stays green.
    reflection: noeta_ast::reflect::ReflectionInfo,
}

impl std::fmt::Debug for SessionCompiler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionCompiler")
            .field("protos", &self.mc.protos.len())
            .field("globals", &self.mc.global_names.len())
            .field("types", &self.mc.types.len())
            .finish_non_exhaustive()
    }
}

impl Default for SessionCompiler {
    fn default() -> SessionCompiler {
        SessionCompiler::new()
    }
}

impl SessionCompiler {
    /// A fresh session: an empty module with just the entry-`main` placeholder at proto 0.
    pub fn new() -> SessionCompiler {
        let mc = ModuleCompiler {
            protos: vec![Chunk::placeholder()],
            shapes: Vec::new(),
            packed_schemas: Vec::new(),
            methods: Vec::new(),
            destructors: Vec::new(),
            field_defaults: Vec::new(),
            comparable_derives: Vec::new(),
            tojson_derives: Vec::new(),
            deserialize_recipes: Vec::new(),
            type_args: Vec::new(),
            structural_eq_types: HashSet::new(),
            packed_fields: HashMap::new(),
            key_capable_types: HashSet::new(),
            types: HashMap::new(),
            module_globals: HashMap::new(),
            module_fns: HashSet::new(),
            module_binding_names: Vec::new(),
            type_of_sites: HashMap::new(),
            cache_slots: 0,
            type_reprs: Vec::new(),
            names: Vec::new(),
            name_ids: HashMap::new(),
            global_names: Vec::new(),
            global_slots: HashMap::new(),
            debug: false,
            // A fresh REPL session resolves native names against the process-global default; an
            // embed session that assembled its own set installs it explicitly (instance-registry IR5).
            registry: noeta_ext_abi::registry::single_registry_process(),
        };
        SessionCompiler {
            mc,
            map_packed: Vec::new(),
            reflection: noeta_ast::reflect::ReflectionInfo::default(),
        }
    }

    /// Compile one REPL entry, **appending** its declarations to the persistent tables and its
    /// top-level statements into proto 0 — overwriting the previous entry's now-dead `main` (no live
    /// value references proto 0). Returns a [`Module`] snapshot ready to run against the session's
    /// persistent globals. New protos / shapes / global slots keep the indices they are assigned here
    /// forever.
    pub fn extend(&mut self, entry: &Program) -> Result<Module, Unsupported> {
        self.extend_impl(entry, None)
    }

    /// [`SessionCompiler::extend`] with the checker's **accumulated [`Sites`]** (session-checker
    /// C5): the entry lowers with its span-keyed codegen hints active — packed lists, `type_of`
    /// full fidelity, method handles, streaming `for`s, width masking — and with PRECISE destructor
    /// relevance/reachability instead of the conservative over-approximations. Only sound when the
    /// checker has seen **every entry of the session** (the caller gates on that): precise
    /// relevance derived from a registry that missed an unchecked entry's `destruct` class could
    /// skip a destructor. Conservative is the always-safe direction; precise is the earned one.
    pub fn extend_checked(
        &mut self,
        entry: &Program,
        sites: &Sites,
    ) -> Result<Module, Unsupported> {
        self.extend_impl(entry, Some(sites))
    }

    fn extend_impl(
        &mut self,
        entry: &Program,
        sites: Option<&Sites>,
    ) -> Result<Module, Unsupported> {
        // Hoist standalone-`impl` methods onto their target type (L1 user traits, UT2) so surface
        // registration and IR compilation agree; idempotent, so lowering re-hoisting is a no-op.
        // Against the session's own registry — see the note in `compile_module`; a hot-swapped
        // edit must materialize the same native derive recipes the initial compile did.
        let hoisted = noeta_ir::hoist_impl_methods_with_registry(entry, Some(self.mc.registry));
        let entry: &Program = hoisted.as_ref().unwrap_or(entry);
        // Checkerless lowering (matches the tree-walker `Session`) unless the caller supplied the
        // checker's bundle: then the SAME lowering the file pipeline runs, sites and all. The
        // conservative path's `insert_drops(_, None)` marks every value destructor-relevant;
        // `thread_reuse` runs identically either way (a pure function of the drop-annotated IR).
        let ir = match sites {
            None => noeta_ir::lower(entry),
            Some(sites) => noeta_ir::lower_with_sites_opts(
                entry,
                noeta_ir::lowering_sites!(sites),
                noeta_ir::LowerOptions {
                    // The REPL keeps cooperative isolates, exactly like the checkerless path.
                    real_isolates: false,
                    // The session's own registry (instance-registry IR5) — the default for a REPL
                    // session, an embed session's own set when it installed one.
                    registry: self.mc.registry,
                },
            ),
        }
        .map_err(|u| Unsupported {
            reason: format!("not yet lowered to the Core IR: {}", u.feature),
        })?;
        let relevance = sites.map(|s| passes_relevance(&s.destructor_relevance));
        let ir = noeta_ir_passes::insert_drops(&ir, relevance.as_ref());
        let ir = noeta_ir_passes::thread_reuse(&ir);

        // The checker's `type_of` full-fidelity map merges into the persistent table the codegen
        // reads (span-keyed by this entry's SourceId; re-merging the accumulated bundle is an
        // idempotent overwrite). Must precede any FnCompiler run below.
        if let Some(sites) = sites {
            self.mc
                .type_of_sites
                .extend(sites.type_of_sites.iter().map(|(k, v)| (*k, v.clone())));
            // `@derive(Deserialize<Json>)` decode recipes (L2.2 DI): the checker records one per
            // deriving struct into its accumulated sites, and lowering already emits the
            // `Rvalue::DecodeTyped` (via the accumulated `decode_typed_sites`) — but the runtime
            // registry `json.decode_typed` resolves against is baked from `Module::deserialize_recipes`,
            // which a REPL entry never populated. Bake the checker's accumulated recipes here, exactly
            // as the whole-program checked compile does (`compile_with_sites`'s `sites.deserialize_recipes`),
            // so a type declared at the prompt decodes. The snapshot is the full accumulated set
            // (latest-wins on a redeclared name once the VM lifts it into a name→recipe map), so a
            // wholesale replace stays idempotent across entries. The checkerless path has no checker to
            // derive a recipe (and does not recognize `decode_typed` at all), so this is a checked-session
            // capability by construction.
            self.mc.deserialize_recipes = sites.deserialize_recipes.clone();
            // The forwarding type-argument table (F2b): the session checker accumulates it across
            // entries (indexes are append-only), so the lowered snapshot is cumulative and a
            // wholesale replace stays index-stable.
            self.mc.type_args = ir.type_args.clone();
        }

        // Register this entry's globals/types/methods into the persistent tables (all additive:
        // `HashMap`/`HashSet` inserts, and `register_types` reserves *new* protos at the current end).
        self.mc.register_globals(entry);
        self.mc.register_types(entry);
        self.mc.compile_methods(&ir)?;

        // Compile the entry's top-level statements into a fresh chunk and install it at proto 0.
        let main = {
            let mut fc = FnCompiler::new(&mut self.mc, true, None, Vec::new(), Vec::new());
            fc.init_temps(ir.temp_count);
            fc.setup_main_scopes(&ir.top);
            for stmt in &ir.top.stmts {
                fc.stmt(stmt)?;
            }
            fc.code.push(Op::Halt);
            fc.into_chunk(0, Vec::new(), Some("main".to_string()), Some(ir.span))
        };
        self.mc.protos[0] = main;

        // Intern any NEW `map(...)`-result packed pairs into the accumulation (sorted by span for
        // deterministic schema order, exactly like the whole-program compile; already-known spans
        // re-arrive with each accumulated bundle and are skipped).
        if let Some(sites) = sites {
            let known: HashSet<Span> = self.map_packed.iter().map(|(s, _)| *s).collect();
            let mut fresh: Vec<(Span, &noeta_ast::reflect::PackedLayout)> = sites
                .map_packed_sites
                .iter()
                .filter(|(s, _)| !known.contains(s))
                .map(|(s, l)| (*s, l))
                .collect();
            fresh.sort_by_key(|(s, _)| (s.source, s.start, s.end));
            for (span, layout) in fresh {
                let idx = self.mc.intern_packed_schema(layout);
                self.map_packed.push((span, idx));
            }
        }

        // Destruct-reachability: PRECISE from the checker's accumulated fixpoint when supplied;
        // otherwise the conservative over-approximation to *all* type names, so the VM walks every
        // value container-first and no destructor is missed (correct; it only forgoes the
        // plain-free fast path for a genuinely destructor-free type).
        let destruct_reachable: Vec<String> = match sites {
            Some(sites) => sites
                .destructor_relevance
                .reachable_types
                .iter()
                .cloned()
                .collect(),
            None => self.mc.types.keys().cloned().collect(),
        };

        // Accumulate this entry's reflection into the persistent set (latest-wins), so a query on a
        // type declared in an earlier entry resolves — the tree-walker `Session` accumulates the same
        // way, keeping the session differential green.
        let native_roles = self.mc.registry.native_roles();
        let native_traits = noeta_ir::native_trait_impls(self.mc.registry);
        self.reflection.accumulate(noeta_ast::reflect::build(
            entry,
            &native_roles,
            &native_traits,
        ));
        // Re-embed extension attribute shapes: `accumulate` purges a redeclared name's records, and
        // the extension shapes must survive every entry (idempotent for names already present).
        noeta_check::extend_reflection(&mut self.reflection);

        // Snapshot the persistent tables into a runnable module (cloned, not moved, so the tables
        // stay alive for the next entry). The full map-packed accumulation rides every snapshot —
        // an earlier checked entry's still-live `map(...)` resolves its span at run time.
        Ok(self.snapshot(self.map_packed.clone(), destruct_reachable))
    }

    /// Snapshot the persistent tables into a runnable [`Module`]. Cloned (not moved) so the tables
    /// stay alive for the next entry; O(total bytecode) per snapshot, negligible at an interactive
    /// prompt. `map_packed_sites` / `destruct_reachable` differ by path: a checkerless REPL entry
    /// passes empty / all-types (conservative), the checked session seed
    /// ([`compile_with_sites_session`]) passes the checker's precise outputs.
    fn snapshot(
        &self,
        map_packed_sites: Vec<(Span, u32)>,
        destruct_reachable: Vec<String>,
    ) -> Module {
        Module {
            protos: self.mc.protos.clone(),
            shapes: self.mc.shapes.clone(),
            packed_schemas: self.mc.packed_schemas.clone(),
            map_packed_sites,
            methods: self.mc.methods.clone(),
            destructors: self.mc.destructors.clone(),
            field_defaults: self.mc.field_defaults.clone(),
            comparable_derives: self.mc.comparable_derives.clone(),
            tojson_derives: self.mc.tojson_derives.clone(),
            deserialize_recipes: self.mc.deserialize_recipes.clone(),
            type_args: self.mc.type_args.clone(),
            destruct_reachable,
            cache_slots: self.mc.cache_slots,
            reflection: self.reflection.clone(),
            type_reprs: self.mc.type_reprs.clone(),
            names: self.mc.names.clone(),
            global_bindings: if self.mc.debug {
                self.mc
                    .module_binding_names
                    .iter()
                    .filter_map(|name| self.mc.global_slots.get(name).copied().map(GlobalId))
                    .collect()
            } else {
                Vec::new()
            },
            global_names: self.mc.global_names.clone(),
        }
    }

    /// Register `name` as a module-global binding **without compiling a declaration** — the debug
    /// console promotes a fragment's top-level bindings to session globals (tooling-unification
    /// U2), so the assignment inside the fragment's closure wrapper resolves to a global slot
    /// instead of declaring a closure-local that dies with the entry. `mutable` marks the binding
    /// re-assignable (console bindings are, like REPL bindings). With `overwrite` (a console `mut`
    /// redeclaration) an existing registration is replaced — the same latest-wins a REPL entry's
    /// `register_globals` applies; without it an existing binding — e.g. the program's own global,
    /// whose declared mutability must stand — is left untouched. The slot itself is interned when
    /// the first store compiles.
    pub fn declare_global(&mut self, name: &str, mutable: bool, overwrite: bool) {
        if overwrite || !self.mc.module_globals.contains_key(name) {
            self.mc.module_globals.insert(name.to_string(), mutable);
        }
    }

    /// The current global slot table (global name → dense slot index), for the REPL's `:drop` /
    /// `:bindings` meta-commands. A binding re-declared across entries keeps the same slot.
    pub fn global_slots(&self) -> &HashMap<String, u32> {
        &self.mc.global_slots
    }

    /// The global slot names in slot order (`global_names[i]` is the name in slot `i`), for
    /// `:bindings` to enumerate the live user bindings.
    pub fn global_names(&self) -> &[String] {
        &self.mc.global_names
    }
}

/// What a top-level type name denotes, with the layout/dispatch data the compiler needs to
/// lower object literals, member access, method calls, and enum construction.
enum TypeInfo {
    /// A struct (`struct X { ... }`) — the value kind: declared field order, plus each `fn`'s
    /// reserved prototype index (a struct `fn` dispatches as `X.f(...)` / `obj.f(...)`, exactly
    /// like a class method — the unified body grammar shares the dispatch machinery).
    Struct {
        fields: Vec<String>,
        fns: HashMap<String, u32>,
    },
    /// A class: declared field order plus each `fn`'s reserved prototype index (a class `fn`
    /// is callable both as an associated function `X.f(...)` and as an instance method
    /// `obj.f(...)`, so one prototype serves both — see [`ModuleCompiler::compile_methods`]).
    Class {
        fields: Vec<String>,
        fns: HashMap<String, u32>,
    },
    /// An enum: each variant's positional data-field names, plus each `fn`'s reserved prototype
    /// index (an enum `fn` is callable as an associated function `E.f(...)` and as an instance
    /// method `value.f(...)`, exactly like a class method — the unified body, object-model slice 3).
    Enum {
        variants: HashMap<String, VariantSlots>,
        fns: HashMap<String, u32>,
    },
    /// A `use`-imported stub whose real field set is unknown until a literal supplies it.
    Opaque,
}

/// One enum variant's compile-time record: its **declaration index** (what derived `Comparable`
/// orders by — baked into the variant's [`Shape`]) and its positional data-field names.
#[derive(Clone)]
struct VariantSlots {
    index: u32,
    fields: Vec<String>,
}

/// Accumulates the prototype table, the shape/method side tables, and the top-level type
/// environment across compilation.
struct ModuleCompiler {
    protos: Vec<Chunk>,
    shapes: Vec<Shape>,
    /// The packed-list element layouts (P-PACK 2.4), interned by [`Self::intern_packed_schema`] and
    /// referenced by index from [`Op::MakePackedList`].
    packed_schemas: Vec<noeta_bytecode::PackedSchemaDef>,
    methods: Vec<MethodEntry>,
    destructors: Vec<(String, u32)>,
    /// `(type_name, field_name, proto)` for each field with a default (object-model slice 5), the
    /// thunk compiled in global scope (see [`Module::field_defaults`]).
    field_defaults: Vec<(String, String, u32)>,
    comparable_derives: Vec<String>,
    tojson_derives: Vec<String>,
    /// `@derive(Deserialize<Json>)` decode recipes (L2.2 DI), taken verbatim from the checker's
    /// [`noeta_check::Sites::deserialize_recipes`] and copied onto [`Module::deserialize_recipes`].
    deserialize_recipes: Vec<(String, noeta_ext_abi::TypeRecipe)>,
    /// The program-wide type-argument table (poly-values F2b), taken from the lowered IR
    /// `Program::type_args` and copied onto [`Module::type_args`].
    type_args: Vec<noeta_ext_abi::TypeArgInfo>,
    /// Type names whose `==` is **structural** (baked into each instance's `Shape::structural_eq`):
    /// every `struct`, plus a `class` that is `Equatable` (derives it or hand-`impl`s `eq`). A
    /// `class` absent here compares by reference identity. Mirrors the tree-walker's
    /// `TypeDef::structural_eq` so both backends agree (object-model slice 2).
    structural_eq_types: HashSet<String>,
    /// Every `@packed` struct's field-type names (P-PKEY, `noeta_ast::packed_named_fields`),
    /// accumulated across `register_types` passes (a session declares incrementally) — the input
    /// to the key-capability fixpoint below.
    packed_fields: HashMap<String, Vec<Option<String>>>,
    /// The **key-capable** packed structs (P-PKEY, `noeta_ast::key_capable_packed` over
    /// `packed_fields`): types whose values may key a `Map` / member a `Set`. Baked into each
    /// instance's `Shape::key_capable`, like `structural_eq`; recomputed after every
    /// `register_types` pass. Mirrored by the eval backend from the same inputs.
    key_capable_types: HashSet<String>,
    types: HashMap<String, TypeInfo>,
    /// Every top-level value global's name and whether it is mutable. Computed before any body
    /// is compiled so a nested function can resolve a global (and check its mutability on
    /// assignment) and so the free-variable analysis can tell a global from a captured local.
    module_globals: HashMap<String, bool>,
    /// The names of top-level `fn` declarations — immutable, zero-upvalue globals bound once to a
    /// closure. A call whose callee resolves to one of these lowers to a direct [`Op::CallGlobal`]
    /// instead of `LoadGlobal` + `Op::Call` (perf A). Populated in [`Self::register_globals`].
    module_fns: HashSet<String>,
    /// Top-level **value binding** names (`x = 1`) in source order — the subset of globals the
    /// debugger shows on the `main` frame. Distinct from `module_fns` (function names) and from the
    /// native values a `use` imports, both of which also occupy globals but are not user variables.
    /// Mapped to slots to build [`Module::global_bindings`] (debug compiles only).
    module_binding_names: Vec<String>,
    /// The concrete static type the checker resolved for each `type_of(value)` site (keyed by the
    /// `Expr::TypeOf` span), harvested from the *same* program the tree-walker harvests, so both
    /// backends bake identical full-fidelity `Type` constants (`type_of` fidelity A, P2.3). A site
    /// absent here lowers to the runtime head-constructor op instead.
    type_of_sites: HashMap<Span, noeta_ast::reflect::TypeRepr>,
    /// Running count of inline-cache slots assigned so far. Each `LoadField`/`CallMethod` emission
    /// takes the next id (module-global across all chunks); the total becomes [`Module::cache_slots`],
    /// sizing the VM's per-run cache array. See [`ModuleCompiler::next_cache_slot`].
    cache_slots: u32,
    /// The interned reflected element types (R1), referenced by index from [`Op::MakeList`]. Interned
    /// by [`Self::intern_type_repr`]; becomes [`Module::type_reprs`].
    type_reprs: Vec<noeta_ast::reflect::TypeRepr>,
    /// The interned instruction name table (P-VMT-OPSZ), becomes [`Module::names`]. Every op name
    /// (field / method / global / type, ext-call module+func, `match`-literal strings) is interned
    /// by [`Self::intern_name`] to a 4-byte [`NameId`] instead of an inline `String`, shrinking `Op`.
    names: Vec<String>,
    /// Dedup index for [`Self::names`]: name → its id, so a name used at N sites is stored once.
    name_ids: HashMap<String, u32>,
    /// The global slot table (P-VMT-GSLOT), becomes [`Module::global_names`]. Each top-level binding
    /// and `fn` name gets a dense slot by [`Self::intern_global`] so the VM indexes a `Vec` instead of
    /// hashing a name on every global access.
    global_names: Vec<String>,
    /// Dedup index for [`Self::global_names`]: global name → its slot id.
    global_slots: HashMap<String, u32>,
    /// Whether this is a **debug** compile (`noeta dap`): emit per-prototype debug info — the
    /// `reg → name` locals map, function names, and defining spans — and pin every named local
    /// through register coalescing so that map stays 1:1 (see [`FnCompiler::into_chunk`]). A
    /// production/differential compile leaves this `false`, so no debug info is produced and
    /// coalescing is unconstrained (goldens/benchmarks are untouched).
    debug: bool,
    /// The extension registry name resolution consults during compilation (instance-registry IR5):
    /// `use` import lowering — `is_native_module` / `selective_import_module` / the module-function
    /// binding — resolves against it, so a session compiling with its own extension set binds
    /// imports to *its* registry. The production/CLI path holds the process-global default; an embed
    /// session threads its own assembled set. Returns `&'static`, so it outlives any borrow.
    registry: &'static noeta_ext_abi::registry::Registry,
}

impl ModuleCompiler {
    /// Reserve and return the next inline-cache slot id (module-global across all chunks). Called
    /// once per `LoadField`/`CallMethod` emission; the final count sizes the VM's per-run cache array.
    fn next_cache_slot(&mut self) -> u32 {
        let slot = self.cache_slots;
        self.cache_slots += 1;
        slot
    }

    /// Intern an instruction name into [`Self::names`], returning its [`NameId`] (P-VMT-OPSZ).
    /// Deduped module-wide, so a name emitted at many sites costs one table entry.
    fn intern_name(&mut self, name: &str) -> NameId {
        if let Some(&id) = self.name_ids.get(name) {
            return NameId(id);
        }
        let id = self.names.len() as u32;
        self.names.push(name.to_string());
        self.name_ids.insert(name.to_string(), id);
        NameId(id)
    }

    /// Assign (or reuse) the global slot for `name` (P-VMT-GSLOT), returning its [`GlobalId`]. Called
    /// at every `LoadGlobal`/`StoreGlobal`/`TakeGlobal` emission; the final table sizes the VM's
    /// per-run globals vector. Slot order is assignment order and carries no semantics — the VM tracks
    /// runtime binding order separately for reverse-order destruction.
    fn intern_global(&mut self, name: &str) -> GlobalId {
        if let Some(&id) = self.global_slots.get(name) {
            return GlobalId(id);
        }
        let id = self.global_names.len() as u32;
        self.global_names.push(name.to_string());
        self.global_slots.insert(name.to_string(), id);
        GlobalId(id)
    }

    /// Pre-pass: collect every top-level value global (a binding or `fn`/native-module name) and
    /// its mutability, so functions can resolve and assign globals and the capture analysis can
    /// distinguish a global from an enclosing local.
    fn register_globals(&mut self, program: &Program) {
        for stmt in &program.stmts {
            match stmt {
                noeta_ast::Stmt::Binding { mut_decl, name, .. } => {
                    self.module_globals.insert(name.clone(), *mut_decl);
                    // A user value binding — the only global kind the debugger shows on `main`.
                    self.module_binding_names.push(name.clone());
                }
                noeta_ast::Stmt::Fn(decl) => {
                    self.module_globals.insert(decl.name.clone(), false);
                    self.module_fns.insert(decl.name.clone());
                }
                noeta_ast::Stmt::Use { path, names, .. } => {
                    // A plain module import (`use std.{math}`) binds the module name; a selective
                    // member import (`use std.math.sqrt`) binds each member as a bare global.
                    for imported in names {
                        if binds_native_value(self.registry, path, &imported.name) {
                            self.module_globals.insert(imported.name.clone(), false);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// The set of top-level global names (for the free-variable analysis' globals argument).
    fn global_names(&self) -> HashSet<String> {
        self.module_globals.keys().cloned().collect()
    }

    /// Pass 1: register every top-level `type`/`class`/`enum`/`use` so bodies compiled later
    /// can resolve them, and reserve a placeholder prototype for each class `fn`.
    fn register_types(&mut self, program: &Program) {
        // The built-in `Ordering` enum is namable like any other (so `Ordering.Less` can be
        // constructed, not only received from `.compare()`); registered first so a user `enum
        // Ordering` would shadow it. Its variants carry no data, matching `make_ordering`.
        self.types.insert(
            "Ordering".to_string(),
            TypeInfo::Enum {
                variants: ["Less", "Equal", "Greater"]
                    .into_iter()
                    .enumerate()
                    .map(|(i, v)| {
                        (
                            v.to_string(),
                            VariantSlots {
                                index: i as u32,
                                fields: Vec::new(),
                            },
                        )
                    })
                    .collect(),
                fns: HashMap::new(),
            },
        );
        for stmt in &program.stmts {
            match stmt {
                noeta_ast::Stmt::Struct(decl) => {
                    let fields = decl.fields.iter().map(|f| f.name.clone()).collect();
                    // A value `struct` always compares structurally.
                    self.structural_eq_types.insert(decl.name.clone());
                    // A `@packed` struct feeds the key-capability fixpoint (P-PKEY, below).
                    if let Some(named) = noeta_ast::packed_named_fields(decl) {
                        self.packed_fields.insert(decl.name.clone(), named);
                    }
                    // A hand-written `compare`/`to_json` (via an `impl` block) takes precedence over
                    // the derived version — same rule as a class.
                    if noeta_ast::derives_trait(&decl.decorators.derives, "Comparable")
                        && !decl.methods.iter().any(|m| m.name == "compare")
                    {
                        self.comparable_derives.push(decl.name.clone());
                    }
                    // `@derive(Serialize<Json>)` synthesizes the structural JSON serializer (`Json`
                    // is the only format today, so it maps to the existing `to_json` codegen).
                    if noeta_ast::derives_trait(&decl.decorators.derives, "Serialize")
                        && !decl.methods.iter().any(|m| m.name == "to_json")
                    {
                        self.tojson_derives.push(decl.name.clone());
                    }
                    // Reserve a prototype per method, shared by associated-fn and instance-method
                    // dispatch (a struct `fn` is callable both ways, exactly like a class method).
                    let mut fns = HashMap::new();
                    for method in &decl.methods {
                        let proto = self.protos.len() as u32;
                        self.protos.push(Chunk::placeholder());
                        fns.insert(method.name.clone(), proto);
                        self.methods.push(MethodEntry {
                            type_name: decl.name.clone(),
                            method: method.name.clone(),
                            proto,
                        });
                    }
                    self.types
                        .insert(decl.name.clone(), TypeInfo::Struct { fields, fns });
                }
                noeta_ast::Stmt::Class(decl) => {
                    let fields: Vec<String> = decl.fields.iter().map(|f| f.name.clone()).collect();
                    // A reference `class` compares structurally only if it is `Equatable` — derives
                    // it or hand-`impl`s `eq`; otherwise `==` falls back to reference identity.
                    if noeta_ast::derives_trait(&decl.decorators.derives, "Equatable")
                        || decl.methods.iter().any(|m| m.name == "eq")
                    {
                        self.structural_eq_types.insert(decl.name.clone());
                    }
                    // A hand-written `compare` (via `impl Comparable`) takes precedence over the
                    // derived structural ordering.
                    if noeta_ast::derives_trait(&decl.decorators.derives, "Comparable")
                        && !decl.methods.iter().any(|m| m.name == "compare")
                    {
                        self.comparable_derives.push(decl.name.clone());
                    }
                    // A hand-written `to_json` takes precedence over the derived serializer.
                    if noeta_ast::derives_trait(&decl.decorators.derives, "Serialize")
                        && !decl.methods.iter().any(|m| m.name == "to_json")
                    {
                        self.tojson_derives.push(decl.name.clone());
                    }
                    let mut fns = HashMap::new();
                    for method in &decl.methods {
                        let proto = self.protos.len() as u32;
                        self.protos.push(Chunk::placeholder());
                        fns.insert(method.name.clone(), proto);
                        self.methods.push(MethodEntry {
                            type_name: decl.name.clone(),
                            method: method.name.clone(),
                            proto,
                        });
                    }
                    // Reserve a prototype for the `destruct` block (compiled like a method).
                    if decl.destructor.is_some() {
                        let proto = self.protos.len() as u32;
                        self.protos.push(Chunk::placeholder());
                        self.destructors.push((decl.name.clone(), proto));
                    }
                    self.types
                        .insert(decl.name.clone(), TypeInfo::Class { fields, fns });
                }
                noeta_ast::Stmt::Enum(decl) => {
                    let variants = decl
                        .variants
                        .iter()
                        .enumerate()
                        .map(|(i, v)| {
                            (
                                v.name.clone(),
                                VariantSlots {
                                    index: i as u32,
                                    fields: v.fields.iter().map(|f| f.name.clone()).collect(),
                                },
                            )
                        })
                        .collect();
                    // Enum derives, same precedence rules as a struct/class: a hand-written
                    // `compare`/`to_json` (via an `impl` block) beats the derived version. Ordering
                    // is variant declaration index, then payload fields (the shape carries the
                    // index — see `Shape::variant_index`).
                    if noeta_ast::derives_trait(&decl.decorators.derives, "Comparable")
                        && !decl.methods.iter().any(|m| m.name == "compare")
                    {
                        self.comparable_derives.push(decl.name.clone());
                    }
                    if noeta_ast::derives_trait(&decl.decorators.derives, "Serialize")
                        && !decl.methods.iter().any(|m| m.name == "to_json")
                    {
                        self.tojson_derives.push(decl.name.clone());
                    }
                    // Reserve a prototype per method, shared by associated-fn and instance-method
                    // dispatch (the unified body, object-model slice 3).
                    let mut fns = HashMap::new();
                    for method in &decl.methods {
                        let proto = self.protos.len() as u32;
                        self.protos.push(Chunk::placeholder());
                        fns.insert(method.name.clone(), proto);
                        self.methods.push(MethodEntry {
                            type_name: decl.name.clone(),
                            method: method.name.clone(),
                            proto,
                        });
                    }
                    self.types
                        .insert(decl.name.clone(), TypeInfo::Enum { variants, fns });
                }
                noeta_ast::Stmt::Use { path, names, .. } => {
                    // A `use std.{json}` native module — or a selective member import
                    // (`use std.math.sqrt`) — resolves as a global value bound at the `use` site,
                    // not an opaque type, so neither is registered here.
                    for imported in names {
                        use noeta_ext_abi::registry::UseKind;
                        match self.registry.classify_use(path, &imported.name) {
                            UseKind::Module(_) | UseKind::MemberFn { .. } => continue,
                            // A native enum (`use shade.Hue`, native-extensibility S1b): register it
                            // as a **constructible** type handle keyed by the imported short name — so
                            // `Hue.Red` / `Hue.Labeled(x)` lower exactly like a `.noe` enum. The
                            // registry (keyed by qualified identity) is the single source of variants,
                            // the same channel `Ordering` is hand-seeded through. The shape name is the
                            // short local name, matching what a native-returned variant carries and
                            // what a `TheEnum.Variant` pattern compares against (S1 identity note).
                            UseKind::ExtEnum(qualified) => {
                                if let Some(en) = self.registry.find_enum_qualified(&qualified) {
                                    self.types
                                        .insert(imported.name.clone(), ext_enum_type_info(en));
                                }
                            }
                            // A native **fielded type** — a class (`use geo.Handle`,
                            // native-extensibility S2) or a value struct (`use geo.Point`, fielded
                            // unification): register it as a **constructible** type handle keyed by
                            // the imported short name, so `Handle { … }` / `Point { … }` lowers
                            // exactly like a `.noe` class/struct. The registry (keyed by qualified
                            // identity) is the single source of fields; `ext_fielded_type_info`
                            // selects the class-kind or struct-kind shape off `ExtFielded::kind`.
                            UseKind::ExtClass(qualified) | UseKind::ExtStruct(qualified) => {
                                if let Some(cl) = self.registry.resolve_fielded(&qualified) {
                                    // A native value **struct** always compares structurally — record
                                    // its imported (runtime shape) name so `MakeStruct` builds the
                                    // shape with `structural_eq = true`, exactly as a `.noe` struct
                                    // declaration does above. A native class stays identity (`==` is
                                    // reference), so it is NOT added.
                                    if cl.kind == noeta_ext_abi::FieldedKind::Struct {
                                        self.structural_eq_types.insert(imported.name.clone());
                                    }
                                    self.types
                                        .insert(imported.name.clone(), ext_fielded_type_info(cl));
                                }
                            }
                            _ => {
                                self.types.insert(imported.name.clone(), TypeInfo::Opaque);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        // P-PKEY: with every declaration of this pass recorded, settle which packed structs are
        // key-capable (the fixpoint spans passes — a session's later entry can complete a nested
        // chain declared earlier).
        self.key_capable_types = noeta_ast::key_capable_packed(&self.packed_fields);
    }

    /// Pass 2: compile each class/struct method (and a class's `destruct` block) into its reserved
    /// prototype, from the lowered IR. Methods see all registered types (forward references work)
    /// and run with the receiver in register 0, the declared parameters in registers `1..`, and
    /// field names resolving to the receiver.
    fn compile_methods(&mut self, ir: &noeta_ir::Program) -> Result<(), Unsupported> {
        for stmt in &ir.top.stmts {
            // A struct shares the class method-dispatch machinery (the unified body), minus the
            // `destruct` block — so compile its methods into the prototypes reserved in pass 1.
            if let Stmt::Decl(Decl::Struct(strukt)) = stmt {
                let name = strukt.decl.name.clone();
                for (method, func) in &strukt.methods {
                    let TypeInfo::Struct { fns, .. } = &self.types[&name] else {
                        unreachable!("a struct registered as non-struct");
                    };
                    let proto = fns[method];
                    let chunk = self.compile_func(
                        func,
                        Some(MethodCtx),
                        Vec::new(),
                        Vec::new(),
                        Some(format!("{name}.{method}")),
                    )?;
                    self.protos[proto as usize] = chunk;
                }
                self.compile_field_defaults(&name, &strukt.field_defaults)?;
                continue;
            }
            // An enum method (object-model slice 3) compiles like a struct/class method but with an
            // **empty** field scope: an enum's variants carry different data, so there are no implicit
            // fields to resolve — a method reaches its payload by `match`ing on the receiver (`self`).
            if let Stmt::Decl(Decl::Enum(en)) = stmt {
                let name = en.decl.name.clone();
                for (method, func) in &en.methods {
                    let TypeInfo::Enum { fns, .. } = &self.types[&name] else {
                        unreachable!("an enum registered as non-enum");
                    };
                    let proto = fns[method];
                    let chunk = self.compile_func(
                        func,
                        Some(MethodCtx),
                        Vec::new(),
                        Vec::new(),
                        Some(format!("{name}.{method}")),
                    )?;
                    self.protos[proto as usize] = chunk;
                }
                continue;
            }
            let Stmt::Decl(Decl::Class(class)) = stmt else {
                continue;
            };
            let name = class.decl.name.clone();
            for (method, func) in &class.methods {
                let TypeInfo::Class { fns, .. } = &self.types[&name] else {
                    unreachable!("a class registered as non-class");
                };
                let proto = fns[method];
                let chunk = self.compile_func(
                    func,
                    Some(MethodCtx),
                    Vec::new(),
                    Vec::new(),
                    Some(format!("{name}.{method}")),
                )?;
                self.protos[proto as usize] = chunk;
            }
            // The `destruct` block compiles like a parameterless method (fields in scope).
            if let Some(func) = &class.destructor {
                let proto = self
                    .destructors
                    .iter()
                    .find(|(n, _)| n == &name)
                    .map(|(_, proto)| *proto)
                    .expect("a destructor proto was reserved in pass 1");
                let chunk = self.compile_func(
                    func,
                    Some(MethodCtx),
                    Vec::new(),
                    Vec::new(),
                    Some(format!("{name}::destruct")),
                )?;
                self.protos[proto as usize] = chunk;
            }
            self.compile_field_defaults(&name, &class.field_defaults)?;
        }
        Ok(())
    }

    /// Compile each field-default thunk (object-model slice 5) into a fresh parameterless prototype
    /// and record `(type, field) → proto` in [`Self::field_defaults`]. The thunk is compiled in
    /// **global scope** (empty upvalues + no enclosing locals — a type is top-level, so its default
    /// resolves globals only, never `self`/sibling fields/the construction site). `MakeStruct` runs
    /// it for an omitted field, matching the tree-walker's definition-scope fill.
    fn compile_field_defaults(
        &mut self,
        type_name: &str,
        defaults: &[(String, noeta_ir::Thunk)],
    ) -> Result<(), Unsupported> {
        for (field, thunk) in defaults {
            let proto = self.add_thunk(thunk, Vec::new(), Vec::new())?;
            self.field_defaults
                .push((type_name.to_string(), field.clone(), proto));
        }
        Ok(())
    }

    /// Compile one IR [`Func`] (function/closure/method/`destruct` body) into a [`Chunk`]. `name`
    /// is the name a debugger shows for this prototype (`None` for an anonymous closure); it and the
    /// func's span are recorded on the chunk only in a debug compile.
    fn compile_func(
        &mut self,
        func: &Func,
        method: Option<MethodCtx>,
        upvalues: Vec<(String, bool)>,
        enclosing_locals: Vec<HashSet<String>>,
        name: Option<String>,
    ) -> Result<Chunk, Unsupported> {
        self.compile_chunk(
            &func.params,
            &func.defaults,
            &func.body,
            func.temp_count,
            method,
            upvalues,
            enclosing_locals,
            name,
            Some(func.span),
            func.captures.as_deref(),
        )
    }

    /// Compile a function-like body into a [`Chunk`]: its parameters, defaulted-parameter thunks,
    /// and a [`Block`] body sized for `temp_count` frame temporaries. A `method` context reserves
    /// register 0 for the receiver (`self`) and resolves the class's field names against it.
    /// `upvalues` are the names (and mutability) this function captures from enclosing functions,
    /// in the order its parent will supply the cells; `enclosing_locals` are the enclosing
    /// functions' capturable local names (outermost first), so the function can lower its own
    /// nested closures. A body [`Block`] with a `tail` atom (a closure/arrow or a default thunk)
    /// returns that atom; a block body without one falls off the end as an implicit unit return.
    #[allow(clippy::too_many_arguments)]
    fn compile_chunk(
        &mut self,
        params: &[String],
        defaults: &[Option<Thunk>],
        body: &Block,
        temp_count: u32,
        method: Option<MethodCtx>,
        upvalues: Vec<(String, bool)>,
        enclosing_locals: Vec<HashSet<String>>,
        name: Option<String>,
        def_span: Option<Span>,
        captures: Option<&[String]>,
    ) -> Result<Chunk, Unsupported> {
        let is_method = method.is_some();
        let debug = self.debug;
        let globals = self.global_names();
        let analysis = freevars::analyze(
            params,
            defaults,
            body,
            &enclosing_locals,
            &globals,
            captures,
        );

        // The capturable layer this function exposes to its own nested closures. A method also
        // exposes `self` (captured by boxing the receiver — aether F3); its FIELDS are deliberately
        // absent: a bare name never resolves to a field anywhere (prelude-redesign EX.1 — the
        // reference binds only `self` into a method frame, see `call_method_on`), so a bare name
        // that happens to match a field is an ordinary local/global reference here too.
        let mut local_layer = analysis.local.clone();
        if method.is_some() {
            local_layer.insert("self".to_string());
        }

        // Compile each defaulted parameter's default value into a zero-argument thunk prototype,
        // recording the `(parameter register, thunk proto)` pair. Parameter registers are fixed by
        // declaration order — a method reserves register 0 for the receiver, so its parameters start
        // at 1 — which lets the VM fill an omitted argument's register from its thunk. The thunk is
        // compiled with **this function's own upvalue layout**, so a default that references a
        // captured variable resolves to the same upvalue index the body would use; at call time the
        // VM hands the thunk the closure's upvalue cells. For a top-level function or method
        // `upvalues` is empty, so the thunk resolves globals only. Compiled before the body's
        // `FnCompiler` borrows `self`.
        let base: u16 = if is_method { 1 } else { 0 };
        let mut default_pairs: Vec<(u16, u32)> = Vec::new();
        for (j, default) in defaults.iter().enumerate() {
            if let Some(thunk) = default {
                let proto = self.add_thunk(thunk, upvalues.clone(), enclosing_locals.clone())?;
                default_pairs.push((base + j as u16, proto));
            }
        }

        let mut fc = FnCompiler::new(self, false, method, upvalues, enclosing_locals);
        fc.celled = analysis.celled;
        fc.local_layer = local_layer;
        // The sealed-fn write frontier for `binding` (see `FnCompiler::seal`).
        fc.seal = captures.map(|allow| allow.iter().cloned().collect());
        fc.init_temps(temp_count);

        // A method reserves register 0 for the receiver; ordinary functions do not.
        if is_method {
            fc.alloc_reg();
        }
        fc.scopes.push(HashMap::new());
        for param in params {
            let reg = fc.alloc_reg();
            let celled = fc.celled.contains(param);
            // A captured parameter is boxed into a cell so the closure shares the live binding.
            if celled {
                fc.code.push(Op::MakeCell { dst: reg, src: reg });
            }
            fc.scopes.last_mut().unwrap().insert(
                param.clone(),
                Var {
                    reg,
                    mutable: false,
                    celled,
                },
            );
            // In a debug compile, a parameter is a named local a debugger should see. The IR drops
            // per-parameter spans, so it is attributed to the function's own defining span. Its
            // register (`0..num_params`) is pinned through coalescing by `into_chunk`'s `frame_locals`.
            if let Some(span) = def_span.filter(|_| debug) {
                fc.debug_locals.push(LocalDebug {
                    name: param.clone(),
                    reg,
                    def_span: span,
                });
            }
        }
        fc.hoist_nested_fn_cells(&body.stmts);
        for stmt in &body.stmts {
            fc.stmt(stmt)?;
        }
        // A value-position body (closure/arrow, default thunk) returns its tail atom; a block
        // body that falls off the end implicitly returns unit (M0's `exec_fn_body`).
        if let Some(tail) = &body.tail {
            let src = fc.atom_reg(tail)?;
            fc.code.push(Op::Return { src });
        }
        fc.code.push(Op::Halt);
        let num_params = params.len() as u16 + if is_method { 1 } else { 0 };
        Ok(fc.into_chunk(num_params, default_pairs, name, def_span))
    }

    /// Compile an IR [`Func`] into a fresh prototype and return its index. `name` is the name a
    /// debugger shows for it — the binding's name for a named `fn`, `None` for an anonymous closure.
    fn add_function(
        &mut self,
        func: &Func,
        upvalues: Vec<(String, bool)>,
        enclosing_locals: Vec<HashSet<String>>,
        name: Option<String>,
    ) -> Result<u32, Unsupported> {
        let chunk = self.compile_func(func, None, upvalues, enclosing_locals, name)?;
        let idx = self.protos.len() as u32;
        self.protos.push(chunk);
        Ok(idx)
    }

    /// Compile a defaulted-parameter [`Thunk`] (a zero-parameter value-position body) into a fresh
    /// prototype and return its index.
    fn add_thunk(
        &mut self,
        thunk: &Thunk,
        upvalues: Vec<(String, bool)>,
        enclosing_locals: Vec<HashSet<String>>,
    ) -> Result<u32, Unsupported> {
        let chunk = self.compile_chunk(
            &[],
            &[],
            &thunk.body,
            thunk.temp_count,
            None,
            upvalues,
            enclosing_locals,
            // A defaulted-parameter / field-default thunk is an anonymous value evaluator.
            None,
            None,
            // A thunk evaluates in the definition scope (globals) — no seal of its own.
            None,
        )?;
        let idx = self.protos.len() as u32;
        self.protos.push(chunk);
        Ok(idx)
    }

    /// Intern a shape, returning its index in the shape table (equal shapes share an index, so
    /// equal-built aggregates share one runtime shape — the declared-order determinism the
    /// object model guarantees).
    fn intern_shape(&mut self, shape: Shape) -> u32 {
        if let Some(i) = self.shapes.iter().position(|s| *s == shape) {
            // Same structural identity (`Shape::eq` excludes metadata): merge the metadata so
            // insertion order can't drop it — e.g. the packed-schema path builds a type's shape
            // before `make_record` arrives with the key-capable flag (P-PKEY), or vice versa.
            if shape.key_capable {
                self.shapes[i].key_capable = true;
            }
            return i as u32;
        }
        let idx = self.shapes.len() as u32;
        self.shapes.push(shape);
        idx
    }

    /// Intern a packed-list element layout from the checker's [`PackedLayout`], returning its index
    /// in [`Self::packed_schemas`]. The layout is self-describing (P-PACK 2.1), so the element's
    /// `Shape` is built straight from it — and `intern_shape` dedups it to the *same* entry
    /// `MakeStruct` uses for that type, so a materialized element is shape-identical to a constructed
    /// one. Nested packed structs are interned first (a lower index than their parent), giving the VM
    /// an inner-before-outer build order.
    fn intern_packed_schema(&mut self, layout: &noeta_ast::reflect::PackedLayout) -> u32 {
        use noeta_ast::reflect::PackedKind;

        // A bare-scalar element (`List<i32>`/`List<f32>`) has no struct wrapper — no shape to intern; it
        // materializes to a bare `int`/`f32`. A `@packed` struct interns its shape (the same entry
        // `MakeStruct` uses) so a materialized element shares shape identity with a constructed one.
        let shape = (!layout.is_scalar()).then(|| {
            self.intern_shape(
                Shape::object(
                    noeta_object::ShapeKind::Struct,
                    layout.type_name.clone(),
                    layout.fields.iter().map(|f| f.name.clone()).collect(),
                )
                // Carry the key-capability so a materialized element keys maps/sets exactly like a
                // constructed one (P-PKEY).
                .with_key_capable(self.key_capable_types.contains(&layout.type_name)),
            )
        });
        let fields = layout
            .fields
            .iter()
            .map(|f| match &f.kind {
                PackedKind::Int => noeta_bytecode::PackedFieldDef::Int,
                PackedKind::Float => noeta_bytecode::PackedFieldDef::Float,
                PackedKind::F32 => noeta_bytecode::PackedFieldDef::F32,
                PackedKind::F64 => noeta_bytecode::PackedFieldDef::F64,
                PackedKind::IntN { bits, signed } => noeta_bytecode::PackedFieldDef::IntN {
                    bits: *bits,
                    signed: *signed,
                },
                PackedKind::Bool => noeta_bytecode::PackedFieldDef::Bool,
                PackedKind::Struct(inner) => {
                    noeta_bytecode::PackedFieldDef::Struct(self.intern_packed_schema(inner))
                }
            })
            .collect();
        let def = noeta_bytecode::PackedSchemaDef {
            shape,
            fields,
            byte_size: layout.byte_size() as u32,
            column: layout.column,
        };
        if let Some(i) = self.packed_schemas.iter().position(|s| *s == def) {
            return i as u32;
        }
        let idx = self.packed_schemas.len() as u32;
        self.packed_schemas.push(def);
        idx
    }

    /// Intern a reflected element type (R1) into [`Self::type_reprs`], returning its index — the value
    /// [`Op::MakeList`]'s `reflect` carries. Dedups structurally so repeated identical list-literal
    /// element types share one table entry.
    fn intern_type_repr(&mut self, repr: &noeta_ast::reflect::TypeRepr) -> u32 {
        if let Some(i) = self.type_reprs.iter().position(|r| r == repr) {
            return i as u32;
        }
        let idx = self.type_reprs.len() as u32;
        self.type_reprs.push(repr.clone());
        idx
    }

    /// Intern a built-in `Result`/`Option` variant shape (these display with their bare
    /// constructor, `Ok(x)`/`none`, rather than `Type.Variant`). The data carried varies at
    /// the use site, so the shape needs no declared field names.
    fn builtin_enum_shape(&mut self, enum_name: &str, variant: &str) -> u32 {
        // The built-in enums' defined variant order: `none < some`, `Ok < Err` (derived
        // `Comparable` over an `?T`/`Result` payload orders by variant, then payload).
        let index = match variant {
            "none" | "Ok" => 0,
            _ => 1,
        };
        self.intern_shape(
            Shape::enum_variant(enum_name.to_string(), variant.to_string(), Vec::new(), true)
                .with_variant_index(index),
        )
    }
}

/// Marks a chunk as a method body: register 0 is the receiver, so `self` resolves to it (and a
/// nested closure captures it by boxing that register — aether F3). Fields carry no compile
/// state: a bare name never resolves to a field (prelude-redesign EX.1).
struct MethodCtx;

/// Per-prototype compilation state (one register file, one constant/diagnostic pool).
struct FnCompiler<'m> {
    module: &'m mut ModuleCompiler,
    code: Vec<Op>,
    consts: Vec<Const>,
    diags: Vec<Diagnostic>,
    /// The register holding each frame temporary's value, indexed by [`Temp`] index. ANF
    /// temporaries are write-once and defined before use, so each slot is filled when its
    /// defining `Stmt::Let` (or a `match`/`&&`/`??` destination) is lowered and read thereafter.
    /// Sized to the frame's `temp_count` by [`Self::init_temps`].
    temp_regs: Vec<Option<Reg>>,
    /// Register scopes, innermost last. Empty only in `main` at the global depth.
    scopes: Vec<HashMap<String, Var>>,
    /// Declared top-level globals and their mutability (tracked only in `main`).
    globals: HashMap<String, GlobalInfo>,
    next_reg: u16,
    is_main: bool,
    /// When compiling a class method, the receiver's field names (resolved against register 0).
    method: Option<MethodCtx>,
    /// This function's captured upvalues: name → index, indexed into the frame's upvalue cells.
    upvalue_index: HashMap<String, u16>,
    /// Whether each captured upvalue (by index) is mutable in its defining scope — governs
    /// whether an `x = v` reassignment through the upvalue is allowed or raises E0006.
    upvalue_mut: Vec<bool>,
    /// The names of this function's own locals that an inner closure captures, so they are
    /// stored as cells (computed by the free-variable analysis before lowering).
    celled: HashSet<String>,
    /// The SEALED-fn write frontier: `Some(allow)` when this chunk compiles a named fn/method —
    /// `binding` lets a bare assignment reach a module global only for allow-listed names; every
    /// other bare-assigned name declares a fresh local (matching the checker and the eval
    /// backend's scope frontier). `None` for `main`, closures, and thunks.
    seal: Option<HashSet<String>>,
    /// The enclosing functions' capturable locals (outermost first) — for lowering this
    /// function's own nested closures.
    enclosing_locals: Vec<HashSet<String>>,
    /// This function's own capturable locals (the layer it exposes to its nested closures).
    local_layer: HashSet<String>,
    /// Enclosing-loop jump-patch sites, innermost last. Each `break`/`continue` records a pending
    /// `Jump` here; the loop patches them to its exit / continue target once those are known.
    loops: Vec<LoopCtx>,
    /// The registers of this function's body locals in **declaration order** — the source of the
    /// `Chunk.frame_locals` panic-teardown list (params, prepended at `into_chunk`, come first).
    frame_locals: Vec<Reg>,
    /// The debugger's `reg → name` records (one per source binding), collected by [`Self::declare_local`]
    /// when the compile is in debug mode. Moved onto the `Chunk` (and register-remapped by coalescing)
    /// at [`Self::into_chunk`]. Always empty in a non-debug compile.
    debug_locals: Vec<LocalDebug>,
    /// The line table (`Chunk::line_table`): one `(pc, span)` per source statement, pushed at the
    /// start of [`Self::stmt`] so every instruction resolves to a line — for the debugger's
    /// breakpoints/stepping and for production stack traces. Always emitted (line-info tier). Moved
    /// onto the `Chunk` (and pc-remapped by the hoisting pass) at [`Self::into_chunk`].
    line_table: Vec<LineEntry>,
}

/// Pending forward jumps from `break`/`continue` inside one loop, patched at the loop's end.
#[derive(Default)]
struct LoopCtx {
    /// Code positions of `break` jumps — patched to the instruction after the loop.
    breaks: Vec<usize>,
    /// Code positions of `continue` jumps — patched to the loop's continue target (the `while`
    /// condition re-test, or the `for` index increment).
    continues: Vec<usize>,
    /// For a `for`-loop streaming a **temp** iterator this loop owns (`for x in gen()`), the iterator's
    /// register. A `return` inside the body unwinds past the loop's post-loop drop, so it must drop this
    /// iterator destructor-aware itself (running a generator's captured destructor). `None` for `while`
    /// loops and for-loops over a named/snapshotted iterable (whose value is dropped elsewhere).
    stream_iter: Option<Reg>,
}

/// The span to record in the debug line table for `stmt`, or `None` for a statement that is **not** a
/// source line the debugger should stop on: a synthetic reclamation `Drop`/`DropVar`, a concurrency
/// scope marker, or a nested declaration. Skipping those keeps the line table monotonic in `pc → line`
/// (a last-use `DropVar` would otherwise map a later `pc` to an earlier line) — the excluded ops are
/// covered by the preceding real statement's entry.
fn line_entry_span(stmt: &Stmt) -> Option<Span> {
    match stmt {
        Stmt::Let { span, .. }
        | Stmt::Eval { span, .. }
        | Stmt::Bind { span, .. }
        | Stmt::Echo { span, .. }
        | Stmt::Return { span, .. }
        | Stmt::Break { span, .. }
        | Stmt::Continue { span, .. }
        | Stmt::If { span, .. }
        | Stmt::While { span, .. }
        | Stmt::For { span, .. }
        | Stmt::Match { span, .. }
        | Stmt::Logical { span, .. }
        | Stmt::Coalesce { span, .. } => Some(*span),
        Stmt::Drop(_)
        | Stmt::DropVar { .. }
        | Stmt::ScopeBegin { .. }
        | Stmt::ScopeEnd { .. }
        | Stmt::Decl(_) => None,
    }
}

#[derive(Clone, Copy)]
struct Var {
    reg: Reg,
    mutable: bool,
    /// Whether this local lives in a cell (it is captured by an inner closure). A celled local
    /// is read with `CellGet`, written with `CellSet`, and shared with the capturing closure.
    celled: bool,
}

#[derive(Clone, Copy)]
struct GlobalInfo {
    mutable: bool,
}

/// A nested closure's resolved captures: its ordered upvalue list (each name and whether it is
/// mutable, for the child's reassignment check) paired with the `CaptureFrom` source the building
/// frame uses to supply each cell. The two vectors are index-aligned.
type CaptureLayout = (Vec<(String, bool)>, Vec<PendingCapture>);

/// A capture source before emission: a ready [`CaptureFrom`], or the method receiver — which has
/// no celled register of its own and is boxed into a fresh cell at closure creation
/// ([`Compiler::emit_captures`]). `self` is immutable, so a per-closure cell aliases nothing.
enum PendingCapture {
    From(CaptureFrom),
    SelfCell,
}

/// How a name resolves at a use site.
enum Resolved {
    /// A frame-local register holding the value directly.
    Local(Reg),
    /// A frame-local register holding a cell (a captured local); read/written through it.
    CelledLocal(Reg),
    /// The method receiver (`self`) — register 0 in a method body.
    SelfRecv,
    /// A field of the method receiver, loaded via `LoadField` from register 0.
    /// An upvalue captured from an enclosing function (read via its index). Reassignment goes
    /// through `binding`, which checks the upvalue's mutability separately.
    Upvalue(u16),
    /// A global, read via `LoadGlobal` (the name may or may not exist at runtime).
    Global,
    /// A prelude value/builtin — not yet modeled by the VM, so the program is skipped.
    Prelude,
}

impl<'m> FnCompiler<'m> {
    fn new(
        module: &'m mut ModuleCompiler,
        is_main: bool,
        method: Option<MethodCtx>,
        upvalues: Vec<(String, bool)>,
        enclosing_locals: Vec<HashSet<String>>,
    ) -> FnCompiler<'m> {
        let mut upvalue_index = HashMap::new();
        let mut upvalue_mut = Vec::with_capacity(upvalues.len());
        for (i, (name, mutable)) in upvalues.into_iter().enumerate() {
            upvalue_index.insert(name, i as u16);
            upvalue_mut.push(mutable);
        }
        FnCompiler {
            module,
            code: Vec::new(),
            consts: Vec::new(),
            diags: Vec::new(),
            temp_regs: Vec::new(),
            scopes: Vec::new(),
            globals: HashMap::new(),
            next_reg: 0,
            is_main,
            method,
            upvalue_index,
            upvalue_mut,
            celled: HashSet::new(),
            seal: None,
            enclosing_locals,
            local_layer: HashSet::new(),
            loops: Vec::new(),
            frame_locals: Vec::new(),
            debug_locals: Vec::new(),
            line_table: Vec::new(),
        }
    }

    /// Size the frame-temporary register map to `temp_count` (all slots initially unfilled).
    /// Called once per body before lowering.
    fn init_temps(&mut self, temp_count: u32) {
        self.temp_regs = vec![None; temp_count as usize];
    }

    /// Allocate the register backing a temporary at its defining site (`let t = …`, or a
    /// `match`/`&&`/`??` destination) and record it. A temp is write-once, so each slot is filled
    /// exactly once.
    fn define_temp(&mut self, t: Temp) -> Reg {
        let reg = self.alloc_reg();
        self.temp_regs[t.index()] = Some(reg);
        reg
    }

    /// The register holding temporary `t`'s value. ANF guarantees the temp was defined before this
    /// read, so the slot is filled.
    fn temp_reg(&self, t: Temp) -> Reg {
        self.temp_regs[t.index()].expect("ANF temporary read before it was defined")
    }

    /// The enclosing-locals chain to pass to one of *this* function's nested closures: the chain
    /// we were given, extended with our own capturable layer.
    fn child_enclosing(&self) -> Vec<HashSet<String>> {
        let mut chain = self.enclosing_locals.clone();
        chain.push(self.local_layer.clone());
        chain
    }

    fn into_chunk(
        self,
        num_params: u16,
        defaults: Vec<(u16, u32)>,
        name: Option<String>,
        def_span: Option<Span>,
    ) -> Chunk {
        // The panic-teardown list, in construction order: parameters (registers `0..num_params`, live
        // from entry) then body locals in declaration order. Two things want it populated: a program
        // that **defines a destructor** (so panic teardown is observable), and a **debug** compile (so
        // every named local's register is pinned through coalescing, keeping `debug_locals` a clean
        // 1:1 `reg → name`). Otherwise the common case keeps an empty list and full coalescing, so
        // benchmarks/goldens are untouched.
        let debug = self.module.debug;
        let frame_locals: Vec<u16> = if self.module.destructors.is_empty() && !debug {
            Vec::new()
        } else {
            let mut locals: Vec<u16> = (0..num_params).collect();
            for &reg in &self.frame_locals {
                if !locals.contains(&reg) {
                    locals.push(reg);
                }
            }
            locals
        };
        // Two tiers of debug info (the `-g1` vs `-g` split native toolchains use). The **line-info
        // tier** — `name`, `def_span`, and the pc→line `line_table` — is *always* emitted: it is pure
        // cold metadata (the dispatch loop never reads it) that production stack traces resolve
        // frames through, and it cannot perturb codegen by construction. The **full-debug tier** —
        // `debug_locals`, whose 1:1 `reg → name` contract requires pinning named locals through
        // coalescing and keeping them past their last-use drop — *does* change generated code, so it
        // stays gated on the debug compile (`noeta dap`) that opts into that trade.
        let debug_locals = if debug { self.debug_locals } else { Vec::new() };
        let line_table = self.line_table;
        let mut chunk = Chunk {
            code: self.code,
            consts: self.consts,
            diagnostics: self.diags,
            num_params,
            num_registers: self.next_reg,
            defaults,
            frame_locals,
            name,
            def_span,
            debug_locals,
            line_table,
        };
        // Coalescing reuses a dead local's slot for a later one; but a destructor-bearing local that
        // dies only at an *unreachable* drop (a dead store before a `panic`, whose scope-exit drop the
        // abort skips) would have its slot reused and its value lost before the panic teardown could
        // fire its `destruct` — diverging from the tree-walker, which keeps it as a live scope binding.
        // `coalesce` therefore **pins the `frame_locals` registers** (when present), keeping each in
        // its own slot so the teardown list stays accurate; temporaries — the bulk of the coalescing
        // win — still coalesce freely.
        // Hoist loop-invariant primitive-constant loads out of loops (P-VMT-LICM) on the monotonic
        // code, then coalesce — hoisting first lets coalescing give the pre-header load a slot with
        // its (now loop-spanning) live range.
        regalloc::hoist_loop_invariant_consts(&mut chunk);
        regalloc::coalesce(&mut chunk);
        chunk
    }

    fn alloc_reg(&mut self) -> Reg {
        let r = self.next_reg;
        self.next_reg += 1;
        r
    }

    fn add_const(&mut self, value: Const) -> u16 {
        let idx = self.consts.len() as u16;
        self.consts.push(value);
        idx
    }

    fn add_diag(&mut self, diag: Diagnostic) -> u16 {
        let idx = self.diags.len() as u16;
        self.diags.push(diag);
        idx
    }

    /// Whether we are at `main`'s global depth (no register scope pushed). A binding here is
    /// a global; once a block (or function) is entered, bindings become frame-locals.
    fn at_global_depth(&self) -> bool {
        self.is_main && self.scopes.is_empty()
    }

    fn lookup_local(&self, name: &str) -> Option<Var> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    fn resolve(&self, name: &str) -> Resolved {
        if let Some(var) = self.lookup_local(name) {
            return if var.celled {
                Resolved::CelledLocal(var.reg)
            } else {
                Resolved::Local(var.reg)
            };
        }
        // Inside a method, `self` and the class's fields resolve against the receiver. Locals
        // (parameters) are checked first, so a parameter shadows a same-named field — matching
        // the tree-walker, which binds fields, then `self`, then parameters (last wins).
        // (A bare name inside a method NEVER resolves to a field — prelude-redesign EX.1,
        // member access is explicit: `self.field` reads it; a bare name is a local/global.)
        if self.method.is_some() && name == "self" {
            return Resolved::SelfRecv;
        }
        // A name captured from an enclosing function resolves to an upvalue cell. Checked before
        // globals/prelude: the tree-walker captures the nearest lexical binding, so a same-named
        // global must not silently take its place.
        if let Some(&index) = self.upvalue_index.get(name) {
            return Resolved::Upvalue(index);
        }
        if self.is_main && self.globals.contains_key(name) {
            return Resolved::Global;
        }
        if PRELUDE_NAMES.contains(&name) {
            return Resolved::Prelude;
        }
        // Unknown in `main` (→ runtime E0005), or a forward/global reference inside a function.
        Resolved::Global
    }

    /// Compute, for a nested closure/`fn` `func`, the ordered upvalue list (name + mutability) it
    /// captures and the matching `CaptureFrom` source for each (in this — the building —
    /// function's terms). Returns `Unsupported` if a capture reaches a method's `self`/field or
    /// cannot be sourced (e.g. a forward capture of a not-yet-declared local), so such a program
    /// is skipped rather than miscompiled.
    fn resolve_captures(&self, func: &Func) -> Result<CaptureLayout, Unsupported> {
        let globals = self.module.global_names();
        let enclosing = self.child_enclosing();
        let free = freevars::free_vars(
            &func.params,
            &func.defaults,
            &func.body,
            &enclosing,
            &globals,
            func.captures.as_deref(),
        );
        let mut upvalues = Vec::with_capacity(free.len());
        let mut captures = Vec::with_capacity(free.len());
        for name in free {
            // The method receiver captures like an (immutable) local: boxed into a fresh cell at
            // closure creation (aether F3 — this was the long-standing VM `Unsupported`; the
            // reference interpreter always supported it). Inside the closure, `self` then
            // resolves as an ordinary upvalue, and `self.field` reads go through it.
            if name == "self" && self.method.is_some() {
                upvalues.push((name, false));
                captures.push(PendingCapture::SelfCell);
                continue;
            }
            if let Some(var) = self.lookup_local(&name) {
                if !var.celled {
                    return unsupported("a forward capture of a not-yet-celled local");
                }
                upvalues.push((name, var.mutable));
                captures.push(PendingCapture::From(CaptureFrom::Local(var.reg)));
            } else if let Some(&index) = self.upvalue_index.get(&name) {
                upvalues.push((name, self.upvalue_mut[index as usize]));
                captures.push(PendingCapture::From(CaptureFrom::Upvalue(index)));
            } else {
                // A free name the analysis flagged but that is neither a live celled local nor an
                // upvalue here (e.g. captured before its binding was lowered) — skip the program.
                return unsupported("a capture that could not be sourced from the enclosing frame");
            }
        }
        Ok((upvalues, captures))
    }

    /// Materialize a pending capture list into the wire [`CaptureFrom`]s, emitting the
    /// receiver-boxing `MakeCell` (register 0 → a fresh cell) for each [`PendingCapture::SelfCell`]
    /// immediately before the `MakeClosure` that consumes it.
    fn emit_captures(&mut self, pending: Vec<PendingCapture>) -> Vec<CaptureFrom> {
        pending
            .into_iter()
            .map(|c| match c {
                PendingCapture::From(from) => from,
                PendingCapture::SelfCell => {
                    let t = self.alloc_reg();
                    self.code.push(Op::MakeCell { dst: t, src: 0 });
                    CaptureFrom::Local(t)
                }
            })
            .collect()
    }

    fn stmt(&mut self, stmt: &Stmt) -> Result<(), Unsupported> {
        // Line table: record `(this statement's first pc, its span)` before emitting it, so any
        // instruction maps to a source line — including a statement (like a bare `return x`) that
        // compiles to only spanless ops. Always emitted (the line-info tier): the debugger breaks and
        // steps through it, and production stack traces resolve caller frames through it. Pure
        // metadata — never read by the dispatch loop, and it cannot affect codegen. Real source
        // statements only; a synthetic reclamation `Drop`/`DropVar`, a scope marker, or a nested
        // declaration is skipped so it cannot inject a *backward* line (a last-use drop sits at a
        // later pc but an earlier line) and is instead covered by the preceding real statement's
        // entry. Coalescing keeps pcs; the hoisting pass remaps them.
        if let Some(span) = line_entry_span(stmt) {
            self.line_table.push(LineEntry {
                pc: self.code.len() as u32,
                span,
            });
        }
        match stmt {
            // `let t = rvalue` — bind the operation's result to its frame temporary's register.
            Stmt::Let { dst, rvalue, .. } => {
                let reg = self.define_temp(*dst);
                self.rvalue(rvalue, reg)
            }
            // A bare expression statement: evaluate for effect into a scratch register, discard.
            Stmt::Eval { rvalue, .. } => {
                let reg = self.alloc_reg();
                self.rvalue(rvalue, reg)
            }
            // The discarded result of a bare expression statement (`Resource.new();`). Release its
            // register destructor-aware (Phase 4.4) so a destructor-bearing value used only as a
            // statement fires its `destruct` at last use — a temp is an owner too (spec §2).
            // `relevant: true` routes through `release_value`, which runtime-gates on reachability
            // (a non-destructor/immediate result just frees, as the reuse allocator did before).
            Stmt::Drop(t) => {
                let reg = self.temp_reg(*t);
                self.code.push(Op::Drop {
                    reg,
                    relevant: true,
                });
                Ok(())
            }
            // A source-variable drop (Phase 3 drop-insertion) releases the binding's value at its
            // last use. Only a value held **directly** in a frame register is released here, via
            // `Op::Drop` (clears the register to unit). A celled/captured local, an upvalue, a
            // global, or a method field is left alone (the drop pass already excludes captured
            // locals and globals; resolving to any of those means there is nothing this frame
            // uniquely owns to release). The IR's destructor-relevance bit (Phase 3.2b) rides along
            // as `Op::Drop.relevant`: a relevant drop fires the value's `destruct` at this last use
            // (Phase 4) if it is the final reference; an irrelevant one stays a plain release.
            Stmt::DropVar { name, relevant, .. } => {
                if let Resolved::Local(reg) = self.resolve(name) {
                    // Debug compiles keep **plain** named locals alive to frame teardown instead of
                    // freeing them at last use, so a debugger paused past a variable's final read
                    // still sees its value rather than the `unit` the last-use `Op::Drop` would leave
                    // (the register is pinned through coalescing, so nothing reuses the slot, and the
                    // frame's normal window teardown releases the surviving value exactly once — the
                    // interpreter path always releases the full window). This trades a little
                    // promptness for an inspectable stack; it is behaviour-invisible because a
                    // non-destructor value's reclamation is unobservable. A **destructor-bearing**
                    // (`relevant`) local is *not* skipped: its `destruct` must fire at last use (spec
                    // §2), and the window teardown uses a plain, non-destructor-aware release — so
                    // dropping it here is both spec-correct and required to run the destructor at all.
                    // Non-debug compiles are unaffected (the `Op::Drop` is emitted as before), so the
                    // production bytecode and the differential oracle stay byte-identical.
                    if self.module.debug && !*relevant {
                        return Ok(());
                    }
                    self.code.push(Op::Drop {
                        reg,
                        relevant: *relevant,
                    });
                }
                Ok(())
            }
            Stmt::Bind {
                mut_decl,
                name,
                name_span,
                value,
                field_assign,
                ..
            } => self.binding(*mut_decl, *field_assign, name, *name_span, value),
            Stmt::Echo { value, span } => {
                let t = self.atom_reg(value)?;
                // Route a `Display` object through its `to_string`; identity otherwise.
                let s = self.alloc_reg();
                self.code.push(Op::Stringify {
                    dst: s,
                    src: t,
                    span: *span,
                });
                self.code.push(Op::Echo { reg: s });
                Ok(())
            }
            Stmt::Return { value, span } => self.return_stmt(value.as_ref(), *span),
            Stmt::If {
                cond,
                then_block,
                else_block,
                span,
            } => self.if_stmt(cond, then_block, else_block.as_ref(), *span),
            Stmt::For {
                pattern,
                iterable,
                body,
                span,
                stream,
            } => self.for_stmt(pattern, iterable, body, *span, *stream),
            Stmt::While { cond, body, span } => self.while_stmt(cond, body, *span),
            // `break`/`continue` emit a placeholder `Jump` recorded on the innermost loop, which
            // patches it once its exit / continue target is known. The checker guarantees a loop is
            // present, so `loops` is non-empty here.
            Stmt::Break { .. } => {
                let site = self.code.len();
                self.code.push(Op::Jump { target: 0 });
                self.loops
                    .last_mut()
                    .expect("`break` inside a loop (checker-enforced)")
                    .breaks
                    .push(site);
                Ok(())
            }
            Stmt::Continue { .. } => {
                let site = self.code.len();
                self.code.push(Op::Jump { target: 0 });
                self.loops
                    .last_mut()
                    .expect("`continue` inside a loop (checker-enforced)")
                    .continues
                    .push(site);
                Ok(())
            }
            Stmt::ScopeBegin { .. } => {
                self.code.push(Op::ScopeBegin);
                Ok(())
            }
            Stmt::ScopeEnd { span } => {
                self.code.push(Op::ScopeEnd { span: *span });
                Ok(())
            }
            Stmt::Match {
                scrutinee,
                arms,
                dst,
                span,
            } => self.match_stmt(scrutinee, arms, *dst, *span),
            Stmt::Logical {
                dst,
                op,
                left,
                right,
                span,
            } => self.logical(*dst, *op, left, right, *span),
            Stmt::Coalesce {
                dst,
                value,
                fallback,
                span,
            } => self.coalesce(*dst, value, fallback, *span),
            Stmt::Decl(decl) => self.decl(decl),
        }
    }

    /// Lower a declaration statement. A `fn` binds a closure; type declarations were registered in
    /// the pre-pass and emit no code; `use std.{json, ...}` binds each native module as a global.
    fn decl(&mut self, decl: &Decl) -> Result<(), Unsupported> {
        match decl {
            Decl::Fn { name, func, .. } => self.declare_fn(name, func),
            Decl::Class(_) | Decl::Enum(_) | Decl::Struct(_) => Ok(()),
            Decl::Use { path, names, .. } => {
                for imported in names {
                    // Classification is `classify_use` — the same source of truth the checker
                    // resolved this import against, so binding can never diverge from checking.
                    match self.module.registry.classify_use(path, &imported.name) {
                        noeta_ext_abi::registry::UseKind::Module(qualified) => {
                            // The bound global keeps the imported name (the last segment); the
                            // module *value* carries the root-qualified identity so its member
                            // calls dispatch to the right module (`std.http.client` ≠ a
                            // third-party `guzzle.http.client`).
                            let value = self.alloc_reg();
                            let k = self.add_const(Const::NativeModule(qualified));
                            self.code.push(Op::LoadConst { dst: value, k });
                            let global = self.module.intern_global(&imported.name);
                            self.code.push(Op::StoreGlobal { global, src: value });
                        }
                        noeta_ext_abi::registry::UseKind::MemberFn { module, func } => {
                            // `use std.math.sqrt` — bind `sqrt` to a `(std.math, sqrt)`
                            // module-function value. An unknown member is left unbound (the
                            // checker reports it); a bare call then raises E0005 like any
                            // missing name.
                            let value = self.alloc_reg();
                            let k = self.add_const(Const::ModuleFn { module, func });
                            self.code.push(Op::LoadConst { dst: value, k });
                            let global = self.module.intern_global(&imported.name);
                            self.code.push(Op::StoreGlobal { global, src: value });
                        }
                        _ => {}
                    }
                }
                Ok(())
            }
        }
    }

    /// `fn name(params) { body }` — compile the body to a prototype, then bind `name` to a
    /// closure over it. A top-level `fn` is a global capturing only globals. A nested `fn` is a
    /// local that may capture enclosing locals as upvalues; if it (or a sibling) captures the
    /// `fn` itself, the binding is celled — and the cell is created *before* the closure is built
    /// so a self-recursive `fn` can capture its own (still-unset) cell.
    fn declare_fn(&mut self, name: &str, func: &Func) -> Result<(), Unsupported> {
        if self.at_global_depth() {
            let proto =
                self.module
                    .add_function(func, Vec::new(), Vec::new(), Some(name.to_string()))?;
            let t = self.alloc_reg();
            self.code.push(Op::MakeClosure {
                dst: t,
                proto,
                captures: Box::new([]),
            });
            self.globals
                .insert(name.to_string(), GlobalInfo { mutable: false });
            let global = self.module.intern_global(name);
            self.code.push(Op::StoreGlobal { global, src: t });
            return Ok(());
        }

        let celled = self.celled.contains(name);
        // A celled nested `fn` lives in a cell the block's hoist pass (`hoist_nested_fn_cells`)
        // pre-created before any sibling body was lowered — so a forward or mutual reference
        // (`even` calling `odd` declared later, or two mutually recursive fns) already sources a
        // live cell. Reuse that binding; fall back to creating the cell here if this declaration
        // was reached without a preceding hoist (defensive — every block hoists before lowering).
        let cell_reg = if celled {
            let existing = self.scopes.last().and_then(|s| s.get(name)).copied();
            Some(match existing {
                Some(var) if var.celled => var.reg,
                _ => self.make_fn_cell(name),
            })
        } else {
            None
        };
        let (upvalues, captures) = self.resolve_captures(func)?;
        let enclosing = self.child_enclosing();
        let proto = self
            .module
            .add_function(func, upvalues, enclosing, Some(name.to_string()))?;
        let t = self.alloc_reg();
        let captures = self.emit_captures(captures).into_boxed_slice();
        self.code.push(Op::MakeClosure {
            dst: t,
            proto,
            captures,
        });
        if let Some(cell) = cell_reg {
            self.code.push(Op::CellSet { cell, src: t });
        } else {
            // `t` holds a just-built closure read nowhere else — the local adopts it (consuming move).
            self.declare_local(name, t, true, false, func.span);
        }
        Ok(())
    }

    /// Create a fresh unit-holding cell for a nested `fn` binding `name`, binding it (celled) in the
    /// current scope and returning the cell's register. The closure value is stored into the cell
    /// (`CellSet`) once built. Shared by the block hoist pass and [`Self::declare_fn`]'s fallback.
    fn make_fn_cell(&mut self, name: &str) -> Reg {
        let reg = self.alloc_reg();
        let unit = self.alloc_reg();
        let k = self.add_const(Const::Unit);
        self.code.push(Op::LoadConst { dst: unit, k });
        self.code.push(Op::MakeCell {
            dst: reg,
            src: unit,
        });
        self.scopes.last_mut().unwrap().insert(
            name.to_string(),
            Var {
                reg,
                mutable: false,
                celled: true,
            },
        );
        reg
    }

    /// Pre-create the upvalue cells for a block's **directly nested `fn` declarations** (F1 forward /
    /// mutual capture) — the codegen mirror of the checker's `bind_nested_fns`. A captured (celled)
    /// nested `fn` is bound to a fresh unit-holding cell *before* any sibling body is lowered, so a
    /// forward reference (`a` calling `b` declared later) or mutual recursion (`even`/`odd`) sources a
    /// live cell instead of failing to resolve ("a capture that could not be sourced from the
    /// enclosing frame"). The closure value is stored into the cell when [`Self::declare_fn`] reaches
    /// its declaration. Only celled fns need a cell; a `fn` nobody captures binds directly as a plain
    /// local at its declaration site. Nested `fn`s in sub-blocks (an `if` body, a match arm) are
    /// hoisted by that block's own scope, so only the direct statements are scanned here — matching
    /// the strictly-lexical visibility a non-`fn` local keeps (a forward reference to one is E0005).
    fn hoist_nested_fn_cells(&mut self, stmts: &[Stmt]) {
        // At `main`'s global depth a top-level `fn` is a global (resolved by name regardless of
        // order via `register_globals`), never a cell — so nothing to hoist.
        if self.at_global_depth() {
            return;
        }
        for stmt in stmts {
            if let Stmt::Decl(Decl::Fn { name, .. }) = stmt
                && self.celled.contains(name)
                && !self.scopes.last().is_some_and(|s| s.contains_key(name))
            {
                self.make_fn_cell(name);
            }
        }
    }

    /// Prime `main`'s closure-conversion state from its top-level body. Unlike an ordinary function
    /// (compiled through [`Self::compile_chunk`], which runs the free-variable analysis and seeds
    /// `celled`/`local_layer`), `main` is lowered by iterating `ir.top.stmts` directly, so without
    /// this its `celled`/`local_layer` sets stay empty. That empties the celling decision for a `fn`
    /// nested in a **top-level block** (`while { fn rec(){ ... rec() ... } }`): the block's hoist
    /// pass finds nothing to cell, so the self-reference resolves to a `LoadGlobal` no one stores
    /// (E0005), diverging from the reference interpreter which runs it fine.
    ///
    /// `main`'s peculiarity is that its **depth-0** bindings are module globals, not frame locals
    /// (see [`Self::at_global_depth`]). The free-variable analysis, which treats every binding as a
    /// local, is therefore filtered by the module-global set: a top-level `fn`/`mut`/import stays a
    /// global (referenced by name, never captured), while a binding introduced inside a top-level
    /// block — the only genuine `main` locals — is kept, so a nested `fn` that captures it (self or
    /// mutual recursion) resolves to its cell/upvalue exactly as inside any other function.
    fn setup_main_scopes(&mut self, top: &Block) {
        let globals = self.module.global_names();
        // `main` (the top level) is never sealed — `None` keeps the full outward rule.
        let analysis = freevars::analyze(&[], &[], top, &[], &globals, None);
        self.local_layer = &analysis.local - &globals;
        self.celled = &analysis.celled - &globals;
    }

    fn return_stmt(&mut self, value: Option<&Atom>, _span: Span) -> Result<(), Unsupported> {
        // A `return` unwinds past every enclosing `for`-loop's post-loop iterator drop, so drop each
        // owned temp iterator destructor-aware here — innermost first (reverse-scope order) — running a
        // generator's captured local's destructor. (Named/snapshotted iterables carry `None`.)
        let iters: Vec<Reg> = self
            .loops
            .iter()
            .rev()
            .filter_map(|ctx| ctx.stream_iter)
            .collect();
        for iter in iters {
            self.code.push(Op::Drop {
                reg: iter,
                relevant: true,
            });
        }
        let src = match value {
            Some(atom) => self.atom_reg(atom)?,
            None => {
                let t = self.alloc_reg();
                let k = self.add_const(Const::Unit);
                self.code.push(Op::LoadConst { dst: t, k });
                t
            }
        };
        self.code.push(Op::Return { src });
        Ok(())
    }

    /// `if cond { then } else { else }`, lowered to a bool-check and forward jumps. Mirrors
    /// the tree-walker: a non-bool condition is E0007 at the `if`'s span, and each branch
    /// body runs in its own (block) scope. The condition is a pre-computed bool atom.
    fn if_stmt(
        &mut self,
        cond: &Atom,
        then_block: &Block,
        else_block: Option<&Block>,
        span: Span,
    ) -> Result<(), Unsupported> {
        let rc = self.atom_reg(cond)?;
        let jf = self.push_cond_branch(rc, span);

        self.block(then_block)?;

        match else_block {
            Some(else_block) => {
                let j_end = self.code.len();
                self.code.push(Op::Jump { target: 0 });
                let else_start = self.code.len() as u32;
                self.patch_jump(jf, else_start);
                self.block(else_block)?;
                let end = self.code.len() as u32;
                self.patch_jump(j_end, end);
            }
            None => {
                let end = self.code.len() as u32;
                self.patch_jump(jf, end);
            }
        }
        Ok(())
    }

    /// `for <pattern> in <iterable> { body }`, lowered to an index loop over a snapshot of
    /// the iterable's elements. Mirrors the tree-walker: the iterable is snapshotted once
    /// (`IterSnapshot`), a non-iterable is E0007 at the `for`'s span, each iteration binds the
    /// pattern in a fresh body scope, and a map iterates its values in sorted-key order.
    fn for_stmt(
        &mut self,
        pattern: &ForPattern,
        iterable: &Atom,
        body: &Block,
        span: Span,
        stream: bool,
    ) -> Result<(), Unsupported> {
        // A `for` over a statically-known `Iterator<T>` (Track I.2) drives `next()` one element at a
        // time, so a lazy source streams (and `break` stops early) — no list snapshot.
        if stream {
            return self.for_stream_stmt(pattern, iterable, body, span);
        }
        // The loop's bookkeeping registers live in the enclosing frame (a list snapshot, its
        // length, the running index, and a constant 1 to advance it). The iterable is a
        // pre-computed atom, snapshotted once into a fresh register.
        let src = self.atom_reg(iterable)?;
        let items = self.alloc_reg();
        self.code.push(Op::IterSnapshot {
            dst: items,
            src,
            span,
        });
        let len = self.alloc_reg();
        self.code.push(Op::ListLen {
            dst: len,
            src: items,
            span,
        });
        let index = self.alloc_reg();
        let zero = self.add_const(Const::Int(0));
        self.code.push(Op::LoadConst {
            dst: index,
            k: zero,
        });
        let one_reg = self.alloc_reg();
        let one = self.add_const(Const::Int(1));
        self.code.push(Op::LoadConst {
            dst: one_reg,
            k: one,
        });

        let loop_top = self.code.len() as u32;
        let cond = self.alloc_reg();
        self.code.push(Op::Binary {
            op: BinaryOp::Lt,
            dst: cond,
            a: index,
            b: len,
            span,
        });
        let exit_jump = self.code.len();
        self.code.push(Op::JumpIfFalse {
            reg: cond,
            target: 0,
        });

        // Fetch the current element and bind the loop pattern in a fresh body scope.
        let element = self.alloc_reg();
        self.code.push(Op::ListGet {
            dst: element,
            list: items,
            index,
        });
        self.scopes.push(HashMap::new());
        self.loops.push(LoopCtx::default());
        let result = (|| {
            match pattern {
                ForPattern::Single { name, name_span } => {
                    self.bind_loop_var(name, element, *name_span);
                }
                // A tuple for-pattern is desugared to a `Single` hidden var + `.N` projections in
                // lowering (object-model slice 4b), so it never reaches the compiler.
                ForPattern::Tuple { .. } => {
                    unreachable!("a tuple for-pattern is desugared to projections in IR lowering")
                }
            }
            self.hoist_nested_fn_cells(&body.stmts);
            for stmt in &body.stmts {
                self.stmt(stmt)?;
            }
            Ok(())
        })();
        let ctx = self.loops.pop().expect("loop context");
        self.scopes.pop();
        result?;

        // `continue` skips to the index increment (so the loop still advances), which begins here.
        let increment = self.code.len() as u32;
        for site in ctx.continues {
            self.patch_jump(site, increment);
        }
        // Advance the index and loop back; patch the exit once the end is known.
        self.code.push(Op::Binary {
            op: BinaryOp::Add,
            dst: index,
            a: index,
            b: one_reg,
            span,
        });
        self.code.push(Op::Jump { target: loop_top });
        let end = self.code.len() as u32;
        self.patch_jump(exit_jump, end);
        // `break` lands past the back-edge.
        for site in ctx.breaks {
            self.patch_jump(site, end);
        }
        Ok(())
    }

    /// Streaming `for` over an `Iterator<T>` (Track I.2): drive `next()` each iteration via
    /// [`Op::IterForNext`] (which runs any `map`/`filter` closure), bind the element, run the body,
    /// and loop until the iterator is exhausted. No list snapshot — a lazy source streams and an early
    /// `break` stops it. The iterator value stays in its source register, released by the post-`for`
    /// `Drop` of its temp (the same machinery the snapshot path relies on).
    fn for_stream_stmt(
        &mut self,
        pattern: &ForPattern,
        iterable: &Atom,
        body: &Block,
        span: Span,
    ) -> Result<(), Unsupported> {
        let iter = self.atom_reg(iterable)?;
        let elem = self.alloc_reg();
        let has = self.alloc_reg();

        let loop_top = self.code.len() as u32;
        self.code.push(Op::IterForNext {
            iter,
            elem,
            has,
            span,
        });
        let exit_jump = self.code.len();
        self.code.push(Op::JumpIfFalse {
            reg: has,
            target: 0,
        });

        self.scopes.push(HashMap::new());
        self.loops.push(LoopCtx::default());
        // A temp iterable is owned by this loop; record its register so an early `return` inside the
        // body drops it destructor-aware (a named iterable's binding is dropped at its own scope end).
        if matches!(iterable, Atom::Temp(_)) {
            self.loops.last_mut().unwrap().stream_iter = Some(iter);
        }
        let result = (|| {
            match pattern {
                ForPattern::Single { name, name_span } => {
                    self.bind_loop_var(name, elem, *name_span);
                }
                // A tuple for-pattern is desugared to a `Single` hidden var + `.N` projections in
                // lowering (object-model slice 4b), so it never reaches the compiler.
                ForPattern::Tuple { .. } => {
                    unreachable!("a tuple for-pattern is desugared to projections in IR lowering")
                }
            }
            self.hoist_nested_fn_cells(&body.stmts);
            for stmt in &body.stmts {
                self.stmt(stmt)?;
            }
            Ok(())
        })();
        let ctx = self.loops.pop().expect("loop context");
        self.scopes.pop();
        result?;

        // `continue` re-advances the iterator (there is no separate index increment to skip to).
        for site in ctx.continues {
            self.patch_jump(site, loop_top);
        }
        self.code.push(Op::Jump { target: loop_top });
        let end = self.code.len() as u32;
        self.patch_jump(exit_jump, end);
        for site in ctx.breaks {
            self.patch_jump(site, end);
        }
        // A temp iterable is owned by this loop (no binding holds it), so release it destructor-aware at
        // every loop exit — exhaustion (the `JumpIfFalse` above lands here) and `break` (patched here) —
        // running a generator's captured destructor-bearing local at the iterator's last reference. A
        // *named* iterable's binding outlives the loop and is dropped at its own scope end, so it must
        // not be dropped here (which would also empty the binding). (`continue` re-loops, not here; an
        // early `return` unwinds past this — the abandoned-on-return case stays a plain release.)
        if matches!(iterable, Atom::Temp(_)) {
            self.code.push(Op::Drop {
                reg: iter,
                relevant: true,
            });
        }
        Ok(())
    }

    /// `while <cond> { body }`, lowered to a top-tested loop: evaluate the condition, require it
    /// be a bool, exit if false, run the body in a fresh scope, then jump back. Mirrors the
    /// tree-walker — a bare reassignment in the body updates its enclosing binding's register, so
    /// the condition makes progress. The condition is a re-evaluated **block** (its `let`s plus a
    /// tail bool atom); its straight-line code runs in the enclosing scope each iteration.
    fn while_stmt(&mut self, cond: &Block, body: &Block, span: Span) -> Result<(), Unsupported> {
        let loop_top = self.code.len() as u32;
        let rc = self.value_block(cond)?;
        let exit_jump = self.push_cond_branch(rc, span);

        self.loops.push(LoopCtx::default());
        let result = self.block(body);
        let ctx = self.loops.pop().expect("loop context");
        result?;

        // `continue` re-tests the condition, so it targets the loop top.
        for site in ctx.continues {
            self.patch_jump(site, loop_top);
        }
        self.code.push(Op::Jump { target: loop_top });
        let end = self.code.len() as u32;
        self.patch_jump(exit_jump, end);
        // `break` lands just past the back-edge.
        for site in ctx.breaks {
            self.patch_jump(site, end);
        }
        Ok(())
    }

    /// Register an already-populated register as an immutable loop-body binding. Unlike
    /// [`FnCompiler::declare_local`] this emits no `Move`: the element/destructure op has
    /// already written the value into `reg`.
    fn bind_loop_var(&mut self, name: &str, reg: Reg, def_span: Span) {
        let celled = self.celled.contains(name);
        // A captured loop variable is boxed in place; the `MakeCell` sits inside the loop body, so
        // each iteration captures a distinct cell (matching the tree-walker's per-iteration scope).
        if celled {
            self.code.push(Op::MakeCell { dst: reg, src: reg });
        }
        // In a debug compile a loop/match binding is a named local the debugger's Variables view
        // should see, like any `declare_local` binding. Deliberately NOT added to `frame_locals`:
        // that list is also the panic-teardown list, and a debug compile must not change which
        // destructors fire. Its register is pinned through coalescing via `debug_locals` itself
        // (see regalloc's debug-locals pin), so the 1:1 `reg → name` contract still holds.
        if self.module.debug {
            self.debug_locals.push(LocalDebug {
                name: name.to_string(),
                reg,
                def_span,
            });
        }
        self.scopes.last_mut().unwrap().insert(
            name.to_string(),
            Var {
                reg,
                mutable: false,
                celled,
            },
        );
    }

    /// Compile a statement-position block (an `if`/`while` body) in its own (block) scope.
    fn block(&mut self, block: &Block) -> Result<(), Unsupported> {
        self.scopes.push(HashMap::new());
        let result = (|| {
            self.hoist_nested_fn_cells(&block.stmts);
            for stmt in &block.stmts {
                self.stmt(stmt)?;
            }
            Ok(())
        })();
        self.scopes.pop();
        result
    }

    /// Compile a value-position block (a `while` condition, a `&&`/`||` right operand, a `??`
    /// fallback) **inline** in the current scope — its straight-line `let`s were sub-expressions,
    /// not a new lexical scope — and return the register holding its tail atom.
    fn value_block(&mut self, block: &Block) -> Result<Reg, Unsupported> {
        for stmt in &block.stmts {
            self.stmt(stmt)?;
        }
        let tail = block
            .tail
            .as_ref()
            .expect("a value-position block always has a tail atom");
        self.atom_reg(tail)
    }

    /// Emit a fused conditional branch (P-VMT-CBR) and return its code index (the patch site). One
    /// `CondBranch` replaces the adjacent `RequireCondBool` + `JumpIfFalse` pair every `if`/`while`
    /// condition test used to emit — the bool-check and the false-branch in a single dispatch.
    fn push_cond_branch(&mut self, rc: Reg, span: Span) -> usize {
        let site = self.code.len();
        self.code.push(Op::CondBranch {
            reg: rc,
            target: 0,
            span,
        });
        site
    }

    fn patch_jump(&mut self, at: usize, target: u32) {
        match &mut self.code[at] {
            Op::Jump { target: t }
            | Op::JumpIfFalse { target: t, .. }
            | Op::JumpIfTrue { target: t, .. }
            | Op::CondBranch { target: t, .. }
            | Op::Coalesce { fallback: t, .. }
            | Op::MatchInt { fail: t, .. }
            | Op::MatchStr { fail: t, .. }
            | Op::MatchBool { fail: t, .. }
            | Op::MatchVariant { fail: t, .. }
            | Op::MatchTuple { fail: t, .. } => *t = target,
            _ => unreachable!("patching a jump we just emitted"),
        }
    }

    /// `mut x = v`, an immutable `x = v` declaration, or a reassignment — mirroring the
    /// tree-walker's `bind`: the value is always evaluated first, then the binding rule
    /// applies (so a reassignment to an immutable still runs the value's side effects).
    fn binding(
        &mut self,
        mut_decl: bool,
        field_assign: bool,
        name: &str,
        name_span: Span,
        value: &Atom,
    ) -> Result<(), Unsupported> {
        // The value is a pre-computed atom (its side effects already ran in the preceding `let`s);
        // `atom_reg_owned` only materializes the register holding it and reports whether a
        // declaration may consume (adopt) that register. The binding rule then applies — so a
        // reassignment to an immutable still runs the value (no observable change), matching the
        // tree-walker's `bind`. (Only the declaration paths use `owned`; the store/reassign paths
        // retain on write, so a borrowed source is fine there.)
        let (src, owned) = self.atom_reg_owned(value)?;

        if mut_decl {
            if self.at_global_depth() {
                self.globals
                    .insert(name.to_string(), GlobalInfo { mutable: true });
                let global = self.module.intern_global(name);
                self.code.push(Op::StoreGlobal { global, src });
            } else {
                self.declare_local(name, src, owned, true, name_span);
            }
            return Ok(());
        }

        // A bare `x = v`: reassign the nearest existing binding (searching local scopes, then
        // captured upvalues, then globals — mirroring the tree-walker's outward `Scope::assign`),
        // else declare a fresh local.
        if let Some(var) = self.lookup_local(name) {
            // A field-set `x.f = v` skips the immutability check (object-model slice 2b′): the
            // checker has already rejected a `struct` field-set on an immutable `x` statically, and a
            // `class` field-set mutates in place (this store just restores `x` to that instance).
            if !var.mutable && !field_assign {
                let idx = self.add_diag(immutable_diag(name, name_span));
                self.code.push(Op::Raise { idx });
            } else if var.celled {
                self.code.push(Op::CellSet { cell: var.reg, src });
            } else {
                // A reassignment destroys the **displaced** value (spec §5): its destructor runs at
                // the assignment point if this was its last reference. The tree-walker does this
                // unconditionally (`Scope::assign` → `destroy_value`), and `StoreGlobal` already
                // does it for globals; for a local register, fire it explicitly before the overwrite
                // (`Op::Drop` is destructor-aware and clears the slot, so the following `Move` writes
                // into `unit`). Skip a degenerate self-assignment, where `src` *is* the slot.
                if src != var.reg {
                    self.code.push(Op::Drop {
                        reg: var.reg,
                        relevant: true,
                    });
                }
                self.code.push(Op::Move { dst: var.reg, src });
                // When the value came from a dead **owned** source (an ANF temp — single-use by the
                // ANF invariant — or a freshly-materialized constant/global/field), the retaining
                // `Move` above left a now-redundant alias of it in `src`. Release that alias so the
                // binding becomes the **sole owner** (a retaining move + this drop = a *consuming*
                // move). This is what lets an accumulator self-update (`x = x.add(i)`,
                // `x = T { ...x, … }`) see `refcount == 1` and reuse its backing allocation in place on
                // the next iteration, rather than the stale temp alias forcing a copy every step. The
                // binding holds the value, so this is never its last reference — a plain drop. A
                // *borrowed* source (a live local / `self`) keeps its register and is not dropped.
                if owned && src != var.reg {
                    self.code.push(Op::Drop {
                        reg: src,
                        relevant: false,
                    });
                }
            }
            return Ok(());
        }

        // A captured upvalue: reassign through its cell, enforcing the source binding's mutability.
        if let Some(&index) = self.upvalue_index.get(name) {
            if self.upvalue_mut[index as usize] || field_assign {
                self.code.push(Op::UpvalueSet { index, src });
            } else {
                let idx = self.add_diag(immutable_diag(name, name_span));
                self.code.push(Op::Raise { idx });
            }
            return Ok(());
        }

        // A global: in `main` only the globals declared so far are visible (matching the
        // tree-walker's not-yet-declared lookups); inside a function every module global is —
        // unless the function is SEALED, where only `use (…)`-listed names reach a global store
        // (any other name falls through to a fresh local, exactly as the checker typed it).
        let sealed_out = self
            .seal
            .as_ref()
            .is_some_and(|allow| !allow.contains(name) && !name.starts_with('$'));
        let global_mut = if sealed_out {
            None
        } else if self.is_main {
            self.globals.get(name).map(|info| info.mutable)
        } else {
            self.module.module_globals.get(name).copied()
        };
        if let Some(mutable) = global_mut {
            if mutable || field_assign {
                let global = self.module.intern_global(name);
                self.code.push(Op::StoreGlobal { global, src });
            } else {
                let idx = self.add_diag(immutable_diag(name, name_span));
                self.code.push(Op::Raise { idx });
            }
            return Ok(());
        }

        // Not found anywhere: a new immutable binding in the current scope.
        if self.scopes.is_empty() {
            self.globals
                .insert(name.to_string(), GlobalInfo { mutable: false });
            let global = self.module.intern_global(name);
            self.code.push(Op::StoreGlobal { global, src });
        } else {
            self.declare_local(name, src, owned, false, name_span);
        }
        Ok(())
    }

    /// Bind `name` to a local register initialized from `src`, reusing the slot if the name already
    /// exists in the innermost scope (a re-`mut` shadow). A captured (celled) local boxes `src` into
    /// a fresh cell instead — re-run each time the binding executes (e.g. per loop iteration), so
    /// each entry gets a distinct cell, matching the tree-walker's fresh per-iteration scope.
    ///
    /// `owned` marks `src` as a value the binding may **consume**: a single-use ANF temporary or a
    /// freshly materialized constant/global/field/closure that no live binding holds. When it is
    /// (and we are not writing into an existing shadowed slot), the local **adopts** `src`'s register
    /// as its home — a *consuming move* with no copying `Op::Move` (the value is already there). A
    /// borrowed source (a directly-held local or `self`, whose slot another live binding owns) is
    /// copied into a fresh slot so a later reassignment of either binding cannot disturb the other.
    fn declare_local(&mut self, name: &str, src: Reg, owned: bool, mutable: bool, def_span: Span) {
        let celled = self.celled.contains(name);
        let reg = match self.scopes.last().unwrap().get(name) {
            // A re-`mut` shadow writes into the existing slot — no adoption.
            Some(v) => v.reg,
            // An owned source: adopt its register as the local's home (consuming move).
            None if owned => src,
            // A borrowed source: copy into a fresh slot.
            None => self.alloc_reg(),
        };
        if celled {
            // A captured local always boxes; an adopted owned source boxes *in place* (`dst == src`).
            self.code.push(Op::MakeCell { dst: reg, src });
        } else if reg != src {
            // A plain adoption (`reg == src`) needs no copy; only a fresh/shadowed slot does.
            self.code.push(Op::Move { dst: reg, src });
        }
        // Record this local's home register for the panic-teardown list, in declaration (≈
        // construction) order. A re-`mut` shadow reuses the slot, so record a register only the first
        // time it appears; the VM fires each register once anyway (a second is a no-op on `unit`).
        if !self.frame_locals.contains(&reg) {
            self.frame_locals.push(reg);
        }
        // In a debug compile, keep the source name → register mapping for the debugger's Variables
        // view. A re-`mut` shadow reuses the slot but re-declares the name; record the latest span so
        // the reported location tracks the live declaration. (Register-remapped later by coalescing.)
        if self.module.debug {
            self.debug_locals.push(LocalDebug {
                name: name.to_string(),
                reg,
                def_span,
            });
        }
        self.scopes.last_mut().unwrap().insert(
            name.to_string(),
            Var {
                reg,
                mutable,
                celled,
            },
        );
    }

    /// Produce a register holding `atom`'s value. A temporary or a directly-held local / `self`
    /// resolves to its existing register (operands are read-only); a constant, or a celled /
    /// captured / global / field name, is materialized into a fresh register.
    fn atom_reg(&mut self, atom: &Atom) -> Result<Reg, Unsupported> {
        match atom {
            Atom::Const(c) => {
                let dst = self.alloc_reg();
                let k = self.add_const(const_value(c));
                self.code.push(Op::LoadConst { dst, k });
                Ok(dst)
            }
            Atom::Temp(t) => Ok(self.temp_reg(*t)),
            Atom::Var { name, span } => self.var_reg(name, *span),
        }
    }

    /// Like [`FnCompiler::atom_reg`], but also reports whether the returned register is **owned** —
    /// freshly produced for this read and held by no other live binding, so a declaration may adopt
    /// it as its home (a consuming move). The borrowed cases are a directly-held local or `self`,
    /// whose slot another live binding owns; everything else (an ANF temporary — single-use by the
    /// ANF invariant — a materialized constant/global/field/upvalue) is owned.
    fn atom_reg_owned(&mut self, atom: &Atom) -> Result<(Reg, bool), Unsupported> {
        match atom {
            // A single-use temporary: this read is its only consumer, so its slot is free to adopt.
            Atom::Temp(t) => Ok((self.temp_reg(*t), true)),
            // A constant materializes into its own fresh register.
            Atom::Const(_) => Ok((self.atom_reg(atom)?, true)),
            Atom::Var { name, span } => match self.resolve(name) {
                // A live local / receiver — its register is borrowed; copy on declaration.
                Resolved::Local(_) | Resolved::SelfRecv => Ok((self.var_reg(name, *span)?, false)),
                // Celled / upvalue / field / global / prelude — `var_reg` materializes a fresh slot.
                _ => Ok((self.var_reg(name, *span)?, true)),
            },
        }
    }

    /// Materialize each atom in `atoms` into a register, in order.
    fn atom_regs(&mut self, atoms: &[Atom]) -> Result<Box<[Reg]>, Unsupported> {
        let mut regs = Vec::with_capacity(atoms.len());
        for a in atoms {
            regs.push(self.atom_reg(a)?);
        }
        Ok(regs.into_boxed_slice())
    }

    /// Materialize an **aggregate-constructor operand**, recording (in `consumed`) the register to
    /// release after the constructor runs. An aggregate op (`MakeList`/`MakeMap`/`MakeStruct`/
    /// `MakeEnum`) *retains* each operand into the new heap value, so an **owned heap** operand — an
    /// ANF temp or a freshly-materialized string/global/field/upvalue, held by no live binding — is
    /// left with a now-redundant reference in its register. Releasing it promptly (a plain drop the
    /// caller emits right after the constructor) makes the aggregate the **sole owner at this
    /// point**, so a *same-frame* destruction sees refcount 1 and fires the contained destructor in
    /// declared order — matching the tree-walker, which *moves* its temporaries into the aggregate
    /// (`Frame::take`). A **borrowed** operand (a live local / `self`) is not released — its binding
    /// keeps the reference, and the aggregate's own retained copy is what the later element drop
    /// reclaims. An immediate constant (int/bool/unit/float) has no refcount, so nothing to release.
    ///
    /// The drop's read of the operand register also keeps it live across the constructor, so register
    /// coalescing cannot fuse the operand with the constructor's destination — the release stays
    /// sound by construction.
    fn consume_operand(
        &mut self,
        atom: &Atom,
        consumed: &mut Vec<Reg>,
    ) -> Result<Reg, Unsupported> {
        if let Atom::Const(c) = atom
            && is_immediate_const(c)
        {
            return self.atom_reg(atom);
        }
        let (reg, owned) = self.atom_reg_owned(atom)?;
        if owned {
            consumed.push(reg);
        }
        Ok(reg)
    }

    /// Emit a plain drop for each owned-heap constructor operand recorded by [`Self::consume_operand`],
    /// releasing the redundant reference the constructor's retain left behind.
    fn release_consumed(&mut self, consumed: &[Reg]) {
        for &reg in consumed {
            self.code.push(Op::Drop {
                reg,
                relevant: false,
            });
        }
    }

    /// After an access (field / method / index) consumes a receiver register, release the receiver
    /// if it was an owned ANF **temporary** — firing its destructor at last use (Phase 4.4): a
    /// destructor-bearing value used *only* as a receiver (`Resource.new().use()`, the `b.inner`
    /// projected in `b.inner.tag`) would otherwise never fire. A non-temp receiver (a live local or
    /// a type name) is left alone — its binding fires at its own drop, or it is borrowed. The access
    /// op retains whatever it keeps, so the temp's remaining register reference is now dead; the
    /// drop's read also keeps it live across the access, so coalescing cannot fuse it with the
    /// destination. `relevant: true` routes through `release_value`, which runtime-gates on
    /// reachability (a non-destructor receiver simply frees, as register reuse did before).
    fn drop_temp_receiver(&mut self, receiver: &Atom, recv: Reg) {
        if matches!(receiver, Atom::Temp(_)) {
            self.code.push(Op::Drop {
                reg: recv,
                relevant: true,
            });
        }
    }

    /// Resolve a source-variable reference to a register holding its value, mirroring the
    /// tree-walker's name resolution (`Expr::Ident` evaluation).
    fn var_reg(&mut self, name: &str, span: Span) -> Result<Reg, Unsupported> {
        match self.resolve(name) {
            // A directly-held local or the method receiver is read in place (operands are read-only;
            // any op that stores the value retains it, so a later reassignment cannot disturb it).
            Resolved::Local(reg) => Ok(reg),
            Resolved::SelfRecv => Ok(0),
            Resolved::CelledLocal(cell) => {
                let dst = self.alloc_reg();
                self.code.push(Op::CellGet { dst, cell });
                Ok(dst)
            }
            Resolved::Upvalue(index) => {
                let dst = self.alloc_reg();
                self.code.push(Op::UpvalueGet { dst, index });
                Ok(dst)
            }
            Resolved::Global => {
                let dst = self.alloc_reg();
                let global = self.module.intern_global(name);
                self.code.push(Op::LoadGlobal { dst, global, span });
                Ok(dst)
            }
            // `none` is the one prelude *value* (not a function): the `Option.none` variant.
            Resolved::Prelude if name == "none" => {
                let dst = self.alloc_reg();
                let shape = self.module.builtin_enum_shape("Option", "none");
                self.code.push(Op::MakeEnum {
                    dst,
                    shape,
                    args: Box::new([]),
                    reflect: None,
                });
                Ok(dst)
            }
            // A bare reference to a prelude builtin becomes a first-class native-function value (a
            // direct call still uses its fast op / `CallBuiltin`). This covers the collection
            // builtins AND (poly-values F3) the constructors `Ok`/`Err`/`some` and `panic`, so
            // `results.map(Ok)` passes a genuine callable — both backends construct the same value
            // family, and a call through it shares `call_builtin`'s exact arity/error text.
            Resolved::Prelude => match Builtin::from_name(name) {
                Some(func) => {
                    let dst = self.alloc_reg();
                    self.code.push(Op::LoadNativeFn { dst, func });
                    Ok(dst)
                }
                None => unsupported("reference to a prelude value/builtin"),
            },
        }
    }

    /// The register a result is written to: a temporary's allocated register, or — in a discard
    /// position (`dst == None`) — a fresh scratch register.
    fn dst_reg(&mut self, dst: Option<Temp>) -> Reg {
        match dst {
            Some(t) => self.define_temp(t),
            None => self.alloc_reg(),
        }
    }

    /// Lower a primitive operation ([`Rvalue`]) into register `dst`. Operands are already atoms
    /// (ANF), so this is a near-1:1 mapping to bytecode — no recursive flattening.
    fn rvalue(&mut self, rvalue: &Rvalue, dst: Reg) -> Result<(), Unsupported> {
        match rvalue {
            Rvalue::Use(atom) => {
                // Copy the atom into `dst` (a retaining `Move`), so the temp is an independent
                // snapshot even if its source is later reassigned.
                let src = self.atom_reg(atom)?;
                self.code.push(Op::Move { dst, src });
                Ok(())
            }
            Rvalue::Unary { op, operand, span } => {
                let src = self.atom_reg(operand)?;
                self.code.push(Op::Unary {
                    op: *op,
                    dst,
                    src,
                    span: *span,
                });
                Ok(())
            }
            Rvalue::MaskWidth {
                operand,
                signed,
                bits,
                ..
            } => {
                let src = self.atom_reg(operand)?;
                self.code.push(Op::MaskWidth {
                    dst,
                    src,
                    signed: *signed,
                    bits: *bits,
                });
                Ok(())
            }
            // Sign-dependent fixed-width `/ % < <= > >=` (Tier W3): a single width-carrying op the VM
            // resolves via `apply_binary_wide`. No trait dispatch (operands are erased ints), no
            // reuse — always the copying form.
            Rvalue::WideInt {
                op,
                lhs,
                rhs,
                signed,
                bits,
                span,
            } => {
                let a = self.atom_reg(lhs)?;
                let b = self.atom_reg(rhs)?;
                self.code.push(Op::WideInt {
                    op: *op,
                    dst,
                    a,
                    b,
                    signed: *signed,
                    bits: *bits,
                    span: *span,
                });
                Ok(())
            }
            // Width-exact bit intrinsic on a fixed-width receiver (Tier W5): a single op the backend
            // computes via `int_method_width`. `arg` is the sole `rotate_*` amount (absent otherwise).
            Rvalue::WidthIntMethod {
                receiver,
                method,
                args,
                bits,
                span,
            } => {
                let recv = self.atom_reg(receiver)?;
                let arg = match args.first() {
                    Some(a) => Some(self.atom_reg(a)?),
                    None => None,
                };
                self.code.push(Op::WidthIntMethod {
                    dst,
                    recv,
                    method: *method,
                    arg,
                    bits: *bits,
                    span: *span,
                });
                Ok(())
            }
            // `&&`/`||` never reach here (they lower to `Stmt::Logical`); every other infix does.
            Rvalue::Binary {
                op,
                lhs,
                rhs,
                reuse,
                span,
            } => {
                // In-place self-append (Phase 5.1b): a marked `acc = acc ~ rhs` whose accumulator is a
                // directly-held local or a top-level global reuses that list's buffer via `ConcatInPlace`
                // instead of the copying `~`. The right operand is resolved *before* the accumulator is
                // taken so a `TakeGlobal` cannot vacate a slot the rhs still reads (the reuse pass also
                // guarantees `rhs` does not mention the base). A celled/captured/upvalue base is not
                // handled — it falls through to the copying `Op::Binary`, always correct.
                if *reuse
                    && *op == BinaryOp::Concat
                    && let Atom::Var { name, .. } = lhs
                {
                    let b = self.atom_reg(rhs)?;
                    let base = match self.resolve(name) {
                        Resolved::Local(reg) => Some(reg),
                        Resolved::Global => {
                            let reg = self.alloc_reg();
                            let global = self.module.intern_global(name);
                            self.code.push(Op::TakeGlobal {
                                dst: reg,
                                global,
                                span: *span,
                            });
                            Some(reg)
                        }
                        _ => None,
                    };
                    if let Some(base) = base {
                        self.code.push(Op::ConcatInPlace {
                            dst,
                            lhs: base,
                            rhs: b,
                            span: *span,
                        });
                        return Ok(());
                    }
                    // Not a reusable base: build the copying concat with the already-resolved rhs.
                    let a = self.atom_reg(lhs)?;
                    self.code.push(Op::Binary {
                        op: *op,
                        dst,
                        a,
                        b,
                        span: *span,
                    });
                    return Ok(());
                }
                let a = self.atom_reg(lhs)?;
                let b = self.atom_reg(rhs)?;
                self.code.push(Op::Binary {
                    op: *op,
                    dst,
                    a,
                    b,
                    span: *span,
                });
                Ok(())
            }
            Rvalue::Call {
                callee,
                args,
                span,
                supplied,
                ..
            } => self.lower_call(callee, args, dst, *span, *supplied),
            Rvalue::Method {
                receiver,
                name,
                args,
                reuse,
                reflect,
                span,
                supplied,
                ..
            } => {
                // A generic enum-variant construction carries its reflected type (R2b.2); intern it so
                // the `MakeEnum` op can stamp it. `None` for an ordinary method call.
                let reflect = reflect.as_ref().map(|r| self.module.intern_type_repr(r));
                self.lower_method(receiver, name, args, *reuse, reflect, dst, *span, *supplied)
            }
            Rvalue::Field {
                receiver,
                name,
                span,
                ..
            } => self.lower_field(receiver, name, dst, *span),
            // A bound method handle (`value.method` as a value, EX.2b): evaluate the receiver and
            // capture it into the handle (`Op::BindMethod` retains it).
            Rvalue::BoundHandle { recv, method, .. } => {
                let recv_reg = self.atom_reg(recv)?;
                let method = self.module.intern_name(method);
                self.code.push(Op::BindMethod {
                    dst,
                    recv: recv_reg,
                    method,
                });
                self.drop_temp_receiver(recv, recv_reg);
                Ok(())
            }
            // An unbound method handle (`Type.method` as a value) is a static constant: load the
            // `(ty, method, associated)` triple. The VM materializes a `Payload::MethodHandle`.
            Rvalue::MethodHandle {
                ty,
                method,
                associated,
                ..
            } => {
                let k = self.add_const(Const::MethodHandle {
                    ty: ty.clone(),
                    method: method.clone(),
                    associated: *associated,
                });
                self.code.push(Op::LoadConst { dst, k });
                Ok(())
            }
            Rvalue::SetField {
                receiver,
                name,
                value,
                reuse,
                span,
                ..
            } => self.lower_set_field(receiver, name, value, *reuse, dst, *span),
            Rvalue::Index {
                receiver,
                index,
                span,
            } => {
                let recv = self.atom_reg(receiver)?;
                let idx = self.atom_reg(index)?;
                self.code.push(Op::Index {
                    dst,
                    recv,
                    index: idx,
                    span: *span,
                });
                self.drop_temp_receiver(receiver, recv);
                Ok(())
            }
            Rvalue::IndexField {
                receiver,
                index,
                field,
                span,
                ..
            } => {
                // Mirrors `Rvalue::Index`: the index atom is read in place (an int, not consumed),
                // and the list receiver's temp — if any — is dropped after the read, exactly as the
                // unfused `Index` would (the intermediate element temp the unfused pair created never
                // exists, so there is nothing else to drop).
                let recv = self.atom_reg(receiver)?;
                let idx = self.atom_reg(index)?;
                let field = self.module.intern_name(field);
                self.code.push(Op::IndexField {
                    dst,
                    recv,
                    index: idx,
                    field,
                    span: *span,
                });
                self.drop_temp_receiver(receiver, recv);
                Ok(())
            }
            Rvalue::List { items, reflect, .. } => {
                // A boxed list: each element is consumed (retained into the list) and the temporary
                // released. A `List<packed>` literal never reaches here — lowering streams it into
                // `PackedListNew` + `PackedListPush` instead (P-PACK 2.5).
                // The checker-resolved element type (R1), interned into the module table so the VM can
                // stamp it onto the built list for `type_of`. `None` → the list stays untagged.
                let reflect = reflect.as_ref().map(|r| self.module.intern_type_repr(r));
                let mut consumed = Vec::new();
                let mut regs = Vec::with_capacity(items.len());
                for item in items {
                    regs.push(self.consume_operand(item, &mut consumed)?);
                }
                self.code.push(Op::MakeList {
                    dst,
                    items: regs.into_boxed_slice(),
                    reflect,
                });
                self.release_consumed(&consumed);
                Ok(())
            }
            Rvalue::PackedListNew { layout, .. } => {
                // Allocate the empty flat buffer that the following `PackedListPush` chain fills
                // (P-PACK 2.5 streaming construction); intern the element schema from the layout the
                // IR carries (so it dedups to the same shape `MakeStruct` uses for the element type).
                let schema = self.module.intern_packed_schema(layout);
                self.code.push(Op::PackedListNew { dst, schema });
                Ok(())
            }
            Rvalue::PackedListPush {
                list, value, span, ..
            } => {
                // Pack one element onto the streaming accumulator. The accumulator (`list`) is an ANF
                // temp — uniquely owned — so the buffer extends in place; the element (`value`) is an
                // owned temp the op consumes (its primitives copied into the buffer), so its now-dead
                // register reference is released right after, freeing the element object at peak of
                // one. A borrowed (non-temp) value is left to its binding's own drop.
                let list_reg = self.atom_reg(list)?;
                let mut consumed = Vec::new();
                let value_reg = self.consume_operand(value, &mut consumed)?;
                self.code.push(Op::PackedListPush {
                    dst,
                    list: list_reg,
                    value: value_reg,
                    span: *span,
                });
                self.release_consumed(&consumed);
                Ok(())
            }
            // A tuple literal builds exactly like a list (object-model slice 4) — each element
            // consumed into the aggregate, which takes one reference to each.
            Rvalue::Tuple { items, .. } => {
                let mut consumed = Vec::new();
                let mut regs = Vec::with_capacity(items.len());
                for item in items {
                    regs.push(self.consume_operand(item, &mut consumed)?);
                }
                self.code.push(Op::MakeTuple {
                    dst,
                    items: regs.into_boxed_slice(),
                });
                self.release_consumed(&consumed);
                Ok(())
            }
            Rvalue::TupleIndex {
                receiver,
                index,
                span,
            } => {
                let recv = self.atom_reg(receiver)?;
                self.code.push(Op::TupleIndex {
                    dst,
                    receiver: recv,
                    index: *index,
                    span: *span,
                });
                self.drop_temp_receiver(receiver, recv);
                Ok(())
            }
            Rvalue::Map {
                entries,
                reflect,
                span,
            } => {
                // Evaluate each key, check it is a string (matching M0's per-entry error timing),
                // then the value — then assemble the map.
                // The checker-resolved `Map(K, V)` type (R1), interned into the module table so the VM
                // can stamp it onto the built map for `type_of`. `None` → the map stays untagged.
                let reflect = reflect.as_ref().map(|r| self.module.intern_type_repr(r));
                let mut consumed = Vec::new();
                let mut pairs = Vec::with_capacity(entries.len());
                for (key, value) in entries {
                    let key_reg = self.consume_operand(key, &mut consumed)?;
                    self.code.push(Op::RequireMapKey {
                        reg: key_reg,
                        span: *span,
                    });
                    let value_reg = self.consume_operand(value, &mut consumed)?;
                    pairs.push((key_reg, value_reg));
                }
                self.code.push(Op::MakeMap {
                    dst,
                    entries: pairs.into_boxed_slice(),
                    reflect,
                });
                self.release_consumed(&consumed);
                Ok(())
            }
            Rvalue::Range {
                start,
                end,
                inclusive,
                span,
            } => {
                let start_reg = self.atom_reg(start)?;
                let end_reg = self.atom_reg(end)?;
                self.code.push(Op::MakeRange {
                    dst,
                    start: start_reg,
                    end: end_reg,
                    inclusive: *inclusive,
                    span: *span,
                });
                Ok(())
            }
            Rvalue::Object {
                type_name,
                type_name_span,
                fields,
                spread,
                reuse,
                reflect,
                span,
            } => {
                // The checker-resolved reflected type (R2), interned so the VM can stamp it onto the
                // built struct for `type_of`. `None` for a non-generic type → the value reflects
                // head-only.
                let reflect = reflect.as_ref().map(|r| self.module.intern_type_repr(r));
                self.lower_object(
                    type_name,
                    *type_name_span,
                    fields,
                    spread,
                    *reuse,
                    reflect,
                    dst,
                    *span,
                )
            }
            Rvalue::Interp { parts, span } => self.lower_interp(parts, dst, *span),
            Rvalue::Closure { func, .. } => {
                // Resolve the closure's captures in this (the building) frame's terms, compile its
                // body with the matching upvalue layout, and emit `MakeClosure` so the VM threads
                // the captured cells into the new closure.
                let (upvalues, captures) = self.resolve_captures(func)?;
                let enclosing = self.child_enclosing();
                // The IR carries the trace name: `None` for a user's anonymous closure, the enclosing
                // function's name for a synthesized async/generator step closure — so an `async fn
                // work` panic traces as `work`, not `<anonymous>`.
                let proto =
                    self.module
                        .add_function(func, upvalues, enclosing, func.name.clone())?;
                let captures = self.emit_captures(captures).into_boxed_slice();
                self.code.push(Op::MakeClosure {
                    dst,
                    proto,
                    captures,
                });
                Ok(())
            }
            Rvalue::Try {
                operand,
                on_error,
                span,
            } => {
                let src = self.atom_reg(operand)?;
                // Resolve the drop pass's error-path locals to registers; on the `Err`/`none` path the
                // VM drops these (firing destructors) before unwinding (Phase 4.2c). Owned frame-locals
                // always resolve to a register; anything else is conservatively skipped.
                let mut on_error: Vec<(Reg, bool)> = on_error
                    .iter()
                    .filter_map(|d| match self.resolve(&d.name) {
                        Resolved::Local(reg) => Some((reg, d.relevant)),
                        _ => None,
                    })
                    .collect();
                // A `?` propagation, like an explicit `return`, unwinds past every enclosing for-loop's
                // post-loop iterator drop — so drop each owned temp iterator destructor-aware on the
                // error path too (innermost first), running a generator's captured local's destructor.
                for iter in self.loops.iter().rev().filter_map(|ctx| ctx.stream_iter) {
                    on_error.push((iter, true));
                }
                self.code.push(Op::TryUnwrap {
                    dst,
                    src,
                    on_error,
                    span: *span,
                });
                Ok(())
            }
            Rvalue::As { operand, ty, .. } => {
                let src = self.atom_reg(operand)?;
                let target = Box::new(narrow_target(ty));
                let some_shape = self.module.builtin_enum_shape("Option", "some");
                let none_shape = self.module.builtin_enum_shape("Option", "none");
                self.code.push(Op::Narrow {
                    dst,
                    src,
                    target,
                    some_shape,
                    none_shape,
                });
                Ok(())
            }
            Rvalue::TypeTest { operand, ty, .. } => {
                let src = self.atom_reg(operand)?;
                let target = Box::new(narrow_target(ty));
                self.code.push(Op::IsType { dst, src, target });
                Ok(())
            }
            Rvalue::MakeGen { step, .. } => {
                // Wrap the lowered step closure into a generator iterator (Track G.1b). The closure was
                // produced by a preceding `Rvalue::Closure`, so `step` is already in a register.
                let src = self.atom_reg(step)?;
                self.code.push(Op::MakeGen { dst, src });
                Ok(())
            }
            Rvalue::MakeFuture { thunk, .. } => {
                // Wrap the lowered lazy thunk closure into a future (Track A.1). The closure was
                // produced by a preceding `Rvalue::Closure`, so `thunk` is already in a register.
                let src = self.atom_reg(thunk)?;
                self.code.push(Op::MakeFuture { dst, src });
                Ok(())
            }
            Rvalue::RunFuture { future, span } => {
                // Drive an awaited future to completion, yielding its value (Track A.2/A.3 top-level).
                let src = self.atom_reg(future)?;
                self.code.push(Op::RunFuture {
                    dst,
                    src,
                    span: *span,
                });
                Ok(())
            }
            Rvalue::PollFuture { future, span } => {
                // Poll a future once — `some(v)`/`none` (Track A.3 state machine).
                let src = self.atom_reg(future)?;
                self.code.push(Op::PollFuture {
                    dst,
                    src,
                    span: *span,
                });
                Ok(())
            }
            Rvalue::Pending { .. } => {
                // The async pending sentinel (Track A.3).
                self.code.push(Op::LoadPending { dst });
                Ok(())
            }
            Rvalue::Spawn { future, span } => {
                // Register the future as a task in the current scope, yielding a handle (Track A.3b).
                let src = self.atom_reg(future)?;
                self.code.push(Op::Spawn {
                    dst,
                    src,
                    span: *span,
                });
                Ok(())
            }
            Rvalue::ScopeBegin { span } => {
                // Open a scope and yield its index (Track A.7): the value form of `Op::ScopeBegin`, used
                // by the async desugar's split `concurrent { }` to thread the index to its join test.
                self.code.push(Op::ScopeBeginValue { dst, span: *span });
                Ok(())
            }
            Rvalue::ScopeReady { scope, span } => {
                // Whether the scope at index `scope` is fully drained (Track A.7) — the boolean the
                // split `concurrent { }`'s join poll-state tests each poll.
                let src = self.atom_reg(scope)?;
                self.code.push(Op::ScopeReady {
                    dst,
                    src,
                    span: *span,
                });
                Ok(())
            }
            Rvalue::ScopeEndAt { scope, span } => {
                // Close the drained scope at index `scope` (Track A.7): the value form of `ScopeEnd`
                // that closes a specific scope by index (a sibling's scope may still be open above it).
                // `dst` is unused (the effect is the close); the desugar discards it.
                let src = self.atom_reg(scope)?;
                self.code.push(Op::ScopeEndAt { src, span: *span });
                Ok(())
            }
            Rvalue::SpawnIsolate { callee, args, span } => {
                // Spawn a call as a fresh isolate (I.4b): carry the callee + unbuilt args so a real
                // isolate can copy-marshal them; the sandbox builds `callee(args)` and registers a
                // cooperative task, identical to `spawn`.
                let callee = self.atom_reg(callee)?;
                let args = self.atom_regs(args)?;
                self.code.push(Op::SpawnIsolate {
                    dst,
                    callee,
                    args,
                    span: *span,
                });
                Ok(())
            }
            Rvalue::MakeChannel { capacity, span } => {
                // Create a bounded channel, yielding a `(Sender, Receiver)` endpoint tuple (I.1).
                let capacity = self.atom_reg(capacity)?;
                self.code.push(Op::MakeChannel {
                    dst,
                    capacity,
                    span: *span,
                });
                Ok(())
            }
            Rvalue::FromBytes {
                blob,
                layout,
                validate,
                span,
            } => {
                // Deserialize a `bytes` buffer into a flat `List<T>` (P-PACK 4.4). Intern element T's
                // schema from the layout the checker recorded (the same channel list literals use). A
                // `None` layout means T was not packable — the checker already emitted E0038, so this
                // program never runs; load unit to keep the register defined.
                let src = self.atom_reg(blob)?;
                match layout {
                    Some(layout) => {
                        let schema = self.module.intern_packed_schema(layout);
                        self.code.push(Op::FromBytes {
                            dst,
                            src,
                            schema,
                            validate: *validate,
                            span: *span,
                        });
                    }
                    None => {
                        let k = self.add_const(Const::Unit);
                        self.code.push(Op::LoadConst { dst, k });
                    }
                }
                Ok(())
            }
            Rvalue::TypeOf { operand, span } => {
                // Evaluate the operand for its side effects in both fidelities. When the checker
                // resolved a concrete static type for this site, bake the precise `Type` constant
                // (fidelity A); otherwise classify the runtime value's head constructor (fidelity B).
                let src = self.atom_reg(operand)?;
                match self.module.type_of_sites.get(span) {
                    Some(repr) => self.code.push(Op::TypeOfStatic {
                        dst,
                        repr: Box::new(repr.clone()),
                    }),
                    None => self.code.push(Op::TypeOf { dst, src }),
                }
                Ok(())
            }
            Rvalue::FieldsOf { operand, .. } => {
                let src = self.atom_reg(operand)?;
                self.code.push(Op::FieldsOf { dst, src });
                Ok(())
            }
            Rvalue::TraitsOf { operand, .. } => {
                let src = self.atom_reg(operand)?;
                self.code.push(Op::TraitsOf { dst, src });
                Ok(())
            }
            Rvalue::AttributesOf { ty, dynamic, .. } => {
                // The attribute type is resolved at compile time (closed-world); the VM reads the
                // matching manifest entries from `Module::reflection` and materializes them. A
                // FORWARDED type parameter (F2b) instead resolves its name at runtime through the
                // hidden slot register and the module's type-argument table.
                let dynamic = match dynamic {
                    Some(slot) => Some(self.atom_reg(slot)?),
                    None => None,
                };
                let type_name = match ty {
                    TypeRef::Named { name, .. } => name.as_str(),
                    _ => "",
                };
                let type_name = self.module.intern_name(type_name);
                self.code.push(Op::AttributesOf {
                    dst,
                    type_name,
                    dynamic,
                });
                Ok(())
            }
            Rvalue::RolesOf { ty, .. } => {
                // Optional turbofish scope (mirrors `AttributesOf`): resolve the role enum name at
                // compile time (closed-world); the VM keeps only bindings of that enum. `None` = all.
                let role_enum = ty.as_ref().and_then(|ty| match ty {
                    TypeRef::Named { name, .. } => Some(self.module.intern_name(name)),
                    _ => None,
                });
                self.code.push(Op::RolesOf { dst, role_enum });
                Ok(())
            }
            Rvalue::ParamsOf { target, .. } => {
                // The target is a runtime string; the VM reads the matching parameter records from
                // `Module::reflection` and materializes them. Load the operand into a register.
                let src = self.atom_reg(target)?;
                self.code.push(Op::ParamsOf { dst, src });
                Ok(())
            }
            Rvalue::FieldSpecsOf { name, .. } => {
                // The name is a runtime string; the VM reads the type's field schema from
                // `Module::reflection` and materializes them. Load the operand into a register.
                let src = self.atom_reg(name)?;
                self.code.push(Op::FieldSpecsOf { dst, src });
                Ok(())
            }
            Rvalue::Construct {
                name, fields, span, ..
            } => {
                let name = self.atom_reg(name)?;
                let fields = self.atom_reg(fields)?;
                // The `Result<dyn, string>` wrapper shapes, interned exactly as `Op::Invoke` interns
                // them — the VM builds `Ok(value)` / `Err(message)` from the same two shapes.
                let ok_shape = self.module.builtin_enum_shape("Result", "Ok");
                let err_shape = self.module.builtin_enum_shape("Result", "Err");
                self.code.push(Op::Construct {
                    dst,
                    name,
                    fields,
                    ok_shape,
                    err_shape,
                    span: *span,
                });
                Ok(())
            }
            Rvalue::Invoke {
                recv,
                name,
                args,
                span,
            } => self.lower_invoke(recv.as_ref(), name, args, dst, *span),
            Rvalue::TypedModuleCall {
                module,
                func,
                args,
                recipe,
                dynamic,
                span,
            } => {
                let args = self.atom_regs(args)?;
                let dynamic = match dynamic {
                    Some(slot) => Some(self.atom_reg(slot)?),
                    None => None,
                };
                let module_id = self.module.intern_name(module);
                let func_id = self.module.intern_name(func);
                self.code.push(Op::TypedModuleCall {
                    dst,
                    module: module_id,
                    func: func_id,
                    args,
                    recipe: recipe.clone().map(Box::new),
                    dynamic,
                    span: *span,
                });
                Ok(())
            }
            Rvalue::TypedMethodCall {
                recv,
                method,
                args,
                recipe,
                dynamic,
                span,
            } => {
                let recv = self.atom_reg(recv)?;
                let args = self.atom_regs(args)?;
                let dynamic = match dynamic {
                    Some(slot) => Some(self.atom_reg(slot)?),
                    None => None,
                };
                let method_id = self.module.intern_name(method);
                self.code.push(Op::TypedMethodCall {
                    dst,
                    recv,
                    method: method_id,
                    args,
                    recipe: recipe.clone().map(Box::new),
                    dynamic,
                    span: *span,
                });
                Ok(())
            }
            Rvalue::DecodeTyped { name, text, span } => {
                // The router-facing runtime decode (L2.2 DI): load the type-name and JSON-text
                // operands into registers and carry the `Result.Ok`/`Result.Err` shapes (as
                // `Op::Invoke`/`TypedModuleCall` do) — the VM wraps a decode / lookup outcome in one.
                let name = self.atom_reg(name)?;
                let text = self.atom_reg(text)?;
                let ok_shape = self.module.builtin_enum_shape("Result", "Ok");
                let err_shape = self.module.builtin_enum_shape("Result", "Err");
                self.code.push(Op::DecodeTyped {
                    dst,
                    name,
                    text,
                    ok_shape,
                    err_shape,
                    span: *span,
                });
                Ok(())
            }
            // A native module-fn reference as a value (expr-tiers arc): load the same
            // `Const::ModuleFn` a `use std.mod.fn` binding produces — a `Call` on it then dispatches
            // to the native function, exactly like a call to an imported module fn.
            Rvalue::ModuleFn { module, func, .. } => {
                let k = self.add_const(Const::ModuleFn {
                    module: module.clone(),
                    func: func.clone(),
                });
                self.code.push(Op::LoadConst { dst, k });
                Ok(())
            }
            // A native module value resolved from a namespace group (`http.client`): load the same
            // `Const::NativeModule` a direct `use std.http.client` binding produces, carrying the
            // concrete leaf identity — so a method call dispatches identically and AOT ring DCE sees
            // `std.http.client` in the const pool.
            Rvalue::NativeModule { module, .. } => {
                let k = self.add_const(Const::NativeModule(module.clone()));
                self.code.push(Op::LoadConst { dst, k });
                Ok(())
            }
            // A trait method call (native default body, slice 2; or a kernel-trait method since the
            // ExtBundle→ExtTrait fold-in, slice 4): route baked by the checker; the receiver and args
            // are borrowed registers, the ctx-method convention.
            Rvalue::TraitMethod {
                receiver,
                trait_name,
                name,
                args,
                span,
            } => {
                let recv = self.atom_reg(receiver)?;
                let args = self.atom_regs(args)?;
                let trait_id = self.module.intern_name(trait_name);
                let method_id = self.module.intern_name(name);
                self.code.push(Op::TraitMethod {
                    dst,
                    recv,
                    trait_name: trait_id,
                    method: method_id,
                    args,
                    span: *span,
                });
                Ok(())
            }
        }
    }

    /// Lower a string interpolation: start from an empty string and fold each part's display form
    /// in with `~` concatenation, mirroring the tree-walker (literal text verbatim, a hole via its
    /// `display`). The interpolation's own span is used for each `Stringify`, matching M0.
    fn lower_interp(
        &mut self,
        parts: &[InterpPart],
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        // Build the interpolation in one pass (P-VMT-STR): each literal is a constant-pool string
        // copied verbatim, each hole is `Stringify`-ed into its own register (so a `Display` object
        // still dispatches to `to_string` via a call frame) and then rendered by the single
        // `BuildString`. This replaces the old `LoadConst "" + N×(Stringify + Concat)` left-fold,
        // which allocated an intermediate `String` for every part.
        let mut segments: Vec<StrPart> = Vec::with_capacity(parts.len());
        for part in parts {
            match part {
                InterpPart::Literal(text) => {
                    let k = self.add_const(Const::Str(text.clone()));
                    segments.push(StrPart::Literal(k));
                }
                InterpPart::Hole { atom, .. } => {
                    let src = self.atom_reg(atom)?;
                    let r = self.alloc_reg();
                    // Route a `Display` object through its `to_string` before `BuildString` renders
                    // it via `display`; identity for every other value.
                    self.code.push(Op::Stringify { dst: r, src, span });
                    segments.push(StrPart::Hole(r));
                }
            }
        }
        self.code.push(Op::BuildString {
            dst,
            parts: segments.into_boxed_slice(),
        });
        Ok(())
    }

    /// Lower an ordinary call `callee(args)` (a method call is [`Rvalue::Method`], lowered
    /// separately). A prelude function called directly by name routes to its dedicated op.
    fn lower_call(
        &mut self,
        callee: &Atom,
        args: &[Atom],
        dst: Reg,
        span: Span,
        supplied: Option<u64>,
    ) -> Result<(), Unsupported> {
        // A prelude function called directly by name. A user binding of the same name shadows the
        // prelude (then `resolve` is not `Prelude`, so this falls to the ordinary call below).
        if let Atom::Var { name, .. } = callee
            && matches!(self.resolve(name), Resolved::Prelude)
        {
            // Prelude/builtin callees have no defaulted parameters, so the checker never binds a
            // hole against one. Refuse rather than drop the mask: silently lowering it would call
            // the builtin with the args shifted into the wrong positions.
            if supplied.is_some() {
                return unsupported("named arguments that skip a parameter of a prelude function");
            }
            // `Ok(x)`/`Ok()`, `Err(e)`, `some(x)`, `panic(msg)` — an arity-correct direct call
            // keeps its dedicated fast op (`MakeEnum` / `Op::Panic`, which tier-1 also compiles).
            // A wrong-arity call falls through to the generic `CallBuiltin` below, whose RUNTIME
            // arity error is byte-identical to a call through the first-class value (poly-values
            // F3) — so the direct and indirect paths cannot diverge, and neither aborts compile.
            match name.as_str() {
                "Ok" if args.len() <= 1 => {
                    return self.make_result_option("Result", "Ok", args, dst);
                }
                "Err" if args.len() == 1 => {
                    return self.make_result_option("Result", "Err", args, dst);
                }
                "some" if args.len() == 1 => {
                    return self.make_result_option("Option", "some", args, dst);
                }
                "panic" if args.len() == 1 => return self.make_panic(args, span),
                _ => {}
            }
            if let Some(builtin) = Builtin::from_name(name) {
                // `len`/`map`/`filter`/`sum`/`assert` — the collection/assertion builtins — plus
                // the wrong-arity constructor calls from above.
                let args = self.atom_regs(args)?;
                self.code.push(Op::CallBuiltin {
                    dst,
                    builtin,
                    args,
                    span,
                });
                return Ok(());
            }
            return unsupported("prelude function not in the VM subset");
        }
        // A statically-known top-level `fn` (immutable, zero-upvalue global) — call it directly
        // through its slot, skipping the `LoadGlobal` + per-call retain/release of the callee
        // closure (perf A). Guarded on the name still resolving to the global: a local binding of
        // the same name shadows it, in which case the ordinary indirect call below applies.
        if let Atom::Var { name, .. } = callee
            && self.module.module_fns.contains(name)
            && matches!(self.resolve(name), Resolved::Global)
        {
            let global = self.module.intern_global(name);
            let args = self.atom_regs(args)?;
            self.code.push(Op::CallGlobal {
                dst,
                global,
                args,
                span,
                supplied,
            });
            return Ok(());
        }
        let callee_reg = self.atom_reg(callee)?;
        let args = self.atom_regs(args)?;
        self.code.push(Op::Call {
            dst,
            callee: callee_reg,
            args,
            span,
            supplied,
        });
        Ok(())
    }

    /// Lower a method/associated call `receiver.name(args)`. A bare type-name receiver resolves at
    /// compile time to an enum-variant construction or an associated-function call; any other
    /// receiver is a runtime-dispatched instance method.
    #[allow(clippy::too_many_arguments)]
    fn lower_method(
        &mut self,
        receiver: &Atom,
        name: &str,
        args: &[Atom],
        reuse: bool,
        reflect: Option<u32>,
        dst: Reg,
        span: Span,
        supplied: Option<u64>,
    ) -> Result<(), Unsupported> {
        // `Type.something(args)` where `Type` is a known type name. Keyed purely on the type
        // registry (a same-named local does not shadow a type member), matching the tree-walker.
        if let Atom::Var {
            name: type_name, ..
        } = receiver
        {
            if let Some(TypeInfo::Enum { fns, .. }) = self.module.types.get(type_name) {
                // `Enum.try_from(s)` / `Enum.from(s)` — string→case conversion (intercepted before
                // variant construction, mirroring the checker and the tree-walker).
                if name == "try_from" || name == "from" {
                    return self.lower_enum_from_str(type_name, name == "from", args, dst, span);
                }
                // An associated function `Enum.f(...)` (the unified body, object-model slice 3):
                // resolved at compile time when the name is a method, not a variant. Variant
                // construction still wins for a variant name (uppercase by convention, so no clash).
                if let Some(&proto) = fns.get(name) {
                    return self.call_associated(proto, args, dst, span, supplied);
                }
                return self.make_enum(type_name, name, args, reflect, dst);
            }
            // An associated function `Type.f(...)` resolves at compile time for both kinds — struct
            // and class share the dispatch table (the unified body).
            if let Some(TypeInfo::Class { fns, .. } | TypeInfo::Struct { fns, .. }) =
                self.module.types.get(type_name)
                && let Some(&proto) = fns.get(name)
            {
                return self.call_associated(proto, args, dst, span, supplied);
            }
        }
        // Otherwise the receiver is a value: a runtime-dispatched method call (a user instance
        // method, or a `count`/`enumerate` built-in — the VM decides).
        //
        // In-place collection self-update (Phase 5.1c / S1): forward the IR reuse token to the op when
        // the receiver's sole reference can be handed to the in-place path — a directly-held **local**
        // (its register *is* the binding) or a top-level **global** (moved out with `TakeGlobal` so the
        // in-place op sees refcount 1, the same shape `lower_set_field` uses for a global field-set and
        // the global struct/list accumulator reuse uses). A celled/captured base — or an unmarked op —
        // falls through to the copying path (`reuse: false`), always correct value semantics. The VM's
        // reuse branch additionally checks the runtime receiver kind, so a same-named user method only
        // ever costs the flag. The trailing `acc = %t` reassignment re-stores the mutated collection
        // (`StoreGlobal` for a global), so the vacated slot is never observed between the two.
        //
        // Args are resolved *before* a `TakeGlobal` so moving the receiver global out cannot vacate a
        // slot an arg still reads (the reuse pass also guarantees no arg mentions the receiver var).
        // Consume the key of a map `set`/`remove` when it is a single-use temporary (`Atom::Temp` — a
        // freshly-built value, e.g. an interpolation, never a source variable that could be read
        // again), so the VM can move its buffer into the map instead of cloning it (see
        // `Op::CallMethod::consume_key`). The VM re-checks the receiver is a map and the key is sole-owned.
        let consume_key =
            matches!(name, "set" | "remove") && matches!(args.first(), Some(Atom::Temp(_)));
        let arg_regs = self.atom_regs(args)?;
        let (recv, recv_reuse) = match (reuse, receiver) {
            (true, Atom::Var { name, .. }) => match self.resolve(name) {
                Resolved::Local(reg) => (reg, true),
                Resolved::Global => {
                    let reg = self.alloc_reg();
                    let global = self.module.intern_global(name);
                    self.code.push(Op::TakeGlobal {
                        dst: reg,
                        global,
                        span,
                    });
                    (reg, true)
                }
                _ => (self.atom_reg(receiver)?, false),
            },
            _ => (self.atom_reg(receiver)?, false),
        };
        let cache = self.module.next_cache_slot();
        let method = self.module.intern_name(name);
        self.code.push(Op::CallMethod {
            dst,
            recv,
            method,
            args: arg_regs,
            span,
            cache,
            reuse: recv_reuse,
            consume_key,
            // Into the callee's register space: the receiver lands in register 0 and is always
            // supplied, so every declared parameter's bit moves up by one.
            supplied: supplied.map(|m| (m << 1) | 1),
        });
        // A reuse-marked call consumes the receiver itself (the VM clears it on the in-place path); the
        // receiver is always a `Var` (never an owned `Temp`), so `drop_temp_receiver` is a no-op there.
        // Only the copying path can carry an owned temp receiver that still needs its drop.
        if !recv_reuse {
            self.drop_temp_receiver(receiver, recv);
        }
        Ok(())
    }

    /// Lower a field assignment `receiver.name = value` (`x.f = v`, Phase 5.2). Forward the IR reuse
    /// token to the op only when the receiver is a storage kind whose sole reference we can hand to
    /// the in-place path: a directly-held **local** (its register *is* the binding) or a top-level
    /// **global** (moved out with `TakeGlobal` so the in-place op sees refcount 1, the same shape as
    /// the global struct/list accumulator reuse). A celled/captured base — or an unmarked op — falls
    /// through to the copying path (`reuse: false`), always correct value semantics. The value is
    /// resolved *before* a `TakeGlobal` so moving the global out cannot vacate a slot it still reads.
    fn lower_set_field(
        &mut self,
        receiver: &Atom,
        field: &str,
        value: &Atom,
        reuse: bool,
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        let val = self.atom_reg(value)?;
        let reuse_base = if reuse {
            if let Atom::Var { name, .. } = receiver {
                match self.resolve(name) {
                    Resolved::Local(reg) => Some(reg),
                    Resolved::Global => {
                        let reg = self.alloc_reg();
                        let global = self.module.intern_global(name);
                        self.code.push(Op::TakeGlobal {
                            dst: reg,
                            global,
                            span,
                        });
                        Some(reg)
                    }
                    _ => None,
                }
            } else {
                None
            }
        } else {
            None
        };
        let field_id = self.module.intern_name(field);
        match reuse_base {
            Some(obj) => {
                self.code.push(Op::SetField {
                    dst,
                    obj,
                    field: field_id,
                    value: val,
                    reuse: true,
                    span,
                });
                Ok(())
            }
            None => {
                let obj = self.atom_reg(receiver)?;
                self.code.push(Op::SetField {
                    dst,
                    obj,
                    field: field_id,
                    value: val,
                    reuse: false,
                    span,
                });
                self.drop_temp_receiver(receiver, obj);
                Ok(())
            }
        }
    }

    /// Lower bare member access `receiver.name`: a no-data enum variant (`Status.Pending`) or a
    /// field load.
    fn lower_field(
        &mut self,
        receiver: &Atom,
        name: &str,
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        if let Atom::Var {
            name: type_name, ..
        } = receiver
        {
            enum Member {
                EmptyVariant(u32),
                Unsupported(&'static str),
                FieldAccess,
            }
            let kind = match self.module.types.get(type_name) {
                Some(TypeInfo::Enum { variants, .. }) => match variants.get(name) {
                    Some(v) if v.fields.is_empty() => Member::EmptyVariant(v.index),
                    Some(_) => Member::Unsupported("data-carrying variant used without arguments"),
                    None => Member::Unsupported("unknown enum variant"),
                },
                Some(_) => Member::Unsupported("type member used as a value"),
                None => Member::FieldAccess,
            };
            match kind {
                Member::EmptyVariant(index) => {
                    let shape = self.module.intern_shape(
                        Shape::enum_variant(type_name.clone(), name.to_string(), Vec::new(), false)
                            .with_variant_index(index),
                    );
                    self.code.push(Op::MakeEnum {
                        dst,
                        shape,
                        args: Box::new([]),
                        // A nullary variant infers `Enum<dyn>` — reflected head-only (R2b.2 tags only
                        // payload variants, which pin the type arguments).
                        reflect: None,
                    });
                    return Ok(());
                }
                Member::Unsupported(reason) => return unsupported(reason),
                Member::FieldAccess => {}
            }
        }
        let obj = self.atom_reg(receiver)?;
        let cache = self.module.next_cache_slot();
        let field = self.module.intern_name(name);
        self.code.push(Op::LoadField {
            dst,
            obj,
            field,
            span,
            cache,
        });
        self.drop_temp_receiver(receiver, obj);
        Ok(())
    }

    /// `Type { field: value, ...spread }` — construct a struct/class/opaque instance, or raise the
    /// tree-walker's runtime error for an unknown type.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn lower_object(
        &mut self,
        type_name: &str,
        type_name_span: Span,
        fields: &[noeta_ir::ObjectFieldInit],
        spread: &Option<(Atom, Span)>,
        reuse: bool,
        reflect: Option<u32>,
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        match self.module.types.get(type_name) {
            Some(TypeInfo::Struct { fields: decl, .. }) => {
                let decl = decl.clone();
                self.make_record(
                    type_name,
                    ShapeKind::Struct,
                    &decl,
                    fields,
                    spread,
                    reuse,
                    reflect,
                    dst,
                    span,
                )
            }
            Some(TypeInfo::Class { fields: decl, .. }) => {
                let decl = decl.clone();
                self.make_record(
                    type_name,
                    ShapeKind::Class,
                    &decl,
                    fields,
                    spread,
                    reuse,
                    reflect,
                    dst,
                    span,
                )
            }
            // Opaque/imported types have no fixed shape, so the in-place struct-reuse op does not
            // apply; the reuse pass never marks them (it excludes nothing here, but `make_opaque`
            // simply ignores the token — the copying path is always correct).
            Some(TypeInfo::Opaque) => self.make_opaque(type_name, fields, spread, dst),
            Some(TypeInfo::Enum { .. }) => unsupported("enum type used as a record literal"),
            None => {
                // The tree-walker looks the type up first and errors before touching fields.
                let idx = self.add_diag(unknown_type_diag(type_name, type_name_span));
                self.code.push(Op::Raise { idx });
                Ok(())
            }
        }
    }

    /// Construct a declared struct/class instance. The full-initialization guarantee (E0009) is
    /// enforced at runtime by `MakeStruct`; an unknown field is a compile-time-detected runtime
    /// raise (its value atom was already computed by the preceding `let`s).
    #[allow(clippy::too_many_arguments)]
    fn make_record(
        &mut self,
        type_name: &str,
        kind: ShapeKind,
        decl_fields: &[String],
        inits: &[noeta_ir::ObjectFieldInit],
        spread: &Option<(Atom, Span)>,
        reuse: bool,
        reflect: Option<u32>,
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        let structural_eq = self.module.structural_eq_types.contains(type_name);
        let key_capable = self.module.key_capable_types.contains(type_name);
        let shape = self.module.intern_shape(
            Shape::object_equatable(
                kind,
                type_name.to_string(),
                decl_fields.to_vec(),
                structural_eq,
            )
            .with_key_capable(key_capable),
        );
        // In-place reuse (Phase 5): a self-update `acc = Type { ...acc, f: v }` whose spread base is a
        // directly-held **local** (Phase 5.1a) or a top-level **global** (Phase 5.1b) reuses that
        // allocation. For a local the register *is* the binding's sole storage, read in place; for a
        // global, `TakeGlobal` moves the value out of its slot so the in-place op sees unique ownership
        // (the trailing reassignment stores the result back). `MakeStructInPlace` then moves the base
        // out of its register — the runtime sees refcount 1 on the unique path and an alias forces a
        // copy. A captured cell or upvalue base is not handled — it falls through to the copying
        // `MakeStruct`, which is always correct.
        //
        // The field operands are consumed *before* `TakeGlobal` runs, so a field that itself reads the
        // global (`g = T { ...g, x: g }`) loads the live value before the slot is vacated.
        if reuse
            && let Some((Atom::Var { name, .. }, _)) = spread
            && matches!(self.resolve(name), Resolved::Local(_) | Resolved::Global)
        {
            let mut consumed = Vec::new();
            let mut named: Vec<(u16, Reg)> = Vec::with_capacity(inits.len());
            for init in inits {
                let Some(slot) = decl_fields.iter().position(|f| f == &init.name) else {
                    let idx =
                        self.add_diag(unknown_field_diag(type_name, &init.name, init.name_span));
                    self.code.push(Op::Raise { idx });
                    return Ok(());
                };
                let r = self.consume_operand(&init.value, &mut consumed)?;
                named.push((slot as u16, r));
            }
            let base = match self.resolve(name) {
                Resolved::Local(reg) => reg,
                // A global: take its single owning reference out into a fresh register. (Resolved
                // exactly once more here; the `matches!` guard above already proved it is one of the
                // two handled cases.)
                _ => {
                    let reg = self.alloc_reg();
                    let global = self.module.intern_global(name);
                    self.code.push(Op::TakeGlobal {
                        dst: reg,
                        global,
                        span,
                    });
                    reg
                }
            };
            self.code.push(Op::MakeStructInPlace {
                dst,
                shape,
                named: named.into_boxed_slice(),
                base,
                check: ReuseCheck::Runtime,
                reflect,
                span,
            });
            self.release_consumed(&consumed);
            return Ok(());
        }
        let mut consumed = Vec::new();
        let spread_reg = match spread {
            Some((a, _)) => Some(self.consume_operand(a, &mut consumed)?),
            None => None,
        };
        let mut named: Vec<(u16, Reg)> = Vec::with_capacity(inits.len());
        for init in inits {
            let Some(slot) = decl_fields.iter().position(|f| f == &init.name) else {
                let idx = self.add_diag(unknown_field_diag(type_name, &init.name, init.name_span));
                self.code.push(Op::Raise { idx });
                return Ok(());
            };
            let r = self.consume_operand(&init.value, &mut consumed)?;
            named.push((slot as u16, r));
        }
        self.code.push(Op::MakeStruct {
            dst,
            shape,
            named: named.into_boxed_slice(),
            spread: spread_reg,
            reflect,
            span,
        });
        self.release_consumed(&consumed);
        Ok(())
    }

    /// Construct an opaque (`use`-imported) instance: any fields are accepted, the runtime builds a
    /// sorted-key shape, and there are no field checks.
    fn make_opaque(
        &mut self,
        type_name: &str,
        inits: &[noeta_ir::ObjectFieldInit],
        spread: &Option<(Atom, Span)>,
        dst: Reg,
    ) -> Result<(), Unsupported> {
        let mut consumed = Vec::new();
        let spread_reg = match spread {
            Some((a, _)) => Some(self.consume_operand(a, &mut consumed)?),
            None => None,
        };
        let mut keys: Vec<(NameId, Reg)> = Vec::with_capacity(inits.len());
        for init in inits {
            let r = self.consume_operand(&init.value, &mut consumed)?;
            let key = self.module.intern_name(&init.name);
            keys.push((key, r));
        }
        let type_name = self.module.intern_name(type_name);
        self.code.push(Op::MakeOpaque {
            dst,
            type_name,
            keys: keys.into_boxed_slice(),
            spread: spread_reg,
        });
        self.release_consumed(&consumed);
        Ok(())
    }

    /// Construct an enum variant carrying data (`OrderError.NegativePrice(i)`).
    fn make_enum(
        &mut self,
        type_name: &str,
        variant: &str,
        args: &[Atom],
        reflect: Option<u32>,
        dst: Reg,
    ) -> Result<(), Unsupported> {
        let slots = match self.module.types.get(type_name) {
            Some(TypeInfo::Enum { variants, .. }) => match variants.get(variant) {
                Some(slots) => slots.clone(),
                None => return unsupported("unknown enum variant"),
            },
            _ => unreachable!("make_enum is only reached for enum types"),
        };
        let shape = self.module.intern_shape(
            Shape::enum_variant(
                type_name.to_string(),
                variant.to_string(),
                slots.fields,
                false,
            )
            .with_variant_index(slots.index),
        );
        let mut consumed = Vec::new();
        let mut arg_regs = Vec::with_capacity(args.len());
        for a in args {
            arg_regs.push(self.consume_operand(a, &mut consumed)?);
        }
        self.code.push(Op::MakeEnum {
            dst,
            shape,
            args: arg_regs.into_boxed_slice(),
            reflect,
        });
        self.release_consumed(&consumed);
        Ok(())
    }

    /// `Enum.try_from(s)` / `Enum.from(s)` (`panic` = the `from` form) — lower the string→case
    /// conversion. Interns the shape of every **payload-free** variant (the name-constructible ones)
    /// plus the `Option` wrappers, and emits `Op::EnumFromStr`; the VM matches the runtime string
    /// against the case names. The checker guarantees a single string argument.
    fn lower_enum_from_str(
        &mut self,
        type_name: &str,
        panic: bool,
        args: &[Atom],
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        let variants = match self.module.types.get(type_name) {
            Some(TypeInfo::Enum { variants, .. }) => variants.clone(),
            _ => unreachable!("lower_enum_from_str is only reached for enum types"),
        };
        let mut cases: Vec<(String, u32)> = variants
            .into_iter()
            .filter(|(_, slots)| slots.fields.is_empty())
            .map(|(vname, slots)| {
                let shape = self.module.intern_shape(
                    Shape::enum_variant(type_name.to_string(), vname.clone(), Vec::new(), false)
                        .with_variant_index(slots.index),
                );
                (vname, shape)
            })
            .collect();
        // Stable order keeps the case list deterministic (the source `HashMap` is not ordered).
        // Sort by name *before* interning, since ids follow emission order, not lexical order.
        cases.sort_by(|a, b| a.0.cmp(&b.0));
        let cases: Vec<(NameId, u32)> = cases
            .into_iter()
            .map(|(vname, shape)| (self.module.intern_name(&vname), shape))
            .collect();
        let some_shape = self.module.builtin_enum_shape("Option", "some");
        let none_shape = self.module.builtin_enum_shape("Option", "none");
        let mut consumed = Vec::new();
        let arg = self.consume_operand(&args[0], &mut consumed)?;
        let enum_name = self.module.intern_name(type_name);
        self.code.push(Op::EnumFromStr {
            dst,
            arg,
            enum_name,
            cases: cases.into_boxed_slice(),
            some_shape,
            none_shape,
            panic,
            span,
        });
        self.release_consumed(&consumed);
        Ok(())
    }

    /// Call an associated function `Type.f(args)`. The method prototype reserves register 0 for
    /// `self`; an associated call has no receiver, so unit is passed there.
    fn call_associated(
        &mut self,
        proto: u32,
        args: &[Atom],
        dst: Reg,
        span: Span,
        supplied: Option<u64>,
    ) -> Result<(), Unsupported> {
        let self_reg = self.alloc_reg();
        let k = self.add_const(Const::Unit);
        self.code.push(Op::LoadConst { dst: self_reg, k });
        let mut arg_regs: Vec<Reg> = Vec::with_capacity(args.len() + 1);
        arg_regs.push(self_reg);
        for a in args {
            arg_regs.push(self.atom_reg(a)?);
        }
        let callee = self.alloc_reg();
        self.code.push(Op::MakeClosure {
            dst: callee,
            proto,
            captures: Box::new([]),
        });
        self.code.push(Op::Call {
            dst,
            callee,
            args: arg_regs.into_boxed_slice(),
            span,
            // A unit receiver occupies register 0 above, so the declared parameters shift by one.
            supplied: supplied.map(|m| (m << 1) | 1),
        });
        Ok(())
    }

    /// Construct a built-in `Result`/`Option` value (`Ok`/`Err`/`some`). `Ok` accepts 0 or 1
    /// arguments (the void success `Ok()` and the wrapping `Ok(x)`); `Err`/`some` take 1.
    fn make_result_option(
        &mut self,
        enum_name: &str,
        variant: &str,
        args: &[Atom],
        dst: Reg,
    ) -> Result<(), Unsupported> {
        let arg_regs = self.atom_regs(args)?;
        let allowed = if variant == "Ok" { 0..=1 } else { 1..=1 };
        if !allowed.contains(&arg_regs.len()) {
            return unsupported("Result/Option constructor with an unexpected argument count");
        }
        let shape = self.module.builtin_enum_shape(enum_name, variant);
        self.code.push(Op::MakeEnum {
            dst,
            shape,
            args: arg_regs,
            reflect: None,
        });
        Ok(())
    }

    /// `panic(msg)` — evaluate the message and emit the abort op (E0010). The `dst` is unused (the
    /// op aborts the program), matching the tree-walker.
    fn make_panic(&mut self, args: &[Atom], span: Span) -> Result<(), Unsupported> {
        let arg_regs = self.atom_regs(args)?;
        if arg_regs.len() != 1 {
            return unsupported("`panic` with an unexpected argument count");
        }
        self.code.push(Op::Panic {
            msg: arg_regs[0],
            span,
        });
        Ok(())
    }

    /// `invoke(recv, name, args)` / `invoke(name, args)` — fallible by-name dispatch. A bare
    /// type-name receiver becomes a first-class type handle; any other receiver compiles normally;
    /// the free-fn form emits no receiver register at all. All flow through the runtime-dispatched
    /// `Op::Invoke`.
    fn lower_invoke(
        &mut self,
        recv: Option<&Atom>,
        name: &Atom,
        args: &Atom,
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        let recv_reg = match recv {
            Some(Atom::Var {
                name: type_name, ..
            }) if self.module.types.contains_key(type_name) => {
                let r = self.alloc_reg();
                let name = self.module.intern_name(type_name);
                self.code.push(Op::TypeValue { dst: r, name });
                Some(r)
            }
            Some(recv) => Some(self.atom_reg(recv)?),
            None => None,
        };
        let name_reg = self.atom_reg(name)?;
        let args_reg = self.atom_reg(args)?;
        let ok_shape = self.module.builtin_enum_shape("Result", "Ok");
        let err_shape = self.module.builtin_enum_shape("Result", "Err");
        self.code.push(Op::Invoke {
            dst,
            recv: recv_reg,
            name: name_reg,
            args: args_reg,
            ok_shape,
            err_shape,
            span,
        });
        Ok(())
    }

    /// `value ?? fallback` — unwrap the success payload, or evaluate `fallback` on the empty case
    /// (mirroring the tree-walker's `eval_coalesce`). `value` is a pre-computed atom; `fallback`
    /// is a lazily-evaluated value block.
    fn coalesce(
        &mut self,
        dst: Option<Temp>,
        value: &Atom,
        fallback: &Block,
        span: Span,
    ) -> Result<(), Unsupported> {
        let out = self.dst_reg(dst);
        let src = self.atom_reg(value)?;
        let coalesce_pos = self.code.len();
        self.code.push(Op::Coalesce {
            dst: out,
            src,
            fallback: 0,
            span,
        });
        // Success path: `out` is set, then jump past the fallback.
        let jump_end = self.code.len();
        self.code.push(Op::Jump { target: 0 });
        let fallback_start = self.code.len() as u32;
        self.patch_jump(coalesce_pos, fallback_start);
        let fb = self.value_block(fallback)?;
        self.code.push(Op::Move { dst: out, src: fb });
        let end = self.code.len() as u32;
        self.patch_jump(jump_end, end);
        Ok(())
    }

    /// `match scrutinee { pattern => body, ... }` — a linear decision chain: each arm tests its
    /// pattern (jumping to the next arm on mismatch), binds, evaluates its body, and jumps to the
    /// end. A value matching no arm hits `MatchFail` (E0007). In expression position (`dst`
    /// `Some`) each arm body is a value block whose tail is moved into the destination.
    ///
    /// A guarded arm (`pattern if cond`) evaluates its guard block after the pattern test and
    /// bindings, still inside the arm scope (the guard sees the bindings), then branches with the
    /// same fused `CondBranch` an `if`/`while` condition compiles to: false jumps to the next
    /// arm's test (fall-through, exactly like a failed pattern), a non-bool raises the
    /// `if`-condition E0007 — matching the reference interpreter byte for byte.
    fn match_stmt(
        &mut self,
        scrutinee: &Atom,
        arms: &[noeta_ir::Arm],
        dst: Option<Temp>,
        span: Span,
    ) -> Result<(), Unsupported> {
        let s = self.atom_reg(scrutinee)?;
        let out = dst.map(|t| self.define_temp(t));
        let mut end_jumps: Vec<usize> = Vec::new();
        for arm in arms {
            let mut fail_jumps: Vec<usize> = Vec::new();
            self.scopes.push(HashMap::new());
            self.emit_pattern(&arm.pattern, s, &mut fail_jumps);
            let body = (|| {
                if let Some(guard) = &arm.guard {
                    let g = self.value_block(&guard.block)?;
                    fail_jumps.push(self.push_cond_branch(g, guard.span));
                }
                self.hoist_nested_fn_cells(&arm.body.stmts);
                for stmt in &arm.body.stmts {
                    self.stmt(stmt)?;
                }
                if let (Some(out), Some(tail)) = (out, &arm.body.tail) {
                    let r = self.atom_reg(tail)?;
                    self.code.push(Op::Move { dst: out, src: r });
                }
                Ok(())
            })();
            self.scopes.pop();
            body?;
            end_jumps.push(self.code.len());
            self.code.push(Op::Jump { target: 0 });
            // A mismatch in this arm jumps to the next arm (or the `MatchFail` below).
            let next = self.code.len() as u32;
            for pos in fail_jumps {
                self.patch_jump(pos, next);
            }
        }
        self.code.push(Op::MatchFail { src: s, span });
        let end = self.code.len() as u32;
        for pos in end_jumps {
            self.patch_jump(pos, end);
        }
        Ok(())
    }

    /// Emit the test for one pattern against value register `reg`, recording into `fail_jumps` the
    /// positions of every conditional that must jump to the arm's failure target. Bindings alias
    /// the matched register into the current (arm) scope. Mirrors `match_pattern`.
    fn emit_pattern(&mut self, pattern: &Pattern, reg: Reg, fail_jumps: &mut Vec<usize>) {
        match pattern {
            Pattern::Wildcard { .. } => {}
            Pattern::Binding { name, span } => self.bind_loop_var(name, reg, *span),
            Pattern::Int { value, .. } => {
                fail_jumps.push(self.code.len());
                self.code.push(Op::MatchInt {
                    src: reg,
                    value: *value,
                    fail: 0,
                });
            }
            Pattern::Str { value, .. } => {
                fail_jumps.push(self.code.len());
                let value = self.module.intern_name(value);
                self.code.push(Op::MatchStr {
                    src: reg,
                    value,
                    fail: 0,
                });
            }
            Pattern::Bool { value, .. } => {
                fail_jumps.push(self.code.len());
                self.code.push(Op::MatchBool {
                    src: reg,
                    value: *value,
                    fail: 0,
                });
            }
            Pattern::Variant {
                type_name,
                variant,
                bindings,
                ..
            } => {
                fail_jumps.push(self.code.len());
                let type_name = type_name.as_ref().map(|n| self.module.intern_name(n));
                let variant = self.module.intern_name(variant);
                self.code.push(Op::MatchVariant {
                    src: reg,
                    type_name,
                    variant,
                    arity: bindings.len() as u16,
                    fail: 0,
                });
                for (i, sub) in bindings.iter().enumerate() {
                    let dr = self.alloc_reg();
                    self.code.push(Op::ExtractField {
                        dst: dr,
                        src: reg,
                        index: i as u16,
                    });
                    self.emit_pattern(sub, dr, fail_jumps);
                }
            }
            // `is T` — test the head constructor with the shared matcher into a temp bool, then
            // fall through on `true` / jump to the next arm on `false`. Binds nothing.
            Pattern::IsType { ty, .. } => {
                let test = self.alloc_reg();
                self.code.push(Op::IsType {
                    dst: test,
                    src: reg,
                    target: Box::new(narrow_target(ty)),
                });
                fail_jumps.push(self.code.len());
                self.code.push(Op::JumpIfFalse {
                    reg: test,
                    target: 0,
                });
            }
            // A tuple pattern `(p, q, …)` (object-model slice 4b.2): test the value is a tuple of
            // the right arity (fail-jump otherwise), then project each element with `TupleIndex` and
            // recurse — the structural mirror of the variant case's `MatchVariant` + `ExtractField`.
            Pattern::Tuple { elements, span } => {
                fail_jumps.push(self.code.len());
                self.code.push(Op::MatchTuple {
                    src: reg,
                    arity: elements.len() as u16,
                    fail: 0,
                });
                for (i, sub) in elements.iter().enumerate() {
                    let dr = self.alloc_reg();
                    self.code.push(Op::TupleIndex {
                        dst: dr,
                        receiver: reg,
                        index: i as u32,
                        span: *span,
                    });
                    self.emit_pattern(sub, dr, fail_jumps);
                }
            }
        }
    }

    /// Lower `a && b` / `a || b` to branches, matching the tree-walker's `eval_logical`: the left
    /// operand (a pre-computed atom) must be a bool; on short-circuit its value is the result;
    /// otherwise the right operand (a lazily-evaluated value block) must be a bool and is the
    /// result.
    fn logical(
        &mut self,
        dst: Option<Temp>,
        op: BinaryOp,
        left: &Atom,
        right: &Block,
        span: Span,
    ) -> Result<(), Unsupported> {
        let out = self.dst_reg(dst);
        let left_reg = self.atom_reg(left)?;
        self.code.push(Op::Move {
            dst: out,
            src: left_reg,
        });
        self.code.push(Op::RequireBool {
            reg: out,
            side: BoolSide::Left,
            op,
            span,
        });
        let jump_pos = self.code.len();
        // Short-circuit: `&&` stops when the left is false, `||` when it is true.
        self.code.push(match op {
            BinaryOp::And => Op::JumpIfFalse {
                reg: out,
                target: 0,
            },
            BinaryOp::Or => Op::JumpIfTrue {
                reg: out,
                target: 0,
            },
            _ => unreachable!("logical only handles && and ||"),
        });
        let right_reg = self.value_block(right)?;
        self.code.push(Op::Move {
            dst: out,
            src: right_reg,
        });
        self.code.push(Op::RequireBool {
            reg: out,
            side: BoolSide::Right,
            op,
            span,
        });
        let end = self.code.len() as u32;
        self.patch_jump(jump_pos, end);
        Ok(())
    }
}

/// Whether a Core-IR constant materializes to an **immediate** (no heap allocation, no refcount):
/// the unit/bool/int/float scalars. A string is heap, so it is *not* immediate. Used to skip the
/// post-constructor operand release for immediates (nothing to release).
fn is_immediate_const(c: &IrConst) -> bool {
    matches!(
        c,
        // `f32` is an immediate NaN-boxed value (P-PACK Phase 3) — not refcounted, nothing to release.
        IrConst::Unit | IrConst::Bool(_) | IrConst::Int(_) | IrConst::Float(_) | IrConst::F32(_)
    )
}

/// Map a Core-IR constant to its bytecode-pool [`Const`].
fn const_value(c: &IrConst) -> Const {
    match c {
        IrConst::Unit => Const::Unit,
        IrConst::Bool(b) => Const::Bool(*b),
        IrConst::Int(i) => Const::Int(*i),
        IrConst::Float(f) => Const::Float(*f),
        IrConst::F32(f) => Const::F32(*f),
        IrConst::Str(s) => Const::Str(s.clone()),
    }
}

fn immutable_diag(name: &str, span: Span) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::ImmutableAssignment,
        span,
        format!("cannot assign to `{name}`, which is immutable"),
    )
    .with_help(format!(
        "declare it with `mut {name} = ...` to allow reassignment"
    ))
}

/// `cannot find type `T` in this scope` — an object literal naming an undeclared type
/// (mirrors the tree-walker's `eval_object` type lookup).
fn unknown_type_diag(name: &str, span: Span) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::UnknownName,
        span,
        format!("cannot find type `{name}` in this scope"),
    )
}

/// `type `T` has no field `f`` — an object literal initializing a field the type does not
/// declare (mirrors the tree-walker's `has_field` check).
fn unknown_field_diag(type_name: &str, field: &str, span: Span) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::UnknownName,
        span,
        format!("type `{type_name}` has no field `{field}`"),
    )
}

/// The runtime head constructor a **built-in type name** narrows to, or `None` when the name has
/// no dedicated head and falls back to a nominal (`NarrowTarget::Named`) match by shape name.
///
/// Exhaustive over [`BuiltinTy`] so this and the tree-walker's `runtime_matches` — the two halves
/// of the differential — cannot drift: a new built-in fails to compile in both until handled.
///
/// This funnel dropped a bare `tuple` head the VM alone recognized. `tuple` is not a built-in type
/// name (`Type::is_builtin_name` rejects it, so the checker reports an unknown type before any
/// narrowing runs) and the tree-walker always treated it nominally — so the two backends now agree
/// on an unreachable case they used to answer differently. A tuple target is written `(A, B)`,
/// which reaches [`NarrowTarget::Tuple`] through `TypeRef::Tuple`.
fn narrow_head(name: &str) -> Option<NarrowTarget> {
    use noeta_ast::BuiltinTy;
    Some(match BuiltinTy::from_name_any(name)? {
        BuiltinTy::Int => NarrowTarget::Int,
        BuiltinTy::Float => NarrowTarget::Float,
        // `f32` is reified at runtime (distinct NaN-box tag), so it gets a head; the matcher's
        // `F32 <: float` edge makes `(f32) is float` true while `(float) is f32` stays false.
        BuiltinTy::F32 => NarrowTarget::F32,
        BuiltinTy::Bool => NarrowTarget::Bool,
        BuiltinTy::Str => NarrowTarget::String,
        BuiltinTy::Bytes => NarrowTarget::Bytes,
        BuiltinTy::Unit => NarrowTarget::Unit,
        BuiltinTy::Dyn => NarrowTarget::Dyn,
        BuiltinTy::List => NarrowTarget::List,
        BuiltinTy::Map => NarrowTarget::Map,
        BuiltinTy::Set => NarrowTarget::Set,
        // Abstract kind-types match any value of that declaration kind.
        BuiltinTy::KindEnum => NarrowTarget::AnyEnum,
        BuiltinTy::KindStruct => NarrowTarget::AnyStruct,
        BuiltinTy::KindClass => NarrowTarget::AnyClass,
        // `Option`/`Result` are enums whose shape name *is* the type name, so they narrow through
        // the nominal path like a user enum rather than needing a head of their own.
        BuiltinTy::Option | BuiltinTy::Result => return None,
        // The erased widths (`f64`, `i8..u64`) carry no runtime tag on a scalar, so they fall to the
        // nominal path and never match a scalar. The checker warns on a bare-scalar `is i32`/`is f64`
        // (statically always-false); giving them heads would need scalar reification, which the arc
        // deliberately declines. `f32` alone is reified and handled above. Both backends agree.
        BuiltinTy::F64 | BuiltinTy::IntN { .. } => return None,
    })
}

/// Reduce a narrowing target type (`x.as<T>()`) to its runtime head constructor. Mirrors the
/// tree-walker's `runtime_matches` mapping exactly so both backends decide a narrowing the same
/// way. `Option`/`Result` and user records/classes/enums all become `Named` (matched by shape
/// name); generic arguments are dropped (erasure).
fn narrow_target(ty: &TypeRef) -> NarrowTarget {
    match ty {
        TypeRef::Union { members, .. } => {
            NarrowTarget::AnyOf(members.iter().map(narrow_target).collect())
        }
        TypeRef::Optional { .. } => NarrowTarget::Named("Option".to_string()),
        // A trait object narrows PRECISELY: the target carries the trait's canonical identity
        // (resolved at lowering by `resolve_type_aliases`), and the VM's `narrow_matches` tests the
        // value's nominal type against the module reflection's membership table — mirroring the
        // tree-walker's `runtime_matches` on the same shared table, so the differential holds by
        // construction.
        TypeRef::DynTrait { trait_name, .. } => NarrowTarget::DynTrait(trait_name.clone()),
        // A `Self::Name` projection has no static runtime head (resolution is per-impl at the
        // checker); narrowing to one stays the permissive dynamic top — deliberately, and now
        // UNLIKE the precise `dyn Trait` above: a projection names a concrete per-impl type, not a
        // trait, and the erased value carries no impl identity to reconstruct that binding from
        // (slice 1a).
        TypeRef::AssocProjection { .. } => NarrowTarget::Dyn,
        TypeRef::Tuple { .. } => NarrowTarget::Tuple,
        // Function types are erased: narrowing to one is a head-constructor "is callable" test
        // (params/return dropped), matching any function/closure value — like `List` ignoring its
        // element type.
        TypeRef::Fn { .. } => NarrowTarget::Fn,
        TypeRef::Named { name, args, .. } => {
            let head = narrow_head(name).unwrap_or_else(|| NarrowTarget::Named(name.clone()));
            // A parametrized target (`List<int>`, `Box<int>`) additionally checks its type arguments
            // against the value's reflected tag (R3); a bare name (`List`, `Box`, `Struct`) stays the
            // head-only target, preserving the widening `x is List` and the untagged fallback.
            if args.is_empty() {
                head
            } else {
                NarrowTarget::Generic {
                    head: Box::new(head),
                    args: args
                        .iter()
                        .map(noeta_ast::reflect::typeref_to_repr_arg)
                        .collect(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{compile, compile_with_sites};

    /// The compiled-in ctx fast route must key on the exact module identity the compiler emits —
    /// which is whatever `classify_use` returns in `UseKind::Module` (the ONE classification
    /// source both backends now consume). It silently rotted once when identities became
    /// root-qualified (`"cell"` → `"std.cell"`) and every `signal(…)`/`cell(…)` fell through to
    /// the dyn table with no behavioral difference to notice. This pins the two ends together
    /// through the real classification path.
    #[test]
    fn the_static_ctx_fast_route_keys_match_the_emitted_module_identity() {
        let reg = noeta_stdlib::registry::single_registry_process();
        let emitted = |name: &str| match reg.classify_use(&["std".to_string()], name) {
            noeta_stdlib::registry::UseKind::Module(q) => q,
            other => panic!("`use std.{name}` must classify as a module, got {other:?}"),
        };
        assert!(noeta_stdlib::registry::has_static_ctx_route(&emitted(
            "cell"
        )));
        assert!(noeta_stdlib::registry::has_static_ctx_route(&emitted(
            "reactive"
        )));
        // An out-of-std module never takes std's compiled-in route, even with a matching tail.
        assert!(!noeta_stdlib::registry::has_static_ctx_route("acme.cell"));
    }
    use noeta_ast::AttrValue;
    use noeta_ast::reflect::AttributeRecord;
    use noeta_bytecode::Module;
    use noeta_lexer::lex;
    use noeta_parser::parse;
    use noeta_span::{Source, SourceId};

    /// Compile `src` in **debug** mode (as `noeta dap` does), threading the checker's site maps into
    /// `compile_with_sites` with `debug = true` so the per-prototype debug info is emitted.
    fn compile_dbg(src: &str) -> Module {
        compile_flag(src, true)
    }

    /// Compile `src` through the full check→compile pipeline with an explicit `debug` flag, so a test
    /// can compare the two compiles of the *same* program (debug vs production).
    fn compile_flag(src: &str, debug: bool) -> Module {
        let source = Source::new(SourceId::FIRST, "test.noe", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        assert!(
            parsed.diagnostics.is_empty(),
            "test program must parse cleanly: {:?}",
            parsed.diagnostics
        );
        let checked = noeta_check::check_all(&parsed.program);
        assert!(
            checked.diagnostics.is_empty(),
            "test program must type-check cleanly: {:?}",
            checked.diagnostics
        );
        compile_with_sites(&parsed.program, checked.sites, false, debug).expect("compiles")
    }

    #[test]
    fn debug_skips_plain_local_last_use_drops() {
        // `a` is read for the last time at `mut b = a + 1`; the drop pass places an `Op::Drop` there.
        // A **production** compile keeps that drop (prompt reclamation, unchanged). A **debug** compile
        // skips it so a debugger paused at `b` can still read `a` — the whole point of D2. All values
        // here are non-destructor immediates, so this is pure promptness, invisible to behaviour.
        let src = "fn f(): int {\n  mut a = 10;\n  mut b = a + 1;\n  return b\n}\nf();\n";
        let drops = |m: &Module| {
            m.protos
                .iter()
                .flat_map(|c| c.code.iter())
                .filter(|op| matches!(op, noeta_bytecode::Op::Drop { .. }))
                .count()
        };
        let prod = drops(&compile_flag(src, false));
        let dbg = drops(&compile_flag(src, true));
        assert!(prod > 0, "production drops `a` at its last use");
        assert!(
            dbg < prod,
            "debug keeps plain locals live (skips their last-use drops): prod={prod} debug={dbg}"
        );
    }

    #[test]
    fn main_carries_its_name_and_defining_span() {
        let m = compile_dbg("echo \"hi\";\n");
        let main = m.main();
        assert_eq!(main.name.as_deref(), Some("main"));
        assert!(main.def_span.is_some());
    }

    #[test]
    fn a_functions_locals_and_params_are_recorded_by_register() {
        // `x`/`y` are locals (declare_local); `p` is a parameter — all should appear, each with a
        // register. Top-level `mut` binds a *global*, so the names live inside a function.
        let m = compile_dbg(
            "fn f(p: int): int {\n  mut x = p;\n  mut y = 2;\n  return x + y\n}\nf(1);\n",
        );
        let f = m
            .protos
            .iter()
            .find(|c| c.name.as_deref() == Some("f"))
            .expect("a proto named f");
        let names: std::collections::HashSet<&str> =
            f.debug_locals.iter().map(|l| l.name.as_str()).collect();
        assert!(names.contains("p"), "missing param p: {names:?}");
        assert!(names.contains("x"), "missing local x: {names:?}");
        assert!(names.contains("y"), "missing local y: {names:?}");
        // Every recorded register is a valid slot in the frame.
        for local in &f.debug_locals {
            assert!(
                local.reg < f.num_registers,
                "reg {} out of range",
                local.reg
            );
        }
    }

    #[test]
    fn named_locals_stay_one_to_one_under_coalescing() {
        // `a` dies once `r` copies it, before `b` is defined — disjoint live ranges that coalescing
        // would merge onto one register. In a debug compile named locals are pinned, so each keeps a
        // distinct slot: the 1:1 `reg → name` the Variables view depends on. (Without pinning `a` and
        // `b` would share a register and the distinct-count check below would fail.)
        let m = compile_dbg(
            "fn f(): int {\n  mut a = 1;\n  mut r = a;\n  mut b = 2;\n  r = r + b;\n  return r\n}\nf();\n",
        );
        let f = m
            .protos
            .iter()
            .find(|c| c.name.as_deref() == Some("f"))
            .expect("a proto named f");
        let regs: Vec<u16> = f.debug_locals.iter().map(|l| l.reg).collect();
        let distinct: std::collections::HashSet<u16> = regs.iter().copied().collect();
        assert_eq!(
            regs.len(),
            distinct.len(),
            "reg->name is not 1:1: {:?}",
            f.debug_locals
        );
    }

    #[test]
    fn a_method_proto_is_named_type_dot_method() {
        let m = compile_dbg(
            "struct Point { x: int\n  fn mag(): int { return self.x }\n}\nmut p = Point { x: 3 };\np.mag();\n",
        );
        assert!(
            m.protos
                .iter()
                .any(|c| c.name.as_deref() == Some("Point.mag")),
            "expected a proto named Point.mag; names: {:?}",
            m.protos.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn loop_and_match_bindings_are_named_locals_in_a_debug_compile() {
        // A `for` variable and a match-arm binding bind through `bind_loop_var`, not
        // `declare_local` — both must still reach the Variables view, each 1:1 on its own
        // register (pinned via `debug_locals` itself, NOT the panic-teardown list).
        let m = compile_dbg(
            "fn f(values: List<int>): int {\n  mut total = 0;\n  for v in values {\n    total = total + v;\n  }\n  return match total { n => n }\n}\nf([1, 2]);\n",
        );
        let f = m
            .protos
            .iter()
            .find(|c| c.name.as_deref() == Some("f"))
            .expect("a proto named f");
        let names: Vec<&str> = f.debug_locals.iter().map(|l| l.name.as_str()).collect();
        assert!(names.contains(&"v"), "missing loop var v: {names:?}");
        assert!(names.contains(&"n"), "missing match binding n: {names:?}");
        // The loop var holds a fresh element register — it must keep a slot of its own. (The
        // match binding `n` is different: it *aliases* the scrutinee's register by design, so
        // sharing `total`'s slot is truthful, not a coalescing collapse.)
        let reg_of = |name: &str| f.debug_locals.iter().find(|l| l.name == name).unwrap().reg;
        let v_reg = reg_of("v");
        for other in ["values", "total", "n"] {
            assert_ne!(v_reg, reg_of(other), "v shares a register with {other}");
        }
        assert_eq!(reg_of("n"), reg_of("total"), "n aliases its scrutinee");
        // The loop var is a named local, not a teardown register: the teardown list is behavior
        // (which destructors fire on a panic) and a debug compile must not change it.
        let v_reg = f
            .debug_locals
            .iter()
            .find(|l| l.name == "v")
            .expect("v recorded")
            .reg;
        assert!(
            !f.frame_locals.contains(&v_reg),
            "loop var leaked into the panic-teardown list"
        );
    }

    #[test]
    fn a_non_debug_compile_carries_line_info_but_no_full_debug_info() {
        // The two debug-info tiers: **line info** (names, defining spans, the pc→line table) is
        // always emitted — production stack traces resolve frames through it, and it is pure cold
        // metadata that cannot affect codegen. **Full debug** (`debug_locals`, whose contract pins
        // registers through coalescing) is a codegen trade only the `noeta dap` debug compile makes.
        let source = Source::new(
            SourceId::FIRST,
            "test.noe",
            "fn f() {\n  mut x = 1;\n  echo x;\n}\nf();\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        assert!(parsed.diagnostics.is_empty());
        let m = compile(&parsed.program).expect("compiles");
        // Line-info tier: present without the debug flag.
        let f = m
            .protos
            .iter()
            .find(|c| c.name.as_deref() == Some("f"))
            .expect("f is named off-debug");
        assert!(f.def_span.is_some(), "expected a def_span off-debug");
        assert!(
            !f.line_table.is_empty(),
            "expected a line table off-debug (stack traces resolve through it)"
        );
        // Full-debug tier: absent without the debug flag.
        for chunk in &m.protos {
            assert!(
                chunk.debug_locals.is_empty(),
                "unexpected debug_locals off-debug"
            );
        }
    }

    /// Compile `src` and return its attribute manifest (the VM-side view of the shared artifact).
    fn manifest(src: &str) -> Vec<AttributeRecord> {
        let source = Source::new(SourceId::FIRST, "test.noe", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        assert!(
            parsed.diagnostics.is_empty(),
            "test program must parse cleanly: {:?}",
            parsed.diagnostics
        );
        compile(&parsed.program)
            .expect("compiles")
            .reflection
            .manifest
    }

    #[test]
    fn bare_attribute_has_no_args() {
        let m = manifest("#[Entity]\nstruct User { id: int }\n");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].target, "User");
        assert_eq!(m[0].name, "Entity");
        assert!(m[0].args.is_empty());
    }

    #[test]
    fn positional_literal_args_reach_the_manifest() {
        // The richer-argument case: a string literal survives end-to-end into the manifest as a
        // typed value (not the identifier-only form of the earlier prototype).
        let m = manifest("#[Route(\"/users\")]\nstruct Users { id: int }\n");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].args.len(), 1);
        assert_eq!(m[0].args[0].name, None);
        assert_eq!(m[0].args[0].value, AttrValue::Str("/users".to_string()));
    }

    #[test]
    fn named_and_mixed_literal_args_reach_the_manifest() {
        let m = manifest("#[Cache(ttl: 60, eager: true)]\nstruct Page { id: int }\n");
        assert_eq!(m[0].args.len(), 2);
        assert_eq!(m[0].args[0].name, Some("ttl".to_string()));
        assert_eq!(m[0].args[0].value, AttrValue::Int(60));
        assert_eq!(m[0].args[1].name, Some("eager".to_string()));
        assert_eq!(m[0].args[1].value, AttrValue::Bool(true));
    }

    #[test]
    fn vm_reflection_is_the_shared_builder_output() {
        // P2.0's parity guarantee: the VM-side artifact (in the compiled `Module`) is *exactly*
        // what `noeta_ast::reflect::build` produces from the AST — the same pure builder the
        // tree-walker calls. So both backends agree on reflection by construction, no drift.
        let src = "#[Entity]\nstruct User { id: int }\nenum Color { Red; Rgb(r: int); }\n";
        let source = Source::new(SourceId::FIRST, "t.noe", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let from_module = compile(&parsed.program).expect("compiles").reflection;
        // `compile` embeds the installed extensions' attribute shapes (`#[Skip]`/`#[Bench]`/…,
        // now registry-declared) via `extend_reflection`, so the parity comparison applies the
        // same embedding to the raw builder output.
        // `compile` builds reflection against the process-global registry (`CompileOptions::default`),
        // so the parity comparison must feed the raw builder the same native-role table.
        let registry = noeta_ext_abi::registry::single_registry_process();
        let native_roles = registry.native_roles();
        let native_traits = noeta_ir::native_trait_impls(registry);
        let mut from_builder =
            noeta_ast::reflect::build(&parsed.program, &native_roles, &native_traits);
        noeta_check::extend_reflection(&mut from_builder);
        assert_eq!(from_module, from_builder);
        // Deterministic: the same AST always yields the same artifact.
        let mut again = noeta_ast::reflect::build(&parsed.program, &native_roles, &native_traits);
        noeta_check::extend_reflection(&mut again);
        assert_eq!(from_builder, again);
    }

    /// The membership table behind the precise `is dyn Trait` / `traits_of`: a standalone
    /// `impl Trait for T`, an in-body `impl` block, and a `@derive` (built-in and user traits
    /// alike) all register — and a type with none registers nothing (the row-absence that makes
    /// the runtime test answer `false` where it used to answer `true`).
    #[test]
    fn reflection_records_trait_impls_from_every_declaration_form() {
        let src = "trait Speaks { fn speak(): string }\n\
                   trait Greets { fn hello(): string { return \"hi\"; } }\n\
                   struct Dog { name: string }\n\
                   impl Speaks for Dog { fn speak(): string { return \"woof\"; } }\n\
                   @derive(Greets)\n\
                   struct Robot { id: int  impl Display { fn to_string(): string { return \"r\"; } } }\n\
                   struct Plain { n: int }\n";
        let source = Source::new(SourceId::FIRST, "t.noe", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let reflection = compile(&parsed.program).expect("compiles").reflection;
        assert_eq!(reflection.traits_for("Dog"), vec!["Speaks"]);
        assert_eq!(reflection.traits_for("Robot"), vec!["Display", "Greets"]);
        assert!(reflection.traits_for("Plain").is_empty());
        assert!(reflection.type_implements("Dog", "Speaks"));
        assert!(!reflection.type_implements("Plain", "Speaks"));
    }

    #[test]
    fn type_registry_records_declared_shapes() {
        // The shared artifact also carries every declared type's reflectable shape (name, kind,
        // member names) — the half `attributes_of` materialization and `type_of` will read.
        let source = Source::new(
            SourceId::FIRST,
            "t.noe",
            "struct Point { x: int y: int }\nenum Color { Red; Rgb(r: int); }\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let reflection = compile(&parsed.program).expect("compiles").reflection;
        let point = reflection.type_named("Point").expect("Point registered");
        assert_eq!(point.kind, noeta_ast::reflect::TypeKind::Struct);
        assert_eq!(point.fields, vec!["x".to_string(), "y".to_string()]);
        let color = reflection.type_named("Color").expect("Color registered");
        assert_eq!(color.kind, noeta_ast::reflect::TypeKind::Enum);
        assert_eq!(color.variants.len(), 2);
        assert_eq!(color.variants[1].name, "Rgb");
        assert_eq!(color.variants[1].fields, vec!["r".to_string()]);
    }
}
