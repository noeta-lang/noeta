//! The register bytecode IR: opcodes, the constant pool, and a disassembler.
//!
//! Register-based (Lua/Dalvik style), not stack-based — fewer dispatches and a friendlier
//! base for the later specializing interpreter (architecture §6). This crate is pure data:
//! it knows nothing about runtime values. The compiler (`lang-compiler`) emits a [`Chunk`];
//! the VM (`lang-vm`) interprets one. The disassembler renders a [`Chunk`] to a stable text
//! form for snapshot tests (raw bytes are never asserted).

use std::fmt::Write as _;

use lang_ast::{BinaryOp, UnaryOp};
use lang_diagnostics::Diagnostic;
use lang_object::Shape;
use lang_span::Span;

/// A register index. M1.0's allocator is monotonic (one register per value, no reuse); a
/// reusing allocator is a later optimization the disassembly snapshots will make visible.
pub type Reg = u16;

/// An interned name index into [`Module::names`] (P-VMT-OPSZ). Every instruction-embedded name —
/// field/method names, the ext-call module+func, type names, and `match`-literal strings —
/// is held as this 4-byte id instead of an inline 24-byte `String`, which is what shrinks `Op` from
/// two cache lines toward one. The VM resolves it back to `&str` only at the cold lookup sites
/// (method / field resolution, which then hit a hashmap or field scan anyway); the
/// disassembler resolves it for readable output. Distinct newtype (not a bare `u32`) so it can't be
/// confused with the other u32 indices an op carries (shape, proto, cache slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NameId(pub u32);

/// A global-variable **slot index** into the VM's per-run globals vector (P-VMT-GSLOT). Top-level
/// bindings and `fn` names used to be resolved by hashing their name against a `HashMap` on every
/// access — the dominant cost of a top-level loop and of every global-function call. The compiler
/// now assigns each global a dense slot at emit time, so `LoadGlobal`/`StoreGlobal`/`TakeGlobal`
/// index a `Vec` directly, with no hashing (PHP's compiled-variable model). [`Module::global_names`]
/// maps a slot back to its name for diagnostics and disassembly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlobalId(pub u32);

/// Which operand of a logical operator is being checked, for the "expects a bool on the
/// left/right" diagnostic (matching the M0 tree-walker's `eval_logical`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoolSide {
    Left,
    Right,
}

/// Where a closure's upvalue cell comes from, in the frame that builds the closure
/// (`MakeClosure`): either a celled local in one of the building frame's registers, or one of
/// the building frame's own upvalues (forwarding a capture down another level).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureFrom {
    /// The cell currently in register `0` (the field is the register index).
    Local(Reg),
    /// The building frame's `index`-th upvalue cell.
    Upvalue(u16),
}

/// A prelude collection builtin, called directly by name (`len(x)`, `map(xs, f)`). These
/// are never first-class values in the M1.3 subset — a program that passes one around
/// (rather than calling it) is left unsupported — so they ride in a dedicated `CallBuiltin`
/// op rather than being materialized into a register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    Len,
    Map,
    Filter,
    Sum,
    /// `assert(cond)` / `assert(cond, msg)` — abort (a `Panic` diagnostic) when `cond` is false.
    /// The assertion primitive the test runner's `@test` blocks (object-model slice 6) rest on.
    Assert,
    /// `sleep(ms)` — produce a leaf timer future (Track A.2) that becomes ready once the executor's
    /// logical clock reaches `now + ms`. The first future that can actually report `Pending`.
    Sleep,
    /// `all(list)` — await every future in `list` concurrently, returning a `List<T>` of their
    /// results in order (Track A.9). Drives the scheduler until all are ready.
    All,
    /// `race(list)` — await the futures in `list` concurrently, returning the first result and
    /// **cancelling** the losing tasks (Track A.9 + cooperative cancellation A.8).
    Race,
    /// `map_bounded(items, n, f)` — apply the async `f` to each item, at most `n` in flight at once,
    /// returning the results as a `List<B>` in item order (Track A.9, bounded-parallelism map).
    MapBounded,
}

impl Builtin {
    /// The surface name, for diagnostics ("`map` expects a list, ...").
    pub fn name(self) -> &'static str {
        match self {
            Builtin::Len => "len",
            Builtin::Map => "map",
            Builtin::Filter => "filter",
            Builtin::Sum => "sum",
            Builtin::Assert => "assert",
            Builtin::Sleep => "sleep",
            Builtin::All => "all",
            Builtin::Race => "race",
            Builtin::MapBounded => "map_bounded",
        }
    }

    /// The builtin a prelude name refers to, if it is one this slice implements.
    pub fn from_name(name: &str) -> Option<Builtin> {
        match name {
            "len" => Some(Builtin::Len),
            "map" => Some(Builtin::Map),
            "filter" => Some(Builtin::Filter),
            "sum" => Some(Builtin::Sum),
            "assert" => Some(Builtin::Assert),
            "sleep" => Some(Builtin::Sleep),
            "all" => Some(Builtin::All),
            "race" => Some(Builtin::Race),
            "map_bounded" => Some(Builtin::MapBounded),
            _ => None,
        }
    }
}

/// A compile-time constant, materialized into a runtime value on `LoadConst`.
#[derive(Debug, Clone, PartialEq)]
pub enum Const {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A 32-bit float literal (`1.0f32`, P-PACK Phase 3).
    F32(f32),
    Str(String),
    /// A Ring 2 native module, by surface name (`use std.{json}` lowers to loading this then
    /// storing it into the named global).
    NativeModule(String),
}

/// One segment of a fused string interpolation ([`Op::BuildString`], P-VMT-STR). A `Literal` is a
/// constant-pool string copied verbatim; a `Hole` register is rendered through `display` (a
/// `Display` object was already routed through its `to_string` by the preceding `Stringify`, so by
/// this point the register holds a plain value). Replaces the old `LoadConst "" + N×(Stringify +
/// Concat)` left-fold, which allocated an intermediate `String` per part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StrPart {
    /// Index into the prototype's constant pool; the referenced `Const` is always a `Str`.
    Literal(u16),
    /// A register holding the (already `Stringify`-ed) hole value, rendered via `display`.
    Hole(Reg),
}

/// The target of a checked narrowing (`x.as<T>()`) reduced to its runtime **head constructor**.
/// Generics are erased, so only the constructor is retained — a `List<int>` target narrows on
/// "is a list", trusting the element type from the static annotation. `Named` covers user
/// records/classes/enums and the built-in `Option`/`Result` (matched by their shape name); `Dyn`
/// always matches (narrowing to the open top is a no-op).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NarrowTarget {
    Int,
    Float,
    Bool,
    String,
    /// A `bytes` target (`x is bytes` / `x.as<bytes>()`) — P-PACK 4.4.
    Bytes,
    Unit,
    List,
    Map,
    Set,
    /// A tuple target (`x.as<(int, string)>()`): matches any tuple value — head-constructor only,
    /// arity and element types erased, like `List` ignoring its element type (object-model slice 4).
    Tuple,
    Fn,
    Dyn,
    Named(String),
    /// A union target (`x.as<int | string>()`): matches if the value matches **any** member.
    AnyOf(Vec<NarrowTarget>),
    /// An abstract kind-type target (`x.as<Enum>()` / `x is Struct`): matches any value of that
    /// declaration kind, regardless of which concrete type it is.
    AnyEnum,
    AnyStruct,
    AnyClass,
    /// A **parametrized** target (`x is List<int>` / `x is Box<int>`, R3): `head` is the head-only
    /// target (matched exactly as before — this preserves the widening `x is List` and the untagged
    /// fallback), and `args` are the expected type arguments. When the value carries a reflected type
    /// (R1/R2), the matcher additionally requires its arguments match `args` (a `dyn` on either side is
    /// a wildcard), so `List<int>` no longer matches a value tagged `List<string>`. An untagged value
    /// classifies its arguments to `dyn` and so still matches head-only.
    Generic {
        head: Box<NarrowTarget>,
        args: Vec<lang_ast::reflect::TypeRepr>,
    },
}

/// How a [`Op::MakeStructInPlace`] is allowed to reuse its consumed `base` object's allocation.
///
/// This is the compile-time half of reuse analysis: the compiler always knows the construct is a
/// *self-update* (`acc = Type { ...acc, f: v }`, so `base` is consumed), but whether the in-place
/// mutation is sound depends on `base` being uniquely owned **and** already the target shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReuseCheck {
    /// Decide at execution: reuse in place only when `base.refcount() == 1` and `base`'s shape is
    /// the target shape; otherwise fall back to a fresh copying allocation (always correct). This
    /// is the runtime-checked analogue of the COW list append — safe under any aliasing.
    Runtime,
    /// The compiler's linearity analysis proved `base` is the sole owner of a same-shape object at
    /// this point (the accumulator is never aliased in its scope), so the in-place mutation is
    /// unconditional — no runtime refcount/shape branch. This is the Perceus/Roc "hoist the
    /// uniqueness decision to compile time" path.
    Static,
}

/// One register-machine instruction.
#[derive(Debug, Clone)]
pub enum Op {
    /// `dst = consts[k]`
    LoadConst {
        dst: Reg,
        k: u16,
    },
    /// `dst = src` (a refcounted copy: release old `dst`, retain `src`).
    Move {
        dst: Reg,
        src: Reg,
    },
    /// `dst = globals[name]`, or raise E0005 ("cannot find `name` in this scope") at `span`
    /// if the global is unbound. The single name-resolution path for everything that is not a
    /// frame-local register: top-level bindings, function names, and (at runtime) unknowns.
    LoadGlobal {
        dst: Reg,
        global: GlobalId,
        span: Span,
    },
    /// `globals[name] = src` (refcounted: release old binding, retain `src`). Only emitted at
    /// the top level — functions never assign globals in the M1.2 subset.
    StoreGlobal {
        global: GlobalId,
        src: Reg,
    },
    /// `dst = take(globals[name])` — **move** the global's value into `dst`, leaving `unit` in the
    /// slot (no retain, unlike `LoadGlobal`). Transfers the single owning reference so a following
    /// `ConcatInPlace` sees unique ownership (refcount 1) when the global is otherwise unaliased —
    /// the copy-on-write self-append fast path for a global accumulator (`acc ~= [x]`). The compiler
    /// re-stores the result with `StoreGlobal`. Raises `UnknownName` at `span` if the global is
    /// unbound (matching `LoadGlobal`).
    TakeGlobal {
        dst: Reg,
        global: GlobalId,
        span: Span,
    },
    /// `drop(reg)` — release the value held in `reg` and leave `unit` in its place. Inserted by the
    /// compiler's targeted drop insertion to free a compiler-generated **single-use temporary** at its
    /// last use (e.g. a `LoadField` receiver) instead of waiting for frame teardown. Freeing the
    /// temporary promptly restores the accumulator's unique ownership, so a following self-update can
    /// reuse it in place even when the update *reads* the accumulator (`acc = T { ...acc, x: acc.x }`).
    /// Clearing to `unit` keeps it idempotent with `set_reg`'s release-on-overwrite and frame teardown
    /// (both then release `unit`, a no-op) — so no value is double-freed. See `p-reuse-analysis.md`.
    ///
    /// `relevant` (Phase 4, from the IR `DropVar`'s destructor-relevance bit): when `true`, the
    /// release runs through the destructor-firing path (`release_value`) so a `destruct` block fires
    /// at this last use if this is the final owning reference; when `false`, the value provably
    /// reaches no destructor, so the plain `release` is used (the fast path, unchanged from Phase 3).
    Drop {
        reg: Reg,
        relevant: bool,
    },
    /// `dst = lhs ~ rhs`, **consuming `lhs`** (the copy-on-write list-append fast path). `lhs` is the
    /// taken-out accumulator (its register is left `unit`); `rhs` is borrowed. When `lhs` is a
    /// uniquely-owned list (`refcount == 1`) its backing buffer is extended in place — O(1)
    /// amortized; otherwise (an alias keeps the count > 1) it copies, preserving immutable
    /// semantics. A non-list pairing falls back to display concatenation, identical to `Op::Binary`'s
    /// `~`. The compiler emits this only for the self-append shape `name = name ~ rhs` where `rhs`
    /// does not mention `name`.
    ConcatInPlace {
        dst: Reg,
        lhs: Reg,
        rhs: Reg,
        span: Span,
    },
    /// `dst = <closure over proto>` — materialize a function value referencing `proto` and
    /// capturing one cell per entry of `captures` (in order), each becoming an upvalue of the
    /// new closure. A top-level `fn`/closure captures nothing (`captures` is empty).
    MakeClosure {
        dst: Reg,
        proto: u32,
        captures: Box<[CaptureFrom]>,
    },
    /// `dst = <cell holding src>` — box a value into a fresh mutable cell, the storage for a
    /// local that an inner closure captures. Reads/writes of that local go through the cell, so
    /// the closure and the defining frame share one live binding (matching the tree-walker's
    /// `Rc`-captured scope chain).
    MakeCell {
        dst: Reg,
        src: Reg,
    },
    /// `dst = *cell` — read the value out of a cell held in register `cell`.
    CellGet {
        dst: Reg,
        cell: Reg,
    },
    /// `*cell = src` — write into a cell held in register `cell` (release old, retain new).
    CellSet {
        cell: Reg,
        src: Reg,
    },
    /// `dst = *upvalues[index]` — read through this frame's `index`-th captured cell.
    UpvalueGet {
        dst: Reg,
        index: u16,
    },
    /// `*upvalues[index] = src` — write through this frame's `index`-th captured cell.
    UpvalueSet {
        index: u16,
        src: Reg,
    },
    /// `dst = <native function value>` — materialize a first-class prelude builtin (`len`/`map`/
    /// `filter`/`sum`) so it can be stored, passed, and called indirectly. A direct call still
    /// uses `CallBuiltin`; this is only for a bare reference to the builtin.
    LoadNativeFn {
        dst: Reg,
        func: Builtin,
    },
    /// `dst = [items...]` — build a heap list, retaining each element into it.
    MakeList {
        dst: Reg,
        items: Box<[Reg]>,
        /// The list's reflected element type (runtime type-argument reflection, R1): an index into
        /// [`Module::type_reprs`], or `None` when the literal carried no checker-resolved type (it
        /// stays untagged and reflects head-only). Stamped onto the freshly-built list so `type_of`
        /// recovers the element type after a `dyn` launder. Invisible to value semantics.
        reflect: Option<u32>,
    },
    /// `dst = <empty List<packed>>` (P-PACK 2.5) — allocate an empty flat raw-primitive buffer for a
    /// `List<@packed struct>` literal, using `schema` (an index into [`Module::packed_schemas`]). It
    /// is filled element-by-element by [`Op::PackedListPush`]; building this way (rather than from all
    /// N element registers at once) keeps only one element object live at a time.
    PackedListNew {
        dst: Reg,
        schema: u32,
    },
    /// `dst = packed-push(list, value)` (P-PACK 2.5) — pack the value-struct in `value` onto the flat
    /// `List<packed>` in `list` and place the (in-place-extended) list in `dst`. `list` is the
    /// streaming accumulator — an ANF temp, uniquely owned — so the buffer extends in place; `value`
    /// is consumed (its primitive fields copied into the buffer, the element object then dropped by
    /// the compiler-emitted release). If `value` cannot be packed (a shape mismatch — never for a
    /// checked `@packed` type) the list demotes to boxed, keeping the layout invisible to `RunResult`.
    PackedListPush {
        dst: Reg,
        list: Reg,
        value: Reg,
        span: Span,
    },
    /// `dst = (items...)` — build a heap tuple (object-model slice 4), retaining each element into
    /// it exactly like `MakeList`.
    MakeTuple {
        dst: Reg,
        items: Box<[Reg]>,
    },
    /// `dst = receiver.N` — positional tuple projection by a constant index (object-model slice 4).
    /// The index is in range by construction (the checker verifies it against the tuple's arity).
    TupleIndex {
        dst: Reg,
        receiver: Reg,
        index: u32,
        span: Span,
    },
    /// `dst = start..end` (exclusive) or `start..=end` (inclusive) — eagerly build a `List<int>`
    /// from two integer registers, raising E0007 at `span` if either bound is not an int. Mirrors
    /// the tree-walker's eager materialization; an empty range yields an empty list.
    MakeRange {
        dst: Reg,
        start: Reg,
        end: Reg,
        inclusive: bool,
        span: Span,
    },
    /// `dst = {key: value, ...}` — build a heap map (sorted-key), retaining each value. Keys
    /// are validated by a preceding `RequireMapKey`, so they are known strings here.
    MakeMap {
        dst: Reg,
        entries: Box<[(Reg, Reg)]>,
        /// The map's reflected `Map(K, V)` type (R1): an index into [`Module::type_reprs`], or `None`
        /// when the literal carried no checker-resolved type. Stamped onto the built map so `type_of`
        /// recovers it after a `dyn` launder. Invisible to value semantics, like `MakeList`'s tag.
        reflect: Option<u32>,
    },
    /// Require `reg` to be a string (a map key), else raise E0007 ("map keys must be strings,
    /// found <type>") at `span`. Emitted between a map entry's key and value so the error
    /// timing matches the M0 tree-walker (key checked before the value is evaluated).
    RequireMapKey {
        reg: Reg,
        span: Span,
    },
    /// `dst = <elements of src to iterate>`. A list yields a retained shallow copy; a map
    /// yields a new list of its values in sorted-key order; anything else raises E0007
    /// ("cannot iterate over <type>") at `span`. Snapshots iteration, as the M0 tree-walker
    /// does, so `dst` is always a list the loop can index.
    IterSnapshot {
        dst: Reg,
        src: Reg,
        span: Span,
    },
    /// `dst = len(src)` where `src` is a list (an iteration snapshot). When `src` is not a list —
    /// only reachable when an `Iterable::iter` returned a non-list — raises E0007 at `span`.
    ListLen {
        dst: Reg,
        src: Reg,
        span: Span,
    },
    /// `dst = list[index]` (retained), where `list` is a list and `index` an in-bounds int.
    ListGet {
        dst: Reg,
        list: Reg,
        index: Reg,
    },
    /// Streaming `for` step (Track I.2): advance the iterator in `iter` (a `Value::Iter`) one element.
    /// On success `elem` ← the next element (retained, owned) and `has` ← `true`; at end `elem` ← unit
    /// and `has` ← `false`. A `map`/`filter` closure runs here, so it can raise (a closure error, or a
    /// non-bool `filter` verdict → E0007) at `span`. The loop tests `has` to continue or exit.
    IterForNext {
        iter: Reg,
        elem: Reg,
        has: Reg,
        span: Span,
    },
    /// `dst = builtin(args...)` — a prelude collection builtin (`len`/`map`/`filter`/`sum`).
    /// `map`/`filter` re-enter the VM to call their closure argument per element.
    CallBuiltin {
        dst: Reg,
        builtin: Builtin,
        args: Box<[Reg]>,
        span: Span,
    },
    /// `dst = recv.method(args...)` — a runtime-dispatched method call (mirroring the M0
    /// tree-walker's `call_method`). On an object, `method` resolves through the module's
    /// instance-method table by the receiver's type name and pushes a call frame `[recv,
    /// args...]`; on a list/map/string, `count`/`enumerate` are the built-in zero-arg methods
    /// computed inline. An unresolved method raises E0005, an arity/type misuse E0007, at
    /// `span`.
    CallMethod {
        dst: Reg,
        recv: Reg,
        method: NameId,
        args: Box<[Reg]>,
        span: Span,
        /// Inline-cache slot (index into the VM's per-run cache array). Memoizes the last receiver
        /// shape → resolved method prototype, so a monomorphic site skips the `(type, method)`
        /// hashmap lookup (and its two `String` clones). Assigned by the compiler.
        cache: u32,
        /// In-place-reuse token (Phase 5.1c, from the IR `Rvalue::Method.reuse`): set on a collection
        /// **method self-update** `m = m.set(k, v)` whose receiver is a directly-held local. When the
        /// runtime receiver is actually a map and `method` is an in-place-capable update (`set`/
        /// `remove`), its sole-owned backing buffer is mutated in place (consuming the receiver
        /// register) instead of copied; an aliased map, or any other receiver (a user method that
        /// happens to be named `set`), takes the ordinary borrowing dispatch — so reuse stays
        /// observationally invisible. Always `false` for a non-self-update call.
        reuse: bool,
    },
    /// `dst = recv[index]` — index access (the `Index` trait / list element access), mirroring
    /// the tree-walker's `eval_index`. On an object it dispatches to the `get` method (pushing a
    /// call frame `[recv, index]`); on a list it addresses an element by integer position,
    /// raising E0016 out of bounds. A non-int list index or a non-indexable receiver raises
    /// E0007, at `span`.
    Index {
        dst: Reg,
        recv: Reg,
        index: Reg,
        span: Span,
    },
    /// `dst = recv[index].field` — a fused indexed field read (P-PACK 2.5+). The compiler emits this
    /// only when the checker proved `recv` is a built-in `List` (see `Rvalue::IndexField`): a packed
    /// list decodes the one field's word(s) directly, without materializing the indexed element; a
    /// boxed (demoted) list does the ordinary index-then-load. Either way the result equals the
    /// `Index` + `LoadField` it replaces. A non-int/out-of-bounds index or missing field raises the
    /// same diagnostic at `span` as the unfused pair would.
    IndexField {
        dst: Reg,
        recv: Reg,
        index: Reg,
        field: NameId,
        span: Span,
    },
    /// `dst = Type { named..., ...spread }` — construct a declared struct/class instance whose
    /// layout is `shapes[shape]`. `named` gives each provided field's slot index and value
    /// register; `spread` (if present) is a base object every still-unset declared slot is
    /// copied from. A declared slot left unset by both raises E0009 ("missing field(s) ...")
    /// at `span`.
    MakeStruct {
        dst: Reg,
        shape: u32,
        named: Box<[(u16, Reg)]>,
        spread: Option<Reg>,
        /// The reflected type (R2): an index into [`Module::type_reprs`] for a **generic**
        /// instantiation, or `None` for a non-generic type (recovered head-only). Stamped onto the
        /// built struct so `type_of` recovers its type arguments after a `dyn` launder.
        reflect: Option<u32>,
        span: Span,
    },
    /// `dst = Type { named..., ...base }` for a **self-update** (`acc = Type { ...acc, f: v }`),
    /// where `base` holds the consumed accumulator — its single reference has been moved into the
    /// `base` register (cleared before this op, transferring the ref), the struct analogue of
    /// [`Op::ConcatInPlace`]. When `base` is uniquely owned and already the target `shape`, its
    /// allocation is **reused**: only the `named` slots are overwritten (each unchanged field's
    /// reference transfers from `base` to the result), avoiding the allocation and per-field
    /// retain a plain `MakeStruct { spread }` performs. `check` selects the runtime-checked or the
    /// statically-proven path (see [`ReuseCheck`]); the runtime path and the `Static`-but-aliased
    /// fallback build a fresh object exactly like `MakeStruct` and release `base`. The missing-field
    /// guarantee holds by construction (a same-shape base provides every field).
    MakeStructInPlace {
        dst: Reg,
        shape: u32,
        named: Box<[(u16, Reg)]>,
        base: Reg,
        check: ReuseCheck,
        /// The reflected type (R2), as on [`Op::MakeStruct`]. A self-update rebuilds a value of the
        /// same type, so the reused node is (re)stamped with the current literal's tag.
        reflect: Option<u32>,
        span: Span,
    },
    /// `dst = Type { key: value, ...spread }` for an **opaque** `use`-imported type, whose
    /// real field set is unknown until the literal supplies it. The runtime object's shape is
    /// built from the (spread ∪ named) keys in sorted order — matching the M0 tree-walker's
    /// `BTreeMap`-ordered field bag — with no missing/unknown-field checks.
    MakeOpaque {
        dst: Reg,
        type_name: NameId,
        keys: Box<[(NameId, Reg)]>,
        spread: Option<Reg>,
    },
    /// `dst = <enum variant>` — construct the `(enum, variant)` value whose shape is
    /// `shapes[shape]`, carrying `args` as the variant's positional data (empty for a no-data
    /// variant). Each argument is retained into the value.
    MakeEnum {
        dst: Reg,
        shape: u32,
        args: Box<[Reg]>,
        /// The reflected type for a **generic** enum-variant construction (R2b.2): an index into
        /// [`Module::type_reprs`], or `None` for a non-generic enum or an ordinary variant. Stamped
        /// onto the built value so `type_of` recovers the enum's type arguments after a `dyn` launder.
        reflect: Option<u32>,
    },
    /// `dst = Enum.try_from(s)` / `Enum.from(s)` — construct a **payload-free** enum case from the
    /// string in `arg`, matched by case name (the PHP `tryFrom`/`from` pair). `cases` lists every
    /// payload-free `(name, shape)` of the enum. On a hit: `from` (`panic = true`) yields the case
    /// itself; `try_from` (`panic = false`) wraps it as `Option.some` (`some_shape`). On a miss:
    /// `from` raises an `E0010` panic at `span`; `try_from` yields `Option.none` (`none_shape`). A
    /// non-string `arg` raises `E0007`.
    EnumFromStr {
        dst: Reg,
        arg: Reg,
        enum_name: NameId,
        cases: Box<[(NameId, u32)]>,
        some_shape: u32,
        none_shape: u32,
        panic: bool,
        span: Span,
    },
    /// `dst = obj.field` — load an object field by name (resolved through the receiver's
    /// shape). A receiver that is not an object, or lacks the field, raises E0005 at `span`.
    LoadField {
        dst: Reg,
        obj: Reg,
        field: NameId,
        span: Span,
        /// Inline-cache slot (index into the VM's per-run cache array). Memoizes the last receiver
        /// shape → field slot index, so a monomorphic site skips the linear `slot_of` field scan.
        /// Assigned by the compiler.
        cache: u32,
    },
    /// `dst = (obj with field = value)` — in-place field assignment (`x.f = v`, Phase 5.2),
    /// **value semantics**. When `reuse` is set and `obj` is uniquely owned at runtime (`refcount
    /// == 1`) its slot is overwritten in place (the displaced old value's `destruct` fires now) and
    /// `obj` is consumed (its register cleared) into `dst`; otherwise a shallow copy with the field
    /// replaced is produced, leaving any aliased observer's object untouched. `reuse` is the IR
    /// reuse token (set only on the `x.f = v` self-update, for a directly-held local or a
    /// `TakeGlobal`-moved global); an unmarked op always copies.
    SetField {
        dst: Reg,
        obj: Reg,
        field: NameId,
        value: Reg,
        reuse: bool,
        span: Span,
    },
    /// `dst = next_id()` — the deterministic seeded counter (1, 2, 3, …), reproducing the M0
    /// tree-walker's `IdGen` (seed 1).
    NextId {
        dst: Reg,
    },
    /// `panic(msg)` — record E0010 ("panic: <msg display>") at `span` and abort the program.
    Panic {
        msg: Reg,
        span: Span,
    },
    /// The `?` operator. If `src` is `Ok(x)`/`some(x)`, `dst = x` (unit for the void `Ok()`)
    /// and execution continues; if it is `Err(_)`/`none`, that value is early-returned from the
    /// current frame (the M0 `Unwind::Return`); anything else raises E0007 at `span`. On the
    /// early-return path, the `(reg, relevant)` pairs in `on_error` are dropped first — the drop
    /// pass's reclamation of the frame locals this `?` abandons, destructor-relevant ones firing
    /// `destruct` (Phase 4.2c) — in the order given (innermost scope first, reverse-construction).
    TryUnwrap {
        dst: Reg,
        src: Reg,
        on_error: Vec<(Reg, bool)>,
        span: Span,
    },
    /// The `??` operator. If `src` is `Ok(x)`/`some(x)`, `dst = x` and execution continues; if
    /// it is `Err(_)`/`none`, jump to `fallback` (the right-hand expression); anything else
    /// raises E0007 at `span`.
    Coalesce {
        dst: Reg,
        src: Reg,
        fallback: u32,
        span: Span,
    },
    /// `dst = src.as<T>()` — checked narrowing. `dst = some(src)` (built from `some_shape`) if
    /// `src`'s runtime head constructor matches `target`; otherwise `dst = none` (`none_shape`).
    /// The two `Option` shape indices are resolved at compile time, so the VM only tests and
    /// constructs — the op cannot fail, so it carries no span.
    Narrow {
        dst: Reg,
        src: Reg,
        /// Boxed (P-VMT-OPSZ): `NarrowTarget` is 32 bytes and narrowing is a cold op, so it lives
        /// behind a pointer to keep it off the hot instruction stream.
        target: Box<NarrowTarget>,
        some_shape: u32,
        none_shape: u32,
    },
    /// The `is` type-test: `dst = bool` — `true` if `src`'s runtime head constructor matches
    /// `target`. Shares the matcher with [`Op::Narrow`] but yields a plain `bool` (no `Option`
    /// allocation); it cannot fail, so it carries no span.
    IsType {
        dst: Reg,
        src: Reg,
        /// Boxed (P-VMT-OPSZ), as in [`Op::Narrow`].
        target: Box<NarrowTarget>,
    },
    /// `dst = make_gen(src)` (Track G.1b): wrap the step closure in `src` into a generator iterator
    /// (`IterState::Gen`). The generator desugar emits this as the tail of a generator function — the
    /// step closure is the lowered state machine over `mut`-captured cells; the resulting iterator
    /// composes with every Track-I adapter. Cannot fail, so it carries no span.
    MakeGen {
        dst: Reg,
        src: Reg,
    },
    /// `dst = make_future(src)` (Track A.1): wrap the lazy thunk closure in `src` into a `Future`.
    /// The async desugar emits this as the tail of an `async fn` — the thunk defers the body until the
    /// future is awaited/run. Cannot fail, so it carries no span.
    MakeFuture {
        dst: Reg,
        src: Reg,
    },
    /// `dst = run_future(src)` (Track A.2/A.3): drive the future in `src` to completion via the
    /// executor, yielding its value — the top-level `expr.await`. Polls; on pending advances the
    /// logical clock and re-polls. Carries a span for the call boundary (the step body can fault).
    RunFuture {
        dst: Reg,
        src: Reg,
        span: Span,
    },
    /// `dst = poll_future(src)` (Track A.3): poll the future in `src` once — `some(v)` if ready, `none`
    /// if pending. The single-step primitive the async state machine emits at each `.await`. Carries a
    /// span (the step body can fault).
    PollFuture {
        dst: Reg,
        src: Reg,
        span: Span,
    },
    /// `dst = pending` (Track A.3): the async pending sentinel — what a state-machine step returns to
    /// signal it suspended at an `.await`. Cannot fail, so it carries no span.
    LoadPending {
        dst: Reg,
    },
    /// Open a structured-concurrency scope (Track A.3b) — the start of a lowered `concurrent { }`.
    ScopeBegin,
    /// `dst = spawn(src)` (Track A.3b): register the future in `src` as a task in the current scope and
    /// yield a handle (a `Future<T>`). Carries a span (the operand must be a future).
    Spawn {
        dst: Reg,
        src: Reg,
        span: Span,
    },
    /// `dst = isolate callee(args)` (isolates I.4b): spawn the call as a concurrent unit in a fresh
    /// isolate, yielding a handle (`Future<T>`). Unlike [`Self::Spawn`] the callee and arguments are
    /// carried **unbuilt** (like a [`Self::Call`]) so a real-thread isolate can copy-marshal the args and
    /// reconstruct the callee on the worker — a pre-built future captures its args in the parent heap and
    /// cannot cross a thread. In the deterministic sandbox this is identical to building `callee(args)`
    /// and `Spawn`ing it (a cooperative task), so the differential is unchanged. Carries a span.
    SpawnIsolate {
        dst: Reg,
        callee: Reg,
        args: Box<[Reg]>,
        span: Span,
    },
    /// Close the current concurrency scope (Track A.3b): drive every task spawned in it to completion
    /// (the join), then pop the scope. Carries a span (a task body can fault at the join).
    ScopeEnd {
        span: Span,
    },
    /// `dst = channel(capacity)` (isolates I.1): create a bounded channel with the buffer size in
    /// `capacity` and yield a `(Sender, Receiver)` tuple of endpoint ids. Carries a span (the capacity
    /// must be a non-negative int).
    MakeChannel {
        dst: Reg,
        capacity: Reg,
        span: Span,
    },
    /// `attributes_of::<T>()`: `dst = List<Attributed<T>>` — the `#[T(...)]` attributes from the
    /// module manifest, each materialized into a `T` struct and paired with its target. `type_name`
    /// is the attribute type, resolved at compile time (closed-world). Reads `Module::reflection`.
    AttributesOf {
        dst: Reg,
        type_name: NameId,
    },
    /// `roles_of()`: `dst = List<RoleBinding>` — the `(declaration, Role)` semantic-role index from
    /// the module's reflection info, each entry materialized into a `RoleBinding { target, role }`.
    /// Compile-time resolved (closed-world); reads `Module::reflection`. (P2.7.)
    RolesOf {
        dst: Reg,
    },
    /// `type_of(value)`: `dst = Type` — the runtime [`lang_ast::reflect`] head-constructor descriptor
    /// of the value in `src` (`List(Dyn)`, `Named("Route")`, `Int`, …). Generics are erased at
    /// runtime, so element/argument types collapse to `Dyn` at this fidelity.
    TypeOf {
        dst: Reg,
        src: Reg,
    },
    /// `from_bytes::<T>(blob)` — deserialize the `bytes` in `src` into a flat `List<T>` (P-PACK 4.4).
    /// `schema` is element `T`'s interned [`PackedSchemaDef`] index; the VM wraps the raw buffer as a
    /// packed list (validating the length is a whole number of elements). The inverse of `to_bytes`.
    FromBytes {
        dst: Reg,
        src: Reg,
        schema: u32,
        span: Span,
    },
    /// `type_of(value)` where the checker resolved the operand's **concrete** static type: `dst` is
    /// the full-fidelity [`lang_ast::reflect::TypeRepr`] baked as a constant (`Type.List(Type.Int)`),
    /// recovering the element/argument types runtime erasure drops. The operand is still evaluated
    /// (for its side effects) but its register is unused. (`type_of` fidelity A, P2.3.)
    TypeOfStatic {
        dst: Reg,
        /// Boxed (P-VMT-OPSZ): a full-fidelity `TypeRepr` is 56 bytes and `type_of` is a cold op.
        repr: Box<lang_ast::reflect::TypeRepr>,
    },
    /// `dst = <the reflection `Type` value for `name`>` — materialize a bare type name as a
    /// first-class value (the one "type as a value" representation, shared with `type_of` and stored
    /// type-refs). Emitted when an `invoke(...)` receiver is a bare type name; `Op::Invoke` resolves
    /// the `Type` value back to the named type via `reflection_type_name` to dispatch its associated
    /// function. The kind is classified at run time from the module's reflection (`type_ref_repr`).
    TypeValue {
        dst: Reg,
        name: NameId,
    },
    /// `invoke(recv, name, args)`: `dst = Result<dyn, dyn>` — fallible by-name dispatch. `recv` holds
    /// an object (→ instance method, keyed `(shape, name)`) or a reflection `Type` value (→ associated
    /// function, keyed `(type, name)`); `name` is a runtime `string`; `args` a runtime `List`. An unknown name,
    /// a non-string name, a non-list args, or an arity mismatch builds `Result.Err(string)` (via
    /// `err_shape`); a hit pushes a call frame whose result is wrapped in `Result.Ok` (via
    /// `ok_shape`). A panic inside the invoked body propagates as a normal abort (P2.6).
    Invoke {
        dst: Reg,
        recv: Reg,
        name: Reg,
        args: Reg,
        ok_shape: u32,
        err_shape: u32,
        span: Span,
    },
    /// A call-site-typed native module call (`json.parse::<T>(args)`): `dst = T`. The VM marshals the
    /// argument registers, runs the shared native function (keyed by `module`/`func`), and
    /// materializes the result tree into a value of `T` per `recipe` (the checker-resolved turbofish
    /// type; `None` means `T` had no decoding — a checker error — and the VM raises at `span`).
    ExtCall {
        dst: Reg,
        module: NameId,
        func: NameId,
        args: Box<[Reg]>,
        /// Boxed (P-VMT-OPSZ): a `TypeRecipe` is 48 bytes and only a call-site-typed native call
        /// (`json.parse::<T>`) carries one, so it lives behind a pointer.
        recipe: Option<Box<lang_stdlib::TypeRecipe>>,
        span: Span,
    },
    /// A `match` literal test: if `src` equals the literal, continue; else jump to `fail` (the
    /// next arm). Three variants for the three literal pattern kinds.
    MatchInt {
        src: Reg,
        value: i64,
        fail: u32,
    },
    MatchStr {
        src: Reg,
        value: NameId,
        fail: u32,
    },
    MatchBool {
        src: Reg,
        value: bool,
        fail: u32,
    },
    /// A `match` variant test: if `src` is an enum of the given variant (and, when
    /// `type_name` is set, that enum) with `arity` data fields, continue; else jump to `fail`.
    MatchVariant {
        src: Reg,
        type_name: Option<NameId>,
        variant: NameId,
        arity: u16,
        fail: u32,
    },
    /// A `match` **tuple** test (object-model slice 4b.2): if `src` is a tuple of exactly `arity`
    /// elements, continue; else jump to `fail`. Elements are then read with `TupleIndex` for the
    /// sub-patterns.
    MatchTuple {
        src: Reg,
        arity: u16,
        fail: u32,
    },
    /// `dst = src.data[index]` (retained) — extract an enum variant's positional field for a
    /// sub-pattern, after a `MatchVariant` has confirmed the shape.
    ExtractField {
        dst: Reg,
        src: Reg,
        index: u16,
    },
    /// No `match` arm matched `src`: raise E0007 ("no match arm matched the value <...>") at
    /// `span` (the M0 runtime non-exhaustive-match error).
    MatchFail {
        src: Reg,
        span: Span,
    },
    /// `dst = callee(args...)`. Pushes a new call frame; the callee must be a closure whose
    /// prototype's arity equals `args.len()` (else E0007 at `span`); a non-callable callee is
    /// E0007 ("<type> is not callable") at `span`.
    Call {
        dst: Reg,
        callee: Reg,
        args: Box<[Reg]>,
        span: Span,
    },
    /// Return `src` from the current frame to the caller's destination register (or end the
    /// program if returning from the top-level frame).
    Return {
        src: Reg,
    },
    /// `dst = op src` — may raise (E0007) at `span`.
    Unary {
        op: UnaryOp,
        dst: Reg,
        src: Reg,
        span: Span,
    },
    /// `dst = mask_to_width(src, signed, bits)` — reduce an erased i64 to a fixed-width integer's
    /// range (Tier W). Emitted after a same-width `+ - *` / unary `-` on an `IntN`. Total (never
    /// raises); the shared `lang_stdlib::mask_to_width` runs identically here and in the tree-walker.
    MaskWidth {
        dst: Reg,
        src: Reg,
        signed: bool,
        bits: u8,
    },
    /// `dst = a op b` read as a `signed`/unsigned `bits`-wide integer — the sign-dependent
    /// fixed-width ops `/ % < <= > >=` (Tier W3). `/ %` may raise E0008 (division by zero) at `span`
    /// and mask their result into the width; `< <= > >=` yield a bool. The shared
    /// `apply_binary_wide` runs identically here and in the tree-walker. `op` ∈ {Div,Rem,Lt,Le,Gt,Ge}.
    WideInt {
        op: BinaryOp,
        dst: Reg,
        a: Reg,
        b: Reg,
        signed: bool,
        bits: u8,
        span: Span,
    },
    /// `dst = int_method_width(recv, method, arg, bits)` — a bit intrinsic computed **within a
    /// fixed width** (Tier W5): `count_ones`/`leading_zeros`/`rotate_*`/`reverse_bits`/… on an `IntN`
    /// receiver act on the low `bits` bits, not the full erased i64. `arg` is the sole `rotate_*`
    /// shift amount (absent for the nullary intrinsics). Total (never raises). Shared with the
    /// tree-walker. `method` is never `Convert`.
    WidthIntMethod {
        dst: Reg,
        recv: Reg,
        method: lang_stdlib::IntMethod,
        arg: Option<Reg>,
        bits: u8,
        span: Span,
    },
    /// `dst = a op b` — may raise (E0007/E0008) at `span`. Never `&&`/`||` (those lower to
    /// branches).
    Binary {
        op: BinaryOp,
        dst: Reg,
        a: Reg,
        b: Reg,
        span: Span,
    },
    /// Require `reg` to be a bool, else raise E0007 with the logical-operator message.
    RequireBool {
        reg: Reg,
        side: BoolSide,
        op: BinaryOp,
        span: Span,
    },
    /// Require `reg` to be a bool, else raise E0007 with the `if`-condition message
    /// ("`if` condition must be a bool, found <type>") at `span`.
    RequireCondBool {
        reg: Reg,
        span: Span,
    },
    /// Unconditional jump to `target`.
    Jump {
        target: u32,
    },
    /// Jump to `target` if `reg` (a bool) is true.
    JumpIfTrue {
        reg: Reg,
        target: u32,
    },
    /// Jump to `target` if `reg` (a bool) is false.
    JumpIfFalse {
        reg: Reg,
        target: u32,
    },
    /// A fused conditional branch (P-VMT-CBR): require `reg` be a bool (else raise E0007, `` `if`
    /// condition must be a bool ``, at `span`), then jump to `target` if it is `false` and fall
    /// through if `true`. Emitted at `if`/`while` condition sites in place of the adjacent
    /// `RequireCondBool` + `JumpIfFalse` pair (one dispatch instead of two, per condition test) —
    /// byte-identical behavior, the Binary that computes the condition is untouched.
    CondBranch {
        reg: Reg,
        target: u32,
        span: Span,
    },
    /// Print `reg`'s display form followed by a newline.
    Echo {
        reg: Reg,
    },
    /// `dst = display_string(src)` — render `src` for `echo`/interpolation. A user object that
    /// implements the `Display` trait dispatches to its `to_string` method (pushing a call
    /// frame); every other value is copied unchanged, since the consuming `Echo`/`Concat`
    /// stringifies it via `display`. Emitted before each `Echo` and each interpolation hole.
    Stringify {
        dst: Reg,
        src: Reg,
        span: Span,
    },
    /// `dst = concat(display(part) for part in parts)` — build an interpolated string in one pass
    /// and one output allocation (P-VMT-STR). Each `Literal` part is copied verbatim from the
    /// constant pool; each `Hole` part is a register rendered via `display` (a `Display` object was
    /// already dispatched to `to_string` by a preceding `Stringify`). Replaces the pre-S5
    /// `LoadConst "" + N×(Stringify + Concat)` fold, which allocated an intermediate `String` per
    /// part and reallocated the accumulator on every step.
    BuildString {
        dst: Reg,
        parts: Box<[StrPart]>,
    },
    /// Push a precomputed diagnostic (`diagnostics[idx]`) and halt — the unknown-name (E0005)
    /// and immutable-assignment (E0006) errors, whose text the compiler knows statically.
    Raise {
        idx: u16,
    },
    /// Stop the program successfully.
    Halt,
}

/// A compiled function prototype: instructions, the constant pool, the precomputed raise
/// diagnostics, the parameter count, and the number of registers the frame needs. The
/// top-level program is just the prototype at index 0 (`num_params == 0`); every `fn` and
/// closure is another prototype.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub code: Vec<Op>,
    pub consts: Vec<Const>,
    pub diagnostics: Vec<Diagnostic>,
    /// Parameters occupy registers `0..num_params` on entry.
    pub num_params: u16,
    pub num_registers: u16,
    /// The frame's source locals (params + body bindings) in **construction order**, by register —
    /// the order they come to life as the function runs. On a panic the VM walks this reversed and
    /// fires the `destruct` of any register still holding a live destructor-bearing value, so an
    /// aborting program destroys its abandoned frame values deterministically (spec §6, Phase
    /// 4.2c-ii). Pure metadata: it changes no codegen, and is empty for the placeholder.
    pub frame_locals: Vec<u16>,
    /// Optional parameters with defaults: each `(register, thunk_proto)` pairs a trailing parameter
    /// register with the zero-argument prototype that computes its default value (against globals
    /// only). When a call omits that argument, the VM runs the thunk to fill the register. Empty for
    /// a function with no defaults; the entries are the trailing parameters, so the lowest legal
    /// argument count is `num_params - defaults.len()` (less the receiver for a method).
    pub defaults: Vec<(u16, u32)>,
}

impl Chunk {
    /// An empty placeholder prototype, used to reserve the top-level slot (index 0) while
    /// nested functions are compiled into later slots.
    pub fn placeholder() -> Chunk {
        Chunk {
            code: Vec::new(),
            consts: Vec::new(),
            diagnostics: Vec::new(),
            num_params: 0,
            num_registers: 0,
            defaults: Vec::new(),
            frame_locals: Vec::new(),
        }
    }

    /// Render the chunk as stable, human-readable disassembly for snapshot tests. `names` is the
    /// owning module's interned name table (P-VMT-OPSZ), used to resolve each op's [`NameId`]s back
    /// to their strings so the output is unchanged from the pre-interning inline-`String` form.
    /// `global_names` (P-VMT-GSLOT) resolves each global slot back to its name likewise.
    pub fn disassemble(&self, names: &[String], global_names: &[String]) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "params: {}, registers: {}",
            self.num_params, self.num_registers
        );
        if !self.defaults.is_empty() {
            out.push_str("defaults:\n");
            for (reg, proto) in &self.defaults {
                let _ = writeln!(out, "  r{reg} = thunk proto {proto}");
            }
        }
        if !self.consts.is_empty() {
            out.push_str("constants:\n");
            for (i, c) in self.consts.iter().enumerate() {
                let _ = writeln!(out, "  k{i} = {}", const_repr(c));
            }
        }
        out.push_str("code:\n");
        for (i, op) in self.code.iter().enumerate() {
            let _ = writeln!(
                out,
                "  {i:>3}  {}",
                op_repr(op, &self.diagnostics, names, global_names)
            );
        }
        out
    }
}

/// One entry in a module's instance-method dispatch table: a class's method, keyed by the
/// `(type_name, method)` pair the VM looks it up by, resolving to the method's prototype.
#[derive(Debug, Clone)]
pub struct MethodEntry {
    pub type_name: String,
    pub method: String,
    pub proto: u32,
}

/// The compiled layout of one `List<packed>` element type (P-PACK 2.4) — the pure-data form of a
/// `lang_object::PackedSchema`, referenced by index from [`Op::PackedListNew`]. Shapes and nested
/// schemas are held by **index** (into [`Module::shapes`] / [`Module::packed_schemas`]) so the
/// module stays plain data; the VM resolves these to `Rc`-handles once at load.
#[derive(Debug, Clone, PartialEq)]
pub struct PackedSchemaDef {
    /// The element type's shape, an index into [`Module::shapes`] — the same entry `MakeStruct`
    /// uses for that type, so a materialized element shares shape identity with a constructed one.
    pub shape: u32,
    /// One entry per field, in slot (declared) order.
    pub fields: Vec<PackedFieldDef>,
    /// Bytes per element (the per-element stride into the flat byte buffer; P-PACK 3.2b — an `f32`
    /// field is 4 bytes, the other primitives 8).
    pub byte_size: u32,
    /// Whether the list is stored column-major (`@packed(layout: column)`, P-SIMD C2). Pure-data
    /// mirror of `lang_object::PackedSchema::column`.
    pub column: bool,
}

/// A compiled packed field's kind — the pure-data form of a `lang_object::PackedKind`. A nested
/// packed struct refers to its own schema by index into [`Module::packed_schemas`].
#[derive(Debug, Clone, PartialEq)]
pub enum PackedFieldDef {
    Int,
    Float,
    /// A 32-bit float field (P-PACK Phase 3).
    F32,
    Bool,
    Struct(u32),
}

/// A compiled module: the prototype table plus the object-model side tables. `protos[0]` is
/// the top-level program; the rest are functions, closures, and methods, referenced by
/// `MakeClosure`/`Call`/the method table via their index. `shapes` is the layout table
/// (referenced by index from `MakeStruct`/`MakeEnum`); `methods` is the instance-method
/// dispatch table.
#[derive(Debug, Clone)]
pub struct Module {
    pub protos: Vec<Chunk>,
    pub shapes: Vec<Shape>,
    /// The packed-list element layouts (P-PACK 2.4), referenced by index from
    /// [`Op::PackedListNew`]. Empty for a program with no `List<packed>` literal.
    pub packed_schemas: Vec<PackedSchemaDef>,
    /// `map(...)` call sites whose result element type is packed (P-PACK 2.6 category B): the call's
    /// `Span` paired with the index (into [`Self::packed_schemas`]) of the result element layout. The
    /// VM's `map` builtin looks up its call span here to build a flat result. Empty for a program with
    /// no such `map`.
    pub map_packed_sites: Vec<(Span, u32)>,
    pub methods: Vec<MethodEntry>,
    /// `(type_name, proto)` for each class with a `destruct` block — the runtime-invoked
    /// destructor, compiled like a parameterless method (receiver in register 0). The VM runs
    /// it when the last reference to an instance of that type drops.
    pub destructors: Vec<(String, u32)>,
    /// `(type_name, field_name, proto)` for each field declared with a default (`x: T = expr`,
    /// object-model slice 5). Each `proto` is a parameterless value thunk compiled in global scope
    /// (empty upvalues — types are top-level, so a default resolves globals only). `MakeStruct` runs
    /// it to fill the field when a literal omits it, mirroring the tree-walker's `TypeDef`
    /// field-default thunks so both backends construct an identical instance.
    pub field_defaults: Vec<(String, String, u32)>,
    /// Type names that `@derive(Comparable)` without a hand-written `compare` method — the VM
    /// gives their instances structural field-wise ordering for `< <= > >=`.
    pub comparable_derives: Vec<String>,
    /// Type names that `@derive(ToJson)` without a hand-written `to_json` method — the VM
    /// synthesizes a structural JSON serializer for `o.to_json()`.
    pub tojson_derives: Vec<String>,
    /// Type names whose value, when destroyed, can run *some* `destruct` block — its own or a
    /// transitively-owned field / variant-payload / collection element (the checker's
    /// destruct-reachability fixpoint). The VM's **container-before-contained field-walk gate**
    /// (Phase 4.3, spec §4): an object/enum whose name is absent here owns no destructor in its
    /// subtree and frees on the plain-release fast path; one that is present is walked
    /// container-first, releasing each child recursively so contained destructors fire in declared
    /// order. Includes every type with its own `destruct` (the fixpoint seeds with them).
    pub destruct_reachable: Vec<String>,
    /// The number of inline-cache slots the program's cacheable call sites were assigned (one per
    /// `LoadField`/`CallMethod`). The VM allocates a per-run side array of this length, indexed by
    /// each op's `cache` field, to memoize the last receiver shape's field-slot / method prototype
    /// — turning the repeated linear field scan / `(type, method)` hashmap lookup into a pointer
    /// compare on a monomorphic call site. A pure optimization; zero if the program has no such
    /// sites.
    pub cache_slots: u32,
    /// The shared reflection artifact: the attribute manifest plus the registry of every declared
    /// type's reflectable shape. Built from the AST by [`lang_ast::reflect::build`] — the *same*
    /// pure builder the tree-walker uses — so reflection is identical across backends by
    /// construction. A build artifact for tooling and runtime reflection (pass 2); the rest of the
    /// runtime ignores it.
    pub reflection: lang_ast::reflect::ReflectionInfo,
    /// The interned reflected element types (runtime type-argument reflection, R1), referenced by
    /// index from [`Op::MakeList`]'s `reflect`. A list literal whose element type the checker resolved
    /// gets that [`TypeRepr`] interned here; the VM stamps a fresh `Rc` of it onto the built list so
    /// `type_of` recovers the element type after a `dyn` launder. Held as a module table (rather than
    /// inline on the op) so the op stays `Copy`-cheap and the `TypeRepr` — which is `Send` — keeps the
    /// module shareable across isolate threads. Empty for a program with no tagged list literal.
    pub type_reprs: Vec<lang_ast::reflect::TypeRepr>,
    /// The interned instruction name table (P-VMT-OPSZ): every [`NameId`] in an op indexes here.
    /// Deduped module-wide by the compiler, so a name used at N sites is stored once. Holds field /
    /// method / global / type names, ext-call module+func, and `match`-literal strings; the VM
    /// resolves an id to `&str` only at the cold lookup sites, the disassembler for readable output.
    pub names: Vec<String>,
    /// The global slot table (P-VMT-GSLOT): `global_names[i]` is the name of the global in slot `i`.
    /// Its length is the number of slots the VM's per-run globals vector needs. The slot **index** is
    /// what `LoadGlobal`/`StoreGlobal`/`TakeGlobal` carry ([`GlobalId`]); the name here is only for
    /// the unbound-global diagnostic and disassembly, never the hot path.
    pub global_names: Vec<String>,
}

impl Module {
    /// The top-level program prototype (always present).
    pub fn main(&self) -> &Chunk {
        &self.protos[0]
    }

    /// Resolve an interned [`NameId`] to its string (P-VMT-OPSZ). Ids are minted by the compiler
    /// against this same table, so the index is always in range.
    pub fn name(&self, id: NameId) -> &str {
        &self.names[id.0 as usize]
    }

    /// Resolve a [`GlobalId`] to the global's name (P-VMT-GSLOT) — for the unbound-global diagnostic
    /// and disassembly only.
    pub fn global_name(&self, id: GlobalId) -> &str {
        &self.global_names[id.0 as usize]
    }

    /// The data attributes (`#[...]`) attached to `target`, in source order — the manifest query
    /// tooling uses to discover, e.g., every type tagged `#[Entity]`.
    pub fn attributes_for<'a>(
        &'a self,
        target: &'a str,
    ) -> impl Iterator<Item = &'a lang_ast::reflect::AttributeRecord> {
        self.reflection.attributes_for(target)
    }

    /// Render the whole module as stable disassembly: the shape and method tables (when
    /// present), then the top-level program followed by each numbered function prototype.
    pub fn disassemble(&self) -> String {
        let mut out = String::new();
        if !self.shapes.is_empty() {
            out.push_str("shapes:\n");
            for (i, shape) in self.shapes.iter().enumerate() {
                let _ = match &shape.variant {
                    Some(variant) => writeln!(
                        out,
                        "  s{i} = enum {}.{variant}({})",
                        shape.name,
                        shape.fields.join(", ")
                    ),
                    None => writeln!(
                        out,
                        "  s{i} = {:?} {}{{{}}}",
                        shape.kind,
                        shape.name,
                        shape.fields.join(", ")
                    ),
                };
            }
        }
        if !self.packed_schemas.is_empty() {
            out.push_str("packed schemas:\n");
            for (i, schema) in self.packed_schemas.iter().enumerate() {
                let fields: Vec<String> = schema
                    .fields
                    .iter()
                    .map(|f| match f {
                        PackedFieldDef::Int => "int".to_string(),
                        PackedFieldDef::Float => "float".to_string(),
                        PackedFieldDef::F32 => "f32".to_string(),
                        PackedFieldDef::Bool => "bool".to_string(),
                        PackedFieldDef::Struct(idx) => format!("packed{idx}"),
                    })
                    .collect();
                let _ = writeln!(
                    out,
                    "  packed{i} = s{} [{}] ({} bytes)",
                    schema.shape,
                    fields.join(", "),
                    schema.byte_size
                );
            }
        }
        if !self.methods.is_empty() {
            out.push_str("methods:\n");
            for m in &self.methods {
                let _ = writeln!(out, "  {}.{} -> proto {}", m.type_name, m.method, m.proto);
            }
        }
        if !self.destructors.is_empty() {
            out.push_str("destructors:\n");
            for (type_name, proto) in &self.destructors {
                let _ = writeln!(out, "  {type_name} -> proto {proto}");
            }
        }
        for (i, proto) in self.protos.iter().enumerate() {
            if i == 0 {
                out.push_str("=== main ===\n");
            } else {
                let _ = writeln!(out, "=== proto {i} ===");
            }
            out.push_str(&proto.disassemble(&self.names, &self.global_names));
        }
        out
    }
}

fn const_repr(c: &Const) -> String {
    match c {
        Const::Unit => "unit".to_string(),
        Const::Bool(b) => b.to_string(),
        Const::Int(i) => i.to_string(),
        Const::Float(f) => format!("{f:?}"),
        Const::F32(f) => format!("{f:?}f32"),
        Const::Str(s) => format!("{s:?}"),
        Const::NativeModule(name) => format!("module {name}"),
    }
}

fn op_repr(
    op: &Op,
    diagnostics: &[Diagnostic],
    names: &[String],
    global_names: &[String],
) -> String {
    // Resolve an interned name id to its string for readable disassembly (P-VMT-OPSZ).
    let n = |id: &NameId| names[id.0 as usize].as_str();
    // Resolve a global slot to its name (P-VMT-GSLOT).
    let g = |id: &GlobalId| global_names[id.0 as usize].as_str();
    match op {
        Op::LoadConst { dst, k } => format!("LoadConst   r{dst} <- k{k}"),
        Op::Move { dst, src } => format!("Move        r{dst} <- r{src}"),
        Op::LoadGlobal { dst, global, .. } => format!("LoadGlobal  r{dst} <- {:?}", g(global)),
        Op::StoreGlobal { global, src } => format!("StoreGlobal {:?} <- r{src}", g(global)),
        Op::TakeGlobal { dst, global, .. } => {
            format!("TakeGlobal  r{dst} <- take({:?})", g(global))
        }
        Op::Drop { reg, relevant } => {
            let tag = if *relevant { " ~destruct" } else { "" };
            format!("Drop        r{reg}{tag}")
        }
        Op::ConcatInPlace { dst, lhs, rhs, .. } => format!("ConcatIP    r{dst} <- r{lhs} ~ r{rhs}"),
        Op::MakeClosure {
            dst,
            proto,
            captures,
        } => format!(
            "MakeClosure r{dst} <- proto {proto} [{} captures]",
            captures.len()
        ),
        Op::LoadNativeFn { dst, func } => format!("LoadNativeFn r{dst} <- {}", func.name()),
        Op::MakeCell { dst, src } => format!("MakeCell    r{dst} <- cell(r{src})"),
        Op::CellGet { dst, cell } => format!("CellGet     r{dst} <- *r{cell}"),
        Op::CellSet { cell, src } => format!("CellSet     *r{cell} <- r{src}"),
        Op::UpvalueGet { dst, index } => format!("UpvalueGet  r{dst} <- *upvalue[{index}]"),
        Op::UpvalueSet { index, src } => format!("UpvalueSet  *upvalue[{index}] <- r{src}"),
        Op::MakeList {
            dst,
            items,
            reflect,
        } => {
            let items: Vec<String> = items.iter().map(|r| format!("r{r}")).collect();
            let tag = match reflect {
                Some(idx) => format!("  ; reflect #{idx}"),
                None => String::new(),
            };
            format!("MakeList    r{dst} <- [{}]{tag}", items.join(", "))
        }
        Op::PackedListNew { dst, schema } => {
            format!("PackedListNew r{dst} <- [] packed{schema}")
        }
        Op::PackedListPush {
            dst, list, value, ..
        } => format!("PackedListPush r{dst} <- push(r{list}, r{value})"),
        Op::MakeTuple { dst, items } => {
            let items: Vec<String> = items.iter().map(|r| format!("r{r}")).collect();
            format!("MakeTuple   r{dst} <- ({})", items.join(", "))
        }
        Op::TupleIndex {
            dst,
            receiver,
            index,
            ..
        } => format!("TupleIndex  r{dst} <- r{receiver}.{index}"),
        Op::MakeRange {
            dst,
            start,
            end,
            inclusive,
            ..
        } => {
            let op = if *inclusive { "..=" } else { ".." };
            format!("MakeRange   r{dst} <- r{start}{op}r{end}")
        }
        Op::MakeMap {
            dst,
            entries,
            reflect,
        } => {
            let entries: Vec<String> = entries.iter().map(|(k, v)| format!("r{k}: r{v}")).collect();
            let tag = match reflect {
                Some(idx) => format!("  ; reflect #{idx}"),
                None => String::new(),
            };
            format!("MakeMap     r{dst} <- {{{}}}{tag}", entries.join(", "))
        }
        Op::RequireMapKey { reg, .. } => format!("RequireMapKey r{reg}"),
        Op::IterSnapshot { dst, src, .. } => format!("IterSnapshot r{dst} <- r{src}"),
        Op::ListLen { dst, src, .. } => format!("ListLen     r{dst} <- len r{src}"),
        Op::ListGet { dst, list, index } => format!("ListGet     r{dst} <- r{list}[r{index}]"),
        Op::IterForNext {
            iter, elem, has, ..
        } => format!("IterForNext r{elem}, r{has} <- next r{iter}"),
        Op::CallBuiltin {
            dst, builtin, args, ..
        } => {
            let args: Vec<String> = args.iter().map(|r| format!("r{r}")).collect();
            format!(
                "CallBuiltin r{dst} <- {}({})",
                builtin.name(),
                args.join(", ")
            )
        }
        Op::CallMethod {
            dst,
            recv,
            method,
            args,
            reuse,
            ..
        } => {
            let args: Vec<String> = args.iter().map(|r| format!("r{r}")).collect();
            let marker = if *reuse { " [reuse]" } else { "" };
            format!(
                "CallMethod  r{dst} <- r{recv}.{}({}){marker}",
                n(method),
                args.join(", ")
            )
        }
        Op::Index {
            dst, recv, index, ..
        } => format!("Index       r{dst} <- r{recv}[r{index}]"),
        Op::IndexField {
            dst,
            recv,
            index,
            field,
            ..
        } => format!("IndexField  r{dst} <- r{recv}[r{index}].{}", n(field)),
        Op::MakeStruct {
            dst,
            shape,
            named,
            spread,
            ..
        } => {
            let mut parts: Vec<String> = named
                .iter()
                .map(|(slot, r)| format!("s{slot}=r{r}"))
                .collect();
            if let Some(base) = spread {
                parts.push(format!("..r{base}"));
            }
            format!(
                "MakeStruct  r{dst} <- shape s{shape} {{{}}}",
                parts.join(", ")
            )
        }
        Op::MakeStructInPlace {
            dst,
            shape,
            named,
            base,
            check,
            ..
        } => {
            let mut parts: Vec<String> = named
                .iter()
                .map(|(slot, r)| format!("s{slot}=r{r}"))
                .collect();
            parts.push(format!("..r{base}"));
            let tag = match check {
                ReuseCheck::Runtime => "rt",
                ReuseCheck::Static => "static",
            };
            format!(
                "MakeRecIP   r{dst} <- shape s{shape} {{{}}} [{tag}]",
                parts.join(", ")
            )
        }
        Op::MakeOpaque {
            dst,
            type_name,
            keys,
            spread,
        } => {
            let mut parts: Vec<String> = keys
                .iter()
                .map(|(k, r)| format!("{:?}=r{r}", n(k)))
                .collect();
            if let Some(base) = spread {
                parts.push(format!("..r{base}"));
            }
            format!(
                "MakeOpaque  r{dst} <- {} {{{}}}",
                n(type_name),
                parts.join(", ")
            )
        }
        Op::MakeEnum {
            dst, shape, args, ..
        } => {
            let args: Vec<String> = args.iter().map(|r| format!("r{r}")).collect();
            format!("MakeEnum    r{dst} <- shape s{shape}({})", args.join(", "))
        }
        Op::EnumFromStr {
            dst,
            arg,
            enum_name,
            panic,
            ..
        } => {
            let kind = if *panic { "from" } else { "try_from" };
            format!("EnumFromStr r{dst} <- {}.{kind}(r{arg})", n(enum_name))
        }
        Op::LoadField {
            dst, obj, field, ..
        } => format!("LoadField   r{dst} <- r{obj}.{}", n(field)),
        Op::SetField {
            dst,
            obj,
            field,
            value,
            reuse,
            ..
        } => {
            let marker = if *reuse { " [reuse]" } else { "" };
            format!(
                "SetField    r{dst} <- r{obj}.{} = r{value}{marker}",
                n(field)
            )
        }
        Op::NextId { dst } => format!("NextId      r{dst}"),
        Op::Panic { msg, .. } => format!("Panic       r{msg}"),
        Op::TryUnwrap {
            dst, src, on_error, ..
        } => {
            if on_error.is_empty() {
                format!("TryUnwrap   r{dst} <- r{src}?")
            } else {
                let drops = on_error
                    .iter()
                    .map(|(r, relevant)| {
                        if *relevant {
                            format!("r{r}~")
                        } else {
                            format!("r{r}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                format!("TryUnwrap   r{dst} <- r{src}? !drop[{drops}]")
            }
        }
        Op::Coalesce {
            dst, src, fallback, ..
        } => format!("Coalesce    r{dst} <- r{src} ?? -> {fallback}"),
        Op::Narrow {
            dst, src, target, ..
        } => format!("Narrow      r{dst} <- r{src}.as<{target:?}>()"),
        Op::AttributesOf { dst, type_name } => {
            format!("AttributesOf r{dst} <- attributes_of::<{}>()", n(type_name))
        }
        Op::RolesOf { dst } => format!("RolesOf     r{dst} <- roles_of()"),
        Op::TypeOf { dst, src } => format!("TypeOf      r{dst} <- type_of(r{src})"),
        Op::FromBytes {
            dst, src, schema, ..
        } => {
            format!("FromBytes   r{dst} <- from_bytes(r{src}, schema {schema})")
        }
        Op::TypeOfStatic { dst, repr } => format!("TypeOfStatic r{dst} <- {repr:?}"),
        Op::TypeValue { dst, name } => format!("TypeValue   r{dst} <- type {}", n(name)),
        Op::Invoke {
            dst,
            recv,
            name,
            args,
            ..
        } => format!("Invoke      r{dst} <- invoke(r{recv}, r{name}, r{args})"),
        Op::ExtCall {
            dst,
            module,
            func,
            args,
            ..
        } => {
            let args: Vec<String> = args.iter().map(|r| format!("r{r}")).collect();
            format!(
                "ExtCall     r{dst} <- {}.{}::<T>({})",
                n(module),
                n(func),
                args.join(", ")
            )
        }
        Op::IsType { dst, src, target } => {
            format!("IsType      r{dst} <- r{src} is {target:?}")
        }
        Op::MakeFuture { dst, src } => {
            format!("MakeFuture  r{dst} <- future r{src}")
        }
        Op::RunFuture { dst, src, .. } => {
            format!("RunFuture   r{dst} <- await r{src}")
        }
        Op::PollFuture { dst, src, .. } => {
            format!("PollFuture  r{dst} <- poll r{src}")
        }
        Op::LoadPending { dst } => {
            format!("LoadPending r{dst} <- pending")
        }
        Op::ScopeBegin => "ScopeBegin".to_string(),
        Op::SpawnIsolate {
            dst, callee, args, ..
        } => {
            let args = args
                .iter()
                .map(|r| format!("r{r}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("SpawnIsolate r{dst} <- isolate r{callee}({args})")
        }
        Op::Spawn { dst, src, .. } => {
            format!("Spawn       r{dst} <- spawn r{src}")
        }
        Op::ScopeEnd { .. } => "ScopeEnd".to_string(),
        Op::MakeChannel { dst, capacity, .. } => {
            format!("MakeChannel r{dst} <- channel(r{capacity})")
        }
        Op::MakeGen { dst, src } => {
            format!("MakeGen     r{dst} <- gen r{src}")
        }
        Op::MatchInt { src, value, fail } => {
            format!("MatchInt    r{src} == {value} else -> {fail}")
        }
        Op::MatchStr { src, value, fail } => {
            format!("MatchStr    r{src} == {:?} else -> {fail}", n(value))
        }
        Op::MatchBool { src, value, fail } => {
            format!("MatchBool   r{src} == {value} else -> {fail}")
        }
        Op::MatchVariant {
            src,
            type_name,
            variant,
            arity,
            fail,
        } => {
            let qualifier = match type_name {
                Some(name) => format!("{}.", n(name)),
                None => String::new(),
            };
            format!(
                "MatchVariant r{src} is {qualifier}{}/{arity} else -> {fail}",
                n(variant)
            )
        }
        Op::MatchTuple { src, arity, fail } => {
            format!("MatchTuple  r{src} is tuple/{arity} else -> {fail}")
        }
        Op::ExtractField { dst, src, index } => format!("ExtractField r{dst} <- r{src}.{index}"),
        Op::MatchFail { src, .. } => format!("MatchFail   r{src}"),
        Op::Call {
            dst, callee, args, ..
        } => {
            let args: Vec<String> = args.iter().map(|r| format!("r{r}")).collect();
            format!("Call        r{dst} <- r{callee}({})", args.join(", "))
        }
        Op::Return { src } => format!("Return      r{src}"),
        Op::Unary { op, dst, src, .. } => format!("Unary       r{dst} <- {} r{src}", op.symbol()),
        Op::MaskWidth {
            dst,
            src,
            signed,
            bits,
        } => format!(
            "MaskWidth   r{dst} <- mask_{}{bits} r{src}",
            if *signed { 'i' } else { 'u' }
        ),
        Op::Binary { op, dst, a, b, .. } => {
            format!("Binary      r{dst} <- r{a} {} r{b}", op.symbol())
        }
        Op::WideInt {
            op,
            dst,
            a,
            b,
            signed,
            bits,
            ..
        } => format!(
            "WideInt     r{dst} <- r{a} {} r{b} {}{bits}",
            op.symbol(),
            if *signed { 'i' } else { 'u' },
        ),
        Op::WidthIntMethod {
            dst,
            recv,
            method,
            arg,
            bits,
            ..
        } => match arg {
            Some(a) => format!("WidthIntMethod r{dst} <- r{recv}.{method:?}(r{a}) w{bits}"),
            None => format!("WidthIntMethod r{dst} <- r{recv}.{method:?}() w{bits}"),
        },
        Op::RequireBool { reg, side, op, .. } => {
            format!("RequireBool r{reg} ({} {side:?})", op.symbol())
        }
        Op::RequireCondBool { reg, .. } => format!("RequireCondBool r{reg} (if)"),
        Op::Jump { target } => format!("Jump        -> {target}"),
        Op::JumpIfTrue { reg, target } => format!("JumpIfTrue  r{reg} -> {target}"),
        Op::JumpIfFalse { reg, target } => format!("JumpIfFalse r{reg} -> {target}"),
        Op::CondBranch { reg, target, .. } => format!("CondBranch  r{reg} unless -> {target}"),
        Op::Echo { reg } => format!("Echo        r{reg}"),
        Op::Stringify { dst, src, .. } => format!("Stringify   r{dst} <- display(r{src})"),
        Op::BuildString { dst, parts } => {
            let rendered = parts
                .iter()
                .map(|p| match p {
                    StrPart::Literal(k) => format!("k{k}"),
                    StrPart::Hole(r) => format!("display(r{r})"),
                })
                .collect::<Vec<_>>()
                .join(" ~ ");
            format!("BuildString r{dst} <- {rendered}")
        }
        Op::Raise { idx } => {
            let code = diagnostics
                .get(*idx as usize)
                .map(|d| d.code.to_string())
                .unwrap_or_else(|| "?".to_string());
            format!("Raise       {code} (d{idx})")
        }
        Op::Halt => "Halt".to_string(),
    }
}
