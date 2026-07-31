//! The **impact engine** (server-hmr W3; multi-file since the salsa rework) — one
//! runner-agnostic seam answering: *given this edit, which declarations may behave
//! differently?* Consumers filter the answer to their own tier and rerun only that:
//! `noeta test --watch` reruns the impacted `@test` fns (via the runner's `--name` filter),
//! `noeta bench --watch` the impacted `@bench` fns, and a third-party tier runner gets the
//! same query through this module.
//!
//! Two surfaces share one pipeline — the hot-swap differ
//! ([`noeta_compiler::hotswap::diff_programs`]) attributes an edit to definition names, and the
//! **reverse transitive closure** over the static call graph ([`crate::callgraph`]) walks from
//! those names to everything that calls (or references) them:
//!
//! - [`impact_of_edit`] — the single-file query (one source, no project context), unchanged.
//! - [`ImpactSession`] — the whole-project engine the watch loop drives: a salsa
//!   [`Workspace`](noeta_db::Workspace) over the entry's directory (built by
//!   [`crate::workspace::sync`], the same construction the editor uses), per-file baselines,
//!   and a call graph over the **linked** program — so an edit to an imported module narrows
//!   to the entry tests that transitively reach it, instead of degrading to a full rerun.
//!   Between edits only the changed file's salsa input moves; every other member's parse
//!   stays memoized.
//!
//! # Names and qualification
//!
//! A per-file diff yields the file's own (unqualified) declaration names — `add`,
//! `Counter.bump` — but the linked program the runner executes carries **namespace-qualified**
//! identity (`App.Lib.add`, `App.Lib.Counter.bump`), and a tier fn's `TierFn` name is
//! qualified the same way. The session re-qualifies diff names with the edited file's declared
//! `namespace` before seeding the closure, so seeds, graph nodes, and the runner's `--name`
//! filter all speak the linked vocabulary.
//!
//! # Soundness valves (part of the contract, not the consumer's job)
//!
//! An edit the differ cannot attribute — a layout/signature/namespace change, a changed
//! *top-level statement* (fixtures and globals live there), red code — degrades to
//! [`Impact::All`] **with the reason**, as does a top-level *use* of an impacted declaration
//! (setup may differ for every run). The session adds the project-shaped valves: a change
//! outside the workspace's members, a changed member set, an unreadable file, a project that
//! no longer links. The closure is static: a call through a closure that was stored in a data
//! structure and invoked elsewhere is attributed to where the function was *referenced*, not
//! where the value ends up called — reference edges (`f` passed as a value) are followed
//! exactly like calls, which covers the common callback shapes, but consumers should still
//! surface an occasional full pass. False positives rerun harmlessly; the valves exist so
//! false *negatives* require genuinely dynamic reachability.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use noeta_compiler::hotswap::SwapDiff;
use salsa::Setter as _;

use crate::callgraph::{self, Callee};
use crate::workspace::{self, WorkspaceCache, disk_noe_uris, path_to_uri, uri_to_path};

/// The engine's answer for one edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Impact {
    /// The declarations (top-level fns, `Type.method`s, tier fns) whose behavior may have
    /// changed — the edited definitions plus the reverse closure of their callers. Empty means
    /// the edit was behaviorally inert (formatting, comments).
    Decls(Vec<String>),
    /// The edit cannot be attributed to declarations; rerun everything, and report why.
    All { reason: String },
}

/// Compute the impact of editing `old_src` into `new_src` (one file — the entry the runner was
/// pointed at, with no project context; [`ImpactSession`] is the multi-file engine).
pub fn impact_of_edit(old_src: &str, new_src: &str, edition: noeta_lexer::Edition) -> Impact {
    let tiers = noeta_lexer::TextTiers::default();
    let names = match diff_file(old_src, new_src, edition, &tiers) {
        FileDiff::All(reason) => return Impact::All { reason },
        FileDiff::Names { names, .. } => names,
    };
    if names.is_empty() {
        return Impact::Decls(Vec::new());
    }
    let Some(new_program) = parse_clean(new_src, edition, &tiers) else {
        // Unreachable in practice (diff_file just parsed this text), but degrade over panic.
        return Impact::All {
            reason: "the edit does not parse".into(),
        };
    };
    // Activate EVERY tier the file declares before checking and graphing: tier fns are
    // ordinary fns to a runner, and their method-call edges only resolve with checker
    // types — which the residual (stripped) form never gets.
    let tier_names = declared_tiers(&new_program);
    let tier_refs: Vec<&str> = tier_names.iter().map(String::as_str).collect();
    let activated = noeta_check::activate_tiers(&new_program, &tier_refs);
    let program = &activated.program;
    let mut editions = noeta_lexer::EditionMap::new();
    editions.set(noeta_span::SourceId::FIRST, edition);
    let checked = noeta_check::check_all_with_editions(program, editions);
    if noeta_diagnostics::has_errors(
        activated
            .diagnostics
            .iter()
            .chain(checked.diagnostics.iter()),
    ) {
        // Red code: the consumer's own run will surface the diagnostics. A *warning* is not red —
        // narrowing an edit that merely lints is still sound, and widening to `All` over one would
        // silently cost the watcher its whole point.
        return Impact::All {
            reason: "the edit does not check".into(),
        };
    }
    let graph = callgraph::build(program, &checked.expr_types, &[new_src]);
    // In an unlinked single file a dotted diff name is always `Type.method` (qualification is
    // the linker's job), so its member is everything after the first dot.
    let members: BTreeSet<String> = names
        .iter()
        .filter_map(|n| n.split_once('.').map(|(_, m)| m.to_string()))
        .collect();
    match reverse_closure(&graph, names.into_iter().collect(), members) {
        Ok(impacted) => Impact::Decls(impacted.into_iter().collect()),
        Err(reason) => Impact::All { reason },
    }
}

// --------------------------------------------------------------------- the shared pipeline

/// One file's diff, attributed to declaration names (still unqualified — the file's own view).
enum FileDiff {
    /// Unattributable; rerun everything for this reason.
    All(String),
    /// The changed/added/removed declaration names (empty = behaviorally inert), plus the
    /// file's declared `namespace` path (dotted) — the qualification prefix its declarations
    /// carry in the linked program.
    Names {
        names: Vec<String>,
        namespace: Option<String>,
    },
}

/// Diff one file's old and new sources into declaration names, applying the differ's
/// soundness valves. `tiers` is the text-tier set to lex under (the workspace's, so a
/// verbatim `@sql { … }` body declared in a sibling still parses).
fn diff_file(
    old_src: &str,
    new_src: &str,
    edition: noeta_lexer::Edition,
    tiers: &noeta_lexer::TextTiers,
) -> FileDiff {
    let Some(old_program) = parse_clean(old_src, edition, tiers) else {
        // The BASELINE not parsing means we cannot attribute anything against it.
        return FileDiff::All("the previous version does not parse".into());
    };
    let Some(new_program) = parse_clean(new_src, edition, tiers) else {
        return FileDiff::All("the edit does not parse".into());
    };
    let namespace = new_program.stmts.iter().find_map(|s| match s {
        noeta_ast::Stmt::Namespace { path, .. } => Some(path.join(".")),
        _ => None,
    });
    match noeta_compiler::hotswap::diff_programs(&old_program, old_src, &new_program, new_src) {
        SwapDiff::Unchanged => FileDiff::Names {
            names: Vec::new(),
            namespace,
        },
        SwapDiff::NeedsRestart(blockers) => FileDiff::All(
            blockers
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        ),
        SwapDiff::Swap(plan) => {
            if plan.rerun_top_level {
                // Top-level statements are every run's setup (globals, fixtures): a change
                // there can shift what any consumer observes.
                return FileDiff::All("top-level statements changed".into());
            }
            FileDiff::Names {
                names: plan
                    .changed
                    .iter()
                    .chain(&plan.added)
                    .chain(&plan.removed)
                    .cloned()
                    .collect(),
                namespace,
            }
        }
    }
}

/// The tier names a program declares (its `@<tier> { … }` blocks), plus the runner tiers
/// (`test`/`bench`) unconditionally — a method-attached `@test` (directive-sites) contributes
/// no top-level block, and activating a tier with no content is a no-op, so always naming the
/// runner tiers costs nothing and closes that gap.
fn declared_tiers(program: &noeta_ast::Program) -> BTreeSet<String> {
    let mut tiers: BTreeSet<String> = program
        .stmts
        .iter()
        .filter_map(|s| match s {
            noeta_ast::Stmt::TierBlock { tier, .. } => Some(tier.clone()),
            _ => None,
        })
        .collect();
    tiers.insert("test".into());
    tiers.insert("bench".into());
    tiers
}

/// Walk the reverse transitive closure over `graph` from the seeds, to a fixpoint: any caller
/// (or referencer) of an impacted declaration is impacted. Two edge flavors count: a resolved
/// `Function` edge, and — the method fallback — a `Dynamic` member edge whose member NAME
/// matches an impacted method's member (an untyped receiver's `c.bump()` cannot be resolved
/// to `Counter.bump` statically, so it over-approximates by name; a false positive reruns
/// harmlessly, and the fallback is what keeps missed static method calls out of the
/// false-negative budget). `members` seeds that fallback set; it grows as impacted METHOD
/// nodes join (a caller that is itself a method re-enters by member name too). `Err` is the
/// setup valve: a TOP-LEVEL statement uses an impacted declaration.
fn reverse_closure(
    graph: &callgraph::CallGraph,
    mut impacted: BTreeSet<String>,
    mut members: BTreeSet<String>,
) -> Result<BTreeSet<String>, String> {
    loop {
        let mut grew = false;
        for edge in &graph.edges {
            let hit: Option<String> = match &edge.callee {
                Callee::Function(i) => impacted
                    .contains(&graph.functions[*i].name)
                    .then(|| graph.functions[*i].name.clone()),
                Callee::Dynamic(target) => {
                    let member = target.rsplit('.').next().unwrap_or(target);
                    members.contains(member).then(|| target.clone())
                }
                Callee::External(_) => None,
            };
            let Some(used) = hit else { continue };
            match edge.caller {
                Some(j) => {
                    let node = &graph.functions[j];
                    if impacted.insert(node.name.clone()) {
                        if node.method
                            && let Some(member) = node.name.rsplit('.').next()
                        {
                            members.insert(member.to_string());
                        }
                        grew = true;
                    }
                }
                None => {
                    return Err(format!("the top level uses changed `{used}`"));
                }
            }
        }
        if !grew {
            return Ok(impacted);
        }
    }
}

fn parse_clean(
    src: &str,
    edition: noeta_lexer::Edition,
    tiers: &noeta_lexer::TextTiers,
) -> Option<noeta_ast::Program> {
    let source = noeta_span::Source::new(noeta_span::SourceId::FIRST, "<impact>", src);
    let lexed = noeta_lexer::lex_in(&source, edition, tiers);
    let parsed = noeta_parser::parse_in(&source, &lexed.tokens, edition, tiers);
    (lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty()).then_some(parsed.program)
}

// ------------------------------------------------------------------ the whole-project engine

/// The multi-file impact engine the watch loop drives (`noeta test --watch` / `bench`): a
/// salsa [`Workspace`](noeta_db::Workspace) over the entry's directory — built by the same
/// [`workspace::sync`] the editor uses, dependency packages included — plus a per-file source
/// baseline captured at each run. [`impact_of_changes`](Self::impact_of_changes) then answers
/// for a change burst anywhere in the project: per-file diffs seed a reverse closure over the
/// call graph of the **linked** program (the program the runner actually executes), so an edit
/// to an imported module narrows to the entry tests that transitively reach it.
///
/// Incrementality: each edit moves only the changed member's salsa input, so every other
/// member's lex/parse stays memoized across the watch session; the link and the check re-run
/// per edit (they depend on every member by construction).
#[derive(Debug)]
pub struct ImpactSession {
    db: noeta_db::LangDatabase,
    cache: Option<WorkspaceCache>,
    /// The watched directory (the canonical entry's parent), re-scanned on each rebaseline.
    dir: PathBuf,
    /// The canonical entry's URI — the member the linked program is resolved from.
    entry_uri: String,
    /// Member URI → the source text the last run observed (the diff baseline).
    baselines: HashMap<String, String>,
    /// The **non-`.noe` files the linked program's expansion hooks reported reading** — an
    /// `@openapi` spec, say — canonicalized. Captured at each rebaseline from
    /// [`noeta_db::LinkedProgram::reads`]. The watcher watches these alongside the members so that
    /// editing (or creating) one re-runs the client generation; `impact_of_changes` treats a change
    /// to one as [`Impact::All`], because a spec change invalidates generated members that the
    /// `.noe`-vocabulary diff cannot otherwise see. Empty for the common case of no expanding
    /// directive.
    reads: Vec<PathBuf>,
}

impl ImpactSession {
    /// Build a session for `entry` (a `.noe` file): scan its directory, build the salsa
    /// workspace (members + dependency packages), and capture the initial baseline. `None`
    /// when the entry cannot anchor a project — unreadable, directory-less, missing from its
    /// own directory scan — in which case the caller falls back to rerunning everything.
    pub fn new(entry: &Path) -> Option<Self> {
        let entry = entry.canonicalize().ok()?;
        let dir = entry.parent()?.to_path_buf();
        let entry_uri = path_to_uri(&entry);
        let mut session = ImpactSession {
            db: noeta_db::LangDatabase::default(),
            cache: None,
            dir,
            entry_uri,
            baselines: HashMap::new(),
            reads: Vec::new(),
        };
        session.rebaseline();
        session
            .cache
            .as_ref()
            .is_some_and(|c| c.source_uris.contains(&session.entry_uri))
            .then_some(session)
    }

    /// Capture the current on-disk state as the next diff's baseline — call when (re)starting
    /// a run, so the following edit diffs against exactly what that run observed. Also
    /// re-syncs the workspace: a member-set change re-points the inputs and re-resolves
    /// dependencies (the finding-9 reuse keeps unchanged members' parses memoized).
    pub fn rebaseline(&mut self) {
        let sources = self.scan();
        self.baselines = sources.iter().cloned().collect();
        self.cache = workspace::sync(&mut self.db, self.cache.take(), sources);
        self.reads = self.baseline_reads();
    }

    /// The files the linked program's expansion hooks reported reading, canonicalized — the
    /// non-`.noe` inputs the watcher must watch. Empty unless the project links and carries an
    /// expanding directive. Errors (unlinkable project, unreadable path) degrade to "no extra reads"
    /// rather than failing the session: the `.noe` watch still works, and the next successful link
    /// recaptures them.
    fn baseline_reads(&self) -> Vec<PathBuf> {
        let Some(cache) = &self.cache else {
            return Vec::new();
        };
        let Some(entry_index) = cache.source_uris.iter().position(|u| *u == self.entry_uri) else {
            return Vec::new();
        };
        // Cheap when nothing changed — salsa memoizes the link. Reads survive even a failed link
        // (an `@openapi` whose spec is missing still reported the path), which is what lets creating
        // that spec re-trigger the watcher.
        let link = noeta_db::linked_from(&self.db, cache.workspace, cache.programs[entry_index]);
        link.reads
            .iter()
            .map(|r| {
                // A read is resolved against the hook's `source_dir`, which in the editor's link is
                // the source's *URI* (`file://…`), not a plain path — because the salsa workspace
                // names its members by URI. Fold it back to a filesystem path so it can be compared
                // to the canonical paths `notify` reports, then canonicalize.
                let p = if r.starts_with("file://") {
                    uri_to_path(r).unwrap_or_else(|| PathBuf::from(r))
                } else {
                    PathBuf::from(r)
                };
                p.canonicalize().unwrap_or(p)
            })
            .collect()
    }

    /// The non-`.noe` files this session watches on the expansion hooks' behalf (canonicalized) —
    /// the watcher folds these into its watch set so a spec edit reaches [`impact_of_changes`].
    pub fn reads(&self) -> &[PathBuf] {
        &self.reads
    }

    /// The impact of a change burst (`changed` — the debounced paths the watcher collected)
    /// against the last baseline. Attributed edits return the impacted declarations in the
    /// linked program's qualified vocabulary — exactly what the runners' `--name` filter and
    /// their `TierFn` names speak. Everything unattributable degrades to [`Impact::All`].
    pub fn impact_of_changes(&mut self, changed: &[PathBuf]) -> Impact {
        let Some(cache) = &self.cache else {
            return Impact::All {
                reason: "the project's members cannot be read".into(),
            };
        };
        if changed.is_empty() {
            return Impact::All {
                reason: "the change could not be attributed to a file".into(),
            };
        }
        // The member set must be exactly the baseline's: a new or deleted `.noe` file re-links
        // the world (and re-baselines at the next run).
        let mut now = disk_noe_uris(&self.dir);
        now.sort();
        now.dedup();
        if now != cache.source_uris {
            return Impact::All {
                reason: "the project's module set changed".into(),
            };
        }

        // Attribute every changed path to a member, and read its current text.
        let mut edited: Vec<(usize, String)> = Vec::new(); // (member index, new text)
        let mut seen = BTreeSet::new();
        for path in changed {
            let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
            // A file an expansion hook read (an `@openapi` spec) is not a project member, but its
            // change still invalidates the generated members. The impact diff speaks the `.noe`
            // vocabulary and cannot narrow a spec change, so rerun everything — the honest,
            // correct answer, and the whole reason the watcher was taught to watch this file.
            if self.reads.contains(&canon) {
                return Impact::All {
                    reason: format!(
                        "a spec read by an expanding directive changed ({})",
                        canon.display()
                    ),
                };
            }
            let uri = path_to_uri(&canon);
            let Some(index) = cache.source_uris.iter().position(|u| *u == uri) else {
                return Impact::All {
                    reason: format!(
                        "a change outside the project's modules ({})",
                        canon.display()
                    ),
                };
            };
            if !seen.insert(index) {
                continue;
            }
            let Some(text) = uri_to_path(&uri).and_then(|p| std::fs::read_to_string(p).ok()) else {
                return Impact::All {
                    reason: format!("cannot read {}", canon.display()),
                };
            };
            edited.push((index, text));
        }

        // Push the new texts into the salsa inputs first (unchanged members backdate), so the
        // link, the check, and the workspace tier set below all see the post-edit project.
        for (index, text) in &edited {
            cache.programs[*index]
                .set_text(&mut self.db)
                .to(text.clone());
        }
        let cache = self.cache.as_ref().expect("present: checked above");

        // Per-file diffs → qualified seeds.
        let tier_set = noeta_lexer::TextTiers::with(
            noeta_db::workspace_text_tiers(&self.db, cache.workspace)
                .iter()
                .cloned(),
        );
        let mut seeds: BTreeSet<String> = BTreeSet::new();
        let mut members: BTreeSet<String> = BTreeSet::new();
        for (index, new_text) in &edited {
            let uri = &cache.source_uris[*index];
            let file = uri_to_path(uri)
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| uri.clone());
            let Some(old_text) = self.baselines.get(uri) else {
                return Impact::All {
                    reason: format!("{file}: no baseline to diff against"),
                };
            };
            if old_text == new_text {
                continue; // an event without a byte change (editors touch files)
            }
            let edition = *cache.programs[*index].edition(&self.db);
            match diff_file(old_text, new_text, edition, &tier_set) {
                FileDiff::All(reason) => {
                    return Impact::All {
                        reason: format!("{file}: {reason}"),
                    };
                }
                FileDiff::Names { names, namespace } => {
                    for name in names {
                        // A dotted per-file name is `Type.method` (qualification is the
                        // linker's); its member seeds the dynamic-edge fallback.
                        if let Some((_, member)) = name.split_once('.') {
                            members.insert(member.to_string());
                        }
                        seeds.insert(match &namespace {
                            Some(prefix) => format!("{prefix}.{name}"),
                            None => name,
                        });
                    }
                }
            }
        }
        if seeds.is_empty() {
            return Impact::Decls(Vec::new());
        }

        // The linked program — what the runner executes — then tiers, check, graph, closure.
        // The entry was a member at construction, but a deletion + rebaseline can remove it.
        let Some(entry_index) = cache.source_uris.iter().position(|u| *u == self.entry_uri) else {
            return Impact::All {
                reason: "the entry left the project".into(),
            };
        };
        let entry_program = cache.programs[entry_index];
        let link = noeta_db::linked_from(&self.db, cache.workspace, entry_program);
        let linked = match &link.program {
            Ok(program) => program,
            Err(_) => {
                return Impact::All {
                    reason: "the project does not link".into(),
                };
            }
        };
        let tier_names = declared_tiers(linked);
        let tier_refs: Vec<&str> = tier_names.iter().map(String::as_str).collect();
        let activated = noeta_check::activate_tiers(linked, &tier_refs);
        let checked = noeta_check::check_all_with(
            &activated.program,
            noeta_check::CheckOptions {
                // The span→type index resolves method receivers to `Function` edges (more
                // precise than the member-name fallback alone) — cheap at dev-loop rate.
                record_expr_types: true,
                editions: noeta_db::workspace_editions(&self.db, cache.workspace),
                packages: noeta_db::workspace_packages(&self.db, cache.workspace),
                ..noeta_check::CheckOptions::default()
            },
        );
        if noeta_diagnostics::has_errors(
            activated
                .diagnostics
                .iter()
                .chain(checked.diagnostics.iter()),
        ) {
            return Impact::All {
                reason: "the edit does not check".into(),
            };
        }
        // Every source's text by SourceId — `WorkspaceCache::sources_with` yields in SourceId order,
        // the same assignment `workspace::sync` made — for the graph's call-site syntax probes. This
        // link's generated sources are included: a call written by an expansion is a call, and a
        // short vector would silently drop it from the graph and so from the impact closure.
        let texts: Vec<&str> = cache
            .sources_with(&link.expansions)
            .map(|s| s.text(&self.db))
            .collect();
        let graph = callgraph::build(&activated.program, &checked.expr_types, &texts);
        match reverse_closure(&graph, seeds, members) {
            Ok(impacted) => Impact::Decls(impacted.into_iter().collect()),
            Err(reason) => Impact::All { reason },
        }
    }

    /// The ordered `(uri, text)` scan of the watched directory (unreadable files read as
    /// empty, like the editor's scan — the parse then fails and the diff degrades soundly).
    fn scan(&self) -> Vec<(String, String)> {
        let mut uris = disk_noe_uris(&self.dir);
        uris.sort();
        uris.dedup();
        uris.into_iter()
            .map(|uri| {
                let text = uri_to_path(&uri)
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .unwrap_or_default();
                (uri, text)
            })
            .collect()
    }
}

/// The non-`.noe` files an entry's expansion hooks read — the spec-watch set for the `run`/`serve`
/// watch modes, which build no [`ImpactSession`] of their own (only `test`/`bench` narrow by
/// impact). Links once and reports the reads, discarding the session. Empty when the entry cannot
/// anchor a project or declares no expanding directive.
pub fn spec_reads(entry: &Path) -> Vec<PathBuf> {
    ImpactSession::new(entry)
        .map(|s| s.reads().to_vec())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seed the process-global std registry — the checker behind the engine resolves std/tier
    /// names through it, and a filtered test run must not depend on a sibling test having
    /// seeded first. Idempotent, so per-test calls across parallel threads are safe.
    fn seed() {
        noeta_stdlib::registry::default_seeded();
    }

    fn decls(impact: Impact) -> Vec<String> {
        match impact {
            Impact::Decls(d) => d,
            Impact::All { reason } => panic!("expected attributed impact, got All: {reason}"),
        }
    }

    const V1: &str = "fn leaf(): int { return 1; }\n\
                      fn mid(): int { return leaf(); }\n\
                      fn other(): int { return 2; }\n\
                      @test fn t_mid(): void { assert(mid() == 1); }\n\
                      @test fn t_other(): void { assert(other() == 2); }\n";

    #[test]
    fn a_leaf_edit_impacts_exactly_its_reverse_closure() {
        seed();
        // leaf changed → mid (calls leaf) → t_mid (calls mid). t_other untouched.
        let v2 = V1.replace("return 1;", "return 0 + 1;");
        assert_eq!(
            decls(impact_of_edit(V1, &v2, noeta_lexer::Edition::DEFAULT)),
            vec!["leaf".to_string(), "mid".to_string(), "t_mid".to_string()]
        );
    }

    #[test]
    fn a_test_body_edit_impacts_only_that_test() {
        seed();
        let v2 = V1.replace("assert(other() == 2);", "assert(other() == 1 + 1);");
        assert_eq!(
            decls(impact_of_edit(V1, &v2, noeta_lexer::Edition::DEFAULT)),
            vec!["t_other".to_string()]
        );
    }

    #[test]
    fn a_formatting_edit_impacts_nothing() {
        seed();
        let v2 = V1.replace(
            "fn other(): int { return 2; }",
            "fn other(): int  { return 2; }",
        );
        assert_eq!(
            decls(impact_of_edit(V1, &v2, noeta_lexer::Edition::DEFAULT)),
            Vec::<String>::new()
        );
    }

    #[test]
    fn unattributable_edits_degrade_to_all_with_a_reason() {
        seed();
        // A signature change.
        let v2 = V1.replace("fn leaf(): int", "fn leaf(n: int): int");
        let Impact::All { reason } = impact_of_edit(V1, &v2, noeta_lexer::Edition::DEFAULT) else {
            panic!("a signature change is unattributable");
        };
        assert!(reason.contains("leaf"), "{reason}");
        // A top-level statement change.
        let v3 = format!("{V1}echo mid()\n");
        assert!(matches!(
            impact_of_edit(V1, &v3, noeta_lexer::Edition::DEFAULT),
            Impact::All { .. }
        ));
        // Red code.
        let v4 = V1.replace("return leaf();", "return leaf() * \"boom\";");
        let Impact::All { reason } = impact_of_edit(V1, &v4, noeta_lexer::Edition::DEFAULT) else {
            panic!("red code is unattributable");
        };
        assert!(reason.contains("check"), "{reason}");
    }

    #[test]
    fn a_reference_edge_counts_like_a_call() {
        seed();
        // `apply` takes leaf as a VALUE; the closure walks the reference edge.
        let v1 = "fn leaf(): int { return 1; }\n\
                  fn apply(f: () -> int): int { return f(); }\n\
                  @test fn t(): void { assert(apply(leaf) == 1); }\n";
        let v2 = v1.replace("return 1;", "return 2 - 1;");
        assert_eq!(
            decls(impact_of_edit(v1, &v2, noeta_lexer::Edition::DEFAULT)),
            vec!["leaf".to_string(), "t".to_string()]
        );
    }

    #[test]
    fn a_method_edit_impacts_its_calling_tests() {
        seed();
        let v1 = "struct Counter {\n    n: int\n\n    fn bump(): int { return self.n + 1; }\n}\n\
                  @test fn t_bump(): void {\n    c = Counter { n: 1 }\n    assert(c.bump() == 2);\n}\n\
                  @test fn t_none(): void { assert(true); }\n";
        let v2 = v1.replace("return self.n + 1;", "return 1 + self.n;");
        assert_eq!(
            decls(impact_of_edit(v1, &v2, noeta_lexer::Edition::DEFAULT)),
            vec!["Counter.bump".to_string(), "t_bump".to_string()]
        );
    }

    // ------------------------------------------------------------ the whole-project engine

    /// A throwaway project directory (fresh per test — the loader treats every sibling `.noe`
    /// as a module, so tests must not share directories) — and fresh per *process* too, which
    /// `/tmp/noeta_impact_test_<name>` was not: every checkout and every concurrent test binary
    /// shared it, and each opened by `remove_dir_all`ing it. The guard comes back with the
    /// directory; dropping it removes the tree.
    fn project(name: &str, files: &[(&str, &str)]) -> noeta_test_temp::TempDir {
        let dir = noeta_test_temp::TempDir::new(&format!("impact-{name}"));
        for (file, text) in files {
            std::fs::write(dir.join(file), text).expect("write test module");
        }
        dir
    }

    fn write(dir: &Path, file: &str, text: &str) {
        std::fs::write(dir.join(file), text).expect("rewrite test module");
    }

    const LIB: &str = "namespace App.Lib;\n\
                       pub fn add(a: int, b: int): int { return a + b; }\n\
                       pub fn stray(): int { return 9; }\n";
    const APP: &str = "use App.Lib.add;\n\
                       fn compose(n: int): int { return add(n, 1); }\n\
                       @test fn t_add(): void { assert(compose(1) == 2); }\n\
                       @test fn t_other(): void { assert(true); }\n";

    #[test]
    fn a_lib_edit_narrows_to_the_importing_tests() {
        seed();
        let dir = project("lib_narrows", &[("lib.noe", LIB), ("app.noe", APP)]);
        let mut session = ImpactSession::new(&dir.join("app.noe")).expect("session builds");
        write(
            &dir,
            "lib.noe",
            &LIB.replace("return a + b;", "return b + a;"),
        );
        let impacted = decls(session.impact_of_changes(&[dir.join("lib.noe")]));
        // The changed decl carries its linked (qualified) identity; the closure reaches the
        // entry's caller and its test — and NOT the unrelated test.
        assert_eq!(
            impacted,
            vec![
                "App.Lib.add".to_string(),
                "compose".to_string(),
                "t_add".to_string()
            ]
        );
    }

    #[test]
    fn an_inert_lib_edit_impacts_nothing() {
        seed();
        let dir = project("lib_inert", &[("lib.noe", LIB), ("app.noe", APP)]);
        let mut session = ImpactSession::new(&dir.join("app.noe")).expect("session builds");
        write(
            &dir,
            "lib.noe",
            &LIB.replace("pub fn stray", "pub  fn stray"),
        );
        assert_eq!(
            decls(session.impact_of_changes(&[dir.join("lib.noe")])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn an_unimported_lib_fn_edit_reaches_no_tests() {
        seed();
        let dir = project("lib_unimported", &[("lib.noe", LIB), ("app.noe", APP)]);
        let mut session = ImpactSession::new(&dir.join("app.noe")).expect("session builds");
        write(&dir, "lib.noe", &LIB.replace("return 9;", "return 10 - 1;"));
        // `stray` is outside the entry's import closure: no linked node carries it, so the
        // impacted set is just the (qualified) seed — the runner's `--name` filter then
        // matches no test and nothing reruns.
        assert_eq!(
            decls(session.impact_of_changes(&[dir.join("lib.noe")])),
            vec!["App.Lib.stray".to_string()]
        );
    }

    #[test]
    fn a_signature_change_in_a_lib_degrades_to_all_with_the_file() {
        seed();
        let dir = project("lib_signature", &[("lib.noe", LIB), ("app.noe", APP)]);
        let mut session = ImpactSession::new(&dir.join("app.noe")).expect("session builds");
        write(
            &dir,
            "lib.noe",
            &LIB.replace("fn add(a: int, b: int)", "fn add(a: int, b: int, c: int)"),
        );
        let Impact::All { reason } = session.impact_of_changes(&[dir.join("lib.noe")]) else {
            panic!("a signature change is unattributable");
        };
        assert!(
            reason.contains("lib.noe") && reason.contains("add"),
            "{reason}"
        );
    }

    #[test]
    fn a_change_outside_the_members_degrades_to_all() {
        seed();
        let dir = project("outside", &[("lib.noe", LIB), ("app.noe", APP)]);
        let mut session = ImpactSession::new(&dir.join("app.noe")).expect("session builds");
        std::fs::write(dir.join("noeta.toml"), "[package]\n").expect("write manifest");
        assert!(matches!(
            session.impact_of_changes(&[dir.join("noeta.toml")]),
            Impact::All { .. }
        ));
    }

    #[test]
    fn a_cross_module_method_edit_impacts_its_calling_test() {
        seed();
        let lib = "namespace App.Lib;\n\
                   pub class Counter {\n    pub n: int\n\n    fn bump(): int { return self.n + 1; }\n}\n";
        let app = "use App.Lib.Counter;\n\
                   @test fn t_bump(): void {\n    c = Counter { n: 1 }\n    assert(c.bump() == 2);\n}\n\
                   @test fn t_none(): void { assert(true); }\n";
        let dir = project("method_cross", &[("lib.noe", lib), ("app.noe", app)]);
        let mut session = ImpactSession::new(&dir.join("app.noe")).expect("session builds");
        write(
            &dir,
            "lib.noe",
            &lib.replace("return self.n + 1;", "return 1 + self.n;"),
        );
        let impacted = decls(session.impact_of_changes(&[dir.join("lib.noe")]));
        assert!(
            impacted.contains(&"App.Lib.Counter.bump".to_string())
                && impacted.contains(&"t_bump".to_string())
                && !impacted.contains(&"t_none".to_string()),
            "{impacted:?}"
        );
    }

    #[test]
    fn consecutive_edits_diff_against_the_last_baseline_until_rebaselined() {
        seed();
        let dir = project("baseline_cycle", &[("lib.noe", LIB), ("app.noe", APP)]);
        let mut session = ImpactSession::new(&dir.join("app.noe")).expect("session builds");
        // First edit: inert (formatting between declarations).
        write(
            &dir,
            "lib.noe",
            &LIB.replace("pub fn stray", "pub  fn stray"),
        );
        assert_eq!(
            decls(session.impact_of_changes(&[dir.join("lib.noe")])),
            Vec::<String>::new()
        );
        // A second edit on top also changes behavior — the diff runs against the ORIGINAL
        // baseline (no run happened in between), so the behavioral change is caught.
        write(
            &dir,
            "lib.noe",
            &LIB.replace("pub fn stray", "pub  fn stray")
                .replace("return a + b;", "return b + a;"),
        );
        assert_eq!(
            decls(session.impact_of_changes(&[dir.join("lib.noe")])),
            vec![
                "App.Lib.add".to_string(),
                "compose".to_string(),
                "t_add".to_string()
            ]
        );
        // After a rebaseline (a run consumed the state), the same on-disk content is inert.
        session.rebaseline();
        assert_eq!(
            decls(session.impact_of_changes(&[dir.join("lib.noe")])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_new_module_degrades_to_all() {
        seed();
        let dir = project("new_module", &[("lib.noe", LIB), ("app.noe", APP)]);
        let mut session = ImpactSession::new(&dir.join("app.noe")).expect("session builds");
        write(&dir, "extra.noe", "namespace App.Extra;\n");
        let Impact::All { reason } = session.impact_of_changes(&[dir.join("extra.noe")]) else {
            panic!("a module-set change is unattributable");
        };
        assert!(reason.contains("module set"), "{reason}");
    }
}
