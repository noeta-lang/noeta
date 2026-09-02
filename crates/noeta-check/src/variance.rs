//! **Declaration-site variance**: where a generic type puts its type parameters, and therefore
//! whether a `C<Sub>` may be read as a `C<Sup>`.
//!
//! Reading a value at a wider type argument is safe exactly when nothing can put a `Sup` where the
//! value's own type says `Sub`. That is a property of the *declaration*, not of the arguments, so it
//! is computed once per declared type and consulted by [`Type::subtype_with`] through
//! [`noeta_types::NominalRules::covariant_arg`] at every generic-argument position it descends into.
//!
//! One rule covers every wider argument a program can write. `C<Dog>` reads as a `C<dyn Speak>`, as
//! a `C<dyn>` and as a `C<Struct>` in exactly the same declarations, because the store that would
//! corrupt the original is the same store whichever of the three the reader named.
//!
//! Three occurrences make a parameter unsafe to widen, and each is a different mechanism:
//!
//! - A **`mut` field of a reference `class`**. A widened view of a class *is* the original — class
//!   values have reference identity — so a store through it is observed through the original. A
//!   value `struct` (and an `enum`) is exempt: a struct field-set rebinds the binding rather than
//!   writing through a shared object, so the widened view is a separate value and the store lands
//!   only there.
//! - A **method parameter** mentioning the parameter, in any kind. This one has nothing to do with
//!   aliasing: the body of `fn set(x: T)` is type-checked believing `x` is a `Dog`, so calling it
//!   through a widened receiver hands it a `Cat` and the body's `x.bark()` fails at run time. A
//!   field of function type mentioning the parameter among its *own* parameters is the same
//!   occurrence, reached through a field instead of a method table.
//! - The parameter reaching an **invariant argument of another generic type**, through a field or a
//!   return. Composition is what makes the value-semantic exemption above safe rather than merely
//!   plausible: a `struct` that holds a `class` shares that class with its copies, so
//!   `struct S<T> { c: C<T> }` is invariant exactly when `C` is.
//!
//! Everything else is a read position and stays covariant: an immutable field, a `mut` field of a
//! struct, a method's return type, and any depth of the value-semantic built-in containers
//! (`List`, `Set`, `Map`, `Option`, `Result`, tuples, unions), which copy on write.
//!
//! A generic type this program cannot see the declaration of — a native extension type — is
//! invariant, because nothing here can prove otherwise.

use std::collections::HashMap;

use noeta_ast::{FieldDecl, FnDecl, Program, Stmt, TypeRef};
use noeta_types::{Type, TypeKind};

use crate::{Checker, env::Receiver, stdlib};

/// The **checker-native generics'** variance, stated rather than derived: their declarations are in
/// the checker, not in the program, so the walk below has nothing to read for them.
///
/// Each carries one type argument, and each is decided the same way every declared type is — by
/// whether a value of the widened argument can reach a position that expects the narrow one.
/// `Iterator<T>`, `Future<T>` and `Receiver<T>` only ever hand a `T` *out* (`next`, `.await`,
/// `recv`), so reading one at a wider argument is safe. A `Sender<T>` takes a `T` *in* — `send(v: T)`
/// — which is the method-parameter occurrence, so it is not.
///
/// Everything else the program has no declaration for stays invariant by the same default a native
/// extension type gets: nothing here can prove otherwise.
fn native_variance(name: &str) -> Option<ArgVariance> {
    match name {
        stdlib::ITERATOR | stdlib::FUTURE | stdlib::RECEIVER => Some(vec![None]),
        stdlib::SENDER => Some(vec![Some(InvarianceCause::MethodParam("send".to_string()))]),
        _ => None,
    }
}

/// Where a type parameter occurs, as the source names it — the half of an [`InvarianceCause`] that
/// points at something the reader can go and look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Site {
    Field(String),
    Method(String),
}

impl std::fmt::Display for Site {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Site::Field(n) => write!(f, "the field `{n}`"),
            Site::Method(n) => write!(f, "the method `{n}`"),
        }
    }
}

/// Why a generic type is **invariant** in one of its type arguments: the occurrence of the
/// parameter that makes reading a widened view unsound, in the words the diagnostic prints.
///
/// Carried rather than recomputed at the report, because the walk that finds it has the declaration
/// in hand and the diagnostic does not — and "which occurrence" is the whole content of the
/// message. [`Display`] writes it as a verb phrase, so a caller composes it as
/// "`Box2` <cause>, so a `Box2<Dog>` cannot be read as a `Box2<dyn Speak>`".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InvarianceCause {
    /// A `mut` field of a reference `class`: a store through the widened alias is a store to the
    /// original.
    MutField(String),
    /// An instance method takes the parameter as an argument: the body is checked at the narrow
    /// type, and a widened receiver would hand it a value of another one.
    MethodParam(String),
    /// A field of function type takes the parameter among its own parameters — the method case
    /// reached through a field.
    FnParam(Site),
    /// The parameter reaches an argument of another generic type that is itself invariant in it.
    Through { site: Site, via: String },
    /// The parameter reaches an argument of a generic type this program has no declaration for.
    Opaque { site: Site, via: String },
}

impl std::fmt::Display for InvarianceCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InvarianceCause::MutField(n) => write!(f, "stores it in the `mut` field `{n}`"),
            InvarianceCause::MethodParam(n) => {
                write!(f, "takes it as a parameter of the method `{n}`")
            }
            InvarianceCause::FnParam(site) => {
                write!(f, "takes it as a parameter of the function in {site}")
            }
            InvarianceCause::Through { site, via } => {
                write!(f, "passes it through {site} to `{via}`, which stores it")
            }
            InvarianceCause::Opaque { site, via } => write!(
                f,
                "passes it through {site} to `{via}`, whose declaration is not in this program"
            ),
        }
    }
}

/// Per type argument: `None` where the type is covariant in it, the cause where it is not. The
/// vector is as long as the declaration's parameter list.
pub(crate) type ArgVariance = Vec<Option<InvarianceCause>>;

/// One generic declaration, reduced to what variance needs: its kind, its parameter spellings, and
/// every position a parameter can occur in.
struct DeclShape<'a> {
    kind: TypeKind,
    params: Vec<&'a str>,
    /// `(name, declared `mut`, declared type)` for each field — an enum variant's payload is a
    /// field here too, and is never `mut`.
    fields: Vec<(&'a str, bool, &'a TypeRef)>,
    methods: Vec<MethodShape<'a>>,
}

/// One **instance** method's signature, as variance reads it. An associated (receiverless) function
/// never appears: with no receiver there is no widened view to call it through, which is what makes
/// a generic type's own `fn new(v: T): C<T>` constructor harmless.
struct MethodShape<'a> {
    name: &'a str,
    /// The method's own type parameters, which **shadow** the type's where the spellings collide —
    /// a `fn map<T>(…)` on a `class Box<T>` mentions a different `T`, and reading it as the class's
    /// would invent an occurrence the source did not write.
    shadowed: Vec<&'a str>,
    params: Vec<&'a TypeRef>,
    ret: Option<&'a TypeRef>,
}

impl Checker {
    /// Compute [`crate::Symbols::arg_variance`] for every generic type the program declares.
    ///
    /// Runs at the end of `collect`, after the receiver classification it reads: whether a method
    /// binds a receiver decides whether its parameters are reachable through a widened view at all.
    ///
    /// A **fixpoint**, because the answer composes: a field of type `Other<T>` is a read position
    /// only if `Other` is covariant at that argument, and `Other` may in turn hold a `Self<T>`.
    /// Every type starts covariant and only ever loses it, so the iteration is monotone and
    /// terminates in at most one round per parameter in the program.
    pub(crate) fn compute_arg_variance(&mut self, program: &Program) {
        let mut shapes: HashMap<&str, DeclShape> = HashMap::new();
        for stmt in &program.stmts {
            match stmt {
                Stmt::Struct(d) => {
                    shapes.insert(
                        d.name.as_str(),
                        self.shape_of(
                            d.name.as_str(),
                            TypeKind::Struct,
                            &d.type_params,
                            &d.fields,
                            &d.methods,
                        ),
                    );
                }
                Stmt::Class(d) => {
                    shapes.insert(
                        d.name.as_str(),
                        self.shape_of(
                            d.name.as_str(),
                            TypeKind::Class,
                            &d.type_params,
                            &d.fields,
                            &d.methods,
                        ),
                    );
                }
                Stmt::Enum(d) => {
                    // A variant's payload is a field for this purpose: an immutable slot holding a
                    // value of the parameter's type, read out of a `match`.
                    let mut shape = self.shape_of(
                        d.name.as_str(),
                        TypeKind::Enum,
                        &d.type_params,
                        &[],
                        &d.methods,
                    );
                    for v in &d.variants {
                        for p in &v.fields {
                            if let Some(ty) = &p.ty {
                                shape.fields.push((p.name.as_str(), false, ty));
                            }
                        }
                    }
                    shapes.insert(d.name.as_str(), shape);
                }
                _ => {}
            }
        }
        // A **standalone** `impl Trait for T` contributes methods to `T` exactly as an in-body
        // `impl` block does; they are simply written elsewhere. Missing them would let a type
        // acquire a `T`-taking method after its variance was decided.
        for stmt in &program.stmts {
            let Stmt::Impl(decl) = stmt else { continue };
            let Some(shape) = shapes.get_mut(decl.target.as_str()) else {
                continue;
            };
            let owner = decl.target.as_str().to_string();
            for m in &decl.methods {
                if let Some(ms) = Self::method_shape(self, &owner, m) {
                    shape.methods.push(ms);
                }
            }
        }

        // Seeded from what the checker already knows, and **accumulated**, because a session entry
        // collects only its own statements: a type declared at an earlier prompt is not in `shapes`
        // here, and recomputing from scratch would forget it (and make a later entry's composition
        // through it read as an opaque, invariant head). Every type this program *does* declare is
        // reset to covariant first, so a redeclaration is recomputed rather than remembered.
        let mut table: HashMap<String, ArgVariance> = self.symbols.arg_variance.clone();
        table.extend(
            stdlib::NATIVE_TYPE_NAMES
                .iter()
                .filter_map(|n| native_variance(n).map(|v| ((*n).to_string(), v))),
        );
        table.extend(
            shapes
                .iter()
                .filter(|(_, s)| !s.params.is_empty())
                .map(|(n, s)| ((*n).to_string(), vec![None; s.params.len()])),
        );
        loop {
            let mut changed = false;
            for (name, shape) in &shapes {
                for (i, p) in shape.params.iter().enumerate() {
                    if table.get(*name).is_some_and(|v| v[i].is_some()) {
                        continue;
                    }
                    if let Some(cause) = scan(shape, p, &table)
                        && let Some(v) = table.get_mut(*name)
                    {
                        v[i] = Some(cause);
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        self.symbols.arg_variance = table;
    }

    /// Reduce one declaration to its [`DeclShape`].
    fn shape_of<'a>(
        &self,
        owner: &str,
        kind: TypeKind,
        type_params: &'a [noeta_ast::TypeParam],
        fields: &'a [FieldDecl],
        methods: &'a [FnDecl],
    ) -> DeclShape<'a> {
        DeclShape {
            kind,
            params: type_params.iter().map(|p| p.name.as_str()).collect(),
            fields: fields
                .iter()
                .filter_map(|f| f.ty.as_ref().map(|t| (f.name.as_str(), f.mut_field, t)))
                .collect(),
            methods: methods
                .iter()
                .filter_map(|m| Self::method_shape(self, owner, m))
                .collect(),
        }
    }

    /// One method's [`MethodShape`], or `None` for an associated (receiverless) function — read
    /// through the one receiver classification [`Checker::receiver_of`] owns, never re-derived.
    fn method_shape<'a>(&self, owner: &str, m: &'a FnDecl) -> Option<MethodShape<'a>> {
        if self.receiver_of(owner, m.name.as_str()) == Receiver::Static {
            return None;
        }
        Some(MethodShape {
            name: m.name.as_str(),
            shadowed: m.type_params.iter().map(|p| p.name.as_str()).collect(),
            params: m.params.iter().filter_map(|p| p.ty.as_ref()).collect(),
            ret: m.ret.as_ref(),
        })
    }

    /// Whether the generic type `name` may be **read at a wider argument** in position `index` —
    /// [`noeta_types::NominalRules::covariant_arg`]'s answer.
    ///
    /// A type with no entry (a native extension type, or one this program never declared) is
    /// invariant: the rule only ever admits a widening, so an unknown declaration must not.
    pub(crate) fn covariant_arg(&self, name: &str, index: usize) -> bool {
        self.symbols
            .arg_variance
            .get(name)
            .and_then(|v| v.get(index))
            .is_some_and(Option::is_none)
    }

    /// The occurrence that made `name` invariant at `index`, for the diagnostic.
    pub(crate) fn invariance_cause(&self, name: &str, index: usize) -> Option<&InvarianceCause> {
        self.symbols
            .arg_variance
            .get(name)
            .and_then(|v| v.get(index))
            .and_then(Option::as_ref)
    }

    /// The help line for a refused **widening of a generic instantiation** — the case where
    /// `C<Sub>` would have been readable as `C<Sup>` had `C` not written `T` into a position a
    /// widened view can reach.
    ///
    /// The wider argument is a trait object (`C<dyn Speak>`), the open top (`C<dyn>`) or an abstract
    /// kind (`C<Struct>`); all three are the same widening, refused for the same occurrence, so all
    /// three name it.
    ///
    /// `None` for every other mismatch, so an ordinary `E0007` is unchanged. Naming the occurrence
    /// is the point: the two types differ by one argument the reader can see is related, and
    /// without the cause the message reads as though the wider type simply does not work here.
    pub(crate) fn variance_refusal_help(&self, actual: &Type, expected: &Type) -> Option<String> {
        let (Type::Named(an, aa), Type::Named(bn, ba)) = (actual, expected) else {
            return None;
        };
        if an != bn || aa.len() != ba.len() {
            return None;
        }
        let (i, sup) = aa.iter().zip(ba).enumerate().find_map(|(i, (a, b))| {
            let widens = match b {
                // The open top takes any argument that is not already open: `dyn <: dyn` is
                // identity, and a gradual hole is no claim at all.
                Type::Dyn => !a.is_gradual() && !matches!(a, Type::Dyn | Type::Never),
                Type::DynTrait(tr) => {
                    matches!(a, Type::Named(sub, _) if self.implements_trait(sub, tr))
                }
                Type::Kind(k) => matches!(a, Type::Named(sub, _) if self.is_of_kind(sub, *k)),
                _ => false,
            };
            (widens && !self.covariant_arg(an, i)).then_some((i, b))
        })?;
        let cause = self.invariance_cause(an, i)?;
        let short = |t: &Type| t.to_string();
        Some(format!(
            "`{an}` {cause}, so a `{}` cannot be read as a `{}`. Construct the value where the \
             wider type is stated instead — a literal checked against `{}` instantiates its type \
             argument at `{}` directly",
            short(actual),
            short(expected),
            short(expected),
            short(sup),
        ))
    }
}

/// Every position of the parameter `p` in one declaration, stopping at the first that cannot be
/// read widened.
fn scan(
    shape: &DeclShape,
    p: &str,
    table: &HashMap<String, ArgVariance>,
) -> Option<InvarianceCause> {
    for (name, is_mut, ty) in &shape.fields {
        // A shared mutable field is a write position — but only where the value is shared. A
        // `struct`'s field-set rebinds the binding, so a store into the widened copy is not a store
        // into the original, and the field is read-only as far as the *original* is concerned.
        if *is_mut && shape.kind == TypeKind::Class && mentions(ty, p) {
            return Some(InvarianceCause::MutField((*name).to_string()));
        }
        if let Some(cause) = out_position(ty, p, table, &|| Site::Field((*name).to_string())) {
            return Some(cause);
        }
    }
    for m in &shape.methods {
        if m.shadowed.contains(&p) {
            continue;
        }
        for ty in &m.params {
            if mentions(ty, p) {
                return Some(InvarianceCause::MethodParam(m.name.to_string()));
            }
        }
        if let Some(ret) = m.ret
            && let Some(cause) = out_position(ret, p, table, &|| Site::Method(m.name.to_string()))
        {
            return Some(cause);
        }
    }
    None
}

/// Whether `p` occurs anywhere inside `ty` in a position that **cannot** be read at a wider type,
/// and which occurrence that is. `None` means every occurrence is a read position.
///
/// `site` is a thunk so the (rare) failure path pays for the string and the common path does not.
fn out_position(
    ty: &TypeRef,
    p: &str,
    table: &HashMap<String, ArgVariance>,
    site: &dyn Fn() -> Site,
) -> Option<InvarianceCause> {
    match ty {
        // The bare parameter, read out of a field or returned: the covariant case, and the whole
        // point of the rule.
        TypeRef::Named { name, args, .. } if name.as_str() == p && args.is_empty() => None,
        TypeRef::Named { name, args, .. } => {
            for (j, arg) in args.iter().enumerate() {
                if !mentions(arg, p) {
                    continue;
                }
                // The value-semantic built-in containers copy on write, so every depth of them is a
                // read position — which is why an annotated `List<dyn Trait>` accepts implementors
                // and a `List<Dog>` may be read as one.
                if Type::is_builtin_name(name.as_str()) {
                    if let Some(cause) = out_position(arg, p, table, site) {
                        return Some(cause);
                    }
                    continue;
                }
                match table.get(name.as_str()) {
                    Some(v) if v.get(j).is_some_and(Option::is_none) => {
                        if let Some(cause) = out_position(arg, p, table, site) {
                            return Some(cause);
                        }
                    }
                    Some(_) => {
                        return Some(InvarianceCause::Through {
                            site: site(),
                            via: name.to_string(),
                        });
                    }
                    None => {
                        return Some(InvarianceCause::Opaque {
                            site: site(),
                            via: name.to_string(),
                        });
                    }
                }
            }
            None
        }
        TypeRef::Optional { inner, .. } => out_position(inner, p, table, site),
        TypeRef::Union { members: items, .. }
        | TypeRef::Tuple {
            elements: items, ..
        } => items.iter().find_map(|t| out_position(t, p, table, site)),
        // A function's parameters are contravariant: a widened view would let a caller pass a value
        // of the wider type into a body checked at the narrower one.
        TypeRef::Fn { params, ret, .. } => {
            if params.iter().any(|t| mentions(t, p)) {
                return Some(InvarianceCause::FnParam(site()));
            }
            out_position(ret, p, table, site)
        }
        TypeRef::DynTrait { .. } | TypeRef::AssocProjection { .. } => None,
    }
}

/// Whether the type parameter spelled `p` occurs anywhere in `ty`.
fn mentions(ty: &TypeRef, p: &str) -> bool {
    match ty {
        TypeRef::Named { name, args, .. } => {
            name.as_str() == p || args.iter().any(|a| mentions(a, p))
        }
        TypeRef::Optional { inner, .. } => mentions(inner, p),
        TypeRef::Union { members: items, .. }
        | TypeRef::Tuple {
            elements: items, ..
        } => items.iter().any(|t| mentions(t, p)),
        TypeRef::Fn { params, ret, .. } => {
            params.iter().any(|t| mentions(t, p)) || mentions(ret, p)
        }
        TypeRef::DynTrait { .. } | TypeRef::AssocProjection { .. } => false,
    }
}
