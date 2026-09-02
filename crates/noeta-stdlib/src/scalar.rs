//! The `Scalar` element trait — one source of truth for per-element-type numeric behaviour.
//!
//! The numeric kernels (list reductions in [`crate::reductions`], list element-wise ops in
//! [`crate::bulk`], and the vector bundles) would otherwise each re-express the same handful of
//! operations per width via local macros: `read_le`/`chunks_exact`/wrapping arithmetic, ten
//! near-identical copies apiece. That duplication is a coverage gap, because adding a width means
//! editing every file, and a width nobody edits in reaches no surface at all.
//!
//! This trait is a single impl per numeric type instead. Each consumer becomes a thin generic
//! `fn f<S: Scalar>(..)` body that the compiler monomorphizes once per width — the loops stay
//! `chunks_exact`-shaped so LLVM autovectorizes each monomorphization (this is *not* explicit
//! `std::simd`). Adding a width becomes one trait impl that lights
//! up every surface at once; "which types are covered" becomes "which types implement `Scalar`".
//!
//! ## Settled semantics
//! - **Default arithmetic wraps** for integers (matches scalar `+`, the reductions and element-wise
//!   ops) and is **IEEE** for floats. [`Scalar::sat_add`]/[`Scalar::sat_sub`] are the opt-in
//!   saturating mode — meaningful for integers, `== add`/`sub` for floats.
//! - **[`Scalar::Wide`]** is the dot/accumulator type: integers widen to `i64`/`u64` (a dot or a
//!   widened sum must not wrap silently); `f32`/`f64` stay themselves.
//! - **[`Scalar::Float`]** is the length/`sqrt` result: integers promote to `f64`; `f32` stays `f32`;
//!   `f64` stays `f64`.
//! - Signed vs unsigned differ in `read_le` extension, `min`/`max`, saturation bounds and `neg` —
//!   all captured in the impl, invisible to the generic bodies.
//!
//! ## Three members the consumers need
//! Three members exist for the generic bodies rather than for the arithmetic itself:
//! - [`Scalar::ZERO`] / [`Scalar::ONE`] — the additive / multiplicative identities. `sum`/`product`
//!   accumulate in `S` (wrapping), and an *empty* list must fold to the identity (`0` / `1`); you
//!   cannot seed a generic accumulator without them.
//! - [`Scalar::checked_add`] — the opt-in `checked_sum` reports overflow at the element width instead
//!   of wrapping. Floats never overflow, so their `checked_add` is total (`Some(self + o)`).

/// One numeric element type. Implemented for all ten primitives (`i8 i16 i32 i64 u8 u16 u32 u64
/// f32 f64`); every numeric kernel is generic over this trait so a width is described exactly once.
pub trait Scalar: Copy {
    /// Packed width in bytes (`1`, `2`, `4`, or `8`) — the `chunks_exact` stride.
    const BYTES: usize;
    /// Additive identity (`0` / `0.0`) — the `sum` accumulator seed and empty-list fold.
    const ZERO: Self;
    /// Multiplicative identity (`1` / `1.0`) — the `product` accumulator seed and empty-list fold.
    const ONE: Self;

    /// Accumulator for dot/widened sums: `iN` → `i64`, `uN` → `u64`, `f32` → `f32`, `f64` → `f64`.
    type Wide: Copy;
    /// Length/`sqrt` result: integers → `f64`, `f32` → `f32`, `f64` → `f64`.
    type Float: Copy;

    /// Decode one element from the first [`Self::BYTES`] of `bytes`, little-endian, sign- or
    /// zero-extending per the type. `bytes` must be at least `BYTES` long (a `chunks_exact` chunk).
    fn read_le(bytes: &[u8]) -> Self;
    /// Append this element as [`Self::BYTES`] little-endian bytes to `out`.
    fn write_le(self, out: &mut Vec<u8>);

    /// Default arithmetic: **wrapping** for integers, IEEE for floats.
    fn add(self, o: Self) -> Self;
    fn sub(self, o: Self) -> Self;
    fn mul(self, o: Self) -> Self;

    /// `checked_add`: `None` on integer overflow at the element width; floats never report overflow.
    fn checked_add(self, o: Self) -> Option<Self>;

    /// Saturating mode: integers clamp at the type's bounds; floats fall back to plain `add`/`sub`.
    fn sat_add(self, o: Self) -> Self;
    fn sat_sub(self, o: Self) -> Self;

    /// Total-order `min`/`max`. For floats this is the inherent `f32::min`/`f64::max` (returns the
    /// non-NaN operand when one side is NaN), matching the existing reduction/vector fold policy.
    fn min(self, o: Self) -> Self;
    fn max(self, o: Self) -> Self;

    /// Absolute value: signed integers **wrap** (`i32::MIN.abs() == i32::MIN`), unsigned is the
    /// identity, floats use `f32::abs`.
    fn abs(self) -> Self;
    /// Negation: integers **wrap** (two's-complement, so unsigned `neg` is a wrapping negate),
    /// floats use unary `-`.
    fn neg(self) -> Self;

    /// Widen then multiply, into [`Self::Wide`] — the per-lane step of a dot product that must not
    /// wrap at the narrow width. Floats multiply in place.
    fn widen_mul(self, o: Self) -> Self::Wide;
    /// Accumulate two wide values (wrapping for integers, IEEE for floats).
    fn wide_add(a: Self::Wide, b: Self::Wide) -> Self::Wide;

    /// Promote one element to its [`Self::Float`] (for `length`/`normalize`).
    fn to_float(self) -> Self::Float;

    /// Round a [`Self::Float`] back to an element — the closing step of a kernel that computes in the
    /// promoted domain (`lerp`). Integers round to nearest, half away from zero, and **saturate** at
    /// the width's bounds; `f32`/`f64` are already their own `Float`, so this is the identity.
    fn from_float(f: Self::Float) -> Self;

    /// A plain `f64` as a [`Self::Float`] — how a language `float` argument (an interpolation
    /// parameter) enters the promoted domain. `f32` narrows; the integer widths keep `f64`.
    fn float_from_f64(f: f64) -> Self::Float;

    /// Arithmetic in the promoted domain. A kernel that must not wrap at the element width — a
    /// distance between two `u8` components, an interpolation between two `i32` ones — computes here
    /// and closes with [`Self::from_float`]. `f32` stays `f32`, so a float vector's result is the one
    /// its own precision gives, not a rounded `f64`.
    fn float_add(a: Self::Float, b: Self::Float) -> Self::Float;
    fn float_sub(a: Self::Float, b: Self::Float) -> Self::Float;
    fn float_mul(a: Self::Float, b: Self::Float) -> Self::Float;

    /// Promote a **widened accumulator** ([`Self::Wide`]) to a float ([`Self::Float`]) — the missing
    /// rung `length` needs: a vector length is `sqrt` of the *widened*
    /// dot accumulator, but [`Self::to_float`] promotes a bare element, not a `Wide`. Integers widen
    /// `i64`/`u64` → `f64`; `f32`/`f64` are already their own `Wide == Float`.
    fn wide_to_float(w: Self::Wide) -> Self::Float;

    /// The square root of a [`Self::Float`] — the last step of `length`. `f64::sqrt` for the integer
    /// widths (whose `Float` is `f64`), the inherent `sqrt` for `f32`/`f64`.
    fn sqrt(f: Self::Float) -> Self::Float;

    /// Build an element from an `f64`, **saturating** at the type's bounds — the saturating-scale
    /// step (`vec.SatKernels`: `Color.scale(2.0)` clamps a channel to `[0, 255]`). Rounds to nearest,
    /// then a saturating `as` cast (float→int casts saturate). A float element casts plainly (a
    /// saturating bundle never binds a float, so this arm is only for trait completeness).
    fn saturating_from_f64(f: f64) -> Self;

    /// This element widened to a plain `f64` — the input to the saturating float-scale math
    /// (`v.to_f64() * factor`), where the factor is a language `float`. Distinct from [`Self::to_float`]
    /// (which returns the *associated* `Float`, `f32` for an `f32` element) so the arithmetic is
    /// unambiguously `f64` regardless of the element width.
    fn to_f64(self) -> f64;
}

/// Implement `Scalar` for a **signed** integer type (`Wide = i64`, `Float = f64`).
macro_rules! impl_signed_scalar {
    ($ty:ty, $bytes:literal) => {
        impl Scalar for $ty {
            const BYTES: usize = $bytes;
            const ZERO: Self = 0;
            const ONE: Self = 1;
            type Wide = i64;
            type Float = f64;

            #[inline]
            fn read_le(bytes: &[u8]) -> Self {
                <$ty>::from_le_bytes(bytes[..$bytes].try_into().expect("read_le width"))
            }
            #[inline]
            fn write_le(self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }
            #[inline]
            fn add(self, o: Self) -> Self {
                self.wrapping_add(o)
            }
            #[inline]
            fn sub(self, o: Self) -> Self {
                self.wrapping_sub(o)
            }
            #[inline]
            fn mul(self, o: Self) -> Self {
                self.wrapping_mul(o)
            }
            #[inline]
            fn checked_add(self, o: Self) -> Option<Self> {
                <$ty>::checked_add(self, o)
            }
            #[inline]
            fn sat_add(self, o: Self) -> Self {
                self.saturating_add(o)
            }
            #[inline]
            fn sat_sub(self, o: Self) -> Self {
                self.saturating_sub(o)
            }
            #[inline]
            fn min(self, o: Self) -> Self {
                if self <= o { self } else { o }
            }
            #[inline]
            fn max(self, o: Self) -> Self {
                if self >= o { self } else { o }
            }
            #[inline]
            fn abs(self) -> Self {
                self.wrapping_abs()
            }
            #[inline]
            fn neg(self) -> Self {
                self.wrapping_neg()
            }
            #[inline]
            fn widen_mul(self, o: Self) -> i64 {
                (self as i64).wrapping_mul(o as i64)
            }
            #[inline]
            fn wide_add(a: i64, b: i64) -> i64 {
                a.wrapping_add(b)
            }
            #[inline]
            fn to_float(self) -> f64 {
                self as f64
            }
            #[inline]
            fn from_float(f: f64) -> Self {
                <Self as Scalar>::saturating_from_f64(f)
            }
            #[inline]
            fn float_from_f64(f: f64) -> f64 {
                f
            }
            #[inline]
            fn float_add(a: f64, b: f64) -> f64 {
                a + b
            }
            #[inline]
            fn float_sub(a: f64, b: f64) -> f64 {
                a - b
            }
            #[inline]
            fn float_mul(a: f64, b: f64) -> f64 {
                a * b
            }
            #[inline]
            fn wide_to_float(w: i64) -> f64 {
                w as f64
            }
            #[inline]
            fn sqrt(f: f64) -> f64 {
                f.sqrt()
            }
            #[inline]
            fn saturating_from_f64(f: f64) -> Self {
                f.round() as $ty
            }
            #[inline]
            fn to_f64(self) -> f64 {
                self as f64
            }
        }
    };
}

/// Implement `Scalar` for an **unsigned** integer type (`Wide = u64`, `Float = f64`; `abs` identity).
macro_rules! impl_unsigned_scalar {
    ($ty:ty, $bytes:literal) => {
        impl Scalar for $ty {
            const BYTES: usize = $bytes;
            const ZERO: Self = 0;
            const ONE: Self = 1;
            type Wide = u64;
            type Float = f64;

            #[inline]
            fn read_le(bytes: &[u8]) -> Self {
                <$ty>::from_le_bytes(bytes[..$bytes].try_into().expect("read_le width"))
            }
            #[inline]
            fn write_le(self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }
            #[inline]
            fn add(self, o: Self) -> Self {
                self.wrapping_add(o)
            }
            #[inline]
            fn sub(self, o: Self) -> Self {
                self.wrapping_sub(o)
            }
            #[inline]
            fn mul(self, o: Self) -> Self {
                self.wrapping_mul(o)
            }
            #[inline]
            fn checked_add(self, o: Self) -> Option<Self> {
                <$ty>::checked_add(self, o)
            }
            #[inline]
            fn sat_add(self, o: Self) -> Self {
                self.saturating_add(o)
            }
            #[inline]
            fn sat_sub(self, o: Self) -> Self {
                self.saturating_sub(o)
            }
            #[inline]
            fn min(self, o: Self) -> Self {
                if self <= o { self } else { o }
            }
            #[inline]
            fn max(self, o: Self) -> Self {
                if self >= o { self } else { o }
            }
            /// Unsigned magnitude is the value itself.
            #[inline]
            fn abs(self) -> Self {
                self
            }
            /// Two's-complement negation at the width (`200u8.neg() == 56`).
            #[inline]
            fn neg(self) -> Self {
                self.wrapping_neg()
            }
            #[inline]
            fn widen_mul(self, o: Self) -> u64 {
                (self as u64).wrapping_mul(o as u64)
            }
            #[inline]
            fn wide_add(a: u64, b: u64) -> u64 {
                a.wrapping_add(b)
            }
            #[inline]
            fn to_float(self) -> f64 {
                self as f64
            }
            #[inline]
            fn from_float(f: f64) -> Self {
                <Self as Scalar>::saturating_from_f64(f)
            }
            #[inline]
            fn float_from_f64(f: f64) -> f64 {
                f
            }
            #[inline]
            fn float_add(a: f64, b: f64) -> f64 {
                a + b
            }
            #[inline]
            fn float_sub(a: f64, b: f64) -> f64 {
                a - b
            }
            #[inline]
            fn float_mul(a: f64, b: f64) -> f64 {
                a * b
            }
            #[inline]
            fn wide_to_float(w: u64) -> f64 {
                w as f64
            }
            #[inline]
            fn sqrt(f: f64) -> f64 {
                f.sqrt()
            }
            #[inline]
            fn saturating_from_f64(f: f64) -> Self {
                f.round() as $ty
            }
            #[inline]
            fn to_f64(self) -> f64 {
                self as f64
            }
        }
    };
}

/// Implement `Scalar` for a float type (`Wide = Float = Self`; IEEE arithmetic, saturating == plain).
macro_rules! impl_float_scalar {
    ($ty:ty, $bytes:literal) => {
        impl Scalar for $ty {
            const BYTES: usize = $bytes;
            const ZERO: Self = 0.0;
            const ONE: Self = 1.0;
            type Wide = $ty;
            type Float = $ty;

            #[inline]
            fn read_le(bytes: &[u8]) -> Self {
                <$ty>::from_le_bytes(bytes[..$bytes].try_into().expect("read_le width"))
            }
            #[inline]
            fn write_le(self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }
            #[inline]
            fn add(self, o: Self) -> Self {
                self + o
            }
            #[inline]
            fn sub(self, o: Self) -> Self {
                self - o
            }
            #[inline]
            fn mul(self, o: Self) -> Self {
                self * o
            }
            /// Floats never report overflow — an IEEE sum saturates to `±inf`, not an error.
            #[inline]
            fn checked_add(self, o: Self) -> Option<Self> {
                Some(self + o)
            }
            /// Saturating mode is meaningful only for integers; a float saturates in the IEEE sense.
            #[inline]
            fn sat_add(self, o: Self) -> Self {
                self + o
            }
            #[inline]
            fn sat_sub(self, o: Self) -> Self {
                self - o
            }
            /// The inherent `min`/`max`: returns the non-NaN operand when one side is NaN, so a fold
            /// is deterministic across backends.
            #[inline]
            fn min(self, o: Self) -> Self {
                <$ty>::min(self, o)
            }
            #[inline]
            fn max(self, o: Self) -> Self {
                <$ty>::max(self, o)
            }
            #[inline]
            fn abs(self) -> Self {
                <$ty>::abs(self)
            }
            #[inline]
            fn neg(self) -> Self {
                -self
            }
            #[inline]
            fn widen_mul(self, o: Self) -> $ty {
                self * o
            }
            #[inline]
            fn wide_add(a: $ty, b: $ty) -> $ty {
                a + b
            }
            #[inline]
            fn to_float(self) -> $ty {
                self
            }
            /// `Float == Self` for a float element, so closing the promoted domain is the identity —
            /// an `f32` vector's interpolation is computed and returned at `f32`.
            #[inline]
            fn from_float(f: $ty) -> Self {
                f
            }
            #[inline]
            fn float_from_f64(f: f64) -> $ty {
                f as $ty
            }
            #[inline]
            fn float_add(a: $ty, b: $ty) -> $ty {
                a + b
            }
            #[inline]
            fn float_sub(a: $ty, b: $ty) -> $ty {
                a - b
            }
            #[inline]
            fn float_mul(a: $ty, b: $ty) -> $ty {
                a * b
            }
            #[inline]
            fn wide_to_float(w: $ty) -> $ty {
                w
            }
            #[inline]
            fn sqrt(f: $ty) -> $ty {
                f.sqrt()
            }
            #[inline]
            fn saturating_from_f64(f: f64) -> Self {
                f as $ty
            }
            #[inline]
            fn to_f64(self) -> f64 {
                self as f64
            }
        }
    };
}

impl_signed_scalar!(i8, 1);
impl_signed_scalar!(i16, 2);
impl_signed_scalar!(i32, 4);
impl_signed_scalar!(i64, 8);
impl_unsigned_scalar!(u8, 1);
impl_unsigned_scalar!(u16, 2);
impl_unsigned_scalar!(u32, 4);
impl_unsigned_scalar!(u64, 8);
impl_float_scalar!(f32, 4);
impl_float_scalar!(f64, 8);

#[cfg(test)]
mod tests {
    use super::*;

    /// `read_le`/`write_le` round-trip for every width, and that `write_le` emits exactly `BYTES`.
    #[test]
    fn read_write_round_trip() {
        fn round<S: Scalar + PartialEq + std::fmt::Debug>(v: S) {
            let mut out = Vec::new();
            v.write_le(&mut out);
            assert_eq!(out.len(), S::BYTES, "write_le width");
            assert_eq!(S::read_le(&out), v, "round-trip");
        }
        round(-5i8);
        round(-1234i16);
        round(-70000i32);
        round(-5_000_000_000i64);
        round(200u8);
        round(60000u16);
        round(4_000_000_000u32);
        round(18_000_000_000_000_000_000u64);
        round(1.5f32);
        round(-2.5f64);
    }

    /// Sign- vs zero-extension: `0xFF` decodes to `-1` as `i8` but `255` as `u8`.
    #[test]
    fn sign_vs_zero_extension() {
        assert_eq!(<i8 as Scalar>::read_le(&[0xFF]), -1i8);
        assert_eq!(<u8 as Scalar>::read_le(&[0xFF]), 255u8);
        assert_eq!(<i16 as Scalar>::read_le(&[0xFF, 0xFF]), -1i16);
        assert_eq!(<u16 as Scalar>::read_le(&[0xFF, 0xFF]), 65535u16);
    }

    /// Default arithmetic wraps at the element width for integers.
    #[test]
    fn integer_arithmetic_wraps() {
        assert_eq!(Scalar::add(i32::MAX, 1), i32::MIN);
        assert_eq!(Scalar::add(200u8, 100u8), 44u8); // 300 & 0xFF
        assert_eq!(Scalar::mul(i16::MAX, 2), -2i16);
        assert_eq!(Scalar::sub(0u8, 1u8), 255u8);
    }

    /// Saturating mode clamps integers at the type bounds; floats fall back to plain arithmetic.
    #[test]
    fn saturating_clamps() {
        assert_eq!(Scalar::sat_add(i32::MAX, 1), i32::MAX);
        assert_eq!(Scalar::sat_add(200u8, 100u8), 255u8);
        assert_eq!(Scalar::sat_sub(0u8, 1u8), 0u8);
        assert_eq!(Scalar::sat_sub(i8::MIN, 1), i8::MIN);
        // Float saturating == plain IEEE.
        assert_eq!(Scalar::sat_add(1.5f32, 2.0), 3.5f32);
    }

    /// `checked_add` reports overflow for integers and never for floats.
    #[test]
    fn checked_add_reports_overflow() {
        assert_eq!(Scalar::checked_add(1i32, 2), Some(3));
        assert_eq!(Scalar::checked_add(i32::MAX, 1), None);
        assert_eq!(Scalar::checked_add(255u8, 1), None);
        assert_eq!(Scalar::checked_add(1.0f64, 2.0), Some(3.0));
    }

    /// `widen_mul` computes in the wide type, so a product that overflows the narrow width survives.
    #[test]
    fn widen_mul_avoids_overflow() {
        // 46341² overflows i32 but not i64.
        let x = 46341i32;
        assert_eq!(x.widen_mul(x), 46341i64 * 46341);
        // u32 near its max: product overflows u32, fits u64.
        let y = 4_000_000_000u32;
        assert_eq!(y.widen_mul(2), 8_000_000_000u64);
        // Wide accumulation.
        assert_eq!(
            <i32 as Scalar>::wide_add(x.widen_mul(x), x.widen_mul(x)),
            2 * 46341i64 * 46341
        );
    }

    /// `to_float` promotes integers to `f64`; `f32`/`f64` stay themselves.
    #[test]
    fn to_float_promotion() {
        let i: f64 = 7i32.to_float();
        assert_eq!(i, 7.0f64);
        let u: f64 = 255u8.to_float();
        assert_eq!(u, 255.0f64);
        let f: f32 = 1.5f32.to_float();
        assert_eq!(f, 1.5f32);
        let d: f64 = 2.5f64.to_float();
        assert_eq!(d, 2.5f64);
    }

    /// `abs`/`neg` wrap for integers; unsigned `abs` is identity.
    #[test]
    fn abs_neg_conventions() {
        assert_eq!(Scalar::abs(i32::MIN), i32::MIN); // wraps
        assert_eq!(Scalar::neg(i32::MIN), i32::MIN); // wraps
        assert_eq!(Scalar::abs(200u8), 200u8); // identity
        assert_eq!(Scalar::neg(200u8), 56u8); // wrapping negate
        assert_eq!(Scalar::abs(-3.5f32), 3.5f32);
    }

    /// Float `min`/`max` return the non-NaN operand (total order for the fold).
    #[test]
    fn float_min_max_nan_policy() {
        assert_eq!(Scalar::min(f32::NAN, 1.0), 1.0f32);
        assert_eq!(Scalar::max(1.0f32, f32::NAN), 1.0f32);
        assert_eq!(Scalar::min(-1.0f64, 2.0), -1.0f64);
    }

    /// The promoted domain: integers compute at `f64` and round back half-away-from-zero with
    /// saturation, `f32` computes and closes at `f32` (no `f64` detour), `f64` is the identity.
    #[test]
    fn promoted_domain_round_trip() {
        // An integer element promotes, keeps the fraction through the arithmetic, and rounds back.
        let half = <i32 as Scalar>::float_from_f64(0.5);
        let lerped = <i32 as Scalar>::float_add(
            1i32.to_float(),
            <i32 as Scalar>::float_mul(
                <i32 as Scalar>::float_sub(4i32.to_float(), 1i32.to_float()),
                half,
            ),
        );
        assert_eq!(lerped, 2.5f64);
        assert_eq!(<i32 as Scalar>::from_float(lerped), 3i32); // half away from zero
        assert_eq!(<i32 as Scalar>::from_float(-2.5f64), -3i32);
        // Closing saturates at the width rather than wrapping.
        assert_eq!(<u8 as Scalar>::from_float(400.0), 255u8);
        assert_eq!(<i8 as Scalar>::from_float(-999.0), -128i8);
        // An unsigned difference computed in the promoted domain is signed, so it cannot wrap.
        assert_eq!(
            <u8 as Scalar>::float_sub(3u8.to_float(), 5u8.to_float()),
            -2.0f64
        );
        // A float element stays at its own precision end to end.
        let f: f32 = <f32 as Scalar>::float_from_f64(0.1);
        assert_eq!(f, 0.1f32);
        assert_eq!(<f32 as Scalar>::from_float(1.5f32), 1.5f32);
        assert_eq!(<f64 as Scalar>::from_float(1.5f64), 1.5f64);
    }

    /// Identities: seeding an empty fold gives `0` / `1`.
    #[test]
    fn identities() {
        assert_eq!(<i32 as Scalar>::ZERO, 0);
        assert_eq!(<i32 as Scalar>::ONE, 1);
        assert_eq!(<f64 as Scalar>::ZERO, 0.0);
        assert_eq!(<f32 as Scalar>::ONE, 1.0);
    }
}
