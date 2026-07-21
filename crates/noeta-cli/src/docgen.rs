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
const SCHEMA: u32 = 1;

/// Generate the documentation artifact for the package containing `entry` into `out`.
pub fn generate(entry: &Path, out: &Path) -> Result<Generated, String> {
    let workspace = noeta_loader::read_workspace(entry)
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

/// Build the **API-reference** `docs.json` (docs-browser Arc 2) from the intrinsic registry — the
/// stdlib and any composed native modules — rather than from `.noe` source. One module entry per
/// registry module (`std.math`, `std.http.client`, …), each function an `fn` item carrying its
/// rendered signature and any registered doc prose. Same schema-1 shape as [`generate`], so it
/// rides to the registry and renders on the hosted docs page identically. `package` names the
/// artifact (e.g. the toolchain's `std`), or `None` for a generic title. `root` scopes the surface
/// to a single extension's namespace (a package documenting *itself*, excluding std); `None`
/// documents the whole registry.
pub fn registry_docs_json(
    package: Option<(String, String)>,
    root: Option<&str>,
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
    let (api_modules, api_types) = match root {
        Some(r) => (noeta_ide::api::modules_of(r), noeta_ide::api::types_of(r)),
        None => (noeta_ide::api::modules(), noeta_ide::api::types()),
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
    modules.extend(api_types.into_iter().map(|t| ModuleDocs {
        file: String::new(),
        slug: t.qualified.replace('.', "-"),
        namespace: Some(t.qualified),
        doc: None,
        items: t.methods.into_iter().map(fn_item).collect(),
    }));
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
    if !lexed.diagnostics.is_empty() || !parsed.diagnostics.is_empty() {
        return None;
    }
    let program: &Program = &parsed.program;

    let namespace = program.stmts.iter().find_map(|s| match s {
        Stmt::Namespace { path, .. } => Some(path.join(".")),
        _ => None,
    });
    // A package module documents its `pub` API; a bare script documents everything.
    let public_only = namespace.is_some();

    // Adjacency-resolved docs: the module doc, the sections, and each decl's text keyed by name.
    let mut module_doc = None;
    let mut decl_docs: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut sections: Vec<(u32, String)> = Vec::new();
    for doc in resolve_docs(program) {
        let text = dedent_doc(&doc.text).trim().to_string();
        match doc.target {
            DocTarget::Module => module_doc = Some(text),
            DocTarget::Section => sections.push((doc.span.start, text)),
            DocTarget::Decl { name, .. } => {
                decl_docs.insert(name, text);
            }
        }
    }

    // Items in source order: sections at their span, declarations at theirs.
    let mut items: Vec<(u32, Item)> = sections
        .into_iter()
        .map(|(at, text)| (at, Item::Section(text)))
        .collect();
    for stmt in &program.stmts {
        let (at, decl) = match stmt {
            Stmt::Fn(f) => (f.span.start, fn_docs(f, &decl_docs)),
            Stmt::Struct(s) => (s.span.start, struct_docs(s, &decl_docs)),
            Stmt::Class(c) => (c.span.start, class_docs(c, &decl_docs)),
            Stmt::Enum(e) => (e.span.start, enum_docs(e, &decl_docs)),
            Stmt::Trait(t) => (t.span.start, trait_docs(t, &decl_docs)),
            _ => continue,
        };
        if public_only && !decl.public {
            continue;
        }
        items.push((at, Item::Decl(decl)));
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

fn fn_docs(f: &FnDecl, docs: &std::collections::HashMap<String, String>) -> DeclDocs {
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
    if f.is_async {
        sig.push_str("async ");
    }
    let params: Vec<String> = f
        .params
        .iter()
        .map(|p| match &p.ty {
            Some(ty) => format!("{}: {}", p.name, render_type_ref(ty)),
            None => p.name.clone(),
        })
        .collect();
    sig.push_str(&format!("fn {}({})", f.name, params.join(", ")));
    if let Some(ret) = &f.ret {
        sig.push_str(&format!(": {}", render_type_ref(ret)));
    }
    DeclDocs {
        kind: "fn",
        name: f.name.clone(),
        signature: sig,
        doc: docs.get(&f.name).cloned(),
        public: f.is_public,
    }
}

/// Render a field list body (`{ name: T … }`), one field per line.
fn fields_block(fields: &[noeta_ast::FieldDecl]) -> String {
    let mut out = String::from(" {\n");
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
    out.push('}');
    out
}

fn struct_docs(s: &StructDecl, docs: &std::collections::HashMap<String, String>) -> DeclDocs {
    let mut sig = String::new();
    if s.decorators.attribute.is_some() {
        sig.push_str("@attribute\n");
    }
    if s.is_public {
        sig.push_str("pub ");
    }
    sig.push_str(&format!("struct {}{}", s.name, fields_block(&s.fields)));
    DeclDocs {
        kind: "struct",
        name: s.name.clone(),
        signature: sig,
        doc: docs.get(&s.name).cloned(),
        public: s.is_public,
    }
}

fn class_docs(c: &ClassDecl, docs: &std::collections::HashMap<String, String>) -> DeclDocs {
    let mut sig = String::new();
    if c.is_public {
        sig.push_str("pub ");
    }
    sig.push_str(&format!("class {}{}", c.name, fields_block(&c.fields)));
    DeclDocs {
        kind: "class",
        name: c.name.clone(),
        signature: sig,
        doc: docs.get(&c.name).cloned(),
        public: c.is_public,
    }
}

fn enum_docs(e: &EnumDecl, docs: &std::collections::HashMap<String, String>) -> DeclDocs {
    let mut sig = String::new();
    if e.is_public {
        sig.push_str("pub ");
    }
    sig.push_str(&format!("enum {} {{\n", e.name));
    for v in &e.variants {
        if v.fields.is_empty() {
            sig.push_str(&format!("    {}\n", v.name));
        } else {
            let fields: Vec<String> = v
                .fields
                .iter()
                .map(|p| match &p.ty {
                    Some(ty) => format!("{}: {}", p.name, render_type_ref(ty)),
                    None => p.name.clone(),
                })
                .collect();
            sig.push_str(&format!("    {}({})\n", v.name, fields.join(", ")));
        }
    }
    sig.push('}');
    DeclDocs {
        kind: "enum",
        name: e.name.clone(),
        signature: sig,
        doc: docs.get(&e.name).cloned(),
        public: e.is_public,
    }
}

fn trait_docs(t: &TraitDecl, docs: &std::collections::HashMap<String, String>) -> DeclDocs {
    let mut sig = String::new();
    for a in &t.decorators.attrs {
        sig.push_str(&format!("#[{}]\n", a.name));
    }
    if t.is_public {
        sig.push_str("pub ");
    }
    sig.push_str(&format!("trait {} {{\n", t.name));
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
        sig.push_str(&format!("    fn {}({})", m.sig.name, params.join(", ")));
        if let Some(ret) = &m.sig.ret {
            sig.push_str(&format!(": {}", render_type_ref(ret)));
        }
        // A default method shows a `{ … }` marker; a required one is bodiless.
        sig.push_str(if m.has_default { " { … }\n" } else { "\n" });
    }
    sig.push('}');
    DeclDocs {
        kind: "trait",
        name: t.name.clone(),
        signature: sig,
        doc: docs.get(&t.name).cloned(),
        public: t.is_public,
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
        let (text, done) = registry_docs_json(None, None);
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
        assert_eq!(sqrt["signature"], "fn sqrt(float): float");
        assert!(sqrt["doc"].as_str().unwrap().contains("square root"));
        assert_eq!(sqrt["public"], true);

        // The whole artifact round-trips through the registry-render path (schema-only).
        let out = std::env::temp_dir().join("noeta_docgen_api_test");
        let _ = std::fs::remove_dir_all(&out);
        render_json_to(&out, &text).expect("renders from schema alone");
        assert!(out.join("std-math.md").exists());
        let _ = std::fs::remove_dir_all(&out);
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
        let (std_text, std_done) = registry_docs_json(None, Some("std"));
        assert!(std_done.modules > 3);
        let doc: serde_json::Value = serde_json::from_str(&std_text).unwrap();
        assert!(
            doc["modules"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m["namespace"].as_str().unwrap().starts_with("std"))
        );
        let (_empty_text, empty_done) = registry_docs_json(None, Some("nosuchpkg"));
        assert_eq!(empty_done.modules, 0);
    }
}
