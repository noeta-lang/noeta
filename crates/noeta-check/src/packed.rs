//! Fixed-width integers (Tier W) and packed layout (P-PACK) checking — an `impl Checker` split
//! out of the crate root purely to shrink `lib.rs`. Two cohesive concerns kept together because
//! both are about the unboxed-primitive surface: `IntN` literal range-checking + width-aware
//! arithmetic/comparison/bitwise synthesis (`E0044`), and `@packed` struct layout computation +
//! placement validation (`E0039`/`InvalidPackedType`). All methods are `Checker` methods moved
//! verbatim; the synthesis and declaration paths in `lib.rs` are the callers.

use super::*;

/// Which walk a [`noeta_ast::RenderHint`] is being built for — the one axis on which the hints
/// differ.
///
/// A hint's `Slots` numbers are **positions in the value the walk sees**, and not every walk sees
/// the same object: one that reads a value's own slots counts every declared field, while the deep
/// marshal a JSON encoding runs on drops the `#[Transient]` ones. Everything else — the widths that
/// need a hint, the collection and enum shapes, the cycle cut — is identical, so one builder answers
/// all of them with this as its only branch.
///
/// There are three door kinds and two numberings: display and ordering share one, JSON has its own.
/// The variants are named for the numbering's original door rather than split three ways, because a
/// third variant equal to [`HintPurpose::Display`] in every respect would be a second place to keep
/// in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HintPurpose {
    /// The walks that read a value's **own declared slots**, `#[Transient]` fields included: the
    /// display doors (`echo`, an interpolation hole, a display-based `~` operand) and the ordering
    /// doors (`.sorted()`, `.min()`, `.max()`, `.keys()`, `.values()`, a rendered set or map, a
    /// `for` over one), which compare two values slot by slot against the object itself.
    Display,
    /// The walks that read the **marshalled** value, whose `#[Transient]` fields are gone: the JSON
    /// doors — the `json.stringify` argument, a derived `to_json()` or `inspect()` receiver.
    Json,
}

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

    /// Record the [`RenderHint`](noeta_ast::RenderHint) for a value **about to be displayed** at
    /// `span` (an `echo`, an interpolation hole, a display-based `~` operand), if its static type
    /// contains an unsigned 64-bit integer. Nothing is recorded otherwise — the overwhelmingly
    /// common case — so a program that never displays a `u64` carries no hint and lowers unchanged.
    ///
    /// This is the display counterpart of the `width_sites` recording on fixed-width arithmetic:
    /// both exist because the width and signedness of an `IntN` live only in the type, and the
    /// operations that need them (division, ordering, and rendering) must be told.
    pub(crate) fn note_render_hint(&mut self, ty: &Type, span: Span) {
        // A **declared** type implementing `Display` renders through its own `to_string`, whose body
        // does its own displaying (and takes its own hints there). Hinting the site would render the
        // value structurally instead, silently replacing the type's chosen form. The test is
        // deliberately narrow — a named type, which is exactly what the runtime dispatches on (an
        // object or enum value) — because every built-in type satisfies `Display` too, and exempting
        // those would exempt the whole surface. Only the *outermost* type is exempt: a value nested
        // in a collection or field renders with `repr`, which never dispatches `Display`.
        if matches!(ty, Type::Named(..)) && self.satisfies(ty, BuiltinTrait::Display) {
            return;
        }
        if let Some(hint) = self.render_hint(ty, HintPurpose::Display, &mut Vec::new()) {
            self.sites.render_hint_sites.insert(span, hint);
        }
    }

    /// Record the [`RenderHint`](noeta_ast::RenderHint) for a value **about to be serialized as
    /// JSON** at `span` (the `json.stringify` argument, a derived `to_json()` or `inspect()`
    /// receiver), if its static type contains an unsigned 64-bit integer. The JSON twin of
    /// [`Self::note_render_hint`], and
    /// sparse in the same way: nothing is recorded for a type with no such integer under it, so a
    /// program that never serializes a `u64` lowers unchanged.
    ///
    /// **No `Display` exemption.** A JSON encoding is structural at every depth — a declared type's
    /// own `to_string` has no part in it — so a type implementing `Display` is hinted here exactly
    /// like any other.
    pub(crate) fn note_json_hint(&mut self, ty: &Type, span: Span) {
        if let Some(hint) = self.render_hint(ty, HintPurpose::Json, &mut Vec::new()) {
            self.sites.json_hint_sites.insert(span, hint);
        }
    }

    /// Record the JSON hint for a call to the native `json.stringify(value)`, whichever way its
    /// callee was spelled — qualified (`json.stringify(v)`), selectively imported (`stringify(v)`),
    /// or a resolved native forward (the body a `@derive(Inspect)` synthesizes). One helper for all
    /// three because they are one door: the same function, serializing the same argument, and a
    /// hint that reached only one spelling would leave the others writing the signed word.
    pub(crate) fn note_json_stringify_hint(
        &mut self,
        module: &str,
        func: &str,
        args: &[Type],
        call_span: Span,
    ) {
        if func != "stringify" || self.reg().find_module(module).map(|m| m.name) != Some("json") {
            return;
        }
        if let [value] = args {
            self.note_json_hint(value, call_span);
        }
    }

    /// Record the [`RenderHint`](noeta_ast::RenderHint) for a value **about to be ordered** at
    /// `span` (`.sorted()`, `.min()`, `.max()`, `.keys()`, `.values()`, a `for` over a set or map),
    /// if `ty` contains an unsigned 64-bit integer. Nothing is recorded otherwise, so a program that
    /// never orders a `u64` carries no hint and lowers unchanged.
    ///
    /// [`HintPurpose::Display`] is the numbering, deliberately: an ordering walk compares two values
    /// slot by slot against the object's **own** slot array (`Payload::Object`'s slots in the VM, the
    /// `TypeDef`'s field specs in the tree-walker) — the deep marshal never runs, so a `#[Transient]`
    /// field is present and counted exactly as a display renders it. Taking the JSON numbering here
    /// would shift every hint after a transient field onto the wrong slot, silently.
    ///
    /// The same structural hint [`Self::note_render_hint`] records, without its `Display`-trait
    /// exemption: a type's own `to_string` decides how it *prints*, never how it *orders* (ordering
    /// is `Comparable`, and a hand-written `compare` is dispatched before this walk is reached).
    pub(crate) fn note_order_hint(&mut self, ty: &Type, span: Span) {
        if let Some(hint) = self.render_hint(ty, HintPurpose::Display, &mut Vec::new()) {
            self.sites.order_hint_sites.insert(span, hint);
        }
    }

    /// Build the render hint for `ty`, or `None` when nothing under it is an unsigned 64-bit
    /// integer. `stack` carries the named types already being expanded, so a self-referential type
    /// (`class Node { next: ?Node }`) terminates instead of recursing forever; the cut is safe
    /// because a cycle adds no *new* position — every position it could reach is already described
    /// by the outer expansion of the same type.
    ///
    /// Only 64-bit unsigned values need one: every `u8`/`u16`/`u32` value fits in an i64 word with
    /// room to spare, so it already renders correctly, and every *signed* width is exactly what the
    /// erased word is.
    ///
    /// `purpose` decides how an object's slots are numbered — see [`HintPurpose`]; every other
    /// position is identical for both walks, which is why they are one function.
    fn render_hint(
        &self,
        ty: &Type,
        purpose: HintPurpose,
        stack: &mut Vec<String>,
    ) -> Option<noeta_ast::RenderHint> {
        use noeta_ast::RenderHint;
        match ty {
            Type::IntN {
                signed: false,
                bits: 64,
            } => Some(RenderHint::Unsigned),
            Type::List(e) | Type::Set(e) => Some(RenderHint::Elements(Box::new(
                self.render_hint(e, purpose, stack)?,
            ))),
            Type::Map(k, v) => {
                let key = self.render_hint(k, purpose, stack).map(Box::new);
                let value = self.render_hint(v, purpose, stack).map(Box::new);
                (key.is_some() || value.is_some()).then_some(RenderHint::Entries { key, value })
            }
            Type::Tuple(items) => {
                RenderHint::slots(items.iter().map(|t| self.render_hint(t, purpose, stack)))
            }
            // `?T` and `Result<T, E>` are enums at runtime, so they render through their variant
            // names exactly as a user enum does — `some`/`Ok`/`Err` carry the payload in slot 0.
            Type::Option(inner) => self
                .render_hint(inner, purpose, stack)
                .map(|h| RenderHint::Variants(vec![("some".to_string(), vec![(0, h)])])),
            Type::Result(ok, err) => {
                let variants: Vec<(String, Vec<(u32, RenderHint)>)> = [
                    ("Ok", self.render_hint(ok, purpose, stack)),
                    ("Err", self.render_hint(err, purpose, stack)),
                ]
                .into_iter()
                .filter_map(|(name, h)| h.map(|h| (name.to_string(), vec![(0, h)])))
                .collect();
                (!variants.is_empty()).then_some(RenderHint::Variants(variants))
            }
            Type::Named(name, args) => self.named_render_hint(name, args, purpose, stack),
            _ => None,
        }
    }

    /// The render hint for a declared struct/class (its fields, as positional slots) or enum (its
    /// variants' payloads), at the instantiation `args`. Split from [`Self::render_hint`] to keep
    /// the cycle guard's push/pop in one place.
    fn named_render_hint(
        &self,
        name: &str,
        args: &[Type],
        purpose: HintPurpose,
        stack: &mut Vec<String>,
    ) -> Option<noeta_ast::RenderHint> {
        use noeta_ast::RenderHint;
        if stack.iter().any(|n| n == name) {
            return None;
        }
        stack.push(name.to_string());
        let subst = self.type_arg_subst(name, args);
        let at = |t: &Type| crate::subst::apply_subst(t, &subst);
        let transient = self.symbols.transient_fields.get(name);
        let hint = match self.symbols.records.get(name) {
            Some(fields) => {
                let hints: Vec<Option<RenderHint>> = fields
                    .iter()
                    // A `#[Transient]` field is absent from the marshalled value entirely, so a JSON
                    // walk must not count it: the slot numbers are positions in the value being
                    // walked, and one extra slot shifts every field after it onto the wrong hint.
                    // A display walk renders the object's declared fields, transient ones included,
                    // and numbers them all.
                    .filter(|(fname, _)| {
                        purpose == HintPurpose::Display
                            || !transient.is_some_and(|t| t.contains(fname))
                    })
                    .map(|(_, fty)| self.render_hint(&at(fty), purpose, stack))
                    .collect();
                RenderHint::slots(hints)
            }
            None => self.symbols.enums.get(name).and_then(|variants| {
                let variants: Vec<(String, Vec<(u32, RenderHint)>)> = variants
                    .iter()
                    .filter_map(|v| {
                        let slots: Vec<(u32, RenderHint)> = v
                            .fields
                            .iter()
                            .enumerate()
                            .filter_map(|(i, fty)| {
                                self.render_hint(&at(fty), purpose, stack)
                                    .map(|h| (i as u32, h))
                            })
                            .collect();
                        (!slots.is_empty()).then(|| (v.name.clone(), slots))
                    })
                    .collect();
                (!variants.is_empty()).then_some(RenderHint::Variants(variants))
            }),
        };
        stack.pop();
        hint
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
        let ty = erase_type_params(ty.clone(), &self.scope_param_ids());
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
    ///
    /// The third condition has exactly one **narrowing**, and it is what makes a generic type able to
    /// build another generic type out of its own parameter: an instantiation left open *only* by
    /// **forwardable** in-scope parameters, for which this body carries a hidden type-argument slot
    /// whose template is this very instantiation, is recorded as a
    /// [`dynamic construction site`](crate::Sites::dynamic_construction_sites). Nothing is invented —
    /// the concrete `TypeRepr` is still resolved and interned *statically*, by the outer call that
    /// pins `T`; only *which* interned entry applies is read from the slot. A parameter with no
    /// channel to fill it (an instance method's, whose class parameters ride the receiver's tag, or a
    /// position the pre-pass does not see) still records nothing, and [`Self::report_unrecordable`]
    /// turns the run-time abort that would follow into a check-time diagnostic.
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
        if n != type_name || args.is_empty() {
            return;
        }
        if args.iter().all(|a| self.fully_concrete(a)) {
            self.note_construction(ret, span);
            return;
        }
        // Open by an in-scope parameter. The slot channel is the only thing that can still deliver
        // the instantiation, and the template match is the whole gate: a slot exists for this exact
        // instantiation only because the forwarding pre-pass saw a *declared* position demanding it,
        // and the outer call resolved that template against its own (concrete) instantiation.
        if let Some(slot) = self.dynamic_ctor_slot(ret) {
            self.sites.dynamic_construction_sites.insert(span, slot);
            return;
        }
        self.report_unrecordable(type_name, method, ret, span);
    }

    /// Whether `ty` is concrete **except** for bare mentions of the type parameters `allowed` — the
    /// dynamic half of [`Self::fully_concrete`], parameterized over *which* openness is acceptable.
    ///
    /// Two callers, one judgment. Passing [`Coloring::forwardable_params`] asks "can the slot channel
    /// still deliver this?" — the gate on recording a dynamic construction site. Passing every
    /// in-scope parameter asks "is the only thing missing an instantiation?" — the gate on
    /// [`Self::report_unrecordable`], which must stay silent where the openness is an ordinary
    /// inference hole instead (the deliberate bottom-up path a `fn keep<T>(x: T)` argument takes).
    ///
    /// Deliberately as strict as its static twin everywhere else: a `dyn`, a `dyn Trait` or an
    /// inference hole is refused wherever it appears, and so is a mention of an in-scope parameter
    /// outside `allowed`.
    pub(crate) fn open_only_by_params(&self, ty: &Type, allowed: &[ParamRef]) -> bool {
        if let Type::Param(p) = ty {
            return allowed.contains(p);
        }
        if matches!(ty, Type::Dyn | Type::Unknown | Type::DynTrait(_)) {
            return false;
        }
        let rec = |t: &Type| self.open_only_by_params(t, allowed);
        match ty {
            // A composite's HEAD may not itself be a parameter (`T<int>` is not a type this
            // language spells, but a bare `T` reaching here has already failed the check above —
            // it is an in-scope parameter no slot carries).
            Type::Named(n, args) => {
                !self.mentions_in_scope_param(&Type::Named(n.clone(), Vec::new()))
                    && args.iter().all(rec)
            }
            Type::Tuple(args) | Type::Union(args) => args.iter().all(rec),
            Type::List(e) | Type::Set(e) | Type::Option(e) => rec(e),
            Type::Map(k, v) | Type::Result(k, v) => rec(k) && rec(v),
            Type::Fn { params, ret } => params.iter().all(&rec) && rec(ret),
            _ => true,
        }
    }

    /// Whether `ty` is concrete **except** for `dyn`/unresolved holes — the third openness, and the
    /// one that means *no instantiation reached this construction*.
    ///
    /// The two are one bucket because by the time a call's result is in hand they are literally the
    /// same type: `subst_or_dyn` erases a type parameter no argument, receiver or expectation bound
    /// to `dyn`, so `r = Repo.new("t")` and `r: Repo<dyn> = Repo.new("t")` both arrive here as
    /// `Repo<dyn>` with nothing left to tell them apart. It costs nothing to conflate them, because
    /// the consequence is identical: a `Repo` that reads `type_name::<T>()` cannot answer for an
    /// erased `T` either way, and aborts.
    ///
    /// The sibling of [`Self::open_only_by_params`], and deliberately as strict everywhere else. A
    /// `dyn Trait` is refused — a trait object names a real bound and the head-only runtime
    /// classification still describes it, so it is not the missing-answer shape — and so is a
    /// mention of an in-scope type parameter, which is the other function's case.
    pub(crate) fn open_only_by_erasure(&self, ty: &Type) -> bool {
        if matches!(ty, Type::Unknown | Type::Dyn) {
            return true;
        }
        if matches!(ty, Type::DynTrait(_)) {
            return false;
        }
        let rec = |t: &Type| self.open_only_by_erasure(t);
        match ty {
            // A parameter that is IN SCOPE here is open by the parameter, not by erasure — that is
            // the sibling function's case, and the caller picks a different (more precise)
            // diagnostic for it. This arm was implicit while a parameter was a `Named` and the head
            // was tested against the in-scope name set; without it a `Type::Param` falls into the
            // `_ => true` below and every such construction reports "records no type argument"
            // instead of "`T` is a type parameter of the enclosing member".
            Type::Param(p) => !self.param_in_scope(p),
            Type::Named(_, args) => args.iter().all(rec),
            Type::Tuple(args) | Type::Union(args) => args.iter().all(rec),
            Type::List(e) | Type::Set(e) | Type::Option(e) => rec(e),
            Type::Map(k, v) | Type::Result(k, v) => rec(k) && rec(v),
            Type::Fn { params, ret } => params.iter().all(&rec) && rec(ret),
            _ => true,
        }
    }

    /// Report a fresh generic construction **nothing supplied an instantiation for** (`E0058`) — the
    /// call that used to check clean and abort at run time on the first `type_name::<T>()`.
    ///
    /// It existed as a hole for one reason: there was no way to say the type at the call site, so
    /// erroring would have rejected code with no fix. `Repo::<Todo>.new(…)` is that fix, which is
    /// what turns silence into a diagnostic here — and the help names the new spelling first,
    /// because it is the only one that works without moving the call.
    ///
    /// Guarded by the same `reflective_generic_types` filter as its sibling: a generic type that
    /// never asks what `T` is does not care whether it carries a tag, so the erasure is harmless and
    /// erroring would be noise.
    fn report_unsupplied_instantiation(&mut self, type_name: &str, method: &str, span: Span) {
        let params = self
            .symbols
            .generic_types
            .get(type_name)
            .cloned()
            .unwrap_or_default();
        let names = params
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join("`, `");
        let is_are = if params.len() == 1 { "is" } else { "are" };
        self.error(
            DiagnosticCode::InvalidTypeArguments,
            span,
            format!(
                "this `{type_name}` records no type argument for `{names}`: nothing at or around \
                 this call supplies an instantiation, so `{names}` {is_are} erased and the object \
                 can never report it"
            ),
        )
        .help(format!(
            "state it AT the call — `{type_name}::<...>.{method}(args)` — or put the call in a \
             position that declares the type: an annotated binding \
             (`r: {type_name}<...> = …`), a declared `return`, a field's declared type, or a \
             parameter's"
        ));
    }

    /// Report a fresh generic construction whose instantiation is **structurally unrecordable**
    /// (`E0058`) — the check-time replacement for a run-time abort, and the reason it is worth an
    /// error rather than silence.
    ///
    /// The situation: this call freshly builds a `G<…>` whose type arguments are known to be
    /// in-scope type *parameters* — not inference holes, so nothing is waiting to be inferred — and
    /// no channel here can carry their instantiation to the value. The tag therefore provably never
    /// gets written, and the first `type_name::<T>()` inside `G` aborts. Before this it checked clean
    /// and failed at run time, which is the very check/run divergence the construction-site tag
    /// exists to close.
    ///
    /// Two guards keep it from being noise:
    ///
    /// * `G` must actually **read one of its own parameters reflectively**
    ///   ([`crate::forwarding::reflective_generic_types`]). A generic type that never asks what `T`
    ///   is does not care whether it carries a tag, and erroring there would reject working code.
    /// * the openness must be by a parameter, not a hole — the caller checks that, because an
    ///   un-inferred argument is the deliberate bottom-up path (`keep(Inner.new("todos"))`), whose
    ///   run-time abort `constructor_type_arg_open_parameter` pins on purpose.
    ///
    /// The message names the real cause, which differs by channel, and the route that works.
    fn report_unrecordable(&mut self, type_name: &str, method: &str, ret: &Type, span: Span) {
        if !self.symbols.reflective_generic_types.contains(type_name) {
            return;
        }
        let in_scope: Vec<ParamRef> = self
            .coloring
            .type_params
            .params()
            .map(|s| s.param.clone())
            .collect();
        let Type::Named(_, args) = ret else { return };
        // ERASED rather than open by a parameter: no instantiation reached this call at all —
        // nothing is waiting to be inferred, and the object provably never gets a tag. That is the
        // check/run divergence this arc closes, and it is now sayable at the call site, so it gets
        // its own diagnostic.
        if args.iter().all(|a| self.open_only_by_erasure(a)) {
            self.report_unsupplied_instantiation(type_name, method, span);
            return;
        }
        if !args.iter().all(|a| self.open_only_by_params(a, &in_scope)) {
            return;
        }
        // Which parameters are open here, and whether this member could forward them at all. A
        // self-less member CAN (the hidden slot is its channel) and reached here because the
        // *position* is not one the forwarding pre-pass reads a declared type from; an instance
        // method cannot, because its class parameters ride the receiver's reflected tag and a tag on
        // the receiver cannot become a tag on something the body constructs.
        //
        // Non-empty by construction: the caller reached here because some argument failed
        // `fully_concrete`, and the check above already refused every other way that can fail (a
        // `dyn`, a `dyn Trait`, an inference hole), leaving a parameter mention as the only cause.
        let open: Vec<ParamRef> = in_scope
            .iter()
            .filter(|p| {
                let one: ParamSet = std::iter::once(p.id).collect();
                args.iter().any(|a| mentions_param(a, &one))
            })
            .cloned()
            .collect();
        let names = open
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join("`, `");
        let is_are = if open.len() == 1 {
            "is a type parameter"
        } else {
            "are type parameters"
        };
        let forwardable = open
            .iter()
            .all(|p| self.coloring.forwardable_params.contains(p));
        let d = self.error(
            DiagnosticCode::InvalidTypeArguments,
            span,
            format!(
                "this `{type_name}` cannot record its instantiation: `{names}` {is_are} of the \
                 enclosing declaration, and no instantiation reaches this position"
            ),
        );
        if forwardable {
            d.help(format!(
                "a self-less member carries its instantiation on a hidden slot, resolved from the \
                 DECLARED type of the position the construction stands in — a field's type, a \
                 declared `return`, or an annotated binding. Bind it first \
                 (`r: {type_name}<{names}> = …`) and use the binding here"
            ));
        } else {
            d.help(format!(
                "`{names}` reaches this body on the RECEIVER's reflected tag, which can describe \
                 `self` but cannot tag an object this body builds. Construct the \
                 `{type_name}<{names}>` in a self-less member instead (its caller pins the \
                 instantiation), or take it as a parameter"
            ));
        }
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
            let ty = self.annot_field(&f.ty);
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
