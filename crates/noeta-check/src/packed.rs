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

    /// Record the [`RenderHint`](noeta_ast::RenderHint) for a value a native method **binds now and
    /// serializes later** — the argument at the index its receiver's `ExtType::push_hint_args`
    /// declares (`view.expose(name, signal)`, whose value is pushed as JSON on every later flush
    /// tick). The same structural hint [`Self::note_json_hint`] records, for the same walk and under
    /// the same [`HintPurpose::Json`] numbering, because the serialization it feeds *is* that walk —
    /// only the moment differs, which is the whole reason this door needs its own site map: by the
    /// time the value is written there is no call site left to read a static type from.
    ///
    /// A hint mentioning a **type parameter** is recorded like any other and resolved at the
    /// **binding call**, not at the tick that serializes. The later tick has no frame to read a
    /// render slot from, but the call that binds the value does — and it is the site that knows the
    /// instantiation, since that is where the generic body was entered. Both backends splice there
    /// and keep the already-resolved hint, so a `view.expose` of a `T = u64` pushes the number on
    /// every tick instead of the negative word it is erased to.
    pub(crate) fn note_binding_hint(&mut self, ty: &Type, span: Span) {
        if let Some(hint) = self.render_hint(ty, HintPurpose::Json, &mut Vec::new()) {
            self.sites.binding_hint_sites.insert(span, hint);
        }
    }

    /// Record the [`RenderHint`](noeta_ast::RenderHint) for a value a **session echoes** — the
    /// trailing bare expression of a REPL / debug-console entry, rendered by the host once the
    /// entry has run. The display twin of [`Self::note_render_hint`] for the one display door the
    /// program does not contain, and it needs its own site map because lowering consumes
    /// `render_hint_sites` (an entry marked there would have its value replaced by a *string*).
    ///
    /// **No `Display`-trait exemption**, unlike [`Self::note_render_hint`]: a session echoes a value
    /// structurally — `Gauge {v: 1}`, never the type's own `to_string` — so a named type
    /// implementing `Display` is hinted here exactly like any other, and its fields are numbered as
    /// the echo renders them ([`HintPurpose::Display`]).
    ///
    /// A hint mentioning a **type parameter** cannot arise here, and the guard says so rather than
    /// assuming it. A render slot exists only inside a generic body — the enclosing `fn`'s hidden
    /// slots, or the receiver channel of a generic type's instance method — and the echoed
    /// statement is the entry's **trailing top-level statement**, which is inside neither. So there
    /// is no instantiation to name, no frame that could name one, and nothing to resolve: a hint
    /// that named a parameter would resolve to nothing on every entry, which is the unhinted
    /// reading spelled the long way. Dropped rather than stored, so nothing downstream has to
    /// re-derive that.
    pub(crate) fn note_echo_hint(&mut self, ty: &Type, span: Span) {
        if let Some(hint) = self.render_hint(ty, HintPurpose::Display, &mut Vec::new())
            && !hint.has_param()
        {
            self.sites.echo_hint_sites.insert(span, hint);
        }
    }

    /// Record the [`RenderHint`](noeta_ast::RenderHint) for a value **about to be ordered** at
    /// `span` (`.sorted()`, `.min()`, `.max()`, `.keys()`, `.values()`, a `for` over a set or map),
    /// if `ty` contains an unsigned 64-bit integer. Nothing is recorded otherwise, so a program that
    /// never orders a `u64` carries no hint and lowers unchanged.
    ///
    /// One arithmetic door reads the same hint at the same kind of span: `checked_sum` folds at the
    /// element width, and a `u64` past bit 63 is a negative word, so a signed fold finds no overflow
    /// where the type says there is one. Same question, same answer, same channel — see
    /// [`crate::stdlib::folds_at_element_width`].
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
            // An `Iterator<T>` describes its elements exactly as a `List<T>` does — the ordering
            // doors on one (`min`/`max`) read the element hint through the same `Elements` shape as
            // the doors on the other, so an iterator terminal cannot read a `u64` differently from
            // its eager twin.
            Type::Named(n, args) if n == crate::stdlib::ITERATOR => Some(RenderHint::Elements(
                Box::new(self.render_hint(args.first()?, purpose, stack)?),
            )),
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
            // A **type parameter**: the one position whose width is not in the type at all. Erased
            // generics give one compiled body to every instantiation, so the answer arrives at run
            // time on the enclosing body's render slot; the hint records which slot, and both
            // backends splice the instantiation's own hint in at the door. A parameter with no
            // slot (a generic method's, a nested `fn`'s own, an enclosing type's) has no channel,
            // so it stays unhinted and its value renders as the erased word.
            Type::Param(p) => self.render_slot_of(p).map(RenderHint::Param),
            _ => None,
        }
    }

    /// Every hint one **concrete instantiation** answers a [`noeta_ast::RenderHint::Param`] with —
    /// what the render slot delivers, built once here from the resolved type.
    ///
    /// Three answers rather than one because the doors differ in two ways the type cannot: a JSON
    /// walk numbers an object's slots without its `#[Transient]` fields, and a display door's
    /// **outermost** position renders a declared type through its own `to_string`, which hinting
    /// would replace. The exemption is the same one [`Self::note_render_hint`] applies to a
    /// concretely-typed door, applied here because this is where the concrete type is known.
    pub(crate) fn type_arg_hints(&self, sigma: &Type) -> noeta_ext_abi::TypeArgHints {
        let order = self.render_hint(sigma, HintPurpose::Display, &mut Vec::new());
        let exempt =
            matches!(sigma, Type::Named(..)) && self.satisfies(sigma, BuiltinTrait::Display);
        noeta_ext_abi::TypeArgHints {
            display: if exempt { None } else { order.clone() },
            order,
            json: self.render_hint(sigma, HintPurpose::Json, &mut Vec::new()),
        }
    }

    /// The **render slots** a call of `key` must supply, as slot TEMPLATES over the callee's own
    /// type parameters — each type parameter in declaration order, then every composite of them its
    /// PARAMETER types name ([`render_composite_templates`]) — empty for everything that has none.
    /// A call substitutes its instantiation into each template exactly as it does into a forwarding
    /// one.
    ///
    /// The parameters and not the return type, because a slot is only ever spent on a value the
    /// body *holds*, and what a body is given is its parameters. A return type describes a value it
    /// has yet to produce, so a slot for one would be interned at every call of the extremely
    /// ordinary `fn load<T>(text: string): Result<T, JsonError>` and read by nothing.
    ///
    /// The composites are what let a generic body hand a call an instantiation built out of its
    /// OWN parameters. `fn outer<T>(xs: List<T>)` calling `wrap(xs)` instantiates `wrap` at
    /// `List<T>`, which names nothing `outer`'s bare-`T` slot can answer: the value has to come
    /// from `outer`'s callers, who know what `T` is, and it does — through `outer`'s own `List<T>`
    /// slot, passed on with [`noeta_ext_abi::HiddenArg::Forward`] like every other pass-through.
    ///
    /// Top-level `fn`s only, which is why the lookup is [`Symbols::functions`]: a method reaches
    /// four name-keyed entry points that carry no instantiation at all (a `dyn` receiver, either
    /// handle form, `invoke`) and bind positionally, so hidden slots on one would be filled with a
    /// value argument. The declaration side ([`Coloring::current_render`]) lays out the same list
    /// by calling this same function, which is what makes the two ends agree.
    pub(crate) fn render_slot_templates(&self, key: &str) -> Vec<Type> {
        let Some(g) = self
            .symbols
            .functions
            .get(key)
            .and_then(|sig| sig.generic.as_ref())
        else {
            return Vec::new();
        };
        let own: ParamSet = g.params.iter().map(|(p, _)| p.id).collect();
        let mut out: Vec<Type> = g
            .params
            .iter()
            .map(|(p, _)| Type::Param(p.clone()))
            .collect();
        for t in &g.raw_params {
            render_composite_templates(t, &own, &mut out);
        }
        out
    }

    /// How this call fills the callee's render slot for `template`: the instantiation's interned
    /// table entry, a pass-through of the caller's own matching slot when the instantiation still
    /// mentions the caller's parameters, or [`noeta_ext_abi::HiddenArg::Erased`].
    ///
    /// Erasing is never a diagnostic, and that is the whole difference between this slot and a
    /// forwarding one. A forwarding slot feeds a decode recipe or a runtime name, where an absent
    /// instantiation means the call cannot be compiled; signedness only refines an answer that
    /// already exists, so a call that cannot name its instantiation — an inference hole, a `dyn`, a
    /// composite no slot of this body carries — falls back to reading the erased word, exactly as a
    /// `dyn` does.
    pub(crate) fn render_hidden_arg(
        &mut self,
        template: &Type,
        subst: &Subst,
        span: Span,
        callee: &str,
        slot: u32,
    ) -> noeta_ext_abi::HiddenArg {
        // Every parameter the template names has to be pinned to a real type. One the call left
        // unbound — or pinned only to `dyn`/an inference hole — names no instantiation at all.
        if all_params_mentioned(template)
            .iter()
            .any(|p| subst.get(&p.id).is_none_or(Type::defers_to_runtime))
        {
            return noeta_ext_abi::HiddenArg::Erased;
        }
        let sigma = apply_subst(template, subst);
        if self.mentions_in_scope_param(&sigma) {
            // The instantiation still mentions the CALLER's own parameters (the bare `T` of
            // `wrap(v)` inside `fn outer<T>(v: T)`, or the composite `List<T>` of `wrap(xs)` inside
            // `fn outer<T>(xs: List<T>)`), so it can only be answered by whatever pinned those —
            // this body's own matching slot, passed through, or composed out of the slots that do
            // pin its leaves when no single slot carries it whole.
            return match self.render_forward_slot(&sigma) {
                Some(j) => {
                    if let Some(feed) = self.forwarded_slot_feed(j) {
                        self.note_slot_feed(callee, slot, feed);
                    }
                    noeta_ext_abi::HiddenArg::Forward(j)
                }
                None => {
                    let arg = self.composed_hidden_arg(&sigma);
                    if let noeta_ext_abi::HiddenArg::Compose(id) = arg {
                        self.note_slot_feed(callee, slot, SlotFeed::Composed(id));
                    }
                    arg
                }
            };
        }
        if !self.fully_concrete(&sigma) {
            return noeta_ext_abi::HiddenArg::Erased;
        }
        // Interned through the one interner, with the slot's own template and no recipe demand: a
        // render slot consumes the entry's hints and nothing else.
        let slot = crate::forwarding::ForwardSlot {
            template: template.clone(),
            needs_recipe: false,
        };
        self.intern_type_arg(&sigma, &slot, "", span)
    }

    /// The hidden-slot ordinal of the body being checked whose per-instantiation entry **is**
    /// `sigma`, or `None` where nothing here delivers it (which erases the callee's slot).
    ///
    /// Both halves of the layout are searched, because both carry the same thing: a slot's runtime
    /// value is an index into the one type-argument table, and every entry in it holds that
    /// instantiation's [`noeta_ext_abi::TypeArgHints`] whether the slot was registered for a decode
    /// recipe or for a render hint. So a body that already forwards `List<T>` for a
    /// `json.try_parse::<List<T>>` answers a render slot for `List<T>` off the same ordinal instead
    /// of erasing beside it.
    pub(crate) fn render_forward_slot(&self, sigma: &Type) -> Option<u32> {
        let base = self.coloring.current_forwarding.len() as u32;
        if let Some(i) = self
            .coloring
            .current_render
            .iter()
            .position(|t| t.as_ref() == Some(sigma))
        {
            return Some(base + i as u32);
        }
        self.coloring
            .current_forwarding
            .iter()
            .position(|t| t == sigma)
            .map(|i| i as u32)
    }

    /// Record that this call can put `feed` into the callee's render slot `slot` — the only two
    /// answers the composition enumeration needs.
    ///
    /// A composition's cases enumerate what its **leaf slots** can hold, and a leaf slot holds one
    /// of the program's own interned instantiations unless a *composed* value reaches it. Composed
    /// values reach only render slots, and only through these two edges: a call that composes one
    /// directly, and a call that passes an enclosing render slot through. (A forwarding slot never
    /// carries one — it is filled by an interning site or by another forwarding slot — so a
    /// receiver's tag never does either, and neither does a leaf read off one.) Without the edges
    /// the enumeration would have to treat every composed entry as reachable from every leaf, and
    /// each round would intern a deeper composite no program can produce.
    fn note_slot_feed(&mut self, callee: &str, slot: u32, feed: SlotFeed) {
        self.slot_feeds
            .entry((callee.to_string(), slot))
            .or_default()
            .push(feed);
    }

    /// The pass-through edge for `j`, or `None` where that slot can carry no composed value — which
    /// is every slot outside this body's own render section, and every body with no slot key at all.
    /// A pass-through of one teaches the enumeration nothing.
    fn forwarded_slot_feed(&self, j: u32) -> Option<SlotFeed> {
        let key = self.coloring.current_slot_key.clone()?;
        self.is_render_slot(j).then_some(SlotFeed::Forward(key, j))
    }

    /// Whether slot ordinal `j` of the body being checked is one of its **own render slots** — the
    /// middle section of the layout, and the only one a composed value can reach.
    fn is_render_slot(&self, j: u32) -> bool {
        let base = self.coloring.current_forwarding.len() as u32;
        j >= base && j < base + self.coloring.current_render.len() as u32
    }

    /// How this call fills a render slot naming an instantiation **this body built** out of its own
    /// type parameters — `wrap([v])` inside `fn built<T>(v: T)`, which instantiates `wrap` at
    /// `List<T>`.
    ///
    /// [`Self::render_forward_slot`] has already failed, and it had to: nothing in `built`'s
    /// parameter types names `List<T>`, so no slot of `built` carries it and no caller of `built`
    /// could have interned a type the body invents. The slot layout is declaration-derived — it has
    /// to be identical at the declaration and at every call site — so an instantiation a body
    /// *constructs* is out of its reach by construction.
    ///
    /// What the body does hold is the composite's **leaves**, each on a slot its callers filled, and
    /// the shape around them is static: `List<u64>`'s hints are `Elements` of whatever `T`'s turned
    /// out to be. So the answer is arithmetic on the slots this body already has, and
    /// [`noeta_ext_abi::HintComposition`] is that arithmetic, precomputed per combination of leaf
    /// values once the table is complete ([`Self::finish_hint_compositions`]).
    ///
    /// [`noeta_ext_abi::HiddenArg::Erased`] where the composite renders nothing under any door —
    /// `List<T>` at `T = int` composes to no hint, and so does every composite over a parameter with
    /// no slot at all. Erasing is always available here, and never a diagnostic, for the same reason
    /// it is on the two arms beside it.
    fn composed_hidden_arg(&mut self, sigma: &Type) -> noeta_ext_abi::HiddenArg {
        let template = self.type_arg_hints(sigma);
        if template.is_empty() {
            return noeta_ext_abi::HiddenArg::Erased;
        }
        let mut leaves: Vec<u32> = [&template.display, &template.order, &template.json]
            .into_iter()
            .flatten()
            .flat_map(noeta_ast::RenderHint::param_slots)
            .collect();
        leaves.sort_unstable();
        leaves.dedup();
        // Which of this body's own slots each leaf is, so the enumeration can ask what that slot can
        // hold. `None` where the leaf is not a render slot of this body — a forwarding slot or a
        // read of the receiver's tag, neither of which a composed value ever reaches.
        let owner = self.coloring.current_slot_key.clone();
        let leaf_slots: Vec<Option<(String, u32)>> = leaves
            .iter()
            .map(|j| match (&owner, self.is_render_slot(*j)) {
                (Some(key), true) => Some((key.clone(), *j)),
                _ => None,
            })
            .collect();
        let draft = CompositionDraft {
            head: sigma.head_name(),
            template,
            leaves,
            leaf_slots,
        };
        let id = match self.hint_compositions.iter().position(|d| *d == draft) {
            Some(i) => i,
            None => {
                self.hint_compositions.push(draft);
                self.hint_compositions.len() - 1
            }
        };
        noeta_ext_abi::HiddenArg::Compose(id as u32)
    }

    /// The render-slot ordinal carrying `param`'s per-instantiation hints in the body being
    /// checked, or `None` where nothing delivers it.
    ///
    /// A body's slots are one list in three sections — the forwarding slots, the `fn`'s own render
    /// slots ([`crate::Coloring::current_render`]), then the ones read off the receiver
    /// ([`crate::Coloring::self_type_params`]) — and each section's base is the length of
    /// everything before it. Appending rather than interleaving is what leaves every ordinal the
    /// forwarding pre-pass minted exactly where it was.
    ///
    /// The two render sections differ only in **where the slot's value comes from**, which is
    /// lowering's business: the first two are the hidden `$ty` locals a call fills, the third is a
    /// read of the receiver's reflected tag. The hint records one ordinal either way, and one
    /// splice resolves it.
    pub(crate) fn render_slot_of(&self, param: &ParamRef) -> Option<u32> {
        let base = self.coloring.current_forwarding.len() as u32;
        if let Some(i) = self
            .coloring
            .current_render
            .iter()
            .position(|t| matches!(t, Some(Type::Param(p)) if p == param))
        {
            return Some(base + i as u32);
        }
        let self_base = base + self.coloring.current_render.len() as u32;
        self.coloring
            .self_type_params
            .iter()
            .position(|p| p.as_ref() == Some(param))
            .map(|i| self_base + i as u32)
    }

    /// Intern the render hints of every **type argument** of a generic construction, so a door
    /// inside one of that type's instance methods can find them again from the receiver's tag.
    ///
    /// The receiver channel resolves a slot by matching the tag's argument against the
    /// type-argument table's reflection projection ([`noeta_ast::reflect::TypeRepr::render_slot_arg`]),
    /// and a table only holds what some site put there. A generic `fn`'s call site interns its
    /// instantiation because it *fills* a slot; a construction fills none, so this is where the
    /// same instantiation is recorded for the method bodies to read.
    ///
    /// Only an argument that actually renders something is interned — which is nearly none of them
    /// — so a program with no `u64` under a generic type carries no extra entry. Nested arguments
    /// are interned too (`Holder<Holder<u64>>` records the inner instantiation as well), because
    /// the inner type's own methods read their own receiver's tag.
    pub(crate) fn note_self_render_args(&mut self, ty: &Type, span: Span) {
        let Type::Named(_, args) = ty else { return };
        for arg in args.clone() {
            self.intern_render_hints(&arg, span);
        }
    }

    /// Intern one instantiation into the type-argument table **for its hints alone** — nothing
    /// else about the entry is read, so it carries no recipe demand.
    ///
    /// Skipped where the instantiation renders nothing, which is nearly every one: the table (and
    /// the scan a receiver-read slot runs over it) stays empty for a program with no `u64` under a
    /// generic type. Its own arguments follow through [`Self::intern_type_arg`], which recurses.
    pub(crate) fn intern_render_hints(&mut self, sigma: &Type, span: Span) {
        if !self.fully_concrete(sigma) || self.type_arg_hints(sigma).is_empty() {
            return;
        }
        let slot = crate::forwarding::ForwardSlot {
            template: sigma.clone(),
            needs_recipe: false,
        };
        self.intern_type_arg(sigma, &slot, "", span);
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
            // The tag this stamps is the only channel an instance method of a generic type has for
            // its instantiation, so the arguments it names are interned here — see
            // [`Self::note_self_render_args`].
            self.note_self_render_args(&ty, span);
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

/// A render-slot composition while the check is still running: what
/// [`Checker::composed_hidden_arg`] recorded at a call, before
/// [`Checker::finish_hint_compositions`] can enumerate its cases.
///
/// The template is the built instantiation's own hints over the enclosing body's slots — a
/// [`noeta_ast::RenderHint::Param`] at each leaf, the composite's shape around them — so composing
/// one case is substituting the leaves' hints into it. `head` is the instantiation's qualified head
/// name, which is what the entry it interns is called.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CompositionDraft {
    /// The built instantiation's qualified head name (`List`, `Map`, `app.storage.Box`).
    pub(crate) head: String,
    /// Its hints over the enclosing body's slots, with a `Param` at every leaf.
    pub(crate) template: noeta_ext_abi::TypeArgHints,
    /// The slot ordinals those `Param`s name, ascending and deduplicated.
    pub(crate) leaves: Vec<u32>,
    /// Which of the enclosing body's own render slots each leaf is, positionally — `(the body's
    /// slot key, the ordinal)`. `None` for a leaf that is not one, which is a leaf no composed value
    /// can reach; see [`Checker::note_slot_feed`].
    pub(crate) leaf_slots: Vec<Option<(String, u32)>>,
}

/// What one call can put into a callee's render slot, as far as the composition enumeration cares.
/// Recorded by [`Checker::note_slot_feed`], which is where the two forms are explained.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum SlotFeed {
    /// This call composes the value: the slot can hold whatever composition `id` yields.
    Composed(u32),
    /// This call passes the CALLER's own render slot through — its slot key and the ordinal — so the
    /// callee's slot can hold whatever that one can.
    Forward(String, u32),
}

/// Every call's answer to "what can reach this render slot", keyed `(the callee's slot key, the slot
/// ordinal)`. See [`Checker::note_slot_feed`].
pub(crate) type SlotFeeds = HashMap<(String, u32), Vec<SlotFeed>>;

/// Fill every [`Checker::composed_hidden_arg`] composition's case table into `sites`, once its
/// type-argument table is complete — the last thing done to that table.
///
/// A composition's answer is a pure function of what its leaf slots hold, and a leaf slot holds a
/// table index. The set of *distinct answers* is therefore finite and small: only an entry carrying
/// hints can change one, and a program's hint-carrying instantiations number in the handful (every
/// other entry, and every slot the call site could not name, contributes the same nothing
/// [`noeta_ext_abi::NO_TYPE_ARG`] does). So each combination is enumerated here and its composed
/// hints interned, and the runtime lookup is a scan of the result.
///
/// Iterated to a fixpoint, because a composed entry is itself a candidate leaf value for the next
/// composition: `a<T>` handing `[v]` to `b<U>`, which hands `[u]` on again, needs `List<List<u64>>`
/// to exist before `b`'s composition can name it. Bounded, because polymorphic recursion
/// (`fn f<T>(v: T) { f([v]) }`) builds a deeper type every round and no static table can enumerate
/// that; a combination the bound does not reach is simply absent, which is the erased word.
///
/// **The table does not grow for a program that renders nothing.** A composed entry is interned only
/// where its hints are non-empty, and hints are non-empty only under an unsigned 64-bit integer — so
/// a composition over `List<T>` in a program that never instantiates it at `u64` enumerates its
/// combinations, finds every one composes to nothing, and appends no entry.
///
/// A function over the produced [`Sites`] rather than over the checker, because a session snapshots
/// its bundle once per entry while the checker lives on: the composed entries belong to the bundle
/// that is about to be lowered, and a session install merges each bundle's table by content anyway.
pub(crate) fn finish_hint_compositions(
    drafts: &[CompositionDraft],
    feeds: &SlotFeeds,
    sites: &mut crate::Sites,
) {
    /// How many rounds of composed-entry discovery to run. Each round can deepen a type by one
    /// composition, and a chain of generic frames that deep is already past what any real program
    /// forwards; polymorphic recursion never terminates and is cut here.
    const ROUNDS: usize = 4;
    /// The largest combination space one composition is enumerated over. A composition reads one
    /// leaf almost always and two at most in practice; this only stops a pathological arity from
    /// making the enumeration exponential, and a composition past it erases like any other the
    /// checker cannot name.
    const MAX_COMBINATIONS: usize = 256;

    if drafts.is_empty() {
        return;
    }
    // The program's OWN interned instantiations — what any leaf can hold before a composition has
    // produced anything. Snapshotted before the first round, because everything appended below is a
    // composed entry and reaches a leaf only along a recorded edge.
    let interned: Vec<i64> = std::iter::once(noeta_ext_abi::NO_TYPE_ARG)
        .chain(
            sites
                .type_arg_hints
                .iter()
                .enumerate()
                .filter(|(_, h)| !h.is_empty())
                .map(|(i, _)| i as i64),
        )
        .collect();
    let reaching = compositions_reaching_each_leaf(drafts, feeds);
    let mut cases: Vec<Vec<noeta_ext_abi::HintCase>> = vec![Vec::new(); drafts.len()];
    for _ in 0..ROUNDS {
        let mut grew = false;
        for (id, draft) in drafts.iter().enumerate() {
            // Each leaf ranges over the program's own instantiations, plus the answers of exactly
            // the compositions whose output can arrive on that leaf's slot. A composition whose
            // leaves nothing composed reaches — which is nearly every one — settles in one round.
            let per_leaf: Vec<Vec<i64>> = (0..draft.leaves.len())
                .map(|at| {
                    let mut vs = interned.clone();
                    for from in &reaching[id][at] {
                        vs.extend(cases[*from as usize].iter().map(|c| c.composed));
                    }
                    vs.sort_unstable();
                    vs.dedup();
                    vs
                })
                .collect();
            if per_leaf
                .iter()
                .try_fold(1usize, |n, vs| n.checked_mul(vs.len()))
                .is_none_or(|n| n > MAX_COMBINATIONS)
            {
                continue;
            }
            for key in combinations(&per_leaf) {
                if cases[id]
                    .iter()
                    .any(|c| c.leaves.as_ref() == key.as_slice())
                {
                    continue;
                }
                let composed = compose_case(draft, &key, &sites.type_arg_hints);
                if composed.is_empty() {
                    continue;
                }
                let idx = crate::intern_type_arg_entry(
                    &mut sites.type_arg_table,
                    &mut sites.type_arg_reprs,
                    &mut sites.type_arg_hints,
                    noeta_ext_abi::TypeArgInfo {
                        name: draft.head.clone(),
                        // A composed entry answers the hints projection and nothing else: it is
                        // reachable only from a render slot, which is interned with no recipe demand
                        // and may legitimately be `NO_TYPE_ARG`. Giving one a recipe would let a
                        // recipe-consuming door resolve off it, turning a check-time diagnostic into
                        // a runtime abort.
                        recipe: None,
                    },
                    None,
                    composed,
                );
                cases[id].push(noeta_ext_abi::HintCase {
                    leaves: key.into_boxed_slice(),
                    composed: i64::from(idx.get()),
                });
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    sites.type_arg_compositions = drafts
        .iter()
        .zip(cases)
        .map(|(draft, cases)| noeta_ext_abi::HintComposition {
            leaves: draft.leaves.clone().into_boxed_slice(),
            cases: cases.into_boxed_slice(),
        })
        .collect();
}

/// The composed hints when `draft`'s leaf slots hold exactly `key` — the substitution
/// [`Checker::finish_hint_compositions`] interns one entry per distinct answer of.
///
/// Each door substitutes the answer *it* would read, which is what keeps the three apart: a display
/// door's outermost position exempts a type that prints through its own `to_string`, and every
/// nested position (which is where a composition's leaves always sit) reads the ordering hint
/// instead. A leaf holding [`noeta_ext_abi::NO_TYPE_ARG`], or an entry with no hint of its own,
/// substitutes nothing — and the sparseness [`noeta_ast::RenderHint::resolve`] re-establishes
/// collapses the branch above it, so a composite whose only unsigned position was that leaf composes
/// to no hint at all rather than to an empty aggregate.
fn compose_case(
    draft: &CompositionDraft,
    key: &[i64],
    table: &[noeta_ext_abi::TypeArgHints],
) -> noeta_ext_abi::TypeArgHints {
    let entry = |n: u32| -> Option<&noeta_ext_abi::TypeArgHints> {
        let at = draft.leaves.iter().position(|l| *l == n)?;
        let v = *key.get(at)?;
        (v >= 0).then(|| table.get(v as usize)).flatten()
    };
    let splice =
        |hint: &Option<noeta_ast::RenderHint>,
         pick: &dyn Fn(&noeta_ext_abi::TypeArgHints, bool) -> Option<noeta_ast::RenderHint>|
         -> Option<noeta_ast::RenderHint> {
            hint.as_ref()?
                .resolve(&|n, outermost| entry(n).and_then(|e| pick(e, outermost)))
                .map(std::borrow::Cow::into_owned)
        };
    noeta_ext_abi::TypeArgHints {
        display: splice(&draft.template.display, &|e, outermost| {
            e.at_display(outermost)
        }),
        order: splice(&draft.template.order, &|e, _| e.order.clone()),
        json: splice(&draft.template.json, &|e, _| e.json.clone()),
    }
}

/// Every tuple picking one value from each of `per_leaf`, in odometer order — the combinations one
/// composition is enumerated over. The empty tuple when `per_leaf` is empty, which is the
/// composition whose hints mention no parameter at all (`Map<u64, T>` at a `T` with no slot) and
/// therefore has exactly one answer.
fn combinations(per_leaf: &[Vec<i64>]) -> Vec<Vec<i64>> {
    let mut out: Vec<Vec<i64>> = vec![Vec::new()];
    for values in per_leaf {
        out = out
            .into_iter()
            .flat_map(|prefix| {
                values.iter().map(move |v| {
                    let mut next = prefix.clone();
                    next.push(*v);
                    next
                })
            })
            .collect();
    }
    out
}

/// For each composition and each of its leaves, the compositions whose output can arrive there —
/// `reaching[id][leaf]`.
///
/// A leaf is a slot of the body the composition sits in, and a slot's value comes from that body's
/// callers. [`Checker::note_slot_feed`] recorded the two edges a *composed* value can travel: a call
/// that composes one into the slot, and a call that passes an enclosing render slot through. This
/// closes the pass-through edges transitively, so `a<T>` handing `b<U>` a `List<T>` it built — and
/// `b` building `List<U>` on top of that — knows it must enumerate `b`'s leaf over `a`'s answers as
/// well as over the program's own instantiations.
///
/// Without it every composed entry would be a candidate leaf for every composition, and each round
/// would intern a `List<List<…>>` one level deeper than the last — types no program can produce,
/// interned into the table that ships.
fn compositions_reaching_each_leaf(
    drafts: &[CompositionDraft],
    feeds: &SlotFeeds,
) -> Vec<Vec<Vec<u32>>> {
    // The compositions that can arrive on one slot, closed over pass-throughs. Depth-bounded rather
    // than visited-set-bounded because a pass-through cycle is a body forwarding its own slot to
    // itself, and the answer there is the fixpoint the outer round loop already takes.
    fn at(slot: &(String, u32), feeds: &SlotFeeds, depth: usize, out: &mut Vec<u32>) {
        if depth == 0 {
            return;
        }
        for feed in feeds.get(slot).into_iter().flatten() {
            match feed {
                SlotFeed::Composed(id) => out.push(*id),
                // The caller passes its own render slot through, so whatever reaches THAT slot
                // reaches this one.
                SlotFeed::Forward(key, j) => at(&(key.clone(), *j), feeds, depth - 1, out),
            }
        }
    }
    /// How many pass-through frames to follow. A chain of generic frames deeper than this forwards
    /// its own slot through more hands than any real program does, and the cut only costs the
    /// erased word.
    const DEPTH: usize = 16;

    drafts
        .iter()
        .map(|draft| {
            draft
                .leaf_slots
                .iter()
                .map(|slot| {
                    let mut out = Vec::new();
                    if let Some(slot) = slot {
                        at(slot, feeds, DEPTH, &mut out);
                    }
                    out.sort_unstable();
                    out.dedup();
                    out
                })
                .collect()
        })
        .collect()
}

/// Append every **composite** render-slot template `ty` contributes: each of its sub-terms that
/// mentions one of `own`'s type parameters and is a shape a render hint descends through, in
/// first-appearance order and deduplicated against what `out` already holds.
///
/// Why the callee's own parameter types are the source. A render slot answers "what did this
/// instantiation turn out to be", and the only way a body can hand an onward call an instantiation
/// that mentions its OWN parameters is out of a value it holds — whose type is the one its
/// declaration gave it. So `fn outer<T>(xs: List<T>)` demands `List<T>` and gets it, while `outer`'s
/// callers, which know `T`, are the ones that can intern `List<u64>` for it. Deriving the layout
/// from the declaration alone is not an economy but the requirement: the slot layout has to be
/// identical at the declaration and at every call site, and an inference-derived one would have to
/// be known before the body that determines it has been checked.
///
/// Sub-terms and not just the top level, so a body that passes a *part* of what it was given
/// (`fn f<T>(xss: List<List<T>>)` handing `xss[0]` on) is answered too. A [`Type::Fn`] is walked
/// but never registered: no render hint descends into a function value, so a slot for one could
/// only ever resolve to no hint.
fn render_composite_templates(ty: &Type, own: &ParamSet, out: &mut Vec<Type>) {
    if !mentions_param(ty, own) {
        return;
    }
    let hintable = matches!(
        ty,
        Type::List(_)
            | Type::Set(_)
            | Type::Option(_)
            | Type::Map(..)
            | Type::Result(..)
            | Type::Tuple(_)
            | Type::Named(..)
    );
    if hintable && !out.contains(ty) {
        out.push(ty.clone());
    }
    match ty {
        Type::List(t) | Type::Set(t) | Type::Option(t) => render_composite_templates(t, own, out),
        Type::Map(k, v) | Type::Result(k, v) => {
            render_composite_templates(k, own, out);
            render_composite_templates(v, own, out);
        }
        Type::Named(_, args) | Type::Tuple(args) | Type::Union(args) => {
            for a in args {
                render_composite_templates(a, own, out);
            }
        }
        Type::Fn { params, ret } => {
            for p in params {
                render_composite_templates(p, own, out);
            }
            render_composite_templates(ret, own, out);
        }
        _ => {}
    }
}
