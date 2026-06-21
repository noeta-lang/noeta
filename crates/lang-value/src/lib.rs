//! The M1 runtime value: a NaN-boxed 64-bit word.
//!
//! Every value is one `u64`. Doubles are stored as their own bit pattern; everything else
//! lives in the unused encoding space of a quiet NaN. The scheme (a refinement of the
//! classic Lox/Wren tagging):
//!
//! ```text
//!   float        : any bits where (bits & QNAN) != QNAN          (NaN is canonicalized)
//!   pointer      : SIGN | QNAN | addr48                          (heap object, refcounted)
//!   small int    : QNAN | INT_TAG | payload48 (sign-extended)    (immediate, ±2^47)
//!   unit/bool    : QNAN | tag                                    (immediate singletons)
//! ```
//!
//! `i64` magnitudes beyond the 48-bit immediate range are boxed on the heap so full i64
//! wrapping semantics survive — only storage differs, never arithmetic, which always runs
//! in `i64`. This is the representation the VM (`lang-vm`) operates on through the safe
//! API here; all `unsafe` is quarantined to the [`heap`] module.
//!
//! Why a separate value type from the M0 tree-walker's `Value` enum? An `Rc<T>` cannot
//! live in a NaN-box pointer slot, so the two backends keep different value models and are
//! only ever compared on observable output (`RunResult`), never on representation.

mod heap;
mod ops;

pub use ops::{OpError, apply_binary, apply_unary};

use heap::Payload;

/// A NaN-boxed runtime value (one 64-bit word). `Copy`: it is just an integer; ownership of
/// any heap object it points at is tracked by refcount, not by Rust's move semantics.
#[derive(Clone, Copy)]
pub struct Value(pub(crate) u64);

impl Value {
    // --- NaN-box layout constants ---
    /// Quiet-NaN prefix (exponent all ones + the two top mantissa bits). A word is a tagged
    /// (non-float) value iff all these bits are set.
    pub(crate) const QNAN: u64 = 0x7ffc_0000_0000_0000;
    /// Sign bit; set on pointers to distinguish them from immediate tagged values.
    pub(crate) const SIGN_BIT: u64 = 0x8000_0000_0000_0000;
    /// Low 48 bits — the heap address payload (canonical user-space pointers fit).
    pub(crate) const PTR_MASK: u64 = 0x0000_ffff_ffff_ffff;
    /// Discriminates an immediate small int from the unit/bool singletons (a free QNAN bit).
    const INT_TAG: u64 = 1 << 49;
    /// Low-bit tags for the immediate singletons.
    const TAG_UNIT: u64 = 0;
    const TAG_FALSE: u64 = 1;
    const TAG_TRUE: u64 = 2;
    /// Largest immediate small-int magnitude (48-bit signed payload).
    const INT_MIN: i64 = -(1 << 47);
    const INT_MAX: i64 = (1 << 47) - 1;

    // --- Constructors ---

    /// The unit value.
    pub fn unit() -> Value {
        Value(Self::QNAN | Self::TAG_UNIT)
    }

    pub fn bool(b: bool) -> Value {
        Value(Self::QNAN | if b { Self::TAG_TRUE } else { Self::TAG_FALSE })
    }

    /// A float. Any NaN is canonicalized to the standard quiet NaN so it can never collide
    /// with the tag space (canonical NaN has bit 50 clear; the tag prefix needs it set).
    pub fn float(f: f64) -> Value {
        if f.is_nan() {
            Value(0x7ff8_0000_0000_0000)
        } else {
            Value(f.to_bits())
        }
    }

    /// An integer: immediate when it fits the 48-bit range, boxed otherwise. Either way the
    /// value round-trips through [`Value::as_int`] as a full `i64`.
    pub fn int(i: i64) -> Value {
        if (Self::INT_MIN..=Self::INT_MAX).contains(&i) {
            Value(Self::QNAN | Self::INT_TAG | (i as u64 & Self::PTR_MASK))
        } else {
            heap::alloc(Payload::Int(i))
        }
    }

    /// A heap string (refcount 1).
    pub fn string(s: &str) -> Value {
        heap::alloc(Payload::Str(s.to_string()))
    }

    /// A heap closure (refcount 1) referencing function prototype `proto` in the module's
    /// proto table. M1.2 closures capture only globals (read live), so there are no upvalues.
    pub fn closure(proto: u32) -> Value {
        heap::alloc(Payload::Closure(proto))
    }

    // --- Classification ---

    fn is_float(self) -> bool {
        (self.0 & Self::QNAN) != Self::QNAN
    }

    pub fn is_pointer(self) -> bool {
        (self.0 & (Self::SIGN_BIT | Self::QNAN)) == (Self::SIGN_BIT | Self::QNAN)
    }

    fn is_small_int(self) -> bool {
        !self.is_float() && !self.is_pointer() && (self.0 & Self::INT_TAG) != 0
    }

    /// Whether this is the unit value.
    pub fn is_unit(self) -> bool {
        self.0 == Value::unit().0
    }

    /// The boolean payload, if this is `true`/`false`.
    pub fn as_bool(self) -> Option<bool> {
        if self.0 == Value::bool(true).0 {
            Some(true)
        } else if self.0 == Value::bool(false).0 {
            Some(false)
        } else {
            None
        }
    }

    /// The integer value, reading either an immediate small int or a boxed `i64`.
    pub fn as_int(self) -> Option<i64> {
        if self.is_small_int() {
            let p = self.0 & Self::PTR_MASK;
            // Sign-extend the 48-bit payload to a full i64.
            Some(((p << 16) as i64) >> 16)
        } else if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Int(i) => Some(*i),
                _ => None,
            })
        } else {
            None
        }
    }

    /// The float value, if this is a float.
    pub fn as_float(self) -> Option<f64> {
        if self.is_float() {
            Some(f64::from_bits(self.0))
        } else {
            None
        }
    }

    /// A clone of the string value, if this is a heap string.
    pub fn as_string(self) -> Option<String> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Str(s) => Some(s.clone()),
                _ => None,
            })
        } else {
            None
        }
    }

    /// The function-prototype index, if this is a closure.
    pub fn as_closure(self) -> Option<u32> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Closure(proto) => Some(*proto),
                _ => None,
            })
        } else {
            None
        }
    }

    // --- Display (mirrors the M0 tree-walker's `Value::display`) ---

    /// The display form used by `echo` and `~` concatenation.
    pub fn display(self) -> String {
        if let Some(b) = self.as_bool() {
            b.to_string()
        } else if self.is_small_int() {
            self.as_int().unwrap().to_string()
        } else if self.is_float() {
            format_float(self.as_float().unwrap())
        } else if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Str(s) => s.clone(),
                Payload::Int(i) => i.to_string(),
                // Mirrors the M0 tree-walker's `Value::Function(_) => "<fn>"`.
                Payload::Closure(_) => "<fn>".to_string(),
            })
        } else {
            // The unit value (and any other singleton) displays as empty, as in M0.
            String::new()
        }
    }

    /// The user-facing type name, for diagnostics (mirrors M0's `Value::type_name`).
    pub fn type_name(self) -> &'static str {
        if self.as_bool().is_some() {
            "bool"
        } else if self.as_int().is_some() {
            "int"
        } else if self.is_float() {
            "float"
        } else if self.is_pointer() {
            // Boxed ints were already caught by `as_int` above, so a pointer here is either a
            // string or a closure. M0 names both user functions and builtins "function".
            if self.as_closure().is_some() {
                "function"
            } else {
                "string"
            }
        } else {
            "unit"
        }
    }

    // --- Refcount management (the GC policy layer lives in `lang-gc`) ---

    /// Increment the refcount (no-op for immediates).
    pub fn inc_ref(self) {
        if self.is_pointer() {
            heap::inc_ref(self);
        }
    }

    /// Decrement the refcount; return `true` if it reached zero and the value should be
    /// [`free`](Value::free)d. No-op (`false`) for immediates.
    pub fn dec_ref(self) -> bool {
        if self.is_pointer() {
            heap::dec_ref(self)
        } else {
            false
        }
    }

    /// Free a heap value whose refcount has reached zero. Must only follow a `dec_ref`
    /// that returned `true`.
    pub fn free(self) {
        if self.is_pointer() {
            heap::free(self);
        }
    }
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Show the logical value, not the raw word — but stay shallow and allocation-free
        // where possible.
        if let Some(b) = self.as_bool() {
            write!(f, "Bool({b})")
        } else if let Some(i) = self.as_int() {
            write!(f, "Int({i})")
        } else if let Some(x) = self.as_float() {
            write!(f, "Float({x})")
        } else if let Some(proto) = self.as_closure() {
            write!(f, "Closure(proto={proto})")
        } else if self.is_pointer() {
            write!(f, "Str({:?})", self.as_string().unwrap_or_default())
        } else {
            write!(f, "Unit")
        }
    }
}

/// Render a float deterministically: whole-valued floats keep a trailing `.0` so they are
/// visibly distinct from ints (mirrors the M0 tree-walker exactly).
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
    use proptest::prelude::*;

    #[test]
    fn immediates_round_trip() {
        assert_eq!(Value::unit().display(), "");
        assert_eq!(Value::bool(true).as_bool(), Some(true));
        assert_eq!(Value::bool(false).as_bool(), Some(false));
        assert_eq!(Value::int(42).as_int(), Some(42));
        assert_eq!(Value::int(-42).as_int(), Some(-42));
        assert_eq!(Value::float(2.5).as_float(), Some(2.5));
    }

    #[test]
    fn big_ints_box_and_keep_full_i64() {
        for i in [
            i64::MAX,
            i64::MIN,
            1 << 50,
            -(1 << 50),
            1 << 47,
            -(1 << 47) - 1,
        ] {
            let v = Value::int(i);
            assert_eq!(v.as_int(), Some(i), "round-trip {i}");
            // Each is boxed (outside the immediate range); free it so miri sees no leak.
            assert!(v.is_pointer(), "{i} should box");
            assert!(v.dec_ref());
            v.free();
        }
    }

    #[test]
    fn small_int_boundaries_stay_immediate() {
        assert!(!Value::int(Value::INT_MAX).is_pointer());
        assert!(!Value::int(Value::INT_MIN).is_pointer());
        // Just outside the immediate range, integers box (and must be freed for miri).
        for i in [Value::INT_MAX + 1, Value::INT_MIN - 1] {
            let v = Value::int(i);
            assert!(v.is_pointer());
            assert!(v.dec_ref());
            v.free();
        }
    }

    #[test]
    fn nan_is_canonicalized_and_classified_as_float() {
        let v = Value::float(f64::NAN);
        assert!(v.as_float().unwrap().is_nan());
        assert_eq!(v.type_name(), "float");
        assert!(!v.is_pointer());
    }

    #[test]
    fn strings_round_trip_and_free() {
        let v = Value::string("héllo");
        assert_eq!(v.as_string().as_deref(), Some("héllo"));
        assert_eq!(v.display(), "héllo");
        assert_eq!(v.type_name(), "string");
        assert!(v.dec_ref());
        v.free();
    }

    #[test]
    fn closures_round_trip_and_free() {
        let v = Value::closure(7);
        assert_eq!(v.as_closure(), Some(7));
        assert_eq!(v.type_name(), "function");
        assert_eq!(v.display(), "<fn>");
        // A closure is not an int/string/bool, so it never compares "equal" numerically.
        assert_eq!(v.as_int(), None);
        assert_eq!(v.as_string(), None);
        assert!(v.dec_ref());
        v.free();
    }

    #[test]
    fn refcount_keeps_object_alive() {
        let v = Value::string("x");
        v.inc_ref(); // count 2
        assert!(!v.dec_ref()); // count 1, not freed
        assert_eq!(v.as_string().as_deref(), Some("x"));
        assert!(v.dec_ref()); // count 0
        v.free();
    }

    proptest! {
        #[test]
        fn float_round_trips(bits in any::<u64>()) {
            let f = f64::from_bits(bits);
            let v = Value::float(f);
            if f.is_nan() {
                prop_assert!(v.as_float().unwrap().is_nan());
            } else {
                prop_assert_eq!(v.as_float(), Some(f));
            }
            prop_assert!(!v.is_pointer());
        }

        #[test]
        fn int_round_trips(i in any::<i64>()) {
            let v = Value::int(i);
            prop_assert_eq!(v.as_int(), Some(i));
            if v.dec_ref() { v.free(); }
        }
    }
}
