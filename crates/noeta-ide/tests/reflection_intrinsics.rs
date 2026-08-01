//! **What holds [`REFLECTION_INTRINSICS`] to reality**, and what the editor does with it.
//!
//! [`noeta_builtins::REFLECTION_INTRINSICS`] is a table of thirteen signatures, and a table of
//! signatures is exactly the kind of thing that drifts: the parser owns which call *surfaces* exist,
//! the checker owns what each one's *result type* is, and neither reads the table. Both halves are
//! therefore asserted here, against the real parser and the real checker, so a divergence is a
//! failing test rather than a wrong tooltip.
//!
//! - [`the_result_types_are_the_checkers_own_answers`] hovers every intrinsic in a live buffer,
//!   through the same `DocumentStore` the editor uses, and compares the checker's inferred type with
//!   the table's. That is a measurement of the whole pipeline — parse, link, check, `expr_types` —
//!   not of a helper.
//! - [`the_call_surfaces_are_the_parsers_own`] drives the full (turbofish?, arity) grid for every
//!   intrinsic and asserts a snippet parses **exactly** when the table has a matching form. A
//!   surface added to (or removed from) the grammar without a matching table edit fails here.
//! - [`every_intrinsic_completes_and_signature_helps`] is the row's own deliverable: all thirteen ×
//!   both features, measured over live buffers.
//!
//! Why an assertion and not unification — why the checker does not simply *read* the table. Each
//! reflection arm computes its result while validating its operands, in the same pass and often from
//! the same resolved type (`attributes_of::<T>` reports `List<Attributed<T>>` only after deciding
//! `T` is an attribute; `from_bytes::<T>` reports `List<T>` from the resolved element type). Making
//! those arms read a string template would mean parsing surface spelling back into a `Type` in the
//! checker's hot path to gain nothing the arm did not already know. The table's job is to state
//! those answers where a *consumer* can reach them; this file's job is to make the statement false
//! out loud when it stops being true.
//!
//! Its own test binary rather than a `#[cfg(test)]` module because it drives `DocumentStore` over a
//! seeded registry, exactly as `tests/expansion.rs` does.
//!
//! [`REFLECTION_INTRINSICS`]: noeta_builtins::REFLECTION_INTRINSICS

use noeta_builtins::{REFLECTION_INTRINSICS, ReflectForm, ReflectionIntrinsic};
use noeta_ide::{DocumentStore, Encoding, Position};
use noeta_span::{Source, SourceId};

/// The declarations every probe buffer shares: one of each kind the turbofish surfaces require —
/// an `@attribute` record, a `@semantic` enum, a `@packed` struct, a plain struct, a plain enum.
const PREAMBLE: &str = "\
@attribute
struct Marker {
    tag: string = \"\"
}

@semantic
enum WebRole {
    Controller;
    Middleware;
}

@packed struct Vec2 { x: f32; y: f32 }

struct Point {
    x: int = 0
    y: int = 0
}

enum Color {
    Red;
    Green;
}

";

/// The concrete type each intrinsic's turbofish is given in the probe buffers, and therefore what
/// its form's type-parameter name stands for when the result is compared with the checker's answer.
///
/// Keyed by intrinsic rather than derived, because *which* declaration is legal differs per
/// intrinsic and the checker enforces it: `attributes_of` requires an `@attribute` record,
/// `roles_of` a `@semantic` enum, `from_bytes` a packable one, `variants_of` an enum. That is the
/// point of probing with real types — a probe that passed `Point` everywhere would be checking a
/// diagnostic path, not the answer.
fn type_argument(name: &str) -> &'static str {
    match name {
        "attributes_of" => "Marker",
        "roles_of" => "WebRole",
        "from_bytes" => "Vec2",
        "variants_of" => "Color",
        _ => "Point",
    }
}

/// A source expression writing `intrinsic` in `form`, with operands of the right types drawn from
/// the probe function's parameters.
fn call_expr(intrinsic: &ReflectionIntrinsic, form: &ReflectForm) -> String {
    let turbofish = match form.turbofish {
        Some(_) => format!("::<{}>", type_argument(intrinsic.name)),
        None => String::new(),
    };
    let args: Vec<&str> = form
        .params
        .iter()
        .map(|p| match p.ty {
            "string" => "target",
            "bytes" => "blob",
            "List<dyn>" => "args",
            _ => "v",
        })
        .collect();
    // The runtime-string surfaces of `field_specs_of`/`variants_of`/`construct` name a *type*, and
    // the probe's `target` parameter is as good a string as any — the checker is lenient about what
    // the name resolves to (an unknown name is a runtime empty answer), which is the contract.
    format!("{}{turbofish}({})", intrinsic.name, args.join(", "))
}

/// The result the table claims for `form`, with the form's type-parameter name replaced by the
/// concrete type the probe passes — `List<Attributed<T>>` against `Marker` is
/// `List<Attributed<Marker>>`.
fn expected_result(intrinsic: &ReflectionIntrinsic, form: &ReflectForm) -> String {
    match form.turbofish {
        Some(param) => intrinsic
            .result
            .split_inclusive(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .map(|chunk| {
                let (word, tail) = match chunk
                    .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                {
                    Some(i) => chunk.split_at(i),
                    None => (chunk, ""),
                };
                if word == param {
                    format!("{}{tail}", type_argument(intrinsic.name))
                } else {
                    chunk.to_string()
                }
            })
            .collect(),
        None => intrinsic.result.to_string(),
    }
}

/// A probe buffer: the shared declarations, then a function whose parameters supply an operand of
/// every type an intrinsic can take, with `expr` bound in its body.
fn probe_source(expr: &str) -> String {
    format!(
        "{PREAMBLE}fn probe(v: dyn, blob: bytes, target: string, args: List<dyn>) {{\n    \
         probed = {expr}\n}}\n"
    )
}

/// The `(line, character)` of `needle`'s first occurrence in `text` — the buffers are ASCII, so the
/// character index is the byte index within the line.
fn position_of(text: &str, needle: &str) -> Position {
    let offset = text.find(needle).unwrap_or_else(|| panic!("`{needle}` not in the buffer"));
    let line = text[..offset].matches('\n').count();
    let line_start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    Position {
        line: line as u32,
        character: (offset - line_start) as u32,
    }
}

fn store_with(uri: &str, text: &str) -> DocumentStore {
    noeta_stdlib::registry::default_seeded();
    let mut store = DocumentStore::default();
    store.open(uri, text.to_string());
    store
}

/// **The table's result types are the checker's own answers.**
///
/// For every intrinsic and every one of its call forms: write the call in a live buffer, hover the
/// intrinsic, and compare the type the checker inferred with the type the table claims. This is the
/// tie that makes the table safe to publish through completion and signature help — those two show
/// the table, and this is the reason the table is not a fourteenth restatement waiting to rot.
///
/// The measured answers are the ones the prior audit reported from hover, which is the same index
/// (`Checked::expr_types`) this reads: `type_of`→`Type`, `construct`→`Result<dyn, string>`,
/// `params_of`→`List<ParamInfo>`, and so on for all thirteen.
#[test]
fn the_result_types_are_the_checkers_own_answers() {
    for (i, intrinsic) in REFLECTION_INTRINSICS.iter().enumerate() {
        for (j, form) in intrinsic.forms.iter().enumerate() {
            let expr = call_expr(intrinsic, form);
            let source = probe_source(&expr);
            let uri = format!("file:///probe_{i}_{j}.noe");
            let store = store_with(&uri, &source);
            let position = position_of(&source, &expr);
            let (repr, _note, _range) = store
                .hover_type(&uri, position, Encoding::Utf8)
                .unwrap_or_else(|| panic!("no type for `{expr}` — the probe did not check"));
            assert_eq!(
                repr.to_string(),
                expected_result(intrinsic, form),
                "`{expr}`: the checker and REFLECTION_INTRINSICS disagree about the result type"
            );
        }
    }
}

/// **The table's call surfaces are the parser's own.**
///
/// The full grid, for every intrinsic: with and without a turbofish, at every arity from zero to
/// four, a snippet must parse exactly when the table has a form of that shape. Every cell is
/// checked, so the test fails in both directions — a surface the grammar accepts and the table does
/// not (the editor would never offer it) and a surface the table claims and the grammar rejects (the
/// editor would offer a parse error).
///
/// A parse-level grid is the right granularity: the surfaces are literally productions, one per
/// intrinsic, and what a consumer of the table needs to know is which shapes a user may write.
#[test]
fn the_call_surfaces_are_the_parsers_own() {
    for intrinsic in REFLECTION_INTRINSICS {
        for turbofish in [false, true] {
            for arity in 0..=4usize {
                let claimed = intrinsic.forms.iter().any(|f| {
                    f.turbofish.is_some() == turbofish && f.params.len() == arity
                });
                let head = match turbofish {
                    true => format!("{}::<{}>", intrinsic.name, type_argument(intrinsic.name)),
                    false => intrinsic.name.to_string(),
                };
                let args = vec!["v"; arity].join(", ");
                let expr = format!("{head}({args})");
                let parses = parses_as_written(&expr);
                assert_eq!(
                    parses, claimed,
                    "`{expr}`: the grammar {} it, REFLECTION_INTRINSICS {} a form of that shape",
                    if parses { "accepts" } else { "rejects" },
                    if claimed { "has" } else { "has no" },
                );
            }
        }
    }
}

/// Whether `expr` parses as *itself* — cleanly, and to the reflection node the intrinsic names.
///
/// Both halves matter. The parser recovers, so "no diagnostics" alone is not enough; and a snippet
/// that recovered into some other node (a call of an ordinary identifier, say) would be a false
/// positive for a surface that does not exist. So the statement is: no parse errors, and the bound
/// value is the intrinsic's own AST node.
fn parses_as_written(expr: &str) -> bool {
    let text = format!("probed = {expr}\n");
    let source = Source::new(SourceId::FIRST, "grid.noe", &text);
    let lexed = noeta_lexer::lex(&source);
    if !lexed.diagnostics.is_empty() {
        return false;
    }
    let parsed = noeta_parser::parse(&source, &lexed.tokens);
    if !parsed.diagnostics.is_empty() {
        return false;
    }
    let Some(noeta_ast::Stmt::Binding { value, .. }) = parsed.program.stmts.first() else {
        return false;
    };
    matches!(
        value,
        noeta_ast::Expr::AttributesOf { .. }
            | noeta_ast::Expr::TypeOf { .. }
            | noeta_ast::Expr::TypeName { .. }
            | noeta_ast::Expr::FieldsOf { .. }
            | noeta_ast::Expr::TraitsOf { .. }
            | noeta_ast::Expr::FromBytes { .. }
            | noeta_ast::Expr::RolesOf { .. }
            | noeta_ast::Expr::ParamsOf { .. }
            | noeta_ast::Expr::ReturnsOf { .. }
            | noeta_ast::Expr::Invoke { .. }
            | noeta_ast::Expr::FieldSpecsOf { .. }
            | noeta_ast::Expr::VariantsOf { .. }
            | noeta_ast::Expr::Construct { .. }
    ) && parsed.program.stmts.len() == 1
}

/// **All thirteen, both features, over live buffers** — the row's deliverable, measured rather than
/// assumed.
///
/// Completion must offer every intrinsic as a function with its signature as the detail; signature
/// help must answer for every intrinsic, in every form, with that form's parameters. Both were zero
/// of thirteen before: completion filtered the reflection role out of the keyword offer and had no
/// other, and signature help resolved through `FnDecl`s that a reserved word does not have.
#[test]
fn every_intrinsic_completes_and_signature_helps() {
    // Completion: one buffer, mid-identifier, as a user typing a name would be.
    let source = probe_source("1");
    let uri = "file:///offer.noe";
    let store = store_with(uri, &source);
    let offered = store
        .completions(uri, position_of(&source, "1\n}"), Encoding::Utf8)
        .expect("the document is open");
    for intrinsic in REFLECTION_INTRINSICS {
        let candidate = offered
            .iter()
            .find(|c| c.label == intrinsic.name)
            .unwrap_or_else(|| panic!("`{}` is not offered in completion", intrinsic.name));
        assert_eq!(
            candidate.kind,
            noeta_ide::completion::CandidateKind::Function,
            "`{}` is offered, but not as a function",
            intrinsic.name
        );
        assert_eq!(
            candidate.detail.as_deref(),
            Some(intrinsic.signature().as_str())
        );
    }

    // Signature help: one buffer per form, cursor just inside the open paren.
    for (i, intrinsic) in REFLECTION_INTRINSICS.iter().enumerate() {
        for (j, form) in intrinsic.forms.iter().enumerate() {
            let expr = call_expr(intrinsic, form);
            let source = probe_source(&expr);
            let uri = format!("file:///sig_{i}_{j}.noe");
            let store = store_with(&uri, &source);
            // Inside the *last* argument — the position that distinguishes an intrinsic's arities
            // (on `invoke`'s first argument, the two-operand form is the right answer even in a
            // three-operand call, because that is all the user has written so far).
            let mut position = position_of(&source, &expr);
            position.character += expr.len() as u32 - 1;
            let data = store
                .signature_help(&uri, position, Encoding::Utf8)
                .unwrap_or_else(|| panic!("no signature help inside `{expr}`"));
            assert!(
                data.label.starts_with(&intrinsic.render_form(form)),
                "`{expr}` shows `{}`, not this form",
                data.label
            );
            let rendered: Vec<String> = form
                .params
                .iter()
                .map(noeta_builtins::ReflectParam::render)
                .collect();
            assert_eq!(data.parameters, rendered, "parameters shown for `{expr}`");
            assert_eq!(
                data.active_param,
                form.params.len().saturating_sub(1),
                "active argument in `{expr}`"
            );
        }
    }
}

/// The active argument selects among an intrinsic's arities — the reason `invoke`'s two forms and
/// `construct`'s two surfaces can both be described from one table.
#[test]
fn signature_help_follows_the_argument_being_typed() {
    let source = probe_source("invoke(target, args)");
    let uri = "file:///invoke.noe";
    let store = store_with(uri, &source);
    let sig = |offset_into_args: u32| {
        let mut position = position_of(&source, "invoke(");
        position.character += "invoke(".len() as u32 + offset_into_args;
        store
            .signature_help(uri, position, Encoding::Utf8)
            .expect("inside the call")
    };
    // On the first argument: the two-operand form, `invoke(name, args)`.
    let first = sig(0);
    assert_eq!(first.parameters, ["name: string", "args: List<dyn>"]);
    assert_eq!(first.active_param, 0);
    // On the second: still the two-operand form, second parameter active.
    let second = sig("target, ".len() as u32);
    assert_eq!(second.parameters, ["name: string", "args: List<dyn>"]);
    assert_eq!(second.active_param, 1);
}

/// A turbofish head is a call head. `construct::<Point>(` gets signature help for the turbofish
/// form, not for the two-operand string form and not for nothing at all — the `::<…>` sits between
/// the callee and the `(`, and the token scan steps back over it.
#[test]
fn a_turbofish_head_still_resolves_the_callee() {
    let source = probe_source("construct::<Point>(args)");
    let uri = "file:///turbofish.noe";
    let store = store_with(uri, &source);
    let mut position = position_of(&source, "construct::<Point>(");
    position.character += "construct::<Point>(".len() as u32;
    let data = store
        .signature_help(uri, position, Encoding::Utf8)
        .expect("inside the call");
    assert!(
        data.label
            .starts_with("construct::<T>(fields: List<dyn>): Result<dyn, string>"),
        "got `{}`",
        data.label
    );
    assert_eq!(data.parameters, ["fields: List<dyn>"]);
}
