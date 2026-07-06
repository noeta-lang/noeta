//! Builtin / receiver **method dispatch**: the `call_*_method` cluster and the
//! small per-receiver helpers (list / set / map / vec / iter / file-handle
//! methods, the in-place mutation paths, and the stdlib arity / coercion
//! helpers). Every item here is an `impl Vm` method moved verbatim out of the
//! crate root; the `dispatch` loop in `lib.rs` is the sole caller. Kept in its
//! own file purely to shrink `lib.rs` — no behavior change.

use noeta_diagnostics::DiagnosticCode;
use noeta_span::Span;
use noeta_value::{CompactString, Value};

use crate::*;

impl<'m> Vm<'m> {
    /// A Ring 1 list method (`reverse`/`contains`/`join`). Mirrors the tree-walker's
    /// `call_list_method`; the result is a freshly-owned value (refcount 1). The receiver's
    /// elements shared from `list_items` are not retained, so any element placed into a *new*
    /// list must be retained first (the list then owns that reference).
    /// Dispatch a Ring 1 list method. A packed list (P-PACK 2.4) has no specialized list methods
    /// yet, so it is materialized to a temporary boxed list, dispatched, and released — the result
    /// is observably identical to the boxed equivalent. A boxed list dispatches directly.
    pub(crate) fn call_list_method(
        &mut self,
        list: Value,
        method: noeta_stdlib::ListMethod,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        if list.is_packed_list() {
            // Selection producers keep the list *flat* — `reverse`/`slice` build a new packed buffer
            // by copying the selected elements' word-blocks (P-PACK 2.6), instead of demoting to N
            // boxed objects. Their arity/bounds checks and errors mirror the boxed arms exactly, so
            // the result is observably identical. Every other method demotes (materialize-on-read).
            match method {
                noeta_stdlib::ListMethod::Reverse => {
                    self.stdlib_arity(name, args, 0, span)?;
                    let n = list.list_len().expect("packed list");
                    let indices: Vec<usize> = (0..n).rev().collect();
                    return Ok(list.packed_select(&indices));
                }
                noeta_stdlib::ListMethod::Slice => {
                    self.stdlib_arity(name, args, 2, span)?;
                    let start = self.stdlib_int(name, args[0], span)?;
                    let end = self.stdlib_int(name, args[1], span)?;
                    let len = list.list_len().expect("packed list");
                    if start < 0 || end < start || end as usize > len {
                        let error = noeta_stdlib::slice_bounds_error(start, end, len);
                        return Err(self.error(stdlib_error_code(error.kind), span, error.message));
                    }
                    let indices: Vec<usize> = (start as usize..end as usize).collect();
                    return Ok(list.packed_select(&indices));
                }
                noeta_stdlib::ListMethod::Set => {
                    self.stdlib_arity(name, args, 2, span)?;
                    let i = self.stdlib_int(name, args[0], span)?;
                    let len = list.list_len().expect("packed list");
                    if i < 0 || i as usize >= len {
                        return Err(self.error(
                            DiagnosticCode::IndexOutOfBounds,
                            span,
                            format!("index {i} out of bounds for list of length {len}"),
                        ));
                    }
                    // Stays flat unless the new element does not pack (impossible for a well-typed
                    // `List<packed>.set` — then demote).
                    if let Some(result) = list.packed_set(i as usize, args[1]) {
                        return Ok(result);
                    }
                }
                _ => {}
            }
            let boxed = list.realize_list();
            let result = self.call_list_method_boxed(boxed, method, name, args, span);
            boxed.release();
            result
        } else {
            self.call_list_method_boxed(list, method, name, args, span)
        }
    }

    /// As [`Self::call_list_method`], but `list` is guaranteed to be a boxed list (the caller has
    /// materialized any packed receiver).
    fn call_list_method_boxed(
        &mut self,
        list: Value,
        method: noeta_stdlib::ListMethod,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        let items = list.list_items().expect("list receiver");
        match method {
            noeta_stdlib::ListMethod::Reverse => {
                self.stdlib_arity(name, args, 0, span)?;
                let mut reversed = items;
                reversed.reverse();
                for &element in &reversed {
                    retain(element);
                }
                Ok(Value::list(reversed))
            }
            noeta_stdlib::ListMethod::Contains => {
                self.stdlib_arity(name, args, 1, span)?;
                let target = args[0];
                let found = items.iter().any(|&item| {
                    apply_binary(BinaryOp::Eq, item, target)
                        .ok()
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                });
                Ok(Value::bool(found))
            }
            noeta_stdlib::ListMethod::Join => {
                self.stdlib_arity(name, args, 1, span)?;
                let separator = self.stdlib_string(name, args[0], span)?;
                let joined = items
                    .iter()
                    .map(|v| v.display())
                    .collect::<Vec<_>>()
                    .join(&separator);
                Ok(Value::string(&joined))
            }
            noeta_stdlib::ListMethod::Sorted => {
                self.stdlib_arity(name, args, 0, span)?;
                // Mutual orderability check against the first element (homogeneous numbers or
                // strings); a stable sort then matches the tree-walker element-for-element.
                if items
                    .iter()
                    .any(|&item| compare_primitive(items[0], item).is_none())
                {
                    let error = noeta_stdlib::unorderable_error(name);
                    return Err(self.error(stdlib_error_code(error.kind), span, error.message));
                }
                let mut sorted = items;
                sorted
                    .sort_by(|&a, &b| compare_primitive(a, b).unwrap_or(std::cmp::Ordering::Equal));
                for &element in &sorted {
                    retain(element);
                }
                Ok(Value::list(sorted))
            }
            noeta_stdlib::ListMethod::Slice => {
                self.stdlib_arity(name, args, 2, span)?;
                let start = self.stdlib_int(name, args[0], span)?;
                let end = self.stdlib_int(name, args[1], span)?;
                let len = items.len();
                if start < 0 || end < start || end as usize > len {
                    let error = noeta_stdlib::slice_bounds_error(start, end, len);
                    return Err(self.error(stdlib_error_code(error.kind), span, error.message));
                }
                let slice: Vec<Value> = items[start as usize..end as usize].to_vec();
                for &element in &slice {
                    retain(element);
                }
                Ok(Value::list(slice))
            }
            noeta_stdlib::ListMethod::First => {
                self.stdlib_arity(name, args, 0, span)?;
                Ok(match items.first() {
                    Some(&value) => {
                        retain(value);
                        make_some(value)
                    }
                    None => make_none(),
                })
            }
            noeta_stdlib::ListMethod::Last => {
                self.stdlib_arity(name, args, 0, span)?;
                Ok(match items.last() {
                    Some(&value) => {
                        retain(value);
                        make_some(value)
                    }
                    None => make_none(),
                })
            }
            noeta_stdlib::ListMethod::ToSet => {
                self.stdlib_arity(name, args, 0, span)?;
                match canonical_set(&items) {
                    Some(canonical) => {
                        for &element in &canonical {
                            retain(element);
                        }
                        let set = Value::set(canonical);
                        // Carry the element type from the source list's `List<T>` tag onto the
                        // resulting `Set<T>` (R1 set tags) — sets have no literal, so `to_set` is the
                        // one construction point where the element type is known.
                        set.set_reflect(set_tag_from_list(list));
                        Ok(set)
                    }
                    None => {
                        let error = noeta_stdlib::unorderable_error(name);
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            noeta_stdlib::ListMethod::Set => {
                self.stdlib_arity(name, args, 2, span)?;
                let i = self.stdlib_int(name, args[0], span)?;
                if i < 0 || i as usize >= items.len() {
                    return Err(self.error(
                        DiagnosticCode::IndexOutOfBounds,
                        span,
                        format!("index {i} out of bounds for list of length {}", items.len()),
                    ));
                }
                // Replace the slot; the displaced old element is just dropped from the clone (it was
                // never retained by `list_items`). Every element the new list ends up holding is
                // retained once (the new list is a fresh owner).
                let mut new = items;
                new[i as usize] = args[1];
                for &element in &new {
                    retain(element);
                }
                Ok(Value::list(new))
            }
        }
    }

    /// A Ring 1 set method (`contains`/`union`/`intersection`). Mirrors the tree-walker's
    /// `call_set_method`. The receiver's elements (from `set_items`) are already canonical and
    /// shared (not retained); any element placed into a new set is retained first.
    pub(crate) fn call_set_method(
        &mut self,
        set: Value,
        method: noeta_stdlib::SetMethod,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        let items = set.set_items().expect("set receiver");
        match method {
            noeta_stdlib::SetMethod::Contains => {
                self.stdlib_arity(name, args, 1, span)?;
                let target = args[0];
                let found = items.iter().any(|&item| {
                    apply_binary(BinaryOp::Eq, item, target)
                        .ok()
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                });
                Ok(Value::bool(found))
            }
            noeta_stdlib::SetMethod::Union => {
                self.stdlib_arity(name, args, 1, span)?;
                let other = self.stdlib_set(name, args[0], span)?;
                let mut combined = items;
                combined.extend(other);
                // Both operands are valid sets, so every element is orderable.
                let canonical = canonical_set(&combined).expect("set elements are orderable");
                for &element in &canonical {
                    retain(element);
                }
                Ok(Value::set(canonical))
            }
            noeta_stdlib::SetMethod::Intersection => {
                self.stdlib_arity(name, args, 1, span)?;
                let other = self.stdlib_set(name, args[0], span)?;
                // `items` is already canonical, so filtering preserves sorted, de-duplicated order.
                let kept: Vec<Value> = items
                    .into_iter()
                    .filter(|&item| {
                        other.iter().any(|&o| {
                            apply_binary(BinaryOp::Eq, item, o)
                                .ok()
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false)
                        })
                    })
                    .collect();
                for &element in &kept {
                    retain(element);
                }
                Ok(Value::set(kept))
            }
            noeta_stdlib::SetMethod::Add => {
                self.stdlib_arity(name, args, 1, span)?;
                let mut combined = items;
                combined.push(args[0]);
                match canonical_set(&combined) {
                    Some(canonical) => {
                        for &element in &canonical {
                            retain(element);
                        }
                        Ok(Value::set(canonical))
                    }
                    None => {
                        let error = noeta_stdlib::unorderable_error(name);
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            noeta_stdlib::SetMethod::Remove => {
                self.stdlib_arity(name, args, 1, span)?;
                let target = args[0];
                let kept: Vec<Value> = items
                    .into_iter()
                    .filter(|&item| {
                        !apply_binary(BinaryOp::Eq, item, target)
                            .ok()
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                    })
                    .collect();
                for &element in &kept {
                    retain(element);
                }
                Ok(Value::set(kept))
            }
        }
    }

    /// Read a set argument for a set method, raising the shared `noeta-stdlib` type error. Returns
    /// the set's canonical elements (shared, not retained).
    fn stdlib_set(&mut self, name: &str, value: Value, span: Span) -> Result<Vec<Value>, Abort> {
        match value.set_items() {
            Some(items) => Ok(items),
            None => {
                let error = noeta_stdlib::type_error(name, "set");
                Err(self.error(stdlib_error_code(error.kind), span, error.message))
            }
        }
    }

    /// Dispatch a Ring 2 native module function call (`json.parse(...)`). Mirrors the
    /// tree-walker's `call_native_module`.
    pub(crate) fn call_native_module(
        &mut self,
        module: &str,
        func: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        // `fs.*_async` (Track A.4c/A.10) are not synchronous dispatch: each produces a leaf async-IO
        // *future* (ticketed in the executor) that `.await` later resolves. Intercepted here, ahead of
        // the normal registry dispatch (which is synchronous, value-only).
        if module == "fs"
            && let Some(req) = vm_fs_async_request(func, args)
        {
            let id = self.executor.spawn_io(&mut *self.host, req);
            return Ok(Value::make_async_io(id));
        }
        // A virtual module's functions (`reactive.signal(...)`, prelude-redesign P2) are builtins —
        // they need the executor/reactive graph the registry seam cannot reach — so a qualified call
        // intercepts here, ahead of registry dispatch, exactly like `fs.*_async`. The tree-walker
        // mirrors this in its own `call_native_module`.
        if noeta_stdlib::registry::is_virtual_module(module) {
            let Some(builtin) =
                noeta_stdlib::registry::virtual_module_function(module, func)
                    .then(|| Builtin::from_name(func))
                    .flatten()
            else {
                return Err(self.error(
                    DiagnosticCode::UnknownName,
                    span,
                    format!("module `{module}` has no function `{func}`"),
                ));
            };
            return self.call_builtin(builtin, args, span);
        }
        // A function registered in the native-extension registry dispatches through the shared
        // seam: project arguments onto `NativeValue`, run the one shared dispatch body (host
        // threaded in), and materialize the `NativeOut` result (the result shape supplied from the
        // function's `RetTy`). Routing is per-function so a partially-migrated module (`vec`/`quat`,
        // whose bulk `*_all` kernels stay per-backend) falls through for its unmigrated functions.
        if let Some(sig) = noeta_stdlib::registry::find_function(module, func) {
            // A reflective module (`json`) marshals its arguments deeply (the recursive value tree
            // `json.stringify` introspects); every other module uses the cheap shallow projection.
            let deep = noeta_stdlib::registry::find_module(module).is_some_and(|m| m.deep_marshal);
            let nargs: Vec<noeta_stdlib::NativeValue> = if deep {
                args.iter().map(|a| a.to_native_deep()).collect()
            } else {
                args.iter().map(|a| marshal_native_arg(*a)).collect()
            };
            return match noeta_stdlib::registry::dispatch(module, func, &mut *self.host, &nargs) {
                Ok(out) => Ok(materialize_ext(out, sig.ret, args)),
                Err(error) => Err(self.error(stdlib_error_code(error.kind), span, error.message)),
            };
        }
        // `vec`'s bulk `*_all` kernels are the only unmigrated native functions and stay per-backend;
        // every other reachable name is registered, so anything else here is an unknown function.
        if module == "vec" {
            return self.call_vec(func, args, span);
        }
        let error = noeta_stdlib::no_function_error(module, func);
        Err(self.error(stdlib_error_code(error.kind), span, error.message))
    }

    /// The `vec` 3D-math module (P-PACK Phase 4.1): scalar Vec3 ops over structural 3-`f32` objects.
    /// The arithmetic lives in `noeta_stdlib::vec3`, so this is glue — read the components, dispatch,
    /// rebuild a same-shape result (mirrors the tree-walker's `call_vec`).
    /// The `vec` module's **bulk** kernels over `List<Vec3<f32>>` (P-PACK 4.2). The scalar ops
    /// (`add`/`dot`/…) migrated to the shared native-extension dispatch; these stay per-backend
    /// because they operate on the packed buffer (a layout specialization, not a value-seam
    /// concern). Packed inputs take the flat autovectorized buffer path; a boxed/demoted operand
    /// falls back to a scalar loop.
    fn call_vec(&mut self, func: &str, args: &[Value], span: Span) -> Result<Value, Abort> {
        use noeta_stdlib::vec3;
        match func {
            "add_all" | "sub_all" => {
                self.stdlib_arity(func, args, 2, span)?;
                self.expect_list(func, args[0], span)?;
                self.expect_list(func, args[1], span)?;
                // `add`/`sub` are element-wise over the flat `f32` array, so they are layout-agnostic:
                // the same `*_buffers` kernel on two column buffers yields the correct column result
                // (P-SIMD C3). Handle either layout uniformly — both operands must share it.
                if let (Some((schema, a)), Some((_, b))) =
                    (args[0].packed_vec3_any(), args[1].packed_vec3_any())
                {
                    if a.len() != b.len() {
                        return Err(self.vec_len_error(func, span));
                    }
                    let out = if func == "add_all" {
                        vec3::add_buffers(&a, &b)
                    } else {
                        vec3::sub_buffers(&a, &b)
                    };
                    return Ok(Value::packed_list(schema, out));
                }
                self.vec_bulk_binary_scalar(func, args[0], args[1], span)
            }
            "scale_all" => {
                self.stdlib_arity(func, args, 2, span)?;
                self.expect_list(func, args[0], span)?;
                let s = self.read_scalar_f32(func, args[1], span)?;
                // Layout-agnostic like `add`/`sub` — `scale_buffer` on column bytes is a column result.
                if let Some((schema, a)) = args[0].packed_vec3_any() {
                    return Ok(Value::packed_list(schema, vec3::scale_buffer(&a, s)));
                }
                // Scalar fallback: materialize, scale each element, rebuild a boxed list.
                let xb = args[0].realize_list();
                let result = self.vec_map_scalar(func, xb, span, |c| vec3::scale(c, s));
                xb.release();
                result
            }
            "dot_all" => {
                self.stdlib_arity(func, args, 2, span)?;
                self.expect_list(func, args[0], span)?;
                self.expect_list(func, args[1], span)?;
                // Column fast path (P-SIMD C3): read the three contiguous columns directly (no decode)
                // — `col_dot` autovectorizes and is bit-identical to the AoS reduction.
                if let (Some((_, a)), Some((_, b))) =
                    (args[0].packed_vec3_columns(), args[1].packed_vec3_columns())
                {
                    if a.len() != b.len() {
                        return Err(self.vec_len_error(func, span));
                    }
                    return Ok(self.f32_list(&vec3::col_dot(&a, &b)));
                }
                if let (Some((_, a)), Some((_, b))) =
                    (args[0].packed_vec3_data(), args[1].packed_vec3_data())
                {
                    if a.len() != b.len() {
                        return Err(self.vec_len_error(func, span));
                    }
                    let scalars = vec3::dot_buffers(&a, &b);
                    return Ok(self.f32_list(&scalars));
                }
                self.vec_bulk_dot_scalar(func, args[0], args[1], span)
            }
            "length_all" => {
                self.stdlib_arity(func, args, 1, span)?;
                self.expect_list(func, args[0], span)?;
                if let Some((_, a)) = args[0].packed_vec3_columns() {
                    return Ok(self.f32_list(&vec3::col_length(&a)));
                }
                if let Some((_, a)) = args[0].packed_vec3_data() {
                    let scalars = vec3::length_buffer(&a);
                    return Ok(self.f32_list(&scalars));
                }
                let xb = args[0].realize_list();
                let n = xb.list_len().unwrap_or(0);
                let mut scalars = Vec::with_capacity(n);
                let mut err = None;
                for i in 0..n {
                    match self.read_vec3(func, xb.list_get(i).expect("in bounds"), span) {
                        Ok(c) => scalars.push(vec3::length(c)),
                        Err(e) => {
                            err = Some(e);
                            break;
                        }
                    }
                }
                xb.release();
                match err {
                    Some(e) => Err(e),
                    None => Ok(self.f32_list(&scalars)),
                }
            }
            _ => {
                let error = noeta_stdlib::no_function_error("vec", func);
                Err(self.error(stdlib_error_code(error.kind), span, error.message))
            }
        }
    }

    /// Read a Vec3 argument — a struct value with exactly three `f32` fields — into `[f32; 3]`, or a
    /// type error. The fields are taken in slot (declared) order.
    fn read_vec3(&mut self, func: &str, value: Value, span: Span) -> Result<[f32; 3], Abort> {
        let slots = value.slots().filter(|s| s.len() == 3);
        let components: Option<[f32; 3]> =
            slots.and_then(|s| Some([s[0].as_f32()?, s[1].as_f32()?, s[2].as_f32()?]));
        components.ok_or_else(|| {
            self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`vec.{func}` expects a Vec3 (a struct of three f32 fields), found {}",
                    value.type_name()
                ),
            )
        })
    }

    /// Read a numeric scalar (`f32`/`float`/`int`) as an `f32` — the `vec.scale` factor.
    fn read_scalar_f32(&mut self, func: &str, value: Value, span: Span) -> Result<f32, Abort> {
        value
            .as_f32()
            .or_else(|| value.as_float().map(|f| f as f32))
            .or_else(|| value.as_int().map(|i| i as f32))
            .ok_or_else(|| {
                self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!(
                        "`vec.{func}` expects a number factor, found {}",
                        value.type_name()
                    ),
                )
            })
    }

    /// Build a Vec3 result with the same shape as `like`, from three `f32` components.
    fn build_vec3(&self, like: Value, c: [f32; 3]) -> Value {
        let shape = like.shape().expect("read_vec3 verified an object shape");
        Value::object(
            shape,
            vec![Value::f32(c[0]), Value::f32(c[1]), Value::f32(c[2])],
        )
    }

    /// A boxed `List<f32>` from scalar results (the output of `dot_all`/`length_all`). Each `f32` is
    /// an immediate, so the list owns no heap references.
    fn f32_list(&self, scalars: &[f32]) -> Value {
        Value::list(scalars.iter().map(|&f| Value::f32(f)).collect())
    }

    fn vec_len_error(&mut self, func: &str, span: Span) -> Abort {
        self.error(
            DiagnosticCode::TypeMismatch,
            span,
            format!("`vec.{func}` expects two lists of equal length"),
        )
    }

    /// Guard that `v` is a list (the bulk `vec.*_all` kernels operate on `List<Vec3>`); errors before
    /// any `realize_list`, which assumes a list receiver.
    fn expect_list(&mut self, func: &str, v: Value, span: Span) -> Result<(), Abort> {
        if v.is_list() {
            Ok(())
        } else {
            Err(self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("`vec.{func}` expects a list, found {}", v.type_name()),
            ))
        }
    }

    /// Scalar fallback for `add_all`/`sub_all` on boxed/demoted operands: materialize both lists,
    /// apply the component-wise op per element, and rebuild a boxed `List<Vec3>`.
    fn vec_bulk_binary_scalar(
        &mut self,
        func: &str,
        xs: Value,
        ys: Value,
        span: Span,
    ) -> Result<Value, Abort> {
        use noeta_stdlib::vec3;
        let xb = xs.realize_list();
        let yb = ys.realize_list();
        let result = (|| {
            let n = xb.list_len().unwrap_or(0);
            if yb.list_len() != Some(n) {
                return Err(self.vec_len_error(func, span));
            }
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                let a = self.read_vec3(func, xb.list_get(i).expect("in bounds"), span)?;
                let b = self.read_vec3(func, yb.list_get(i).expect("in bounds"), span)?;
                let c = if func == "add_all" {
                    vec3::add(a, b)
                } else {
                    vec3::sub(a, b)
                };
                out.push(self.build_vec3(xb.list_get(i).expect("in bounds"), c));
            }
            Ok(Value::list(out))
        })();
        xb.release();
        yb.release();
        result
    }

    /// Scalar fallback for `dot_all`: materialize both lists and reduce each element pair to an `f32`.
    fn vec_bulk_dot_scalar(
        &mut self,
        func: &str,
        xs: Value,
        ys: Value,
        span: Span,
    ) -> Result<Value, Abort> {
        use noeta_stdlib::vec3;
        let xb = xs.realize_list();
        let yb = ys.realize_list();
        let result = (|| {
            let n = xb.list_len().unwrap_or(0);
            if yb.list_len() != Some(n) {
                return Err(self.vec_len_error(func, span));
            }
            let mut scalars = Vec::with_capacity(n);
            for i in 0..n {
                let a = self.read_vec3(func, xb.list_get(i).expect("in bounds"), span)?;
                let b = self.read_vec3(func, yb.list_get(i).expect("in bounds"), span)?;
                scalars.push(vec3::dot(a, b));
            }
            Ok(self.f32_list(&scalars))
        })();
        xb.release();
        yb.release();
        result
    }

    /// Map a component-wise unary op over a (materialized boxed) `List<Vec3>`, rebuilding a boxed
    /// `List<Vec3>` — the `scale_all` scalar fallback.
    fn vec_map_scalar(
        &mut self,
        func: &str,
        list: Value,
        span: Span,
        op: impl Fn([f32; 3]) -> [f32; 3],
    ) -> Result<Value, Abort> {
        let n = list.list_len().unwrap_or(0);
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let elem = list.list_get(i).expect("in bounds");
            let c = self.read_vec3(func, elem, span)?;
            out.push(self.build_vec3(elem, op(c)));
        }
        Ok(Value::list(out))
    }

    /// Dispatch a method on an extern-type receiver (extern-types X1) through its registered
    /// [`noeta_stdlib::ExtType`]'s shared dispatch — project the arguments, run the one shared
    /// body (host threaded in, receiver `&mut`), materialize the result. Mirrors the
    /// tree-walker's `call_extern_method`, so the two backends agree by construction.
    pub(crate) fn call_extern_method(
        &mut self,
        recv: Value,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        let nargs: Vec<noeta_stdlib::NativeValue> =
            args.iter().map(|a| marshal_native_arg(*a)).collect();
        let host = &mut *self.host;
        let result =
            recv.with_extern_mut(|e| noeta_stdlib::registry::dispatch_method(e, method, host, &nargs));
        match result {
            Ok(out) => Ok(materialize_native(out)),
            Err(error) => Err(self.error(stdlib_error_code(error.kind), span, error.message)),
        }
    }

    /// Dispatch a file-handle method. Mirrors the tree-walker's `call_file_handle_method`: the
    /// cursor logic lives in the shared `FileHandle`, so the two backends differ only in value glue
    /// (building `some`/`none`, routing the close flush through `self.host`).
    pub(crate) fn call_file_handle_method(
        &mut self,
        recv: Value,
        method: noeta_stdlib::FileHandleMethod,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        use noeta_stdlib::FileHandleMethod as M;
        match method {
            M::ReadLine => {
                self.stdlib_arity(name, args, 0, span)?;
                // `with_file_handle_mut` works on `recv` (a `Value`), not on `self`, so capturing
                // `self.host` in the closure is conflict-free; a lazy handle refills through it.
                let host = &mut *self.host;
                match recv.with_file_handle_mut(|handle| handle.read_line(host)) {
                    Ok(Some(line)) => Ok(make_some(Value::string(&line))),
                    Ok(None) => Ok(make_none()),
                    Err(error) => {
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            M::Read => {
                self.stdlib_arity(name, args, 1, span)?;
                let count = self.stdlib_int(name, args[0], span)?;
                let host = &mut *self.host;
                match recv.with_file_handle_mut(|handle| handle.read(count, host)) {
                    Ok(Some(chunk)) => Ok(make_some(Value::string(&chunk))),
                    Ok(None) => Ok(make_none()),
                    Err(error) => {
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            M::Write => {
                self.stdlib_arity(name, args, 1, span)?;
                let chunk = self.stdlib_string(name, args[0], span)?;
                match recv.with_file_handle_mut(|handle| handle.write(&chunk)) {
                    Ok(()) => Ok(Value::unit()),
                    Err(error) => {
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
            M::Close => {
                self.stdlib_arity(name, args, 0, span)?;
                // Take the flush instruction first (the handle borrow ends), then hit the host.
                let flush = recv.with_file_handle_mut(|handle| handle.close());
                let result = match flush {
                    None => Ok(()),
                    Some(noeta_stdlib::Flush::Write { path, content }) => {
                        self.host.fs_write(&path, &content)
                    }
                    Some(noeta_stdlib::Flush::Append { path, content }) => {
                        self.host.fs_append(&path, &content)
                    }
                };
                match result {
                    Ok(()) => Ok(Value::unit()),
                    Err(error) => {
                        Err(self.error(stdlib_error_code(error.kind), span, error.message))
                    }
                }
            }
        }
    }

    /// Dispatch an iterator method (Track I). Mirrors the tree-walker's `call_iter_method`. `next`/
    /// `collect`/`count` consume the cursor; `take`/`drop`/`chain` build a new adapter that retains
    /// the receiver (and `chain`'s argument) — the same retain pattern as `iter()`, leak-verified.
    pub(crate) fn call_iter_method(
        &mut self,
        recv: Value,
        method: noeta_stdlib::IterMethod,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        use noeta_stdlib::IterMethod as M;
        Ok(match method {
            M::Next => {
                self.stdlib_arity(name, args, 0, span)?;
                let stepped = {
                    let mut apply =
                        |func: Value, arg: Value| self.call_value(func, vec![arg], span);
                    recv.iter_next_apply(&mut apply)
                };
                match stepped {
                    Ok(Some(element)) => make_some(element),
                    Ok(None) => make_none(),
                    Err(err) => return Err(self.iter_abort(err, span)),
                }
            }
            M::Collect => {
                self.stdlib_arity(name, args, 0, span)?;
                let mut out = Vec::new();
                let result = {
                    let mut apply =
                        |func: Value, arg: Value| self.call_value(func, vec![arg], span);
                    loop {
                        match recv.iter_next_apply(&mut apply) {
                            Ok(Some(e)) => out.push(e),
                            Ok(None) => break Ok(()),
                            Err(err) => break Err(err),
                        }
                    }
                };
                if let Err(err) = result {
                    for v in out {
                        release(v); // free the elements collected before the closure aborted
                    }
                    return Err(self.iter_abort(err, span));
                }
                Value::list(out)
            }
            M::Count => {
                self.stdlib_arity(name, args, 0, span)?;
                let mut n = 0i64;
                let result = {
                    let mut apply =
                        |func: Value, arg: Value| self.call_value(func, vec![arg], span);
                    loop {
                        match recv.iter_next_apply(&mut apply) {
                            // Drain the iterator, releasing each element it retained.
                            Ok(Some(e)) => {
                                e.release();
                                n += 1;
                            }
                            Ok(None) => break Ok(()),
                            Err(err) => break Err(err),
                        }
                    }
                };
                if let Err(err) = result {
                    return Err(self.iter_abort(err, span));
                }
                Value::int(n)
            }
            M::Take | M::Drop => {
                self.stdlib_arity(name, args, 1, span)?;
                let n = self.stdlib_int(name, args[0], span)?.max(0) as usize;
                if method == M::Take {
                    Value::iter_take(recv, n)
                } else {
                    Value::iter_drop(recv, n)
                }
            }
            M::Chain => {
                self.stdlib_arity(name, args, 1, span)?;
                Value::iter_chain(recv, args[0])
            }
            M::Enumerate => {
                self.stdlib_arity(name, args, 0, span)?;
                Value::iter_enumerate(recv)
            }
            M::Zip => {
                self.stdlib_arity(name, args, 1, span)?;
                Value::iter_zip(recv, args[0])
            }
            M::Map => {
                self.stdlib_arity(name, args, 1, span)?;
                Value::iter_map(recv, args[0])
            }
            M::Filter => {
                self.stdlib_arity(name, args, 1, span)?;
                Value::iter_filter(recv, args[0])
            }
            M::Sum => {
                self.stdlib_arity(name, args, 0, span)?;
                let mut int_total: i64 = 0;
                let mut float_total: f64 = 0.0;
                let mut any_float = false;
                let mut bad: Option<&'static str> = None;
                let result = {
                    let mut apply =
                        |func: Value, arg: Value| self.call_value(func, vec![arg], span);
                    loop {
                        match recv.iter_next_apply(&mut apply) {
                            Ok(Some(e)) => {
                                if let Some(i) = e.as_int() {
                                    int_total = int_total.wrapping_add(i);
                                } else if let Some(f) = e.as_float() {
                                    any_float = true;
                                    float_total += f;
                                } else {
                                    bad = Some(e.type_name());
                                    e.release();
                                    break Ok(());
                                }
                                e.release();
                            }
                            Ok(None) => break Ok(()),
                            Err(err) => break Err(err),
                        }
                    }
                };
                if let Err(err) = result {
                    return Err(self.iter_abort(err, span));
                }
                if let Some(found) = bad {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`sum` expects numeric elements, found {found}"),
                    ));
                }
                if any_float {
                    Value::float(float_total + int_total as f64)
                } else {
                    Value::int(int_total)
                }
            }
        })
    }

    /// Advance an iterator one element for a streaming `for` (Track I.2) — drives `iter_next_apply`
    /// with the closure applier (so `map`/`filter` run) and maps an abort to the VM's native error.
    pub(crate) fn iter_for_next(
        &mut self,
        iter: Value,
        span: Span,
    ) -> Result<Option<Value>, Abort> {
        let stepped = {
            let mut apply = |func: Value, arg: Value| self.call_value(func, vec![arg], span);
            iter.iter_next_apply(&mut apply)
        };
        stepped.map_err(|err| self.iter_abort(err, span))
    }

    /// Map an iterator-pull abort (Track I.1c) back into the VM's native error: a closure failure
    /// carries its `Abort` through unchanged; a non-bool `filter` verdict becomes a `TypeMismatch`.
    fn iter_abort(&mut self, err: noeta_value::IterAbort<Abort>, span: Span) -> Abort {
        match err {
            noeta_value::IterAbort::Closure(abort) => abort,
            noeta_value::IterAbort::FilterNotBool(found) => self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("`filter` predicate must return a bool, found {found}"),
            ),
        }
    }

    /// A Ring 1 map method (`keys`/`values`/`has`). Mirrors the tree-walker's `call_map_method`.
    pub(crate) fn call_map_method(
        &mut self,
        map: Value,
        method: noeta_stdlib::MapMethod,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        match method {
            noeta_stdlib::MapMethod::Keys => {
                self.stdlib_arity(name, args, 0, span)?;
                let keys = map.map_keys().expect("map receiver");
                Ok(Value::list(keys.iter().map(|k| Value::string(k)).collect()))
            }
            noeta_stdlib::MapMethod::Values => {
                self.stdlib_arity(name, args, 0, span)?;
                let values = map.map_values().expect("map receiver");
                for &element in &values {
                    retain(element);
                }
                Ok(Value::list(values))
            }
            noeta_stdlib::MapMethod::Has => {
                self.stdlib_arity(name, args, 1, span)?;
                // Borrow the key's `&str` for the lookup — no clone.
                let present = match args[0].with_str(|key| map.map_get(key).is_some()) {
                    Some(p) => p,
                    None => {
                        self.stdlib_string(name, args[0], span)?; // non-string key → the type error
                        false
                    }
                };
                Ok(Value::bool(present))
            }
            noeta_stdlib::MapMethod::Set => {
                self.stdlib_arity(name, args, 2, span)?;
                let key = self.stdlib_string(name, args[0], span)?;
                let mut new = map.map_entries().expect("map receiver");
                new.insert(key, args[1]);
                // The receiver is borrowed (untouched); the new map is a fresh owner, so retain each
                // value it ends up holding exactly once. A displaced/absent value is simply not in
                // `new`, so it keeps only the receiver's reference — no leak, no double-free.
                for &value in new.values() {
                    retain(value);
                }
                Ok(Value::map(new))
            }
            noeta_stdlib::MapMethod::Remove => {
                self.stdlib_arity(name, args, 1, span)?;
                let key = self.stdlib_string(name, args[0], span)?;
                let mut new = map.map_entries().expect("map receiver");
                new.remove(&key);
                for &value in new.values() {
                    retain(value);
                }
                Ok(Value::map(new))
            }
            noeta_stdlib::MapMethod::GetOr => {
                self.stdlib_arity(name, args, 2, span)?;
                // Borrow the key's `&str` for the single probe — no clone.
                let found = match args[0].with_str(|key| map.map_get(key)) {
                    Some(found) => found,
                    None => {
                        self.stdlib_string(name, args[0], span)?; // non-string key → the type error
                        None
                    }
                };
                // Hit: the value is borrowed from the map. Miss: `default` is borrowed from the
                // caller's argument register. Either way the result register is a new owner.
                let out = found.unwrap_or(args[1]);
                retain(out);
                Ok(out)
            }
        }
    }

    /// Apply an in-place map update (`set`/`remove`) to a **consumed** map receiver (Phase 5.1c): the
    /// caller has already taken the receiver's single reference out of its register. When uniquely
    /// owned (`refcount == 1`) the backing buffer is mutated in place — O(1) — and the displaced value
    /// (if any) fires its destructor now via `release_value`, matching the copy-and-reassign baseline
    /// (which releases it when the old map dies at the reassignment). An aliased map copies (preserving
    /// the other owner's view), then drops the consumed reference. Run under miri to validate refcounts.
    /// Apply an in-place list `set(index, value)` to a **consumed** list receiver (the caller has
    /// taken its single reference out of the register). When uniquely owned (`refcount == 1`) the slot
    /// is overwritten in place — O(1), the displaced element released — otherwise the list copies
    /// (preserving an alias), then the consumed reference is dropped. An out-of-range index is E0016.
    pub(crate) fn list_set_in_place(
        &mut self,
        list: Value,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        let i = self.stdlib_int("set", args[0], span)?;
        let len = list.list_len().unwrap_or(0);
        if i < 0 || i as usize >= len {
            release(list);
            return Err(self.error(
                DiagnosticCode::IndexOutOfBounds,
                span,
                format!("index {i} out of bounds for list of length {len}"),
            ));
        }
        // Sole owner: overwrite the slot in place — O(word_count) for a packed list (its primitives
        // copied into the buffer, P-PACK 2.6) or O(1) pointer-swap for a boxed one. An aliased list
        // (or a packed element that does not pack) copies via `call_list_method` (still flat for a
        // packed receiver) and then drops the consumed reference.
        if list.refcount() == 1 {
            if list.is_packed_list() {
                if list.packed_set_in_place(i as usize, args[1]) {
                    return Ok(list);
                }
                // element did not pack (impossible for a well-typed `List<packed>`) — copy below.
            } else {
                let value = args[1];
                retain(value);
                let old = list.list_replace_slot(i as usize, value);
                self.release_value(old);
                return Ok(list);
            }
        }
        // Aliased (or a packed pack-failure): copy via the ordinary method, then drop the consumed
        // reference.
        let new = self.call_list_method(list, noeta_stdlib::ListMethod::Set, "set", args, span)?;
        release(list);
        Ok(new)
    }

    pub(crate) fn map_update_in_place(
        &mut self,
        map: Value,
        method: noeta_stdlib::MapMethod,
        name: &str,
        args: &[Value],
        consume_key: bool,
        span: Span,
    ) -> Result<Value, Abort> {
        if map.refcount() != 1 {
            // Aliased: copy, then release the reference we consumed from the receiver register.
            let new = self.call_map_method(map, method, name, args, span)?;
            release(map);
            return Ok(new);
        }
        match method {
            noeta_stdlib::MapMethod::Set => {
                self.stdlib_arity(name, args, 2, span)?;
                let key = self.map_update_key(consume_key, args[0], name, span)?;
                let value = args[1];
                // The map gains an owned reference to the new value.
                retain(value);
                if let Some(old) = map.map_insert(key, value) {
                    self.release_value(old);
                }
                Ok(map)
            }
            noeta_stdlib::MapMethod::Remove => {
                self.stdlib_arity(name, args, 1, span)?;
                let key = self.map_update_key(consume_key, args[0], name, span)?;
                if let Some(old) = map.map_remove(&key) {
                    self.release_value(old);
                }
                Ok(map)
            }
            // Only `set`/`remove` are routed to the in-place path by the dispatch guard.
            _ => unreachable!("non-update map method on the in-place path"),
        }
    }

    /// Extract the owned key for an in-place map `set`/`remove`, in the map's own
    /// [`CompactString`] representation. When `consume_key` is set (the compiler proved the key
    /// is a single-use temporary) and the key value is a sole-owned string, **move** its buffer
    /// out instead of cloning it — the register still holds the value (now an empty string,
    /// freed later), and a single-use temp is never read again. Otherwise clone — allocation-free
    /// for inline (≤ 24-byte) content, which map keys overwhelmingly are (P-SSO). A non-string
    /// key raises the type error through `stdlib_string`, exactly as before.
    fn map_update_key(
        &mut self,
        consume_key: bool,
        key: Value,
        name: &str,
        span: Span,
    ) -> Result<CompactString, Abort> {
        if consume_key && key.is_string() && key.refcount() == 1 {
            Ok(key.take_string_in_place())
        } else if let Some(k) = key.as_compact_string() {
            Ok(k)
        } else {
            // Not a string: surface the same type error the clone path always raised.
            Err(self
                .stdlib_string(name, key, span)
                .expect_err("non-string key must be a type error"))
        }
    }

    /// In-place `add`/`remove` for a reuse-marked set self-update (`s = s.add(x)` / `s = s.remove(x)`).
    /// The receiver has been consumed from its register by the dispatch above. A uniquely-owned set
    /// mutates its canonical buffer in place via a binary search (the displaced element of a `remove`,
    /// or nothing for `add`, releases now — matching the copy baseline, which drops the old set); an
    /// aliased set copies through the ordinary method so the other owner's view is preserved.
    pub(crate) fn set_update_in_place(
        &mut self,
        set: Value,
        method: noeta_stdlib::SetMethod,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        if set.refcount() != 1 {
            // Aliased: copy, then release the reference we consumed from the receiver register.
            let new = self.call_set_method(set, method, name, args, span)?;
            release(set);
            return Ok(new);
        }
        if let Err(err) = self.stdlib_arity(name, args, 1, span) {
            release(set);
            return Err(err);
        }
        let target = args[0];
        // A target not orderable against the set's class behaves exactly as the copy path: `add`
        // raises the unorderable error, `remove` finds nothing (a no-op). An empty set is orderable
        // with anything, so a first-element probe of `None` (empty) takes the in-place path.
        let orderable = set
            .set_first()
            .is_none_or(|first| compare_primitive(first, target).is_some());
        match method {
            noeta_stdlib::SetMethod::Add => {
                if !orderable {
                    release(set);
                    let error = noeta_stdlib::unorderable_error(name);
                    return Err(self.error(stdlib_error_code(error.kind), span, error.message));
                }
                // The set gains an owned reference only when the element is newly inserted.
                if set.set_insert_sorted(target) {
                    retain(target);
                }
                Ok(set)
            }
            noeta_stdlib::SetMethod::Remove => {
                if orderable && let Some(old) = set.set_remove_sorted(target) {
                    self.release_value(old);
                }
                Ok(set)
            }
            // Only `add`/`remove` are routed to the in-place path by the dispatch guard.
            _ => unreachable!("non-update set method on the in-place path"),
        }
    }

    /// Enforce a collection method's arity, raising the shared `noeta-stdlib` arity error.
    fn stdlib_arity(
        &mut self,
        name: &str,
        args: &[Value],
        expected: usize,
        span: Span,
    ) -> Result<(), Abort> {
        if args.len() == expected {
            Ok(())
        } else {
            let error = noeta_stdlib::arity_error(name, expected, args.len());
            Err(self.error(stdlib_error_code(error.kind), span, error.message))
        }
    }

    /// Read a string argument for a collection method, raising the shared `noeta-stdlib` type error.
    fn stdlib_string(&mut self, name: &str, value: Value, span: Span) -> Result<String, Abort> {
        match value.as_string() {
            Some(s) => Ok(s),
            None => {
                let error = noeta_stdlib::type_error(name, "string");
                Err(self.error(stdlib_error_code(error.kind), span, error.message))
            }
        }
    }

    /// Read an int argument for a collection method, raising the shared `noeta-stdlib` type error.
    /// `as_int` is `None` for a float, so `slice(1.0, 2)` is a type error — matching the
    /// tree-walker, which accepts only `Value::Int`.
    fn stdlib_int(&mut self, name: &str, value: Value, span: Span) -> Result<i64, Abort> {
        match value.as_int() {
            Some(i) => Ok(i),
            None => {
                let error = noeta_stdlib::type_error(name, "int");
                Err(self.error(stdlib_error_code(error.kind), span, error.message))
            }
        }
    }
}
