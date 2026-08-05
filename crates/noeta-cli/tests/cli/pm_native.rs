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
         [directives]\nfx_spec = \"imgfx\"\nfx_shape = \"imgfx\"\n\
         [trust]\nnative = [\"acme/imgfx\"]\n\
         [trust.commands]\nfx-info = \"acme/imgfx\"\n",
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

    // The SHAPE-driven generator (see `expand_fx_shape`): it takes no arguments and reads no file,
    // so everything it emits is derived from the decorated declaration's own fields. Its own entry
    // for the same reason `spec.noe` is one. `tags` is generic on purpose — an erased `List` would
    // generate an accessor with the wrong return type, and only a full-fidelity spelling catches it.
    std::fs::write(
        app.join("shape.noe"),
        "@fx_shape\nstruct Order { id: int; tags: List<string> }\n\
         echo Order.tags_type();\n",
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

/// [`composed_project`]'s twin, with the native crate depending on `noeta-ext-abi` **by version**
/// rather than by path — the form a package takes once the contract crates are published to
/// crates.io (`noeta-ext-abi = "0.5"`).
///
/// This is the shape that proves `[patch.crates-io]` works. Without that table the version
/// requirement resolves the *real* crates.io crate, which is a **second** copy of `noeta-ext-abi`:
/// the package's `dyn Extension` then fails to match the shim's `noeta_ext_abi::Extension` and the
/// `NOETA_EXTENSIONS` aggregation does not type-check. Exactly the failure the git patch has always
/// prevented, reached through the other door.
fn composed_project_versioned(name: &str) -> PathBuf {
    let entry = composed_project(name);
    // <base>/app/main.noe -> <base>/imgfx/native
    let krate = entry
        .parent()
        .and_then(std::path::Path::parent)
        .expect("fixture base")
        .join("imgfx")
        .join("native");
    let manifest = krate.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).expect("fixture manifest");
    // Swap the path dependency for the published-style version requirement.
    let start = text.find("noeta-ext-abi = {").expect("path dep present");
    let end = text[start..].find('\n').expect("line end") + start;
    let swapped = format!(
        "{}noeta-ext-abi = \"{}\"{}",
        &text[..start],
        env!("CARGO_PKG_VERSION"),
        &text[end..]
    );
    std::fs::write(&manifest, swapped).unwrap();
    entry
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
    BundleReceiver, ConstraintArity, ConstraintField, ConstraintLayout, DirectiveCtx,
    ExtDirective, ExtFn, ExtModule, ExtTrait, ExtTraitMethod, ExtType, Expansion, ExpansionError,
    Extension, NativeOut, NativeValue,
    PackedConstraint, RetTy, Scalar, SigType, TierSite,
};
use noeta_ext_abi::{
    no_function_error, no_method_error, CommandCtx, CtxError, CtxOut, ErrorKind, ExtCommand,
    ExternValue, Host, NativeCtx, ParsedArgs, Slot, StdError,
};

const FX_FNS: &[ExtFn] = &[
    ExtFn {
param_names: &[],
        name: "double",
        params: &[SigType::Int],
        ret: RetTy::Concrete(SigType::Int),
    },
    ExtFn {
param_names: &[],
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
param_names: &[],
        name: "sum_r",
        params: &[SigType::Dyn],
        ret: RetTy::Concrete(SigType::F32),
    },
    ExtFn {
param_names: &[],
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
param_names: &[],
        name: "add",
        params: &[SigType::Int],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
param_names: &[],
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
param_names: &[],
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
// Since the ExtBundle→ExtTrait fold-in (slice 4) a method bundle is a native `ExtTrait`, namespaced
// to the qualified module (`imgfx.fx`) so `impl fx.Pixels for Px {}` resolves through the surface
// adapter (`resolve_bundle_ref` → `find_trait_in_module`).
const PIXELS_BUNDLE: ExtTrait = ExtTrait {
    name: "Pixels",
    namespace: "imgfx.fx",
    methods: &[ExtTraitMethod {
        sig: ExtFn {
param_names: &[],
            name: "brighten",
            params: &[SigType::F32],
            ret: RetTy::SameAsArg(0),
        },
        has_default: true,
        receiver: BundleReceiver::Bulk,
    }],
    assoc_types: &[],
    dispatch: Some(pixels_bundle_dispatch),
    self_constraint: Some(PackedConstraint {
        fields: &[
            ConstraintField::F32,
            ConstraintField::F32,
            ConstraintField::F32,
        ],
        layout: ConstraintLayout::Any,
        arity: ConstraintArity::Exact,
    }),
    // Prose, so the composed-toolchain docs test proves the whole path: an extension's declaration
    // AND its documentation reach the published artifact.
    doc: "Raw-buffer pixel kernels. `impl fx.Pixels for YourPixel {}` over a three-`f32` @packed \
          struct and the whole list gains `brighten`.",
    docs: &[("brighten", "Brighten every pixel in the list by `delta`.")],
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
fn expand_fx_spec(ctx: &DirectiveCtx) -> Result<Expansion, ExpansionError> {
    let arg = ctx.args.first().ok_or("no spec given")?;
    let path = std::path::Path::new(&ctx.source_dir).join(arg);
    // `?` converts the `String`/`&str` messages via `ExpansionError`'s `From` impls; a real
    // generator that wants the missing path watched would build the struct with `reads` instead.
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

/// A SHAPE-driven directive (`DirectiveCtx::fields`): one accessor per field of the decorated
/// declaration, reporting that field's declared type spelling. It takes no arguments and reads no
/// file, so everything it emits is derived from the declaration's own shape — which makes `noeta
/// expand`'s printout a direct assertion on what the compiler handed the hook.
fn expand_fx_shape(ctx: &DirectiveCtx) -> Result<Expansion, ExpansionError> {
    let mut source = String::new();
    for (name, spelling) in &ctx.fields {
        source.push_str(&format!(
            "fn {name}_type(): string {{ return \"{spelling}\"; }}\n"
        ));
    }
    Ok(Expansion {
        source,
        reads: Vec::new(),
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
    fn traits(&self) -> &'static [ExtTrait] {
        &[PIXELS_BUNDLE]
    }
    fn commands(&self) -> &'static [ExtCommand] {
        &[FX_INFO]
    }
    fn directives(&self) -> &'static [ExtDirective] {
        &[
            ExtDirective {
                name: "fx_spec",
                sites: &[TierSite::Type],
                max_args: Some(1),
                named_keys: &[],
                detail: "@fx_spec(\"<file>\")",
                doc: "Generate one accessor per name in the given spec file.",
                params: &["spec"],
                expand: Some(expand_fx_spec),
            },
            ExtDirective {
                name: "fx_shape",
                sites: &[TierSite::Type],
                max_args: Some(0),
                named_keys: &[],
                detail: "@fx_shape",
                doc: "Generate one accessor per field of the decorated declaration.",
                params: &[],
                expand: Some(expand_fx_shape),
            },
        ]
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
    // Publish-time native-package docs: a native package's module surface exists only in its
    // compiled Rust, so its API docs are generated by running `noeta doc --api` INSIDE a composed
    // toolchain that links the package's extension — a client-side build (here the composed
    // toolchain), never anything on the registry. `noeta publish` runs the `--non-builtin` scope
    // (below); `--root <ns>` remains the explicit user-facing filter. This proves the composed
    // toolchain emits the package's own surface, scoped away from std, in both forms.
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

    // Generate the package's own API docs in the composed toolchain (the explicit `--root` form).
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
    // …and so is the package's native **trait** — the surface `impl fx.Pixels for Px {}` binds
    // against. The generator walked `modules()`/`types()` only, so a published native package's
    // reference named none of its traits, enums, classes or structs. This is the composed-toolchain
    // proof of the whole path: the declaration, its rendered contract, and its prose.
    assert!(
        json.contains("\"kind\": \"trait\""),
        "the Pixels trait is documented:\n{json}"
    );
    assert!(
        json.contains("trait Pixels {"),
        "the trait renders as a declaration:\n{json}"
    );
    assert!(
        json.contains("Raw-buffer pixel kernels"),
        "the trait's prose rides along:\n{json}"
    );
    assert!(
        json.contains("Brighten every pixel"),
        "and its per-method prose:\n{json}"
    );
    // …and the package's `@`-directives, the one declared surface that is neither a callable nor a
    // nominal type. Everything needed to document one was already on `ExtDirective` — its `doc`,
    // `params` and `sites` — and nothing read it, so `@openapi` (para/api's flagship, the whole
    // reason the hook has an `expand`) appeared in no reference.
    assert!(
        json.contains("\"kind\": \"directive\""),
        "the fx directives are documented:\n{json}"
    );
    assert!(
        json.contains("@fx_spec(spec)"),
        "rendered as the invocation its contract accepts:\n{json}"
    );
    assert!(
        json.contains("**Attaches to:** types."),
        "with the placement rule its sites state:\n{json}"
    );
    // …and std is excluded by the root scope (a package documents only itself).
    assert!(
        !json.contains("\"std.math\""),
        "--root imgfx must exclude the stdlib"
    );

    // The PUBLISH scope (`--non-builtin`, what `noeta publish` actually runs since the
    // docsgen-root fix): document every extension the composition adds over the toolchain's
    // builtin units — no root guessed from the package name, so an extension whose `root()`
    // diverges from its package segment (para/p2p rooting at `para`) documents too (that case is
    // unit-tested in noeta-ide::api; here the convention fixture proves the composed path emits
    // the identical surface).
    let out = std::process::Command::new(&composed)
        .args(["doc", "--api", "--non-builtin"])
        .output()
        .expect("run `doc --api --non-builtin` in the composed toolchain");
    assert!(
        out.status.success(),
        "doc --api --non-builtin failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let non_builtin = String::from_utf8(out.stdout).expect("docs.json is UTF-8");
    assert!(
        non_builtin.contains("\"imgfx.fx\""),
        "the package's module is documented without naming a root:\n{non_builtin}"
    );
    assert!(
        !non_builtin.contains("\"std.math\"") && !non_builtin.contains("\"css"),
        "--non-builtin must exclude every builtin unit (std family + html/css)"
    );

    // The publish namespace lint passes on the well-namespaced fixture — in both the explicit
    // `--root` form and the publish-path `--non-builtin` form. The negative
    // case — a type omitting `namespace:` and so defaulting to `std` — no longer *reaches* the
    // lint: registry assembly refuses the unit at startup (`validate()`, unit-tested in
    // noeta-ext-abi as `a_type_namespace_outside_the_units_root_is_rejected`), so a sloppy package
    // fails at its very first composed run with the type named, not at publish time. (The lint
    // violations that DO survive assembly — a toolchain-root squat, a leak in a divergent-root
    // unit list — are unit-tested in noeta-ide::api.)
    for scope in [
        ["doc", "--api", "--root", "imgfx", "--lint"].as_slice(),
        ["doc", "--api", "--non-builtin", "--lint"].as_slice(),
    ] {
        let lint = std::process::Command::new(&composed)
            .args(scope)
            .output()
            .expect("run the publish namespace lint");
        assert!(
            lint.status.success(),
            "the lint ({scope:?}) passes a package whose types are namespaced under its own \
             root: {}",
            String::from_utf8_lossy(&lint.stderr)
        );
    }
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

/// A native package that depends on the contract crate **by version** (the crates.io form) composes
/// and dispatches exactly like one that depends by path. See [`composed_project_versioned`] — this
/// is the regression test for publishing `noeta-ext-abi`, and it fails with a `dyn Extension` type
/// mismatch if `[patch.crates-io]` is dropped from the composed shim.
#[test]
fn a_version_dependency_on_the_contract_crate_composes() {
    let _guard = compose_guard();
    let entry = composed_project_versioned("pm_compose_versioned");
    lang()
        .arg("run")
        .arg(&entry)
        .assert()
        .success()
        // `42` is `fx.double(21)` — a call into the extension's native module. Reaching it proves the
        // package's `dyn Extension` unified with the shim's, i.e. that composition redirected the
        // version requirement to this toolchain's own `noeta-ext-abi` rather than resolving a second
        // copy from the index.
        .stdout(predicate::str::contains("42"));
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

    // 7. `DirectiveCtx::fields`: a hook that takes no arguments and reads no file still generates
    //    members derived from the decorated declaration's **shape**. The spelling must be the
    //    declared one at full fidelity — `List<string>`, never `List` — because the generator writes
    //    it back out as source.
    composed_env(&mut lang())
        .arg("expand")
        .arg(app.join("shape.noe"))
        .assert()
        .success()
        .stdout(
            predicate::str::contains("// Order ⟨@fx_shape⟩")
                .and(predicate::str::contains(
                    r#"fn id_type(): string { return "int"; }"#,
                ))
                .and(predicate::str::contains(
                    r#"fn tags_type(): string { return "List<string>"; }"#,
                )),
        )
        .stderr(predicate::str::contains("expanded 1 declaration"));

    // And it runs: the shape-derived accessor really is callable code, not just printed text.
    composed_env(&mut lang())
        .arg("run")
        .arg(app.join("shape.noe"))
        .assert()
        .success()
        .stdout(predicate::str::contains("List<string>"));

    // 8. **The servers compose too.** `noeta mcp` and `noeta lsp` never delegated, so in any project
    //    with a `[trust] native` dependency the agent's `check` tool — which the generated
    //    `AGENTS.md` offers *as* `noeta check` — answered with an unresolved-import per file, and the
    //    editor showed a working file as broken. The MCP server started in this app must now agree
    //    with step 3's `noeta check`: clean.
    //
    //    A server delegates only on a compose-cache **hit** (a long-lived server may not vanish into
    //    a cargo build at startup), which steps 1-2 have established, so this also pins that the
    //    delegation is keyed on the working directory rather than on a request's path.
    let response = mcp_check(&app, &entry);
    assert!(
        response.contains("\"ok\":true"),
        "the MCP `check` tool must agree with `noeta check` on a composed project: {response}"
    );
    assert!(
        !response.contains("E0019"),
        "no unresolved-import cascade from a delegated server: {response}"
    );

    // 9. **The debugger composes too**, and it is the sharpest case of the three: the adapter does
    //    not merely analyze the program, it loads, checks, compiles and *runs* it. Undelegated, a
    //    launch in this app cannot resolve `imgfx` at all, so debugging failed at its first step —
    //    surfacing as an `output` event full of unresolved imports rather than as a missing
    //    toolchain. A successful launch runs the same entry step 3 checked, so the native handler's
    //    answer (`fx.double(21)`) must appear in the program's output.
    //
    //    The assertion is the program's *output*, not the absence of one diagnostic code. Measured
    //    by neutralizing the delegation: an undelegated launch of this entry reports `E0014 unknown
    //    module fx` and `E0007 no method brighten` — not the `E0019` the MCP surface produces, since
    //    the extension's failure lands wherever its modules were used. Any check for a particular
    //    code would have passed straight through the real defect.
    let output = dap_launch(&app, &entry);
    assert!(
        !output.contains(r#""category":"stderr""#),
        "a delegated debug adapter launches cleanly, with no diagnostics on stderr: {output}"
    );
    assert!(
        output.contains("42"),
        "the debugged program must reach the native handler: {output}"
    );
}

/// Drive a real `noeta dap` server over stdio from `cwd`: launch `entry` and return every `output`
/// event body the adapter emitted, concatenated.
///
/// Spawned as a real process from a real working directory for the same reason as [`mcp_check`] —
/// what is under test is which toolchain the `noeta dap` *process* is, which an in-process harness
/// (`noeta_dap::serve` over in-memory buffers, as `noeta-dap`'s own tests use) cannot show.
/// Hand-rolled DAP framing: `Content-Length: N\r\n\r\n<json>`.
fn dap_launch(cwd: &std::path::Path, entry: &std::path::Path) -> String {
    use std::io::{BufRead, BufReader, Read, Write};

    // stderr goes to a `ServerLog` file, not a pipe: `NOETA_COMPOSE_DEBUG=1` makes this child the
    // chattiest of the stdio-protocol ones — it narrates a whole toolchain composition — and a piped
    // stderr nobody reads blocks the writer at 64 KiB, which here means the adapter stops mid-compose
    // and the framed-message loop below waits on a reply that will never come.
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("noeta"));
    let log = noeta_test_temp::ServerLog::new("dap-launch");
    let mut child = log
        .spawn_stdio_protocol(
            cmd.env(
                "NOETA_CACHE_DIR",
                concat!(env!("CARGO_TARGET_TMPDIR"), "/noeta-cache"),
            )
            .env("NOETA_COMPOSE_DEBUG", "1")
            .env(
                "NOETA_COMPOSE_TARGET_DIR",
                PathBuf::from(env!("CARGO_TARGET_TMPDIR")).parent().unwrap(),
            )
            .arg("dap")
            .current_dir(cwd),
        )
        .expect("the debug adapter starts");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    let mut send = |seq: u32, command: &str, arguments: String| {
        let body = format!(
            r#"{{"seq":{seq},"type":"request","command":"{command}","arguments":{arguments}}}"#
        );
        write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        stdin.flush().unwrap();
    };
    send(1, "initialize", r#"{"adapterID":"noeta"}"#.to_string());
    send(
        2,
        "launch",
        format!(r#"{{"program":"{}"}}"#, entry.display()),
    );
    send(3, "configurationDone", "{}".to_string());

    // Read framed messages until the adapter says the program ended, collecting the `output` bodies.
    // The loop is bounded by `terminated` and by the child's own exit — a launch that never produces
    // it ends when the pipe closes, so a hang here is a real hang and not a missing sentinel.
    let mut collected = String::new();
    let mut header = String::new();
    let mut terminated = false;
    loop {
        header.clear();
        if stdout.read_line(&mut header).unwrap_or(0) == 0 {
            break;
        }
        let Some(len) = header
            .strip_prefix("Content-Length:")
            .and_then(|n| n.trim().parse::<usize>().ok())
        else {
            continue; // the blank separator line
        };
        let mut blank = String::new();
        let _ = stdout.read_line(&mut blank);
        let mut buf = vec![0u8; len];
        if stdout.read_exact(&mut buf).is_err() {
            break;
        }
        let message = String::from_utf8_lossy(&buf).to_string();
        if message.contains(r#""event":"output""#) {
            collected.push_str(&message);
        }
        if message.contains(r#""event":"terminated""#) {
            terminated = true;
            break;
        }
    }
    drop(stdin);
    let _ = child.wait();
    // The pipe closing before `terminated` means the adapter died rather than finished the program.
    // Its reason is on stderr, which the caller's assertion on `collected` cannot show — so say it
    // here, where the log is still in hand.
    assert!(
        terminated,
        "{}",
        log.explain(
            "the debug adapter closed stdout without a `terminated` event — it died mid-launch"
        )
    );
    collected
}

/// Drive a real `noeta mcp` server over stdio from `cwd` and return the raw `tools/call` response
/// for `check` on `entry`.
///
/// Deliberately hand-rolled JSON-RPC over the child's pipes rather than a client library: what is
/// under test is that **the process `noeta mcp` runs in** is the composed toolchain, which only a
/// real spawn from a real working directory can show.
fn mcp_check(cwd: &std::path::Path, entry: &std::path::Path) -> String {
    use std::io::{BufRead, BufReader, Write};

    // A raw `std::process::Command`: `assert_cmd`'s wrapper owns the child's pipes, and this test
    // needs to talk on them. Same environment `lang()`/`composed_env` set, spelled out.
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("noeta"));
    // stderr into a `ServerLog` file for the same two reasons as `dap_launch`: `NOETA_COMPOSE_DEBUG`
    // narration cannot fill an unread pipe and stall the server, and the composition it narrates is
    // exactly what a failing `initialize` here needs quoted.
    let log = noeta_test_temp::ServerLog::new("mcp-compose");
    let mut child = log
        .spawn_stdio_protocol(
            cmd.env(
                "NOETA_CACHE_DIR",
                concat!(env!("CARGO_TARGET_TMPDIR"), "/noeta-cache"),
            )
            .env("NOETA_COMPOSE_DEBUG", "1")
            .env(
                "NOETA_COMPOSE_TARGET_DIR",
                PathBuf::from(env!("CARGO_TARGET_TMPDIR")).parent().unwrap(),
            )
            .arg("mcp")
            .current_dir(cwd),
        )
        .expect("the MCP server starts");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();

    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"e2e","version":"0"}}}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    stdout.read_line(&mut line).expect("the initialize reply");
    assert!(
        line.contains("serverInfo"),
        "{}",
        log.explain(format!("initialize reply: {line}"))
    );

    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized","params":{{}}}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"check","arguments":{{"file":"{}"}}}}}}"#,
        entry.display()
    )
    .unwrap();
    stdin.flush().unwrap();

    line.clear();
    let read = stdout.read_line(&mut line).expect("the check reply");
    drop(stdin);
    let _ = child.wait();
    assert!(
        read != 0,
        "{}",
        log.explain("the MCP server closed stdout without answering `check` — it died mid-request")
    );
    line
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

// --- out-of-tree native package: git ABI dep + [patch] unification (para-extraction) ------------

/// The **out-of-tree** native compose e2e, made a permanent regression test (it was proven only
/// manually during the para extraction): a standalone package repo whose entry crate git-deps
/// `noeta-ext-abi` from a *clone* of the toolchain repo — the exact shape every extracted `para`
/// package ships in — composes against this workspace's source and runs, extension command
/// included. The package repo also path-deps a sibling impl crate (entry/impl split, the
/// first-party layout), so the `[patch]` must unify the ABI across BOTH crates.
///
/// The gotcha this encodes: the compose `[patch]` key must EQUAL the URL the package's Cargo.toml
/// declares for its toolchain git deps. That is why `NOETA_TOOLCHAIN_REPO` is set explicitly to
/// the clone's `file://` URL — the default patch key is this build's `CARGO_PKG_REPOSITORY`,
/// which the fixture package never references, so without the override the git crates would be a
/// SECOND `noeta-ext-abi` and the shim's `NOETA_EXTENSIONS` aggregation would not type-check.
///
/// `#[ignore]`d like the other compose-heavy gates (it clones the repo and cargo-fetches git
/// deps). ci.yml's `test` job runs it as its own serial step — `… --locked -- --ignored
/// composed_toolchain`, whose prefix filter selects this test and its registry sibling — after the
/// step above reclaims ~25 GB of runner disk for the second toolchain the compose builds. That is
/// the one CI step `scripts/gate.sh` deliberately omits (see its header), so run it by hand,
/// `-- --ignored` from the repo root, when you touch native-package composition.
#[test]
#[ignore = "compose-heavy: clones the toolchain repo + composes a toolchain; run explicitly or via its own CI step"]
fn composed_toolchain_out_of_tree_git_abi_dep() {
    let _guard = compose_guard();
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("out_of_tree_git_abi");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    let git = |dir: &std::path::Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    };

    // A depth-1 clone of this workspace's repo — the stand-in for the *published* toolchain repo
    // a standalone package git-deps (`NOETA_TOOLCHAIN_REPO=file://<clone>` below points the
    // compose `[patch]` at the same URL).
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf();
    let clone = base.join("toolchain-clone");
    git(
        &base,
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &format!("file://{}", workspace.display()),
            clone.to_str().unwrap(),
        ],
    );
    let repo_url = format!("file://{}", clone.display());

    // The standalone package repo: `noeta.toml` + entry crate (`native/`) + a sibling impl crate
    // (`impl/`) the entry path-deps. Both crates reference the ABI by git on the clone's URL.
    let pkg = base.join("gitfx");
    std::fs::create_dir_all(pkg.join("native/src")).unwrap();
    std::fs::create_dir_all(pkg.join("impl/src")).unwrap();
    std::fs::write(
        pkg.join("noeta.toml"),
        "[package]\nname = \"acme/gitfx\"\nversion = \"1.0.0\"\nnative = \"native\"\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("native/Cargo.toml"),
        format!(
            "[package]\nname = \"gitfx-native\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
             [lib]\npath = \"src/lib.rs\"\n\n\
             [dependencies]\nnoeta-ext-abi = {{ git = \"{repo_url}\" }}\n\
             gitfx-impl = {{ path = \"../impl\" }}\n\n[workspace]\n"
        ),
    )
    .unwrap();
    std::fs::write(
        pkg.join("native/src/lib.rs"),
        r##"//! Entry crate of the out-of-tree fixture package: declares the extension surface; the
//! behaviour lives in the path-depped sibling impl crate (the first-party entry/impl layout).

use noeta_ext_abi::registry::{ExtFn, ExtModule, Extension, NativeOut, RetTy, SigType};
use noeta_ext_abi::{no_function_error, CommandCtx, ExtCommand, Host, NativeValue, ParsedArgs, StdError};

const GFX_FNS: &[ExtFn] = &[ExtFn {
param_names: &[],
    name: "triple",
    params: &[SigType::Int],
    ret: RetTy::Concrete(SigType::Int),
}];

fn gfx_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "triple" => gitfx_impl::triple(args),
        _ => Err(no_function_error("gfx", func)),
    }
}

const GFX_INFO: ExtCommand = ExtCommand {
    name: "gfx-info",
    about: "Prove an out-of-tree extension command dispatches through composition",
    args: &[],
    run: gfx_info_run,
};

fn gfx_info_run(_ctx: &mut dyn CommandCtx, _args: &ParsedArgs) -> u8 {
    println!("gitfx: out-of-tree native extension ok");
    0
}

#[derive(Debug, Clone, Copy)]
struct GitfxExtension;

impl Extension for GitfxExtension {
    fn name(&self) -> &'static str {
        "gitfx"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[ExtModule {
            name: "gfx",
            functions: GFX_FNS,
            dispatch: gfx_dispatch,
            ..ExtModule::DEFAULTS
        }]
    }
    fn commands(&self) -> &'static [ExtCommand] {
        &[GFX_INFO]
    }
}

pub static NOETA_EXTENSIONS: &[&(dyn Extension + Sync)] = &[&GitfxExtension];
"##,
    )
    .unwrap();
    std::fs::write(
        pkg.join("impl/Cargo.toml"),
        format!(
            "[package]\nname = \"gitfx-impl\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
             [lib]\npath = \"src/lib.rs\"\n\n\
             [dependencies]\nnoeta-ext-abi = {{ git = \"{repo_url}\" }}\n\n[workspace]\n"
        ),
    )
    .unwrap();
    std::fs::write(
        pkg.join("impl/src/lib.rs"),
        r##"//! Impl crate of the out-of-tree fixture package — ABI-typed behaviour the entry crate
//! path-deps, proving the `[patch]` unifies the git ABI across BOTH crates of the package repo.

use noeta_ext_abi::registry::{NativeOut, Scalar};
use noeta_ext_abi::{ErrorKind, NativeValue, StdError};

pub fn triple(args: &[NativeValue]) -> Result<NativeOut, StdError> {
    match args.first() {
        Some(NativeValue::Scalar(Scalar::Int(n))) => Ok(NativeOut::Scalar(Scalar::Int(n * 3))),
        _ => Err(StdError {
            kind: ErrorKind::ArgType,
            message: "`gfx.triple` expects an int".to_string(),
        }),
    }
}
"##,
    )
    .unwrap();
    git(&pkg, &["init", "-q"]);
    git(&pkg, &["add", "-A"]);
    git(&pkg, &["commit", "-qm", "v1.0.0"]);

    // The consuming app takes the package as a GIT dep (HEAD) and trusts its native + commands.
    let app = base.join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("noeta.toml"),
        format!(
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
             [dependencies]\ngitfx = {{ git = \"file://{}\" }}\n\
             [trust]\nnative = [\"acme/gitfx\"]\n\
             [trust.commands]\ngfx-info = \"acme/gitfx\"\n",
            pkg.display()
        ),
    )
    .unwrap();
    std::fs::write(
        app.join("main.noe"),
        "use gitfx.{gfx}\n\necho gfx.triple(14);\n",
    )
    .unwrap();

    // Compose + run: the native module resolves and dispatches (14 * 3 = 42).
    composed_env(&mut lang())
        .env("NOETA_TOOLCHAIN_REPO", &repo_url)
        .env("NOETA_TOOLCHAIN_SRC", &workspace)
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));

    // The extension-contributed command dispatches through the same composed toolchain.
    composed_env(&mut lang())
        .env("NOETA_TOOLCHAIN_REPO", &repo_url)
        .env("NOETA_TOOLCHAIN_SRC", &workspace)
        .arg("gfx-info")
        .current_dir(&app)
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "gitfx: out-of-tree native extension ok",
        ));
}

// --- out-of-tree native package: REGISTRY-INDEX round trip + trust + lock shape -----------------

/// The **registry-index** sibling of [`composed_toolchain_out_of_tree_git_abi_dep`], and the
/// NATIVE-package half of `doc.rs::pure_source_package_publishes_and_resolves_from_the_registry`
/// (whose pure-source round trip deliberately carries no native crate): a standalone native
/// package repo — entry crate git-depping `noeta-ext-abi` + a path-depped sibling impl crate, the
/// extracted-`para` shape — is `noeta publish`ed into a directory-backed `LocalIndex`
/// (`NOETA_REGISTRY_DIR`), and a fresh consumer takes it as a REGISTRY dependency
/// (`{ version = "^1", package = "acme/imgfx" }`), never a path or git dep. End to end, hermetic
/// (`file://` URLs only, no network), this proves:
///
///   1. resolution REFUSES the native package until the root app's `[trust].native` lists its
///      identity — the Phase-4 authority gate, through the registry path;
///   2. once trusted, `noeta.lock` pins the registry release as git coordinates — `source = "git"`
///      + `url` + `tag` + publish-pinned `sha` — plus the content `hash`;
///   3. the consumer-side compose builds the package FROM THE STORE and the program dispatches its
///      native fn (`NOETA_TOOLCHAIN_REPO` = the `file://` URL the package's Cargo.toml declares,
///      so the compose `[patch]` key matches — the same gotcha the git-dep test encodes);
///   4. the package's `ExtCommand` stays refused until `[trust].commands` grants it, and
///      dispatches once granted.
///
/// `noeta publish` itself composes too (the native-build publish quality gate), so the whole test
/// runs three compositions; like its git-dep sibling it is `#[ignore]`d and runs in ci.yml's `test`
/// job under the shared `-- --ignored composed_toolchain` filter, which the `compose_guard` above
/// keeps serial against its sibling.
#[test]
#[ignore = "compose-heavy: publishes + composes a toolchain; run explicitly or via its own CI step"]
fn composed_toolchain_native_package_from_registry_index() {
    let _guard = compose_guard();
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("native_from_registry_index");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let reg = base.join("registry");
    let cache = base.join("cache");

    // The toolchain repo URL the package's Cargo.toml declares — the workspace itself, so the
    // compose `[patch."<url>"]` (keyed by `NOETA_TOOLCHAIN_REPO`) matches without any clone.
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf();
    let repo_url = format!("file://{}", workspace.display());

    // The standalone native package repo: `noeta.toml` (`native = "native"`) + entry crate +
    // path-depped sibling impl crate — the shape every extracted `para` package ships in.
    let pkg = base.join("imgfx-repo");
    std::fs::create_dir_all(pkg.join("native/src")).unwrap();
    std::fs::create_dir_all(pkg.join("impl/src")).unwrap();
    std::fs::write(
        pkg.join("noeta.toml"),
        "[package]\nname = \"acme/imgfx\"\nversion = \"1.0.0\"\nnative = \"native\"\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("native/Cargo.toml"),
        format!(
            "[package]\nname = \"imgfx-reg-native\"\nversion = \"1.0.0\"\nedition = \"2024\"\n\n\
             [lib]\npath = \"src/lib.rs\"\n\n\
             [dependencies]\nnoeta-ext-abi = {{ git = \"{repo_url}\" }}\n\
             imgfx-reg-impl = {{ path = \"../impl\" }}\n\n[workspace]\n"
        ),
    )
    .unwrap();
    std::fs::write(
        pkg.join("native/src/lib.rs"),
        r##"//! Entry crate of the registry-index fixture package: declares the extension surface;
//! the behaviour lives in the path-depped sibling impl crate (the first-party entry/impl layout).

use noeta_ext_abi::registry::{ExtFn, ExtModule, Extension, NativeOut, RetTy, SigType};
use noeta_ext_abi::{no_function_error, CommandCtx, ExtCommand, Host, NativeValue, ParsedArgs, StdError};

const FX_FNS: &[ExtFn] = &[ExtFn {
param_names: &[],
    name: "triple",
    params: &[SigType::Int],
    ret: RetTy::Concrete(SigType::Int),
}];

fn fx_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "triple" => imgfx_reg_impl::triple(args),
        _ => Err(no_function_error("fx", func)),
    }
}

const IMGFX_INFO: ExtCommand = ExtCommand {
    name: "imgfx-info",
    about: "Prove a registry-resolved extension command dispatches only when command-trusted",
    args: &[],
    run: imgfx_info_run,
};

fn imgfx_info_run(_ctx: &mut dyn CommandCtx, _args: &ParsedArgs) -> u8 {
    println!("imgfx: registry-index native extension ok");
    0
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
            ..ExtModule::DEFAULTS
        }]
    }
    fn commands(&self) -> &'static [ExtCommand] {
        &[IMGFX_INFO]
    }
}

pub static NOETA_EXTENSIONS: &[&(dyn Extension + Sync)] = &[&ImgfxExtension];
"##,
    )
    .unwrap();
    std::fs::write(
        pkg.join("impl/Cargo.toml"),
        format!(
            "[package]\nname = \"imgfx-reg-impl\"\nversion = \"1.0.0\"\nedition = \"2024\"\n\n\
             [lib]\npath = \"src/lib.rs\"\n\n\
             [dependencies]\nnoeta-ext-abi = {{ git = \"{repo_url}\" }}\n\n[workspace]\n"
        ),
    )
    .unwrap();
    std::fs::write(
        pkg.join("impl/src/lib.rs"),
        r##"//! Impl crate of the registry-index fixture package — ABI-typed behaviour the entry
//! crate path-deps, so the `[patch]` must unify the git ABI across BOTH crates here too.

use noeta_ext_abi::registry::{NativeOut, Scalar};
use noeta_ext_abi::{ErrorKind, NativeValue, StdError};

pub fn triple(args: &[NativeValue]) -> Result<NativeOut, StdError> {
    match args.first() {
        Some(NativeValue::Scalar(Scalar::Int(n))) => Ok(NativeOut::Scalar(Scalar::Int(n * 3))),
        _ => Err(StdError {
            kind: ErrorKind::ArgType,
            message: "`fx.triple` expects an int".to_string(),
        }),
    }
}
"##,
    )
    .unwrap();
    git_in(&["init", "-q"], &pkg);
    commit_version(
        &pkg,
        "v1.0.0",
        "[package]\nname = \"acme/imgfx\"\nversion = \"1.0.0\"\nnative = \"native\"\n",
    );

    // Publish the release into the directory-backed index. The native-build publish quality gate
    // composes a toolchain to build the package's own crate, so the compose env + the `[patch]`
    // key (`NOETA_TOOLCHAIN_REPO`) are already needed here.
    let pkg_url = format!("file://{}", pkg.display());
    composed_env(&mut lang())
        .current_dir(&pkg)
        .env("NOETA_REGISTRY_DIR", &reg)
        .env("NOETA_CACHE_DIR", &cache)
        .env("NOETA_TOOLCHAIN_REPO", &repo_url)
        .env("NOETA_TOOLCHAIN_SRC", &workspace)
        .args(["publish", "--git", &pkg_url, "--tag", "v1.0.0"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("building native crate")
                .and(predicate::str::contains("published `acme/imgfx` 1.0.0")),
        );

    // The consumer app: a REGISTRY dependency — a version requirement resolved through the index
    // (`{ version, package }`), never a path or git URL. The key equals the package's root segment
    // (`imgfx`), the native convention: a native module's namespace is compiled into its extension,
    // so it is not re-rooted to an arbitrary key the way a source package's modules are.
    let app = base.join("app");
    std::fs::create_dir_all(&app).unwrap();
    let manifest = |trust: &str| {
        format!(
            "[package]\nname = \"acme/photo_app\"\nversion = \"0.1.0\"\n\n\
             [dependencies]\nimgfx = {{ version = \"^1\", package = \"acme/imgfx\" }}\n{trust}"
        )
    };
    std::fs::write(
        app.join("main.noe"),
        "use imgfx.{fx}\n\necho fx.triple(14);\n",
    )
    .unwrap();

    // 1. The Phase-4 authority gate, through the registry path: WITHOUT `[trust].native` the
    //    resolve refuses the native package, naming the identity and the grant to add.
    std::fs::write(app.join("noeta.toml"), manifest("")).unwrap();
    lang()
        .current_dir(&app)
        .env("NOETA_REGISTRY_DIR", &reg)
        .env("NOETA_CACHE_DIR", &cache)
        .arg("update")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("ships native code")
                .and(predicate::str::contains("acme/imgfx"))
                .and(predicate::str::contains("[trust].native")),
        );

    // 2. WITH the grant the resolve passes, and the lock pins the registry release: git coords
    //    (`source`/`url`/`tag` + the publish-pinned `sha`) and the content `hash`.
    std::fs::write(
        app.join("noeta.toml"),
        manifest("\n[trust]\nnative = [\"acme/imgfx\"]\n"),
    )
    .unwrap();
    lang()
        .current_dir(&app)
        .env("NOETA_REGISTRY_DIR", &reg)
        .env("NOETA_CACHE_DIR", &cache)
        .arg("update")
        .assert()
        .success();
    let lock = std::fs::read_to_string(app.join("noeta.lock")).unwrap();
    let sha = git_sha(&pkg, "v1.0.0");
    for needle in [
        "name = \"acme/imgfx\"".to_string(),
        "version = \"1.0.0\"".to_string(),
        "source = \"git\"".to_string(),
        format!("url = \"{pkg_url}\""),
        "tag = \"v1.0.0\"".to_string(),
        format!("sha = \"{sha}\""),
        "hash = ".to_string(),
    ] {
        assert!(lock.contains(&needle), "lock missing `{needle}`:\n{lock}");
    }

    // 3. Consumer-side compose + run: the store-materialized package's native crate composes
    //    against this workspace's source and the program dispatches its fn (14 * 3 = 42).
    composed_env(&mut lang())
        .env("NOETA_REGISTRY_DIR", &reg)
        .env("NOETA_CACHE_DIR", &cache)
        .env("NOETA_TOOLCHAIN_REPO", &repo_url)
        .env("NOETA_TOOLCHAIN_SRC", &workspace)
        .arg("run")
        .arg(app.join("main.noe"))
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));

    // 4. The package's ExtCommand is NOT dispatchable on `[trust].native` alone: the composed
    //    toolchain registers commands only for `[trust].commands`-granted identities, so the
    //    subcommand stays unknown (clap's error, after the compose delegation).
    composed_env(&mut lang())
        .current_dir(&app)
        .env("NOETA_REGISTRY_DIR", &reg)
        .env("NOETA_CACHE_DIR", &cache)
        .env("NOETA_TOOLCHAIN_REPO", &repo_url)
        .env("NOETA_TOOLCHAIN_SRC", &workspace)
        .arg("imgfx-info")
        .assert()
        .failure()
        .stderr(predicate::str::contains("imgfx-info"))
        .stdout(predicate::str::contains("registry-index native extension ok").not());

    // 5. WITH a `[trust.commands]` binding the command-trust change recomposes and the command
    //    dispatches under its bound local name (here the same as its exported name).
    std::fs::write(
        app.join("noeta.toml"),
        manifest(
            "\n[trust]\nnative = [\"acme/imgfx\"]\n[trust.commands]\nimgfx-info = \"acme/imgfx\"\n",
        ),
    )
    .unwrap();
    composed_env(&mut lang())
        .current_dir(&app)
        .env("NOETA_REGISTRY_DIR", &reg)
        .env("NOETA_CACHE_DIR", &cache)
        .env("NOETA_TOOLCHAIN_REPO", &repo_url)
        .env("NOETA_TOOLCHAIN_SRC", &workspace)
        .arg("imgfx-info")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "imgfx: registry-index native extension ok",
        ));
}
