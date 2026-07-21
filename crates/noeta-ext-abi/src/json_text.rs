//! The pure text half of `std.json` — serialization of the neutral [`NativeValue`] tree
//! (cross-cutting audit finding 2). It lives in the ABI crate so the value crates (`noeta-value`,
//! the reference interpreter) reach it without sitting above the stdlib battery tree in the build
//! graph; `noeta_stdlib::json` re-exports both functions, so the dispatch surface is unchanged.

use crate::registry::{NativeValue, Scalar};

/// Serialize a deeply-marshalled [`NativeValue`] to a JSON string — the shared half of
/// `json.stringify` and the `@derive(Serialize<Json>)` method (`o.to_json()`). Each backend marshals
/// its own value into the neutral [`NativeValue`] tree (numbers as scalars, strings/enum-variants/
/// length-summaries as [`NativeValue::Str`], lists/tuples/sets as [`NativeValue::List`], maps and
/// objects as [`NativeValue::Map`]); this single walk produces the bytes, so both backends agree by
/// construction rather than by two hand-kept-identical copies.
///
/// Numbers render unquoted via the shared [`crate::format_float`]/[`crate::format_f32`]; strings are
/// quoted and escaped via [`json_string`]; a list is a JSON array; a keyed aggregate a JSON object
/// (entries in the order supplied — sorted for a map, declared order for an object); unit is `null`.
pub fn stringify(value: &NativeValue) -> String {
    match value {
        NativeValue::Scalar(Scalar::Int(n)) => n.to_string(),
        NativeValue::Scalar(Scalar::Bool(b)) => b.to_string(),
        NativeValue::Scalar(Scalar::Float(f)) => crate::format_float(*f),
        NativeValue::Scalar(Scalar::F32(f)) => crate::format_f32(*f),
        NativeValue::Str(s) => json_string(s),
        NativeValue::Unit => "null".to_string(),
        NativeValue::List(items) => {
            let parts: Vec<String> = items.iter().map(stringify).collect();
            format!("[{}]", parts.join(","))
        }
        NativeValue::Map(entries) => {
            let parts: Vec<String> = entries
                .iter()
                .map(|(key, value)| format!("{}:{}", json_string(key), stringify(value)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        // An extern-type value serializes as its display form, quoted — a `Uuid` is its
        // canonical string in JSON, the same form `echo` prints.
        NativeValue::Extern(e) => json_string(&e.display_string()),
        // A native enum value (native-extensibility S1): a fieldless/backed variant renders as its
        // case name (the same string an enum already stringified to via the `Str` projection), a
        // payload-carrying one as `{"Variant": [fields]}`.
        NativeValue::Variant {
            variant, fields, ..
        } => {
            if fields.is_empty() {
                json_string(variant)
            } else {
                let parts: Vec<String> = fields.iter().map(stringify).collect();
                format!("{{{}:[{}]}}", json_string(variant), parts.join(","))
            }
        }
        // The shallow-marshal-only variants never reach the serializer: `json` is always deeply
        // marshalled (the only producer of a `stringify` argument).
        NativeValue::Bytes(_) | NativeValue::Object { .. } | NativeValue::Opaque(_) => {
            unreachable!("json.stringify is always deeply marshalled")
        }
    }
}

/// Encode a string as a JSON string literal (quotes + the mandatory escapes). Shared by the
/// serializer above and both backends, so `json.stringify` and `@derive(Serialize<Json>)` render
/// identical output — the single source the two duplicated copies used to be.
pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
