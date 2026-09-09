//! The static function-level **call graph**: which function uses which, with call
//! sites — the structural skeleton a role-aware trace walks (`noeta mcp`'s `trace` tool) and a
//! future LSP call hierarchy can serve.
//!
//! Built as a **join over the existing indices**, not a new resolver: [`DefUse`](crate::resolve::
//! DefUse) already records every value-identifier use → definition (function references included,
//! cross-file over the merged program), its member occurrences record every `receiver.member`
//! access, and the checker's `expr_types` resolve receivers to nominal types for method targets.
//! A function-declaration inventory turns "a use at this span" into "an edge from its enclosing
//! function". The inventory reaches every function-like declaration the program has: top-level
//! `fn`s, struct/class/enum methods, standalone `impl Trait for T` methods, a trait's default
//! methods, everything a tier block declares (qualified like its top-level siblings), and `fn`s
//! nested in another function's body (named `<enclosing>.<name>`).
//!
//! Honesty over completeness: what static analysis cannot resolve is *labeled*, never guessed. A
//! call whose target the graph holds is a `function` edge; a method on a built-in or extern type
//! (`xs.len()`) and a call into an imported module (`math.sqrt`) are `external`; a call through a
//! closure-valued binding, a member a type does not declare, and a receiver naming no module the
//! program imports are `dynamic`. Every syntactic call leaves exactly one edge under one of those
//! three labels. An edge is a `call` when the use is followed by `(` in source, otherwise a
//! `reference` (the function passed as a value — a handler registration or callback, still part of
//! the flow a trace should follow).

use std::collections::{HashMap, HashSet};

use noeta_ast::reflect::TypeRepr;
use noeta_ast::{FnDecl, Program, Stmt};
use noeta_span::{SourceId, Span};

use crate::resolve::{DefUse, MemberTable};

/// One function-like node: a top-level `fn` or a method (named `Type.method`, the reflection
/// index's target convention, so role bindings join by name).
#[derive(Debug, Clone)]
pub struct FnNode {
    pub name: String,
    /// The declared name's span (what `DefUse` definitions resolve to).
    pub name_span: Span,
    /// The whole declaration's span — the containment range that assigns call sites to this
    /// function.
    pub decl_span: Span,
    /// True for a method (declared inside a type or `impl`); false for a top-level `fn`. The name
    /// alone cannot tell (`Counter.bump` vs the namespace-qualified fn `App.Util.helper`).
    pub method: bool,
}

/// Who an edge points at.
#[derive(Debug, Clone, PartialEq)]
pub enum Callee {
    /// A function in the graph (an index into [`CallGraph::functions`]).
    Function(usize),
    /// A target outside the program, named by its own identity — a module function (`math.sqrt`)
    /// or a method on a built-in or extern type (`List.len`, `string.split`). Not traversable, and
    /// not a gap in the answer: the callee is known, its body just is not Noeta the graph holds.
    External(String),
    /// A statically unresolvable call, named for the report: a closure-valued parameter, local or
    /// field invoked as a call, a member the receiver's type does not declare (a trait default
    /// method, a dynamic dispatch), or a receiver naming no module the program imports.
    Dynamic(String),
}

/// What [`CallGraph::lookup_named`] made of a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameLookup {
    /// Exactly one function answers to the name.
    Found(usize),
    /// The name is a leaf several qualified functions share; the candidates, in graph order.
    Ambiguous(Vec<String>),
    /// No function answers to the name.
    Missing,
}

/// One use edge: `caller` (None = the program's top-level statements) uses `callee` at `site`.
#[derive(Debug, Clone)]
pub struct CallEdge {
    pub caller: Option<usize>,
    pub callee: Callee,
    /// The use's span (the callee identifier at the call/reference site).
    pub site: Span,
    /// True when the use is syntactically a call (`f(...)`); false for a reference (the function
    /// passed as a value — a callback, a pipeline stage, a handler registration).
    pub call: bool,
}

/// The program's static call graph.
#[derive(Debug, Clone, Default)]
pub struct CallGraph {
    pub functions: Vec<FnNode>,
    pub edges: Vec<CallEdge>,
}

impl CallGraph {
    /// The index of the function named `name`, or `None` when the name is missing or ambiguous.
    /// [`lookup_named`](Self::lookup_named) tells the two apart.
    pub fn function_named(&self, name: &str) -> Option<usize> {
        match self.lookup_named(name) {
            NameLookup::Found(i) => Some(i),
            NameLookup::Ambiguous(_) | NameLookup::Missing => None,
        }
    }

    /// Resolve `name` to a function. A **qualified** name (`App.Lib.add`, `App.Lib.Counter.bump`)
    /// matches exactly. A **bare** name — what `symbols`, go-to-definition and the source itself
    /// speak — falls back to matching a declaration whose name ends in `.<name>`, the way
    /// [`Definitions`](crate::resolve::Definitions) resolves a bare source token against the
    /// linker's qualified declarations, so an agent can address the graph with a name it can
    /// obtain. A suffix several functions share resolves to none of them and reports the
    /// candidates instead of picking the first.
    pub fn lookup_named(&self, name: &str) -> NameLookup {
        let name = name.trim();
        let exact = self.matching(|n| n == name);
        if !exact.is_empty() {
            return self.one_of(exact);
        }
        let suffix = format!(".{name}");
        self.one_of(self.matching(|n| n.ends_with(&suffix)))
    }

    fn matching(&self, pred: impl Fn(&str) -> bool) -> Vec<usize> {
        self.functions
            .iter()
            .enumerate()
            .filter(|(_, f)| pred(&f.name))
            .map(|(i, _)| i)
            .collect()
    }

    fn one_of(&self, matches: Vec<usize>) -> NameLookup {
        match matches.as_slice() {
            [] => NameLookup::Missing,
            [only] => NameLookup::Found(*only),
            many => NameLookup::Ambiguous(
                many.iter()
                    .map(|i| self.functions[*i].name.clone())
                    .collect(),
            ),
        }
    }

    /// The declared names closest to `name`, case-insensitively: those whose leaf matches it
    /// first, then those that contain it, capped at `limit`. What a not-found report offers a
    /// reader instead of sending them to another tool.
    pub fn near_matches(&self, name: &str, limit: usize) -> Vec<String> {
        let want = name.trim().to_ascii_lowercase();
        if want.is_empty() {
            return Vec::new();
        }
        let mut by_leaf = Vec::new();
        let mut by_substring = Vec::new();
        for f in &self.functions {
            let lower = f.name.to_ascii_lowercase();
            if noeta_ast::short_type_name(&lower) == want {
                by_leaf.push(f.name.clone());
            } else if lower.contains(&want) {
                by_substring.push(f.name.clone());
            }
        }
        by_leaf.extend(by_substring);
        by_leaf.truncate(limit);
        by_leaf
    }

    /// The edges out of `caller` (`None` = the top-level statements), in source order.
    pub fn edges_from(&self, caller: Option<usize>) -> impl Iterator<Item = &CallEdge> {
        self.edges.iter().filter(move |e| e.caller == caller)
    }

    /// The function a cursor at `offset` (in `source`) addresses, the way an editor means it:
    /// the declared **name** it sits on, else the call/reference **site** it sits on (resolving to
    /// the callee — hierarchy-from-a-call-site), else the tightest **declaration** containing it
    /// (hierarchy-from-inside-the-body; methods nest inside their type's decl, so tightest wins).
    pub fn function_at(&self, offset: u32, source: SourceId) -> Option<usize> {
        let on = |span: Span| span.source == source && span.start <= offset && offset <= span.end;
        if let Some(i) = self.functions.iter().position(|f| on(f.name_span)) {
            return Some(i);
        }
        if let Some(i) = self
            .edges
            .iter()
            .filter(|e| on(e.site))
            .find_map(|e| match e.callee {
                Callee::Function(i) => Some(i),
                _ => None, // an external/dynamic site addresses no graph node — fall to enclosing
            })
        {
            return Some(i);
        }
        self.functions
            .iter()
            .enumerate()
            .filter(|(_, f)| on(f.decl_span))
            .min_by_key(|(_, f)| f.decl_span.end - f.decl_span.start)
            .map(|(i, _)| i)
    }
}

/// Render `graph` as the fixture manifest's line-oriented text form — one `node` line per
/// declaration and one `edge` line per use, each sorted, so two runs over the same program produce
/// byte-identical output:
///
/// ```text
/// node <name> <fn|method> <file>:<line>
/// edge <caller|<top>> -> <callee> <call|reference> <function|external|dynamic>
/// ```
///
/// `files` names each source by [`SourceId`] index and `texts` holds each source's text (for the
/// 1-based declaration line). A source index neither covers renders as `?`, so a short slice
/// degrades the location and never panics.
pub fn render(graph: &CallGraph, files: &[&str], texts: &[&str]) -> String {
    let at = |span: Span| {
        let file = files.get(span.source.0 as usize).copied().unwrap_or("?");
        let line = texts
            .get(span.source.0 as usize)
            .and_then(|t| t.get(..span.start as usize))
            .map_or(0, |head| 1 + head.bytes().filter(|b| *b == b'\n').count());
        format!("{file}:{line}")
    };
    let mut nodes: Vec<String> = graph
        .functions
        .iter()
        .map(|f| {
            let kind = if f.method { "method" } else { "fn" };
            format!("node {} {kind} {}", f.name, at(f.name_span))
        })
        .collect();
    nodes.sort();
    let mut edges: Vec<String> = graph
        .edges
        .iter()
        .map(|e| {
            let caller = match e.caller {
                Some(i) => graph.functions[i].name.as_str(),
                None => "<top>",
            };
            let (callee, target) = match &e.callee {
                Callee::Function(i) => (graph.functions[*i].name.as_str(), "function"),
                Callee::External(name) => (name.as_str(), "external"),
                Callee::Dynamic(name) => (name.as_str(), "dynamic"),
            };
            let how = if e.call { "call" } else { "reference" };
            format!("edge {caller} -> {callee} {how} {target}")
        })
        .collect();
    edges.sort();
    nodes
        .into_iter()
        .chain(edges)
        .map(|line| line + "\n")
        .collect()
}
/// Build the call graph for the (merged) `program`. `expr_types` is the checker's span→type index
/// (method receivers resolve through it); `texts` holds each source's text by [`SourceId`] index —
/// used for the call-vs-reference classification and for naming external module targets. A missing
/// text degrades that edge's classification, never drops it.
pub fn build(
    program: &Program,
    expr_types: &HashMap<Span, TypeRepr>,
    sites: &noeta_check::Sites,
    texts: &[&str],
) -> CallGraph {
    // 1. The function inventory. Tier-block declarations qualify like their top-level siblings, so
    //    one graph speaks one vocabulary; a nested `fn` is named under the function that declares
    //    it.
    let prefixes = module_prefixes(program);
    let mut functions: Vec<FnNode> = Vec::new();
    collect_stmts(&program.stmts, None, &prefixes, &mut functions);
    // Definition span → function index, for resolving a use to its target.
    let by_name_span: HashMap<Span, usize> = functions
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name_span, i))
        .collect();
    // The type names the program declares, and the expression tiers' handlers.
    let types = TypeNames::collect(program, &prefixes);
    let trait_impls = TraitImpls::collect(program, &prefixes);
    let handlers = expr_tier_handlers(program, &by_name_span);
    let modules = imported_modules(program);

    let def_use = DefUse::build(program, sites);
    let members = MemberTable::collect(program);
    let mut edges: Vec<CallEdge> = Vec::new();

    // 2. Value edges: every identifier use that resolves to a function's declared name, plus the
    //    calls through a value that is not one (a function-typed parameter or local).
    for (use_span, def_span) in def_use.refs() {
        let Some(&target) = by_name_span.get(&def_span) else {
            // Not a function declaration. Invoking it anyway is a call through a closure-valued
            // binding: an indirection the trace must show, not a leaf.
            if followed_by_paren(texts, use_span)
                && let Some(name) = slice(texts, use_span)
            {
                edges.push(CallEdge {
                    caller: enclosing(&functions, use_span),
                    callee: Callee::Dynamic(name.to_string()),
                    site: use_span,
                    call: true,
                });
            }
            continue;
        };
        // The definition name itself is a "use" in no meaningful sense; skip self-position.
        if use_span == def_span {
            continue;
        }
        edges.push(CallEdge {
            caller: enclosing(&functions, use_span),
            callee: Callee::Function(target),
            site: use_span,
            call: followed_by_paren(texts, use_span),
        });
    }

    // 3. Member edges: every `receiver.member`, labeled by what the receiver's type says it is.
    for (name, name_span, receiver_span) in def_use.member_occurrences() {
        let is_call = followed_by_paren(texts, name_span);
        let mut push = |callee: Callee| {
            edges.push(CallEdge {
                caller: enclosing(&functions, name_span),
                callee,
                site: name_span,
                call: is_call,
            });
        };
        let resolve = |ty: &str| {
            members
                .lookup(ty, name)
                .and_then(|decl| by_name_span.get(&decl).copied())
        };
        // A member the receiver's type does not declare may be a **default method** of a trait
        // the type implements, whose body the graph holds under `Trait.method`. Only the traits
        // *this* type implements are consulted, so a same-named default on a trait it does not
        // implement is never the answer.
        let trait_default = |ty: &str| {
            trait_impls.of(ty).iter().find_map(|trait_name| {
                // An `impl` names its trait as the source wrote it; the declaration carries the
                // linker's qualified identity, which is what the member table is keyed by.
                let canonical = types.resolve(trait_name).unwrap_or(trait_name);
                resolve(canonical)
            })
        };
        match receiver_kind(expr_types.get(&receiver_span), &types) {
            // A type the program declares: the member resolves through the member table, or
            // through a trait it implements. A field access lands on a field span the inventory
            // does not hold and is no edge at all; a *call* nothing declares (a closure-valued
            // field, a dispatch the checker resolved another way) is an indirection, labeled
            // rather than dropped.
            ReceiverKind::Nominal(ty) => {
                let target = match members.lookup(&ty, name) {
                    Some(decl) => by_name_span.get(&decl).copied(),
                    None => trait_default(&ty),
                };
                match target {
                    Some(target) => push(Callee::Function(target)),
                    None if is_call => push(Callee::Dynamic(format!("{ty}.{name}"))),
                    None => {}
                }
            }
            // A built-in or extern type: its methods live outside the program, named by the
            // method's own identity (`List.len`) rather than by the receiver's source text.
            ReceiverKind::Builtin(ty) if is_call => push(Callee::External(format!("{ty}.{name}"))),
            ReceiverKind::Builtin(_) => {}
            ReceiverKind::Untyped if !is_call => {}
            ReceiverKind::Untyped => {
                let receiver_text = slice(texts, receiver_span);
                // An associated call through a type name (`Counter.new()`): the receiver is a
                // declared type, not a value, so it carries no checker type of its own.
                let declared = receiver_text
                    .filter(|_| {
                        def_use
                            .definition_at(receiver_span.start, receiver_span.source)
                            .is_none()
                    })
                    .and_then(|text| types.resolve(text));
                if let Some(ty) = declared {
                    match resolve(ty) {
                        Some(target) => push(Callee::Function(target)),
                        None => push(Callee::Dynamic(format!("{ty}.{name}"))),
                    }
                    continue;
                }
                // A module function (`math.sqrt(…)`) — only when the program imports a module by
                // that name. Anything else is a dynamic call named by the receiver, and a receiver
                // that is a whole expression is named `<expr>` rather than quoted into the label.
                match receiver_text {
                    Some(recv) if modules.contains(recv) => {
                        push(Callee::External(format!("{recv}.{name}")))
                    }
                    Some(recv) if is_dotted_ident(recv) => {
                        push(Callee::Dynamic(format!("{recv}.{name}")))
                    }
                    _ => push(Callee::Dynamic(format!("<expr>.{name}"))),
                }
            }
        }
    }

    // 4. Expression-tier edges: an `@html { … }` body is a call of the tier's handler, so the
    //    function that writes one reaches it. The holes are ordinary expressions and are already
    //    joined above.
    for (tier, span) in def_use.tier_expr_occurrences() {
        let Some(&target) = handlers.get(tier) else {
            continue; // a tier this program does not declare a handler for
        };
        edges.push(CallEdge {
            caller: enclosing(&functions, span),
            callee: Callee::Function(target),
            site: span,
            call: true,
        });
    }

    // Stable order: by caller, then source position — a deterministic report.
    edges.sort_by_key(|e| {
        (
            e.caller.map_or(usize::MAX, |c| c),
            e.site.source.0,
            e.site.start,
        )
    });
    CallGraph { functions, edges }
}

// --------------------------------------------------------------------- the declaration inventory

/// Walk `stmts` for every function-like declaration, appending each as a node. `prefix` is the
/// qualification a tier block's declarations inherit from their top-level siblings (`None` at top
/// level, where the linker has already qualified each name).
fn collect_stmts(
    stmts: &[Stmt],
    prefix: Option<&str>,
    prefixes: &HashMap<SourceId, String>,
    out: &mut Vec<FnNode>,
) {
    for stmt in stmts {
        match stmt {
            Stmt::Fn(decl) => {
                let name = qualified(prefix, decl.name.as_str());
                collect_nested(&decl.body, &name, out);
                out.push(FnNode {
                    name,
                    name_span: decl.name_span,
                    decl_span: decl.span,
                    method: false,
                });
            }
            Stmt::Struct(decl) => {
                collect_methods(&qualified(prefix, decl.name.as_str()), &decl.methods, out)
            }
            Stmt::Class(decl) => {
                collect_methods(&qualified(prefix, decl.name.as_str()), &decl.methods, out)
            }
            Stmt::Enum(decl) => {
                collect_methods(&qualified(prefix, decl.name.as_str()), &decl.methods, out)
            }
            // A standalone `impl Trait for T` method belongs to `T` exactly as an in-body one does.
            Stmt::Impl(decl) => {
                collect_methods(&qualified(prefix, decl.target.as_str()), &decl.methods, out)
            }
            // A trait's **default** methods are real bodies with real callees, so they are nodes
            // under the trait's name. A bodiless signature declares no code and is not one.
            Stmt::Trait(decl) => {
                let name = qualified(prefix, decl.name.as_str());
                let defaults: Vec<FnDecl> = decl
                    .methods
                    .iter()
                    .filter(|m| !m.sig.body.is_empty())
                    .map(|m| m.sig.clone())
                    .collect();
                collect_methods(&name, &defaults, out);
            }
            // A tier block's declarations (`@test { … }`) are the program's declarations: the
            // impact engine's reverse closure walks from a changed fn to the tests that call it,
            // and the editor's call hierarchy works inside tier bodies. They qualify like their
            // top-level siblings so one graph does not mix two vocabularies.
            Stmt::TierBlock { items, span, .. } => {
                let inherited = prefix.map(str::to_string).or_else(|| {
                    prefixes
                        .get(&span.source)
                        .filter(|p| !p.is_empty())
                        .cloned()
                });
                collect_stmts(items, inherited.as_deref(), prefixes, out);
            }
            _ => {}
        }
    }
}

/// Append `methods` as nodes of the type named `type_name`, each with its own nested `fn`s.
fn collect_methods(type_name: &str, methods: &[FnDecl], out: &mut Vec<FnNode>) {
    for method in methods {
        let name = format!("{type_name}.{}", method.name);
        collect_nested(&method.body, &name, out);
        out.push(FnNode {
            name,
            name_span: method.name_span,
            decl_span: method.span,
            method: true,
        });
    }
}

/// Append every `fn` declared inside `body` (at any block depth) as a node named
/// `<enclosing>.<name>`, recursively.
fn collect_nested(body: &[Stmt], enclosing_name: &str, out: &mut Vec<FnNode>) {
    for stmt in body {
        match stmt {
            Stmt::Fn(decl) => {
                let name = format!("{enclosing_name}.{}", decl.name);
                collect_nested(&decl.body, &name, out);
                out.push(FnNode {
                    name,
                    name_span: decl.name_span,
                    decl_span: decl.span,
                    method: false,
                });
            }
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                collect_nested(then_body, enclosing_name, out);
                if let Some(body) = else_body {
                    collect_nested(body, enclosing_name, out);
                }
            }
            Stmt::For { body, .. } | Stmt::While { body, .. } | Stmt::Concurrent { body, .. } => {
                collect_nested(body, enclosing_name, out)
            }
            Stmt::TierBlock { items, .. } => collect_nested(items, enclosing_name, out),
            _ => {}
        }
    }
}

/// `name` under `prefix`, unless the linker already qualified it that way.
fn qualified(prefix: Option<&str>, name: &str) -> String {
    match prefix {
        Some(p) if !name.starts_with(&format!("{p}.")) => format!("{p}.{name}"),
        _ => name.to_string(),
    }
}

/// The qualification prefix each source's top-level declarations carry (`App.Lib` for a declaration
/// the linker rewrote to `App.Lib.add`), so a tier block in that file can inherit it. A source whose
/// declarations are unqualified — a lone buffer, an unlinked workspace — maps to the empty string.
fn module_prefixes(program: &Program) -> HashMap<SourceId, String> {
    let mut prefixes: HashMap<SourceId, String> = HashMap::new();
    for stmt in &program.stmts {
        let (name, span) = match stmt {
            Stmt::Fn(d) => (d.name.as_str(), d.name_span),
            Stmt::Struct(d) => (d.name.as_str(), d.name_span),
            Stmt::Class(d) => (d.name.as_str(), d.name_span),
            Stmt::Enum(d) => (d.name.as_str(), d.name_span),
            Stmt::Trait(d) => (d.name.as_str(), d.name_span),
            _ => continue,
        };
        let prefix = name.rsplit_once('.').map_or("", |(head, _)| head);
        prefixes
            .entry(span.source)
            .or_insert_with(|| prefix.to_string());
    }
    prefixes
}

// ------------------------------------------------------------------------- receiver classification

/// What a member call's receiver type makes of the member.
enum ReceiverKind {
    /// A type the program declares (or a `dyn Trait` bound): the member resolves through the
    /// member table into the graph.
    Nominal(String),
    /// A built-in the language ships (`List`, `string`, `Map`) or an extern type a native package
    /// registers: its methods are outside the program, and named by the type's own identity.
    Builtin(String),
    /// The checker gave the receiver no usable type.
    Untyped,
}

fn receiver_kind(repr: Option<&TypeRepr>, types: &TypeNames) -> ReceiverKind {
    let Some(repr) = repr else {
        return ReceiverKind::Untyped;
    };
    match repr {
        // The three declared nominal kinds always name a declaration.
        TypeRepr::Struct(name, _) | TypeRepr::Class(name, _) | TypeRepr::Enum(name, _) => {
            ReceiverKind::Nominal(name.clone())
        }
        // A `dyn Trait` receiver dispatches through the trait's contract, which the member table
        // holds under the trait's name.
        TypeRepr::DynTrait(name) => ReceiverKind::Nominal(name.clone()),
        // An opaque nominal is the program's own type when it declares one by that name, else an
        // extern type a native package registered.
        TypeRepr::Named(name, _) => {
            if types.declares(name) {
                ReceiverKind::Nominal(name.clone())
            } else {
                ReceiverKind::Builtin(name.clone())
            }
        }
        // The shipped types, each named by its surface spelling.
        TypeRepr::Int
        | TypeRepr::Float
        | TypeRepr::F32
        | TypeRepr::F64
        | TypeRepr::IntN { .. }
        | TypeRepr::Bool
        | TypeRepr::Str
        | TypeRepr::Bytes
        | TypeRepr::List(_)
        | TypeRepr::Set(_)
        | TypeRepr::Option(_)
        | TypeRepr::Map(..)
        | TypeRepr::Result(..) => ReceiverKind::Builtin(repr.head_name()),
        // `dyn`, `never`, `unit`, a union and a function type name no method table to resolve
        // against; the receiver's own text decides what the call is called.
        TypeRepr::Dyn
        | TypeRepr::Never
        | TypeRepr::Unit
        | TypeRepr::Union(_)
        | TypeRepr::Fn(..) => ReceiverKind::Untyped,
    }
}

/// The type names the program declares, so a receiver's source text can be recognized as one
/// (`Counter.new()`) and an opaque nominal told apart from an extern type. A bare source token
/// resolves against the linker's qualified declaration by leaf, as long as one declaration owns it.
struct TypeNames {
    exact: HashSet<String>,
    /// Leaf → the one qualified declaration carrying it; absent when several do.
    by_leaf: HashMap<String, Option<String>>,
}

impl TypeNames {
    fn collect(program: &Program, prefixes: &HashMap<SourceId, String>) -> TypeNames {
        let mut names = TypeNames {
            exact: HashSet::new(),
            by_leaf: HashMap::new(),
        };
        names.walk(&program.stmts, None, prefixes);
        names
    }

    fn walk(&mut self, stmts: &[Stmt], prefix: Option<&str>, prefixes: &HashMap<SourceId, String>) {
        for stmt in stmts {
            let name = match stmt {
                Stmt::Struct(d) => qualified(prefix, d.name.as_str()),
                Stmt::Class(d) => qualified(prefix, d.name.as_str()),
                Stmt::Enum(d) => qualified(prefix, d.name.as_str()),
                Stmt::Trait(d) => qualified(prefix, d.name.as_str()),
                Stmt::TierBlock { items, span, .. } => {
                    let inherited = prefix.map(str::to_string).or_else(|| {
                        prefixes
                            .get(&span.source)
                            .filter(|p| !p.is_empty())
                            .cloned()
                    });
                    self.walk(items, inherited.as_deref(), prefixes);
                    continue;
                }
                _ => continue,
            };
            let leaf = noeta_ast::short_type_name(&name).to_string();
            match self.by_leaf.entry(leaf) {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    if e.get().as_deref() != Some(name.as_str()) {
                        e.insert(None); // two declarations share the leaf — it names neither
                    }
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(Some(name.clone()));
                }
            }
            self.exact.insert(name);
        }
    }

    fn declares(&self, name: &str) -> bool {
        self.exact.contains(name)
    }

    /// The declaration `text` names, exactly or by an unambiguous leaf.
    fn resolve(&self, text: &str) -> Option<&str> {
        if self.exact.contains(text) {
            return self.exact.get(text).map(String::as_str);
        }
        self.by_leaf.get(text)?.as_deref()
    }
}

/// Which traits each declared type implements, from both forms that declare the relation: a
/// standalone `impl Trait for T { … }` and an in-body `impl Trait { … }`. A member call the type
/// itself does not declare resolves through these to the trait's default method.
#[derive(Default)]
struct TraitImpls {
    by_type: HashMap<String, Vec<String>>,
}

impl TraitImpls {
    fn collect(program: &Program, prefixes: &HashMap<SourceId, String>) -> TraitImpls {
        let mut impls = TraitImpls::default();
        impls.walk(&program.stmts, None, prefixes);
        impls
    }

    fn walk(&mut self, stmts: &[Stmt], prefix: Option<&str>, prefixes: &HashMap<SourceId, String>) {
        for stmt in stmts {
            match stmt {
                Stmt::Impl(d) => self.add(
                    qualified(prefix, d.target.as_str()),
                    qualified(prefix, d.trait_name.as_str()),
                ),
                Stmt::Struct(d) => self.add_blocks(prefix, d.name.as_str(), &d.impls),
                Stmt::Class(d) => self.add_blocks(prefix, d.name.as_str(), &d.impls),
                Stmt::Enum(d) => self.add_blocks(prefix, d.name.as_str(), &d.impls),
                Stmt::TierBlock { items, span, .. } => {
                    let inherited = prefix.map(str::to_string).or_else(|| {
                        prefixes
                            .get(&span.source)
                            .filter(|p| !p.is_empty())
                            .cloned()
                    });
                    self.walk(items, inherited.as_deref(), prefixes);
                }
                _ => {}
            }
        }
    }

    fn add_blocks(&mut self, prefix: Option<&str>, ty: &str, blocks: &[noeta_ast::ImplBlock]) {
        for block in blocks {
            self.add(
                qualified(prefix, ty),
                qualified(prefix, block.trait_name.as_str()),
            );
        }
    }

    fn add(&mut self, ty: String, trait_name: String) {
        let traits = self.by_type.entry(ty).or_default();
        if !traits.contains(&trait_name) {
            traits.push(trait_name);
        }
    }

    /// The traits `ty` implements, in declaration order.
    fn of(&self, ty: &str) -> &[String] {
        self.by_type.get(ty).map_or(&[], Vec::as_slice)
    }
}

/// Each **expression tier**'s handler node, keyed by the tier name a `@name { … }` body writes.
/// The tier's declaration is the decorated `fn`, so its body's calls are already in the graph;
/// this is what connects the writer of a block to it.
fn expr_tier_handlers(
    program: &Program,
    by_name_span: &HashMap<Span, usize>,
) -> HashMap<String, usize> {
    let mut handlers = HashMap::new();
    for stmt in &program.stmts {
        let Stmt::Fn(decl) = stmt else { continue };
        let Some(tier) = &decl.tier else { continue };
        if tier.expr.is_none() {
            continue; // a statement tier's block declares items; it is no call
        }
        if let Some(&idx) = by_name_span.get(&decl.name_span) {
            handlers.insert(tier.name.clone(), idx);
        }
    }
    handlers
}

/// Every module name the program can address: each `use` path, its prefixes, and each binding a
/// `use` introduces. A receiver outside this set names no module the program imports, so a call on
/// it is dynamic rather than a confident `external` leaf.
///
/// This is the import set, not the loader's module pool: `build`'s inputs are the merged program
/// and the checker's indices, and the pool is not among them.
fn imported_modules(program: &Program) -> HashSet<String> {
    let mut modules = HashSet::new();
    for stmt in &program.stmts {
        match stmt {
            Stmt::Use { path, names, .. } => {
                for i in 1..=path.len() {
                    modules.insert(path[..i].join("."));
                }
                if let Some(last) = path.last() {
                    modules.insert(last.to_string());
                }
                for n in names {
                    modules.insert(n.local().to_string());
                    if let Some(prefix) = path.last() {
                        modules.insert(format!("{prefix}.{}", n.name));
                    }
                }
            }
            Stmt::Namespace { path, .. } => {
                for i in 1..=path.len() {
                    modules.insert(path[..i].join("."));
                }
            }
            _ => {}
        }
    }
    modules
}

/// The tightest function declaration containing `span` (methods nest inside their type's decl, and
/// a nested `fn` inside its enclosing one, so tightest wins), or `None` for a top-level-statement
/// site.
fn enclosing(functions: &[FnNode], span: Span) -> Option<usize> {
    functions
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            f.decl_span.source == span.source
                && f.decl_span.start <= span.start
                && span.end <= f.decl_span.end
        })
        .min_by_key(|(_, f)| f.decl_span.end - f.decl_span.start)
        .map(|(i, _)| i)
}

/// Whether the use at `span` is syntactically a call — the next non-space character is `(`.
fn followed_by_paren(texts: &[&str], span: Span) -> bool {
    let Some(text) = texts.get(span.source.0 as usize) else {
        return false;
    };
    text.get(span.end as usize..)
        .map(|rest| rest.trim_start().starts_with('('))
        .unwrap_or(false)
}

fn slice<'a>(texts: &'a [&str], span: Span) -> Option<&'a str> {
    texts
        .get(span.source.0 as usize)?
        .get(span.start as usize..span.end as usize)
}

/// Whether `text` is a plain (possibly dotted) identifier — the shape a receiver has to be for its
/// own text to serve as a callee label. Anything else is a whole expression.
fn is_dotted_ident(text: &str) -> bool {
    !text.is_empty()
        && text.split('.').all(|seg| {
            let mut chars = seg.chars();
            matches!(chars.next(), Some(c) if c.is_alphabetic() || c == '_')
                && chars.all(|c| c.is_alphanumeric() || c == '_')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_span::{Source, SourceId};

    fn graph(src: &str) -> (CallGraph, noeta_check::Checked) {
        let source = Source::new(SourceId::FIRST, "test.noe", src);
        let lexed = noeta_lexer::lex(&source);
        let parsed = noeta_parser::parse(&source, &lexed.tokens);
        assert!(
            lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
            "fixture parses"
        );
        let checked = noeta_check::check_all_with_types(&parsed.program);
        let g = build(&parsed.program, &checked.expr_types, &checked.sites, &[src]);
        (g, checked)
    }

    #[test]
    fn direct_calls_edge_between_functions() {
        let (g, _) = graph(
            "fn helper(): int { return 1 }\nfn work(): int { return helper() + helper() }\necho work()\n",
        );
        let helper = g.function_named("helper").unwrap();
        let work = g.function_named("work").unwrap();
        // work → helper twice, both syntactic calls.
        let out: Vec<_> = g.edges_from(Some(work)).collect();
        assert_eq!(out.len(), 2);
        assert!(
            out.iter()
                .all(|e| e.callee == Callee::Function(helper) && e.call)
        );
        // Top level calls work.
        let top: Vec<_> = g.edges_from(None).collect();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].callee, Callee::Function(work));
    }

    #[test]
    fn passing_a_function_is_a_reference_edge() {
        let (g, _) = graph(
            "fn cb(n: int): int { return n }\nfn run(f: (int) -> int): int { return f(1) }\necho run(cb)\n",
        );
        let cb = g.function_named("cb").unwrap();
        let top: Vec<_> = g.edges_from(None).collect();
        let cb_edge = top
            .iter()
            .find(|e| e.callee == Callee::Function(cb))
            .expect("cb referenced");
        assert!(!cb_edge.call, "passed as a value, not called");
        // `f(1)` inside `run` calls the parameter, not a declaration: the flow continues through an
        // indirection static analysis cannot follow, and the edge says so rather than vanishing.
        let run = g.function_named("run").unwrap();
        assert!(
            g.edges_from(Some(run))
                .any(|e| e.callee == Callee::Dynamic("f".to_string()) && e.call),
            "edges: {:?}",
            g.edges
        );
    }

    #[test]
    fn a_receiver_naming_no_imported_module_is_dynamic_not_external() {
        // `cfg` names no import and no binding. Classifying it `external` by its spelling alone
        // would present a guess as a resolved native call, so it is labeled dynamic.
        let (g, _) = graph(
            "use std.math\nfn a(): float { return math.sqrt(4.0) }\nfn b(): int { return cfg.limit() }\n",
        );
        let a = g.function_named("a").unwrap();
        let b = g.function_named("b").unwrap();
        assert!(
            g.edges_from(Some(a))
                .any(|e| e.callee == Callee::External("math.sqrt".to_string()))
        );
        assert!(
            g.edges_from(Some(b))
                .any(|e| e.callee == Callee::Dynamic("cfg.limit".to_string())),
            "edges: {:?}",
            g.edges
        );
    }

    #[test]
    fn a_dynamic_label_is_the_member_identity_not_the_receiver_expression() {
        // The receiver is a whole expression, so it cannot name the callee; the label keeps the
        // member and says the receiver was an expression.
        let (g, _) = graph("fn pick(a: dyn, b: dyn): dyn { return (a ?? b).render() }\n");
        let pick = g.function_named("pick").unwrap();
        assert!(
            g.edges_from(Some(pick))
                .any(|e| e.callee == Callee::Dynamic("<expr>.render".to_string())),
            "edges: {:?}",
            g.edges
        );
    }

    #[test]
    fn a_bare_leaf_resolves_and_a_shared_one_reports_its_candidates() {
        let graph = CallGraph {
            functions: vec![
                node("App.A.shared"),
                node("App.B.shared"),
                node("App.A.Counter.bump"),
            ],
            edges: Vec::new(),
        };
        assert_eq!(
            graph.lookup_named("shared"),
            NameLookup::Ambiguous(vec!["App.A.shared".into(), "App.B.shared".into()])
        );
        assert_eq!(graph.lookup_named("App.B.shared"), NameLookup::Found(1));
        // A method addresses by `Type.method`, the spelling `symbols` and the source both use.
        assert_eq!(graph.lookup_named("Counter.bump"), NameLookup::Found(2));
        assert_eq!(graph.lookup_named("bump"), NameLookup::Found(2));
        assert_eq!(graph.lookup_named("ghost"), NameLookup::Missing);
        assert_eq!(
            graph.near_matches("shar", 5),
            vec!["App.A.shared", "App.B.shared"]
        );
    }

    #[test]
    fn a_tier_block_declaration_is_a_node_distinct_from_its_top_level_namesake() {
        let (g, _) = graph(
            "fn helper(): int { return 1 }\n\
             @test {\n\
             fn helper(): int { return 2 }\n\
             fn checks(): void { assert(helper() == 2) }\n\
             }\n",
        );
        let both: Vec<&FnNode> = g.functions.iter().filter(|f| f.name == "helper").collect();
        assert_eq!(
            both.len(),
            2,
            "two declarations, two nodes: {:?}",
            g.functions
        );
        assert_ne!(both[0].name_span, both[1].name_span);
        // The name names neither on its own, so the lookup hands back the choice.
        assert!(matches!(g.lookup_named("helper"), NameLookup::Ambiguous(_)));
        assert!(g.function_named("checks").is_some());
    }

    fn node(name: &str) -> FnNode {
        let span = Span::new_in(SourceId::FIRST, 0, 1);
        FnNode {
            name: name.to_string(),
            name_span: span,
            decl_span: span,
            method: name.matches('.').count() > 2,
        }
    }

    #[test]
    fn method_calls_resolve_via_the_receiver_type() {
        let (g, _) = graph(
            "struct Counter {\n  n: int\n  fn bump(): int { return self.n + 1 }\n}\nfn use_it(): int {\n  c = Counter { n: 1 }\n  return c.bump()\n}\n",
        );
        let bump = g.function_named("Counter.bump").unwrap();
        let use_it = g.function_named("use_it").unwrap();
        assert!(
            g.edges_from(Some(use_it))
                .any(|e| e.callee == Callee::Function(bump) && e.call)
        );
    }

    #[test]
    fn module_calls_are_external_edges() {
        let (g, _) = graph(
            "use std.math\nfn area(r: float): float { return math.sqrt(r) }\necho area(4.0)\n",
        );
        let area = g.function_named("area").unwrap();
        assert!(
            g.edges_from(Some(area))
                .any(|e| e.callee == Callee::External("math.sqrt".to_string())),
            "edges: {:?}",
            g.edges
        );
    }
}
