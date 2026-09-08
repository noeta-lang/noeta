//! The synthetic tier: projects at a chosen size whose graph is known because the generator wrote
//! it.
//!
//! The hand-written corpus carries realistic naming and is the only place accuracy is measured. This
//! is for the other question — how the tools behave as a project grows — so it emits a project of N
//! declarations across N/100 modules and, beside it, the manifest of every call and import edge it
//! produced. The call structure is acyclic by construction (a function calls only functions minted
//! before it), so the manifest is the whole truth about the graph and the harness can report how
//! much of it the compiler's own graph reproduces.
//!
//! Nothing here is gated. A latency number on this box measures the afternoon, and the agreement
//! number is a property of the compiler that the conformance corpus owns.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use serde_json::json;

use crate::corpus::Analysis;
use crate::gold;
use crate::service::{Service, Tool, args};

/// How many functions one generated module holds.
const PER_MODULE: usize = 100;

/// How many callees a generated function has.
const FAN_OUT: usize = 3;

/// The generated project, and the graph the generator knows it emitted.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Manifest {
    pub declarations: usize,
    pub modules: usize,
    /// Every emitted function, as `(qualified name, file)`.
    pub nodes: Vec<(String, String)>,
    /// Every call edge the generator wrote, as `(caller, callee)` qualified names.
    pub calls: Vec<(String, String)>,
    /// Every reference edge (a function passed as a value), as `(holder, referenced)`.
    pub references: Vec<(String, String)>,
    /// Every `use` the generator wrote, as `(file, module)`.
    pub imports: Vec<(String, String)>,
}

/// Write a synthetic project of `declarations` functions under `dir`, and return its manifest.
pub fn emit(dir: &Path, prefix: &str, declarations: usize) -> Result<Manifest, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    std::fs::write(
        dir.join("noeta.toml"),
        format!("[package]\nname = \"local/{prefix}\"\nversion = \"0.1.0\"\n"),
    )
    .map_err(|e| format!("cannot write the manifest: {e}"))?;

    let modules = declarations.div_ceil(PER_MODULE).max(1);
    let mut manifest = Manifest {
        declarations,
        modules,
        nodes: Vec::new(),
        calls: Vec::new(),
        references: Vec::new(),
        imports: Vec::new(),
    };
    // Name every function up front, so a body can call one the generator has already minted and the
    // call graph stays acyclic without a second pass.
    let name_of = |index: usize| format!("f{index}");
    let module_of = |index: usize| index / PER_MODULE;
    for index in 0..declarations {
        manifest.nodes.push((
            format!("{prefix}.m{}.{}", module_of(index), name_of(index)),
            format!("m{}.noe", module_of(index)),
        ));
    }

    for module in 0..modules {
        let file = format!("m{module}.noe");
        let first = module * PER_MODULE;
        let last = ((module + 1) * PER_MODULE).min(declarations);
        let mut imports: BTreeSet<usize> = BTreeSet::new();
        let mut body = String::new();
        for index in first..last {
            // Three callees, spread backwards so most edges cross a module boundary.
            let callees: Vec<usize> = (1..=FAN_OUT)
                .map(|step| index.wrapping_sub(step * 7 + 1))
                .filter(|callee| *callee < index)
                .collect();
            for callee in &callees {
                if module_of(*callee) != module {
                    imports.insert(module_of(*callee));
                }
                manifest.calls.push((
                    format!("{prefix}.m{module}.{}", name_of(index)),
                    format!("{prefix}.m{}.{}", module_of(*callee), name_of(*callee)),
                ));
            }
            let mut expression = format!("n + {}", index % 17);
            for callee in &callees {
                expression = format!("{expression} + {}(n)", name_of(*callee));
            }
            body.push_str(&format!(
                "pub fn {}(n: int): int {{\n    return {expression}\n}}\n\n",
                name_of(index)
            ));
        }
        // One function-as-a-value per module, so the corpus carries reference edges too.
        if last > first {
            let referenced = name_of(first);
            body.push_str(&format!(
                "pub fn hold{module}(): (int) -> int {{\n    return {referenced}\n}}\n\n"
            ));
            manifest.references.push((
                format!("{prefix}.m{module}.hold{module}"),
                format!("{prefix}.m{module}.{referenced}"),
            ));
            manifest
                .nodes
                .push((format!("{prefix}.m{module}.hold{module}"), file.clone()));
        }
        let mut head = String::new();
        for other in &imports {
            let start = other * PER_MODULE;
            let end = ((other + 1) * PER_MODULE).min(declarations);
            let names: Vec<String> = (start..end).map(name_of).collect();
            head.push_str(&format!(
                "use {prefix}.m{other}.{{{}}};\n",
                names.join(", ")
            ));
            manifest
                .imports
                .push((file.clone(), format!("{prefix}.m{other}")));
        }
        if !head.is_empty() {
            head.push('\n');
        }
        std::fs::write(dir.join(&file), format!("{head}{body}"))
            .map_err(|e| format!("cannot write {file}: {e}"))?;
    }

    // The entry: call the last function of every module, so the whole project is reachable.
    let mut entry = String::new();
    for module in 0..modules {
        entry.push_str(&format!("use {prefix}.m{module}.hold{module};\n"));
        manifest
            .imports
            .push(("main.noe".to_string(), format!("{prefix}.m{module}")));
    }
    entry.push_str("\nfn total(): int {\n    mut sum = 0\n");
    for module in 0..modules {
        entry.push_str(&format!("    sum = sum + hold{module}()(1)\n"));
        manifest.calls.push((
            format!("{prefix}.main.total"),
            format!("{prefix}.m{module}.hold{module}"),
        ));
    }
    entry.push_str("    return sum\n}\n\necho total()\n");
    manifest
        .nodes
        .push((format!("{prefix}.main.total"), "main.noe".to_string()));
    std::fs::write(dir.join("main.noe"), entry)
        .map_err(|e| format!("cannot write main.noe: {e}"))?;
    Ok(manifest)
}

/// Generate a project at each requested size, check it, and report build time, graph agreement and
/// per-tool latency.
pub async fn measure(sizes: &[usize], out_dir: Option<&Path>) -> Result<ExitCode, String> {
    let root = match out_dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::temp_dir().join(format!("graphbench-synthetic-{}", std::process::id())),
    };
    let mut service = Service::start().await?;
    println!(
        "{:<8} {:>8} {:>8} {:>9} {:>9} {:>10} {:>10} {:>10}",
        "decls", "modules", "files", "emit ms", "link ms", "nodes ok", "edges ok", "symbols ms"
    );
    for size in sizes {
        let dir = root.join(format!("n{size}"));
        let started = Instant::now();
        let manifest = emit(&dir, "Syn", *size)?;
        let emitted = started.elapsed().as_millis();

        let entry = dir.join("main.noe");
        let started = Instant::now();
        let analysis = Analysis::open(&format!("synthetic-{size}"), &dir, &entry)?;
        let facts = gold::facts(&analysis)?;
        let linked = started.elapsed().as_millis();

        let names: BTreeSet<&str> = facts.nodes.iter().map(|n| n.qualified.as_str()).collect();
        let nodes_found = manifest
            .nodes
            .iter()
            .filter(|(name, _)| names.contains(name.as_str()))
            .count();
        let mut edges_found = 0usize;
        for (from, to) in manifest.calls.iter().chain(manifest.references.iter()) {
            let Some(caller) = facts.nodes.iter().find(|n| &n.qualified == from) else {
                continue;
            };
            if facts
                .callees(caller.id, 1)
                .iter()
                .any(|id| &facts.node(*id).qualified == to)
            {
                edges_found += 1;
            }
        }

        let started = Instant::now();
        let _ = service
            .call(
                Tool::Symbols,
                args([("file", json!(entry.display().to_string()))]),
            )
            .await;
        let symbols = started.elapsed().as_millis();

        let total_edges = manifest.calls.len() + manifest.references.len();
        println!(
            "{:<8} {:>8} {:>8} {:>9} {:>9} {:>9.1}% {:>9.1}% {:>10}",
            size,
            manifest.modules,
            manifest.modules + 1,
            emitted,
            linked,
            100.0 * nodes_found as f64 / manifest.nodes.len() as f64,
            100.0 * edges_found as f64 / total_edges.max(1) as f64,
            symbols,
        );
        let path = dir.join("gold-manifest.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&manifest)
                .map_err(|e| format!("cannot serialize the manifest: {e}"))?,
        )
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    service.shutdown().await;
    println!("\ngraphbench: synthetic projects under {}", root.display());
    println!(
        "graphbench: these rows are a scaling and latency observation, gated nowhere — accuracy is \
         measured on the hand-written corpus only."
    );
    Ok(ExitCode::SUCCESS)
}

/// The synthetic project's directory for a size, for a caller that wants to keep it.
pub fn dir_for(root: &Path, size: usize) -> PathBuf {
    root.join(format!("n{size}"))
}
