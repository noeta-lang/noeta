//! **Member access typing**: field get/set with privacy (E0033/private-field reporting),
//! bundle-method dispatch (kernel-methods K2), namespace-group resolution, and the full
//! `synth_member` receiver dispatch. All `Checker` methods moved verbatim out of the crate root.

use crate::*;

impl Checker {
    /// Type-check a field assignment `x.f = v` (Phase 5.2): the receiver must be a class instance,
    /// the field must be declared `mut` (else E0033), and the value must be assignable to the
    /// field's declared type (else E0007). The result is the receiver's own type — the surrounding
    /// `Stmt::Binding` reassigns `x` to a value of the same type. A `dyn`/hole receiver defers to
    /// runtime (the field cannot be resolved statically).
    pub(crate) fn synth_field_set(
        &mut self,
        receiver: &Expr,
        field: &str,
        field_span: Span,
        value: &Expr,
        env: &mut Env,
    ) -> Type {
        let recv = self.synth(receiver, env);
        let vty = self.synth(value, env);
        if recv.defers_to_runtime() {
            return recv;
        }
        let Type::Named(name, recv_args) = recv.clone() else {
            self.error(
                DiagnosticCode::ImmutableField,
                field_span,
                format!("cannot assign to field `{field}`: `{recv}` is not a class instance"),
            )
            .help("only a `mut` field of a class instance can be assigned with `x.f = v`");
            return recv;
        };
        // A private field is assignable only inside its declaring type's own methods (slice 2d).
        if !self.field_visible(&name, field) {
            self.report_private_field(&name, field, FieldAccess::Assign, field_span);
        }
        // Asymmetric `mut` rule (object-model slice 2b′): a value `struct` field-set is desugared to
        // a rebind of the receiver (`x = T { ...x, f: v }`), so the receiver binding must be `mut`
        // (E0006); a reference `class` field-set mutates the shared instance in place, needing no
        // `mut` binding. (The field itself must still be declared `mut` — E0033, checked below.)
        if matches!(
            self.symbols.type_kinds.get(&name),
            Some(noeta_types::TypeKind::Struct)
        ) && let Expr::Ident {
            name: recv_name,
            span: recv_span,
        } = receiver
            && !lookup_mutable(env, recv_name)
        {
            self.error(
                DiagnosticCode::ImmutableAssignment,
                *recv_span,
                format!(
                    "cannot assign to field `{field}`: `{recv_name}` is an immutable binding, \
                         and a `struct` field-set rebinds it"
                ),
            )
            .help(format!(
                "declare it `mut {recv_name} = ...` (a value `struct` is updated by rebinding); \
                     a reference `class` field mutates in place without `mut`"
            ));
        }
        let is_mut = self
            .symbols
            .mut_fields
            .get(&name)
            .is_some_and(|fields| fields.contains(field));
        if !is_mut {
            let exists = self
                .symbols
                .records
                .get(&name)
                .is_some_and(|fs| fs.iter().any(|(n, _)| n == field));
            // Both `struct` (value) and `class` (reference) fields are immutable unless declared
            // `mut`; the unified body grammar gives them the same rule and the same diagnostic.
            if !exists {
                self.error(
                    DiagnosticCode::ImmutableField,
                    field_span,
                    format!("type `{name}` has no field `{field}`"),
                );
            } else {
                self.error(
                    DiagnosticCode::ImmutableField,
                    field_span,
                    format!("field `{field}` of `{name}` is not declared `mut`"),
                )
                .help(format!(
                    "declare it `mut {field}: ...` to allow `x.{field} = ...`, or build a new value \
                     with `{name} {{ ...x, {field}: ... }}`"
                ));
            }
            return recv;
        }
        // The field is `mut`; check the new value against its declared type, substituting the
        // class's generic parameters from the receiver's type arguments (mirroring `synth_member`).
        if let Some((_, fty)) = self
            .symbols
            .records
            .get(&name)
            .and_then(|fs| fs.iter().find(|(n, _)| n == field))
            .map(|(n, t)| (n.clone(), t.clone()))
        {
            let params = self
                .symbols
                .generic_types
                .get(&name)
                .cloned()
                .unwrap_or_default();
            let subst: HashMap<String, Type> = params
                .iter()
                .cloned()
                .zip(recv_args.iter().cloned())
                .collect();
            let pset: HashSet<String> = params.into_iter().collect();
            let expected = erase_type_params(apply_subst(&fty, &subst), &pset);
            if !self.assignable(&vty, &expected) {
                self.error(
                    DiagnosticCode::TypeMismatch,
                    value.span(),
                    format!("field `{field}` has type `{expected}`, but the value is `{vty}`"),
                );
            }
        }
        recv
    }

    /// The return type of a method call `recv.name(...)`: a built-in method, a user-declared
    /// method, or — when the receiver defers to runtime (`dyn`/hole) — the deferred type itself.
    /// Type a method-bundle method call (kernel-methods K2): an `Element` method on a bound
    /// `@packed` type, or a `Bulk` method on a `List<T>` of one. On a hit: checks arity and
    /// argument types against the bundle's declared signature (nominal — the shape requirement
    /// was already verified at the impl site), records the call-site route for lowering, and
    /// returns the method's type under the receiver-at-0 convention. `None` = not a bundle
    /// method; the caller falls through to the ordinary paths.
    pub(crate) fn bundle_method_call(
        &mut self,
        recv: &Type,
        name: &str,
        args: &[Type],
        span: Span,
        call_span: Span,
    ) -> Option<Type> {
        use noeta_ext_abi::BundleReceiver;
        let (type_name, receiver_kind) = match recv {
            Type::Named(n, targs) if targs.is_empty() => (n, BundleReceiver::Element),
            Type::List(elem) => match elem.as_ref() {
                Type::Named(n, targs) if targs.is_empty() => (n, BundleReceiver::Bulk),
                _ => return None,
            },
            _ => return None,
        };
        let bindings = self.symbols.bundle_impls.get(type_name)?;
        let (route, method) = bindings.iter().find_map(|b| {
            b.bundle
                .method(name)
                .filter(|m| m.receiver == receiver_kind)
                .map(|m| ((b.module.clone(), b.bundle.name.to_string()), m))
        })?;
        let params = stdlib::bundle_method_params(self.reg(), &method.sig, args);
        let required = noeta_ext_abi::SigType::required_count(method.sig.params);
        self.check_args(&params, required, args, &[], span, name);
        self.sites.bundle_call_sites.insert(call_span, route);
        Some(stdlib::bundle_method_return(
            self.reg(),
            &method.sig,
            recv,
            args,
        ))
    }

    pub(crate) fn method_call_return(&self, recv: &Type, name: &str) -> Type {
        if let Some(t) = stdlib::method_return(self.reg(), recv, name) {
            return t;
        }
        if let Type::Named(n, _) = recv
            && let Some(sig) = self.symbols.methods.get(&(n.clone(), name.to_string()))
        {
            return sig.ret.clone();
        }
        // A method call on a trait object (L1 user traits, UT4) resolves against the trait's declared
        // signatures — dispatched dynamically at runtime, but statically typed by the contract.
        if let Type::DynTrait(tr) = recv
            && let Some(decl) = self.symbols.user_traits.get(tr)
            && let Some(m) = decl.methods.iter().find(|m| m.sig.name == name)
        {
            return field_type(&m.sig.ret, &self.imports.extern_types);
        }
        if recv.defers_to_runtime() {
            return recv.clone();
        }
        Type::Unknown
    }

    /// Whether field `field` of type `type_name` is accessible at the current checking context
    /// (object-model slice 2d): a public field always is; a private one (a `class` field not
    /// declared `pub`) only inside the declaring type's own methods/destructor ([`Self::current_type`]).
    pub(crate) fn field_visible(&self, type_name: &str, field: &str) -> bool {
        let private = self
            .symbols
            .private_fields
            .get(type_name)
            .is_some_and(|fs| fs.contains(field));
        // White-box for dev-tier (`@test`/…) fn bodies: co-located tooling sees its module's
        // privates (slice 6d), so a private field is visible there regardless of `current_type`.
        !private
            || self.coloring.in_dev_tier
            || self.coloring.current_type.as_deref() == Some(type_name)
    }

    /// Report an access to a private field from outside its type (E0035). `access` names the action
    /// for the message — a closed [`FieldAccess`] so a call site cannot invent a verb.
    pub(crate) fn report_private_field(
        &mut self,
        type_name: &str,
        field: &str,
        access: FieldAccess,
        span: Span,
    ) {
        let verb = access.verb();
        self.error(
            DiagnosticCode::PrivateField,
            span,
            format!("cannot {verb} private field `{field}` of `{type_name}` from outside it"),
        )
        .help(format!(
            "fields of a `class` are private by default; declare it `pub {field}: ...` to expose \
                 it, or go through a method"
        ));
    }

    /// The **root-qualified namespace prefix** an expression denotes, if it is a namespace group
    /// (`http` bound by `use std.http` → `"std.http"`) or a deeper namespace member chain
    /// (`http.v2` → `"std.http.v2"`). `None` for anything that is not a pure namespace path — a
    /// value, a concrete module, or a type. A local binding shadows a namespace of the same name.
    pub(crate) fn resolve_namespace_prefix(&self, expr: &Expr, env: &Env) -> Option<String> {
        use noeta_ext_abi::registry::NsChild;
        match expr {
            Expr::Ident { name, .. } if lookup(env, name).is_none() => {
                self.imports.namespaces.get(name).cloned()
            }
            Expr::Member { receiver, name, .. } => {
                let prefix = self.resolve_namespace_prefix(receiver, env)?;
                match self.reg().resolve_namespace_child(&prefix, name) {
                    NsChild::Namespace(sub) => Some(sub),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// If `expr` is a namespace-group member chain resolving to a concrete native **module**
    /// (`http.client` from `use std.http`), return its root-qualified identity (`std.http.client`)
    /// and record the `Member` span in `namespace_module_sites` so lowering emits an
    /// [`Rvalue::NativeModule`] carrying the leaf identity. `None` when the chain is not a namespace
    /// path or the final hop is not a module (a sub-namespace, a type, or unresolved).
    pub(crate) fn resolve_namespace_module(&mut self, expr: &Expr, env: &Env) -> Option<String> {
        use noeta_ext_abi::registry::NsChild;
        let Expr::Member {
            receiver,
            name,
            span,
            ..
        } = expr
        else {
            return None;
        };
        let prefix = self.resolve_namespace_prefix(receiver, env)?;
        match self.reg().resolve_namespace_child(&prefix, name) {
            NsChild::Module(qm) => {
                self.sites.namespace_module_sites.insert(*span, qm.clone());
                Some(qm)
            }
            _ => None,
        }
    }

    /// Report an unresolved member on a namespace group (`http.nope`, whether read or called) — a
    /// bare-name miss (E0005). A group is fully enumerable, so an unknown member is never a forward
    /// reference; when a child name is a plausible typo we attach a "did you mean" hint. `prefix` is
    /// the group's root-qualified identity; the message names it as written in source (root stripped).
    pub(crate) fn namespace_member_error(&mut self, prefix: &str, name: &str, span: Span) {
        let group = prefix.split_once('.').map_or(prefix, |(_, rest)| rest);
        let candidates = self.reg().namespace_children(prefix);
        let suggestion = noeta_diagnostics::closest(name, candidates.iter().map(String::as_str))
            .map(str::to_string);
        let diag = self.error(
            DiagnosticCode::UnknownName,
            span,
            format!("namespace `{group}` has no member `{name}`"),
        );
        if let Some(s) = suggestion {
            diag.help(format!("did you mean `{s}`?"));
        }
    }

    pub(crate) fn synth_member(
        &mut self,
        receiver: &Expr,
        name: &str,
        name_span: Span,
        member_span: Span,
        env: &mut Env,
    ) -> Type {
        // `Type.Variant` (a nullary enum constructor like `Status.Paid`) reads as the enum type. For a
        // generic enum a payload-free variant pins no parameter, so its arguments infer to `dyn`
        // (R2b) — keeping the arity consistent with a payload variant of the same enum.
        if let Expr::Ident { name: tn, .. } = receiver
            && self.is_enum_variant(tn, name)
        {
            return self.enum_construction_type(tn, name, &[], member_span);
        }
        // `Type.method` in value position (not the callee of a call) is an unbound **method handle**:
        // a callable taking the receiver as its first argument (prelude-redesign MH). Guarded to a
        // bare type name not shadowed by a local, naming a method of a user type. Typed
        // `Fn(ReceiverType, ...method_params) -> ret`; the resolution is recorded so lowering emits an
        // `Rvalue::MethodHandle`. (Built-in-type receivers — `list.len` — land in a later slice.)
        if let Expr::Ident { name: tn, .. } = receiver
            && lookup(env, tn).is_none()
            && let Some(sig) = self.symbols.methods.get(&(tn.clone(), name.to_string()))
        {
            // The handle's shape follows the derived classification (EX.2): an INSTANCE method's
            // handle takes the receiver as its first argument (`Fn(T, ...params) -> ret`); an
            // ASSOCIATED function's handle is the function itself (`Fn(params) -> ret`) — e.g.
            // `ctor = Stack.new`.
            let instance = self
                .symbols
                .method_instance
                .get(&(tn.clone(), name.to_string()))
                .copied()
                .unwrap_or(true);
            let mut params = Vec::with_capacity(sig.params.len() + 1);
            if instance {
                params.push(Type::Named(tn.clone(), Vec::new()));
            }
            params.extend(sig.params.iter().cloned());
            let ret = sig.ret.clone();
            self.sites
                .handle_sites
                .insert(member_span, (tn.clone(), name.to_string(), !instance));
            return Type::Fn {
                params,
                ret: Box::new(ret),
            };
        }
        // The same for a **built-in** type receiver (`list.len`, `string.upper`): a bare built-in
        // type name (not shadowed) whose `name` is one of its built-in methods → an instance handle
        // `Fn(ReceiverType, ...method_params) -> ret` (prelude-redesign MH.2). Built-in types have no
        // associated fns, so a built-in handle is always instance.
        if let Expr::Ident { name: tn, .. } = receiver
            && lookup(env, tn).is_none()
            && let Some(recv_ty) = builtin_receiver_type(tn)
            && let Some(ret) = stdlib::method_return(self.reg(), &recv_ty, name)
        {
            let mut params = vec![recv_ty.clone()];
            params.extend(stdlib::method_params(self.reg(), &recv_ty, name).unwrap_or_default());
            self.sites
                .handle_sites
                .insert(member_span, (tn.clone(), name.to_string(), false));
            return Type::Fn {
                params,
                ret: Box::new(ret),
            };
        }
        // A namespace-group member access (`http.client` from `use std.http`) in value position:
        // resolve one hop against the group prefix. A landing module records its span so lowering
        // materializes the leaf module value; a sub-namespace or extension type is a valid
        // intermediate. An unresolved member is a hard error (`http.nope`) — a group is fully
        // enumerable, so this is never a forward reference. The group handle is never a value on its
        // own, so this precedes the generic receiver synth below (which would treat `http` as an
        // unknown name).
        if let Some(prefix) = self.resolve_namespace_prefix(receiver, env) {
            use noeta_ext_abi::registry::NsChild;
            match self.reg().resolve_namespace_child(&prefix, name) {
                NsChild::Module(qm) => {
                    self.sites.namespace_module_sites.insert(member_span, qm);
                }
                NsChild::None => {
                    self.namespace_member_error(&prefix, name, member_span);
                }
                // A sub-namespace or extension type reached as a value is not statically typed here
                // (associated calls resolve through the call path); no error.
                NsChild::Namespace(_) | NsChild::Type(_) => {}
            }
            return Type::Unknown;
        }
        let recv = self.synth(receiver, env);
        if let Type::Named(n, recv_args) = &recv
            && let Some(ty) = self
                .symbols
                .records
                .get(n)
                .and_then(|fields| fields.iter().find(|(fname, _)| fname == name))
                .map(|(_, ty)| ty.clone())
        {
            // A private field is readable only inside its declaring type's own methods (slice 2d).
            if !self.field_visible(n, name) {
                self.report_private_field(n, name, FieldAccess::Read, name_span);
            }
            // Fusable indexed field read: `list[i].field`, where the index receiver typed as a
            // built-in `List` (recorded in the `Expr::Index` arm) and the field resolved on the
            // element type `n`. Lowering reads `index_field_sites` to emit a single `Rvalue::IndexField`
            // (P-PACK 2.5+); restricting to a `List` receiver keeps the backends' fast path / boxed
            // fallback list-only (no map/string/`Index`-trait dispatch to reproduce).
            if let Expr::Index { span: idx_span, .. } = receiver
                && self.coloring.index_on_list.contains(idx_span)
            {
                self.sites.index_field_sites.insert(member_span);
            }
            // Substitute the class's type parameters from the receiver's type arguments, so a field
            // of a `Box<int>` reads as `int`. An unresolved parameter (the receiver's arguments are
            // unknown, e.g. from a literal) erases to `dyn` rather than leaking the parameter name.
            let params = self
                .symbols
                .generic_types
                .get(n)
                .cloned()
                .unwrap_or_default();
            let subst: HashMap<String, Type> = params
                .iter()
                .cloned()
                .zip(recv_args.iter().cloned())
                .collect();
            // Inside the generic type's OWN body (`self.value` in a method of `Box<T>`), `T` is in
            // scope and must stay `T` — erasing it to `dyn` would break `fn get(): T { return
            // self.value }` (prelude-redesign EX.1: this path now serves what the retired bare
            // field read did). Only parameters NOT in scope erase.
            let pset: HashSet<String> = params
                .into_iter()
                .filter(|p| !self.coloring.type_params.contains_key(p))
                .collect();
            return erase_type_params(apply_subst(&ty, &subst), &pset);
        }
        // `value.method` in value position — a **bound** method handle (EX.2b): the receiver is
        // captured at bind time; the handle is `Fn(params) -> ret` (no receiver parameter). Checked
        // AFTER the field path, so a same-named field keeps winning member access. Covers user
        // types (instance methods only — binding an associated fn through a value is the E0047
        // wrong-way shape) and built-in receivers (`xs.len`, `s.upper`).
        if let Type::Named(n, _) = &recv
            && let Some(sig) = self.symbols.methods.get(&(n.clone(), name.to_string()))
        {
            let params = sig.params.clone();
            let ret = sig.ret.clone();
            let instance = self
                .symbols
                .method_instance
                .get(&(n.clone(), name.to_string()))
                .copied()
                .unwrap_or(true);
            // Binding an ASSOCIATED function through a value is the wrong-way shape (E0047) —
            // there is no receiver to capture; bind it off the type instead.
            if !instance {
                self.error(
                    DiagnosticCode::InvalidReceiver,
                    member_span,
                    format!("`{name}` is an associated function of `{n}`"),
                )
                .help(format!("bind it off the type: `{n}.{name}`"));
            } else {
                self.sites.bound_handle_sites.insert(member_span);
            }
            return Type::Fn {
                params,
                ret: Box::new(ret),
            };
        }
        if !matches!(recv, Type::Unknown | Type::Dyn)
            && let Some(ret) = stdlib::method_return(self.reg(), &recv, name)
        {
            let params = stdlib::method_params(self.reg(), &recv, name).unwrap_or_default();
            self.sites.bound_handle_sites.insert(member_span);
            return Type::Fn {
                params,
                ret: Box::new(ret),
            };
        }
        // A field/member access on a `dyn` (or hole) receiver stays deferred.
        if recv.defers_to_runtime() {
            return recv;
        }
        Type::Unknown
    }
}
