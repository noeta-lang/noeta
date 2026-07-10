//! The **composed toolchain** (package-manager Phase 3, N3.2): when an app's dependency graph
//! carries native entry crates (`[package] native = …`, N3.1), the stock `noeta` binary cannot
//! serve it — the extensions' signatures must feed the checker, their completions the LSP, their
//! commands the CLI. So the stock binary **generates a shim crate** (depend on `noeta-cli` as a
//! lib + each entry crate; `main` passes the extra units into `run_cli`), builds it with `cargo`,
//! caches the result content-addressed, and **delegates the original invocation** to the composed
//! binary. A pure-Noeta app never reaches this module.
//!
//! Retrieval of the toolchain source (user decision, 2026-07-09): **cargo fetches it** — inside
//! the noeta workspace the shim uses path dependencies (instant, the interim norm); outside, it
//! declares git dependencies pinned to the running binary's version tag and cargo's own git cache
//! handles fetch/offline/reuse. `NOETA_TOOLCHAIN_SRC` overrides with an explicit checkout.
//!
//! An entry crate exports its units under a fixed convention:
//! `pub static NOETA_EXTENSIONS: &[&(dyn noeta_native::Extension + Sync)]` — a slice, so one
//! package registers any number of extension units (std's own shape).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use noeta_pm::graph::{self, NativeCrate};
use sha2::{Digest, Sha256};

/// The env guard that marks a composed binary: set on delegation so the composed toolchain (which
/// contains this same code) never re-composes, even though its graph still lists native crates.
const COMPOSED_GUARD: &str = "NOETA_COMPOSED";

/// Delegate to the app's composed toolchain if its dependency graph carries native crates.
///
/// Returns `None` when the stock binary should just proceed (no manifest, no native deps, we ARE
/// the composed binary, or the graph fails to resolve — the normal command path re-resolves and
/// surfaces the identical error with its usual rendering). Returns `Some(code)` only when
/// composition was needed and **failed** — declared native capability must never silently degrade
/// to a stock run, which would surface later as baffling unknown-module errors. On success the
/// delegation `exec`s and never returns (non-unix: waits and exits with the child's code).
pub fn maybe_delegate(entry: &Path) -> Option<ExitCode> {
    if std::env::var_os(COMPOSED_GUARD).is_some() {
        return None;
    }
    // `noeta check` accepts a directory — the manifest is discovered from the directory itself,
    // so probe with a synthetic child (resolve_graph only uses the entry's parent).
    let probe;
    let entry = if entry.is_dir() {
        probe = entry.join("_.noe");
        probe.as_path()
    } else {
        entry
    };
    let Ok(resolved) = graph::resolve_graph(entry) else {
        return None; // the command path re-resolves and renders the error
    };
    if resolved.native_crates.is_empty() {
        return None;
    }
    match delegate(&resolved.native_crates, &resolved.trusted_command_roots) {
        Ok(never) => match never {},
        Err(err) => {
            eprintln!("lang: cannot compose the toolchain for this app's native dependencies:");
            eprintln!("{err}");
            Some(ExitCode::from(1))
        }
    }
}

/// [`maybe_delegate`] keyed on the **current directory**'s manifest — for invocations that carry
/// no file argument but may only make sense inside a composed toolchain (an unknown subcommand
/// that is really a native dependency's `ExtCommand`). Returns `None` when there is nothing to
/// compose, exactly like the entry-file form.
pub fn maybe_delegate_cwd() -> Option<ExitCode> {
    let cwd = std::env::current_dir().ok()?;
    maybe_delegate(&cwd)
}

/// The uninhabited "success" of [`delegate`] — on unix `exec` replaces the process; elsewhere we
/// exit with the child's status. Either way, control never comes back.
enum Never {}

fn delegate(crates: &[NativeCrate], trusted_command_roots: &[String]) -> Result<Never, String> {
    let entries = resolve_entries(crates)?;
    let toolchain = toolchain_source()?;
    let key = compose_key(&entries, &toolchain, trusted_command_roots);
    let dir = compose_dir(&key)?;
    // Content-addressed: the key covers the entry crates' package trees, the toolchain source
    // form, and the running binary's build identity — a hit means this exact composition already
    // built (the binary was copied into the compose dir as its own artifact), and the whole
    // delegation is one exec.
    let binary = dir.join("bin").join(BIN_NAME);
    if !binary.is_file() {
        build(&dir, &entries, &toolchain, trusted_command_roots, &binary)?;
    }
    exec(&binary)
}

/// The shim's `[[bin]]` name (also the cached binary's file name).
const BIN_NAME: &str = "noeta-composed";

/// One native entry crate, with the cargo-level facts the shim needs.
struct Entry {
    /// The owning noeta package (`company/package`) — for messages and the compose key.
    identity: String,
    /// The crate dir (absolute).
    dir: PathBuf,
    /// The crate's cargo `[package] name` — the dependency line's `package = …`.
    cargo_name: String,
    /// `cargo_name` as a Rust identifier (`-` → `_`) — how `main.rs` references the crate.
    ident: String,
}

fn resolve_entries(crates: &[NativeCrate]) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::with_capacity(crates.len());
    for nc in crates {
        let cargo_name = noeta_pm::manifest::cargo_package_name(&nc.crate_dir)
            .map_err(|err| format!("native crate of `{}`: {err}", nc.identity))?;
        let ident = cargo_name.replace('-', "_");
        entries.push(Entry {
            identity: nc.identity.clone(),
            dir: nc.crate_dir.clone(),
            cargo_name,
            ident,
        });
    }
    // Deterministic shim content (the graph already sorts by identity; keep it locally true too).
    entries.sort_by(|a, b| a.identity.cmp(&b.identity));
    Ok(entries)
}

/// Where the shim's toolchain crates come from.
enum ToolchainSource {
    /// The source tree that built the running binary (or `NOETA_TOOLCHAIN_SRC`): path deps.
    Workspace(PathBuf),
    /// Out-of-workspace: git deps pinned to the running binary's release tag.
    GitTag { repo: String, tag: String },
}

/// Locate the toolchain source. Priority: `NOETA_TOOLCHAIN_SRC` (a noeta repo checkout) → the
/// baked-in source dir of the running binary (`CARGO_MANIFEST_DIR`, valid while developing in the
/// workspace — the interim norm) → the released git tag (`repository` + `v<version>`).
fn toolchain_source() -> Result<ToolchainSource, String> {
    if let Some(root) = std::env::var_os("NOETA_TOOLCHAIN_SRC") {
        let root = PathBuf::from(root);
        if !root
            .join("crates")
            .join("noeta-cli")
            .join("Cargo.toml")
            .is_file()
        {
            return Err(format!(
                "NOETA_TOOLCHAIN_SRC (`{}`) is not a noeta source checkout \
                 (no crates/noeta-cli/Cargo.toml)",
                root.display()
            ));
        }
        return Ok(ToolchainSource::Workspace(root));
    }
    // crates/noeta-cli at the toolchain's own build time → the repo root two levels up.
    let baked = Path::new(env!("CARGO_MANIFEST_DIR"));
    if let Some(root) = baked.parent().and_then(Path::parent)
        && baked.join("Cargo.toml").is_file()
    {
        return Ok(ToolchainSource::Workspace(root.to_path_buf()));
    }
    let repo = env!("CARGO_PKG_REPOSITORY");
    let version = env!("CARGO_PKG_VERSION");
    if repo.is_empty() || version == "0.0.0" {
        return Err(
            "this noeta binary was built away from its source tree and carries no released \
             version tag to fetch — set NOETA_TOOLCHAIN_SRC to a noeta source checkout"
                .to_string(),
        );
    }
    Ok(ToolchainSource::GitTag {
        repo: repo.to_string(),
        tag: format!("v{version}"),
    })
}

/// The composition's content address: the running binary's build identity (any toolchain rebuild
/// recomposes), the toolchain-source form, and each entry crate's identity + dir. The entry
/// crates' *content* is covered by re-resolution: a path dep's tree hash changes on edit and the
/// resolve step feeds fresh dirs — but the dir alone doesn't see edits, so the package tree hash
/// is folded in by the caller passing entries derived from the freshly-hashed graph. To keep the
/// key honest about content, each entry dir's own `Cargo.toml` bytes are folded in as well.
fn compose_key(
    entries: &[Entry],
    toolchain: &ToolchainSource,
    trusted_command_roots: &[String],
) -> String {
    let mut h = Sha256::new();
    h.update(b"noeta-compose-v1");
    h.update(noeta_cache::binary_identity().unwrap_or_default());
    // Which packages' commands are trusted changes the shim (and the CLI surface), so a change in
    // `[trust].commands` must recompose (Phase 4).
    for root in trusted_command_roots {
        h.update(b"cmd:");
        h.update(root);
    }
    match toolchain {
        ToolchainSource::Workspace(root) => {
            h.update(b"ws");
            h.update(root.display().to_string());
        }
        ToolchainSource::GitTag { repo, tag } => {
            h.update(b"git");
            h.update(repo);
            h.update(tag);
        }
    }
    for e in entries {
        h.update(&e.identity);
        h.update(e.dir.display().to_string());
        h.update(std::fs::read(e.dir.join("Cargo.toml")).unwrap_or_default());
    }
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The compose workspace under the user cache (`~/.cache/noeta/compose/<key>/`).
fn compose_dir(key: &str) -> Result<PathBuf, String> {
    let root = noeta_cache::Cache::locate()
        .ok_or("no cache directory could be resolved (set HOME or NOETA_CACHE_DIR)")?;
    let dir = root.join("compose").join(key);
    std::fs::create_dir_all(dir.join("src"))
        .map_err(|err| format!("cannot create `{}`: {err}", dir.display()))?;
    Ok(dir)
}

/// Generate the shim crate, `cargo build` it, and copy the produced binary to `cached` (the
/// compose dir owns its artifact — the cargo target dir is a build detail). The shim is
/// regenerated on every (cache-miss) build — it is derived state, never edited.
///
/// Two env knobs exist for the test suite / development (not user surface):
/// `NOETA_COMPOSE_DEBUG=1` builds the shim in the debug profile, and `NOETA_COMPOSE_TARGET_DIR`
/// points cargo at an existing target dir — together they let the e2e tests reuse the
/// workspace's already-built debug artifacts instead of a cold release build.
fn build(
    dir: &Path,
    entries: &[Entry],
    toolchain: &ToolchainSource,
    trusted_command_roots: &[String],
    cached: &Path,
) -> Result<(), String> {
    std::fs::write(dir.join("Cargo.toml"), shim_cargo_toml(entries, toolchain))
        .map_err(|err| format!("writing shim Cargo.toml: {err}"))?;
    std::fs::write(
        dir.join("src").join("main.rs"),
        shim_main_rs(entries, trusted_command_roots),
    )
    .map_err(|err| format!("writing shim main.rs: {err}"))?;
    let names: Vec<&str> = entries.iter().map(|e| e.identity.as_str()).collect();
    eprintln!(
        "noeta: composing the toolchain with native dependencies [{}] (first build of this \
         dependency set — cached afterwards)",
        names.join(", ")
    );
    let debug = std::env::var_os("NOETA_COMPOSE_DEBUG").is_some();
    let target_dir = std::env::var_os("NOETA_COMPOSE_TARGET_DIR")
        .map_or_else(|| dir.join("target"), PathBuf::from);
    let mut cmd = std::process::Command::new("cargo");
    cmd.arg("build");
    if !debug {
        cmd.arg("--release");
    }
    let output = cmd
        .arg("--manifest-path")
        .arg(dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target_dir)
        .output()
        .map_err(|err| {
            format!(
                "cannot run `cargo` (required to build native dependencies — install a Rust \
                 toolchain): {err}"
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "building the composed toolchain failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let built = target_dir
        .join(if debug { "debug" } else { "release" })
        .join(BIN_NAME);
    std::fs::create_dir_all(cached.parent().expect("bin/ parent"))
        .and_then(|()| std::fs::copy(&built, cached))
        .map_err(|err| {
            format!(
                "caching the composed binary (`{}` → `{}`): {err}",
                built.display(),
                cached.display()
            )
        })?;
    Ok(())
}

/// The generated shim manifest. `[workspace]` is deliberately empty so the shim never gets
/// adopted by an enclosing cargo workspace; the release profile mirrors the toolchain's own
/// (codegen-units=1 + thin LTO — the composed binary should perform like the stock one).
fn shim_cargo_toml(entries: &[Entry], toolchain: &ToolchainSource) -> String {
    let mut out = String::new();
    out.push_str(
        "# Generated by noeta (package-manager Phase 3) — the composed toolchain shim.\n\
         # Derived state: regenerated on dependency-set changes. Do not edit.\n\n\
         [package]\nname = \"noeta-composed\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
         [[bin]]\nname = \"noeta-composed\"\npath = \"src/main.rs\"\n\n[dependencies]\n",
    );
    let toolchain_dep = |krate: &str| match toolchain {
        ToolchainSource::Workspace(root) => format!(
            "{krate} = {{ path = {} }}\n",
            toml_quote(&root.join("crates").join(krate).display().to_string())
        ),
        ToolchainSource::GitTag { repo, tag } => format!(
            "{krate} = {{ git = {}, tag = {} }}\n",
            toml_quote(repo),
            toml_quote(tag)
        ),
    };
    out.push_str(&toolchain_dep("noeta-cli"));
    out.push_str(&toolchain_dep("noeta-native"));
    for (n, e) in entries.iter().enumerate() {
        out.push_str(&format!(
            "ext{n} = {{ package = {}, path = {} }}\n",
            toml_quote(&e.cargo_name),
            toml_quote(&e.dir.display().to_string())
        ));
    }
    out.push_str("\n[workspace]\n\n[profile.release]\ncodegen-units = 1\nlto = \"thin\"\n");
    out
}

/// The generated shim entry point: aggregate every entry crate's exported `NOETA_EXTENSIONS`
/// slice and hand the whole toolchain to `run_cli`, along with the **command-trusted namespace
/// roots** (Phase 4) so `run_cli` registers a dependency's `noeta <cmd>` only for a package the root
/// app authorized in `[trust].commands`. `Box::leak` is fine — the units live for the process,
/// exactly like the stock binary's statics.
fn shim_main_rs(entries: &[Entry], trusted_command_roots: &[String]) -> String {
    let mut out = String::new();
    out.push_str(
        "//! Generated by noeta (package-manager Phase 3) — the composed toolchain. Do not edit.\n\n\
         fn main() -> std::process::ExitCode {\n\
         \x20   let mut units: Vec<&'static (dyn noeta_native::Extension + Sync)> = Vec::new();\n",
    );
    for (n, e) in entries.iter().enumerate() {
        out.push_str(&format!(
            "    units.extend_from_slice(ext{n}::NOETA_EXTENSIONS); // {} ({})\n",
            e.identity, e.ident
        ));
    }
    let roots = trusted_command_roots
        .iter()
        .map(|r| rust_str(r))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!(
        "    let trusted_command_roots: &[&str] = &[{roots}];\n\
         \x20   noeta_cli::run_cli(Box::leak(units.into_boxed_slice()), trusted_command_roots)\n}}\n"
    ));
    out
}

/// A Rust string literal for a shim-embedded value (roots are identifier segments, but escape
/// defensively).
fn rust_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Minimal TOML basic-string quoting (paths, names, URLs — never control characters).
fn toml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Hand the invocation to the composed binary: same argv, `NOETA_COMPOSED=1`. On unix this
/// replaces the process (`exec`); elsewhere it waits and exits with the child's code.
fn exec(binary: &Path) -> Result<Never, String> {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(binary)
            .args(&args)
            .env(COMPOSED_GUARD, "1")
            .exec();
        Err(format!("exec `{}` failed: {err}", binary.display()))
    }
    #[cfg(not(unix))]
    {
        let status = std::process::Command::new(binary)
            .args(&args)
            .env(COMPOSED_GUARD, "1")
            .status()
            .map_err(|err| format!("running `{}` failed: {err}", binary.display()))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<Entry> {
        vec![Entry {
            identity: "acme/imgfx".to_string(),
            dir: PathBuf::from("/store/acme_imgfx/native"),
            cargo_name: "imgfx-native".to_string(),
            ident: "imgfx_native".to_string(),
        }]
    }

    #[test]
    fn shim_manifest_uses_path_deps_in_workspace_form() {
        let toml = shim_cargo_toml(
            &entries(),
            &ToolchainSource::Workspace(PathBuf::from("/src/noeta")),
        );
        assert!(toml.contains("noeta-cli = { path = \"/src/noeta/crates/noeta-cli\" }"));
        assert!(toml.contains(
            "ext0 = { package = \"imgfx-native\", path = \"/store/acme_imgfx/native\" }"
        ));
        assert!(
            toml.contains("[workspace]"),
            "must not be workspace-adopted"
        );
        assert!(toml.contains("codegen-units = 1"));
    }

    #[test]
    fn shim_manifest_uses_git_tag_out_of_workspace() {
        let toml = shim_cargo_toml(
            &entries(),
            &ToolchainSource::GitTag {
                repo: "https://github.com/nsrosenqvist/noeta".to_string(),
                tag: "v0.1.0".to_string(),
            },
        );
        assert!(
            toml.contains(
                "noeta-cli = { git = \"https://github.com/nsrosenqvist/noeta\", tag = \"v0.1.0\" }"
            ),
            "{toml}"
        );
    }

    #[test]
    fn shim_main_aggregates_unit_slices() {
        let main = shim_main_rs(&entries(), &["imgfx".to_string()]);
        assert!(main.contains("units.extend_from_slice(ext0::NOETA_EXTENSIONS);"));
        assert!(main.contains("noeta_cli::run_cli(Box::leak("));
        // The command-trusted roots are baked in and passed to run_cli (Phase 4).
        assert!(main.contains("let trusted_command_roots: &[&str] = &[\"imgfx\"];"));
        assert!(main.contains("trusted_command_roots)"));
    }

    #[test]
    fn shim_main_with_no_trusted_commands_passes_an_empty_slice() {
        let main = shim_main_rs(&entries(), &[]);
        assert!(main.contains("let trusted_command_roots: &[&str] = &[];"));
    }
}
