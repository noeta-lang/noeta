//! The native-extension ABI (P-NATIVE): the contract a crate implements to register native
//! modules and first-class types into the language, plus the dep-free primitives both backends
//! and the front-end share.
//!
//! Split out of `noeta-stdlib` so the contract does not drag core's batteries (crypto/UUID/JSON):
//! a third-party extension — and internal mid-end crates like `noeta-ir` — depend on this lean
//! crate, while `noeta-stdlib` re-exports it (`pub use noeta_ext_abi::*`) and adds the concrete
//! `std` modules on top (the `core`/`std` relationship). See `plans/native-abi/README.md`.

/// The extension **ABI version** — bumped on any change to the registration/dispatch contract
/// (`Extension`, `ExtModule`/`ExtType`/`ExtFn` shapes, `NativeValue`/`NativeOut` marshalling,
/// `NativeCtx`, the `NOETA_EXTENSIONS` symbol convention). Today every extension is compiled
/// from source against the exact toolchain (the composed build's `[patch]` unification), so an
/// ABI break is a compile error and this constant is *recorded*, not yet *checked* — it exists
/// so the future dynamically-loaded-extension path has a handshake to refuse a mismatch with,
/// instead of undefined behavior through a stale `TypeId`/layout (audit-2 F10).
///
/// **Bump it freely.** "Any change" means any change — an added registration field, a new capability
/// method with a default, a new `ExtType`, not only something that stops existing code compiling.
/// Pre-1.0 the cost of a bump is a digit, while the cost of a *missed* bump is a version number that
/// silently under-describes the contract, which is the one thing this constant exists to prevent. Do
/// not spend a paragraph deciding whether an addition qualifies; if you touched the contract, bump.
/// Entries 2–5 below were written under a narrower source-break-only test and read as though the
/// question were finely balanced — it is not.
///
/// **2** — [`registry::ExtFn`] gained the required `param_names` field (so a `name:` label at a
/// call site binds against a native signature). A registration table written for ABI 1 omits it
/// and no longer compiles; the composed build reports that as an ABI mismatch naming the package
/// it belongs to (`compose::compose_failure`) rather than as a bare `rustc` error.
///
/// **3** — [`registry::TypeRecipe::Fielded`]'s `fields` became `Vec<FieldRecipe>` (from
/// `Vec<(String, TypeRecipe)>`) so each field carries its [`registry::FieldDefault`] — what an
/// omitted input field means. A `typed_dispatch` that walks a struct recipe destructures the tuple
/// and no longer compiles; there is no silent behavior change, and `..DEFAULTS` cannot cover it
/// (the change is inside a matched enum variant, not an added registration field).
///
/// **4** — [`registry::DirectiveCtx`] gained the required `fields` field (the decorated
/// declaration's shape, so an `expand` hook can generate from a struct's members and not only from
/// its name). A hook itself takes `&DirectiveCtx` and is unaffected, but code that *constructs* one
/// — in practice a package's own tests — no longer compiles without it.
///
/// **5** — [`host::Network::net_stream_open`] returns [`stream::StreamHead`] (the stream id plus the
/// response status/headers/url) instead of a bare `u64`, so a streamed non-2xx is observable at all.
/// A host that *overrides* it, and an `ExternIo` that calls it through `&mut dyn Host`, both need the
/// new type. It is deliberately not the additive shape (a separate `net_stream_status(id)` accessor
/// with a default), because a default there would let a streaming host silently report `200` for a
/// real `429` — the same invisible failure the change exists to remove.
///
/// **6** — the streaming-body surface itself, counted rather than waved through: the `Network`
/// capability's `net_stream_*` methods, the `SseSink` send/close pair, and the `Frame`/`Framing`/
/// `FrameStream` registrations. Every one is additive and nothing written for ABI 5 stops compiling
/// — and under the rule above that is beside the point. They changed the contract, so they get a
/// number. Retroactive, because they landed while the narrower test was in force.
/// **7** — the Ring 1 `bytes` surface grew its element reads: [`ring1::bytes_index`] (the `b[i]`
/// body) and [`ring1::bytes_slice`] (the `b.slice(start, end?)` body), the shared semantics both
/// backends call in for a primitive `bytes` previously did not have at all — it could be produced
/// and measured but never taken apart, which made every in-language decoder inexpressible. Purely
/// additive: nothing written for ABI 6 stops compiling. Counted anyway, per the rule above, because
/// it widens what the ABI crate promises an extension (and a backend) can rely on.
///
/// **8** — the subprocess doors got their recoverable and awaitable twins, and the [`host::Os`]
/// capability was **re-rooted on them**. [`host::Os::os_try_spawn`] and
/// [`host::Os::os_proc_try_write_stdin`] are the new required primitives (returning a classified
/// [`os::OsError`]); the aborting `os_spawn`/`os_proc_write_stdin` became *defaults* derived from
/// them, so the two doors of each pair cannot drift. [`host::Os::os_proc_read_spawn`] is additive
/// (a default over the blocking reads). Plus the registrations: `os.try_spawn`,
/// `Process.try_write`, `Process.read_line_async`/`read_err_line_async`/`read_async`, the
/// [`os::OsError`] extern type, and the [`os::ProcRead`]/[`os::ProcReadIo`] seam types.
///
/// This one is a genuine break rather than a counted-anyway addition: a host that implemented
/// `os_spawn` and `os_proc_write_stdin` still compiles those methods, but now satisfies neither
/// required primitive, so the composed build reports an ABI mismatch naming the package rather than
/// a bare trait error. That is the intended failure — the alternative (a defaulted `os_try_spawn`
/// that classifies by sniffing the aborting door's message string) would make every third-party
/// host silently answer `other` for a missing binary, which is exactly the information these doors
/// exist to deliver.
///
/// **9** — `std.tracing` grew the **active-span annotators**: `tracing.set_attribute`,
/// `tracing.add_event`, `tracing.add_event_with`, and `tracing.record_error`, ctx functions that
/// apply the `Span` mutations to the span the caller is already *inside* rather than to a handle it
/// holds. `Span` gained the matching `add_event_with(name, attrs)` (and with it `deep_marshal`, so
/// the map argument arrives whole), closing the one place the `Tracing` capability could carry
/// event attributes but no surface could produce them. Purely additive registration (no
/// [`host::Host`] method changed, nothing written for ABI 8 stops compiling), counted anyway under
/// the rule above because it widens the registry surface an extension and a backend can rely on.
/// Deliberately not a `current_span() -> ?Span`: that would hand a caller `.end()` on a span it
/// never opened — see the `std.tracing` module header.
///
/// **10** — the enum-construction arc: [`registry::TypeRecipe`] gained an `Enum` form (with
/// [`registry::VariantRecipe`]/[`registry::VariantTag`]), so an enum-typed field decodes from the
/// wire values its own JSON Schema advertises, and [`registry::NativeOut::Variant`] gained the
/// required `has_validator` field that makes a decoded case honor the same `Validate` door contract a
/// decoded struct already did. `TypeRecipe` is named in this list twice over now, so the form is
/// squarely a bump; the `NativeOut` field is a genuine source break for an extension that returns an
/// enum (add `has_validator: false` — a dispatch's own return value is not untrusted input crossing a
/// door). Also `json.parse`/`try_parse`/`decode_typed` report a new
/// [`crate::registry::TypeRecipe`]-driven failure kind, `unknown_variant`, distinct from `mismatch`.
///
/// **11** — `regex.compile` got its recoverable twin, `regex.try_compile(pattern):
/// Result<Pattern, string>`, and `std.regex` was re-rooted on it the way [`host::Os`] was in ABI 8:
/// `regex::try_compile` is the primitive and the aborting `regex::compile` is derived from it, so
/// the two doors cannot report different things about the same pattern. Purely additive
/// registration (nothing written for ABI 10 stops compiling), counted anyway under the rule above
/// because it widens the registry surface. The `Err` side is a plain `string` rather than a
/// classified extern error: a pattern has exactly one way to be invalid, so a `kind()` would be a
/// constant, and the engine's own caret-carrying diagnostic is the whole value.
///
/// **12** — [`registry::HiddenArg::Table`] carries a [`registry::TypeArgIndex`] instead of a bare
/// `u32`. A genuine source break for anything that matched the variant and used its payload as an
/// integer (call `.get()`), and the point of the break: an index into the program's type-argument
/// table is the one integer in the checker→lowering vocabulary a LIVE session must renumber, and it
/// sat beside three `u32`s — hidden-slot ordinals and a type parameter's declaration position —
/// that must NOT be. Remapping one carrier and not another is silent (the program keeps running
/// with the wrong type argument), so the distinction moved into the type, where
/// `noeta_check::SITE_POLICIES`'s gate can read it.
///
/// **13** — [`command::CommandCtx::serve_parallel`] takes the command's own
/// [`command::EntryCall`] (`serve_parallel(file, entry, host, port, workers)`) instead of
/// rebuilding one from `port`/`host`. A source break only for a driver that *overrides* the
/// method; the default body is now a plain forward to `run_file`. The break is the point: the
/// serve entry call was declared three times — in `SERVE_COMMAND`, in this trait's default, and
/// in the CLI's multi-core path — so its signature was three edits in two crates, and the copies
/// were kept in step by a comment (audit-10). One declaration, passed down.
///
/// **14** — [`host::RealP2pConfig`] gained `data_dir`, an exact directory for the `para.p2p` node's
/// persistent identity and store, beside the `app_id` that could name only an app namespace. A host
/// could say which *app* a node belonged to but never which *node*, which left the multi-tenant
/// case inexpressible: a server running one isolate per signed-in user, each user with their own
/// p2p identity and store. `RealHost::with_p2p_dir` fills it. Purely additive — the struct derives
/// `Default` and nothing written for ABI 13 stops compiling — and counted anyway, per the rule
/// above, because it widens what a host promises the extension side it can read.
///
/// The field's own doc carries the precedence this seam settled on, and that is the half worth
/// repeating here: an explicitly named directory, whether from the host or from a program opening a
/// node, beats `$NOETA_P2P_DIR`, which steers only the node nobody named. The ordering is a safety
/// property rather than a tidiness one, and it is deliberately the opposite of the usual
/// env-overrides-config reflex — a process-wide variable outranking a per-tenant directory would
/// collapse every signed-in user onto one identity and store, silently mixing one user's data into
/// another's.
///
/// **15** — [`registry::ExtTrait`], [`registry::ExtEnum`] and [`registry::ExtFielded`] each gained a
/// `doc` (the declaration's own prose) and a `docs` (its per-member table, the field
/// [`registry::ExtModule`] and [`registry::ExtType`] have carried since the docs-browser arc). A
/// source break only for a literal that named every field rather than spreading `..DEFAULTS`; add
/// nothing and the defaults are empty. The point of the addition is that until it existed a native
/// trait had nowhere to say what an implementor promises, and the API reference walked
/// `modules()`/`types()` alone — so a published package's docs named none of its traits, enums,
/// classes or structs. Assembly also now holds a trait's `namespace` to its unit's root, the rule
/// the other four nominal hooks were already held to and traits were not.
///
/// **16** — [`command::ArgKind`] gained `OptInt`, `OptFloat`, `Strings` (a repeatable flag) and
/// `PathDefault`, with [`command::ParsedArgs::get_float`] and [`command::ParsedArgs::strs`] to read
/// the two new shapes. A source break only for code that *matches* `ArgKind` exhaustively — the
/// CLI does, an extension declaring commands does not. The reason they exist is that `noeta test`,
/// `noeta bench` and `noeta doc` stopped being clap variants the binary hardcodes and became
/// ordinary [`command::ExtCommand`]s std contributes: an argument list rich enough to say
/// `--jobs <N>` (unset ≠ zero), `--max-regress <PCT>` (negative allowed), `--name a --name b`, and
/// a defaulted positional had to exist first. What that buys is replaceability — a
/// `[trust.commands]` binding under one of those names now takes the whole verb over, flags and
/// help included, so a third-party test runner can *be* `noeta test`.
/// **17** — [`command::ArgSpec`] gained `short` (a one-letter alias, `-j` for `--jobs`) and, with it,
/// an `ArgSpec::DEFAULTS` to spread. A **source break** for every existing `ArgSpec` literal, which
/// named all three fields: add `..ArgSpec::DEFAULTS` and nothing else changes. That break is the
/// reason the field waited — ABI 16 shipped `test`/`bench`/`doc` as declared commands *without* the
/// `-j` the clap derive had offered, because adding the field then would have broken every
/// out-of-tree literal in the same release that moved the verbs. Doing it as its own version keeps
/// the two diagnosable apart. A `short` on a positional kind is ignored rather than fatal, since a
/// package's slip should not abort the CLI for everyone who installed it.
///
/// **18** — a `class` decodes from JSON. [`registry::TypeRecipe::Fielded`] and
/// [`registry::NativeOut::Fielded`] are both now `Fielded`, and both carry a
/// [`registry::FieldedKind`]: the recipe says which kind to build, because a class interns a class
/// shape and keeps reference semantics while a struct interns a struct shape, and the backend
/// cannot re-derive that from the type's name. A **source break** for an extension that builds
/// either variant by name — rename it and add `kind` (`FieldedKind::Struct` preserves the old
/// meaning exactly).
///
/// The rule it implements: a decode is the *declaration's* own doing, since
/// `@derive(Deserialize<Json>)` is written inside the type like a method, so it reaches the whole
/// shape — private fields included — where a caller-side reflective door (`construct`, `fields_of`)
/// does not. Refusing a class left it serializable and never recoverable, because `Serialize`
/// writes every field regardless; the wire form existed with nothing able to read it back.
///
/// **19** — a field can leave the serialized shape. [`registry::FieldRecipe`] gained `skipped` and
/// [`registry::ExtAttribute`] gained `targets`; both default to the previous behavior, so the break
/// is source-only and only for code building either by literal (`..Default::default()`, or the new
/// `FieldRecipe::transient` / an empty `targets`). What they carry is `#[std.json.Transient]`: the
/// marker that takes a field out of the wire form in both directions, and the placement list that
/// makes writing it anywhere but a field a diagnostic instead of a no-op.
///
/// **20** — a blocking host leaf can be interrupted. [`Host`] gained [`Cancellable`] as an arm of
/// its union, so a run that can be cancelled hands its host the same flag-and-wake pair the
/// executor already gets; [`ErrorKind`] gained `Interrupted` and [`NetErrorKind`] the transport
/// spelling of it, for a leaf that ended its wait early; and [`CancelSignal`] bundles the flag and
/// the [`CancelWake`] into the one object a canceller passes around. A **source break** only for a
/// host that spells out the union it implements: the method defaults to a no-op, so
/// `impl Cancellable for MyHost {}` restores it, and a host with nothing that blocks unboundedly
/// wants exactly that. The rule it implements: ending a wait is not ending the work, so an
/// interruptible leaf **returns** `Interrupted` rather than being abandoned — teardown waits for
/// every blocking body that has started, and one that never returns turns a leaked thread into a
/// hung run.
///
/// **21** — the [`render_hint`] surface: [`render_hint::RenderHint`] — the structural map of the
/// unsigned 64-bit integers under a static type — and the walks that apply one,
/// [`render_hint::json_stringify`], [`render_hint::map_key_display`], [`render_hint::map_key_order`],
/// [`render_hint::unsigned_digits`] and [`render_hint::unsigned_order`]. Purely additive; nothing
/// written for ABI 20 stops compiling. A `u64` past bit 63 is a negative i64 word and the signedness
/// lives only in the static type, so writing one out correctly takes a description from the door —
/// and the hint belongs beside the one JSON encoder its walk delegates to, the [`MapKey`] it renders
/// and orders, and the [`NativeValue`] tree both backends marshal into. That an extension, and the
/// native seam itself, can now name it is the point: a native function that keeps a value for a
/// *later* serialization can keep its hint too.
///
/// **22** — [`map_key::packed_names::display_hinted`]: the display form of a `@packed` struct key,
/// with the slots a [`render_hint::RenderHint`] marks unsigned written as the `u64` they stand for.
/// Purely additive — [`map_key::packed_names::display`] is it with no hint, byte for byte. A packed
/// key stores its declared fields as flat words, so a `u64` field is indistinguishable from an `i64`
/// one inside the key; the ordering walk already read the hint's slots and the rendering walk did
/// not, which left a rendered map printing a key its own `keys()` printed differently.
pub const ABI_VERSION: u32 = 22;

pub mod args;
pub mod channel;
pub mod command;
pub mod ctx;
pub mod delegate;
pub mod executor;
pub mod extern_value;
pub mod host;
pub mod json_text;
pub mod map_key;
pub mod net;
pub mod os;
pub mod p2p;
pub mod registry;
pub mod render_hint;
pub mod ring1;
pub mod stream;
pub mod telemetry;

pub use command::{ArgKind, ArgSpec, CommandCtx, EntryArg, EntryCall, ExtCommand, ParsedArgs};
pub use ctx::{
    Cap, CtxDispatch, CtxError, CtxOut, CtxResult, ExtState, FutureTracing, HotReload, NativeCtx,
    PackedField, PackedView, Retained, Slot, TaskContext, capabilities, capability, ctx_arity,
};
pub use executor::{CancelSignal, CancelWake, Executor, ExternIo, FsIo, RealBody, SandboxExecutor};
pub use extern_value::{ExternBox, ExternValue};
pub use host::{
    Cancellable, Clock, Console, Entropy, Env, FileReader, FileSystem, Host, Ids, Network, Os, P2p,
    P2pProvider, ReadSource, RealP2pConfig, Rng, Stream, SyncStatus,
};
pub use map_key::{ExternKeyRef, MapKey, PackedKeyField};
pub use net::{
    AcceptIo, NetError, NetErrorKind, NetFetchIo, NetRequest, NetResponse, ReplyIo, Request,
};
pub use os::{ExecIo, ExecResult, Process};
pub use p2p::{P2pBackend, P2pBroker, P2pReceiveIo};
// The streaming-body surface (http-streaming arc). Deliberately NOT a glob: the module's own
// `Stream`-adjacent names would collide with `host::Stream` (the stdin/stdout/stderr enum), and
// the type here is `FrameStream` precisely so the two never have to be disambiguated.
pub use registry::{
    ArenaGetter, AssocDerivation, AttrTarget, BundleReceiver, ClassDispatch, ConstraintArity,
    ConstraintField, ConstraintLayout, CtxTypeDispatch, EnumBacking, ExtAssocType, ExtCapability,
    ExtClass, ExtEnum, ExtField, ExtFielded, ExtFn, ExtModule, ExtRoleTag, ExtStruct, ExtTier,
    ExtTierRunner, ExtTrait, ExtTraitMethod, ExtType, ExtTypeDirective, ExtVariant, Extension,
    FieldDefault, FieldRecipe, FieldedDispatch, FieldedKind, HiddenArg, JSON_STRINGIFY_HANDLER,
    ModuleDispatch, NativeOut, NativeValue, Nominal, NominalKind, NominalType, PackedConstraint,
    PackedLayoutKind, RetTy, Scalar, ScalarVec, SigType, TierRoot, TierRoots, TierRun, TierRunner,
    TierText, TraitDispatch, TypeArgIndex, TypeArgInfo, TypeDispatch, TypeRecipe, TypedDispatch,
    TypedTypeDispatch, VariantRecipe, VariantTag, VariantValue,
};
pub use render_hint::{
    RenderHint, json_stringify, map_key_display, map_key_order, unsigned_digits, unsigned_order,
};
pub use stream::{
    Frame, FrameDecoder, FrameStream, Framing, SseCloseIo, SseSendIo, SseSink, StreamRecvIo,
    Utf8Chunker,
};
// The Ring 1 bodies moved to `ring1` (audit-2 F8); the glob keeps every existing path
// (`noeta_ext_abi::Arg`, `noeta_stdlib::string_method`, ...) compiling unchanged. The shared
// argument guards stay namespaced (`noeta_ext_abi::args::want_str`) — dispatch modules import
// them explicitly, so a module-local extractor never shadows silently.
pub use ring1::*;
pub use telemetry::{
    AttrValue, DEFAULT_HISTOGRAM_BOUNDS, HistogramPoint, InstrumentId, InstrumentKind, LogRecord,
    Logging, MetricData, MetricPoints, MetricStore, MetricValue, Metrics, NumberPoint, Severity,
    SpanData, SpanEvent, SpanId, SpanKind, SpanStatus, SpanTracker, Temporality, TraceContext,
    Tracing,
};

/// Macro-expansion support — types the [`delegate_host!`] arms must name from the caller's crate
/// without the caller depending on our private deps (`compact_str` in the telemetry signatures).
/// Not API; never use directly.
#[doc(hidden)]
pub mod __private {
    pub use compact_str::CompactString;
}

/// The category of a stdlib misuse, mapped by each backend onto a `DiagnosticCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Wrong number of arguments.
    Arity,
    /// An argument was the wrong type.
    ArgType,
    /// An index/range argument fell outside the collection's bounds.
    Bounds,
    /// A name that does not exist (e.g. an unknown function on a native module).
    UnknownName,
    /// A Ring 2 IO operation failed (e.g. reading a path absent from the sandbox).
    Io,
    /// An unrecoverable runtime condition a dispatch raises deliberately (higher-order-abi H2) —
    /// an async deadlock, an empty `race`. Maps onto the language's panic diagnostic, exactly as
    /// the hand-written `Builtin` arms it replaces reported.
    Panic,
    /// A native-driven callback fixpoint failed to converge (higher-order-abi H5) — the reactive
    /// flush's runaway guard (an effect that keeps changing a signal it depends on). Maps onto
    /// the language's reactive-cycle diagnostic (E0045).
    ReactiveCycle,
    /// A deliberate program termination with this exit code (`os.exit(n)`, stdlib-gaps). NOT a
    /// diagnostic: each backend intercepts it at the dispatch boundary, halts cleanly (stdout
    /// kept, nothing printed), and surfaces the code as the run's exit code.
    Exit(i32),
    /// A blocking host operation gave up because the run it serves is being cancelled — see
    /// [`Cancellable`]. Distinct from [`ErrorKind::Io`] because it says nothing about the resource:
    /// a `read_line` that answers `none` means *end of stream*, and a cancelled read is not an end
    /// of stream, so a leaf that stopped early must be able to say which happened.
    ///
    /// Not a failure mode a program is expected to see. The safepoint the unwind passes through
    /// observes the cancellation flag and ends the run as cancelled, which is what a caller
    /// actually gets; this kind only survives that far in the window where a cancellation was
    /// already honored elsewhere, where it renders as the ordinary IO failure it is.
    Interrupted,
}

/// A stdlib misuse error. The `message` is rendered here so both backends report it
/// identically; the `kind` selects the diagnostic code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdError {
    pub kind: ErrorKind,
    pub message: String,
}

/// Build the canonical "wrong number of arguments" error. Public so the collection methods
/// (implemented per backend over their own value types) report misuse with text identical to
/// the string surface — keeping both backends' diagnostics in lockstep.
pub fn arity_error(method: &str, expected: usize, got: usize) -> StdError {
    StdError {
        kind: ErrorKind::Arity,
        message: format!("method `{method}` takes {expected} argument(s) but {got} were supplied"),
    }
}

/// Build the canonical "wrong argument type" error. `expected` is the type noun (`"string"`,
/// `"int"`, `"list of strings"`, ...); the article is chosen for readability. Public for the
/// same reason as [`arity_error`].
pub fn type_error(method: &str, expected: &str) -> StdError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!("method `{method}` expects {} argument", an(expected)),
    }
}

/// Build the canonical "cannot order" error for a method (`sorted`, `to_set`) over values that
/// are not mutually orderable (mixed kinds, or a non-orderable element). Maps to `E0007` like
/// other type misuse. Both `sorted` and set construction require a single orderable element type
/// so the result has a deterministic canonical order.
pub fn unorderable_error(method: &str) -> StdError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!(
            "method `{method}` requires values of a single orderable type (a primitive, or a \
             value kind — struct/enum — ordering structurally)"
        ),
    }
}

/// Build the canonical "slice out of bounds" error for `slice(start, end)` on a list of
/// length `len`. Public so both backends render the bounds error identically (→ `IndexOutOfBounds`).
pub fn slice_bounds_error(start: i64, end: i64, len: usize) -> StdError {
    StdError {
        kind: ErrorKind::Bounds,
        message: format!("slice [{start}..{end}] is out of bounds for list of length {len}"),
    }
}

/// The `vec` module's scalar Vec3 function names (P-PACK Phase 4.1), in surface order. A "Vec3" is
/// any `@packed`-or-plain struct value with exactly three `f32` fields; structural, so a user names
/// the type. `dot`/`length` return an `f32`; the rest return a Vec3 of the same shape as the input.
pub const VEC_SCALAR_FUNCTIONS: &[&str] = &[
    "add",
    "sub",
    "scale",
    "dot",
    "cross",
    "length",
    "normalize",
    "distance",
    "lerp",
    "reflect",
    "clamp",
    "min",
    "max",
    "abs",
];

/// Build the canonical "no such function on a native module" error (→ `E0005`).
pub fn no_function_error(module: &str, func: &str) -> StdError {
    StdError {
        kind: ErrorKind::UnknownName,
        message: format!("module `{module}` has no function `{func}`"),
    }
}

/// Build a deliberate panic (→ the language's panic diagnostic) with a message the dispatch
/// renders in full — deadlocks, an empty `race`, and the other unrecoverable conditions the
/// migrated `Builtin` arms reported as panics (higher-order-abi H2).
pub fn panic_error(message: impl Into<String>) -> StdError {
    StdError {
        kind: ErrorKind::Panic,
        message: message.into(),
    }
}

/// Build the canonical "the run is stopping" error for a blocking host leaf that ended its wait
/// early — see [`ErrorKind::Interrupted`]. `operation` names what stopped (`"read_line"`,
/// `"fetch"`), because the leaf is the only place that still knows.
pub fn interrupted_error(operation: &str) -> StdError {
    StdError {
        kind: ErrorKind::Interrupted,
        message: format!("`{operation}` stopped: the run it belongs to is being cancelled"),
    }
}

/// Build the canonical "no such method on an extern type" error (→ `E0005`), the type-shaped
/// sibling of [`no_function_error`] (extern-types X2).
pub fn no_method_error(type_name: &str, method: &str) -> StdError {
    StdError {
        kind: ErrorKind::UnknownName,
        message: format!("type `{type_name}` has no method `{method}`"),
    }
}

/// Build the canonical "invalid JSON" error for `json.parse` (→ `E0007`).
pub fn invalid_json_error(detail: &str) -> StdError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!("invalid JSON: {detail}"),
    }
}

/// "a string" / "an int" — pick the article so messages read naturally.
fn an(noun: &str) -> String {
    let article = match noun.chars().next() {
        Some('a' | 'e' | 'i' | 'o' | 'u') => "an",
        _ => "a",
    };
    format!("{article} {noun}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_builders_render_canonically() {
        assert_eq!(
            arity_error("reverse", 0, 2).message,
            "method `reverse` takes 0 argument(s) but 2 were supplied"
        );
        assert_eq!(
            type_error("has", "string").message,
            "method `has` expects a string argument"
        );
    }
}
