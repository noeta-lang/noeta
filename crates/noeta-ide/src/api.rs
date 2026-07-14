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

/// Every module the registry knows, qualified and sorted (`std.crypto`, `std.http.client`,
/// `std.math`, …), each with its functions (plain + higher-order) sorted by name. Reads the
/// process-global registry via the stdlib facade, which lazily seeds the built-in `std` units.
pub fn modules() -> Vec<ApiModule> {
    let mut out: Vec<ApiModule> = Vec::new();
    for ext in registry::extensions() {
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
        // The Arc-2 pilot prose is attached.
        assert!(sqrt.doc.contains("square root"));
        // An undocumented function still appears, signature-only.
        let sinh = math.functions.iter().find(|f| f.name == "sinh").unwrap();
        assert!(sinh.doc.is_empty());
        assert_eq!(sinh.signature, "fn sinh(float): float");
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
