//! The **declared shape** of a data declaration: its members as `(name, declared type spelling)`
//! pairs, in declaration order.
//!
//! One derivation, two consumers. A native extension can look at the shape of the declaration it
//! decorates from two seams, and they must never disagree:
//!
//! - `ExtDerive::validate` — the checker hands a derive recipe the deriving type's name and its
//!   fields, so the recipe can reject a shape it cannot serve (`E0050`);
//! - `DirectiveCtx::fields` — the loader hands an `ExtDirective::expand` hook the same shape, so a
//!   code-generating directive can generate members *derived from* the declaration's own fields.
//!
//! Both answer the same question ("what was this written on?"), so both call
//! [`field_shape`]/[`decl_shape`] here rather than each walking the AST their own way. Two walks
//! would drift the moment the surface grows a type form, and the drift would be invisible: a derive
//! recipe and an expansion hook in the *same* extension would then see the same struct differently.
//!
//! ## What a spelling is
//!
//! The **declared surface spelling**, at full fidelity — `List<int>` is `"List<int>"`, not `"List"`,
//! and `?User` is `"?User"`, not `"Option<User>"`. Nothing is normalized through the type lattice:
//! a hook that generates code from a field's type needs the text the author wrote, because that text
//! is what it must write back out.
//!
//! The one adjustment is **name shortening**: a namespace-qualified identity renders as its short
//! name ([`short_type_name`]), because by the time either consumer runs, the linker has rewritten an
//! imported `Uuid` to `std.id.Uuid` — an identity the author never wrote and that no generated
//! source should spell. This is the same choice `noeta-types`' `Display` makes for the same reason.

use crate::{FieldDecl, Param, Stmt, TypeRef, VariantDecl, short_type_name};

/// The spelling reported for a member with no type annotation: the gradual top, which is what an
/// unannotated member means and what one would have to write to say it explicitly.
pub const UNANNOTATED: &str = "dyn";

/// Render a [`TypeRef`] back to its declared surface spelling, with every named identity shortened
/// to the name the author wrote (see the module docs).
pub fn type_spelling(ty: &TypeRef) -> String {
    match ty {
        TypeRef::Named { name, args, .. } if args.is_empty() => short_type_name(name).to_string(),
        TypeRef::Named { name, args, .. } => {
            let args: Vec<String> = args.iter().map(type_spelling).collect();
            format!("{}<{}>", short_type_name(name), args.join(", "))
        }
        TypeRef::DynTrait { trait_name, .. } => format!("dyn {}", short_type_name(trait_name)),
        TypeRef::Optional { inner, .. } => format!("?{}", type_spelling(inner)),
        TypeRef::Union { members, .. } => members
            .iter()
            .map(type_spelling)
            .collect::<Vec<_>>()
            .join(" | "),
        TypeRef::Tuple { elements, .. } => {
            let elements: Vec<String> = elements.iter().map(type_spelling).collect();
            format!("({})", elements.join(", "))
        }
        TypeRef::Fn { params, ret, .. } => {
            let params: Vec<String> = params.iter().map(type_spelling).collect();
            format!("({}) -> {}", params.join(", "), type_spelling(ret))
        }
        TypeRef::AssocProjection { name, .. } => format!("Self::{name}"),
    }
}

/// The spelling of an optional annotation — [`UNANNOTATED`] when there is none.
pub fn annotation_spelling(ty: &Option<TypeRef>) -> String {
    ty.as_ref()
        .map(type_spelling)
        .unwrap_or_else(|| UNANNOTATED.to_string())
}

/// A `struct`/`class` declaration's shape: each field as `(name, declared type spelling)`, in
/// declaration order.
pub fn field_shape(fields: &[FieldDecl]) -> Vec<(String, String)> {
    fields
        .iter()
        .map(|f| (f.name.clone(), annotation_spelling(&f.ty)))
        .collect()
}

/// An `enum` declaration's shape: each **variant** as `(name, payload spelling)`, in declaration
/// order — the enum's analogue of a struct's fields, because a variant is what an enum is made of.
///
/// The payload is spelled exactly as declared, parentheses included: `"(index: int)"` for a named
/// payload, `"(T)"` for a positional one, and the **empty string** for a variant that carries no
/// payload (a plain or backed variant). The empty string rather than `"()"` so a hook can ask
/// `payload.is_empty()` — the question it actually has — instead of comparing against a spelling.
pub fn variant_shape(variants: &[VariantDecl]) -> Vec<(String, String)> {
    variants
        .iter()
        .map(|v| (v.name.clone(), payload_spelling(&v.fields)))
        .collect()
}

/// One variant's payload, as declared. Empty for a variant that has none.
fn payload_spelling(fields: &[Param]) -> String {
    if fields.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = fields
        .iter()
        .map(|p| match &p.ty {
            // A named payload field (`NegativePrice(index: int)`) spells name and type.
            Some(ty) => format!("{}: {}", p.name, type_spelling(ty)),
            // A POSITIONAL payload (`Leaf(T)`) is parsed with its type as the parameter's *name*
            // and no annotation, so the name is the spelling — the same reconstruction the
            // checker's `variant_field_type` performs.
            None => short_type_name(&p.name).to_string(),
        })
        .collect();
    format!("({})", parts.join(", "))
}

/// The declared shape of whatever a statement declares: a struct's or class's fields, an enum's
/// variants, and **nothing** for anything else (a `fn`, a `trait`, a plain statement) — those
/// declare no members with types, so the honest answer is an empty shape rather than an error.
pub fn decl_shape(stmt: &Stmt) -> Vec<(String, String)> {
    match stmt {
        Stmt::Struct(d) => field_shape(&d.fields),
        Stmt::Class(d) => field_shape(&d.fields),
        Stmt::Enum(d) => variant_shape(&d.variants),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_span::Span;

    /// `noeta-ast` is the bottom of the stack — the parser depends on it, not the other way round —
    /// so these fixtures are built by hand. The *parse*-driven fidelity tests (an author writes
    /// `List<int>`, a hook receives `"List<int>"`) live where a parser is available: the loader's
    /// `tests/dir_expansion.rs`, over a real program and a real hook.
    fn span() -> Span {
        Span::new(0, 0)
    }

    fn named(name: &str, args: Vec<TypeRef>) -> TypeRef {
        TypeRef::Named {
            name: name.to_string(),
            args,
            span: span(),
        }
    }

    fn field(name: &str, ty: Option<TypeRef>) -> FieldDecl {
        FieldDecl {
            name: name.to_string(),
            name_span: span(),
            mut_field: false,
            is_public: false,
            ty,
            default: None,
            attrs: Vec::new(),
            span: span(),
        }
    }

    fn param(name: &str, ty: Option<TypeRef>) -> Param {
        Param {
            attrs: Vec::new(),
            name: name.to_string(),
            name_span: span(),
            ty,
            default: None,
            span: span(),
        }
    }

    fn variant(name: &str, fields: Vec<Param>) -> VariantDecl {
        VariantDecl {
            name: name.to_string(),
            name_span: span(),
            fields,
            backed_value: None,
            attrs: Vec::new(),
            span: span(),
        }
    }

    #[test]
    fn a_generic_type_keeps_its_arguments() {
        // The whole point of "full fidelity": `List<int>` must not render as `List`, or a hook
        // generating an accessor from a field would generate one with the wrong return type.
        assert_eq!(
            type_spelling(&named("List", vec![named("int", vec![])])),
            "List<int>"
        );
        assert_eq!(
            type_spelling(&named(
                "Map",
                vec![
                    named("string", vec![]),
                    named("List", vec![named("int", vec![])]),
                ],
            )),
            "Map<string, List<int>>"
        );
    }

    #[test]
    fn the_surface_sugar_is_not_desugared() {
        // `?T` is spelled `?T`, not `Option<T>` — a hook writes source, and source is written in
        // the surface language.
        assert_eq!(
            type_spelling(&TypeRef::Optional {
                inner: Box::new(named("User", vec![])),
                span: span(),
            }),
            "?User"
        );
        assert_eq!(
            type_spelling(&TypeRef::Union {
                members: vec![named("int", vec![]), named("string", vec![])],
                span: span(),
            }),
            "int | string"
        );
        assert_eq!(
            type_spelling(&TypeRef::Tuple {
                elements: vec![named("int", vec![]), named("string", vec![])],
                span: span(),
            }),
            "(int, string)"
        );
        assert_eq!(
            type_spelling(&TypeRef::Fn {
                params: vec![named("int", vec![])],
                ret: Box::new(named("string", vec![])),
                span: span(),
            }),
            "(int) -> string"
        );
    }

    #[test]
    fn a_qualified_identity_renders_as_the_name_that_was_written() {
        // The linker rewrites an imported `Uuid` to `std.id.Uuid` before either consumer runs; a
        // generated source spelling that identity would name something the consumer's file cannot
        // resolve. Nested arguments shorten too, or `List<std.id.Uuid>` would leak the same way.
        assert_eq!(type_spelling(&named("std.id.Uuid", vec![])), "Uuid");
        assert_eq!(
            type_spelling(&named("List", vec![named("app.models.User", vec![])])),
            "List<User>"
        );
        assert_eq!(
            type_spelling(&TypeRef::DynTrait {
                trait_name: "pkg.Shape".to_string(),
                span: span(),
            }),
            "dyn Shape"
        );
    }

    #[test]
    fn an_unannotated_field_reports_the_gradual_top() {
        assert_eq!(
            field_shape(&[field("x", None), field("y", Some(named("int", vec![])))]),
            vec![
                ("x".to_string(), UNANNOTATED.to_string()),
                ("y".to_string(), "int".to_string()),
            ]
        );
    }

    #[test]
    fn a_variant_reports_its_payload_as_declared() {
        // A payload-free variant is the empty string, not `"()"`, so the question a hook has —
        // "does this variant carry anything?" — is `is_empty()`.
        assert_eq!(
            variant_shape(&[
                variant("Empty", Vec::new()),
                variant("Named", vec![param("index", Some(named("int", vec![])))]),
                // A POSITIONAL payload parses its type as the parameter's name, with no annotation.
                variant("Positional", vec![param("string", None)]),
                variant(
                    "Two",
                    vec![
                        param("a", Some(named("int", vec![]))),
                        param("b", Some(named("List", vec![named("string", vec![])]))),
                    ],
                ),
            ]),
            vec![
                ("Empty".to_string(), String::new()),
                ("Named".to_string(), "(index: int)".to_string()),
                ("Positional".to_string(), "(string)".to_string()),
                ("Two".to_string(), "(a: int, b: List<string>)".to_string()),
            ]
        );
    }
}
