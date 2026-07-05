//! Rendering a reflected [`TypeRepr`] back to Noeta surface syntax for hover.
//!
//! `TypeRepr` (carried on `Checked.type_of_sites`) has no `Display` — it is a runtime/compile-time
//! reflection tag, not a source form. Hover wants the *source* spelling a developer recognizes, so
//! this module walks the tag and reproduces the surface type grammar: scalars by name, containers
//! as `List<T>` / `Map<K, V>`, optionals as `?T`, unions as `A | B`, function types as
//! `(A, B) -> R`, and nominal types by name with `<…>` type arguments.

use noeta_ast::reflect::TypeRepr;

/// Render a [`TypeRepr`] as its Noeta surface spelling.
pub fn render_type(repr: &TypeRepr) -> String {
    match repr {
        TypeRepr::Int => "int".to_string(),
        TypeRepr::Float => "float".to_string(),
        TypeRepr::F32 => "f32".to_string(),
        TypeRepr::Bool => "bool".to_string(),
        TypeRepr::Str => "string".to_string(),
        TypeRepr::Bytes => "bytes".to_string(),
        TypeRepr::Unit => "void".to_string(),
        TypeRepr::Dyn => "dyn".to_string(),
        TypeRepr::List(t) => format!("List<{}>", render_type(t)),
        TypeRepr::Set(t) => format!("Set<{}>", render_type(t)),
        TypeRepr::Option(t) => format!("?{}", render_type(t)),
        TypeRepr::Map(k, v) => format!("Map<{}, {}>", render_type(k), render_type(v)),
        TypeRepr::Result(o, e) => format!("Result<{}, {}>", render_type(o), render_type(e)),
        TypeRepr::Enum(name, args)
        | TypeRepr::Struct(name, args)
        | TypeRepr::Class(name, args)
        | TypeRepr::Named(name, args) => render_nominal(name, args),
        TypeRepr::Fn(params, ret) => {
            let params = params
                .iter()
                .map(render_type)
                .collect::<Vec<_>>()
                .join(", ");
            format!("({params}) -> {}", render_type(ret))
        }
        TypeRepr::Union(members) => members
            .iter()
            .map(render_type)
            .collect::<Vec<_>>()
            .join(" | "),
    }
}

/// A nominal type `Name` or `Name<A, B>`.
fn render_nominal(name: &str, args: &[TypeRepr]) -> String {
    if args.is_empty() {
        name.to_string()
    } else {
        let args = args.iter().map(render_type).collect::<Vec<_>>().join(", ");
        format!("{name}<{args}>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(t: TypeRepr) -> Box<TypeRepr> {
        Box::new(t)
    }

    #[test]
    fn scalars() {
        assert_eq!(render_type(&TypeRepr::Int), "int");
        assert_eq!(render_type(&TypeRepr::Str), "string");
        assert_eq!(render_type(&TypeRepr::Unit), "void");
        assert_eq!(render_type(&TypeRepr::Dyn), "dyn");
    }

    #[test]
    fn containers_nest() {
        assert_eq!(
            render_type(&TypeRepr::List(boxed(TypeRepr::Int))),
            "List<int>"
        );
        assert_eq!(
            render_type(&TypeRepr::Map(boxed(TypeRepr::Str), boxed(TypeRepr::Int))),
            "Map<string, int>"
        );
        assert_eq!(
            render_type(&TypeRepr::List(boxed(TypeRepr::Option(boxed(
                TypeRepr::Int
            ))))),
            "List<?int>"
        );
    }

    #[test]
    fn nominal_with_and_without_args() {
        assert_eq!(
            render_type(&TypeRepr::Struct("Point".to_string(), vec![])),
            "Point"
        );
        assert_eq!(
            render_type(&TypeRepr::Class("Box".to_string(), vec![TypeRepr::Int])),
            "Box<int>"
        );
    }

    #[test]
    fn function_and_union() {
        assert_eq!(
            render_type(&TypeRepr::Fn(
                vec![TypeRepr::Int, TypeRepr::Str],
                boxed(TypeRepr::Bool)
            )),
            "(int, string) -> bool"
        );
        assert_eq!(
            render_type(&TypeRepr::Union(vec![TypeRepr::Int, TypeRepr::Str])),
            "int | string"
        );
    }
}
