//! A snapshot "gallery" of the type checker's *rendered* diagnostics. Like the M0 runtime
//! gallery in `noeta-eval`, this asserts the full rendered text (caret placement, code, help
//! line) of each checker diagnostic, not just the code enum — diagnostic quality is a product
//! feature. Color is disabled in the renderer, so output is deterministic.

use noeta_check::check;
use noeta_diagnostics::render;
use noeta_lexer::lex;
use noeta_parser::parse;
use noeta_span::{Source, SourceId};

fn render_checks(src: &str) -> String {
    let source = Source::new(SourceId::FIRST, "snippet.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
        "gallery snippet must lex/parse cleanly so only checker diagnostics show"
    );
    check(&parsed.program)
        .iter()
        .map(|d| render(&source, d))
        .collect()
}

#[test]
fn checker_diagnostic_gallery() {
    // This test is its own assembling driver (audit-6 F2): seed the std units first.
    noeta_stdlib::registry::default_seeded();
    // One representative program per checker diagnostic. E0013 has a gallery of its own below,
    // because what it says about a *nearly* correct name is the whole point of it.
    let cases = [
        ("E0007 arithmetic type mismatch", "echo 1 + true;"),
        (
            "E0011 non-exhaustive match",
            "enum E { A; B; C; }\necho match E.A { E.A => 1, E.B => 2 };",
        ),
        (
            "E0012 `?` on a non-fallible value",
            "fn f(): int { return 5?; }",
        ),
        (
            "E0014 `impl` of an unknown trait",
            "class W {\n  impl Frob {\n    pub fn frob(other: W): W { return other; }\n  }\n}",
        ),
        (
            "E0015 `impl` missing the trait's required method",
            "class M {\n  amount: int\n  impl Add {\n    pub fn plus(other: M): M { return other; }\n  }\n}",
        ),
    ];
    let mut out = String::new();
    for (label, src) in cases {
        out.push_str(&format!("================ {label} ================\n"));
        out.push_str(&render_checks(src));
        out.push('\n');
    }
    insta::assert_snapshot!(out);
}

/// **An unknown type names a real one badly, far more often than it names nothing.**
///
/// Two shapes, both ordinary. A bare name the file forgot to import (`x: Uuid`), and a dotted one
/// reaching through the wrong module — `Request` lives at `std.http.Request`, not under
/// `std.http.server`, so `server.Request` names nothing however sensible it looks.
///
/// The second half of each case is the diagnostic that used to follow and read as a riddle. An
/// unresolved annotation survives as a `Type::Named("server.Request")`, which displays by its last
/// segment exactly as the real `std.http.Request` does — so the mismatch rendered as
/// `expected `Request`, found `Request``. Both sides now qualify when, and only when, their short
/// forms collide.
#[test]
fn unknown_type_gallery() {
    noeta_stdlib::registry::default_seeded();
    let cases = [
        (
            "E0013 a bare name that was never imported",
            "fn label(id: Uuid): string { return \"x\"; }\necho label(\"u\");",
        ),
        (
            "E0013 a dotted name reaching through the wrong module",
            "use std.http.server\nfn handle(r: server.Request): string { return r.method(); }\necho handle(server.incoming(\"GET\", \"/a\"));",
        ),
        (
            "E0013 a name nothing resolves, which keeps the general help",
            "fn f(x: Nonesuch): int { return 1; }",
        ),
    ];
    let mut out = String::new();
    for (label, src) in cases {
        out.push_str(&format!("================ {label} ================\n"));
        out.push_str(&render_checks(src));
        out.push('\n');
    }
    insta::assert_snapshot!(out);
}

/// **The advice compiles.**
///
/// A help line that names an import is worth more than the general one only if following it
/// actually works — and "write `Request` with `use std.http.Request`" is a claim about two things
/// at once, the import path and the spelling to use afterwards. Getting either wrong sends the
/// reader somewhere worse than the generic help would have.
#[test]
fn the_import_a_diagnostic_suggests_resolves_the_error() {
    noeta_stdlib::registry::default_seeded();
    for taken in [
        "use std.id\nuse std.id.Uuid\nfn label(u: Uuid): string { return \"x\"; }\necho label(id.uuid());",
        "use std.http.server\nuse std.http.Request\nfn handle(r: Request): string { return r.method(); }\necho handle(server.incoming(\"GET\", \"/a\"));",
    ] {
        assert_eq!(
            render_checks(taken),
            "",
            "the suggested spelling must check clean:\n{taken}"
        );
    }
}

/// The `match`-arm gallery. E0066 is the first diagnostic in the catalog to carry **two** labels —
/// the arm that died and the pattern that killed it — so its rendered shape (both carets in one
/// snippet, in source order) is worth pinning rather than assumed. The bare-variant cases ride
/// along because they are what an author now hits *instead* of the retired E0067: a bare identifier
/// that resolved is a case test (so the only complaint left is the cases it does NOT cover, E0011),
/// and one that did not resolve is the plain catch-all it always was.
#[test]
fn match_arm_gallery() {
    noeta_stdlib::registry::default_seeded();
    let type_enum = "enum Type { String; Int; List(inner: string) }\n";
    let cases = [
        (
            "E0066 unreachable arm after a wildcard",
            "fn rank(n: int): string {\n    return match n {\n        _ => \"many\",\n        1 => \"one\",\n    }\n}".to_string(),
        ),
        (
            "E0066 unreachable arm after a bare `none` the scrutinee did not resolve",
            "fn show(d: dyn): string {\n    return match d {\n        none => \"empty\",\n        some(v) => \"${v}\",\n    }\n}".to_string(),
        ),
        (
            "E0011 a bare payload-free variant covers only its own case",
            format!("{type_enum}fn describe(t: Type): string {{\n    return match t {{\n        String => \"string\",\n    }}\n}}"),
        ),
        (
            "clean: every payload-free variant spelled bare, no `_` needed",
            format!("{type_enum}fn describe(t: Type): string {{\n    return match t {{\n        String => \"string\",\n        Int => \"int\",\n        List(i) => i,\n    }}\n}}"),
        ),
    ];
    let mut out = String::new();
    for (label, src) in cases {
        out.push_str(&format!("================ {label} ================\n"));
        out.push_str(&render_checks(&src));
        out.push('\n');
    }
    insta::assert_snapshot!(out);
}

/// The **forwarding-refusal** gallery: the three spellings that can reach the one E0058 raised when
/// a body has no forwarding slot to pass through. They are pinned by rendered text, not just by
/// code, because for a full arc this seam answered all three with a single message that was false
/// in every one of them — it claimed "call-site-typed forwarding is supported in top-level generic
/// functions only" (generic methods have forwarded since the generic-forwarding arc, and the
/// *inferred* case fires from inside a top-level generic `fn`), and its help advised the explicit
/// turbofish that two of the three spellings already write. A conformance case can pin the code and
/// the span; only a snapshot can pin that the sentence stays true.
#[test]
fn forwarding_refusal_gallery() {
    noeta_stdlib::registry::default_seeded();
    let cases = [
        (
            // The pre-pass registers a transitive slot from an EXPLICIT turbofish only, so a call
            // whose instantiation the arguments inferred leaves the caller with nothing to
            // forward. Here the turbofish help is the real fix, and it is the only spelling for
            // which that is so.
            "E0058 forwarding into a call whose instantiation was inferred",
            "fn inner<T>(x: T): string { return type_name::<T>(); }\n\
             fn outer<T>(x: T): string { return inner(x); }",
        ),
        (
            // The receiver is a compound expression, so naming the callee means typing it first —
            // which the syntactic pre-pass cannot do. Binding it to a local restores the bare-name
            // spelling (`tests/conformance/generic_methods/forward_receiver_bound_local.noe`
            // measures that it then works), which is what the help says.
            "E0058 forwarding through a compound receiver",
            "class Inner {\n    pub tag: string\n    pub fn label<T>(): string { return \"${self.tag}${type_name::<T>()}\"; }\n}\n\
             class Outer {\n    pub inner: Inner\n    fn label<T>(): string { return self.inner.label::<T>(); }\n}",
        ),
        (
            // The turbofish is already spelled, on a bare name the pre-pass does see — but the
            // callee's own slot is a composite this call's type argument does not name, and the
            // caller carries only what its own sites spell. No route to point at, so no help: a
            // help line that repeats what the source already does is worse than none.
            "E0058 forwarding a callee's composite slot through a member turbofish",
            "class Holder {\n    pub tag: string\n    fn deep<U>(x: U): string { return \"${self.tag}${type_name::<U>()}\"; }\n    pub fn mid<V>(v: List<V>): string { return self.deep::<List<V>>(v); }\n}\n\
             fn outer<T>(h: Holder, xs: List<T>): string { return h.mid::<T>(xs); }",
        ),
    ];
    let mut out = String::new();
    for (label, src) in cases {
        out.push_str(&format!("================ {label} ================\n"));
        out.push_str(&render_checks(src));
        out.push('\n');
    }
    insta::assert_snapshot!(out);
}
