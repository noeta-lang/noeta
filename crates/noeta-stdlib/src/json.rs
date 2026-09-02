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
//! A value whose static type carries an **unsigned 64-bit integer** takes one detour: the erased
//! i64 word says nothing about signedness, so the checker hands the door a `noeta_ast::RenderHint`
//! and the tree goes through `noeta_ast::json_stringify`, which writes those positions unsigned and
//! delegates every other branch straight back to [`stringify`]. Decoding is the mirror:
//! [`TypeRecipe::IntN`] accepts a number that fits the declared width, and [`Json::Uint`] keeps the
//! range past `i64::MAX` exact on the way in, so what the encoder wrote the decoder recovers.
//!
//! The parse goes through `serde_json` (robust over escapes, Unicode, and number edge cases).
//! With `serde_json`'s default features its object map is a `BTreeMap`, so keys arrive sorted —
//! deterministic, and matching the language's sorted-key maps.

use crate::registry::{
    FieldDefault, FieldRecipe, NativeOut, Scalar, TypeRecipe, VariantRecipe, VariantTag,
};
use crate::{ErrorKind, ExternValue, StdError};
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
    /// A whole number **above** `i64::MAX` but within `u64` — the range a `u64` occupies and an
    /// `i64` does not. Split from [`Json::Int`] rather than stored as its wrapped word so nothing
    /// downstream can mistake it for a negative value; only a `u64` recipe accepts one.
    Uint(u64),
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

// --- the one JSON error story ----------------------------------------------

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
    /// A well-formed, shape-correct value whose type rejected it in its `Validate::validate`:
    /// the invariant, not the JSON shape, failed. Carries the validator's own
    /// message as the detail and the path to the failing value.
    Validation,
    /// A value of the right JSON *kind* for a target enum that names none of its variants
    /// (`"gold"` for `enum Tier: string { Free = "free"; Paid = "paid" }`). Split out of
    /// [`JsonErrorKind::Mismatch`] because the two want different responses: a mismatch means the
    /// document has the wrong *shape*, while this means it has the right shape and an
    /// out-of-vocabulary *value* — which a caller can act on, since the detail lists every accepted
    /// one. A JSON object where an enum was expected is still a `Mismatch`.
    UnknownVariant,
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
            JsonErrorKind::UnknownVariant => "unknown_variant",
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

    /// A whole number at `path` that does not fit the fixed-width integer type it decodes into.
    /// Kept a [`JsonErrorKind::Mismatch`] — the value is the wrong one for this type, which is what
    /// that kind means — with a detail that names the width and quotes the number, because "expected
    /// u8, found JSON number" about the number `300` reads as a shape complaint and is not
    /// actionable.
    fn out_of_width(path: &str, width: &str, found: &Json) -> JsonError {
        JsonError {
            kind: JsonErrorKind::Mismatch,
            path: path.to_string(),
            detail: format!(
                "expected {width}, found {} — out of range for {width}",
                json_scalar_text(found)
            ),
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

    /// A struct field absent from the JSON object at `path` that *declared* a default the decode
    /// cannot bake (json-defaults). Same [`JsonErrorKind::MissingField`] kind and same shape as
    /// [`Self::missing_field`] — the field really is missing — with the reason appended, so an author
    /// who wrote `at: Time = now()` is told why their default did not apply instead of concluding
    /// defaults are ignored.
    fn dynamic_default(path: &str, field: &str, type_name: &str) -> JsonError {
        JsonError {
            kind: JsonErrorKind::MissingField,
            path: path.to_string(),
            detail: format!(
                "missing field `{field}` for `{type_name}`: its default is not a literal, so a \
                 JSON decode cannot fill it"
            ),
            line: None,
            column: None,
        }
    }

    /// A baked literal default that did not decode through its own field's recipe — unreachable for
    /// a checker-built recipe (the checker renders the default from a folded literal that already
    /// type-checked against the field), and reported rather than swallowed for a hand-built one.
    fn bad_default(path: &str, field: &str, type_name: &str, why: &str) -> JsonError {
        JsonError {
            kind: JsonErrorKind::MissingField,
            path: path.to_string(),
            detail: format!(
                "field `{field}` of `{type_name}` is absent and its baked default is unusable: {why}"
            ),
            line: None,
            column: None,
        }
    }

    /// A value of the right JSON kind for `type_name` that names none of its variants. The detail
    /// lists **every** accepted wire value, in declaration order, because that list is the actionable
    /// half: a caller re-prompting a model, or rendering the failure to a user, otherwise has to go
    /// back to the schema to say what would have worked.
    fn unknown_variant(
        path: &str,
        type_name: &str,
        found: &Json,
        accepted: &[String],
    ) -> JsonError {
        JsonError {
            kind: JsonErrorKind::UnknownVariant,
            path: path.to_string(),
            detail: format!(
                "{} is not a variant of `{type_name}`: expected one of {}",
                json_scalar_text(found),
                accepted.join(", ")
            ),
            line: None,
            column: None,
        }
    }

    /// A validation failure at `path`: a shape-correct value whose type's
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

/// Parse `text` as JSON into a **dynamic** value tree, recoverably (`json.try_parse(text)`, no
/// turbofish) — the door for a document whose shape is the remote party's: a malformed body is a
/// [`JsonError`] the program handles, not an abort.
///
/// This is the dynamic twin of [`try_parse_typed`], and the only recoverable door that needs no
/// declared recipe: a JSON object becomes a string-keyed [`NativeOut::Map`], an array a
/// [`NativeOut::List`], `null` the unit value, and scalars their matching [`NativeOut::Scalar`]/
/// [`NativeOut::Str`]. Only [`JsonErrorKind::Syntax`] can occur — there is no target type to
/// mismatch against — so the error always carries the source line/column from `serde_json`, exactly
/// as a typed decode's syntax failure does.
pub fn try_parse_dynamic(text: &str) -> Result<NativeOut, JsonError> {
    let value: JsonValue = serde_json::from_str(text).map_err(|e| JsonError::syntax(&e))?;
    Ok(to_native(&convert(value)))
}

/// Parse `text` as JSON and decode it into a **dynamic** value tree (`json.parse(text)`, no
/// turbofish) — the **aborting** door over the same walk as [`try_parse_dynamic`], the dynamic twin
/// of [`parse_typed`]. A malformed document is an [`ErrorKind::ArgType`] error the backend raises at
/// the call site (`invalid JSON: …`, via [`JsonError::into_std_error`]).
pub fn parse_dynamic(text: &str) -> Result<NativeOut, StdError> {
    try_parse_dynamic(text).map_err(JsonError::into_std_error)
}

/// Convert a parsed [`Json`] tree into the neutral dynamic [`NativeOut`] tree (`json.parse`'s result).
fn to_native(json: &Json) -> NativeOut {
    match json {
        Json::Null => NativeOut::Unit,
        Json::Bool(b) => NativeOut::Scalar(Scalar::Bool(*b)),
        Json::Int(n) => NativeOut::Scalar(Scalar::Int(*n)),
        // The **dynamic** door has no type to say "unsigned", and the language's `int` is the signed
        // 64-bit word: a number past `i64::MAX` therefore arrives as the `float` its magnitude fits,
        // exactly as any other number too large for an `int` does. Recovering it as a `u64` is what
        // the *typed* door is for (`json.parse::<u64>`).
        Json::Uint(n) => NativeOut::Scalar(Scalar::Float(*n as f64)),
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
/// `Validate::validate`, keeping the two path stories byte-identical.
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
        // A fixed-width integer: the number must fit the declared width, and the built value is the
        // erased 64-bit word — the same representation the language holds one in, so `u64::MAX`
        // round-trips through the negative word it is stored as.
        TypeRecipe::IntN { signed, bits } => match int_within_width(json, *signed, *bits) {
            Some(word) => Ok(NativeOut::Scalar(Scalar::Int(word))),
            // A whole number of the right *kind* that the width cannot hold is its own failure, not
            // a shape mismatch: the document says `300` where the field is a `u8`, and a caller can
            // act on that only if the message says so rather than reporting "found JSON number"
            // about a number.
            None => Err(match json {
                // A magnitude past `u64` arrives as a float with no fractional part — still a whole
                // number the width cannot hold, so it reports as one. A genuinely fractional number
                // is the ordinary shape mismatch.
                Json::Int(_) | Json::Uint(_) => {
                    JsonError::out_of_width(path, &int_width_name(*signed, *bits), json)
                }
                Json::Float(f) if f.fract() == 0.0 => {
                    JsonError::out_of_width(path, &int_width_name(*signed, *bits), json)
                }
                _ => JsonError::mismatch(path, &int_width_name(*signed, *bits), json),
            }),
        },
        TypeRecipe::Float => match json {
            Json::Float(f) => Ok(NativeOut::Scalar(Scalar::Float(*f))),
            Json::Int(n) => Ok(NativeOut::Scalar(Scalar::Float(*n as f64))),
            Json::Uint(n) => Ok(NativeOut::Scalar(Scalar::Float(*n as f64))),
            _ => Err(JsonError::mismatch(path, "float", json)),
        },
        TypeRecipe::F32 => match json {
            Json::Float(f) => Ok(NativeOut::Scalar(Scalar::F32(*f as f32))),
            Json::Int(n) => Ok(NativeOut::Scalar(Scalar::F32(*n as f32))),
            Json::Uint(n) => Ok(NativeOut::Scalar(Scalar::F32(*n as f32))),
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
        // A transient field's type has no wire form, so no JSON value decodes through it. The fill
        // paths that legitimately reach a transient field never call this (an absent `?T` answers
        // `none` without looking at its payload); arriving here means something asked to decode a
        // value into a slot that has no encoding, which is stated rather than approximated.
        TypeRecipe::Transient => Err(JsonError::mismatch(path, "a non-serialized field", json)),
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
        TypeRecipe::Fielded {
            name,
            fields,
            kind,
            has_validator,
        } => match json {
            Json::Object(entries) => {
                let mut slots = Vec::with_capacity(fields.len());
                for field in fields {
                    let key = field.name.as_str();
                    // A **transient** field is not part of this type's serialized shape, so the
                    // input is not consulted for it at all: it fills exactly as an absent field
                    // does, whatever the document happens to contain. Looking the key up first and
                    // ignoring a hit would be the same behavior with a worse failure mode — a
                    // document carrying the key would round-trip through a value nothing wrote.
                    let supplied = match field.skipped {
                        true => None,
                        false => entries.iter().find(|(k, _)| k == key),
                    };
                    match supplied {
                        Some((_, value)) => {
                            let mark = push_member(path, key);
                            let decoded = decode(value, &field.recipe, path)?;
                            path.truncate(mark);
                            slots.push((field.name.clone(), decoded));
                        }
                        // An absent field is filled when the type says it can be — a `?T` field is
                        // `none`, a literal-defaulted field is its default — and is otherwise an
                        // error at the *object's* path (the field itself is named in the detail).
                        // The three cases are exactly the ones `field_specs_of`/`construct` treat as
                        // omittable, minus a default that is not a literal (see [`FieldDefault`]).
                        None => {
                            slots.push((field.name.clone(), fill_absent_field(field, path, name)?))
                        }
                    }
                }
                Ok(NativeOut::Fielded {
                    name: name.clone(),
                    fields: slots,
                    // Carried through so the backend builds the kind the declaration is, rather
                    // than assuming one kind for every recipe.
                    kind: *kind,
                    has_validator: *has_validator,
                })
            }
            _ => Err(JsonError::mismatch(path, name, json)),
        },
        TypeRecipe::Enum {
            name,
            variants,
            has_validator,
        } => decode_variant(json, name, variants, *has_validator, path),
    }
}

/// Walk one JSON value against a [`TypeRecipe::Enum`]: select the variant whose
/// [`VariantTag`] the value matches, and build it as a payload-free [`NativeOut::Variant`] the
/// backend materializes into a real enum value (not a string standing in for one, so a `match` over
/// the result is exhaustive and `==` against a source-written case holds).
///
/// **Two rejections, deliberately distinct.** A value of the wrong JSON *kind* for every tag (an
/// object where a string-backed enum was expected) is an ordinary [`JsonErrorKind::Mismatch`], the
/// same answer any other recipe gives a wrong-shaped value. A value of the *right* kind that simply
/// names no case is a [`JsonErrorKind::UnknownVariant`] listing every accepted wire value. Both carry
/// the path, so an enum three levels down reports `items[2].tier: …` — the decode-door contract in
/// `docs/Validation.md` — and neither can panic or fall through to a silently-wrong value, because
/// the walk has no default arm: a tag matches or the walk fails.
///
/// The tag order is declaration order, so a (declaration-level) duplicate backing resolves to the
/// first case that claims it, deterministically and identically in both backends.
fn decode_variant(
    json: &Json,
    name: &str,
    variants: &[VariantRecipe],
    has_validator: bool,
    path: &str,
) -> Result<NativeOut, JsonError> {
    let matched = variants.iter().find(|v| tag_matches(&v.tag, &v.name, json));
    if let Some(variant) = matched {
        return Ok(NativeOut::Variant {
            enum_name: name.to_string(),
            variant: variant.name.clone(),
            variant_index: variant.index,
            fields: Vec::new(),
            has_validator,
        });
    }
    // Nothing matched. Distinguish "wrong shape" from "unknown value" by asking whether *any* tag
    // could ever have accepted a value of this JSON kind.
    let kind_is_addressable = variants.iter().any(|v| tag_kind_matches(&v.tag, json));
    if !kind_is_addressable {
        return Err(JsonError::mismatch(path, name, json));
    }
    let accepted: Vec<String> = variants.iter().map(|v| v.tag.render(&v.name)).collect();
    Err(JsonError::unknown_variant(path, name, json, &accepted))
}

/// Whether `json` is exactly the wire value `tag` selects on. Numeric widening follows the same
/// lattice rule the scalar recipes use (`int <: float`): a JSON integer selects a `float`-backed
/// case, a fractional number never selects an `int`-backed one.
fn tag_matches(tag: &VariantTag, case_name: &str, json: &Json) -> bool {
    match (tag, json) {
        (VariantTag::Name, Json::Str(s)) => s == case_name,
        (VariantTag::Str(b), Json::Str(s)) => s == b,
        (VariantTag::Int(b), Json::Int(n)) => n == b,
        (VariantTag::Float(b), Json::Float(f)) => f == b,
        (VariantTag::Float(b), Json::Int(n)) => (*n as f64) == *b,
        (VariantTag::Float(b), Json::Uint(n)) => (*n as f64) == *b,
        (VariantTag::Bool(b), Json::Bool(v)) => v == b,
        _ => false,
    }
}

/// Whether `tag` selects on values of `json`'s *kind* at all — the "could this ever have matched?"
/// question that separates a shape failure from an out-of-vocabulary value. Deliberately the
/// kind-only half of [`tag_matches`], so the two cannot drift into disagreeing about which JSON kinds
/// an enum addresses.
fn tag_kind_matches(tag: &VariantTag, json: &Json) -> bool {
    matches!(
        (tag, json),
        (VariantTag::Name | VariantTag::Str(_), Json::Str(_))
            | (VariantTag::Int(_), Json::Int(_))
            | (
                VariantTag::Float(_),
                Json::Float(_) | Json::Int(_) | Json::Uint(_)
            )
            | (VariantTag::Bool(_), Json::Bool(_))
    )
}

/// Decide what an **absent** struct field decodes to, or fail with the missing-field error.
///
/// The one place the "may this field be omitted?" question is answered, so the decode's notion of
/// optionality is a single rule rather than a condition spread over the struct walk:
///
/// - a `?T` field is `none` (an absent optional has always been `none`, never an error);
/// - a field whose declared default is a **literal** ([`FieldDefault::Literal`]) decodes its baked
///   JSON text through the field's own recipe — so the filled value is built by the identical walk a
///   supplied value takes, including numeric widening;
/// - anything else is a missing field. A [`FieldDefault::Dynamic`] field *did* declare a default,
///   so its error says why the default could not be used instead of reporting a bare omission.
///
/// This is the boundary that makes a decode agree with `construct(name, fields)` about which fields
/// may be omitted: `construct` runs the field's compiled default thunk, which a data-only decode
/// walk has no way to reach, so a non-literal default is the one case where the two still differ —
/// and it says so out loud.
fn fill_absent_field(
    field: &FieldRecipe,
    path: &str,
    type_name: &str,
) -> Result<NativeOut, JsonError> {
    match &field.default {
        FieldDefault::Literal(json) => {
            // The baked text is produced by the checker from a folded literal, so it parses and
            // matches the field's recipe by construction; a malformed one is reported (at the
            // object's path, naming the field — the missing-field shape) rather than swallowed.
            let mut default_path = String::new();
            push_member(&mut default_path, &field.name);
            let value = parse(json).map_err(|_| {
                JsonError::bad_default(path, &field.name, type_name, "it is not valid JSON")
            })?;
            decode(&value, &field.recipe, &mut default_path)
                .map_err(|e| JsonError::bad_default(path, &field.name, type_name, &e.detail))
        }
        // An absent optional is `none` whatever the field's default says (`?T` with a `= none`
        // default folds to no literal, and would otherwise report as dynamic).
        _ if matches!(field.recipe, TypeRecipe::Option(_)) => Ok(NativeOut::None),
        FieldDefault::Dynamic => Err(JsonError::dynamic_default(path, &field.name, type_name)),
        FieldDefault::Required => Err(JsonError::missing_field(path, &field.name, type_name)),
    }
}

/// A **scalar** JSON value as its own JSON text, for the enum rejection that quotes what it was
/// handed alongside the values it would have accepted — so the two read in one vocabulary (`"gold" is
/// not a variant of `Tier`: expected one of "free", "paid"`) rather than mixing a rendered value with
/// a kind name. A composite never reaches the enum door (it fails as a `Mismatch` first), so it falls
/// back to its kind name rather than growing a second serializer.
fn json_scalar_text(json: &Json) -> String {
    match json {
        Json::Str(s) => noeta_ext_abi::json_text::json_string(s),
        Json::Int(n) => n.to_string(),
        Json::Uint(n) => n.to_string(),
        Json::Float(f) => noeta_ext_abi::format_float(*f),
        Json::Bool(b) => b.to_string(),
        Json::Null => "null".to_string(),
        Json::Array(_) | Json::Object(_) => format!("a JSON {}", json_kind(json)),
    }
}

/// A JSON number as the erased 64-bit word of a `(signed, bits)` fixed-width integer, or `None` when
/// it is not a whole number or does not fit that width.
///
/// The width check is the recipe's whole contract: both readings of the erased word are legal values
/// of *some* integer type, so a decode that skipped it would silently deliver a `u8` field the value
/// `300` — or a `u64` field a negative one. `u64` is the width that needs [`Json::Uint`]: its upper
/// half has no `i64` spelling, and `json.stringify` writes it as the unsigned digits this reads back.
fn int_within_width(json: &Json, signed: bool, bits: u8) -> Option<i64> {
    let value: i128 = match json {
        Json::Int(n) => i128::from(*n),
        Json::Uint(n) => i128::from(*n),
        _ => return None,
    };
    let (min, max): (i128, i128) = match signed {
        true => (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1),
        false => (0, (1i128 << bits) - 1),
    };
    (min..=max).contains(&value).then_some(value as i64)
}

/// A fixed-width integer type's surface name (`u64`, `i8`) — the "expected" half of a width
/// mismatch, spelled as the author wrote the type.
fn int_width_name(signed: bool, bits: u8) -> String {
    format!("{}{bits}", if signed { 'i' } else { 'u' })
}

/// The surface kind name of a JSON value, for mismatch messages.
fn json_kind(json: &Json) -> &'static str {
    match json {
        Json::Null => "null",
        Json::Bool(_) => "boolean",
        Json::Int(_) | Json::Uint(_) | Json::Float(_) => "number",
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
        JsonValue::Number(n) => match (n.as_i64(), n.as_u64()) {
            // A whole number that fits `i64` is an int; one that fits only `u64` is the range a
            // `u64` occupies past bit 63, kept exactly so it can decode back into one; anything else
            // (a fractional number, or a magnitude beyond both) is a float — matching the language's
            // int/float split.
            (Some(i), _) => Json::Int(i),
            (None, Some(u)) => Json::Uint(u),
            (None, None) => Json::Float(n.as_f64().unwrap_or(f64::NAN)),
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
    use crate::registry::FieldedKind;

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

    use crate::registry::{FieldDefault, FieldRecipe, NativeOut, Scalar, TypeRecipe};

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
        let recipe = TypeRecipe::Fielded {
            name: "Point".into(),
            fields: vec![
                FieldRecipe::required("x", TypeRecipe::Int),
                FieldRecipe::required("y", TypeRecipe::Int),
            ],
            kind: FieldedKind::Struct,
            has_validator: false,
        };
        assert_eq!(
            parse_typed("{\"y\": 2, \"x\": 1}", &recipe).unwrap(),
            NativeOut::Fielded {
                name: "Point".into(),
                fields: vec![
                    ("x".into(), NativeOut::Scalar(Scalar::Int(1))),
                    ("y".into(), NativeOut::Scalar(Scalar::Int(2))),
                ],
                kind: FieldedKind::Struct,
                has_validator: false,
            }
        );
    }

    #[test]
    fn missing_required_field_errors_optional_becomes_none() {
        let recipe = TypeRecipe::Fielded {
            name: "Pair".into(),
            fields: vec![
                FieldRecipe::required("a", TypeRecipe::Int),
                FieldRecipe::required("b", TypeRecipe::Option(boxed(TypeRecipe::Int))),
            ],
            kind: FieldedKind::Struct,
            has_validator: false,
        };
        // `b` absent → `None`.
        assert_eq!(
            parse_typed("{\"a\": 1}", &recipe).unwrap(),
            NativeOut::Fielded {
                name: "Pair".into(),
                fields: vec![
                    ("a".into(), NativeOut::Scalar(Scalar::Int(1))),
                    ("b".into(), NativeOut::None),
                ],
                kind: FieldedKind::Struct,
                has_validator: false,
            }
        );
        // `a` absent → error.
        let recipe_missing_required = TypeRecipe::Fielded {
            name: "Pair".into(),
            fields: vec![FieldRecipe::required("a", TypeRecipe::Int)],
            kind: FieldedKind::Struct,
            has_validator: false,
        };
        assert!(parse_typed("{}", &recipe_missing_required).is_err());
    }

    // --- declared field defaults (json-defaults) ------------------------------------------------

    /// `Pet { id: int, name: string = "(unnamed)" }` — the literal-defaulted struct the tests decode.
    fn pet_recipe() -> TypeRecipe {
        TypeRecipe::Fielded {
            name: "Pet".into(),
            fields: vec![
                FieldRecipe::required("id", TypeRecipe::Int),
                FieldRecipe::with_default("name", TypeRecipe::Str, "\"(unnamed)\""),
            ],
            kind: FieldedKind::Struct,
            has_validator: false,
        }
    }

    #[test]
    fn omitted_literal_default_field_is_filled_with_the_default() {
        assert_eq!(
            parse_typed("{\"id\": 7}", &pet_recipe()).unwrap(),
            NativeOut::Fielded {
                name: "Pet".into(),
                fields: vec![
                    ("id".into(), NativeOut::Scalar(Scalar::Int(7))),
                    ("name".into(), NativeOut::Str("(unnamed)".into())),
                ],
                kind: FieldedKind::Struct,
                has_validator: false,
            }
        );
    }

    #[test]
    fn a_present_value_wins_over_the_declared_default() {
        assert_eq!(
            parse_typed("{\"id\": 7, \"name\": \"Rex\"}", &pet_recipe()).unwrap(),
            NativeOut::Fielded {
                name: "Pet".into(),
                fields: vec![
                    ("id".into(), NativeOut::Scalar(Scalar::Int(7))),
                    ("name".into(), NativeOut::Str("Rex".into())),
                ],
                kind: FieldedKind::Struct,
                has_validator: false,
            }
        );
    }

    #[test]
    fn a_baked_default_decodes_through_its_own_field_recipe() {
        // Every recipe kind a literal default can reach: the widening `int → float`, a `bool`, and a
        // list of scalars. The filled value is built by the same walk a supplied value takes.
        let recipe = TypeRecipe::Fielded {
            name: "Config".into(),
            fields: vec![
                FieldRecipe::with_default("ratio", TypeRecipe::Float, "1"),
                FieldRecipe::with_default("on", TypeRecipe::Bool, "true"),
                FieldRecipe::with_default(
                    "sizes",
                    TypeRecipe::List(boxed(TypeRecipe::Int)),
                    "[1, 2]",
                ),
                // A defaulted OPTIONAL field: the baked default goes through the `Option` recipe
                // like any value, so a `null` default is `none` — a decode never invents a `Some`
                // the declaration did not ask for (and a `= "hi"` default would be `Some("hi")`).
                FieldRecipe::with_default(
                    "note",
                    TypeRecipe::Option(boxed(TypeRecipe::Str)),
                    "null",
                ),
            ],
            kind: FieldedKind::Struct,
            has_validator: false,
        };
        assert_eq!(
            parse_typed("{}", &recipe).unwrap(),
            NativeOut::Fielded {
                name: "Config".into(),
                fields: vec![
                    ("ratio".into(), NativeOut::Scalar(Scalar::Float(1.0))),
                    ("on".into(), NativeOut::Scalar(Scalar::Bool(true))),
                    (
                        "sizes".into(),
                        NativeOut::List(vec![
                            NativeOut::Scalar(Scalar::Int(1)),
                            NativeOut::Scalar(Scalar::Int(2)),
                        ])
                    ),
                    ("note".into(), NativeOut::None),
                ],
                kind: FieldedKind::Struct,
                has_validator: false,
            }
        );
    }

    #[test]
    fn a_non_literal_default_is_still_required_and_says_why() {
        // `Event { at: int = now() }` — the checker could not fold the default, so the recipe carries
        // `Dynamic`: the field stays required, and the error tells the author why their default did
        // not apply (the whole point of keeping the case distinct from a bare missing field).
        let recipe = TypeRecipe::Fielded {
            name: "Event".into(),
            fields: vec![FieldRecipe {
                name: "at".into(),
                recipe: TypeRecipe::Int,
                default: FieldDefault::Dynamic,
                skipped: false,
            }],
            kind: FieldedKind::Struct,
            has_validator: false,
        };
        let err = try_parse_typed("{}", &recipe).unwrap_err();
        assert_eq!(err.kind, JsonErrorKind::MissingField);
        assert_eq!(err.path, "");
        assert_eq!(
            err.detail,
            "missing field `at` for `Event`: its default is not a literal, so a JSON decode \
             cannot fill it"
        );
    }

    #[test]
    fn a_defaulted_field_still_type_checks_a_supplied_value() {
        // A default makes the field OPTIONAL, not untyped: a present value of the wrong kind is the
        // ordinary mismatch, at the field's path.
        let err = try_parse_typed("{\"id\": 1, \"name\": 5}", &pet_recipe()).unwrap_err();
        assert_eq!(err.kind, JsonErrorKind::Mismatch);
        assert_eq!(err.message(), "name: expected string, found JSON number");
    }

    #[test]
    fn defaults_fill_at_every_depth_and_keep_the_path() {
        // Nested: a defaulted field inside a list element is filled per element, and a *required*
        // sibling still fails at the element's own path.
        let pets = TypeRecipe::List(boxed(pet_recipe()));
        let out = parse_typed("[{\"id\": 1}, {\"id\": 2, \"name\": \"Rex\"}]", &pets).unwrap();
        let NativeOut::List(items) = out else {
            panic!("expected a list")
        };
        assert_eq!(
            items,
            vec![
                NativeOut::Fielded {
                    name: "Pet".into(),
                    fields: vec![
                        ("id".into(), NativeOut::Scalar(Scalar::Int(1))),
                        ("name".into(), NativeOut::Str("(unnamed)".into())),
                    ],
                    kind: FieldedKind::Struct,
                    has_validator: false,
                },
                NativeOut::Fielded {
                    name: "Pet".into(),
                    fields: vec![
                        ("id".into(), NativeOut::Scalar(Scalar::Int(2))),
                        ("name".into(), NativeOut::Str("Rex".into())),
                    ],
                    kind: FieldedKind::Struct,
                    has_validator: false,
                },
            ]
        );
        let err = try_parse_typed("[{\"name\": \"Rex\"}]", &pets).unwrap_err();
        assert_eq!(err.kind, JsonErrorKind::MissingField);
        assert_eq!(err.message(), "[0]: missing field `id` for `Pet`");
    }

    #[test]
    fn an_unusable_baked_default_is_reported_not_swallowed() {
        // Unreachable for a checker-built recipe (the default is rendered from a folded literal that
        // already type-checked against the field), so this guards the hand-built/foreign-recipe edge:
        // a default that is not JSON, and one that is JSON of the wrong kind, both report.
        let not_json = TypeRecipe::Fielded {
            name: "Broken".into(),
            fields: vec![FieldRecipe::with_default("n", TypeRecipe::Int, "nope")],
            kind: FieldedKind::Struct,
            has_validator: false,
        };
        let err = try_parse_typed("{}", &not_json).unwrap_err();
        assert_eq!(err.kind, JsonErrorKind::MissingField);
        assert!(err.detail.contains("not valid JSON"), "{}", err.detail);

        let wrong_kind = TypeRecipe::Fielded {
            name: "Broken".into(),
            fields: vec![FieldRecipe::with_default("n", TypeRecipe::Int, "\"five\"")],
            kind: FieldedKind::Struct,
            has_validator: false,
        };
        let err = try_parse_typed("{}", &wrong_kind).unwrap_err();
        assert!(
            err.detail.contains("expected int, found JSON string"),
            "{}",
            err.detail
        );
    }

    #[test]
    fn decodes_nested_list_and_struct() {
        // `List<Point>` where `Point { x: int, y: int }`.
        let point = TypeRecipe::Fielded {
            name: "Point".into(),
            fields: vec![
                FieldRecipe::required("x", TypeRecipe::Int),
                FieldRecipe::required("y", TypeRecipe::Int),
            ],
            kind: FieldedKind::Struct,
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
        let recipe = TypeRecipe::Fielded {
            name: "Point".into(),
            fields: vec![FieldRecipe::required("x", TypeRecipe::Int)],
            kind: FieldedKind::Struct,
            has_validator: false,
        };
        assert!(parse_typed("[1, 2, 3]", &recipe).is_err());
    }

    // --- the path-carrying `JsonError` ------------------------------------

    /// `Order { items: List<Item { price: float }> }` — the nested recipe the path tests decode.
    fn order_recipe() -> TypeRecipe {
        let item = TypeRecipe::Fielded {
            name: "Item".into(),
            fields: vec![FieldRecipe::required("price", TypeRecipe::Float)],
            kind: FieldedKind::Struct,
            has_validator: false,
        };
        TypeRecipe::Fielded {
            name: "Order".into(),
            fields: vec![FieldRecipe::required(
                "items",
                TypeRecipe::List(boxed(item)),
            )],
            kind: FieldedKind::Struct,
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
            &TypeRecipe::Fielded {
                name: "Pair".into(),
                fields: vec![FieldRecipe::required("a", TypeRecipe::Int)],
                kind: FieldedKind::Struct,
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

    // --- the recoverable DYNAMIC door (`json.try_parse`) ----------------------------------------

    #[test]
    fn try_parse_dynamic_builds_the_same_tree_as_the_aborting_door() {
        // The recoverable and aborting dynamic doors are one walk: same tree, no recipe involved.
        let text = "{\"a\": [1, 2.5, null], \"b\": \"x\", \"c\": true}";
        assert_eq!(
            try_parse_dynamic(text).unwrap(),
            parse_dynamic(text).unwrap()
        );
        assert_eq!(
            try_parse_dynamic(text).unwrap(),
            NativeOut::Map(vec![
                (
                    "a".into(),
                    NativeOut::List(vec![
                        NativeOut::Scalar(Scalar::Int(1)),
                        NativeOut::Scalar(Scalar::Float(2.5)),
                        NativeOut::Unit,
                    ])
                ),
                ("b".into(), NativeOut::Str("x".into())),
                ("c".into(), NativeOut::Scalar(Scalar::Bool(true))),
            ])
        );
    }

    #[test]
    fn try_parse_dynamic_reports_a_syntax_failure_with_line_and_column() {
        // The whole point of the door: a malformed document is a value, and it carries the same
        // path/line/column detail the typed door's syntax failure does.
        let err = try_parse_dynamic("{\n  bad").unwrap_err();
        assert_eq!(err.kind, JsonErrorKind::Syntax);
        assert_eq!(err.path, "");
        assert!(err.detail.starts_with("invalid JSON: "), "{}", err.detail);
        assert_eq!(err.line, Some(2));
        assert!(err.column.is_some());
        // Byte-identical to the typed door's syntax error on the same input — one `JsonError::syntax`.
        let typed = try_parse_typed("{\n  bad", &TypeRecipe::Int).unwrap_err();
        assert_eq!(err, typed);
    }

    #[test]
    fn the_dynamic_abort_door_message_is_unchanged() {
        // `json.parse` now composes its abort through `JsonError::into_std_error`; the message must
        // stay the `invalid JSON: …` / `ErrorKind::ArgType` pair the string-error era produced.
        let err = parse_dynamic("{not json}").unwrap_err();
        assert_eq!(err.kind, ErrorKind::ArgType);
        assert!(err.message.starts_with("invalid JSON: "), "{}", err.message);
    }

    // --- fixed-width integers -------------------------------------------------------------------

    /// The three boundaries a `u64` has and an `i64` does not, decoded into the erased word the
    /// language holds one in — the read half of what `json.stringify` writes.
    #[test]
    fn a_u64_decodes_past_bit_63_into_its_erased_word() {
        let u64_recipe = TypeRecipe::IntN {
            signed: false,
            bits: 64,
        };
        for (text, word) in [
            ("9223372036854775807", i64::MAX),
            ("9223372036854775808", i64::MIN),
            ("18446744073709551615", -1),
            ("0", 0),
        ] {
            assert_eq!(
                parse_typed(text, &u64_recipe).unwrap(),
                NativeOut::Scalar(Scalar::Int(word)),
                "{text}"
            );
        }
    }

    /// The width is the contract: a number outside it is refused, at both ends and for both
    /// signednesses, with a detail that names the width and quotes the number.
    #[test]
    fn a_number_outside_the_declared_width_is_refused() {
        let u8_recipe = TypeRecipe::IntN {
            signed: false,
            bits: 8,
        };
        assert_eq!(
            parse_typed("255", &u8_recipe).unwrap(),
            NativeOut::Scalar(Scalar::Int(255))
        );
        let err = try_parse_typed("300", &u8_recipe).unwrap_err();
        assert_eq!(err.kind, JsonErrorKind::Mismatch);
        assert_eq!(err.detail, "expected u8, found 300 — out of range for u8");
        assert!(try_parse_typed("-1", &u8_recipe).is_err());
        let i8_recipe = TypeRecipe::IntN {
            signed: true,
            bits: 8,
        };
        assert_eq!(
            parse_typed("-128", &i8_recipe).unwrap(),
            NativeOut::Scalar(Scalar::Int(-128))
        );
        assert!(try_parse_typed("128", &i8_recipe).is_err());
        // An unsigned width never accepts a negative number, however small the magnitude.
        let u64_recipe = TypeRecipe::IntN {
            signed: false,
            bits: 64,
        };
        let err = try_parse_typed("-1", &u64_recipe).unwrap_err();
        assert_eq!(err.detail, "expected u64, found -1 — out of range for u64");
        // A magnitude past `u64` arrives as a float with no fractional part — still a whole number
        // the width cannot hold, so it reports as one rather than as a shape mismatch.
        let err = try_parse_typed("18446744073709551616", &u64_recipe).unwrap_err();
        assert!(
            err.detail.contains("out of range for u64"),
            "{}",
            err.detail
        );
        // A genuinely fractional number IS a shape mismatch.
        let err = try_parse_typed("1.5", &u64_recipe).unwrap_err();
        assert_eq!(err.detail, "expected u64, found JSON number");
    }

    /// The range past `i64::MAX` is kept exact on the way in (`Json::Uint`) so the typed door can
    /// recover it — while the DYNAMIC door, which has no type to read a width from, still sees the
    /// float any oversized number has always been.
    #[test]
    fn a_number_past_i64_parses_as_a_uint_and_stays_a_float_dynamically() {
        assert_eq!(parse("18446744073709551615").unwrap(), Json::Uint(u64::MAX));
        assert_eq!(parse("9223372036854775807").unwrap(), Json::Int(i64::MAX));
        assert_eq!(
            try_parse_dynamic("18446744073709551615").unwrap(),
            NativeOut::Scalar(Scalar::Float(u64::MAX as f64))
        );
        // A `float` field widens one, exactly as it widens an `int`.
        assert_eq!(
            parse_typed("18446744073709551615", &TypeRecipe::Float).unwrap(),
            NativeOut::Scalar(Scalar::Float(u64::MAX as f64))
        );
    }

    #[test]
    fn unknown_type_error_matches_the_decode_typed_contract() {
        let err = JsonError::unknown_type("Ghost");
        assert_eq!(err.kind, JsonErrorKind::UnknownType);
        assert_eq!(err.message(), "unknown deserializable type `Ghost`");
    }
}
