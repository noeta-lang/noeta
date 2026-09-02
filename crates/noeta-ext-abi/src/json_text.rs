//! The pure text half of `std.json` — serialization of the neutral [`NativeValue`] tree
//! (cross-cutting audit finding 2). It lives in the ABI crate so the value crates (`noeta-value`,
//! the reference interpreter) reach it without sitting above the stdlib battery tree in the build
//! graph; `noeta_stdlib::json` re-exports both functions, so the dispatch surface is unchanged.

use crate::registry::{NativeValue, Scalar};

/// Serialize a deeply-marshalled [`NativeValue`] to a JSON string — the shared half of
/// `json.stringify` and the `@derive(Serialize<Json>)` method (`o.to_json()`). Each backend marshals
/// its own value into the neutral [`NativeValue`] tree (numbers as scalars, strings and opaque
/// summaries as [`NativeValue::Str`], byte buffers as [`NativeValue::Bytes`], enum values as
/// [`NativeValue::Variant`], lists/tuples/sets as [`NativeValue::List`], maps and
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
        // An enum value — every enum reaches the serializer this way, because the deep projection
        // carries the real variant rather than its name. A fieldless/backed variant renders as its
        // case name, a payload-carrying one as `{"Variant":[fields]}`: the tag
        // names the case and the array carries the positional payload, so no field is dropped.
        // Not symmetric with decoding, deliberately — a payload-carrying variant has no canonical
        // JSON spelling, so `type_to_recipe` declines such an enum *whole* (`TypeRecipe::Enum`
        // documents this).
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
        // A native class instance serializes as a JSON object — its
        // fields in declared order, exactly like a `Map`/record aggregate.
        NativeValue::Instance { fields, .. } => {
            let parts: Vec<String> = fields
                .iter()
                .map(|(key, value)| format!("{}:{}", json_string(key), stringify(value)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        // A `bytes` buffer serializes as a JSON **array of its byte values** — the one encoding JSON
        // offers that is lossless and needs no out-of-band convention (base64 would be a second
        // wire format the reader has to be told about, and `"<N bytes>"` is the display form
        // standing in for the value, i.e. a wrong value on the wire). It is also exactly how the embedding API surfaces a `NativeValue::Bytes`
        // (`noeta_embed`: a list of ints), so one representation covers both doors. Not symmetric
        // with decoding: `bytes` is not a decodable field type at all (E0050), so nothing
        // round-trips a byte buffer through JSON.
        NativeValue::Bytes(bytes) => {
            let parts: Vec<String> = bytes.iter().map(|b| b.to_string()).collect();
            format!("[{}]", parts.join(","))
        }
        // The shallow-marshal-only variants never reach the serializer: `json` is always deeply
        // marshalled (the only producer of a `stringify` argument).
        NativeValue::Object { .. } | NativeValue::Opaque(_) => {
            unreachable!("json.stringify is always deeply marshalled")
        }
    }
}

/// Encode a string as a JSON string literal (quotes + the mandatory escapes). Shared by the
/// serializer above and both backends, so `json.stringify` and `@derive(Serialize<Json>)` render
/// identical output from one source.
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
