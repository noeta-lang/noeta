//! Compile-time **directive expansion**: running an extension's
//! [`ExtDirective::expand`](noeta_ext_abi::registry::ExtDirective::expand) hook and splicing the
//! members it returns into the declaration the directive decorates.
//!
//! This is the seam that makes `@openapi("petstore.yaml")` mean something. A directive is the
//! language's codegen half (`@` runs at compile time; `#[…]` is the runtime-readable half), and
//! until now an extension could only *declare* one — the name resolved, the arguments were checked,
//! and then nothing happened. A hook turns the declaration into the code it implies.
//!
//! ## Why it runs at link
//!
//! Expansion belongs to linking: it is the step that turns parsed files into the one program every
//! later pass reads, and generated members have to be in that program before anything checks it.
//!
//! There are several link *entry points* — [`crate::link`], [`crate::link_with_deps`],
//! `ParsedDir::link_entry` for `noeta check`'s directory mode, and `noeta_db::linked_from` for the
//! IDE — because each owns its own `SourceId` numbering and must append expansion sources to its
//! own map. They all call [`expand_program`]: several call sites, one implementation, the same
//! shape as the shared calling-convention rules. What must not happen is a *second* decision about
//! what a directive expands to, because then the editor and the compiler would disagree about a
//! type's members.
//!
//! That is deliberately *unlike* `plan_derive`, which is a pure function every consumer re-runs.
//! Derive cannot run this early because it needs checked information (the program's user traits).
//! Expansion can, because [`DirectiveCtx`] is narrow by construction: a hook is given what the
//! directive was written with and what it was written on, never the surrounding program. Its output
//! therefore depends only on its arguments and the files it reports reading — inputs the caller can
//! key a memoized result on.
//!
//! ## What comes out
//!
//! Each expansion becomes a **real [`Source`]**, appended to the program's source map, and the
//! members are parsed out of it. Generated code therefore has true spans: a diagnostic inside a
//! generated method points at that method, in a source the editor can show, rather than at the
//! one-line directive that produced a hundred of them.
//!
//! The expansion source is the whole synthetic declaration — `class PetStore { … }`, not just its
//! body — because that is literally the text that was parsed. Showing anything else would be a
//! second rendering, free to disagree with what the compiler saw.

use noeta_ast::{Program, Stmt};
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_ext_abi::registry::{DirectiveCtx, ExtDirective, Registry};
use noeta_parser::directives::{arg_faults, attaches_to, tier_site_of};
use noeta_span::{Source, SourceId, Span};

use crate::LoadDiagnostic;

/// One successful expansion: the generated [`Source`], and the `@directive` token that generated it.
///
/// The origin is carried, not recoverable: a consumer holding only the generated source can render a
/// fault *inside* it, but cannot say where to go and change something — the author did not write that
/// text. The one span in a file they can edit is the directive itself, so it travels with the output.
/// (The IDE needs exactly this: a per-document diagnostics view filters to spans its document owns, so
/// a checker error landing in generated code is invisible unless it can be re-blamed on the directive.)
#[derive(Debug, Clone)]
pub struct ExpandedSource {
    /// The generated declaration, id'd past every source the caller already had.
    pub source: Source,
    /// The `@name` token that produced it, in the file the author wrote.
    pub origin: Span,
}

/// Everything one pass of expansion produced.
#[derive(Debug)]
pub struct Expanded {
    /// One entry per successful expansion, with ids continuing from where the caller stopped. The
    /// caller appends these to its source map and its edition map together.
    pub sources: Vec<ExpandedSource>,
    /// Every file every hook reported reading, in expansion order. This is the program's rebuild
    /// trigger: the caller registers these paths so editing one re-runs the expansion.
    pub reads: Vec<String>,
    /// Faults in the expansions themselves: a hook that returned `Err`, or one whose output did not
    /// parse.
    pub diagnostics: Vec<LoadDiagnostic>,
}

/// Run every expandable directive in `program` and splice in what the hooks return.
///
/// `sources` is the program's sources so far — a directive's file, and therefore the directory a
/// relative path argument resolves against, is found from its span — and `next_id` is the first
/// unused [`SourceId`]. Both come from the caller, which owns the numbering.
///
/// A directive that will not survive checking is **skipped silently**, not reported here: it is
/// misplaced, or its arguments do not match what it declared, and the checker reports those with
/// spans and help this pass has no business duplicating. Skipping is also what upholds the hook
/// contract — an `expand` hook only ever sees an invocation that was legal.
pub fn expand_program(
    program: &mut Program,
    source_maps: &std::collections::HashMap<SourceId, crate::qualify::QMap>,
    sources: &[Source],
    next_id: u32,
    edition: noeta_lexer::Edition,
    text_tiers: &noeta_lexer::TextTiers,
    registry: &'static Registry,
) -> Expanded {
    let mut out = Expanded {
        sources: Vec::new(),
        reads: Vec::new(),
        diagnostics: Vec::new(),
    };
    for index in 0..program.stmts.len() {
        // Plan against an immutable borrow, then splice — the only way to read a declaration's
        // decorators while intending to add to its members, and the same shape as `plan_derive`.
        for plan in plan_for(&program.stmts[index], sources, registry) {
            let id = SourceId(next_id + out.sources.len() as u32);
            // The map of the file the directive was written in: generated members are written
            // against *that* file's imports, so they qualify as its own statements did.
            let map = source_maps.get(&plan.span.source);
            let (reads, result) = run_one(&plan, id, edition, text_tiers, sources, map);
            // Reads are recorded whether the expansion succeeded or failed: a failed hook's reported
            // files are precisely what must re-trigger it when they change (a missing spec that is
            // later written), so they belong in the rebuild trigger even though nothing was spliced.
            out.reads.extend(reads);
            match result {
                Ok(done) => {
                    splice(&mut program.stmts[index], done.members);
                    out.sources.push(ExpandedSource {
                        source: done.source,
                        origin: plan.span,
                    });
                }
                Err(diagnostic) => out.diagnostics.push(*diagnostic),
            }
        }
    }
    out
}

/// Whether any statement calls for expansion — the cheap guard that keeps a program with no
/// expandable directives, which is nearly every program, off this path entirely.
pub fn has_expansions(program: &Program, registry: &'static Registry) -> bool {
    program.stmts.iter().any(|stmt| {
        stmt.decorated().is_some_and(|at| {
            at.decorators.foreign.iter().any(|f| {
                registry
                    .find_ext_directive(&f.name)
                    .is_some_and(|d| d.expand.is_some())
            })
        })
    })
}

/// One expansion waiting to run: the hook, the context to call it with, the declaration keyword its
/// members will be wrapped in, and where to blame if it fails.
struct Plan {
    directive: &'static ExtDirective,
    ctx: DirectiveCtx,
    /// The keyword of the declaration being expanded, taken from the AST rather than derived from
    /// [`DirectiveCtx::site`]: the ABI's `TierSite::Type` covers struct, class and enum alike, and
    /// an expansion on an enum may write variants — which only `enum` admits.
    keyword: &'static str,
    /// The `@name` token, so a hook's `Err` and a parse failure in its output both point at the
    /// directive the author wrote rather than at the declaration.
    span: Span,
}

/// A finished expansion. Reads are returned separately from [`run_one`], because they must survive
/// the *failure* paths too and a `Done` is only built on success.
struct Done {
    source: Source,
    members: Members,
}

/// Every expansion this statement calls for, in written order.
fn plan_for(stmt: &Stmt, sources: &[Source], registry: &'static Registry) -> Vec<Plan> {
    let Some(at) = stmt.decorated() else {
        return Vec::new();
    };
    let Some(site) = tier_site_of(at.site) else {
        return Vec::new();
    };
    let Some(keyword) = keyword_of(stmt) else {
        return Vec::new();
    };
    let mut plans = Vec::new();
    for f in &at.decorators.foreign {
        let Some(directive) = registry.find_ext_directive(&f.name) else {
            continue;
        };
        if directive.expand.is_none() {
            continue;
        }
        // The two gates the hook contract rests on, asked of the same shared predicates the checker
        // asks — not re-implemented here, so "legal" cannot come to mean two different things.
        if !attaches_to(directive.sites, at.site) || !arg_faults(directive, &f.args).is_empty() {
            continue;
        }
        plans.push(Plan {
            directive,
            ctx: DirectiveCtx {
                args: f
                    .args
                    .iter()
                    .filter(|a| a.name.is_none())
                    .map(|a| a.value.as_directive_arg())
                    .collect(),
                named: f
                    .args
                    .iter()
                    .filter_map(|a| {
                        a.name
                            .as_ref()
                            .map(|k| (k.clone(), a.value.as_directive_arg()))
                    })
                    .collect(),
                // The declaration's name as an IDENTIFIER — its last dotted segment.
                //
                // By the time expansion runs, the linker has already qualified this file's
                // declarations, so `at.name` is `shop.upstream.PetStore` in any file with a
                // `namespace`. A hook's whole job is to emit source that is spliced INTO that
                // declaration, where the only spelling in scope is the bare one — and the wrapper
                // this text is parsed inside (`struct <target> { … }`) is not even syntax with a
                // dotted name, so a qualified target made every `@`-directive on a namespaced
                // declaration fail to parse. That is every multi-file project.
                target: at
                    .name
                    .rsplit('.')
                    .next()
                    .unwrap_or(at.name.as_ref())
                    .to_string(),
                site,
                source_dir: source_dir_of(f.name_span, sources),
            },
            keyword,
            span: f.name_span,
        });
    }
    plans
}

/// The declaration keyword to wrap an expansion's members in, or `None` for a declaration that has
/// no members to add to.
fn keyword_of(stmt: &Stmt) -> Option<&'static str> {
    match stmt {
        Stmt::Struct(_) => Some("struct"),
        Stmt::Class(_) => Some("class"),
        Stmt::Enum(_) => Some("enum"),
        Stmt::Trait(_) => Some("trait"),
        _ => None,
    }
}

/// The directory of the file a span belongs to, so a relative path argument resolves against the
/// file the directive was written in rather than the process's working directory.
///
/// A source's name is a path for a file on disk and a label otherwise (the REPL, an embedded
/// snippet); a label has no parent, which correctly yields the empty string — a hook then resolves
/// against the working directory, the only meaning left when there is no file.
fn source_dir_of(span: Span, sources: &[Source]) -> String {
    sources
        .get(span.source.0 as usize)
        .and_then(|s| std::path::Path::new(s.name()).parent())
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// Run one hook and parse what it returned.
///
/// Returns the files the hook reported reading **and** the outcome, as two independent values,
/// because the reads must survive every failure path — a hook that fails because a spec is missing
/// still reported that spec, and *creating* it has to re-trigger this expansion. A `Done` is built
/// only on success, so the reads cannot ride on it.
fn run_one(
    plan: &Plan,
    id: SourceId,
    edition: noeta_lexer::Edition,
    text_tiers: &noeta_lexer::TextTiers,
    sources: &[Source],
    map: Option<&crate::qualify::QMap>,
) -> (Vec<String>, Result<Done, Box<LoadDiagnostic>>) {
    // Boxed: a `LoadDiagnostic` carries a whole `Source`, so an unboxed `Err` would make every
    // successful expansion pay for the failure case.
    let blame = |message: String| {
        Box::new(LoadDiagnostic {
            source: sources
                .get(plan.span.source.0 as usize)
                .cloned()
                .unwrap_or_else(|| sources[0].clone()),
            diagnostic: Diagnostic::error(
                DiagnosticCode::DirectiveExpansionFailed,
                plan.span,
                message,
            ),
        })
    };
    let name = plan.directive.name;
    let hook = plan.directive.expand.expect("planned only where a hook is");
    // The hook's own error carries its reads (`ExpansionError`) for exactly this reason: a failure
    // that read a file must report it, or the file's later appearance is invisible.
    let expansion = match hook(&plan.ctx) {
        Ok(expansion) => expansion,
        Err(error) => {
            return (
                error.reads,
                Err(blame(format!(
                    "`@{name}` could not expand: {}",
                    error.message
                ))),
            );
        }
    };
    // From here on the reads are the success reads, returned on every branch below — a parse fault
    // in the generated code does not un-read the spec that produced it.
    let reads = expansion.reads;

    let text = format!(
        "{} {} {{\n{}\n}}\n",
        plan.keyword,
        plan.ctx.target,
        expansion.source.trim_end()
    );
    let source = Source::new(id, display_name(&plan.ctx, plan.directive), text);
    let lexed = noeta_lexer::lex_in(&source, edition, text_tiers);
    let parsed = noeta_parser::parse_in(&source, &lexed.tokens, edition, text_tiers);
    if let Some(first) = lexed
        .diagnostics
        .iter()
        .chain(parsed.diagnostics.iter())
        .next()
    {
        // Say where in the generated text the fault is. The author cannot fix a line they did not
        // write, so the actionable facts are which expansion misbehaved and where — and since the
        // generated source is real and openable, a position in it is a real address.
        let at = source.line_col(first.span.start);
        return (
            reads,
            Err(blame(format!(
                "`@{name}` produced code that does not parse: {} (in the expansion at {}:{})",
                first.message, at.line, at.col
            ))),
        );
    }
    // Qualify before lifting the members out, while the expansion is still one statement — the
    // same rewrite the linker already applied to everything the author wrote in this file.
    //
    // Without this, generated code can only name built-ins: `Api` in a generated field would reach
    // the checker bare and resolve to nothing, and a hard-coded `para.api.Api` would be no better,
    // because the qualified identity depends on what the *consumer* called the dependency. Borrowing
    // the file's own map is the only spelling that is right in both cases.
    let mut parsed = parsed;
    if let Some(map) = map {
        for stmt in &mut parsed.program.stmts {
            crate::qualify::qualify_stmt(stmt, map);
        }
    }
    match members_of(parsed.program) {
        Some(members) => (reads, Ok(Done { source, members })),
        None => (
            reads,
            Err(blame(format!(
                "`@{name}` produced no declaration to take members from"
            ))),
        ),
    }
}

/// The name the generated source appears under — in a diagnostic, in the editor, and in
/// `noeta expand`.
///
/// It names the *cause*, not just the output: `PetStore ⟨@openapi "petstore.yaml"⟩` says which
/// declaration grew these members and which directive grew them, which is what someone who did not
/// write the generator needs in order to know where to look.
fn display_name(ctx: &DirectiveCtx, directive: &ExtDirective) -> String {
    let args: Vec<String> = ctx.args.iter().map(|a| format!("{a:?}")).collect();
    if args.is_empty() {
        format!("{} ⟨@{}⟩", ctx.target, directive.name)
    } else {
        format!("{} ⟨@{} {}⟩", ctx.target, directive.name, args.join(", "))
    }
}

/// The members lifted out of a parsed expansion, ready to join the real declaration.
#[derive(Default)]
struct Members {
    fields: Vec<noeta_ast::FieldDecl>,
    variants: Vec<noeta_ast::VariantDecl>,
    methods: Vec<noeta_ast::FnDecl>,
    trait_methods: Vec<noeta_ast::TraitMethod>,
    impls: Vec<noeta_ast::ImplBlock>,
}

/// Take the members out of the one declaration a wrapped expansion parses to.
fn members_of(program: Program) -> Option<Members> {
    match program.stmts.into_iter().next()? {
        Stmt::Struct(d) => Some(Members {
            fields: d.fields,
            methods: d.methods,
            impls: d.impls,
            ..Members::default()
        }),
        Stmt::Class(d) => Some(Members {
            fields: d.fields,
            methods: d.methods,
            impls: d.impls,
            ..Members::default()
        }),
        Stmt::Enum(d) => Some(Members {
            variants: d.variants,
            methods: d.methods,
            impls: d.impls,
            ..Members::default()
        }),
        Stmt::Trait(d) => Some(Members {
            trait_methods: d.methods,
            ..Members::default()
        }),
        _ => None,
    }
}

/// Add the generated members to the declaration, **after** the hand-written ones.
///
/// Order is the visible rule: what the author wrote comes first, so reading a type top-to-bottom
/// reads their code before the generator's, and a generated member never shifts the position of a
/// hand-written one.
fn splice(stmt: &mut Stmt, m: Members) {
    match stmt {
        Stmt::Struct(d) => {
            d.fields.extend(m.fields);
            d.methods.extend(m.methods);
            d.impls.extend(m.impls);
        }
        Stmt::Class(d) => {
            d.fields.extend(m.fields);
            d.methods.extend(m.methods);
            d.impls.extend(m.impls);
        }
        Stmt::Enum(d) => {
            d.variants.extend(m.variants);
            d.methods.extend(m.methods);
            d.impls.extend(m.impls);
        }
        Stmt::Trait(d) => d.methods.extend(m.trait_methods),
        // `plan_for` plans only for the kinds `keyword_of` names, which are exactly these.
        _ => {}
    }
}
