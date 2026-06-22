//! The runtime value representation for the M0 tree-walker.
//!
//! Deliberately a simple boxed `enum` in M0. M1 replaces this with the NaN-boxed
//! value representation and the shape-based object model; keeping it behind this type
//! (and the `display()`/`type_name()` methods) keeps that swap local.
//!
//! `Debug` and `PartialEq` are hand-written rather than derived: function values hold
//! an `Rc<Scope>` whose graph can contain reference cycles (a global function captures
//! the global scope, which holds the function), so a derived recursive `Debug`/`PartialEq`
//! could loop forever. We print functions opaquely and treat them as never structurally
//! equal.

use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

use crate::{Builtin, Closure, EnumDef, EnumValue, ObjectValue, TypeDef};

/// A runtime value.
#[derive(Clone)]
pub enum Value {
    /// The unit value, produced by statements and effectful calls.
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// An immutable list. `Rc` keeps copies cheap (map/filter produce new lists).
    List(Rc<Vec<Value>>),
    /// An immutable set, held in canonical (sorted, de-duplicated) order so iteration,
    /// display, and equality are deterministic and identical to the VM's `Payload::Set`.
    Set(Rc<Vec<Value>>),
    /// An immutable string-keyed map. `BTreeMap` gives deterministic iteration order.
    Map(Rc<BTreeMap<String, Value>>),
    /// A user-defined function or closure.
    Function(Rc<Closure>),
    /// A built-in (native) function from the prelude.
    Builtin(Builtin),
    /// An enum *type* (e.g. the value `Status`), used to construct variants.
    EnumType(Rc<EnumDef>),
    /// An enum *value* (e.g. `Status.Pending` or `OrderError.NegativePrice(2)`).
    Enum(Rc<EnumValue>),
    /// A record or class *type* (e.g. the value `Order`), used to construct instances
    /// and call associated functions (`Order.new(...)`).
    Type(Rc<TypeDef>),
    /// A record or class *instance* — a bag of named field values.
    Object(Rc<ObjectValue>),
}

impl Value {
    /// The display form used by `echo`, `~` concatenation, and (later) interpolation.
    /// In M1 this becomes `Display` trait dispatch; in M0 it is built in per value kind.
    pub fn display(&self) -> String {
        match self {
            Value::Unit => String::new(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => format_float(*f),
            Value::Str(s) => s.clone(),
            Value::List(items) => {
                let parts: Vec<String> = items.iter().map(Value::repr).collect();
                format!("[{}]", parts.join(", "))
            }
            // Braces with no key colons (`{1, 2, 3}`) distinguish a set from a non-empty map;
            // an empty set is `{}`, like an empty map.
            Value::Set(items) => {
                let parts: Vec<String> = items.iter().map(Value::repr).collect();
                format!("{{{}}}", parts.join(", "))
            }
            Value::Map(entries) => {
                let parts: Vec<String> = entries
                    .iter()
                    .map(|(k, v)| format!("{k:?}: {}", v.repr()))
                    .collect();
                format!("{{{}}}", parts.join(", "))
            }
            Value::Function(_) => "<fn>".to_string(),
            Value::Builtin(b) => format!("<builtin {}>", b.name()),
            Value::EnumType(def) => format!("<enum {}>", def.name()),
            Value::Enum(value) => value.display(),
            Value::Type(def) => format!("<type {}>", def.name()),
            Value::Object(object) => object.display(),
        }
    }

    /// The representation of a value *inside* a collection or object: strings are quoted
    /// so the structure stays legible (`["a", "b"]`, not `[a, b]`).
    pub(crate) fn repr(&self) -> String {
        match self {
            Value::Str(s) => format!("{s:?}"),
            other => other.display(),
        }
    }

    /// The user-facing name of this value's type, for diagnostics.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Unit => "unit",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "string",
            Value::List(_) => "list",
            Value::Set(_) => "set",
            Value::Map(_) => "map",
            Value::Function(_) | Value::Builtin(_) => "function",
            Value::EnumType(_) => "enum type",
            Value::Enum(_) => "enum",
            Value::Type(_) => "type",
            Value::Object(_) => "object",
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Unit => write!(f, "Unit"),
            Value::Bool(b) => write!(f, "Bool({b})"),
            Value::Int(i) => write!(f, "Int({i})"),
            Value::Float(x) => write!(f, "Float({x})"),
            Value::Str(s) => write!(f, "Str({s:?})"),
            Value::List(items) => write!(f, "List({items:?})"),
            Value::Set(items) => write!(f, "Set({items:?})"),
            Value::Map(entries) => write!(f, "Map({entries:?})"),
            Value::Function(_) => write!(f, "Function(<fn>)"),
            Value::Builtin(b) => write!(f, "Builtin({})", b.name()),
            Value::EnumType(def) => write!(f, "EnumType({})", def.name()),
            Value::Enum(value) => write!(f, "Enum({})", value.display()),
            Value::Type(def) => write!(f, "Type({})", def.name()),
            Value::Object(object) => write!(f, "Object({})", object.display()),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Unit, Value::Unit) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Set(a), Value::Set(b)) => a == b,
            (Value::Map(a), Value::Map(b)) => a == b,
            (Value::Enum(a), Value::Enum(b)) => a == b,
            (Value::Object(a), Value::Object(b)) => a == b,
            // Functions and types are not structurally comparable.
            _ => false,
        }
    }
}

/// Render a float deterministically. Whole-valued floats keep a trailing `.0` so they
/// are visibly distinct from ints (`3.0`, not `3`).
fn format_float(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 {
        format!("{f:.1}")
    } else {
        f.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: Vec<Value>) -> Value {
        Value::List(Rc::new(items))
    }

    fn map(pairs: &[(&str, Value)]) -> Value {
        Value::Map(Rc::new(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        ))
    }

    #[test]
    fn display_of_scalars() {
        assert_eq!(Value::Unit.display(), "");
        assert_eq!(Value::Bool(true).display(), "true");
        assert_eq!(Value::Bool(false).display(), "false");
        assert_eq!(Value::Int(-42).display(), "-42");
        assert_eq!(Value::Str("hi".into()).display(), "hi");
    }

    #[test]
    fn float_formatting_keeps_whole_values_distinct_from_ints() {
        assert_eq!(format_float(3.0), "3.0");
        assert_eq!(format_float(-2.0), "-2.0");
        assert_eq!(format_float(2.5), "2.5");
        assert_eq!(format_float(-1.25), "-1.25");
        assert_eq!(format_float(0.0), "0.0");
        // Non-finite values fall back to the default formatting rather than `.0`.
        assert_eq!(format_float(f64::INFINITY), "inf");
        assert_eq!(format_float(f64::NAN), "NaN");
    }

    #[test]
    fn repr_quotes_strings_but_display_does_not() {
        let s = Value::Str("x".into());
        assert_eq!(s.display(), "x");
        assert_eq!(s.repr(), "\"x\"");
        // Non-strings render the same either way.
        assert_eq!(Value::Int(1).repr(), "1");
    }

    #[test]
    fn collections_use_repr_for_their_elements() {
        assert_eq!(
            list(vec![Value::Int(1), Value::Str("a".into())]).display(),
            "[1, \"a\"]"
        );
        // Maps iterate in deterministic (sorted) key order.
        assert_eq!(
            map(&[("b", Value::Int(2)), ("a", Value::Int(1))]).display(),
            "{\"a\": 1, \"b\": 2}"
        );
        assert_eq!(list(vec![]).display(), "[]");
    }

    #[test]
    fn type_names() {
        assert_eq!(Value::Unit.type_name(), "unit");
        assert_eq!(Value::Bool(true).type_name(), "bool");
        assert_eq!(Value::Int(0).type_name(), "int");
        assert_eq!(Value::Float(0.0).type_name(), "float");
        assert_eq!(Value::Str(String::new()).type_name(), "string");
        assert_eq!(list(vec![]).type_name(), "list");
        assert_eq!(map(&[]).type_name(), "map");
        assert_eq!(Value::Builtin(Builtin::Len).type_name(), "function");
    }

    #[test]
    fn structural_equality_and_cross_kind_inequality() {
        assert_eq!(Value::Int(1), Value::Int(1));
        assert_ne!(Value::Int(1), Value::Int(2));
        assert_eq!(Value::Unit, Value::Unit);
        assert_eq!(list(vec![Value::Int(1)]), list(vec![Value::Int(1)]));
        assert_ne!(list(vec![Value::Int(1)]), list(vec![Value::Int(2)]));
        // Different kinds are never equal; functions are never equal even to themselves.
        assert_ne!(Value::Int(1), Value::Bool(true));
        assert_ne!(Value::Builtin(Builtin::Len), Value::Builtin(Builtin::Len));
    }

    #[test]
    fn debug_is_shallow_and_does_not_panic() {
        // Debug must never recurse into the (possibly cyclic) closure scope graph.
        assert_eq!(format!("{:?}", Value::Int(7)), "Int(7)");
        assert_eq!(
            format!("{:?}", Value::Builtin(Builtin::Sum)),
            "Builtin(sum)"
        );
    }
}
