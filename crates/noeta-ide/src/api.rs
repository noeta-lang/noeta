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

use noeta_stdlib::registry;

/// One documented function of a module: its name, its rendered signature, and its prose (empty when
/// undocumented).
#[derive(Debug, Clone)]
pub struct ApiFn {
    pub name: String,
    /// The whole signature, `fn sqrt(float): float` (from [`ExtFn::render`]).
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
    modules_impl(None)
}

/// The modules of just the extension whose [`root`](noeta_stdlib::Extension::root) is `root` (a
/// package's own namespace segment) — for scoping a package's API docs to itself, excluding std.
pub fn modules_of(root: &str) -> Vec<ApiModule> {
    modules_impl(Some(root))
}

fn modules_impl(root: Option<&str>) -> Vec<ApiModule> {
    let mut out: Vec<ApiModule> = Vec::new();
    for ext in registry::extensions() {
        if root.is_some_and(|r| ext.root() != r) {
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
    types_impl(None)
}

/// The extern types of just the extension whose `root` is `root` — the type analogue of
/// [`modules_of`].
pub fn types_of(root: &str) -> Vec<ApiType> {
    types_impl(Some(root))
}

fn types_impl(root: Option<&str>) -> Vec<ApiType> {
    let mut out: Vec<ApiType> = Vec::new();
    for ext in registry::extensions() {
        if root.is_some_and(|r| ext.root() != r) {
            continue;
        }
        for t in ext.types() {
            let mut methods: Vec<ApiFn> = t
                .methods
                .iter()
                .chain(t.ctx_methods.iter())
                .map(|f| ApiFn {
                    name: f.name.to_string(),
                    signature: f.render(),
                    doc: t
                        .docs
                        .iter()
                        .find(|(n, _)| *n == f.name)
                        .map(|(_, d)| d.to_string())
                        .unwrap_or_default(),
                })
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

/// The prose for `name` in module `m`, or empty. Keyed by name so one table serves both plain and
/// higher-order functions.
fn doc_of(m: &noeta_stdlib::ExtModule, name: &str) -> String {
    m.docs
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, d)| d.to_string())
        .unwrap_or_default()
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
        assert_eq!(sqrt.signature, "fn sqrt(float): float");
        assert!(sqrt.doc.contains("square root"));
        let hyp = math.functions.iter().find(|f| f.name == "hypot").unwrap();
        assert!(hyp.doc.contains("Euclidean"));
        assert_eq!(hyp.signature, "fn hypot(float, float): float");
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
        }
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
