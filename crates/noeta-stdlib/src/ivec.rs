//! The `vec` module's integer-vector kernels (array-ops arc), over `[i32; N]` for `N ∈ {2, 3, …}`.
//!
//! An "integer vector" in the surface is any `@packed` struct whose fields are **all `i32`** (the
//! user names the type, e.g. `@packed struct IVec3 { x: i32; y: i32; z: i32 }`). It is the integer
//! twin of the structural `Vec3` (`vec3.rs`): std ships the *kernels*, a type opts in with
//! `impl vec.IntKernels for T {}`, and both backends compute through the ONE shared dispatch below —
//! so the differential holds by construction, exactly as for the f32 vector.
//!
//! **Wrapping.** Component add/sub/scale **wrap at `i32` width** — the arc convention, matching the
//! scalar `+` and the packed reductions. A user who wants overflow *detected* reaches for the list
//! layer's `checked_*`; a vector op is the fast, total, wrap-defined path.
//!
//! **`dot` widens to `int` (i64).** A dot product sums componentwise `i32·i32` products, which
//! overflows `i32` almost immediately (`46341² > i32::MAX`). Because a dot is a *reduction to a
//! single value* — the one place the accumulator width matters most and the wrap would be silently,
//! grossly wrong — it widens: each product and the sum are computed at `i64`, and the result type is
//! the language `int`. (The elementwise ops stay at `i32` because their result is *another `i32`
//! vector*, so a wider result has nowhere to live; the dot's result is a scalar that does.)

use crate::registry::{
    BundleFn, BundleReceiver, ConstraintArity, ConstraintField, ConstraintLayout, ExtBundle, ExtFn,
    NativeOut, NativeValue, PackedConstraint, RetTy, Scalar, ScalarVec, SigType,
};
use crate::{CtxError, CtxOut, CtxResult, NativeCtx, PackedField, PackedView, Slot, ctx_arity};

// --- Element kernels over `[i32]` (componentwise, wrapping at i32 width) ---

/// Componentwise sum, wrapping at `i32` width. The operands share a length (the caller guarantees it).
pub fn add(a: &[i32], b: &[i32]) -> Vec<i32> {
    a.iter().zip(b).map(|(x, y)| x.wrapping_add(*y)).collect()
}

/// Componentwise difference `a - b`, wrapping at `i32` width.
pub fn sub(a: &[i32], b: &[i32]) -> Vec<i32> {
    a.iter().zip(b).map(|(x, y)| x.wrapping_sub(*y)).collect()
}

/// Scale every component by the integer `s`, wrapping at `i32` width.
pub fn scale(a: &[i32], s: i32) -> Vec<i32> {
    a.iter().map(|x| x.wrapping_mul(s)).collect()
}

/// Componentwise minimum.
pub fn min(a: &[i32], b: &[i32]) -> Vec<i32> {
    a.iter().zip(b).map(|(x, y)| (*x).min(*y)).collect()
}

/// Componentwise maximum.
pub fn max(a: &[i32], b: &[i32]) -> Vec<i32> {
    a.iter().zip(b).map(|(x, y)| (*x).max(*y)).collect()
}

/// Dot product, **widened to `i64`**: `Σ aᵢ·bᵢ` with each product and the accumulation at 64 bits
/// (see the module note on the width decision). Total — no overflow within `i64` for realistic
/// vector lengths.
pub fn dot(a: &[i32], b: &[i32]) -> i64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| *x as i64 * *y as i64)
        .sum()
}

// --- Bulk kernels over a packed `List<IVecN>` byte buffer ---
//
// A packed `List<IVecN>` is a contiguous little-endian `i32` byte buffer (`4·N` bytes/element). Like
// the `vec3` kernels these stream over it byte-direct through `i32::from_le_bytes`/`to_le_bytes`,
// which fold to plain loads/stores on a little-endian target while `chunks_exact` elides bounds
// checks — a tight `i32` loop LLVM autovectorizes under `-O` (the arc's "measure, don't hand-roll
// intrinsics" path). The elementwise kernels are **layout-agnostic**: every 4-byte slot is one `i32`
// component, so a flat op over the whole buffer *is* the componentwise op on both row and column
// layouts (identical to how `vec3::add_buffers` is layout-agnostic).

/// Read one little-endian `i32` from the first 4 bytes of `c`.
#[inline]
fn read_i32(c: &[u8]) -> i32 {
    i32::from_le_bytes([c[0], c[1], c[2], c[3]])
}

/// Elementwise binary op over two equal-length packed `i32` buffers, byte-direct.
fn zip_buffers(a: &[u8], b: &[u8], op: impl Fn(i32, i32) -> i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len());
    for (p, q) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        out.extend_from_slice(&op(read_i32(p), read_i32(q)).to_le_bytes());
    }
    out
}

/// `add_all`: componentwise wrapping sum of two packed integer-vector lists.
pub fn add_buffers(a: &[u8], b: &[u8]) -> Vec<u8> {
    zip_buffers(a, b, |x, y| x.wrapping_add(y))
}

/// `sub_all`: componentwise wrapping difference of two packed integer-vector lists.
pub fn sub_buffers(a: &[u8], b: &[u8]) -> Vec<u8> {
    zip_buffers(a, b, |x, y| x.wrapping_sub(y))
}

/// `scale_all`: wrapping-scale every component of a packed integer-vector list by `s`.
pub fn scale_buffer(a: &[u8], s: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len());
    for c in a.chunks_exact(4) {
        out.extend_from_slice(&read_i32(c).wrapping_mul(s).to_le_bytes());
    }
    out
}

/// [`scale_buffer`] **in place** — the `with_packed_mut` form: a uniquely-owned COW buffer, scaled
/// with no second allocation.
pub fn scale_buffer_in_place(a: &mut [u8], s: i32) {
    for c in a.chunks_exact_mut(4) {
        let scaled = read_i32(c).wrapping_mul(s).to_le_bytes();
        c.copy_from_slice(&scaled);
    }
}

/// `dot_all` over a **row-major** packed integer-vector list of `comps`-component elements → one
/// widened `i64` per element. Each element's `comps` contiguous `i32`s are folded at 64 bits.
pub fn dot_buffers(a: &[u8], b: &[u8], comps: usize) -> Vec<i64> {
    let stride = comps * 4;
    a.chunks_exact(stride)
        .zip(b.chunks_exact(stride))
        .map(|(p, q)| {
            p.chunks_exact(4)
                .zip(q.chunks_exact(4))
                .map(|(x, y)| read_i32(x) as i64 * read_i32(y) as i64)
                .sum()
        })
        .collect()
}

// --- The `vec.IntKernels` bundle + its shared dispatch ---

/// Whether a packed buffer's element is an integer vector — **all** fields `i32` (`IntN{32, signed}`),
/// at least two of them. Returns the component count; `None` otherwise. Signedness of the *slot* is
/// `true` for the `IVec` shapes, but a `u32`-field vector reads identically at 4 bytes and wraps the
/// same, so the buffer kernels accept either — only the read-back extension of a lone slot differs,
/// which componentwise `i32` math never observes.
fn ivec_view(v: &PackedView) -> Option<usize> {
    let all_i32 = v
        .fields
        .iter()
        .all(|f| matches!(f, PackedField::IntN { bits: 32, .. }));
    (v.fields.len() >= 2 && all_i32).then_some(v.fields.len())
}

/// The `vec.IntKernels` method bundle: the integer-vector twin of `vec.Kernels`. Its constraint is a
/// **uniform** `i32` vector of flexible width (`ConstraintArity::Uniform { min: 2 }`), so a single
/// bundle binds `IVec2`, `IVec3`, and any wider all-`i32` `@packed` struct. The receiver rides as
/// slot 0 (`RetTy::SameAsArg(0)` = the receiver's own type); `dot`/`dot_all` return `int`/`List<int>`
/// (the widened result), everything else the receiver's shape.
pub(crate) const VEC_INT_KERNELS: ExtBundle = ExtBundle {
    name: "IntKernels",
    constraint: PackedConstraint {
        fields: &[ConstraintField::IntN {
            bits: 32,
            signed: true,
        }],
        layout: ConstraintLayout::Any,
        arity: ConstraintArity::Uniform { min: 2 },
    },
    methods: &[
        // --- Element methods (`v.dot(w)` on a single bound value) ---
        BundleFn {
            sig: ExtFn {
                name: "add",
                params: &[SigType::Dyn],
                ret: RetTy::SameAsArg(0),
            },
            receiver: BundleReceiver::Element,
        },
        BundleFn {
            sig: ExtFn {
                name: "sub",
                params: &[SigType::Dyn],
                ret: RetTy::SameAsArg(0),
            },
            receiver: BundleReceiver::Element,
        },
        BundleFn {
            sig: ExtFn {
                name: "scale",
                params: &[SigType::Dyn],
                ret: RetTy::SameAsArg(0),
            },
            receiver: BundleReceiver::Element,
        },
        BundleFn {
            sig: ExtFn {
                name: "min",
                params: &[SigType::Dyn],
                ret: RetTy::SameAsArg(0),
            },
            receiver: BundleReceiver::Element,
        },
        BundleFn {
            sig: ExtFn {
                name: "max",
                params: &[SigType::Dyn],
                ret: RetTy::SameAsArg(0),
            },
            receiver: BundleReceiver::Element,
        },
        BundleFn {
            sig: ExtFn {
                name: "dot",
                params: &[SigType::Dyn],
                ret: RetTy::Concrete(SigType::Int),
            },
            receiver: BundleReceiver::Element,
        },
        // --- Bulk methods (`xs.add_all(ys)` on a List of the bound type) ---
        BundleFn {
            sig: ExtFn {
                name: "add_all",
                params: &[SigType::Dyn],
                ret: RetTy::SameAsArg(0),
            },
            receiver: BundleReceiver::Bulk,
        },
        BundleFn {
            sig: ExtFn {
                name: "sub_all",
                params: &[SigType::Dyn],
                ret: RetTy::SameAsArg(0),
            },
            receiver: BundleReceiver::Bulk,
        },
        BundleFn {
            sig: ExtFn {
                name: "scale_all",
                params: &[SigType::Dyn],
                ret: RetTy::SameAsArg(0),
            },
            receiver: BundleReceiver::Bulk,
        },
        BundleFn {
            sig: ExtFn {
                name: "dot_all",
                params: &[SigType::Dyn],
                ret: RetTy::Concrete(SigType::List(&SigType::Int)),
            },
            receiver: BundleReceiver::Bulk,
        },
    ],
    ctx_dispatch: ivec_bundle_dispatch,
};

/// The bundle's dispatch. Element methods compute over the receiver's own fields; bulk methods
/// prepend the receiver and run the shared bulk routing, so `xs.add_all(ys)` is one code path with
/// its `vec3` twin by construction.
fn ivec_bundle_dispatch(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match method {
        "add" | "sub" | "scale" | "min" | "max" | "dot" => {
            ivec_element_dispatch(method, ctx, recv, args)
        }
        _ => {
            let mut all = Vec::with_capacity(args.len() + 1);
            all.push(recv);
            all.extend_from_slice(args);
            ivec_bulk_dispatch(method, ctx, &all)
        }
    }
}

// --- Error + read helpers ---

fn arg_error(message: String) -> CtxError {
    CtxError::Std(crate::StdError {
        kind: crate::ErrorKind::ArgType,
        message,
    })
}

fn len_error(func: &str) -> CtxError {
    arg_error(format!("`vec.{func}` expects two integer vectors of equal length"))
}

/// Read a bound element value — an object of two-or-more `i32` fields — through the deep view.
fn read_ivec_deep(ctx: &mut dyn NativeCtx, func: &str, slot: Slot) -> Result<Vec<i32>, CtxError> {
    if let NativeValue::Map(fields) = ctx.view(slot)? {
        let ints: Option<Vec<i32>> = fields
            .iter()
            .map(|(_, v)| match v {
                NativeValue::Scalar(Scalar::Int(n)) => Some(*n as i32),
                _ => None,
            })
            .collect();
        if let Some(ints) = ints
            && ints.len() >= 2
        {
            return Ok(ints);
        }
    }
    let found = ctx.type_name(slot)?;
    Err(arg_error(format!(
        "`vec.{func}` expects an integer vector (a struct of `i32` fields), found {found}"
    )))
}

/// Read `list[index]` as an integer vector through the reused scalar buffer (no per-element alloc).
fn read_ivec_at(
    ctx: &mut dyn NativeCtx,
    func: &str,
    list: Slot,
    index: usize,
    buf: &mut Vec<Scalar>,
) -> Result<Vec<i32>, CtxError> {
    if ctx.object_scalars_at(list, index, buf)? {
        let ints: Option<Vec<i32>> = buf
            .iter()
            .map(|s| match s {
                Scalar::Int(n) => Some(*n as i32),
                _ => None,
            })
            .collect();
        if let Some(ints) = ints
            && ints.len() >= 2
        {
            return Ok(ints);
        }
    }
    let element = ctx.list_get(list, index)?;
    let found = ctx.type_name(element)?;
    Err(arg_error(format!(
        "`vec.{func}` expects an integer vector (a struct of `i32` fields), found {found}"
    )))
}

/// Read a numeric scalar (`int`) as the integer scale factor.
fn read_factor(ctx: &mut dyn NativeCtx, func: &str, slot: Slot) -> Result<i32, CtxError> {
    match ctx.view(slot)? {
        NativeValue::Scalar(Scalar::Int(n)) => Ok(n as i32),
        _ => {
            let found = ctx.type_name(slot)?;
            Err(arg_error(format!(
                "`vec.{func}` expects an integer factor, found {found}"
            )))
        }
    }
}

fn int_fields(c: &[i32]) -> Vec<Scalar> {
    c.iter().map(|&v| Scalar::Int(v as i64)).collect()
}

fn expect_list(ctx: &mut dyn NativeCtx, func: &str, slot: Slot) -> CtxResult<()> {
    if ctx.is_list(slot)? {
        Ok(())
    } else {
        let found = ctx.type_name(slot)?;
        Err(arg_error(format!("`vec.{func}` expects a list, found {found}")))
    }
}

/// The element half of `vec.IntKernels`: scalar integer-vector math on one bound value.
fn ivec_element_dispatch(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    let a = read_ivec_deep(ctx, method, recv)?;
    let object = |c: Vec<i32>| CtxOut::Out(NativeOut::Object(int_fields(&c)));
    Ok(match method {
        "scale" => {
            ctx_arity(method, args, 1)?;
            let s = read_factor(ctx, method, args[0])?;
            object(scale(&a, s))
        }
        "dot" => {
            ctx_arity(method, args, 1)?;
            let b = read_ivec_deep(ctx, method, args[0])?;
            if a.len() != b.len() {
                return Err(len_error(method));
            }
            CtxOut::Out(NativeOut::Scalar(Scalar::Int(dot(&a, &b))))
        }
        "add" | "sub" | "min" | "max" => {
            ctx_arity(method, args, 1)?;
            let b = read_ivec_deep(ctx, method, args[0])?;
            if a.len() != b.len() {
                return Err(len_error(method));
            }
            object(match method {
                "add" => add(&a, &b),
                "sub" => sub(&a, &b),
                "min" => min(&a, &b),
                _ => max(&a, &b),
            })
        }
        _ => return Err(crate::no_method_error("vec.IntKernels", method).into()),
    })
}

/// If `slot` is a packed integer-vector list, its `(column, component_count, bytes)`. `None` → the
/// caller falls back to the element-wise path.
fn packed_ivec<C: noeta_ext_abi::ctx::PackedBuffers + ?Sized>(
    ctx: &mut C,
    slot: Slot,
) -> CtxResult<Option<(bool, usize, Vec<u8>)>> {
    let mut out = None;
    ctx.with_packed(slot, &mut |v, bytes| {
        if let Some(comps) = ivec_view(v) {
            out = Some((v.column, comps, bytes.to_vec()));
        }
    })?;
    Ok(out)
}

/// Elementwise fallback for `add_all`/`sub_all`/`min_all`/`max_all` over boxed (or column) operands.
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
    let (mut ba, mut bb) = (Vec::new(), Vec::new());
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let a = read_ivec_at(ctx, func, xs, i, &mut ba)?;
        let b = read_ivec_at(ctx, func, ys, i, &mut bb)?;
        if a.len() != b.len() {
            return Err(len_error(func));
        }
        let c = match func {
            "add_all" => add(&a, &b),
            _ => sub(&a, &b),
        };
        out.push(ctx.make_object_like_element(xs, i, &int_fields(&c))?);
    }
    Ok(CtxOut::Slot(ctx.make_list(&out)?))
}

/// The integer-vector bulk kernels' shared dispatch (both backends): `add_all`/`sub_all`/`scale_all`/
/// `dot_all` over packed `i32` buffers, with a layout-correct element-wise fallback.
pub(crate) fn ivec_bulk_dispatch(
    func: &str,
    ctx: &mut dyn NativeCtx,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match func {
        "add_all" | "sub_all" => {
            ctx_arity(func, args, 2)?;
            expect_list(ctx, func, args[0])?;
            expect_list(ctx, func, args[1])?;
            // Fast path: two packed integer-vector buffers of the same layout and length — the flat
            // elementwise `i32` kernel is layout-agnostic when the layout is shared.
            if let Some((column, comps, ab)) = packed_ivec(ctx, args[0])? {
                let mut out: Option<Vec<u8>> = None;
                ctx.with_packed(args[1], &mut |v, b| {
                    if ivec_view(v) == Some(comps) && v.column == column && b.len() == ab.len() {
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
            let mut is_packed = false;
            ctx.with_packed(args[0], &mut |v, _| is_packed = ivec_view(v).is_some())?;
            if is_packed
                && let Some(result) =
                    ctx.with_packed_mut(args[0], &mut |_, bytes| scale_buffer_in_place(bytes, s))?
            {
                return Ok(CtxOut::Slot(result));
            }
            let n = ctx.list_len(args[0])?;
            let mut buf = Vec::new();
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                let c = scale(&read_ivec_at(ctx, func, args[0], i, &mut buf)?, s);
                out.push(ctx.make_object_like_element(args[0], i, &int_fields(&c))?);
            }
            Ok(CtxOut::Slot(ctx.make_list(&out)?))
        }
        "dot_all" => {
            ctx_arity(func, args, 2)?;
            expect_list(ctx, func, args[0])?;
            expect_list(ctx, func, args[1])?;
            // Fast path: row-major packed buffers of equal length and component count. A column
            // buffer (or boxed operands) takes the element-wise fallback, which is layout-correct.
            if let Some((false, comps, ab)) = packed_ivec(ctx, args[0])? {
                let mut out: Option<Vec<i64>> = None;
                ctx.with_packed(args[1], &mut |v, b| {
                    if ivec_view(v) == Some(comps) && !v.column && b.len() == ab.len() {
                        out = Some(dot_buffers(&ab, b, comps));
                    }
                })?;
                if let Some(scalars) = out {
                    return Ok(CtxOut::Out(NativeOut::Scalars(ScalarVec::Int(scalars))));
                }
            }
            let n = ctx.list_len(args[0])?;
            if ctx.list_len(args[1])? != n {
                return Err(len_error(func));
            }
            let (mut ba, mut bb) = (Vec::new(), Vec::new());
            let mut scalars = Vec::with_capacity(n);
            for i in 0..n {
                let a = read_ivec_at(ctx, func, args[0], i, &mut ba)?;
                let b = read_ivec_at(ctx, func, args[1], i, &mut bb)?;
                if a.len() != b.len() {
                    return Err(len_error(func));
                }
                scalars.push(dot(&a, &b));
            }
            Ok(CtxOut::Out(NativeOut::Scalars(ScalarVec::Int(scalars))))
        }
        _ => Err(crate::no_function_error("vec", func).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(ints: &[i32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(ints.len() * 4);
        for &i in ints {
            out.extend_from_slice(&i.to_le_bytes());
        }
        out
    }
    fn decode(bytes: &[u8]) -> Vec<i32> {
        bytes.chunks_exact(4).map(read_i32).collect()
    }

    #[test]
    fn element_ops_wrap_and_widen() {
        assert_eq!(add(&[1, 2, 3], &[10, 20, 30]), [11, 22, 33]);
        assert_eq!(sub(&[10, 20, 30], &[1, 2, 3]), [9, 18, 27]);
        assert_eq!(scale(&[1, 2, 3], 2), [2, 4, 6]);
        assert_eq!(min(&[1, 5, 3], &[4, 2, 3]), [1, 2, 3]);
        assert_eq!(max(&[1, 5, 3], &[4, 2, 3]), [4, 5, 3]);
        // 2-component works too.
        assert_eq!(add(&[1, 2], &[3, 4]), [4, 6]);
        // Wrapping at i32 width (arc convention): MAX + 1 → MIN.
        assert_eq!(add(&[i32::MAX], &[1]), [i32::MIN]);
        // Dot widens: 46341² overflows i32 but not i64.
        assert_eq!(dot(&[46341], &[46341]), 46341i64 * 46341);
        assert_eq!(dot(&[1, 2, 3], &[4, 5, 6]), 32);
    }

    #[test]
    fn bulk_kernels_match_element() {
        let a = buf(&[1, 2, 3, 4, 5, 6]); // two IVec3s
        let b = buf(&[10, 20, 30, 40, 50, 60]);
        assert_eq!(decode(&add_buffers(&a, &b)), [11, 22, 33, 44, 55, 66]);
        assert_eq!(decode(&sub_buffers(&b, &a)), [9, 18, 27, 36, 45, 54]);
        assert_eq!(decode(&scale_buffer(&a, 3)), [3, 6, 9, 12, 15, 18]);
        // dot per element: [1·10+2·20+3·30, 4·40+5·50+6·60] = [140, 770]
        assert_eq!(dot_buffers(&a, &b, 3), [140, 770]);
        // 2-component grouping.
        let c = buf(&[1, 2, 3, 4]);
        let d = buf(&[10, 20, 30, 40]);
        // dot per 2-group: [1·10+2·20, 3·30+4·40] = [50, 250]
        assert_eq!(dot_buffers(&c, &d, 2), [50, 250]);
        // in-place scale matches.
        let mut m = buf(&[1, 2, 3, 4, 5, 6]);
        scale_buffer_in_place(&mut m, 3);
        assert_eq!(decode(&m), decode(&scale_buffer(&a, 3)));
    }
}
