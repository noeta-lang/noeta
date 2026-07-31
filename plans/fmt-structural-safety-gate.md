# `noeta fmt`'s safety gate — the `Pretty` proxy, its survey, and what should replace it

Status: **proposal** (the survey and the per-field fixes have landed; the replacement has not)

`noeta fmt` promises that its output "re-parses to the same AST modulo spans" and aborts untouched otherwise. It does not check that. It checks that the output re-parses to the same **`Pretty` rendering** — the S-expression dump in `noeta-ast/src/pretty.rs` — with `@start..end` annotations erased (`noeta-fmt/src/safety.rs`).

`Pretty` is a hand-written printer, one arm per node, written for *snapshot legibility* and later pressed into service as the gate. Every field it does not render is a field the gate cannot see, and nothing forces it to render any. The safety property is therefore only as complete as someone remembered to make a debug dump.

That is not a theoretical gap. Two formatter defects have gone through it:

- **`TierBlock::attached`** — the arm destructured it away with `..`. A printer rule collapsed `@test { fn t() {…} }` into `@test fn t()`, which flips the flag; the checker branches on it (E0054's declared-site check runs only when attached), so the collapse can invent an attachment-site error the author's source does not have. Fixed at the printer end in `1e13e2007`, at the gate end here.
- **A payload-less variant pattern** — `Pattern::Variant { variant: "Ok", bindings: [] }` and `Pattern::Binding { name: "Ok" }` both rendered as `Ok`. The printer was dropping exactly those parens, rewriting `Ok() => …` into `Ok => …`. `Ok` is a catch-all binding, so every later arm goes dead — and for a prelude name it does not even compile (E0046). Found by the structural comparison described below, on two files already in the corpus. Fixed both ends here.

## The survey

Every field `Pretty` dropped, `..`-destructured, or rendered so that two semantically different values coincided. "Reachable" means the formatter re-emits the field from the AST, so a printing bug in it was undetectable; "**live**" means a printing bug in it existed.

| Field | Judgement | Why | Status |
|---|---|---|---|
| `Stmt::TierBlock::attached` | significant, **live** | The checker's E0054 site check runs only when attached | rendered `:attached` |
| `Pattern::Variant` w/ empty bindings vs `Pattern::Binding` | significant, **live** | `Ok()` matches one constructor; `Ok` matches everything | unqualified variants render `Ok()` |
| `Stmt::Binding::ty` | significant, reachable | The annotation is the boundary the value is checked against (`acc: List<int> = []`) | rendered `: T` |
| `FnDecl::ret` | significant, reachable | Every `return` is typed against it; the `?` position rule (E0012) reads its shape | rendered `: T` |
| `Param::ty` (fn + closure params) | significant, reachable | What an argument is checked against | rendered `: T` |
| `Param::default` (presence) | significant, reachable | Presence is what makes a parameter optional (arity) | rendered `=` |
| `Param::default` (expression) | significant, reachable | Two different defaults are two different programs | `(param-default …)` child |
| `Expr::Closure::ret` | significant, reachable | Explicitly discarded (`ret: _`); checked when written | rendered `: T` |
| `FieldDecl::ty` | significant, reachable | The field's declared type | rendered `: T` |
| `FieldDecl::is_public` | significant, reachable | The explicit `pub` marker slice-2 privacy enforcement reads | rendered `pub ` |
| `FieldDecl::attrs` | significant, reachable | Reaches the reflection manifest exactly as a type's attributes do | rendered `#[…]` |
| `FieldDecl::default` (expression) | significant, reachable | As `Param::default` | `(field-default …)` child |
| `EnumDecl::backing` (which type) | significant, reachable | `enum S: string` and `enum S: int` both printed `enum-backed` | rendered `: T` |
| `VariantDecl::backed_value` | significant, reachable | The backed value *is* what the variant means | `(variant-value …)` child |
| `VariantDecl::attrs` | significant, reachable | As `FieldDecl::attrs` | rendered `#[…]` |
| `ClassDecl::destructor` | significant, reachable | Not a method, so it appeared nowhere; the collector runs it | `(destruct …)` child |
| `StructDecl/ClassDecl/EnumDecl::impls` | significant, reachable | Methods are flattened into `methods`, so *which trait* a method implements lived only here | `(impl-block …)` child, members listed |
| `ImplDecl::assoc_bindings`, `ImplBlock::assoc_bindings` | significant, reachable | Resolve `Self::Name` per implementor | rendered `{N=T}` |
| `TraitDecl::is_public` | significant, reachable | Decides whether the trait is importable | rendered `pub ` |
| `TraitDecl::type_params` | significant, reachable | Decides what an `impl` must instantiate | rendered `<…>` |
| `TraitDecl::assoc_types` | significant, reachable | Each is a binding every impl must provide | `(assoc-type …)` child |
| `TraitMethod::has_default` | significant, reachable | `fn f(): int` and `fn f(): int {}` have the same (empty) body list and opposite obligations | required methods wrapped in `(required …)` |
| `MethodDirective::args` (values) | significant, reachable | Only the *arity* was printed, as `(1)`; these are the tier's knobs, read by its runner | rendered via `attr_args_str` |
| `AttrValue::List` vs `::Set` | significant, reachable | Shared one arm; `[1, 2]` and `#{1, 2}` are different values of different types | `#{…}` for a set |
| `AttrValue::Enum::args` | significant, reachable | `Status.Code(404)` rendered as `Status.Code` | payload rendered |
| `AttrValue::Struct::fields` | significant, reachable | Rendered `Type {…}` — every struct-valued argument of a type compared equal | fields rendered |
| `AttrValue::Int(1)` vs `::Float(1.0)` | significant, reachable | `Display` renders both as `1` | float via `{:?}` |
| `Expr::Closure` block body statement separator | hygiene | Statements ran together on one line, a rendering in which two statement lists can coincide | one per line |
| **Every `Span`, `*_span`** | irrelevant | Formatting shifts every byte offset by construction; the gate erases them on purpose. Note the printer *reads* spans to detect desugars (`#{…}`, `+=`, `x[k] = v`), but that is an input to printing, not a property of the program | correctly elided |
| `FnDecl::is_dev_tier` | irrelevant | Set by `activate_tiers`, never by the parser. fmt parses fresh, so it is `false` on both sides of every comparison and no source spelling can differ in it | correctly elided |
| Decorator *order* between different directives | irrelevant | `decorators_str` iterates `BuiltinDirective::ALL`, so `@derive(A) @validated` and `@validated @derive(A)` normalize together. They are the same program, and the printer canonicalizes the order anyway | correctly elided |
| `@derive(A) @derive(B)` vs `@derive(A, B)` | irrelevant | Both flatten into one `derives` vec in source order; the two spellings mean the same thing | correctly elided |
| `ObjectLit::type_name == None` (`.{`) | irrelevant | Rendered as the literal `.{`, which no type name can spell | correctly elided |
| `Pattern` scalar arms vs `Binding` | irrelevant | `Int`/`Str`/`Bool` render as literals a binding name cannot be (the lexer would not produce them) | correctly elided |
| `TypeRef` (all arms) | irrelevant | `type_ref_str` is `shape::type_source`, a total faithful re-rendering with names verbatim | correctly elided |

## Why the proxy is the wrong shape

Each row above is a patch. The next field added to the AST starts un-rendered, and nothing fails. The two live defects were both found *after* the fact — one by exploitation, one by writing a stronger comparison and running it.

Two properties are wanted, and `Pretty` has neither:

1. **Exhaustive by construction** — a field added to a node must show up in the comparison with no edit, or fail to compile.
2. **Span-blind by construction** — spans differ on every format and must be erased, but nothing *else* may be.

## The options

### A. Derived `Debug` with the span blob stripped — **landed, as a corpus property**

`Debug` is derived on every AST node, so it prints every field by construction. Erase the `Span { start: N, end: N, source: SourceId(N) }` blobs and compare the strings. That is `crates/noeta-fmt/tests/structural.rs`, which sweeps the whole `.noe` corpus in two configurations and cost ~1.9 s. **It is what found the `Ok()` defect.**

What it is not: a replacement for the in-process gate. It is still textual, so a string literal containing exactly a well-formed span blob would be stripped from it (harmlessly — identically on both sides — but the argument is now about strings rather than about structure), and it cannot express the *relaxed* gate (`ast_equal_ignoring_tier_statics`, which must ignore a tier body's `statics` while comparing everything else) without string-aware bracket matching in the middle of a `Debug` dump. It also allocates two full `Debug` renderings per file, which is fine once per corpus run and wasteful once per keystroke through the LSP.

As a **corpus property** it is exactly right, and it is standing: any future formatter change that touches a field `Pretty` does not render now fails a test, whether or not anyone thought to render it.

### B. A span normalizer + derived `PartialEq` — the recommendation

Add `noeta-ast::normalize::zero_spans(&mut Program)`: an exhaustive mutable walk that sets every `Span` to a fixed value, destructuring **without `..`** everywhere. Then the gate is:

```rust
pub fn ast_equal_modulo_spans(a: &Program, b: &Program) -> bool {
    let (mut a, mut b) = (canonical_imports(a), canonical_imports(b));
    zero_spans(&mut a);
    zero_spans(&mut b);
    a == b            // the derived PartialEq — every field, no printer
}
```

Why this is the right shape:

- **Field completeness is free.** `PartialEq` is derived on every AST node and compares every field. There is no rendering to keep up to date, and no field can be "forgotten".
- **The walk's failure mode is safe.** The only thing the walk can get wrong is *missing a span* — and a missed span makes the gate **stricter**, not blinder: fmt declines to format and leaves the file untouched. It cannot cause a silent rewrite. (The corpus test in §A would catch such a miss immediately.)
- **Adding a field is a compile error**, at the one site that must consider it, exactly as `Stmt::decorated`/`attachment_site` and `decorators_str`'s `BuiltinDirective::ALL` loop already are.
- **The relaxed gate becomes trivial**: the tier-statics relaxation is `for each TierExpr { statics.clear() }` in the same walk, behind a flag — structural, not textual.

Cost, honestly: roughly 600–800 lines of mechanical code in a new `noeta-ast/src/normalize.rs` — `Stmt` (23 variants), `Expr` (48), and ~20 structs, each destructured exhaustively. It is dull and it is large, and it is the kind of file a `..` will creep back into unless the module doc says why it must not. There is no existing AST visitor to reuse: `noeta-loader/src/qualify.rs` has the only exhaustive walk and it is a name-rewriting one (it binds `attached: _` for its own reasons).

### C. A `#[derive(ZeroSpans)]` proc macro

Same result as B with no hand-written walk: a `ZeroSpans` trait with blanket impls for `Vec<T>`/`Option<T>`/`Box<T>`/tuples and a no-op for leaves, plus a derive that walks a struct's/enum's fields. ~200 lines of macro instead of ~700 of walk, and it is genuinely exhaustive rather than exhaustive-because-someone-wrote-it-out.

The cost is a **new proc-macro crate in the workspace**, on `syn`/`quote`, in the dependency path of `noeta-ast` — which is to say, of everything. That is a real build-time and supply-surface change for one guard, and `noeta-ast`'s whole premise is "pure data, no behavior". Not obviously worth it unless a second consumer for span normalization appears (an IDE AST-diff, an incremental-reparse equality check).

## Recommendation

1. **Keep §A**, the corpus structural property. It is landed, cheap, and it already earned its keep.
2. **Do §B** as its own small arc when someone is next in `noeta-ast`. It is not urgent *now* — the survey above closed every field the formatter can currently reach, and §A stands guard over the corpus — but it is the only version of the safety property that stays true without maintenance.
3. **Do not do §C** unless a second consumer appears.

Until §B lands, the rule for `pretty.rs` is: **no `..` in a `Pretty` arm.** Bind every field, and `_`-bind the ones that are deliberately not rendered, so the decision is visible and a new field is a compile error rather than a silent hole. That is what the `TierBlock` and `Stmt::Binding` arms now do.
