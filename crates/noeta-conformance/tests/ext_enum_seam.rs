//! **Native-declared enums** (native-extensibility S1): an extension *outside* `std` declares real
//! language enums — plain, string-backed, int-backed, and payload-carrying — and a program returns,
//! `match`es (exhaustively, E0011), and passes their values back into native code.
//!
//! A synthetic extension rather than an `std` consumer, deliberately: the whole path is
//! registry-driven, so nothing about `Hue`/`Tone`/`Level`/`Tag` is known to the checker or either
//! backend except its [`ExtEnum`] declaration. The corpus's `differential_backends_agree` oracle
//! cannot reach it (std declares no native enum), so the differential assertion lives here: the
//! tree-walker reference and the bytecode VM must agree on the materialized enum values, which is
//! what proves `NativeOut::Variant` is built identically on both sides.
//!
//! An **integration test** (own process) because the fixture unit installs into the process-global
//! default registry — once per process — the same single-registry path the CLI uses.

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_stdlib::registry::{
    EnumBacking, ExtEnum, ExtFn, ExtModule, ExtVariant, Extension, NativeOut, NativeValue, RetTy,
    SigType, VariantValue,
};
use noeta_stdlib::{Host, StdError};
use noeta_vm::VmBackend;

// --- The fixture extension: four native enums + a module that round-trips them -------------------

const HUE: ExtEnum = ExtEnum {
    name: "Hue",
    namespace: "shade",
    variants: &[
        ExtVariant {
            name: "Red",
            fields: &[],
            value: VariantValue::None,
        },
        ExtVariant {
            name: "Green",
            fields: &[],
            value: VariantValue::None,
        },
        ExtVariant {
            name: "Blue",
            fields: &[],
            value: VariantValue::None,
        },
    ],
    backing: EnumBacking::None,
    // A **fieldless-variant** instance method (Slice B): reads the case off the marshalled
    // `Variant` and lower-cases it (`Hue.Red.name()` → "red").
    methods: &[ExtFn {
        name: "name",
        params: &[],
        ret: RetTy::Concrete(SigType::String),
    }],
    dispatch: shade_enum_dispatch,
    traits: &[],
};

const TONE: ExtEnum = ExtEnum {
    name: "Tone",
    namespace: "shade",
    variants: &[
        ExtVariant {
            name: "Warm",
            fields: &[],
            value: VariantValue::Str("warm"),
        },
        ExtVariant {
            name: "Cool",
            fields: &[],
            value: VariantValue::Str("cool"),
        },
    ],
    backing: EnumBacking::Str,
    // A method-less enum still routes `.value()` through the built-in accessor (unchanged): a
    // native enum method call never shadows it. `dispatch` is inert here (no `methods`).
    methods: &[],
    dispatch: shade_enum_dispatch,
    traits: &[],
};

const LEVEL: ExtEnum = ExtEnum {
    name: "Level",
    namespace: "shade",
    variants: &[
        ExtVariant {
            name: "Low",
            fields: &[],
            value: VariantValue::Int(1),
        },
        ExtVariant {
            name: "High",
            fields: &[],
            value: VariantValue::Int(9),
        },
    ],
    backing: EnumBacking::Int,
    methods: &[],
    dispatch: shade_enum_dispatch,
    traits: &[],
};

const TAG: ExtEnum = ExtEnum {
    name: "Tag",
    namespace: "shade",
    variants: &[
        ExtVariant {
            name: "Plain",
            fields: &[],
            value: VariantValue::None,
        },
        ExtVariant {
            name: "Labeled",
            fields: &[SigType::String],
            value: VariantValue::None,
        },
    ],
    backing: EnumBacking::None,
    // A **payload-carrying-variant** instance method (Slice B): reads the payload off the
    // marshalled `Variant` — `Tag.Labeled("hi").describe()` → "labeled:hi", `Tag.Plain` → "plain".
    methods: &[ExtFn {
        name: "describe",
        params: &[],
        ret: RetTy::Concrete(SigType::String),
    }],
    dispatch: shade_enum_dispatch,
    traits: &[],
};

const FX_ENUMS: &[ExtEnum] = &[HUE, TONE, LEVEL, TAG];

const PALETTE_FNS: &[ExtFn] = &[
    ExtFn {
        name: "pick",
        params: &[],
        ret: RetTy::Concrete(SigType::Named("Hue")),
    },
    // Takes a variant as an argument (arg-IN), returns its case name.
    ExtFn {
        name: "name_of",
        params: &[SigType::Named("Hue")],
        ret: RetTy::Concrete(SigType::String),
    },
    ExtFn {
        name: "default_tone",
        params: &[],
        ret: RetTy::Concrete(SigType::Named("Tone")),
    },
    ExtFn {
        name: "default_level",
        params: &[],
        ret: RetTy::Concrete(SigType::Named("Level")),
    },
    // Returns a payload-carrying variant (return-OUT with a payload).
    ExtFn {
        name: "make_tag",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Named("Tag")),
    },
    // Takes a payload-carrying variant back (arg-IN with a payload).
    ExtFn {
        name: "tag_label",
        params: &[SigType::Named("Tag")],
        ret: RetTy::Concrete(SigType::String),
    },
];

fn variant(
    enum_name: &str,
    variant: &str,
    variant_index: u32,
    fields: Vec<NativeOut>,
) -> NativeOut {
    NativeOut::Variant {
        enum_name: enum_name.to_string(),
        variant: variant.to_string(),
        variant_index,
        fields,
    }
}

fn palette_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "pick" => Ok(variant("Hue", "Green", 1, vec![])),
        "name_of" => {
            let name = match args.first() {
                Some(NativeValue::Variant { variant, .. }) => variant.clone(),
                _ => "?".to_string(),
            };
            Ok(NativeOut::Str(name))
        }
        "default_tone" => Ok(variant("Tone", "Warm", 0, vec![])),
        "default_level" => Ok(variant("Level", "High", 1, vec![])),
        "make_tag" => {
            let label = match args.first() {
                Some(NativeValue::Str(s)) => s.clone(),
                _ => String::new(),
            };
            Ok(variant("Tag", "Labeled", 1, vec![NativeOut::Str(label)]))
        }
        "tag_label" => {
            let label = match args.first() {
                Some(NativeValue::Variant {
                    variant, fields, ..
                }) if variant == "Labeled" => match fields.first() {
                    Some(NativeValue::Str(s)) => s.clone(),
                    _ => String::new(),
                },
                _ => "plain".to_string(),
            };
            Ok(NativeOut::Str(label))
        }
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no function `{func}`"),
        }),
    }
}

/// The four enums' shared **instance-method** dispatch (Slice B) — the [`ExtEnum`] twin of a
/// fielded type's `dispatch`, reusing the neutral `NativeMethodDispatch` seam. The receiver crosses
/// as a [`NativeValue::Variant`] (case + payload); routing (`find_enum_method`) has already ensured
/// the method belongs to the receiver's enum, so matching on the method name alone is sufficient and
/// robust to the short-vs-qualified spelling of the receiver's `enum_name`.
fn shade_enum_dispatch(
    recv: &NativeValue,
    method: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let NativeValue::Variant {
        variant, fields, ..
    } = recv
    else {
        return Err(StdError {
            kind: noeta_stdlib::ErrorKind::ArgType,
            message: "enum method called on a non-variant receiver".to_string(),
        });
    };
    match method {
        // A fieldless-variant method: the case name, lower-cased.
        "name" => Ok(NativeOut::Str(variant.to_lowercase())),
        // A payload-carrying-variant method: reads the payload off the marshalled `Variant`.
        "describe" => {
            let out = match variant.as_str() {
                "Labeled" => match fields.first() {
                    Some(NativeValue::Str(s)) => format!("labeled:{s}"),
                    _ => "labeled:?".to_string(),
                },
                _ => "plain".to_string(),
            };
            Ok(NativeOut::Str(out))
        }
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no enum method `{method}`"),
        }),
    }
}

struct ShadeExtension;

impl Extension for ShadeExtension {
    fn name(&self) -> &'static str {
        "shade"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[ExtModule {
            name: "palette",
            functions: PALETTE_FNS,
            dispatch: palette_dispatch,
            ..ExtModule::DEFAULTS
        }]
    }
    fn enums(&self) -> &'static [ExtEnum] {
        FX_ENUMS
    }
}

static SHADE: ShadeExtension = ShadeExtension;

/// Install the fixture unit exactly once per process — the registry is process-global and `install`
/// must run before any lookup, so both tests in this binary share one installation.
fn ensure_installed() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[&SHADE]));
}

const PROGRAM: &str = r#"
use shade.palette
use shade.Hue
use shade.Tag

// return-OUT + exhaustive `match` (E0011) over a real native enum value.
h = palette.pick()
echo match h {
    Hue.Red => "red",
    Hue.Green => "green",
    Hue.Blue => "blue",
}

// arg-IN: a fieldless variant crosses back into native code.
echo palette.name_of(h)

// A backed enum's `.value()` yields its declared backing scalar (string and int).
echo palette.default_tone().value()
echo palette.default_level().value()

// A payload-carrying variant: returned, match-bound, and passed back in.
t = palette.make_tag("hi")
echo match t {
    Tag.Plain => "plain",
    Tag.Labeled(s) => s,
}
echo palette.tag_label(t)
"#;

const EXPECTED_STDOUT: &str = "green\nGreen\nwarm\n9\nhi\nhi\n";

#[test]
fn native_enums_round_trip_identically_on_both_backends() {
    ensure_installed();

    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_enum_seam.noe", PROGRAM);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);

    let parsed = noeta_db::ast(&db, src);
    assert!(
        parsed.0.diagnostics.is_empty(),
        "fixture program must parse cleanly: {:?}",
        parsed.0.diagnostics
    );
    // The native enum unifies by qualified identity: `use shade.Hue` re-roots the short name onto
    // `shade.Hue`, so the match scrutinee (typed from `pick(): Hue`) is exhaustive and clean.
    let checked = noeta_db::checked(&db, src);
    assert!(
        checked.diagnostics.is_empty(),
        "fixture program must check cleanly: {:?}",
        checked.diagnostics
    );

    let reference =
        noeta_conformance::reference::reference_run(&parsed.0.program, checked.sites.clone());
    let module = noeta_db::bytecode(&db, src)
        .0
        .as_ref()
        .expect("fixture program compiles to bytecode")
        .clone();
    let vm = VmBackend::new().run_module(&module);

    assert_eq!(
        reference, vm,
        "backends must agree on the native-enum round-trip program"
    );
    assert_eq!(
        reference.exit_code, 0,
        "diagnostics: {:?}",
        reference.diagnostics
    );
    assert_eq!(reference.stdout, EXPECTED_STDOUT);
}

/// **Source-level construction** (native-extensibility S1b): a native enum variant written directly
/// in Noeta source — `Hue.Red`, backed `Tone.Cool`/`Level.Low`, payload-carrying `Tag.Labeled(s)` —
/// constructs a real enum value, usable in a `match`, comparable with `==`, carrying its declared
/// `.value()`, and passable back into native code. The two backends must build the identical value,
/// which is the differential assertion here (the S1 round-trip only *received* a variant from native
/// code; this originates one).
const CONSTRUCT_PROGRAM: &str = r#"
use shade.palette
use shade.Hue
use shade.Tone
use shade.Level
use shade.Tag

// Construct a fieldless variant in source, then match it.
c = Hue.Red
echo match c {
    Hue.Red => "red",
    Hue.Green => "green",
    Hue.Blue => "blue",
}

// A constructed variant is `==`-comparable and passes into native code (arg-IN).
echo Hue.Blue == Hue.Blue
echo Hue.Blue == Hue.Green
echo palette.name_of(Hue.Green)

// Backed-enum construction carries its declared `.value()` (string- and int-backed).
echo Tone.Cool.value()
echo Level.Low.value()

// Payload-carrying variant construction: match-bound, and passed back into native code.
lt = Tag.Labeled("built")
echo match lt {
    Tag.Plain => "plain",
    Tag.Labeled(s) => s,
}
echo palette.tag_label(lt)
"#;

const CONSTRUCT_EXPECTED_STDOUT: &str = "red\ntrue\nfalse\nGreen\ncool\n1\nbuilt\nbuilt\n";

#[test]
fn native_enum_source_construction_round_trips_identically_on_both_backends() {
    ensure_installed();

    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_enum_construct.noe", CONSTRUCT_PROGRAM);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);

    let parsed = noeta_db::ast(&db, src);
    assert!(
        parsed.0.diagnostics.is_empty(),
        "construction program must parse cleanly: {:?}",
        parsed.0.diagnostics
    );
    let checked = noeta_db::checked(&db, src);
    assert!(
        checked.diagnostics.is_empty(),
        "construction program must check cleanly: {:?}",
        checked.diagnostics
    );

    let reference =
        noeta_conformance::reference::reference_run(&parsed.0.program, checked.sites.clone());
    let module = noeta_db::bytecode(&db, src)
        .0
        .as_ref()
        .expect("construction program compiles to bytecode")
        .clone();
    let vm = VmBackend::new().run_module(&module);

    assert_eq!(
        reference, vm,
        "backends must agree on the native-enum construction program"
    );
    assert_eq!(
        reference.exit_code, 0,
        "diagnostics: {:?}",
        reference.diagnostics
    );
    assert_eq!(reference.stdout, CONSTRUCT_EXPECTED_STDOUT);
}

/// **Native enum instance methods** (native-extensibility S1 / Slice B): a native enum declares
/// instance methods dispatched to native code, exactly like a fielded type — reusing the shared
/// dispatch seam. A fieldless-variant method (`Hue.name()`) and a payload-carrying-variant method
/// (`Tag.describe()`, which reads the payload off the marshalled `Variant`) both route to the enum's
/// native `dispatch`, and the two backends must materialize the identical result. The program also
/// re-exercises S1's `.value()`, `match`, and source construction alongside the new methods, so this
/// one differential run proves the method path does not disturb any of them.
const METHOD_PROGRAM: &str = r#"
use shade.palette
use shade.Hue
use shade.Tag

// A fieldless-variant instance method on a native-returned value and a source-constructed one.
h = palette.pick()
echo h.name()
echo Hue.Red.name()
echo Hue.Blue.name()

// A method call never shadows a `match` over the same value (S1 still works).
echo match Hue.Red {
    Hue.Red => "matched-red",
    Hue.Green => "matched-green",
    Hue.Blue => "matched-blue",
}

// A payload-carrying-variant method reads the payload off the marshalled `Variant`.
t = palette.make_tag("hi")
echo t.describe()
echo Tag.Labeled("built").describe()
echo Tag.Plain.describe()

// The built-in backed-enum `.value()` accessor is untouched by the method path.
echo palette.default_tone().value()
echo palette.default_level().value()
"#;

const METHOD_EXPECTED_STDOUT: &str =
    "green\nred\nblue\nmatched-red\nlabeled:hi\nlabeled:built\nplain\nwarm\n9\n";

#[test]
fn native_enum_instance_methods_dispatch_identically_on_both_backends() {
    ensure_installed();

    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_enum_methods.noe", METHOD_PROGRAM);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);

    let parsed = noeta_db::ast(&db, src);
    assert!(
        parsed.0.diagnostics.is_empty(),
        "method program must parse cleanly: {:?}",
        parsed.0.diagnostics
    );
    // The native enum's methods type-check off its `ExtEnum::methods` signature table: `h.name()`
    // is `string`, `t.describe()` is `string` — the enum twin of a fielded type's method typing.
    let checked = noeta_db::checked(&db, src);
    assert!(
        checked.diagnostics.is_empty(),
        "method program must check cleanly: {:?}",
        checked.diagnostics
    );

    // Leak oracle (mirrors `ext_class_seam.rs`'s `run_both_agree`): each backend must return heap
    // residency to baseline across the run, so a native-enum method that marshals a payload-carrying
    // `Variant` in and a string out leaks nothing.
    let eval_before = noeta_eval::live_count();
    let reference =
        noeta_conformance::reference::reference_run(&parsed.0.program, checked.sites.clone());
    let eval_after = noeta_eval::live_count();
    assert_eq!(
        eval_before, eval_after,
        "tree-walker leak oracle: heap residency must return to baseline"
    );

    let module = noeta_db::bytecode(&db, src)
        .0
        .as_ref()
        .expect("method program compiles to bytecode")
        .clone();
    let vm_before = noeta_value::live_count() as i64;
    let vm = VmBackend::new().run_module(&module);
    let vm_after = noeta_value::live_count() as i64;
    assert_eq!(
        vm_before, vm_after,
        "VM leak oracle: heap residency must return to baseline"
    );

    assert_eq!(
        reference, vm,
        "backends must agree on the native-enum instance-method program"
    );
    assert_eq!(
        reference.exit_code, 0,
        "diagnostics: {:?}",
        reference.diagnostics
    );
    assert_eq!(reference.stdout, METHOD_EXPECTED_STDOUT);
}

/// A non-exhaustive `match` over a native enum is E0011, exactly as it is for a `.noe` enum — the
/// native enum's variants reach the exhaustiveness checker through `symbols.enums` keyed by its
/// qualified identity.
#[test]
fn a_non_exhaustive_match_over_a_native_enum_is_rejected() {
    ensure_installed();

    const NON_EXHAUSTIVE: &str = r#"
use shade.palette
use shade.Hue

h = palette.pick()
echo match h {
    Hue.Red => "red",
    Hue.Green => "green",
}
"#;

    let db = LangDatabase::default();
    let source = Source::new(
        SourceId::FIRST,
        "ext_enum_nonexhaustive.noe",
        NON_EXHAUSTIVE,
    );
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);
    let checked = noeta_db::checked(&db, src);
    assert!(
        checked
            .diagnostics
            .iter()
            .any(|d| d.message.contains("non-exhaustive") && d.message.contains("Blue")),
        "expected a non-exhaustive-match diagnostic naming the missing `Blue`, got {:?}",
        checked.diagnostics
    );
}
