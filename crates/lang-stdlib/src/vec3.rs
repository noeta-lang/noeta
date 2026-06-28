//! The `vec` module's pure 3D-vector math (P-PACK Phase 4), over `[f32; 3]`.
//!
//! A "Vec3" in the surface is any struct value with exactly three `f32` fields (structural — the
//! user names the type, e.g. `@packed struct Vec3 { x: f32; y: f32; z: f32 }`). Each backend extracts
//! the three components into a `[f32; 3]`, calls one of these functions, and rebuilds a same-shape
//! result — so the arithmetic lives here **once** and both backends compute bit-identically (the
//! differential oracle holds by construction). All math is at `f32` precision, matching the fields.

/// Component-wise sum.
pub fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

/// Component-wise difference.
pub fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// Scale each component by `s`.
pub fn scale(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

/// Dot product `a · b`.
pub fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Cross product `a × b`.
pub fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Euclidean length `√(a · a)`.
pub fn length(a: [f32; 3]) -> f32 {
    dot(a, a).sqrt()
}

/// Unit vector in `a`'s direction. A zero vector has no direction, so it normalizes to the zero
/// vector (rather than `NaN`) — a deterministic, total convention shared by both backends.
pub fn normalize(a: [f32; 3]) -> [f32; 3] {
    let len = length(a);
    if len == 0.0 {
        [0.0, 0.0, 0.0]
    } else {
        scale(a, 1.0 / len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ops_match_hand_computation() {
        assert_eq!(add([1.0, 2.0, 3.0], [4.0, 5.0, 6.0]), [5.0, 7.0, 9.0]);
        assert_eq!(sub([4.0, 5.0, 6.0], [1.0, 2.0, 3.0]), [3.0, 3.0, 3.0]);
        assert_eq!(scale([1.0, 2.0, 3.0], 2.0), [2.0, 4.0, 6.0]);
        assert_eq!(dot([1.0, 2.0, 3.0], [4.0, 5.0, 6.0]), 32.0);
        assert_eq!(cross([1.0, 0.0, 0.0], [0.0, 1.0, 0.0]), [0.0, 0.0, 1.0]);
        assert_eq!(length([3.0, 4.0, 0.0]), 5.0);
        assert_eq!(normalize([3.0, 4.0, 0.0]), [0.6, 0.8, 0.0]);
        assert_eq!(normalize([0.0, 0.0, 0.0]), [0.0, 0.0, 0.0]);
    }
}
