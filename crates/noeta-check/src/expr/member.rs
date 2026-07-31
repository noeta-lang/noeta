//! **Member access typing**: field get/set with privacy (E0033/private-field reporting),
//! bundle-method dispatch (kernel-methods K2), namespace-group resolution, and the full
//! `synth_member` receiver dispatch. All `Checker` methods moved verbatim out of the crate root.

use crate::expr::calls::closed_to_new_methods;
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
            && !lookup_mutable(env, recv_name.as_str())
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
        args: &mut [Type],
        arg_exprs: &[noeta_ast::CallArg],
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
        // Resolve the bound kernel **trait** + method (the surface adapter reads `bundle_impls`, the
        // typing index; the identity is the one `ExtTrait`). `trait_q` is the trait's qualified identity
        // (`std.vec.Kernels`) — the runtime dispatch key — and `assoc_types` resolves `Self::Wide` /
        // `Self::Float` returns from the bound element (ExtBundle→ExtTrait fold-in, slice 4).
        let (trait_q, method, assoc_types) = bindings.iter().find_map(|b| {
            b.bundle
                .methods
                .iter()
                .find(|m| m.sig.name == name && m.receiver == receiver_kind)
                .map(|m| (b.bundle.qualified(), m, b.bundle.assoc_types))
        })?;
        // `Self` is the IMPLEMENTOR — the bound `@packed` struct — not the receiver: an `Element`
        // method's receiver is `Self` while a `Bulk` method's is `List<Self>`, and both spell their
        // operand relative to the same type. The uniform element backs `Self::Name` projections
        // (`dot` → `int` for an i16 vector, `f32` for an f32 one). Both are resolved BEFORE the
        // parameter types, which now depend on them.
        let self_ty = Type::Named(type_name.clone(), vec![]);
        let elem = self
            .packed_layout(&self_ty)
            .and_then(|layout| stdlib::packed_elem_type(&layout));
        let params = stdlib::bundle_method_params(
            self.reg(),
            &method.sig,
            args,
            &self_ty,
            assoc_types,
            elem.as_ref(),
        );
        let required = noeta_ext_abi::SigType::required_count(method.sig.params);
        // A kernel method's declared parameter names bind a label here, through the same
        // `order_arguments` a declared call uses — `v.scale(factor: 2.0)`. Passing the argument
        // EXPRESSIONS (this used to pass `&[]`) is also what lets `check_args` see a label it
        // cannot bind and refuse it, instead of the list arriving label-free and the label being
        // silently dropped on the floor.
        let bound = self.bind_sig_args(
            &method.sig,
            arg_exprs,
            &params,
            required,
            args,
            span,
            call_span,
        );
        let arg_exprs = bound.as_deref().unwrap_or(arg_exprs);
        self.check_args(&params, required, args, arg_exprs, span, name);
        // Record the `(trait, method)` route: the fold-in unified the bundle runtime route onto the
        // trait route, so every kernel method dispatches through `Registry::dispatch_trait_method`.
        self.sites
            .trait_call_sites
            .insert(call_span, (trait_q, name.to_string()));
        // The same `elem` resolved above types the `Self::Wide` / `Self::Float` returns against the
        // concrete field kind of the struct this trait was `impl`-bound to; `SameAsArg(0)` returns
        // are `Self` (element) / `List<Self>` (bulk).
        Some(stdlib::bundle_method_return(
            self.reg(),
            &method.sig,
            recv,
            args,
            assoc_types,
            elem.as_ref(),
        ))
    }

    /// Type a **trait default-body** method call (ExtBundle→ExtTrait convergence, slice 2): a method
    /// on a concrete receiver whose type implements a **native** trait, where the method is that
    /// trait's *defaulted* method, the trait carries a native default-body dispatch
    /// ([`noeta_ext_abi::ExtTrait::dispatch`]), and the receiver's type provides **no override**. On a
    /// hit: records the `(trait-qualified, method)` route at the call span for lowering to bake in
    /// (source 2 of the three answer sources), checks arity/argument types against the trait's declared
    /// signature, and returns the method's declared return type. `None` = not a trait-answered call;
    /// the caller falls through to `method_call_return` (source 1 — the type's own method — and the
    /// `.noe`-default hoist, source 3, are both reached there and take precedence by construction).
    ///
    /// Like the bundle path, only **statically-known concrete receivers** route here — a `dyn` receiver
    /// stays the documented escape hatch (it cannot know the concrete type provides no override).
    pub(crate) fn trait_default_method_call(
        &mut self,
        recv: &Type,
        name: &str,
        args: &[Type],
        span: Span,
        call_span: Span,
    ) -> Option<Type> {
        let Type::Named(type_name, _) = recv else {
            return None;
        };
        // A pure lookup: `native_trait_default_sites` was computed at collect time and already excludes
        // every method the type provides (native inherent or an `impl` override — source 1) and every
        // `.noe` trait (source 3). A hit here is exactly a source-(2) trait-answered call.
        let (qualified, local) = self
            .symbols
            .native_trait_default_sites
            .get(&(type_name.clone(), name.to_string()))?
            .clone();
        // Type the call from the synthesized decl (identical to the `dyn Trait` typing) — native traits
        // carry no type parameters, so no substitution is needed.
        let decl = self.symbols.user_traits.get(&local)?;
        let m = decl.methods.iter().find(|m| m.sig.name == name)?;
        let params: Vec<Type> = m
            .sig
            .params
            .iter()
            .map(|p| param_type(p, &self.imports.extern_types))
            .collect();
        let required = required_params(&m.sig.params);
        // `async` is part of the return type on every path that reads a signature (`async fn m(): T`
        // is called for a `Future<T>`), so this one wraps too — a native trait's synthesized decl is
        // never `async` today, but the rule belongs with the read, not with today's registry.
        let ret = async_return(
            m.sig
                .ret
                .as_ref()
                .map(|t| from_ref_q(t, &self.imports.extern_types))
                .unwrap_or(Type::Unknown),
            m.sig.is_async,
        );
        self.sites
            .trait_call_sites
            .insert(call_span, (qualified, name.to_string()));
        self.check_args(&params, required, args, &[], span, name);
        Some(ret)
    }

    /// Resolve a method call on an in-scope **type parameter** through its user-trait bounds
    /// (S4.3c, typed): the first bound whose trait declares `name` types the call, its signature
    /// substituted at the bound's instantiation — under `<T: Keyed<int>>`, `x.key()` is `int` and
    /// `x.same(other)` demands an `int`. A bare bound on a generic trait substitutes its
    /// parameters permissively (`dyn`). Returns `(parameter types, required count, return type)`;
    /// `None` when the receiver is not a bounded parameter or no bound's trait declares `name` —
    /// the caller stays lenient exactly as before (bounds license, they don't close the world).
    pub(crate) fn type_param_trait_method(
        &self,
        param: &str,
        name: &str,
    ) -> Option<(Vec<Type>, usize, Type)> {
        let bounds = self.coloring.type_params.get(param)?;
        for b in bounds {
            let Some(decl) = self.symbols.user_traits.get(&b.name) else {
                continue;
            };
            let Some(m) = decl.methods.iter().find(|m| m.sig.name == name) else {
                continue;
            };
            let subst: HashMap<String, Type> = decl
                .type_params
                .iter()
                .enumerate()
                .map(|(i, tp)| (tp.name.clone(), b.args.get(i).cloned().unwrap_or(Type::Dyn)))
                .collect();
            let params: Vec<Type> = m
                .sig
                .params
                .iter()
                .map(|p| apply_subst(&param_type(p, &self.imports.extern_types), &subst))
                .collect();
            let ret = async_return(
                m.sig
                    .ret
                    .as_ref()
                    .map(|t| from_ref_q(t, &self.imports.extern_types))
                    .unwrap_or(Type::Unknown),
                m.sig.is_async,
            );
            return Some((
                params,
                required_params(&m.sig.params),
                apply_subst(&ret, &subst),
            ));
        }
        None
    }

    /// Resolve a method call on a **trait object** (`dyn Trait`, UT4) against the trait's declared
    /// contract — the `dyn` twin of [`Self::type_param_trait_method`], and deliberately its mirror
    /// image so the two receivers can never disagree about the same method. Returns `(parameter
    /// types, required count, return type)`; `None` when `tr` names no known trait or the trait
    /// declares no `name`.
    ///
    /// Two things the raw signature read this replaced got wrong, both of which the bound path had
    /// always got right:
    ///
    /// * **`async` is part of the return type.** A call to an `async fn m(): T` produces
    ///   `Future<T>` ([`async_return`]) — the runtime returns a future through `dyn` dispatch
    ///   exactly as it does through a bound, so typing the call `T` was a soundness hole: the
    ///   program declared `string` and held a `<future>`.
    /// * **A generic trait's parameters must be substituted.** `dyn Trait` carries no type
    ///   arguments (the surface has no `dyn Trait<...>` form), so — exactly as for a bare bound on a
    ///   generic trait — its parameters instantiate permissively to `dyn`. Leaving them raw leaked
    ///   the trait's own parameter name into the call's type (`s.get(k)` typed as `V`), which then
    ///   mismatched every real type it met.
    pub(crate) fn dyn_trait_method(
        &self,
        tr: &str,
        name: &str,
    ) -> Option<(Vec<Type>, usize, Type)> {
        let decl = self.symbols.user_traits.get(tr)?;
        let m = decl.methods.iter().find(|m| m.sig.name == name)?;
        let subst: HashMap<String, Type> = decl
            .type_params
            .iter()
            .map(|tp| (tp.name.clone(), Type::Dyn))
            .collect();
        let params: Vec<Type> = m
            .sig
            .params
            .iter()
            .map(|p| apply_subst(&param_type(p, &self.imports.extern_types), &subst))
            .collect();
        let ret = async_return(
            field_type(&m.sig.ret, &self.imports.extern_types),
            m.sig.is_async,
        );
        Some((
            params,
            required_params(&m.sig.params),
            apply_subst(&ret, &subst),
        ))
    }

    /// How `type_name.name` may be reached ([`Receiver`]) — the one place the table is consulted,
    /// so every call form (`T.m(…)`, `x.m(…)`, either of those with a turbofish, and both handle
    /// spellings) asks the same question and gets the same answer. An unrecorded method is
    /// `Either`: the checker knows nothing that forbids a spelling, and refusing one would be
    /// inventing a rule out of missing data.
    pub(crate) fn receiver_of(&self, type_name: &str, name: &str) -> Receiver {
        self.symbols
            .method_receiver
            .get(&(type_name.to_string(), name.to_string()))
            .copied()
            .unwrap_or(Receiver::Either)
    }

    pub(crate) fn method_call_return(&self, recv: &Type, name: &str) -> Type {
        // A native (fielded/extern) method whose return is a trait associated-type projection
        // `Self::Name` / `List<Self::Name>` (slice 1b): resolve it against `trait_assoc` at this
        // concrete receiver — the type the implementing type's `AssocDerivation` computed at seed
        // time. Checked BEFORE `method_return` (whose `sig_to_type` would erase the projection to a
        // hole); an unresolved projection degrades to that same gradual hole.
        if let Some(t) = self.native_method_assoc_return(recv, name) {
            return t;
        }
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
            && let Some((_, _, ret)) = self.dyn_trait_method(tr, name)
        {
            return ret;
        }
        // `@derive(Serialize<Json>)` synthesizes a structural `to_json(): string`, and it is the one
        // derive-provided member with no entry anywhere else here: `BuiltinTrait::Serialize` has no
        // `required_method` (its whole body is synthesized rather than written), so the trait table
        // says the type serializes without saying what that gives it. Typed here rather than left
        // `Unknown` for two reasons — `x.to_json().len()` should check like the `string` it is, and
        // the closed-user-type guard reads a `Unknown` return as proof the member does not exist.
        if let Type::Named(n, _) = recv
            && name == "to_json"
            && self
                .symbols
                .trait_impls
                .get(n.as_str())
                .is_some_and(|ts| ts.contains(&noeta_types::BuiltinTrait::Serialize))
        {
            return Type::String;
        }
        if recv.defers_to_runtime() {
            return recv.clone();
        }
        Type::Unknown
    }

    /// Resolve a native method's associated-type return `Self::Name` (slice 1b) against `trait_assoc`
    /// at a concrete receiver. Returns the concrete `Type` the implementing type's [`AssocDerivation`]
    /// computed (`seed_ext_traits` folded it into `trait_assoc[(type, trait)]`), wrapped in `List<_>`
    /// for the `List<Self::Name>` form. `Some(Type::Unknown)` when the method IS an associated-type
    /// projection but no binding resolves (a gradual hole, never a wrong type); `None` when the method
    /// is not an associated-type projection at all (defer to the ordinary [`stdlib::method_return`]).
    fn native_method_assoc_return(&self, recv: &Type, name: &str) -> Option<Type> {
        let Type::Named(type_name, _) = recv else {
            return None;
        };
        let (assoc, wrap_list) = match stdlib::native_method_assoc_ret(self.reg(), recv, name)? {
            stdlib::AssocRet::Bare(a) => (a, false),
            stdlib::AssocRet::List(a) => (a, true),
        };
        let resolved = self
            .resolve_assoc_for_type(type_name, assoc)
            .unwrap_or(Type::Unknown);
        Some(if wrap_list {
            Type::List(Box::new(resolved))
        } else {
            resolved
        })
    }

    /// The concrete `Type` bound to associated type `assoc` for `type_name` by ANY trait it
    /// implements (slice 1b) — the native-trait analogue of [`Self::resolve_assoc`], which needs the
    /// trait named. A native method's `Self::Name` return carries no trait context, but the assoc
    /// name is unique to the (one) native trait declaring it, so the first `trait_assoc[(type_name,
    /// _)]` entry binding `assoc` is the answer.
    fn resolve_assoc_for_type(&self, type_name: &str, assoc: &str) -> Option<Type> {
        self.symbols
            .trait_assoc
            .iter()
            .find(|((t, _), map)| t == type_name && map.contains_key(assoc))
            .map(|(_, map)| map[assoc].clone())
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
            Expr::Ident { name, .. } if lookup(env, name.as_str()).is_none() => {
                self.imports.namespaces.get(name.as_str()).cloned()
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

    /// The declared type of field `name` on the user type `n`, **instantiated at the receiver's
    /// type arguments** — the one field-type computation shared by value-position member access
    /// ([`Self::synth_member`]) and the field-call desugar in the call arm, so the two positions
    /// can never disagree on what `obj.f` is. Substitutes the type's generic parameters from
    /// `recv_args` (a field of a `Box<int>` reads as `int`); a parameter the receiver leaves
    /// unresolved erases to `dyn` — except inside the generic type's OWN body, where `T` is in
    /// scope and must stay `T` (`fn get(): T { return self.value }`, prelude-redesign EX.1).
    /// `None` when `n` has no such field.
    pub(crate) fn record_field_type(
        &self,
        n: &str,
        name: &str,
        recv_args: &[Type],
    ) -> Option<Type> {
        let ty = self
            .symbols
            .records
            .get(n)
            .and_then(|fields| fields.iter().find(|(fname, _)| fname == name))
            .map(|(_, ty)| ty.clone())?;
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
        let pset: HashSet<String> = params
            .into_iter()
            .filter(|p| !self.coloring.type_params.contains_key(p))
            .collect();
        Some(erase_type_params(apply_subst(&ty, &subst), &pset))
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
        //
        // A **payload-carrying** variant in value position is not that, and used to fall through
        // here as if it were: `Shape.Circle` where `Circle(int)` typed as `Shape` and then died in
        // the backend with `internal error: the VM cannot compile this program`. That is the
        // checks-clean-then-fails shape — a value the type system believes in that neither runtime
        // has heard of — reached through the enum-member spelling. Naming the variant as a
        // *constructor value* (`Fn(int) -> Shape`, the way `some`/`Ok` are first-class) is a real
        // feature and a real slice; until it exists the honest answer is a static error, because
        // the expression has no value at run time.
        if let Expr::Ident { name: tn, .. } = receiver
            && let Some(key) = self.enum_type_key(tn.as_str())
            && let Some(fields) = self.enum_variant_fields(&key, name)
        {
            if fields == 0 {
                return self.enum_construction_type(&key, name, &[], member_span);
            }
            self.error(
                DiagnosticCode::TypeMismatch,
                member_span,
                format!(
                    "`{tn}.{name}` carries {fields} value{}, so it is a constructor and not a value",
                    match fields {
                        1 => "",
                        _ => "s",
                    }
                ),
            )
            .help(format!(
                "construct it with its arguments — `{tn}.{name}(…)`; a payload-carrying variant \
                 cannot yet be passed around as a function"
            ));
            return Type::Unknown;
        }
        // `Type.method` in value position (not the callee of a call) is an unbound **method handle**:
        // a callable taking the receiver as its first argument (prelude-redesign MH). Guarded to a
        // bare type name not shadowed by a local, naming a method of a user type. Typed
        // `Fn(ReceiverType, ...method_params) -> ret`; the resolution is recorded so lowering emits an
        // `Rvalue::MethodHandle`. (Built-in-type receivers — `list.len` — land in a later slice.)
        if let Expr::Ident { name: tn, .. } = receiver
            && lookup(env, tn.as_str()).is_none()
            && let Some(sig) = self
                .symbols
                .methods
                .get(&(tn.to_string(), name.to_string()))
        {
            // The handle's shape follows the derived classification (EX.2): an INSTANCE method's
            // handle takes the receiver as its first argument (`Fn(T, ...params) -> ret`); an
            // ASSOCIATED function's handle is the function itself (`Fn(params) -> ret`) — e.g.
            // `ctor = Stack.new`. A trait's self-less method ([`Receiver::Either`]) takes the
            // instance shape, which is what the unclassified entry already meant here.
            let instance = self.receiver_of(tn.as_str(), name).handle_takes_receiver();
            let mut params = Vec::with_capacity(sig.params.len() + 1);
            if instance {
                params.push(Type::Named(tn.to_string(), Vec::new()));
            }
            params.extend(sig.params.iter().cloned());
            let ret = sig.ret.clone();
            self.sites
                .handle_sites
                .insert(member_span, (tn.to_string(), name.to_string(), !instance));
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
            && lookup(env, tn.as_str()).is_none()
            && let Some(recv_ty) = builtin_receiver_type(tn.as_str())
            && let Some(ret) = stdlib::method_return(self.reg(), &recv_ty, name)
        {
            let mut params = vec![recv_ty.clone()];
            params.extend(stdlib::method_params(self.reg(), &recv_ty, name).unwrap_or_default());
            self.sites
                .handle_sites
                .insert(member_span, (tn.to_string(), name.to_string(), false));
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
            && let Some(ty) = self.record_field_type(n, name, recv_args)
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
            return ty;
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
            // Binding an ASSOCIATED function through a value is the wrong-way shape (E0047) —
            // there is no receiver to capture; bind it off the type instead.
            if !self.receiver_of(n, name).allows_instance_call() {
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
        // Nothing resolved. On a CLOSED builtin receiver that is proof the member does not exist —
        // the same reasoning (and the same predicate) the *call* path already applies to
        // `s.nope()`. Without it the member path stayed silently `Unknown`, so `p.x` on an
        // `Option<P>`, `"s".nope`, and `[1].nope` all passed `noeta check` and only failed at run
        // time with E0005 — precisely the check-vs-run divergence the call-path guard was added to
        // close, reached through the one spelling it did not cover. `Named`/`dyn`/holes stay
        // lenient for the reasons documented on `closed_to_new_methods`.
        if closed_to_new_methods(&recv) || crate::expr::calls::user_type_is_closed(self, &recv) {
            let diag = self.error(
                DiagnosticCode::TypeMismatch,
                name_span,
                format!("type `{recv}` has no field or method `{name}`"),
            );
            // The overwhelmingly common way to reach here is reading a field *through* an optional
            // (`entry.size` where `entry: ?Entry`), so name the unwrap rather than leave the user
            // to rediscover it.
            if let Type::Option(inner) = &recv
                && matches!(inner.as_ref(), Type::Named(..))
            {
                diag.help(format!(
                    "`{recv}` may be `none`; reach the `{inner}` first — \
                     `match x {{ some(v) => v.{name}, none => … }}`, or `(x ?? fallback).{name}`"
                ));
            }
        }
        Type::Unknown
    }
}
