//! Which files constitute a **package's source** — the `.noe` modules a dependency contributes to
//! the consumer's link.
//!
//! This is package-manager knowledge, not linker knowledge: what makes a directory a package is a
//! [`MANIFEST_NAME`], and this crate is what owns that fact. The loader is *handed* a package's
//! modules ([`noeta_loader::DepPackage`]); it does not go looking for them.
//!
//! The walk lived in `noeta-loader` before, which sits *below* this crate in the crate DAG and so
//! could not consult [`MANIFEST_NAME`] at all. Lacking the one predicate that matters, it had no
//! exclusions whatsoever — "every `*.noe` anywhere under the package directory" — and swept a
//! package's example apps, any VCS/agent copy of its tree, and its build output into the consumer's
//! program. Moving the walk up to the layer that knows what a package *is* is what lets it prune.

use std::io;
use std::path::{Path, PathBuf};

use noeta_loader::RawModule;

use crate::manifest::MANIFEST_NAME;

/// The build-output directory (`cargo`'s, for a package shipping a native crate). Never source.
const BUILD_DIR: &str = "target";

/// Read every `.noe` file **belonging to the package rooted at `dir`**, in sorted order (so
/// `SourceId` assignment stays deterministic). A package is a directory *tree*, not the single flat
/// directory the entry's siblings live in, so this walks subdirectories — but only the ones that are
/// still this package (see [`is_outside_package`]). Names are the files' display paths (for
/// diagnostics). Unreadable files are skipped.
pub fn read_package_sources(dir: &Path) -> io::Result<Vec<RawModule>> {
    let mut paths = Vec::new();
    collect_package_noe_files(dir, &mut paths)?;
    paths.sort();
    Ok(paths
        .into_iter()
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            Some(RawModule {
                name: path.display().to_string(),
                text,
            })
        })
        .collect())
}

/// Recursively gather `.noe` file paths under `dir` into `out`, pruning every subtree that is not
/// this package's source. A subdirectory that can't be read is skipped (best-effort), matching the
/// sibling scan's tolerance.
fn collect_package_noe_files(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            if is_outside_package(&path) {
                continue;
            }
            let _ = collect_package_noe_files(&path, out);
        } else if path.is_file() && path.extension().is_some_and(|ext| ext == "noe") {
            out.push(path);
        }
    }
    Ok(())
}

/// Whether `dir` — a *sub*directory of the package being read — is outside that package's source.
///
/// Three ways it can be:
///
/// * **It is itself a package.** A directory holding its own [`MANIFEST_NAME`] is a different
///   package with its own namespace and its own dependencies — a package's example app
///   (`examples/<app>/noeta.toml`) is the standard case. Linking one into a consumer's program
///   merges declarations nobody imported, and two example apps under one package collide outright.
/// * **It is a dot-directory.** `.git` is metadata; an agent worktree under `.claude/worktrees/` is
///   a *whole second checkout of the package*, every module in it duplicating the real one
///   namespace for namespace.
/// * **It is build output** (`target/`).
///
/// The package root itself is never asked — it is expected to hold a manifest, which is what made it
/// the root.
fn is_outside_package(dir: &Path) -> bool {
    let name = dir.file_name().and_then(|n| n.to_str());
    name.is_some_and(|n| n.starts_with('.') || n == BUILD_DIR) || dir.join(MANIFEST_NAME).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh temp package root, matching this crate's existing test convention (`store`/`lock`).
    fn tmp_package(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("noeta_sources_test_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the package root");
        write(
            &dir.join(MANIFEST_NAME),
            "[package]\nname = \"local/pkg\"\n",
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
        read_package_sources(root)
            .expect("read the package")
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
}
