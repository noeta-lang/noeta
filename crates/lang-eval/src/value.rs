//! The runtime value representation for the M0 tree-walker.
//!
//! Deliberately a simple boxed `enum` in M0. M1 replaces this with the NaN-boxed
//! value representation and the shape-based object model; keeping it behind this type
//! (and the `display()` method the evaluator uses) keeps that swap local.

/// A runtime value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// The unit value, produced by statements and effectful calls.
    Unit,
    /// A string.
    Str(String),
}

impl Value {
    /// The display form used by `echo` and string interpolation. (In M1 this becomes
    /// the `Display` trait dispatch; in M0 it is built in per value kind.)
    pub fn display(&self) -> String {
        match self {
            Value::Unit => String::new(),
            Value::Str(s) => s.clone(),
        }
    }
}
