//! The runtime value representation for the M0 tree-walker.
//!
//! Deliberately a simple boxed `enum` in M0. M1 replaces this with the NaN-boxed
//! value representation and the shape-based object model; keeping it behind this type
//! (and the `display()`/`type_name()` methods) keeps that swap local.

/// A runtime value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// The unit value, produced by statements and effectful calls.
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
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
