//! The API-reference corpus (docs-browser arc, Arc 2): the stdlib and native-package surface read
//! straight from the intrinsic registry, so signatures are DRY against the real source of truth and
//! third-party native packages appear through the same path.
//!
//! Organized **by module** (`std.math`, `std.http.client`, …) — a first-class registry concept — so
//! the crate boundary is invisible: a package spanning several modules contributes several
//! module-pages, and a module assembled from several crates still surfaces as one. Signatures come
//! from [`ExtFn::render`]; per-function prose comes from each module's opt-in [`ExtModule::docs`]
//! table (undocumented functions render signature-only, like docs.rs). Workspace-independent — the
//! registry is process-global — so, like the language guide, the API reference browses with no file
//! open.

use noeta_stdlib::NominalType;
use noeta_stdlib::registry;

/// The extension unit type every enumeration here walks — the registry's own element type.
type Ext = &'static (dyn noeta_stdlib::Extension + Sync);

/// One documented function of a module: its name, its rendered signature, and its prose (empty when
/// undocumented).
#[derive(Debug, Clone)]
pub struct ApiFn {
    pub name: String,
    /// The whole signature, `fn sqrt(x: float): float` (from [`ExtFn::render`]).
    pub signature: String,
    /// The markdown prose from the module's [`ExtModule::docs`] table, or empty.
    pub doc: String,
}

/// One module of the API reference: its qualified name (`std.math`) and its functions, sorted.
#[derive(Debug, Clone)]
pub struct ApiModule {
    pub qualified: String,
    pub functions: Vec<ApiFn>,
}

/// One extern **type** of the API reference: its qualified name (`std.id.Uuid`) and its methods.
#[derive(Debug, Clone)]
pub struct ApiType {
    pub qualified: String,
    pub methods: Vec<ApiFn>,
}

/// Every module the registry knows, qualified and sorted (`std.crypto`, `std.http.client`,
/// `std.math`, …), each with its functions (plain + higher-order) sorted by name. Reads the
/// process-global registry via the stdlib facade, which lazily seeds the built-in `std` units.
pub fn modules() -> Vec<ApiModule> {
    modules_in(registry::extensions(), &|_| true)
}

/// The modules of just the extensions whose [`root`](noeta_stdlib::Extension::root) is `root` —
/// an explicit single-namespace filter (`noeta doc --api --root <ns>`).
pub fn modules_of(root: &str) -> Vec<ApiModule> {
    modules_in(registry::extensions(), &|ext| ext.root() == root)
}

/// The modules of every registered extension EXCEPT the named units — the publish docs path
/// (`noeta doc --api --non-builtin`): a composed toolchain's registry holds exactly the builtin
/// units plus the package's own extension(s), so excluding the builtins by unit name documents the
/// package's real surface. Filtering by [`Extension::name`](noeta_stdlib::Extension::name), never by
/// root, honors an extension whose `root()` deliberately diverges from its package segment
/// (`para/p2p` rooting at `para`) and a package registering several units — the cases the old
/// `root == package segment` guess silently documented as `{"modules": []}`.
pub fn modules_excluding(exclude_units: &[&str]) -> Vec<ApiModule> {
    modules_in(registry::extensions(), &|ext| {
        !exclude_units.contains(&ext.name())
    })
}

fn modules_in(exts: &[Ext], keep: &dyn Fn(Ext) -> bool) -> Vec<ApiModule> {
    let mut out: Vec<ApiModule> = Vec::new();
    for ext in exts {
        if !keep(*ext) {
            continue;
        }
        for m in ext.modules() {
            let qualified = format!("{}.{}", ext.root(), m.name);
            let mut functions: Vec<ApiFn> = m
                .functions
                .iter()
                .chain(m.ctx_functions.iter())
                .map(|f| ApiFn {
                    name: f.name.to_string(),
                    signature: f.render(),
                    doc: doc_of(m, f.name),
                })
                // Call-site-typed functions are part of the module's surface, for the same reason
                // `typed_methods` are part of a type's (below): without them `json.parse::<T>` /
                // `json.try_parse::<T>` are invisible to the docs browser, `noeta doc --api`, and the
                // MCP docs tools — which is most of what makes the turbofish discoverable at all.
                // They are listed under their **turbofish spelling** (`parse::<T>`), which is both
                // how the surface is written and what keeps the entry distinct from a plain function
                // of the same name: the two tables deliberately allow a shared name (`json.parse` is
                // a dynamic `parse(text): dyn` AND a typed `parse::<T>: T`), so listing both under
                // the bare name would collide two different doors onto one page.
                .chain(m.typed_functions.iter().map(|f| ApiFn {
                    name: turbofish_name(f.name),
                    signature: f.render(),
                    doc: doc_of(m, f.name),
                }))
                .collect();
            functions.sort_by(|a, b| a.name.cmp(&b.name));
            out.push(ApiModule {
                qualified,
                functions,
            });
        }
    }
    out.sort_by(|a, b| a.qualified.cmp(&b.qualified));
    out
}

/// The module with the given qualified name (`std.math`), if the registry has it.
pub fn module(qualified: &str) -> Option<ApiModule> {
    modules().into_iter().find(|m| m.qualified == qualified)
}

/// A single function `qualified::name` (e.g. `std.math` / `sqrt`), if present.
pub fn function(qualified: &str, name: &str) -> Option<ApiFn> {
    module(qualified)?
        .functions
        .into_iter()
        .find(|f| f.name == name)
}

/// Every extern type the registry knows, qualified and sorted (`std.crypto.Hasher`, `std.id.Uuid`,
/// …), each with its methods (plain + higher-order) sorted by name.
pub fn types() -> Vec<ApiType> {
    types_in(registry::extensions(), &|_| true)
}

/// The extern types of just the extensions whose `root` is `root` — the type analogue of
/// [`modules_of`].
pub fn types_of(root: &str) -> Vec<ApiType> {
    types_in(registry::extensions(), &|ext| ext.root() == root)
}

/// The extern types of every registered extension EXCEPT the named units — the type analogue of
/// [`modules_excluding`].
pub fn types_excluding(exclude_units: &[&str]) -> Vec<ApiType> {
    types_in(registry::extensions(), &|ext| {
        !exclude_units.contains(&ext.name())
    })
}

fn types_in(exts: &[Ext], keep: &dyn Fn(Ext) -> bool) -> Vec<ApiType> {
    let mut out: Vec<ApiType> = Vec::new();
    for ext in exts {
        if !keep(*ext) {
            continue;
        }
        for t in ext.types() {
            let doc_of_method = |name: &str| {
                t.docs
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, d)| d.to_string())
                    .unwrap_or_default()
            };
            let mut methods: Vec<ApiFn> = t
                .methods
                .iter()
                .chain(t.ctx_methods.iter())
                .map(|f| ApiFn {
                    name: f.name.to_string(),
                    signature: f.render(),
                    doc: doc_of_method(f.name),
                })
                // Call-site-typed methods (http arc H8) are part of the type's surface — without
                // them `resp.json::<T>()` would be invisible to completion, hover, and the docs
                // browser, which is most of what makes the turbofish discoverable at all. Listed
                // under their turbofish spelling, like a module's typed functions above (a name may
                // appear in BOTH `methods` and `typed_methods`, so the bare name is not unique).
                .chain(t.typed_methods.iter().map(|f| ApiFn {
                    name: turbofish_name(f.name),
                    signature: f.render(),
                    doc: doc_of_method(f.name),
                }))
                .collect();
            methods.sort_by(|a, b| a.name.cmp(&b.name));
            out.push(ApiType {
                qualified: t.qualified(),
                methods,
            });
        }
    }
    out.sort_by(|a, b| a.qualified.cmp(&b.qualified));
    out
}

/// The extern type with the given qualified name (`std.id.Uuid`), if present.
pub fn type_(qualified: &str) -> Option<ApiType> {
    types().into_iter().find(|t| t.qualified == qualified)
}

/// A single method `qualified::name` (e.g. `std.id.Uuid` / `to_string`), if present.
pub fn method(qualified: &str, name: &str) -> Option<ApiFn> {
    type_(qualified)?
        .methods
        .into_iter()
        .find(|m| m.name == name)
}

// --- Nominal declarations: traits, enums, classes, structs ---------------------------------------
//
// `ExtModule` and `ExtType` are bags of callables, and the two walkers above render them as such.
// The other four nominal hooks are not: a trait is a contract, an enum is a closed choice, and a
// fielded type is a shape. Each renders the way its `.noe` twin does in `docgen` — a `trait`/`enum`/
// `struct`/`class` item on the page of the module it is namespaced under, carrying a whole rendered
// declaration as its signature. That is what makes a native package's reference read identically to
// a Noeta-source one, which is the point of documenting them through the same schema.

/// One nominal declaration of the API reference: a native trait, enum, class, or struct.
#[derive(Debug, Clone)]
pub struct ApiDecl {
    /// The qualified identity (`std.vec.Kernels`).
    pub qualified: String,
    /// The module page this declaration belongs on — its namespace (`std.vec`).
    pub module: String,
    /// The short name (`Kernels`).
    pub name: String,
    /// The docs.json item kind: `trait`, `enum`, `struct`, or `class`.
    pub kind: &'static str,
    /// The whole rendered declaration, ready for a code block.
    pub signature: String,
    /// The declaration's prose, with any per-member prose folded in beneath it (see
    /// [`with_members`]). Empty when the declaration documents nothing.
    pub doc: String,
}

/// Every nominal declaration the registry knows, sorted by qualified name.
pub fn decls() -> Vec<ApiDecl> {
    decls_in(registry::extensions(), &|_| true)
}

/// Every `@`-directive the registry knows, sorted by qualified name — the [`decls`] analogue for the
/// one declared surface that is neither a callable nor a nominal type.
///
/// A directive has no `namespace` of its own (nothing imports one; the name resolves globally after
/// the built-ins and the tier name-space), so it is documented on the page of its **extension's
/// root** — the namespace a reader already associates with the package that ships it.
pub fn directives() -> Vec<ApiDecl> {
    directives_in(registry::extensions(), &|_| true)
}

/// The directives of just the extensions whose `root` is `root`.
pub fn directives_of(root: &str) -> Vec<ApiDecl> {
    directives_in(registry::extensions(), &|ext| ext.root() == root)
}

/// The directives of every registered extension EXCEPT the named units — the publish scope.
pub fn directives_excluding(exclude_units: &[&str]) -> Vec<ApiDecl> {
    directives_in(registry::extensions(), &|ext| {
        !exclude_units.contains(&ext.name())
    })
}

fn directives_in(exts: &[Ext], keep: &dyn Fn(Ext) -> bool) -> Vec<ApiDecl> {
    let mut out: Vec<ApiDecl> = Vec::new();
    for ext in exts.iter().filter(|e| keep(**e)) {
        for d in ext.directives() {
            out.push(ApiDecl {
                qualified: format!("{}.@{}", ext.root(), d.name),
                module: ext.root().to_string(),
                name: format!("@{}", d.name),
                kind: "directive",
                signature: render_directive(d),
                doc: directive_doc(d),
            });
        }
    }
    out.sort_by(|a, b| a.qualified.cmp(&b.qualified));
    out.dedup_by(|a, b| a.qualified == b.qualified);
    out
}

/// A directive as a declaration: how it is written. The positional parameters come from
/// [`ExtDirective::params`] and the named ones from [`ExtDirective::named_keys`], so the rendered
/// form is the invocation the argument contract actually accepts.
fn render_directive(d: &noeta_stdlib::registry::ExtDirective) -> String {
    let mut args: Vec<String> = d.params.iter().map(|p| (*p).to_string()).collect();
    // A variadic directive (`max_args: None`) accepts more than its named parameters.
    if d.max_args.is_none() {
        args.push("…".to_string());
    }
    args.extend(d.named_keys.iter().map(|k| format!("{k}: …")));
    if args.is_empty() {
        format!("@{}", d.name)
    } else {
        format!("@{}({})", d.name, args.join(", "))
    }
}

/// A directive's prose: its hover doc, then the placement rule its `sites` state. Where a directive
/// may be written is not derivable from its name and is the first thing a reader gets wrong, so it
/// is documented rather than left to a diagnostic.
fn directive_doc(d: &noeta_stdlib::registry::ExtDirective) -> String {
    use noeta_stdlib::registry::TierSite;
    let mut out = d.doc.trim().to_string();
    let sites: Vec<&str> = d
        .sites
        .iter()
        .map(|s| match s {
            TierSite::Function => "functions",
            TierSite::Method => "methods",
            TierSite::Type => "types",
            TierSite::Trait => "traits",
        })
        .collect();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    // Empty `sites` means "attaches to nothing" — the same polarity `ExtTier::sites` has. A reader
    // meeting such a directive needs to be told it, not left to infer "anywhere" from silence.
    out.push_str(&match sites.as_slice() {
        [] => "**Attaches to:** nothing — this directive declares no sites.".to_string(),
        _ => format!("**Attaches to:** {}.", sites.join(", ")),
    });
    out
}

/// The nominal declarations of just the extensions whose `root` is `root` — the [`modules_of`]
/// analogue.
pub fn decls_of(root: &str) -> Vec<ApiDecl> {
    decls_in(registry::extensions(), &|ext| ext.root() == root)
}

/// The nominal declarations of every registered extension EXCEPT the named units — the
/// [`modules_excluding`] analogue, and the publish path's scope.
pub fn decls_excluding(exclude_units: &[&str]) -> Vec<ApiDecl> {
    decls_in(registry::extensions(), &|ext| {
        !exclude_units.contains(&ext.name())
    })
}

fn decls_in(exts: &[Ext], keep: &dyn Fn(Ext) -> bool) -> Vec<ApiDecl> {
    let mut out: Vec<ApiDecl> = Vec::new();
    for ext in exts.iter().filter(|e| keep(**e)) {
        for t in ext.traits() {
            out.push(ApiDecl {
                qualified: t.qualified(),
                module: t.namespace.to_string(),
                name: t.name.to_string(),
                kind: "trait",
                signature: render_trait(t),
                doc: with_members(t.doc, trait_members(t), t.docs),
            });
        }
        for e in ext.enums() {
            out.push(ApiDecl {
                qualified: e.qualified(),
                module: e.namespace.to_string(),
                name: e.name.to_string(),
                kind: "enum",
                signature: render_enum(e),
                doc: with_members(e.doc, enum_members(e), e.docs),
            });
        }
        // Classes and structs are one ABI type (`ExtFielded`) behind two hooks; the `kind`
        // discriminant, not the hook, decides how each reads — so both walk the same renderer and a
        // hook/discriminant mismatch (which `Registry::validate` refuses) cannot produce a page
        // that lies about which it is.
        for f in ext.classes().iter().chain(ext.structs().iter()) {
            out.push(ApiDecl {
                qualified: f.qualified(),
                module: f.namespace.to_string(),
                name: f.name.to_string(),
                kind: match f.kind {
                    noeta_stdlib::FieldedKind::Class => "class",
                    noeta_stdlib::FieldedKind::Struct => "struct",
                },
                signature: render_fielded(f),
                doc: with_members(f.doc, fielded_members(f), f.docs),
            });
        }
    }
    out.sort_by(|a, b| a.qualified.cmp(&b.qualified));
    out.dedup_by(|a, b| a.qualified == b.qualified);
    out
}

/// A native trait as a declaration: its associated types, then its `Self`-receiver methods, then any
/// `List<Self>` bulk methods under a marker comment. A defaulted method shows `{ … }` and a required
/// one is bodiless — the same signal `docgen`'s `.noe` `trait_docs` emits, and the one that tells an
/// implementor which methods they must write.
fn render_trait(t: &noeta_stdlib::ExtTrait) -> String {
    use noeta_stdlib::BundleReceiver;
    let mut sig = format!("trait {} {{\n", t.name);
    for a in t.assoc_types {
        sig.push_str(&format!("    type {}\n", a.name));
    }
    let body = |m: &noeta_stdlib::ExtTraitMethod| if m.has_default { " { … }\n" } else { "\n" };
    for m in t
        .methods
        .iter()
        .filter(|m| m.receiver == BundleReceiver::Element)
    {
        sig.push_str(&format!("    {}{}", render_trait_method(m), body(m)));
    }
    let mut bulk = t
        .methods
        .iter()
        .filter(|m| m.receiver == BundleReceiver::Bulk)
        .peekable();
    if bulk.peek().is_some() {
        sig.push_str("\n    // on List<Self>:\n");
        for m in bulk {
            sig.push_str(&format!("    {}{}", render_trait_method(m), body(m)));
        }
    }
    sig.push('}');
    sig
}

/// One trait method's signature. Not [`ExtFn::render`], because a trait method's [`RetTy`] is
/// **receiver-relative** and `ExtFn::render` has no receiver to resolve it against: the receiver
/// rides as slot 0, so `SameAsArg(0)` is `Self` (or `List<Self>` on a bulk method) and every other
/// index is shifted by one against the declared parameters. That is exactly how the checker types
/// the call (`bundle_method_return`), so rendering it any other way documents a return type the
/// compiler does not produce — `vec.Kernels.scale` would read `: number` where the call really
/// yields `Self`.
fn render_trait_method(m: &noeta_stdlib::ExtTraitMethod) -> String {
    use noeta_stdlib::{BundleReceiver, RetTy};
    let params: Vec<String> = m
        .sig
        .params
        .iter()
        .enumerate()
        .map(|(i, ty)| match m.sig.param_names.get(i) {
            Some(n) => format!("{n}: {}", ty.render()),
            None => ty.render(),
        })
        .collect();
    let ret = match m.sig.ret {
        RetTy::SameAsArg(0) => match m.receiver {
            BundleReceiver::Element => "Self".to_string(),
            BundleReceiver::Bulk => "List<Self>".to_string(),
        },
        RetTy::SameAsArg(i) => m
            .sig
            .params
            .get(i - 1)
            .map(noeta_stdlib::SigType::render)
            .unwrap_or_else(|| "dyn".to_string()),
        ref other => other.render(m.sig.params),
    };
    format!("fn {}({}): {ret}", m.sig.name, params.join(", "))
}

/// A native enum as a declaration: its backing (`: string`) when it has one, then its variants —
/// fieldless, algebraic (`Tagged(name: string)`), or backed (`Pending = "pending"`) — then any
/// instance methods.
fn render_enum(e: &noeta_stdlib::ExtEnum) -> String {
    use noeta_stdlib::{EnumBacking, VariantValue};
    let backing = match e.backing {
        EnumBacking::None => "",
        EnumBacking::Str => ": string",
        EnumBacking::Int => ": int",
    };
    let mut sig = format!("enum {}{backing} {{\n", e.name);
    for v in e.variants {
        let payload = if v.fields.is_empty() {
            String::new()
        } else {
            format!(
                "({})",
                v.fields
                    .iter()
                    .map(noeta_stdlib::SigType::render)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let value = match v.value {
            VariantValue::None => String::new(),
            VariantValue::Str(s) => format!(" = \"{s}\""),
            VariantValue::Int(i) => format!(" = {i}"),
        };
        sig.push_str(&format!("    {}{payload}{value}\n", v.name));
    }
    for m in e.methods {
        sig.push_str(&format!("    {}\n", m.render()));
    }
    sig.push('}');
    sig
}

/// A native class or struct as a declaration: its fields with their visibility and mutability, then
/// its instance methods.
fn render_fielded(f: &noeta_stdlib::ExtFielded) -> String {
    let keyword = match f.kind {
        noeta_stdlib::FieldedKind::Class => "class",
        noeta_stdlib::FieldedKind::Struct => "struct",
    };
    let mut sig = format!("{keyword} {} {{\n", f.name);
    for field in f.fields {
        let vis = if field.is_public { "pub " } else { "" };
        let mutability = if field.is_mut { "mut " } else { "" };
        sig.push_str(&format!(
            "    {vis}{mutability}{}: {}\n",
            field.name,
            field.ty.render()
        ));
    }
    for m in f.methods {
        sig.push_str(&format!("    {}\n", m.render()));
    }
    sig.push('}');
    sig
}

/// The members of a trait, in the order [`render_trait`] lists them.
fn trait_members(t: &noeta_stdlib::ExtTrait) -> Vec<(&'static str, String)> {
    t.methods
        .iter()
        .map(|m| (m.sig.name, render_trait_method(m)))
        .collect()
}

/// The members of an enum: its variants (rendered as their bare case name — a variant is not a
/// signature) followed by its instance methods.
fn enum_members(e: &noeta_stdlib::ExtEnum) -> Vec<(&'static str, String)> {
    e.variants
        .iter()
        .map(|v| (v.name, v.name.to_string()))
        .chain(e.methods.iter().map(|m| (m.name, m.render())))
        .collect()
}

/// The members of a fielded type: its fields (rendered `name: T`) followed by its instance methods.
fn fielded_members(f: &noeta_stdlib::ExtFielded) -> Vec<(&'static str, String)> {
    f.fields
        .iter()
        .map(|field| (field.name, format!("{}: {}", field.name, field.ty.render())))
        .chain(f.methods.iter().map(|m| (m.name, m.render())))
        .collect()
}

/// Fold a declaration's own prose and its per-member prose into one markdown body.
///
/// The rendered signature already lists every member, so a member is given its own subsection only
/// when it is **documented** — an undocumented one is already visible above and a bare heading
/// would add nothing. Members appear in declaration order, not the `docs` table's order, so the
/// prose reads in the same order as the signature it annotates.
fn with_members(
    doc: &str,
    members: Vec<(&'static str, String)>,
    docs: &[(&'static str, &'static str)],
) -> String {
    let mut out = doc.trim().to_string();
    for (name, rendered) in members {
        let Some((_, prose)) = docs.iter().find(|(n, _)| *n == name) else {
            continue;
        };
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("#### `{rendered}`\n\n{}", prose.trim()));
    }
    out
}

/// Namespacing violations for the extensions rooted at `pkg_root` (the `--root` form of the publish
/// lint): every extern type such an extension registers must be qualified under that root — a type
/// that leaks into another namespace (most commonly an `ExtType` that omits `namespace:` and so
/// defaults to `std`) would be unreachable and is a publish-blocking error. Returns one message per
/// violation; empty = clean.
pub fn namespace_violations(pkg_root: &str) -> Vec<String> {
    namespace_violations_in(
        registry::extensions(),
        &|ext| under(ext.root(), pkg_root),
        &[],
    )
}

/// Namespacing violations for **every registered non-builtin extension** — the publish-path lint
/// (`noeta doc --api --non-builtin --lint`). `exclude_units` names the toolchain's own builtin units
/// (by [`Extension::name`](noeta_stdlib::Extension::name)); every other unit is the publishing
/// package's real surface, whatever roots it declares. The old form filtered by
/// `root == package segment`, which made the lint vacuous for a package whose extension roots
/// diverge from its segment (`para/p2p` rooting at `para`). Checks, per package unit:
///
/// - its `root()` must not claim a **toolchain-owned root** (`toolchain_roots`: the builtin units'
///   own roots plus the reserved built-in scopes) — an extra unit rooting at `std`/`css`/… would
///   win `use std.X` resolution for its own additions, which assembly-time validation cannot
///   refuse (no module identity collides);
/// - every extern type must be namespaced under the unit's **own** root (belt-and-suspenders with
///   `Registry::validate`, which enforces the same rule at assembly for the default paths).
pub fn namespace_violations_excluding(
    exclude_units: &[&str],
    toolchain_roots: &[&str],
) -> Vec<String> {
    namespace_violations_in(
        registry::extensions(),
        &|ext| !exclude_units.contains(&ext.name()),
        toolchain_roots,
    )
}

/// Whether namespace `ns` sits at or under `root` (`para` / `para.p2p` under `para`).
fn under(ns: &str, root: &str) -> bool {
    ns == root || (ns.starts_with(root) && ns.as_bytes().get(root.len()) == Some(&b'.'))
}

fn namespace_violations_in(
    exts: &[Ext],
    keep: &dyn Fn(Ext) -> bool,
    toolchain_roots: &[&str],
) -> Vec<String> {
    let mut out = Vec::new();
    for ext in exts.iter().filter(|e| keep(**e)) {
        let root = ext.root();
        if let Some(owned) = toolchain_roots.iter().find(|r| under(root, r)) {
            out.push(format!(
                "extension `{}` declares the namespace root `{root}`, which is owned by the Noeta \
                 toolchain (`{owned}`) — a package must publish its surface under its own root",
                ext.name()
            ));
            continue; // its types would all re-report the same squat
        }
        // Every nominal declaration, not just the extern types. All five hooks default `namespace`
        // to `"std"` through their `DEFAULTS`, so all five leak the same way — a package's trait
        // that forgets `namespace:` claims `std.Mergeable`, is unreachable under the package's own
        // root, and squats a name in the toolchain's namespace. The lint checked one of the five.
        for (what, name, namespace) in nominal_namespaces(*ext) {
            if !under(namespace, root) {
                out.push(format!(
                    "{what} `{name}` is namespaced `{namespace}`, not under its extension's root \
                     `{root}` — set `namespace: \"{root}\"` on it (the field defaults to `std`)"
                ));
            }
        }
    }
    out
}

/// Every nominal declaration an extension registers, as `(what it is, its name, its namespace)` —
/// the one place the five hooks that carry a `namespace` are enumerated together, so a lint or a
/// walker over "the extension's declared identities" cannot silently cover four of five.
fn nominal_namespaces(ext: Ext) -> Vec<(&'static str, &'static str, &'static str)> {
    let mut out: Vec<(&'static str, &'static str, &'static str)> = ext
        .types()
        .iter()
        .map(|t| ("extern type", t.name, t.namespace))
        .collect();
    out.extend(ext.traits().iter().map(|t| ("trait", t.name, t.namespace)));
    out.extend(ext.enums().iter().map(|e| ("enum", e.name, e.namespace)));
    out.extend(ext.classes().iter().map(|c| ("class", c.name, c.namespace)));
    out.extend(
        ext.structs()
            .iter()
            .map(|s| ("struct", s.name, s.namespace)),
    );
    out
}

/// The prose for `name` in module `m`, or empty. Keyed by the **declared** name so one table serves
/// the plain, higher-order, and call-site-typed tables alike (a name shared by two of them documents
/// both doors, which is why the `json.parse`/`try_parse` prose describes each).
fn doc_of(m: &noeta_stdlib::ExtModule, name: &str) -> String {
    m.docs
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, d)| d.to_string())
        .unwrap_or_default()
}

/// How a call-site-typed function/method is *listed*: `parse` → `parse::<T>`. The surface spelling
/// is the turbofish one, and it keeps a typed entry distinct from a same-named plain one — the two
/// registration tables deliberately allow a shared name, so the bare name is not a unique key.
fn turbofish_name(name: &str) -> String {
    format!("{name}::<T>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_enumerates_std_math_with_signatures() {
        let mods = modules();
        let math = mods
            .iter()
            .find(|m| m.qualified == "std.math")
            .expect("std.math is a registered module");
        let sqrt = math
            .functions
            .iter()
            .find(|f| f.name == "sqrt")
            .expect("math.sqrt exists");
        assert_eq!(sqrt.signature, "fn sqrt(x: float): float");
        assert!(sqrt.doc.contains("square root"));
        let hyp = math.functions.iter().find(|f| f.name == "hypot").unwrap();
        assert!(hyp.doc.contains("Euclidean"));
        assert_eq!(hyp.signature, "fn hypot(x: float, y: float): float");
    }

    #[test]
    fn call_site_typed_functions_are_listed_under_their_turbofish_spelling() {
        // The `typed_functions` table used to be skipped here while `typed_methods` was chained in,
        // so `json.parse::<T>` / `json.try_parse::<T>` existed in the language and in no reference.
        let json = module("std.json").expect("std.json is a registered module");
        let names: Vec<&str> = json.functions.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"parse"), "the dynamic door: {names:?}");
        assert!(names.contains(&"parse::<T>"), "the typed door: {names:?}");
        assert!(
            names.contains(&"try_parse"),
            "recoverable dynamic: {names:?}"
        );
        assert!(
            names.contains(&"try_parse::<T>"),
            "recoverable typed: {names:?}"
        );
        // Both doors of one name carry the shared prose and their own signature.
        let dynamic = function("std.json", "try_parse").expect("plain try_parse");
        let typed = function("std.json", "try_parse::<T>").expect("typed try_parse");
        assert_eq!(
            dynamic.signature,
            "fn try_parse(text: string): Result<dyn, JsonError>"
        );
        assert!(
            typed
                .signature
                .starts_with("fn try_parse(text: string): Result<T, JsonError>")
        );
        assert!(dynamic.doc.contains("Result<dyn, JsonError>"));
        assert_eq!(dynamic.doc, typed.doc, "one table, keyed by declared name");
    }

    #[test]
    fn extern_type_methods_carry_prose() {
        // The Uuid type's methods are all documented (Arc 2 A3 backfill).
        let uuid = type_("std.id.Uuid").expect("std.id.Uuid present");
        assert!(!uuid.methods.is_empty());
        assert!(
            uuid.methods.iter().all(|m| !m.doc.is_empty()),
            "every Uuid method has prose"
        );
        let to_string = method("std.id.Uuid", "to_string").unwrap();
        assert!(to_string.doc.contains("hyphenated"));
    }

    #[test]
    fn every_docs_key_names_a_real_function_or_method() {
        // Prose lives in `ExtModule::docs`/`ExtType::docs` tables keyed by name — co-located with
        // the signature tables but not compile-checked against them. This guard fails CI if a
        // rename or typo leaves a doc entry keyed to a name no function/method has, which would
        // otherwise silently orphan the prose (the symbol quietly drops back to signature-only).
        use std::collections::HashSet;
        for ext in registry::extensions() {
            for m in ext.modules() {
                let names: HashSet<&str> = m
                    .functions
                    .iter()
                    .chain(m.ctx_functions.iter())
                    .chain(m.typed_functions.iter())
                    .map(|f| f.name)
                    .collect();
                for (key, _) in m.docs {
                    assert!(
                        names.contains(key),
                        "module `{}.{}` docs key `{key}` names no function",
                        ext.root(),
                        m.name
                    );
                }
            }
            for t in ext.types() {
                let names: HashSet<&str> = t
                    .methods
                    .iter()
                    .chain(t.ctx_methods.iter())
                    .chain(t.typed_methods.iter())
                    .map(|f| f.name)
                    .collect();
                for (key, _) in t.docs {
                    assert!(
                        names.contains(key),
                        "type `{}` docs key `{key}` names no method",
                        t.qualified()
                    );
                }
            }
            // The nominal hooks' tables are keyed the same way and orphan the same way. Each is
            // checked against the members `with_members` will look the key up among — the exact set
            // that decides whether the prose renders — so a rename that strands an entry fails here
            // rather than silently dropping a paragraph from the published page.
            for tr in ext.traits() {
                let names: HashSet<&str> = tr.methods.iter().map(|m| m.sig.name).collect();
                for (key, _) in tr.docs {
                    assert!(
                        names.contains(key),
                        "trait `{}` docs key `{key}` names no method",
                        tr.qualified()
                    );
                }
            }
            for e in ext.enums() {
                let names: HashSet<&str> = e
                    .variants
                    .iter()
                    .map(|v| v.name)
                    .chain(e.methods.iter().map(|m| m.name))
                    .collect();
                for (key, _) in e.docs {
                    assert!(
                        names.contains(key),
                        "enum `{}` docs key `{key}` names no variant or method",
                        e.qualified()
                    );
                }
            }
            for f in ext.classes().iter().chain(ext.structs().iter()) {
                let names: HashSet<&str> = f
                    .fields
                    .iter()
                    .map(|field| field.name)
                    .chain(f.methods.iter().map(|m| m.name))
                    .collect();
                for (key, _) in f.docs {
                    assert!(
                        names.contains(key),
                        "type `{}` docs key `{key}` names no field or method",
                        f.qualified()
                    );
                }
            }
        }
    }

    // --- nominal declarations (traits / enums / classes / structs) -------------------------------

    #[test]
    fn native_traits_reach_the_api_reference_with_their_prose() {
        // The hole this closes: `decls()` did not exist, so `ext.traits()` was documented nowhere.
        let kernels = decls()
            .into_iter()
            .find(|d| d.qualified == "std.vec.Kernels")
            .expect("std.vec.Kernels is a registered native trait");
        assert_eq!(kernels.kind, "trait");
        assert_eq!(kernels.module, "std.vec", "lands on its namespace's page");
        // The declaration renders whole: associated types, the contract, and the default marker.
        assert!(kernels.signature.starts_with("trait Kernels {"));
        assert!(kernels.signature.contains("type Wide"));
        assert!(
            kernels
                .signature
                .contains("fn add(other: Self): Self { … }")
        );
        // The bulk methods are grouped under their real receiver rather than passed off as `Self`.
        assert!(kernels.signature.contains("// on List<Self>:"));
        assert!(
            kernels
                .signature
                .contains("fn add_all(other: List<Self>): List<Self>")
        );
        // The trait's own prose leads, and per-method prose follows under its signature.
        assert!(kernels.doc.starts_with("Element-wise arithmetic"));
        assert!(
            kernels
                .doc
                .contains("#### `fn dot(other: Self): Self::Wide`")
        );
        assert!(kernels.doc.contains("the sum of the element-wise products"));
    }

    #[test]
    fn a_trait_methods_receiver_relative_return_renders_as_self() {
        // `RetTy::SameAsArg(0)` is the RECEIVER on a trait method (`bundle_method_return`), not the
        // first declared parameter. `ExtFn::render` cannot know that and renders `vec.Kernels.scale`
        // as `: number` — the type of its `factor` argument, which is not what the call yields.
        // Rendering the reference from that would have documented a return the compiler never
        // produces.
        let kernels = decls()
            .into_iter()
            .find(|d| d.qualified == "std.vec.Kernels")
            .unwrap();
        assert!(kernels.signature.contains("fn scale(factor: number): Self"));
        assert!(
            !kernels
                .signature
                .contains("fn scale(factor: number): number")
        );
        // A no-argument receiver-relative return has no parameter to be confused with at all.
        assert!(kernels.signature.contains("fn abs(): Self"));
        // And the bulk twin of the same shape is `List<Self>`.
        assert!(
            kernels
                .signature
                .contains("fn scale_all(factor: number): List<Self>")
        );
    }

    #[test]
    fn native_enums_and_structs_reach_the_api_reference() {
        let all = decls();
        let framing = all
            .iter()
            .find(|d| d.qualified == "std.http.Framing")
            .expect("std.http.Framing is a registered native enum");
        assert_eq!(framing.kind, "enum");
        assert_eq!(framing.module, "std.http");
        assert!(framing.signature.contains("enum Framing {"));
        for variant in ["Sse", "Ndjson", "Lines"] {
            assert!(framing.signature.contains(variant), "variant {variant}");
            // Per-variant prose is what a caller actually needs here — which `Frame` fields each
            // framing populates is not derivable from the variant's name.
            assert!(
                framing.doc.contains(&format!("#### `{variant}`")),
                "prose for {variant}"
            );
        }

        let frame = all
            .iter()
            .find(|d| d.qualified == "std.http.Frame")
            .expect("std.http.Frame is a registered native struct");
        // The `kind` discriminant decides the keyword, not the hook it arrived through.
        assert_eq!(frame.kind, "struct");
        assert!(frame.signature.contains("struct Frame {"));
        assert!(frame.signature.contains("pub event: string"));
        assert!(frame.signature.contains("pub retry: Option<int>"));
        assert!(frame.doc.contains("#### `retry: Option<int>`"));
    }

    #[test]
    fn an_undocumented_member_gets_no_empty_subsection() {
        // The signature already lists every member, so a bare heading over nothing is noise. Only
        // documented members earn a subsection.
        let doc = with_members(
            "The trait.",
            vec![("a", "fn a(): Self".into()), ("b", "fn b(): Self".into())],
            &[("b", "Only b is documented.")],
        );
        assert!(doc.contains("#### `fn b(): Self`"));
        assert!(!doc.contains("#### `fn a(): Self`"));
        // A declaration with no prose of its own but documented members still renders them.
        let members_only = with_members("", vec![("a", "fn a(): Self".into())], &[("a", "Prose.")]);
        assert!(members_only.starts_with("#### `fn a(): Self`"));
    }

    static DEMO_DIRECTIVES: &[noeta_stdlib::registry::ExtDirective] = &[
        noeta_stdlib::registry::ExtDirective {
            name: "openapi",
            sites: &[noeta_stdlib::registry::TierSite::Type],
            max_args: Some(1),
            named_keys: &[],
            detail: "@openapi(\"spec.json\")",
            doc: "Generates one method per operation in the named OpenAPI document.",
            params: &["spec"],
            expand: None,
        },
        // A directive with no arguments and no declared sites — the "attaches to nothing" polarity
        // an empty `sites` means, which a reader has no other way to learn.
        noeta_stdlib::registry::ExtDirective {
            name: "marker",
            sites: &[],
            max_args: Some(0),
            named_keys: &[],
            detail: "@marker",
            doc: "",
            params: &[],
            expand: None,
        },
    ];
    struct DirectiveExt;
    impl noeta_stdlib::Extension for DirectiveExt {
        fn name(&self) -> &'static str {
            "api-native"
        }
        fn root(&self) -> &'static str {
            "para"
        }
        fn modules(&self) -> &'static [noeta_stdlib::ExtModule] {
            &[]
        }
        fn directives(&self) -> &'static [noeta_stdlib::registry::ExtDirective] {
            DEMO_DIRECTIVES
        }
    }
    static DIRECTIVE_EXT: DirectiveExt = DirectiveExt;

    #[test]
    fn directives_are_documented_with_their_invocation_and_placement() {
        // A directive is the one declared surface that is neither a callable nor a nominal type, so
        // no walker covered it: `@openapi` — para/api's flagship, the whole reason `ExtDirective`
        // grew an `expand` hook — appeared in no reference at all. Everything needed to document
        // one was already on the declaration (`doc`, `params`, `named_keys`, `sites`); nothing read
        // it.
        let ds = directives_in(&[&DIRECTIVE_EXT], &|_| true);
        assert_eq!(ds.len(), 2, "{ds:?}");

        let openapi = ds.iter().find(|d| d.name == "@openapi").unwrap();
        assert_eq!(openapi.kind, "directive");
        // Documented on its extension's root — a directive has no namespace of its own.
        assert_eq!(openapi.module, "para");
        assert_eq!(openapi.qualified, "para.@openapi");
        // The rendered form is the invocation the argument contract accepts.
        assert_eq!(openapi.signature, "@openapi(spec)");
        assert!(openapi.doc.contains("one method per operation"));
        // Where it may be written is the first thing a reader gets wrong, so it is stated.
        assert!(
            openapi.doc.contains("**Attaches to:** types."),
            "{}",
            openapi.doc
        );

        let marker = ds.iter().find(|d| d.name == "@marker").unwrap();
        assert_eq!(marker.signature, "@marker", "no arguments, no parens");
        // Empty `sites` means "attaches to nothing", the same polarity `ExtTier::sites` has —
        // silence would read as "anywhere", which is the opposite.
        assert!(marker.doc.contains("nothing"), "{}", marker.doc);
    }

    #[test]
    fn decls_scope_the_same_way_modules_do() {
        // The three scopes are the publish path's contract: `--root` filters to one namespace and
        // `--non-builtin` is empty in the stock toolchain (every unit is a builtin one).
        assert!(
            decls_of("std")
                .iter()
                .all(|d| d.qualified.starts_with("std"))
        );
        assert!(!decls_of("std").is_empty());
        assert!(decls_of("nosuchpkg").is_empty());
        let builtin: Vec<&str> = registry::extensions().iter().map(|e| e.name()).collect();
        assert!(decls_excluding(&builtin).is_empty());
    }

    #[test]
    fn namespace_violations_are_clean_for_well_namespaced_extensions() {
        // The stdlib (and bundled first-party extensions) keep their types under their own root, so
        // the lint reports nothing — no false positives. A package root with no extension present
        // is likewise clean. (The catches-a-leak case needs a composed package extension and is
        // covered by the cli integration test over the imgfx fixture.)
        assert!(namespace_violations("std").is_empty());
        assert!(namespace_violations("nosuchpkg").is_empty());
    }

    // --- publish docs-gen scoping (docsgen-root regression) -------------------------------------
    //
    // `noeta publish` documents a native package by running the composed toolchain's docs
    // generator. It used to scope with `--root <package segment>`, but an extension may
    // deliberately root at a DIFFERENT namespace (`Extension::root()` defaults to `name()` and is
    // overridable) — the published para/p2p package roots at `para` while its segment is `p2p`, so
    // the exact-root filter matched nothing and the registry stored `{"modules": []}`. The publish
    // path now excludes the toolchain's builtin units by unit NAME and documents everything else.
    // These fixtures mirror that shape without composing a toolchain (the cheap layer that would
    // have caught it).

    static DIVERGENT_FNS: &[noeta_stdlib::ExtFn] = &[noeta_stdlib::ExtFn {
        param_names: &[],
        name: "connect",
        params: &[noeta_stdlib::SigType::Int],
        ret: noeta_stdlib::RetTy::Concrete(noeta_stdlib::SigType::Int),
    }];
    static DIVERGENT_MODULES: &[noeta_stdlib::ExtModule] = &[noeta_stdlib::ExtModule {
        name: "p2p",
        functions: DIVERGENT_FNS,
        ..noeta_stdlib::ExtModule::DEFAULTS
    }];
    static DIVERGENT_TYPES: &[noeta_stdlib::ExtType] = &[noeta_stdlib::ExtType {
        name: "Peer",
        namespace: "para.p2p",
        ..noeta_stdlib::ExtType::DEFAULTS
    }];

    /// The para/p2p shape: package segment `p2p`, extension unit `p2p-native`, root `para`.
    struct DivergentRootExt;
    impl noeta_stdlib::Extension for DivergentRootExt {
        fn name(&self) -> &'static str {
            "p2p-native"
        }
        fn root(&self) -> &'static str {
            "para"
        }
        fn modules(&self) -> &'static [noeta_stdlib::ExtModule] {
            DIVERGENT_MODULES
        }
        fn types(&self) -> &'static [noeta_stdlib::ExtType] {
            DIVERGENT_TYPES
        }
    }
    static DIVERGENT_EXT: DivergentRootExt = DivergentRootExt;

    static CONVENTION_MODULES: &[noeta_stdlib::ExtModule] = &[noeta_stdlib::ExtModule {
        name: "fx",
        functions: DIVERGENT_FNS,
        ..noeta_stdlib::ExtModule::DEFAULTS
    }];
    /// The convention shape (acme/imgfx): root == unit name == package segment.
    struct ConventionExt;
    impl noeta_stdlib::Extension for ConventionExt {
        fn name(&self) -> &'static str {
            "imgfx"
        }
        fn modules(&self) -> &'static [noeta_stdlib::ExtModule] {
            CONVENTION_MODULES
        }
    }
    static CONVENTION_EXT: ConventionExt = ConventionExt;

    static LEAKY_TYPES: &[noeta_stdlib::ExtType] = &[noeta_stdlib::ExtType {
        name: "Peer",
        namespace: "std", // the classic omitted-`namespace:` default
        ..noeta_stdlib::ExtType::DEFAULTS
    }];
    /// A divergent-root package whose type leaks into `std`.
    struct LeakyExt;
    impl noeta_stdlib::Extension for LeakyExt {
        fn name(&self) -> &'static str {
            "leaky-native"
        }
        fn root(&self) -> &'static str {
            "para"
        }
        fn modules(&self) -> &'static [noeta_stdlib::ExtModule] {
            &[]
        }
        fn types(&self) -> &'static [noeta_stdlib::ExtType] {
            LEAKY_TYPES
        }
    }
    static LEAKY_EXT: LeakyExt = LeakyExt;

    /// A package extension squatting a toolchain-owned root outright.
    struct SquatExt;
    impl noeta_stdlib::Extension for SquatExt {
        fn name(&self) -> &'static str {
            "squat-native"
        }
        fn root(&self) -> &'static str {
            "std"
        }
        fn modules(&self) -> &'static [noeta_stdlib::ExtModule] {
            CONVENTION_MODULES
        }
    }
    static SQUAT_EXT: SquatExt = SquatExt;

    /// The builtin-unit names of a `std`-only assembly, the exclusion set the publish path passes.
    fn std_unit_names() -> Vec<&'static str> {
        noeta_stdlib::registry::std_units()
            .iter()
            .map(|e| e.name())
            .collect()
    }

    #[test]
    fn publish_docs_scope_includes_a_divergent_root_extension() {
        // (a) The publish-path scope (exclude builtins by unit name) yields the divergent
        // extension's whole surface, under its REAL root…
        let mut exts: Vec<Ext> = noeta_stdlib::registry::std_units();
        exts.push(&DIVERGENT_EXT);
        let builtin = std_unit_names();
        let keep = |ext: Ext| !builtin.contains(&ext.name());
        let mods = modules_in(&exts, &keep);
        assert_eq!(
            mods.iter()
                .map(|m| m.qualified.as_str())
                .collect::<Vec<_>>(),
            ["para.p2p"],
            "the divergent-root extension's module is documented, std's are excluded"
        );
        assert_eq!(mods[0].functions[0].name, "connect");
        let types = types_in(&exts, &keep);
        assert_eq!(
            types
                .iter()
                .map(|t| t.qualified.as_str())
                .collect::<Vec<_>>(),
            ["para.p2p.Peer"]
        );
        // …while the old publish scoping — exact root == the package's manifest segment (`p2p`) —
        // matches nothing: the empty `{"modules": []}` artifact this regression pins down.
        assert!(
            modules_in(&exts, &|ext| ext.root() == "p2p").is_empty(),
            "root-equals-segment filtering is exactly the bug"
        );
    }

    #[test]
    fn publish_docs_scope_is_unchanged_for_the_convention_case() {
        // (b) A package whose extension follows the convention (root == unit name == segment)
        // documents identically under the old `--root` filter and the new builtin-exclusion scope.
        let mut exts: Vec<Ext> = noeta_stdlib::registry::std_units();
        exts.push(&CONVENTION_EXT);
        let builtin = std_unit_names();
        let by_exclusion = modules_in(&exts, &|ext| !builtin.contains(&ext.name()));
        let by_root = modules_in(&exts, &|ext| ext.root() == "imgfx");
        assert_eq!(
            by_exclusion
                .iter()
                .map(|m| &m.qualified)
                .collect::<Vec<_>>(),
            by_root.iter().map(|m| &m.qualified).collect::<Vec<_>>(),
        );
        assert_eq!(by_exclusion[0].qualified, "imgfx.fx");
    }

    #[test]
    fn publish_lint_fires_for_a_divergent_root_package() {
        // (c) The publish-path lint checks the package's REAL extensions. A divergent-root
        // package's type leaking into `std` fires (the old segment-root filter skipped the
        // extension entirely, making the lint vacuous)…
        let violations = namespace_violations_in(&[&LEAKY_EXT], &|_| true, &["std"]);
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(
            violations[0].contains("`Peer`") && violations[0].contains("`std`"),
            "{violations:?}"
        );
        // …and an extension claiming a toolchain-owned root at all is itself a violation.
        let squat = namespace_violations_in(&[&SQUAT_EXT], &|_| true, &["std", "css", "html"]);
        assert_eq!(squat.len(), 1, "{squat:?}");
        assert!(squat[0].contains("`squat-native`"), "{squat:?}");
        // A well-namespaced divergent-root package is clean under the same checks.
        assert!(
            namespace_violations_in(&[&DIVERGENT_EXT], &|_| true, &["std", "css", "html"])
                .is_empty()
        );
    }

    /// A package whose trait, enum, class and struct each forget `namespace:` and so default to
    /// `std` — the exact omission the lint exists to catch, on the four hooks it did not check.
    static LEAKY_TRAITS: &[noeta_stdlib::ExtTrait] = &[noeta_stdlib::ExtTrait {
        name: "Mergeable",
        ..noeta_stdlib::ExtTrait::DEFAULTS
    }];
    static LEAKY_ENUMS: &[noeta_stdlib::ExtEnum] = &[noeta_stdlib::ExtEnum {
        name: "Framing",
        ..noeta_stdlib::ExtEnum::DEFAULTS
    }];
    static LEAKY_CLASSES: &[noeta_stdlib::ExtClass] = &[noeta_stdlib::ExtClass {
        name: "Node",
        ..noeta_stdlib::ExtClass::DEFAULTS
    }];
    static LEAKY_STRUCTS: &[noeta_stdlib::ExtStruct] = &[noeta_stdlib::ExtStruct {
        name: "Frame",
        ..noeta_stdlib::ExtStruct::STRUCT_DEFAULTS
    }];
    struct LeakyNominalExt;
    impl noeta_stdlib::Extension for LeakyNominalExt {
        fn name(&self) -> &'static str {
            "leaky-nominal"
        }
        fn root(&self) -> &'static str {
            "para"
        }
        fn modules(&self) -> &'static [noeta_stdlib::ExtModule] {
            &[]
        }
        fn traits(&self) -> &'static [noeta_stdlib::ExtTrait] {
            LEAKY_TRAITS
        }
        fn enums(&self) -> &'static [noeta_stdlib::ExtEnum] {
            LEAKY_ENUMS
        }
        fn classes(&self) -> &'static [noeta_stdlib::ExtClass] {
            LEAKY_CLASSES
        }
        fn structs(&self) -> &'static [noeta_stdlib::ExtStruct] {
            LEAKY_STRUCTS
        }
    }
    static LEAKY_NOMINAL_EXT: LeakyNominalExt = LeakyNominalExt;

    #[test]
    fn the_publish_lint_covers_every_nominal_hook_not_just_extern_types() {
        // The lint iterated `ext.types()` alone, so a package trait that omitted `namespace:`
        // claimed `std.Mergeable` — unreachable under the package's own root, squatting a name in
        // the toolchain's namespace — and published clean. All five hooks default to `"std"`
        // through their `DEFAULTS`, so all five leak identically.
        let violations = namespace_violations_in(&[&LEAKY_NOMINAL_EXT], &|_| true, &[]);
        assert_eq!(
            violations.len(),
            4,
            "one per leaked declaration: {violations:?}"
        );
        // Each names what it is, so the fix ("set `namespace:`") lands on the right declaration.
        for (what, name) in [
            ("trait", "Mergeable"),
            ("enum", "Framing"),
            ("class", "Node"),
            ("struct", "Frame"),
        ] {
            assert!(
                violations
                    .iter()
                    .any(|v| v.starts_with(&format!("{what} `{name}`")) && v.contains("`std`")),
                "a leaked {what} is reported: {violations:?}"
            );
        }
    }

    #[test]
    fn a_well_namespaced_nominal_surface_is_clean() {
        // No false positives: the shipped stdlib's own traits/enums/structs sit under `std`, which
        // is their unit's root, and the whole registry lints clean under it.
        assert!(namespace_violations("std").is_empty());
    }

    #[test]
    fn modules_are_sorted_and_lookups_resolve() {
        let mods = modules();
        assert!(mods.windows(2).all(|w| w[0].qualified <= w[1].qualified));
        assert!(module("std.math").is_some());
        assert!(function("std.math", "pow").is_some());
        assert!(function("std.math", "nope").is_none());
        assert!(module("std.nonexistent").is_none());
    }
}
