//! Real OS-thread isolates: copy-at-the-boundary marshalling (isolates I.4b).
//!
//! A real `isolate f(args)` runs on its own OS thread with its own VM and heap (out-of-oracle, CLI
//! only). Because runtime `Value`s are raw NaN-boxed heap pointers carrying a non-atomic `Rc<Shape>`,
//! **no `Value` may cross a thread**. Instead the argument graph is copied into a `Send`, self-contained
//! [`Wire`] on the parent thread and rebuilt into fresh heap objects on the worker (and the result
//! copied back the same way). The one thing genuinely shared is `Arc<Module>` — the compiled module is
//! `Send + Sync` (fully index-based, no `Rc`) — so shapes are carried by their `Module.shapes` **index**,
//! identical across every VM that shares the module, needing no name lookup on rebuild.
//!
//! Cross-thread channels (shipping a `Sender`/`Receiver` into an isolate) are a separate, larger
//! sub-problem (the blocking channel must interoperate with the in-isolate cooperative scheduler) and
//! land in I.4c; [`marshal`] rejects an endpoint so the spawn site can fall back to a cooperative task.

use std::rc::Rc;

use lang_object::Shape;
use lang_value::Value;

/// A `Send`, self-contained serialization of a value graph crossing an isolate boundary by copy. No
/// `Value`/heap pointer or `Rc` is inside it, so it moves freely between threads; the worker rebuilds
/// its own heap objects from it. Shapes are the `Module.shapes` index (stable across VMs sharing the
/// one `Arc<Module>`).
#[derive(Debug, Clone)]
pub enum Wire {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    F32(f32),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Wire>),
    Tuple(Vec<Wire>),
    Set(Vec<Wire>),
    Map(Vec<(String, Wire)>),
    /// A struct/class object: its `Module.shapes` index and its fields in slot order.
    Object {
        shape: u32,
        fields: Vec<Wire>,
    },
    /// An enum value: its `Module.shapes` index (which encodes name + variant) and its payload.
    Enum {
        shape: u32,
        data: Vec<Wire>,
    },
    /// A top-level function value (empty upvalues): its prototype index. Lets a marshalled global
    /// function be reconstructed on the worker so an isolate body can call it.
    Function(u32),
}

/// The `Module.shapes` index of `value`'s shape, found by pointer identity — every shaped value shares
/// the one interned `Rc<Shape>` per table entry, so a `ptr_eq` scan resolves the index. `None` if the
/// value has no shape or its shape is somehow not in the table (defensive).
fn shape_index(value: Value, shapes: &[Rc<Shape>]) -> Option<u32> {
    let shape = value.shape()?;
    shapes
        .iter()
        .position(|s| Rc::ptr_eq(s, &shape))
        .map(|i| i as u32)
}

/// Copy a value graph into a `Send` [`Wire`] on the source thread (isolates I.4b). Only value-type
/// (`Send`) payloads and top-level functions are representable; a channel endpoint is rejected (the
/// caller falls back to a cooperative task, pending I.4c), and any other non-`Send` payload is a bug
/// (the checker's E0042 classifier keeps it away from a boundary).
pub fn marshal(value: Value, shapes: &[Rc<Shape>]) -> Result<Wire, String> {
    // A channel endpoint reaching here means an isolate arg or result ships a channel — deferred to
    // I.4c. Signalled distinctly so the spawn site can choose the cooperative fallback.
    if value.sender_id().is_some() || value.receiver_id().is_some() {
        return Err("channel".to_string());
    }
    if value.is_unit() {
        return Ok(Wire::Unit);
    }
    if let Some(b) = value.as_bool() {
        return Ok(Wire::Bool(b));
    }
    if let Some(i) = value.as_int() {
        return Ok(Wire::Int(i));
    }
    if let Some(f) = value.as_f32() {
        return Ok(Wire::F32(f));
    }
    if let Some(f) = value.as_float() {
        return Ok(Wire::Float(f));
    }
    if let Some(s) = value.as_string() {
        return Ok(Wire::Str(s));
    }
    if let Some(b) = value.bytes_data() {
        return Ok(Wire::Bytes(b));
    }
    if let Some(items) = value.list_items() {
        return Ok(Wire::List(marshal_each(&items, shapes)?));
    }
    if let Some(items) = value.tuple_items() {
        return Ok(Wire::Tuple(marshal_each(&items, shapes)?));
    }
    if let Some(items) = value.set_items() {
        return Ok(Wire::Set(marshal_each(&items, shapes)?));
    }
    if let Some(entries) = value.map_entries() {
        let mut out = Vec::with_capacity(entries.len());
        for (k, v) in entries {
            out.push((k, marshal(v, shapes)?));
        }
        return Ok(Wire::Map(out));
    }
    if value.is_object() {
        let shape = shape_index(value, shapes).ok_or("unknown object shape")?;
        let fields = value.slots().unwrap_or_default();
        return Ok(Wire::Object {
            shape,
            fields: marshal_each(&fields, shapes)?,
        });
    }
    if value.is_enum() {
        let shape = shape_index(value, shapes).ok_or("unknown enum shape")?;
        let data = value.enum_data().unwrap_or_default();
        return Ok(Wire::Enum {
            shape,
            data: marshal_each(&data, shapes)?,
        });
    }
    // A top-level function (no captured upvalues) marshals to its prototype index; a closure with
    // captures is not shippable (it holds heap cells) — but the Send classifier already rejects
    // closures at a boundary, so this only fires for a marshalled global function.
    if let Some(proto) = value.as_closure() {
        if value.closure_upvalue_count() == 0 {
            return Ok(Wire::Function(proto));
        }
        return Err("closure with captures".to_string());
    }
    Err(format!(
        "value of type `{}` is not shippable",
        value.type_name()
    ))
}

fn marshal_each(values: &[Value], shapes: &[Rc<Shape>]) -> Result<Vec<Wire>, String> {
    values.iter().map(|&v| marshal(v, shapes)).collect()
}

/// Rebuild a [`Wire`] into fresh heap objects on the current (worker) thread (isolates I.4b), using the
/// worker's own interned `Rc<Shape>` table (indices match the source's — same `Module`). Every returned
/// `Value` owns one reference, exactly like a directly-constructed value.
pub fn rebuild(wire: &Wire, shapes: &[Rc<Shape>]) -> Value {
    match wire {
        Wire::Unit => Value::unit(),
        Wire::Bool(b) => Value::bool(*b),
        Wire::Int(i) => Value::int(*i),
        Wire::Float(f) => Value::float(*f),
        Wire::F32(f) => Value::f32(*f),
        Wire::Str(s) => Value::string(s),
        Wire::Bytes(b) => Value::bytes(b.clone()),
        Wire::List(items) => Value::list(rebuild_each(items, shapes)),
        Wire::Tuple(items) => Value::tuple(rebuild_each(items, shapes)),
        Wire::Set(items) => Value::set(rebuild_each(items, shapes)),
        Wire::Map(entries) => {
            let map = entries
                .iter()
                .map(|(k, v)| (k.clone(), rebuild(v, shapes)))
                .collect();
            Value::map(map)
        }
        Wire::Object { shape, fields } => Value::object(
            Rc::clone(&shapes[*shape as usize]),
            rebuild_each(fields, shapes),
        ),
        Wire::Enum { shape, data } => Value::enum_value(
            Rc::clone(&shapes[*shape as usize]),
            rebuild_each(data, shapes),
        ),
        Wire::Function(proto) => Value::closure(*proto, Vec::new()),
    }
}

fn rebuild_each(wires: &[Wire], shapes: &[Rc<Shape>]) -> Vec<Value> {
    wires.iter().map(|w| rebuild(w, shapes)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lang_object::ShapeKind;
    use lang_value::apply_binary;

    fn eq(a: Value, b: Value) -> bool {
        apply_binary(lang_ast::BinaryOp::Eq, a, b)
            .unwrap()
            .as_bool()
            .unwrap()
    }

    #[test]
    fn round_trips_a_nested_value_graph() {
        // A struct shape plus the built-in-free primitives/collections a `Send` graph is made of.
        let shapes = vec![Rc::new(Shape::object(
            ShapeKind::Struct,
            "Point",
            vec!["x".into(), "y".into()],
        ))];
        // list[ (1, "two"), Point{3,4}, [true, 3.5] ] — tuples, strings, an object, a nested list, f64.
        let original = Value::list(vec![
            Value::tuple(vec![Value::int(1), Value::string("two")]),
            Value::object(Rc::clone(&shapes[0]), vec![Value::int(3), Value::int(4)]),
            Value::list(vec![Value::bool(true), Value::float(3.5)]),
        ]);

        // Marshal to the Send wire form, then rebuild — round-trips structurally, and the rebuilt graph
        // is a distinct allocation (marshalling copies).
        let wire = marshal(original, &shapes).expect("a Send value graph marshals");
        let rebuilt = rebuild(&wire, &shapes);
        assert_ne!(original.bits(), rebuilt.bits());
        assert!(
            eq(original, rebuilt),
            "the rebuilt graph equals the original"
        );
        assert_eq!(
            rebuilt.display(),
            "[(1, \"two\"), Point {x: 3, y: 4}, [true, 3.5]]"
        );

        original.release();
        rebuilt.release();
    }

    #[test]
    fn rejects_a_channel_endpoint() {
        // Shipping a channel endpoint into an isolate is deferred to I.4c; marshalling one is a
        // distinct "channel" error so the spawn site can fall back to a cooperative task.
        let tx = Value::make_sender(0);
        assert_eq!(marshal(tx, &[]).unwrap_err(), "channel");
        tx.release();
        let rx = Value::make_receiver(0);
        assert_eq!(marshal(rx, &[]).unwrap_err(), "channel");
        rx.release();
    }

    #[test]
    fn round_trips_f32_and_bytes_and_enum() {
        // f32 (distinct immediate), bytes, and an enum value (shape carries name + variant).
        let shapes = vec![Rc::new(Shape::enum_variant(
            "Color",
            "rgb",
            vec!["r".into()],
            false,
        ))];
        let original = Value::enum_value(Rc::clone(&shapes[0]), vec![Value::int(255)]);
        let wire = marshal(original, &shapes).unwrap();
        let rebuilt = rebuild(&wire, &shapes);
        assert!(eq(original, rebuilt));
        original.release();
        rebuilt.release();

        let f = Value::f32(1.5);
        assert!(matches!(marshal(f, &[]).unwrap(), Wire::F32(x) if x == 1.5));
        let b = Value::bytes(vec![1, 2, 3]);
        let wb = marshal(b, &[]).unwrap();
        let rb = rebuild(&wb, &[]);
        assert_eq!(rb.bytes_data(), Some(vec![1, 2, 3]));
        b.release();
        rb.release();
    }
}
