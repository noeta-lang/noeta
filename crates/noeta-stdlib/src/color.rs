//! The `vec` module's `Color` kernels (array-ops arc), over `[u8; 4]` (RGBA).
//!
//! A "Color" in the surface is any `@packed` struct with exactly **four `u8` fields** (the user names
//! the type, e.g. `@packed struct Color { r: u8; g: u8; b: u8; a: u8 }`). Like `Vec3`/`IVec3` it is
//! structural: std ships the *kernels*, a type opts in with `impl vec.ColorKernels for T {}`, and the
//! ONE shared dispatch below runs on both backends — differential-identical by construction.
//!
//! **Saturating, NOT wrapping.** This is the *one deliberate exception* to the arc's wrap-at-width
//! convention, and it is domain-driven: channel math must clamp to `[0, 255]`, never wrap. `bright +
//! bright` has to pin at white (`200 + 100 → 255`), never roll over to a dark value (`44`); a
//! subtraction that underflows has to pin at black (`50 - 100 → 0`), never wrap up to `206`. Wrapping
//! colour arithmetic produces the classic "overflow sparkle" bug, so add/sub/scale all **saturate**.
//! (The wrapping convention is right for `IVec`, where a component is a plain modular integer; a
//! colour channel is a clamped intensity, a different domain.)

use crate::registry::{
    BundleFn, BundleReceiver, ConstraintArity, ConstraintField, ConstraintLayout, ExtBundle, ExtFn,
    NativeOut, NativeValue, PackedConstraint, RetTy, Scalar, SigType,
};
use crate::{CtxError, CtxOut, CtxResult, NativeCtx, PackedField, PackedView, Slot, ctx_arity};

// --- Element kernels over `[u8; 4]` (componentwise, saturating) ---

/// Componentwise sum, **saturating** at 255.
pub fn add(a: [u8; 4], b: [u8; 4]) -> [u8; 4] {
    [
        a[0].saturating_add(b[0]),
        a[1].saturating_add(b[1]),
        a[2].saturating_add(b[2]),
        a[3].saturating_add(b[3]),
    ]
}

/// Componentwise difference `a - b`, **saturating** at 0.
pub fn sub(a: [u8; 4], b: [u8; 4]) -> [u8; 4] {
    [
        a[0].saturating_sub(b[0]),
        a[1].saturating_sub(b[1]),
        a[2].saturating_sub(b[2]),
        a[3].saturating_sub(b[3]),
    ]
}

/// Saturating-scale one channel by `s`: multiply in `f32`, round to nearest, clamp to `[0, 255]`.
#[inline]
fn scale_channel(v: u8, s: f32) -> u8 {
    (v as f32 * s).round().clamp(0.0, 255.0) as u8
}

/// Scale every channel by the factor `s`, **saturating** at 255 (and clamping negatives to 0).
pub fn scale(a: [u8; 4], s: f32) -> [u8; 4] {
    [
        scale_channel(a[0], s),
        scale_channel(a[1], s),
        scale_channel(a[2], s),
        scale_channel(a[3], s),
    ]
}

// --- Bulk kernels over a packed `List<Color>` byte buffer ---
//
// A packed `List<Color>` is a contiguous `u8` byte buffer (4 bytes/element). The elementwise kernels
// are **layout-agnostic**: every byte *is* one channel, so a flat per-byte op over the whole buffer
// is the componentwise colour op on both row and column layouts. These are tight `u8` loops LLVM
// autovectorizes (`saturating_add`/`saturating_sub` lower to packed-saturating SIMD on most targets)
// — the arc's "measure, don't hand-roll intrinsics" path, same as the `vec3`/`ivec` kernels.

/// Elementwise byte op over two equal-length packed colour buffers.
fn zip_buffers(a: &[u8], b: &[u8], op: impl Fn(u8, u8) -> u8) -> Vec<u8> {
    a.iter().zip(b).map(|(x, y)| op(*x, *y)).collect()
}

/// `add_all`: componentwise saturating sum of two packed colour lists.
pub fn add_buffers(a: &[u8], b: &[u8]) -> Vec<u8> {
    zip_buffers(a, b, u8::saturating_add)
}

/// `sub_all`: componentwise saturating difference of two packed colour lists.
pub fn sub_buffers(a: &[u8], b: &[u8]) -> Vec<u8> {
    zip_buffers(a, b, u8::saturating_sub)
}

/// `scale_all`: saturating-scale every channel of a packed colour list by `s`.
pub fn scale_buffer(a: &[u8], s: f32) -> Vec<u8> {
    a.iter().map(|&v| scale_channel(v, s)).collect()
}

/// [`scale_buffer`] **in place** — the `with_packed_mut` form (uniquely-owned COW buffer).
pub fn scale_buffer_in_place(a: &mut [u8], s: f32) {
    for v in a.iter_mut() {
        *v = scale_channel(*v, s);
    }
}

// --- The `vec.ColorKernels` bundle + its shared dispatch ---

/// Whether a packed buffer's element is a `Color` — exactly four `u8` fields (`IntN{8, false}`).
fn color_view(v: &PackedView) -> bool {
    v.fields.len() == 4
        && v.fields.iter().all(|f| {
            matches!(
                f,
                PackedField::IntN {
                    bits: 8,
                    signed: false
                }
            )
        })
}

/// The `vec.ColorKernels` method bundle: saturating channel math on a 4×`u8` `@packed` struct. Add/
/// sub/scale only — a colour has no dot/length/normalize (those are vector-space ops; a colour is a
/// clamped 4-tuple of intensities). The receiver rides as slot 0 (`RetTy::SameAsArg(0)`).
pub(crate) const VEC_COLOR_KERNELS: ExtBundle = ExtBundle {
    name: "ColorKernels",
    constraint: PackedConstraint {
        fields: &[
            ConstraintField::IntN {
                bits: 8,
                signed: false,
            },
            ConstraintField::IntN {
                bits: 8,
                signed: false,
            },
            ConstraintField::IntN {
                bits: 8,
                signed: false,
            },
            ConstraintField::IntN {
                bits: 8,
                signed: false,
            },
        ],
        layout: ConstraintLayout::Any,
        arity: ConstraintArity::Exact,
    },
    methods: &[
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
    ],
    ctx_dispatch: color_bundle_dispatch,
};

fn color_bundle_dispatch(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match method {
        "add" | "sub" | "scale" => color_element_dispatch(method, ctx, recv, args),
        _ => {
            let mut all = Vec::with_capacity(args.len() + 1);
            all.push(recv);
            all.extend_from_slice(args);
            color_bulk_dispatch(method, ctx, &all)
        }
    }
}

fn arg_error(message: String) -> CtxError {
    CtxError::Std(crate::StdError {
        kind: crate::ErrorKind::ArgType,
        message,
    })
}

fn len_error(func: &str) -> CtxError {
    arg_error(format!("`vec.{func}` expects two colour lists of equal length"))
}

/// Read one `u8` channel from a `Scalar::Int` in `[0, 255]`; `None` otherwise.
fn channel(s: &Scalar) -> Option<u8> {
    match s {
        Scalar::Int(n) if (0..=255).contains(n) => Some(*n as u8),
        _ => None,
    }
}

/// Read a bound element value — an object of exactly four `u8` channels — through the deep view.
fn read_color_deep(ctx: &mut dyn NativeCtx, func: &str, slot: Slot) -> Result<[u8; 4], CtxError> {
    if let NativeValue::Map(fields) = ctx.view(slot)?
        && let [
            (_, NativeValue::Scalar(r)),
            (_, NativeValue::Scalar(g)),
            (_, NativeValue::Scalar(b)),
            (_, NativeValue::Scalar(a)),
        ] = fields.as_slice()
        && let (Some(r), Some(g), Some(b), Some(a)) = (channel(r), channel(g), channel(b), channel(a))
    {
        return Ok([r, g, b, a]);
    }
    let found = ctx.type_name(slot)?;
    Err(arg_error(format!(
        "`vec.{func}` expects a Color (a struct of four u8 fields), found {found}"
    )))
}

/// Read `list[index]` as a Color through the reused scalar buffer (no per-element alloc).
fn read_color_at(
    ctx: &mut dyn NativeCtx,
    func: &str,
    list: Slot,
    index: usize,
    buf: &mut Vec<Scalar>,
) -> Result<[u8; 4], CtxError> {
    if ctx.object_scalars_at(list, index, buf)?
        && buf.len() == 4
        && let (Some(r), Some(g), Some(b), Some(a)) = (
            channel(&buf[0]),
            channel(&buf[1]),
            channel(&buf[2]),
            channel(&buf[3]),
        )
    {
        return Ok([r, g, b, a]);
    }
    let element = ctx.list_get(list, index)?;
    let found = ctx.type_name(element)?;
    Err(arg_error(format!(
        "`vec.{func}` expects a Color (a struct of four u8 fields), found {found}"
    )))
}

/// Read a numeric scale factor (`float`/`f32`/`int`) as an `f32`.
fn read_factor(ctx: &mut dyn NativeCtx, func: &str, slot: Slot) -> Result<f32, CtxError> {
    match ctx.view(slot)? {
        NativeValue::Scalar(Scalar::F32(f)) => Ok(f),
        NativeValue::Scalar(Scalar::Float(f)) => Ok(f as f32),
        NativeValue::Scalar(Scalar::Int(n)) => Ok(n as f32),
        _ => {
            let found = ctx.type_name(slot)?;
            Err(arg_error(format!(
                "`vec.{func}` expects a number factor, found {found}"
            )))
        }
    }
}

fn u8_fields(c: [u8; 4]) -> [Scalar; 4] {
    [
        Scalar::Int(c[0] as i64),
        Scalar::Int(c[1] as i64),
        Scalar::Int(c[2] as i64),
        Scalar::Int(c[3] as i64),
    ]
}

fn expect_list(ctx: &mut dyn NativeCtx, func: &str, slot: Slot) -> CtxResult<()> {
    if ctx.is_list(slot)? {
        Ok(())
    } else {
        let found = ctx.type_name(slot)?;
        Err(arg_error(format!("`vec.{func}` expects a list, found {found}")))
    }
}

/// The element half of `vec.ColorKernels`: saturating colour math on one bound value.
fn color_element_dispatch(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    let a = read_color_deep(ctx, method, recv)?;
    let object = |c: [u8; 4]| CtxOut::Out(NativeOut::Object(u8_fields(c).to_vec()));
    ctx_arity(method, args, 1)?;
    Ok(match method {
        "scale" => {
            let s = read_factor(ctx, method, args[0])?;
            object(scale(a, s))
        }
        "add" => object(add(a, read_color_deep(ctx, method, args[0])?)),
        "sub" => object(sub(a, read_color_deep(ctx, method, args[0])?)),
        _ => return Err(crate::no_method_error("vec.ColorKernels", method).into()),
    })
}

/// If `slot` is a packed colour list, its layout + a copy of its bytes. `None` → element fallback.
fn packed_color<C: noeta_ext_abi::ctx::PackedBuffers + ?Sized>(
    ctx: &mut C,
    slot: Slot,
) -> CtxResult<Option<(bool, Vec<u8>)>> {
    let mut out = None;
    ctx.with_packed(slot, &mut |v, bytes| {
        if color_view(v) {
            out = Some((v.column, bytes.to_vec()));
        }
    })?;
    Ok(out)
}

/// Elementwise fallback for `add_all`/`sub_all` over boxed (or column) operands.
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
        let a = read_color_at(ctx, func, xs, i, &mut ba)?;
        let b = read_color_at(ctx, func, ys, i, &mut bb)?;
        let c = if func == "add_all" { add(a, b) } else { sub(a, b) };
        out.push(ctx.make_object_like_element(xs, i, &u8_fields(c))?);
    }
    Ok(CtxOut::Slot(ctx.make_list(&out)?))
}

/// The colour bulk kernels' shared dispatch (both backends): `add_all`/`sub_all`/`scale_all` over
/// packed `u8` buffers, with a layout-correct element-wise fallback.
pub(crate) fn color_bulk_dispatch(
    func: &str,
    ctx: &mut dyn NativeCtx,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match func {
        "add_all" | "sub_all" => {
            ctx_arity(func, args, 2)?;
            expect_list(ctx, func, args[0])?;
            expect_list(ctx, func, args[1])?;
            if let Some((column, ab)) = packed_color(ctx, args[0])? {
                let mut out: Option<Vec<u8>> = None;
                ctx.with_packed(args[1], &mut |v, b| {
                    if color_view(v) && v.column == column && b.len() == ab.len() {
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
            ctx.with_packed(args[0], &mut |v, _| is_packed = color_view(v))?;
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
                let c = scale(read_color_at(ctx, func, args[0], i, &mut buf)?, s);
                out.push(ctx.make_object_like_element(args[0], i, &u8_fields(c))?);
            }
            Ok(CtxOut::Slot(ctx.make_list(&out)?))
        }
        _ => Err(crate::no_function_error("vec", func).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn element_ops_saturate() {
        // add saturates at 255, never wraps to a dark value.
        assert_eq!(add([200, 10, 0, 255], [100, 10, 0, 0]), [255, 20, 0, 255]);
        // sub saturates at 0, never wraps up.
        assert_eq!(sub([50, 100, 0, 255], [100, 10, 0, 255]), [0, 90, 0, 0]);
        // scale saturates.
        assert_eq!(scale([200, 50, 10, 255], 2.0), [255, 100, 20, 255]);
        assert_eq!(scale([200, 50, 10, 100], 0.5), [100, 25, 5, 50]);
    }

    #[test]
    fn bulk_kernels_match_element() {
        let a = [200u8, 10, 0, 255, 50, 100, 0, 255];
        let b = [100u8, 10, 0, 0, 100, 10, 0, 255];
        assert_eq!(add_buffers(&a, &b), [255, 20, 0, 255, 150, 110, 0, 255]);
        assert_eq!(sub_buffers(&a, &b), [100, 0, 0, 255, 0, 90, 0, 0]);
        assert_eq!(scale_buffer(&a, 2.0), [255, 20, 0, 255, 100, 200, 0, 255]);
        let mut m = a;
        scale_buffer_in_place(&mut m, 2.0);
        assert_eq!(m, scale_buffer(&a, 2.0)[..]);
    }
}
