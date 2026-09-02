//! The **composed toolchain**: when an app's dependency graph carries native entry crates
//! (`[package] native = …`), the stock `noeta` binary cannot
//! serve it — the extensions' signatures must feed the checker, their completions the LSP, their
//! commands the CLI. So the stock binary **generates a shim crate** (depend on `noeta-cli` as a
//! lib + each entry crate; `main` passes the extra units into `run_cli`), builds it with `cargo`,
//! caches the result content-addressed, and **delegates the original invocation** to the composed
//! binary. A pure-Noeta app never reaches this module.
//!
//! Retrieval of the toolchain source: **cargo fetches it** — inside
//! the noeta workspace the shim uses path dependencies (instant); outside, it
//! declares git dependencies pinned to the running binary's version tag and cargo's own git cache
//! handles fetch/offline/reuse. `NOETA_TOOLCHAIN_SRC` overrides with an explicit checkout. In
//! both forms the shim also carries a `[patch]` on the canonical toolchain repo redirecting
//! *every* toolchain crate to the consumer binary's own source ([`toolchain_patch_section`]) — a
//! released binary materializes a cached checkout of its own tag for this — so a package pinned
//! at any older release tag still composes as ONE copy of each toolchain crate.
//!
//! An entry crate exports its units under a fixed convention:
//! `pub static NOETA_EXTENSIONS: &[&(dyn noeta_ext_abi::Extension + Sync)]` — a slice, so one
//! package registers any number of extension units (std's own shape).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use noeta_pm::graph::{self, NativeCrate, ResolvedCommandBinding};
use sha2::{Digest, Sha256};

/// The env guard that marks a composed binary: set on delegation so the composed toolchain (which
/// contains this same code) never re-composes, even though its graph still lists native crates.
///
/// Its **value** is the composition manifest — the identities the delegated binary links
/// ([`noeta_pm::composed::env_value`]). A one-shot verb never needs to read it back: it composed for
/// the very project it is about to serve. A long-lived server does, because it answers about files
/// its own working directory never named, so it must be able to tell whether the process it is
/// running in can link the project in front of it (see [`noeta_pm::composed`]).
const COMPOSED_GUARD: &str = noeta_pm::composed::COMPOSED_ENV;

/// The conventional Cargo features a package uses to gate its **dev-kind** capabilities — a tier's
/// formatter today (`fmt`, which drags in a parser like `malva`). A composed **dev
/// toolchain** ([`ShimKind::Toolchain`]) turns on each of these that a native crate *declares*, so
/// `noeta fmt` can reflow that crate's tier bodies; a shipped **runner/AOT** base never enables them,
/// so the formatter and its parser stay uncompiled and out of the artifact (the security split).
/// Package authors opt in purely by naming the feature per this convention — the composer never
/// enables a feature a crate doesn't declare, so this list can grow without breaking any crate.
const DEV_FEATURES: &[&str] = &["fmt"];

/// What a composed shim is built as. The **toolchain** (dev) embeds `noeta-cli` and serves every
/// command for a native-dependency app. The **runner** embeds only the lean
/// `noeta-runner` + the app's native runtime extensions — no dev tooling — and is the base a
/// `build --exe`/`--native` artifact staples onto, so a shipped native-dependency app carries its
/// runtime handlers but none of the toolchain.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ShimKind {
    Toolchain,
    Runner,
    /// A **composed AOT runtime** staticlib (dev-deps): the base a `build --native` artifact for a
    /// *native-dependency* app links its AOT object against. Like [`Runner`](ShimKind::Runner) it is a
    /// lean, dev-tooling-free base carrying the app's native runtime extensions — but it is a
    /// `staticlib` (not a `bin`): the `--native` linker (`cc`) combines it with the program's AOT
    /// object, and it forwards the program's stdlib rings to `noeta-aot-runtime`.
    ///
    /// Ahead-of-time compilation is Cranelift, so this whole composition — the variant, its archive
    /// builder and the two generators that write its project — exists only in a `jit`-featured build,
    /// exactly like the `build --native` flow in `cmd::native` that reaches it. A Cranelift-free
    /// binary has no `--native` to serve and carries none of it.
    #[cfg(feature = "jit")]
    AotRuntime,
}

impl ShimKind {
    /// The toolchain crate the shim's base depends on.
    fn base_crate(self) -> &'static str {
        match self {
            ShimKind::Toolchain => "noeta-cli",
            ShimKind::Runner => "noeta-runner",
            #[cfg(feature = "jit")]
            ShimKind::AotRuntime => "noeta-aot-runtime",
        }
    }
    /// The compose-key discriminator, so toolchain/runner/AOT compositions of the same dep set cache
    /// under distinct addresses.
    fn tag(self) -> &'static [u8] {
        match self {
            ShimKind::Toolchain => b"kind:toolchain",
            ShimKind::Runner => b"kind:runner",
            #[cfg(feature = "jit")]
            ShimKind::AotRuntime => b"kind:aot-runtime",
        }
    }
}

/// Delegate to the app's composed toolchain if its dependency graph carries native crates.
///
/// Returns `Ok(_)` when the stock binary should just proceed: `Ok(Some(graph))` hands back the
/// dependency graph this probe resolved (the **default** selection — no `--target`), so the same
/// invocation's command path can reuse it instead of resolving again; `Ok(None)`
/// means no graph is available (we ARE the composed binary, or the graph fails to resolve — the
/// normal command path re-resolves and surfaces the identical error with its usual rendering).
/// Returns `Err(code)` only when composition was needed and **failed** — declared native
/// capability must never silently degrade to a stock run, which would surface later as baffling
/// unknown-module errors. On success the delegation `exec`s and never returns (non-unix: waits
/// and exits with the child's code).
pub fn maybe_delegate(entry: &Path) -> Result<Option<graph::ResolvedGraph>, ExitCode> {
    if std::env::var_os(COMPOSED_GUARD).is_some() {
        return Ok(None);
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
        return Ok(None); // the command path re-resolves and renders the error
    };
    delegate_resolved(resolved)
}

/// The half of [`maybe_delegate`] past resolution: delegate if this app has native crates, else
/// hand the graph back. Shared with [`maybe_delegate_cwd`], which resolves for itself so it can
/// *report* a resolution failure rather than swallow it.
fn delegate_resolved(
    resolved: graph::ResolvedGraph,
) -> Result<Option<graph::ResolvedGraph>, ExitCode> {
    // Only **runtime** native crates force composition here: a `run`/`build`/`check` that loads a
    // program needs the composed toolchain iff the graph carries native runtime code. A dev-only
    // `dev-native` crate (a formatter) contributes nothing at runtime, so its presence must not
    // drag a plain run through a compose — it is reached only by `noeta fmt` (see
    // [`maybe_delegate_fmt`]).
    let crates = resolved.runtime_native_crates();
    if crates.is_empty() {
        return Ok(Some(resolved));
    }
    match delegate(&crates, &resolved.command_bindings) {
        Ok(never) => match never {},
        Err(err) => {
            eprintln!("noeta: cannot compose the toolchain for this app's native dependencies:");
            eprintln!("{err}");
            Err(ExitCode::from(1))
        }
    }
}

/// `noeta fmt`'s delegation: compose and `exec` a dev toolchain whenever the app's graph carries
/// **any** native entry crate — dev-only formatter crates INCLUDED (unlike [`delegate_resolved`],
/// which triggers only on runtime crates). A `dev-native` package's whole point is to provide a
/// tier-body formatter that runs here, so `fmt` is the one command that must reach it.
///
/// Guarded by [`COMPOSED_GUARD`]: a composed toolchain re-invokes `noeta fmt` inside itself, and
/// that inner process must format with its own linked-in extensions, not compose a third time.
/// Returns `Ok(())` when there is nothing to compose (a pure-Noeta app formats in-process); on a
/// successful compose it `exec`s and never returns. `Err` on a compose/resolve failure — the caller
/// surfaces it rather than silently formatting without the package's formatter.
pub fn maybe_delegate_fmt(entry: &Path) -> Result<(), String> {
    if std::env::var_os(COMPOSED_GUARD).is_some() {
        return Ok(());
    }
    // `fmt` accepts a directory; the manifest is discovered from it, so probe with a synthetic child
    // (see `maybe_delegate` — `resolve_graph` only uses the entry's parent).
    let probe;
    let entry = if entry.is_dir() {
        probe = entry.join("_.noe");
        probe.as_path()
    } else {
        entry
    };
    let resolved = match graph::resolve_graph(entry) {
        Ok(resolved) => resolved,
        // No manifest / not in a package: format in-process (a bare script has no formatter deps).
        Err(_) => return Ok(()),
    };
    if resolved.native_crates.is_empty() {
        return Ok(());
    }
    // The full set (dev-only included), always a Toolchain composition — `fmt` lives only in the
    // full toolchain base, never the lean runner.
    match delegate(&resolved.native_crates, &resolved.command_bindings) {
        Ok(never) => match never {},
        Err(err) => Err(err),
    }
}

/// [`maybe_delegate`] keyed on the **current directory**'s manifest — for invocations that carry
/// no file argument but may only make sense inside a composed toolchain (an unknown subcommand
/// that is really a native dependency's `ExtCommand`). Returns `None` when there is nothing to
/// compose, exactly like the entry-file form's `Ok` (the resolved graph has no consumer here —
/// the unknown-subcommand chain never loads a program).
///
/// **A resolution failure is reported here**, unlike in [`maybe_delegate`], where the command path
/// re-resolves and renders it. Nothing downstream re-resolves *this* one: the unknown-subcommand
/// chain never loads a program, so the manifest error was swallowed and the user got clap's
/// "unrecognized subcommand" instead. That is what a project still on the pre-mapping
/// `commands = ["company/package"]` array saw for the very command it was trying to trust —
/// the migration message naming `[trust.commands]` was written, and thrown away.
pub fn maybe_delegate_cwd() -> Option<ExitCode> {
    if std::env::var_os(COMPOSED_GUARD).is_some() {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    // The manifest is discovered from the directory, so probe with a synthetic child (see
    // `maybe_delegate`: `resolve_graph` only uses the entry's parent).
    let resolved = match graph::resolve_graph(&cwd.join("_.noe")) {
        Ok(resolved) => resolved,
        Err(err) => {
            eprintln!("noeta: {err}");
            return Some(ExitCode::from(2));
        }
    };
    delegate_resolved(resolved).err()
}

/// Delegate a **long-lived server** (`noeta lsp` / `noeta mcp`) to the composed toolchain of the
/// project it was launched in — but **only when that toolchain is already built**.
///
/// A server is not a CLI invocation, and this is where the two part company.
///
/// *Why delegate at all.* A package's native extension is statically linked Rust: the modules,
/// types and directives it registers exist in a process only because that process **is** the
/// composed toolchain. There is no in-process way to acquire them, so a server that does not
/// delegate cannot link any project with a `[trust] native` dependency — every `use` of the
/// package's namespace is an E0019 and the editor shows a working file as broken. `exec` is the
/// right shape here precisely because a server has nothing to lose yet: this runs before the
/// handshake, stdio is inherited untouched, and no request has been read.
///
/// *Why only on a cache hit.* Composing on a miss is a full `cargo` build of the shim and the
/// toolchain — minutes. A one-shot `noeta check` can pay that and print a line saying so; a server
/// cannot. An MCP client abandons a server that does not answer `initialize` inside its timeout, and
/// an editor that gives up and restarts the server would start a *second* cargo build. So a miss is
/// not built here — it is **reported**, per request, by whichever surface is about to answer
/// ([`noeta_pm::composed::uncomposed`]). The composition is a cache: one `noeta check` in the
/// project fills it, and every later server start hits it.
///
/// *Why the working directory.* It is the only project a server has before its first request. Both
/// an editor and an agent host launch these servers with the project as the cwd, so the hit rate is
/// high — and when it is wrong (a server asked about a project it did not compose for), the
/// per-request check catches it, because it compares against what this process actually links rather
/// than against what it was launched in.
///
/// Resolution is the **query** walk: starting a language server must not rewrite `noeta.lock`.
/// Every failure — no manifest, an unresolvable graph, no cache dir — returns quietly and leaves the
/// stock server running, which is correct: none of those is composable, and the per-request check
/// still speaks for anything that needed a composition it did not get.
pub fn delegate_server_if_composed() {
    delegate_cwd_if_composed()
}

/// Hand this invocation to the working directory's composed toolchain **if that toolchain is
/// already built** — the cache-hit-only delegation. Returns (having done nothing) when there is no
/// project, no native dependency, or no composition on disk; on a hit it `exec`s and never returns.
///
/// Two callers, for the same reason in different clothes: a long-lived server ([`delegate_server_if_composed`])
/// cannot link a native package's registrations without being the composed toolchain, and `--help`
/// cannot *describe* the composed toolchain's commands without being it. Neither may pay a cold
/// compose — a server would blow its client's `initialize` timeout, and a help request that
/// disappeared into a multi-minute cargo build would be a worse answer than a slightly short list.
pub fn delegate_cwd_if_composed() {
    if std::env::var_os(COMPOSED_GUARD).is_some() {
        return;
    }
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    // The manifest is discovered from the directory, so probe with a synthetic child (see
    // `maybe_delegate` — `resolve_graph` only uses the entry's parent).
    let Ok(resolved) = graph::resolve_graph_query(&cwd.join("_.noe")) else {
        return;
    };
    let crates = resolved.runtime_native_crates();
    if crates.is_empty() {
        return; // a pure-Noeta project: the stock server is already the whole truth
    }
    let Some(binary) = cached_toolchain(&crates, &resolved.command_bindings) else {
        return; // cold: serve, and let each request say what it cannot see
    };
    // On unix this never returns; a failed exec falls through to the stock server, which will then
    // report the missing composition per request rather than answering wrongly.
    match exec(&binary, &crates) {
        Ok(never) => match never {},
        Err(err) => eprintln!("noeta: {err}"),
    }
}

/// The composed toolchain binary for `crates`, **if it is already built** — no build, and no
/// creation of the compose directory on a miss (a probe must not litter the cache with empty
/// entries). On a hit the entry is stamped through [`compose_dir`] exactly as a building path would,
/// so `noeta cache ls`/`clean` see a server's use as a use.
fn cached_toolchain(
    crates: &[NativeCrate],
    command_bindings: &[ResolvedCommandBinding],
) -> Option<PathBuf> {
    let entries = resolve_entries(crates).ok()?;
    let toolchain = toolchain_source().ok()?;
    let key = compose_key(
        &entries,
        &toolchain,
        command_bindings,
        ShimKind::Toolchain,
        &[],
    );
    let binary = noeta_cache::Cache::locate()?
        .join("compose")
        .join(&key)
        .join("bin")
        .join(BIN_NAME);
    if !binary.is_file() {
        return None;
    }
    let _ = compose_dir(&key);
    Some(binary)
}

/// The uninhabited "success" of [`delegate`] — on unix `exec` replaces the process; elsewhere we
/// exit with the child's status. Either way, control never comes back.
enum Never {}

fn delegate(
    crates: &[NativeCrate],
    command_bindings: &[ResolvedCommandBinding],
) -> Result<Never, String> {
    let binary = compose_binary(crates, command_bindings, ShimKind::Toolchain)?;
    exec(&binary, crates)
}

/// Build (or reuse the cached) **composed runner** for an app whose dependency graph carries native
/// runtime crates, and return its path — the lean base a `build --exe`/`--native` artifact staples
/// onto. Unlike [`delegate`], this never execs: `emit_exe`/`emit_native` read the
/// returned binary and staple the program's bundle onto it. Returns `Ok(None)` when the app has **no**
/// native crates (the stock lean `noeta-runner` is the base instead).
pub fn compose_runner_binary(entry: &Path) -> Result<Option<PathBuf>, String> {
    let resolved = graph::resolve_graph(entry)
        .map_err(|err| format!("resolving the app's native dependencies: {err}"))?;
    // A shipped `--exe` base carries only RUNTIME native crates — a dev-only formatter has no place
    // in a production binary.
    let crates = resolved.runtime_native_crates();
    if crates.is_empty() {
        return Ok(None);
    }
    let binary = compose_binary(&crates, &resolved.command_bindings, ShimKind::Runner)?;
    Ok(Some(binary))
}

/// Build (client-side, cached) a composed toolchain that links **the package's own** native entry
/// crate at `crate_dir`, then run `noeta doc --api --non-builtin` inside it and return the emitted
/// `docs.json`. This is how `noeta publish` generates a native package's API reference: the
/// module surface lives only in the compiled Rust, so it must be *built* — on the **publisher's**
/// machine (the registry never compiles anything). A build failure surfaces as `Err`, which the
/// publish flow uses as a quality gate (don't publish a native package whose crate won't compile).
///
/// `--non-builtin`, never `--root <package segment>`: the composed toolchain links exactly the
/// builtin units plus this package's own extension(s), so "everything non-builtin" IS the
/// package's surface. Guessing the root from the manifest segment silently documented `[]` for any
/// extension whose `root()` diverges from its segment (para/p2p roots at `para`) — the empty
/// `docs.json` every published para/* release carried.
pub fn package_api_docs(identity: &str, crate_dir: &Path) -> Result<String, String> {
    let nc = NativeCrate {
        identity: identity.to_string(),
        crate_dir: crate_dir.to_path_buf(),
        // No resolved graph here (publish hands us the crate dir directly) — hash it ourselves so
        // the publish quality gate also recomposes on source edits.
        content_hash: noeta_pm::hash_tree(crate_dir).unwrap_or_default(),
        // Publish docs a real runtime `native` crate; a dev-native package documents nothing here.
        dev_only: false,
    };
    // A doc-generation query exposes no CLI of its own, so command-trust is irrelevant — `&[]`.
    let binary = compose_binary(&[nc], &[], ShimKind::Toolchain)?;
    // `--lint`: the composed toolchain refuses (exit 2) if the package registers any extern type
    // outside its extensions' own roots, or an extension claims a toolchain-owned root — the
    // publish quality gate. Its stderr carries the offenders; surface it verbatim.
    let output = std::process::Command::new(&binary)
        .arg("doc")
        .arg("--api")
        .arg("--non-builtin")
        .arg("--lint")
        .output()
        .map_err(|err| {
            format!(
                "running the composed toolchain `{}`: {err}",
                binary.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "generating API docs in the composed toolchain failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|err| format!("the API docs.json was not UTF-8: {err}"))
}

/// Build (or reuse the cached) **composed AOT runtime** staticlib for a native-dependency app, and
/// return its archive path plus the native system libraries it must be linked against. This is the
/// `--native` analogue of [`compose_runner_binary`]: where a `--exe` artifact staples its bundle onto a
/// composed *runner binary*, a `--native` artifact links its AOT object against this composed
/// *staticlib* — a lean, dev-tooling-free base that additionally installs the app's native runtime
/// extensions (so the AOT-compiled bundle resolves the native modules/types/tiers it references) and
/// forwards the program's stdlib `rings` to `noeta-aot-runtime` (so an unused ring's native deps are
/// still dropped, DCE Axis B). Returns `Ok(None)` when the app has **no** native crates (the caller
/// falls back to the stock `libnoeta_aot.a`).
#[cfg(feature = "jit")]
pub fn compose_aot_runtime_archive(
    entry: &Path,
    rings: &[String],
) -> Result<Option<(PathBuf, Vec<String>)>, String> {
    let resolved = graph::resolve_graph(entry)
        .map_err(|err| format!("resolving the app's native dependencies: {err}"))?;
    // A `--native` AOT base installs only RUNTIME native crates — a dev-only formatter never ships.
    let crates = resolved.runtime_native_crates();
    if crates.is_empty() {
        return Ok(None);
    }
    let entries = resolve_entries(&crates)?;
    let toolchain = toolchain_source()?;
    // A stapled AOT artifact exposes no CLI, so command-trust never reaches this base — key on `&[]`.
    let key = compose_key(&entries, &toolchain, &[], ShimKind::AotRuntime, rings);
    let dir = compose_dir(&key)?;
    let archive = dir.join("lib").join(AOT_ARCHIVE_NAME);
    let libs_file = dir.join("lib").join("link-libs.txt");
    // Content-addressed on the same axes as the bin composers, plus the ring set: a hit means this
    // exact (deps × rings) archive already built. The link-libs note is cached beside the archive so a
    // hit needs no rebuild to recover it.
    if !(archive.is_file() && libs_file.is_file()) {
        build_aot_archive(&dir, &entries, &toolchain, rings, &archive, &libs_file)?;
    }
    let libs = std::fs::read_to_string(&libs_file)
        .map_err(|err| {
            format!(
                "reading cached AOT link libs `{}`: {err}",
                libs_file.display()
            )
        })?
        .split_whitespace()
        .map(str::to_string)
        .collect();
    Ok(Some((archive, libs)))
}

/// The composed AOT runtime's staticlib file name (its `[lib] name` is `noeta_composed_aot`).
#[cfg(feature = "jit")]
const AOT_ARCHIVE_NAME: &str = "libnoeta_composed_aot.a";

/// Resolve the composed shim of `kind` for `crates` to a built binary (cached, content-addressed),
/// building it on a miss. Shared by the toolchain delegation and the runner-artifact base.
fn compose_binary(
    crates: &[NativeCrate],
    command_bindings: &[ResolvedCommandBinding],
    kind: ShimKind,
) -> Result<PathBuf, String> {
    let entries = resolve_entries(crates)?;
    let toolchain = toolchain_source()?;
    let key = compose_key(&entries, &toolchain, command_bindings, kind, &[]);
    let dir = compose_dir(&key)?;
    // Content-addressed: the key covers the entry crates' package trees, the toolchain source form,
    // the running binary's build identity, and the shim kind — a hit means this exact composition
    // already built (the binary was copied into the compose dir as its own artifact).
    let binary = dir.join("bin").join(BIN_NAME);
    if !binary.is_file() {
        build(&dir, &entries, &toolchain, command_bindings, &binary, kind)?;
    }
    Ok(binary)
}

/// The shim's `[[bin]]` name (also the cached binary's file name).
const BIN_NAME: &str = "noeta-composed";

/// Explain a failed composed-toolchain build. The common cause by far is a **native ABI
/// mismatch**: an extension crate is compiled from source against *this* toolchain (the composed
/// build unifies every `noeta-*` crate through `[patch]`), so a package whose release predates a
/// change to the registration contract — a new [`noeta_ext_abi::registry::ExtFn`] field, a changed
/// dispatch signature — fails to compile against it. Raw `rustc` output pointing into the package
/// cache is unactionable on its own, so name the package whose crate failed and say what the
/// consumer can actually do about it. The full build log still follows: the diagnosis is a header,
/// never a replacement.
fn compose_failure(stderr: &str, entries: &[Entry]) -> String {
    // Which package owns the crate rustc complained about — matched on the crate directory, since
    // every diagnostic path inside a dependency's tree lies under it.
    // Match on the owning package's **tree**, not the entry crate's own directory: an entry crate
    // (`native = "native"`) usually depends on sibling crates in the same package, and it is one of
    // those that rustc names. The tree root is the nearest ancestor holding the package manifest.
    let culprits: Vec<&Entry> = entries
        .iter()
        .filter(|e| stderr.contains(&package_tree(&e.dir).display().to_string()))
        .collect();
    // The signature of an out-of-date registration table: a struct literal that predates a field.
    let abi_shaped = stderr.contains("E0063")
        || stderr.contains("E0560")
        || stderr.contains("missing field")
        || stderr.contains("no field");
    let diagnosis = match (&culprits[..], abi_shaped) {
        ([], _) => String::new(),
        (owners, true) => {
            let names: Vec<&str> = owners.iter().map(|e| e.identity.as_str()).collect();
            format!(
                "the native code of {} does not compile against this toolchain — its registration \
                 tables are missing a field the extension ABI now requires, so the release predates \
                 this `noeta`. An extension is built from source against the exact toolchain, so \
                 there is no version of it that both can load: update the package (`noeta update`) \
                 once a release built for this ABI exists, pin a `noeta` matching the release, or \
                 point the identity at a fixed checkout with `[patch]` in `{}`.\n\n",
                describe_packages(&names),
                noeta_pm::manifest::MANIFEST_NAME,
            )
        }
        (owners, false) => {
            let names: Vec<&str> = owners.iter().map(|e| e.identity.as_str()).collect();
            format!(
                "the native code of {} failed to compile. The build log follows — it is that \
                 package's own build error, not your program's.\n\n",
                describe_packages(&names),
            )
        }
    };
    format!("{diagnosis}building the composed toolchain failed:\n{stderr}")
}

/// The noeta package tree an entry crate belongs to: the nearest ancestor of `crate_dir` holding a
/// `noeta.toml`. Falls back to `crate_dir` itself when there is none (a bare crate path), which
/// keeps the caller's substring match as tight as it was.
fn package_tree(crate_dir: &Path) -> &Path {
    let mut dir = crate_dir;
    loop {
        if dir.join(noeta_pm::manifest::MANIFEST_NAME).is_file() {
            return dir;
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return crate_dir,
        }
    }
}

/// `` `para/db` `` / `` `para/db` and `para/p2p` `` — package identities for a sentence.
fn describe_packages(names: &[&str]) -> String {
    match names {
        [one] => format!("`{one}`"),
        [rest @ .., last] => format!(
            "{} and `{last}`",
            rest.iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        [] => "the native dependencies".to_string(),
    }
}

/// One native entry crate, with the cargo-level facts the shim needs.
struct Entry {
    /// The owning noeta package (`company/package`) — for messages and the compose key.
    identity: String,
    /// The crate dir (absolute).
    dir: PathBuf,
    /// The owning package's tree content hash (from the resolved graph) — folded into the compose
    /// key so an edit to a **path** dependency's source recomposes (its `dir` never changes).
    content_hash: String,
    /// The crate's cargo `[package] name` — the dependency line's `package = …`.
    cargo_name: String,
    /// `cargo_name` as a Rust identifier (`-` → `_`) — how `main.rs` references the crate.
    ident: String,
    /// The conventional dev-capability features this crate declares (⊆ [`DEV_FEATURES`]) — the ones a
    /// [`ShimKind::Toolchain`] composition turns on; empty for a pure-runtime crate. A shipped base
    /// ignores this and pulls the crate at default features.
    dev_features: Vec<String>,
    /// The **footprint rings** this entry crate declares (a `ring-*` feature — e.g. `ring-p2p` for the
    /// para.p2p transport). A Toolchain/Runner composition enables **all** of them
    /// (full runtime capability); the AOT composition enables only the subset the program's footprint
    /// scan selected, so a `--native` binary that never imports the ring's modules sheds its native
    /// dep tree. Empty for a crate with no gated rings (built at default features, unchanged).
    ring_features: Vec<String>,
    /// A **dev-only** entry crate (from `package.dev-native`) — a formatter/dev-tool. It is pulled
    /// with `default-features = false, features = ["fmt"]` (only its formatter code, never its heavy
    /// deps), its `NOETA_EXTENSIONS` are wrapped through [`crate::formatter_only`] so only their body
    /// formatters reach the toolchain, and it appears ONLY in a [`ShimKind::Toolchain`] composition
    /// (excluded from `Runner`/`AotRuntime` — a prod shim never lists a dev tool). `false` for a
    /// normal runtime crate.
    dev_only: bool,
}

fn resolve_entries(crates: &[NativeCrate]) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::with_capacity(crates.len());
    for nc in crates {
        let cargo_name = noeta_pm::manifest::cargo_package_name(&nc.crate_dir)
            .map_err(|err| format!("native crate of `{}`: {err}", nc.identity))?;
        let ident = cargo_name.replace('-', "_");
        // A dev toolchain turns on the crate's conventional dev features; take the intersection with
        // what it actually declares so enabling one never makes cargo error on an unknown feature.
        let declared = noeta_pm::manifest::cargo_features(&nc.crate_dir)
            .map_err(|err| format!("native crate of `{}`: {err}", nc.identity))?;
        let dev_features = DEV_FEATURES
            .iter()
            .filter(|f| declared.iter().any(|d| d == *f))
            .map(|f| (*f).to_string())
            .collect();
        // Footprint rings the crate gates (a `ring-*` feature). Sorted for deterministic shim output.
        let mut ring_features: Vec<String> = declared
            .iter()
            .filter(|f| f.starts_with("ring-"))
            .cloned()
            .collect();
        ring_features.sort();
        entries.push(Entry {
            identity: nc.identity.clone(),
            dir: nc.crate_dir.clone(),
            content_hash: nc.content_hash.clone(),
            cargo_name,
            ident,
            dev_features,
            ring_features,
            dev_only: nc.dev_only,
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
/// recomposes), the toolchain-source form, and each entry crate's identity + dir + **package tree
/// content hash**. The hash is what makes the key honest for a **path** dependency: its `dir`
/// never changes on edit (unlike a store-materialized git/registry dep, whose dir is per-SHA), so
/// without the hash an edited native crate kept serving the stale composed binary. The entry
/// dir's own `Cargo.toml` bytes are folded in as well (feature/dep changes recompose even when a
/// caller passes a hashless entry).
fn compose_key(
    entries: &[Entry],
    toolchain: &ToolchainSource,
    command_bindings: &[ResolvedCommandBinding],
    kind: ShimKind,
    rings: &[String],
) -> String {
    let mut h = Sha256::new();
    h.update(b"noeta-compose-v1");
    h.update(kind.tag());
    h.update(noeta_cache::binary_identity().unwrap_or_default());
    // The AOT runtime forwards the program's stdlib rings to `noeta-aot-runtime`, so two programs of
    // the same native-dep set but different ring footprints must cache distinct archives (bin kinds
    // pass no rings).
    for ring in rings {
        h.update(b"ring:");
        h.update(ring);
    }
    // The command bindings change the shim (and the CLI surface), so any edit to `[trust.commands]`
    // — a new binding, a dropped one, or a rename of the local or exported name — must recompose.
    // Fold each binding's three parts in, delimited, so distinct bindings can't alias.
    for b in command_bindings {
        h.update(b"cmd:");
        h.update(&b.local);
        h.update(b"=");
        h.update(&b.provider);
        h.update(b":");
        h.update(&b.exported);
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
        h.update(b"tree:");
        h.update(&e.content_hash);
        h.update(std::fs::read(e.dir.join("Cargo.toml")).unwrap_or_default());
    }
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The marker file inside each compose entry recording the **build identity** of the `noeta`
/// binary that last used it. The compose key folds `noeta_cache::binary_identity()` in, so every
/// toolchain rebuild strands the previous build's entries — gigabytes no future invocation can
/// ever hit again. The key is a hash (the identity can't be recovered from it), so the identity is
/// also written beside the entry: `noeta cache clean` removes entries whose marker is not this
/// binary's, and `noeta cache ls` reads the marker's mtime as the entry's last-used time (it is
/// rewritten on every use, hit or miss). An entry with no marker was last used by a pre-marker
/// toolchain build — stale by the same argument.
pub(crate) const COMPOSE_IDENTITY_FILE: &str = "identity";

/// The compose workspace under the user cache (`~/.cache/noeta/compose/<key>/`).
fn compose_dir(key: &str) -> Result<PathBuf, String> {
    let root = noeta_cache::Cache::locate()
        .ok_or("no cache directory could be resolved (set HOME or NOETA_CACHE_DIR)")?;
    let dir = root.join("compose").join(key);
    std::fs::create_dir_all(dir.join("src"))
        .map_err(|err| format!("cannot create `{}`: {err}", dir.display()))?;
    // Stamp the entry with this binary's identity (see [`COMPOSE_IDENTITY_FILE`]). Best-effort —
    // a failed write costs only an over-eager future `cache clean`, never the composition.
    let _ = std::fs::write(
        dir.join(COMPOSE_IDENTITY_FILE),
        noeta_cache::binary_identity().unwrap_or_default(),
    );
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
    command_bindings: &[ResolvedCommandBinding],
    cached: &Path,
    kind: ShimKind,
) -> Result<(), String> {
    // The compose dir's basename IS the compose key (`compose_dir`); stamp it into the shim's
    // package version so distinct compositions are distinct cargo packages ([`shim_version`]).
    let key = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    // The local toolchain source the `[patch]` section redirects to — for a released (git-tag)
    // binary this materializes the cached checkout of its own tag (a cache miss is the only path
    // that reaches here, so the clone happens at most once per tag).
    let src_root = toolchain_src_root(toolchain)?;
    std::fs::write(
        dir.join("Cargo.toml"),
        shim_cargo_toml(entries, toolchain, &src_root, kind, key),
    )
    .map_err(|err| format!("writing shim Cargo.toml: {err}"))?;
    std::fs::write(
        dir.join("src").join("main.rs"),
        shim_main_rs(entries, command_bindings, kind),
    )
    .map_err(|err| format!("writing shim main.rs: {err}"))?;
    let names: Vec<&str> = entries.iter().map(|e| e.identity.as_str()).collect();
    eprintln!(
        "noeta: composing the toolchain with native dependencies [{}] (building — later runs \
         reuse the cached binary)",
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(compose_failure(&stderr, entries));
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
    discard_build_scratch(dir, &target_dir);
    Ok(())
}

/// Delete the compose dir's own cargo target dir now that its artifact has been copied out.
///
/// The compose dir owns its artifact and the target dir is a build detail — but it was kept
/// forever, and it is *enormous* next to what it produces: one composition measured 1.4G of target
/// beside a 51M artifact, a 27× tail. Nothing evicts compose entries, so a machine that checks a
/// handful of native-dependency packages accumulates several gigabytes per composition and keeps
/// them indefinitely (41G observed, which twice filled a 450G disk).
///
/// Dropping it costs nothing on the hit path: [`compose_binary`] and its AOT twin test for the
/// cached *artifact*, never the target dir, so a pruned entry still hits and never rebuilds. It
/// costs a full rebuild only when the entry misses anyway — which is the same work a cold entry
/// does. Best-effort: the artifact is already cached, so a failure to remove is not a build error.
///
/// **Only ever removes the compose dir's own `target/`.** Under `NOETA_COMPOSE_TARGET_DIR` the dir
/// belongs to the caller — the e2e tests point it at the workspace's own build directory — and
/// deleting that would destroy work this function never created.
fn discard_build_scratch(dir: &Path, target_dir: &Path) {
    if target_dir == dir.join("target") {
        let _ = std::fs::remove_dir_all(target_dir);
    }
}

/// Generate the composed AOT runtime staticlib project, build it with `cargo rustc … --print
/// native-static-libs` (which both produces `libnoeta_composed_aot.a` and reports the exact native
/// link line the final `cc` step needs), and cache the archive + that link line. Honors the same
/// `NOETA_COMPOSE_DEBUG`/`NOETA_COMPOSE_TARGET_DIR` test knobs as [`build`].
#[cfg(feature = "jit")]
fn build_aot_archive(
    dir: &Path,
    entries: &[Entry],
    toolchain: &ToolchainSource,
    rings: &[String],
    cached_archive: &Path,
    cached_libs: &Path,
) -> Result<(), String> {
    // Same per-key package identity as [`build`] ([`shim_version`]).
    let key = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    // Same [`toolchain_src_root`] materialization as [`build`] — the AOT shim's `[patch]` section
    // must unify a package's pinned toolchain tag onto this binary's own source too.
    let src_root = toolchain_src_root(toolchain)?;
    std::fs::write(
        dir.join("Cargo.toml"),
        aot_shim_cargo_toml(entries, toolchain, &src_root, rings, key),
    )
    .map_err(|err| format!("writing AOT shim Cargo.toml: {err}"))?;
    std::fs::write(dir.join("src").join("lib.rs"), aot_shim_lib_rs(entries))
        .map_err(|err| format!("writing AOT shim lib.rs: {err}"))?;
    let names: Vec<&str> = entries.iter().map(|e| e.identity.as_str()).collect();
    eprintln!(
        "noeta: composing the native AOT runtime with dependencies [{}] (building — later runs \
         reuse the cached archive)",
        names.join(", ")
    );
    let debug = std::env::var_os("NOETA_COMPOSE_DEBUG").is_some();
    let target_dir = std::env::var_os("NOETA_COMPOSE_TARGET_DIR")
        .map_or_else(|| dir.join("target"), PathBuf::from);
    let mut cmd = std::process::Command::new("cargo");
    cmd.arg("rustc");
    if !debug {
        cmd.arg("--release");
    }
    // Force plain output: under `CARGO_TERM_COLOR=always` the `native-static-libs` note arrives
    // ANSI-colored and a stray reset code lands inside the last `-l` flag (mirrors the stock scrape).
    let output = cmd
        .arg("--manifest-path")
        .arg(dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target_dir)
        .env("CARGO_TERM_COLOR", "never")
        .args(["--", "--print", "native-static-libs"])
        .output()
        .map_err(|err| {
            format!(
                "cannot run `cargo` (required to build native dependencies — install a Rust \
                 toolchain): {err}"
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "building the composed AOT runtime failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let libs = String::from_utf8_lossy(&output.stderr)
        .lines()
        .find_map(|l| l.split_once("native-static-libs:"))
        .map(|(_, libs)| libs.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_AOT_LIBS.join(" "));
    let built = target_dir
        .join(if debug { "debug" } else { "release" })
        .join(AOT_ARCHIVE_NAME);
    std::fs::create_dir_all(cached_archive.parent().expect("lib/ parent"))
        .and_then(|()| std::fs::copy(&built, cached_archive))
        .map_err(|err| {
            format!(
                "caching the composed AOT archive (`{}` → `{}`): {err}",
                built.display(),
                cached_archive.display()
            )
        })?;
    std::fs::write(cached_libs, &libs).map_err(|err| {
        format!(
            "caching the AOT link libs (`{}`): {err}",
            cached_libs.display()
        )
    })?;
    discard_build_scratch(dir, &target_dir);
    Ok(())
}

/// A conservative Linux native-link fallback when rustc's `native-static-libs` note is somehow
/// absent — matches the CLI's stock `default_native_libs`.
#[cfg(feature = "jit")]
const DEFAULT_AOT_LIBS: &[&str] = &[
    "-lgcc_s",
    "-lutil",
    "-lrt",
    "-lpthread",
    "-lm",
    "-ldl",
    "-lc",
];

/// The composed AOT runtime's manifest: a `staticlib` depending on `noeta-aot-runtime` (with its C
/// `main` OFF via `default-features = false`, forwarding the program's stdlib rings), `noeta-ext-abi`,
/// and each native crate at **default features** (a mixed crate's formatter stays stripped). The empty
/// `[workspace]` keeps it from being workspace-adopted; the release profile mirrors the toolchain's.
#[cfg(feature = "jit")]
fn aot_shim_cargo_toml(
    entries: &[Entry],
    toolchain: &ToolchainSource,
    src_root: &Path,
    rings: &[String],
    key: &str,
) -> String {
    // The dependency source spec (`path = …` in-workspace, `git = …, tag = …` out-of-workspace) that
    // extra keys (default-features/features/package) are appended to.
    let src_spec = |krate: &str| -> String {
        match toolchain {
            ToolchainSource::Workspace(root) => format!(
                "path = {}",
                toml_quote(&root.join("crates").join(krate).display().to_string())
            ),
            ToolchainSource::GitTag { repo, tag } => {
                format!("git = {}, tag = {}", toml_quote(repo), toml_quote(tag))
            }
        }
    };
    // Rings the AOT **base** (`noeta-aot-runtime`) owns — it forwards them to noeta-stdlib /
    // noeta-host-real. An **extension-owned** ring is not a base feature: the p2panda transport is
    // `ring-p2p` on the `para.p2p` *extension* crate, whose native tree is linked
    // through the entry crate that declares it (default-on there), not the base. So such a ring is
    // filtered out of the base feature set here — applying it to the base would be an unknown-feature
    // error. (Shedding p2panda from a para-*depending* but non-*importing* `--native` binary is a
    // future refinement — it would toggle the entry crate's own `ring-p2p` from the footprint scan.)
    const AOT_BASE_RINGS: &[&str] = &["ring-http-client", "ring-datetime", "ring-regex"];
    let ring_list = rings
        .iter()
        .filter(|r| AOT_BASE_RINGS.contains(&r.as_str()))
        .map(|r| toml_quote(r))
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = String::new();
    out.push_str(&format!(
        "# Generated by noeta (dev-deps) — a composed AOT runtime staticlib.\n\
             # Derived state: regenerated on dependency-set / ring changes. Do not edit.\n\n\
             [package]\nname = \"noeta-composed-aot\"\nversion = \"{}\"\nedition = \"2024\"\n\n\
             [lib]\nname = \"noeta_composed_aot\"\ncrate-type = [\"staticlib\"]\ntest = false\n\
             doctest = false\n\n[dependencies]\n",
        shim_version(key)
    ));
    out.push_str(&format!(
        "noeta-aot-runtime = {{ {}, default-features = false, features = [{ring_list}] }}\n",
        src_spec("noeta-aot-runtime")
    ));
    out.push_str(&format!(
        "noeta-ext-abi = {{ {} }}\n",
        src_spec("noeta-ext-abi")
    ));
    for (n, e) in entries.iter().enumerate() {
        // Footprint-gate the entry crate's rings: enable only those the program actually selected, so
        // a `--native` binary that depends on a native package but never imports the ring's modules
        // sheds that ring's native dep tree (e.g. a para-p2p-depending program that never touches
        // `para.p2p`/`para.synced` links no p2panda). A crate with no `ring-*` features is unaffected.
        let selected: Vec<&String> = e
            .ring_features
            .iter()
            .filter(|r| rings.iter().any(|s| s == *r))
            .collect();
        let features = if selected.is_empty() {
            String::new()
        } else {
            let list = selected
                .iter()
                .map(|f| toml_quote(f))
                .collect::<Vec<_>>()
                .join(", ");
            format!(", features = [{list}]")
        };
        out.push_str(&format!(
            "ext{n} = {{ package = {}, path = {}{features} }}\n",
            toml_quote(&e.cargo_name),
            toml_quote(&e.dir.display().to_string())
        ));
    }
    out.push_str(&toolchain_patch_section(src_root));
    out.push_str("\n[workspace]\n\n[profile.release]\ncodegen-units = 1\nlto = \"thin\"\n");
    out
}

/// The composed AOT runtime's entry: export a C-ABI `main` that aggregates every native crate's
/// `NOETA_EXTENSIONS` and hands them to `noeta_aot_runtime::run_embedded_with_extensions`, which
/// installs them into the registry (so the AOT-compiled bundle resolves its native
/// modules/types/tiers) and runs the embedded program. `noeta-aot-runtime`'s own `main` is off here
/// (its `entry` feature is disabled), so this is the sole entry. `Box::leak` gives the units the
/// `'static` the registry requires — they live for the process, like the stock binary's statics.
#[cfg(feature = "jit")]
fn aot_shim_lib_rs(entries: &[Entry]) -> String {
    let mut out = String::new();
    out.push_str(
        "//! Generated by noeta (dev-deps) — a composed AOT runtime staticlib. Do not edit.\n\
         //! Installs the app's native runtime extensions, then runs the embedded AOT program.\n\n\
         #[unsafe(no_mangle)]\n\
         pub extern \"C\" fn main() -> core::ffi::c_int {\n\
         \x20   let mut units: Vec<&'static (dyn noeta_ext_abi::Extension + Sync)> = Vec::new();\n",
    );
    for (n, e) in entries.iter().enumerate() {
        out.push_str(&format!(
            "    units.extend_from_slice(ext{n}::NOETA_EXTENSIONS); // {} ({})\n",
            e.identity, e.ident
        ));
    }
    // `noeta-aot-runtime`'s `[lib] name` is `noeta_aot`, so that is the crate path here.
    out.push_str(
        "    noeta_aot::run_embedded_with_extensions(Box::leak(units.into_boxed_slice()))\n}\n",
    );
    out
}

/// The generated shim's `[package] version`: `0.0.0` stamped with the compose key as semver build
/// metadata (`0.0.0+<key prefix>`). This ties the shim's cargo **package identity** to the
/// composition: every shim is named `noeta-composed` with a `src/main.rs`, and cargo's unit
/// fingerprint does not fold in the manifest's absolute path — so two *different* compositions
/// built into one shared target dir (the `NOETA_COMPOSE_TARGET_DIR` dev/test knob; production uses
/// a per-key target dir) could hash to the same unit, and the second build would be judged
/// "fresh" against the FIRST shim's fingerprint, silently caching a stale binary (observed as a
/// `[trust].commands` recompose that still lacked the newly trusted command). A distinct version
/// per key makes each composition its own cargo package, so that collision cannot exist.
fn shim_version(key: &str) -> String {
    let stamp = key.get(..12).unwrap_or(key);
    if stamp.is_empty() {
        "0.0.0".to_string()
    } else {
        format!("0.0.0+{stamp}")
    }
}

/// The generated shim manifest. `[workspace]` is deliberately empty so the shim never gets
/// adopted by an enclosing cargo workspace; the release profile mirrors the toolchain's own
/// (codegen-units=1 + thin LTO — the composed binary should perform like the stock one).
/// `key` is the compose key, stamped into the package version ([`shim_version`]).
fn shim_cargo_toml(
    entries: &[Entry],
    toolchain: &ToolchainSource,
    src_root: &Path,
    kind: ShimKind,
    key: &str,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Generated by noeta (package-manager Phase 3 / dev-deps D4c) — a composed shim.\n\
         # Derived state: regenerated on dependency-set changes. Do not edit.\n\n\
         [package]\nname = \"noeta-composed\"\nversion = \"{}\"\nedition = \"2024\"\n\n\
         [[bin]]\nname = \"noeta-composed\"\npath = \"src/main.rs\"\n\n[dependencies]\n",
        shim_version(key)
    ));
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
    // The base: the full toolchain (`noeta-cli`) for a dev composition, or the lean `noeta-runner`
    // for a shipped artifact's base (no fmt/LSP/DAP). `noeta-ext-abi` supplies the
    // extension ABI for both. Each `extN` (a native runtime crate) is pulled with **default features**
    // for a shipped base, so a mixed crate's formatter (gated behind its `fmt` feature) is *not*
    // compiled in; a **dev toolchain** additionally turns on the crate's declared dev features
    // so `noeta fmt` can reflow its tier bodies.
    out.push_str(&toolchain_dep(kind.base_crate()));
    out.push_str(&toolchain_dep("noeta-ext-abi"));
    for (n, e) in entries.iter().enumerate() {
        // A **dev-only** formatter crate is pulled trimmed: `default-features = false` sheds its heavy
        // deps, and only its declared dev feature(s) (`fmt`) turn its formatter code on. It appears
        // only in a Toolchain composition (excluded upstream from Runner/AotRuntime entry sets), so
        // there is no prod path here to trim it out of.
        if e.dev_only {
            let list = e
                .dev_features
                .iter()
                .map(|f| toml_quote(f))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!(
                "ext{n} = {{ package = {}, path = {}, default-features = false, features = [{list}] }}\n",
                toml_quote(&e.cargo_name),
                toml_quote(&e.dir.display().to_string())
            ));
            continue;
        }
        // A dev toolchain turns on the crate's dev features (`fmt`); a Toolchain *and* a Runner
        // (shipped `--exe`) both enable ALL of the crate's footprint rings — a runnable binary is
        // fully capable. (Only the AOT composition footprint-gates rings; see `aot_shim_cargo_toml`.)
        let mut feats: Vec<String> = Vec::new();
        if kind == ShimKind::Toolchain {
            feats.extend(e.dev_features.iter().cloned());
        }
        feats.extend(e.ring_features.iter().cloned());
        let features = if feats.is_empty() {
            String::new()
        } else {
            let list = feats
                .iter()
                .map(|f| toml_quote(f))
                .collect::<Vec<_>>()
                .join(", ");
            format!(", features = [{list}]")
        };
        out.push_str(&format!(
            "ext{n} = {{ package = {}, path = {}{features} }}\n",
            toml_quote(&e.cargo_name),
            toml_quote(&e.dir.display().to_string())
        ));
    }
    out.push_str(&toolchain_patch_section(src_root));
    out.push_str("\n[workspace]\n\n[profile.release]\ncodegen-units = 1\nlto = \"thin\"\n");
    out
}

/// The local source tree the shim's `[patch]` section redirects every toolchain crate to. For a
/// **workspace** toolchain this is the workspace itself; for a released (**git-tag**) toolchain it
/// is a cached checkout of the running binary's own release tag, materialized on first use under
/// `<cache>/toolchain-src/` ([`materialize_toolchain_checkout`]). Called only on a compose cache
/// **miss** (from [`build`]/[`build_aot_archive`]) — a cache hit never clones anything.
fn toolchain_src_root(toolchain: &ToolchainSource) -> Result<PathBuf, String> {
    match toolchain {
        ToolchainSource::Workspace(root) => Ok(root.clone()),
        ToolchainSource::GitTag { repo, tag } => materialize_toolchain_checkout(repo, tag),
    }
}

/// Clone the toolchain repo at `tag` into the cache (`<cache>/toolchain-src/<tag>-<repo hash>/`)
/// and return the checkout, reusing an existing one. Release tags are immutable by policy, so a
/// complete checkout (detected by its `crates/noeta-cli/Cargo.toml`) never goes stale. The clone
/// lands in a temp sibling first and is renamed into place, so a torn clone is never mistaken for
/// a checkout and a concurrent compose racing the same tag resolves to identical content.
fn materialize_toolchain_checkout(repo: &str, tag: &str) -> Result<PathBuf, String> {
    let cache = noeta_cache::Cache::locate()
        .ok_or("no cache directory could be resolved (set HOME or NOETA_CACHE_DIR)")?;
    let mut h = Sha256::new();
    h.update(repo.as_bytes());
    let repo_hash = hex(&h.finalize());
    let safe_tag: String = tag
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let dir = cache
        .join("toolchain-src")
        .join(format!("{safe_tag}-{}", &repo_hash[..12]));
    let complete = |d: &Path| {
        d.join("crates")
            .join("noeta-cli")
            .join("Cargo.toml")
            .is_file()
    };
    if complete(&dir) {
        return Ok(dir);
    }
    let _ = std::fs::remove_dir_all(&dir); // a torn earlier attempt
    std::fs::create_dir_all(dir.parent().expect("cache root parent")).map_err(|err| {
        format!(
            "cannot create `{}`: {err}",
            dir.parent().expect("cache root parent").display()
        )
    })?;
    let tmp = dir.with_file_name(format!(
        "{}.tmp-{}",
        dir.file_name().and_then(|n| n.to_str()).unwrap_or("clone"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let output = std::process::Command::new("git")
        .args(["clone", "--quiet", "--depth", "1", "--branch", tag, repo])
        .arg(&tmp)
        .output()
        .map_err(|err| {
            format!(
                "cannot run `git` (required to fetch the toolchain source `{repo}` at `{tag}` \
                 for composing native dependencies): {err}"
            )
        })?;
    if !output.status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!(
            "fetching the toolchain source (`{repo}` at tag `{tag}`) failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if let Err(err) = std::fs::rename(&tmp, &dir) {
        // A concurrent compose won the race — its checkout of the same immutable tag is identical.
        if complete(&dir) {
            let _ = std::fs::remove_dir_all(&tmp);
        } else {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(format!(
                "installing the toolchain checkout (`{}` → `{}`): {err}",
                tmp.display(),
                dir.display()
            ));
        }
    }
    Ok(dir)
}

/// The `[patch]` section that redirects a native package's git dependencies **on the canonical
/// toolchain repo** to *this* toolchain's own crates. It is what makes
/// an **out-of-tree** native package buildable: the package's entry crate depends on `noeta-ext-abi`
/// (and, for a first-party package, its own toolchain-resident impl crate) by git on the noeta repo —
/// resolvable in a standalone clone — and here the composer overrides every one of those with the
/// consumer's *exact* toolchain source. Without this the git crates would be a **second** copy of
/// `noeta-ext-abi`, so a `dyn Extension` from the package would not match the shim's
/// `noeta_ext_abi::Extension` type and the `NOETA_EXTENSIONS` aggregation would not type-check.
///
/// Emitted for **both** toolchain forms, always redirecting to local **paths** under `src_root`
/// ([`toolchain_src_root`] — the workspace itself, or the cached checkout of a released binary's
/// own tag). A git-tag toolchain must patch too: a package pinned at an *older* tag than the
/// running binary would otherwise resolve a **second** copy of every toolchain crate (the
/// every-release-breaks-every-published-package defect). And the patch must use `path` sources —
/// Cargo rejects a `[patch."<url>"]` entry whose own source is the same canonical URL *regardless
/// of a differing `tag`, `rev`, or `.git` suffix* ("points to the same source, but patches must
/// point to different sources"; canonical-URL comparison ignores the git ref — verified
/// empirically against cargo 1.97). A `path` source is a different source kind, always accepted,
/// and also covers the redundant case where the package already pins the binary's own tag.
///
/// Every `crates/*` member is patched to its path; Cargo ignores the unused ones (a package depends
/// on only a few), and the composed build captures cargo's output, so the unused-patch notes never
/// reach the user. Crate directory names equal their package names across the workspace, so the
/// directory name is the patch key.
fn toolchain_patch_section(src_root: &Path) -> String {
    // The git URL a native package references its toolchain crates by. Defaults to this build's
    // `repository`, overridable via `NOETA_TOOLCHAIN_REPO` for a fork, a private mirror, or a local
    // `file://` clone — the patch key must equal the URL the package's Cargo.toml declares.
    let repo = std::env::var("NOETA_TOOLCHAIN_REPO")
        .unwrap_or_else(|_| env!("CARGO_PKG_REPOSITORY").to_string());
    if repo.is_empty() {
        return String::new();
    }
    let crates_dir = src_root.join("crates");
    let Ok(entries) = std::fs::read_dir(&crates_dir) else {
        return String::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().join("Cargo.toml").is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort(); // deterministic shim content (folds into the compose cache key)
    if names.is_empty() {
        return String::new();
    }
    // Two patch tables, because a package may reference the contract crates either way. `[patch.<git
    // url>]` covers a git pin; `[patch.crates-io]` covers the published `noeta-ext-abi = "0.5"` form.
    // BOTH are required for one-toolchain-wins: a version dependency resolves the real crates.io
    // crate otherwise, which is a *second* copy of `noeta-ext-abi` — the package's `dyn Extension`
    // then fails to match the shim's `noeta_ext_abi::Extension` and `NOETA_EXTENSIONS` does not
    // type-check. Same failure the git patch exists to prevent, reached through the other door.
    let entries: String = names
        .iter()
        .map(|name| {
            format!(
                "{name} = {{ path = {} }}\n",
                toml_quote(&crates_dir.join(name).display().to_string())
            )
        })
        .collect();
    format!(
        "\n[patch.{}]\n{entries}\n[patch.crates-io]\n{entries}",
        toml_quote(&repo)
    )
}

/// The generated shim entry point: aggregate every entry crate's exported `NOETA_EXTENSIONS`
/// slice and hand the whole toolchain to `run_cli`, along with the **command bindings** — one per
/// `[trust.commands]` entry, each pairing the local name the command is registered under with the
/// exported command and the providing entry's units. Trust is tied to the providing
/// package identity (never to a namespace-root string, which would over-trust every package sharing
/// a scope root — trusting `para/db`'s commands must not trust all of `para/*`), and the local name
/// is what resolves a collision between two packages exporting the same command name.
/// `Box::leak` is fine — the units live for the process, exactly like the stock binary's statics.
fn shim_main_rs(
    entries: &[Entry],
    command_bindings: &[ResolvedCommandBinding],
    kind: ShimKind,
) -> String {
    let mut out = String::new();
    out.push_str(
        "//! Generated by noeta (package-manager Phase 3 / dev-deps D4c) — a composed shim. Do not edit.\n\n\
         fn main() -> std::process::ExitCode {\n\
         \x20   let mut units: Vec<&'static (dyn noeta_ext_abi::Extension + Sync)> = Vec::new();\n",
    );
    for (n, e) in entries.iter().enumerate() {
        if e.dev_only {
            // A dev-only formatter crate contributes ONLY its body formatters: wrap each of its units
            // in `formatter_only` so the toolchain sees no modules/types/commands/tiers from it —
            // only `name()`/`root()`/`body_formatters()`. (A dev-only entry reaches only a Toolchain
            // shim, whose base is `noeta-cli`, so `noeta_cli::formatter_only` is always in scope here.)
            out.push_str(&format!(
                "    for e in ext{n}::NOETA_EXTENSIONS {{ units.push(noeta_cli::formatter_only(*e)); }} // {} ({}, formatter-only)\n",
                e.identity, e.ident
            ));
            continue;
        }
        out.push_str(&format!(
            "    units.extend_from_slice(ext{n}::NOETA_EXTENSIONS); // {} ({})\n",
            e.identity, e.ident
        ));
    }
    match kind {
        // The dev toolchain: hand the whole extension set to `run_cli`, plus one command binding per
        // `[trust.commands]` entry — `run_cli` registers the provider's `exported` command under the
        // `local` name. A binding whose provider isn't among the entries can't arise (the graph only
        // resolves bindings for native packages, which all become entries), but skip defensively so
        // a stray one drops the command rather than mis-indexing a sibling.
        ShimKind::Toolchain => {
            out.push_str("    let command_bindings: Vec<noeta_cli::CommandBinding> = vec![\n");
            for b in command_bindings {
                let Some(n) = entries.iter().position(|e| e.identity == b.provider) else {
                    continue;
                };
                out.push_str(&format!(
                    "        noeta_cli::CommandBinding {{ local: {:?}, exported: {:?}, units: ext{n}::NOETA_EXTENSIONS }}, // {}\n",
                    b.local, b.exported, b.provider
                ));
            }
            out.push_str("    ];\n");
            out.push_str(
                "    noeta_cli::run_cli(\n\
                 \x20       Box::leak(units.into_boxed_slice()),\n\
                 \x20       &command_bindings,\n\
                 \x20   )\n}\n",
            );
        }
        // The shipped-artifact base: install the native runtime capabilities, then run the stapled
        // program. No CLI, no command-trust (a stapled artifact exposes no commands), no dev tooling.
        ShimKind::Runner => {
            out.push_str(
                "    noeta_runner::run_stapled_with_extensions(Box::leak(units.into_boxed_slice()))\n}\n",
            );
        }
        // The AOT runtime is a staticlib, not a bin — it is generated by `aot_shim_lib_rs`, never here.
        #[cfg(feature = "jit")]
        ShimKind::AotRuntime => {
            unreachable!("the AOT runtime shim is a staticlib (aot_shim_lib_rs), not a bin main")
        }
    }
    out
}

/// Minimal TOML basic-string quoting (paths, names, URLs — never control characters).
fn toml_quote(s: &str) -> String {
    noeta_pm::toml_quote(s)
}

/// Hand the invocation to the composed binary: same argv, [`COMPOSED_GUARD`] set to the identities
/// it links. On unix this replaces the process (`exec`); elsewhere it waits and exits with the
/// child's code.
fn exec(binary: &Path, crates: &[NativeCrate]) -> Result<Never, String> {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    // The guard doubles as the composition manifest: the runtime identities this binary links, so a
    // server running *inside* it can tell whether a project it is asked about is one it can link.
    // Dev-only crates are excluded — they contribute nothing a program can import, so naming them
    // would claim a coverage the composition does not give.
    let stamp = noeta_pm::composed::env_value(
        &crates
            .iter()
            .filter(|nc| !nc.dev_only)
            .map(|nc| nc.identity.clone())
            .collect::<Vec<_>>(),
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(binary)
            .args(&args)
            .env(COMPOSED_GUARD, &stamp)
            .exec();
        Err(format!("exec `{}` failed: {err}", binary.display()))
    }
    #[cfg(not(unix))]
    {
        let status = std::process::Command::new(binary)
            .args(&args)
            .env(COMPOSED_GUARD, &stamp)
            .status()
            .map_err(|err| format!("running `{}` failed: {err}", binary.display()))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_key_changes_when_the_crate_source_changes() {
        // The stale-composed-binary regression: a PATH dependency's dir never changes on edit, so
        // the key must fold in the package tree's content hash — two compositions identical in
        // everything but the hash cache to different binaries.
        let toolchain = ToolchainSource::GitTag {
            repo: "https://example.com/noeta".to_string(),
            tag: "v0.1.0".to_string(),
        };
        let before = compose_key(&entries(), &toolchain, &[], ShimKind::Toolchain, &[]);
        let mut edited = entries();
        edited[0].content_hash = "editedhash".to_string();
        let after = compose_key(&edited, &toolchain, &[], ShimKind::Toolchain, &[]);
        assert_ne!(
            before, after,
            "an edit to a path-dep native crate's source must recompose"
        );
    }

    fn entries() -> Vec<Entry> {
        vec![Entry {
            identity: "acme/imgfx".to_string(),
            dir: PathBuf::from("/store/acme_imgfx/native"),
            content_hash: "testhash".to_string(),
            cargo_name: "imgfx-native".to_string(),
            ident: "imgfx_native".to_string(),
            dev_features: vec![],
            ring_features: vec![],
            dev_only: false,
        }]
    }

    /// A mixed crate that declares the conventional `fmt` dev feature (its formatter + parser).
    fn mixed_entries() -> Vec<Entry> {
        vec![Entry {
            identity: "acme/imgfx".to_string(),
            dir: PathBuf::from("/store/acme_imgfx/native"),
            content_hash: "testhash".to_string(),
            cargo_name: "imgfx-native".to_string(),
            ident: "imgfx_native".to_string(),
            dev_features: vec!["fmt".to_string()],
            ring_features: vec![],
            dev_only: false,
        }]
    }

    /// A **dev-only** formatter crate (`package.dev-native`) — admitted untrusted, composed
    /// formatter-only. It declares the `fmt` dev feature (the code its formatter needs).
    fn dev_only_entries() -> Vec<Entry> {
        vec![Entry {
            identity: "acme/htmlfmt".to_string(),
            dir: PathBuf::from("/store/acme_htmlfmt/native"),
            content_hash: "testhash".to_string(),
            cargo_name: "htmlfmt-native".to_string(),
            ident: "htmlfmt_native".to_string(),
            dev_features: vec!["fmt".to_string()],
            ring_features: vec![],
            dev_only: true,
        }]
    }

    /// A crate declaring a footprint ring (like the para-p2p package's native `ring-p2p`).
    fn ring_entries() -> Vec<Entry> {
        vec![Entry {
            identity: "acme/imgfx".to_string(),
            dir: PathBuf::from("/store/acme_imgfx/native"),
            content_hash: "testhash".to_string(),
            cargo_name: "imgfx-native".to_string(),
            ident: "imgfx_native".to_string(),
            dev_features: vec![],
            ring_features: vec!["ring-p2p".to_string()],
            dev_only: false,
        }]
    }

    #[test]
    fn shim_package_identity_is_stamped_with_the_compose_key() {
        // Two different compositions must be two different cargo PACKAGES: every shim shares the
        // name `noeta-composed` and a `src/main.rs`, and cargo's unit fingerprint ignores the
        // manifest's absolute path — so in a shared target dir (`NOETA_COMPOSE_TARGET_DIR`) a
        // second composition would be judged "fresh" against the first's fingerprint and a STALE
        // binary would be cached (a `[trust].commands` recompose that still lacks the command).
        // The compose key stamped into the version as semver build metadata breaks the tie.
        let ws = ToolchainSource::Workspace(PathBuf::from("/src/noeta"));
        let root = Path::new("/src/noeta");
        let a = shim_cargo_toml(
            &entries(),
            &ws,
            root,
            ShimKind::Toolchain,
            "cafe0123deadbeef",
        );
        let b = shim_cargo_toml(
            &entries(),
            &ws,
            root,
            ShimKind::Toolchain,
            "beefbeefbeefbeef",
        );
        assert!(a.contains("version = \"0.0.0+cafe0123dead\""), "{a}");
        assert!(b.contains("version = \"0.0.0+beefbeefbeef\""), "{b}");
        // A keyless call (unit-test convenience) still yields a VALID semver — no dangling `+`.
        assert_eq!(shim_version(""), "0.0.0");
    }

    /// The AOT half of the row above: the composed staticlib shares the same per-key identity, so a
    /// shared target dir cannot serve one composition's archive for another's.
    #[cfg(feature = "jit")]
    #[test]
    fn aot_shim_package_identity_is_stamped_with_the_compose_key() {
        let ws = ToolchainSource::Workspace(PathBuf::from("/src/noeta"));
        let root = Path::new("/src/noeta");
        let aot = aot_shim_cargo_toml(&entries(), &ws, root, &[], "cafe0123deadbeef");
        assert!(aot.contains("version = \"0.0.0+cafe0123dead\""), "{aot}");
    }

    #[test]
    fn shim_manifest_uses_path_deps_in_workspace_form() {
        let toml = shim_cargo_toml(
            &entries(),
            &ToolchainSource::Workspace(PathBuf::from("/src/noeta")),
            Path::new("/src/noeta"),
            ShimKind::Toolchain,
            "cafe0123deadbeef",
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
                repo: "https://github.com/noeta-lang/noeta".to_string(),
                tag: "v0.1.0".to_string(),
            },
            Path::new("/nonexistent/checkout"),
            ShimKind::Toolchain,
            "cafe0123deadbeef",
        );
        assert!(
            toml.contains(
                "noeta-cli = { git = \"https://github.com/noeta-lang/noeta\", tag = \"v0.1.0\" }"
            ),
            "{toml}"
        );
    }

    #[test]
    fn shim_patches_noeta_repo_git_crates_to_the_workspace_for_out_of_tree_packages() {
        // A workspace toolchain with two crates. `toolchain_patch_section` must emit a `[patch]` on
        // the canonical repo URL redirecting each `crates/*` member to its path — so a native package
        // that git-deps the noeta repo unifies its `noeta_ext_abi::Extension` with the shim's.
        let root = noeta_test_temp::unique_path("patch-test");
        for c in ["noeta-ext-abi", "noeta-vm"] {
            let d = root.join("crates").join(c);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("Cargo.toml"), format!("[package]\nname = \"{c}\"\n")).unwrap();
        }
        let toml = shim_cargo_toml(
            &entries(),
            &ToolchainSource::Workspace(root.clone()),
            &root,
            ShimKind::Toolchain,
            "cafe0123deadbeef",
        );

        // A `[patch]` on the toolchain repo (this build's `repository`, or a `NOETA_TOOLCHAIN_REPO`
        // override) redirecting each `crates/*` member to its path.
        assert!(toml.contains("[patch."), "a [patch] section:\n{toml}");
        assert!(
            toml.contains(&format!(
                "noeta-ext-abi = {{ path = \"{}\" }}",
                root.join("crates").join("noeta-ext-abi").display()
            )),
            "each crate redirected to its path:\n{toml}"
        );
        assert!(toml.contains("noeta-vm = { path ="), "{toml}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn gittag_shim_patches_all_toolchain_crates_to_the_binaries_own_tag() {
        // The every-release-breaks-every-published-package defect: a released (git-tag) binary
        // composing a package whose crates pin an OLDER toolchain tag resolved TWO copies of
        // `noeta-ext-abi` (the shim's tag + the package's pin), so the package's `dyn Extension`
        // was a different type than the shim's and the compose build failed with E0308. The fix:
        // a git-tag toolchain emits the same `[patch]` as a workspace one, redirecting every
        // toolchain crate to the cached checkout of the BINARY's own tag — `path` entries, because
        // Cargo rejects a patch whose source is the same canonical git URL regardless of a
        // differing `tag`/`rev`/`.git` suffix (verified against cargo 1.97).
        let checkout = noeta_test_temp::unique_path("gittag-patch-test");
        for c in ["noeta-ext-abi", "noeta-stdlib"] {
            let d = checkout.join("crates").join(c);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("Cargo.toml"), format!("[package]\nname = \"{c}\"\n")).unwrap();
        }
        let git = shim_cargo_toml(
            &entries(),
            &ToolchainSource::GitTag {
                repo: "https://github.com/noeta-lang/noeta".to_string(),
                tag: "v0.2.1".to_string(),
            },
            &checkout,
            ShimKind::Toolchain,
            "cafe0123deadbeef",
        );
        // The shim's own deps still pin the binary's tag by git…
        assert!(
            git.contains(
                "noeta-cli = { git = \"https://github.com/noeta-lang/noeta\", tag = \"v0.2.1\" }"
            ),
            "{git}"
        );
        // …and the `[patch]` (keyed on the canonical repo URL a package's Cargo.toml declares)
        // redirects every toolchain crate — the shim's own deps AND any tag the package pinned —
        // to the binary's-tag checkout.
        assert!(git.contains("[patch."), "a [patch] section:\n{git}");
        for c in ["noeta-ext-abi", "noeta-stdlib"] {
            assert!(
                git.contains(&format!(
                    "{c} = {{ path = \"{}\" }}",
                    checkout.join("crates").join(c).display()
                )),
                "crate `{c}` redirected to the binary's-tag checkout:\n{git}"
            );
        }
        let _ = std::fs::remove_dir_all(&checkout);
    }

    #[test]
    fn footprint_rings_are_full_in_a_runnable_shim() {
        let ws = ToolchainSource::Workspace(PathBuf::from("/src/noeta"));
        let root = Path::new("/src/noeta");

        // A Toolchain (and a Runner) shim enables ALL of an entry crate's rings — a runnable binary
        // is fully capable, so `noeta run` / `--exe` get real p2p.
        for kind in [ShimKind::Toolchain, ShimKind::Runner] {
            let toml = shim_cargo_toml(&ring_entries(), &ws, root, kind, "cafe0123deadbeef");
            assert!(
                toml.contains("ext0 = { package = \"imgfx-native\", path = \"/store/acme_imgfx/native\", features = [\"ring-p2p\"] }"),
                "{kind:?} shim enables the ring:\n{toml}"
            );
        }
    }

    /// The contrast with the row above: where a runnable shim is fully capable, the AOT staticlib
    /// gates each ring on the program's measured footprint.
    #[cfg(feature = "jit")]
    #[test]
    fn footprint_rings_are_gated_in_the_aot_shim() {
        let ws = ToolchainSource::Workspace(PathBuf::from("/src/noeta"));
        let root = Path::new("/src/noeta");

        // Selected ⇒ enabled …
        let selected = aot_shim_cargo_toml(
            &ring_entries(),
            &ws,
            root,
            &["ring-p2p".to_string()],
            "cafe0123deadbeef",
        );
        assert!(
            selected.contains("ext0 = { package = \"imgfx-native\", path = \"/store/acme_imgfx/native\", features = [\"ring-p2p\"] }"),
            "selected ring enabled:\n{selected}"
        );
        // … not selected (the program never imports the ring's modules) ⇒ shed (no features).
        let shed = aot_shim_cargo_toml(&ring_entries(), &ws, root, &[], "cafe0123deadbeef");
        assert!(
            shed.contains(
                "ext0 = { package = \"imgfx-native\", path = \"/store/acme_imgfx/native\" }"
            ),
            "unselected ring shed:\n{shed}"
        );
    }

    #[test]
    fn runner_shim_manifest_bases_on_noeta_runner_not_the_cli() {
        // A shipped artifact's base is the LEAN runner — no `noeta-cli` (no dev tooling).
        let toml = shim_cargo_toml(
            &entries(),
            &ToolchainSource::Workspace(PathBuf::from("/src/noeta")),
            Path::new("/src/noeta"),
            ShimKind::Runner,
            "cafe0123deadbeef",
        );
        assert!(
            toml.contains("noeta-runner = { path = \"/src/noeta/crates/noeta-runner\" }"),
            "{toml}"
        );
        assert!(
            !toml.contains("noeta-cli ="),
            "a composed runner must not link the full CLI:\n{toml}"
        );
        // The native runtime crate is still linked (for its tier handler / modules).
        assert!(toml.contains("ext0 = { package = \"imgfx-native\","));
    }

    #[test]
    fn dev_toolchain_enables_a_mixed_crates_fmt_feature() {
        // A composed *dev toolchain* turns on the crate's declared `fmt` feature, so
        // `noeta fmt` can reflow its tier bodies (the formatter + its parser compile in).
        let toml = shim_cargo_toml(
            &mixed_entries(),
            &ToolchainSource::Workspace(PathBuf::from("/src/noeta")),
            Path::new("/src/noeta"),
            ShimKind::Toolchain,
            "cafe0123deadbeef",
        );
        assert!(
            toml.contains(
                "ext0 = { package = \"imgfx-native\", path = \"/store/acme_imgfx/native\", \
                 features = [\"fmt\"] }"
            ),
            "{toml}"
        );
    }

    #[test]
    fn a_dev_only_formatter_crate_is_composed_trimmed_and_wrapped() {
        // A `package.dev-native` formatter crate is admitted trust-free and stripped to formatter-only:
        // in the dev toolchain manifest it is pulled `default-features = false, features = ["fmt"]`
        // (only its formatter code, no heavy deps), and its `main.rs` wraps each unit through
        // `formatter_only` so nothing that runs reaches the toolchain.
        let src = ToolchainSource::Workspace(PathBuf::from("/src/noeta"));
        let toml = shim_cargo_toml(
            &dev_only_entries(),
            &src,
            Path::new("/src/noeta"),
            ShimKind::Toolchain,
            "cafe0123deadbeef",
        );
        assert!(
            toml.contains(
                "ext0 = { package = \"htmlfmt-native\", path = \"/store/acme_htmlfmt/native\", \
                 default-features = false, features = [\"fmt\"] }"
            ),
            "a dev-only crate is trimmed to its fmt feature:\n{toml}"
        );
        let main = shim_main_rs(&dev_only_entries(), &[], ShimKind::Toolchain);
        assert!(
            main.contains(
                "for e in ext0::NOETA_EXTENSIONS { units.push(noeta_cli::formatter_only(*e)); }"
            ),
            "a dev-only crate's units are wrapped formatter-only, not extended raw:\n{main}"
        );
        assert!(
            !main.contains("units.extend_from_slice(ext0::NOETA_EXTENSIONS)"),
            "a dev-only crate must never contribute its full extension surface:\n{main}"
        );
    }

    #[test]
    fn shipped_runner_keeps_a_mixed_crate_at_default_features() {
        // The shipped base pulls the same crate at *default* features — its `fmt`
        // feature stays off, so the formatter and its parser never enter the artifact.
        let toml = shim_cargo_toml(
            &mixed_entries(),
            &ToolchainSource::Workspace(PathBuf::from("/src/noeta")),
            Path::new("/src/noeta"),
            ShimKind::Runner,
            "cafe0123deadbeef",
        );
        assert!(
            toml.contains(
                "ext0 = { package = \"imgfx-native\", path = \"/store/acme_imgfx/native\" }"
            ),
            "the runner base must not enable any dev feature:\n{toml}"
        );
        assert!(
            !toml.contains("features = [\"fmt\"]"),
            "a shipped runner must not turn on a formatter feature:\n{toml}"
        );
    }

    #[cfg(feature = "jit")]
    #[test]
    fn aot_shim_manifest_is_a_lean_staticlib_forwarding_rings() {
        // dev-deps `--native` gap: the composed AOT runtime is a lean `staticlib` (no CLI/runner base)
        // that forwards the program's rings to `noeta-aot-runtime` with its C `main` OFF, and pulls the
        // native crate at *default* features (formatter stripped).
        let toml = aot_shim_cargo_toml(
            &mixed_entries(),
            &ToolchainSource::Workspace(PathBuf::from("/src/noeta")),
            Path::new("/src/noeta"),
            &["ring-http-client".to_string()],
            "cafe0123deadbeef",
        );
        assert!(toml.contains("crate-type = [\"staticlib\"]"), "{toml}");
        assert!(
            toml.contains(
                "noeta-aot-runtime = { path = \"/src/noeta/crates/noeta-aot-runtime\", \
                 default-features = false, features = [\"ring-http-client\"] }"
            ),
            "{toml}"
        );
        // The native crate is linked at default features — no `fmt`, even for a mixed crate.
        assert!(
            toml.contains(
                "ext0 = { package = \"imgfx-native\", path = \"/store/acme_imgfx/native\" }"
            ),
            "{toml}"
        );
        assert!(!toml.contains("features = [\"fmt\"]"), "{toml}");
        // No dev-tooling base is dragged into a shipped AOT runtime.
        assert!(!toml.contains("noeta-cli ="), "{toml}");
        assert!(!toml.contains("noeta-runner ="), "{toml}");
    }

    #[cfg(feature = "jit")]
    #[test]
    fn aot_shim_manifest_with_no_rings_forwards_an_empty_feature_set() {
        let toml = aot_shim_cargo_toml(
            &entries(),
            &ToolchainSource::Workspace(PathBuf::from("/src/noeta")),
            Path::new("/src/noeta"),
            &[],
            "cafe0123deadbeef",
        );
        assert!(
            toml.contains("default-features = false, features = [] }"),
            "{toml}"
        );
    }

    #[cfg(feature = "jit")]
    #[test]
    fn aot_shim_lib_installs_units_and_runs_embedded() {
        // The composed AOT runtime exports a C `main` that installs the app's native units then runs
        // the embedded program — no `run_cli`, no stapled-runner call.
        let lib = aot_shim_lib_rs(&entries());
        assert!(lib.contains("#[unsafe(no_mangle)]"), "{lib}");
        assert!(
            lib.contains("pub extern \"C\" fn main() -> core::ffi::c_int"),
            "{lib}"
        );
        assert!(
            lib.contains("units.extend_from_slice(ext0::NOETA_EXTENSIONS);"),
            "{lib}"
        );
        assert!(
            lib.contains("noeta_aot::run_embedded_with_extensions(Box::leak("),
            "{lib}"
        );
        assert!(!lib.contains("run_cli"), "{lib}");
        assert!(!lib.contains("run_stapled_with_extensions"), "{lib}");
    }

    /// A resolved command binding for the shim tests.
    fn binding(local: &str, provider: &str, exported: &str) -> ResolvedCommandBinding {
        ResolvedCommandBinding {
            local: local.to_string(),
            provider: provider.to_string(),
            exported: exported.to_string(),
        }
    }

    #[test]
    fn shim_main_aggregates_unit_slices() {
        let bindings = vec![binding("blur", "acme/imgfx", "blur")];
        let main = shim_main_rs(&entries(), &bindings, ShimKind::Toolchain);
        assert!(main.contains("units.extend_from_slice(ext0::NOETA_EXTENSIONS);"));
        assert!(main.contains("noeta_cli::run_cli("));
        // The binding emits a `CommandBinding` tying the local name to the provider's exported
        // command and its units — trust keyed by the providing package's IDENTITY (its entry index),
        // no root-name strings anywhere.
        assert!(
            main.contains(
                "noeta_cli::CommandBinding { local: \"blur\", exported: \"blur\", units: ext0::NOETA_EXTENSIONS }"
            ),
            "{main}"
        );
        assert!(main.contains("&command_bindings,"));
    }

    #[test]
    fn shim_main_registers_commands_for_a_scope_keyed_identity_only_when_bound() {
        // The para/db-shaped defect: a scope-keyed package's dependency ROOT SEGMENT (`db`) differs
        // from its extensions' namespace root (`para`), so any root-name matching drops (or, matched
        // the other way, over-grants) its commands. Keyed by identity (the entry index), a binding
        // for `para/db` emits against ext0 only; the untrusted sibling ext1 gets no binding.
        let entries = vec![
            Entry {
                identity: "para/db".to_string(),
                dir: PathBuf::from("/store/para_db/native"),
                content_hash: "dbhash".to_string(),
                cargo_name: "para-db-native".to_string(),
                ident: "para_db_native".to_string(),
                dev_features: vec![],
                ring_features: vec![],
                dev_only: false,
            },
            Entry {
                identity: "para/p2p".to_string(),
                dir: PathBuf::from("/store/para_p2p/native"),
                content_hash: "p2phash".to_string(),
                cargo_name: "para-p2p-native".to_string(),
                ident: "para_p2p_native".to_string(),
                dev_features: vec![],
                ring_features: vec![],
                dev_only: false,
            },
        ];
        let bindings = vec![binding("migrate", "para/db", "migrate")];
        let main = shim_main_rs(&entries, &bindings, ShimKind::Toolchain);
        // Both packages' units join the toolchain…
        assert!(main.contains("units.extend_from_slice(ext0::NOETA_EXTENSIONS); // para/db"));
        assert!(main.contains("units.extend_from_slice(ext1::NOETA_EXTENSIONS); // para/p2p"));
        // …but only the bound package's command is registered, and against ext0 (para/db): trusting
        // `para/db` must NOT trust every `para/*` package's commands.
        assert!(
            main.contains(
                "noeta_cli::CommandBinding { local: \"migrate\", exported: \"migrate\", units: ext0::NOETA_EXTENSIONS }"
            ),
            "{main}"
        );
        assert!(
            !main.contains("units: ext1::NOETA_EXTENSIONS }"),
            "the untrusted sibling's units must not back a command binding:\n{main}"
        );
    }

    #[test]
    fn shim_main_emits_a_renamed_binding_under_its_local_name() {
        // The escape hatch: a binding whose local name differs from the exported command name is
        // emitted under the local name, resolving a collision with another package's same-named
        // command. The provider's exported name is what run_cli looks up in the units.
        let bindings = vec![binding("undo", "acme/imgfx", "rollback")];
        let main = shim_main_rs(&entries(), &bindings, ShimKind::Toolchain);
        assert!(
            main.contains(
                "noeta_cli::CommandBinding { local: \"undo\", exported: \"rollback\", units: ext0::NOETA_EXTENSIONS }"
            ),
            "{main}"
        );
    }

    #[test]
    fn shim_main_with_no_bindings_emits_an_empty_binding_list() {
        let main = shim_main_rs(&entries(), &[], ShimKind::Toolchain);
        assert!(main.contains("let command_bindings: Vec<noeta_cli::CommandBinding> = vec!["));
        assert!(!main.contains("noeta_cli::CommandBinding {"));
        assert!(main.contains("&command_bindings,"));
    }

    #[test]
    fn runner_shim_main_installs_units_and_runs_stapled() {
        // The runner shim installs the native runtime units then runs the stapled
        // program — no `run_cli`, no command-trust (a stapled artifact exposes no CLI).
        let bindings = vec![binding("blur", "acme/imgfx", "blur")];
        let main = shim_main_rs(&entries(), &bindings, ShimKind::Runner);
        assert!(main.contains("units.extend_from_slice(ext0::NOETA_EXTENSIONS);"));
        assert!(main.contains("noeta_runner::run_stapled_with_extensions(Box::leak("));
        assert!(
            !main.contains("run_cli"),
            "the runner base must not call run_cli"
        );
        assert!(!main.contains("command_bindings"));
    }

    /// The compose dir's own target dir is build scratch and is dropped once the artifact is
    /// copied out — a 1.4G tail beside a 51M binary, retained forever by a cache nothing evicts.
    #[test]
    fn build_scratch_is_dropped_when_it_is_the_compose_dirs_own() {
        let dir = noeta_test_temp::TempDir::new("compose-scratch-own");
        let target = dir.join("target");
        std::fs::create_dir_all(target.join("release")).expect("create scratch");
        std::fs::write(dir.join("Cargo.toml"), "[package]\n").expect("write manifest");

        discard_build_scratch(&dir, &target);

        assert!(
            !target.exists(),
            "the compose dir's own target must be dropped"
        );
        assert!(
            dir.join("Cargo.toml").is_file(),
            "only the target dir goes — the compose entry itself stays, so the cache still hits"
        );
    }

    /// The dangerous half. Under `NOETA_COMPOSE_TARGET_DIR` the target dir belongs to the caller —
    /// the e2e tests point it at the workspace's own build directory — so it must never be removed.
    #[test]
    fn build_scratch_outside_the_compose_dir_is_left_alone() {
        let dir = noeta_test_temp::TempDir::new("compose-scratch-external");
        let external = noeta_test_temp::TempDir::new("compose-scratch-external-target");

        discard_build_scratch(&dir, &external);

        assert!(
            external.is_dir(),
            "a caller-supplied target dir must survive — deleting it would destroy work this \
             function never created"
        );
    }
}
