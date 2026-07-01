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

/// Euclidean distance between two points, `‖a − b‖`.
pub fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    length(sub(a, b))
}

/// Linear interpolation `a + (b − a)·t` (component-wise; `t` is not clamped).
pub fn lerp(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    add(a, scale(sub(b, a), t))
}

/// Reflect `v` about the plane with normal `n`: `v − 2·(v·n)·n` (the standard mirror reflection;
/// `n` is assumed unit-length, matching every graphics library).
pub fn reflect(v: [f32; 3], n: [f32; 3]) -> [f32; 3] {
    sub(v, scale(n, 2.0 * dot(v, n)))
}

/// Component-wise clamp into `[lo, hi]`, computed as `max(lo).min(hi)` per component so it is total
/// even if `lo > hi` (no panic, deterministic).
pub fn clamp(v: [f32; 3], lo: [f32; 3], hi: [f32; 3]) -> [f32; 3] {
    [
        v[0].max(lo[0]).min(hi[0]),
        v[1].max(lo[1]).min(hi[1]),
        v[2].max(lo[2]).min(hi[2]),
    ]
}

/// Component-wise minimum.
pub fn min(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].min(b[2])]
}

/// Component-wise maximum.
pub fn max(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0].max(b[0]), a[1].max(b[1]), a[2].max(b[2])]
}

/// Component-wise absolute value.
pub fn abs(a: [f32; 3]) -> [f32; 3] {
    [a[0].abs(), a[1].abs(), a[2].abs()]
}

// --- Bulk kernels over a packed `List<Vec3<f32>>` byte buffer (P-PACK Phase 4.2/4.3) ---
//
// A packed `List<Vec3<f32>>` is a contiguous little-endian `f32` byte buffer (12 bytes/element).
// These kernels stream over it **byte-direct** through `f32::from_le_bytes`/`to_le_bytes` — on a
// little-endian target those fold to plain loads/stores, and `chunks_exact` elides bounds checks,
// so the body is a tight `f32` loop LLVM autovectorizes under `-O`. Going byte-direct (one output
// allocation, one pass) rather than decode→`Vec<f32>`→compute→encode (four allocations, four passes)
// is a small but real win — ~1.5–3% on the packed `add_all` path (`vm_vec_add_all` bench, P-PACK 4.3);
// modest because the kernel is a fraction of the op's cost (building the result list dominates). Both
// backends store this layout identically, so they call these directly and agree by construction.
// (`lang-stdlib` is `unsafe`-free and the buffer is `Vec<u8>` / 1-aligned, so a zero-copy `&[f32]`
// reinterpret — the path to true aligned SIMD — is unavailable here without either `unsafe` in
// `lang-value` (which would break the shared-kernel design) or an SoA layout; deferred.)

/// Read one little-endian `f32` from the first 4 bytes of `c`.
#[inline]
fn read_f32(c: &[u8]) -> f32 {
    f32::from_le_bytes([c[0], c[1], c[2], c[3]])
}

/// Element-wise binary op over two equal-length packed f32 buffers, byte-direct. Used for
/// `add_all`/`sub_all`: a Vec3 list is `[x0,y0,z0, x1,…]`, so a flat element-wise `f32` op over the
/// whole buffer *is* the component-wise Vec3 op. The two operands must be the same length (same list
/// length and element width); the caller guarantees it.
fn zip_buffers(a: &[u8], b: &[u8], op: impl Fn(f32, f32) -> f32) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len());
    for (p, q) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        out.extend_from_slice(&op(read_f32(p), read_f32(q)).to_le_bytes());
    }
    out
}

/// `add_all`: component-wise sum of two packed Vec3 lists.
pub fn add_buffers(a: &[u8], b: &[u8]) -> Vec<u8> {
    zip_buffers(a, b, |x, y| x + y)
}

/// `sub_all`: component-wise difference of two packed Vec3 lists.
pub fn sub_buffers(a: &[u8], b: &[u8]) -> Vec<u8> {
    zip_buffers(a, b, |x, y| x - y)
}

/// `scale_all`: scale every component of a packed Vec3 list by `s`.
pub fn scale_buffer(a: &[u8], s: f32) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len());
    for c in a.chunks_exact(4) {
        out.extend_from_slice(&(read_f32(c) * s).to_le_bytes());
    }
    out
}

/// `dot_all`: the per-element dot product of two packed Vec3 lists → one `f32` per element.
pub fn dot_buffers(a: &[u8], b: &[u8]) -> Vec<f32> {
    a.chunks_exact(12)
        .zip(b.chunks_exact(12))
        .map(|(p, q)| {
            read_f32(&p[0..]) * read_f32(&q[0..])
                + read_f32(&p[4..]) * read_f32(&q[4..])
                + read_f32(&p[8..]) * read_f32(&q[8..])
        })
        .collect()
}

/// `length_all`: the Euclidean length of each element of a packed Vec3 list → one `f32` per element.
pub fn length_buffer(a: &[u8]) -> Vec<f32> {
    a.chunks_exact(12)
        .map(|p| {
            let (x, y, z) = (read_f32(&p[0..]), read_f32(&p[4..]), read_f32(&p[8..]));
            (x * x + y * y + z * z).sqrt()
        })
        .collect()
}

// --- Opt-in columnar (SoA) Vec3 batch (P-SIMD) ---
//
// The AoS packed `List<Vec3>` above interleaves components (`x0,y0,z0,x1,…`), which is right for O(1)
// append and contiguous per-element access but defeats manual SIMD: a component of N elements is
// strided, so a reduction (dot/length) needs a scalar gather to fill a lane register (benched
// 1.8×–9× *slower* than the autovectorized scalar loop — see `plans/perf/p-simd.md`).
//
// [`SoaVec3`] is the opt-in alternative a user builds explicitly for bulk math: three **contiguous**
// `f32` columns. Now a whole reduction runs over each column independently, so `x[i]*bx[i]` is a
// contiguous same-type `f32` loop LLVM **autovectorizes across elements** — which the AoS stride-12
// layout could not (each AoS step is a horizontal 3-wide combine that stays scalar). That is the
// actual throughput lever: on `dot`/`length` the SoA scalar kernels run **2.7×–4× faster** than the
// AoS kernels (`plans/perf/p-simd.md`, `soa_reductions` bench). Explicit `wide` SIMD was tried on
// these same columns and was *not* faster than the autovectorized scalar loop (it added marshaling
// over what LLVM already does), so these stay scalar. It is a separate value type (not the general
// packed list), so the general list keeps its O(1) append; the SoA batch is built once (an O(n)
// transpose) and reduced many times.
//
// The kernels stay **bit-identical to the AoS kernels**: element-wise ops are per-lane IEEE, and the
// reductions keep the scalar left-to-right order per element (`(x·bx + y·by) + z·bz`), so there is no
// float-add reorder — the differential and the AoS conformance expectations hold by construction.

/// A columnar batch of Vec3s: three contiguous `f32` columns of equal length. The opt-in SoA layout
/// for bulk 3D math (see the module note above). Immutable and value-semantic; the bulk ops return a
/// fresh batch or a `Vec<f32>`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SoaVec3 {
    pub xs: Vec<f32>,
    pub ys: Vec<f32>,
    pub zs: Vec<f32>,
}

impl SoaVec3 {
    /// The number of elements (one per column entry).
    pub fn len(&self) -> usize {
        self.xs.len()
    }

    /// Whether the batch has no elements.
    pub fn is_empty(&self) -> bool {
        self.xs.is_empty()
    }
}

/// Build a [`SoaVec3`] from an AoS packed `List<Vec3>` byte buffer (12 bytes/element, `x,y,z`
/// interleaved) — the one-time O(n) transpose that pays for the fast reductions afterward.
pub fn soa_from_packed(bytes: &[u8]) -> SoaVec3 {
    let n = bytes.len() / 12;
    let mut xs = Vec::with_capacity(n);
    let mut ys = Vec::with_capacity(n);
    let mut zs = Vec::with_capacity(n);
    for c in bytes.chunks_exact(12) {
        xs.push(read_f32(&c[0..]));
        ys.push(read_f32(&c[4..]));
        zs.push(read_f32(&c[8..]));
    }
    SoaVec3 { xs, ys, zs }
}

/// Serialize a [`SoaVec3`] back to an AoS packed `List<Vec3>` byte buffer — the inverse transpose,
/// so a result batch can re-enter the packed-list machinery (`vec.soa_list`).
pub fn soa_to_packed(a: &SoaVec3) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len() * 12);
    for i in 0..a.len() {
        out.extend_from_slice(&a.xs[i].to_le_bytes());
        out.extend_from_slice(&a.ys[i].to_le_bytes());
        out.extend_from_slice(&a.zs[i].to_le_bytes());
    }
    out
}

/// Component-wise sum of two batches → a new batch.
pub fn soa_add(a: &SoaVec3, b: &SoaVec3) -> SoaVec3 {
    soa_zip(a, b, |x, y| x + y)
}

/// Component-wise difference of two batches → a new batch.
pub fn soa_sub(a: &SoaVec3, b: &SoaVec3) -> SoaVec3 {
    soa_zip(a, b, |x, y| x - y)
}

/// Scale every component of a batch by `s` → a new batch.
pub fn soa_scale(a: &SoaVec3, s: f32) -> SoaVec3 {
    SoaVec3 {
        xs: a.xs.iter().map(|&v| v * s).collect(),
        ys: a.ys.iter().map(|&v| v * s).collect(),
        zs: a.zs.iter().map(|&v| v * s).collect(),
    }
}

/// Element-wise binary op over each column (contiguous `f32`, so LLVM autovectorizes each column).
fn soa_zip(a: &SoaVec3, b: &SoaVec3, op: impl Fn(f32, f32) -> f32 + Copy) -> SoaVec3 {
    let col = |u: &[f32], v: &[f32]| u.iter().zip(v).map(|(&x, &y)| op(x, y)).collect();
    SoaVec3 {
        xs: col(&a.xs, &b.xs),
        ys: col(&a.ys, &b.ys),
        zs: col(&a.zs, &b.zs),
    }
}

/// `dot` over an SoA batch → one `f32` per element. Iterator-zipped (bounds-check-free) so the three
/// contiguous column products autovectorize; each element keeps the `(x·bx + y·by) + z·bz` order.
pub fn soa_dot(a: &SoaVec3, b: &SoaVec3) -> Vec<f32> {
    let xy = a.xs.iter().zip(&b.xs).zip(a.ys.iter().zip(&b.ys));
    xy.zip(a.zs.iter().zip(&b.zs))
        .map(|(((xa, xb), (ya, yb)), (za, zb))| xa * xb + ya * yb + za * zb)
        .collect()
}

/// `length` over an SoA batch → one `f32` per element: `√((x·x + y·y) + z·z)`, per element (matching
/// the AoS `length_buffer` order). Iterator-zipped so the contiguous columns autovectorize.
pub fn soa_length(a: &SoaVec3) -> Vec<f32> {
    a.xs.iter()
        .zip(&a.ys)
        .zip(&a.zs)
        .map(|((&x, &y), &z)| (x * x + y * y + z * z).sqrt())
        .collect()
}

/// Encode an `f32` slice to a little-endian byte buffer — the packed-list representation. Used to
/// build test inputs and to check kernel outputs.
#[cfg(test)]
fn encode(floats: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(floats.len() * 4);
    for &f in floats {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Decode a packed f32 byte buffer into a `Vec<f32>` — the inverse of [`encode`], for tests.
#[cfg(test)]
fn decode(bytes: &[u8]) -> Vec<f32> {
    bytes.chunks_exact(4).map(read_f32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(floats: &[f32]) -> Vec<u8> {
        encode(floats)
    }

    #[test]
    fn bulk_kernels_match_scalar() {
        let a = buf(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]); // two Vec3s
        let b = buf(&[10.0, 20.0, 30.0, 40.0, 50.0, 60.0]);
        assert_eq!(
            decode(&add_buffers(&a, &b)),
            [11.0, 22.0, 33.0, 44.0, 55.0, 66.0]
        );
        assert_eq!(
            decode(&sub_buffers(&b, &a)),
            [9.0, 18.0, 27.0, 36.0, 45.0, 54.0]
        );
        assert_eq!(
            decode(&scale_buffer(&a, 2.0)),
            [2.0, 4.0, 6.0, 8.0, 10.0, 12.0]
        );
        // dot: [1·10+2·20+3·30, 4·40+5·50+6·60] = [140, 770]
        assert_eq!(dot_buffers(&a, &b), [140.0, 770.0]);
        assert_eq!(length_buffer(&buf(&[3.0, 4.0, 0.0])), [5.0]);
    }

    #[test]
    fn soa_reductions_match_aos_and_roundtrip() {
        // The SoA reductions must be byte-identical to the AoS kernels (they pin the same conformance
        // expectations), and the AoS↔SoA transpose must round-trip. 19 elements, values spanning
        // negatives/fractions/zero so any reduction-order change would surface as a mismatch.
        let n = 19;
        let xs: Vec<f32> = (0..n)
            .map(|i| (i as f32) * 0.3125 - 3.0 + (i as f32).sin())
            .collect();
        let ys: Vec<f32> = (0..n).map(|i| (i as f32).cos() * 1.75 - 2.5).collect();
        let zs: Vec<f32> = (0..n).map(|i| (i as f32) * -0.5 + 0.125).collect();
        let a = SoaVec3 {
            xs: xs.clone(),
            ys: ys.clone(),
            zs: zs.clone(),
        };
        let b = SoaVec3 {
            xs: ys,
            ys: zs,
            zs: xs,
        };

        // SoA reductions == AoS reductions on the transposed buffers (bit-identical).
        let a_aos = soa_to_packed(&a);
        let b_aos = soa_to_packed(&b);
        assert_eq!(soa_dot(&a, &b), dot_buffers(&a_aos, &b_aos));
        assert_eq!(soa_length(&a), length_buffer(&a_aos));
        // The AoS→SoA→AoS transpose round-trips exactly.
        assert_eq!(soa_from_packed(&a_aos), a);
        // Element-wise ops: add/sub/scale agree with per-lane references.
        let sum = soa_add(&a, &b);
        let diff = soa_sub(&a, &b);
        let scaled = soa_scale(&a, 2.5);
        for i in 0..n as usize {
            assert_eq!(sum.xs[i], a.xs[i] + b.xs[i]);
            assert_eq!(diff.ys[i], a.ys[i] - b.ys[i]);
            assert_eq!(scaled.zs[i], a.zs[i] * 2.5);
        }
    }

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
        assert_eq!(distance([0.0, 0.0, 0.0], [3.0, 4.0, 0.0]), 5.0);
        assert_eq!(
            lerp([0.0, 0.0, 0.0], [10.0, 20.0, 30.0], 0.5),
            [5.0, 10.0, 15.0]
        );
        assert_eq!(reflect([1.0, -1.0, 0.0], [0.0, 1.0, 0.0]), [1.0, 1.0, 0.0]);
        assert_eq!(
            clamp([5.0, -5.0, 2.0], [0.0, 0.0, 0.0], [1.0, 1.0, 1.0]),
            [1.0, 0.0, 1.0]
        );
        assert_eq!(min([1.0, 5.0, 3.0], [4.0, 2.0, 3.0]), [1.0, 2.0, 3.0]);
        assert_eq!(max([1.0, 5.0, 3.0], [4.0, 2.0, 3.0]), [4.0, 5.0, 3.0]);
        assert_eq!(abs([-1.0, 2.0, -3.0]), [1.0, 2.0, 3.0]);
    }
}
