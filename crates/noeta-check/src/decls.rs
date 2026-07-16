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
            if p.default.is_some() {
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
        let tps: HashSet<String> = self.type_params.keys().cloned().collect();
        for p in params {
            let Some(default) = &p.default else { continue };
            let actual = self.synth(default, env);
            // Skip the type check when the parameter has no annotation (already an `E0022`) or its
            // type is generic/`dyn` (erases to `dyn`, which accepts any default).
            if p.ty.is_some() {
                let expected = erase_type_params(param_type(p, &self.extern_types), &tps);
                if !self.arg_assignable(&actual, &expected) {
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
    }

    /// Validate each field's default value (`x: T = expr`), object-model slice 5. A default is
    /// checked in the type's **definition scope** — the `env` here carries globals only (fields are
    /// not yet bound), so a default that references `self` or a sibling field is an `E0007` unknown
    /// name, matching its globals-only runtime scope. Its inferred type must be assignable to the
    /// field's declared type (`E0007` mismatch). Unlike parameter defaults there is **no
    /// trailing-only rule**: literal fields are named, so a default makes its field optional
    /// regardless of position. Call before binding fields into `env`.
    pub(crate) fn validate_field_defaults(&mut self, fields: &[FieldDecl], env: &mut Env) {
        let tps: HashSet<String> = self.type_params.keys().cloned().collect();
        for f in fields {
            let Some(default) = &f.default else { continue };
            let actual = self.synth(default, env);
            // Skip the type check when the field has no annotation (every field requires one, so an
            // un-annotated field is already reported) or its type erases to `dyn` (accepts any).
            if f.ty.is_some() {
                let expected = erase_type_params(field_type(&f.ty, &self.extern_types), &tps);
                if !self.arg_assignable(&actual, &expected) {
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
        let fields: Vec<(String, Type)> =
            vec![("self".to_string(), self_type(&r.name, &r.type_params))];
        for f in &r.fields {
            self.check_type_opt(&f.ty);
            self.check_attrs(&f.attrs, TargetKind::Field);
        }
        self.validate_field_defaults(&r.fields, env);
        self.check_derives(&r.name, &r.derives);
        let standalone = self.standalone_for(&r.name);
        // A struct carries in-body `impl Trait { }` blocks and inherent methods (the unified body),
        // checked exactly as a class's — coherence over its impls, then each method body.
        self.check_coherence(&r.derives, &r.impls, &standalone);
        self.check_attrs(&r.attrs, TargetKind::Struct);
        // Inside the type's own body, its (always-public) fields are accessible; the marker is
        // uniform with classes (a struct simply has no private fields to gate).
        let saved_type = self.current_type.replace(r.name.clone());
        for block in &r.impls {
            self.check_impl(block);
        }
        for method in &r.methods {
            self.check_fn(method, env, &fields, TargetKind::Method);
        }
        self.current_type = saved_type;
        self.type_params = saved;
    }

    /// The `(trait, span)` occurrences of every standalone `impl Trait for <name> {}`, cloned so a
    /// `&mut self` coherence check can borrow them without conflicting with `self.standalone_impls`.
    pub(crate) fn standalone_for(&self, name: &str) -> Vec<(String, Span)> {
        self.standalone_impls.get(name).cloned().unwrap_or_default()
    }

    pub(crate) fn check_class(&mut self, c: &ClassDecl, env: &mut Env) {
        let saved = self.enter_type_params(&c.type_params);
        // Only `self` is bound in a method body (prelude-redesign EX.1 — member access is
        // explicit): `self.field` types through `synth_member`; a bare field name is an unknown
        // name with a targeted hint (see the `Expr::Ident` fallback in `synth`).
        let fields: Vec<(String, Type)> =
            vec![("self".to_string(), self_type(&c.name, &c.type_params))];
        for f in &c.fields {
            self.check_type_opt(&f.ty);
            self.check_attrs(&f.attrs, TargetKind::Field);
        }
        self.validate_field_defaults(&c.fields, env);
        self.check_derives(&c.name, &c.derives);
        let standalone = self.standalone_for(&c.name);
        self.check_coherence(&c.derives, &c.impls, &standalone);
        self.check_attrs(&c.attrs, TargetKind::Class);
        // Inside the class's own methods/destructor its private fields are accessible — on `self`
        // and on any same-type value (the type-scoped privacy rule, object-model slice 2d).
        let saved_type = self.current_type.replace(c.name.clone());
        for block in &c.impls {
            self.check_impl(block);
        }
        for method in &c.methods {
            self.check_fn(method, env, &fields, TargetKind::Method);
        }
        if let Some(destructor) = &c.destructor {
            env.push(HashMap::new());
            for (name, ty) in &fields {
                bind(env, name, ty.clone());
            }
            for stmt in destructor {
                self.check_stmt(stmt, env);
            }
            env.pop();
        }
        self.current_type = saved_type;
        self.type_params = saved;
    }

    pub(crate) fn check_enum(&mut self, e: &EnumDecl, env: &mut Env) {
        let saved = self.enter_type_params(&e.type_params);
        self.check_type_opt(&e.backing);
        for variant in &e.variants {
            for field in &variant.fields {
                self.check_type_opt(&field.ty);
            }
            self.check_attrs(&variant.attrs, TargetKind::Variant);
        }
        self.check_derives(&e.name, &e.derives);
        let standalone = self.standalone_for(&e.name);
        // An enum carries in-body `impl Trait { }` blocks and inherent methods (the unified body,
        // object-model slice 3), checked exactly as a class's — coherence over its impls, then each
        // method body.
        self.check_coherence(&e.derives, &e.impls, &standalone);
        self.check_attrs(&e.attrs, TargetKind::Enum);
        // Inside an enum's own methods, `self` is the whole enum value (the variants differ, so —
        // unlike a struct/class — there is no implicit per-field scope; a method `match`es on
        // `self`). Bind `self` to the enum type so that `match self` is exhaustiveness-checked, and
        // set `current_type` for the same type-scoped resolution a class uses.
        let self_ty = Type::Named(e.name.clone(), Vec::new());
        let saved_type = self.current_type.replace(e.name.clone());
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
        self.current_type = saved_type;
        self.type_params = saved;
    }

    // ----- unknown-type resolution (E0013) -----

    /// Install `params` as the in-scope generic type parameters and return the previous set (to
    /// restore once the declaration is checked). Generic parameters are erased at runtime but are
    /// legal referents for annotations within their declaration. Each parameter's trait bounds are
    /// validated here (an unknown trait in a bound is `E0014`).
    pub(crate) fn enter_type_params(
        &mut self,
        params: &[TypeParam],
    ) -> HashMap<String, Vec<String>> {
        self.check_type_param_bounds(params);
        std::mem::replace(
            &mut self.type_params,
            params
                .iter()
                .map(|p| (p.name.clone(), p.bounds.clone()))
                .collect(),
        )
    }

    /// Validate each type parameter's trait bounds: a bound must name a built-in trait, else
    /// `E0014 UnknownTrait` (reusing the `impl`/`@derive` name-validation path). The bound names
    /// are what S4.2 enforces at instantiation; here we only check they refer to real traits.
    pub(crate) fn check_type_param_bounds(&mut self, params: &[TypeParam]) {
        for p in params {
            for bound in &p.bounds {
                if BuiltinTrait::from_name(bound).is_none() {
                    self.error(
                        DiagnosticCode::UnknownTrait,
                        p.span,
                        format!(
                            "unknown trait `{bound}` in bound on type parameter `{}`",
                            p.name
                        ),
                    )
                    .help(
                        "a bound must name a built-in trait, e.g. `Comparable`, `Equatable`, \
                             or `Display`",
                    );
                }
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
            TypeRef::Named { name, args, span } => {
                if !Type::is_builtin_name(name)
                    && !PRELUDE_TYPES.contains(&name.as_str())
                    && !self.type_params.contains_key(name)
                    && !self.types.contains(name)
                    // A native extern type is a valid annotation only when `use`-imported into this
                    // file (`use std.id.Uuid` → `extern_types["Uuid"]`), like a user type — it is no
                    // longer globally in scope by bare name.
                    && !self.extern_types.contains_key(name)
                {
                    self.error(
                        DiagnosticCode::UnknownType,
                        *span,
                        format!("unknown type `{name}`"),
                    )
                    .help(
                        "name a declared type, one imported with `use` (native types too, e.g. \
                             `use std.id.Uuid`), a generic parameter, or a built-in",
                    );
                }
                // Key-capability gate (extern-types X4): a `Map<K, _>` key / `Set<T>` element
                // formed from an extern type requires it key-capable — a mutable handle's hash
                // or order could go stale under a key, so `Map<FileHandle, _>` is a type error.
                let key_position = match name.as_str() {
                    "Map" => args.first(),
                    "Set" => args.first(),
                    _ => None,
                };
                if let Some(TypeRef::Named {
                    name: key_name,
                    span: key_span,
                    ..
                }) = key_position
                    && self.named_key_capable(key_name, name == "Set") == Some(false)
                {
                    let role = if name == "Map" {
                        "key a map"
                    } else {
                        "member a set"
                    };
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
                for arg in args {
                    self.check_type_ref(arg);
                }
            }
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
                PackedKind::Int | PackedKind::Bool => true,
                PackedKind::Float | PackedKind::F32 => false,
                PackedKind::Struct(inner) => layout_key_capable(inner),
            })
        }
        if matches!(
            key_name,
            "string" | "int" | "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64"
        ) {
            return Some(true);
        }
        if matches!(key_name, "float" | "f32") {
            return Some(false);
        }
        // `bool` splits by role (post derive-soundness: bool is orderable, `false < true`):
        // a `Set<bool>` is fine — the runtime canonicalizes it like any orderable element — but
        // a map key needs a `MapKey` kind, which bool deliberately lacks (two possible keys is a
        // smell; use fields). The gates pass their role via `for_set`.
        if key_name == "bool" {
            return Some(for_set);
        }
        if let Some(ext) = self.imported_extern(key_name) {
            return Some(ext.key_capable);
        }
        if let Some(layout) = self.packed_layout(&Type::Named(key_name.to_string(), Vec::new())) {
            return Some(layout_key_capable(&layout));
        }
        if self.records.contains_key(key_name) || self.enums.contains_key(key_name) {
            // A set additionally admits any **value kind** (derive-soundness follow-up F2): a
            // non-packed struct or an enum orders structurally (`set_order` — the same total
            // ordering `@derive(Comparable)` and `.sorted()` use), so `Set<P>`/`Set<Dir>` are
            // fine. A `class` stays out of both roles: a set stores a sorted snapshot, and a
            // reference could be mutated after insertion. Maps still need a `MapKey` form, which
            // only the packed/int/string/extern kinds above have.
            if for_set {
                return Some(!matches!(
                    self.type_kinds.get(key_name),
                    Some(noeta_types::TypeKind::Class)
                ));
            }
            return Some(false);
        }
        None
    }
}
