//! The **declared-type reflection round-trip oracle**: a surface type annotation, projected onto
//! its reflection [`TypeRepr`] and rendered back to a surface spelling, must denote the *same*
//! reflected type when that spelling is parsed again — `repr(parse(render(repr(t)))) == repr(t)` —
//! and the render must be a fixed point (`render` of the re-parsed repr is byte-identical).
//!
//! Why this exists (reflection-drift audit): the eval↔VM differential is blind to **correlated**
//! error — when both backends share a conversion table, they agree on the same wrong answer
//! (`type_ref_repr` dropping attribute type arguments hid exactly that way). This oracle is
//! backend-free: it pits the three independently-written legs against each other — the parser's
//! type grammar, the `TypeRef → TypeRepr` projection (`typeref_to_repr`, the funnel behind
//! `params_of`), and the `TypeRepr` surface-spelling renderer (`Display`, the hover/debugger
//! form). A vocabulary entry any leg forgets or spells differently breaks the loop.
//!
//! Coverage is generated from [`BuiltinTy::all`], the funnel's own enumeration — every built-in
//! constructor under **every** surface spelling (canonical, bare, aliases, all eight generated
//! widths), each both at top level and in container-element position, where the declared-width
//! story differs on purpose: a *top-level* scalar `i32`/`f64` erases to `int`/`float` (a runtime
//! scalar value carries no width tag, so `params_of` must agree with `type_of`), while an
//! *element* `List<i32>` keeps its width (a physically distinct storage slot). The erasure is
//! upstream of rendering, so both forms round-trip.
//!
//! Deliberately **not** generated — surface-unwritable trees, since the grammar has no
//! parenthesized-type grouping: `?(A | B)` (an `Optional` of a `Union`; `?A | B` parses as
//! `(?A) | B`) and a function type as a union member (`(int) -> int | string` parses the union
//! into the return type). Neither shape can come out of the parser, so declared-type reflection
//! can never be asked to render one.

use noeta_ast::reflect::{TypeRepr, typeref_to_repr};
use noeta_ast::{BuiltinTy, Stmt, TypeRef};
use noeta_lexer::lex;
use noeta_parser::parse;
use noeta_span::{Source, SourceId};

/// Parse `spelling` in a parameter-annotation position and return the annotation's [`TypeRef`].
/// Panics (with the spelling) if it does not parse cleanly — a spelling the grammar rejects is a
/// round-trip failure in itself.
fn parse_annotation(spelling: &str) -> TypeRef {
    let src = format!("fn probe(x: {spelling}): void {{ return }}\n");
    let source = Source::new(SourceId::FIRST, "roundtrip.noe", &src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
        "spelling `{spelling}` does not lex/parse cleanly: {:?}{:?}",
        lexed.diagnostics,
        parsed.diagnostics
    );
    let Some(Stmt::Fn(decl)) = parsed.program.stmts.first() else {
        panic!("spelling `{spelling}`: probe fn did not parse as a fn");
    };
    decl.params[0]
        .ty
        .clone()
        .unwrap_or_else(|| panic!("spelling `{spelling}`: annotation missing"))
}

/// One round-trip: `spelling` → parse → repr → render → parse → repr, asserting the two reprs are
/// identical and the render is a fixed point. Returns the repr for callers that also pin its shape.
fn round_trip(spelling: &str) -> TypeRepr {
    let repr = typeref_to_repr(&parse_annotation(spelling));
    let rendered = repr.to_string();
    let reparsed = typeref_to_repr(&parse_annotation(&rendered));
    assert_eq!(
        reparsed, repr,
        "`{spelling}` → repr → `{rendered}` → repr drifted"
    );
    assert_eq!(
        reparsed.to_string(),
        rendered,
        "`{spelling}`: render of the re-parsed repr is not a fixed point"
    );
    repr
}

/// Every surface spelling of `ty` — [`BuiltinTy::spellings`] for the listed constructors, the
/// generated width name for the `IntN` family.
fn spellings_of(ty: BuiltinTy) -> Vec<String> {
    if let BuiltinTy::IntN { signed, bits } = ty {
        vec![BuiltinTy::int_width_name(signed, bits)]
    } else {
        ty.spellings().iter().map(|s| (*s).to_string()).collect()
    }
}

/// Sample generic arguments matching `ty`'s arity, for the applied form (`List<int>`,
/// `Map<string, i32>` — the second argument a width, so applied-position width retention is
/// exercised for the 2-arity constructors too).
fn applied(name: &str, arity: usize) -> Option<String> {
    match arity {
        0 => None,
        1 => Some(format!("{name}<i32>")),
        2 => Some(format!("{name}<string, i32>")),
        n => panic!("unexpected builtin arity {n}"),
    }
}

/// Every built-in constructor, under every spelling, round-trips — bare, applied (per arity), and
/// as a `List` element (where a declared width must *survive*, unlike at top level, where it must
/// *erase*). Driven by [`BuiltinTy::all`], so a new built-in is covered the moment the funnel
/// learns it.
#[test]
fn every_builtin_spelling_round_trips() {
    for ty in BuiltinTy::all() {
        for spelling in spellings_of(ty) {
            round_trip(&spelling);
            round_trip(&format!("List<{spelling}>"));
            if let Some(applied) = applied(&spelling, ty.arity()) {
                round_trip(&applied);
                round_trip(&format!("List<{applied}>"));
            }
        }
    }
}

/// The declared-width story, pinned exactly: top-level scalars erase (`params_of` must agree with
/// `type_of`, which can only see the erased runtime scalar), element positions reify (a `List<i32>`
/// element is a physically distinct slot). See `builtin_repr` and docs/Fixed-Width-Integers.md.
#[test]
fn width_erasure_is_positional() {
    assert_eq!(round_trip("i32"), TypeRepr::Int);
    assert_eq!(round_trip("u64"), TypeRepr::Int);
    assert_eq!(round_trip("f64"), TypeRepr::Float);
    assert_eq!(
        round_trip("f32"),
        TypeRepr::F32,
        "f32 is reified everywhere"
    );
    assert_eq!(
        round_trip("List<i32>"),
        TypeRepr::List(Box::new(TypeRepr::IntN {
            signed: true,
            bits: 32
        }))
    );
    assert_eq!(
        round_trip("List<f64>"),
        TypeRepr::List(Box::new(TypeRepr::F64))
    );
    // Depth does not restore erasure: an element stays width-carrying at every nesting level.
    assert_eq!(
        round_trip("Map<string, List<u16>>"),
        TypeRepr::Map(
            Box::new(TypeRepr::Str),
            Box::new(TypeRepr::List(Box::new(TypeRepr::IntN {
                signed: false,
                bits: 16
            })))
        )
    );
}

/// Composite shapes: nested generics, optionals, unions, tuples, function types, trait objects,
/// qualified nominals — each a writable annotation, each through the same loop.
#[test]
fn composite_spellings_round_trip() {
    for spelling in [
        // Nested generics, holes, and nominals with arguments.
        "Map<string, List<i32>>",
        "List<Map<u16, ?string>>",
        "Set<?u8>",
        "Result<int, string>",
        "Result<Codec, List<Codec>>",
        "Codec<int>",
        "geometry.vec.Vec2",
        "List<geometry.vec.Vec2>",
        "List", // bare canonical container: element is an inference hole (`dyn`)
        // Optionals, nested and over containers. A nested optional is written `? ?int` — `??`
        // lexes as the null-coalescing operator — and must render back that way.
        "? ?int",
        "?List<i32>",
        "List<? ?string>",
        // Unions (top level, as element, with an optional member: `?int | string` = `(?int) | string`).
        "int | string",
        "int | string | Codec",
        "List<int | string>",
        "?int | string",
        // Tuples: reflect as `dyn` by design (no runtime tuple tag), so the loop still closes.
        "(int, string)",
        "List<(int, string)>",
        // Function types, incl. nesting in and around containers and a union in parameter position.
        "() -> void",
        "(int) -> int",
        "(int, List<f32>) -> string",
        "(int) -> (int) -> int",
        "(int | string) -> ?int",
        "List<(int) -> int>",
        "Map<string, (int) -> int>",
        // Trait objects, bare and as an element.
        "dyn Store",
        "List<dyn Store>",
        "dyn app.io.Store",
    ] {
        round_trip(spelling);
    }
}

/// A tuple annotation reflects as the dynamic top — pinned so a future reified tuple repr must
/// visit this oracle (and the renderer) deliberately.
#[test]
fn tuple_reflects_as_dyn() {
    assert_eq!(round_trip("(int, string)"), TypeRepr::Dyn);
}
