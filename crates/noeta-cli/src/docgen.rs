//! The documentation **generator** (`noeta doc --out <DIR>`): turn a package's `@doc` prose and
//! public API into a self-contained, deterministic documentation artifact — a machine-readable
//! `docs.json` plus a browsable Markdown tree. Designed registry-first: the JSON manifest is the
//! canonical form (schema-versioned, keyed by the package's `[package]` identity and version, no
//! timestamps or absolute paths), so a published package's docs can ride along to the registry
//! and be rendered server-side; the Markdown is a faithful local rendering of the same data.
//!
//! Everything works from a **bare parse** — no type-checking — so docs generate from
//! work-in-progress code, exactly like `noeta doc`'s extraction mode. Scope is the package's own
//! modules (the entry and its sibling `.noe` files), never its dependencies: each package
//! documents itself. A module that declares a `namespace` is a package module and documents its
//! `pub` API; a bare entry script documents every top-level declaration.

use std::path::Path;

use noeta_ast::{ClassDecl, EnumDecl, FnDecl, Program, Stmt, StructDecl, TraitDecl};
use noeta_check::{DocTarget, dedent_doc, resolve_docs};
use noeta_ide::symbols::render_type_ref;
use noeta_span::{Source, SourceId};

/// What [`generate`] produced, for the command's summary line.
pub struct Generated {
    pub modules: usize,
    pub decls: usize,
    /// Modules skipped because they did not parse (documented-from-WIP still needs a parse).
    pub skipped: Vec<String>,
}

/// The docs.json schema version. Bump on any breaking shape change — the registry dispatches on
/// it.
///
/// A **new item `kind` is not one.** The hosted renderer requires an item to carry `name` and
/// `kind` and then prints `kind` as a plain label (`noeta-registry`'s `renderModule` /
/// `renderDecl`, one generic `.kind` CSS rule), so `impl` and `directive` render on an unchanged
/// Worker. Bumping would have been actively harmful: [`render_json_to`] refuses an artifact whose
/// schema is not this constant, so a bump makes every already-published package's stored docs
/// unreadable by `noeta doc --package` — a real regression bought for nothing. Widen
/// [`render_json_to`]'s kind whitelist instead.
const SCHEMA: u32 = 1;

/// Generate the documentation artifact for the package containing `entry` into `out`.
pub fn generate(entry: &Path, out: &Path) -> Result<Generated, String> {
    let workspace =
        noeta_loader::read_workspace(entry, noeta_pm::sources::package_root(entry).as_ref())
            .map_err(|e| format!("cannot read {}: {e}", entry.display()))?;
    let package = package_meta(entry.parent().unwrap_or_else(|| Path::new(".")));

    // Parse each module independently: docs and signatures are per-file facts (adjacency never
    // crosses a file), and a broken sibling must not take the whole artifact down.
    let mut modules: Vec<ModuleDocs> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for source in std::iter::once(&workspace.entry).chain(workspace.modules.iter()) {
        match module_docs(source) {
            Some(m) => modules.push(m),
            None => skipped.push(basename(source.name()).to_string()),
        }
    }

    std::fs::create_dir_all(out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    let json = docs_json(&package, &modules);
    std::fs::write(out.join("docs.json"), format!("{json:#}\n")).map_err(|e| e.to_string())?;
    std::fs::write(out.join("index.md"), index_markdown(&package, &modules))
        .map_err(|e| e.to_string())?;
    for module in &modules {
        std::fs::write(
            out.join(format!("{}.md", module.slug)),
            module_markdown(module),
        )
        .map_err(|e| e.to_string())?;
    }

    Ok(Generated {
        modules: modules.len(),
        decls: modules.iter().map(|m| m.decl_count()).sum(),
        skipped,
    })
}

/// Build the documentation artifact for the **package** rooted at `dir` — every `.noe` file,
/// sorted by name (no entry-file concept: a package's modules are peers) — returning the
/// `docs.json` text. The `noeta publish` producer: nothing is written to disk, and the sorted
/// order keeps the artifact deterministic.
pub fn package_docs_json(dir: &Path) -> Result<(String, Generated), String> {
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "noe"))
        .collect();
    paths.sort();
    let mut modules = Vec::new();
    let mut skipped = Vec::new();
    for path in &paths {
        let Ok(text) = std::fs::read_to_string(path) else {
            skipped.push(path.display().to_string());
            continue;
        };
        let source = Source::new(SourceId::FIRST, path.display().to_string(), text);
        match module_docs(&source) {
            Some(m) => modules.push(m),
            None => skipped.push(basename(source.name()).to_string()),
        }
    }
    let json = docs_json(&package_meta(dir), &modules);
    Ok((
        format!("{json:#}\n"),
        Generated {
            modules: modules.len(),
            decls: modules.iter().map(|m| m.decl_count()).sum(),
            skipped,
        },
    ))
}

/// How `noeta doc --api` scopes the registry surface it documents.
#[derive(Debug, Clone, Copy)]
pub enum ApiScope<'a> {
    /// The whole registry (the default `--api`).
    All,
    /// Only the extensions rooted at this namespace (`--root <ns>`) — an explicit user filter.
    Root(&'a str),
    /// Every registered extension EXCEPT the toolchain's builtin units (`--non-builtin`) — the
    /// publish docs path: in a package's composed toolchain this is exactly the package's own
    /// surface, whatever namespace root(s) it declares (`root()` may diverge from the package's
    /// manifest segment, e.g. para/p2p rooting at `para`).
    NonBuiltin,
}

/// The unit names of the extensions the toolchain itself installs — [`run_cli`](crate::run_cli)'s
/// exact builtin set, derived from the same statics it assembles (the `std` family from
/// `std_units()`), never a parallel string list. Tier-body formatters (`html`/`css`) are no longer
/// among the toolchain's own units — they arrive as a `package.dev-native` dependency, composed
/// formatter-only. The complement of this set in any composed registry is the composition's own
/// package surface.
pub fn builtin_extension_names() -> Vec<&'static str> {
    use noeta_stdlib::Extension;
    let mut names: Vec<&'static str> = noeta_stdlib::registry::std_units()
        .iter()
        .map(|e| e.name())
        .collect();
    // The CLI-layer unit that registers std's native dev-tier runners (Part B). It attaches to the
    // `std` root but ships no documented surface, so the publish lint must count it among the
    // toolchain's own units rather than flagging it as a package squatting `std`.
    names.push(crate::tier_runner::STD_TIER_RUNNERS_UNIT.name());
    names
}

/// The namespace roots the toolchain owns — the builtin units' own roots plus the reserved
/// built-in scopes (`std`/`noeta`/`core`) — which the publish lint refuses a package extension to
/// claim. Assembly-time validation cannot catch this squat (a fresh module under `std.` collides
/// with nothing), so the lint is where it surfaces.
pub fn toolchain_roots() -> Vec<&'static str> {
    let mut roots: Vec<&'static str> = noeta_stdlib::registry::std_units()
        .iter()
        .map(|e| e.root())
        .collect();
    roots.extend_from_slice(noeta_pm::reserved::builtin_scopes());
    roots.sort_unstable();
    roots.dedup();
    roots
}

/// Build the **API-reference** `docs.json` (docs-browser Arc 2) from the intrinsic registry — the
/// stdlib and any composed native modules — rather than from `.noe` source. One module entry per
/// registry module (`std.math`, `std.http.client`, …), each function an `fn` item carrying its
/// rendered signature and any registered doc prose. Same schema-1 shape as [`generate`], so it
/// rides to the registry and renders on the hosted docs page identically. `package` names the
/// artifact (e.g. the toolchain's `std`), or `None` for a generic title. `scope` selects which
/// extensions' surface is documented (see [`ApiScope`]).
///
/// A registry unit registers more than functions. Its **nominal** declarations — native traits,
/// enums, classes and structs ([`noeta_ide::api::decls`]) — land as `trait`/`enum`/`struct`/`class`
/// items on the page of the module they are namespaced under, exactly where their `.noe` twins sit
/// in [`generate`]'s output. Until they did, publishing para/p2p emitted a reference with no mention
/// of `Mergeable` or `Syncable`, the two traits the package exists to have you implement.
pub fn registry_docs_json(
    package: Option<(String, String)>,
    scope: ApiScope<'_>,
) -> (String, Generated) {
    let fn_item = |f: noeta_ide::api::ApiFn| {
        Item::Decl(DeclDocs {
            kind: "fn",
            name: f.name,
            signature: f.signature,
            doc: (!f.doc.is_empty()).then_some(f.doc),
            public: true,
        })
    };
    let (api_modules, api_types, api_decls) = match scope {
        ApiScope::All => (
            noeta_ide::api::modules(),
            noeta_ide::api::types(),
            [noeta_ide::api::decls(), noeta_ide::api::directives()].concat(),
        ),
        ApiScope::Root(r) => (
            noeta_ide::api::modules_of(r),
            noeta_ide::api::types_of(r),
            [
                noeta_ide::api::decls_of(r),
                noeta_ide::api::directives_of(r),
            ]
            .concat(),
        ),
        ApiScope::NonBuiltin => {
            let builtin = builtin_extension_names();
            (
                noeta_ide::api::modules_excluding(&builtin),
                noeta_ide::api::types_excluding(&builtin),
                [
                    noeta_ide::api::decls_excluding(&builtin),
                    noeta_ide::api::directives_excluding(&builtin),
                ]
                .concat(),
            )
        }
    };
    // One `docs.json` module per registry module and per extern type — both are qualified surfaces
    // of functions/methods, so they render uniformly by-module on the hosted page.
    let mut modules: Vec<ModuleDocs> = api_modules
        .into_iter()
        .map(|m| ModuleDocs {
            file: String::new(), // native: no source file
            slug: m.qualified.replace('.', "-"),
            namespace: Some(m.qualified),
            doc: None,
            items: m.functions.into_iter().map(fn_item).collect(),
        })
        .collect();
    // Nominal declarations join their namespace's module page. A namespace with no *module* of its
    // own still gets a page — an extension may declare a trait under a namespace it registers no
    // functions in, and dropping it because there was nowhere to put it is the hole this closes.
    for d in api_decls {
        let page = match modules
            .iter()
            .position(|m| m.namespace.as_deref() == Some(&d.module))
        {
            Some(at) => &mut modules[at],
            None => {
                modules.push(ModuleDocs {
                    file: String::new(),
                    slug: d.module.replace('.', "-"),
                    namespace: Some(d.module.clone()),
                    doc: None,
                    items: Vec::new(),
                });
                modules.last_mut().expect("just pushed")
            }
        };
        page.items.push(Item::Decl(DeclDocs {
            kind: d.kind,
            name: d.name,
            signature: d.signature,
            doc: (!d.doc.is_empty()).then_some(d.doc),
            public: true,
        }));
    }
    modules.extend(api_types.into_iter().map(|t| ModuleDocs {
        file: String::new(),
        slug: t.qualified.replace('.', "-"),
        namespace: Some(t.qualified),
        doc: None,
        items: t.methods.into_iter().map(fn_item).collect(),
    }));
    // Pages are emitted in qualified order; a page created for a declaration-only namespace above
    // would otherwise land after every module regardless of its name.
    modules.sort_by(|a, b| a.namespace.cmp(&b.namespace));
    let json = docs_json(&package, &modules);
    let generated = Generated {
        modules: modules.len(),
        decls: modules.iter().map(|m| m.decl_count()).sum(),
        skipped: Vec::new(),
    };
    (format!("{json:#}\n"), generated)
}

/// Combine a native package's registry-derived API `docs.json` (`api_json`, the primary surface)
/// with any `.noe`-source docs it also ships (`noe_json`) and stamp the package identity, for
/// `noeta publish` to upload. Modules are concatenated (API first); a `.noe` module whose namespace
/// already appears in the API surface is dropped (the compiled surface wins). Malformed inputs are
/// skipped rather than fatal — docs are advisory.
pub fn finalize_native_docs(
    api_json: &str,
    noe_json: Option<&str>,
    name: &str,
    version: &str,
) -> String {
    let mut doc: serde_json::Value =
        serde_json::from_str(api_json).unwrap_or_else(|_| serde_json::json!({ "schema": SCHEMA }));
    let mut modules: Vec<serde_json::Value> = doc
        .get("modules")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();
    let mut seen: std::collections::HashSet<String> = modules
        .iter()
        .filter_map(|m| {
            m.get("namespace")
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
        .collect();
    if let Some(noe) = noe_json
        && let Ok(noe_doc) = serde_json::from_str::<serde_json::Value>(noe)
        && let Some(noe_mods) = noe_doc.get("modules").and_then(|m| m.as_array())
    {
        for m in noe_mods {
            let ns = m.get("namespace").and_then(|n| n.as_str()).unwrap_or("");
            if ns.is_empty() || seen.insert(ns.to_string()) {
                modules.push(m.clone());
            }
        }
    }
    doc["schema"] = serde_json::json!(SCHEMA);
    doc["package"] = serde_json::json!({ "name": name, "version": version });
    doc["modules"] = serde_json::json!(modules);
    format!("{doc:#}\n")
}

/// Render the Markdown tree from a stored `docs.json` (the registry-fetch path: `noeta doc
/// --package … --out DIR`) into `out`, alongside a copy of the artifact itself. The inverse of
/// [`generate`]'s emit step, working purely from the schema — no source needed.
pub fn render_json_to(out: &Path, docs_json_text: &str) -> Result<Generated, String> {
    let doc: serde_json::Value =
        serde_json::from_str(docs_json_text).map_err(|e| format!("corrupt docs.json: {e}"))?;
    let schema = doc["schema"].as_u64().unwrap_or(0);
    if schema != SCHEMA as u64 {
        return Err(format!(
            "docs.json schema {schema} is not the supported {SCHEMA}"
        ));
    }
    let package = doc["package"].as_object().and_then(|p| {
        Some((
            p.get("name")?.as_str()?.to_string(),
            p.get("version")?.as_str()?.to_string(),
        ))
    });
    let mut modules = Vec::new();
    for m in doc["modules"].as_array().into_iter().flatten() {
        let file = m["file"].as_str().unwrap_or("module.noe").to_string();
        let items = m["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                if let Some(text) = item.get("section").and_then(|s| s.as_str()) {
                    return Some(Item::Section(text.to_string()));
                }
                Some(Item::Decl(DeclDocs {
                    kind: match item.get("kind")?.as_str()? {
                        "fn" => "fn",
                        "struct" => "struct",
                        "class" => "class",
                        "enum" => "enum",
                        "trait" => "trait",
                        // A standalone `impl Trait for T` whose target is declared elsewhere.
                        "impl" => "impl",
                        // An extension's `@`-directive.
                        "directive" => "directive",
                        _ => return None,
                    },
                    name: item.get("name")?.as_str()?.to_string(),
                    signature: item.get("signature")?.as_str()?.to_string(),
                    doc: item.get("doc").and_then(|d| d.as_str()).map(str::to_string),
                    public: item.get("public").and_then(|p| p.as_bool()).unwrap_or(true),
                }))
            })
            .collect();
        let namespace = m["namespace"].as_str().map(str::to_string);
        // Prefer the source-file stem; a native module (registry API) has no file, so fall back to
        // its namespace (`std.math` → `std-math`) for a unique, readable page name.
        let slug = match (file.trim_end_matches(".noe"), &namespace) {
            ("", Some(ns)) => ns.replace('.', "-"),
            (stem, _) if !stem.is_empty() => stem.to_string(),
            _ => "module".to_string(),
        };
        modules.push(ModuleDocs {
            slug,
            file,
            namespace,
            doc: m["doc"].as_str().map(str::to_string),
            items,
        });
    }
    std::fs::create_dir_all(out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    std::fs::write(out.join("docs.json"), docs_json_text).map_err(|e| e.to_string())?;
    std::fs::write(out.join("index.md"), index_markdown(&package, &modules))
        .map_err(|e| e.to_string())?;
    for module in &modules {
        std::fs::write(
            out.join(format!("{}.md", module.slug)),
            module_markdown(module),
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(Generated {
        modules: modules.len(),
        decls: modules.iter().map(|m| m.decl_count()).sum(),
        skipped: Vec::new(),
    })
}

/// The `[package]` identity of the manifest governing `dir`, if any.
fn package_meta(dir: &Path) -> Option<(String, String)> {
    let path = noeta_pm::manifest::find(dir)?;
    let text = std::fs::read_to_string(&path).ok()?;
    let manifest = noeta_pm::manifest::Manifest::parse(&text).ok()?;
    let pkg = manifest.package()?;
    Some((
        format!("{}/{}", pkg.name.company, pkg.name.package),
        pkg.version.to_string(),
    ))
}

/// One documented module: its identity, module-level prose, and its items in **source order**
/// (sections woven between declarations, preserving the authored narrative).
struct ModuleDocs {
    /// The source file's basename (`tiers.noe`).
    file: String,
    /// The Markdown file stem (`tiers`) — unique per directory by construction.
    slug: String,
    /// The declared `namespace`, if any (a package module); a bare script has none.
    namespace: Option<String>,
    /// The module doc (the first non-attached `@doc` block).
    doc: Option<String>,
    /// Sections and documented/public declarations, in source order.
    items: Vec<Item>,
}

enum Item {
    /// A free-floating `@doc` section.
    Section(String),
    Decl(DeclDocs),
}

/// One declaration's documentation entry.
struct DeclDocs {
    kind: &'static str,
    name: String,
    /// The rendered signature (directives included), ready for a code block.
    signature: String,
    doc: Option<String>,
    public: bool,
}

impl ModuleDocs {
    fn decl_count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i, Item::Decl(_)))
            .count()
    }
}

/// Parse one source and assemble its [`ModuleDocs`]; `None` when it does not parse.
fn module_docs(source: &Source) -> Option<ModuleDocs> {
    // Docs are per-file: re-key the source to FIRST so spans and doc resolution stay local.
    let local = Source::new(SourceId::FIRST, source.name(), source.text().to_string());
    let lexed = noeta_lexer::lex(&local);
    let parsed = noeta_parser::parse(&local, &lexed.tokens);
    // Only a real lex/parse **error** means there is no tree to document. An advisory diagnostic
    // still leaves a well-formed program, and dropping a module's whole API reference over a lint
    // would be a silent documentation hole.
    if noeta_diagnostics::has_errors(lexed.diagnostics.iter().chain(parsed.diagnostics.iter())) {
        return None;
    }
    let program: &Program = &parsed.program;

    let namespace = program.stmts.iter().find_map(|s| match s {
        Stmt::Namespace { path, .. } => Some(path.join(".")),
        _ => None,
    });
    // A package module documents its `pub` API; a bare script documents everything.
    let public_only = namespace.is_some();

    // Adjacency-resolved docs: the module doc, the sections, and each decl's text keyed by the
    // declaration's **name span**, not its name. `resolve_docs` reports a method's prose under the
    // method's bare name, so a name key collides a `Point.describe` with a top-level `describe` and
    // hands one of them the other's paragraph. A name span is unique in the file by construction.
    let mut module_doc = None;
    let mut decl_docs: Docs = std::collections::HashMap::new();
    let mut sections: Vec<(u32, String)> = Vec::new();
    for doc in resolve_docs(program) {
        let text = dedent_doc(&doc.text).trim().to_string();
        match doc.target {
            DocTarget::Module => module_doc = Some(text),
            DocTarget::Section => sections.push((doc.span.start, text)),
            DocTarget::Decl { name_span, .. } => {
                decl_docs.insert(name_span, text);
            }
        }
    }

    // Items in source order: sections at their span, declarations at theirs.
    let mut items: Vec<(u32, Item)> = sections
        .into_iter()
        .map(|(at, text)| (at, Item::Section(text)))
        .collect();
    // A **standalone** `impl Trait for T { … }` is not part of any declaration's AST node (unlike an
    // in-body `impl`, which the parser flattens into the type's own `methods`). Collect them first
    // so a target declared in this file can absorb its own; the leftovers — impls of a type declared
    // in a sibling module, which the package orphan rule permits — become items of their own rather
    // than vanishing.
    let mut standalone: Vec<&noeta_ast::ImplDecl> = program
        .stmts
        .iter()
        .filter_map(|s| match s {
            Stmt::Impl(i) => Some(i),
            _ => None,
        })
        .collect();
    let declared: std::collections::HashSet<&str> = program
        .stmts
        .iter()
        .filter_map(|s| match s {
            Stmt::Struct(d) => Some(d.name.as_str()),
            Stmt::Class(d) => Some(d.name.as_str()),
            Stmt::Enum(d) => Some(d.name.as_str()),
            _ => None,
        })
        .collect();
    for stmt in &program.stmts {
        let (at, decl) = match stmt {
            Stmt::Fn(f) => (f.span.start, fn_docs(f, &decl_docs)),
            Stmt::Struct(s) => (
                s.span.start,
                struct_docs(s, &decl_docs, &impls_for(s.name.as_str(), &standalone)),
            ),
            Stmt::Class(c) => (
                c.span.start,
                class_docs(c, &decl_docs, &impls_for(c.name.as_str(), &standalone)),
            ),
            Stmt::Enum(e) => (
                e.span.start,
                enum_docs(e, &decl_docs, &impls_for(e.name.as_str(), &standalone)),
            ),
            Stmt::Trait(t) => (t.span.start, trait_docs(t, &decl_docs)),
            _ => continue,
        };
        if public_only && !decl.public {
            continue;
        }
        items.push((at, Item::Decl(decl)));
    }
    standalone.retain(|i| !declared.contains(i.target.as_str()));
    for i in standalone {
        items.push((i.span.start, Item::Decl(impl_docs(i, &decl_docs))));
    }
    items.sort_by_key(|(at, _)| *at);

    let file = basename(source.name()).to_string();
    let slug = file.trim_end_matches(".noe").to_string();
    Some(ModuleDocs {
        file,
        slug,
        namespace,
        doc: module_doc,
        items: items.into_iter().map(|(_, i)| i).collect(),
    })
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Declaration prose, keyed by the declaration's **name span** — unique in the file, unlike the
/// bare name a method and a top-level function may share.
type Docs = std::collections::HashMap<noeta_span::Span, String>;

/// The standalone impls in `from` whose target is the type named `name`.
fn impls_for<'a>(name: &str, from: &[&'a noeta_ast::ImplDecl]) -> Vec<&'a noeta_ast::ImplDecl> {
    from.iter()
        .filter(|i| i.target.as_str() == name)
        .copied()
        .collect()
}

/// The generic parameter list (`<T, K: Comparable>`), or empty for a non-generic declaration.
fn type_params(params: &[noeta_ast::TypeParam]) -> String {
    if params.is_empty() {
        return String::new();
    }
    let rendered: Vec<String> = params
        .iter()
        .map(|p| {
            if p.bounds.is_empty() {
                p.name.clone()
            } else {
                let bounds: Vec<String> = p.bounds.iter().map(|b| b.name.to_string()).collect();
                format!("{}: {}", p.name, bounds.join(" + "))
            }
        })
        .collect();
    format!("<{}>", rendered.join(", "))
}

/// One function/method signature, without the leading `pub`/`async` a top-level declaration adds.
fn fn_signature(f: &FnDecl) -> String {
    let params: Vec<String> = f
        .params
        .iter()
        .map(|p| match &p.ty {
            Some(ty) => format!("{}: {}", p.name, render_type_ref(ty)),
            None => p.name.clone(),
        })
        .collect();
    let mut sig = String::new();
    if f.is_async {
        sig.push_str("async ");
    }
    sig.push_str(&format!(
        "fn {}{}({})",
        f.name,
        type_params(&f.type_params),
        params.join(", ")
    ));
    if let Some(ret) = &f.ret {
        sig.push_str(&format!(": {}", render_type_ref(ret)));
    }
    sig
}

fn fn_docs(f: &FnDecl, docs: &Docs) -> DeclDocs {
    let mut sig = String::new();
    if let Some(tier) = &f.tier {
        match &tier.config {
            Some((cfg, _)) => sig.push_str(&format!("@tier({}, config: {cfg})\n", tier.name)),
            None => sig.push_str(&format!("@tier({})\n", tier.name)),
        }
    }
    if f.is_public {
        sig.push_str("pub ");
    }
    sig.push_str(&fn_signature(f));
    DeclDocs {
        kind: "fn",
        name: f.name.to_string(),
        signature: sig,
        doc: docs.get(&f.name_span).cloned(),
        public: f.is_public,
    }
}

/// Render a field list, one field per line.
fn fields_block(fields: &[noeta_ast::FieldDecl]) -> String {
    let mut out = String::new();
    for field in fields {
        let ty = field
            .ty
            .as_ref()
            .map(render_type_ref)
            .unwrap_or_else(|| "_".to_string());
        let vis = if field.is_public { "pub " } else { "" };
        let mutability = if field.mut_field { "mut " } else { "" };
        out.push_str(&format!("    {vis}{mutability}{}: {ty}\n", field.name));
    }
    out
}

/// Render a type's methods, one signature per line — the half of a type's API that lived only in
/// the AST. A type's `methods` already holds the flattened copy of every in-body `impl Trait { … }`
/// method (the parser puts them there so dispatch resolves them), so this one list is the whole
/// callable surface and walking `impls` as well would print each of those twice.
fn methods_block(methods: &[FnDecl]) -> String {
    let mut out = String::new();
    for m in methods {
        let vis = if m.is_public { "pub " } else { "" };
        out.push_str(&format!("    {vis}{}\n", fn_signature(m)));
    }
    out
}

/// The trait names a type conforms to: its in-body `impl Trait { … }` blocks plus any **standalone**
/// `impl Trait for T { … }` found in the same file. Sorted and deduped — the same trait reached by
/// two routes is one conformance.
fn conformances(
    in_body: &[noeta_ast::ImplBlock],
    standalone: &[&noeta_ast::ImplDecl],
) -> Vec<String> {
    let mut names: Vec<String> = in_body
        .iter()
        .map(|i| i.trait_name.to_string())
        .chain(standalone.iter().map(|i| i.trait_name.to_string()))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Fold a declaration's own prose, its trait conformances, and its documented members into one
/// markdown body — the `.noe` twin of `noeta_ide::api::with_members`, and deliberately the same
/// shape so a package's own types read like the native ones beside them.
///
/// A member earns a subsection only when it is documented: the signature above already lists every
/// one, so an empty heading would be noise.
fn with_members(
    doc: Option<&String>,
    conformances: &[String],
    members: &[&FnDecl],
    docs: &Docs,
) -> Option<String> {
    let mut out = doc.cloned().unwrap_or_default();
    if !conformances.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        let list: Vec<String> = conformances.iter().map(|t| format!("`{t}`")).collect();
        out.push_str(&format!("**Implements:** {}", list.join(", ")));
    }
    for m in members {
        let Some(prose) = docs.get(&m.name_span) else {
            continue;
        };
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("#### `{}`\n\n{}", fn_signature(m), prose.trim()));
    }
    (!out.is_empty()).then_some(out)
}

fn struct_docs(s: &StructDecl, docs: &Docs, standalone: &[&noeta_ast::ImplDecl]) -> DeclDocs {
    let mut sig = String::new();
    if s.decorators.attribute.is_some() {
        sig.push_str("@attribute\n");
    }
    if s.is_public {
        sig.push_str("pub ");
    }
    sig.push_str(&format!(
        "struct {}{} {{\n{}{}{}}}",
        s.name,
        type_params(&s.type_params),
        fields_block(&s.fields),
        methods_block(&s.methods),
        standalone_block(standalone)
    ));
    let members: Vec<&FnDecl> = s
        .methods
        .iter()
        .chain(standalone_methods(standalone))
        .collect();
    DeclDocs {
        kind: "struct",
        name: s.name.to_string(),
        signature: sig,
        doc: with_members(
            docs.get(&s.name_span),
            &conformances(&s.impls, standalone),
            &members,
            docs,
        ),
        public: s.is_public,
    }
}

fn class_docs(c: &ClassDecl, docs: &Docs, standalone: &[&noeta_ast::ImplDecl]) -> DeclDocs {
    let mut sig = String::new();
    if c.is_public {
        sig.push_str("pub ");
    }
    sig.push_str(&format!(
        "class {}{} {{\n{}{}{}}}",
        c.name,
        type_params(&c.type_params),
        fields_block(&c.fields),
        methods_block(&c.methods),
        standalone_block(standalone)
    ));
    let members: Vec<&FnDecl> = c
        .methods
        .iter()
        .chain(standalone_methods(standalone))
        .collect();
    DeclDocs {
        kind: "class",
        name: c.name.to_string(),
        signature: sig,
        doc: with_members(
            docs.get(&c.name_span),
            &conformances(&c.impls, standalone),
            &members,
            docs,
        ),
        public: c.is_public,
    }
}

/// The methods carried by a set of standalone impls — the one method carrier a type's own `methods`
/// does not already contain.
fn standalone_methods<'a>(
    impls: &'a [&'a noeta_ast::ImplDecl],
) -> impl Iterator<Item = &'a FnDecl> {
    impls.iter().flat_map(|i| i.methods.iter())
}

/// A type's standalone-impl methods, rendered under a `// impl Trait` marker per impl. They belong
/// in the declaration — they are callable on the type like any other method — but the marker keeps
/// the reader's mental model right: these arrived from a separate `impl` block, and an in-body one's
/// methods (already flattened into the type's own `methods`) are printed without it.
fn standalone_block(impls: &[&noeta_ast::ImplDecl]) -> String {
    let mut out = String::new();
    for i in impls {
        if i.methods.is_empty() {
            continue;
        }
        out.push_str(&format!("\n    // impl {}\n", i.trait_name));
        out.push_str(&methods_block(&i.methods));
    }
    out
}

/// A backed variant's constant (`= "pending"`), for the literal forms an enum backing permits.
fn backed_value(expr: &noeta_ast::Expr) -> String {
    use noeta_ast::Expr;
    match expr {
        Expr::Str { value, .. } => format!(" = \"{value}\""),
        Expr::Int { value, .. } => format!(" = {value}"),
        Expr::Float { value, .. } => format!(" = {value}"),
        Expr::Bool { value, .. } => format!(" = {value}"),
        // A backing must be a literal, so anything else is not one — say nothing rather than guess.
        _ => String::new(),
    }
}

fn enum_docs(e: &EnumDecl, docs: &Docs, standalone: &[&noeta_ast::ImplDecl]) -> DeclDocs {
    let mut sig = String::new();
    if e.is_public {
        sig.push_str("pub ");
    }
    // The backing is what makes `.value()` exist and typed — dropping it turned every backed enum
    // in the reference into a plain one.
    let backing = e
        .backing
        .as_ref()
        .map(|b| format!(": {}", render_type_ref(b)))
        .unwrap_or_default();
    sig.push_str(&format!("enum {}{backing} {{\n", e.name));
    for v in &e.variants {
        let payload = if v.fields.is_empty() {
            String::new()
        } else {
            let fields: Vec<String> = v
                .fields
                .iter()
                .map(|p| match &p.ty {
                    Some(ty) => format!("{}: {}", p.name, render_type_ref(ty)),
                    None => p.name.clone(),
                })
                .collect();
            format!("({})", fields.join(", "))
        };
        let value = v
            .backed_value
            .as_ref()
            .map(backed_value)
            .unwrap_or_default();
        sig.push_str(&format!("    {}{payload}{value}\n", v.name));
    }
    sig.push_str(&methods_block(&e.methods));
    sig.push_str(&standalone_block(standalone));
    sig.push('}');
    let members: Vec<&FnDecl> = e
        .methods
        .iter()
        .chain(standalone_methods(standalone))
        .collect();
    DeclDocs {
        kind: "enum",
        name: e.name.to_string(),
        signature: sig,
        doc: with_members(
            docs.get(&e.name_span),
            &conformances(&e.impls, standalone),
            &members,
            docs,
        ),
        public: e.is_public,
    }
}

fn trait_docs(t: &TraitDecl, docs: &Docs) -> DeclDocs {
    let mut sig = String::new();
    for a in &t.decorators.attrs {
        sig.push_str(&format!("#[{}]\n", a.name));
    }
    if t.is_public {
        sig.push_str("pub ");
    }
    sig.push_str(&format!(
        "trait {}{} {{\n",
        t.name,
        type_params(&t.type_params)
    ));
    for a in &t.assoc_types {
        match &a.default {
            Some(d) => sig.push_str(&format!("    type {} = {}\n", a.name, render_type_ref(d))),
            None => sig.push_str(&format!("    type {}\n", a.name)),
        }
    }
    for m in &t.methods {
        let params: Vec<String> = m
            .sig
            .params
            .iter()
            .map(|p| match &p.ty {
                Some(ty) => format!("{}: {}", p.name, render_type_ref(ty)),
                None => p.name.clone(),
            })
            .collect();
        sig.push_str(&format!(
            "    fn {}{}({})",
            m.sig.name,
            type_params(&m.sig.type_params),
            params.join(", ")
        ));
        if let Some(ret) = &m.sig.ret {
            sig.push_str(&format!(": {}", render_type_ref(ret)));
        }
        // A default method shows a `{ … }` marker; a required one is bodiless.
        sig.push_str(if m.has_default { " { … }\n" } else { "\n" });
    }
    sig.push('}');
    DeclDocs {
        kind: "trait",
        name: t.name.to_string(),
        signature: sig,
        doc: docs.get(&t.name_span).cloned(),
        public: t.is_public,
    }
}

/// A **standalone** `impl Trait for T { … }` whose target is not declared in this file — the
/// cross-module case the package orphan rule permits (the rule's boundary is the package, not the
/// file). It has no declaration to fold into here, so it documents itself.
fn impl_docs(i: &noeta_ast::ImplDecl, docs: &Docs) -> DeclDocs {
    let sig = format!(
        "impl {} for {} {{\n{}}}",
        i.trait_name,
        i.target,
        methods_block(&i.methods)
    );
    let members: Vec<&FnDecl> = i.methods.iter().collect();
    DeclDocs {
        kind: "impl",
        name: format!("{} for {}", i.trait_name, i.target),
        signature: sig,
        doc: with_members(None, &[], &members, docs),
        // A standalone impl has no visibility of its own; it is as public as the pair it joins, and
        // a package module that writes one is stating a fact about its public surface.
        public: true,
    }
}

/// The canonical machine-readable artifact — what a registry indexes and renders.
fn docs_json(package: &Option<(String, String)>, modules: &[ModuleDocs]) -> serde_json::Value {
    serde_json::json!({
        "schema": SCHEMA,
        "package": package.as_ref().map(|(name, version)| serde_json::json!({
            "name": name,
            "version": version,
        })),
        "modules": modules.iter().map(|m| serde_json::json!({
            "file": m.file,
            "namespace": m.namespace,
            "doc": m.doc,
            "items": m.items.iter().map(|item| match item {
                Item::Section(text) => serde_json::json!({ "section": text }),
                Item::Decl(d) => serde_json::json!({
                    "kind": d.kind,
                    "name": d.name,
                    "signature": d.signature,
                    "doc": d.doc,
                    "public": d.public,
                }),
            }).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// The first line of a Markdown body, for index summaries.
fn first_line(text: &str) -> &str {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("")
}

fn index_markdown(package: &Option<(String, String)>, modules: &[ModuleDocs]) -> String {
    let mut out = String::new();
    match package {
        Some((name, version)) => out.push_str(&format!("# {name} `{version}`\n\n")),
        None => out.push_str("# Documentation\n\n"),
    }
    if let Some(first) = modules.first()
        && let Some(doc) = &first.doc
    {
        out.push_str(doc);
        out.push_str("\n\n");
    }
    out.push_str("## Modules\n\n");
    for m in modules {
        let title = m.namespace.as_deref().unwrap_or(&m.file);
        out.push_str(&format!(
            "- [{title}]({}.md) — {}\n",
            m.slug,
            m.doc.as_deref().map(first_line).unwrap_or(""),
        ));
    }
    out
}

fn module_markdown(m: &ModuleDocs) -> String {
    let mut out = String::new();
    match &m.namespace {
        // A native (registry API) module has no source file — omit the empty parenthetical.
        Some(ns) if m.file.is_empty() => out.push_str(&format!("# `{ns}`\n\n")),
        Some(ns) => out.push_str(&format!("# `{ns}` ({})\n\n", m.file)),
        None => out.push_str(&format!("# `{}`\n\n", m.file)),
    }
    // Prose may carry `// sample:start`/`// sample:end` context folding. Static markdown has no
    // viewer to expand, so the fold is baked in as a `<details>` block — the reader gets the short
    // sample, and the whole compiling program is still one click away. Unmarked prose is unchanged.
    if let Some(doc) = &m.doc {
        out.push_str(&noeta_ide::sample::fold_markdown(doc));
        out.push_str("\n\n");
    }
    for item in &m.items {
        match item {
            Item::Section(text) => {
                out.push_str(&noeta_ide::sample::fold_markdown(text));
                out.push_str("\n\n");
            }
            Item::Decl(d) => {
                out.push_str(&format!("### `{} {}`\n\n", d.kind, d.name));
                // The signature is a rendered one-liner, never a sample — emitted as-is.
                out.push_str(&format!("```noeta\n{}\n```\n\n", d.signature));
                if let Some(doc) = &d.doc {
                    out.push_str(&noeta_ide::sample::fold_markdown(doc));
                    out.push_str("\n\n");
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_docs_json_is_schema1_by_module_with_prose() {
        let (text, done) = registry_docs_json(None, ApiScope::All);
        assert!(done.modules > 3 && done.decls > 0);
        let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["schema"].as_u64(), Some(SCHEMA as u64));

        let modules = doc["modules"].as_array().unwrap();
        let math = modules
            .iter()
            .find(|m| m["namespace"] == "std.math")
            .expect("std.math module present");
        // Every item carries `kind` + `name` — the hosted registry renderer drops items missing
        // either, so this is the contract with noeta-registry's src/web.ts renderModule().
        let items = math["items"].as_array().unwrap();
        assert!(
            items
                .iter()
                .all(|i| i["kind"].is_string() && i["name"].is_string())
        );
        let sqrt = items.iter().find(|i| i["name"] == "sqrt").unwrap();
        assert_eq!(sqrt["kind"], "fn");
        // A rendered signature names its parameters wherever the native declaration does — the
        // same `param_names` a `name:` label at a call site binds against.
        assert_eq!(sqrt["signature"], "fn sqrt(x: float): float");
        assert!(sqrt["doc"].as_str().unwrap().contains("square root"));
        assert_eq!(sqrt["public"], true);

        // The whole artifact round-trips through the registry-render path (schema-only).
        let out = noeta_test_temp::TempDir::new("docgen-api");
        render_json_to(&out, &text).expect("renders from schema alone");
        assert!(out.join("std-math.md").exists());
    }

    /// Document one in-memory module and return its items, for the `.noe`-source tests below.
    fn docs_of(text: &str) -> ModuleDocs {
        let source = Source::new(SourceId::FIRST, "t.noe".to_string(), text.to_string());
        module_docs(&source).expect("the fixture parses")
    }

    /// The `DeclDocs` named `name`, or panic.
    fn decl<'a>(m: &'a ModuleDocs, name: &str) -> &'a DeclDocs {
        m.items
            .iter()
            .find_map(|i| match i {
                Item::Decl(d) if d.name == name => Some(d),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no declaration named `{name}`"))
    }

    #[test]
    fn a_types_methods_and_impls_are_documented() {
        // The statement walk matched five `Stmt` kinds and ended `_ => continue`, and the type
        // renderers printed fields only. So a package written entirely in Noeta shipped a reference
        // listing its types with no methods on them and no sign of what they implement — the same
        // hole the native side had, on the path every package takes.
        let m = docs_of(
            r#"
namespace demo.shapes

@doc { A point in the plane. }
pub struct Point {
    pub x: float

    @doc { The distance from the origin. }
    fn magnitude(): float { return self.x }

    impl Describable {
        fn describe(): string { return "point" }
    }
}

pub trait Describable {
    fn describe(): string
}
"#,
        );
        let point = decl(&m, "Point");
        // The methods reach the signature — including the one the parser flattened out of the
        // in-body `impl`, which is how a call resolves it.
        assert!(
            point.signature.contains("fn magnitude(): float"),
            "{}",
            point.signature
        );
        assert!(
            point.signature.contains("fn describe(): string"),
            "{}",
            point.signature
        );
        // The conformance is stated, and the method's own `@doc` reaches the page.
        let doc = point.doc.as_deref().unwrap();
        assert!(doc.contains("**Implements:** `Describable`"), "{doc}");
        assert!(doc.contains("#### `fn magnitude(): float`"), "{doc}");
        assert!(doc.contains("The distance from the origin."), "{doc}");
    }

    #[test]
    fn a_standalone_impl_joins_its_target_or_documents_itself() {
        // A standalone `impl Trait for T` is the one method carrier the parser flattens nowhere, so
        // it needs collecting on its own. When its target is declared in the same file it folds
        // into that declaration; when the target lives in a sibling module — which the package
        // orphan rule permits — it becomes an item rather than vanishing.
        let m = docs_of(
            r#"
namespace demo.shapes

pub enum Stroke: string {
    Solid = "solid"
}

impl Describable for Stroke {
    fn describe(): string { return "stroke" }
}

impl Describable for Imported {
    fn describe(): string { return "imported" }
}
"#,
        );
        let stroke = decl(&m, "Stroke");
        assert!(
            stroke
                .doc
                .as_deref()
                .unwrap()
                .contains("**Implements:** `Describable`")
        );
        // The impl's methods are callable on the type, so they belong in its declaration — under a
        // marker, because they did not come from the type's own body.
        assert!(
            stroke.signature.contains("// impl Describable"),
            "{}",
            stroke.signature
        );
        assert!(stroke.signature.contains("fn describe(): string"));
        // The backing and its variant constants survive: an enum's `.value()` exists *because* of
        // the backing, and dropping it turned every backed enum into a plain one.
        assert!(
            stroke.signature.contains("enum Stroke: string {"),
            "{}",
            stroke.signature
        );
        assert!(
            stroke.signature.contains("Solid = \"solid\""),
            "{}",
            stroke.signature
        );

        let orphan = decl(&m, "Describable for Imported");
        assert_eq!(orphan.kind, "impl");
        assert!(
            orphan
                .signature
                .starts_with("impl Describable for Imported {")
        );
    }

    #[test]
    fn method_prose_is_keyed_by_span_not_name() {
        // `resolve_docs` reports a method's prose under its bare name, so a name-keyed map hands a
        // top-level `describe` and a method `describe` each other's paragraph — whichever was
        // inserted last wins for both.
        let m = docs_of(
            r#"
namespace demo.shapes

@doc { The FREE function. }
pub fn describe(): string { return "free" }

pub class Panel {
    @doc { The METHOD. }
    fn describe(): string { return "method" }
}
"#,
        );
        assert_eq!(
            decl(&m, "describe").doc.as_deref(),
            Some("The FREE function.")
        );
        assert!(
            decl(&m, "Panel")
                .doc
                .as_deref()
                .unwrap()
                .contains("The METHOD.")
        );
    }

    #[test]
    fn nominal_declarations_land_on_their_namespaces_module_page() {
        // `registry_docs_json` walked only `modules()`/`types()`, so a unit's traits, enums,
        // classes and structs reached no artifact at all. They now join the page of the module they
        // are namespaced under — the same place `generate` puts a `.noe` `trait`/`enum`/`struct` —
        // so a native package's reference reads like a Noeta-source one.
        let (text, _) = registry_docs_json(None, ApiScope::All);
        let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
        let modules = doc["modules"].as_array().unwrap();

        let vec_page = modules
            .iter()
            .find(|m| m["namespace"] == "std.vec")
            .expect("std.vec page");
        let kernels = vec_page["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["name"] == "Kernels")
            .expect("the Kernels trait is an item on std.vec");
        assert_eq!(kernels["kind"], "trait");
        assert!(
            kernels["signature"]
                .as_str()
                .unwrap()
                .contains("trait Kernels {")
        );
        assert!(
            kernels["doc"]
                .as_str()
                .unwrap()
                .contains("Element-wise arithmetic")
        );

        // The http page carries both an enum and a struct alongside its functions.
        let http = modules
            .iter()
            .find(|m| m["namespace"] == "std.http")
            .expect("std.http page");
        let kinds: Vec<(&str, &str)> = http["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|i| Some((i["name"].as_str()?, i["kind"].as_str()?)))
            .collect();
        assert!(kinds.contains(&("Framing", "enum")), "{kinds:?}");
        assert!(kinds.contains(&("Frame", "struct")), "{kinds:?}");

        // Every kind the artifact now emits survives the schema-only render path — `render_json_to`
        // drops an item whose `kind` it does not know, so a new kind that never reached its
        // whitelist would vanish between generating and rendering.
        let out = noeta_test_temp::TempDir::new("docgen-nominal");
        render_json_to(&out, &text).expect("renders from schema alone");
        let page = std::fs::read_to_string(out.join("std-vec.md")).unwrap();
        assert!(page.contains("### `trait Kernels`"), "{page}");
        let http_page = std::fs::read_to_string(out.join("std-http.md")).unwrap();
        assert!(http_page.contains("### `enum Framing`"));
        assert!(http_page.contains("### `struct Frame`"));
    }

    #[test]
    fn finalize_native_docs_merges_and_stamps() {
        let api = r#"{"schema":1,"package":null,"modules":[
            {"file":"","namespace":"imgfx.fx","doc":null,"items":[
                {"kind":"fn","name":"blur","signature":"fn blur(bytes): bytes","doc":"Blur.","public":true}]}]}"#;
        // A `.noe` glue module plus a duplicate of the native namespace (the compiled surface wins).
        let noe = r#"{"schema":1,"package":null,"modules":[
            {"file":"helpers.noe","namespace":"imgfx.helpers","doc":null,"items":[
                {"kind":"fn","name":"clamp","signature":"pub fn clamp(x: int): int","doc":null,"public":true}]},
            {"file":"dup.noe","namespace":"imgfx.fx","doc":null,"items":[]}]}"#;
        let out = finalize_native_docs(api, Some(noe), "acme/imgfx", "1.2.0");
        let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(doc["schema"], 1);
        assert_eq!(doc["package"]["name"], "acme/imgfx");
        assert_eq!(doc["package"]["version"], "1.2.0");
        let ns: Vec<&str> = doc["modules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["namespace"].as_str().unwrap())
            .collect();
        // Native module first, the .noe glue merged in, the duplicate namespace dropped.
        assert_eq!(ns, vec!["imgfx.fx", "imgfx.helpers"]);
        // API prose survives (the compiled surface, not the empty dup).
        assert_eq!(doc["modules"][0]["items"][0]["doc"], "Blur.");
    }

    #[test]
    fn registry_docs_json_scopes_to_a_root() {
        // Scoping to `std` keeps the std surface; an unknown root yields nothing (a native package
        // documenting itself excludes std this way).
        let (std_text, std_done) = registry_docs_json(None, ApiScope::Root("std"));
        assert!(std_done.modules > 3);
        let doc: serde_json::Value = serde_json::from_str(&std_text).unwrap();
        assert!(
            doc["modules"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m["namespace"].as_str().unwrap().starts_with("std"))
        );
        let (_empty_text, empty_done) = registry_docs_json(None, ApiScope::Root("nosuchpkg"));
        assert_eq!(empty_done.modules, 0);
    }

    #[test]
    fn registry_docs_json_non_builtin_is_empty_in_the_stock_toolchain() {
        // The stock (uncomposed) toolchain registers ONLY builtin units, so the publish scope
        // documents nothing — the builtin set really is the exact complement of a composition's
        // package surface. (The composed-toolchain half — a package extension surviving the
        // exclusion — is the cli integration test over the imgfx fixture, and the divergent-root
        // case is unit-tested in noeta-ide::api.)
        let (_text, done) = registry_docs_json(None, ApiScope::NonBuiltin);
        assert_eq!(done.modules, 0, "no non-builtin units in the stock binary");
    }
}
