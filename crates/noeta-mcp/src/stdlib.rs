//! M2 — the `stdlib_api` tool's engine: render the native-extension registry into agent-readable
//! Noeta signatures, so an agent reads the *real* standard-library surface instead of inventing
//! calls it half-remembers from other languages.
//!
//! The registry ([`noeta_stdlib::registry`]) is the single source of truth the checker itself maps
//! onto its `Type` lattice — every `use std.…` module, its function signatures, and every extern
//! value type with its methods live here as [`SigType`]/[`RetTy`] data. This module walks that data
//! and renders each signature back into surface syntax (`fn split(string, string): List<string>`),
//! the form an agent should copy. Nothing here runs code or touches the salsa graph; it is a pure
//! projection of static registry data, so it needs no database and no host.

use noeta_stdlib::registry;
use noeta_stdlib::{ExtFn, ExtType, SigType};
use rmcp::schemars;
use serde::Serialize;

/// The `stdlib_api` result. Single-shaped whether the caller filtered or not: a `module`/type filter
/// narrows `modules`/`types` to the matched entry (full signatures); no filter lists the whole
/// surface. `not_found` is set when a filter matched nothing and the full catalog is returned as a
/// fallback so the agent can see the valid names.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct StdlibApiOutput {
    /// The `use`-able modules, each with its fully-qualified identity (`std.math`) and function
    /// signatures. One entry when a specific module was requested; every module otherwise.
    pub modules: Vec<ModuleApi>,
    /// The extern value types (`Uuid`, `Response`, …) with their instance-method signatures. One
    /// entry when a type was requested; every type otherwise.
    pub types: Vec<TypeApi>,
    /// True when a `module` filter was given but matched no module or type — `modules`/`types` then
    /// hold the full catalog as a fallback so the agent can pick a real name.
    pub not_found: bool,
}

/// One native module rendered for an agent: its qualified identity, its dependency ring, and its
/// function signatures (plain calls and higher-order/ctx calls, merged — the split is a backend
/// dispatch detail an agent never sees).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ModuleApi {
    /// The fully-qualified module identity to `use` and call under, e.g. `std.math`, `std.http.client`.
    pub module: String,
    /// The optional native-dependency ring gating the module (`ring-http-client`), or absent for
    /// always-on core. Informational — the agent still calls it the same way.
    pub ring: Option<String>,
    /// Every function the module exposes, in registration order.
    pub functions: Vec<FnSig>,
    /// The module's **method bundles** (kernel methods): named method sets a user type acquires by
    /// explicit opt-in — `impl <module>.<Bundle> for T {}` — validated against the bundle's
    /// structural constraint at compile time. Empty for most modules.
    pub bundles: Vec<BundleApi>,
}

/// One method bundle rendered for an agent: its opt-in syntax, what a binding type must look like,
/// and the methods the type (and `List<T>`, for the bulk forms) acquires.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct BundleApi {
    /// The bundle's surface name, e.g. `Kernels`.
    pub name: String,
    /// The opt-in a user writes, e.g. `impl vec.Kernels for MyType {}`.
    pub impl_syntax: String,
    /// The structural constraint a binding type must satisfy, e.g.
    /// `@packed struct with fields (f32, f32, f32), column layout`.
    pub constraint: String,
    /// Methods acquired on a value of the bound type itself (`v.dot(w)`).
    pub element_methods: Vec<FnSig>,
    /// Methods acquired on a `List<T>` of the bound type (`xs.dot_all(ys)`).
    pub bulk_methods: Vec<FnSig>,
}

/// One extern value type rendered for an agent: its name, the built-in traits it satisfies, and its
/// method signatures. Methods are called `value.method(args)`; the receiver is implicit.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TypeApi {
    /// The type name as it appears in Noeta source, e.g. `Uuid`, `Response`.
    pub name: String,
    /// The namespace the type lives under (extern-type namespacing) — its qualified identity is
    /// `namespace.name` (`std.id.Uuid`), and it must be **imported** to use its short name:
    /// `use std.id.Uuid` (extern types are no longer globally reserved).
    pub namespace: String,
    /// Built-in traits this type declares (e.g. `Mergeable`), empty for most.
    pub traits: Vec<String>,
    /// Every instance method, in declaration order.
    pub methods: Vec<FnSig>,
}

/// One rendered function or method signature. `signature` is the copy-ready surface form; `params`
/// and `returns` are the same data split out so a caller need not re-parse the string.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct FnSig {
    pub name: String,
    /// The full rendered signature, e.g. `fn split(string, string): List<string>`. Native parameters
    /// carry no names in the registry, so parameters render as their types positionally.
    pub signature: String,
    /// The rendered parameter types, in order (a trailing `?` marks an optional parameter).
    pub params: Vec<String>,
    /// The rendered return type.
    pub returns: String,
}

/// Answer a `stdlib_api` request. `filter` is an optional module identity (`std.math`, `math`,
/// or a `http` prefix that expands to `std.http.client`/`server`) or an extern type name (`Uuid`).
/// `None` returns the whole surface.
pub fn query(filter: Option<&str>) -> StdlibApiOutput {
    match filter {
        None => StdlibApiOutput {
            modules: all_modules(),
            types: all_types(),
            not_found: false,
        },
        Some(q) => query_filtered(q.trim()),
    }
}

fn query_filtered(q: &str) -> StdlibApiOutput {
    // 1. An exact module identity (qualified `std.math` or bare `math`).
    if let Some(m) = registry::find_module(q) {
        return StdlibApiOutput {
            modules: vec![render_module(qualified_name(q, m), m)],
            types: Vec::new(),
            not_found: false,
        };
    }
    // 2. An extern type name — short (`Uuid`, unique across namespaces) or qualified (`std.id.Uuid`).
    if let Some(t) = registry::resolve_type(q).or_else(|| registry::find_type_qualified(q)) {
        return StdlibApiOutput {
            modules: Vec::new(),
            types: vec![render_type(t)],
            not_found: false,
        };
    }
    // 3. A prefix over qualified module identities (`http` → std.http.client / std.http.server).
    let prefix: Vec<ModuleApi> = each_module()
        .filter(|(qname, m)| {
            qname == q || registry::module_name(qname).starts_with(q) || m.name.starts_with(q)
        })
        .map(|(qname, m)| render_module(qname, m))
        .collect();
    if !prefix.is_empty() {
        return StdlibApiOutput {
            modules: prefix,
            types: Vec::new(),
            not_found: false,
        };
    }
    // 4. No match — return the full catalog so the agent can pick a real name.
    StdlibApiOutput {
        modules: all_modules(),
        types: all_types(),
        not_found: true,
    }
}

/// The qualified identity to display for a matched module: the query already qualified keeps its
/// form; a bare-name match is re-qualified against the module's extension root.
fn qualified_name(query: &str, m: &noeta_stdlib::ExtModule) -> String {
    // If the caller passed a root-qualified path that resolved, use it verbatim; otherwise find the
    // module in the enumeration to recover its root.
    if query.contains('.') && registry::find_module(query).is_some() {
        return query.to_string();
    }
    each_module()
        .find(|(_, mm)| mm.name == m.name)
        .map(|(qname, _)| qname)
        .unwrap_or_else(|| m.name.to_string())
}

/// Every registered `(qualified identity, module)` pair — `std.math`, `std.http.client`, …
fn each_module() -> impl Iterator<Item = (String, &'static noeta_stdlib::ExtModule)> {
    registry::extensions().iter().flat_map(|e| {
        e.modules()
            .iter()
            .map(move |m| (format!("{}.{}", e.root(), m.name), m))
    })
}

fn all_modules() -> Vec<ModuleApi> {
    let mut modules: Vec<ModuleApi> = each_module()
        .map(|(qname, m)| render_module(qname, m))
        .collect();
    modules.sort_by(|a, b| a.module.cmp(&b.module));
    modules
}

fn all_types() -> Vec<TypeApi> {
    let mut types: Vec<TypeApi> = registry::extensions()
        .iter()
        .flat_map(|e| e.types())
        .map(render_type)
        .collect();
    types.sort_by(|a, b| a.name.cmp(&b.name));
    types
}

fn render_module(qname: String, m: &noeta_stdlib::ExtModule) -> ModuleApi {
    // `functions` (plain marshalled calls) and `ctx_functions` (higher-order/closure-taking calls)
    // are the same surface to an agent — merge them, plain first, in registration order.
    let functions = m
        .functions
        .iter()
        .chain(m.ctx_functions.iter())
        .map(render_fn)
        .collect();
    let bundles = m
        .bundles
        .iter()
        .map(|b| render_bundle(&qname, m.name, b))
        .collect();
    ModuleApi {
        module: qname,
        ring: m.ring.map(str::to_string),
        functions,
        bundles,
    }
}

fn render_bundle(qualified: &str, module: &str, b: &noeta_stdlib::ExtBundle) -> BundleApi {
    use noeta_stdlib::{BundleReceiver, ConstraintField, ConstraintLayout};
    // The impl site names the bundle through the module *binding* (`use std.{vec}` then
    // `impl vec.Kernels for T {}`), so the short module name is the one to show.
    let impl_syntax = format!(
        "use {qualified} … impl {module}.{} for YourType {{}}",
        b.name
    );
    let fields = b
        .constraint
        .fields
        .iter()
        .map(|f| match f {
            ConstraintField::Int => "int".to_string(),
            ConstraintField::Float => "float".to_string(),
            ConstraintField::F32 => "f32".to_string(),
            ConstraintField::Bool => "bool".to_string(),
            ConstraintField::IntN { bits, signed } => {
                format!("{}{bits}", if *signed { 'i' } else { 'u' })
            }
            ConstraintField::AnyNumeric => "numeric".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let layout = match b.constraint.layout {
        ConstraintLayout::Any => "any layout",
        ConstraintLayout::Row => "row layout",
        ConstraintLayout::Column => "column layout",
    };
    // A `Uniform` constraint reads only `fields[0]` — every field must be that kind, `min` or more.
    let fields = match b.constraint.arity {
        noeta_stdlib::ConstraintArity::Exact => fields,
        noeta_stdlib::ConstraintArity::Uniform { min } => {
            format!("{min}+ uniform {fields}")
        }
    };
    let (mut element_methods, mut bulk_methods) = (Vec::new(), Vec::new());
    for m in b.methods {
        let sig = render_fn(&m.sig);
        match m.receiver {
            BundleReceiver::Element => element_methods.push(sig),
            BundleReceiver::Bulk => bulk_methods.push(sig),
        }
    }
    BundleApi {
        name: b.name.to_string(),
        impl_syntax,
        constraint: format!("@packed struct with fields ({fields}), {layout}"),
        element_methods,
        bulk_methods,
    }
}

fn render_type(t: &ExtType) -> TypeApi {
    let methods = t
        .methods
        .iter()
        .chain(t.ctx_methods.iter())
        .map(render_fn)
        .collect();
    TypeApi {
        name: t.name.to_string(),
        namespace: t.namespace.to_string(),
        traits: t.traits.iter().map(|s| s.to_string()).collect(),
        methods,
    }
}

/// Project an [`ExtFn`] into the tool's shape. The rendering itself is the canonical
/// [`ExtFn::render`]/[`SigType::render`] in `noeta-ext-abi` — the same renderer the LSP's
/// completion detail uses, so every tooling surface shows one syntax.
fn render_fn(f: &ExtFn) -> FnSig {
    FnSig {
        name: f.name.to_string(),
        signature: f.render(),
        params: f.params.iter().map(SigType::render).collect(),
        returns: f.ret.render(f.params),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_filter_lists_the_whole_surface() {
        let out = query(None);
        assert!(!out.not_found);
        // A representative core module and extern type are always present.
        assert!(
            out.modules.iter().any(|m| m.module == "std.math"),
            "std.math missing from {:?}",
            out.modules.iter().map(|m| &m.module).collect::<Vec<_>>()
        );
        assert!(out.modules.len() > 5, "expected many modules");
        assert!(!out.types.is_empty(), "expected extern types");
        // Modules are sorted by identity.
        let names: Vec<&String> = out.modules.iter().map(|m| &m.module).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "modules should be sorted by identity");
    }

    #[test]
    fn signatures_render_in_surface_syntax() {
        let out = query(None);
        // Every rendered signature starts `fn <name>(` and names a return after `): `.
        for m in &out.modules {
            for f in &m.functions {
                assert!(
                    f.signature.starts_with(&format!("fn {}(", f.name)),
                    "bad signature: {}",
                    f.signature
                );
                assert!(f.signature.contains("): "), "no return in {}", f.signature);
            }
        }
    }

    #[test]
    fn exact_module_filter_narrows_to_one() {
        let out = query(Some("std.math"));
        assert!(!out.not_found);
        assert_eq!(out.modules.len(), 1);
        assert_eq!(out.modules[0].module, "std.math");
        assert!(out.types.is_empty());
        assert!(
            !out.modules[0].functions.is_empty(),
            "std.math has functions"
        );
    }

    #[test]
    fn bare_module_name_resolves_and_requalifies() {
        let out = query(Some("math"));
        assert_eq!(out.modules.len(), 1);
        // A bare name comes back with its fully-qualified identity.
        assert_eq!(out.modules[0].module, "std.math");
    }

    #[test]
    fn prefix_filter_expands_to_the_family() {
        // `http` is not a module itself, but `std.http.client`/`std.http.server` are — a prefix
        // query surfaces the whole family.
        let out = query(Some("http"));
        assert!(!out.not_found, "http prefix should match the http.* family");
        assert!(
            out.modules.len() >= 2,
            "expected http.client + http.server, got {:?}",
            out.modules.iter().map(|m| &m.module).collect::<Vec<_>>()
        );
        assert!(out.modules.iter().all(|m| m.module.contains("http")));
    }

    #[test]
    fn unknown_filter_falls_back_to_catalog() {
        let out = query(Some("no_such_module_xyz"));
        assert!(out.not_found);
        assert!(!out.modules.is_empty(), "catalog returned as fallback");
    }

    // (SigType/RetTy/ExtFn rendering itself is covered where it lives — `noeta-ext-abi`'s
    // `render_tests` — since the renderer is shared with the LSP's completion detail.)

    #[test]
    fn types_carry_their_namespace() {
        // Extern types are namespace-scoped (`std.id.Uuid`) and imported like user types — the
        // agent needs the namespace to write the `use`.
        let out = query(Some("Uuid"));
        assert_eq!(out.types.len(), 1);
        assert_eq!(out.types[0].name, "Uuid");
        assert!(
            !out.types[0].namespace.is_empty(),
            "Uuid should carry its namespace"
        );
        // The qualified identity resolves too.
        let qualified = format!("{}.Uuid", out.types[0].namespace);
        let out2 = query(Some(&qualified));
        assert_eq!(out2.types.len(), 1, "qualified {qualified} should resolve");
        assert_eq!(out2.types[0].name, "Uuid");
    }

    #[test]
    fn bundles_surface_with_opt_in_syntax() {
        // kernel-methods: `vec` contributes the `Kernels` bundle; agents must see the opt-in
        // (`impl vec.Kernels for T {}`), the structural constraint, and both method receivers.
        let out = query(Some("std.vec"));
        assert_eq!(out.modules.len(), 1);
        let bundles = &out.modules[0].bundles;
        assert!(
            !bundles.is_empty(),
            "std.vec should contribute at least one bundle"
        );
        let kernels = &bundles[0];
        assert!(
            kernels
                .impl_syntax
                .contains(&format!("impl vec.{} for", kernels.name)),
            "opt-in syntax was {:?}",
            kernels.impl_syntax
        );
        assert!(kernels.constraint.contains("@packed"));
        assert!(
            !kernels.element_methods.is_empty() || !kernels.bulk_methods.is_empty(),
            "bundle should list its methods"
        );
    }
}
