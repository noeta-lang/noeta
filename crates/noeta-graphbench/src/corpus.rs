//! The corpus: which projects the benchmark measures, and the one salsa workspace each of them is
//! read through.
//!
//! A project is a directory with a `noeta.toml` and an entry file, listed in
//! `tests/graphbench/corpus/index.json`. Building the workspace here mirrors what the MCP surface
//! does per call (read the entry and its siblings, assign `SourceId`s in the loader's one ordering,
//! hand them to `workspace_with_deps`), so gold and arms read the same program. No corpus project
//! declares a dependency package, which is what keeps that mirror a mirror.

use std::path::{Path, PathBuf};

use noeta_db::{LangDatabase, Workspace};
use noeta_span::Source;

/// One project of the corpus, as the roster declares it.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ProjectSpec {
    pub name: String,
    pub entry: String,
    pub domain: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Roster {
    projects: Vec<ProjectSpec>,
}

/// Read `tests/graphbench/corpus/index.json`.
pub fn roster(data: &Path) -> Result<Vec<ProjectSpec>, String> {
    let path = data.join("corpus/index.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let roster: Roster = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
    Ok(roster.projects)
}

/// A prepared project: the database, its workspace handle, and the ordered sources.
///
/// Held together because every salsa query borrows the database. Index `i` of [`Analysis::sources`]
/// is `SourceId(i)`, which is what turns a span back into a file.
#[derive(Debug)]
pub struct Analysis {
    pub db: LangDatabase,
    pub ws: Workspace,
    pub sources: Vec<Source>,
    pub paths: Vec<noeta_loader::ModulePath>,
    pub root: PathBuf,
    pub entry: PathBuf,
    pub name: String,
}

impl Analysis {
    /// Build the workspace for a project directory and its entry file.
    pub fn open(name: &str, root: &Path, entry: &Path) -> Result<Analysis, String> {
        let package_root = noeta_pm::sources::package_root(entry);
        let raw = noeta_loader::read_workspace(entry, package_root.as_ref())
            .map_err(|e| format!("cannot read {}: {e}", entry.display()))?;
        let sources = noeta_loader::workspace_sources(&raw, &[]);
        let paths = raw.paths.clone();
        let db = LangDatabase::default();
        let (first, rest) = sources
            .split_first()
            .expect("read_workspace always yields the entry");
        let ws = noeta_db::workspace_with_deps(
            &db,
            first,
            rest,
            &[],
            &noeta_span::PackageUses::new(),
            noeta_lexer::Edition::default(),
            &paths,
        );
        Ok(Analysis {
            db,
            ws,
            sources,
            paths,
            root: root.to_path_buf(),
            entry: entry.to_path_buf(),
            name: name.to_string(),
        })
    }

    /// The source texts, index-aligned with `SourceId` — what `callgraph::build` classifies through.
    pub fn texts(&self) -> Vec<&str> {
        self.sources.iter().map(|s| s.text()).collect()
    }

    /// The file name a `SourceId` names.
    pub fn file_of(&self, source: noeta_span::SourceId) -> Option<&str> {
        self.sources.get(source.0 as usize).map(|s| s.name())
    }

    /// The module path a `SourceId` derives (`Shop.handlers.orders`).
    pub fn module_of(&self, source: noeta_span::SourceId) -> Option<String> {
        self.paths
            .get(source.0 as usize)
            .and_then(|p| p.derived())
            .map(|segments| segments.join("."))
    }
}

/// The corpus gate: every project must **link** and check with zero errors, and the harness refuses
/// to score one that does not. A project that stopped linking would degrade every graph tool to the
/// entry file's own AST (unqualified names, real calls rendered as external leaves) and the arms
/// would score against a program the compiler never saw.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GateResult {
    pub name: String,
    pub linked: bool,
    pub errors: usize,
    pub warnings: usize,
    pub files: usize,
    pub first_problem: Option<String>,
}

impl GateResult {
    pub fn ok(&self) -> bool {
        self.linked && self.errors == 0
    }
}

/// Check one project the way `noeta check <dir>` does, and report whether the link held.
pub fn gate(analysis: &Analysis) -> GateResult {
    let linked = noeta_db::linked(&analysis.db, analysis.ws);
    let link_ok = linked.program.is_ok();
    let options = noeta_project::ProjectCheckOptions::new();
    let checked = noeta_project::project_check(&analysis.root, &options);
    let first = checked
        .diagnostics
        .iter()
        .find(|d| d.diagnostic.severity == noeta_diagnostics::Severity::Error)
        .map(|d| format!("{} {}", d.diagnostic.code.code(), d.diagnostic.message))
        .or_else(|| checked.problems.first().cloned())
        .or_else(|| {
            linked
                .program
                .as_ref()
                .err()
                .and_then(|diagnostics| diagnostics.first())
                .map(|d| format!("{} {}", d.code.code(), d.message))
        });
    GateResult {
        name: analysis.name.clone(),
        linked: link_ok,
        errors: checked.errors(),
        warnings: checked.warnings(),
        files: checked.files_checked,
        first_problem: first,
    }
}
