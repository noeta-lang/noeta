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

use lang_span::Span;

pub use lang_ast::{BinaryOp, ForPattern, Pattern, TypeRef, UnaryOp};

mod lower;
mod pretty;
pub use lower::{Unsupported, lower, lower_with_sites, lower_with_sites_opts};
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
    /// the declared width via `lang_stdlib::mask_to_width(value, signed, bits)`. Both backends apply
    /// the identical helper, so wraparound agrees by construction. Pure, single-operand (like
    /// [`Rvalue::Unary`]); never appears on the boxed/REPL path (only IntN arithmetic produces it).
    MaskWidth {
        operand: Atom,
        signed: bool,
        bits: u8,
        span: Span,
    },
    /// A non-short-circuiting infix operation. `&&`/`||` are lowered to control flow and
    /// never appear here; everything else (arithmetic, concat, comparisons, equality) does.
    /// Operator-trait overloading on user objects is resolved by the interpreter, not the
    /// IR.
    ///
    /// `reuse` is the **in-place-reuse token** for a list **self-append** (`acc = acc ~ rhs`,
    /// the desugaring of `acc ~= rhs`): set by the reuse-analysis pass ([`lang_ir_passes`],
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
    /// A call of a callee value: `callee(args)`.
    Call {
        callee: Atom,
        args: Vec<Atom>,
        span: Span,
    },
    /// A method/associated call: `receiver.name(args)`. Kept distinct from [`Rvalue::Call`]
    /// so the interpreter can route it through method dispatch (built-in, stdlib, and
    /// user-defined) without reconstructing the receiver.
    ///
    /// `reuse` is the **in-place-reuse token** for a collection **method self-update** (`m = m.set(k,v)`
    /// / the `m[k] = v` desugaring): set by the reuse-analysis pass ([`lang_ir_passes`], Phase 5) only
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
    /// A list literal `[a, b, c]`.
    List { items: Vec<Atom>, span: Span },
    /// Allocate an empty flat `List<packed>` buffer for a `List<@packed struct>` literal, filled
    /// element-by-element by [`Rvalue::PackedListPush`] (P-PACK 2.5 streaming construction). The
    /// `layout` is carried inline so the backends need no side channel — both build their packed
    /// schema from it identically. Produced only by lowering a list literal whose span the checker
    /// marked `List<packed>`; an unmarked literal stays a boxed [`Rvalue::List`].
    PackedListNew {
        layout: lang_ast::reflect::PackedLayout,
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
    /// A map literal `{k: v, ...}`.
    Map {
        entries: Vec<(Atom, Atom)>,
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
    /// `reuse` is the **in-place-reuse token** the reuse-analysis pass ([`lang_ir_passes`], Phase 5):
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
        span: Span,
    },
    /// `expr is T` — a `bool` head-constructor type test.
    TypeTest {
        operand: Atom,
        ty: TypeRef,
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
    /// `type_of(value)` — the runtime `Type` descriptor of a value.
    TypeOf { operand: Atom, span: Span },
    /// `from_bytes::<T>(blob)` — deserialize a `bytes` buffer into a flat `List<T>` (P-PACK 4.4).
    /// `blob` is the byte operand; `layout` is element `T`'s packed layout (looked up by the lowering
    /// in the `packed_list_sites` channel — the same one list literals use). `None` if the checker
    /// did not record a layout (T not packable — already an error), letting the backend fail cleanly.
    FromBytes {
        blob: Atom,
        layout: Option<lang_ast::reflect::PackedLayout>,
        span: Span,
    },
    /// `channel::<T>(capacity)` — construct a bounded channel (isolates I.1), yielding a
    /// `(Sender, Receiver)` tuple of scheduler-owned endpoint ids. The message type `T` is a
    /// checker-only concern (the runtime channel is untyped), so only `capacity` reaches here.
    MakeChannel { capacity: Atom, span: Span },
    /// `attributes_of::<T>()` — the manifest's `#[T(...)]` attributes.
    AttributesOf { ty: TypeRef, span: Span },
    /// `roles_of()` — the `(declaration, Role)` index.
    RolesOf { span: Span },
    /// `invoke(recv, name, args)` — fallible by-name dispatch.
    Invoke {
        recv: Atom,
        name: Atom,
        args: Atom,
        span: Span,
    },
    /// A call-site-typed native module call (`json.parse::<T>(args)`). `recipe` is the turbofish `T`
    /// resolved by the checker (baked here from `ext_call_sites`); `None` means `T` had no decoding
    /// (already a checker error), letting the backend fail cleanly. The backend marshals `args`, runs
    /// the shared native function, and materializes the result tree into a value of `T`.
    ExtCall {
        module: String,
        func: String,
        args: Vec<Atom>,
        recipe: Option<lang_stdlib::TypeRecipe>,
        span: Span,
    },
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
    /// ([`lang_ir_passes`]) at a binding's death point — only for function-local bindings, never a
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
    pub body: Block,
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

/// The lowered template of a function, method, associated function, or closure. At runtime
/// the interpreter pairs this with a captured scope to form a callable value; `params` and
/// `defaults` mirror the surface signature, `body` is the lowered body (its `tail` is the
/// value for an arrow body; a block body returns via [`Stmt::Return`] or unit), and
/// `temp_count` sizes the activation's temporary frame.
#[derive(Debug, Clone)]
pub struct Func {
    pub params: Vec<String>,
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
        names: Vec<lang_ast::UseName>,
        span: Span,
    },
}

/// A lowered class: the surface declaration (for fields, derives, and name) paired with its
/// methods lowered to IR funcs and, when present, its `destruct` block lowered to a
/// parameterless [`Func`].
#[derive(Debug, Clone)]
pub struct ClassDef {
    pub decl: Rc<lang_ast::ClassDecl>,
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
    pub decl: Rc<lang_ast::StructDecl>,
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
    pub decl: Rc<lang_ast::EnumDecl>,
    pub methods: Vec<(String, Rc<Func>)>,
    pub span: Span,
}
