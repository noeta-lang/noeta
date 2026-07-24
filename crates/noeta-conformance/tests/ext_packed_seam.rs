//! **Native-declared `@packed` structs** (native type-declaration unification, Slice E1): an
//! extension *outside* `std` declares real language **value structs** carrying the built-in
//! [`ExtTypeDirective::Packed`] directive — the native twin of a `.noe` `@packed struct` — and a
//! program lays a `List` of them out as one flat, contiguous raw-primitive buffer, exactly as a source
//! `@packed` struct's list does. The whole path is registry-driven: nothing about `Pt` is known to the
//! checker or either backend except its [`ExtStruct`] declaration and its `Packed(Row)` directive.
//!
//! **The load-bearing test** is [`source_list_of_a_native_packed_struct_packs_flat`]: it proves the
//! source-construction parity that Slice E1 is about — once the native `@packed` struct's qualified
//! identity is seeded into `packed_structs` (by `seed_ext_directives`) and its fields into `records`
//! (by `seed_ext_fielded`), a source `List<Pt>` literal hits `note_packed_list` → `packed_layout` and
//! the checker records a flat [`PackedLayout`] at the construction site, so both backends pack it flat
//! (eval `ListRepr::Packed` / VM `Op::MakePackedList`) — no new code beyond the directive seed. The
//! recorded layout is asserted directly (`packed_list_sites` is non-empty, keyed by the *qualified*
//! `geo.Pt`, fields in slot order), and the program is run on both backends (they must agree, leak
//! nothing).
//!
//! The [`column_packed_struct_is_column_major`] test proves a `Packed(Column)` directive seeds
//! `column_structs` (the recorded layout's `column` flag is set), the column-major twin.
//!
//! An **integration test** (own process) because the fixture installs into the process-global default
//! registry — once per process — the single-registry path the CLI uses.

use noeta_ast::reflect::{PackedKind, PackedLayout};
use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_stdlib::registry::{
    ExtField, ExtFn, ExtModule, ExtStruct, ExtTypeDirective, Extension, FieldedKind, NativeOut,
    NativeValue, PackedLayoutKind, RetTy, Scalar, SigType,
};
use noeta_stdlib::{Host, StdError};
use noeta_vm::VmBackend;

// --- The native @packed value structs ------------------------------------------------------------

/// A `@packed(Row)` value struct `Pt { x: int, y: int }` — the native twin of a `.noe`
/// `@packed struct`. All-primitive, all-public, source-constructible. Its [`ExtTypeDirective::Packed`]
/// directive seeds its qualified identity (`geo.Pt`) into the checker's `packed_structs`, so a source
/// `List<Pt>` packs flat.
const PT: ExtStruct = ExtStruct {
    name: "Pt",
    namespace: "geo",
    fields: &[
        ExtField {
            name: "x",
            ty: SigType::Int,
            is_public: true,
            is_mut: false,
        },
        // `y` is `mut` (rebindable via `p.y = ...`, a value-semantic copy-on-write) so the
        // copy-on-assign test can exercise it; packing is field-type based, unaffected by mutability.
        ExtField {
            name: "y",
            ty: SigType::Int,
            is_public: true,
            is_mut: true,
        },
    ],
    directives: &[ExtTypeDirective::Packed(PackedLayoutKind::Row)],
    ..ExtStruct::STRUCT_DEFAULTS
};

/// A `@packed(Layout.Column)` value struct — the column-major twin. Its directive seeds
/// `column_structs` in addition to `packed_structs`, so a `List<Grid>` records a `column`-flagged
/// layout.
const GRID: ExtStruct = ExtStruct {
    name: "Grid",
    namespace: "geo",
    fields: &[
        ExtField {
            name: "u",
            ty: SigType::Int,
            is_public: true,
            is_mut: false,
        },
        ExtField {
            name: "v",
            ty: SigType::Int,
            is_public: true,
            is_mut: false,
        },
    ],
    directives: &[ExtTypeDirective::Packed(PackedLayoutKind::Column)],
    ..ExtStruct::STRUCT_DEFAULTS
};

// --- A module that natively constructs a Pt ------------------------------------------------------

const KIT_FNS: &[ExtFn] = &[
    // Native constructor: returns a real `Pt` value (a single `@packed` value is ALWAYS boxed — flat
    // storage is a property of the *list*, so this crosses back as the same boxed `Object` a source
    // `Pt { .. }` literal builds).
    ExtFn {
        name: "make",
        params: &[SigType::Int, SigType::Int],
        ret: RetTy::Concrete(SigType::Named("Pt")),
    },
    // Takes a `Pt` back and sums its fields — proves a native `@packed` struct marshals INTO a dispatch
    // as a `NativeValue::Instance` (boxed), like any native struct.
    ExtFn {
        name: "sum",
        params: &[SigType::Named("Pt")],
        ret: RetTy::Concrete(SigType::Int),
    },
];

fn make_pt(x: i64, y: i64) -> NativeOut {
    NativeOut::Instance {
        class: "Pt".to_string(),
        fields: vec![
            ("x".to_string(), NativeOut::Scalar(Scalar::Int(x))),
            ("y".to_string(), NativeOut::Scalar(Scalar::Int(y))),
        ],
        kind: FieldedKind::Struct,
    }
}

fn kit_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let arg_int = |i: usize| -> i64 {
        match args.get(i) {
            Some(NativeValue::Scalar(Scalar::Int(n))) => *n,
            _ => 0,
        }
    };
    match func {
        "make" => Ok(make_pt(arg_int(0), arg_int(1))),
        "sum" => {
            let field = |name: &str| -> i64 {
                match args.first() {
                    Some(NativeValue::Instance { fields, .. }) => fields
                        .iter()
                        .find(|(k, _)| k == name)
                        .and_then(|(_, v)| match v {
                            NativeValue::Scalar(Scalar::Int(n)) => Some(*n),
                            _ => None,
                        })
                        .unwrap_or(0),
                    _ => 0,
                }
            };
            Ok(NativeOut::Scalar(Scalar::Int(field("x") + field("y"))))
        }
        _ => Err(StdError {
            kind: noeta_stdlib::ErrorKind::UnknownName,
            message: format!("no function `{func}`"),
        }),
    }
}

struct GeoExtension;

impl Extension for GeoExtension {
    fn name(&self) -> &'static str {
        "geo_packed"
    }
    fn root(&self) -> &'static str {
        "geo"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[ExtModule {
            name: "kit",
            functions: KIT_FNS,
            dispatch: kit_dispatch,
            ..ExtModule::DEFAULTS
        }]
    }
    fn structs(&self) -> &'static [ExtStruct] {
        &[PT, GRID]
    }
}

static GEO: GeoExtension = GeoExtension;

fn ensure_installed() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[&GEO]));
}

/// Every fixture test touches the shared process-global registry; each holds this for its whole run so
/// the installs/counters do not race (mirrors `ext_struct_seam.rs`).
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

// --- Helpers -------------------------------------------------------------------------------------

/// Check + run a program on both backends, asserting they agree, exit 0, and each leaves the heap
/// residency unchanged (leak oracle zero). Returns the shared stdout.
#[track_caller]
fn run_both_agree(program: &str) -> String {
    ensure_installed();
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_packed_seam.noe", program);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);

    let parsed = noeta_db::ast(&db, src);
    assert!(
        parsed.0.diagnostics.is_empty(),
        "program must parse cleanly: {:?}",
        parsed.0.diagnostics
    );
    let checked = noeta_db::checked(&db, src);
    assert!(
        checked.diagnostics.is_empty(),
        "program must check cleanly: {:?}",
        checked.diagnostics
    );

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
        .expect("program compiles to bytecode")
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
        "backends must agree on the native-@packed-struct program"
    );
    assert_eq!(
        reference.exit_code, 0,
        "diagnostics: {:?}",
        reference.diagnostics
    );
    reference.stdout
}

/// The [`PackedLayout`]s the checker recorded at `program`'s list-construction sites (the flat-storage
/// channel both backends key on). Empty unless a `List<packed>` literal packed flat. This is the direct
/// observable of source-construction parity: a native `@packed` struct's list must record a layout here
/// exactly as a `.noe` `@packed` struct's does.
#[track_caller]
fn recorded_packed_layouts(program: &str) -> Vec<PackedLayout> {
    ensure_installed();
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ext_packed_seam.noe", program);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);
    let checked = noeta_db::checked(&db, src);
    assert!(
        checked.diagnostics.is_empty(),
        "program must check cleanly: {:?}",
        checked.diagnostics
    );
    checked.sites.packed_list_sites.values().cloned().collect()
}

// --- Tests ---------------------------------------------------------------------------------------

/// **The load-bearing test — source-construction parity.** A source `List<Pt>` literal (`Pt` a native
/// `@packed` struct) records a flat [`PackedLayout`] at its construction site, keyed on `Pt`'s
/// **qualified** identity (`geo.Pt`) with the fields in slot order — proving `note_packed_list` →
/// `packed_layout` resolved the native struct's fields out of the qualified `records` seed with ZERO
/// new code beyond the `Packed` directive seed. Both backends then pack it flat and agree, leaking
/// nothing.
const FLAT_PROGRAM: &str = r#"
use geo.kit
use geo.Pt

// A List of a native @packed struct — must lay out flat, keyed by the construction-site span.
xs = [Pt { x: 1, y: 2 }, Pt { x: 3, y: 4 }, Pt { x: 5, y: 6 }]
echo xs.len()
echo xs[0].x
echo xs[1].y
echo xs[2].x

// A native-built Pt drops into a flat List<Pt> just like a source-built one.
ys = [kit.make(7, 8), Pt { x: 9, y: 10 }]
echo ys[0].x
echo ys[1].y
echo kit.sum(ys[0])
"#;

#[test]
fn source_list_of_a_native_packed_struct_packs_flat() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // 1. The checker records a flat layout at the `List<Pt>` construction site — the direct proof that
    //    the native `@packed` directive seed made source construction pack flat with no new code.
    let layouts = recorded_packed_layouts(FLAT_PROGRAM);
    assert!(
        !layouts.is_empty(),
        "a source List<Pt> of a native @packed struct must record a flat packed layout"
    );
    let pt_layout = layouts
        .iter()
        .find(|l| l.type_name == "geo.Pt")
        .expect("the recorded layout is keyed on Pt's QUALIFIED identity (geo.Pt)");
    assert_eq!(
        pt_layout
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>(),
        ["x", "y"],
        "fields recorded in slot order"
    );
    assert!(
        pt_layout.fields.iter().all(|f| f.kind == PackedKind::Int),
        "both fields pack as Int"
    );
    assert!(
        !pt_layout.column,
        "a Packed(Row) struct is row-major (not in column_structs)"
    );

    // 2. Both backends pack it flat, agree, and leak nothing.
    let stdout = run_both_agree(FLAT_PROGRAM);
    assert_eq!(stdout, "3\n1\n4\n5\n7\n10\n15\n");
}

/// **Value semantics** (the same shape as `ext_struct_seam::native_structs_have_value_semantics`): a
/// native `@packed` struct is still a *value* type — structural `==` and copy-on-assign — a single
/// `@packed` value being boxed does not change that. Both backends agree, heap returns to baseline.
const VALUE_SEMANTICS_PROGRAM: &str = r#"
use geo.Pt

// Structural equality: equal fields ⇒ equal (value type, not reference identity).
echo Pt { x: 1, y: 2 } == Pt { x: 1, y: 2 }
echo Pt { x: 1, y: 2 } == Pt { x: 1, y: 3 }

// Copy-on-assign: `b` is an independent copy of `a`; rebinding `b.y` does NOT affect `a`.
a = Pt { x: 1, y: 2 }
mut b = a
b.y = 99
echo a.y
echo b.y
"#;

#[test]
fn native_packed_structs_have_value_semantics() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let stdout = run_both_agree(VALUE_SEMANTICS_PROGRAM);
    assert_eq!(stdout, "true\nfalse\n2\n99\n");
}

/// **Native single-value construction agrees with source construction, and both drop into a flat
/// list.** A single `@packed` value is always boxed (flat storage is a list property), so a
/// native-constructed `kit.make(x, y)` and a source `Pt { .. }` literal are the same boxed value —
/// interchangeable, structurally equal, and both usable as elements of a flat `List<Pt>`. Both
/// backends agree, leaking nothing.
const CONSTRUCTION_PARITY_PROGRAM: &str = r#"
use geo.kit
use geo.Pt

// A native-built and a source-built Pt with equal fields are structurally equal (boxed value type).
echo kit.make(3, 4) == Pt { x: 3, y: 4 }

// Both go into the same flat List<Pt>; read them back and sum through native code.
mixed = [Pt { x: 1, y: 2 }, kit.make(3, 4)]
echo mixed[0].x
echo mixed[1].x
echo kit.sum(mixed[0])
echo kit.sum(mixed[1])
"#;

#[test]
fn native_and_source_packed_construction_agree_and_drop_into_a_flat_list() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let stdout = run_both_agree(CONSTRUCTION_PARITY_PROGRAM);
    assert_eq!(stdout, "true\n1\n3\n3\n7\n");
}

/// **`@packed(Column)` seeds `column_structs`** — a `List<Grid>` records a `column`-flagged layout
/// (column-major storage), while the `Packed(Row)` `Pt` does not. Both backends agree on the program.
const COLUMN_PROGRAM: &str = r#"
use geo.Grid

gs = [Grid { u: 1, v: 2 }, Grid { u: 3, v: 4 }]
echo gs.len()
echo gs[0].u
echo gs[1].v
"#;

#[test]
fn column_packed_struct_is_column_major() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let layouts = recorded_packed_layouts(COLUMN_PROGRAM);
    let grid = layouts
        .iter()
        .find(|l| l.type_name == "geo.Grid")
        .expect("a source List<Grid> of a native @packed(Column) struct records a flat layout");
    assert!(
        grid.column,
        "a @packed(Layout.Column) struct is stored column-major (column_structs membership)"
    );

    let stdout = run_both_agree(COLUMN_PROGRAM);
    assert_eq!(stdout, "2\n1\n4\n");
}
