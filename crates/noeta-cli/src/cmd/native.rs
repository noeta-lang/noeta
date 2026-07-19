//! The native/wasm AOT toolchain drivers: locating (or workspace-building) the runtime
//! bases, ring-feature selection, and the `cc` link step behind `noeta build --native`.

use std::process::ExitCode;

use crate::compose;

/// Locate the generic `noeta-wasm-serve` component to staple into — the serve twin of
/// [`resolve_wasm_runner`]'s ladder: `NOETA_WASM_SERVE` → next to this binary → the workspace
/// build, compiled on demand (interim; a packaged toolchain ships the component).
pub(crate) fn resolve_serve_component() -> Result<std::path::PathBuf, String> {
    if let Ok(path) = std::env::var("NOETA_WASM_SERVE") {
        return Ok(std::path::PathBuf::from(path));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("noeta-wasm-serve.wasm");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    // Interim workspace build. `--locked` is deliberately absent: this is the dev-tree path.
    let output = std::process::Command::new("cargo")
        .args([
            "build",
            "-p",
            "noeta-wasm-serve",
            "--target",
            "wasm32-wasip2",
            "--profile",
            "wasm-release",
        ])
        .output()
        .map_err(|e| format!("cannot run cargo to build the serve component: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "building the serve component failed (is the wasm32-wasip2 target installed? \
             `rustup target add wasm32-wasip2`):\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let artifact = workspace_target_dir()?
        .join("wasm32-wasip2")
        .join("wasm-release")
        .join("noeta_wasm_serve.wasm");
    if !artifact.is_file() {
        return Err(format!(
            "the serve component was not found at {} after building",
            artifact.display()
        ));
    }
    Ok(artifact)
}

/// Locate the lean `noeta-runner` native binary to staple a `--exe` bundle onto (dev-deps D4a): the
/// production runtime that links only app-execution layers — no fmt/LSP/DAP/formatter parsers.
/// Priority mirrors [`resolve_wasm_runner`]: an explicit `NOETA_RUNNER` (packaged/hermetic path) → a
/// runner shipped next to this toolchain binary → the workspace build, compiled on demand.
///
/// The workspace build uses **`-p noeta-runner`** (not `--workspace`): building the runner as its own
/// crate graph keeps `noeta-pm` at `[]` features so cargo's feature unification cannot turn
/// `fmt-config` on and drag `noeta-fmt` into the artifact — the D3c build-isolation invariant.
/// `--release` because a shipped artifact wants an optimized runtime. Packaging the runner with a
/// shipped toolchain is the same later distribution decision as the wasm runner's.
/// The base binary a `--exe` artifact staples onto: a **composed runner** (the lean runner + the
/// app's native runtime extensions, dev tooling off — dev-deps D4c) when the app's dependency graph
/// carries native crates, else the **stock** lean `noeta-runner` ([`resolve_native_runner`]). Both
/// bases are free of dev tooling; the composed one additionally carries the runtime handlers the
/// shipped program needs (without which the artifact would fail on an unknown native module).
pub(crate) fn runner_base(file: &std::path::Path) -> Result<std::path::PathBuf, String> {
    match compose::compose_runner_binary(file)? {
        Some(composed) => Ok(composed),
        None => resolve_native_runner(),
    }
}

pub(crate) fn resolve_native_runner() -> Result<std::path::PathBuf, String> {
    let bin_name = if cfg!(windows) {
        "noeta-runner.exe"
    } else {
        "noeta-runner"
    };
    if let Ok(path) = std::env::var("NOETA_RUNNER") {
        return Ok(std::path::PathBuf::from(path));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(bin_name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    // Interim workspace build. `--locked` is deliberately absent (dev-tree path); `-p` isolation is
    // load-bearing (see doc) — never `--workspace`.
    let output = std::process::Command::new("cargo")
        .args(["build", "-p", "noeta-runner", "--release"])
        .output()
        .map_err(|e| format!("cannot run cargo to build the lean runtime: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "building the lean runtime (`noeta-runner`) failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let artifact = workspace_target_dir()?.join("release").join(bin_name);
    if !artifact.is_file() {
        return Err(format!(
            "the lean runtime was not found at {} after building",
            artifact.display()
        ));
    }
    Ok(artifact)
}

/// Locate the `noeta-wasm-runner` wasm32-wasip1 binary to staple into. Priority: an explicit
/// `NOETA_WASM_RUNNER` (the packaged/hermetic path) → a runner shipped next to this toolchain
/// binary → the workspace build, compiled on demand with cargo (interim: needs cargo + the
/// `wasm32-wasip1` target, mirroring `resolve_aot_runtime`'s ladder). Packaging the runner with
/// a shipped toolchain is the same later distribution decision as the AOT archive's.
pub(crate) fn resolve_wasm_runner() -> Result<std::path::PathBuf, String> {
    if let Ok(path) = std::env::var("NOETA_WASM_RUNNER") {
        return Ok(std::path::PathBuf::from(path));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("noeta-wasm-runner.wasm");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    // Interim workspace build. `--locked` is deliberately absent: this is the dev-tree path.
    let output = std::process::Command::new("cargo")
        .args([
            "build",
            "-p",
            "noeta-wasm-runner",
            "--target",
            "wasm32-wasip1",
            "--profile",
            "wasm-release",
        ])
        .output()
        .map_err(|e| format!("cannot run cargo to build the wasm runner: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "building the wasm runner failed (is the wasm32-wasip1 target installed? \
             `rustup target add wasm32-wasip1`):\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let artifact = workspace_target_dir()?
        .join("wasm32-wasip1")
        .join("wasm-release")
        .join("noeta-wasm-runner.wasm");
    if !artifact.is_file() {
        return Err(format!(
            "the wasm runner was not found at {} after building",
            artifact.display()
        ));
    }
    Ok(artifact)
}

/// Emit `module` as a **native** executable (P-AOT L3.2b(3)) — the final level. Steps:
///   1. AOT-compile every eligible prototype to a relocatable object (`compile_module_aot`), which
///      also defines the `noeta_aot_dispatch` table.
///   2. Link that object against the AOT runtime staticlib (`libnoeta_aot.a`) with a C toolchain
///      (`cc`) into a native binary: the runtime provides `main` + the `noeta_jit_*` helpers, the
///      object provides the native bodies + the dispatch table, and the linker resolves it all.
///   3. Staple the program's bundle onto that binary (the L2 mechanism), so at startup the runtime
///      recovers the module and binds the linked-in native bodies through the dispatch table.
///
/// The eligible prototypes run as machine code; ineligible ones interpret the same bytecode from the
/// stapled bundle — the identical hybrid the runtime JIT uses, just resolved at build time.
#[cfg(feature = "jit")]
pub(crate) fn emit_native(
    file: &std::path::Path,
    out: Option<&std::path::Path>,
    module: &noeta_bytecode::Module,
) -> ExitCode {
    let default_out = if cfg!(windows) {
        file.with_extension("exe")
    } else {
        file.with_extension("")
    };
    let out_path = out.map(std::path::Path::to_path_buf).unwrap_or(default_out);
    if out_path == file {
        eprintln!(
            "noeta: refusing to overwrite the source file {}; pass -o <path>",
            file.display()
        );
        return ExitCode::from(2);
    }

    // 1. AOT object.
    let object = match noeta_vm::compile_module_aot(module) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("noeta: AOT compile failed: {err}");
            return ExitCode::from(1);
        }
    };

    // A per-invocation scratch dir for the object + the pre-staple linked binary.
    let work = std::env::temp_dir().join(format!("noeta-aot-{}", std::process::id()));
    if let Err(err) = std::fs::create_dir_all(&work) {
        eprintln!("noeta: cannot create a build directory: {err}");
        return ExitCode::from(2);
    }
    let cleanup = |code: ExitCode| -> ExitCode {
        let _ = std::fs::remove_dir_all(&work);
        code
    };
    let obj_path = work.join("program.o");
    if let Err(err) = std::fs::write(&obj_path, &object) {
        eprintln!("noeta: cannot write the AOT object: {err}");
        return cleanup(ExitCode::from(2));
    }

    // 2. Locate the runtime archive + the system libs it must link with, then `cc`-link. The archive
    // is built with only the stdlib rings this program uses, so unused rings' native deps are dropped.
    // For a native-dependency app it is a *composed* AOT runtime (the lean runtime + those crates'
    // runtime extensions, dev tooling off — dev-deps); a pure-Noeta app uses the stock archive.
    // The import footprint selects each ring whose module the program references. A native package
    // with several drivers behind one module (para/db's SQLite + Postgres) picks its driver at runtime
    // from the dsn — invisible to a static scan — so union in any driver rings the manifest requests
    // (`[native] rings = [...]`). An undeclared name is harmlessly ignored by the composer.
    let mut rings = aot_ring_features(module);
    rings.extend(noeta_pm::manifest::native_rings(file));
    rings.sort();
    rings.dedup();
    let (archive, libs) = match aot_runtime_base(file, &rings) {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("noeta: {err}");
            return cleanup(ExitCode::from(1));
        }
    };
    let linked = work.join("linked");
    if let Err(err) = link_native(&obj_path, &archive, &libs, &linked) {
        eprintln!("noeta: link failed: {err}");
        return cleanup(ExitCode::from(1));
    }

    // 3. Staple the bundle onto the linked binary (L2), so the runtime recovers the module.
    let runtime = match std::fs::read(&linked) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("noeta: cannot read the linked binary: {err}");
            return cleanup(ExitCode::from(2));
        }
    };
    let blob = noeta_bundle::write(module);
    let image = noeta_bundle::staple(&runtime, &blob);
    if let Err(err) = std::fs::write(&out_path, &image) {
        eprintln!("noeta: cannot write {}: {err}", out_path.display());
        return cleanup(ExitCode::from(2));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) =
            std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(0o755))
        {
            eprintln!(
                "noeta: cannot mark {} executable: {err}",
                out_path.display()
            );
            return cleanup(ExitCode::from(2));
        }
    }
    eprintln!(
        "wrote {} ({} bytes, native AOT)",
        out_path.display(),
        image.len()
    );
    cleanup(ExitCode::SUCCESS)
}

/// Native AOT (`noeta build --native`) needs the JIT codegen; an interpreter-only build
/// (`--no-default-features`) has no AOT compiler, so it reports that rather than emitting a binary.
#[cfg(not(feature = "jit"))]
pub(crate) fn emit_native(
    _file: &std::path::Path,
    _out: Option<&std::path::Path>,
    _module: &noeta_bytecode::Module,
) -> ExitCode {
    eprintln!("noeta: native AOT (`--native`) requires the JIT-enabled build (default features)");
    ExitCode::from(2)
}

/// The stdlib rings the program actually needs, as `noeta-aot-runtime` cargo features — derived from
/// the native modules it references. `noeta build --native` builds the runtime archive with exactly
/// these features, so the linker drops every unused ring's native dependency tree (DCE Axis B — e.g.
/// an http-free program sheds reqwest + its ~5 MB TLS stack).
///
/// A native module surfaces in the bytecode three ways, all covered here (missing one would build a
/// binary that stubs out a ring the program actually calls):
/// - `Const::NativeModule(name)` — `use std.{http}` loads the whole module as a value, then member
///   calls (`http.get_async(...)`) dispatch on it via `CallMethod` (no `TypedModuleCall`).
/// - `Const::ModuleFn { module, .. }` — a selective import (`use std.http.get`).
/// - `Op::TypedModuleCall { module, .. }` — the fully-qualified / turbofish call form (`json.parse::<T>`).
///
/// Conservative by construction: a module with **no** ring row here is always-on core (Ring-1
/// string/list/map/set, the pure builders) or a ring not yet gated — either way it stays in the
/// default feature set and is never stripped. Only a module with an explicit row can turn its ring
/// off, so an unrecognized or newly-added module can never be silently dropped.
#[cfg(feature = "jit")]
pub(crate) fn aot_ring_features(module: &noeta_bytecode::Module) -> Vec<String> {
    use noeta_bytecode::{Const, Op};
    use std::collections::BTreeSet;

    let mut rings: BTreeSet<String> = BTreeSet::new();

    // A whole-module value (`use std.{http}`): the program holds the module and can call *any* of its
    // functions dynamically (member calls lower to `CallMethod` on the value, whose receiver isn't
    // statically pinned), so a module that owns a native-dep ring is selected conservatively.
    // Module identities are root-qualified (`std.http`); the ring tables key on the module name, so
    // strip the root before looking up. A turbofish's module is the source receiver (a local name,
    // unrooted) and passes through `module_name` unchanged.
    // The module→ring map is the registry's now (package-manager P1.0): each `ExtModule` declares its
    // `ring`, so both the whole-module and precisely-named forms funnel through one registry lookup —
    // no CLI-side table to keep in sync with the stdlib. `ring_of` accepts a root-qualified path, a
    // bare name, or a turbofish's bound local alike.
    let note_module = |name: &str, rings: &mut BTreeSet<String>| {
        if let Some(ring) = noeta_stdlib::registry::ring_of(name) {
            rings.insert(ring.to_string());
        }
    };
    // A precisely-named function (`use std.http.client.get`, or a turbofish `TypedModuleCall`): post
    // the `std.http` client/server split the client/server distinction lives in the *module* identity,
    // so a named function selects exactly its module's ring — the same `ring_of` lookup as a whole
    // module. This is what lets the split pay off: a program naming only `http.server` functions
    // selects no client ring and sheds reqwest, while any `http.client` reference selects it.
    let note_fn = |m: &str, _func: &str, rings: &mut BTreeSet<String>| {
        if let Some(ring) = noeta_stdlib::registry::ring_of(m) {
            rings.insert(ring.to_string());
        }
    };

    for chunk in &module.protos {
        for c in &chunk.consts {
            match c {
                Const::NativeModule(name) => note_module(name, &mut rings),
                Const::ModuleFn { module: m, func } => note_fn(m, func, &mut rings),
                _ => {}
            }
        }
        for op in &chunk.code {
            if let Op::TypedModuleCall {
                module: m, func, ..
            } = op
            {
                note_fn(module.name(*m), module.name(*func), &mut rings);
            }
        }
    }
    rings.into_iter().collect()
}

/// The AOT runtime base a `--native` artifact links against: a **composed** AOT runtime staticlib (the
/// lean `noeta-aot-runtime` + the app's native runtime extensions, dev tooling off — dev-deps) when the
/// app's dependency graph carries native crates, else the **stock** `libnoeta_aot.a`
/// ([`resolve_aot_runtime`]). Both are free of dev tooling; the composed one additionally installs the
/// runtime handlers the AOT-compiled program needs (without which it would abort on an unknown native
/// module) — the `--native` analogue of [`runner_base`], closing the last native-dependency gap.
#[cfg(feature = "jit")]
pub(crate) fn aot_runtime_base(
    file: &std::path::Path,
    rings: &[String],
) -> Result<(std::path::PathBuf, Vec<String>), String> {
    match compose::compose_aot_runtime_archive(file, rings)? {
        Some(pair) => Ok(pair),
        None => resolve_aot_runtime(rings),
    }
}

/// Locate the AOT runtime staticlib (`libnoeta_aot.a`) and the native system libraries it must be
/// linked against, built with exactly the stdlib `rings` the program needs (DCE Axis B).
/// Priority: an explicit `NOETA_AOT_RUNTIME_LIB` (paired with `NOETA_AOT_LINK_LIBS`,
/// space-separated) — the packaged/hermetic path, which supplies its own ring set so `rings` is
/// ignored — else build it from the workspace with `cargo rustc --no-default-features --features
/// <rings> … --print native-static-libs` (interim: needs cargo + the source tree), which both
/// produces the archive and prints the exact link line. Packaging the archive for a shipped toolchain
/// (so `--native` works outside the workspace) is a later distribution decision.
#[cfg(feature = "jit")]
pub(crate) fn resolve_aot_runtime(
    rings: &[String],
) -> Result<(std::path::PathBuf, Vec<String>), String> {
    if let Ok(path) = std::env::var("NOETA_AOT_RUNTIME_LIB") {
        let libs = std::env::var("NOETA_AOT_LINK_LIBS")
            .ok()
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_else(default_native_libs);
        return Ok((std::path::PathBuf::from(path), libs));
    }

    // Interim workspace build: one `cargo rustc` compiles the staticlib and prints its
    // native-static-libs note. `--no-default-features --features entry,<rings>` links only the rings
    // the program uses; the `aot` runtime support is a hard dep feature, so it survives regardless.
    // `entry` is forced on (it is *not* selected by `--no-default-features`) so the stock archive
    // exports its C `main` — only a composed AOT runtime omits it to supply its own.
    let mut args = vec![
        "rustc",
        "-p",
        "noeta-aot-runtime",
        "--release",
        "--no-default-features",
    ];
    let joined = std::iter::once("entry".to_string())
        .chain(rings.iter().cloned())
        .collect::<Vec<_>>()
        .join(",");
    args.push("--features");
    args.push(&joined);
    args.extend(["--", "--print", "native-static-libs"]);
    let output = std::process::Command::new("cargo")
        .args(&args)
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("cannot run cargo to build the AOT runtime: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "building the AOT runtime failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let notes = String::from_utf8_lossy(&output.stderr);
    let libs = notes
        .lines()
        .find_map(|l| l.split_once("native-static-libs:"))
        .map(|(_, libs)| {
            libs.split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(default_native_libs);
    let archive = workspace_target_dir()?
        .join("release")
        .join("libnoeta_aot.a");
    if !archive.exists() {
        return Err(format!(
            "the AOT runtime archive was not found at {} after building",
            archive.display()
        ));
    }
    Ok((archive, libs))
}

/// A conservative default native-link set for a Rust staticlib on Linux, used when the exact
/// `native-static-libs` note is unavailable (an explicit archive with no `NOETA_AOT_LINK_LIBS`).
#[cfg(feature = "jit")]
pub(crate) fn default_native_libs() -> Vec<String> {
    [
        "-lgcc_s",
        "-lutil",
        "-lrt",
        "-lpthread",
        "-lm",
        "-ldl",
        "-lc",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// The workspace's Cargo target directory: `CARGO_TARGET_DIR` if set, else `<workspace root>/target`
/// found by walking up from the current directory for the `Cargo.toml` that declares `[workspace]`.
/// Shared by the `--native` (jit builds) and `--wasm` interim workspace-build ladders.
pub(crate) fn workspace_target_dir() -> Result<std::path::PathBuf, String> {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        return Ok(std::path::PathBuf::from(dir));
    }
    let mut dir = std::env::current_dir().map_err(|e| e.to_string())?;
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.exists()
            && std::fs::read_to_string(&manifest)
                .map(|s| s.contains("[workspace]"))
                .unwrap_or(false)
        {
            return Ok(dir.join("target"));
        }
        if !dir.pop() {
            return Err(
                "cannot find the workspace root; run `noeta build --native` from inside the \
                 workspace, or set NOETA_AOT_RUNTIME_LIB to a prebuilt archive"
                    .to_string(),
            );
        }
    }
}

/// Link the AOT `object` against the runtime `archive` (+ its native `libs`) into `out` with a C
/// toolchain. Everything — the program's native bodies, the runtime, and Rust std — lives in the one
/// archive, so a single archive mention resolves the object↔runtime mutual references (the object
/// defines `noeta_aot_dispatch`, the archive defines `main` + the `noeta_jit_*` helpers). `cc` adds
/// the C runtime that calls `main`. The linker (`cc`) is overridable via `NOETA_CC`.
#[cfg(feature = "jit")]
pub(crate) fn link_native(
    object: &std::path::Path,
    archive: &std::path::Path,
    libs: &[String],
    out: &std::path::Path,
) -> Result<(), String> {
    let cc = std::env::var("NOETA_CC").unwrap_or_else(|_| "cc".to_string());
    let mut cmd = std::process::Command::new(&cc);
    // `-s` strips the symbol table + DWARF during the link (~5 MB on a core binary — nearly half).
    // A shipped `--native` artifact never needs native debug symbols: its panic tracebacks come from
    // the bundle's own line table (`<aot>` source, production-stack-traces arc), not DWARF — the same
    // reason `profile.wasm-release` sets `strip = true`. Stripping HERE, at the link, is deliberate:
    // the caller staples the bundle onto this binary *after* we return, and stripping a stapled
    // executable would rewrite the ELF and discard the appended bundle ("no stapled bundle found").
    cmd.arg("-s");
    cmd.arg(object).arg(archive).args(libs).arg("-o").arg(out);
    let output = cmd
        .output()
        .map_err(|e| format!("cannot run the linker `{cc}` (override with NOETA_CC): {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "`{cc}` exited with {}:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    use noeta_lexer::lex;
    use noeta_parser::parse;
    use noeta_span::{Source, SourceId};

    /// A one-proto module whose const pool + code carry the given entries, with `names` as the op
    /// name table — enough to exercise the ring scan.
    #[cfg(feature = "jit")]
    fn module_with(
        consts: Vec<noeta_bytecode::Const>,
        code: Vec<noeta_bytecode::Op>,
        names: Vec<String>,
    ) -> noeta_bytecode::Module {
        let mut chunk = noeta_bytecode::Chunk::placeholder();
        chunk.consts = consts;
        chunk.code = code;
        noeta_bytecode::Module {
            protos: vec![chunk],
            shapes: Vec::new(),
            packed_schemas: Vec::new(),
            map_packed_sites: Vec::new(),
            methods: Vec::new(),
            destructors: Vec::new(),
            field_defaults: Vec::new(),
            comparable_derives: Vec::new(),
            tojson_derives: Vec::new(),
            deserialize_recipes: Vec::new(),
            type_args: Vec::new(),
            destruct_reachable: Vec::new(),
            cache_slots: 0,
            reflection: Default::default(),
            type_reprs: Vec::new(),
            names,
            global_names: Vec::new(),
        }
    }

    #[cfg(feature = "jit")]
    #[test]
    fn aot_ring_features_selects_http_client_but_not_server() {
        use noeta_bytecode::Const;
        let client = vec!["ring-http-client".to_string()];

        // `use std.http.client` → a whole-module value. Post-split (P0.3b) this is *precise*: the
        // client module owns reqwest, so the whole-module reference selects the client ring. Module
        // identities are root-qualified (`std.http.client`), as the compiler now emits them.
        let via_module = module_with(
            vec![Const::NativeModule("std.http.client".into())],
            vec![],
            vec![],
        );
        assert_eq!(aot_ring_features(&via_module), client);

        // `use std.http.client.get` → a selective import of a client function → client ring.
        let client_fn = module_with(
            vec![Const::ModuleFn {
                module: "std.http.client".into(),
                func: "get".into(),
            }],
            vec![],
            vec![],
        );
        assert_eq!(aot_ring_features(&client_fn), client);

        // The split payoff: a `use std.http.server` program — whole module *or* its `response`/`serve`
        // functions — selects NO client ring, so reqwest is shed from the archive.
        let server_module = module_with(
            vec![Const::NativeModule("std.http.server".into())],
            vec![],
            vec![],
        );
        assert!(aot_ring_features(&server_module).is_empty());
        let server_fn = module_with(
            vec![Const::ModuleFn {
                module: "std.http.server".into(),
                func: "response".into(),
            }],
            vec![],
            vec![],
        );
        assert!(aot_ring_features(&server_fn).is_empty());

        // A program that only touches an always-on core / not-yet-gated module selects no ring.
        let non_http = module_with(vec![Const::NativeModule("std.math".into())], vec![], vec![]);
        assert!(aot_ring_features(&non_http).is_empty());
    }

    #[test]
    fn aot_ring_features_group_form_selects_http_client() {
        // The navigable namespace group (`use std.http; http.client.get(...)`, module-namespaces)
        // must lower to the *same* concrete leaf identity `std.http.client` as the direct import, so
        // AOT ring DCE keeps reqwest. If the group ever recorded the prefix `std.http` in the const
        // pool, `ring_of` would miss and `--native` would ship a binary with no HTTP client. This
        // compiles the group form end-to-end and asserts the client ring is selected.
        let src = "use std.http\nr = http.client.get(\"https://svc.test/echo\")\necho r.status()\n";
        let source = Source::new(SourceId(0), "<test>", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        assert!(
            parsed.diagnostics.is_empty(),
            "parse errors: {:?}",
            parsed.diagnostics
        );
        let checked = noeta_check::check_all(&parsed.program);
        assert!(
            checked.diagnostics.is_empty(),
            "check errors: {:?}",
            checked.diagnostics
        );
        let module = noeta_compiler::compile(&parsed.program).expect("group form compiles");
        assert_eq!(
            aot_ring_features(&module),
            vec!["ring-http-client".to_string()],
            "group-form http.client.get must select the http-client ring (concrete leaf identity)"
        );
    }
}
