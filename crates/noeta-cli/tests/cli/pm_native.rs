//! Package manager phase 3: third-party NATIVE packages and the composed toolchain
//! (N3.2/N3.3).

use crate::support::*;

// --- package manager: composed toolchain (Phase 3, N3.2/N3.3) -----------------------------------

/// Lay out an app + a dependency package carrying a **native entry crate** (the Phase-3 proving
/// package): module `fx` (plain dispatch), extern type `Acc` (plain methods + a higher-order ctx
/// method), and an `fx-info` ExtCommand. The crate depends on this workspace's `noeta-ext-abi` by
/// path and exports the composition convention symbol `NOETA_EXTENSIONS`.
fn composed_project(name: &str) -> PathBuf {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("app");
    let dep = base.join("imgfx");
    let krate = dep.join("native");
    std::fs::create_dir_all(&app).expect("mk app");
    std::fs::create_dir_all(krate.join("src")).expect("mk crate");

    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\nimgfx = { path = \"../imgfx\" }\n\
         [trust]\nnative = [\"acme/imgfx\"]\ncommands = [\"acme/imgfx\"]\n",
    )
    .unwrap();
    std::fs::write(
        app.join("main.noe"),
        "use imgfx.{fx}\n\n\
         @packed(Layout.Column) struct Px { r: f32; g: f32; b: f32 }\n\n\
         a = fx.acc();\n\
         a.add(2);\n\
         a.apply(fn(t) => t * 10);\n\
         echo fx.double(21);\n\
         echo a.total();\n\n\
         // The raw-buffer seam, third-party edition (N3.4): the extension's column kernel\n\
         // reduces the app's own @packed type, and its COW-mutating kernel produces a new list.\n\
         impl fx.Pixels for Px {}\n\
         ps = [Px { r: 0.25f32, g: 1.0f32, b: 2.0f32 }, Px { r: 0.5f32, g: 1.0f32, b: 2.0f32 }];\n\
         echo fx.sum_r(ps);\n\
         bright = fx.brighten_all(ps, 0.5f32);\n\
         echo bright[1].g;\n\
         echo ps[1].g;\n\
         echo ps.brighten(2.0f32)[0].r;\n",
    )
    .unwrap();
    std::fs::write(
        app.join("bad.noe"),
        "use imgfx.{fx}\n\necho fx.double(\"nope\");\n",
    )
    .unwrap();

    // The code-GENERATING directive and the spec it generates from (see `expand_fx_spec`), for
    // `noeta expand`. Its own entry rather than a decoration on `main.noe`, so every other test's
    // expectations of the app's output are untouched; it declares no namespace, so it is nobody's
    // imported module either.
    std::fs::write(app.join("pets.yaml"), "list_pets\nget_pet\n").unwrap();
    std::fs::write(
        app.join("spec.noe"),
        "use imgfx.{fx}\n\n@fx_spec(\"pets.yaml\")\nstruct PetStore { base_url: string }\n\
         echo PetStore.list_pets();\n",
    )
    .unwrap();

    std::fs::write(
        dep.join("noeta.toml"),
        "[package]\nname = \"acme/imgfx\"\nversion = \"1.0.0\"\nnative = \"native\"\n",
    )
    .unwrap();

    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf();
    std::fs::write(
        krate.join("Cargo.toml"),
        format!(
            "[package]\nname = \"imgfx-native\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
             [lib]\npath = \"src/lib.rs\"\n\n\
             [dependencies]\nnoeta-ext-abi = {{ path = \"{}\" }}\n\n\
             # dev-deps D5: the mixed package gates its dev formatter behind `fmt` (a real crate would\n\
             # also put `malva`/etc. behind it as `dep:`). Off by default — a shipped runner never\n\
             # enables it; the dev toolchain does.\n\
             [features]\nfmt = []\n\n[workspace]\n",
            workspace.join("crates").join("noeta-ext-abi").display()
        ),
    )
    .unwrap();
    std::fs::write(krate.join("src").join("lib.rs"), IMGFX_NATIVE_SRC).unwrap();
    app.join("main.noe")
}

/// The proving extension's Rust source (see [`composed_project`]).
const IMGFX_NATIVE_SRC: &str = r##"
//! The Phase-3 proving extension: one module, one extern type with plain + ctx methods, one
//! CLI command — exercised end-to-end through toolchain composition.

use std::any::Any;
use std::cmp::Ordering;
use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering as AtomicOrd};

use noeta_ext_abi::registry::{
    BundleFn, BundleReceiver, ConstraintField, ConstraintLayout, DirectiveCtx, ExtBundle,
    ExtDirective, ExtFn, ExtModule, ExtType, Expansion, Extension, NativeOut, NativeValue,
    PackedConstraint, RetTy, Scalar, SigType, TierSite,
};
use noeta_ext_abi::{
    no_function_error, no_method_error, CommandCtx, CtxError, CtxOut, ErrorKind, ExtCommand,
    ExternValue, Host, NativeCtx, ParsedArgs, Slot, StdError,
};

const FX_FNS: &[ExtFn] = &[
    ExtFn {
        name: "double",
        params: &[SigType::Int],
        ret: RetTy::Concrete(SigType::Int),
    },
    ExtFn {
        name: "acc",
        params: &[],
        ret: RetTy::Concrete(SigType::Named("Acc")),
    },
];

fn fx_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "double" => match args.first() {
            Some(NativeValue::Scalar(Scalar::Int(n))) => Ok(NativeOut::Scalar(Scalar::Int(n * 2))),
            _ => Err(StdError {
                kind: ErrorKind::ArgType,
                message: "`fx.double` expects an int".to_string(),
            }),
        },
        "acc" => Ok(NativeOut::Extern(noeta_ext_abi::ExternBox(Box::new(
            Acc::default(),
        )))),
        _ => Err(no_function_error("fx", func)),
    }
}

// The raw-buffer seam (package-manager N3.4), third-party edition: kernels over the CONSUMER's
// own `@packed` pixel type — a column reduction (zero per-element traffic) and a COW-mutating
// transform producing a new list.
const FX_CTX_FNS: &[ExtFn] = &[
    ExtFn {
        name: "sum_r",
        params: &[SigType::Dyn],
        ret: RetTy::Concrete(SigType::F32),
    },
    ExtFn {
        name: "brighten_all",
        params: &[SigType::Dyn, SigType::F32],
        ret: RetTy::SameAsArg(0),
    },
];

fn packed_error(func: &str) -> CtxError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!("`fx.{func}` expects a packed pixel list"),
    }
    .into()
}

fn fx_ctx_dispatch(
    func: &str,
    ctx: &mut dyn NativeCtx,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match func {
        // Sum the first (`r`) component across the buffer, layout-aware through the neutral
        // view: a column list's `r`s are one contiguous run; a row list strides.
        "sum_r" => {
            let mut sum: Option<f32> = None;
            ctx.with_packed(args[0], &mut |v, bytes| {
                if v.fields.len() == 3 {
                    let (run, step) = if v.column {
                        (&bytes[..v.count * 4], 4)
                    } else {
                        (bytes, v.byte_size)
                    };
                    sum = Some(
                        run.chunks(step)
                            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                            .sum(),
                    );
                }
            })?;
            match sum {
                Some(s) => Ok(CtxOut::Out(NativeOut::Scalar(Scalar::F32(s)))),
                None => Err(packed_error(func)),
            }
        }
        // Add `delta` to every component — value semantics through the copy-on-write mutable
        // borrow; the transformed list arrives as a fresh slot, the input stays intact.
        "brighten_all" => {
            let NativeValue::Scalar(Scalar::F32(delta)) = ctx.view(args[1])? else {
                return Err(StdError {
                    kind: ErrorKind::ArgType,
                    message: "`fx.brighten_all` expects an f32 delta".to_string(),
                }
                .into());
            };
            match ctx.with_packed_mut(args[0], &mut |_, bytes| {
                for c in bytes.chunks_exact_mut(4) {
                    let v = f32::from_le_bytes([c[0], c[1], c[2], c[3]]) + delta;
                    c.copy_from_slice(&v.to_le_bytes());
                }
            })? {
                Some(result) => Ok(CtxOut::Slot(result)),
                None => Err(packed_error(func)),
            }
        }
        _ => Err(no_function_error("fx", func).into()),
    }
}

#[derive(Debug, Default)]
struct Acc {
    total: AtomicI64,
}

impl ExternValue for Acc {
    fn type_identity(&self) -> &'static str {
        "imgfx.fx.Acc"
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other
            .as_any()
            .downcast_ref::<Acc>()
            .is_some_and(|o| o.total.load(AtomicOrd::Relaxed) == self.total.load(AtomicOrd::Relaxed))
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0
    }
    fn display(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(out, "<acc {}>", self.total.load(AtomicOrd::Relaxed))
    }
    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(Acc {
            total: AtomicI64::new(self.total.load(AtomicOrd::Relaxed)),
        })
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

const ACC_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "add",
        params: &[SigType::Int],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "total",
        params: &[],
        ret: RetTy::Concrete(SigType::Int),
    },
];

fn acc_method_dispatch(
    recv: &mut dyn ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let acc = recv
        .as_any_mut()
        .downcast_mut::<Acc>()
        .expect("receiver is an Acc");
    match method {
        "add" => match args.first() {
            Some(NativeValue::Scalar(Scalar::Int(n))) => {
                acc.total.fetch_add(*n, AtomicOrd::Relaxed);
                Ok(NativeOut::Unit)
            }
            _ => Err(StdError {
                kind: ErrorKind::ArgType,
                message: "`Acc.add` expects an int".to_string(),
            }),
        },
        "total" => Ok(NativeOut::Scalar(Scalar::Int(
            acc.total.load(AtomicOrd::Relaxed),
        ))),
        _ => Err(no_method_error("Acc", method)),
    }
}

const ACC_CTX_METHODS: &[ExtFn] = &[ExtFn {
    name: "apply",
    params: &[SigType::Fn(&[SigType::Int], &SigType::Int)],
    ret: RetTy::Concrete(SigType::Unit),
}];

/// `acc.apply(f)` — replace the total with `f(total)`: the higher-order ctx seam, third-party
/// edition (closure call-back through `NativeCtx`).
fn acc_ctx_dispatch(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match method {
        "apply" => {
            let mut total = 0;
            ctx.with_extern(recv, &mut |e| {
                if let Some(acc) = e.as_any().downcast_ref::<Acc>() {
                    total = acc.total.load(AtomicOrd::Relaxed);
                }
            })?;
            let arg = ctx.intern(NativeOut::Scalar(Scalar::Int(total)))?;
            let out = ctx.call(args[0], &[arg])?;
            let NativeValue::Scalar(Scalar::Int(new_total)) = ctx.view(out)? else {
                return Err(StdError {
                    kind: ErrorKind::ArgType,
                    message: "`Acc.apply` closure must return an int".to_string(),
                }
                .into());
            };
            ctx.free(arg);
            ctx.free(out);
            ctx.with_extern(recv, &mut |e| {
                if let Some(acc) = e.as_any().downcast_ref::<Acc>() {
                    acc.total.store(new_total, AtomicOrd::Relaxed);
                }
            })?;
            Ok(CtxOut::Out(NativeOut::Unit))
        }
        _ => Err(no_method_error("Acc", method).into()),
    }
}

const FX_INFO: ExtCommand = ExtCommand {
    name: "fx-info",
    about: "Prove an extension-contributed command dispatches through composition",
    args: &[],
    run: fx_info_run,
};

fn fx_info_run(_ctx: &mut dyn CommandCtx, _args: &ParsedArgs) -> u8 {
    println!("imgfx: native extension ok");
    0
}

// A third-party METHOD BUNDLE (kernel-methods K6): the consumer's own @packed pixel type opts in
// with `impl fx.Pixels for Px {}` and gains `ps.brighten(delta)` — same COW raw-buffer kernel as
// `fx.brighten_all`, in method position, statically routed through the composed toolchain.
const PIXELS_BUNDLE: ExtBundle = ExtBundle {
    name: "Pixels",
    constraint: PackedConstraint {
        fields: &[
            ConstraintField::F32,
            ConstraintField::F32,
            ConstraintField::F32,
        ],
        layout: ConstraintLayout::Any,
    },
    methods: &[BundleFn {
        sig: ExtFn {
            name: "brighten",
            params: &[SigType::F32],
            ret: RetTy::SameAsArg(0),
        },
        receiver: BundleReceiver::Bulk,
    }],
    ctx_dispatch: pixels_bundle_dispatch,
};

fn pixels_bundle_dispatch(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: noeta_ext_abi::Slot,
    args: &[noeta_ext_abi::Slot],
) -> Result<CtxOut, CtxError> {
    match method {
        // `ps.brighten(delta)` ≡ `fx.brighten_all(ps, delta)` — one kernel, two surfaces.
        "brighten" => {
            let mut all = Vec::with_capacity(args.len() + 1);
            all.push(recv);
            all.extend_from_slice(args);
            fx_ctx_dispatch("brighten_all", ctx, &all)
        }
        _ => Err(no_method_error("fx.Pixels", method).into()),
    }
}

/// A code-GENERATING directive (`ExtDirective::expand`): one accessor per name listed in the spec
/// file the invocation points at. Deliberately reads a real file relative to `ctx.source_dir` and
/// reports it in `reads` — that is the shape a spec-driven generator has, and it is what makes
/// `noeta expand`'s output a function of the spec rather than of the directive's one line.
fn expand_fx_spec(ctx: &DirectiveCtx) -> Result<Expansion, String> {
    let arg = ctx.args.first().ok_or("no spec given")?;
    let path = std::path::Path::new(&ctx.source_dir).join(arg);
    let spec = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut source = String::new();
    for name in spec.lines().map(str::trim).filter(|l| !l.is_empty()) {
        source.push_str(&format!("fn {name}(): int {{ return fx.double(21); }}\n"));
    }
    Ok(Expansion {
        source,
        reads: vec![path.display().to_string()],
    })
}

#[derive(Debug, Clone, Copy)]
struct ImgfxExtension;

impl Extension for ImgfxExtension {
    fn name(&self) -> &'static str {
        "imgfx"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[ExtModule {
            name: "fx",
            functions: FX_FNS,
            dispatch: fx_dispatch,
            ctx_functions: FX_CTX_FNS,
            ctx_dispatch: Some(fx_ctx_dispatch),
            bundles: &[PIXELS_BUNDLE],
            ..ExtModule::DEFAULTS
        }]
    }
    fn types(&self) -> &'static [ExtType] {
        &[ExtType {
            name: "Acc",
            namespace: "imgfx.fx",
            methods: ACC_METHODS,
            dispatch: acc_method_dispatch,
            ctx_methods: ACC_CTX_METHODS,
            ctx_dispatch: Some(acc_ctx_dispatch),
            ..ExtType::DEFAULTS
        }]
    }
    fn commands(&self) -> &'static [ExtCommand] {
        &[FX_INFO]
    }
    fn directives(&self) -> &'static [ExtDirective] {
        &[ExtDirective {
            name: "fx_spec",
            sites: &[TierSite::Type],
            max_args: Some(1),
            named_keys: &[],
            detail: "@fx_spec(\"<file>\")",
            doc: "Generate one accessor per name in the given spec file.",
            params: &["spec"],
            expand: Some(expand_fx_spec),
        }]
    }
    // dev-deps D5: a DEV-only capability — a tier-body formatter — gated behind the `fmt` feature.
    // The runtime capabilities above (module/type/command) always compile; this one, and the marker
    // string it carries, only when `fmt` is enabled. A shipped composed runner is built with default
    // features (fmt OFF), so the formatter and marker are absent from the artifact; the dev toolchain
    // would enable `fmt` to reflow this extension's tier bodies under `noeta fmt`.
    #[cfg(feature = "fmt")]
    fn body_formatters(&self) -> &'static [noeta_ext_abi::registry::BodyFormatter] {
        &[("imgfx", imgfx_reformat)]
    }
}

/// The gated dev formatter (see `body_formatters`). Its distinctive marker proves compilation: it is
/// in the binary iff the `fmt` feature was on.
#[cfg(feature = "fmt")]
fn imgfx_reformat(
    body: &str,
    _indent: &str,
    _sub: &noeta_ext_abi::registry::SubFormat,
) -> Option<String> {
    const MARKER: &str = "IMGFX_FMT_ONLY_MARKER_7c4e9a";
    Some(format!("{MARKER}:{}", body.trim()))
}

/// The composition convention (package-manager Phase 3): the entry crate exports its units as a
/// slice — one crate, any number of units.
pub static NOETA_EXTENSIONS: &[&(dyn Extension + Sync)] = &[&ImgfxExtension];
"##;

/// Point the compose build at the workspace's existing debug artifacts (the shim links the
/// already-built noeta-cli lib in seconds instead of a cold release build).
/// Serializes the composition-heavy e2e tests. Each shells out to `cargo` into the **shared**
/// workspace target dir (`composed_env`'s `NOETA_COMPOSE_TARGET_DIR`, set for speed so the composes
/// reuse the workspace's already-built debug deps), where every shim crate is named `noeta-composed`.
/// Running two at once lets cargo's concurrent manifest resolution trip over that shared artifact
/// (`can't find bin … src/main.rs`). Production never points two composes at one target dir, so this
/// is purely a test-harness concern — the guard runs these few tests one at a time. Poison-tolerant:
/// a panicking compose test must not wedge the others.
static COMPOSE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn compose_guard() -> std::sync::MutexGuard<'static, ()> {
    COMPOSE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn composed_env(cmd: &mut Command) -> &mut Command {
    cmd.env("NOETA_COMPOSE_DEBUG", "1").env(
        "NOETA_COMPOSE_TARGET_DIR",
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).parent().unwrap(),
    )
}

#[test]
fn build_exe_of_a_native_dep_app_strips_the_mixed_crates_formatter() {
    let _guard = compose_guard();
    // dev-deps D5, the capstone: a shipped native-dependency app carries its runtime handler but not
    // the mixed crate's dev formatter. `build --exe` composes a RUNNER (lean base + imgfx runtime
    // extension, `fmt` OFF) and staples the bundle. We prove both halves:
    //   1. the artifact RUNS the native handler (`fx.double(21)` → 42) — the extension is composed in;
    //   2. the gated formatter is STRIPPED — its distinctive marker is absent from the binary.
    let entry = composed_project("d5_exe_strip");
    let app_bin = entry.parent().unwrap().join("app_native_exe");
    let _ = std::fs::remove_file(&app_bin);

    // The runner composition needs the lean runner binary as its base? No — the *composed* runner IS
    // the base (built from the shim). `composed_env` reuses the workspace's debug artifacts so this
    // stays a fast debug composition rather than a cold release build.
    composed_env(&mut lang())
        .arg("build")
        .arg(&entry)
        .arg("--exe")
        .arg("-o")
        .arg(&app_bin)
        .assert()
        .success()
        .stderr(predicate::str::contains("self-contained"));

    // 1. Runs the native handler — success alone proves it (an unknown `imgfx` module would abort);
    //    the first echoed line is `fx.double(21)` = 42.
    Command::new(&app_bin)
        .assert()
        .success()
        .stdout(predicate::str::starts_with("42\n"));

    // 2. The dev formatter is absent from the shipped artifact.
    let bytes = std::fs::read(&app_bin).expect("read the artifact");
    let marker = b"IMGFX_FMT_ONLY_MARKER_7c4e9a";
    assert!(
        !bytes.windows(marker.len()).any(|w| w == marker),
        "the composed runner leaked the mixed crate's dev formatter into the shipped artifact — \
         the `fmt` feature was not stripped"
    );
    let _ = std::fs::remove_file(&app_bin);
}

/// Find the composed binary a delegation cached under an (isolated) compose cache dir —
/// `<cache>/compose/<key>/bin/noeta-composed`. Exactly one exists per distinct composition.
fn find_composed_binary(cache: &std::path::Path) -> Option<PathBuf> {
    let compose = cache.join("compose");
    for key in std::fs::read_dir(&compose).ok()? {
        let bin = key.ok()?.path().join("bin").join("noeta-composed");
        if bin.is_file() {
            return Some(bin);
        }
    }
    None
}

#[test]
fn dev_toolchain_composition_includes_a_mixed_crates_formatter() {
    let _guard = compose_guard();
    // dev-deps D5b, the mirror of the capstone: a *dev toolchain* composed for the same native-dep app
    // turns the mixed crate's `fmt` feature ON, so its tier-body formatter (and its marker) compile IN
    // — exactly the capability a shipped runner strips. We compose the toolchain via a delegating dev
    // command (`check`) and confirm the cached composed binary carries the formatter marker.
    let entry = composed_project("d5b_toolchain_fmt");
    // Isolate the compose cache so we can locate *this* composition's binary (and not touch the user's).
    let cache = entry.parent().unwrap().join("cache");
    let _ = std::fs::remove_dir_all(&cache);

    composed_env(&mut lang())
        .arg("check")
        .arg(&entry)
        .env("NOETA_CACHE_DIR", &cache)
        .assert()
        .success();

    let composed = find_composed_binary(&cache).expect("a composed toolchain binary was cached");
    let bytes = std::fs::read(&composed).expect("read the composed toolchain");
    let marker = b"IMGFX_FMT_ONLY_MARKER_7c4e9a";
    assert!(
        bytes.windows(marker.len()).any(|w| w == marker),
        "the dev toolchain composition did not enable the mixed crate's `fmt` feature — its \
         formatter marker is absent from {}",
        composed.display()
    );
}

#[test]
fn doc_api_in_a_composed_toolchain_documents_the_native_package() {
    // Publish-time native-package docs (the mechanism `noeta publish` uses): a native package's
    // module surface exists only in its compiled Rust, so its API docs are generated by running
    // `noeta doc --api --root <pkg>` INSIDE a composed toolchain that links the package's extension
    // — a client-side build (here the composed toolchain), never anything on the registry. This
    // proves the composed toolchain emits the package's own surface, scoped away from std.
    let entry = composed_project("docs_api_native");
    let cache = entry.parent().unwrap().join("cache");
    let _ = std::fs::remove_dir_all(&cache);

    // A delegating dev command composes + caches the toolchain (the imgfx extension linked).
    composed_env(&mut lang())
        .arg("check")
        .arg(&entry)
        .env("NOETA_CACHE_DIR", &cache)
        .assert()
        .success();
    let composed = find_composed_binary(&cache).expect("a composed toolchain binary was cached");

    // Generate the package's own API docs in the composed toolchain.
    let out = std::process::Command::new(&composed)
        .args(["doc", "--api", "--root", "imgfx"])
        .output()
        .expect("run `doc --api` in the composed toolchain");
    assert!(
        out.status.success(),
        "doc --api failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = String::from_utf8(out.stdout).expect("docs.json is UTF-8");
    // The package's own module and its plain + higher-order functions are documented…
    assert!(
        json.contains("\"imgfx.fx\""),
        "fx module documented:\n{json}"
    );
    for f in ["\"double\"", "\"acc\"", "\"sum_r\"", "\"brighten_all\""] {
        assert!(json.contains(f), "fx function {f} documented");
    }
    // …and std is excluded by the root scope (a package documents only itself).
    assert!(
        !json.contains("\"std.math\""),
        "--root imgfx must exclude the stdlib"
    );

    // The publish namespace lint (`--lint`) passes on the well-namespaced fixture. The negative
    // case — a type omitting `namespace:` and so defaulting to `std` — no longer *reaches* the
    // lint: registry assembly refuses the unit at startup (`validate()`, unit-tested in
    // noeta-ext-abi as `a_type_namespace_outside_the_units_root_is_rejected`), so a sloppy package
    // fails at its very first composed run with the type named, not at publish time.
    let lint = std::process::Command::new(&composed)
        .args(["doc", "--api", "--root", "imgfx", "--lint"])
        .output()
        .expect("run the publish namespace lint");
    assert!(
        lint.status.success(),
        "the lint passes a package whose types are namespaced under its own root: {}",
        String::from_utf8_lossy(&lint.stderr)
    );
}

#[cfg(feature = "jit")] // `--native` exists only in the JIT-enabled build.
#[test]
fn build_native_of_a_native_dep_app_runs_the_composed_handler() {
    let _guard = compose_guard();
    // dev-deps `--native` gap, closed: a native-dependency app built with `--native` links a *composed
    // AOT runtime* (the lean runtime + the imgfx native extension) so the self-contained native binary
    // resolves the `imgfx` module and runs its handler. Before this, `--native` linked the stock
    // `libnoeta_aot.a` (no extension seam) and aborted on the unknown native module.
    if !has_cc() {
        eprintln!("skipping native-dep AOT test: no `cc` on PATH");
        return;
    }
    let entry = composed_project("native_dep_aot");
    let app_bin = entry.parent().unwrap().join("app_native_aot");
    let _ = std::fs::remove_file(&app_bin);

    // The composed toolchain (the delegation target) builds the composed AOT staticlib and `cc`-links
    // it against the program's AOT object. `composed_env` reuses the workspace debug artifacts so both
    // compositions stay fast; the env is inherited across the `exec` delegation.
    composed_env(&mut lang())
        .arg("build")
        .arg(&entry)
        .arg("--native")
        .arg("-o")
        .arg(&app_bin)
        .assert()
        .success()
        .stderr(predicate::str::contains("native AOT"));

    // The native binary runs on its own and resolves the native handler (`fx.double(21)` → 42).
    Command::new(&app_bin)
        .assert()
        .success()
        .stdout(predicate::str::starts_with("42\n"));

    // And it is lean: the composed AOT runtime pulls the mixed crate at default features, so its dev
    // formatter (and marker) are stripped from the shipped native artifact — same guarantee as `--exe`.
    let bytes = std::fs::read(&app_bin).expect("read the native artifact");
    let marker = b"IMGFX_FMT_ONLY_MARKER_7c4e9a";
    assert!(
        !bytes.windows(marker.len()).any(|w| w == marker),
        "the composed AOT runtime leaked the mixed crate's dev formatter into the native artifact"
    );
    let _ = std::fs::remove_file(&app_bin);
}

#[test]
fn composed_toolchain_end_to_end() {
    let _guard = compose_guard();
    let entry = composed_project("pm_compose_e2e");
    let app = entry.parent().unwrap().to_path_buf();

    // Step 1 asserts a compose-cache MISS, but the shared test cache dir outlives test
    // invocations — once the binary and fixture are both stable, a second `cargo test` would hit
    // the previous run's entry and see no banner. Clear the compose cache (only) for idempotence;
    // the step-2 hit is then proven within this run.
    let _ = std::fs::remove_dir_all(
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("noeta-cache/compose"),
    );

    // 1. First run: composes (banner on stderr), then dispatches the native module, the extern
    //    type's plain methods, the higher-order ctx method, and the raw-buffer kernels (N3.4:
    //    `sum_r` reduces the app's own @packed column type; `brighten_all` produces a new list
    //    while — copy-on-write — the input stays intact: 1.5 then 1.0) — all composed.
    composed_env(&mut lang())
        .arg("run")
        .arg(&entry)
        .assert()
        .success()
        .stdout(
            predicate::str::contains("42")
                .and(predicate::str::contains("20"))
                .and(predicate::str::contains("0.75"))
                .and(predicate::str::contains("1.5\n1.0"))
                .and(predicate::str::contains("2.25")),
        )
        .stderr(predicate::str::contains("composing the toolchain"));

    // 2. Second run: content-addressed cache hit — no compose banner, same output.
    composed_env(&mut lang())
        .arg("run")
        .arg(&entry)
        .assert()
        .success()
        .stdout(predicate::str::contains("42"))
        .stderr(predicate::str::contains("composing the toolchain").not());

    // 3. `noeta check` sees the extension's signatures: a wrong-typed argument to the native fn
    //    is a *static* error (the composed binary IS the checker), and the good file checks clean.
    composed_env(&mut lang())
        .arg("check")
        .arg(app.join("bad.noe"))
        .assert()
        .failure();
    composed_env(&mut lang())
        .arg("check")
        .arg(&entry)
        .assert()
        .success();

    // 4. An extension-contributed command is an unknown subcommand to the stock binary; the
    //    cwd-manifest fallback composes (cache hit) and the composed binary dispatches it.
    composed_env(&mut lang())
        .arg("fx-info")
        .current_dir(&app)
        .assert()
        .success()
        .stdout(predicate::str::contains("imgfx: native extension ok"));

    // 5. `noeta expand` prints the source the extension's `expand` hook generated. This is the only
    //    place the command can be proven end to end: no SHIPPED extension declares an `expand` hook,
    //    so the stock binary has no expanding directive to reach at all — the composed toolchain
    //    linking `imgfx` does.
    let expanded = composed_env(&mut lang())
        .arg("expand")
        .arg(app.join("spec.noe"))
        .assert()
        .success()
        // The header names the CAUSE — the declaration that grew the members and the directive
        // that grew them, with its arguments — and the body is the whole synthetic declaration…
        .stdout(
            predicate::str::contains(r#"// PetStore ⟨@fx_spec "pets.yaml"⟩"#)
                .and(predicate::str::contains("struct PetStore {"))
                // …one accessor per line of the spec, so the printed source is a function of the
                // spec file: this is the CI diff that makes a spec change reviewable.
                .and(predicate::str::contains("fn list_pets(): int"))
                .and(predicate::str::contains("fn get_pet(): int")),
        )
        .stderr(predicate::str::contains("expanded 1 declaration"))
        .get_output()
        .stdout
        .clone();
    // The hand-written field is NOT printed: what prints is the expansion, not the declaration it
    // was spliced into.
    assert!(
        !String::from_utf8_lossy(&expanded).contains("base_url"),
        "expand printed the hand-written members too: {}",
        String::from_utf8_lossy(&expanded)
    );

    // 6. A generated member is real code: running the same file calls one and gets the native
    //    handler's answer. `expand` showed the source; this proves that source is what runs.
    composed_env(&mut lang())
        .arg("run")
        .arg(app.join("spec.noe"))
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));
}

#[test]
#[cfg(unix)]
fn an_unknown_subcommand_falls_back_to_a_noeta_prefixed_binary_on_path() {
    use std::os::unix::fs::PermissionsExt;
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pm_external_cmd");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let tool = dir.join("noeta-hello");
    std::fs::write(
        &tool,
        "#!/bin/sh\necho \"hello from external: $1\"\nexit 7\n",
    )
    .unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

    // PATH includes only our dir — the fallback finds `noeta-hello`, forwards trailing args, and
    // the exit code passes through.
    lang()
        .arg("hello")
        .arg("world")
        .env("PATH", &dir)
        .assert()
        .code(7)
        .stdout(predicate::str::contains("hello from external: world"));

    // Without the binary on PATH the ordinary clap error renders (exit 2, mentions the name).
    lang()
        .arg("hello")
        .env("PATH", env!("CARGO_TARGET_TMPDIR"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("hello"));
}
