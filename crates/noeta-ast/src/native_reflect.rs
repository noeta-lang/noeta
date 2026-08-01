//! **Native reflection** — what a [`ReflectionInfo`](crate::reflect::ReflectionInfo) answers about a
//! declaration that lives in the **extension registry** rather than in the program's AST.
//!
//! `reflect::build` walks a *program*, so a type or callable the program does not declare is absent
//! from the artifact however real it is to the rest of the language. The prelude enums, an
//! extension's attributes, its native enums/classes/structs and every native callable are as
//! constructible, matchable, namable and callable as any `.noe` declaration, so reflection has to
//! answer for them too — "a native class is indistinguishable from a `.noe` class" is the invariant,
//! and reflection is one of its consumers.
//!
//! **This is the one seam that answers, and it answers lazily.** The projections used to be run
//! *eagerly* over the whole registry and pushed into every compiled artifact
//! (`noeta_check::extend_reflection`), which cost 1.83M instructions on `noeta run` of a one-line
//! program — 33% of the whole process — before the program was looked at, and grew with every
//! declaration the stdlib gained. Nothing about a native declaration depends on the program, so
//! there is nothing to precompute: a lookup that misses the program's own tables resolves the one
//! name it was asked about, out of the `&'static` registry, and memoizes that one answer. A program
//! that never mentions `res.Handle` pays nothing for it.
//!
//! Order of resolution is the order the eager seeding pushed records in, and for the same reason: the
//! artifact's lookups were `find`s over a `Vec`, so the *first* record for a name won. Keeping the
//! order keeps every answer byte-identical to the eager path's.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::reflect::{ParamRecord, ParamSig, TypeInfo, TypeKind, TypeRepr, VariantInfo};

/// The reflectable shape of a **native or prelude** type by name, or `None` when no installed
/// declaration carries that name — the fallback [`ReflectionInfo::type_named`](crate::reflect::ReflectionInfo::type_named)
/// reaches on a miss.
///
/// Resolution order (first match wins, exactly as the eager seeding's push order made it): the
/// prelude types, an extension's data-only `@attribute` declarations, its fielded `@attribute`s, its
/// native enums, its native fielded types.
///
/// Memoized per name, including the misses — the registry is `&'static` and installed once, so an
/// answer can never change. A miss is *not* memoized while no registry is installed yet, so a lookup
/// made before seeding cannot poison the table.
pub fn native_type_info(name: &str) -> Option<&'static TypeInfo> {
    static MEMO: OnceLock<Mutex<HashMap<String, Option<&'static TypeInfo>>>> = OnceLock::new();
    memoized(&MEMO, name, resolve_type_info)
}

/// The declared signature of a **native** callable under the target spelling reflection keys it by
/// (`std.math.sqrt`, `std.id.Uuid.to_string`, `std.vec.Kernels.dot`), or `None` when the target names
/// no native callable — the fallback both
/// [`ReflectionInfo::params_for`](crate::reflect::ReflectionInfo::params_for) and
/// [`returns_for`](crate::reflect::ReflectionInfo::returns_for) reach on a miss. One record answers
/// both, exactly as one eagerly-seeded record did.
///
/// Memoized per target on the same terms as [`native_type_info`].
pub fn native_param_record(target: &str) -> Option<&'static ParamRecord> {
    static MEMO: OnceLock<Mutex<HashMap<String, Option<&'static ParamRecord>>>> = OnceLock::new();
    memoized(&MEMO, target, resolve_param_record)
}

/// The shared memo: resolve `key` at most once, then hand back the leaked `&'static` answer.
///
/// Leaking is deliberate and bounded: every value here is derived purely from the `&'static`
/// registry (or the prelude table), so the set of reachable answers is finite and each is live for
/// the rest of the process — which is exactly the lifetime the eagerly-seeded tables had, minus the
/// ones nobody asked for.
fn memoized<T: 'static>(
    memo: &'static OnceLock<Mutex<HashMap<String, Option<&'static T>>>>,
    key: &str,
    resolve: fn(&str) -> Option<T>,
) -> Option<&'static T> {
    let mut table = memo
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some(hit) = table.get(key) {
        return *hit;
    }
    let answer = resolve(key).map(|value| &*Box::leak(Box::new(value)));
    // Do not memoize a miss taken before the process installed its registry: the answer would be
    // wrong for the rest of the process (see [`native_type_info`]).
    if answer.is_some() || noeta_ext_abi::registry::default_registry().is_some() {
        table.insert(key.to_string(), answer);
    }
    answer
}

/// Whether `parts` joined by `.` spells `target` — an allocation-free comparison against a
/// dotted identity, so probing every module function for one target does not build every
/// module function's name.
fn joined_eq(parts: &[&str], target: &str) -> bool {
    let mut rest = target;
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            match rest.strip_prefix('.') {
                Some(tail) => rest = tail,
                None => return false,
            }
        }
        match rest.strip_prefix(*part) {
            Some(tail) => rest = tail,
            None => return false,
        }
    }
    rest.is_empty()
}

/// Resolve one type name against the prelude table and the installed registry — [`native_type_info`]'s
/// body, split out so the memo above stays generic over both lookups.
fn resolve_type_info(name: &str) -> Option<TypeInfo> {
    use noeta_ext_abi::NominalType;
    use noeta_ext_abi::registry as ext;
    if let Some(prelude) = crate::reflect::prelude_type_infos()
        .into_iter()
        .find(|t| t.name == name)
    {
        return Some(prelude);
    }
    if let Some(attr) = ext::ext_attributes().find(|a| a.is_qualified(name)) {
        return Some(attribute_type_info(attr));
    }
    if let Some(fielded) = ext::ext_fielded_attributes().find(|f| f.is_qualified(name)) {
        return Some(fielded_type_info(fielded));
    }
    let reg = ext::default_registry()?;
    if let Some(en) = reg.enums().find(|e| e.is_qualified(name)) {
        return Some(enum_type_info(en));
    }
    reg.fielded()
        .find(|f| f.is_qualified(name))
        .map(fielded_type_info)
}

/// Resolve one callable target against the installed registry — [`native_param_record`]'s body.
///
/// The families are probed in the eager seeding's push order: each unit's module functions, then its
/// native traits' methods, then every native type's methods, then the fielded types', then the enums'.
fn resolve_param_record(target: &str) -> Option<ParamRecord> {
    use noeta_ext_abi::NominalType;
    use noeta_ext_abi::registry as ext;
    let reg = ext::default_registry()?;
    // Every keyed spelling is `<owner>.<callable>`, so the owner is everything up to the last dot.
    let (owner, last) = target.rsplit_once('.')?;
    for unit in reg.extensions() {
        for module in unit.modules() {
            if !joined_eq(&[unit.root(), module.name], owner) {
                continue;
            }
            for table in [
                module.functions,
                module.ctx_functions,
                module.typed_functions,
            ] {
                if let Some(f) = table.iter().find(|f| f.name == last) {
                    return Some(ext_fn_record(target.to_string(), f));
                }
            }
        }
        for tr in unit.traits() {
            if !tr.is_qualified(owner) {
                continue;
            }
            if let Some(m) = tr.methods.iter().find(|m| m.sig.name == last) {
                return Some(ext_fn_record(target.to_string(), &m.sig));
            }
        }
    }
    for t in reg.extensions().iter().flat_map(|e| e.types()) {
        if !t.is_qualified(owner) {
            continue;
        }
        for table in [t.methods, t.ctx_methods] {
            if let Some(f) = table.iter().find(|f| f.name == last) {
                return Some(ext_fn_record(target.to_string(), f));
            }
        }
    }
    for t in reg.fielded() {
        if t.is_qualified(owner)
            && let Some(f) = t.methods.iter().find(|f| f.name == last)
        {
            return Some(ext_fn_record(target.to_string(), f));
        }
    }
    for t in reg.enums() {
        if t.is_qualified(owner)
            && let Some(f) = t.methods.iter().find(|f| f.name == last)
        {
            return Some(ext_fn_record(target.to_string(), f));
        }
    }
    None
}

/// One installed extension's **data-only `@attribute` declaration** as a reflection [`TypeInfo`]
/// (tier-extensions port) — the materialization shape `attributes_of` needs for an attribute that has
/// no AST declaration. The registry declaration is the single source (the old hardcoded
/// `builtin_attribute_shape` fallback is gone).
///
/// A native **fielded** `@attribute` (D2) — a real `ExtFielded` carrying `ExtTypeDirective::Attribute`
/// — is an attribute to every consumer, so its shape reaches the same lookup through
/// [`fielded_type_info`]. Without that, `attribute_shape` found no `TypeInfo` for the fielded
/// attribute and `attributes_of::<Route>()` materialized an empty instance.
fn attribute_type_info(attr: &noeta_ext_abi::registry::ExtAttribute) -> TypeInfo {
    use noeta_ext_abi::registry as ext;
    TypeInfo {
        // The **qualified** identity (`std.test.Skip`) — the manifest shape `attributes_of`
        // matches must key on the same FQN the loader rewrites applications to (D2b).
        name: attr.qualified(),
        kind: TypeKind::Struct,
        fields: attr.fields.iter().map(|f| f.name.to_string()).collect(),
        // The field's declared type as a reflection `TypeRepr` (struct-reflection arc), so
        // `field_specs_of` reports a data-only native attribute's field types precisely.
        field_types: attr
            .fields
            .iter()
            .map(|f| match f.ty {
                ext::AttrFieldType::Int => TypeRepr::Int,
                ext::AttrFieldType::Str => TypeRepr::Str,
                ext::AttrFieldType::Dyn => TypeRepr::Dyn,
            })
            .collect(),
        // Optional iff the field carries a literal default (`Skip.reason = ""`).
        field_optional: attr.fields.iter().map(|f| f.default.is_some()).collect(),
        // A data-only attribute's fields ARE its arguments, so every one is public: an `ExtAttribute`
        // field carries no visibility channel, and the E0009 application-site check already requires
        // each mandatory one to be supplied by name.
        field_public: attr.fields.iter().map(|_| true).collect(),
        field_defaults: attr
            .fields
            .iter()
            .map(|f| {
                f.default.map(|d| match d {
                    ext::AttrFieldDefault::Str(s) => crate::AttrValue::Str(s.to_string()),
                    ext::AttrFieldDefault::Int(n) => crate::AttrValue::Int(n),
                })
            })
            .collect(),
        variants: Vec::new(),
    }
}

/// One installed extension's declared **enum** as a reflection [`TypeInfo`] — the native twin of
/// [`attribute_type_info`], and of `crate::reflect::prelude_type_infos`.
///
/// A native enum reaches a program under its qualified identity (`std.http.Framing`), which is what
/// `type_of` stamps on one of its values and therefore the key a consumer probes with; so that is
/// what this keys on, exactly as the attribute projection keys on the attribute's FQN. Payload slot
/// names are synthesized positionally (`_0`, `_1`, …) — the same convention the compiler's
/// `ext_enum_type_info` and a prelude variant use, and for the same reason: a native payload is
/// positional, so only the slot *count* and the declared types are load-bearing. A **backed**
/// native enum's per-variant constant rides through as the variant's `backing`, so `variants_of`
/// reports the wire values a schema derived from it must emit.
fn enum_type_info(en: &noeta_ext_abi::registry::ExtEnum) -> TypeInfo {
    use noeta_ext_abi::NominalType;
    use noeta_ext_abi::registry as ext;
    TypeInfo {
        name: en.qualified(),
        kind: TypeKind::Enum,
        fields: Vec::new(),
        field_types: Vec::new(),
        field_optional: Vec::new(),
        field_public: Vec::new(),
        field_defaults: Vec::new(),
        variants: en
            .variants
            .iter()
            .map(|v| VariantInfo {
                name: v.name.to_string(),
                fields: (0..v.fields.len()).map(|i| format!("_{i}")).collect(),
                field_types: v.fields.iter().map(sig_type_to_repr).collect(),
                backing: match v.value {
                    ext::VariantValue::None => None,
                    ext::VariantValue::Str(s) => Some(crate::AttrValue::Str(s.to_string())),
                    ext::VariantValue::Int(n) => Some(crate::AttrValue::Int(n)),
                },
            })
            .collect(),
    }
}

/// One native fielded type as a reflection [`TypeInfo`] — the single projection both
/// [`extension_attribute_types`]'s fielded arm and [`extension_fielded_types`] read.
///
/// The kind comes from the declaration's own [`FieldedKind`](noeta_ext_abi::FieldedKind), so a native
/// class reflects as `Class` and a native value struct as `Struct` — the same discriminant the
/// compiler's constructible-type record and the checker's `type_kinds` take, so a consumer branching
/// on kind sees what the rest of the language sees. (A fielded `@attribute` is `Struct`-kind by
/// assembly, so the attribute arm's shape is unchanged by sharing this.)
///
/// Field types come from the same `SigType` signature vocabulary the checker seeds into
/// `symbols.records`, projected through [`sig_type_to_repr`]. An `ExtField` carries no literal
/// default, so every field is mandatory (`optional: false`, `default: None`) — for an attribute that
/// is what makes the E0009 construction check require each one at the application site, and for a
/// plain native type it is what makes a `construct` that omits one a refusal.
fn fielded_type_info(f: &noeta_ext_abi::registry::ExtFielded) -> TypeInfo {
    use noeta_ext_abi::NominalType;
    TypeInfo {
        name: f.qualified(),
        kind: match f.kind {
            noeta_ext_abi::FieldedKind::Class => TypeKind::Class,
            noeta_ext_abi::FieldedKind::Struct => TypeKind::Struct,
        },
        fields: f
            .fields
            .iter()
            .map(|field| field.name.to_string())
            .collect(),
        field_types: f
            .fields
            .iter()
            .map(|field| sig_type_to_repr(&field.ty))
            .collect(),
        field_optional: f.fields.iter().map(|_| false).collect(),
        // Visibility straight off the declaration's own [`ExtField::is_public`] — the identical read
        // `seed_ext_fielded` performs to populate `symbols.private_fields`, and deliberately NOT
        // kind-adjusted the way a `.noe` type's is: a native fielded type states each field's
        // visibility explicitly (a native *struct*'s non-`pub` field is private too, per that
        // seeder), so the reflective construction door and the E0035 gate read one statement.
        field_public: f.fields.iter().map(|field| field.is_public).collect(),
        field_defaults: f.fields.iter().map(|_| None).collect(),
        variants: Vec::new(),
    }
}

/// Project a registry [`SigType`](noeta_ext_abi::registry::SigType) onto its reflection
/// [`TypeRepr`](TypeRepr) — the type-level counterpart of the checker's
/// [`sig_to_typeref`](crate::stdlib::sig_to_typeref)/[`sig_to_type`](crate::stdlib::sig_to_type),
/// so a native fielded type's field types reflect through `field_specs_of` the same way a `.noe`
/// struct's do. A polymorphic/variable position has no declaration-site type and becomes `Dyn`; a
/// trailing-optional wrapper is an arity marker and unwraps to its inner type.
///
/// A **nominal** resolves through the registry to the identity and kind a *value* of that type
/// carries ([`nominal_to_repr`]), which is a correction this projection needed the moment
/// `extension_param_records` started reporting native signatures: a registry signature spells a
/// nominal by its **short** name (`Named("Uuid")`), while `type_of` on one of its values reports the
/// qualified identity (`Type.Named(std.id.Uuid, [])`). So `returns_of("std.id.uuid")` said
/// `Type.Named(Uuid, [])` about a value that says `Type.Named(std.id.Uuid, [])` — one type, two
/// names, from the two queries the docs promise share a decoder, and a framework matching a declared
/// return against a runtime tag would have missed on every native type.
fn sig_type_to_repr(sig: &noeta_ext_abi::registry::SigType) -> TypeRepr {
    use TypeRepr;
    use noeta_ext_abi::registry::SigType;
    let boxed = |s: &SigType| Box::new(sig_type_to_repr(s));
    match sig {
        SigType::Int => TypeRepr::Int,
        SigType::Float => TypeRepr::Float,
        SigType::F32 => TypeRepr::F32,
        SigType::Bool => TypeRepr::Bool,
        SigType::String => TypeRepr::Str,
        SigType::Bytes => TypeRepr::Bytes,
        SigType::Unit => TypeRepr::Unit,
        SigType::Dyn => TypeRepr::Dyn,
        SigType::Never => TypeRepr::Never,
        SigType::List(t) => TypeRepr::List(boxed(t)),
        SigType::Option(t) => TypeRepr::Option(boxed(t)),
        SigType::Map(k, v) => TypeRepr::Map(boxed(k), boxed(v)),
        SigType::Result(ok, err) => TypeRepr::Result(boxed(ok), boxed(err)),
        SigType::Future(t) => TypeRepr::Named("Future".to_string(), vec![sig_type_to_repr(t)]),
        SigType::Named(n) => nominal_to_repr(n, Vec::new()),
        SigType::Generic(n, args) => {
            nominal_to_repr(n, args.iter().map(sig_type_to_repr).collect())
        }
        SigType::Union(members) => TypeRepr::Union(members.iter().map(sig_type_to_repr).collect()),
        SigType::Fn(params, ret) => {
            TypeRepr::Fn(params.iter().map(sig_type_to_repr).collect(), boxed(ret))
        }
        // A trailing-optional parameter is an arity marker, not a value type (as `sig_to_typeref`
        // treats it) — reflect the inner type.
        SigType::Optional(inner) => sig_type_to_repr(inner),
        // A signature-level type variable has no declaration-site type — a permissive hole.
        SigType::Var(_) | SigType::BoundedVar(_, _) => TypeRepr::Dyn,
        // A trait associated-type projection (`Self::Wide`, slice 1b) is resolved per-implementor by
        // the checker, not at the declaration site — a permissive hole in a reflected signature.
        SigType::Assoc(_) => TypeRepr::Dyn,
        // `Self` is likewise receiver-relative: a reflected signature has no receiver to resolve it
        // against, so it reflects as the same permissive hole rather than a fabricated nominal type.
        SigType::SelfTy => TypeRepr::Dyn,
        // "Any number" has no single reflected type — enumerating twelve members here would say
        // less than the hole does, since reflection consumers read a shape, not a constraint.
        SigType::Numeric => TypeRepr::Dyn,
    }
}

/// One nominal name out of a registry signature as a reflection [`TypeRepr`] under the **identity**
/// the rest of the language knows that type by — its qualified `namespace.name`, resolved through the
/// installed registry.
///
/// A registry signature spells a nominal by the short name its own extension knows it under
/// (`Named("Uuid")`, `Named("Framing")`), but identity is the qualified name: it is what `type_of`
/// stamps on a value, what `field_specs_of` / `variants_of` are keyed on, and what a `.noe`
/// annotation of the same type reflects as once the loader has qualified it. Without this resolution
/// `returns_of("std.id.uuid")` said `Type.Named(Uuid, [])` about a value that says
/// `Type.Named(std.id.Uuid, [])` — one type under two names, from the two queries the reflection docs
/// promise share a decoder, so a framework matching a declared return against a runtime tag missed on
/// every native type.
///
/// Kind-**agnostic** [`TypeRepr::Named`], deliberately, and for the same reason: that is exactly what
/// a `.noe` declaration of this type reflects as (`fn f(): Framing` → `Type.Named(std.http.Framing,
/// [])`, the documented spelling of a declared nominal annotation), so one type in a declared position
/// reads the same however it was declared. Classifying the native side into `Enum`/`Struct`/`Class`
/// would report the *value* channel's spelling in the *declaration* channel and make a consumer that
/// branches on `Type.Named(n, _)` miss precisely the native declarations.
///
/// A name the registry does not resolve keeps its bare spelling (the synthesized `Future` wrapper, a
/// third-party name registered elsewhere): inventing a namespace for it would fabricate an identity,
/// which is the failure this resolution exists to prevent.
fn nominal_to_repr(
    name: &str,
    args: Vec<TypeRepr>,
) -> TypeRepr {
    use TypeRepr;
    use noeta_ext_abi::NominalType;
    use noeta_ext_abi::registry as ext;
    let qualified = ext::default_registry().and_then(|reg| {
        reg.resolve_enum(name)
            .map(|t| t.qualified())
            .or_else(|| reg.resolve_fielded(name).map(|t| t.qualified()))
            .or_else(|| reg.resolve_type(name).map(|t| t.qualified()))
            .or_else(|| reg.resolve_trait(name).map(|t| t.qualified()))
    });
    TypeRepr::Named(qualified.unwrap_or_else(|| name.to_string()), args)
}

/// One native signature as a reflection [`ParamRecord`](ParamRecord) under
/// `target` — the single projection every callable kind in [`extension_param_records`] goes through,
/// so a module function and a method cannot come to describe their parameters differently.
///
/// A parameter is **optional** exactly when the declaration makes it so: a trailing
/// [`SigType::Optional`](noeta_ext_abi::registry::SigType::Optional) is the registry's arity marker
/// (a call may leave it unsupplied), which is precisely what `ParamSig::optional` reports for a
/// `.noe` parameter carrying a default. Its *type* is the wrapped inner type, not an `Option<…>` —
/// `sig_type_to_repr` already unwraps it, and reporting the marker as the value type would describe
/// a parameter the callee never sees.
fn ext_fn_record(
    target: String,
    f: &noeta_ext_abi::registry::ExtFn,
) -> ParamRecord {
    use noeta_ext_abi::registry as ext;
    let params = f
        .params
        .iter()
        .enumerate()
        .map(|(i, ty)| ParamSig {
            // A declared name where there is one (measured: every std signature that takes a
            // parameter names it); the positional slot name a native payload already reflects under
            // where there is not, so a blank name never reaches a consumer keying on it.
            name: f
                .param_names
                .get(i)
                .map(|n| (*n).to_string())
                .unwrap_or_else(|| format!("_{i}")),
            ty: sig_type_to_repr(ty),
            optional: matches!(ty, ext::SigType::Optional(_)),
        })
        .collect();
    ParamRecord {
        target,
        params,
        ret: ret_ty_to_repr(&f.ret, f.params),
    }
}

/// Project a registry [`RetTy`](noeta_ext_abi::registry::RetTy) onto its reflection
/// [`TypeRepr`](TypeRepr) — the return-type half of [`sig_type_to_repr`], and
/// deliberately more precise than the checker's `ret_to_typeref`, which flattens every polymorphic
/// form to `dyn` because it only needs a *declaration-site* annotation for the user-trait checkers.
/// `returns_of` reports what the signature says, so each form resolves as far as the declaration
/// does:
///
/// * `SameAsArg(n)` **is** parameter `n`'s declared type (`vec.add(v, w): typeof v`), so it reports
///   that type — `dyn` only when the parameter itself is a hole, or when the index names no
///   parameter (a declaration bug the registry's own conformance test catches).
/// * `NumericPreserving` means `int` when every argument is concretely `int` and `float` otherwise —
///   a union of exactly those two, which is what the surface renderer already prints for it.
/// * `TypeArg(wrap)` is named by the call site's turbofish, which a signature reflection has nothing
///   to resolve against — so the declared *wrapper* is reported around a `dyn` hole, the same
///   permissive hole a `SigType::Var` takes. The `Result` wrap's error type is declared, not
///   call-site, so it stays precise.
fn ret_ty_to_repr(
    ret: &noeta_ext_abi::registry::RetTy,
    params: &[noeta_ext_abi::registry::SigType],
) -> TypeRepr {
    use TypeRepr;
    use noeta_ext_abi::registry as ext;
    match ret {
        ext::RetTy::Concrete(s) => sig_type_to_repr(s),
        ext::RetTy::SameAsArg(n) => params
            .get(*n)
            .map(sig_type_to_repr)
            .unwrap_or(TypeRepr::Dyn),
        ext::RetTy::NumericPreserving => TypeRepr::Union(vec![TypeRepr::Int, TypeRepr::Float]),
        ext::RetTy::TypeArg(ext::TypeArgWrap::Plain) => TypeRepr::Dyn,
        ext::RetTy::TypeArg(ext::TypeArgWrap::Option) => TypeRepr::Option(Box::new(TypeRepr::Dyn)),
        ext::RetTy::TypeArg(ext::TypeArgWrap::Result(e)) => {
            TypeRepr::Result(Box::new(TypeRepr::Dyn), Box::new(sig_type_to_repr(e)))
        }
    }
}
