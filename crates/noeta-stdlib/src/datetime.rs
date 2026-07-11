//! `std.datetime` — calendar/timezone datetime (the Ring 3 datetime arc), backed by `jiff`.
//!
//! The lightweight `time` module (monotonic/sleep/`unix_ms`) stays always-on in `CoreExtension`;
//! this is the heavy, DST-correct calendar layer, gated behind the default-on `ring-datetime`
//! feature (jiff + the IANA tzdb) so a footprint-tailored build sheds it.
//!
//! Three extern value types, all wrapping jiff:
//! - [`Instant`] — an absolute moment (a `jiff::Timestamp`), timezone-independent.
//! - [`Zoned`] — a timezone-aware civil datetime (a `jiff::Zoned`).
//! - [`Duration`] — a span of time (a `jiff::Span`), fed to the arithmetic methods.
//!
//! Everything here is **pure** except `datetime.now()`, which reads the host `Clock` capability
//! (`clock_unix_ms`) — deterministic in the sandbox (fixed epoch + logical clock), so the whole
//! surface is differential-identical across backends by construction.

use crate::{ErrorKind, ExternBox, ExternValue, Host, StdError};
use jiff::{Span, Timestamp, Zoned as JiffZoned, tz::TimeZone};
use noeta_native::registry::{
    ExtFn, ExtModule, ExtType, Extension, NativeOut, NativeValue, RetTy, Scalar, SigType,
};
use std::any::Any;
use std::cmp::Ordering;

pub const INSTANT_TYPE_NAME: &str = "Instant";
pub const ZONED_TYPE_NAME: &str = "Zoned";
pub const DURATION_TYPE_NAME: &str = "Duration";

// --- The three extern value types ---------------------------------------------------------------

/// An absolute moment in time (`jiff::Timestamp`) — timezone-independent, ordered chronologically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instant(pub Timestamp);

/// A timezone-aware civil datetime (`jiff::Zoned`): a moment resolved into a wall-clock date/time
/// in a specific IANA zone, with DST-correct field access and arithmetic.
#[derive(Debug, Clone, PartialEq)]
pub struct Zoned(pub JiffZoned);

/// A span of time (`jiff::Span`) — the argument to `add`/`sub`, and the result of `diff`. Displays
/// as an ISO-8601 duration (`PT1H30M`, `P2DT3H`).
#[derive(Debug, Clone)]
pub struct Duration(pub Span);

impl ExternValue for Instant {
    fn type_name(&self) -> &'static str {
        INSTANT_TYPE_NAME
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<Instant>() == Some(self)
    }
    fn cmp_value(&self, other: &dyn ExternValue) -> Option<Ordering> {
        other
            .as_any()
            .downcast_ref::<Instant>()
            .map(|o| self.0.cmp(&o.0))
    }
    fn hash_value(&self) -> u64 {
        0
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "{}", self.0)
    }
    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl ExternValue for Zoned {
    fn type_name(&self) -> &'static str {
        ZONED_TYPE_NAME
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<Zoned>() == Some(self)
    }
    fn cmp_value(&self, other: &dyn ExternValue) -> Option<Ordering> {
        other
            .as_any()
            .downcast_ref::<Zoned>()
            .map(|o| self.0.timestamp().cmp(&o.0.timestamp()))
    }
    fn hash_value(&self) -> u64 {
        0
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "{}", self.0)
    }
    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl ExternValue for Duration {
    fn type_name(&self) -> &'static str {
        DURATION_TYPE_NAME
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        // `Span` intentionally has no `PartialEq` (equality is ambiguous for calendar units); its
        // `fieldwise` view compares the stored fields, which is the useful "same duration" test.
        other
            .as_any()
            .downcast_ref::<Duration>()
            .is_some_and(|o| self.0.fieldwise() == o.0.fieldwise())
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None // a calendar span has no total order without a reference date
    }
    fn hash_value(&self) -> u64 {
        0
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "{}", self.0)
    }
    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

// --- Errors -------------------------------------------------------------------------------------

fn dt_error(message: impl Into<String>) -> StdError {
    StdError {
        kind: ErrorKind::Io,
        message: message.into(),
    }
}

// --- Argument helpers ---------------------------------------------------------------------------

fn want_arity(func: &str, args: &[NativeValue], n: usize) -> Result<(), StdError> {
    if args.len() == n {
        Ok(())
    } else {
        Err(crate::arity_error(func, n, args.len()))
    }
}

fn want_int(func: &str, args: &[NativeValue], i: usize) -> Result<i64, StdError> {
    match args.get(i) {
        Some(NativeValue::Scalar(Scalar::Int(n))) => Ok(*n),
        _ => Err(crate::type_error(func, "int")),
    }
}

fn want_str<'a>(func: &str, args: &'a [NativeValue], i: usize) -> Result<&'a str, StdError> {
    match args.get(i) {
        Some(NativeValue::Str(s)) => Ok(s),
        _ => Err(crate::type_error(func, "string")),
    }
}

/// Downcast an extern-value argument to a concrete datetime type (`Instant`/`Duration`/…).
fn want_extern<'a, T: 'static>(
    func: &str,
    args: &'a [NativeValue],
    i: usize,
    noun: &str,
) -> Result<&'a T, StdError> {
    match args.get(i) {
        Some(NativeValue::Extern(b)) => {
            b.0.as_any()
                .downcast_ref::<T>()
                .ok_or_else(|| crate::type_error(func, noun))
        }
        _ => Err(crate::type_error(func, noun)),
    }
}

/// Reject a `Duration` carrying calendar units (days/weeks/months/years) on an `Instant`: an
/// absolute moment has no timezone, so a "day" is not a fixed length (23–25 h across DST). Those
/// units are only meaningful on a `Zoned`. Time units (hours and smaller) are fine on either.
fn require_time_only(span: Span) -> Result<(), StdError> {
    let has_calendar = span.get_years() != 0
        || span.get_months() != 0
        || span.get_weeks() != 0
        || span.get_days() != 0;
    if has_calendar {
        return Err(dt_error(
            "an Instant supports only time-based durations (seconds/minutes/hours); for calendar \
             units like days or months, resolve a zone first: instant.in_zone(tz).add(...)",
        ));
    }
    Ok(())
}

fn instant_out(ts: Timestamp) -> NativeOut {
    NativeOut::Extern(ExternBox::new(Instant(ts)))
}

fn zoned_out(z: JiffZoned) -> NativeOut {
    NativeOut::Extern(ExternBox::new(Zoned(z)))
}

fn duration_out(span: Span) -> NativeOut {
    NativeOut::Extern(ExternBox::new(Duration(span)))
}

// --- Module dispatch: `datetime.<fn>()` ---------------------------------------------------------

fn datetime_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    // A duration constructor `datetime.<unit>(n)` — one integer, one `Span`.
    if let Some(span_builder) = duration_unit(func) {
        want_arity(func, args, 1)?;
        let n = want_int(func, args, 0)?;
        return Ok(duration_out(
            span_builder(n).map_err(|e| dt_error(e.to_string()))?,
        ));
    }
    match func {
        // The current instant, from the host clock (deterministic in the sandbox).
        "now" => {
            want_arity(func, args, 0)?;
            let ms = host.clock_unix_ms() as i64;
            Ok(instant_out(
                Timestamp::from_millisecond(ms).map_err(|e| dt_error(e.to_string()))?,
            ))
        }
        "from_unix_ms" => {
            want_arity(func, args, 1)?;
            let ms = want_int(func, args, 0)?;
            Ok(instant_out(
                Timestamp::from_millisecond(ms).map_err(|e| dt_error(e.to_string()))?,
            ))
        }
        // Parse an RFC-3339 / ISO-8601 instant; `none` on malformed input (the safe-probe style).
        "parse" => {
            want_arity(func, args, 1)?;
            let s = want_str(func, args, 0)?;
            Ok(match s.parse::<Timestamp>() {
                Ok(ts) => NativeOut::Some(Box::new(instant_out(ts))),
                Err(_) => NativeOut::None,
            })
        }
        _ => Err(crate::no_function_error("datetime", func)),
    }
}

/// The duration-constructor unit functions: each takes an integer count and builds a single-unit
/// `Span`. A count outside jiff's supported range is an error (surfaced from the builder).
fn duration_unit(func: &str) -> Option<fn(i64) -> Result<Span, jiff::Error>> {
    match func {
        "seconds" => Some(|n| Span::new().try_seconds(n)),
        "minutes" => Some(|n| Span::new().try_minutes(n)),
        "hours" => Some(|n| Span::new().try_hours(n)),
        "days" => Some(|n| Span::new().try_days(n)),
        "weeks" => Some(|n| Span::new().try_weeks(n)),
        "months" => Some(|n| Span::new().try_months(n)),
        "years" => Some(|n| Span::new().try_years(n)),
        _ => None,
    }
}

// --- `Instant` methods --------------------------------------------------------------------------

fn instant_method_dispatch(
    recv: &mut dyn ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(Instant(ts)) = recv.as_any().downcast_ref::<Instant>() else {
        return Err(crate::type_error(method, "Instant"));
    };
    let ts = *ts;
    match method {
        "unix_ms" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Int(ts.as_millisecond())))
        }
        // strftime formatting in UTC (a bare instant has no local zone).
        "format" => {
            want_arity(method, args, 1)?;
            let fmt = want_str(method, args, 0)?;
            let utc = ts.to_zoned(TimeZone::UTC);
            jiff::fmt::strtime::format(fmt, &utc)
                .map(NativeOut::Str)
                .map_err(|e| dt_error(format!("invalid datetime format: {e}")))
        }
        // Resolve into a named IANA timezone; an unknown zone is an error.
        "in_zone" => {
            want_arity(method, args, 1)?;
            let tz = want_str(method, args, 0)?;
            ts.in_tz(tz)
                .map(zoned_out)
                .map_err(|e| dt_error(format!("unknown timezone `{tz}`: {e}")))
        }
        "add" => {
            want_arity(method, args, 1)?;
            let Duration(span) = want_extern::<Duration>(method, args, 0, "Duration")?;
            require_time_only(*span)?;
            ts.checked_add(*span)
                .map(instant_out)
                .map_err(|e| dt_error(format!("datetime overflow: {e}")))
        }
        "sub" => {
            want_arity(method, args, 1)?;
            let Duration(span) = want_extern::<Duration>(method, args, 0, "Duration")?;
            require_time_only(*span)?;
            ts.checked_sub(*span)
                .map(instant_out)
                .map_err(|e| dt_error(format!("datetime overflow: {e}")))
        }
        // The span **from** `self` **to** `other` — positive when `other` is later. Balanced with
        // hours as the largest unit (an absolute instant has no calendar zone, so no days), which
        // reads far better than jiff's default all-seconds span (`PT3H`, not `PT10800S`).
        "diff" => {
            want_arity(method, args, 1)?;
            let Instant(other) = want_extern::<Instant>(method, args, 0, "Instant")?;
            ts.until((jiff::Unit::Hour, *other))
                .map(duration_out)
                .map_err(|e| dt_error(e.to_string()))
        }
        "is_before" => {
            want_arity(method, args, 1)?;
            let Instant(other) = want_extern::<Instant>(method, args, 0, "Instant")?;
            Ok(NativeOut::Scalar(Scalar::Bool(ts < *other)))
        }
        "is_after" => {
            want_arity(method, args, 1)?;
            let Instant(other) = want_extern::<Instant>(method, args, 0, "Instant")?;
            Ok(NativeOut::Scalar(Scalar::Bool(ts > *other)))
        }
        _ => Err(crate::no_method_error(INSTANT_TYPE_NAME, method)),
    }
}

// --- `Zoned` methods ----------------------------------------------------------------------------

fn zoned_method_dispatch(
    recv: &mut dyn ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(Zoned(z)) = recv.as_any().downcast_ref::<Zoned>() else {
        return Err(crate::type_error(method, "Zoned"));
    };
    let int = |n: i64| Ok(NativeOut::Scalar(Scalar::Int(n)));
    match method {
        "year" => {
            want_arity(method, args, 0)?;
            int(i64::from(z.year()))
        }
        "month" => {
            want_arity(method, args, 0)?;
            int(i64::from(z.month()))
        }
        "day" => {
            want_arity(method, args, 0)?;
            int(i64::from(z.day()))
        }
        "hour" => {
            want_arity(method, args, 0)?;
            int(i64::from(z.hour()))
        }
        "minute" => {
            want_arity(method, args, 0)?;
            int(i64::from(z.minute()))
        }
        "second" => {
            want_arity(method, args, 0)?;
            int(i64::from(z.second()))
        }
        // ISO weekday: 1 = Monday … 7 = Sunday.
        "weekday" => {
            want_arity(method, args, 0)?;
            int(i64::from(z.weekday().to_monday_one_offset()))
        }
        "zone" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Str(
                z.time_zone().iana_name().unwrap_or("UTC").to_string(),
            ))
        }
        "format" => {
            want_arity(method, args, 1)?;
            let fmt = want_str(method, args, 0)?;
            jiff::fmt::strtime::format(fmt, z)
                .map(NativeOut::Str)
                .map_err(|e| dt_error(format!("invalid datetime format: {e}")))
        }
        "to_instant" => {
            want_arity(method, args, 0)?;
            Ok(instant_out(z.timestamp()))
        }
        // DST-correct calendar arithmetic.
        "add" => {
            want_arity(method, args, 1)?;
            let Duration(span) = want_extern::<Duration>(method, args, 0, "Duration")?;
            z.checked_add(*span)
                .map(zoned_out)
                .map_err(|e| dt_error(format!("datetime overflow: {e}")))
        }
        "sub" => {
            want_arity(method, args, 1)?;
            let Duration(span) = want_extern::<Duration>(method, args, 0, "Duration")?;
            z.checked_sub(*span)
                .map(zoned_out)
                .map_err(|e| dt_error(format!("datetime overflow: {e}")))
        }
        "is_before" => {
            want_arity(method, args, 1)?;
            let Zoned(other) = want_extern::<Zoned>(method, args, 0, "Zoned")?;
            Ok(NativeOut::Scalar(Scalar::Bool(
                z.timestamp() < other.timestamp(),
            )))
        }
        "is_after" => {
            want_arity(method, args, 1)?;
            let Zoned(other) = want_extern::<Zoned>(method, args, 0, "Zoned")?;
            Ok(NativeOut::Scalar(Scalar::Bool(
                z.timestamp() > other.timestamp(),
            )))
        }
        _ => Err(crate::no_method_error(ZONED_TYPE_NAME, method)),
    }
}

// --- `Duration` methods -------------------------------------------------------------------------

fn duration_method_dispatch(
    recv: &mut dyn ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(Duration(span)) = recv.as_any().downcast_ref::<Duration>() else {
        return Err(crate::type_error(method, "Duration"));
    };
    match method {
        "to_string" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Str(span.to_string()))
        }
        _ => Err(crate::no_method_error(DURATION_TYPE_NAME, method)),
    }
}

// --- Registry tables ----------------------------------------------------------------------------

const INSTANT_SIG: SigType = SigType::Named(INSTANT_TYPE_NAME);
const ZONED_SIG: SigType = SigType::Named(ZONED_TYPE_NAME);
const DURATION_SIG: SigType = SigType::Named(DURATION_TYPE_NAME);

use RetTy::Concrete;
use SigType::{Int, String as Str};

/// One duration-constructor signature `<unit>(n: int) -> Duration`.
const fn dur_fn(name: &'static str) -> ExtFn {
    ExtFn {
        name,
        params: &[Int],
        ret: Concrete(DURATION_SIG),
    }
}

const DATETIME_FNS: &[ExtFn] = &[
    ExtFn {
        name: "now",
        params: &[],
        ret: Concrete(INSTANT_SIG),
    },
    ExtFn {
        name: "from_unix_ms",
        params: &[Int],
        ret: Concrete(INSTANT_SIG),
    },
    ExtFn {
        name: "parse",
        params: &[Str],
        ret: Concrete(SigType::Option(&INSTANT_SIG)),
    },
    dur_fn("seconds"),
    dur_fn("minutes"),
    dur_fn("hours"),
    dur_fn("days"),
    dur_fn("weeks"),
    dur_fn("months"),
    dur_fn("years"),
];

const INSTANT_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "unix_ms",
        params: &[],
        ret: Concrete(Int),
    },
    ExtFn {
        name: "format",
        params: &[Str],
        ret: Concrete(Str),
    },
    ExtFn {
        name: "in_zone",
        params: &[Str],
        ret: Concrete(ZONED_SIG),
    },
    ExtFn {
        name: "add",
        params: &[DURATION_SIG],
        ret: Concrete(INSTANT_SIG),
    },
    ExtFn {
        name: "sub",
        params: &[DURATION_SIG],
        ret: Concrete(INSTANT_SIG),
    },
    ExtFn {
        name: "diff",
        params: &[INSTANT_SIG],
        ret: Concrete(DURATION_SIG),
    },
    ExtFn {
        name: "is_before",
        params: &[INSTANT_SIG],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        name: "is_after",
        params: &[INSTANT_SIG],
        ret: Concrete(SigType::Bool),
    },
];

/// A zero-arg `Zoned` field accessor returning `int`.
const fn zoned_field(name: &'static str) -> ExtFn {
    ExtFn {
        name,
        params: &[],
        ret: Concrete(Int),
    }
}

const ZONED_METHODS: &[ExtFn] = &[
    zoned_field("year"),
    zoned_field("month"),
    zoned_field("day"),
    zoned_field("hour"),
    zoned_field("minute"),
    zoned_field("second"),
    zoned_field("weekday"),
    ExtFn {
        name: "zone",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        name: "format",
        params: &[Str],
        ret: Concrete(Str),
    },
    ExtFn {
        name: "to_instant",
        params: &[],
        ret: Concrete(INSTANT_SIG),
    },
    ExtFn {
        name: "add",
        params: &[DURATION_SIG],
        ret: Concrete(ZONED_SIG),
    },
    ExtFn {
        name: "sub",
        params: &[DURATION_SIG],
        ret: Concrete(ZONED_SIG),
    },
    ExtFn {
        name: "is_before",
        params: &[ZONED_SIG],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        name: "is_after",
        params: &[ZONED_SIG],
        ret: Concrete(SigType::Bool),
    },
];

const DURATION_METHODS: &[ExtFn] = &[ExtFn {
    name: "to_string",
    params: &[],
    ret: Concrete(Str),
}];

const DATETIME_MODULES: &[ExtModule] = &[ExtModule {
    name: "datetime",
    functions: DATETIME_FNS,
    dispatch: datetime_dispatch,
    // Ring-attributed so the AOT footprint scan drops jiff from a binary that never imports it.
    ring: Some("ring-datetime"),
    ..ExtModule::DEFAULTS
}];

const DATETIME_TYPES: &[ExtType] = &[
    ExtType {
        name: INSTANT_TYPE_NAME,
        namespace: "std.datetime",
        methods: INSTANT_METHODS,
        dispatch: instant_method_dispatch,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: ZONED_TYPE_NAME,
        namespace: "std.datetime",
        methods: ZONED_METHODS,
        dispatch: zoned_method_dispatch,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: DURATION_TYPE_NAME,
        namespace: "std.datetime",
        methods: DURATION_METHODS,
        dispatch: duration_method_dispatch,
        ..ExtType::DEFAULTS
    },
];

/// The `std.datetime` extension unit (Ring 3), registered only under the `ring-datetime` feature.
#[derive(Debug, Clone, Copy)]
pub struct DateTimeExtension;

impl Extension for DateTimeExtension {
    fn name(&self) -> &'static str {
        "std.datetime"
    }
    fn root(&self) -> &'static str {
        "std"
    }
    fn modules(&self) -> &'static [ExtModule] {
        DATETIME_MODULES
    }
    fn types(&self) -> &'static [ExtType] {
        DATETIME_TYPES
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SandboxHost;

    fn call(host: &mut dyn Host, func: &str, args: &[NativeValue]) -> Result<NativeOut, StdError> {
        datetime_dispatch(func, host, args)
    }

    fn instant_of(out: NativeOut) -> Instant {
        match out {
            NativeOut::Extern(b) => b.0.as_any().downcast_ref::<Instant>().unwrap().clone(),
            other => panic!("expected an Instant, got {other:?}"),
        }
    }

    #[test]
    fn now_reads_the_sandbox_clock_deterministically() {
        // The sandbox clock is a fixed epoch, so `now()` is identical every run (what makes the
        // whole surface differential-safe). Two fresh sandboxes agree.
        let mut a = SandboxHost::new();
        let mut b = SandboxHost::new();
        let ta = instant_of(call(&mut a, "now", &[]).unwrap());
        let tb = instant_of(call(&mut b, "now", &[]).unwrap());
        assert_eq!(ta.0, tb.0);
    }

    #[test]
    fn from_unix_ms_round_trips_and_calendar_units_reject_on_instant() {
        let mut h = SandboxHost::new();
        let out = call(
            &mut h,
            "from_unix_ms",
            &[NativeValue::Scalar(Scalar::Int(0))],
        )
        .unwrap();
        let epoch = instant_of(out);
        assert_eq!(epoch.0.as_millisecond(), 0);

        // A calendar-unit duration cannot be added to an absolute Instant (no zone → variable day).
        let days = match call(&mut h, "days", &[NativeValue::Scalar(Scalar::Int(2))]).unwrap() {
            NativeOut::Extern(b) => b,
            other => panic!("expected Duration, got {other:?}"),
        };
        let mut inst = Instant(epoch.0);
        let err = instant_method_dispatch(&mut inst, "add", &mut h, &[NativeValue::Extern(days)])
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Io);
        assert!(
            err.message.contains("in_zone"),
            "message steers to Zoned: {}",
            err.message
        );
    }

    #[test]
    fn parse_is_a_safe_probe() {
        let mut h = SandboxHost::new();
        let ok = call(
            &mut h,
            "parse",
            &[NativeValue::Str("2024-07-11T01:14:00Z".into())],
        )
        .unwrap();
        assert!(matches!(ok, NativeOut::Some(_)));
        let bad = call(&mut h, "parse", &[NativeValue::Str("not a date".into())]).unwrap();
        assert!(matches!(bad, NativeOut::None));
    }
}
