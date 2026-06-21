//! The built-in trait registry — the fixed set of traits an `impl` block or `#[derive(...)]`
//! may name.
//!
//! The language has no user-defined traits: a class implements one of these built-ins to "light
//! up" its operator or protocol (`impl Add` enables `+`, `impl Display` enables `echo`), and
//! `#[derive(...)]` asks the compiler to synthesize the implementation for the value-object cases.
//! This table is the single source of truth the checker validates `impl`/`derive` names against
//! (`lang-check`), and the operator → method correspondence it encodes is kept in lockstep with
//! [`BinaryOp::overload_method`](lang_ast::BinaryOp::overload_method) by a unit test below.
//!
//! M1.8a wires the *infix operator traits* (`Add`/`Sub`/`Mul`/`Div`/`Concat`) end-to-end through
//! both backends; M1.8b adds `Equatable` (`==`/`!=` → `eq`). Every trait/derive name is validated
//! against this table. The behavior behind the remaining protocols (`Comparable` ordering — which
//! needs an `Ordering` type — `Display`/`ToJson` codegen, `Index`/`Members`/`Callable` dispatch)
//! is the rest of M1.8b; their names are registered now so the surface parses, checks, and reads
//! as the design intends. (`TryAdd` is fallible-by-method: `a.try_add(b)?`, no operator wiring.)

use lang_ast::BinaryOp;

/// One built-in trait: the name users write in `impl`/`#[derive(...)]`, the single method an
/// `impl` block must provide (with its user-facing arity, i.e. excluding the receiver), the infix
/// operator it overloads (if any), and whether it may be derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinTrait {
    pub name: &'static str,
    /// The required method's name and parameter count *excluding the receiver*, or `None` for a
    /// marker trait whose behavior is fully synthesized (e.g. `Clone`, `ToJson`) and so imposes no
    /// single hand-written method.
    pub required_method: Option<(&'static str, usize)>,
    /// The infix operator this trait overloads, for the operator traits; `None` otherwise.
    pub operator: Option<BinaryOp>,
    /// Whether `#[derive(Name)]` is accepted for this trait.
    pub derivable: bool,
}

impl BuiltinTrait {
    /// Look up a trait by the name written in source, or `None` if it is not a built-in trait.
    pub fn lookup(name: &str) -> Option<&'static BuiltinTrait> {
        BUILTIN_TRAITS.iter().find(|t| t.name == name)
    }
}

/// The built-in trait that overloads `op`, if any. Used by the checker; the backends use the
/// lighter [`BinaryOp::overload_method`](lang_ast::BinaryOp::overload_method) directly.
pub fn operator_trait(op: BinaryOp) -> Option<&'static BuiltinTrait> {
    BUILTIN_TRAITS.iter().find(|t| t.operator == Some(op))
}

/// The complete set of built-in traits. Operator traits come first, then the protocol/derivable
/// traits. Keep the operator entries consistent with [`BinaryOp::overload_method`].
pub const BUILTIN_TRAITS: &[BuiltinTrait] = &[
    // --- infix operator traits (wired through both backends in M1.8a) ---
    BuiltinTrait {
        name: "Add",
        required_method: Some(("add", 1)),
        operator: Some(BinaryOp::Add),
        derivable: false,
    },
    BuiltinTrait {
        name: "Sub",
        required_method: Some(("sub", 1)),
        operator: Some(BinaryOp::Sub),
        derivable: false,
    },
    BuiltinTrait {
        name: "Mul",
        required_method: Some(("mul", 1)),
        operator: Some(BinaryOp::Mul),
        derivable: false,
    },
    BuiltinTrait {
        name: "Div",
        required_method: Some(("div", 1)),
        operator: Some(BinaryOp::Div),
        derivable: false,
    },
    BuiltinTrait {
        name: "Concat",
        required_method: Some(("concat", 1)),
        operator: Some(BinaryOp::Concat),
        derivable: false,
    },
    // --- protocol traits (surface + validation now; behavior in M1.8b) ---
    BuiltinTrait {
        name: "Equatable",
        required_method: Some(("eq", 1)),
        operator: None,
        derivable: true,
    },
    BuiltinTrait {
        name: "Comparable",
        required_method: Some(("compare", 1)),
        operator: None,
        derivable: true,
    },
    BuiltinTrait {
        name: "Display",
        required_method: Some(("to_string", 0)),
        operator: None,
        derivable: true,
    },
    BuiltinTrait {
        name: "Clone",
        required_method: None,
        operator: None,
        derivable: true,
    },
    BuiltinTrait {
        name: "ToJson",
        required_method: None,
        operator: None,
        derivable: true,
    },
    BuiltinTrait {
        name: "Serialize",
        required_method: None,
        operator: None,
        derivable: true,
    },
    BuiltinTrait {
        name: "Attribute",
        required_method: None,
        operator: None,
        derivable: true,
    },
    BuiltinTrait {
        name: "Index",
        required_method: Some(("get", 1)),
        operator: None,
        derivable: false,
    },
    BuiltinTrait {
        name: "Length",
        required_method: Some(("len", 0)),
        operator: None,
        derivable: false,
    },
    BuiltinTrait {
        name: "Iterable",
        required_method: Some(("iter", 0)),
        operator: None,
        derivable: false,
    },
    BuiltinTrait {
        name: "Callable",
        required_method: None,
        operator: None,
        derivable: false,
    },
    BuiltinTrait {
        name: "Members",
        required_method: Some(("get", 1)),
        operator: None,
        derivable: false,
    },
    BuiltinTrait {
        name: "DynamicCall",
        required_method: Some(("call", 2)),
        operator: None,
        derivable: false,
    },
    BuiltinTrait {
        name: "TryAdd",
        required_method: Some(("try_add", 1)),
        operator: None,
        derivable: false,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every infix operator that `lang-ast` says is overloadable must have exactly one operator
    /// trait here whose required method matches — and vice versa. This pins the two definitions
    /// (the backends' `overload_method`, the checker's registry) together so they cannot drift.
    #[test]
    fn operator_traits_match_overload_methods() {
        use BinaryOp::*;
        for op in [
            Add, Sub, Mul, Div, Rem, Concat, Eq, Ne, Lt, Le, Gt, Ge, And, Or,
        ] {
            match op.overload_method() {
                Some(method) => {
                    let t = operator_trait(op)
                        .unwrap_or_else(|| panic!("no operator trait for {op:?}"));
                    assert_eq!(
                        t.required_method,
                        Some((method, 1)),
                        "method mismatch for {op:?}"
                    );
                }
                None => assert!(
                    operator_trait(op).is_none(),
                    "{op:?} is not overloadable but has an operator trait"
                ),
            }
        }
    }

    /// `Equatable`'s required method is the one the backends dispatch `==`/`!=` to, and only the
    /// two equality operators carry a negation flag.
    #[test]
    fn equatable_dispatch_matches_registry() {
        use BinaryOp::*;
        let eq = BuiltinTrait::lookup("Equatable").unwrap();
        assert_eq!(eq.required_method, Some(("eq", 1)));
        assert_eq!(Eq.equatable_negation(), Some(false));
        assert_eq!(Ne.equatable_negation(), Some(true));
        for op in [Add, Sub, Mul, Div, Rem, Concat, Lt, Le, Gt, Ge, And, Or] {
            assert_eq!(
                op.equatable_negation(),
                None,
                "{op:?} is not an equality op"
            );
        }
    }

    #[test]
    fn lookup_finds_and_rejects() {
        assert_eq!(BuiltinTrait::lookup("Add").map(|t| t.name), Some("Add"));
        assert!(BuiltinTrait::lookup("Equatable").is_some_and(|t| t.derivable));
        assert!(BuiltinTrait::lookup("Add").is_some_and(|t| !t.derivable));
        assert!(BuiltinTrait::lookup("Nonexistent").is_none());
    }
}
