//! `std.telemetry` — the tracing SDK surface (native OTEL, T1).
//!
//! A thin facade over the [`Telemetry`](noeta_native::Telemetry) Host capability: `span(name)`
//! starts a span and returns a [`Span`] extern value; the span's methods (`set_attribute`, `end`)
//! marshal to `host.tel_span_*`. The span tree and export live host-side (the sandbox recorder / the
//! real OTLP exporter), so this module is **stateless** — no `ExtState`, no retained arena (a span
//! holds no language value, unlike a reactive signal). Implicit parenting and the scoped `with_span`
//! form arrive in T2; a T1 `span()` is always a root.

use std::any::Any;
use std::cmp::Ordering;

use noeta_native::registry::{ExtFn, RetTy, SigType};
use noeta_native::{
    arity_error, no_function_error, no_method_error, type_error, AttrValue, ExternBox, ExternValue,
    Host, NativeOut, NativeValue, Scalar, SpanId, SpanKind, StdError,
};

/// The reserved surface type name for a span handle. A user declaration of this name is E0049.
pub const SPAN_TYPE_NAME: &str = "Span";

/// An OTel attribute value — the scalar union the surface accepts (a non-scalar is a compile-time
/// type error, not a runtime one).
const ATTR_VALUE: SigType = SigType::Union(&[
    SigType::String,
    SigType::Int,
    SigType::Float,
    SigType::Bool,
]);

/// `std.telemetry`'s module functions.
pub const TELEMETRY_FNS: &[ExtFn] = &[ExtFn {
    name: "span",
    params: &[SigType::String],
    ret: RetTy::Concrete(SigType::Named(SPAN_TYPE_NAME)),
}];

/// The `Span` instance methods. `set_attribute` returns the span for chaining
/// (`span.set_attribute("a", 1).set_attribute("b", 2)`); `end` finalizes it.
pub const SPAN_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "set_attribute",
        params: &[SigType::String, ATTR_VALUE],
        ret: RetTy::Concrete(SigType::Named(SPAN_TYPE_NAME)),
    },
    ExtFn {
        name: "end",
        params: &[],
        ret: RetTy::Concrete(SigType::Unit),
    },
];

/// The `Span` extern value: the host's opaque span handle (plain `Copy` data). Mutating methods
/// reach the host by id; the backend's extern cell gives it reference semantics, so a span passed
/// around refers to one host-side span. Not key-capable (it identifies a mutable host resource).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    pub id: SpanId,
}

impl ExternValue for Span {
    fn type_name(&self) -> &'static str {
        SPAN_TYPE_NAME
    }

    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<Span>() == Some(self)
    }

    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }

    fn hash_value(&self) -> u64 {
        0 // not key-capable; never consulted
    }

    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<span {}>", self.id)
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

/// `std.telemetry` module dispatch.
pub fn telemetry_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "span" => {
            want_arity(func, args, 1)?;
            let name = want_str(func, args, 0)?;
            let id = host.tel_span_start(name, SpanKind::Internal, None);
            Ok(span_value(id))
        }
        _ => Err(no_function_error("telemetry", func)),
    }
}

/// `Span` method dispatch.
pub fn span_method_dispatch(
    recv: &mut dyn ExternValue,
    method: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let id = recv
        .as_any()
        .downcast_ref::<Span>()
        .expect("Span method dispatch on a non-Span extern")
        .id;
    match method {
        "set_attribute" => {
            want_arity(method, args, 2)?;
            let key = want_str(method, args, 0)?;
            let value = want_attr(method, args, 1)?;
            host.tel_span_set_attr(id, key, value);
            Ok(span_value(id))
        }
        "end" => {
            want_arity(method, args, 0)?;
            host.tel_span_end(id);
            Ok(NativeOut::Unit)
        }
        _ => Err(no_method_error(SPAN_TYPE_NAME, method)),
    }
}

fn span_value(id: SpanId) -> NativeOut {
    NativeOut::Extern(ExternBox::new(Span { id }))
}

fn want_arity(method: &str, args: &[NativeValue], expected: usize) -> Result<(), StdError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(arity_error(method, expected, args.len()))
    }
}

fn want_str<'a>(method: &str, args: &'a [NativeValue], index: usize) -> Result<&'a str, StdError> {
    match &args[index] {
        NativeValue::Str(s) => Ok(s),
        _ => Err(type_error(method, "string")),
    }
}

/// Project an attribute-value argument (str/int/float/bool) into an [`AttrValue`]. The checker
/// constrains the param to that union, so a non-scalar can only arrive through a `dyn` launder.
fn want_attr(method: &str, args: &[NativeValue], index: usize) -> Result<AttrValue, StdError> {
    match &args[index] {
        NativeValue::Str(s) => Ok(AttrValue::Str(s.as_str().into())),
        NativeValue::Scalar(Scalar::Int(i)) => Ok(AttrValue::Int(*i)),
        NativeValue::Scalar(Scalar::Float(f)) => Ok(AttrValue::Float(*f)),
        NativeValue::Scalar(Scalar::F32(f)) => Ok(AttrValue::Float(*f as f64)),
        NativeValue::Scalar(Scalar::Bool(b)) => Ok(AttrValue::Bool(*b)),
        _ => Err(type_error(method, "string, int, float, or bool")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::SandboxHost;

    fn str_arg(s: &str) -> NativeValue {
        NativeValue::Str(s.to_string())
    }

    #[test]
    fn span_lifecycle_records_to_the_host() {
        let mut host = SandboxHost::new();

        // `telemetry.span("request")` mints a Span extern and a live (not-yet-recorded) span.
        let out = telemetry_dispatch("span", &mut host, &[str_arg("request")]).unwrap();
        let NativeOut::Extern(mut span) = out else {
            panic!("span() should return a Span extern");
        };
        assert_eq!(span.0.type_name(), SPAN_TYPE_NAME);
        assert!(
            host.recorded_spans().is_empty(),
            "a span is not recorded until end()"
        );

        // `set_attribute` chains (returns a Span); an int value goes through the union.
        let chained = span_method_dispatch(
            &mut *span.0,
            "set_attribute",
            &mut host,
            &[str_arg("http.method"), str_arg("GET")],
        )
        .unwrap();
        assert!(matches!(chained, NativeOut::Extern(_)));
        span_method_dispatch(
            &mut *span.0,
            "set_attribute",
            &mut host,
            &[str_arg("http.status"), NativeValue::Scalar(Scalar::Int(200))],
        )
        .unwrap();

        // `end` finalizes; only now is it recorded.
        assert_eq!(
            span_method_dispatch(&mut *span.0, "end", &mut host, &[]).unwrap(),
            NativeOut::Unit
        );
        let spans = host.recorded_spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "request");
        assert_eq!(spans[0].parent, None);
        assert_eq!(
            spans[0].attributes,
            vec![
                ("http.method".into(), AttrValue::Str("GET".into())),
                ("http.status".into(), AttrValue::Int(200)),
            ]
        );
    }

    #[test]
    fn unknown_function_and_method_are_errors() {
        let mut host = SandboxHost::new();
        assert!(telemetry_dispatch("nope", &mut host, &[str_arg("x")]).is_err());
        let mut span = Span { id: 1 };
        assert!(span_method_dispatch(&mut span, "nope", &mut host, &[]).is_err());
    }
}
