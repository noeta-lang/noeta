//! The `quat` module's pure quaternion math (P-PACK Phase 4 follow-on), over `[f32; 4]`.
//!
//! A "Quat" in the surface is any struct with exactly four `f32` fields, ordered `(x, y, z, w)` —
//! `x`/`y`/`z` the vector (imaginary) part and `w` the scalar (real) part, matching glm/Unity. As with
//! `vec`, the user names the type (`@packed struct Quat { x: f32; y: f32; z: f32; w: f32 }`); each
//! backend extracts the four components, calls one of these functions, and rebuilds a same-shape
//! result, so the arithmetic lives here once and both backends agree by construction.
//!
//! These are the *transform* operations (compose, conjugate, normalize, interpolate, apply to a
//! vector). Constructing a rotation quaternion from an axis + angle needs trig (`math.sin`/`cos`,
//! not yet present) and a result-shape source for a from-scratch value — deferred; build the initial
//! quaternion with a literal for now.

use crate::vec3;

/// Hamilton product `a ⊗ b` (rotation composition: applies `b` then `a`).
pub fn mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let [ax, ay, az, aw] = a;
    let [bx, by, bz, bw] = b;
    [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]
}

/// Conjugate `(−x, −y, −z, w)` — the inverse rotation of a unit quaternion.
pub fn conjugate(q: [f32; 4]) -> [f32; 4] {
    [-q[0], -q[1], -q[2], q[3]]
}

/// Dot product (the 4-component inner product).
pub fn dot(a: [f32; 4], b: [f32; 4]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3]
}

/// Magnitude `√(q · q)`.
pub fn length(q: [f32; 4]) -> f32 {
    dot(q, q).sqrt()
}

/// Unit quaternion. A zero quaternion normalizes to the zero quaternion (deterministic, no `NaN`),
/// matching `vec3::normalize`.
pub fn normalize(q: [f32; 4]) -> [f32; 4] {
    let len = length(q);
    if len == 0.0 {
        [0.0, 0.0, 0.0, 0.0]
    } else {
        let inv = 1.0 / len;
        [q[0] * inv, q[1] * inv, q[2] * inv, q[3] * inv]
    }
}

/// Spherical linear interpolation along the shortest arc from `a` to `b`. Falls back to normalized
/// linear interpolation when the quaternions are nearly parallel (where `sin θ → 0`), so it is total.
pub fn slerp(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    let mut b = b;
    let mut cos = dot(a, b);
    // Take the shortest path: q and −q are the same rotation.
    if cos < 0.0 {
        b = [-b[0], -b[1], -b[2], -b[3]];
        cos = -cos;
    }
    if cos > 0.9995 {
        let lin = [
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
            a[3] + (b[3] - a[3]) * t,
        ];
        return normalize(lin);
    }
    let theta0 = cos.clamp(-1.0, 1.0).acos();
    let sin0 = theta0.sin();
    let s1 = ((1.0 - t) * theta0).sin() / sin0;
    let s2 = (t * theta0).sin() / sin0;
    [
        s1 * a[0] + s2 * b[0],
        s1 * a[1] + s2 * b[1],
        s1 * a[2] + s2 * b[2],
        s1 * a[3] + s2 * b[3],
    ]
}

/// Rotate a 3-vector by the (unit) quaternion `q`: `v' = 2(u·v)u + (s²−u·u)v + 2s(u×v)`, where
/// `u = (x,y,z)` and `s = w`. (No normalization is applied; pass a unit quaternion for a pure
/// rotation.)
pub fn rotate_vec3(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let u = [q[0], q[1], q[2]];
    let s = q[3];
    let a = vec3::scale(u, 2.0 * vec3::dot(u, v));
    let b = vec3::scale(v, s * s - vec3::dot(u, u));
    let c = vec3::scale(vec3::cross(u, v), 2.0 * s);
    vec3::add(vec3::add(a, b), c)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

    #[test]
    fn quat_ops() {
        // Identity is a multiplicative unit.
        let q = [0.1, 0.2, 0.3, 0.9];
        assert_eq!(mul(IDENTITY, q), q);
        assert_eq!(mul(q, IDENTITY), q);
        // Conjugate flips the vector part.
        assert_eq!(conjugate(q), [-0.1, -0.2, -0.3, 0.9]);
        assert_eq!(length(IDENTITY), 1.0);
        assert_eq!(normalize([0.0, 0.0, 0.0, 0.0]), [0.0, 0.0, 0.0, 0.0]);
        // Slerp endpoints.
        assert_eq!(slerp(IDENTITY, q, 0.0), IDENTITY);
        // A 90° rotation about +z maps +x → +y.
        let half = std::f32::consts::FRAC_PI_4; // angle/2 for a 90° rotation
        let rz = [0.0, 0.0, half.sin(), half.cos()];
        let r = rotate_vec3(rz, [1.0, 0.0, 0.0]);
        assert!((r[0] - 0.0).abs() < 1e-6 && (r[1] - 1.0).abs() < 1e-6 && r[2].abs() < 1e-6);
    }
}
