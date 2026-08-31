//! Span normalization — the exhaustive walk that makes structural AST equality usable as a safety
//! property.
//!
//! `noeta fmt` promises that its output re-parses to **the same program**. The AST derives
//! `PartialEq`, which is exactly that question — except that it also compares [`Span`]s, and
//! formatting shifts every byte offset by construction. So the comparison needs one preparation
//! step: set every span to a fixed value on both sides, then ask `a == b`.
//!
//! That is what this module is, and the shape is chosen for one property: **a field added to the
//! AST tomorrow cannot be missed here.** Every `impl` below destructures its type *by name*, with
//! no `..` anywhere, so a new field is a compile error at the one site that must consider it. The
//! decision of what to do with that field is then made by the type system rather than by the
//! author: [`Normalize`] is implemented for `Span` (zero it), for the containers (walk into them),
//! and as a no-op for the leaves that cannot hold one. Nobody has to judge whether a `String` needs
//! visiting.
//!
//! ## Why this is a safety property and a rendering is not
//!
//! The alternative — compare a printed form of both programs — is only equivalent to structural
//! equality if the printer is *injective*, and nothing enforces that. Two formatter defects reached
//! `main` through exactly that gap: a `..` in the [`Pretty`](crate::Pretty) arm for
//! [`Stmt::TierBlock`] hid `attached`, so a rule that collapsed `@test { fn t() {…} }` into
//! `@test fn t()` compared equal; and a payload-less `Ok()` pattern rendered identically to the
//! catch-all binding `Ok`, so a rule that dropped exactly those parens compared equal too. Both
//! rewrote the user's file.
//!
//! The failure modes are not symmetric, and that is the whole argument for this module. A printer
//! that forgets a field makes the gate **blinder** — it approves a rewrite it should have refused.
//! A walk that forgets a *span* makes the gate **stricter** — the two programs then differ, the gate
//! trips, and fmt declines and leaves the file untouched. This module can be incomplete. It cannot
//! be wrong.

use crate::{
    Arg, AssocTypeDecl, AttrValue, Attribute, ClassDecl, ClosureBody, Decorators, DeriveSpec,
    EnumDecl, ExpansionMark, Expr, FieldDecl, FieldInit, FnDecl, ForPattern, ForeignDirective,
    ImplBlock, ImplDecl, MatchArm, MemberBinding, MethodDirective, Name, ObjectLit,
    PackedDirective, PackedLayout, Param, Pattern, Program, ReflectKind, ReflectOperand, RoleTag,
    Stmt, StrPart, StructDecl, TierDecl, TraitBound, TraitDecl, TraitMethod, TypeOperand,
    TypeParam, TypeRef, UnaryOp, UseName, VariantDecl,
};
use noeta_span::{SourceId, Span};

/// What a normalized span is set to. Any fixed value works; zero is the one that reads as "erased"
/// in a `Debug` dump when a comparison does fail.
const ZERO: Span = Span {
    start: 0,
    end: 0,
    source: SourceId(0),
};

/// How much to erase, beyond spans.
#[derive(Debug, Clone, Copy, Default)]
pub struct Normalization {
    /// Also clear the **static text** of every [`Expr::TierExpr`] body.
    ///
    /// A tier-body formatter reflows foreign text (SQL, HTML, a shader), so its statics legitimately
    /// change across a format. `noeta fmt` cannot prove that reflow value-preserving in a language
    /// it does not speak — only the body formatter's author can — so a caller that ran one compares
    /// with this set. Everything else, the `${…}` holes between the statics included, is still
    /// compared exactly.
    pub clear_tier_statics: bool,
}

/// Set every span in `program` to [`ZERO`], so two programs that differ only in byte offsets compare
/// equal under the derived `PartialEq`.
pub fn zero_spans(program: &mut Program) {
    program.normalize(&Normalization::default());
}

/// [`zero_spans`], plus whatever else `how` asks for.
pub fn normalize(program: &mut Program, how: &Normalization) {
    program.normalize(how);
}

/// An AST node that can have its spans erased in place.
///
/// Private on purpose: the two free functions above are the whole surface. Making the trait public
/// would invite an `impl` outside this module, and the property that makes this sound — every
/// implementation destructures exhaustively — is only checkable while they all live here.
trait Normalize {
    fn normalize(&mut self, how: &Normalization);
}

// --- the two ends of the recursion -------------------------------------------------------------

impl Normalize for Span {
    fn normalize(&mut self, _how: &Normalization) {
        *self = ZERO;
    }
}

/// The leaves: values that cannot contain a span, so visiting them is a no-op. Listed explicitly
/// rather than covered by a blanket impl — a blanket `impl<T> Normalize for T` would silently
/// swallow a *new node type* that does hold spans, which is the one mistake this module is built to
/// make impossible.
macro_rules! leaf {
    ($($ty:ty),* $(,)?) => {$(
        impl Normalize for $ty {
            fn normalize(&mut self, _how: &Normalization) {}
        }
    )*};
}

leaf!(
    bool,
    u8,
    u32,
    u64,
    i64,
    f32,
    f64,
    String,
    Name,
    SourceId,
    PackedLayout,
    ReflectKind,
    UnaryOp,
    crate::BinaryOp,
);

// --- containers --------------------------------------------------------------------------------

impl<T: Normalize> Normalize for Vec<T> {
    fn normalize(&mut self, how: &Normalization) {
        for item in self {
            item.normalize(how);
        }
    }
}

impl<T: Normalize> Normalize for Option<T> {
    fn normalize(&mut self, how: &Normalization) {
        if let Some(inner) = self {
            inner.normalize(how);
        }
    }
}

impl<T: Normalize> Normalize for Box<T> {
    fn normalize(&mut self, how: &Normalization) {
        (**self).normalize(how);
    }
}

impl<A: Normalize, B: Normalize> Normalize for (A, B) {
    fn normalize(&mut self, how: &Normalization) {
        self.0.normalize(how);
        self.1.normalize(how);
    }
}

// --- the program -------------------------------------------------------------------------------

impl Normalize for Program {
    fn normalize(&mut self, how: &Normalization) {
        let Program { stmts, span } = self;
        stmts.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for Stmt {
    fn normalize(&mut self, how: &Normalization) {
        match self {
            Stmt::Echo { value, span } => {
                value.normalize(how);
                span.normalize(how);
            }
            Stmt::Binding {
                mut_decl,
                name,
                name_span,
                ty,
                value,
                span,
            } => {
                mut_decl.normalize(how);
                name.normalize(how);
                name_span.normalize(how);
                ty.normalize(how);
                value.normalize(how);
                span.normalize(how);
            }
            Stmt::Destructure {
                mut_decl,
                targets,
                value,
                span,
            } => {
                mut_decl.normalize(how);
                targets.normalize(how);
                value.normalize(how);
                span.normalize(how);
            }
            Stmt::Fn(decl) => decl.normalize(how),
            Stmt::Enum(decl) => decl.normalize(how),
            Stmt::Struct(decl) => decl.normalize(how),
            Stmt::Class(decl) => decl.normalize(how),
            Stmt::Impl(decl) => decl.normalize(how),
            Stmt::Trait(decl) => decl.normalize(how),
            Stmt::Namespace { path, span } => {
                path.normalize(how);
                span.normalize(how);
            }
            Stmt::Use { path, names, span } => {
                path.normalize(how);
                names.normalize(how);
                span.normalize(how);
            }
            Stmt::Return { value, span } => {
                value.normalize(how);
                span.normalize(how);
            }
            Stmt::Yield { value, span } => {
                value.normalize(how);
                span.normalize(how);
            }
            Stmt::Concurrent { body, span } => {
                body.normalize(how);
                span.normalize(how);
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
                span,
            } => {
                cond.normalize(how);
                then_body.normalize(how);
                else_body.normalize(how);
                span.normalize(how);
            }
            Stmt::For {
                pattern,
                iterable,
                body,
                span,
            } => {
                pattern.normalize(how);
                iterable.normalize(how);
                body.normalize(how);
                span.normalize(how);
            }
            Stmt::While { cond, body, span } => {
                cond.normalize(how);
                body.normalize(how);
                span.normalize(how);
            }
            Stmt::Break { span } => span.normalize(how),
            Stmt::Continue { span } => span.normalize(how),
            Stmt::Expr { expr, span } => {
                expr.normalize(how);
                span.normalize(how);
            }
            Stmt::TierBlock {
                tier,
                tier_span,
                args,
                items,
                doc_text,
                attached,
                span,
            } => {
                tier.normalize(how);
                tier_span.normalize(how);
                args.normalize(how);
                items.normalize(how);
                doc_text.normalize(how);
                attached.normalize(how);
                span.normalize(how);
            }
        }
    }
}

// --- declarations ------------------------------------------------------------------------------

impl Normalize for StructDecl {
    fn normalize(&mut self, how: &Normalization) {
        let StructDecl {
            name,
            name_span,
            is_public,
            type_params,
            fields,
            methods,
            impls,
            decorators,
            span,
        } = self;
        name.normalize(how);
        name_span.normalize(how);
        is_public.normalize(how);
        type_params.normalize(how);
        fields.normalize(how);
        methods.normalize(how);
        impls.normalize(how);
        decorators.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for ClassDecl {
    fn normalize(&mut self, how: &Normalization) {
        let ClassDecl {
            name,
            name_span,
            is_public,
            type_params,
            fields,
            methods,
            impls,
            decorators,
            destructor,
            span,
        } = self;
        name.normalize(how);
        name_span.normalize(how);
        is_public.normalize(how);
        type_params.normalize(how);
        fields.normalize(how);
        methods.normalize(how);
        impls.normalize(how);
        decorators.normalize(how);
        destructor.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for EnumDecl {
    fn normalize(&mut self, how: &Normalization) {
        let EnumDecl {
            name,
            name_span,
            is_public,
            type_params,
            backing,
            variants,
            methods,
            impls,
            decorators,
            span,
        } = self;
        name.normalize(how);
        name_span.normalize(how);
        is_public.normalize(how);
        type_params.normalize(how);
        backing.normalize(how);
        variants.normalize(how);
        methods.normalize(how);
        impls.normalize(how);
        decorators.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for VariantDecl {
    fn normalize(&mut self, how: &Normalization) {
        let VariantDecl {
            name,
            name_span,
            fields,
            backed_value,
            attrs,
            span,
        } = self;
        name.normalize(how);
        name_span.normalize(how);
        fields.normalize(how);
        backed_value.normalize(how);
        attrs.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for FieldDecl {
    fn normalize(&mut self, how: &Normalization) {
        let FieldDecl {
            name,
            name_span,
            mut_field,
            is_public,
            ty,
            default,
            attrs,
            span,
        } = self;
        name.normalize(how);
        name_span.normalize(how);
        mut_field.normalize(how);
        is_public.normalize(how);
        ty.normalize(how);
        default.normalize(how);
        attrs.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for TraitDecl {
    fn normalize(&mut self, how: &Normalization) {
        let TraitDecl {
            name,
            name_span,
            is_public,
            type_params,
            methods,
            assoc_types,
            decorators,
            span,
        } = self;
        name.normalize(how);
        name_span.normalize(how);
        is_public.normalize(how);
        type_params.normalize(how);
        methods.normalize(how);
        assoc_types.normalize(how);
        decorators.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for TraitMethod {
    fn normalize(&mut self, how: &Normalization) {
        let TraitMethod { sig, has_default } = self;
        sig.normalize(how);
        has_default.normalize(how);
    }
}

impl Normalize for AssocTypeDecl {
    fn normalize(&mut self, how: &Normalization) {
        let AssocTypeDecl {
            name,
            name_span,
            default,
            span,
        } = self;
        name.normalize(how);
        name_span.normalize(how);
        default.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for ImplDecl {
    fn normalize(&mut self, how: &Normalization) {
        let ImplDecl {
            trait_name,
            trait_span,
            trait_args,
            target,
            target_span,
            methods,
            assoc_bindings,
            span,
        } = self;
        trait_name.normalize(how);
        trait_span.normalize(how);
        trait_args.normalize(how);
        target.normalize(how);
        target_span.normalize(how);
        methods.normalize(how);
        assoc_bindings.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for ImplBlock {
    fn normalize(&mut self, how: &Normalization) {
        let ImplBlock {
            trait_name,
            trait_span,
            trait_args,
            methods,
            assoc_bindings,
            span,
        } = self;
        trait_name.normalize(how);
        trait_span.normalize(how);
        trait_args.normalize(how);
        methods.normalize(how);
        assoc_bindings.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for FnDecl {
    fn normalize(&mut self, how: &Normalization) {
        let FnDecl {
            name,
            name_span,
            is_public,
            type_params,
            params,
            ret,
            attrs,
            directives,
            is_dev_tier,
            is_async,
            is_static,
            tier,
            captures,
            body,
            span,
        } = self;
        name.normalize(how);
        name_span.normalize(how);
        is_public.normalize(how);
        type_params.normalize(how);
        params.normalize(how);
        ret.normalize(how);
        attrs.normalize(how);
        directives.normalize(how);
        is_dev_tier.normalize(how);
        is_async.normalize(how);
        is_static.normalize(how);
        tier.normalize(how);
        captures.normalize(how);
        body.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for TierDecl {
    fn normalize(&mut self, how: &Normalization) {
        let TierDecl {
            name,
            name_span,
            config,
            text,
            expr,
            span,
        } = self;
        name.normalize(how);
        name_span.normalize(how);
        config.normalize(how);
        text.normalize(how);
        expr.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for Param {
    fn normalize(&mut self, how: &Normalization) {
        let Param {
            attrs,
            name,
            name_span,
            ty,
            default,
            span,
            positional,
        } = self;
        attrs.normalize(how);
        name.normalize(how);
        name_span.normalize(how);
        ty.normalize(how);
        default.normalize(how);
        span.normalize(how);
        positional.normalize(how);
    }
}

impl Normalize for TypeParam {
    fn normalize(&mut self, how: &Normalization) {
        let TypeParam { name, bounds, span } = self;
        name.normalize(how);
        bounds.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for TraitBound {
    fn normalize(&mut self, how: &Normalization) {
        let TraitBound { name, args, span } = self;
        name.normalize(how);
        args.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for UseName {
    fn normalize(&mut self, how: &Normalization) {
        let UseName { name, span, alias } = self;
        name.normalize(how);
        span.normalize(how);
        alias.normalize(how);
    }
}

// --- decorators and attributes -----------------------------------------------------------------

impl Normalize for Decorators {
    fn normalize(&mut self, how: &Normalization) {
        let Decorators {
            derives,
            attrs,
            attribute,
            role,
            semantic,
            packed,
            validated,
            foreign,
            expansions,
        } = self;
        derives.normalize(how);
        attrs.normalize(how);
        attribute.normalize(how);
        role.normalize(how);
        semantic.normalize(how);
        packed.normalize(how);
        validated.normalize(how);
        foreign.normalize(how);
        expansions.normalize(how);
    }
}

impl Normalize for ExpansionMark {
    fn normalize(&mut self, how: &Normalization) {
        let ExpansionMark {
            directive,
            origin,
            source,
        } = self;
        directive.normalize(how);
        origin.normalize(how);
        // The generated source's id is the key a member's span is matched against, so zeroing it
        // would make every expansion on a declaration look like the same one. It is also not a
        // span: nothing a format shifts.
        let _ = source;
    }
}

impl Normalize for DeriveSpec {
    fn normalize(&mut self, how: &Normalization) {
        let DeriveSpec {
            name,
            args,
            bindings,
            via,
            span,
        } = self;
        name.normalize(how);
        args.normalize(how);
        bindings.normalize(how);
        via.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for MemberBinding {
    fn normalize(&mut self, how: &Normalization) {
        let MemberBinding {
            member,
            target,
            span,
        } = self;
        member.normalize(how);
        target.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for Attribute {
    fn normalize(&mut self, how: &Normalization) {
        let Attribute {
            name,
            name_span,
            args,
            span,
        } = self;
        name.normalize(how);
        name_span.normalize(how);
        args.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for ForeignDirective {
    fn normalize(&mut self, how: &Normalization) {
        let ForeignDirective {
            name,
            name_span,
            args,
            span,
        } = self;
        name.normalize(how);
        name_span.normalize(how);
        args.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for MethodDirective {
    fn normalize(&mut self, how: &Normalization) {
        let MethodDirective {
            name,
            name_span,
            args,
            doc_text,
            span,
        } = self;
        name.normalize(how);
        name_span.normalize(how);
        args.normalize(how);
        doc_text.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for PackedDirective {
    fn normalize(&mut self, how: &Normalization) {
        let PackedDirective { span, layout } = self;
        span.normalize(how);
        layout.normalize(how);
    }
}

impl Normalize for RoleTag {
    fn normalize(&mut self, how: &Normalization) {
        let RoleTag {
            enum_name,
            variant,
            span,
        } = self;
        enum_name.normalize(how);
        variant.normalize(how);
        span.normalize(how);
    }
}

impl<V: Normalize> Normalize for Arg<V> {
    fn normalize(&mut self, how: &Normalization) {
        let Arg { name, value, span } = self;
        name.normalize(how);
        value.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for AttrValue {
    fn normalize(&mut self, how: &Normalization) {
        match self {
            AttrValue::Str(value) => value.normalize(how),
            AttrValue::Int(value) => value.normalize(how),
            AttrValue::Float(value) => value.normalize(how),
            AttrValue::Bool(value) => value.normalize(how),
            AttrValue::List(items) => items.normalize(how),
            AttrValue::Set(items) => items.normalize(how),
            AttrValue::Map(entries) => entries.normalize(how),
            AttrValue::Enum {
                enum_name,
                variant,
                args,
            } => {
                enum_name.normalize(how);
                variant.normalize(how);
                args.normalize(how);
            }
            AttrValue::Struct { type_name, fields } => {
                type_name.normalize(how);
                fields.normalize(how);
            }
            AttrValue::TypeRef { name, args } => {
                name.normalize(how);
                args.normalize(how);
            }
        }
    }
}

// --- types -------------------------------------------------------------------------------------

impl Normalize for TypeRef {
    fn normalize(&mut self, how: &Normalization) {
        match self {
            TypeRef::Named { name, args, span } => {
                name.normalize(how);
                args.normalize(how);
                span.normalize(how);
            }
            TypeRef::DynTrait { trait_name, span } => {
                trait_name.normalize(how);
                span.normalize(how);
            }
            TypeRef::Optional { inner, span } => {
                inner.normalize(how);
                span.normalize(how);
            }
            TypeRef::Union { members, span } => {
                members.normalize(how);
                span.normalize(how);
            }
            TypeRef::Tuple { elements, span } => {
                elements.normalize(how);
                span.normalize(how);
            }
            TypeRef::Fn { params, ret, span } => {
                params.normalize(how);
                ret.normalize(how);
                span.normalize(how);
            }
            TypeRef::AssocProjection { name, span } => {
                name.normalize(how);
                span.normalize(how);
            }
        }
    }
}

impl Normalize for TypeOperand {
    fn normalize(&mut self, how: &Normalization) {
        match self {
            TypeOperand::Static(ty) => ty.normalize(how),
            TypeOperand::Dynamic(expr) => expr.normalize(how),
        }
    }
}

impl Normalize for ReflectOperand {
    fn normalize(&mut self, how: &Normalization) {
        match self {
            ReflectOperand::Nothing => {}
            ReflectOperand::Type(operand) => operand.normalize(how),
            ReflectOperand::Value(expr) => expr.normalize(how),
            ReflectOperand::StaticType(ty) => ty.normalize(how),
            ReflectOperand::TypeWith { ty, arg } => {
                ty.normalize(how);
                arg.normalize(how);
            }
            ReflectOperand::StaticTypeWith { ty, arg } => {
                ty.normalize(how);
                arg.normalize(how);
            }
            ReflectOperand::Dispatch { recv, name, args } => {
                recv.normalize(how);
                name.normalize(how);
                args.normalize(how);
            }
        }
    }
}

// --- patterns ----------------------------------------------------------------------------------

impl Normalize for Pattern {
    fn normalize(&mut self, how: &Normalization) {
        match self {
            Pattern::Wildcard { span } => span.normalize(how),
            Pattern::Binding { name, span } => {
                name.normalize(how);
                span.normalize(how);
            }
            Pattern::Int { value, span } => {
                value.normalize(how);
                span.normalize(how);
            }
            Pattern::Str { value, span } => {
                value.normalize(how);
                span.normalize(how);
            }
            Pattern::Bool { value, span } => {
                value.normalize(how);
                span.normalize(how);
            }
            Pattern::Variant {
                type_name,
                variant,
                bindings,
                span,
            } => {
                type_name.normalize(how);
                variant.normalize(how);
                bindings.normalize(how);
                span.normalize(how);
            }
            Pattern::IsType { ty, span } => {
                ty.normalize(how);
                span.normalize(how);
            }
            Pattern::Tuple { elements, span } => {
                elements.normalize(how);
                span.normalize(how);
            }
        }
    }
}

impl Normalize for ForPattern {
    fn normalize(&mut self, how: &Normalization) {
        match self {
            ForPattern::Single { name, name_span } => {
                name.normalize(how);
                name_span.normalize(how);
            }
            ForPattern::Tuple { names, span } => {
                names.normalize(how);
                span.normalize(how);
            }
        }
    }
}

// --- expressions -------------------------------------------------------------------------------

impl Normalize for Expr {
    fn normalize(&mut self, how: &Normalization) {
        match self {
            Expr::Str { value, span } => {
                value.normalize(how);
                span.normalize(how);
            }
            Expr::Int { value, span } => {
                value.normalize(how);
                span.normalize(how);
            }
            Expr::Float { value, span } => {
                value.normalize(how);
                span.normalize(how);
            }
            Expr::F32 { value, span } => {
                value.normalize(how);
                span.normalize(how);
            }
            Expr::F64 { value, span } => {
                value.normalize(how);
                span.normalize(how);
            }
            Expr::IntN {
                magnitude,
                signed,
                bits,
                span,
            } => {
                magnitude.normalize(how);
                signed.normalize(how);
                bits.normalize(how);
                span.normalize(how);
            }
            Expr::Bool { value, span } => {
                value.normalize(how);
                span.normalize(how);
            }
            Expr::Ident { name, span } => {
                name.normalize(how);
                span.normalize(how);
            }
            Expr::Unary { op, operand, span } => {
                op.normalize(how);
                operand.normalize(how);
                span.normalize(how);
            }
            Expr::Binary { op, lhs, rhs, span } => {
                op.normalize(how);
                lhs.normalize(how);
                rhs.normalize(how);
                span.normalize(how);
            }
            Expr::Call { callee, args, span } => {
                callee.normalize(how);
                args.normalize(how);
                span.normalize(how);
            }
            Expr::Closure {
                params,
                ret,
                body,
                span,
            } => {
                params.normalize(how);
                ret.normalize(how);
                body.normalize(how);
                span.normalize(how);
            }
            Expr::Pipeline { left, right, span } => {
                left.normalize(how);
                right.normalize(how);
                span.normalize(how);
            }
            Expr::List { items, span } => {
                items.normalize(how);
                span.normalize(how);
            }
            Expr::Tuple { items, span } => {
                items.normalize(how);
                span.normalize(how);
            }
            Expr::TupleIndex {
                receiver,
                index,
                span,
            } => {
                receiver.normalize(how);
                index.normalize(how);
                span.normalize(how);
            }
            Expr::Range {
                start,
                end,
                inclusive,
                span,
            } => {
                start.normalize(how);
                end.normalize(how);
                inclusive.normalize(how);
                span.normalize(how);
            }
            Expr::Map { entries, span } => {
                entries.normalize(how);
                span.normalize(how);
            }
            Expr::Member {
                receiver,
                name,
                name_span,
                span,
            } => {
                receiver.normalize(how);
                name.normalize(how);
                name_span.normalize(how);
                span.normalize(how);
            }
            Expr::Index {
                receiver,
                index,
                span,
            } => {
                receiver.normalize(how);
                index.normalize(how);
                span.normalize(how);
            }
            Expr::Interp { parts, span } => {
                parts.normalize(how);
                span.normalize(how);
            }
            Expr::Match {
                scrutinee,
                arms,
                span,
            } => {
                scrutinee.normalize(how);
                arms.normalize(how);
                span.normalize(how);
            }
            Expr::Object(lit) => lit.normalize(how),
            Expr::Try { expr, span } => {
                expr.normalize(how);
                span.normalize(how);
            }
            Expr::Await { expr, span } => {
                expr.normalize(how);
                span.normalize(how);
            }
            Expr::Spawn {
                future,
                isolate,
                span,
            } => {
                future.normalize(how);
                isolate.normalize(how);
                span.normalize(how);
            }
            Expr::Coalesce {
                value,
                fallback,
                span,
            } => {
                value.normalize(how);
                fallback.normalize(how);
                span.normalize(how);
            }
            Expr::As { expr, ty, span } => {
                expr.normalize(how);
                ty.normalize(how);
                span.normalize(how);
            }
            Expr::Reflect {
                which,
                operand,
                span,
            } => {
                which.normalize(how);
                operand.normalize(how);
                span.normalize(how);
            }
            Expr::Channel {
                elem,
                capacity,
                span,
            } => {
                elem.normalize(how);
                capacity.normalize(how);
                span.normalize(how);
            }
            Expr::TypedModuleCall {
                recv,
                func,
                func_span,
                ty,
                args,
                span,
            } => {
                recv.normalize(how);
                func.normalize(how);
                func_span.normalize(how);
                ty.normalize(how);
                args.normalize(how);
                span.normalize(how);
            }
            Expr::TypedCall {
                name,
                name_span,
                type_args,
                args,
                span,
            } => {
                name.normalize(how);
                name_span.normalize(how);
                type_args.normalize(how);
                args.normalize(how);
                span.normalize(how);
            }
            Expr::TypedMethodCall {
                recv,
                name,
                name_span,
                type_args,
                args,
                span,
            } => {
                recv.normalize(how);
                name.normalize(how);
                name_span.normalize(how);
                type_args.normalize(how);
                args.normalize(how);
                span.normalize(how);
            }
            Expr::InstantiatedType {
                recv,
                type_args,
                span,
            } => {
                recv.normalize(how);
                type_args.normalize(how);
                span.normalize(how);
            }
            Expr::TypeTest { expr, ty, span } => {
                expr.normalize(how);
                ty.normalize(how);
                span.normalize(how);
            }
            Expr::FieldSet {
                receiver,
                field,
                field_span,
                value,
                span,
            } => {
                receiver.normalize(how);
                field.normalize(how);
                field_span.normalize(how);
                value.normalize(how);
                span.normalize(how);
            }
            Expr::TierExpr {
                tier,
                tier_span,
                statics,
                holes,
                span,
            } => {
                tier.normalize(how);
                tier_span.normalize(how);
                // The one thing a caller may ask to erase beyond spans — see
                // [`Normalization::clear_tier_statics`].
                if how.clear_tier_statics {
                    statics.clear();
                }
                statics.normalize(how);
                holes.normalize(how);
                span.normalize(how);
            }
            Expr::NativeFnRef { module, func, span } => {
                module.normalize(how);
                func.normalize(how);
                span.normalize(how);
            }
        }
    }
}

impl Normalize for ObjectLit {
    fn normalize(&mut self, how: &Normalization) {
        let ObjectLit {
            type_name,
            type_name_span,
            fields,
            spread,
            span,
        } = self;
        type_name.normalize(how);
        type_name_span.normalize(how);
        fields.normalize(how);
        spread.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for FieldInit {
    fn normalize(&mut self, how: &Normalization) {
        let FieldInit {
            name,
            name_span,
            value,
            span,
        } = self;
        name.normalize(how);
        name_span.normalize(how);
        value.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for MatchArm {
    fn normalize(&mut self, how: &Normalization) {
        let MatchArm {
            pattern,
            guard,
            body,
            span,
        } = self;
        pattern.normalize(how);
        guard.normalize(how);
        body.normalize(how);
        span.normalize(how);
    }
}

impl Normalize for ClosureBody {
    fn normalize(&mut self, how: &Normalization) {
        match self {
            ClosureBody::Expr(expr) => expr.normalize(how),
            ClosureBody::Block(stmts) => stmts.normalize(how),
        }
    }
}

impl Normalize for StrPart {
    fn normalize(&mut self, how: &Normalization) {
        match self {
            StrPart::Literal(text) => text.normalize(how),
            StrPart::Hole(expr) => expr.normalize(how),
        }
    }
}
