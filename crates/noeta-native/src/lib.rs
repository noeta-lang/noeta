//! The native-extension ABI (P-NATIVE): the contract a crate implements to register native
//! modules and first-class types into the language, plus the dep-free primitives both backends
//! and the front-end share.
//!
//! Split out of `noeta-stdlib` so the contract does not drag core's batteries (crypto/UUID/JSON):
//! a third-party extension — and internal mid-end crates like `noeta-ir` — depend on this lean
//! crate, while `noeta-stdlib` re-exports it (`pub use noeta_native::*`) and adds the concrete
//! `std` modules on top (the `core`/`std` relationship). See `plans/native-abi/README.md`.

use serde::{Deserialize, Serialize};

pub mod command;
pub mod ctx;
pub mod executor;
pub mod extern_value;
pub mod host;
pub mod map_key;
pub mod net;
pub mod os;
pub mod p2p;
pub mod registry;
pub mod telemetry;

pub use command::{ArgKind, ArgSpec, CommandCtx, EntryArg, EntryCall, ExtCommand, ParsedArgs};
pub use ctx::{
    CtxDispatch, CtxError, CtxOut, CtxResult, ExtState, NativeCtx, PackedField, PackedView,
    Retained, Slot, ctx_arity,
};
pub use executor::{Executor, ExternIo, FsIo, RealBody, SandboxExecutor};
pub use extern_value::{ExternBox, ExternValue};
pub use host::{
    Clock, Entropy, Env, FileReader, FileSystem, Host, Ids, Network, Os, P2p, ReadSource, Rng,
    SyncStatus,
};
pub use map_key::{ExternKeyRef, MapKey, PackedKeyField};
pub use net::{AcceptIo, NetFetchIo, NetRequest, NetResponse, ReplyIo, Request};
pub use os::{ExecIo, ExecResult};
pub use p2p::{P2pBroker, ReceiveIo};
pub use registry::{
    ArenaGetter, BundleFn, BundleReceiver, ConstraintField, ConstraintLayout, CtxTypeDispatch,
    ExtBundle, ExtFn, ExtModule, ExtType, Extension, ModuleDispatch, NativeOut, NativeValue,
    PackedConstraint, RetTy, Scalar, ScalarVec, SigType, TypeDispatch, TypeRecipe,
};
pub use telemetry::{
    AttrValue, DEFAULT_HISTOGRAM_BOUNDS, HistogramPoint, InstrumentId, InstrumentKind, LogRecord,
    Logging, MetricData, MetricPoints, MetricStore, MetricValue, Metrics, NumberPoint, Severity,
    SpanData, SpanEvent, SpanId, SpanKind, SpanStatus, Temporality, TraceContext, Tracing,
};

/// A backend-agnostic view of an argument value, covering only the primitive shapes the
/// stdlib introspects. Each backend cheaply projects its own `Value` onto this; anything
/// the stdlib never inspects collapses to [`Arg::Other`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Arg<'a> {
    Str(&'a str),
    Int(i64),
    Float(f64),
    Bool(bool),
    Other,
}

/// A backend-agnostic result the caller wraps back into its own `Value`. Kept to the
/// shapes Ring 1 string methods actually produce.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    Str(String),
    Bool(bool),
    Int(i64),
    /// A float (e.g. `math.sqrt`). The caller wraps it in its float value.
    Float(f64),
    /// A list of strings (e.g. `split`). The caller builds its native list of string values.
    StrList(Vec<String>),
    /// A byte buffer (e.g. `to_bytes`). The caller wraps it in its bytes value.
    Bytes(Vec<u8>),
    /// An optional string (e.g. `char_at`). The caller builds its `some(...)`/`none` value.
    OptStr(Option<String>),
    /// An optional int (e.g. `to_int`, `index_of`).
    OptInt(Option<i64>),
    /// An optional float (e.g. `to_float`).
    OptFloat(Option<f64>),
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
}

/// A stdlib misuse error. The `message` is rendered here so both backends report it
/// identically; the `kind` selects the diagnostic code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdError {
    pub kind: ErrorKind,
    pub message: String,
}

/// The outcome of attempting a built-in method dispatch.
#[derive(Debug, Clone, PartialEq)]
pub enum Dispatch {
    /// Handled — wrap this output into a native value.
    Done(Output),
    /// Not a built-in method of this surface; the caller continues to other dispatch.
    Unknown,
    /// A built-in method of this surface, but misused.
    Err(StdError),
}

/// The Ring 1 string methods, in dispatch order. Used by name resolution / tooling that
/// wants to know the surface without exercising it.
pub const STRING_METHODS: &[&str] = &[
    "upper",
    "lower",
    "trim",
    "trim_start",
    "trim_end",
    "contains",
    "starts_with",
    "ends_with",
    "split",
    "replace",
    "repeat",
    "is_empty",
    "chars",
    "lines",
    "slice",
    "char_at",
    "index_of",
    "pad_start",
    "pad_end",
    "to_int",
    "to_float",
    "to_bytes",
];

/// Dispatch a built-in string method. `recv` is the receiver string; `args` are the
/// already-projected call arguments. Returns [`Dispatch::Unknown`] when `method` is not
/// part of the string surface so the caller can fall through to its other dispatch.
pub fn string_method(recv: &str, method: &str, args: &[Arg]) -> Dispatch {
    if !STRING_METHODS.contains(&method) {
        return Dispatch::Unknown;
    }
    match string_method_inner(recv, method, args) {
        Ok(output) => Dispatch::Done(output),
        Err(error) => Dispatch::Err(error),
    }
}

fn string_method_inner(recv: &str, method: &str, args: &[Arg]) -> Result<Output, StdError> {
    match method {
        "upper" => {
            want_arity(method, args, 0)?;
            Ok(Output::Str(recv.to_uppercase()))
        }
        "lower" => {
            want_arity(method, args, 0)?;
            Ok(Output::Str(recv.to_lowercase()))
        }
        "trim" => {
            want_arity(method, args, 0)?;
            Ok(Output::Str(recv.trim().to_string()))
        }
        "contains" => {
            want_arity(method, args, 1)?;
            Ok(Output::Bool(recv.contains(want_str(method, args, 0)?)))
        }
        "starts_with" => {
            want_arity(method, args, 1)?;
            Ok(Output::Bool(recv.starts_with(want_str(method, args, 0)?)))
        }
        "ends_with" => {
            want_arity(method, args, 1)?;
            Ok(Output::Bool(recv.ends_with(want_str(method, args, 0)?)))
        }
        // `split(sep, limit?)` — an optional `limit` caps the number of pieces (the last holds
        // the unsplit remainder, Rust's `splitn`); absent or ≤ 0 means unlimited.
        "split" => {
            want_arity_range(method, args, 1, 2)?;
            let sep = want_str(method, args, 0)?;
            let limit = opt_int(method, args, 1)?;
            Ok(Output::StrList(split(recv, sep, limit)))
        }
        "replace" => {
            want_arity(method, args, 2)?;
            let from = want_str(method, args, 0)?;
            let to = want_str(method, args, 1)?;
            Ok(Output::Str(recv.replace(from, to)))
        }
        "repeat" => {
            want_arity(method, args, 1)?;
            let count = want_int(method, args, 0)?;
            Ok(Output::Str(recv.repeat(count.max(0) as usize)))
        }
        "trim_start" => {
            want_arity(method, args, 0)?;
            Ok(Output::Str(recv.trim_start().to_string()))
        }
        "trim_end" => {
            want_arity(method, args, 0)?;
            Ok(Output::Str(recv.trim_end().to_string()))
        }
        "is_empty" => {
            want_arity(method, args, 0)?;
            Ok(Output::Bool(recv.is_empty()))
        }
        // `chars()` — the Unicode scalar characters, each as a string (identical to `split("")`).
        "chars" => {
            want_arity(method, args, 0)?;
            Ok(Output::StrList(split(recv, "", None)))
        }
        // `lines()` — split on `\n`, dropping a trailing `\r` (so `\r\n` input works) and the
        // final empty segment after a trailing newline (Rust's `str::lines`).
        "lines" => {
            want_arity(method, args, 0)?;
            Ok(Output::StrList(recv.lines().map(str::to_string).collect()))
        }
        // `slice(start, end?)` — the half-open character range `[start, end)`, with the same
        // bounds rule as list `slice` (out of bounds is an error, not a clamp). Character-based,
        // so multi-byte text slices at scalar boundaries. `end` is optional — to the string's end.
        "slice" => {
            want_arity_range(method, args, 1, 2)?;
            let start = want_int(method, args, 0)?;
            let len = recv.chars().count();
            let end = opt_int(method, args, 1)?.unwrap_or(len as i64);
            if start < 0 || end < start || end as usize > len {
                return Err(str_slice_bounds_error(start, end, len));
            }
            let taken: String = recv
                .chars()
                .skip(start as usize)
                .take((end - start) as usize)
                .collect();
            Ok(Output::Str(taken))
        }
        // `char_at(i)` — the character at index `i` as a string, or `none` out of range: the
        // safe probe (unlike `slice`, which errors).
        "char_at" => {
            want_arity(method, args, 1)?;
            let index = want_int(method, args, 0)?;
            let found = usize::try_from(index)
                .ok()
                .and_then(|i| recv.chars().nth(i))
                .map(|c| c.to_string());
            Ok(Output::OptStr(found))
        }
        // `index_of(sub, from?)` — the character index of the first occurrence at or after the
        // optional `from` character offset (default 0), or `none`.
        "index_of" => {
            want_arity_range(method, args, 1, 2)?;
            let sub = want_str(method, args, 0)?;
            let from = opt_int(method, args, 1)?.unwrap_or(0).max(0) as usize;
            // Advance to the byte offset of the `from`-th character, then search the remainder;
            // report the match's character index counted from the whole string's start.
            let byte_from = recv
                .char_indices()
                .nth(from)
                .map(|(b, _)| b)
                .unwrap_or(recv.len());
            let found = recv[byte_from..]
                .find(sub)
                .map(|byte| recv[..byte_from + byte].chars().count() as i64);
            Ok(Output::OptInt(found))
        }
        // `pad_start(width, fill?)` / `pad_end(width, fill?)` — `fill` optional (default a space).
        "pad_start" => {
            want_arity_range(method, args, 1, 2)?;
            let width = want_int(method, args, 0)?;
            let fill = opt_str(method, args, 1)?.unwrap_or(" ");
            Ok(Output::Str(pad(recv, width, fill, method, Pad::Start)?))
        }
        "pad_end" => {
            want_arity_range(method, args, 1, 2)?;
            let width = want_int(method, args, 0)?;
            let fill = opt_str(method, args, 1)?.unwrap_or(" ");
            Ok(Output::Str(pad(recv, width, fill, method, Pad::End)?))
        }
        // `to_int()`/`to_float()` — strict numeric parsing (no implicit trim; compose with
        // `trim()`), `none` on any malformed input. The safe bridge from text to numbers.
        "to_int" => {
            want_arity(method, args, 0)?;
            Ok(Output::OptInt(recv.parse::<i64>().ok()))
        }
        "to_float" => {
            want_arity(method, args, 0)?;
            Ok(Output::OptFloat(recv.parse::<f64>().ok()))
        }
        // `to_bytes()` — the UTF-8 encoding. `bytes.decode()` is the inverse.
        "to_bytes" => {
            want_arity(method, args, 0)?;
            Ok(Output::Bytes(recv.as_bytes().to_vec()))
        }
        // STRING_METHODS gates entry, so every listed method is handled above.
        _ => unreachable!("unlisted string method `{method}`"),
    }
}

enum Pad {
    Start,
    End,
}

/// Pad `recv` with repetitions of `fill` (truncated to fit) until it is `width` characters —
/// JS `padStart`/`padEnd` semantics. A string already at least `width` long is returned
/// unchanged; an empty `fill` is a type error (it can never make progress).
fn pad(recv: &str, width: i64, fill: &str, method: &str, side: Pad) -> Result<String, StdError> {
    if fill.is_empty() {
        return Err(type_error(method, "non-empty string fill"));
    }
    let len = recv.chars().count();
    let width = usize::try_from(width).unwrap_or(0);
    if len >= width {
        return Ok(recv.to_string());
    }
    let padding: String = fill.chars().cycle().take(width - len).collect();
    Ok(match side {
        Pad::Start => format!("{padding}{recv}"),
        Pad::End => format!("{recv}{padding}"),
    })
}

/// Decode a byte buffer as UTF-8 — the shared body of `bytes.decode()`, `None` when the bytes
/// are not valid UTF-8 (the caller renders its `none`).
pub fn bytes_decode_utf8(data: &[u8]) -> Option<String> {
    std::str::from_utf8(data).ok().map(str::to_string)
}

/// The string twin of [`slice_bounds_error`] — same shape, "string" noun, character count.
fn str_slice_bounds_error(start: i64, end: i64, len: usize) -> StdError {
    StdError {
        kind: ErrorKind::Bounds,
        message: format!("slice [{start}..{end}] is out of bounds for string of length {len}"),
    }
}

/// Split `recv` on `sep`. An empty separator yields the Unicode scalar characters (the
/// useful, surprise-free reading), avoiding Rust's leading/trailing empty fields. A `limit > 0`
/// caps the number of pieces (the last holds the unsplit remainder, `splitn`); an absent or
/// non-positive limit is unlimited. An empty separator ignores the limit (it always yields chars).
fn split(recv: &str, sep: &str, limit: Option<i64>) -> Vec<String> {
    if sep.is_empty() {
        return recv.chars().map(|c| c.to_string()).collect();
    }
    match limit {
        Some(n) if n > 0 => recv.splitn(n as usize, sep).map(str::to_string).collect(),
        _ => recv.split(sep).map(str::to_string).collect(),
    }
}

fn want_arity(method: &str, args: &[Arg], expected: usize) -> Result<(), StdError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(arity_error(method, expected, args.len()))
    }
}

/// Accept `min..=max` arguments — a built-in method with a trailing-optional parameter (the core
/// analogue of a Ring 2 function's `SigType::Optional`). The checker already gates the range, so
/// this is the defensive twin of [`want_arity`]; on violation it reports `max` as the expected.
fn want_arity_range(method: &str, args: &[Arg], min: usize, max: usize) -> Result<(), StdError> {
    if (min..=max).contains(&args.len()) {
        Ok(())
    } else {
        Err(arity_error(method, max, args.len()))
    }
}

fn want_str<'a>(method: &str, args: &[Arg<'a>], index: usize) -> Result<&'a str, StdError> {
    match args[index] {
        Arg::Str(value) => Ok(value),
        _ => Err(type_error(method, "string")),
    }
}

fn want_int(method: &str, args: &[Arg], index: usize) -> Result<i64, StdError> {
    match args[index] {
        Arg::Int(value) => Ok(value),
        _ => Err(type_error(method, "int")),
    }
}

/// An **optional** int argument at `index`: `None` when absent, the value when present, a type
/// error when present-but-not-an-int. The reader for a trailing-optional parameter.
fn opt_int(method: &str, args: &[Arg], index: usize) -> Result<Option<i64>, StdError> {
    match args.get(index) {
        None => Ok(None),
        Some(Arg::Int(value)) => Ok(Some(*value)),
        Some(_) => Err(type_error(method, "int")),
    }
}

/// An **optional** string argument at `index` — the string twin of [`opt_int`].
fn opt_str<'a>(method: &str, args: &[Arg<'a>], index: usize) -> Result<Option<&'a str>, StdError> {
    match args.get(index) {
        None => Ok(None),
        Some(Arg::Str(value)) => Ok(Some(value)),
        Some(_) => Err(type_error(method, "string")),
    }
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

/// Build the canonical "no such method on an extern type" error (→ `E0005`), the type-shaped
/// sibling of [`no_function_error`] (extern-types X2).
pub fn no_method_error(type_name: &str, method: &str) -> StdError {
    StdError {
        kind: ErrorKind::UnknownName,
        message: format!("type `{type_name}` has no method `{method}`"),
    }
}

/// Lowercase hex rendering of a byte buffer — the `bytes.to_hex()` method (crypto arc C1),
/// defined once so both backends print digests identically.
pub fn bytes_to_hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for b in data {
        use std::fmt::Write;
        write!(out, "{b:02x}").expect("writing to a String cannot fail");
    }
    out
}

/// Build the canonical "invalid JSON" error for `json.parse` (→ `E0007`).
pub fn invalid_json_error(detail: &str) -> StdError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!("invalid JSON: {detail}"),
    }
}

/// Format an `f64` for display and serialization: a whole finite value keeps one decimal place
/// (`2.0`, not `2`), everything else uses the shortest round-tripping form. Both backends' `display`
/// and the shared JSON serializer call this, so numbers render identically everywhere — the
/// single source the two duplicated copies used to be.
pub fn format_float(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 {
        format!("{f:.1}")
    } else {
        f.to_string()
    }
}

/// Format an `f32` at f32 precision (the shortest round-tripping f32 decimal) — the `f32` analog of
/// [`format_float`], so e.g. `0.1f32` shows `0.1`, not the f64-widened `0.10000000149…`.
pub fn format_f32(f: f32) -> String {
    if f.is_finite() && f.fract() == 0.0 {
        format!("{f:.1}")
    } else {
        f.to_string()
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

/// The Ring 1 list methods that manipulate backend-specific values, so each backend implements
/// the value work itself. They are enumerated here (rather than matched as strings in each
/// backend) so a `match` over [`ListMethod`] is exhaustive: adding a method will not compile
/// until *both* backends handle it — the differential's static guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListMethod {
    /// `reverse()` → a new list with the elements in reverse order.
    Reverse,
    /// `contains(x)` → `bool`, by structural value equality.
    Contains,
    /// `join(sep)` → a string of the elements' display forms separated by `sep`.
    Join,
    /// `sorted()` → a new list sorted by the primitive ordering (homogeneous numbers or
    /// strings); a non-orderable or mixed-kind element is an error.
    Sorted,
    /// `slice(start, end)` → the sublist `[start, end)`; out-of-range bounds are an error.
    Slice,
    /// `first()` → `some(head)` if the list is non-empty, else `none`.
    First,
    /// `last()` → `some(tail)` if the list is non-empty, else `none`.
    Last,
    /// `to_set()` → a `Set` of the list's elements (sorted + de-duplicated); a non-orderable or
    /// mixed-kind element is an error.
    ToSet,
    /// `set(index, value)` → a **new** list with `index` replaced by `value` (value semantics; the
    /// receiver is unchanged). An out-of-range index is an error (E0016), as for index reads — `set`
    /// replaces an existing position, it does not append. The reuse pass may make this an in-place
    /// overwrite when the receiver is uniquely owned. This is the target of the `xs[i] = v` sugar.
    Set,
}

impl ListMethod {
    pub fn from_name(name: &str) -> Option<ListMethod> {
        match name {
            "reverse" => Some(ListMethod::Reverse),
            "contains" => Some(ListMethod::Contains),
            "join" => Some(ListMethod::Join),
            "sorted" => Some(ListMethod::Sorted),
            "slice" => Some(ListMethod::Slice),
            "first" => Some(ListMethod::First),
            "last" => Some(ListMethod::Last),
            "to_set" => Some(ListMethod::ToSet),
            "set" => Some(ListMethod::Set),
            _ => None,
        }
    }
}

/// The Ring 1 set methods. Value-specific (each backend implements them), enumerated here so the
/// dispatch `match` is exhaustive in both backends — see [`ListMethod`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetMethod {
    /// `contains(x)` → `bool`, whether `x` is a member (by structural equality).
    Contains,
    /// `union(other)` → a new set with every element of `self` or `other`.
    Union,
    /// `intersection(other)` → a new set with the elements in both `self` and `other`.
    Intersection,
    /// `add(x)` → a **new** set with `x` added (a no-op copy if already present). Value semantics; the
    /// single-element companion to `union`. Same in-place-reuse treatment as the other updates.
    Add,
    /// `remove(x)` → a **new** set without `x` (a no-op copy if absent). Value semantics.
    Remove,
}

impl SetMethod {
    pub fn from_name(name: &str) -> Option<SetMethod> {
        match name {
            "contains" => Some(SetMethod::Contains),
            "union" => Some(SetMethod::Union),
            "intersection" => Some(SetMethod::Intersection),
            "add" => Some(SetMethod::Add),
            "remove" => Some(SetMethod::Remove),
            _ => None,
        }
    }
}

/// The Ring 1 map methods. See [`ListMethod`] for why these are an enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapMethod {
    /// `keys()` → a list of the map's keys (sorted, since maps iterate in key order).
    Keys,
    /// `values()` → a list of the map's values, in key order.
    Values,
    /// `has(key)` → `bool`, whether `key` is present.
    Has,
    /// `set(key, value)` → a **new** map with `key` mapped to `value` (added or overwritten). Value
    /// semantics — the receiver is unchanged; the reuse pass may make this an in-place mutation when
    /// the receiver is uniquely owned (the map analog of the list `~` self-append).
    Set,
    /// `remove(key)` → a **new** map without `key` (a no-op copy if `key` is absent). Value semantics,
    /// same in-place-reuse treatment as [`MapMethod::Set`].
    Remove,
    /// `get_or(key, default)` → the value at `key`, or `default` if `key` is absent. The fused,
    /// allocation-free read-with-default: one probe where `if m.has(k) then m[k] else d` costs two
    /// (and no `Option` box, which is why this is not `get() -> ?V`).
    GetOr,
}

impl MapMethod {
    pub fn from_name(name: &str) -> Option<MapMethod> {
        match name {
            "keys" => Some(MapMethod::Keys),
            "values" => Some(MapMethod::Values),
            "has" => Some(MapMethod::Has),
            "set" => Some(MapMethod::Set),
            "remove" => Some(MapMethod::Remove),
            "get_or" => Some(MapMethod::GetOr),
            _ => None,
        }
    }
}

/// The bit-manipulation methods on `int` (P-BITS Tier B4) — the popcount-class intrinsics that turn a
/// bitmask into an index/count. Enumerated (like [`ListMethod`]) so both backends' dispatch `match` is
/// exhaustive; the actual computation is the shared [`int_method`] below, so the backends agree by
/// construction. All operate on the full signed i64.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntMethod {
    /// `count_ones()` → the number of set bits (population count).
    CountOnes,
    /// `count_zeros()` → the number of clear bits — **width-relative to i64** (so `(0).count_zeros()`
    /// is 64), exact for the user's width only under Tier W.
    CountZeros,
    /// `leading_zeros()` → clear bits above the highest set bit — **width-relative to i64** (so
    /// `(1).leading_zeros()` is 63).
    LeadingZeros,
    /// `trailing_zeros()` → clear bits below the lowest set bit (the index of the lowest set bit);
    /// `(0).trailing_zeros()` is 64.
    TrailingZeros,
    /// `rotate_left(n)` → bits rotated left by `n`, wrapping around (cyclic — the amount is taken mod
    /// 64, so any `n` is valid and lossless).
    RotateLeft,
    /// `rotate_right(n)` → the mirror of [`IntMethod::RotateLeft`].
    RotateRight,
    /// `reverse_bits()` → the value with its bit order reversed (bit 0 ↔ bit 63).
    ReverseBits,
    /// `swap_bytes()` → the value with its byte order reversed (endianness swap).
    SwapBytes,
    /// A total, explicit numeric conversion (Tier W4): `to_u8`/`to_i32`/…/`to_int` on an `int` or a
    /// fixed-width integer. Because every fixed-width value is an erased i64 already sign/zero-extended
    /// for its *source* type, the conversion to any destination is a single [`mask_to_width`] into the
    /// destination's `(signed, bits)` — matching Rust's `as` cast (widen = safe, narrow = wrapping
    /// truncation, cross-signedness = bit reinterpretation). `to_int` and `to_i64` both carry
    /// `(true, 64)` (identical at runtime; the checker keeps their static types distinct).
    Convert { signed: bool, bits: u8 },
}

impl IntMethod {
    pub fn from_name(name: &str) -> Option<IntMethod> {
        Some(match name {
            "count_ones" => IntMethod::CountOnes,
            "count_zeros" => IntMethod::CountZeros,
            "leading_zeros" => IntMethod::LeadingZeros,
            "trailing_zeros" => IntMethod::TrailingZeros,
            "rotate_left" => IntMethod::RotateLeft,
            "rotate_right" => IntMethod::RotateRight,
            "reverse_bits" => IntMethod::ReverseBits,
            "swap_bytes" => IntMethod::SwapBytes,
            _ => return Self::conversion_from_name(name),
        })
    }

    /// Decode a `to_<type>` conversion method name into its destination `(signed, bits)`. `to_int`
    /// is the i64-signed identity; otherwise the suffix is an `i8`/`u32`/… spelling. Kept here (not
    /// in `noeta-types`) so `noeta-stdlib` stays dependency-free; the checker decodes names to *types*
    /// separately (it must tell `to_int` from `to_i64`, which share a runtime `Convert`).
    fn conversion_from_name(name: &str) -> Option<IntMethod> {
        let rest = name.strip_prefix("to_")?;
        if rest == "int" {
            return Some(IntMethod::Convert {
                signed: true,
                bits: 64,
            });
        }
        let signed = match rest.as_bytes().first()? {
            b'i' => true,
            b'u' => false,
            _ => return None,
        };
        let bits = match &rest[1..] {
            "8" => 8,
            "16" => 16,
            "32" => 32,
            "64" => 64,
            _ => return None,
        };
        Some(IntMethod::Convert { signed, bits })
    }

    /// The number of arguments the method takes: `rotate_left`/`rotate_right` take one shift amount;
    /// the rest (including the `Convert` conversions) take none.
    pub fn arity(self) -> usize {
        match self {
            IntMethod::RotateLeft | IntMethod::RotateRight => 1,
            _ => 0,
        }
    }
}

/// Apply an [`IntMethod`] to receiver `recv`, with `arg` the shift amount for `rotate_left`/
/// `rotate_right` (ignored otherwise). The single source of truth both backends call, delegating to
/// the `i64` inherent methods so the results are identical. The bit-count methods return a value in
/// `0..=64`; the rotate amount is taken mod 64 (cyclic, lossless).
pub fn int_method(recv: i64, method: IntMethod, arg: i64) -> i64 {
    match method {
        IntMethod::CountOnes => recv.count_ones() as i64,
        IntMethod::CountZeros => recv.count_zeros() as i64,
        IntMethod::LeadingZeros => recv.leading_zeros() as i64,
        IntMethod::TrailingZeros => recv.trailing_zeros() as i64,
        IntMethod::RotateLeft => recv.rotate_left((arg as u64 & 63) as u32),
        IntMethod::RotateRight => recv.rotate_right((arg as u64 & 63) as u32),
        IntMethod::ReverseBits => recv.reverse_bits(),
        IntMethod::SwapBytes => recv.swap_bytes(),
        // Total conversion (Tier W4): the erased i64 is already correctly extended for its source
        // type, so re-masking into the destination width yields the `as`-cast result in one step.
        IntMethod::Convert { signed, bits } => mask_to_width(recv, signed, bits),
    }
}

/// Apply a bit-manipulation intrinsic to `recv` **exactly within a `bits`-wide integer** (Tier W5):
/// the ops act on the low `bits` bits, not the full i64, so `(1u8).leading_zeros() == 7`,
/// `(0u8).count_zeros() == 8`, and rotate/reverse/swap wrap within the width. Signedness is
/// irrelevant (these read the value as its `bits`-bit pattern). For `bits >= 64` this is exactly
/// [`int_method`]. Never called with `Convert` (a conversion is width-typed at the call site, not a
/// width-relative intrinsic).
pub fn int_method_width(recv: i64, method: IntMethod, arg: i64, bits: u8) -> i64 {
    if bits >= 64 {
        return int_method(recv, method, arg);
    }
    let width = bits as u32;
    let mask = (1u64 << width) - 1;
    let v = (recv as u64) & mask; // the bits-wide value, zero-extended into a u64
    match method {
        IntMethod::CountOnes => v.count_ones() as i64,
        IntMethod::CountZeros => (width - v.count_ones()) as i64,
        // `64 - v.leading_zeros()` is the count of significant bits (≤ width, since `v < 2^width`),
        // so the leading zeros *within the width* is `width - significant`.
        IntMethod::LeadingZeros => (width - (64 - v.leading_zeros())) as i64,
        IntMethod::TrailingZeros => {
            if v == 0 {
                width as i64
            } else {
                v.trailing_zeros() as i64
            }
        }
        IntMethod::RotateLeft => {
            let n = (arg.rem_euclid(width as i64)) as u32;
            let r = if n == 0 {
                v
            } else {
                ((v << n) | (v >> (width - n))) & mask
            };
            r as i64
        }
        IntMethod::RotateRight => {
            let n = (arg.rem_euclid(width as i64)) as u32;
            let r = if n == 0 {
                v
            } else {
                ((v >> n) | (v << (width - n))) & mask
            };
            r as i64
        }
        // Reverse / swap operate on the full 64 bits, so shift the reversed low-`bits` field back down.
        IntMethod::ReverseBits => (v.reverse_bits() >> (64 - width)) as i64,
        IntMethod::SwapBytes => (v.swap_bytes() >> (64 - width)) as i64,
        IntMethod::Convert { .. } => {
            unreachable!("Convert is a width-typed conversion, not a width-relative intrinsic")
        }
    }
}

/// Reduce an i64 word to the value it represents in a fixed-width integer type (Tier W). Fixed-width
/// values are **erased to i64** at runtime — the width lives only in the static type — so after any
/// width-bearing op (`+ - *`, unary `-`) the compiler applies this to wrap the result into range:
///
/// - **unsigned**: zero the bits above `bits` (`value & (2^bits - 1)`), so `255u8 + 1 → 0`.
/// - **signed**: sign-extend from bit `bits-1` via an arithmetic shift pair, so `128 → -128` for i8
///   and the stored word is the correct two's-complement pattern for the width.
/// - **`bits == 64`**: the i64 word already *is* the u64/i64 bit pattern — a no-op (and avoids the
///   `1u64 << 64` overflow).
///
/// Both backends call this on the same erased word, so wraparound is identical by construction. It is
/// sign-agnostic for `+ - *` (their low `bits` are the same whether operands are read signed or
/// unsigned), which is why those three land before the sign-dependent `/ % < >>`.
pub fn mask_to_width(value: i64, signed: bool, bits: u8) -> i64 {
    if bits >= 64 {
        return value;
    }
    if signed {
        let shift = 64 - bits;
        (value << shift) >> shift
    } else {
        (value as u64 & ((1u64 << bits) - 1)) as i64
    }
}

/// A numeric scalar in either the integer or the float domain — the shared currency of the
/// cross-domain conversion tower (S0 / P-VMT-CONV). Both backends read a receiver into one of these,
/// convert with [`num_convert`], and map the result back to their own `Value`. An integer (`int` or
/// any fixed-width `IntN`) is carried as its erased, sign/zero-extended `i64` (the runtime
/// representation); `f64` is the platform `float`, `f32` the 32-bit float.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NumScalar {
    Int(i64),
    F64(f64),
    F32(f32),
}

/// A conversion destination that **crosses** the int/float domains, decoded from a `to_<type>`
/// method name: `to_float`/`to_f64` → `f64`, `to_f32` → `f32`, and `to_int`/`to_i8`…/`to_u64` → an
/// integer of that width. The pure int→int conversions keep their existing [`IntMethod::Convert`]
/// path; this type is used only where at least one side is a float (an `int` receiver reaches it only
/// for the two float destinations, a `float`/`f32` receiver for any).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NumConvert {
    /// `to_float` / `to_f64` — widen/convert to the 64-bit float.
    ToF64,
    /// `to_f32` — convert to the 32-bit float (round-to-nearest on narrowing).
    ToF32,
    /// `to_int` / `to_i8` … / `to_u64` — convert to an integer of `(signed, bits)`.
    ToInt { signed: bool, bits: u8 },
}

impl NumConvert {
    /// Decode a conversion method name, or `None` if `name` is not a `to_<type>` conversion. Reuses
    /// [`IntMethod::from_name`] for the integer-destination spellings so the width parsing has one
    /// source of truth.
    pub fn from_name(name: &str) -> Option<NumConvert> {
        match name {
            "to_float" | "to_f64" => Some(NumConvert::ToF64),
            "to_f32" => Some(NumConvert::ToF32),
            _ => match IntMethod::from_name(name) {
                Some(IntMethod::Convert { signed, bits }) => {
                    Some(NumConvert::ToInt { signed, bits })
                }
                _ => None,
            },
        }
    }
}

/// Convert a numeric scalar across domains, matching Rust's `as` cast: int↔float is value-preserving
/// where it fits (rounding to nearest on `f32`), **float→int saturates** to the destination range
/// (with `NaN` → 0), and int→int re-masks into the destination width (wrapping truncation). The
/// single source of truth both backends call, so a conversion result is identical by construction.
pub fn num_convert(src: NumScalar, dest: NumConvert) -> NumScalar {
    let as_f64 = |s: NumScalar| match s {
        NumScalar::Int(i) => i as f64,
        NumScalar::F64(f) => f,
        NumScalar::F32(f) => f as f64,
    };
    match dest {
        NumConvert::ToF64 => NumScalar::F64(as_f64(src)),
        NumConvert::ToF32 => NumScalar::F32(match src {
            NumScalar::Int(i) => i as f32,
            NumScalar::F64(f) => f as f32,
            NumScalar::F32(f) => f,
        }),
        NumConvert::ToInt { signed, bits } => NumScalar::Int(match src {
            // int→int keeps the bit-preserving mask (the erased word is already correctly extended).
            NumScalar::Int(i) => mask_to_width(i, signed, bits),
            // float→int: Rust's saturating `as` (NaN→0), cast straight to the destination width so
            // out-of-range values clamp rather than wrap, then erase to the i64 word.
            NumScalar::F64(f) => float_to_int(f, signed, bits),
            NumScalar::F32(f) => float_to_int(f as f64, signed, bits),
        }),
    }
}

/// A saturating float→integer cast into `(signed, bits)`, returned as the erased, sign/zero-extended
/// `i64` word. Delegates to Rust's primitive `as` casts (saturating + `NaN`→0 since 1.45), so
/// `(1000.0).to_u8()` is `255`, `(-1.0).to_u8()` is `0`, and `(3.9).to_int()` is `3`.
fn float_to_int(f: f64, signed: bool, bits: u8) -> i64 {
    if signed {
        match bits {
            8 => f as i8 as i64,
            16 => f as i16 as i64,
            32 => f as i32 as i64,
            _ => f as i64,
        }
    } else {
        match bits {
            8 => f as u8 as i64,
            16 => f as u16 as i64,
            32 => f as u32 as i64,
            _ => f as u64 as i64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(recv: &str, method: &str, args: &[Arg]) -> Output {
        match string_method(recv, method, args) {
            Dispatch::Done(output) => output,
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn case_transforms_and_trim() {
        assert_eq!(done("Hi", "upper", &[]), Output::Str("HI".into()));
        assert_eq!(done("Hi", "lower", &[]), Output::Str("hi".into()));
        assert_eq!(done("  x  ", "trim", &[]), Output::Str("x".into()));
    }

    #[test]
    fn predicates_return_bool() {
        assert_eq!(
            done("hello", "contains", &[Arg::Str("ell")]),
            Output::Bool(true)
        );
        assert_eq!(
            done("hello", "starts_with", &[Arg::Str("he")]),
            Output::Bool(true)
        );
        assert_eq!(
            done("hello", "ends_with", &[Arg::Str("lo")]),
            Output::Bool(true)
        );
        assert_eq!(
            done("hello", "contains", &[Arg::Str("z")]),
            Output::Bool(false)
        );
    }

    #[test]
    fn split_on_separator_and_on_empty() {
        assert_eq!(
            done("a,b,c", "split", &[Arg::Str(",")]),
            Output::StrList(vec!["a".into(), "b".into(), "c".into()])
        );
        assert_eq!(
            done("abc", "split", &[Arg::Str("")]),
            Output::StrList(vec!["a".into(), "b".into(), "c".into()])
        );
    }

    #[test]
    fn replace_and_repeat() {
        assert_eq!(
            done("a.b.c", "replace", &[Arg::Str("."), Arg::Str("/")]),
            Output::Str("a/b/c".into())
        );
        assert_eq!(
            done("ab", "repeat", &[Arg::Int(3)]),
            Output::Str("ababab".into())
        );
        // A negative count clamps to an empty string rather than panicking.
        assert_eq!(
            done("ab", "repeat", &[Arg::Int(-1)]),
            Output::Str(String::new())
        );
    }

    #[test]
    fn unicode_is_handled_per_scalar() {
        assert_eq!(done("naïve", "upper", &[]), Output::Str("NAÏVE".into()));
        assert_eq!(
            done("héllo", "split", &[Arg::Str("")]),
            Output::StrList(vec![
                "h".into(),
                "é".into(),
                "l".into(),
                "l".into(),
                "o".into()
            ])
        );
    }

    #[test]
    fn trim_sides_and_emptiness() {
        assert_eq!(done("  x  ", "trim_start", &[]), Output::Str("x  ".into()));
        assert_eq!(done("  x  ", "trim_end", &[]), Output::Str("  x".into()));
        assert_eq!(done("", "is_empty", &[]), Output::Bool(true));
        assert_eq!(done(" ", "is_empty", &[]), Output::Bool(false));
    }

    #[test]
    fn chars_and_lines() {
        assert_eq!(
            done("héy", "chars", &[]),
            Output::StrList(vec!["h".into(), "é".into(), "y".into()])
        );
        // `\r\n` and a trailing newline both normalize away (Rust `str::lines`).
        assert_eq!(
            done("a\r\nb\nc\n", "lines", &[]),
            Output::StrList(vec!["a".into(), "b".into(), "c".into()])
        );
    }

    #[test]
    fn slice_is_char_based_and_bounds_checked() {
        assert_eq!(
            done("héllo", "slice", &[Arg::Int(1), Arg::Int(3)]),
            Output::Str("él".into())
        );
        assert_eq!(
            done("abc", "slice", &[Arg::Int(0), Arg::Int(0)]),
            Output::Str(String::new())
        );
        match string_method("abc", "slice", &[Arg::Int(1), Arg::Int(9)]) {
            Dispatch::Err(error) => assert_eq!(error.kind, ErrorKind::Bounds),
            other => panic!("expected Err, got {other:?}"),
        }
        match string_method("abc", "slice", &[Arg::Int(-1), Arg::Int(2)]) {
            Dispatch::Err(error) => assert_eq!(error.kind, ErrorKind::Bounds),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn char_at_and_index_of_are_safe_probes() {
        assert_eq!(
            done("héy", "char_at", &[Arg::Int(1)]),
            Output::OptStr(Some("é".into()))
        );
        assert_eq!(done("héy", "char_at", &[Arg::Int(9)]), Output::OptStr(None));
        assert_eq!(
            done("héy", "char_at", &[Arg::Int(-1)]),
            Output::OptStr(None)
        );
        // `index_of` reports the *character* index, not the byte offset.
        assert_eq!(
            done("héllo", "index_of", &[Arg::Str("llo")]),
            Output::OptInt(Some(2))
        );
        assert_eq!(
            done("héllo", "index_of", &[Arg::Str("z")]),
            Output::OptInt(None)
        );
        assert_eq!(
            done("abc", "index_of", &[Arg::Str("")]),
            Output::OptInt(Some(0))
        );
    }

    #[test]
    fn pad_fills_to_width() {
        assert_eq!(
            done("7", "pad_start", &[Arg::Int(3), Arg::Str("0")]),
            Output::Str("007".into())
        );
        assert_eq!(
            done("ab", "pad_end", &[Arg::Int(5), Arg::Str("xy")]),
            Output::Str("abxyx".into())
        );
        // Already wide enough — unchanged (and a negative width is a no-op, not a panic).
        assert_eq!(
            done("abcd", "pad_start", &[Arg::Int(2), Arg::Str("0")]),
            Output::Str("abcd".into())
        );
        assert_eq!(
            done("ab", "pad_end", &[Arg::Int(-3), Arg::Str("0")]),
            Output::Str("ab".into())
        );
        match string_method("x", "pad_start", &[Arg::Int(3), Arg::Str("")]) {
            Dispatch::Err(error) => assert_eq!(error.kind, ErrorKind::ArgType),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn optional_params_default_when_absent() {
        // `split(sep, limit?)` — absent limit is unlimited; a positive limit caps the pieces
        // (last holds the remainder); an empty separator yields chars and ignores the limit.
        assert_eq!(
            done("a,b,c,d", "split", &[Arg::Str(",")]),
            Output::StrList(vec!["a".into(), "b".into(), "c".into(), "d".into()])
        );
        assert_eq!(
            done("a,b,c,d", "split", &[Arg::Str(","), Arg::Int(2)]),
            Output::StrList(vec!["a".into(), "b,c,d".into()])
        );
        assert_eq!(
            done("a,b,c", "split", &[Arg::Str(","), Arg::Int(0)]),
            Output::StrList(vec!["a".into(), "b".into(), "c".into()])
        );
        // `slice(start, end?)` — absent end runs to the string's end.
        assert_eq!(
            done("héllo", "slice", &[Arg::Int(1)]),
            Output::Str("éllo".into())
        );
        // `index_of(sub, from?)` — search starts at the optional char offset.
        assert_eq!(
            done("abcabc", "index_of", &[Arg::Str("b")]),
            Output::OptInt(Some(1))
        );
        assert_eq!(
            done("abcabc", "index_of", &[Arg::Str("b"), Arg::Int(2)]),
            Output::OptInt(Some(4))
        );
        assert_eq!(
            done("abcabc", "index_of", &[Arg::Str("b"), Arg::Int(5)]),
            Output::OptInt(None)
        );
        // `pad_start/pad_end(width, fill?)` — absent fill defaults to a space.
        assert_eq!(
            done("7", "pad_start", &[Arg::Int(3)]),
            Output::Str("  7".into())
        );
        assert_eq!(
            done("7", "pad_end", &[Arg::Int(3)]),
            Output::Str("7  ".into())
        );
        // The optional arg still type-checks defensively when present.
        match string_method("x", "split", &[Arg::Str(","), Arg::Str("nope")]) {
            Dispatch::Err(error) => assert_eq!(error.kind, ErrorKind::ArgType),
            other => panic!("expected Err, got {other:?}"),
        }
        // Too many args is still an arity error (max is 2).
        match string_method("x", "slice", &[Arg::Int(0), Arg::Int(1), Arg::Int(2)]) {
            Dispatch::Err(error) => assert_eq!(error.kind, ErrorKind::Arity),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn numeric_parsing_is_strict() {
        assert_eq!(done("42", "to_int", &[]), Output::OptInt(Some(42)));
        assert_eq!(done("-7", "to_int", &[]), Output::OptInt(Some(-7)));
        // No implicit trim, no float acceptance — compose with `trim()` instead.
        assert_eq!(done(" 42", "to_int", &[]), Output::OptInt(None));
        assert_eq!(done("4.2", "to_int", &[]), Output::OptInt(None));
        assert_eq!(done("nope", "to_int", &[]), Output::OptInt(None));
        assert_eq!(done("4.2", "to_float", &[]), Output::OptFloat(Some(4.2)));
        assert_eq!(done("-3", "to_float", &[]), Output::OptFloat(Some(-3.0)));
        assert_eq!(done("x", "to_float", &[]), Output::OptFloat(None));
    }

    #[test]
    fn bytes_round_trip() {
        assert_eq!(
            done("hé", "to_bytes", &[]),
            Output::Bytes(vec![0x68, 0xc3, 0xa9])
        );
        assert_eq!(
            bytes_decode_utf8(&[0x68, 0xc3, 0xa9]),
            Some("hé".to_string())
        );
        // Invalid UTF-8 decodes to `None`, not a lossy replacement.
        assert_eq!(bytes_decode_utf8(&[0xff, 0xfe]), None);
    }

    #[test]
    fn unknown_method_falls_through() {
        assert_eq!(string_method("x", "frobnicate", &[]), Dispatch::Unknown);
    }

    #[test]
    fn arity_mismatch_is_an_error() {
        match string_method("x", "upper", &[Arg::Str("y")]) {
            Dispatch::Err(error) => assert_eq!(error.kind, ErrorKind::Arity),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn wrong_argument_type_is_an_error() {
        match string_method("x", "contains", &[Arg::Int(1)]) {
            Dispatch::Err(error) => assert_eq!(error.kind, ErrorKind::ArgType),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn collection_method_names_resolve() {
        assert_eq!(ListMethod::from_name("reverse"), Some(ListMethod::Reverse));
        assert_eq!(
            ListMethod::from_name("contains"),
            Some(ListMethod::Contains)
        );
        assert_eq!(ListMethod::from_name("join"), Some(ListMethod::Join));
        assert_eq!(ListMethod::from_name("sorted"), Some(ListMethod::Sorted));
        assert_eq!(ListMethod::from_name("slice"), Some(ListMethod::Slice));
        assert_eq!(ListMethod::from_name("first"), Some(ListMethod::First));
        assert_eq!(ListMethod::from_name("last"), Some(ListMethod::Last));
        assert_eq!(ListMethod::from_name("to_set"), Some(ListMethod::ToSet));
        assert_eq!(ListMethod::from_name("nope"), None);
        assert_eq!(MapMethod::from_name("keys"), Some(MapMethod::Keys));
        assert_eq!(MapMethod::from_name("values"), Some(MapMethod::Values));
        assert_eq!(MapMethod::from_name("has"), Some(MapMethod::Has));
        assert_eq!(MapMethod::from_name("nope"), None);
        assert_eq!(SetMethod::from_name("contains"), Some(SetMethod::Contains));
        assert_eq!(SetMethod::from_name("union"), Some(SetMethod::Union));
        assert_eq!(
            SetMethod::from_name("intersection"),
            Some(SetMethod::Intersection)
        );
        assert_eq!(SetMethod::from_name("nope"), None);
    }

    #[test]
    fn int_methods_resolve_and_compute() {
        // Name resolution + arity.
        assert_eq!(
            IntMethod::from_name("count_ones"),
            Some(IntMethod::CountOnes)
        );
        assert_eq!(
            IntMethod::from_name("trailing_zeros"),
            Some(IntMethod::TrailingZeros)
        );
        assert_eq!(
            IntMethod::from_name("rotate_left"),
            Some(IntMethod::RotateLeft)
        );
        assert_eq!(IntMethod::from_name("nope"), None);
        assert_eq!(IntMethod::CountOnes.arity(), 0);
        assert_eq!(IntMethod::RotateRight.arity(), 1);

        // Computation, delegating to the i64 inherent methods.
        assert_eq!(int_method(0b1011, IntMethod::CountOnes, 0), 3);
        assert_eq!(int_method(0b1011, IntMethod::CountZeros, 0), 61);
        assert_eq!(int_method(1, IntMethod::LeadingZeros, 0), 63);
        assert_eq!(int_method(8, IntMethod::TrailingZeros, 0), 3);
        assert_eq!(int_method(0, IntMethod::TrailingZeros, 0), 64);
        assert_eq!(int_method(1, IntMethod::RotateLeft, 4), 16);
        assert_eq!(int_method(256, IntMethod::RotateRight, 4), 16);
        assert_eq!(int_method(-1, IntMethod::ReverseBits, 0), -1);
        // The rotate amount is cyclic (mod 64), so a huge or negative amount is well-defined.
        assert_eq!(
            int_method(1, IntMethod::RotateLeft, 64),
            int_method(1, IntMethod::RotateLeft, 0)
        );
    }

    #[test]
    fn conversions_resolve_and_cast_like_rust_as() {
        // Name resolution: `to_int` and every `to_<width>` decode to a `Convert`; `to_int`/`to_i64`
        // share the signed-64 identity; unknown / non-conversion `to_*` do not resolve.
        assert_eq!(
            IntMethod::from_name("to_u8"),
            Some(IntMethod::Convert {
                signed: false,
                bits: 8
            })
        );
        assert_eq!(
            IntMethod::from_name("to_int"),
            IntMethod::from_name("to_i64")
        );
        assert_eq!(IntMethod::from_name("to_u7"), None);
        assert_eq!(IntMethod::from_name("to_bytes"), None);
        assert_eq!(
            IntMethod::Convert {
                signed: false,
                bits: 8
            }
            .arity(),
            0
        );

        // Computation matches `x as T`: widen (safe), narrow (truncate), cross-signedness (reinterpret).
        let u8 = IntMethod::Convert {
            signed: false,
            bits: 8,
        };
        let i8 = IntMethod::Convert {
            signed: true,
            bits: 8,
        };
        let i32 = IntMethod::Convert {
            signed: true,
            bits: 32,
        };
        assert_eq!(int_method(200, u8, 0), 200); // 200u8 as u8
        assert_eq!(int_method(300, u8, 0), 44); // 300 as u8 (truncate)
        assert_eq!(int_method(200, i8, 0), -56); // 200u8 as i8 (reinterpret)
        assert_eq!(int_method(-56, u8, 0), 200); // -56i8 as u8 (reinterpret)
        assert_eq!(int_method(4000000000, i32, 0), -294967296); // 4e9u32 as i32
    }

    #[test]
    fn width_exact_intrinsics_operate_within_the_width() {
        use IntMethod::*;
        // count within the width (a signed negative's high i64 bits do not leak in).
        assert_eq!(int_method_width(0b1011, CountOnes, 0, 8), 3);
        assert_eq!(int_method_width(-1, CountOnes, 0, 8), 8); // i8 -1 = 0xFF
        assert_eq!(int_method_width(0, CountZeros, 0, 8), 8); // all 8 bits clear
        assert_eq!(int_method_width(0xFF, CountZeros, 0, 8), 0);
        // leading/trailing relative to the width, not to 64.
        assert_eq!(int_method_width(1, LeadingZeros, 0, 8), 7);
        assert_eq!(int_method_width(0, LeadingZeros, 0, 8), 8);
        assert_eq!(int_method_width(0, TrailingZeros, 0, 8), 8);
        assert_eq!(int_method_width(8, TrailingZeros, 0, 8), 3);
        // rotate wraps within the width: 0x80 (top bit of u8) rotate_left 1 -> 0x01.
        assert_eq!(int_method_width(0x80, RotateLeft, 1, 8), 1);
        assert_eq!(int_method_width(1, RotateRight, 1, 8), 0x80);
        // reverse / swap within the width.
        assert_eq!(int_method_width(1, ReverseBits, 0, 8), 0x80);
        assert_eq!(int_method_width(0x0102, SwapBytes, 0, 16), 0x0201);
        assert_eq!(int_method_width(0x05, SwapBytes, 0, 8), 0x05); // one byte: identity
        // bits >= 64 is exactly the full-width `int_method`.
        assert_eq!(int_method_width(1, LeadingZeros, 0, 64), 63);
        assert_eq!(
            int_method_width(0x0102, SwapBytes, 0, 64),
            int_method(0x0102, SwapBytes, 0)
        );
    }

    #[test]
    fn mask_to_width_wraps_each_width() {
        // Unsigned: zero the high bits (`255u8 + 1 → 0`, `200 + 100 → 44`).
        assert_eq!(mask_to_width(256, false, 8), 0);
        assert_eq!(mask_to_width(300, false, 8), 44);
        assert_eq!(mask_to_width(255, false, 8), 255);
        assert_eq!(mask_to_width(65536, false, 16), 0);
        // Signed: sign-extend from the width's high bit (`128 → -128` for i8, `300 → 44`).
        assert_eq!(mask_to_width(128, true, 8), -128);
        assert_eq!(mask_to_width(300, true, 8), 44);
        assert_eq!(mask_to_width(127, true, 8), 127);
        assert_eq!(mask_to_width(-1, true, 8), -1);
        // `-(-128i8)` overflows back to `-128i8`.
        assert_eq!(mask_to_width(-mask_to_width(128, true, 8), true, 8), -128);
        // 64-bit is a no-op: the word already is the pattern (and avoids `1 << 64`).
        assert_eq!(mask_to_width(-1, false, 64), -1);
        assert_eq!(mask_to_width(i64::MIN, true, 64), i64::MIN);
    }

    #[test]
    fn num_convert_crosses_domains_like_rust_as() {
        use NumConvert::*;
        use NumScalar::*;
        // int → float / f32
        assert_eq!(num_convert(Int(5), ToF64), F64(5.0));
        assert_eq!(num_convert(Int(5), ToF32), F32(5.0));
        assert_eq!(num_convert(Int(-3), ToF32), F32(-3.0));
        // f32 ↔ f64
        assert_eq!(num_convert(F32(2.5), ToF64), F64(2.5));
        assert_eq!(num_convert(F64(2.5), ToF32), F32(2.5));
        // float → int truncates toward zero
        assert_eq!(
            num_convert(
                F64(3.9),
                ToInt {
                    signed: true,
                    bits: 64
                }
            ),
            Int(3)
        );
        assert_eq!(
            num_convert(
                F64(-3.9),
                ToInt {
                    signed: true,
                    bits: 64
                }
            ),
            Int(-3)
        );
        assert_eq!(
            num_convert(
                F32(3.9),
                ToInt {
                    signed: true,
                    bits: 64
                }
            ),
            Int(3)
        );
        // float → int SATURATES to the destination width (not wrapping), NaN → 0
        assert_eq!(
            num_convert(
                F64(1000.0),
                ToInt {
                    signed: false,
                    bits: 8
                }
            ),
            Int(255)
        );
        assert_eq!(
            num_convert(
                F64(-1.0),
                ToInt {
                    signed: false,
                    bits: 8
                }
            ),
            Int(0)
        );
        assert_eq!(
            num_convert(
                F64(f64::NAN),
                ToInt {
                    signed: true,
                    bits: 32
                }
            ),
            Int(0)
        );
        // int → int stays bit-preserving (wrapping mask), matching the IntMethod::Convert path
        assert_eq!(
            num_convert(
                Int(300),
                ToInt {
                    signed: false,
                    bits: 8
                }
            ),
            Int(44)
        );
    }

    #[test]
    fn num_convert_decodes_names() {
        use NumConvert::*;
        assert_eq!(NumConvert::from_name("to_float"), Some(ToF64));
        assert_eq!(NumConvert::from_name("to_f64"), Some(ToF64));
        assert_eq!(NumConvert::from_name("to_f32"), Some(ToF32));
        assert_eq!(
            NumConvert::from_name("to_int"),
            Some(ToInt {
                signed: true,
                bits: 64
            })
        );
        assert_eq!(
            NumConvert::from_name("to_u8"),
            Some(ToInt {
                signed: false,
                bits: 8
            })
        );
        assert_eq!(NumConvert::from_name("count_ones"), None);
        assert_eq!(NumConvert::from_name("upper"), None);
    }

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
