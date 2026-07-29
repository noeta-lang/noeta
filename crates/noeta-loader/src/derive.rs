//! **Where a module's path comes from: the filesystem.**
//!
//! A module's path is not declared, it is *derived* — the package's import prefix plus the file's
//! path relative to the package root, `/` → `.`, case preserved verbatim. `namespace` is still
//! accepted (it is removed in a later slice) but it is now checked against the derivation rather
//! than believed: a declaration that disagrees is an error naming both paths.
//!
//! Deriving fixes three things a declaration got wrong by construction:
//!
//! * The consumer's **import key becomes real.** A dependency's modules derive under the key the
//!   consumer wrote, so keying `para/cli` as `mycli` gives `mycli.cli` — where re-rooting a declared
//!   `namespace para.cli` silently did nothing (it rewrites the leading segment only when it is the
//!   package's own root segment, and `para` is the *scope* half), leaving the package's internal
//!   names as its public API.
//! * **Subdirectories work in an app.** The path carries the directories, so `deep/nested.noe` is
//!   `<prefix>.deep.nested` and not a name collision with `nested.noe` beside it.
//! * **Two files cannot claim one path** without saying so — the derivation makes a collision a
//!   property of the file *names*, which the loader can see and report against both files
//!   ([`super::link`]), instead of the second file's exports silently vanishing.

use std::path::{Component, Path, PathBuf};

use noeta_span::{Source, SourceId};

/// The file that marks a directory as a package.
///
/// The loader needs the *name* only as a **pruning** predicate for the package walk (a subdirectory
/// holding one is a different package — an example app, a vendored copy — and none of its modules
/// are this package's). Everything the manifest *means* still lives in `noeta-pm`, which re-exports
/// this constant rather than spelling it a second time, so the two cannot drift.
pub const MANIFEST_NAME: &str = "noeta.toml";

/// The conventional source subdirectory. A package that keeps its modules under `src/` derives the
/// same paths as one that keeps them at its root: `src/human.noe` and `human.noe` are both
/// `<prefix>.human`. `src` is a layout choice, not a namespace segment — no first-party package
/// writes `use pkg.src.human`, and making the layout observable in the import path would mean
/// moving files renames the API.
const SOURCE_DIR: &str = "src";

/// The build-output directory (`cargo`'s, for a package shipping a native crate). Never source.
const BUILD_DIR: &str = "target";

/// A package's root on disk and the path prefix its modules derive under.
///
/// The **prefix** is the consumer's view of the package, which only the caller can know:
///
/// * a plain `[dependencies]` entry → the **key** (`mycli = { … }` → `mycli.…`);
/// * a **scope-array** member → `{key}.{package root segment}` (`para = [{ package = "para/db" },
///   …]` → `para.db.…`);
/// * the **root package's own** modules → its `[package] name`'s package half (`local/dirscan` →
///   `dirscan.…`).
///
/// Deciding which of those applies takes the manifest, so `noeta-pm` builds this and the loader is
/// handed it — the same division as [`super::DepPackage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageRoot {
    /// The directory the package is rooted at — module paths are relative to it.
    pub dir: PathBuf,
    /// The dotted prefix every module of this package derives under.
    pub prefix: Vec<String>,
}

impl PackageRoot {
    pub fn new(dir: impl Into<PathBuf>, prefix: Vec<String>) -> PackageRoot {
        PackageRoot {
            dir: dir.into(),
            prefix,
        }
    }

    /// `path` as seen from the package root, or `None` when it is not under this root at all.
    ///
    /// `None` rather than a fallback, because there is no honest path to derive for a file outside
    /// the package: taking the whole (possibly absolute) path would invent segments out of the
    /// machine's directory layout. The caller treats it as "no package context" — the file's own
    /// `namespace` declaration stands.
    pub fn relative<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        if self.dir.as_os_str().is_empty() {
            return Some(path);
        }
        path.strip_prefix(&self.dir).ok()
    }
}

/// A module's path, as far as the *filesystem* can say.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ModulePath {
    /// No package context: the file was reached without a [`PackageRoot`] (a lone script, a
    /// directory that is not a package), so nothing is derived and the module's own `namespace`
    /// declaration stands. The pre-derivation behavior, and what every in-memory caller gets by
    /// default.
    #[default]
    Declared,
    /// The path derived from the file's location. Authoritative: a `namespace` declaration that
    /// disagrees with it is an error ([`DiagnosticCode::ModulePathMismatch`]).
    ///
    /// [`DiagnosticCode::ModulePathMismatch`]: noeta_diagnostics::DiagnosticCode::ModulePathMismatch
    Derived(Vec<String>),
    /// The file's location cannot *be* a module path: some directory name or the file stem is not a
    /// legal identifier segment, so no `use` could ever spell it. Reported against the file with a
    /// rename hint — never silently mapped (`my-utils` → `my_utils` would give one module two
    /// spellings, which is the thing derivation exists to remove).
    Illegal {
        /// The offending path segment, as it appears on disk.
        segment: String,
        /// A legal spelling of it, to rename to.
        rename_to: String,
    },
}

impl ModulePath {
    /// The derived path, if one was derived.
    pub fn derived(&self) -> Option<&[String]> {
        match self {
            ModulePath::Derived(path) => Some(path),
            _ => None,
        }
    }
}

/// Derive the module path of the file at `relative` (a path **relative to the package root**) under
/// `prefix`.
///
/// The rule, in full:
///
/// 1. A leading `src/` is a layout convention, not a segment — it is dropped ([`SOURCE_DIR`]).
/// 2. Every remaining directory, then the file stem, is a segment; `/` becomes `.` and **case is
///    preserved verbatim** (`Helpers/URI.noe` → `Helpers.URI`).
/// 3. The stem is dropped when it repeats the segment before it — the package-named root file
///    (`para-db/db.noe` under prefix `para.db`) *is* the module the prefix names, not a `para.db.db`
///    beside it.
/// 4. Every segment must lex as a single identifier, or the path is [`ModulePath::Illegal`].
pub fn derive_module_path(prefix: &[String], relative: &Path) -> ModulePath {
    let mut segments: Vec<String> = Vec::new();
    let components: Vec<&std::ffi::OsStr> = relative
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    let Some((file, dirs)) = components.split_last() else {
        return ModulePath::Derived(prefix.to_vec());
    };
    let dirs = match dirs.split_first() {
        // `src/` is the source root when it leads the *package-relative* path, and only there — a
        // `src` directory deeper in the tree is an ordinary segment.
        Some((first, rest)) if *first == SOURCE_DIR => rest,
        _ => dirs,
    };
    for dir in dirs {
        segments.push(dir.to_string_lossy().into_owned());
    }
    let stem = Path::new(file)
        .file_stem()
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned();
    segments.push(stem);

    for segment in &segments {
        if !is_path_segment(segment) {
            return ModulePath::Illegal {
                rename_to: legalize(segment),
                segment: segment.clone(),
            };
        }
    }

    let mut path = prefix.to_vec();
    for segment in segments {
        // The root-file collapse: a stem that repeats what it sits under names that module, not a
        // child of it.
        if path.last() == Some(&segment) {
            continue;
        }
        path.push(segment);
    }
    ModulePath::Derived(path)
}

/// Whether `segment` can appear in a `use` path — i.e. it lexes as exactly one identifier.
///
/// Asked of the **lexer**, not of a hand-rolled character class, so the answer is the language's
/// own: a keyword (`class.noe`) lexes as its keyword token and is refused, as it must be — nothing
/// could import it.
fn is_path_segment(segment: &str) -> bool {
    let source = Source::new(SourceId(0), "<module-path>", segment);
    let lexed = noeta_lexer::lex(&source);
    lexed.diagnostics.is_empty()
        && matches!(
            lexed.tokens.as_slice(),
            [token] if token.kind == noeta_lexer::TokenKind::Ident
        )
}

/// A legal spelling of an illegal segment, for the rename hint: non-identifier characters become
/// `_`, and a leading digit gains one. Advice only — the compiler never applies it.
fn legalize(segment: &str) -> String {
    let mut out: String = segment
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// Read every `.noe` file **belonging to the package rooted at `root.dir`**, deriving each one's
/// module path, in sorted order (so `SourceId` assignment stays deterministic).
///
/// A package is a directory *tree*, not one flat directory, so this walks subdirectories — but only
/// the ones that are still this package ([`is_outside_package`]). Unreadable files and directories
/// are skipped (best-effort: a lone entry must not fail to link because something beside it is
/// unreadable).
///
/// This is the **one** package walk. `noeta_pm::sources::read_package_sources` is this function
/// under the package manager's own name (it owns what a package *means*, and adds nothing to how
/// its files are found), and the app's own sibling scan is this same walk over the root package —
/// which is what makes `src/deep/nested.noe` visible to `src/main.noe`, where a flat scan left it
/// invisible while a *dependency's* `inner/deep.noe` resolved fine.
pub fn read_package_modules(root: &PackageRoot) -> Vec<super::RawModule> {
    let mut paths = Vec::new();
    collect_noe_files(&root.dir, &mut paths);
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            let relative = root.relative(&path)?;
            Some(super::RawModule {
                path: derive_module_path(&root.prefix, relative),
                name: path.display().to_string(),
                text,
            })
        })
        .collect()
}

/// Recursively gather `.noe` file paths under `dir`, pruning every subtree that is not this
/// package's source.
fn collect_noe_files(dir: &Path, out: &mut Vec<PathBuf>) {
    // A bare relative root (`noeta test app.noe` → parent `""`) is the current directory, which
    // `read_dir("")` refuses; scan `.` but keep the produced paths rooted at the original (empty)
    // prefix, so a module's name stays byte-equal to how the invocation addresses it.
    let bare = dir.as_os_str().is_empty();
    let scan: &Path = if bare { Path::new(".") } else { dir };
    let Ok(entries) = std::fs::read_dir(scan) else {
        return;
    };
    for entry in entries.flatten() {
        let path = if bare {
            PathBuf::from(entry.file_name())
        } else {
            entry.path()
        };
        if path.is_dir() {
            if !is_outside_package(&path) {
                collect_noe_files(&path, out);
            }
        } else if path.is_file() && path.extension().is_some_and(|ext| ext == "noe") {
            out.push(path);
        }
    }
}

/// Whether `dir` — a *sub*directory of the package being read — is outside that package's source.
///
/// Three ways it can be:
///
/// * **It is itself a package.** A directory holding its own [`MANIFEST_NAME`] is a different
///   package with its own paths and its own dependencies — a package's example app
///   (`examples/<app>/noeta.toml`) is the standard case. Linking one into a consumer's program
///   merges declarations nobody imported, and two example apps under one package collide outright.
/// * **It is a dot-directory.** `.git` is metadata; an agent worktree under `.claude/worktrees/` is
///   a *whole second checkout of the package*, every module in it deriving the same path as the real
///   one.
/// * **It is build output** ([`BUILD_DIR`]).
///
/// The package root itself is never asked — it is expected to hold a manifest, which is what made it
/// the root.
pub fn is_outside_package(dir: &Path) -> bool {
    let name = dir.file_name().and_then(|n| n.to_str());
    name.is_some_and(|n| n.starts_with('.') || n == BUILD_DIR) || dir.join(MANIFEST_NAME).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix(segments: &[&str]) -> Vec<String> {
        segments.iter().map(|s| (*s).to_string()).collect()
    }

    fn derived(prefix_segments: &[&str], relative: &str) -> Vec<String> {
        match derive_module_path(&prefix(prefix_segments), Path::new(relative)) {
            ModulePath::Derived(path) => path,
            other => panic!("expected a derived path, got {other:?}"),
        }
    }

    #[test]
    fn the_relative_path_becomes_the_module_path() {
        assert_eq!(
            derived(&["para", "api"], "middleware.noe"),
            ["para", "api", "middleware"]
        );
        assert_eq!(
            derived(&["para", "db"], "query.noe"),
            ["para", "db", "query"]
        );
        assert_eq!(
            derived(&["dirscan"], "deep/nested.noe"),
            ["dirscan", "deep", "nested"]
        );
    }

    #[test]
    fn case_is_preserved_verbatim() {
        assert_eq!(
            derived(&["pkg"], "Helpers/URI.noe"),
            ["pkg", "Helpers", "URI"]
        );
    }

    #[test]
    fn the_root_file_collapses_into_the_prefix() {
        // `para-api/api.noe` under the scope-array prefix `para.api` IS `para.api`.
        assert_eq!(derived(&["para", "api"], "api.noe"), ["para", "api"]);
        // …but a plain dependency keyed `mycli` keeps the package's own root file as a segment:
        // the key is the prefix, so `cli.noe` is `mycli.cli` — which is what makes the key real.
        assert_eq!(derived(&["mycli"], "cli.noe"), ["mycli", "cli"]);
    }

    #[test]
    fn a_leading_src_is_not_a_segment() {
        assert_eq!(derived(&["dirscan"], "src/human.noe"), ["dirscan", "human"]);
        assert_eq!(
            derived(&["dirscan"], "src/deep/nested.noe"),
            ["dirscan", "deep", "nested"]
        );
        // Only when it leads: a `src` deeper in the tree is an ordinary directory.
        assert_eq!(
            derived(&["pkg"], "vendor/src/thing.noe"),
            ["pkg", "vendor", "src", "thing"]
        );
    }

    #[test]
    fn an_illegal_stem_is_refused_with_a_rename() {
        assert_eq!(
            derive_module_path(&prefix(&["pkg"]), Path::new("my-utils.noe")),
            ModulePath::Illegal {
                segment: "my-utils".to_string(),
                rename_to: "my_utils".to_string(),
            }
        );
        // A keyword cannot be a segment either — nothing could `use` it.
        assert!(matches!(
            derive_module_path(&prefix(&["pkg"]), Path::new("class.noe")),
            ModulePath::Illegal { .. }
        ));
        // A directory counts exactly as much as the stem.
        assert!(matches!(
            derive_module_path(&prefix(&["pkg"]), Path::new("my utils/ok.noe")),
            ModulePath::Illegal { .. }
        ));
    }

    #[test]
    fn a_digit_leading_stem_gains_an_underscore() {
        assert_eq!(
            derive_module_path(&prefix(&["pkg"]), Path::new("2fa.noe")),
            ModulePath::Illegal {
                segment: "2fa".to_string(),
                rename_to: "_2fa".to_string(),
            }
        );
    }
}
