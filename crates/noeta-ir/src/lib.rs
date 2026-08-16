//! The **Core IR** — a lowered, A-normal-form (ANF) second representation of the
//! language, shared by every backend from the memory-management migration onward.
//!
//! # Why an IR, and why ANF
//!
//! The tree-walker and the register VM each re-derive evaluation order from the AST as
//! they go. That works, but it leaves the *intermediate* values of an expression
//! anonymous: in `acc.x + 1`, the receiver-field load `acc.x` has no AST node of its own,
//! so neither backend can name it, compute its last use, or reuse its storage. The whole
//! point of the Core IR is to make every intermediate value an explicitly **named** `let`
//! binding over **atoms** (a constant or a name), so a later pass can compute precise
//! last-use points and attach reference-counting decisions to concrete IR nodes.
//!
//! A-normal form gives exactly that: `acc.x + 1` lowers to `let t0 = acc.x; let t1 = t0 +
//! 1`. Evaluation order — the tree-walker's order, which the VM already matches — becomes
//! *explicit structure* (the `let` sequence) rather than something each backend rederives.
//! That is a correctness win on its own, independent of reference counting.
//!
//! # Shape of the IR
//!
//! Control flow stays **structured** (no arbitrary `goto`): `if`/`while`/`for`/`match` are
//! nodes with sub-[`Block`]s, not a basic-block graph. A structured tree keeps the
//! backward last-use walk (a later phase) a simple structured traversal and keeps the IR
//! tree-interpreter straightforward.
//!
//! Two storage classes are deliberately kept separate:
//!
//! * **Source variables** ([`Atom::Var`]) stay name-keyed and live in the runtime's lexical
//!   scope chain. Closures capture them, reassignment flows through them, and end-of-program
//!   destruction drains them — all exactly as before. Lowering never renames them.
//! * **Temporaries** ([`Temp`]) are the anonymous intermediates ANF introduces. They are
//!   frame-local (one flat store per function activation), write-once, and **never escape
//!   into a closure** — a closure body only ever references source variables (which it
//!   captures by name) plus its own params and temps. This separation is what lets a
//!   temporary's storage be reclaimed at its last use without disturbing the scope model.
//!
//! # Reserved reference-counting slots
//!
//! IR nodes carry no `dup`/`drop`/`reuse`/`in-place` annotations *yet*. Those are filled by
//! a later phase; this crate fixes the node shapes both backends will consume so that phase
//! is purely additive. Until then the IR is a faithful, RC-neutral mirror of the AST.

use std::rc::Rc;

use noeta_span::Span;

pub use noeta_ast::{BinaryOp, ForPattern, Pattern, TypeRef, UnaryOp};

mod lower;
mod pretty;
pub use lower::{
    LowerOptions, LoweringSites, ProgramFacts, Unsupported, hoist_impl_methods_with_registry,
    hoist_standalone_impl_methods, lower, lower_with_sites, lower_with_sites_opts,
    native_trait_impls,
};
pub use pretty::dump;

/// A whole lowered program: the top-level statement stream plus the size of its temporary
/// frame. Top-level source bindings still land in the global scope (so end-of-program
/// destruction is unchanged); `temp_count` sizes the flat temporary store the interpreter
/// allocates for the top-level activation.
#[derive(Debug, Clone)]
pub struct Program {
    pub top: Block,
    /// The number of distinct [`Temp`]s used anywhere in `top` (the top-level frame size).
    pub temp_count: u32,
    /// The program-wide **type-argument table** (poly-values F2b): the concrete instantiations of
    /// forwarding generics, indexed by the hidden call arguments. Both backends resolve dynamic
    /// call-site-typed sites (`json.try_parse::<T>` with a forwarded `T`) through this one table,
    /// so they agree by construction. Empty for programs without forwarding.
    pub type_args: Vec<noeta_ext_abi::TypeArgInfo>,
    /// The **reflection projection** of [`Program::type_args`], indexed identically: each interned
    /// instantiation's [`noeta_ast::reflect::TypeRepr`], or `None` where it has none. Read by
    /// [`Rvalue::Method::reflect_slot`] — a construction whose instantiation arrives on a hidden slot
    /// resolves its tag here, so the tag is still a statically-interned repr and only the *choice* of
    /// entry is dynamic. A parallel table because `noeta_ext_abi::TypeArgInfo` may not depend on
    /// `noeta-ast` (the lean-ABI-crate decision).
    pub type_arg_reprs: Vec<Option<noeta_ast::reflect::TypeRepr>>,
    pub span: Span,
}

/// A frame-local temporary: an ANF-introduced intermediate value. Write-once, read-many
/// within its function activation, and never captured by a closure. Identified by a dense
/// index into the activation's flat temporary store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Temp(pub u32);

impl Temp {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// An **atom**: an operand that needs no further computation — a literal constant or a
/// name. Every [`Rvalue`] operand is an atom; that is the defining property of A-normal
/// form (nesting is flattened into preceding `let`s until only atoms remain).
#[derive(Debug, Clone)]
pub enum Atom {
    /// A literal constant.
    Const(Const),
    /// A frame-local temporary, read from the activation's temporary store.
    Temp(Temp),
    /// A source-level variable, resolved through the lexical scope chain. Kept by name (not
    /// renamed to a temp) so captures, reassignment, and end-of-scope destruction match the
    /// tree-walker exactly.
    Var { name: String, span: Span },
}

/// A literal constant operand.
#[derive(Debug, Clone, PartialEq)]
pub enum Const {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A 32-bit float literal (`1.0f32`, P-PACK Phase 3).
    F32(f32),
    Str(String),
}

/// One owned local to drop on a `?`-operator's error path: its source name and whether its type is
/// destructor-relevant (so the backend runs `destruct` only when it can fire). See [`Rvalue::Try`].
#[derive(Debug, Clone, PartialEq)]
pub struct TryDrop {
    pub name: String,
    pub relevant: bool,
}

/// One `name: atom` initializer in an [`Rvalue::Object`] literal, keeping the field-name span
/// for diagnostics.
#[derive(Debug, Clone)]
pub struct ObjectFieldInit {
    pub name: String,
    pub name_span: Span,
    pub value: Atom,
}

/// One part of an interpolated string, with its holes reduced to atoms. A hole keeps the
/// source span of its original expression so display-time diagnostics point at it exactly as
/// the tree-walker's do.
#[derive(Debug, Clone)]
pub enum InterpPart {
    Literal(String),
    Hole { atom: Atom, span: Span },
}

/// A **primitive operation** over atoms — the right-hand side of a `let` (or an evaluated-
/// for-effect statement). Each variant computes a single value from already-evaluated
/// operands. Operations that *short-circuit* (`&&`, `||`, `??`) or that *branch on a value*
/// (`match`, the `if`/`while`/`for` of the surface) are **not** here — those lower to the
/// structured control-flow [`Stmt`]s, because their sub-expressions must not all be
/// evaluated up-front into atoms.
#[derive(Debug, Clone)]
pub enum Rvalue {
    /// Move/copy an atom into a temp (`let t = other`).
    Use(Atom),
    /// A prefix unary operation.
    Unary {
        op: UnaryOp,
        operand: Atom,
        span: Span,
    },
    /// Reduce a value to its fixed-width integer range (Tier W). Emitted by lowering immediately
    /// after a width-bearing op — same-width `+ - *` and unary `-` on an `IntN` — because fixed-width
    /// values are erased to i64 and the arithmetic runs full-width; this wraps the result back into
    /// the declared width via `noeta_ext_abi::mask_to_width(value, signed, bits)`. Both backends apply
    /// the identical helper, so wraparound agrees by construction. Pure, single-operand (like
    /// [`Rvalue::Unary`]); never appears on the boxed/REPL path (only IntN arithmetic produces it).
    MaskWidth {
        operand: Atom,
        signed: bool,
        bits: u8,
        span: Span,
    },
    /// Render a value to its **display string** under a [`noeta_ast::RenderHint`] — the display twin
    /// of [`Rvalue::MaskWidth`], and emitted for the same reason. A fixed-width integer is erased to
    /// its i64 word, so an unsigned value past bit 63 is a negative word and would print as its
    /// signed reinterpretation; the hint carries the signedness the *static type* holds and the
    /// render walk reads those words unsigned. Lowering emits it at the display sites the checker
    /// marked — `echo`, an interpolation hole, a display-based `~` operand — and only there, so a
    /// value with no unsigned integer in its type never meets one. Both backends run the identical
    /// hinted walk, so the differential pins them equal. Pure, single-operand; the result is a
    /// `string`, which every downstream display door renders as itself.
    Render {
        operand: Atom,
        hint: std::rc::Rc<noeta_ast::RenderHint>,
        span: Span,
    },
    /// Serialize a value to **JSON text** under a [`noeta_ast::RenderHint`] — the wire twin of
    /// [`Rvalue::Render`], and emitted for the same reason. An erased i64 word carries no
    /// signedness, so a `u64` past bit 63 would be *written to the wire* as its signed
    /// reinterpretation: a wrong number in an API response or a persisted record, with nothing to
    /// tell the reader. Lowering emits it in place of the serializing call at the JSON doors the
    /// checker marked — the `json.stringify` argument, a derived `to_json` receiver — and only
    /// there, so a program with no `u64` in a serialized position serializes through the untouched
    /// path. Both backends deep-marshal their own value and run the one hinted walk
    /// ([`noeta_ast::json_stringify`]), so the differential pins them equal. Pure, single-operand;
    /// the result is a `string`.
    JsonRender {
        operand: Atom,
        hint: std::rc::Rc<noeta_ast::RenderHint>,
        span: Span,
    },
    /// A non-short-circuiting infix operation. `&&`/`||` are lowered to control flow and
    /// never appear here; everything else (arithmetic, concat, comparisons, equality) does.
    /// Operator-trait overloading on user objects is resolved by the interpreter, not the
    /// IR.
    ///
    /// `reuse` is the **in-place-reuse token** for a list **self-append** (`acc = acc ~ rhs`,
    /// the desugaring of `acc ~= rhs`): set by the reuse-analysis pass ([`noeta_ir_passes`],
    /// Phase 5) only on a `Concat` whose `lhs` is the very binding the result is reassigned to
    /// (and whose `rhs` does not mention that binding), so the old list is dead after this op
    /// and its backing buffer may be extended in place. Both backends consume the same token —
    /// the VM via `ConcatInPlace` (with a `TakeGlobal` first for a global accumulator), the IR
    /// interpreter via a move-out (`take_mut`) + `cow_concat` — each gated on the runtime
    /// refcount (`== 1`) so an aliased list copies. It is meaningful only for `Concat`; lowering
    /// always emits `false` and the pass leaves every other operator untouched.
    Binary {
        op: BinaryOp,
        lhs: Atom,
        rhs: Atom,
        reuse: bool,
        span: Span,
    },
    /// A **sign-dependent** fixed-width integer op (Tier W3): `/ % < <= > >=` on two same-width
    /// `IntN`. Unlike `+ - *` (sign-agnostic — a plain [`Rvalue::Binary`] + [`Rvalue::MaskWidth`]
    /// suffices), division, remainder and ordering read the erased-i64 operands as signed or unsigned
    /// per `signed` (unsigned `u64` differs from signed once bit 63 is set), so the operation itself
    /// carries the width. Both backends apply the shared `apply_binary_wide`: `/ %` mask the result
    /// back into `bits` (so signed `MIN / -1` wraps), comparisons yield a bool. `op` is always one of
    /// `Div`/`Rem`/`Lt`/`Le`/`Gt`/`Ge`.
    WideInt {
        op: BinaryOp,
        lhs: Atom,
        rhs: Atom,
        signed: bool,
        bits: u8,
        span: Span,
    },
    /// A bit-manipulation intrinsic applied **within a fixed width** (Tier W5): `count_ones`/
    /// `leading_zeros`/`rotate_*`/`reverse_bits`/… on an `IntN` receiver, which must act on the low
    /// `bits` bits rather than the full erased i64 (`(1u8).leading_zeros() == 7`). Emitted (in place of
    /// a generic `Method`) when the checker marks a call site as an `IntN`-receiver intrinsic; both
    /// backends compute it via the shared `noeta_ext_abi::int_method_width`. `args` holds the sole
    /// `rotate_*` shift amount (empty for the nullary intrinsics). `method` is never `Convert` (a
    /// width-typed conversion stays an ordinary `Method`, resolved by `int_method`).
    WidthIntMethod {
        receiver: Atom,
        method: noeta_ext_abi::IntMethod,
        args: Vec<Atom>,
        bits: u8,
        span: Span,
    },
    /// A call of a callee value: `callee(args)`.
    Call {
        callee: Atom,
        args: Vec<Atom>,
        /// The **type arguments** this call supplies to a forwarding generic's leading
        /// [`Func::hidden`] slots (poly-values F2b), in slot order — empty for the overwhelming
        /// majority of calls, which forward nothing.
        ///
        /// A separate channel from `args` on purpose. Each atom is either an interned index into
        /// [`Program::type_args`] (a concrete instantiation) or a read of the enclosing function's
        /// own `$ty` slot (a pass-through). Carrying them here rather than prepending them onto
        /// `args` is what lets `supplied` keep meaning "which **value** parameters are supplied":
        /// while these rode in the argument list, every parameter position shifted and the binding
        /// map had to be discarded at any forwarding call.
        type_args: Vec<Atom>,
        /// Which **value** parameters `args` supplies, when that is not simply the first
        /// `args.len()` of them — bit `p` set means value parameter `p` is supplied, and `args`
        /// holds the supplied values in parameter order. Indexed over the value parameters alone;
        /// the callee's leading type-argument slots are not part of this space.
        ///
        /// `None` is the ordinary call: arguments fill parameters left to right and the callee
        /// defaults any trailing remainder. `Some` arises from named arguments that skip a
        /// defaulted parameter (`f(1, c: 9)`), which a count cannot express. The default is still
        /// evaluated by the CALLEE over its own upvalues — the mask says which to run, and changes
        /// nothing about where or when they run.
        supplied: Option<u64>,
        span: Span,
    },
    /// A method/associated call: `receiver.name(args)`. Kept distinct from [`Rvalue::Call`]
    /// so the interpreter can route it through method dispatch (built-in, stdlib, and
    /// user-defined) without reconstructing the receiver.
    ///
    /// `reuse` is the **in-place-reuse token** for a collection **method self-update** (`m = m.set(k,v)`
    /// / the `m[k] = v` desugaring): set by the reuse-analysis pass ([`noeta_ir_passes`], Phase 5) only
    /// on a whitelisted update method whose `receiver` is the very binding the result is reassigned to
    /// (and whose args do not mention it), so the old collection is dead after this call and its backing
    /// buffer may be mutated in place. Each backend gates the actual reuse on the runtime receiver kind
    /// and refcount (`== 1`), so an aliased or non-matching receiver copies — value semantics are
    /// preserved either way (reuse is observationally invisible), so the two backends agree even if they
    /// reuse at different points. Lowering always emits `false`.
    Method {
        receiver: Atom,
        name: String,
        name_span: Span,
        args: Vec<Atom>,
        reuse: bool,
        /// The checker-resolved reflected type when this "method call" is actually a **generic
        /// enum-variant construction** (`Tree.Leaf(5)` : `Tree<int>`, runtime type-argument
        /// reflection, R2b.2) — baked from the construction-site map so `type_of` recovers the enum's
        /// type arguments after a `dyn` launder. `None` for an ordinary method call (the common case)
        /// and for a non-generic enum. Invisible to value semantics.
        reflect: Option<noeta_ast::reflect::TypeRepr>,
        /// The **dynamic** twin of `reflect` (generic-in-generic construction): the enclosing body's
        /// hidden type-argument slot atom (`$ty<i>`) whose entry names the instantiation to stamp on
        /// the object this call freshly built.
        ///
        /// `Some` only where the checker recorded a
        /// [`dynamic construction site`](noeta_check::Sites::dynamic_construction_sites): a provably
        /// fresh constructor of a generic type whose instantiation is a type *parameter* of the
        /// enclosing self-less member (`Repository.new(…)` initializing a `repo: Repository<T>` inside
        /// `LiveRepository<T>`). One compiled body serves every instantiation, so the concrete
        /// `Repository<Todo>` is not in it — but the OUTER call resolved and interned it, and passes
        /// its table index on this slot. Both backends read the same index out of the same table, so
        /// they resolve the identical tag.
        ///
        /// Never `Some` together with `reflect`: a call site's instantiation is either in the body or
        /// on the slot, and the checker's two recording arms are mutually exclusive.
        reflect_slot: Option<Atom>,
        /// The [`Rvalue::Call::type_args`] twin (Axis A): the type arguments this call supplies to
        /// a forwarding generic **method**'s leading [`Func::hidden`] slots, in slot order — empty
        /// for the overwhelming majority of method calls.
        ///
        /// Only a call that resolved a static receiver type can fill this; the four name-keyed
        /// entry points a method has — a `dyn` receiver, a bound handle (`v.m`), an unbound handle
        /// (`T.m`), and `invoke(v, "m", args)` — carry no instantiation, which is exactly why the
        /// slots may not be smuggled in as prepended arguments: those paths bind positionally and
        /// would read a value argument as a type-table index. Reaching a forwarding method through
        /// one of them aborts instead ("no instantiation reaches here"), on the same precedent
        /// `class_type_param_unknown_instantiation` set for an unknowable instantiation on the
        /// receiver channel.
        type_args: Vec<Atom>,
        /// The [`Rvalue::Call::supplied`] twin. Indexed over the method's **declared** parameters,
        /// parallel to `args` — the receiver travels separately, so it takes no bit. A backend
        /// whose register layout places the receiver in parameter slot 0 (the VM's does) shifts
        /// the mask by one on the way in; one whose method scope holds only the declared
        /// parameters (the reference interpreter's) uses it as-is.
        supplied: Option<u64>,
        /// The **ordering hint** for a method that reveals an order a program can observe —
        /// `.sorted()`, `.min()`, `.max()` on a list, `.keys()`/`.values()` on a map — whose
        /// receiver's static type carries an unsigned 64-bit integer. `None` for every other call,
        /// which is nearly all of them.
        ///
        /// The ordering twin of [`Rvalue::Render`], emitted for the same reason: a fixed-width
        /// integer is erased to its i64 word, so a `u64` past bit 63 is a negative word and would
        /// order below every small value. Both backends order under the identical hint, so the
        /// differential pins them equal. A set's canonical buffer and a map's key placement are
        /// **identity** orders and never see it — see [`noeta_ast::render_hint`].
        order: Option<std::rc::Rc<noeta_ast::RenderHint>>,
        span: Span,
    },
    /// A **trait** method call with a baked-in route: `receiver.name(args)` where the checker
    /// statically resolved the call to a native trait's shared ctx dispatch. Two producers: (a) a
    /// native trait's *defaulted* method with a trait-level dispatch and no overriding implementor
    /// ([`noeta_ext_abi::ExtTrait::dispatch`], slice 2); (b) — since the ExtBundle→ExtTrait fold-in
    /// (slice 4) — every kernel-trait method (`impl vec.Kernels for T {}`), whose bundle runtime route
    /// was unified onto this one. Dispatch goes straight to the registered trait's shared ctx dispatch
    /// with the receiver as slot 0, no runtime discovery (which is what makes an empty list receiver
    /// work for the bulk kernels). `trait_name` is the trait's qualified identity (`"std.vec.Kernels"`).
    TraitMethod {
        receiver: Atom,
        trait_name: String,
        name: String,
        args: Vec<Atom>,
        span: Span,
    },
    /// Bare member access: `receiver.name`. Resolves to a field load, an enum-variant
    /// constructor reference, or an associated-function reference, exactly as the
    /// tree-walker's `Member` evaluation does.
    Field {
        receiver: Atom,
        name: String,
        name_span: Span,
        span: Span,
    },
    /// An unbound method handle: `Type.method` used as a value (the checker resolved the receiver to
    /// a type and the member to a method/associated fn). Produces a callable value that dispatches by
    /// name — on its first argument (instance) or as an associated call `ty.method(args)`
    /// (`associated`). The receiver type name is a static string, so no receiver atom is lowered.
    MethodHandle {
        ty: String,
        method: String,
        associated: bool,
        span: Span,
    },
    /// A **bound** method handle: `value.method` used as a value (prelude-redesign EX.2b). The
    /// receiver is evaluated and captured into the handle (one owned reference); calling the handle
    /// dispatches `method` on it.
    BoundHandle {
        recv: Atom,
        method: String,
        span: Span,
    },
    /// In-place field assignment: `receiver.name = value` (Phase 5.2). Evaluates to the updated
    /// object. Both backends set the field **in place when the object is uniquely owned** (the
    /// reuse pass's `reuse` token, gated on the runtime refcount `== 1`) and **copy-first when
    /// shared**, so value semantics hold for any aliased observer — like the collection/struct
    /// self-update reuse, it is observationally invisible, so the backends agree even reusing at
    /// different points. `reuse` is set by the reuse pass on the `x.f = v` self-update shape
    /// (`let %t = SetField(Var(x), …); Bind x = %t`); lowering always emits `false`.
    SetField {
        receiver: Atom,
        name: String,
        name_span: Span,
        value: Atom,
        reuse: bool,
        span: Span,
    },
    /// Index access: `receiver[index]`.
    Index {
        receiver: Atom,
        index: Atom,
        span: Span,
    },
    /// Fused indexed field read `list[index].field` (P-PACK 2.5+). Emitted by lowering only when the
    /// checker proves `receiver` is a built-in `List` (its span recorded in `index_field_sites`), so
    /// the backends can read a single packed-element field **without materializing the whole element**
    /// — the scalar-access win the flat `List<packed>` layout otherwise leaves on the table. A packed
    /// list decodes the one field's word(s) directly; a boxed (demoted) list does the ordinary
    /// index-then-load, so the result is identical to the unfused [`Rvalue::Index`] + [`Rvalue::Field`]
    /// it replaces — the layout stays invisible to `RunResult`.
    IndexField {
        receiver: Atom,
        index: Atom,
        field: String,
        field_span: Span,
        span: Span,
    },
    /// A list literal `[a, b, c]`. `reflect` is the checker-resolved element type (runtime
    /// type-argument reflection, R1), baked from the construction-site map so `type_of` recovers the
    /// list's element type even after the value is laundered through `dyn`; `None` when the checker
    /// carried no type (the boxed/REPL path, or a genuinely unknowable element type) — the value stays
    /// untagged and reflects head-only. Invisible to value semantics — the backends stash it beside the
    /// value, never inside it.
    List {
        items: Vec<Atom>,
        reflect: Option<noeta_ast::reflect::TypeRepr>,
        span: Span,
    },
    /// Allocate an empty flat `List<packed>` buffer for a `List<@packed struct>` literal, filled
    /// element-by-element by [`Rvalue::PackedListPush`] (P-PACK 2.5 streaming construction). The
    /// `layout` is carried inline so the backends need no side channel — both build their packed
    /// schema from it identically. Produced only by lowering a list literal whose span the checker
    /// marked `List<packed>`; an unmarked literal stays a boxed [`Rvalue::List`].
    PackedListNew {
        layout: noeta_ast::reflect::PackedLayout,
        span: Span,
    },
    /// Pack one element onto a flat `List<packed>` and yield the (in-place-extended) list. `list` is
    /// the accumulator from [`Rvalue::PackedListNew`] or a prior push — an ANF temp, so it is
    /// uniquely owned and extended in place; `value` is the freshly-built element, **consumed** (its
    /// primitive fields copied into the buffer, then the element object freed). Lowering builds each
    /// element immediately before its push, so only one element object is live at a time — peak
    /// residency is one element + the buffer, not N elements. If `value` cannot be packed (a shape
    /// mismatch — never for a checker-validated `@packed` type) the list demotes to boxed and the
    /// value is pushed as-is, so the flat form is only ever an exact optimization.
    PackedListPush { list: Atom, value: Atom, span: Span },
    /// A tuple literal `(a, b, c)` — a fixed-arity, value-semantic positional aggregate
    /// (object-model slice 4).
    Tuple { items: Vec<Atom>, span: Span },
    /// Tuple projection `receiver.N` — positional access by a constant index.
    TupleIndex {
        receiver: Atom,
        index: u32,
        span: Span,
    },
    /// A map literal `{k: v, ...}`. `reflect` is the checker-resolved `Map(K, V)` type (runtime
    /// type-argument reflection, R1), baked from the construction-site map so `type_of` recovers the
    /// map's key/value types after a `dyn` launder; `None` on the boxed/REPL path. Invisible to value
    /// semantics — carried beside the value, exactly like [`Rvalue::List`]'s tag.
    Map {
        entries: Vec<(Atom, Atom)>,
        reflect: Option<noeta_ast::reflect::TypeRepr>,
        span: Span,
    },
    /// An integer range `start..end` / `start..=end`, eagerly materialized to a list.
    Range {
        start: Atom,
        end: Atom,
        inclusive: bool,
        span: Span,
    },
    /// An all-fields object literal `Type { field: atom, ...spread }`. Field name spans and the
    /// spread's span are retained so construction diagnostics point exactly where the
    /// tree-walker's do.
    ///
    /// `reuse` is the **in-place-reuse token** the reuse-analysis pass ([`noeta_ir_passes`], Phase 5):
    /// `true` marks a *self-update* (`acc = Type { ...acc, f: v }`) where the `spread` base is the
    /// very binding the result is reassigned to, so the base is dead after this construction and its
    /// allocation may be reused. Both backends consume the same token — the VM via an in-place
    /// `MakeStructInPlace`, the IR interpreter via a move-out + `Rc::get_mut` — each gated on the
    /// runtime refcount (`== 1`) so an aliased base falls back to a copy. Reuse is invisible to
    /// observable behavior (value semantics; destructor timing of a replaced field is pinned by the
    /// spec), so the differential stays in agreement. Lowering always emits `false`; the pass sets it.
    Object {
        type_name: String,
        type_name_span: Span,
        fields: Vec<ObjectFieldInit>,
        spread: Option<(Atom, Span)>,
        reuse: bool,
        /// The checker-resolved reflected type (runtime type-argument reflection, R2) — `Some` only
        /// for a **generic** instantiation (`Box<int>` → `Struct("Box", [Int])`), so `type_of`
        /// recovers the type arguments after a `dyn` launder; `None` for a non-generic type (whose
        /// head-only shape name already recovers it) and on the boxed/REPL path. Invisible to value
        /// semantics — carried beside the value like the collection tags.
        reflect: Option<noeta_ast::reflect::TypeRepr>,
        span: Span,
    },
    /// An interpolated string with its holes reduced to atoms.
    Interp { parts: Vec<InterpPart>, span: Span },
    /// A closure construction: capture the current lexical scope around `func`'s template.
    Closure { func: Rc<Func>, span: Span },
    /// The `?` propagation operator: yield the success payload, or early-return the
    /// `Err`/`none` from the enclosing function. `on_error` is the statically-computed list of
    /// owned frame locals (name + destructor-relevance, innermost-scope-first reverse-construction
    /// order) to drop on the **error** path before propagating — the drop pass fills it so a `?`
    /// early-return reclaims abandoned values exactly as an explicit `return` does (Phase 4.2c).
    /// The propagated operand is excluded (it is moved out).
    Try {
        operand: Atom,
        on_error: Vec<TryDrop>,
        span: Span,
    },
    /// `expr.as<T>()` — narrow to `?T` (head-constructor match).
    As {
        operand: Atom,
        ty: TypeRef,
        /// The target's head name as a **run-time string**, when `T` is a type parameter of an
        /// enclosing generic (`v.as<T>()` inside `class Repo<T>` or `fn load<T>`): the atom is the
        /// [`Rvalue::TypeArgName`]/[`Rvalue::TypeSlotName`] lowering already emits for
        /// `type_name::<T>()`, so the narrow matches on exactly the name that surface answers with.
        ///
        /// `None` for every statically-written target, which keeps its baked `ty`. When `Some`, the
        /// string replaces `ty`'s head — the checker only records the site for a *bare* parameter
        /// target (a composite is E0058), so there is never a second name to place.
        dynamic: Option<Atom>,
        span: Span,
    },
    /// `expr is T` — a `bool` head-constructor type test.
    TypeTest {
        operand: Atom,
        ty: TypeRef,
        /// The run-time head name, exactly as [`Rvalue::As`] carries it — the two share the matcher,
        /// so they share this channel.
        dynamic: Option<Atom>,
        span: Span,
    },
    /// Wrap a step closure into a generator iterator (`IterState::Gen`) — the tail of a lowered
    /// generator function (Track G.1b). `step` is the lowered state-machine closure (its `mut`-captured
    /// cells hold the `$state` discriminant and the hoisted locals); the result is an ordinary
    /// `Iterator` that drives `step` once per element. Produced only by the generator desugar
    /// (`lower_generator`), never by surface syntax.
    MakeGen { step: Atom, span: Span },
    /// Wrap a step closure into a `Future` — the tail of a lowered `async fn` (Track A.3). `thunk` is
    /// the async state-machine step closure (its `mut`-captured cells hold `$state` + the hoisted
    /// locals + the awaited-future cells); polling it runs one segment and returns the completion value
    /// or the pending sentinel. Produced only by the async desugar (`lower_async`), never by surface
    /// syntax. (Field still named `thunk` — the wrapper is unchanged since A.1; only the closure's
    /// calling convention became a poll.)
    MakeFuture { thunk: Atom, span: Span },
    /// `expr.await` at the async **top level** — drive a `Future` to completion via the executor and
    /// yield its value (Track A.2/A.3): poll; on pending, advance the logical clock and re-poll. Inside
    /// an `async fn` body the `.await` is instead compiled into a poll-suspend state of the state
    /// machine (see [`Self::PollFuture`]); this rvalue is only the root driver.
    RunFuture { future: Atom, span: Span },
    /// Poll a `Future` once (Track A.3): returns `some(v)` if it is ready with `v`, or `none` if it is
    /// pending. The single-step primitive the async state machine uses at each `.await` — produced only
    /// by the async desugar (the synthetic `$poll(f)` call), never by surface syntax.
    PollFuture { future: Atom, span: Span },
    /// The async **pending** sentinel (Track A.3) — the value a state-machine step returns to signal it
    /// suspended at an `.await`. Produced only by the async desugar (the synthetic `$pending`), never by
    /// surface syntax; always caught at a poll site, never bound to a user value.
    Pending { span: Span },
    /// `spawn e` (Track A.3b): register the future `future` as a task in the current concurrency scope
    /// and yield a handle (itself a `Future<T>`). Legal only inside a `ScopeBegin`/`ScopeEnd` region.
    Spawn { future: Atom, span: Span },
    /// Open a structured-concurrency scope and yield its **index** (Track A.7): the value form of
    /// `Stmt::ScopeBegin`, emitted only by the async desugar (the synthetic `$scope_begin()`) when a
    /// `concurrent { }` block inside an async fn is split into state-machine states. The returned int is
    /// threaded to the block's join poll-state ([`Self::ScopeReady`]) so the join checks *this* scope's
    /// completion rather than whatever scope is innermost at re-poll time (a nested `concurrent` in a
    /// sub-task may have pushed a deeper scope above it). The statement-position `Stmt::ScopeBegin` (used
    /// by the synchronous, non-flattened `concurrent` lowering) is unchanged.
    ScopeBegin { span: Span },
    /// Whether every task in the scope at index `scope` has completed or been cancelled (Track A.7) —
    /// the boolean the async desugar's `concurrent` join poll-state tests each poll. Emitted only by the
    /// async desugar (the synthetic `$scope_ready(idx)`); when false the step suspends (`$pending`) so
    /// the scheduler round-robins the inner scope's tasks with the outer scope's siblings across polls,
    /// instead of driving the inner scope to completion inside one poll of the outer task.
    ScopeReady { scope: Atom, span: Span },
    /// Close the (already-drained) scope at index `scope` (Track A.7): release its tasks' futures and
    /// results and tombstone the slot. The value form of `Stmt::ScopeEnd` that closes a **specific**
    /// scope by index rather than the innermost — necessary because a split `concurrent { }` in one task
    /// may finish while a *sibling* task's own `concurrent` scope is still open above it on the stack, so
    /// the two close out of structured-stack order. Emitted only by the async desugar (the synthetic
    /// `$scope_end(idx)`); the join happened already at the [`Self::ScopeReady`] poll-state, so no drive.
    ScopeEndAt { scope: Atom, span: Span },
    /// `isolate f(args)` (isolates I.4b): spawn the call as a concurrent unit in a **fresh isolate**.
    /// Unlike [`Self::Spawn`], the callee and arguments are carried **unbuilt** — an isolate can run on
    /// a real OS thread (out-of-oracle), where a pre-built future (which captures its args in the parent
    /// heap) could not cross the boundary; the arguments are copy-marshalled and the callee is
    /// reconstructed on the worker from its prototype. In the deterministic sandbox this is
    /// observationally identical to `spawn f(args)`: the backend calls `callee(args)` to build the
    /// future and registers it as a cooperative task, so the differential holds. Legal only inside a
    /// `ScopeBegin`/`ScopeEnd` region (orphan `isolate` is E0041).
    SpawnIsolate {
        callee: Atom,
        args: Vec<Atom>,
        span: Span,
    },
    /// `type_name::<T>()` where `T` is a **type parameter of the enclosing generic type**, inside
    /// one of its instance methods (generic constructor reflection, Gap B): the qualified name of
    /// type argument `index` of `operand`'s reflected type tag, as a `string`.
    ///
    /// One compiled body serves every instantiation, so there is no constant to fold; the
    /// instantiation travels on the receiver instead, in the very tag `type_of` already reads.
    /// `operand` is always `self` as lowering emits it. A value whose tag does not carry that
    /// argument — an instance built where the instantiation was genuinely unknown — **aborts** with
    /// a message naming the type and the parameter, rather than answering `"dyn"`: a wrong name
    /// would flow silently into whatever keyed on it.
    TypeArgName {
        operand: Atom,
        index: u32,
        /// The enclosing type and parameter names, for the abort message only.
        type_name: String,
        param: String,
        span: Span,
    },
    /// `type_name::<T>()` where `T` is a **forwarded type parameter of the enclosing top-level
    /// generic fn** (poly-values F2b): the instantiation's qualified name, read out of
    /// [`Program::type_args`] at the index the hidden slot holds.
    ///
    /// The fn-side twin of [`Rvalue::TypeArgName`] — same answer, different channel. A generic
    /// *type* carries its instantiation on the receiver; a generic *fn* has no receiver, so it
    /// carries it in the hidden argument that already delivers `json.try_parse::<T>`'s decode
    /// recipe. This surface reads only the entry's NAME, which is why it forwards even for an
    /// instantiation that has no recipe at all.
    ///
    /// `slot` is the hidden `$ty<i>` local as lowering emits it. An index the table does not hold
    /// cannot arise from a checked program (the checker resolves every slot at the instantiating
    /// call); the backends treat it as the corrupt-slot abort the recipe path already does.
    TypeSlotName { slot: Atom, span: Span },
    /// **The reflection surface** — twelve of the thirteen intrinsics as one rvalue: `which` says
    /// which query, `args` its already-lowered operands.
    ///
    /// The IR twin of [`noeta_ast::Expr::Reflect`], and collapsed for the same reason. Eight of the
    /// twelve carry exactly one [`Atom`], and before the collapse those eight were eight variants
    /// that every operand walk — free variables, liveness, printing — had to list by name. They are
    /// one arm now, and the shapes that genuinely differ are named in [`ReflectArgs`].
    ///
    /// [`ReflectKind::TypeName`](noeta_ast::ReflectKind::TypeName) is the one kind that never
    /// appears here, and it is not an omission: `type_name::<T>()` is either a compile-time constant
    /// (the program is linked by now, so the `TypeRef` already carries its qualified identity) or a
    /// read of whichever per-instantiation channel carries `T` — [`Rvalue::TypeArgName`] or
    /// [`Rvalue::TypeSlotName`]. It has no runtime query of its own to name. The census records
    /// that.
    Reflect {
        which: noeta_ast::ReflectKind,
        args: ReflectArgs,
        /// `fields_of` only: whether this site may report the operand's **private** fields.
        ///
        /// The value-level door hands back values, so it answers the same visibility question a
        /// written `x.secret` does — and only the checker can, since a runtime knows neither its
        /// caller's type nor its package. `false` (the common case, and every other
        /// [`noeta_ast::ReflectKind`]) means the door reports what the caller could have read
        /// itself. Baked here rather than looked up per backend so the two cannot disagree.
        private_fields: bool,
        span: Span,
    },
    /// `channel::<T>(capacity)` — construct a bounded channel (isolates I.1), yielding a
    /// `(Sender, Receiver)` tuple of scheduler-owned endpoint ids. The message type `T` is a
    /// checker-only concern (the runtime channel is untyped), so only `capacity` reaches here.
    MakeChannel { capacity: Atom, span: Span },
    /// A **call-site-typed** native module call — the turbofish form (`json.parse::<T>(args)`)
    /// only; an ordinary module call (`http.get(url)`) lowers as `CallMethod` on a first-class
    /// module value. `recipe` is the turbofish `T` resolved by the checker (baked here from
    /// `typed_module_call_sites`); `None` means `T` had no decoding (already a checker error),
    /// letting the backend fail cleanly. The backend marshals `args`, runs the shared native
    /// function, and materializes the result tree into a value of `T`.
    TypedModuleCall {
        module: String,
        func: String,
        args: Vec<Atom>,
        recipe: Option<noeta_ext_abi::TypeRecipe>,
        /// A **per-instantiation** recipe source (poly-values F2b): the enclosing forwarding fn's
        /// hidden slot holding the instantiation's index into [`Program::type_args`]. `Some` iff
        /// the turbofish was a forwarded type parameter; then `recipe` is `None` and the backend
        /// resolves the table entry's recipe at runtime.
        dynamic: Option<Atom>,
        span: Span,
    },
    /// A **call-site-typed** native extern-METHOD call (http arc H8) — `resp.json::<T>()`, the
    /// [`Rvalue::TypedModuleCall`] twin. The receiver's own runtime identity selects the type (as
    /// every extern method call does), so no type name is carried; `method` names the entry in
    /// that type's `typed_methods` table.
    ///
    /// Emitted only where the checker recorded a recipe (`typed_method_call_sites`); every other
    /// turbofish method call is an erased user-generic instantiation and lowers as a plain
    /// method call.
    TypedMethodCall {
        recv: Atom,
        method: String,
        args: Vec<Atom>,
        recipe: Option<noeta_ext_abi::TypeRecipe>,
        /// A per-instantiation recipe source — the [`Rvalue::TypedModuleCall`] `dynamic` twin.
        dynamic: Option<Atom>,
        span: Span,
    },
    /// The **router-facing** runtime JSON decode `json.decode_typed(name, text)` (L2.2 DI): decode
    /// `text` into the type named by the runtime string `name`, using the recipe a
    /// `@derive(Deserialize<Json>)` type registered (baked into the backend's per-type recipe
    /// registry). Recoverable end to end — a malformed body **or** an unknown type name yields
    /// `Result.Err(message)`, a successful decode `Result.Ok(value)`.
    DecodeTyped { name: Atom, text: Atom, span: Span },
    /// A **native module function as a first-class value** (expr-tiers arc) — the same value a
    /// `use std.math.sqrt` binding holds (`Const::ModuleFn`), but produced from a compiler
    /// [`noeta_ast::Expr::NativeFnRef`] rather than a user import. The expression-tier desugar
    /// emits this as a native handler's call callee, so the handler call lowers through the
    /// ordinary `Call` path with a function value, exactly like a Noeta handler. The backend loads
    /// the module-function const; a `Call` on the result dispatches to the native function.
    ModuleFn {
        module: String,
        func: String,
        span: Span,
    },
    /// A **native module as a first-class value** — the same value a `use std.http.client` binding
    /// holds (`Const::NativeModule`), but materialized at a `http.client` member-access site that a
    /// namespace group (`use std.http`) resolved. `module` is the **root-qualified** leaf identity
    /// (`std.http.client`, never the group prefix `std.http`), so it flows to the const pool exactly
    /// as a direct import would — keeping AOT ring DCE (which keys on the concrete identity) intact.
    /// A method call on the result dispatches through the ordinary native-module path.
    NativeModule { module: String, span: Span },
}

/// **The lowered operands of a reflection query** — the IR twin of
/// [`ReflectShape`](noeta_ast::ReflectShape), and the reason [`Rvalue::Reflect`] can be one variant.
///
/// Four shapes for twelve kinds, against the AST's seven for thirteen: lowering has already
/// resolved the static/dynamic distinction that the surface has to keep (a turbofish type and a
/// runtime string both arrive here as *a name atom*), which is exactly what the two
/// per-instantiation channels made possible. What survives are the genuine differences in operand
/// count and payload.
#[derive(Debug, Clone)]
pub enum ReflectArgs {
    /// No operand — the unscoped `roles_of()`.
    Nothing,
    /// One operand. Eight of the twelve: a value (`type_of`, `fields_of`, `traits_of`), a runtime
    /// string naming a callable (`params_of`, `returns_of`), or a **type name** — which is what
    /// `attributes_of`, `field_specs_of`, `variants_of` and the scoped `roles_of` all reduce to,
    /// whichever surface they were written in and whichever channel carried a type parameter.
    One(Atom),
    /// A type name plus one runtime argument — `construct(name, fields)`.
    Two { name: Atom, arg: Atom },
    /// `invoke(recv, name, args)` / `invoke(name, args)`. `recv` is `None` for the free-function
    /// form, where `name` resolves in the top-level function namespace instead of a type's method
    /// table — the distinction every reader must make, so it stays an `Option` rather than a
    /// sentinel.
    Dispatch {
        recv: Option<Atom>,
        name: Atom,
        args: Atom,
    },
    /// `from_bytes::<T>(blob)`: the byte operand plus element `T`'s packed layout, looked up by
    /// lowering in the `packed_list_sites` channel — the same one list literals use. `layout` is
    /// `None` if the checker recorded none (`T` not packable — already an error), letting the
    /// backend fail cleanly.
    ///
    /// The layout, not a name, is why `from_bytes` is the one query a per-instantiation channel
    /// cannot answer: both channels carry names.
    Bytes {
        blob: Atom,
        layout: Option<noeta_ast::reflect::PackedLayout>,
        /// Whether element type `T` implements `Validate` (validation arc): when set, the backend
        /// runs `validate()` on each decoded element and aborts at `[i]` on the first rejection —
        /// the abort door, consistent with a shape mismatch.
        validate: bool,
    },
}

impl ReflectArgs {
    /// Visit every operand [`Atom`], in evaluation order — the one walk free-variable collection
    /// and liveness both need, and which they each used to spell as twelve arms.
    pub fn for_each_atom(&self, f: &mut impl FnMut(&Atom)) {
        match self {
            ReflectArgs::Nothing => {}
            ReflectArgs::One(a) | ReflectArgs::Bytes { blob: a, .. } => f(a),
            ReflectArgs::Two { name, arg } => {
                f(name);
                f(arg);
            }
            ReflectArgs::Dispatch { recv, name, args } => {
                if let Some(recv) = recv {
                    f(recv);
                }
                f(name);
                f(args);
            }
        }
    }
}

/// A statement in an IR [`Block`]. The `let`/`eval`/`bind`/`echo`/`return` forms are
/// straight-line; the rest are structured control flow whose sub-[`Block`]s the interpreter
/// walks recursively.
#[derive(Debug, Clone)]
pub enum Stmt {
    /// Bind a primitive operation's result to a frame temporary: `let dst = rvalue`.
    Let {
        dst: Temp,
        rvalue: Rvalue,
        span: Span,
    },
    /// Evaluate an operation for its effect and discard the value (a bare expression
    /// statement).
    Eval { rvalue: Rvalue, span: Span },
    /// A source binding or reassignment: `name = atom` / `mut name = atom`. Whether this
    /// declares a new immutable binding or reassigns an existing one is a runtime decision
    /// (the interpreter's `bind`), exactly as in the AST walker.
    Bind {
        mut_decl: bool,
        name: String,
        name_span: Span,
        value: Atom,
        /// True when this bind is the reassignment wrapping a field-set `x.f = v` (object-model
        /// slice 2b′). The backends **skip the immutable-reassignment check (E0006)** for it: a
        /// reference `class` field-set mutates in place (the rebind just restores `x`), and a value
        /// `struct` field-set on an immutable `x` is already rejected *statically* by the checker.
        field_assign: bool,
        span: Span,
    },
    /// `echo atom;` — display the atom and append a newline to stdout.
    Echo { value: Atom, span: Span },
    /// `return atom;` / `return;`.
    Return { value: Option<Atom>, span: Span },
    /// `break;`.
    Break { span: Span },
    /// `continue;`.
    Continue { span: Span },
    /// Open a structured-concurrency scope (Track A.3b) — the start of a lowered `concurrent { }`.
    /// Subsequent `Rvalue::Spawn`s register tasks in this scope; [`Self::ScopeEnd`] joins them.
    ScopeBegin { span: Span },
    /// Close the current concurrency scope: drive every task spawned in it to completion (the join),
    /// then pop the scope. The end of a lowered `concurrent { }`.
    ScopeEnd { span: Span },
    /// A statement `if cond { then } else { else_ }`. The condition is a pre-computed bool
    /// atom; each arm is a statement-context block. (The `if … then … else` *expression* is
    /// desugared to a `match` in the parser, so it arrives as [`Stmt::Match`], not here.)
    If {
        cond: Atom,
        then_block: Block,
        else_block: Option<Block>,
        span: Span,
    },
    /// `while cond { body }`. The condition is re-evaluated each iteration, so it is a
    /// **block** (computing into its tail atom) rather than a pre-computed atom; both the
    /// condition block and the body run in the enclosing activation's temporary frame.
    While {
        cond: Block,
        body: Block,
        span: Span,
    },
    /// `for pattern in iterable { body }`. The iterable is pre-computed to an atom; the
    /// loop pattern reuses the surface [`ForPattern`]. `stream` is set (from the checker's
    /// `for_stream_sites`) when the iterable is statically an `Iterator<T>` (Track I.2): the loop
    /// then drives `next()` one element at a time instead of snapshotting a list, so a lazy source
    /// streams (and `break` stops early). A collection iterable keeps the snapshot/cursor fast path.
    For {
        pattern: ForPattern,
        iterable: Atom,
        body: Block,
        span: Span,
        stream: bool,
        /// The **ordering hint** for a loop over a `Set`/`Map` whose element or key type carries an
        /// unsigned 64-bit integer: the snapshot the loop walks is sorted under it, so the program
        /// sees the order its type says. `None` for a list iterable (whose order is its data) and
        /// for every collection with no `u64` in it. See [`Rvalue::Method::order`].
        order: Option<std::rc::Rc<noeta_ast::RenderHint>>,
    },
    /// `match scrutinee { arms }`. When the match is used as an expression its value is
    /// written to `dst`; in statement position `dst` is `None`. Patterns reuse the surface
    /// [`Pattern`] (matched by the interpreter's shared matcher).
    Match {
        scrutinee: Atom,
        arms: Vec<Arm>,
        dst: Option<Temp>,
        span: Span,
    },
    /// `dst = left && right` (or `||`). The `left` operand is pre-computed to an atom; the
    /// `right` operand `Block` (tail `Some`) is evaluated **only** when `left` does not
    /// short-circuit. Modelled as a statement rather than an [`Rvalue`] precisely because
    /// the right operand must not be evaluated up-front; the bool-operand checks match the
    /// tree-walker's `&&`/`||` exactly. `dst` is `None` in discard position.
    Logical {
        dst: Option<Temp>,
        op: BinaryOp,
        left: Atom,
        right: Block,
        span: Span,
    },
    /// `dst = value ?? fallback`. `value` is pre-computed; the `fallback` block (tail
    /// `Some`) runs **only** when `value` is `Err`/`none`. A statement for the same
    /// laziness reason as [`Stmt::Logical`].
    Coalesce {
        dst: Option<Temp>,
        value: Atom,
        fallback: Block,
        span: Span,
    },
    /// A declaration. `fn`/`class` carry lowered IR bodies; the rest carry the surface
    /// declaration verbatim (they have no executable body, so the interpreter reuses the
    /// tree-walker's registration unchanged).
    Decl(Decl),
    /// Release a temporary's value now (drop its reference), rather than at activation end.
    ///
    /// ANF temporaries are **single-use**: every `let`-bound temp is read by exactly one
    /// consumer, so the interpreter *moves* a temp's value out of its slot when read — its
    /// reference then lives no longer than the corresponding intermediate in the tree-walker.
    /// The one exception is the **discarded result of a bare expression statement**, which no
    /// consumer reads; lowering emits a `Drop` for it so its reference is released at the end
    /// of the statement (matching the tree-walker, where that intermediate drops there). This
    /// is a faithfulness device — destructors are reference-count-gated, so a lingering temp
    /// reference would suppress a destructor the tree-walker fires — not a reference-counting
    /// optimization (those land in a later phase).
    Drop(Temp),
    /// Release a **source variable's** value now, at its last use, rather than at scope/teardown
    /// (memory-management migration, Phase 3). Inserted by the drop-insertion pass
    /// ([`noeta_ir_passes`]) at a binding's death point — only for function-local bindings, never a
    /// top-level global (those stay teardown-reclaimed) and never an immediately-reassigned binding
    /// (its displaced value is released by the reassignment itself). `name` resolves through the
    /// runtime scope/register model exactly as a read would.
    ///
    /// In Phase 3 this lowers to a **plain reference release** in both backends (prompt memory
    /// reclamation, the peak-residency win) and fires **no** destructor — destructor firing stays
    /// globals-only until Phase 4, which flips local drops to the destructor-running release.
    ///
    /// `relevant` is the **destructor-relevance** annotation (Phase 3.2b): `true` if dropping this
    /// binding's value could run *some* `destruct` block (its type transitively reaches one),
    /// `false` if it provably cannot. Both backends ignore it in Phase 3 (every drop is a plain
    /// release); Phase 4 reads it to skip the destructor-firing check for a `false` drop. Computed
    /// from the checker's per-binding types; conservatively `true` when the type is unknown.
    DropVar {
        name: String,
        span: Span,
        relevant: bool,
    },
}

/// One arm of an IR `match`: a surface pattern and the block it runs. In expression
/// position the block's `tail` is the arm's value (written to the match's `dst`).
#[derive(Debug, Clone)]
pub struct Arm {
    pub pattern: Pattern,
    /// The arm's guard (`pattern if cond => …`), if any. Evaluated **only after** the pattern
    /// structurally matches, with the pattern's bindings bound; a `false` guard falls through to
    /// the next arm exactly as a failed pattern would. Its block's `tail` is the guard's bool.
    pub guard: Option<Guard>,
    pub body: Block,
    pub span: Span,
}

/// A match-arm guard: the lazily-evaluated value block computing the guard's bool (tail `Some`),
/// plus the guard expression's source span — a [`Block`] carries no span of its own, and the
/// runtime non-bool error (both backends, identical) is reported at the guard.
#[derive(Debug, Clone)]
pub struct Guard {
    pub block: Block,
    pub span: Span,
}

/// A sequence of statements, optionally yielding a value. `tail` is `Some` when the block
/// is in **expression** position — an arrow function body, a `match` arm, a `while`
/// condition, or a defaulted-parameter thunk — and `None` in statement position. A block
/// runs in its own child scope (mirroring the tree-walker's `exec_block`), but shares the
/// enclosing activation's temporary frame.
#[derive(Debug, Clone)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub tail: Option<Atom>,
}

impl Block {
    /// A statement-position block (no value).
    pub fn stmts(stmts: Vec<Stmt>) -> Block {
        Block { stmts, tail: None }
    }
}

impl Stmt {
    /// Visit each **control-flow child block** of this statement: the two arms of `if`, the
    /// `while` condition and body, the `for` body, each `match` arm's guard and body, and the lazy
    /// `right`/`fallback` operand of `&&`/`||`/`??`. Leaf statements have none.
    ///
    /// Deliberately does **not** descend into a `Decl` (a nested `fn`/`class` is a *separate*
    /// scope that the reference-counting passes treat case-by-case). This is the single shared
    /// definition of "which blocks does a statement nest," so a pure structural collector can
    /// recurse control flow without re-listing every variant — and a new control-flow variant is
    /// picked up here in one place. Order- or context-sensitive walkers (backward liveness,
    /// drop-insertion rewrite) intentionally match the variants themselves instead, so adding a
    /// variant still fails their exhaustive `match` and forces a deliberate update.
    pub fn for_each_child_block(&self, mut f: impl FnMut(&Block)) {
        match self {
            Stmt::If {
                then_block,
                else_block,
                ..
            } => {
                f(then_block);
                if let Some(else_block) = else_block {
                    f(else_block);
                }
            }
            Stmt::While { cond, body, .. } => {
                f(cond);
                f(body);
            }
            Stmt::For { body, .. } => f(body),
            Stmt::Match { arms, .. } => {
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        f(&guard.block);
                    }
                    f(&arm.body);
                }
            }
            Stmt::Logical { right, .. } => f(right),
            Stmt::Coalesce { fallback, .. } => f(fallback),
            Stmt::Let { .. }
            | Stmt::Eval { .. }
            | Stmt::Bind { .. }
            | Stmt::Echo { .. }
            | Stmt::Return { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. }
            | Stmt::ScopeBegin { .. }
            | Stmt::ScopeEnd { .. }
            | Stmt::Decl(_)
            | Stmt::Drop(_)
            | Stmt::DropVar { .. } => {}
        }
    }
}

/// The lowered template of a function, method, associated function, or closure. At runtime
/// the interpreter pairs this with a captured scope to form a callable value; `params` and
/// `defaults` mirror the surface signature, `body` is the lowered body (its `tail` is the
/// value for an arrow body; a block body returns via [`Stmt::Return`] or unit), and
/// `temp_count` sizes the activation's temporary frame.
#[derive(Debug, Clone)]
pub struct Func {
    /// The declared name this callable traces under (`"f"`, `"Type.method"`, `"Type::destruct"`),
    /// set at lowering — `None` for a user's anonymous closure. A synthesized async/generator step
    /// closure inherits its **enclosing** function's name, so an `async fn work` panic traces as
    /// `work`, not `<anonymous>`. Both backends read this one field, so trace names agree by
    /// construction.
    pub name: Option<String>,
    /// The **seal**: `Some(allow)` for a named fn/method — its body's bare assignments may only
    /// reach surrounding bindings named in `allow` (the surface `use (…)` capture clause); any
    /// other bare assignment declares a fresh local, exactly as the checker typed it. `None` for
    /// an anonymous closure and every synthesized step closure (auto-capturing, unchanged).
    /// Reads are not gated here — the checker already rejected unlisted reads, and free-variable
    /// capture / global loads for statics and allow-listed names behave as before.
    pub captures: Option<Vec<String>>,
    pub params: Vec<String>,
    /// How many of the leading [`Self::params`] are **type-argument slots** rather than value
    /// parameters (poly-values F2b): a forwarding generic's `$ty0`, `$ty1`, … .
    ///
    /// The slots are still parameters — the body names them and register allocation places them
    /// like any other — but they are supplied through their **own channel**, the call node's
    /// `type_args`, never through the value-argument list. That separation is what this count
    /// expresses: every binder lays type arguments into this many leading slots and value
    /// arguments after them, so arity, defaults and the `supplied` mask are all reckoned over the
    /// value parameters alone. Before it, a forwarding call smuggled its slots in as *prepended
    /// arguments*, which shifted every parameter position and forced the argument-binding map to
    /// be thrown away at any forwarding call site.
    ///
    /// A call supplying a different count than the callee declares cannot arise from a checked
    /// program; a backend that meets one aborts rather than misbinding, because the alternative is
    /// silently reading a value argument as a type-table index.
    pub hidden: u32,
    /// Each parameter's default thunk, parallel to `params` (`None` for a required
    /// parameter). A default is evaluated in the *captured* scope when its argument is
    /// omitted, so it carries its own temporary frame.
    pub defaults: Vec<Option<Thunk>>,
    pub body: Block,
    /// The number of distinct [`Temp`]s used in `body` (the activation frame size). Default
    /// thunks size their own frames; their temps are not counted here.
    pub temp_count: u32,
    pub span: Span,
}

/// A self-contained value-producing block with its own temporary frame — used for a
/// defaulted parameter, which is evaluated independently in the closure's captured scope.
/// `body.tail` is always `Some` (a default always produces a value).
#[derive(Debug, Clone)]
pub struct Thunk {
    pub body: Block,
    pub temp_count: u32,
}

/// A declaration node. `fn`/`class` carry lowered IR; `enum`/`struct`/`use` carry the
/// surface declaration unchanged (no executable body to lower).
#[derive(Debug, Clone)]
pub enum Decl {
    /// `fn name(params) { body }`.
    Fn {
        name: String,
        func: Rc<Func>,
        span: Span,
    },
    /// `class Name { fields; methods; destruct { … } }`. Fields, derives, and the
    /// destructor are read from the surface `decl`; methods are lowered to IR. (The
    /// destructor body stays surface AST and is run by the shared end-of-program
    /// destruction path, unchanged in this phase.)
    Class(ClassDef),
    /// `enum Name { variants; methods; impl Trait { … } }`. Variants/derives are read from the
    /// surface `decl`; methods are lowered to IR (object-model slice 3).
    Enum(EnumDef),
    /// `struct Name { fields; methods }` — the value kind. Like [`Decl::Class`] but with no
    /// `destruct`; fields/derives are read from the surface `decl`, methods are lowered to IR.
    Struct(StructDef),
    /// A `use` import (binds names / native modules; no executable body).
    Use {
        path: Vec<String>,
        names: Vec<noeta_ast::UseName>,
        span: Span,
    },
}

/// A lowered class: the surface declaration (for fields, derives, and name) paired with its
/// methods lowered to IR funcs and, when present, its `destruct` block lowered to a
/// parameterless [`Func`].
#[derive(Debug, Clone)]
pub struct ClassDef {
    pub decl: Rc<noeta_ast::ClassDecl>,
    pub methods: Vec<(String, Rc<Func>)>,
    /// Each field declared with a default (`x: T = expr`), lowered to a parameterless value
    /// [`Thunk`] (object-model slice 5). A construction that omits the field fills it by running
    /// this thunk in the type's **definition scope** (globals — empty upvalues, exactly the
    /// parameter-default protocol), so a default never sees `self` or sibling fields. Keyed by field
    /// name; only defaulted fields appear (a mandatory field is absent). Empty for the common case.
    pub field_defaults: Vec<(String, Thunk)>,
    /// The `destruct { ... }` block lowered to a parameterless [`Func`] (fields in scope via the
    /// receiver), or `None` for a class without one. The VM compiles this to a bytecode prototype
    /// it runs when an instance's last reference drops. The IR *interpreter* ignores it and runs
    /// the surface destructor on `decl` through the shared teardown path, so the Phase-1
    /// faithfulness differential is unaffected.
    pub destructor: Option<Rc<Func>>,
    pub span: Span,
}

/// A lowered struct (the value kind): the surface declaration (for fields, derives, and name)
/// paired with its methods lowered to IR funcs. Mirrors [`ClassDef`] but carries no destructor —
/// a struct is pure data and never has a `destruct` block.
#[derive(Debug, Clone)]
pub struct StructDef {
    pub decl: Rc<noeta_ast::StructDecl>,
    pub methods: Vec<(String, Rc<Func>)>,
    /// Each field declared with a default (`x: T = expr`), lowered to a parameterless value
    /// [`Thunk`] run in the type's definition scope when a literal omits the field (object-model
    /// slice 5). See [`ClassDef::field_defaults`].
    pub field_defaults: Vec<(String, Thunk)>,
    pub span: Span,
}

/// A lowered enum: the surface declaration (for variants, derives, and name) paired with its
/// methods lowered to IR funcs (object-model slice 3). Mirrors [`StructDef`] — an enum has no
/// `destruct` block. An enum method takes the whole enum value as `self` and has no implicit field
/// scope (variants differ), so its body typically `match`es on `self`.
#[derive(Debug, Clone)]
pub struct EnumDef {
    pub decl: Rc<noeta_ast::EnumDecl>,
    pub methods: Vec<(String, Rc<Func>)>,
    pub span: Span,
}
