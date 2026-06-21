//! The type lattice: the vocabulary the M1 checker reasons in.
//!
//! Pure data, no inference logic (that lives in `lang-check`). A [`Type`] is either a concrete
//! type (`int`, `List<T>`, a named record/class/enum, a function), an *inference variable*
//! (`Var`, filled in during checking), or [`Type::Unknown`] — the **gradual top**. `Unknown`
//! is compatible with everything and is what the checker falls back to wherever it cannot (yet)
//! infer a precise type. That fallback is what makes the checker *gradual*: an un-inferable
//! expression never produces a false-positive error, so every program the M0 tree-walker
//! accepts keeps type-checking. The checker only reports an error when types are *concretely*
//! known and unambiguously wrong.
//!
//! ## `TypeId` interning — deferred
//!
//! The architecture calls for interning types behind a `TypeId`. That is a throughput
//! optimization (cheap structural equality, small handles) with no effect on what the checker
//! accepts or rejects, and the checker runs once per compile over a small AST. Interning is
//! therefore deferred until a benchmark justifies it; today `Type` is a plain owned tree.

use lang_ast::TypeRef;

mod traits;
pub use traits::{BUILTIN_TRAITS, BuiltinTrait, operator_trait};

/// A type in the lattice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    /// The gradual top: compatible with every type, the fallback for anything not yet inferred.
    /// Named "Unknown" rather than "Any" to stress that it marks *absence of information*, not a
    /// universal supertype the user can name.
    Unknown,
    /// `void` / the empty tuple — the type of statements and `Ok()`-style unit payloads.
    Unit,
    Int,
    Float,
    Bool,
    String,
    List(Box<Type>),
    Map(Box<Type>, Box<Type>),
    /// `?T` / `Option<T>`.
    Option(Box<Type>),
    /// `Result<T, E>`.
    Result(Box<Type>, Box<Type>),
    /// A declared record/class/enum, or an imported (opaque, until M1.9) type, named.
    Named(String),
    /// A function value.
    Fn {
        params: Vec<Type>,
        ret: Box<Type>,
    },
    /// An inference variable, resolved during checking. (Unused by the conservative gradual
    /// pass today; reserved for the unification front that hardens inference.)
    Var(u32),
}

impl Type {
    /// Whether arithmetic (`+ - * / %`) accepts this type: the two numeric types, or a
    /// not-yet-known type (gradual — never the source of a false positive).
    pub fn is_numeric(&self) -> bool {
        matches!(self, Type::Int | Type::Float)
    }

    /// Whether this type carries no static information — the gradual top or an open variable.
    /// Checks suppress diagnostics when an operand is gradual, so inference gaps never error.
    pub fn is_gradual(&self) -> bool {
        matches!(self, Type::Unknown | Type::Var(_))
    }

    /// The built-in type names that desugar to a lattice variant rather than a [`Type::Named`].
    /// `lang-check` uses this to decide whether a `TypeRef` base name needs to resolve to a
    /// *declared* type (for the unknown-type diagnostic).
    pub fn is_builtin_name(name: &str) -> bool {
        matches!(
            name,
            "int"
                | "float"
                | "bool"
                | "string"
                | "void"
                | "unit"
                | "List"
                | "Map"
                | "Option"
                | "Result"
        )
    }

    /// Desugar a surface [`TypeRef`] into a lattice [`Type`]. `?T` becomes `Option<T>`; the
    /// built-in names map to their variants; everything else becomes [`Type::Named`] (a
    /// declared or imported type). Resolution of whether a `Named` *exists* is the checker's
    /// job — this is a pure structural mapping.
    pub fn from_ref(ty: &TypeRef) -> Type {
        match ty {
            TypeRef::Optional { inner, .. } => Type::Option(Box::new(Type::from_ref(inner))),
            TypeRef::Named { name, args, .. } => {
                let arg = |i: usize| args.get(i).map(Type::from_ref).unwrap_or(Type::Unknown);
                match name.as_str() {
                    "int" => Type::Int,
                    "float" => Type::Float,
                    "bool" => Type::Bool,
                    "string" => Type::String,
                    "void" | "unit" => Type::Unit,
                    "List" => Type::List(Box::new(arg(0))),
                    "Map" => Type::Map(Box::new(arg(0)), Box::new(arg(1))),
                    "Option" => Type::Option(Box::new(arg(0))),
                    "Result" => Type::Result(Box::new(arg(0)), Box::new(arg(1))),
                    _ => Type::Named(name.clone()),
                }
            }
        }
    }
}

impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Unknown => f.write_str("?"),
            Type::Unit => f.write_str("void"),
            Type::Int => f.write_str("int"),
            Type::Float => f.write_str("float"),
            Type::Bool => f.write_str("bool"),
            Type::String => f.write_str("string"),
            Type::List(t) => write!(f, "List<{t}>"),
            Type::Map(k, v) => write!(f, "Map<{k}, {v}>"),
            Type::Option(t) => write!(f, "Option<{t}>"),
            Type::Result(t, e) => write!(f, "Result<{t}, {e}>"),
            Type::Named(n) => f.write_str(n),
            Type::Fn { params, ret } => {
                f.write_str("fn(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{p}")?;
                }
                write!(f, ") -> {ret}")
            }
            Type::Var(n) => write!(f, "?{n}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lang_span::Span;

    fn named(name: &str, args: Vec<TypeRef>) -> TypeRef {
        TypeRef::Named {
            name: name.to_string(),
            args,
            span: Span::new(0, 0),
        }
    }

    #[test]
    fn primitives_desugar() {
        assert_eq!(Type::from_ref(&named("int", vec![])), Type::Int);
        assert_eq!(Type::from_ref(&named("string", vec![])), Type::String);
        assert_eq!(Type::from_ref(&named("void", vec![])), Type::Unit);
    }

    #[test]
    fn optional_is_option() {
        let opt = TypeRef::Optional {
            inner: Box::new(named("int", vec![])),
            span: Span::new(0, 0),
        };
        assert_eq!(Type::from_ref(&opt), Type::Option(Box::new(Type::Int)));
    }

    #[test]
    fn generics_carry_args() {
        let list = named("List", vec![named("Item", vec![])]);
        assert_eq!(
            Type::from_ref(&list),
            Type::List(Box::new(Type::Named("Item".to_string())))
        );
        let res = named(
            "Result",
            vec![named("void", vec![]), named("OrderError", vec![])],
        );
        assert_eq!(
            Type::from_ref(&res),
            Type::Result(
                Box::new(Type::Unit),
                Box::new(Type::Named("OrderError".to_string()))
            )
        );
    }

    #[test]
    fn unknown_is_gradual_and_numeric_is_strict() {
        assert!(Type::Unknown.is_gradual());
        assert!(Type::Int.is_numeric());
        assert!(Type::Float.is_numeric());
        assert!(!Type::String.is_numeric());
        assert!(!Type::Unknown.is_numeric());
    }
}
