//! Which files constitute a **package's source** — the `.noe` modules a package contributes to the
//! link, and the path prefix they derive under.
//!
//! The *walk* itself lives in `noeta_loader::derive` (one implementation, so the app's own sibling
//! scan and a dependency's are the same scan — the asymmetry that made `src/deep/nested.noe`
//! invisible to `src/main.noe` while a dependency's `inner/deep.noe` resolved fine). What lives here
//! is the package-manager half of the question: **what a package is** (a [`MANIFEST_NAME`]) and
//! **what prefix its modules derive under**, which takes the manifest to answer — the loader is
//! handed both.

use std::path::Path;

use noeta_loader::{PackageRoot, RawModule};

use crate::manifest;

/// Read every `.noe` file **belonging to the package rooted at `dir`**, deriving each one's module
/// path under `prefix`, in sorted order (so `SourceId` assignment stays deterministic).
///
/// Names are the files' display paths (for diagnostics). Unreadable files are skipped, and so is
/// every subtree that is not this package's source (a nested package, a dot-directory, build
/// output).
pub fn read_package_sources(dir: &Path, prefix: &[String]) -> Vec<RawModule> {
    noeta_loader::read_package_modules(&PackageRoot::new(dir, prefix.to_vec()))
}

/// The **root package** the file at `entry` belongs to: where it is rooted, and the prefix its
/// modules derive under (its `[package] name`'s package half — `local/dirscan` → `dirscan.…`).
///
/// `None` when there is no manifest above the entry (a lone script), when the manifest cannot be
/// read or parsed, or when it declares no `[package]`. Without a package there is no prefix, so
/// nothing is derived and each module's own `namespace` declaration stands — which is exactly the
/// pre-derivation behavior a bare script should keep. A corrupt manifest is not silently accepted
/// overall: every invocation that reaches here also resolves dependencies through the same manifest,
/// and *that* parse reports it (the same division as [`crate::manifest::root_edition`]).
pub fn package_root(entry: &Path) -> Option<PackageRoot> {
    package_root_of(entry.parent().unwrap_or_else(|| Path::new(".")))
}

/// [`package_root`] asked of a **directory** — the package that directory's files belong to. What
/// the editor asks: its unit of work is an open document's directory, not a single entry.
pub fn package_root_of(dir: &Path) -> Option<PackageRoot> {
    let manifest_path = manifest::find(dir)?;
    let text = std::fs::read_to_string(&manifest_path).ok()?;
    let parsed = manifest::Manifest::parse(&text).ok()?;
    let name = parsed.package()?.name.root().to_string();
    // `find` returns the manifest file; the package is the directory holding it. An empty parent
    // (the manifest is in the current directory, reached from a bare relative entry) stays empty:
    // the walk scans `.` but keeps its paths bare, so module names stay byte-equal to how the
    // invocation spells the entry.
    let root = manifest_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    Some(PackageRoot::new(root, vec![name]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::MANIFEST_NAME;
    use noeta_loader::ModulePath;
    use std::path::PathBuf;

    /// A fresh temp package root, matching this crate's existing test convention (`store`/`lock`).
    fn tmp_package(name: &str) -> crate::test_temp::TempDir {
        let dir = crate::test_temp::TempDir::new(name);
        write(
            &dir.join(MANIFEST_NAME),
            "[package]\nname = \"local/pkg\"\nversion = \"0.1.0\"\n",
        );
        dir
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("create dirs");
        std::fs::write(path, text).expect("write");
    }

    /// Module names, relative to the package root and slash-joined, so the assertions read the same
    /// on any platform.
    fn names(root: &Path) -> Vec<String> {
        read_package_sources(root, &["pkg".to_string()])
            .into_iter()
            .map(|m| {
                Path::new(&m.name)
                    .strip_prefix(root)
                    .expect("under the root")
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .collect()
    }

    #[test]
    fn a_nested_package_contributes_nothing() {
        let root = tmp_package("nested");
        write(&root.join("api.noe"), "namespace pkg.api;\n");
        write(&root.join("sub/deep.noe"), "namespace pkg.sub;\n");
        // An example app inside the tree: its own package, its own namespace, its own dependencies.
        write(
            &root.join("examples/app").join(MANIFEST_NAME),
            "[package]\nname = \"local/app\"\n",
        );
        write(&root.join("examples/app/client.noe"), "namespace app;\n");

        assert_eq!(names(&root), vec!["api.noe", "sub/deep.noe"]);
    }

    #[test]
    fn dot_directories_and_build_output_contribute_nothing() {
        let root = tmp_package("pruned_dirs");
        write(&root.join("api.noe"), "namespace pkg.api;\n");
        // A worktree under `.claude/` is a whole second copy of the package — same namespaces.
        write(
            &root.join(".claude/worktrees/wip").join(MANIFEST_NAME),
            "[package]\nname = \"local/pkg\"\n",
        );
        write(
            &root.join(".claude/worktrees/wip/api.noe"),
            "namespace pkg.api;\n",
        );
        write(&root.join(".git/hooks/api.noe"), "namespace pkg.api;\n");
        write(&root.join("target/debug/api.noe"), "namespace pkg.api;\n");

        assert_eq!(names(&root), vec!["api.noe"]);
    }

    #[test]
    fn a_package_root_holding_a_manifest_is_still_read() {
        // The pruning predicate must not be applied to the root: it has a manifest by definition.
        let root = tmp_package("root_manifest");
        write(&root.join("only.noe"), "namespace pkg;\n");

        assert_eq!(names(&root), vec!["only.noe"]);
    }

    #[test]
    fn every_module_carries_the_path_its_location_derives() {
        let root = tmp_package("derived_paths");
        write(&root.join("pkg.noe"), "");
        write(&root.join("api.noe"), "");
        write(&root.join("src/deep/nested.noe"), "");

        let derived: Vec<Option<Vec<String>>> = read_package_sources(&root, &["pkg".to_string()])
            .into_iter()
            .map(|m| m.path.derived().map(<[String]>::to_vec))
            .collect();
        assert_eq!(
            derived,
            vec![
                Some(vec!["pkg".into(), "api".into()]),
                // The package-named root file IS the module the prefix names.
                Some(vec!["pkg".into()]),
                // A subdirectory is path segments; `src/` is layout, not a segment.
                Some(vec!["pkg".into(), "deep".into(), "nested".into()]),
            ]
        );
    }

    #[test]
    fn the_root_package_is_found_above_the_entry() {
        let root = tmp_package("root_lookup");
        write(&root.join("src/main.noe"), "");

        let found = package_root(&root.join("src/main.noe")).expect("a root package");
        assert_eq!(found.dir, root);
        assert_eq!(found.prefix, vec!["pkg".to_string()]);
    }

    #[test]
    fn an_illegal_stem_is_carried_as_illegal_not_silently_mapped() {
        let root = tmp_package("illegal_stem");
        write(&root.join("my-utils.noe"), "");

        let modules = read_package_sources(&root, &["pkg".to_string()]);
        assert!(matches!(
            modules[0].path,
            ModulePath::Illegal { ref segment, .. } if segment == "my-utils"
        ));
    }
}
