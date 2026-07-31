//! A stable, indented S-expression pretty-printer for the AST.
//!
//! This is the textual form snapshot tests assert against (never `Debug` of raw
//! structs, which is noisy and unstable). Spans are rendered as `@start..end` so a
//! span regression shows up directly in a snapshot diff. It is also the printer the
//! parse→print→parse property test (Slice 9) builds on.
//!
//! # It is also the fmt safety gate, so: **no `..` in an arm**
//!
//! `noeta fmt` promises its output "re-parses to the same AST modulo spans" and implements that by
//! comparing *this* rendering with the span annotations erased (`noeta-fmt/src/safety.rs`). A field
//! no arm here prints is therefore a field the formatter may silently rewrite — which is not a
//! hypothetical: a `..` on the [`Stmt::TierBlock`] arm hid `attached`, and a printer rule that
//! collapsed `@test { fn t() {…} }` into `@test fn t()` flipped it past the gate; an unqualified
//! payload-less variant pattern rendered like a catch-all binding, and the printer was dropping
//! exactly the parens that tell them apart, turning `Ok() => …` into a pattern that matches
//! everything.
//!
//! So every arm **binds every field by name**, and a field that is deliberately not rendered is
//! `_`-bound rather than swept up by `..`. That makes the decision visible at the site and makes a
//! newly added field a compile error here instead of a silent hole in the safety property. The only
//! fields currently `_`-bound are spans (formatting shifts every byte offset by construction, so
//! the gate erases them on purpose) — see `plans/fmt-structural-safety-gate.md` for the full survey
//! and for the structural comparison that should eventually replace this proxy.

use crate::{
    AttrArg, AttrValue, CallArg, ClassDecl, ClosureBody, EnumDecl, Expr, FieldDecl, FnDecl,
    ForPattern, ImplDecl, Name, ObjectLit, Param, Pattern, Program, Stmt, StrPart, StructDecl,
    TraitBound, TraitDecl, TypeOperand, TypeParam, TypeRef,
};
use noeta_span::Span;

/// Render an AST node to the canonical pretty form.
pub trait Pretty {
    fn pretty(&self, out: &mut String, indent: usize);

    fn to_pretty_string(&self) -> String {
        let mut out = String::new();
        self.pretty(&mut out, 0);
        out
    }
}

fn indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn span(s: Span) -> String {
    format!("@{}..{}", s.start, s.end)
}

/// A callable's parameters, for the S-expression dump.
///
/// Each parameter renders its `#[...]` attributes ahead of its name. The fmt safety gate compares
/// this form before and after formatting, so an unrendered attribute would be an attribute the
/// formatter could silently drop — the same failure the labelled-argument rendering below exists to
/// prevent, and a worse one here: dropping `#[Arg(short: "r")]` changes what a signature-driven
/// framework generates while leaving the program's own behaviour identical, so nothing else would
/// notice.
///
/// The **type annotation** and the *presence* of a **default** are rendered for the same reason:
/// the formatter re-emits both from the AST, so dropping `: int` (which is what the checker checks
/// the argument against) or a ` = expr` (which is what makes the parameter optional) would
/// otherwise compare equal. The default's *expression* is rendered separately, as a child of the
/// declaration — it can be arbitrarily large, and a flat list is the wrong place for it.
fn param_list(params: &[Param]) -> String {
    params
        .iter()
        .map(|p| {
            let attrs: String = p
                .attrs
                .iter()
                .map(|a| format!("#[{}{}] ", a.name, attr_args_str(&a.args)))
                .collect();
            let ty =
                p.ty.as_ref()
                    .map(|t| format!(": {}", type_ref_str(t)))
                    .unwrap_or_default();
            let default = if p.default.is_some() { "=" } else { "" };
            format!("{attrs}{}{ty}{default}", p.name)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Emit `(<label> <name> …expr…)` as a child line — the declaration-attached expressions the head
/// line can only mark as present: a parameter's or field's default, an enum variant's backing value.
///
/// They are part of the declaration's meaning (`x: int = 1` and `x: int = 2` are different
/// programs) and the formatter re-emits each of them through the ordinary expression printer, so a
/// printing bug in one is a program change. Rendering only the marker in the head line, as this
/// printer did, made every such bug invisible to the fmt safety gate.
fn expr_child(out: &mut String, level: usize, label: &str, name: &str, value: &Expr) {
    out.push('\n');
    indent(out, level);
    out.push_str(&format!("({label} {name}\n"));
    value.pretty(out, level + 1);
    out.push(')');
}

/// The `impl Trait { … }` blocks written inside a type body, as child lines.
///
/// A block's methods are *also* flattened into the declaration's `methods` list, so the bodies were
/// already compared — but which trait each belongs to lived only here, and here was unrendered. A
/// formatter that emitted an impl block's method as an inherent one (or moved it between blocks)
/// produced a program with different `impls` and an identical `methods` list, and the gate compared
/// the two equal. The member *names* are listed so the grouping, not just the trait, is compared.
fn impl_blocks(out: &mut String, level: usize, impls: &[crate::ImplBlock]) {
    for b in impls {
        let args = if b.trait_args.is_empty() {
            String::new()
        } else {
            format!(
                "<{}>",
                b.trait_args
                    .iter()
                    .map(type_ref_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let names: Vec<&str> = b.methods.iter().map(|m| m.name.as_str()).collect();
        out.push('\n');
        indent(out, level);
        out.push_str(&format!(
            "(impl-block {}{args} [{}]{})",
            b.trait_name,
            names.join(" "),
            assoc_bindings_str(&b.assoc_bindings)
        ));
    }
}

/// An impl's `type Name = Concrete;` associated-type bindings, or the empty string when it has
/// none. They resolve `Self::Name` in the trait's signatures for this implementor, so dropping or
/// re-pointing one changes what the checker resolves — it belongs in the gate.
fn assoc_bindings_str(bindings: &[(String, TypeRef)]) -> String {
    if bindings.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = bindings
        .iter()
        .map(|(n, t)| format!("{n}={}", type_ref_str(t)))
        .collect();
    format!(" {{{}}}", parts.join(", "))
}

/// An enum variant's payload list, for the AST rendering the fmt safety gate compares.
///
/// Unlike [`param_list`], this renders the **type** — because for a variant payload the type is
/// what the declaration is about, and it is the only thing distinguishing `Leaf(User)` from
/// `Leaf(Post)`. A positional payload prints its type alone; a named one prints `name: type`, so
/// the two spellings stay distinguishable in the snapshot (a formatter that turned one into the
/// other would be changing the source's meaning to a reader, even though nothing binds a payload
/// by name).
///
/// The gate saw neither before: a named payload rendered as its bare name, so dropping its `: T`
/// annotation would have compared equal, and a positional one rendered as *its type in the name
/// slot* — right by accident, and only until the representation was fixed.
fn variant_payload_list(fields: &[Param]) -> String {
    fields
        .iter()
        .map(|p| {
            let attrs: String = p
                .attrs
                .iter()
                .map(|a| format!("#[{}{}] ", a.name, attr_args_str(&a.args)))
                .collect();
            let ty = p.ty.as_ref().map(type_ref_str).unwrap_or_default();
            if p.positional {
                format!("{attrs}{ty}")
            } else {
                format!("{attrs}{}: {ty}", p.name)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Pretty for CallArg {
    /// A labelled argument renders its label. The fmt safety gate compares this form before and
    /// after formatting, so an unrendered label would be a label the formatter could silently
    /// drop — turning `f(b: 1, a: 2)` into `f(1, 2)` and changing which parameter each value
    /// binds to. That is precisely the failure this representation exists to prevent.
    fn pretty(&self, out: &mut String, level: usize) {
        match &self.name {
            Some(name) => {
                indent(out, level);
                out.push_str(&format!("(arg {name} {}\n", span(self.span)));
                self.value.pretty(out, level + 1);
                out.push(')');
            }
            None => self.value.pretty(out, level),
        }
    }
}

/// A reflection surface's type operand. The two arms render **differently** on purpose: the fmt
/// safety gate compares this form before and after formatting, so a formatter that turned a
/// turbofish `field_specs_of::<T>()` into the string call `field_specs_of("T")` (or the reverse)
/// must show up as a diff rather than as an equal-looking `(str "T")` either way.
impl Pretty for TypeOperand {
    fn pretty(&self, out: &mut String, level: usize) {
        match self {
            TypeOperand::Static(ty) => {
                indent(out, level);
                out.push_str(&format!(
                    "(type-arg {} {})",
                    type_ref_str(ty),
                    span(ty.span())
                ));
            }
            TypeOperand::Dynamic(e) => e.pretty(out, level),
        }
    }
}

impl Pretty for Program {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        out.push_str(&format!("(program {}", span(self.span)));
        for stmt in &self.stmts {
            out.push('\n');
            stmt.pretty(out, level + 1);
        }
        out.push(')');
    }
}

impl Pretty for Stmt {
    fn pretty(&self, out: &mut String, level: usize) {
        match self {
            Stmt::Echo { value, span: s } => {
                indent(out, level);
                out.push_str(&format!("(echo {}\n", span(*s)));
                value.pretty(out, level + 1);
                out.push(')');
            }
            Stmt::Binding {
                mut_decl,
                name,
                name_span: _,
                ty,
                value,
                span: s,
            } => {
                indent(out, level);
                let kw = if *mut_decl { "binding-mut" } else { "binding" };
                // The **type annotation** is the boundary the value is checked against and the only
                // way to type an otherwise un-inferable value (`acc: List<int> = []`), so a
                // formatter that dropped or altered it would change what the checker accepts. It is
                // re-emitted from the AST, and it was not in this dump — rendered now, and only
                // when written, so an unannotated binding's snapshot is unchanged.
                let ty = ty
                    .as_ref()
                    .map(|t| format!(": {}", type_ref_str(t)))
                    .unwrap_or_default();
                out.push_str(&format!("({kw} {name}{ty} {}\n", span(*s)));
                value.pretty(out, level + 1);
                out.push(')');
            }
            Stmt::Destructure {
                mut_decl,
                targets,
                value,
                span: s,
            } => {
                indent(out, level);
                let kw = if *mut_decl {
                    "destructure-mut"
                } else {
                    "destructure"
                };
                let names: Vec<&str> = targets.iter().map(|(n, _)| n.as_str()).collect();
                out.push_str(&format!("({kw} [{}] {}\n", names.join(" "), span(*s)));
                value.pretty(out, level + 1);
                out.push(')');
            }
            Stmt::Fn(decl) => decl.pretty(out, level),
            Stmt::Enum(decl) => decl.pretty(out, level),
            Stmt::Struct(decl) => decl.pretty(out, level),
            Stmt::Class(decl) => decl.pretty(out, level),
            Stmt::Impl(decl) => decl.pretty(out, level),
            Stmt::Trait(decl) => decl.pretty(out, level),
            Stmt::Namespace { path, span: s } => {
                indent(out, level);
                out.push_str(&format!("(namespace {} {})", path.join("."), span(*s)));
            }
            Stmt::Use {
                path,
                names,
                span: s,
            } => {
                indent(out, level);
                // A renamed import renders `Name=Alias`; a plain one just `Name`.
                let names: Vec<String> = names
                    .iter()
                    .map(|n| match &n.alias {
                        Some(a) => format!("{}={a}", n.name),
                        None => n.name.clone(),
                    })
                    .collect();
                out.push_str(&format!(
                    "(use {} [{}] {})",
                    path.join("."),
                    names.join(" "),
                    span(*s)
                ));
            }
            Stmt::Return { value, span: s } => {
                indent(out, level);
                match value {
                    Some(value) => {
                        out.push_str(&format!("(return {}\n", span(*s)));
                        value.pretty(out, level + 1);
                        out.push(')');
                    }
                    None => out.push_str(&format!("(return {})", span(*s))),
                }
            }
            Stmt::Yield { value, span: s } => {
                indent(out, level);
                out.push_str(&format!("(yield {}\n", span(*s)));
                value.pretty(out, level + 1);
                out.push(')');
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
                span: s,
            } => {
                indent(out, level);
                out.push_str(&format!("(if {}\n", span(*s)));
                cond.pretty(out, level + 1);
                out.push('\n');
                indent(out, level + 1);
                out.push_str("(then");
                for stmt in then_body {
                    out.push('\n');
                    stmt.pretty(out, level + 2);
                }
                out.push(')');
                if let Some(else_body) = else_body {
                    out.push('\n');
                    indent(out, level + 1);
                    out.push_str("(else");
                    for stmt in else_body {
                        out.push('\n');
                        stmt.pretty(out, level + 2);
                    }
                    out.push(')');
                }
                out.push(')');
            }
            Stmt::For {
                pattern,
                iterable,
                body,
                span: s,
            } => {
                indent(out, level);
                let pat = match pattern {
                    ForPattern::Single { name, name_span: _ } => name.clone(),
                    ForPattern::Tuple { names, span: _ } => {
                        let names: Vec<&str> = names.iter().map(|(n, _)| n.as_str()).collect();
                        format!("({})", names.join(", "))
                    }
                };
                out.push_str(&format!("(for [{pat}] {}\n", span(*s)));
                iterable.pretty(out, level + 1);
                for stmt in body {
                    out.push('\n');
                    stmt.pretty(out, level + 1);
                }
                out.push(')');
            }
            Stmt::While {
                cond,
                body,
                span: s,
            } => {
                indent(out, level);
                out.push_str(&format!("(while {}\n", span(*s)));
                cond.pretty(out, level + 1);
                for stmt in body {
                    out.push('\n');
                    stmt.pretty(out, level + 1);
                }
                out.push(')');
            }
            Stmt::Concurrent { body, span: s } => {
                indent(out, level);
                out.push_str(&format!("(concurrent {}", span(*s)));
                for stmt in body {
                    out.push('\n');
                    stmt.pretty(out, level + 1);
                }
                out.push(')');
            }
            Stmt::Break { span: s } => {
                indent(out, level);
                out.push_str(&format!("(break {})", span(*s)));
            }
            Stmt::Continue { span: s } => {
                indent(out, level);
                out.push_str(&format!("(continue {})", span(*s)));
            }
            Stmt::Expr { expr, span: s } => {
                indent(out, level);
                out.push_str(&format!("(expr-stmt {}\n", span(*s)));
                expr.pretty(out, level + 1);
                out.push(')');
            }
            Stmt::TierBlock {
                tier,
                tier_span: _,
                args,
                items,
                doc_text,
                attached,
                span: s,
            } => {
                indent(out, level);
                out.push_str(&format!("(tier {tier}{} {}", attr_args_str(args), span(*s)));
                // `attached` — "there were no braces" — is part of the block's identity, not a
                // layout detail: `@test fn t() {…}` and `@test { fn t() {…} }` produce otherwise
                // byte-identical `TierBlock`s, and the checker treats them differently (E0054's
                // declared-site check runs only on an attached block). The formatter once collapsed
                // the braced form into the annotation form and the gate compared the two EQUAL,
                // because this arm destructured with `..`. Rendered only when set, so a braced
                // block's snapshot is unchanged.
                if *attached {
                    out.push_str(" :attached");
                }
                if let Some(text) = doc_text {
                    out.push_str(&format!(" :text {text:?}"));
                }
                for item in items {
                    out.push('\n');
                    item.pretty(out, level + 1);
                }
                out.push(')');
            }
        }
    }
}

impl Pretty for FnDecl {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        // A `@tier(…)` declaration rides on its runner/handler fn — render it so structural
        // comparisons (e.g. the formatter's safety gate) see it: dropping the directive is a
        // program change. The `expr:` field (expr-tiers arc) is part of that identity.
        let tier = match &self.tier {
            Some(t) => {
                let config = match &t.config {
                    Some((c, _)) => format!(", config: {c}"),
                    None => String::new(),
                };
                let text = match &t.text {
                    Some((lang, _)) => format!(", text: {lang:?}"),
                    None => String::new(),
                };
                let expr = match &t.expr {
                    Some((ty, _)) => format!(", expr: {ty}"),
                    None => String::new(),
                };
                format!("@tier({}{config}{text}{expr}) ", t.name)
            }
            None => String::new(),
        };
        // Leading `@<tier>` method directives (`@test`, `@doc { … }`) — rendered into the structural
        // skeleton so the formatter's safety gate treats dropping or altering one as a program
        // change (a text tier's body is part of its identity, like the `@tier` `text:` field above).
        let directives: String = self
            .directives
            .iter()
            .map(|d| {
                // The argument **values**, not just how many there were. A directive's args are the
                // tier's knobs (`@bench(iterations: 1000)`), read back by its runner, so
                // `@bench(1000)` and `@bench(2000)` are different programs — and they rendered
                // identically as `@bench(1)` while only the arity was printed.
                let args = attr_args_str(&d.args);
                let body = match &d.doc_text {
                    Some(text) => format!(" {{{text}}}"),
                    None => String::new(),
                };
                format!("@{}{args}{body} ", d.name)
            })
            .collect();
        let attrs: String = self
            .attrs
            .iter()
            .map(|a| format!("#[{}{}] ", a.name, attr_args_str(&a.args)))
            .collect();
        // The `use (…)` capture clause is part of the sealed fn's identity: dropping it would
        // strip the body's access to its captured bindings, so the safety gate must see it.
        let captures = if self.captures.is_empty() {
            String::new()
        } else {
            let names: Vec<&str> = self.captures.iter().map(|(n, _)| n.as_str()).collect();
            format!(" use ({})", names.join(", "))
        };
        // The declared **return type**. The checker types every `return` against it, and the `?`
        // position rule (E0012) reads its shape, so dropping or changing it is a program change —
        // and the formatter re-emits it from the AST. It was not in this dump at all.
        let ret = match &self.ret {
            Some(t) => format!(": {}", type_ref_str(t)),
            None => String::new(),
        };
        out.push_str(&format!(
            "({tier}{directives}{attrs}{}fn {}{}{}{captures} [{}]{ret} {}",
            if self.is_async { "async " } else { "" },
            pub_str(self.is_public),
            self.name,
            type_params_str(&self.type_params),
            param_list(&self.params),
            span(self.span)
        ));
        // Parameter defaults, whose presence the parameter list marks with `=`.
        for p in &self.params {
            if let Some(default) = &p.default {
                expr_child(out, level + 1, "param-default", &p.name, default);
            }
        }
        for stmt in &self.body {
            out.push('\n');
            stmt.pretty(out, level + 1);
        }
        out.push(')');
    }
}

impl Pretty for EnumDecl {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        let kind = if self.backing.is_some() {
            "enum-backed"
        } else {
            "enum"
        };
        let variants: Vec<String> = self
            .variants
            .iter()
            .map(|v| {
                // A variant's own `#[...]` attributes reach the reflection manifest exactly as a
                // field's do, and a backed variant's `= value` is what the enum *is*; both were
                // dropped here. The backing expression itself is a child line (below).
                let attrs: String = v
                    .attrs
                    .iter()
                    .map(|a| format!("#[{}{}] ", a.name, attr_args_str(&a.args)))
                    .collect();
                let backed = if v.backed_value.is_some() { "=" } else { "" };
                if v.fields.is_empty() {
                    format!("{attrs}{}{backed}", v.name)
                } else {
                    format!(
                        "{attrs}{}({}){backed}",
                        v.name,
                        variant_payload_list(&v.fields)
                    )
                }
            })
            .collect();
        // Which primitive backs the enum, not merely *that* one does: `enum S: string` and
        // `enum S: int` both printed `enum-backed`, so a formatter that rewrote the backing type
        // compared equal.
        let backing = match &self.backing {
            Some(t) => format!(": {}", type_ref_str(t)),
            None => String::new(),
        };
        out.push_str(&format!(
            "({kind} {}{}{}{}{backing} [{}] {}",
            decorators_str(&self.decorators),
            pub_str(self.is_public),
            self.name,
            type_params_str(&self.type_params),
            variants.join(" "),
            span(self.span)
        ));
        for v in &self.variants {
            if let Some(value) = &v.backed_value {
                expr_child(out, level + 1, "variant-value", &v.name, value);
            }
        }
        // An enum body may carry methods (the unified body, object-model slice 3); print them like a
        // class's so a method-bearing enum is visible in the AST snapshot. A variant-only enum prints
        // exactly as before (no trailing methods).
        for method in &self.methods {
            out.push('\n');
            method.pretty(out, level + 1);
        }
        impl_blocks(out, level + 1, &self.impls);
        out.push(')');
    }
}

/// The leading decorators of a declaration, rendered into the structural snapshot — the fmt safety
/// gate compares this output, so a formatter dropping a decorator (a whole `@derive`, one of its
/// `member: target` bindings or `via:`, a `@packed` layout, an `@attribute`/`@role` tag, a
/// `@validated` marker, a `#[...]` data attribute) is a DETECTED program change, not a silent one.
/// Empty when the declaration is undecorated, so undecorated snapshots are untouched.
///
/// Emission order is [`BuiltinDirective::ALL`](crate::BuiltinDirective::ALL) order, with the
/// `#[...]` data attributes last. Driving the loop off `ALL` — rather than a hand-written sequence
/// of `if let`s — is what makes a newly added directive a compile error here instead of a silently
/// unrendered (and therefore ungated) decorator. The other half of that lock is [`Decorators`]
/// itself: one struct shared by every declaration kind, so a directive cannot be rendered for a
/// struct but forgotten for an enum, which is precisely how `@validated` came to be ungated.
fn decorators_str(d: &crate::Decorators) -> String {
    let mut parts: Vec<String> = Vec::new();
    for directive in crate::BuiltinDirective::ALL {
        match directive {
            crate::BuiltinDirective::Derive => {
                for spec in &d.derives {
                    let mut s = spec.name.to_string();
                    if !spec.args.is_empty() {
                        s.push_str(&format!(
                            "<{}>",
                            spec.args
                                .iter()
                                .map(type_ref_str)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    if let Some((via, _)) = &spec.via {
                        s.push_str(&format!(", via: {via}"));
                    }
                    for b in &spec.bindings {
                        s.push_str(&format!(", {}: {}", b.member, b.target));
                    }
                    parts.push(format!("@derive({s})"));
                }
            }
            crate::BuiltinDirective::Attribute => {
                if let Some(kinds) = d.attribute.as_deref() {
                    if kinds.is_empty() {
                        parts.push("@attribute".to_string());
                    } else {
                        parts.push(format!(
                            "@attribute({})",
                            kinds
                                .iter()
                                .map(|(k, _)| k.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                }
            }
            crate::BuiltinDirective::Role => {
                if let Some(tags) = d.role.as_deref() {
                    parts.push(format!(
                        "@role({})",
                        tags.iter()
                            .map(|t| if t.enum_name.is_empty() {
                                t.variant.clone()
                            } else {
                                format!("{}.{}", t.enum_name, t.variant)
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            crate::BuiltinDirective::Semantic => {
                if d.semantic.is_some() {
                    parts.push("@semantic".to_string());
                }
            }
            crate::BuiltinDirective::Packed => {
                if let Some(p) = d.packed {
                    parts.push(match p.layout {
                        crate::PackedLayout::Row => "@packed".to_string(),
                        crate::PackedLayout::Column => "@packed(Layout.Column)".to_string(),
                    });
                }
            }
            crate::BuiltinDirective::Validated => {
                if d.validated.is_some() {
                    parts.push("@validated".to_string());
                }
            }
            // `@tier(...)` decorates a `fn`, not a type declaration, and is carried in
            // `FnDecl::tier` rather than in any of the fields above — it is rendered by the `fn`
            // printer. Listing it here (instead of a `_ =>` catch-all) is what keeps this match
            // exhaustive, so a future directive cannot slip through unrendered.
            crate::BuiltinDirective::Tier => {}
        }
    }
    // Directives the decorator grammar does not own (an extension's, a misplaced `@tier`, a typo).
    // They MUST appear in the gate: the formatter has to round-trip them, and a decorator the gate
    // cannot see is a decorator the formatter may silently delete — exactly how `@validated` went
    // unnoticed before it was rendered here.
    for f in &d.foreign {
        parts.push(format!("@{}{}", f.name, attr_args_str(&f.args)));
    }
    for a in &d.attrs {
        parts.push(format!("#[{}{}]", a.name, attr_args_str(&a.args)));
    }
    if parts.is_empty() {
        String::new()
    } else {
        let mut s = parts.join(" ");
        s.push(' ');
        s
    }
}

/// One `struct`/`class` field, for the S-expression dump.
///
/// A trailing `=` marks a field carrying a default (slice 5) — the expression itself is not inlined
/// (it can be multi-line); it is rendered as a `(field-default …)` child of the declaration, so the
/// gate still compares it.
///
/// The declared **type**, the `pub` marker and the field's `#[...]` attributes are rendered for the
/// same reason every other declaration detail is: the formatter re-emits all three from the AST, and
/// none of them was in this dump — so dropping a field's `: int`, its visibility, or its `#[Column]`
/// compared equal to the gate.
fn field_decl_str(field: &FieldDecl) -> String {
    let attrs: String = field
        .attrs
        .iter()
        .map(|a| format!("#[{}{}] ", a.name, attr_args_str(&a.args)))
        .collect();
    let mut_marker = if field.mut_field { "mut " } else { "" };
    let ty = field
        .ty
        .as_ref()
        .map(|t| format!(": {}", type_ref_str(t)))
        .unwrap_or_default();
    let default = if field.default.is_some() { " =" } else { "" };
    format!(
        "{attrs}{}{mut_marker}{}{ty}{default}",
        pub_str(field.is_public),
        field.name
    )
}

/// Render a declaration's generic parameters as `<A, B>` (or `<T: Comparable + Display>` when
/// bounded), or the empty string when there are none (so non-generic declarations' pretty output
/// is unchanged, as is any unbounded generic's).
fn type_params_str(params: &[TypeParam]) -> String {
    if params.is_empty() {
        String::new()
    } else {
        let parts: Vec<String> = params
            .iter()
            .map(|p| {
                if p.bounds.is_empty() {
                    p.name.clone()
                } else {
                    let bounds: Vec<String> = p.bounds.iter().map(trait_bound_str).collect();
                    format!("{}: {}", p.name, bounds.join(" + "))
                }
            })
            .collect();
        format!("<{}>", parts.join(", "))
    }
}

/// Render one trait bound: the bare name, or `Name<args>` for an instantiated bound.
fn trait_bound_str(b: &TraitBound) -> String {
    if b.args.is_empty() {
        b.name.to_string()
    } else {
        let args: Vec<String> = b.args.iter().map(type_ref_str).collect();
        format!("{}<{}>", b.name, args.join(", "))
    }
}

/// Render `pub ` for an exported declaration, or the empty string otherwise (so module-private
/// declarations' pretty output is unchanged).
fn pub_str(is_public: bool) -> &'static str {
    if is_public { "pub " } else { "" }
}

/// Render directive/attribute arguments as `(name: value, value)`, or the empty string when there
/// are none (so a bare `@test { }` block's pretty output is unchanged). Used by the tier-block
/// printer so `@bench(iterations: 1000)` surfaces in snapshots.
fn attr_args_str(args: &[AttrArg]) -> String {
    if args.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = args
        .iter()
        .map(|a| match &a.name {
            Some(name) => format!("{name}: {}", attr_value_str(&a.value)),
            None => attr_value_str(&a.value),
        })
        .collect();
    format!("({})", parts.join(", "))
}

/// A comma-separated rendering of a list of attribute-argument values.
fn attr_value_list(items: &[AttrValue]) -> String {
    items
        .iter()
        .map(attr_value_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A compact rendering of an attribute-argument literal value, enough to make a snapshot legible.
pub(crate) fn attr_value_str(value: &AttrValue) -> String {
    match value {
        AttrValue::Str(s) => format!("{s:?}"),
        AttrValue::Int(n) => n.to_string(),
        // `{:?}`, not `to_string()`: a whole-valued float renders `1` through `Display`, which is
        // exactly what `Int(1)` renders — two different attribute arguments, one spelling.
        AttrValue::Float(f) => format!("{f:?}"),
        AttrValue::Bool(b) => b.to_string(),
        AttrValue::List(items) => format!("[{}]", attr_value_list(items)),
        // A set is `#{…}`, its own surface form. Sharing the list arm meant `[1, 2]` and `#{1, 2}`
        // — different values of different types — rendered identically.
        AttrValue::Set(items) => format!("#{{{}}}", attr_value_list(items)),
        AttrValue::Map(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(k, v)| format!("{k:?}: {}", attr_value_str(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        // With its payload: `Status.Code(404)` and `Status.Code(500)` construct different attribute
        // instances, and both rendered as the bare `Status.Code`.
        AttrValue::Enum {
            enum_name,
            variant,
            args,
        } if args.is_empty() => format!("{enum_name}.{variant}"),
        AttrValue::Enum {
            enum_name,
            variant,
            args,
        } => format!("{enum_name}.{variant}({})", attr_value_list(args)),
        // With its fields: `Point { x: 1 }` rendered as `Point {…}`, so every struct-valued
        // attribute argument of a given type compared equal to every other.
        AttrValue::Struct { type_name, fields } => format!(
            "{type_name} {{{}}}",
            fields
                .iter()
                .map(|(k, v)| format!("{k}: {}", attr_value_str(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        // Rendered WITH its generic arguments: the fmt safety gate compares this output, so a
        // formatter dropping the `<Json>` from `@derive(Serialize<Json>)` must be detectable.
        AttrValue::TypeRef { name, args } if args.is_empty() => name.to_string(),
        AttrValue::TypeRef { name, args } => format!(
            "{name}<{}>",
            args.iter().map(type_ref_str).collect::<Vec<_>>().join(", ")
        ),
    }
}

impl Pretty for StructDecl {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        let fields: Vec<String> = self.fields.iter().map(field_decl_str).collect();
        out.push_str(&format!(
            "(struct {}{}{}{} [{}] {}",
            decorators_str(&self.decorators),
            pub_str(self.is_public),
            self.name,
            type_params_str(&self.type_params),
            fields.join(" "),
            span(self.span)
        ));
        for f in &self.fields {
            if let Some(default) = &f.default {
                expr_child(out, level + 1, "field-default", &f.name, default);
            }
        }
        // A struct body may carry methods (the unified body) — print them like a class's, so a
        // formatter dropping one is a detected change. A field-only struct prints as before.
        for method in &self.methods {
            out.push('\n');
            method.pretty(out, level + 1);
        }
        impl_blocks(out, level + 1, &self.impls);
        out.push(')');
    }
}

impl Pretty for ClassDecl {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        let fields: Vec<String> = self.fields.iter().map(field_decl_str).collect();
        out.push_str(&format!(
            "(class {}{}{}{} [{}] {}",
            decorators_str(&self.decorators),
            pub_str(self.is_public),
            self.name,
            type_params_str(&self.type_params),
            fields.join(" "),
            span(self.span)
        ));
        for f in &self.fields {
            if let Some(default) = &f.default {
                expr_child(out, level + 1, "field-default", &f.name, default);
            }
        }
        for method in &self.methods {
            out.push('\n');
            method.pretty(out, level + 1);
        }
        impl_blocks(out, level + 1, &self.impls);
        // The `destruct { … }` block. It is not a method — it has no call site and never appeared
        // in `methods` — so it was rendered nowhere, and a formatter that dropped it silently
        // removed the code the collector runs when the last reference to an instance goes away.
        if let Some(body) = &self.destructor {
            out.push('\n');
            indent(out, level + 1);
            out.push_str("(destruct");
            for stmt in body {
                out.push('\n');
                stmt.pretty(out, level + 2);
            }
            out.push(')');
        }
        out.push(')');
    }
}

impl Pretty for ImplDecl {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        let args = if self.trait_args.is_empty() {
            String::new()
        } else {
            format!(
                "<{}>",
                self.trait_args
                    .iter()
                    .map(type_ref_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        out.push_str(&format!(
            "(impl {}{args} for {}{} {}",
            self.trait_name,
            self.target,
            assoc_bindings_str(&self.assoc_bindings),
            span(self.span)
        ));
        for method in &self.methods {
            out.push('\n');
            method.pretty(out, level + 1);
        }
        out.push(')');
    }
}

impl Pretty for TraitDecl {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        // A trait's decorators were previously not rendered at all, which left every `@derive`/
        // `@role`/`@attribute`/`@semantic`/`@packed` on a trait outside the fmt safety gate — the
        // formatter could drop one without the gate noticing. They are misplaced directives (the
        // checker reports E0054), but "the checker rejects it" is not a reason for the *formatter*
        // to be free to silently rewrite the program: fmt runs on code that does not yet check.
        //
        // `pub` and the generic parameters were missing for the same class of reason — nothing
        // rendered them on this path — even though `pub` decides whether the trait is importable at
        // all and `<Fmt>` decides what an `impl` must instantiate.
        out.push_str(&format!(
            "(trait {}{}{}{} {}",
            decorators_str(&self.decorators),
            pub_str(self.is_public),
            self.name,
            type_params_str(&self.type_params),
            span(self.span)
        ));
        // Associated types: `type Name;` (every impl must bind it) or `type Name = Default;`.
        for a in &self.assoc_types {
            out.push('\n');
            indent(out, level + 1);
            match &a.default {
                Some(t) => out.push_str(&format!("(assoc-type {} = {})", a.name, type_ref_str(t))),
                None => out.push_str(&format!("(assoc-type {})", a.name)),
            }
        }
        for method in &self.methods {
            out.push('\n');
            // `has_default` distinguishes a **required** method from a default whose body happens
            // to be empty — `fn f(): int` and `fn f(): int {}` render the same signature with the
            // same (empty) body list, and the checker demands an `impl` provide the first and not
            // the second. Wrapping the required ones is the only thing in this dump that tells them
            // apart.
            if method.has_default {
                method.sig.pretty(out, level + 1);
            } else {
                indent(out, level + 1);
                out.push_str("(required\n");
                method.sig.pretty(out, level + 2);
                out.push(')');
            }
        }
        out.push(')');
    }
}

impl Pretty for ObjectLit {
    fn pretty(&self, out: &mut String, level: usize) {
        // The header line is already indented by the caller (`Expr::pretty` emits the
        // leading indent before delegating here), so we start the text directly.
        // A target-typed `.{ … }` dumps its head as `.{` — the name is not in the source, and the
        // dump is a faithful view of the AST, not of what the checker will later infer.
        let head = self.type_name.as_ref().map_or(".{", Name::as_str);
        out.push_str(&format!("(object {} {}", head, span(self.span)));
        for field in &self.fields {
            out.push('\n');
            indent(out, level + 1);
            out.push_str(&format!("(field {} {}\n", field.name, span(field.span)));
            field.value.pretty(out, level + 2);
            out.push(')');
        }
        if let Some(spread) = &self.spread {
            out.push('\n');
            indent(out, level + 1);
            out.push_str("(spread\n");
            spread.pretty(out, level + 2);
            out.push(')');
        }
        out.push(')');
    }
}

/// Render a pattern to a compact inline form for snapshots.
fn pattern_str(pattern: &Pattern) -> String {
    match pattern {
        Pattern::Wildcard { span: _ } => "_".to_string(),
        Pattern::Binding { name, span: _ } => name.clone(),
        Pattern::Int { value, span: _ } => value.to_string(),
        Pattern::Str { value, span: _ } => format!("{value:?}"),
        Pattern::Bool { value, span: _ } => value.to_string(),
        Pattern::Variant {
            type_name,
            variant,
            bindings,
            span: _,
        } => {
            let head = match type_name {
                Some(t) => format!("{t}.{variant}"),
                None => variant.clone(),
            };
            match (bindings.is_empty(), type_name.is_some()) {
                // A **qualified** fieldless variant is unambiguous: `Color.Red` can only be a
                // constructor, and `Color.Red()` parses to this same node.
                (true, true) => head,
                // An **unqualified** one is not. `Ok()` is this node; bare `Ok` is a
                // `Pattern::Binding` that matches *anything*. Both rendered as `Ok`, so the fmt
                // safety gate compared a payload-less variant arm equal to a catch-all binding —
                // and the printer was dropping exactly those parens, turning `Ok() => …` into
                // `Ok => …`. The `()` is written here because it is written in the source.
                (true, false) => format!("{head}()"),
                (false, _) => {
                    let inner: Vec<String> = bindings.iter().map(pattern_str).collect();
                    format!("{head}({})", inner.join(", "))
                }
            }
        }
        Pattern::IsType { ty, span: _ } => format!("is {}", type_ref_str(ty)),
        Pattern::Tuple { elements, span: _ } => {
            let inner: Vec<String> = elements.iter().map(pattern_str).collect();
            format!("({})", inner.join(", "))
        }
    }
}

impl Pretty for Expr {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        match self {
            Expr::Str { value, span: s } => {
                out.push_str(&format!("(str {:?} {})", value, span(*s)));
            }
            Expr::Int { value, span: s } => {
                out.push_str(&format!("(int {value} {})", span(*s)));
            }
            Expr::Float { value, span: s } => {
                out.push_str(&format!("(float {value} {})", span(*s)));
            }
            Expr::F32 { value, span: s } => {
                out.push_str(&format!("(f32 {value} {})", span(*s)));
            }
            Expr::F64 { value, span: s } => {
                out.push_str(&format!("(f64 {value} {})", span(*s)));
            }
            Expr::IntN {
                magnitude,
                signed,
                bits,
                span: s,
            } => {
                let suffix = if *signed { 'i' } else { 'u' };
                out.push_str(&format!("(intn {magnitude}{suffix}{bits} {})", span(*s)));
            }
            Expr::Bool { value, span: s } => {
                out.push_str(&format!("(bool {value} {})", span(*s)));
            }
            Expr::Ident { name, span: s } => {
                out.push_str(&format!("(ident {name} {})", span(*s)));
            }
            Expr::Unary {
                op,
                operand,
                span: s,
            } => {
                out.push_str(&format!("(unary {:?} {}\n", op.symbol(), span(*s)));
                operand.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Binary {
                op,
                lhs,
                rhs,
                span: s,
            } => {
                out.push_str(&format!("(binary {:?} {}\n", op.symbol(), span(*s)));
                lhs.pretty(out, level + 1);
                out.push('\n');
                rhs.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Call {
                callee,
                args,
                span: s,
            } => {
                out.push_str(&format!("(call {}\n", span(*s)));
                callee.pretty(out, level + 1);
                for arg in args {
                    out.push('\n');
                    arg.pretty(out, level + 1);
                }
                out.push(')');
            }
            Expr::Closure {
                params,
                ret,
                body,
                span: s,
            } => {
                // The return annotation `fn(x): int => …` is optional and checked when written, so
                // dropping it changes what the checker accepts. It was explicitly discarded here.
                let ret = match ret {
                    Some(t) => format!(": {}", type_ref_str(t)),
                    None => String::new(),
                };
                out.push_str(&format!(
                    "(closure [{}]{ret} {}\n",
                    param_list(params),
                    span(*s)
                ));
                match body {
                    ClosureBody::Expr(e) => e.pretty(out, level + 1),
                    ClosureBody::Block(stmts) => {
                        // One statement per line. Without the separator, consecutive statements ran
                        // together on one line, which is a rendering in which two different
                        // statement lists can coincide.
                        for (i, stmt) in stmts.iter().enumerate() {
                            if i > 0 {
                                out.push('\n');
                            }
                            stmt.pretty(out, level + 1);
                        }
                    }
                }
                out.push(')');
            }
            Expr::Pipeline {
                left,
                right,
                span: s,
            } => {
                out.push_str(&format!("(pipeline {}\n", span(*s)));
                left.pretty(out, level + 1);
                out.push('\n');
                right.pretty(out, level + 1);
                out.push(')');
            }
            Expr::List { items, span: s } => {
                out.push_str(&format!("(list {}", span(*s)));
                for item in items {
                    out.push('\n');
                    item.pretty(out, level + 1);
                }
                out.push(')');
            }
            Expr::Tuple { items, span: s } => {
                out.push_str(&format!("(tuple {}", span(*s)));
                for item in items {
                    out.push('\n');
                    item.pretty(out, level + 1);
                }
                out.push(')');
            }
            Expr::TupleIndex {
                receiver,
                index,
                span: s,
            } => {
                out.push_str(&format!("(tuple-index {index} {}\n", span(*s)));
                receiver.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Range {
                start,
                end,
                inclusive,
                span: s,
            } => {
                let op = if *inclusive { "range-incl" } else { "range" };
                out.push_str(&format!("({op} {}\n", span(*s)));
                start.pretty(out, level + 1);
                out.push('\n');
                end.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Map { entries, span: s } => {
                out.push_str(&format!("(map {}", span(*s)));
                for (key, value) in entries {
                    out.push('\n');
                    indent(out, level + 1);
                    out.push_str("(entry\n");
                    key.pretty(out, level + 2);
                    out.push('\n');
                    value.pretty(out, level + 2);
                    out.push(')');
                }
                out.push(')');
            }
            Expr::Member {
                receiver,
                name,
                name_span: _,
                span: s,
            } => {
                out.push_str(&format!("(member {name} {}\n", span(*s)));
                receiver.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Index {
                receiver,
                index,
                span: s,
            } => {
                out.push_str(&format!("(index {}\n", span(*s)));
                receiver.pretty(out, level + 1);
                out.push('\n');
                index.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Interp { parts, span: s } => {
                out.push_str(&format!("(interp {}", span(*s)));
                for part in parts {
                    out.push('\n');
                    match part {
                        StrPart::Literal(text) => {
                            indent(out, level + 1);
                            out.push_str(&format!("(lit {text:?})"));
                        }
                        StrPart::Hole(expr) => {
                            indent(out, level + 1);
                            out.push_str("(hole\n");
                            expr.pretty(out, level + 2);
                            out.push(')');
                        }
                    }
                }
                out.push(')');
            }
            Expr::Match {
                scrutinee,
                arms,
                span: s,
            } => {
                out.push_str(&format!("(match {}\n", span(*s)));
                scrutinee.pretty(out, level + 1);
                for arm in arms {
                    out.push('\n');
                    indent(out, level + 1);
                    out.push_str(&format!("(arm {}\n", pattern_str(&arm.pattern)));
                    if let Some(guard) = &arm.guard {
                        indent(out, level + 2);
                        out.push_str("(guard\n");
                        guard.pretty(out, level + 3);
                        out.push_str(")\n");
                    }
                    match &arm.body {
                        ClosureBody::Expr(e) => e.pretty(out, level + 2),
                        ClosureBody::Block(stmts) => {
                            indent(out, level + 2);
                            out.push_str("(block");
                            for stmt in stmts {
                                out.push('\n');
                                stmt.pretty(out, level + 3);
                            }
                            out.push(')');
                        }
                    }
                    out.push(')');
                }
                out.push(')');
            }
            Expr::Object(lit) => lit.pretty(out, level),
            Expr::Try { expr, span: s } => {
                out.push_str(&format!("(try {}\n", span(*s)));
                expr.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Await { expr, span: s } => {
                out.push_str(&format!("(await {}\n", span(*s)));
                expr.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Spawn {
                future,
                isolate,
                span: s,
            } => {
                let kw = if *isolate { "isolate" } else { "spawn" };
                out.push_str(&format!("({kw} {}\n", span(*s)));
                future.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Coalesce {
                value,
                fallback,
                span: s,
            } => {
                out.push_str(&format!("(coalesce {}\n", span(*s)));
                value.pretty(out, level + 1);
                out.push('\n');
                fallback.pretty(out, level + 1);
                out.push(')');
            }
            Expr::As { expr, ty, span: s } => {
                out.push_str(&format!("(as {} {}\n", type_ref_str(ty), span(*s)));
                expr.pretty(out, level + 1);
                out.push(')');
            }
            Expr::TypeTest { expr, ty, span: s } => {
                out.push_str(&format!("(is {} {}\n", type_ref_str(ty), span(*s)));
                expr.pretty(out, level + 1);
                out.push(')');
            }
            Expr::AttributesOf { ty, span: s } => {
                out.push_str(&format!(
                    "(attributes_of {} {})",
                    type_ref_str(ty),
                    span(*s)
                ));
            }
            Expr::TypeName { ty, span: s } => {
                out.push_str(&format!("(type_name {} {})", type_ref_str(ty), span(*s)));
            }
            Expr::TypeOf { value, span: s } => {
                out.push_str(&format!("(type_of {}\n", span(*s)));
                value.pretty(out, level + 1);
                out.push(')');
            }
            Expr::FieldsOf { value, span: s } => {
                out.push_str(&format!("(fields_of {}\n", span(*s)));
                value.pretty(out, level + 1);
                out.push(')');
            }
            Expr::TraitsOf { value, span: s } => {
                out.push_str(&format!("(traits_of {}\n", span(*s)));
                value.pretty(out, level + 1);
                out.push(')');
            }
            Expr::FromBytes { ty, blob, span: s } => {
                out.push_str(&format!("(from_bytes {} {}\n", type_ref_str(ty), span(*s)));
                blob.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Channel {
                elem,
                capacity,
                span: s,
            } => {
                out.push_str(&format!("(channel {} {}\n", type_ref_str(elem), span(*s)));
                capacity.pretty(out, level + 1);
                out.push(')');
            }
            Expr::RolesOf { ty, span: s } => match ty {
                Some(ty) => out.push_str(&format!("(roles_of {} {})", type_ref_str(ty), span(*s))),
                None => out.push_str(&format!("(roles_of {})", span(*s))),
            },
            Expr::ParamsOf { target, span: s } => {
                out.push_str(&format!("(params_of {}\n", span(*s)));
                target.pretty(out, level + 1);
                out.push(')');
            }
            Expr::ReturnsOf { target, span: s } => {
                out.push_str(&format!("(returns_of {}\n", span(*s)));
                target.pretty(out, level + 1);
                out.push(')');
            }
            Expr::FieldSpecsOf { name, span: s } => {
                out.push_str(&format!("(field_specs_of {}\n", span(*s)));
                name.pretty(out, level + 1);
                out.push(')');
            }
            Expr::VariantsOf { name, span: s } => {
                out.push_str(&format!("(variants_of {}\n", span(*s)));
                name.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Construct {
                name,
                fields,
                span: s,
            } => {
                out.push_str(&format!("(construct {}\n", span(*s)));
                name.pretty(out, level + 1);
                out.push('\n');
                fields.pretty(out, level + 1);
                out.push(')');
            }
            Expr::TypedModuleCall {
                recv,
                func,
                func_span: _,
                ty,
                args,
                span: s,
            } => {
                out.push_str(&format!(
                    "(typed-call {func} {} {}\n",
                    type_ref_str(ty),
                    span(*s)
                ));
                recv.pretty(out, level + 1);
                for arg in args {
                    out.push('\n');
                    arg.pretty(out, level + 1);
                }
                out.push(')');
            }
            Expr::TypedCall {
                name,
                name_span: _,
                type_args,
                args,
                span: s,
            } => {
                let tys: Vec<String> = type_args.iter().map(type_ref_str).collect();
                out.push_str(&format!(
                    "(typed-fn-call {name} <{}> {}",
                    tys.join(", "),
                    span(*s)
                ));
                for arg in args {
                    out.push('\n');
                    arg.pretty(out, level + 1);
                }
                out.push(')');
            }
            Expr::InstantiatedType {
                recv,
                type_args,
                span: s,
            } => {
                let tys: Vec<String> = type_args.iter().map(type_ref_str).collect();
                out.push_str(&format!(
                    "(instantiated-type <{}> {}\n",
                    tys.join(", "),
                    span(*s)
                ));
                recv.pretty(out, level + 1);
                out.push(')');
            }
            Expr::TypedMethodCall {
                recv,
                name,
                name_span: _,
                type_args,
                args,
                span: s,
            } => {
                let tys: Vec<String> = type_args.iter().map(type_ref_str).collect();
                out.push_str(&format!(
                    "(typed-method-call {name} <{}> {}\n",
                    tys.join(", "),
                    span(*s)
                ));
                recv.pretty(out, level + 1);
                for arg in args {
                    out.push('\n');
                    arg.pretty(out, level + 1);
                }
                out.push(')');
            }
            Expr::FieldSet {
                receiver,
                field,
                field_span: _,
                value,
                span: s,
            } => {
                out.push_str(&format!("(field-set {field} {}\n", span(*s)));
                receiver.pretty(out, level + 1);
                out.push('\n');
                value.pretty(out, level + 1);
                out.push(')');
            }
            Expr::TierExpr {
                tier,
                tier_span: _,
                statics,
                holes,
                span: s,
            } => {
                out.push_str(&format!("(tier-expr {tier} {}", span(*s)));
                for (i, static_) in statics.iter().enumerate() {
                    out.push('\n');
                    indent(out, level + 1);
                    out.push_str(&format!("(static {static_:?})"));
                    if let Some(hole) = holes.get(i) {
                        out.push('\n');
                        hole.pretty(out, level + 1);
                    }
                }
                out.push(')');
            }
            Expr::NativeFnRef {
                module,
                func,
                span: s,
            } => {
                out.push_str(&format!("(native-fn {module}.{func} {})", span(*s)));
            }
            Expr::Invoke {
                recv,
                name,
                args,
                span: s,
            } => {
                // The free-fn form prints as `invoke-free`, so a snapshot can never confuse the two
                // dispatch namespaces by operand count alone.
                let head = if recv.is_some() {
                    "invoke"
                } else {
                    "invoke-free"
                };
                out.push_str(&format!("({head} {}\n", span(*s)));
                if let Some(recv) = recv {
                    recv.pretty(out, level + 1);
                    out.push('\n');
                }
                name.pretty(out, level + 1);
                out.push('\n');
                args.pretty(out, level + 1);
                out.push(')');
            }
        }
    }
}

/// Render a [`TypeRef`] back to its surface spelling (`int`, `List<int>`, `?User`) for snapshots.
///
/// Names stay **verbatim** — a snapshot is about identity, so `app.models.User` must not shorten to
/// `User` and hide a real difference in a diff. That is `shape::type_source`, the same walk the
/// extension-facing `shape::type_spelling` runs with a different name transform.
pub(crate) fn type_ref_str(ty: &TypeRef) -> String {
    crate::shape::type_source(ty)
}
