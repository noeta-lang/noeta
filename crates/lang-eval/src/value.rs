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

use crate::{Builtin, Closure};

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
    /// An immutable string-keyed map. `BTreeMap` gives deterministic iteration order.
    Map(Rc<BTreeMap<String, Value>>),
    /// A user-defined function or closure.
    Function(Rc<Closure>),
    /// A built-in (native) function from the prelude.
    Builtin(Builtin),
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
            Value::Map(entries) => {
                let parts: Vec<String> = entries
                    .iter()
                    .map(|(k, v)| format!("{k:?}: {}", v.repr()))
                    .collect();
                format!("{{{}}}", parts.join(", "))
            }
            Value::Function(_) => "<fn>".to_string(),
            Value::Builtin(b) => format!("<builtin {}>", b.name()),
        }
    }

    /// The representation of a value *inside* a collection: strings are quoted so the
    /// structure stays legible (`["a", "b"]`, not `[a, b]`).
    fn repr(&self) -> String {
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
            Value::Map(_) => "map",
            Value::Function(_) | Value::Builtin(_) => "function",
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
            Value::Map(entries) => write!(f, "Map({entries:?})"),
            Value::Function(_) => write!(f, "Function(<fn>)"),
            Value::Builtin(b) => write!(f, "Builtin({})", b.name()),
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
            (Value::Map(a), Value::Map(b)) => a == b,
            // Functions are not structurally comparable.
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
