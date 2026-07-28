//! Fixed-width integers (Tier W) and packed layout (P-PACK) checking — an `impl Checker` split
//! out of the crate root purely to shrink `lib.rs`. Two cohesive concerns kept together because
//! both are about the unboxed-primitive surface: `IntN` literal range-checking + width-aware
//! arithmetic/comparison/bitwise synthesis (`E0044`), and `@packed` struct layout computation +
//! placement validation (`E0039`/`InvalidPackedType`). All methods are `Checker` methods moved
//! verbatim; the synthesis and declaration paths in `lib.rs` are the callers.

use super::*;

impl Checker {
    /// Range-check a fixed-width integer literal (Tier W) and return its `Type::IntN`. `negated` is
    /// set when the literal is the operand of a unary `-`, which widens a signed type's low bound to
    /// `-2^(bits-1)` (so `-128i8` is valid though bare `128i8` overflows) and makes negating an
    /// **unsigned** literal an error. Out of range / illegal negation pushes `E0044`; the type is
    /// still returned so downstream inference proceeds.
    pub(crate) fn check_intn_literal(
        &mut self,
        magnitude: u64,
        signed: bool,
        bits: u8,
        negated: bool,
        span: Span,
    ) -> Type {
        let ty = Type::IntN { signed, bits };
        let mag = magnitude as u128;
        if negated && !signed {
            self.error(
                DiagnosticCode::FixedWidthOutOfRange,
                span,
                format!("cannot negate an unsigned literal `{magnitude}{ty}`"),
            );
            return ty;
        }
        // Legal magnitude bound: unsigned `2^bits - 1`; signed positive `2^(bits-1) - 1`; a negated
        // signed literal reaches down to `-2^(bits-1)`, i.e. magnitude `2^(bits-1)`.
        let max = if !signed {
            (1u128 << bits) - 1
        } else if negated {
            1u128 << (bits - 1)
        } else {
            (1u128 << (bits - 1)) - 1
        };
        if mag > max {
            let (lo, hi) = Self::int_width_range(signed, bits);
            self.error(
                DiagnosticCode::FixedWidthOutOfRange,
                span,
                format!(
                    "literal `{}{magnitude}{ty}` is out of range for `{ty}` (valid range {lo}..={hi})",
                    if negated { "-" } else { "" },
                ),
            );
        }
        ty
    }

    /// Type a fixed-width `+ - * / %` (Tier W2/W3). Both operands must be the **same** `IntN`; the
    /// result is that type and its span is recorded in `width_sites` so lowering wraps the op into the
    /// width (`+ - *` via a `MaskWidth` on the plain result — sign-agnostic; `/ %` via the sign-aware
    /// `WideInt`, which masks internally). Mixed-width, or `IntN` with `int`/`float`, needs an explicit
    /// conversion → E0044 (a `dyn`/hole operand defers to the concrete side). Only called with at
    /// least one `IntN`.
    pub(crate) fn synth_intn_arith(
        &mut self,
        op: BinaryOp,
        lt: &Type,
        rt: &Type,
        span: Span,
    ) -> Type {
        // Pick the concrete IntN as the fallback result type (for a deferred or erroneous pairing).
        let concrete = if matches!(lt, Type::IntN { .. }) {
            lt
        } else {
            rt
        };
        // A `dyn`/hole on the other side defers to runtime — no static width to mask.
        if lt.defers_to_runtime() || rt.defers_to_runtime() {
            return concrete.clone();
        }
        if let Some((signed, bits)) = same_width_intn(lt, rt) {
            self.sites.width_sites.insert(span, (signed, bits));
            return Type::IntN { signed, bits };
        }
        self.report_intn_mismatch(op, lt, rt, "arithmetic", span);
        concrete.clone()
    }

    /// Type a fixed-width ordering comparison `< <= > >=` (Tier W3). Both operands must be the
    /// **same** `IntN`; the operand width is recorded in `width_sites` so lowering emits the
    /// sign-aware `WideInt` (unsigned ordering differs from signed past bit 63). The result is always
    /// `bool` (the caller sets it). Mixed-width, or `IntN` with `int`/`float`, needs an explicit
    /// conversion → E0044; a `dyn`/hole defers. Only called with at least one `IntN`.
    pub(crate) fn synth_intn_compare(&mut self, op: BinaryOp, lt: &Type, rt: &Type, span: Span) {
        if lt.defers_to_runtime() || rt.defers_to_runtime() {
            return;
        }
        if let Some((signed, bits)) = same_width_intn(lt, rt) {
            self.sites.width_sites.insert(span, (signed, bits));
            return;
        }
        self.report_intn_mismatch(op, lt, rt, "comparison", span);
    }

    /// Type a strict fixed-width **float** `+ - * / %` (P-NUM-SYM). Both operands must be the same
    /// fixed float (`f32`/`f64`); the result is that type. Unlike `IntN` there is no masking —
    /// `f32`/`f64` arithmetic is native — so nothing is recorded in `width_sites`, and at runtime an
    /// `f64` op is just a `float` op. Mixed (`f32`+`f64`, or a fixed float with `int`/`float`) needs
    /// an explicit conversion → E0044; a `dyn`/hole defers. Only called with at least one fixed float.
    pub(crate) fn synth_fixed_float_arith(
        &mut self,
        op: BinaryOp,
        lt: &Type,
        rt: &Type,
        span: Span,
    ) -> Type {
        let concrete = if matches!(lt, Type::F32 | Type::F64) {
            lt
        } else {
            rt
        };
        if lt.defers_to_runtime() || rt.defers_to_runtime() {
            return concrete.clone();
        }
        if lt == rt && matches!(lt, Type::F32 | Type::F64) {
            return lt.clone();
        }
        self.report_intn_mismatch(op, lt, rt, "arithmetic", span);
        concrete.clone()
    }

    /// Type a strict fixed-width **float** ordering comparison `< <= > >=` (P-NUM-SYM). Both operands
    /// must be the same fixed float; the result is `bool` (the caller sets it). Mixed → E0044; a
    /// `dyn`/hole defers. Only called with at least one fixed float.
    pub(crate) fn synth_fixed_float_compare(
        &mut self,
        op: BinaryOp,
        lt: &Type,
        rt: &Type,
        span: Span,
    ) {
        if lt.defers_to_runtime() || rt.defers_to_runtime() {
            return;
        }
        if lt == rt && matches!(lt, Type::F32 | Type::F64) {
            return;
        }
        self.report_intn_mismatch(op, lt, rt, "comparison", span);
    }

    /// Type a fixed-width symmetric bitwise op `& | ^` (Tier W5). Both operands must be the **same**
    /// `IntN` and the result is that type; unlike shifts and arithmetic, the erased `& | ^` of two
    /// correctly-extended words is already correctly extended, so **no mask** (and no `width_sites`
    /// entry) is needed. Mixed-width or `IntN`+`int` → E0044; a `dyn`/hole defers. Only called with
    /// at least one `IntN`.
    pub(crate) fn synth_intn_bitwise(
        &mut self,
        op: BinaryOp,
        lt: &Type,
        rt: &Type,
        span: Span,
    ) -> Type {
        let concrete = if matches!(lt, Type::IntN { .. }) {
            lt
        } else {
            rt
        };
        if lt.defers_to_runtime() || rt.defers_to_runtime() {
            return concrete.clone();
        }
        if let Some((signed, bits)) = same_width_intn(lt, rt) {
            return Type::IntN { signed, bits };
        }
        self.report_intn_mismatch(op, lt, rt, "bitwise", span);
        concrete.clone()
    }

    /// The E0043 for a non-fixed-width bitwise/shift op whose operands are not `int` (P-BITS Tier B).
    pub(crate) fn report_noninteger_bitwise(
        &mut self,
        op: BinaryOp,
        lt: &Type,
        rt: &Type,
        span: Span,
    ) {
        self.error(
            DiagnosticCode::NonIntegerBitwise,
            span,
            format!(
                "`{}` requires integer operands, but found `{lt}` and `{rt}`",
                op.symbol(),
            ),
        );
    }

    /// The shared E0044 for a fixed-width op whose operands are not the same `IntN` — `kind` is
    /// `"arithmetic"`, `"comparison"`, or `"bitwise"` (the W2/W3/W5 sites).
    fn report_intn_mismatch(&mut self, op: BinaryOp, lt: &Type, rt: &Type, kind: &str, span: Span) {
        self.error(
            DiagnosticCode::FixedWidthOutOfRange,
            span,
            format!(
                "cannot apply `{}` to `{lt}` and `{rt}`: fixed-width {kind} requires both \
                 operands to be the same type — convert explicitly",
                op.symbol(),
            ),
        );
    }

    /// The inclusive `(min, max)` value range of a fixed-width integer type, as `i128` so every
    /// width (including `u64`) fits — used only for diagnostic text.
    pub(crate) fn int_width_range(signed: bool, bits: u8) -> (i128, i128) {
        if signed {
            (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
        } else {
            (0, (1i128 << bits) - 1)
        }
    }

    /// The packed layout of a **list element** type `elem` — a `@packed` struct (via [`packed_layout`])
    /// **or** a bare sub-8-byte fixed-width numeric ([`scalar_packed_kind`]), the latter as a scalar
    /// (`no-struct`) layout. This is the gate for `List<T>` *literal* construction and `from_bytes`,
    /// where a bare `List<i32>`/`List<u8>`/`List<f32>` earns a compact flat buffer (packed-widths
    /// bare-scalar arc). `note_map_packed` and nested struct-field recursion deliberately keep using the
    /// struct-only `packed_layout`, so a mapped scalar result stays boxed (behaviourally identical) and a
    /// scalar struct field is still resolved by its field-kind, not by recursion.
    ///
    /// [`packed_layout`]: Self::packed_layout
    pub(crate) fn packed_list_layout(
        &self,
        elem: &Type,
    ) -> Option<noeta_ast::reflect::PackedLayout> {
        if let Some(kind) = scalar_packed_kind(elem) {
            return Some(noeta_ast::reflect::PackedLayout::scalar(kind));
        }
        self.packed_layout(elem)
    }

    pub(crate) fn packed_layout(&self, ty: &Type) -> Option<noeta_ast::reflect::PackedLayout> {
        use noeta_ast::reflect::{PackedField, PackedKind, PackedLayout};
        let Type::Named(name, args) = ty else {
            return None;
        };
        if !args.is_empty() || !self.symbols.packed_structs.contains(name) {
            return None;
        }
        let mut fields = Vec::new();
        for (fname, fty) in self.symbols.records.get(name)? {
            let kind = match fty {
                Type::Int => PackedKind::Int,
                Type::Float => PackedKind::Float,
                Type::F32 => PackedKind::F32,
                Type::F64 => PackedKind::F64,
                Type::IntN { signed, bits } => PackedKind::IntN {
                    bits: *bits,
                    signed: *signed,
                },
                Type::Bool => PackedKind::Bool,
                Type::Named(..) => PackedKind::Struct(Box::new(self.packed_layout(fty)?)),
                _ => return None,
            };
            fields.push(PackedField {
                name: fname.clone(),
                kind,
            });
        }
        Some(PackedLayout {
            type_name: name.clone(),
            fields,
            column: self.symbols.column_structs.contains(name),
        })
    }

    /// Record a list-construction site at `span` if its element type `elem` is a packed struct
    /// (P-PACK Phase 2) — the span both backends key on to pick the flat raw-buffer representation.
    pub(crate) fn note_packed_list(&mut self, elem: &Type, span: Span) {
        if let Some(layout) = self.packed_list_layout(elem) {
            self.sites.packed_list_sites.insert(span, layout);
        }
    }

    /// Record the resolved `TypeRepr` at a collection/object construction site (runtime type-arg
    /// reflection, slice A — see [`Checked::construction_sites`]). A hole/`dyn`-top type is skipped, so
    /// the value stays untagged and `type_of`/`is` fall back to the head-only runtime classification.
    /// A **non-generic nominal** type (a `struct`/`class`/`enum` with no type arguments, R2) is also
    /// skipped: its head-only runtime classification already recovers the type in full (the shape name
    /// with empty args), so tagging it would add per-instance overhead for no fidelity gain. Only a
    /// generic instantiation (`Box<int>` → `Struct("Box", [Int])`) and the collections (whose element
    /// types are always erased at runtime) carry a tag.
    pub(crate) fn note_construction(&mut self, ty: &Type, span: Span) {
        // Erase any in-scope generic type **parameter** to `dyn` first (R2): a literal inside a generic
        // constructor (`Holder { item: x }` where `x: T`) has type `Holder<T>`, and the concrete `T` is
        // not known at the literal's site — only at the call. Recording it as `Holder<T>` would present
        // the *parameter name* as if it were a concrete type; erasing to `Holder<dyn>` is the honest
        // runtime fidelity. A direct literal whose args the checker inferred concretely (`Box { value:
        // 5 }` → `Box<int>`) is unaffected (it has no in-scope param to erase).
        let params: HashSet<String> = self.coloring.type_params.keys().cloned().collect();
        let ty = erase_type_params(ty.clone(), &params);
        if let Some(repr) = type_to_repr_top(&ty, &self.symbols.type_kinds) {
            if is_nongeneric_nominal(&repr) {
                return;
            }
            self.sites.construction_sites.insert(span, repr);
        }
    }

    /// Record a **generic constructor call** as a construction site (generic constructor
    /// reflection): `Repo.new("todos")` resolved to `Repo<Todo>` tags the object the call returns,
    /// recovering the instantiation the constructor body cannot see (inside `fn new` the literal's
    /// type is `Repo<T>`, with `T` still a parameter).
    ///
    /// Three conditions, all of them load-bearing:
    ///
    /// * `Type.method` is a **provable fresh constructor** ([`crate::constructors`]) — every
    ///   `return` hands back a new literal — so the returned object is this call's alone and the
    ///   backends may write its tag in place. A factory returning a shared instance is excluded:
    ///   two differently-instantiated call sites would otherwise overwrite each other's answer.
    /// * the resolved result is that very type, generically instantiated;
    /// * every argument is **fully concrete** — no `dyn`, no inference hole, no enclosing type
    ///   parameter. A partially-open instantiation is left untagged rather than recorded as
    ///   `Repo<dyn>`: erasure is what the value already reports, and inventing a `dyn` argument
    ///   would claim a fact the call site does not have.
    pub(crate) fn note_constructor_call(
        &mut self,
        type_name: &str,
        method: &str,
        ret: &Type,
        span: Span,
    ) {
        if !self
            .symbols
            .fresh_constructors
            .contains(&(type_name.to_string(), method.to_string()))
        {
            return;
        }
        let Type::Named(n, args) = ret else { return };
        if n != type_name || args.is_empty() || !args.iter().all(|a| self.fully_concrete(a)) {
            return;
        }
        self.note_construction(ret, span);
    }

    /// Whether `ty` names a fully-determined type: no `dyn`/`dyn Trait` top, no inference hole, and
    /// no mention of a type parameter in scope. The gate on recording a constructor call's
    /// instantiation — a tag is a claim about the value, so a partially-open type makes no claim.
    pub(crate) fn fully_concrete(&self, ty: &Type) -> bool {
        if matches!(ty, Type::Dyn | Type::Unknown | Type::DynTrait(_)) {
            return false;
        }
        if self.mentions_in_scope_param(ty) {
            return false;
        }
        match ty {
            Type::Named(_, args) | Type::Tuple(args) | Type::Union(args) => {
                args.iter().all(|a| self.fully_concrete(a))
            }
            Type::List(e) | Type::Set(e) | Type::Option(e) => self.fully_concrete(e),
            Type::Map(k, v) | Type::Result(k, v) => {
                self.fully_concrete(k) && self.fully_concrete(v)
            }
            Type::Fn { params, ret } => {
                params.iter().all(|p| self.fully_concrete(p)) && self.fully_concrete(ret)
            }
            _ => true,
        }
    }

    /// Record a `map(...)` call site at `span` if its result element type `elem` is a packed struct
    /// (P-PACK 2.6 category B) — the span the VM's `map` builtin keys on to build a flat result.
    pub(crate) fn note_map_packed(&mut self, elem: &Type, span: Span) {
        if let Some(layout) = self.packed_layout(elem) {
            self.sites.map_packed_sites.insert(span, layout);
        }
    }

    /// Validate a `@packed` struct's all-primitive field constraint (P-PACK, `E0038`). A no-op for an
    /// ordinary (non-packed) struct. Runs after `collect`, so a field naming a packed struct declared
    /// later resolves.
    pub(crate) fn check_packed_struct(&mut self, r: &StructDecl) {
        if r.decorators.packed.is_none() {
            return;
        }
        for f in &r.fields {
            let ty = field_type(&f.ty, &self.imports.extern_types);
            if !self.is_packable_type(&ty) {
                self.error(
                        DiagnosticCode::InvalidPackedType,
                        f.span,
                        format!(
                            "field `{}: {ty}` of packed struct `{}` is not a packable type",
                            f.name, r.name
                        ),
                    )
                    .help(
                        "a `@packed` struct's fields must be primitives (`int`, `float`, `bool`, a fixed width like `i32`/`u8`/`f64`, or `f32`) or other packed structs",
                    );
            }
        }
    }
}

/// The [`PackedKind`] a **bare scalar** list element packs to, or `None` if `ty` is not a compaction
/// candidate (packed-widths bare-scalar arc). Only fixed-width numerics **narrower than 8 bytes**
/// qualify — `i8 u8 i16 u16 i32 u32` (widths 1/2/4) and `f32` (4). A `i64`/`u64`/`f64` element is
/// already 8 bytes when boxed, so packing it buys **zero** storage and only adds materialization cost;
/// the width is still reified through slice 1's construction tag, so no reflection fidelity is lost.
/// `int`/`float` are the hot-path erased defaults (no width to store, no win) and `bool` is out of
/// scope for this slice. A scalar element has no struct wrapper — see [`PackedLayout::scalar`].
///
/// [`PackedKind`]: noeta_ast::reflect::PackedKind
/// [`PackedLayout::scalar`]: noeta_ast::reflect::PackedLayout::scalar
fn scalar_packed_kind(ty: &Type) -> Option<noeta_ast::reflect::PackedKind> {
    use noeta_ast::reflect::PackedKind;
    match ty {
        Type::F32 => Some(PackedKind::F32),
        Type::IntN { bits, signed } if *bits < 64 => Some(PackedKind::IntN {
            bits: *bits,
            signed: *signed,
        }),
        _ => None,
    }
}
