//! **The AST field-coverage gate**: every field of every node the namespace qualifier walks is
//! classified here, and the classification is checked by running the real walk over a probe that
//! carries a sentinel in each field position.
//!
//! ## The bug class
//!
//! Three times in one week a name or type reference shipped that the linker's qualification pass
//! never rewrote, producing a **silent wrong answer** under a `namespace`:
//!
//! 1. `field_specs_of::<T>()` / `construct::<T>(…)` — the parser flattened the turbofish into an
//!    `Expr::Str` *before* qualification ran. `Expr::Str` is a leaf in the qualifier, so the query
//!    asked for the unqualified key and returned an EMPTY schema, with no diagnostic.
//! 2. [`Expr::TypedCall`](noeta_ast::Expr::TypedCall)'s **callee** was never visited, so
//!    `gen::<T>(x)` under a `namespace` was `E0005` while `gen(x)` resolved — every generic
//!    function unusable with an explicit turbofish.
//! 3. Reflection over an erased type parameter answered with silence.
//!
//! Every one of them is the same shape, and the tree made all three easy to write:
//!
//! * **`..` defeats field-level exhaustiveness.** Rust forces a match to mention every *variant*;
//!   it does not force an arm to mention every *field*. `Expr::TypedCall { type_args, args, .. }`
//!   compiled fine with the callee unvisited. That half is now closed structurally — `qualify.rs`
//!   binds every field by name, so adding one is a compile error there.
//! * **The leaf group is an attractive nuisance.** `Expr::Str {..} | Expr::Int {..} | … => {}` is
//!   the cheapest way to satisfy the compiler when a variant is added, and it is silently wrong for
//!   anything carrying a name. Bug 1 landed exactly there.
//! * **There is no single walk.** ~16 files match on `Expr` independently, and `qualify.rs` alone
//!   has four walks that must agree about which fields carry names.
//!
//! Banning `..` proves a field is *named*. It cannot prove the field is *visited* — `field: _` and
//! `field` both compile. This file proves the second half. Patching each instance as it is
//! discovered guarantees a fourth, so the property is made structural rather than remembered.
//!
//! ## What the gate does
//!
//! [`TABLE`] plus a set of **type-derived defaults** classify every field of every node the
//! qualifier walks as one of:
//!
//! - [`Verdict::Name`] — an identifier the linker must qualify (a callee, a type name, a trait
//!   name, an attribute name…). Must be **reached**.
//! - [`Verdict::Type`] — a [`TypeRef`]. Must be **reached**.
//! - [`Verdict::Sub`] — a sub-expression, sub-statement, sub-pattern, or a carrier of them. Must be
//!   **reached**.
//! - [`Verdict::Unqualified`] — a name-shaped payload deliberately left alone: a local binding, a
//!   member name, a string literal's text, a globally-namespaced tier name. Must **not** be
//!   reached, and must say why. This is the verdict that makes "no rewrite" a *decision*: a string
//!   literal's value being reached is precisely bug 1's shape, and the gate now fails on it.
//! - [`Verdict::Inert`] — a span, a number, a flag, an operator. Nothing to probe; the reason is
//!   recorded so the exemption was stated rather than omitted.
//!
//! The **derivation** carries most of the weight, and is the part worth keeping: a field whose
//! declared type mentions `TypeRef` must be qualified, a field whose type mentions another AST node
//! must be recursed into, and a field of `Span`/`bool`/`i64`/… is inert — no human judgement, read
//! straight off the declaration.
//!
//! Since the [`Name`](noeta_ast::Name) newtype landed, **the qualifiable half is derived too**. A
//! `String` in this AST used to be a qualifiable declaration name *or* a local binding *or* a member
//! name *or* literal text, so each one needed a hand-written [`TABLE`] row — and a row is exactly
//! where a wrong judgement hides. Splitting the primitive moved that judgement to the declaration:
//! whoever adds a field says which of the two it is by choosing its type, `derived_verdict` answers
//! [`Verdict::Name`] for a `Name` with nobody's opinion involved, and a new `Name` field the walk
//! misses fails this gate with no row written at all.
//!
//! What still needs a human is the residue: a `String`, which the walk must not touch — but which
//! must not be touched for *different reasons* (a local binding, a member name, a globally-scoped
//! tier name, literal text), and the reason is what [`Verdict::Unqualified`] records. Roughly 390
//! fields; a few dozen judgements, all of them about names that stay put.
//!
//! [`the_declared_type_decides_which_names_qualify`] pins the correspondence in both directions, so
//! neither half can drift: a row may add history to a `Name` field but never overrule its type, and
//! a `String` classified `Name` has to appear on a short, named list of positions the walk *reaches*
//! without rewriting (there is exactly one: a member chain's segments, visited as one dotted
//! candidate).
//!
//! ## How the check is run
//!
//! Not by reading source text. For each node type the gate builds a **probe**: a real AST value
//! with a distinct sentinel name in every classified position, wrapped so the walk reaches it. It
//! then runs [`referenced_names`](super::referenced_names) — the *collecting* client of the very
//! walk [`qualify_stmt`](super::qualify_stmt) rewrites through, so the two cannot disagree — and
//! compares the sentinels that came back against the table.
//!
//! That makes the check semantic rather than textual: it fails if the walk stops recursing, if an
//! arm is deleted, if a field is bound `field: _`, or if a rewrite is added where the table says
//! there must be none. Reintroducing bug 2 (dropping `Expr::TypedCall`'s `visit(name, …)`) turns
//! this file red with `Expr::TypedCall.name` named in the failure.
//!
//! ## What it deliberately does not do
//!
//! It does not check the *other* fifteen files that match on `Expr`. Their walks answer different
//! questions (lowering, formatting, hover), and a single classification cannot serve them. What
//! reaches those files instead is the type: a name they must not confuse with a runtime string is
//! a [`Name`](noeta_ast::Name) there too, and the compiler rejects the confusion without this gate
//! having to know anything about their walks.

use noeta_ast::Name as AstName;
use noeta_ast::{
    AssocTypeDecl, AttrArg, AttrValue, Attribute, CallArg, ClassDecl, ClosureBody, Decorators,
    DeriveSpec, EnumDecl, Expr, FieldDecl, FieldInit, FnDecl, ForPattern, ForeignDirective,
    ImplBlock, ImplDecl, MatchArm, MemberBinding, MethodDirective, ObjectLit, Param, Pattern,
    ReflectKind, ReflectOperand, RoleTag, Stmt, StrPart, StructDecl, TierDecl, TraitBound,
    TraitDecl, TraitMethod, TypeOperand, TypeParam, TypeRef, UnaryOp, UseName,
};
use noeta_span::{SourceId, Span};

use super::referenced_names;

/// What the qualifier must do with one field.
enum Verdict {
    /// An identifier naming a declaration the linker qualifies. Must be reached.
    Name(&'static str),
    /// A type reference. Must be reached.
    Type(&'static str),
    /// A sub-node the walk must recurse into. Must be reached.
    Sub(&'static str),
    /// A name-shaped payload that must be left alone. Must **not** be reached; the string says why.
    Unqualified(&'static str),
    /// Nothing that could carry a name. Nothing to probe; the string says why.
    Inert(&'static str),
}

impl Verdict {
    /// Whether the walk must reach this field's sentinel.
    fn must_reach(&self) -> Option<bool> {
        match self {
            Verdict::Name(_) | Verdict::Type(_) | Verdict::Sub(_) => Some(true),
            Verdict::Unqualified(_) => Some(false),
            Verdict::Inert(_) => None,
        }
    }

    fn why(&self) -> &'static str {
        match self {
            Verdict::Name(w)
            | Verdict::Type(w)
            | Verdict::Sub(w)
            | Verdict::Unqualified(w)
            | Verdict::Inert(w) => w,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Verdict::Name(_) => "Name",
            Verdict::Type(_) => "Type",
            Verdict::Sub(_) => "Sub",
            Verdict::Unqualified(_) => "Unqualified",
            Verdict::Inert(_) => "Inert",
        }
    }
}

/// One classified field. `0` is the node (`"Expr::TypedCall"` or `"FnDecl"`), `1` the field
/// (`"0"` for a tuple variant's payload).
struct Row(&'static str, &'static str, Verdict);

use Verdict::{Inert, Name, Sub, Type, Unqualified};

/// The **judgement** rows: every field whose declared type does not decide the answer, plus the
/// type-shaped fields the walk deliberately does not enter. Everything else is derived from the
/// declaration by [`derived_verdict`], and a field that is neither derivable nor listed here fails
/// [`every_ast_field_is_classified`].
const TABLE: &[Row] = &[
    // ---- Stmt -----------------------------------------------------------------------------
    Row(
        "Stmt::Binding",
        "name",
        Unqualified(
            "a value binding, not a declaration: `x = …` under a `namespace` is still `x` to a \
             reader of that module, and the qualifier leaves it alone. The walk does *visit* it, as \
             `NameKind::Binder` — a kind this rewriter and the reference collector both refuse — so \
             that the one pass with a different question can ask it: `qualify_module_bindings` \
             gives a MERGED module's global its qualified identity (`conn` → `App.Store.conn`), \
             which is what keeps it from colliding with the consumer's own `conn`",
        ),
    ),
    Row(
        "Stmt::Destructure",
        "targets",
        Unqualified("value bindings, like `Stmt::Binding::name`"),
    ),
    Row(
        "Stmt::Namespace",
        "path",
        Unqualified(
            "the module's OWN namespace — the input the linker builds the rewrite map FROM, not a \
             reference to rewrite",
        ),
    ),
    Row(
        "Stmt::Use",
        "path",
        Unqualified(
            "an import path is resolved against the loaded modules by the linker; rewriting it \
             here would resolve the import against itself",
        ),
    ),
    Row(
        "Stmt::Use",
        "names",
        Unqualified("the imported leaf names, resolved with the path above"),
    ),
    Row(
        "Stmt::For",
        "pattern",
        Unqualified("a `for` binder introduces value names, never type references"),
    ),
    Row(
        "Stmt::TierBlock",
        "tier",
        Unqualified(
            "a tier name lives in a GLOBAL, package-spanning name-space: a consumer writes the \
             short name a `@tier` runner declared, so qualifying it would make every declared tier \
             unreachable",
        ),
    ),
    Row(
        "Stmt::TierBlock",
        "doc_text",
        Unqualified("a text tier's verbatim body — foreign-language text, not Noeta"),
    ),
    Row("Stmt::TierBlock", "attached", Inert("a parser flag")),
    // ---- Expr -----------------------------------------------------------------------------
    Row(
        "Expr::Str",
        "value",
        Unqualified(
            "a string literal's text is DATA. This is bug 1's exact position: the parser flattened \
             `field_specs_of::<T>()`'s turbofish to an `Expr::Str`, the qualifier treated it as a \
             leaf, and the query silently asked for the unqualified key. Routing a type through \
             here again must fail this gate, not ship",
        ),
    ),
    Row("Expr::Unary", "op", Inert("an operator token")),
    Row("Expr::Binary", "op", Inert("an operator token")),
    Row(
        "Expr::Ident",
        "name",
        Name(
            "a bare identifier may name a type used as a value (`User.new()`'s base) or a \
              now-qualified top-level fn",
        ),
    ),
    Row(
        "Expr::Member",
        "name",
        Name(
            "a member chain may spell a qualified reference — the collapse visits the whole \
              dotted prefix as one candidate",
        ),
    ),
    Row(
        "Expr::Reflect",
        "which",
        Inert(
            "which reflection query this is — a fieldless enum, so there is no name or type inside \
             it to rewrite. What the query is asked ABOUT lives in `operand`, and that is the field \
             qualification has an opinion about",
        ),
    ),
    Row("Expr::Range", "inclusive", Inert("a flag")),
    Row("Expr::Spawn", "isolate", Inert("a flag")),
    Row(
        "Expr::TypedModuleCall",
        "func",
        Unqualified("a native module function, resolved against the module the receiver names"),
    ),
    Row(
        "Expr::TypedCall",
        "name",
        Name(
            "bug 2: the callee of `f::<T>(args)`. Held inline rather than as an `Expr::Ident` \
              sub-expression, so `..` ate it and every generic function was unusable with an \
              explicit turbofish under a `namespace`",
        ),
    ),
    Row(
        "Expr::TypedMethodCall",
        "name",
        Unqualified("a method name resolves against the receiver's type, never the module map"),
    ),
    Row(
        "Expr::FieldSet",
        "field",
        Unqualified("a member name on the receiver's type"),
    ),
    Row(
        "Expr::TierExpr",
        "tier",
        Unqualified("a tier name — see `Stmt::TierBlock::tier`"),
    ),
    Row(
        "Expr::TierExpr",
        "statics",
        Unqualified("the verbatim foreign-language segments between the `${…}` holes"),
    ),
    Row(
        "Expr::NativeFnRef",
        "module",
        Unqualified("compiler-synthesized AFTER qualification — already canonical"),
    ),
    Row(
        "Expr::NativeFnRef",
        "func",
        Unqualified("compiler-synthesized after qualification — already canonical"),
    ),
    // ---- Pattern --------------------------------------------------------------------------
    Row(
        "Pattern::Binding",
        "name",
        Unqualified("a pattern binding introduces a value name"),
    ),
    Row("Pattern::Str", "value", Unqualified("a literal's text")),
    Row(
        "Pattern::Variant",
        "type_name",
        Name(
            "`Type.Variant`'s enum — a real type reference, and the head a dotted miss is \
              diagnosed at",
        ),
    ),
    Row(
        "Pattern::Variant",
        "variant",
        Unqualified(
            "resolved through the (now-qualified) enum, or against the prelude for a bare \
             `Ok(x)`/`some(x)`",
        ),
    ),
    // ---- TypeRef --------------------------------------------------------------------------
    Row(
        "TypeRef::AssocProjection",
        "name",
        Unqualified(
            "`Self::Name` resolves per-impl at the checker, from the impl's binding — never \
             through the module map",
        ),
    ),
    // ---- StrPart --------------------------------------------------------------------------
    Row(
        "StrPart::Literal",
        "0",
        Unqualified("the literal text between an interpolated string's holes"),
    ),
    // ---- AttrValue ------------------------------------------------------------------------
    Row("AttrValue::Str", "0", Unqualified("a literal's text")),
    Row("AttrValue::Int", "0", Inert("a literal")),
    Row("AttrValue::Float", "0", Inert("a literal")),
    Row("AttrValue::Bool", "0", Inert("a literal")),
    Row(
        "AttrValue::Enum",
        "enum_name",
        Name("an enum-valued attribute argument names a real type"),
    ),
    Row(
        "AttrValue::Enum",
        "variant",
        Unqualified("resolved through the (now-qualified) enum"),
    ),
    Row(
        "AttrValue::Map",
        "0",
        Sub(
            "a map literal's VALUES are recursed into; its keys are string literals, since a runtime \
             map is string-keyed",
        ),
    ),
    Row(
        "AttrValue::Struct",
        "type_name",
        Name("a struct-valued attribute argument names a real type"),
    ),
    Row(
        "AttrValue::Struct",
        "fields",
        Sub(
            "a struct literal's field VALUES are recursed into; the field names belong to the \
             (now-qualified) type",
        ),
    ),
    Row(
        "AttrValue::TypeRef",
        "name",
        Name("a type name used as a value (`@derive(Serialize<Json>)`'s `Serialize`)"),
    ),
    // ---- carriers -------------------------------------------------------------------------
    Row(
        "CallArg",
        "name",
        Unqualified("an argument's parameter LABEL"),
    ),
    Row("CallArg", "value", Sub("the argument expression")),
    Row("AttrArg", "name", Unqualified("an argument's field LABEL")),
    Row("AttrArg", "value", Sub("the argument's literal value tree")),
    Row(
        "ObjectLit",
        "type_name",
        Name(
            "the literal's nominal head; `None` for the target-typed `.{ … }`, whose name comes \
              from the already-qualified expected type",
        ),
    ),
    Row(
        "FieldInit",
        "name",
        Unqualified("a field name on the literal's type"),
    ),
    // ---- declarations ---------------------------------------------------------------------
    Row(
        "FnDecl",
        "name",
        Name(
            "a TOP-LEVEL function's name qualifies like a type's. A method's does not — methods \
              resolve through their type — so the visit lives on the `Stmt::Fn` arm rather than in \
              the `q_fn` shared with methods, and this row is probed through `Stmt::Fn`",
        ),
    ),
    Row("FnDecl", "is_public", Inert("a visibility flag")),
    Row("FnDecl", "is_dev_tier", Inert("a tier-provenance flag")),
    Row("FnDecl", "is_async", Inert("a flag")),
    Row("FnDecl", "is_static", Inert("a flag")),
    Row(
        "FnDecl",
        "captures",
        Unqualified("`use (a, b)` names value bindings at the declaration site"),
    ),
    Row(
        "TierDecl",
        "name",
        Unqualified("the tier's global, consumer-written identity — see `Stmt::TierBlock::tier`"),
    ),
    Row(
        "TierDecl",
        "config",
        Name(
            "`@tier(name, config: T)` names the knob ATTRIBUTE STRUCT — a real type in this \
              module",
        ),
    ),
    Row(
        "TierDecl",
        "text",
        Unqualified("a body LANGUAGE ID (`text: \"markdown\"`), for editor injection"),
    ),
    Row(
        "TierDecl",
        "expr",
        Name(
            "`expr: Query` names the block-value type the handler must return (E0051 compares \
              them, so they have to qualify in lockstep)",
        ),
    ),
    Row("Param", "name", Unqualified("a parameter binding")),
    Row(
        "Param",
        "positional",
        Inert("a flag: the payload wrote no name"),
    ),
    Row(
        "FieldDecl",
        "name",
        Unqualified("a field name on its own type"),
    ),
    Row("FieldDecl", "mut_field", Inert("a mutability flag")),
    Row("FieldDecl", "is_public", Inert("a visibility flag")),
    Row("StructDecl", "is_public", Inert("a visibility flag")),
    Row("ClassDecl", "is_public", Inert("a visibility flag")),
    Row("EnumDecl", "is_public", Inert("a visibility flag")),
    Row(
        "VariantDecl",
        "name",
        Unqualified("a variant is reached through its (now-qualified) enum"),
    ),
    Row("TraitDecl", "is_public", Inert("a visibility flag")),
    Row("TraitMethod", "has_default", Inert("a flag")),
    Row(
        "AssocTypeDecl",
        "name",
        Unqualified("an associated type's name is resolved per-impl against its trait"),
    ),
    Row(
        "ImplDecl",
        "trait_name",
        Name("qualifies iff it is a user trait — a built-in is absent from the module map"),
    ),
    Row(
        "ImplDecl",
        "assoc_bindings",
        Type(
            "`type Item = Concrete;` — the CONCRETE half is an ordinary type reference. The \
              binding's own name is the trait's associated-type name, resolved per-impl",
        ),
    ),
    Row(
        "ImplBlock",
        "trait_name",
        Name("qualifies iff it is a user trait — a built-in is absent from the module map"),
    ),
    Row(
        "ImplBlock",
        "assoc_bindings",
        Type("as `ImplDecl::assoc_bindings`"),
    ),
    Row(
        "TypeParam",
        "name",
        Unqualified("a generic parameter is scoped to its declaration, not to a module"),
    ),
    Row(
        "Decorators",
        "attribute",
        Unqualified(
            "the `@attribute(Method, Function, …)` placement kinds — a closed built-in set",
        ),
    ),
    Row("Decorators", "semantic", Inert("a marker directive's span")),
    Row(
        "Decorators",
        "packed",
        Inert("a layout marker: a span and a row/column enum"),
    ),
    Row(
        "Decorators",
        "validated",
        Inert("a marker directive's span"),
    ),
    Row(
        "Decorators",
        "foreign",
        Unqualified(
            "an EXTENSION-declared `@`-directive. Its arguments reach the hook as source spelling \
             (`AttrValue::as_directive_arg`, whose contract is \"the path the author wrote\"), and \
             nothing in the compiler resolves one as a type. Rewriting them would hand a hook a \
             name that appears nowhere in the source; a hook that needs a RESOLVED type wants a \
             new declared argument kind, not a silent rewrite of every hook's strings",
        ),
    ),
    Row(
        "DeriveSpec",
        "bindings",
        Unqualified(
            "`value: amount` bridges the trait's required MEMBER to a member of the \
                     deriving type — both member names, neither a type",
        ),
    ),
    Row(
        "DeriveSpec",
        "via",
        Unqualified("`via: amount` delegates through a FIELD of the deriving type"),
    ),
    Row(
        "Attribute",
        "name",
        Name("a `#[Attr(…)]` names an `@attribute` struct"),
    ),
    Row(
        "RoleTag",
        "enum_name",
        Name(
            "`@role(Enum.Variant)` names a `@semantic` enum, which the checker looks up by its \
              QUALIFIED name. Nothing visited it, and the grammar takes a bare `Enum.Variant`, so \
              no spelling reached an imported role enum at all",
        ),
    ),
    Row(
        "RoleTag",
        "variant",
        Unqualified("resolved through the (now-qualified) enum"),
    ),
    Row(
        "MethodDirective",
        "name",
        Unqualified("a tier name — see `Stmt::TierBlock::tier`"),
    ),
    Row(
        "MethodDirective",
        "doc_text",
        Unqualified("a text tier's verbatim body"),
    ),
];

// -------------------------------------------------------------------------------------------
// The type-derived half.
// -------------------------------------------------------------------------------------------

/// Field types that carry nothing nameable.
const SCALARS: &[&str] = &[
    "Span", "bool", "i64", "u64", "u32", "u8", "f32", "f64", "usize",
];

/// The AST node types a field may hold: mentioning one means the walk must recurse into the field.
/// A type NOT listed here and not a scalar is one the gate refuses to guess about — it must have a
/// [`TABLE`] row, which is how `Vec<UseName>`, `Option<PackedDirective>` and `Vec<ForeignDirective>`
/// come to state their exemptions out loud.
const NODES: &[&str] = &[
    "Expr",
    "Stmt",
    "Pattern",
    "ClosureBody",
    "StrPart",
    "TypeOperand",
    "ReflectOperand",
    "MatchArm",
    "ObjectLit",
    "FieldInit",
    "CallArg",
    "AttrArg",
    "AttrValue",
    "FnDecl",
    "Param",
    "FieldDecl",
    "VariantDecl",
    "ImplBlock",
    "TypeParam",
    "TraitBound",
    "Decorators",
    "DeriveSpec",
    "Attribute",
    "RoleTag",
    "TraitMethod",
    "AssocTypeDecl",
    "TierDecl",
    "MethodDirective",
    "StructDecl",
    "ClassDecl",
    "EnumDecl",
    "ImplDecl",
    "TraitDecl",
];

/// The verdict a field's DECLARED TYPE settles on its own, or `None` when only a human can say.
///
/// This is the half worth keeping and the half that scales: `Option<TypeRef>` must be qualified and
/// `Vec<Stmt>` must be recursed into whatever the field is called, so three quarters of the fields
/// need no judgement at all. The residue — anything mentioning `String` — is genuinely ambiguous (a
/// declaration name? a local? a member name? literal text?) and is what [`TABLE`] is for; it is
/// never derived, so a future `Vec<(String, Span)>` cannot slip through as "inert, it has a Span in
/// it".
fn derived_verdict(field_ty: &str) -> Option<Verdict> {
    let mentions = |t: &str| {
        field_ty
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .any(|w| w == t)
    };
    // `Name` FIRST, and before `String`: a [`noeta_ast::Name`] *is* the classification. The field
    // types this gate reads used to be ambiguous — a `String` is a qualifiable declaration name or
    // a local binding or a member name or literal text, and only a human knew which — so each one
    // needed a hand-written `TABLE` row, and a row is exactly where a wrong judgement can hide.
    // Splitting the primitive moved that judgement to the declaration: whoever adds a field says
    // which of the two it is *by choosing its type*, and a new `Name` field the walk fails to
    // visit fails this gate with no row written at all.
    if mentions("Name") {
        return Some(Name(
            "a `Name` — the type says the qualifier owns this position",
        ));
    }
    // A field mentioning `String` is NEVER derived, whatever else its type holds. `Name` took the
    // qualifiable half away, but the remainder is still ambiguous between a local binding, a member
    // name, a globally-scoped tier name and literal text — the walk must not touch any of them, but
    // it must not touch them for *different reasons*, and the reason is what `Unqualified` records.
    // Checked before everything else below so that a future `Vec<(String, Span)>` cannot slip
    // through as "inert, it has a Span in it".
    if mentions("String") {
        return None;
    }
    // `TypeRef` next: a type reference must be qualified wherever it sits.
    if mentions("TypeRef") {
        return Some(Type("a declared type reference"));
    }
    if NODES.iter().any(|n| mentions(n)) {
        return Some(Sub("a sub-node of the AST"));
    }
    if SCALARS.iter().any(|s| mentions(s)) {
        return Some(Inert("a span, a literal, or a flag"));
    }
    None
}

// -------------------------------------------------------------------------------------------
// Probes: real AST values with a sentinel in every classified position.
// -------------------------------------------------------------------------------------------

const SP: Span = Span {
    start: 0,
    end: 0,
    source: SourceId(0),
};

/// The sentinel for one field. Distinct per (node, field), and shaped like an identifier so a
/// member chain built out of two of them is still a plausible dotted candidate.
fn sentinel(node: &str, field: &str) -> String {
    format!("Zq{}_{}", node.replace("::", ""), field)
}

fn ident(node: &str, field: &str) -> Expr {
    Expr::Ident {
        name: AstName::written(sentinel(node, field)),
        span: SP,
    }
}

fn bx(node: &str, field: &str) -> Box<Expr> {
    Box::new(ident(node, field))
}

fn tref(node: &str, field: &str) -> TypeRef {
    TypeRef::Named {
        name: AstName::written(sentinel(node, field)),
        args: Vec::new(),
        span: SP,
    }
}

/// A statement carrying the sentinel — for the `Vec<Stmt>` positions.
fn stm(node: &str, field: &str) -> Stmt {
    Stmt::Expr {
        expr: ident(node, field),
        span: SP,
    }
}

fn arg(node: &str, field: &str) -> CallArg {
    CallArg::positional(ident(node, field))
}

fn attr(node: &str, field: &str) -> Attribute {
    Attribute {
        name: AstName::written(sentinel(node, field)),
        name_span: SP,
        args: Vec::new(),
        span: SP,
    }
}

/// A `FnDecl` carrying the sentinel in its **return type** — the one position `q_fn` reaches in
/// every context (a method's name deliberately does not qualify, so a name-carried sentinel would
/// report a false negative for `TraitMethod::sig` and friends).
fn fn_with(node: &str, field: &str) -> FnDecl {
    FnDecl {
        name: AstName::written("zqfn"),
        name_span: SP,
        is_public: false,
        type_params: Vec::new(),
        params: Vec::new(),
        ret: Some(tref(node, field)),
        attrs: Vec::new(),
        directives: Vec::new(),
        is_dev_tier: false,
        is_async: false,
        is_static: false,
        tier: None,
        captures: Vec::new(),
        body: Vec::new(),
        span: SP,
    }
}

fn param_with(node: &str, field: &str) -> Param {
    Param {
        attrs: Vec::new(),
        name: "zqp".into(),
        name_span: SP,
        ty: Some(tref(node, field)),
        default: None,
        span: SP,
        positional: false,
    }
}

fn type_param_with(node: &str, field: &str) -> TypeParam {
    TypeParam {
        name: "Zqtp".into(),
        bounds: vec![TraitBound {
            name: AstName::written(sentinel(node, field)),
            args: Vec::new(),
            span: SP,
        }],
        span: SP,
    }
}

fn impl_block_with(node: &str, field: &str) -> ImplBlock {
    ImplBlock {
        trait_name: AstName::written(sentinel(node, field)),
        trait_span: SP,
        trait_args: Vec::new(),
        methods: Vec::new(),
        assoc_bindings: Vec::new(),
        span: SP,
    }
}

fn decorators_with(node: &str, field: &str) -> Decorators {
    Decorators {
        attrs: vec![attr(node, field)],
        ..Decorators::default()
    }
}

fn field_with(node: &str, field: &str) -> FieldDecl {
    FieldDecl {
        name: "zqf".into(),
        name_span: SP,
        mut_field: false,
        is_public: false,
        ty: Some(tref(node, field)),
        default: None,
        attrs: Vec::new(),
        span: SP,
    }
}

/// Every `Expr` variant, each with a sentinel in every classified field.
fn expr_variants() -> Vec<Expr> {
    let n = |v: &str| format!("Expr::{v}");
    let f = |v: &str, fld: &str| sentinel(&n(v), fld);
    vec![
        Expr::Str {
            value: f("Str", "value"),
            span: SP,
        },
        Expr::Int { value: 1, span: SP },
        Expr::Float {
            value: 1.0,
            span: SP,
        },
        Expr::F32 {
            value: 1.0,
            span: SP,
        },
        Expr::F64 {
            value: 1.0,
            span: SP,
        },
        Expr::IntN {
            magnitude: 1,
            signed: false,
            bits: 8,
            span: SP,
        },
        Expr::Bool {
            value: true,
            span: SP,
        },
        ident(&n("Ident"), "name"),
        Expr::Unary {
            op: UnaryOp::Neg,
            operand: bx(&n("Unary"), "operand"),
            span: SP,
        },
        Expr::Binary {
            op: noeta_ast::BinaryOp::Add,
            lhs: bx(&n("Binary"), "lhs"),
            rhs: bx(&n("Binary"), "rhs"),
            span: SP,
        },
        Expr::Call {
            callee: bx(&n("Call"), "callee"),
            args: vec![arg(&n("Call"), "args")],
            span: SP,
        },
        Expr::Closure {
            params: vec![param_with(&n("Closure"), "params")],
            ret: Some(tref(&n("Closure"), "ret")),
            body: ClosureBody::Expr(bx(&n("Closure"), "body")),
            span: SP,
        },
        Expr::Pipeline {
            left: bx(&n("Pipeline"), "left"),
            right: bx(&n("Pipeline"), "right"),
            span: SP,
        },
        Expr::List {
            items: vec![ident(&n("List"), "items")],
            span: SP,
        },
        Expr::Tuple {
            items: vec![ident(&n("Tuple"), "items")],
            span: SP,
        },
        Expr::TupleIndex {
            receiver: bx(&n("TupleIndex"), "receiver"),
            index: 0,
            span: SP,
        },
        Expr::Range {
            start: bx(&n("Range"), "start"),
            end: bx(&n("Range"), "end"),
            inclusive: false,
            span: SP,
        },
        Expr::Map {
            entries: vec![(ident(&n("Map"), "entries"), ident(&n("Map"), "entries"))],
            span: SP,
        },
        // The receiver is a plain `Ident` on purpose: only a pure ident/member chain reaches the
        // qualified-chain collapse, which is what visits the member NAME.
        Expr::Member {
            receiver: bx(&n("Member"), "receiver"),
            name: f("Member", "name"),
            name_span: SP,
            span: SP,
        },
        Expr::Index {
            receiver: bx(&n("Index"), "receiver"),
            index: bx(&n("Index"), "index"),
            span: SP,
        },
        Expr::Interp {
            parts: vec![
                StrPart::Literal(sentinel("StrPart::Literal", "0")),
                StrPart::Hole(ident("StrPart::Hole", "0")),
                StrPart::Hole(ident(&n("Interp"), "parts")),
            ],
            span: SP,
        },
        Expr::Match {
            scrutinee: bx(&n("Match"), "scrutinee"),
            arms: vec![MatchArm {
                pattern: Pattern::IsType {
                    ty: tref(&n("Match"), "arms"),
                    span: SP,
                },
                guard: None,
                body: ClosureBody::Expr(Box::new(Expr::Int { value: 0, span: SP })),
                span: SP,
            }],
            span: SP,
        },
        Expr::Object(ObjectLit {
            type_name: Some(AstName::written(f("Object", "0"))),
            type_name_span: SP,
            fields: Vec::new(),
            spread: None,
            span: SP,
        }),
        Expr::Try {
            expr: bx(&n("Try"), "expr"),
            span: SP,
        },
        Expr::Await {
            expr: bx(&n("Await"), "expr"),
            span: SP,
        },
        Expr::Spawn {
            future: bx(&n("Spawn"), "future"),
            isolate: false,
            span: SP,
        },
        Expr::Coalesce {
            value: bx(&n("Coalesce"), "value"),
            fallback: bx(&n("Coalesce"), "fallback"),
            span: SP,
        },
        Expr::As {
            expr: bx(&n("As"), "expr"),
            ty: tref(&n("As"), "ty"),
            span: SP,
        },
        // **The reflection surface**: one `Expr::Reflect` per `ReflectOperand` arm, rather than one
        // per intrinsic. That is the point of the collapse — the qualifier's obligation is a
        // function of the *operand shape*, not of which query is being asked, so probing thirteen
        // keywords would have been thirteen probes of seven behaviours.
        //
        // `which` is inert (a fieldless enum); `operand` is the carrier.
        Expr::Reflect {
            which: ReflectKind::AttributesOf,
            operand: ReflectOperand::Type(TypeOperand::Static(tref("ReflectOperand::Type", "0"))),
            span: SP,
        },
        // The dynamic arm of the same operand — a runtime string, which must be recursed into and
        // must NOT be rewritten as a type name.
        Expr::Reflect {
            which: ReflectKind::AttributesOf,
            operand: ReflectOperand::Type(TypeOperand::Dynamic(bx("TypeOperand::Dynamic", "0"))),
            span: SP,
        },
        Expr::Reflect {
            which: ReflectKind::RolesOf,
            operand: ReflectOperand::Nothing,
            span: SP,
        },
        Expr::Reflect {
            which: ReflectKind::TypeName,
            operand: ReflectOperand::StaticType(tref("ReflectOperand::StaticType", "0")),
            span: SP,
        },
        Expr::Reflect {
            which: ReflectKind::TypeOf,
            operand: ReflectOperand::Value(bx("ReflectOperand::Value", "0")),
            span: SP,
        },
        // The carrier itself. Every arm probe above names a position INSIDE a `ReflectOperand`,
        // which proves the qualifier reaches that arm's payload but says nothing about whether it
        // descends into `operand` at all — and "the walk never descends into this field" is exactly
        // the shape of the three bugs this gate was built for.
        Expr::Reflect {
            which: ReflectKind::TypeOf,
            operand: ReflectOperand::Value(bx(&n("Reflect"), "operand")),
            span: SP,
        },
        Expr::Reflect {
            which: ReflectKind::FromBytes,
            operand: ReflectOperand::StaticTypeWith {
                ty: tref("ReflectOperand::StaticTypeWith", "ty"),
                arg: bx("ReflectOperand::StaticTypeWith", "arg"),
            },
            span: SP,
        },
        Expr::Reflect {
            which: ReflectKind::Construct,
            operand: ReflectOperand::TypeWith {
                ty: TypeOperand::Static(tref("ReflectOperand::TypeWith", "ty")),
                arg: bx("ReflectOperand::TypeWith", "arg"),
            },
            span: SP,
        },
        Expr::Reflect {
            which: ReflectKind::Invoke,
            operand: ReflectOperand::Dispatch {
                recv: Some(bx("ReflectOperand::Dispatch", "recv")),
                name: bx("ReflectOperand::Dispatch", "name"),
                args: bx("ReflectOperand::Dispatch", "args"),
            },
            span: SP,
        },
        Expr::Channel {
            elem: tref(&n("Channel"), "elem"),
            capacity: bx(&n("Channel"), "capacity"),
            span: SP,
        },
        Expr::TypedModuleCall {
            recv: bx(&n("TypedModuleCall"), "recv"),
            func: f("TypedModuleCall", "func"),
            func_span: SP,
            ty: tref(&n("TypedModuleCall"), "ty"),
            args: vec![arg(&n("TypedModuleCall"), "args")],
            span: SP,
        },
        Expr::TypedCall {
            name: AstName::written(f("TypedCall", "name")),
            name_span: SP,
            type_args: vec![tref(&n("TypedCall"), "type_args")],
            args: vec![arg(&n("TypedCall"), "args")],
            span: SP,
        },
        Expr::TypedMethodCall {
            recv: bx(&n("TypedMethodCall"), "recv"),
            name: f("TypedMethodCall", "name"),
            name_span: SP,
            type_args: vec![tref(&n("TypedMethodCall"), "type_args")],
            args: vec![arg(&n("TypedMethodCall"), "args")],
            span: SP,
        },
        Expr::InstantiatedType {
            recv: bx(&n("InstantiatedType"), "recv"),
            type_args: vec![tref(&n("InstantiatedType"), "type_args")],
            span: SP,
        },
        // The `TypeWith` operand's dynamic arm — the name is an expression, the argument still an
        // expression.
        Expr::Reflect {
            which: ReflectKind::Construct,
            operand: ReflectOperand::TypeWith {
                ty: TypeOperand::Dynamic(bx("TypeOperand::Dynamic", "0")),
                arg: Box::new(Expr::Int { value: 0, span: SP }),
            },
            span: SP,
        },
        // `invoke`'s free-function form: no receiver at all.
        Expr::Reflect {
            which: ReflectKind::Invoke,
            operand: ReflectOperand::Dispatch {
                recv: None,
                name: Box::new(Expr::Int { value: 0, span: SP }),
                args: Box::new(Expr::Int { value: 0, span: SP }),
            },
            span: SP,
        },
        Expr::TypeTest {
            expr: bx(&n("TypeTest"), "expr"),
            ty: tref(&n("TypeTest"), "ty"),
            span: SP,
        },
        Expr::FieldSet {
            receiver: bx(&n("FieldSet"), "receiver"),
            field: f("FieldSet", "field"),
            field_span: SP,
            value: bx(&n("FieldSet"), "value"),
            span: SP,
        },
        Expr::TierExpr {
            tier: f("TierExpr", "tier"),
            tier_span: SP,
            statics: vec![f("TierExpr", "statics")],
            holes: vec![ident(&n("TierExpr"), "holes")],
            span: SP,
        },
        Expr::NativeFnRef {
            module: f("NativeFnRef", "module"),
            func: f("NativeFnRef", "func"),
            span: SP,
        },
        // `TypeOperand::Static` — its own row, distinct from the `ReflectOperand::Type` one above.
        Expr::Reflect {
            which: ReflectKind::FieldSpecsOf,
            operand: ReflectOperand::Type(TypeOperand::Static(tref("TypeOperand::Static", "0"))),
            span: SP,
        },
        // `ClosureBody::Block` and `ClosureBody::Expr`.
        Expr::Closure {
            params: Vec::new(),
            ret: None,
            body: ClosureBody::Block(vec![stm("ClosureBody::Block", "0")]),
            span: SP,
        },
        Expr::Closure {
            params: Vec::new(),
            ret: None,
            body: ClosureBody::Expr(bx("ClosureBody::Expr", "0")),
            span: SP,
        },
        // `MatchArm`'s own fields.
        Expr::Match {
            scrutinee: Box::new(Expr::Int { value: 0, span: SP }),
            arms: vec![MatchArm {
                pattern: Pattern::IsType {
                    ty: tref("MatchArm", "pattern"),
                    span: SP,
                },
                guard: Some(ident("MatchArm", "guard")),
                body: ClosureBody::Expr(bx("MatchArm", "body")),
                span: SP,
            }],
            span: SP,
        },
        // `ObjectLit`'s and `FieldInit`'s own fields.
        Expr::Object(ObjectLit {
            type_name: Some(AstName::written(sentinel("ObjectLit", "type_name"))),
            type_name_span: SP,
            fields: vec![
                FieldInit {
                    name: sentinel("FieldInit", "name"),
                    name_span: SP,
                    value: ident("FieldInit", "value"),
                    span: SP,
                },
                FieldInit {
                    name: "zqfi".into(),
                    name_span: SP,
                    value: ident("ObjectLit", "fields"),
                    span: SP,
                },
            ],
            spread: Some(bx("ObjectLit", "spread")),
            span: SP,
        }),
        // `CallArg`'s own fields.
        Expr::Call {
            callee: Box::new(Expr::Int { value: 0, span: SP }),
            args: vec![CallArg {
                name: Some(sentinel("CallArg", "name")),
                value: ident("CallArg", "value"),
                span: SP,
            }],
            span: SP,
        },
        // Every `Pattern` variant, in one match.
        Expr::Match {
            scrutinee: Box::new(Expr::Int { value: 0, span: SP }),
            arms: pattern_variants()
                .into_iter()
                .map(|pattern| MatchArm {
                    pattern,
                    guard: None,
                    body: ClosureBody::Expr(Box::new(Expr::Int { value: 0, span: SP })),
                    span: SP,
                })
                .collect(),
            span: SP,
        },
        // Every `TypeRef` variant, hung off one `As`.
        Expr::As {
            expr: Box::new(Expr::Int { value: 0, span: SP }),
            ty: TypeRef::Named {
                name: AstName::written("Zqhost"),
                args: typeref_variants(),
                span: SP,
            },
            span: SP,
        },
    ]
}

fn pattern_variants() -> Vec<Pattern> {
    let n = |v: &str| format!("Pattern::{v}");
    vec![
        Pattern::Wildcard { span: SP },
        Pattern::Binding {
            name: sentinel(&n("Binding"), "name"),
            span: SP,
        },
        Pattern::Int { value: 1, span: SP },
        Pattern::Str {
            value: sentinel(&n("Str"), "value"),
            span: SP,
        },
        Pattern::Bool {
            value: true,
            span: SP,
        },
        Pattern::Variant {
            type_name: Some(AstName::written(sentinel(&n("Variant"), "type_name"))),
            variant: sentinel(&n("Variant"), "variant"),
            bindings: vec![Pattern::IsType {
                ty: tref(&n("Variant"), "bindings"),
                span: SP,
            }],
            span: SP,
        },
        Pattern::IsType {
            ty: tref(&n("IsType"), "ty"),
            span: SP,
        },
        Pattern::Tuple {
            elements: vec![Pattern::IsType {
                ty: tref(&n("Tuple"), "elements"),
                span: SP,
            }],
            span: SP,
        },
    ]
}

fn typeref_variants() -> Vec<TypeRef> {
    let n = |v: &str| format!("TypeRef::{v}");
    vec![
        TypeRef::Named {
            name: AstName::written(sentinel(&n("Named"), "name")),
            args: vec![tref(&n("Named"), "args")],
            span: SP,
        },
        TypeRef::DynTrait {
            trait_name: AstName::written(sentinel(&n("DynTrait"), "trait_name")),
            span: SP,
        },
        TypeRef::Optional {
            inner: Box::new(tref(&n("Optional"), "inner")),
            span: SP,
        },
        TypeRef::Union {
            members: vec![tref(&n("Union"), "members")],
            span: SP,
        },
        TypeRef::Tuple {
            elements: vec![tref(&n("Tuple"), "elements")],
            span: SP,
        },
        TypeRef::Fn {
            params: vec![tref(&n("Fn"), "params")],
            ret: Box::new(tref(&n("Fn"), "ret")),
            span: SP,
        },
        TypeRef::AssocProjection {
            name: sentinel(&n("AssocProjection"), "name"),
            span: SP,
        },
    ]
}

/// Every `AttrValue` variant, as the arguments of one attribute.
fn attr_value_variants() -> Vec<AttrArg> {
    let n = |v: &str| format!("AttrValue::{v}");
    let one = |v: AttrValue| AttrArg {
        name: None,
        value: v,
        span: SP,
    };
    let tr = |node: &str, field: &str| AttrValue::TypeRef {
        name: AstName::written(sentinel(node, field)),
        args: Vec::new(),
    };
    vec![
        one(AttrValue::Str(sentinel(&n("Str"), "0"))),
        one(AttrValue::Int(1)),
        one(AttrValue::Float(1.0)),
        one(AttrValue::Bool(true)),
        one(AttrValue::List(vec![tr(&n("List"), "0")])),
        one(AttrValue::Set(vec![tr(&n("Set"), "0")])),
        one(AttrValue::Map(vec![("k".into(), tr(&n("Map"), "0"))])),
        one(AttrValue::Enum {
            enum_name: AstName::written(sentinel(&n("Enum"), "enum_name")),
            variant: sentinel(&n("Enum"), "variant"),
            args: vec![tr(&n("Enum"), "args")],
        }),
        one(AttrValue::Struct {
            type_name: AstName::written(sentinel(&n("Struct"), "type_name")),
            fields: vec![("f".into(), tr(&n("Struct"), "fields"))],
        }),
        one(AttrValue::TypeRef {
            name: AstName::written(sentinel(&n("TypeRef"), "name")),
            args: vec![tref(&n("TypeRef"), "args")],
        }),
        // `AttrArg`'s own fields.
        AttrArg {
            name: Some(sentinel("AttrArg", "name")),
            value: tr("AttrArg", "value"),
            span: SP,
        },
    ]
}

/// Every `Stmt` variant, each with a sentinel in every classified field.
fn stmt_variants() -> Vec<Stmt> {
    let n = |v: &str| format!("Stmt::{v}");
    let f = |v: &str, fld: &str| sentinel(&n(v), fld);
    vec![
        Stmt::Echo {
            value: ident(&n("Echo"), "value"),
            span: SP,
        },
        Stmt::Binding {
            mut_decl: false,
            name: f("Binding", "name"),
            name_span: SP,
            ty: Some(tref(&n("Binding"), "ty")),
            value: ident(&n("Binding"), "value"),
            span: SP,
        },
        Stmt::Destructure {
            mut_decl: false,
            targets: vec![(f("Destructure", "targets"), SP)],
            value: ident(&n("Destructure"), "value"),
            span: SP,
        },
        Stmt::Fn(FnDecl {
            name: AstName::written(f("Fn", "0")),
            ..fn_with("zq", "unused")
        }),
        Stmt::Enum(EnumDecl {
            name: AstName::written(f("Enum", "0")),
            name_span: SP,
            is_public: false,
            type_params: Vec::new(),
            backing: None,
            variants: Vec::new(),
            methods: Vec::new(),
            impls: Vec::new(),
            decorators: Decorators::default(),
            span: SP,
        }),
        Stmt::Struct(StructDecl {
            name: AstName::written(f("Struct", "0")),
            name_span: SP,
            is_public: false,
            type_params: Vec::new(),
            fields: Vec::new(),
            methods: Vec::new(),
            impls: Vec::new(),
            decorators: Decorators::default(),
            span: SP,
        }),
        Stmt::Class(ClassDecl {
            name: AstName::written(f("Class", "0")),
            name_span: SP,
            is_public: false,
            type_params: Vec::new(),
            fields: Vec::new(),
            methods: Vec::new(),
            impls: Vec::new(),
            decorators: Decorators::default(),
            destructor: None,
            span: SP,
        }),
        Stmt::Impl(ImplDecl {
            trait_name: AstName::written(f("Impl", "0")),
            trait_span: SP,
            trait_args: Vec::new(),
            target: AstName::written("Zqtarget"),
            target_span: SP,
            methods: Vec::new(),
            assoc_bindings: Vec::new(),
            span: SP,
        }),
        Stmt::Trait(TraitDecl {
            name: AstName::written(f("Trait", "0")),
            name_span: SP,
            is_public: false,
            type_params: Vec::new(),
            methods: Vec::new(),
            assoc_types: Vec::new(),
            decorators: Decorators::default(),
            span: SP,
        }),
        Stmt::Namespace {
            path: vec![f("Namespace", "path")],
            span: SP,
        },
        Stmt::Use {
            path: vec![f("Use", "path")],
            names: vec![UseName {
                name: f("Use", "names"),
                span: SP,
                alias: None,
            }],
            span: SP,
        },
        Stmt::Return {
            value: Some(ident(&n("Return"), "value")),
            span: SP,
        },
        Stmt::Yield {
            value: ident(&n("Yield"), "value"),
            span: SP,
        },
        Stmt::Concurrent {
            body: vec![stm(&n("Concurrent"), "body")],
            span: SP,
        },
        Stmt::If {
            cond: ident(&n("If"), "cond"),
            then_body: vec![stm(&n("If"), "then_body")],
            else_body: Some(vec![stm(&n("If"), "else_body")]),
            span: SP,
        },
        Stmt::For {
            pattern: ForPattern::Single {
                name: f("For", "pattern"),
                name_span: SP,
            },
            iterable: ident(&n("For"), "iterable"),
            body: vec![stm(&n("For"), "body")],
            span: SP,
        },
        Stmt::While {
            cond: ident(&n("While"), "cond"),
            body: vec![stm(&n("While"), "body")],
            span: SP,
        },
        Stmt::Break { span: SP },
        Stmt::Continue { span: SP },
        Stmt::Expr {
            expr: ident(&n("Expr"), "expr"),
            span: SP,
        },
        Stmt::TierBlock {
            tier: f("TierBlock", "tier"),
            tier_span: SP,
            args: vec![AttrArg {
                name: None,
                value: AttrValue::TypeRef {
                    name: AstName::written(f("TierBlock", "args")),
                    args: Vec::new(),
                },
                span: SP,
            }],
            items: vec![stm(&n("TierBlock"), "items")],
            doc_text: Some(f("TierBlock", "doc_text")),
            attached: false,
            span: SP,
        },
    ]
}

/// The declaration structs, each wrapped in the statement that reaches it.
fn decl_probes() -> Vec<Stmt> {
    let s = sentinel;
    vec![
        // FnDecl (and, through it, TierDecl, Param, TypeParam, TraitBound, Attribute, AttrArg,
        // AttrValue, MethodDirective).
        Stmt::Fn(FnDecl {
            name: AstName::written(s("FnDecl", "name")),
            name_span: SP,
            is_public: false,
            type_params: vec![
                type_param_with("FnDecl", "type_params"),
                TypeParam {
                    name: s("TypeParam", "name"),
                    bounds: vec![
                        TraitBound {
                            name: AstName::written(s("TraitBound", "name")),
                            args: vec![tref("TraitBound", "args")],
                            span: SP,
                        },
                        TraitBound {
                            name: AstName::written(s("TypeParam", "bounds")),
                            args: Vec::new(),
                            span: SP,
                        },
                    ],
                    span: SP,
                },
            ],
            params: vec![
                param_with("FnDecl", "params"),
                Param {
                    attrs: vec![attr("Param", "attrs")],
                    name: s("Param", "name"),
                    name_span: SP,
                    ty: Some(tref("Param", "ty")),
                    default: Some(ident("Param", "default")),
                    span: SP,
                    positional: false,
                },
            ],
            ret: Some(tref("FnDecl", "ret")),
            attrs: vec![
                attr("FnDecl", "attrs"),
                Attribute {
                    name: AstName::written(s("Attribute", "name")),
                    name_span: SP,
                    args: {
                        let mut args = attr_value_variants();
                        args.push(AttrArg {
                            name: None,
                            value: AttrValue::TypeRef {
                                name: AstName::written(s("Attribute", "args")),
                                args: Vec::new(),
                            },
                            span: SP,
                        });
                        args
                    },
                    span: SP,
                },
            ],
            directives: vec![
                MethodDirective {
                    name: s("MethodDirective", "name"),
                    name_span: SP,
                    args: vec![AttrArg {
                        name: None,
                        value: AttrValue::TypeRef {
                            name: AstName::written(s("MethodDirective", "args")),
                            args: Vec::new(),
                        },
                        span: SP,
                    }],
                    doc_text: Some(s("MethodDirective", "doc_text")),
                    span: SP,
                },
                MethodDirective {
                    name: "zqd".into(),
                    name_span: SP,
                    args: vec![AttrArg {
                        name: None,
                        value: AttrValue::TypeRef {
                            name: AstName::written(s("FnDecl", "directives")),
                            args: Vec::new(),
                        },
                        span: SP,
                    }],
                    doc_text: None,
                    span: SP,
                },
            ],
            is_dev_tier: false,
            is_async: false,
            is_static: false,
            tier: Some(TierDecl {
                name: s("TierDecl", "name"),
                name_span: SP,
                config: Some((AstName::written(s("TierDecl", "config")), SP)),
                text: Some((s("TierDecl", "text"), SP)),
                expr: Some((AstName::written(s("TierDecl", "expr")), SP)),
                span: SP,
            }),
            captures: vec![(s("FnDecl", "captures"), SP)],
            body: vec![stm("FnDecl", "body")],
            span: SP,
        }),
        // A second `Stmt::Fn` whose tier carries only the `FnDecl::tier` sentinel.
        Stmt::Fn(FnDecl {
            tier: Some(TierDecl {
                name: "zqt".into(),
                name_span: SP,
                config: Some((AstName::written(s("FnDecl", "tier")), SP)),
                text: None,
                expr: None,
                span: SP,
            }),
            ..fn_with("zq", "unused")
        }),
        // StructDecl (and FieldDecl, Decorators, DeriveSpec, RoleTag, ImplBlock).
        Stmt::Struct(StructDecl {
            name: AstName::written(s("StructDecl", "name")),
            name_span: SP,
            is_public: false,
            type_params: vec![type_param_with("StructDecl", "type_params")],
            fields: vec![
                field_with("StructDecl", "fields"),
                FieldDecl {
                    name: s("FieldDecl", "name"),
                    name_span: SP,
                    mut_field: false,
                    is_public: false,
                    ty: Some(tref("FieldDecl", "ty")),
                    default: Some(ident("FieldDecl", "default")),
                    attrs: vec![attr("FieldDecl", "attrs")],
                    span: SP,
                },
            ],
            methods: vec![fn_with("StructDecl", "methods")],
            impls: vec![
                impl_block_with("StructDecl", "impls"),
                ImplBlock {
                    trait_name: AstName::written(s("ImplBlock", "trait_name")),
                    trait_span: SP,
                    trait_args: vec![tref("ImplBlock", "trait_args")],
                    methods: vec![fn_with("ImplBlock", "methods")],
                    assoc_bindings: vec![("Item".into(), tref("ImplBlock", "assoc_bindings"))],
                    span: SP,
                },
            ],
            decorators: Decorators {
                derives: vec![
                    DeriveSpec {
                        name: AstName::written(s("Decorators", "derives")),
                        args: Vec::new(),
                        bindings: Vec::new(),
                        via: None,
                        span: SP,
                    },
                    DeriveSpec {
                        name: AstName::written(s("DeriveSpec", "name")),
                        args: vec![tref("DeriveSpec", "args")],
                        bindings: vec![MemberBinding {
                            member: "m".into(),
                            target: s("DeriveSpec", "bindings"),
                            span: SP,
                        }],
                        via: Some((s("DeriveSpec", "via"), SP)),
                        span: SP,
                    },
                ],
                attrs: vec![attr("Decorators", "attrs")],
                attribute: Some(vec![(s("Decorators", "attribute"), SP)]),
                role: Some(vec![
                    RoleTag {
                        enum_name: AstName::written(s("Decorators", "role")),
                        variant: "V".into(),
                        span: SP,
                    },
                    RoleTag {
                        enum_name: AstName::written(s("RoleTag", "enum_name")),
                        variant: s("RoleTag", "variant"),
                        span: SP,
                    },
                ]),
                semantic: None,
                packed: None,
                validated: None,
                foreign: vec![ForeignDirective {
                    name: "zqfd".into(),
                    name_span: SP,
                    args: vec![AttrArg {
                        name: None,
                        value: AttrValue::TypeRef {
                            name: AstName::written(s("Decorators", "foreign")),
                            args: Vec::new(),
                        },
                        span: SP,
                    }],
                    span: SP,
                }],
            },
            span: SP,
        }),
        // ClassDecl.
        Stmt::Class(ClassDecl {
            name: AstName::written(s("ClassDecl", "name")),
            name_span: SP,
            is_public: false,
            type_params: vec![type_param_with("ClassDecl", "type_params")],
            fields: vec![field_with("ClassDecl", "fields")],
            methods: vec![fn_with("ClassDecl", "methods")],
            impls: vec![impl_block_with("ClassDecl", "impls")],
            decorators: decorators_with("ClassDecl", "decorators"),
            destructor: Some(vec![stm("ClassDecl", "destructor")]),
            span: SP,
        }),
        // A second struct, to carry `StructDecl::decorators` on its own.
        Stmt::Struct(StructDecl {
            name: AstName::written("Zqs2"),
            name_span: SP,
            is_public: false,
            type_params: Vec::new(),
            fields: Vec::new(),
            methods: Vec::new(),
            impls: Vec::new(),
            decorators: decorators_with("StructDecl", "decorators"),
            span: SP,
        }),
        // EnumDecl (and VariantDecl).
        Stmt::Enum(EnumDecl {
            name: AstName::written(s("EnumDecl", "name")),
            name_span: SP,
            is_public: false,
            type_params: vec![type_param_with("EnumDecl", "type_params")],
            backing: Some(tref("EnumDecl", "backing")),
            variants: vec![
                noeta_ast::VariantDecl {
                    name: "Zqv".into(),
                    name_span: SP,
                    fields: vec![param_with("EnumDecl", "variants")],
                    backed_value: None,
                    attrs: Vec::new(),
                    span: SP,
                },
                noeta_ast::VariantDecl {
                    name: s("VariantDecl", "name"),
                    name_span: SP,
                    fields: vec![param_with("VariantDecl", "fields")],
                    backed_value: Some(ident("VariantDecl", "backed_value")),
                    attrs: vec![attr("VariantDecl", "attrs")],
                    span: SP,
                },
            ],
            methods: vec![fn_with("EnumDecl", "methods")],
            impls: vec![impl_block_with("EnumDecl", "impls")],
            decorators: decorators_with("EnumDecl", "decorators"),
            span: SP,
        }),
        // TraitDecl (and TraitMethod, AssocTypeDecl).
        Stmt::Trait(TraitDecl {
            name: AstName::written(s("TraitDecl", "name")),
            name_span: SP,
            is_public: false,
            type_params: vec![type_param_with("TraitDecl", "type_params")],
            methods: vec![
                TraitMethod {
                    sig: fn_with("TraitDecl", "methods"),
                    has_default: false,
                },
                TraitMethod {
                    sig: fn_with("TraitMethod", "sig"),
                    has_default: true,
                },
            ],
            assoc_types: vec![
                AssocTypeDecl {
                    name: "Item".into(),
                    name_span: SP,
                    default: Some(tref("TraitDecl", "assoc_types")),
                    span: SP,
                },
                AssocTypeDecl {
                    name: s("AssocTypeDecl", "name"),
                    name_span: SP,
                    default: Some(tref("AssocTypeDecl", "default")),
                    span: SP,
                },
            ],
            decorators: decorators_with("TraitDecl", "decorators"),
            span: SP,
        }),
        // ImplDecl.
        Stmt::Impl(ImplDecl {
            trait_name: AstName::written(s("ImplDecl", "trait_name")),
            trait_span: SP,
            trait_args: vec![tref("ImplDecl", "trait_args")],
            target: AstName::written(s("ImplDecl", "target")),
            target_span: SP,
            methods: vec![fn_with("ImplDecl", "methods")],
            assoc_bindings: vec![("Item".into(), tref("ImplDecl", "assoc_bindings"))],
            span: SP,
        }),
    ]
}

/// Every probe, as one statement list.
fn probes() -> Vec<Stmt> {
    let mut all = vec![
        // The `Expr` corpus, hung off a list literal so every variant is walked.
        Stmt::Expr {
            expr: Expr::List {
                items: expr_variants(),
                span: SP,
            },
            span: SP,
        },
        // `TypeOperand::Dynamic`'s payload is exercised in `expr_variants`; the `Static` arm too.
    ];
    all.extend(stmt_variants());
    all.extend(decl_probes());
    all
}

// -------------------------------------------------------------------------------------------
// The declaration scan.
// -------------------------------------------------------------------------------------------

/// The AST types this gate covers, by kind. A type absent from here is exempt only because a
/// [`TABLE`] row on the field that HOLDS it says so out loud (`Vec<UseName>`,
/// `Option<PackedDirective>`, `Vec<ForeignDirective>`, `Vec<MemberBinding>`, `ForPattern`).
const SCANNED_ENUMS: &[&str] = &[
    "Stmt",
    "Expr",
    "Pattern",
    "TypeRef",
    "TypeOperand",
    "ReflectOperand",
    "ClosureBody",
    "StrPart",
    "AttrValue",
];

const SCANNED_STRUCTS: &[&str] = &[
    "ObjectLit",
    "FieldInit",
    "MatchArm",
    "FnDecl",
    "TierDecl",
    "Param",
    "FieldDecl",
    "StructDecl",
    "ClassDecl",
    "EnumDecl",
    "VariantDecl",
    "TraitDecl",
    "TraitMethod",
    "AssocTypeDecl",
    "ImplDecl",
    "ImplBlock",
    "TypeParam",
    "TraitBound",
    "Decorators",
    "DeriveSpec",
    "Attribute",
    "RoleTag",
    "MethodDirective",
];

/// One declared field: `(node, field, declared type)`.
type Declared = (String, String, String);

fn ast_source() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../noeta-ast/src/lib.rs")
        .canonicalize()
        .expect("the AST crate is a sibling of this one");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} is unreadable: {e}", path.display()))
}

/// The lines of `src` with comment and attribute lines removed — so a doc comment containing a
/// brace cannot throw off the depth counting below.
fn code_lines(src: &str) -> Vec<&str> {
    src.lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with("#[")
        })
        .collect()
}

/// The body lines of the item whose declaration line starts with `head`.
fn item_body<'a>(lines: &[&'a str], head: &str) -> Vec<&'a str> {
    let start = lines
        .iter()
        .position(|l| l.starts_with(head))
        .unwrap_or_else(|| panic!("`{head}` not found — did the type move or get renamed?"));
    let mut depth = 0i32;
    let mut out = Vec::new();
    for l in &lines[start..] {
        depth += l.matches('{').count() as i32 - l.matches('}').count() as i32;
        out.push(*l);
        if depth == 0 && out.len() > 1 {
            break;
        }
    }
    out[1..out.len() - 1].to_vec()
}

fn struct_fields(lines: &[&str], head: &str, node: &str) -> Vec<Declared> {
    item_body(lines, head)
        .iter()
        .filter_map(|l| {
            let t = l.trim();
            let rest = t.strip_prefix("pub ")?;
            let (name, ty) = rest.split_once(':')?;
            Some((
                node.to_string(),
                name.trim().to_string(),
                ty.trim().trim_end_matches(',').to_string(),
            ))
        })
        .collect()
}

fn enum_fields(lines: &[&str], name: &str) -> Vec<Declared> {
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    for l in item_body(lines, &format!("pub enum {name} ")) {
        let t = l.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(variant) = &current {
            if t.starts_with('}') {
                current = None;
                continue;
            }
            if let Some((f, ty)) = t.split_once(':') {
                out.push((
                    variant.clone(),
                    f.trim().to_string(),
                    ty.trim().trim_end_matches(',').to_string(),
                ));
            }
            continue;
        }
        let head = t.split(|c: char| !c.is_alphanumeric()).next().unwrap_or("");
        if head.is_empty() || !head.starts_with(|c: char| c.is_ascii_uppercase()) {
            continue;
        }
        let node = format!("{name}::{head}");
        let after = t[head.len()..].trim();
        if let Some(inner) = after.strip_prefix('{') {
            let inner = inner.trim();
            if inner.ends_with('}') || inner.ends_with("},") {
                // A one-line struct variant.
                for part in inner.trim_end_matches(',').trim_end_matches('}').split(',') {
                    if let Some((f, ty)) = part.split_once(':') {
                        out.push((node.clone(), f.trim().to_string(), ty.trim().to_string()));
                    }
                }
            } else {
                current = Some(node);
            }
        } else if let Some(inner) = after.strip_prefix('(') {
            let ty = inner.trim_end_matches(',').trim_end_matches(')');
            out.push((node, "0".to_string(), ty.trim().to_string()));
        }
        // A unit variant contributes no fields.
    }
    out
}

/// Every field the gate must classify.
fn declared_fields() -> Vec<Declared> {
    let src = ast_source();
    let lines = code_lines(&src);
    let mut out = Vec::new();
    for e in SCANNED_ENUMS {
        out.extend(enum_fields(&lines, e));
    }
    for s in SCANNED_STRUCTS {
        out.extend(struct_fields(&lines, &format!("pub struct {s} "), s));
    }
    // `CallArg` and `AttrArg` are two aliases of one generic `Arg<V>`; the walk treats them
    // differently (one holds expressions, the other a literal tree), so each is classified.
    for alias in ["CallArg", "AttrArg"] {
        out.extend(struct_fields(&lines, "pub struct Arg<V> ", alias));
    }
    out
}

/// The verdict for one field: the [`TABLE`] row if there is one, else the type-derived default.
fn verdict_for(node: &str, field: &str, ty: &str) -> Option<Verdict> {
    if let Some(row) = TABLE.iter().find(|r| r.0 == node && r.1 == field) {
        // A borrowed verdict would do, but re-deriving keeps `Verdict` free of lifetimes.
        return Some(match &row.2 {
            Verdict::Name(w) => Name(w),
            Verdict::Type(w) => Type(w),
            Verdict::Sub(w) => Sub(w),
            Verdict::Unqualified(w) => Unqualified(w),
            Verdict::Inert(w) => Inert(w),
        });
    }
    derived_verdict(ty)
}

// -------------------------------------------------------------------------------------------
// The tests.
// -------------------------------------------------------------------------------------------

/// Adding a field to an AST node must not be possible without saying what qualification does with
/// it. This is the half that fires at the commit that introduces the field, rather than whenever
/// someone next reads the walk — which is what all three historical instances needed.
#[test]
fn every_ast_field_is_classified() {
    let declared = declared_fields();
    assert!(
        declared.len() > 300,
        "the AST scan found only {} fields — the parser is broken, not the AST",
        declared.len()
    );
    let unclassified: Vec<String> = declared
        .iter()
        .filter(|(node, field, ty)| verdict_for(node, field, ty).is_none())
        .map(|(node, field, ty)| format!("{node}.{field}: {ty}"))
        .collect();
    assert!(
        unclassified.is_empty(),
        "these AST fields are classified by neither the declared type nor TABLE:\n  {}\n\n\
         A field on one of these nodes is something namespace qualification must have an opinion \
         about. Either its type should be recognised by `derived_verdict` (add it to `NODES` or \
         `SCALARS` if it is a new AST node or a new scalar), or add a `TABLE` row:\n\
         \x20 Name        — an identifier the linker must qualify. It must be visited.\n\
         \x20 Type        — a type reference. It must be qualified.\n\
         \x20 Sub         — a sub-node. It must be recursed into.\n\
         \x20 Unqualified — a name deliberately left alone. Say WHY; the gate then checks the walk \
         does not touch it.\n\
         \x20 Inert       — a span, a literal, a flag. Say why nothing needs to happen.",
        unclassified.join("\n  ")
    );

    let stale: Vec<String> = TABLE
        .iter()
        .filter(|r| !declared.iter().any(|(n, f, _)| n == r.0 && f == r.1))
        .map(|r| format!("{}.{}", r.0, r.1))
        .collect();
    assert!(
        stale.is_empty(),
        "TABLE classifies fields that no longer exist (renamed or removed?): {stale:?}"
    );

    let mut seen: Vec<(&str, &str)> = TABLE.iter().map(|r| (r.0, r.1)).collect();
    seen.sort_unstable();
    let before = seen.len();
    seen.dedup();
    assert_eq!(before, seen.len(), "TABLE classifies a field twice");

    for row in TABLE {
        assert!(
            !row.2.why().is_empty(),
            "{}.{}: a verdict must say why",
            row.0,
            row.1
        );
    }
}

/// Every classified field must be **probed**: the corpus below has to place a sentinel at it, or
/// the reach check silently proves nothing about that field. An `Inert` field is the only
/// exemption, and it is exempt because there is no name to place.
///
/// Without this, the gate would decay exactly the way a green-but-vacuous test does: someone adds a
/// variant, classifies its fields, forgets the probe, and the reach test passes by not looking.
#[test]
fn every_classified_field_is_probed() {
    let corpus = format!("{:?}", probes());
    let missing: Vec<String> = declared_fields()
        .iter()
        .filter(|(node, field, ty)| {
            verdict_for(node, field, ty)
                .and_then(|v| v.must_reach())
                .is_some()
                && !corpus.contains(&sentinel(node, field))
        })
        .map(|(node, field, _)| format!("{node}.{field}"))
        .collect();
    assert!(
        missing.is_empty(),
        "these fields are classified but never probed — add a sentinel for each to the probe \
         corpus, or the reach check below says nothing about them:\n  {}",
        missing.join("\n  ")
    );
}

/// **The reach check.** Run the real walk over the probe corpus and confirm that exactly the fields
/// the table says are visited, are visited.
///
/// [`referenced_names`](super::referenced_names) is the *collecting* client of the very walk
/// [`qualify_stmt`](super::qualify_stmt) rewrites through — one walk, two visitors — so a name this
/// finds is a name the rewriter rewrites, and a name it misses is one the rewriter leaves bare
/// under a `namespace`.
#[test]
fn the_qualifier_reaches_exactly_the_classified_names() {
    let reached: Vec<String> = probes()
        .iter()
        .flat_map(|s| referenced_names(s).into_iter())
        .collect();
    let saw = |needle: &str| reached.iter().any(|n| n.contains(needle));

    let mut unreached = Vec::new();
    let mut spurious = Vec::new();
    for (node, field, ty) in declared_fields() {
        let Some(verdict) = verdict_for(&node, &field, &ty) else {
            continue; // reported by `every_ast_field_is_classified`
        };
        let Some(must) = verdict.must_reach() else {
            continue;
        };
        let hit = saw(&sentinel(&node, &field));
        if must && !hit {
            unreached.push(format!(
                "{node}.{field} [{}] — {}",
                verdict.label(),
                verdict.why()
            ));
        } else if !must && hit {
            spurious.push(format!("{node}.{field} — {}", verdict.why()));
        }
    }

    assert!(
        unreached.is_empty(),
        "the qualifier's walk never reaches these fields, so a name written there stays \
         UNQUALIFIED under a `namespace` — the exact shape of the turbofish-callee, \
         reflection-turbofish and erased-type-param bugs:\n  {}\n\n\
         Either visit the field in `qualify.rs`, or — if it genuinely must not be rewritten — \
         change its verdict to `Unqualified` and say why.",
        unreached.join("\n  ")
    );
    assert!(
        spurious.is_empty(),
        "the qualifier's walk REACHES these fields, which are classified as deliberately left \
         alone. A rewrite here changes what the program means (a string literal that happens to \
         spell a type name, a local binding, a method name resolved against its receiver):\n  {}",
        spurious.join("\n  ")
    );
}

/// The `..` ban itself, checked on the file rather than trusted. Field-level exhaustiveness is the
/// property the three historical bugs each needed and none had; a single `..` restored anywhere in
/// a walk silently gives it up again.
#[test]
fn the_qualifier_binds_every_field() {
    let src = include_str!("qualify.rs");
    // The walks end where the test module begins; its assertions legitimately probe one field of a
    // node with `..` and are not walks.
    let walks = src.split("mod tests {").next().expect("a source body");
    let offenders: Vec<&str> = walks
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.starts_with("//")
                && (t.ends_with("..") || t.contains(".. }") || t.contains(".. =>") || t == "..,")
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "`..` is banned in this file's AST patterns — bind every field, deliberately-unused ones \
         as `field: _`, so that adding a field to an AST node is a compile error here:\n  {}",
        offenders.join("\n  ")
    );
}

/// **The declared type is the classification.** The `Name` newtype exists so that "this position
/// holds a name the qualifier owns" is a fact about the AST rather than a row someone remembered to
/// write here, and this test pins the correspondence in both directions:
///
/// * a `Name`-typed field may not be classified as anything but [`Verdict::Name`] — a `TABLE` row
///   is welcome to add history or a caveat, but it may not overrule the type; and
/// * a field classified `Name` must *be* `Name`-typed, except for the short list below, which is
///   spelled out so that "a `String` the walk nevertheless reaches" stays a stated exception rather
///   than a habit.
///
/// Without the second half, the first would rot: someone could reclassify a `String` as a name and
/// the gate would demand a rewrite the type never sanctioned.
#[test]
fn the_declared_type_decides_which_names_qualify() {
    /// `String` fields the walk legitimately **reaches** without ever rewriting them in place.
    ///
    /// One entry, and it earns it: a member chain may spell a qualified reference, so the collapse
    /// hands the visitor the whole dotted prefix as one synthesized candidate — which is why this
    /// field's sentinel comes back. The chain's own segment strings are never assigned to; a
    /// collapse rebuilds the node. As a *field*, `Expr::Member::name` is a member name on the
    /// receiver's type, exactly like `Expr::FieldSet::field`, and typing it `Name` would claim the
    /// qualifier rewrites it.
    const REACHED_BUT_NOT_REWRITTEN: &[(&str, &str)] = &[("Expr::Member", "name")];

    let declared = declared_fields();
    let mut wrong = Vec::new();
    for (node, field, ty) in &declared {
        let is_name_typed = ty
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .any(|w| w == "Name");
        let verdict = verdict_for(node, field, ty);
        let classified_name = matches!(verdict, Some(Verdict::Name(_)));
        if is_name_typed && !classified_name {
            wrong.push(format!(
                "{node}.{field}: {ty} is a `Name`, but TABLE classifies it as {} — a row may not \
                 overrule the type",
                verdict.map_or("nothing", |v| v.label())
            ));
        }
        if classified_name
            && !is_name_typed
            && !REACHED_BUT_NOT_REWRITTEN.contains(&(node.as_str(), field.as_str()))
        {
            wrong.push(format!(
                "{node}.{field}: {ty} is classified `Name` but is not declared `Name`. If the \
                 qualifier rewrites it, change the field's type; if it only reaches it, add it to \
                 REACHED_BUT_NOT_REWRITTEN and say why"
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n  "));

    // Non-vacuity: the correspondence above is only worth checking if `Name` fields actually exist
    // and the derivation — not a leftover row — is what classifies most of them.
    let name_typed = declared
        .iter()
        .filter(|(_, _, ty)| {
            ty.split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|w| w == "Name")
        })
        .count();
    assert!(
        name_typed >= 20,
        "only {name_typed} `Name`-typed AST fields — the conversion has been undone somewhere"
    );
    let derived_not_tabled = declared
        .iter()
        .filter(|(node, field, ty)| {
            ty.split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|w| w == "Name")
                && !TABLE.iter().any(|r| r.0 == node && r.1 == field)
        })
        .count();
    assert!(
        derived_not_tabled >= 8,
        "only {derived_not_tabled} `Name` fields are classified by the type alone — if every one \
         also has a TABLE row, the derivation is never exercised and can rot"
    );
}
