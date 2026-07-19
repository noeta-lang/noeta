//! JSON decoding for the Ring 2 `json` module, shared by both backends — and the module's one
//! error story, [`JsonError`].
//!
//! **Decoding** lives here: parsing produces a backend-agnostic [`Json`] tree, and the typed walk
//! ([`decode`]) checks it against a call-site [`TypeRecipe`], so both backends build identical
//! values from identical input (the differential holds by construction). The walk threads the
//! **path** from the document root, so every failure is a [`JsonError`] naming its exact location.
//! One walk serves both doors: [`try_parse_typed`] (`json.try_parse::<T>` / `json.decode_typed`,
//! recoverable) and [`parse_typed`] (`json.parse::<T>`, the aborting convenience form).
//!
//! **Encoding** is likewise single-engined: [`stringify`] (re-exported below) is the one shared
//! text serializer in `noeta_ext_abi::json_text`, serving both `json.stringify` and the
//! `@derive(Serialize<Json>)` method `to_json()` — each backend only deep-marshals its own value
//! representation into the neutral `NativeValue` tree (the VM's `Value::to_native_deep`, the
//! reference interpreter's `value_to_native_deep` — representation glue, mirrored by design; see
//! `plans/backend-mirror.md`). Key order is per aggregate kind, identical in both doors: maps
//! sorted (their canonical order), objects in declared field order.
//!
//! The parse goes through `serde_json` (robust over escapes, Unicode, and number edge cases).
//! With `serde_json`'s default features its object map is a `BTreeMap`, so keys arrive sorted —
//! deterministic, and matching the language's sorted-key maps.

use crate::registry::{NativeOut, Scalar, TypeRecipe};
use crate::{ErrorKind, ExternValue, StdError, invalid_json_error};
use serde_json::Value as JsonValue;
use std::any::Any;
use std::cmp::Ordering;

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

// --- the one JSON error story (error-machinery arc) ----------------------------------------------

/// What went wrong in a JSON decode — the kind axis of a [`JsonError`]. An enum (not a magic
/// string): the surface `kind()` accessor renders [`JsonErrorKind::label`], and every interior
/// consumer matches the variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonErrorKind {
    /// The document itself is not valid JSON (a `serde_json` parse error; carries line/column).
    Syntax,
    /// A well-formed value of the wrong kind for the target type (`expected float, found JSON
    /// string`).
    Mismatch,
    /// A JSON object is missing a field the target type requires (a missing `Option` field is
    /// `none`, never an error).
    MissingField,
    /// `json.decode_typed(name, …)` was handed a type name with no registered decode recipe.
    UnknownType,
    /// A well-formed, shape-correct value whose type rejected it in its `Validate::validate`
    /// (validation arc): the invariant, not the JSON shape, failed. Carries the validator's own
    /// message as the detail and the path to the failing value.
    Validation,
}

impl JsonErrorKind {
    /// The surface label `JsonError.kind()` returns.
    pub fn label(self) -> &'static str {
        match self {
            JsonErrorKind::Syntax => "syntax",
            JsonErrorKind::Mismatch => "mismatch",
            JsonErrorKind::MissingField => "missing_field",
            JsonErrorKind::UnknownType => "unknown_type",
            JsonErrorKind::Validation => "validation",
        }
    }
}

/// `JsonError`'s registered short name (its `ExtType` in the registry).
pub const JSON_ERROR_TYPE_NAME: &str = "JsonError";

/// `JsonError`'s qualified runtime identity (`{namespace}.{name}` of its `ExtType` registration)
/// — what [`ExternValue::type_identity`] returns, and the `Type::Named` key the checker uses for
/// `json.try_parse::<T>` / `json.decode_typed` error arms.
pub const JSON_ERROR_TYPE_IDENTITY: &str = "std.json.JsonError";

/// The one JSON decode error — std's first [`Error`](noeta trait) implementor. Carries the failure
/// kind, the **path** to the failing value (`items[2].price`; empty at the document root), the
/// detail sentence, and — for a malformed document — the source line/column from `serde_json`.
///
/// Pure `Send` data with content equality, the `ExecResult`/`Response` accessor model: user code
/// reaches the parts through registered methods (`message`/`kind`/`path`/`line`/`column`), and the
/// value displays as its composed [`JsonError::message`] (which is also what `impl Display`'s
/// `to_string()` and `impl Error`'s `message()` return — both declared on its `ExtType`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonError {
    /// What went wrong.
    pub kind: JsonErrorKind,
    /// The path from the document root to the failing value (`items[2].price`); empty for a
    /// document-level failure (a syntax error, a root-value mismatch, an unknown type name).
    pub path: String,
    /// The kind-specific detail sentence (`expected float, found JSON string`).
    pub detail: String,
    /// The 1-based source line of a [`JsonErrorKind::Syntax`] failure; `None` otherwise.
    pub line: Option<u32>,
    /// The 1-based source column of a [`JsonErrorKind::Syntax`] failure; `None` otherwise.
    pub column: Option<u32>,
}

impl JsonError {
    /// A malformed-document error, from the `serde_json` parse failure. The detail keeps serde's
    /// full rendering (which already names the line/column), prefixed `invalid JSON:` — the exact
    /// message the string-error era produced — and additionally carries the position structurally.
    fn syntax(error: &serde_json::Error) -> JsonError {
        JsonError {
            kind: JsonErrorKind::Syntax,
            path: String::new(),
            detail: format!("invalid JSON: {error}"),
            line: u32::try_from(error.line()).ok().filter(|l| *l > 0),
            column: u32::try_from(error.column()).ok().filter(|c| *c > 0),
        }
    }

    /// A wrong-kind error at `path`: the value is well-formed JSON but not what the recipe needs.
    fn mismatch(path: &str, expected: &str, found: &Json) -> JsonError {
        JsonError {
            kind: JsonErrorKind::Mismatch,
            path: path.to_string(),
            detail: format!("expected {expected}, found JSON {}", json_kind(found)),
            line: None,
            column: None,
        }
    }

    /// A required struct field absent from the JSON object at `path`.
    fn missing_field(path: &str, field: &str, type_name: &str) -> JsonError {
        JsonError {
            kind: JsonErrorKind::MissingField,
            path: path.to_string(),
            detail: format!("missing field `{field}` for `{type_name}`"),
            line: None,
            column: None,
        }
    }

    /// A validation failure at `path` (validation arc): a shape-correct value whose type's
    /// `Validate::validate` rejected it. `message` is the validator's own error message (a
    /// `string`-typed validator's bare string, or an `Error`-typed validator's `message()`), which
    /// becomes the detail so the composed message reads `items[2]: <validator message>`.
    pub fn validation(path: &str, message: String) -> JsonError {
        JsonError {
            kind: JsonErrorKind::Validation,
            path: path.to_string(),
            detail: message,
            line: None,
            column: None,
        }
    }

    /// A `json.decode_typed` type name with no registered decode recipe.
    pub fn unknown_type(name: &str) -> JsonError {
        JsonError {
            kind: JsonErrorKind::UnknownType,
            path: String::new(),
            detail: format!("unknown deserializable type `{name}`"),
            line: None,
            column: None,
        }
    }

    /// The composed human message — `impl Error`'s `message()`: the detail, prefixed by the path
    /// when the failure is below the document root (`items[2].price: expected float, found JSON
    /// string`).
    pub fn message(&self) -> String {
        if self.path.is_empty() {
            self.detail.clone()
        } else {
            format!("{}: {}", self.path, self.detail)
        }
    }

    /// The **abort-door** mapping (`json.parse::<T>`): the same walk failure as an
    /// [`ErrorKind::ArgType`] diagnostic. Message compositions are kept byte-identical to the
    /// string-error era at the document root (`invalid JSON: …` for a malformed document,
    /// `json.parse: …` for a decode failure); below the root the path is threaded in.
    pub fn into_std_error(self) -> StdError {
        let message = match self.kind {
            JsonErrorKind::Syntax => self.message(),
            _ => format!("json.parse: {}", self.message()),
        };
        StdError {
            kind: ErrorKind::ArgType,
            message,
        }
    }
}

/// `JsonError` IS a user-facing extern type — pure, host-free, content-equal, not key-capable
/// (the `ExecResult` model). It displays as its composed message, so `echo`/interpolation of an
/// `Err(e)` payload reads naturally in both backends by construction.
impl ExternValue for JsonError {
    fn type_identity(&self) -> &'static str {
        JSON_ERROR_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<JsonError>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "{}", self.message())
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

/// Parse `text` as JSON and decode it into a value of the call-site type `recipe` — the
/// **recoverable** typed decode both `json.try_parse::<T>` and `json.decode_typed` share. A
/// malformed document, or one whose shape does not match the recipe, is a path-carrying
/// [`JsonError`] the backend wraps into a `Result.Err`.
pub fn try_parse_typed(text: &str, recipe: &TypeRecipe) -> Result<NativeOut, JsonError> {
    let value: JsonValue = serde_json::from_str(text).map_err(|e| JsonError::syntax(&e))?;
    let mut path = String::new();
    decode(&convert(value), recipe, &mut path)
}

/// Parse `text` as JSON and decode it into a value of the call-site type `recipe`
/// (`json.parse::<T>(text)`) — the **aborting** convenience door over the same walk as
/// [`try_parse_typed`]. Produces a [`NativeOut`] tree the backend materializes into a `T`; any
/// failure is an [`ErrorKind::ArgType`] error the backend raises at the call site (via
/// [`JsonError::into_std_error`], so abort messages carry the same path precision).
pub fn parse_typed(text: &str, recipe: &TypeRecipe) -> Result<NativeOut, StdError> {
    try_parse_typed(text, recipe).map_err(JsonError::into_std_error)
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

/// Append one member segment (`.field`, or the bare name at the root) to a path, returning the
/// length to truncate back to after the child walk — the pop of the push. Public so a backend's
/// `materialize_recipe` reconstructs the **same** path while re-walking a decoded tree to run
/// `Validate::validate` (validation arc), keeping the two path stories byte-identical.
pub fn push_member(path: &mut String, name: &str) -> usize {
    let mark = path.len();
    if !path.is_empty() {
        path.push('.');
    }
    path.push_str(name);
    mark
}

/// Append one list-index segment (`[2]`) to a path, returning the truncation mark. Public for the
/// backends' `Validate` re-walk (see [`push_member`]).
pub fn push_index(path: &mut String, index: usize) -> usize {
    use std::fmt::Write;
    let mark = path.len();
    let _ = write!(path, "[{index}]");
    mark
}

/// Walk one JSON value against a recipe, threading the **path** from the document root so a
/// failure names its exact location (`items[2].price`). Numeric widening matches the language
/// lattice (`int <: f32 <: float`): a JSON integer satisfies a `float`/`f32` field, but a
/// fractional number does not satisfy an `int`. `path` is a shared segment stack: each recursion
/// pushes its segment and truncates back on the way out, so only the failing branch's path is
/// ever materialized in an error.
fn decode(json: &Json, recipe: &TypeRecipe, path: &mut String) -> Result<NativeOut, JsonError> {
    match recipe {
        TypeRecipe::Int => match json {
            Json::Int(n) => Ok(NativeOut::Scalar(Scalar::Int(*n))),
            _ => Err(JsonError::mismatch(path, "int", json)),
        },
        TypeRecipe::Float => match json {
            Json::Float(f) => Ok(NativeOut::Scalar(Scalar::Float(*f))),
            Json::Int(n) => Ok(NativeOut::Scalar(Scalar::Float(*n as f64))),
            _ => Err(JsonError::mismatch(path, "float", json)),
        },
        TypeRecipe::F32 => match json {
            Json::Float(f) => Ok(NativeOut::Scalar(Scalar::F32(*f as f32))),
            Json::Int(n) => Ok(NativeOut::Scalar(Scalar::F32(*n as f32))),
            _ => Err(JsonError::mismatch(path, "f32", json)),
        },
        TypeRecipe::Bool => match json {
            Json::Bool(b) => Ok(NativeOut::Scalar(Scalar::Bool(*b))),
            _ => Err(JsonError::mismatch(path, "bool", json)),
        },
        TypeRecipe::Str => match json {
            Json::Str(s) => Ok(NativeOut::Str(s.clone())),
            _ => Err(JsonError::mismatch(path, "string", json)),
        },
        TypeRecipe::Unit => match json {
            Json::Null => Ok(NativeOut::Unit),
            _ => Err(JsonError::mismatch(path, "unit", json)),
        },
        TypeRecipe::Option(inner) => match json {
            Json::Null => Ok(NativeOut::None),
            value => Ok(NativeOut::Some(Box::new(decode(value, inner, path)?))),
        },
        TypeRecipe::List(inner) => match json {
            Json::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    let mark = push_index(path, i);
                    let value = decode(item, inner, path)?;
                    path.truncate(mark);
                    out.push(value);
                }
                Ok(NativeOut::List(out))
            }
            _ => Err(JsonError::mismatch(path, "list", json)),
        },
        TypeRecipe::Map(value_recipe) => match json {
            Json::Object(entries) => {
                let mut out = Vec::with_capacity(entries.len());
                for (key, value) in entries {
                    let mark = push_member(path, key);
                    let decoded = decode(value, value_recipe, path)?;
                    path.truncate(mark);
                    out.push((key.clone(), decoded));
                }
                Ok(NativeOut::Map(out))
            }
            _ => Err(JsonError::mismatch(path, "map", json)),
        },
        TypeRecipe::Struct {
            name,
            fields,
            has_validator,
        } => match json {
            Json::Object(entries) => {
                let mut slots = Vec::with_capacity(fields.len());
                for (field, field_recipe) in fields {
                    match entries.iter().find(|(key, _)| key == field) {
                        Some((_, value)) => {
                            let mark = push_member(path, field);
                            let decoded = decode(value, field_recipe, path)?;
                            path.truncate(mark);
                            slots.push((field.clone(), decoded));
                        }
                        // A missing optional field is `None`; a missing required field is an error
                        // at the *object's* path (the field itself is named in the detail).
                        None if matches!(field_recipe, TypeRecipe::Option(_)) => {
                            slots.push((field.clone(), NativeOut::None))
                        }
                        None => return Err(JsonError::missing_field(path, field, name)),
                    }
                }
                Ok(NativeOut::Struct {
                    name: name.clone(),
                    fields: slots,
                    has_validator: *has_validator,
                })
            }
            _ => Err(JsonError::mismatch(path, name, json)),
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

// `stringify` + `json_string` moved to `noeta_ext_abi::json_text` (cross-cutting audit finding 2:
// they are the pure text half both backends and the value crate share, and keeping them here forced
// `noeta-value` to sit above the whole stdlib battery tree). Re-exported so `crate::json::stringify`
// / `json_string` remain the paths every module and doc reference uses.
pub use noeta_ext_abi::json_text::{json_string, stringify};

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
            has_validator: false,
        };
        assert_eq!(
            parse_typed("{\"y\": 2, \"x\": 1}", &recipe).unwrap(),
            NativeOut::Struct {
                name: "Point".into(),
                fields: vec![
                    ("x".into(), NativeOut::Scalar(Scalar::Int(1))),
                    ("y".into(), NativeOut::Scalar(Scalar::Int(2))),
                ],
                has_validator: false,
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
            has_validator: false,
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
                has_validator: false,
            }
        );
        // `a` absent → error.
        let recipe_missing_required = TypeRecipe::Struct {
            name: "Pair".into(),
            fields: vec![("a".into(), TypeRecipe::Int)],
            has_validator: false,
        };
        assert!(parse_typed("{}", &recipe_missing_required).is_err());
    }

    #[test]
    fn decodes_nested_list_and_struct() {
        // `List<Point>` where `Point { x: int, y: int }`.
        let point = TypeRecipe::Struct {
            name: "Point".into(),
            fields: vec![("x".into(), TypeRecipe::Int), ("y".into(), TypeRecipe::Int)],
            has_validator: false,
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
            has_validator: false,
        };
        assert!(parse_typed("[1, 2, 3]", &recipe).is_err());
    }

    // --- the path-carrying `JsonError` (error-machinery arc) ------------------------------------

    /// `Order { items: List<Item { price: float }> }` — the nested recipe the path tests decode.
    fn order_recipe() -> TypeRecipe {
        let item = TypeRecipe::Struct {
            name: "Item".into(),
            fields: vec![("price".into(), TypeRecipe::Float)],
            has_validator: false,
        };
        TypeRecipe::Struct {
            name: "Order".into(),
            fields: vec![("items".into(), TypeRecipe::List(boxed(item)))],
            has_validator: false,
        }
    }

    #[test]
    fn mismatch_error_carries_the_nested_path() {
        // The third item's `price` is a string — the path names it exactly.
        let err = try_parse_typed(
            "{\"items\": [{\"price\": 1.0}, {\"price\": 2.0}, {\"price\": \"three\"}]}",
            &order_recipe(),
        )
        .unwrap_err();
        assert_eq!(err.kind, JsonErrorKind::Mismatch);
        assert_eq!(err.path, "items[2].price");
        assert_eq!(err.detail, "expected float, found JSON string");
        assert_eq!(
            err.message(),
            "items[2].price: expected float, found JSON string"
        );
        assert_eq!((err.line, err.column), (None, None));
    }

    #[test]
    fn missing_field_error_points_at_the_object_and_names_the_field() {
        let err = try_parse_typed("{\"items\": [{}]}", &order_recipe()).unwrap_err();
        assert_eq!(err.kind, JsonErrorKind::MissingField);
        assert_eq!(err.path, "items[0]");
        assert_eq!(err.detail, "missing field `price` for `Item`");
        assert_eq!(err.message(), "items[0]: missing field `price` for `Item`");
    }

    #[test]
    fn map_value_mismatch_paths_through_the_key() {
        let recipe = TypeRecipe::Map(boxed(TypeRecipe::Int));
        let err = try_parse_typed("{\"a\": 1, \"b\": true}", &recipe).unwrap_err();
        assert_eq!(err.path, "b");
        assert_eq!(err.message(), "b: expected int, found JSON boolean");
    }

    #[test]
    fn syntax_error_carries_line_and_column() {
        let err = try_parse_typed("{\n  bad", &TypeRecipe::Int).unwrap_err();
        assert_eq!(err.kind, JsonErrorKind::Syntax);
        assert_eq!(err.path, "");
        assert!(err.detail.starts_with("invalid JSON: "), "{}", err.detail);
        assert_eq!(err.line, Some(2));
        assert!(err.column.is_some());
    }

    #[test]
    fn abort_door_messages_are_byte_identical_at_the_root() {
        // The `json.parse::<T>` abort door composes the SAME messages the string-error era
        // produced for document-root failures; below the root the path is threaded in.
        let root = parse_typed("\"nope\"", &TypeRecipe::Int).unwrap_err();
        assert_eq!(root.kind, ErrorKind::ArgType);
        assert_eq!(root.message, "json.parse: expected int, found JSON string");
        let missing = parse_typed(
            "{}",
            &TypeRecipe::Struct {
                name: "Pair".into(),
                fields: vec![("a".into(), TypeRecipe::Int)],
                has_validator: false,
            },
        )
        .unwrap_err();
        assert_eq!(missing.message, "json.parse: missing field `a` for `Pair`");
        let syntax = parse_typed("{ bad", &TypeRecipe::Int).unwrap_err();
        assert!(syntax.message.starts_with("invalid JSON: "), "no prefix");
        let nested = parse_typed("{\"items\": [{\"price\": []}]}", &order_recipe()).unwrap_err();
        assert_eq!(
            nested.message,
            "json.parse: items[0].price: expected float, found JSON array"
        );
    }

    #[test]
    fn json_error_displays_as_its_message_and_compares_by_content() {
        let err = try_parse_typed("[]", &TypeRecipe::Int).unwrap_err();
        assert_eq!(
            (&err as &dyn ExternValue).display_string(),
            "expected int, found JSON array"
        );
        assert_eq!(err.type_identity(), JSON_ERROR_TYPE_IDENTITY);
        let twin = try_parse_typed("[]", &TypeRecipe::Int).unwrap_err();
        assert!(err.eq_value(&twin));
        let other = try_parse_typed("{}", &TypeRecipe::Int).unwrap_err();
        assert!(!err.eq_value(&other));
    }

    #[test]
    fn unknown_type_error_matches_the_decode_typed_contract() {
        let err = JsonError::unknown_type("Ghost");
        assert_eq!(err.kind, JsonErrorKind::UnknownType);
        assert_eq!(err.message(), "unknown deserializable type `Ghost`");
    }
}
