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
