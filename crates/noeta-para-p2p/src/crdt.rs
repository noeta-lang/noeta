//! `para.crdt` — the language surface over the [`noeta_crdt`] convergence core (p2p P0): the three
//! state-based CRDTs (`GCounter`, `PnCounter`, `GSet`) as first-class, **immutable value** extern
//! types, plus the `crdt` module that constructs them.
//!
//! Each type is a thin newtype around its [`noeta_crdt`] counterpart carrying the [`ExternValue`]
//! contract — the orphan rule forbids implementing the ABI's trait for the foreign core type
//! directly, exactly as [`crate::id::Uuid`] wraps `uuid::Uuid` (and exactly as a third-party
//! extension would wrap any foreign type). The values are **plain `Send` data** whose whole state
//! lives inside the box (a `BTreeMap`/`BTreeSet` of primitives), so they take the ordinary
//! value-in/value-out dispatch — no retained arena, no ctx seam. That is the whole point of keeping
//! P0's CRDTs primitive-state: convergence is provable with the simplest possible surface. (Value-
//! *carrying* CRDTs — an LWW register over an arbitrary language value, an OR-Set — need the arena
//! seam and arrive with the synced-signal machinery in a later slice.)
//!
//! Semantics mirror the core: updates return a **new** value (functional update), `merge` is pure
//! and order-independent, equality is by content, and there is no ordering or key capability. Both
//! backends run this identical dispatch, so the differential holds by construction.

use std::any::Any;
use std::cmp::Ordering;
use std::fmt;

use noeta_crdt::Mergeable;
use noeta_ext_abi::registry::{ExtFn, NativeOut, RetTy, SigType};
use noeta_ext_abi::{
    ErrorKind, ExternValue, Host, NativeValue, Scalar, StdError, arity_error, no_function_error,
    no_method_error, type_error,
};

pub const GCOUNTER_TYPE_NAME: &str = "GCounter";
pub const PNCOUNTER_TYPE_NAME: &str = "PnCounter";
pub const GSET_TYPE_NAME: &str = "GSet";

/// The CRDT types' qualified runtime identities — what
/// [`noeta_ext_abi::ExternValue::type_identity`] returns; registered under `para.crdt`.
pub const GCOUNTER_TYPE_IDENTITY: &str = "para.crdt.GCounter";
pub const PNCOUNTER_TYPE_IDENTITY: &str = "para.crdt.PnCounter";
pub const GSET_TYPE_IDENTITY: &str = "para.crdt.GSet";

/// The built-in traits every CRDT extern type declares (p2p P2): `Mergeable` — the convergence
/// capability that makes a value safe to sync, which the checker enforces as a `T: Mergeable` bound
/// on `synced_signal`. This is the extern-type analogue of a user type's `@derive`, seeded into the
/// checker's trait table from the registry.
pub const CRDT_TRAITS: &[&str] = &["Mergeable"];

const GCOUNTER_SIG: SigType = SigType::Named(GCOUNTER_TYPE_NAME);
const PNCOUNTER_SIG: SigType = SigType::Named(PNCOUNTER_TYPE_NAME);
const GSET_SIG: SigType = SigType::Named(GSET_TYPE_NAME);

// --- Constructors: the `crdt` module ------------------------------------------------------------

/// The `crdt` module functions — the three constructors, each a zero-arg factory for an empty CRDT.
pub const CRDT_FNS: &[ExtFn] = &[
    ExtFn {
        name: "gcounter",
        params: &[],
        ret: RetTy::Concrete(GCOUNTER_SIG),
    },
    ExtFn {
        name: "pncounter",
        params: &[],
        ret: RetTy::Concrete(PNCOUNTER_SIG),
    },
    ExtFn {
        name: "gset",
        params: &[],
        ret: RetTy::Concrete(GSET_SIG),
    },
];

pub fn crdt_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "gcounter" => {
            want_arity(func, args, 0)?;
            Ok(extern_out(GCounter(noeta_crdt::GCounter::new())))
        }
        "pncounter" => {
            want_arity(func, args, 0)?;
            Ok(extern_out(PnCounter(noeta_crdt::PnCounter::new())))
        }
        "gset" => {
            want_arity(func, args, 0)?;
            Ok(extern_out(GSet(noeta_crdt::GSet::new())))
        }
        _ => Err(no_function_error("crdt", func)),
    }
}

// --- GCounter -----------------------------------------------------------------------------------

/// A grow-only counter value (wraps [`noeta_crdt::GCounter`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GCounter(pub noeta_crdt::GCounter);

pub const GCOUNTER_METHODS: &[ExtFn] = &[
    // `increment(replica, by?)` — raise `replica`'s count by `by` (default 1). Grow-only, so `by`
    // must be non-negative; a decrement is only available on `PnCounter`.
    ExtFn {
        name: "increment",
        params: &[SigType::String, SigType::Optional(&SigType::Int)],
        ret: RetTy::Concrete(GCOUNTER_SIG),
    },
    ExtFn {
        name: "value",
        params: &[],
        ret: RetTy::Concrete(SigType::Int),
    },
    ExtFn {
        name: "merge",
        params: &[GCOUNTER_SIG],
        ret: RetTy::Concrete(GCOUNTER_SIG),
    },
];

fn gcounter_method_dispatch(
    recv: &mut dyn ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let c = downcast::<GCounter>(recv, GCOUNTER_TYPE_NAME)?;
    match method {
        "increment" => {
            want_arity_range(method, args, 1, 2)?;
            let replica = want_str(method, args, 0)?;
            let by = want_amount(method, args, 1)?;
            Ok(extern_out(GCounter(c.0.increment(replica, by))))
        }
        "value" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Int(clamp_u64(c.0.value()))))
        }
        "merge" => {
            want_arity(method, args, 1)?;
            let other = want_extern::<GCounter>(method, args, 0, GCOUNTER_TYPE_NAME)?;
            Ok(extern_out(GCounter(c.0.merge(&other.0))))
        }
        _ => Err(no_method_error(GCOUNTER_TYPE_NAME, method)),
    }
}

impl ExternValue for GCounter {
    fn type_identity(&self) -> &'static str {
        GCOUNTER_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<GCounter>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable
    }
    fn display(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(out, "<gcounter {}>", self.0.value())
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

// --- PnCounter ----------------------------------------------------------------------------------

/// A positive-negative counter value (wraps [`noeta_crdt::PnCounter`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PnCounter(pub noeta_crdt::PnCounter);

pub const PNCOUNTER_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "increment",
        params: &[SigType::String, SigType::Optional(&SigType::Int)],
        ret: RetTy::Concrete(PNCOUNTER_SIG),
    },
    ExtFn {
        name: "decrement",
        params: &[SigType::String, SigType::Optional(&SigType::Int)],
        ret: RetTy::Concrete(PNCOUNTER_SIG),
    },
    ExtFn {
        name: "value",
        params: &[],
        ret: RetTy::Concrete(SigType::Int),
    },
    ExtFn {
        name: "merge",
        params: &[PNCOUNTER_SIG],
        ret: RetTy::Concrete(PNCOUNTER_SIG),
    },
];

fn pncounter_method_dispatch(
    recv: &mut dyn ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let c = downcast::<PnCounter>(recv, PNCOUNTER_TYPE_NAME)?;
    match method {
        "increment" | "decrement" => {
            want_arity_range(method, args, 1, 2)?;
            let replica = want_str(method, args, 0)?;
            let by = want_amount(method, args, 1)?;
            let next = if method == "increment" {
                c.0.increment(replica, by)
            } else {
                c.0.decrement(replica, by)
            };
            Ok(extern_out(PnCounter(next)))
        }
        "value" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Int(c.0.value())))
        }
        "merge" => {
            want_arity(method, args, 1)?;
            let other = want_extern::<PnCounter>(method, args, 0, PNCOUNTER_TYPE_NAME)?;
            Ok(extern_out(PnCounter(c.0.merge(&other.0))))
        }
        _ => Err(no_method_error(PNCOUNTER_TYPE_NAME, method)),
    }
}

impl ExternValue for PnCounter {
    fn type_identity(&self) -> &'static str {
        PNCOUNTER_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<PnCounter>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0
    }
    fn display(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(out, "<pncounter {}>", self.0.value())
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

// --- GSet ---------------------------------------------------------------------------------------

/// A grow-only set value (wraps [`noeta_crdt::GSet`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GSet(pub noeta_crdt::GSet);

pub const GSET_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "insert",
        params: &[SigType::String],
        ret: RetTy::Concrete(GSET_SIG),
    },
    ExtFn {
        name: "contains",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Bool),
    },
    ExtFn {
        name: "len",
        params: &[],
        ret: RetTy::Concrete(SigType::Int),
    },
    ExtFn {
        name: "members",
        params: &[],
        ret: RetTy::Concrete(SigType::List(&SigType::String)),
    },
    ExtFn {
        name: "merge",
        params: &[GSET_SIG],
        ret: RetTy::Concrete(GSET_SIG),
    },
];

fn gset_method_dispatch(
    recv: &mut dyn ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let s = downcast::<GSet>(recv, GSET_TYPE_NAME)?;
    match method {
        "insert" => {
            want_arity(method, args, 1)?;
            let element = want_str(method, args, 0)?;
            Ok(extern_out(GSet(s.0.insert(element))))
        }
        "contains" => {
            want_arity(method, args, 1)?;
            let element = want_str(method, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Bool(s.0.contains(element))))
        }
        "len" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Int(s.0.len() as i64)))
        }
        "members" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::List(
                s.0.members().into_iter().map(NativeOut::Str).collect(),
            ))
        }
        "merge" => {
            want_arity(method, args, 1)?;
            let other = want_extern::<GSet>(method, args, 0, GSET_TYPE_NAME)?;
            Ok(extern_out(GSet(s.0.merge(&other.0))))
        }
        _ => Err(no_method_error(GSET_TYPE_NAME, method)),
    }
}

impl ExternValue for GSet {
    fn type_identity(&self) -> &'static str {
        GSET_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<GSet>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0
    }
    fn display(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(out, "<gset [{}]>", self.0.members().join(", "))
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

// --- Dynamic CRDT operations over boxed extern values (p2p P2, used by `para.synced`) ------------
//
// A synced signal holds a CRDT as a backend heap value it only sees as `&dyn ExternValue`; these
// recover the concrete type to merge/serialize it. Each is a small match over the three CRDTs —
// the one place that enumerates them for the sync engine, so adding a CRDT touches only here.

/// Merge two CRDT values of the **same** concrete type into a new value; `None` if they are
/// different types or not CRDTs (the checker's `Mergeable` bound makes the latter unreachable, but
/// a `dyn`-laundered value could still reach here, so it is a clean `None`, not a panic).
pub fn merge_dyn(
    current: &dyn ExternValue,
    delta: &dyn ExternValue,
) -> Option<Box<dyn ExternValue>> {
    if let (Some(a), Some(b)) = (
        current.as_any().downcast_ref::<GCounter>(),
        delta.as_any().downcast_ref::<GCounter>(),
    ) {
        return Some(Box::new(GCounter(a.0.merge(&b.0))));
    }
    if let (Some(a), Some(b)) = (
        current.as_any().downcast_ref::<PnCounter>(),
        delta.as_any().downcast_ref::<PnCounter>(),
    ) {
        return Some(Box::new(PnCounter(a.0.merge(&b.0))));
    }
    if let (Some(a), Some(b)) = (
        current.as_any().downcast_ref::<GSet>(),
        delta.as_any().downcast_ref::<GSet>(),
    ) {
        return Some(Box::new(GSet(a.0.merge(&b.0))));
    }
    None
}

/// Serialize a CRDT value to wire bytes for a peer; `None` if it is not a CRDT.
pub fn to_bytes_dyn(value: &dyn ExternValue) -> Option<Vec<u8>> {
    if let Some(g) = value.as_any().downcast_ref::<GCounter>() {
        return Some(g.0.to_bytes());
    }
    if let Some(p) = value.as_any().downcast_ref::<PnCounter>() {
        return Some(p.0.to_bytes());
    }
    if let Some(s) = value.as_any().downcast_ref::<GSet>() {
        return Some(s.0.to_bytes());
    }
    None
}

/// Decode a peer's wire bytes into a CRDT value of the **same concrete type** as `like` (a topic
/// carries one CRDT type); `None` if `like` is not a CRDT or the bytes are malformed/cross-type.
pub fn from_bytes_like(like: &dyn ExternValue, bytes: &[u8]) -> Option<Box<dyn ExternValue>> {
    if like.as_any().is::<GCounter>() {
        return noeta_crdt::GCounter::from_bytes(bytes)
            .map(|c| Box::new(GCounter(c)) as Box<dyn ExternValue>);
    }
    if like.as_any().is::<PnCounter>() {
        return noeta_crdt::PnCounter::from_bytes(bytes)
            .map(|c| Box::new(PnCounter(c)) as Box<dyn ExternValue>);
    }
    if like.as_any().is::<GSet>() {
        return noeta_crdt::GSet::from_bytes(bytes)
            .map(|c| Box::new(GSet(c)) as Box<dyn ExternValue>);
    }
    None
}

// --- Registration handles (referenced from `registry`'s `P2P_MODULES` / `P2P_TYPES`) ------------

/// The `ExtType` method-dispatch entry for each CRDT — paired with its `*_METHODS` table when the
/// type is registered.
pub const GCOUNTER_DISPATCH: noeta_ext_abi::registry::TypeDispatch = gcounter_method_dispatch;
pub const PNCOUNTER_DISPATCH: noeta_ext_abi::registry::TypeDispatch = pncounter_method_dispatch;
pub const GSET_DISPATCH: noeta_ext_abi::registry::TypeDispatch = gset_method_dispatch;

// --- Small argument helpers (the plain-dispatch ABI exposes only the error constructors) --------

/// Box an extern value as a dispatch result.
fn extern_out(value: impl ExternValue + 'static) -> NativeOut {
    NativeOut::Extern(noeta_ext_abi::ExternBox::new(value))
}

fn want_arity(func: &str, args: &[NativeValue], expected: usize) -> Result<(), StdError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(arity_error(func, expected, args.len()))
    }
}

/// Arity gate for a call with a trailing-optional parameter: `min..=max` arguments.
fn want_arity_range(
    func: &str,
    args: &[NativeValue],
    min: usize,
    max: usize,
) -> Result<(), StdError> {
    if (min..=max).contains(&args.len()) {
        Ok(())
    } else {
        // Report against the required count — the same shape the checker's arity gate uses.
        Err(arity_error(func, min, args.len()))
    }
}

fn want_str<'a>(func: &str, args: &'a [NativeValue], index: usize) -> Result<&'a str, StdError> {
    match args.get(index) {
        Some(NativeValue::Str(s)) => Ok(s),
        _ => Err(type_error(func, "string")),
    }
}

/// A grow-only increment amount at `index`, defaulting to 1 when omitted. Rejects a negative amount
/// (grow-only counters cannot go backward — use `PnCounter.decrement`).
fn want_amount(func: &str, args: &[NativeValue], index: usize) -> Result<u64, StdError> {
    let n = match args.get(index) {
        None => 1,
        Some(NativeValue::Scalar(Scalar::Int(n))) => *n,
        _ => return Err(type_error(func, "int")),
    };
    u64::try_from(n).map_err(|_| StdError {
        kind: ErrorKind::ArgType,
        message: format!("`{func}` amount must be non-negative, got {n}"),
    })
}

/// Downcast a method receiver to its concrete newtype.
fn downcast<'a, T: 'static>(
    recv: &'a mut dyn ExternValue,
    type_name: &str,
) -> Result<&'a T, StdError> {
    recv.as_any()
        .downcast_ref::<T>()
        .ok_or_else(|| type_error("method", type_name))
}

/// Read an extern argument at `index` and downcast it to `T` (its cloned box is owned here).
fn want_extern<T: Clone + 'static>(
    func: &str,
    args: &[NativeValue],
    index: usize,
    type_name: &str,
) -> Result<T, StdError> {
    match args.get(index) {
        Some(NativeValue::Extern(b)) => b
            .as_any()
            .downcast_ref::<T>()
            .cloned()
            .ok_or_else(|| type_error(func, type_name)),
        _ => Err(type_error(func, type_name)),
    }
}

/// Clamp a `u64` counter value into the `int` the surface returns (a grow-only counter can exceed
/// `i64::MAX` only after saturating there, which already means "effectively unbounded").
fn clamp_u64(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}
