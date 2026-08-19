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
    Stmt as AstStmt, StrPart, TypeOperand, TypeRef,
};
use noeta_ext_abi::NominalType;
use noeta_span::Span;

use crate::{
    Atom, Block, ClassDef, Const, Decl, EnumDef, Func, InterpPart, Program, ReflectArgs, Rvalue,
    Stmt, StructDef, Temp, Thunk,
};

mod state_machine;
use state_machine::{
    PENDING_IDENT, POLL_FN, SCOPE_BEGIN_FN, SCOPE_END_FN, SCOPE_READY_FN, SuspendMode,
    body_has_yield, desugar_state_machine,
};

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

/// The checker's bare-payload-free-variant resolutions: a `match`-arm binding pattern's span → the
/// `(qualifier, variant)` it names. See [`LoweringSites::variant_pattern_sites`].
pub type VariantPatternSites = HashMap<Span, (Option<String>, String)>;

/// Rewrite the bare-identifier patterns the checker resolved to payload-free variants into ordinary
/// [`noeta_ast::Pattern::Variant`] tests, recursing so a nested one (`Ok(none)`, a tuple element, a
/// variant payload) resolves against the type it was actually matched on. Everything else is
/// returned verbatim: a binding the checker did not record stays a binding.
pub(crate) fn resolve_variant_patterns(
    pattern: &noeta_ast::Pattern,
    sites: &VariantPatternSites,
) -> noeta_ast::Pattern {
    use noeta_ast::Pattern;
    match pattern {
        Pattern::Binding { span, .. } => match sites.get(span) {
            Some((type_name, variant)) => Pattern::Variant {
                type_name: type_name.clone().map(noeta_ast::Name::canonical),
                variant: variant.clone(),
                bindings: Vec::new(),
                span: *span,
            },
            None => pattern.clone(),
        },
        Pattern::Variant {
            type_name,
            variant,
            bindings,
            span,
        } => Pattern::Variant {
            type_name: type_name.clone(),
            variant: variant.clone(),
            bindings: bindings
                .iter()
                .map(|b| resolve_variant_patterns(b, sites))
                .collect(),
            span: *span,
        },
        Pattern::Tuple { elements, span } => Pattern::Tuple {
            elements: elements
                .iter()
                .map(|e| resolve_variant_patterns(e, sites))
                .collect(),
            span: *span,
        },
        // Leaves — no sub-pattern can carry a resolution. Spelled out rather than wildcarded so a
        // new pattern form has to decide here.
        Pattern::Wildcard { .. }
        | Pattern::Int { .. }
        | Pattern::Str { .. }
        | Pattern::Bool { .. }
        | Pattern::IsType { .. } => pattern.clone(),
    }
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
    /// A `from_bytes::<T>` whose span is here has a `Validate`-implementing packed element type
    /// (validation arc): the emitted [`Rvalue::FromBytes`] carries `validate: true`.
    pub from_bytes_validated: &'a HashSet<Span>,
    /// A `fields_of` whose span is here may report the operand's **private** fields: the emitted
    /// [`Rvalue::Reflect`] carries `private_fields: true`. Decided by the checker (the same rule a
    /// written `x.secret` goes through), baked here so both backends read one answer.
    pub fields_of_private: &'a HashSet<Span>,
    /// A `list[i].field` member read whose span is here (the index receiver is a built-in `List`) fuses
    /// to a single [`Rvalue::IndexField`], reading a packed element's field without materializing it.
    pub index_field_sites: &'a HashSet<Span>,
    /// Calls whose labelled arguments bind out of written order: parameter position → written
    /// index. Applied in [`Lowerer::lower_args`], so the VM and the reference agree by
    /// construction rather than by each re-deriving the binding.
    pub arg_orders: &'a HashMap<Span, Vec<Option<usize>>>,
    /// Call-site-typed native-call recipes (`json.parse::<T>`), baked into [`Rvalue::TypedModuleCall`].
    pub typed_module_call_sites: &'a HashMap<Span, noeta_ext_abi::TypeRecipe>,
    /// Call-site-typed extern-METHOD recipes (`resp.json::<T>`, http arc H8), baked into
    /// [`Rvalue::TypedMethodCall`]. Presence here overrides the turbofish erasure.
    pub typed_method_call_sites: &'a HashMap<Span, noeta_ext_abi::TypeRecipe>,
    /// `json.decode_typed(name, text)` call spans (L2.2 DI) → lowered to [`Rvalue::DecodeTyped`]
    /// instead of a generic method call, routing to the runtime decode-by-type registry.
    pub decode_typed_sites: &'a HashSet<Span>,
    /// `for` spans whose iterable is statically an `Iterator<T>` → the lowered [`Stmt::For`] streams
    /// via `next()` rather than snapshotting a list (Track I.2).
    pub for_stream_sites: &'a HashSet<Span>,
    /// Fixed-width arithmetic sites (Tier W) → the result's `(signed, bits)`, wrapping the op's result
    /// in [`Rvalue::MaskWidth`] so the erased i64 is masked back into the declared width.
    pub width_sites: &'a HashMap<Span, (bool, u8)>,
    /// Display sites whose value contains an unsigned 64-bit integer → the
    /// [`noeta_ast::RenderHint`] built from its static type. The rendered atom is wrapped in an
    /// [`Rvalue::Render`], which turns it into its display string with the erased words read
    /// unsigned — the display twin of `width_sites`, and applied by both backends alike.
    pub render_hint_sites: &'a HashMap<Span, noeta_ast::RenderHint>,
    /// JSON sites whose serialized value contains an unsigned 64-bit integer → the
    /// [`noeta_ast::RenderHint`] built from its static type. The serializing call is replaced by an
    /// [`Rvalue::JsonRender`] over the one value it serializes, so the erased words reach the wire
    /// unsigned — the wire twin of `render_hint_sites`, and applied by both backends alike.
    pub json_hint_sites: &'a HashMap<Span, noeta_ast::RenderHint>,
    /// Ordering sites whose value contains an unsigned 64-bit integer → the
    /// [`noeta_ast::RenderHint`] built from the receiver's (or iterable's) static type. Baked onto
    /// [`Rvalue::Method::order`] / [`Stmt::For::order`] so both backends read those erased words
    /// unsigned when producing the order a program sees — the ordering twin of `render_hint_sites`.
    pub order_hint_sites: &'a HashMap<Span, noeta_ast::RenderHint>,
    /// Deferred-serialization sites: the span of a native call that BINDS a value it serializes on a
    /// later tick → the [`noeta_ast::RenderHint`] built from the bound value's static type.
    pub binding_hint_sites: &'a HashMap<Span, noeta_ast::RenderHint>,
    /// `Expr::TypeTest` spans the checker answered statically → the answer. The test lowers to that
    /// constant (the scrutinee still evaluated, for its effects) instead of an [`Rvalue::TypeTest`],
    /// so no runtime matcher is consulted for a question it cannot answer. See
    /// `noeta_check::Sites::folded_type_tests` for which tests land here and why.
    pub folded_type_tests: &'a HashMap<Span, bool>,
    /// Collection-construction sites → the resolved element [`noeta_ast::reflect::TypeRepr`] baked onto
    /// [`Rvalue::List`] so `type_of` recovers it after a `dyn` launder (R1 reflection).
    pub construction_sites: &'a HashMap<Span, noeta_ast::reflect::TypeRepr>,
    /// The nominal type each target-typed `.{ … }` literal resolved to (see
    /// `noeta_check::Sites::inferred_object_types`) → the name baked into [`Rvalue::Object`].
    pub inferred_object_types: &'a HashMap<Span, String>,
    /// Bare payload-free variant patterns (see `noeta_check::Sites::variant_pattern_sites`): each
    /// `match`-arm [`noeta_ast::Pattern::Binding`] span the checker resolved to a variant of the
    /// scrutinee's own enum → `(qualifier, variant)`. Rewritten here into a
    /// [`noeta_ast::Pattern::Variant`] with no bindings, so both backends keep seeing an ordinary
    /// qualified variant pattern and neither has to know the bare spelling exists.
    pub variant_pattern_sites: &'a VariantPatternSites,
    /// Unbound method-handle sites (`Type.method` in value position) → the resolved
    /// `(ty, method, associated)`, emitted as an [`Rvalue::MethodHandle`] instead of a field load.
    pub handle_sites: &'a HashMap<Span, (String, String, bool)>,
    /// **Bound**-handle sites (`value.method` in value position, EX.2b) → emitted as an
    /// [`Rvalue::BoundHandle`] (the receiver captured) instead of a field load.
    pub bound_handle_sites: &'a HashSet<Span>,
    /// Type-parameter associated-call sites (`T.m(…)`) → the receiver parameter's spelling. The
    /// call is rewritten into the runtime's by-name dispatch ([`type_param_assoc_call`]); the
    /// spelling is what stops the rewritten call from matching this map again.
    pub type_param_assoc_sites: &'a HashMap<Span, String>,
    /// Field-call sites: `obj.f(args)` call spans the checker resolved to a **field** of the
    /// receiver's type → lowered as [`Rvalue::Field`] then [`Rvalue::Call`] (the field-access-
    /// then-call desugar) instead of [`Rvalue::Method`], so `obj.f(args)` means `(obj.f)(args)`.
    pub field_call_sites: &'a HashSet<Span>,
    /// Generic-method turbofish spans reached via the `TypedModuleCall` surface (D3): a
    /// `recv.m::<T>(args)` whose bare-identifier receiver is a value or a user type (not an
    /// imported native module) → desugared to a plain method call ([`Rvalue::Method`] / the
    /// associated-call path) instead of a native [`Rvalue::TypedModuleCall`].
    pub member_method_call_sites: &'a HashSet<Span>,
    /// Bare float-literal spans the checker adapted into an `f32` context (`mut x: f32 = 1.5`,
    /// P-NUM-SYM). Unlike `f64` (bit-identical to `float`), `f32` is a *distinct 32-bit*
    /// representation, so an adapted literal must lower to a narrow [`Const::F32`] rather than the
    /// default [`Const::Float`] — this set is that type-directed hint.
    pub f32_literal_sites: &'a HashSet<Span>,
    /// Trait method call sites → the resolved `(trait_qualified, method)` route, baked into an
    /// [`Rvalue::TraitMethod`] instead of the generic [`Rvalue::Method`]. Native trait default bodies
    /// (slice 2) plus, since the ExtBundle→ExtTrait fold-in (slice 4), every kernel-trait method.
    pub trait_call_sites: &'a HashMap<Span, (String, String)>,
    /// Namespace-group member-access sites (`http.client`) → the resolved concrete module identity,
    /// emitted as an [`Rvalue::NativeModule`] instead of a field load.
    pub namespace_module_sites: &'a HashMap<Span, String>,
    /// `?`-conversion sites (error-ergonomics): `Expr::Try` spans whose `Err` payload converts
    /// through the enclosing function's error type → that target type's name, and the method-table
    /// key of the conversion its `Err` type selected. The `?` operand is rewritten to
    /// `match v { Ok($t) => Ok($t), Err($t) => Err(Target.from($t)) }` — ordinary IR, so both
    /// backends (and the JIT) convert identically by construction.
    pub try_conversion_sites: &'a HashMap<Span, (String, String)>,
    /// Explicit `Target.from(x)` spans on a target declaring several conversions → the method-table
    /// key the argument's type selected. Lowering substitutes it for the `from` the source wrote, so
    /// the call reaches the one body the checker resolved.
    pub from_call_sites: &'a HashMap<Span, String>,
    /// The program-wide type-argument table (poly-values F2b) — embedded into
    /// [`Program::type_args`] so both backends resolve a hidden slot's instantiation identically.
    pub type_arg_table: &'a Vec<noeta_ext_abi::TypeArgInfo>,
    /// The type-argument table's reflection projection, indexed identically — embedded into
    /// [`Program::type_arg_reprs`] so a dynamic construction site resolves the same interned
    /// `TypeRepr` in either backend.
    pub type_arg_reprs: &'a Vec<Option<noeta_ast::reflect::TypeRepr>>,
    /// The type-argument table's render-hint projection, indexed identically — embedded into
    /// [`Program::type_arg_hints`] so a door inside a generic body resolves the same signedness in
    /// either backend.
    pub type_arg_hints: &'a Vec<noeta_ext_abi::TypeArgHints>,
    /// Forwarding-generic call spans → the type-argument slots the call supplies, in slot order
    /// (`Table(i)` → an int const; `Forward(j)` → the enclosing body's `$ty<j>` local). They land
    /// in the call node's own `type_args` channel, beside the value arguments.
    pub hidden_arg_sites: &'a HashMap<Span, Vec<noeta_ext_abi::HiddenArg>>,
    /// **Dynamic** construction sites (generic-in-generic construction): a fresh-constructor call
    /// span → the enclosing body's hidden slot index whose table entry names the instantiation to
    /// stamp on the object the call built. Lowered onto [`Rvalue::Method::reflect_slot`].
    pub dynamic_construction_sites: &'a HashMap<Span, u32>,
    /// Spans whose turbofish is a FORWARDED type parameter of the enclosing top-level generic fn →
    /// the hidden slot index whose table entry names the instantiation. One map over three
    /// surfaces, each consulted from its own expression arm: a `TypedModuleCall` lowers with a
    /// dynamic table-index operand instead of a baked recipe, an `attributes_of::<T>` resolves the
    /// type name through the table at run time, and a `type_name::<T>()` becomes an
    /// [`Rvalue::TypeSlotName`]. Same slot, same entry — which is why a forwarded name and a
    /// forwarded manifest cannot disagree about what `T` is.
    pub forwarded_slot_sites: &'a HashMap<Span, u32>,
    /// `type_name::<T>()` spans where `T` is a parameter of the ENCLOSING generic type, inside one
    /// of its instance methods → `(enclosing type name, the parameter's declaration index)`. The
    /// name is read off argument `index` of the receiver's reflected type tag at run time.
    pub self_type_arg_sites: &'a HashMap<Span, (String, u32)>,
    /// Forwarding generic fns and methods → their hidden-slot count, keyed as the callable traces
    /// (a bare `fn` name, or `Type.method`); lowered as the leading parameters `$ty0`, `$ty1`, … .
    pub forwarding_fns: &'a HashMap<String, u32>,
    /// Instance methods of a generic type → how many render slots they read off the receiver,
    /// keyed as `forwarding_fns` is. Slot `hidden + i` of such a body is type argument `i` of
    /// `self`'s reflected tag.
    pub self_render_fns: &'a HashMap<String, u32>,
    /// Forwarding-fn-as-value sites (poly-deferrals D2c): `Expr::Ident` spans → `(fn name,
    /// adopted arity)`. The reference lowers to a synthesized closure calling the fn; the inner
    /// call reuses this same span, so `hidden_arg_sites` binds the resolved slots into the value.
    pub fn_value_sites: &'a HashMap<Span, (String, u32)>,
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
        static HINTS: OnceLock<HashMap<Span, noeta_ast::RenderHint>> = OnceLock::new();
        static FOLDED: OnceLock<HashMap<Span, bool>> = OnceLock::new();
        static REPRS: OnceLock<HashMap<Span, noeta_ast::reflect::TypeRepr>> = OnceLock::new();
        static HANDLES: OnceLock<HashMap<Span, (String, String, bool)>> = OnceLock::new();
        static PAIRS: OnceLock<HashMap<Span, (String, String)>> = OnceLock::new();
        static NAMES: OnceLock<HashMap<Span, String>> = OnceLock::new();
        static VARIANT_PATTERNS: OnceLock<VariantPatternSites> = OnceLock::new();
        static TYPE_ARGS: OnceLock<Vec<noeta_ext_abi::TypeArgInfo>> = OnceLock::new();
        static TYPE_ARG_REPRS: OnceLock<Vec<Option<noeta_ast::reflect::TypeRepr>>> =
            OnceLock::new();
        static TYPE_ARG_HINTS: OnceLock<Vec<noeta_ext_abi::TypeArgHints>> = OnceLock::new();
        static HIDDEN: OnceLock<HashMap<Span, Vec<noeta_ext_abi::HiddenArg>>> = OnceLock::new();
        static SLOTS: OnceLock<HashMap<Span, u32>> = OnceLock::new();
        static SELF_TY: OnceLock<HashMap<Span, (String, u32)>> = OnceLock::new();
        static COUNTS: OnceLock<HashMap<String, u32>> = OnceLock::new();
        static FN_VALUES: OnceLock<HashMap<Span, (String, u32)>> = OnceLock::new();
        static ORDERS: OnceLock<HashMap<Span, Vec<Option<usize>>>> = OnceLock::new();
        LoweringSites {
            packed_list_sites: PACKED.get_or_init(HashMap::new),
            from_bytes_validated: SPANS.get_or_init(HashSet::new),
            fields_of_private: SPANS.get_or_init(HashSet::new),
            index_field_sites: SPANS.get_or_init(HashSet::new),
            arg_orders: ORDERS.get_or_init(HashMap::new),
            typed_module_call_sites: RECIPES.get_or_init(HashMap::new),
            typed_method_call_sites: RECIPES.get_or_init(HashMap::new),
            decode_typed_sites: SPANS.get_or_init(HashSet::new),
            for_stream_sites: SPANS.get_or_init(HashSet::new),
            width_sites: WIDTHS.get_or_init(HashMap::new),
            render_hint_sites: HINTS.get_or_init(HashMap::new),
            json_hint_sites: HINTS.get_or_init(HashMap::new),
            order_hint_sites: HINTS.get_or_init(HashMap::new),
            binding_hint_sites: HINTS.get_or_init(HashMap::new),
            folded_type_tests: FOLDED.get_or_init(HashMap::new),
            construction_sites: REPRS.get_or_init(HashMap::new),
            inferred_object_types: NAMES.get_or_init(HashMap::new),
            variant_pattern_sites: VARIANT_PATTERNS.get_or_init(HashMap::new),
            handle_sites: HANDLES.get_or_init(HashMap::new),
            bound_handle_sites: SPANS.get_or_init(HashSet::new),
            type_param_assoc_sites: NAMES.get_or_init(HashMap::new),
            field_call_sites: SPANS.get_or_init(HashSet::new),
            member_method_call_sites: SPANS.get_or_init(HashSet::new),
            f32_literal_sites: SPANS.get_or_init(HashSet::new),
            trait_call_sites: PAIRS.get_or_init(HashMap::new),
            namespace_module_sites: NAMES.get_or_init(HashMap::new),
            try_conversion_sites: PAIRS.get_or_init(HashMap::new),
            from_call_sites: NAMES.get_or_init(HashMap::new),
            type_arg_table: TYPE_ARGS.get_or_init(Vec::new),
            type_arg_reprs: TYPE_ARG_REPRS.get_or_init(Vec::new),
            type_arg_hints: TYPE_ARG_HINTS.get_or_init(Vec::new),
            dynamic_construction_sites: SLOTS.get_or_init(HashMap::new),
            hidden_arg_sites: HIDDEN.get_or_init(HashMap::new),
            forwarded_slot_sites: SLOTS.get_or_init(HashMap::new),
            self_type_arg_sites: SELF_TY.get_or_init(HashMap::new),
            forwarding_fns: COUNTS.get_or_init(HashMap::new),
            self_render_fns: COUNTS.get_or_init(HashMap::new),
            fn_value_sites: FN_VALUES.get_or_init(HashMap::new),
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
            from_bytes_validated: &$s.from_bytes_validated,
            fields_of_private: &$s.fields_of_private,
            index_field_sites: &$s.index_field_sites,
            arg_orders: &$s.arg_orders,
            typed_module_call_sites: &$s.typed_module_call_sites,
            typed_method_call_sites: &$s.typed_method_call_sites,
            decode_typed_sites: &$s.decode_typed_sites,
            for_stream_sites: &$s.for_stream_sites,
            width_sites: &$s.width_sites,
            render_hint_sites: &$s.render_hint_sites,
            json_hint_sites: &$s.json_hint_sites,
            order_hint_sites: &$s.order_hint_sites,
            binding_hint_sites: &$s.binding_hint_sites,
            folded_type_tests: &$s.folded_type_tests,
            construction_sites: &$s.construction_sites,
            inferred_object_types: &$s.inferred_object_types,
            variant_pattern_sites: &$s.variant_pattern_sites,
            handle_sites: &$s.handle_sites,
            bound_handle_sites: &$s.bound_handle_sites,
            type_param_assoc_sites: &$s.type_param_assoc_sites,
            field_call_sites: &$s.field_call_sites,
            member_method_call_sites: &$s.member_method_call_sites,
            f32_literal_sites: &$s.f32_literal_sites,
            trait_call_sites: &$s.trait_call_sites,
            namespace_module_sites: &$s.namespace_module_sites,
            try_conversion_sites: &$s.try_conversion_sites,
            from_call_sites: &$s.from_call_sites,
            type_arg_table: &$s.type_arg_table,
            type_arg_reprs: &$s.type_arg_reprs,
            type_arg_hints: &$s.type_arg_hints,
            dynamic_construction_sites: &$s.dynamic_construction_sites,
            hidden_arg_sites: &$s.hidden_arg_sites,
            forwarded_slot_sites: &$s.forwarded_slot_sites,
            self_type_arg_sites: &$s.self_type_arg_sites,
            forwarding_fns: &$s.forwarding_fns,
            self_render_fns: &$s.self_render_fns,
            fn_value_sites: &$s.fn_value_sites,
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
    /// The facts of the **enclosing program**, for a lowering that is a *fragment* of one: a
    /// hot-swapped definition, a REPL entry in a session whose earlier entries declared what it
    /// uses. Empty (the default) means "this **is** the whole program", which is every file-pipeline
    /// compile.
    ///
    /// See [`ProgramFacts`] for what belongs here and why a fragment lowered without it produced
    /// panics and silently wrong narrowings. [`ProgramFacts::under`] states the merge rule.
    pub ambient: ProgramFacts,
}

/// Deliberately **not** `Default`. The field that would default is [`Self::ambient`], and an empty
/// ambient set is not a neutral choice — it means "the code I am lowering IS the whole program".
/// That is right for a file compile and silently wrong for a fragment (a hot-swap install, a REPL
/// entry), and the silence is where three shipped bugs came from: `@html` lowered to a panic,
/// `x is Uuid` answered `false`, a swapped `async` body touching a module global panicked. A caller
/// that has to write `ambient:` has to decide which case it is in; a caller reaching for
/// `..Default::default()` never gets asked.
impl LowerOptions {
    /// The options a **whole-program** lowering wants: no enclosing program, cooperative isolates,
    /// the process-global registry. Named rather than `Default` so the fragment case cannot be
    /// reached by omission — see the note above.
    pub fn whole_program() -> LowerOptions {
        LowerOptions {
            real_isolates: false,
            registry: noeta_ext_abi::registry::single_registry_process(),
            ambient: ProgramFacts::default(),
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

/// Project a registry's **native trait data** into the plain-data form
/// [`noeta_ast::reflect::build`] joins with a program's own declarations (the trait-membership
/// twin of `Registry::native_roles`): every native trait's qualified identity, every native
/// type/class/struct/enum's ABI-advertised trait impls (keyed by the qualified identity an extern
/// value reports at runtime), and every native derive recipe's name (excluded from membership —
/// a recipe synthesizes methods but implements no trait). Shared by both backends' reflection
/// builds so the membership table agrees across the differential by construction.
pub fn native_trait_impls(
    registry: &'static noeta_ext_abi::registry::Registry,
) -> noeta_ast::reflect::NativeTraitImpls {
    let traits: Vec<String> = registry.traits().map(|t| t.qualified()).collect();
    let advertised = |names: &'static [&'static str]| -> Option<Vec<String>> {
        (!names.is_empty()).then(|| names.iter().map(|n| (*n).to_string()).collect())
    };
    let type_impls: Vec<(String, Vec<String>)> = registry
        .extensions()
        .iter()
        .flat_map(|ext| ext.types())
        .filter_map(|ty| Some((ty.qualified(), advertised(ty.traits)?)))
        .chain(
            registry
                .fielded()
                .filter_map(|f| Some((f.qualified(), advertised(f.traits)?))),
        )
        .chain(
            registry
                .enums()
                .filter_map(|e| Some((e.qualified(), advertised(e.traits)?))),
        )
        .collect();
    let derives: Vec<String> = registry.ext_derives().map(|d| d.name.to_string()).collect();
    noeta_ast::reflect::NativeTraitImpls {
        traits,
        type_impls,
        derives,
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
    lower_with_sites_opts(program, sites, LowerOptions::whole_program())
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
    // The process-global registry when one is seeded (`None` in registry-less unit tests — native
    // derives simply don't materialize there, exactly as they don't check).
    hoist_impl_methods_with_registry(program, noeta_ext_abi::registry::default_registry())
}

/// As [`hoist_standalone_impl_methods`], against an explicit extension registry — the
/// instance-registry seam: native derive recipes (`ExtDerive`, derive layer 4) resolve against
/// `registry`, so an embed session's own extensions' derives materialize.
/// The hoist's answers for the shared derive cascade: user traits from a scan of the (linked)
/// program, native recipes from this lowering's extension registry.
struct HoistDeriveContext<'a> {
    traits: &'a StdHashMap<&'a str, &'a noeta_ast::TraitDecl>,
    registry: Option<&'static noeta_ext_abi::registry::Registry>,
}

impl noeta_ast::derive::DeriveContext for HoistDeriveContext<'_> {
    fn user_trait(&self, name: &str) -> Option<noeta_ast::TraitDecl> {
        self.traits.get(name).map(|t| (*t).clone())
    }

    fn native_recipe(&self, name: &str) -> Option<Vec<(String, usize, String)>> {
        let d = self.registry?.find_ext_derive(name)?;
        Some(
            d.methods
                .iter()
                .map(|m| (m.name.to_string(), m.arity, m.handler.to_string()))
                .collect(),
        )
    }
}

pub fn hoist_impl_methods_with_registry(
    program: &AstProgram,
    registry: Option<&'static noeta_ext_abi::registry::Registry>,
) -> Option<AstProgram> {
    // The trait declarations by name, for default-method fallback (UT5) — generic traits
    // included: an impl at an instantiation (`impl Cache<string>`) substitutes the arguments
    // through the defaults before they hoist.
    let traits: StdHashMap<&str, &noeta_ast::TraitDecl> = program
        .stmts
        .iter()
        .filter_map(|s| match s {
            AstStmt::Trait(t) => Some((t.name.as_str(), t)),
            _ => None,
        })
        .collect();
    // A trait's default methods that `provided` does not override, in declaration order —
    // instantiated at `trait_args` when the trait is generic. An arity mismatch (including a
    // generic trait implemented with no arguments) contributes nothing; the checker reports it.
    let omitted_defaults =
        |trait_name: &str, trait_args: &[noeta_ast::TypeRef], provided: &[FnDecl]| -> Vec<FnDecl> {
            let Some(t) = traits.get(trait_name) else {
                return Vec::new();
            };
            let instantiated = match noeta_ast::derive::instantiate_trait(t, trait_args) {
                Ok(concrete) => concrete,
                Err(_) => return Vec::new(),
            };
            let t = instantiated.as_ref().unwrap_or(t);
            t.methods
                .iter()
                .filter(|tm| tm.has_default && !provided.iter().any(|m| m.name == tm.sig.name))
                .map(|tm| tm.sig.clone())
                .collect()
        };

    let mut additions: StdHashMap<String, Vec<FnDecl>> = StdHashMap::new();
    for stmt in &program.stmts {
        if let AstStmt::Impl(decl) = stmt {
            let entry = additions.entry(decl.target.to_string()).or_default();
            entry.extend(decl.methods.iter().cloned());
            // Default-method fallback (UT5): the impl'd trait's omitted defaults ride along,
            // after the impl's own methods so a provided override wins the name-skip below.
            entry.extend(omitted_defaults(
                decl.trait_name.as_str(),
                &decl.trait_args,
                &decl.methods,
            ));
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
        AstStmt::Struct(d) => body_references_trait(&d.impls, &d.decorators.derives),
        AstStmt::Class(d) => body_references_trait(&d.impls, &d.decorators.derives),
        AstStmt::Enum(d) => body_references_trait(&d.impls, &d.decorators.derives),
        _ => false,
    });
    // A builtin `via:` derive (`@derive(Comparable, via: amount)`), a plain `@derive(Error)`
    // (whose `message()` is synthesized, error-ergonomics), and a native derive recipe
    // (`@derive(Inspect)`, layer 4) synthesize methods even with no user trait in the program.
    let derive_needs = |derives: &[noeta_ast::DeriveSpec]| {
        derives.iter().any(|d| {
            d.via.is_some()
                || d.name == "Error"
                || registry.is_some_and(|r| r.find_ext_derive(d.name.as_str()).is_some())
        })
    };
    let body_needs = body_needs
        || program.stmts.iter().any(|s| match s {
            AstStmt::Struct(d) => derive_needs(&d.decorators.derives),
            AstStmt::Class(d) => derive_needs(&d.decorators.derives),
            AstStmt::Enum(d) => derive_needs(&d.decorators.derives),
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
                &d.decorators.derives,
            ),
            AstStmt::Class(d) => (
                &d.name,
                &mut d.methods,
                d.fields.as_slice(),
                &d.impls,
                &d.decorators.derives,
            ),
            AstStmt::Enum(d) => (
                &d.name,
                &mut d.methods,
                &[] as &[noeta_ast::FieldDecl],
                &d.impls,
                &d.decorators.derives,
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
            synthesized.extend(omitted_defaults(
                block.trait_name.as_str(),
                &block.trait_args,
                methods,
            ));
        }
        // The ONE cascade (`noeta_ast::derive::plan_derive`), the same call the checker makes when
        // it registers these methods' signatures — so what the checker types and what runs cannot
        // disagree about which planner a derive resolves to. A plan error is dropped here; the
        // checker reports it and the program does not run.
        let ctx = HoistDeriveContext {
            traits: &traits,
            registry,
        };
        for spec in derives {
            if let Some(Ok(planned)) =
                noeta_ast::derive::plan_derive(&ctx, spec, name.as_str(), fields, methods)
            {
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
        ambient,
    } = opts;
    // Hoist standalone-`impl` methods onto their target type (L1 user traits, UT2) before lowering,
    // so `(type, method)` dispatch resolves them — against THIS lowering's registry, so an embed
    // session's native derives (layer 4) materialize. Only rebinds when something hoists.
    let hoisted = hoist_impl_methods_with_registry(program, Some(registry));
    let program: &AstProgram = hoisted.as_ref().unwrap_or(program);
    let mut lowerer = Lowerer {
        temps: 0,
        fn_depth: 0,
        hidden_slots: 0,
        self_render_slots: 0,
        sites,
        real_isolates,
        synth_step_name: None,
        synth_step_captures: None,
        self_type_name: None,
        // What this lowering knows about the program: the code in hand, folded over whatever the
        // caller says encloses it (empty for a whole program — see `LowerOptions::ambient`).
        facts: ambient.under(program, registry),
        registry,
    };
    let top = lowerer.lower_top_level(&program.stmts)?;
    Ok(Program {
        top,
        temp_count: lowerer.temps,
        type_args: sites.type_arg_table.clone(),
        type_arg_reprs: sites.type_arg_reprs.clone(),
        type_arg_hints: sites.type_arg_hints.clone(),
        span: program.span,
    })
}

/// Carries the temporary counter for the function frame currently being lowered. One
/// `Lowerer` field is reused across nested frames by save/restore (see `lower_func`), so the
/// counter always reflects the innermost activation.
struct Lowerer<'a> {
    /// The next free temporary index in the current frame; also the running frame size.
    temps: u32,
    /// How many function frames enclose the code being lowered: `0` at module top level, `1+`
    /// inside a fn/method/closure body. Only a TOP-LEVEL `fn` (depth 0) may carry hidden
    /// forwarding parameters — a NESTED fn that forwards (D2b) reads the enclosing fn's hidden
    /// locals through closure capture instead, and its (possibly colliding) name must never key
    /// the top-level `forwarding_fns` table.
    fn_depth: u32,
    /// How many hidden type-argument slots (`$ty0`, `$ty1`, …) the innermost **top-level** `fn`
    /// enclosing this code carries — the operands a [`noeta_ast::RenderHint::Param`] at a door
    /// resolves through ([`Lowerer::hint_slots`]).
    ///
    /// Set when a top-level declaration is entered and **retained** through nested `fn`s and
    /// closures, which reach the same locals as captures (D2b). `0` at module top level and inside
    /// any top-level declaration that carries no slots.
    hidden_slots: u32,
    /// How many render slots the innermost **top-level** declaration enclosing this code reads off
    /// its **receiver** — the enclosing generic type's own parameters, when this is one of its
    /// instance methods — the checker's `self_render_fns`, keyed exactly as `forwarding_fns` is.
    ///
    /// They follow the hidden `$ty` slots in the one slot list a
    /// [`noeta_ast::RenderHint::Param`] indexes, so slot `hidden_slots + i` is type argument `i` of
    /// `self`'s reflected tag ([`Lowerer::hint_slots`]). Set and retained exactly as
    /// [`Self::hidden_slots`] is, so a closure inside such a method resolves a door through the
    /// receiver it captures. `0` at top level and in every declaration that is not one.
    self_render_slots: u32,
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
    /// The armed seal for the synthesized generator/async step closure (see `synth_step_name`).
    synth_step_captures: Option<Vec<String>>,
    /// The type whose body is being lowered — what a written `Self` denotes here. Set around a
    /// type's methods, destructor and field defaults; `None` at top level and inside a free `fn`.
    ///
    /// Per-NODE state, not program-derived: it is a property of where the lowering currently is,
    /// so a fragment lowering (a hot swap, a REPL entry) computes it from the very declaration it
    /// is handed rather than needing to see the whole program. It carries the declaration's name
    /// as the linker left it, so `Self` folds to the same qualified identity the type's own
    /// spelling would.
    self_type_name: Option<String>,
    /// Everything lowering knows about the **program** rather than about the node in hand — see
    /// [`ProgramFacts`], which is the one place such state may live. There is exactly one field of
    /// this kind, and `tests/lowerer_field_census.rs` is what keeps it that way: a second
    /// program-derived table beside this one would be empty for a fragment lowering (a hot swap, a
    /// REPL entry), which is how three shipped bugs happened.
    facts: ProgramFacts,
    /// The extension registry a **native** expression tier's handler resolves against
    /// (instance-registry IR5): an `@json` block's `ExtTier::handler` is looked up here, so an
    /// embed session's own extension-declared expression tier lowers against *its* registry.
    registry: &'static noeta_ext_abi::registry::Registry,
}

/// The **compile-time half of the field census** (`tests/lowerer_field_census.rs`).
///
/// The census proper reads [`Lowerer`]'s field list out of this file's *source* and refuses any
/// field it cannot classify. That read is text, so it could in principle go stale — a field written
/// in a shape the scanner does not recognize would be silently invisible to it, and an unclassified
/// field is exactly what the census exists to catch.
///
/// This test is the backstop: it names every field twice, once building a [`Lowerer`] and once
/// taking it apart, with no `..` on either side. Adding a field therefore fails to **compile** here
/// (E0063 on the literal, E0027 on the pattern) whatever shape it is written in, and the compiler
/// error lands next to this doc comment, which says where to go classify it.
#[cfg(test)]
#[test]
fn every_lowerer_field_is_named_by_the_census() {
    // An empty registry, leaked once: this test is about the field LIST, not about which natives
    // resolve, and `noeta-ir` links no extension units of its own (so there is no process default).
    static REGISTRY: std::sync::OnceLock<noeta_ext_abi::registry::Registry> =
        std::sync::OnceLock::new();
    let registry = REGISTRY.get_or_init(|| noeta_ext_abi::registry::Registry::new(Vec::new()));

    let lowerer = Lowerer {
        temps: 0,
        fn_depth: 0,
        hidden_slots: 0,
        self_render_slots: 0,
        sites: LoweringSites::empty(),
        real_isolates: false,
        synth_step_name: None,
        synth_step_captures: None,
        self_type_name: None,
        facts: ProgramFacts::default(),
        registry,
    };
    // No `..` — a new field must be added here, and then classified in
    // `tests/lowerer_field_census.rs::TABLE` as per-node state, environment/config, checker-supplied
    // sites, or (for the ONE `ProgramFacts`) program-derived.
    let Lowerer {
        temps: _,
        fn_depth: _,
        hidden_slots: _,
        self_render_slots: _,
        sites: _,
        real_isolates: _,
        synth_step_name: _,
        synth_step_captures: _,
        self_type_name: _,
        facts: _,
        registry: _,
    } = lowerer;
}

/// What the IR pipeline learns from **the whole program** instead of from the node in hand.
///
/// This exists because a lowering is not always given a whole program. A hot-swap fragment and a
/// REPL entry are *pieces* of one — and every table here, derived by reading the program's
/// top-level statements, is therefore empty or partial for them. Each such table was independently
/// a bug: an `@html { … }` in a swapped body lowered to a panic because the `@tier` declaration is
/// in the imported package; `x is Uuid` silently answered `false` because only a *changed* `use`
/// rides in a fragment; a swapped `async` body that assigned a module global panicked because the
/// state-machine desugar hoisted a global into a cell; and a self-update in a swapped body reused
/// its allocation in place — skipping a destructor a cold start runs — because the class carrying
/// the `destruct` block is declared outside the fragment ([`ProgramFacts::own_destructors`]).
///
/// That last one is read by a **pass over the lowered IR**
/// ([`noeta_ir_passes::thread_reuse`](../../noeta_ir_passes/fn.thread_reuse.html)) rather than by
/// the lowerer, and it still belongs here: the question it answers is the identical one — *what
/// does a fragment see of the program it belongs to?* — and answering it anywhere else is how it
/// went unasked for four passes running.
///
/// So: this struct is the single home for that state. A new table lowering derives from the program
/// belongs **here**, not beside it as another `Lowerer` field — landing here is what forces the
/// question "and what does a fragment see?" to be answered rather than skipped. The answer is
/// [`LowerOptions::ambient`]: the facts of the enclosing program, which [`ProgramFacts::under`]
/// folds beneath the ones the code in hand declares for itself.
#[derive(Debug, Default, Clone)]
pub struct ProgramFacts {
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
    pub type_aliases: HashMap<String, String>,
    /// The **qualified identity** each leaf-imported native type's local name denotes (`Framing` →
    /// `std.http.Framing`), built from the program's own `use` statements — the rewrite
    /// [`Lowerer::lower_type_operand`] applies so a reflection turbofish keys on the name the
    /// reflection artifact registers the type under. Every native nominal kind (enum, fielded,
    /// extern handle) is resolved; see [`collect_native_type_imports`]. Import-driven rather than a
    /// registry-wide short-name lookup on purpose: only a name this program actually imported is
    /// rewritten, so a program's own `Framing` is never redirected to a native type it never
    /// mentioned.
    pub native_type_imports: HashMap<String, String>,
    /// The program's declared expression-tier handlers (tier name → handler fn name), so an
    /// [`Expr::TierExpr`] lowers as the handler call it means — the same
    /// [`noeta_ast::desugar::tier_expr_call`] construction the checker typed. The checker gated
    /// unknown/non-expr tiers (E0052), so a miss here is `Unsupported`, never a panic.
    pub expr_tiers: HashMap<String, String>,
    /// Every top-level binding/`fn` name — the module globals. Threaded into the async/generator
    /// state-machine desugar so a **bare reassignment of a global** (`g.n = …`, `counter = …`) is
    /// kept as a global store rather than mis-hoisted into a state-machine cell (which would
    /// initialize a fresh `none` upvalue and shadow the real global — the reference reads a stale
    /// value). A global is never a capturable local, mirroring the compiler's free-variable
    /// analysis (which already filters globals out of captures).
    pub module_globals: HashSet<String>,
    /// Every declared class name carrying its **own** `destruct { … }` block — the reuse pass's
    /// semantic gate ([`noeta_ir_passes::thread_reuse`](../../noeta_ir_passes/fn.thread_reuse.html)).
    ///
    /// A self-update (`acc = T { ...acc, f: v }`) may only reuse the displaced allocation when `T`
    /// runs no destructor of its own, because reuse means the displaced value is *never destroyed*
    /// while the copy-and-destroy baseline destroys it on every update (spec §5). The pass derived
    /// that set from the class declarations in the IR it was handed — which for a fragment is
    /// **none of them**, so every self-update looked destructor-free and a hot-swapped body
    /// silently stopped running a destructor its cold start runs. Deterministic destruction is not
    /// something a swap may quietly change, so the fact travels with the program.
    ///
    /// Top-level declarations only, and that is complete: a class declared inside a function body
    /// is nameable only there, so a self-update of it is in that same body — which is in the
    /// fragment whenever it changed, and its own `ClassDef` is then in the IR in hand.
    pub own_destructors: HashSet<String>,
}

impl ProgramFacts {
    /// Read a program's own facts. For a whole-program lowering this is the entire table; for a
    /// fragment it is the part the fragment happens to carry, which is why [`Self::under`] exists.
    pub fn of(
        program: &AstProgram,
        registry: &'static noeta_ext_abi::registry::Registry,
    ) -> ProgramFacts {
        ProgramFacts {
            type_aliases: collect_type_aliases(program, registry),
            native_type_imports: collect_native_type_imports(program, registry),
            expr_tiers: noeta_ast::desugar::expr_tier_handlers(program)
                .into_iter()
                .collect(),
            module_globals: module_global_names(program),
            own_destructors: own_destructor_class_names(program),
        }
    }

    /// Fold these (ambient) facts **under** `program`'s own, yielding what a lowering of `program`
    /// should see.
    ///
    /// The merge rule differs per table, and this is the one place it is stated. The three
    /// name → name maps **shadow**: an entry the code in hand declares wins, because redeclaring a
    /// local name means the local one (a fragment that adds `use other.Uuid` means *that* `Uuid`).
    /// `module_globals` **unions**: a global is a global no matter which side of the program the
    /// lowering can see, and the set is only ever read as "is this name a global", never "whose".
    /// `own_destructors` unions too, and deliberately errs *toward* membership: a stale entry (a
    /// later version dropped the `destruct` block) only costs the in-place-reuse optimization,
    /// which is observationally transparent, whereas a missing entry costs a destructor.
    pub fn under(
        mut self,
        program: &AstProgram,
        registry: &'static noeta_ext_abi::registry::Registry,
    ) -> ProgramFacts {
        let own = ProgramFacts::of(program, registry);
        self.type_aliases.extend(own.type_aliases);
        self.native_type_imports.extend(own.native_type_imports);
        self.expr_tiers.extend(own.expr_tiers);
        self.module_globals.extend(own.module_globals);
        self.own_destructors.extend(own.own_destructors);
        self
    }

    /// Absorb a later program's facts into an accumulating set (a session adding an entry): same
    /// per-table rules as [`Self::under`], with the newcomer winning.
    pub fn absorb(&mut self, other: ProgramFacts) {
        self.type_aliases.extend(other.type_aliases);
        self.native_type_imports.extend(other.native_type_imports);
        self.expr_tiers.extend(other.expr_tiers);
        self.module_globals.extend(other.module_globals);
        self.own_destructors.extend(other.own_destructors);
    }
}

/// The top-level class names carrying their own `destruct { … }` block — see
/// [`ProgramFacts::own_destructors`]. A struct is bodiless and never has one, so only classes are
/// looked at.
fn own_destructor_class_names(program: &AstProgram) -> HashSet<String> {
    program
        .stmts
        .iter()
        .filter_map(|stmt| match stmt {
            AstStmt::Class(decl) if decl.destructor.is_some() => Some(decl.name.to_string()),
            _ => None,
        })
        .collect()
}

/// The top-level binding and `fn` names of a program — the module globals a nested function
/// resolves by name (never captures). These are the only names that can appear as a bare
/// reassignment target inside an async/generator body while denoting a global, so they are the
/// set the state-machine desugar excludes from cell-hoisting.
/// Build the synthetic conversion `match` a `?`-conversion site (error-ergonomics) lowers its
/// operand through: `match <operand> { Ok($try) => Ok($try), Err($try) => Err(Target.from($try)) }`.
/// The operand is the `?`'s own operand expression (evaluated exactly once, as the match
/// scrutinee); the `$try` binding name cannot collide with user bindings (`$` is not writable in
/// source), and every synthetic node reuses the `?`'s span so diagnostics and tracebacks keep
/// pointing at the `?`.
fn try_conversion_match(operand: &Expr, target: &str, method: &str, span: Span) -> Expr {
    let ident = |name: &str| Expr::Ident {
        name: noeta_ast::Name::canonical(name),
        span,
    };
    let call = |callee: Expr, args: Vec<Expr>| Expr::Call {
        callee: Box::new(callee),
        // A desugar builds positional calls by construction.
        args: args
            .into_iter()
            .map(noeta_ast::CallArg::positional)
            .collect(),
        span,
    };
    let arm = |variant: &str, body: Expr| noeta_ast::MatchArm {
        guard: None,
        pattern: noeta_ast::Pattern::Variant {
            type_name: None,
            variant: variant.to_string(),
            bindings: vec![noeta_ast::Pattern::Binding {
                name: "$try".to_string(),
                span,
            }],
            span,
        },
        body: noeta_ast::ClosureBody::Expr(Box::new(body)),
        span,
    };
    let from_call = call(
        Expr::Member {
            receiver: Box::new(ident(target)),
            // The conversion the checker selected — `from`, or the source-named key one of several
            // conversions on this target occupies. Written here rather than looked up again,
            // because "which conversion does this `Err` type select" is a typing question and this
            // is lowering.
            name: method.to_string(),
            name_span: span,
            span,
        },
        vec![ident("$try")],
    );
    Expr::Match {
        scrutinee: Box::new(operand.clone()),
        arms: vec![
            arm("Ok", call(ident("Ok"), vec![ident("$try")])),
            arm("Err", call(ident("Err"), vec![from_call])),
        ],
        span,
    }
}

/// Build the rewrite a **type-parameter associated call** (`T.m(a, b)`) lowers through:
///
/// ```text
/// match invoke(Type.Named(type_name::<T>(), []), "m", [a, b]) {
///     Ok($assoc)  => $assoc,
///     Err($assoc) => panic($assoc),
/// }
/// ```
///
/// A type parameter is erased, so the compiled body holds no type to dispatch on — but the
/// instantiation's *name* reaches it through the same per-instantiation channel `type_name::<T>()`
/// reads, and by-name dispatch of an associated function is something the runtime already does
/// (`invoke` with a reflection `Type` receiver, both backends, one implementation). So this is a
/// rewrite onto machinery that exists rather than a new dispatch path: nothing in either backend
/// learns that type parameters can appear in receiver position.
///
/// `Type.Named` rather than `Type.Struct`/`Class`/`Enum`: the receiver is read for the name it
/// carries, and `Named` is the one case that commits to no kind — `T` may be instantiated with any
/// of the three.
///
/// `invoke` is fallible, and the `Err` arm is not dead code: the checker proves the *bound's* trait
/// supplies `m` without a receiver, which is everything it can prove statically, and the panic
/// carries the runtime's own message for anything that still cannot be reached. `$assoc` cannot
/// collide with a user binding (`$` is not writable in source), and every synthesized node reuses
/// the call's span except the name lookup, which must sit at the RECEIVER's span — that is where
/// the checker recorded the channel.
fn type_param_assoc_call(param: &str, method: &str, args: &[Expr], span: Span, recv: Span) -> Expr {
    let ident = |name: &str| Expr::Ident {
        name: noeta_ast::Name::canonical(name),
        span,
    };
    let call = |callee: Expr, args: Vec<Expr>| Expr::Call {
        callee: Box::new(callee),
        args: args
            .into_iter()
            .map(noeta_ast::CallArg::positional)
            .collect(),
        span,
    };
    let arm = |variant: &str, body: Expr| noeta_ast::MatchArm {
        guard: None,
        pattern: noeta_ast::Pattern::Variant {
            type_name: None,
            variant: variant.to_string(),
            bindings: vec![noeta_ast::Pattern::Binding {
                name: "$assoc".to_string(),
                span,
            }],
            span,
        },
        body: noeta_ast::ClosureBody::Expr(Box::new(body)),
        span,
    };
    // `type_name::<T>()` at the receiver's span — the site the checker recorded the channel at,
    // and the reason the rewrite resolves `T` at all.
    let type_name = Expr::Reflect {
        which: noeta_ast::ReflectKind::TypeName,
        operand: noeta_ast::ReflectOperand::StaticType(noeta_ast::TypeRef::Named {
            name: noeta_ast::Name::canonical(param),
            args: Vec::new(),
            span: recv,
        }),
        span: recv,
    };
    let receiver = call(
        Expr::Member {
            receiver: Box::new(ident(noeta_ast::reflect::TYPE_ENUM)),
            name: "Named".to_string(),
            name_span: span,
            span,
        },
        vec![
            type_name,
            Expr::List {
                items: Vec::new(),
                span,
            },
        ],
    );
    let invoke = Expr::Reflect {
        which: noeta_ast::ReflectKind::Invoke,
        operand: noeta_ast::ReflectOperand::Dispatch {
            recv: Some(Box::new(receiver)),
            name: Box::new(Expr::Str {
                value: method.to_string(),
                span,
            }),
            args: Box::new(Expr::List {
                items: args.to_vec(),
                span,
            }),
        },
        span,
    };
    Expr::Match {
        scrutinee: Box::new(invoke),
        arms: vec![
            arm("Ok", ident("$assoc")),
            arm("Err", call(ident("panic"), vec![ident("$assoc")])),
        ],
        span,
    }
}

/// Whether a statement is a `fn` declaration eligible for the runtime declaration-hoist
/// ([`Lowerer::lower_stmts_hoisting_fns`]): a named `fn` with **no captures**. A capturing nested fn
/// (`fn f() use (x) { … }`) is excluded — its upvalues are sourced from the enclosing frame's local
/// slots at closure construction, so it must not move above the binding of a captured local. A
/// module-top-level `fn` always has empty captures, so this is exactly `matches!(_, Fn)` there.
fn is_hoistable_fn(stmt: &AstStmt) -> bool {
    matches!(stmt, AstStmt::Fn(decl) if decl.captures.is_empty())
}

/// `stmts` reordered so every hoistable `fn` declaration precedes everything else, each group
/// keeping its source order — the declaration-hoist as a pure **AST** rewrite.
///
/// Extracted from [`Lowerer::lower_stmts_hoisting_fns`] because the coroutine paths need the same
/// rule one stage earlier: [`Lowerer::lower_generator`] / [`Lowerer::lower_async`] hand the raw body
/// to [`desugar_state_machine`], which splits it into states *before* any lowering runs, so a
/// declaration-hoist applied during lowering never reaches them. Sharing this keeps the hoist ONE
/// rule across every scope — bodies, top level, and now state-machine bodies — rather than a second
/// copy that can drift from the first.
fn hoisted_fn_order(stmts: &[AstStmt]) -> impl Iterator<Item = &AstStmt> {
    stmts
        .iter()
        .filter(|s| is_hoistable_fn(s))
        .chain(stmts.iter().filter(|s| !is_hoistable_fn(s)))
}

fn module_global_names(program: &AstProgram) -> HashSet<String> {
    program
        .stmts
        .iter()
        .filter_map(|stmt| match stmt {
            AstStmt::Binding { name, .. } => Some(name.clone()),
            AstStmt::Fn(decl) => Some(decl.name.to_string()),
            _ => None,
        })
        .collect()
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
            } else if let Some(tr) = registry.find_trait_qualified(&qualified) {
                // A native trait import (`use fx.Widget`): a `dyn Widget` narrowing target
                // resolves to the trait's qualified identity — the name the shared membership
                // table (`ReflectionInfo::trait_impls`) keys the implementor set on.
                map.insert(local, tr.qualified());
            } else {
                // A **module or namespace group** (`use std.http`, `use std.{id}`): expose the
                // types under it so a *dotted* narrowing target (`http.Response`, `id.Uuid`)
                // resolves to the same qualified identity the value carries, exactly as the
                // checker's import collection does. Aliased groups key on the alias.
                //
                // Deliberately **not** gated on `is_namespace`. Whether a `use` target is a group
                // or a leaf module is a fact about std's shape, not about the spelling: `std.http`
                // is a group (parent of `.client`/`.server`) and `std.id` is a concrete module, so
                // gating on the group made `id.Uuid` resolve to nothing while `http.Response`
                // resolved — same syntax, different answer, and the failing half was silent
                // (`d is id.Uuid` compared the tag against the literal string `id.Uuid`). The
                // checker's own import pass had the identical split and closed it; this is the
                // other copy of that rule. `namespace_types` is self-gating — it answers with the
                // types actually under the prefix — so no membership test is needed to ask.
                let mut bound = false;
                for (rel, q) in registry.namespace_types(&qualified) {
                    map.insert(format!("{local}.{rel}"), q);
                    bound = true;
                }
                // A module's kernel traits project the same way (`use std.vec` → a dotted
                // `dyn vec.Kernels` target resolves to `std.vec.Kernels`).
                for tr in registry.traits() {
                    let q = tr.qualified();
                    if let Some(rest) = q.strip_prefix(&qualified)
                        && let Some(short) = rest.strip_prefix('.')
                        && !short.contains('.')
                    {
                        map.insert(format!("{local}.{short}"), q);
                        bound = true;
                    }
                }
                if !bound && n.alias.is_some() {
                    // A renamed user (or opaque) import: narrows against the imported leaf name.
                    map.insert(local, n.name.clone());
                }
            }
        }
    }
    map
}

/// The **qualified identity** each leaf-imported native type's local name denotes (`use
/// std.http.{Framing}` → `Framing` ⇒ `std.http.Framing`, honoring `as` renames) — the key the
/// reflection artifact registers a native type under, and the name `type_of` reports for one of its
/// values.
///
/// Every native nominal kind is resolved, not just enums: an **enum**, a **fielded** type (a native
/// class or value struct), and an **extern handle** type. It covered enums only until the native
/// fielded types reached the reflection artifact, and the omission had the same shape for each kind —
/// `field_specs_of::<Frame>()` answered the empty schema right after `use std.http.{Frame}`, and
/// `type_name::<Uuid>()` answered `"Uuid"`, a name nothing is registered under. The latter is the
/// worse of the two: `type_name`'s whole job is to hand a key to something that looks it up, so a
/// plausible-looking wrong name travels silently, which is exactly what the surface exists to prevent.
///
/// Both import forms need an entry. A **leaf** import binds the bare local name (`Uuid` ⇒
/// `std.id.Uuid`); a **group** import binds the module or namespace, and the types reached through
/// it are written dotted (`use std.{id}` ⇒ `id.Uuid` ⇒ `std.id.Uuid`). The loader does not rewrite
/// the dotted form — it α-renames a native `use` handle in the *value* namespace, which is what
/// makes `id.uuid()` resolve and says nothing about a type spelling — so without the entry below
/// `d is id.Uuid` compared the runtime tag against the string `id.Uuid` and answered `false`, and
/// `type_name::<id.Uuid>()` handed back that same key, which nothing is registered under. Silently,
/// and for the one spelling that matches what `type_of` prints.
///
/// The dotted half goes through [`Registry::namespace_types`](noeta_ext_abi::registry::Registry::namespace_types),
/// the projection the checker's own import pass binds these names with — so a spelling that
/// type-checks as an annotation resolves to the same identity here rather than to a second answer.
/// Built from this program's own `use` statements, so a name the program never imported is left
/// exactly as written.
///
/// **A local declaration wins**, the same shadowing rule the loader's own native-type aliasing
/// follows: a name the linked program declares itself is never rewritten to a native type of that
/// name. (A declaration under a `namespace` links to a qualified name and could not collide in the
/// first place; the guard is what covers the un-namespaced case.)
fn collect_native_type_imports(
    program: &AstProgram,
    registry: &'static noeta_ext_abi::registry::Registry,
) -> HashMap<String, String> {
    let declared: HashSet<&str> = program
        .stmts
        .iter()
        .filter_map(|stmt| match stmt {
            AstStmt::Struct(d) => Some(d.name.as_str()),
            AstStmt::Class(d) => Some(d.name.as_str()),
            AstStmt::Enum(d) => Some(d.name.as_str()),
            _ => None,
        })
        .collect();
    let mut map = HashMap::new();
    for stmt in &program.stmts {
        let AstStmt::Use { path, names, .. } = stmt else {
            continue;
        };
        let prefix = path.join(".");
        for n in names {
            let local = n.local();
            if declared.contains(local) {
                continue;
            }
            let qualified = format!("{prefix}.{}", n.name);
            // One probe per native nominal kind, all keyed on the qualified identity the import
            // spells out; a name that resolves to none of them is not a native type and is left as
            // written.
            let resolved = registry
                .find_enum_qualified(&qualified)
                .map(|t| t.qualified())
                .or_else(|| {
                    registry
                        .find_fielded_qualified(&qualified)
                        .map(|t| t.qualified())
                })
                .or_else(|| {
                    registry
                        .find_type_qualified(&qualified)
                        .map(|t| t.qualified())
                });
            if let Some(q) = resolved {
                map.insert(local.to_string(), q);
                continue;
            }
            // Not a type: then the import may be a **module or namespace**, whose types this
            // program spells through it (`use std.{id}` → `id.Uuid`). Bind each of them under that
            // dotted local spelling, through the one `namespace_types` projection the checker's
            // `collect_imports` binds them with — so the name a written `id.Uuid` resolves to here
            // is the name it resolved to there.
            //
            // A dotted key cannot collide with a local declaration (a declared type name has no
            // dot), so the shadowing guard above has nothing to say about these.
            for (rel, qualified_ty) in registry.namespace_types(&qualified) {
                map.insert(format!("{local}.{rel}"), qualified_ty);
            }
        }
    }
    map
}

impl Lowerer<'_> {
    /// Rewrite a narrowing target's [`TypeRef`] so every **written spelling** becomes the name a
    /// value's runtime tag actually carries — recursively, so `List<MyId>` / `?Self` are covered
    /// too. Two spellings differ from the tag: an import **alias** (`MyId` → `Uuid`) and `Self`
    /// (→ the type whose body this is). A no-op (plain clone) when neither is in play, which is the
    /// overwhelmingly common case.
    fn resolve_type_spelling(&self, ty: &TypeRef) -> TypeRef {
        if self.facts.type_aliases.is_empty() && self.self_type_name.is_none() {
            return ty.clone();
        }
        match ty {
            TypeRef::Named { name, args, span } => TypeRef::Named {
                name: self
                    .facts
                    .type_aliases
                    .get(name.as_str())
                    .map(noeta_ast::Name::canonical)
                    .unwrap_or_else(|| {
                        // `x is Self` matches the enclosing type's runtime tag. Applied after the
                        // alias lookup, which cannot hold a `Self` key — a `use … as Self` names a
                        // type `Self`, and declaring one is refused.
                        noeta_ast::Name::canonical(
                            self.resolve_self_spelling(name.as_str().to_string()),
                        )
                    }),
                args: args.iter().map(|a| self.resolve_type_spelling(a)).collect(),
                span: *span,
            },
            TypeRef::Union { members, span } => TypeRef::Union {
                members: members
                    .iter()
                    .map(|m| self.resolve_type_spelling(m))
                    .collect(),
                span: *span,
            },
            TypeRef::Tuple { elements, span } => TypeRef::Tuple {
                elements: elements
                    .iter()
                    .map(|e| self.resolve_type_spelling(e))
                    .collect(),
                span: *span,
            },
            TypeRef::Fn { params, ret, span } => TypeRef::Fn {
                params: params
                    .iter()
                    .map(|p| self.resolve_type_spelling(p))
                    .collect(),
                ret: Box::new(self.resolve_type_spelling(ret)),
                span: *span,
            },
            TypeRef::Optional { inner, span } => TypeRef::Optional {
                inner: Box::new(self.resolve_type_spelling(inner)),
                span: *span,
            },
            // A trait object's trait name resolves like a nominal leaf: a native trait's local
            // `use` spelling (`Widget`, `vec.Kernels`) becomes its qualified identity — the key
            // the shared membership table uses — so the precise `is dyn Trait` test compares one
            // canonical string. A `.noe` trait misses the map and keeps its (loader-qualified)
            // linked name, which is already the table's key.
            TypeRef::DynTrait { trait_name, span } => TypeRef::DynTrait {
                trait_name: self
                    .facts
                    .type_aliases
                    .get(trait_name.as_str())
                    .map(noeta_ast::Name::canonical)
                    .unwrap_or_else(|| trait_name.clone()),
                span: *span,
            },
            // A `Self::Name` projection is never an import alias — resolution is per-impl at the
            // checker (slice 1a).
            TypeRef::AssocProjection { .. } => ty.clone(),
        }
    }

    /// Allocate a fresh frame-local temporary.
    fn fresh(&mut self) -> Temp {
        let t = Temp(self.temps);
        self.temps += 1;
        t
    }

    /// Lower a statement list **hoisting every CAPTURELESS `fn` declaration ahead of the rest** into
    /// `out`, preserving the relative order within each group. This is the ONE hoisting rule that
    /// covers every ordinary lexical scope — the module top level and every nested block body (fn
    /// bodies, `if`/`while`/`for` bodies, statement-`match` arms) all funnel through it — so a
    /// statement may call a `fn` declared textually later in the SAME scope. It is the runtime
    /// counterpart of the checker's forward-reference hoist (`collect.rs` pass 1), and makes a
    /// **direct call before the declaration** resolve, which the backends' scope-sharing
    /// forward-capture (a later block-local `fn` becomes visible to an *already-constructed* sibling
    /// closure, so mutual recursion and closures-invoked-later already worked) does not cover.
    ///
    /// Only a `fn` DECLARATION with **no captures** hoists (see [`is_hoistable_fn`]). Two exclusions:
    ///
    /// * A value binding (`x = 5`, including a closure bound to a name — that is an `AstStmt::Binding`,
    ///   not `AstStmt::Fn`) stays in source order, so using a value before its assignment is still a
    ///   runtime E0005. Hoisting a captureless named `fn` past a value binding is sound precisely
    ///   because a named fn is **sealed** (sealed-fns arc): with no `use (…)` clause its body reads
    ///   only its params/statics, globals, and sibling fns — never a surrounding value binding — so
    ///   moving its declaration earlier cannot change what it observes.
    ///
    /// * A nested `fn` that DOES capture (`fn bump() use (count) { … }`) is left in source order. Its
    ///   `use (…)` upvalues are sourced from the enclosing frame's already-bound local slots when the
    ///   closure is constructed, so hoisting it *above* the binding of a captured local would leave
    ///   the capture unsourceable (the VM compiler rejects it outright). The existing scope-sharing
    ///   forward-capture already makes mutual recursion and later-invocation of capturing fns work
    ///   (both fns are declared before either is called), so leaving them ordered loses nothing that
    ///   worked before. At the module top level every `fn` is captureless (no enclosing frame), so
    ///   this gate is a no-op there and the top-level behavior is unchanged.
    ///
    /// Class/enum/struct declarations do NOT hoist here either (their forward references are settled
    /// by the type-registration fixpoint, not by statement order). The relative order of the hoisted
    /// fns is preserved, as is the relative order of everything else, so destructor-bearing value
    /// bindings destruct in their original reverse-binding order (only a function value's own —
    /// unobservable — teardown moves). Both backends consume this one reordered stream, so they stay
    /// differential-identical by construction; the compiler's slot numbering carries no semantics
    /// (destruction order is tracked from runtime binding order by the post-lowering drop-insertion
    /// pass, which reads this same reordered stream).
    fn lower_stmts_hoisting_fns(
        &mut self,
        stmts: &[AstStmt],
        out: &mut Vec<Stmt>,
    ) -> Result<(), Unsupported> {
        for stmt in hoisted_fn_order(stmts) {
            self.lower_stmt(stmt, out)?;
        }
        Ok(())
    }

    /// Lower a statement-position block of statements (no value), in the current frame, applying the
    /// shared fn-declaration hoist ([`Self::lower_stmts_hoisting_fns`]).
    fn lower_body(&mut self, stmts: &[AstStmt]) -> Result<Block, Unsupported> {
        let mut out = Vec::new();
        self.lower_stmts_hoisting_fns(stmts, &mut out)?;
        Ok(Block::stmts(out))
    }

    /// Lower the module's top-level statement stream. Unified with block-body lowering: the top
    /// level is just another scope with the same fn-declaration hoist ([`Self::lower_body`]).
    fn lower_top_level(&mut self, stmts: &[AstStmt]) -> Result<Block, Unsupported> {
        self.lower_body(stmts)
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
                // A loop over a set/map whose element or key carries a `u64` sorts its snapshot
                // under this hint, so the program sees the order its type states.
                let order = self.order_hint(span);
                let order_slots = self.order_slots(order.as_ref(), *span, out);
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
                            order: order.clone(),
                            order_slots: order_slots.clone(),
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
                        // The synthetic tuple-projection prelude runs first; the user body then
                        // lowers through the shared fn-hoist so a fn called before its decl in the
                        // loop body resolves, exactly as a `Single`-pattern body does via `lower_body`.
                        self.lower_stmts_hoisting_fns(body, &mut body_stmts)?;
                        out.push(Stmt::For {
                            pattern: AstForPattern::Single {
                                name: elem,
                                name_span: *span,
                            },
                            iterable,
                            body: Block::stmts(body_stmts),
                            span: *span,
                            stream,
                            order,
                            order_slots,
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
                    Some(decl.name.to_string()),
                    Some(decl.captures.iter().map(|(n, _)| n.clone()).collect()),
                )?;
                out.push(Stmt::Decl(Decl::Fn {
                    name: decl.name.to_string(),
                    func: Rc::new(func),
                    span: decl.span,
                }));
                Ok(())
            }
            AstStmt::Class(decl) => {
                let (methods, destructor, field_defaults) =
                    self.lower_type_body(&decl.name, |lw| {
                        let methods =
                            lw.lower_type_methods(&decl.name, &decl.methods, &decl.impls)?;
                        // The `destruct` block lowers to a parameterless block [`Func`] (fields
                        // resolve against the receiver, like a method), so the VM can compile it to
                        // a prototype.
                        let destructor = match &decl.destructor {
                            Some(body) => Some(Rc::new(lw.lower_func(
                                &[],
                                BodyKind::Block(body),
                                decl.span,
                                false,
                                false,
                                // The VM's destructor-prototype naming.
                                Some(format!("{}::destruct", decl.name)),
                                // A destructor touches only `self` — fully sealed.
                                Some(Vec::new()),
                            )?)),
                            None => None,
                        };
                        let field_defaults = lw.lower_field_defaults(&decl.fields)?;
                        Ok((methods, destructor, field_defaults))
                    })?;
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
                let methods = self.lower_type_body(&decl.name, |lw| {
                    lw.lower_type_methods(&decl.name, &decl.methods, &decl.impls)
                })?;
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
                let (methods, field_defaults) = self.lower_type_body(&decl.name, |lw| {
                    let methods = lw.lower_type_methods(&decl.name, &decl.methods, &decl.impls)?;
                    let field_defaults = lw.lower_field_defaults(&decl.fields)?;
                    Ok((methods, field_defaults))
                })?;
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
    /// A forwarding-generic call's **type arguments** (poly-values F2b), in slot order, when the
    /// checker recorded slots for this call span: a concrete instantiation passes its interned
    /// table index as an int const; a pass-through reads the enclosing fn's own `$ty` slot.
    ///
    /// These travel in the call node's own `type_args` channel rather than prepended onto the
    /// value arguments, so a forwarding call's parameter positions — and therefore its `supplied`
    /// binding map — are exactly those of the same call without forwarding.
    fn type_arg_atoms(&mut self, span: &Span) -> Vec<Atom> {
        let Some(slots) = self.sites.hidden_arg_sites.get(span) else {
            return Vec::new();
        };
        slots
            .iter()
            .map(|slot| match slot {
                // The one place a `TypeArgIndex` becomes a runtime integer. Everything upstream of
                // here carries the newtype, which is what lets `noeta_check::SITE_POLICIES` tell a
                // table index from the slot ordinals that look just like one.
                noeta_ext_abi::HiddenArg::Table(i) => Atom::Const(Const::Int(i.get() as i64)),
                noeta_ext_abi::HiddenArg::Forward(j) => Atom::Var {
                    name: hidden_param_name(*j),
                    span: *span,
                },
                // A render slot the call site could not name. `NO_TYPE_ARG` indexes nothing, so
                // every consumer resolves it to "no hint" and the value renders as the erased word.
                noeta_ext_abi::HiddenArg::Erased => {
                    Atom::Const(Const::Int(noeta_ext_abi::NO_TYPE_ARG))
                }
            })
            .collect()
    }

    /// The **method-table name** a call site dispatches through: the name the source wrote, unless
    /// the checker resolved it to something else at this span.
    ///
    /// One resolution does that today — `Target.from(x)` on a target declaring several `From`
    /// conversions, where the argument's type selects one of them and the plain `from` names none
    /// ([`LoweringSites::from_call_sites`]). Applied at every form a method call takes, the piped
    /// spellings included, because the choice belongs to the call and not to how it was written.
    fn method_name(&self, span: &Span, written: &str) -> String {
        self.sites
            .from_call_sites
            .get(span)
            .cloned()
            .unwrap_or_else(|| written.to_string())
    }

    /// The **dynamic construction tag** operand for a call span (generic-in-generic construction):
    /// the enclosing body's `$ty<i>` hidden local whose table entry names the instantiation to stamp
    /// on the freshly-built object. `None` at every ordinary call — the overwhelming majority.
    ///
    /// A `Var` reference to a hidden parameter, exactly as [`Self::type_arg_atoms`]'s pass-through
    /// arm builds: a nested `fn` or closure reaches the enclosing slot the same way it reaches any
    /// other local, through closure conversion, so no separate capture rule is needed.
    fn reflect_slot_atom(&self, span: &Span) -> Option<Atom> {
        self.sites
            .dynamic_construction_sites
            .get(span)
            .map(|slot| Atom::Var {
                name: hidden_param_name(*slot),
                span: *span,
            })
    }

    /// Lower everything inside a type's own body — methods, a `destruct` block, field defaults —
    /// with `Self` bound to that type for the duration.
    ///
    /// One scoping site per declaration kind, wrapping the *whole* body rather than the method
    /// loop: a destructor and a field default are as much inside the type as a method is, and a
    /// `Self` in one of them would otherwise fold to the literal word.
    fn lower_type_body<T>(
        &mut self,
        type_name: &noeta_ast::Name,
        body: impl FnOnce(&mut Self) -> Result<T, Unsupported>,
    ) -> Result<T, Unsupported> {
        let saved = self.self_type_name.replace(type_name.to_string());
        let lowered = body(self);
        self.self_type_name = saved;
        lowered
    }

    /// Lower a type's own methods. The three declaration kinds ran identical loops; sharing them is
    /// what keeps their naming and sealing from drifting apart.
    fn lower_type_methods(
        &mut self,
        type_name: &noeta_ast::Name,
        methods: &[FnDecl],
        impls: &[noeta_ast::ImplBlock],
    ) -> Result<Vec<(String, Rc<Func>)>, Unsupported> {
        // A type declaring several `From` conversions carries several `from` bodies, which the
        // parser flattened into `methods` under the one name they were written with. Each takes the
        // key named after the source it converts, so the table both backends build from this list
        // has one entry per conversion — and it is the same key the checker registered the
        // signature under, because both ask [`noeta_ast::conversion::from_conversion_keys`].
        let keys = noeta_ast::conversion::from_conversion_keys(impls);
        let mut out = Vec::with_capacity(methods.len());
        for m in methods {
            let key = keys
                .get(&m.name_span)
                .cloned()
                .unwrap_or_else(|| m.name.to_string());
            let func = self.lower_func(
                &m.params,
                BodyKind::Block(&m.body),
                m.span,
                true,
                m.is_async,
                // Methods trace as `Type.method` (the VM's chunk naming).
                Some(format!("{type_name}.{key}")),
                Some(m.captures.iter().map(|(n, _)| n.clone()).collect()),
            )?;
            out.push((key, Rc::new(func)));
        }
        Ok(out)
    }

    // The lowering inputs for one function/closure body — a bundle, not a signature worth a struct.
    #[allow(clippy::too_many_arguments)]
    fn lower_func(
        &mut self,
        params: &[Param],
        body: BodyKind<'_>,
        span: Span,
        generator: bool,
        is_async: bool,
        name: Option<String>,
        captures: Option<Vec<String>>,
    ) -> Result<Func, Unsupported> {
        let outer = self.temps;
        self.temps = 0;
        // A FORWARDING generic fn (poly-values F2b) carries its type-argument slots as LEADING
        // parameters (`$ty0`, `$ty1`, …) so the body can name them and register allocation places
        // them like any other — but they are filled from the call node's own `type_args` channel,
        // never from its value arguments, and `Func::hidden` is what tells every binder how many
        // leading slots to lay down before the value arguments start. Keyed by the name this
        // callable traces under — a bare `fn` name, or `Type.method` for a forwarding generic
        // method (Axis A) — and only at depth 0: a NESTED fn (D2b) may share a top-level name but
        // never carries slots of its own (it captures the enclosing `$ty` locals instead), and
        // closures never appear in the map.
        let hidden = if self.fn_depth == 0 {
            name.as_deref()
                .and_then(|n| self.sites.forwarding_fns.get(n).copied())
                .unwrap_or(0)
        } else {
            0
        };
        // The slot count a door in this body resolves against. A nested declaration keeps the
        // enclosing one — it reads the same `$ty` locals through capture — so only depth 0 sets it.
        let outer_hidden = self.hidden_slots;
        // …and the receiver-read half of that list (`self_render_fns`), retained the same way: a
        // closure inside a method captures the very `self` its enclosing body reads.
        let outer_self_render = self.self_render_slots;
        if self.fn_depth == 0 {
            self.hidden_slots = hidden;
            self.self_render_slots = name
                .as_deref()
                .and_then(|n| self.sites.self_render_fns.get(n).copied())
                .unwrap_or(0);
        }
        self.fn_depth += 1;
        let mut param_names: Vec<String> = (0..hidden).map(hidden_param_name).collect();
        param_names.extend(params.iter().map(|p| p.name.clone()));
        // Defaults are evaluated in the captured scope at call time, each in its own frame, so
        // lower each as a self-contained thunk (this also restores `self.temps` to 0 between
        // thunks, keeping the body's numbering independent).
        let mut defaults: Vec<Option<Thunk>> = (0..hidden).map(|_| None).collect();
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
                // traces under this function's name (see `synth_step_name`), and it inherits
                // this function's SEAL — the user statements live in the step's body, so its
                // bare-assignment locality must match what the checker typed for the fn.
                self.synth_step_name = name.clone();
                self.synth_step_captures = captures.clone();
                self.lower_generator(stmts, span, &param_names)?
            }
            // An `async fn` (Track A) lowers to a lazy `Future` over its body — `make_future(thunk)` —
            // not the body's statements directly (like a generator, but a single deferred computation
            // rather than a per-element state machine). `is_async` is set only at named-`fn`/method
            // sites, never a closure or the synthesized thunk, so the wrap applies exactly once.
            BodyKind::Block(stmts) if is_async => {
                // As for a generator: the future's step closure is this function's body, and it
                // inherits the seal.
                self.synth_step_name = name.clone();
                self.synth_step_captures = captures.clone();
                self.lower_async(stmts, span, &param_names)?
            }
            BodyKind::Block(stmts) => self.lower_body(stmts)?,
        };
        let temp_count = self.temps;
        self.temps = outer;
        self.fn_depth -= 1;
        self.hidden_slots = outer_hidden;
        self.self_render_slots = outer_self_render;
        Ok(Func {
            name,
            captures,
            params: param_names,
            hidden,
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
    /// The module globals a state machine's body may actually **store** to — what
    /// [`desugar_state_machine`] excludes from cell-hoisting, because a bare `g = …` against one is
    /// a global store rather than a fresh local.
    ///
    /// The seal decides this, exactly as it decides bare-assignment locality everywhere else. A
    /// named `async fn`/generator is SEALED: its body reaches an outer binding only through
    /// `use (…)`, so a bare `x = …` naming anything *not* in that allow-list is a fresh local — a
    /// module global of the same name is unrelated and unreachable. Only an auto-capturing closure
    /// (no armed seal) keeps the outward rule over the whole global set.
    ///
    /// Taking every global as storable regardless — what this did before — made a coroutine's
    /// locality disagree with the synchronous path's, and the disagreement is not confined to one
    /// file: a program's globals include everything the **linker merged in**, so a *dependency
    /// package's* `async fn` had its locals decided against its **consumer's** top-level names. A
    /// package binding `page = render_page()` silently became a store to whatever `page` the
    /// consuming program happened to declare, and the reads that followed loaded that global. The
    /// package author cannot see, predict, or defend against those names.
    fn storable_globals(&self) -> HashSet<String> {
        match &self.synth_step_captures {
            // Sealed: only the globals the function explicitly named in `use (…)`.
            Some(allow) => {
                let allowed: HashSet<&str> = allow.iter().map(String::as_str).collect();
                self.facts
                    .module_globals
                    .iter()
                    .filter(|g| allowed.contains(g.as_str()))
                    .cloned()
                    .collect()
            }
            // An auto-capturing closure keeps the full outward rule.
            None => self.facts.module_globals.clone(),
        }
    }

    fn lower_generator(
        &mut self,
        stmts: &[AstStmt],
        span: Span,
        params: &[String],
    ) -> Result<Block, Unsupported> {
        // Hoist nested `fn` declarations before the body is split into states: the flattener cuts
        // at each `yield`, so a declaration left below one lands in a later state than its callers.
        // Same rule every other scope gets (see [`hoisted_fn_order`]).
        let hoisted: Vec<AstStmt> = hoisted_fn_order(stmts).cloned().collect();
        let storable = self.storable_globals();
        let desugar = desugar_state_machine(
            &hoisted,
            span,
            self.sites.for_stream_sites,
            SuspendMode::Gen,
            &storable,
            params,
            self.sites.variant_pattern_sites,
        );
        // The sealed step closure must keep writing the machine's PERSISTENT locals — the
        // desugar's hoisted prelude cells (`$state`, awaited-future cells, and any USER local
        // that lives across a suspend) — through the enclosing scope. Extend the armed seal
        // with the prelude's binding names so those writes cross the frontier; everything else
        // stays sealed exactly as the checker typed the surface body.
        if let Some(allow) = &mut self.synth_step_captures {
            allow.extend(desugar.prelude.iter().filter_map(|stmt| match stmt {
                AstStmt::Binding { name, .. } => Some(name.clone()),
                _ => None,
            }));
        }

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
    fn lower_async(
        &mut self,
        stmts: &[AstStmt],
        span: Span,
        params: &[String],
    ) -> Result<Block, Unsupported> {
        // Same declaration-hoist the generator gets: the flattener cuts at each `.await`, so a
        // nested `fn` must be declared before the first cut to be visible to every state.
        let hoisted: Vec<AstStmt> = hoisted_fn_order(stmts).cloned().collect();
        let storable = self.storable_globals();
        let desugar = desugar_state_machine(
            &hoisted,
            span,
            self.sites.for_stream_sites,
            SuspendMode::Async,
            &storable,
            params,
            self.sites.variant_pattern_sites,
        );
        // The sealed step closure must keep writing the machine's PERSISTENT locals — the
        // desugar's hoisted prelude cells (`$state`, awaited-future cells, and any USER local
        // that lives across a suspend) — through the enclosing scope. Extend the armed seal
        // with the prelude's binding names so those writes cross the frontier; everything else
        // stays sealed exactly as the checker typed the surface body.
        if let Some(allow) = &mut self.synth_step_captures {
            allow.extend(desugar.prelude.iter().filter_map(|stmt| match stmt {
                AstStmt::Binding { name, .. } => Some(name.clone()),
                _ => None,
            }));
        }

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

    /// Lower a reflection surface's type operand to the **type-name atom** both its surfaces share.
    ///
    /// This is where the turbofish finally becomes a name, and it is deliberately *here* rather than
    /// in the parser: lowering runs on the already-linked program, so the [`TypeRef`] now carries its
    /// **qualified** identity (`app.storage.Todo`) — exactly the key the reflection registry stores
    /// the type under, and exactly what the dynamic string surface would have been handed. Flattening
    /// it at parse time instead put it beyond the linker's namespace rewrite, and
    /// `field_specs_of::<Todo>()` under a `namespace` silently answered with the empty schema.
    ///
    /// One spelling the linker deliberately leaves short is a **leaf**-imported native type
    /// (`use std.http.{Framing}`): the loader aliases only native *attribute* structs there, since
    /// rewriting the rest would also rewrite the value spelling the backends bind under. Those
    /// resolve through the checker instead — so the head name arrives short, while the reflection
    /// artifact keys a native type on the qualified identity `type_of` stamps on its values. Left
    /// alone, `variants_of::<Framing>()` / `field_specs_of::<Frame>()` folded to a name nothing is
    /// registered under and answered the empty list, right after the program imported the type, and
    /// `type_name::<Uuid>()` handed out that unregistered name as a key.
    /// [`Self::native_type_imports`] carries the one rewrite that closes it.
    ///
    /// The one static operand that is NOT a constant is a bare type parameter of an enclosing
    /// generic, which the checker resolved to a per-instantiation channel at `span`: one compiled
    /// body serves every instantiation, so the name arrives per call from
    /// [`Self::type_param_name_atom`] — the same helper `type_name::<T>()` and the narrows read, so
    /// `field_specs_of::<T>()` and `field_specs_of(type_name::<T>())` are the same two instructions
    /// in the same order. Everything downstream is untouched: the rvalue already takes an `Atom`,
    /// so this is the surface's own dynamic arm, reached without the author writing it.
    fn lower_type_operand(
        &mut self,
        operand: &TypeOperand,
        span: &Span,
        out: &mut Vec<Stmt>,
    ) -> Result<Atom, Unsupported> {
        match operand {
            TypeOperand::Static(ty) => Ok(self.static_type_name_atom(ty, span, out)),
            TypeOperand::Dynamic(e) => self.lower_expr(e, out),
        }
    }

    /// The **turbofish half** of [`Self::lower_type_operand`]: the run-time name of a statically
    /// written type — a folded constant, or the per-instantiation channel read when the checker
    /// resolved the head to a type parameter.
    ///
    /// Its own function because `type_name::<T>()` needs exactly this and has no dynamic arm to go
    /// with it, and because "what name does this turbofish denote at run time" is one question with
    /// one answer: `type_name::<T>()` and `field_specs_of::<T>()`'s operand are the same two
    /// instructions in the same order, by construction rather than by two copies agreeing.
    fn static_type_name_atom(&mut self, ty: &TypeRef, span: &Span, out: &mut Vec<Stmt>) -> Atom {
        match self.type_param_name_atom(ty, span, out) {
            Some(atom) => atom,
            None => Atom::Const(Const::Str(self.reflection_head_name(ty))),
        }
    }

    /// **Lower a reflection query** — one dispatch for all thirteen intrinsics, over
    /// [`ReflectKind`].
    ///
    /// The thirteen used to be thirteen arms of `lower_expr`, and they had converged on three
    /// operand resolutions between them: [`Self::lower_type_operand`] for a named type, an ordinary
    /// [`Self::lower_expr`] for a runtime operand, and a checker-recorded site lookup for
    /// `from_bytes`' packed layout. Once the surface admits that, the per-kind work is a
    /// [`ReflectArgs`] shape and nothing else, so it is written once.
    ///
    /// `type_name` is the one kind that emits no [`Rvalue::Reflect`]: it *is* the name-resolution
    /// step the others use as an operand, so it returns that atom directly.
    fn lower_reflect(
        &mut self,
        which: noeta_ast::ReflectKind,
        operand: &noeta_ast::ReflectOperand,
        span: &Span,
        out: &mut Vec<Stmt>,
    ) -> Result<Atom, Unsupported> {
        use noeta_ast::{ReflectKind as K, ReflectOperand as Op};

        // A shape mismatch is a compiler bug — the parser is an `Expr::Reflect`'s only constructor
        // and the census asserts the whole (kind × arm) grid — so it is reported as one, not turned
        // into an `Unsupported` the backends would then have to explain to a user.
        let mismatch = || -> ! {
            panic!(
                "`{}` carries a {:?} operand and the parser is its only constructor",
                which.keyword(),
                which.shape()
            )
        };
        let args = match which {
            // `type_name::<T>()` — a **compile-time constant string**, with no runtime node at all:
            // by the time lowering runs the program is linked, so this `TypeRef` already carries its
            // qualified identity (`app.storage.Todo`) and there is nothing left to look up. Resolved
            // through the same `TypeRef::head_name` the name-keyed queries use, which is what makes
            // the two agree by construction rather than by convention.
            //
            // …unless the checker recognized `T` as a parameter of an ENCLOSING generic (a type's,
            // read off the receiver's reflected tag — Gap B; or a fn's own, read off the hidden
            // type-argument slot — F2b). One compiled body serves every instantiation, so there is
            // no constant to fold: the name arrives per call, from `type_param_name_atom` — the same
            // helper the narrow surfaces read, so `type_name::<T>()` and `v.as<T>()` agree on `T`.
            K::TypeName => {
                let Op::StaticType(ty) = operand else {
                    mismatch()
                };
                return Ok(self.static_type_name_atom(ty, span, out));
            }
            // The name-keyed queries: the turbofish arm folds to a constant name (or reads a
            // per-instantiation channel), the dynamic arm is the operand as written. One helper,
            // so the two surfaces of one query cannot answer differently.
            K::AttributesOf | K::FieldSpecsOf | K::VariantsOf => {
                let Op::Type(ty) = operand else { mismatch() };
                ReflectArgs::One(self.lower_type_operand(ty, span, out)?)
            }
            // The same, with the operand optional: no atom at all for the unscoped `roles_of()`.
            K::RolesOf => match operand {
                Op::Nothing => ReflectArgs::Nothing,
                Op::Type(ty) => ReflectArgs::One(self.lower_type_operand(ty, span, out)?),
                _ => mismatch(),
            },
            // The value queries: one ordinary operand, evaluated as written.
            K::TypeOf | K::FieldsOf | K::TraitsOf | K::ParamsOf | K::ReturnsOf => {
                let Op::Value(e) = operand else { mismatch() };
                ReflectArgs::One(self.lower_expr(e, out)?)
            }
            K::Construct => {
                let Op::TypeWith { ty, arg } = operand else {
                    mismatch()
                };
                let name = self.lower_type_operand(ty, span, out)?;
                let arg = self.lower_expr(arg, out)?;
                ReflectArgs::Two { name, arg }
            }
            K::Invoke => {
                let Op::Dispatch { recv, name, args } = operand else {
                    mismatch()
                };
                let recv = match recv {
                    Some(recv) => Some(self.lower_expr(recv, out)?),
                    None => None,
                };
                let name = self.lower_expr(name, out)?;
                let args = self.lower_expr(args, out)?;
                ReflectArgs::Dispatch { recv, name, args }
            }
            K::FromBytes => {
                let Op::StaticTypeWith { ty: _, arg } = operand else {
                    mismatch()
                };
                let blob = self.lower_expr(arg, out)?;
                // The element layout was recorded by the checker at this span in the same channel
                // list literals use (`packed_list_sites`); `None` means T was not packable (already
                // a checker error), and the backend then fails cleanly rather than mis-decoding.
                let layout = self.sites.packed_list_sites.get(span).cloned();
                // Validation arc: the checker marked this site if `T` implements `Validate`.
                let validate = self.sites.from_bytes_validated.contains(span);
                ReflectArgs::Bytes {
                    blob,
                    layout,
                    validate,
                }
            }
        };
        Ok(self.emit(
            out,
            Rvalue::Reflect {
                which,
                args,
                // Meaningful for `fields_of` alone; every other kind reports no field values, so
                // the checker records no site for one and this stays `false`.
                private_fields: self.sites.fields_of_private.contains(span),
                span: *span,
            },
            *span,
        ))
    }

    /// The **name a reflection surface keys on** for a statically written type: its linked head
    /// name, with a leaf-imported native type's short spelling resolved to the qualified identity
    /// the reflection artifact registers it under (see [`Lowerer::native_type_imports`]).
    ///
    /// Shared by the turbofish operands (`field_specs_of::<T>()`, `variants_of::<T>()`,
    /// `construct::<T>(…)`) and by `type_name::<T>()`, which is the surface whose whole job is to
    /// hand that name to the others — so the two agree by construction rather than by convention,
    /// and `variants_of(type_name::<Framing>())` answers what `variants_of::<Framing>()` does.
    fn reflection_head_name(&self, ty: &TypeRef) -> String {
        let name = self.resolve_self_spelling(ty.head_name());
        self.facts
            .native_type_imports
            .get(name.as_str())
            .cloned()
            .unwrap_or(name)
    }

    /// `Self` written as a type's **head** denotes the type whose body is being lowered.
    ///
    /// Both surfaces that turn a written head into a runtime name go through here — the reflection
    /// queries ([`Self::reflection_head_name`]) and the narrows ([`Self::resolve_type_spelling`]) —
    /// because they are asking one question, and an answer given to only one of them is the shape
    /// where `type_name::<Self>()` reports `Todo` while `x is Self` matches nothing.
    ///
    /// Outside any type body `Self` names nothing and is passed through unchanged; the checker has
    /// already refused it there (E0013), so there is no reachable program this decides for.
    fn resolve_self_spelling(&self, name: String) -> String {
        match &self.self_type_name {
            Some(ty) if name == noeta_ast::SELF_TYPE => ty.clone(),
            _ => name,
        }
    }

    /// The **run-time name atom** of a target type whose head the checker resolved to a
    /// per-instantiation channel at `span` — `Some` exactly for a bare type parameter of an
    /// enclosing generic, `None` for every statically-written type (which stays a folded constant).
    ///
    /// One helper over three surfaces, because there is one fact — what `T` *is* here.
    /// `type_name::<T>()` answers with it, and `v.as<T>()` / `v is T` match on it; routing all three
    /// through this function is what makes them agree by construction rather than by convention,
    /// which is the whole reason the narrow works: `Expr::As` is a head-constructor match on a
    /// name, and this is the name.
    ///
    /// The two channels, mirroring the checker's [`Sites::self_type_arg_sites`] /
    /// [`Sites::forwarded_slot_sites`] split: a generic TYPE's parameter travels on the receiver's
    /// reflected type tag (read off `self` — a generic fn has no receiver), and a generic FN's or
    /// METHOD's own parameter travels in the hidden `$ty<i>` slot that also carries a forwarded
    /// decode recipe, of which this reads only the name.
    fn type_param_name_atom(
        &mut self,
        ty: &TypeRef,
        span: &Span,
        out: &mut Vec<Stmt>,
    ) -> Option<Atom> {
        if let Some((owner, index)) = self.sites.self_type_arg_sites.get(span).cloned() {
            return Some(self.emit(
                out,
                Rvalue::TypeArgName {
                    operand: Atom::Var {
                        name: "self".to_string(),
                        span: *span,
                    },
                    index,
                    type_name: owner,
                    param: ty.head_name(),
                    span: *span,
                },
                *span,
            ));
        }
        if let Some(&slot) = self.sites.forwarded_slot_sites.get(span) {
            return Some(self.emit(
                out,
                Rvalue::TypeSlotName {
                    slot: Atom::Var {
                        name: hidden_param_name(slot),
                        span: *span,
                    },
                    span: *span,
                },
                *span,
            ));
        }
        None
    }

    /// Lower an expression to an [`Atom`], emitting the `let`s that compute any
    /// sub-expressions into `out` first (A-normal form). Literals and identifiers reduce
    /// directly to an atom with no `let`.
    ///
    /// An expression the checker marked as a **display site carrying an unsigned 64-bit integer**
    /// (`render_hint_sites`) is wrapped here in an [`Rvalue::Render`], so the value reaches its
    /// `echo` / interpolation hole / `~` operand already rendered — the one place the wrap happens,
    /// for all three doors, because "which expressions are display sites" is the checker's answer
    /// and this is where an expression becomes an atom.
    fn lower_expr(&mut self, expr: &Expr, out: &mut Vec<Stmt>) -> Result<Atom, Unsupported> {
        let atom = self.lower_expr_unrendered(expr, out)?;
        let Some(hint) = self.sites.render_hint_sites.get(&expr.span()).cloned() else {
            return Ok(atom);
        };
        let span = expr.span();
        let slots = self.hint_slots(&hint, span, out);
        Ok(self.emit(
            out,
            Rvalue::Render {
                operand: atom,
                hint: std::rc::Rc::new(hint),
                slots,
                span,
            },
            span,
        ))
    }

    /// The enclosing body's render-slot operands, for a hint that mentions a
    /// [`noeta_ast::RenderHint::Param`] — and an **empty vector** for every hint that does not,
    /// which is every door outside a generic body.
    ///
    /// The whole slot list, in slot order, so a `Param(n)` indexes it directly. The leading
    /// [`Self::hidden_slots`] are ordinary [`Atom::Var`] reads of the `$ty<i>` locals — the same
    /// reads [`Self::type_arg_atoms`]'s pass-through arm and [`Self::reflect_slot_atom`] build —
    /// which is what makes a closure capture them and a register allocator leave them alone. The
    /// [`Self::self_render_slots`] after them are reads of the **receiver's** reflected tag
    /// ([`Rvalue::SelfRenderSlot`]), emitted into `out` here so the ordinary operand machinery
    /// carries them for the same three analyses.
    fn hint_slots(
        &mut self,
        hint: &noeta_ast::RenderHint,
        span: Span,
        out: &mut Vec<Stmt>,
    ) -> Vec<Atom> {
        if !hint.has_param() {
            return Vec::new();
        }
        let mut slots: Vec<Atom> = (0..self.hidden_slots)
            .map(|i| Atom::Var {
                name: hidden_param_name(i),
                span,
            })
            .collect();
        for index in 0..self.self_render_slots {
            let atom = self.emit(
                out,
                Rvalue::SelfRenderSlot {
                    operand: Atom::Var {
                        name: "self".to_string(),
                        span,
                    },
                    index,
                    span,
                },
                span,
            );
            slots.push(atom);
        }
        slots
    }

    /// [`Self::hint_slots`] for an optional hint — the shape the two ordering doors hold theirs in.
    fn order_slots(
        &mut self,
        hint: Option<&std::rc::Rc<noeta_ast::RenderHint>>,
        span: Span,
        out: &mut Vec<Stmt>,
    ) -> Vec<Atom> {
        match hint {
            Some(h) => {
                let h = h.clone();
                self.hint_slots(&h, span, out)
            }
            None => Vec::new(),
        }
    }

    /// [`Self::order_slots`] for the ordering hint recorded at `span` — the form a method call's
    /// node literal asks for, beside its `order: self.order_hint(span)`.
    fn order_slots_at(&mut self, span: &Span, out: &mut Vec<Stmt>) -> Vec<Atom> {
        match self.sites.order_hint_sites.get(span).cloned() {
            Some(hint) => self.hint_slots(&hint, *span, out),
            None => Vec::new(),
        }
    }

    /// The ordering hint the checker recorded at `span` (`order_hint_sites`), shared as an `Rc` so
    /// the backends' walks borrow it rather than clone the tree per call.
    fn order_hint(&self, span: &Span) -> Option<std::rc::Rc<noeta_ast::RenderHint>> {
        self.sites
            .order_hint_sites
            .get(span)
            .map(|h| std::rc::Rc::new(h.clone()))
    }

    /// The push hint the checker recorded at `span` (`binding_hint_sites`) — the deferred twin of
    /// [`Self::order_hint`], shared as an `Rc` for the same reason.
    fn push_hint(&self, span: &Span) -> Option<std::rc::Rc<noeta_ast::RenderHint>> {
        self.sites
            .binding_hint_sites
            .get(span)
            .map(|h| std::rc::Rc::new(h.clone()))
    }

    /// [`Self::hint_slots`] for the push hint recorded at `span` — the operands a kept hint is
    /// spliced against **at the binding call**, since the tick that serializes has no frame.
    fn push_slots(&mut self, span: &Span, out: &mut Vec<Stmt>) -> Vec<Atom> {
        match self.sites.binding_hint_sites.get(span).cloned() {
            Some(hint) => self.hint_slots(&hint, *span, out),
            None => Vec::new(),
        }
    }

    fn lower_expr_unrendered(
        &mut self,
        expr: &Expr,
        out: &mut Vec<Stmt>,
    ) -> Result<Atom, Unsupported> {
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
            // `Repo::<Todo>` — a type reference carrying an explicit instantiation. Generics are
            // erased at runtime and the instantiation already reached the value at check time (the
            // construction-site tag recorded at the enclosing call's span), so the node is
            // TRANSPARENT here: it lowers as the type reference it wraps, and `Repo::<Todo>.new(x)`
            // emits byte-for-byte what `Repo.new(x)` emits.
            Expr::InstantiatedType { recv, .. } => self.lower_expr(recv, out),
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
            // A FORWARDING generic fn used as a VALUE (poly-deferrals D2c): the checker resolved
            // the instantiation's hidden slots at this span — wrap the reference in a synthesized
            // closure `($fv0, …) => name($fv0, …)` whose inner call carries THIS span, so
            // `type_arg_atoms` binds the resolved atoms into the value (a partial
            // application over the type-argument slots; a `Forward` slot captures the enclosing
            // `$ty` local like any closure upvalue). The inner callee gets a zero-width span so
            // it cannot re-trigger this arm.
            Expr::Ident { name, span } if matches!(self.sites.fn_value_sites.get(span), Some((n, _)) if n == name) =>
            {
                let (fname, arity) = self.sites.fn_value_sites[span].clone();
                let callee_span = Span {
                    start: span.start,
                    end: span.start,
                    source: span.source,
                };
                let params: Vec<Param> = (0..arity)
                    .map(|i| Param {
                        attrs: Vec::new(),
                        name: format!("$fv{i}"),
                        name_span: *span,
                        ty: None,
                        default: None,
                        span: *span,
                        positional: false,
                    })
                    .collect();
                let call = Expr::Call {
                    callee: Box::new(Expr::Ident {
                        name: noeta_ast::Name::canonical(fname),
                        span: callee_span,
                    }),
                    args: params
                        .iter()
                        .map(|p| {
                            noeta_ast::CallArg::positional(Expr::Ident {
                                name: noeta_ast::Name::canonical(p.name.clone()),
                                span: *span,
                            })
                        })
                        .collect(),
                    span: *span,
                };
                // Anonymous like any closure — naming it after the fn would make the wrapper
                // adopt the fn's hidden parameters at top level (the `forwarding_fns` lookup is
                // name-keyed); the inner call still traces under the real fn.
                // A synthetic wrapper has no explicit capture clause.
                let func = self.lower_func(
                    &params,
                    BodyKind::Arrow(&call),
                    *span,
                    false,
                    false,
                    None,
                    None,
                )?;
                Ok(self.emit(
                    out,
                    Rvalue::Closure {
                        func: Rc::new(func),
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Ident { name, span } => Ok(Atom::Var {
                name: name.to_string(),
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
                // A **JSON door** whose serialized value carries an unsigned 64-bit integer: the
                // checker recorded this call span, whichever way its callee was spelled — a
                // qualified `json.stringify(v)`, a selectively-imported `stringify(v)`, a resolved
                // native forward (a `@derive(Inspect)` body), or a derived `v.to_json()`. Emit the
                // hinted serializer over the one value being serialized, so the erased words reach
                // the wire unsigned instead of as their signed reinterpretation. Decided before the
                // callee shape is examined at all, because the door is the *call*, not the spelling:
                // only `to_json` serializes its receiver, and every other form its single argument.
                if let Some(hint) = self.sites.json_hint_sites.get(span).cloned() {
                    let operand = match callee.as_ref() {
                        // A receiver-serializing door (`v.to_json()`, `v.inspect()`) takes no
                        // arguments; every other spelling passes the value as its one argument.
                        Expr::Member { receiver, .. } if args.is_empty() => {
                            self.lower_expr(receiver, out)?
                        }
                        _ => {
                            let (mut arg_atoms, _supplied) = self.lower_args(args, *span, out)?;
                            arg_atoms
                                .pop()
                                .expect("a hinted serializing call has its one argument")
                        }
                    };
                    // The slot reads a receiver-channel hint needs are emitted BEFORE the door
                    // that consumes them, into the same statement list.
                    let slots = self.hint_slots(&hint, *span, out);
                    return Ok(self.emit(
                        out,
                        Rvalue::JsonRender {
                            operand,
                            hint: std::rc::Rc::new(hint),
                            slots,
                            span: *span,
                        },
                        *span,
                    ));
                }
                // The async desugar's single-poll primitive (`$poll(future)`, Track A.3) — a synthetic
                // name (lexer-forbidden `$`, no source collision) the state machine emits at each
                // `.await`. Lowers to the dedicated poll rvalue (`some(v)`/`none`).
                if let Expr::Ident { name, .. } = callee.as_ref()
                    && name == POLL_FN
                    && let [arg] = args.as_slice()
                {
                    let future = self.lower_expr(&arg.value, out)?;
                    return Ok(self.emit(
                        out,
                        Rvalue::PollFuture {
                            future,
                            span: *span,
                        },
                        *span,
                    ));
                }
                // The A.7 nested-`concurrent` desugar's scope primitives (synthetic `$`-names the async
                // state machine emits when it splits a `concurrent { }` block — see `state_machine.rs`).
                // `$scope_begin()` opens a scope and yields its index; `$scope_ready(idx)` is the join
                // poll-state's readiness test; `$scope_end()` closes the drained scope (`Stmt::ScopeEnd`,
                // whose join is a no-op here since the poll-states already drained it, then pops).
                if let Expr::Ident { name, .. } = callee.as_ref() {
                    if name == SCOPE_BEGIN_FN && args.is_empty() {
                        return Ok(self.emit(out, Rvalue::ScopeBegin { span: *span }, *span));
                    }
                    if name == SCOPE_READY_FN
                        && let [arg] = args.as_slice()
                    {
                        let scope = self.lower_expr(&arg.value, out)?;
                        return Ok(self.emit(
                            out,
                            Rvalue::ScopeReady { scope, span: *span },
                            *span,
                        ));
                    }
                    if name == SCOPE_END_FN
                        && let [arg] = args.as_slice()
                    {
                        let scope = self.lower_expr(&arg.value, out)?;
                        // Effect-only (closes the scope); no value binding — `Stmt::Eval`, not a `let`.
                        out.push(Stmt::Eval {
                            rvalue: Rvalue::ScopeEndAt { scope, span: *span },
                            span: *span,
                        });
                        return Ok(Atom::Const(Const::Unit));
                    }
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
                        let (mut arg_atoms, _supplied) = self.lower_args(args, *span, out)?;
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
                    // `T.m(args)` — the receiver is a bounded TYPE PARAMETER, which names a type
                    // only at run time. Rewritten into the by-name dispatch the runtime already
                    // performs and re-lowered; the receiver is not lowered as a value, because
                    // there is no value to lower. Matched on the recorded SPELLING so the rewrite's
                    // own `Type.Named(…)` member call — which lands on this very span — cannot
                    // re-enter it.
                    if let Some(param) = self.sites.type_param_assoc_sites.get(span)
                        && let Expr::Ident {
                            name: tn,
                            span: rspan,
                        } = receiver.as_ref()
                        && tn.as_str() == param
                    {
                        let values: Vec<Expr> = noeta_ast::CallArg::values(args).cloned().collect();
                        let rewritten = type_param_assoc_call(param, name, &values, *span, *rspan);
                        return self.lower_expr(&rewritten, out);
                    }
                    let receiver = self.lower_expr(receiver, out)?;
                    // A field-call site (`obj.f(args)` where the checker resolved `f` to a FIELD of
                    // the receiver's type): the field-access-then-call desugar — load the field,
                    // then call the loaded value, exactly the `Field` + `Call` sequence the
                    // spelled-out `g = obj.f; g(args)` lowers to. The field is loaded **before**
                    // the arguments, matching the `(obj.f)(args)` callee-first evaluation order.
                    if self.sites.field_call_sites.contains(span) {
                        let callee = self.emit(
                            out,
                            Rvalue::Field {
                                receiver,
                                name: name.clone(),
                                name_span: *name_span,
                                span: *span,
                            },
                            *span,
                        );
                        let (arg_atoms, supplied) = self.lower_args(args, *span, out)?;
                        let type_args = self.type_arg_atoms(span);
                        return Ok(self.emit(
                            out,
                            Rvalue::Call {
                                callee,
                                args: arg_atoms,
                                type_args,
                                supplied,
                                span: *span,
                            },
                            *span,
                        ));
                    }
                    let (arg_atoms, supplied) = self.lower_args(args, *span, out)?;
                    // The receiver's dedicated forms below — `WidthIntMethod`, `TraitMethod`,
                    // `DecodeTyped` — carry no mask, because the intrinsics and trait methods they
                    // route to declare no defaulted parameters, so no call of one can leave a hole.
                    // Asserted rather than assumed: if that ever stops holding, a dropped mask would
                    // place arguments on the wrong parameters silently, which is precisely the class
                    // of failure this whole change exists to remove.
                    debug_assert!(
                        supplied.is_none()
                            || !(self.sites.width_sites.contains_key(span)
                                || self.sites.trait_call_sites.contains_key(span)
                                || self.sites.decode_typed_sites.contains(span)),
                        "a call with a skipped parameter lowered to a form that cannot carry its \
                         supplied-mask"
                    );
                    // An int method that needs the receiver's static width (Tier W5): the checker
                    // marked this call span in `width_sites`. Emit the width-carrying
                    // `WidthIntMethod` rather than the generic `Method` (which would compute on the
                    // full erased i64) — a bit intrinsic computes within the width, a range-checked
                    // conversion reads the erased word by the receiver's signedness. A total
                    // `Convert` (`to_*`) needs neither and stays an ordinary method.
                    if let Some(&(recv_signed, bits)) = self.sites.width_sites.get(span)
                        && let Some(method) = noeta_ext_abi::IntMethod::from_name(name)
                        && !matches!(method, noeta_ext_abi::IntMethod::Convert { .. })
                    {
                        // A range-checked conversion decoded the plain-`int` source default from its
                        // name; here the receiver's real signedness is known, and it decides how the
                        // erased word is read (a `u64` above `i64::MAX` carries a negative one).
                        let method = match method {
                            noeta_ext_abi::IntMethod::CheckedConvert { signed, bits, .. } => {
                                noeta_ext_abi::IntMethod::CheckedConvert {
                                    src_signed: recv_signed,
                                    signed,
                                    bits,
                                }
                            }
                            other => other,
                        };
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
                    // A trait method call: a native trait's defaulted method with a trait-level dispatch
                    // and no overriding implementor (slice 2), OR — since the ExtBundle→ExtTrait fold-in
                    // (slice 4) — a kernel-trait method (`impl vec.Kernels for T {}`). Bake the
                    // `(trait, method)` route in, receiver as slot 0; the runtime dispatches both through
                    // `Registry::dispatch_trait_method` (the bundle runtime route was unified onto it).
                    if let Some((trait_name, _method)) = self.sites.trait_call_sites.get(span) {
                        return Ok(self.emit(
                            out,
                            Rvalue::TraitMethod {
                                receiver,
                                trait_name: trait_name.clone(),
                                name: name.clone(),
                                args: arg_atoms,
                                span: *span,
                            },
                            *span,
                        ));
                    }
                    // A call of a FORWARDING generic METHOD (Axis A) supplies its type arguments
                    // through their own channel, exactly as a free function's call does — so
                    // `supplied` still indexes the value parameters.
                    let type_args = self.type_arg_atoms(span);
                    let reflect = self.sites.construction_sites.get(span).cloned();
                    // …and its dynamic twin: a generic type's fresh constructor whose instantiation
                    // is the enclosing self-less member's own type parameter, delivered on a slot.
                    let reflect_slot = self.reflect_slot_atom(span);
                    // Emitted into `out` before the call node that consumes them: a receiver-read
                    // render slot is an ordinary computation, not an operand read off the frame.
                    let order_slots = self.order_slots_at(span, out);
                    let push_slots = self.push_slots(span, out);
                    Ok(self.emit(
                        out,
                        Rvalue::Method {
                            receiver,
                            name: self.method_name(span, name),
                            name_span: *name_span,
                            args: arg_atoms,
                            reuse: false,
                            // Generic enum-variant construction records its type here (R2b.2); an
                            // ordinary method-call span is not a construction site.
                            reflect,
                            reflect_slot,
                            type_args,
                            supplied,
                            order: self.order_hint(span),
                            order_slots,
                            push: self.push_hint(span),
                            push_slots,
                            span: *span,
                        },
                        *span,
                    ))
                } else {
                    let callee = self.lower_expr(callee, out)?;
                    let (arg_atoms, supplied) = self.lower_args(args, *span, out)?;
                    // A call of a FORWARDING generic (F2b) supplies its type arguments through
                    // their own channel, so the value arguments — and `supplied` — are untouched.
                    let type_args = self.type_arg_atoms(span);
                    Ok(self.emit(
                        out,
                        Rvalue::Call {
                            callee,
                            args: arg_atoms,
                            type_args,
                            supplied,
                            span: *span,
                        },
                        *span,
                    ))
                }
            }
            // `f::<T, ...>(args)` (poly-values F2): the explicit instantiation is a checker-only
            // fact — generics are erased — so this lowers exactly as the plain `f(args)` does: an
            // ordinary call of the named function. The compiler's direct-global fast path applies
            // unchanged (the callee is the same `Var` atom).
            Expr::TypedCall {
                name,
                name_span,
                args,
                span,
                ..
            } => {
                let callee = Atom::Var {
                    name: name.to_string(),
                    span: *name_span,
                };
                let (arg_atoms, supplied) = self.lower_args(args, *span, out)?;
                // A call of a FORWARDING generic (F2b) supplies its type arguments through their
                // own channel. `supplied` therefore survives verbatim: it indexes the VALUE
                // parameters, and those no longer shift. While the type arguments rode in the
                // argument list this had to be thrown away at every forwarding call site, so a
                // forwarding call could not use a named argument that skipped a default.
                let type_args = self.type_arg_atoms(span);
                Ok(self.emit(
                    out,
                    Rvalue::Call {
                        callee,
                        args: arg_atoms,
                        type_args,
                        supplied,
                        span: *span,
                    },
                    *span,
                ))
            }
            // `recv.m::<U, ...>(args)` (generic methods, D3): the explicit instantiation is a
            // checker-only fact — the method's own type parameters are erased — so this lowers
            // EXACTLY as the plain `recv.m(args)` method call does, and the desugared call keeps
            // THIS span, so a forwarding method's resolved type-argument slots (keyed on that
            // span) are picked up either way: an inferred instantiation and a spelled one produce
            // the same node. Rebuild the equivalent
            // member-call `Expr` at the same span and reuse the one method-dispatch path (instance
            // → `Rvalue::Method`, `Type.assoc` → the associated-call lowering), so every
            // site-keyed dispatch decision is shared by construction.
            Expr::TypedMethodCall {
                recv,
                name,
                name_span,
                args,
                span,
                ..
            } => {
                // A call-site-typed EXTERN method (http arc H8) keeps its turbofish: the checker
                // recorded a recipe here, so the native typed dispatch runs. Every other turbofish
                // method call is an erased user-generic instantiation.
                if self.sites.typed_method_call_sites.contains_key(span) {
                    return self.lower_typed_method_call(recv, name, args, *span, out);
                }
                let desugared = Expr::Call {
                    callee: Box::new(Expr::Member {
                        receiver: recv.clone(),
                        name: name.clone(),
                        name_span: *name_span,
                        span: *span,
                    }),
                    args: args.clone(),
                    span: *span,
                };
                self.lower_expr(&desugared, out)
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
                        self.facts
                            .expr_tiers
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
                            name: noeta_ast::Name::canonical("panic"),
                            span: *tier_span,
                        }),
                        args: vec![noeta_ast::CallArg::positional(Expr::Str {
                            value: format!(
                                "`@{tier}` is not an expression tier — its blocks are not values"
                            ),
                            span: *span,
                        })],
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
                    // The guard lowers to a lazily-evaluated value block (tail = the bool), run
                    // by the backends only after the pattern matches — like a `while` condition,
                    // it must not be hoisted into the pre-computed statement stream.
                    let guard = arm
                        .guard
                        .as_ref()
                        .map(|g| {
                            Ok::<_, Unsupported>(crate::Guard {
                                block: self.lower_value_block(g)?,
                                span: g.span(),
                            })
                        })
                        .transpose()?;
                    // An expression arm's value block yields its value; a statement-block arm
                    // (aether F1) lowers its statements in the SAME frame (so `return` exits the
                    // enclosing function) and yields `unit`.
                    let body = match &arm.body {
                        noeta_ast::ClosureBody::Expr(e) => self.lower_value_block(e)?,
                        noeta_ast::ClosureBody::Block(stmts) => {
                            let mut out = Vec::new();
                            // Same shared fn-hoist as any other block body (a statement-`match` arm
                            // is a scope): a fn called before its decl in the arm resolves.
                            self.lower_stmts_hoisting_fns(stmts, &mut out)?;
                            // A block arm's value is `unit` — an explicit tail atom, so both
                            // backends' "arm writes the match result" paths stay uniform.
                            Block {
                                stmts: out,
                                tail: Some(Atom::Const(Const::Unit)),
                            }
                        }
                    };
                    ir_arms.push(crate::Arm {
                        // A bare identifier the checker resolved to a payload-free variant becomes
                        // the variant test it reads as — the ONE place that rewrite happens, so
                        // both backends consume ordinary IR and neither knows the form exists.
                        pattern: resolve_variant_patterns(
                            &arm.pattern,
                            self.sites.variant_pattern_sites,
                        ),
                        guard,
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
                // A `?`-conversion site (error-ergonomics): the checker resolved this span's `Err`
                // payload to convert through `Target.from` (`impl From<Source>` on the enclosing
                // function's error type). Rewrite the operand to a synthetic
                // `match v { Ok($t) => Ok($t), Err($t) => Err(Target.from($t)) }` — built as AST
                // and lowered through the ordinary match/call paths (the `TierExpr` construction
                // pattern), so both backends and the drop/reuse passes see plain IR and the
                // conversion is identical by construction. The ordinary propagation then applies
                // unchanged to the converted `Result`.
                let operand = match self.sites.try_conversion_sites.get(span) {
                    Some((target, method)) => {
                        let converted = try_conversion_match(expr, target, method, *span);
                        self.lower_expr(&converted, out)?
                    }
                    None => self.lower_expr(expr, out)?,
                };
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
                let (arg_atoms, _supplied) = self.lower_args(args, *span, out)?;
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
            // A narrow is a head-constructor match on the target's runtime NAME, so a target that is
            // an enclosing generic's type parameter needs exactly what `type_name::<T>()` reads and
            // nothing more — the same helper supplies it, and the backends match on that string
            // instead of the erased letter `T` (which nothing is ever registered under).
            Expr::As { expr, ty, span } => {
                let operand = self.lower_expr(expr, out)?;
                let dynamic = self.type_param_name_atom(ty, span, out);
                Ok(self.emit(
                    out,
                    Rvalue::As {
                        operand,
                        ty: self.resolve_type_spelling(ty),
                        dynamic,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::TypeTest { expr, ty, span } => {
                // A test the checker already answered (`Sites::folded_type_tests` — an erased-width
                // target on a scrutinee whose static type fixes the width) becomes its constant.
                // The scrutinee is still lowered: `f() is i32` must still call `f`. Its temp is
                // released right here, exactly as a statement-position expression's is, since
                // nothing downstream reads it.
                if let Some(&answer) = self.sites.folded_type_tests.get(span) {
                    let operand = self.lower_expr(expr, out)?;
                    if let Atom::Temp(t) = operand {
                        out.push(Stmt::Drop(t));
                    }
                    return Ok(Atom::Const(Const::Bool(answer)));
                }
                let operand = self.lower_expr(expr, out)?;
                let dynamic = self.type_param_name_atom(ty, span, out);
                Ok(self.emit(
                    out,
                    Rvalue::TypeTest {
                        operand,
                        ty: self.resolve_type_spelling(ty),
                        dynamic,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::TypedModuleCall {
                recv,
                func,
                func_span,
                args,
                span,
                ..
            } => {
                // A generic-METHOD turbofish (D3) that shares the `ident.func::<T>(args)` surface —
                // the checker resolved the receiver to a value or a user type, not a native module —
                // lowers EXACTLY as the plain `recv.func(args)` method call does (the method's own
                // type parameter is erased and never forwards). Rebuild the equivalent member call
                // at the same span and reuse the one method-dispatch path.
                //
                // …UNLESS the checker recorded a typed-extern-method recipe here (http arc H8):
                // `resp.json::<User>()` shares this same bare-ident-receiver surface, and a
                // recorded recipe is exactly the signal that it is a native typed call, not an
                // erased instantiation. Checked first, because the erasure marker is also set.
                if self.sites.typed_method_call_sites.contains_key(span) {
                    return self.lower_typed_method_call(recv, func, args, *span, out);
                }
                if self.sites.member_method_call_sites.contains(span) {
                    let desugared = Expr::Call {
                        callee: Box::new(Expr::Member {
                            receiver: recv.clone(),
                            name: func.clone(),
                            name_span: *func_span,
                            span: *span,
                        }),
                        args: args.clone(),
                        span: *span,
                    };
                    return self.lower_expr(&desugared, out);
                }
                let module = match recv.as_ref() {
                    Expr::Ident { name, .. } => name.to_string(),
                    _ => String::new(),
                };
                let args = args
                    .iter()
                    .map(|a| self.lower_expr(&a.value, out))
                    .collect::<Result<Vec<_>, _>>()?;
                // The recipe was resolved by the checker at this span (the same channel the other
                // typed sites use); `None` means `T` had no decoding (already a checker error) —
                // or (F2b) that the turbofish was a FORWARDED type parameter, in which case the
                // instantiation's table index arrives dynamically through the enclosing fn's
                // hidden slot local (`$ty<i>`).
                let recipe = self.sites.typed_module_call_sites.get(span).cloned();
                let dynamic = self
                    .sites
                    .forwarded_slot_sites
                    .get(span)
                    .map(|&i| Atom::Var {
                        name: hidden_param_name(i),
                        span: *span,
                    });
                Ok(self.emit(
                    out,
                    Rvalue::TypedModuleCall {
                        module,
                        func: func.clone(),
                        args,
                        recipe,
                        dynamic,
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
            // **The whole reflection surface, one arm.** Every operand resolution the thirteen
            // queries do is now the same three cases (`lower_type_operand`, an ordinary expression,
            // a checker-recorded layout), so what used to be thirteen near-identical arms is one
            // dispatch — see `lower_reflect`.
            Expr::Reflect {
                which,
                operand,
                span,
            } => self.lower_reflect(*which, operand, span, out),
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
                // A synthesized step closure inherits its fn's seal; a user closure is `None`
                // (auto-capturing).
                let captures = self.synth_step_captures.take();
                let func =
                    self.lower_func(params, body_kind, *span, false, false, name, captures)?;
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
                // A target-typed `.{ … }` carries no name in the AST; the checker resolved it from
                // the expected type and left it here. Reading it back at this one point is what
                // keeps `Rvalue::Object` a plain named construction, so every backend below IR is
                // untouched by the form's existence. An absent entry means the checker rejected the
                // literal (E0023) and already reported — lowering only ever runs on a checked
                // program, so the empty name cannot reach a backend.
                let type_name = lit
                    .type_name
                    .as_ref()
                    .map(noeta_ast::Name::to_string)
                    .or_else(|| self.sites.inferred_object_types.get(&lit.span).cloned())
                    .unwrap_or_default();
                Ok(self.emit(
                    out,
                    Rvalue::Object {
                        type_name,
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

    /// Lower `left |> right`, desugaring to a call/method with `left` threaded as an argument —
    /// mirroring `eval_pipeline`. `left` is evaluated first (matching the tree-walker), then the
    /// callee/receiver, then any remaining arguments.
    ///
    /// Which *parameter* the piped value ends up in is the checker's answer, not this pass's: a
    /// labelled right-hand side (`x |> f(b: 1)`) records a binding at the call span exactly as a
    /// labelled direct call does, and [`Self::permute_args`] applies it here. The desugared
    /// argument list this builds — piped value first, written arguments after — is the same list
    /// the checker bound, so the two agree by construction.
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
                // A **JSON door** reached through a pipe (`value |> json.stringify()`): the piped
                // value is the serializer's one argument, already lowered as `left_atom`. Same site
                // map and same emitted form as the written-out call — a spelling must not decide
                // whether a `u64` reaches the wire correctly.
                if let Some(hint) = self.sites.json_hint_sites.get(span).cloned()
                    && args.is_empty()
                {
                    let slots = self.hint_slots(&hint, *span, out);
                    return Ok(self.emit(
                        out,
                        Rvalue::JsonRender {
                            operand: left_atom,
                            hint: std::rc::Rc::new(hint),
                            slots,
                            span: *span,
                        },
                        *span,
                    ));
                }
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
                            arg_atoms.push(self.lower_expr(&a.value, out)?);
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
                        arg_atoms.push(self.lower_expr(&a.value, out)?);
                    }
                    let (arg_atoms, supplied) = self.permute_args(arg_atoms, *span);
                    let type_args = self.type_arg_atoms(span);
                    let reflect = self.sites.construction_sites.get(span).cloned();
                    // …and its dynamic twin: a generic type's fresh constructor whose instantiation
                    // is the enclosing self-less member's own type parameter, delivered on a slot.
                    let reflect_slot = self.reflect_slot_atom(span);
                    // Emitted into `out` before the call node that consumes them: a receiver-read
                    // render slot is an ordinary computation, not an operand read off the frame.
                    let order_slots = self.order_slots_at(span, out);
                    let push_slots = self.push_slots(span, out);
                    Ok(self.emit(
                        out,
                        Rvalue::Method {
                            receiver,
                            name: self.method_name(span, name),
                            name_span: *name_span,
                            args: arg_atoms,
                            reuse: false,
                            // Generic enum-variant construction records its type here (R2b.2); an
                            // ordinary method-call span is not a construction site.
                            reflect,
                            reflect_slot,
                            type_args,
                            supplied,
                            order: self.order_hint(span),
                            order_slots,
                            push: self.push_hint(span),
                            push_slots,
                            span: *span,
                        },
                        *span,
                    ))
                } else {
                    let callee = self.lower_expr(callee, out)?;
                    let mut arg_atoms = vec![left_atom];
                    for a in args {
                        arg_atoms.push(self.lower_expr(&a.value, out)?);
                    }
                    let (arg_atoms, supplied) = self.permute_args(arg_atoms, *span);
                    let type_args = self.type_arg_atoms(span);
                    Ok(self.emit(
                        out,
                        Rvalue::Call {
                            callee,
                            args: arg_atoms,
                            type_args,
                            supplied,
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
                let type_args = self.type_arg_atoms(span);
                let reflect = self.sites.construction_sites.get(span).cloned();
                let reflect_slot = self.reflect_slot_atom(span);
                let order_slots = self.order_slots_at(span, out);
                let push_slots = self.push_slots(span, out);
                Ok(self.emit(
                    out,
                    Rvalue::Method {
                        receiver,
                        name: self.method_name(span, name),
                        name_span: *name_span,
                        args: vec![left_atom],
                        reuse: false,
                        reflect,
                        reflect_slot,
                        order: self.order_hint(span),
                        order_slots,
                        push: self.push_hint(span),
                        push_slots,
                        type_args,
                        // A bare callee takes the piped value and nothing else, so there is no
                        // argument list to rebind.
                        supplied: None,
                        span: *span,
                    },
                    *span,
                ))
            }
            // `x |> f` ⟶ `f(x)`.
            _ => {
                let callee = self.lower_expr(right, out)?;
                let type_args = self.type_arg_atoms(&span);
                Ok(self.emit(
                    out,
                    Rvalue::Call {
                        callee,
                        args: vec![left_atom],
                        type_args,
                        supplied: None,
                        span,
                    },
                    span,
                ))
            }
        }
    }

    /// Lower a call's argument list left-to-right (the tree-walker's order).
    /// Lower a call's argument list to atoms, in **written** order.
    ///
    /// The one funnel every call form goes through, which is why it is also where re-ordering a
    /// labelled argument list into parameter order belongs once the checker resolves the binding.
    fn lower_args(
        &mut self,
        args: &[noeta_ast::CallArg],
        call_span: Span,
        out: &mut Vec<Stmt>,
    ) -> Result<(Vec<Atom>, Option<u64>), Unsupported> {
        // Arguments are evaluated in the order the author WROTE them — a call's side effects must
        // not be resequenced by how its parameters happen to be declared — and then permuted into
        // parameter order, using the binding the checker resolved.
        let mut atoms = Vec::with_capacity(args.len());
        for arg in noeta_ast::CallArg::values(args) {
            atoms.push(self.lower_expr(arg, out)?);
        }
        Ok(self.permute_args(atoms, call_span))
    }

    /// Reorder an already-evaluated argument list into **parameter** order, and say which
    /// parameters it supplies.
    ///
    /// `atoms` is in the order the arguments were evaluated, which for a pipeline means the piped
    /// value at index 0 — the same list the checker bound, so the binding it recorded indexes into
    /// this one. A call with no recorded binding is already in parameter order and passes through
    /// untouched.
    fn permute_args(&self, atoms: Vec<Atom>, call_span: Span) -> (Vec<Atom>, Option<u64>) {
        let Some(binding) = self.sites.arg_orders.get(&call_span) else {
            return (atoms, None);
        };
        // Permute into parameter order, and say which parameters were supplied. A skipped one
        // contributes no atom — the callee fills its default, over its own upvalues, exactly as it
        // does for an argument list that simply stopped early.
        let permuted: Vec<Atom> = binding
            .iter()
            .flatten()
            .filter_map(|&i| atoms.get(i).cloned())
            .collect();
        let mut mask: u64 = 0;
        for (p, b) in binding.iter().enumerate() {
            if b.is_some() && p < 64 {
                mask |= 1 << p;
            }
        }
        // A mask is only worth carrying when it says something the prefix rule cannot. A pure
        // reordering (`sub(b: 1, a: 10)`) fills a *prefix* of the parameters, and `permuted` is
        // already in parameter order — so the ordinary rule describes it exactly, and every fast
        // path that assumes the prefix rule (the JIT's direct-call setup, the tier-1 helpers)
        // keeps applying. `Some` then means precisely "this call skips a defaulted parameter".
        // `mask` has exactly `permuted.len()` bits set by construction, so "the low `len` bits are
        // all set" is the same as "the set bits are the prefix" — and it does not overflow at 64.
        let is_prefix = mask.trailing_ones() as usize == permuted.len();
        (permuted, if is_prefix { None } else { Some(mask) })
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

    /// Lower a **call-site-typed extern method** (http arc H8) into [`Rvalue::TypedMethodCall`].
    ///
    /// Shared by both surface spellings, which the parser splits purely syntactically:
    /// `resp.json::<T>()` (bare-ident receiver, one type argument) arrives as
    /// `Expr::TypedModuleCall`, while `gh.get(p)?.json::<T>()` arrives as `Expr::TypedMethodCall`.
    /// Both mean the same thing, so both land here.
    fn lower_typed_method_call(
        &mut self,
        recv: &Expr,
        method: &str,
        args: &[noeta_ast::CallArg],
        span: Span,
        out: &mut Vec<Stmt>,
    ) -> Result<Atom, Unsupported> {
        let recv = self.lower_expr(recv, out)?;
        let args = args
            .iter()
            .map(|a| self.lower_expr(&a.value, out))
            .collect::<Result<Vec<_>, _>>()?;
        let recipe = self.sites.typed_method_call_sites.get(&span).cloned();
        let dynamic = self
            .sites
            .forwarded_slot_sites
            .get(&span)
            .map(|&i| Atom::Var {
                name: hidden_param_name(i),
                span,
            });
        Ok(self.emit(
            out,
            Rvalue::TypedMethodCall {
                recv,
                method: method.to_string(),
                args,
                recipe,
                dynamic,
                span,
            },
            span,
        ))
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

/// The synthetic name of a forwarding generic fn's `i`-th hidden type-argument parameter
/// (poly-values F2b). `$`-prefixed like the other lowering-synthesized names, so it can never
/// collide with a source identifier.
fn hidden_param_name(i: u32) -> String {
    format!("$ty{i}")
}

#[cfg(test)]
mod program_facts_tests {
    use super::*;

    fn parse(src: &str) -> AstProgram {
        let source = noeta_span::Source::new(noeta_span::SourceId::FIRST, "<facts>", src);
        let lexed = noeta_lexer::lex(&source);
        let parsed = noeta_parser::parse(&source, &lexed.tokens);
        assert!(
            lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
            "fixture should parse: {:?} {:?}",
            lexed.diagnostics,
            parsed.diagnostics
        );
        parsed.program
    }

    /// An empty registry, leaked once: these tests are about the merge rule, not about which
    /// natives resolve, and `noeta-ir` links no extension units of its own (installing the process
    /// default would need `noeta-stdlib`, which it deliberately does not depend on).
    fn registry() -> &'static noeta_ext_abi::registry::Registry {
        static REGISTRY: std::sync::OnceLock<noeta_ext_abi::registry::Registry> =
            std::sync::OnceLock::new();
        REGISTRY.get_or_init(|| noeta_ext_abi::registry::Registry::new(Vec::new()))
    }

    /// The merge rule, stated once here and once in [`ProgramFacts::under`]: the name → name maps
    /// shadow (the code in hand wins), the global-name set unions.
    #[test]
    fn a_fragments_own_declarations_shadow_the_ambient_ones() {
        let whole = parse(
            "fn render(statics: List<string>, holes: List<() -> string>): int { return 1 }\n\
             mut outer = 0\n",
        );
        let mut ambient = ProgramFacts::of(&whole, registry());
        ambient
            .expr_tiers
            .insert("html".to_string(), "package.render".to_string());

        // A fragment that declares nothing keeps every ambient entry.
        let bare = parse("fn page(): int { return 2 }\n");
        let folded = ambient.clone().under(&bare, registry());
        assert_eq!(
            folded.expr_tiers.get("html").map(String::as_str),
            Some("package.render")
        );
        // …and its own globals JOIN the enclosing program's rather than replacing them.
        assert!(folded.module_globals.contains("outer"));
        assert!(folded.module_globals.contains("page"));

        // A fragment that redeclares the tier means its own handler.
        let shadowing = parse(
            "@tier(html, text: \"html\", expr: int)\n\
             fn mine(statics: List<string>, holes: List<() -> string>): int { return 3 }\n",
        );
        let folded = ambient.under(&shadowing, registry());
        assert_eq!(
            folded.expr_tiers.get("html").map(String::as_str),
            Some("mine")
        );
    }

    /// A whole-program lowering passes no ambient facts, so `under` must be exactly `of` — the
    /// property that keeps every file-pipeline compile byte-identical to what it was before facts
    /// became a thing a caller can supply.
    #[test]
    fn an_empty_ambient_set_leaves_a_programs_own_facts_untouched() {
        let program = parse(
            "use std.id.Uuid\n\
             mut counter = 0\n\
             fn f(): int { return 1 }\n",
        );
        let own = ProgramFacts::of(&program, registry());
        let folded = ProgramFacts::default().under(&program, registry());
        assert_eq!(own.type_aliases, folded.type_aliases);
        assert_eq!(own.native_type_imports, folded.native_type_imports);
        assert_eq!(own.expr_tiers, folded.expr_tiers);
        assert_eq!(own.module_globals, folded.module_globals);
    }
}
