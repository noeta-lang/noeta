//! Shared **disk-backed workspace construction** (multi-file impact arc): build and refresh a
//! salsa [`Workspace`] input — members, dependency modules, per-package editions — from an
//! ordered `(uri, text)` member list, reusing the live inputs in place across refreshes.
//!
//! Extracted from the [`DocumentStore`](crate::DocumentStore) so every disk-shaped consumer
//! drives ONE construction path: the editor session overlays its open buffers on top of the
//! directory scan, the watch-mode impact engine ([`crate::impact`]) has no buffers and feeds the
//! scan straight through — and neither re-implements dependency resolution or input reuse. The
//! store remains the buffer/cancellation/revision owner; this module owns only "sources in →
//! live salsa inputs out".
//!
//! The reuse discipline is the finding-9 contract, unchanged by the move: an input is never
//! abandoned just because a sibling appeared or vanished — its id/text are updated in place, and
//! only genuinely new files get new inputs, so downstream memoization survives file-set churn.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use noeta_db::{DepModule, LangDatabase, SourceProgram, Workspace, WorkspaceUses};
use noeta_span::SourceId;
use salsa::Setter as _;

/// What **category** of source a [`SourceId`] names within a [`WorkspaceCache`].
///
/// The IDE's addressing was, until this type existed, a *range convention*: an id below
/// `programs.len()` meant a member, anything above meant a dependency, and the caller subtracted.
/// That rule was open-coded at nine sites, and when it drifted it did not crash — it attributed a
/// span to the WRONG FILE. Naming the category makes the decision explicit at each site.
///
/// The **third** category, [`SourceKind::Expansion`], is why the predicate discipline exists: call
/// sites ask [`SourceRef::is_member`] rather than matching every arm, so the variant slotted in
/// without a nine-site sweep and the sites that must exclude non-members excluded it by
/// construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind {
    /// A workspace member: a `.noe` file of the directory the user has open.
    Member,
    /// A module of a resolved dependency package (package-manager P2.1c).
    Dependency,
    /// A source **generated during linking** by compile-time directive expansion — the text an
    /// `@openapi("petstore.yaml")` hook returned, wrapped in the declaration it decorates.
    ///
    /// Unlike the other two it has no file: no URI to open, no salsa input, and no id of its own
    /// until an entry links. Its ids therefore belong to *one link*, not to the workspace (two
    /// entries of one directory each number their expansions from the same first unused id), which
    /// is why they are never in [`WorkspaceCache::source`]'s own tables — a caller resolving a span
    /// from a link passes that link's expansions to [`WorkspaceCache::source_with`].
    Expansion,
}

/// The [`SourceId`] the **first dependency module** carries, given the member count — the single
/// place the member/dependency id layout is written down. Dependency ids continue past the members,
/// so every id→source lookup ([`WorkspaceCache::source`]), every dependency id
/// ([`WorkspaceCache::dep_source_id`]) and the origin handed to [`resolve_dep_modules`] derive from
/// here; nothing else may re-derive it. A third category would extend this function, not the ~nine
/// call sites that used to open-code it.
fn first_dep_id(member_count: usize) -> u32 {
    member_count as u32
}

/// One **resolved** source: its category, how it is named, and where its text lives.
///
/// Deliberately NOT a [`noeta_span::SourceMap`] entry: that type *owns* the source text, which a
/// `SourceRef` must never do — it always borrows from whoever owns the text already. For a member or
/// a dependency module that owner is the salsa [`SourceProgram`] input, memoized per file, so an
/// owning copy would duplicate every open buffer and fight incrementality. For an **expansion** the
/// owner is the `linked_from` memo that generated the text (see [`noeta_db::LinkedProgram`]); it has
/// no file and no input to read through, so the text is borrowed straight out of that memo.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SourceRef<'a> {
    pub(crate) kind: SourceKind,
    /// How this source is addressed: a `file:` URI for a member or a dependency module. For an
    /// [`SourceKind::Expansion`] it is the generated source's **display name**
    /// (`PetStore ⟨@openapi "petstore.yaml"⟩`) and **not openable** — showing generated code in the
    /// editor needs a virtual-document scheme, which is a separate arc. A site that turns this into
    /// something the editor navigates to must therefore keep to members and dependencies (which is
    /// what resolving through [`WorkspaceCache::source`], with no expansions, gives it).
    pub(crate) uri: &'a str,
    body: SourceBody<'a>,
}

/// Where a [`SourceRef`]'s text lives — the one difference between a source backed by a file and one
/// generated during linking.
#[derive(Debug, Clone, Copy)]
enum SourceBody<'a> {
    /// A salsa input; the text is read through the db, memoized per file.
    Input(SourceProgram),
    /// Text owned by the `linked_from` memo that generated it.
    Generated(&'a str),
}

impl<'a> SourceRef<'a> {
    /// Whether this source is a workspace **member** (as opposed to any non-member category — a
    /// dependency module or an expansion). The question the exclusion sites actually ask, phrased so
    /// a new category is excluded by default rather than silently included.
    pub(crate) fn is_member(&self) -> bool {
        matches!(self.kind, SourceKind::Member)
    }

    /// The salsa input behind this source, or `None` for an [`SourceKind::Expansion`] — which has
    /// none, and deliberately: minting an input per expansion would leak a slot per expansion ever
    /// produced (salsa 0.27 cannot delete an input; see [`noeta_db::release_source`]).
    pub(crate) fn input(&self) -> Option<SourceProgram> {
        match self.body {
            SourceBody::Input(program) => Some(program),
            SourceBody::Generated(_) => None,
        }
    }

    /// This source's text, read through salsa for a file-backed source and borrowed out of the
    /// generating memo for an expansion. Both borrows live as long as the db borrow.
    pub(crate) fn text(self, db: &'a dyn salsa::Database) -> &'a str {
        match self.body {
            SourceBody::Input(program) => program.text(db).as_str(),
            SourceBody::Generated(text) => text,
        }
    }
}

/// One **directory's** workspace (audit-4 finding 6): the salsa [`Workspace`] input over the
/// directory's `.noe` members — sorted by path, which is the stable
/// [`SourceId`](noeta_span::SourceId) assignment every consumer of the directory agrees on — and,
/// per `SourceId`, the member's URI and salsa input. Member texts are updated in place on every
/// change; a file-*set* change **reuses** the existing inputs by URI (text/id updated, only
/// genuinely new files get new inputs — the finding-9 input-growth fix).
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceCache {
    pub(crate) workspace: Workspace,
    /// Per `SourceId`: the member's URI, sorted by path. Maps a merged-program span back to the
    /// file it belongs to, for cross-file diagnostics and navigation. Members only — the reuse
    /// fast-path compares this against a fresh directory scan; dependency modules live in
    /// `dep_uris`/`dep_programs`.
    pub(crate) source_uris: Vec<String>,
    /// Per `SourceId`: the salsa input, for in-place text updates.
    pub(crate) programs: Vec<SourceProgram>,
    /// Dependency-package modules (package-manager P2.1c); their [`SourceId`]s continue past the
    /// members, per [`first_dep_id`]. Kept apart from `source_uris`/`programs` so the per-keystroke
    /// reuse check and text-update loop stay over members only, while cross-package navigation still
    /// maps a dependency span back to its file. **Address these through [`WorkspaceCache::source`]
    /// / [`WorkspaceCache::dep_source_id`]**, never by open-coding the id offset.
    pub(crate) dep_uris: Vec<String>,
    pub(crate) dep_programs: Vec<SourceProgram>,
    /// The [`DepModule`] inputs backing `workspace.dep_modules`, kept so a file-set rescan can
    /// reuse them by URI instead of abandoning them (finding 9).
    pub(crate) dep_modules: Vec<DepModule>,
    /// **Tombstoned member inputs** — [`SourceProgram`]s whose files were deleted (audit F9 residual
    /// a). salsa 0.27 cannot free an input slot, so rather than abandon them (leaking the slot *and*
    /// their downstream memos), a deleted member is emptied via [`noeta_db::release_source`] — text
    /// reclaimed, fat memos overwritten with empty-program equivalents — and parked here. The next
    /// genuinely-new file reuses a tombstone instead of minting a fresh input, bounding the salsa
    /// input table by the directory's *concurrent* `.noe` high-water mark rather than the total ever
    /// created/deleted across a session.
    pub(crate) tombstones: Vec<SourceProgram>,
    /// A **surfaced** dependency-resolution failure (audit-5 #7): a hard `noeta-pm` failure —
    /// a trust refusal, a version conflict, a broken manifest, a lockfile drift — recorded here
    /// so the consumer reports the real cause instead of silently degrading to "no dependencies"
    /// (which showed up only as inexplicable unknown-import errors). `None` when resolution
    /// succeeded or failed for a routine reason (no manifest, network/IO), where the quiet
    /// degrade is the right behavior.
    pub(crate) dep_error: Option<noeta_pm::PmError>,
}

impl WorkspaceCache {
    /// The [`SourceId`] of the `index`-th dependency module, or `None` if there is no such module.
    pub(crate) fn dep_source_id(&self, index: usize) -> Option<SourceId> {
        (index < self.dep_programs.len())
            .then(|| SourceId(first_dep_id(self.programs.len()) + index as u32))
    }

    /// The [`SourceId`] the **first expansion** of a link carries: past every member and every
    /// dependency module. The second half of the id layout `first_dep_id` opens, and the editor's
    /// statement of the same rule `noeta_db::linked_from` links under (it takes the first unused id
    /// as the member count plus the dependency-module count). Nothing may re-derive it.
    fn first_expansion_id(&self) -> u32 {
        first_dep_id(self.programs.len()) + self.dep_programs.len() as u32
    }

    /// **The** `SourceId` → source lookup, for the sources the *workspace* owns: members and
    /// dependency modules. `None` for an id this workspace does not map — an id from another
    /// workspace, one past every source, or one belonging to a link's generated code — which every
    /// caller must handle explicitly instead of falling into a range and being attributed to the
    /// wrong file.
    ///
    /// A caller holding a span that may come from generated code resolves it through
    /// [`Self::source_with`] instead, passing the expansions of the link that produced the span.
    /// This one is not merely the `expansions = &[]` convenience it is written as: a site that turns
    /// a resolved source into a location the editor opens **must** use it, because an expansion has
    /// no openable URI.
    pub(crate) fn source(&self, id: SourceId) -> Option<SourceRef<'_>> {
        self.source_with(id, &[])
    }

    /// [`Self::source`] extended with **one link's expansions** — the generated sources
    /// `noeta_db::linked_from` produced for a particular entry, whose ids continue past
    /// [`Self::first_expansion_id`].
    ///
    /// The expansions are a parameter rather than a field of the cache because they belong to a
    /// link, not to a workspace: every entry of a directory links on its own and numbers its
    /// expansions from the same first unused id, so one flat per-workspace table would attribute an
    /// id to whichever entry happened to fill it — precisely the wrong-file failure naming the
    /// categories was introduced to end. A caller therefore passes the expansions of the link the
    /// span came from, or none.
    pub(crate) fn source_with<'a>(
        &'a self,
        id: SourceId,
        expansions: &'a [noeta_loader::ExpandedSource],
    ) -> Option<SourceRef<'a>> {
        let idx = id.0 as usize;
        let dep_origin = first_dep_id(self.programs.len()) as usize;
        if idx < dep_origin {
            return Some(SourceRef {
                kind: SourceKind::Member,
                uri: self.source_uris.get(idx)?,
                body: SourceBody::Input(*self.programs.get(idx)?),
            });
        }
        let expansion_origin = self.first_expansion_id() as usize;
        if idx < expansion_origin {
            let dep = idx - dep_origin;
            return Some(SourceRef {
                kind: SourceKind::Dependency,
                uri: self.dep_uris.get(dep)?,
                body: SourceBody::Input(*self.dep_programs.get(dep)?),
            });
        }
        let generated = &expansions.get(idx - expansion_origin)?.source;
        Some(SourceRef {
            kind: SourceKind::Expansion,
            uri: generated.name(),
            body: SourceBody::Generated(generated.text()),
        })
    }

    /// Every source this workspace maps, in [`SourceId`] order — so `nth` element IS `SourceId(n)`.
    /// What the `SourceId`-indexed text vectors the call graph consumes are built from.
    pub(crate) fn sources(&self) -> impl Iterator<Item = SourceRef<'_>> {
        self.sources_with(&[])
    }

    /// [`Self::sources`] continued through one link's expansions, still in [`SourceId`] order — so a
    /// span in generated code indexes the id-keyed vectors built from this exactly as one in a
    /// hand-written file does.
    pub(crate) fn sources_with<'a>(
        &'a self,
        expansions: &'a [noeta_loader::ExpandedSource],
    ) -> impl Iterator<Item = SourceRef<'a>> {
        (0..self.first_expansion_id() as usize + expansions.len())
            .filter_map(move |i| self.source_with(SourceId(i as u32), expansions))
    }

    /// The workspace **member** at `uri`, with the [`SourceId`] it carries. `None` if `uri` is not a
    /// member — a dependency module's URI deliberately does NOT resolve here.
    pub(crate) fn find_member(&self, uri: &str) -> Option<(SourceId, SourceRef<'_>)> {
        let idx = self.source_uris.iter().position(|u| u == uri)?;
        let id = SourceId(idx as u32);
        Some((id, self.source(id)?))
    }

    /// The **dependency module** at `uri`, with the [`SourceId`] it carries. `None` if no resolved
    /// dependency module has that URI.
    pub(crate) fn find_dep(&self, uri: &str) -> Option<(SourceId, SourceRef<'_>)> {
        let idx = self.dep_uris.iter().position(|u| u == uri)?;
        let id = self.dep_source_id(idx)?;
        Some((id, self.source(id)?))
    }

    /// Reclaim every input's resident content when the whole workspace is torn down — its last open
    /// document closed (audit F9 residual a). salsa 0.27 cannot free the input slots themselves, but
    /// [`noeta_db::release_source`] releases each member and dependency source's text and overwrites
    /// its fat downstream memos with empty-program equivalents, so a closed directory stops holding
    /// its whole analysis resident. (Already-emptied tombstones are skipped — they hold nothing.)
    pub(crate) fn release_all(&self, db: &mut LangDatabase) {
        let ws = self.workspace;
        // Members and dependency modules only — `sources()` yields no expansion (they belong to a
        // link, not to the workspace), and an expansion has no input to release in any case.
        for program in self.sources().filter_map(|src| src.input()) {
            noeta_db::release_source(db, ws, program);
        }
    }
}

/// The resolved dependency modules for a workspace (package-manager P2.1c): the salsa
/// [`DepModule`] inputs the `Workspace` links, plus — parallel-indexed — each module's URI and
/// salsa input so a cross-package definition span maps back to its file.
#[derive(Debug, Default)]
pub(crate) struct ResolvedDeps {
    pub(crate) modules: Vec<DepModule>,
    pub(crate) uris: Vec<String>,
    pub(crate) programs: Vec<SourceProgram>,
    /// The whole program's per-package `@name` resolution tables (`[directives]`/`[tiers]`), from the
    /// same query-path graph resolve that produced the modules. Threaded onto the [`Workspace`] input
    /// so the editor lexes a package's renamed text tiers verbatim (per-package tier-naming arc, 3g).
    pub(crate) package_uses: noeta_span::PackageUses,
    /// A hard resolution failure worth the user's attention (see [`WorkspaceCache::dep_error`]).
    pub(crate) error: Option<noeta_pm::PmError>,
    /// Previously-resolved dependency sources that vanished from this resolution (finding a): their
    /// resident content is reclaimed by the caller via [`noeta_db::release_source`]. Not pooled —
    /// dependency inputs carry a [`DepModule`] wrapper, so the member tombstone pool cannot reuse
    /// them; the reuse-by-URI path already bounds the common dependency-churn case.
    pub(crate) deleted: Vec<SourceProgram>,
}

/// Build or update a workspace from the ordered `(uri, text)` member list, reusing `existing`'s
/// inputs in place. `None` means the member set is empty — the caller drops the workspace.
///
/// Three paths, in cost order: an unchanged file set updates each member's text in place (salsa
/// backdates the unchanged ones); a changed set **reuses the inputs by URI** across the new list
/// (id/text re-pointed, only genuinely new files get new inputs) and re-resolves dependencies;
/// a first build mints everything. The [`Workspace`] input itself is likewise updated in place
/// once created, so `(workspace, entry)`-keyed query memoization survives the refresh.
pub(crate) fn sync(
    db: &mut LangDatabase,
    existing: Option<WorkspaceCache>,
    sources: Vec<(String, String)>,
) -> Option<WorkspaceCache> {
    if sources.is_empty() {
        return None;
    }
    let uris: Vec<String> = sources.iter().map(|(u, _)| u.clone()).collect();

    // File set unchanged → update each member's text in place (salsa backdates unchanged ones).
    if let Some(cache) = existing.as_ref().filter(|cache| cache.source_uris == uris) {
        for (program, (_, text)) in cache.programs.iter().zip(&sources) {
            program.set_text(db).to(text.clone());
        }
        return existing;
    }

    // File set changed (or first build) → reuse existing inputs by URI, create only new ones.
    let mut old_by_uri: HashMap<String, SourceProgram> = match &existing {
        Some(cache) => cache
            .source_uris
            .iter()
            .cloned()
            .zip(cache.programs.iter().copied())
            .collect(),
        None => HashMap::new(),
    };
    // The tombstone pool a genuinely-new file draws from before minting a fresh input (finding a):
    // previously-deleted member inputs, already emptied by `release_source`.
    let mut pool: Vec<SourceProgram> = existing
        .as_ref()
        .map(|cache| cache.tombstones.clone())
        .unwrap_or_default();
    let programs: Vec<SourceProgram> = sources
        .iter()
        .enumerate()
        .map(|(id, (u, text))| match old_by_uri.remove(u) {
            Some(program) => {
                // The member moved in the sorted order (or kept its slot): re-point its id and
                // text; name and edition are functions of the URI and cannot have changed.
                program.set_id(db).to(id as u32);
                program.set_text(db).to(text.clone());
                program
            }
            // A genuinely-new file: repurpose a tombstoned input if one is parked (reclaiming the
            // slot), else mint. A repurposed tombstone points at a *different* URI, so — unlike the
            // reuse-by-URI path above — its name and edition change too.
            None => match pool.pop() {
                Some(tomb) => {
                    tomb.set_id(db).to(id as u32);
                    tomb.set_name(db).to(u.clone());
                    tomb.set_text(db).to(text.clone());
                    tomb.set_edition(db).to(edition_of_uri(u));
                    // A repurposed tombstone points at a different file, so its derived module path
                    // changes with it.
                    tomb.set_module_path(db)
                        .to(noeta_db::DerivedPath(derived_path_of_uri(u)));
                    tomb
                }
                None => SourceProgram::new(
                    db,
                    id as u32,
                    u.clone(),
                    text.clone(),
                    edition_of_uri(u),
                    noeta_db::DerivedPath(derived_path_of_uri(u)),
                ),
            },
        })
        .collect();
    // Members that vanished from the file set: whatever is left in `old_by_uri`. Collected now,
    // released (and re-parked as tombstones) once the workspace input reflects the new member set.
    let deleted_members: Vec<SourceProgram> = old_by_uri.into_values().collect();
    // Dependency packages (package-manager P2.1c): resolve the directory's deps and add each
    // dep module as a `DepModule` input (SourceIds continue past the members), so cross-package
    // `use <dep-key>.…` resolves exactly as the CLI resolves it. Every member of one directory
    // shares one manifest, so one resolution serves them all.
    let mut deps = resolve_dep_modules(db, existing.as_ref(), &uris, first_dep_id(programs.len()));
    // Dependency sources whose modules vanished from the resolution (finding a): reclaimed below.
    let deleted_deps = std::mem::take(&mut deps.deleted);
    let mut cache = match existing {
        Some(mut cache) => {
            cache.workspace.set_members(db).to(programs.clone());
            cache.workspace.set_dep_modules(db).to(deps.modules.clone());
            // Backdates when the dependency graph's `@name` tables are unchanged (a member-text edit),
            // so re-syncing an open directory does not invalidate the per-package text-tier lexes.
            cache
                .workspace
                .set_package_uses(db)
                .to(WorkspaceUses(deps.package_uses.clone()));
            cache.source_uris = uris;
            cache.programs = programs;
            cache.dep_uris = deps.uris;
            cache.dep_programs = deps.programs;
            cache.dep_modules = deps.modules;
            cache.dep_error = deps.error;
            cache.tombstones = pool;
            cache
        }
        None => WorkspaceCache {
            workspace: Workspace::new(
                db,
                programs.clone(),
                deps.modules.clone(),
                WorkspaceUses(deps.package_uses.clone()),
            ),
            source_uris: uris,
            programs,
            dep_uris: deps.uris,
            dep_programs: deps.programs,
            dep_modules: deps.modules,
            dep_error: deps.error,
            tombstones: pool,
        },
    };
    // Now that the `Workspace` input carries the new member set, reclaim the deleted members'
    // resident content (text + fat downstream memos) and park them for reuse (finding a). Deleted
    // dependency sources are reclaimed too, but not pooled — the member pool reuses member inputs.
    let ws = cache.workspace;
    for src in deleted_members {
        noeta_db::release_source(db, ws, src);
        cache.tombstones.push(src);
    }
    for src in deleted_deps {
        noeta_db::release_source(db, ws, src);
    }
    Some(cache)
}

/// Resolve the directory's dependency packages into salsa [`DepModule`] inputs (package-manager
/// P2.1c), each source given a [`SourceId`](noeta_span::SourceId) continuing from `first_id`
/// (past the members) so its spans stay distinct and map back to its file for cross-package
/// navigation. Resolution reuses the CLI's `noeta-pm` walk — path deps read locally, git deps
/// served from the package store (materialized by a prior CLI run) — so every consumer sees the
/// same cross-package program. A **routine** resolution failure (not a project, an offline
/// registry/network, a filesystem hiccup) degrades to no dependencies rather than breaking the
/// workspace; the members' own analysis still proceeds. A **hard** failure — a trust refusal, a
/// version conflict, a broken manifest, a lockfile drift — is recorded on the returned deps so
/// the consumer surfaces the real cause (audit-5 #7). Existing dep inputs (from a previous
/// rescan, via `previous`) are reused by URI — updated in place, never abandoned (finding 9).
fn resolve_dep_modules(
    db: &mut LangDatabase,
    previous: Option<&WorkspaceCache>,
    member_uris: &[String],
    first_id: u32,
) -> ResolvedDeps {
    let mut deps = ResolvedDeps::default();
    let Some(entry_path) = member_uris.first().and_then(|uri| uri_to_path(uri)) else {
        return deps;
    };
    // The query-path graph resolve (no lockfile refresh — opening a file must not rewrite
    // `noeta.lock`): it yields both the dependency packages AND the per-package `@name` tables
    // (`package_uses`), so a renamed text tier lexes verbatim in the editor exactly as under the CLI.
    let graph = match noeta_pm::graph::resolve_graph_query(&entry_path) {
        Ok(graph) => graph,
        // Not a project / environmental: the quiet degrade IS the right behavior (formatting and
        // single-file analysis must not nag about a flaky network).
        Err(
            noeta_pm::PmError::NoManifest(_)
            | noeta_pm::PmError::Network(_)
            | noeta_pm::PmError::Io(_),
        ) => {
            // Degraded to no dependencies: any previously-resolved dep sources are now orphaned —
            // hand them back for reclamation (finding a).
            deps.deleted = previous.map(|c| c.dep_programs.clone()).unwrap_or_default();
            return deps;
        }
        // A hard failure the user must see: trust/conflict/manifest/lock/auth/native-build.
        Err(err) => {
            deps.error = Some(err);
            deps.deleted = previous.map(|c| c.dep_programs.clone()).unwrap_or_default();
            return deps;
        }
    };
    let packages = graph.packages;
    // The per-package `@name` tables travel onto the workspace even when the package has no linkable
    // modules (the root's own `[tiers]` bindings live here), so carry them before the module walk.
    deps.package_uses = graph.package_uses;
    // Previous dep inputs by URI, for reuse.
    let mut old_by_uri: HashMap<String, (DepModule, SourceProgram)> = match previous {
        Some(cache) => cache
            .dep_uris
            .iter()
            .cloned()
            .zip(
                cache
                    .dep_modules
                    .iter()
                    .copied()
                    .zip(cache.dep_programs.iter().copied()),
            )
            .collect(),
        None => HashMap::new(),
    };
    let mut next_id = first_id;
    for package in &packages {
        let renames: Vec<String> = package
            .dep_renames
            .iter()
            .flat_map(|(local, global)| [local.clone(), global.clone()])
            .collect();
        for module in &package.modules {
            let uri = path_to_uri(Path::new(&module.name));
            let (dep, src) = match old_by_uri.remove(&uri) {
                Some((dep, src)) => {
                    src.set_id(db).to(next_id);
                    src.set_text(db).to(module.text.clone());
                    src.set_edition(db).to(package.edition);
                    src.set_module_path(db)
                        .to(noeta_db::DerivedPath(module.path.clone()));
                    dep.set_root(db).to(package.root.clone());
                    dep.set_key(db).to(package.key.clone());
                    dep.set_renames(db).to(renames.clone());
                    (dep, src)
                }
                None => {
                    let src = SourceProgram::new(
                        db,
                        next_id,
                        module.name.clone(),
                        module.text.clone(),
                        // The dependency package's own edition (typed end to end).
                        package.edition,
                        // …and the module path its location in that package derives.
                        noeta_db::DerivedPath(module.path.clone()),
                    );
                    let dep = DepModule::new(
                        db,
                        src,
                        package.root.clone(),
                        package.key.clone(),
                        renames.clone(),
                    );
                    (dep, src)
                }
            };
            next_id += 1;
            deps.modules.push(dep);
            deps.uris.push(uri);
            deps.programs.push(src);
        }
    }
    // Dep modules that were resolved before but are gone now: reclaim them (finding a).
    deps.deleted
        .extend(old_by_uri.into_values().map(|(_, src)| src));
    deps
}

/// The directory's on-disk `.noe` member URIs (unsorted — the caller owns ordering, because the
/// editor overlays open buffers before the sort). A directory that cannot be read is an empty
/// member set, not an error: the consumer degrades exactly as for an empty directory.
pub(crate) fn disk_noe_uris(dir: &Path) -> Vec<String> {
    // A directory inside a **package** contributes the whole package, walked as the compiler walks
    // it — otherwise the editor would analyze `src/main.noe` against an empty sibling pool while
    // `noeta run` links `src/deep/nested.noe` beside it, and report an unresolved import on a
    // program that compiles.
    if let Some(root) = noeta_pm::sources::package_root_of(dir) {
        return noeta_loader::read_package_modules(&root)
            .into_iter()
            .map(|m| path_to_uri(Path::new(&m.name)))
            .collect();
    }
    let mut uris = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(dir) {
        uris.extend(
            read_dir
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "noe"))
                .map(|p| path_to_uri(&p)),
        );
    }
    uris
}

/// Convert a `file:` document URI to a filesystem path. Returns `None` for any other scheme (e.g.
/// `untitled:`), which the caller treats as a lone, directory-less document. A minimal decoder: the
/// path component after `file://`, with `%`-escapes not yet decoded (paths with escaped bytes are
/// rare and degrade to a lone workspace, never a wrong file).
/// The module path a member file's **location** derives — its package's prefix plus its path inside
/// the package (namespace-derivation arc). `Declared` for a file in no package, and for a URI that is
/// not a local path at all: with no package there is no prefix, so the file's own `namespace`
/// declaration stands, exactly as before derivation.
fn derived_path_of_uri(uri: &str) -> noeta_loader::ModulePath {
    let Some(path) = uri_to_path(uri) else {
        return noeta_loader::ModulePath::Declared;
    };
    let Some(root) = noeta_pm::sources::package_root(&path) else {
        return noeta_loader::ModulePath::Declared;
    };
    noeta_loader::derive_module_path(&root.prefix, path.strip_prefix(&root.dir).unwrap_or(&path))
}

pub(crate) fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///abs` → `/abs`; a leading host (`file://host/p`) is not expected for local files.
    Some(PathBuf::from(rest))
}

/// The shared-workspace key of a document: its **directory** (`dir:<path>`) for a `file:` URI —
/// every document in one directory shares one [`WorkspaceCache`] — or the URI itself
/// (`lone:<uri>`) for a directory-less document (`untitled:` …), which forms a lone workspace.
pub(crate) fn workspace_key(uri: &str) -> String {
    match uri_to_path(uri).and_then(|p| p.parent().map(Path::to_path_buf)) {
        Some(dir) => format!("dir:{}", dir.display()),
        None => format!("lone:{uri}"),
    }
}

/// The language edition the document at `uri` is written against — its package's `edition`, read
/// from the nearest `noeta.toml` (the editions arc). Defaults to
/// [`Edition::DEFAULT`](noeta_lexer::Edition::DEFAULT) for a directory-less document (e.g.
/// `untitled:`) or a manifest-less file, so formatting/analysis of a lone buffer is unchanged. The
/// formatter and (later) analysis run under this so a future edition's grammar is parsed correctly.
pub(crate) fn edition_of_uri(uri: &str) -> noeta_lexer::Edition {
    uri_to_path(uri)
        .map(|p| noeta_pm::manifest::root_edition(&p))
        .unwrap_or_default()
}

/// The `file:` URI for a filesystem path — the inverse of [`uri_to_path`] for the paths it produces.
pub(crate) fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_db::LangDatabase;

    fn member(uri: &str, ns: &str, name: &str) -> (String, String) {
        (
            uri.to_string(),
            format!("namespace {ns}\npub fn {name}(): int {{ return 1 }}\n"),
        )
    }

    /// One expansion as a link hands it back: a generated source at `id`, blamed on a directive in
    /// the first member. (The origin span is what the diagnostics view re-attributes to; the
    /// addressing tests only need it to be well-formed.)
    fn generated(id: SourceId, name: &str, text: &str) -> noeta_loader::ExpandedSource {
        noeta_loader::ExpandedSource {
            source: noeta_span::Source::new(id, name, text),
            origin: noeta_span::Span::new_in(SourceId(0), 0, 3),
        }
    }

    /// A cache with `members` member sources and `deps` dependency modules, wired exactly as
    /// [`sync`]/[`resolve_dep_modules`] wire them — member ids `0..members`, dependency ids
    /// continuing from [`first_dep_id`] — without running dependency resolution.
    fn cache_with(db: &mut LangDatabase, members: &[&str], deps: &[&str]) -> WorkspaceCache {
        let ed = noeta_lexer::Edition::default();
        let programs: Vec<SourceProgram> = members
            .iter()
            .enumerate()
            .map(|(i, u)| {
                SourceProgram::new(
                    db,
                    i as u32,
                    (*u).to_string(),
                    String::new(),
                    ed,
                    noeta_db::DerivedPath::default(),
                )
            })
            .collect();
        let first = first_dep_id(programs.len());
        let dep_programs: Vec<SourceProgram> = deps
            .iter()
            .enumerate()
            .map(|(i, u)| {
                SourceProgram::new(
                    db,
                    first + i as u32,
                    (*u).to_string(),
                    String::new(),
                    ed,
                    noeta_db::DerivedPath::default(),
                )
            })
            .collect();
        let dep_modules: Vec<DepModule> = dep_programs
            .iter()
            .map(|src| DepModule::new(db, *src, "root".into(), "key".into(), Vec::new()))
            .collect();
        WorkspaceCache {
            workspace: Workspace::new(
                db,
                programs.clone(),
                dep_modules.clone(),
                WorkspaceUses::default(),
            ),
            source_uris: members.iter().map(|u| (*u).to_string()).collect(),
            programs,
            dep_uris: deps.iter().map(|u| (*u).to_string()).collect(),
            dep_programs,
            dep_modules,
            tombstones: Vec::new(),
            dep_error: None,
        }
    }

    /// A member's `SourceId` resolves to that member — the category is named, not inferred from a
    /// range, and the URI is the member's own.
    #[test]
    fn a_member_id_resolves_to_that_member() {
        let mut db = LangDatabase::default();
        let cache = cache_with(
            &mut db,
            &["file:///w/a.noe", "file:///w/b.noe"],
            &["dep:///d"],
        );
        for (i, uri) in ["file:///w/a.noe", "file:///w/b.noe"].iter().enumerate() {
            let source = cache.source(SourceId(i as u32)).expect("a mapped member");
            assert_eq!(source.kind, SourceKind::Member);
            assert!(source.is_member());
            assert_eq!(source.uri, *uri);
            assert_eq!(source.input().unwrap(), cache.programs[i]);
        }
        // …and the reverse lookup agrees, while a dependency URI is deliberately not a member.
        assert_eq!(
            cache.find_member("file:///w/b.noe").map(|(id, _)| id),
            Some(SourceId(1))
        );
        assert!(cache.find_member("dep:///d0").is_none());
    }

    /// A dependency module's `SourceId` resolves to **that** dependency — the case the open-coded
    /// subtraction silently got wrong, attributing a span to a member instead.
    #[test]
    fn a_dep_id_resolves_to_that_dep() {
        let mut db = LangDatabase::default();
        let cache = cache_with(
            &mut db,
            &["file:///w/a.noe", "file:///w/b.noe"],
            &["dep:///d0", "dep:///d1"],
        );
        for (i, uri) in ["dep:///d0", "dep:///d1"].iter().enumerate() {
            let id = cache.dep_source_id(i).expect("a mapped dependency");
            let source = cache.source(id).expect("a mapped dependency");
            assert_eq!(source.kind, SourceKind::Dependency);
            assert!(!source.is_member());
            assert_eq!(source.uri, *uri);
            assert_eq!(source.input().unwrap(), cache.dep_programs[i]);
            assert_eq!(cache.find_dep(uri).map(|(id, _)| id), Some(id));
        }
        // A member URI is not a dependency, and vice versa.
        assert!(cache.find_dep("file:///w/a.noe").is_none());
    }

    /// The member/dependency **boundary** — the off-by-one the arithmetic invited — resolves
    /// correctly on both sides: the last member is still a member, and the first id past it is the
    /// first dependency, not a member and not the second dependency.
    #[test]
    fn the_member_dep_boundary_resolves_on_both_sides() {
        let mut db = LangDatabase::default();
        let cache = cache_with(
            &mut db,
            &["file:///w/a.noe", "file:///w/b.noe"],
            &["dep:///d0", "dep:///d1"],
        );
        let last_member = cache.source(SourceId(1)).expect("the last member");
        assert_eq!(last_member.kind, SourceKind::Member);
        assert_eq!(last_member.uri, "file:///w/b.noe");

        let first_dep = cache.source(SourceId(2)).expect("the first dependency");
        assert_eq!(first_dep.kind, SourceKind::Dependency);
        assert_eq!(first_dep.uri, "dep:///d0");

        // And `sources()` really is SourceId-ordered — the invariant the text vectors rely on.
        let seen: Vec<&str> = cache.sources().map(|s| s.uri).collect();
        assert_eq!(
            seen,
            vec![
                "file:///w/a.noe",
                "file:///w/b.noe",
                "dep:///d0",
                "dep:///d1"
            ]
        );
    }

    /// An expansion's `SourceId` — past every member *and* every dependency module — resolves to
    /// that expansion, with its generated text, once the link that produced it is supplied. This is
    /// what makes a span in generated code addressable at all: the id is beyond everything the
    /// workspace itself maps.
    #[test]
    fn an_expansion_id_resolves_to_its_generated_source() {
        let mut db = LangDatabase::default();
        let cache = cache_with(
            &mut db,
            &["file:///w/a.noe", "file:///w/b.noe"],
            &["dep:///d0"],
        );
        // Members 0,1; dependency 2; so the first expansion is 3.
        let id = SourceId(3);
        let expansions = vec![generated(
            id,
            "Api ⟨@dx \"petstore\"⟩",
            "fn gen(): int { return 1 }",
        )];

        // Without the link's expansions the id is honestly unresolved — the lookup never guesses.
        assert!(cache.source(id).is_none());

        let source = cache
            .source_with(id, &expansions)
            .expect("the expansion resolves once its link is supplied");
        assert_eq!(source.kind, SourceKind::Expansion);
        assert!(
            !source.is_member(),
            "an expansion must be excluded wherever members are what is meant"
        );
        assert_eq!(source.uri, "Api ⟨@dx \"petstore\"⟩");
        assert_eq!(source.text(&db), "fn gen(): int { return 1 }");
        assert!(
            source.input().is_none(),
            "an expansion has no salsa input — minting one would leak a slot per expansion"
        );

        // The boundary below it still resolves to the dependency, not to the expansion.
        let dep = cache
            .source_with(SourceId(2), &expansions)
            .expect("the dep");
        assert_eq!(dep.kind, SourceKind::Dependency);
        assert_eq!(dep.uri, "dep:///d0");

        // And `sources_with` continues in SourceId order, so an id-indexed vector built from it
        // addresses generated code at the right slot.
        let seen: Vec<&str> = cache.sources_with(&expansions).map(|s| s.uri).collect();
        assert_eq!(
            seen,
            vec![
                "file:///w/a.noe",
                "file:///w/b.noe",
                "dep:///d0",
                "Api ⟨@dx \"petstore\"⟩"
            ]
        );
        // One past this link's expansions is unresolved, not the next link's.
        assert!(cache.source_with(SourceId(4), &expansions).is_none());
    }

    /// An id past every source is **explicitly unresolved** — `None`, which the caller must handle
    /// — rather than falling into the dependency range and being attributed to the wrong file.
    #[test]
    fn an_out_of_range_id_is_explicitly_unresolved() {
        let mut db = LangDatabase::default();
        let cache = cache_with(&mut db, &["file:///w/a.noe"], &["dep:///d0"]);
        assert!(cache.source(SourceId(2)).is_none(), "one past the last dep");
        assert!(cache.source(SourceId(9)).is_none(), "far past the last dep");
        assert!(cache.dep_source_id(1).is_none(), "no such dependency index");

        // A dependency-less workspace: every id past the members is unresolved, not a dependency.
        let members_only = cache_with(&mut db, &["file:///w/a.noe"], &[]);
        assert!(members_only.source(SourceId(0)).is_some());
        assert!(members_only.source(SourceId(1)).is_none());
        assert!(members_only.dep_source_id(0).is_none());
    }

    /// audit F9 residual (a) at the editor seam: deleting a member from the file set must reclaim its
    /// input (text cleared) and **tombstone** it — and the next genuinely-new file must reuse that
    /// tombstoned input slot rather than mint a fresh one, bounding the salsa input table.
    #[test]
    fn deleting_a_member_releases_it_and_the_next_new_file_reuses_the_tombstone() {
        // The checker runs inside `release_source` (recomputing the emptied source's memos), so the
        // process-default registry must be seeded.
        noeta_stdlib::registry::default_seeded();
        let mut db = LangDatabase::default();

        // A three-member workspace.
        let s0 = sync(
            &mut db,
            None,
            vec![
                member("file:///w/a.noe", "W.A", "a"),
                member("file:///w/b.noe", "W.B", "b"),
                member("file:///w/c.noe", "W.C", "c"),
            ],
        )
        .expect("built the initial workspace");
        assert_eq!(s0.programs.len(), 3);
        assert!(s0.tombstones.is_empty());

        // Delete b.noe — the file set shrinks to {a, c}.
        let s1 = sync(
            &mut db,
            Some(s0),
            vec![
                member("file:///w/a.noe", "W.A", "a"),
                member("file:///w/c.noe", "W.C", "c"),
            ],
        )
        .expect("rebuilt after deletion");
        assert_eq!(s1.programs.len(), 2);
        assert_eq!(
            s1.tombstones.len(),
            1,
            "the deleted member is tombstoned, not abandoned"
        );
        let tomb = s1.tombstones[0];
        assert!(
            tomb.text(&db).is_empty(),
            "the deleted member's text must be reclaimed (set empty)"
        );

        // Add a genuinely-new file d.noe — it must reuse the tombstoned input slot.
        let s2 = sync(
            &mut db,
            Some(s1),
            vec![
                member("file:///w/a.noe", "W.A", "a"),
                member("file:///w/c.noe", "W.C", "c"),
                member("file:///w/d.noe", "W.D", "d"),
            ],
        )
        .expect("rebuilt after add");
        assert_eq!(s2.programs.len(), 3);
        assert!(
            s2.tombstones.is_empty(),
            "the tombstone pool was drained to serve the new file"
        );
        assert!(
            s2.programs.contains(&tomb),
            "the new file must reuse the tombstoned input slot, not mint a fresh one"
        );
        // And the reused slot now carries the new file's content.
        assert!(tomb.text(&db).contains("W.D"));
    }

    /// The unchanged-file-set fast path must preserve the tombstone pool across a keystroke edit (it
    /// only re-points member texts), so a later add can still reuse a parked slot.
    #[test]
    fn a_keystroke_edit_preserves_the_tombstone_pool() {
        noeta_stdlib::registry::default_seeded();
        let mut db = LangDatabase::default();
        let s0 = sync(
            &mut db,
            None,
            vec![
                member("file:///w/a.noe", "W.A", "a"),
                member("file:///w/b.noe", "W.B", "b"),
            ],
        )
        .unwrap();
        // Delete b → one tombstone parked.
        let s1 = sync(
            &mut db,
            Some(s0),
            vec![member("file:///w/a.noe", "W.A", "a")],
        )
        .unwrap();
        assert_eq!(s1.tombstones.len(), 1);
        // A keystroke edit of a.noe (same file set) — text re-pointed, pool untouched.
        let mut a_edited = member("file:///w/a.noe", "W.A", "a");
        a_edited.1.push_str("pub fn a2(): int { return 2 }\n");
        let s2 = sync(&mut db, Some(s1), vec![a_edited]).unwrap();
        assert_eq!(
            s2.tombstones.len(),
            1,
            "the fast path must not discard parked tombstones"
        );
        // Prove the fast path really ran (text updated in place).
        assert!(s2.programs[0].text(&db).contains("a2"));
    }
}
