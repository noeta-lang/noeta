//! `std.metrics` — the metrics SDK surface (native OTEL, Phase M).
//!
//! A facade over the [`Metrics`](noeta_ext_abi::Metrics) Host capability. Instruments are
//! **long-lived and host-owned**: a constructor (`counter`/`up_down_counter`/`histogram`/`gauge`) is
//! get-or-create by name, returning an extern handle (`Counter`/`Histogram`/`Gauge`) that wraps the
//! opaque [`InstrumentId`], exactly as `Span` wraps a `SpanId`. Aggregation (sums, histogram buckets)
//! lives entirely host-side; the handle's methods only marshal a measurement to the host.
//!
//! - Constructors reach host state (get-or-create), so they are **ctx functions** (like the tracing
//!   surface); the instrument methods only touch the host, so they are **plain dispatch** (like the
//!   `Span` mutators).
//! - `Counter.add(n)` / `.add_with(n, attrs)`; `Histogram`/`Gauge` `.record(v)` / `.record_with(v,
//!   attrs)`. The `*_with` split carries a `Map<string, string|int|float|bool>` of attributes,
//!   mirroring `std.log` (and sidestepping optional-argument methods).
//!
//! **Three distinct types** — `Counter` (also the up/down counter), `Histogram`, `Gauge` — each a
//! namespaced extern type under `std.metrics`, so `.add` sits only on counters and `.record` only on
//! histograms/gauges (compile-time method-typing). Namespacing (extern types are `use`-imported, not
//! globally reserved) is what makes the idiomatic OTel names viable: a native `std.metrics.Counter`
//! coexists with a user's own `Counter`. Users bring them in with `use std.metrics.{Counter, …}`.

use std::any::Any;
use std::cmp::Ordering;

use noeta_ext_abi::registry::{ExtFn, NativeOut, RetTy, SigType};
use noeta_ext_abi::{
    AttrValue, CtxError, CtxOut, CtxResult, ExternBox, ExternValue, Host, InstrumentId,
    InstrumentKind, MetricValue, NativeCtx, NativeValue, Scalar, Slot, StdError, arity_error,
    ctx_arity, no_function_error, no_method_error, type_error,
};

/// The surface type names for the three instrument handles. Namespaced under `std.metrics` (see the
/// registry), so they are `use`-imported, not globally reserved.
pub const COUNTER_TYPE_NAME: &str = "Counter";
pub const HISTOGRAM_TYPE_NAME: &str = "Histogram";
pub const GAUGE_TYPE_NAME: &str = "Gauge";

/// The instruments' qualified runtime identities — what
/// [`crate::ExternValue::type_identity`] returns. These short names are exactly the ones a
/// third-party metrics extension is most likely to reuse; the qualified identity is what keeps
/// `std.metrics.Counter` and an `acme.metrics.Counter` distinct at runtime.
pub const COUNTER_TYPE_IDENTITY: &str = "std.metrics.Counter";
pub const HISTOGRAM_TYPE_IDENTITY: &str = "std.metrics.Histogram";
pub const GAUGE_TYPE_IDENTITY: &str = "std.metrics.Gauge";

/// A measurement value — `int` or `float` (a non-numeric is a compile-time type error).
const NUM_VALUE: SigType = SigType::Union(&[SigType::Int, SigType::Float]);

/// The scalar attribute value union (shared with `std.log`/`std.tracing`).
const ATTR_VALUE: SigType =
    SigType::Union(&[SigType::String, SigType::Int, SigType::Float, SigType::Bool]);

/// The structured-attributes parameter — `Map<string, string|int|float|bool>` (the `*_with` forms).
const ATTR_MAP: SigType = SigType::Map(&SigType::String, &ATTR_VALUE);

/// `std.metrics`'s instrument constructors — all ctx functions (get-or-create touches host state),
/// each returning its instrument's handle type.
pub const METRICS_CTX_FNS: &[ExtFn] = &[
    ExtFn {
        name: "counter",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Named(COUNTER_TYPE_NAME)),
    },
    ExtFn {
        name: "up_down_counter",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Named(COUNTER_TYPE_NAME)),
    },
    ExtFn {
        name: "histogram",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Named(HISTOGRAM_TYPE_NAME)),
    },
    ExtFn {
        name: "gauge",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Named(GAUGE_TYPE_NAME)),
    },
];

/// `Counter` methods (Counter + UpDownCounter): `add`, and `add_with` for attributed measurements.
pub const COUNTER_METHODS: &[ExtFn] = &[
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
];

/// `Histogram`/`Gauge` methods: `record`, and `record_with` for attributed measurements.
pub const RECORD_METHODS: &[ExtFn] = &[
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

/// Declare an instrument handle extern type — the host's opaque [`InstrumentId`] (plain `Copy` data).
/// Methods reach the host by id; the aggregation state (and the kind) live host-side. Not key-capable.
macro_rules! instrument_handle {
    ($ty:ident, $name:expr, $identity:expr) => {
        #[derive(Debug, Clone, Copy, PartialEq)]
        pub struct $ty {
            pub id: InstrumentId,
        }

        impl ExternValue for $ty {
            fn type_identity(&self) -> &'static str {
                $identity
            }
            fn eq_value(&self, other: &dyn ExternValue) -> bool {
                other.as_any().downcast_ref::<$ty>() == Some(self)
            }
            fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
                None
            }
            fn hash_value(&self) -> u64 {
                0 // not key-capable; never consulted
            }
            fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
                write!(out, "<{} {}>", $name, self.id)
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
    };
}

instrument_handle!(Counter, COUNTER_TYPE_NAME, COUNTER_TYPE_IDENTITY);
instrument_handle!(Histogram, HISTOGRAM_TYPE_NAME, HISTOGRAM_TYPE_IDENTITY);
instrument_handle!(Gauge, GAUGE_TYPE_NAME, GAUGE_TYPE_IDENTITY);

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
    let out = match kind {
        InstrumentKind::Counter | InstrumentKind::UpDownCounter => {
            NativeOut::Extern(ExternBox::new(Counter { id }))
        }
        InstrumentKind::Histogram => NativeOut::Extern(ExternBox::new(Histogram { id })),
        InstrumentKind::Gauge => NativeOut::Extern(ExternBox::new(Gauge { id })),
    };
    Ok(CtxOut::Out(out))
}

/// `Counter` method dispatch — `add` / `add_with` (plain: the host owns aggregation).
pub fn counter_method_dispatch(
    recv: &mut dyn ExternValue,
    method: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let id = downcast::<Counter>(recv);
    match method {
        "add" => observe(host, id, method, args, false),
        "add_with" => observe(host, id, method, args, true),
        _ => Err(no_method_error(COUNTER_TYPE_NAME, method)),
    }
}

/// `Histogram` method dispatch — `record` / `record_with`.
pub fn histogram_method_dispatch(
    recv: &mut dyn ExternValue,
    method: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    record_dispatch(
        downcast::<Histogram>(recv),
        HISTOGRAM_TYPE_NAME,
        method,
        host,
        args,
    )
}

/// `Gauge` method dispatch — `record` / `record_with`.
pub fn gauge_method_dispatch(
    recv: &mut dyn ExternValue,
    method: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    record_dispatch(downcast::<Gauge>(recv), GAUGE_TYPE_NAME, method, host, args)
}

fn record_dispatch(
    id: InstrumentId,
    type_name: &'static str,
    method: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match method {
        "record" => observe(host, id, method, args, false),
        "record_with" => observe(host, id, method, args, true),
        _ => Err(no_method_error(type_name, method)),
    }
}

/// The [`InstrumentId`] inside any instrument handle (they share the `.id` field).
fn downcast<T: ExternValue + HasInstrumentId + 'static>(recv: &dyn ExternValue) -> InstrumentId {
    recv.as_any()
        .downcast_ref::<T>()
        .expect("instrument method dispatch on the wrong extern")
        .instrument_id()
}

trait HasInstrumentId {
    fn instrument_id(&self) -> InstrumentId;
}
impl HasInstrumentId for Counter {
    fn instrument_id(&self) -> InstrumentId {
        self.id
    }
}
impl HasInstrumentId for Histogram {
    fn instrument_id(&self) -> InstrumentId {
        self.id
    }
}
impl HasInstrumentId for Gauge {
    fn instrument_id(&self) -> InstrumentId {
        self.id
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
