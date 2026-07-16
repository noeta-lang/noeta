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

use crate::registry::{NativeOut, Scalar, TypeRecipe};
use crate::{ErrorKind, StdError, invalid_json_error};
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

/// Parse `text` as JSON and decode it into a value of the call-site type `recipe`
/// (`json.parse::<T>(text)`). Produces a [`NativeOut`] tree the backend materializes into a `T`.
///
/// This is the shared, backend-agnostic half of typed deserialization: both backends drive the
/// same walk, so the decoded structure agrees by construction (the differential property the
/// registry exists for). A malformed document, or one whose shape does not match `T` (a missing
/// required field, a wrong scalar kind, an array where an object was expected), is an
/// [`ErrorKind::ArgType`] error the backend raises at the call site.
pub fn parse_typed(text: &str, recipe: &TypeRecipe) -> Result<NativeOut, StdError> {
    let json = parse(text).map_err(|detail| invalid_json_error(&detail))?;
    decode(&json, recipe)
}

/// Parse `text` as JSON and decode it into a **dynamic** value tree (`json.parse(text)`, no
/// turbofish): a JSON object becomes a string-keyed [`NativeOut::Map`], an array a
/// [`NativeOut::List`], `null` the unit value, and scalars their matching [`NativeOut::Scalar`]/
/// [`NativeOut::Str`]. The backend materializes it into the same map/list/scalar values both
/// backends build, so the differential holds by construction.
pub fn parse_dynamic(text: &str) -> Result<NativeOut, StdError> {
    let json = parse(text).map_err(|detail| invalid_json_error(&detail))?;
    Ok(to_native(&json))
}

/// Convert a parsed [`Json`] tree into the neutral dynamic [`NativeOut`] tree (`json.parse`'s result).
fn to_native(json: &Json) -> NativeOut {
    match json {
        Json::Null => NativeOut::Unit,
        Json::Bool(b) => NativeOut::Scalar(Scalar::Bool(*b)),
        Json::Int(n) => NativeOut::Scalar(Scalar::Int(*n)),
        Json::Float(f) => NativeOut::Scalar(Scalar::Float(*f)),
        Json::Str(s) => NativeOut::Str(s.clone()),
        Json::Array(items) => NativeOut::List(items.iter().map(to_native).collect()),
        Json::Object(entries) => NativeOut::Map(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), to_native(value)))
                .collect(),
        ),
    }
}

/// Walk one JSON value against a recipe. Numeric widening matches the language lattice
/// (`int <: f32 <: float`): a JSON integer satisfies a `float`/`f32` field, but a fractional
/// number does not satisfy an `int`.
fn decode(json: &Json, recipe: &TypeRecipe) -> Result<NativeOut, StdError> {
    match recipe {
        TypeRecipe::Int => match json {
            Json::Int(n) => Ok(NativeOut::Scalar(Scalar::Int(*n))),
            _ => Err(mismatch("int", json)),
        },
        TypeRecipe::Float => match json {
            Json::Float(f) => Ok(NativeOut::Scalar(Scalar::Float(*f))),
            Json::Int(n) => Ok(NativeOut::Scalar(Scalar::Float(*n as f64))),
            _ => Err(mismatch("float", json)),
        },
        TypeRecipe::F32 => match json {
            Json::Float(f) => Ok(NativeOut::Scalar(Scalar::F32(*f as f32))),
            Json::Int(n) => Ok(NativeOut::Scalar(Scalar::F32(*n as f32))),
            _ => Err(mismatch("f32", json)),
        },
        TypeRecipe::Bool => match json {
            Json::Bool(b) => Ok(NativeOut::Scalar(Scalar::Bool(*b))),
            _ => Err(mismatch("bool", json)),
        },
        TypeRecipe::Str => match json {
            Json::Str(s) => Ok(NativeOut::Str(s.clone())),
            _ => Err(mismatch("string", json)),
        },
        TypeRecipe::Unit => match json {
            Json::Null => Ok(NativeOut::Unit),
            _ => Err(mismatch("unit", json)),
        },
        TypeRecipe::Option(inner) => match json {
            Json::Null => Ok(NativeOut::None),
            value => Ok(NativeOut::Some(Box::new(decode(value, inner)?))),
        },
        TypeRecipe::List(inner) => match json {
            Json::Array(items) => Ok(NativeOut::List(
                items
                    .iter()
                    .map(|item| decode(item, inner))
                    .collect::<Result<_, _>>()?,
            )),
            _ => Err(mismatch("list", json)),
        },
        TypeRecipe::Map(value_recipe) => match json {
            Json::Object(entries) => Ok(NativeOut::Map(
                entries
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), decode(value, value_recipe)?)))
                    .collect::<Result<_, StdError>>()?,
            )),
            _ => Err(mismatch("map", json)),
        },
        TypeRecipe::Struct { name, fields } => match json {
            Json::Object(entries) => {
                let mut slots = Vec::with_capacity(fields.len());
                for (field, field_recipe) in fields {
                    match entries.iter().find(|(key, _)| key == field) {
                        Some((_, value)) => {
                            slots.push((field.clone(), decode(value, field_recipe)?))
                        }
                        // A missing optional field is `None`; a missing required field is an error.
                        None if matches!(field_recipe, TypeRecipe::Option(_)) => {
                            slots.push((field.clone(), NativeOut::None))
                        }
                        None => {
                            return Err(StdError {
                                kind: ErrorKind::ArgType,
                                message: format!(
                                    "json.parse: missing field `{field}` for `{name}`"
                                ),
                            });
                        }
                    }
                }
                Ok(NativeOut::Struct {
                    name: name.clone(),
                    fields: slots,
                })
            }
            _ => Err(mismatch(name, json)),
        },
    }
}

/// The surface kind name of a JSON value, for mismatch messages.
fn json_kind(json: &Json) -> &'static str {
    match json {
        Json::Null => "null",
        Json::Bool(_) => "boolean",
        Json::Int(_) | Json::Float(_) => "number",
        Json::Str(_) => "string",
        Json::Array(_) => "array",
        Json::Object(_) => "object",
    }
}

fn mismatch(expected: &str, found: &Json) -> StdError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!(
            "json.parse: expected {expected}, found JSON {}",
            json_kind(found)
        ),
    }
}

// `stringify` + `json_string` moved to `noeta_native::json_text` (cross-cutting audit finding 2:
// they are the pure text half both backends and the value crate share, and keeping them here forced
// `noeta-value` to sit above the whole stdlib battery tree). Re-exported so `crate::json::stringify`
// / `json_string` remain the paths every module and doc reference uses.
pub use noeta_native::json_text::{json_string, stringify};

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

    // --- typed decode (`json.parse::<T>`) -------------------------------------------------------

    use crate::registry::{NativeOut, Scalar, TypeRecipe};

    fn boxed(r: TypeRecipe) -> Box<TypeRecipe> {
        Box::new(r)
    }

    #[test]
    fn decodes_scalars_with_numeric_widening() {
        assert_eq!(
            parse_typed("42", &TypeRecipe::Int).unwrap(),
            NativeOut::Scalar(Scalar::Int(42))
        );
        // A JSON integer widens to `float`/`f32` (int <: f32 <: float).
        assert_eq!(
            parse_typed("42", &TypeRecipe::Float).unwrap(),
            NativeOut::Scalar(Scalar::Float(42.0))
        );
        assert_eq!(
            parse_typed("1.5", &TypeRecipe::F32).unwrap(),
            NativeOut::Scalar(Scalar::F32(1.5))
        );
        // A fractional number does not satisfy `int`.
        assert_eq!(
            parse_typed("1.5", &TypeRecipe::Int).unwrap_err().kind,
            ErrorKind::ArgType
        );
    }

    #[test]
    fn decodes_flat_struct_fields_in_declared_order() {
        // Declared order is `x, y` even though JSON keys arrive sorted (`x, y` here anyway).
        let recipe = TypeRecipe::Struct {
            name: "Point".into(),
            fields: vec![("x".into(), TypeRecipe::Int), ("y".into(), TypeRecipe::Int)],
        };
        assert_eq!(
            parse_typed("{\"y\": 2, \"x\": 1}", &recipe).unwrap(),
            NativeOut::Struct {
                name: "Point".into(),
                fields: vec![
                    ("x".into(), NativeOut::Scalar(Scalar::Int(1))),
                    ("y".into(), NativeOut::Scalar(Scalar::Int(2))),
                ],
            }
        );
    }

    #[test]
    fn missing_required_field_errors_optional_becomes_none() {
        let recipe = TypeRecipe::Struct {
            name: "Pair".into(),
            fields: vec![
                ("a".into(), TypeRecipe::Int),
                ("b".into(), TypeRecipe::Option(boxed(TypeRecipe::Int))),
            ],
        };
        // `b` absent → `None`.
        assert_eq!(
            parse_typed("{\"a\": 1}", &recipe).unwrap(),
            NativeOut::Struct {
                name: "Pair".into(),
                fields: vec![
                    ("a".into(), NativeOut::Scalar(Scalar::Int(1))),
                    ("b".into(), NativeOut::None),
                ],
            }
        );
        // `a` absent → error.
        let recipe_missing_required = TypeRecipe::Struct {
            name: "Pair".into(),
            fields: vec![("a".into(), TypeRecipe::Int)],
        };
        assert!(parse_typed("{}", &recipe_missing_required).is_err());
    }

    #[test]
    fn decodes_nested_list_and_struct() {
        // `List<Point>` where `Point { x: int, y: int }`.
        let point = TypeRecipe::Struct {
            name: "Point".into(),
            fields: vec![("x".into(), TypeRecipe::Int), ("y".into(), TypeRecipe::Int)],
        };
        let recipe = TypeRecipe::List(boxed(point));
        let out = parse_typed("[{\"x\": 1, \"y\": 2}, {\"x\": 3, \"y\": 4}]", &recipe).unwrap();
        match out {
            NativeOut::List(items) => assert_eq!(items.len(), 2),
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn decodes_option_null_and_present() {
        let recipe = TypeRecipe::Option(boxed(TypeRecipe::Str));
        assert_eq!(parse_typed("null", &recipe).unwrap(), NativeOut::None);
        assert_eq!(
            parse_typed("\"hi\"", &recipe).unwrap(),
            NativeOut::Some(Box::new(NativeOut::Str("hi".into())))
        );
    }

    #[test]
    fn shape_mismatch_is_an_error() {
        // An array where an object (struct) is expected.
        let recipe = TypeRecipe::Struct {
            name: "Point".into(),
            fields: vec![("x".into(), TypeRecipe::Int)],
        };
        assert!(parse_typed("[1, 2, 3]", &recipe).is_err());
    }
}
