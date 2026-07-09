//! `std.metrics` — the metrics SDK surface (native OTEL, Phase M).
//!
//! A facade over the [`Metrics`](noeta_native::Metrics) Host capability. Instruments are
//! **long-lived and host-owned**: a constructor (`counter`/`up_down_counter`/`histogram`/`gauge`) is
//! get-or-create by name, returning an extern handle (`Counter`/`Histogram`/`Gauge`) that wraps the
//! opaque [`InstrumentId`], exactly as `Span` wraps a `SpanId`. Aggregation (sums, histogram buckets)
//! lives entirely host-side; the handle's methods only marshal a measurement to the host.
//!
//! - Constructors reach host state (get-or-create), so they are **ctx functions** (like the tracing
//!   surface); the instrument methods only touch the host, so they are **plain dispatch** (like the
//!   `Span` mutators).
//! - `.add(n)` / `.add_with(n, attrs)` and `.record(v)` / `.record_with(v, attrs)`. The `*_with`
//!   split carries a `Map<string, string|int|float|bool>` of attributes, mirroring `std.log` (and
//!   sidestepping optional-argument methods).
//!
//! **One `Instrument` type, not three.** The four constructors all return a single [`Instrument`]
//! handle rather than distinct `Counter`/`Histogram`/`Gauge` types. Distinct types would have to
//! reserve those names (extern-type dispatch is by name → E0049), and `Counter`/`Gauge`/`Histogram`
//! are far too common as user type names to reserve (they collide across the corpus and real code);
//! `Instrument` is safe to reserve. The behavior is the instrument's **kind** (set at construction),
//! not the method name: `.add`/`.record` are interchangeable aliases that both `observe`, dispatched
//! host-side on the kind (a `counter` sums, a `gauge` takes the last value, …). Users write the
//! idiomatic verb (`counter.add`, `histogram.record`); the type system does not distinguish them.

use std::any::Any;
use std::cmp::Ordering;

use noeta_native::registry::{ExtFn, NativeOut, RetTy, SigType};
use noeta_native::{
    arity_error, ctx_arity, no_function_error, no_method_error, type_error, AttrValue, CtxError,
    CtxOut, CtxResult, ExternBox, ExternValue, Host, InstrumentId, InstrumentKind, MetricValue,
    NativeCtx, NativeValue, Scalar, Slot, StdError,
};

/// The reserved surface type name for the single instrument handle (declaring your own is E0049).
pub const INSTRUMENT_TYPE_NAME: &str = "Instrument";

/// A measurement value — `int` or `float` (a non-numeric is a compile-time type error).
const NUM_VALUE: SigType = SigType::Union(&[SigType::Int, SigType::Float]);

/// The scalar attribute value union (shared with `std.log`/`std.tracing`).
const ATTR_VALUE: SigType = SigType::Union(&[
    SigType::String,
    SigType::Int,
    SigType::Float,
    SigType::Bool,
]);

/// The structured-attributes parameter — `Map<string, string|int|float|bool>` (the `*_with` forms).
const ATTR_MAP: SigType = SigType::Map(&SigType::String, &ATTR_VALUE);

/// `std.metrics`'s instrument constructors — all ctx functions (get-or-create touches host state),
/// each returning the single `Instrument` handle (its kind is fixed at construction).
pub const METRICS_CTX_FNS: &[ExtFn] = &[
    ExtFn {
        name: "counter",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Named(INSTRUMENT_TYPE_NAME)),
    },
    ExtFn {
        name: "up_down_counter",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Named(INSTRUMENT_TYPE_NAME)),
    },
    ExtFn {
        name: "histogram",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Named(INSTRUMENT_TYPE_NAME)),
    },
    ExtFn {
        name: "gauge",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Named(INSTRUMENT_TYPE_NAME)),
    },
];

/// `Instrument` methods — `add`/`record` (interchangeable, kind-dispatched host-side) and their
/// `*_with` attributed forms. `add` reads best for counters, `record` for histograms/gauges.
pub const INSTRUMENT_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "add",
        params: &[NUM_VALUE],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "add_with",
        params: &[NUM_VALUE, ATTR_MAP],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "record",
        params: &[NUM_VALUE],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "record_with",
        params: &[NUM_VALUE, ATTR_MAP],
        ret: RetTy::Concrete(SigType::Unit),
    },
];

/// An instrument handle: the host's opaque [`InstrumentId`] (plain `Copy` data). Methods reach the
/// host by id; the aggregation state (and the instrument's kind) live host-side, so a handle passed
/// around refers to one host-side instrument. Not key-capable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Instrument {
    pub id: InstrumentId,
}

impl ExternValue for Instrument {
    fn type_name(&self) -> &'static str {
        INSTRUMENT_TYPE_NAME
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<Instrument>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable; never consulted
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<{INSTRUMENT_TYPE_NAME} {}>", self.id)
    }
    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(*self)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// `std.metrics` ctx dispatch — the instrument constructors (get-or-create). Generic over the ctx so
/// a compiled-in backend inlines the small ctx ops.
pub fn metrics_ctx_dispatch<C: NativeCtx + ?Sized>(
    func: &str,
    ctx: &mut C,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    let kind = match func {
        "counter" => InstrumentKind::Counter,
        "up_down_counter" => InstrumentKind::UpDownCounter,
        "histogram" => InstrumentKind::Histogram,
        "gauge" => InstrumentKind::Gauge,
        _ => return Err(no_function_error("metrics", func).into()),
    };
    ctx_arity(func, args, 1)?;
    let name = slot_str(ctx, args[0])?;
    // Unit is unset in this surface (`counter(name)`); a `counter(name, unit)` overload is future.
    let id = ctx.host().metric_get_or_create(&name, "", kind);
    Ok(CtxOut::Out(NativeOut::Extern(ExternBox::new(Instrument { id }))))
}

/// `Instrument` method dispatch — `add`/`record` (+ `*_with`), all marshalling one measurement to
/// the host, which aggregates by the instrument's kind. Plain dispatch (the host owns aggregation).
pub fn instrument_method_dispatch(
    recv: &mut dyn ExternValue,
    method: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let id = recv
        .as_any()
        .downcast_ref::<Instrument>()
        .expect("instrument method dispatch on a non-Instrument extern")
        .id;
    match method {
        "add" | "record" => observe(host, id, method, args, false),
        "add_with" | "record_with" => observe(host, id, method, args, true),
        _ => Err(no_method_error(INSTRUMENT_TYPE_NAME, method)),
    }
}

/// Marshal a measurement (value + optional attributes) to the host.
fn observe(
    host: &mut dyn Host,
    id: InstrumentId,
    method: &str,
    args: &[NativeValue],
    with_attrs: bool,
) -> Result<NativeOut, StdError> {
    // Free when metrics are disabled (real host, no endpoint): no aggregation, no unbounded store
    // growth for a program that records into instruments it never exports. The sandbox recorder is
    // always enabled, so the parity oracle still observes every measurement.
    if !host.tel_metrics_enabled() {
        return Ok(NativeOut::Unit);
    }
    let expected = if with_attrs { 2 } else { 1 };
    if args.len() != expected {
        return Err(arity_error(method, expected, args.len()));
    }
    let value = want_num(method, &args[0])?;
    let attrs = if with_attrs {
        want_attrs(method, &args[1])?
    } else {
        Vec::new()
    };
    host.metric_observe(id, value, attrs);
    Ok(NativeOut::Unit)
}

fn want_num(method: &str, arg: &NativeValue) -> Result<MetricValue, StdError> {
    match arg {
        NativeValue::Scalar(Scalar::Int(n)) => Ok(MetricValue::Int(*n)),
        NativeValue::Scalar(Scalar::Float(f)) => Ok(MetricValue::Float(*f)),
        NativeValue::Scalar(Scalar::F32(f)) => Ok(MetricValue::Float(*f as f64)),
        _ => Err(type_error(method, "int or float")),
    }
}

fn want_attrs(
    method: &str,
    arg: &NativeValue,
) -> Result<Vec<(compact_str::CompactString, AttrValue)>, StdError> {
    match arg {
        NativeValue::Map(entries) => entries
            .iter()
            .map(|(k, v)| Ok((k.as_str().into(), attr_from_native(method, v)?)))
            .collect(),
        _ => Err(type_error(method, "map")),
    }
}

fn attr_from_native(method: &str, v: &NativeValue) -> Result<AttrValue, StdError> {
    match v {
        NativeValue::Str(s) => Ok(AttrValue::Str(s.as_str().into())),
        NativeValue::Scalar(Scalar::Int(i)) => Ok(AttrValue::Int(*i)),
        NativeValue::Scalar(Scalar::Float(f)) => Ok(AttrValue::Float(*f)),
        NativeValue::Scalar(Scalar::F32(f)) => Ok(AttrValue::Float(*f as f64)),
        NativeValue::Scalar(Scalar::Bool(b)) => Ok(AttrValue::Bool(*b)),
        _ => Err(type_error(method, "string, int, float, or bool")),
    }
}

fn slot_str<C: NativeCtx + ?Sized>(ctx: &mut C, slot: Slot) -> CtxResult<String> {
    match ctx.view(slot)? {
        NativeValue::Str(s) => Ok(s),
        _ => Err(type_error("metrics", "string").into()),
    }
}
