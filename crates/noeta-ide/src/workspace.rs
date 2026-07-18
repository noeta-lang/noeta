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

use noeta_db::{DepModule, LangDatabase, SourceProgram, Workspace};
use salsa::Setter as _;

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
    /// Dependency-package modules (package-manager P2.1c), indexed by `SourceId - programs.len()`
    /// (their ids continue past the members). Kept apart from `source_uris`/`programs` so the
    /// per-keystroke reuse check and text-update loop stay over members only, while cross-package
    /// navigation still maps a dependency span back to its file.
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
    /// Reclaim every input's resident content when the whole workspace is torn down — its last open
    /// document closed (audit F9 residual a). salsa 0.27 cannot free the input slots themselves, but
    /// [`noeta_db::release_source`] releases each member and dependency source's text and overwrites
    /// its fat downstream memos with empty-program equivalents, so a closed directory stops holding
    /// its whole analysis resident. (Already-emptied tombstones are skipped — they hold nothing.)
    pub(crate) fn release_all(&self, db: &mut LangDatabase) {
        let ws = self.workspace;
        for &src in self.programs.iter().chain(&self.dep_programs) {
            noeta_db::release_source(db, ws, src);
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
                    tomb
                }
                None => {
                    SourceProgram::new(db, id as u32, u.clone(), text.clone(), edition_of_uri(u))
                }
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
    let mut deps = resolve_dep_modules(db, existing.as_ref(), &uris, programs.len() as u32);
    // Dependency sources whose modules vanished from the resolution (finding a): reclaimed below.
    let deleted_deps = std::mem::take(&mut deps.deleted);
    let mut cache = match existing {
        Some(mut cache) => {
            cache.workspace.set_members(db).to(programs.clone());
            cache.workspace.set_dep_modules(db).to(deps.modules.clone());
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
            workspace: Workspace::new(db, programs.clone(), deps.modules.clone()),
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
    let packages = match noeta_pm::manifest::dependency_packages_query(&entry_path) {
        Ok(packages) => packages,
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
