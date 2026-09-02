//! The `noeta-value` unit tests, moved verbatim out of `lib.rs` (audit-1 finding 8) so the
//! crate root's line-count ratchet measures the library, not its tests.

use noeta_object::{PackedKind, PackedSchema};

use super::*;
use noeta_object::ShapeKind;
use proptest::prelude::*;

/// The [`Value::NANBOX`] layout the JIT compiles against must reproduce this crate's own encoding
/// bit-for-bit — the whole point of exposing it as one source of truth. Round-trip the singletons
/// and a small int through both the safe API and the raw layout formulas the JIT uses.
#[test]
fn nanbox_layout_matches_the_value_encoding() {
    let l = Value::NANBOX;
    assert_eq!(l.unit_bits, Value::unit().0);
    assert_eq!(l.true_bits, Value::bool(true).0);
    assert_eq!(l.false_bits, Value::bool(false).0);
    assert_eq!(l.unbound_bits, Value::unbound().0);
    // Boxing a small int: `qnan | int_tag | (i & ptr_mask)` — the exact sequence the JIT emits.
    for i in [0i64, 1, -1, 42, -42, l.int_max, l.int_min] {
        let boxed = l.qnan | l.int_tag | (i as u64 & l.ptr_mask);
        assert_eq!(boxed, Value::int(i).0, "boxing {i}");
        // Unboxing: sign-extend the low 48 bits.
        let p = boxed & l.ptr_mask;
        let unboxed = ((p << 16) as i64) >> 16;
        assert_eq!(unboxed, i, "unboxing {i}");
    }
    // The small-int tag test the JIT inlines: `(bits & (sign|qnan|int_tag)) == (qnan|int_tag)`.
    let mask = l.sign_bit | l.qnan | l.int_tag;
    let want = l.qnan | l.int_tag;
    assert_eq!(Value::int(5).0 & mask, want, "5 reads as small int");
    assert_ne!(Value::bool(true).0 & mask, want, "true is not a small int");
    assert_ne!(Value::unit().0 & mask, want, "unit is not a small int");
    // The pointer test: `(bits & (sign|qnan)) == (sign|qnan)`.
    let pmask = l.sign_bit | l.qnan;
    assert_ne!(Value::int(5).0 & pmask, pmask, "small int is not a pointer");
    let s = Value::string("hi");
    assert_eq!(s.0 & pmask, pmask, "a heap string is a pointer");
    s.free();
}

/// Build a packed byte buffer from a sequence of `int` field values (each an 8-byte LE word) —
/// the byte-addressed form of the old `Vec<u64>` literals.
fn ibytes(vals: &[i64]) -> Vec<u8> {
    vals.iter()
        .flat_map(|v| (*v as u64).to_le_bytes())
        .collect()
}

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
fn tuple_display_type_name_and_projection() {
    // A heap tuple renders parenthesized with `repr` elements, names its
    // type, and projects by position. Freed at the end so miri sees no leak (it owns one
    // reference per element).
    let t = Value::tuple(vec![Value::int(1), Value::string("two"), Value::float(3.0)]);
    assert_eq!(t.display(), "(1, \"two\", 3.0)");
    assert_eq!(t.type_name(), "tuple");
    assert!(t.is_tuple());
    assert_eq!(t.tuple_field(0).unwrap().as_int(), Some(1));
    assert_eq!(t.tuple_field(1).unwrap().as_string().unwrap(), "two");
    assert!(t.tuple_field(3).is_none());
    // Release the sole reference: frees the tuple and its owned elements (the boxed string),
    // so miri sees no leak.
    t.release();
}

#[test]
fn packed_list_round_trips_and_frees() {
    // A flat `List<packed>` packs elements into raw words and materializes them on
    // demand. Exercise construct → len → packed_get → realize → equality → display → free so
    // miri checks the materialize/free paths for use-after-free or double-free.
    let shape = noeta_object::intern_shape(Shape::object(
        ShapeKind::Struct,
        "V",
        vec!["x".into(), "y".into()],
    ));
    let schema = noeta_object::intern_schema(PackedSchema {
        shape: Some(shape),
        fields: vec![PackedKind::Int, PackedKind::Int],
        byte_size: 16,
        column: false,
    });

    // Pack two `V { x, y }` instances into one flat buffer (the source objects are freed after).
    let mut bytes = Vec::new();
    for (x, y) in [(3_i64, 1_i64), (1, 2)] {
        let obj = Value::object(shape, vec![Value::int(x), Value::int(y)]);
        assert!(obj.pack_element(schema, &mut bytes));
        obj.release();
    }
    assert_eq!(bytes.len(), 32); // 2 elements × 2 int fields × 8 bytes

    let list = Value::packed_list(schema, bytes);
    assert!(list.is_packed_list());
    assert!(list.is_list());
    assert_eq!(list.list_len(), Some(2));
    assert_eq!(list.type_name(), "list");

    // A single element materializes to an owned object, shape-identical to a constructed one.
    let first = list.packed_get(0);
    assert_eq!(first.display(), "V {x: 3, y: 1}");
    let constructed = Value::object(shape, vec![Value::int(3), Value::int(1)]);
    assert!(
        apply_binary(noeta_ast::BinaryOp::Eq, first, constructed)
            .unwrap()
            .as_bool()
            .unwrap()
    );
    first.release();
    constructed.release();

    // The whole list displays and compares as the boxed equivalent.
    assert_eq!(list.display(), "[V {x: 3, y: 1}, V {x: 1, y: 2}]");
    let boxed = Value::list(vec![
        Value::object(shape, vec![Value::int(3), Value::int(1)]),
        Value::object(shape, vec![Value::int(1), Value::int(2)]),
    ]);
    assert!(
        apply_binary(noeta_ast::BinaryOp::Eq, list, boxed)
            .unwrap()
            .as_bool()
            .unwrap()
    );
    boxed.release();

    // Release the packed list (a leaf — frees the buffer, no child release), so miri sees no leak.
    list.release();
}

#[test]
fn packed_push_streams_in_place_and_frees() {
    // Streaming construction — start from an empty packed list and `packed_push` each
    // element in place (the in-place path the VM uses). `packed_push` reads the element through
    // the heap while mutating the list through the heap (two distinct objects), so this exercises
    // that nested `with_payload_mut`/`with_payload` access for use-after-free under miri, and
    // confirms the element object is freed by the caller after its primitives are copied.
    let shape = noeta_object::intern_shape(Shape::object(
        ShapeKind::Struct,
        "V",
        vec!["x".into(), "y".into()],
    ));
    let schema = noeta_object::intern_schema(PackedSchema {
        shape: Some(shape),
        fields: vec![PackedKind::Int, PackedKind::Int],
        byte_size: 16,
        column: false,
    });

    let list = Value::packed_list(schema, Vec::new());
    assert_eq!(list.list_len(), Some(0));
    for (x, y) in [(3_i64, 1_i64), (1, 2), (7, 9)] {
        let obj = Value::object(shape, vec![Value::int(x), Value::int(y)]);
        assert!(list.packed_push(obj)); // primitives copied into the buffer (no retain)
        obj.release(); // the caller still owns the element — free it (its data is now in `words`)
    }
    assert_eq!(list.list_len(), Some(3));
    assert_eq!(
        list.display(),
        "[V {x: 3, y: 1}, V {x: 1, y: 2}, V {x: 7, y: 9}]"
    );
    list.release();

    // The demote fall-back: push onto a boxed list in place (the caller hands over one reference).
    let boxed = Value::list(Vec::new());
    let elem = Value::object(shape, vec![Value::int(5), Value::int(6)]);
    boxed.list_push(elem); // boxed now owns the reference handed over
    assert_eq!(boxed.list_len(), Some(1));
    assert_eq!(boxed.display(), "[V {x: 5, y: 6}]");
    boxed.release(); // frees the list and its owned element, so miri sees no leak
}

#[test]
fn packed_column_layout_round_trips_and_frees() {
    // A `@packed(Layout.Column)` list stores each field contiguously across elements
    // (`[x×n][y×n]`) yet observes identically to a row list. Build it by streaming push (exercising
    // `column_append`'s O(n) rebuild), then check get / field / select / set / concat all read the
    // right values through the column offset math — and free clean under miri. Mixed field widths
    // (`int` 8 + `bool` 1) stress the non-uniform column strides.
    let shape = noeta_object::intern_shape(Shape::object(
        ShapeKind::Struct,
        "P",
        vec!["v".into(), "flag".into()],
    ));
    let schema = noeta_object::intern_schema(PackedSchema {
        shape: Some(shape),
        fields: vec![PackedKind::Int, PackedKind::Bool],
        byte_size: 9,
        column: true,
    });

    // Stream three elements in — each `column_append` rebuilds the buffer in column order.
    let list = Value::packed_list(schema, Vec::new());
    for (v, flag) in [(10_i64, true), (20, false), (30, true)] {
        let obj = Value::object(shape, vec![Value::int(v), Value::bool(flag)]);
        assert!(list.packed_push(obj));
        obj.release();
    }
    assert_eq!(list.list_len(), Some(3));
    // Buffer is column-major: `[v0 v1 v2 (24 bytes)][flag0 flag1 flag2 (3 bytes)]`.
    let raw = list.packed_bytes().unwrap();
    assert_eq!(raw.len(), 27);
    assert_eq!(&raw[0..8], &10_i64.to_le_bytes());
    assert_eq!(&raw[8..16], &20_i64.to_le_bytes());
    assert_eq!(&raw[24..27], &[1u8, 0, 1]); // the bool column, contiguous

    // Per-element gather and per-field read both resolve through the column offsets.
    let mid = list.packed_get(1);
    assert_eq!(mid.display(), "P {v: 20, flag: false}");
    mid.release();
    assert_eq!(list.packed_field(2, "v").unwrap().as_int(), Some(30));
    assert_eq!(list.packed_field(0, "flag").unwrap().as_bool(), Some(true));

    // Whole-list display equals the boxed equivalent (materialize gathers every column).
    assert_eq!(
        list.display(),
        "[P {v: 10, flag: true}, P {v: 20, flag: false}, P {v: 30, flag: true}]"
    );

    // `select` (reverse) and `set` keep the column layout and the right values.
    let rev = list.packed_select(&[2, 1, 0]);
    assert_eq!(
        rev.display(),
        "[P {v: 30, flag: true}, P {v: 20, flag: false}, P {v: 10, flag: true}]"
    );
    rev.release();

    let repl = Value::object(shape, vec![Value::int(99), Value::bool(false)]);
    let set = list.packed_set(1, repl).unwrap();
    assert_eq!(
        set.display(),
        "[P {v: 10, flag: true}, P {v: 99, flag: false}, P {v: 30, flag: true}]"
    );
    repl.release();
    set.release();

    // `concat` of two column lists interleaves per column.
    let other = Value::packed_list(schema, Vec::new());
    let tail = Value::object(shape, vec![Value::int(40), Value::bool(false)]);
    assert!(other.packed_push(tail));
    tail.release();
    let joined = list.packed_concat(other).unwrap();
    assert_eq!(joined.list_len(), Some(4));
    assert_eq!(joined.packed_field(3, "v").unwrap().as_int(), Some(40));
    let head = joined.packed_get(0);
    assert_eq!(head.display(), "P {v: 10, flag: true}");
    head.release();
    joined.release();
    other.release();

    list.release(); // a leaf — frees the column buffer, no child release; miri sees no leak
}

#[test]
fn future_wraps_and_frees_its_step() {
    // Track A: a future owns one reference to its thunk/step closure (a list stands in for the
    // closure here). Constructing retains the step; `future_step` hands back a fresh retained
    // reference; releasing the future releases its held reference — all leak-clean under miri.
    let step = Value::list(vec![Value::int(1), Value::int(2)]);
    let fut = Value::make_future(step); // the future retains a second reference to `step`
    assert!(fut.is_future());
    assert_eq!(fut.type_name(), "future");
    assert_eq!(fut.display(), "<future>");

    let got = fut.future_step().expect("a future yields its step");
    assert_eq!(got.display(), "[1, 2]"); // a freshly-retained reference to the wrapped step
    got.release(); // drop the borrowed step reference

    step.release(); // drop the caller's original reference; the future still holds one
    fut.release(); // frees the future and its held step reference → step freed, no leak
}

#[test]
fn timer_carries_its_deadline_and_frees_cleanly() {
    // Track A.2: a leaf timer future carries only its integer deadline (no heap children), names
    // itself "future", displays opaquely, and frees as a plain node — leak-clean under miri.
    let timer = Value::make_timer(42);
    assert!(timer.is_future());
    assert!(timer.is_timer());
    assert!(!timer.is_future() || timer.future_step().is_none()); // a timer wraps no closure
    assert_eq!(timer.timer_deadline(), Some(42));
    assert_eq!(timer.type_name(), "future");
    assert_eq!(timer.display(), "<future>");
    timer.release(); // frees the node, no leak
}

#[test]
fn handle_carries_its_indices_and_frees_cleanly() {
    // Track A.3b: a task handle is a GC leaf holding `(scope, task)` indices; it names itself
    // "future", displays opaquely, and frees as a plain node — leak-clean under miri.
    let handle = Value::make_handle(ScopeId::from_index(2), TaskId::from_index(5));
    assert!(handle.is_future());
    assert!(handle.is_handle());
    assert_eq!(
        handle.handle_parts(),
        Some((ScopeId::from_index(2), TaskId::from_index(5)))
    );
    assert_eq!(handle.type_name(), "future");
    assert_eq!(handle.display(), "<future>");
    handle.release(); // frees the node, no leak
}

#[test]
fn async_io_carries_its_ticket_and_frees_cleanly() {
    // Track A.4c/A.10: a leaf async-IO future is a GC leaf holding the executor ticket id; it names
    // itself "future", displays opaquely, and frees as a plain node — leak-clean under miri.
    let io = Value::make_async_io(7);
    assert!(io.is_future());
    assert_eq!(io.async_io_id(), Some(7));
    assert!(io.future_step().is_none()); // it wraps no closure
    assert!(!io.is_timer() && !io.is_handle());
    assert_eq!(io.type_name(), "future");
    assert_eq!(io.display(), "<future>");
    io.release(); // frees the node, no leak
}

#[test]
fn channel_endpoints_are_leaves_and_free_cleanly() {
    // Isolates I.1: a sender/receiver endpoint is a GC leaf holding a channel id; it names itself
    // "sender"/"receiver", displays opaquely, and frees as a plain node — leak-clean under miri.
    let tx = Value::make_sender(ChannelId::from_index(3));
    assert_eq!(tx.sender_id(), Some(ChannelId::from_index(3)));
    assert_eq!(tx.receiver_id(), None);
    assert_eq!(tx.type_name(), "sender");
    assert_eq!(tx.display(), "<sender>");
    tx.release();

    let rx = Value::make_receiver(ChannelId::from_index(3));
    assert_eq!(rx.receiver_id(), Some(ChannelId::from_index(3)));
    assert_eq!(rx.type_name(), "receiver");
    assert_eq!(rx.display(), "<receiver>");
    rx.release();

    // Isolates I.4b: an isolate-result future is a GC leaf carrying a backend isolate-table id;
    // it reports as a "future", displays opaquely, and frees as a plain node.
    let h = Value::make_isolate_future(9);
    assert_eq!(h.isolate_future_id(), Some(9));
    assert!(h.is_future());
    assert_eq!(h.type_name(), "future");
    assert_eq!(h.display(), "<future>");
    h.release();
}

#[test]
fn channel_send_future_owns_its_message_and_frees_cleanly() {
    // Isolates I.1: a channel-send future is a future GC node owning one reference to its message
    // (like `make_future`'s closure). Dropping the future must release that reference — no leak,
    // no double-free — which miri verifies. `channel_send_parts` hands back a fresh owning ref.
    let msg = Value::string("hi"); // refcount 1
    let fut = Value::make_channel_send(ChannelId::from_index(5), msg); // retains msg → refcount 2
    assert!(fut.is_future());
    msg.release(); // drop the caller's original reference → refcount 1 (the future's)
    let (id, borrowed) = fut.channel_send_parts().expect("a channel-send future");
    assert_eq!(id, ChannelId::from_index(5));
    borrowed.release(); // channel_send_parts retained a fresh ref; balance it
    fut.release(); // frees the future and its remaining message reference — no leak

    let recv = Value::make_channel_recv(ChannelId::from_index(5));
    assert!(recv.is_future());
    assert_eq!(recv.channel_recv_id(), Some(ChannelId::from_index(5)));
    recv.release();
}

/// Build a `Point { x: int; y: int }` struct value on a shared shape (helper for the I.3 tests).
fn point(shape: &&'static Shape, x: i64, y: i64) -> Value {
    Value::object(shape, vec![Value::int(x), Value::int(y)])
}

#[test]
fn shared_object_retain_release_are_noops() {
    // Isolates I.3: a promoted (borrow-shared) object's refcount is never written — `inc_ref`/
    // `release` no-op on it, so a storm of them across "isolates" touches no count and frees
    // nothing. The region alone owns it and frees it wholesale. miri verifies no use-after-free.
    let base = live_count();
    let shape = noeta_object::intern_shape(Shape::object(
        ShapeKind::Struct,
        "Point",
        vec!["x".into(), "y".into()],
    ));
    let local = point(&shape, 3, 4); // an ordinary local object, refcount 1

    let mut region = SharedRegion::new();
    let shared = region.promote(local);
    assert!(shared.is_shared());
    assert!(!local.is_shared()); // the original stays local — promotion is a copy, not a move
    assert_eq!(shared.refcount(), 1);

    // Simulate N isolates each borrowing the shared object: retain/release many times. On a shared
    // object these are no-ops, so the count stays put and the object is never reclaimed.
    for _ in 0..1000 {
        shared.inc_ref();
        shared.release();
    }
    assert_eq!(
        shared.refcount(),
        1,
        "a shared object's count is never written"
    );
    // Still fully readable after the storm (never freed).
    assert_eq!(shared.display(), "Point {x: 3, y: 4}");

    local.release(); // the local original frees normally (refcount → 0)
    region.free_all(); // the region frees the shared graph wholesale
    assert_eq!(live_count(), base, "region balance: promoted N, freed N");
}

#[test]
fn shared_region_deep_copies_and_is_independent() {
    // Isolates I.3: promotion deep-copies the whole graph into shared objects. The copy equals the
    // original structurally, yet is independent — freeing the entire original leaves the shared
    // copy intact and readable (it shares no allocation with the original). Then `free_all` returns
    // the live count exactly to baseline (leak-clean, both allocations balanced).
    let base = live_count();
    let shape = noeta_object::intern_shape(Shape::object(
        ShapeKind::Struct,
        "Point",
        vec!["x".into(), "y".into()],
    ));
    // A nested value graph: a list of two structs, each holding boxed ints (and the list itself).
    let original = Value::list(vec![point(&shape, 1, 2), point(&shape, 3, 4)]);

    let mut region = SharedRegion::new();
    let shared = region.promote(original);
    assert_ne!(
        shared.bits(),
        original.bits(),
        "a copy, not the same allocation"
    );
    assert!(shared.is_shared());
    assert!(
        apply_binary(noeta_ast::BinaryOp::Eq, shared, original)
            .unwrap()
            .as_bool()
            .unwrap(),
        "the promoted copy equals the original structurally"
    );

    // Free the entire original graph. If promotion had aliased any of its allocations, the shared
    // copy would now dangle — miri would catch a use-after-free on the read below.
    original.release();
    assert_eq!(
        shared.display(),
        "[Point {x: 1, y: 2}, Point {x: 3, y: 4}]",
        "the shared copy is unaffected by freeing the original"
    );

    region.free_all();
    assert_eq!(live_count(), base);
}

#[test]
fn promotion_preserves_dag_sharing() {
    // Isolates I.3: when the same object is reachable by two paths (a DAG — value semantics let two
    // slots hold one allocation), promotion copies it **once** via its memo, so the promoted graph
    // preserves that sharing rather than duplicating. Freeing the region then frees the shared node
    // exactly once — miri would flag a double-free otherwise.
    let base = live_count();
    let shape = noeta_object::intern_shape(Shape::object(ShapeKind::Struct, "P", vec!["x".into()]));
    let p = Value::object(shape, vec![Value::int(7)]); // refcount 1
    // A tuple *transfers* one reference per slot in, so to alias `p` in both slots we owe it two
    // references: bump the count once more, then hand both to the tuple (refcount 2).
    p.inc_ref();
    let pair = Value::tuple(vec![p, p]); // both slots point at the SAME object → refcount 2

    let mut region = SharedRegion::new();
    let shared = region.promote(pair);
    let a = shared.tuple_field(0).unwrap();
    let b = shared.tuple_field(1).unwrap();
    assert_eq!(
        a.bits(),
        b.bits(),
        "the shared subgraph is promoted once, not twice"
    );
    assert!(a.is_shared());
    // Region holds: the tuple + the single shared `P` (deduped) = 2 objects.
    assert_eq!(region.len(), 2);

    pair.release();
    region.free_all();
    assert_eq!(live_count(), base);
}

#[test]
fn promote_passes_immediates_through() {
    // Isolates I.3: an immediate (a small int) carries no refcount and no heap identity, so
    // promoting it returns it unchanged and adds nothing to the region.
    let mut region = SharedRegion::new();
    let promoted = region.promote(Value::int(42));
    assert_eq!(promoted.as_int(), Some(42));
    assert!(!promoted.is_shared());
    assert!(region.is_empty());
    region.free_all();
}

#[test]
fn promote_with_memo_dedups_across_calls() {
    // The spawn path keeps one memo across every `isolate f(corpus)` in flight, so
    // fanning one corpus to N workers promotes once — the second call returns the same
    // promoted root and the region grows by nothing.
    let base = live_count();
    let corpus = Value::list(vec![Value::string("a"), Value::string("b")]);
    let mut region = SharedRegion::new();
    let mut memo = std::collections::HashMap::new();
    let first = region.promote_with(corpus, &mut memo);
    let after_first = region.len();
    let second = region.promote_with(corpus, &mut memo);
    assert_eq!(
        first.bits(),
        second.bits(),
        "memo hit returns the same root"
    );
    assert_eq!(region.len(), after_first, "a memo hit adds nothing");
    corpus.release();
    region.free_all();
    assert_eq!(live_count(), base);
}

#[test]
fn shared_values_are_never_uniquely_owned() {
    // The COW gate: a shared object's refcount is frozen at 1, so the in-place
    // mutation fast paths must consult `is_uniquely_owned`, which excludes it — a worker
    // "mutating" a borrowed corpus copies instead of touching the shared buffer.
    let base = live_count();
    let local = Value::list(vec![Value::int(1)]);
    assert!(local.is_uniquely_owned());
    let mut region = SharedRegion::new();
    let shared = region.promote(local);
    assert_eq!(
        shared.refcount(),
        1,
        "a shared object's count is frozen at 1"
    );
    assert!(
        !shared.is_uniquely_owned(),
        "shared must never pass the COW uniqueness gate"
    );
    local.release();
    region.free_all();
    assert_eq!(live_count(), base);
}

#[test]
fn promotable_graph_accepts_data_and_rejects_functions() {
    // Promotability = Send *data* kinds. A closure is Wire-shippable but has no
    // promoted form, so a graph containing one falls back to the copy path.
    let list = Value::list(vec![Value::int(1), Value::string("x")]);
    assert!(list.is_promotable_graph());
    let f = Value::closure(0, Vec::new());
    assert!(!f.is_promotable_graph());
    let holding = Value::list(vec![f]);
    f.inc_ref(); // the list owns one reference; keep ours for the assert below
    assert!(!holding.is_promotable_graph());
    holding.release();
    f.release();
    list.release();
}

#[test]
fn f32_is_immediate_and_round_trips() {
    // An `f32` is an *immediate* NaN-boxed value — no heap allocation, not
    // refcounted (like an immediate int/float). Round-trips through `as_f32`, names itself `f32`,
    // and is distinguishable from every other immediate.
    let v = Value::f32(1.5);
    assert!(!v.is_pointer(), "f32 must be immediate, not a heap pointer");
    assert_eq!(v.as_f32(), Some(1.5));
    assert!(v.is_f32());
    assert_eq!(v.as_float(), None); // not an f64 float
    assert_eq!(v.as_int(), None); // not an int
    assert_eq!(v.as_bool(), None);
    assert!(!v.is_unit());
    assert_eq!(v.type_name(), "f32");
    assert_eq!(v.display(), "1.5");

    // F32 precision is observable: 0.1 + 0.2 at f32 is exactly 0.3 (f64 would be 0.30000…04).
    assert_eq!(Value::f32(0.1 + 0.2_f32).display(), "0.3");
    // Bit patterns round-trip exactly, including the awkward ones.
    for f in [0.0f32, -0.0, 1.0, -2.5, f32::MAX, f32::MIN, f32::EPSILON] {
        assert_eq!(Value::f32(f).as_f32(), Some(f), "round-trip {f}");
    }
    // An immediate int and an immediate f32 with the same numeric value are distinct values.
    assert_ne!(Value::f32(3.0).0, Value::int(3).0);
}

#[test]
fn packed_f32_byte_layout_round_trips() {
    // An `f32` packed field is 4 bytes, an `int` 8 — a mixed `{f32, int}` element is
    // 12 bytes, exercising unaligned byte offsets (the int starts at byte 4). Pack two, then read
    // each field back: the f32 keeps f32 precision and the int its full value. Checked under miri.
    let shape = noeta_object::intern_shape(Shape::object(
        ShapeKind::Struct,
        "P",
        vec!["a".into(), "b".into()],
    ));
    let schema = noeta_object::intern_schema(PackedSchema {
        shape: Some(shape),
        fields: vec![PackedKind::F32, PackedKind::Int],
        byte_size: 12,
        column: false,
    });
    let mut bytes = Vec::new();
    for (a, b) in [(0.1f32 + 0.2, 7_i64), (-1.5, 1_000_000)] {
        let obj = Value::object(shape, vec![Value::f32(a), Value::int(b)]);
        assert!(obj.pack_element(schema, &mut bytes));
        obj.release();
    }
    assert_eq!(bytes.len(), 24); // 2 elements × 12 bytes

    let list = Value::packed_list(schema, bytes);
    assert_eq!(list.list_len(), Some(2));
    assert_eq!(
        list.display(),
        "[P {a: 0.3, b: 7}, P {a: -1.5, b: 1000000}]"
    );
    // Fused single-field reads land on the right byte offsets.
    assert_eq!(list.packed_field(0, "a").and_then(Value::as_f32), Some(0.3));
    assert_eq!(list.packed_field(0, "b").and_then(Value::as_int), Some(7));
    assert_eq!(
        list.packed_field(1, "a").and_then(Value::as_f32),
        Some(-1.5)
    );
    assert_eq!(
        list.packed_field(1, "b").and_then(Value::as_int),
        Some(1_000_000)
    );
    list.release();
}

#[test]
fn packed_field_reads_one_field_without_materializing() {
    // The fused `list[i].field` read decodes a single field's word, returning an
    // owned primitive (or `None` for an out-of-range index / unknown field). Exercised under miri
    // to confirm the targeted slice read borrows the buffer correctly and leaks nothing.
    let shape = noeta_object::intern_shape(Shape::object(
        ShapeKind::Struct,
        "V",
        vec!["x".into(), "y".into()],
    ));
    let schema = noeta_object::intern_schema(PackedSchema {
        shape: Some(shape),
        fields: vec![PackedKind::Int, PackedKind::Int],
        byte_size: 16,
        column: false,
    });
    // Two elements: (3, 1) and (7, 9).
    let list = Value::packed_list(schema, ibytes(&[3, 1, 7, 9]));

    assert_eq!(list.packed_field(0, "x").and_then(Value::as_int), Some(3));
    assert_eq!(list.packed_field(0, "y").and_then(Value::as_int), Some(1));
    assert_eq!(list.packed_field(1, "x").and_then(Value::as_int), Some(7));
    assert_eq!(list.packed_field(1, "y").and_then(Value::as_int), Some(9));
    // Out of range and unknown field both decline (the caller then falls back).
    assert!(list.packed_field(2, "x").is_none());
    assert!(list.packed_field(0, "z").is_none());

    list.release();
}

#[test]
fn packed_select_keeps_the_list_flat() {
    // `packed_select` rebuilds a flat buffer from chosen element word-blocks (the
    // selection producers reverse/slice/filter). The result is still a packed list (no demote) and
    // owns no child refs, so it frees cleanly — checked under miri.
    let shape = noeta_object::intern_shape(Shape::object(
        ShapeKind::Struct,
        "V",
        vec!["x".into(), "y".into()],
    ));
    let schema = noeta_object::intern_schema(PackedSchema {
        shape: Some(shape),
        fields: vec![PackedKind::Int, PackedKind::Int],
        byte_size: 16,
        column: false,
    });
    // Three elements: (3,1), (1,2), (7,9).
    let list = Value::packed_list(schema, ibytes(&[3, 1, 1, 2, 7, 9]));

    // Reverse-order selection yields a flat packed list with the blocks reordered.
    let reversed = list.packed_select(&[2, 1, 0]);
    assert!(reversed.is_packed_list());
    assert_eq!(reversed.list_len(), Some(3));
    assert_eq!(
        reversed.display(),
        "[V {x: 7, y: 9}, V {x: 1, y: 2}, V {x: 3, y: 1}]"
    );
    reversed.release();

    // A subset selection (a filter/slice keeping a prefix) stays flat too.
    let kept = list.packed_select(&[0, 2]);
    assert!(kept.is_packed_list());
    assert_eq!(kept.display(), "[V {x: 3, y: 1}, V {x: 7, y: 9}]");
    kept.release();

    list.release();
}

#[test]
fn packed_set_and_concat_keep_the_list_flat() {
    // `set`/`~` on a packed list stay flat. `packed_set` (copy) and
    // `packed_set_in_place` (sole-owner overwrite) replace one element's words; `packed_concat`
    // (copy) and `packed_extend_in_place` (sole-owner append) join same-layout buffers. All
    // results are still packed lists owning no child refs — checked under miri.
    let shape = noeta_object::intern_shape(Shape::object(
        ShapeKind::Struct,
        "V",
        vec!["x".into(), "y".into()],
    ));
    let schema = noeta_object::intern_schema(PackedSchema {
        shape: Some(shape),
        fields: vec![PackedKind::Int, PackedKind::Int],
        byte_size: 16,
        column: false,
    });
    let mk = |vals: &[i64]| Value::packed_list(schema, ibytes(vals));
    let elem = |x: i64, y: i64| Value::object(shape, vec![Value::int(x), Value::int(y)]);

    // Functional `set`: a fresh flat list with one element replaced; the original is untouched.
    let list = mk(&[1, 1, 2, 2, 3, 3]);
    let nine = elem(9, 9);
    let updated = list.packed_set(1, nine).expect("packs");
    nine.release();
    assert!(updated.is_packed_list());
    assert_eq!(
        updated.display(),
        "[V {x: 1, y: 1}, V {x: 9, y: 9}, V {x: 3, y: 3}]"
    );
    assert_eq!(
        list.display(),
        "[V {x: 1, y: 1}, V {x: 2, y: 2}, V {x: 3, y: 3}]"
    );
    updated.release();

    // In-place `set` (sole owner): overwrite one block, the list keeps its single reference.
    let eight = elem(8, 8);
    assert!(list.packed_set_in_place(0, eight));
    eight.release();
    assert!(list.is_packed_list());
    assert_eq!(
        list.display(),
        "[V {x: 8, y: 8}, V {x: 2, y: 2}, V {x: 3, y: 3}]"
    );

    // Functional concat of same-layout lists → a flat join; both inputs survive.
    let other = mk(&[7, 7]);
    let joined = list.packed_concat(other).expect("same layout");
    assert!(joined.is_packed_list());
    assert_eq!(joined.list_len(), Some(4));
    assert_eq!(other.list_len(), Some(1)); // input unchanged
    joined.release();

    // In-place extend (sole owner): append `other`'s words; `other` is borrowed (still owned).
    assert!(list.packed_extend_in_place(other));
    assert!(list.is_packed_list());
    assert_eq!(
        list.display(),
        "[V {x: 8, y: 8}, V {x: 2, y: 2}, V {x: 3, y: 3}, V {x: 7, y: 7}]"
    );
    other.release();
    list.release();
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

/// The three properties the map store's hasher must have, none of which the type system states.
///
/// 1. **Probe equivalence.** A bare `&str` must hash exactly as the `MapKey::Str` holding it, or
///    the zero-allocation heterogeneous lookup silently misses every key.
/// 2. **Length disambiguation.** The word-at-a-time `write` zero-pads its tail, so a string and
///    that string followed by NULs must still differ.
/// 3. **Bucket-index spread.** hashbrown indexes on the LOW bits; a multiply's low bits are the
///    weak end, which is what `finish`'s fold exists to fix. Realistic key families must not pile
///    into a few buckets.
#[test]
fn map_hasher_matches_the_str_probe_disambiguates_length_and_spreads() {
    use std::hash::BuildHasher as _;
    let build = std::hash::BuildHasherDefault::<crate::heap::FxHasher>::default();
    let hash_of = |s: &str| build.hash_one(s);

    for s in [
        "",
        "a",
        "key",
        "key12345",
        "key123456",
        "a much longer key than one machine word",
        "\0",
        "with\0embedded\0nuls",
    ] {
        assert_eq!(
            hash_of(s),
            build.hash_one(noeta_ext_abi::MapKey::from(s)),
            "the &str probe must hash as the stored MapKey for {s:?}"
        );
    }

    assert_ne!(
        hash_of("ab"),
        hash_of("ab\0"),
        "zero padding must not alias"
    );
    assert_ne!(hash_of("ab\0"), hash_of("ab\0\0\0\0\0\0\0"));

    // Spread: the `assoc`/`wordcount` key families, bucketed as hashbrown would bucket them.
    for keys in [
        (0..4096).map(|i| format!("key{i}")).collect::<Vec<_>>(),
        (0..4096).map(|i| format!("word{i}")).collect::<Vec<_>>(),
        (0..4096).map(|i| format!("{i}")).collect::<Vec<_>>(),
    ] {
        let mut buckets = [0u32; 512];
        for k in &keys {
            buckets[(hash_of(k) & 511) as usize] += 1;
        }
        let worst = buckets.iter().copied().max().unwrap_or(0);
        // 4096 keys over 512 buckets averages 8; a well-spread hash stays well under 4x that.
        assert!(
            worst < 32,
            "hash piles up: worst bucket held {worst} of 4096 keys ({:?}…)",
            keys[0]
        );
    }
}

/// The acyclic exclusion, stated as a test: a **leaf** payload never enters the cycle collector's
/// live-object registry (nothing can reach it through a chain of references that returns to it), a
/// **node** payload always does, and both are counted by the leak oracle's `live_count` either way —
/// so the exclusion buys allocation bookkeeping without blinding the residency check.
#[test]
fn acyclic_leaves_stay_out_of_the_registry_but_still_count_as_live() {
    let registered = |v: Value| live_objects().iter().any(|r| r.bits() == v.bits());

    let before = live_count();
    // Leaves: a string, a bytes buffer, a boxed (out-of-immediate-range) int.
    let leaves = [
        Value::string("no cycle can ever reach me"),
        Value::bytes(vec![1, 2, 3]),
        Value::int(i64::MAX),
    ];
    for leaf in leaves {
        assert!(leaf.is_pointer(), "the fixture must be heap-allocated");
        assert!(!registered(leaf), "a leaf must not be registered");
    }
    // Nodes: anything that owns a child value, so anything that can close a cycle.
    let nodes = [
        Value::list(vec![Value::int(1)]),
        Value::cell(Value::unit()),
        Value::closure(0, Vec::new()),
        Value::map(BTreeMap::from([("k".to_string(), Value::int(1))])),
    ];
    for node in nodes {
        assert!(registered(node), "a node must be registered");
    }
    // Registered or not, every heap object is counted — residency is the oracle, not the registry.
    assert_eq!(live_count(), before + leaves.len() + nodes.len());

    for v in leaves.into_iter().chain(nodes) {
        v.release();
    }
    assert_eq!(live_count(), before, "everything reclaimed");
}

#[test]
fn closures_round_trip_and_free() {
    let v = Value::closure(7, Vec::new());
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
fn lists_display_with_repr_and_free_their_elements() {
    // The list owns one reference to each element; building it from retained values and
    // then freeing it must release them (miri verifies no leak and no double-free).
    let a = Value::string("a");
    let items = vec![Value::int(1), a, Value::int(3)];
    let list = Value::list(items);
    assert_eq!(list.type_name(), "list");
    // Strings are quoted inside a collection; bare ints are not.
    assert_eq!(list.display(), "[1, \"a\", 3]");
    assert_eq!(list.list_len(), Some(3));
    assert!(list.dec_ref());
    list.free();
}

#[test]
fn cells_box_and_update_their_contents() {
    let cell = Value::cell(Value::int(1));
    assert!(cell.is_cell());
    assert_eq!(cell.cell_get().as_int(), Some(1));
    // `cell_set` retains the new occupant for the cell and releases the old; the caller still
    // owns its own reference (as a VM register would), so release it here.
    let s = Value::string("two");
    cell.cell_set(s);
    if s.dec_ref() {
        s.free();
    }
    assert_eq!(cell.cell_get().as_string().as_deref(), Some("two"));
    assert!(cell.dec_ref());
    cell.free();
}

#[test]
fn closure_owns_its_upvalue_cells() {
    let cell = Value::cell(Value::int(42));
    // The closure takes ownership of one reference to the cell.
    let closure = Value::closure(3, vec![cell]);
    assert_eq!(closure.as_closure(), Some(3));
    assert_eq!(closure.closure_upvalue_count(), 1);
    assert_eq!(closure.closure_upvalue(0).cell_get().as_int(), Some(42));
    // Freeing the closure releases its upvalue cell (and the int the cell held).
    assert!(closure.dec_ref());
    closure.free();
}

#[test]
fn native_fn_values_round_trip_and_compare_by_builtin() {
    let len = Value::native_fn(Builtin::Len);
    let len2 = Value::native_fn(Builtin::Len);
    let map = Value::native_fn(Builtin::Map);
    assert_eq!(len.as_native_fn(), Some(Builtin::Len));
    assert_eq!(len.type_name(), "function");
    assert_eq!(len.display(), "<fn>");
    // Same builtin compares equal; different builtins do not (matches `Value::Builtin`).
    // `apply_binary` borrows its operands, so each value is freed explicitly below.
    assert!(
        crate::ops::apply_binary(noeta_ast::BinaryOp::Eq, len, len2)
            .unwrap()
            .as_bool()
            .unwrap()
    );
    assert!(
        !crate::ops::apply_binary(noeta_ast::BinaryOp::Eq, len, map)
            .unwrap()
            .as_bool()
            .unwrap()
    );
    for v in [len, len2, map] {
        assert!(v.dec_ref());
        v.free();
    }
}

#[test]
fn nested_lists_free_recursively() {
    let inner = Value::list(vec![Value::string("x"), Value::string("y")]);
    let outer = Value::list(vec![inner, Value::int(7)]);
    assert_eq!(outer.display(), "[[\"x\", \"y\"], 7]");
    assert!(outer.dec_ref());
    outer.free();
}

#[test]
fn maps_iterate_in_sorted_key_order() {
    let mut entries = BTreeMap::new();
    entries.insert("b".to_string(), Value::int(2));
    entries.insert("a".to_string(), Value::string("v"));
    let map = Value::map(entries);
    assert_eq!(map.type_name(), "map");
    assert_eq!(map.display(), "{\"a\": \"v\", \"b\": 2}");
    assert_eq!(map.map_len(), Some(2));
    let values = map.map_values().unwrap();
    assert_eq!(values.len(), 2);
    assert_eq!(values[0].as_string().as_deref(), Some("v"));
    assert_eq!(values[1].as_int(), Some(2));
    assert!(map.dec_ref());
    map.free();
}

#[test]
fn empty_collections_display_distinctly() {
    let list = Value::list(vec![]);
    assert_eq!(list.display(), "[]");
    let map = Value::map(BTreeMap::new());
    assert_eq!(map.display(), "{}");
    assert!(list.dec_ref());
    list.free();
    assert!(map.dec_ref());
    map.free();
}

#[test]
fn objects_display_in_slot_order_and_free_their_slots() {
    let shape = noeta_object::intern_shape(Shape::object(
        ShapeKind::Struct,
        "Item",
        vec!["price".into(), "qty".into()],
    ));
    let obj = Value::object(shape, vec![Value::float(2.5), Value::int(4)]);
    assert_eq!(obj.type_name(), "object");
    assert_eq!(obj.display(), "Item {price: 2.5, qty: 4}");
    assert_eq!(obj.field("price").unwrap().as_float(), Some(2.5));
    assert!(obj.field("missing").is_none());
    // Same shape handle (the `Rc`) is shared, not copied per-instance.
    let obj2 = Value::object(shape, vec![Value::float(2.5), Value::int(4)]);
    assert!(std::ptr::eq(obj.shape().unwrap(), obj2.shape().unwrap()));
    // Structural equality (tree-walker parity): same type + equal fields.
    assert!(
        apply_binary(noeta_ast::BinaryOp::Eq, obj, obj2)
            .unwrap()
            .as_bool()
            == Some(true)
    );
    for v in [obj, obj2] {
        assert!(v.dec_ref());
        v.free();
    }
}

#[test]
fn structural_compare_orders_objects_lexicographically() {
    use std::cmp::Ordering;
    let shape = noeta_object::intern_shape(Shape::object(
        ShapeKind::Struct,
        "Version",
        vec!["major".into(), "minor".into()],
    ));
    let v19 = Value::object(shape, vec![Value::int(1), Value::int(9)]);
    let v20 = Value::object(shape, vec![Value::int(2), Value::int(0)]);
    let v19b = Value::object(shape, vec![Value::int(1), Value::int(9)]);
    // Major dominates; equal major falls to minor; equal objects compare Equal.
    assert_eq!(structural_compare(v19, v20), Some(Ordering::Less));
    assert_eq!(structural_compare(v20, v19), Some(Ordering::Greater));
    assert_eq!(structural_compare(v19, v19b), Some(Ordering::Equal));
    // A primitive on one side is not an object pair: no defined order.
    assert_eq!(structural_compare(v19, Value::int(1)), None);
    assert_eq!(
        compare_primitive(Value::int(3), Value::int(5)),
        Some(Ordering::Less)
    );
    for v in [v19, v20, v19b] {
        assert!(v.dec_ref());
        v.free();
    }
}

#[test]
fn enum_values_display_and_compare() {
    let pending =
        noeta_object::intern_shape(Shape::enum_variant("Status", "Pending", vec![], false));
    let a = Value::enum_value(pending, vec![]);
    assert_eq!(a.type_name(), "enum");
    assert_eq!(a.display(), "Status.Pending");
    let b = Value::enum_value(pending, vec![]);
    assert!(
        apply_binary(noeta_ast::BinaryOp::Eq, a, b)
            .unwrap()
            .as_bool()
            == Some(true)
    );

    // A built-in Result variant displays bare, with its data unquoted.
    let err =
        noeta_object::intern_shape(Shape::enum_variant("Result", "Err", vec!["0".into()], true));
    let e = Value::enum_value(err, vec![Value::string("boom")]);
    assert_eq!(e.display(), "Err(boom)");
    for v in [a, b, e] {
        assert!(v.dec_ref());
        v.free();
    }
}

#[test]
fn live_count_tracks_alloc_and_free() {
    // The leak oracle's measuring stick: every allocation bumps the live count and every
    // reclamation drops it, so a build-then-free round trip returns to the starting value.
    let before = live_count();
    let s = Value::string("x");
    let list = Value::list(vec![Value::string("a"), Value::string("b")]);
    // String + (list + its two element strings) = 4 live objects.
    assert_eq!(live_count(), before + 4);
    assert!(s.dec_ref());
    s.free();
    assert!(list.dec_ref());
    list.free(); // frees the list and recursively its two elements
    assert_eq!(live_count(), before);
}

#[test]
fn reflect_tag_is_invisible_to_value_semantics_and_leaks_nothing() {
    // The reflected-type tag lives beside the payload: it round-trips through construction and
    // free with no residual (a leaf `Rc<TypeRepr>`, not a child object, so `live_count` is
    // unchanged by tagging), and it is invisible to equality — a tagged and an untagged list of
    // the same elements compare equal.
    use noeta_ast::reflect::TypeRepr;
    let before = live_count();
    let tagged = Value::list(vec![Value::int(1), Value::int(2)]);
    tagged.set_reflect(Some(Rc::new(TypeRepr::List(Box::new(TypeRepr::Int)))));
    let untagged = Value::list(vec![Value::int(1), Value::int(2)]);
    // Tagging allocates no heap object (small ints are immediate): two lists, nothing else.
    assert_eq!(live_count(), before + 2);
    // The tag is readable, and clearing it (the mutation path) drops back to `None`.
    assert!(tagged.reflect().is_some());
    assert!(untagged.reflect().is_none());
    // Equality ignores the tag entirely.
    assert!(
        apply_binary(noeta_ast::BinaryOp::Eq, tagged, untagged)
            .unwrap()
            .as_bool()
            .unwrap()
    );
    // A content-changing op clears the tag (refcount-independent — matches the tree-walker).
    let displaced = tagged.list_replace_slot(0, Value::int(9));
    assert!(displaced.as_int().is_some());
    assert!(tagged.reflect().is_none());
    assert!(tagged.dec_ref());
    tagged.free();
    assert!(untagged.dec_ref());
    untagged.free();
    assert_eq!(live_count(), before, "the tag leaves no residual heap");
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

#[test]
fn iterator_advances_collects_and_frees_without_leaking() {
    // Heap elements (strings) so the refcount + unsafe heap-access paths in `iter`/`iter_next`/
    // `iter_collect` are actually exercised (immediates would no-op the refcounting). The leak
    // oracle's invariant: live count returns to baseline once every reference is released.
    let before = live_count();
    let list = Value::list(vec![Value::string("a"), Value::string("b")]); // list + 2 strings
    let it = Value::iter(list); // +1 (iter); retains the list (rc 2)
    // The iterator now owns the list; drop this local reference (rc → 1, still alive).
    assert!(!list.dec_ref());

    // `next()` hands back a freshly-retained element.
    let a = it.iter_next().unwrap();
    assert_eq!(a.as_string().as_deref(), Some("a"));
    // The backing list still owns "a", so this only drops the next()-retained reference.
    assert!(!a.dec_ref());

    // `collect()` drains the rest into a new list (["b"]).
    let rest = it.iter_collect();
    assert_eq!(rest.list_len(), Some(1));
    assert!(rest.dec_ref());
    rest.free();

    // A drained iterator yields `none` and is safe to keep calling.
    assert!(it.iter_next().is_none());

    // Freeing the iterator releases its backing list, which frees its elements.
    assert!(it.dec_ref());
    it.free();
    assert_eq!(live_count(), before);
}

#[test]
fn iterator_adapters_free_without_leaking() {
    // Heap elements again, exercising the recursive `iter_next` (Take → Chain → List), `drop`'s
    // skip-release, and the multi-source GC node (Chain owns two children). Live count must
    // return to baseline after the whole pipeline is released.
    let before = live_count();
    let l1 = Value::list(vec![Value::string("a"), Value::string("b")]);
    let l2 = Value::list(vec![Value::string("c")]);
    let i1 = Value::iter(l1);
    l1.release(); // i1 is now l1's sole owner
    let i2 = Value::iter(l2);
    l2.release();
    let chained = Value::iter_chain(i1, i2); // retains i1, i2
    i1.release();
    i2.release(); // `chained` is now the sole owner of i1, i2
    let taken = Value::iter_take(chained, 2); // retains chained
    chained.release();
    // Take(2) over chain([a, b], [c]) yields "a", "b".
    let collected = taken.iter_collect();
    assert_eq!(collected.list_len(), Some(2));
    collected.release();
    taken.release(); // frees taken → chained → i1/i2 → l1/l2 → their strings
    assert_eq!(live_count(), before);

    // `drop` skips (and releases) the first n, yielding the rest.
    let l3 = Value::list(vec![
        Value::string("x"),
        Value::string("y"),
        Value::string("z"),
    ]);
    let i3 = Value::iter(l3);
    l3.release();
    let dropped = Value::iter_drop(i3, 2); // retains i3
    i3.release();
    let rest = dropped.iter_collect(); // ["z"]
    assert_eq!(rest.list_len(), Some(1));
    rest.release();
    dropped.release();
    assert_eq!(live_count(), before);
}

#[test]
fn iterator_tuple_adapters_and_sum_free_without_leaking() {
    // Track I.1b.2: `enumerate` builds `(int, element)` tuples (each owning the heap element
    // `iter_next` handed it), `zip` pairs two sources and releases a leftover element of the
    // longer one, and `sum`'s error path releases the offending heap element. Live count must
    // return to baseline throughout.
    let before = live_count();

    // `enumerate` over heap strings → a list of `(index, string)` tuples.
    let le = Value::list(vec![Value::string("a"), Value::string("b")]);
    let ie = Value::iter(le);
    le.release();
    let enumerated = Value::iter_enumerate(ie);
    ie.release();
    let collected = enumerated.iter_collect(); // [(0, "a"), (1, "b")]
    assert_eq!(collected.list_len(), Some(2));
    collected.release();
    enumerated.release();
    assert_eq!(live_count(), before);

    // `zip` with a longer left source: the leftover "c" pulled from `a` after `b` runs dry must
    // be released, not leaked.
    let la = Value::list(vec![
        Value::string("a"),
        Value::string("b"),
        Value::string("c"),
    ]);
    let lb = Value::list(vec![Value::string("x")]);
    let ia = Value::iter(la);
    la.release();
    let ib = Value::iter(lb);
    lb.release();
    let zipped = Value::iter_zip(ia, ib); // retains ia, ib
    ia.release();
    ib.release();
    let pairs = zipped.iter_collect(); // [("a", "x")] — "b" stays in the source, "c" consumed+freed
    assert_eq!(pairs.list_len(), Some(1));
    pairs.release();
    zipped.release();
    assert_eq!(live_count(), before);

    // `sum` over ints totals to an immediate and frees the iterator.
    let ls = Value::list(vec![Value::int(1), Value::int(2), Value::int(3)]);
    let is = Value::iter(ls);
    ls.release();
    assert_eq!(is.iter_sum().unwrap().as_int(), Some(6));
    is.release();
    assert_eq!(live_count(), before);

    // `sum`'s error path: a heap (non-numeric) element is released as the error is raised.
    let lerr = Value::list(vec![Value::int(1), Value::string("nope")]);
    let ierr = Value::iter(lerr);
    lerr.release();
    assert_eq!(ierr.iter_sum().unwrap_err(), "string");
    ierr.release();
    assert_eq!(live_count(), before);
}

#[test]
fn iterator_closure_adapters_free_without_leaking() {
    // Track I.1c: `map`/`filter` call back into a supplied applier (the real backend runs a user
    // closure; here a Rust closure stands in). Exercises the source-element-consumed-by-apply path,
    // the adapter owning a heap "closure" value, filter's keep/drop refcounting, and the non-bool
    // error path — all of which must return the live count to baseline.
    let before = live_count();

    // `map`: each int element → a fresh heap string. `func` is a heap stand-in the adapter retains.
    let lm = Value::list(vec![Value::int(1), Value::int(2), Value::int(3)]);
    let im = Value::iter(lm);
    lm.release();
    let func = Value::string("f");
    let mapped = Value::iter_map(im, func);
    im.release();
    func.release(); // the adapter is now the sole owner of `im` and `func`
    let mut to_str = |_f: Value, arg: Value| -> Result<Value, ()> {
        let n = arg.as_int().expect("int element");
        arg.release(); // the call consumes the argument's reference
        Ok(Value::string(&n.to_string()))
    };
    let mut out = Vec::new();
    while let Some(e) = mapped.iter_next_apply(&mut to_str).expect("no abort") {
        out.push(e);
    }
    assert_eq!(out.len(), 3);
    for e in out {
        e.release();
    }
    mapped.release(); // frees mapped → im → lm and the func string
    assert_eq!(live_count(), before);

    // `filter`: keep the even elements — exercises the inc_ref + keep-or-release element paths.
    let lf = Value::list(vec![
        Value::int(1),
        Value::int(2),
        Value::int(3),
        Value::int(4),
    ]);
    let iff = Value::iter(lf);
    lf.release();
    let pred = Value::string("p");
    let filtered = Value::iter_filter(iff, pred);
    iff.release();
    pred.release();
    let mut even = |_f: Value, arg: Value| -> Result<Value, ()> {
        let keep = arg.as_int().is_some_and(|n| n % 2 == 0);
        arg.release(); // the predicate consumes the argument
        Ok(Value::bool(keep))
    };
    let mut kept = Vec::new();
    while let Some(e) = filtered.iter_next_apply(&mut even).expect("no abort") {
        kept.push(e);
    }
    assert_eq!(kept.len(), 2);
    for e in kept {
        e.release();
    }
    filtered.release();
    assert_eq!(live_count(), before);

    // `filter`'s non-bool error path: the held element is released as the abort is raised.
    let le = Value::list(vec![Value::int(5)]);
    let ie = Value::iter(le);
    le.release();
    let filtered2 = Value::iter_filter(ie, Value::int(0));
    ie.release();
    let mut bad = |_f: Value, arg: Value| -> Result<Value, ()> {
        arg.release();
        Ok(Value::int(99)) // a non-bool verdict
    };
    match filtered2.iter_next_apply(&mut bad) {
        Err(IterAbort::FilterNotBool(name)) => assert_eq!(name, "int"),
        _ => panic!("expected a FilterNotBool abort"),
    }
    filtered2.release();
    assert_eq!(live_count(), before);
}

#[test]
fn generator_iterator_steps_and_frees_without_leaking() {
    // Track G: a `Gen` iterator drives a step closure returning `?T`. Here a Rust closure stands
    // in for the lowered generator (the real backend passes a real closure), building an `Option`
    // each call. Exercises `option_take`'s some-payload extraction + Option-wrapper release with
    // heap elements, plus the adapter owning the heap step value — live count returns to baseline.
    let before = live_count();

    let some_shape = noeta_object::intern_shape(noeta_object::Shape::enum_variant(
        "Option",
        "some",
        vec!["0".into()],
        true,
    ));
    let none_shape = noeta_object::intern_shape(noeta_object::Shape::enum_variant(
        "Option",
        "none",
        Vec::new(),
        true,
    ));

    let step = Value::string("step"); // heap stand-in for the closure the adapter retains
    let g = Value::iter_gen(step);
    step.release(); // `g` is now the sole owner of `step`

    let mut calls = 0;
    let mut apply = |_s: Value, arg: Value| -> Result<Value, ()> {
        arg.release(); // the resume argument is consumed by the call
        calls += 1;
        Ok(match calls {
            1 => Value::enum_value(some_shape, vec![Value::string("a")]),
            2 => Value::enum_value(some_shape, vec![Value::string("b")]),
            _ => Value::enum_value(none_shape, Vec::new()),
        })
    };

    let mut out = Vec::new();
    while let Some(e) = g.iter_next_apply(&mut apply).expect("no abort") {
        out.push(e);
    }
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].as_string().as_deref(), Some("a"));
    for e in out {
        e.release();
    }
    g.release(); // frees the generator and its step value
    assert_eq!(live_count(), before);
}

proptest! {
    // Disable on-disk failure persistence: its default backend calls `getcwd` to absolutize the
    // source path, which Miri's isolation forbids (so `cargo miri test` aborted here). Regression
    // seeds are a convenience we don't rely on, and dropping them lets these properties run under
    // Miri alongside the rest of the crate.
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]

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

/// The **line-count ratchet** on `lib.rs` (audit-1 finding 8) — the same guard `noeta-vm`
/// carries. The 2026 split moved the packed-list machinery (`packed.rs`), the iterator engine
/// (`iter.rs`), the concurrency value kinds (`conc.rs`), display/serialization (`display.rs`),
/// and this test module (`tests.rs`) out of the crate root, leaving lib.rs at ~1,670 lines
/// (crate docs, the NaN-box codec, constructors/accessors, collection primitives, reflection
/// tags, and the refcount/GC bridge). The budget is that figure plus ~10% headroom for doc
/// growth: a NEW METHOD CLUSTER BELONGS IN ITS OWN MODULE, not here — if this fires, move the
/// addition out rather than raising the budget.
#[test]
fn lib_rs_stays_decomposed() {
    const BUDGET: usize = 1840;
    let lines = include_str!("lib.rs").lines().count();
    assert!(
        lines <= BUDGET,
        "src/lib.rs is {lines} lines (budget {BUDGET}). The god-file is regrowing — land new \
         method clusters in their own module (packed/iter/conc/display) instead of raising the budget."
    );
}

/// The hinted render (`Rvalue::Render`'s VM half) reads an erased `u64` word unsigned at every
/// position the hint names, and hands every other position back to the plain `display`. This is the
/// unit-level half of `tests/conformance/types/unsigned_display.noe`; the differential is what
/// asserts the tree-walker's twin agrees.
#[test]
fn a_hint_renders_an_erased_word_unsigned_at_the_positions_it_names() {
    use noeta_ast::RenderHint;
    let max = Value::int(u64::MAX as i64);
    // The bare scalar, and the control: no hint means the signed word.
    assert_eq!(
        max.display_hinted(&RenderHint::Unsigned),
        "18446744073709551615"
    );
    assert_eq!(max.display(), "-1");
    // Elements of a list; a hint whose shape does not match the value falls back to `display`.
    let list = Value::list(vec![max, Value::int(1)]);
    let elems = RenderHint::Elements(Box::new(RenderHint::Unsigned));
    assert_eq!(list.display_hinted(&elems), "[18446744073709551615, 1]");
    assert_eq!(max.display_hinted(&elems), "-1");
    // A sparse `Slots` hint: only slot 1 is unsigned, so slot 0 keeps the signed reading.
    let tuple = Value::tuple(vec![max, max]);
    let slots = RenderHint::slots([None, Some(RenderHint::Unsigned)]).unwrap();
    assert_eq!(tuple.display_hinted(&slots), "(-1, 18446744073709551615)");
    for v in [list, tuple] {
        if v.dec_ref() {
            v.free();
        }
    }
}

/// The ordering half, and its two sources. A slot the SHAPE declares `u64` orders unsigned with no
/// hint at all — that is what makes a `@derive(Comparable)` field order the same way after a `dyn`
/// launder — while a bare erased word takes the site's hint, since nothing the value carries says
/// so. Without either, the signed word wins, which is the reading `u64::MAX` erases to.
#[test]
fn a_u64_orders_unsigned_from_the_shape_or_from_the_site_hint() {
    use noeta_ast::RenderHint;
    use std::cmp::Ordering;
    let max = Value::int(u64::MAX as i64);
    let one = Value::int(1);
    // No shape, no hint: the erased word, which is where the defect lived.
    assert_eq!(compare_values(max, one), Some(Ordering::Less));
    // The site hint reaches a bare scalar.
    assert_eq!(
        compare_values_hinted(max, one, Some(&RenderHint::Unsigned)),
        Some(Ordering::Greater)
    );
    // The shape reaches a declared field, with no hint in sight. Slot 0 is declared `u64`; slot 1
    // is the control and keeps the signed reading.
    let reading = noeta_object::intern_shape(
        Shape::object(
            noeta_object::ShapeKind::Struct,
            "Reading",
            vec!["at".into(), "delta".into()],
        )
        .with_unsigned_slots(vec![0]),
    );
    let big = Value::object(reading, vec![max, max]);
    let small = Value::object(reading, vec![one, one]);
    assert_eq!(compare_values(big, small), Some(Ordering::Greater));
    // Equal in the unsigned slot ⇒ the tie falls to the signed one, where `-1 < 1`.
    let a = Value::object(reading, vec![one, max]);
    let b = Value::object(reading, vec![one, one]);
    assert_eq!(compare_values(a, b), Some(Ordering::Less));
    for v in [big, small, a, b] {
        if v.dec_ref() {
            v.free();
        }
    }
}
