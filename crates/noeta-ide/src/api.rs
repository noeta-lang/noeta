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
        for t in ext.types() {
            if !under(t.namespace, root) {
                out.push(format!(
                    "extern type `{}` is namespaced `{}`, not under its extension's root `{root}` \
                     — set `namespace: \"{root}\"` on it (the field defaults to `std`)",
                    t.name, t.namespace
                ));
            }
        }
    }
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
        }
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
