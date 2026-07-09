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
use noeta_stdlib::{ExtFn, ExtType, RetTy, SigType};
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
}

/// One extern value type rendered for an agent: its name, the built-in traits it satisfies, and its
/// method signatures. Methods are called `value.method(args)`; the receiver is implicit.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TypeApi {
    /// The type name as it appears in Noeta source, e.g. `Uuid`, `Response`.
    pub name: String,
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
    // 2. An extern type name (`Uuid`, `Response`).
    if let Some(t) = registry::find_type(q) {
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
    ModuleApi {
        module: qname,
        ring: m.ring.map(str::to_string),
        functions,
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
        traits: t.traits.iter().map(|s| s.to_string()).collect(),
        methods,
    }
}

fn render_fn(f: &ExtFn) -> FnSig {
    let params: Vec<String> = f.params.iter().map(render_sig).collect();
    let returns = render_ret(&f.ret, f.params);
    let signature = format!("fn {}({}): {}", f.name, params.join(", "), returns);
    FnSig {
        name: f.name.to_string(),
        signature,
        params,
        returns,
    }
}

/// Render a [`SigType`] into surface Noeta syntax — the form an agent should write.
fn render_sig(sig: &SigType) -> String {
    match sig {
        SigType::Int => "int".to_string(),
        SigType::Float => "float".to_string(),
        SigType::F32 => "f32".to_string(),
        SigType::Bool => "bool".to_string(),
        SigType::String => "string".to_string(),
        SigType::Bytes => "bytes".to_string(),
        SigType::Unit => "()".to_string(),
        SigType::Dyn => "dyn".to_string(),
        SigType::List(t) => format!("List<{}>", render_sig(t)),
        // A nullable value type renders with the surface `?` suffix.
        SigType::Option(t) => format!("{}?", render_sig(t)),
        SigType::Map(k, v) => format!("Map<{}, {}>", render_sig(k), render_sig(v)),
        SigType::Future(t) => format!("Future<{}>", render_sig(t)),
        SigType::Named(n) => (*n).to_string(),
        SigType::Union(ts) => ts.iter().map(render_sig).collect::<Vec<_>>().join("|"),
        // A trailing-optional *parameter*: the argument (and every one after it) may be omitted.
        SigType::Optional(t) => format!("{}?", render_sig(t)),
        SigType::Fn(params, ret) => format!(
            "Fn({}) -> {}",
            params.iter().map(render_sig).collect::<Vec<_>>().join(", "),
            render_sig(ret)
        ),
        SigType::Var(n) => type_var_name(*n),
        SigType::BoundedVar(n, bound) => format!("{}: {}", type_var_name(*n), bound),
        SigType::Generic(name, args) => format!(
            "{}<{}>",
            name,
            args.iter().map(render_sig).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Render a [`RetTy`] into surface syntax, resolving the polymorphic forms against the call's
/// parameter types where they reference them.
fn render_ret(ret: &RetTy, params: &[SigType]) -> String {
    match ret {
        RetTy::Concrete(s) => render_sig(s),
        // Same type as a positional argument (`vec.add(v, w): typeof v`).
        RetTy::SameAsArg(n) => params
            .get(*n)
            .map(render_sig)
            .unwrap_or_else(|| "dyn".to_string()),
        // `int` when every argument is concretely `int`, else `float`.
        RetTy::NumericPreserving => "int|float".to_string(),
        // Named at the call site by a turbofish (`json.parse::<T>(): T`).
        RetTy::TypeArg => "T /* call-site type: name it with ::<T> */".to_string(),
    }
}

/// Signature-level type variable names — `Var(0)` → `T`, `Var(1)` → `U`, … then `T2`, `T3`, … past
/// the single-letter run. Matches the informal `T`/`U` the docs use for generic positions.
fn type_var_name(n: u8) -> String {
    const LETTERS: &[u8] = b"TUVWXYZ";
    let i = n as usize;
    if i < LETTERS.len() {
        (LETTERS[i] as char).to_string()
    } else {
        format!("T{}", i - LETTERS.len() + 2)
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

    #[test]
    fn type_var_names_are_stable() {
        assert_eq!(type_var_name(0), "T");
        assert_eq!(type_var_name(1), "U");
        assert_eq!(type_var_name(6), "Z");
        assert_eq!(type_var_name(7), "T2");
    }
}
