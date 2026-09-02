//! The **packed-list machinery**: flat `List<packed>` values storing raw primitive
//! bytes interpreted through a `&'static PackedSchema` — pack/unpack, in-place mutation, the
//! columnar (SoA) codec, and the byte-level free helpers. `impl Value` methods moved verbatim
//! from the crate root (audit-1 finding 8) — same crate, so private access is preserved; no
//! behavior change.

use noeta_ast::reflect::TypeRepr;
use noeta_object::{PackedKind, PackedSchema};

use crate::Value;
use crate::heap::{self, Payload};

/// Map a bare-scalar element's [`PackedKind`] to its reflected [`TypeRepr`] — the element type a
/// scalar packed list reflects. Only the sub-8-byte numerics ever reach here
/// (`IntN`/`F32`); the other arms are defensive and mirror the width-erased scalar each stores.
fn packed_kind_to_repr(kind: &PackedKind) -> TypeRepr {
    match kind {
        PackedKind::Int => TypeRepr::Int,
        PackedKind::Float => TypeRepr::Float,
        PackedKind::F32 => TypeRepr::F32,
        PackedKind::F64 => TypeRepr::F64,
        PackedKind::IntN { bits, signed } => TypeRepr::IntN {
            bits: *bits,
            signed: *signed,
        },
        PackedKind::Bool => TypeRepr::Bool,
        // A scalar element is never a nested struct; reflect head-only if one ever appears.
        PackedKind::Struct(_) => TypeRepr::Dyn,
    }
}

impl Value {
    /// A flat `List<packed>` value (refcount 1, P-PACK 2.4): `bytes` holds the elements packed as raw
    /// primitive bytes (`schema.byte_size` bytes each — an `f32` field is 4 bytes, P-PACK 3.2b),
    /// interpreted through `schema`. A leaf — it owns no child `Value`s (only primitive bytes), so
    /// freeing it just drops the buffer. The elements are materialized on demand (index, iterate,
    /// demote), so the layout is invisible to `RunResult`.
    pub fn packed_list(schema: &'static PackedSchema, bytes: Vec<u8>) -> Value {
        heap::alloc(Payload::PackedList { schema, bytes })
    }

    /// Whether this is a flat packed list (the `List<packed>` representation, P-PACK 2.4).
    pub fn is_packed_list(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::PackedList { .. }))
    }

    /// Pack this value (a value-struct instance) onto the end of `out` per `schema` — each primitive
    /// field as its little-endian bytes (`int`/`float`/`bool` 8, `f32` 4; P-PACK 3.2b), recursing into
    /// nested packed structs. Returns `false` on any shape mismatch (a non-object, wrong field count,
    /// or a field whose runtime kind disagrees) so the caller can fall back to a boxed list — the flat
    /// form is only ever used when exactly correct.
    pub fn pack_element(self, schema: &PackedSchema, out: &mut Vec<u8>) -> bool {
        // A bare-scalar element (`List<i32>`/`List<f32>`): the value is a bare `int`/`f32`, not an
        // object, so it packs directly through its single field kind (no `Payload::Object` to read).
        if schema.shape.is_none() {
            return pack_scalar(self, &schema.fields[0], out);
        }
        heap::with_payload(self, |p| match p {
            Payload::Object { slots, .. } if slots.len() == schema.fields.len() => {
                for (kind, &slot) in schema.fields.iter().zip(slots.iter()) {
                    let ok = match kind {
                        PackedKind::Int => slot
                            .as_int()
                            .map(|i| out.extend_from_slice(&(i as u64).to_le_bytes()))
                            .is_some(),
                        PackedKind::Float => slot
                            .as_float()
                            .map(|f| out.extend_from_slice(&f.to_bits().to_le_bytes()))
                            .is_some(),
                        PackedKind::F32 => slot
                            .as_f32()
                            .map(|f| out.extend_from_slice(&f.to_bits().to_le_bytes()))
                            .is_some(),
                        // `f64`/`iN`/`uN` fields carry width-erased scalars at runtime (a
                        // `float`/`int`); only the buffer slot is narrowed.
                        PackedKind::F64 => slot
                            .as_float()
                            .map(|f| out.extend_from_slice(&f.to_bits().to_le_bytes()))
                            .is_some(),
                        PackedKind::IntN { bits, .. } => {
                            slot.as_int().map(|i| write_intn(out, i, *bits)).is_some()
                        }
                        PackedKind::Bool => slot.as_bool().map(|b| out.push(u8::from(b))).is_some(),
                        PackedKind::Struct(inner) => slot.pack_element(inner, out),
                    };
                    if !ok {
                        return false;
                    }
                }
                true
            }
            _ => false,
        })
    }

    /// Pack `element` (a value-struct instance) onto the end of this packed list's buffer **in
    /// place** (P-PACK 2.5 streaming construction). The caller must guarantee a uniquely-owned packed
    /// list (`refcount == 1`) — true for the streaming accumulator, which is never aliased. The
    /// element's primitives are *copied* into the buffer (not retained), so the caller still owns the
    /// element value and must release it. Returns `false` without modifying the buffer if `element`
    /// fails to pack (staged into a scratch vector first), so the caller can demote to a boxed list.
    #[must_use]
    pub fn packed_push(self, element: Value) -> bool {
        debug_assert!(
            self.is_packed_list() && heap::refcount(self) == 1 && !heap::is_shared(self),
            "packed_push requires a uniquely-owned packed list"
        );
        heap::with_payload_mut(self, |p| match p {
            Payload::PackedList { schema, bytes } => {
                let mut staged = Vec::with_capacity(schema.byte_size);
                if element.pack_element(schema, &mut staged) {
                    if schema.column {
                        // Column-major append rebuilds the buffer (O(n)); see `column_append`.
                        *bytes = column_append(schema, bytes, &staged);
                    } else {
                        bytes.extend(staged);
                    }
                    true
                } else {
                    false
                }
            }
            _ => false,
        })
    }

    /// Demote a list to an **owned** boxed list the caller must release: a packed list materializes
    /// into a fresh `Payload::List` of owned objects (refcount 1 each, owned by the list); an
    /// already-boxed list is returned with one extra reference. Either way the result is a boxed
    /// list value with an independent reference — so a generic list op can reuse the boxed code path
    /// on a packed list and then `release` the result, with no double-counting. The caller must have
    /// checked [`Value::is_list`].
    pub fn realize_list(self) -> Value {
        if self.is_packed_list() {
            Value::list(self.packed_items())
        } else {
            self.inc_ref();
            self
        }
    }

    /// Materialize the packed element at `index` into an owned `Value::Object` (refcount 1) — a
    /// single-element read with no full-list materialization. The caller owns the returned value.
    /// `index` must be in bounds (callers check via [`Value::list_len`]).
    pub fn packed_get(self, index: usize) -> Value {
        let (schema, elem) = heap::with_payload(self, |p| match p {
            Payload::PackedList { schema, bytes } => {
                let stride = schema.byte_size;
                // Gather the element into row order first (a plain byte copy, no allocation), so the
                // materialization below is layout-agnostic. Row-major is a contiguous stride; column-
                // major scatters the element across its columns.
                let elem = if schema.column {
                    let count = schema.count(bytes.len());
                    gather_row(schema, bytes, index, count)
                } else {
                    let offset = index * stride;
                    bytes[offset..offset + stride].to_vec()
                };
                (*schema, elem)
            }
            _ => unreachable!("packed_get on a non-packed list"),
        });
        unpack_element(schema, &elem, 0).0
    }

    /// Read a single field of the packed element at `index` (P-PACK 2.5+ fused `list[i].field`),
    /// decoding only that field's word(s) — a primitive materializes directly, a nested packed struct
    /// is unpacked from its inline sub-range. Returns the owned field value (refcount 1), or `None`
    /// if `index` is out of range or `field` is not in the element schema (the checker only fuses
    /// real field reads on a packed type, so a hit is the norm; the caller falls back on `None`). No
    /// full-element materialization — this is the scalar-access win over `packed_get`.
    pub fn packed_field(self, index: usize, field: &str) -> Option<Value> {
        let (kind, slice) = heap::with_payload(self, |p| match p {
            Payload::PackedList { schema, bytes } => {
                let count = schema.count(bytes.len());
                if index >= count {
                    return None;
                }
                // A bare-scalar element has no named fields (`shape == None`), so a field read never
                // resolves — the checker only fuses `list[i].field` on a struct element anyway.
                let slot = schema.shape?.slot_of(field)?;
                // The field's byte offset resolves through the layout axis (row vs column).
                let at = schema.field_offset(index, slot, count);
                let width = schema.fields[slot].byte_width();
                Some((schema.fields[slot].clone(), bytes[at..at + width].to_vec()))
            }
            _ => None,
        })?;
        Some(decode_packed_field(&kind, &slice, 0))
    }

    /// The reflected **element** [`TypeRepr`] of a bare-scalar packed list (`List<i32>`/`List<f32>`),
    /// derived from its schema — so a value laundered through `dyn` still reports `List<i32>`, not
    /// `List<dyn>`, keeping slice-1's reflection distinctness (`List<i32> is List<int>` → false) without
    /// a per-value tag. `None` for a boxed list or a **struct**-packed list (whose reflection stays
    /// head-only, unchanged). Read on the reflection path only — the packed-list value carries no tag.
    pub fn packed_scalar_elem_repr(self) -> Option<TypeRepr> {
        if !self.is_packed_list() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::PackedList { schema, .. } if schema.shape.is_none() => {
                Some(packed_kind_to_repr(&schema.fields[0]))
            }
            _ => None,
        })
    }

    /// The raw flat byte buffer of a packed list (`to_bytes`, P-PACK 4.4), regardless of element
    /// schema; `None` for a boxed list (which has no canonical serialized form).
    pub fn packed_bytes(self) -> Option<Vec<u8>> {
        if !self.is_packed_list() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::PackedList { bytes, .. } => Some(bytes.clone()),
            _ => None,
        })
    }

    /// Borrow this packed list's schema and raw byte buffer for the duration of `f` — the
    /// zero-copy read under the native raw-buffer seam (`NativeCtx::with_packed`).
    /// `None` (without running `f`) for anything that is not a packed list.
    pub fn with_packed_ref<R>(
        self,
        f: impl FnOnce(&'static PackedSchema, &[u8]) -> R,
    ) -> Option<R> {
        if !self.is_packed_list() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::PackedList { schema, bytes } => Some(f(schema, bytes)),
            _ => None,
        })
    }

    /// This packed list's schema handle and a **copy** of its byte buffer — the allocating read
    /// for a caller that outlives the borrow (`NativeCtx::with_packed_mut`'s copy-on-write path).
    /// `None` for anything that is not a packed list.
    pub fn packed_parts(self) -> Option<(&'static PackedSchema, Vec<u8>)> {
        if !self.is_packed_list() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::PackedList { schema, bytes } => Some((*schema, bytes.clone())),
            _ => None,
        })
    }

    /// Mutate this packed list's byte buffer **in place** through `f` (the raw-buffer seam's
    /// proven-sole-ownership fast path). The caller must guarantee a uniquely-owned packed list
    /// (`refcount == 1`), like the other `*_in_place` ops.
    pub fn packed_mutate_in_place(self, f: impl FnOnce(&'static PackedSchema, &mut [u8])) {
        debug_assert!(
            self.is_packed_list() && heap::refcount(self) == 1 && !heap::is_shared(self),
            "packed_mutate_in_place requires a uniquely-owned packed list"
        );
        heap::with_payload_mut(self, |p| match p {
            Payload::PackedList { schema, bytes } => f(schema, bytes),
            _ => unreachable!("packed_mutate_in_place on a non-packed list"),
        });
    }

    /// Build a new flat packed list from selected element `indices` of this one, copying each
    /// selected element's word-block verbatim — no per-element materialization (P-PACK 2.6). The
    /// schema is shared (an `Rc` clone). This keeps a `List<packed>` *flat* through the selection
    /// producers (`reverse`/`slice`/`filter`) instead of demoting to N boxed objects. A packed list
    /// is a GC leaf, so the new buffer owns no child references; the caller owns the result (rc 1).
    /// Every index must be in range (callers validate against [`Value::list_len`]).
    pub fn packed_select(self, indices: &[usize]) -> Value {
        let (schema, buf) = heap::with_payload(self, |p| match p {
            Payload::PackedList { schema, bytes } => {
                let buf = if schema.column {
                    let count = schema.count(bytes.len());
                    column_select(schema, bytes, indices, count)
                } else {
                    let stride = schema.byte_size;
                    let mut out = Vec::with_capacity(indices.len() * stride);
                    for &i in indices {
                        out.extend_from_slice(&bytes[i * stride..i * stride + stride]);
                    }
                    out
                };
                (*schema, buf)
            }
            _ => unreachable!("packed_select on a non-packed list"),
        });
        Value::packed_list(schema, buf)
    }

    /// Build a new flat packed list with element `index` replaced by `element` (P-PACK 2.6 flat
    /// `set`). The element's primitives are copied into a fresh buffer; the caller still owns
    /// `element`. Returns `None` (so the caller demotes) if `element` does not pack into the schema.
    pub fn packed_set(self, index: usize, element: Value) -> Option<Value> {
        let (schema, mut buf) = heap::with_payload(self, |p| match p {
            Payload::PackedList { schema, bytes } => (*schema, bytes.clone()),
            _ => unreachable!("packed_set on a non-packed list"),
        });
        let stride = schema.byte_size;
        let mut staged = Vec::with_capacity(stride);
        if !element.pack_element(schema, &mut staged) {
            return None;
        }
        if schema.column {
            let count = schema.count(buf.len());
            column_set(schema, &mut buf, index, count, &staged);
        } else {
            buf[index * stride..index * stride + stride].copy_from_slice(&staged);
        }
        Some(Value::packed_list(schema, buf))
    }

    /// Overwrite element `index` of this packed list **in place** with `element` (P-PACK 2.6 reuse
    /// path for `acc = acc.set(i, v)`). The caller must guarantee a uniquely-owned packed list
    /// (`refcount == 1`). `element`'s primitives are copied into the buffer (no retain); the caller
    /// still owns `element`. Returns `false` (buffer untouched) if `element` does not pack, so the
    /// caller can fall back to the copy path.
    #[must_use]
    pub fn packed_set_in_place(self, index: usize, element: Value) -> bool {
        debug_assert!(
            self.is_packed_list() && heap::refcount(self) == 1 && !heap::is_shared(self),
            "packed_set_in_place requires a uniquely-owned packed list"
        );
        heap::with_payload_mut(self, |p| match p {
            Payload::PackedList { schema, bytes } => {
                let stride = schema.byte_size;
                let mut staged = Vec::with_capacity(stride);
                if element.pack_element(schema, &mut staged) {
                    if schema.column {
                        let count = schema.count(bytes.len());
                        column_set(schema, bytes, index, count, &staged);
                    } else {
                        bytes[index * stride..index * stride + stride].copy_from_slice(&staged);
                    }
                    true
                } else {
                    false
                }
            }
            _ => false,
        })
    }

    /// Concatenate two packed lists of the **same layout** into a new flat packed list (P-PACK 2.6
    /// `a ~ b`), copying both word buffers. Returns `None` (so the caller demotes) unless both are
    /// packed and share an element shape. Both operands are borrowed (the caller still owns them).
    pub fn packed_concat(self, other: Value) -> Option<Value> {
        if !self.is_packed_list() || !other.is_packed_list() {
            return None;
        }
        let (schema, mut buf) = heap::with_payload(self, |p| match p {
            Payload::PackedList { schema, bytes } => (*schema, bytes.clone()),
            _ => unreachable!("packed_concat on a non-packed list"),
        });
        let other_bytes = heap::with_payload(other, |p| match p {
            Payload::PackedList {
                schema: s2,
                bytes: b2,
            } => std::ptr::eq(schema, *s2).then(|| b2.clone()),
            _ => None,
        })?;
        // Same shape ⇒ same layout. Row appends the buffers; column interleaves per column.
        if schema.column {
            buf = column_concat(schema, &buf, &other_bytes);
        } else {
            buf.extend_from_slice(&other_bytes);
        }
        Some(Value::packed_list(schema, buf))
    }

    /// Append `other`'s elements to this packed list **in place** (P-PACK 2.6 reuse path for
    /// `acc = acc ~ xs`). The caller must guarantee a uniquely-owned packed list (`refcount == 1`).
    /// `other` is borrowed (its words copied). Returns `false` (buffer untouched) unless `other` is a
    /// packed list of the same layout, so the caller can fall back to the copy path.
    #[must_use]
    pub fn packed_extend_in_place(self, other: Value) -> bool {
        debug_assert!(
            self.is_packed_list() && heap::refcount(self) == 1 && !heap::is_shared(self),
            "packed_extend_in_place requires a uniquely-owned packed list"
        );
        if !other.is_packed_list() {
            return false;
        }
        let (other_schema, other_bytes) = heap::with_payload(other, |p| match p {
            Payload::PackedList { schema, bytes } => (*schema, bytes.clone()),
            _ => unreachable!("packed_extend_in_place on a non-packed list"),
        });
        heap::with_payload_mut(self, |p| match p {
            Payload::PackedList { schema, bytes } if std::ptr::eq(*schema, other_schema) => {
                if schema.column {
                    // Column layout must rebuild (each column grows in the middle of the buffer).
                    *bytes = column_concat(schema, bytes, &other_bytes);
                } else {
                    bytes.extend_from_slice(&other_bytes);
                }
                true
            }
            _ => false,
        })
    }

    /// Materialize every packed element into an owned vector (each refcount 1). Used by
    /// [`Value::realize_list`]; the words are copied out before allocating so no heap borrow is held
    /// across element construction.
    fn packed_items(self) -> Vec<Value> {
        let (schema, bytes) = heap::with_payload(self, |p| match p {
            Payload::PackedList { schema, bytes } => (*schema, bytes.clone()),
            _ => unreachable!("packed_items on a non-packed list"),
        });
        let count = schema.count(bytes.len());
        let mut out = Vec::with_capacity(count);
        if schema.column {
            // Column-major: each element is scattered across columns — gather it to row order first.
            for i in 0..count {
                let row = gather_row(schema, &bytes, i, count);
                out.push(unpack_element(schema, &row, 0).0);
            }
        } else {
            let mut at = 0;
            for _ in 0..count {
                let (value, next) = unpack_element(schema, &bytes, at);
                out.push(value);
                at = next;
            }
        }
        out
    }
}

/// Render a float deterministically: whole-valued floats keep a trailing `.0` so they are
/// visibly distinct from ints (mirrors the tree-walker exactly).
/// Materialize one packed element from `words` starting at `offset`, returning the owned
/// `Value::Object` (refcount 1) and the offset just past it — so nested structs and the caller
/// advance in lock-step with [`Value::pack_element`]. Each primitive becomes an immediate (or a
/// boxed int for a large magnitude); each nested struct recurses, the parent object owning its
/// reference. The object reuses `schema.shape`, so it is shape-identical to a constructed instance.
/// Read 8 little-endian bytes at `offset` as a `u64` (the storage word for `int`/`float`/`bool`).
fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

/// Read 4 little-endian bytes at `offset` as a `u32` (the storage word for `f32`).
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

/// Read a fixed-width integer slot (`bits/8` little-endian bytes at `offset`) back into the runtime's
/// 8-byte `int`. A **signed** slot sign-extends its top bit (a stored `-1i8`
/// reads back `-1`); an **unsigned** slot zero-extends (`255u8` reads back `255`).
fn read_intn(bytes: &[u8], offset: usize, bits: u8, signed: bool) -> i64 {
    let n = (bits as usize) / 8;
    let mut raw: u64 = 0;
    for i in 0..n {
        raw |= (bytes[offset + i] as u64) << (8 * i);
    }
    if signed && bits < 64 {
        let sign_bit = 1u64 << (bits - 1);
        if raw & sign_bit != 0 {
            raw |= !((1u64 << bits) - 1);
        }
    }
    raw as i64
}

/// Append a fixed-width integer's low `bits/8` little-endian bytes to `out`.
fn write_intn(out: &mut Vec<u8>, value: i64, bits: u8) {
    let n = (bits as usize) / 8;
    let raw = value as u64;
    for i in 0..n {
        out.push((raw >> (8 * i)) as u8);
    }
}

/// Pack one bare-scalar list element (`self`) — a bare `int`/`f32`, not an object — onto the end of
/// `out` per its single `kind`. The inverse of [`decode_packed_field`].
/// Returns `false` (leaving `out` untouched of a partial write is the caller's staging concern) if the
/// value's runtime kind disagrees with the slot, so the caller can demote to a boxed list. A scalar
/// element is only ever a fixed-width numeric (`IntN`/`F32`) or, defensively, one of the other
/// primitives — never a nested `Struct` (a scalar has no struct wrapper).
fn pack_scalar(value: Value, kind: &PackedKind, out: &mut Vec<u8>) -> bool {
    match kind {
        PackedKind::Int => value
            .as_int()
            .map(|i| out.extend_from_slice(&(i as u64).to_le_bytes()))
            .is_some(),
        PackedKind::Float | PackedKind::F64 => value
            .as_float()
            .map(|f| out.extend_from_slice(&f.to_bits().to_le_bytes()))
            .is_some(),
        PackedKind::F32 => value
            .as_f32()
            .map(|f| out.extend_from_slice(&f.to_bits().to_le_bytes()))
            .is_some(),
        PackedKind::IntN { bits, .. } => {
            value.as_int().map(|i| write_intn(out, i, *bits)).is_some()
        }
        PackedKind::Bool => value.as_bool().map(|b| out.push(u8::from(b))).is_some(),
        // Unreachable for a real scalar list (a scalar element is a primitive, never a nested struct),
        // but demote rather than panic if some future caller hands one in.
        PackedKind::Struct(_) => false,
    }
}

/// Decode one packed field at byte `offset` into an owned [`Value`] — the per-field counterpart of
/// [`unpack_element`], used by [`Value::packed_field`] to read a single field without materializing
/// the whole element (P-PACK 3.2b byte-addressed).
fn decode_packed_field(kind: &PackedKind, bytes: &[u8], offset: usize) -> Value {
    match kind {
        PackedKind::Int => Value::int(read_u64(bytes, offset) as i64),
        PackedKind::Float => Value::float(f64::from_bits(read_u64(bytes, offset))),
        PackedKind::F32 => Value::f32(f32::from_bits(read_u32(bytes, offset))),
        PackedKind::F64 => Value::float(f64::from_bits(read_u64(bytes, offset))),
        PackedKind::IntN { bits, signed } => Value::int(read_intn(bytes, offset, *bits, *signed)),
        PackedKind::Bool => Value::bool(bytes[offset] != 0),
        PackedKind::Struct(inner) => unpack_element(inner, bytes, offset).0,
    }
}

fn unpack_element(schema: &PackedSchema, bytes: &[u8], offset: usize) -> (Value, usize) {
    // A bare-scalar element (`List<i32>`/`List<f32>`) materializes to a bare `Value` — a byte-read
    // and tag, no object allocation (the scalar-access win). Its single field is the whole element.
    if schema.shape.is_none() {
        let kind = &schema.fields[0];
        return (
            decode_packed_field(kind, bytes, offset),
            offset + kind.byte_width(),
        );
    }
    let mut slots = Vec::with_capacity(schema.fields.len());
    let mut at = offset;
    for kind in &schema.fields {
        match kind {
            PackedKind::Int => {
                slots.push(Value::int(read_u64(bytes, at) as i64));
                at += 8;
            }
            PackedKind::Float => {
                slots.push(Value::float(f64::from_bits(read_u64(bytes, at))));
                at += 8;
            }
            PackedKind::F32 => {
                slots.push(Value::f32(f32::from_bits(read_u32(bytes, at))));
                at += 4;
            }
            PackedKind::F64 => {
                slots.push(Value::float(f64::from_bits(read_u64(bytes, at))));
                at += 8;
            }
            PackedKind::IntN { bits, signed } => {
                slots.push(Value::int(read_intn(bytes, at, *bits, *signed)));
                at += (*bits as usize) / 8;
            }
            PackedKind::Bool => {
                slots.push(Value::bool(bytes[at] != 0));
                at += 1;
            }
            PackedKind::Struct(inner) => {
                let (nested, next) = unpack_element(inner, bytes, at);
                slots.push(nested);
                at = next;
            }
        }
    }
    // The scalar case returned above, so a struct element always has a shape here.
    (
        Value::object(schema.shape.expect("struct element has a shape"), slots),
        at,
    )
}

/// Gather element `index`'s fields (a buffer of `count` elements) into a fresh **row-order** byte
/// vector (fields contiguous, slot order) — the inverse of the column scatter. For a
/// row-major buffer this simply copies the element's contiguous stride; for a column-major one it
/// pulls each field from its column. The row-order result feeds [`unpack_element`], so materializing
/// an element is layout-agnostic once gathered. Pure byte copies — no `Value` allocation, so it is
/// safe to call while a heap payload is borrowed.
fn gather_row(schema: &PackedSchema, bytes: &[u8], index: usize, count: usize) -> Vec<u8> {
    let mut row = Vec::with_capacity(schema.byte_size);
    for (slot, kind) in schema.fields.iter().enumerate() {
        let off = schema.field_offset(index, slot, count);
        row.extend_from_slice(&bytes[off..off + kind.byte_width()]);
    }
    row
}

/// Append one packed `row` (`byte_size` bytes, slot order) to a column-major buffer, rebuilding it so
/// each field's column gains the new element at its end. O(n) — column layout trades
/// cheap append for fast bulk field math.
fn column_append(schema: &PackedSchema, buf: &[u8], row: &[u8]) -> Vec<u8> {
    let n = schema.count(buf.len());
    let mut out = Vec::with_capacity(buf.len() + schema.byte_size);
    let mut row_at = 0;
    for (slot, kind) in schema.fields.iter().enumerate() {
        let w = kind.byte_width();
        let base = n * schema.field_prefix(slot);
        out.extend_from_slice(&buf[base..base + n * w]);
        out.extend_from_slice(&row[row_at..row_at + w]);
        row_at += w;
    }
    out
}

/// Build a new column-major buffer holding the selected `indices` of a column-major buffer of `count`
/// elements — each field's column is the gather of that field across the selected
/// elements. Mirrors [`Value::packed_select`]'s row-block copy for the column layout.
fn column_select(schema: &PackedSchema, buf: &[u8], indices: &[usize], count: usize) -> Vec<u8> {
    let m = indices.len();
    let mut out = vec![0u8; m * schema.byte_size];
    for (slot, kind) in schema.fields.iter().enumerate() {
        let w = kind.byte_width();
        let new_base = m * schema.field_prefix(slot);
        for (j, &i) in indices.iter().enumerate() {
            let src = schema.field_offset(i, slot, count);
            out[new_base + j * w..new_base + j * w + w].copy_from_slice(&buf[src..src + w]);
        }
    }
    out
}

/// Overwrite element `index`'s fields in a column-major buffer of `count` elements with one packed
/// `row` (slot order), writing each field into its column.
fn column_set(schema: &PackedSchema, buf: &mut [u8], index: usize, count: usize, row: &[u8]) {
    let mut row_at = 0;
    for (slot, kind) in schema.fields.iter().enumerate() {
        let w = kind.byte_width();
        let dst = schema.field_offset(index, slot, count);
        buf[dst..dst + w].copy_from_slice(&row[row_at..row_at + w]);
        row_at += w;
    }
}

/// Concatenate two column-major buffers of the same schema into a new one: each field's
/// output column is `a`'s column followed by `b`'s. Mirrors the row path's buffer append.
fn column_concat(schema: &PackedSchema, a: &[u8], b: &[u8]) -> Vec<u8> {
    let na = schema.count(a.len());
    let nb = schema.count(b.len());
    let total = na + nb;
    let mut out = vec![0u8; total * schema.byte_size];
    for (slot, kind) in schema.fields.iter().enumerate() {
        let w = kind.byte_width();
        let prefix = schema.field_prefix(slot);
        let a_base = na * prefix;
        let b_base = nb * prefix;
        let out_base = total * prefix;
        out[out_base..out_base + na * w].copy_from_slice(&a[a_base..a_base + na * w]);
        out[out_base + na * w..out_base + (na + nb) * w]
            .copy_from_slice(&b[b_base..b_base + nb * w]);
    }
    out
}
