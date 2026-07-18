//! Lowering — `AST → Core IR`.
//!
//! A **pure, total** translation of a parsed program into the A-normal-form [`Program`].
//! "Total" in the sense the migration needs: every construct the IR interpreter supports
//! lowers; anything not yet covered returns [`Unsupported`] (never a panic, never a partial
//! tree), so the transitional differential can *skip* exactly the programs the IR path does
//! not yet handle — the same skip discipline the VM's bytecode path uses. As coverage grows
//! the set of `Unsupported` programs shrinks to empty.
//!
//! The single invariant the lowering establishes is **explicit evaluation order**: every
//! nested sub-expression becomes a preceding `let t = …` over atoms, in exactly the order
//! the tree-walker evaluates them. Order stops being something each backend rederives and
//! becomes IR structure.
//!
//! # On type facts
//!
//! Phase 1's IR carries no reference-counting annotations yet, so the lowering is purely
//! syntactic and does **not** consult the type checker. The reference-counting phase will
//! need per-value type facts (which fields are heap-bearing) that the checker does not
//! expose today; wiring that in is that phase's prerequisite, deliberately out of scope here
//! so the Phase 1 IR stays a faithful, RC-neutral mirror of the AST.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use std::collections::HashMap as StdHashMap;

use noeta_ast::{
    BinaryOp, Expr, FnDecl, ForPattern as AstForPattern, Param, Program as AstProgram,
    Stmt as AstStmt, StrPart, TypeRef,
};
use noeta_span::Span;

use crate::{
    Atom, Block, ClassDef, Const, Decl, EnumDef, Func, InterpPart, Program, Rvalue, Stmt,
    StructDef, Temp, Thunk,
};

mod state_machine;
use state_machine::{PENDING_IDENT, POLL_FN, SuspendMode, body_has_yield, desugar_state_machine};

/// A construct the lowering does not yet handle. Carried back so the caller can skip the
/// program (the transitional differential's "outside the IR subset" bucket), mirroring the
/// VM's `Unsupported`.
#[derive(Debug, Clone)]
pub struct Unsupported {
    /// A short, stable description of the unhandled construct (for diagnostics/tests).
    pub feature: &'static str,
    pub span: Span,
}

impl Unsupported {
    /// Build an "outside the IR subset" marker. The lowering is now **total** over the current
    /// AST — every construct lowers — so this is unused today; it is retained as the skip path
    /// the transitional differential expects, ready for any new AST node added ahead of its
    /// lowering (and to keep the one-slice-at-a-time discipline available).
    #[allow(dead_code)]
    fn at(feature: &'static str, span: Span) -> Unsupported {
        Unsupported { feature, span }
    }
}

/// Where a function's body comes from: an arrow expression (its value is the return) or a
/// statement block (returns via `return`, else unit).
enum BodyKind<'a> {
    Arrow(&'a Expr),
    Block(&'a [AstStmt]),
}

/// The checker's **lowering-site maps**, bundled so the lowering takes one reference rather than six
/// span-keyed side channels. Every field is a pure function of the program, so the representation
/// choices they drive stay invisible to `RunResult`; the REPL / IR-corpus path passes an all-empty
/// set (see [`lower`]) and stays on the boxed/unfused path.
/// `Copy` — it is a bundle of shared references.
#[derive(Clone, Copy, Debug)]
pub struct LoweringSites<'a> {
    /// A list literal whose span is here lowers to a **streaming** flat build (a
    /// [`Rvalue::PackedListNew`] then one [`Rvalue::PackedListPush`] per element) instead of a boxed
    /// [`Rvalue::List`] — its element is a `@packed` struct with this flat
    /// [`noeta_ast::reflect::PackedLayout`].
    pub packed_list_sites: &'a HashMap<Span, noeta_ast::reflect::PackedLayout>,
    /// A `list[i].field` member read whose span is here (the index receiver is a built-in `List`) fuses
    /// to a single [`Rvalue::IndexField`], reading a packed element's field without materializing it.
    pub index_field_sites: &'a HashSet<Span>,
    /// Call-site-typed native-call recipes (`json.parse::<T>`), baked into [`Rvalue::TypedModuleCall`].
    pub typed_module_call_sites: &'a HashMap<Span, noeta_ext_abi::TypeRecipe>,
    /// `json.decode_typed(name, text)` call spans (L2.2 DI) → lowered to [`Rvalue::DecodeTyped`]
    /// instead of a generic method call, routing to the runtime decode-by-type registry.
    pub decode_typed_sites: &'a HashSet<Span>,
    /// `for` spans whose iterable is statically an `Iterator<T>` → the lowered [`Stmt::For`] streams
    /// via `next()` rather than snapshotting a list (Track I.2).
    pub for_stream_sites: &'a HashSet<Span>,
    /// Fixed-width arithmetic sites (Tier W) → the result's `(signed, bits)`, wrapping the op's result
    /// in [`Rvalue::MaskWidth`] so the erased i64 is masked back into the declared width.
    pub width_sites: &'a HashMap<Span, (bool, u8)>,
    /// Collection-construction sites → the resolved element [`noeta_ast::reflect::TypeRepr`] baked onto
    /// [`Rvalue::List`] so `type_of` recovers it after a `dyn` launder (R1 reflection).
    pub construction_sites: &'a HashMap<Span, noeta_ast::reflect::TypeRepr>,
    /// Unbound method-handle sites (`Type.method` in value position) → the resolved
    /// `(ty, method, associated)`, emitted as an [`Rvalue::MethodHandle`] instead of a field load.
    pub handle_sites: &'a HashMap<Span, (String, String, bool)>,
    /// **Bound**-handle sites (`value.method` in value position, EX.2b) → emitted as an
    /// [`Rvalue::BoundHandle`] (the receiver captured) instead of a field load.
    pub bound_handle_sites: &'a HashSet<Span>,
    /// Bare float-literal spans the checker adapted into an `f32` context (`mut x: f32 = 1.5`,
    /// P-NUM-SYM). Unlike `f64` (bit-identical to `float`), `f32` is a *distinct 32-bit*
    /// representation, so an adapted literal must lower to a narrow [`Const::F32`] rather than the
    /// default [`Const::Float`] — this set is that type-directed hint.
    pub f32_literal_sites: &'a HashSet<Span>,
    /// Method-bundle call sites (kernel-methods K2) → the resolved `(module, bundle)` route,
    /// baked into an [`Rvalue::BundleMethod`] instead of the generic [`Rvalue::Method`].
    pub bundle_call_sites: &'a HashMap<Span, (String, String)>,
    /// Namespace-group member-access sites (`http.client`) → the resolved concrete module identity,
    /// emitted as an [`Rvalue::NativeModule`] instead of a field load.
    pub namespace_module_sites: &'a HashMap<Span, String>,
}

impl LoweringSites<'static> {
    /// The all-empty site set (the no-hints path: REPL / IR-corpus / property tests) — `'static`
    /// borrows of shared empty maps, so callers need no eleven-local dance to keep them alive.
    pub fn empty() -> LoweringSites<'static> {
        use std::sync::OnceLock;
        static PACKED: OnceLock<HashMap<Span, noeta_ast::reflect::PackedLayout>> = OnceLock::new();
        static SPANS: OnceLock<HashSet<Span>> = OnceLock::new();
        static RECIPES: OnceLock<HashMap<Span, noeta_ext_abi::TypeRecipe>> = OnceLock::new();
        static WIDTHS: OnceLock<HashMap<Span, (bool, u8)>> = OnceLock::new();
        static REPRS: OnceLock<HashMap<Span, noeta_ast::reflect::TypeRepr>> = OnceLock::new();
        static HANDLES: OnceLock<HashMap<Span, (String, String, bool)>> = OnceLock::new();
        static PAIRS: OnceLock<HashMap<Span, (String, String)>> = OnceLock::new();
        static NAMES: OnceLock<HashMap<Span, String>> = OnceLock::new();
        LoweringSites {
            packed_list_sites: PACKED.get_or_init(HashMap::new),
            index_field_sites: SPANS.get_or_init(HashSet::new),
            typed_module_call_sites: RECIPES.get_or_init(HashMap::new),
            decode_typed_sites: SPANS.get_or_init(HashSet::new),
            for_stream_sites: SPANS.get_or_init(HashSet::new),
            width_sites: WIDTHS.get_or_init(HashMap::new),
            construction_sites: REPRS.get_or_init(HashMap::new),
            handle_sites: HANDLES.get_or_init(HashMap::new),
            bound_handle_sites: SPANS.get_or_init(HashSet::new),
            f32_literal_sites: SPANS.get_or_init(HashSet::new),
            bundle_call_sites: PAIRS.get_or_init(HashMap::new),
            namespace_module_sites: NAMES.get_or_init(HashMap::new),
        }
    }
}

/// Project a checker `Sites` bundle (or anything with the same field names) into a
/// [`LoweringSites`] borrow bundle — THE one projection. `noeta-check` and `noeta-ir` are
/// deliberately decoupled (no dependency edge in either direction), so this is a field-name-
/// coupled macro rather than a `From` impl; before it existed the projection was hand-copied at
/// seven call sites across four crates, and a driver that forgot one field compiled fine while
/// silently dropping a semantic hint (`f32_literal_sites`) or a fusion (`index_field_sites`).
/// Adding a site map is now three lines: the `Sites` field, the [`LoweringSites`] field, and one
/// line here.
#[macro_export]
macro_rules! lowering_sites {
    ($s:expr) => {
        $crate::LoweringSites {
            packed_list_sites: &$s.packed_list_sites,
            index_field_sites: &$s.index_field_sites,
            typed_module_call_sites: &$s.typed_module_call_sites,
            decode_typed_sites: &$s.decode_typed_sites,
            for_stream_sites: &$s.for_stream_sites,
            width_sites: &$s.width_sites,
            construction_sites: &$s.construction_sites,
            handle_sites: &$s.handle_sites,
            bound_handle_sites: &$s.bound_handle_sites,
            f32_literal_sites: &$s.f32_literal_sites,
            bundle_call_sites: &$s.bundle_call_sites,
            namespace_module_sites: &$s.namespace_module_sites,
        }
    };
}

/// Lower a whole parsed program to the Core IR, or report the first construct outside the
/// currently-supported subset. List literals lower to the boxed [`Rvalue::List`] and `list[i].f`
/// reads to the unfused [`Rvalue::Index`] + [`Rvalue::Field`]; see [`lower_with_sites`] to also
/// stream `List<packed>` literals into a flat buffer and fuse indexed field reads.
pub fn lower(program: &AstProgram) -> Result<Program, Unsupported> {
    lower_with_sites(program, LoweringSites::empty())
}

/// Everything that varies a lowering, so callers configure one entry point
/// ([`lower_with_sites_opts`]) instead of this family growing a positional-flag parameter per
/// concern (audit-3 finding 9 — the checker's [`CheckOptions`] pattern applied here). `Default`
/// is every in-oracle path: cooperative isolates, process-global registry — identical to
/// [`lower_with_sites`].
///
/// [`CheckOptions`]: noeta_check::CheckOptions
pub struct LowerOptions {
    /// Selects the isolate lowering (isolates I.4b): when **true**, `isolate f(args)` lowers to
    /// [`Rvalue::SpawnIsolate`] (callee + unbuilt args, for the real OS-thread path); when
    /// **false** (every in-oracle path — the differential, the salsa graph, the REPL), it lowers
    /// to a plain [`Rvalue::Spawn`] of the pre-built future, exactly as `spawn f(args)`, so the
    /// sandbox and the whole differential corpus are byte-identical and never see the new op.
    /// Only the CLI's real (VM) execution path passes `true`.
    pub real_isolates: bool,
    /// The extension registry native-type import narrowing resolves against (instance-registry
    /// IR5): `collect_type_aliases` reads it so `is`/`as`/`type_of` on an imported extern type
    /// lower to the right qualified identity. The production/CLI path keeps the process-global
    /// default; an embed session threads its own assembled set, so a session's compile honors
    /// its extensions.
    pub registry: &'static noeta_ext_abi::registry::Registry,
}

impl Default for LowerOptions {
    fn default() -> Self {
        LowerOptions {
            real_isolates: false,
            registry: noeta_ext_abi::registry::single_registry_process(),
        }
    }
}

impl std::fmt::Debug for LowerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LowerOptions")
            .field("real_isolates", &self.real_isolates)
            // The registry is a `&'static` handle whose contents aren't `Debug`.
            .finish_non_exhaustive()
    }
}

/// As [`lower`], but driven by the checker's [`LoweringSites`] (all pure functions of the program, so
/// the optimizations they enable stay invisible to `RunResult`). The production execution paths
/// (`lang run`, the conformance reference, the bytecode compiler) pass real maps; the REPL and IR
/// corpus pass empty ones and stay on the boxed/unfused path. Single-registry, cooperative isolates
/// ([`LowerOptions::default`]).
pub fn lower_with_sites(
    program: &AstProgram,
    sites: LoweringSites,
) -> Result<Program, Unsupported> {
    lower_with_sites_opts(program, sites, LowerOptions::default())
}

/// Copy a standalone `impl Trait for T { methods }`'s method bodies onto the target type `T`'s own
/// method table (L1 user traits, UT2), so both backends' `(type, method)` dispatch resolves them —
/// the same flattening the parser already performs for in-body `impl` blocks. Additionally hoists
/// **default-method fallback** (UT5): a trait method the impl omits falls back to the trait's
/// default body, materialized onto the type for every impl form — standalone, in-body, and a
/// `@derive(UserTrait)` (which adopts *all* defaults; the checker enforces the trait is fully
/// defaulted). A provided method always wins over a default (the name-skip below). Generic traits
/// are excluded from default hoisting (their bodies would need per-implementor substitution —
/// deferred with generic-trait derivation). Returns a modified clone only when something was
/// hoisted; otherwise `None` (use the original by reference — zero cost for the common case).
///
/// **Idempotent**: a method whose name already exists on the target is skipped, so applying this to
/// an already-hoisted program is a no-op. That lets the VM compiler hoist the AST `entry` for its
/// surface-reading pass-1 (`register_types`) while the shared lowering below hoists again for the
/// IR — both converge without duplicating a method. The IR interpreter (reference/eval) has no such
/// split, so the lowering call alone covers it.
pub fn hoist_standalone_impl_methods(program: &AstProgram) -> Option<AstProgram> {
    // The trait declarations by name, for default-method fallback (UT5). Only a NON-generic
    // trait's defaults hoist — a generic trait's default body would need type-parameter
    // substitution per implementor, which is deferred with the rest of generic-trait derivation.
    let traits: StdHashMap<&str, &noeta_ast::TraitDecl> = program
        .stmts
        .iter()
        .filter_map(|s| match s {
            AstStmt::Trait(t) if t.type_params.is_empty() => Some((t.name.as_str(), t)),
            _ => None,
        })
        .collect();
    // A trait's default methods that `provided` does not override, in declaration order.
    let omitted_defaults = |trait_name: &str, provided: &[FnDecl]| -> Vec<FnDecl> {
        traits
            .get(trait_name)
            .map(|t| {
                t.methods
                    .iter()
                    .filter(|tm| tm.has_default && !provided.iter().any(|m| m.name == tm.sig.name))
                    .map(|tm| tm.sig.clone())
                    .collect()
            })
            .unwrap_or_default()
    };

    let mut additions: StdHashMap<String, Vec<FnDecl>> = StdHashMap::new();
    for stmt in &program.stmts {
        if let AstStmt::Impl(decl) = stmt {
            let entry = additions.entry(decl.target.clone()).or_default();
            entry.extend(decl.methods.iter().cloned());
            // Default-method fallback (UT5): the impl'd trait's omitted defaults ride along,
            // after the impl's own methods so a provided override wins the name-skip below.
            entry.extend(omitted_defaults(&decl.trait_name, &decl.methods));
        }
    }
    additions.retain(|_, v| !v.is_empty());
    // Fast path: nothing to hoist — no standalone-impl methods and no type body referencing a
    // known trait through an in-body impl or a derive. Keeps the common no-trait program clone-free.
    let body_references_trait =
        |impls: &[noeta_ast::ImplBlock], derives: &[noeta_ast::DeriveSpec]| {
            impls
                .iter()
                .any(|b| traits.contains_key(b.trait_name.as_str()))
                || derives.iter().any(|d| traits.contains_key(d.name.as_str()))
        };
    let body_needs = program.stmts.iter().any(|s| match s {
        AstStmt::Struct(d) => body_references_trait(&d.impls, &d.derives),
        AstStmt::Class(d) => body_references_trait(&d.impls, &d.derives),
        AstStmt::Enum(d) => body_references_trait(&d.impls, &d.derives),
        _ => false,
    });
    // A builtin `via:` derive (`@derive(Comparable, via: amount)`) synthesizes a method even with
    // no user trait in the program.
    let has_via = |derives: &[noeta_ast::DeriveSpec]| derives.iter().any(|d| d.via.is_some());
    let body_needs = body_needs
        || program.stmts.iter().any(|s| match s {
            AstStmt::Struct(d) => has_via(&d.derives),
            AstStmt::Class(d) => has_via(&d.derives),
            AstStmt::Enum(d) => has_via(&d.derives),
            _ => false,
        });
    if additions.is_empty() && !body_needs {
        return None;
    }
    let mut changed = false;
    let mut cloned = program.clone();
    for stmt in &mut cloned.stmts {
        let (name, methods, fields, impls, derives) = match stmt {
            AstStmt::Struct(d) => (
                &d.name,
                &mut d.methods,
                d.fields.as_slice(),
                &d.impls,
                &d.derives,
            ),
            AstStmt::Class(d) => (
                &d.name,
                &mut d.methods,
                d.fields.as_slice(),
                &d.impls,
                &d.derives,
            ),
            AstStmt::Enum(d) => (
                &d.name,
                &mut d.methods,
                &[] as &[noeta_ast::FieldDecl],
                &d.impls,
                &d.derives,
            ),
            _ => continue,
        };
        // UT5 for the unified body: an in-body `impl Trait { … }` block's omitted defaults, plus a
        // derive's *planned* methods (derive layers 1+2 — defaults adopted, required members
        // bridged onto fields/methods, `via:` forwards, builtin `via:` templates) from the shared
        // planner the checker validates with. A plan error contributes nothing here — the checker
        // reports it and the program does not run. The parser already copied an in-body impl's OWN
        // methods into `methods`, so the name-skip makes provided overrides win here too.
        let mut synthesized: Vec<FnDecl> = Vec::new();
        for block in impls {
            synthesized.extend(omitted_defaults(&block.trait_name, methods));
        }
        for spec in derives {
            let planned = if let Some(t) = traits.get(spec.name.as_str()) {
                noeta_ast::derive::plan_user_trait_derive(t, fields, methods, spec)
            } else if spec.via.is_some() {
                noeta_ast::derive::plan_builtin_via(&spec.name, name, fields, spec)
            } else {
                continue;
            };
            if let Ok(planned) = planned {
                synthesized.extend(planned);
            }
        }
        let add = additions.get(name.as_str()).cloned().unwrap_or_default();
        for m in add.into_iter().chain(synthesized) {
            if !methods.iter().any(|existing| existing.name == m.name) {
                methods.push(m);
                changed = true;
            }
        }
    }
    changed.then_some(cloned)
}

/// As [`lower_with_sites`], but against explicit [`LowerOptions`] — the single configurable entry
/// the other `lower*` functions are thin presets of.
pub fn lower_with_sites_opts(
    program: &AstProgram,
    sites: LoweringSites,
    opts: LowerOptions,
) -> Result<Program, Unsupported> {
    let LowerOptions {
        real_isolates,
        registry,
    } = opts;
    // Hoist standalone-`impl` methods onto their target type (L1 user traits, UT2) before lowering,
    // so `(type, method)` dispatch resolves them. Only rebinds when such an impl exists.
    let hoisted = hoist_standalone_impl_methods(program);
    let program: &AstProgram = hoisted.as_ref().unwrap_or(program);
    let mut lowerer = Lowerer {
        temps: 0,
        sites,
        real_isolates,
        synth_step_name: None,
        type_aliases: collect_type_aliases(program, registry),
        expr_tiers: noeta_ast::desugar::expr_tier_handlers(program)
            .into_iter()
            .collect(),
        registry,
    };
    let top = lowerer.lower_body(&program.stmts)?;
    Ok(Program {
        top,
        temp_count: lowerer.temps,
        span: program.span,
    })
}

/// Carries the temporary counter for the function frame currently being lowered. One
/// `Lowerer` field is reused across nested frames by save/restore (see `lower_func`), so the
/// counter always reflects the innermost activation.
struct Lowerer<'a> {
    /// The next free temporary index in the current frame; also the running frame size.
    temps: u32,
    /// The checker's lowering-site maps (see [`LoweringSites`]) — the span-keyed hints that drive
    /// packed/fused/streamed lowering. Empty on the boxed/unfused REPL/IR-corpus path.
    sites: LoweringSites<'a>,
    /// Whether `isolate f(args)` lowers to [`Rvalue::SpawnIsolate`] (real OS-thread path, I.4b) rather
    /// than a plain [`Rvalue::Spawn`] of a pre-built future. Only the CLI's real (VM) execution path
    /// sets this; every in-oracle path leaves it false, so the differential never sees the new rvalue.
    real_isolates: bool,
    /// The name the **next** lowered closure should carry — set (to the enclosing function's name)
    /// just before an async/generator desugar lowers its synthesized step closure, and `take()`n by
    /// the first `Expr::Closure` the lowering meets (the step itself, which lowers before anything
    /// nested inside it). A user's own closure therefore always finds `None` and stays anonymous.
    synth_step_name: Option<String>,
    /// The runtime **narrowing identity** each `use`-imported local type name resolves to, so a
    /// target (`x is MyId`, `x.as<Uuid>()`) matches the value's runtime tag in both backends (they
    /// share this lowered IR). Two kinds of entry:
    ///
    /// - a **native** type (`use std.id.Uuid [as MyId]`) → its **qualified** identity
    ///   (`std.id.Uuid`), which is what an extern value reports for narrowing — so a native target
    ///   never collides with a same-short-named *user* type (whose runtime tag is the bare name).
    /// - a **user-type alias** (`use App.User as Customer`) → the imported type's own name (`User`),
    ///   its runtime tag.
    ///
    /// A plain (non-aliased) *user* import needs no entry — its local name is already the tag.
    type_aliases: HashMap<String, String>,
    /// The program's declared expression-tier handlers (tier name → handler fn name), so an
    /// [`Expr::TierExpr`] lowers as the handler call it means — the same
    /// [`noeta_ast::desugar::tier_expr_call`] construction the checker typed. The checker gated
    /// unknown/non-expr tiers (E0052), so a miss here is `Unsupported`, never a panic.
    expr_tiers: HashMap<String, String>,
    /// The extension registry a **native** expression tier's handler resolves against
    /// (instance-registry IR5): an `@json` block's `ExtTier::handler` is looked up here, so an
    /// embed session's own extension-declared expression tier lowers against *its* registry.
    registry: &'static noeta_ext_abi::registry::Registry,
}

/// Build the narrowing-identity map (see [`Lowerer::type_aliases`]) from a program's `use`
/// statements. A native-type import resolves to its qualified identity via the registry; a renamed
/// user-type import resolves to its leaf name.
fn collect_type_aliases(
    program: &AstProgram,
    registry: &'static noeta_ext_abi::registry::Registry,
) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for stmt in &program.stmts {
        let AstStmt::Use { path, names, .. } = stmt else {
            continue;
        };
        let prefix = path.join(".");
        for n in names {
            let local = n.alias.clone().unwrap_or_else(|| n.name.clone());
            let qualified = format!("{prefix}.{}", n.name);
            if let Some(ext) = registry.find_type_qualified(&qualified) {
                // A native type: narrows against its qualified identity, whichever local name it
                // was bound to.
                map.insert(local, ext.qualified());
            } else if registry.is_namespace(&qualified) {
                // A namespace group (`use std.http`): expose its types so a *dotted* narrowing
                // target (`http.Response`) resolves to the same qualified identity a value carries,
                // exactly as the checker's import collection does. Aliased groups key on the alias.
                for (rel, q) in registry.namespace_types(&qualified) {
                    map.insert(format!("{local}.{rel}"), q);
                }
            } else if n.alias.is_some() {
                // A renamed user (or opaque) import: narrows against the imported leaf name.
                map.insert(local, n.name.clone());
            }
        }
    }
    map
}

impl Lowerer<'_> {
    /// Rewrite a narrowing target's [`TypeRef`] so any import **alias** resolves to the imported
    /// type's own name (`MyId` → `Uuid`) — recursively, so `List<MyId>` / `?MyId` are covered too —
    /// making `is`/`as`/`type_of` match a value's runtime tag. A no-op (plain clone) when the file
    /// declared no aliases, which is the overwhelmingly common case.
    fn resolve_type_aliases(&self, ty: &TypeRef) -> TypeRef {
        if self.type_aliases.is_empty() {
            return ty.clone();
        }
        match ty {
            TypeRef::Named { name, args, span } => TypeRef::Named {
                name: self
                    .type_aliases
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| name.clone()),
                args: args.iter().map(|a| self.resolve_type_aliases(a)).collect(),
                span: *span,
            },
            TypeRef::Union { members, span } => TypeRef::Union {
                members: members
                    .iter()
                    .map(|m| self.resolve_type_aliases(m))
                    .collect(),
                span: *span,
            },
            TypeRef::Tuple { elements, span } => TypeRef::Tuple {
                elements: elements
                    .iter()
                    .map(|e| self.resolve_type_aliases(e))
                    .collect(),
                span: *span,
            },
            TypeRef::Fn { params, ret, span } => TypeRef::Fn {
                params: params
                    .iter()
                    .map(|p| self.resolve_type_aliases(p))
                    .collect(),
                ret: Box::new(self.resolve_type_aliases(ret)),
                span: *span,
            },
            TypeRef::Optional { inner, span } => TypeRef::Optional {
                inner: Box::new(self.resolve_type_aliases(inner)),
                span: *span,
            },
            // A trait object's trait name is not an import alias — narrowing never targets it.
            TypeRef::DynTrait { .. } => ty.clone(),
        }
    }

    /// Allocate a fresh frame-local temporary.
    fn fresh(&mut self) -> Temp {
        let t = Temp(self.temps);
        self.temps += 1;
        t
    }

    /// Lower a statement-position block of statements (no value), in the current frame.
    fn lower_body(&mut self, stmts: &[AstStmt]) -> Result<Block, Unsupported> {
        let mut out = Vec::new();
        for stmt in stmts {
            self.lower_stmt(stmt, &mut out)?;
        }
        Ok(Block::stmts(out))
    }

    fn lower_stmt(&mut self, stmt: &AstStmt, out: &mut Vec<Stmt>) -> Result<(), Unsupported> {
        match stmt {
            AstStmt::Echo { value, span } => {
                let atom = self.lower_expr(value, out)?;
                out.push(Stmt::Echo {
                    value: atom,
                    span: *span,
                });
                Ok(())
            }
            AstStmt::Binding {
                mut_decl,
                name,
                name_span,
                value,
                span,
                ..
            } => {
                // A `x.f = v` field-set is parsed as a reassignment of `x` whose value is an
                // `Expr::FieldSet`; flag it so the backends skip the immutable-reassignment check
                // (object-model slice 2b′ — the checker enforces the `struct` case statically).
                let field_assign = matches!(value, Expr::FieldSet { .. });
                let atom = self.lower_expr(value, out)?;
                out.push(Stmt::Bind {
                    mut_decl: *mut_decl,
                    name: name.clone(),
                    name_span: *name_span,
                    value: atom,
                    field_assign,
                    span: *span,
                });
                Ok(())
            }
            AstStmt::Expr { expr, .. } => {
                // Evaluate for effect; the result atom is discarded. If it landed in a temp,
                // release that temp here so its reference does not outlive the statement (the
                // tree-walker drops the corresponding intermediate at the same point).
                let atom = self.lower_expr(expr, out)?;
                if let Atom::Temp(t) = atom {
                    out.push(Stmt::Drop(t));
                }
                Ok(())
            }
            AstStmt::Return { value, span } => {
                let atom = match value {
                    Some(expr) => Some(self.lower_expr(expr, out)?),
                    None => None,
                };
                out.push(Stmt::Return {
                    value: atom,
                    span: *span,
                });
                Ok(())
            }
            // `yield` (Track G) is desugared into the generator state machine in a dedicated pass
            // (Track G.1b). Until that lands, every generator is gated as a checker error (E0039 "not
            // yet executable"), so a `yield` never reaches a *run* path through a clean program. To
            // keep lowering **total** (the `lower(...).expect(...)` invariant the eval backend and the
            // determinism property test rely on — both lower regardless of diagnostics), the interim
            // lowering evaluates the operand for effect and discards it, like an expression statement.
            // Replaced by the real state-machine desugar in G.1b.
            AstStmt::Yield { value, .. } => {
                let atom = self.lower_expr(value, out)?;
                if let Atom::Temp(t) = atom {
                    out.push(Stmt::Drop(t));
                }
                Ok(())
            }
            AstStmt::If {
                cond,
                then_body,
                else_body,
                span,
            } => {
                let cond = self.lower_expr(cond, out)?;
                let then_block = self.lower_body(then_body)?;
                let else_block = match else_body {
                    Some(body) => Some(self.lower_body(body)?),
                    None => None,
                };
                out.push(Stmt::If {
                    cond,
                    then_block,
                    else_block,
                    span: *span,
                });
                Ok(())
            }
            AstStmt::While { cond, body, span } => {
                let cond = self.lower_value_block(cond)?;
                let body = self.lower_body(body)?;
                out.push(Stmt::While {
                    cond,
                    body,
                    span: *span,
                });
                Ok(())
            }
            AstStmt::For {
                pattern,
                iterable,
                body,
                span,
            } => {
                let stream = self.sites.for_stream_sites.contains(span);
                let iterable = self.lower_expr(iterable, out)?;
                match pattern {
                    AstForPattern::Single { .. } => {
                        let body = self.lower_body(body)?;
                        out.push(Stmt::For {
                            pattern: pattern.clone(),
                            iterable,
                            body,
                            span: *span,
                            stream,
                        });
                    }
                    // A tuple destructure `for (a, b, …) in …` (object-model slice 4b) desugars to a
                    // single hidden element var plus per-position `.N` projections at the top of the
                    // body — so the IR for-loop only ever carries a `Single` pattern and reuses the
                    // existing `TupleIndex` machinery (no new runtime op).
                    AstForPattern::Tuple { names, .. } => {
                        let elem = format!("$for{}", self.fresh().0);
                        let mut body_stmts = Vec::new();
                        self.destructure_into(&elem, names, &mut body_stmts);
                        for s in body {
                            self.lower_stmt(s, &mut body_stmts)?;
                        }
                        out.push(Stmt::For {
                            pattern: AstForPattern::Single {
                                name: elem,
                                name_span: *span,
                            },
                            iterable,
                            body: Block::stmts(body_stmts),
                            span: *span,
                            stream,
                        });
                    }
                }
                Ok(())
            }
            AstStmt::Destructure {
                mut_decl,
                targets,
                value,
                span,
            } => {
                // Evaluate the value once, bind it to a hidden holder var (so its lifetime spans all
                // projections — a bare temp would be consumed by the first read), then bind each
                // target to its tuple position. Object-model slice 4b.
                let value_atom = self.lower_expr(value, out)?;
                let holder = format!("$destr{}", self.fresh().0);
                out.push(Stmt::Bind {
                    mut_decl: false,
                    name: holder.clone(),
                    name_span: *span,
                    value: value_atom,
                    field_assign: false,
                    span: *span,
                });
                for (i, (name, name_span)) in targets.iter().enumerate() {
                    let proj = self.emit(
                        out,
                        Rvalue::TupleIndex {
                            receiver: Atom::Var {
                                name: holder.clone(),
                                span: *name_span,
                            },
                            index: i as u32,
                            span: *name_span,
                        },
                        *name_span,
                    );
                    out.push(Stmt::Bind {
                        mut_decl: *mut_decl,
                        name: name.clone(),
                        name_span: *name_span,
                        value: proj,
                        field_assign: false,
                        span: *name_span,
                    });
                }
                Ok(())
            }
            AstStmt::Break { span } => {
                out.push(Stmt::Break { span: *span });
                Ok(())
            }
            AstStmt::Continue { span } => {
                out.push(Stmt::Continue { span: *span });
                Ok(())
            }
            AstStmt::Fn(decl) => {
                let func = self.lower_func(
                    &decl.params,
                    BodyKind::Block(&decl.body),
                    decl.span,
                    true,
                    decl.is_async,
                    Some(decl.name.clone()),
                )?;
                out.push(Stmt::Decl(Decl::Fn {
                    name: decl.name.clone(),
                    func: Rc::new(func),
                    span: decl.span,
                }));
                Ok(())
            }
            AstStmt::Class(decl) => {
                let mut methods = Vec::with_capacity(decl.methods.len());
                for m in &decl.methods {
                    let func = self.lower_func(
                        &m.params,
                        BodyKind::Block(&m.body),
                        m.span,
                        true,
                        m.is_async,
                        // Methods trace as `Type.method` (the VM's chunk naming).
                        Some(format!("{}.{}", decl.name, m.name)),
                    )?;
                    methods.push((m.name.clone(), Rc::new(func)));
                }
                // The `destruct` block lowers to a parameterless block [`Func`] (fields resolve
                // against the receiver, like a method), so the VM can compile it to a prototype.
                let destructor = match &decl.destructor {
                    Some(body) => Some(Rc::new(self.lower_func(
                        &[],
                        BodyKind::Block(body),
                        decl.span,
                        false,
                        false,
                        // The VM's destructor-prototype naming.
                        Some(format!("{}::destruct", decl.name)),
                    )?)),
                    None => None,
                };
                let field_defaults = self.lower_field_defaults(&decl.fields)?;
                out.push(Stmt::Decl(Decl::Class(ClassDef {
                    decl: Rc::new(decl.clone()),
                    methods,
                    field_defaults,
                    destructor,
                    span: decl.span,
                })));
                Ok(())
            }
            AstStmt::Enum(decl) => {
                // An enum carries inherent methods and `impl`-block methods (the unified body,
                // object-model slice 3), lowered to IR funcs exactly like a struct's. Variant/derive
                // data stays on the surface `decl`.
                let mut methods = Vec::with_capacity(decl.methods.len());
                for m in &decl.methods {
                    let func = self.lower_func(
                        &m.params,
                        BodyKind::Block(&m.body),
                        m.span,
                        true,
                        m.is_async,
                        // Methods trace as `Type.method` (the VM's chunk naming).
                        Some(format!("{}.{}", decl.name, m.name)),
                    )?;
                    methods.push((m.name.clone(), Rc::new(func)));
                }
                out.push(Stmt::Decl(Decl::Enum(EnumDef {
                    decl: Rc::new(decl.clone()),
                    methods,
                    span: decl.span,
                })));
                Ok(())
            }
            AstStmt::Struct(decl) => {
                // A struct carries inherent methods and `impl`-block methods (the unified body),
                // lowered to IR funcs exactly like a class's — minus any `destruct` (structs have
                // none). Field/derive data stays on the surface `decl`.
                let mut methods = Vec::with_capacity(decl.methods.len());
                for m in &decl.methods {
                    let func = self.lower_func(
                        &m.params,
                        BodyKind::Block(&m.body),
                        m.span,
                        true,
                        m.is_async,
                        // Methods trace as `Type.method` (the VM's chunk naming).
                        Some(format!("{}.{}", decl.name, m.name)),
                    )?;
                    methods.push((m.name.clone(), Rc::new(func)));
                }
                let field_defaults = self.lower_field_defaults(&decl.fields)?;
                out.push(Stmt::Decl(Decl::Struct(StructDef {
                    decl: Rc::new(decl.clone()),
                    methods,
                    field_defaults,
                    span: decl.span,
                })));
                Ok(())
            }
            AstStmt::Use { path, names, span } => {
                out.push(Stmt::Decl(Decl::Use {
                    path: path.clone(),
                    names: names.clone(),
                    span: *span,
                }));
                Ok(())
            }
            // A standalone `impl` and a `namespace` have no runtime effect in the tree-walker
            // (both are `Ok(Flow::Normal)` no-ops), so they lower to nothing.
            // A `trait` declaration (L1) has no runtime footprint of its own — its methods reach the
            // backends only as flattened impls on concrete types (UT2).
            AstStmt::Impl(_) | AstStmt::Trait(_) | AstStmt::Namespace { .. } => Ok(()),
            // `concurrent { }` (Track A.3b) lowers to a scope-bracketed body: `ScopeBegin`, the body
            // statements (their `spawn`s register tasks; their `.await`s drive the scope), then
            // `ScopeEnd` (which joins every remaining task). The body runs inline in the enclosing
            // frame — the scope is a runtime stack, not a new callable.
            AstStmt::Concurrent { body, span } => {
                out.push(Stmt::ScopeBegin { span: *span });
                for s in body {
                    self.lower_stmt(s, out)?;
                }
                out.push(Stmt::ScopeEnd { span: *span });
                Ok(())
            }
            // A dev-tier block reaching lowering is an *inactive* residual (object-model slice 6):
            // the tier-strip pass already spliced any *active* block's items into the statement
            // stream and dropped the inactive ones, so an inactive block lowers to nothing (stripped
            // from the build, identically on both backends since both lower the same program).
            AstStmt::TierBlock { .. } => Ok(()),
        }
    }

    /// Lower a function/method/closure into an IR [`Func`] with its own temporary frame. The
    /// enclosing frame's temporary counter is saved and restored, so a nested function (a
    /// closure inside a function body) is numbered independently and the outer numbering
    /// continues afterward.
    fn lower_func(
        &mut self,
        params: &[Param],
        body: BodyKind<'_>,
        span: Span,
        generator: bool,
        is_async: bool,
        name: Option<String>,
    ) -> Result<Func, Unsupported> {
        let outer = self.temps;
        self.temps = 0;
        let param_names = params.iter().map(|p| p.name.clone()).collect();
        // Defaults are evaluated in the captured scope at call time, each in its own frame, so
        // lower each as a self-contained thunk (this also restores `self.temps` to 0 between
        // thunks, keeping the body's numbering independent).
        let mut defaults = Vec::with_capacity(params.len());
        for p in params {
            match &p.default {
                Some(expr) => defaults.push(Some(self.lower_thunk(expr)?)),
                None => defaults.push(None),
            }
        }
        let body = match body {
            BodyKind::Arrow(expr) => self.lower_value_block(expr)?,
            // A generator (a function whose body contains `yield`, Track G) lowers to a state-machine
            // step closure wrapped in `make_gen` — not the body's statements directly. `generator` is
            // set only at the call sites where a generator is legal (named `fn`/methods), never for a
            // closure or the synthesized step closure itself, so the desugar applies exactly once.
            BodyKind::Block(stmts) if generator && body_has_yield(stmts) => {
                // The desugar's synthesized step closure executes this function's body, so it
                // traces under this function's name (see `synth_step_name`).
                self.synth_step_name = name.clone();
                self.lower_generator(stmts, span)?
            }
            // An `async fn` (Track A) lowers to a lazy `Future` over its body — `make_future(thunk)` —
            // not the body's statements directly (like a generator, but a single deferred computation
            // rather than a per-element state machine). `is_async` is set only at named-`fn`/method
            // sites, never a closure or the synthesized thunk, so the wrap applies exactly once.
            BodyKind::Block(stmts) if is_async => {
                // As for a generator: the future's step closure is this function's body.
                self.synth_step_name = name.clone();
                self.lower_async(stmts, span)?
            }
            BodyKind::Block(stmts) => self.lower_body(stmts)?,
        };
        let temp_count = self.temps;
        self.temps = outer;
        Ok(Func {
            name,
            params: param_names,
            defaults,
            body,
            temp_count,
            span,
        })
    }

    /// Lower a **generator** body (Track G.1b) — a function whose top-level statements include
    /// `yield` — into the state-machine representation: a step closure (the desugared dispatch) wrapped
    /// in [`Rvalue::MakeGen`]. The body becomes
    ///
    /// ```text
    /// mut $state = 0                    // dispatch discriminant
    /// mut <local> = none               // every top-level local, hoisted to a cell
    /// ...
    /// let $step = ($resume) => { <dispatch> }   // captures $state + the hoisted cells
    /// return make_gen($step)
    /// ```
    ///
    /// where the dispatch is an if-chain over `$state`: state *k* runs segment *k* (the statements up
    /// to the *k*-th top-level `yield`), advances `$state`, and `return some(<yielded>)`; the final
    /// segment runs the trailing statements and `return none`. The hoisted `mut` locals become
    /// captured cells (the original `let x = …` inside the closure reassigns the outer binding rather
    /// than declaring a closure-local — the language's bare-assignment rule), so a value computed in
    /// one segment survives into the next. Straight-line only: a `yield` nested in control flow is
    /// rejected by the checker (E0039, "not yet supported — Track G.2") and never reaches a *run*; if
    /// one survives in a check-failed program it stays a `Stmt::Yield` inside a segment and lowers
    /// through the interim discard arm, keeping lowering total.
    fn lower_generator(&mut self, stmts: &[AstStmt], span: Span) -> Result<Block, Unsupported> {
        let desugar =
            desugar_state_machine(stmts, span, self.sites.for_stream_sites, SuspendMode::Gen);
        let mut out = Vec::new();
        for stmt in &desugar.prelude {
            self.lower_stmt(stmt, &mut out)?;
        }
        let step = self.lower_expr(&desugar.step, &mut out)?;
        let generator = self.emit(&mut out, Rvalue::MakeGen { step, span }, span);
        out.push(Stmt::Return {
            value: Some(generator),
            span,
        });
        Ok(Block::stmts(out))
    }

    /// Lower an **async** function body (Track A.3) into a pollable [`Future`] state machine — the
    /// exact same stackless CFG desugar as a generator ([`desugar_state_machine`]), but polled instead
    /// of pulled. The body becomes a step closure wrapped in `make_future`:
    ///
    /// ```text
    /// mut $state = 0
    /// mut <hoisted cells> = none        // locals live across a suspend + the awaited-future cells
    /// ...
    /// let $step = ($resume) => { <dispatch> }
    /// return make_future($step)
    /// ```
    ///
    /// Each statement-position `.await` becomes a poll-state: poll the awaited future once; if ready,
    /// bind the value and advance; if pending, stay and `return $pending` so the caller re-polls here.
    /// `return e` completes the future with the raw `e` (so `?`'s injected error-return propagates
    /// unchanged); the driver/`.await` wraps completion vs pending. Unlike A.1's thunk, this can suspend
    /// mid-body and resume — the mechanism A.3b's concurrency needs to run a sibling while one task waits.
    fn lower_async(&mut self, stmts: &[AstStmt], span: Span) -> Result<Block, Unsupported> {
        let desugar =
            desugar_state_machine(stmts, span, self.sites.for_stream_sites, SuspendMode::Async);
        let mut out = Vec::new();
        for stmt in &desugar.prelude {
            self.lower_stmt(stmt, &mut out)?;
        }
        let step = self.lower_expr(&desugar.step, &mut out)?;
        let future = self.emit(&mut out, Rvalue::MakeFuture { thunk: step, span }, span);
        out.push(Stmt::Return {
            value: Some(future),
            span,
        });
        Ok(Block::stmts(out))
    }

    /// Lower each field carrying a default (`x: T = expr`) to a parameterless value [`Thunk`]
    /// (object-model slice 5), keyed by field name. A defaulted field's thunk is run in the type's
    /// definition scope at construction when a literal omits it — the same self-contained-thunk
    /// machinery as a defaulted parameter. A mandatory field contributes nothing.
    fn lower_field_defaults(
        &mut self,
        fields: &[noeta_ast::FieldDecl],
    ) -> Result<Vec<(String, Thunk)>, Unsupported> {
        let mut defaults = Vec::new();
        for f in fields {
            if let Some(expr) = &f.default {
                defaults.push((f.name.clone(), self.lower_thunk(expr)?));
            }
        }
        Ok(defaults)
    }

    /// Lower a defaulted-parameter expression into a self-contained value-producing [`Thunk`]
    /// with its own temporary frame (defaults run independently in the captured scope).
    fn lower_thunk(&mut self, expr: &Expr) -> Result<Thunk, Unsupported> {
        let outer = self.temps;
        self.temps = 0;
        let body = self.lower_value_block(expr)?;
        let temp_count = self.temps;
        self.temps = outer;
        Ok(Thunk { body, temp_count })
    }

    /// Lower an expression into a fresh value-position [`Block`] (its computed `let`s plus a
    /// tail atom). Used where an expression is re-evaluated or evaluated lazily — a `while`
    /// condition, a defaulted parameter — so it cannot be hoisted into the surrounding
    /// straight-line sequence.
    fn lower_value_block(&mut self, expr: &Expr) -> Result<Block, Unsupported> {
        let mut stmts = Vec::new();
        let atom = self.lower_expr(expr, &mut stmts)?;
        Ok(Block {
            stmts,
            tail: Some(atom),
        })
    }

    /// Lower an expression to an [`Atom`], emitting the `let`s that compute any
    /// sub-expressions into `out` first (A-normal form). Literals and identifiers reduce
    /// directly to an atom with no `let`.
    fn lower_expr(&mut self, expr: &Expr, out: &mut Vec<Stmt>) -> Result<Atom, Unsupported> {
        match expr {
            // A resolved native module-function reference (expr-tiers arc) → the first-class
            // module-function value, emitted as its own rvalue (the backend loads the const).
            Expr::NativeFnRef { module, func, span } => Ok(self.emit(
                out,
                Rvalue::ModuleFn {
                    module: module.clone(),
                    func: func.clone(),
                    span: *span,
                },
                *span,
            )),
            Expr::Str { value, .. } => Ok(Atom::Const(Const::Str(value.clone()))),
            Expr::Int { value, .. } => Ok(Atom::Const(Const::Int(*value))),
            // A fixed-width integer literal (Tier W) is **erased to an ordinary `int` const**: the
            // magnitude's bit pattern is the runtime i64 word (a `u64` with the high bit set boxes as
            // the corresponding negative i64 — the correct erased pattern). Width/signedness lived in
            // the type and have already been range-checked (E0044); nothing survives to runtime.
            Expr::IntN { magnitude, .. } => Ok(Atom::Const(Const::Int(*magnitude as i64))),
            // A bare float literal is `float` by default, but lowers to a narrow `f32` const where
            // the checker adapted it into an `f32` context (P-NUM-SYM). `as f32` round-to-nearest
            // matches the runtime narrowing; both backends share this lowering, so they agree.
            Expr::Float { value, span } if self.sites.f32_literal_sites.contains(span) => {
                Ok(Atom::Const(Const::F32(*value as f32)))
            }
            Expr::Float { value, .. } => Ok(Atom::Const(Const::Float(*value))),
            Expr::F32 { value, .. } => Ok(Atom::Const(Const::F32(*value))),
            // `f64` is bit-identical to `float`: lower to a plain 64-bit float constant.
            Expr::F64 { value, .. } => Ok(Atom::Const(Const::Float(*value))),
            Expr::Bool { value, .. } => Ok(Atom::Const(Const::Bool(*value))),
            // The async desugar's pending sentinel (`$pending`, Track A.3) — a synthetic name (the
            // lexer forbids `$`, so it can never collide with a source identifier) the state machine
            // returns to signal it suspended at an `.await`. Lowers to the dedicated rvalue.
            Expr::Ident { name, span } if name == PENDING_IDENT => {
                Ok(self.emit(out, Rvalue::Pending { span: *span }, *span))
            }
            Expr::Ident { name, span } => Ok(Atom::Var {
                name: name.clone(),
                span: *span,
            }),
            Expr::Unary { op, operand, span } => {
                let operand = self.lower_expr(operand, out)?;
                let result = self.emit(
                    out,
                    Rvalue::Unary {
                        op: *op,
                        operand,
                        span: *span,
                    },
                    *span,
                );
                Ok(self.mask_if_width(out, result, *span))
            }
            Expr::Binary { op, lhs, rhs, span } if matches!(op, BinaryOp::And | BinaryOp::Or) => {
                self.lower_logical(*op, lhs, rhs, *span, out)
            }
            Expr::Binary { op, lhs, rhs, span } => {
                let lhs = self.lower_expr(lhs, out)?;
                let rhs = self.lower_expr(rhs, out)?;
                // Sign-dependent fixed-width op (Tier W3/W5): `/ % < <= > >=` and `>>` on `IntN`,
                // whose operand width the checker recorded in `width_sites`. Emit the width-carrying
                // `WideInt` (it masks div/rem itself, yields a bool for comparisons, and shifts
                // arithmetically/logically per signedness for `>>`) rather than a plain `Binary`. The
                // sign-agnostic `+ - *` and `<<` fall through to `Binary` + a `MaskWidth`; `& | ^`
                // (no `width_sites` entry) stay a plain `Binary`.
                if let Some(&(signed, bits)) = self.sites.width_sites.get(span)
                    && matches!(
                        op,
                        BinaryOp::Div
                            | BinaryOp::Rem
                            | BinaryOp::Lt
                            | BinaryOp::Le
                            | BinaryOp::Gt
                            | BinaryOp::Ge
                            | BinaryOp::Shr
                    )
                {
                    return Ok(self.emit(
                        out,
                        Rvalue::WideInt {
                            op: *op,
                            lhs,
                            rhs,
                            signed,
                            bits,
                            span: *span,
                        },
                        *span,
                    ));
                }
                let result = self.emit(
                    out,
                    Rvalue::Binary {
                        op: *op,
                        lhs,
                        rhs,
                        reuse: false,
                        span: *span,
                    },
                    *span,
                );
                Ok(self.mask_if_width(out, result, *span))
            }
            Expr::List { items, span } => {
                // A `List<@packed struct>` literal (its span recorded by the checker) streams into a
                // flat buffer: allocate, then build-and-push each element in turn so only one element
                // object is ever live (P-PACK 2.5). Any other list builds the boxed `Rvalue::List`,
                // materializing all element atoms first.
                if let Some(layout) = self.sites.packed_list_sites.get(span) {
                    let mut acc = self.emit(
                        out,
                        Rvalue::PackedListNew {
                            layout: layout.clone(),
                            span: *span,
                        },
                        *span,
                    );
                    for item in items {
                        let item_span = item.span();
                        let value = self.lower_expr(item, out)?;
                        acc = self.emit(
                            out,
                            Rvalue::PackedListPush {
                                list: acc,
                                value,
                                span: item_span,
                            },
                            item_span,
                        );
                    }
                    return Ok(acc);
                }
                let mut atoms = Vec::with_capacity(items.len());
                for item in items {
                    atoms.push(self.lower_expr(item, out)?);
                }
                Ok(self.emit(
                    out,
                    Rvalue::List {
                        items: atoms,
                        // The checker-resolved element type for this literal (R1), so `type_of` can
                        // recover it after the value is laundered through `dyn`. Empty on the
                        // boxed/REPL path (no construction-site map) → the list stays untagged.
                        reflect: self.sites.construction_sites.get(span).cloned(),
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Tuple { items, span } => {
                let mut atoms = Vec::with_capacity(items.len());
                for item in items {
                    atoms.push(self.lower_expr(item, out)?);
                }
                Ok(self.emit(
                    out,
                    Rvalue::Tuple {
                        items: atoms,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::TupleIndex {
                receiver,
                index,
                span,
            } => {
                let receiver = self.lower_expr(receiver, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::TupleIndex {
                        receiver,
                        index: *index,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Range {
                start,
                end,
                inclusive,
                span,
            } => {
                let start = self.lower_expr(start, out)?;
                let end = self.lower_expr(end, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Range {
                        start,
                        end,
                        inclusive: *inclusive,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Map { entries, span } => {
                let mut pairs = Vec::with_capacity(entries.len());
                for (k, v) in entries {
                    let key = self.lower_expr(k, out)?;
                    let value = self.lower_expr(v, out)?;
                    pairs.push((key, value));
                }
                Ok(self.emit(
                    out,
                    Rvalue::Map {
                        entries: pairs,
                        // The checker-resolved `Map(K, V)` type for this literal (R1); empty on the
                        // boxed/REPL path → the map stays untagged and reflects head-only.
                        reflect: self.sites.construction_sites.get(span).cloned(),
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Index {
                receiver,
                index,
                span,
            } => {
                let receiver = self.lower_expr(receiver, out)?;
                let index = self.lower_expr(index, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Index {
                        receiver,
                        index,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Interp { parts, span } => {
                let mut ir_parts = Vec::with_capacity(parts.len());
                for part in parts {
                    match part {
                        StrPart::Literal(text) => ir_parts.push(InterpPart::Literal(text.clone())),
                        StrPart::Hole(e) => {
                            let atom = self.lower_expr(e, out)?;
                            ir_parts.push(InterpPart::Hole {
                                atom,
                                span: e.span(),
                            });
                        }
                    }
                }
                Ok(self.emit(
                    out,
                    Rvalue::Interp {
                        parts: ir_parts,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Call { callee, args, span } => {
                // The async desugar's single-poll primitive (`$poll(future)`, Track A.3) — a synthetic
                // name (lexer-forbidden `$`, no source collision) the state machine emits at each
                // `.await`. Lowers to the dedicated poll rvalue (`some(v)`/`none`).
                if let Expr::Ident { name, .. } = callee.as_ref()
                    && name == POLL_FN
                    && let [arg] = args.as_slice()
                {
                    let future = self.lower_expr(arg, out)?;
                    return Ok(self.emit(
                        out,
                        Rvalue::PollFuture {
                            future,
                            span: *span,
                        },
                        *span,
                    ));
                }
                // A call whose callee is a member access is a method call; otherwise an
                // ordinary call. Evaluation order matches the tree-walker's `eval_call`:
                // receiver/callee first, then arguments left-to-right.
                if let Expr::Member {
                    receiver,
                    name,
                    name_span,
                    ..
                } = callee.as_ref()
                {
                    // Router-facing runtime decode `json.decode_typed(name, text)` (L2.2 DI): the
                    // checker recorded this call span. Emit the dedicated op over the two argument
                    // atoms — the receiver (the `json` module handle) is not a runtime value, so it is
                    // not lowered.
                    if self.sites.decode_typed_sites.contains(span) && name == "decode_typed" {
                        let mut arg_atoms = self.lower_args(args, out)?;
                        let text = arg_atoms.pop().expect("decode_typed takes 2 args");
                        let name = arg_atoms.pop().expect("decode_typed takes 2 args");
                        return Ok(self.emit(
                            out,
                            Rvalue::DecodeTyped {
                                name,
                                text,
                                span: *span,
                            },
                            *span,
                        ));
                    }
                    let receiver = self.lower_expr(receiver, out)?;
                    let arg_atoms = self.lower_args(args, out)?;
                    // Width-exact bit intrinsic on a fixed-width receiver (Tier W5): the checker marked
                    // this call span in `width_sites`. Emit the width-carrying `WidthIntMethod` so both
                    // backends compute within the width via `int_method_width`, rather than the generic
                    // `Method` (which would compute on the full erased i64). A `Convert` (`to_*`) is not
                    // width-relative — it stays an ordinary method.
                    if let Some(&(_, bits)) = self.sites.width_sites.get(span)
                        && let Some(method) = noeta_ext_abi::IntMethod::from_name(name)
                        && !matches!(method, noeta_ext_abi::IntMethod::Convert { .. })
                    {
                        return Ok(self.emit(
                            out,
                            Rvalue::WidthIntMethod {
                                receiver,
                                method,
                                args: arg_atoms,
                                bits,
                                span: *span,
                            },
                            *span,
                        ));
                    }
                    // A method-bundle call (kernel-methods K2): the checker resolved this call
                    // span to a bound bundle — bake the route in.
                    if let Some((module, bundle)) = self.sites.bundle_call_sites.get(span) {
                        return Ok(self.emit(
                            out,
                            Rvalue::BundleMethod {
                                receiver,
                                module: module.clone(),
                                bundle: bundle.clone(),
                                name: name.clone(),
                                args: arg_atoms,
                                span: *span,
                            },
                            *span,
                        ));
                    }
                    Ok(self.emit(
                        out,
                        Rvalue::Method {
                            receiver,
                            name: name.clone(),
                            name_span: *name_span,
                            args: arg_atoms,
                            reuse: false,
                            // Generic enum-variant construction records its type here (R2b.2); an
                            // ordinary method-call span is not a construction site.
                            reflect: self.sites.construction_sites.get(span).cloned(),
                            span: *span,
                        },
                        *span,
                    ))
                } else {
                    let callee = self.lower_expr(callee, out)?;
                    let arg_atoms = self.lower_args(args, out)?;
                    Ok(self.emit(
                        out,
                        Rvalue::Call {
                            callee,
                            args: arg_atoms,
                            span: *span,
                        },
                        *span,
                    ))
                }
            }
            Expr::Member {
                receiver,
                name,
                name_span,
                span,
            } => {
                // A namespace-group member access (`http.client`) the checker resolved to a concrete
                // native module → materialize the module value from its **root-qualified leaf
                // identity** (`std.http.client`), exactly as a direct `use std.http.client` binding
                // would. The receiver (the group handle) is not itself a value, so it is not lowered.
                if let Some(module) = self.sites.namespace_module_sites.get(span) {
                    return Ok(self.emit(
                        out,
                        Rvalue::NativeModule {
                            module: module.clone(),
                            span: *span,
                        },
                        *span,
                    ));
                }
                // A `list[i].field` read the checker proved fusable (its index receiver is a built-in
                // `List`) lowers to one [`Rvalue::IndexField`] over the list and index atoms, so a
                // packed element's field is read without materializing the element (P-PACK 2.5+). Any
                // other member access lowers to the ordinary field load.
                // `Type.method` in value position (the checker resolved a type receiver + a method) →
                // an unbound method handle, not a field load. The receiver is a static type name, so
                // no receiver atom is lowered.
                if let Some((ty, method, associated)) = self.sites.handle_sites.get(span) {
                    return Ok(self.emit(
                        out,
                        Rvalue::MethodHandle {
                            ty: ty.clone(),
                            method: method.clone(),
                            associated: *associated,
                            span: *span,
                        },
                        *span,
                    ));
                }
                // `value.method` in value position (EX.2b) → a bound handle capturing the receiver.
                if self.sites.bound_handle_sites.contains(span) {
                    let recv = self.lower_expr(receiver, out)?;
                    return Ok(self.emit(
                        out,
                        Rvalue::BoundHandle {
                            recv,
                            method: name.clone(),
                            span: *span,
                        },
                        *span,
                    ));
                }
                if self.sites.index_field_sites.contains(span)
                    && let Expr::Index {
                        receiver: list,
                        index,
                        ..
                    } = receiver.as_ref()
                {
                    let list = self.lower_expr(list, out)?;
                    let index = self.lower_expr(index, out)?;
                    return Ok(self.emit(
                        out,
                        Rvalue::IndexField {
                            receiver: list,
                            index,
                            field: name.clone(),
                            field_span: *name_span,
                            span: *span,
                        },
                        *span,
                    ));
                }
                let receiver = self.lower_expr(receiver, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Field {
                        receiver,
                        name: name.clone(),
                        name_span: *name_span,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::FieldSet {
                receiver,
                field,
                field_span,
                value,
                span,
            } => {
                // Lower receiver then value (left-to-right), matching the tree-walker's order.
                let receiver = self.lower_expr(receiver, out)?;
                let value = self.lower_expr(value, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::SetField {
                        receiver,
                        name: field.clone(),
                        name_span: *field_span,
                        value,
                        reuse: false,
                        span: *span,
                    },
                    *span,
                ))
            }
            // An expression-tier block lowers as the handler call it means — the same
            // [`noeta_ast::desugar::tier_expr_call`] construction the checker typed, so the two
            // can never drift. A tier with no expr declaration is checker-rejected (E0052), but
            // lowering is total over the parsed language: an unchecked pipeline gets a
            // deterministic runtime panic, exactly like other checker-gated constructs.
            Expr::TierExpr {
                tier,
                tier_span,
                statics,
                holes,
                span,
            } => {
                // Resolve the handler: a native (extension) tier's module function first (the
                // registry is authoritative and a built-in name is not shadowable), else a
                // program `@tier` fn. Both build the identical `Call`, differing only in the
                // callee — the native one a resolved `NativeFnRef`, no user import needed.
                let handler = self
                    .registry
                    .find_ext_tier(tier)
                    .filter(|t| t.expr.is_some())
                    .and_then(|t| t.handler)
                    .map(noeta_ast::desugar::ExprTierHandler::from_native_path)
                    .or_else(|| {
                        self.expr_tiers
                            .get(tier)
                            .cloned()
                            .map(noeta_ast::desugar::ExprTierHandler::Program)
                    });
                let call = match handler {
                    Some(handler) => noeta_ast::desugar::tier_expr_call(
                        &handler, *tier_span, statics, holes, *span,
                    ),
                    None => Expr::Call {
                        callee: Box::new(Expr::Ident {
                            name: "panic".to_string(),
                            span: *tier_span,
                        }),
                        args: vec![Expr::Str {
                            value: format!(
                                "`@{tier}` is not an expression tier — its blocks are not values"
                            ),
                            span: *span,
                        }],
                        span: *span,
                    },
                };
                self.lower_expr(&call, out)
            }
            Expr::Match {
                scrutinee,
                arms,
                span,
            } => {
                let scrut = self.lower_expr(scrutinee, out)?;
                let mut ir_arms = Vec::with_capacity(arms.len());
                for arm in arms {
                    let body = self.lower_value_block(&arm.body)?;
                    ir_arms.push(crate::Arm {
                        pattern: arm.pattern.clone(),
                        body,
                        span: arm.span,
                    });
                }
                let dst = self.fresh();
                out.push(Stmt::Match {
                    scrutinee: scrut,
                    arms: ir_arms,
                    dst: Some(dst),
                    span: *span,
                });
                Ok(Atom::Temp(dst))
            }
            Expr::Try { expr, span } => {
                let operand = self.lower_expr(expr, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Try {
                        operand,
                        // Filled by the drop-insertion pass; lowering emits none.
                        on_error: Vec::new(),
                        span: *span,
                    },
                    *span,
                ))
            }
            // `.await` — the **drive-to-completion** path. Statement-position awaits *inside an async
            // fn body* are rewritten by `desugar_state_machine` into `$poll` poll-states before they
            // reach here, so this arm handles the awaits that are *not* part of a caller's state
            // machine: the implicit-async top level and any inline drive context. `run_future` lowers
            // to `drive_future` (both backends), which polls the future — advancing the executor clock
            // on a `Pending` timer/read leaf and driving any open `concurrent` scope's sibling tasks a
            // round each iteration — until it is ready. (Tracks A.1→A.4.)
            Expr::Await { expr, span } => {
                let future = self.lower_expr(expr, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::RunFuture {
                        future,
                        span: *span,
                    },
                    *span,
                ))
            }
            // `spawn e` (Track A.3b): register the future `e` as a task in the current scope and yield
            // a handle (itself a `Future<T>`). The future operand is evaluated first (an `async fn`
            // call producing the lazy state machine), then handed to `Rvalue::Spawn`.
            // `isolate f(args)` (isolates I.4b) lowers to `Rvalue::SpawnIsolate`, carrying the callee and
            // arguments **unbuilt** so a real-thread isolate can copy-marshal the args (a pre-built
            // future captures them in the parent heap and cannot cross a thread). The checker restricts
            // `isolate` to a direct call (E0042), so the future is an `Expr::Call`; if for any reason it
            // is not, fall back to the plain `Spawn` path. In the deterministic sandbox `SpawnIsolate`
            // behaves exactly like `spawn f(args)`, so both backends and the differential are unchanged.
            Expr::Spawn {
                future,
                isolate: true,
                span,
            } if self.real_isolates && matches!(future.as_ref(), Expr::Call { .. }) => {
                let Expr::Call { callee, args, .. } = future.as_ref() else {
                    unreachable!("guarded by the match arm");
                };
                let callee = self.lower_expr(callee, out)?;
                let arg_atoms = self.lower_args(args, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::SpawnIsolate {
                        callee,
                        args: arg_atoms,
                        span: *span,
                    },
                    *span,
                ))
            }
            // `spawn e` (Track A.3b) — and any `isolate` that is not a direct call (defensive; the
            // checker rejects that with E0042) — register the pre-built future `e` as a task.
            Expr::Spawn { future, span, .. } => {
                let future = self.lower_expr(future, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Spawn {
                        future,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Coalesce {
                value,
                fallback,
                span,
            } => {
                let value = self.lower_expr(value, out)?;
                let fallback = self.lower_value_block(fallback)?;
                let dst = self.fresh();
                out.push(Stmt::Coalesce {
                    dst: Some(dst),
                    value,
                    fallback,
                    span: *span,
                });
                Ok(Atom::Temp(dst))
            }
            Expr::As { expr, ty, span } => {
                let operand = self.lower_expr(expr, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::As {
                        operand,
                        ty: self.resolve_type_aliases(ty),
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::TypeTest { expr, ty, span } => {
                let operand = self.lower_expr(expr, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::TypeTest {
                        operand,
                        ty: self.resolve_type_aliases(ty),
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::TypeOf { value, span } => {
                let operand = self.lower_expr(value, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::TypeOf {
                        operand,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::FromBytes { blob, span, .. } => {
                let blob = self.lower_expr(blob, out)?;
                // The element layout was recorded by the checker at this span in the same channel
                // list literals use (`packed_list_sites`); `None` means T was not packable (already
                // a checker error), and the backend then fails cleanly rather than mis-decoding.
                let layout = self.sites.packed_list_sites.get(span).cloned();
                Ok(self.emit(
                    out,
                    Rvalue::FromBytes {
                        blob,
                        layout,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::TypedModuleCall {
                recv,
                func,
                args,
                span,
                ..
            } => {
                let module = match recv.as_ref() {
                    Expr::Ident { name, .. } => name.clone(),
                    _ => String::new(),
                };
                let args = args
                    .iter()
                    .map(|a| self.lower_expr(a, out))
                    .collect::<Result<Vec<_>, _>>()?;
                // The recipe was resolved by the checker at this span (the same channel the other
                // typed sites use); `None` means `T` had no decoding (already a checker error).
                let recipe = self.sites.typed_module_call_sites.get(span).cloned();
                Ok(self.emit(
                    out,
                    Rvalue::TypedModuleCall {
                        module,
                        func: func.clone(),
                        args,
                        recipe,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Channel { capacity, span, .. } => {
                // The message type `T` is checker-only; only the buffer size reaches the runtime.
                let capacity = self.lower_expr(capacity, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::MakeChannel {
                        capacity,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::AttributesOf { ty, span } => Ok(self.emit(
                out,
                Rvalue::AttributesOf {
                    ty: ty.clone(),
                    span: *span,
                },
                *span,
            )),
            Expr::RolesOf { ty, span } => Ok(self.emit(
                out,
                Rvalue::RolesOf {
                    ty: ty.clone(),
                    span: *span,
                },
                *span,
            )),
            Expr::ParamsOf { target, span } => {
                let target = self.lower_expr(target, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::ParamsOf {
                        target,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Invoke {
                recv,
                name,
                args,
                span,
            } => {
                let recv = self.lower_expr(recv, out)?;
                let name = self.lower_expr(name, out)?;
                let args = self.lower_expr(args, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Invoke {
                        recv,
                        name,
                        args,
                        span: *span,
                    },
                    *span,
                ))
            }
            // The return annotation is runtime-erased (the checker has already used it); lowering
            // ignores it, exactly as it ignores parameter type annotations. An arrow body lowers like
            // a value-returning expression; a block body lowers exactly like a named function's body.
            Expr::Closure {
                params, body, span, ..
            } => {
                let body_kind = match body {
                    noeta_ast::ClosureBody::Expr(e) => BodyKind::Arrow(e),
                    noeta_ast::ClosureBody::Block(stmts) => BodyKind::Block(stmts),
                };
                // A closure is never a generator or an async fn: `yield`/`.await` reset at a callable
                // boundary (the checker rejects them inside a closure), and the generator/async
                // desugar's own thunk must not be re-desugared. So both flags are `false` here.
                // The name is the armed synthesized-step name if this closure IS such a step (taken,
                // so anything nested inside the step's body finds `None`); a user closure is anonymous.
                let name = self.synth_step_name.take();
                let func = self.lower_func(params, body_kind, *span, false, false, name)?;
                Ok(self.emit(
                    out,
                    Rvalue::Closure {
                        func: Rc::new(func),
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Object(lit) => {
                // Evaluation order matches `eval_object`: the `..` spread first, then named
                // initializers left-to-right.
                let spread = match &lit.spread {
                    Some(s) => Some((self.lower_expr(s, out)?, s.span())),
                    None => None,
                };
                let mut fields = Vec::with_capacity(lit.fields.len());
                for init in &lit.fields {
                    let value = self.lower_expr(&init.value, out)?;
                    fields.push(crate::ObjectFieldInit {
                        name: init.name.clone(),
                        name_span: init.name_span,
                        value,
                    });
                }
                Ok(self.emit(
                    out,
                    Rvalue::Object {
                        type_name: lit.type_name.clone(),
                        type_name_span: lit.type_name_span,
                        fields,
                        spread,
                        // The reuse-analysis pass (Phase 5) sets this when it recognizes a self-update;
                        // lowering is reuse-neutral.
                        reuse: false,
                        // The checker-resolved reflected type (R2) for a generic instantiation; `None`
                        // for a non-generic type or the boxed path → the value reflects head-only.
                        reflect: self.sites.construction_sites.get(&lit.span).cloned(),
                        span: lit.span,
                    },
                    lit.span,
                ))
            }
            Expr::Pipeline { left, right, span } => self.lower_pipeline(left, right, *span, out),
        }
    }

    /// Lower `left |> right`, desugaring to a call/method with `left` threaded as the leading
    /// argument — mirroring `eval_pipeline`. `left` is evaluated first (matching the
    /// tree-walker), then the callee/receiver, then any remaining arguments.
    fn lower_pipeline(
        &mut self,
        left: &Expr,
        right: &Expr,
        span: Span,
        out: &mut Vec<Stmt>,
    ) -> Result<Atom, Unsupported> {
        let left_atom = self.lower_expr(left, out)?;
        match right {
            // `x |> f(a)` ⟶ `f(x, a)`; `x |> obj.m(a)` ⟶ `obj.m(x, a)`.
            Expr::Call { callee, args, span } => {
                if let Expr::Member {
                    receiver,
                    name,
                    name_span,
                    ..
                } = callee.as_ref()
                {
                    // Router-facing runtime decode via a pipe (`name |> json.decode_typed(text)`,
                    // L2.2 DI): `left` threads in as the leading (`name`) argument. Emit the dedicated
                    // op; the receiver (the `json` module handle) is not a runtime value.
                    if self.sites.decode_typed_sites.contains(span) && name == "decode_typed" {
                        let mut arg_atoms = vec![left_atom];
                        for a in args {
                            arg_atoms.push(self.lower_expr(a, out)?);
                        }
                        let text = arg_atoms.pop().expect("decode_typed takes 2 args");
                        let name = arg_atoms.pop().expect("decode_typed takes 2 args");
                        return Ok(self.emit(
                            out,
                            Rvalue::DecodeTyped {
                                name,
                                text,
                                span: *span,
                            },
                            *span,
                        ));
                    }
                    let receiver = self.lower_expr(receiver, out)?;
                    let mut arg_atoms = vec![left_atom];
                    for a in args {
                        arg_atoms.push(self.lower_expr(a, out)?);
                    }
                    Ok(self.emit(
                        out,
                        Rvalue::Method {
                            receiver,
                            name: name.clone(),
                            name_span: *name_span,
                            args: arg_atoms,
                            reuse: false,
                            // Generic enum-variant construction records its type here (R2b.2); an
                            // ordinary method-call span is not a construction site.
                            reflect: self.sites.construction_sites.get(span).cloned(),
                            span: *span,
                        },
                        *span,
                    ))
                } else {
                    let callee = self.lower_expr(callee, out)?;
                    let mut arg_atoms = vec![left_atom];
                    for a in args {
                        arg_atoms.push(self.lower_expr(a, out)?);
                    }
                    Ok(self.emit(
                        out,
                        Rvalue::Call {
                            callee,
                            args: arg_atoms,
                            span: *span,
                        },
                        *span,
                    ))
                }
            }
            // `x |> obj.m` ⟶ `obj.m(x)`.
            Expr::Member {
                receiver,
                name,
                name_span,
                span,
            } => {
                let receiver = self.lower_expr(receiver, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Method {
                        receiver,
                        name: name.clone(),
                        name_span: *name_span,
                        args: vec![left_atom],
                        reuse: false,
                        reflect: self.sites.construction_sites.get(span).cloned(),
                        span: *span,
                    },
                    *span,
                ))
            }
            // `x |> f` ⟶ `f(x)`.
            _ => {
                let callee = self.lower_expr(right, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Call {
                        callee,
                        args: vec![left_atom],
                        span,
                    },
                    span,
                ))
            }
        }
    }

    /// Lower a call's argument list left-to-right (the tree-walker's order).
    fn lower_args(&mut self, args: &[Expr], out: &mut Vec<Stmt>) -> Result<Vec<Atom>, Unsupported> {
        let mut atoms = Vec::with_capacity(args.len());
        for arg in args {
            atoms.push(self.lower_expr(arg, out)?);
        }
        Ok(atoms)
    }

    /// Lower `a && b` / `a || b` to a [`Stmt::Logical`] writing into a fresh temp, so the
    /// right operand is evaluated lazily (a [`Block`]) rather than up-front.
    fn lower_logical(
        &mut self,
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
        span: Span,
        out: &mut Vec<Stmt>,
    ) -> Result<Atom, Unsupported> {
        let left = self.lower_expr(lhs, out)?;
        let mut right_stmts = Vec::new();
        let right_atom = self.lower_expr(rhs, &mut right_stmts)?;
        let dst = self.fresh();
        out.push(Stmt::Logical {
            dst: Some(dst),
            op,
            left,
            right: Block {
                stmts: right_stmts,
                tail: Some(right_atom),
            },
            span,
        });
        Ok(Atom::Temp(dst))
    }

    /// Emit `let t = rvalue` into `out` and return the new temp as an atom.
    fn emit(&mut self, out: &mut Vec<Stmt>, rvalue: Rvalue, span: Span) -> Atom {
        let dst = self.fresh();
        out.push(Stmt::Let { dst, rvalue, span });
        Atom::Temp(dst)
    }

    /// If `span` is a fixed-width arithmetic site (Tier W), wrap `value` in [`Rvalue::MaskWidth`] to
    /// reduce the erased i64 back into the declared width; otherwise return `value` untouched. Called
    /// on the result of a lowered `Expr::Binary`/`Expr::Unary`.
    fn mask_if_width(&mut self, out: &mut Vec<Stmt>, value: Atom, span: Span) -> Atom {
        match self.sites.width_sites.get(&span) {
            Some(&(signed, bits)) => self.emit(
                out,
                Rvalue::MaskWidth {
                    operand: value,
                    signed,
                    bits,
                    span,
                },
                span,
            ),
            None => value,
        }
    }

    /// Emit, into `out`, the per-position `.N` projections that destructure a tuple held by the
    /// variable `holder` into `names` — `name_i = holder.i` (object-model slice 4b). Shared by the
    /// for-loop tuple pattern's body prologue (the binding-statement destructure inlines its own,
    /// since it carries a `mut` flag).
    fn destructure_into(&mut self, holder: &str, names: &[(String, Span)], out: &mut Vec<Stmt>) {
        for (i, (name, name_span)) in names.iter().enumerate() {
            let proj = self.emit(
                out,
                Rvalue::TupleIndex {
                    receiver: Atom::Var {
                        name: holder.to_string(),
                        span: *name_span,
                    },
                    index: i as u32,
                    span: *name_span,
                },
                *name_span,
            );
            out.push(Stmt::Bind {
                mut_decl: false,
                name: name.clone(),
                name_span: *name_span,
                value: proj,
                field_assign: false,
                span: *name_span,
            });
        }
    }
}
