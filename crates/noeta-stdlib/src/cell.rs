//! `std.cell` — a shared, mutable, identity-carrying box: `cell.new(v)` yields a `Cell<T>` whose
//! `get`/`set`/`update` read and replace the held value. The **Class-3 proving client**
//! (higher-order-abi H4): the first extension that owns language values *across* dispatches, and
//! deliberately the smallest possible one — reactive (H5) is this shape plus a dependency graph.
//!
//! The structural rule on display: the extern box ([`CellBox`]) carries only a plain [`Retained`]
//! id — the **value** lives in the backend's retained arena, where the refcount discipline, the
//! leak oracle, and the cycle collector can see it. Copying a `Cell` copies the id, so copies
//! alias the one box (reference semantics — the point of a cell); the arena entry is released at
//! program teardown, exactly like an undisposed signal's content.

use std::any::Any;
use std::cmp::Ordering;

use noeta_native::registry::{ExtFn, NativeOut, RetTy, SigType};
use noeta_native::{
    ctx_arity, no_function_error, CtxError, CtxOut, CtxResult, ExternValue, NativeCtx, Retained,
    Slot,
};

pub const CELL_TYPE_NAME: &str = "Cell";

const VAR_A: SigType = SigType::Var(0);

/// `cell.new(v: A) -> Cell<A>` — the module's one function.
pub const CELL_CTX_FNS: &[ExtFn] = &[ExtFn {
    name: "new",
    params: &[VAR_A],
    ret: RetTy::Concrete(SigType::Generic(CELL_TYPE_NAME, &[VAR_A])),
}];

/// The `Cell<A>` instance methods — all higher-order (they reach the retained arena; `update`
/// also calls a closure back), so they live in the ctx table.
pub const CELL_CTX_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "get",
        params: &[],
        ret: RetTy::Concrete(VAR_A),
    },
    ExtFn {
        name: "set",
        params: &[VAR_A],
        ret: RetTy::Concrete(SigType::Unit),
    },
    // `update(f)` applies `f` to the held value, stores the result, and returns it.
    ExtFn {
        name: "update",
        params: &[SigType::Fn(&[VAR_A], &VAR_A)],
        ret: RetTy::Concrete(VAR_A),
    },
];

/// The extern box: nothing but the arena id. Equality is **identity** (same box), the natural
/// semantics for a mutable reference cell — and all a plain-data box *can* compare, since the
/// held value is not in here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellBox {
    pub retained: Retained,
}

impl ExternValue for CellBox {
    fn type_name(&self) -> &'static str {
        CELL_TYPE_NAME
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<CellBox>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<cell>")
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

pub fn cell_ctx_dispatch(
    func: &str,
    ctx: &mut dyn NativeCtx,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match func {
        "new" => {
            ctx_arity(func, args, 1)?;
            let retained = ctx.retain(args[0])?;
            Ok(CtxOut::Out(NativeOut::Extern(noeta_native::ExternBox::new(
                CellBox { retained },
            ))))
        }
        _ => Err(no_function_error("cell", func).into()),
    }
}

pub fn cell_ctx_method_dispatch(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    let retained = cell_of(ctx, recv)?;
    match method {
        "get" => {
            ctx_arity(method, args, 0)?;
            Ok(CtxOut::Slot(ctx.retained_get(retained)?))
        }
        "set" => {
            ctx_arity(method, args, 1)?;
            ctx.retained_set(retained, args[0])?;
            Ok(CtxOut::Out(NativeOut::Unit))
        }
        "update" => {
            ctx_arity(method, args, 1)?;
            let current = ctx.retained_get(retained)?;
            // The closure may re-enter this cell (or create new ones) — the arena ops hold no
            // borrows across the call, so re-entrancy is structurally fine.
            let new = ctx.call(args[0], &[current])?;
            ctx.free(current);
            ctx.retained_set(retained, new)?;
            Ok(CtxOut::Slot(new))
        }
        _ => Err(noeta_native::no_method_error(CELL_TYPE_NAME, method).into()),
    }
}

/// The arena id riding inside a `Cell` receiver.
fn cell_of(ctx: &mut dyn NativeCtx, recv: Slot) -> CtxResult<Retained> {
    let mut retained = None;
    ctx.with_extern(recv, &mut |e| {
        retained = e.as_any().downcast_ref::<CellBox>().map(|c| c.retained);
    })?;
    Ok(retained.expect("a Cell receiver wraps a CellBox"))
}
