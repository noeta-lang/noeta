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
    /// **Dynamic** construction sites (generic-in-generic construction): a fresh-constructor call
    /// span whose instantiation is not in the compiled body at all → the **hidden type-argument
    /// slot** of the enclosing member that carries it. The dynamic twin of
    /// [`Sites::construction_sites`] at a call span, and the reason it must exist: inside
    ///
    /// ```noe
    /// class LiveRepository<T> {
    ///     repo: Repository<T>
    ///     fn new(…): LiveRepository<T> { return LiveRepository { repo: Repository.new(…), … }; }
    /// }
    /// ```
    ///
    /// the inner call's resolved type is `Repository<T>` with `T` still a *parameter*, so
    /// [`Checker::note_constructor_call`](crate::Checker::note_constructor_call) has no concrete
    /// `TypeRepr` to record — one compiled `LiveRepository.new` serves every instantiation. The
    /// concrete `Repository<Todo>` is resolved (and interned) by the OUTER call, which supplies it on
    /// the hidden slot; the body then stamps whatever the slot names onto the object this inner call
    /// freshly built. So the *tag* is still a statically-interned `TypeRepr` — only *which* interned
    /// entry is dynamic, which is what keeps both backends resolving it identically (one table index).
    ///
    /// Resolved through [`Sites::type_arg_reprs`], indexed by the slot's runtime value. A pure
    /// function of the program, like the other site maps.
    pub dynamic_construction_sites: HashMap<Span, u32>,
    /// The nominal type each **target-typed** object literal `.{ … }` resolved to, keyed by the
    /// literal's span. The source elides the name and the checker recovers it from the expected
    /// type; lowering reads this to fill `Rvalue::Object.type_name`, so every backend keeps seeing
    /// an ordinary named construction and none of them needs to know the form exists. Distinct from
    /// [`Sites::construction_sites`], which is the *reflection* hint and drops non-generic nominals
    /// — the name here must be present for every `.{ … }`, generic or not. A pure function of the
    /// program.
    pub inferred_object_types: HashMap<Span, String>,
    /// **Bare payload-free variant patterns**: every `match`-arm
    /// [`Pattern::Binding`](noeta_ast::Pattern) span (top-level *or* nested) whose name the checker
    /// resolved to a **payload-free variant of the scrutinee's own enum** → the qualifier to test
    /// against and the variant's name. `(Some("Type"), "String")` for a bare `String` arm on a
    /// `Type` scrutinee; `(None, "none")` for a bare `none` on an `?T`, whose built-in `Option` has
    /// no written type name.
    ///
    /// A bare identifier is otherwise a binding, and resolution is **scrutinee-directed**: a name
    /// that is not a payload-free variant of *this* scrutinee's enum, or any name when the
    /// scrutinee's type is gradual/`dyn`/unknown, is absent here and stays a binding. Lowering
    /// rewrites the recorded spans into `Pattern::Variant { bindings: [] }`, so both backends keep
    /// seeing an ordinary qualified variant pattern and neither needs to know the bare form exists
    /// — the same "checker recovers what the source elided" seam as
    /// [`Sites::inferred_object_types`]. A pure function of the program.
    pub variant_pattern_sites: HashMap<Span, (Option<String>, String)>,
    /// The packed-`List` construction-site map (see [`resolve_packed_list_sites`]).
    pub packed_list_sites: HashMap<Span, noeta_ast::reflect::PackedLayout>,
    /// `from_bytes::<T>` spans whose packed element type `T` implements `Validate` (validation arc):
    /// lowering sets `Rvalue::FromBytes.validate`, so both backends run `validate()` on each decoded
    /// element (aborting at `[i]` on the first rejection — the abort door, consistent with a shape
    /// error). A pure function of the program.
    pub from_bytes_validated: HashSet<Span>,
    /// `fields_of(value)` spans at which the operand's own **private** fields are visible — the
    /// value-level reflection door's visibility answer, decided once here by the same
    /// [`Checker::field_visible`](crate::Checker) rule a written `x.secret` goes through.
    ///
    /// Absent (the common case) means the door reports only the fields the caller could have read
    /// itself. Present means every field, which is what an operand of the *enclosing* type — the
    /// `fields_of(self)` a type writes about itself — and a white-box dev-tier body get. One bit per
    /// site rather than per field, because visibility does not vary across a type's fields: they are
    /// declared in one place, so one package and one `current_type` comparison answers for all of
    /// them. A pure function of the program.
    pub fields_of_private: HashSet<Span>,
    /// Call-site-typed native-call recipes (`json.parse::<T>`): the turbofish `T` resolved into a
    /// [`noeta_ext_abi::TypeRecipe`] the lowering bakes into `Rvalue::TypedModuleCall`. A pure function of the
    /// program, like the other site maps.
    pub typed_module_call_sites: HashMap<Span, noeta_ext_abi::TypeRecipe>,
    /// Call-site-typed **extern-method** recipes (`resp.json::<T>`, http arc H8): the turbofish `T`
    /// resolved into a [`noeta_ext_abi::TypeRecipe`] the lowering bakes into
    /// `Rvalue::TypedMethodCall`. The extern-type twin of [`Sites::typed_module_call_sites`] —
    /// presence here is exactly what distinguishes a *native typed* method call from an ordinary
    /// (erased) generic-method instantiation. A pure function of the program.
    pub typed_method_call_sites: HashMap<Span, noeta_ext_abi::TypeRecipe>,
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
    /// Every `@packed` struct **bound to a `vec` method bundle** (`impl vec.Kernels for T {}`), by its
    /// flat [`PackedLayout`] (scalar-unification slice 3). The compiler interns each into the module's
    /// `packed_schemas` so the VM has the element width for the bundle's *element* methods even when the
    /// type never appears in a `List<T>` (a single struct value erases its field widths to boxed
    /// scalars; the interned schema recovers them, matching the tree-walker's per-type field index).
    /// Not a construction site — purely a schema-availability channel.
    pub bundle_schema_layouts: Vec<noeta_ast::reflect::PackedLayout>,
    /// **Every** `@packed` struct's flat [`PackedLayout`](noeta_ast::reflect::PackedLayout) (source
    /// *and* native), by type name in a deterministic order — the from-scratch producer's
    /// schema-availability channel (native type-declaration unification, Slice E2). The compiler
    /// interns each into the module's `packed_schemas` **unconditionally**, so a native fn calling
    /// [`NativeCtx::make_packed`](noeta_ext_abi::NativeCtx::make_packed) can resolve a produced
    /// `List<packed>`'s element schema BY (qualified) name even when the type never appears in a
    /// source `List<T>` literal (the case a `like`-borrowing producer cannot cover). A superset of
    /// [`Sites::bundle_schema_layouts`] deduplicated by the interner; not a construction site. The
    /// tree-walker reference reads it too, keyed by name, so its `make_packed` resolves the same
    /// layouts. A pure function of the program.
    pub packed_type_layouts: Vec<noeta_ast::reflect::PackedLayout>,
    /// Member-access spans (`list[i].field`) lowering fuses into a single `Rvalue::IndexField`, so a
    /// packed list element's field is read without materializing the element (P-PACK 2.5+). A pure
    /// function of the program, like the other site maps; the fusion is invisible to `RunResult`.
    pub index_field_sites: HashSet<Span>,
    /// Call spans whose arguments need rebinding: for each parameter position, the index of the
    /// argument in **written** order, or `None` where the parameter was skipped and the callee
    /// must fill its default.
    ///
    /// The checker resolves the binding — it is the only pass that knows the callee's parameter
    /// names — and lowering permutes the evaluated atoms, so both backends bind identically by
    /// construction. Absent for a purely positional call, which is already in order.
    pub arg_orders: HashMap<Span, Vec<Option<usize>>>,
    /// `for` statement spans whose iterable is statically an `Iterator<T>` (Track I.2) — the lowering
    /// sets `Stmt::For.stream` so both backends drive the iterator's `next()` instead of snapshotting.
    pub for_stream_sites: HashSet<Span>,
    /// Fixed-width arithmetic sites (Tier W), keyed by the `Expr::Binary`/`Expr::Unary` span → the
    /// result's `(signed, bits)`. A same-width `+ - *` or unary `-` on an `IntN` records its site here;
    /// lowering wraps the op's result in `Rvalue::MaskWidth` to wrap the erased i64 into the width. A
    /// pure function of the program, like the other site maps — the masking is invisible to `RunResult`.
    pub width_sites: HashMap<Span, (bool, u8)>,
    /// **Display sites whose value contains an unsigned 64-bit integer**, keyed by the rendered
    /// expression's span → the [`RenderHint`](noeta_ast::RenderHint) built from its static type.
    ///
    /// The display twin of [`Sites::width_sites`], and it exists for the same reason: a fixed-width
    /// integer is erased to its i64 word, so a `u64` past bit 63 is a negative word and renders as
    /// its signed reinterpretation unless the *type* says otherwise. Lowering reads this (via
    /// [`Checked::render_hint_sites`](crate::Checked)) and wraps the atom in an
    /// `Rvalue::Render`, which both backends apply through the same walk. Recorded at the three
    /// doors that render a value from an expression whose static type is in hand — `echo`, an
    /// interpolation hole, and a display-based `~` operand — and empty for every program with no
    /// `u64` in a displayed position. A pure function of the program, like the other site maps.
    pub render_hint_sites: HashMap<Span, noeta_ast::RenderHint>,
    /// **JSON sites whose value contains an unsigned 64-bit integer**, keyed by the serializing
    /// call's span → the [`RenderHint`](noeta_ast::RenderHint) built from the serialized value's
    /// static type.
    ///
    /// The JSON twin of [`Sites::render_hint_sites`]: an erased i64 word carries no signedness, so a
    /// `u64` past bit 63 would be *written to the wire* as its signed reinterpretation — a wrong
    /// number in an API response or a persisted record, with nothing to tell the reader. Recorded at
    /// the doors that turn a value whose static type is in hand into JSON text — the
    /// `json.stringify` argument and a derived `to_json` receiver — and read by lowering (via
    /// [`Checked::json_hint_sites`](crate::Checked)), which emits an `Rvalue::JsonRender` both
    /// backends serialize through the one hinted walk. Empty for every program with no `u64` in a
    /// serialized position. A pure function of the program, like the other site maps.
    pub json_hint_sites: HashMap<Span, noeta_ast::RenderHint>,
    /// **Ordering sites whose value contains an unsigned 64-bit integer**, keyed by the site's span
    /// → the [`RenderHint`](noeta_ast::RenderHint) built from the receiver's (or iterable's) static
    /// type.
    ///
    /// The ordering twin of [`Sites::render_hint_sites`], and it exists for the same reason: a
    /// `u64` past bit 63 is a negative i64 word, so it would order below every small value unless
    /// the *type* says otherwise. Recorded at the doors that reveal an order a program can see —
    /// `.sorted()`, `.min()`, `.max()`, `.keys()`, `.values()`, and a `for` over a set or map —
    /// **and** at the arithmetic doors needing the identical bit: `checked_sum`, which reports
    /// overflow at the element width rather than wrapping, so the two readings of a 64-bit word
    /// disagree about which sums overflow at all, and the two bulk array ops that compare — `abs`
    /// against zero and `clamp` against its bounds. Never at a set's canonical buffer or a map's key
    /// placement, which are identity orders
    /// built at one site and probed at another (see [`noeta_ast::render_hint`]). Empty for every
    /// program with no `u64` in an ordered position. A pure function of the program.
    ///
    /// It carries one door that **renders** rather than orders: `.join()`, whose elements are
    /// written into a string by the method itself and so never reach a display site of their own.
    /// The hint is the same hint — built from the same receiver type, shaped `Elements(…)`, and
    /// numbered by the same `HintPurpose::Display` walk both backends run on their own value model —
    /// so it travels on this map rather than on a second one that would have to stay in step with
    /// it. The recorder (`Checker::note_order_hint`) and the checker's door list
    /// (`stdlib::discloses_width`) name every entry.
    pub order_hint_sites: HashMap<Span, noeta_ast::RenderHint>,
    /// **Deferred-serialization sites whose bound value contains an unsigned 64-bit integer**, keyed
    /// by the binding call's span → the [`RenderHint`](noeta_ast::RenderHint) built from the bound
    /// argument's static type.
    ///
    /// The third JSON door, and the one with no serializing call to sit on. A native type may declare
    /// (`ExtType::push_hint_args`) that a method *keeps* one of its arguments and serializes it to
    /// JSON on some later tick — `std.reactive`'s `View.expose` binds a reactive node whose value is
    /// pushed afresh on every flush. By then there is no call site left to read a static type from,
    /// so the hint is recorded where the value is **bound** and travels with the binding. Read by
    /// lowering (via [`Checked::binding_hint_sites`](crate::Checked)); the tree-walker takes it off
    /// the method node and the VM off a span-keyed module table, and both hand it to the dispatch as
    /// `NativeCtx::push_hint`. Empty for every program with no `u64` in a bound position.
    pub binding_hint_sites: HashMap<Span, noeta_ast::RenderHint>,
    /// **Session-echo sites whose value contains an unsigned 64-bit integer**, keyed by the trailing
    /// bare expression's span → the [`RenderHint`](noeta_ast::RenderHint) built from its static type.
    ///
    /// The fourth display door, and the only one the *program* does not contain: a REPL or debug
    /// console echoes an entry's trailing bare expression, so the value is rendered by the **host**
    /// rather than by an `echo` the author wrote. Recorded only in session mode, only for the
    /// statement [`noeta_ast::desugar::trailing_expr_span`] names, and read only by the sessions —
    /// **never by lowering**, which is why it is its own map: `render_hint_sites` is consumed by
    /// `lower_expr`, which would wrap the expression in an `Rvalue::Render` and turn the entry's
    /// value into a string.
    ///
    /// No `Display`-trait exemption, deliberately: a session echoes a value **structurally**
    /// (`Gauge {v: 1}`), where `echo` dispatches the type's own `to_string`. The hint therefore
    /// describes every declared field, exactly as the echo renders them.
    pub echo_hint_sites: HashMap<Span, noeta_ast::RenderHint>,
    /// **Statically decided type tests**: `Expr::TypeTest` spans the checker answered itself → the
    /// answer. Lowering emits the constant (after still evaluating the scrutinee for its effects)
    /// instead of an `Rvalue::TypeTest`, so both backends agree by construction.
    ///
    /// Recorded for exactly one family today — a bare **erased-width** target (`x is iN` /
    /// `x is f64`) whose scrutinee's static type settles the question. That is the family the
    /// *runtime* cannot answer at all: no scalar value carries a width tag, so the runtime matcher
    /// reaches no head for it and always says `false`. Where the checker knows the width
    /// (`a: i32` → `a is i32`), `false` is simply the wrong answer, and folding is what makes the
    /// two agree; where it does not (a `dyn` launder, a union, an erased type parameter), the test
    /// stays unanswerable and E0063 says so rather than a fold happening silently.
    ///
    /// Deliberately *not* extended to tests the runtime already answers correctly (`x is int` on an
    /// `int`): folding those would change no answer and would put the checker's subtyping opinion
    /// where the shared runtime matcher is the single source of truth.
    pub folded_type_tests: HashMap<Span, bool>,
    /// Unbound method-handle sites (`Type.method` in value position) → the resolved
    /// `(ty, method, associated)`. Lowering emits an [`Rvalue::MethodHandle`] at these spans.
    pub handle_sites: HashMap<Span, (String, String, bool)>,
    /// Bound-handle sites (`value.method` in value position, EX.2b) → lowered to
    /// [`Rvalue::BoundHandle`] (the receiver captured into the handle).
    pub bound_handle_sites: HashSet<Span>,
    /// **Type-parameter associated-call** sites: the span of a `T.m(…)` call whose receiver `T` is
    /// an in-scope type parameter, mapped to the parameter's spelling.
    ///
    /// A type parameter is erased, so there is no type in the compiled body to dispatch on — but
    /// the instantiation's NAME reaches the body through the same per-instantiation channel
    /// `type_name::<T>()` reads (recorded at the *receiver's* span, which is where lowering asks
    /// for it). Lowering rewrites the call into the by-name dispatch the runtime already performs,
    /// so this is a rewrite instruction, not a new dispatch mechanism.
    ///
    /// The value is the spelling rather than a bare span set because the rewrite it triggers
    /// contains member calls of its own; matching on the receiver's name is what keeps the
    /// desugared call from re-entering the rewrite.
    pub type_param_assoc_sites: HashMap<Span, String>,
    /// Field-call sites: `obj.f(args)` spans the checker resolved to a **field** of the receiver's
    /// type (no method `f` exists — a method wins in call position). Lowering emits field-get +
    /// indirect call (`Rvalue::Field` then `Rvalue::Call`) at these spans instead of method
    /// dispatch, so `obj.f(args)` means `(obj.f)(args)`. A pure function of the program, like the
    /// other site maps.
    pub field_call_sites: HashSet<Span>,
    /// **Generic-method turbofish** spans reached through the `TypedModuleCall` surface (generic
    /// methods, D3): a `recv.m::<T>(args)` with a single type argument and a bare-identifier
    /// receiver parses as [`noeta_ast::Expr::TypedModuleCall`] (the atom that also spells
    /// `json.parse::<T>(s)`), but when the receiver is a value or a user type — not an imported
    /// native module — it is a generic **method** call. The checker records the span here so
    /// lowering desugars it to a plain method call (`Rvalue::Method` / the associated-call path)
    /// instead of a native `Rvalue::TypedModuleCall`. A pure function of the program, like the
    /// other site maps.
    pub member_method_call_sites: HashSet<Span>,
    /// Trait method call sites → the statically resolved route `(trait qualified identity, method)`.
    /// Recorded (a) when a call resolves to a native trait's *defaulted* method answered by the trait's
    /// native default-body dispatch (slice 2), and (b) — since the ExtBundle→ExtTrait fold-in (slice 4)
    /// — for **every kernel-trait method** (`impl vec.Kernels for T {}`): the bundle runtime route was
    /// unified onto the trait route, so lowering bakes it into an [`Rvalue::TraitMethod`] dispatched
    /// through the trait's ctx dispatch with the receiver as slot 0. `dyn` receivers are the documented
    /// escape hatch (only statically-known concrete receivers route here).
    pub trait_call_sites: HashMap<Span, (String, String)>,
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
    /// `?`-conversion sites (error-ergonomics): each `Expr::Try` span whose `Err` payload type
    /// differs from the enclosing function's declared error type **and** converts through that
    /// target's `impl From<Source>` → the target error type's name and the **method-table key** of
    /// the conversion the propagated `Err` type selected. Lowering rewrites the `?` operand at these
    /// spans to convert the `Err` payload (`Err(e)` → `Err(Target.from(e))`) before the ordinary
    /// propagation, so both backends convert identically by construction. A pure function of the
    /// program, like the other site maps.
    ///
    /// The key travels with the target because the target alone does not determine the conversion:
    /// a type declaring several names each of them after its source
    /// ([`noeta_ast::conversion::from_conversion_keys`]), and matching the source against them is a typing
    /// question lowering cannot re-ask.
    pub try_conversion_sites: HashMap<Span, (String, String)>,
    /// Explicit `Target.from(x)` call spans on a type declaring **several** conversions → the
    /// method-table key the argument's type selected. The static twin of
    /// [`Sites::try_conversion_sites`]: same resolution rule, same reason lowering cannot redo it,
    /// and lowering substitutes the recorded name for the `from` the source wrote.
    ///
    /// Absent for a target declaring a single conversion, which keeps the plain `from` its call
    /// site already names.
    pub from_call_sites: HashMap<Span, String>,
    /// The program-wide **type-argument table** (poly-values F2b): every concrete instantiation a
    /// call of a forwarding generic fn resolved, interned by structural equality. Lowering embeds
    /// it into the IR `Program` (and the VM `Module`), and a hidden call argument indexes it at
    /// runtime. A pure function of the program, like the other site maps.
    ///
    /// **Numbered per check run.** A fresh whole-program check numbers this table from zero in its
    /// own discovery order, so the indices are meaningful only *together with the bundle that
    /// produced them*. A LIVE session (the REPL, a hot swap) whose runtime values already hold
    /// indices into an earlier table must therefore ABSORB this one by content rather than adopt it
    /// — `noeta_compiler::SessionCompiler::absorb_type_args` does exactly that, merging on the same
    /// dedup key this table is interned with and remapping [`Sites::hidden_arg_sites`] to match.
    pub type_arg_table: Vec<noeta_ext_abi::TypeArgInfo>,
    /// The **reflection projection** of [`Sites::type_arg_table`], indexed identically (same length,
    /// entry `i` describes entry `i`): each interned instantiation's [`noeta_ast::reflect::TypeRepr`],
    /// or `None` where it has none (a `dyn`/union/hole top — see [`type_to_repr_top`]).
    ///
    /// A *parallel* table rather than a third field on [`noeta_ext_abi::TypeArgInfo`], and
    /// deliberately: `noeta-ext-abi` is the lean ABI crate and has no `noeta-ast` dependency, which a
    /// `TypeRepr` field would force on it (and on every extension that links it). The two tables are
    /// built and grown in lockstep by the one interner
    /// ([`Checker::intern_type_arg`](crate::Checker::intern_type_arg)), which is also what makes the
    /// index shared: [`Sites::dynamic_construction_sites`] reads a slot's runtime value as an index
    /// into *both*.
    ///
    /// The repr is part of the interner's **dedup key**, not merely carried alongside it: two
    /// instantiations of one generic class (`Repository<Todo>`, `Repository<Order>`) share a
    /// [`noeta_ext_abi::TypeArgInfo`] exactly (its `name` is head-keyed and a class has no decode
    /// recipe), so deduplicating on that alone would fold them into one entry and let two
    /// differently-instantiated construction sites report each other's argument.
    pub type_arg_reprs: Vec<Option<noeta_ast::reflect::TypeRepr>>,
    /// The **render-hint projection** of [`Sites::type_arg_table`], indexed identically (same
    /// length, entry `i` describes entry `i`): each interned instantiation's own
    /// [`noeta_ext_abi::TypeArgHints`], and empty hints where it holds no unsigned 64-bit integer
    /// anywhere — which is nearly every instantiation.
    ///
    /// What a door inside a **generic** body resolves through. Its static type names a type
    /// parameter, which names no width, so it records [`noeta_ast::RenderHint::Param`] pointing at
    /// the render slot the call fills; the backends read this table at that slot's runtime value and
    /// splice the answer in. Without it a `u64` written or displayed through any generic reaches the
    /// wire as the negative word it is erased to.
    ///
    /// A third parallel table for the same reason [`Sites::type_arg_reprs`] is a second one: it is
    /// grown in lockstep by the one interner ([`intern_type_arg_entry`]) and is part of that
    /// interner's dedup key, so two instantiations that render differently can never share an entry.
    pub type_arg_hints: Vec<noeta_ext_abi::TypeArgHints>,
    /// Call spans of **forwarding-generic** calls → the hidden type-argument slots the call must
    /// supply, in the callee's forwarding order (`Table(i)` = a concrete instantiation's table
    /// index; `Forward(j)` = pass the enclosing body's own hidden slot `j` through). Lowering
    /// puts the matching atoms in the call node's `type_args` channel, beside its value
    /// arguments — never inside them, so the callee's value parameter positions are exactly what
    /// the source wrote.
    ///
    /// The only field of this bundle carrying a [`Sites::type_arg_table`] **index** — and that is
    /// no longer a claim in prose: a table index has its own type
    /// ([`noeta_ext_abi::TypeArgIndex`], reached here through
    /// [`HiddenArg::Table`](noeta_ext_abi::HiddenArg::Table)), every field is classified in
    /// [`SITE_POLICIES`], and the census gate fails a field that carries one and says otherwise.
    /// The `u32`s in `forwarded_slot_sites` / `dynamic_construction_sites`, like `Forward(j)` here,
    /// are per-body hidden SLOT ordinals resolved through the slot's runtime value. So a session
    /// absorbing a fresh table (see [`intern_type_arg_entry`]) remaps this map and nothing else —
    /// which is what [`Sites::remap_type_arg_indices`] spells out, exhaustively.
    pub hidden_arg_sites: HashMap<Span, Vec<noeta_ext_abi::HiddenArg>>,
    /// The **render-slot compositions** a
    /// [`HiddenArg::Compose`](noeta_ext_abi::HiddenArg::Compose) in [`Sites::hidden_arg_sites`]
    /// indexes: how a call inside a generic body fills a render slot naming an instantiation the
    /// body BUILT out of its own type parameters (`wrap([v])` inside `fn built<T>(v: T)`).
    ///
    /// Carries [`Sites::type_arg_table`] indices on both sides of each case — the leaf slots' values
    /// and the entry they compose to — so a session absorbing a fresh table rewrites them exactly as
    /// it rewrites `hidden_arg_sites`. Consumed entirely by lowering, which bakes each composition's
    /// cases into the op that performs the lookup, so the *index* into this list never leaves the
    /// compile.
    pub type_arg_compositions: Vec<noeta_ext_abi::HintComposition>,
    /// Spans whose turbofish is a **FORWARDED type parameter** of the enclosing top-level generic
    /// `fn` → the hidden slot index holding the instantiation's entry in [`Sites::type_arg_table`].
    ///
    /// One map over every surface that names a type, because there is one fact. Two shapes read it:
    /// `Expr::TypedModuleCall` (`json.try_parse::<T>` — lowering emits a dynamic *recipe* operand
    /// instead of a baked `TypeRecipe`), and every **name-keyed** surface — `type_name::<T>()`,
    /// `attributes_of::<T>()`, `roles_of::<E>()`, `field_specs_of::<T>()`, `variants_of::<T>()`,
    /// `construct::<T>(…)`, `v.as<T>()`, `v is T` — which all reach it through the one helper
    /// `Lowerer::type_param_name_atom`, emitting an [`Rvalue::TypeSlotName`](noeta_ir::Rvalue)
    /// instead of folding a constant string. A span belongs to exactly one `Expr`, and each consumer
    /// consults this from its own variant's arm, so the variant the span came from is already known
    /// where the slot is read; parallel `HashMap<Span, u32>`s would only spread one lookup across
    /// several places to keep in step.
    ///
    /// The type-side twin is [`Sites::self_type_arg_sites`], which reads a generic *type*'s
    /// argument off the receiver's reflected tag; there is no receiver here, so the name and the
    /// decode recipe both come from the same hidden slot — which is why a forwarded name and a
    /// forwarded manifest can never disagree about what `T` is.
    ///
    /// A pure function of the program, like the other site maps.
    pub forwarded_slot_sites: HashMap<Span, u32>,
    /// `type_name::<T>()` spans inside a **generic type's instance method**, where `T` is one of
    /// the enclosing type's own parameters → that parameter's index in the type's declaration order
    /// (generic constructor reflection, Gap B). The instantiation is not in the compiled body — one
    /// body serves every `Repo<…>` — but it *is* on the receiver: `self` carries the reflected type
    /// tag its construction site stamped, so lowering reads argument `i` off that tag instead of
    /// baking a constant string. A pure function of the program, like the other site maps.
    pub self_type_arg_sites: HashMap<Span, (String, u32)>,
    /// Forwarding generic fns and methods → their hidden-slot count, keyed as the callable traces
    /// (a bare `fn` name, or `Type.method`). Lowering gives the callable that many leading
    /// type-argument parameters (`$ty0`, `$ty1`, …) and records the count on `Func::hidden`, which
    /// is what tells every binder how many slots to lay down before the value arguments start.
    pub forwarding_fns: HashMap<String, u32>,
    /// **Instance methods of a generic type** → how many of their render slots are read off the
    /// receiver, keyed exactly as [`Sites::forwarding_fns`] is (`Type.method`).
    ///
    /// A body's render slots are laid out as the forwarding slots, then the `fn`'s own render
    /// slots, then these — and the first two of those are the hidden `$ty` parameters
    /// [`Sites::forwarding_fns`] counts, so that count *is* the base ordinal of this section. A
    /// method carries no hidden parameter of its own for its class's arguments (its four
    /// name-keyed entry points bind positionally and would read a value argument as a table
    /// index); it carries a receiver instead, and slot `base + i` is filled at the door by reading
    /// type argument `i` off that receiver's reflected tag
    /// ([`noeta_ast::reflect::TypeRepr::render_slot_arg`]).
    ///
    /// Populated for exactly the methods [`Coloring::self_type_params`] is populated for — an
    /// instance method of a generic type, and nothing else — so the count and the parameters it
    /// counts come from one decision.
    ///
    /// [`Coloring::self_type_params`]: crate::Coloring::self_type_params
    pub self_render_fns: HashMap<String, u32>,
    /// **Forwarding-fn-as-value** sites (poly-deferrals D2c): `Expr::Ident` spans where a
    /// forwarding generic fn is used as a VALUE with its instantiation pinned by the expected
    /// type → `(fn name, adopted arity)`. Lowering wraps the reference in a synthesized closure
    /// whose body calls the fn — the closure's inner call span is this same ident span, which
    /// keys the resolved hidden atoms in [`Sites::hidden_arg_sites`], so the hidden slots are
    /// bound into the value (a partial application over the type-argument slots).
    pub fn_value_sites: HashMap<Span, (String, u32)>,
    /// Per-binding destructor-relevance (Phase 3.2b) — the input the drop-insertion pass reads to
    /// mark each `DropVar`'s `relevant` bit. A pure function of the program, like `type_of_sites`,
    /// so both backends derive identical annotations.
    pub destructor_relevance: DestructorRelevance,
}

/// What a [`Sites`] field carries — asked as the question a **live session** has to answer about
/// it: *does anything in here index a table the session renumbers?*
///
/// A cold whole-program compile never has to care: the compiler is empty, the checker's numbering
/// is adopted whole, and every class below behaves the same. A REPL entry or a hot swap installs a
/// freshly-checked bundle into a compiler whose tables are already numbered — and the incoming
/// [`Sites::type_arg_table`] was numbered from zero, in this check run's own discovery order, while
/// live runtime values hold indices into the *previous* numbering. That is bug 3 of the four that
/// closed the `compile_to_mc` / `extend_impl` split ("the type-argument table replaced where it had
/// to be merged"), and its shape is: one carrier of a table index gets remapped, another does not,
/// and the program keeps running with the wrong type argument.
///
/// So every field states its class here, and the classes are the distinctions that mistake is made
/// out of. Three of these fields carry a `u32` that looks *exactly* like a table index and is not
/// one; that near-miss is why [`SiteClass::Ordinal`] exists as its own class instead of being
/// folded into [`SiteClass::SpanKeyed`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SiteClass {
    /// Carries a [`noeta_ext_abi::TypeArgIndex`] — an index into [`Sites::type_arg_table`].
    /// `noeta_compiler::SessionCompiler::absorb_type_args` **must** rewrite it into session space,
    /// which it does through [`Sites::remap_type_arg_indices`].
    TableIndexed,
    /// The type-argument table itself, or a projection indexed in lockstep with it. Not remapped —
    /// *replaced*, by the merged superset the session already holds.
    TheTable,
    /// Carries an integer that indexes or counts something ELSE, and must therefore be left alone.
    /// The note names the space: a per-body hidden **slot** ordinal (`$tyN`, resolved through the
    /// slot's runtime value — the table lookup happens at run time), an argument position, a type
    /// parameter's declaration index, a bit width, a count. This is the class a table index gets
    /// mistaken for.
    Ordinal,
    /// Keyed by [`Span`], with a payload that holds no integer index at all — a type, a name, a
    /// layout, a flag, or nothing (a bare span set). Span keys are stable across installs (a
    /// [`Span`] carries its `SourceId`), so nothing here is renumbered.
    SpanKeyed,
    /// Not span-keyed: a `Vec` or a name-keyed table of pure payload, consumed by content.
    Content,
}

/// **Every field of [`Sites`]**, with its [`SiteClass`] and what its integer payload (if any)
/// actually indexes.
///
/// [`Sites`] is the *input* to the compile pipeline whose own tables are classified by
/// `noeta_compiler::TABLE_POLICIES`. That table exists because four bugs lived in the delta between
/// a cold compile and a session install; this one exists because the bundle those installs consume
/// had thirty-five fields, one hand-maintained sentence claiming a property of all of them, and no
/// check. Adding a thirty-sixth field that carries a type-argument table index is the natural shape
/// of any future call-site-typed feature — three fields already hold a `u32` that *looks* like one
/// — and it would compile clean, pass every cold test, and be wrong only in a REPL or a hot swap.
///
/// Machine-checked by `noeta-check/tests/site_policies.rs`, which reads this file as source text and
/// requires:
///
/// * every declared field to appear here exactly once, with a note (adding a field fails the gate);
/// * the fields BOUND (rather than `_`-ignored) by [`Sites::remap_type_arg_indices`]'s destructure
///   to be exactly the [`SiteClass::TableIndexed`] rows — the census cannot claim a remap the code
///   does not perform, or hide one it does;
/// * the fields `noeta_compiler::SessionCompiler::absorb_type_args` overwrites to be exactly the
///   [`SiteClass::TheTable`] rows;
/// * any field whose declared type mentions [`noeta_ext_abi::TypeArgIndex`] — directly, or through
///   an ABI type that carries one, which is how `hidden_arg_sites` does it — to be classified
///   [`SiteClass::TableIndexed`]. This is the half the newtype bought: a table index written in the
///   type cannot be classified as anything else.
///
/// What no gate here can catch is a table index written as a **bare `u32`** and classified
/// [`SiteClass::Ordinal`] or [`SiteClass::SpanKeyed`]. Nothing in the type distinguishes it, and no
/// text scan can. That is what [`noeta_ext_abi::TypeArgIndex`] exists to make unnecessary, what the
/// `Ordinal` note is for (say what the integer indexes, and the answer "the type-argument table" is
/// the wrong answer to write there), and what the non-identity absorption oracle in
/// `noeta-vm/tests/hotswap.rs` catches behaviorally once such a field reaches lowering.
///
/// Read as source *text* by the gate rather than as a value, like `TABLE_POLICIES`: its job is to
/// be impossible to omit, not to be executed.
#[allow(dead_code)]
#[rustfmt::skip]
pub(crate) const SITE_POLICIES: &[(&str, SiteClass, &str)] = &[
    ("type_of_sites", SiteClass::SpanKeyed, "span → TypeRepr: a structural type (names and widths), nothing numbered"),
    ("construction_sites", SiteClass::SpanKeyed, "span → TypeRepr, statically interned at the site; the DYNAMIC twin is the row below"),
    ("dynamic_construction_sites", SiteClass::Ordinal, "the enclosing body's hidden SLOT ordinal ($tyN); the table lookup happens at run time, through the slot's value"),
    ("inferred_object_types", SiteClass::SpanKeyed, "span → the nominal type name a `.{ … }` resolved to"),
    ("variant_pattern_sites", SiteClass::SpanKeyed, "span → (enum qualifier, variant name)"),
    ("packed_list_sites", SiteClass::SpanKeyed, "span → PackedLayout: field names, kinds and bit widths; no index"),
    ("from_bytes_validated", SiteClass::SpanKeyed, "a bare span set"),
    ("fields_of_private", SiteClass::SpanKeyed, "a bare span set"),
    ("typed_module_call_sites", SiteClass::SpanKeyed, "span → TypeRecipe: structural; its VariantRecipe.index is an enum's DECLARATION position"),
    ("typed_method_call_sites", SiteClass::SpanKeyed, "span → TypeRecipe, the extern-method twin of the row above"),
    ("deserialize_recipes", SiteClass::Content, "Vec<(type name, TypeRecipe)>, lifted into a name-keyed runtime registry"),
    ("decode_typed_sites", SiteClass::SpanKeyed, "a bare span set"),
    ("map_packed_sites", SiteClass::SpanKeyed, "span → PackedLayout"),
    ("bundle_schema_layouts", SiteClass::Content, "Vec<PackedLayout>, interned into the module's packed_schemas by content"),
    ("packed_type_layouts", SiteClass::Content, "Vec<PackedLayout>, interned by (qualified) name"),
    ("index_field_sites", SiteClass::SpanKeyed, "a bare span set"),
    ("arg_orders", SiteClass::Ordinal, "each usize is an ARGUMENT position in written order, permuted by lowering"),
    ("for_stream_sites", SiteClass::SpanKeyed, "a bare span set"),
    ("width_sites", SiteClass::Ordinal, "(signed, bits): a fixed-width arithmetic BIT WIDTH, not an index"),
    ("render_hint_sites", SiteClass::SpanKeyed, "span → RenderHint: structural; its Slots/Variants numbers are SLOT positions within the rendered value"),
    ("json_hint_sites", SiteClass::SpanKeyed, "span → RenderHint: structural; its Slots/Variants numbers are SLOT positions within the serialized value"),
    ("order_hint_sites", SiteClass::SpanKeyed, "span → RenderHint: structural; its Slots/Variants numbers are SLOT positions within the ordered (or joined) value"),
    ("binding_hint_sites", SiteClass::SpanKeyed, "span → RenderHint: structural; its Slots/Variants numbers are SLOT positions within the value serialized later"),
    ("echo_hint_sites", SiteClass::SpanKeyed, "span → RenderHint: structural; its Slots/Variants numbers are SLOT positions within the value a session echoes"),
    ("folded_type_tests", SiteClass::SpanKeyed, "span → the constant answer of a statically decided `is`"),
    ("handle_sites", SiteClass::SpanKeyed, "span → (type, method, associated)"),
    ("bound_handle_sites", SiteClass::SpanKeyed, "a bare span set"),
    ("type_param_assoc_sites", SiteClass::SpanKeyed, "span → the receiver type parameter's SPELLING; no index"),
    ("field_call_sites", SiteClass::SpanKeyed, "a bare span set"),
    ("member_method_call_sites", SiteClass::SpanKeyed, "a bare span set"),
    ("trait_call_sites", SiteClass::SpanKeyed, "span → (trait qualified identity, method)"),
    ("namespace_module_sites", SiteClass::SpanKeyed, "span → the root-qualified module identity"),
    ("f32_literal_sites", SiteClass::SpanKeyed, "a bare span set"),
    ("try_conversion_sites", SiteClass::SpanKeyed, "span → (target error type name, the conversion's method-table key)"),
    ("from_call_sites", SiteClass::SpanKeyed, "span → the conversion's method-table key an explicit `Target.from(x)` selected"),
    ("type_arg_table", SiteClass::TheTable, "the table itself; absorb_type_args REPLACES it with the session's merged superset"),
    ("type_arg_reprs", SiteClass::TheTable, "the table's reflection projection, indexed in lockstep; replaced with it"),
    ("type_arg_hints", SiteClass::TheTable, "the table's render-hint projection, indexed in lockstep; replaced with it"),
    ("hidden_arg_sites", SiteClass::TableIndexed, "HiddenArg::Table(TypeArgIndex) — the one carrier; the Forward(j) beside it is a slot ordinal and passes through"),
    ("type_arg_compositions", SiteClass::TableIndexed, "HintCase leaves and composed are both type-arg TABLE indices (or NO_TYPE_ARG); the HintComposition::leaves beside them are slot ordinals"),
    ("forwarded_slot_sites", SiteClass::Ordinal, "a hidden SLOT ordinal ($tyN) of the enclosing forwarding fn, read at run time"),
    ("self_type_arg_sites", SiteClass::Ordinal, "the type parameter's position in its own type's declaration order, read off the receiver's reflected tag"),
    ("forwarding_fns", SiteClass::Ordinal, "the callable's hidden-slot COUNT (Func::hidden), keyed by traced name"),
    ("self_render_fns", SiteClass::Ordinal, "the method's receiver-read render-slot COUNT, keyed by traced name; the ordinals sit after forwarding_fns' slots"),
    ("fn_value_sites", SiteClass::Ordinal, "the fn's VALUE-parameter arity; its hidden atoms live in hidden_arg_sites under the same span"),
    ("destructor_relevance", SiteClass::Content, "spans, (span, param name) pairs and type names; nothing numbered"),
];

impl Sites {
    /// Rewrite every type-argument **table** index in this bundle through `remap` (a freshly-checked
    /// index → the session index the same entry was merged to), in place.
    ///
    /// Called by `noeta_compiler::SessionCompiler::absorb_type_args` and nowhere else, and split out
    /// to here on purpose: *which fields carry a table index* is a fact about [`Sites`], so it
    /// belongs beside [`Sites`] and beside [`SITE_POLICIES`], where the author of a thirty-sixth
    /// field is already looking — not in the compiler, where a reader has no reason to check the
    /// list is still complete.
    ///
    /// **The destructure is exhaustive and has no `..`, deliberately.** A new field fails to
    /// *compile* here until its author says which arm it belongs in, and the arms are the only two
    /// answers there are: remapped, or explicitly not. That is the compile-time half of the census;
    /// the gate is the half that checks the answer against [`SITE_POLICIES`] and against the field's
    /// own type.
    pub fn remap_type_arg_indices(&mut self, remap: &[noeta_ext_abi::TypeArgIndex]) {
        let Sites {
            // Carries a type-arg TABLE index. Bound, because it is rewritten below — and the gate
            // reads exactly that: a binding here must be a `TableIndexed` row of `SITE_POLICIES`.
            hidden_arg_sites,
            type_arg_compositions,
            // The tables themselves. Not remapped but REPLACED, by the caller, with the merged
            // superset — see the `TheTable` rows.
            type_arg_table: _,
            type_arg_reprs: _,
            type_arg_hints: _,
            // Everything else: no index into the type-argument table. Three of these hold a `u32`
            // that looks like one (`dynamic_construction_sites`, `forwarded_slot_sites`,
            // `self_type_arg_sites`); `SITE_POLICIES` says what each actually indexes, and the one
            // thing none of them indexes is this table.
            type_of_sites: _,
            construction_sites: _,
            dynamic_construction_sites: _,
            inferred_object_types: _,
            variant_pattern_sites: _,
            packed_list_sites: _,
            from_bytes_validated: _,
            fields_of_private: _,
            typed_module_call_sites: _,
            typed_method_call_sites: _,
            deserialize_recipes: _,
            decode_typed_sites: _,
            map_packed_sites: _,
            bundle_schema_layouts: _,
            packed_type_layouts: _,
            index_field_sites: _,
            arg_orders: _,
            for_stream_sites: _,
            width_sites: _,
            render_hint_sites: _,
            json_hint_sites: _,
            order_hint_sites: _,
            binding_hint_sites: _,
            echo_hint_sites: _,
            folded_type_tests: _,
            handle_sites: _,
            bound_handle_sites: _,
            type_param_assoc_sites: _,
            field_call_sites: _,
            member_method_call_sites: _,
            trait_call_sites: _,
            namespace_module_sites: _,
            f32_literal_sites: _,
            try_conversion_sites: _,
            from_call_sites: _,
            forwarded_slot_sites: _,
            self_type_arg_sites: _,
            forwarding_fns: _,
            self_render_fns: _,
            fn_value_sites: _,
            destructor_relevance: _,
        } = self;

        for slots in hidden_arg_sites.values_mut() {
            for slot in slots.iter_mut() {
                if let noeta_ext_abi::HiddenArg::Table(i) = slot
                    && let Some(&to) = remap.get(i.get() as usize)
                {
                    *slot = noeta_ext_abi::HiddenArg::Table(to);
                }
            }
        }
        // A composition's cases are table indices on BOTH sides — the leaf slots' values it matches
        // on and the entry it composes to — and a value the fresh check never interned stays
        // `NO_TYPE_ARG`, which indexes nothing in either numbering.
        let rewrite = |v: &mut i64| {
            if *v >= 0
                && let Some(&to) = remap.get(*v as usize)
            {
                *v = i64::from(to.get());
            }
        };
        for composition in type_arg_compositions.iter_mut() {
            for case in composition.cases.iter_mut() {
                for leaf in case.leaves.iter_mut() {
                    rewrite(leaf);
                }
                rewrite(&mut case.composed);
            }
        }
    }
}

/// **The type-argument table's interning key, and the only place it is applied.** Look `(info,
/// repr)` up in the parallel tables [`Sites::type_arg_table`] / [`Sites::type_arg_reprs`], returning
/// the existing entry's index or appending a new one to *both* and returning that.
///
/// Two callers, deliberately one function. The checker
/// ([`Checker::intern_type_arg`](crate::Checker::intern_type_arg)) builds the table while checking;
/// a LIVE session (`noeta_compiler::SessionCompiler::absorb_type_args`) re-runs the very same
/// interning to ABSORB a freshly-checked table into the one its running values already index. If
/// those two disagreed about the key, the session would fold two entries the checker kept apart —
/// or split one it merged — and a hidden type-argument slot would silently resolve to the wrong
/// type with nothing to crash. The key has already widened once (the repr joined it when generic
/// classes started constructing from their own `T`); this is what makes the next widening reach
/// both sides at once.
///
/// The key is the PAIR. `TypeArgInfo::name` is head-keyed and a class carries no decode recipe, so
/// `Repository<Todo>` and `Repository<Order>` produce an identical [`noeta_ext_abi::TypeArgInfo`]
/// and are told apart only by the repr — see [`Sites::type_arg_reprs`].
pub fn intern_type_arg_entry(
    table: &mut Vec<noeta_ext_abi::TypeArgInfo>,
    reprs: &mut Vec<Option<noeta_ast::reflect::TypeRepr>>,
    hints: &mut Vec<noeta_ext_abi::TypeArgHints>,
    info: noeta_ext_abi::TypeArgInfo,
    repr: Option<noeta_ast::reflect::TypeRepr>,
    hint: noeta_ext_abi::TypeArgHints,
) -> noeta_ext_abi::TypeArgIndex {
    debug_assert_eq!(
        table.len(),
        reprs.len(),
        "the type-argument table and its reflection projection are grown in lockstep"
    );
    debug_assert_eq!(
        table.len(),
        hints.len(),
        "the type-argument table and its render-hint projection are grown in lockstep"
    );
    // The ONE place a `TypeArgIndex` is minted. Its type — rather than the `u32` it wraps — is what
    // marks the value as "an index a live session has to renumber"; see `TypeArgIndex`.
    match table
        .iter()
        .zip(reprs.iter())
        .zip(hints.iter())
        .position(|((e, r), h)| *e == info && *r == repr && *h == hint)
    {
        Some(i) => noeta_ext_abi::TypeArgIndex::new(i as u32),
        None => {
            table.push(info);
            reprs.push(repr);
            hints.push(hint);
            noeta_ext_abi::TypeArgIndex::new((table.len() - 1) as u32)
        }
    }
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
    /// See [`Checked::diverging_stmts`](crate::Checked::diverging_stmts) — the spans of
    /// statement-expressions whose expression types as [`noeta_types::Type::Never`]. Always
    /// populated (unlike `expr_types`): it is one `HashSet` insert on the rare statement that
    /// diverges, and empty for every program that has none.
    pub(crate) diverging_stmts: HashSet<Span>,
    /// Every **expression** whose inferred type is [`noeta_types::Type::Never`] — the raw fact
    /// `diverging_stmts` is the statement-level projection of.
    ///
    /// Checker-internal, and read by exactly one consumer: [`crate::subst::expr_diverges`], the
    /// must-diverge analysis behind E0048 ("this function can reach the end of its body"). That
    /// analysis used to hard-code the single name `panic`, so a user-written
    /// `fn die(msg: string): never` was not recognised and every caller that ended in `die(…)` was
    /// rejected — the feature would have been decorative. Carried across by span exactly as
    /// [`crate::Checker::exhaustive_matches`] is, and read only after the body is typed.
    pub(crate) never_exprs: HashSet<Span>,
    pub(crate) construction_sites: HashMap<Span, noeta_ast::reflect::TypeRepr>,
    /// See [`Sites::dynamic_construction_sites`].
    pub(crate) dynamic_construction_sites: HashMap<Span, u32>,
    /// See [`Sites::inferred_object_types`].
    pub(crate) inferred_object_types: HashMap<Span, String>,
    /// See [`Sites::variant_pattern_sites`].
    pub(crate) variant_pattern_sites: HashMap<Span, (Option<String>, String)>,
    /// List-construction sites whose element type is a `@packed` struct (P-PACK Phase 2), keyed by the
    /// constructing expression's span → the element's flat [`PackedLayout`]. Both backends consult this
    /// via [`resolve_packed_list_sites`] to lay out a `List<packed>` as one contiguous raw-primitive
    /// buffer instead of N boxed objects. A pure function of the program, like `type_of_sites`, so the
    /// two backends pick the same representation by construction (the flat layout stays invisible to
    /// `RunResult`).
    pub(crate) packed_list_sites: HashMap<Span, noeta_ast::reflect::PackedLayout>,
    /// `from_bytes::<T>` spans whose packed element type implements `Validate` — see
    /// [`Sites::from_bytes_validated`].
    pub(crate) from_bytes_validated: HashSet<Span>,
    /// `fields_of` spans whose operand's private fields are visible — see
    /// [`Sites::fields_of_private`].
    pub(crate) fields_of_private: HashSet<Span>,
    /// Call-site-typed native-call recipes (`json.parse::<T>`), keyed by the `Expr::TypedModuleCall`
    /// span → the turbofish `T` resolved into a [`noeta_ext_abi::TypeRecipe`]. Both backends harvest
    /// this on the same program, so the lowering bakes identical recipes into `Rvalue::TypedModuleCall`.
    pub(crate) typed_module_call_sites: HashMap<Span, noeta_ext_abi::TypeRecipe>,
    /// Call-site-typed **extern-method** recipes (`resp.json::<T>`, http arc H8): the turbofish `T`
    /// resolved into a [`noeta_ext_abi::TypeRecipe`] the lowering bakes into
    /// `Rvalue::TypedMethodCall`. The extern-type twin of `typed_module_call_sites` —
    /// presence here is exactly what distinguishes a *native typed* method call from an ordinary
    /// (erased) generic-method instantiation. A pure function of the program.
    pub(crate) typed_method_call_sites: HashMap<Span, noeta_ext_abi::TypeRecipe>,
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
    /// `@packed` structs bound to a `vec` bundle (scalar-unification slice 3) — see [`Sites`].
    pub(crate) bundle_schema_layouts: Vec<noeta_ast::reflect::PackedLayout>,
    /// Member-access spans (`list[i].field`) the checker proved fusable: the index receiver is a
    /// built-in `List` and the field resolves on its element type. Lowering reads this (via
    /// [`Checked::index_field_sites`]) to emit a single [`Rvalue::IndexField`] that reads a packed
    /// element's field without materializing the element (P-PACK 2.5+). A pure function of the
    /// program, invisible to `RunResult`, so both backends fuse the same sites by construction.
    pub(crate) index_field_sites: HashSet<Span>,
    /// Call spans whose arguments need rebinding: for each parameter position, the index of the
    /// argument in **written** order, or `None` where the parameter was skipped and the callee
    /// must fill its default.
    ///
    /// The checker resolves the binding — it is the only pass that knows the callee's parameter
    /// names — and lowering permutes the evaluated atoms, so both backends bind identically by
    /// construction. Absent for a purely positional call, which is already in order.
    pub(crate) arg_orders: HashMap<Span, Vec<Option<usize>>>,
    /// Call spans reached through a pipeline (`x |> f(…)`), recorded by
    /// [`Checker::synth_piped`] before it types the desugared call.
    ///
    /// Argument binding needs this because the piped value is the one argument with no written
    /// position: it fills the first parameter no label claimed, rather than parameter zero. A set
    /// rather than a single "current" span so a pipeline nested in another's arguments
    /// (`a |> f(b |> g(k: 1))`) marks both calls.
    ///
    /// Checker-internal — lowering knows a pipeline from its own AST node, so this is deliberately
    /// not projected into [`Sites`].
    pub(crate) piped_calls: HashSet<Span>,
    /// `for` statement spans whose iterable is statically an `Iterator<T>` — the loop streams via
    /// `next()` instead of snapshotting a list (Track I.2). Lowering reads this (via
    /// [`Checked::for_stream_sites`]) to set `Stmt::For.stream`. A pure function of the program; a
    /// collection or `dyn` iterable is absent here and keeps the snapshot/cursor fast path.
    pub(crate) for_stream_sites: HashSet<Span>,
    /// Fixed-width arithmetic sites (Tier W): the span of a same-width `+ - *` / unary `-` on an
    /// `IntN` → the result's `(signed, bits)`. Lowering reads this (via [`Checked::width_sites`]) to
    /// wrap the op's result in `Rvalue::MaskWidth`. Empty for programs with no fixed-width arithmetic.
    pub(crate) width_sites: HashMap<Span, (bool, u8)>,
    /// Display sites carrying an unsigned 64-bit integer — see [`Sites::render_hint_sites`].
    pub(crate) render_hint_sites: HashMap<Span, noeta_ast::RenderHint>,
    /// JSON sites carrying an unsigned 64-bit integer — see [`Sites::json_hint_sites`].
    pub(crate) json_hint_sites: HashMap<Span, noeta_ast::RenderHint>,
    /// Ordering sites carrying an unsigned 64-bit integer — see [`Sites::order_hint_sites`].
    pub(crate) order_hint_sites: HashMap<Span, noeta_ast::RenderHint>,
    /// Deferred-serialization sites carrying an unsigned 64-bit integer — see
    /// [`Sites::binding_hint_sites`].
    pub(crate) binding_hint_sites: HashMap<Span, noeta_ast::RenderHint>,
    /// Session-echo sites carrying an unsigned 64-bit integer — see [`Sites::echo_hint_sites`].
    pub(crate) echo_hint_sites: HashMap<Span, noeta_ast::RenderHint>,
    /// Statically decided type tests — see [`Sites::folded_type_tests`].
    pub(crate) folded_type_tests: HashMap<Span, bool>,
    /// Unbound method-handle sites: a `Type.method` member expression in value position → the
    /// resolved `(ty, method, associated)`. Lowering reads this (via [`Checked::handle_sites`]) to
    /// emit an [`Rvalue::MethodHandle`] instead of a field load. A pure function of the program.
    pub(crate) handle_sites: HashMap<Span, (String, String, bool)>,
    /// **Bound**-handle sites (`value.method` in value position, EX.2b): spans whose `Member`
    /// lowers to an [`Rvalue::BoundHandle`] (receiver captured) instead of a field load.
    pub(crate) bound_handle_sites: HashSet<Span>,
    /// Type-parameter associated-call sites (`T.m(…)`) — see [`Sites::type_param_assoc_sites`].
    pub(crate) type_param_assoc_sites: HashMap<Span, String>,
    /// Field-call sites (`obj.f(args)` where `f` is a field) — see [`Sites::field_call_sites`].
    pub(crate) field_call_sites: HashSet<Span>,
    /// Generic-method turbofish spans reached via the `TypedModuleCall` surface (D3) — see
    /// [`Sites::member_method_call_sites`].
    pub(crate) member_method_call_sites: HashSet<Span>,
    /// Bare float-literal spans adapted into an `f32` context (P-NUM-SYM) — lowering reads this (via
    /// [`Checked::f32_literal_sites`]) to emit a narrow `Const::F32` for the literal.
    pub(crate) f32_literal_sites: HashSet<Span>,
    /// Trait method call sites (slice 2 default bodies + slice 4 kernel traits) — see
    /// [`Sites::trait_call_sites`].
    pub(crate) trait_call_sites: HashMap<Span, (String, String)>,
    /// Namespace-group member-access sites — see [`Sites::namespace_module_sites`].
    pub(crate) namespace_module_sites: HashMap<Span, String>,
    /// `?`-conversion sites (error-ergonomics) — see [`Sites::try_conversion_sites`].
    pub(crate) try_conversion_sites: HashMap<Span, (String, String)>,
    /// Explicit `Target.from(x)` conversion-selection sites — see [`Sites::from_call_sites`].
    pub(crate) from_call_sites: HashMap<Span, String>,
    /// The type-argument table (poly-values F2b) — see [`Sites::type_arg_table`].
    pub(crate) type_arg_table: Vec<noeta_ext_abi::TypeArgInfo>,
    /// The type-argument table's reflection projection — see [`Sites::type_arg_reprs`].
    pub(crate) type_arg_reprs: Vec<Option<noeta_ast::reflect::TypeRepr>>,
    /// The type-argument table's render-hint projection — see [`Sites::type_arg_hints`].
    pub(crate) type_arg_hints: Vec<noeta_ext_abi::TypeArgHints>,
    /// Forwarding-call hidden-argument slots — see [`Sites::hidden_arg_sites`].
    pub(crate) hidden_arg_sites: HashMap<Span, Vec<noeta_ext_abi::HiddenArg>>,
    /// Render-slot compositions — see [`Sites::type_arg_compositions`].
    pub(crate) type_arg_compositions: Vec<noeta_ext_abi::HintComposition>,
    /// Forwarded-type-parameter turbofish sites — see [`Sites::forwarded_slot_sites`].
    pub(crate) forwarded_slot_sites: HashMap<Span, u32>,
    /// Enclosing-type type-argument reflection sites — see [`Sites::self_type_arg_sites`].
    pub(crate) self_type_arg_sites: HashMap<Span, (String, u32)>,
    /// Forwarding fns' hidden-parameter counts — see [`Sites::forwarding_fns`].
    pub(crate) forwarding_fns: HashMap<String, u32>,
    /// Receiver-read render-slot counts — see [`Sites::self_render_fns`].
    pub(crate) self_render_fns: HashMap<String, u32>,
    /// Forwarding-fn-as-value wrap sites — see [`Sites::fn_value_sites`].
    pub(crate) fn_value_sites: HashMap<Span, (String, u32)>,
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
            dynamic_construction_sites: self.dynamic_construction_sites,
            inferred_object_types: self.inferred_object_types,
            variant_pattern_sites: self.variant_pattern_sites,
            packed_list_sites: self.packed_list_sites,
            from_bytes_validated: self.from_bytes_validated,
            fields_of_private: self.fields_of_private,
            typed_module_call_sites: self.typed_module_call_sites,
            typed_method_call_sites: self.typed_method_call_sites,
            deserialize_recipes: self.deserialize_recipes,
            decode_typed_sites: self.decode_typed_sites,
            map_packed_sites: self.map_packed_sites,
            bundle_schema_layouts: self.bundle_schema_layouts,
            // Seeded by the `Checked` producers from `packed_layouts_public` (which reads `symbols`,
            // out of reach here); `into_sites` projects only the accumulator's own maps, so this
            // stays empty until the producer fills it. See [`Sites::packed_type_layouts`].
            packed_type_layouts: Vec::new(),
            index_field_sites: self.index_field_sites,
            arg_orders: self.arg_orders,
            for_stream_sites: self.for_stream_sites,
            width_sites: self.width_sites,
            render_hint_sites: self.render_hint_sites,
            json_hint_sites: self.json_hint_sites,
            order_hint_sites: self.order_hint_sites,
            binding_hint_sites: self.binding_hint_sites,
            echo_hint_sites: self.echo_hint_sites,
            folded_type_tests: self.folded_type_tests,
            handle_sites: self.handle_sites,
            bound_handle_sites: self.bound_handle_sites,
            type_param_assoc_sites: self.type_param_assoc_sites,
            field_call_sites: self.field_call_sites,
            member_method_call_sites: self.member_method_call_sites,
            f32_literal_sites: self.f32_literal_sites,
            trait_call_sites: self.trait_call_sites,
            namespace_module_sites: self.namespace_module_sites,
            try_conversion_sites: self.try_conversion_sites,
            from_call_sites: self.from_call_sites,
            type_arg_table: self.type_arg_table,
            type_arg_reprs: self.type_arg_reprs,
            type_arg_hints: self.type_arg_hints,
            hidden_arg_sites: self.hidden_arg_sites,
            type_arg_compositions: self.type_arg_compositions,
            forwarded_slot_sites: self.forwarded_slot_sites,
            self_type_arg_sites: self.self_type_arg_sites,
            forwarding_fns: self.forwarding_fns,
            self_render_fns: self.self_render_fns,
            fn_value_sites: self.fn_value_sites,
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
    ///
    /// A **`BTreeSet`, and that is load-bearing**: the compiler collects this field straight into
    /// `Module::destruct_reachable`, which is serialized into the `.noeb` bundle. A `HashSet` here
    /// made that table's order the hasher's random per-process seed, so the *same program compiled
    /// to different bytes on every run* — reproducible builds, content-addressed caching, and any
    /// artifact diff were all off the table for as long as it stayed one. A set whose order is
    /// observable downstream is a set that must have one, so it gets it here at the source rather
    /// than by a sort at each collect site (which the next collect site would forget). The
    /// membership queries the analysis itself runs go through
    /// [`Symbols::destruct_reachable`](crate::Symbols), still hash-backed — this set is built once
    /// per check and only iterated.
    pub reachable_types: BTreeSet<String>,
}
