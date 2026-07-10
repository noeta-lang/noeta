//! The static function-level **call graph** (role-graph R3): which function uses which, with call
//! sites — the structural skeleton a role-aware trace walks (`noeta mcp`'s `trace` tool) and a
//! future LSP call hierarchy can serve.
//!
//! Built as a **join over the existing indices**, not a new resolver: [`DefUse`](crate::resolve::
//! DefUse) already records every value-identifier use → definition (function references included,
//! cross-file over the merged program), its member occurrences record every `receiver.member`
//! access, and the checker's `expr_types` resolve receivers to nominal types for method targets.
//! A function-declaration inventory (top-level `fn`s plus struct/class/enum/impl methods) turns
//! "a use at this span" into "an edge from its enclosing function".
//!
//! Honesty over completeness: what static analysis cannot resolve is *labeled*, never guessed —
//! a call through a closure-valued binding is a `dynamic` callee, a call into a native module
//! (`math.sqrt`, `http.serve`) an `external` one. An edge is a `call` when the use is followed by
//! `(` in source, otherwise a `reference` (the function passed as a value — a handler registration
//! or callback, still part of the flow a trace should follow).

use std::collections::HashMap;

use noeta_ast::reflect::TypeRepr;
use noeta_ast::{Program, Stmt};
use noeta_span::Span;

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
}

/// Who an edge points at.
#[derive(Debug, Clone, PartialEq)]
pub enum Callee {
    /// A function in the graph (an index into [`CallGraph::functions`]).
    Function(usize),
    /// A native/module target outside the program (`math.sqrt`, `http.serve`) — named, not
    /// traversable.
    External(String),
    /// A call through a closure-valued binding — statically unresolvable; named for the report.
    Dynamic(String),
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
    /// The index of the function named `name` (a top-level `fn` name or `Type.method`).
    pub fn function_named(&self, name: &str) -> Option<usize> {
        self.functions.iter().position(|f| f.name == name)
    }

    /// The edges out of `caller` (`None` = the top-level statements), in source order.
    pub fn edges_from(&self, caller: Option<usize>) -> impl Iterator<Item = &CallEdge> {
        self.edges.iter().filter(move |e| e.caller == caller)
    }
}

/// Build the call graph for the (merged) `program`. `expr_types` is the checker's span→type index
/// (method receivers resolve through it); `texts` holds each source's text by [`SourceId`] index —
/// used for the call-vs-reference classification and for naming external module targets. A missing
/// text degrades that edge's classification, never drops it.
pub fn build(program: &Program, expr_types: &HashMap<Span, TypeRepr>, texts: &[&str]) -> CallGraph {
    // 1. The function inventory: top-level fns, plus methods qualified as `Type.method`.
    let mut functions: Vec<FnNode> = Vec::new();
    for stmt in &program.stmts {
        match stmt {
            Stmt::Fn(decl) => functions.push(FnNode {
                name: decl.name.clone(),
                name_span: decl.name_span,
                decl_span: decl.span,
            }),
            Stmt::Struct(decl) => {
                for m in &decl.methods {
                    functions.push(method_node(&decl.name, m));
                }
            }
            Stmt::Class(decl) => {
                for m in &decl.methods {
                    functions.push(method_node(&decl.name, m));
                }
            }
            Stmt::Enum(decl) => {
                for m in &decl.methods {
                    functions.push(method_node(&decl.name, m));
                }
            }
            Stmt::Impl(decl) => {
                for m in &decl.methods {
                    functions.push(method_node(&decl.target, m));
                }
            }
            _ => {}
        }
    }
    // Definition span → function index, for resolving a use to its target.
    let by_name_span: HashMap<Span, usize> = functions
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name_span, i))
        .collect();

    let def_use = DefUse::build(program);
    let members = MemberTable::collect(program);
    let mut edges: Vec<CallEdge> = Vec::new();

    // 2. Value edges: every identifier use that resolves to a function's declared name.
    for (use_span, def_span) in def_use.refs() {
        let Some(&target) = by_name_span.get(&def_span) else {
            continue; // a local/parameter use, not a function
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

    // 3. Member edges: every `receiver.member` whose receiver types to a nominal with that method,
    //    plus external module calls (`math.sqrt`) and honestly-dynamic member calls.
    for (name, name_span, receiver_span) in def_use.member_occurrences() {
        let nominal = expr_types.get(&receiver_span).and_then(nominal_name);
        if let Some(ty) = nominal {
            // A typed receiver: a method resolves into the graph; a field access resolves to a
            // field span (not in the inventory) and is not a call edge.
            if let Some(decl) = members.lookup(ty, name)
                && let Some(&target) = by_name_span.get(&decl)
            {
                edges.push(CallEdge {
                    caller: enclosing(&functions, name_span),
                    callee: Callee::Function(target),
                    site: name_span,
                    call: followed_by_paren(texts, name_span),
                });
            }
            continue;
        }
        // Untyped receiver + syntactic call: a module function (`math.sqrt(…)`) when the receiver
        // is a plain identifier that is not a value binding; otherwise a dynamic member call.
        if !followed_by_paren(texts, name_span) {
            continue;
        }
        let receiver_is_value = def_use
            .definition_at(receiver_span.start, receiver_span.source)
            .is_some();
        let receiver_text = slice(texts, receiver_span);
        match receiver_text {
            Some(recv) if !receiver_is_value && is_module_path(recv) => {
                edges.push(CallEdge {
                    caller: enclosing(&functions, name_span),
                    callee: Callee::External(format!("{recv}.{name}")),
                    site: name_span,
                    call: true,
                });
            }
            _ => {
                let recv = receiver_text.unwrap_or("<expr>");
                edges.push(CallEdge {
                    caller: enclosing(&functions, name_span),
                    callee: Callee::Dynamic(format!("{recv}.{name}")),
                    site: name_span,
                    call: true,
                });
            }
        }
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

fn method_node(type_name: &str, method: &noeta_ast::FnDecl) -> FnNode {
    FnNode {
        name: format!("{type_name}.{}", method.name),
        name_span: method.name_span,
        decl_span: method.span,
    }
}

/// The tightest function declaration containing `span` (methods nest inside their type's decl, so
/// tightest wins), or `None` for a top-level-statement site.
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

/// Whether `text` looks like a (possibly dotted) module path — lowercase-led identifiers, the way
/// module receivers (`math`, `http.client`) read.
fn is_module_path(text: &str) -> bool {
    !text.is_empty()
        && text.split('.').all(|seg| {
            let mut chars = seg.chars();
            matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_')
                && chars.all(|c| c.is_alphanumeric() || c == '_')
        })
}

fn nominal_name(repr: &TypeRepr) -> Option<&str> {
    match repr {
        TypeRepr::Struct(name, _)
        | TypeRepr::Class(name, _)
        | TypeRepr::Enum(name, _)
        | TypeRepr::Named(name, _) => Some(name),
        _ => None,
    }
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
        let g = build(&parsed.program, &checked.expr_types, &[src]);
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
        // The call through the parameter is dynamic-by-binding and simply absent from value edges
        // (f resolves to the parameter, not a function declaration) — never guessed.
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
