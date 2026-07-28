//! **Declaration checking**: struct/class/enum bodies, parameter/field default validation,
//! mandatory signatures (E0022), type-parameter scoping and bounds, and type-reference
//! validation (E0013) including named-key capability. All `Checker` methods moved verbatim out
//! of the crate root purely to shrink `lib.rs`.

use super::*;

impl Checker {
    /// Validate a callable's parameter defaults. Two rules: defaults must be **trailing-only** — a
    /// required parameter after a defaulted one is `E0026` — and each default's type must be
    /// assignable to its parameter (`E0007`). The default expression is synthesized in `env` *before
    /// the parameter frame is pushed*, so it sees the function's **definition scope** but not its own
    /// parameters: for a top-level function or method that scope is the module's globals; for a
    /// closure it is the captured enclosing scope (so a closure default may use captured variables,
    /// exactly like the closure body). A default that reaches for a sibling parameter resolves to
    /// nothing — a runtime `E0005`, as elsewhere in the language — rather than silently capturing it.
    pub(crate) fn validate_param_defaults(&mut self, params: &[Param], env: &mut Env) {
        let mut seen_default = false;
        for p in params {
            if p.is_optional() {
                seen_default = true;
            } else if seen_default {
                self.error(
                    DiagnosticCode::RequiredAfterOptional,
                    p.name_span,
                    format!(
                        "required parameter `{}` cannot follow a parameter with a default value",
                        p.name
                    ),
                )
                .help("give this parameter a default too, or move it before the optional ones");
            }
        }
        let tps: HashSet<String> = self.coloring.type_params.keys().cloned().collect();
        for p in params {
            let Some(default) = &p.default else { continue };
            // The parameter's declared type is the default's expected type, so a target-typed
            // `.{ … }` default adopts it (`fn retry(r: Retry = .{ attempts: 3 })`). Only that form
            // routes through `check`; every other default keeps synthesizing exactly as before, so
            // no existing inference changes.
            let declared =
                p.ty.is_some()
                    .then(|| erase_type_params(param_type(p, &self.imports.extern_types), &tps));
            let actual = match (&declared, default) {
                (Some(expected), Expr::Object(l)) if l.type_name.is_none() => {
                    self.check(default, expected, env)
                }
                _ => self.synth(default, env),
            };
            // Skip the type check when the parameter has no annotation (already an `E0022`) or its
            // type is generic/`dyn` (erases to `dyn`, which accepts any default).
            if let Some(expected) = declared
                && !self.arg_assignable(&actual, &expected)
            {
                self.error(
                    DiagnosticCode::TypeMismatch,
                    default.span(),
                    format!(
                        "default value of type `{actual}` is not assignable to parameter type `{expected}`"
                    ),
                );
            }
        }
    }

    /// Validate each field's default value (`x: T = expr`), object-model slice 5. A default is
    /// checked in the type's **definition scope** — the `env` here carries globals only (fields are
    /// not yet bound), so a default that references `self` or a sibling field is an `E0007` unknown
    /// name, matching its globals-only runtime scope. Its inferred type must be assignable to the
    /// field's declared type (`E0007` mismatch). Unlike parameter defaults there is **no
    /// trailing-only rule**: literal fields are named, so a default makes its field optional
    /// regardless of position. Call before binding fields into `env`.
    pub(crate) fn validate_field_defaults(&mut self, fields: &[FieldDecl], env: &mut Env) {
        let tps: HashSet<String> = self.coloring.type_params.keys().cloned().collect();
        for f in fields {
            let Some(default) = &f.default else { continue };
            // The field's declared type is the default's expected type — the field analogue of the
            // parameter rule above, so `r: Retry = .{ attempts: 3 }` names its type.
            let declared = f
                .ty
                .is_some()
                .then(|| erase_type_params(field_type(&f.ty, &self.imports.extern_types), &tps));
            let actual = match (&declared, default) {
                (Some(expected), Expr::Object(l)) if l.type_name.is_none() => {
                    self.check(default, expected, env)
                }
                _ => self.synth(default, env),
            };
            // Skip the type check when the field has no annotation (every field requires one, so an
            // un-annotated field is already reported) or its type erases to `dyn` (accepts any).
            if let Some(expected) = declared
                && !self.arg_assignable(&actual, &expected)
            {
                self.error(
                    DiagnosticCode::TypeMismatch,
                    default.span(),
                    format!(
                        "default value of type `{actual}` is not assignable to field type `{expected}`"
                    ),
                );
            }
        }
    }

    /// Inferred-static requires a full signature on every **named** function or method: a type on
    /// each parameter and a return type. (Closures and local bindings stay inferred — inference
    /// reconstructs them.) Each missing piece is its own `E0022`.
    pub(crate) fn require_signature(&mut self, decl: &FnDecl) {
        for p in &decl.params {
            if p.ty.is_none() {
                self.error(
                    DiagnosticCode::MissingSignature,
                    p.name_span,
                    format!("parameter `{}` needs a type annotation", p.name),
                )
                .help(
                    "every parameter of a named function needs a type; only closures and \
                         locals are inferred",
                );
            }
        }
        if decl.ret.is_none() {
            self.error(
                DiagnosticCode::MissingSignature,
                decl.name_span,
                format!("function `{}` needs a return type", decl.name),
            )
            .help("annotate the return type after the parameters, e.g. `): int`");
        }
    }

    pub(crate) fn check_struct(&mut self, r: &StructDecl, env: &mut Env) {
        let saved = self.enter_type_params(&r.type_params);
        // Only `self` is bound in a method body (prelude-redesign EX.1 — member access is
        // explicit): `self.field` types through `synth_member`; a bare field name is an unknown
        // name with a targeted hint (see the `Expr::Ident` fallback in `synth`).
        let fields: Vec<(String, Type)> = vec![(
            "self".to_string(),
            self_type(r.name.as_str(), &r.type_params),
        )];
        for f in &r.fields {
            self.check_type_opt(&f.ty);
            self.check_attrs(&f.attrs, TargetKind::Field);
        }
        self.validate_field_defaults(&r.fields, env);
        self.check_derives(
            r.name.as_str(),
            &r.decorators.derives,
            &r.fields,
            &r.methods,
        );
        let standalone = self.standalone_for(r.name.as_str());
        // A struct carries in-body `impl Trait { }` blocks and inherent methods (the unified body),
        // checked exactly as a class's — coherence over its impls, then each method body.
        self.check_coherence(&r.decorators.derives, &r.impls, &standalone);
        self.check_attrs(&r.decorators.attrs, TargetKind::Struct);
        // Inside the type's own body, its (always-public) fields are accessible; the marker is
        // uniform with classes (a struct simply has no private fields to gate).
        let saved_type = self.coloring.current_type.replace(r.name.to_string());
        for block in &r.impls {
            self.check_impl(block);
        }
        for method in &r.methods {
            self.check_fn(method, env, &fields, TargetKind::Method);
        }
        self.coloring.current_type = saved_type;
        self.coloring.type_params = saved;
    }

    /// The `(trait, span)` occurrences of every standalone `impl Trait for <name> {}`, cloned so a
    /// `&mut self` coherence check can borrow them without conflicting with `self.symbols.standalone_impls`.
    pub(crate) fn standalone_for(&self, name: &str) -> Vec<(String, Span)> {
        self.symbols
            .standalone_impls
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn check_class(&mut self, c: &ClassDecl, env: &mut Env) {
        let saved = self.enter_type_params(&c.type_params);
        // Only `self` is bound in a method body (prelude-redesign EX.1 — member access is
        // explicit): `self.field` types through `synth_member`; a bare field name is an unknown
        // name with a targeted hint (see the `Expr::Ident` fallback in `synth`).
        let fields: Vec<(String, Type)> = vec![(
            "self".to_string(),
            self_type(c.name.as_str(), &c.type_params),
        )];
        for f in &c.fields {
            self.check_type_opt(&f.ty);
            self.check_attrs(&f.attrs, TargetKind::Field);
        }
        self.validate_field_defaults(&c.fields, env);
        self.check_derives(
            c.name.as_str(),
            &c.decorators.derives,
            &c.fields,
            &c.methods,
        );
        let standalone = self.standalone_for(c.name.as_str());
        self.check_coherence(&c.decorators.derives, &c.impls, &standalone);
        self.check_attrs(&c.decorators.attrs, TargetKind::Class);
        // Inside the class's own methods/destructor its private fields are accessible — on `self`
        // and on any same-type value (the type-scoped privacy rule, object-model slice 2d).
        let saved_type = self.coloring.current_type.replace(c.name.to_string());
        for block in &c.impls {
            self.check_impl(block);
        }
        for method in &c.methods {
            self.check_fn(method, env, &fields, TargetKind::Method);
        }
        if let Some(destructor) = &c.destructor {
            // A destructor is a body without being a method, so it records its own ledger entry
            // (keyed, like the walker's, on the declaring class's name span).
            self.visited_bodies.insert(c.name_span);
            env.push(HashMap::new());
            for (name, ty) in &fields {
                bind(env, name, ty.clone());
            }
            for stmt in destructor {
                self.check_stmt(stmt, env);
            }
            env.pop();
        }
        self.coloring.current_type = saved_type;
        self.coloring.type_params = saved;
    }

    pub(crate) fn check_enum(&mut self, e: &EnumDecl, env: &mut Env) {
        let saved = self.enter_type_params(&e.type_params);
        self.check_type_opt(&e.backing);
        for variant in &e.variants {
            // Both payload spellings annotate their type — `Leaf(Item)` no less than
            // `Leaf(item: Item)` — so one rule reaches both. This used to reconstruct a `TypeRef`
            // by hand for the positional form, whose type the parser stored in the field's *name*.
            for field in &variant.fields {
                self.check_type_opt(&field.ty);
            }
            self.check_attrs(&variant.attrs, TargetKind::Variant);
            // A variant's payload fields are parsed by the shared parameter grammar, so they can
            // carry `#[...]` too. Validate them as parameters — capability gate and construction —
            // rather than leaving the one parameter list in the language whose attributes nothing
            // looks at.
            self.check_param_attrs(&variant.fields);
        }
        self.check_derives(e.name.as_str(), &e.decorators.derives, &[], &e.methods);
        let standalone = self.standalone_for(e.name.as_str());
        // An enum carries in-body `impl Trait { }` blocks and inherent methods (the unified body,
        // object-model slice 3), checked exactly as a class's — coherence over its impls, then each
        // method body.
        self.check_coherence(&e.decorators.derives, &e.impls, &standalone);
        self.check_attrs(&e.decorators.attrs, TargetKind::Enum);
        // Inside an enum's own methods, `self` is the whole enum value (the variants differ, so —
        // unlike a struct/class — there is no implicit per-field scope; a method `match`es on
        // `self`). Bind `self` to the enum type so that `match self` is exhaustiveness-checked, and
        // set `current_type` for the same type-scoped resolution a class uses.
        let self_ty = Type::Named(e.name.to_string(), Vec::new());
        let saved_type = self.coloring.current_type.replace(e.name.to_string());
        for block in &e.impls {
            self.check_impl(block);
        }
        for method in &e.methods {
            self.check_fn(
                method,
                env,
                std::slice::from_ref(&("self".to_string(), self_ty.clone())),
                TargetKind::Method,
            );
        }
        self.coloring.current_type = saved_type;
        self.coloring.type_params = saved;
    }

    // ----- unknown-type resolution (E0013) -----

    /// Install `params` as the in-scope generic type parameters and return the previous set (to
    /// restore once the declaration is checked). Generic parameters are erased at runtime but are
    /// legal referents for annotations within their declaration. Each parameter's trait bounds are
    /// validated here (an unknown trait in a bound is `E0014`).
    pub(crate) fn enter_type_params(
        &mut self,
        params: &[TypeParam],
    ) -> HashMap<String, Vec<BoundReq>> {
        let saved = std::mem::replace(
            &mut self.coloring.type_params,
            params
                .iter()
                .map(|p| {
                    (
                        p.name.clone(),
                        bound_reqs(&p.bounds, &self.imports.extern_types),
                    )
                })
                .collect(),
        );
        // Validated AFTER the parameters enter scope: a bound argument may name a sibling
        // parameter (`<K, T: Keyed<K>>`), which is a legal annotation referent here.
        self.check_type_param_bounds(params);
        saved
    }

    /// Validate each type parameter's trait bounds: a bound must name a built-in trait or a user
    /// trait, else `E0014 UnknownTrait` (reusing the `impl`/`@derive` name-validation path); an
    /// instantiated bound (`T: Keyed<int>`) must match the trait's generic arity — built-ins take
    /// no bound arguments, a generic user trait takes exactly its parameter count (a BARE bound on
    /// a generic trait stays legal and accepts any instantiation). What the bounds demand is what
    /// S4.2 enforces at instantiation; here we only check they are well-formed.
    pub(crate) fn check_type_param_bounds(&mut self, params: &[TypeParam]) {
        for p in params {
            for bound in &p.bounds {
                // A bound may name a built-in trait or a user-defined one (L1, UT3).
                if let Some(decl) = self.symbols.user_traits.get(bound.name.as_str()) {
                    let arity = decl.type_params.len();
                    if !bound.args.is_empty() && bound.args.len() != arity {
                        let msg = if arity == 0 {
                            format!(
                                "`{}` is not generic; the bound takes no type arguments",
                                bound.name
                            )
                        } else {
                            format!(
                                "generic trait `{}` takes {arity} type argument(s), the bound names {}",
                                bound.name,
                                bound.args.len()
                            )
                        };
                        self.error(DiagnosticCode::UnknownTrait, bound.span, msg)
                            .help(
                                "write the bound bare (`T: Trait`, any instantiation) or at the \
                             trait's full arity (`T: Trait<...>`)",
                            );
                    }
                    for arg in &bound.args {
                        self.check_type_ref(arg);
                    }
                    continue;
                }
                if BuiltinTrait::from_name(bound.name.as_str()).is_some() {
                    if !bound.args.is_empty() {
                        self.error(
                            DiagnosticCode::UnknownTrait,
                            bound.span,
                            format!("built-in trait `{}` takes no bound arguments", bound.name),
                        )
                        .help("only a generic user trait is bounded at an instantiation");
                    }
                    continue;
                }
                self.error(
                    DiagnosticCode::UnknownTrait,
                    p.span,
                    format!(
                        "unknown trait `{}` in bound on type parameter `{}`",
                        bound.name, p.name
                    ),
                )
                .help(
                    "a bound must name a built-in trait (e.g. `Comparable`, `Equatable`, \
                         `Display`) or a `trait` you declare",
                );
            }
        }
    }

    pub(crate) fn check_type_opt(&mut self, ty: &Option<TypeRef>) {
        if let Some(ty) = ty {
            self.check_type_ref(ty);
        }
    }

    /// Verify that every named type in an annotation resolves: a built-in, a declared/imported
    /// type, or a generic parameter in scope. An unresolvable name is `E0013`. Generic arguments
    /// are checked recursively, so `List<Ghost>` flags `Ghost`.
    pub(crate) fn check_type_ref(&mut self, ty: &TypeRef) {
        match ty {
            TypeRef::Union { members, .. } => {
                for m in members {
                    self.check_type_ref(m);
                }
            }
            TypeRef::Tuple { elements, .. } => {
                for e in elements {
                    self.check_type_ref(e);
                }
            }
            TypeRef::Fn { params, ret, .. } => {
                for p in params {
                    self.check_type_ref(p);
                }
                self.check_type_ref(ret);
            }
            TypeRef::Optional { inner, .. } => self.check_type_ref(inner),
            // `Self::Name` — an associated-type projection (slice 1a). It names no nominal type to
            // resolve here; the checker projects it per-impl at each typing site (an unbound name
            // degrades to a gradual hole rather than an E0013), so there is nothing to reject.
            TypeRef::AssocProjection { .. } => {}
            // `dyn Trait` — the trait must resolve to a built-in or user-defined trait (L1, UT4).
            TypeRef::DynTrait { trait_name, span } => {
                if BuiltinTrait::from_name(trait_name.as_str()).is_none()
                    && !self.symbols.user_traits.contains_key(trait_name.as_str())
                {
                    self.error(
                        DiagnosticCode::UnknownTrait,
                        *span,
                        format!("unknown trait `{trait_name}` in `dyn {trait_name}`"),
                    )
                    .help("`dyn` must be followed by a built-in trait or a `trait` you declare");
                }
            }
            TypeRef::Named { name, args, span } => {
                if !Type::is_builtin_name(name.as_str())
                    && !PRELUDE_TYPES.contains(&name.as_str())
                    && !self.coloring.type_params.contains_key(name.as_str())
                    && !self.symbols.types.contains(name.as_str())
                    // A native extern type is a valid annotation only when `use`-imported into this
                    // file (`use std.id.Uuid` → `extern_types["Uuid"]`), like a user type — it is no
                    // longer globally in scope by bare name.
                    && !self.imports.extern_types.contains_key(name.as_str())
                    // A module-qualified type rooted at a retained user import (`use geometry.vec`
                    // then `vec.Vec2` in an *isolated* check — REPL/session, a docs fragment): the
                    // linker resolves the dotted head in a full link, and an unlinked fragment
                    // tolerates it exactly like other unresolved external names (F1). Root-only —
                    // std namespace members keep their exact-key resolution and stay strict.
                    && !name
                        .as_str()
                        .split_once('.')
                        .is_some_and(|(root, _)| self.symbols.types.contains(root))
                {
                    let diag = self.error(
                        DiagnosticCode::UnknownType,
                        *span,
                        format!("unknown type `{name}`"),
                    );
                    // `some`/`none`/`Ok`/`Err` are the *constructors* of `Option`/`Result`, not
                    // types — but they are the obvious thing to reach for in `x is …`, since they
                    // are exactly what a `match` arm names. Point at the spelling that works
                    // rather than at the type catalog, which has nothing for the user to find.
                    match name.as_str() {
                        "some" | "none" => diag.help(
                            "`some`/`none` are an optional's values, not types — test one with \
                             `x != none` / `x == none`, or take it apart with \
                             `match x { some(v) => …, none => … }`",
                        ),
                        "Ok" | "Err" => diag.help(
                            "`Ok`/`Err` are a `Result`'s values, not types — take it apart with \
                             `match x { Ok(v) => …, Err(e) => … }`, or propagate it with `?`",
                        ),
                        _ => diag.help(
                            "name a declared type, one imported with `use` (native types too, \
                             e.g. `use std.id.Uuid`), a generic parameter, or a built-in",
                        ),
                    };
                }
                // Key-capability gate (extern-types X4): a `Map<K, _>` key / `Set<T>` element
                // formed from an extern type requires it key-capable — a mutable handle's hash
                // or order could go stale under a key, so `Map<FileHandle, _>` is a type error.
                let key_role = keyed_container_role(name.as_str());
                if let Some((role, is_set)) = key_role
                    && let Some(TypeRef::Named {
                        name: key_name,
                        span: key_span,
                        ..
                    }) = args.first()
                    && self.named_key_capable(key_name.as_str(), is_set) == Some(false)
                {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        *key_span,
                        format!("`{key_name}` cannot {role}: it is not a key-capable type"),
                    )
                    .help(
                        "key-capable types are strings, key-capable extern types (e.g. `Uuid`), \
                         and `@packed` structs of int/bool fields; a set additionally admits any \
                         value kind (struct/enum) ordering structurally",
                    );
                }
                self.check_type_args(name.as_str(), args, *span);
            }
        }
    }

    /// Validate a named type's **generic arguments**: each argument is itself a type reference that
    /// must resolve (`List<Ghost>` flags `Ghost`, E0013), and a built-in constructor applied to the
    /// wrong number of them is E0058.
    ///
    /// Arity is read from [`BuiltinTy::arity`] — the one table that knows a `List` takes one
    /// argument and a `Map` two — so the rule is stated once rather than per use site. Two forms are
    /// deliberately admitted: **no** arguments at all (`x: List` is an inference hole the checker
    /// fills forward, not an error), and the bare lowercase spellings `list`/`map`/`set`, which are
    /// *defined* as the unspecified-element form. Only a written argument list of the wrong length
    /// is diagnosed. User generic types are not arity-checked here; instantiation owns that.
    ///
    /// Shared by [`check_type_ref`](Self::check_type_ref) and the attribute-argument type reference,
    /// which reaches a `TypeRef` through [`AttrValue::TypeRef`] rather than an annotation — the
    /// second caller is why this is a method and not inlined above. Its arguments used to be
    /// validated by nobody at all, so `#[Builds(target: List<int, string, bogus>)]` passed silently.
    pub(crate) fn check_type_args(&mut self, name: &str, args: &[TypeRef], span: Span) {
        if let Some((builtin, spelling)) = BuiltinTy::from_name(name)
            && spelling == noeta_ast::Spelling::Canonical
            && !args.is_empty()
            && args.len() != builtin.arity()
        {
            let arity = builtin.arity();
            let msg = if arity == 0 {
                format!("`{name}` takes no type arguments, found {}", args.len())
            } else {
                format!(
                    "`{name}` takes {arity} type argument(s), found {}",
                    args.len()
                )
            };
            self.error(DiagnosticCode::InvalidTypeArguments, span, msg)
                .help("supply exactly one type argument per parameter, or omit `<…>` entirely and let the element type infer");
        }
        for arg in args {
            self.check_type_ref(arg);
        }
    }

    /// Check-time key-capability of a **named** type in `Map<K, _>` key / `Set<T>` element
    /// position (P-PKEY S3/S4): `Some(true)` for `string`, the integer family (S4 — `int` and
    /// every fixed-width `{i,u}N`, erased to the same word), a key-capable extern, or a
    /// key-capable `@packed` struct (all fields int/`{i,u}N`/bool or nested such structs — no
    /// floats); `Some(false)` for `float`/`f32` (the NaN footgun), `bool` in MAP position
    /// (`for_set` splits the role: an orderable `Set<bool>` is fine), a known user record/enum, or a
    /// non-capable extern (statically certain to abort at runtime); `None` when the name is not
    /// a resolvable concrete type here (a generic parameter, an unknown — other diagnostics own
    /// those).
    pub(crate) fn named_key_capable(&self, key_name: &str, for_set: bool) -> Option<bool> {
        fn layout_key_capable(layout: &noeta_ast::reflect::PackedLayout) -> bool {
            use noeta_ast::reflect::PackedKind;
            layout.fields.iter().all(|f| match &f.kind {
                PackedKind::Int | PackedKind::IntN { .. } | PackedKind::Bool => true,
                PackedKind::Float | PackedKind::F32 | PackedKind::F64 => false,
                PackedKind::Struct(inner) => layout_key_capable(inner),
            })
        }
        // A built-in scalar decides here, exhaustively over `BuiltinTy` — the string list this
        // replaces knew `float`/`f32` but not `f64`, so a `Map<f64, _>` annotation slipped
        // through the gate a `Map<float, _>` failed (the silent-fallthrough drift the funnel
        // exists to prevent).
        if let Some(builtin) = noeta_types::BuiltinTy::from_name_any(key_name) {
            use noeta_types::BuiltinTy::*;
            return match builtin {
                Str | Int | IntN { .. } => Some(true),
                // The float family is uniformly barred: NaN ≠ NaN and `-0.0 == 0.0` make float
                // keys a footgun, and `f64` is `float` under another name.
                Float | F32 | F64 => Some(false),
                // `number` is a UNION of scalars, so there is no single key form to hash or order
                // by — `Map<number, _>` would need one key representation spanning twelve types.
                // Barred like the float family, by a different route.
                Number => Some(false),
                // `bool` splits by role (post derive-soundness: bool is orderable, `false <
                // true`): a `Set<bool>` is fine — the runtime canonicalizes it like any orderable
                // element — but a map key needs a `MapKey` kind, which bool deliberately lacks
                // (two possible keys is a smell; use fields). The gates pass their role via
                // `for_set`.
                Bool => Some(for_set),
                // The remaining built-ins keep the lenient answer the old fallthrough gave them
                // — now stated rather than inherited. `bytes` keys are undecided (content
                // comparison exists; a `MapKey` form does not — an explicit deferral), and a
                // container/abstract-kind head in key position is a malformed type the arity
                // check (E0058) already reports on its own span.
                Bytes | Unit | Dyn | List | Set | Map | Option | Result | KindEnum | KindStruct
                | KindClass => None,
            };
        }
        if let Some(ext) = self.imported_extern(key_name) {
            return Some(ext.key_capable);
        }
        if let Some(layout) = self.packed_layout(&Type::Named(key_name.to_string(), Vec::new())) {
            return Some(layout_key_capable(&layout));
        }
        if self.symbols.records.contains_key(key_name) || self.symbols.enums.contains_key(key_name)
        {
            // A set additionally admits any **value kind** (derive-soundness follow-up F2): a
            // non-packed struct or an enum orders structurally (`set_order` — the same total
            // ordering `@derive(Comparable)` and `.sorted()` use), so `Set<P>`/`Set<Dir>` are
            // fine. A `class` stays out of both roles: a set stores a sorted snapshot, and a
            // reference could be mutated after insertion. Maps still need a `MapKey` form, which
            // only the packed/int/string/extern kinds above have.
            if for_set {
                return Some(!matches!(
                    self.symbols.type_kinds.get(key_name),
                    Some(noeta_types::TypeKind::Class)
                ));
            }
            return Some(false);
        }
        None
    }
}

/// Whether a built-in type name has a **key position** its first type argument occupies, and if so
/// the role that position plays in a diagnostic (`Map<K, _>` keys a map, `Set<T>` members a set)
/// plus whether the looser set rules apply. `None` for everything else.
///
/// Exhaustive over [`BuiltinTy`] on purpose: a new built-in container has to declare here whether
/// its arguments are keys, rather than silently inheriting "not keyed". Only the canonical
/// spellings are keyed — a bare `map`/`set` carries no arguments to gate.
fn keyed_container_role(name: &str) -> Option<(&'static str, bool)> {
    use noeta_types::{BuiltinTy, Spelling};
    let (builtin, spelling) = BuiltinTy::from_name(name)?;
    if spelling == Spelling::Bare {
        return None;
    }
    match builtin {
        BuiltinTy::Map => Some(("key a map", false)),
        BuiltinTy::Set => Some(("member a set", true)),
        // `List`/`Option`/`Result` arguments are ordinary element/payload positions — nothing is
        // hashed or ordered by them, so any type may sit there.
        BuiltinTy::List | BuiltinTy::Option | BuiltinTy::Result => None,
        // The scalars and the abstract kind-types take no arguments at all.
        BuiltinTy::Int
        | BuiltinTy::Float
        | BuiltinTy::F32
        | BuiltinTy::F64
        | BuiltinTy::IntN { .. }
        | BuiltinTy::Bool
        | BuiltinTy::Str
        | BuiltinTy::Bytes
        | BuiltinTy::Unit
        | BuiltinTy::Dyn
        | BuiltinTy::KindEnum
        | BuiltinTy::KindStruct
        | BuiltinTy::KindClass
        | BuiltinTy::Number => None,
    }
}
