//! The `vec` module's pure 3D-vector math, over `[f32; 3]`.
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

// --- Bulk kernels over a packed `List<Vec3<f32>>` byte buffer ---
//
// A packed `List<Vec3<f32>>` is a contiguous little-endian `f32` byte buffer (12 bytes/element).
// These kernels stream over it **byte-direct** through `f32::from_le_bytes`/`to_le_bytes` — on a
// little-endian target those fold to plain loads/stores, and `chunks_exact` elides bounds checks,
// so the body is a tight `f32` loop LLVM autovectorizes under `-O`. Going byte-direct (one output
// allocation, one pass) rather than decode→`Vec<f32>`→compute→encode (four allocations, four passes)
// is a small but real win — ~1.5–3% on the packed `add_all` path (`vm_vec_add_all` bench);
// modest because the kernel is a fraction of the op's cost (building the result list dominates). Both
// backends store this layout identically, so they call these directly and agree by construction.
// These **AoS/row** kernels stay byte-direct: an interleaved buffer has no contiguous per-component
// run to reinterpret, so a typed `&[f32]` view would not help a reduction (the component stride is 12
// bytes). The **column** layout does have contiguous per-component runs — see [`col_dot`]/[`col_length`]
// below, which reinterpret each column to `&[f32]` via `bytemuck` (safe, checked) for the aligned-SIMD
// win. That is why the win needs the column *layout*, not just these kernels.

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

/// [`scale_buffer`] **in place** — the `with_packed_mut` form: the seam
/// hands the kernel a uniquely-owned COW buffer, so scaling needs no second allocation at all.
pub fn scale_buffer_in_place(a: &mut [u8], s: f32) {
    for c in a.chunks_exact_mut(4) {
        let scaled = (read_f32(c) * s).to_le_bytes();
        c.copy_from_slice(&scaled);
    }
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

// --- Opt-in columnar (SoA) Vec3 batch ---
//
// The AoS packed `List<Vec3>` above interleaves components (`x0,y0,z0,x1,…`), which is right for O(1)
// append and contiguous per-element access but defeats manual SIMD: a component of N elements is
// strided, so a reduction (dot/length) needs a scalar gather to fill a lane register (benched
// 1.8×–9× *slower* than the autovectorized scalar loop).
//
// [`SoaVec3`] is the opt-in alternative a user builds explicitly for bulk math: three **contiguous**
// `f32` columns. Now a whole reduction runs over each column independently, so `x[i]*bx[i]` is a
// contiguous same-type `f32` loop LLVM **autovectorizes across elements** — which the AoS stride-12
// layout could not (each AoS step is a horizontal 3-wide combine that stays scalar). That is the
// actual throughput lever: on `dot`/`length` the SoA scalar kernels run **2.7×–4× faster** than the
// AoS kernels (the `soa_reductions` bench). Explicit `wide` SIMD was tried on
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

/// Serialize a [`SoaVec3`] to a **column-major** packed `List<Vec3>` byte buffer
/// (`[x×n][y×n][z×n]`) — so a batch (or a test) can produce a `Layout.Column` buffer. Each column is written
/// as one contiguous `f32` run. (The reverse — reading a column buffer — is deliberately *not* a
/// decode helper: the reduction kernels [`col_dot`]/[`col_length`] read the contiguous columns in
/// place, since a per-call `SoaVec3` decode benched slower than the AoS kernels; see below.)
pub fn soa_to_columns(a: &SoaVec3) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len() * 12);
    for &x in &a.xs {
        out.extend_from_slice(&x.to_le_bytes());
    }
    for &y in &a.ys {
        out.extend_from_slice(&y.to_le_bytes());
    }
    for &z in &a.zs {
        out.extend_from_slice(&z.to_le_bytes());
    }
    out
}

// --- Direct column-buffer reduction kernels ---
//
// The `dot`/`length` reductions over a `@packed(Layout.Column)` Vec3 buffer, reading the three
// **contiguous** `f32` columns straight out of the byte buffer — **no** `SoaVec3` decode (that
// per-call `Vec<f32>` allocation benched *slower* than the AoS kernels, wiping out the layout win).
// Each column is a contiguous run, so the per-element products are three contiguous `f32` streams
// LLVM autovectorizes across elements — the same lever as the `soa_*` kernels, without the alloc.
// Bit-identical to the AoS kernels: each element keeps the `(x·bx + y·by) + z·bz` order.
//
// (`add`/`sub`/`scale` need **no** column kernel: they are element-wise over the flat `f32` array, so
// the same permutation carries through — `add_buffers` on two column buffers already yields the
// correct column result, at row speed. Only the reductions, which combine an element's three
// components, care about the layout.)

// The throughput lever is reading each column as a typed `&[f32]` — which LLVM autovectorizes across
// elements — rather than a `&[u8]` decoded per element (benched: byte reads exactly match the AoS
// kernel, no win; the layout alone buys nothing). `bytemuck::try_cast_slice` gives that `&[f32]` view
// **zero-copy** when the buffer is `f32`-aligned (which a heap `Vec<u8>` of these sizes is), with the
// unsafe reinterpret encapsulated behind its safe, checked API — so our source stays `unsafe`-free
// (`unsafe_code = "forbid"` holds). If a buffer is ever misaligned (tiny lists), we fall back to the
// per-element byte kernel — bit-identical, just not vectorized. This is the S4 win reached through
// the column *directive* rather than an explicit SoA value type.

/// View a column-major Vec3 byte buffer (`n` elements, `[x×n][y×n][z×n]`) as three typed `f32` column
/// slices, or `None` if the buffer is not `f32`-aligned (then the caller uses the byte-read fallback).
fn columns_f32(buf: &[u8], n: usize) -> Option<(&[f32], &[f32], &[f32])> {
    let floats: &[f32] = bytemuck::try_cast_slice(buf).ok()?;
    // `try_cast_slice` already guarantees `floats.len() == buf.len() / 4 == 3n`; slice into columns.
    Some((&floats[..n], &floats[n..2 * n], &floats[2 * n..]))
}

/// The `k`-th contiguous `f32` column (`k` = 0/1/2 for x/y/z) of a column-major Vec3 buffer of `n`
/// elements — a `4n`-byte sub-slice (byte-fallback view).
fn column(buf: &[u8], k: usize, n: usize) -> &[u8] {
    &buf[k * 4 * n..(k + 1) * 4 * n]
}

/// `dot_all` over two column-major Vec3 buffers → one `f32` per element. Fast path: read the three
/// columns as typed `&[f32]` and reduce (autovectorized); fallback: per-element byte reads. Both keep
/// the `(x·bx + y·by) + z·bz` order, so bit-identical to the AoS kernel.
pub fn col_dot(a: &[u8], b: &[u8]) -> Vec<f32> {
    let n = a.len() / 12;
    match (columns_f32(a, n), columns_f32(b, n)) {
        (Some((ax, ay, az)), Some((bx, by, bz))) => ax
            .iter()
            .zip(bx)
            .zip(ay.iter().zip(by))
            .zip(az.iter().zip(bz))
            .map(|(((xa, xb), (ya, yb)), (za, zb))| xa * xb + ya * yb + za * zb)
            .collect(),
        _ => {
            let xs = column(a, 0, n)
                .chunks_exact(4)
                .zip(column(b, 0, n).chunks_exact(4));
            let ys = column(a, 1, n)
                .chunks_exact(4)
                .zip(column(b, 1, n).chunks_exact(4));
            let zs = column(a, 2, n)
                .chunks_exact(4)
                .zip(column(b, 2, n).chunks_exact(4));
            xs.zip(ys)
                .zip(zs)
                .map(|(((xa, xb), (ya, yb)), (za, zb))| {
                    read_f32(xa) * read_f32(xb)
                        + read_f32(ya) * read_f32(yb)
                        + read_f32(za) * read_f32(zb)
                })
                .collect()
        }
    }
}

/// `length_all` over a column-major Vec3 buffer → one `f32` per element: `√((x·x + y·y) + z·z)`. Same
/// fast-typed / byte-fallback split as [`col_dot`].
pub fn col_length(a: &[u8]) -> Vec<f32> {
    let n = a.len() / 12;
    match columns_f32(a, n) {
        Some((xs, ys, zs)) => xs
            .iter()
            .zip(ys)
            .zip(zs)
            .map(|((&x, &y), &z)| (x * x + y * y + z * z).sqrt())
            .collect(),
        None => {
            let xs = column(a, 0, n).chunks_exact(4);
            let ys = column(a, 1, n).chunks_exact(4);
            let zs = column(a, 2, n).chunks_exact(4);
            xs.zip(ys)
                .zip(zs)
                .map(|((px, py), pz)| {
                    let (x, y, z) = (read_f32(px), read_f32(py), read_f32(pz));
                    (x * x + y * y + z * z).sqrt()
                })
                .collect()
        }
    }
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

// --- The bulk `vec.*_all` dispatch over the raw-buffer seam ---
//
// Until N3.4 these five functions were the LAST per-backend native intercepts (`call_vec` twins in
// the VM and the tree-walker): the neutral value seam could not lend a dispatch a packed list's
// contiguous bytes. `NativeCtx::with_packed`/`with_packed_mut`/`make_packed_like` close that gap,
// so the routing now lives here ONCE — a registered ctx dispatch, like `task`'s — and the
// differential holds by construction. Packed `Vec3` operands take the flat autovectorized kernels
// above (zero per-element traffic: one borrow per operand, one result allocation); anything else
// falls back to an element-wise loop over `object_scalars`/`make_object_like` — the boxed path.

use crate::registry::{ExtFn, NativeOut, NativeValue, RetTy, Scalar, ScalarVec, SigType};
use crate::{CtxError, CtxOut, CtxResult, NativeCtx, PackedField, PackedView, Slot, ctx_arity};

/// The bulk kernels' signatures. Structural arguments are `Dyn` (any 3-`f32`-field struct is a
/// Vec3 — checked at dispatch); the element-wise ops return their first argument's type, the
/// reductions a `List<f32>` — the same rows the checker's hand-written fallback carried before.
pub(crate) const VEC_CTX_FNS: &[ExtFn] = &[
    ExtFn {
        param_names: &["a", "b"],
        name: "add_all",
        params: &[SigType::Dyn, SigType::Dyn],
        ret: RetTy::SameAsArg(0),
    },
    ExtFn {
        param_names: &["a", "b"],
        name: "sub_all",
        params: &[SigType::Dyn, SigType::Dyn],
        ret: RetTy::SameAsArg(0),
    },
    ExtFn {
        param_names: &["a", "factor"],
        name: "scale_all",
        params: &[SigType::Dyn, SigType::Dyn],
        ret: RetTy::SameAsArg(0),
    },
    ExtFn {
        param_names: &["a", "b"],
        name: "dot_all",
        params: &[SigType::Dyn, SigType::Dyn],
        ret: RetTy::Concrete(SigType::List(&SigType::F32)),
    },
    ExtFn {
        param_names: &["a"],
        name: "length_all",
        params: &[SigType::Dyn],
        ret: RetTy::Concrete(SigType::List(&SigType::F32)),
    },
];

/// Whether a packed buffer's element is a Vec3 — exactly three `f32` fields (either layout).
fn vec3_view(v: &PackedView) -> bool {
    v.fields.len() == 3 && v.fields.iter().all(|f| matches!(f, PackedField::F32))
}

/// A `vec.*` argument-type misuse (maps to the backends' `TypeMismatch` diagnostic).
fn arg_error(message: String) -> CtxError {
    CtxError::Std(crate::StdError {
        kind: crate::ErrorKind::ArgType,
        message,
    })
}

fn len_error(func: &str) -> CtxError {
    arg_error(format!("`vec.{func}` expects two lists of equal length"))
}

/// Guard that a slot holds a list, with the intercepts' exact message.
fn expect_list(ctx: &mut dyn NativeCtx, func: &str, slot: Slot) -> CtxResult<()> {
    if ctx.is_list(slot)? {
        Ok(())
    } else {
        let found = ctx.type_name(slot)?;
        Err(arg_error(format!(
            "`vec.{func}` expects a list, found {found}"
        )))
    }
}

/// Read `list[index]` as a Vec3 — an object of exactly three `f32` fields — through the reused
/// scalar buffer (no per-element allocation), or a type error naming the element's type.
fn read_vec3_at(
    ctx: &mut dyn NativeCtx,
    func: &str,
    list: Slot,
    index: usize,
    buf: &mut Vec<Scalar>,
) -> Result<[f32; 3], CtxError> {
    if ctx.object_scalars_at(list, index, buf)?
        && let [Scalar::F32(x), Scalar::F32(y), Scalar::F32(z)] = buf[..]
    {
        return Ok([x, y, z]);
    }
    // Error path only: mint the element slot just to render its type name.
    let element = ctx.list_get(list, index)?;
    let found = ctx.type_name(element)?;
    Err(arg_error(format!(
        "`vec.{func}` expects a Vec3 (a struct of three f32 fields), found {found}"
    )))
}

/// Read a numeric scalar (`f32`/`float`/`int`) as an `f32` — the `scale_all` factor.
fn read_factor(ctx: &mut dyn NativeCtx, func: &str, slot: Slot) -> Result<f32, CtxError> {
    match ctx.view(slot)? {
        NativeValue::Scalar(Scalar::F32(f)) => Ok(f),
        NativeValue::Scalar(Scalar::Float(f)) => Ok(f as f32),
        NativeValue::Scalar(Scalar::Int(i)) => Ok(i as f32),
        _ => {
            let found = ctx.type_name(slot)?;
            Err(arg_error(format!(
                "`vec.{func}` expects a number factor, found {found}"
            )))
        }
    }
}

/// A Vec3's components as result-object field scalars.
fn f32_fields(c: [f32; 3]) -> [Scalar; 3] {
    [Scalar::F32(c[0]), Scalar::F32(c[1]), Scalar::F32(c[2])]
}

/// A `List<f32>` result from reduction scalars (`dot_all`/`length_all`): the typed bulk vector
/// crosses the seam whole ([`NativeOut::Scalars`]) — one backend conversion pass, no per-element
/// [`NativeOut`] boxing, no intermediate vector.
fn f32_list_out(scalars: Vec<f32>) -> Result<CtxOut, CtxError> {
    Ok(CtxOut::Out(NativeOut::Scalars(ScalarVec::F32(scalars))))
}

/// If `slot` is a packed Vec3 list, its layout + a copy of its bytes (the binary ops' left
/// operand, which must outlive the right operand's borrow). `None` → the caller falls back.
///
/// Bounded on the narrow [`noeta_ext_abi::ctx::PackedBuffers`] view rather than the full
/// `NativeCtx`: the signature states this helper only reads packed buffers, and the
/// `&mut dyn NativeCtx` callers pass straight through the blanket impl. The bound is
/// path-qualified on purpose — importing the view's name file-wide would make the sibling
/// helpers' `ctx.with_packed(…)` calls ambiguous against `NativeCtx`'s own methods.
fn packed_vec3<C: noeta_ext_abi::ctx::PackedBuffers + ?Sized>(
    ctx: &mut C,
    slot: Slot,
) -> CtxResult<Option<(bool, Vec<u8>)>> {
    let mut out = None;
    ctx.with_packed(slot, &mut |v, bytes| {
        if vec3_view(v) {
            out = Some((v.column, bytes.to_vec()));
        }
    })?;
    Ok(out)
}

/// Element-wise fallback for `add_all`/`sub_all`: boxed operands (or packed operands of
/// disagreeing layout/length, which the fast path cannot mix). Each result element is shaped like
/// its left input.
fn bulk_binary_fallback(
    ctx: &mut dyn NativeCtx,
    func: &str,
    xs: Slot,
    ys: Slot,
) -> Result<CtxOut, CtxError> {
    let n = ctx.list_len(xs)?;
    if ctx.list_len(ys)? != n {
        return Err(len_error(func));
    }
    let mut buf = Vec::new();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let a = read_vec3_at(ctx, func, xs, i, &mut buf)?;
        let b = read_vec3_at(ctx, func, ys, i, &mut buf)?;
        let c = if func == "add_all" {
            add(a, b)
        } else {
            sub(a, b)
        };
        out.push(ctx.make_object_like_element(xs, i, &f32_fields(c))?);
    }
    Ok(CtxOut::Slot(ctx.make_list(&out)?))
}

/// The `vec` module's higher-order dispatch: the five bulk kernels, shared by both backends.
pub(crate) fn vec_ctx_dispatch(
    func: &str,
    ctx: &mut dyn NativeCtx,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match func {
        "add_all" | "sub_all" => {
            ctx_arity(func, args, 2)?;
            expect_list(ctx, func, args[0])?;
            expect_list(ctx, func, args[1])?;
            // Fast path: two packed Vec3 buffers of the SAME layout and length — the flat
            // element-wise kernel is layout-agnostic when the layout is shared.
            if let Some((column, ab)) = packed_vec3(ctx, args[0])? {
                let mut out: Option<Vec<u8>> = None;
                ctx.with_packed(args[1], &mut |v, b| {
                    if vec3_view(v) && v.column == column && b.len() == ab.len() {
                        out = Some(if func == "add_all" {
                            add_buffers(&ab, b)
                        } else {
                            sub_buffers(&ab, b)
                        });
                    }
                })?;
                if let Some(bytes) = out {
                    return Ok(CtxOut::Slot(ctx.make_packed_like(args[0], bytes)?));
                }
            }
            bulk_binary_fallback(ctx, func, args[0], args[1])
        }
        "scale_all" => {
            ctx_arity(func, args, 2)?;
            expect_list(ctx, func, args[0])?;
            let s = read_factor(ctx, func, args[1])?;
            // Fast path: scale the packed buffer through the COW-mutable borrow — in place when
            // the backend proves sole ownership, one clone otherwise (either way, zero extra
            // result allocation). Layout-agnostic: every byte is an `f32` component.
            let mut is_packed_vec3 = false;
            ctx.with_packed(args[0], &mut |v, _| is_packed_vec3 = vec3_view(v))?;
            if is_packed_vec3
                && let Some(result) =
                    ctx.with_packed_mut(args[0], &mut |_, bytes| scale_buffer_in_place(bytes, s))?
            {
                return Ok(CtxOut::Slot(result));
            }
            let n = ctx.list_len(args[0])?;
            let mut buf = Vec::new();
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                let c = scale(read_vec3_at(ctx, func, args[0], i, &mut buf)?, s);
                out.push(ctx.make_object_like_element(args[0], i, &f32_fields(c))?);
            }
            Ok(CtxOut::Slot(ctx.make_list(&out)?))
        }
        "dot_all" => {
            ctx_arity(func, args, 2)?;
            expect_list(ctx, func, args[0])?;
            expect_list(ctx, func, args[1])?;
            // Fast path: same-layout packed Vec3 buffers — the column pair reads three contiguous
            // `f32` columns (`col_dot`, the aligned-SIMD reduction), the row pair the interleaved
            // kernel; a mixed pair falls back (each side decodes correctly element-wise).
            if let Some((column, ab)) = packed_vec3(ctx, args[0])? {
                let mut out: Option<Vec<f32>> = None;
                ctx.with_packed(args[1], &mut |v, b| {
                    if vec3_view(v) && v.column == column && b.len() == ab.len() {
                        out = Some(if column {
                            col_dot(&ab, b)
                        } else {
                            dot_buffers(&ab, b)
                        });
                    }
                })?;
                if let Some(scalars) = out {
                    return f32_list_out(scalars);
                }
            }
            let n = ctx.list_len(args[0])?;
            if ctx.list_len(args[1])? != n {
                return Err(len_error(func));
            }
            let mut buf = Vec::new();
            let mut scalars = Vec::with_capacity(n);
            for i in 0..n {
                let a = read_vec3_at(ctx, func, args[0], i, &mut buf)?;
                let b = read_vec3_at(ctx, func, args[1], i, &mut buf)?;
                scalars.push(dot(a, b));
            }
            f32_list_out(scalars)
        }
        "length_all" => {
            ctx_arity(func, args, 1)?;
            expect_list(ctx, func, args[0])?;
            let mut out: Option<Vec<f32>> = None;
            ctx.with_packed(args[0], &mut |v, b| {
                if vec3_view(v) {
                    out = Some(if v.column {
                        col_length(b)
                    } else {
                        length_buffer(b)
                    });
                }
            })?;
            if let Some(scalars) = out {
                return f32_list_out(scalars);
            }
            let n = ctx.list_len(args[0])?;
            let mut buf = Vec::new();
            let mut scalars = Vec::with_capacity(n);
            for i in 0..n {
                scalars.push(length(read_vec3_at(ctx, func, args[0], i, &mut buf)?));
            }
            f32_list_out(scalars)
        }
        _ => Err(crate::no_function_error("vec", func).into()),
    }
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

        // The direct column-buffer reductions read the same values from a `[x×n][y×n][z×n]`
        // buffer and must be bit-identical to the AoS kernels too (they serve the column dispatch
        // path). `add`/`sub`/`scale` are layout-agnostic: `add_buffers` on the *column*
        // buffers equals the column-serialized SoA add.
        let a_col = soa_to_columns(&a);
        let b_col = soa_to_columns(&b);
        assert_eq!(col_dot(&a_col, &b_col), dot_buffers(&a_aos, &b_aos));
        assert_eq!(col_length(&a_col), length_buffer(&a_aos));
        assert_eq!(
            add_buffers(&a_col, &b_col),
            soa_to_columns(&soa_add(&a, &b))
        );
        assert_eq!(
            sub_buffers(&a_col, &b_col),
            soa_to_columns(&soa_sub(&a, &b))
        );
        assert_eq!(
            scale_buffer(&a_col, 2.5),
            soa_to_columns(&soa_scale(&a, 2.5))
        );
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
