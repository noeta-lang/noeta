//! The extern-type value contract — the uniform behavior seam a registered
//! native type implements ONCE, hosted by both backends.
//!
//! An [`crate::registry::ExtType`] names the type and carries its method signatures + dispatch;
//! this trait is what the *value* can do: compare, order, hash, display, clone. The two backends
//! each add exactly one hosting variant (`Payload::Extern` in the VM, `Value::Extern` in the
//! tree-walker) and delegate every behavior here — so a new native type never touches backend
//! code, and the differential holds by construction.
//!
//! Mutation and effects: the backends host extern values in shared cells (the VM's RC'd heap
//! payload, the tree-walker's `Rc<RefCell<…>>`), so a mutating method has **reference
//! semantics** — exactly like `FileHandle`, the type this contract generalizes. Effects reach
//! the world only through the `&mut dyn Host` the method dispatch hands the implementation.
//!
//! Lifecycle: freeing an extern value is a plain Rust drop (the GC cannot reach the Host at
//! free time). Self-contained RAII inside the value works by construction; Host-coupled
//! finalizers are out of scope — types that buffer effects keep `FileHandle`'s explicit
//! `close()` discipline.

use std::any::Any;
use std::cmp::Ordering;
use std::fmt;

/// The behavior contract of a registered extern type's values. `Send` because results may cross
/// the real executor's runtime (the async `ExternIo` seam); `'static` (implied by `Any`) because
/// values live on the heap beyond any borrow.
pub trait ExternValue: fmt::Debug + Send {
    /// The value's **qualified type identity** — `"{namespace}.{name}"`, exactly
    /// [`crate::registry::NominalType::qualified`] of the `ExtType` this value belongs to
    /// (`"std.id.Uuid"`). Drives `type_of`, `is`/`.as<T>()` narrowing, and method-table lookup;
    /// the runtime compares it by pointer/content, so return one pre-joined `&'static` literal —
    /// never a formatted string. Two extensions may register the same *short* name under distinct
    /// namespaces; this identity is what keeps their values distinct at runtime. Human-facing
    /// surfaces (diagnostics, `Value::type_name`) display its short form via
    /// `noeta_ast::short_type_name`.
    fn type_identity(&self) -> &'static str;

    /// The human-facing **short name** of this value's type — the segment after the final `.`
    /// of [`ExternValue::type_identity`] (`"std.id.Uuid"` → `"Uuid"`). The same display rule as
    /// `noeta_ast::short_type_name` (restated here because the dep-free ABI crate sits below the
    /// AST): diagnostics and `type_of` stringification show this; identity comparisons never do.
    fn type_display_name(&self) -> &'static str {
        let identity = self.type_identity();
        identity
            .rsplit_once('.')
            .map_or(identity, |(_, short)| short)
    }

    /// Value equality. Called with an arbitrary extern value: downcast via [`Self::as_any`] and
    /// return `false` on a kind mismatch. Content vs identity semantics are the implementation's
    /// call per kind (`Uuid` compares bytes; `FileHandle` compares its full shared state).
    fn eq_value(&self, other: &dyn ExternValue) -> bool;

    /// Value ordering — `None` for an unordered kind (or a kind mismatch, which a checked
    /// program never produces). A `key_capable` type MUST return a total order over its kind:
    /// it drives set canonicalization and sorted map display.
    fn cmp_value(&self, other: &dyn ExternValue) -> Option<Ordering>;

    /// A stable, content-derived hash. Meaningful only for `key_capable` types (map keys);
    /// others may return 0.
    fn hash_value(&self) -> u64;

    /// The `echo`/interpolation form, written into `out`.
    fn display(&self, out: &mut dyn fmt::Write) -> fmt::Result;

    /// Clone the value (GC promotion, argument marshalling). For plain-data types this is a
    /// derived `Clone`; the semantics mirror what payload cloning already means in each backend.
    fn clone_box(&self) -> Box<dyn ExternValue>;

    /// Downcast support for method dispatch and `eq_value`/`cmp_value` implementations.
    fn as_any(&self) -> &dyn Any;

    /// Mutable downcast support — the receiver of a mutating method.
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl dyn ExternValue {
    /// The display form as an owned string (backend `type_name`/`repr` glue).
    pub fn display_string(&self) -> String {
        let mut s = String::new();
        let _ = self.display(&mut s);
        s
    }
}

/// An owned extern value with the standard traits forwarded to the contract, so it can sit
/// inside derive-happy enums ([`crate::registry::NativeOut`], [`crate::registry::NativeValue`])
/// and backend payloads without manual impls at every site.
#[derive(Debug)]
pub struct ExternBox(pub Box<dyn ExternValue>);

impl ExternBox {
    /// Box an extern value.
    pub fn new(value: impl ExternValue + 'static) -> ExternBox {
        ExternBox(Box::new(value))
    }
}

impl Clone for ExternBox {
    fn clone(&self) -> ExternBox {
        ExternBox(self.0.clone_box())
    }
}

impl PartialEq for ExternBox {
    fn eq(&self, other: &ExternBox) -> bool {
        self.0.eq_value(&*other.0)
    }
}

impl std::ops::Deref for ExternBox {
    type Target = dyn ExternValue;
    fn deref(&self) -> &(dyn ExternValue + 'static) {
        &*self.0
    }
}

impl std::ops::DerefMut for ExternBox {
    fn deref_mut(&mut self) -> &mut (dyn ExternValue + 'static) {
        &mut *self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal extern value for contract-level tests (not registered anywhere).
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    struct Token(u32);

    impl ExternValue for Token {
        fn type_identity(&self) -> &'static str {
            "test.tokens.Token"
        }
        fn eq_value(&self, other: &dyn ExternValue) -> bool {
            other.as_any().downcast_ref::<Token>() == Some(self)
        }
        fn cmp_value(&self, other: &dyn ExternValue) -> Option<Ordering> {
            other
                .as_any()
                .downcast_ref::<Token>()
                .map(|o| self.0.cmp(&o.0))
        }
        fn hash_value(&self) -> u64 {
            u64::from(self.0)
        }
        fn display(&self, out: &mut dyn fmt::Write) -> fmt::Result {
            write!(out, "token#{}", self.0)
        }
        fn clone_box(&self) -> Box<dyn ExternValue> {
            Box::new(self.clone())
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
    }

    /// A second kind, to pin the cross-kind contract (`eq_value` false, `cmp_value` None).
    #[derive(Debug, Clone)]
    struct Other;

    impl ExternValue for Other {
        fn type_identity(&self) -> &'static str {
            "test.tokens.Other"
        }
        fn eq_value(&self, other: &dyn ExternValue) -> bool {
            other.as_any().downcast_ref::<Other>().is_some()
        }
        fn cmp_value(&self, other: &dyn ExternValue) -> Option<Ordering> {
            other
                .as_any()
                .downcast_ref::<Other>()
                .map(|_| Ordering::Equal)
        }
        fn hash_value(&self) -> u64 {
            0
        }
        fn display(&self, out: &mut dyn fmt::Write) -> fmt::Result {
            write!(out, "other")
        }
        fn clone_box(&self) -> Box<dyn ExternValue> {
            Box::new(self.clone())
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
    }

    #[test]
    fn extern_box_forwards_eq_clone_and_display() {
        let a = ExternBox::new(Token(7));
        let b = a.clone();
        assert_eq!(a, b);
        assert_ne!(a, ExternBox::new(Token(8)));
        assert_eq!(a.display_string(), "token#7");
        assert_eq!(a.type_identity(), "test.tokens.Token");
    }

    #[test]
    fn cross_kind_comparisons_are_false_and_unordered() {
        let token = ExternBox::new(Token(1));
        let other = ExternBox::new(Other);
        assert_ne!(token, other);
        assert_eq!(token.cmp_value(&*other), None);
        assert_eq!(
            token.cmp_value(&*ExternBox::new(Token(2))),
            Some(Ordering::Less)
        );
    }
}
