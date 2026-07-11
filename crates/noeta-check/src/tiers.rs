//! Dev-tier **activation** (object-model slice 6): resolve a program's `@<tier> { … }` blocks
//! against an *active-tier set* before the checker and the backends see it.
//!
//! A tier block is co-located developer-tooling content (`test`/`bench`/`doc`/`debug`). Whether a
//! block is compiled in is the *build target*'s call (the `noeta.toml` manifest); this module is
//! the front-end mechanism a target drives. Given the resolved active set, [`activate_tiers`]:
//!
//! - **inlines** an active code-tier block's items into the top-level statement stream, so they are
//!   checked and lowered as ordinary declarations (the block is pure grouping sugar), **stamping**
//!   the block's directive args onto each lifted fn as the tier's config attribute
//!   (`@bench(iterations: N)` ⇒ `#[Bench(iterations: N)]`; a per-fn attribute wins) — so the
//!   ordinary attribute construction gate validates them and the runner reads one place;
//! - **drops** an inactive block (it never reaches the checker or the IR — the strip is by
//!   construction, no DCE pass);
//! - **validates** every block's tier name against [`BUILTIN_TIERS`] (an unknown tier is an
//!   `E0036`, active or not — a typo must surface, not silently vanish); and
//! - **discovers** the `@test` fns it activated, so the runner finds them without a second walk.
//!
//! The *default* program path (`lang run`, the conformance differential) runs with an **empty**
//! active set and does **not** call this — those paths keep stripping inactive blocks at lowering
//! (`noeta_ir::lower`), and the checker keeps validating tier names in place. Only the test runner
//! activates a tier, so the differential is untouched by construction. The two E0036 sources (this
//! module and the checker's in-place arm) share [`unknown_tier_diagnostic`], so they never drift.

use noeta_ast::reflect::{TIER_ATTR_BENCH, TIER_ATTR_DOC};
use noeta_ast::{AttrArg, Attribute, Program, Stmt};
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_span::Span;

/// The **tier-knob attribute** of a built-in tier — the prelude `@attribute` struct its directive
/// arguments construct (`@bench(iterations: N)` ⇒ `#[Bench(iterations: N)]` stamped onto each
/// contained fn; a per-fn attribute wins over the block's). `None` for a tier with no knobs, which
/// therefore accepts no arguments. One schema source: the attribute's registered fields drive
/// validation (the ordinary construction gate) and the runner's reads alike. A *declared* tier's
/// config attribute lives in the [`TierRegistry`] instead (its `@tier(…, config: T)` directive).
pub fn tier_config_attribute(tier: &str) -> Option<&'static str> {
    match tier {
        "bench" => Some(TIER_ATTR_BENCH),
        _ => None,
    }
}

/// A tier brought into existence by a `@tier(name[, config: T]) fn runner(…)` declaration
/// (tier-providers T2) — the program-declared counterpart of a [`BUILTIN_TIERS`] entry.
#[derive(Debug, Clone, PartialEq)]
pub struct DeclaredTier {
    /// The tier name consumers write as `@<name> { … }`.
    pub name: String,
    /// The knob-attribute type from `config: T`, if the tier has knobs.
    pub config: Option<String>,
    /// The runner fn's (possibly link-qualified) name — what dispatch invokes with the roots.
    pub runner: String,
    /// The `@tier` directive's span, for diagnostics.
    pub span: Span,
}

/// The tier name-space a program sees: the built-in four plus every `@tier` declaration in the
/// (linked) program — imported packages' declarations included, since linking merges their
/// modules. The one lookup activation, the checker's in-place `TierBlock` arm, and the CLI's
/// runner dispatch all resolve against.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TierRegistry {
    declared: std::collections::HashMap<String, DeclaredTier>,
}

impl TierRegistry {
    /// Collect every `@tier` declaration from `program`'s top-level fns. Duplicates keep the first
    /// (the checker reports the collision as E0051; resolution just stays stable).
    pub fn collect(program: &Program) -> TierRegistry {
        let mut declared = std::collections::HashMap::new();
        for stmt in &program.stmts {
            if let Stmt::Fn(f) = stmt
                && let Some(t) = &f.tier
            {
                declared
                    .entry(t.name.clone())
                    .or_insert_with(|| DeclaredTier {
                        name: t.name.clone(),
                        config: t.config.as_ref().map(|(n, _)| n.clone()),
                        runner: f.name.clone(),
                        span: t.span,
                    });
            }
        }
        TierRegistry { declared }
    }

    /// Whether `tier` names a known tier — built-in or declared.
    pub fn is_known(&self, tier: &str) -> bool {
        BUILTIN_TIERS.contains(&tier) || self.declared.contains_key(tier)
    }

    /// The declared tier named `tier`, if any (`None` for a built-in).
    pub fn declared(&self, tier: &str) -> Option<&DeclaredTier> {
        self.declared.get(tier)
    }

    /// The tier's config attribute — the built-in mapping for the built-in four, the `@tier`
    /// directive's `config:` for a declared tier.
    pub fn config_attribute<'a>(&'a self, tier: &str) -> Option<&'a str> {
        tier_config_attribute(tier)
            .or_else(|| self.declared.get(tier).and_then(|d| d.config.as_deref()))
    }

    /// The `E0037` for directive arguments on a tier that has no knob attribute (`@test(x)` —
    /// `test` takes no arguments), or `None` when the args are acceptable at this level. Args on a
    /// knob-carrying tier are *not* validated here — they construct the tier's config attribute,
    /// checked by the ordinary attribute construction gate (in place by the checker's `TierBlock`
    /// arm on the default path; on the stamped fns when activated).
    pub fn knobless_args_diagnostic(&self, tier: &str, args: &[AttrArg]) -> Option<Diagnostic> {
        if self.config_attribute(tier).is_some() {
            return None;
        }
        let span = args.first().map(|a| a.span)?;
        Some(
            Diagnostic::error(
                DiagnosticCode::InvalidDirectiveArgument,
                span,
                format!("tier `@{tier}` takes no arguments"),
            )
            .with_help("a tier's knobs come from its config attribute (`@bench`'s is `Bench { iterations: int }`; a declared tier's is its `@tier(…, config: T)`)"),
        )
    }
}

/// The attribute a tier block's directive args construct, stamped at the block's spans — the
/// desugar `@bench(iterations: N)` ⇒ `#[Bench(iterations: N)]`. Shared by the stamping in
/// [`activate_tiers`] and the checker's in-place validation of an inactive block, so the two paths
/// check the identical construction.
pub fn synthesized_config_attr(attr_name: &str, args: &[AttrArg], tier_span: Span) -> Attribute {
    Attribute {
        name: attr_name.to_string(),
        name_span: tier_span,
        args: args.to_vec(),
        span: tier_span,
    }
}

/// Dedent a verbatim `@doc` body for presentation: drop leading/trailing blank lines, then strip
/// the common leading whitespace shared by all non-blank lines (so text written indented inside
/// `@doc { … }` renders flush-left). Blank lines do not count toward the common indent and are
/// emitted empty. The lexer captured the body exactly; this is purely presentation formatting —
/// shared by `lang doc` and the LSP's hover so both render identically. The AST's bytes are
/// untouched.
pub fn dedent_doc(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    // Trim leading and trailing blank lines.
    let start = lines.iter().position(|l| !l.trim().is_empty());
    let Some(start) = start else {
        return String::new();
    };
    let end = lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .unwrap_or(start);
    let body = &lines[start..=end];

    let indent = body
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    body.iter()
        .map(|l| {
            if l.trim().is_empty() {
                ""
            } else {
                &l[indent..]
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The dev-tiers the language ships built in. The tier name-space is **open** (tier-providers
/// T2/T3): a program (or its dependencies) adds names with `@tier` declarations, and the
/// [`TierRegistry`] resolves against built-ins ∪ declared — a name in neither is an `E0036` (a
/// typo must not silently vanish). These four stay hardcoded until the follow-up arc ports them
/// to std's own `@tier` declarations (dogfooding the provider mechanism).
pub const BUILTIN_TIERS: &[&str] = &["test", "bench", "doc", "debug"];

/// The `E0036 UnknownTier` diagnostic for a `@<tier>` whose name is not a built-in tier. Shared by
/// [`activate_tiers`] and the checker's in-place `TierBlock` arm so the two never diverge.
pub fn unknown_tier_diagnostic(tier: &str, span: Span) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::UnknownTier,
        span,
        format!("unknown dev-tier `@{tier}`"),
    )
    .with_help(format!(
        "the built-in tiers are {}",
        BUILTIN_TIERS
            .iter()
            .map(|t| format!("`@{t}`"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// A code-tier `fn` surfaced by activation — a root the matching runner invokes by name (a `@test`
/// fn for `lang test`, a `@bench` fn for `lang bench`).
#[derive(Debug, Clone, PartialEq)]
pub struct TierFn {
    /// The fn's name, used to invoke it.
    pub name: String,
    /// Where it is declared (for the runner's report).
    pub span: Span,
    /// The `#[...]` data attributes on the fn — test metadata (`#[Skip]`, `#[Name("…")]`,
    /// `#[Group("…")]`, `#[Data([…])]`) and tier knobs (`#[Bench(iterations: N)]`, whether written
    /// per-fn or stamped from the block's `@bench(…)` directive args). The one place a runner reads
    /// configuration from. Empty for an unannotated fn in a bare block.
    pub attrs: Vec<Attribute>,
}

/// A `@doc { … }` text-tier block's verbatim body, surfaced for extraction by `lang doc` (slice
/// 6f). The text is the raw source between the braces, captured un-parsed by the lexer.
#[derive(Debug, Clone, PartialEq)]
pub struct DocBlock {
    /// The verbatim body text.
    pub text: String,
    /// The whole `@doc { … }` span, for the extractor's source-location header.
    pub span: Span,
    /// What the block documents — resolved by adjacency (see [`resolve_docs`]).
    pub target: DocTarget,
}

/// What a `@doc { … }` block documents, resolved purely by **position** (no new syntax):
#[derive(Debug, Clone, PartialEq)]
pub enum DocTarget {
    /// A block immediately followed by a declaration (`fn`/`struct`/`class`/`enum`) in the same
    /// source: it documents that declaration. Tooling keys off `name_span` (stable through
    /// activation's inlining), display uses `name`.
    Decl { name: String, name_span: Span },
    /// The first *non-attached* doc block of its source file — the module's doc. In practice a
    /// module doc sits above the file's `use` header (or stands alone), so adjacency
    /// disambiguates naturally: prose above a declaration belongs to the declaration, prose above
    /// anything else opens the module.
    Module,
    /// Any later non-attached block — free-floating section prose between declarations.
    Section,
}

/// Resolve every top-level `@doc { … }` block, in source order, with its adjacency-resolved
/// [`DocTarget`]. `@doc` is a *declaration-position* text tier, so a top-level walk is the whole
/// story; on a normal run the bodies never reach the checker or lowering (stripped like any
/// inactive tier). Works from a bare parse — no type-checking — so docs extract from
/// work-in-progress code. Consumers: `lang doc` (extraction with symbol association), the LSP's
/// hover, and [`activate_tiers`]'s `#[Doc]` stamping when the `doc` tier is live.
pub fn resolve_docs(program: &Program) -> Vec<DocBlock> {
    // Sources that already produced a non-attached doc block — the first is the module doc, the
    // rest are sections.
    let mut module_doc_seen = std::collections::HashSet::new();
    let mut docs = Vec::new();
    for (i, stmt) in program.stmts.iter().enumerate() {
        let Stmt::TierBlock {
            tier,
            doc_text: Some(text),
            span,
            ..
        } = stmt
        else {
            continue;
        };
        if tier != "doc" {
            continue;
        }
        let decl_target = program.stmts.get(i + 1).and_then(|next| {
            if next.span().source != span.source {
                return None;
            }
            let (name, name_span) = match next {
                Stmt::Fn(d) => (&d.name, d.name_span),
                Stmt::Struct(d) => (&d.name, d.name_span),
                Stmt::Class(d) => (&d.name, d.name_span),
                Stmt::Enum(d) => (&d.name, d.name_span),
                _ => return None,
            };
            Some(DocTarget::Decl {
                name: name.clone(),
                name_span,
            })
        });
        let target = decl_target.unwrap_or_else(|| {
            if module_doc_seen.insert(span.source) {
                DocTarget::Module
            } else {
                DocTarget::Section
            }
        });
        docs.push(DocBlock {
            text: text.clone(),
            span: *span,
            target,
        });
    }
    docs
}

/// The result of resolving a program's tier blocks against an active set.
#[derive(Debug, Clone, PartialEq)]
pub struct Activated {
    /// The program with active tier blocks inlined and inactive ones removed — ready to check and
    /// lower as if the tier blocks had never been a distinct form.
    pub program: Program,
    /// The `@test` fns activated by this resolution, in source order (roots for `lang test`).
    pub tests: Vec<TierFn>,
    /// The `@bench` fns activated by this resolution, in source order (roots for `lang bench`).
    pub benches: Vec<TierFn>,
    /// The roots of every activated **declared** tier (tier-providers T2), keyed by tier name — a
    /// `@fuzz { fn f() }` block's fns under `"fuzz"`, for the dispatching runner.
    pub custom: std::collections::BTreeMap<String, Vec<TierFn>>,
    /// The tier name-space this resolution ran against — built-ins plus the program's `@tier`
    /// declarations. Surfaced so the caller (the CLI's dispatch) resolves the runner from the same
    /// registry activation validated with.
    pub registry: TierRegistry,
    /// `E0036` for any block naming an unknown tier (active or not).
    pub diagnostics: Vec<Diagnostic>,
}

/// Resolve `program`'s `@<tier> { … }` blocks against `active` (the set of live tier names),
/// **everywhere they appear** — top-level (a `@test` block of declarations) and nested in statement
/// position (a `@debug { … }` block inside a fn/method body or a control-flow branch). Active blocks
/// are inlined in place; inactive blocks are dropped; every block's name is validated. The `@test`
/// fns among the activated *top-level* blocks are collected as roots the runner invokes.
pub fn activate_tiers(program: &Program, active: &[&str]) -> Activated {
    let mut roots = Roots::default();
    let mut diagnostics = Vec::new();
    // The tier name-space: built-ins ∪ the program's own `@tier` declarations (imported packages'
    // included — the linked program carries their decls). Unknown-name validation, config-attr
    // stamping, and root collection all resolve against it.
    let registry = TierRegistry::collect(program);
    // With the `doc` tier live, a declaration-attached `@doc` block (adjacency-resolved on the
    // *input* program, before its blocks are gone) stamps `#[Doc("…")]` onto its declaration —
    // the text tier's counterpart of `@bench`'s knob stamping, giving runtime docstrings via
    // `attributes_of`. Keyed by the declaration's name-span, which survives inlining.
    let doc_stamps: std::collections::HashMap<Span, String> = if active.contains(&"doc") {
        resolve_docs(program)
            .into_iter()
            .filter_map(|d| match d.target {
                DocTarget::Decl { name_span, .. } => Some((name_span, d.text)),
                _ => None,
            })
            .collect()
    } else {
        std::collections::HashMap::new()
    };
    // The top-level statement list collects roots (a `@test`/`@bench` block's fns are runnable roots
    // only here — a tier block nested in a fn body holds inline code, not roots).
    let mut stmts = resolve_block(
        &program.stmts,
        active,
        &registry,
        &mut diagnostics,
        &mut roots,
        true,
    );
    if !doc_stamps.is_empty() {
        for stmt in &mut stmts {
            let (name_span, attrs) = match stmt {
                Stmt::Fn(d) => (d.name_span, &mut d.attrs),
                Stmt::Struct(d) => (d.name_span, &mut d.attrs),
                Stmt::Class(d) => (d.name_span, &mut d.attrs),
                Stmt::Enum(d) => (d.name_span, &mut d.attrs),
                _ => continue,
            };
            if let Some(text) = doc_stamps.get(&name_span)
                && !attrs.iter().any(|a| a.name == TIER_ATTR_DOC)
            {
                attrs.push(Attribute {
                    name: TIER_ATTR_DOC.to_string(),
                    name_span,
                    args: vec![AttrArg {
                        name: Some("text".to_string()),
                        value: noeta_ast::AttrValue::Str(text.clone()),
                        span: name_span,
                    }],
                    span: name_span,
                });
            }
        }
    }
    Activated {
        program: Program {
            stmts,
            span: program.span,
        },
        tests: roots.tests,
        benches: roots.benches,
        custom: roots.custom,
        registry,
        diagnostics,
    }
}

/// The runnable roots a top-level activation collects, partitioned by tier — `@test` fns for
/// `lang test`, `@bench` fns for `lang bench`.
#[derive(Default)]
struct Roots {
    tests: Vec<TierFn>,
    benches: Vec<TierFn>,
    /// Declared-tier roots, keyed by tier name.
    custom: std::collections::BTreeMap<String, Vec<TierFn>>,
}

/// Resolve the tier blocks in one statement list. A `@<tier> { … }` is validated, then inlined (its
/// items spliced in place, each recursively resolved) when its tier is active or dropped when not;
/// every other statement is left in place with its *own* nested statement lists resolved
/// ([`resolve_children`]). `collect_tests` is true only for the program's top-level list, so only a
/// top-level `@test` block's fns become runnable roots.
fn resolve_block(
    stmts: &[Stmt],
    active: &[&str],
    registry: &TierRegistry,
    diagnostics: &mut Vec<Diagnostic>,
    roots: &mut Roots,
    collect_roots: bool,
) -> Vec<Stmt> {
    let mut out = Vec::with_capacity(stmts.len());
    for stmt in stmts {
        let Stmt::TierBlock {
            tier,
            tier_span,
            args,
            items,
            ..
        } = stmt
        else {
            out.push(resolve_children(stmt, active, registry, diagnostics));
            continue;
        };

        if !registry.is_known(tier) {
            diagnostics.push(unknown_tier_diagnostic(tier, *tier_span));
        } else if let Some(d) = registry.knobless_args_diagnostic(tier, args) {
            // Directive args on a knob-less tier (`@test(x)`) — E0037. A knob-carrying tier's args
            // are validated as the stamped attribute's construction, by the checker, below.
            diagnostics.push(d);
        }
        if !active.contains(&tier.as_str()) {
            // Inactive (including an unknown tier): stripped, never reaches the checker or the IR.
            continue;
        }

        // Active tier: resolve the items (so a tier block nested among them, and each item's own
        // body, are handled), then splice them in place. The items are spliced at *this* level, so
        // `collect_roots` carries through unchanged. Each lifted `fn` is marked `is_dev_tier` so the
        // checker grants it white-box access to the module's private fields (slice 6d), and the
        // block's directive args are stamped onto it as the tier's config attribute
        // (`@bench(iterations: N)` ⇒ `#[Bench(iterations: N)]`) unless the fn already carries one —
        // a per-fn attribute wins. The checker then validates the stamp through the ordinary
        // attribute construction gate, and the runner reads it off the fn's `attrs`; a top-level
        // `@test`/`@bench` block's fns are also recorded as roots.
        let config_attr = registry
            .config_attribute(tier)
            .filter(|_| !args.is_empty())
            .map(str::to_string);
        let resolved = resolve_block(items, active, registry, diagnostics, roots, collect_roots);
        for mut item in resolved {
            if let Stmt::Fn(decl) = &mut item {
                decl.is_dev_tier = true;
                if let Some(attr_name) = config_attr.as_deref()
                    && !decl.attrs.iter().any(|a| a.name == attr_name)
                {
                    decl.attrs
                        .push(synthesized_config_attr(attr_name, args, *tier_span));
                }
                if collect_roots {
                    let sink = match tier.as_str() {
                        "test" => Some(&mut roots.tests),
                        "bench" => Some(&mut roots.benches),
                        // A declared tier's fns are roots for its runner's dispatch.
                        name if registry.declared(name).is_some() => {
                            Some(roots.custom.entry(name.to_string()).or_default())
                        }
                        _ => None,
                    };
                    if let Some(sink) = sink {
                        sink.push(TierFn {
                            name: decl.name.clone(),
                            span: decl.name_span,
                            attrs: decl.attrs.clone(),
                        });
                    }
                }
            }
            out.push(item);
        }
    }
    out
}

/// Rewrite a non-tier statement's own nested statement lists (control-flow branches, loop and
/// fn/method bodies, a class destructor), resolving any tier blocks within. Nested lists never
/// collect tests (`collect_tests = false`). Statements with no nested statements are returned
/// unchanged. Tier blocks live only in statement position, so there is no need to descend into
/// expressions (closures and `match`/`if` *expressions* are expression-bodied).
fn resolve_children(
    stmt: &Stmt,
    active: &[&str],
    registry: &TierRegistry,
    diagnostics: &mut Vec<Diagnostic>,
) -> Stmt {
    let mut stmt = stmt.clone();
    let block = |stmts: &[Stmt], diags: &mut Vec<Diagnostic>| -> Vec<Stmt> {
        // Nested statement lists never produce runnable roots (`collect_roots = false`); the sink is
        // a throwaway.
        let mut sink = Roots::default();
        resolve_block(stmts, active, registry, diags, &mut sink, false)
    };
    match &mut stmt {
        Stmt::If {
            then_body,
            else_body,
            ..
        } => {
            *then_body = block(then_body, diagnostics);
            if let Some(eb) = else_body {
                *eb = block(eb, diagnostics);
            }
        }
        Stmt::For { body, .. } | Stmt::While { body, .. } => {
            *body = block(body, diagnostics);
        }
        Stmt::Fn(decl) => decl.body = block(&decl.body, diagnostics),
        Stmt::Class(c) => {
            for m in &mut c.methods {
                m.body = block(&m.body, diagnostics);
            }
            for im in &mut c.impls {
                for m in &mut im.methods {
                    m.body = block(&m.body, diagnostics);
                }
            }
            if let Some(d) = &mut c.destructor {
                *d = block(d, diagnostics);
            }
        }
        Stmt::Struct(s) => {
            for m in &mut s.methods {
                m.body = block(&m.body, diagnostics);
            }
            for im in &mut s.impls {
                for m in &mut im.methods {
                    m.body = block(&m.body, diagnostics);
                }
            }
        }
        Stmt::Enum(en) => {
            for m in &mut en.methods {
                m.body = block(&m.body, diagnostics);
            }
            for im in &mut en.impls {
                for m in &mut im.methods {
                    m.body = block(&m.body, diagnostics);
                }
            }
        }
        Stmt::Impl(im) => {
            for m in &mut im.methods {
                m.body = block(&m.body, diagnostics);
            }
        }
        _ => {}
    }
    stmt
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_lexer::lex;
    use noeta_parser::parse;
    use noeta_span::{Source, SourceId};

    fn parse_program(text: &str) -> Program {
        let source = Source::new(SourceId::FIRST, "test.noe", text.to_string());
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        assert!(
            lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
            "fixture must parse cleanly"
        );
        parsed.program
    }

    /// An active `@test` block inlines its fns as top-level decls and surfaces them as tests; the
    /// program's own declarations are preserved and the `@test` *block* form is gone.
    #[test]
    fn active_test_block_inlines_and_discovers() {
        let program = parse_program(
            "fn add(a: int, b: int): int { return a + b; }\n\
             @test {\n\
                 fn adds() { assert(add(1, 2) == 3); }\n\
                 fn more() { assert(add(2, 2) == 4); }\n\
             }\n",
        );
        let out = activate_tiers(&program, &["test"]);
        assert!(out.diagnostics.is_empty());
        assert_eq!(
            out.tests
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["adds", "more"]
        );
        // `add` + the two inlined test fns — and no `TierBlock` survives.
        assert_eq!(out.program.stmts.len(), 3);
        assert!(
            !out.program
                .stmts
                .iter()
                .any(|s| matches!(s, Stmt::TierBlock { .. }))
        );
    }

    /// With the tier inactive (a normal run / `lang test` over a non-`test` tier) the block is
    /// dropped entirely: no inlining, no tests, and nothing left in the stream.
    #[test]
    fn inactive_test_block_is_stripped() {
        let program = parse_program(
            "fn add(a: int, b: int): int { return a + b; }\n\
             @test { fn adds() { assert(add(1, 2) == 3); } }\n",
        );
        let out = activate_tiers(&program, &[]);
        assert!(out.diagnostics.is_empty());
        assert!(out.tests.is_empty());
        assert_eq!(out.program.stmts.len(), 1);
    }

    /// A `@tier`-declared tier opens the name space: its blocks activate (no E0036), block knobs
    /// stamp its config attribute, and its fns collect as roots under the tier's name in
    /// `Activated.custom`.
    #[test]
    fn a_declared_tier_activates_and_collects_custom_roots() {
        let program = parse_program(
            "@attribute(Function)\n\
             struct Fuzz { cases: int }\n\
             @tier(fuzz, config: Fuzz)\n\
             fn run_fuzz(roots: List<TierRoot>): void { return; }\n\
             @fuzz(cases: 9) { fn probe(): void { return; } }\n",
        );
        let out = activate_tiers(&program, &["fuzz"]);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let tier = out.registry.declared("fuzz").expect("declared");
        assert_eq!(tier.runner, "run_fuzz");
        assert_eq!(tier.config.as_deref(), Some("Fuzz"));
        let roots = &out.custom["fuzz"];
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "probe");
        // The block knob stamped the declared config attribute.
        assert!(roots[0].attrs.iter().any(|a| a.name == "Fuzz"));
        // The activated program (stamp included) type-checks through the ordinary gates.
        assert!(crate::check_all(&out.program).diagnostics.is_empty());
        // Inactive: the block strips like any tier.
        let stripped = activate_tiers(&program, &[]);
        assert!(stripped.custom.is_empty());
        assert!(stripped.diagnostics.is_empty());
    }

    /// `resolve_docs` adjacency: a file-leading `@doc` is the module doc (Python-docstring rule),
    /// a block immediately followed by a declaration attaches to it, and anything else is a
    /// free-floating section.
    #[test]
    fn doc_blocks_resolve_module_decl_and_section_targets() {
        let program = parse_program(
            "@doc { The module. }\n\
             @doc { Adds. }\n\
             fn add(a: int, b: int): int { return a + b; }\n\
             @doc { A section. }\n\
             x = 1;\n",
        );
        let docs = resolve_docs(&program);
        assert_eq!(docs.len(), 3);
        assert_eq!(docs[0].target, DocTarget::Module);
        assert!(
            matches!(&docs[1].target, DocTarget::Decl { name, .. } if name == "add"),
            "{:?}",
            docs[1].target
        );
        assert_eq!(docs[2].target, DocTarget::Section);
    }

    /// With the `doc` tier live, activation stamps a declaration-attached block as `#[Doc(text:
    /// "…")]` on that declaration (runtime docstrings); with it inactive nothing is stamped and the
    /// blocks strip as before, so production carries no doc text.
    #[test]
    fn active_doc_tier_stamps_the_doc_attribute() {
        let program = parse_program(
            "@doc { Adds two ints. }\n\
             fn add(a: int, b: int): int { return a + b; }\n",
        );
        let doc_attr_of = |activated: &Activated| {
            activated.program.stmts.iter().find_map(|s| match s {
                Stmt::Fn(d) => d.attrs.iter().find(|a| a.name == TIER_ATTR_DOC).cloned(),
                _ => None,
            })
        };
        let active = activate_tiers(&program, &["doc"]);
        let attr = doc_attr_of(&active).expect("Doc attr stamped");
        assert!(
            matches!(&attr.args[0].value, noeta_ast::AttrValue::Str(t) if t.contains("Adds two ints")),
            "{:?}",
            attr.args
        );
        // The stamped attribute passes the ordinary construction gate.
        assert!(crate::check_all(&active.program).diagnostics.is_empty());
        // Inactive: no stamp, block stripped.
        let inactive = activate_tiers(&program, &[]);
        assert!(doc_attr_of(&inactive).is_none());
        assert_eq!(inactive.program.stmts.len(), 1);
    }

    /// A `@bench(iterations: N)` block's directive args are stamped onto each contained fn as the
    /// `Bench` config attribute (the desugar), and a fn that already carries its own `#[Bench(…)]`
    /// keeps it — the per-fn attribute wins over the block's.
    #[test]
    fn bench_block_args_stamp_the_config_attribute() {
        let program = parse_program(
            "@bench(iterations: 1000) {\n\
                 fn plain(): void { return; }\n\
                 #[Bench(iterations: 5)]\n\
                 fn tuned(): void { return; }\n\
             }\n",
        );
        let out = activate_tiers(&program, &["bench"]);
        assert!(out.diagnostics.is_empty());
        let knob = |name: &str| {
            let bench = out.benches.iter().find(|b| b.name == name).expect(name);
            let attr = bench
                .attrs
                .iter()
                .find(|a| a.name == TIER_ATTR_BENCH)
                .expect("Bench attr");
            attr.args[0].value.clone()
        };
        assert_eq!(knob("plain"), noeta_ast::AttrValue::Int(1000));
        assert_eq!(knob("tuned"), noeta_ast::AttrValue::Int(5));
        // The stamped attribute type-checks through the ordinary construction gate.
        assert!(crate::check_all(&out.program).diagnostics.is_empty());
    }

    /// The stamped construction is validated like any attribute: a wrong-typed knob
    /// (`@bench(iterations: true)`) is rejected by the construction gate on the activated program,
    /// and the same block fails `check_all` in place on the default (non-activated) path — the two
    /// paths reject identically.
    #[test]
    fn bad_bench_arg_fails_both_paths() {
        let program = parse_program("@bench(iterations: true) { fn b(): void { return; } }\n");
        let activated = activate_tiers(&program, &["bench"]);
        assert!(activated.diagnostics.is_empty(), "stamping itself is clean");
        assert!(
            !crate::check_all(&activated.program).diagnostics.is_empty(),
            "activated path must reject the stamped construction"
        );
        assert!(
            !crate::check_all(&program).diagnostics.is_empty(),
            "default path must reject the in-place block args"
        );
    }

    /// Directive args on a knob-less tier are an E0037 (`@test` takes no arguments), from both the
    /// activation path and the checker's in-place arm.
    #[test]
    fn args_on_a_knobless_tier_are_e0037() {
        let program = parse_program("@test(1) { fn t(): void { return; } }\n");
        let out = activate_tiers(&program, &["test"]);
        assert_eq!(out.diagnostics.len(), 1);
        assert_eq!(
            out.diagnostics[0].code,
            DiagnosticCode::InvalidDirectiveArgument
        );
        let in_place = crate::check_all(&program).diagnostics;
        assert!(
            in_place
                .iter()
                .any(|d| d.code == DiagnosticCode::InvalidDirectiveArgument),
            "in-place arm must also reject: {in_place:?}"
        );
    }

    /// White-box (slice 6d): a private `class` field is visible inside an active dev-tier (`@test`)
    /// fn body — read, write, and construct — so a white-box test type-checks with no E0035. With
    /// the tier inactive the block is stripped, so the bare program checks clean too.
    #[test]
    fn dev_tier_fn_gets_white_box_field_access() {
        let program = parse_program(
            "class Account { mut balance: int  fn new(b: int): Account { return Account { balance: b }; } }\n\
             @test fn touches(): void { mut a = Account { balance: 0 }; a.balance = 5; assert(a.balance == 5); }\n",
        );
        let active = crate::check_all(&activate_tiers(&program, &["test"]).program);
        assert!(
            active.diagnostics.is_empty(),
            "white-box dev-tier fn must not raise E0035: {:?}",
            active.diagnostics
        );
        let inactive = crate::check_all(&activate_tiers(&program, &[]).program);
        assert!(inactive.diagnostics.is_empty());
    }

    /// The white-box relaxation is **scoped**: ordinary same-module code (not a dev-tier fn) still
    /// cannot read a private field — it is an E0035, exactly as before slice 6d.
    #[test]
    fn ordinary_fn_cannot_read_private_field() {
        let program = parse_program(
            "class Account { balance: int  fn new(b: int): Account { return Account { balance: b }; } }\n\
             fn reads(): int { a = Account.new(1); return a.balance; }\n",
        );
        let diags = crate::check_all(&program).diagnostics;
        assert!(
            diags.iter().any(|d| d.code == DiagnosticCode::PrivateField),
            "ordinary code reading a private field must still be E0035: {diags:?}"
        );
    }

    /// An unknown tier is an `E0036` whether or not it would be active, and its block is dropped.
    #[test]
    fn unknown_tier_reports_e0036() {
        let program = parse_program("@tset { fn x() { echo \"hi\"; } }\n");
        let out = activate_tiers(&program, &["test"]);
        assert_eq!(out.diagnostics.len(), 1);
        assert_eq!(out.diagnostics[0].code, DiagnosticCode::UnknownTier);
        assert!(out.tests.is_empty());
        assert!(out.program.stmts.is_empty());
    }

    /// The number of `echo` statements anywhere inside a fn body (recursively) — a proxy for how
    /// much of a `@debug` block survived activation.
    fn echoes_in_fn(stmt: &Stmt) -> usize {
        fn count(stmts: &[Stmt]) -> usize {
            stmts
                .iter()
                .map(|s| match s {
                    Stmt::Echo { .. } => 1,
                    Stmt::For { body, .. } | Stmt::While { body, .. } => count(body),
                    _ => 0,
                })
                .sum()
        }
        match stmt {
            Stmt::Fn(decl) => count(&decl.body),
            _ => 0,
        }
    }

    /// A `@debug { … }` block *nested in a fn body* (statement position) is resolved recursively:
    /// inlined in place when `debug` is active, stripped when not. (The top-level `@test` resolution
    /// is not the only one — activation reaches inside bodies.)
    #[test]
    fn nested_debug_block_is_resolved_in_place() {
        let program = parse_program(
            "fn f(x: int): void {\n\
                 @debug { echo \"dbg ${x}\"; }\n\
                 echo \"always\";\n\
             }\n",
        );
        // Inactive: only the unconditional `echo` survives in the body.
        let stripped = activate_tiers(&program, &[]);
        assert_eq!(echoes_in_fn(&stripped.program.stmts[0]), 1);
        // Active: the `@debug` echo is inlined too — two echoes in the body.
        let active = activate_tiers(&program, &["debug"]);
        assert_eq!(echoes_in_fn(&active.program.stmts[0]), 2);
        // An active nested `@debug` block does not produce test roots.
        assert!(active.tests.is_empty());
    }
}
