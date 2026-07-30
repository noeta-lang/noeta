//! **Display / serialization** of values: `display`/`display_into` (echo + interpolation),
//! `to_json`, `to_native_deep` (host marshalling), and `repr`. `impl Value` methods moved
//! verbatim from the crate root (audit-1 finding 8) — same crate, so private access is
//! preserved; rendering is byte-identical.

use crate::heap::{self, Payload};
use crate::{CompactString, Value};

impl Value {
    // --- Display (mirrors the M0 tree-walker's `Value::display`) ---

    /// The display form used by `echo` and `~` concatenation.
    /// Append this value's [`display`](Self::display) form to `out` **without** the intermediate
    /// `String` that `push_str(&self.display())` would allocate. Fast paths cover the values that
    /// dominate string interpolation — a heap string (append its bytes, no clone), a small int, and a
    /// bool — and everything else falls back to `display()`, so the rendering is byte-identical.
    pub fn display_into(self, out: &mut CompactString) {
        if let Some(b) = self.as_bool() {
            out.push_str(if b { "true" } else { "false" });
        } else if self.is_small_int() {
            // `itoa`, not `write!`: the `fmt::Formatter` round-trip costs about as much as the
            // digits themselves on the short ints interpolation overwhelmingly renders.
            out.push_str(itoa::Buffer::new().format(self.as_int().unwrap()));
        } else if self.is_pointer() {
            let handled = heap::with_payload(self, |p| match p {
                Payload::Str(s) => {
                    out.push_str(s);
                    true
                }
                Payload::Int(i) => {
                    out.push_str(itoa::Buffer::new().format(*i));
                    true
                }
                _ => false,
            });
            if !handled {
                out.push_str(&self.display());
            }
        } else {
            out.push_str(&self.display());
        }
    }

    pub fn display(self) -> String {
        // A packed list (P-PACK 2.4) has no specialized display: materialize a temporary boxed list,
        // render it (identically to the boxed equivalent), and release the temporary.
        if self.is_packed_list() {
            let boxed = self.realize_list();
            let out = boxed.display();
            boxed.release();
            return out;
        }
        if let Some(b) = self.as_bool() {
            b.to_string()
        } else if self.is_small_int() {
            self.as_int().unwrap().to_string()
        } else if self.is_float() {
            noeta_ext_abi::format_float(self.as_float().unwrap())
        } else if self.is_f32() {
            // An immediate `f32` displays at f32 precision, byte-identical to the tree-walker.
            noeta_ext_abi::format_f32(self.as_f32().unwrap())
        } else if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Str(s) => s.as_str().to_owned(),
                // A byte buffer renders as a length summary (`<N bytes>`) — opaque and identical on
                // both backends; its content round-trips through `from_bytes`, not display.
                Payload::Bytes(b) => format!("<{} bytes>", b.len()),
                Payload::Int(i) => i.to_string(),
                // Mirrors the M0 tree-walker's `Value::Function(_) => "<fn>"` (and `Builtin`).
                Payload::Closure { .. }
                | Payload::NativeFn(_)
                | Payload::ModuleFn { .. }
                | Payload::MethodHandle { .. }
                | Payload::BoundMethod { .. } => "<fn>".to_string(),
                // A cell is internal capture storage and never reaches a display site (the
                // compiler derefs it first); render transparently as its contents if it ever does.
                Payload::Cell(inner) => inner.display(),
                // Collections render their elements with `repr` (strings quoted), exactly
                // like the M0 tree-walker's `Value::List`/`Value::Map` display.
                Payload::List(items) => {
                    let parts: Vec<String> = items.iter().map(|v| v.repr()).collect();
                    format!("[{}]", parts.join(", "))
                }
                // A tuple renders parenthesized with `repr` elements (`(1, "a")`), the positional
                // counterpart of a list's brackets.
                Payload::Tuple(items) => {
                    let parts: Vec<String> = items.iter().map(|v| v.repr()).collect();
                    format!("({})", parts.join(", "))
                }
                // A set renders with braces and no key colons (`{1, 2, 3}`), distinguishing it
                // from a non-empty map; an empty set is `{}`, like an empty map.
                Payload::Set(items) => {
                    let parts: Vec<String> = items.iter().map(|v| v.repr()).collect();
                    format!("{{{}}}", parts.join(", "))
                }
                Payload::Map(entries) => {
                    let mut kv: Vec<(&noeta_ext_abi::MapKey, &Value)> = entries.iter().collect();
                    kv.sort_unstable_by(|a, b| a.0.cmp(b.0));
                    // A string key keeps its quoted `{k:?}` form; an extern key renders its
                    // display form unquoted (`MapKey::render` — the shared contract).
                    let parts: Vec<String> = kv
                        .iter()
                        .map(|(k, v)| format!("{}: {}", k.render(), v.repr()))
                        .collect();
                    format!("{{{}}}", parts.join(", "))
                }
                // `Type {field: repr, ...}` in slot (declared) order — M0's `ObjectValue`.
                Payload::Object { shape, slots } => {
                    let parts: Vec<String> = shape
                        .fields
                        .iter()
                        .zip(slots)
                        .map(|(name, v)| format!("{name}: {}", v.repr()))
                        .collect();
                    // Display strips a qualified identity to its short name (`App.Models.User` →
                    // `User`); the identity keyed on for dispatch/`is`/`as` stays qualified.
                    format!(
                        "{} {{{}}}",
                        noeta_ast::short_type_name(&shape.name),
                        parts.join(", ")
                    )
                }
                // `Ok(x)`/`none` for built-in Result/Option, else `Type.Variant(data...)`;
                // a no-data variant is just the head. Data renders with `display` (unquoted),
                // matching M0's `EnumValue::display`.
                Payload::Enum { shape, data } => {
                    let head = if shape.builtin_result_option {
                        shape.variant.clone().unwrap_or_default()
                    } else {
                        format!(
                            "{}.{}",
                            noeta_ast::short_type_name(&shape.name),
                            shape.variant.clone().unwrap_or_default()
                        )
                    };
                    if data.is_empty() {
                        head
                    } else {
                        let parts: Vec<String> = data.iter().map(|v| v.display()).collect();
                        format!("{head}({})", parts.join(", "))
                    }
                }
                Payload::NativeModule(name) => format!("<module {name}>"),
                // An extern-type value renders through its contract, identically on both backends.
                Payload::Extern(e) => e.display_string(),
                // An iterator is an opaque reference value (like a file handle).
                Payload::Iter { .. } => "<iterator>".to_string(),
                // A future — step, leaf timer, task handle, async-read, or channel op — is an opaque
                // reference.
                Payload::Future(_)
                | Payload::Timer(_)
                | Payload::Handle(..)
                | Payload::AsyncIo(_)
                | Payload::ChannelSend(..)
                | Payload::ChannelRecv(_)
                | Payload::IsolateFuture(_) => "<future>".to_string(),
                // Channel endpoints are opaque reference values (like an iterator/file handle).
                Payload::Sender(_) => "<sender>".to_string(),
                Payload::Receiver(_) => "<receiver>".to_string(),
                // Handled by the early return at the top of `display`.
                Payload::PackedList { .. } => unreachable!("packed list demoted before display"),
            })
        } else {
            // The unit value (and any other singleton) displays as empty, as in M0.
            String::new()
        }
    }

    /// The JSON encoding synthesized by `@derive(ToJson)` (and `json.stringify`). Marshals the value
    /// into the neutral [`noeta_ext_abi::NativeValue`] tree (see [`Self::to_native_deep`]) and runs the
    /// shared [`noeta_ext_abi::json_text::stringify`], so the tree-walker — driving the same walk over its
    /// own marshalled tree — produces byte-identical output by construction.
    pub fn to_json(self) -> String {
        noeta_ext_abi::json_text::stringify(&self.to_native_deep())
    }

    /// Deeply marshal this value into the neutral [`noeta_ext_abi::NativeValue`] tree the shared JSON
    /// serializer ([`noeta_ext_abi::json_text::stringify`]) consumes — the VM half of `json.stringify` and
    /// `@derive(Serialize<Json>)`. Mirrors the reference interpreter's `value_to_native_deep`
    /// (`noeta-eval/src/lib.rs`) — per-representation glue, mirrored by design (see
    /// `plans/backend-mirror.md`); divergence is caught by `std/json_encoder_one_engine.noe` and
    /// the differential. Numbers become scalars; strings, enum variants, and the opaque
    /// `<fn>`/`<module …>` summaries become [`NativeValue::Str`]; a `bytes` buffer becomes
    /// [`NativeValue::Bytes`]; lists/tuples/sets become a
    /// [`NativeValue::List`]; maps and objects a [`NativeValue::Map`]. Read-only — it never changes a
    /// refcount (a packed list materializes a temporary that is released here).
    pub fn to_native_deep(self) -> noeta_ext_abi::NativeValue {
        use noeta_ext_abi::{NativeValue, Scalar};
        // A packed list serializes via a temporary boxed materialization, identical to the boxed form.
        if self.is_packed_list() {
            let boxed = self.realize_list();
            let out = boxed.to_native_deep();
            boxed.release();
            return out;
        }
        if let Some(b) = self.as_bool() {
            NativeValue::Scalar(Scalar::Bool(b))
        } else if self.is_small_int() {
            NativeValue::Scalar(Scalar::Int(self.as_int().unwrap()))
        } else if self.is_float() {
            NativeValue::Scalar(Scalar::Float(self.as_float().unwrap()))
        } else if self.is_f32() {
            NativeValue::Scalar(Scalar::F32(self.as_f32().unwrap()))
        } else if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Str(s) => NativeValue::Str(s.as_str().to_owned()),
                // A byte buffer crosses as ITSELF, exactly as the shallow projection
                // (`noeta_vm::values::marshal_native_arg`) sends it. It used to marshal to the human
                // summary `"<N bytes>"` — the display form standing in for the value — so a `bytes`
                // argument to any `deep_marshal` consumer (`http.client.post(url, body)`, a SQL bind
                // parameter) silently arrived as that ASCII text instead of the buffer. JSON's lack
                // of a byte type is the *serializer's* problem, not the projection's:
                // `json_text::stringify` renders the buffer as an array of its byte values.
                Payload::Bytes(b) => NativeValue::Bytes(b.to_vec()),
                // An extern-type value marshals as itself; the shared serializer renders its
                // display form as a JSON string (a `Uuid` is its canonical string).
                Payload::Extern(e) => NativeValue::Extern(e.clone()),
                Payload::Int(i) => NativeValue::Scalar(Scalar::Int(*i)),
                // Lists, tuples, and sets all serialize as a JSON array (JSON has neither tuple nor
                // set), so they marshal to one neutral list.
                Payload::List(items) | Payload::Tuple(items) | Payload::Set(items) => {
                    NativeValue::List(items.iter().map(|v| v.to_native_deep()).collect())
                }
                Payload::Map(entries) => {
                    // NativeValue::Map is an ordered Vec; present in sorted-key order. An extern
                    // key marshals as its canonical display form (JSON keys are strings).
                    let mut kv: Vec<(&noeta_ext_abi::MapKey, &Value)> = entries.iter().collect();
                    kv.sort_unstable_by(|a, b| a.0.cmp(b.0));
                    NativeValue::Map(
                        kv.into_iter()
                            .map(|(k, v)| (k.as_native_str(), v.to_native_deep()))
                            .collect(),
                    )
                }
                Payload::Object { shape, slots } => NativeValue::Map(
                    shape
                        .fields
                        .iter()
                        .zip(slots)
                        .map(|(name, v)| (name.clone(), v.to_native_deep()))
                        .collect(),
                ),
                Payload::Closure { .. }
                | Payload::NativeFn(_)
                | Payload::ModuleFn { .. }
                | Payload::MethodHandle { .. }
                | Payload::BoundMethod { .. } => NativeValue::Str("<fn>".to_string()),
                Payload::Cell(inner) => inner.to_native_deep(),
                // An `Option` marshals **through** its payload — the JSON-null convention, and what a
                // native consumer (a SQL bind parameter, `json.stringify`) means by an optional value:
                // `some(x)` is `x`, `none` is null/unit. Without this an `Option` would flatten to its
                // variant *name* (`"some"`) — a silently wrong bound value / serialization.
                Payload::Enum { shape, data } if shape.name == "Option" => {
                    match shape.variant.as_deref() {
                        Some("some") => data
                            .first()
                            .map(|v| v.to_native_deep())
                            .unwrap_or(noeta_ext_abi::NativeValue::Unit),
                        _ => noeta_ext_abi::NativeValue::Unit,
                    }
                }
                // Any other enum marshals to its variant name (the tag) — the JSON convention for a
                // nominal sum type.
                Payload::Enum { shape, .. } => {
                    NativeValue::Str(shape.variant.as_deref().unwrap_or(&shape.name).to_string())
                }
                Payload::NativeModule(name) => NativeValue::Str(format!("<module {name}>")),
                // An iterator has no JSON analog either — its opaque display form.
                Payload::Iter { .. } => NativeValue::Str("<iterator>".to_string()),
                // A future has no JSON analog — its opaque display form.
                Payload::Future(_)
                | Payload::Timer(_)
                | Payload::Handle(..)
                | Payload::AsyncIo(_)
                | Payload::ChannelSend(..)
                | Payload::ChannelRecv(_)
                | Payload::IsolateFuture(_) => NativeValue::Str("<future>".to_string()),
                // Channel endpoints have no JSON analog — their opaque display form.
                Payload::Sender(_) => NativeValue::Str("<sender>".to_string()),
                Payload::Receiver(_) => NativeValue::Str("<receiver>".to_string()),
                // Handled by the early return at the top.
                Payload::PackedList { .. } => {
                    unreachable!("packed list demoted before to_native_deep")
                }
            })
        } else {
            NativeValue::Unit
        }
    }

    /// The representation of a value *inside* a collection: strings are quoted so the
    /// structure stays legible (`["a", "b"]`, not `[a, b]`). Mirrors M0's `Value::repr`.
    pub fn repr(self) -> String {
        match self.as_string() {
            Some(s) => format!("{s:?}"),
            None => self.display(),
        }
    }
}
