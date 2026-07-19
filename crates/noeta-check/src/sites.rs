//! The checker's **codegen-hint site maps**: the private [`SiteMaps`] accumulator, its public
//! compile-input projection [`Sites`], and the exported [`DestructorRelevance`]. Split out of the
//! crate root verbatim purely to shrink `lib.rs`; the recording sites in the checking/synthesis
//! paths are the writers, the backends (via [`Checked`](crate::Checked)) the readers.

use super::*;

/// The checker's **compile-input bundle**: every span-keyed codegen hint plus destructor relevance,
/// produced by one checker run and consumed as a unit by both backends
/// (`noeta_compiler::compile_with_sites` and the conformance reference). Each field is a *pure
/// function of the program* and invisible to `RunResult`, so both backends derive identical
/// behavior from the same bundle by construction. Bundling makes adding a site map one field here
/// plus the producers/consumers that care — not an arity bump across every pipeline driver. The
/// flip side: a consumer no longer *fails to compile* when a map is added, so a consumer that
/// deliberately ignores a field says so at its definition (the reference stays boxed for
/// [`Sites::map_packed_sites`]), and the differential oracle is what catches a forgotten
/// semantically-relevant map.
#[derive(Debug, Clone, Default)]
pub struct Sites {
    /// The full-fidelity `type_of` site map (see [`resolve_type_of_sites`]).
    pub type_of_sites: HashMap<Span, noeta_ast::reflect::TypeRepr>,
    /// Runtime type-argument reflection (`plans/reflection/runtime-type-args.md`, slice A): the
    /// resolved `TypeRepr` at each collection/object **construction** site (list/map/set/object/enum
    /// literal), so a value can be tagged with the type it was built as and `type_of`/`is` recover its
    /// type arguments after the static type is lost to `dyn`. Annotation-driven — a `List<dyn>` literal
    /// records `List(Dyn)`. Populated only for concretely-typed sites (a hole/`dyn` top is omitted →
    /// the value stays untagged, i.e. the pre-track head-only runtime behavior).
    pub construction_sites: HashMap<Span, noeta_ast::reflect::TypeRepr>,
    /// The packed-`List` construction-site map (see [`resolve_packed_list_sites`]).
    pub packed_list_sites: HashMap<Span, noeta_ast::reflect::PackedLayout>,
    /// Call-site-typed native-call recipes (`json.parse::<T>`): the turbofish `T` resolved into a
    /// [`noeta_ext_abi::TypeRecipe`] the lowering bakes into `Rvalue::TypedModuleCall`. A pure function of the
    /// program, like the other site maps.
    pub typed_module_call_sites: HashMap<Span, noeta_ext_abi::TypeRecipe>,
    /// `@derive(Deserialize<Json>)` decode recipes (L2.2 DI): each deriving **struct**'s type name
    /// paired with the [`noeta_ext_abi::TypeRecipe`] the checker resolved from its fields, in declaration
    /// order. Baked into a per-type runtime registry both backends lift, so `json.decode_typed(name,
    /// text)` can decode a JSON body into the type named by a runtime string. A pure function of the
    /// program, like the other site maps.
    pub deserialize_recipes: Vec<(String, noeta_ext_abi::TypeRecipe)>,
    /// `json.decode_typed(name, text)` call spans (L2.2 DI): the router-facing runtime decode. Lowering
    /// reads this to emit an [`Rvalue::DecodeTyped`](noeta_ir::Rvalue) at these spans instead of a
    /// generic method call. A pure function of the program.
    pub decode_typed_sites: HashSet<Span>,
    /// `map(...)` call spans whose result element type is packed → the result element's layout. The
    /// VM's `map` builtin builds a flat result at these sites (P-PACK 2.6 category B); invisible to
    /// `RunResult`, so the eval reference may ignore it and stay boxed.
    pub map_packed_sites: HashMap<Span, noeta_ast::reflect::PackedLayout>,
    /// Member-access spans (`list[i].field`) lowering fuses into a single `Rvalue::IndexField`, so a
    /// packed list element's field is read without materializing the element (P-PACK 2.5+). A pure
    /// function of the program, like the other site maps; the fusion is invisible to `RunResult`.
    pub index_field_sites: HashSet<Span>,
    /// `for` statement spans whose iterable is statically an `Iterator<T>` (Track I.2) — the lowering
    /// sets `Stmt::For.stream` so both backends drive the iterator's `next()` instead of snapshotting.
    pub for_stream_sites: HashSet<Span>,
    /// Fixed-width arithmetic sites (Tier W), keyed by the `Expr::Binary`/`Expr::Unary` span → the
    /// result's `(signed, bits)`. A same-width `+ - *` or unary `-` on an `IntN` records its site here;
    /// lowering wraps the op's result in `Rvalue::MaskWidth` to wrap the erased i64 into the width. A
    /// pure function of the program, like the other site maps — the masking is invisible to `RunResult`.
    pub width_sites: HashMap<Span, (bool, u8)>,
    /// Unbound method-handle sites (`Type.method` in value position) → the resolved
    /// `(ty, method, associated)`. Lowering emits an [`Rvalue::MethodHandle`] at these spans.
    pub handle_sites: HashMap<Span, (String, String, bool)>,
    /// Bound-handle sites (`value.method` in value position, EX.2b) → lowered to
    /// [`Rvalue::BoundHandle`] (the receiver captured into the handle).
    pub bound_handle_sites: HashSet<Span>,
    /// Field-call sites: `obj.f(args)` spans the checker resolved to a **field** of the receiver's
    /// type (no method `f` exists — a method wins in call position). Lowering emits field-get +
    /// indirect call (`Rvalue::Field` then `Rvalue::Call`) at these spans instead of method
    /// dispatch, so `obj.f(args)` means `(obj.f)(args)`. A pure function of the program, like the
    /// other site maps.
    pub field_call_sites: HashSet<Span>,
    /// Method-bundle call sites (kernel-methods K2): each bound bundle-method call span → the
    /// statically resolved route `(module qualified identity, bundle name)`. Lowering bakes the
    /// route into the call, so runtime dispatch is **call-site-resolved** — no shape-keyed
    /// discovery, and an empty list receiver dispatches fine. (The flip side, documented: a
    /// bundle method is not reachable through a `dyn` receiver — `dyn` stays the escape hatch;
    /// a runtime binding table would be an additive later extension.)
    pub bundle_call_sites: HashMap<Span, (String, String)>,
    /// Namespace-group member-access sites (`http.client`, `use std.http`) → the concrete
    /// **root-qualified module identity** the chain resolves to (`std.http.client`). Lowering emits
    /// an [`Rvalue::NativeModule`] at these `Member` spans instead of a field load — so the leaf
    /// identity reaches the const pool (AOT ring DCE intact) and a method call dispatches as a
    /// direct import would. A pure function of the program, like the other site maps.
    pub namespace_module_sites: HashMap<Span, String>,
    /// Bare float-literal spans adapted into an `f32` context (P-NUM-SYM) — the type-directed hint
    /// that makes lowering emit a narrow `Const::F32` instead of the default `Const::Float`. A pure
    /// function of the program (both backends narrow identically), like the other site maps.
    pub f32_literal_sites: HashSet<Span>,
    /// Per-binding destructor-relevance (Phase 3.2b) — the input the drop-insertion pass reads to
    /// mark each `DropVar`'s `relevant` bit. A pure function of the program, like `type_of_sites`,
    /// so both backends derive identical annotations.
    pub destructor_relevance: DestructorRelevance,
}

/// The checker's **codegen-hint output**: span-keyed site maps the backends and the lowering consult
/// to pick a representation or fuse an operation, kept as one cohesive group rather than scattered
/// across the [`Checker`]'s type-environment and control-flow-coloring fields (they are a distinct
/// concern — codegen hints, not type facts). Every one is a *pure function of the program* and
/// invisible to `RunResult`, so both backends derive the same hints by construction; they are lifted
/// out into the public [`Checked`] result verbatim.
#[derive(Clone, Default)]
pub(crate) struct SiteMaps {
    /// Each `type_of(value)` site (keyed by the `Expr::TypeOf` span) whose operand has a **concrete**
    /// static type, mapped to the precise [`TypeRepr`] the backends bake as a constant (`type_of`
    /// full fidelity, P2.3). A `dyn`/union/un-inferred operand is absent here — those fall back to
    /// the runtime head-constructor path. Both backends harvest this map via [`resolve_type_of_sites`]
    /// on the same program, so they emit identical `Type` values by construction.
    pub(crate) type_of_sites: HashMap<Span, noeta_ast::reflect::TypeRepr>,
    /// Every synthesized expression's inferred static type, keyed by the expression's span — the
    /// span→type index the IDE hover feature reads. Populated **only** when
    /// [`Checker::record_expr_types`] is set (the `check_all_with_types` / IDE path); the hot
    /// compile path leaves it empty and pays nothing. Concretely-typed sites only, like the other
    /// maps: a `dyn`/union/un-inferred result is omitted (hover simply shows nothing there).
    pub(crate) expr_types: HashMap<Span, noeta_ast::reflect::TypeRepr>,
    pub(crate) construction_sites: HashMap<Span, noeta_ast::reflect::TypeRepr>,
    /// List-construction sites whose element type is a `@packed` struct (P-PACK Phase 2), keyed by the
    /// constructing expression's span → the element's flat [`PackedLayout`]. Both backends consult this
    /// via [`resolve_packed_list_sites`] to lay out a `List<packed>` as one contiguous raw-primitive
    /// buffer instead of N boxed objects. A pure function of the program, like `type_of_sites`, so the
    /// two backends pick the same representation by construction (the flat layout stays invisible to
    /// `RunResult`).
    pub(crate) packed_list_sites: HashMap<Span, noeta_ast::reflect::PackedLayout>,
    /// Call-site-typed native-call recipes (`json.parse::<T>`), keyed by the `Expr::TypedModuleCall`
    /// span → the turbofish `T` resolved into a [`noeta_ext_abi::TypeRecipe`]. Both backends harvest
    /// this on the same program, so the lowering bakes identical recipes into `Rvalue::TypedModuleCall`.
    pub(crate) typed_module_call_sites: HashMap<Span, noeta_ext_abi::TypeRecipe>,
    /// `@derive(Deserialize<Json>)` decode recipes (L2.2 DI) — see [`Sites::deserialize_recipes`].
    /// Accumulated as each deriving struct is validated in `check_derives`.
    pub(crate) deserialize_recipes: Vec<(String, noeta_ext_abi::TypeRecipe)>,
    /// `json.decode_typed(name, text)` call spans (L2.2 DI) — see [`Sites::decode_typed_sites`].
    pub(crate) decode_typed_sites: HashSet<Span>,
    /// `map(list, fn)` call spans whose result element type is a `@packed` struct (P-PACK 2.6
    /// category B), keyed by the whole-call span → the result element's [`PackedLayout`]. The VM's
    /// `map` builtin consults this to build a flat result instead of N boxed objects; like the other
    /// site maps it is a pure function of the program, invisible to `RunResult`.
    pub(crate) map_packed_sites: HashMap<Span, noeta_ast::reflect::PackedLayout>,
    /// Member-access spans (`list[i].field`) the checker proved fusable: the index receiver is a
    /// built-in `List` and the field resolves on its element type. Lowering reads this (via
    /// [`Checked::index_field_sites`]) to emit a single [`Rvalue::IndexField`] that reads a packed
    /// element's field without materializing the element (P-PACK 2.5+). A pure function of the
    /// program, invisible to `RunResult`, so both backends fuse the same sites by construction.
    pub(crate) index_field_sites: HashSet<Span>,
    /// `for` statement spans whose iterable is statically an `Iterator<T>` — the loop streams via
    /// `next()` instead of snapshotting a list (Track I.2). Lowering reads this (via
    /// [`Checked::for_stream_sites`]) to set `Stmt::For.stream`. A pure function of the program; a
    /// collection or `dyn` iterable is absent here and keeps the snapshot/cursor fast path.
    pub(crate) for_stream_sites: HashSet<Span>,
    /// Fixed-width arithmetic sites (Tier W): the span of a same-width `+ - *` / unary `-` on an
    /// `IntN` → the result's `(signed, bits)`. Lowering reads this (via [`Checked::width_sites`]) to
    /// wrap the op's result in `Rvalue::MaskWidth`. Empty for programs with no fixed-width arithmetic.
    pub(crate) width_sites: HashMap<Span, (bool, u8)>,
    /// Unbound method-handle sites: a `Type.method` member expression in value position → the
    /// resolved `(ty, method, associated)`. Lowering reads this (via [`Checked::handle_sites`]) to
    /// emit an [`Rvalue::MethodHandle`] instead of a field load. A pure function of the program.
    pub(crate) handle_sites: HashMap<Span, (String, String, bool)>,
    /// **Bound**-handle sites (`value.method` in value position, EX.2b): spans whose `Member`
    /// lowers to an [`Rvalue::BoundHandle`] (receiver captured) instead of a field load.
    pub(crate) bound_handle_sites: HashSet<Span>,
    /// Field-call sites (`obj.f(args)` where `f` is a field) — see [`Sites::field_call_sites`].
    pub(crate) field_call_sites: HashSet<Span>,
    /// Bare float-literal spans adapted into an `f32` context (P-NUM-SYM) — lowering reads this (via
    /// [`Checked::f32_literal_sites`]) to emit a narrow `Const::F32` for the literal.
    pub(crate) f32_literal_sites: HashSet<Span>,
    /// Method-bundle call sites (kernel-methods K2) — see [`Sites::bundle_call_sites`].
    pub(crate) bundle_call_sites: HashMap<Span, (String, String)>,
    /// Namespace-group member-access sites — see [`Sites::namespace_module_sites`].
    pub(crate) namespace_module_sites: HashMap<Span, String>,
}

impl SiteMaps {
    /// Project the accumulator into the public compile-input [`Sites`] bundle (consuming the
    /// accumulated maps; `expr_types` stays behind — it is the IDE index, not a compile input).
    /// One projection shared by `check_all`, `check_all_session`, and the session snapshot, so the
    /// three can never drift field-wise.
    pub(crate) fn into_sites(self, destructor_relevance: DestructorRelevance) -> Sites {
        Sites {
            type_of_sites: self.type_of_sites,
            construction_sites: self.construction_sites,
            packed_list_sites: self.packed_list_sites,
            typed_module_call_sites: self.typed_module_call_sites,
            deserialize_recipes: self.deserialize_recipes,
            decode_typed_sites: self.decode_typed_sites,
            map_packed_sites: self.map_packed_sites,
            index_field_sites: self.index_field_sites,
            for_stream_sites: self.for_stream_sites,
            width_sites: self.width_sites,
            handle_sites: self.handle_sites,
            bound_handle_sites: self.bound_handle_sites,
            field_call_sites: self.field_call_sites,
            f32_literal_sites: self.f32_literal_sites,
            bundle_call_sites: self.bundle_call_sites,
            namespace_module_sites: self.namespace_module_sites,
            destructor_relevance,
        }
    }
}

/// Which bindings hold a value whose drop could run a `destruct` block — the **destructor-relevance**
/// the checker exports for the Phase-3 drop-insertion pass (memory-management migration). Sound and
/// **conservative**: a binding absent here is provably non-relevant (its type reaches no destructor);
/// a binding present here *may* be relevant, so its drop keeps the runtime destructor check. Two
/// keyings because the Core IR identifies the two binding kinds differently: a local by its binding
/// `name_span`, a parameter by `(its function's span, its name)` — the IR's `Func` carries the span
/// and the parameter names, but not per-parameter spans.
#[derive(Debug, Clone, Default)]
pub struct DestructorRelevance {
    /// `name_span`s of non-parameter bindings whose value's type is destruct-reachable.
    pub locals: HashSet<Span>,
    /// `(function span, parameter name)` of parameters whose type is destruct-reachable.
    pub params: HashSet<(Span, String)>,
    /// **Type names** whose value, when destroyed, could run *some* `destruct` block — its own or a
    /// transitively-owned field / variant-payload / collection element (the [`Checker::compute_relevance`]
    /// fixpoint). This is the *per-type* projection of the same reachability the per-binding sets use;
    /// the backends consume it as the **container-before-contained field-walk gate** (Phase 4.3, spec
    /// §4): an object/enum whose name is absent here owns no destructor anywhere in its subtree, so it
    /// frees on the plain-release fast path with no recursive destructor walk. (The drop-insertion pass
    /// uses only `locals`/`params`; `passes_relevance` drops this field.) Includes every type with its
    /// own `destruct` by construction (the fixpoint seeds with them), so own-destructor firing is never
    /// gated away.
    pub reachable_types: HashSet<String>,
}
