//! JSON parsing for the Ring 2 `json` module, shared by both backends.
//!
//! Parsing is the only half that lives here: it produces a backend-agnostic [`Json`] tree that
//! each backend converts into its own value representation, so both build identical values from
//! identical input (the differential holds by construction). Serialization is the inverse and
//! already exists in each backend as `to_json`, so `json.stringify` reuses that.
//!
//! The parse goes through `serde_json` (robust over escapes, Unicode, and number edge cases).
//! With `serde_json`'s default features its object map is a `BTreeMap`, so keys arrive sorted —
//! deterministic, and matching the language's sorted-key maps.

use serde_json::Value as JsonValue;

/// A parsed JSON value, backend-agnostic. Numbers split into [`Json::Int`] and [`Json::Float`]
/// to match the language's distinct integer and float types; `null` maps to the unit value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<Json>),
    /// Object entries in sorted-key order (see the module note).
    Object(Vec<(String, Json)>),
}

/// Parse `text` as JSON into a [`Json`] tree, or return a human-readable error message.
pub fn parse(text: &str) -> Result<Json, String> {
    let value: JsonValue = serde_json::from_str(text).map_err(|error| error.to_string())?;
    Ok(convert(value))
}

fn convert(value: JsonValue) -> Json {
    match value {
        JsonValue::Null => Json::Null,
        JsonValue::Bool(b) => Json::Bool(b),
        JsonValue::Number(n) => match n.as_i64() {
            // A whole number that fits `i64` is an int; anything else (a fractional number, or a
            // magnitude beyond `i64`) is a float — matching the language's int/float split.
            Some(i) => Json::Int(i),
            None => Json::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        JsonValue::String(s) => Json::Str(s),
        JsonValue::Array(items) => Json::Array(items.into_iter().map(convert).collect()),
        JsonValue::Object(map) => {
            Json::Object(map.into_iter().map(|(k, v)| (k, convert(v))).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scalars() {
        assert_eq!(parse("null").unwrap(), Json::Null);
        assert_eq!(parse("true").unwrap(), Json::Bool(true));
        assert_eq!(parse("42").unwrap(), Json::Int(42));
        assert_eq!(parse("4.5").unwrap(), Json::Float(4.5));
        assert_eq!(parse("\"hi\"").unwrap(), Json::Str("hi".into()));
    }

    #[test]
    fn parses_array_and_object_with_sorted_keys() {
        assert_eq!(
            parse("[1, 2, 3]").unwrap(),
            Json::Array(vec![Json::Int(1), Json::Int(2), Json::Int(3)])
        );
        // Keys arrive sorted regardless of source order.
        assert_eq!(
            parse("{\"b\": 2, \"a\": 1}").unwrap(),
            Json::Object(vec![("a".into(), Json::Int(1)), ("b".into(), Json::Int(2))])
        );
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(parse("{not json}").is_err());
        assert!(parse("[1, 2").is_err());
    }
}
